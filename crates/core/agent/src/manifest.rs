//! Reconstruction-complete runtime manifests and immutable checkpoints.
//!
//! The content-addressed closed object graph is the sole persisted checkpoint
//! codec. This module owns the semantic values and the small canonical value
//! encodings used by that graph. Exact bytes from versioned component codecs
//! remain mandatory reconstruction data; digests never substitute for them.

#![allow(
    clippy::missing_errors_doc,
    reason = "the closed error enum documents shared failures"
)]

mod codec;
mod jobs;
mod ledger;
pub(crate) use jobs::MAX_JOB_ROWS;
pub use jobs::{JobLedger, JobRow, JobTerminal, JobTerminalStatus};
pub(crate) use ledger::OperationLedgerMutationRef;
pub use ledger::RuntimeOperationLedger;
#[allow(
    clippy::wildcard_imports,
    reason = "the ledger split shares the manifest vocabulary"
)]
use ledger::*;

pub(crate) use codec::{OperationRecordWire, RuntimeLifecycleCodec};
use codec::{Reader, Writer, decode_envelope, encode_envelope};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

#[cfg(test)]
use plakous_model::ContextHistoryPlan;
use plakous_model::{
    ActiveFamilyArtifact, AttemptId, BranchId, CallId, CanonicalHistory, CheckpointId,
    CompiledProviderConfiguration, ContextAnchor, ConversationView, Digest, DigestAlgorithm,
    DigestDomain, EntryId, HistorySegment, HistorySegmentKind, LocalDurabilityDisposition,
    ModelAdapterProfileId, ModelCacheAffinity, ModelFamilyId, ModelId, ModelOperationRoot,
    ProtocolId, ReasoningEffortDisposition, ReasoningPolicy, RouteId,
    SealedFamilyArtifactCandidate, SemanticCompactionCut, SessionId, SessionWorkflowRootId,
    StableRequestPrefix, Timestamp, ToolCallBlock, TurnId, Usage, WorkingProjection,
};
use plakous_tools::{
    TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION, TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION, TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
};
use thiserror::Error;

use crate::{HARNESS_PLAN_COMPONENT_FORMAT_VERSION, RUNTIME_POLICY_REVISION};
use crate::{HarnessPlanReadiness, ResolvedHarnessPlan, rehydrate_harness_plan};

/// Canonical binary revision for [`RuntimeContextState`].
pub(crate) const RUNTIME_CONTEXT_STATE_CODEC_REVISION: u32 = 3;
/// Canonical binary revision for [`RuntimeLifecycle`].
const RUNTIME_LIFECYCLE_CODEC_REVISION: u32 = 3;
/// Canonical JSON revision for reconstruction-complete provider configuration.
pub const RUNTIME_PROVIDER_CONFIGURATION_CODEC_REVISION: u32 = 4;
/// Responses-family artifact schema revision supported by this runtime.
pub const RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION: u32 = 7;
/// Responses-family artifact codec revision supported by this runtime.
pub const RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION: u32 = 6;
/// Messages-family artifact schema revision supported by this runtime.
pub const MESSAGES_FAMILY_ARTIFACT_SCHEMA_REVISION: u32 = 1;
/// Messages-family artifact codec revision supported by this runtime.
pub const MESSAGES_FAMILY_ARTIFACT_CODEC_REVISION: u32 = 1;
/// Maximum encoded bytes for one complete runtime manifest.
pub(crate) const MAX_RUNTIME_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded bytes for one complete runtime checkpoint.
pub(crate) const MAX_RUNTIME_CHECKPOINT_BYTES: usize = MAX_RUNTIME_MANIFEST_BYTES + 1024;
/// Maximum encoded bytes for one lifecycle and its bounded obligations.
const MAX_RUNTIME_LIFECYCLE_BYTES: usize = 128 * 1024;
/// Maximum encoded bytes for one complete typed context state.
pub(crate) const MAX_RUNTIME_CONTEXT_STATE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum bytes in a non-operation reconstruction component.
pub(crate) const MAX_RUNTIME_COMPONENT_BYTES: usize = 4 * 1024 * 1024;
// Operation payloads retain complete requests and outcomes, not just metadata.
// This resource budget does not change their encoding or integrity identity.
pub(crate) const MAX_RUNTIME_OPERATION_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
/// History-segment object payload. Messages thinking+signature replay is stored
/// in the segment body and can exceed a reconstructive-component bound.
const MAX_CHECKPOINT_HISTORY_SEGMENT_BYTES: usize = MAX_RUNTIME_CHECKPOINT_BYTES;
/// Maximum retained commands, operations, history segments, or obligations.
pub(crate) const MAX_RUNTIME_RECORDS: usize = 4_096;
/// Exact byte width of one runtime-minted durable process-session identity.
pub const PROCESS_SESSION_ID_BYTES: usize = 16;

// Prepared/post-dispatch locators may each be checkpointed before terminal
// evidence and finalization. The exact closed path remains statically bounded.
const MAX_DURABLE_OPERATION_TRANSITIONS: usize = 8;

const LIFECYCLE_MAGIC: [u8; 8] = *b"PLAKRLC\0";
const CONTEXT_STATE_MAGIC: [u8; 8] = *b"PLAKRCS\0";
pub(crate) const PROVIDER_CONFIGURATION_MAGIC: [u8; 8] = *b"PLAKPCF\0";
const MAX_ID_BYTES: usize = 128;
const MAX_PROVENANCE_BYTES: usize = 256;
const INTEGRITY_REVISION: u32 = 1;

/// Non-authoritative producing-build provenance.
#[derive(Clone, Eq, PartialEq)]
pub struct ProducerProvenance {
    producer: String,
    build: String,
    features: String,
}

impl ProducerProvenance {
    /// Creates bounded diagnostic provenance. These fields never gate reopen.
    pub fn new(
        producer: impl Into<String>,
        build: impl Into<String>,
        features: impl Into<String>,
    ) -> Result<Self, RuntimeManifestError> {
        let producer = bounded_text(producer.into(), MAX_PROVENANCE_BYTES, "producer")?;
        let build = bounded_text(build.into(), MAX_PROVENANCE_BYTES, "build")?;
        let features = bounded_text(features.into(), MAX_PROVENANCE_BYTES, "features")?;
        Ok(Self {
            producer,
            build,
            features,
        })
    }

    /// Returns the producing component identity.
    #[must_use]
    pub fn producer(&self) -> &str {
        &self.producer
    }

    /// Returns the diagnostic build identity.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }

    /// Returns the diagnostic feature-closure identity.
    #[must_use]
    pub fn features(&self) -> &str {
        &self.features
    }
}

impl fmt::Debug for ProducerProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProducerProvenance")
            .field("producer", &self.producer)
            .field("build", &self.build)
            .field("features", &self.features)
            .finish()
    }
}

/// Closed meaning of an embedded canonical component.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum RuntimeComponentKind {
    /// Exact harness profile/compiler inputs and resolved plan.
    HarnessPlan = 1,
    /// Exact context measurement, accounting, and compaction state.
    ContextState = 2,
    /// Exact model descriptor and effective capability facts.
    ModelDescriptor = 3,
    /// Exact non-secret admission/revalidation evidence.
    AdmissionEvidence = 4,
    /// Exact profile-binding facts.
    ProfileBinding = 5,
    /// Frozen canonical tool registry, schemas, and executor references.
    ToolRegistry = 6,
    /// Frozen canonical-to-provider tool exposure.
    ToolExposure = 7,
    /// Bound execution-environment facts and revalidation reference.
    ExecutionEnvironment = 8,
    /// Tool policy/effect protocol facts.
    EffectPolicy = 9,
    /// Exact canonical command input.
    Command = 10,
    /// Exact operation request, outcome, or reconciliation evidence.
    Operation = 11,
    /// Exact binding of a family-native artifact.
    FamilyBinding = 12,
}

impl RuntimeComponentKind {
    const fn max_bytes(self) -> usize {
        match self {
            Self::Operation => MAX_RUNTIME_OPERATION_COMPONENT_BYTES,
            _ => MAX_RUNTIME_COMPONENT_BYTES,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, RuntimeManifestError> {
        match tag {
            1 => Ok(Self::HarnessPlan),
            2 => Ok(Self::ContextState),
            3 => Ok(Self::ModelDescriptor),
            4 => Ok(Self::AdmissionEvidence),
            5 => Ok(Self::ProfileBinding),
            6 => Ok(Self::ToolRegistry),
            7 => Ok(Self::ToolExposure),
            8 => Ok(Self::ExecutionEnvironment),
            9 => Ok(Self::EffectPolicy),
            10 => Ok(Self::Command),
            11 => Ok(Self::Operation),
            12 => Ok(Self::FamilyBinding),
            _ => Err(RuntimeManifestError::UnknownTag {
                field: "runtime component",
                tag,
            }),
        }
    }
}

/// Exact bounded bytes from one named, revisioned canonical sub-codec.
#[derive(Clone, Eq, PartialEq)]
pub struct ReconstructiveComponent {
    kind: RuntimeComponentKind,
    codec_revision: u32,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

impl ReconstructiveComponent {
    /// Captures mandatory reconstruction bytes and their integrity identity.
    pub fn new(
        kind: RuntimeComponentKind,
        codec_revision: u32,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, RuntimeManifestError> {
        if codec_revision == 0 {
            return Err(RuntimeManifestError::ZeroRevision);
        }
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > kind.max_bytes() {
            return Err(RuntimeManifestError::ComponentSize);
        }
        let integrity = component_integrity(kind, codec_revision, &bytes)?;
        Ok(Self {
            kind,
            codec_revision,
            bytes,
            integrity,
        })
    }

    fn from_recorded(
        kind: RuntimeComponentKind,
        codec_revision: u32,
        bytes: Arc<[u8]>,
        integrity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        let component = Self::new(kind, codec_revision, bytes)?;
        if component.integrity != integrity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        Ok(component)
    }

    /// Returns the closed component meaning.
    #[must_use]
    pub const fn kind(&self) -> RuntimeComponentKind {
        self.kind
    }

    /// Returns the exact component codec revision.
    #[must_use]
    pub const fn codec_revision(&self) -> u32 {
        self.codec_revision
    }

    /// Returns the mandatory exact reconstruction bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns integrity evidence without substituting for [`Self::bytes`].
    #[must_use]
    pub const fn integrity(&self) -> Digest {
        self.integrity
    }
}

impl fmt::Debug for ReconstructiveComponent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReconstructiveComponent")
            .field("kind", &self.kind)
            .field("codec_revision", &self.codec_revision)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("integrity", &self.integrity)
            .finish()
    }
}

/// Stable non-secret reference whose authority owner must revalidate it.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BindingReference {
    authority: String,
    subject: String,
    revision: u64,
}

impl BindingReference {
    /// Creates a bounded reference. It carries no credential or capability.
    pub fn new(
        authority: impl Into<String>,
        subject: impl Into<String>,
        revision: u64,
    ) -> Result<Self, RuntimeManifestError> {
        if revision == 0 {
            return Err(RuntimeManifestError::ZeroRevision);
        }
        Ok(Self {
            authority: portable_id(authority.into(), "binding authority")?,
            subject: portable_id(subject.into(), "binding subject")?,
            revision,
        })
    }

    /// Returns the owning revalidation authority.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Returns the non-secret subject reference.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the exact reference semantics revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

impl fmt::Debug for BindingReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BindingReference")
            .field("authority", &self.authority)
            .field("subject", &"<redacted>")
            .field("revision", &self.revision)
            .finish()
    }
}

/// Historical admission/revalidation fact retained without credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevalidationFact {
    reference: BindingReference,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    evidence: ReconstructiveComponent,
}

impl RevalidationFact {
    /// Creates a bounded historical fact. Reopen still requires a fresh witness.
    pub fn new(
        reference: BindingReference,
        observed_at: Timestamp,
        valid_until: Option<Timestamp>,
        evidence: ReconstructiveComponent,
    ) -> Result<Self, RuntimeManifestError> {
        expect_component(&evidence, RuntimeComponentKind::AdmissionEvidence)?;
        if valid_until.is_some_and(|until| until <= observed_at) {
            return Err(RuntimeManifestError::InvalidRevalidation);
        }
        Ok(Self {
            reference,
            observed_at,
            valid_until,
            evidence,
        })
    }

    /// Returns the stable external reference.
    #[must_use]
    pub const fn reference(&self) -> &BindingReference {
        &self.reference
    }

    /// Returns when this evidence was observed.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the historical expiry, if any.
    #[must_use]
    pub const fn valid_until(&self) -> Option<Timestamp> {
        self.valid_until
    }

    /// Returns exact evidence bytes.
    #[must_use]
    pub const fn evidence(&self) -> &ReconstructiveComponent {
        &self.evidence
    }
}

/// Complete non-secret model, route, account, and profile binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRouteBinding {
    protocol: ProtocolId,
    route: RouteId,
    requested_model: ModelId,
    reported_model: Option<ModelId>,
    model_family: Option<ModelFamilyId>,
    adapter: ModelAdapterProfileId,
    account: BindingReference,
    profile: BindingReference,
    model_binding: Digest,
    descriptor: ReconstructiveComponent,
    admission: RevalidationFact,
}

impl ModelRouteBinding {
    /// Creates the exact binding selected before dispatch.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed provider binding"
    )]
    pub fn new(
        protocol: ProtocolId,
        route: RouteId,
        requested_model: ModelId,
        reported_model: Option<ModelId>,
        model_family: Option<ModelFamilyId>,
        adapter: ModelAdapterProfileId,
        account: BindingReference,
        profile: BindingReference,
        model_binding: Digest,
        descriptor: ReconstructiveComponent,
        admission: RevalidationFact,
    ) -> Result<Self, RuntimeManifestError> {
        validate_digest(model_binding, DigestDomain::ModelBinding)?;
        expect_component(&descriptor, RuntimeComponentKind::ModelDescriptor)?;
        Ok(Self {
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
        })
    }

    /// Returns the exact descriptor binding identity.
    #[must_use]
    pub const fn model_binding(&self) -> Digest {
        self.model_binding
    }

    /// Returns the exact selected route/account/profile/admission binding.
    pub fn route_binding_identity(&self) -> Result<Digest, RuntimeManifestError> {
        let group = binding_group_integrity(self)?;
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-agent/model-route-binding-v1",
                &group.identity_bytes(),
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)
    }

    /// Returns the account authority reference.
    #[must_use]
    pub const fn account(&self) -> &BindingReference {
        &self.account
    }

    /// Returns the profile authority reference.
    #[must_use]
    pub const fn profile(&self) -> &BindingReference {
        &self.profile
    }

    /// Returns the exact descriptor reconstruction component.
    #[must_use]
    pub const fn descriptor(&self) -> &ReconstructiveComponent {
        &self.descriptor
    }

    /// Returns the historical admission/revalidation evidence.
    #[must_use]
    pub const fn admission(&self) -> &RevalidationFact {
        &self.admission
    }

    /// Returns the exact protocol dialect.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the exact route.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the exact requested model.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }

    /// Returns the provider-reported model identity, when recorded.
    #[must_use]
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.reported_model.as_ref()
    }

    /// Returns the resolved model family, when recorded.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the exact selected model-adapter profile.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }
}

/// Exact tool registry/exposure/environment/effect-policy reconstruction data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRuntimeManifest {
    registry: ReconstructiveComponent,
    exposure: ReconstructiveComponent,
    environment: ReconstructiveComponent,
    effect_policy: ReconstructiveComponent,
    policy: BindingReference,
}

impl ToolRuntimeManifest {
    /// Creates a complete non-secret tool/host binding.
    pub fn new(
        registry: ReconstructiveComponent,
        exposure: ReconstructiveComponent,
        environment: ReconstructiveComponent,
        effect_policy: ReconstructiveComponent,
        policy: BindingReference,
    ) -> Result<Self, RuntimeManifestError> {
        expect_component(&registry, RuntimeComponentKind::ToolRegistry)?;
        expect_component(&exposure, RuntimeComponentKind::ToolExposure)?;
        expect_component(&environment, RuntimeComponentKind::ExecutionEnvironment)?;
        expect_component(&effect_policy, RuntimeComponentKind::EffectPolicy)?;
        Ok(Self {
            registry,
            exposure,
            environment,
            effect_policy,
            policy,
        })
    }

    /// Returns the exact frozen registry component.
    #[must_use]
    pub const fn registry(&self) -> &ReconstructiveComponent {
        &self.registry
    }

    /// Returns the exact frozen exposure component.
    #[must_use]
    pub const fn exposure(&self) -> &ReconstructiveComponent {
        &self.exposure
    }

    /// Returns the exact environment facts.
    #[must_use]
    pub const fn environment(&self) -> &ReconstructiveComponent {
        &self.environment
    }

    /// Returns the exact effect policy facts.
    #[must_use]
    pub const fn effect_policy(&self) -> &ReconstructiveComponent {
        &self.effect_policy
    }

    /// Returns the live policy revalidation reference.
    #[must_use]
    pub const fn policy(&self) -> &BindingReference {
        &self.policy
    }
}

/// Exact root-command acknowledgement retained for idempotent retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommandDisposition {
    /// The command entered the state machine.
    Accepted,
    /// The command was classified without semantic mutation.
    Rejected,
}

/// Reconstruction-complete command ledger record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableCommandRecord {
    command_id: String,
    sequence: u64,
    turn_id: Option<TurnId>,
    binding: Digest,
    command: ReconstructiveComponent,
    disposition: DurableCommandDisposition,
    rejection_code: Option<String>,
}

impl DurableCommandRecord {
    /// Creates one exact command/idempotency record.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are one exact command receipt"
    )]
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

/// Exact stable root shared by model requests and durable runtime operations.
pub type RuntimeOperationRoot = ModelOperationRoot;

/// Runtime-minted semantic identity of one joined process session.
///
/// This is never an operating-system PID or a reconnect capability. The
/// allocation-local backend identity remains outside the durable manifest.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessSessionId([u8; PROCESS_SESSION_ID_BYTES]);

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

/// Nonzero generation of the process-local owner that minted a session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessSessionOwnerGeneration(NonZeroU64);

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

/// Root-owned progress retained only by a recovery-only Working lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeWorkingRoot {
    /// One ordinary turn and its next model-round counter.
    Turn {
        /// Exact admitted root turn.
        turn_id: TurnId,
        /// Next round under the recorded runtime semantics.
        next_round: u32,
    },
    /// One high-level session workflow with no fabricated turn or round.
    SessionWorkflow(SessionWorkflowRootId),
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

/// Closed durable operation kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableOperationKind {
    /// Caller command classification or safe-point handoff.
    Command {
        /// Bound root turn, if any.
        turn_id: Option<TurnId>,
    },
    /// One admitted physical provider attempt.
    Provider {
        /// Bound logical operation root.
        root: RuntimeOperationRoot,
        /// Bound attempt identity.
        attempt_id: AttemptId,
    },
    /// One exact pending effect decision.
    EffectDecision {
        /// Bound root turn.
        turn_id: TurnId,
        /// Bound attempt identity.
        attempt_id: AttemptId,
        /// Bound tool-call identity.
        call_id: CallId,
    },
    /// One admitted tool effect.
    Effect {
        /// Bound root turn.
        turn_id: TurnId,
        /// Bound attempt identity.
        attempt_id: AttemptId,
        /// Bound tool-call identity.
        call_id: CallId,
        /// Assistant source order.
        source_index: u32,
    },
    /// One reconstruction-complete customer-tool batch continuation barrier.
    ToolBatchContinuation {
        /// Bound root turn.
        turn_id: TurnId,
        /// Provider attempt whose response requested the batch.
        attempt_id: AttemptId,
    },
    /// One reconstruction-complete provider server-pause continuation barrier.
    ServerPauseContinuation {
        /// Bound root turn.
        turn_id: TurnId,
        /// Provider attempt whose response paused server execution.
        attempt_id: AttemptId,
    },
    /// One driver-mediated local composition evaluation.
    CompositionEvaluation {
        /// Exact admitted root turn.
        turn_id: TurnId,
        /// Provider attempt that proposed the outer marker call.
        attempt_id: AttemptId,
        /// Exact outer exec/wait call identity.
        call_id: CallId,
        /// Assistant source order.
        source_index: u32,
    },
    /// One control/observation continuation bound to an existing evaluation.
    CompositionContinuation {
        /// Exact admitted root turn.
        turn_id: TurnId,
        /// Provider attempt that proposed the control call.
        attempt_id: AttemptId,
        /// Exact outer wait/control call identity.
        call_id: CallId,
        /// Assistant source order.
        source_index: u32,
        /// The one owning composition evaluation operation.
        evaluation_operation_id: u64,
    },
    /// One nested bridge request durably bound to its owning composition.
    CompositionChild {
        /// Exact admitted root turn.
        turn_id: TurnId,
        /// Provider attempt that proposed the outer composition call.
        attempt_id: AttemptId,
        /// Driver-derived canonical nested tool-call identity.
        call_id: CallId,
        /// Owning composition evaluation operation.
        evaluation_operation_id: u64,
        /// Exact interpreter generation that emitted the nested request.
        interpreter_generation: u64,
        /// Exact evaluator cell identity that emitted the nested request.
        cell_id: String,
        /// Driver acceptance sequence within the evaluation.
        request_sequence: u32,
    },
    /// One direct-host process lifetime that must remain Working until joined.
    ProcessSession {
        /// Exact admitted root turn.
        turn_id: TurnId,
        /// Runtime-minted semantic session identity; never a PID.
        session_id: ProcessSessionId,
        /// Exact process-local owner generation.
        owner_generation: ProcessSessionOwnerGeneration,
    },
    /// One exact checkpoint commit.
    Commit {
        /// Bound logical operation root.
        root: RuntimeOperationRoot,
    },
    /// One exact high-level compaction workflow.
    Compaction {
        /// Bound session workflow; compaction cannot fabricate a turn.
        workflow_id: SessionWorkflowRootId,
    },
}

impl DurableOperationKind {
    fn operation_root(&self) -> Option<RuntimeOperationRoot> {
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

    fn obligation_kind(&self) -> RecoveryObligationKind {
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

/// One exact operation transition, including all terminal evidence bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableOperationState {
    /// Identity and intent are recorded; absence of later state proves nothing.
    Prepared,
    /// One exact authorization request is durably paused before dispatch.
    ///
    /// The enclosed reconstruction state carries the complete bounded request
    /// and an explicit false dispatch-capability fence. It is legal only for
    /// the effect-decision operation kind.
    DecisionRequired(PendingEffectDecisionState),
    /// One exact provider-hosted continuation frame is durably paused.
    ///
    /// This is a provider-round program counter, not effect authorization and
    /// carries no local dispatch capability.
    ContinuationRequired(ReconstructiveComponent),
    /// The exact mutating-effect dispatch claim and fence are durable.
    Claimed(ReconstructiveComponent),
    /// The durable dispatch fence was crossed and physical start was authorized.
    ///
    /// This state deliberately does not assert that the provider observed the
    /// call. A crash between this checkpoint and the first paid poll is
    /// reconciled only through exact provider recovery evidence.
    Dispatched,
    /// The latest exact provider recovery locator was durably bound.
    ///
    /// A later locator transition supersedes an optional prepared locator but
    /// never implies dispatch or terminal outcome by itself.
    LocatorBound(ReconstructiveComponent),
    /// A typed outcome is proven but not necessarily folded/finalized.
    Completed(ReconstructiveComponent),
    /// Exact evidence proves physical dispatch did not occur.
    ProvenNotDispatched(ReconstructiveComponent),
    /// Physical outcome is ambiguous and requires reconciliation.
    OutcomeUnknown(ReconstructiveComponent),
    /// Proven terminal evidence was folded into checkpoint authority.
    ///
    /// Retaining the exact evidence is required for authoritative duplicate
    /// result retrieval after the operation has left the working set.
    Finalized(ReconstructiveComponent),
}

impl DurableOperationState {
    fn class(&self) -> OperationStateClass {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationStateClass {
    Prepared,
    DecisionRequired,
    ContinuationRequired,
    Claimed,
    Dispatched,
    LocatorBound,
    Completed,
    ProvenNotDispatched,
    OutcomeUnknown,
    Finalized,
}

/// Reconstruction-complete pre-dispatch effect-decision pause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingEffectDecisionState {
    request: ReconstructiveComponent,
    dispatch_capability_minted: bool,
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

/// State transition committed at one immutable checkpoint boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOperationTransition {
    checkpoint_id: CheckpointId,
    state: DurableOperationState,
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

/// Reconstruction-complete provider/tool/commit operation record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOperationRecord {
    operation_id: u64,
    kind: DurableOperationKind,
    request: ReconstructiveComponent,
    transitions: Vec<DurableOperationTransition>,
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

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CompositionEvaluationRequest {
    version: u32,
    descriptor_identity: Digest,
    host_identity: Vec<u8>,
    call: ToolCallBlock,
    arguments_digest: Digest,
    source_digest: Option<Digest>,
    result_entry_id: EntryId,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CompositionContinuationRequest {
    version: u32,
    descriptor_identity: Digest,
    host_identity: Vec<u8>,
    evaluation_operation_id: u64,
    call: ToolCallBlock,
    arguments_digest: Digest,
    result_entry_id: EntryId,
}

fn decode_exact_composition_request<T>(
    bytes: &[u8],
    expected_tag: &str,
) -> Result<T, RuntimeManifestError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let (tag, request): (String, T) =
        serde_json::from_slice(bytes).map_err(|_| RuntimeManifestError::InvalidLedger)?;
    if tag != expected_tag
        || serde_json::to_vec(&(tag.as_str(), &request))
            .map_err(|_| RuntimeManifestError::InvalidLedger)?
            != bytes
    {
        return Err(RuntimeManifestError::InvalidLedger);
    }
    Ok(request)
}

fn composition_child_call_id(binding: Digest) -> Result<CallId, RuntimeManifestError> {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&binding.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CallId::try_from(uuid::Uuid::from_bytes(bytes)).map_err(|_| RuntimeManifestError::InvalidLedger)
}

fn validate_composition_graph(ledger: &RuntimeOperationLedger) -> Result<(), RuntimeManifestError> {
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
    kind: RecoveryObligationKind,
    action: RecoveryAction,
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
    operation_id: u64,
    signature: RecoveryHandlerSignature,
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

/// Exact branch source recorded for a forked checkpoint lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForkLineage {
    /// Source session.
    pub session_id: SessionId,
    /// Source branch.
    pub branch_id: BranchId,
    /// Exact source checkpoint.
    pub checkpoint_id: CheckpointId,
    /// Exact source branch revision.
    pub branch_revision: u64,
}

/// Immutable topology and commit authority bound by the manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTopology {
    /// Session identity.
    pub session_id: SessionId,
    /// Branch identity.
    pub branch_id: BranchId,
    /// Checkpoint identity.
    pub checkpoint_id: CheckpointId,
    /// Immediate parent checkpoint, absent only for an initial root.
    pub parent_checkpoint_id: Option<CheckpointId>,
    /// Independent branch revision.
    pub branch_revision: u64,
    /// Exact fork lineage, if this branch was forked.
    pub fork: Option<ForkLineage>,
    /// Lease/fence value under which this checkpoint committed.
    pub lease_fence: u64,
    /// Exact commit operation identity.
    pub commit_identity: Digest,
}

impl RuntimeTopology {
    fn validate(self) -> Result<Self, RuntimeManifestError> {
        if self.lease_fence == 0
            || self.parent_checkpoint_id == Some(self.checkpoint_id)
            || self.fork.is_some_and(|fork| {
                fork.session_id == self.session_id
                    && fork.branch_id == self.branch_id
                    && fork.checkpoint_id == self.checkpoint_id
            })
        {
            return Err(RuntimeManifestError::InvalidTopology);
        }
        validate_digest(self.commit_identity, DigestDomain::ProviderOperation)?;
        Ok(self)
    }
}

/// Closed runtime-owned context transition represented by the typed context state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeContextTransitionKind {
    /// A safe ordinary branch fork with unchanged binding semantics.
    OrdinaryFork,
    /// A safe explicit rebind onto a newly validated target binding.
    Rebind,
    /// One successfully published semantic compaction on the same branch.
    SuccessfulCompaction,
}

impl RuntimeContextTransitionKind {
    const fn tag(self) -> u8 {
        match self {
            Self::OrdinaryFork => 1,
            Self::Rebind => 2,
            Self::SuccessfulCompaction => 5,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, RuntimeManifestError> {
        match tag {
            1 => Ok(Self::OrdinaryFork),
            2 => Ok(Self::Rebind),
            5 => Ok(Self::SuccessfulCompaction),
            _ => Err(RuntimeManifestError::UnknownTag {
                field: "runtime context transition",
                tag,
            }),
        }
    }
}

/// Exact source and target proof for the last context-rotation fence.
#[derive(Clone, Eq, PartialEq)]
pub struct RuntimeContextTransition {
    kind: RuntimeContextTransitionKind,
    source_session: SessionId,
    source_branch: BranchId,
    source_checkpoint: CheckpointId,
    source_context: Digest,
    source_affinity: Digest,
    source_epoch: u64,
    target_session: SessionId,
    target_branch: BranchId,
    target_affinity: Digest,
    target_epoch: u64,
    identity: Digest,
}

impl RuntimeContextTransition {
    fn new(
        kind: RuntimeContextTransitionKind,
        source: &RuntimeContextState,
        source_checkpoint: CheckpointId,
        target: &ModelCacheAffinity,
    ) -> Result<Self, RuntimeManifestError> {
        let mut transition = Self {
            kind,
            source_session: source.affinity.session(),
            source_branch: source.affinity.branch(),
            source_checkpoint,
            source_context: source.identity,
            source_affinity: source.affinity.identity(),
            source_epoch: source.affinity.epoch(),
            target_session: target.session(),
            target_branch: target.branch(),
            target_affinity: target.identity(),
            target_epoch: target.epoch(),
            identity: source.identity,
        };
        transition.identity = runtime_context_transition_identity(&transition)?;
        transition.validate()?;
        Ok(transition)
    }

    fn from_recorded(mut transition: Self, identity: Digest) -> Result<Self, RuntimeManifestError> {
        transition.validate()?;
        if runtime_context_transition_identity(&transition)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        transition.identity = identity;
        Ok(transition)
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        validate_digest(self.source_context, DigestDomain::ContextBudget)?;
        validate_digest(self.source_affinity, DigestDomain::CacheAffinity)?;
        validate_digest(self.target_affinity, DigestDomain::CacheAffinity)?;
        match self.kind {
            RuntimeContextTransitionKind::OrdinaryFork | RuntimeContextTransitionKind::Rebind => {
                if self.source_affinity == self.target_affinity
                    || self.target_epoch != 0
                    || (self.source_session == self.target_session
                        && self.source_branch == self.target_branch)
                {
                    return Err(RuntimeManifestError::InvalidContextTransition);
                }
            }
            RuntimeContextTransitionKind::SuccessfulCompaction => {
                if self.source_session != self.target_session
                    || self.source_branch != self.target_branch
                    || if self.source_affinity == self.target_affinity {
                        self.target_epoch != self.source_epoch
                    } else {
                        self.source_epoch.checked_add(1) != Some(self.target_epoch)
                    }
                {
                    return Err(RuntimeManifestError::InvalidContextTransition);
                }
            }
        }
        Ok(())
    }

    /// Returns the closed transition kind.
    #[must_use]
    pub const fn kind(&self) -> RuntimeContextTransitionKind {
        self.kind
    }

    /// Returns the exact source session.
    #[must_use]
    pub const fn source_session(&self) -> SessionId {
        self.source_session
    }

    /// Returns the exact source branch.
    #[must_use]
    pub const fn source_branch(&self) -> BranchId {
        self.source_branch
    }

    /// Returns the exact source checkpoint.
    #[must_use]
    pub const fn source_checkpoint(&self) -> CheckpointId {
        self.source_checkpoint
    }

    /// Returns the exact source context-state identity.
    #[must_use]
    pub const fn source_context_identity(&self) -> Digest {
        self.source_context
    }

    /// Returns the source affinity identity without exposing opaque entropy.
    #[must_use]
    pub const fn source_affinity_identity(&self) -> Digest {
        self.source_affinity
    }

    /// Returns the source branch cache epoch.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.source_epoch
    }

    /// Returns the target session.
    #[must_use]
    pub const fn target_session(&self) -> SessionId {
        self.target_session
    }

    /// Returns the target branch.
    #[must_use]
    pub const fn target_branch(&self) -> BranchId {
        self.target_branch
    }

    /// Returns the target affinity identity without exposing opaque entropy.
    #[must_use]
    pub const fn target_affinity_identity(&self) -> Digest {
        self.target_affinity
    }

    /// Returns the target branch cache epoch.
    #[must_use]
    pub const fn target_epoch(&self) -> u64 {
        self.target_epoch
    }

    /// Returns the recomputed transition identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl fmt::Debug for RuntimeContextTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeContextTransition")
            .field("kind", &self.kind)
            .field("source_session", &self.source_session)
            .field("source_branch", &self.source_branch)
            .field("source_checkpoint", &self.source_checkpoint)
            .field("source_epoch", &self.source_epoch)
            .field("target_session", &self.target_session)
            .field("target_branch", &self.target_branch)
            .field("target_epoch", &self.target_epoch)
            .field("affinity", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Non-authoritative audit facts for one fully evaluated projection.
///
/// These identities never reconstruct or authorize a [`plakous_model::ContextStatus`].
/// They are retained only inside compaction provenance together with the exact
/// observed semantic input-token count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextEvaluationAudit {
    measurement: Digest,
    status: Digest,
    input_tokens: u64,
}

impl ContextEvaluationAudit {
    /// Captures bounded audit facts from one complete prepared projection.
    pub fn new(
        measurement: Digest,
        status: Digest,
        input_tokens: u64,
    ) -> Result<Self, RuntimeManifestError> {
        validate_digest(measurement, DigestDomain::TokenMeasurement)?;
        validate_digest(status, DigestDomain::ContextAdmission)?;
        Ok(Self {
            measurement,
            status,
            input_tokens,
        })
    }

    /// Returns the measurement identity as audit provenance only.
    #[must_use]
    pub const fn measurement_identity(self) -> Digest {
        self.measurement
    }

    /// Returns the checked-status identity as audit provenance only.
    #[must_use]
    pub const fn status_identity(self) -> Digest {
        self.status
    }

    /// Returns the complete semantic input-token count observed at that fence.
    #[must_use]
    pub const fn input_tokens(self) -> u64 {
        self.input_tokens
    }
}

/// Closed compaction strategies constructible by this release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCompactionStrategy {
    /// Driver-owned semantic summary of a closed whole-group prefix.
    SemanticSummaryPrefixV1,
    /// Codex OAuth remote compact: `compaction_trigger` on `/responses`.
    NativeResponsesTriggerV1,
    /// Offline oldest-first placeholders of tool-result bodies in the protected tail.
    ///
    /// Every call-and-result group stays whole and paired; only the result
    /// body is replaced by a bounded placeholder naming the tool, the call id
    /// and the original size. The successor appends exactly one compaction
    /// segment recording the placeholder pass, so the transformation is committed
    /// history, never silent deletion.
    ToolResultPlaceholdersV1,
    /// Offline omission of unbound replay, retaining all portable tool results.
    ReplayOmissionV1,
}

impl RuntimeCompactionStrategy {
    const fn tag(self) -> u8 {
        match self {
            Self::SemanticSummaryPrefixV1 => 1,
            Self::NativeResponsesTriggerV1 => 2,
            Self::ToolResultPlaceholdersV1 => 3,
            Self::ReplayOmissionV1 => 4,
        }
    }
}

/// Exact invalidation and epoch-rotation proof published with compaction.
#[derive(Clone, Eq, PartialEq)]
pub struct CompactionInvalidation {
    source_affinity: Digest,
    source_epoch: u64,
    target_affinity: Digest,
    target_epoch: u64,
}

impl CompactionInvalidation {
    fn new(
        source: &ModelCacheAffinity,
        target: &ModelCacheAffinity,
    ) -> Result<Self, RuntimeManifestError> {
        let invalidation = Self {
            source_affinity: source.identity(),
            source_epoch: source.epoch(),
            target_affinity: target.identity(),
            target_epoch: target.epoch(),
        };
        invalidation.validate()?;
        Ok(invalidation)
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        validate_digest(self.source_affinity, DigestDomain::CacheAffinity)?;
        validate_digest(self.target_affinity, DigestDomain::CacheAffinity)?;
        let preserved =
            self.source_affinity == self.target_affinity && self.source_epoch == self.target_epoch;
        let rotated = self.source_affinity != self.target_affinity
            && self.source_epoch.checked_add(1) == Some(self.target_epoch);
        if !preserved && !rotated {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    /// Returns the exact retired epoch.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.source_epoch
    }

    /// Returns the exact published epoch.
    #[must_use]
    pub const fn target_epoch(&self) -> u64 {
        self.target_epoch
    }

    /// Reports mandatory capacity-observation invalidation.
    #[must_use]
    pub const fn clears_capacity_observations(&self) -> bool {
        true
    }

    /// Reports mandatory continuation-anchor invalidation.
    #[must_use]
    pub const fn clears_continuation_anchor(&self) -> bool {
        true
    }
}

impl fmt::Debug for CompactionInvalidation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionInvalidation")
            .field("source_affinity", &"<redacted>")
            .field("source_epoch", &self.source_epoch)
            .field("target_affinity", &"<redacted>")
            .field("target_epoch", &self.target_epoch)
            .finish_non_exhaustive()
    }
}

/// Validated driver inputs not already derivable from canonical histories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCompactionPublication {
    source_checkpoint: CheckpointId,
    operation: Digest,
    route_binding: Digest,
    model_binding: Digest,
    harness_plan: Digest,
    before: ContextEvaluationAudit,
    after: ContextEvaluationAudit,
    usage: Option<Usage>,
}

impl SemanticCompactionPublication {
    /// Captures exact non-content publication facts for semantic compaction.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact compact publication authority tuple"
    )]
    pub fn new(
        source_checkpoint: CheckpointId,
        operation: Digest,
        route_binding: Digest,
        model_binding: Digest,
        harness_plan: Digest,
        before: ContextEvaluationAudit,
        after: ContextEvaluationAudit,
        usage: Option<Usage>,
    ) -> Result<Self, RuntimeManifestError> {
        validate_digest(operation, DigestDomain::ProviderOperation)?;
        validate_digest(route_binding, DigestDomain::ModelBinding)?;
        validate_digest(model_binding, DigestDomain::ModelBinding)?;
        validate_digest(harness_plan, DigestDomain::ResolvedHarnessPlan)?;
        Ok(Self {
            source_checkpoint,
            operation,
            route_binding,
            model_binding,
            harness_plan,
            before,
            after,
            usage,
        })
    }
}

/// Bounded reconstructive audit provenance for one committed compaction.
///
/// Summary content remains solely in the referenced canonical compaction
/// segment. Measurement/status identities are explicitly audit provenance and
/// never become reusable admission anchors.
#[derive(Clone, Eq, PartialEq)]
pub struct CompactionProvenance {
    source_checkpoint: CheckpointId,
    source_history: Digest,
    operation: Digest,
    route_binding: Digest,
    model_binding: Digest,
    harness_plan: Digest,
    selection_plan: Digest,
    strategy: RuntimeCompactionStrategy,
    strategy_revision: u32,
    source_projection: Digest,
    selected_start: u32,
    selected_end: u32,
    selected_group_count: u32,
    selected_segment_count: u32,
    selected_identity: Digest,
    retained_segment_count: u32,
    retained_tail_identity: Digest,
    target_history: Digest,
    target_segment_count: u32,
    target_projection: Digest,
    before: ContextEvaluationAudit,
    after: ContextEvaluationAudit,
    summary_segment: Digest,
    usage: Option<Usage>,
    invalidation: CompactionInvalidation,
    identity: Digest,
}

impl CompactionProvenance {
    fn from_recorded(mut provenance: Self, identity: Digest) -> Result<Self, RuntimeManifestError> {
        provenance.validate()?;
        if compaction_provenance_identity(&provenance)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        provenance.identity = identity;
        Ok(provenance)
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        for (digest, domain) in [
            (self.source_history, DigestDomain::CanonicalHistory),
            (self.operation, DigestDomain::ProviderOperation),
            (self.route_binding, DigestDomain::ModelBinding),
            (self.model_binding, DigestDomain::ModelBinding),
            (self.harness_plan, DigestDomain::ResolvedHarnessPlan),
            (self.selection_plan, DigestDomain::ContextPlan),
            (self.source_projection, DigestDomain::HistoryProjection),
            (self.selected_identity, DigestDomain::ContextPlan),
            (self.retained_tail_identity, DigestDomain::ContextPlan),
            (self.target_history, DigestDomain::CanonicalHistory),
            (self.target_projection, DigestDomain::HistoryProjection),
            (self.summary_segment, DigestDomain::CanonicalHistorySegment),
        ] {
            validate_digest(digest, domain)?;
        }
        if self.strategy_revision == 0
            || (!matches!(
                self.strategy,
                RuntimeCompactionStrategy::ToolResultPlaceholdersV1
                    | RuntimeCompactionStrategy::ReplayOmissionV1
            ) && self.invalidation.source_affinity == self.invalidation.target_affinity)
            || self.selected_group_count == 0
            || (self.strategy == RuntimeCompactionStrategy::ReplayOmissionV1
                && self.selected_group_count != 1)
            || self.selected_segment_count == 0
            || self.selected_start >= self.selected_end
            || self.selected_end.saturating_sub(self.selected_start) < self.selected_segment_count
            || self.target_segment_count != self.retained_segment_count.saturating_add(1)
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        self.invalidation.validate()
    }

    fn validate_history_prefix(
        &self,
        history: &CanonicalHistory,
    ) -> Result<(), RuntimeManifestError> {
        let segments = history.segments();
        let target_len = usize::try_from(self.target_segment_count)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        if segments.len() < target_len {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let summary_index = match self.strategy {
            RuntimeCompactionStrategy::NativeResponsesTriggerV1 => {
                let index = usize::try_from(self.retained_segment_count)
                    .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
                if index.saturating_add(1) != target_len {
                    return Err(RuntimeManifestError::InvalidCompactionProvenance);
                }
                index
            }
            RuntimeCompactionStrategy::SemanticSummaryPrefixV1 => 0,
            // A placeholder pass merges into whichever compaction segment the source
            // already had, so its position is whatever the published prefix
            // says — provided there is exactly one.
            RuntimeCompactionStrategy::ToolResultPlaceholdersV1
            | RuntimeCompactionStrategy::ReplayOmissionV1 => {
                let mut found = None;
                for (index, segment) in segments.iter().take(target_len).enumerate() {
                    if segment.kind() != HistorySegmentKind::Compaction {
                        continue;
                    }
                    if found.is_some() {
                        return Err(RuntimeManifestError::InvalidCompactionProvenance);
                    }
                    found = Some(index);
                }
                found.ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            }
        };
        let summary = segments
            .get(summary_index)
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if summary.kind() != HistorySegmentKind::Compaction
            || summary.identity() != self.summary_segment
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let prefix = history
            .segment_prefix_identity(target_len)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        if prefix != self.target_history {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    /// Returns the exact source checkpoint.
    #[must_use]
    pub const fn source_checkpoint(&self) -> CheckpointId {
        self.source_checkpoint
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub const fn source_history_identity(&self) -> Digest {
        self.source_history
    }

    /// Returns the durable operation identity.
    #[must_use]
    pub const fn operation_identity(&self) -> Digest {
        self.operation
    }

    /// Returns which closed strategy published this compaction.
    ///
    /// A consumer that has to tell a fold from a placeholder pass reads this rather
    /// than inferring it from the successor's shape: both put their summary
    /// segment at position zero, so the shape alone does not discriminate.
    #[must_use]
    pub const fn strategy(&self) -> RuntimeCompactionStrategy {
        self.strategy
    }

    /// Returns the exact selected source range and complete-segment count.
    #[must_use]
    pub const fn selected_range(&self) -> (u32, u32, u32) {
        (
            self.selected_start,
            self.selected_end,
            self.selected_segment_count,
        )
    }

    /// Returns the selection root binding every selected complete group.
    #[must_use]
    pub const fn selected_identity(&self) -> Digest {
        self.selected_identity
    }

    /// Returns the retained-tail root and complete-segment count.
    #[must_use]
    pub const fn retained_tail(&self) -> (Digest, u32) {
        (self.retained_tail_identity, self.retained_segment_count)
    }

    /// Returns the canonical summary segment identity without duplicating content.
    #[must_use]
    pub const fn summary_segment_identity(&self) -> Digest {
        self.summary_segment
    }

    /// Returns provider usage when it was available.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns the exact independently measured pre-compaction audit facts.
    #[must_use]
    pub const fn before_audit(&self) -> ContextEvaluationAudit {
        self.before
    }

    /// Returns the exact independently measured post-compaction audit facts.
    #[must_use]
    pub const fn after_audit(&self) -> ContextEvaluationAudit {
        self.after
    }

    /// Returns exact cache/anchor invalidation evidence.
    #[must_use]
    pub const fn invalidation(&self) -> &CompactionInvalidation {
        &self.invalidation
    }

    /// Returns the recomputed provenance identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl fmt::Debug for CompactionProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionProvenance")
            .field("source_checkpoint", &self.source_checkpoint)
            .field("source_history", &self.source_history)
            .field("operation", &self.operation)
            .field("strategy", &self.strategy)
            .field("selected_segment_count", &self.selected_segment_count)
            .field("retained_segment_count", &self.retained_segment_count)
            .field("summary_segment", &self.summary_segment)
            .field("invalidation", &self.invalidation)
            .finish_non_exhaustive()
    }
}

/// One compact typed, bounded, versioned runtime context authority.
#[derive(Clone, Eq, PartialEq)]
pub struct RuntimeContextState {
    affinity: ModelCacheAffinity,
    /// The driver's context-measure anchor as of the last turn terminal
    /// (spec 01, 10.3). Every transition and compaction successor starts
    /// without one.
    occupancy_anchor: Option<ContextAnchor>,
    request_anchor: Option<plakous_model::ContextRequestAnchor>,
    transition: Option<RuntimeContextTransition>,
    compaction: Option<CompactionProvenance>,
    identity: Digest,
}

impl RuntimeContextState {
    /// Constructs branch genesis from runtime-supplied 256-bit entropy.
    pub fn genesis(
        session: SessionId,
        branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        let affinity = ModelCacheAffinity::from_runtime_entropy(session, branch, 0, entropy)
            .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        Self::from_parts(affinity, None, None, None)
    }

    /// Retains the exact state for retry or verified quiescent resume.
    #[must_use]
    pub fn retained_for_exact_resume(&self) -> Self {
        self.clone()
    }

    /// Records the driver's context-measure anchor, replacing any earlier one.
    pub fn with_occupancy_anchor(
        &self,
        occupancy_anchor: Option<ContextAnchor>,
    ) -> Result<Self, RuntimeManifestError> {
        Self::from_parts(
            self.affinity.clone(),
            occupancy_anchor,
            self.transition.clone(),
            self.compaction.clone(),
        )?
        .with_request_anchor(self.request_anchor.clone())
    }

    /// Records item-delta admission authority from a completed response.
    pub fn with_request_anchor(
        &self,
        request_anchor: Option<plakous_model::ContextRequestAnchor>,
    ) -> Result<Self, RuntimeManifestError> {
        let mut state = self.clone();
        state.request_anchor = request_anchor;
        state.identity = runtime_context_state_identity(&state)?;
        Ok(state)
    }

    /// Returns completed-response authority for exact-binding admission.
    #[must_use]
    pub const fn request_anchor(&self) -> Option<&plakous_model::ContextRequestAnchor> {
        self.request_anchor.as_ref()
    }

    /// Creates a fresh target-branch affinity and exact ordinary-fork proof.
    pub fn ordinary_fork(
        &self,
        source_checkpoint: CheckpointId,
        target_session: SessionId,
        target_branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        self.branch_transition(
            RuntimeContextTransitionKind::OrdinaryFork,
            source_checkpoint,
            target_session,
            target_branch,
            entropy,
        )
    }

    /// Creates a fresh target-branch affinity and exact rebind proof.
    ///
    /// No capacity observation, measurement anchor, or continuation anchor is
    /// carried: this revision deliberately has no reusable observation fields.
    pub fn rebind(
        &self,
        source_checkpoint: CheckpointId,
        target_session: SessionId,
        target_branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        self.branch_transition(
            RuntimeContextTransitionKind::Rebind,
            source_checkpoint,
            target_session,
            target_branch,
            entropy,
        )
    }

    /// Atomically describes a Codex remote-compaction publication.
    ///
    /// The successor keeps a newest-first 64k-token retained
    /// user/developer/system tail and appends exactly one native
    /// compaction segment last. Cache affinity still rotates.
    #[allow(
        clippy::too_many_arguments,
        reason = "native publication binds source, summary, successor, and affinity entropy together"
    )]
    pub fn successful_native_compaction(
        &self,
        publication: &SemanticCompactionPublication,
        summary_segment: &HistorySegment,
        successor_history: &CanonicalHistory,
        source_history: Digest,
        source_projection: Digest,
        entropy: [u8; 32],
        source_segment_count: usize,
    ) -> Result<Self, RuntimeManifestError> {
        let successor_segments = successor_history.segments();
        let retained_segment_count = successor_segments.len().saturating_sub(1);
        let summary = if successor_segments.is_empty() {
            None
        } else {
            successor_segments.get(successor_segments.len() - 1)
        }
        .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if summary_segment.kind() != HistorySegmentKind::Compaction
            || summary.kind() != HistorySegmentKind::Compaction
            || summary.identity() != summary_segment.identity()
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let selected_end = u32::try_from(source_segment_count.max(1))
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let next_epoch = self
            .affinity
            .epoch()
            .checked_add(1)
            .ok_or(RuntimeManifestError::InvalidContextTransition)?;
        let affinity = ModelCacheAffinity::from_runtime_entropy(
            self.affinity.session(),
            self.affinity.branch(),
            next_epoch,
            entropy,
        )
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        let transition = RuntimeContextTransition::new(
            RuntimeContextTransitionKind::SuccessfulCompaction,
            self,
            publication.source_checkpoint,
            &affinity,
        )?;
        let invalidation = CompactionInvalidation::new(&self.affinity, &affinity)?;
        let target_projection =
            ConversationView::from_shared(successor_history.clone(), WorkingProjection::default());
        let retained_segment_count = u32::try_from(retained_segment_count)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let target_segment_count = u32::try_from(successor_history.segments().len())
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let retained_tail_identity = if retained_segment_count == 0 {
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[b"plakous-agent/native-compaction-empty-tail-v1"],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        } else {
            let prefix = successor_history
                .segment_prefix_identity(retained_segment_count as usize)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[
                    b"plakous-agent/native-compaction-retained-prefix-v1",
                    prefix.bytes(),
                ],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        };
        let selection_plan = Digest::sha256(
            DigestDomain::ContextPlan,
            2,
            &[
                b"plakous-agent/native-compaction-selection-v1",
                source_history.bytes(),
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)?;
        let mut provenance = CompactionProvenance {
            source_checkpoint: publication.source_checkpoint,
            source_history,
            operation: publication.operation,
            route_binding: publication.route_binding,
            model_binding: publication.model_binding,
            harness_plan: publication.harness_plan,
            selection_plan,
            strategy: RuntimeCompactionStrategy::NativeResponsesTriggerV1,
            strategy_revision: 1,
            source_projection,
            selected_start: 0,
            selected_end,
            selected_group_count: selected_end,
            selected_segment_count: selected_end,
            selected_identity: selection_plan,
            retained_segment_count,
            retained_tail_identity,
            target_history: successor_history.identity(),
            target_segment_count,
            target_projection: target_projection.identity(),
            before: publication.before,
            after: publication.after,
            summary_segment: summary_segment.identity(),
            usage: publication.usage,
            invalidation,
            identity: self.identity,
        };
        provenance.identity = compaction_provenance_identity(&provenance)?;
        provenance.validate()?;
        Self::from_parts(affinity, None, Some(transition), Some(provenance))
    }

    /// Atomically describes a successful offline tool-result placeholders.
    ///
    /// The successor keeps every source segment in order with replaced
    /// tool-result bodies replaced by bounded placeholders, and appends
    /// exactly one compaction segment recording the placeholder pass. No provider
    /// operation is involved, so `publication` carries no usage; the before
    /// and after audits are still independently measured. Cache affinity and
    /// its epoch are preserved; stale anchors are cleared.
    #[allow(
        clippy::too_many_arguments,
        reason = "placeholders publication binds source, notice, successor and selection together"
    )]
    pub fn successful_tool_result_placeholders(
        &self,
        publication: &SemanticCompactionPublication,
        notice_segment: &HistorySegment,
        successor_history: &CanonicalHistory,
        source_history: Digest,
        source_projection: Digest,
        selected_start: u32,
        selected_end: u32,
        replaced_result_count: u32,
        source_segment_count: u32,
    ) -> Result<Self, RuntimeManifestError> {
        let successor_segments = successor_history.segments();
        let retained_segment_count = successor_segments.len().saturating_sub(1);
        // The notice is the history's sole compaction segment: merged into an
        // existing summary in place, or created at position zero. A second one
        // would make the whole history unplannable.
        let mut notice_position = None;
        for (index, segment) in successor_segments.iter().enumerate() {
            if segment.kind() != HistorySegmentKind::Compaction {
                continue;
            }
            if notice_position.is_some() {
                return Err(RuntimeManifestError::InvalidCompactionProvenance);
            }
            notice_position = Some(index);
        }
        let notice = notice_position
            .and_then(|index| successor_segments.get(index))
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if notice_segment.kind() != HistorySegmentKind::Compaction
            || notice.identity() != notice_segment.identity()
            || selected_start >= selected_end
            || selected_end > source_segment_count
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let retained_segment_count = u32::try_from(retained_segment_count)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let affinity = self.affinity.clone();
        let transition = RuntimeContextTransition::new(
            RuntimeContextTransitionKind::SuccessfulCompaction,
            self,
            publication.source_checkpoint,
            &affinity,
        )?;
        let invalidation = CompactionInvalidation::new(&self.affinity, &affinity)?;
        let target_projection =
            ConversationView::from_shared(successor_history.clone(), WorkingProjection::default());
        let target_segment_count = u32::try_from(successor_segments.len())
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let retained_tail_identity = if retained_segment_count == 0 {
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[b"plakous-agent/tool-result-placeholders-empty-tail-v1"],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        } else {
            let prefix = successor_history
                .segment_prefix_identity(retained_segment_count as usize)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[
                    b"plakous-agent/tool-result-placeholders-retained-prefix-v1",
                    prefix.bytes(),
                ],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        };
        let selection_plan = Digest::sha256(
            DigestDomain::ContextPlan,
            2,
            &[
                b"plakous-agent/tool-result-placeholders-selection-v1",
                source_history.bytes(),
                &selected_start.to_be_bytes(),
                &selected_end.to_be_bytes(),
                &replaced_result_count.to_be_bytes(),
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)?;
        let mut provenance = CompactionProvenance {
            source_checkpoint: publication.source_checkpoint,
            source_history,
            operation: publication.operation,
            route_binding: publication.route_binding,
            model_binding: publication.model_binding,
            harness_plan: publication.harness_plan,
            selection_plan,
            strategy: if replaced_result_count == 0 {
                RuntimeCompactionStrategy::ReplayOmissionV1
            } else {
                RuntimeCompactionStrategy::ToolResultPlaceholdersV1
            },
            strategy_revision: 1,
            source_projection,
            selected_start,
            selected_end,
            selected_group_count: replaced_result_count.max(1),
            selected_segment_count: selected_end.saturating_sub(selected_start),
            selected_identity: selection_plan,
            retained_segment_count,
            retained_tail_identity,
            target_history: successor_history.identity(),
            target_segment_count,
            target_projection: target_projection.identity(),
            before: publication.before,
            after: publication.after,
            summary_segment: notice_segment.identity(),
            usage: publication.usage,
            invalidation,
            identity: self.identity,
        };
        provenance.identity = compaction_provenance_identity(&provenance)?;
        provenance.validate()?;
        Self::from_parts(affinity, None, Some(transition), Some(provenance))
    }

    /// Atomically describes a successful semantic-summary publication.
    ///
    /// The target history must contain exactly one leading compaction segment
    /// followed by the closed planner's retained source segments. Summary
    /// content stays only in that canonical segment.
    #[allow(
        clippy::too_many_arguments,
        reason = "the histories and summary are independently reconstructive publication inputs"
    )]
    pub fn successful_semantic_compaction(
        &self,
        publication: &SemanticCompactionPublication,
        cut: &SemanticCompactionCut,
        summary_segment: &HistorySegment,
        successor_history: &CanonicalHistory,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        if summary_segment.kind() != HistorySegmentKind::Compaction
            || successor_history
                .segments()
                .get(0)
                .map(HistorySegment::identity)
                != Some(summary_segment.identity())
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let sealed_successor = cut
            .build_successor(summary_segment)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        if sealed_successor.identity() != successor_history.identity()
            || sealed_successor
                .segments()
                .iter()
                .map(HistorySegment::identity)
                .ne(successor_history
                    .segments()
                    .iter()
                    .map(HistorySegment::identity))
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let selected = cut.selected_segment_evidence();
        let selected_start = selected
            .first()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            .position();
        let selected_end = selected
            .last()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            .position()
            .checked_add(1)
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        let retained_segment_count = cut.retained_segment_count();
        if successor_history.segments().len() != retained_segment_count.saturating_add(1)
            || successor_history
                .segments()
                .iter()
                .skip(1)
                .map(HistorySegment::identity)
                .ne(cut.retained_segments().iter().map(HistorySegment::identity))
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let next_epoch = self
            .affinity
            .epoch()
            .checked_add(1)
            .ok_or(RuntimeManifestError::InvalidContextTransition)?;
        let affinity = ModelCacheAffinity::from_runtime_entropy(
            self.affinity.session(),
            self.affinity.branch(),
            next_epoch,
            entropy,
        )
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        let transition = RuntimeContextTransition::new(
            RuntimeContextTransitionKind::SuccessfulCompaction,
            self,
            publication.source_checkpoint,
            &affinity,
        )?;
        let invalidation = CompactionInvalidation::new(&self.affinity, &affinity)?;
        let target_projection =
            ConversationView::from_shared(successor_history.clone(), WorkingProjection::default());
        let mut provenance = CompactionProvenance {
            source_checkpoint: publication.source_checkpoint,
            source_history: cut.source_history_identity(),
            operation: publication.operation,
            route_binding: publication.route_binding,
            model_binding: publication.model_binding,
            harness_plan: publication.harness_plan,
            selection_plan: cut.plan_identity(),
            strategy: RuntimeCompactionStrategy::SemanticSummaryPrefixV1,
            strategy_revision: 1,
            source_projection: cut.source_projection_identity(),
            selected_start,
            selected_end,
            selected_group_count: u32::try_from(cut.selected_intent_groups().len())
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            selected_segment_count: u32::try_from(selected.len())
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            selected_identity: cut.selection_identity(),
            retained_segment_count: u32::try_from(retained_segment_count)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            retained_tail_identity: cut.retained_tail_identity(),
            target_history: successor_history.identity(),
            target_segment_count: u32::try_from(successor_history.segments().len())
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            target_projection: target_projection.identity(),
            before: publication.before,
            after: publication.after,
            summary_segment: summary_segment.identity(),
            usage: publication.usage,
            invalidation,
            identity: self.identity,
        };
        provenance.identity = compaction_provenance_identity(&provenance)?;
        provenance.validate()?;
        Self::from_parts(affinity, None, Some(transition), Some(provenance))
    }

    fn branch_transition(
        &self,
        kind: RuntimeContextTransitionKind,
        source_checkpoint: CheckpointId,
        target_session: SessionId,
        target_branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        if !matches!(
            kind,
            RuntimeContextTransitionKind::OrdinaryFork | RuntimeContextTransitionKind::Rebind
        ) {
            return Err(RuntimeManifestError::InvalidContextTransition);
        }
        let affinity =
            ModelCacheAffinity::from_runtime_entropy(target_session, target_branch, 0, entropy)
                .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        let transition = RuntimeContextTransition::new(kind, self, source_checkpoint, &affinity)?;
        Self::from_parts(affinity, None, Some(transition), self.compaction.clone())
    }

    fn from_parts(
        affinity: ModelCacheAffinity,
        occupancy_anchor: Option<ContextAnchor>,
        transition: Option<RuntimeContextTransition>,
        compaction: Option<CompactionProvenance>,
    ) -> Result<Self, RuntimeManifestError> {
        let mut state = Self {
            affinity,
            occupancy_anchor,
            request_anchor: None,
            transition,
            compaction,
            identity: Digest::sha256(DigestDomain::ContextBudget, 1, &[b"initial"])
                .map_err(|_| RuntimeManifestError::InvalidDigest)?,
        };
        state.validate_shape()?;
        state.identity = runtime_context_state_identity(&state)?;
        Ok(state)
    }

    fn from_recorded(
        affinity: ModelCacheAffinity,
        occupancy_anchor: Option<ContextAnchor>,
        request_anchor: Option<plakous_model::ContextRequestAnchor>,
        transition: Option<RuntimeContextTransition>,
        compaction: Option<CompactionProvenance>,
        identity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        let state = Self {
            affinity,
            occupancy_anchor,
            request_anchor,
            transition,
            compaction,
            identity,
        };
        state.validate_shape()?;
        if runtime_context_state_identity(&state)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        Ok(state)
    }

    fn validate_shape(&self) -> Result<(), RuntimeManifestError> {
        validate_digest(self.affinity.identity(), DigestDomain::CacheAffinity)?;
        if let Some(transition) = &self.transition {
            transition.validate()?;
            if transition.target_session != self.affinity.session()
                || transition.target_branch != self.affinity.branch()
                || transition.target_epoch != self.affinity.epoch()
                || transition.target_affinity != self.affinity.identity()
            {
                return Err(RuntimeManifestError::InvalidContextTransition);
            }
        }
        if let Some(provenance) = &self.compaction {
            provenance.validate()?;
        }
        if self.transition.as_ref().is_some_and(|transition| {
            transition.kind == RuntimeContextTransitionKind::SuccessfulCompaction
                && self.compaction.as_ref().is_none_or(|provenance| {
                    provenance.source_checkpoint != transition.source_checkpoint
                        || provenance.invalidation.source_affinity != transition.source_affinity
                        || provenance.invalidation.target_affinity != transition.target_affinity
                        || provenance.invalidation.source_epoch != transition.source_epoch
                        || provenance.invalidation.target_epoch != transition.target_epoch
                })
        }) {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    fn validate_history(&self, history: &CanonicalHistory) -> Result<(), RuntimeManifestError> {
        if let Some(provenance) = &self.compaction {
            provenance.validate_history_prefix(history)?;
        }
        Ok(())
    }

    /// Verifies exact binding to one manifest topology.
    pub fn validate_binding(
        &self,
        session: SessionId,
        branch: BranchId,
    ) -> Result<(), RuntimeManifestError> {
        self.validate_shape()?;
        if self.affinity.session() != session || self.affinity.branch() != branch {
            return Err(RuntimeManifestError::InvalidContextState);
        }
        Ok(())
    }

    /// Verifies a store-owned safe branch transition against the exact source.
    pub fn validate_branch_transition_from(
        &self,
        source: &Self,
        source_checkpoint: CheckpointId,
        expected: RuntimeContextTransitionKind,
    ) -> Result<(), RuntimeManifestError> {
        if !matches!(
            expected,
            RuntimeContextTransitionKind::OrdinaryFork | RuntimeContextTransitionKind::Rebind
        ) || self.compaction != source.compaction
            || self.occupancy_anchor.is_some()
            || self.request_anchor.is_some()
        {
            return Err(RuntimeManifestError::InvalidContextTransition);
        }
        let proof = self
            .transition
            .as_ref()
            .ok_or(RuntimeManifestError::InvalidContextTransition)?;
        if proof.kind != expected
            || proof.source_session != source.affinity.session()
            || proof.source_branch != source.affinity.branch()
            || proof.source_checkpoint != source_checkpoint
            || proof.source_context != source.identity
            || proof.source_affinity != source.affinity.identity()
            || proof.source_epoch != source.affinity.epoch()
            || proof.target_session != self.affinity.session()
            || proof.target_branch != self.affinity.branch()
            || proof.target_affinity != self.affinity.identity()
            || proof.target_epoch != self.affinity.epoch()
            || proof.source_affinity == proof.target_affinity
        {
            return Err(RuntimeManifestError::InvalidContextTransition);
        }
        Ok(())
    }

    /// Returns the sole bound cache affinity.
    #[must_use]
    pub const fn affinity(&self) -> &ModelCacheAffinity {
        &self.affinity
    }

    /// Returns the context-measure anchor recorded at the last turn terminal.
    #[must_use]
    pub const fn occupancy_anchor(&self) -> Option<ContextAnchor> {
        self.occupancy_anchor
    }

    /// Returns the last closed rotation proof, when a rotation has occurred.
    #[must_use]
    pub const fn transition(&self) -> Option<&RuntimeContextTransition> {
        self.transition.as_ref()
    }

    /// Returns the latest committed compaction provenance, when present.
    #[must_use]
    pub const fn compaction_provenance(&self) -> Option<&CompactionProvenance> {
        self.compaction.as_ref()
    }

    /// Returns the recomputed complete context-state identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Encodes this typed state with its bounded canonical codec.
    pub fn encode(&self) -> Result<Vec<u8>, RuntimeManifestError> {
        encode_runtime_context_state(self)
    }

    /// Decodes and fully revalidates this typed state.
    pub fn decode(bytes: &[u8]) -> Result<Self, RuntimeManifestError> {
        decode_runtime_context_state(bytes)
    }
}

impl fmt::Debug for RuntimeContextState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeContextState")
            .field("session", &self.affinity.session())
            .field("branch", &self.affinity.branch())
            .field("epoch", &self.affinity.epoch())
            .field("affinity", &"<redacted>")
            .field("occupancy_anchor", &self.occupancy_anchor)
            .field("transition", &self.transition)
            .field("compaction", &self.compaction)
            .finish_non_exhaustive()
    }
}

/// Stable prefix, canonical history, bounded working projection, and context state.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeCanonicalState {
    stable_prefix: StableRequestPrefix,
    history: CanonicalHistory,
    working: WorkingProjection,
    context: RuntimeContextState,
}

impl RuntimeCanonicalState {
    /// Captures exact canonical/model-visible reconstruction data.
    pub fn new(
        stable_prefix: StableRequestPrefix,
        history: CanonicalHistory,
        working: WorkingProjection,
        context: RuntimeContextState,
    ) -> Result<Self, RuntimeManifestError> {
        context.validate_history(&history)?;
        Ok(Self {
            stable_prefix,
            history,
            working,
            context,
        })
    }

    /// Returns the exact stable request prefix.
    #[must_use]
    pub const fn stable_prefix(&self) -> &StableRequestPrefix {
        &self.stable_prefix
    }

    /// Returns committed canonical history.
    #[must_use]
    pub const fn history(&self) -> &CanonicalHistory {
        &self.history
    }

    /// Returns the bounded working projection.
    #[must_use]
    pub const fn working(&self) -> &WorkingProjection {
        &self.working
    }

    /// Returns exact context/compaction facts.
    #[must_use]
    pub const fn context(&self) -> &RuntimeContextState {
        &self.context
    }

    /// Returns the exact committed-plus-working model-visible projection.
    #[must_use]
    pub fn projection(&self) -> ConversationView {
        ConversationView::from_shared(self.history.clone(), self.working.clone())
    }
}

/// Exact optional native family artifact retained as mandatory bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct FamilyArtifact {
    family: String,
    schema_revision: u32,
    codec_revision: u32,
    binding: Digest,
    binding_component: ReconstructiveComponent,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

impl FamilyArtifact {
    /// Captures exact family-native bytes. The family kernel remains their validator.
    pub fn new(
        family: impl Into<String>,
        schema_revision: u32,
        codec_revision: u32,
        binding: Digest,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, RuntimeManifestError> {
        if schema_revision == 0 || codec_revision == 0 {
            return Err(RuntimeManifestError::ZeroRevision);
        }
        validate_digest(binding, DigestDomain::ModelBinding)?;
        let family = portable_id(family.into(), "protocol family")?;
        if !supported_family_artifact_revision(&family, schema_revision, codec_revision) {
            return Err(RuntimeManifestError::RevisionDisagreement);
        }
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAX_RUNTIME_COMPONENT_BYTES {
            return Err(RuntimeManifestError::ComponentSize);
        }
        let binding_component = family_binding_component(binding)?;
        let integrity = family_artifact_integrity(
            &family,
            schema_revision,
            codec_revision,
            &binding_component,
            &bytes,
        )?;
        Ok(Self {
            family,
            schema_revision,
            codec_revision,
            binding,
            binding_component,
            bytes,
            integrity,
        })
    }

    fn from_recorded(
        family: String,
        schema_revision: u32,
        codec_revision: u32,
        binding_component: &ReconstructiveComponent,
        bytes: Arc<[u8]>,
        integrity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        let binding = family_binding_digest(binding_component)?;
        let artifact = Self::new(family, schema_revision, codec_revision, binding, bytes)?;
        if artifact.binding_component != *binding_component {
            return Err(RuntimeManifestError::Malformed);
        }
        if artifact.integrity != integrity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        Ok(artifact)
    }

    /// Converts one fully sealed terminal candidate without decoding family bytes.
    pub fn from_candidate(
        candidate: &SealedFamilyArtifactCandidate,
    ) -> Result<Self, RuntimeManifestError> {
        Self::new(
            candidate.family(),
            candidate.schema_revision(),
            candidate.codec_revision(),
            candidate.binding(),
            candidate.bytes(),
        )
    }

    /// Converts the durable envelope into preparation-independent model-call state.
    pub fn to_active(&self) -> Result<ActiveFamilyArtifact, RuntimeManifestError> {
        ActiveFamilyArtifact::new(
            self.family.clone(),
            self.schema_revision,
            self.codec_revision,
            self.binding,
            Arc::clone(&self.bytes),
        )
        .map_err(|_| RuntimeManifestError::Malformed)
    }

    /// Returns the family identity (for example `responses`).
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Returns the exact artifact bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the exact producing artifact binding.
    #[must_use]
    pub const fn binding(&self) -> Digest {
        self.binding
    }

    pub(crate) const fn binding_component(&self) -> &ReconstructiveComponent {
        &self.binding_component
    }

    /// Returns the behavior-bearing family schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }

    /// Returns the exact family codec revision.
    #[must_use]
    pub const fn codec_revision(&self) -> u32 {
        self.codec_revision
    }

    pub(crate) const fn integrity(&self) -> Digest {
        self.integrity
    }
}

impl fmt::Debug for FamilyArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FamilyArtifact")
            .field("family", &self.family)
            .field("schema_revision", &self.schema_revision)
            .field("codec_revision", &self.codec_revision)
            .field("binding", &self.binding)
            .field("binding_component", &"<redacted binding component>")
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("integrity", &self.integrity)
            .finish()
    }
}

/// Durability contract selected for the checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeDurability {
    /// Required durable commit acknowledgement.
    Required,
    /// Truthful best-effort contract (initial `SQLite` may upgrade it).
    BestEffort,
    /// Process-memory only; never restart durable.
    Disabled,
}

/// Closed availability of one external stateful interpreter generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterpreterAvailability {
    /// The named generation is live and may accept a cell.
    Available,
    /// The previous process state is unavailable; source is never reconstructed or replayed.
    Reset,
}

/// Closed reason an interpreter generation was reset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterpreterResetReason {
    /// A new agent/process incarnation opened durable state.
    ProcessReopen,
    /// The host process or its owner was lost.
    HostLost,
    /// Exact host/client protocol validation failed.
    ProtocolViolation,
    /// Cancellation destroyed the generation.
    Cancellation,
    /// A bound attachment, catalog, or environment drifted.
    Drift,
    /// Agent shutdown joined the generation.
    AgentShutdown,
}

/// Last exact acknowledgement observed from the interpreter boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterpreterAcknowledgedBoundary {
    /// The exact generation launched and passed its handshake.
    Launch,
    /// The host acknowledged the named source immediately before evaluator entry.
    SourceAccepted,
    /// The host acknowledged one correlated child result.
    ChildResultAccepted,
    /// The host emitted the cell's sole terminal outcome.
    Terminal,
    /// Destruction completed with a joined process tree.
    Join,
}

/// Reconstruction-complete semantic observation of external interpreter state.
///
/// Evaluator memory, source, frames, and VM bytes are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterpreterObservation {
    generation: u64,
    availability: InterpreterAvailability,
    reset_reason: Option<InterpreterResetReason>,
    affected_cell: Option<String>,
    last_acknowledged: Option<InterpreterAcknowledgedBoundary>,
}

impl InterpreterObservation {
    /// Creates one validated closed observation.
    pub fn new(
        generation: u64,
        availability: InterpreterAvailability,
        reset_reason: Option<InterpreterResetReason>,
        affected_cell: Option<String>,
        last_acknowledged: Option<InterpreterAcknowledgedBoundary>,
    ) -> Result<Self, RuntimeManifestError> {
        if generation == 0
            || matches!(availability, InterpreterAvailability::Available) != reset_reason.is_none()
        {
            return Err(RuntimeManifestError::InvalidInterpreterObservation);
        }
        let affected_cell = affected_cell
            .map(|cell| bounded_text(cell, MAX_ID_BYTES, "interpreter affected cell"))
            .transpose()?;
        let cell_boundary = matches!(
            last_acknowledged,
            Some(
                InterpreterAcknowledgedBoundary::SourceAccepted
                    | InterpreterAcknowledgedBoundary::ChildResultAccepted
            )
        );
        let cell_consistent = cell_boundary == affected_cell.is_some()
            || matches!(availability, InterpreterAvailability::Reset)
                && last_acknowledged.is_none();
        let availability_consistent = match availability {
            InterpreterAvailability::Available => last_acknowledged
                .is_some_and(|boundary| !matches!(boundary, InterpreterAcknowledgedBoundary::Join)),
            InterpreterAvailability::Reset => true,
        };
        if !cell_consistent || !availability_consistent {
            return Err(RuntimeManifestError::InvalidInterpreterObservation);
        }
        Ok(Self {
            generation,
            availability,
            reset_reason,
            affected_cell,
            last_acknowledged,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub const fn availability(&self) -> InterpreterAvailability {
        self.availability
    }
    #[must_use]
    pub const fn reset_reason(&self) -> Option<InterpreterResetReason> {
        self.reset_reason
    }
    #[must_use]
    pub fn affected_cell(&self) -> Option<&str> {
        self.affected_cell.as_deref()
    }
    #[must_use]
    pub const fn last_acknowledged(&self) -> Option<InterpreterAcknowledgedBoundary> {
        self.last_acknowledged
    }
}

/// One reopen that applied a freshly derived output bound in place of the one
/// the session was created under.
///
/// A bound the product derives from a route's attestation moves when the
/// provider's catalog moves. Reopen re-derives it rather than refusing the
/// session, and records the change here so it reaches the transcript instead of
/// being applied silently: the output contract of the session did change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputBoundRederived {
    previous: Option<std::num::NonZeroU32>,
    current: Option<std::num::NonZeroU32>,
}

impl OutputBoundRederived {
    /// The bound the session was created under.
    #[must_use]
    pub const fn previous(self) -> Option<std::num::NonZeroU32> {
        self.previous
    }

    /// The bound this reopen derived and will use.
    #[must_use]
    pub const fn current(self) -> Option<std::num::NonZeroU32> {
        self.current
    }
}

/// Reconstruction-complete runtime manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeManifest {
    output_bound_rederived: Option<OutputBoundRederived>,
    provenance: ProducerProvenance,
    topology: RuntimeTopology,
    lifecycle: RuntimeLifecycle,
    harness_plan: ReconstructiveComponent,
    provider_configuration: CompiledProviderConfiguration,
    canonical: RuntimeCanonicalState,
    binding: ModelRouteBinding,
    tools: ToolRuntimeManifest,
    ledger: RuntimeOperationLedger,
    family_artifact: Option<FamilyArtifact>,
    interpreter_observation: Option<InterpreterObservation>,
    jobs: JobLedger,
    durability: RuntimeDurability,
}

impl RuntimeManifest {
    /// Creates and validates the one canonical manifest aggregate.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the closed manifest groups"
    )]
    pub fn new(
        provenance: ProducerProvenance,
        topology: RuntimeTopology,
        lifecycle: RuntimeLifecycle,
        harness_plan: ReconstructiveComponent,
        provider_configuration: CompiledProviderConfiguration,
        canonical: RuntimeCanonicalState,
        binding: ModelRouteBinding,
        tools: ToolRuntimeManifest,
        ledger: RuntimeOperationLedger,
        family_artifact: Option<FamilyArtifact>,
        interpreter_observation: Option<InterpreterObservation>,
        jobs: JobLedger,
        durability: RuntimeDurability,
    ) -> Result<Self, RuntimeManifestError> {
        topology.validate()?;
        expect_component(&harness_plan, RuntimeComponentKind::HarnessPlan)?;
        if harness_plan.codec_revision() != HARNESS_PLAN_COMPONENT_FORMAT_VERSION {
            return Err(RuntimeManifestError::RevisionDisagreement);
        }
        if !matches!(
            (
                tools.registry().codec_revision(),
                tools.exposure().codec_revision()
            ),
            (
                TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
                TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION
            ) | (
                TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
                TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION
            )
        ) {
            return Err(RuntimeManifestError::RevisionDisagreement);
        }
        let plan = match rehydrate_harness_plan(&harness_plan.bytes, RUNTIME_POLICY_REVISION)? {
            HarnessPlanReadiness::Exact(plan) => plan,
            HarnessPlanReadiness::Mismatch(_) => {
                return Err(RuntimeManifestError::UnsupportedComponent);
            }
        };
        let output_bound_rederived = validate_provider_configuration(
            &provider_configuration,
            &canonical,
            &binding,
            &plan,
            durability,
        )?;
        if family_artifact.as_ref().is_some_and(|artifact| {
            !supported_family_artifact_revision(
                &artifact.family,
                artifact.schema_revision,
                artifact.codec_revision,
            )
        }) {
            return Err(RuntimeManifestError::RevisionDisagreement);
        }
        canonical
            .context
            .validate_binding(topology.session_id, topology.branch_id)?;
        validate_composition_graph(&ledger)?;
        validate_lifecycle(&lifecycle, &canonical, &ledger)?;
        Ok(Self {
            output_bound_rederived,
            provenance,
            topology,
            lifecycle,
            harness_plan,
            provider_configuration,
            canonical,
            binding,
            tools,
            ledger,
            family_artifact,
            interpreter_observation,
            jobs,
            durability,
        })
    }

    /// Returns the output-bound re-derivation this reopen applied, if any.
    ///
    /// `None` on a fresh session and on a reopen whose derived bound is the one
    /// the session already had.
    #[must_use]
    pub const fn output_bound_rederived(&self) -> Option<OutputBoundRederived> {
        self.output_bound_rederived
    }

    /// Returns session-owned jobs, which are not recovery obligations.
    #[must_use]
    pub const fn jobs(&self) -> &JobLedger {
        &self.jobs
    }

    /// Returns producing provenance (never a compatibility gate).
    #[must_use]
    pub const fn provenance(&self) -> &ProducerProvenance {
        &self.provenance
    }

    /// Returns immutable checkpoint topology.
    #[must_use]
    pub const fn topology(&self) -> RuntimeTopology {
        self.topology
    }

    /// Returns quiescent or recovery-only working state.
    #[must_use]
    pub const fn lifecycle(&self) -> &RuntimeLifecycle {
        &self.lifecycle
    }

    /// Returns exact canonical/model-visible state.
    #[must_use]
    pub const fn canonical(&self) -> &RuntimeCanonicalState {
        &self.canonical
    }

    /// Returns the complete selected and effective provider configuration.
    #[must_use]
    pub const fn provider_configuration(&self) -> &CompiledProviderConfiguration {
        &self.provider_configuration
    }

    /// Returns the exact provider binding.
    #[must_use]
    pub const fn binding(&self) -> &ModelRouteBinding {
        &self.binding
    }

    /// Returns exact tool/environment facts.
    #[must_use]
    pub const fn tools(&self) -> &ToolRuntimeManifest {
        &self.tools
    }

    /// Returns the exact operation ledger.
    #[must_use]
    pub const fn ledger(&self) -> &RuntimeOperationLedger {
        &self.ledger
    }

    /// Returns exact optional family-native bytes.
    #[must_use]
    pub const fn family_artifact(&self) -> Option<&FamilyArtifact> {
        self.family_artifact.as_ref()
    }

    /// Returns the optional stateful-interpreter semantic observation.
    #[must_use]
    pub const fn interpreter_observation(&self) -> Option<&InterpreterObservation> {
        self.interpreter_observation.as_ref()
    }

    /// Returns the declared persistence guarantee.
    #[must_use]
    pub const fn durability(&self) -> RuntimeDurability {
        self.durability
    }

    /// Returns the exact reconstruction-complete harness-plan component.
    #[must_use]
    pub const fn harness_plan_component(&self) -> &ReconstructiveComponent {
        &self.harness_plan
    }

    /// Reconstructs the exact currently implemented harness plan.
    pub(crate) fn rehydrate_harness_plan(
        &self,
    ) -> Result<ResolvedHarnessPlan, RuntimeManifestError> {
        match rehydrate_harness_plan(self.harness_plan.bytes(), RUNTIME_POLICY_REVISION)? {
            HarnessPlanReadiness::Exact(plan) => Ok(plan),
            HarnessPlanReadiness::Mismatch(_) => Err(RuntimeManifestError::UnsupportedComponent),
        }
    }
}

/// Compatibility class of one immutable checkpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RuntimeCheckpointKind {
    /// Safe ordinary resume/fork/rebind boundary.
    Quiescent,
    /// Recovery-only boundary with unresolved obligations.
    Working,
}

/// Sole immutable checkpoint authority plus its integrity identity.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeCheckpoint {
    manifest: Arc<RuntimeManifest>,
    integrity: Digest,
}

impl RuntimeCheckpoint {
    /// Creates a checkpoint around one complete manifest.
    pub fn new(manifest: RuntimeManifest) -> Result<Self, RuntimeManifestError> {
        let integrity = checkpoint_compositional_integrity(&manifest)?;
        Ok(Self {
            manifest: Arc::new(manifest),
            integrity,
        })
    }

    /// Returns the complete manifest.
    #[must_use]
    pub fn manifest(&self) -> &RuntimeManifest {
        self.manifest.as_ref()
    }

    /// Returns the immutable checkpoint identity.
    #[must_use]
    pub fn id(&self) -> CheckpointId {
        self.manifest.topology.checkpoint_id
    }

    /// Returns quiescent or working compatibility class.
    #[must_use]
    pub fn kind(&self) -> RuntimeCheckpointKind {
        self.manifest.lifecycle.checkpoint_kind()
    }

    /// Returns checkpoint integrity evidence.
    #[must_use]
    pub const fn integrity(&self) -> Digest {
        self.integrity
    }

    /// Returns the semantic commit operation identity the store must receipt.
    #[must_use]
    pub fn checkpoint_commit_identity(&self) -> Digest {
        self.manifest.topology.commit_identity
    }
}

/// Typed fail-closed manifest/checkpoint error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RuntimeManifestError {
    /// A behavior-bearing revision used reserved zero.
    #[error("runtime semantic revision must be nonzero")]
    ZeroRevision,
    /// The root codec/digest revision is not implemented by this decoder.
    #[error("runtime root codec revision is unsupported")]
    UnsupportedRootRevision,
    /// Related exact revisions disagree.
    #[error("runtime semantic revisions disagree")]
    RevisionDisagreement,
    /// An externally owned component has no bytes or exceeds its hard bound.
    #[error("runtime reconstruction component violates its byte bound")]
    ComponentSize,
    /// An embedded component occupied the wrong closed field.
    #[error("runtime reconstruction component has the wrong kind")]
    ComponentKind,
    /// A complete root envelope exceeds its hard bound.
    #[error("runtime canonical envelope exceeds its byte bound")]
    EnvelopeSize,
    /// A vector exceeds the fixed semantic record bound.
    #[error("runtime canonical record bound was exceeded")]
    RecordLimit,
    /// A bounded identity or provenance value is invalid.
    #[error("runtime canonical text is invalid: {field}")]
    InvalidText {
        /// Stable field name.
        field: &'static str,
    },
    /// Interpreter availability, generation, or reset facts contradict one another.
    #[error("runtime interpreter observation is invalid")]
    InvalidInterpreterObservation,
    /// Topology or lineage is impossible.
    #[error("runtime checkpoint topology is invalid")]
    InvalidTopology,
    /// Historical revalidation facts contradict their timestamps or kinds.
    #[error("runtime revalidation fact is invalid")]
    InvalidRevalidation,
    /// Ledger records or transitions are contradictory/unreachable.
    #[error("runtime operation ledger is invalid")]
    InvalidLedger,
    /// Lifecycle and recovery obligations are partial or contradictory.
    #[error("runtime checkpoint lifecycle is invalid")]
    InvalidLifecycle,
    /// Typed context state is malformed or bound to another branch.
    #[error("runtime context state is invalid")]
    InvalidContextState,
    /// A cache/context rotation is stale, incomplete, or unauthorized.
    #[error("runtime context transition is invalid")]
    InvalidContextTransition,
    /// Compaction provenance is partial or contradicts canonical history.
    #[error("runtime compaction provenance is invalid")]
    InvalidCompactionProvenance,
    /// A digest has the wrong domain, algorithm, version, or bytes.
    #[error("runtime canonical digest is invalid")]
    InvalidDigest,
    /// Envelope or component integrity disagrees with exact bytes.
    #[error("runtime canonical integrity mismatch")]
    IntegrityMismatch,
    /// Envelope magic is not the requested canonical type.
    #[error("runtime canonical envelope magic is invalid")]
    InvalidMagic,
    /// A fixed root version is unavailable.
    #[error("runtime canonical envelope version {found} is unsupported")]
    UnsupportedVersion {
        /// Encoded root version.
        found: u32,
    },
    /// A closed discriminant is unavailable.
    #[error("unknown runtime canonical {field} tag {tag}")]
    UnknownTag {
        /// Closed field name.
        field: &'static str,
        /// Unknown encoded tag.
        tag: u8,
    },
    /// Bytes are truncated or carry an impossible length/value.
    #[error("runtime canonical encoding is malformed")]
    Malformed,
    /// Bytes remain after the sole canonical value.
    #[error("runtime canonical encoding has trailing bytes")]
    TrailingBytes,
    /// Model-owned canonical bytes failed exact reconstruction.
    #[error("runtime canonical model component is invalid")]
    InvalidModelComponent,
    /// Persisted provider configuration is malformed or disagrees with bound authority.
    #[error("runtime provider configuration is invalid or disagrees with its bindings")]
    InvalidProviderConfiguration,
    /// Stored bytes require a semantic component not compiled into this build.
    #[error("runtime semantic component is unavailable")]
    UnsupportedComponent,
    /// Harness-plan component framing or validation failed.
    #[error("runtime harness-plan component is invalid")]
    HarnessPlan,
}

impl From<crate::HarnessManifestCodecError> for RuntimeManifestError {
    fn from(_: crate::HarnessManifestCodecError) -> Self {
        Self::HarnessPlan
    }
}

fn validate_operation_transitions(
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

fn validate_lifecycle(
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

fn validate_lifecycle_shape(lifecycle: &RuntimeLifecycle) -> Result<(), RuntimeManifestError> {
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

fn expect_component(
    component: &ReconstructiveComponent,
    expected: RuntimeComponentKind,
) -> Result<(), RuntimeManifestError> {
    if component.kind == expected {
        Ok(())
    } else {
        Err(RuntimeManifestError::ComponentKind)
    }
}

fn component_integrity(
    kind: RuntimeComponentKind,
    revision: u32,
    bytes: &[u8],
) -> Result<Digest, RuntimeManifestError> {
    Digest::sha256(
        DigestDomain::Blob,
        INTEGRITY_REVISION,
        &[
            b"plakous-agent/runtime-component-v1",
            &[kind as u8],
            &revision.to_be_bytes(),
            bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn family_binding_component(
    binding: Digest,
) -> Result<ReconstructiveComponent, RuntimeManifestError> {
    validate_digest(binding, DigestDomain::ModelBinding)?;
    let bytes = serde_json::to_vec(&binding).map_err(|_| RuntimeManifestError::Malformed)?;
    ReconstructiveComponent::new(RuntimeComponentKind::FamilyBinding, 1, bytes)
}

fn family_binding_digest(
    component: &ReconstructiveComponent,
) -> Result<Digest, RuntimeManifestError> {
    expect_component(component, RuntimeComponentKind::FamilyBinding)?;
    if component.codec_revision() != 1 {
        return Err(RuntimeManifestError::RevisionDisagreement);
    }
    let binding: Digest =
        serde_json::from_slice(component.bytes()).map_err(|_| RuntimeManifestError::Malformed)?;
    validate_digest(binding, DigestDomain::ModelBinding)?;
    Ok(binding)
}

fn family_artifact_integrity(
    family: &str,
    schema: u32,
    codec: u32,
    binding: &ReconstructiveComponent,
    bytes: &[u8],
) -> Result<Digest, RuntimeManifestError> {
    Digest::sha256(
        DigestDomain::Blob,
        INTEGRITY_REVISION,
        &[
            b"plakous-agent/family-artifact-v1",
            family.as_bytes(),
            &schema.to_be_bytes(),
            &codec.to_be_bytes(),
            binding.integrity.bytes(),
            bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn supported_family_artifact_revision(family: &str, schema: u32, codec: u32) -> bool {
    matches!(
        (family, schema, codec),
        (
            "responses",
            RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
            RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION
        ) | (
            "messages",
            MESSAGES_FAMILY_ARTIFACT_SCHEMA_REVISION,
            MESSAGES_FAMILY_ARTIFACT_CODEC_REVISION
        )
    )
}

fn root_integrity(tag: &[u8], bytes: &[u8]) -> Result<Digest, RuntimeManifestError> {
    Digest::sha256(
        DigestDomain::Blob,
        INTEGRITY_REVISION,
        &[b"plakous-agent/runtime-root-v1", tag, bytes],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn checkpoint_compositional_integrity(
    manifest: &RuntimeManifest,
) -> Result<Digest, RuntimeManifestError> {
    let metadata = root_integrity(
        b"runtime-checkpoint-metadata",
        &encode_bundle_metadata(manifest)?,
    )?;
    let canonical = canonical_group_integrity(&manifest.canonical)?;
    let binding = binding_group_integrity(&manifest.binding)?;
    let provider_configuration =
        provider_configuration_integrity(&manifest.provider_configuration)?;
    let tools = tool_group_integrity(&manifest.tools)?;
    let ledger = ledger_group_integrity(&manifest.ledger)?;
    let family_present = [u8::from(manifest.family_artifact.is_some())];
    Digest::sha256(
        DigestDomain::Blob,
        INTEGRITY_REVISION,
        &[
            b"plakous-agent/runtime-checkpoint-composition-v1",
            metadata.bytes(),
            manifest.harness_plan.integrity.bytes(),
            provider_configuration.bytes(),
            canonical.bytes(),
            binding.bytes(),
            tools.bytes(),
            ledger.bytes(),
            &family_present,
            manifest
                .family_artifact
                .as_ref()
                .map_or(&[][..], |artifact| artifact.integrity.bytes()),
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn provider_configuration_integrity(
    configuration: &CompiledProviderConfiguration,
) -> Result<Digest, RuntimeManifestError> {
    root_integrity(
        b"runtime-provider-configuration",
        &encode_bundle_provider_configuration(configuration)?,
    )
}

fn validate_provider_configuration(
    configuration: &CompiledProviderConfiguration,
    canonical: &RuntimeCanonicalState,
    binding: &ModelRouteBinding,
    plan: &ResolvedHarnessPlan,
    durability: RuntimeDurability,
) -> Result<Option<OutputBoundRederived>, RuntimeManifestError> {
    let prefix = canonical.stable_prefix();
    let descriptor = configuration.effective().descriptor();
    let descriptor_binding = descriptor
        .semantic_binding_digest()
        .map_err(|_| RuntimeManifestError::InvalidProviderConfiguration)?;
    let behavior = configuration.effective().behavior();
    let reasoning = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => ReasoningPolicy::Disabled,
        ReasoningEffortDisposition::Enabled(effort) => ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: plakous_model::CapabilityRequirement::Required,
        },
    };
    let output_bound = behavior.output_bound.bound();
    let configured_tools = behavior.tools.iter();
    // A bound the product derives from the route's attestation is digested like
    // every other field, so a route that moved its figure digests to a
    // different identity. A reopen is therefore compiled with the figure the
    // session was created under, which makes the comparison below exact for
    // every session that reaches here. What this substitution still covers is a
    // session created under a product-chosen figure on a route that has since
    // started deriving: its persisted identity carries that figure, so it is
    // admitted when it is the identity this very configuration would carry
    // under it, and everything else still has to match exactly.
    let derived_bound = behavior.output_bound.is_derived_from_route();
    let (selected, effective) = configuration
        .reopen_identities(
            prefix.limits().max_output_tokens(),
            (
                prefix.selected_provider_configuration_id(),
                prefix.effective_provider_configuration_id(),
            ),
        )
        .map_err(|_| RuntimeManifestError::InvalidProviderConfiguration)?;
    if prefix.selected_provider_configuration_id() != selected
        || prefix.effective_provider_configuration_id() != effective
        || plan.selected_provider_configuration_id() != selected
        || plan.effective_provider_configuration_id() != effective
        || descriptor_binding != binding.model_binding
        || descriptor.protocol() != &binding.protocol
        || descriptor.route() != &binding.route
        || descriptor.requested_model() != &binding.requested_model
        || descriptor.reported_model() != binding.reported_model.as_ref()
        || descriptor.model_family() != binding.model_family.as_ref()
        || descriptor.profile() != &binding.adapter
        || prefix.reasoning() != reasoning
        || prefix.output() != &behavior.output
        || prefix.tool_choice() != &behavior.tool_choice
        || prefix.cache() != behavior.cache
        || (!derived_bound && prefix.limits().max_output_tokens() != output_bound)
        || !prefix.limits().stop_sequences().is_empty()
        || prefix.context_admission_requirement() != behavior.context_evidence_requirement
        || !prefix.tools().iter().eq(configured_tools)
        || prefix.hosted_tools() != behavior.hosted_tools
        || !matches!(
            (behavior.local_durability, durability),
            (
                LocalDurabilityDisposition::Ephemeral,
                RuntimeDurability::Disabled
            ) | (
                LocalDurabilityDisposition::BestEffort,
                RuntimeDurability::BestEffort
            ) | (
                LocalDurabilityDisposition::DurableRequired,
                RuntimeDurability::Required
            )
        )
    {
        return Err(RuntimeManifestError::InvalidProviderConfiguration);
    }
    let persisted_bound = prefix.limits().max_output_tokens();
    Ok(
        (derived_bound && persisted_bound != output_bound).then_some(OutputBoundRederived {
            previous: persisted_bound,
            current: output_bound,
        }),
    )
}

fn runtime_context_transition_identity(
    transition: &RuntimeContextTransition,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    write_runtime_context_transition_fields(&mut writer, transition)?;
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-agent/runtime-context-transition-v2",
            &writer.bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn compaction_provenance_identity(
    provenance: &CompactionProvenance,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    write_compaction_provenance_fields(&mut writer, provenance)?;
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-agent/runtime-compaction-provenance-v2",
            &writer.bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn runtime_context_state_identity(
    state: &RuntimeContextState,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.id(state.affinity.session().as_uuid());
    writer.id(state.affinity.branch().as_uuid());
    writer.u64(state.affinity.epoch());
    writer.digest(state.affinity.identity())?;
    write_occupancy_anchor(&mut writer, state.occupancy_anchor);
    write_request_anchor(&mut writer, state.request_anchor.as_ref())?;
    writer.optional(state.transition.is_some());
    if let Some(transition) = &state.transition {
        writer.digest(transition.identity)?;
    }
    writer.optional(state.compaction.is_some());
    if let Some(compaction) = &state.compaction {
        writer.digest(compaction.identity)?;
    }
    Digest::sha256(
        DigestDomain::ContextBudget,
        3,
        &[b"plakous-agent/runtime-context-state-v3", &writer.bytes],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn write_request_anchor(
    writer: &mut Writer,
    anchor: Option<&plakous_model::ContextRequestAnchor>,
) -> Result<(), RuntimeManifestError> {
    writer.optional(anchor.is_some());
    if let Some(anchor) = anchor {
        let bytes =
            serde_json::to_vec(anchor).map_err(|_| RuntimeManifestError::InvalidContextState)?;
        writer.raw(&bytes, MAX_RUNTIME_CONTEXT_STATE_BYTES)?;
    }
    Ok(())
}

fn read_request_anchor(
    reader: &mut Reader<'_>,
) -> Result<Option<plakous_model::ContextRequestAnchor>, RuntimeManifestError> {
    if !reader.optional()? {
        return Ok(None);
    }
    let bytes = reader.raw(MAX_RUNTIME_CONTEXT_STATE_BYTES)?;
    let anchor: plakous_model::ContextRequestAnchor =
        serde_json::from_slice(&bytes).map_err(|_| RuntimeManifestError::InvalidContextState)?;
    if serde_json::to_vec(&anchor).map_err(|_| RuntimeManifestError::InvalidContextState)?
        != bytes.as_ref()
    {
        return Err(RuntimeManifestError::InvalidContextState);
    }
    Ok(Some(anchor))
}

fn write_occupancy_anchor(writer: &mut Writer, anchor: Option<ContextAnchor>) {
    writer.optional(anchor.is_some());
    if let Some(anchor) = anchor {
        writer.u64(anchor.provider_input_tokens());
        writer.u64(anchor.request_estimate_tokens());
    }
}

fn read_occupancy_anchor(
    reader: &mut Reader<'_>,
) -> Result<Option<ContextAnchor>, RuntimeManifestError> {
    if !reader.optional()? {
        return Ok(None);
    }
    let provider_input_tokens = reader.u64()?;
    let request_estimate_tokens = reader.u64()?;
    ContextAnchor::new(provider_input_tokens, request_estimate_tokens)
        .map(Some)
        .ok_or(RuntimeManifestError::InvalidContextState)
}

fn write_runtime_context_transition_fields(
    writer: &mut Writer,
    transition: &RuntimeContextTransition,
) -> Result<(), RuntimeManifestError> {
    writer.u8(transition.kind.tag());
    writer.id(transition.source_session.as_uuid());
    writer.id(transition.source_branch.as_uuid());
    writer.id(transition.source_checkpoint.as_uuid());
    writer.digest(transition.source_context)?;
    writer.digest(transition.source_affinity)?;
    writer.u64(transition.source_epoch);
    writer.id(transition.target_session.as_uuid());
    writer.id(transition.target_branch.as_uuid());
    writer.digest(transition.target_affinity)?;
    writer.u64(transition.target_epoch);
    Ok(())
}

fn write_context_evaluation_audit(
    writer: &mut Writer,
    audit: ContextEvaluationAudit,
) -> Result<(), RuntimeManifestError> {
    writer.digest(audit.measurement)?;
    writer.digest(audit.status)?;
    writer.u64(audit.input_tokens);
    Ok(())
}

fn write_compaction_provenance_fields(
    writer: &mut Writer,
    provenance: &CompactionProvenance,
) -> Result<(), RuntimeManifestError> {
    writer.id(provenance.source_checkpoint.as_uuid());
    writer.digest(provenance.source_history)?;
    writer.digest(provenance.operation)?;
    writer.digest(provenance.route_binding)?;
    writer.digest(provenance.model_binding)?;
    writer.digest(provenance.harness_plan)?;
    writer.digest(provenance.selection_plan)?;
    writer.u8(provenance.strategy.tag());
    writer.u32(provenance.strategy_revision);
    writer.digest(provenance.source_projection)?;
    writer.u32(provenance.selected_start);
    writer.u32(provenance.selected_end);
    writer.u32(provenance.selected_group_count);
    writer.u32(provenance.selected_segment_count);
    writer.digest(provenance.selected_identity)?;
    writer.u32(provenance.retained_segment_count);
    writer.digest(provenance.retained_tail_identity)?;
    writer.digest(provenance.target_history)?;
    writer.u32(provenance.target_segment_count);
    writer.digest(provenance.target_projection)?;
    write_context_evaluation_audit(writer, provenance.before)?;
    write_context_evaluation_audit(writer, provenance.after)?;
    writer.digest(provenance.summary_segment)?;
    writer.optional(provenance.usage.is_some());
    if let Some(usage) = provenance.usage {
        writer.u64(usage.input_tokens());
        writer.u64(usage.output_tokens());
        writer.u64(usage.reasoning_tokens());
        writer.u64(usage.cached_input_tokens());
        writer.u64(usage.cache_write_tokens());
    }
    writer.digest(provenance.invalidation.source_affinity)?;
    writer.u64(provenance.invalidation.source_epoch);
    writer.digest(provenance.invalidation.target_affinity)?;
    writer.u64(provenance.invalidation.target_epoch);
    Ok(())
}

fn encode_runtime_context_state(
    state: &RuntimeContextState,
) -> Result<Vec<u8>, RuntimeManifestError> {
    state.validate_shape()?;
    let mut writer = Writer::default();
    let affinity = serde_json::to_vec(&state.affinity)
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
    writer.raw(&affinity, 4 * 1024)?;
    write_occupancy_anchor(&mut writer, state.occupancy_anchor);
    write_request_anchor(&mut writer, state.request_anchor.as_ref())?;
    writer.optional(state.transition.is_some());
    if let Some(transition) = &state.transition {
        write_runtime_context_transition_fields(&mut writer, transition)?;
        writer.digest(transition.identity)?;
    }
    writer.optional(state.compaction.is_some());
    if let Some(provenance) = &state.compaction {
        write_compaction_provenance_fields(&mut writer, provenance)?;
        writer.digest(provenance.identity)?;
    }
    writer.digest(state.identity)?;
    let integrity = root_integrity(b"runtime-context-state", &writer.bytes)?;
    encode_envelope(
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        &writer.bytes,
        integrity,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
    )
}

fn decode_runtime_context_state(bytes: &[u8]) -> Result<RuntimeContextState, RuntimeManifestError> {
    let body = decode_envelope(
        bytes,
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
        b"runtime-context-state",
    )?;
    let mut reader = Reader::new(body);
    let affinity_bytes = reader.raw(4 * 1024)?;
    let affinity: ModelCacheAffinity = serde_json::from_slice(&affinity_bytes)
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
    if serde_json::to_vec(&affinity).map_err(|_| RuntimeManifestError::InvalidContextState)?
        != affinity_bytes.as_ref()
    {
        return Err(RuntimeManifestError::InvalidContextState);
    }
    let occupancy_anchor = read_occupancy_anchor(&mut reader)?;
    let request_anchor = read_request_anchor(&mut reader)?;
    let transition = reader
        .optional()?
        .then(|| read_runtime_context_transition(&mut reader))
        .transpose()?;
    let compaction = reader
        .optional()?
        .then(|| read_compaction_provenance(&mut reader))
        .transpose()?;
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    reader.finish()?;
    RuntimeContextState::from_recorded(
        affinity,
        occupancy_anchor,
        request_anchor,
        transition,
        compaction,
        identity,
    )
}

fn read_runtime_context_transition(
    reader: &mut Reader<'_>,
) -> Result<RuntimeContextTransition, RuntimeManifestError> {
    let kind = RuntimeContextTransitionKind::from_tag(reader.u8()?)?;
    let source_session = reader.session_id()?;
    let source_branch = reader.branch_id()?;
    let source_checkpoint = reader.checkpoint_id()?;
    let source_context = reader.digest(Some(DigestDomain::ContextBudget))?;
    let source_affinity = reader.digest(Some(DigestDomain::CacheAffinity))?;
    let source_epoch = reader.u64()?;
    let target_session = reader.session_id()?;
    let target_branch = reader.branch_id()?;
    let target_affinity = reader.digest(Some(DigestDomain::CacheAffinity))?;
    let target_epoch = reader.u64()?;
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    RuntimeContextTransition::from_recorded(
        RuntimeContextTransition {
            kind,
            source_session,
            source_branch,
            source_checkpoint,
            source_context,
            source_affinity,
            source_epoch,
            target_session,
            target_branch,
            target_affinity,
            target_epoch,
            identity,
        },
        identity,
    )
}

fn read_context_evaluation_audit(
    reader: &mut Reader<'_>,
) -> Result<ContextEvaluationAudit, RuntimeManifestError> {
    ContextEvaluationAudit::new(
        reader.digest(Some(DigestDomain::TokenMeasurement))?,
        reader.digest(Some(DigestDomain::ContextAdmission))?,
        reader.u64()?,
    )
}

fn read_compaction_provenance(
    reader: &mut Reader<'_>,
) -> Result<CompactionProvenance, RuntimeManifestError> {
    let source_checkpoint = reader.checkpoint_id()?;
    let source_history = reader.digest(Some(DigestDomain::CanonicalHistory))?;
    let operation = reader.digest(Some(DigestDomain::ProviderOperation))?;
    let route_binding = reader.digest(Some(DigestDomain::ModelBinding))?;
    let model_binding = reader.digest(Some(DigestDomain::ModelBinding))?;
    let harness_plan = reader.digest(Some(DigestDomain::ResolvedHarnessPlan))?;
    let selection_plan = reader.digest(Some(DigestDomain::ContextPlan))?;
    let strategy = match reader.u8()? {
        1 => RuntimeCompactionStrategy::SemanticSummaryPrefixV1,
        2 => RuntimeCompactionStrategy::NativeResponsesTriggerV1,
        3 => RuntimeCompactionStrategy::ToolResultPlaceholdersV1,
        4 => RuntimeCompactionStrategy::ReplayOmissionV1,
        tag => {
            return Err(RuntimeManifestError::UnknownTag {
                field: "runtime compaction strategy",
                tag,
            });
        }
    };
    let strategy_revision = reader.u32()?;
    let source_projection = reader.digest(Some(DigestDomain::HistoryProjection))?;
    let selected_start = reader.u32()?;
    let selected_end = reader.u32()?;
    let selected_group_count = reader.u32()?;
    let selected_segment_count = reader.u32()?;
    let selected_identity = reader.digest(Some(DigestDomain::ContextPlan))?;
    let retained_segment_count = reader.u32()?;
    let retained_tail_identity = reader.digest(Some(DigestDomain::ContextPlan))?;
    let target_history = reader.digest(Some(DigestDomain::CanonicalHistory))?;
    let target_segment_count = reader.u32()?;
    let target_projection = reader.digest(Some(DigestDomain::HistoryProjection))?;
    let before = read_context_evaluation_audit(reader)?;
    let after = read_context_evaluation_audit(reader)?;
    let summary_segment = reader.digest(Some(DigestDomain::CanonicalHistorySegment))?;
    let usage = reader
        .optional()?
        .then(|| {
            Ok::<_, RuntimeManifestError>(Usage::new(
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
            ))
        })
        .transpose()?;
    let invalidation = CompactionInvalidation {
        source_affinity: reader.digest(Some(DigestDomain::CacheAffinity))?,
        source_epoch: reader.u64()?,
        target_affinity: reader.digest(Some(DigestDomain::CacheAffinity))?,
        target_epoch: reader.u64()?,
    };
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    CompactionProvenance::from_recorded(
        CompactionProvenance {
            source_checkpoint,
            source_history,
            operation,
            route_binding,
            model_binding,
            harness_plan,
            selection_plan,
            strategy,
            strategy_revision,
            source_projection,
            selected_start,
            selected_end,
            selected_group_count,
            selected_segment_count,
            selected_identity,
            retained_segment_count,
            retained_tail_identity,
            target_history,
            target_segment_count,
            target_projection,
            before,
            after,
            summary_segment,
            usage,
            invalidation,
            identity,
        },
        identity,
    )
}

fn canonical_group_integrity(
    canonical: &RuntimeCanonicalState,
) -> Result<Digest, RuntimeManifestError> {
    let mut working = Writer::default();
    let limits = canonical.working.limits();
    working.usize(limits.max_segments())?;
    working.usize(limits.max_items())?;
    working.usize(limits.max_retained_bytes())?;
    working.count(canonical.working.segments().len())?;
    for segment in canonical.working.segments() {
        working.digest(segment.identity())?;
    }
    let working = root_integrity(b"runtime-working-projection", &working.bytes)?;
    Digest::sha256(
        DigestDomain::Blob,
        INTEGRITY_REVISION,
        &[
            b"plakous-agent/runtime-canonical-composition-v1",
            canonical.stable_prefix.identity().bytes(),
            canonical.history.identity().bytes(),
            working.bytes(),
            canonical.context.identity.bytes(),
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn binding_group_integrity(binding: &ModelRouteBinding) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.string(binding.protocol.as_str(), MAX_ID_BYTES)?;
    writer.string(binding.route.as_str(), MAX_ID_BYTES)?;
    writer.string(binding.requested_model.as_str(), MAX_ID_BYTES)?;
    writer.optional(binding.reported_model.is_some());
    if let Some(model) = &binding.reported_model {
        writer.string(model.as_str(), MAX_ID_BYTES)?;
    }
    writer.optional(binding.model_family.is_some());
    if let Some(family) = &binding.model_family {
        writer.string(family.as_str(), MAX_ID_BYTES)?;
    }
    writer.string(binding.adapter.as_str(), MAX_ID_BYTES)?;
    writer.binding_reference(&binding.account)?;
    writer.binding_reference(&binding.profile)?;
    writer.digest(binding.model_binding)?;
    writer.digest(binding.descriptor.integrity)?;
    writer.binding_reference(&binding.admission.reference)?;
    writer.timestamp(binding.admission.observed_at);
    writer.optional(binding.admission.valid_until.is_some());
    if let Some(valid_until) = binding.admission.valid_until {
        writer.timestamp(valid_until);
    }
    writer.digest(binding.admission.evidence.integrity)?;
    root_integrity(b"runtime-binding-composition", &writer.bytes)
}

fn tool_group_integrity(tools: &ToolRuntimeManifest) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.digest(tools.registry.integrity)?;
    writer.digest(tools.exposure.integrity)?;
    writer.digest(tools.environment.integrity)?;
    writer.digest(tools.effect_policy.integrity)?;
    writer.binding_reference(&tools.policy)?;
    root_integrity(b"runtime-tool-composition", &writer.bytes)
}

fn ledger_group_integrity(ledger: &RuntimeOperationLedger) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.usize(ledger.command_count)?;
    writer.optional(ledger.command_tail.is_some());
    if let Some(command_tail) = &ledger.command_tail {
        writer.digest(command_tail.identity)?;
    }
    writer.optional(ledger.command_ids.is_some());
    if let Some(command_ids) = &ledger.command_ids {
        writer.digest(command_ids.identity)?;
    }
    writer.usize(ledger.operation_count)?;
    writer.usize(ledger.unresolved_operation_count())?;
    writer.optional(ledger.operation_root.is_some());
    if let Some(operation_root) = &ledger.operation_root {
        writer.digest(operation_root.identity)?;
    }
    root_integrity(b"runtime-ledger-composition", &writer.bytes)
}

fn validate_digest(digest: Digest, domain: DigestDomain) -> Result<(), RuntimeManifestError> {
    if digest.algorithm() == DigestAlgorithm::Sha256
        && digest.domain() == domain
        && digest.version() != 0
    {
        Ok(())
    } else {
        Err(RuntimeManifestError::InvalidDigest)
    }
}

fn portable_id(value: String, field: &'static str) -> Result<String, RuntimeManifestError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        Err(RuntimeManifestError::InvalidText { field })
    } else {
        Ok(value)
    }
}

fn bounded_text(
    value: String,
    maximum: usize,
    field: &'static str,
) -> Result<String, RuntimeManifestError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(RuntimeManifestError::InvalidText { field })
    } else {
        Ok(value)
    }
}

pub(crate) struct RuntimeManifestBundleMetadata {
    provenance: ProducerProvenance,
    topology: RuntimeTopology,
    lifecycle: RuntimeLifecycle,
    interpreter_observation: Option<InterpreterObservation>,
    jobs: JobLedger,
    durability: RuntimeDurability,
}

pub(crate) fn encode_bundle_metadata(
    manifest: &RuntimeManifest,
) -> Result<Vec<u8>, RuntimeManifestError> {
    encode_bundle_metadata_value(&RuntimeManifestBundleMetadata {
        provenance: manifest.provenance.clone(),
        topology: manifest.topology,
        lifecycle: manifest.lifecycle.clone(),
        interpreter_observation: manifest.interpreter_observation.clone(),
        jobs: manifest.jobs.clone(),
        durability: manifest.durability,
    })
}

pub(crate) fn encode_bundle_metadata_value(
    metadata: &RuntimeManifestBundleMetadata,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.string(&metadata.provenance.producer, MAX_PROVENANCE_BYTES)?;
    writer.string(&metadata.provenance.build, MAX_PROVENANCE_BYTES)?;
    writer.string(&metadata.provenance.features, MAX_PROVENANCE_BYTES)?;
    writer.topology(metadata.topology)?;
    writer.lifecycle(&metadata.lifecycle)?;
    writer.u32(metadata.jobs.next_id());
    writer.usize(metadata.jobs.rows().len())?;
    for row in metadata.jobs.rows() {
        writer.u32(row.number);
        writer.string(&row.command, 1024)?;
        writer.u64(row.started_at.seconds());
        writer.u32(row.started_at.nanoseconds());
        writer.string(&row.owner, 128)?;
        writer.bytes.extend_from_slice(&row.session_id.as_bytes());
        writer.u64(row.owner_generation.get());
        writer.optional(row.terminal.is_some());
        if let Some(terminal) = &row.terminal {
            writer.u8(match terminal.status {
                JobTerminalStatus::Exited => 1,
                JobTerminalStatus::Stopped => 2,
            });
            writer.optional(terminal.exit_code.is_some());
            if let Some(code) = terminal.exit_code {
                writer.u32(u32::from_be_bytes(code.to_be_bytes()));
            }
            writer.optional(terminal.stopper.is_some());
            if let Some(stopper) = terminal.stopper {
                writer.u8(match stopper {
                    plakous_tools::JobStopReason::Model => 1,
                    plakous_tools::JobStopReason::User => 2,
                    plakous_tools::JobStopReason::SessionExit => 3,
                    plakous_tools::JobStopReason::AttemptEnded => 4,
                    plakous_tools::JobStopReason::HardCap => 5,
                });
            }
            writer.u64(terminal.elapsed.as_secs());
            writer.u32(terminal.elapsed.subsec_nanos());
            writer.string(&terminal.tail, 4096)?;
        }
    }
    writer.optional(metadata.interpreter_observation.is_some());
    if let Some(observation) = &metadata.interpreter_observation {
        writer.u64(observation.generation);
        writer.u8(match observation.availability {
            InterpreterAvailability::Available => 1,
            InterpreterAvailability::Reset => 2,
        });
        writer.optional(observation.reset_reason.is_some());
        if let Some(reason) = observation.reset_reason {
            writer.u8(match reason {
                InterpreterResetReason::ProcessReopen => 1,
                InterpreterResetReason::HostLost => 2,
                InterpreterResetReason::ProtocolViolation => 3,
                InterpreterResetReason::Cancellation => 4,
                InterpreterResetReason::Drift => 5,
                InterpreterResetReason::AgentShutdown => 6,
            });
        }
        writer.optional(observation.affected_cell.is_some());
        if let Some(cell) = &observation.affected_cell {
            writer.string(cell, MAX_ID_BYTES)?;
        }
        writer.optional(observation.last_acknowledged.is_some());
        if let Some(boundary) = observation.last_acknowledged {
            writer.u8(match boundary {
                InterpreterAcknowledgedBoundary::Launch => 1,
                InterpreterAcknowledgedBoundary::SourceAccepted => 2,
                InterpreterAcknowledgedBoundary::ChildResultAccepted => 3,
                InterpreterAcknowledgedBoundary::Terminal => 4,
                InterpreterAcknowledgedBoundary::Join => 5,
            });
        }
    }
    writer.u8(match metadata.durability {
        RuntimeDurability::Required => 1,
        RuntimeDurability::BestEffort => 2,
        RuntimeDurability::Disabled => 3,
    });
    Ok(writer.bytes)
}

pub(crate) fn decode_bundle_metadata(
    bytes: &[u8],
) -> Result<RuntimeManifestBundleMetadata, RuntimeManifestError> {
    let mut reader = Reader::new(bytes);
    let provenance = ProducerProvenance::new(
        reader.string(MAX_PROVENANCE_BYTES)?,
        reader.string(MAX_PROVENANCE_BYTES)?,
        reader.string(MAX_PROVENANCE_BYTES)?,
    )?;
    let topology = reader.topology()?;
    let lifecycle = reader.lifecycle()?;
    let next_id = reader.u32()?;
    let count = reader.count()?;
    if count > MAX_JOB_ROWS {
        return Err(RuntimeManifestError::RecordLimit);
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(JobRow {
            number: reader.u32()?,
            command: reader.string(1024)?,
            started_at: Timestamp::new(reader.u64()?, reader.u32()?)
                .map_err(|_| RuntimeManifestError::Malformed)?,
            owner: reader.string(128)?,
            session_id: reader.process_session_id()?,
            owner_generation: reader.process_session_owner_generation()?,
            terminal: reader
                .optional()?
                .then(|| {
                    let status = match reader.u8()? {
                        1 => JobTerminalStatus::Exited,
                        2 => JobTerminalStatus::Stopped,
                        tag => {
                            return Err(RuntimeManifestError::UnknownTag {
                                field: "job terminal status",
                                tag,
                            });
                        }
                    };
                    let exit_code = reader
                        .optional()?
                        .then(|| {
                            reader
                                .u32()
                                .map(|code| i32::from_be_bytes(code.to_be_bytes()))
                        })
                        .transpose()?;
                    let stopper = reader
                        .optional()?
                        .then(|| {
                            Ok(match reader.u8()? {
                                1 => plakous_tools::JobStopReason::Model,
                                2 => plakous_tools::JobStopReason::User,
                                3 => plakous_tools::JobStopReason::SessionExit,
                                4 => plakous_tools::JobStopReason::AttemptEnded,
                                5 => plakous_tools::JobStopReason::HardCap,
                                tag => {
                                    return Err(RuntimeManifestError::UnknownTag {
                                        field: "job stopper",
                                        tag,
                                    });
                                }
                            })
                        })
                        .transpose()?;
                    let seconds = reader.u64()?;
                    let nanos = reader.u32()?;
                    if nanos >= 1_000_000_000 {
                        return Err(RuntimeManifestError::Malformed);
                    }
                    Ok(JobTerminal {
                        status,
                        exit_code,
                        stopper,
                        elapsed: std::time::Duration::new(seconds, nanos),
                        tail: reader.string(4096)?,
                    })
                })
                .transpose()?,
        });
    }
    let jobs = JobLedger::new(next_id, rows)?;
    let interpreter_observation = reader
        .optional()?
        .then(|| {
            let generation = reader.u64()?;
            let availability = match reader.u8()? {
                1 => InterpreterAvailability::Available,
                2 => InterpreterAvailability::Reset,
                tag => {
                    return Err(RuntimeManifestError::UnknownTag {
                        field: "interpreter availability",
                        tag,
                    });
                }
            };
            let reset_reason = reader
                .optional()?
                .then(|| match reader.u8()? {
                    1 => Ok(InterpreterResetReason::ProcessReopen),
                    2 => Ok(InterpreterResetReason::HostLost),
                    3 => Ok(InterpreterResetReason::ProtocolViolation),
                    4 => Ok(InterpreterResetReason::Cancellation),
                    5 => Ok(InterpreterResetReason::Drift),
                    6 => Ok(InterpreterResetReason::AgentShutdown),
                    tag => Err(RuntimeManifestError::UnknownTag {
                        field: "interpreter reset reason",
                        tag,
                    }),
                })
                .transpose()?;
            let affected_cell = reader
                .optional()?
                .then(|| reader.string(MAX_ID_BYTES))
                .transpose()?;
            let last_acknowledged = reader
                .optional()?
                .then(|| match reader.u8()? {
                    1 => Ok(InterpreterAcknowledgedBoundary::Launch),
                    2 => Ok(InterpreterAcknowledgedBoundary::SourceAccepted),
                    3 => Ok(InterpreterAcknowledgedBoundary::ChildResultAccepted),
                    4 => Ok(InterpreterAcknowledgedBoundary::Terminal),
                    5 => Ok(InterpreterAcknowledgedBoundary::Join),
                    tag => Err(RuntimeManifestError::UnknownTag {
                        field: "interpreter acknowledged boundary",
                        tag,
                    }),
                })
                .transpose()?;
            InterpreterObservation::new(
                generation,
                availability,
                reset_reason,
                affected_cell,
                last_acknowledged,
            )
        })
        .transpose()?;
    let durability = match reader.u8()? {
        1 => RuntimeDurability::Required,
        2 => RuntimeDurability::BestEffort,
        3 => RuntimeDurability::Disabled,
        tag => {
            return Err(RuntimeManifestError::UnknownTag {
                field: "durability",
                tag,
            });
        }
    };
    reader.finish()?;
    Ok(RuntimeManifestBundleMetadata {
        provenance,
        topology,
        lifecycle,
        interpreter_observation,
        jobs,
        durability,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the arguments are the closed manifest groups"
)]
pub(crate) fn assemble_bundle_manifest(
    metadata: RuntimeManifestBundleMetadata,
    harness_plan: ReconstructiveComponent,
    provider_configuration: CompiledProviderConfiguration,
    canonical: RuntimeCanonicalState,
    binding: ModelRouteBinding,
    tools: ToolRuntimeManifest,
    ledger: RuntimeOperationLedger,
    family_artifact: Option<FamilyArtifact>,
) -> Result<RuntimeManifest, RuntimeManifestError> {
    RuntimeManifest::new(
        metadata.provenance,
        metadata.topology,
        metadata.lifecycle,
        harness_plan,
        provider_configuration,
        canonical,
        binding,
        tools,
        ledger,
        family_artifact,
        metadata.interpreter_observation,
        metadata.jobs,
        metadata.durability,
    )
}

pub(crate) fn encode_bundle_provider_configuration(
    value: &CompiledProviderConfiguration,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let payload = serde_json::to_vec(value)
        .map_err(|_| RuntimeManifestError::InvalidProviderConfiguration)?;
    if payload.is_empty() || payload.len().saturating_add(12) > MAX_RUNTIME_COMPONENT_BYTES {
        return Err(RuntimeManifestError::ComponentSize);
    }
    let mut bytes = Vec::with_capacity(payload.len().saturating_add(12));
    bytes.extend_from_slice(&PROVIDER_CONFIGURATION_MAGIC);
    bytes.extend_from_slice(&RUNTIME_PROVIDER_CONFIGURATION_CODEC_REVISION.to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

pub(crate) fn decode_bundle_provider_configuration(
    bytes: &[u8],
) -> Result<CompiledProviderConfiguration, RuntimeManifestError> {
    if bytes.len() <= 12 || bytes.len() > MAX_RUNTIME_COMPONENT_BYTES {
        return Err(RuntimeManifestError::ComponentSize);
    }
    if bytes[..8] != PROVIDER_CONFIGURATION_MAGIC {
        return Err(RuntimeManifestError::InvalidMagic);
    }
    let revision = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| RuntimeManifestError::Malformed)?,
    );
    if revision != RUNTIME_PROVIDER_CONFIGURATION_CODEC_REVISION {
        return Err(RuntimeManifestError::UnsupportedVersion { found: revision });
    }
    serde_json::from_slice(&bytes[12..])
        .map_err(|_| RuntimeManifestError::InvalidProviderConfiguration)
}

macro_rules! bundle_value_codec {
    ($encode:ident, $decode:ident, $ty:ty, $write:ident, $read:ident) => {
        pub(crate) fn $encode(value: &$ty) -> Result<Vec<u8>, RuntimeManifestError> {
            let mut writer = Writer::default();
            writer.$write(value)?;
            Ok(writer.bytes)
        }

        pub(crate) fn $decode(bytes: &[u8]) -> Result<$ty, RuntimeManifestError> {
            let mut reader = Reader::new(bytes);
            let value = reader.$read()?;
            reader.finish()?;
            Ok(value)
        }
    };
}

bundle_value_codec!(
    encode_bundle_stable_prefix,
    decode_bundle_stable_prefix,
    StableRequestPrefix,
    stable_prefix,
    stable_prefix
);
bundle_value_codec!(
    encode_bundle_history_segment,
    decode_bundle_history_segment,
    HistorySegment,
    segment,
    segment
);
bundle_value_codec!(
    encode_bundle_binding,
    decode_bundle_binding,
    ModelRouteBinding,
    binding,
    binding
);
bundle_value_codec!(
    encode_bundle_tools,
    decode_bundle_tools,
    ToolRuntimeManifest,
    tools,
    tools
);
bundle_value_codec!(
    encode_bundle_family_artifact,
    decode_bundle_family_artifact,
    FamilyArtifact,
    family_artifact,
    family_artifact
);

pub(crate) fn encode_bundle_component(
    component: &ReconstructiveComponent,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.component(component)?;
    Ok(writer.bytes)
}

pub(crate) fn decode_bundle_component(
    bytes: &[u8],
) -> Result<ReconstructiveComponent, RuntimeManifestError> {
    let expected =
        RuntimeComponentKind::from_tag(*bytes.first().ok_or(RuntimeManifestError::Malformed)?)?;
    let mut reader = Reader::new(bytes);
    let value = reader.component(expected)?;
    reader.finish()?;
    Ok(value)
}

pub(crate) fn encode_bundle_command_record(
    record: &DurableCommandRecord,
) -> Result<Vec<u8>, RuntimeManifestError> {
    encode_command_record_body(record)
}

pub(crate) fn decode_bundle_command_record(
    bytes: &[u8],
) -> Result<DurableCommandRecord, RuntimeManifestError> {
    let mut reader = Reader::new(bytes);
    let record = reader.command_record()?;
    reader.finish()?;
    Ok(record)
}

/// Encodes the persisted record, naming each component by its object digest.
pub(crate) fn encode_bundle_operation_record(
    record: &DurableOperationRecord,
) -> Result<Vec<u8>, RuntimeManifestError> {
    OperationRecordWire::new(record, crate::checkpoint_bundle::component_object_digest)?.encode()
}

fn encode_command_record_body(
    record: &DurableCommandRecord,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.command_record(record)?;
    Ok(writer.bytes)
}

fn ledger_prefix_identity(
    kind: &[u8],
    previous: Option<Digest>,
    record: &[u8],
) -> Result<Digest, RuntimeManifestError> {
    let previous_present = [u8::from(previous.is_some())];
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-agent/runtime-ledger-prefix-v1",
            kind,
            &previous_present,
            previous.as_ref().map_or(&[][..], |digest| digest.bytes()),
            record,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

pub(crate) fn encode_bundle_digest(digest: Digest) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.digest(digest)?;
    Ok(writer.bytes)
}

pub(crate) fn decode_bundle_digest(bytes: &[u8]) -> Result<Digest, RuntimeManifestError> {
    let mut reader = Reader::new(bytes);
    let digest = reader.digest(None)?;
    reader.finish()?;
    Ok(digest)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "negative assertions use expect/expect_err to preserve precise typed failure diagnostics"
)]
mod tests;
