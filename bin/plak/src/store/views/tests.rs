#![allow(
    clippy::too_many_lines,
    reason = "Projection store scenarios assert atomic successor publication, replay identity, and prefix notice copying together."
)]

use std::path::{Path, PathBuf};

use plakous_hosted::sessions::projector::{
    CallView, Counters, OpenTurn, ProcessTrack, SessionViewState,
};
use plakous_hosted::sessions::{
    NoticeScope, ProductCompactReason, SessionRecord, SessionWorkflowKind,
};
use tempfile::tempdir;

use super::*;
use crate::home::paths::PlakPaths;

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

fn store_with_session(root: &Path) -> (ProductStore, String) {
    let mut store = ProductStore::open(&PlakPaths::explicit(root.join("p"))).unwrap();
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    store.commit(&mut session).unwrap();
    (store, session.id)
}

#[test]
fn queue_command_join_does_not_decode_transcript_rows() {
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let views = views_at(1, &[]);
    store
        .commit_projection_views(
            &id,
            &ViewWrites::between(&SessionViews::empty(), &views),
            &views.state,
        )
        .unwrap();
    let expected = store.load_queue_commands(&id).unwrap();
    store.connection.execute("INSERT INTO transcript_rows(session_id,anchor,lane,ordinal,row) VALUES(?1,1,0,0,'{\"kind\":\"invalid\"}')", [&id]).unwrap();
    assert!(store.load_views(&id).is_err());
    assert_eq!(store.load_queue_commands(&id).unwrap(), expected);
}

#[test]
fn inherited_submissions_do_not_hydrate_ancestor_transcripts_or_other_intents() {
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let mut source = store.load(&id).unwrap();
    let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
    source.bind_plakous_locator(locator).unwrap();
    store.commit(&mut source).unwrap();
    let mut child = SessionRecord::new(source.workspace_root.clone());
    child.bind_plakous_locator(locator).unwrap();
    child.source_product_session_id = Some(id.clone());
    child.source_workflow_operation_id = Some("plak:fork:narrow-read".into());
    store.commit(&mut child).unwrap();
    let views = views_at(1, &[]);
    store
        .commit_projection_views(
            &id,
            &ViewWrites::between(&SessionViews::empty(), &views),
            &views.state,
        )
        .unwrap();
    // Valid SQLite rows with invalid presentation/workflow encodings would
    // fail a full record load. An ancestry read does not need either.
    store.connection.execute("INSERT INTO transcript_rows(session_id,anchor,lane,ordinal,row) VALUES(?1,1,0,0,'{\"kind\":\"invalid\"}')", [&id]).unwrap();
    assert!(store.load(&id).is_err());
    assert_eq!(
        store.load_inherited_submissions(&child.id).unwrap(),
        [] as [Submission; 0]
    );
    store
        .connection
        .execute("DELETE FROM transcript_rows WHERE session_id=?1", [&id])
        .unwrap();
    store.connection.execute("INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,anchor) VALUES(?1,'unrelated','invalid','fork',0)", [&id]).unwrap();
    assert!(store.load(&id).is_err());
    assert_eq!(
        store.load_inherited_submissions(&child.id).unwrap(),
        [] as [Submission; 0]
    );
}

#[test]
fn a_cross_connection_writer_moves_the_projection_cursor_without_store_failure() {
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let paths = PlakPaths::explicit(root.path().join("p"));
    let mut other = ProductStore::open(&paths).unwrap();
    store
        .connection
        .busy_timeout(std::time::Duration::ZERO)
        .unwrap();
    let tx = other
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let views = views_at(1, &[(1, TranscriptRow::user("once"))]);
    let writes = ViewWrites::between(&SessionViews::empty(), &views);
    assert_eq!(
        store
            .commit_projection_views(&id, &writes, &views.state)
            .unwrap(),
        ProjectionCommit::CursorMoved
    );
    assert!(store.load_views(&id).unwrap().is_none());
    tx.rollback().unwrap();
    assert_eq!(
        other
            .commit_projection_views(&id, &writes, &views.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    assert_eq!(
        store
            .commit_projection_views(&id, &writes, &views.state)
            .unwrap(),
        ProjectionCommit::CursorMoved
    );
    assert_eq!(store.load_views(&id).unwrap(), Some(views));
    store.delete_views(&id).unwrap();
    assert!(other.load_views(&id).unwrap().is_none());
}

#[test]
fn historical_clear_and_submissions_are_consumed_without_withdrawal_after_retention() {
    assert_eq!(crate::store::SCHEMA_VERSION, 34);
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let mut source = store.load(&id).unwrap();
    let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
    source.bind_plakous_locator(locator).unwrap();
    store.commit(&mut source).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:clear:retained".into(),
        source_locator: locator,
        kind: SessionWorkflowKind::Clear,
    };
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    let submission = Submission {
        command_id: "old-root".into(),
        kind: SubmissionKind::RootStart,
        queue_item_id: "old-item".into(),
        queue_item_revision: 1,
        target_turn_id: None,
        canonical_command: "exact command".into(),
        submitted_at_ms: 42,
    };
    store.write_submission(&id, &submission).unwrap();
    // The target's creation fact is gone too. Its nonempty views prove
    // this is a historical clear, not an unadmitted empty reservation.
    let target = views_at(8, &[(8, TranscriptRow::user("target history"))]);
    store
        .commit_projection_views(
            &request.successor_product_id(),
            &ViewWrites::between(&SessionViews::empty(), &target),
            &target.state,
        )
        .unwrap();
    for deleted in [false, true] {
        if deleted {
            store.delete_views(&id).unwrap();
        }
        let previous = store
            .load_views(&id)
            .unwrap()
            .unwrap_or_else(SessionViews::empty);
        let mut retained = views_at(10, &[(10, TranscriptRow::user("head history"))]);
        retained.state.retained_from = Some(9);
        store
            .commit_projection_views(
                &id,
                &ViewWrites::between(&previous, &retained),
                &retained.state,
            )
            .unwrap();
        let views = store.load_views(&id).unwrap().unwrap();
        store.write_submission(&id, &submission).unwrap();
        store.write_workflow_request(&id, &request).unwrap();
        let intents = store.load_intents(&id).unwrap();
        assert_eq!(intents.submission_anchors[&submission.command_id], 0);
        assert_eq!(intents.workflow_anchors[&request.operation_id], 0);
        assert_eq!(
            plakous_hosted::sessions::projector::pending_command_ids(&views, &intents),
            [] as [String; 0]
        );
        assert_eq!(
            plakous_hosted::sessions::projector::pending_workflow_operation_ids(&views, &intents),
            [] as [String; 0]
        );
        assert_eq!(
            store.withdraw_unadmitted_workflows(&mut source).unwrap(),
            [] as [TranscriptRow; 0]
        );
        assert_eq!(
            store.load_intents(&id).unwrap().notices,
            [] as [OperatorNotice; 0]
        );
        assert!(store.load(&request.successor_product_id()).is_ok());
        assert!(store.load(&id).unwrap().queue_commands.is_consumed(
            &plakous_hosted::sessions::QueueItem {
                id: submission.queue_item_id.clone(),
                revision: 1,
                text: "old item".into(),
                submitted_at_ms: 42,
                images: std::sync::Arc::default(),
            }
        ));
    }
    let newer = WorkflowRequest {
        operation_id: "plak:compact:newer-than-retention".into(),
        kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
        source_locator: locator,
    };
    store.write_workflow_request(&id, &newer).unwrap();
    let intents = store.load_intents(&id).unwrap();
    assert_eq!(intents.workflow_anchors[&newer.operation_id], 10);
    assert_eq!(
        plakous_hosted::sessions::projector::pending_workflow_operation_ids(
            &store.load_views(&id).unwrap().unwrap(),
            &intents,
        ),
        vec![newer.operation_id],
    );
}

fn views_at(cursor: u64, rows: &[(u64, TranscriptRow)]) -> SessionViews {
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(cursor);
    views.state = SessionViewState {
        runtime_session_id: Some(uuid(1).parse().unwrap()),
        counters: Counters {
            root_turns: cursor,
            tool_calls: 1,
            compactions: 0,
        },
        ..SessionViewState::default()
    };
    for (anchor, row) in rows {
        views
            .rows
            .insert(RowKey::new(*anchor, Lane::Runtime, 0), row.clone());
    }
    views
}

#[test]
fn clear_intent_and_genesis_successor_are_one_atomic_cas_write() {
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let mut source = store.load(&id).unwrap();
    let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
    source.bind_plakous_locator(locator).unwrap();
    store.commit(&mut source).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:clear:atomic-intent".into(),
        source_locator: locator,
        kind: SessionWorkflowKind::Clear,
    };
    let target_id = request.successor_product_id();
    store.connection.execute_batch(&format!(
        "CREATE TEMP TRIGGER refuse_clear BEFORE INSERT ON sessions WHEN NEW.id='{target_id}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
    )).unwrap();
    let revision = source.store_revision;
    assert!(
        store
            .submit_workflow_request(&mut source, &request)
            .is_err()
    );
    assert_eq!(source.store_revision, revision);
    assert_eq!(store.load(&id).unwrap().store_revision, revision);
    assert_eq!(
        store.load_intents(&id).unwrap().workflow_requests,
        [] as [WorkflowRequest; 0]
    );
    assert!(store.load(&target_id).is_err());
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_clear")
        .unwrap();
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    let target = store.load(&target_id).unwrap();
    assert_eq!(
        target.plakous_locator,
        Some(
            plakous::sessions::AgentSessions::genesis_locator_for(
                &plakous::sessions::SessionOperationId::new(&target.create_operation_id).unwrap(),
            )
            .unwrap()
        )
    );
    assert!(target.transcript.is_empty());
    assert_eq!(target.prompt_history, [] as [String; 0]);
}

fn id_from<T: std::str::FromStr>(n: u64) -> T
where
    T::Err: std::fmt::Debug,
{
    uuid(n).parse().unwrap()
}

#[test]
fn clear_withdrawal_rolls_back_reservation_deletion_and_keeps_identity_bindings() {
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let mut source = store.load(&id).unwrap();
    let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
    source.bind_plakous_locator(locator).unwrap();
    store.commit(&mut source).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:clear:withdrawal-cut".into(),
        source_locator: locator,
        kind: SessionWorkflowKind::Clear,
    };
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    let target = store.load(&request.successor_product_id()).unwrap();
    let empty = SessionViews::empty();
    store
        .commit_projection_views(&id, &ViewWrites::between(&empty, &empty), &empty.state)
        .unwrap();
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_clear_notice BEFORE INSERT ON operator_notices BEGIN SELECT RAISE(ABORT, 'cut'); END;").unwrap();
    let revision = source.store_revision;
    assert!(store.withdraw_unadmitted_workflows(&mut source).is_err());
    assert_eq!(source.store_revision, revision);
    assert_eq!(store.load(&id).unwrap().store_revision, revision);
    assert!(store.load(&target.id).is_ok());
    assert_eq!(
        store.load_intents(&id).unwrap().workflow_requests,
        vec![request]
    );
    assert_eq!(
        store.load_intents(&id).unwrap().notices,
        [] as [OperatorNotice; 0]
    );
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_clear_notice")
        .unwrap();
    assert_eq!(
        store
            .withdraw_unadmitted_workflows(&mut source)
            .unwrap()
            .len(),
        1
    );
    assert!(store.load(&target.id).is_err());
    let bound: bool = store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM identity_bindings WHERE identity=?1 AND owner_session_id=?2)",
        params![target.create_operation_id, target.id], |row| row.get(0),
    ).unwrap();
    assert!(
        bound,
        "deleting an uncreated reservation never releases its identity"
    );
}

#[test]
fn clear_creation_join_is_atomic_rebuildable_and_survives_same_cursor_writers() {
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let mut source = store.load(&id).unwrap();
    let locator = SessionLocator::new(id_from(1), id_from(2), id_from(3));
    source.bind_plakous_locator(locator).unwrap();
    store.commit(&mut source).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:clear:atomic-join".into(),
        source_locator: locator,
        kind: SessionWorkflowKind::Clear,
    };
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    let target = store.load(&request.successor_product_id()).unwrap();
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(1);
    views.state.created_operation_id = Some(target.create_operation_id.clone());
    views.state.runtime_session_id = target.plakous_locator.map(SessionLocator::session_id);
    let writes = ViewWrites::between(&SessionViews::empty(), &views);
    store.connection.execute_batch(&format!(
        "CREATE TEMP TRIGGER refuse_clear_join BEFORE INSERT ON session_views WHEN NEW.session_id='{id}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
    )).unwrap();
    assert!(
        store
            .commit_projection_views(&target.id, &writes, &views.state)
            .is_err()
    );
    assert!(store.load_views(&target.id).unwrap().is_none());
    assert!(store.load_views(&id).unwrap().is_none());
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_clear_join")
        .unwrap();
    store
        .commit_projection_views(&target.id, &writes, &views.state)
        .unwrap();
    let source_views = store.load_views(&id).unwrap().unwrap();
    assert!(
        source_views.state.workflows[&request.operation_id]
            .settled
            .is_some()
    );
    let stale = SessionViews::empty();
    store
        .commit_projection_views(
            &id,
            &ViewWrites::between(&source_views, &stale),
            &stale.state,
        )
        .unwrap();
    assert_eq!(store.load_views(&id).unwrap().unwrap(), source_views);
    store.delete_views(&id).unwrap();
    store
        .commit_projection_views(
            &target.id,
            &ViewWrites::between(&views, &views),
            &views.state,
        )
        .unwrap();
    assert_eq!(store.load_views(&id).unwrap().unwrap(), source_views);
    assert_eq!(
        store.withdraw_unadmitted_workflows(&mut source).unwrap(),
        [] as [TranscriptRow; 0]
    );
}

#[test]
fn fork_successor_publication_is_atomic_idempotent_and_copies_only_prefix_notices() {
    use plakous::sessions::feed::{FeedBranchTransition, FeedLocator};
    use plakous_hosted::sessions::projector::Successor;

    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let source = SessionLocator::new(
        uuid(1).parse().unwrap(),
        uuid(2).parse().unwrap(),
        uuid(3).parse().unwrap(),
    );
    let mut record = store.load(&id).unwrap();
    record.bind_plakous_locator(source).unwrap();
    store.commit(&mut record).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:fork:atomic".into(),
        source_locator: source,
        kind: SessionWorkflowKind::Fork,
    };
    store
        .submit_workflow_request(&mut record, &request)
        .unwrap();
    for anchor in [2, 5, 6] {
        insert_notice(
            &store.connection,
            &id,
            &OperatorNotice {
                notice_id: format!("notice-{anchor}"),
                anchor,
                ordinal: 0,
                row: TranscriptRow::info(NoticeScope::Session, format!("notice {anchor}")),
            },
        )
        .unwrap();
    }
    let source_views = views_at(5, &[(1, TranscriptRow::user("prefix"))]);
    let mut target_views = source_views.clone();
    target_views.state.head = Some(FeedLocator {
        session_id: source.session_id(),
        branch_id: uuid(4).parse().unwrap(),
        checkpoint_id: uuid(5).parse().unwrap(),
    });
    let mut writes = ViewWrites::between(&SessionViews::empty(), &source_views);
    writes.successors.push(Successor {
        operation_id: Some(request.operation_id.clone()),
        transition: FeedBranchTransition::Fork,
        target: target_views.state.head.unwrap(),
        published_seq: source_views.cursor,
        created_at_unix: 42,
        seed: Some(target_views.clone()),
    });
    let target_id = request.successor_product_id();
    store.connection.execute_batch(&format!(
        "CREATE TEMP TRIGGER refuse_successor BEFORE INSERT ON sessions WHEN NEW.id='{target_id}' BEGIN SELECT RAISE(ABORT, 'cut'); END;"
    )).unwrap();
    assert!(
        store
            .commit_projection_views(&id, &writes, &source_views.state)
            .is_err()
    );
    assert!(store.load_views(&id).unwrap().is_none());
    assert!(store.load(&target_id).is_err());
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_successor")
        .unwrap();
    store.connection.execute_batch(&format!(
        "CREATE TEMP TRIGGER refuse_seed BEFORE INSERT ON session_views WHEN NEW.session_id='{target_id}' BEGIN SELECT RAISE(ABORT, 'seed cut'); END;"
    )).unwrap();
    assert!(
        store
            .commit_projection_views(&id, &writes, &source_views.state)
            .is_err()
    );
    assert!(store.load_views(&id).unwrap().is_none());
    assert!(store.load(&target_id).is_err());
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_seed")
        .unwrap();
    assert_eq!(
        store
            .commit_projection_views(&id, &writes, &source_views.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    assert_eq!(
        store
            .commit_projection_views(&id, &writes, &source_views.state)
            .unwrap(),
        ProjectionCommit::CursorMoved
    );
    let target = store.load(&target_id).unwrap();
    assert_eq!(target.created_at_unix, 42);
    assert_eq!(
        target.source_product_session_id.as_deref(),
        Some(id.as_str())
    );

    assert_eq!(
        store.load_views(&target_id).unwrap().as_ref(),
        Some(&target_views)
    );
    let notices = store.load_intents(&target_id).unwrap().notices;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].anchor, 2);
    store.delete_views(&id).unwrap();
    assert_eq!(
        store
            .commit_projection_views(&id, &writes, &source_views.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    assert_eq!(
        store.load_views(&target_id).unwrap().as_ref(),
        Some(&target_views)
    );
    assert_eq!(store.load_intents(&target_id).unwrap().notices, notices);
    assert_eq!(
        store.load(&target_id).unwrap().store_revision,
        target.store_revision
    );
}

#[test]
fn successor_publication_skips_an_existing_target_cursor_even_at_zero() {
    use plakous::sessions::feed::{FeedBranchTransition, FeedLocator};
    use plakous_hosted::sessions::projector::Successor;
    let root = tempdir().unwrap();
    let (mut store, id) = store_with_session(root.path());
    let source = SessionLocator::new(
        uuid(1).parse().unwrap(),
        uuid(2).parse().unwrap(),
        uuid(3).parse().unwrap(),
    );
    let mut record = store.load(&id).unwrap();
    record.bind_plakous_locator(source).unwrap();
    store.commit(&mut record).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:fork:existing-cursor".into(),
        source_locator: source,
        kind: SessionWorkflowKind::Fork,
    };
    store
        .submit_workflow_request(&mut record, &request)
        .unwrap();
    let target_id = request.successor_product_id();
    // Model a target cursor already installed by another projection path.
    let state = serde_json::to_string(&SessionViews::empty().state).unwrap();
    store.connection.execute_batch(&format!(
        "CREATE TEMP TRIGGER existing_cursor AFTER INSERT ON sessions WHEN NEW.id='{target_id}' BEGIN INSERT INTO projector_cursors(session_id,runtime_session_id,applied_seq) VALUES(NEW.id,'',0); INSERT INTO session_views(session_id,head_locator,root_turn_count,tool_call_count,compaction_count,state) VALUES(NEW.id,NULL,0,0,0,'{state}'); END;"
    )).unwrap();
    let source_views = views_at(5, &[]);
    let mut seed = source_views.clone();
    seed.state.head = Some(FeedLocator {
        session_id: source.session_id(),
        branch_id: uuid(4).parse().unwrap(),
        checkpoint_id: uuid(5).parse().unwrap(),
    });
    let mut writes = ViewWrites::between(&SessionViews::empty(), &source_views);
    writes.successors.push(Successor {
        operation_id: Some(request.operation_id),
        transition: FeedBranchTransition::Fork,
        target: seed.state.head.unwrap(),
        published_seq: seed.cursor,
        created_at_unix: 42,
        seed: Some(seed),
    });
    assert_eq!(
        store
            .commit_projection_views(&id, &writes, &source_views.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    let cursor: i64 = store
        .connection
        .query_row(
            "SELECT applied_seq FROM projector_cursors WHERE session_id=?1",
            [&target_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cursor, 0);
    assert!(store.load(&target_id).is_ok());
}

fn writes(from: &SessionViews, to: &SessionViews) -> ViewWrites {
    ViewWrites::between(from, to)
}

#[test]
fn long_session_projection_write_measurement() {
    use plakous_hosted::sessions::projector::CallView;
    let root = tempdir().unwrap();
    let (mut store, session) = store_with_session(root.path());
    let mut views = views_at(1, &[]);
    for n in 0..2_000 {
        views.state.calls.insert(
            format!("call-{n}"),
            CallView {
                key: RowKey::new(n + 1, Lane::Runtime, 0),
                turn_id: Some("turn-1".to_owned()),
                counted: true,
                operated: true,
                settled: true,
            },
        );
    }
    store
        .commit_projection_views(
            &session,
            &writes(&SessionViews::empty(), &views),
            &views.state,
        )
        .unwrap();
    let start = std::time::Instant::now();
    let mut state_bytes = 0;
    let before = store.connection.total_changes();
    for _ in 0..100 {
        let from = views.clone();
        views.cursor = FeedSeq::new(views.cursor.get() + 1);
        views.state.calls.get_mut("call-1999").unwrap().settled ^= true;
        store
            .commit_projection_views(&session, &writes(&from, &views), &views.state)
            .unwrap();
        state_bytes = store
            .connection
            .query_row(
                "SELECT length(state) FROM session_views WHERE session_id=?1",
                [&session],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
    }
    eprintln!(
        "L5_MEASUREMENT calls=2000 commits=100 elapsed_us={} rows_changed={} state_bytes={state_bytes}",
        start.elapsed().as_micros(),
        store.connection.total_changes() - before
    );
}

#[test]
fn keyed_bookkeeping_is_not_in_the_session_state_document() {
    let state = serde_json::to_value(SessionViewState::default()).unwrap();
    for field in ["calls", "open_turns", "processes"] {
        assert!(state.get(field).is_none(), "{field} must be a keyed table");
    }
}

fn with_bookkeeping(mut views: SessionViews) -> SessionViews {
    for n in 0..2 {
        views.state.calls.insert(
            format!("call-{n}"),
            CallView {
                key: RowKey::new(n + 1, Lane::Runtime, 0),
                turn_id: Some(format!("turn-{n}")),
                counted: true,
                operated: true,
                settled: false,
            },
        );
        views.state.open_turns.insert(
            format!("turn-{n}"),
            OpenTurn {
                command_id: format!("command-{n}"),
                host_wake: false,
                round_anchor: Some(n + 1),
                cancel_requested: false,
            },
        );
        views.state.processes.insert(
            n,
            ProcessTrack {
                dispatched_at_ms: Some(n * 100),
                reported: false,
            },
        );
    }
    views
}

#[test]
fn keyed_views_round_trip_change_only_touched_keys_and_rebuild_under_cas() {
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let (mut store, session) = store_with_session(root.path());
    assert_eq!(super::super::SCHEMA_VERSION, 34);
    let empty = SessionViews::empty();
    let first = with_bookkeeping(views_at(1, &[]));
    store
        .commit_projection_views(&session, &writes(&empty, &first), &first.state)
        .unwrap();
    assert_eq!(store.load_views(&session).unwrap(), Some(first.clone()));
    let mut second = first.clone();
    second.cursor = FeedSeq::new(2);
    second.state.calls.remove("call-0");
    second.state.calls.get_mut("call-1").unwrap().settled = true;
    second.state.open_turns.remove("turn-0");
    second
        .state
        .open_turns
        .get_mut("turn-1")
        .unwrap()
        .cancel_requested = true;
    second.state.processes.remove(&0);
    second.state.processes.get_mut(&1).unwrap().reported = true;
    let before = store.connection.total_changes();
    store
        .commit_projection_views(&session, &writes(&first, &second), &second.state)
        .unwrap();
    assert_eq!(store.connection.total_changes() - before, 8);
    let before = store.connection.total_changes();
    assert_eq!(
        store
            .commit_projection_views(&session, &writes(&first, &second), &second.state)
            .unwrap(),
        ProjectionCommit::CursorMoved
    );
    assert_eq!(store.connection.total_changes(), before);
    drop(store);
    let mut store = ProductStore::open(&paths).unwrap();
    assert_eq!(store.load_views(&session).unwrap(), Some(second.clone()));
    store.delete_views(&session).unwrap();
    for table in [
        "session_call_views",
        "session_open_turn_views",
        "session_process_views",
    ] {
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
                    [&session],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
    store
        .commit_projection_views(&session, &writes(&empty, &second), &second.state)
        .unwrap();
    assert_eq!(store.load_views(&session).unwrap(), Some(second));
}

#[test]
fn a_failed_keyed_write_rolls_back_the_cursor_and_all_views() {
    let root = tempdir().unwrap();
    let (mut store, session) = store_with_session(root.path());
    let empty = SessionViews::empty();
    let first = with_bookkeeping(views_at(1, &[]));
    store
        .commit_projection_views(&session, &writes(&empty, &first), &first.state)
        .unwrap();
    store.connection.execute_batch("CREATE TEMP TRIGGER fail_process_update BEFORE UPDATE ON session_process_views BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
    let mut second = first.clone();
    second.cursor = FeedSeq::new(2);
    second.state.calls.get_mut("call-1").unwrap().settled = true;
    second.state.processes.get_mut(&1).unwrap().reported = true;
    assert!(
        store
            .commit_projection_views(&session, &writes(&first, &second), &second.state)
            .is_err()
    );
    assert_eq!(store.load_views(&session).unwrap(), Some(first));
}

#[test]
fn keyed_views_are_session_scoped_and_cascade_on_session_deletion() {
    let root = tempdir().unwrap();
    let (mut store, first) = store_with_session(root.path());
    let mut second = SessionRecord::new(PathBuf::from("/work"));
    store.commit(&mut second).unwrap();
    let views = with_bookkeeping(views_at(1, &[]));
    let writes = writes(&SessionViews::empty(), &views);
    for session in [&first, &second.id] {
        store
            .commit_projection_views(session, &writes, &views.state)
            .unwrap();
    }
    // Schema 31 rejects the obsolete JSON shape even through raw SQL.
    for field in ["calls", "open_turns", "processes"] {
        assert!(store.connection.execute(
            "UPDATE session_views SET state=json_set(state,?2,json('{}')) WHERE session_id=?1",
            params![first, format!("$.{field}")],
        ).is_err());
    }
    let revision = store.load(&first).unwrap().store_revision;
    store.delete_session(&first, revision).unwrap();
    assert_eq!(store.load_views(&first).unwrap(), None);
    assert_eq!(store.load_views(&second.id).unwrap(), Some(views));
    for table in [
        "session_call_views",
        "session_open_turn_views",
        "session_process_views",
    ] {
        assert_eq!(
            store
                .connection
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
                    [&first],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

#[test]
fn intents_round_trip_by_session() {
    let root = tempdir().unwrap();
    let (mut store, session) = store_with_session(root.path());
    let submission = Submission {
        command_id: "cmd-1".to_owned(),
        kind: SubmissionKind::RootStart,
        queue_item_id: "item-1".to_owned(),
        queue_item_revision: 2,
        target_turn_id: None,
        canonical_command: "{\"prompt\":\"hi\"}".to_owned(),
        submitted_at_ms: 1_234,
    };
    store.write_submission(&session, &submission).unwrap();
    store.write_submission(&session, &submission).unwrap();
    let conflicting = Submission {
        canonical_command: "other".to_owned(),
        ..submission.clone()
    };
    assert!(store.write_submission(&session, &conflicting).is_err());
    let request = WorkflowRequest {
        operation_id: "plak:compact:1".to_owned(),
        kind: SessionWorkflowKind::Compact(ProductCompactReason::Manual),
        source_locator: SessionLocator::new(
            uuid(1).parse().unwrap(),
            uuid(2).parse().unwrap(),
            uuid(3).parse().unwrap(),
        ),
    };
    store.write_workflow_request(&session, &request).unwrap();
    store.write_workflow_request(&session, &request).unwrap();
    write_import_intents(
        &store.connection,
        &session,
        &[TranscriptRow::user("carried")],
    )
    .unwrap();
    let notice = OperatorNotice {
        notice_id: "n-1".to_owned(),
        anchor: 4,
        ordinal: 0,
        row: TranscriptRow::warning(NoticeScope::Queue, "compaction did not start"),
    };
    insert_notice(&store.connection, &session, &notice).unwrap();
    store
        .write_wake_suppression(
            &session,
            &AnchoredIntent {
                anchor: 4,
                ordinal: 1,
                value: (),
            },
        )
        .unwrap();
    let pause = AnchoredIntent {
        anchor: 5,
        ordinal: 0,
        value: HostPauseCause::WakePreflightFailed {
            reason: "tree store unavailable".to_owned(),
        },
    };
    store.write_pause_cause(&session, &pause).unwrap();
    let intents = store.load_intents(&session).unwrap();
    assert_eq!(intents.submissions, vec![submission]);
    assert_eq!(intents.workflow_requests, vec![request]);
    assert_eq!(intents.notices.len(), 2);
    assert_eq!(intents.notices[0].anchor, 0);
    assert_eq!(intents.notices[1], notice);
    assert_eq!(intents.wake_suppressions.len(), 1);
    assert_eq!(intents.pause_causes, vec![pause]);
    let mut record = store.load(&session).unwrap();
    store
        .discard_submission(&mut record, "cmd-1", None)
        .unwrap();
    store
        .commit_projection_views(
            &session,
            &ViewWrites::between(&SessionViews::empty(), &views_at(0, &[])),
            &views_at(0, &[]).state,
        )
        .unwrap();
    let notices = store.withdraw_unadmitted_workflows(&mut record).unwrap();
    assert_eq!(notices.len(), 1);
    let intents = store.load_intents(&session).unwrap();
    assert!(intents.submissions.is_empty() && intents.workflow_requests.is_empty());
    assert_eq!(intents.notices.len(), 3);
    assert_eq!(
        store.withdraw_unadmitted_workflows(&mut record).unwrap(),
        [] as [TranscriptRow; 0]
    );
}

/// Spec 02 §2.6: the counters are views. The live record's counters are
/// display state; load and listing read only the committed views.
#[test]
fn counters_load_and_list_only_from_committed_views() {
    let root = tempdir().unwrap();
    let mut store = ProductStore::open(&PlakPaths::explicit(root.path().join("p"))).unwrap();
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    session.root_turn_count = 9;
    session.tool_call_count = 9;
    session.compaction_count = 9;
    store.commit(&mut session).unwrap();
    let counters = |record: &SessionRecord| {
        (
            record.root_turn_count,
            record.tool_call_count,
            record.compaction_count,
        )
    };
    assert_eq!(counters(&store.load(&session.id).unwrap()), (0, 0, 0));
    let views = views_at(2, &[(1, TranscriptRow::user("hello"))]);
    store
        .commit_projection_views(
            &session.id,
            &writes(&SessionViews::empty(), &views),
            &views.state,
        )
        .unwrap();
    assert_eq!(counters(&store.load(&session.id).unwrap()), (2, 1, 0));
    let listed = store.list_for_workspace(Path::new("/work")).unwrap();
    assert_eq!(counters(&listed[0]), (2, 1, 0));
}

#[test]
fn a_projection_commits_by_cursor_cas_and_a_stale_applier_changes_nothing() {
    let root = tempdir().unwrap();
    let (mut store, session) = store_with_session(root.path());
    assert_eq!(store.load_views(&session).unwrap(), None);
    let empty = SessionViews::empty();
    let first = views_at(
        2,
        &[
            (1, TranscriptRow::user("hello")),
            (2, TranscriptRow::assistant("hi")),
        ],
    );
    assert_eq!(
        store
            .commit_projection_views(&session, &writes(&empty, &first), &first.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    assert_eq!(store.load_views(&session).unwrap().as_ref(), Some(&first));
    // A concurrent applier that read the empty cursor loses the CAS.
    let other = views_at(1, &[(1, TranscriptRow::user("stale"))]);
    assert_eq!(
        store
            .commit_projection_views(&session, &writes(&empty, &other), &other.state)
            .unwrap(),
        ProjectionCommit::CursorMoved
    );
    let mut second = views_at(3, &[(3, TranscriptRow::assistant("more"))]);
    second.rows.insert(
        RowKey::new(2, Lane::Runtime, 0),
        TranscriptRow::assistant("hi"),
    );
    assert_eq!(
        store
            .commit_projection_views(&session, &writes(&first, &second), &second.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    assert_eq!(store.load_views(&session).unwrap(), Some(second.clone()));
    // Deleting the views is always safe; a rebuild from empty writes the
    // same views.
    store.delete_views(&session).unwrap();
    assert_eq!(store.load_views(&session).unwrap(), None);
    store
        .commit_projection_views(&session, &writes(&empty, &second), &second.state)
        .unwrap();
    assert_eq!(store.load_views(&session).unwrap(), Some(second));
}

/// Spec 02 §2.6: nothing but the projector writes views. Every statement
/// that writes a view table lives in this module, behind
/// `commit_projection_views` and `delete_views`.
#[test]
fn only_the_projector_commit_writes_view_tables() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        manifest.join("src"),
        manifest.join("../../crates/hosting/hosted/src"),
    ];
    let own = manifest.join("src/store/views.rs");
    let own_tests = manifest.join("src/store/views/tests.rs");
    let mut offenders = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs")
            || path.canonicalize().unwrap() == own.canonicalize().unwrap()
            || path == own_tests
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for table in [
            "transcript_rows",
            "session_views",
            "projector_cursors",
            "session_call_views",
            "session_open_turn_views",
            "session_process_views",
        ] {
            for verb in ["INSERT INTO ", "UPDATE ", "DELETE FROM ", "REPLACE INTO "] {
                if text.contains(&format!("{verb}{table}")) {
                    offenders.push(format!("{}: {verb}{table}", path.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "view writers outside the projector commit: {offenders:?}"
    );
}
