use crate::carry::CarriedImport;
use crate::carry::document::{VERSION, read_document, sample_document};
use crate::home::paths::PlakPaths;
use crate::store::sessions::QueueDrainState;
use plakous::model::{Actor, ContentBlock, ConversationItem, HistorySegmentKind, Provenance};
use plakous_hosted::sessions::TranscriptRow;
use std::fs;

/// Every stored row type survives an export and its import unchanged.
#[test]
fn previous_document_preserves_every_transcript_row_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    let mut document = sample_document();
    document.transcript = crate::store::every_stored_row_type();
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let imported = read_document(&path).unwrap();
    assert_eq!(imported.version, 7);
    assert_eq!(imported.transcript, crate::store::every_stored_row_type());
}

#[test]
fn previous_document_carries_typed_rows_and_refuses_v5() {
    assert_eq!(VERSION, 8);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    let mut value = serde_json::to_value(sample_document()).unwrap();
    assert_eq!(
        value["transcript"],
        serde_json::json!([{"kind": "user", "text": "first"}])
    );
    value["version"] = serde_json::json!(5);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        read_document(&path)
            .unwrap_err()
            .to_string()
            .contains("version 5")
    );
}

#[test]
fn previous_export_fixture_imports_as_the_rows_it_carried() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    fs::write(
        &path,
        include_str!("../../../tests/fixtures/session-export-v7.json"),
    )
    .unwrap();
    let document = read_document(&path).unwrap();
    assert_eq!(document.version, 7);
    assert_eq!(document.source_resolved_count, Some(3));
    assert_eq!(document.transcript[0], TranscriptRow::user("first prompt"));
    // An absent exposed name renders as the canonical tool name.
    assert!(matches!(
        &document.transcript[1],
        TranscriptRow::Tool(tool) if tool.name == "loopback_effect"
            && tool.exposed_name.is_none()
            && tool.label() == "loopback_effect"
            && tool.state == "succeeded"
    ));
    assert!(
        document
            .transcript
            .contains(&TranscriptRow::assistant("first answer"))
    );
    let exported = serde_json::to_value(document).unwrap();
    assert_eq!(exported["version"], 7);
    assert_eq!(
        exported["transcript"][0],
        serde_json::json!({"kind": "user", "text": "first prompt"})
    );
}

/// A version 7 tool row whose exposed name equals its canonical name
/// imports with the exposed name absent.
#[test]
fn a_version_7_redundant_exposed_name_imports_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    let row = serde_json::json!({
        "kind": "tool", "call_id": "c1", "name": "apply_patch",
        "exposed_name": "apply_patch", "activity_kind": "tool", "state": "succeeded"
    });
    let mut value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/session-export-v7.json"
    ))
    .unwrap();
    value["version"] = serde_json::json!(7);
    value["transcript"] = serde_json::json!([row]);
    fs::write(&path, value.to_string()).unwrap();
    let document = read_document(&path).unwrap();
    let [TranscriptRow::Tool(tool)] = document.transcript.as_slice() else {
        panic!("one tool row: {:?}", document.transcript);
    };
    assert_eq!(tool.exposed_name, None);
    assert_eq!(tool.label(), "apply_patch");
    let CarriedImport { record, .. } = document.into_import().unwrap();
    assert!(matches!(
        record.transcript.to_vec().as_slice(),
        [TranscriptRow::Tool(tool)] if tool.exposed_name.is_none()
    ));
}

#[test]
fn previous_document_carries_send_times_and_process_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    let mut document = sample_document();
    document.transcript = vec![
        TranscriptRow::User {
            text: "first".into(),
            sent_at_ms: Some(1_791_383_520_000),
        },
        TranscriptRow::Process(plakous_host_protocol::views::transcript::ProcessNotice {
            job_id: "p261".into(),
            outcome: plakous_host_protocol::views::transcript::ProcessOutcome::Exited {
                code: Some(0),
            },
            duration_ms: Some(1_300_000),
            subject: Some("just check".into()),
        }),
    ];
    let value = serde_json::to_value(&document).unwrap();
    assert_eq!(value["transcript"][0]["sent_at_ms"], 1_791_383_520_000_u64);
    assert_eq!(value["transcript"][1]["subject"], "just check");
    fs::write(&path, value.to_string()).unwrap();
    assert_eq!(
        read_document(&path).unwrap().transcript,
        document.transcript
    );
}

#[test]
fn previous_document_rejects_resolved_count_pseudo_work() {
    let mut value = serde_json::to_value(sample_document()).unwrap();
    value["pending_work"] = serde_json::json!([{
        "operation": "resolved_source_work", "kind": "source_resolved_count",
        "state": "3", "detail": "settled child/candidate/workspace rows omitted"
    }]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read_document(&path).is_err());
}

#[test]
fn golden_v7_document_imports_with_separate_reference_provenance() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v7.json");
    fs::write(
        &path,
        include_str!("../../../tests/fixtures/session-export-v7.json"),
    )
    .unwrap();
    let document = read_document(&path).unwrap();
    assert_eq!(document.version, 7);
    assert_eq!(document.source_resolved_count, Some(3));
    let imported = document.into_import().unwrap();
    let segments = imported.history.segments();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].items()[0].actor(), Actor::User);
    assert_eq!(segments[1].items()[0].provenance(), Provenance::Imported);
    assert_eq!(
        readable_item_text(&segments[0].items()[0]),
        "Original user goal:\nfirst prompt\n\nRecent user goals or requests (newest first):\n- first prompt"
    );
    assert!(readable_item_text(&segments[1].items()[0]).contains("first answer"));
}

#[test]
fn v7_import_intents_survive_reopen_and_rebuild() {
    assert_import_intents_rebuild(7);
}

fn assert_import_intents_rebuild(version: u32) {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_hosted::session_store::SessionRecordStore;
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let mut value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/session-export-v7.json"
    ))
    .unwrap();
    value["version"] = serde_json::json!(version);
    value["workspace_root"] = serde_json::json!(root.path());
    value["queue"] = serde_json::json!(["carried queue stays paused"]);
    // A carried truncation marker is an intent too, not a disposable view.
    value["transcript"]
        .as_array_mut()
        .unwrap()
        .insert(0, serde_json::json!({"kind": "truncated"}));
    let path = root.path().join("document.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let CarriedImport {
        mut record,
        history,
        ..
    } = read_document(&path).unwrap().into_import().unwrap();
    let expected = record.transcript.to_vec();
    let make_binding = || {
        text_runtime_binding(
            root.path(),
            BoundaryController::quiet_from_evidence_path(
                CrashBoundary::None,
                root.path().join("evidence"),
            )
            .unwrap(),
            "synthetic",
            "synthetic",
        )
        .unwrap()
    };
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let CreateSessionOutcome::Created { session, .. } = executor
        .block_on(
            sessions.create(
                CreateSessionRequest::new(
                    SessionOperationId::new(&record.create_operation_id).unwrap(),
                    make_binding(),
                )
                .with_history(history),
            ),
        )
        .unwrap()
        .unwrap()
    else {
        panic!("new import required");
    };
    record.plakous_locator = Some(executor.block_on(session.release()).unwrap().unwrap());
    let store: Arc<dyn SessionRecordStore> = Arc::new(
        crate::store::session_records::SqliteSessionStore::new(paths.clone()),
    );
    store.save(&mut record).unwrap();
    let projection = executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &store, &record,
        ))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(projection.pending_commands, [] as [String; 0]);
    assert_eq!(projection.pending_workflows, [] as [String; 0]);
    assert_eq!(
        store.load(&record.id).unwrap().transcript.to_vec(),
        expected
    );
    let OpenSessionOutcome::Opened { session, .. } = executor
        .block_on(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("import:reopen").unwrap(),
            projection.head,
            make_binding(),
        )))
        .unwrap()
        .unwrap()
    else {
        panic!("import must reopen");
    };
    executor.block_on(session.release()).unwrap().unwrap();
    executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &store, &record,
        ))
        .unwrap()
        .unwrap();
    let mut database = crate::store::ProductStore::open(&paths).unwrap();
    let intents = database.load_intents(&record.id).unwrap();
    let incremental = database.load_views(&record.id).unwrap().unwrap();
    assert_eq!(
        incremental
            .transcript(&intents)
            .into_iter()
            .map(|(_, row)| row)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(record.queue_drain_state, QueueDrainState::RestoredPaused);
    assert_eq!(
        intents.submissions,
        [] as [plakous_hosted::sessions::projector::Submission; 0]
    );
    assert_eq!(
        intents.workflow_requests,
        [] as [plakous_hosted::sessions::projector::WorkflowRequest; 0]
    );
    database.delete_views(&record.id).unwrap();
    drop(database);
    executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &store, &record,
        ))
        .unwrap()
        .unwrap();
    let mut database = crate::store::ProductStore::open(&paths).unwrap();
    assert_eq!(database.load_intents(&record.id).unwrap(), intents);
    assert_eq!(
        database.load_views(&record.id).unwrap().unwrap(),
        incremental
    );
}

fn readable_item_text(item: &ConversationItem) -> &str {
    let ContentBlock::Text(text) = &item.blocks()[0] else {
        panic!("carry item must be text");
    };
    text.text()
}

#[test]
fn previous_document_preserves_user_voice_and_reference_provenance() {
    let imported = sample_document().into_import().unwrap();
    let segments = imported.history.segments();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].kind(), HistorySegmentKind::RecentIntent);
    assert_eq!(segments[0].items()[0].actor(), Actor::User);
    assert_eq!(segments[0].items()[0].provenance(), Provenance::UserInput);
    assert_eq!(segments[1].kind(), HistorySegmentKind::Conversation);
    assert_eq!(segments[1].items()[0].actor(), Actor::Assistant);
    assert_eq!(segments[1].items()[0].provenance(), Provenance::Imported);
}
