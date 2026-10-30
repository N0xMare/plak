//! Persisted continuation codecs and exact runtime reconstruction.

mod decode;
#[cfg(test)]
pub(super) use decode::decode_await_effect_decision_continuation;
#[cfg(test)]
pub(super) use decode::decode_hosted_continuation;
pub(super) use decode::decode_restored_continuation;
mod validate;
pub(super) use validate::previous_response;
#[cfg(test)]
pub(super) use validate::previous_response_metadata;
use validate::reconcile_hosted_tool_slots;
pub(in crate::runtime) use validate::restore_frame_plan;
use validate::validate_hosted_frame;
mod rehydrate;
pub(super) use rehydrate::reconstruct_persisted_tools;
pub(super) use rehydrate::rehydrate_runtime_indexes;
pub(super) use rehydrate::runtime_revalidation_witness;
pub(super) use rehydrate::terminal_record_at_checkpoint;
pub(super) use rehydrate::terminal_record_for_command;

use crate::local_composition::LocalCompositionConstruction;
use crate::runtime::build::stable_runtime_environment_component;
use crate::runtime::config::{AgentConfig, RUNTIME_COMPONENT_CODEC_REVISION};
use crate::runtime::control::{actor_command_effect, control_error_from_durable};
use crate::runtime::driver::execution::canonicalize_observed_tool_execution;
use crate::runtime::observation::ObservationClip;
use crate::runtime::operation_record::{
    ProviderOutcome, RoundDecision, TurnBudgets, collect_local_calls, provider_outcome,
    recorded_provider_response,
};
use crate::runtime::terminal::{
    DurableTerminalResultRecord, TerminalRecordRejection, read_terminal_result_record,
};
use crate::runtime::{
    CommandLedgerEntry, DurableCommandEnvelope, OperationId, ToolExecutionResult, ToolPresentation,
};
use crate::tool_runtime::ToolRuntime;
use crate::{
    ActorCommandEffect, AgentError, AgentErrorKind, BindingReference, CommandAck, CommandId,
    DurableCommandDisposition, DurableCommandRecord, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, EffectDecisionRef, FamilyArtifact, ProfileError,
    ReconstructiveComponent, RecoveryAction, RuntimeCheckpoint, RuntimeComponentKind,
    RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger, RuntimeRevalidationWitness,
    RuntimeWorkingRoot, SessionStoreError, TurnUsage, WorkingRecoveryPhase,
};
use plakous_model::{
    Actor, AttemptId, CompiledProviderConfiguration, ConversationItem, Digest, DigestDomain,
    EntryId, FinishReason, LanguageModel, ModelOperationRoot, ModelRequest, ModelResponse,
    PreparedCallRecord, Provenance, RuntimeModelAuthority, Timestamp, ToolCallBlock, TurnId,
    ValidatedRequestPlan, ValidationMode, WorkingProjection,
};
use plakous_tools::{
    FrozenToolExposureDescriptor, FrozenToolProjectionDescriptorV2, FrozenToolRegistry,
    FrozenToolRegistryDescriptor, TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, ToolBindingError, ToolExecutorBindings,
    WorkspacePolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
pub(super) mod command_feed;

/// One clipped observation recovered while reconciling a restored tool batch.
#[derive(Clone)]
pub(in crate::runtime) struct RestoredObservationClip {
    /// Tool whose observation was truncated.
    pub(in crate::runtime) name: plakous_model::ToolName,
    /// Call whose observation was truncated.
    pub(in crate::runtime) call_id: plakous_model::CallId,
    /// What the budget kept and removed.
    pub(in crate::runtime) clip: ObservationClip,
}

pub(super) fn runtime_component(
    kind: RuntimeComponentKind,
    bytes: Vec<u8>,
) -> Result<ReconstructiveComponent, AgentError> {
    ReconstructiveComponent::new(kind, RUNTIME_COMPONENT_CODEC_REVISION, bytes)
        .map_err(runtime_manifest_error)
}

pub(super) fn runtime_component_revision(
    kind: RuntimeComponentKind,
    revision: u32,
    bytes: Vec<u8>,
) -> Result<ReconstructiveComponent, AgentError> {
    ReconstructiveComponent::new(kind, revision, bytes).map_err(runtime_manifest_error)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned semantic validation failure"
)]
pub(super) fn tool_descriptor_error(error: plakous_tools::ToolDescriptorError) -> AgentError {
    AgentError::new(
        AgentErrorKind::InvalidInput,
        format!("tool semantics are not reconstruction-complete: {error}"),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned semantic validation failure"
)]
pub(super) fn runtime_manifest_error(error: crate::RuntimeManifestError) -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        format!("runtime manifest construction or validation failed: {error}"),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned semantic validation failure"
)]
pub(super) fn checkpoint_bundle_error(error: crate::CheckpointBundleError) -> AgentError {
    let kind = if error == crate::CheckpointBundleError::UnsupportedRevision {
        AgentErrorKind::IncompatibleSession
    } else {
        AgentErrorKind::Session
    };
    AgentError::new(
        kind,
        format!("runtime checkpoint object mapping failed closed: {error}"),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned rehydration failure"
)]
pub(super) fn runtime_rehydration_error(error: crate::RuntimeRehydrationError) -> AgentError {
    // An account, profile or admission mismatch is a revalidation failure of
    // the caller's binding. Everything else means this build cannot read or
    // rebuild what an earlier build stored.
    let kind = match error {
        crate::RuntimeRehydrationError::Revalidation(
            crate::recovery_scope::RevalidationError::BindingMismatch,
        ) => AgentErrorKind::InvalidInput,
        _ => AgentErrorKind::IncompatibleSession,
    };
    // The typed source names the exact recovery scope, binding, or manifest
    // fact that did not revalidate; without it a reopen refusal is undiagnosable.
    AgentError::new(
        kind,
        format!("runtime checkpoint recovery scope or revalidation failed: {error}"),
    )
}

pub(super) fn profile_construction_error(error: &ProfileError) -> AgentError {
    AgentError::new(
        AgentErrorKind::InvalidInput,
        format!("harness profile construction failed: {error}"),
    )
}

impl RestoredContinuationFrame {
    pub(super) const fn root(&self) -> &ContinuationRoot {
        match self {
            Self::EffectDecision(frame) => &frame.root,
            Self::ToolBatch(frame) => &frame.frame.root,
            Self::ServerPause(frame) => &frame.frame.root,
        }
    }

    pub(super) const fn config(&self) -> DurableAgentConfig {
        match self {
            Self::EffectDecision(frame) => frame.config,
            Self::ToolBatch(frame) => frame.frame.config,
            Self::ServerPause(frame) => frame.frame.config,
        }
    }

    pub(super) const fn usage(&self) -> TurnUsage {
        match self {
            Self::EffectDecision(frame) => frame.usage,
            Self::ToolBatch(frame) => frame.frame.usage,
            Self::ServerPause(frame) => frame.frame.usage,
        }
    }

    pub(super) const fn budgets(&self) -> TurnBudgets {
        match self {
            Self::EffectDecision(frame) => frame.budgets,
            Self::ToolBatch(frame) => frame.frame.budgets,
            Self::ServerPause(frame) => frame.frame.budgets,
        }
    }

    pub(super) fn request_anchor(&self) -> Option<Digest> {
        match self {
            Self::EffectDecision(frame) => frame.request_anchor,
            Self::ToolBatch(frame) => frame.frame.request_anchor,
            Self::ServerPause(frame) => frame.frame.request_anchor,
        }
    }

    pub(super) const fn context_anchor(&self) -> Option<plakous_model::ContextAnchor> {
        match self {
            Self::EffectDecision(frame) => frame.context_anchor,
            Self::ToolBatch(frame) => frame.frame.context_anchor,
            Self::ServerPause(frame) => frame.frame.context_anchor,
        }
    }

    pub(super) const fn provider_request(&self) -> &ModelRequest {
        match self {
            Self::EffectDecision(frame) => &frame.provider_request,
            Self::ToolBatch(frame) => &frame.frame.provider_request,
            Self::ServerPause(frame) => &frame.frame.provider_request,
        }
    }

    pub(super) fn steering(&self) -> &[ContinuationSteering] {
        match self {
            Self::EffectDecision(frame) => &frame.steering,
            Self::ToolBatch(frame) => &frame.frame.steering,
            Self::ServerPause(frame) => &frame.frame.steering,
        }
    }

    pub(super) fn pending_decision(&self) -> Option<EffectDecisionRef> {
        match self {
            Self::EffectDecision(frame) => Some(frame.decision.clone()),
            Self::ToolBatch(_) | Self::ServerPause(_) => None,
        }
    }
}

impl From<&BindingReference> for ContinuationBindingReference {
    fn from(reference: &BindingReference) -> Self {
        Self {
            authority: reference.authority().to_owned(),
            subject: reference.subject().to_owned(),
            revision: reference.revision(),
        }
    }
}

impl DurableAgentConfig {
    pub(super) fn from_config(config: AgentConfig) -> Result<Self, AgentError> {
        fn nanos(value: Duration) -> Result<u64, AgentError> {
            u64::try_from(value.as_nanos()).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "runtime duration exceeded the continuation codec bound",
                )
            })
        }
        Ok(Self {
            prompt_queue_capacity: config.prompt_queue_capacity,
            control_queue_capacity: config.control_queue_capacity,
            steering_queue_capacity: config.steering_queue_capacity,
            follow_up_queue_capacity: config.follow_up_queue_capacity,
            event_capacity: config.event_capacity,
            event_byte_capacity: config.event_byte_capacity,
            max_event_subscribers: config.max_event_subscribers,
            tool_progress_capacity: config.tool_progress_capacity,
            effect_decision_timeout_nanos: nanos(config.effect_decision_timeout)?,
            model_cleanup_timeout_nanos: nanos(config.model_cleanup_timeout)?,
            model_operation_timeout_nanos: nanos(config.model_operation_timeout)?,
        })
    }
}

/// The only serialized program counter accepted by the v14 driver.
///
/// This is semantic data at the clean pre-dispatch decision barrier, not a
/// serialized Rust future or task stack.  Keeping the variant closed makes a
/// new suspension point an explicit runtime/codec decision.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum RuntimeContinuation {
    AwaitEffectDecisionV1(Box<AwaitEffectDecisionV1>),
    AwaitToolBatchV2(Box<AwaitToolBatchV2>),
    AwaitServerPauseV1(Box<AwaitServerPauseV1>),
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct AwaitToolBatchV2 {
    pub(in crate::runtime) root: ContinuationRoot,
    pub(in crate::runtime) config: DurableAgentConfig,
    pub(in crate::runtime) budgets: TurnBudgets,
    pub(in crate::runtime) tool_calls: u64,
    pub(in crate::runtime) usage: TurnUsage,
    pub(in crate::runtime) context_anchor: Option<plakous_model::ContextAnchor>,
    pub(in crate::runtime) request_anchor: Option<Digest>,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_request: ModelRequest,
    pub(in crate::runtime) provider_plan_digest: Digest,
    pub(in crate::runtime) provider_response: ModelResponse,
    pub(in crate::runtime) working: WorkingProjection,
    pub(in crate::runtime) steering: Vec<ContinuationSteering>,
    pub(in crate::runtime) slots: Vec<ContinuationToolSlot>,
    pub(in crate::runtime) observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    pub(in crate::runtime) model_admission_reference: ContinuationBindingReference,
    pub(in crate::runtime) policy_reference: ContinuationBindingReference,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct AwaitServerPauseV1 {
    pub(in crate::runtime) root: ContinuationRoot,
    pub(in crate::runtime) config: DurableAgentConfig,
    pub(in crate::runtime) budgets: TurnBudgets,
    pub(in crate::runtime) tool_calls: u64,
    pub(in crate::runtime) usage: TurnUsage,
    pub(in crate::runtime) context_anchor: Option<plakous_model::ContextAnchor>,
    pub(in crate::runtime) request_anchor: Option<Digest>,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_request: ModelRequest,
    pub(in crate::runtime) provider_plan_digest: Digest,
    pub(in crate::runtime) provider_response: ModelResponse,
    pub(in crate::runtime) working: WorkingProjection,
    pub(in crate::runtime) steering: Vec<ContinuationSteering>,
    /// Observation bytes still available when the round paused.
    ///
    /// `AwaitToolBatchV2` has always carried this; the server-pause frame did
    /// not, so restoring one reset the budget to `None` and the tool results of
    /// the round after a pause were never measured against it. Nothing was
    /// clipped and nothing was reported.
    ///
    /// Capacity pressure is restored with the remaining allowance.
    pub(in crate::runtime) observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    pub(in crate::runtime) model_admission_reference: ContinuationBindingReference,
    pub(in crate::runtime) policy_reference: ContinuationBindingReference,
}

#[derive(Clone)]
pub(in crate::runtime) enum RestoredContinuationFrame {
    EffectDecision(Box<AwaitEffectDecisionV1>),
    ToolBatch(Box<RestoredHostedFrame<AwaitToolBatchV2>>),
    ServerPause(Box<RestoredHostedFrame<AwaitServerPauseV1>>),
}

#[derive(Clone)]
pub(in crate::runtime) struct RestoredHostedFrame<T> {
    pub(in crate::runtime) frame: T,
    pub(in crate::runtime) continuation_operation: OperationId,
    pub(in crate::runtime) remaining_observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    /// What the budget cut while canonicalizing this round's results.
    ///
    /// A durable session canonicalizes its tool results here, not at the
    /// driver's own call sites, so this is where an ordinary durable round's
    /// clips are found. The caller publishes them.
    pub(in crate::runtime) observation_clips: Vec<RestoredObservationClip>,
    pub(in crate::runtime) promoted: bool,
    pub(in crate::runtime) recovered_dispatched: usize,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct AwaitEffectDecisionV1 {
    pub(in crate::runtime) root: ContinuationRoot,
    pub(in crate::runtime) config: DurableAgentConfig,
    pub(in crate::runtime) budgets: TurnBudgets,
    pub(in crate::runtime) tool_calls: u64,
    pub(in crate::runtime) usage: TurnUsage,
    pub(in crate::runtime) context_anchor: Option<plakous_model::ContextAnchor>,
    pub(in crate::runtime) request_anchor: Option<Digest>,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_request: ModelRequest,
    pub(in crate::runtime) provider_plan_digest: Digest,
    pub(in crate::runtime) provider_response: ModelResponse,
    pub(in crate::runtime) working: WorkingProjection,
    pub(in crate::runtime) steering: Vec<ContinuationSteering>,
    pub(in crate::runtime) slots: Vec<ContinuationToolSlot>,
    pub(in crate::runtime) pending_index: u32,
    pub(in crate::runtime) decision_operation: OperationId,
    pub(in crate::runtime) decision: EffectDecisionRef,
    pub(in crate::runtime) decision_prompt: String,
    pub(in crate::runtime) decision_expires_at: Timestamp,
    pub(in crate::runtime) dispatch_capability_minted: bool,
    pub(in crate::runtime) hosted_continuation_operation: Option<OperationId>,
    pub(in crate::runtime) observation_budget:
        crate::runtime::observation::budget::ObservationBudget,
    pub(in crate::runtime) model_admission_reference: ContinuationBindingReference,
    pub(in crate::runtime) policy_reference: ContinuationBindingReference,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ContinuationRoot {
    pub(in crate::runtime) command_id: String,
    pub(in crate::runtime) turn_id: TurnId,
    pub(in crate::runtime) follow_up: bool,
    pub(in crate::runtime) operation_id: OperationId,
    pub(in crate::runtime) binding: Digest,
    /// The round cap the turn was admitted under; see `PromptCommand`.
    pub(in crate::runtime) max_model_rounds: usize,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ContinuationSteering {
    pub(in crate::runtime) content: Vec<plakous_model::ContentBlock>,
    pub(in crate::runtime) operation_id: OperationId,
}

#[derive(Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ContinuationBindingReference {
    authority: String,
    subject: String,
    revision: u64,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum ContinuationToolSlot {
    Completed {
        call: ToolCallBlock,
        result_entry_id: EntryId,
        item: ConversationItem,
        operation_id: OperationId,
    },
    PendingDecision {
        call: ToolCallBlock,
        result_entry_id: EntryId,
        binding_digest: Digest,
    },
    NotAdmitted {
        call: ToolCallBlock,
        result_entry_id: EntryId,
    },
}

#[derive(Clone, Copy, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) struct DurableAgentConfig {
    prompt_queue_capacity: usize,
    control_queue_capacity: usize,
    steering_queue_capacity: usize,
    follow_up_queue_capacity: usize,
    event_capacity: usize,
    event_byte_capacity: usize,
    max_event_subscribers: usize,
    tool_progress_capacity: usize,
    effect_decision_timeout_nanos: u64,
    model_cleanup_timeout_nanos: u64,
    model_operation_timeout_nanos: u64,
}

pub(in crate::runtime) struct ToolBatchContinuation<'a> {
    pub(in crate::runtime) tool_calls: usize,
    pub(in crate::runtime) provider_operation: OperationId,
    pub(in crate::runtime) provider_plan: &'a ValidatedRequestPlan,
    pub(in crate::runtime) provider_response: &'a ModelResponse,
    pub(in crate::runtime) calls: &'a [ToolCallBlock],
    pub(in crate::runtime) result_entry_ids: &'a [EntryId],
    pub(in crate::runtime) items: &'a [Option<ConversationItem>],
    pub(in crate::runtime) operations: &'a [OperationId],
    pub(in crate::runtime) source_index: usize,
}
