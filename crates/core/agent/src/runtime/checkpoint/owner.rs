//! Single checkpoint publication, lease admission and joined heartbeat owner.

use crate::feed::FeedSeq;
use crate::manifest::InterpreterObservation;
use crate::recovery_scope::require_recoverable;
use crate::runtime::checkpoint::candidate::{
    CheckpointCandidate, SuccessorFacts, successor_candidate,
};
use crate::runtime::checkpoint::identity::deterministic_checkpoint_id;
use crate::runtime::checkpoint::io::{
    DurabilityCommitTelemetry, StoreCallError, StorePhase, commit_with_acknowledgement_recovery,
    store_call,
};
#[cfg(test)]
use crate::runtime::checkpoint::owner::heartbeat::renew_shared_durable_lease;
use crate::runtime::checkpoint::owner::heartbeat::{
    LeaseHeartbeat, renew_shared_durable_lease_under_gate,
};
use crate::runtime::cleanup::{combine_cleanup_results, contain_cleanup_panic};
use crate::runtime::continuation::checkpoint_bundle_error;
use crate::runtime::durable::DurableCodeCellAbandonmentDisposition;
use crate::runtime::durable::turn_recovery::{
    RootClosure, abandonment_cause, recover_root_turn_publication, verdict_closure,
    verdict_workflow_closure,
};
use crate::runtime::durable::workflow_closure::{
    WorkflowClosureBranch, WorkflowClosureRuntime, publish_workflow_closure,
};
use crate::runtime::terminal::{cancelled_error, lock_mutex};
use crate::runtime::{AbandonedTurnClaim, AbandonmentCause};
use crate::runtime::{DEFAULT_STORE_OPERATION_TIMEOUT, StoreTimeouts};
use crate::{
    AgentError, AgentErrorKind, BranchHead, BranchLease, CheckpointId, CheckpointObjectBundle,
    CommitCheckpoint, DurabilityAttemptKind, PruneBranch, PruneReceipt, RuntimeCheckpoint,
    RuntimeCheckpointKind, RuntimeDurability, SessionStore, SessionStoreCapacity, StoreAdmission,
    StoreBoundUsage,
};
use plakous_model::{
    CallId, CancellationToken, ContentBlock, ConversationView, Digest, ToolName, TurnId,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
pub(in crate::runtime) mod heartbeat;

#[derive(Clone)]
pub(in crate::runtime) struct DurableBranchFacts {
    store: Arc<dyn SessionStore>,
    head: BranchHead,
    lease: BranchLease,
    store_timeouts: StoreTimeouts,
}

#[derive(Clone)]
struct DurableBranchContext {
    store: Arc<dyn SessionStore>,
    head: BranchHead,
    lease: BranchLease,
    lease_gate: Arc<AsyncMutex<()>>,
    store_timeouts: StoreTimeouts,
}

struct CheckpointAuthority {
    checkpoint: Arc<RuntimeCheckpoint>,
    bundle: CheckpointObjectBundle,
    /// The feed record of the newest commit this owner published, keyed by
    /// the checkpoint it published so a later publication cannot misattribute it.
    published_feed_seq: Option<(CheckpointId, FeedSeq)>,
    durable: Option<DurableBranchContext>,
    /// The branch horizon the last prune receipt reported. Nothing at or below
    /// it can be superseded again, so it bounds what a prune could reclaim
    /// without asking the store.
    pruned_horizon: u64,
    abandoned: bool,
    released: bool,
}

impl CheckpointAuthority {
    fn conversation(&self) -> ConversationView {
        self.checkpoint.manifest().canonical().projection()
    }

    fn checkpoint(&self) -> Arc<RuntimeCheckpoint> {
        Arc::clone(&self.checkpoint)
    }
}

// Admission uses the store's clock after the final wait. A receipt is a
// historical fact; acknowledging it never refreshes the granted interval.
#[derive(Clone)]
pub(in crate::runtime) struct StartAuthority {
    checkpoint: Arc<Mutex<CheckpointAuthority>>,
    lost: CancellationToken,
    shutdown: CancellationToken,
}

impl StartAuthority {
    /// Admits one start before anything is dispatched. A refusal here is
    /// definite: nothing has started, so it never leaves an unknown outcome.
    pub(in crate::runtime) async fn admit(&self) -> Result<StartAdmission, AgentError> {
        let branch = lock_mutex(&self.checkpoint).durable.clone();
        let guard = if let Some(branch) = branch {
            Some(
                tokio::time::timeout(
                    branch.store_timeouts.operation,
                    branch.lease_gate.lock_owned(),
                )
                .await
                .map_err(|_| lease_lost_before_dispatch())?,
            )
        } else {
            None
        };
        // The runtime's timers do not count a suspended machine, so the first
        // start after a wake can arrive before the heartbeat notices the
        // lease ran out. Revive it here, under the gate this admission holds.
        if self.lease_expired() && !self.lost.is_cancelled() && !self.shutdown.is_cancelled() {
            renew_shared_durable_lease_under_gate(&self.checkpoint)
                .await
                .map_err(|_| lease_lost_before_dispatch())?;
        }
        self.check().map_err(|error| {
            if error.kind() == AgentErrorKind::OutcomeUnknown {
                lease_lost_before_dispatch()
            } else {
                error
            }
        })?;
        Ok(StartAdmission { _guard: guard })
    }

    fn lease_expired(&self) -> bool {
        let authority = lock_mutex(&self.checkpoint);
        authority.durable.as_ref().is_some_and(|branch| {
            branch.store.lease_now().is_ok_and(|now| {
                now.same_domain_cmp(branch.lease.expires_at())
                    .is_ok_and(|order| order != std::cmp::Ordering::Less)
            })
        })
    }

    pub(in crate::runtime) fn check(&self) -> Result<(), AgentError> {
        if self.shutdown.is_cancelled() {
            return Err(cancelled_error());
        }
        if self.lost.is_cancelled() {
            return Err(lease_authority_lost());
        }
        let authority = lock_mutex(&self.checkpoint);
        if let Some(branch) = &authority.durable {
            let now = branch
                .store
                .lease_now()
                .map_err(|_| lease_authority_lost())?;
            if now
                .same_domain_cmp(branch.lease.expires_at())
                .map_err(|_| lease_authority_lost())?
                != std::cmp::Ordering::Less
            {
                return Err(lease_authority_lost());
            }
        }
        Ok(())
    }
}

fn lease_authority_lost() -> AgentError {
    AgentError::new(
        AgentErrorKind::OutcomeUnknown,
        "durable branch lease authority was lost",
    )
}

fn lease_lost_before_dispatch() -> AgentError {
    AgentError::new(
        AgentErrorKind::Session,
        "durable branch lease was lost before dispatch; nothing was started",
    )
}

fn lease_taken_over() -> AgentError {
    AgentError::new(
        AgentErrorKind::Session,
        "durable branch lease expired and could not be revived; another owner may have \
         opened this session",
    )
}

/// A bounded admission lifetime; no mutex or mutation authority escapes.
pub(in crate::runtime) struct StartAdmission {
    _guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

pub(in crate::runtime) struct CheckpointOwner {
    state: Arc<Mutex<CheckpointAuthority>>,
    heartbeat: LeaseHeartbeat,
}

#[derive(Clone)]
pub(in crate::runtime) struct CheckpointReader {
    state: Arc<Mutex<CheckpointAuthority>>,
    heartbeat_stop: CancellationToken,
    heartbeat_lost: CancellationToken,
}

impl CheckpointOwner {
    pub(in crate::runtime) async fn admission_reclaimed(
        &self,
        seq: FeedSeq,
    ) -> Result<bool, AgentError> {
        let branch = lock_mutex(&self.state).durable.clone();
        let Some(branch) = branch else {
            return Ok(false);
        };
        let page = store_call(
            StorePhase::ReadFeed,
            branch.store_timeouts.operation,
            branch.store.read_feed(
                branch.head.session().session_id(),
                FeedSeq::new(seq.get() - 1),
                std::num::NonZeroUsize::MIN,
            ),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        Ok(matches!(
            page,
            crate::feed::FeedPage::BehindRetention { .. }
        ))
    }

    pub(in crate::runtime) fn reader(&self) -> CheckpointReader {
        CheckpointReader {
            state: Arc::clone(&self.state),
            heartbeat_stop: self.heartbeat.stop_token(),
            heartbeat_lost: self.heartbeat.lost().clone(),
        }
    }
    pub(in crate::runtime) fn checkpoint(&self) -> Arc<RuntimeCheckpoint> {
        lock_mutex(&self.state).checkpoint()
    }
    pub(in crate::runtime) fn conversation(&self) -> ConversationView {
        lock_mutex(&self.state).conversation()
    }
    /// The feed record that published `checkpoint`, when this owner committed
    /// it to a durable store.
    pub(in crate::runtime) fn feed_seq_at(&self, checkpoint: CheckpointId) -> Option<FeedSeq> {
        lock_mutex(&self.state)
            .published_feed_seq
            .and_then(|(published, seq)| (published == checkpoint).then_some(seq))
    }
    /// The feed record of the current head, when this owner committed it.
    pub(in crate::runtime) fn head_feed_seq(&self) -> Option<FeedSeq> {
        let authority = lock_mutex(&self.state);
        let head = authority.checkpoint.manifest().topology().checkpoint_id;
        authority
            .published_feed_seq
            .and_then(|(published, seq)| (published == head).then_some(seq))
    }
    pub(in crate::runtime) fn start_authority(
        &self,
        shutdown: CancellationToken,
    ) -> StartAuthority {
        StartAuthority {
            checkpoint: Arc::clone(&self.state),
            lost: self.heartbeat.lost().clone(),
            shutdown,
        }
    }
    pub(in crate::runtime) fn check_heartbeat(&self) -> Result<(), AgentError> {
        self.heartbeat.check()
    }
    pub(in crate::runtime) fn heartbeat_failure(&self) -> AgentError {
        self.heartbeat.failure()
    }
    pub(in crate::runtime) async fn lease_lost(&self) {
        self.heartbeat.lost().cancelled().await;
    }
    pub(in crate::runtime) fn require_durability(&self) -> Result<(), AgentError> {
        let authority = lock_mutex(&self.state);
        authority.durable.as_ref().ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "mutating tools require a required-durability session store",
            )
        })?;
        if authority.checkpoint.manifest().durability() != RuntimeDurability::Required {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "mutating tools require the required durability contract",
            ));
        }
        Ok(())
    }
    /// Returns the aggregate bound on which ordinary work has no headroom left.
    ///
    /// A usage read that fails or times out admits: the commit itself still
    /// enforces the threshold, this only refuses a root before it starts.
    pub(in crate::runtime) async fn ordinary_store_pressure(&self) -> Option<SessionStoreCapacity> {
        let (store, timeout) = lock_mutex(&self.state)
            .durable
            .as_ref()
            .map(|branch| (Arc::clone(&branch.store), branch.store_timeouts.operation))?;
        let mut usage = tokio::time::timeout(timeout, store.usage())
            .await
            .ok()?
            .ok()?;
        if usage.ordinary_exhausted().is_some() {
            // Pressure is exceptional; ordinary admission remains snapshot-only.
            let _ = tokio::time::timeout(
                timeout,
                store.reclaim(crate::session::StoreReclaimMode::Bounded),
            )
            .await;
            usage = tokio::time::timeout(timeout, store.usage())
                .await
                .ok()?
                .ok()?;
        }
        usage
            .ordinary_exhausted()
            .map(StoreBoundUsage::ordinary_refusal)
    }

    pub(in crate::runtime) async fn publish(
        &mut self,
        facts: SuccessorFacts,
        observation: Option<InterpreterObservation>,
        telemetry: &DurabilityCommitTelemetry,
        admission: StoreAdmission,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let checkpoint_id = facts.checkpoint_id;
        self.heartbeat.check()?;
        let gate = lock_mutex(&self.state).durable.as_ref().map(|branch| {
            (
                Arc::clone(&branch.lease_gate),
                branch.store_timeouts.operation,
            )
        });
        let gate_timeout = gate
            .as_ref()
            .map_or(DEFAULT_STORE_OPERATION_TIMEOUT, |(_, timeout)| *timeout);
        let lease_gate = gate.as_ref().map(|(gate, _)| Arc::clone(gate));
        let _lease_guard = if let Some(gate) = lease_gate.as_ref() {
            Some(
                tokio::time::timeout(gate_timeout, gate.lock())
                    .await
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Session,
                            "checkpoint lease gate exceeded its configured deadline",
                        )
                    })?,
            )
        } else {
            None
        };
        self.heartbeat.check()?;
        let (prior, prior_bundle, durable) = {
            let authority = lock_mutex(&self.state);
            (
                authority.checkpoint(),
                authority.bundle.clone(),
                authority.durable.clone(),
            )
        };
        let prior_manifest = prior.manifest();
        let prior_topology = prior_manifest.topology();
        let branch_revision = prior_topology
            .branch_revision
            .checked_add(1)
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "branch revision space was exhausted",
                )
            })?;
        if let Some(branch) = &durable
            && branch_revision != branch.head.revision().get().saturating_add(1)
        {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "runtime checkpoint revision disagrees with the durable head",
            ));
        }
        let lease_fence = durable
            .as_ref()
            .map_or(prior_topology.lease_fence, |branch| {
                branch.lease.fence().get()
            });
        let CheckpointCandidate {
            checkpoint,
            bundle,
            expected_ref,
            commit_identity,
        } = successor_candidate(
            &prior,
            &prior_bundle,
            facts,
            lease_fence,
            observation.or_else(|| prior_manifest.interpreter_observation().cloned()),
        )?;
        let mut accepted_head = None;
        if durable.is_some() {
            // Publication blocks the periodic heartbeat on the same gate. Give
            // its potentially slow store operation freshly acknowledged lease
            // authority rather than the remainder of an earlier heartbeat.
            let branch = renew_shared_durable_lease_under_gate(&self.state).await?;
            let request = CommitCheckpoint::new(branch.lease.clone(), branch.head, bundle.clone())
                .with_admission(admission)
                .with_feed_facts(crate::feed::facts::commit_facts(&prior, &checkpoint)?);
            let receipt = Box::pin(commit_with_acknowledgement_recovery(
                &branch.store,
                branch.store_timeouts.operation,
                checkpoint_id,
                request,
                branch.head,
                expected_ref,
                commit_identity,
                Some(telemetry),
            ))
            .await?;
            accepted_head = Some((receipt.head(), receipt.feed_seq()));
        }
        let mut authority = lock_mutex(&self.state);
        if authority.checkpoint.manifest().topology().checkpoint_id != prior_topology.checkpoint_id
        {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "checkpoint publication raced the single runtime owner",
            ));
        }
        authority.checkpoint = Arc::clone(&checkpoint);
        // Amortize cache trimming after acknowledged publication. Before a
        // store sweep, prune_superseded unconditionally discards old membership.
        authority.bundle = bundle.trimmed().unwrap_or(bundle);
        if let Some((head, feed_seq)) = accepted_head {
            authority.published_feed_seq = Some((checkpoint_id, feed_seq));
            let branch = authority.durable.as_mut().ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "durable branch context disappeared",
                )
            })?;
            branch.head = head;
        }
        Ok(checkpoint)
    }
    /// Reclaims superseded history at the current head when at least
    /// `minimum` checkpoints would go. Returns `None` without a durable
    /// branch or at a Working head: only a Quiescent head has no in-flight
    /// work whose recovery could still read older checkpoints.
    pub(in crate::runtime) async fn prune_superseded(
        &self,
        minimum: u64,
    ) -> Result<Option<PruneReceipt>, AgentError> {
        self.heartbeat.check()?;
        let gate = lock_mutex(&self.state).durable.as_ref().map(|branch| {
            (
                Arc::clone(&branch.lease_gate),
                branch.store_timeouts.operation,
            )
        });
        let Some((gate, timeout)) = gate else {
            return Ok(None);
        };
        if !self.may_reach_prune_minimum(minimum) {
            return Ok(None);
        }
        // The heartbeat renews under this gate; holding it keeps the lease
        // this request names the store's current one. The prune it guards
        // costs what it reclaims (at most about `minimum` checkpoints' closure
        // once caught up), and the store call is bounded by the operation
        // deadline.
        let _guard = tokio::time::timeout(timeout, gate.lock())
            .await
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "checkpoint lease gate exceeded its configured deadline",
                )
            })?;
        self.heartbeat.check()?;
        let (checkpoint, branch) = {
            let authority = lock_mutex(&self.state);
            (authority.checkpoint(), authority.durable.clone())
        };
        let Some(branch) = branch else {
            return Ok(None);
        };
        if checkpoint.kind() != RuntimeCheckpointKind::Quiescent {
            return Ok(None);
        }
        // Pruning may commit even if its acknowledgement is lost. Forget all
        // ancestral CAS membership before dispatch, or a later reconstruction
        // of a reclaimed address could be incorrectly omitted from its delta.
        // A failed closure rebuild must refuse the sweep, not retain the cache.
        {
            let mut authority = lock_mutex(&self.state);
            authority.bundle = authority
                .bundle
                .current_root_closure()
                .map_err(checkpoint_bundle_error)?;
        }
        let request = PruneBranch::new(
            branch.lease.clone(),
            branch.head,
            crate::runtime::checkpoint_retained_history(&checkpoint),
            minimum,
        );
        let receipt = store_call(
            StorePhase::Prune,
            branch.store_timeouts.operation,
            branch.store.prune(request),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let mut authority = lock_mutex(&self.state);
        authority.pruned_horizon = authority.pruned_horizon.max(receipt.horizon().get());
        Ok(Some(receipt))
    }

    /// Whether the checkpoints strictly between the known horizon and the
    /// head's parent could number `minimum`. When they cannot, neither the
    /// retained set nor a store transaction is needed.
    fn may_reach_prune_minimum(&self, minimum: u64) -> bool {
        let authority = lock_mutex(&self.state);
        let Some(branch) = authority.durable.as_ref() else {
            return false;
        };
        let parent = branch.head.revision().get().saturating_sub(1);
        parent.saturating_sub(authority.pruned_horizon) >= minimum
    }
    pub(in crate::runtime) async fn retire(
        &mut self,
        fallback_timeout: Duration,
        telemetry: Option<&DurabilityCommitTelemetry>,
    ) -> [Result<(), AgentError>; 2] {
        if lock_mutex(&self.state).abandoned {
            return [Ok(()), Ok(())];
        }
        let cleanup_timeout = lock_mutex(&self.state)
            .durable
            .as_ref()
            .map_or(fallback_timeout, |branch| branch.store_timeouts.cleanup);
        // The lease heartbeat must be joined before the branch context is read.
        // A renewal that lands after the read but before the release would
        // leave this cleanup holding a superseded lease, and the store refuses
        // that release fail-closed. Joining first makes the authority's lease
        // the exact lease the store made active.
        let heartbeat_started = std::time::Instant::now();
        let heartbeat_result =
            contain_cleanup_panic(self.heartbeat.stop_and_join(cleanup_timeout)).await;
        if let Some(telemetry) = telemetry {
            telemetry.emit_cleanup(
                heartbeat_started.elapsed(),
                DurabilityAttemptKind::CleanupHeartbeatJoin,
                &heartbeat_result,
            );
        }
        let durable = lock_mutex(&self.state).durable.clone();
        let store_result = if let Some(branch) = durable {
            let release_started = std::time::Instant::now();
            let release = contain_cleanup_panic(async {
                store_call(
                    StorePhase::Release,
                    branch.store_timeouts.cleanup,
                    branch.store.release(branch.lease.clone()),
                )
                .await
                .map_err(StoreCallError::into_agent)
            })
            .await;
            if let Some(telemetry) = telemetry {
                telemetry.emit_cleanup(
                    release_started.elapsed(),
                    DurabilityAttemptKind::CleanupLeaseRelease,
                    &release,
                );
            }
            if release.is_ok() {
                lock_mutex(&self.state).released = true;
            }
            let flush_started = std::time::Instant::now();
            let flush = contain_cleanup_panic(async {
                store_call(
                    StorePhase::Flush,
                    branch.store_timeouts.cleanup,
                    branch.store.flush(),
                )
                .await
                .map_err(StoreCallError::into_agent)
                .and_then(|report| {
                    if report.is_complete() {
                        Ok(())
                    } else {
                        Err(AgentError::new(
                            AgentErrorKind::Session,
                            "durable session flush retained unresolved checkpoint debt",
                        ))
                    }
                })
            })
            .await;
            if let Some(telemetry) = telemetry {
                telemetry.emit_cleanup(
                    flush_started.elapsed(),
                    DurabilityAttemptKind::CleanupFlush,
                    &flush,
                );
            }
            combine_cleanup_results([release, flush])
        } else {
            lock_mutex(&self.state).released = true;
            Ok(())
        };
        [heartbeat_result, store_result]
    }
    pub(in crate::runtime) async fn load_checkpoint(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let branch = lock_mutex(&self.state).durable.clone().ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "historical terminal checkpoint is unavailable from memory-only authority",
            )
        })?;
        let stored = store_call(
            StorePhase::LoadCheckpoint,
            branch.store_timeouts.operation,
            branch.store.load_checkpoint(checkpoint_id),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        if stored.reference().checkpoint_id() != checkpoint_id {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "historical terminal checkpoint reference disagrees with its result record",
            ));
        }
        Ok(Arc::clone(stored.checkpoint()))
    }
}

impl CheckpointReader {
    pub(in crate::runtime) fn lease_released(&self) -> bool {
        lock_mutex(&self.state).released
    }

    pub(in crate::runtime) fn mark_lease_released(&self) {
        lock_mutex(&self.state).released = true;
    }
    // Exit has fenced the actor and stopped renewal. Publish only terminal
    // evidence under its lease gate, before the batch releases that lease.
    pub(in crate::runtime) async fn record_abandoned_job_terminals(
        &self,
        views: &[plakous_tools::JobView],
    ) -> Result<(), AgentError> {
        if views.is_empty() {
            return Ok(());
        }
        let gate = lock_mutex(&self.state)
            .durable
            .as_ref()
            .map(|branch| Arc::clone(&branch.lease_gate));
        let _guard = match gate {
            Some(gate) => Some(gate.lock_owned().await),
            None => None,
        };
        let (prior, bundle, durable) = {
            let state = lock_mutex(&self.state);
            if !state.abandoned {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "exit publication requires abandonment",
                ));
            }
            (
                state.checkpoint(),
                state.bundle.clone(),
                state.durable.clone(),
            )
        };
        let manifest = prior.manifest();
        if manifest.lifecycle() != &crate::RuntimeLifecycle::Quiescent {
            return Ok(());
        }
        let jobs = manifest
            .jobs()
            .with_terminals(views)
            .map_err(crate::runtime::continuation::runtime_manifest_error)?;
        if jobs == *manifest.jobs() {
            return Ok(());
        }
        let checkpoint_id = CheckpointId::generate(
            &plakous_model::SystemClock,
            &plakous_model::SystemIdGenerator,
        )
        .map_err(|error| AgentError::from_identity(&error))?;
        let candidate = successor_candidate(
            &prior,
            &bundle,
            SuccessorFacts {
                checkpoint_id,
                lifecycle: crate::RuntimeLifecycle::Quiescent,
                history: manifest.canonical().history().clone(),
                ledger: manifest.ledger().clone(),
                jobs,
                context: manifest.canonical().context().retained_for_exact_resume(),
                family: manifest.family_artifact().cloned(),
            },
            durable
                .as_ref()
                .map_or(manifest.topology().lease_fence, |branch| {
                    branch.lease.fence().get()
                }),
            manifest.interpreter_observation().cloned(),
        )?;
        let head = if let Some(branch) = &durable {
            Some(
                Box::pin(commit_with_acknowledgement_recovery(
                    &branch.store,
                    branch.store_timeouts.operation,
                    checkpoint_id,
                    CommitCheckpoint::new(
                        branch.lease.clone(),
                        branch.head,
                        candidate.bundle.clone(),
                    )
                    .with_admission(StoreAdmission::Settlement)
                    .with_feed_facts(crate::feed::facts::commit_facts(
                        &prior,
                        &candidate.checkpoint,
                    )?),
                    branch.head,
                    candidate.expected_ref,
                    candidate.commit_identity,
                    None,
                ))
                .await?,
            )
        } else {
            None
        };
        let mut state = lock_mutex(&self.state);
        state.checkpoint = candidate.checkpoint;
        state.bundle = candidate.bundle;
        if let Some(receipt) = head
            && let Some(branch) = &mut state.durable
        {
            branch.head = receipt.head();
            state.published_feed_seq = Some((checkpoint_id, receipt.feed_seq()));
        }
        Ok(())
    }

    pub(in crate::runtime) fn abandon(&self) {
        lock_mutex(&self.state).abandoned = true;
        self.heartbeat_stop.cancel();
        self.heartbeat_lost.cancel();
    }

    pub(in crate::runtime) async fn abandoned_lease(
        &self,
    ) -> Option<(
        Arc<dyn SessionStore>,
        BranchLease,
        tokio::sync::OwnedMutexGuard<()>,
    )> {
        let gate = lock_mutex(&self.state)
            .durable
            .as_ref()
            .map(|branch| Arc::clone(&branch.lease_gate))?;
        let guard = gate.lock_owned().await;
        let branch = lock_mutex(&self.state).durable.clone()?;
        Some((branch.store, branch.lease, guard))
    }
    pub(in crate::runtime) fn checkpoint(&self) -> Arc<RuntimeCheckpoint> {
        lock_mutex(&self.state).checkpoint()
    }
    pub(in crate::runtime) fn conversation(&self) -> ConversationView {
        lock_mutex(&self.state).conversation()
    }
    /// Runs `observe` while no checkpoint can be published and returns the
    /// committed history, topology and immutable ledger it ran against, so
    /// all four describe one instant.
    pub(in crate::runtime) fn committed_history_with<R>(
        &self,
        observe: impl FnOnce() -> R,
    ) -> (
        plakous_model::CanonicalHistory,
        crate::manifest::RuntimeTopology,
        crate::manifest::RuntimeOperationLedger,
        R,
    ) {
        let state = lock_mutex(&self.state);
        let manifest = state.checkpoint.manifest();
        let history = manifest.canonical().history().clone();
        let topology = manifest.topology();
        (history, topology, manifest.ledger().clone(), observe())
    }
    #[cfg(test)]
    pub(in crate::runtime) fn lease_owner(&self) -> Option<plakous_model::RunId> {
        lock_mutex(&self.state)
            .durable
            .as_ref()
            .map(|branch| branch.lease.owner())
    }
    #[cfg(test)]
    pub(in crate::runtime) async fn pause_lease(&self) -> Option<StartAdmission> {
        let branch = lock_mutex(&self.state).durable.clone()?;
        Some(StartAdmission {
            _guard: Some(branch.lease_gate.lock_owned().await),
        })
    }
    #[cfg(test)]
    pub(in crate::runtime) async fn renew_lease(&self) -> Result<(), AgentError> {
        renew_shared_durable_lease(&self.state).await
    }
}

impl DurableBranchFacts {
    pub(in crate::runtime) fn new(
        store: Arc<dyn SessionStore>,
        head: BranchHead,
        lease: BranchLease,
        store_timeouts: StoreTimeouts,
    ) -> Self {
        Self {
            store,
            head,
            lease,
            store_timeouts,
        }
    }
    pub(in crate::runtime) fn store(&self) -> &Arc<dyn SessionStore> {
        &self.store
    }
    pub(in crate::runtime) fn head(&self) -> BranchHead {
        self.head
    }
    pub(in crate::runtime) fn lease(&self) -> &BranchLease {
        &self.lease
    }
    pub(in crate::runtime) fn store_timeouts(&self) -> StoreTimeouts {
        self.store_timeouts
    }
}

fn uncommitted_tool_calls(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
) -> Result<Vec<(CallId, ToolName)>, AgentError> {
    let manifest = checkpoint.manifest();
    let mut calls = BTreeMap::new();
    for segment in manifest.canonical().working().segments() {
        for item in segment.items() {
            for block in item.blocks() {
                if let ContentBlock::ToolCall(call) = block {
                    calls.insert(call.call_id(), call.exposed_name().clone());
                }
            }
        }
    }
    // Nested calls live in the ledger, not model history. Finalized operations
    // already committed their semantic exchange and must not be rolled back.
    for record in manifest.ledger().operations() {
        if matches!(record.state(), crate::DurableOperationState::Finalized(_)) {
            continue;
        }
        match record.kind() {
            crate::DurableOperationKind::Effect {
                turn_id: root,
                call_id,
                ..
            } if *root == turn_id => {
                let request = crate::runtime::durable::effect_recovery::decode_effect_call(record)?;
                let call = request.call();
                if call.call_id() != *call_id {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "abandoned tool identity conflicts",
                    ));
                }
                calls.insert(*call_id, call.exposed_name().clone());
            }
            crate::DurableOperationKind::CompositionChild {
                turn_id: root,
                call_id,
                ..
            } if *root == turn_id => {
                type ChildEdge = (String, u64, u64, String, u32, Digest, Vec<u8>);
                type ChildRequest = (String, String, String, ToolName, serde::de::IgnoredAny);
                let edge: ChildEdge =
                    serde_json::from_slice(record.request().bytes()).map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Session,
                            "abandoned nested tool identity is invalid",
                        )
                    })?;
                let request: ChildRequest = serde_json::from_slice(&edge.6).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Session,
                        "abandoned nested tool name is invalid",
                    )
                })?;
                if edge.0 != "plakous-local-composition-child-edge-v2"
                    || request.0 != "plakous-local-composition-child-request-v1"
                {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "abandoned nested tool identity conflicts",
                    ));
                }
                calls.insert(*call_id, request.3);
            }
            _ => {}
        }
    }
    Ok(calls.into_iter().collect())
}

/// Construction ownership before any reader, start capability or heartbeat exists.
pub(in crate::runtime) struct PreparedCheckpoint {
    state: CheckpointAuthority,
}

impl PreparedCheckpoint {
    pub(in crate::runtime) fn durable_facts(&self) -> Option<DurableBranchFacts> {
        self.state
            .durable
            .as_ref()
            .map(|branch| DurableBranchFacts {
                store: Arc::clone(&branch.store),
                head: branch.head,
                lease: branch.lease.clone(),
                store_timeouts: branch.store_timeouts,
            })
    }

    pub(in crate::runtime) fn new(
        checkpoint: Arc<RuntimeCheckpoint>,
        bundle: CheckpointObjectBundle,
        durable: Option<DurableBranchFacts>,
    ) -> Self {
        Self {
            state: CheckpointAuthority {
                checkpoint,
                bundle,
                durable: durable.map(|facts| DurableBranchContext {
                    store: facts.store,
                    head: facts.head,
                    lease: facts.lease,
                    lease_gate: Arc::new(AsyncMutex::new(())),
                    store_timeouts: facts.store_timeouts,
                }),
                published_feed_seq: None,
                pruned_horizon: 0,
                abandoned: false,
                released: false,
            },
        }
    }
    pub(in crate::runtime) fn checkpoint(&self) -> Arc<RuntimeCheckpoint> {
        self.state.checkpoint()
    }
    pub(in crate::runtime) fn conversation(&self) -> ConversationView {
        self.state.conversation()
    }
    pub(in crate::runtime) fn start(self) -> CheckpointOwner {
        let state = Arc::new(Mutex::new(self.state));
        let heartbeat = LeaseHeartbeat::start(Arc::clone(&state));
        CheckpointOwner { state, heartbeat }
    }
    /// Publishes one pure closure under the newly opened owner's fenced lease.
    pub(in crate::runtime) async fn publish_root_turn_closure(
        &mut self,
        telemetry: &DurabilityCommitTelemetry,
        workflow: WorkflowClosureRuntime<'_>,
    ) -> Result<Option<RootTurnRecovery>, AgentError> {
        let authority = &mut self.state;
        if let Some(closure) = verdict_workflow_closure(&authority.checkpoint) {
            let branch = authority.durable.as_mut().ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "workflow recovery lost its durable branch authority",
                )
            })?;
            let (checkpoint, bundle, head) = Box::pin(publish_workflow_closure(
                WorkflowClosureBranch {
                    store: Arc::clone(&branch.store),
                    store_timeout: branch.store_timeouts.operation,
                    lease: branch.lease.clone(),
                    head: branch.head,
                    checkpoint: Arc::clone(&authority.checkpoint),
                    bundle: authority.bundle.clone(),
                },
                closure,
                workflow,
            ))
            .await?;
            authority.checkpoint = checkpoint;
            authority.bundle = bundle;
            branch.head = head;
            return Ok(Some(RootTurnRecovery {
                abandoned_turn: None,
            }));
        }
        let closure = match verdict_closure(&authority.checkpoint) {
            Ok(closure) => closure,
            Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => None,
            Err(error) => return Err(error),
        };
        let Some(closure) = closure else {
            return Ok(None);
        };
        let command_id = closure.command_id().clone();
        let branch = authority.durable.as_mut().ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "root turn recovery lost its durable branch authority",
            )
        })?;
        require_recoverable(&authority.checkpoint).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "root turn recovery scope is not recoverable by this build",
            )
        })?;
        let checkpoint_id = deterministic_checkpoint_id(branch.lease.owner(), 1)?;
        let (checkpoint, bundle, head) = recover_root_turn_publication(
            &branch.store,
            &branch.lease,
            branch.head,
            &authority.checkpoint,
            &authority.bundle,
            closure,
            authority.checkpoint.manifest().lifecycle().obligations(),
            branch.store_timeouts.operation,
            checkpoint_id,
            Some(telemetry),
        )
        .await?;
        require_recoverable(&checkpoint).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "recovered checkpoint scope is not recoverable by this build",
            )
        })?;
        let report =
            super::super::continuation::terminal_record_at_checkpoint(&checkpoint, &command_id)?
                .abandonment;
        authority.checkpoint = checkpoint;
        authority.bundle = bundle;
        branch.head = head;
        Ok(Some(RootTurnRecovery {
            abandoned_turn: report,
        }))
    }
}

pub(in crate::runtime) struct RootTurnRecovery {
    pub(in crate::runtime) abandoned_turn: Option<crate::runtime::AbandonedTurnReport>,
}

pub(in crate::runtime) fn root_closure_report(
    checkpoint: &RuntimeCheckpoint,
    closure: &RootClosure,
) -> Result<Option<crate::runtime::AbandonedTurnReport>, AgentError> {
    let (turn_id, mut abandoned, mut effects, claim, cause, completed_tool_round) = match closure {
        RootClosure::Abandon { fold, claim } => (
            fold.turn_id,
            fold.provider_operations
                .iter()
                .chain(&fold.effect_operations)
                .chain(&fold.uncommitted_effect_operations)
                .chain(&fold.unrun_effect_operations)
                .chain(&fold.process_operations)
                .chain(&fold.compaction_operations)
                .map(|operation| operation.0)
                .collect::<Vec<_>>(),
            fold.effect_operations
                .iter()
                .chain(&fold.uncommitted_effect_operations)
                .chain(&fold.process_operations)
                .map(|operation| operation.0)
                .collect::<Vec<_>>(),
            *claim,
            abandonment_cause(fold),
            fold.tool_round_completed,
        ),
        RootClosure::FinalizeRecorded {
            code_cell: Some(cell),
            ..
        } => {
            let effects = cell
                .operations
                .iter()
                .filter(|(_, disposition)| {
                    *disposition == DurableCodeCellAbandonmentDisposition::PossiblyStarted
                })
                .map(|(operation, _)| operation.0)
                .collect::<Vec<_>>();
            let claim = if effects.is_empty() {
                AbandonedTurnClaim::NothingStarted
            } else {
                AbandonedTurnClaim::RunningTool
            };
            (
                cell.turn_id,
                cell.operations
                    .iter()
                    .map(|(operation, _)| operation.0)
                    .collect::<Vec<_>>(),
                effects,
                claim,
                AbandonmentCause::Interrupted,
                false,
            )
        }
        RootClosure::FinalizeRecorded {
            code_cell: None, ..
        }
        | RootClosure::Complete(_) => {
            return Ok(None);
        }
    };
    abandoned.sort_unstable();
    effects.sort_unstable();
    let uncommitted_calls = uncommitted_tool_calls(checkpoint, turn_id)?;
    Ok(Some(
        crate::runtime::AbandonedTurnReport::new(
            abandoned,
            effects,
            uncommitted_calls,
            claim,
            cause,
        )
        .with_completed_tool_round(completed_tool_round)
        .with_completed_model_round(matches!(closure, RootClosure::Abandon { fold, .. }
            if fold.tool_round_completed && crate::runtime::durable::turn_recovery::model_round_boundary(checkpoint, fold.turn_id, fold.root_operation))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_bundle::tests::{
        fixture_with_history, fixture_with_history_and_ledger, history_segment,
        without_cached_objects,
    };
    use crate::internal::{LeaseDomain, LeaseInstant, RuntimeObject};
    use crate::runtime::ToolExecutionResult;
    use crate::runtime::continuation::runtime_component;
    use crate::runtime::driver::DurableUndispatchedToolRequest;
    use crate::runtime::operation_record::{tool_operation_terminal_state, working_lifecycle};
    use crate::*;
    use plakous_model::{
        AttemptId, CallId, CanonicalHistory, DigestDomain, EntryId, ToolCallBlock, ToolCallState,
        ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind,
    };
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;
    use std::sync::Weak;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn abandoned_undispatched_call_is_named(proven_not_dispatched: bool) -> TestResult {
        let turn_id = TurnId::try_from(uuid::Uuid::from_u128(900))?;
        let call = ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(902))?,
            ToolName::new("loopback_effect")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{}",
        )?;
        let result = ToolResultBlock::new(
            call.call_id(),
            ToolExecutionOwner::PlakLocal,
            ToolResultKind::Failure,
            plakous_tools::ToolOutcome::failure_text("superseded by steering")?
                .parts()
                .to_vec(),
            call.correlation().clone(),
        )?;
        let execution = ToolExecutionResult::new(result, false);
        let binding = Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"undispatched"])?;
        let request: DurableUndispatchedToolRequest = (
            "proven-not-dispatched-tool-v1".to_owned(),
            binding,
            call.clone(),
            EntryId::try_from(uuid::Uuid::from_u128(903))?,
            execution.clone(),
        );
        let bytes = serde_json::to_vec(&request)?;
        let component = runtime_component(RuntimeComponentKind::Operation, bytes.clone())?;
        let mut record = DurableOperationRecord::new(
            1,
            DurableOperationKind::Effect {
                turn_id,
                attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(901))?,
                call_id: call.call_id(),
                source_index: 0,
            },
            component,
            vec![DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(910))?,
                DurableOperationState::Prepared,
            )],
        )?;
        if proven_not_dispatched {
            record = record.append_transition(DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(911))?,
                tool_operation_terminal_state(&Ok(execution), false, request.3)?,
            ))?;
        }
        let ledger = RuntimeOperationLedger::new(Vec::new(), vec![record])?;
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        let checkpoint = fixture_with_history_and_ledger(
            CanonicalHistory::default(),
            1,
            true,
            Some(ledger),
            Some(lifecycle),
        )?;
        assert_eq!(
            uncommitted_tool_calls(&checkpoint, turn_id).map_err(|error| error.to_string())?,
            vec![(call.call_id(), call.exposed_name().clone())]
        );
        let retained = checkpoint
            .manifest()
            .ledger()
            .operation(1)
            .ok_or("effect")?;
        assert_eq!(retained.request().bytes(), bytes);
        assert_eq!(
            matches!(
                retained.state(),
                DurableOperationState::ProvenNotDispatched(_)
            ),
            proven_not_dispatched
        );
        Ok(())
    }

    #[test]
    fn abandoned_prepared_undispatched_call_is_named_without_dispatch() -> TestResult {
        abandoned_undispatched_call_is_named(false)
    }

    #[test]
    fn abandoned_proven_not_dispatched_call_is_named_without_dispatch() -> TestResult {
        abandoned_undispatched_call_is_named(true)
    }

    /// A deterministic CAS sweep with optional lost acknowledgement. Commit
    /// verification uses only supplied objects and the surviving CAS, never
    /// the publication's warm cache.
    #[derive(Default)]
    struct SweepStore {
        objects: Mutex<BTreeMap<RuntimeObjectAddress, RuntimeObject>>,
        authority: Mutex<Weak<Mutex<CheckpointAuthority>>>,
        lose_ack: Mutex<bool>,
        calls: Mutex<usize>,
    }

    impl SweepStore {
        fn accept(&self, bundle: &CheckpointObjectBundle) -> TestResult {
            let mut objects = lock_mutex(&self.objects);
            for object in bundle.supplied_objects() {
                objects.insert(object.address(), object.clone());
            }
            let closure = Self::closure(&objects, bundle.root())?;
            CheckpointObjectBundle::from_store(
                bundle.checkpoint_id(),
                bundle.checkpoint_integrity(),
                bundle.root(),
                closure.into_values().collect(),
            )?;
            Ok(())
        }

        fn closure(
            objects: &BTreeMap<RuntimeObjectAddress, RuntimeObject>,
            root: RuntimeObjectAddress,
        ) -> TestResult<BTreeMap<RuntimeObjectAddress, RuntimeObject>> {
            let mut closure = BTreeMap::new();
            let mut pending = vec![root];
            while let Some(address) = pending.pop() {
                if closure.contains_key(&address) {
                    continue;
                }
                let object = objects.get(&address).ok_or("missing committed object")?;
                pending.extend_from_slice(object.children());
                closure.insert(address, object.clone());
            }
            Ok(closure)
        }
    }

    macro_rules! unused_store_methods {
        ($($name:ident($arg:ident: $input:ty) -> $output:ty;)*) => {
            $(fn $name(&self, _: $input) -> StoreFuture<'_, $output> {
                Box::pin(async { Err(SessionStoreError::BackendUnavailable) })
            })*
        };
    }

    impl SessionStore for SweepStore {
        fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
            LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 0, 0)
        }

        unused_store_methods! {
            create(request: CreateSession) -> CreateReceipt;
            recover_create(request: CreateSession) -> CreateReceipt;
            load_branch(session: SessionRef) -> LoadedBranch;
            load_checkpoint(checkpoint: CheckpointId) -> StoredCheckpoint;
            acquire(request: AcquireBranch) -> BranchLease;
            renew(lease: BranchLease) -> BranchLease;
            release(lease: BranchLease) -> ();
            commit(request: CommitCheckpoint) -> CommitReceipt;
            recover_commit(checkpoint: CheckpointId) -> CommittedCheckpoint;
            fork(request: ForkBranch) -> ForkReceipt;
            recover_fork(request: RecoverFork) -> ForkReceipt;
            list(query: SessionQuery) -> Page<SessionSummary>;
            archive(request: ArchiveSession) -> ArchiveReceipt;
            delete(request: DeleteSession) -> DeleteReceipt;
            recover_delete(request: DeleteSession) -> DeleteReceipt;
            feed_tail(session: SessionId) -> feed::FeedTail;
        }

        fn read_feed(
            &self,
            _: SessionId,
            _: FeedSeq,
            _: std::num::NonZeroUsize,
        ) -> StoreFuture<'_, feed::FeedPage> {
            Box::pin(async { Err(SessionStoreError::BackendUnavailable) })
        }

        fn usage(&self) -> StoreFuture<'_, StoreUsage> {
            Box::pin(async { Err(SessionStoreError::BackendUnavailable) })
        }
        fn flush(&self) -> StoreFuture<'_, DurabilityReport> {
            Box::pin(async { Err(SessionStoreError::BackendUnavailable) })
        }
        fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
            self.flush()
        }

        fn prune(&self, request: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
            Box::pin(async move {
                *lock_mutex(&self.calls) += 1;
                let state = lock_mutex(&self.authority)
                    .upgrade()
                    .ok_or(SessionStoreError::corrupt())?;
                let authority = lock_mutex(&state);
                let mut objects = lock_mutex(&self.objects);
                let closure = Self::closure(&objects, authority.bundle.root())
                    .map_err(|_| SessionStoreError::corrupt())?;
                // The cache must be narrowed before dispatch, not on receipt.
                assert_eq!(authority.bundle.stats().object_count(), closure.len());
                *objects = closure;
                if *lock_mutex(&self.lose_ack) {
                    return Err(SessionStoreError::BackendUnavailable);
                }
                Ok(PruneReceipt::from_store(
                    request.expected(),
                    BranchRevision::from_store(request.expected().revision().get() - 1),
                    1,
                ))
            })
        }
    }

    fn checkpoint(revision: u128, tail: usize) -> TestResult<Arc<RuntimeCheckpoint>> {
        // Ballast ensures the warm cache remains below both amortized thresholds.
        let mut segments = (0..32)
            .map(history_segment)
            .collect::<TestResult<Vec<_>>>()?;
        segments.push(history_segment(tail)?);
        Ok(Arc::new(fixture_with_history(
            CanonicalHistory::new(segments)?,
            revision,
            false,
        )?))
    }

    fn install(
        owner: &CheckpointOwner,
        checkpoint: Arc<RuntimeCheckpoint>,
        bundle: CheckpointObjectBundle,
    ) {
        let mut authority = lock_mutex(&owner.state);
        let topology = checkpoint.manifest().topology();
        let reference = CheckpointObjectRef::from_store(
            topology.checkpoint_id,
            bundle.root(),
            checkpoint.integrity(),
        );
        if let Some(branch) = authority.durable.as_mut() {
            branch.head = BranchHead::from_store(
                branch.head.session(),
                BranchRevision::from_store(topology.branch_revision),
                reference,
            );
        }
        authority.checkpoint = checkpoint;
        authority.bundle = bundle;
    }

    #[tokio::test]
    async fn warm_owner_reconstructs_reclaimed_addresses_even_after_lost_prune_ack() -> TestResult {
        let first = checkpoint(1, 1_000)?;
        let bundle = RuntimeCheckpointCodec::bundle(&first, None)?;
        let store = Arc::new(SweepStore::default());
        store.accept(&bundle)?;
        let topology = first.manifest().topology();
        let session = SessionRef::new(topology.session_id, topology.branch_id);
        let head = BranchHead::from_store(
            session,
            BranchRevision::from_store(1),
            CheckpointObjectRef::from_store(
                topology.checkpoint_id,
                bundle.root(),
                first.integrity(),
            ),
        );
        let lease = BranchLease::from_store(
            session,
            RunId::try_from(uuid::Uuid::from_u128(99))?,
            NonZeroU64::new(1).ok_or("nonzero fence")?,
            LeaseInstant::new(LeaseDomain::new([1; 16], [2; 32]), 1_000, 0)?,
            NonZeroU64::new(1_000).ok_or("nonzero interval")?,
        );
        let mut owner = PreparedCheckpoint::new(
            first,
            bundle,
            Some(DurableBranchFacts::new(
                store.clone(),
                head,
                lease,
                StoreTimeouts::default(),
            )),
        )
        .start();
        *lock_mutex(&store.authority) = Arc::downgrade(&owner.state);
        for cycle in 0..4_u128 {
            let tail = if cycle % 2 == 0 { 2_000 } else { 1_000 };
            let next = checkpoint(cycle + 2, tail)?;
            let prior = lock_mutex(&owner.state).bundle.clone();
            let publication = RuntimeCheckpointCodec::advance(&next, &prior)?;
            // This is an acknowledged publication into the test CAS.
            store.accept(&publication)?;
            let cached = publication.trimmed()?;
            let cold = RuntimeCheckpointCodec::bundle(&next, None)?;
            assert!(
                cached.stats().object_count() > cold.stats().object_count(),
                "ordinary trimming must leave obsolete membership in this regression"
            );
            install(&owner, next, cached);
            *lock_mutex(&store.lose_ack) = cycle % 2 == 0;
            let result = owner.prune_superseded(1).await;
            assert_eq!(result.is_err(), cycle % 2 == 0);
            let obsolete = history_segment(if tail == 2_000 { 1_000 } else { 2_000 })?;
            let obsolete_checkpoint =
                fixture_with_history(CanonicalHistory::new(vec![obsolete])?, 1, false)?;
            let obsolete_bundle = RuntimeCheckpointCodec::bundle(&obsolete_checkpoint, None)?;
            let address = obsolete_bundle
                .supplied_objects()
                .iter()
                .find(|object| object.address().kind() == RuntimeObjectKind::HistorySegment)
                .ok_or("history object")?
                .address();
            assert!(
                !lock_mutex(&store.objects).contains_key(&address),
                "old object was reclaimed"
            );
            // Reopen the exact surviving closure and continue on the same warm owner.
            let cache = lock_mutex(&owner.state).bundle.clone();
            let reopened = CheckpointObjectBundle::from_store(
                cache.checkpoint_id(),
                cache.checkpoint_integrity(),
                cache.root(),
                SweepStore::closure(&lock_mutex(&store.objects), cache.root())?
                    .into_values()
                    .collect(),
            )?;
            assert_eq!(
                reopened.validated_checkpoint()?.as_ref(),
                owner.checkpoint().as_ref()
            );
        }
        assert_eq!(*lock_mutex(&store.calls), 4);
        {
            let mut authority = lock_mutex(&owner.state);
            authority.bundle = without_cached_objects(&authority.bundle);
        }
        assert!(owner.prune_superseded(0).await.is_err());
        assert_eq!(
            *lock_mutex(&store.calls),
            4,
            "invalid cache refuses the sweep"
        );
        owner
            .heartbeat
            .stop_and_join(Duration::from_secs(1))
            .await?;
        Ok(())
    }
}
