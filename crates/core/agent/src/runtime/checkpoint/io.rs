//! Bounded store calls and exact commit acknowledgement recovery.

use crate::{
    AgentError, AgentErrorKind, BranchHead, BranchLease, CheckpointId, CheckpointObjectRef,
    CommitCheckpoint, CommitReceipt, CommittedCheckpoint, DurabilityAttemptKind,
    DurabilityAttemptOutcome, SessionStore, SessionStoreError, TelemetryContext, TelemetryEmitter,
    TelemetryObservation, TelemetryObservationKind, TelemetryTerminalStatus,
};
use plakous_model::{Clock, Digest};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
#[derive(Clone, Copy)]
pub(in crate::runtime) enum StorePhase {
    Create,
    RecoverCreate,
    Fork,
    RecoverFork,
    LoadBranch,
    LoadCheckpoint,
    ReadFeed,
    Acquire,
    Commit,
    RecoverCommit,
    Renew,
    Release,
    Prune,
    Flush,
    Shutdown,
}

impl StorePhase {
    const fn timeout_message(self) -> &'static str {
        match self {
            Self::Create => "session store create phase exceeded its configured deadline",
            Self::RecoverCreate => {
                "session store create-recovery phase exceeded its configured deadline"
            }
            Self::Fork => "session store fork phase exceeded its configured deadline",
            Self::RecoverFork => {
                "session store fork-recovery phase exceeded its configured deadline"
            }
            Self::LoadBranch => "session store branch-load phase exceeded its configured deadline",
            Self::LoadCheckpoint => {
                "session store checkpoint-load phase exceeded its configured deadline"
            }
            Self::ReadFeed => "session store feed-read phase exceeded its configured deadline",
            Self::Acquire => "session store lease-acquire phase exceeded its configured deadline",
            Self::Commit => "session store commit phase exceeded its configured deadline",
            Self::RecoverCommit => {
                "session store commit-recovery phase exceeded its configured deadline"
            }
            Self::Renew => "session store lease-renew phase exceeded its configured deadline",
            Self::Release => "session store lease-release phase exceeded its configured deadline",
            Self::Prune => "session store prune phase exceeded its configured deadline",
            Self::Flush => "session store flush phase exceeded its configured deadline",
            Self::Shutdown => "session store shutdown phase exceeded its configured deadline",
        }
    }
}

pub(in crate::runtime) enum StoreCallError {
    Store(SessionStoreError),
    Timeout(StorePhase),
}

impl StoreCallError {
    pub(in crate::runtime) fn is(&self, expected: SessionStoreError) -> bool {
        matches!(self, Self::Store(error) if *error == expected)
    }

    pub(in crate::runtime) fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout(_)
                | Self::Store(
                    SessionStoreError::BackendUnavailable | SessionStoreError::BackendBusy
                )
        )
    }

    pub(in crate::runtime) fn into_agent(self) -> AgentError {
        match self {
            Self::Store(error) => AgentError::from_store(error),
            Self::Timeout(phase) => AgentError::from_canonical_parts(
                AgentErrorKind::Session,
                phase.timeout_message(),
                None,
                None,
                Some(SessionStoreError::BackendBusy),
            ),
        }
    }

    pub(in crate::runtime) fn into_ambiguous_agent(self) -> AgentError {
        match self {
            Self::Store(error) => AgentError::from_ambiguous_store(error),
            Self::Timeout(phase) => AgentError::from_canonical_parts(
                AgentErrorKind::OutcomeUnknown,
                phase.timeout_message(),
                None,
                None,
                Some(SessionStoreError::BackendBusy),
            ),
        }
    }
}

pub(in crate::runtime) async fn store_call<T>(
    phase: StorePhase,
    timeout: Duration,
    future: impl Future<Output = Result<T, SessionStoreError>>,
) -> Result<T, StoreCallError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| StoreCallError::Timeout(phase))?
        .map_err(StoreCallError::Store)
}

pub(in crate::runtime) fn recovered_commit_matches_exact(
    recovered: &CommittedCheckpoint,
    lease: &BranchLease,
    head: BranchHead,
    expected: CheckpointObjectRef,
    commit_identity: Digest,
) -> bool {
    let operation = recovered.operation();
    let receipt = recovered.receipt();
    operation.lease() == lease
        && operation.expected() == head
        && operation.checkpoint() == expected
        && operation.checkpoint_commit_identity() == commit_identity
        && receipt.previous() == head
        && receipt.checkpoint() == expected
        && receipt.head().checkpoint() == expected
}

pub(in crate::runtime) struct DurabilityCommitTelemetry {
    pub(in crate::runtime) emitter: Arc<TelemetryEmitter>,
    pub(in crate::runtime) clock: Arc<dyn Clock>,
    pub(in crate::runtime) context: TelemetryContext,
}

impl DurabilityCommitTelemetry {
    pub(in crate::runtime) fn emit_cleanup<T>(
        &self,
        elapsed: Duration,
        phase: DurabilityAttemptKind,
        result: &Result<T, AgentError>,
    ) {
        self.emit_attempt(
            elapsed,
            phase,
            if result.is_ok() {
                DurabilityAttemptOutcome::Joined
            } else {
                DurabilityAttemptOutcome::Failed
            },
        );
    }

    fn emit(&self, kind: TelemetryObservationKind) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.emitter.emit(TelemetryObservation::new(
                self.context,
                self.clock.now(),
                kind,
            ));
        }));
    }

    fn emit_attempt(
        &self,
        elapsed: Duration,
        attempt: DurabilityAttemptKind,
        outcome: DurabilityAttemptOutcome,
    ) {
        self.emit(TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed,
            attempt,
            outcome,
        });
    }

    fn emit_finished(&self, elapsed: Duration, status: TelemetryTerminalStatus) {
        self.emit(TelemetryObservationKind::DurabilityFinished { elapsed, status });
    }
}

/// Bounded durability protocol: one commit, then at most one acknowledgement
/// recovery. Call sites retain `Box::pin` to bound already-large driver futures;
/// inlining this protocol previously overflowed the nested local-composition
/// scheduler's stack.
#[allow(
    clippy::too_many_arguments,
    reason = "the bounded commit protocol consumes the exact store, head, identity, and telemetry tuple"
)]
pub(in crate::runtime) async fn commit_with_acknowledgement_recovery(
    store: &Arc<dyn SessionStore>,
    timeout: Duration,
    checkpoint_id: CheckpointId,
    request: CommitCheckpoint,
    expected_previous: BranchHead,
    expected_checkpoint: CheckpointObjectRef,
    commit_identity: Digest,
    telemetry: Option<&DurabilityCommitTelemetry>,
) -> Result<CommitReceipt, AgentError> {
    let lease = request.lease().clone();
    let durability_started = std::time::Instant::now();
    let finish_durability = |status: TelemetryTerminalStatus| {
        if let Some(telemetry) = telemetry {
            telemetry.emit_finished(durability_started.elapsed(), status);
        }
    };
    let commit_started = std::time::Instant::now();
    let receipt = match store_call(StorePhase::Commit, timeout, store.commit(request)).await {
        Ok(receipt) => {
            if let Some(telemetry) = telemetry {
                telemetry.emit_attempt(
                    commit_started.elapsed(),
                    DurabilityAttemptKind::InitialCommit,
                    DurabilityAttemptOutcome::Committed,
                );
            }
            receipt
        }
        Err(commit_error) => {
            if let Some(telemetry) = telemetry {
                telemetry.emit_attempt(
                    commit_started.elapsed(),
                    DurabilityAttemptKind::InitialCommit,
                    DurabilityAttemptOutcome::Failed,
                );
            }
            // A failed recovery read cannot turn a proven rejection into
            // lost-ack ambiguity: these store refusals precede publication.
            if matches!(
                commit_error,
                StoreCallError::Store(
                    SessionStoreError::Corrupt(_)
                        | SessionStoreError::StorageFull
                        | SessionStoreError::Invalid(_)
                        | SessionStoreError::Capacity(_)
                )
            ) {
                finish_durability(TelemetryTerminalStatus::Failed);
                return Err(commit_error.into_agent());
            }
            let recovery_started = std::time::Instant::now();
            match store_call(
                StorePhase::RecoverCommit,
                timeout,
                store.recover_commit(checkpoint_id),
            )
            .await
            {
                Ok(recovered)
                    if recovered_commit_matches_exact(
                        &recovered,
                        &lease,
                        expected_previous,
                        expected_checkpoint,
                        commit_identity,
                    ) =>
                {
                    if let Some(telemetry) = telemetry {
                        telemetry.emit_attempt(
                            recovery_started.elapsed(),
                            DurabilityAttemptKind::InitialAcknowledgementRecovery,
                            DurabilityAttemptOutcome::RecoveredAcknowledgement,
                        );
                    }
                    recovered.receipt()
                }
                Ok(_) => {
                    if let Some(telemetry) = telemetry {
                        telemetry.emit_attempt(
                            recovery_started.elapsed(),
                            DurabilityAttemptKind::InitialAcknowledgementRecovery,
                            DurabilityAttemptOutcome::Failed,
                        );
                    }
                    finish_durability(TelemetryTerminalStatus::Failed);
                    return Err(AgentError::from_ambiguous_store(
                        SessionStoreError::corrupt(),
                    ));
                }
                Err(recovery) if recovery.is(SessionStoreError::NotFound) => {
                    if let Some(telemetry) = telemetry {
                        telemetry.emit_attempt(
                            recovery_started.elapsed(),
                            DurabilityAttemptKind::InitialAcknowledgementRecovery,
                            DurabilityAttemptOutcome::Failed,
                        );
                    }
                    finish_durability(TelemetryTerminalStatus::Failed);
                    return Err(if commit_error.is_retryable() {
                        commit_error.into_ambiguous_agent()
                    } else {
                        commit_error.into_agent()
                    });
                }
                Err(recovery_error) => {
                    if let Some(telemetry) = telemetry {
                        telemetry.emit_attempt(
                            recovery_started.elapsed(),
                            DurabilityAttemptKind::InitialAcknowledgementRecovery,
                            DurabilityAttemptOutcome::Failed,
                        );
                    }
                    finish_durability(TelemetryTerminalStatus::Failed);
                    return Err(recovery_error.into_ambiguous_agent());
                }
            }
        }
    };
    if receipt.previous() != expected_previous
        || receipt.checkpoint() != expected_checkpoint
        || receipt.head().checkpoint() != expected_checkpoint
    {
        finish_durability(TelemetryTerminalStatus::Failed);
        return Err(AgentError::from_ambiguous_store(
            SessionStoreError::corrupt(),
        ));
    }
    finish_durability(TelemetryTerminalStatus::Completed);
    Ok(receipt)
}
