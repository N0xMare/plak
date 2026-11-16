//! Shared real-provider fixtures and headless disposable-view rebuild proof.

fn assert_live_compact_id(
    outcome: &AgentOutcome,
    views: &plakous_hosted::sessions::projector::SessionViews,
) {
    let projected_ids = views
        .rows
        .values()
        .filter_map(|row| match row {
            plakous_hosted::sessions::TranscriptRow::Tool(tool) if tool.name == "compact" => {
                Some(tool.call_id.as_str())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!projected_ids.is_empty(), "projected workflow compact rows");
    for row in outcome
        .records()
        .iter()
        .filter(|row| row["kind"] == "tool" && row["activity_kind"] == "compact")
    {
        assert!(
            projected_ids.contains(row["call_id"].as_str().unwrap()),
            "every live compact has its projected workflow row: {row}; projected={projected_ids:?}"
        );
    }
}

#[test]
fn in_turn_compaction_live_rows_use_the_projected_workflow_identity() {
    let (fixture, mut agent) = overflow_turn_then("continue after the fold");
    agent.wait_for_completed_turns(3);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    let session = outcome
        .records()
        .iter()
        .find(|row| row["kind"] == "hello")
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = SqliteSessionStore::new(PlakPaths::explicit(fixture.home()));
    let views = store.load_views(&session).unwrap().unwrap();
    assert!(outcome.records().iter().any(|row| {
        row["kind"] == "tool" && row["activity_kind"] == "compact" && row["state"] == "running"
    }));
    assert_live_compact_id(&outcome, &views);
    let mut product =
        plakd::store::ProductStore::open(&PlakPaths::explicit(fixture.home())).unwrap();
    product.delete_views(&session).unwrap();
    drop(product);
    let mut rebuilt = AgentProcess::start(&fixture, &["--session", &session]);
    rebuilt.wait_for("rebuilt ready", |row| row["kind"] == "ready");
    rebuilt.send(&quit_frame());
    assert!(rebuilt.finish().status.success());
    assert_eq!(
        store.load_views(&session).unwrap().unwrap().rows,
        views.rows
    );
}

#[test]
fn failed_manual_and_scheduled_compacts_rebuild_one_failure_line() {
    for scheduled in [false, true] {
        let fixture = Fixture::new(&json!({
            "version": 1,
            "maximum_projected_input": 20_000,
            "content_sensitive_projection": true,
            "fold": "fail",
            "steps": [
                {"output": {"text": format!("seed {}", "detail ".repeat(800))}, "usage": {}},
                {"output": {"text": "second seed"}, "usage": {"input": 19_000}},
                {"output": {"text": ANSWER}, "usage": {}}
            ]
        }));
        write_private(
            &fixture.home().join("config.toml"),
            &format!(
                "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = {scheduled}\n"
            ),
        );
        let mut agent = AgentProcess::start(&fixture, &[]);
        let hello = agent.wait_for("hello", |row| row["kind"] == "hello");
        let session = hello["session_id"].as_str().unwrap().to_owned();
        agent.wait_for("ready", |row| row["kind"] == "ready");
        for (index, prompt) in ["first seed", "second seed"].into_iter().enumerate() {
            agent.send(&prompt_frame(prompt));
            agent.wait_for_completed_turns(index + 1);
        }
        if scheduled {
            agent.send(&prompt_frame("continue after the scheduled compact"));
        } else {
            agent.send(&compact_frame());
        }
        agent.wait_for("failed compact", |row| {
            row["kind"] == "tool" && row["activity_kind"] == "compact" && row["state"] == "failed"
        });
        if !scheduled {
            agent.send(&prompt_frame("continue after the failed manual compact"));
        }
        agent.wait_for_completed_turns(3);
        agent.send(&quit_frame());
        let outcome = agent.finish();
        assert!(outcome.status.success(), "{}", outcome.stderr);
        let paths = PlakPaths::explicit(fixture.home());
        let store = SqliteSessionStore::new(paths.clone());
        let live = store.load_views(&session).unwrap().unwrap();
        assert_live_compact_id(&outcome, &live);
        let intents = store.load_intents(&session).unwrap();
        assert!(
            intents
                .workflow_requests
                .iter()
                .any(|request| request.compact_reason()
                    == Some(if scheduled {
                        plakous_hosted::sessions::ProductCompactReason::Scheduled
                    } else {
                        plakous_hosted::sessions::ProductCompactReason::Manual
                    })),
            "the intended workflow ran: {intents:?}"
        );
        let mut product = plakd::store::ProductStore::open(&paths).unwrap();
        product.delete_views(&session).unwrap();
        drop(product);
        let mut rebuilt = AgentProcess::start(&fixture, &["--session", &session]);
        rebuilt.wait_for("rebuilt ready", |row| row["kind"] == "ready");
        rebuilt.send(&quit_frame());
        assert!(rebuilt.finish().status.success());
        let views = store.load_views(&session).unwrap().unwrap();
        let failures = views
            .rows
            .values()
            .filter(|row| {
                matches!(row,
                    plakous_hosted::sessions::TranscriptRow::Notice(notice)
                        if notice.scope == plakous_hosted::sessions::NoticeScope::Compaction
                            && !notice.text.contains("the prompt fits")
                            && !notice.text.starts_with("scheduled compaction produced")
                            && !notice.text.starts_with("interrupt observed")
                )
            })
            .count();
        assert_eq!(failures, 1, "scheduled={scheduled}: {:#?}", views.rows);
    }
}

use super::*;
use plakd::home::paths::PlakPaths;
use plakd::store::session_records::{SessionRecordStore, SqliteSessionStore};

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
            plakd::store::ProductStore::inspect_session_read_only(&paths, &session).unwrap();
        if folding {
            assert_eq!(record.transcript.iter().filter(|row| matches!(row, plakous_hosted::sessions::TranscriptRow::Tool(tool) if tool.name == "compact")).count(), 1, "{shape}: one compact row");
        }
        let before = plakous_offline_crash_harness::evidence::read_evidence(
            fixture.root.join("evidence.jsonl"),
        )
        .unwrap();
        let mut product = plakd::store::ProductStore::open(&paths).unwrap();
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
            plakd::store::ProductStore::inspect_session_read_only(&paths, &session)
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
