//! Post-round decisions shared by live driving and durable-evidence recovery.

use crate::runtime::operation_record::collect_local_calls;
use plakous_model::{
    ConversationView, FinishReason, ModelError, ModelErrorKind, ModelErrorPhase, ModelResponse,
    ToolCallBlock,
};
use std::num::NonZeroU32;

#[derive(Clone, Copy)]
pub(super) struct RoundClassificationInput<'a> {
    pub(super) response: &'a ModelResponse,
    pub(super) history: &'a ConversationView,
    /// The live counted round exceeds the admitted cap. Terminal is budget-
    /// invariant. Recovery passes false for completion/abandonment decisions:
    /// never claim `RoundLimit` from recovered evidence; abandon instead.
    pub(super) over_budget: bool,
    /// The prepared receipt's output bound, not today's model configuration.
    pub(super) wire_output_limit: Option<NonZeroU32>,
}

#[derive(Debug)]
pub(super) enum HostedRoundDisposition {
    Terminal,
    AssistantContinuation,
    CustomerToolsRequested(Vec<ToolCallBlock>),
    ServerPaused,
}

impl HostedRoundDisposition {
    /// Called after provider finalization, only for terminal/assistant rounds.
    /// Steering is sampled there, not before the driver's intervening awaits.
    pub(super) fn continuing(&self, has_pending_steering: bool) -> bool {
        has_pending_steering || matches!(self, Self::AssistantContinuation)
    }
}

#[derive(Debug)]
pub(super) enum RoundClassificationError {
    InvalidCalls(ModelError),
    /// Only this response-validation failure enters the output-limit retry path.
    IncompleteLocalCall(ModelError),
    RoundLimit,
    InvalidDisposition(ModelError),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RoundFailureAction {
    pub(super) phase: ModelErrorPhase,
    pub(super) retry_output_limit: bool,
}

pub(super) fn round_failure_action(error: &RoundClassificationError) -> RoundFailureAction {
    RoundFailureAction {
        phase: ModelErrorPhase::Terminal,
        retry_output_limit: matches!(error, RoundClassificationError::IncompleteLocalCall(_)),
    }
}

pub(super) fn output_limit_retry_eligible(
    kind: Option<ModelErrorKind>,
    retry_taken: bool,
    over_budget: bool,
) -> bool {
    kind == Some(ModelErrorKind::OutputLimit) && !retry_taken && !over_budget
}

pub(super) fn classify_round(
    input: RoundClassificationInput<'_>,
) -> Result<HostedRoundDisposition, RoundClassificationError> {
    let response = input.response;
    let calls = collect_local_calls(response, input.history)
        .map_err(RoundClassificationError::InvalidCalls)?;
    if !calls.is_empty() && response.finish().reason() != FinishReason::ToolCalls {
        let error = if response.finish().reason() == FinishReason::Length {
            let mut error = ModelError::new(
                ModelErrorKind::OutputLimit,
                "response reached its output bound before its tool call was complete",
            );
            if let Some(tokens) = input.wire_output_limit {
                error.fill_missing_wire_output_limit(tokens);
            }
            error
        } else {
            ModelError::new(
                ModelErrorKind::IncompleteResponse,
                "model proposed a local tool call without a complete tool-call finish",
            )
        };
        return Err(RoundClassificationError::IncompleteLocalCall(error));
    }
    let disposition = if calls.is_empty() {
        match response.finish().reason() {
            FinishReason::ProviderContinuation => HostedRoundDisposition::ServerPaused,
            FinishReason::AssistantContinuation => HostedRoundDisposition::AssistantContinuation,
            _ => HostedRoundDisposition::Terminal,
        }
    } else {
        HostedRoundDisposition::CustomerToolsRequested(calls.into_iter().cloned().collect())
    };
    if input.over_budget
        && matches!(
            &disposition,
            HostedRoundDisposition::CustomerToolsRequested(_)
                | HostedRoundDisposition::AssistantContinuation
        )
    {
        return Err(RoundClassificationError::RoundLimit);
    }
    if !matches!(
        &disposition,
        HostedRoundDisposition::CustomerToolsRequested(_)
    ) && response.finish().reason() == FinishReason::ToolCalls
    {
        return Err(RoundClassificationError::InvalidDisposition(
            ModelError::new(
                ModelErrorKind::InvalidEvent,
                "model reported tool-call completion without a dispatchable local call",
            ),
        ));
    }
    if matches!(&disposition, HostedRoundDisposition::ServerPaused)
        && response.family_artifact_candidate().is_none()
    {
        return Err(RoundClassificationError::InvalidDisposition(
            ModelError::new(
                ModelErrorKind::IncompleteResponse,
                "provider server pause omitted its exact family artifact",
            ),
        ));
    }
    Ok(disposition)
}

#[cfg(test)]
mod tests;
