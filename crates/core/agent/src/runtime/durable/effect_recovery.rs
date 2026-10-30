//! Operator effect evidence for one durable obligation.

use crate::recovery_scope::require_recoverable;
use crate::runtime::ToolExecutionResult;
use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::driver::execution::canonicalize_tool_execution;
use crate::runtime::driver::{DurableEffectIntentEnvelope, DurableUndispatchedToolRequest};
use crate::runtime::durable::branch::validate_session_operation_id;
use crate::runtime::durable::{
    DurableEffectEvidenceIdentity, DurableEffectEvidenceRecoveryOutcome,
    DurableEffectEvidenceRecoveryRequest, DurableSessionAddress, DurableSessionRuntime,
    EffectEvidencePreparation,
};
use crate::runtime::operation_record::{
    completed_tool_outcome_component, operation_kind_turn_id, working_lifecycle,
};
use crate::runtime::terminal::finalized_operation_evidence;
use crate::{
    AgentError, AgentErrorKind, CheckpointId, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, RecoveryAction, RuntimeCheckpoint,
    RuntimeLifecycle, RuntimeManifest, RuntimeWorkingRoot, SessionStoreError,
};
use plakous_model::{
    Digest, DigestDomain, EntryId, HistorySegment, ToolCallBlock, ToolResultBlock, ToolResultKind,
};
use plakous_tools::{
    FrozenToolRegistryDescriptor, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, ToolExecutorId, ToolOutcome, ToolOutcomeKind,
};
impl DurableSessionRuntime {
    /// Finalizes one exact Working effect from caller-supplied terminal evidence.
    ///
    /// The driver validates the original claim/dispatch evidence and executor
    /// identity, then completes that same effect without executing the tool.
    /// Identical evidence returns the first committed target. Conflicting
    /// evidence leaves the caller turn `RecoveryRequired` and never mints a
    /// replacement effect identity.
    #[doc(hidden)]
    pub async fn recover_effect_evidence(
        &self,
        request: DurableEffectEvidenceRecoveryRequest,
    ) -> Result<DurableEffectEvidenceRecoveryOutcome, AgentError> {
        let identity = DurableEffectEvidenceIdentity::new(&request)?;
        if let Some(outcome) = self.effect_evidence_receipt(identity, &request).await? {
            return Ok(outcome);
        }
        let mut lease = self
            .acquire_recovery_lease(request.operation_id(), identity.source)
            .await?;
        let recovery = async {
            if let Some(outcome) = self.effect_evidence_receipt(identity, &request).await? {
                return Ok(outcome);
            }
            let mut owner = self
                .load_recovery_owner(&mut lease, identity.source)
                .await?;
            if owner.checkpoint.id() != identity.source.checkpoint_id {
                return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
            }
            require_recoverable(&owner.checkpoint).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "effect-evidence recovery scope is not recoverable by this build",
                )
            })?;
            let context = owner
                .checkpoint
                .manifest()
                .canonical()
                .context()
                .retained_for_exact_resume();
            let family = owner.checkpoint.manifest().family_artifact().cloned();
            match prepare_effect_evidence_publication(&owner.checkpoint, identity, &request)? {
                EffectEvidencePreparation::NotRequired => Ok(
                    DurableEffectEvidenceRecoveryOutcome::NotRequired(identity.source),
                ),
                EffectEvidencePreparation::Conflict => Ok(
                    DurableEffectEvidenceRecoveryOutcome::RecoveryRequired(identity.source),
                ),
                EffectEvidencePreparation::Publish {
                    history,
                    ledger,
                    lifecycle,
                } => {
                    owner
                        .publish(
                            identity.evidence_checkpoint,
                            lifecycle,
                            history,
                            ledger,
                            context,
                            family,
                        )
                        .await?;
                    Ok(DurableEffectEvidenceRecoveryOutcome::Finalized {
                        source: identity.source,
                        target: DurableSessionAddress::new(
                            identity.source.session_id,
                            identity.source.branch_id,
                            identity.evidence_checkpoint,
                        ),
                    })
                }
            }
        }
        .await;
        self.release_recovery_lease(lease, recovery).await
    }

    pub(super) async fn effect_evidence_receipt(
        &self,
        identity: DurableEffectEvidenceIdentity,
        request: &DurableEffectEvidenceRecoveryRequest,
    ) -> Result<Option<DurableEffectEvidenceRecoveryOutcome>, AgentError> {
        match store_call(
            StorePhase::RecoverCommit,
            self.store_timeouts.operation,
            self.store.recover_commit(identity.evidence_checkpoint),
        )
        .await
        {
            Ok(_) => {
                let stored = store_call(
                    StorePhase::LoadCheckpoint,
                    self.store_timeouts.operation,
                    self.store.load_checkpoint(identity.evidence_checkpoint),
                )
                .await
                .map_err(StoreCallError::into_agent)?;
                validate_effect_evidence_checkpoint(stored.checkpoint(), identity, request)?;
                return Ok(Some(DurableEffectEvidenceRecoveryOutcome::Finalized {
                    source: identity.source,
                    target: DurableSessionAddress::new(
                        identity.source.session_id,
                        identity.source.branch_id,
                        identity.evidence_checkpoint,
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
                return Ok(Some(DurableEffectEvidenceRecoveryOutcome::NotRequired(
                    identity.source,
                )));
            }
            return Ok(None);
        }
        if checkpoint.id() == identity.evidence_checkpoint {
            validate_effect_evidence_checkpoint(checkpoint, identity, request)?;
            return Ok(Some(DurableEffectEvidenceRecoveryOutcome::Finalized {
                source: identity.source,
                target: DurableSessionAddress::new(
                    identity.source.session_id,
                    identity.source.branch_id,
                    identity.evidence_checkpoint,
                ),
            }));
        }
        Err(AgentError::from_store(SessionStoreError::IdentityConflict))
    }
}

impl DurableEffectEvidenceRecoveryRequest {
    /// Binds one retry-stable operation to an exact Working effect obligation.
    ///
    /// # Errors
    ///
    /// Returns invalid input when the operation identity is not bounded and canonical.
    pub fn new(
        operation_id: impl Into<String>,
        source: DurableSessionAddress,
        recovery_operation: u64,
        claim_digest: Option<Digest>,
        dispatched: bool,
        executor: ToolExecutorId,
        outcome: ToolOutcome,
    ) -> Result<Self, AgentError> {
        let operation_id = operation_id.into();
        validate_session_operation_id(&operation_id)?;
        if recovery_operation == 0 {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect recovery operation identity is missing",
            ));
        }
        if matches!(outcome.kind(), ToolOutcomeKind::OutcomeUnknown) {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect recovery requires a canonical terminal tool outcome",
            ));
        }
        Ok(Self {
            operation_id,
            source,
            recovery_operation,
            claim_digest,
            dispatched,
            executor,
            outcome,
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

impl DurableEffectEvidenceIdentity {
    pub(in crate::runtime) fn new(
        request: &DurableEffectEvidenceRecoveryRequest,
    ) -> Result<Self, AgentError> {
        let source = request.source();
        let workflow = Digest::sha256(
            DigestDomain::EffectReconciliation,
            1,
            &[
                b"plakous-agent/effect-evidence-recovery-workflow-v1",
                request.operation_id().as_bytes(),
                source.session_id.as_uuid().as_bytes(),
                source.branch_id.as_uuid().as_bytes(),
                source.checkpoint_id.as_uuid().as_bytes(),
                &request.recovery_operation.to_be_bytes(),
            ],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "effect-evidence recovery identity failed",
            )
        })?;
        Ok(Self {
            source,
            recovery_operation: request.recovery_operation,
            workflow,
            evidence_checkpoint: effect_evidence_checkpoint_id(workflow, b"finalized-effect")?,
        })
    }
}

fn effect_evidence_checkpoint_id(
    workflow: Digest,
    role: &[u8],
) -> Result<CheckpointId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::EffectReconciliation,
        1,
        &[
            b"plakous-agent/effect-evidence-recovery-derived-id-v1",
            workflow.bytes(),
            role,
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "effect-evidence recovery identity failed",
        )
    })?;
    derived_uuid_checkpoint(digest)
}

fn effect_evidence_entry_id(workflow: Digest) -> Result<EntryId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::EffectReconciliation,
        1,
        &[
            b"plakous-agent/effect-evidence-recovery-entry-v1",
            workflow.bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "effect-evidence recovery identity failed",
        )
    })?;
    let bytes = crate::derived_id::bytes(digest);
    EntryId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

fn derived_uuid_checkpoint(digest: Digest) -> Result<CheckpointId, AgentError> {
    let bytes = crate::derived_id::bytes(digest);
    CheckpointId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

#[allow(
    clippy::too_many_lines,
    reason = "Effect-evidence recovery prepares a successor only after validating the stored dispatch and terminal outcome together"
)]
fn prepare_effect_evidence_publication(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableEffectEvidenceIdentity,
    request: &DurableEffectEvidenceRecoveryRequest,
) -> Result<EffectEvidencePreparation, AgentError> {
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        ..
    } = manifest.lifecycle()
    else {
        return Ok(EffectEvidencePreparation::NotRequired);
    };
    let Some(record) = manifest
        .ledger()
        .operation(identity.recovery_operation)
        .cloned()
    else {
        return Ok(EffectEvidencePreparation::Conflict);
    };
    if !matches!(record.kind(), DurableOperationKind::Effect { .. }) {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    if operation_kind_turn_id(record.kind()) != Some(*turn_id) {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    match record.state() {
        DurableOperationState::Prepared
        | DurableOperationState::Claimed(_)
        | DurableOperationState::Dispatched
        | DurableOperationState::OutcomeUnknown(_) => {}
        _ => return Ok(EffectEvidencePreparation::Conflict),
    }
    if !effect_obligation_is_evidence_required(manifest.lifecycle(), identity.recovery_operation) {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    if !effect_evidence_matches_ledger(&record, request.claim_digest, request.dispatched) {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    let decoded = decode_effect_call(&record)?;
    if matches!(decoded, EffectRequest::Undispatched(_)) {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    let call = decoded.call();
    if !effect_executor_matches(manifest, call, &request.executor)? {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    if matches!(request.outcome.kind(), ToolOutcomeKind::Success) && !request.dispatched {
        return Ok(EffectEvidencePreparation::Conflict);
    }
    let result_entry_id = effect_evidence_entry_id(identity.workflow)?;
    let execution = terminal_tool_execution(call, request)?;
    let completed = completed_tool_outcome_component(&Ok(execution.clone()), result_entry_id)?;
    let item = canonicalize_tool_execution(
        result_entry_id,
        execution,
        &mut crate::runtime::observation::budget::ObservationBudget::default(),
    )?;
    let kind = history_segment_kind(std::slice::from_ref(&item));
    let history = manifest
        .canonical()
        .history()
        .append(HistorySegment::new(kind, vec![item]).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "canonical history rejected recovered tool evidence",
            )
        })?)
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "canonical history rejected recovered tool evidence",
            )
        })?;
    let mut ledger = manifest.ledger().clone();
    let checkpoint_id = identity.evidence_checkpoint;
    if matches!(
        record.state(),
        DurableOperationState::Prepared | DurableOperationState::Claimed(_)
    ) && request.dispatched
    {
        ledger = ledger
            .transition_operation(
                identity.recovery_operation,
                DurableOperationTransition::new(checkpoint_id, DurableOperationState::Dispatched),
            )
            .map_err(runtime_manifest_error)?;
    }
    let terminal = if request.dispatched {
        DurableOperationState::Completed(completed.clone())
    } else {
        DurableOperationState::ProvenNotDispatched(completed.clone())
    };
    ledger = ledger
        .transition_operation(
            identity.recovery_operation,
            DurableOperationTransition::new(checkpoint_id, terminal),
        )
        .map_err(runtime_manifest_error)?;
    let finalized = finalized_operation_evidence(&completed, checkpoint_id, history.identity())?;
    ledger = ledger
        .transition_operation(
            identity.recovery_operation,
            DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Finalized(finalized),
            ),
        )
        .map_err(runtime_manifest_error)?;
    let lifecycle = working_lifecycle(*turn_id, &ledger)?;
    Ok(EffectEvidencePreparation::Publish {
        history,
        ledger,
        lifecycle,
    })
}

fn effect_obligation_is_evidence_required(lifecycle: &RuntimeLifecycle, operation_id: u64) -> bool {
    let RuntimeLifecycle::Working { obligations, .. } = lifecycle else {
        return false;
    };
    obligations.iter().any(|obligation| {
        obligation.operation_id() == operation_id
            && obligation.action() == RecoveryAction::EffectEvidenceRequired
    })
}

fn effect_evidence_matches_ledger(
    record: &DurableOperationRecord,
    claim_digest: Option<Digest>,
    dispatched: bool,
) -> bool {
    let prior_claim = record.transitions().iter().rev().find_map(|transition| {
        if let DurableOperationState::Claimed(evidence) = transition.state() {
            Some(evidence.integrity())
        } else {
            None
        }
    });
    match record.state() {
        DurableOperationState::Prepared => claim_digest.is_none() && prior_claim.is_none(),
        DurableOperationState::Claimed(evidence) => Some(evidence.integrity()) == claim_digest,
        DurableOperationState::Dispatched | DurableOperationState::OutcomeUnknown(_) => {
            dispatched && prior_claim == claim_digest
        }
        _ => false,
    }
}

pub(in crate::runtime) enum EffectRequest {
    Read(ToolCallBlock),
    Mutating(ToolCallBlock),
    Undispatched(ToolCallBlock),
}

impl EffectRequest {
    pub(in crate::runtime) fn call(&self) -> &ToolCallBlock {
        match self {
            Self::Read(call) | Self::Mutating(call) | Self::Undispatched(call) => call,
        }
    }

    pub(in crate::runtime) fn into_call(self) -> ToolCallBlock {
        match self {
            Self::Read(call) | Self::Mutating(call) | Self::Undispatched(call) => call,
        }
    }
}

pub(in crate::runtime) fn decode_effect_call(
    record: &DurableOperationRecord,
) -> Result<EffectRequest, AgentError> {
    if let Ok(envelope) =
        serde_json::from_slice::<DurableEffectIntentEnvelope>(record.request().bytes())
    {
        return Ok(EffectRequest::Mutating(envelope.call));
    }
    serde_json::from_slice::<(Digest, ToolCallBlock)>(record.request().bytes())
        .map(|(_, call)| EffectRequest::Read(call))
        .or_else(|_| {
            serde_json::from_slice::<DurableUndispatchedToolRequest>(record.request().bytes())
                .and_then(|(tag, _, call, _, _)| {
                    if tag == "proven-not-dispatched-tool-v1" {
                        Ok(EffectRequest::Undispatched(call))
                    } else {
                        Err(serde::de::Error::custom(
                            "unsupported undispatched request tag",
                        ))
                    }
                })
        })
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "effect operation request could not be reconstructed",
            )
        })
}

fn effect_executor_matches(
    manifest: &RuntimeManifest,
    call: &ToolCallBlock,
    executor: &ToolExecutorId,
) -> Result<bool, AgentError> {
    let component = manifest.tools().registry();
    let descriptor = match component.codec_revision() {
        TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v1(component.bytes())
        }
        TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v2(component.bytes())
        }
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "durable tool registry codec is unsupported",
            ));
        }
    }
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "durable tool registry is unsupported or corrupt",
        )
    })?;
    Ok(descriptor
        .executor_id_for_name(call.canonical_name())
        .as_ref()
        == Some(executor))
}

fn terminal_tool_execution(
    call: &ToolCallBlock,
    request: &DurableEffectEvidenceRecoveryRequest,
) -> Result<ToolExecutionResult, AgentError> {
    let kind = match request.outcome.kind() {
        ToolOutcomeKind::Success => ToolResultKind::Success,
        _ => ToolResultKind::Failure,
    };
    let result = ToolResultBlock::new(
        call.call_id(),
        call.owner().clone(),
        kind,
        request.outcome.parts().to_vec(),
        call.correlation().clone(),
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "canonical tool outcome could not be reconstructed",
        )
    })?;
    Ok(ToolExecutionResult::new(result, request.dispatched)
        .with_presentation_subject(request.outcome.presentation_subject().map(str::to_owned)))
}

fn validate_effect_evidence_checkpoint(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableEffectEvidenceIdentity,
    request: &DurableEffectEvidenceRecoveryRequest,
) -> Result<(), AgentError> {
    let manifest = checkpoint.manifest();
    if !effect_evidence_topology_matches(&manifest.topology(), identity) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let record = manifest
        .ledger()
        .operation(identity.recovery_operation)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if !matches!(record.kind(), DurableOperationKind::Effect { .. }) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    #[allow(
        clippy::manual_let_else,
        reason = "state() yields a reference; let-else over owned variants would copy ReconstructiveComponent"
    )]
    let evidence = match record.state() {
        DurableOperationState::Completed(evidence)
        | DurableOperationState::ProvenNotDispatched(evidence)
        | DurableOperationState::Finalized(evidence) => evidence,
        _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
    };
    let decoded = decode_effect_call(record)?;
    if matches!(decoded, EffectRequest::Undispatched(_)) {
        return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
    }
    let expected = terminal_tool_execution(decoded.call(), request)?;
    let (tag, _, execution): (String, EntryId, ToolExecutionResult) =
        serde_json::from_slice(evidence.bytes())
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
    if !retained_evidence_matches(&tag, &execution, &expected) {
        return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
    }
    Ok(())
}

/// The tag a completed tool outcome is retained under.
const COMPLETED_TOOL_OUTCOME_TAG: &str = "completed-tool-outcome-v1";

/// Whether one stored checkpoint is the exact evidence successor `identity` names.
///
/// A checkpoint that is not that successor cannot answer for this recovery, and
/// the difference is a corrupt store rather than conflicting evidence: the
/// caller asked for one identity and the store returned another.
fn effect_evidence_topology_matches(
    topology: &crate::RuntimeTopology,
    identity: DurableEffectEvidenceIdentity,
) -> bool {
    topology.checkpoint_id == identity.evidence_checkpoint
        && topology.parent_checkpoint_id == Some(identity.source.checkpoint_id)
        && (topology.session_id, topology.branch_id)
            == (identity.source.session_id, identity.source.branch_id)
}

/// Whether the evidence a prior commit retained is the outcome now presented.
///
/// The tag is checked with the two fields because a decodable tuple under a
/// different tag is not this record; treating it as one would let unrelated
/// retained bytes stand in for a tool outcome. The host-only presentation
/// subject is deliberately not compared: recovery copies it onto the
/// reconstructed execution when the request carries one, but a subject that
/// differs, or that an older commit omitted, is not an evidence conflict.
fn retained_evidence_matches(
    tag: &str,
    retained: &ToolExecutionResult,
    expected: &ToolExecutionResult,
) -> bool {
    tag == COMPLETED_TOOL_OUTCOME_TAG
        && retained.result == expected.result
        && retained.dispatched == expected.dispatched
}

#[cfg(test)]
mod tests;
