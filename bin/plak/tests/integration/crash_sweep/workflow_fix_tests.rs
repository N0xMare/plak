//! Regression cuts for feed-settled workflow request intents.

use super::*;
use plak::store::sessions::{load_session, save_session};

fn workflow_cut(ordinal: u64) -> (Fixture, Fixture, String) {
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    let fixture = workflow_cut_from(&seed, &session, shape, &commits, ordinal);
    (seed, fixture, session)
}

fn workflow_cut_from(
    seed: &Fixture,
    session: &str,
    shape: &str,
    commits: &[trace::Commit],
    ordinal: u64,
) -> Fixture {
    let target = commits
        .iter()
        .find(|cut| cut.store == "runtime" && cut.store_ordinal == ordinal)
        .unwrap();
    kill_fixture(seed, session, shape, commits, target).0
}

#[test]
fn unadmitted_compact_request_is_withdrawn_once_without_dispatch() {
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    let cut = commits
        .iter()
        .find(|cut| {
            cut.store == "product" && cut.tables.iter().any(|table| table == "workflow_requests")
        })
        .unwrap();
    let (fixture, _) = kill_fixture(&seed, &session, shape, &commits, cut);
    let before = invariants::summary_dispatches(&fixture);
    assert_eq!(
        summary::read(&fixture).workflow_requests[0]["state"],
        "pending"
    );
    for label in ["withdraw-request", "withdraw-request-again"] {
        let mut opened = traced_reopen(&fixture, &session, label, None)
            .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
        opened.send(&quit_frame());
        assert!(opened.finish().status.success());
        assert_eq!(invariants::summary_dispatches(&fixture), before);
        let mut store = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
        let intents = store.load_intents(&session).unwrap();
        assert!(intents.workflow_requests.is_empty());
        assert_eq!(
            intents
                .notices
                .iter()
                .filter(|notice| {
                    notice.row.text() == Some("compaction did not start; run /compact again")
                })
                .count(),
            1
        );
    }
}

#[test]
fn admitted_workflow_rehearsal_passes_without_database_writes() {
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    for ordinal in 9..=11 {
        let fixture = workflow_cut_from(&seed, &session, shape, &commits, ordinal);
        let result = summary::rehearse(&fixture, &session);
        assert_eq!(result["verdict"], "ResolvesCompactionWorkflow", "{result}");
        assert_eq!(result["success"], true, "{result}");
        let mut opened = traced_reopen(&fixture, &session, "rehearsal-recovered", None)
            .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
        opened.send(&quit_frame());
        assert!(opened.finish().status.success());
        rehearse_recovered_head(&fixture, &session);
    }
}

fn rehearse_recovered_head(fixture: &Fixture, session: &str) {
    // Cold SQLite readers can create empty WAL coordination files, but no durable bytes.
    let bytes = || {
        let mut files = summary::database_bytes(fixture);
        files.retain(|path, data| !path.to_string_lossy().ends_with("-wal") || !data.is_empty());
        files
    };
    let before = bytes();
    let mut command = fixture.command();
    command.args(["session", "rehearse", session]);
    let outcome = AgentProcess::start_command(command, true).finish();
    assert!(before == bytes(), "recovered rehearsal wrote durable bytes");
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert!(
        outcome
            .lines
            .iter()
            .any(|line| line.starts_with("root: offline=OpensQuiescent "))
    );
}

#[test]
fn abandoned_prepared_workflow_does_not_swallow_next_explicit_compact() {
    let (_seed, fixture, session) = workflow_cut(9);
    let compact_rows = || {
        let database = rusqlite::Connection::open_with_flags(
            summary::paths(&fixture).product_database(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        database
            .query_row(
                "SELECT count(*) FROM (SELECT anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT anchor,1 AS lane,ordinal,row FROM operator_notices) WHERE json_extract(row, '$.name') = 'compact'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let mut opened = traced_reopen(&fixture, &session, "prepared-recovery", None)
        .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
    // The views are the feed's: its compaction derives one row whenever the
    // killed host's projector got to it, and the reopen's abandonment none.
    assert_eq!(
        compact_rows(),
        1,
        "abandonment must not add another compact row"
    );
    let before = invariants::summary_dispatches(&fixture);
    opened.send(&json!({"v":1,"kind":"compact"}));
    opened.wait_for("new explicit compaction", |row| {
        row["kind"] == "tool"
            && row["activity_kind"] == "compact"
            && matches!(row["state"].as_str(), Some("done" | "failed"))
    });
    opened.send(&quit_frame());
    assert!(opened.finish().status.success());
    assert_eq!(
        invariants::summary_dispatches(&fixture).len(),
        before.len() + 1
    );
    assert_eq!(
        plak::store::ProductStore::open(&summary::paths(&fixture))
            .unwrap()
            .load_intents(&session)
            .unwrap()
            .workflow_requests
            .len(),
        2,
        "the next compact is a new operation"
    );
}

#[test]
fn completed_workflow_request_is_applied_without_redispatch() {
    let (_seed, fixture, session) = workflow_cut(11);
    let paths = summary::paths(&fixture);
    let before = invariants::summary_dispatches(&fixture);
    let mut opened = traced_reopen(&fixture, &session, "completed-recovery", None)
        .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
    let after = summary::read_converged(&fixture);
    assert!(
        after
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["state"] == "finalized")
    );
    let record = load_session(&paths, &session).unwrap();
    assert_eq!(
        record.compaction_count, 1,
        "a durable paid summary must be applied"
    );
    assert_eq!(invariants::summary_dispatches(&fixture), before);
    let history = plakous::sessions::AgentSessions::export_history_sqlite(
        paths.runtime_database(),
        record.plakous_locator.unwrap(),
    )
    .unwrap();
    assert!(
        history
            .segments()
            .iter()
            .any(|segment| segment.kind() == plakous::model::HistorySegmentKind::Compaction)
    );
    opened.send(&quit_frame());
    assert!(opened.finish().status.success());
}

/// Spec 02: the runtime closes the admitted compaction at open and the one
/// notice names the dispatch fact the ledger holds; a completed summary is
/// applied from its record and settles `Compacted`, with no notice.
#[test]
fn interrupted_manual_compact_notice_names_dispatch_fact_not_an_answer() {
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    for (ordinal, expected, compactions) in [
        (
            9,
            Some(
                "plak: interrupted compaction was not applied; its summary was not sent and will not be dispatched again; the conversation history is unchanged",
            ),
            0,
        ),
        (
            10,
            Some(
                "plak: interrupted compaction was not applied, may have been billed, and will not be dispatched again; the conversation history is unchanged",
            ),
            0,
        ),
        (11, None, 1),
    ] {
        let fixture = workflow_cut_from(&seed, &session, shape, &commits, ordinal);
        let dispatches = invariants::summary_dispatches(&fixture);
        let mut opened = traced_reopen(&fixture, &session, "notice-recovery", None)
            .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
        opened.send(&quit_frame());
        assert!(opened.finish().status.success());
        let db = rusqlite::Connection::open(summary::paths(&fixture).product_database()).unwrap();
        let mut query = db.prepare("SELECT json_extract(row, '$.text') FROM (SELECT anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT anchor,1 AS lane,ordinal,row FROM operator_notices) WHERE instr(row, 'interrupted compaction') > 0").unwrap();
        let notices = query
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(notices, Vec::from_iter(expected), "cut {ordinal}");
        let record = load_session(&summary::paths(&fixture), &session).unwrap();
        assert_eq!(record.compaction_count, compactions, "cut {ordinal}");
        assert_eq!(invariants::summary_dispatches(&fixture), dispatches);
    }
}

fn compact_rows(fixture: &Fixture) -> Vec<Value> {
    let db = rusqlite::Connection::open(summary::paths(fixture).product_database()).unwrap();
    let mut query = db.prepare("SELECT row FROM (SELECT anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT anchor,1 AS lane,ordinal,row FROM operator_notices) WHERE json_extract(row, '$.name') = 'compact' ORDER BY anchor,lane,ordinal").unwrap();
    let rows = query
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    rows.iter()
        .map(|row| {
            let mut row: Value = serde_json::from_str(row).unwrap();
            let fields = row.as_object_mut().unwrap();
            fields.remove("started_at_ms");
            fields.remove("finished_at_ms");
            row
        })
        .collect()
}

fn interrupted_compact_notices(fixture: &Fixture) -> i64 {
    let db = rusqlite::Connection::open(summary::paths(fixture).product_database()).unwrap();
    db.query_row(
        "SELECT count(*) FROM (SELECT anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT anchor,1 AS lane,ordinal,row FROM operator_notices) WHERE instr(row, 'interrupted compaction') > 0",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn receipt_with_reason(
    fixture: &Fixture,
    session: &str,
    reason: plakous_hosted::sessions::ProductCompactReason,
) -> plakous_hosted::sessions::SessionRecord {
    use plakous_hosted::sessions::SessionWorkflowKind;
    let paths = summary::paths(fixture);
    let record = load_session(&paths, session).unwrap();
    let mut store = plak::store::ProductStore::open(&paths).unwrap();
    let mut request = store
        .load_intents(session)
        .unwrap()
        .workflow_requests
        .last()
        .unwrap()
        .clone();
    request.kind = SessionWorkflowKind::Compact(reason);
    let binding = request.binding_digest(&record.id);
    // Synthesize the product reason together with its immutable identity claim.
    let mut db = rusqlite::Connection::open(paths.product_database()).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute(
        "UPDATE workflow_requests SET compact_reason = ?1 WHERE operation_id = ?2",
        [reason.as_str(), &request.operation_id],
    )
    .unwrap();
    tx.execute(
        "UPDATE identity_bindings SET binding_digest = ?1 WHERE identity = ?2",
        [&binding, &request.operation_id],
    )
    .unwrap();
    tx.commit().unwrap();
    record
}

#[test]
fn committed_prepared_receipt_keeps_base_count_drain_and_rows() {
    use plakous_hosted::sessions::{ProductCompactReason, QueueDrainState, QueueItem};
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    for reason in [
        ProductCompactReason::Manual,
        ProductCompactReason::Scheduled,
        ProductCompactReason::CapacityRecovery,
    ] {
        let fixture = workflow_cut_from(&seed, &session, shape, &commits, 12);
        let paths = summary::paths(&fixture);
        let mut record = receipt_with_reason(&fixture, &session, reason);
        // An over-window queued prompt stays parked after a successful receipt.
        record
            .pending_queue
            .push(QueueItem::new("queued-prompt ".repeat(4096)));
        record.queue_drain_state = QueueDrainState::RestoredPaused;
        save_session(&paths, &mut record).unwrap();
        let dispatches = invariants::summary_dispatches(&fixture);
        let mut opened = traced_reopen(&fixture, &session, "committed-receipt", None)
            .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
        let after = load_session(&paths, &session).unwrap();
        // Counters and rows are the views, caught up at open whatever the
        // killed host had projected: the committed receipt counts once.
        assert_eq!(after.compaction_count, 1);
        assert_eq!(after.queue_drain_state, QueueDrainState::RestoredPaused);
        assert_eq!(summary::read_converged(&fixture).drain, "restored_paused");
        assert_eq!(after.pending_queue, record.pending_queue);
        assert_eq!(compact_rows(&fixture).len(), 1);
        assert_eq!(interrupted_compact_notices(&fixture), 0);
        assert_eq!(invariants::summary_dispatches(&fixture), dispatches);
        opened.send(&quit_frame());
        assert!(opened.finish().status.success());
    }
}

#[test]
fn committed_scheduled_request_keeps_reopened_queue_paused() {
    use plakous_hosted::sessions::{ProductCompactReason, QueueItem};
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let commits = pilot(&seed, &session, shape);
    for reason in [
        ProductCompactReason::Scheduled,
        ProductCompactReason::CapacityRecovery,
    ] {
        let fixture = workflow_cut_from(&seed, &session, shape, &commits, 12);
        let paths = summary::paths(&fixture);
        let mut record = receipt_with_reason(&fixture, &session, reason);
        record
            .pending_queue
            .push(QueueItem::new("queued prompt that fits"));
        save_session(&paths, &mut record).unwrap();
        let mut opened = traced_reopen(&fixture, &session, "receipt-queue-drain", None)
            .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
        let queue = opened.wait_for("restored queue", |row| row["kind"] == "queue");
        assert_eq!(queue["drain"], "restored_paused");
        let after = load_session(&paths, &session).unwrap();
        assert_eq!(summary::read_converged(&fixture).drain, "restored_paused");
        assert_eq!(after.pending_queue, record.pending_queue);
        assert_eq!(after.root_turn_count, record.root_turn_count);
        assert_eq!(after.compaction_count, 1);
        opened.send(&quit_frame());
        assert!(opened.finish().status.success());
    }
}

/// A workflow request is not authority: the runtime closes
/// the admitted compaction at open, the projector's one recovery line is the
/// only notice, and the abandoned request leaves no parked result, so the
/// next `/compact` is a new operation (spec 02).
#[test]
fn admitted_request_yields_the_runtime_closure_notice_and_no_parked_result() {
    let (_seed, fixture, session) = workflow_cut(10);
    let before = invariants::summary_dispatches(&fixture);
    let mut opened = traced_reopen(&fixture, &session, "recovery-required-fallback", None)
        .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
    assert_eq!(interrupted_compact_notices(&fixture), 1);
    assert_eq!(compact_rows(&fixture).len(), 1);
    opened.send(&json!({"v":1,"kind":"compact"}));
    opened.wait_for("new explicit compaction", |row| {
        row["kind"] == "tool"
            && row["activity_kind"] == "compact"
            && matches!(row["state"].as_str(), Some("done" | "failed"))
    });
    opened.send(&quit_frame());
    assert!(opened.finish().status.success());
    assert_eq!(interrupted_compact_notices(&fixture), 1);
    assert_eq!(
        invariants::summary_dispatches(&fixture).len(),
        before.len() + 1
    );
}

#[test]
fn admitted_request_rehearsal_does_not_claim_unadmitted_withdrawal() {
    let (_seed, fixture, session) = workflow_cut(10);
    let paths = summary::paths(&fixture);
    rusqlite::Connection::open(paths.product_database())
        .unwrap()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let reader = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .query_row("SELECT count(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    let result = summary::rehearse(&fixture, &session);
    // The runtime closes the admitted workflow at open whatever the product
    // recorded about it (spec 01 §5.5).
    assert_eq!(result["verdict"], "ResolvesCompactionWorkflow", "{result}");
    assert_eq!(result["success"], true, "{result}");
    let mut opened = traced_reopen(&fixture, &session, "recovery-required-rehearsal", None)
        .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
    opened.send(&quit_frame());
    assert!(opened.finish().status.success());
    rehearse_recovered_head(&fixture, &session);
}

/// Baseline: `PLAK_PREVIOUS_BINARY` must be an all-features test-hooks `plak`
/// built from the chosen base; the expected delta is specific to that base.
#[test]
#[ignore = "opt-in base-vs-new probe; requires PLAK_PREVIOUS_BINARY"]
fn previously_accepted_workflow_cuts_match_previous_binary() {
    let previous = std::env::var_os("PLAK_PREVIOUS_BINARY").expect("previous test-hooks binary");
    let shape = "scheduled_compaction_workflow";
    let (seed, session) = setup(shape);
    let cuts = pilot(&seed, &session, shape);
    let mut widened = Vec::new();
    for cut in &cuts {
        let (killed, _) = kill_fixture(&seed, &session, shape, &cuts, cut);
        let before_head = summary::read(&killed).head;
        let script: Value =
            serde_json::from_slice(&std::fs::read(killed.script()).unwrap()).unwrap();
        let old = killed.fork(&script);
        let new = killed.fork(&script);
        let old_open = probe_reopen(&old, &session, Some(&previous));
        let mut new_open = probe_reopen(&new, &session, None).unwrap_or_else(|outcome| {
            panic!(
                "new {}:{} refused: {}",
                cut.store, cut.store_ordinal, outcome.stderr
            )
        });
        new_open.send(&quit_frame());
        assert!(new_open.finish().status.success());
        match old_open {
            Ok(mut old_open) => {
                old_open.send(&quit_frame());
                assert!(old_open.finish().status.success());
                let mut old_summary = summary::read_converged(&old);
                let mut new_summary = summary::read_converged(&new);
                // Attach/quit CAS saves are not compaction outcomes.
                old_summary.revision = 0;
                new_summary.revision = 0;
                normalize_probe_head(&mut old_summary, &before_head);
                normalize_probe_head(&mut new_summary, &before_head);
                assert_eq!(
                    new_summary, old_summary,
                    "{}:{}",
                    cut.store, cut.store_ordinal
                );
                let old_record = load_session(&summary::paths(&old), &session).unwrap();
                let new_record = load_session(&summary::paths(&new), &session).unwrap();
                assert_eq!(
                    new_record.compaction_count, old_record.compaction_count,
                    "{}:{} count",
                    cut.store, cut.store_ordinal
                );
                assert_eq!(
                    compact_rows(&new),
                    compact_rows(&old),
                    "{}:{} compact rows",
                    cut.store,
                    cut.store_ordinal
                );
                assert_eq!(
                    interrupted_compact_notices(&new),
                    interrupted_compact_notices(&old),
                    "{}:{} notices",
                    cut.store,
                    cut.store_ordinal
                );
                eprintln!("probe {}:{} ACCEPTED_EXACT", cut.store, cut.store_ordinal);
            }
            Err(outcome) => {
                assert_eq!(outcome.status.code(), Some(5), "{}", outcome.stderr);
                widened.push((cut.store.clone(), cut.store_ordinal));
                eprintln!(
                    "probe {}:{} REFUSED_TO_RECOVERED",
                    cut.store, cut.store_ordinal
                );
            }
        }
    }
    assert_eq!(
        widened,
        [
            ("runtime".to_owned(), 10),
            ("runtime".to_owned(), 11),
            ("runtime".to_owned(), 12)
        ]
    );
    eprintln!("probe {} cuts: only runtime:10/11/12 widened", cuts.len());
}

fn normalize_probe_head(summary: &mut summary::Summary, before_head: &str) {
    // Generic root abandonment mints a fresh checkpoint per owner, unlike
    // deterministic compaction receipts. Preserve the source/successor distinction.
    if summary.head == before_head {
        return;
    }
    let head = std::mem::replace(&mut summary.head, "recovered_head".to_owned());
    for locator in [&mut summary.locator] {
        if locator.as_deref() == Some(&head) {
            *locator = Some("recovered_head".to_owned());
        }
    }
    for workflow in &mut summary.workflow_requests {
        if workflow["target"] == head {
            workflow["target"] = json!("recovered_head");
        }
    }
}

fn probe_reopen(
    fixture: &Fixture,
    session: &str,
    binary: Option<&std::ffi::OsStr>,
) -> Result<AgentProcess, AgentOutcome> {
    reopen_configured(fixture, session, |command, _| {
        if let Some(binary) = binary {
            let mut replacement = std::process::Command::new(binary);
            replacement
                .args(command.get_args())
                .current_dir(command.get_current_dir().unwrap())
                .env_clear();
            for (key, value) in command.get_envs() {
                if let Some(value) = value {
                    replacement.env(key, value);
                }
            }
            *command = replacement;
        }
    })
}
