use std::path::Path;
use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;

use plakd_wire_types::v13::{LiveRowLength, RetainedRevision};
use plakous::agent::CommandId;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenedSession, SessionBackend,
    SessionOperationId,
    feed::{FeedCursor, FeedPage, FeedPageRequest, FeedSeq},
};
use plakous_host_protocol::views::transcript::{Lane, TranscriptRow, TranscriptScope};
use plakous_hosted::sessions::SessionRecord;
use plakous_hosted::sessions::projector::{ProductIntents, SessionViews, ViewWrites, run};
use plakous_offline_crash_harness::{
    BoundaryController,
    protocol::CrashBoundary,
    script::{SCRIPT_VERSION, ScriptPlan},
};

use super::*;
use crate::store::ProductStore;

fn key(anchor: u64, ordinal: u32) -> RowKey {
    RowKey::new(anchor, Lane::Runtime, ordinal)
}

fn target() -> BaselineTarget {
    BaselineTarget {
        attachment: 7,
        scope: TranscriptScope::Root,
        revision: RetainedRevision {
            host_generation: 13,
            revision: 1,
            token: "viewport".into(),
        },
    }
}

fn cursor_of(frames: &[HostFrameV13]) -> HistoryCursor {
    let HostFrameV13::Snapshot(baseline) = &frames[1] else {
        panic!("no baseline")
    };
    baseline.older_cursor.clone().unwrap()
}

async fn scripted_session(paths: &PlakPaths, workspace: &Path) -> (AgentSessions, OpenedSession) {
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("evidence.jsonl"),
    )
    .unwrap();
    let plan: Arc<ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": SCRIPT_VERSION,
            "steps": [
                {"output": {"text": "earlier café"}},
                {"output": {"text": "wake answer"}},
                {"output": {"text": "appended answer"}}
            ]
        }))
        .unwrap(),
    );
    let binding = plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
        workspace,
        controller,
        plan,
        Arc::new(AtomicU64::new(1)),
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("history-fixture:create").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("fixture session not created")
    };
    (sessions, session)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_without_a_user_row_starts_a_page_and_cursor_survives_append_and_reopen() {
    let home = PlakPaths::isolated_tempdir();
    let paths = PlakPaths::explicit(home.path().join("home"));
    let mut store = ProductStore::open(&paths).unwrap();
    let (sessions, session) = scripted_session(&paths, home.path()).await;
    let mut record = SessionRecord::new(home.path().to_owned());
    record.plakous_locator = Some(session.locator());
    store.commit(&mut record).unwrap();

    session
        .agent()
        .prompt("earlier prompt")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    session
        .agent()
        .start_instruction(
            CommandId::new("history-fixture:wake").unwrap(),
            "wake input",
        )
        .await
        .unwrap()
        .into_turn()
        .unwrap()
        .result()
        .await
        .unwrap();
    let views = apply_feed(&sessions, &mut store, &record, SessionViews::empty()).await;
    let attached = attach(&paths, &record.id, 1, target(), LiveTail::default()).unwrap();
    let HostFrameV13::Snapshot(baseline) = &attached.frames[1] else {
        panic!("no baseline")
    };
    assert_eq!(baseline.older_rows, 2);
    assert_eq!(
        baseline.older_bytes,
        ("earlier prompt".len() + "earlier café".len()) as u64
    );
    assert_eq!(baseline.items.len(), 1);
    assert_eq!(
        baseline.items[0].row.0,
        TranscriptRow::assistant("wake answer")
    );
    let cursor = cursor_of(&attached.frames);
    let expected = older(&paths, &attached.binding, 2, &cursor).unwrap();
    assert!(
        matches!(&expected[1], HostFrameV13::HistoryPage { items, last: true, has_older: false, .. } if items.len() == 2)
    );

    session
        .agent()
        .prompt("appended prompt")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    let views = apply_feed(&sessions, &mut store, &record, views).await;
    drop(store);
    let mut store = ProductStore::open(&paths).unwrap();
    store.set_archived(&record.id, true).unwrap();
    let attached = attach(&paths, &record.id, 1, target(), LiveTail::default()).unwrap();
    assert_eq!(
        older(&paths, &attached.binding, 2, &cursor).unwrap(),
        expected
    );
    let mut pruned = views.clone();
    pruned.cursor = FeedSeq::new(views.cursor.get() + 1);
    pruned.rows.pop_first();
    pruned.state.truncated = true;
    store
        .commit_projection_views(
            &record.id,
            &ViewWrites::between(&views, &pruned),
            &pruned.state,
        )
        .unwrap();
    assert_expired(&older(&paths, &attached.binding, 2, &cursor).unwrap());
    let attached = attach(&paths, &record.id, 1, target(), LiveTail::default()).unwrap();
    let rebound_cursor = cursor_of(&attached.frames);
    let mut successor = SessionRecord::new(home.path().to_owned());
    successor.node = record.node.clone();
    successor.source_product_session_id = Some(record.id.clone());
    successor.source_workflow_operation_id = Some("history-fixture:rebind".into());
    store.commit(&mut successor).unwrap();
    assert_expired(&older(&paths, &attached.binding, 2, &rebound_cursor).unwrap());
    assert_eq!(
        read_row(&paths, &attached.binding, 3, baseline.items[0].key, 0),
        Err(HistoryError::UnknownRow)
    );
    let successor = attach(&paths, &successor.id, 1, target(), LiveTail::default()).unwrap();
    assert_expired(&older(&paths, &successor.binding, 2, &rebound_cursor).unwrap());
}

fn assert_expired(frames: &[HostFrameV13]) {
    assert!(
        matches!(frames, [HostFrameV13::Response { outcome: Outcome::Refused(refusal), .. }]
        if **refusal == RequestRefusal::Draft(RequestRefusalV13::CursorExpired))
    );
}

async fn apply_feed(
    sessions: &AgentSessions,
    store: &mut ProductStore,
    record: &SessionRecord,
    views: SessionViews,
) -> SessionViews {
    let request = FeedPageRequest::after(FeedCursor::new(
        record.plakous_locator.unwrap().session_id(),
        views.cursor,
    ));
    let FeedPage::Records { records, .. } = sessions.feed_page(request).await.unwrap() else {
        panic!("missing fixture feed")
    };
    let projection = run(sessions, views, &ProductIntents::default(), &records)
        .await
        .unwrap();
    store
        .commit_projection_views(&record.id, &projection.writes, &projection.views.state)
        .unwrap();
    projection.views
}

#[test]
fn an_older_page_never_splits_a_turn_even_when_one_turn_exceeds_the_budget() {
    let fixture = Fixture::new();
    fixture.seed(
        90,
        [
            (1, 0, "old".into()),
            (25, 9, "é".repeat(12)),
            (27, 83, "☕".into()),
            (90, 3, "new".into()),
        ],
    );
    transcript_history::read(&fixture.paths, &fixture.id, |history| {
        let turns = Turns {
            starts: vec![1, 24, 89],
            retained_from: 1,
        };
        let latest = first(&history, &turns)?;
        assert_eq!(latest.items[0].key, key(90, 3));
        assert_eq!(latest.older, Totals { rows: 3, bytes: 30 });
        let (_, _, retention, before) = decode_cursor(&latest.cursor.unwrap())?;
        let older = older_page(&history, &turns, retention, before, 9)?;
        assert_eq!(
            older.items.iter().map(|item| item.key).collect::<Vec<_>>(),
            [key(25, 9), key(27, 83)]
        );
        assert_eq!(older.older, Totals { rows: 1, bytes: 3 });
        let (_, _, retention, before) = decode_cursor(&older.cursor.unwrap())?;
        let oldest = older_page(&history, &turns, retention, before, 9)?;
        assert_eq!(oldest.items[0].key, key(1, 0));
        assert!(oldest.cursor.is_none());
        assert!(matches!(
            older_page(&history, &turns, retention + 1, before, 9),
            Err(HistoryError::CursorExpired)
        ));
        Ok(())
    })
    .unwrap();
}

#[test]
fn baseline_parts_keep_one_turn_order_and_merge_live_rows_by_key() {
    let fixture = Fixture::new();
    assert!(
        matches!(&attach(&fixture.paths, &fixture.id, 1, target(), LiveTail::default()).unwrap().frames[1],
        HostFrameV13::Snapshot(baseline) if baseline.items.is_empty() && baseline.last && !baseline.has_older)
    );
    let body = "☕".repeat(52_000);
    fixture.seed(
        105,
        [
            (10, 0, "old".into()),
            (100, 2, body.clone()),
            (105, 12, body.clone()),
        ],
    );
    transcript_history::read(&fixture.paths, &fixture.id, |history| {
        let turns = Turns {
            starts: vec![10, 100],
            retained_from: 1,
        };
        let page = first(&history, &turns)?;
        let cursor = page.cursor.clone();
        let live_key = key(100, 2);
        let appended = format!("{body} α");
        let live = LiveTail {
            applied: Some(history.applied),
            upserts: vec![TranscriptItem {
                key: live_key,
                row: TranscriptRow::assistant(&appended),
            }],
            lengths: [(live_key, appended.len() as u64)].into(),
        };
        let parts = framing::baseline(page, target(), live)?;
        assert_eq!(parts.len(), 2);
        for (index, (frame, expected)) in parts.iter().zip([live_key, key(105, 12)]).enumerate() {
            let HostFrameV13::Snapshot(baseline) = frame else {
                panic!("wrong frame")
            };
            assert!(baseline.has_older);
            assert_eq!((baseline.older_rows, baseline.older_bytes), (1, 3));
            assert_eq!(baseline.older_cursor, cursor);
            assert_eq!(baseline.part, u32::try_from(index).unwrap());
            assert_eq!(baseline.last, index == 1);
            assert_eq!(baseline.items.len(), 1);
            assert_eq!(baseline.items[0].key, expected);
            assert!(baseline.items[0].truncated_bytes.is_none());
            if index == 0 {
                assert_eq!(baseline.items[0].row.0.text(), Some(appended.as_str()));
                assert_eq!(
                    baseline.live_rows,
                    [LiveRowLength {
                        row: live_key,
                        len: appended.len() as u64
                    }]
                );
            }
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn an_oversized_encoded_row_keeps_a_scalar_prefix_and_is_readable_by_key() {
    let fixture = Fixture::new();
    let body = "é\0".repeat(2_200_000);
    let row = key(42, 99);
    fixture.seed(42, [(42, 99, body.clone())]);
    let attached = attach(
        &fixture.paths,
        &fixture.id,
        1,
        target(),
        LiveTail::default(),
    )
    .unwrap();
    assert!(
        serde_json::to_vec(&attached.frames[1]).unwrap().len()
            < plakous_host_protocol::wire::MAX_LINE_BYTES
    );
    let HostFrameV13::Snapshot(part) = &attached.frames[1] else {
        panic!("no baseline")
    };
    let item = &part.items[0];
    let tail = item.row.0.text().unwrap().len();
    assert!(body.is_char_boundary(tail));
    assert_eq!(item.truncated_bytes, Some((body.len() - tail) as u64));
    let reply = read_row(&fixture.paths, &attached.binding, 2, row, tail as u64).unwrap();
    let HostFrameV13::Response {
        outcome: Outcome::Ok(answer),
        ..
    } = reply
    else {
        panic!("no answer")
    };
    let Answer::Draft(AnswerV13::TranscriptRow {
        offset, end, text, ..
    }) = *answer
    else {
        panic!("no row slice")
    };
    assert_eq!(offset, tail as u64);
    assert_eq!(text, &body[tail..usize::try_from(end).unwrap()]);
    assert_ne!(text, "");
    assert_eq!(
        read_row(&fixture.paths, &attached.binding, 3, row, 1),
        Err(HistoryError::InvalidOffset)
    );
    assert_eq!(
        read_row(&fixture.paths, &attached.binding, 4, key(42, 100), 0),
        Err(HistoryError::UnknownRow)
    );
}

#[test]
fn large_history_measurement() {
    let fixture = Fixture::new();
    let starts: Vec<u64> = (0..1_000).map(|turn| turn * 100 + 1).collect();
    fixture.seed(
        100_000,
        starts
            .iter()
            .copied()
            .flat_map(|start| (0..8).map(move |offset| (start + offset, 7, "é".repeat(512)))),
    );
    let turns = Turns {
        starts,
        retained_from: 1,
    };
    let start = Instant::now();
    let (page, parts) = transcript_history::read(&fixture.paths, &fixture.id, |history| {
        let page = first(&history, &turns)?;
        let cursor = page.cursor.clone().unwrap();
        Ok((
            cursor,
            framing::baseline(page, target(), LiveTail::default())?,
        ))
    })
    .unwrap();
    let first_time = start.elapsed();
    let start = Instant::now();
    let older = transcript_history::read(&fixture.paths, &fixture.id, |history| {
        let (_, _, retention, before) = decode_cursor(&page)?;
        framing::history(
            older_page(
                &history,
                &turns,
                retention,
                before,
                PAGE_TARGET_BYTES as u64,
            )?,
            7,
            2,
        )
    })
    .unwrap();
    let older_time = start.elapsed();
    let bytes = parts
        .iter()
        .map(|frame| serde_json::to_vec(frame).unwrap().len() + 1)
        .sum::<usize>();
    eprintln!(
        "history fixture: 8,000 rows, 8,192,000 body bytes; first {bytes} wire bytes in {first_time:?}; older {} frame(s) in {older_time:?}",
        older.len()
    );
    assert!(
        matches!(&parts[0], HostFrameV13::Snapshot(page) if page.items.len() == 8 && page.older_rows == 7_992 && page.older_bytes == 8_183_808)
    );
}

struct Fixture {
    _home: tempfile::TempDir,
    paths: PlakPaths,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let home = PlakPaths::isolated_tempdir();
        let paths = PlakPaths::explicit(home.path().join("home"));
        let mut record = SessionRecord::new(home.path().to_owned());
        ProductStore::open(&paths)
            .unwrap()
            .commit(&mut record)
            .unwrap();
        Self {
            _home: home,
            paths,
            id: record.id,
        }
    }

    fn seed(&self, applied: u64, rows: impl IntoIterator<Item = (u64, u32, String)>) {
        let mut views = SessionViews::empty();
        views.cursor = FeedSeq::new(applied);
        views.rows.extend(
            rows.into_iter().map(|(anchor, ordinal, body)| {
                (key(anchor, ordinal), TranscriptRow::assistant(body))
            }),
        );
        ProductStore::open(&self.paths)
            .unwrap()
            .commit_projection_views(
                &self.id,
                &ViewWrites::between(&SessionViews::empty(), &views),
                &views.state,
            )
            .unwrap();
    }
}
