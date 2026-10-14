//! Live TUI rows against the rows a resume rebuilds from the feed (spec 02
//! §2.6): the semantic pilot reads the live transcript tail of a finished
//! turn, the product views are deleted, and a resumed TUI over the rebuilt
//! views shows the same tail, while the rebuilt views equal the live ones.

use super::*;
use plak::home::paths::PlakPaths;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};

const ANSWER: &str = "pilot parity answer";

fn script(shape: &str) -> Value {
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

/// Launches the TUI under a fresh pilot directory and waits for its bound,
/// idle composer.
fn launch(
    fixture: &Fixture,
    pilot_dir: &str,
    resume: Option<&str>,
) -> (TerminalChild, PilotClient) {
    let pilot = PilotClient::new(fixture.root.join(pilot_dir));
    let mut terminal = TerminalChild::spawn_with_pilot(fixture, 100, resume, Some(&pilot.root));
    terminal.wait_screen("? keybinds");
    let mut pilot = pilot;
    pilot.observe_until("the bound, idle agent", |document| {
        document["snapshot"]["binding"]["attached"] == true
            && document["snapshot"]["phase"] == "idle"
    });
    (terminal, pilot)
}

fn tail(document: &Value) -> Vec<String> {
    document["snapshot"]["transcript"]["tail"]
        .as_array()
        .expect("transcript tail")
        .iter()
        .map(|line| line.as_str().expect("tail line").to_owned())
        .collect()
}

fn quit(terminal: TerminalChild, pilot: &mut PilotClient) {
    let quit = pilot.act("quit", None);
    assert_eq!(quit["error"], Value::Null);
    assert!(terminal.finish().success());
}

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
        plak::store::ProductStore::open(&PlakPaths::explicit(fixture.home())).unwrap();
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
        let mut product = plak::store::ProductStore::open(&paths).unwrap();
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

#[test]
fn live_tui_rows_equal_the_rows_a_resume_rebuilds_from_deleted_views() {
    for shape in [
        "final_only",
        "direct_tool_then_final",
        "hosted_tool_call_final",
        "code_mode_cell_nested_then_final",
        "manual_compaction",
        "in_turn_compaction",
    ] {
        let fixture = Fixture::new(&script(shape));
        let (mut terminal, mut pilot) = launch(&fixture, "pilot-live", None);
        pilot.observe();
        if matches!(shape, "manual_compaction" | "in_turn_compaction") {
            pilot.act("type-text", Some("seed a foldable turn"));
            assert_eq!(pilot.act("send", None)["error"], Value::Null);
            pilot.observe_until("the seed going idle", |document| {
                document["snapshot"]["phase"] == "idle"
                    && document["snapshot"]["turn"]["phase"] == "completed"
            });
        }
        pilot.act("type-text", Some("pilot parity prompt"));
        let sent = pilot.act("send", None);
        assert_eq!(sent["error"], Value::Null, "{shape}");
        terminal.wait_screen(ANSWER);
        let mut finished = pilot.observe_until("the turn going idle", |document| {
            document["snapshot"]["phase"] == "idle"
                && document["snapshot"]["turn"]["phase"] == "completed"
        });
        if shape == "manual_compaction" {
            pilot.act("type-text", Some("/compact"));
            assert_eq!(pilot.act("accept", None)["error"], Value::Null);
            finished = pilot.observe_until("the compact row settling", |document| {
                document["snapshot"]["phase"] == "idle"
                    && document["snapshot"]["busy"] == false
                    && tail(document)
                        .iter()
                        .any(|line| line.to_ascii_lowercase().contains("compact"))
            });
        }
        if matches!(shape, "manual_compaction" | "in_turn_compaction") {
            assert_eq!(
                tail(&finished)
                    .iter()
                    .filter(|line| line.to_ascii_lowercase().contains("compact"))
                    .count(),
                1,
                "one compact row in the live TUI: {finished}"
            );
        }
        let session = finished["snapshot"]["binding"]["session_id"]
            .as_str()
            .expect("bound session id")
            .to_owned();
        let live_tail = tail(&finished);
        quit(terminal, &mut pilot);

        let paths = PlakPaths::explicit(fixture.home());
        let store = SqliteSessionStore::new(paths.clone());
        let live = store.load_views(&session).unwrap().expect("live views");
        let mut product = plak::store::ProductStore::open(&paths).unwrap();
        product.delete_views(&session).unwrap();
        drop(product);

        let (terminal, mut pilot) = launch(&fixture, "pilot-rebuilt", Some(&session));
        let rebuilt_tail = tail(&pilot.observe());
        quit(terminal, &mut pilot);
        let rebuilt = store.load_views(&session).unwrap().expect("rebuilt views");

        assert_eq!(rebuilt.rows, live.rows, "{shape}: rebuilt view rows");
        assert_eq!(
            rebuilt_tail, live_tail,
            "{shape}: the resumed TUI shows the rows the live TUI showed"
        );
    }
}
