//! Joined model cancellation and terminal error selection.

use crate::{AgentError, AgentErrorKind};
use futures_util::StreamExt;
use plakous_model::{ModelError, ModelErrorKind, ModelErrorPhase, ModelErrorTarget, ModelResponse};
use std::time::Duration;
pub(super) async fn join_cancelled_control_start(
    start: plakous_model::StartModelCallWithControl,
    timeout: Duration,
) -> ModelCleanupOutcome {
    match tokio::time::timeout(timeout, drain_cancelled_control_start(start)).await {
        Ok(result) => result,
        Err(_) => ModelCleanupOutcome::Unjoined(model_cleanup_timeout_error()),
    }
}

async fn drain_cancelled_control_start(
    mut start: plakous_model::StartModelCallWithControl,
) -> ModelCleanupOutcome {
    match start.as_mut().await {
        Ok(mut stream) => drain_cancelled_control_stream(&mut stream).await,
        Err(error) => ModelCleanupOutcome::Joined(Some(error)),
    }
}

fn recovery_authoritative(error: &AgentError) -> bool {
    error.kind() == AgentErrorKind::OutcomeUnknown
        || error
            .model_error()
            .is_some_and(|model| model.paid_recovery().is_some())
}

pub(super) fn resolve_model_operation(
    result: Result<ModelResponse, AgentError>,
    usage_result: Result<(), AgentError>,
) -> Result<ModelResponse, AgentError> {
    match result {
        Err(error) if recovery_authoritative(&error) => Err(error),
        Ok(response) => {
            usage_result?;
            Ok(response)
        }
        Err(error) => {
            usage_result?;
            Err(error)
        }
    }
}

pub(super) async fn join_cancelled_control_stream(
    stream: &mut plakous_model::ModelControlStream,
    timeout: Duration,
) -> ModelCleanupOutcome {
    match tokio::time::timeout(timeout, drain_cancelled_control_stream(stream)).await {
        Ok(result) => result,
        Err(_) => ModelCleanupOutcome::Unjoined(model_cleanup_timeout_error()),
    }
}

async fn drain_cancelled_control_stream(
    stream: &mut plakous_model::ModelControlStream,
) -> ModelCleanupOutcome {
    const MAX_CLEANUP_EVENTS: usize = 100_000;
    for _ in 0..MAX_CLEANUP_EVENTS {
        match stream.next().await {
            Some(Ok(_)) => {}
            Some(Err(error)) => return ModelCleanupOutcome::Joined(Some(error)),
            None => return ModelCleanupOutcome::Joined(None),
        }
    }
    ModelCleanupOutcome::Unjoined(ModelError::new(
        ModelErrorKind::ResourceLimit,
        "cancelled model control stream exceeded its joined cleanup event limit",
    ))
}

pub(in crate::runtime) fn ambiguous_compaction_error(message: &'static str) -> AgentError {
    AgentError::new(AgentErrorKind::OutcomeUnknown, message)
}

/// A dispatched compaction its own driver closed before any response was
/// applied. Definite, not ambiguous: a compaction changes nothing until its
/// own commit, which this driver owns and never made, so the only thing
/// nobody can know is whether it was billed. It is never redispatched.
pub(in crate::runtime) fn abandoned_compaction_error(
    kind: AgentErrorKind,
    message: &'static str,
) -> AgentError {
    AgentError::new(kind, message)
}

fn model_cleanup_timeout_error() -> ModelError {
    ModelError::new(
        ModelErrorKind::ResourceLimit,
        "cancelled model operation exceeded its joined cleanup time limit",
    )
}

pub(super) fn agent_model_timeout(target: &ModelErrorTarget, message: &'static str) -> AgentError {
    let mut error = ModelError::new(ModelErrorKind::Timeout, message);
    error.ensure_context(target, ModelErrorPhase::Operation);
    AgentError::from_model(error)
}

pub(in crate::runtime) enum ModelCleanupOutcome {
    Joined(Option<ModelError>),
    Unjoined(ModelError),
}
