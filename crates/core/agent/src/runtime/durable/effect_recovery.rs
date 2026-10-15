//! Operator effect evidence for one durable obligation.

use crate::recovery_scope::require_recoverable;
use crate::runtime::ToolExecutionResult;
use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::driver::execution::canonicalize_tool_execution;
use crate::runtime::driver::{DurableEffectIntentEnvelope, DurableUndispatchedToolRequest};
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::{operation_run_id, validate_session_operation_id};
use crate::runtime::durable::{
    CompactionRecoveryOwner, DurableEffectEvidenceIdentity, DurableEffectEvidenceRecoveryOutcome,
    DurableEffectEvidenceRecoveryRequest, DurableSessionAddress, DurableSessionRuntime,
    EffectEvidencePreparation,
};
use crate::runtime::operation_record::{
    completed_tool_outcome_component, operation_kind_turn_id, working_lifecycle,
};
use crate::runtime::terminal::finalized_operation_evidence;
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, CheckpointId, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition, RecoveryAction,
    RuntimeCheckpoint, RuntimeLifecycle, RuntimeManifest, RuntimeWorkingRoot, SessionStoreError,
};
use plakous_model::{
    Digest, DigestDomain, EntryId, HistorySegment, ToolCallBlock, ToolResultBlock, ToolResultKind,
};
use plakous_tools::{
    FrozenToolRegistryDescriptor, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, ToolExecutorId, ToolOutcome, ToolOutcomeKind,
};
use std::sync::Arc;
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
            if let Some(outcome) = self.effect_evidence_receipt(identity, &request).await? {
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
        let cleanup =
            release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup).await;
        match (recovery, cleanup) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
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
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EntryId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

fn derived_uuid_checkpoint(digest: Digest) -> Result<CheckpointId, AgentError> {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CheckpointId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

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
mod tests {
    use plakous_model::{
        AttemptId, BranchId, CallId, Digest, DigestDomain, SessionId, ToolExecutionOwner, TurnId,
    };

    use super::{
        COMPLETED_TOOL_OUTCOME_TAG, DurableEffectEvidenceIdentity,
        DurableEffectEvidenceRecoveryRequest, DurableSessionAddress, ToolCallBlock,
        ToolExecutionResult, ToolExecutorId, ToolOutcome, ToolResultBlock, ToolResultKind,
        effect_evidence_matches_ledger, effect_evidence_topology_matches,
        retained_evidence_matches, terminal_tool_execution,
    };
    use crate::runtime::operation_record::operation_binding_component;
    use crate::{
        CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationState,
        DurableOperationTransition, ReconstructiveComponent,
    };

    type TestError = Box<dyn std::error::Error>;

    fn digest(label: &[u8]) -> Result<Digest, TestError> {
        Ok(Digest::sha256(
            DigestDomain::ProviderOperation,
            2,
            &[label],
        )?)
    }

    fn component(label: &[u8]) -> Result<ReconstructiveComponent, TestError> {
        Ok(operation_binding_component(digest(label)?)?)
    }

    fn checkpoint(value: u128) -> Result<CheckpointId, TestError> {
        Ok(CheckpointId::try_from(uuid::Uuid::from_u128(value))?)
    }

    /// One effect record carried through the given states, in order.
    fn effect_record(
        states: Vec<DurableOperationState>,
    ) -> Result<DurableOperationRecord, TestError> {
        let kind = DurableOperationKind::Effect {
            turn_id: TurnId::try_from(uuid::Uuid::from_u128(900))?,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(901))?,
            call_id: CallId::try_from(uuid::Uuid::from_u128(902))?,
            source_index: 0,
        };
        let mut states = states.into_iter().enumerate();
        let (_, first) = states
            .next()
            .ok_or("the fixture needs at least one state")?;
        let mut record = DurableOperationRecord::new(
            1,
            kind,
            component(b"request")?,
            vec![DurableOperationTransition::new(checkpoint(910)?, first)],
        )?;
        for (index, state) in states {
            let at = checkpoint(910_u128.saturating_add(index as u128))?;
            record = record.append_transition(DurableOperationTransition::new(at, state))?;
        }
        Ok(record)
    }

    /// A claim the caller can present, and the evidence the ledger holds for it.
    fn claimed() -> Result<(ReconstructiveComponent, Digest), TestError> {
        let evidence = component(b"claim")?;
        let integrity = evidence.integrity();
        Ok((evidence, integrity))
    }

    #[test]
    fn a_prepared_effect_matches_only_an_unclaimed_request() -> Result<(), TestError> {
        let record = effect_record(vec![DurableOperationState::Prepared])?;
        // No claim was ever taken, so no claim digest is the only truthful one.
        assert!(effect_evidence_matches_ledger(&record, None, false));
        assert!(effect_evidence_matches_ledger(&record, None, true));
        assert!(!effect_evidence_matches_ledger(
            &record,
            Some(digest(b"invented")?),
            false
        ));
        Ok(())
    }

    #[test]
    fn a_claimed_effect_matches_its_own_claim_and_no_other() -> Result<(), TestError> {
        let (evidence, integrity) = claimed()?;
        let record = effect_record(vec![
            DurableOperationState::Prepared,
            DurableOperationState::Claimed(evidence),
        ])?;
        assert!(effect_evidence_matches_ledger(
            &record,
            Some(integrity),
            false
        ));
        // A tampered digest is the case the predicate exists for.
        assert!(!effect_evidence_matches_ledger(
            &record,
            Some(digest(b"tampered")?),
            false
        ));
        // A missing claim is just as wrong as a wrong one.
        assert!(!effect_evidence_matches_ledger(&record, None, false));
        Ok(())
    }

    #[test]
    fn a_dispatched_effect_needs_both_the_prior_claim_and_the_dispatch() -> Result<(), TestError> {
        let (evidence, integrity) = claimed()?;
        let record = effect_record(vec![
            DurableOperationState::Prepared,
            DurableOperationState::Claimed(evidence),
            DurableOperationState::Dispatched,
        ])?;
        assert!(effect_evidence_matches_ledger(
            &record,
            Some(integrity),
            true
        ));
        // The ledger says the request went out; a caller claiming it did not is
        // contradicting durable state, not supplying evidence.
        assert!(!effect_evidence_matches_ledger(
            &record,
            Some(integrity),
            false
        ));
        assert!(!effect_evidence_matches_ledger(
            &record,
            Some(digest(b"tampered")?),
            true
        ));
        assert!(!effect_evidence_matches_ledger(&record, None, true));
        Ok(())
    }

    #[test]
    fn an_unknown_outcome_is_matched_the_same_way_as_a_dispatch() -> Result<(), TestError> {
        let (evidence, integrity) = claimed()?;
        let record = effect_record(vec![
            DurableOperationState::Prepared,
            DurableOperationState::Claimed(evidence),
            DurableOperationState::Dispatched,
            DurableOperationState::OutcomeUnknown(component(b"unknown")?),
        ])?;
        assert!(effect_evidence_matches_ledger(
            &record,
            Some(integrity),
            true
        ));
        assert!(!effect_evidence_matches_ledger(
            &record,
            Some(integrity),
            false
        ));
        Ok(())
    }

    fn address(checkpoint: u128) -> Result<DurableSessionAddress, TestError> {
        Ok(DurableSessionAddress::new(
            SessionId::try_from(uuid::Uuid::from_u128(700))?,
            BranchId::try_from(uuid::Uuid::from_u128(701))?,
            CheckpointId::try_from(uuid::Uuid::from_u128(checkpoint))?,
        ))
    }

    fn identity(source: DurableSessionAddress) -> Result<DurableEffectEvidenceIdentity, TestError> {
        Ok(DurableEffectEvidenceIdentity {
            source,
            recovery_operation: 1,
            workflow: digest(b"workflow")?,
            evidence_checkpoint: checkpoint(720)?,
        })
    }

    fn topology(
        source: DurableSessionAddress,
        checkpoint_id: CheckpointId,
        parent: Option<CheckpointId>,
    ) -> Result<crate::RuntimeTopology, TestError> {
        Ok(crate::RuntimeTopology {
            session_id: source.session_id,
            branch_id: source.branch_id,
            checkpoint_id,
            parent_checkpoint_id: parent,
            branch_revision: 1,
            fork: None,
            lease_fence: 1,
            commit_identity: digest(b"commit")?,
        })
    }

    #[test]
    fn only_the_named_successor_of_the_named_source_answers_for_evidence() -> Result<(), TestError>
    {
        let source = address(710)?;
        let identity = identity(source)?;
        let evidence = checkpoint(720)?;

        let exact = topology(source, evidence, Some(source.checkpoint_id))?;
        assert!(effect_evidence_topology_matches(&exact, identity));

        // A checkpoint that is not the named successor cannot answer, even
        // though it sits on the same branch.
        let other_successor = topology(source, checkpoint(721)?, Some(source.checkpoint_id))?;
        assert!(!effect_evidence_topology_matches(
            &other_successor,
            identity
        ));

        // The successor must descend from the exact source the caller named.
        let other_parent = topology(source, evidence, Some(checkpoint(719)?))?;
        assert!(!effect_evidence_topology_matches(&other_parent, identity));

        // A root has no parent at all, so it is nobody's evidence successor.
        let rootless = topology(source, evidence, None)?;
        assert!(!effect_evidence_topology_matches(&rootless, identity));

        // A same-shaped checkpoint on another session is the case the gate
        // exists for: the store answered with something else entirely.
        let mut foreign = topology(source, evidence, Some(source.checkpoint_id))?;
        foreign.session_id = SessionId::try_from(uuid::Uuid::from_u128(799))?;
        assert!(!effect_evidence_topology_matches(&foreign, identity));

        let mut other_branch = topology(source, evidence, Some(source.checkpoint_id))?;
        other_branch.branch_id = BranchId::try_from(uuid::Uuid::from_u128(798))?;
        assert!(!effect_evidence_topology_matches(&other_branch, identity));
        Ok(())
    }

    fn execution(text: &str, dispatched: bool) -> Result<ToolExecutionResult, TestError> {
        let result = ToolResultBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(902))?,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![plakous_model::ToolResultPart::Text(
                plakous_model::TextBlock::new(text, None)?,
            )],
            plakous_model::ToolCorrelation::default(),
        )?;
        Ok(ToolExecutionResult::new(result, dispatched))
    }

    #[test]
    fn retained_evidence_answers_only_for_the_same_outcome() -> Result<(), TestError> {
        let expected = execution("the tool wrote the file", true)?;
        let same = execution("the tool wrote the file", true)?;
        assert!(retained_evidence_matches(
            COMPLETED_TOOL_OUTCOME_TAG,
            &same,
            &expected
        ));

        // Tampered content under the right tag is the conflict this catches.
        let tampered = execution("the tool wrote a different file", true)?;
        assert!(!retained_evidence_matches(
            COMPLETED_TOOL_OUTCOME_TAG,
            &tampered,
            &expected
        ));

        // The same result claimed as never dispatched is a different fact.
        let undispatched = execution("the tool wrote the file", false)?;
        assert!(!retained_evidence_matches(
            COMPLETED_TOOL_OUTCOME_TAG,
            &undispatched,
            &expected
        ));

        // Decodable bytes under another tag are not this record.
        assert!(!retained_evidence_matches(
            "completed-tool-outcome-v2",
            &same,
            &expected
        ));
        assert!(!retained_evidence_matches("", &same, &expected));
        Ok(())
    }

    fn recovery_call() -> Result<ToolCallBlock, TestError> {
        Ok(ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(902))?,
            plakous_model::ToolName::new("await_agents")?,
            ToolExecutionOwner::Local,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::default(),
            "{}",
        )?)
    }

    fn recovery_request(
        subject: Option<&str>,
    ) -> Result<DurableEffectEvidenceRecoveryRequest, TestError> {
        let outcome = ToolOutcome::success_text("woken")?;
        let outcome = match subject {
            Some(subject) => outcome.with_presentation_subject(subject),
            None => outcome,
        };
        Ok(DurableEffectEvidenceRecoveryRequest::new(
            "effect-evidence-1",
            address(710)?,
            1,
            None,
            true,
            ToolExecutorId::new("local.executor")?,
            outcome,
        )?)
    }

    #[tokio::test]
    async fn prepared_undispatched_cut_refuses_external_success_evidence() -> Result<(), TestError>
    {
        use crate::runtime::continuation::runtime_component;
        use crate::runtime::durable::tests::{
            SessionUpdateFixtureMode, landed_round_abandonment::completed_tool_round_cut,
        };
        use crate::runtime::durable::turn_recovery::review_tests::checkpoint_with_review_state;
        let source =
            completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false)
                .await?;
        let effect = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| matches!(record.kind(), DurableOperationKind::Effect { .. }))
            .ok_or("effect")?;
        let call = super::decode_effect_call(effect)?.into_call();
        let request: crate::runtime::DurableUndispatchedToolRequest = (
            "proven-not-dispatched-tool-v1".into(),
            digest(b"undispatched")?,
            call,
            plakous_model::EntryId::try_from(uuid::Uuid::from_u128(0xEFFF))?,
            execution("not dispatched", false)?,
        );
        let rejected = DurableOperationRecord::new(
            effect.operation_id(),
            effect.kind().clone(),
            runtime_component(
                crate::RuntimeComponentKind::Operation,
                serde_json::to_vec(&request)?,
            )?,
            vec![DurableOperationTransition::new(
                source.id(),
                DurableOperationState::Prepared,
            )],
        )?;
        let records = source
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| {
                if record.operation_id() == effect.operation_id() {
                    rejected.clone()
                } else {
                    record.clone()
                }
            })
            .collect();
        let cut = checkpoint_with_review_state(
            &source,
            records,
            source.manifest().canonical().history().clone(),
            false,
        )?;
        let topology = cut.manifest().topology();
        let external = DurableEffectEvidenceRecoveryRequest::new(
            "evidence:undispatched",
            DurableSessionAddress::new(topology.session_id, topology.branch_id, cut.id()),
            effect.operation_id(),
            None,
            true,
            ToolExecutorId::new("probe.executor")?,
            ToolOutcome::success_text("invented external success")?,
        )?;
        let identity = DurableEffectEvidenceIdentity::new(&external)?;
        assert!(matches!(
            super::prepare_effect_evidence_publication(&cut, identity, &external)?,
            super::EffectEvidencePreparation::Conflict
        ));
        Ok(())
    }

    #[test]
    fn undispatched_request_decoder_checks_the_recorded_tag() -> Result<(), TestError> {
        for tag in ["proven-not-dispatched-tool-v1", "other-tool-v1"] {
            let request: crate::runtime::DurableUndispatchedToolRequest = (
                tag.into(),
                digest(b"request")?,
                recovery_call()?,
                plakous_model::EntryId::try_from(uuid::Uuid::from_u128(0xEFFF))?,
                execution("not dispatched", false)?,
            );
            let original = effect_record(vec![DurableOperationState::Prepared])?;
            let record = DurableOperationRecord::new(
                original.operation_id(),
                original.kind().clone(),
                crate::runtime::continuation::runtime_component(
                    crate::RuntimeComponentKind::Operation,
                    serde_json::to_vec(&request)?,
                )?,
                original.transitions().to_vec(),
            )?;
            assert_eq!(
                super::decode_effect_call(&record).is_ok(),
                tag == "proven-not-dispatched-tool-v1"
            );
        }
        Ok(())
    }

    #[test]
    fn recovered_terminal_execution_carries_the_presentation_subject() -> Result<(), TestError> {
        let call = recovery_call()?;
        let execution = terminal_tool_execution(&call, &recovery_request(Some("c1,c2"))?)?;
        assert_eq!(execution.presentation_subject.as_deref(), Some("c1,c2"));

        let bare = terminal_tool_execution(&call, &recovery_request(None)?)?;
        assert_eq!(bare.presentation_subject.as_deref(), None);
        // Same canonical result and dispatch. The subject is not evidence.
        assert!(retained_evidence_matches(
            COMPLETED_TOOL_OUTCOME_TAG,
            &bare,
            &execution
        ));
        Ok(())
    }

    #[test]
    fn an_already_terminal_effect_matches_nothing() -> Result<(), TestError> {
        let (evidence, integrity) = claimed()?;
        let record = effect_record(vec![
            DurableOperationState::Prepared,
            DurableOperationState::Claimed(evidence),
            DurableOperationState::Dispatched,
            DurableOperationState::Completed(component(b"outcome")?),
        ])?;
        // A completed effect is answered by the receipt path, never by fresh
        // evidence; letting evidence match here would overwrite a settled fact.
        assert!(!effect_evidence_matches_ledger(
            &record,
            Some(integrity),
            true
        ));
        assert!(!effect_evidence_matches_ledger(&record, None, false));
        Ok(())
    }
}
