use super::*;
use plakous::subscriptions::SubscriptionRequestClass;

#[test]
fn canary_selection_is_ephemeral_tool_free_and_dispatch_bounded() {
    let selected = selection(
        NativeProvider::Claude,
        "claude-opus-5-5",
        Some(ReasoningEffort::Low),
        None,
    )
    .unwrap();
    let behavior = selected.behavior();
    assert!(behavior.tools.is_empty() && behavior.hosted_tools.is_empty());
    assert_eq!(behavior.tool_choice, ToolChoice::None);
    assert_eq!(
        behavior.local_durability,
        LocalDurabilityDisposition::Ephemeral
    );
    assert_eq!(behavior.output_bound.bound().unwrap().get(), 16);
    let limits = limits().unwrap();
    assert_eq!(limits.allowance(SubscriptionRequestClass::Inference), 1);
    assert_eq!(limits.retries(), 0);
    assert_eq!(limits.max_elapsed(), Some(Duration::from_secs(90)));
}

#[test]
fn canary_codex_uses_no_wire_bound_and_a_different_evidence_class() {
    let selected = selection(
        NativeProvider::Codex,
        "gpt-6.2-sol",
        Some(ReasoningEffort::Low),
        Some(NativeToolMode::CodeModeOnly),
    )
    .unwrap();
    assert_eq!(
        selected.behavior().output_bound,
        OutputBoundDisposition::NoRequestBound
    );
    assert_eq!(selected.behavior().tool_choice, ToolChoice::None);
    assert_eq!(
        selected.behavior().local_durability,
        LocalDurabilityDisposition::Ephemeral
    );
    assert_eq!(class("codex"), "locally_bounded");
    assert_ne!(pass_line("codex"), pass_line("grok"));
    assert_eq!(
        lowest_effort(&[
            ReasoningEffort::High,
            ReasoningEffort::Low,
            ReasoningEffort::Medium
        ])
        .unwrap(),
        Some(ReasoningEffort::Low),
    );
}

#[test]
fn canary_http_refusal_is_a_closed_content_free_line() {
    assert_eq!(
        refused_status(b"plakd-deploy-canary-upstream-http 429\n"),
        Some(429)
    );
    assert_eq!(
        refused_status(b"plakd-deploy-canary-upstream-http 503\n"),
        Some(503)
    );
    for malformed in [
        &b"plakd-deploy-canary-upstream-http 0429\n"[..],
        b"plakd-deploy-canary-upstream-http 200\n",
        b"plakd-deploy-canary-upstream-http 503 unrelated text\n",
    ] {
        assert_eq!(refused_status(malformed), None);
    }
}

#[tokio::test(start_paused = true)]
async fn canary_joins_cleanup_after_its_turn_deadline() {
    let cleaned = std::cell::Cell::new(false);
    let result = join_after(
        Instant::now() + Duration::from_millis(1),
        std::future::pending(),
        || async {
            cleaned.set(true);
            Ok::<_, ()>(())
        },
    )
    .await;
    assert!(cleaned.get());
    assert!(
        result
            .unwrap_err()
            .message()
            .contains("turn deadline expired")
    );
}
