use super::*;

const CAPTURED_258_BILLING_HEADER: &str =
    "x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000;";

#[test]
fn empty_prompt_suffix_matches_the_current_native_version() {
    assert_eq!(cc_version_suffix("", "2.1.257"), "9c3");
    assert_eq!(cc_version_suffix("hello", "2.1.257"), "468");
    assert_eq!(cc_version_suffix("", "2.1.258"), "1e2");
    assert_eq!(cc_version_suffix("hello", "2.1.258"), "59d");
    assert_eq!(cc_version_suffix("", "2.1.261"), "6af");
    assert_eq!(cc_version_suffix("", "2.1.270"), "ffc");
    assert_eq!(cc_version_suffix("hello", "2.1.270"), "4c5");
    assert_eq!(cc_version_suffix("", "2.1.284"), "12a");
    assert_eq!(cc_version_suffix("hello", "2.1.284"), "b93");
    assert_eq!(cc_version_suffix("hello", "2.1.289"), "7e2");
    assert_eq!(cc_version_suffix("", "2.1.293"), "9b8");
    assert_eq!(cc_version_suffix("hello", "2.1.293"), "72d");
    assert_eq!(cc_version_suffix("", "2.1.295"), "8a8");
    assert_eq!(cc_version_suffix("hello", "2.1.295"), "05c");
    assert_eq!(
        billing_header_prefix(SessionLineage::Root, ""),
        CLAUDE_OAUTH_BILLING_HEADER
    );
    assert_eq!(
        billing_header_prefix(SessionLineage::SpawnedChild, ""),
        CLAUDE_OAUTH_BILLING_HEADER_SUBAGENT
    );
    assert!(billing_header_prefix(SessionLineage::Root, "hello").contains("2.1.295.05c;"));
    assert!(
        billing_header_prefix(SessionLineage::SpawnedChild, "hello")
            .ends_with("cch=00000; cc_is_subagent=true;")
    );
}

#[test]
fn native_version_suffix_indexes_utf16_before_utf8_encoding() {
    // Generated using the pinned native Nct expression in Node.js.
    for (text, expected) in [
        ("😀hello world abcdefghijkl", "f97"),
        ("abc😀defghijklmnopqrstuvwxyz", "493"),
        ("abcd😀x😀abcdefghijklmno", "13b"),
        ("abcd😀😀abcdefghijklmno", "fde"),
    ] {
        assert_eq!(cc_version_suffix(text, "2.1.258"), expected);
    }
}

#[test]
fn native_request_id_grammar_is_bounded_ascii() {
    for valid in [
        "req_a",
        "req_abc-XYZ_123",
        &format!("req_{}", "a".repeat(36)),
    ] {
        assert!(valid_native_request_id(valid));
    }
    for invalid in [
        "req_",
        "msg_abc",
        "req_é",
        "req_a;",
        "req_a\n",
        &format!("req_{}", "a".repeat(37)),
    ] {
        assert!(!valid_native_request_id(invalid));
    }
}

#[test]
fn cch_matches_published_xxhash64_vectors() {
    let cases: &[(&[u8], &str)] = &[
        (b"cch=00000", "a47f7"),
        (br#"{"messages":[],"cch=00000","x":1}"#, "3073d"),
        (
            b"x-anthropic-billing-header: cc_version=2.1.158; cc_entrypoint=cli; cch=00000;",
            "f2b0b",
        ),
    ];
    for (body, expected) in cases {
        let got = format!("{:05x}", xxh64(body, CCH_SEED) & 0xF_FFFF);
        assert_eq!(got, *expected, "{}", std::str::from_utf8(body).unwrap());
    }
}

#[test]
fn native_executed_cch_excludes_model_values_and_max_tokens_fields() {
    // Fully authored local /v1/messages fixtures, captured from native
    // 2.1.258 and 2.1.270. Both emitted the identical Opus bytes below;
    // 2.1.270 also emitted the Sonnet variant with the same checksum.
    // All identity metadata is synthetic; no provider was contacted.
    for (model, max_tokens, wire_sha256) in [
        (
            "claude-opus-5",
            4096,
            "59848468ba54a5bbd704e43fdfb10bddc103db7ff5d193af050339d096b38e1f",
        ),
        (
            "claude-sonnet-5",
            8192,
            "d06fdb77b22e2dc2df1b88270f9c52b1c0a06ec56cd34c52d22d287ba92816e7",
        ),
    ] {
        let mut body = format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hello"}}],"system":[{{"type":"text","text":"{CAPTURED_258_BILLING_HEADER}"}}],"tools":[],"metadata":{{"user_id":"synthetic"}},"max_tokens":{max_tokens},"thinking":{{"type":"disabled"}},"context_management":{{"edits":[]}},"stream":true,"output_config":{{"effort":"low"}}}}"#
        )
        .into_bytes();
        attest_claude_oauth_cch(&mut body).expect("patch");
        assert!(std::str::from_utf8(&body).unwrap().contains("cch=51c04;"));
        assert_eq!(sha256_hex(&[&body]), wire_sha256);
    }
}

#[test]
fn native_cch_routing_fields_preserve_exact_retained_bytes() {
    // Source-derived exclusions; expected hashes are independent libxxhash
    // vectors over literal retained bytes, not this implementation's output.
    for body in [
        r#"{"model":"x","payload":true}"#,
        r#"{"max_tokens":1,"model":"x","payload":true}"#,
        r#"{"model":"x","max_tokens":99999,"payload":true}"#,
        r#"{"model":"x","payload":true,"max_tokens":42}"#,
        r#"{"fallbacks":[],"model":"x","payload":true}"#,
        r#"{"model":"x","fallback_credit_token":"synthetic","payload":true}"#,
        r#"{"model":"x","payload":true,"fallback_credit_token":"synthetic"}"#,
        r#"{"model":"x","payload":true,"fallbacks":[{"model":"y","nested":[["]\\\"["]]}]}"#,
        r#"{"max_tokens":1,"fallbacks":[],"fallback_credit_token":"synthetic","model":"x","payload":true,"max_tokens":2}"#,
    ] {
        serde_json::from_str::<serde_json::Value>(body).expect("valid synthetic JSON");
        assert_eq!(
            format!("{:05x}", cch_digest(body.as_bytes()) & 0xF_FFFF),
            "a611d",
            "{body}"
        );
    }
    assert_eq!(
        format!(
            "{:05x}",
            cch_digest(br#"{"model":"x","nested":{"model":"y","max_tokens":3,"keep":1}}"#)
                & 0xF_FFFF
        ),
        "af1bf"
    );
    // Native does not remove a comma before the current scan cursor.
    // These adjacent terminal exclusions retain the bytes `true,}`.
    assert_eq!(
        format!(
            "{:05x}",
            cch_digest(br#"{"model":"x","payload":true,"max_tokens":1,"fallbacks":[],"fallback_credit_token":"synthetic"}"#) & 0xF_FFFF
        ),
        "5abe4"
    );
}

#[test]
fn native_cch_literal_matching_does_not_reserialize_or_guess_invalid_values() {
    for (body, expected) in [
        (r#"{"model":"x","payload":true,"max_tokens": 4}"#, "ef489"),
        (r#"{"model":"x","payload":true,"max_tokens":"4"}"#, "5d5ed"),
        (r#"{"model":"x","payload":true,"max_tokens":-4}"#, "3f1db"),
        (r#"{"model":"x","payload":true,"fallbacks":[}"#, "01aa4"),
        (
            r#"{"model":"x","payload":true,"fallback_credit_token":"unterminated}"#,
            "5a131",
        ),
        (r#"{"model":"unterminated}"#, "945cb"),
    ] {
        assert_eq!(
            format!("{:05x}", cch_digest(body.as_bytes()) & 0xF_FFFF),
            expected,
            "{body}"
        );
    }
}

#[test]
fn attestation_checks_included_content_but_ignores_native_routing_changes() {
    let original = format!(
        r#"{{"model":"claude-opus-5","max_tokens":4096,"system":[{{"type":"text","text":"{CAPTURED_258_BILLING_HEADER}"}}],"messages":[{{"role":"user","content":"hello"}}]}}"#
    );
    let mut attested = original.as_bytes().to_vec();
    attest_claude_oauth_cch(&mut attested).expect("patch");
    let attested = String::from_utf8(attested).unwrap();
    assert_eq!(attested, original.replace("cch=00000", "cch=14d76"));
    assert!(claude_oauth_cch_matches_body(
        attested
            .replace("claude-opus-5", "claude-sonnet-5")
            .replace("4096", "8192")
            .as_bytes()
    ));
    for changed in [
        attested.replace("hello", "world"),
        attested.replace("cc_entrypoint=cli", "cc_entrypoint=other"),
        attested.replace(r#""messages":"#, r#""messages": "#),
    ] {
        assert!(!claude_oauth_cch_matches_body(changed.as_bytes()));
    }
}

#[test]
fn absent_billing_and_out_of_window_placeholders_never_mutate_the_body() {
    let no_billing =
        br#"{"model":"x","max_tokens":4,"messages":[{"role":"user","content":"cch=00000"}]}"#;
    let mut body = no_billing.to_vec();
    attest_claude_oauth_cch(&mut body).expect("not an OAuth billing body");
    assert_eq!(body, no_billing);
    assert!(!claude_oauth_cch_matches_body(&body));

    let outside = format!(
        "x-anthropic-billing-header:{}cch=00000",
        " ".repeat(CCH_SEARCH_WINDOW)
    );
    let mut body = outside.as_bytes().to_vec();
    assert!(attest_claude_oauth_cch(&mut body).is_err());
    assert_eq!(body, outside.as_bytes());
}

#[test]
fn attestation_overwrites_only_the_billing_placeholder() {
    let mut body =
        br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000;"}],"messages":[{"role":"user","content":"cch=00000"}]}"#
            .to_vec();
    attest_claude_oauth_cch(&mut body).expect("patch");
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains(r#""content":"cch=00000""#));
    assert!(!text.contains("cch=00000;"));
    assert!(claude_oauth_cch_matches_body(&body));
}

#[test]
fn attestation_keeps_cc_is_subagent_after_the_hash() {
    let mut body =
        br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.59d; cc_entrypoint=cli; cch=00000; cc_is_subagent=true;"}]}"#
            .to_vec();
    attest_claude_oauth_cch(&mut body).expect("patch");
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("cc_is_subagent=true;"));
    assert!(!text.contains("cch=00000"));
    assert!(claude_oauth_cch_matches_body(&body));
}

#[test]
fn missing_placeholder_on_a_billing_header_fails_closed() {
    let mut body = br#"{"text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=dead0"}"#
        .to_vec();
    assert!(attest_claude_oauth_cch(&mut body).is_err());
}
