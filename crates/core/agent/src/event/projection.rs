//! Atomic attach state and private event-publication validation.

mod attach;
mod conversation;
pub use attach::{
    AgentViewReducer, AgentViewSnapshot, AgentViewState, PendingEffectDecisionView, ReducerError,
    TurnView, TurnViewState,
};
pub use conversation::ConversationItemView;

use super::hub::subscription::ViewError;
use super::types::{
    AgentEvent, AgentEventKind, MAX_LIFETIME_IDENTITIES, ToolAuditIdentity, ToolProgressSummary,
};
use attach::{AttachDelta, AttachState};
use plakous_model::{AttemptId, CallId, TurnId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type CallBinding = (TurnId, AttemptId, CallId);

#[derive(Clone)]
struct PublishedToolCall {
    identity: ToolAuditIdentity,
    progress: Option<(ToolProgressSummary, Option<Arc<str>>)>,
}

pub(super) struct ProjectionState {
    pub(super) attach: AttachState,
    pub(super) recovery_promotable_turn: Option<TurnId>,
    active_attempts: BTreeSet<(TurnId, AttemptId)>,
    calls: BTreeMap<CallBinding, PublishedToolCall>,
    retired_calls: BTreeSet<CallBinding>,
}

impl ProjectionState {
    pub(super) fn new(retained_turns: usize) -> Self {
        Self {
            attach: AttachState::new(retained_turns),
            recovery_promotable_turn: None,
            active_attempts: BTreeSet::new(),
            calls: BTreeMap::new(),
            retired_calls: BTreeSet::new(),
        }
    }
}

pub(super) struct TransitionPlan {
    attach: AttachDelta,
    validation: ValidationDelta,
}

enum ValidationDelta {
    Close,
    Attempt((TurnId, AttemptId)),
    Start(CallBinding, PublishedToolCall),
    Progress(CallBinding, PublishedToolCall),
    Complete(CallBinding),
    Conversation(TurnId),
    Terminal(TurnId),
    Noop,
}

fn event_context_shape_is_valid(event: &AgentEvent) -> bool {
    let shape_is_valid = match event.kind() {
        AgentEventKind::AgentClosed => {
            event.turn_id.is_none() && event.attempt_id.is_none() && event.call_id.is_none()
        }
        AgentEventKind::JobPromoted { .. } | AgentEventKind::JobFinished { .. } => {
            event.attempt_id.is_none() && event.call_id.is_none()
        }
        AgentEventKind::TurnStarted
        | AgentEventKind::OutputBoundRederived { .. }
        | AgentEventKind::SteeringQueued { .. }
        | AgentEventKind::SteeringApplied
        | AgentEventKind::OperationBudgetExhausted
        | AgentEventKind::ModelRequestQueued { .. }
        | AgentEventKind::ModelCredentialWait { .. }
        | AgentEventKind::OutputLimitRetry { .. }
        | AgentEventKind::ToolResultsReplaced { .. }
        | AgentEventKind::ConversationCommitted { .. }
        | AgentEventKind::CompactionStarted { .. }
        | AgentEventKind::CompactionFinished { .. }
        | AgentEventKind::TurnCompleted { .. }
        | AgentEventKind::TurnFailed { .. } => {
            event.turn_id.is_some() && event.attempt_id.is_none() && event.call_id.is_none()
        }
        AgentEventKind::ModelStarted
        | AgentEventKind::ResponseSettled { .. }
        | AgentEventKind::ModelDispatchStarted
        | AgentEventKind::RequestPrepared { .. }
        | AgentEventKind::Model { .. } => {
            event.turn_id.is_some() && event.attempt_id.is_some() && event.call_id.is_none()
        }
        AgentEventKind::EffectDecisionRequired { .. }
        | AgentEventKind::EffectDecisionResolved { .. }
        | AgentEventKind::ToolAuthorized { .. }
        | AgentEventKind::ToolStarted { .. }
        | AgentEventKind::ToolProgress { .. }
        | AgentEventKind::ToolPreparationRejected { .. }
        | AgentEventKind::ToolCompleted { .. }
        | AgentEventKind::ObservationClipped { .. }
        | AgentEventKind::ObservationReplaced { .. } => {
            event.turn_id.is_some() && event.attempt_id.is_some() && event.call_id.is_some()
        }
    };
    shape_is_valid
        && match event.kind() {
            AgentEventKind::ToolProgress {
                summary,
                progress: Some(progress),
                ..
            } => {
                event.call_id == Some(progress.call_id())
                    && *summary == ToolProgressSummary::from_event_progress(progress)
            }
            _ => true,
        }
}

pub(super) fn preflight_projection(
    state: &ProjectionState,
    event: &AgentEvent,
) -> Result<TransitionPlan, ViewError> {
    if !event_context_shape_is_valid(event) {
        return Err(ViewError::InvalidEvent);
    }
    let attach = state.attach.plan(event).map_err(|error| match error {
        ReducerError::FenceSaturated => ViewError::FenceSaturated,
        _ => ViewError::InvalidEvent,
    })?;
    if let Some(turn_id) = event.turn_id {
        let turn = state
            .attach
            .turns
            .iter()
            .find(|turn| turn.turn_id() == turn_id);
        let active = turn.is_some_and(|turn| turn.state() == TurnViewState::Running);
        match event.kind() {
            AgentEventKind::TurnStarted
            | AgentEventKind::TurnCompleted { .. }
            | AgentEventKind::TurnFailed { .. } => {}
            AgentEventKind::ConversationCommitted { .. } => {
                if !active && turn.is_none() && state.recovery_promotable_turn != Some(turn_id) {
                    return Err(ViewError::InvalidEvent);
                }
            }
            _ if !active => return Err(ViewError::InvalidEvent),
            _ => {}
        }
        if let Some(attempt_id) = event.attempt_id
            && !state.active_attempts.contains(&(turn_id, attempt_id))
            && matches!(
                event.kind(),
                AgentEventKind::EffectDecisionResolved { .. }
                    | AgentEventKind::ToolProgress { .. }
                    | AgentEventKind::ToolCompleted { .. }
            )
        {
            return Err(ViewError::InvalidEvent);
        }
    }
    let validation = plan_validation(state, event)?;
    Ok(TransitionPlan { attach, validation })
}

fn call_binding(event: &AgentEvent) -> Result<CallBinding, ViewError> {
    Ok((
        event.turn_id.ok_or(ViewError::InvalidEvent)?,
        event.attempt_id.ok_or(ViewError::InvalidEvent)?,
        event.call_id.ok_or(ViewError::InvalidEvent)?,
    ))
}

fn plan_validation(
    state: &ProjectionState,
    event: &AgentEvent,
) -> Result<ValidationDelta, ViewError> {
    match event.kind() {
        AgentEventKind::AgentClosed => Ok(ValidationDelta::Close),
        AgentEventKind::ToolStarted { identity, .. } => {
            let binding = call_binding(event)?;
            if let Some(existing) = state.calls.get(&binding) {
                return if existing.identity == *identity {
                    Ok(ValidationDelta::Noop)
                } else {
                    Err(ViewError::InvalidEvent)
                };
            }
            if state.retired_calls.contains(&binding) {
                return Err(ViewError::InvalidEvent);
            }
            Ok(ValidationDelta::Start(
                binding,
                PublishedToolCall {
                    identity: identity.clone(),
                    progress: None,
                },
            ))
        }
        AgentEventKind::ToolCompleted { identity, .. } => {
            let binding = call_binding(event)?;
            if !state
                .calls
                .get(&binding)
                .is_some_and(|call| call.identity == *identity)
            {
                return Err(ViewError::InvalidEvent);
            }
            if state.retired_calls.len() >= MAX_LIFETIME_IDENTITIES {
                return Err(ViewError::FenceSaturated);
            }
            Ok(ValidationDelta::Complete(binding))
        }
        AgentEventKind::ToolProgress {
            identity,
            summary,
            progress,
            ..
        } => {
            let binding = call_binding(event)?;
            let call = state.calls.get(&binding).ok_or(ViewError::InvalidEvent)?;
            if call.identity != *identity {
                return Err(ViewError::InvalidEvent);
            }
            let message = progress
                .as_ref()
                .map(|progress| Arc::from(progress.message()));
            if let Some((current, current_message)) = &call.progress {
                if summary.sequence() < current.sequence() {
                    return Ok(ValidationDelta::Noop);
                }
                if summary.sequence() == current.sequence() {
                    return if summary == current && &message == current_message {
                        Ok(ValidationDelta::Noop)
                    } else {
                        Err(ViewError::InvalidEvent)
                    };
                }
            }
            Ok(ValidationDelta::Progress(
                binding,
                PublishedToolCall {
                    identity: identity.clone(),
                    progress: Some((*summary, message)),
                },
            ))
        }
        AgentEventKind::ConversationCommitted { .. } => Ok(ValidationDelta::Conversation(
            event.turn_id.ok_or(ViewError::InvalidEvent)?,
        )),
        AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => Ok(
            ValidationDelta::Terminal(event.turn_id.ok_or(ViewError::InvalidEvent)?),
        ),
        AgentEventKind::ModelStarted
        | AgentEventKind::ModelDispatchStarted
        | AgentEventKind::ResponseSettled { .. }
        | AgentEventKind::Model { .. }
        | AgentEventKind::RequestPrepared { .. }
        | AgentEventKind::EffectDecisionRequired { .. }
        | AgentEventKind::ToolAuthorized { .. }
        | AgentEventKind::ToolPreparationRejected { .. } => {
            Ok(match (event.turn_id, event.attempt_id) {
                (Some(turn), Some(attempt)) => ValidationDelta::Attempt((turn, attempt)),
                _ => ValidationDelta::Noop,
            })
        }
        _ => Ok(ValidationDelta::Noop),
    }
}

pub(super) fn apply_projection_delta(state: &mut ProjectionState, delta: TransitionPlan) {
    state.attach.apply(delta.attach);
    match delta.validation {
        ValidationDelta::Close => {
            state.recovery_promotable_turn = None;
            state.calls.clear();
            state.active_attempts.clear();
        }
        ValidationDelta::Attempt(binding) => {
            state.active_attempts.insert(binding);
        }
        ValidationDelta::Start(binding, call) => {
            state.active_attempts.insert((binding.0, binding.1));
            state.calls.insert(binding, call);
        }
        ValidationDelta::Progress(binding, call) => {
            state.calls.insert(binding, call);
        }
        ValidationDelta::Complete(binding) => {
            state.calls.remove(&binding);
            state.retired_calls.insert(binding);
        }
        ValidationDelta::Conversation(turn) => {
            if state.recovery_promotable_turn == Some(turn) {
                state.recovery_promotable_turn = None;
            }
        }
        ValidationDelta::Terminal(turn) => {
            state.calls.retain(|(owner, _, _), _| *owner != turn);
            state.active_attempts.retain(|(owner, _)| *owner != turn);
        }
        ValidationDelta::Noop => {}
    }
}

#[cfg(test)]
mod tests;
