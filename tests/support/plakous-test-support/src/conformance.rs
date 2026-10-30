//! Provider-neutral execution used by deterministic conformance suites.

use std::sync::Arc;

use futures_util::StreamExt;

use plakous_model::{
    LanguageModel, ModelCallContext, ModelCallRecoveryError, ModelCallRecoveryRequest,
    ModelCallRecoveryResult, ModelError, ModelEvent, ModelPreparationContext,
    ModelPreparationError, ModelRequest, ModelResponse, ResolvedModelDescriptor, ValidationMode,
};

/// Runs and validates one read-only provider-call recovery classification.
///
/// This helper never prepares or starts a model call. Ambiguous, unsupported,
/// and unavailable results remain classifications and cannot authorize replay.
///
/// # Errors
///
/// Returns provider recovery construction errors or rejects a result bound to
/// another request, plan, response, usage fact, or evidence receipt.
pub async fn recover_model_operation(
    model: Arc<dyn LanguageModel>,
    request: ModelCallRecoveryRequest,
) -> Result<ModelCallRecoveryResult, ModelCallRecoveryError> {
    let result = model.recover_call(request.clone()).await?;
    result.validate_for_request(&request)?;
    Ok(result)
}

/// Runs one prepared model operation to its self-contained terminal aggregate.
///
/// This helper owns no retry, tool, ledger, or transport policy. It exists so
/// every [`LanguageModel`] implementation can be exercised through the exact
/// public seam with identical terminal and EOF classification. Nonterminal
/// events are bounded observations, not construction authority for the result;
/// observation loss or a projector gap therefore cannot invalidate an exact
/// terminal aggregate.
///
/// # Errors
///
/// Returns preparation/start/stream errors unchanged, rejects a terminal bound
/// to another prepared plan or any event after terminal, and returns an
/// incomplete-response error for EOF before the unique terminal aggregate.
pub async fn run_model_operation(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    context: ModelCallContext,
) -> Result<ModelResponse, ModelError> {
    let prepared_at = request.metadata().created_at();
    let plan = descriptor
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)?;
    let plan_digest = plan.digest();
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await
        .map_err(ModelPreparationError::from)
        .map_err(ModelError::from)?;
    let prepared = Arc::clone(&model)
        .prepare_call(plan, ModelPreparationContext::new(authority, prepared_at))
        .map_err(ModelError::from)?;
    let mut stream = prepared.start(context).await?;
    let mut completed = None;

    while let Some(event) = stream.next().await {
        let event = event?;
        if completed.is_some() {
            return Err(ModelError::new(
                plakous_model::ModelErrorKind::InvalidEvent,
                "model operation produced an event after its terminal aggregate",
            ));
        }
        let ModelEvent::ResponseCompleted(response) = event else {
            continue;
        };
        response.validate_for_commit()?;
        if response.plan_digest() != plan_digest {
            return Err(ModelError::new(
                plakous_model::ModelErrorKind::InvalidEvent,
                "model terminal aggregate does not match the prepared request plan",
            ));
        }
        completed = Some(response);
    }
    completed.ok_or_else(ModelError::incomplete)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
