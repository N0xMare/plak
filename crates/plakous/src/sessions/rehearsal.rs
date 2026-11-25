//! Repository-only inspection of exact decoded heads without runtime ownership.

use super::{
    AgentSessionsError, ExactSource, SessionLocator, SessionRecoveryPhase, checkpoint_job_config,
    store_error,
};
use plakous_agent::internal::CheckpointRecoveryVerdict;
use {plakous_store::SqliteSessionStore, std::path::Path};

#[doc(hidden)]
pub use plakous_store::PreservationReader;

#[doc(hidden)]
pub use crate::EvaluatorArtifactFault;

/// A per-child qualification failure, separating environment from binding.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildBindingInspectionError {
    /// The binding could not be decoded or does not agree with this build.
    Binding(AgentSessionsError),
    /// This build does not accept the recorded evaluator engine or contract.
    EvaluatorContract(AgentSessionsError),
    /// A recorded evaluator artifact cannot be used on this machine.
    EvaluatorArtifact(EvaluatorArtifactFault),
}

impl ChildBindingInspectionError {
    pub(crate) fn evaluator_binding(message: &str) -> Self {
        Self::EvaluatorContract(AgentSessionsError::new(
            super::AgentSessionsErrorKind::IncompatibleBuild,
            message,
        ))
    }

    pub(crate) fn evaluator_record(message: &str) -> Self {
        Self::Binding(AgentSessionsError::new(
            super::AgentSessionsErrorKind::InvalidInput,
            message,
        ))
    }
}

/// Construct a verification-only facade over exact SQLite reads, without
/// opening its writer or bootstrapping supported legacy DDL.
///
/// # Errors
/// Returns a runtime construction failure. Exact DB checks occur on read.
#[doc(hidden)]
pub fn sqlite_reopen_inspection(path: &Path) -> Result<super::AgentSessions, AgentSessionsError> {
    super::AgentSessions::new(super::SessionBackend::from_store(
        std::sync::Arc::new(SqliteSessionStore::reopen_inspection(path.to_path_buf())),
        plakous_agent::internal::RuntimeDurability::Required,
    ))
}

/// Read-only preflight for the stopped-store runtime bootstrap.
///
/// # Errors
/// Returns typed shape and storage refusals.
pub fn runtime_bootstrap_pending(database: &Path) -> Result<bool, AgentSessionsError> {
    SqliteSessionStore::bootstrap_pending(database).map_err(store_error)
}

/// Exercise the actual runtime bootstrap on a detached copy.
///
/// # Errors
/// Returns typed shape, integrity and storage refusals.
pub fn rehearse_runtime_bootstrap(source: &Path, scratch: &Path) -> Result<(), AgentSessionsError> {
    SqliteSessionStore::rehearse_bootstrap(source, scratch).map_err(store_error)
}

/// Commit the pending bootstrap under exclusive SQLite locking.
///
/// # Errors
/// Returns typed shape, integrity and storage refusals.
pub fn bootstrap_stopped_runtime(
    database: &Path,
    cut: impl Fn(&str),
) -> Result<(), AgentSessionsError> {
    SqliteSessionStore::bootstrap_stopped(database, cut).map_err(store_error)
}

/// Reads every retained branch's frozen binding data, including children.
/// No actor, ownership, runtime, credential or runner is opened. Each graph is
/// decoded from immutable checkpoint objects through read-only SQLite.
///
/// # Errors
/// Returns schema, integrity or read failures, including a concurrently pruned head.
#[doc(hidden)]
pub struct BindingHeadInspection {
    /// Exact retained head.
    pub locator: SessionLocator,
    /// Runtime archival state.
    pub archived: bool,
    /// Whether this is the current agent-line head.
    pub line_head: bool,
    /// Decoded binding data or a per-head integrity/codec refusal.
    pub presentation: Option<Result<super::FrozenSessionPresentation, AgentSessionsError>>,
}

/// Inspects all retained binding heads in one SQLite read snapshot.
///
/// # Errors
/// Returns database admission, integrity or read failures.
#[doc(hidden)]
pub fn inspect_binding_presentations_sqlite(
    path: impl AsRef<Path>,
) -> Result<Vec<BindingHeadInspection>, AgentSessionsError> {
    SqliteSessionStore::inspect_binding_checkpoints(path, |head| {
        let locator = SessionLocator::new(
            head.session.session_id(),
            head.session.branch_id(),
            head.checkpoint,
        );
        BindingHeadInspection {
            locator,
            archived: head.archived,
            line_head: head.line_head,
            presentation: head.stored.map(|stored| {
                stored.map_err(store_error).and_then(|stored| {
                    super::FrozenSessionPresentation::from_checkpoint(stored.checkpoint())
                })
            }),
        }
    })
    .map_err(store_error)
}

/// Visits only the binding data of the named child branches, following their
/// current head without acquiring authority or materializing history.
///
/// # Errors
/// Returns database admission and storage failures. Per-head binding failures
/// go to the visitor; archived sessions are not visited.
#[doc(hidden)]
pub fn inspect_child_bindings_sqlite(
    path: &Path,
    locators: &[SessionLocator],
    mut visit: impl FnMut(
        Option<SessionLocator>,
        Result<super::FrozenSessionPresentation, ChildBindingInspectionError>,
    ),
) -> Result<(), AgentSessionsError> {
    if locators.is_empty() {
        return Ok(());
    }
    let mut evaluators = std::collections::HashMap::new();
    SqliteSessionStore::inspect_live_bindings(
        path,
        locators.iter().map(|locator| locator.session_ref()),
        |address, binding| {
            let presentation = (|| {
                let binding = binding
                    .map_err(store_error)
                    .map_err(ChildBindingInspectionError::Binding)?;
                let evaluator =
                    plakous_agent::internal::facade_support::tool_runtime_composition_artifact(
                        &binding.tools,
                    )
                    .map_err(|error| {
                        ChildBindingInspectionError::evaluator_record(error.message())
                    })?;
                if let Some(stored) = evaluator {
                    let check = evaluators
                        .entry(stored)
                        .or_insert_with_key(crate::verify_saved_evaluator);
                    check.clone()?;
                }
                super::FrozenSessionPresentation::from_binding(
                    binding.configuration,
                    &binding.tools,
                    &binding.harness,
                )
                .map_err(ChildBindingInspectionError::Binding)
            })();
            visit(address.map(SessionLocator::from_durable), presentation);
        },
    )
    .map_err(store_error)
}

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
            .is_ok_and(|stored| {
                stored.is_none_or(|stored| crate::verify_saved_evaluator(&stored).is_ok())
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
