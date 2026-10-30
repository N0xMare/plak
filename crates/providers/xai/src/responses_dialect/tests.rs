use std::num::NonZeroU32;

use super::*;
use plakous_responses_dialect::{
    ResponsesProviderErrorPolicy, ResponsesResponseLimits, parse_provider_error_with_policy,
};
use serde_json::json;

#[cfg(feature = "native-auth")]
#[test]
fn supergrok_rejects_product_composition_tool_presentations() {
    for tool_mode in [
        ToolPresentationMode::LocalComposition,
        ToolPresentationMode::LocalCompositionOnly,
    ] {
        assert!(matches!(
            require_direct_tool_presentation(tool_mode),
            Err(ResponsesProviderConfigurationError::PolicyMismatch(
                "tool_mode"
            ))
        ));
    }
    assert!(require_direct_tool_presentation(ToolPresentationMode::Direct).is_ok());
}

#[test]
fn request_route_and_revision_facts_match_the_supergrok_golden() {
    let facts = SUPERGROK_RESPONSES_DIALECT;
    assert_eq!(
        json!({
            "route": facts.route().route_id(),
            "protocol": facts.protocol_id(),
            "origin": facts.origin(),
            "path": facts.responses_path(),
            "adapter_revision": facts.adapter_revision(),
            "feature_revision": facts.feature_revision(),
            "research_baseline": facts.research_baseline(),
            "store": false,
            "tool_choice_without_tools": "omit",
            "function_strict": "omit",
            "parallel_tool_calls": "omit",
            "reasoning_include": "reasoning.encrypted_content",
        }),
        json!({
            "route": "xai.grok_subscription",
            "protocol": "xai.responses.grok-subscription.2026-07",
            "origin": "https://cli-chat-proxy.grok.com",
            "path": "/v1/responses",
            "adapter_revision": "plakous-xai-responses-adapter-v7",
            "feature_revision": "plak-xai-subscription-feature-matrix-v8",
            "research_baseline": "2026-08-22",
            "store": false,
            "tool_choice_without_tools": "omit",
            "function_strict": "omit",
            "parallel_tool_calls": "omit",
            "reasoning_include": "reasoning.encrypted_content",
        })
    );
    assert_eq!(
        facts.request_policy(),
        ResponsesRequestPolicy::supergrok_stateless()
    );
    assert!(!facts.emits_parallel_tool_calls());
    assert!(facts.requires_encrypted_reasoning_replay());
}

#[test]
fn only_real_supergrok_reasoning_and_replay_deltas_are_admitted() {
    let facts = SUPERGROK_RESPONSES_DIALECT;
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
    ] {
        assert!(facts.supports_reasoning_effort(Some(&effort)));
    }
    assert!(facts.supports_reasoning_effort(None));
    for effort in [
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
        ReasoningEffort::TokenBudget(NonZeroU32::MIN),
    ] {
        assert!(!facts.supports_reasoning_effort(Some(&effort)));
    }
    assert_eq!(facts.output_item_replay().name(), "xai.output_item");
    assert_eq!(facts.output_item_replay().version(), 2);
    assert_eq!(
        facts.encrypted_reasoning_replay(),
        facts.output_item_replay()
    );
    assert_eq!(facts.opaque_output_replay().name(), "xai.opaque_output");
    assert_eq!(facts.opaque_output_replay().version(), 1);
}

#[test]
fn event_and_error_deltas_do_not_import_other_responses_surfaces() {
    let facts = SUPERGROK_RESPONSES_DIALECT;
    assert_eq!(
        facts.event_delta("response.reasoning_summary_text.delta"),
        SuperGrokEventDelta::Typed
    );
    assert_eq!(
        facts.event_delta("response.completed"),
        SuperGrokEventDelta::Terminal
    );
    assert_eq!(
        facts.event_delta("error"),
        SuperGrokEventDelta::ProviderError
    );
    for foreign in [
        "response.in_progress",
        "response.output_text.done",
        "response.refusal.delta",
        "response.reasoning_summary_part.added",
        "response.custom_tool_call_input.delta",
        "response.output_text.annotation.added",
    ] {
        assert_eq!(
            facts.event_delta(foreign),
            SuperGrokEventDelta::PreserveOpaque
        );
    }
    assert_eq!(
        facts.provider_error_class(None, Some("invalid_api_key")),
        ResponsesProviderErrorClass::Authentication
    );
    assert_eq!(
        facts.provider_error_class(None, Some("unauthorized")),
        ResponsesProviderErrorClass::Unknown
    );
    assert_eq!(
        facts.provider_error_class(None, Some("internal_error")),
        ResponsesProviderErrorClass::Unknown
    );
    assert_eq!(facts.stream_profile(), ResponsesDialectProfile::xai());
    assert_eq!(facts.lifecycle_policy(), LifecycleDialectPolicy::grok());
    assert_eq!(facts.usage_policy(), ResponsesUsagePolicy::RequireTotal);
}

#[test]
fn provider_error_payloads_use_only_the_exact_grok_allowlist()
-> Result<(), Box<dyn std::error::Error>> {
    let limits = ResponsesResponseLimits::default();
    for (payload, expected) in [
        (
            json!({"code":"invalid_api_key","message":"denied"}),
            ResponsesProviderErrorClass::Authentication,
        ),
        (
            json!({"type":"authentication_error","message":"denied"}),
            ResponsesProviderErrorClass::Authentication,
        ),
        (
            json!({"code":"quota_exceeded","message":"quota"}),
            ResponsesProviderErrorClass::Quota,
        ),
        (
            json!({"code":"rate_limit_exceeded","message":"slow"}),
            ResponsesProviderErrorClass::RateLimited,
        ),
        (
            json!({"type":"rate_limit_error","message":"slow"}),
            ResponsesProviderErrorClass::RateLimited,
        ),
        (
            json!({"type":"server_error","message":"later"}),
            ResponsesProviderErrorClass::Server,
        ),
        (
            json!({"code":"unauthorized","message":"foreign near match"}),
            ResponsesProviderErrorClass::Unknown,
        ),
        (
            json!({"code":"internal_error","message":"foreign near match"}),
            ResponsesProviderErrorClass::Unknown,
        ),
    ] {
        let parsed =
            parse_provider_error_with_policy(&payload, limits, ResponsesProviderErrorPolicy::Grok)?;
        assert_eq!(parsed.class(), expected);
        assert_eq!(parsed.native(), &payload);
    }
    Ok(())
}

#[test]
fn shared_stream_and_replay_tables_are_complete_and_distinct()
-> Result<(), Box<dyn std::error::Error>> {
    let facts = SUPERGROK_RESPONSES_DIALECT;
    let model = ModelId::new("grok-test")?;
    let stream = facts.stream_data(&model);
    assert_eq!(stream.usage_policy(), ResponsesUsagePolicy::RequireTotal);

    let protocol = ProtocolId::new(facts.protocol_id())?;
    let projection = facts.replay_projection(
        protocol,
        None,
        model,
        plakous_model::Digest::sha256(plakous_model::DigestDomain::ModelBinding, 1, &[b"account"])?,
    )?;
    assert_eq!(
        projection.output_kind().as_str(),
        SUPERGROK_OUTPUT_ITEM_REPLAY_KIND
    );
    assert_eq!(
        projection.output_kind_version(),
        SUPERGROK_OUTPUT_ITEM_REPLAY_VERSION
    );
    assert_eq!(
        projection.opaque_output_kind().map(ReplayKindId::as_str),
        Some(SUPERGROK_OPAQUE_OUTPUT_REPLAY_KIND)
    );
    assert_eq!(
        projection.opaque_output_kind_version(),
        Some(SUPERGROK_OPAQUE_OUTPUT_REPLAY_VERSION)
    );
    Ok(())
}

#[test]
fn subscription_authority_is_exact() {
    assert_eq!(
        SUPERGROK_SUBSCRIPTION_BASE_URL,
        format!("{SUPERGROK_RESPONSES_ORIGIN}/v1")
    );
    #[cfg(feature = "native-auth")]
    assert_eq!(SUPERGROK_MODELS_PATH, "/v1/models");
    assert_eq!(SUPERGROK_RESPONSES_PATH, "/v1/responses");
}
