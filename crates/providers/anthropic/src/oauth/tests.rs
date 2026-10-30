use super::*;

/// Every consent-bearing Claude OAuth fact, hashed: client id, authorize
/// and token origins and paths, both scope strings, the redirect and the
/// authorize query contract. A digest change is a consent-contract
/// change: under spec 01 §7.2 it requires a new profile id, notice
/// revision and kill-switch revision in the same edit.
#[cfg(feature = "native-auth")]
#[test]
fn claude_consent_facts_are_bound_to_the_contract_revision() {
    let authorize_url = claude_authorize_url("<challenge>", "<state>");
    let mut facts = String::new();
    for (name, value) in [
        ("client_id", CLAUDE_OAUTH_CLIENT_ID),
        ("authorize_origin", CLAUDE_OAUTH_AUTHORIZE_ORIGIN),
        ("authorize_path", CLAUDE_OAUTH_AUTHORIZE_PATH),
        ("token_origin", CLAUDE_OAUTH_TOKEN_ORIGIN),
        ("token_path", CLAUDE_OAUTH_TOKEN_PATH),
        ("authorize_scopes", CLAUDE_OAUTH_AUTHORIZE_SCOPES),
        ("refresh_scopes", CLAUDE_OAUTH_REFRESH_SCOPES),
        ("redirect_uri", CLAUDE_OAUTH_REDIRECT_URI),
        ("authorize_url", authorize_url.as_str()),
    ] {
        facts.push_str(name);
        facts.push('=');
        facts.push_str(value);
        facts.push('\n');
    }
    assert_eq!(
        (
            CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            CLAUDE_OAUTH_NOTICE_REVISION,
            CLAUDE_OAUTH_KILL_SWITCH_REVISION,
            plakous_model::sha256_hex(&[facts.as_bytes()]).as_str(),
        ),
        (
            "anthropic.claude.oauth.compat.2026-09-22.r2",
            "anthropic.claude.oauth.notice.2026-08-20.r1",
            "anthropic.claude.oauth.kill.2026-08-20.r1",
            "bd49ccd00968811b1669b46c3d41ab971626ac28215256487d3999dc069cf54b",
        )
    );
}

#[test]
fn authorize_url_uses_cai_not_platform_authorize() {
    let url = claude_authorize_url("challenge", "independent-state");
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
    assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
    assert!(url.contains("code=true"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(!url.contains("platform.claude.com/oauth/authorize"));
}

#[test]
fn token_grant_requires_access_and_expiry() {
    let grant = parse_claude_oauth_token_grant(
        br#"{"access_token":"at","refresh_token":"rt","expires_in":3600,"scope":"user:inference"}"#,
    )
    .expect("grant");
    assert_eq!(grant.access_token(), "at");
    assert_eq!(grant.refresh_token(), Some("rt"));
    assert_eq!(grant.expires_in(), 3600);
    assert!(claude_oauth_scope_admits_inference(grant.scope().unwrap()));
    assert!(parse_claude_oauth_token_grant(br#"{"access_token":"at"}"#).is_err());
}

#[test]
fn token_material_is_redacted() {
    let grant = parse_claude_oauth_token_grant(
        br#"{"access_token":"fixture-access-secret","refresh_token":"fixture-refresh-secret","expires_in":3600}"#,
    ).unwrap();
    let refresh = claude_token_refresh_body("fixture-refresh-secret");
    let code = claude_token_code_body(
        "fixture-code-secret",
        "redirect",
        "fixture-verifier-secret",
        "state",
    );
    for debug in [
        format!("{grant:?}"),
        format!("{refresh:?}"),
        format!("{code:?}"),
    ] {
        for secret in [
            "fixture-access-secret",
            "fixture-refresh-secret",
            "fixture-code-secret",
            "fixture-verifier-secret",
        ] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }
    assert!(refresh.expose_secret().contains("fixture-refresh-secret"));
    assert!(code.expose_secret().contains("fixture-verifier-secret"));
}

#[test]
fn invalid_expiry_keeps_wire_tokens_in_secret_containers() {
    let bytes = br#"{"access_token":"fixture-access-secret","refresh_token":"fixture-refresh-secret","expires_in":0}"#;
    let wire: TokenGrantWire = serde_json::from_slice(bytes).unwrap();
    // Rejected wire values have the same zeroizing ownership as admitted grants.
    let _: &SecretString = &wire.access_token;
    let _: &SecretString = wire.refresh_token.as_ref().unwrap();
    assert!(!format!("{:?}", wire.access_token).contains("fixture-access-secret"));
    let error = parse_claude_oauth_token_grant(bytes).unwrap_err();
    assert!(!format!("{error:?}").contains("fixture-refresh-secret"));
}

#[test]
fn compatibility_envelope_is_pinned_to_claude_code_messages_costume() {
    // Re-pinning inference must not strand an already selected login or
    // demand a new compatibility acknowledgement for unchanged auth.
    assert_eq!(
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
        "anthropic.claude.oauth.compat.2026-09-22.r2"
    );
    assert_eq!(CLAUDE_OAUTH_SOURCE_REVISION, "claude-oauth.2026-09-22.v3");
    assert!(!CLAUDE_OAUTH_BASELINE_BETAS.contains("context-1m"));
    assert!(!CLAUDE_OAUTH_BASELINE_BETAS.contains("per-turn-control"));
    assert_eq!(CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION, "0.128.0");
    #[cfg(feature = "native-auth")]
    assert!(CLAUDE_OAUTH_USER_AGENT.starts_with("plak/"));
    assert_eq!(
        CLAUDE_OAUTH_MESSAGES_USER_AGENT,
        "claude-cli/2.1.295 (external, cli)"
    );
    assert_eq!(
        crate::cch::CLAUDE_OAUTH_BILLING_HEADER,
        "x-anthropic-billing-header: cc_version=2.1.295.8a8; cc_entrypoint=cli; cch=00000;"
    );
    assert_eq!(
        CLAUDE_OAUTH_IDENTITY,
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_ne!(claude_oauth_stainless_arch(), "");
    assert_ne!(claude_oauth_stainless_os(), "");
}

#[test]
fn enrolled_effort_options_are_exact_per_model_and_unknowns_fail_closed() {
    for model in [
        "claude-sonnet-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
        "claude-opus-5",
        "claude-opus-5-5",
    ] {
        assert_eq!(
            claude_oauth_reasoning_effort_options(model),
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ],
            "{model} must retain its exact effort set"
        );
    }
    assert_eq!(
        claude_oauth_reasoning_effort_options("claude-sonnet-5-5"),
        CLAUDE_OAUTH_EXTENDED_REASONING_EFFORTS
    );
    for dropped in [
        // Adjacent Platform or client documentation alone does not qualify
        // a subscription model.
        "claude-fable-5",
        "claude-sonnet-4-6",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-haiku-4-5",
        "claude-fable-5-neighbor",
    ] {
        assert!(
            claude_oauth_reasoning_effort_options(dropped).is_empty(),
            "{dropped} must fail closed"
        );
    }
}

#[test]
fn enrolled_context_and_output_facts_come_from_one_table_and_unknowns_fail_closed() {
    assert_eq!(
        claude_oauth_enrolled_models().collect::<Vec<_>>(),
        [
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-opus-5-5",
        ]
    );
    for model in [
        "claude-sonnet-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
        "claude-opus-5",
        "claude-opus-5-5",
    ] {
        let row = claude_enrollment(model).expect("enrolled");
        assert_eq!(row.context_window, 1_000_000);
        assert!(claude_oauth_enrolled_model(model));
        assert_eq!(claude_max_output_tokens(model), 128_000);
        assert_eq!(
            claude_oauth_beta_header(model),
            row.oauth_betas,
            "{model} beta header must come from its enrollment row"
        );
    }
    let expected = [
        ("claude-sonnet-5", 64_000, false, false),
        ("claude-sonnet-5-5", 128_000, true, true),
        ("claude-haiku-5-5", 128_000, true, true),
        ("claude-fable-5-1", 64_000, true, true),
        ("claude-opus-5", 64_000, false, true),
        ("claude-opus-5-5", 128_000, true, true),
    ];
    for (model, request_max, per_turn, mid_tools) in expected {
        let row = claude_enrollment(model).expect("enrolled");
        assert_eq!(row.request_max_tokens, request_max, "{model}");
        assert_eq!(
            claude_oauth_request_max_tokens(model),
            Some(request_max),
            "{model}"
        );
        let header = row.oauth_betas;
        assert_eq!(
            header.contains("per-turn-control-2026-07-01"),
            per_turn,
            "{model}"
        );
        assert_eq!(
            header.contains("mid-conversation-tool-changes-2026-07-01"),
            mid_tools,
            "{model}"
        );
        assert!(!header.contains("context-1m"), "{model}");
        let tokens: Vec<&str> = header.split(',').collect();
        let code_at = tokens
            .iter()
            .position(|token| *token == "claude-code-20250219")
            .expect("claude-code beta");
        if model.contains("haiku") {
            assert_eq!(code_at, 6, "{model} puts claude-code seventh");
        } else {
            assert_eq!(code_at, 0, "{model} puts claude-code first");
        }
    }
    assert_eq!(
        claude_enrollment("claude-fable-5-1")
            .expect("fable")
            .oauth_betas,
        claude_enrollment("claude-sonnet-5-5")
            .expect("sonnet 5.5")
            .oauth_betas,
        "fable has both registry flags and is not Haiku"
    );
    assert_eq!(
        claude_oauth_beta_header("claude-sonnet-5"),
        CLAUDE_OAUTH_BASELINE_BETAS
    );
    assert!(claude_oauth_request_max_tokens("claude-haiku-4-5").is_none());
    let haiku = claude_enrollment("claude-haiku-5-5").expect("haiku 5.5");
    assert_eq!(haiku.family, "claude-5-haiku");
    assert!(haiku.oauth);
    for model in ["claude-haiku-4-5", "claude-future-neighbor"] {
        assert!(claude_enrollment(model).is_none());
        assert_eq!(claude_max_output_tokens(model), 64_000);
        assert_eq!(claude_oauth_reasoning_effort_options(model), []);
    }
}

#[test]
fn paste_input_accepts_callback_url_query_hash_and_bare_code() {
    let state = "a".repeat(43);
    let code = "b".repeat(32);
    let url = format!("{CLAUDE_OAUTH_REDIRECT_URI}?code={code}&state={state}");
    assert_eq!(
        parse_claude_oauth_authorization_input(&url, &state).as_deref(),
        Some(code.as_str())
    );
    assert_eq!(
        parse_claude_oauth_authorization_input(
            &format!("{CLAUDE_OAUTH_REDIRECT_URI}?code={code}"),
            &state
        )
        .as_deref(),
        Some(code.as_str())
    );
    assert_eq!(
        parse_claude_oauth_authorization_input(&format!("code={code}"), &state).as_deref(),
        Some(code.as_str())
    );
    assert_eq!(
        parse_claude_oauth_authorization_input(&format!("{code}#{state}"), &state).as_deref(),
        Some(code.as_str())
    );
    assert_eq!(
        parse_claude_oauth_authorization_input(&code, &state).as_deref(),
        Some(code.as_str())
    );
    assert!(
        parse_claude_oauth_authorization_input(
            &format!("{CLAUDE_OAUTH_REDIRECT_URI}?code={code}&state=other-state-value-xxxxxxxx"),
            &state
        )
        .is_none()
    );
    assert!(parse_claude_oauth_authorization_input("nope", &state).is_none());
    assert!(
        parse_claude_oauth_authorization_input(
            "https://evil.example/oauth/code/callback?code=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &state
        )
        .is_none()
    );
}
