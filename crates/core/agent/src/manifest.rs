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

mod bindings;
mod codec;
mod context_state;
mod encoding;
mod jobs;
mod ledger;
mod lifecycle;
pub(crate) mod named_state;
pub use named_state::NamedState;
mod operations;
mod state;
pub(crate) use context_state::ResidencySelectionAudit;
pub use context_state::{
    CompactionInvalidation, CompactionProvenance, ContextEvaluationAudit,
    RuntimeCompactionStrategy, RuntimeContextState, RuntimeContextTransition,
    RuntimeContextTransitionKind, SemanticCompactionPublication,
};
#[allow(
    clippy::wildcard_imports,
    reason = "the encoding split shares the manifest vocabulary"
)]
use encoding::*;
pub(crate) use encoding::{
    assemble_bundle_manifest, decode_bundle_binding, decode_bundle_command_record,
    decode_bundle_component, decode_bundle_digest, decode_bundle_family_artifact,
    decode_bundle_history_segment, decode_bundle_metadata, decode_bundle_provider_configuration,
    decode_bundle_stable_prefix, decode_bundle_tools, encode_bundle_binding,
    encode_bundle_command_record, encode_bundle_component, encode_bundle_digest,
    encode_bundle_family_artifact, encode_bundle_history_segment, encode_bundle_metadata,
    encode_bundle_metadata_value, encode_bundle_operation_record,
    encode_bundle_provider_configuration, encode_bundle_stable_prefix, encode_bundle_tools,
};
pub(crate) use jobs::MAX_JOB_ROWS;
#[cfg(test)]
pub(crate) use jobs::fixture_job_owner;
pub use jobs::{JobLedger, JobRow, JobTerminal, JobTerminalStatus, LineJobTerminal};
use operations::validate_composition_graph;

pub use ledger::RuntimeOperationLedger;
#[allow(
    clippy::wildcard_imports,
    reason = "the ledger split shares the manifest vocabulary"
)]
use ledger::*;
pub(crate) use ledger::{OperationLedgerMutationRef, RetentionAnchor};
#[allow(
    clippy::wildcard_imports,
    reason = "the lifecycle split shares the manifest vocabulary"
)]
use lifecycle::*;
pub use lifecycle::{
    RecoveryAction, RecoveryHandlerSignature, RecoveryObligation, RecoveryObligationKind,
    RuntimeLifecycle, WorkingRecoveryPhase,
};

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
    DigestDomain, EntryId, HistorySegment, HistorySegmentKind, LineId, LocalDurabilityDisposition,
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
/// Committed history segments at which a turn fence schedules a paid fold even
/// below the token trigger (spec 01 §10.3).
pub(crate) const SEGMENT_PRESSURE_SEGMENTS: usize = MAX_RUNTIME_RECORDS / 4 * 3;
/// Committed history segments at which the next round is refused. The margin
/// holds what one round and its terminal append, so the checkpoint that ends a
/// refused turn still encodes under [`MAX_RUNTIME_RECORDS`].
pub(crate) const SEGMENT_REFUSAL_SEGMENTS: usize = MAX_RUNTIME_RECORDS - 64;
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

/// Exact bounded bytes from one named, revisioned canonical sub-codec.
#[derive(Clone, Eq, PartialEq)]
pub struct ReconstructiveComponent {
    kind: RuntimeComponentKind,
    codec_revision: u32,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

/// Stable non-secret reference whose authority owner must revalidate it.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BindingReference {
    authority: String,
    subject: String,
    revision: u64,
}

/// Historical admission/revalidation fact retained without credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevalidationFact {
    reference: BindingReference,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    evidence: ReconstructiveComponent,
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

/// Exact tool registry/exposure/environment/effect-policy reconstruction data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRuntimeManifest {
    registry: ReconstructiveComponent,
    exposure: ReconstructiveComponent,
    environment: ReconstructiveComponent,
    effect_policy: ReconstructiveComponent,
    policy: BindingReference,
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

/// Exact stable root shared by model requests and durable runtime operations.
pub type RuntimeOperationRoot = ModelOperationRoot;

/// Runtime-minted semantic identity of one joined process session.
///
/// This is never an operating-system PID or a reconnect capability. The
/// allocation-local backend identity remains outside the durable manifest.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessSessionId([u8; PROCESS_SESSION_ID_BYTES]);

/// Nonzero generation of the process-local owner that minted a session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessSessionOwnerGeneration(NonZeroU64);

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

/// State transition committed at one immutable checkpoint boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOperationTransition {
    checkpoint_id: CheckpointId,
    state: DurableOperationState,
}

/// Reconstruction-complete provider/tool/commit operation record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOperationRecord {
    operation_id: u64,
    kind: DurableOperationKind,
    request: ReconstructiveComponent,
    transitions: Vec<DurableOperationTransition>,
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

fn composition_child_call_id(binding: Digest) -> Result<CallId, RuntimeManifestError> {
    let bytes = crate::derived_id::bytes(binding);
    CallId::try_from(uuid::Uuid::from_bytes(bytes)).map_err(|_| RuntimeManifestError::InvalidLedger)
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
    /// The agent line this branch belongs to, fixed for the branch's life.
    pub line: LineId,
    /// The line generation at which this branch became its line's head.
    /// It is 1 for a branch that started its line and increases by one with
    /// every head advance; fixed for the branch's life.
    pub line_generation: u64,
    /// Lease/fence value under which this checkpoint committed.
    pub lease_fence: u64,
    /// Exact commit operation identity.
    pub commit_identity: Digest,
}

/// Stable prefix, canonical history, bounded working projection, and context state.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeCanonicalState {
    stable_prefix: StableRequestPrefix,
    history: CanonicalHistory,
    working: WorkingProjection,
    context: RuntimeContextState,
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

/// Reconstruction-complete runtime manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeManifest {
    named_state: NamedState,
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
        named_state: NamedState,
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
            named_state,
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

    /// Returns session-owned named state, independent of interpreter lifetime.
    #[must_use]
    pub const fn named_state(&self) -> &NamedState {
        &self.named_state
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
    /// A named key or canonical JSON value exceeds its closed contract.
    #[error("invalid or oversized named state")]
    InvalidNamedState(#[source] Box<named_state::StateError>),
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
            &manifest.named_state.integrity_bytes(),
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "negative assertions use expect/expect_err to preserve precise typed failure diagnostics"
)]
mod tests;
