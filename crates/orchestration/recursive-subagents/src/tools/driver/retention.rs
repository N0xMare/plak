//! Child conversation retirement and candidate-pin cleanup.

use super::{ProductAgentTree, TopologyError, TopologyErrorCode, TreeDriver, VcsError};
use crate::tree::retention::{MAX_RETIREMENT_BATCH, RetirementIssue, RetirementTicket};
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionLocator};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Action {
    Probe,
    Delete,
}

#[derive(Default)]
pub(super) struct Retries(BTreeMap<(String, Action), Retry>);

struct Retry {
    locator: String,
    failures: u8,
    next: Instant,
}

impl Retries {
    fn ready(&self, agent: &str, action: Action, locator: &str, now: Instant) -> bool {
        self.0
            .get(&(agent.to_owned(), action))
            .is_none_or(|entry| entry.locator != locator || entry.next <= now)
    }
    fn refused(&mut self, agent: &str, action: Action, locator: &str, now: Instant) {
        let entry = self
            .0
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
        self.0.remove(&(agent.to_owned(), action));
    }
}

impl TreeDriver {
    /// Retires sealed dormant conversations after publishing exact deletion intent.
    /// Unadopted nonempty work refs are retained independently of runtime sessions.
    pub(crate) async fn collect_dormant_children(
        &self,
        sessions: &AgentSessions,
        enabled: bool,
        occupied: std::collections::BTreeSet<String>,
    ) -> Result<(), TopologyError> {
        let now = plakous::model::now_unix_millis();
        let heads = if enabled {
            self.read(|tree| tree.child_retirement_heads(now, &occupied))?
        } else {
            Vec::new()
        };
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
                approved.insert(agent, (attempt, raw, revision));
                if approved.len() >= MAX_RETIREMENT_BATCH {
                    break;
                }
            } else {
                self.retry_refused(&agent, Action::Probe, &raw);
            }
        }
        let tickets = self
            .prepare_due_retirements(now, approved, &occupied)
            .await?;
        for ticket in tickets {
            let agent = &ticket.agent_id;
            let raw = ticket.locator.as_deref().unwrap_or_default();
            if let Err(issue) = self.dispatch_retirement(sessions, &ticket).await {
                self.retry_refused(agent, Action::Delete, raw);
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

    async fn prepare_due_retirements(
        &self,
        now: u64,
        approved: crate::tree::retention::ApprovedHeads,
        occupied: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<RetirementTicket>, TopologyError> {
        let driver = self.clone();
        let occupied = occupied.clone();
        self.write(move |tree| {
            let mut snapshot = None;
            let result = tree.prepare_child_retention_guarded(now, Some(&approved), |before| {
                snapshot = Some(before.clone());
            });
            let result = result.and_then(|mut tickets| {
                tickets.retain(|ticket| {
                    !occupied.contains(&ticket.agent_id)
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
            .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(target: "plak.topology", "could not record pending child retirement disposition");
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
    use super::{Action, Duration, Instant, Retries};

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
