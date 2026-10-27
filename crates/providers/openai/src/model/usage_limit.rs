//! Codex-only recovery advice for subscription usage-limit refusals.

use plakous_model::{ModelError, ResolvedModelDescriptor};
use plakous_responses_dialect::ResponsesAttemptError;

pub(super) fn model_error(
    error: &ResponsesAttemptError,
    descriptor: &ResolvedModelDescriptor,
) -> Option<ModelError> {
    let ResponsesAttemptError::HttpStatus {
        provider: Some(provider),
        ..
    } = error
    else {
        return None;
    };
    let detail = provider.usage_limit_detail()?;
    let default = error.contextual_http_error(descriptor)?;
    ModelError::contextual(
        default.kind(),
        format!(
            "Codex {detail}; check your plan and credits at https://chatgpt.com/settings/usage."
        ),
        default.context()?.clone(),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::{ModelErrorContext, ModelErrorKind, ModelRetryability};

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
        let fixture = serde_json::json!({"error":{"type":"usage_limit_reached","code":"usage_limit_reached","resets_at":4_102_444_812_u64}});
        let provider = plakous_responses_dialect::parse_provider_error_with_policy(
            &fixture["error"],
            plakous_responses_dialect::ResponsesResponseLimits::default(),
            plakous_responses_dialect::ResponsesProviderErrorPolicy::OpenAiCompatible,
        )?
        .with_request_id(Some("req_usage_limit_fixture"))?;
        for (provider_acceptance, hosted_effects_possible) in [
            (plakous_attempt::ProviderAcceptance::NotAccepted, false),
            (plakous_attempt::ProviderAcceptance::PossiblyAccepted, false),
            (plakous_attempt::ProviderAcceptance::PossiblyAccepted, true),
        ] {
            let facts = plakous_attempt::TerminalFacts::from_snapshot(
                plakous_attempt::ObservationSnapshot {
                    physical_attempts: 1,
                    provider_acceptance,
                    visible_output: false,
                    replay_safety: plakous_attempt::ReplaySafety::CostOnly,
                    hosted_effects_possible,
                    logical_success: false,
                },
            );
            let error = ResponsesAttemptError::HttpStatus {
                physical: None,
                status: plakous_http::StatusCode::TOO_MANY_REQUESTS,
                class: plakous_responses_dialect::ResponsesHttpStatusClass::Quota,
                provider: Some(provider.clone()),
                retry_after: None,
                retry_disabled: true,
                facts,
            };
            let mapped = model_error(&error, &descriptor).ok_or("Codex mapping")?;
            let default = error
                .contextual_http_error(&descriptor)
                .ok_or("default HTTP mapping")?;
            assert_eq!(mapped.kind(), ModelErrorKind::QuotaExceeded);
            assert_eq!(mapped.kind(), default.kind());
            assert_eq!(mapped.context(), default.context());
            if provider_acceptance == plakous_attempt::ProviderAcceptance::NotAccepted {
                assert_eq!(
                    mapped.context().map(ModelErrorContext::retryability),
                    Some(ModelRetryability::Permanent)
                );
            }
            assert!(mapped.message().contains("2100"));
            assert!(
                mapped
                    .message()
                    .contains("https://chatgpt.com/settings/usage")
            );
        }
        Ok(())
    }
}
