//! `QuickJS` runtime for the isolated `plakous-js-host` worker.

use plakous_js_eval::RuntimeEvent;
#[cfg(test)]
use {plakous_js_eval::DEFAULT_YIELD_TIME_MS, serde_json::Value, std::collections::HashMap};

mod embedded;
pub use embedded::EmbeddedHost;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::match_same_arms,
    clippy::needless_raw_string_hashes,
    reason = "unit tests"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_helper_completes() {
        let host = EmbeddedHost::spawn().expect("spawn");
        host.start_cell(
            1,
            r#"text("hello")"#,
            HashMap::new(),
            vec![serde_json::json!({
                "name": "noop",
                "tool_name": "noop",
                "kind": "function",
                "description": "noop",
            })],
            DEFAULT_YIELD_TIME_MS,
        )
        .expect("start");
        let mut saw_hello = false;
        loop {
            match host.read_event().await.expect("event") {
                RuntimeEvent::Content { content, .. } => {
                    let text = content.get("text").and_then(Value::as_str);
                    assert_eq!(text, Some("hello"));
                    saw_hello = true;
                }
                RuntimeEvent::Done { .. } => break,
                RuntimeEvent::Error { message, .. } => panic!("{message}"),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(saw_hello);
        host.terminate().await.expect("joined");
    }

    #[tokio::test]
    async fn nested_tool_round_trip() {
        let host = EmbeddedHost::spawn().expect("spawn");
        host.start_cell(
            2,
            r#"const r = await tools.echo({ x: 1 }); text(r.x);"#,
            HashMap::new(),
            vec![serde_json::json!({
                "name": "echo",
                "tool_name": "echo",
                "kind": "function",
                "description": "echo",
            })],
            DEFAULT_YIELD_TIME_MS,
        )
        .expect("start");
        loop {
            match host.read_event().await.expect("event") {
                RuntimeEvent::ToolCall {
                    id, name, input, ..
                } => {
                    assert_eq!(name, "echo");
                    assert_eq!(input["x"], 1);
                    host.send_tool_result(2, id, serde_json::json!({"x": 1}), true)
                        .await
                        .expect("result");
                }
                RuntimeEvent::Content { content, .. } => {
                    assert_eq!(content.get("text").and_then(Value::as_str), Some("1"));
                }
                RuntimeEvent::Done { .. } => break,
                RuntimeEvent::Error { message, .. } => panic!("{message}"),
                RuntimeEvent::Notify { .. } | RuntimeEvent::Yielded { .. } => {}
            }
        }
        host.terminate().await.expect("joined");
    }

    #[tokio::test]
    async fn yield_then_content_then_done() {
        let host = EmbeddedHost::spawn().expect("spawn");
        host.start_cell(
            3,
            r#"
await yield_control();
text("after-yield");
"#,
            HashMap::new(),
            vec![serde_json::json!({
                "name": "noop",
                "tool_name": "noop",
                "kind": "function",
                "description": "noop",
            })],
            DEFAULT_YIELD_TIME_MS,
        )
        .expect("start");
        let mut yielded = false;
        let mut after = false;
        loop {
            match host.read_event().await.expect("event") {
                RuntimeEvent::Yielded { cell_id } => {
                    assert_eq!(cell_id, 3);
                    assert!(!after, "content must not precede resume");
                    yielded = true;
                    host.resume(3, DEFAULT_YIELD_TIME_MS).expect("resume");
                }
                RuntimeEvent::Content { content, .. } => {
                    assert!(yielded, "content must follow yield");
                    assert_eq!(
                        content.get("text").and_then(Value::as_str),
                        Some("after-yield")
                    );
                    after = true;
                }
                RuntimeEvent::Done { .. } => break,
                RuntimeEvent::Error { message, .. } => panic!("{message}"),
                RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
            }
        }
        assert!(yielded && after);
        host.terminate().await.expect("joined");
    }

    #[tokio::test]
    async fn interrupt_stops_busy_javascript() {
        let host = EmbeddedHost::spawn().expect("spawn");
        host.start_cell(
            4,
            r"
await yield_control();
while (true) {}
",
            HashMap::new(),
            vec![serde_json::json!({
                "name": "noop",
                "tool_name": "noop",
                "kind": "function",
                "description": "noop",
            })],
            DEFAULT_YIELD_TIME_MS,
        )
        .expect("start");
        let mut saw_yield = false;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), host.read_event()).await {
                Ok(Ok(RuntimeEvent::Yielded { .. })) => {
                    saw_yield = true;
                    host.interrupt();
                }
                Ok(Ok(RuntimeEvent::Error { .. } | RuntimeEvent::Done { .. })) => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    host.interrupt();
                    break;
                }
            }
        }
        assert!(saw_yield);
        host.terminate().await.expect("joined");
    }

    #[tokio::test]
    async fn timers_do_not_fire_while_yielded() {
        let host = EmbeddedHost::spawn().expect("spawn");
        host.start_cell(
            5,
            r#"
setTimeout(() => text("timer"), 1);
await yield_control();
"#,
            HashMap::new(),
            vec![serde_json::json!({
                "name": "noop",
                "tool_name": "noop",
                "kind": "function",
                "description": "noop",
            })],
            DEFAULT_YIELD_TIME_MS,
        )
        .expect("start");
        let mut yielded = false;
        loop {
            match host.read_event().await.expect("event") {
                RuntimeEvent::Yielded { cell_id } => {
                    assert_eq!(cell_id, 5);
                    yielded = true;
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        host.read_event(),
                    )
                    .await
                    {
                        Ok(Ok(RuntimeEvent::Content { .. })) => {
                            panic!("timer content must not arrive before resume")
                        }
                        Ok(Ok(other)) => panic!("unexpected {other:?}"),
                        Ok(Err(error)) => panic!("{error}"),
                        Err(_) => {}
                    }
                    host.resume(5, DEFAULT_YIELD_TIME_MS).expect("resume");
                }
                RuntimeEvent::Content { content, .. } => {
                    assert!(yielded, "content must follow resume");
                    assert_eq!(content.get("text").and_then(Value::as_str), Some("timer"));
                }
                RuntimeEvent::Done { .. } => break,
                RuntimeEvent::Error { message, .. } => panic!("{message}"),
                RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
            }
        }
        assert!(yielded);
        host.terminate().await.expect("joined");
    }

    #[tokio::test]
    async fn auto_yield_parks_before_timer_content() {
        let host = EmbeddedHost::spawn().expect("spawn");
        host.start_cell(
            6,
            r#"
await new Promise((resolve) => setTimeout(resolve, 400));
text("done");
"#,
            HashMap::new(),
            vec![serde_json::json!({
                "name": "noop",
                "tool_name": "noop",
                "kind": "function",
                "description": "noop",
            })],
            50,
        )
        .expect("start");
        let mut yielded = false;
        let mut saw_done = false;
        loop {
            match host.read_event().await.expect("event") {
                RuntimeEvent::Yielded { cell_id } => {
                    assert_eq!(cell_id, 6);
                    assert!(!saw_done, "content must not precede resume");
                    yielded = true;
                    host.resume(6, DEFAULT_YIELD_TIME_MS).expect("resume");
                }
                RuntimeEvent::Content { content, .. } => {
                    assert!(yielded, "content must follow yield");
                    assert_eq!(content.get("text").and_then(Value::as_str), Some("done"));
                    saw_done = true;
                }
                RuntimeEvent::Done { .. } => break,
                RuntimeEvent::Error { message, .. } => panic!("{message}"),
                RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
            }
        }
        assert!(yielded && saw_done);
        host.terminate().await.expect("joined");
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "regression tests"
)]
mod resource_regressions {
    use super::*;

    async fn finite_output_loss(source: &str, tools: Vec<Value>) {
        let host = EmbeddedHost::spawn().expect("host");
        host.start_cell(1, source, HashMap::new(), tools, 10_000)
            .expect("start");
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match host.read_event().await {
                    Err(error) => break error,
                    Ok(RuntimeEvent::Done { .. }) => {
                        panic!("unbounded generation reported success")
                    }
                    Ok(_) => {}
                }
            }
        })
        .await
        .expect("terminal deadline");
        assert!(outcome.contains("resource limit"), "{outcome}");
        let _ = host.terminate().await;
    }

    #[tokio::test]
    async fn finite_repeated_text_has_resource_terminal() {
        finite_output_loss("for (let i=0; i<5000; i++) text('x');", vec![]).await;
    }
    #[tokio::test]
    async fn finite_repeated_notify_has_resource_terminal() {
        finite_output_loss("for (let i=0; i<5000; i++) notify('x');", vec![]).await;
    }
    /// A terminal that fails to decode while a tool request is still pending
    /// is an ordinary resource loss: the pending resolver is released through
    /// the same cleanup path as every other post-start error and the host
    /// stays joinable.
    #[tokio::test]
    async fn oversized_terminal_with_pending_tool_is_a_resource_loss() {
        finite_output_loss(
            "tools.echo({}); store('big', 'x'.repeat(9 * 1024 * 1024));",
            vec![serde_json::json!({"name": "echo", "tool_name": "echo", "kind": "function"})],
        )
        .await;
    }

    #[tokio::test]
    async fn finite_unawaited_calls_have_resource_terminal() {
        finite_output_loss(
            "for (let i=0; i<128; i++) tools.echo({x: i});",
            vec![serde_json::json!({"name": "echo", "tool_name": "echo", "kind": "function"})],
        )
        .await;
    }
}
