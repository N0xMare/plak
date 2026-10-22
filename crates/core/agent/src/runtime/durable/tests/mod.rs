#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit test assertions"
)]
mod fake_composition;

mod admission_occupancy;
#[path = "../artifact_prefix_tests.rs"]
mod artifact_prefix_tests;
#[path = "../atomic_transition_tests.rs"]
mod atomic_transition_tests;
mod cancelled_steer;
mod cleanup_precedence;
mod clip_budget;
#[path = "../code_cell_crash_tests.rs"]
mod code_cell_crash;
#[path = "../code_cell_recovery_tests.rs"]
mod code_cell_recovery;
#[path = "../command_ack_tests.rs"]
mod command_ack_tests;
mod command_feed_open;
#[path = "../command_retention_tests.rs"]
mod command_retention;
#[path = "../composition_steer_tests.rs"]
mod composition_steer_tests;
#[path = "../content_filter_tests.rs"]
mod content_filter_tests;
mod driver_live_paths;
mod driver_reliability;
#[path = "../feed_fact_tests.rs"]
mod feed_fact_tests;
#[path = "../finalization_artifact_tests.rs"]
mod finalization_artifact;
#[path = "../finish_parity_tests.rs"]
mod finish_parity;
#[path = "../idle_retention_tests.rs"]
mod idle_retention;
mod jobs;
pub(super) mod jobs_carry_fixture;
#[path = "../landed_round_abandonment_tests.rs"]
pub(super) mod landed_round_abandonment;
mod long_turn_compaction;
mod native_capacity_tests;
mod observation_budget;
mod process_notices;
#[path = "../provider_outcome_tests.rs"]
mod provider_outcome_tests;
#[path = "../reresolution_tests.rs"]
mod reresolution_tests;
mod round_limit;
mod scheduling_diagnostics;
mod terminal_enrichment;
mod turn_fence_workflow;
#[path = "../workflow_abandonment_tests.rs"]
mod workflow_abandonment;
#[path = "../workflow_closure_tests.rs"]
mod workflow_closure_tests;

mod cancel_cell;
mod capacity_fence;
mod capacity_replacement;
mod checkpoint_fixtures;
mod code_cell_abandonment;
mod code_cell_composition;
mod compaction_admission;
mod compaction_recovery_cases;
mod composition_scheduler;
mod continuation_reopen;
mod descriptor_fixtures;
mod fence_compaction;
mod lease_permits;
mod lineage;
mod model_fixtures;
mod pending_decisions;
mod placeholders;
mod provenance;
mod record_tests;
mod reopen_and_lease;
mod seeded_carry;
mod session_update;
mod store_fixtures;
mod telemetry;
mod terminal_publication;

use crate::lease::{LeaseDomain, LeaseInstant};
use crate::local_composition::{
    LocalCompositionAttachmentIdentity, LocalCompositionConstruction, LocalCompositionContentItem,
    LocalCompositionDescriptor, LocalCompositionEvent, LocalCompositionEventReceiver,
    LocalCompositionExecuteInput, LocalCompositionExecuteRequest, LocalCompositionFuture,
    LocalCompositionIntegration, LocalCompositionLifecycle, LocalCompositionResponse,
    LocalCompositionResultProjection, LocalCompositionSession, LocalCompositionSessionBundle,
    LocalCompositionSessionFuture, LocalCompositionSessionLoss, LocalCompositionShutdownResult,
    LocalCompositionStartedCell, LocalCompositionTerminateResult, LocalCompositionToolDefinition,
    LocalCompositionWaitInput, LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use crate::manifest::RuntimeCompactionStrategy;
use crate::manifest::{
    InterpreterAcknowledgedBoundary, InterpreterAvailability, InterpreterResetReason,
};
use crate::profile::{
    CachePreferenceStrategy, ContinuationPreferenceStrategy, HarnessProfileId,
    HarnessProfileProvenance, HarnessProfileSpecBuilder, InstructionCompositionStrategy,
    ProfileSemanticVersion, ReasoningPreferenceStrategy, ToolExposureStrategy,
};
use crate::runtime::admission::{
    SealedRuntimeAdmission, recorded_admission_authority, sealed_runtime_admission,
    validate_configuration_runtime_authority,
};
use crate::runtime::build::{
    compile_explicit_runtime_profile, local_composition_binding_identity,
    resolve_construction_harness_plan, validate_local_composition_binding_slot,
    validate_reopened_projection,
};
use crate::runtime::checkpoint::identity::{
    CHECKPOINT_COMMIT_IDENTITY_REVISION, CheckpointCommitFacts, checkpoint_commit_identity,
    deterministic_checkpoint_id,
};
use crate::runtime::checkpoint::io::recovered_commit_matches_exact;
use crate::runtime::cleanup::settle_local_composition_deliveries;
use crate::runtime::compaction::fold::{
    build_compaction_success_fold, compaction_summary_segment, native_compaction_successor,
};
use crate::runtime::compaction::record::compaction_attempt_id;
use crate::runtime::compaction::request::{
    compaction_runtime_matches_source_operation, harness_compaction_prefix,
    native_compaction_facet_enrolled,
};
use crate::runtime::compaction::{
    CompactionSuccessFold, DurableCompactionIdentity, DurableCompactionPublicationEvidence,
};
use crate::runtime::composition::nested::{
    deliver_local_composition_nested_result, reserve_local_composition_nested_slot,
};
use crate::runtime::composition::projection::{
    local_composition_child_call_id, local_composition_nested_result_value,
    local_composition_projection_status, local_composition_recovery_authoritative,
    local_composition_started_error,
};
use crate::runtime::composition::record::{
    DurableLocalCompositionContinuationRequestV1, DurableLocalCompositionEvaluationRequestV2,
    decode_local_composition_continuation_request, decode_local_composition_evaluation_request,
    encode_local_composition_continuation_request, encode_local_composition_evaluation_request,
};
use crate::runtime::composition::{
    LocalCompositionRetentionState, PendingNestedDeliveries,
    retain_local_composition_at_root_boundary,
};
use crate::runtime::config::{AgentConfig, AgentRequestPolicy, RUNTIME_EXECUTOR_STACK_BYTES};
use crate::runtime::continuation::{
    RestoredContinuationFrame, decode_await_effect_decision_continuation,
    decode_hosted_continuation, previous_response_metadata, runtime_component,
    runtime_revalidation_witness, terminal_record_at_checkpoint, terminal_record_for_command,
};
use crate::runtime::control::run_environment;
use crate::runtime::durable::DecodedCompactionWorkflow;
use crate::runtime::durable::bootstrap::{lowercase_hex, prepare_durable_genesis};
use crate::runtime::durable::branch::operation_run_id;
use crate::runtime::durable::compaction_recovery::{
    decode_compaction_operations, decode_paid_compaction_operations,
};
use crate::runtime::durable::composition_recovery::code_cell_abandonment_fold;
use crate::runtime::durable::{
    DurableCodeCellAbandonmentIdentity, DurableCodeCellAbandonmentOutcome,
    DurableCodeCellAbandonmentRequest, DurableCompactionReceipt, DurableCompactionRecoveryOutcome,
    DurableGenesisIdentities, DurableRuntimeOpen, DurableSessionAddress, DurableSessionRuntime,
};
use crate::runtime::operation_record::{
    compaction_receipt_lifecycle_ok, operation_binding_component, operation_turn_id,
    provider_terminal_state, working_lifecycle, working_lifecycle_for_root,
};
use crate::runtime::terminal::{
    DurableAgentError, DurableSessionStoreError, DurableTerminalOutcome,
    PendingTerminalResultRecord, TERMINAL_RESULT_RECORD_VERSION, TerminalRecordRejection,
    decode_pending_terminal_record, decode_terminal_result_record, encode_pending_terminal_record,
    finalized_operation_evidence, lock_mutex, read_terminal_result_record,
    tool_error_kind_from_durable,
};
use crate::runtime::{
    Agent, AgentBuilder, DurableActorCommandEffect, DurableCommandEnvelope, OperationId,
    ToolExecutionResult, ToolPresentation, TurnResult, TurnResultOutcome,
};
use crate::session::{StoreAdmission, StoreCapacityBound, StoreInvalidReason};
use crate::tool_runtime::{ToolRuntime, ToolRuntimeConfig, ToolScheduler};
use crate::{
    AcquireBranch, AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason,
    AgentError, AgentErrorKind, AgentEventKind, AgentSessionEndpointError, BindingReference,
    BranchHead, BranchLease, BranchRevision, CancellationStatus, CheckpointId, CheckpointObjectRef,
    ChildExitEscalation, ChildProcessExit, ChildProcessKind, CommandAck, CommandAckDisposition,
    CommandId, CommitCheckpoint, CommitOperation, CommitReceipt, CommittedCheckpoint,
    CompactionEventOutcome, CompactionEventReason, ControlError, ControlErrorKind, CreateSession,
    DurabilityAttemptKind, DurabilityAttemptOutcome, DurableCommandDisposition,
    DurableCommandRecord, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, EffectDecisionStatus, ForkBranch, ForkReceipt,
    RUNTIME_FORMAT_EPOCH, ReconstructiveComponent, RecoverFork, RecoveryAction,
    RecoveryHandlerSignature, RecoveryObligation, RecoveryObligationKind, ResumeOutcome,
    RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointCodec, RuntimeComponentKind,
    RuntimeDurability, RuntimeLifecycle, RuntimeManifest, RuntimeObjectAddress, RuntimeObjectKind,
    RuntimeOperationLedger, RuntimeRehydration, RuntimeRevalidationWitness, RuntimeTopology,
    RuntimeWorkingRoot, SessionRef, SessionStore, SessionStoreError, SessionWorkflowId,
    StoredCheckpoint, TelemetryEmitter, TelemetryObservation, TelemetryObservationKind,
    TelemetrySink, TelemetrySinkError, TelemetryTerminalStatus, ToolCallerClass, TurnCommand,
    TurnEvents, TurnUsage, ViewError, ViewRequest, WorkingRecoveryPhase,
};
use fake_composition::{CompositionCounters, FakeComposition};
use futures_util::StreamExt;
use plakous_compose::{
    CodexNativeLocalCodeModeV1, CodingPresentationExt, ProductJavaScriptCompositionV1,
    PythonPtcPresentationV1, PythonPtcV1,
};
use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, BlockStart, BlockStarted, BranchId, CachePolicy,
    CallId, CancellationToken, CanonicalHistory, Capability, CapabilityConstraints,
    CapabilityRequirement, CapabilitySet, CapacityCertificate, CapacityRule, CompactionBlock,
    CompiledProviderConfiguration, ConfiguredLanguageModel, ConfiguredTool, ContentBlock,
    ContextAdmissionRequirement, ContextContract, ContextEvidenceSource, ConversationItem,
    ConversationView, CountCertificateKind, CountObservation, CredentialSelectionId, Digest,
    DigestDomain, EffectiveProviderConfiguration, EffectiveProviderConfigurationId, EntryId,
    EvidenceFreshness, FactProvenance, FactSource, FamilyArtifactProducerFacts,
    FamilyPolicyEnvelope, FeatureSupport, FinishDetails, FinishReason, FixedClock,
    GenerationLimits, HistorySegment, HistorySegmentKind, LanguageModel,
    LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId, ModelCacheAffinity,
    ModelCallRecoveryOperation, ModelCallRecoveryRequest, ModelError, ModelErrorKind, ModelEvent,
    ModelFamilyId, ModelId, ModelImplementationIdentity, ModelOperationRoot,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, ModelRequest,
    ModelResponse, ModelStream, NamespaceId, NativeOptions, OutputBoundDisposition, OutputLocation,
    OutputPolicy, ParallelToolDisposition, PreparedCacheUse, PreparedCallError, PreparedCallRecord,
    PreparedModelCall, PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection,
    ProtocolId, Provenance, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
    ProviderStorageDisposition, ReasoningBlock, ReasoningEffort, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningPolicy, ReasoningSummaryDisposition, ReasoningVisibility,
    ReplayDisposition, RequestMetadata, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    ResponseStarted, ResponseStateDisposition, RetentionDisposition, RouteId, RunId,
    RuntimeModelAuthority, RuntimeModelAuthorityOperation, SafeLimit,
    SealedFamilyArtifactCandidate, SelectedProviderConfiguration, SessionId, SessionWorkflowRootId,
    StableRequestPrefix, Support, SystemIdGenerator, TextBlock, TextDelta, Timestamp,
    ToolCallBlock, ToolCallState, ToolChoice, ToolConcurrency, ToolCorrelation, ToolDefinition,
    ToolEffect, ToolExecutionDisposition, ToolExecutionOwner, ToolInputContract, ToolName,
    ToolOutputContract, ToolPresentationMode, ToolResultBlock, ToolResultKind, ToolResultPart,
    TurnId, TurnPosition, Usage, UsageUpdate, ValidatedRequestPlan, ValidationMode,
    WorkingProjection,
};
use plakous_tool_process::{ProcessSessionTools, ProcessToolProjectionKind};
use plakous_tools::{
    AlwaysAllowPolicy, CodingPresentation, EffectDecisionAuditId, EffectDecisionPrompt,
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    FrozenToolProjectionDescriptorV2, FrozenToolRegistry, FrozenToolRegistryDescriptor,
    PROCESS_SESSION_CAPABILITY_NAME, ProjectionCandidateAdmission,
    ProviderModelProjectionIdentityV2, RegisteredToolId, ToolExecutorBindings,
    ToolExecutorBindingsBuilder, ToolExecutorId, ToolInvocation, ToolInvocationRequest,
    ToolOutcome, ToolOutcomeKind, ToolPolicy, ToolPolicyDecision, ToolPolicyDenial,
    ToolPolicyFuture, ToolPolicyRequest, ToolPolicyRevision, ToolProjectionMap, ToolRegistration,
    ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind, WorkspacePolicy,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::Notify;

use cancel_cell::*;
use checkpoint_fixtures::*;
use code_cell_abandonment::*;
use code_cell_composition::*;
use compaction_admission::*;
use composition_scheduler::*;
use descriptor_fixtures::*;
use fence_compaction::*;
use lease_permits::*;
use lineage::*;
use model_fixtures::*;
use pending_decisions::*;
use placeholders::*;
use provenance::*;
use record_tests::*;
use reopen_and_lease::*;
use session_update::*;
use store_fixtures::*;
use telemetry::*;

pub(super) use session_update::SessionUpdateFixtureMode;

const TEST_LEASE_DOMAIN: LeaseDomain = LeaseDomain::new([1; 16], [2; 32]);

fn lease_instant(seconds: u64, nanoseconds: u32) -> Result<LeaseInstant, SessionStoreError> {
    LeaseInstant::new(TEST_LEASE_DOMAIN, seconds, nanoseconds)
}
