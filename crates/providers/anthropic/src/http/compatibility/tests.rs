use super::*;

fn envelope(message: &str, kind: &str) -> Vec<u8> {
    serde_json::json!({"type":"error","error":{"type":kind,"message":message}})
        .to_string()
        .into_bytes()
}

#[test]
fn unknown_sent_beta_matches_documented_shape_and_docs_url_variants() {
    for docs in ["docs.anthropic.com", "docs.claude.com"] {
        let message = format!(
            "Unexpected value(s) `effort-2025-11-24` for the `anthropic-beta` header. Please consult our documentation at {docs} or try again without the header."
        );
        assert_eq!(
            unknown_beta(
                400,
                &envelope(&message, "invalid_request_error"),
                "oauth-2025-04-20,effort-2025-11-24"
            ),
            Some(CompatibilityDrift::UnknownBeta)
        );
        assert!(
            CompatibilityDrift::UnknownBeta
                .message("Claude")
                .contains("install a newer build")
        );
    }
}

#[test]
fn unsent_beta_other_400_and_wrong_status_or_type_are_not_drift() {
    let message = "Unexpected value(s) `unrequested-beta` for the `anthropic-beta` header. Please consult our documentation.";
    assert_eq!(
        unknown_beta(
            400,
            &envelope(message, "invalid_request_error"),
            "effort-2025-11-24"
        ),
        None
    );
    let message = "Unexpected value(s) `effort-2025-11-24` for the `anthropic-beta` header. Please consult our documentation.";
    for status in [401, 403, 429, 500] {
        assert_eq!(
            unknown_beta(
                status,
                &envelope(message, "invalid_request_error"),
                "effort-2025-11-24"
            ),
            None
        );
    }
    assert_eq!(
        unknown_beta(
            400,
            &envelope(message, "authentication_error"),
            "effort-2025-11-24"
        ),
        None
    );
    assert_eq!(
        unknown_beta(
            400,
            &envelope("invalid max_tokens", "invalid_request_error"),
            "effort-2025-11-24"
        ),
        None
    );
    assert_eq!(unknown_beta(400, b"not JSON", "effort-2025-11-24"), None);
}
