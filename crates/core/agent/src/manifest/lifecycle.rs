//! Recovery obligations, the runtime lifecycle, and their validation against
//! the operation ledger.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the manifest module that shares its vocabulary"
)]

use super::*;

/// Closed unresolved-obligation class.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecoveryObligationKind {
    /// Command acknowledgement or safe-point handoff.
    Command,
    /// Provider physical attempt.
    ProviderAttempt,
    /// Pending pre-dispatch effect decision.
    EffectDecision,
    /// Tool effect.
    Effect,
    /// Closed provider-hosted round continuation frame.
    HostedContinuation,
    /// Driver-mediated composition cell whose possible start forbids replay.
    CodeCell,
    /// Direct-host process lifetime whose original owner cannot be restored.
    ProcessSession,
    /// Checkpoint commit.
    Commit,
    /// High-level durable compaction workflow.
    Compaction,
}

/// Closed fail-closed recovery action.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecoveryAction {
    /// Load/reconcile the exact commit operation identity.
    CommitRecovery,
    /// Obtain exact provider-owned attempt evidence.
    ProviderEvidenceRequired,
    /// Obtain exact executor/external effect evidence.
    EffectEvidenceRequired,
    /// Restore one exact still-pending pre-dispatch effect decision.
    RestoreEffectDecision,
    /// Restore one exact customer-tool batch before continuing its provider round.
    RestoreToolBatch,
    /// Restore one exact provider server pause before dispatching its continuation.
    RestoreServerPause,
    /// Abandon a possibly-started local composition without replaying source or children.
    EvaluationAbandonmentRequired,
    /// Terminalize only after exact non-dispatch evidence.
    ProvenNotDispatched,
    /// Fold already-proven evidence and commit a terminal checkpoint.
    FinalizationRequired,
}

/// The recovery handler one obligation needs: its kind and its sole action.
///
/// Instance identities are deliberately absent; the obligation binds the
/// signature to its persisted operation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecoveryHandlerSignature {
    pub(super) kind: RecoveryObligationKind,
    pub(super) action: RecoveryAction,
}

impl RecoveryHandlerSignature {
    /// Creates an exact recovery-handler signature.
    #[must_use]
    pub const fn new(kind: RecoveryObligationKind, action: RecoveryAction) -> Self {
        Self { kind, action }
    }

    /// Returns the unresolved obligation class.
    #[must_use]
    pub const fn kind(self) -> RecoveryObligationKind {
        self.kind
    }

    /// Returns the sole permitted recovery action.
    #[must_use]
    pub const fn action(self) -> RecoveryAction {
        self.action
    }
}

/// One recovery-only obligation bound to an exact ledger operation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecoveryObligation {
    pub(super) operation_id: u64,
    pub(super) signature: RecoveryHandlerSignature,
}

impl RecoveryObligation {
    /// Creates one exact obligation scope.
    #[must_use]
    pub const fn new(operation_id: u64, signature: RecoveryHandlerSignature) -> Self {
        Self {
            operation_id,
            signature,
        }
    }

    /// Returns the exact operation identity.
    #[must_use]
    pub const fn operation_id(self) -> u64 {
        self.operation_id
    }

    /// Returns the obligation class.
    #[must_use]
    pub const fn kind(self) -> RecoveryObligationKind {
        self.signature.kind
    }

    /// Returns the sole permitted recovery action.
    #[must_use]
    pub const fn action(self) -> RecoveryAction {
        self.signature.action
    }

    /// Returns the release-static handler semantics for this instance.
    #[must_use]
    pub const fn signature(self) -> RecoveryHandlerSignature {
        self.signature
    }
}

/// Closed working-recovery phase. A recovery-scope row must match it exactly.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkingRecoveryPhase {
    /// Reconcile commands/attempts which may have crossed dispatch.
    DispatchEvidence,
    /// Reconcile decision or tool-effect outcome evidence.
    EffectReconciliation,
    /// Restore only an exact pre-dispatch effect decision capability.
    EffectDecision,
    /// Restore a closed provider-hosted round continuation frame.
    HostedContinuation,
    /// Recover an ambiguous exact checkpoint commit.
    CommitRecovery,
    /// Fold already proven outcomes into a terminal checkpoint.
    Finalization,
}

/// Quiescent or recovery-only working lifecycle authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeLifecycle {
    /// A safe checkpoint may admit ordinary resume/fork/rebind after revalidation.
    Quiescent,
    /// Only exact reconciliation/terminalization of bound obligations is allowed.
    Working {
        /// Exact root plus only the progress legal for that root class.
        root: RuntimeWorkingRoot,
        /// Exact recovery phase; ordinary turn execution is never recreated.
        recovery_phase: WorkingRecoveryPhase,
        /// Complete unresolved recovery scopes.
        obligations: Vec<RecoveryObligation>,
    },
}

impl RuntimeLifecycle {
    /// Returns the checkpoint compatibility kind.
    #[must_use]
    pub const fn checkpoint_kind(&self) -> RuntimeCheckpointKind {
        match self {
            Self::Quiescent => RuntimeCheckpointKind::Quiescent,
            Self::Working { .. } => RuntimeCheckpointKind::Working,
        }
    }

    /// Returns exact working recovery obligations.
    #[must_use]
    pub fn obligations(&self) -> &[RecoveryObligation] {
        match self {
            Self::Quiescent => &[],
            Self::Working { obligations, .. } => obligations,
        }
    }

    /// Returns the exact stable working operation root, when Working.
    #[must_use]
    pub const fn operation_root(&self) -> Option<RuntimeOperationRoot> {
        match self {
            Self::Quiescent => None,
            Self::Working { root, .. } => Some(root.operation_root()),
        }
    }
}

pub(super) fn validate_operation_transitions(
    kind: &DurableOperationKind,
    transitions: &[DurableOperationTransition],
) -> Result<(), RuntimeManifestError> {
    let continuation_kind = matches!(
        kind,
        DurableOperationKind::ToolBatchContinuation { .. }
            | DurableOperationKind::ServerPauseContinuation { .. }
    );
    if continuation_kind
        && transitions.iter().any(|transition| {
            !matches!(
                transition.state,
                DurableOperationState::ContinuationRequired(_)
                    | DurableOperationState::Finalized(_)
            )
        })
    {
        return Err(RuntimeManifestError::InvalidLedger);
    }
    // One transition straight to Finalized is the shape of an operation that
    // was decided and published in the same commit: an atomically finalized
    // command, and the offline compaction rung, which replaces tool-result
    // bodies without any provider dispatch. Recording Prepared first would
    // name a checkpoint this branch never publishes, and publishing one to
    // carry it would leave a Compaction obligation demanding provider
    // evidence for an operation that never dispatched.
    let atomically_finalized = (transitions.len() == 1
        || matches!(kind, DurableOperationKind::Command { turn_id: Some(_) }))
        && matches!(
            kind,
            DurableOperationKind::Command { .. } | DurableOperationKind::Compaction { .. }
        )
        && matches!(transitions[0].state, DurableOperationState::Finalized(_));
    let continuation_starts_restorable = continuation_kind
        && matches!(
            transitions[0].state,
            DurableOperationState::ContinuationRequired(_)
        );
    if !atomically_finalized
        && !continuation_starts_restorable
        && !matches!(transitions[0].state, DurableOperationState::Prepared)
    {
        return Err(RuntimeManifestError::InvalidLedger);
    }
    let mut checkpoints = BTreeSet::new();
    let mut dispatched = false;
    let mut locator_before_dispatch = false;
    let mut locator_after_dispatch = false;
    for (index, transition) in transitions.iter().enumerate() {
        let atomic_recovery_finalization = index > 0
            && transitions[index - 1].checkpoint_id == transition.checkpoint_id
            && match (&transitions[index - 1].state, &transition.state) {
                (
                    DurableOperationState::Completed(terminal)
                    | DurableOperationState::ProvenNotDispatched(terminal),
                    DurableOperationState::Finalized(finalized),
                ) => matches!(kind, DurableOperationKind::Command { .. }) || terminal == finalized,
                // An abandoned model request records that its outcome was never
                // observed and that it is closed, in that order, in the one
                // checkpoint the reopen commits. The evidence is the same on
                // both, because there is only one thing to say about it.
                (
                    DurableOperationState::OutcomeUnknown(unknown),
                    DurableOperationState::Finalized(finalized),
                ) => unknown == finalized,
                // A control command's dispatch, evidence and finalization are
                // one commit (spec 01 §3.5 item 9).
                (DurableOperationState::Dispatched, DurableOperationState::Completed(_)) => {
                    matches!(kind, DurableOperationKind::Command { .. })
                }
                _ => false,
            };
        if !checkpoints.insert(transition.checkpoint_id) && !atomic_recovery_finalization {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        match &transition.state {
            DurableOperationState::DecisionRequired(pending) => {
                if !matches!(kind, DurableOperationKind::EffectDecision { .. })
                    || pending.dispatch_capability_minted()
                {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
            }
            DurableOperationState::ContinuationRequired(component) => {
                if !matches!(
                    kind,
                    DurableOperationKind::ToolBatchContinuation { .. }
                        | DurableOperationKind::ServerPauseContinuation { .. }
                ) {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
                expect_component(component, RuntimeComponentKind::Operation)?;
            }
            DurableOperationState::Claimed(component) => {
                if !matches!(kind, DurableOperationKind::Effect { .. }) {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
                expect_component(component, RuntimeComponentKind::Operation)?;
            }
            DurableOperationState::LocatorBound(component) => {
                let duplicate = if dispatched {
                    std::mem::replace(&mut locator_after_dispatch, true)
                } else {
                    std::mem::replace(&mut locator_before_dispatch, true)
                };
                if duplicate {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
                expect_component(component, RuntimeComponentKind::Operation)?;
            }
            DurableOperationState::Completed(component)
            | DurableOperationState::ProvenNotDispatched(component)
            | DurableOperationState::OutcomeUnknown(component)
            | DurableOperationState::Finalized(component) => {
                if !dispatched
                    && matches!(transition.state, DurableOperationState::Completed(_))
                    && index > 0
                    && matches!(
                        transitions[index - 1].state,
                        DurableOperationState::LocatorBound(_)
                    )
                {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
                expect_component(component, RuntimeComponentKind::Operation)?;
            }
            DurableOperationState::Dispatched => {
                if dispatched {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
                dispatched = true;
            }
            DurableOperationState::Prepared => {}
        }
    }
    for pair in transitions.windows(2) {
        let valid = matches!(
            (pair[0].state.class(), pair[1].state.class()),
            (
                OperationStateClass::Prepared,
                OperationStateClass::DecisionRequired
                    | OperationStateClass::ContinuationRequired
                    | OperationStateClass::Claimed
                    | OperationStateClass::Dispatched
                    | OperationStateClass::LocatorBound
                    | OperationStateClass::ProvenNotDispatched
                    | OperationStateClass::OutcomeUnknown
            ) | (
                OperationStateClass::DecisionRequired
                    | OperationStateClass::ContinuationRequired
                    | OperationStateClass::OutcomeUnknown,
                OperationStateClass::Completed | OperationStateClass::ProvenNotDispatched
            ) | (
                OperationStateClass::Claimed,
                OperationStateClass::Dispatched
                    | OperationStateClass::ProvenNotDispatched
                    | OperationStateClass::OutcomeUnknown
            ) | (
                OperationStateClass::Dispatched,
                OperationStateClass::LocatorBound
                    | OperationStateClass::Completed
                    | OperationStateClass::ProvenNotDispatched
                    | OperationStateClass::OutcomeUnknown
            ) | (
                OperationStateClass::LocatorBound,
                OperationStateClass::Dispatched
                    | OperationStateClass::Completed
                    | OperationStateClass::ProvenNotDispatched
                    | OperationStateClass::OutcomeUnknown
            ) | (
                // `OutcomeUnknown` is here because a newly opened owner — after
                // a lost process, or recycling a session a live owner fenced —
                // may abandon what a turn left unfinished. Nothing can
                // establish those outcomes afterwards, because recovery
                // classification is read-only and no route implements it, so
                // the alternative is a session no one can ever continue.
                // Finalizing from it records exactly that: the outcome was
                // never observed, the evidence says so, and nothing is
                // replayed.
                //
                // `abandoned_turn_recovery_fold` is the only thing that builds
                // it, for exactly the kinds it accepts: a provider attempt
                // (which may have been billed), a tool effect or a joined
                // process session (which may have changed the workspace, so
                // the evidence and the turn's note send the operator to look),
                // and a compaction with its provider attempt (not applied; it
                // may have been billed).
                //
                // `ProvenNotDispatched` is the opposite case and needs no such
                // licence: it is exact evidence that nothing happened, so
                // finalizing from it closes the operation without claiming
                // anything about a workspace.
                OperationStateClass::Completed
                    | OperationStateClass::ProvenNotDispatched
                    | OperationStateClass::OutcomeUnknown,
                OperationStateClass::Finalized
            )
        ) || (matches!(
            kind,
            DurableOperationKind::ToolBatchContinuation { .. }
                | DurableOperationKind::ServerPauseContinuation { .. }
        ) && matches!(
            (pair[0].state.class(), pair[1].state.class()),
            (
                OperationStateClass::ContinuationRequired,
                OperationStateClass::Finalized
            )
        )) || (matches!(kind, DurableOperationKind::Command { .. })
            && matches!(
                (pair[0].state.class(), pair[1].state.class()),
                (
                    // `Dispatched` is here for the turn's own command, closed
                    // as cancelled when its model request is abandoned. Its
                    // record is a terminal result like any other; what it says
                    // is that the turn stopped without an answer.
                    OperationStateClass::Prepared | OperationStateClass::Dispatched,
                    OperationStateClass::Finalized
                )
            ));
        let enrichment = match (kind, &pair[0].state, &pair[1].state) {
            (
                DurableOperationKind::Command {
                    turn_id: Some(turn_id),
                },
                DurableOperationState::Finalized(before),
                DurableOperationState::Finalized(after),
            ) => crate::runtime::valid_terminal_enrichment(*turn_id, before.bytes(), after.bytes()),
            _ => false,
        };
        if !valid && !enrichment {
            return Err(RuntimeManifestError::InvalidLedger);
        }
    }
    if matches!(kind, DurableOperationKind::ProcessSession { .. }) {
        let classes = transitions
            .iter()
            .map(|transition| transition.state.class())
            .collect::<Vec<_>>();
        // `OutcomeUnknown` is reached only by a newly opened owner abandoning
        // the process: teardown was never proven, so the outcome is recorded
        // as unobserved and closed together, in the one checkpoint that owner
        // commits. No shape stops at the unobserved state.
        let valid = matches!(
            classes.as_slice(),
            [OperationStateClass::Prepared]
                | [
                    OperationStateClass::Prepared,
                    OperationStateClass::Dispatched | OperationStateClass::ProvenNotDispatched
                ]
                | [
                    OperationStateClass::Prepared,
                    OperationStateClass::Dispatched,
                    OperationStateClass::Completed
                ]
                | [
                    OperationStateClass::Prepared,
                    OperationStateClass::Dispatched,
                    OperationStateClass::Completed | OperationStateClass::OutcomeUnknown,
                    OperationStateClass::Finalized
                ]
                | [
                    OperationStateClass::Prepared,
                    OperationStateClass::ProvenNotDispatched | OperationStateClass::OutcomeUnknown,
                    OperationStateClass::Finalized
                ]
        );
        if !valid {
            return Err(RuntimeManifestError::InvalidLedger);
        }
    }
    Ok(())
}

/// At the tool-group boundary, a Dispatched turn may host one compaction
/// `SessionWorkflow` (and its provider attempt) on the live head.
const fn compaction_workflow_may_join_turn(
    working: RuntimeOperationRoot,
    record_kind: &DurableOperationKind,
) -> bool {
    matches!(working, RuntimeOperationRoot::Turn(_))
        && matches!(
            record_kind,
            DurableOperationKind::Compaction { .. }
                | DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::SessionWorkflow(_),
                    ..
                }
        )
}

pub(super) fn validate_lifecycle(
    lifecycle: &RuntimeLifecycle,
    canonical: &RuntimeCanonicalState,
    ledger: &RuntimeOperationLedger,
) -> Result<(), RuntimeManifestError> {
    validate_lifecycle_shape(lifecycle)?;
    match lifecycle {
        RuntimeLifecycle::Quiescent => {
            if !canonical.working.segments().is_empty() || ledger.unresolved_operation_count() != 0
            {
                return Err(RuntimeManifestError::InvalidLifecycle);
            }
        }
        RuntimeLifecycle::Working {
            root,
            recovery_phase,
            obligations,
        } => {
            if ledger.unresolved_operation_count() != obligations.len() {
                return Err(RuntimeManifestError::InvalidLifecycle);
            }
            for obligation in obligations {
                let record =
                    operation_map_get(ledger.operation_root.as_deref(), obligation.operation_id)
                        .ok_or(RuntimeManifestError::InvalidLifecycle)?;
                let root_mismatch = record.kind.operation_root().is_some_and(|record_root| {
                    record_root != root.operation_root()
                        && !compaction_workflow_may_join_turn(root.operation_root(), &record.kind)
                });
                let queued_predispatch_command = matches!(
                    (&record.kind, record.state()),
                    (
                        DurableOperationKind::Command { .. },
                        DurableOperationState::Prepared
                    )
                );
                if record.kind.obligation_kind() != obligation.kind()
                    || (root_mismatch && !queued_predispatch_command)
                    || !recovery_phase_matches(*recovery_phase, *obligation)
                    || !recovery_action_matches(record, *obligation)
                {
                    return Err(RuntimeManifestError::InvalidLifecycle);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_lifecycle_shape(
    lifecycle: &RuntimeLifecycle,
) -> Result<(), RuntimeManifestError> {
    let RuntimeLifecycle::Working {
        root, obligations, ..
    } = lifecycle
    else {
        return Ok(());
    };
    if matches!(root, RuntimeWorkingRoot::Turn { next_round: 0, .. })
        || obligations.is_empty()
        || obligations.len() > MAX_RUNTIME_RECORDS
    {
        return Err(RuntimeManifestError::InvalidLifecycle);
    }
    let operation_ids = obligations
        .iter()
        .map(|obligation| obligation.operation_id)
        .collect::<BTreeSet<_>>();
    if operation_ids.len() != obligations.len() || operation_ids.contains(&0) {
        return Err(RuntimeManifestError::InvalidLifecycle);
    }
    Ok(())
}

fn recovery_phase_matches(phase: WorkingRecoveryPhase, obligation: RecoveryObligation) -> bool {
    let action = obligation.action();
    matches!(
        (phase, action),
        (
            WorkingRecoveryPhase::DispatchEvidence,
            RecoveryAction::ProviderEvidenceRequired
                | RecoveryAction::RestoreToolBatch
                | RecoveryAction::RestoreServerPause
                | RecoveryAction::ProvenNotDispatched
                | RecoveryAction::FinalizationRequired
        ) | (
            WorkingRecoveryPhase::EffectReconciliation,
            RecoveryAction::EffectEvidenceRequired
                | RecoveryAction::EvaluationAbandonmentRequired
                | RecoveryAction::RestoreToolBatch
                | RecoveryAction::RestoreServerPause
                | RecoveryAction::ProviderEvidenceRequired
                | RecoveryAction::ProvenNotDispatched
                | RecoveryAction::FinalizationRequired
        ) | (
            WorkingRecoveryPhase::CommitRecovery,
            RecoveryAction::CommitRecovery | RecoveryAction::FinalizationRequired
        ) | (
            WorkingRecoveryPhase::Finalization,
            RecoveryAction::FinalizationRequired
        ) | (
            WorkingRecoveryPhase::EffectDecision,
            RecoveryAction::RestoreEffectDecision
                | RecoveryAction::RestoreToolBatch
                | RecoveryAction::RestoreServerPause
                | RecoveryAction::ProvenNotDispatched
                | RecoveryAction::FinalizationRequired
        ) | (
            WorkingRecoveryPhase::HostedContinuation,
            RecoveryAction::RestoreToolBatch
                | RecoveryAction::RestoreServerPause
                | RecoveryAction::FinalizationRequired
        )
    )
}

fn recovery_action_matches(
    record: &DurableOperationRecord,
    obligation: RecoveryObligation,
) -> bool {
    match obligation.action() {
        RecoveryAction::RestoreEffectDecision => {
            matches!(record.kind, DurableOperationKind::EffectDecision { .. })
                && matches!(
                    record.state(),
                    DurableOperationState::DecisionRequired(pending)
                        if !pending.dispatch_capability_minted()
                            && pending.request() == record.request()
                )
        }
        RecoveryAction::RestoreToolBatch => {
            matches!(
                record.kind,
                DurableOperationKind::ToolBatchContinuation { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::ContinuationRequired(_)
            )
        }
        RecoveryAction::RestoreServerPause => {
            matches!(
                record.kind,
                DurableOperationKind::ServerPauseContinuation { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::ContinuationRequired(_)
            )
        }
        RecoveryAction::CommitRecovery => {
            matches!(record.kind, DurableOperationKind::Commit { .. })
                && !matches!(record.state(), DurableOperationState::Finalized(_))
        }
        RecoveryAction::ProviderEvidenceRequired => {
            matches!(
                record.kind,
                DurableOperationKind::Provider { .. } | DurableOperationKind::Compaction { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::Prepared
                    | DurableOperationState::Dispatched
                    | DurableOperationState::LocatorBound(_)
                    | DurableOperationState::OutcomeUnknown(_)
            )
        }
        RecoveryAction::EffectEvidenceRequired => {
            matches!(
                record.kind,
                DurableOperationKind::Effect { .. }
                    | DurableOperationKind::ProcessSession { .. }
                    | DurableOperationKind::CompositionEvaluation { .. }
                    | DurableOperationKind::CompositionContinuation { .. }
                    | DurableOperationKind::CompositionChild { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::Prepared
                    | DurableOperationState::Claimed(_)
                    | DurableOperationState::Dispatched
                    | DurableOperationState::LocatorBound(_)
                    | DurableOperationState::OutcomeUnknown(_)
            )
        }
        RecoveryAction::EvaluationAbandonmentRequired => {
            matches!(
                record.kind,
                DurableOperationKind::CompositionEvaluation { .. }
                    | DurableOperationKind::CompositionContinuation { .. }
                    | DurableOperationKind::CompositionChild { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::Dispatched
                    | DurableOperationState::LocatorBound(_)
                    | DurableOperationState::OutcomeUnknown(_)
            )
        }
        RecoveryAction::ProvenNotDispatched => {
            !matches!(record.kind, DurableOperationKind::ProcessSession { .. })
                && matches!(record.state(), DurableOperationState::Prepared)
        }
        RecoveryAction::FinalizationRequired => {
            matches!(
                record.state(),
                DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_)
            ) || matches!(
                (&record.kind, record.state()),
                (
                    DurableOperationKind::Command { .. },
                    DurableOperationState::Dispatched
                )
            )
        }
    }
}
