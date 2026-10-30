//! `QuickJS` runtime for the isolated `plakous-js-host` worker.

use std::collections::HashMap;

use serde_json::Value;

mod embedded;
pub use embedded::EmbeddedHost;

/// Default cooperative yield interval (`CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1`).
pub const DEFAULT_YIELD_TIME_MS: u64 = 10_000;

/// Events emitted by one cell on the evaluator thread.
#[derive(Debug)]
pub enum RuntimeEvent {
    /// Nested tool request. Answer through the active evaluator host.
    ToolCall {
        /// Cell execution id.
        cell_id: u64,
        /// Tool call id local to the cell.
        id: u64,
        /// Nested tool name.
        name: String,
        /// JSON arguments.
        input: Value,
    },
    /// `notify(...)` from the cell.
    Notify {
        /// Cell execution id.
        cell_id: u64,
        /// Notification text.
        text: String,
    },
    /// `text` / `image` / `audio` helper output.
    Content {
        /// Cell execution id.
        cell_id: u64,
        /// Helper payload.
        content: Value,
    },
    /// `yield_control()` or cooperative yield timeout.
    Yielded {
        /// Cell execution id.
        cell_id: u64,
    },
    /// Cell completed.
    Done {
        /// Cell execution id.
        cell_id: u64,
        /// `store` writes in this cell.
        stored: HashMap<String, Value>,
    },
    /// Cell failed.
    Error {
        /// Cell execution id.
        cell_id: u64,
        /// Error text.
        message: String,
        /// `store` writes before failure.
        stored: HashMap<String, Value>,
    },
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::match_same_arms,
    clippy::needless_raw_string_hashes,
    reason = "unit tests"
)]
mod tests;

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
