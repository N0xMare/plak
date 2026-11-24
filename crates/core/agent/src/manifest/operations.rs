use super::{
    BTreeMap, BTreeSet, CheckpointId, CompositionContinuationRequest, CompositionEvaluationRequest,
    Digest, DigestDomain, DurableCommandDisposition, DurableCommandRecord, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition,
    MAX_DURABLE_OPERATION_TRANSITIONS, MAX_ID_BYTES, NonZeroU64, OperationStateClass,
    PROCESS_SESSION_ID_BYTES, PendingEffectDecisionState, ProcessSessionId,
    ProcessSessionOwnerGeneration, ReconstructiveComponent, RecoveryObligationKind,
    RuntimeComponentKind, RuntimeManifestError, RuntimeOperationLedger, RuntimeOperationRoot,
    RuntimeWorkingRoot, TurnId, composition_child_call_id, decode_exact_composition_request,
    expect_component, fmt, portable_id, validate_operation_transitions,
};

impl DurableCommandRecord {
    /// Creates one exact command/idempotency record.
    pub fn new(
        command_id: impl Into<String>,
        sequence: u64,
        turn_id: Option<TurnId>,
        binding: Digest,
        command: ReconstructiveComponent,
        disposition: DurableCommandDisposition,
        rejection_code: Option<String>,
    ) -> Result<Self, RuntimeManifestError> {
        if sequence == 0 {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        expect_component(&command, RuntimeComponentKind::Command)?;
        let rejection_code = rejection_code
            .map(|code| portable_id(code, "command rejection code"))
            .transpose()?;
        if matches!(disposition, DurableCommandDisposition::Accepted) != rejection_code.is_none() {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        Ok(Self {
            command_id: portable_id(command_id.into(), "command identity")?,
            sequence,
            turn_id,
            binding,
            command,
            disposition,
            rejection_code,
        })
    }

    /// Returns the stable command identity.
    #[must_use]
    pub fn command_id(&self) -> &str {
        &self.command_id
    }

    /// Returns the deterministic driver sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the bound root turn, when this command has one.
    #[must_use]
    pub const fn turn_id(&self) -> Option<TurnId> {
        self.turn_id
    }

    /// Returns the exact canonical command binding.
    #[must_use]
    pub const fn binding(&self) -> Digest {
        self.binding
    }

    /// Returns the durable command disposition.
    #[must_use]
    pub const fn disposition(&self) -> DurableCommandDisposition {
        self.disposition
    }

    /// Returns the stable rejection code, when rejected.
    #[must_use]
    pub fn rejection_code(&self) -> Option<&str> {
        self.rejection_code.as_deref()
    }

    pub(crate) const fn command_component(&self) -> &ReconstructiveComponent {
        &self.command
    }
}

impl ProcessSessionId {
    /// Creates one nonzero opaque identity.
    pub fn new(bytes: [u8; PROCESS_SESSION_ID_BYTES]) -> Result<Self, RuntimeManifestError> {
        if bytes == [0; PROCESS_SESSION_ID_BYTES] {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        Ok(Self(bytes))
    }

    /// Returns the exact canonical bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; PROCESS_SESSION_ID_BYTES] {
        self.0
    }
}

impl fmt::Debug for ProcessSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProcessSessionId(<opaque>)")
    }
}

impl ProcessSessionOwnerGeneration {
    /// Creates one nonzero owner generation.
    pub fn new(value: u64) -> Result<Self, RuntimeManifestError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(RuntimeManifestError::InvalidLedger)
    }

    /// Returns the exact canonical generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl RuntimeWorkingRoot {
    /// Returns the stable root independent of mutable round progress.
    #[must_use]
    pub const fn operation_root(self) -> RuntimeOperationRoot {
        match self {
            Self::Turn { turn_id, .. } => RuntimeOperationRoot::Turn(turn_id),
            Self::SessionWorkflow(workflow_id) => {
                RuntimeOperationRoot::SessionWorkflow(workflow_id)
            }
        }
    }

    /// Returns next-round progress only for an ordinary turn root.
    #[must_use]
    pub const fn next_round(self) -> Option<u32> {
        match self {
            Self::Turn { next_round, .. } => Some(next_round),
            Self::SessionWorkflow(_) => None,
        }
    }
}

impl DurableOperationKind {
    pub(super) fn operation_root(&self) -> Option<RuntimeOperationRoot> {
        match self {
            Self::Command { turn_id } => turn_id.map(RuntimeOperationRoot::Turn),
            Self::Provider { root, .. } | Self::Commit { root } => Some(*root),
            Self::EffectDecision { turn_id, .. }
            | Self::Effect { turn_id, .. }
            | Self::ToolBatchContinuation { turn_id, .. }
            | Self::ServerPauseContinuation { turn_id, .. }
            | Self::CompositionEvaluation { turn_id, .. }
            | Self::CompositionContinuation { turn_id, .. }
            | Self::CompositionChild { turn_id, .. }
            | Self::ProcessSession { turn_id, .. } => Some(RuntimeOperationRoot::Turn(*turn_id)),
            Self::Compaction { workflow_id } => {
                Some(RuntimeOperationRoot::SessionWorkflow(*workflow_id))
            }
        }
    }

    pub(super) fn obligation_kind(&self) -> RecoveryObligationKind {
        match self {
            Self::Command { .. } => RecoveryObligationKind::Command,
            Self::Provider { .. } => RecoveryObligationKind::ProviderAttempt,
            Self::EffectDecision { .. } => RecoveryObligationKind::EffectDecision,
            Self::Effect { .. } => RecoveryObligationKind::Effect,
            Self::ToolBatchContinuation { .. } | Self::ServerPauseContinuation { .. } => {
                RecoveryObligationKind::HostedContinuation
            }
            Self::CompositionEvaluation { .. }
            | Self::CompositionContinuation { .. }
            | Self::CompositionChild { .. } => RecoveryObligationKind::CodeCell,
            Self::ProcessSession { .. } => RecoveryObligationKind::ProcessSession,
            Self::Commit { .. } => RecoveryObligationKind::Commit,
            Self::Compaction { .. } => RecoveryObligationKind::Compaction,
        }
    }
}

impl DurableOperationState {
    pub(super) fn class(&self) -> OperationStateClass {
        match self {
            Self::Prepared => OperationStateClass::Prepared,
            Self::DecisionRequired(_) => OperationStateClass::DecisionRequired,
            Self::ContinuationRequired(_) => OperationStateClass::ContinuationRequired,
            Self::Claimed(_) => OperationStateClass::Claimed,
            Self::Dispatched => OperationStateClass::Dispatched,
            Self::LocatorBound(_) => OperationStateClass::LocatorBound,
            Self::Completed(_) => OperationStateClass::Completed,
            Self::ProvenNotDispatched(_) => OperationStateClass::ProvenNotDispatched,
            Self::OutcomeUnknown(_) => OperationStateClass::OutcomeUnknown,
            Self::Finalized(_) => OperationStateClass::Finalized,
        }
    }
}

impl PendingEffectDecisionState {
    /// Captures one exact request while proving that no dispatch capability exists.
    pub fn new(request: ReconstructiveComponent) -> Result<Self, RuntimeManifestError> {
        if request.kind() != RuntimeComponentKind::Operation {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        Ok(Self {
            request,
            dispatch_capability_minted: false,
        })
    }

    /// Returns the complete bounded authorization request.
    #[must_use]
    pub const fn request(&self) -> &ReconstructiveComponent {
        &self.request
    }

    /// Reports the durable negative dispatch-capability fence.
    #[must_use]
    pub const fn dispatch_capability_minted(&self) -> bool {
        self.dispatch_capability_minted
    }
}

impl DurableOperationTransition {
    /// Creates an exact operation transition.
    #[must_use]
    pub const fn new(checkpoint_id: CheckpointId, state: DurableOperationState) -> Self {
        Self {
            checkpoint_id,
            state,
        }
    }

    /// Returns the committed state.
    #[must_use]
    pub const fn state(&self) -> &DurableOperationState {
        &self.state
    }

    pub(crate) const fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint_id
    }
}

impl DurableOperationRecord {
    /// Creates and validates one closed operation history.
    pub fn new(
        operation_id: u64,
        kind: DurableOperationKind,
        request: ReconstructiveComponent,
        transitions: Vec<DurableOperationTransition>,
    ) -> Result<Self, RuntimeManifestError> {
        if operation_id == 0
            || transitions.is_empty()
            || transitions.len() > MAX_DURABLE_OPERATION_TRANSITIONS
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        expect_component(&request, RuntimeComponentKind::Operation)?;
        validate_operation_kind(&kind)?;
        validate_operation_transitions(&kind, &transitions)?;
        if transitions.iter().any(|transition| {
            matches!(
                &transition.state,
                DurableOperationState::DecisionRequired(pending)
                    if pending.request() != &request
            )
        }) {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        if transitions
            .iter()
            .any(|transition| matches!(transition.state, DurableOperationState::LocatorBound(_)))
            && !matches!(
                &kind,
                DurableOperationKind::Provider { .. }
                    | DurableOperationKind::Compaction { .. }
                    | DurableOperationKind::CompositionEvaluation { .. }
            )
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        Ok(Self {
            operation_id,
            kind,
            request,
            transitions,
        })
    }

    /// Returns a new record with several transitions appended, validated once.
    pub fn append_transitions(
        &self,
        appended: impl IntoIterator<Item = DurableOperationTransition>,
    ) -> Result<Self, RuntimeManifestError> {
        let mut transitions = self.transitions.clone();
        transitions.extend(appended);
        Self::new(
            self.operation_id,
            self.kind.clone(),
            self.request.clone(),
            transitions,
        )
    }

    /// Returns a new record with one validated transition appended.
    pub fn append_transition(
        &self,
        transition: DurableOperationTransition,
    ) -> Result<Self, RuntimeManifestError> {
        let mut transitions = self.transitions.clone();
        transitions.push(transition);
        Self::new(
            self.operation_id,
            self.kind.clone(),
            self.request.clone(),
            transitions,
        )
    }

    /// Returns the exact operation identity.
    #[must_use]
    pub const fn operation_id(&self) -> u64 {
        self.operation_id
    }

    /// Returns the operation kind.
    #[must_use]
    pub const fn kind(&self) -> &DurableOperationKind {
        &self.kind
    }

    /// Returns its current state.
    #[must_use]
    #[allow(
        clippy::expect_used,
        clippy::missing_panics_doc,
        reason = "construction proves the private transition vector is nonempty"
    )]
    pub fn state(&self) -> &DurableOperationState {
        &self
            .transitions
            .last()
            .expect("validated operation transitions are nonempty")
            .state
    }

    /// Returns the latest persisted locator component, if any.
    ///
    /// Reverse traversal makes a post-dispatch locator supersede an optional
    /// prepared locator without retaining a competing authority field.
    #[must_use]
    pub fn latest_locator(&self) -> Option<&ReconstructiveComponent> {
        self.transitions
            .iter()
            .rev()
            .find_map(|transition| match &transition.state {
                DurableOperationState::LocatorBound(locator) => Some(locator),
                _ => None,
            })
    }

    /// Returns the exact canonical request identity without exposing request bytes.
    #[must_use]
    pub const fn request_identity(&self) -> Digest {
        self.request.integrity()
    }

    pub(crate) const fn request(&self) -> &ReconstructiveComponent {
        &self.request
    }

    pub(crate) fn transitions(&self) -> &[DurableOperationTransition] {
        &self.transitions
    }
}

fn validate_operation_kind(kind: &DurableOperationKind) -> Result<(), RuntimeManifestError> {
    if let DurableOperationKind::CompositionChild {
        evaluation_operation_id,
        interpreter_generation,
        cell_id,
        request_sequence,
        ..
    } = kind
        && (*evaluation_operation_id == 0
            || *interpreter_generation == 0
            || cell_id.is_empty()
            || cell_id.len() > MAX_ID_BYTES
            || *request_sequence == 0)
    {
        return Err(RuntimeManifestError::InvalidLedger);
    }
    Ok(())
}

pub(super) fn validate_composition_graph(
    ledger: &RuntimeOperationLedger,
) -> Result<(), RuntimeManifestError> {
    struct Evaluation {
        turn_id: TurnId,
        descriptor_identity: Digest,
        host_identity: Vec<u8>,
    }

    let operations = ledger.operations();
    let mut evaluations = BTreeMap::new();
    for record in &operations {
        let DurableOperationKind::CompositionEvaluation {
            turn_id, call_id, ..
        } = record.kind()
        else {
            continue;
        };
        let request: CompositionEvaluationRequest = decode_exact_composition_request(
            record.request().bytes(),
            "plakous-local-composition-evaluation-v2",
        )?;
        if request.version != 2 || request.call.call_id() != *call_id {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        evaluations.insert(
            record.operation_id(),
            Evaluation {
                turn_id: *turn_id,
                descriptor_identity: request.descriptor_identity,
                host_identity: request.host_identity,
            },
        );
    }

    let mut evaluation_attempts = evaluations
        .keys()
        .map(|operation_id| (*operation_id, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for record in &operations {
        match record.kind() {
            DurableOperationKind::CompositionEvaluation { attempt_id, .. } => {
                evaluation_attempts
                    .get_mut(&record.operation_id())
                    .ok_or(RuntimeManifestError::InvalidLedger)?
                    .insert(*attempt_id);
            }
            DurableOperationKind::CompositionContinuation {
                attempt_id,
                evaluation_operation_id,
                ..
            } if evaluations.contains_key(evaluation_operation_id) => {
                evaluation_attempts
                    .get_mut(evaluation_operation_id)
                    .ok_or(RuntimeManifestError::InvalidLedger)?
                    .insert(*attempt_id);
            }
            _ => {}
        }
    }

    let mut child_lineage = BTreeMap::<u64, (u64, String, BTreeSet<u32>)>::new();
    for record in operations {
        match record.kind() {
            DurableOperationKind::CompositionContinuation {
                turn_id,
                call_id,
                evaluation_operation_id,
                ..
            } => {
                let parent = evaluations
                    .get(evaluation_operation_id)
                    .ok_or(RuntimeManifestError::InvalidLedger)?;
                let request: CompositionContinuationRequest = decode_exact_composition_request(
                    record.request().bytes(),
                    "plakous-local-composition-continuation-v1",
                )?;
                if request.version != 1
                    || request.evaluation_operation_id != *evaluation_operation_id
                    || request.call.call_id() != *call_id
                    || parent.turn_id != *turn_id
                    || request.descriptor_identity != parent.descriptor_identity
                    || request.host_identity != parent.host_identity
                {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
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
                let parent = evaluations
                    .get(evaluation_operation_id)
                    .ok_or(RuntimeManifestError::InvalidLedger)?;
                let edge: (String, u64, u64, String, u32, Digest, Vec<u8>) =
                    serde_json::from_slice(record.request().bytes())
                        .map_err(|_| RuntimeManifestError::InvalidLedger)?;
                let canonical = serde_json::to_vec(&(
                    edge.0.as_str(),
                    edge.1,
                    edge.2,
                    edge.3.as_str(),
                    edge.4,
                    edge.5,
                    &edge.6,
                ))
                .map_err(|_| RuntimeManifestError::InvalidLedger)?;
                let request_digest = Digest::sha256(
                    DigestDomain::ProviderOperation,
                    3,
                    &[b"plakous-agent/local-composition-child-request-v1", &edge.6],
                )
                .map_err(|_| RuntimeManifestError::InvalidLedger)?;
                let binding = Digest::sha256(
                    DigestDomain::ProviderOperation,
                    4,
                    &[b"plakous-agent/local-composition-child-edge-v2", &canonical],
                )
                .map_err(|_| RuntimeManifestError::InvalidLedger)?;
                let nested: (String, String, String, String, Option<serde_json::Value>) =
                    serde_json::from_slice(&edge.6)
                        .map_err(|_| RuntimeManifestError::InvalidLedger)?;
                if canonical != record.request().bytes()
                    || edge.0 != "plakous-local-composition-child-edge-v2"
                    || edge.1 != *evaluation_operation_id
                    || edge.2 != *interpreter_generation
                    || edge.3 != *cell_id
                    || edge.4 != *request_sequence
                    || edge.5 != request_digest
                    || nested.0 != "plakous-local-composition-child-request-v1"
                    || nested.1 != *cell_id
                    || serde_json::to_vec(&(
                        nested.0.as_str(),
                        nested.1.as_str(),
                        nested.2.as_str(),
                        nested.3.as_str(),
                        &nested.4,
                    ))
                    .map_err(|_| RuntimeManifestError::InvalidLedger)?
                        != edge.6
                    || composition_child_call_id(binding)? != *call_id
                    || parent.turn_id != *turn_id
                    || !evaluation_attempts
                        .get(evaluation_operation_id)
                        .is_some_and(|attempts| attempts.contains(attempt_id))
                {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
                let lineage = child_lineage
                    .entry(*evaluation_operation_id)
                    .or_insert_with(|| (*interpreter_generation, cell_id.clone(), BTreeSet::new()));
                if lineage.0 != *interpreter_generation
                    || lineage.1 != *cell_id
                    || !lineage.2.insert(*request_sequence)
                {
                    return Err(RuntimeManifestError::InvalidLedger);
                }
            }
            _ => {}
        }
    }
    if child_lineage.values().any(|(_, _, sequences)| {
        sequences
            .iter()
            .copied()
            .ne(1..=u32::try_from(sequences.len()).unwrap_or(u32::MAX))
    }) {
        return Err(RuntimeManifestError::InvalidLedger);
    }
    Ok(())
}
