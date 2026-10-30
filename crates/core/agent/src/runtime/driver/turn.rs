//! Drive root turns and resume admitted continuations.

mod admission;
mod decisions;
mod restore;
mod rounds;

use crate::runtime::ArtifactTransition;
use crate::runtime::agent::context_overflow_remedy;
use crate::runtime::checkpoint::identity::history_segment_kind;
use crate::runtime::compaction::request::apply_completed_model_round_occupancy;
use crate::runtime::continuation::{
    AwaitEffectDecisionV1, AwaitServerPauseV1, AwaitToolBatchV2, ContinuationBindingReference,
    ContinuationRoot, ContinuationSteering, ContinuationToolSlot, DurableAgentConfig,
    RestoredContinuationFrame, RestoredHostedFrame, RuntimeContinuation, ToolBatchContinuation,
    runtime_component, runtime_manifest_error,
};
use crate::runtime::control::{effect_intent_bytes, provider_artifact_transition};
use crate::runtime::driver::ToolDispatchOrigin;
use crate::runtime::driver::execution::{
    canonicalize_observed_tool_execution, publish_observation_clip,
};
use crate::runtime::driver::model_cleanup::resolve_model_operation;
use crate::runtime::driver::operations;
use crate::runtime::driver::operations::{CommitPlan, TurnConsequence};
use crate::runtime::driver::{
    AdmittedToolCall, AdmittedToolExecution, AuthorizationOutcome, Driver,
    DurableEffectIntentEnvelope, RoundRequest, ToolAdmission, ToolBatchResult,
};
use crate::runtime::operation_record::{
    DecidedRemedy, FailTurnShape, ProviderAttemptResult, ProviderOutcome, RoundDecision,
    canonical_tool_operation_binding, operation_turn_id,
};
use crate::runtime::round_classification::{
    HostedRoundDisposition, RoundClassificationError, RoundClassificationInput, classify_round,
    output_limit_retry_eligible, round_failure_action,
};
use crate::runtime::terminal::{
    DurableAgentError, actor_panic_error, cancelled_error, remove_known_turn, turn_lifecycle_error,
};
use crate::runtime::{
    DriverRestoredStartup, OperationId, PromptCommand, QueuedSteering, RestoredEffectDecision,
    TerminalProposal, ToolExecutionResult, TurnResult,
};
use crate::tool_runtime::PendingEffectDecision;
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, EventContext, RuntimeComponentKind,
    SessionStoreError, StoreAdmission, ToolAuditIdentity,
};
use futures_util::FutureExt;
use plakous_model::{
    Actor, AttemptId, CallId, ContentBlock, ConversationItem, Digest, EntryId, HistorySegment,
    HistorySegmentKind, ModelError, ModelErrorKind, ModelErrorPhase, ModelOperationRoot,
    ModelRequest, ModelResponse, ModelRetryRisk, ModelRetryability, Provenance, TextBlock,
    ToolCallBlock, TurnId, Usage, ValidatedRequestPlan, WorkingProjection,
};
use plakous_tools::{ToolInvocation, ToolProjectionReceiptV2};
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The note the harness injects when a turn reaches its model-operation limit.
///
/// It is the visible marker in the transcript and the JSONL: a reader sees why
/// the assistant stopped exploring, and the model is told plainly that the only
/// thing left to do is answer. The round that follows it carries
/// `ToolChoice::None`, so the instruction is enforced on the wire rather than
/// merely requested.
pub const OPERATION_BUDGET_WRAP_UP_NOTE: &str = concat!(
    "[harness] This turn has used its entire model-operation budget, so no ",
    "further tool calls are possible. Answer now with your findings so far, ",
    "and say plainly what you were unable to finish."
);

/// The note the harness injects after discarding a response that reached its
/// output bound before completion.
///
/// No truncated call ran and the response is not in the conversation,
/// so without this the model would not know its last attempt failed, and would
/// most likely repeat it. It names the prepared wire maximum when the provider
/// recorded one, and otherwise the request's product bound, so the model can
/// size its retry.
pub(super) fn output_limit_retry_note(bound: Option<NonZeroU32>) -> String {
    let limit = bound.map_or_else(
        || OUTPUT_LIMIT_UNKNOWN.to_owned(),
        |bound| format!("{OUTPUT_LIMIT_KNOWN_HEAD}{bound}{OUTPUT_LIMIT_KNOWN_TAIL}"),
    );
    format!("{OUTPUT_LIMIT_RETRY_HEAD}{limit}{OUTPUT_LIMIT_RETRY_TAIL}")
}

const OUTPUT_LIMIT_RETRY_HEAD: &str = "[harness] Your previous response reached ";
const OUTPUT_LIMIT_RETRY_TAIL: &str = concat!(
    " before completion, so the response was discarded and no tool call ran. ",
    "Retry with a shorter answer or less output per call: split a large file or ",
    "patch into several smaller writes or edits, or reason more briefly."
);
const OUTPUT_LIMIT_UNKNOWN: &str = "the provider's output limit";
const OUTPUT_LIMIT_KNOWN_HEAD: &str = "its output limit of ";
const OUTPUT_LIMIT_KNOWN_TAIL: &str = " tokens";

/// True for exactly the text of a note this driver appends to a running turn.
///
/// Those notes inherit the turn's provenance; in an operator turn they are
/// `UserInput`, and only their text tells them from what the operator typed.
pub(super) fn is_turn_harness_note(text: &str) -> bool {
    text == OPERATION_BUDGET_WRAP_UP_NOTE || is_output_limit_retry_note(text)
}

fn is_output_limit_retry_note(text: &str) -> bool {
    let Some(limit) = text
        .strip_prefix(OUTPUT_LIMIT_RETRY_HEAD)
        .and_then(|rest| rest.strip_suffix(OUTPUT_LIMIT_RETRY_TAIL))
    else {
        return false;
    };
    limit == OUTPUT_LIMIT_UNKNOWN
        || limit
            .strip_prefix(OUTPUT_LIMIT_KNOWN_HEAD)
            .and_then(|rest| rest.strip_suffix(OUTPUT_LIMIT_KNOWN_TAIL))
            .and_then(|bound| bound.parse::<NonZeroU32>().ok())
            .is_some_and(|bound| output_limit_retry_note(Some(bound)) == text)
}

/// The history segment a completed response commits, if it has output.
fn response_segment(response: &ModelResponse) -> Result<Option<HistorySegment>, AgentError> {
    if response.output().is_empty() {
        return Ok(None);
    }
    HistorySegment::new(
        history_segment_kind(response.output()),
        response.output().to_vec(),
    )
    .map(Some)
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "canonical history rejected an atomic semantic group",
        )
    })
}

/// The retry a settled failed attempt leads to.
enum AttemptRetry {
    ContentFilter,
    OutputLimit,
    OverflowFold,
}

/// How a round without customer tools continues the turn.
enum SettledRound {
    /// The loop resumes, holding the hosted continuation operation, if any.
    Continue(Option<OperationId>),
    Finished(Box<TurnResult>),
}

/// How a failed provider attempt continues the turn.
enum AttemptFailure {
    Retry(RoundDecision),
    EndsTurn(AgentError),
}
