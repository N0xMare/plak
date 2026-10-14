//! Repository-only inspection of exact decoded heads without runtime ownership.

use super::{
    AgentSessionsError, ExactSource, Path, SessionLocator, SessionRecoveryPhase,
    SqliteSessionStore, checkpoint_job_config, store_error,
};
use plakous_agent::internal::CheckpointRecoveryVerdict;

/// What the pure resume predicates permit, before credentialed reconstruction.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfflineReopenOutcome {
    /// No unsettled runtime work.
    OpensQuiescent,
    /// Saved local terminal evidence closes the working turn.
    RecoversSavedTerminal,
    /// Certified abandonment closes the working turn without redispatch.
    RecoversAbandonedTurn,
    /// Runtime open closes an admitted paid workflow without dispatch.
    ResolvesCompactionWorkflow,
    /// Resume restores an effect decision for the operator.
    RestoresEffectDecision,
    /// Resume restores a hosted continuation.
    RestoresHostedContinuation,
    /// Runtime open resolves a recorded pending commit.
    RecoversCommit,
    /// Runtime open restores another release-owned working frame.
    RestoresWorkingFrame,
    /// This working phase is outside the build's pure reopen closure.
    RefusesRecoveryScope,
}

/// Content-free checks on one exact head, not reconstructed runtime authority.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfflineReopenInspection {
    /// The same recovery-scope and closure predicates used by exact open.
    pub outcome: OfflineReopenOutcome,
    /// The recorded recovery phase, absent on a quiescent head.
    pub recovery_phase: Option<SessionRecoveryPhase>,
    /// The recorded evaluator's path, bytes, attachment and contract agree.
    /// Also true when this head has no evaluator.
    pub evaluator_verified: bool,
    /// The recorded process budgets decode without substituting defaults.
    pub job_config_verified: bool,
}

/// Reads one exact SQLite head without leases, boot changes, writes or dispatch.
/// This does not compile the harness or revalidate credentialed model authority.
///
/// # Errors
/// Returns typed schema, lookup, stale-locator or integrity failures. Refusals
/// from pure recovery and evaluator checks are returned as content-free fields.
#[doc(hidden)]
pub fn inspect_sqlite(
    path: impl AsRef<Path>,
    locator: SessionLocator,
) -> Result<OfflineReopenInspection, AgentSessionsError> {
    let loaded =
        SqliteSessionStore::inspect_branch(path, locator.session_ref()).map_err(store_error)?;
    let exact = ExactSource::validate(locator, &loaded)?;
    Ok(inspect_exact(&exact))
}

pub(super) fn inspect_exact(exact: &ExactSource) -> OfflineReopenInspection {
    let checkpoint = &exact.checkpoint;
    // One evidence load, one predicate: the same verdict the live open acts on.
    let verdict = plakous_agent::internal::checkpoint_recovery_verdict(checkpoint);
    let recovery_phase = exact.recovery_report().map(|report| report.phase());
    let outcome = match verdict {
        CheckpointRecoveryVerdict::Quiescent => OfflineReopenOutcome::OpensQuiescent,
        CheckpointRecoveryVerdict::RecoverCommit => OfflineReopenOutcome::RecoversCommit,
        CheckpointRecoveryVerdict::Abandon => OfflineReopenOutcome::RecoversAbandonedTurn,
        CheckpointRecoveryVerdict::Finish => OfflineReopenOutcome::RecoversSavedTerminal,
        CheckpointRecoveryVerdict::SettleWorkflow => {
            OfflineReopenOutcome::ResolvesCompactionWorkflow
        }
        CheckpointRecoveryVerdict::Restore(_) => match recovery_phase {
            Some(SessionRecoveryPhase::EffectDecision) => {
                OfflineReopenOutcome::RestoresEffectDecision
            }
            Some(SessionRecoveryPhase::HostedContinuation) => {
                OfflineReopenOutcome::RestoresHostedContinuation
            }
            _ => OfflineReopenOutcome::RestoresWorkingFrame,
        },
        CheckpointRecoveryVerdict::Refuse => OfflineReopenOutcome::RefusesRecoveryScope,
    };
    let evaluator_verified =
        plakous_agent::internal::facade_support::checkpoint_composition_artifact(checkpoint)
            .is_ok_and(|stored| match stored {
                None => true,
                #[cfg(feature = "composition")]
                Some(stored) => crate::local_composition::restore_stored_artifact(&stored).is_ok(),
                #[cfg(not(feature = "composition"))]
                Some(_) => false,
            });
    OfflineReopenInspection {
        outcome,
        recovery_phase,
        evaluator_verified,
        job_config_verified: checkpoint_job_config(checkpoint).is_ok(),
    }
}

/// The same bounded canonical-history tail the live projector uses after
/// feed retention, loaded without runtime construction or ownership.
///
/// # Errors
/// Returns typed lookup, stale-head or integrity failures.
#[doc(hidden)]
pub fn inspect_history_tail_sqlite(
    path: impl AsRef<Path>,
    locator: SessionLocator,
    max_items: usize,
    max_bytes: usize,
) -> Result<Vec<super::SessionTranscriptItem>, AgentSessionsError> {
    let loaded =
        SqliteSessionStore::inspect_branch(path, locator.session_ref()).map_err(store_error)?;
    let exact = ExactSource::validate(locator, &loaded)?;
    let source = super::SessionTranscriptSource::new(
        locator,
        exact.checkpoint.manifest().canonical().history().clone(),
        exact.checkpoint.manifest().ledger().clone(),
    );
    Ok(source.tail(max_items, max_bytes).items().to_vec())
}
