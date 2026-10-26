use std::collections::BTreeMap;

use plakous_attempt::{ObservationSnapshot, ProviderAcceptance};
use plakous_http::StatusCode;
use plakous_model::{
    CapabilitySet, ModelAdapterProfileId, ModelId, ProtocolId, RouteId,
    StandardIntegrationAuthority,
};

use super::*;

fn descriptor() -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error>> {
    Ok(ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("openai.responses")?,
        RouteId::new("test.responses")?,
        ModelId::new("gpt-test")?,
        None,
        None,
        ModelAdapterProfileId::new("responses-test")?,
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    ))
}

/// The Responses row of the provider-error matrix.
///
/// One row covers five routes. OpenAI Responses, Codex, xAI Platform,
/// SuperGrok and `OpenRouter` Responses all reach
/// [`model_error_from_attempt`] and none installs a
/// [`ResponsesModelErrorHook`], so they share this classification exactly;
/// they differ in credential, origin and route identity, and nothing in the
/// classification reads any of those.
///
/// The property is the same as every other row: a provider failure is a
/// definite outcome unless a hosted effect may have run. This row asserts it
/// over the kinds the adapter assigns per column, and over the one lever
/// that can escalate any of them.
#[test]
fn the_responses_row_of_the_provider_failure_matrix_is_definite()
-> Result<(), Box<dyn std::error::Error>> {
    let descriptor = descriptor()?;
    // Facts for a column: accepted, output already streamed, no hosted
    // effect. That is the hardest shape for definiteness and the one the
    // live OpenRouter 520 arrived in.
    let accepted = || {
        TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: ProviderAcceptance::Accepted,
            visible_output: true,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        })
    };
    let rejected = || {
        TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: ProviderAcceptance::NotAccepted,
            visible_output: false,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        })
    };
    let status = |code: u16, class, facts| ResponsesAttemptError::HttpStatus {
        retry_after: None,
        retry_disabled: false,
        physical: None,
        status: StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
        class,
        provider: None,
        facts,
    };
    let authentication = model_error_from_attempt(
        status(401, ResponsesHttpStatusClass::Authentication, rejected()),
        &descriptor,
        None,
        &StreamProgress::new(Instant::now()),
    );
    assert_eq!(authentication.kind(), ModelErrorKind::Authentication);
    let context = authentication
        .context()
        .ok_or("authentication context missing")?;
    assert_eq!(context.phase(), ModelErrorPhase::FirstEvent);
    assert_eq!(context.http_status().map(ModelHttpStatus::get), Some(401));
    assert_eq!(context.retryability(), ModelRetryability::Permanent);
    // Each column is a real attempt error, classified by the real function.
    // Asserting a literal here would have proved nothing: this row used to
    // do exactly that, and the transport and deadline cells never reached
    // a classifier at all.
    let columns: Vec<(&str, ResponsesAttemptError, ModelErrorKind)> = vec![
        (
            "pre-stream 5xx",
            status(503, ResponsesHttpStatusClass::Server, rejected()),
            ModelErrorKind::Unavailable,
        ),
        (
            "pre-stream 429",
            status(429, ResponsesHttpStatusClass::RateLimited, rejected()),
            ModelErrorKind::RateLimited,
        ),
        (
            "in-band error after partial stream",
            status(520, ResponsesHttpStatusClass::Unknown, accepted()),
            ModelErrorKind::Protocol,
        ),
        (
            "transport timeout",
            ResponsesAttemptError::Transport {
                physical: None,
                source: plakous_http::Failure::for_classifier_test(
                    plakous_http::Phase::ResponseBody,
                    plakous_http::TransmissionFact::MayHaveTransmitted,
                    plakous_http::ErrorKind::DeadlineExceeded,
                ),
                facts: accepted(),
            },
            ModelErrorKind::Transport,
        ),
        (
            "operation deadline",
            ResponsesAttemptError::Deadline {
                physical: None,
                facts: accepted(),
            },
            ModelErrorKind::Timeout,
        ),
    ];
    for (name, attempt, expected) in columns {
        let error = model_error_from_attempt(
            attempt,
            &descriptor,
            None,
            &StreamProgress::new(Instant::now()),
        );
        assert_eq!(error.kind(), expected, "{name}");
        assert_ne!(
            error.kind(),
            ModelErrorKind::OutcomeUnknown,
            "{name}: a named provider failure with no hosted effect fences the session"
        );
    }

    Ok(())
}

#[test]
fn hosted_effect_escalates_retry_risk() {
    // The one lever that escalates any column, and the only fact it reads.
    let snapshot = |hosted: bool| ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::Accepted,
        visible_output: true,
        replay_safety: if hosted {
            plakous_attempt::ReplaySafety::ServerEffect
        } else {
            plakous_attempt::ReplaySafety::CostOnly
        },
        hosted_effects_possible: hosted,
        logical_success: false,
    };
    assert_eq!(
        retryability_from_facts(
            TerminalFacts::from_snapshot(snapshot(false)),
            RetryClass::Never
        ),
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling
        },
        "an accepted stream is definite without a hosted effect, and never replayed"
    );
    assert!(
        matches!(
            retryability_from_facts(
                TerminalFacts::from_snapshot(snapshot(true)),
                RetryClass::Never
            ),
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::ProviderHostedEffect
            }
        ),
        "a hosted effect in flight is the one thing that escalates"
    );
}

#[test]
fn provider_terminal_cyber_policy_invalid_request_is_a_refusal()
-> Result<(), Box<dyn std::error::Error>> {
    let provider = crate::parse_provider_error_with_policy(
        &serde_json::json!({"code": "cyber_policy", "type": "invalid_request", "message": "synthetic policy refusal"}),
        crate::ResponsesResponseLimits::default(),
        crate::ResponsesProviderErrorPolicy::OpenAiCompatible,
    )?;
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::Accepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    let error = provider_terminal_error(&descriptor()?, Some(&provider), facts);
    assert_eq!(error.kind(), ModelErrorKind::Policy);
    assert_eq!(
        error.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling
        })
    );
    Ok(())
}

/// The body is the live xAI 403 a SuperGrok turn received; the same
/// status without that exact code stays an authorization failure.
#[test]
fn xai_pre_generation_refusal_is_policy_not_authorization() -> Result<(), Box<dyn std::error::Error>>
{
    let rejected = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::NotAccepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    let classify =
        |body: serde_json::Value, policy| -> Result<ModelError, Box<dyn std::error::Error>> {
            let provider = crate::parse_provider_error_with_policy(
                crate::response_host::provider_error_object(&body),
                crate::ResponsesResponseLimits::default(),
                policy,
            )?;
            Ok(model_error_from_attempt(
                ResponsesAttemptError::HttpStatus {
                    retry_after: None,
                    retry_disabled: false,
                    physical: None,
                    status: StatusCode::FORBIDDEN,
                    class: ResponsesHttpStatusClass::Authorization,
                    provider: Some(provider),
                    facts: rejected,
                },
                &descriptor()?,
                None,
                &StreamProgress::new(Instant::now()),
            ))
        };
    let refusal = serde_json::json!({
        "code": "permission-denied",
        "error": "I'm sorry, I can't help with that request.",
    });
    let error = classify(refusal.clone(), crate::ResponsesProviderErrorPolicy::Grok)?;
    assert_eq!(error.kind(), ModelErrorKind::Policy);
    assert!(
        error
            .message()
            .contains("I'm sorry, I can't help with that request.")
    );
    let context = error.context().ok_or("refusal context missing")?;
    assert_eq!(context.http_status().map(ModelHttpStatus::get), Some(403));
    assert_eq!(context.provider_code(), Some("permission-denied"));
    for (body, policy) in [
        (
            serde_json::json!({"code": "unauthenticated:bad-credentials", "error": "denied"}),
            crate::ResponsesProviderErrorPolicy::Grok,
        ),
        (
            serde_json::json!({"code": "permission-denied:team-blocked", "error": "blocked"}),
            crate::ResponsesProviderErrorPolicy::Grok,
        ),
        (
            refusal,
            crate::ResponsesProviderErrorPolicy::OpenAiCompatible,
        ),
    ] {
        assert_eq!(
            classify(body, policy)?.kind(),
            ModelErrorKind::Authorization,
            "{policy:?}"
        );
    }
    Ok(())
}

#[test]
fn provider_terminal_content_policy_never_retries() -> Result<(), Box<dyn std::error::Error>> {
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::Accepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    for policy in [
        crate::ResponsesProviderErrorPolicy::OpenAiCompatible,
        crate::ResponsesProviderErrorPolicy::OpenRouter,
        crate::ResponsesProviderErrorPolicy::Grok,
    ] {
        for label in [
            "content_policy_violation",
            "content_filter",
            "cyber_safety_violation",
            "cyber_policy",
        ] {
            for field in ["code", "type"] {
                let provider = crate::parse_provider_error_with_policy(
                    &serde_json::json!({field: label, "message": "This content was flagged for possible cybersecurity risk"}),
                    crate::ResponsesResponseLimits::default(),
                    policy,
                )?;
                let error = provider_terminal_error(&descriptor()?, Some(&provider), facts);
                assert_eq!(
                    error.kind(),
                    ModelErrorKind::Policy,
                    "{policy:?}/{field}/{label}"
                );
                assert_eq!(
                    error.context().map(ModelErrorContext::retryability),
                    Some(ModelRetryability::Unsafe {
                        risk: ModelRetryRisk::DuplicateInferenceOrBilling,
                    })
                );
                assert!(
                    error
                        .message()
                        .contains("This content was flagged for possible cybersecurity risk")
                );
            }
        }
    }
    for value in [
        serde_json::json!({"code": "future_policy", "message": "This content was flagged for possible cybersecurity risk"}),
        serde_json::json!({"code":"invalid_request","type":"invalid_request"}),
        serde_json::json!({"type":"invalid_request_error"}),
        serde_json::json!({"code":"cyber_policy_v2"}),
        serde_json::json!({"code":"cyber"}),
        serde_json::json!({"type": "not_content_policy_violation"}),
        serde_json::json!({"message": "content_policy_violation"}),
    ] {
        let provider = crate::parse_provider_error_with_policy(
            &value,
            crate::ResponsesResponseLimits::default(),
            crate::ResponsesProviderErrorPolicy::OpenAiCompatible,
        )?;
        assert_eq!(
            provider_terminal_error(&descriptor()?, Some(&provider), facts).kind(),
            ModelErrorKind::Protocol
        );
    }
    Ok(())
}

#[test]
fn provider_terminal_error_preserves_class_and_code() -> Result<(), Box<dyn std::error::Error>> {
    let provider = crate::parse_provider_error_with_policy(
        &serde_json::json!({"code":"rate_limit_exceeded","message":"redacted"}),
        crate::ResponsesResponseLimits::default(),
        crate::ResponsesProviderErrorPolicy::OpenAiCompatible,
    )?;
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::Accepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    let error = provider_terminal_error(&descriptor()?, Some(&provider), facts);
    assert_eq!(error.kind(), ModelErrorKind::RateLimited);
    assert_eq!(
        error.context().and_then(ModelErrorContext::provider_code),
        Some("rate_limit_exceeded")
    );
    assert_eq!(
        error.context().map(ModelErrorContext::phase),
        Some(ModelErrorPhase::Terminal)
    );
    assert!(
        error.message().contains("redacted"),
        "typed terminal must keep the bounded native message, got {:?}",
        error.message()
    );
    Ok(())
}

#[test]
fn http_400_appends_bounded_native_message() -> Result<(), Box<dyn std::error::Error>> {
    let provider = crate::parse_provider_error_with_policy(
        &serde_json::json!({
            "code": "invalid_request",
            "message": "tools.1.parameters.properties.-i is not a valid schema path\nretry"
        }),
        crate::ResponsesResponseLimits::default(),
        crate::ResponsesProviderErrorPolicy::Grok,
    )?;
    let error = contextual_http_error(
        &descriptor()?,
        ModelErrorKind::InvalidRequest,
        ModelRetryability::Permanent,
        400,
        Some(provider),
    );
    assert_eq!(error.kind(), ModelErrorKind::InvalidRequest);
    assert_eq!(
        error
            .context()
            .and_then(ModelErrorContext::http_status)
            .map(ModelHttpStatus::get),
        Some(400)
    );
    assert_eq!(
        error.context().and_then(ModelErrorContext::provider_code),
        Some("invalid_request")
    );
    assert!(
        error
            .message()
            .contains("Responses HTTP request failed: tools.1.parameters.properties.-i is not a valid schema path retry"),
        "HTTP 400 must keep the collapsed native message, got {:?}",
        error.message()
    );
    assert!(
        !error.message().contains('\n'),
        "operator HTTP detail must be one line, got {:?}",
        error.message()
    );
    Ok(())
}

#[test]
fn grok_string_error_envelope_keeps_native_message() -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "code": "invalid-argument",
        "error": "wait_agent: invalid constraint value at #/properties/agent_ids: minItems must be an integer, got 1.0"
    });
    let provider = crate::parse_provider_error_with_policy(
        crate::response_host::provider_error_object(&body),
        crate::ResponsesResponseLimits::default(),
        crate::ResponsesProviderErrorPolicy::Grok,
    )?;
    let error = contextual_http_error(
        &descriptor()?,
        ModelErrorKind::InvalidRequest,
        ModelRetryability::Permanent,
        400,
        Some(provider),
    );
    assert_eq!(
        error.context().and_then(ModelErrorContext::provider_code),
        Some("invalid-argument")
    );
    assert!(
        error
            .message()
            .contains("minItems must be an integer, got 1.0"),
        "xAI string error field must surface, got {:?}",
        error.message()
    );
    Ok(())
}

/// The turn-scope input budget denial used to read as a bare
/// `Responses fence-admission`; the operator could not tell it from a
/// context-envelope refusal.
#[test]
fn inference_input_budget_denial_names_the_budget() -> Result<(), Box<dyn std::error::Error>> {
    let error = dispatch_rejection_error(
        &descriptor()?,
        crate::ResponsesDispatchRejection::InferenceInput,
        ModelDispatchRejectionPhase::Preflight,
        ModelRetryability::Permanent,
    );
    assert_eq!(error.kind(), ModelErrorKind::ResourceLimit);
    assert!(error.message().starts_with("Responses fence-admission"));
    assert!(
        error.message().contains("input-liability budget"),
        "{}",
        error.message()
    );
    Ok(())
}

#[test]
fn every_dispatch_rejection_class_and_fence_is_typed_and_definite()
-> Result<(), Box<dyn std::error::Error>> {
    let cases = [
        (
            crate::ResponsesDispatchRejection::Binding,
            ModelDispatchRejectionClass::Binding,
            ModelErrorKind::InvalidConfiguration,
        ),
        (
            crate::ResponsesDispatchRejection::Authentication,
            ModelDispatchRejectionClass::Authentication,
            ModelErrorKind::Authentication,
        ),
        (
            crate::ResponsesDispatchRejection::Authorization,
            ModelDispatchRejectionClass::Authorization,
            ModelErrorKind::Authorization,
        ),
        (
            crate::ResponsesDispatchRejection::Admission,
            ModelDispatchRejectionClass::Admission,
            ModelErrorKind::ResourceLimit,
        ),
        (
            crate::ResponsesDispatchRejection::InferenceInput,
            ModelDispatchRejectionClass::Admission,
            ModelErrorKind::ResourceLimit,
        ),
        (
            crate::ResponsesDispatchRejection::Unavailable,
            ModelDispatchRejectionClass::Unavailable,
            ModelErrorKind::Unavailable,
        ),
        (
            crate::ResponsesDispatchRejection::InferenceClass {
                reserved: 8,
                allowance: 8,
            },
            ModelDispatchRejectionClass::Unavailable,
            ModelErrorKind::Unavailable,
        ),
    ];
    for phase in [
        ModelDispatchRejectionPhase::Preflight,
        ModelDispatchRejectionPhase::Revalidation,
    ] {
        for (rejection, class, kind) in cases.clone() {
            let class_limit = matches!(
                rejection,
                crate::ResponsesDispatchRejection::InferenceClass { .. }
            );
            let error = dispatch_rejection_error(
                &descriptor()?,
                rejection,
                phase,
                ModelRetryability::Permanent,
            );
            assert_eq!(error.kind(), kind);
            assert_ne!(error.kind(), ModelErrorKind::OutcomeUnknown);
            assert_eq!(
                error.dispatch_rejection(),
                Some(ModelDispatchRejection::new(class, phase))
            );
            assert_eq!(
                error.context().map(ModelErrorContext::phase),
                Some(ModelErrorPhase::Readiness)
            );
            assert!(
                [
                    "binding",
                    "authentication",
                    "authorization",
                    "admission",
                    "unavailable",
                ]
                .iter()
                .any(|label| error.message().contains(label)),
                "dispatch diagnostic must name the rejection class, got {:?}",
                error.message()
            );
            if class_limit {
                assert!(
                    error.message().contains("inference 8/8"),
                    "class-limit diagnostic must carry reserved/allowance, got {:?}",
                    error.message()
                );
            }
            let encoded = serde_json::to_vec(&error)?;
            let decoded: ModelError = serde_json::from_slice(&encoded)?;
            assert_eq!(decoded, error);
        }
    }
    Ok(())
}

#[test]
fn accepted_hosted_effect_capability_suppresses_automatic_retry() {
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::Accepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::ServerEffect,
        hosted_effects_possible: true,
        logical_success: false,
    });
    assert_eq!(
        retryability_from_facts(facts, RetryClass::OperationOwnedDuplicateRisk),
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        }
    );
    assert_eq!(
        retryability_from_facts(facts, RetryClass::Never),
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        }
    );
}

use crate::{
    ResponsesProviderRequestBinding, ResponsesReplayProjection, ResponsesResponse,
    ResponsesUsagePolicy, project_host::project_terminal_response,
};
use plakous_model::{Digest, DigestDomain, FinishReason, FixedClock, ReplayKindId, Timestamp};
use plakous_responses::ResponsesStorageMode;
use serde_json::json;

#[test]
fn only_native_grok_length_text_projects_a_single_terminal_without_tools()
-> Result<(), Box<dyn std::error::Error>> {
    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp-length","object":"response","status":"incomplete",
            "model":"grok-test","output":[
                {"id":"reason","type":"reasoning","status":"completed","summary":[]},
                {"id":"message","type":"message","role":"assistant","status":"incomplete",
                 "content":[{"type":"output_text","text":"summary","annotations":[]}]}
            ],"usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
            "incomplete_details":{"reason":"max_output_tokens"}
        }),
        ResponsesUsagePolicy::RequireTotal,
        crate::ResponsesResponseLimits::default(),
    )?;
    let replay = ResponsesReplayProjection::new(
        ProtocolId::new("xai.responses")?,
        None,
        ModelId::new("grok-test")?,
        ReplayKindId::new("responses.output_item")?,
        1,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"model-host-account"])?,
    )?;
    let provider_request = ResponsesProviderRequestBinding::new(
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"request"])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"response"])?,
        None,
    )?;
    let projection = project_terminal_response(
        response,
        &replay,
        ResponsesStorageMode::Stateless,
        provider_request,
    )?;
    assert!(grok_length_text_terminal(&projection));
    let grok = ResponsesCanonicalDialectData::grok(ModelId::new("grok-test")?, [])?;
    let platform = ResponsesCanonicalDialectData::xai_platform(ModelId::new("grok-test")?)?;
    assert!(grok.policy().allows_grok_length_text());
    assert!(!platform.policy().allows_grok_length_text());
    let clock = Arc::new(FixedClock::new(Timestamp::new(42, 0)?));
    let ids = CanonicalIdSources::with_clock(clock);
    let canonical = project_canonical_terminal_with_hosted_programs(
        &projection,
        &grok,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"])?,
        &ids,
        ResponsesHostedProgramContext::default(),
    )?;
    assert_eq!(canonical.response().finish().reason(), FinishReason::Length);
    let events = canonical.into_events();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, plakous_model::ModelEvent::ToolInputDelta(_)))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
            .count(),
        1
    );
    Ok(())
}

fn incomplete_projection(
    output: serde_json::Value,
    reason: &str,
) -> Result<ResponsesTerminalProjection, Box<dyn std::error::Error>> {
    let response = ResponsesResponse::parse(
        &json!({
            "id":"resp-cut","object":"response","status":"incomplete",
            "model":"grok-test","output":output,
            "usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
            "incomplete_details":{"reason":reason}
        }),
        ResponsesUsagePolicy::RequireTotal,
        crate::ResponsesResponseLimits::default(),
    )?;
    let replay = ResponsesReplayProjection::new(
        ProtocolId::new("xai.responses")?,
        None,
        ModelId::new("grok-test")?,
        ReplayKindId::new("responses.output_item")?,
        1,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"model-host-account"])?,
    )?;
    let provider_request = ResponsesProviderRequestBinding::new(
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"request"])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"response"])?,
        None,
    )?;
    Ok(project_terminal_response(
        response,
        &replay,
        ResponsesStorageMode::Stateless,
        provider_request,
    )?)
}

/// A tool call cut off by the output bound is an output-limit failure the
/// runtime can retry, never a generic incomplete response, and never a
/// Grok length-text terminal that could dispatch it.
#[test]
fn a_tool_call_cut_off_by_the_output_bound_is_an_output_limit_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let cut = incomplete_projection(
        json!([
            {"id":"reason","type":"reasoning","status":"completed","summary":[]},
            {"id":"call","type":"function_call","status":"incomplete","call_id":"call-1",
             "name":"write","arguments":"{\"path\":\"big.txt\",\"content\":\"aaa"}
        ]),
        "max_output_tokens",
    )?;
    assert!(!grok_length_text_terminal(&cut));
    assert_eq!(
        incomplete_terminal_failure(&cut).0,
        ModelErrorKind::OutputLimit
    );
    let filtered = incomplete_projection(json!([]), "content_filter")?;
    assert_eq!(
        incomplete_terminal_failure(&filtered).0,
        ModelErrorKind::IncompleteResponse
    );
    Ok(())
}
