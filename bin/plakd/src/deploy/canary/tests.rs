use super::*;
use plakous::subscriptions::SubscriptionRequestClass;

fn options() -> Options {
    Options {
        capabilities: false,
        preflight: false,
        account: Some("claude/work".into()),
        model: Some("claude-opus-5-5".into()),
        effort: Some("low".into()),
        protocol: None,
    }
}

#[test]
fn canary_selection_is_ephemeral_tool_free_and_dispatch_bounded() {
    let account = "claude/work".parse().unwrap();
    let selected = selection(&options(), &account).unwrap();
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
fn canary_does_not_fall_back_for_codex_openai_or_a_selected_protocol() {
    for account in ["codex", "openai"] {
        let error = selection(&options(), &account.parse().unwrap()).unwrap_err();
        assert!(error.message().contains("no bounded deploy canary adapter"));
    }
    let mut options = options();
    options.protocol = Some("messages".into());
    let error = selection(&options, &"claude".parse().unwrap()).unwrap_err();
    assert!(error.message().contains("does not select a wire protocol"));
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
