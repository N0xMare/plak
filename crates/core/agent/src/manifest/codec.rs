//! Canonical value primitives, lifecycle envelopes, and paired wire tags.

use super::{
    Arc, AttemptId, BTreeMap, BindingReference, BranchId, CallId, CheckpointId, Digest,
    DigestAlgorithm, DigestDomain, DurableCommandDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, FamilyArtifact, ForkLineage, HistorySegment, LIFECYCLE_MAGIC,
    MAX_CHECKPOINT_HISTORY_SEGMENT_BYTES, MAX_DURABLE_OPERATION_TRANSITIONS, MAX_ID_BYTES,
    MAX_RUNTIME_COMPONENT_BYTES, MAX_RUNTIME_LIFECYCLE_BYTES, MAX_RUNTIME_RECORDS,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelRouteBinding, PROCESS_SESSION_ID_BYTES,
    PendingEffectDecisionState, ProcessSessionId, ProcessSessionOwnerGeneration, ProtocolId,
    RUNTIME_LIFECYCLE_CODEC_REVISION, ReconstructiveComponent, RecoveryAction,
    RecoveryHandlerSignature, RecoveryObligation, RecoveryObligationKind, RevalidationFact,
    RouteId, RuntimeComponentKind, RuntimeLifecycle, RuntimeManifestError, RuntimeOperationRoot,
    RuntimeTopology, RuntimeWorkingRoot, SessionId, SessionWorkflowRootId, StableRequestPrefix,
    Timestamp, ToolRuntimeManifest, TurnId, WorkingRecoveryPhase, root_integrity,
    validate_lifecycle_shape,
};

/// Single canonical lifecycle/obligation representation used by manifests and
/// every identity derived from lifecycle authority.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimeLifecycleCodec;

impl RuntimeLifecycleCodec {
    pub(crate) fn encode(lifecycle: &RuntimeLifecycle) -> Result<Vec<u8>, RuntimeManifestError> {
        validate_lifecycle_shape(lifecycle)?;
        let mut writer = Writer::default();
        writer.lifecycle_body(lifecycle)?;
        let integrity = root_integrity(b"runtime-lifecycle", &writer.bytes)?;
        encode_envelope(
            LIFECYCLE_MAGIC,
            RUNTIME_LIFECYCLE_CODEC_REVISION,
            &writer.bytes,
            integrity,
            MAX_RUNTIME_LIFECYCLE_BYTES,
        )
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<RuntimeLifecycle, RuntimeManifestError> {
        let body = decode_envelope(
            bytes,
            LIFECYCLE_MAGIC,
            RUNTIME_LIFECYCLE_CODEC_REVISION,
            MAX_RUNTIME_LIFECYCLE_BYTES,
            b"runtime-lifecycle",
        )?;
        let mut reader = Reader::new(body);
        let lifecycle = reader.lifecycle_body()?;
        reader.finish()?;
        validate_lifecycle_shape(&lifecycle)?;
        Ok(lifecycle)
    }
}

#[derive(Default)]
pub(super) struct Writer {
    pub(super) bytes: Vec<u8>,
}

impl Writer {
    pub(super) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn optional(&mut self, present: bool) {
        self.u8(u8::from(present));
    }

    pub(super) fn raw(&mut self, bytes: &[u8], maximum: usize) -> Result<(), RuntimeManifestError> {
        if bytes.len() > maximum {
            return Err(RuntimeManifestError::ComponentSize);
        }
        let length = u32::try_from(bytes.len()).map_err(|_| RuntimeManifestError::ComponentSize)?;
        self.u32(length);
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub(super) fn string(
        &mut self,
        value: &str,
        maximum: usize,
    ) -> Result<(), RuntimeManifestError> {
        self.raw(value.as_bytes(), maximum)
    }

    pub(super) fn id(&mut self, id: uuid::Uuid) {
        self.bytes.extend_from_slice(id.as_bytes());
    }

    pub(super) fn timestamp(&mut self, timestamp: Timestamp) {
        self.u64(timestamp.seconds());
        self.u32(timestamp.nanoseconds());
    }

    pub(super) fn digest(&mut self, digest: Digest) -> Result<(), RuntimeManifestError> {
        self.u8(match digest.algorithm() {
            DigestAlgorithm::Sha256 => 1,
            _ => return Err(RuntimeManifestError::InvalidDigest),
        });
        self.u8(digest_domain_tag(digest.domain())?);
        self.u32(digest.version());
        self.bytes.extend_from_slice(digest.bytes());
        Ok(())
    }

    pub(super) fn component(
        &mut self,
        component: &ReconstructiveComponent,
    ) -> Result<(), RuntimeManifestError> {
        self.u8(component.kind as u8);
        self.u32(component.codec_revision);
        self.raw(&component.bytes, component.kind.max_bytes())?;
        self.digest(component.integrity)
    }

    pub(super) fn topology(
        &mut self,
        topology: RuntimeTopology,
    ) -> Result<(), RuntimeManifestError> {
        self.id(topology.session_id.as_uuid());
        self.id(topology.branch_id.as_uuid());
        self.id(topology.checkpoint_id.as_uuid());
        self.optional(topology.parent_checkpoint_id.is_some());
        if let Some(parent) = topology.parent_checkpoint_id {
            self.id(parent.as_uuid());
        }
        self.u64(topology.branch_revision);
        self.optional(topology.fork.is_some());
        if let Some(fork) = topology.fork {
            self.id(fork.session_id.as_uuid());
            self.id(fork.branch_id.as_uuid());
            self.id(fork.checkpoint_id.as_uuid());
            self.u64(fork.branch_revision);
        }
        self.u64(topology.lease_fence);
        self.digest(topology.commit_identity)
    }

    pub(super) fn lifecycle(
        &mut self,
        lifecycle: &RuntimeLifecycle,
    ) -> Result<(), RuntimeManifestError> {
        self.raw(
            &RuntimeLifecycleCodec::encode(lifecycle)?,
            MAX_RUNTIME_LIFECYCLE_BYTES,
        )
    }

    fn lifecycle_body(&mut self, lifecycle: &RuntimeLifecycle) -> Result<(), RuntimeManifestError> {
        match lifecycle {
            RuntimeLifecycle::Quiescent => self.u8(1),
            RuntimeLifecycle::Working {
                root,
                recovery_phase,
                obligations,
            } => {
                self.u8(2);
                self.working_root(*root);
                self.u8(recovery_phase_tag(*recovery_phase));
                self.count(obligations.len())?;
                for obligation in obligations {
                    self.u64(obligation.operation_id);
                    self.recovery_signature(obligation.signature);
                }
            }
        }
        Ok(())
    }

    fn working_root(&mut self, root: RuntimeWorkingRoot) {
        match root {
            RuntimeWorkingRoot::Turn {
                turn_id,
                next_round,
            } => {
                self.u8(1);
                self.id(turn_id.as_uuid());
                self.u32(next_round);
            }
            RuntimeWorkingRoot::SessionWorkflow(workflow_id) => {
                self.u8(2);
                self.bytes.extend_from_slice(workflow_id.as_bytes());
            }
        }
    }

    fn recovery_signature(&mut self, signature: RecoveryHandlerSignature) {
        self.u8(obligation_kind_tag(signature.kind));
        self.u8(recovery_action_tag(signature.action));
    }

    pub(super) fn stable_prefix(
        &mut self,
        prefix: &StableRequestPrefix,
    ) -> Result<(), RuntimeManifestError> {
        self.raw(prefix.canonical_bytes(), MAX_RUNTIME_COMPONENT_BYTES)?;
        self.digest(prefix.identity())
    }

    pub(super) fn segment(&mut self, segment: &HistorySegment) -> Result<(), RuntimeManifestError> {
        self.raw(
            segment.canonical_bytes(),
            MAX_CHECKPOINT_HISTORY_SEGMENT_BYTES,
        )?;
        self.digest(segment.identity())
    }

    pub(super) fn binding_reference(
        &mut self,
        reference: &BindingReference,
    ) -> Result<(), RuntimeManifestError> {
        self.string(&reference.authority, MAX_ID_BYTES)?;
        self.string(&reference.subject, MAX_ID_BYTES)?;
        self.u64(reference.revision);
        Ok(())
    }

    fn revalidation(&mut self, fact: &RevalidationFact) -> Result<(), RuntimeManifestError> {
        self.binding_reference(&fact.reference)?;
        self.timestamp(fact.observed_at);
        self.optional(fact.valid_until.is_some());
        if let Some(until) = fact.valid_until {
            self.timestamp(until);
        }
        self.component(&fact.evidence)
    }

    pub(super) fn binding(
        &mut self,
        binding: &ModelRouteBinding,
    ) -> Result<(), RuntimeManifestError> {
        self.string(binding.protocol.as_str(), MAX_ID_BYTES)?;
        self.string(binding.route.as_str(), MAX_ID_BYTES)?;
        self.string(binding.requested_model.as_str(), MAX_ID_BYTES)?;
        self.optional(binding.reported_model.is_some());
        if let Some(model) = &binding.reported_model {
            self.string(model.as_str(), MAX_ID_BYTES)?;
        }
        self.optional(binding.model_family.is_some());
        if let Some(family) = &binding.model_family {
            self.string(family.as_str(), MAX_ID_BYTES)?;
        }
        self.string(binding.adapter.as_str(), MAX_ID_BYTES)?;
        self.binding_reference(&binding.account)?;
        self.binding_reference(&binding.profile)?;
        self.digest(binding.model_binding)?;
        self.component(&binding.descriptor)?;
        self.revalidation(&binding.admission)
    }

    pub(super) fn tools(
        &mut self,
        tools: &ToolRuntimeManifest,
    ) -> Result<(), RuntimeManifestError> {
        self.component(&tools.registry)?;
        self.component(&tools.exposure)?;
        self.component(&tools.environment)?;
        self.component(&tools.effect_policy)?;
        self.binding_reference(&tools.policy)
    }

    pub(super) fn command_record(
        &mut self,
        command: &DurableCommandRecord,
    ) -> Result<(), RuntimeManifestError> {
        self.string(&command.command_id, MAX_ID_BYTES)?;
        self.u64(command.sequence);
        self.optional(command.turn_id.is_some());
        if let Some(turn_id) = command.turn_id {
            self.id(turn_id.as_uuid());
        }
        self.digest(command.binding)?;
        self.component(&command.command)?;
        self.u8(match command.disposition {
            DurableCommandDisposition::Accepted => 1,
            DurableCommandDisposition::Rejected => 2,
        });
        self.optional(command.rejection_code.is_some());
        if let Some(code) = &command.rejection_code {
            self.string(code, MAX_ID_BYTES)?;
        }
        Ok(())
    }

    pub(super) fn operation_kind(
        &mut self,
        kind: &DurableOperationKind,
    ) -> Result<(), RuntimeManifestError> {
        match kind {
            DurableOperationKind::Command { turn_id } => {
                self.u8(1);
                self.optional(turn_id.is_some());
                if let Some(turn_id) = turn_id {
                    self.id(turn_id.as_uuid());
                }
            }
            DurableOperationKind::Provider { root, attempt_id } => {
                self.u8(2);
                self.operation_root(*root);
                self.id(attempt_id.as_uuid());
            }
            DurableOperationKind::EffectDecision {
                turn_id,
                attempt_id,
                call_id,
            } => {
                self.u8(3);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
                self.id(call_id.as_uuid());
            }
            DurableOperationKind::Effect {
                turn_id,
                attempt_id,
                call_id,
                source_index,
            } => {
                self.u8(4);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
                self.id(call_id.as_uuid());
                self.u32(*source_index);
            }
            DurableOperationKind::ToolBatchContinuation {
                turn_id,
                attempt_id,
            } => {
                self.u8(11);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
            }
            DurableOperationKind::ServerPauseContinuation {
                turn_id,
                attempt_id,
            } => {
                self.u8(12);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
            }
            DurableOperationKind::CompositionEvaluation {
                turn_id,
                attempt_id,
                call_id,
                source_index,
            } => {
                self.u8(8);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
                self.id(call_id.as_uuid());
                self.u32(*source_index);
            }
            DurableOperationKind::CompositionContinuation {
                turn_id,
                attempt_id,
                call_id,
                source_index,
                evaluation_operation_id,
            } => {
                self.u8(10);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
                self.id(call_id.as_uuid());
                self.u32(*source_index);
                self.u64(*evaluation_operation_id);
            }
            DurableOperationKind::CompositionChild {
                turn_id,
                attempt_id,
                call_id,
                evaluation_operation_id,
                interpreter_generation,
                cell_id,
                request_sequence,
            } => {
                self.u8(9);
                self.id(turn_id.as_uuid());
                self.id(attempt_id.as_uuid());
                self.id(call_id.as_uuid());
                self.u64(*evaluation_operation_id);
                self.u64(*interpreter_generation);
                self.string(cell_id, MAX_ID_BYTES)?;
                self.u32(*request_sequence);
            }
            DurableOperationKind::ProcessSession {
                turn_id,
                session_id,
                owner_generation,
            } => {
                self.u8(7);
                self.id(turn_id.as_uuid());
                self.bytes.extend_from_slice(&session_id.as_bytes());
                self.u64(owner_generation.get());
            }
            DurableOperationKind::Commit { root } => {
                self.u8(5);
                self.operation_root(*root);
            }
            DurableOperationKind::Compaction { workflow_id } => {
                self.u8(6);
                self.bytes.extend_from_slice(workflow_id.as_bytes());
            }
        }
        Ok(())
    }

    fn operation_root(&mut self, root: RuntimeOperationRoot) {
        match root {
            RuntimeOperationRoot::Turn(turn_id) => {
                self.u8(1);
                self.id(turn_id.as_uuid());
            }
            RuntimeOperationRoot::SessionWorkflow(workflow_id) => {
                self.u8(2);
                self.bytes.extend_from_slice(workflow_id.as_bytes());
            }
        }
    }

    pub(super) fn family_artifact(
        &mut self,
        artifact: &FamilyArtifact,
    ) -> Result<(), RuntimeManifestError> {
        self.string(&artifact.family, MAX_ID_BYTES)?;
        self.u32(artifact.schema_revision);
        self.u32(artifact.codec_revision);
        self.component(&artifact.binding_component)?;
        self.raw(&artifact.bytes, MAX_RUNTIME_COMPONENT_BYTES)?;
        self.digest(artifact.integrity)
    }

    pub(super) fn count(&mut self, count: usize) -> Result<(), RuntimeManifestError> {
        if count > MAX_RUNTIME_RECORDS {
            return Err(RuntimeManifestError::RecordLimit);
        }
        self.usize(count)
    }

    pub(super) fn usize(&mut self, value: usize) -> Result<(), RuntimeManifestError> {
        self.u64(u64::try_from(value).map_err(|_| RuntimeManifestError::Malformed)?);
        Ok(())
    }
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], RuntimeManifestError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(RuntimeManifestError::Malformed)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(RuntimeManifestError::Malformed)?;
        self.offset = end;
        Ok(value)
    }

    pub(super) fn u8(&mut self) -> Result<u8, RuntimeManifestError> {
        Ok(self.take(1)?[0])
    }
    pub(super) fn u32(&mut self) -> Result<u32, RuntimeManifestError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| RuntimeManifestError::Malformed)?,
        ))
    }
    pub(super) fn u64(&mut self) -> Result<u64, RuntimeManifestError> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| RuntimeManifestError::Malformed)?,
        ))
    }
    pub(super) fn optional(&mut self) -> Result<bool, RuntimeManifestError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(RuntimeManifestError::UnknownTag {
                field: "optional",
                tag,
            }),
        }
    }
    fn length(&mut self, maximum: usize) -> Result<usize, RuntimeManifestError> {
        let length = usize::try_from(self.u32()?).map_err(|_| RuntimeManifestError::Malformed)?;
        if length > maximum {
            return Err(RuntimeManifestError::ComponentSize);
        }
        Ok(length)
    }
    pub(super) fn raw(&mut self, maximum: usize) -> Result<Arc<[u8]>, RuntimeManifestError> {
        let length = self.length(maximum)?;
        Ok(Arc::from(self.take(length)?))
    }
    pub(super) fn string(&mut self, maximum: usize) -> Result<String, RuntimeManifestError> {
        String::from_utf8(self.raw(maximum)?.to_vec()).map_err(|_| RuntimeManifestError::Malformed)
    }
    fn uuid(&mut self) -> Result<uuid::Uuid, RuntimeManifestError> {
        Ok(uuid::Uuid::from_bytes(
            self.take(16)?
                .try_into()
                .map_err(|_| RuntimeManifestError::Malformed)?,
        ))
    }
    pub(super) fn session_id(&mut self) -> Result<SessionId, RuntimeManifestError> {
        SessionId::try_from(self.uuid()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn branch_id(&mut self) -> Result<BranchId, RuntimeManifestError> {
        BranchId::try_from(self.uuid()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn checkpoint_id(&mut self) -> Result<CheckpointId, RuntimeManifestError> {
        CheckpointId::try_from(self.uuid()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn turn_id(&mut self) -> Result<TurnId, RuntimeManifestError> {
        TurnId::try_from(self.uuid()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn workflow_id(&mut self) -> Result<SessionWorkflowRootId, RuntimeManifestError> {
        SessionWorkflowRootId::new(
            self.take(plakous_model::SESSION_WORKFLOW_ROOT_ID_BYTES)?
                .try_into()
                .map_err(|_| RuntimeManifestError::Malformed)?,
        )
        .map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn process_session_id(&mut self) -> Result<ProcessSessionId, RuntimeManifestError> {
        ProcessSessionId::new(
            self.take(PROCESS_SESSION_ID_BYTES)?
                .try_into()
                .map_err(|_| RuntimeManifestError::Malformed)?,
        )
    }
    pub(super) fn process_session_owner_generation(
        &mut self,
    ) -> Result<ProcessSessionOwnerGeneration, RuntimeManifestError> {
        ProcessSessionOwnerGeneration::new(self.u64()?)
    }
    pub(super) fn operation_root(&mut self) -> Result<RuntimeOperationRoot, RuntimeManifestError> {
        match self.u8()? {
            1 => self.turn_id().map(RuntimeOperationRoot::Turn),
            2 => self
                .workflow_id()
                .map(RuntimeOperationRoot::SessionWorkflow),
            tag => Err(RuntimeManifestError::UnknownTag {
                field: "operation root",
                tag,
            }),
        }
    }
    fn working_root(&mut self) -> Result<RuntimeWorkingRoot, RuntimeManifestError> {
        match self.u8()? {
            1 => Ok(RuntimeWorkingRoot::Turn {
                turn_id: self.turn_id()?,
                next_round: self.u32()?,
            }),
            2 => self.workflow_id().map(RuntimeWorkingRoot::SessionWorkflow),
            tag => Err(RuntimeManifestError::UnknownTag {
                field: "working operation root",
                tag,
            }),
        }
    }
    pub(super) fn attempt_id(&mut self) -> Result<AttemptId, RuntimeManifestError> {
        AttemptId::try_from(self.uuid()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn call_id(&mut self) -> Result<CallId, RuntimeManifestError> {
        CallId::try_from(self.uuid()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    fn timestamp(&mut self) -> Result<Timestamp, RuntimeManifestError> {
        Timestamp::new(self.u64()?, self.u32()?).map_err(|_| RuntimeManifestError::Malformed)
    }

    pub(super) fn digest(
        &mut self,
        expected: Option<DigestDomain>,
    ) -> Result<Digest, RuntimeManifestError> {
        if self.u8()? != 1 {
            return Err(RuntimeManifestError::InvalidDigest);
        }
        let domain = digest_domain(self.u8()?)?;
        if expected.is_some_and(|expected| expected != domain) {
            return Err(RuntimeManifestError::InvalidDigest);
        }
        let version = self.u32()?;
        let bytes = self
            .take(32)?
            .try_into()
            .map_err(|_| RuntimeManifestError::Malformed)?;
        Digest::from_sha256_bytes(domain, version, bytes)
            .map_err(|_| RuntimeManifestError::InvalidDigest)
    }

    pub(super) fn component(
        &mut self,
        expected: RuntimeComponentKind,
    ) -> Result<ReconstructiveComponent, RuntimeManifestError> {
        let kind = RuntimeComponentKind::from_tag(self.u8()?)?;
        if kind != expected {
            return Err(RuntimeManifestError::ComponentKind);
        }
        let revision = self.u32()?;
        let bytes = self.raw(kind.max_bytes())?;
        let integrity = self.digest(Some(DigestDomain::Blob))?;
        ReconstructiveComponent::from_recorded(kind, revision, bytes, integrity)
    }

    pub(super) fn topology(&mut self) -> Result<RuntimeTopology, RuntimeManifestError> {
        let session_id = self.session_id()?;
        let branch_id = self.branch_id()?;
        let checkpoint_id = self.checkpoint_id()?;
        let parent_checkpoint_id = self.optional()?.then(|| self.checkpoint_id()).transpose()?;
        let branch_revision = self.u64()?;
        let fork = if self.optional()? {
            Some(ForkLineage {
                session_id: self.session_id()?,
                branch_id: self.branch_id()?,
                checkpoint_id: self.checkpoint_id()?,
                branch_revision: self.u64()?,
            })
        } else {
            None
        };
        let lease_fence = self.u64()?;
        let commit_identity = self.digest(Some(DigestDomain::ProviderOperation))?;
        Ok(RuntimeTopology {
            session_id,
            branch_id,
            checkpoint_id,
            parent_checkpoint_id,
            branch_revision,
            fork,
            lease_fence,
            commit_identity,
        })
    }

    pub(super) fn lifecycle(&mut self) -> Result<RuntimeLifecycle, RuntimeManifestError> {
        let bytes = self.raw(MAX_RUNTIME_LIFECYCLE_BYTES)?;
        RuntimeLifecycleCodec::decode(&bytes)
    }

    fn lifecycle_body(&mut self) -> Result<RuntimeLifecycle, RuntimeManifestError> {
        match self.u8()? {
            1 => Ok(RuntimeLifecycle::Quiescent),
            2 => {
                let root = self.working_root()?;
                let recovery_phase = recovery_phase(self.u8()?)?;
                let count = self.count()?;
                let mut obligations = Vec::with_capacity(count);
                for _ in 0..count {
                    obligations.push(RecoveryObligation::new(
                        self.u64()?,
                        self.recovery_signature()?,
                    ));
                }
                Ok(RuntimeLifecycle::Working {
                    root,
                    recovery_phase,
                    obligations,
                })
            }
            tag => Err(RuntimeManifestError::UnknownTag {
                field: "lifecycle",
                tag,
            }),
        }
    }

    fn recovery_signature(&mut self) -> Result<RecoveryHandlerSignature, RuntimeManifestError> {
        Ok(RecoveryHandlerSignature::new(
            obligation_kind(self.u8()?)?,
            recovery_action(self.u8()?)?,
        ))
    }

    pub(super) fn stable_prefix(&mut self) -> Result<StableRequestPrefix, RuntimeManifestError> {
        let bytes = self.raw(MAX_RUNTIME_COMPONENT_BYTES)?;
        let identity = self.digest(Some(DigestDomain::StableRequestPrefix))?;
        decode_stable_prefix(&bytes, identity)
    }
    pub(super) fn segment(&mut self) -> Result<HistorySegment, RuntimeManifestError> {
        let bytes = self.raw(MAX_CHECKPOINT_HISTORY_SEGMENT_BYTES)?;
        let identity = self.digest(Some(DigestDomain::CanonicalHistorySegment))?;
        decode_history_segment(&bytes, identity)
    }
    fn binding_reference(&mut self) -> Result<BindingReference, RuntimeManifestError> {
        BindingReference::new(
            self.string(MAX_ID_BYTES)?,
            self.string(MAX_ID_BYTES)?,
            self.u64()?,
        )
    }
    fn revalidation(&mut self) -> Result<RevalidationFact, RuntimeManifestError> {
        let reference = self.binding_reference()?;
        let observed_at = self.timestamp()?;
        let valid_until = self.optional()?.then(|| self.timestamp()).transpose()?;
        let evidence = self.component(RuntimeComponentKind::AdmissionEvidence)?;
        RevalidationFact::new(reference, observed_at, valid_until, evidence)
    }
    pub(super) fn binding(&mut self) -> Result<ModelRouteBinding, RuntimeManifestError> {
        let protocol = ProtocolId::new(self.string(MAX_ID_BYTES)?)
            .map_err(|_| RuntimeManifestError::Malformed)?;
        let route = RouteId::new(self.string(MAX_ID_BYTES)?)
            .map_err(|_| RuntimeManifestError::Malformed)?;
        let requested_model = ModelId::new(self.string(MAX_ID_BYTES)?)
            .map_err(|_| RuntimeManifestError::Malformed)?;
        let reported_model = self
            .optional()?
            .then(|| self.string(MAX_ID_BYTES))
            .transpose()?
            .map(ModelId::new)
            .transpose()
            .map_err(|_| RuntimeManifestError::Malformed)?;
        let model_family = self
            .optional()?
            .then(|| self.string(MAX_ID_BYTES))
            .transpose()?
            .map(ModelFamilyId::new)
            .transpose()
            .map_err(|_| RuntimeManifestError::Malformed)?;
        let adapter = ModelAdapterProfileId::new(self.string(MAX_ID_BYTES)?)
            .map_err(|_| RuntimeManifestError::Malformed)?;
        let account = self.binding_reference()?;
        let profile = self.binding_reference()?;
        let model_binding = self.digest(Some(DigestDomain::ModelBinding))?;
        let descriptor = self.component(RuntimeComponentKind::ModelDescriptor)?;
        let admission = self.revalidation()?;
        ModelRouteBinding::new(
            protocol,
            route,
            requested_model,
            reported_model,
            model_family,
            adapter,
            account,
            profile,
            model_binding,
            descriptor,
            admission,
        )
    }
    pub(super) fn tools(&mut self) -> Result<ToolRuntimeManifest, RuntimeManifestError> {
        ToolRuntimeManifest::new(
            self.component(RuntimeComponentKind::ToolRegistry)?,
            self.component(RuntimeComponentKind::ToolExposure)?,
            self.component(RuntimeComponentKind::ExecutionEnvironment)?,
            self.component(RuntimeComponentKind::EffectPolicy)?,
            self.binding_reference()?,
        )
    }

    pub(super) fn command_record(&mut self) -> Result<DurableCommandRecord, RuntimeManifestError> {
        let command_id = self.string(MAX_ID_BYTES)?;
        let sequence = self.u64()?;
        let turn_id = self.optional()?.then(|| self.turn_id()).transpose()?;
        let binding = self.digest(None)?;
        let command = self.component(RuntimeComponentKind::Command)?;
        let disposition = match self.u8()? {
            1 => DurableCommandDisposition::Accepted,
            2 => DurableCommandDisposition::Rejected,
            tag => {
                return Err(RuntimeManifestError::UnknownTag {
                    field: "command disposition",
                    tag,
                });
            }
        };
        let rejection_code = self
            .optional()?
            .then(|| self.string(MAX_ID_BYTES))
            .transpose()?;
        DurableCommandRecord::new(
            command_id,
            sequence,
            turn_id,
            binding,
            command,
            disposition,
            rejection_code,
        )
    }

    pub(super) fn operation_kind(&mut self) -> Result<DurableOperationKind, RuntimeManifestError> {
        match self.u8()? {
            1 => Ok(DurableOperationKind::Command {
                turn_id: self.optional()?.then(|| self.turn_id()).transpose()?,
            }),
            2 => Ok(DurableOperationKind::Provider {
                root: self.operation_root()?,
                attempt_id: self.attempt_id()?,
            }),
            3 => Ok(DurableOperationKind::EffectDecision {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
                call_id: self.call_id()?,
            }),
            4 => Ok(DurableOperationKind::Effect {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
                call_id: self.call_id()?,
                source_index: self.u32()?,
            }),
            5 => Ok(DurableOperationKind::Commit {
                root: self.operation_root()?,
            }),
            6 => Ok(DurableOperationKind::Compaction {
                workflow_id: self.workflow_id()?,
            }),
            7 => Ok(DurableOperationKind::ProcessSession {
                turn_id: self.turn_id()?,
                session_id: self.process_session_id()?,
                owner_generation: self.process_session_owner_generation()?,
            }),
            8 => Ok(DurableOperationKind::CompositionEvaluation {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
                call_id: self.call_id()?,
                source_index: self.u32()?,
            }),
            9 => Ok(DurableOperationKind::CompositionChild {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
                call_id: self.call_id()?,
                evaluation_operation_id: self.u64()?,
                interpreter_generation: self.u64()?,
                cell_id: self.string(MAX_ID_BYTES)?,
                request_sequence: self.u32()?,
            }),
            10 => Ok(DurableOperationKind::CompositionContinuation {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
                call_id: self.call_id()?,
                source_index: self.u32()?,
                evaluation_operation_id: self.u64()?,
            }),
            11 => Ok(DurableOperationKind::ToolBatchContinuation {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
            }),
            12 => Ok(DurableOperationKind::ServerPauseContinuation {
                turn_id: self.turn_id()?,
                attempt_id: self.attempt_id()?,
            }),
            tag => Err(RuntimeManifestError::UnknownTag {
                field: "operation kind",
                tag,
            }),
        }
    }
    pub(super) fn family_artifact(&mut self) -> Result<FamilyArtifact, RuntimeManifestError> {
        let family = self.string(MAX_ID_BYTES)?;
        let schema = self.u32()?;
        let codec = self.u32()?;
        let binding = self.component(RuntimeComponentKind::FamilyBinding)?;
        let bytes = self.raw(MAX_RUNTIME_COMPONENT_BYTES)?;
        let integrity = self.digest(Some(DigestDomain::Blob))?;
        FamilyArtifact::from_recorded(family, schema, codec, &binding, bytes, integrity)
    }
    pub(super) fn count(&mut self) -> Result<usize, RuntimeManifestError> {
        let count = self.usize()?;
        if count > MAX_RUNTIME_RECORDS {
            return Err(RuntimeManifestError::RecordLimit);
        }
        Ok(count)
    }
    fn usize(&mut self) -> Result<usize, RuntimeManifestError> {
        usize::try_from(self.u64()?).map_err(|_| RuntimeManifestError::Malformed)
    }
    pub(super) fn finish(self) -> Result<(), RuntimeManifestError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(RuntimeManifestError::TrailingBytes)
        }
    }
}

/// Persisted form of one operation record. Every component is named by the
/// content digest of its own reconstructive-component object, never inlined,
/// so a new record version re-encodes references rather than evidence bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationRecordWire {
    operation_id: u64,
    kind: DurableOperationKind,
    request: Digest,
    transitions: Vec<(CheckpointId, OperationStateWire)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationStateWire {
    Prepared,
    Dispatched,
    DecisionRequired {
        request: Digest,
        dispatch_capability_minted: bool,
    },
    Evidence {
        state: EvidenceState,
        component: Digest,
    },
}

/// Operation states carrying exactly one component, by persisted tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum EvidenceState {
    Completed = 3,
    ProvenNotDispatched = 4,
    OutcomeUnknown = 5,
    Finalized = 6,
    LocatorBound = 7,
    Claimed = 8,
    ContinuationRequired = 10,
}

impl EvidenceState {
    fn of(state: &DurableOperationState) -> Option<(Self, &ReconstructiveComponent)> {
        Some(match state {
            DurableOperationState::Completed(component) => (Self::Completed, component),
            DurableOperationState::ProvenNotDispatched(component) => {
                (Self::ProvenNotDispatched, component)
            }
            DurableOperationState::OutcomeUnknown(component) => (Self::OutcomeUnknown, component),
            DurableOperationState::Finalized(component) => (Self::Finalized, component),
            DurableOperationState::LocatorBound(component) => (Self::LocatorBound, component),
            DurableOperationState::Claimed(component) => (Self::Claimed, component),
            DurableOperationState::ContinuationRequired(component) => {
                (Self::ContinuationRequired, component)
            }
            DurableOperationState::Prepared
            | DurableOperationState::Dispatched
            | DurableOperationState::DecisionRequired(_) => return None,
        })
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            3 => Self::Completed,
            4 => Self::ProvenNotDispatched,
            5 => Self::OutcomeUnknown,
            6 => Self::Finalized,
            7 => Self::LocatorBound,
            8 => Self::Claimed,
            10 => Self::ContinuationRequired,
            _ => return None,
        })
    }

    fn state(self, component: ReconstructiveComponent) -> DurableOperationState {
        match self {
            Self::Completed => DurableOperationState::Completed(component),
            Self::ProvenNotDispatched => DurableOperationState::ProvenNotDispatched(component),
            Self::OutcomeUnknown => DurableOperationState::OutcomeUnknown(component),
            Self::Finalized => DurableOperationState::Finalized(component),
            Self::LocatorBound => DurableOperationState::LocatorBound(component),
            Self::Claimed => DurableOperationState::Claimed(component),
            Self::ContinuationRequired => DurableOperationState::ContinuationRequired(component),
        }
    }
}

impl OperationRecordWire {
    /// Names every component of `record` through `reference`, which returns
    /// the component object's content digest, in persisted component order.
    pub(crate) fn new<E: From<RuntimeManifestError>>(
        record: &DurableOperationRecord,
        mut reference: impl FnMut(&ReconstructiveComponent) -> Result<Digest, E>,
    ) -> Result<Self, E> {
        let request = reference(&record.request)?;
        let mut transitions = Vec::with_capacity(record.transitions.len());
        for transition in &record.transitions {
            let state = match &transition.state {
                DurableOperationState::Prepared => OperationStateWire::Prepared,
                DurableOperationState::Dispatched => OperationStateWire::Dispatched,
                DurableOperationState::DecisionRequired(pending) => {
                    OperationStateWire::DecisionRequired {
                        request: reference(pending.request())?,
                        dispatch_capability_minted: pending.dispatch_capability_minted(),
                    }
                }
                other => {
                    let (state, component) =
                        EvidenceState::of(other).ok_or(RuntimeManifestError::InvalidLedger)?;
                    OperationStateWire::Evidence {
                        state,
                        component: reference(component)?,
                    }
                }
            };
            transitions.push((transition.checkpoint_id, state));
        }
        Ok(Self {
            operation_id: record.operation_id,
            kind: record.kind.clone(),
            request,
            transitions,
        })
    }

    pub(crate) const fn operation_id(&self) -> u64 {
        self.operation_id
    }

    /// Referenced component-object digests in persisted order: the request,
    /// then each component-carrying transition.
    pub(crate) fn component_digests(&self) -> Vec<Digest> {
        let mut digests = Vec::with_capacity(self.transitions.len().saturating_add(1));
        digests.push(self.request);
        for (_, state) in &self.transitions {
            match *state {
                OperationStateWire::Prepared | OperationStateWire::Dispatched => {}
                OperationStateWire::DecisionRequired { request, .. } => digests.push(request),
                OperationStateWire::Evidence { component, .. } => digests.push(component),
            }
        }
        digests
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, RuntimeManifestError> {
        let mut writer = Writer::default();
        writer.u64(self.operation_id);
        writer.operation_kind(&self.kind)?;
        writer.digest(self.request)?;
        writer.count(self.transitions.len())?;
        for (checkpoint_id, state) in &self.transitions {
            writer.id(checkpoint_id.as_uuid());
            match *state {
                OperationStateWire::Prepared => writer.u8(1),
                OperationStateWire::Dispatched => writer.u8(2),
                OperationStateWire::DecisionRequired {
                    request,
                    dispatch_capability_minted,
                } => {
                    writer.u8(9);
                    writer.digest(request)?;
                    writer.optional(dispatch_capability_minted);
                }
                OperationStateWire::Evidence { state, component } => {
                    writer.u8(state as u8);
                    writer.digest(component)?;
                }
            }
        }
        Ok(writer.bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, RuntimeManifestError> {
        let mut reader = Reader::new(bytes);
        let operation_id = reader.u64()?;
        let kind = reader.operation_kind()?;
        let request = reader.digest(Some(DigestDomain::Blob))?;
        let count = reader.count()?;
        if count == 0 || count > MAX_DURABLE_OPERATION_TRANSITIONS {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        let mut transitions = Vec::with_capacity(count);
        for _ in 0..count {
            let checkpoint_id = reader.checkpoint_id()?;
            let state = match reader.u8()? {
                1 => OperationStateWire::Prepared,
                2 => OperationStateWire::Dispatched,
                9 => {
                    let request = reader.digest(Some(DigestDomain::Blob))?;
                    if reader.optional()? {
                        return Err(RuntimeManifestError::InvalidLedger);
                    }
                    OperationStateWire::DecisionRequired {
                        request,
                        dispatch_capability_minted: false,
                    }
                }
                tag => OperationStateWire::Evidence {
                    state: EvidenceState::from_tag(tag).ok_or(
                        RuntimeManifestError::UnknownTag {
                            field: "operation state",
                            tag,
                        },
                    )?,
                    component: reader.digest(Some(DigestDomain::Blob))?,
                },
            };
            transitions.push((checkpoint_id, state));
        }
        reader.finish()?;
        Ok(Self {
            operation_id,
            kind,
            request,
            transitions,
        })
    }

    /// Rebuilds the exact record, resolving each referenced digest through
    /// `component`. Every resolved component must be operation evidence.
    pub(crate) fn resolve<E: From<RuntimeManifestError>>(
        self,
        mut component: impl FnMut(Digest) -> Result<ReconstructiveComponent, E>,
    ) -> Result<DurableOperationRecord, E> {
        // A digest repeated across transitions resolves once and shares its bytes.
        let mut resolved = BTreeMap::<[u8; 32], ReconstructiveComponent>::new();
        let mut operation = |digest: Digest| {
            if let Some(known) = resolved.get(digest.bytes()) {
                return Ok(known.clone());
            }
            let fresh = component(digest)?;
            if fresh.kind() != RuntimeComponentKind::Operation {
                return Err(E::from(RuntimeManifestError::ComponentKind));
            }
            resolved.insert(*digest.bytes(), fresh.clone());
            Ok(fresh)
        };
        let request = operation(self.request)?;
        let mut transitions = Vec::with_capacity(self.transitions.len());
        for (checkpoint_id, state) in self.transitions {
            let state = match state {
                OperationStateWire::Prepared => DurableOperationState::Prepared,
                OperationStateWire::Dispatched => DurableOperationState::Dispatched,
                OperationStateWire::DecisionRequired { request, .. } => {
                    DurableOperationState::DecisionRequired(PendingEffectDecisionState::new(
                        operation(request)?,
                    )?)
                }
                OperationStateWire::Evidence { state, component } => {
                    state.state(operation(component)?)
                }
            };
            transitions.push(DurableOperationTransition::new(checkpoint_id, state));
        }
        Ok(DurableOperationRecord::new(
            self.operation_id,
            self.kind,
            request,
            transitions,
        )?)
    }
}

pub(super) fn encode_envelope(
    magic: [u8; 8],
    version: u32,
    body: &[u8],
    integrity: Digest,
    maximum: usize,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let body_len = u32::try_from(body.len()).map_err(|_| RuntimeManifestError::EnvelopeSize)?;
    let mut writer = Writer::default();
    writer.bytes.extend_from_slice(&magic);
    writer.u32(version);
    writer.u32(body_len);
    writer.bytes.extend_from_slice(body);
    writer.digest(integrity)?;
    if writer.bytes.len() > maximum {
        return Err(RuntimeManifestError::EnvelopeSize);
    }
    Ok(writer.bytes)
}

pub(super) fn decode_envelope<'a>(
    bytes: &'a [u8],
    magic: [u8; 8],
    version: u32,
    maximum: usize,
    integrity_tag: &[u8],
) -> Result<&'a [u8], RuntimeManifestError> {
    if bytes.len() > maximum {
        return Err(RuntimeManifestError::EnvelopeSize);
    }
    let mut reader = Reader::new(bytes);
    if reader.take(magic.len())? != magic {
        return Err(RuntimeManifestError::InvalidMagic);
    }
    let found = reader.u32()?;
    if found != version {
        return Err(RuntimeManifestError::UnsupportedVersion { found });
    }
    let body_len = reader.length(maximum)?;
    let body = reader.take(body_len)?;
    let integrity = reader.digest(Some(DigestDomain::Blob))?;
    reader.finish()?;
    if root_integrity(integrity_tag, body)? != integrity {
        return Err(RuntimeManifestError::IntegrityMismatch);
    }
    Ok(body)
}

fn decode_stable_prefix(
    bytes: &[u8],
    identity: Digest,
) -> Result<StableRequestPrefix, RuntimeManifestError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| RuntimeManifestError::InvalidModelComponent)?;
    let object = value
        .as_object_mut()
        .ok_or(RuntimeManifestError::InvalidModelComponent)?;
    if object
        .insert(
            "identity".to_owned(),
            serde_json::to_value(identity)
                .map_err(|_| RuntimeManifestError::InvalidModelComponent)?,
        )
        .is_some()
    {
        return Err(RuntimeManifestError::InvalidModelComponent);
    }
    let prefix: StableRequestPrefix =
        serde_json::from_value(value).map_err(|_| RuntimeManifestError::InvalidModelComponent)?;
    if prefix.identity() != identity || prefix.canonical_bytes() != bytes {
        return Err(RuntimeManifestError::IntegrityMismatch);
    }
    Ok(prefix)
}

fn decode_history_segment(
    bytes: &[u8],
    identity: Digest,
) -> Result<HistorySegment, RuntimeManifestError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| RuntimeManifestError::InvalidModelComponent)?;
    let object = value
        .as_object_mut()
        .ok_or(RuntimeManifestError::InvalidModelComponent)?;
    if object
        .insert(
            "identity".to_owned(),
            serde_json::to_value(identity)
                .map_err(|_| RuntimeManifestError::InvalidModelComponent)?,
        )
        .is_some()
    {
        return Err(RuntimeManifestError::InvalidModelComponent);
    }
    let segment: HistorySegment =
        serde_json::from_value(value).map_err(|_| RuntimeManifestError::InvalidModelComponent)?;
    if segment.identity() != identity || segment.canonical_bytes() != bytes {
        return Err(RuntimeManifestError::IntegrityMismatch);
    }
    Ok(segment)
}

pub(super) fn obligation_kind_tag(kind: RecoveryObligationKind) -> u8 {
    match kind {
        RecoveryObligationKind::Command => 1,
        RecoveryObligationKind::ProviderAttempt => 2,
        RecoveryObligationKind::EffectDecision => 3,
        RecoveryObligationKind::Effect => 4,
        RecoveryObligationKind::Commit => 5,
        RecoveryObligationKind::Compaction => 6,
        RecoveryObligationKind::ProcessSession => 7,
        RecoveryObligationKind::CodeCell => 8,
        RecoveryObligationKind::HostedContinuation => 9,
    }
}
pub(super) fn obligation_kind(tag: u8) -> Result<RecoveryObligationKind, RuntimeManifestError> {
    match tag {
        1 => Ok(RecoveryObligationKind::Command),
        2 => Ok(RecoveryObligationKind::ProviderAttempt),
        3 => Ok(RecoveryObligationKind::EffectDecision),
        4 => Ok(RecoveryObligationKind::Effect),
        5 => Ok(RecoveryObligationKind::Commit),
        6 => Ok(RecoveryObligationKind::Compaction),
        7 => Ok(RecoveryObligationKind::ProcessSession),
        8 => Ok(RecoveryObligationKind::CodeCell),
        9 => Ok(RecoveryObligationKind::HostedContinuation),
        _ => Err(RuntimeManifestError::UnknownTag {
            field: "recovery obligation",
            tag,
        }),
    }
}
pub(super) fn recovery_action_tag(action: RecoveryAction) -> u8 {
    match action {
        RecoveryAction::CommitRecovery => 1,
        RecoveryAction::ProviderEvidenceRequired => 2,
        RecoveryAction::EffectEvidenceRequired => 3,
        RecoveryAction::ProvenNotDispatched => 4,
        RecoveryAction::FinalizationRequired => 5,
        RecoveryAction::RestoreEffectDecision => 6,
        RecoveryAction::EvaluationAbandonmentRequired => 7,
        RecoveryAction::RestoreToolBatch => 8,
        RecoveryAction::RestoreServerPause => 9,
    }
}
pub(super) fn recovery_action(tag: u8) -> Result<RecoveryAction, RuntimeManifestError> {
    match tag {
        1 => Ok(RecoveryAction::CommitRecovery),
        2 => Ok(RecoveryAction::ProviderEvidenceRequired),
        3 => Ok(RecoveryAction::EffectEvidenceRequired),
        4 => Ok(RecoveryAction::ProvenNotDispatched),
        5 => Ok(RecoveryAction::FinalizationRequired),
        6 => Ok(RecoveryAction::RestoreEffectDecision),
        7 => Ok(RecoveryAction::EvaluationAbandonmentRequired),
        8 => Ok(RecoveryAction::RestoreToolBatch),
        9 => Ok(RecoveryAction::RestoreServerPause),
        _ => Err(RuntimeManifestError::UnknownTag {
            field: "recovery action",
            tag,
        }),
    }
}
pub(super) fn recovery_phase_tag(phase: WorkingRecoveryPhase) -> u8 {
    match phase {
        WorkingRecoveryPhase::DispatchEvidence => 1,
        WorkingRecoveryPhase::EffectReconciliation => 2,
        WorkingRecoveryPhase::CommitRecovery => 3,
        WorkingRecoveryPhase::Finalization => 4,
        WorkingRecoveryPhase::EffectDecision => 5,
        WorkingRecoveryPhase::HostedContinuation => 6,
    }
}
pub(super) fn recovery_phase(tag: u8) -> Result<WorkingRecoveryPhase, RuntimeManifestError> {
    match tag {
        1 => Ok(WorkingRecoveryPhase::DispatchEvidence),
        2 => Ok(WorkingRecoveryPhase::EffectReconciliation),
        3 => Ok(WorkingRecoveryPhase::CommitRecovery),
        4 => Ok(WorkingRecoveryPhase::Finalization),
        5 => Ok(WorkingRecoveryPhase::EffectDecision),
        6 => Ok(WorkingRecoveryPhase::HostedContinuation),
        _ => Err(RuntimeManifestError::UnknownTag {
            field: "working recovery phase",
            tag,
        }),
    }
}

fn digest_domain_tag(domain: DigestDomain) -> Result<u8, RuntimeManifestError> {
    Ok(match domain {
        DigestDomain::ReplayEnvelope => 1,
        DigestDomain::RequestPlan => 2,
        DigestDomain::ModelBinding => 3,
        DigestDomain::ToolDefinition => 4,
        DigestDomain::Blob => 5,
        DigestDomain::Snapshot => 6,
        DigestDomain::EffectAuthorization => 7,
        DigestDomain::ExecutionEnvironment => 8,
        DigestDomain::EffectReconciliation => 9,
        DigestDomain::EffectReconciliationCarrier => 10,
        DigestDomain::ProviderOperation => 11,
        DigestDomain::HarnessProfileSpec => 12,
        DigestDomain::ResolvedHarnessPlan => 13,
        DigestDomain::HarnessStrategyCatalog => 14,
        DigestDomain::HarnessPlanComponentManifest => 15,
        DigestDomain::CanonicalHistorySegment => 16,
        DigestDomain::CanonicalHistory => 17,
        DigestDomain::HistoryProjection => 18,
        DigestDomain::StableRequestPrefix => 19,
        DigestDomain::TokenMeasurement => 20,
        DigestDomain::ContextBudget => 21,
        DigestDomain::ContextAdmission => 22,
        DigestDomain::CanonicalItemPrefix => 23,
        DigestDomain::ContextPlan => 24,
        DigestDomain::CacheAffinity => 25,
        _ => return Err(RuntimeManifestError::InvalidDigest),
    })
}
fn digest_domain(tag: u8) -> Result<DigestDomain, RuntimeManifestError> {
    match tag {
        1 => Ok(DigestDomain::ReplayEnvelope),
        2 => Ok(DigestDomain::RequestPlan),
        3 => Ok(DigestDomain::ModelBinding),
        4 => Ok(DigestDomain::ToolDefinition),
        5 => Ok(DigestDomain::Blob),
        6 => Ok(DigestDomain::Snapshot),
        7 => Ok(DigestDomain::EffectAuthorization),
        8 => Ok(DigestDomain::ExecutionEnvironment),
        9 => Ok(DigestDomain::EffectReconciliation),
        10 => Ok(DigestDomain::EffectReconciliationCarrier),
        11 => Ok(DigestDomain::ProviderOperation),
        12 => Ok(DigestDomain::HarnessProfileSpec),
        13 => Ok(DigestDomain::ResolvedHarnessPlan),
        14 => Ok(DigestDomain::HarnessStrategyCatalog),
        15 => Ok(DigestDomain::HarnessPlanComponentManifest),
        16 => Ok(DigestDomain::CanonicalHistorySegment),
        17 => Ok(DigestDomain::CanonicalHistory),
        18 => Ok(DigestDomain::HistoryProjection),
        19 => Ok(DigestDomain::StableRequestPrefix),
        20 => Ok(DigestDomain::TokenMeasurement),
        21 => Ok(DigestDomain::ContextBudget),
        22 => Ok(DigestDomain::ContextAdmission),
        23 => Ok(DigestDomain::CanonicalItemPrefix),
        24 => Ok(DigestDomain::ContextPlan),
        25 => Ok(DigestDomain::CacheAffinity),
        _ => Err(RuntimeManifestError::UnknownTag {
            field: "digest domain",
            tag,
        }),
    }
}
