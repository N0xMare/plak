use super::*;

fn classify_accepted(
    status: Option<u16>,
    body: &[u8],
    header_request_id: Option<&str>,
    phase: AnthropicErrorPhase,
) -> AnthropicError {
    classify_provider_error_with_safety(
        status,
        body,
        header_request_id,
        phase,
        RetrySafety::new(true, false, false),
    )
}

const SECRET_SENTINEL: &str = "ANTHROPIC_SECRET_SENTINEL_DO_NOT_RETAIN";

#[test]
fn a_prompt_too_long_rejection_is_a_limit_and_other_invalid_requests_are_not() {
    let overflow = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 270013 tokens > 200000 maximum"}}"#;
    let error = classify_accepted(Some(400), overflow, None, AnthropicErrorPhase::Connect);
    assert_eq!(error.kind(), AnthropicErrorKind::Limit);
    assert!(
        !error.message().contains("270013"),
        "provider prose is never retained"
    );
    let other = br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be at most 8192"}}"#;
    let error = classify_accepted(Some(400), other, None, AnthropicErrorPhase::Connect);
    assert_eq!(error.kind(), AnthropicErrorKind::InvalidRequest);
}

#[test]
fn public_diagnostics_are_bounded_utf8_safe_and_credential_redacted() {
    let raw = format!(
        "authorization: Bearer {SECRET_SENTINEL} token={SECRET_SENTINEL} sk-{SECRET_SENTINEL} {}",
        "🦀".repeat(plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES)
    );
    let error = AnthropicError::new(
        AnthropicErrorKind::InvalidRequest,
        AnthropicErrorPhase::Prepare,
        raw,
    );
    assert!(!error.message().contains(SECRET_SENTINEL));
    assert!(error.message().len() <= plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES);
    assert!(error.message().is_char_boundary(error.message().len()));
    assert_eq!(error.message(), error.diagnostic().as_str());
    assert_eq!(error.to_string(), error.message());
    assert!(!error.to_string().contains(SECRET_SENTINEL));
    let debug = format!("{error:?}");
    assert!(!debug.contains(SECRET_SENTINEL));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn provider_metadata_stays_separately_validated_and_debug_redacted() {
    let error = AnthropicError::new(
        AnthropicErrorKind::RateLimited,
        AnthropicErrorPhase::Connect,
        format!("Bearer {SECRET_SENTINEL}"),
    )
    .with_provider_metadata(
        Some(429),
        Some("rate_limit_error".to_owned()),
        Some("req_safe_01".to_owned()),
        RetryAdvice::Transient,
    );
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.code(), Some("rate_limit_error"));
    assert_eq!(error.request_id(), Some("req_safe_01"));
    assert!(!error.message().contains(SECRET_SENTINEL));
    let debug = format!("{error:?}");
    assert!(!debug.contains("rate_limit_error"));
    assert!(!debug.contains("req_safe_01"));

    let invalid = AnthropicError::new(
        AnthropicErrorKind::Provider,
        AnthropicErrorPhase::Stream,
        "invalid metadata fixture",
    )
    .with_provider_metadata(
        Some(500),
        Some("bad code".to_owned()),
        Some("bad\nrequest".to_owned()),
        RetryAdvice::Never,
    );
    assert_eq!(invalid.code(), None);
    assert_eq!(invalid.request_id(), None);

    let oversized = AnthropicError::new(
        AnthropicErrorKind::Provider,
        AnthropicErrorPhase::Stream,
        "oversized metadata fixture",
    )
    .with_provider_metadata(
        None,
        Some("c".repeat(MAX_PROVIDER_ERROR_CODE_BYTES + 1)),
        Some("r".repeat(MAX_PROVIDER_REQUEST_ID_BYTES + 1)),
        RetryAdvice::Never,
    );
    assert_eq!(oversized.code(), None);
    assert_eq!(oversized.request_id(), None);
}

#[test]
fn provider_prose_is_classification_only_and_never_public() {
    let body = format!(
        "{{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"sk-{SECRET_SENTINEL}\"}},\"request_id\":\"req_body_01\"}}"
    );
    let error = classify_accepted(
        Some(529),
        body.as_bytes(),
        Some("req_header_01"),
        AnthropicErrorPhase::Stream,
    );
    assert_eq!(error.kind(), AnthropicErrorKind::Overloaded);
    assert_eq!(error.code(), Some("overloaded_error"));
    assert_eq!(error.request_id(), Some("req_header_01"));
    assert_eq!(error.message(), "Anthropic provider error");
    assert!(!error.to_string().contains(SECRET_SENTINEL));
    assert!(!format!("{error:?}").contains(SECRET_SENTINEL));
}

#[test]
fn hosted_effect_risk_disables_transient_retry() {
    let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
    let accepted = classify_provider_error_with_safety(
        None,
        body,
        None,
        AnthropicErrorPhase::Stream,
        RetrySafety::new(true, false, false),
    );
    assert_eq!(accepted.retry_advice(), RetryAdvice::Never);

    let before_acceptance = classify_provider_error_with_safety(
        None,
        body,
        None,
        AnthropicErrorPhase::Connect,
        RetrySafety::new(false, false, false),
    );
    assert_eq!(before_acceptance.retry_advice(), RetryAdvice::Transient);

    let unsafe_effect = classify_provider_error_with_safety(
        None,
        body,
        None,
        AnthropicErrorPhase::Stream,
        RetrySafety::new(true, false, true),
    );
    assert_eq!(unsafe_effect.retry_advice(), RetryAdvice::Never);
}
