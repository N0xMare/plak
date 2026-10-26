use super::*;
use crate::CliFailure;
use crate::carry::document::private_dir;
use crate::carry::interactive::write_private_new;

#[test]
fn only_an_exact_private_local_handoff_receipt_retains_replay() {
    let home = tempfile::tempdir().unwrap();
    let carry = private_dir(&home.path().join("carry")).unwrap();
    let path = carry.join("session.json");
    write_private_new(&path, b"exported bytes").unwrap();
    let digest = plakous::model::sha256_hex(&[b"exported bytes"]);
    assert!(!consume_handoff_receipt(&path, home.path(), Some(&digest)));
    write_private_new(&path.with_extension("handoff"), digest.as_bytes()).unwrap();
    assert!(!consume_handoff_receipt(
        &path,
        home.path(),
        Some("tampered")
    ));
    assert!(!path.with_extension("handoff").exists());
    write_private_new(&path.with_extension("handoff"), digest.as_bytes()).unwrap();
    assert!(consume_handoff_receipt(&path, home.path(), Some(&digest)));
    assert!(!consume_handoff_receipt(&path, home.path(), Some(&digest)));
    let foreign = tempfile::tempdir().unwrap();
    assert!(!consume_handoff_receipt(
        &path,
        foreign.path(),
        Some(&digest)
    ));
}

#[test]
fn import_resume_post_commit_failure_names_the_successor_and_recovery_command() {
    let failure = import_resume_failure(
        CliFailure::new("synthetic TUI failure"),
        "new-session",
        "old-session",
    );
    assert_eq!(
        failure.message(),
        "synthetic TUI failure; session new-session was carried forward from old-session; fix the error and run: plak --resume new-session"
    );
}

use crate::carry::document::{read_document, sample_document};
use crate::home::paths::PlakPaths;
use crate::store::sessions::QueueDrainState;

#[test]
fn v9_import_intents_survive_reopen_and_rebuild() {
    assert_import_intents_rebuild();
}

fn assert_import_intents_rebuild() {
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
    let mut value = serde_json::to_value(sample_document()).unwrap();
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
