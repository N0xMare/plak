//! Named product-store persistence benchmark.
//!
//! A long session's feed-derived rows through the projector commit port:
//! every call writes a `running` row and then its terminal row, and every
//! thirtieth call ends a turn with a user and an assistant line. The report
//! states per-row latency over the whole
//! session and over its first and last windows, so growth with the transcript
//! shows as a ratio, plus the database size. Wall time on a shared machine is
//! evidence, not a gate. Count fsyncs by running the bench under `strace -c`;
//! The benchmark entry point may place the store on a tmpfs to measure the
//! work without the disk.
//!
//! ```text
//! cargo bench -p plak --bench product_store --features test-support
//! ```

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "standalone benchmark binary, not library surface"
)]

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::home::paths::PlakPaths;
use crate::store::session_records::{SessionRecordStore, SqliteSessionStore};
use crate::store::sessions::{SessionRecord, ToolItem};
use plakous::sessions::feed::FeedSeq;
use plakous_hosted::sessions::projector::{Lane, RowKey, SessionViews, ViewWrites};

const CALLS: usize = 1_000;
const CALLS_PER_TURN: usize = 30;
const WINDOW: usize = 200;

pub(crate) fn run(directory: Option<&Path>) -> String {
    // A caller-selected directory may be a tmpfs where fsync is free and only
    // the per-event work is left to measure. Environment discovery belongs to
    // the benchmark executable rather than the product library.
    let home = directory
        .map_or_else(tempfile::tempdir, tempfile::tempdir_in)
        .expect("benchmark home");
    let paths = PlakPaths::explicit(home.path().join("home"));
    let store = SqliteSessionStore::new(paths.clone());
    let mut session = SessionRecord::new(home.path().join("workspace"));
    session.account = Some("codex".into());
    session.model = Some("offline-loopback-v1".into());
    session.effort = Some("medium".into());
    store.save(&mut session).expect("first commit");
    let mut views = SessionViews::empty();

    let mut events = Vec::with_capacity(CALLS * 2);
    let started = Instant::now();
    for call in 0..CALLS {
        if call % CALLS_PER_TURN == 0 {
            let key = RowKey::new(views.cursor.get() + 1, Lane::Runtime, 0);
            persist(
                &store,
                &session.id,
                &mut views,
                key,
                crate::store::sessions::TranscriptRow::user(format!(
                    "turn {}",
                    call / CALLS_PER_TURN
                )),
            );
        }
        let call_id = format!("call-{call:06}");
        let key = RowKey::new(views.cursor.get() + 1, Lane::Runtime, 0);
        for state in ["running", "succeeded"] {
            let activity = activity(&call_id, state);
            let event = Instant::now();
            persist(
                &store,
                &session.id,
                &mut views,
                key,
                crate::store::sessions::TranscriptRow::Tool(activity),
            );
            events.push(event.elapsed());
        }
        if call % CALLS_PER_TURN == CALLS_PER_TURN - 1 {
            let key = RowKey::new(views.cursor.get() + 1, Lane::Runtime, 0);
            persist(
                &store,
                &session.id,
                &mut views,
                key,
                crate::store::sessions::TranscriptRow::assistant("done. ".repeat(40)),
            );
        }
    }
    let total = started.elapsed();
    let bytes = ["", "-wal"]
        .iter()
        .filter_map(|suffix| {
            let path = format!("{}{suffix}", paths.product_database().display());
            std::fs::metadata(path).ok().map(|metadata| metadata.len())
        })
        .sum::<u64>();
    let reloaded = store.load(&session.id).expect("reload");
    assert_eq!(reloaded.transcript.len(), views.rows.len());

    let mut report = String::new();
    let _ = writeln!(
        report,
        "product store · {} tool events · {} transcript lines",
        events.len(),
        views.rows.len()
    );
    let _ = writeln!(report, "total            {:>10.1} ms", millis(total));
    let _ = writeln!(
        report,
        "event p50        {:>10.3} ms",
        percentile(&events, 0.5)
    );
    let _ = writeln!(
        report,
        "event p99        {:>10.3} ms",
        percentile(&events, 0.99)
    );
    let _ = writeln!(
        report,
        "event max        {:>10.3} ms",
        percentile(&events, 1.0)
    );
    let _ = writeln!(
        report,
        "first {WINDOW} median {:>10.3} ms",
        percentile(&events[..WINDOW], 0.5)
    );
    let _ = writeln!(
        report,
        "last {WINDOW} median  {:>10.3} ms",
        percentile(&events[events.len() - WINDOW..], 0.5)
    );
    let _ = writeln!(report, "database bytes   {bytes:>10}");
    report
}

fn activity(call_id: &str, state: &str) -> ToolItem {
    ToolItem {
        call_id: call_id.to_owned(),
        name: "read_file".to_owned(),
        exposed_name: None,
        kind: plakous_host_protocol::views::presentation::ActivityKind::Tool,
        state: state.to_owned(),
        subject: Some(format!("crates/example/src/{call_id}.rs")),
        detail: Some("fn example() {}\n".repeat(20)),
        started_at_ms: Some(1),
        finished_at_ms: (state != "running").then_some(2),
        parent_call_id: None,
        cell_id: None,
        wait: None,
    }
}

/// One keyed row update under the projector cursor CAS.
fn persist(
    store: &SqliteSessionStore,
    session_id: &str,
    views: &mut SessionViews,
    key: RowKey,
    row: crate::store::sessions::TranscriptRow,
) {
    let expected_cursor = views.cursor;
    views.cursor = FeedSeq::new(expected_cursor.get() + 1);
    views.rows.insert(key, row.clone());
    store
        .commit_projection_views(
            session_id,
            &ViewWrites {
                successors: Vec::new(),
                expected_cursor,
                cursor: views.cursor,
                upserts: vec![(key, row)],
                deletes: vec![],
                calls: vec![],
                open_turns: vec![],
                processes: vec![],
            },
            views,
        )
        .expect("projector commit");
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn percentile(values: &[Duration], fraction: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "an index into a short benchmark sample"
    )]
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    millis(sorted[index])
}
