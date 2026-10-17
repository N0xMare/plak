//! Curated, consumer-safe single-agent component for Plakous.
//!
//! The supported root exposes construction, turn, control, observation,
//! presentation, context-source, profile, policy, usage, and telemetry values.
//! A single private task owns mutable conversation state and drives any
//! [`plakous_model::LanguageModel`] through the same canonical contracts.
//!
//! Raw runtime and persistence authority is available only through the clearly
//! unsupported [`internal`] namespace for official sibling composition. Direct
//! use of that namespace carries no compatibility or support promise.
//!
//! Raw checkpoint authority is not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::RuntimeCheckpoint;
//! ```
//!
//! Raw manifest authority is not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::RuntimeManifest;
//! ```
//!
//! Store authority is not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::SessionStore;
//! ```
//!
//! The durable coordinator is not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::DurableSessionRuntime;
//! ```
//!
//! The private driver endpoint is not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::AgentSessionEndpoint;
//! ```
//!
//! The facade bridge is not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::facade_support;
//! ```
//!
//! Checkpoint codecs and object graphs are not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::{RuntimeCheckpointCodec, RuntimeObject};
//! ```
//!
//! Recovery-scope internals are not part of the supported root:
//!
//! ```compile_fail
//! use plakous_agent::RuntimeRevalidationWitness;
//! ```

#![forbid(unsafe_code)]
#![allow(
    clippy::too_many_lines,
    reason = "bound agent construction keeps its authority ordering visible"
)]

mod checkpoint_bundle;
mod context_source;
mod control;
mod error;
mod event;
mod facade_support;
mod feed;
mod input;
mod lease;
mod local_composition;
mod manifest;
mod profile;
mod recovery_scope;
mod rehydration;
mod runtime;
mod session;
mod state;
mod telemetry;
mod tool_runtime;
mod usage;

/// Unsupported implementation tier for official sibling composition.
///
/// Items in this namespace are deliberately nameable but are not part of the
/// supported `plakous-agent` compatibility inventory.
pub mod internal {
    pub use crate::checkpoint_bundle::{
        CheckpointBundleError, CheckpointBundleStats, CheckpointObjectBundle, RUNTIME_FORMAT_EPOCH,
        RuntimeCheckpointCodec, RuntimeObject, RuntimeObjectAddress, RuntimeObjectKind,
    };
    pub use crate::context_source::{
        ContextSourceCatalog, ContextSourceResolver, RegisteredContextSource,
    };
    pub(crate) use crate::control::{
        ActorCommandEffect, ActorCommandReply, ActorControl, AgentContextStatusRequest,
        submit_actor_command,
    };
    pub use crate::control::{
        AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason,
        AgentSessionEndpoint, AgentSessionEndpointError, CancellationStatus, CommandAck,
        CommandAckDisposition, CommandId, CompactionRung, CompactionSummary, ControlError,
        ControlErrorKind, EffectDecisionChoice, EffectDecisionRef, EffectDecisionStatus,
        SteeringStatus, TurnCommand, TurnControl, WeakAgentSessionEndpoint,
    };
    pub use crate::error::{AgentError, AgentErrorKind};
    pub use crate::event::{
        AgentEvent, AgentEventKind, AgentViewReducer, AgentViewSnapshot, AgentViewState,
        CompactionEventOutcome, CompactionEventReason, ConversationItemView,
        EffectDecisionResolution, EventCursor, EventDelivery, EventDeliveryClass, EventLossKind,
        EventLostRange, EventOrigin, EventScope, EventSequence, EventSubscription,
        EventToolProgress, EventVisibility, MIN_EVENT_DEBT_BYTES, NormalizedModelEventKind,
        PendingEffectDecisionView, ReducerError, ResumeOutcome, SubscriptionEnd,
        SubscriptionLimitError, SubscriptionLimits, ToolActivityView, ToolAuditIdentity,
        ToolCallAttemptIdentity, ToolCallObservation, ToolCallerClass, ToolCallerLineageView,
        ToolProgressSummary, ToolResultObservation, TurnEventError, TurnEvents, TurnView,
        TurnViewState, ViewAttachment, ViewError, ViewRequest,
    };
    pub(crate) use crate::event::{EventContext, EventHub, EventHubLimits, EventPipeline};
    pub use crate::feed::{
        FeedBranchTransition, FeedCallerLineage, FeedCancelCause, FeedChange, FeedCodecError,
        FeedCommandKind, FeedCompactPass, FeedCompactReason, FeedContextLimit, FeedFact,
        FeedHeadKind, FeedHeader, FeedLocator, FeedOperationKind, FeedOperationState, FeedPage,
        FeedRecord, FeedSeq, FeedTail, FeedTurnOutcome, FeedWorkflowKind, FeedWorkflowOutcome,
        MAX_FEED_BYTES_PER_SESSION, MAX_FEED_PAGE_BYTES, MAX_FEED_RECORD_BYTES,
        MAX_FEED_RECORDS_PER_SESSION, MAX_FEED_TRIM_BATCH,
    };
    pub use crate::input::{MAX_USER_IMAGE_BYTES, MAX_USER_IMAGES, UserInput, UserPart};
    pub use crate::lease::{LeaseClock, LeaseDomain, LeaseInstant};
    pub use crate::local_composition::*;
    pub use crate::manifest::{
        BindingReference, CompactionInvalidation, CompactionProvenance, ContextEvaluationAudit,
        DurableCommandDisposition, DurableCommandRecord, DurableOperationKind,
        DurableOperationRecord, DurableOperationState, DurableOperationTransition, FamilyArtifact,
        ForkLineage, JobLedger, JobRow, JobTerminal, JobTerminalStatus, ModelRouteBinding,
        PendingEffectDecisionState, ProducerProvenance, RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
        RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION, ReconstructiveComponent, RecoveryAction,
        RecoveryHandlerSignature, RecoveryObligation, RecoveryObligationKind, RevalidationFact,
        RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointKind, RuntimeCompactionStrategy,
        RuntimeComponentKind, RuntimeContextState, RuntimeContextTransition,
        RuntimeContextTransitionKind, RuntimeDurability, RuntimeLifecycle, RuntimeManifest,
        RuntimeManifestError, RuntimeOperationLedger, RuntimeOperationRoot, RuntimeTopology,
        RuntimeWorkingRoot, SemanticCompactionPublication, ToolRuntimeManifest,
        WorkingRecoveryPhase,
    };
    pub use crate::profile::{
        CachePreferenceStrategy, CompilerModelFacts, CompilerRuntimeFacts, CompilerToolFacts,
        ContextSourceSelector, ContinuationPreferenceStrategy, FeatureSupportFact,
        HarnessCompilerInputs, HarnessProfileCompiler, HarnessProfileId, HarnessProfileProvenance,
        HarnessProfileSpec, HarnessProfileSpecBuilder, InstructionCompositionStrategy,
        MAX_INSTRUCTION_BYTES, ProfileError, ProfileSemanticVersion, ReasoningPreferenceStrategy,
        ResolvedCachePlan, ResolvedContextSourceFact, ResolvedContinuationPlan,
        ResolvedHarnessPlan, ResolvedInstructionPlan, ResolvedReasoningPlan,
        ResolvedToolExposurePlan, ToolExposureOverride, ToolExposureStrategy,
        VerifiedContextSourceRef, compile_explicit_profile, compiler_model_facts_from_model,
        recommended_neutral_coding_profile,
    };
    pub use crate::recovery_scope::{
        RevalidationError, RuntimeRehydration, RuntimeRehydrationError, RuntimeRevalidationWitness,
        UnsupportedRecoveryScope,
    };
    pub use crate::rehydration::{
        HARNESS_PLAN_COMPONENT_FORMAT_VERSION, HarnessManifestCodecError,
        HarnessPlanComponentEnvelope, HarnessPlanComponentManifest, RUNTIME_POLICY_REVISION,
    };
    pub use crate::runtime::{
        AbandonedTurnClaim, AbandonedTurnReport, AbandonmentCause, Agent, AgentBuilder,
        AgentConfig, AgentRequestPolicy, CheckpointRecoveryVerdict, ContextOverflowRemedy,
        DEFAULT_STORE_OPERATION_TIMEOUT, DurableCodeCellAbandonmentOutcome,
        DurableCodeCellAbandonmentRequest, DurableCompactionReceipt,
        DurableCompactionRecoveryOutcome, DurableEffectEvidenceRecoveryOutcome,
        DurableEffectEvidenceRecoveryRequest, DurableRuntimeBranchOutcome, DurableRuntimeOpen,
        DurableSessionAddress, DurableSessionRuntime, ModelReresolutionError,
        ModelReresolutionOperation, ModelReresolver, OPERATION_BUDGET_WRAP_UP_NOTE,
        RUNTIME_EXECUTOR_STACK_BYTES, RecoveredTurnHandle, RequestSlots, ReresolvedModel, Turn,
        TurnContextCapacity, TurnRecoveryOperationId, TurnResult, TurnResultHandle,
        TurnResultOutcome, TurnSubmission, WeakAgent, abandon_and_release_many,
        checkpoint_closes_on_reopen, checkpoint_recovery_verdict, close_admission,
        compile_explicit_runtime_profile, lease_released, shutdown_for_transfer,
    };
    pub use crate::session::{
        AcquireBranch, ArchiveReceipt, ArchiveSession, BranchHead, BranchLease, BranchRevision,
        BranchTransitionMode, CheckpointObjectRef, CommitCheckpoint, CommitOperation,
        CommitReceipt, CommittedCheckpoint, CreateOperation, CreateReceipt, CreateSession,
        DeleteReceipt, DeleteSession, DurabilityReport, ForkBranch, ForkOperation, ForkReceipt,
        LoadedBranch, Page, PruneBranch, PruneReceipt, RecoverFork, SessionHead, SessionQuery,
        SessionRef, SessionRevision, SessionStore, SessionStoreCapacity, SessionStoreError,
        SessionSummary, SessionWorkflowId, StoreAdmission, StoreBoundUsage, StoreCapacityBound,
        StoreFuture, StoreIntegrityReason, StoreInvalidReason, StoreReclaimMode,
        StoreReclaimReport, StoreUsage, StoredCheckpoint,
    };
    pub use crate::telemetry::{
        ChildProcessKind, CompactionRefusalReason, CredentialProviderLabel,
        CredentialRefreshDetail, CredentialRefreshFailureClass, CredentialRefreshFailureReason,
        CredentialRefreshPhase, DurabilityAttemptKind, DurabilityAttemptOutcome, HttpStatusClass,
        JobHintSource, JobNoticeDelivery, JobPreemption, JobTool, TelemetryContext,
        TelemetryEmitter, TelemetryObservation, TelemetryObservationKind, TelemetrySink,
        TelemetrySinkError, TelemetryStatus, TelemetryTerminalStatus,
    };
    pub use crate::tool_runtime::{
        AuthorizedToolInvocation, ClaimedToolInvocation, EffectAuthorization, EffectDispatchIntent,
        PendingEffectDecision, TokioToolTaskOwner, TokioToolTimer, ToolDispatchObserver,
        ToolDispatchObserverFuture, ToolDispatchPermit, ToolDispatchReport, ToolDispatchState,
        ToolExecutionControl, ToolRuntime, ToolRuntimeConfig, ToolScheduler, ToolSchedulerStatus,
        ToolTask, ToolTaskDrain, ToolTaskFuture, ToolTaskHandle, ToolTaskJoin, ToolTaskOwner,
        ToolTaskOwnerStatus, ToolTaskStatus, ToolTimer, ToolTimerFuture,
    };
    pub use crate::usage::{TurnUsage, UsageAggregationError, UsageTotals};
    pub use plakous_model::{
        Actor, AgentId, AttemptId, BranchId, CallId, CapabilityRequirement, CheckpointId, Clock,
        ContentBlock, ContextCapacityFailure, ConversationItem, DataClassification, Digest,
        DigestDomain, EventId, GenerationLimits, IdGenerator, ModelAdapterProfileId, ModelFamilyId,
        ModelId, NamespaceId, ProtocolId, Provenance, ReasoningEffort, ReasoningPolicy,
        ReplayEnvelope, ReplayKindId, ReplayScope, RunId, SessionId, SystemClock, TextBlock,
        Timestamp, ToolCallBlock, ToolCallState, ToolCorrelation, ToolEffect, ToolExecutionOwner,
        ToolName, ToolResultBlock, ToolResultKind, ToolResultPart, Usage,
    };

    /// Unsupported facade composition bridge.
    pub mod facade_support {
        pub use crate::facade_support::*;
    }
}

pub(crate) use crate::manifest::{MAX_RUNTIME_CHECKPOINT_BYTES, MAX_RUNTIME_RECORDS};

pub(crate) use crate::rehydration::{HarnessPlanReadiness, rehydrate_harness_plan};
#[cfg_attr(
    not(test),
    allow(
        unused_imports,
        reason = "crate-root implementation paths are also exercised by in-crate tests"
    )
)]
pub(crate) use internal::{
    AcquireBranch, ActorCommandEffect, ActorCommandReply, ActorControl, AgentCompactSessionOutcome,
    AgentCompactSessionRequest, AgentCompactionReason, AgentContextStatusRequest,
    AgentSessionEndpoint, AgentSessionEndpointError, ArchiveReceipt, ArchiveSession,
    BindingReference, BranchHead, BranchLease, BranchRevision, BranchTransitionMode,
    CheckpointBundleError, CheckpointObjectBundle, CheckpointObjectRef, CommitCheckpoint,
    CommitOperation, CommitReceipt, CommittedCheckpoint, ContextEvaluationAudit, CreateOperation,
    CreateReceipt, CreateSession, DeleteReceipt, DeleteSession, DurabilityReport,
    DurableCommandDisposition, DurableCommandRecord, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, EventContext, EventHub, EventHubLimits,
    EventPipeline, FamilyArtifact, ForkBranch, ForkLineage, ForkReceipt,
    HARNESS_PLAN_COMPONENT_FORMAT_VERSION, HarnessManifestCodecError, HarnessPlanComponentEnvelope,
    LoadedBranch, ModelRouteBinding, Page, PendingEffectDecisionState, ProducerProvenance,
    PruneBranch, PruneReceipt, RUNTIME_FORMAT_EPOCH, RUNTIME_POLICY_REVISION,
    ReconstructiveComponent, RecoverFork, RecoveryAction, RecoveryHandlerSignature,
    RecoveryObligation, RecoveryObligationKind, RevalidationFact, RuntimeCanonicalState,
    RuntimeCheckpoint, RuntimeCheckpointCodec, RuntimeCheckpointKind, RuntimeComponentKind,
    RuntimeDurability, RuntimeLifecycle, RuntimeManifest, RuntimeManifestError,
    RuntimeObjectAddress, RuntimeObjectKind, RuntimeOperationLedger, RuntimeRehydration,
    RuntimeRehydrationError, RuntimeRevalidationWitness, RuntimeTopology, RuntimeWorkingRoot,
    SemanticCompactionPublication, SessionHead, SessionQuery, SessionRef, SessionRevision,
    SessionStore, SessionStoreCapacity, SessionStoreError, SessionSummary, SessionWorkflowId,
    StoreAdmission, StoreBoundUsage, StoreFuture, StoreUsage, StoredCheckpoint,
    ToolRuntimeManifest, WorkingRecoveryPhase, submit_actor_command,
};

pub use internal::{
    AbandonedTurnClaim, AbandonedTurnReport, AbandonmentCause, Actor, Agent, AgentBuilder,
    AgentConfig, AgentError, AgentErrorKind, AgentEvent, AgentEventKind, AgentId,
    AgentRequestPolicy, AgentViewReducer, AgentViewSnapshot, AgentViewState, AttemptId, BranchId,
    CachePreferenceStrategy, CallId, CancellationStatus, CapabilityRequirement, CheckpointId,
    ChildExitEscalation, ChildProcessExit, ChildProcessKind, Clock, CommandAck,
    CommandAckDisposition, CommandId, CompactionEventOutcome, CompactionEventReason,
    CompactionRefusalReason, CompactionRung, CompactionSummary, CompilerModelFacts,
    CompilerRuntimeFacts, CompilerToolFacts, ContentBlock, ContextCapacityFailure,
    ContextOverflowRemedy, ContextSourceCatalog, ContextSourceResolver, ContextSourceSelector,
    ContinuationPreferenceStrategy, ControlError, ControlErrorKind, ConversationItem,
    ConversationItemView, CredentialProviderLabel, CredentialRefreshDetail,
    CredentialRefreshFailureClass, CredentialRefreshFailureReason, CredentialRefreshPhase,
    DEFAULT_STORE_OPERATION_TIMEOUT, DataClassification, Digest, DigestDomain,
    DurabilityAttemptKind, DurabilityAttemptOutcome, EffectDecisionChoice, EffectDecisionRef,
    EffectDecisionResolution, EffectDecisionStatus, EventCursor, EventDelivery, EventDeliveryClass,
    EventId, EventLossKind, EventLostRange, EventOrigin, EventScope, EventSequence,
    EventSubscription, EventToolProgress, EventVisibility, FeatureSupportFact, GenerationLimits,
    HarnessCompilerInputs, HarnessProfileCompiler, HarnessProfileId, HarnessProfileProvenance,
    HarnessProfileSpec, HarnessProfileSpecBuilder, HttpStatusClass, IdGenerator,
    InstructionCompositionStrategy, JobHintSource, JobNoticeDelivery, JobPreemption, JobTool,
    MAX_INSTRUCTION_BYTES, MAX_USER_IMAGE_BYTES, MAX_USER_IMAGES, MIN_EVENT_DEBT_BYTES,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelReresolutionError,
    ModelReresolutionOperation, ModelReresolver, NamespaceId, NormalizedModelEventKind,
    PendingEffectDecisionView, ProfileError, ProfileSemanticVersion, ProtocolId, Provenance,
    RUNTIME_EXECUTOR_STACK_BYTES, ReasoningEffort, ReasoningPolicy, ReasoningPreferenceStrategy,
    RecoveredTurnHandle, ReducerError, RegisteredContextSource, ReplayEnvelope, ReplayKindId,
    ReplayScope, RequestSlots, ReresolvedModel, ResolvedCachePlan, ResolvedContextSourceFact,
    ResolvedContinuationPlan, ResolvedHarnessPlan, ResolvedInstructionPlan, ResolvedReasoningPlan,
    ResolvedToolExposurePlan, ResumeOutcome, RunId, SessionId, SteeringStatus, SubscriptionEnd,
    SubscriptionLimitError, SubscriptionLimits, SystemClock, TelemetryContext, TelemetryEmitter,
    TelemetryObservation, TelemetryObservationKind, TelemetrySink, TelemetrySinkError,
    TelemetryStatus, TelemetryTerminalStatus, TextBlock, Timestamp, ToolActivityView,
    ToolAuditIdentity, ToolCallAttemptIdentity, ToolCallBlock, ToolCallObservation, ToolCallState,
    ToolCallerClass, ToolCallerLineageView, ToolCorrelation, ToolEffect, ToolExecutionOwner,
    ToolExposureOverride, ToolExposureStrategy, ToolName, ToolProgressSummary, ToolResultBlock,
    ToolResultKind, ToolResultObservation, ToolResultPart, Turn, TurnCommand, TurnContextCapacity,
    TurnControl, TurnEventError, TurnEvents, TurnRecoveryOperationId, TurnResult, TurnResultHandle,
    TurnResultOutcome, TurnSubmission, TurnUsage, TurnView, TurnViewState, Usage,
    UsageAggregationError, UsageTotals, UserInput, UserPart, VerifiedContextSourceRef,
    ViewAttachment, ViewError, ViewRequest, WeakAgent, compile_explicit_profile,
    compile_explicit_runtime_profile, compiler_model_facts_from_model,
    recommended_neutral_coding_profile,
};

/// Session-owned process observations and optional host registry.
pub use plakous_tools::{JobConfig, JobHost, JobOwner, JobStatus, JobStopReason, JobView};
