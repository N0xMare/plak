use super::*;

#[test]
fn deploy_api_canary_requests_are_pinned_tool_free_single_completions() {
    for (kind, cap, path) in [
        ("openai", "max_output_tokens", "/v1/responses"),
        ("anthropic", "max_tokens", "/v1/messages"),
        ("xai-api", "max_output_tokens", "/v1/responses"),
        ("openrouter", "max_tokens", "/api/v1/chat/completions"),
    ] {
        let Some((_, actual_path, wire)) = target(kind) else {
            panic!("unregistered API canary: {kind}")
        };
        assert_eq!(actual_path, path);
        for low_effort in [false, true] {
            let request = wire.body("example-model", low_effort);
            assert_eq!(request["model"], "example-model");
            assert_eq!(request[cap], 16);
            assert!(request.get("tools").is_none());
            assert!(request.get("tool_choice").is_none());
            assert_ne!(request["stream"], true);
            if kind == "openrouter" {
                assert_eq!(request["provider"]["require_parameters"], true);
                assert_eq!(request["provider"]["allow_fallbacks"], false);
            }
        }
    }
    assert!(
        target("codex").is_none(),
        "no API-key fallback for subscriptions"
    );
}

#[test]
fn deploy_api_canary_requires_complete_and_provider_bounded_output() {
    let cases = [
        (
            Wire::Responses,
            "output_tokens",
            json!({"status":"completed","output":[{"role":"assistant","type":"message","content":[{"type":"output_text","text":"OK"}]}]}),
        ),
        (
            Wire::Messages,
            "output_tokens",
            json!({"type":"message","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"OK"}]}),
        ),
        (
            Wire::Chat,
            "completion_tokens",
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"OK"}}]}),
        ),
    ];
    for (wire, key, mut reply) in cases {
        assert!(!wire.completed(&reply), "unreported usage fails closed");
        reply["usage"] = json!({(key):2});
        assert!(wire.completed(&reply));
        reply["usage"][key] = json!(17);
        assert!(!wire.completed(&reply), "over-limit usage fails closed");
    }
}
