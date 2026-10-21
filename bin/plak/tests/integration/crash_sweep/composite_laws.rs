//! Composite product laws, using the complete `SQLite` snapshot and real host.

use super::*;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};
use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
use std::sync::Arc;

pub(super) fn assert_at_tail(fixture: &Fixture, id: &str) {
    let paths = summary::paths(fixture);
    let store = SqliteSessionStore::new(paths.clone());
    let record = store.load(id).unwrap();
    let views = store.load_views(id).unwrap().unwrap();
    let sessions = runtime_sessions(fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let page = executor
        .block_on(sessions.feed_page(FeedPageRequest::after(FeedCursor::new(
            record.plakous_locator.unwrap().session_id(),
            views.cursor,
        ))))
        .unwrap()
        .unwrap();
    assert!(
        matches!(page, FeedPage::Records { records, tail, .. }
            if records.is_empty() && tail == views.cursor),
        "{id}: host did not apply the tail without a harness pass"
    );
}

fn runtime_sessions(fixture: &Fixture) -> plakous::sessions::AgentSessions {
    plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(summary::paths(fixture).runtime_database())
            .unwrap(),
    )
    .unwrap()
}

fn project(fixture: &Fixture, ids: &[String]) {
    let store: Arc<dyn SessionRecordStore> =
        Arc::new(SqliteSessionStore::new(summary::paths(fixture)));
    let sessions = runtime_sessions(fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    for id in ids {
        let record = store.load(id).unwrap();
        let projected = executor
            .block_on(plakous_hosted::runtime::project_session(
                &sessions, &store, &record,
            ))
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            projected.pending_commands.is_empty(),
            "{id}: pending commands"
        );
        assert!(
            projected.pending_workflows.is_empty(),
            "{id}: pending workflows"
        );
    }
}

fn all_laws(fixture: &Fixture, ids: &[String], label: &str, script: &Value) {
    project(fixture, ids);
    for id in ids {
        view_idempotence_tests::assert_idempotent(fixture, id, label);
    }
    let paths = summary::paths(fixture);
    let before = view_snapshot::snapshot(&paths);
    let mut store = plak::store::ProductStore::open(&paths).unwrap();
    for id in ids {
        store.delete_views(id).unwrap();
        assert!(store.load_views(id).unwrap().is_none());
    }
    drop(store);
    project(fixture, ids);
    let rebuilt = view_snapshot::snapshot(&paths);
    if rebuilt != before {
        view_snapshot::explain_difference(&before, &rebuilt);
    }
    assert_eq!(rebuilt, before, "{label}: rebuild");
    for id in ids {
        view_rebuild_laws::assert_plain_open_twin(fixture, id, label, script);
    }
}

fn ready(fixture: &Fixture) -> (AgentProcess, String) {
    let agent = AgentProcess::start(fixture, &[]);
    let hello = agent.wait_for("hello", |row| row["kind"] == "hello");
    agent.wait_for("ready", |row| row["kind"] == "ready");
    (agent, hello["session_id"].as_str().unwrap().to_owned())
}

fn finish(mut agent: AgentProcess) {
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
}

fn text_script() -> Value {
    json!({"version":1,"maximum_projected_input":20_000,
        "content_sensitive_projection":true,"fold":"accept",
        "steps":[{"output":{"text":"first answer"}},
                 {"output":{"text":"second answer"}},
                 {"output":{"text":"after workflow"}}]})
}

#[test]
fn composite_manual_compaction_obeys_all_three_sqlite_laws() {
    let script = text_script();
    let fixture = Fixture::new(&script);
    super::super::super::offline_support::write_private(
        &fixture.home().join("config.toml"),
        "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = false\n",
    );
    let (mut agent, id) = ready(&fixture);
    for (index, prompt) in ["first intent", "second intent"].iter().enumerate() {
        agent.send(&prompt_frame(prompt));
        agent.wait_for_completed_turns(index + 1);
    }
    agent.send(&json!({"v":1,"kind":"compact","id":"manual"}));
    let compact = agent.wait_for("manual compact", |row| {
        row["kind"] == "tool"
            && row["activity_kind"] == "compact"
            && matches!(row["state"].as_str(), Some("done" | "failed"))
    });
    assert_eq!(compact["state"], "done");
    agent.send(&prompt_frame("after manual compaction"));
    agent.wait_for_completed_turns(3);
    finish(agent);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    assert_eq!(store.load(&id).unwrap().compaction_count, 1);
    let intents = store.load_intents(&id).unwrap();
    assert_eq!(intents.workflow_requests.len(), 1);
    assert!(matches!(
        intents.workflow_requests[0].kind,
        plakous_hosted::sessions::SessionWorkflowKind::Compact(
            plakous_hosted::sessions::ProductCompactReason::Manual
        )
    ));
    all_laws(&fixture, &[id], "manual compact", &script);
}

#[test]
fn composite_discarded_compaction_obeys_all_three_sqlite_laws() {
    let script = text_script();
    let fixture = Fixture::new(&script);
    let trace_path = fixture.root.join("discard-trace.jsonl");
    let mut command = fixture.command();
    command.arg("agent").env(TRACE_ENV, &trace_path);
    let mut agent = AgentProcess::start_command(command, true);
    let hello = agent.wait_for("hello", |row| row["kind"] == "hello");
    let id = hello["session_id"].as_str().unwrap().to_owned();
    agent.wait_for("ready", |row| row["kind"] == "ready");
    agent.send(&json!({"v":1,"kind":"compact","id":"discard"}));
    agent.wait_for("definitive no-commit compaction", |row| {
        row["id"] == "discard"
            && row["kind"] == "status"
            && row["phase"] == "note"
            && row["detail"].as_str().is_some_and(|text| {
                text.contains("nothing in this conversation can be compacted yet")
            })
    });
    agent.send(&prompt_frame("continue after discard"));
    agent.wait_for_completed_turns(1);
    finish(agent);
    assert!(
        trace::read(&trace_path).iter().any(|commit| commit
            .tables
            .iter()
            .any(|table| table == "workflow_requests")),
        "the corpus never submitted a workflow request"
    );
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    assert_eq!(
        store.load_intents(&id).unwrap().workflow_requests,
        [] as [plakous_hosted::sessions::projector::WorkflowRequest; 0]
    );
    assert_eq!(store.load(&id).unwrap().compaction_count, 0);
    all_laws(&fixture, &[id], "discarded compact", &script);
}

#[test]
fn composite_fork_and_clear_both_sides_obey_all_three_sqlite_laws() {
    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    agent.send(&prompt_frame("before fork"));
    agent.wait_for_completed_turns(1);
    agent.send(&json!({"v":1,"kind":"fork","id":"fork"}));
    agent.wait_for("fork committed", |row| {
        row["kind"] == "status"
            && row["id"] == "fork"
            && row["detail"]
                .as_str()
                .is_some_and(|text| text.starts_with("forked session"))
    });
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    let fork = store
        .load_intents(&source)
        .unwrap()
        .workflow_requests
        .iter()
        .find(|request| {
            matches!(
                request.kind,
                plakous_hosted::sessions::SessionWorkflowKind::Fork
            )
        })
        .unwrap()
        .successor_product_id();
    agent.send(&prompt_frame("after fork"));
    agent.wait_for_completed_turns(2);
    agent.send(&json!({"v":1,"kind":"clear","id":"clear"}));
    agent.wait_for("clear committed", |row| {
        row["kind"] == "status"
            && row["id"] == "clear"
            && row["detail"]
                .as_str()
                .is_some_and(|text| text.starts_with("new session"))
    });
    let clear = store
        .load_intents(&fork)
        .unwrap()
        .workflow_requests
        .iter()
        .find(|request| {
            matches!(
                request.kind,
                plakous_hosted::sessions::SessionWorkflowKind::Clear
            )
        })
        .unwrap()
        .successor_product_id();
    agent.send(&prompt_frame("after clear"));
    agent.wait_for_completed_turns(3);
    finish(agent);
    assert_ne!(source, fork);
    assert_ne!(fork, clear);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    assert_eq!(
        store
            .load(&fork)
            .unwrap()
            .source_product_session_id
            .as_deref(),
        Some(source.as_str())
    );
    assert_eq!(
        store
            .load(&clear)
            .unwrap()
            .source_product_session_id
            .as_deref(),
        Some(fork.as_str())
    );
    all_laws(&fixture, &[source, fork, clear], "fork and clear", &script);
}

#[test]
fn composite_finished_child_obeys_all_three_sqlite_laws() {
    let script = json!({"version":1,"tree":true,"steps":[
        {"output":{"tool":{"name":"spawn_agent","arguments":{
            "template":"implement","tier":"mutate","workspace":"fork",
            "description":"law corpus child","prompt":"make the scripted change",
            "output_schema_json":"{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
        }}},"finish":"tool_calls"},
        {"output":{"tool":{"name":"wait_agent","arguments":{
            "agent_ids":["$result.agent_id"]}}},"finish":"tool_calls"},
        {"output":{"text":"child finished"}}
    ]});
    let fixture = Fixture::new(&script);
    let (mut agent, id) = ready(&fixture);
    agent.send(&prompt_frame("spawn and finish a child"));
    agent.wait_for("finished child", |row| {
        row["kind"] == "tool"
            && row["name"] == "child_result"
            && row["subject"] == "m1 \u{b7} c1 completed"
    });
    agent.wait_for_completed_turns(1);
    finish(agent);
    all_laws(&fixture, &[id], "finished child", &script);
}

fn exported_and_imported() -> (Fixture, Value, String, String) {
    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    agent.send(&prompt_frame("exported prompt"));
    agent.wait_for_completed_turns(1);
    finish(agent);
    let export = fixture.root.join("corpus-export.json");
    let output = fixture
        .command()
        .args(["session", "export", &source, "--out"])
        .arg(&export)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = fixture
        .command()
        .args(["session", "import"])
        .arg(&export)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let imported = stdout
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plak --resume "))
        .unwrap()
        .to_owned();
    assert_ne!(source, imported);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    assert_ne!(
        store.load_intents(&imported).unwrap().notices,
        [] as [plakous_hosted::sessions::projector::OperatorNotice; 0]
    );
    (fixture, script, source, imported)
}

#[test]
fn composite_export_import_obeys_all_three_sqlite_laws() {
    let (fixture, script, source, imported) = exported_and_imported();
    all_laws(&fixture, &[source, imported], "imported archive", &script);
}

#[test]
fn composite_source_deleted_import_obeys_all_three_sqlite_laws() {
    let (fixture, script, source, imported) = exported_and_imported();
    let output = fixture
        .command()
        .args(["session", "delete", &source, "--yes"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    assert!(store.load(&source).is_err());
    let mut product = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
    product.delete_views(&imported).unwrap();
    drop(product);
    // Pin reader sidecars so SQLite cannot create/remove an empty WAL between
    // the byte snapshots; any change to durable content still fails the test.
    let paths = summary::paths(&fixture);
    let readers = [
        paths.runtime_database(),
        paths.product_database(),
        paths.subagents_database(),
    ]
    .map(|path| {
        let reader =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        reader
            .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
            .unwrap();
        reader
    });
    let before = summary::database_bytes(&fixture);
    let output = fixture
        .command()
        .args(["session", "rehearse", &imported])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        summary::database_bytes(&fixture) == before,
        "rehearsal wrote durable databases"
    );
    drop(readers);
    all_laws(
        &fixture,
        &[imported],
        "imported archive without its source",
        &script,
    );
}

#[test]
fn composite_rebind_both_sides_obey_all_three_sqlite_laws() {
    use plakous::sessions::{RebindSessionOutcome, RebindSessionRequest, SessionOperationId};
    use plakous_hosted::sessions::{
        RebindSelection, SessionWorkflowKind, projector::WorkflowRequest,
    };

    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    agent.send(&prompt_frame("before rebind"));
    agent.wait_for_completed_turns(1);
    finish(agent);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    let mut record = store.load(&source).unwrap();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture.root.join("rebind-evidence.jsonl"),
    )
    .unwrap();
    // JSONL has no profile-fork command. Reuse its exact scripted native
    // binding and submit the same keyed request the product rebind authors.
    let mut target_script = script.clone();
    target_script["maximum_projected_input"] = json!(30_000);
    let runtime = plakous_offline_crash_harness::scripted_runtime_binding(
        &fixture.workspace(),
        controller,
        Arc::new(serde_json::from_value(target_script.clone()).unwrap()),
    )
    .unwrap();
    let binding = runtime.target_binding();
    let request = WorkflowRequest {
        operation_id: format!("plak:rebind:{}", uuid::Uuid::new_v4()),
        source_locator: record.plakous_locator.unwrap(),
        kind: SessionWorkflowKind::Rebind(Box::new(RebindSelection {
            provider: "codex".into(),
            account: "codex".into(),
            model: "offline-loopback-v1".into(),
            effort: Some("medium".into()),
            composition: plak::product_config::ProductCompositionSelection::FollowCatalog,
            target_selection: plak::product_config::ProductTargetSelection::Native,
            protocol: binding.protocol().as_str().into(),
            route: binding.route().as_str().into(),
            requested_model: binding.model().as_str().into(),
            selected_configuration: serde_json::to_string(
                &binding.selected_provider_configuration(),
            )
            .unwrap(),
            effective_configuration: serde_json::to_string(
                &binding.effective_provider_configuration(),
            )
            .unwrap(),
            account_binding: serde_json::to_string(&binding.account_binding()).unwrap(),
        })),
    };
    store
        .submit_workflow_request(&mut record, &request)
        .unwrap();
    let sessions = runtime_sessions(&fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let outcome = executor
        .block_on(sessions.rebind(RebindSessionRequest::recompiling_target(
            SessionOperationId::new(&request.operation_id).unwrap(),
            request.source_locator,
            binding,
            runtime,
        )))
        .unwrap()
        .unwrap();
    let RebindSessionOutcome::Rebound { target, .. } = outcome else {
        panic!("rebind did not commit: {outcome:?}")
    };
    assert_ne!(target.branch_id(), request.source_locator.branch_id());
    project(&fixture, std::slice::from_ref(&source));
    let successor = request.successor_product_id();
    assert_eq!(
        store
            .load(&successor)
            .unwrap()
            .source_product_session_id
            .as_deref(),
        Some(source.as_str())
    );
    all_laws(&fixture, &[source], "rebind source", &script);
    all_laws(&fixture, &[successor], "rebind successor", &target_script);
}

#[test]
fn clean_quit_projects_the_release_record_without_a_harness_pass() {
    let (fixture, id) = setup("final_only");
    let paths = summary::paths(&fixture);
    let store = SqliteSessionStore::new(paths.clone());
    let record = store.load(&id).unwrap();
    let views = store.load_views(&id).unwrap().unwrap();
    let sessions = plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(paths.runtime_database()).unwrap(),
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let page = executor
        .block_on(sessions.feed_page(FeedPageRequest::after(FeedCursor::new(
            record.plakous_locator.unwrap().session_id(),
            views.cursor,
        ))))
        .unwrap()
        .unwrap();
    assert!(
        matches!(page, FeedPage::Records { records, tail, .. }
            if records.is_empty() && tail == views.cursor),
        "clean quit left views behind the release commit"
    );
    assert!(views.state.released);
}

#[test]
fn post_release_timeout_is_benign_and_plain_open_preserves_all_three_sqlite_laws() {
    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, id) = ready(&fixture);
    agent.send(&prompt_frame("healthy store with delayed projection"));
    agent.wait_for_completed_turns(1);
    let terminal_start = agent
        .snapshot()
        .iter()
        .rposition(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|row| row["kind"] == "assistant_done")
        })
        .unwrap()
        + 1;
    agent.wait_for("host idle after the prompt", |row| {
        row["kind"] == "status"
            && row["phase"] == "idle"
            && agent.snapshot().iter().skip(terminal_start).any(|line| {
                serde_json::from_str::<Value>(line)
                    .is_ok_and(|row| row["kind"] == "status" && row["phase"] == "idle")
            })
    });
    project(&fixture, std::slice::from_ref(&id));
    let paths = summary::paths(&fixture);
    let before = view_snapshot::snapshot(&paths);
    let store = SqliteSessionStore::new(paths.clone());
    let views = store.load_views(&id).unwrap().unwrap();
    assert!(!views.state.released);

    // WAL readers remain available, but the host cannot commit views until
    // this healthy writer releases its lock, after the host has quit.
    let mut writer = rusqlite::Connection::open(paths.product_database()).unwrap();
    let lock = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    agent.send(&quit_frame());
    agent.wait_for("quit before transport EOF", |row| row["kind"] == "bye");
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(view_snapshot::snapshot(&paths), before);
    assert!(summary::log_rows(&fixture).iter().any(|row| {
        row["kind"] == "projector.deferred"
            && row["error"] == "the post-release projector pass exceeded its bound"
    }));
    let record = store.load(&id).unwrap();
    let sessions = runtime_sessions(&fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let page = executor
        .block_on(sessions.feed_page(FeedPageRequest::after(FeedCursor::new(
            record.plakous_locator.unwrap().session_id(),
            views.cursor,
        ))))
        .unwrap()
        .unwrap();
    assert!(matches!(page, FeedPage::Records { records, tail, .. }
        if tail > views.cursor && records.iter().any(|record|
            matches!(record.header().change, plakous::sessions::feed::FeedChange::LeaseReleased { .. }))));
    lock.rollback().unwrap();
    drop(writer);

    let mut command = fixture.command();
    command.args(["agent", "--session", &id]);
    let opened = AgentProcess::start_command(command, true);
    opened.wait_for("plain open pays timeout debt", |row| row["kind"] == "ready");
    assert_at_tail(&fixture, &id);
    finish(opened);
    all_laws(&fixture, &[id], "post-release timeout", &script);
}

fn publish_fork(fixture: &Fixture, source: &str) -> String {
    use plakous::sessions::{ForkSessionOutcome, ForkSessionRequest, SessionOperationId};
    use plakous_hosted::sessions::{SessionWorkflowKind, projector::WorkflowRequest};
    let store = SqliteSessionStore::new(summary::paths(fixture));
    let mut record = store.load(source).unwrap();
    let request = WorkflowRequest {
        operation_id: format!("plak:fork:{}", uuid::Uuid::new_v4()),
        source_locator: record.plakous_locator.unwrap(),
        kind: SessionWorkflowKind::Fork,
    };
    store
        .submit_workflow_request(&mut record, &request)
        .unwrap();
    let sessions = runtime_sessions(fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let outcome = executor
        .block_on(sessions.fork(ForkSessionRequest::new(
            SessionOperationId::new(&request.operation_id).unwrap(),
            request.source_locator,
        )))
        .unwrap()
        .unwrap();
    assert!(
        matches!(outcome, ForkSessionOutcome::Forked { .. }),
        "{outcome:?}"
    );
    project(fixture, &[source.to_owned()]);
    request.successor_product_id()
}

#[test]
fn composite_fork_of_fork_obeys_all_three_sqlite_laws() {
    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    agent.send(&prompt_frame("shared before two forks"));
    agent.wait_for_completed_turns(1);
    finish(agent);
    let first = publish_fork(&fixture, &source);
    let second = publish_fork(&fixture, &first);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    assert!(store.load_views(&first).unwrap().is_some());
    assert!(store.load_views(&second).unwrap().is_some());
    all_laws(&fixture, &[source, first, second], "fork of fork", &script);
}

fn record_failed_scheduled_compaction(fixture: &Fixture, source: &str, script: &Value) {
    use plakous::sessions::{
        CompactReason, CompactSessionOutcome, CompactSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionOperationId,
    };
    use plakous_hosted::sessions::{
        ProductCompactReason, SessionWorkflowKind, projector::WorkflowRequest,
    };
    let store = SqliteSessionStore::new(summary::paths(fixture));
    let mut record = store.load(source).unwrap();
    let request = WorkflowRequest {
        operation_id: format!("plak:compact:{}", uuid::Uuid::new_v4()),
        source_locator: record.plakous_locator.unwrap(),
        kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
    };
    store
        .submit_workflow_request(&mut record, &request)
        .unwrap();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture.root.join("scheduled-fork-evidence.jsonl"),
    )
    .unwrap();
    let plan = Arc::new(serde_json::from_value(script.clone()).unwrap());
    let normal = plakous_offline_crash_harness::scripted_runtime_binding(
        &fixture.workspace(),
        controller.clone(),
        Arc::clone(&plan),
    )
    .unwrap();
    let runtime = plakous_offline_crash_harness::scripted_compaction_runtime_binding(
        &fixture.workspace(),
        controller,
        &plan,
        None,
        None,
        Vec::new(),
    )
    .unwrap();
    let sessions = runtime_sessions(fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let opened = executor
        .block_on(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("open:scheduled-fork").unwrap(),
            request.source_locator,
            normal,
        )))
        .unwrap()
        .unwrap();
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        panic!("open: {opened:?}")
    };
    let outcome = executor
        .block_on(
            sessions.compact(
                CompactSessionRequest::new(
                    SessionOperationId::new(&request.operation_id).unwrap(),
                    request.source_locator,
                    CompactReason::Scheduled,
                    runtime,
                )
                .with_pending_user_input("pending protected prompt ".repeat(200).into()),
            ),
        )
        .unwrap()
        .unwrap();
    assert!(
        matches!(outcome, CompactSessionOutcome::Failed { .. }),
        "{outcome:?}"
    );
    executor.block_on(session.release()).unwrap().unwrap();
    project(fixture, &[source.to_owned()]);
}

fn scheduled_compact_then_fork(turn_after: bool) {
    let mut script = text_script();
    script["scheduled_compaction"] = json!(true);
    script["fold"] = json!("fail");
    script["steps"][0]["output"]["text"] = json!("eligible history ".repeat(500));
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    for (index, prompt) in ["eligible earlier intent", "protected latest intent"]
        .iter()
        .enumerate()
    {
        agent.send(&prompt_frame(prompt));
        agent.wait_for_completed_turns(index + 1);
    }
    finish(agent);
    record_failed_scheduled_compaction(&fixture, &source, &script);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    if turn_after {
        let mut command = fixture.command();
        command.args(["agent", "--session", &source]);
        let mut agent = AgentProcess::start_command(command, true);
        agent.wait_for("ready", |row| row["kind"] == "ready");
        agent.send(&prompt_frame(
            "a turn clears the scheduled-compaction pause",
        ));
        agent.wait_for_completed_turns(1);
        finish(agent);
    }
    let target = publish_fork(&fixture, &source);
    assert_eq!(
        store.load_views(&target).unwrap().is_some(),
        turn_after,
        "only the source with a later turn is seed-eligible"
    );
    all_laws(
        &fixture,
        &[source, target],
        "scheduled compact then fork",
        &script,
    );
}

#[test]
fn composite_scheduled_compaction_then_turn_then_fork_uses_seed_and_obeys_all_laws() {
    scheduled_compact_then_fork(true);
}

#[test]
fn composite_scheduled_compaction_then_fork_rebuilds_and_obeys_all_laws() {
    scheduled_compact_then_fork(false);
}
