//! Child conversation retirement and candidate-pin cleanup.

use super::{ProductAgentTree, TopologyError, TopologyErrorCode, TreeDriver, VcsError};
use crate::tree::retention::{
    ApprovedHead, HistoricalCarrierBatch, MAX_RETIREMENT_BATCH, RetirementIssue, RetirementTicket,
};
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionLocator};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Action {
    Certify,
    Probe,
    Delete,
}

#[derive(Default)]
pub(super) struct Retries {
    heads: BTreeMap<(String, Action), Retry>,
    warnings: HousekeepingWarnings,
}

const HOUSEKEEPING_WARNING_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Default)]
struct HousekeepingWarnings(Vec<(TopologyErrorCode, Instant)>);

impl HousekeepingWarnings {
    fn due(&mut self, code: TopologyErrorCode, now: Instant) -> bool {
        if let Some((_, last)) = self.0.iter_mut().find(|(prior, _)| *prior == code) {
            if now.duration_since(*last) < HOUSEKEEPING_WARNING_INTERVAL {
                return false;
            }
            *last = now;
        } else {
            self.0.push((code, now));
        }
        true
    }
}

struct Retry {
    locator: String,
    failures: u8,
    next: Instant,
}

impl Retries {
    fn ready(&self, agent: &str, action: Action, locator: &str, now: Instant) -> bool {
        self.heads
            .get(&(agent.to_owned(), action))
            .is_none_or(|entry| entry.locator != locator || entry.next <= now)
    }
    fn refused(&mut self, agent: &str, action: Action, locator: &str, now: Instant) {
        let entry = self
            .heads
            .entry((agent.to_owned(), action))
            .or_insert_with(|| Retry {
                locator: locator.to_owned(),
                failures: 0,
                next: now,
            });
        if entry.locator != locator {
            entry.locator = locator.to_owned();
            entry.failures = 0;
        }
        entry.failures = entry.failures.saturating_add(1).min(9);
        entry.next = now + Duration::from_secs(1 << entry.failures);
    }
    fn clear(&mut self, agent: &str, action: Action) {
        self.heads.remove(&(agent.to_owned(), action));
    }
}

impl TreeDriver {
    /// Retires sealed dormant conversations after publishing exact deletion intent.
    /// Unadopted nonempty work refs are retained independently of runtime sessions.
    pub(crate) async fn collect_dormant_children(
        &self,
        sessions: &AgentSessions,
        enabled: bool,
        requires_archive_receipt: bool,
        occupied: std::collections::BTreeSet<String>,
        root: Option<SessionLocator>,
    ) -> Result<(), TopologyError> {
        let now = plakous::model::now_unix_millis();
        let heads = if enabled {
            self.certify_historical_carriers(sessions, now, root, &occupied)
                .await?;
            self.read(|tree| tree.child_retirement_heads(now, &occupied))?
        } else {
            Vec::new()
        };
        let tree_id = self.read(|tree| tree.tree_id.clone())?;
        let mut approved = BTreeMap::new();
        for (agent, attempt, raw) in heads {
            if !self.retry_ready(&agent, Action::Probe, &raw) {
                continue;
            }
            let revision = if let Ok(locator) = raw.parse()
                && self.retirement_jobs_settled(&agent)
            {
                sessions.child_retirement_revision(locator).await.ok()
            } else {
                None
            };
            if let Some(revision) = revision {
                self.retry_clear(&agent, Action::Probe);
                let archive = if requires_archive_receipt {
                    let Some(receipt) = self
                        .store
                        .child_archive_receipt(&tree_id, &agent, &attempt, &raw, revision)
                        .ok()
                        .flatten()
                    else {
                        continue;
                    };
                    Some(receipt)
                } else {
                    None
                };
                approved.insert(
                    agent,
                    ApprovedHead {
                        attempt,
                        locator: raw,
                        revision,
                        archive,
                    },
                );
                if approved.len() >= MAX_RETIREMENT_BATCH {
                    break;
                }
            } else {
                self.retry_refused(&agent, Action::Probe, &raw);
            }
        }
        let tickets = self
            .prepare_due_retirements(now, approved, &occupied, requires_archive_receipt)
            .await?;
        for ticket in tickets {
            let agent = &ticket.agent_id;
            let raw = ticket.locator.as_deref().unwrap_or_default();
            if let Err(issue) = self.dispatch_retirement(sessions, &ticket).await {
                self.retry_refused(agent, Action::Delete, raw);
                tracing::debug!(target: "plak.topology", class = issue.label(),
                    "child retirement dispatch deferred");
                self.publish_retirement_issue(agent.clone(), issue).await;
                continue;
            }
            self.retry_clear(agent, Action::Delete);
            let driver = self.clone();
            let (finished, effects) = self
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| tree.finish_child_retention(&ticket))
                })
                .await?;
            self.settle(effects).await?;
            finished?;
        }
        self.cleanup_candidate_pins().await
    }

    async fn certify_historical_carriers(
        &self,
        sessions: &AgentSessions,
        now: u64,
        root: Option<SessionLocator>,
        occupied: &std::collections::BTreeSet<String>,
    ) -> Result<(), TopologyError> {
        let mut certified: Vec<HistoricalCarrierBatch> = Vec::new();
        for batch in self.read(|tree| tree.historical_carriers(now, root, occupied))? {
            let key = format!("{}@{}", batch.branch, batch.cursor);
            if !self.retry_ready(&batch.recipient, Action::Certify, &key) {
                continue;
            }
            let recipient = batch.recipient.clone();
            let (all, proof) = batch.certify(sessions).await;
            if all {
                self.retry_clear(&recipient, Action::Certify);
            } else {
                self.retry_refused(&recipient, Action::Certify, &key);
            }
            certified.extend(proof);
        }
        if certified.is_empty() {
            return Ok(());
        }
        let driver = self.clone();
        let saved = self
            .write(move |tree| {
                let before = (**tree).clone();
                for batch in &certified {
                    tree.apply_historical_carriers(batch, root);
                }
                if before.revision != tree.revision {
                    match driver.save_claimed(tree) {
                        Ok(revision) => tree.acknowledge_revision(revision),
                        Err(error) => {
                            **tree = before;
                            return Err(TopologyError::from(error));
                        }
                    }
                }
                Ok(())
            })
            .await
            .flatten();
        if let Err(error) = saved {
            self.warn_child_housekeeping(error.code, "historical_carrier_save");
        }
        Ok(())
    }

    async fn prepare_due_retirements(
        &self,
        now: u64,
        mut approved: crate::tree::retention::ApprovedHeads,
        occupied: &std::collections::BTreeSet<String>,
        requires_archive_receipt: bool,
    ) -> Result<Vec<RetirementTicket>, TopologyError> {
        let driver = self.clone();
        let occupied = occupied.clone();
        self.write(move |tree| {
            if requires_archive_receipt {
                approved.retain(|agent, head| {
                    driver
                        .store
                        .child_archive_receipt(
                            &tree.tree_id,
                            agent,
                            &head.attempt,
                            &head.locator,
                            head.revision,
                        )
                        .is_ok_and(|receipt| receipt.is_some() && receipt == head.archive)
                });
            }
            let mut snapshot = None;
            let result = tree.prepare_child_retention_guarded(now, Some(&approved), |before| {
                snapshot = Some(before.clone());
            });
            let result = result.and_then(|mut tickets| {
                tickets.retain(|ticket| {
                    !occupied.contains(&ticket.agent_id)
                        && driver.retirement_archive_matches(tree, ticket, requires_archive_receipt)
                        && driver.retry_ready(
                            &ticket.agent_id,
                            Action::Delete,
                            ticket.locator.as_deref().unwrap_or_default(),
                        )
                });
                // Even an old ticket needs the custody check before dispatch.
                if !tickets.is_empty() || snapshot.is_some() {
                    driver.save_guarded(tree)?;
                }
                Ok(tickets)
            });
            if result.is_err()
                && let Some(snapshot) = snapshot
            {
                **tree = snapshot;
            }
            result
        })
        .await?
    }

    // Recovered tickets must prove coverage and pass the guarded save too.
    // Receipts are immutable and removed only with the whole tree under custody.
    fn retirement_archive_matches(
        &self,
        tree: &ProductAgentTree,
        ticket: &RetirementTicket,
        required: bool,
    ) -> bool {
        let Some(head) = &ticket.locator else {
            return true; // No runtime operation remains.
        };
        let summary = tree.retired_children().get(&ticket.agent_id);
        let archive = summary.and_then(|summary| summary.archive.as_ref());
        if !required && !ticket.receipt_required {
            return true;
        }
        let (Some(summary), Some(archive), Some(revision)) =
            (summary, archive, ticket.session_revision)
        else {
            return false;
        };
        tree.nodes().get(&ticket.agent_id).is_some_and(|node| {
            node.attempt_id.as_ref() == Some(&summary.attempt)
                && node.session_locator.as_deref() == Some(head)
        }) && self
            .store
            .child_archive_receipt(
                &tree.tree_id,
                &ticket.agent_id,
                &summary.attempt,
                head,
                revision,
            )
            .is_ok_and(|receipt| receipt.as_ref() == Some(archive))
    }

    fn retry_ready(&self, agent: &str, action: Action, locator: &str) -> bool {
        self.child_retirement_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready(agent, action, locator, Instant::now())
    }
    fn retry_refused(&self, agent: &str, action: Action, locator: &str) {
        self.child_retirement_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refused(agent, action, locator, Instant::now());
    }
    fn retry_clear(&self, agent: &str, action: Action) {
        self.child_retirement_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear(agent, action);
    }

    pub(crate) fn warn_child_housekeeping(&self, code: TopologyErrorCode, phase: &'static str) {
        let due = self
            .child_retirement_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .warnings
            .due(code, Instant::now());
        if due {
            tracing::warn!(target: "plak.topology", ?code, phase, "child retirement housekeeping remains unsettled");
        }
    }

    async fn publish_retirement_issue(&self, agent: String, issue: RetirementIssue) {
        let driver = self.clone();
        let result = self
            .write(move |tree| {
                let before = (**tree).clone();
                let result = tree.defer_child_retirement(&agent, issue).and_then(|()| {
                    if tree.revision != before.revision {
                        let acknowledged =
                            driver.save_claimed(tree).map_err(TopologyError::from)?;
                        tree.acknowledge_revision(acknowledged);
                    }
                    Ok(())
                });
                if result.is_err() {
                    **tree = before;
                }
                result
            })
            .await
            .flatten();
        if let Err(error) = result {
            self.warn_child_housekeeping(error.code, "pending_disposition_save");
        }
    }

    async fn dispatch_retirement(
        &self,
        sessions: &AgentSessions,
        ticket: &RetirementTicket,
    ) -> Result<(), RetirementIssue> {
        use RetirementIssue::{Held, MovedOrPinned, StoreUnavailable};
        let id = &ticket.agent_id;
        if ticket.session_revision.is_none() {
            self.stop_agent_jobs(std::slice::from_ref(id))
                .await
                .map_err(|_| Held)?;
        } else if !self.retirement_jobs_settled(id)
            || self
                .read(|tree| tree.placement_held(id) || !tree.placement_ready(id))
                .map_err(|_| StoreUnavailable)?
        {
            return Err(Held);
        }
        let Some(raw) = &ticket.locator else {
            return Ok(());
        };
        let locator: SessionLocator = raw.parse().map_err(|_| MovedOrPinned)?;
        if let Some(revision) = ticket.session_revision {
            sessions
                .retire_child_session(locator, revision)
                .await
                .map_err(|error| match error.kind() {
                    AgentSessionsErrorKind::UnsettledProcesses
                    | AgentSessionsErrorKind::Busy
                    | AgentSessionsErrorKind::NotQuiescent
                    | AgentSessionsErrorKind::Ownership => Held,
                    AgentSessionsErrorKind::InvalidInput
                    | AgentSessionsErrorKind::StaleLocator
                    | AgentSessionsErrorKind::NotLineHead
                    | AgentSessionsErrorKind::NotFound => MovedOrPinned,
                    _ => StoreUnavailable,
                })
        } else {
            finish_legacy_archive(sessions, locator)
                .await
                .map_err(|_| StoreUnavailable)
        }
    }

    fn retirement_jobs_settled(&self, agent: &str) -> bool {
        self.job_service(agent).is_none_or(|jobs| {
            jobs.refresh_jobs().is_ok()
                && !jobs.has_unreported_terminal_job()
                && jobs.jobs().iter().all(|job| {
                    matches!(
                        job.status,
                        plakous::tools::JobStatus::Exited(_)
                            | plakous::tools::JobStatus::Stopped(_)
                    )
                })
        })
    }

    async fn cleanup_candidate_pins(&self) -> Result<(), TopologyError> {
        let pending = self.read(ProductAgentTree::pending_pin_cleanup)??;
        if pending.is_empty() {
            return Ok(());
        }
        let (namespace, launch, vcs) = self.read(|tree| {
            (
                plakous::model::sha256_hex(&[tree.tree_id.as_bytes()]),
                tree.launch_checkout.clone(),
                tree.vcs_port(),
            )
        })?;
        for ticket in pending {
            let unpinned = {
                let (vcs, launch, namespace, candidates) = (
                    vcs.clone(),
                    launch.clone(),
                    namespace.clone(),
                    ticket.candidates.clone(),
                );
                plakous_workspace::occupancy_vcs::stage::unstaged(
                    "candidate unpin, retained as an intent until acknowledged",
                    async move {
                        for candidate in &candidates {
                            vcs.unpin(&launch, &namespace, candidate.as_str()).await?;
                        }
                        Ok::<(), VcsError>(())
                    },
                )
                .await
            };
            if unpinned.is_err() {
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    "collected candidate pin cleanup remains pending; intent retained",
                ));
            }
            let driver = self.clone();
            let (finished, effects) = self
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| tree.acknowledge_pin_cleanup(&ticket))
                })
                .await?;
            self.settle(effects).await?;
            finished?;
        }
        Ok(())
    }
}

async fn finish_legacy_archive(
    sessions: &AgentSessions,
    locator: SessionLocator,
) -> Result<(), TopologyError> {
    let refused = |_| {
        TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            "child session deletion remains pending; work refs retained",
        )
    };
    match sessions.delete_archived(locator.session_id()).await {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == AgentSessionsErrorKind::InvalidInput => {
            let archived = sessions.archive(locator).await.map_err(refused)?;
            sessions
                .delete(archived.session_id(), archived.archived_revision())
                .await
                .map_err(refused)?;
            Ok(())
        }
        Err(error) => Err(refused(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Action, Duration, HOUSEKEEPING_WARNING_INTERVAL, HousekeepingWarnings, Instant, Retries,
        TopologyErrorCode,
    };

    #[test]
    fn child_retirement_warning_interval_is_independent_per_error_class() {
        use TopologyErrorCode::{OperationRecoveryRequired, WorkspaceRecoveryRequired};
        let mut warnings = HousekeepingWarnings::default();
        let start = Instant::now();
        assert!(warnings.due(OperationRecoveryRequired, start));
        let later = start + Duration::from_secs(1);
        assert!(warnings.due(WorkspaceRecoveryRequired, later));
        assert!(!warnings.due(OperationRecoveryRequired, later));
        let due = start + HOUSEKEEPING_WARNING_INTERVAL;
        assert!(warnings.due(OperationRecoveryRequired, due));
        assert!(!warnings.due(WorkspaceRecoveryRequired, due));
        assert!(warnings.due(WorkspaceRecoveryRequired, due + Duration::from_secs(1)));
    }

    #[test]
    fn child_retirement_backoff_is_per_head_and_does_not_block_other_children() {
        let start = Instant::now();
        let mut retries = Retries::default();
        let (probe, delete) = (Action::Probe, Action::Delete);
        retries.refused("a", probe, "head1", start);
        assert!(!retries.ready("a", probe, "head1", start));
        assert!(retries.ready("b", probe, "head1", start));
        assert!(retries.ready("a", probe, "head2", start));
        assert!(retries.ready("a", delete, "head1", start));
        let next = start + Duration::from_secs(2);
        assert!(retries.ready("a", probe, "head1", next));
        retries.refused("a", probe, "head1", next);
        assert!(!retries.ready("a", probe, "head1", next + Duration::from_secs(3)));
        assert!(retries.ready("a", probe, "head1", next + Duration::from_secs(4)));
        retries.clear("a", probe);
        assert!(retries.ready("a", probe, "head1", next));
    }
}
