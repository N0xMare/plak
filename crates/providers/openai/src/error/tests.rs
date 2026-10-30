use plakous_model::ModelErrorKind;

use super::{
    ErrorPhase, MAX_PROVIDER_CODE_BYTES, MAX_PROVIDER_REQUEST_ID_BYTES, OpenAiError, RetryClass,
    valid_provider_metadata,
};

/// Every phase and retry class this module publishes.
///
/// The table is exhaustive on purpose: both enums are `non_exhaustive` to
/// outside crates but not to this one, so a new variant fails to compile
/// here until someone says which row it belongs on.
fn phases() -> [ErrorPhase; 6] {
    [
        ErrorPhase::Request,
        ErrorPhase::Authentication,
        ErrorPhase::Connect,
        ErrorPhase::FirstEvent,
        ErrorPhase::Stream,
        ErrorPhase::Terminal,
    ]
}

fn retry_classes() -> [RetryClass; 4] {
    [
        RetryClass::Never,
        RetryClass::BeforeAcceptance,
        RetryClass::ProviderTransient,
        RetryClass::OperationOwnedDuplicateRisk,
    ]
}

#[test]
fn every_phase_and_retry_class_survives_construction() {
    for phase in phases() {
        for retry in retry_classes() {
            let error = OpenAiError::new(
                phase,
                ModelErrorKind::Transport,
                retry,
                "the stream ended early",
            );
            assert_eq!(error.phase(), phase);
            assert_eq!(error.retry_class(), retry);
            assert_eq!(error.kind(), ModelErrorKind::Transport);
            assert_eq!(error.safe_message(), "the stream ended early");
            assert_eq!(error.diagnostic().as_str(), error.safe_message());
            // Nothing was attached, so nothing is reported.
            assert_eq!(error.safe_status(), None);
            assert_eq!(error.provider_code(), None);
            assert_eq!(error.request_id(), None);
            // The rendered error names the phase, which is the one piece a
            // reader needs to tell a refused request from a lost stream.
            assert!(
                error.to_string().contains(&format!("{phase:?}")),
                "the display names its phase: {error}"
            );
        }
    }
}

#[test]
fn a_status_outside_the_http_range_is_not_a_status() {
    // `Some(0)` and `Some(600)` are not HTTP; carrying them would let a
    // classifier downstream believe the provider answered when it did not.
    let rows: [(Option<u16>, Option<u16>); 7] = [
        (None, None),
        (Some(0), None),
        (Some(99), None),
        (Some(100), Some(100)),
        (Some(429), Some(429)),
        (Some(599), Some(599)),
        (Some(600), None),
    ];
    for (given, expected) in rows {
        let error = OpenAiError::new(
            ErrorPhase::Terminal,
            ModelErrorKind::RateLimited,
            RetryClass::Never,
            "provider refused",
        )
        .with_provider_metadata(given, None, None);
        assert_eq!(error.safe_status(), expected, "status {given:?}");
    }
}

#[test]
fn provider_metadata_keeps_only_bounded_identifier_text() {
    let long_code = "a".repeat(MAX_PROVIDER_CODE_BYTES + 1);
    let long_request = "b".repeat(MAX_PROVIDER_REQUEST_ID_BYTES + 1);
    let rows: [(&str, bool); 10] = [
        ("rate_limit_exceeded", true),
        ("req_01H.aZ:9/x-Y", true),
        ("a", true),
        ("", false),
        ("has space", false),
        ("new\nline", false),
        ("quote\"", false),
        ("semi;colon", false),
        (long_code.as_str(), false),
        (long_request.as_str(), false),
    ];
    for (value, kept) in rows {
        let error = OpenAiError::new(
            ErrorPhase::Request,
            ModelErrorKind::RateLimited,
            RetryClass::Never,
            "provider refused",
        )
        .with_provider_metadata(None, Some(value.to_owned()), Some(value.to_owned()));
        let expected_code = kept.then_some(value);
        assert_eq!(error.provider_code(), expected_code, "code {value:?}");
        // The request ID ceiling is the larger of the two, so a value the
        // code ceiling rejects for length may still be a valid request ID.
        let request_kept = valid_provider_metadata(value, MAX_PROVIDER_REQUEST_ID_BYTES);
        assert_eq!(
            error.request_id(),
            request_kept.then_some(value),
            "request id {value:?}"
        );
    }
}

#[test]
fn a_credential_in_the_caller_text_never_reaches_the_public_message() {
    let rows: [&str; 4] = [
        "auth failed for sk-live-abcdef0123456789",
        "sent header Authorization: Bearer abcdef0123456789",
        concat!("config had api_key=", "abcdef0123456789"),
        "token ghp_abcdef0123456789abcdef was rejected",
    ];
    for text in rows {
        let error = OpenAiError::new(
            ErrorPhase::Authentication,
            ModelErrorKind::Authentication,
            RetryClass::Never,
            text,
        );
        let message = error.safe_message();
        assert!(
            message.contains("[redacted]"),
            "no redaction in {message:?}"
        );
        for secret in ["abcdef0123456789", "sk-live", "ghp_"] {
            assert!(
                !message.contains(secret),
                "{secret} survived into {message:?}"
            );
        }
        // Display goes through the same sanitized field.
        assert!(!error.to_string().contains("abcdef0123456789"));
    }
}

#[test]
fn debug_renders_neither_the_provider_code_nor_the_request_id() {
    let error = OpenAiError::new(
        ErrorPhase::Stream,
        ModelErrorKind::Unavailable,
        RetryClass::ProviderTransient,
        "overloaded",
    )
    .with_provider_metadata(
        Some(529),
        Some("code_that_should_not_print".to_owned()),
        Some("req_that_should_not_print".to_owned()),
    );
    let rendered = format!("{error:?}");
    assert!(rendered.contains("529"), "the status is safe to print");
    assert!(!rendered.contains("code_that_should_not_print"));
    assert!(!rendered.contains("req_that_should_not_print"));
    assert!(!rendered.contains("overloaded"), "{rendered}");
    // The accessors still return them; only the Debug rendering withholds.
    assert_eq!(error.provider_code(), Some("code_that_should_not_print"));
    assert_eq!(error.request_id(), Some("req_that_should_not_print"));
}
