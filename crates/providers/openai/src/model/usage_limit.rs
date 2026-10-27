//! Codex-only recovery advice for subscription usage-limit refusals.

use plakous_model::{
    ModelError, ModelErrorContext, ModelErrorKind, ModelErrorPhase, ModelHttpStatus,
    ModelRetryRisk, ModelRetryability, ResolvedModelDescriptor,
};
use plakous_responses_dialect::ResponsesAttemptError;

pub(super) fn model_error(
    error: &ResponsesAttemptError,
    descriptor: &ResolvedModelDescriptor,
) -> Option<ModelError> {
    let ResponsesAttemptError::HttpStatus {
        status,
        provider: Some(provider),
        ..
    } = error
    else {
        return None;
    };
    let detail = provider.usage_limit_detail()?;
    let retryability = match error.facts().failure_risk() {
        plakous_attempt::FailureRisk::None => ModelRetryability::Permanent,
        plakous_attempt::FailureRisk::DuplicateInferenceOrBilling => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
        plakous_attempt::FailureRisk::HostedEffect => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        },
    };
    let mut context = ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        ModelErrorPhase::FirstEvent,
        retryability,
    )
    .ok()?
    .with_http_status(ModelHttpStatus::new(status.as_u16()).ok()?);
    if let Some(code) = provider
        .code()
        .and_then(|code| plakous_model::ModelProviderCode::new(code).ok())
    {
        context = context.with_provider_code(code);
    }
    if let Some(id) = provider
        .request_id()
        .and_then(|id| plakous_model::ModelProviderRequestId::new(id).ok())
    {
        context = context.with_provider_request_id(id);
    }
    ModelError::contextual(
        ModelErrorKind::QuotaExceeded,
        format!(
            "Codex {detail}; check your plan and credits at https://chatgpt.com/settings/usage."
        ),
        context,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_usage_limit_hook_retains_reset_and_subscription_plan_advice()
    -> Result<(), Box<dyn std::error::Error>> {
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            plakous_model::ProtocolId::new("openai.responses")?,
            plakous_model::RouteId::new("codex.subscription")?,
            plakous_model::ModelId::new("gpt-test")?,
            None,
            None,
            plakous_model::ModelAdapterProfileId::new("codex-test")?,
            plakous_model::CapabilitySet::new(
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
            ),
        );
        let fixture = serde_json::json!({"error":{"type":"usage_limit_reached","resets_at":4_102_444_812_u64}});
        let provider = plakous_responses_dialect::parse_provider_error_with_policy(
            &fixture["error"],
            plakous_responses_dialect::ResponsesResponseLimits::default(),
            plakous_responses_dialect::ResponsesProviderErrorPolicy::OpenAiCompatible,
        )?;
        let facts =
            plakous_attempt::TerminalFacts::from_snapshot(plakous_attempt::ObservationSnapshot {
                physical_attempts: 1,
                provider_acceptance: plakous_attempt::ProviderAcceptance::NotAccepted,
                visible_output: false,
                replay_safety: plakous_attempt::ReplaySafety::CostOnly,
                hosted_effects_possible: false,
                logical_success: false,
            });
        let error = ResponsesAttemptError::HttpStatus {
            physical: None,
            status: plakous_http::StatusCode::TOO_MANY_REQUESTS,
            class: plakous_responses_dialect::ResponsesHttpStatusClass::Quota,
            provider: Some(provider),
            retry_after: None,
            retry_disabled: true,
            facts,
        };
        let mapped = model_error(&error, &descriptor).ok_or("Codex mapping")?;
        assert_eq!(mapped.kind(), ModelErrorKind::QuotaExceeded);
        assert_eq!(
            mapped.context().map(ModelErrorContext::retryability),
            Some(ModelRetryability::Permanent)
        );
        assert!(mapped.message().contains("2100"));
        assert!(
            mapped
                .message()
                .contains("https://chatgpt.com/settings/usage")
        );
        Ok(())
    }
}
