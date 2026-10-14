//! Retained successor laws over complete SQL cells and one real host open.

use super::*;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};
use std::sync::Arc;

fn runtime_sessions(fixture: &Fixture) -> plakous::sessions::AgentSessions {
    plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(summary::paths(fixture).runtime_database())
            .unwrap(),
    )
    .unwrap()
}

#[track_caller]
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
            "{id}: pending workflows {:?}; locator {:?}; views {:?}; intents {:?}",
            projected.pending_workflows,
            record.plakous_locator,
            store.load_views(id).unwrap().unwrap().state,
            store.load_intents(id).unwrap().workflow_requests,
        );
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

fn rebind(fixture: &Fixture, source: &str, script: &Value) -> (String, Value) {
    use plakous::sessions::{RebindSessionOutcome, RebindSessionRequest, SessionOperationId};
    use plakous_hosted::sessions::{
        RebindSelection, SessionWorkflowKind, projector::WorkflowRequest,
    };

    let store = SqliteSessionStore::new(summary::paths(fixture));
    let mut record = store.load(source).unwrap();
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
    let sessions = runtime_sessions(fixture);
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
    project(fixture, &[source.to_owned()]);
    let successor = request.successor_product_id();
    assert_eq!(
        store
            .load(&successor)
            .unwrap()
            .source_product_session_id
            .as_deref(),
        Some(source)
    );
    (successor, target_script)
}

fn reopen_prompt(fixture: &Fixture, id: &str, prompt: &str) {
    let mut command = fixture.command();
    command.args(["agent", "--session", id]);
    let mut agent = AgentProcess::start_command(command, true);
    agent.wait_for("reopened ready", |row| row["kind"] == "ready");
    agent.send(&prompt_frame(prompt));
    agent.wait_for_completed_turns(1);
    finish(agent);
}

#[test]
fn retained_fork_and_clear_obey_complete_sqlite_rebuild_and_open_laws() {
    retained_successor_laws(false);
}

#[test]
fn retained_rebind_and_clear_obey_complete_sqlite_rebuild_and_open_laws() {
    retained_successor_laws(true);
}

#[test]
fn deleted_successor_is_not_resurrected_by_source_rebuild() {
    let fixture = Fixture::new(&text_script());
    let (mut agent, source) = ready(&fixture);
    agent.send(&json!({"v":1,"kind":"fork","id":"fork"}));
    agent.wait_for("fork committed", |row| {
        row["kind"] == "status"
            && row["id"] == "fork"
            && row["detail"]
                .as_str()
                .is_some_and(|s| s.starts_with("forked session"))
    });
    finish(agent);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    let successor =
        store.load_intents(&source).unwrap().workflow_requests[0].successor_product_id();
    let target = store.load(&successor).unwrap();
    let mut product = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
    product
        .delete_session(&successor, target.store_revision)
        .unwrap();
    product.delete_views(&source).unwrap();
    drop(product);
    project(&fixture, &[source]);
    assert!(
        store.load(&successor).is_err(),
        "deleted successor resurrected"
    );
    assert!(store.load_views(&successor).unwrap().is_none());
}

#[test]
fn refused_successor_projection_keeps_live_fork_source_promptable() {
    let fixture = Fixture::new(&text_script());
    let (mut agent, source) = ready(&fixture);
    let connection =
        rusqlite::Connection::open(summary::paths(&fixture).product_database()).unwrap();
    connection.execute_batch(&format!(
        "CREATE TRIGGER refuse_target_views BEFORE INSERT ON session_views WHEN NEW.session_id <> '{source}' BEGIN SELECT RAISE(ABORT, 'target projection cut'); END;"
    )).unwrap();
    agent.send(&json!({"v":1,"kind":"fork","id":"fork"}));
    let answer = agent.wait_for("fork projection refused", |row| {
        row["id"] == "fork" && (row["kind"] == "error" || row["kind"] == "status")
    });
    let detail = answer["detail"]
        .as_str()
        .or_else(|| answer["message"].as_str())
        .unwrap();
    assert!(detail.contains("this session stays attached"), "{answer}");
    connection
        .execute_batch("DROP TRIGGER refuse_target_views")
        .unwrap();
    drop(connection);
    agent.send(&prompt_frame("source after refused target projection"));
    agent.wait_for_completed_turns(1);
    finish(agent);
    let store = SqliteSessionStore::new(summary::paths(&fixture));
    let successor =
        store.load_intents(&source).unwrap().workflow_requests[0].successor_product_id();
    assert!(
        store.load(&successor).is_ok(),
        "committed fork remains resumable"
    );
    project(&fixture, &[source, successor]);
}

#[test]
fn unpublished_runtime_fork_projects_its_own_prefix_after_source_advances() {
    use plakous::sessions::{ForkSessionOutcome, ForkSessionRequest, SessionOperationId};
    use plakous_hosted::sessions::{
        SessionWorkflowKind, TranscriptRow, projector::WorkflowRequest,
    };

    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    agent.send(&prompt_frame("shared before branch"));
    agent.wait_for_completed_turns(1);
    finish(agent);
    let store: Arc<dyn SessionRecordStore> =
        Arc::new(SqliteSessionStore::new(summary::paths(&fixture)));
    let mut record = store.load(&source).unwrap();
    let request = WorkflowRequest {
        operation_id: format!("plak:fork:{}", uuid::Uuid::new_v4()),
        source_locator: record.plakous_locator.unwrap(),
        kind: SessionWorkflowKind::Fork,
    };
    store
        .submit_workflow_request(&mut record, &request)
        .unwrap();
    let sessions = runtime_sessions(&fixture);
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let outcome = executor
        .block_on(sessions.fork(ForkSessionRequest::new(
            SessionOperationId::new(&request.operation_id).unwrap(),
            request.source_locator,
        )))
        .unwrap()
        .unwrap();
    let ForkSessionOutcome::Forked { target, .. } = outcome else {
        panic!("fork: {outcome:?}")
    };
    let successor = request.successor_product_id();
    let paths = summary::paths(&fixture);
    let connection = rusqlite::Connection::open(paths.product_database()).unwrap();
    connection.execute_batch(&format!(
        "CREATE TRIGGER refuse_successor BEFORE INSERT ON sessions WHEN NEW.id='{successor}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
    )).unwrap();
    assert!(
        executor
            .block_on(plakous_hosted::runtime::project_session(
                &sessions, &store, &record
            ))
            .unwrap()
            .is_err()
    );
    assert!(store.load(&successor).is_err());
    connection
        .execute_batch("DROP TRIGGER refuse_successor")
        .unwrap();
    drop(connection);

    advance_source_without_product(&fixture, &sessions, request.source_locator, &script);
    let mut connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
    let tx = connection.transaction().unwrap();
    tx.execute("DELETE FROM session_feed WHERE seq < 2", [])
        .unwrap();
    tx.execute("UPDATE sessions SET feed_retained_from=2,feed_bytes=(SELECT COALESCE(SUM(length(record)),0) FROM session_feed WHERE session_id=sessions.session_id)", []).unwrap();
    tx.commit().unwrap();
    drop(connection);
    let mut product = plak::store::ProductStore::open(&paths).unwrap();
    product.delete_views(&source).unwrap();
    drop(product);
    project(&fixture, &[source]);
    let published = store.load(&successor).unwrap();
    assert_eq!(published.plakous_locator, Some(target));
    assert!(
        store.load_views(&successor).unwrap().is_none(),
        "source published cloned views"
    );
    project(&fixture, std::slice::from_ref(&successor));
    let views = store.load_views(&successor).unwrap().unwrap();
    let users = views
        .rows
        .values()
        .filter_map(|row| match row {
            TranscriptRow::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users, ["shared before branch"]);
    assert_eq!(views.state.history_through, None);
    let live = view_snapshot::snapshot(&paths);
    let mut product = plak::store::ProductStore::open(&paths).unwrap();
    product.delete_views(&successor).unwrap();
    drop(product);
    project(&fixture, std::slice::from_ref(&successor));
    assert_eq!(view_snapshot::snapshot(&paths), live);
    assert_plain_open_twin(&fixture, &successor, &script);
}

// Advance canonical source work without any product projection or host loop.
fn advance_source_without_product(
    fixture: &Fixture,
    sessions: &plakous::sessions::AgentSessions,
    locator: plakous::sessions::SessionLocator,
    script: &Value,
) {
    use plakous::sessions::{OpenSessionOutcome, OpenSessionRequest, SessionOperationId};
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture.root.join("source-advance.jsonl"),
    )
    .unwrap();
    let runtime = plakous_offline_crash_harness::scripted_runtime_binding(
        &fixture.workspace(),
        controller,
        Arc::new(serde_json::from_value(script.clone()).unwrap()),
    )
    .unwrap();
    executor
        .block_on(async {
            let opened = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("open:source-without-product").unwrap(),
                    locator,
                    runtime,
                ))
                .await
                .unwrap();
            let OpenSessionOutcome::Opened { session, .. } = opened else {
                panic!("source open: {opened:?}")
            };
            session
                .agent()
                .prompt("source only after branch")
                .await
                .unwrap()
                .result()
                .await
                .unwrap();
            session.release().await.unwrap();
        })
        .unwrap();
}

fn retained_successor_laws(is_rebind: bool) {
    let script = text_script();
    let fixture = Fixture::new(&script);
    let (mut agent, source) = ready(&fixture);
    agent.send(&prompt_frame("shared before branch"));
    agent.wait_for_completed_turns(1);
    let (successor, successor_script) = if is_rebind {
        finish(agent);
        rebind(&fixture, &source, &script)
    } else {
        agent.send(&json!({"v":1,"kind":"fork","id":"fork"}));
        agent.wait_for("fork committed", |row| {
            row["kind"] == "status"
                && row["id"] == "fork"
                && row["detail"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("forked session"))
        });
        finish(agent);
        let store = SqliteSessionStore::new(summary::paths(&fixture));
        let successor = store
            .load_intents(&source)
            .unwrap()
            .workflow_requests
            .iter()
            .find(|r| matches!(r.kind, plakous_hosted::sessions::SessionWorkflowKind::Fork))
            .unwrap()
            .successor_product_id();
        (successor, script.clone())
    };
    reopen_prompt(&fixture, &source, "source only after branch");
    let successor_fixture = fixture.fork(&successor_script);
    let mut command = successor_fixture.command();
    command.args(["agent", "--session", &successor]);
    let mut agent = AgentProcess::start_command(command, true);
    agent.wait_for("successor ready", |row| row["kind"] == "ready");
    agent.send(&prompt_frame("successor only after branch"));
    agent.wait_for_completed_turns(1);
    agent.send(&json!({"v":1,"kind":"clear","id":"clear"}));
    agent.wait_for("clear committed", |row| {
        row["kind"] == "status"
            && row["id"] == "clear"
            && row["detail"]
                .as_str()
                .is_some_and(|s| s.starts_with("new session"))
    });
    agent.send(&prompt_frame("clear only"));
    agent.wait_for_completed_turns(2);
    finish(agent);
    let store = SqliteSessionStore::new(summary::paths(&successor_fixture));
    let clear = store
        .load_intents(&successor)
        .unwrap()
        .workflow_requests
        .iter()
        .find(|r| matches!(r.kind, plakous_hosted::sessions::SessionWorkflowKind::Clear))
        .unwrap()
        .successor_product_id();
    let ids = [source, successor, clear];
    project(&successor_fixture, &ids);
    let paths = summary::paths(&successor_fixture);
    let mut connection = rusqlite::Connection::open(paths.runtime_database()).unwrap();
    let tx = connection.transaction().unwrap();
    // Prune genesis only: every Branched record and terminal stays retained.
    tx.execute("DELETE FROM session_feed WHERE seq < 2", [])
        .unwrap();
    tx.execute("UPDATE sessions SET feed_retained_from=2,feed_bytes=(SELECT COALESCE(SUM(length(record)),0) FROM session_feed WHERE session_id=sessions.session_id)", []).unwrap();
    tx.commit().unwrap();
    drop(connection);
    // An incremental pass with its original branch lineage must fall behind too.
    let connection = rusqlite::Connection::open(paths.product_database()).unwrap();
    connection
        .execute("UPDATE projector_cursors SET applied_seq=0", [])
        .unwrap();
    drop(connection);
    project(&successor_fixture, &ids);
    let live = view_snapshot::snapshot(&paths);
    assert_own_prose(&store, &ids);
    let mut product = plak::store::ProductStore::open(&paths).unwrap();
    for id in &ids {
        product.delete_views(id).unwrap();
    }
    drop(product);
    project(&successor_fixture, &ids);
    assert_own_prose(&store, &ids);
    assert_eq!(
        view_snapshot::snapshot(&paths),
        live,
        "retained composite: complete SQL-cell rebuild"
    );
    for (id, script) in [
        (&ids[0], &script),
        (&ids[1], &successor_script),
        (&ids[2], &successor_script),
    ] {
        assert_plain_open_twin(&successor_fixture, id, script);
    }
}

fn assert_own_prose(store: &SqliteSessionStore, ids: &[String; 3]) {
    use plakous_hosted::sessions::TranscriptRow;
    for (index, expected) in [
        "source only after branch",
        "successor only after branch",
        "clear only",
    ]
    .iter()
    .enumerate()
    {
        let views = store.load_views(&ids[index]).unwrap().unwrap();
        let users = views
            .rows
            .values()
            .filter_map(|row| match row {
                TranscriptRow::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            users.iter().filter(|text| **text == *expected).count(),
            1,
            "own head prose: {users:?}"
        );
        for foreign in [
            "source only after branch",
            "successor only after branch",
            "clear only",
        ] {
            if foreign != *expected {
                assert!(!users.contains(&foreign), "foreign branch prose: {users:?}");
            }
        }
        assert_eq!(views.state.history_through, None);
    }
}

fn assert_plain_open_twin(fixture: &Fixture, id: &str, script: &Value) {
    let intact = fixture.fork(script);
    let deleted = fixture.fork(script);
    let mut product = plak::store::ProductStore::open(&summary::paths(&deleted)).unwrap();
    product.delete_views(id).unwrap();
    drop(product);
    let mut snapshots = Vec::new();
    for arm in [&intact, &deleted] {
        let mut command = arm.command();
        command.args(["agent", "--session", id]);
        let agent = AgentProcess::start_command(command, true);
        agent.wait_for("one real open ready", |row| row["kind"] == "ready");
        snapshots.push(view_snapshot::snapshot(&summary::paths(arm)));
        finish(agent);
    }
    assert_eq!(
        snapshots[0], snapshots[1],
        "one real open retained twin: {id}"
    );
}
