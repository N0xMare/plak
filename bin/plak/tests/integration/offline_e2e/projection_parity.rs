//! Shared real-provider fixtures and headless disposable-view rebuild proof.

use super::*;
use plak::home::paths::PlakPaths;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};

pub(super) const ANSWER: &str = "pilot parity answer";

pub(super) fn script(shape: &str) -> Value {
    let final_step = json!({"output": {"text": ANSWER}, "finish": "stop"});
    let tool_step = json!({"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"});
    match shape {
        "final_only" => json!({"version": 1, "steps": [final_step]}),
        "direct_tool_then_final" => json!({"version": 1, "steps": [tool_step, final_step]}),
        "hosted_tool_call_final" => json!({
            "version": 1,
            "hosted_tool_call_final": true,
            "steps": [tool_step, final_step]
        }),
        "manual_compaction" => json!({
            "version": 1,
            "maximum_projected_input": 20_000,
            "content_sensitive_projection": true,
            "fold": "accept",
            "steps": [
                {"output": {"text": format!("pilot foldable seed {}", "detail ".repeat(800))}},
                final_step
            ]
        }),
        "in_turn_compaction" => json!({
            "version": 1,
            "maximum_projected_input": 40_000,
            "scheduled_compaction": true,
            "content_sensitive_projection": true,
            "tool_result_bytes": 60_000,
            "steps": [
                {"output": {"text": "a".repeat(3_000)}, "usage": {}},
                {"output": {"tool": {"name": "loopback_effect", "arguments": {}}},
                 "finish": "tool_calls", "usage": {}},
                {"output": {"text": "Summary of earlier work:\nthe first answer was given"}},
                {"output": {"text": ANSWER}, "usage": {}}
            ]
        }),
        "code_mode_cell_nested_then_final" => code_mode_script(&json!([
            {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
            {"output": {"text": ANSWER}}
        ])),
        other => panic!("no script for {other}"),
    }
}

#[test]
fn real_jsonl_rows_equal_exact_resume_rebuilt_from_deleted_views_without_redispatch() {
    for shape in [
        "final_only",
        "direct_tool_then_final",
        "hosted_tool_call_final",
        "code_mode_cell_nested_then_final",
        "manual_compaction",
        "in_turn_compaction",
    ] {
        let fixture = Fixture::new(&script(shape));
        let folding = matches!(shape, "manual_compaction" | "in_turn_compaction");
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        if folding {
            agent.send(&prompt_frame("seed a foldable turn"));
            agent.wait_for_completed_turns(1);
        }
        agent.send(&prompt_frame("pilot parity prompt"));
        agent.wait_for_completed_turns(if folding { 2 } else { 1 });
        if shape == "manual_compaction" {
            agent.send(&json!({"v":1,"kind":"compact","id":"view-compact"}));
            agent.wait_for("manual compact settled", |record| {
                record["activity_kind"] == "compact" && record["state"] == "done"
            });
        }
        agent.send(&quit_frame());
        let outcome = agent.finish();
        assert!(outcome.status.success(), "{shape}: {}", outcome.stderr);
        outcome.assert_exactly_one_terminal_record();
        assert!(
            outcome
                .assistant_text()
                .iter()
                .any(|text| text.contains(ANSWER)),
            "{shape}"
        );
        let session = outcome.session_id();
        let paths = PlakPaths::explicit(fixture.home());
        let store = SqliteSessionStore::new(paths.clone());
        let live = store.load_views(&session).unwrap().unwrap();
        let record =
            plak::store::ProductStore::inspect_session_read_only(&paths, &session).unwrap();
        if folding {
            assert_eq!(record.transcript.iter().filter(|row| matches!(row, plakous_hosted::sessions::TranscriptRow::Tool(tool) if tool.name == "compact")).count(), 1, "{shape}: one compact row");
        }
        let before = plakous_offline_crash_harness::evidence::read_evidence(
            fixture.root.join("evidence.jsonl"),
        )
        .unwrap();
        let mut product = plak::store::ProductStore::open(&paths).unwrap();
        product.delete_views(&session).unwrap();
        drop(product);
        let mut resumed = AgentProcess::start(&fixture, &["--session", &session]);
        resumed.wait_for("exact reopened ready", |record| record["kind"] == "ready");
        resumed.send(&quit_frame());
        let resumed = resumed.finish();
        assert!(resumed.status.success(), "{shape}: {}", resumed.stderr);
        resumed.assert_exactly_one_terminal_record();
        assert_eq!(resumed.session_id(), session);
        let rebuilt = store.load_views(&session).unwrap().unwrap();
        assert_eq!(rebuilt.rows, live.rows, "{shape}: rebuilt view rows");
        assert_eq!(
            plak::store::ProductStore::inspect_session_read_only(&paths, &session)
                .unwrap()
                .transcript,
            record.transcript,
            "{shape}: complete transcript content and order"
        );
        assert_eq!(
            plakous_offline_crash_harness::evidence::read_evidence(
                fixture.root.join("evidence.jsonl")
            )
            .unwrap(),
            before,
            "{shape}: reopen must not dispatch work"
        );
    }
}
