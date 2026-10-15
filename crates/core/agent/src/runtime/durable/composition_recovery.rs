//! Exact code-cell abandonment evidence and recovery publication.

use crate::recovery_scope::require_recoverable;
use crate::runtime::OperationId;
use crate::runtime::checkpoint::identity::{CheckpointCommitFacts, checkpoint_commit_identity};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::composition::record::{
    DurableLocalCompositionContinuationRequestV1, DurableLocalCompositionEvaluationRequestV2,
    decode_local_composition_evaluation_request, encode_local_composition_continuation_request,
};
use crate::runtime::continuation::{runtime_component, terminal_record_at_checkpoint};
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::{operation_run_id, validate_session_operation_id};
use crate::runtime::durable::turn_recovery::{recover_root_turn_publication, verdict_closure};
use crate::runtime::durable::{
    CodeCellAbandonmentFold, CompactionRecoveryOwner, DurableCodeCellAbandonmentDisposition,
    DurableCodeCellAbandonmentEvidence, DurableCodeCellAbandonmentIdentity,
    DurableCodeCellAbandonmentOutcome, DurableCodeCellAbandonmentRequest, DurableSessionAddress,
    DurableSessionRuntime,
};
use crate::runtime::observation::canonical_tool_result_item;
use crate::runtime::operation_record::{is_accepted_root, operation_turn_id, working_lifecycle};
use crate::runtime::terminal::{DurableTerminalOutcome, decode_pending_terminal_record};
use crate::runtime::{DurableActorCommandEffect, DurableCommandEnvelope};
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, CommandId, CommittedCheckpoint,
    DurableCommandDisposition, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent, RecoveryAction, RecoveryObligationKind,
    RuntimeCheckpoint, RuntimeComponentKind, RuntimeLifecycle, RuntimeWorkingRoot, SessionStore,
    SessionStoreError, WorkingRecoveryPhase,
};
use plakous_model::{
    Actor, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId, HistorySegment,
    HistorySegmentKind, Provenance, TextBlock, ToolCallState, ToolExecutionOwner, ToolResultBlock,
    ToolResultKind, ToolResultPart,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
impl DurableSessionRuntime {
    /// Explicitly abandons one exact recovery-only Code Cell and finalizes its
    /// already committed RecoveryRequired root without source or child replay.
    ///
    /// Exact repetition returns the first committed target. An ambiguous store
    /// acknowledgement is reconciled through the deterministic checkpoint identity.
    #[doc(hidden)]
    pub async fn abandon_code_cell(
        &self,
        request: DurableCodeCellAbandonmentRequest,
    ) -> Result<DurableCodeCellAbandonmentOutcome, AgentError> {
        let identity = DurableCodeCellAbandonmentIdentity::new(&request)?;
        if let Some(outcome) = self.code_cell_abandonment_receipt(identity).await? {
            return Ok(outcome);
        }
        let owner_id = operation_run_id(&self.instance_nonce, request.operation_id())?;
        let lease = store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                identity.source.session(),
                owner_id,
                self.lease_seconds,
            )),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let recovery = async {
            if let Some(outcome) = self.code_cell_abandonment_receipt(identity).await? {
                return Ok(outcome);
            }
            let loaded = store_call(
                StorePhase::LoadBranch,
                self.store_timeouts.operation,
                self.store.load_branch(identity.source.session()),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let mut owner = CompactionRecoveryOwner {
                store: Arc::clone(&self.store),
                store_timeout: self.store_timeouts.operation,
                lease: lease.clone(),
                head: loaded.head(),
                checkpoint: Arc::clone(loaded.checkpoint().checkpoint()),
                bundle: loaded.checkpoint().bundle().clone(),
            };
            if owner.checkpoint.id() == identity.source.checkpoint_id {
                require_recoverable(&owner.checkpoint).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Session,
                        "Code Cell abandonment scope is not recoverable by this build",
                    )
                })?;
                publish_code_cell_abandonment_evidence(&mut owner, identity).await?;
            }
            if owner.checkpoint.id() != identity.evidence_checkpoint {
                return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
            }
            validate_code_cell_evidence_checkpoint(&owner.checkpoint, identity)?;
            require_recoverable(&owner.checkpoint).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "Code Cell finalization scope is not recoverable by this build",
                )
            })?;
            let (checkpoint, _, _) = recover_root_turn_publication(
                &self.store,
                &lease,
                owner.head,
                &owner.checkpoint,
                &owner.bundle,
                verdict_closure(&owner.checkpoint)?
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
                owner.checkpoint.manifest().lifecycle().obligations(),
                self.store_timeouts.operation,
                identity.final_checkpoint,
                None,
            )
            .await?;
            Ok(DurableCodeCellAbandonmentOutcome::Finalized {
                source: identity.source,
                target: DurableSessionAddress::new(
                    identity.source.session_id,
                    identity.source.branch_id,
                    checkpoint.id(),
                ),
            })
        }
        .await;
        let cleanup =
            release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup).await;
        match (recovery, cleanup) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    pub(super) async fn code_cell_abandonment_receipt(
        &self,
        identity: DurableCodeCellAbandonmentIdentity,
    ) -> Result<Option<DurableCodeCellAbandonmentOutcome>, AgentError> {
        match store_call(
            StorePhase::RecoverCommit,
            self.store_timeouts.operation,
            self.store.recover_commit(identity.final_checkpoint),
        )
        .await
        {
            Ok(recovered) => {
                validate_code_cell_final_receipt(
                    &self.store,
                    &recovered,
                    identity,
                    self.store_timeouts.operation,
                )
                .await?;
                return Ok(Some(DurableCodeCellAbandonmentOutcome::Finalized {
                    source: identity.source,
                    target: DurableSessionAddress::new(
                        identity.source.session_id,
                        identity.source.branch_id,
                        identity.final_checkpoint,
                    ),
                }));
            }
            Err(error) if error.is(SessionStoreError::NotFound) => {}
            Err(error) => return Err(error.into_agent()),
        }
        let loaded = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(identity.source.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let checkpoint = loaded.checkpoint().checkpoint();
        if checkpoint.id() == identity.source.checkpoint_id {
            if matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Quiescent
            ) {
                return Ok(Some(DurableCodeCellAbandonmentOutcome::NotRequired(
                    identity.source,
                )));
            }
            code_cell_abandonment_fold(checkpoint)?;
            return Ok(None);
        }
        if checkpoint.id() == identity.evidence_checkpoint {
            validate_code_cell_evidence_checkpoint(checkpoint, identity)?;
            return Ok(None);
        }
        Err(AgentError::from_store(SessionStoreError::IdentityConflict))
    }
}

impl DurableCodeCellAbandonmentRequest {
    /// Binds one retry-stable operation identity to the exact `RecoveryRequired` root.
    ///
    /// # Errors
    ///
    /// Returns invalid input when the operation identity is not bounded and canonical.
    pub fn new(
        operation_id: impl Into<String>,
        source: DurableSessionAddress,
    ) -> Result<Self, AgentError> {
        let operation_id = operation_id.into();
        validate_session_operation_id(&operation_id)?;
        Ok(Self {
            operation_id,
            source,
        })
    }

    /// Returns the retry-stable caller operation identity.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Returns the exact unchanged `RecoveryRequired` source.
    #[must_use]
    pub const fn source(&self) -> DurableSessionAddress {
        self.source
    }
}

fn code_cell_abandonment_entry_id(workflow: Digest, role: &[u8]) -> Result<EntryId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/code-cell-abandonment-entry-v1",
            workflow.bytes(),
            role,
        ],
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Identity, "Code Cell entry identity failed"))?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EntryId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

fn code_cell_abandonment_segment(
    identity: DurableCodeCellAbandonmentIdentity,
    request: &DurableLocalCompositionEvaluationRequestV2,
) -> Result<HistorySegment, AgentError> {
    let call_item = ConversationItem::new(
        code_cell_abandonment_entry_id(identity.workflow, b"outer-call")?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(request.call.clone())],
        Provenance::ModelOutput,
    )
    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let text = TextBlock::new(
        "this Code Cell evaluation was abandoned after its outcome became uncertain. It may \
         have run tools that changed the workspace; its result was never observed, and it \
         is not retried. Check the workspace before relying on what came before this point."
            .to_owned(),
        None,
    )
    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let result = ToolResultBlock::new(
        request.call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Failure,
        vec![ToolResultPart::Text(text)],
        request.call.correlation().clone(),
    )
    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    let result_item = canonical_tool_result_item(request.result_entry_id, result)?;
    HistorySegment::new(
        HistorySegmentKind::ToolExchange,
        vec![call_item, result_item],
    )
    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))
}

pub(in crate::runtime) fn code_cell_abandonment_fold(
    checkpoint: &RuntimeCheckpoint,
) -> Result<CodeCellAbandonmentFold, AgentError> {
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        recovery_phase,
        obligations,
    } = manifest.lifecycle()
    else {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "Code Cell abandonment requires one recovery-only root turn",
        ));
    };
    if !matches!(
        recovery_phase,
        WorkingRecoveryPhase::EffectReconciliation | WorkingRecoveryPhase::Finalization
    ) {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "Code Cell root is outside abandonment or finalization scope",
        ));
    }
    let ledger = manifest.ledger();
    let root_command = ledger
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(*turn_id) && is_accepted_root(command))
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let root_finalized = ledger.operations().into_iter().any(|record| {
        matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if bound == turn_id)
            && record.request().bytes() == root_command.command_component().bytes()
            && matches!(record.state(), DurableOperationState::Finalized(_))
    });
    if root_finalized {
        let terminal = terminal_record_at_checkpoint(
            checkpoint,
            &CommandId::new(root_command.command_id())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        )?;
        if !matches!(
            terminal.outcome,
            DurableTerminalOutcome::RecoveryRequired(_)
        ) {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "a definite root result cannot justify Code Cell abandonment",
            ));
        }
    }
    let mut operations = Vec::new();
    let mut root_terminal_count = 0_usize;
    let mut evaluation = None;
    let mut children = Vec::new();
    let mut continuations = Vec::new();
    let mut turn_abandonment = root_finalized;
    let mut root_recovery_required = root_finalized;
    for obligation in obligations {
        let record = ledger
            .operation(obligation.operation_id())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if operation_turn_id(record) != Some(*turn_id) {
            if queued_root(checkpoint, record, *turn_id)
                && obligation.kind() == RecoveryObligationKind::Command
                && obligation.action() == RecoveryAction::ProvenNotDispatched
            {
                turn_abandonment = true;
                continue;
            }
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        match (
            record.kind(),
            record.state(),
            obligation.kind(),
            obligation.action(),
        ) {
            (
                DurableOperationKind::Command {
                    turn_id: Some(root_turn),
                },
                DurableOperationState::Dispatched,
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
            ) if root_turn == turn_id
                && record.request().bytes() == root_command.command_component().bytes() =>
            {
                root_terminal_count = root_terminal_count.saturating_add(1);
                turn_abandonment = true;
            }
            (
                DurableOperationKind::Provider { .. },
                DurableOperationState::Completed(_),
                RecoveryObligationKind::ProviderAttempt,
                RecoveryAction::FinalizationRequired,
            )
            | (
                DurableOperationKind::Effect { .. },
                DurableOperationState::Completed(_),
                RecoveryObligationKind::Effect,
                RecoveryAction::FinalizationRequired,
            )
            // A tool or process the cell dispatched before the host was lost
            // has no evidence a reopen could read, exactly like a direct tool.
            // It follows the ordinary effect law, never Code Cell evidence:
            // only the turn-level abandonment closes it, as outcome-unknown
            // (or finalizes its proof that it never ran). It is never
            // replayed and never becomes a cell result.
            | (
                DurableOperationKind::Effect { .. },
                DurableOperationState::Prepared
                | DurableOperationState::Claimed(_)
                | DurableOperationState::Dispatched
                | DurableOperationState::OutcomeUnknown(_)
                | DurableOperationState::ProvenNotDispatched(_),
                RecoveryObligationKind::Effect,
                RecoveryAction::EffectEvidenceRequired,
            )
            | (
                DurableOperationKind::ProcessSession { .. },
                DurableOperationState::Prepared | DurableOperationState::Dispatched,
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::EffectEvidenceRequired,
            )
            | (
                DurableOperationKind::ProcessSession { .. },
                DurableOperationState::Completed(_),
                RecoveryObligationKind::ProcessSession,
                RecoveryAction::FinalizationRequired,
            ) => {
                turn_abandonment = true;
            }
            (
                DurableOperationKind::Command {
                    turn_id: Some(root_turn),
                },
                DurableOperationState::Completed(evidence),
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
            ) if root_turn == turn_id
                && record.request().bytes() == root_command.command_component().bytes() =>
            {
                let pending = decode_pending_terminal_record(evidence.bytes())
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                if pending.command_id != root_command.command_id()
                    || pending.turn_id != *turn_id
                    || pending.operation_id.0 != record.operation_id()
                    || !matches!(pending.outcome, DurableTerminalOutcome::RecoveryRequired(_))
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
                root_terminal_count = root_terminal_count.saturating_add(1);
                root_recovery_required = true;
            }
            (
                DurableOperationKind::Command {
                    turn_id: Some(root_turn),
                },
                DurableOperationState::Prepared,
                RecoveryObligationKind::Command,
                RecoveryAction::ProvenNotDispatched,
            ) if root_turn == turn_id
                && record.request().bytes() == root_command.command_component().bytes() =>
            {
                root_terminal_count = root_terminal_count.saturating_add(1);
                turn_abandonment = true;
            }
            (
                DurableOperationKind::Command { .. },
                DurableOperationState::Dispatched | DurableOperationState::Completed(_),
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
            ) if pending_steering(checkpoint, record, *turn_id) => {
                turn_abandonment = true;
            }
            (DurableOperationKind::Command { .. }, _, _, _) => {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            (
                DurableOperationKind::CompositionEvaluation { .. }
                | DurableOperationKind::CompositionContinuation { .. }
                | DurableOperationKind::CompositionChild { .. },
                DurableOperationState::Prepared,
                RecoveryObligationKind::CodeCell,
                RecoveryAction::ProvenNotDispatched,
            ) => {
                let disposition = DurableCodeCellAbandonmentDisposition::ProvenNotDispatched;
                operations.push((OperationId(record.operation_id()), disposition));
                match record.kind() {
                    DurableOperationKind::CompositionEvaluation {
                        attempt_id,
                        call_id,
                        ..
                    } => {
                        let request = decode_local_composition_evaluation_request(record.request())
                            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                        if request.call.call_id() != *call_id || evaluation.is_some() {
                            return Err(AgentError::from_store(SessionStoreError::corrupt()));
                        }
                        evaluation =
                            Some((OperationId(record.operation_id()), *attempt_id, request));
                    }
                    DurableOperationKind::CompositionChild {
                        attempt_id,
                        evaluation_operation_id,
                        request_sequence,
                        ..
                    } => children.push((*attempt_id, *evaluation_operation_id, *request_sequence)),
                    DurableOperationKind::CompositionContinuation {
                        attempt_id,
                        evaluation_operation_id,
                        ..
                    } => continuations.push((*attempt_id, *evaluation_operation_id)),
                    _ => unreachable!(),
                }
            }
            (
                DurableOperationKind::CompositionEvaluation { .. }
                | DurableOperationKind::CompositionContinuation { .. }
                | DurableOperationKind::CompositionChild { .. },
                DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_),
                RecoveryObligationKind::CodeCell,
                RecoveryAction::EvaluationAbandonmentRequired,
            ) => {
                let disposition = DurableCodeCellAbandonmentDisposition::PossiblyStarted;
                operations.push((OperationId(record.operation_id()), disposition));
                match record.kind() {
                    DurableOperationKind::CompositionEvaluation {
                        attempt_id,
                        call_id,
                        ..
                    } => {
                        let request = decode_local_composition_evaluation_request(record.request())
                            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                        if request.call.call_id() != *call_id || evaluation.is_some() {
                            return Err(AgentError::from_store(SessionStoreError::corrupt()));
                        }
                        evaluation =
                            Some((OperationId(record.operation_id()), *attempt_id, request));
                    }
                    DurableOperationKind::CompositionChild {
                        attempt_id,
                        evaluation_operation_id,
                        request_sequence,
                        ..
                    } => children.push((*attempt_id, *evaluation_operation_id, *request_sequence)),
                    DurableOperationKind::CompositionContinuation {
                        attempt_id,
                        evaluation_operation_id,
                        ..
                    } => continuations.push((*attempt_id, *evaluation_operation_id)),
                    _ => unreachable!(),
                }
            }
            (
                DurableOperationKind::CompositionEvaluation { .. }
                | DurableOperationKind::CompositionContinuation { .. }
                | DurableOperationKind::CompositionChild { .. },
                DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_),
                RecoveryObligationKind::CodeCell,
                RecoveryAction::FinalizationRequired,
            ) => match record.kind() {
                DurableOperationKind::CompositionEvaluation {
                    attempt_id,
                    call_id,
                    ..
                } => {
                    let request = decode_local_composition_evaluation_request(record.request())
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                    if request.call.call_id() != *call_id || evaluation.is_some() {
                        return Err(AgentError::from_store(SessionStoreError::corrupt()));
                    }
                    evaluation = Some((OperationId(record.operation_id()), *attempt_id, request));
                }
                DurableOperationKind::CompositionChild {
                    attempt_id,
                    evaluation_operation_id,
                    request_sequence,
                    ..
                } => children.push((*attempt_id, *evaluation_operation_id, *request_sequence)),
                DurableOperationKind::CompositionContinuation {
                    attempt_id,
                    evaluation_operation_id,
                    ..
                } => continuations.push((*attempt_id, *evaluation_operation_id)),
                _ => unreachable!(),
            },
            _ => {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "Code Cell root retains recovery debt outside explicit abandonment scope",
                ));
            }
        }
    }
    // A RecoveryRequired root (pending or finalized) can outlive the original
    // evaluation: finalized records are absent from obligations, but a
    // continuation or a settled child still names its parent (root-boundary
    // cleanup finalizes a terminated cell before its children's debt).
    // Resolve only that exact parent, never another evaluation in the ledger.
    if evaluation.is_none() && root_recovery_required {
        let parent = continuations
            .first()
            .map(|(_, parent)| parent)
            .or_else(|| children.first().map(|(_, parent, _)| parent))
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let record = ledger
            .operation(*parent)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let DurableOperationKind::CompositionEvaluation {
            turn_id: evaluation_turn,
            attempt_id,
            call_id,
            ..
        } = record.kind()
        else {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        };
        let request = decode_local_composition_evaluation_request(record.request())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if evaluation_turn != turn_id
            || !matches!(record.state(), DurableOperationState::Finalized(_))
            || request.call.call_id() != *call_id
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        for continuation in ledger.operations().into_iter().filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionContinuation { .. }
            ) && obligations
                .iter()
                .any(|obligation| obligation.operation_id() == record.operation_id())
        }) {
            let DurableOperationKind::CompositionContinuation { call_id, .. } = continuation.kind()
            else {
                unreachable!();
            };
            let (tag, continued): (String, DurableLocalCompositionContinuationRequestV1) =
                serde_json::from_slice(continuation.request().bytes())
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            if tag != "plakous-local-composition-continuation-v1"
                || continued.version != 1
                || encode_local_composition_continuation_request(&continued)?.as_slice()
                    != continuation.request().bytes()
                || continued.evaluation_operation_id != *parent
                || continued.call.call_id() != *call_id
                || continued.call.owner() != &ToolExecutionOwner::Local
                || continued.call.state() != ToolCallState::Requested
                || continued.descriptor_identity != request.descriptor_identity
                || continued.host_identity != request.host_identity
            {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
        }
        evaluation = Some((OperationId(record.operation_id()), *attempt_id, request));
    }
    let Some((evaluation_operation, evaluation_attempt, evaluation_request)) = evaluation else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    if evaluation_request.call.owner() != &ToolExecutionOwner::Local
        || evaluation_request.call.state() != ToolCallState::Requested
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let mut allowed_attempts = BTreeSet::from([evaluation_attempt]);
    if continuations
        .iter()
        .any(|(_, parent)| *parent != evaluation_operation.0)
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    allowed_attempts.extend(continuations.iter().map(|(attempt_id, _)| *attempt_id));
    // Root-boundary cleanup can finalize a later round's continuation before
    // the children it dispatched; their attempt is still this cell's.
    allowed_attempts.extend(ledger.operations().into_iter().filter_map(|record| {
        match (record.kind(), record.state()) {
            (
                DurableOperationKind::CompositionContinuation {
                    attempt_id,
                    evaluation_operation_id,
                    ..
                },
                DurableOperationState::Finalized(_),
            ) if *evaluation_operation_id == evaluation_operation.0 => Some(*attempt_id),
            _ => None,
        }
    }));
    let mut sequences = BTreeSet::new();
    if children.iter().any(|(attempt_id, parent, sequence)| {
        !allowed_attempts.contains(attempt_id)
            || *parent != evaluation_operation.0
            || *sequence == 0
            || !sequences.insert(*sequence)
    }) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    if root_terminal_count + usize::from(root_finalized) != 1
        || (operations.is_empty() && *recovery_phase != WorkingRecoveryPhase::Finalization)
    {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "Code Cell root lacks exact abandonment and terminal evidence",
        ));
    }
    Ok(CodeCellAbandonmentFold {
        turn_id: *turn_id,
        root_finalized,
        turn_abandonment,
        evaluation_request,
        operations,
    })
}

pub(in crate::runtime) fn pending_steering(
    checkpoint: &RuntimeCheckpoint,
    record: &DurableOperationRecord,
    turn_id: plakous_model::TurnId,
) -> bool {
    operation_turn_id(record) == Some(turn_id)
        && checkpoint
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .any(|command| {
                command.turn_id() == Some(turn_id)
                    && command.disposition() == DurableCommandDisposition::Accepted
                    && command.command_component().bytes() == record.request().bytes()
                    && serde_json::from_slice::<DurableCommandEnvelope>(
                        command.command_component().bytes(),
                    )
                    .is_ok_and(|envelope| {
                        matches!(
                            envelope.effect,
                            DurableActorCommandEffect::SteerQueued { .. }
                        )
                    })
            })
}

pub(in crate::runtime) fn queued_root(
    checkpoint: &RuntimeCheckpoint,
    record: &DurableOperationRecord,
    active_turn: plakous_model::TurnId,
) -> bool {
    let DurableOperationKind::Command {
        turn_id: Some(turn_id),
    } = record.kind()
    else {
        return false;
    };
    *turn_id != active_turn
        && matches!(record.state(), DurableOperationState::Prepared)
        && checkpoint
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .any(|command| {
                command.turn_id() == Some(*turn_id)
                    && is_accepted_root(command)
                    && record.request().bytes() == command.command_component().bytes()
            })
}

/// Binds staged Code Cell evidence to the exact source without publishing it.
pub(in crate::runtime) fn reopen_code_cell_abandonment_identity(
    source: &RuntimeCheckpoint,
) -> Result<DurableCodeCellAbandonmentIdentity, AgentError> {
    let topology = source.manifest().topology();
    let address = DurableSessionAddress::new(
        topology.session_id,
        topology.branch_id,
        topology.checkpoint_id,
    );
    let request = DurableCodeCellAbandonmentRequest::new(
        format!(
            "plakous:reopen-code-cell:{}",
            topology.checkpoint_id.as_uuid()
        ),
        address,
    )?;
    DurableCodeCellAbandonmentIdentity::new(&request)
}

async fn publish_code_cell_abandonment_evidence(
    owner: &mut CompactionRecoveryOwner,
    identity: DurableCodeCellAbandonmentIdentity,
) -> Result<(), AgentError> {
    let (history, ledger, lifecycle) =
        code_cell_abandonment_state(&owner.checkpoint, identity, identity.evidence_checkpoint)?;
    let manifest = owner.checkpoint.manifest();
    owner
        .publish(
            identity.evidence_checkpoint,
            lifecycle,
            history,
            ledger,
            manifest.canonical().context().retained_for_exact_resume(),
            manifest.family_artifact().cloned(),
        )
        .await
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "Code Cell abandonment evidence checkpoint was invalid",
            )
        })
}

pub(in crate::runtime) fn code_cell_abandonment_state(
    source: &RuntimeCheckpoint,
    identity: DurableCodeCellAbandonmentIdentity,
    checkpoint_id: crate::CheckpointId,
) -> Result<
    (
        plakous_model::CanonicalHistory,
        crate::RuntimeOperationLedger,
        RuntimeLifecycle,
    ),
    AgentError,
> {
    let fold = code_cell_abandonment_fold(source)?;
    if fold.turn_abandonment {
        return Err(AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "a Code Cell root with mixed finalization debt is abandoned by exact reopen",
        ));
    }
    if fold.operations.is_empty() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let manifest = source.manifest();
    let source_history = manifest.canonical().history().identity();
    let mut ledger = manifest.ledger().clone();
    for (operation_id, disposition) in fold.operations {
        let record = ledger
            .operation(operation_id.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let evidence = DurableCodeCellAbandonmentEvidence {
            version: 1,
            workflow: identity.workflow,
            source_checkpoint: identity.source.checkpoint_id,
            source_history,
            operation_id: operation_id.0,
            request_identity: record.request().integrity(),
            disposition,
        };
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&("plakous-code-cell-abandonment-evidence-v1", evidence)).map_err(
                |_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "Code Cell abandonment evidence could not be canonicalized",
                    )
                },
            )?,
        )?;
        let state = match disposition {
            DurableCodeCellAbandonmentDisposition::ProvenNotDispatched => {
                DurableOperationState::ProvenNotDispatched(evidence)
            }
            DurableCodeCellAbandonmentDisposition::PossiblyStarted => {
                DurableOperationState::Completed(evidence)
            }
        };
        ledger = ledger
            .transition_operation(
                operation_id.0,
                DurableOperationTransition::new(checkpoint_id, state),
            )
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "Code Cell abandonment transition was invalid",
                )
            })?;
    }
    let lifecycle = working_lifecycle(fold.turn_id, &ledger).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "Code Cell abandonment lifecycle was invalid",
        )
    })?;
    if !matches!(
        lifecycle,
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::Finalization,
            ..
        }
    ) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let history = manifest
        .canonical()
        .history()
        .append(code_cell_abandonment_segment(
            identity,
            &fold.evaluation_request,
        )?)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    Ok((history, ledger, lifecycle))
}

fn decode_code_cell_abandonment_evidence(
    component: &ReconstructiveComponent,
) -> Option<DurableCodeCellAbandonmentEvidence> {
    let (tag, evidence): (String, DurableCodeCellAbandonmentEvidence) =
        serde_json::from_slice(component.bytes()).ok()?;
    if tag != "plakous-code-cell-abandonment-evidence-v1"
        || evidence.version != 1
        || serde_json::to_vec(&(tag, evidence.clone()))
            .ok()?
            .as_slice()
            != component.bytes()
    {
        return None;
    }
    Some(evidence)
}

fn validate_code_cell_evidence_checkpoint(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCodeCellAbandonmentIdentity,
) -> Result<(), AgentError> {
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    if topology.checkpoint_id != identity.evidence_checkpoint
        || topology.parent_checkpoint_id != Some(identity.source.checkpoint_id)
        || (topology.session_id, topology.branch_id)
            != (identity.source.session_id, identity.source.branch_id)
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let fold = code_cell_abandonment_fold(checkpoint)?;
    if !fold.operations.is_empty() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let mut abandonment_records = 0_usize;
    let mut source_history = None;
    for obligation in manifest.lifecycle().obligations() {
        if obligation.kind() != RecoveryObligationKind::CodeCell {
            continue;
        }
        let record = manifest
            .ledger()
            .operation(obligation.operation_id())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let decoded = match record.state() {
            DurableOperationState::Completed(component)
            | DurableOperationState::ProvenNotDispatched(component) => {
                decode_code_cell_abandonment_evidence(component)
            }
            _ => None,
        };
        let Some(evidence) = decoded else {
            continue;
        };
        abandonment_records = abandonment_records.saturating_add(1);
        match source_history {
            Some(observed) if observed != evidence.source_history => {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            None => source_history = Some(evidence.source_history),
            Some(_) => {}
        }
        if evidence.workflow != identity.workflow
            || evidence.source_checkpoint != identity.source.checkpoint_id
            || evidence.operation_id != record.operation_id()
            || evidence.request_identity != record.request().integrity()
            || !matches!(
                (record.state(), evidence.disposition),
                (
                    DurableOperationState::ProvenNotDispatched(_),
                    DurableCodeCellAbandonmentDisposition::ProvenNotDispatched
                ) | (
                    DurableOperationState::Completed(_),
                    DurableCodeCellAbandonmentDisposition::PossiblyStarted
                )
            )
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
    }
    if abandonment_records == 0 {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let source_history =
        source_history.ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let history = manifest.canonical().history();
    let segments = history.segments();
    let expected = code_cell_abandonment_segment(identity, &fold.evaluation_request)?;
    if segments.is_empty()
        || segments.get(segments.len().saturating_sub(1)) != Some(&expected)
        || history
            .segment_prefix_identity(segments.len().saturating_sub(1))
            .ok()
            != Some(source_history)
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    Ok(())
}

async fn validate_code_cell_final_receipt(
    store: &Arc<dyn SessionStore>,
    recovered: &CommittedCheckpoint,
    identity: DurableCodeCellAbandonmentIdentity,
    timeout: Duration,
) -> Result<(), AgentError> {
    let target = store_call(
        StorePhase::LoadCheckpoint,
        timeout,
        store.load_checkpoint(identity.final_checkpoint),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    let evidence = store_call(
        StorePhase::LoadCheckpoint,
        timeout,
        store.load_checkpoint(identity.evidence_checkpoint),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    let source = store_call(
        StorePhase::LoadCheckpoint,
        timeout,
        store.load_checkpoint(identity.source.checkpoint_id),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    validate_code_cell_evidence_checkpoint(evidence.checkpoint(), identity)?;
    let target_checkpoint = target.checkpoint();
    let target_manifest = target_checkpoint.manifest();
    let evidence_history = evidence.checkpoint().manifest().canonical().history();
    let target_topology = target_manifest.topology();
    let operation = recovered.operation();
    let receipt = recovered.receipt();
    let expected_commit = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id: identity.final_checkpoint,
        parent_checkpoint_id: identity.evidence_checkpoint,
        branch_revision: target_topology.branch_revision,
        lifecycle: target_manifest.lifecycle(),
        history_identity: target_manifest.canonical().history().identity(),
        ledger: target_manifest.ledger(),
        context_identity: target_manifest.canonical().context().identity(),
        family_artifact: target_manifest.family_artifact(),
    })?;
    if operation.expected().checkpoint() != evidence.reference()
        || operation.lease().session() != identity.source.session()
        || operation.checkpoint() != target.reference()
        || operation.checkpoint_commit_identity() != expected_commit
        || receipt.previous() != operation.expected()
        || receipt.checkpoint() != target.reference()
        || receipt.head().checkpoint() != target.reference()
        || target_topology.checkpoint_id != identity.final_checkpoint
        || target_topology.parent_checkpoint_id != Some(identity.evidence_checkpoint)
        || target_topology.branch_revision != receipt.head().revision().get()
        || target_topology.lease_fence != operation.lease().fence().get()
        || target_topology.commit_identity != expected_commit
        || !matches!(target_manifest.lifecycle(), RuntimeLifecycle::Quiescent)
        || target_manifest.canonical().history().identity() != evidence_history.identity()
        || evidence_history.segments().is_empty()
        || evidence_history
            .segment_prefix_identity(evidence_history.segments().len().saturating_sub(1))
            .ok()
            != Some(
                source
                    .checkpoint()
                    .manifest()
                    .canonical()
                    .history()
                    .identity(),
            )
        || target_manifest.canonical().context().identity()
            != evidence
                .checkpoint()
                .manifest()
                .canonical()
                .context()
                .identity()
        || target_manifest.family_artifact() != evidence.checkpoint().manifest().family_artifact()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    // The cell belongs to the latest admitted turn, not the session's first.
    let root = target_manifest
        .ledger()
        .commands()
        .into_iter()
        .rfind(|command| {
            command.turn_id().is_some()
                && command.disposition() == DurableCommandDisposition::Accepted
        })
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let terminal = terminal_record_at_checkpoint(
        target_checkpoint,
        &CommandId::new(root.command_id())
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
    )?;
    if terminal.checkpoint_id != identity.final_checkpoint
        || !matches!(
            terminal.outcome,
            DurableTerminalOutcome::RecoveryRequired(_)
        )
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    Ok(())
}
