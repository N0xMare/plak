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
struct HousekeepingWarnings(Vec<(Warning, Instant)>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Warning {
    Refused(TopologyErrorCode),
    Archive(ArchiveStall),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveStall {
    Unguarded,
    InvalidReceipt,
}

impl HousekeepingWarnings {
    fn due(&mut self, warning: Warning, now: Instant) -> bool {
        if let Some((_, last)) = self.0.iter_mut().find(|(prior, _)| *prior == warning) {
            if now.duration_since(*last) < HOUSEKEEPING_WARNING_INTERVAL {
                return false;
            }
            *last = now;
        } else {
            self.0.push((warning, now));
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
        max_age: Option<Duration>,
        occupied: std::collections::BTreeSet<String>,
        root: Option<SessionLocator>,
    ) -> Result<(), TopologyError> {
        let now = plakous::model::now_unix_millis();
        let heads = if enabled {
            self.certify_historical_carriers(sessions, now, max_age, root, &occupied)
                .await?;
            self.read(|tree| tree.child_retirement_heads(now, max_age, &occupied))?
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
                let archive = if requires_archive_receipt {
                    let Some(receipt) = self
                        .store
                        .child_archive_receipt(&tree_id, &agent, &attempt, &raw, revision)
                        .ok()
                        .flatten()
                    else {
                        self.retry_refused(&agent, Action::Probe, &raw);
                        continue;
                    };
                    Some(receipt)
                } else {
                    None
                };
                self.retry_clear(&agent, Action::Probe);
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
            .prepare_due_retirements(now, max_age, approved, &occupied, requires_archive_receipt)
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
        max_age: Option<Duration>,
        root: Option<SessionLocator>,
        occupied: &std::collections::BTreeSet<String>,
    ) -> Result<(), TopologyError> {
        let mut certified: Vec<HistoricalCarrierBatch> = Vec::new();
        for batch in self.read(|tree| tree.historical_carriers(now, max_age, root, occupied))? {
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
        max_age: Option<Duration>,
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
            let result =
                tree.prepare_child_retention_guarded(now, max_age, Some(&approved), |before| {
                    snapshot = Some(before.clone());
                });
            let result = result.and_then(|mut tickets| {
                let (mut unguarded, mut invalid) = (Vec::new(), Vec::new());
                tickets.retain(|ticket| {
                    if occupied.contains(&ticket.agent_id) {
                        return false;
                    }
                    match driver.retirement_archive_stall(tree, ticket, requires_archive_receipt) {
                        Some(ArchiveStall::Unguarded) => unguarded.push(ticket.agent_id.clone()),
                        Some(ArchiveStall::InvalidReceipt) => invalid.push(ticket.agent_id.clone()),
                        None => {
                            return driver.retry_ready(
                                &ticket.agent_id,
                                Action::Delete,
                                ticket.locator.as_deref().unwrap_or_default(),
                            );
                        }
                    }
                    false
                });
                driver.warn_archive_stalls(&tree.tree_id, &unguarded, &invalid);
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
    fn retirement_archive_stall(
        &self,
        tree: &ProductAgentTree,
        ticket: &RetirementTicket,
        required: bool,
    ) -> Option<ArchiveStall> {
        let head = ticket.locator.as_ref()?; // No runtime operation remains without a locator.
        if !ticket.receipt_required {
            return required.then_some(ArchiveStall::Unguarded);
        }
        let summary = tree.retired_children().get(&ticket.agent_id);
        let archive = summary.and_then(|summary| summary.archive.as_ref());
        let (Some(summary), Some(archive), Some(revision)) =
            (summary, archive, ticket.session_revision)
        else {
            return Some(ArchiveStall::InvalidReceipt);
        };
        let matches = tree.nodes().get(&ticket.agent_id).is_some_and(|node| {
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
            .is_ok_and(|receipt| receipt.as_ref() == Some(archive));
        (!matches).then_some(ArchiveStall::InvalidReceipt)
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

    fn warning_due(&self, warning: Warning) -> bool {
        self.child_retirement_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .warnings
            .due(warning, Instant::now())
    }

    fn warn_archive_stalls(&self, tree: &str, unguarded: &[String], invalid: &[String]) {
        if !unguarded.is_empty() && self.warning_due(Warning::Archive(ArchiveStall::Unguarded)) {
            tracing::warn!(target: "plak.topology", tree, children = ?unguarded,
                "child retirement stalled: intent has no archive receipt; with all home writers stopped, use plakd tree abort-retirement before archive or deletion");
        }
        if !invalid.is_empty() && self.warning_due(Warning::Archive(ArchiveStall::InvalidReceipt)) {
            tracing::warn!(target: "plak.topology", tree, children = ?invalid,
                "child retirement stalled: intent's archive receipt is missing, invalid or unreadable; restore the verified original receipt from a consistent backup, or stop all home writers and use plakd tree abort-retirement before archive or deletion");
        }
    }

    pub(crate) fn warn_child_housekeeping(&self, code: TopologyErrorCode, phase: &'static str) {
        if self.warning_due(Warning::Refused(code)) {
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
        Action, ApprovedHead, ArchiveStall, Duration, HOUSEKEEPING_WARNING_INTERVAL,
        HousekeepingWarnings, Instant, Retries, TopologyErrorCode, Warning,
    };
    use crate::protocol::SpawnAgentRequest;
    use crate::tools::tests::{bootstrap_tree, driver_on};
    use crate::tree::{ChildArchiveReceipt, MemoryTreeStore, ProductAgentTree};
    use plakous::sessions::*;
    use plakous_offline_crash_harness::{
        BoundaryController, protocol::CrashBoundary, text_runtime_binding,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::sync::Arc;

    fn due_child(root: &Path, locator: SessionLocator) -> (ProductAgentTree, String) {
        let mut tree = bootstrap_tree("receipt-retention", root);
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&child, locator.to_string())
            .unwrap();
        tree.record_turn_completed(&child).unwrap();
        tree.settle_for_test();
        let mail = tree.pending_result_mail("root").remove(0);
        tree.acknowledge_mail(&mail.message_id).unwrap();
        tree.attempt_mut_for_test(&child).ended_at = Some(1);
        (tree, child)
    }

    #[tokio::test]
    async fn missing_receipt_after_a_successful_probe_backs_off() {
        let root = tempfile::tempdir().unwrap();
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
        )
        .unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("evidence.jsonl"),
        )
        .unwrap();
        let binding = text_runtime_binding(root.path(), controller, "fixture", "fixture").unwrap();
        let request = CreateSessionRequest::new(
            SessionOperationId::new("retention:receipt").unwrap(),
            binding,
        );
        let CreateSessionOutcome::Created { session, .. } = sessions.create(request).await.unwrap()
        else {
            panic!("created session required");
        };
        let locator = session.release().await.unwrap();
        assert!(sessions.child_retirement_revision(locator).await.is_ok());
        let (tree, child) = due_child(root.path(), locator);
        let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
        driver
            .collect_dormant_children(&sessions, true, true, None, BTreeSet::new(), None)
            .await
            .unwrap();
        assert!(
            driver
                .read(|tree| tree.retired_children().is_empty())
                .unwrap()
        );
        let retries = driver.child_retirement_retry.lock().unwrap();
        let retry = &retries.heads[&(child.clone(), Action::Probe)];
        assert_eq!(retry.failures, 1);
        assert!(!retries.ready(
            &child,
            Action::Probe,
            &locator.to_string(),
            retry.next.checked_sub(Duration::from_millis(1)).unwrap()
        ));
        assert!(retries.ready(&child, Action::Probe, &locator.to_string(), retry.next));
    }

    #[tokio::test]
    async fn pending_intents_distinguish_unguarded_and_invalid_receipt_stalls() {
        let root = tempfile::tempdir().unwrap();
        let locator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003"
            .parse::<SessionLocator>()
            .unwrap();
        for (archive, stall) in [
            (None, ArchiveStall::Unguarded),
            (
                Some(ChildArchiveReceipt {
                    id: "fixture".into(),
                    sha256: "a".repeat(64),
                }),
                ArchiveStall::InvalidReceipt,
            ),
        ] {
            let (mut tree, child) = due_child(root.path(), locator);
            let approved = BTreeMap::from([(
                child.clone(),
                ApprovedHead {
                    attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                    locator: locator.to_string(),
                    revision: 1,
                    archive,
                },
            )]);
            let tickets = tree
                .prepare_child_retention_guarded(u64::MAX, None, Some(&approved), |_| {})
                .unwrap();
            let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
            driver
                .read(|tree| {
                    assert_eq!(
                        driver.retirement_archive_stall(tree, &tickets[0], true),
                        Some(stall)
                    );
                    assert_eq!(
                        driver.retirement_archive_stall(tree, &tickets[0], false),
                        (stall == ArchiveStall::InvalidReceipt).then_some(stall)
                    );
                })
                .unwrap();
            assert_eq!(
                driver
                    .prepare_due_retirements(
                        u64::MAX,
                        None,
                        BTreeMap::new(),
                        &BTreeSet::new(),
                        true
                    )
                    .await
                    .unwrap(),
                []
            );
            assert!(
                !driver.warning_due(Warning::Archive(stall)),
                "the blocked pass already warned"
            );
        }
    }

    #[test]
    fn child_retirement_warning_interval_is_independent_per_error_class() {
        let refusal = Warning::Refused(TopologyErrorCode::OperationRecoveryRequired);
        let unguarded = Warning::Archive(ArchiveStall::Unguarded);
        let invalid = Warning::Archive(ArchiveStall::InvalidReceipt);
        let mut warnings = HousekeepingWarnings::default();
        let start = Instant::now();
        assert!(warnings.due(refusal, start));
        let later = start + Duration::from_secs(1);
        assert!(warnings.due(unguarded, later));
        assert!(warnings.due(invalid, later));
        assert!(!warnings.due(refusal, later));
        assert!(!warnings.due(unguarded, later));
        assert!(!warnings.due(invalid, later));
        let due = start + HOUSEKEEPING_WARNING_INTERVAL;
        assert!(warnings.due(refusal, due));
        assert!(!warnings.due(unguarded, due));
        assert!(warnings.due(unguarded, due + Duration::from_secs(1)));
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
        for _ in 0..20 {
            retries.refused("a", probe, "head1", start);
        }
        assert!(!retries.ready("a", probe, "head1", start + Duration::from_secs(511)));
        assert!(retries.ready("a", probe, "head1", start + Duration::from_secs(512)));
    }
}
