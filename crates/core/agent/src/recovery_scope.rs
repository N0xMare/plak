//! The working checkpoints this build can recover, and exact reopen rehydration.
//!
//! A quiescent checkpoint of the current format epoch always reopens. A
//! working checkpoint reopens when a newly opened owner abandons its unfinished
//! turn — the abandonment fold is the one authority for that — or when its
//! recovery phase and the set of (obligation kind, action) pairs it carries is
//! a row of [`RECOVERABLE`]. There are no ranges, wildcards or
//! checkpoint-supplied authority.

use std::collections::BTreeSet;

use plakous_model::{Digest, Timestamp};
use thiserror::Error;

use crate::{
    BindingReference, RecoveryAction, RecoveryObligationKind, ResolvedHarnessPlan,
    RevalidationFact, RuntimeCheckpoint, RuntimeCheckpointKind, RuntimeLifecycle, RuntimeManifest,
    RuntimeManifestError, WorkingRecoveryPhase,
};

use RecoveryAction::{EvaluationAbandonmentRequired, FinalizationRequired, ProvenNotDispatched};
use RecoveryObligationKind::{CodeCell, Command};
use WorkingRecoveryPhase::{EffectReconciliation, Finalization};

type Handler = (RecoveryObligationKind, RecoveryAction);

/// Every working shape this build restores, finalizes or reconciles, as
/// (phase, exact handler set).
///
/// Closures are not here: which unfinished turns and admitted session-workflow
/// compactions a reopen closes is the recovery verdict's decision alone, so no
/// row claims `EffectEvidenceRequired`.
/// Any other reading of that obligation would mean acting on a tool whose
/// outcome this process cannot see.
static RECOVERABLE: &[(WorkingRecoveryPhase, &[Handler])] = &[
    (
        EffectReconciliation,
        &[
            (Command, FinalizationRequired),
            (CodeCell, EvaluationAbandonmentRequired),
        ],
    ),
    (
        EffectReconciliation,
        &[
            (Command, FinalizationRequired),
            (CodeCell, ProvenNotDispatched),
        ],
    ),
    (
        EffectReconciliation,
        &[
            (Command, FinalizationRequired),
            (CodeCell, EvaluationAbandonmentRequired),
            (CodeCell, ProvenNotDispatched),
        ],
    ),
    (
        Finalization,
        &[
            (Command, FinalizationRequired),
            (CodeCell, FinalizationRequired),
        ],
    ),
];

/// A working checkpoint whose phase and obligations this build cannot recover.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("runtime working recovery scope is not recoverable by this build")]
pub struct UnsupportedRecoveryScope;

/// Accepts every head the one recovery predicate does not refuse (spec 01
/// §5.5). Quiescent checkpoints always pass.
pub(crate) fn require_recoverable(
    checkpoint: &RuntimeCheckpoint,
) -> Result<(), UnsupportedRecoveryScope> {
    if crate::runtime::recovery_verdict_refuses(checkpoint) {
        Err(UnsupportedRecoveryScope)
    } else {
        Ok(())
    }
}

/// Whether a working head's exact phase and obligations are one of the
/// release-owned scopes an explicit workflow or a restore owns.
pub(crate) fn in_release_scope(
    checkpoint: &RuntimeCheckpoint,
    phase: WorkingRecoveryPhase,
) -> bool {
    let RuntimeLifecycle::Working { obligations, .. } = checkpoint.manifest().lifecycle() else {
        return false;
    };
    let handlers = obligations
        .iter()
        .map(|obligation| {
            let signature = obligation.signature();
            (signature.kind(), signature.action())
        })
        .collect::<BTreeSet<_>>();
    recoverable_working_scope(phase, &handlers)
}

fn recoverable_working_scope(phase: WorkingRecoveryPhase, handlers: &BTreeSet<Handler>) -> bool {
    RECOVERABLE.iter().any(|(row_phase, row)| {
        *row_phase == phase && row.iter().copied().collect::<BTreeSet<_>>() == *handlers
    })
}

/// Fresh externally owned facts a reopen must match.
#[derive(Clone, Debug)]
pub struct RuntimeRevalidationWitness {
    checked_at: Timestamp,
    account: BindingReference,
    profile: BindingReference,
    model_binding: Digest,
    descriptor: Digest,
    admission: RevalidationFact,
    tool_registry: Digest,
    tool_exposure: Digest,
    environment: Digest,
    effect_policy: Digest,
    policy: BindingReference,
}

impl RuntimeRevalidationWitness {
    /// Creates a complete fresh binding witness.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the closed revalidation tuple"
    )]
    #[must_use]
    pub const fn new(
        checked_at: Timestamp,
        account: BindingReference,
        profile: BindingReference,
        model_binding: Digest,
        descriptor: Digest,
        admission: RevalidationFact,
        tool_registry: Digest,
        tool_exposure: Digest,
        environment: Digest,
        effect_policy: Digest,
        policy: BindingReference,
    ) -> Self {
        Self {
            checked_at,
            account,
            profile,
            model_binding,
            descriptor,
            admission,
            tool_registry,
            tool_exposure,
            environment,
            effect_policy,
            policy,
        }
    }

    fn verify(&self, manifest: &RuntimeManifest) -> Result<(), RevalidationError> {
        let binding = manifest.binding();
        let tools = manifest.tools();
        if self.account != *binding.account()
            || self.profile != *binding.profile()
            || self.model_binding != binding.model_binding()
            || self.descriptor != binding.model_binding()
            || self.admission.reference() != binding.admission().reference()
            || self.admission.observed_at() < binding.admission().observed_at()
            || self.admission.observed_at() > self.checked_at
            || self
                .admission
                .valid_until()
                .is_some_and(|until| until <= self.checked_at)
        {
            return Err(RevalidationError::BindingMismatch);
        }
        if self.tool_registry != tools.registry().integrity()
            || self.tool_exposure != tools.exposure().integrity()
            || self.environment != tools.environment().integrity()
            || self.effect_policy != tools.effect_policy().integrity()
            || self.policy != *tools.policy()
        {
            return Err(RevalidationError::SemanticsDrift);
        }
        Ok(())
    }
}

/// Result of exact reconstruction.
#[derive(Clone, Debug)]
pub enum RuntimeRehydration {
    /// Safe ordinary resume under the recorded semantics.
    Quiescent {
        /// Reconstructed immutable checkpoint.
        checkpoint: RuntimeCheckpoint,
        /// Exactly reconstructed harness plan.
        harness_plan: ResolvedHarnessPlan,
    },
    /// Recovery-only opening; no ordinary turn work is authorized.
    WorkingRecovery {
        /// Reconstructed immutable checkpoint.
        checkpoint: RuntimeCheckpoint,
        /// Exactly reconstructed harness plan.
        harness_plan: ResolvedHarnessPlan,
    },
}

impl RuntimeCheckpoint {
    /// Requires a recoverable scope, revalidates all external bindings, and
    /// reconstructs the exact plan. Working checkpoints remain recovery-only.
    ///
    /// # Errors
    ///
    /// Returns a typed recovery-scope, revalidation, or reconstruction failure.
    pub fn rehydrate(
        &self,
        witness: &RuntimeRevalidationWitness,
    ) -> Result<RuntimeRehydration, RuntimeRehydrationError> {
        require_recoverable(self)?;
        witness.verify(self.manifest())?;
        let harness_plan = self.manifest().rehydrate_harness_plan()?;
        if self
            .manifest()
            .canonical()
            .stable_prefix()
            .resolved_harness_plan_identity()
            != Some(*harness_plan.plan_digest())
        {
            return Err(RuntimeManifestError::RevisionDisagreement.into());
        }
        Ok(match self.kind() {
            RuntimeCheckpointKind::Quiescent => RuntimeRehydration::Quiescent {
                checkpoint: self.clone(),
                harness_plan,
            },
            RuntimeCheckpointKind::Working => RuntimeRehydration::WorkingRecovery {
                checkpoint: self.clone(),
                harness_plan,
            },
        })
    }
}

/// Current external authority/binding mismatch.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RevalidationError {
    /// The account, profile, model binding or admission drifted or expired.
    #[error("runtime external binding revalidation failed")]
    BindingMismatch,
    /// This build rebuilds different tool, environment or policy semantics.
    #[error("runtime tool, environment or policy semantics differ from the stored session")]
    SemanticsDrift,
}

/// Combined exact reconstruction failure.
#[derive(Debug, Error)]
pub enum RuntimeRehydrationError {
    /// A working checkpoint's recovery shape is not in this build's table.
    #[error(transparent)]
    UnsupportedRecoveryScope(#[from] UnsupportedRecoveryScope),
    /// External authority or binding did not revalidate.
    #[error(transparent)]
    Revalidation(#[from] RevalidationError),
    /// Exact canonical reconstruction failed.
    #[error(transparent)]
    Manifest(#[from] RuntimeManifestError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecoveryObligationKind::{Commit, Effect, ProcessSession};

    #[test]
    fn rows_are_distinct() {
        for (index, (phase, handlers)) in RECOVERABLE.iter().enumerate() {
            let set = handlers.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(set.len(), handlers.len(), "row {index} repeats a handler");
            assert!(
                !RECOVERABLE[..index].iter().any(|(prior_phase, prior)| {
                    prior_phase == phase && prior.iter().copied().collect::<BTreeSet<_>>() == set
                }),
                "row {index} duplicates an earlier row"
            );
        }
    }

    /// This build claims no handler it does not own. No row acts on an
    /// unseen tool outcome: abandonment belongs to the fold, not the table.
    #[test]
    fn no_row_claims_an_unseen_tool_outcome() {
        for (_, handlers) in RECOVERABLE {
            for (kind, action) in *handlers {
                assert_ne!(*kind, Commit);
                assert_ne!(*kind, ProcessSession);
                assert_ne!(*action, RecoveryAction::EffectEvidenceRequired);
            }
        }
    }

    #[test]
    fn an_unlisted_shape_is_not_recoverable() {
        let effect_only = [(Effect, RecoveryAction::EffectEvidenceRequired)]
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert!(!recoverable_working_scope(
            EffectReconciliation,
            &effect_only
        ));
        let listed = [(Command, FinalizationRequired)]
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert!(!recoverable_working_scope(Finalization, &listed));
        assert!(!recoverable_working_scope(
            WorkingRecoveryPhase::DispatchEvidence,
            &listed
        ));
    }

    /// An admitted session-workflow compaction is closed by the verdict's
    /// workflow rows at open, never restored through this table.
    #[test]
    fn no_row_restores_a_workflow_compaction() {
        for (_, handlers) in RECOVERABLE {
            assert!(
                !handlers
                    .iter()
                    .any(|(kind, _)| *kind == RecoveryObligationKind::Compaction)
            );
        }
    }
}
