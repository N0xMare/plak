//! Process-isolated contracts: environment controls never mutate the test runner.

#![allow(
    clippy::expect_used,
    reason = "child fixtures fail at exact assertion sites"
)]

use std::error::Error;
use std::fs;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use plakous_agent::internal::{
    AcquireBranch, CreateSession, LeaseClock, LeaseDomain, LeaseInstant, RuntimeDurability,
    SessionRef, SessionStore, SessionStoreError, Timestamp,
};
use plakous_model::{BranchId, RunId, SessionId};
use plakous_store::SqliteSessionStore;
use plakous_store::commit_trace::{KILL_ENV, SettleGuard, TRACE_ENV, install, label, settle};
use rusqlite::Connection;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;
const CHILD_ENV: &str = "PLAK_SW1_TRACE_CHILD";
const ROOT_ENV: &str = "PLAK_SW1_TRACE_ROOT";

fn database(root: &Path, name: &str) -> rusqlite::Result<Connection> {
    let connection = Connection::open(root.join(format!("{name}.sqlite3")))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
         CREATE TABLE a(id INTEGER PRIMARY KEY, value TEXT);
         CREATE TABLE b(id INTEGER PRIMARY KEY);
         CREATE TABLE w(id TEXT PRIMARY KEY) WITHOUT ROWID;",
    )?;
    Ok(connection)
}

fn child(
    mode: &str,
    kill: Option<&str>,
    traced: bool,
) -> Result<(tempfile::TempDir, Output), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", "commit_trace::child_entry", "--nocapture"])
        .env(CHILD_ENV, mode)
        .env(ROOT_ENV, root.path())
        .env_remove(TRACE_ENV)
        .env_remove(KILL_ENV);
    if traced {
        command.env(TRACE_ENV, root.path().join("trace.jsonl"));
    }
    if let Some(kill) = kill {
        command.env(KILL_ENV, kill);
    }
    let output = command.output()?;
    Ok((root, output))
}

fn successful(output: &Output) {
    assert!(output.status.success(), "child failed: {output:?}");
}

fn lines(root: &Path) -> Result<Vec<Value>, Box<dyn Error>> {
    fs::read_to_string(root.join("trace.jsonl"))?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[test]
fn inert_without_env_preserves_hooks_and_ignores_kill() -> TestResult {
    let (root, output) = child("inert", Some("runtime:1"), false)?;
    successful(&output);
    assert!(!root.path().join("trace.jsonl").exists());
    Ok(())
}

#[test]
fn trace_json_lines_have_sorted_tables_and_global_and_store_ordinals() -> TestResult {
    let (root, output) = child("format", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 4);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line.as_object().expect("object").len(), 7);
        assert_eq!(line["ordinal"], index + 1);
        assert_eq!(line["indexed"], true);
    }
    assert_eq!(lines[0]["tables"], serde_json::json!(["a", "b"]));
    assert_eq!(lines[0]["label"], "quote\"\\\n");
    assert_eq!(lines[0]["store"], "runtime");
    assert_eq!(lines[0]["store_index"], 1);
    assert_eq!(lines[0]["store_ordinal"], 1);
    assert_eq!(lines[1]["store_index"], 2);
    assert_eq!(lines[1]["store_ordinal"], 2);
    assert_eq!(lines[2]["store"], "product");
    assert_eq!(lines[2]["store_index"], 1);
    assert_eq!(lines[2]["store_ordinal"], 1);
    assert_eq!(lines[3]["store"], "tree");
    assert_eq!(lines[3]["store_index"], 1);
    assert_eq!(lines[3]["store_ordinal"], 1);
    Ok(())
}

#[cfg(unix)]
#[test]
fn kill_at_k_fsyncs_the_line_and_leaves_exactly_k_durable_writes() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;
    for k in [1_u64, 2, 17] {
        let (root, output) = child("two_stores", Some(&format!("runtime:{k}")), true)?;
        assert_eq!(output.status.signal(), Some(9), "{output:?}");
        let lines = lines(root.path())?;
        assert_eq!(lines.len(), usize::try_from(k)?);
        assert_eq!(lines.last().expect("last")["ordinal"], k);
        let mut durable = 0;
        for name in ["left", "right"] {
            let connection = Connection::open(root.path().join(format!("{name}.sqlite3")))?;
            durable +=
                connection.query_row("SELECT count(*) FROM a", [], |r| r.get::<_, i64>(0))?;
        }
        assert_eq!(durable, i64::try_from(k)?);
    }
    Ok(())
}

#[test]
fn semaphore_serializes_two_store_connections() -> TestResult {
    let (root, output) = child("two_stores", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 256);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["ordinal"], index + 1);
        assert_eq!(line["store_index"], index + 1);
        assert_eq!(line["store_ordinal"], index + 1);
    }
    Ok(())
}

#[test]
fn runtime_renew_and_revive_are_traced_but_not_indexed_or_targeted() -> TestResult {
    let (root, output) = child("leases", Some("runtime:4"), true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0]["label"], "create");
    for (index, name) in [(3, "renew"), (4, "revive")] {
        assert_eq!(lines[index]["label"], name);
        assert_eq!(lines[index]["indexed"], false);
        assert_eq!(lines[index]["ordinal"], 3);
        assert_eq!(lines[index]["store_ordinal"], 3);
        assert_eq!(lines[index]["store_index"], index + 1);
    }
    assert!(lines.iter().all(|line| line["store"] == "runtime"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn unindexed_commits_before_the_first_target_have_ordinal_zero() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;
    let (root, output) = child("unindexed", Some("runtime:1"), true)?;
    assert_eq!(output.status.signal(), Some(9), "{output:?}");
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 3);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["ordinal"], usize::from(index == 2));
        assert_eq!(line["store_ordinal"], usize::from(index == 2));
        assert_eq!(line["indexed"], index == 2);
        assert_eq!(line["store_index"], index + 1);
    }
    Ok(())
}

#[test]
fn autocheckpoint_is_retained_after_wal_hook_installation() -> TestResult {
    let (_, output) = child("checkpoint", None, true)?;
    successful(&output);
    Ok(())
}

#[test]
fn passive_checkpoint_does_not_wait_for_a_reader_or_lose_the_semaphore() -> TestResult {
    let (_, output) = child("checkpoint_reader", None, true)?;
    successful(&output);
    Ok(())
}

#[test]
fn invalid_kill_control_is_a_reported_divergence() -> TestResult {
    for target in [
        "runtime:0",
        "unknown:1",
        "1",
        "runtime",
        "runtime:1:2",
        "runtime:+1",
    ] {
        let (_, output) = child("format", Some(target), true)?;
        assert_eq!(output.status.code(), Some(86), "{target}: {output:?}");
        assert!(String::from_utf8(output.stderr)?.contains("DIVERGENT commit-trace"));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn store_kill_targets_ignore_cross_store_interleaving() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;
    for mode in ["mixed_runtime_first", "mixed_product_first"] {
        for store in ["runtime", "product"] {
            let (root, output) = child(mode, Some(&format!("{store}:2")), true)?;
            assert_eq!(
                output.status.signal(),
                Some(9),
                "{mode} {store}: {output:?}"
            );
            let lines = lines(root.path())?;
            let last = lines.last().expect("target line");
            assert_eq!(last["store"], store);
            assert_eq!(last["store_ordinal"], 2);
            assert_eq!(last["ordinal"], lines.len());
            assert_eq!(
                lines.iter().filter(|line| line["store"] == store).count(),
                2
            );
            let connection = Connection::open(root.path().join(format!("{store}.sqlite3")))?;
            assert_eq!(
                connection.query_row("SELECT count(*) FROM a", [], |row| row.get::<_, i64>(0))?,
                2
            );
        }
    }
    Ok(())
}

#[test]
fn rollback_clears_tables_and_allows_the_next_store_commit() -> TestResult {
    let (root, output) = child("rollback", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["tables"], serde_json::json!(["b"]));
    assert_eq!(lines[1]["tables"], serde_json::json!(["a"]));
    Ok(())
}

#[test]
fn frameless_raw_commit_releases_without_a_trace_line() -> TestResult {
    let (root, output) = child("frameless_raw", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ordinal"], 1);
    assert_eq!(lines[1]["ordinal"], 2);
    Ok(())
}

#[test]
fn frameless_commit_unwind_releases_for_another_thread() -> TestResult {
    let (root, output) = child("frameless_unwind", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ordinal"], 1);
    assert_eq!(lines[1]["ordinal"], 2);
    Ok(())
}

#[test]
fn frameless_idempotent_release_allows_the_next_acquire() -> TestResult {
    let (root, output) = child("frameless_release", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[1]["label"], "release");
    assert_eq!(lines[2]["label"], "acquire");
    assert_eq!(lines[2]["ordinal"], 3);
    Ok(())
}

#[test]
fn frameless_commit_releases_for_another_thread_and_store() -> TestResult {
    let (root, output) = child("frameless_thread", None, true)?;
    successful(&output);
    let lines = lines(root.path())?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ordinal"], 1);
    assert_eq!(lines[1]["ordinal"], 2);
    Ok(())
}

#[test]
fn child_entry() -> TestResult {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return Ok(());
    };
    let root = std::env::var_os(ROOT_ENV).ok_or("root")?;
    let root = Path::new(&root);
    match mode.as_str() {
        "inert" => {
            let connection = database(root, "inert")?;
            let calls = Arc::new(AtomicU64::new(0));
            let hook_calls = calls.clone();
            connection.commit_hook(Some(move || {
                hook_calls.fetch_add(1, Ordering::Relaxed);
                false
            }))?;
            assert!(!install(&connection, "runtime")?);
            connection.execute("INSERT INTO a(value) VALUES ('one')", [])?;
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
        "format" => run_format(root)?,
        "mixed_runtime_first" | "mixed_product_first" => run_mixed(root, &mode)?,
        "two_stores" => run_two_stores(root)?,
        "leases"
        | "frameless_release"
        | "frameless_runtime_release_cross"
        | "frameless_runtime_held_cross"
        | "frameless_runtime_renew_cross" => run_leases(root, &mode)?,
        "frameless_raw" | "frameless_thread" => {
            run_frameless(root, mode == "frameless_thread", true)?;
        }
        "frameless_unwind" => {
            let connection = database(root, "frameless")?;
            install(&connection, "runtime")?;
            connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _settle = SettleGuard;
                connection
                    .execute_batch("BEGIN IMMEDIATE; SELECT * FROM a; COMMIT;")
                    .expect("frameless commit");
                panic!("operation unwind");
            }));
            assert!(panic.is_err());
            let other = database(root, "other")?;
            std::thread::spawn(move || -> rusqlite::Result<()> {
                install(&other, "tree")?;
                other.execute("INSERT INTO a DEFAULT VALUES", [])?;
                Ok(())
            })
            .join()
            .expect("peer thread")?;
        }
        "frameless_reentry" => run_frameless(root, false, false)?,
        "unindexed" => {
            let connection = database(root, "unindexed")?;
            install(&connection, "runtime")?;
            for name in ["renew", "revive", "commit"] {
                let _label = label(name);
                connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
            }
        }
        "checkpoint" => run_checkpoint(root, false)?,
        "checkpoint_reader" => run_checkpoint(root, true)?,
        "rollback" => {
            let connection = database(root, "rollback")?;
            install(&connection, "runtime")?;
            connection.execute_batch("BEGIN; INSERT INTO a DEFAULT VALUES; ROLLBACK;")?;
            connection.execute("INSERT INTO b DEFAULT VALUES", [])?;
            connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
        }
        _ => return Err("unknown child mode".into()),
    }
    Ok(())
}

#[test]
fn same_connection_reclaims_a_frameless_commit_without_settle() -> TestResult {
    let (root, output) = child("frameless_reentry", None, true)?;
    successful(&output);
    assert_eq!(lines(root.path())?.len(), 2);
    Ok(())
}

#[test]
fn runtime_frameless_operations_release_before_another_store_thread_commits() -> TestResult {
    for mode in [
        "frameless_runtime_release_cross",
        "frameless_runtime_held_cross",
        "frameless_runtime_renew_cross",
    ] {
        let (root, output) = child(mode, None, true)?;
        successful(&output);
        let lines = lines(root.path())?;
        assert_eq!(lines.len(), 6, "{mode}: {lines:?}");
        assert_eq!(
            lines.iter().filter(|line| line["label"] == "renew").count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line["label"] == "create")
                .count(),
            2
        );
    }
    Ok(())
}

fn run_frameless(root: &Path, other_thread: bool, explicitly_settle: bool) -> TestResult {
    let connection = database(root, "frameless")?;
    install(&connection, "runtime")?;
    connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
    connection.execute_batch("BEGIN IMMEDIATE; SELECT * FROM a; COMMIT;")?;
    if explicitly_settle {
        settle();
    }
    if other_thread {
        let other = database(root, "other")?;
        std::thread::spawn(move || -> rusqlite::Result<()> {
            install(&other, "runtime")?;
            other.execute("INSERT INTO a DEFAULT VALUES", [])?;
            Ok(())
        })
        .join()
        .expect("other thread")?;
    } else {
        connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
    }
    Ok(())
}

fn run_checkpoint(root: &Path, hold_reader: bool) -> TestResult {
    let connection = database(root, "checkpoint")?;
    install(&connection, "runtime")?;
    let reader = Connection::open(root.join("checkpoint.sqlite3"))?;
    if hold_reader {
        reader.execute_batch("BEGIN; SELECT * FROM a;")?;
    }
    // A single commit well over the normal 1000-page threshold.
    connection.execute("INSERT INTO a(value) VALUES (zeroblob(5000000))", [])?;
    let pages: i64 = connection.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    assert!(pages > 1000);
    if hold_reader {
        // The blocked PASSIVE checkpoint returned; another commit still works.
        connection.execute("INSERT INTO b DEFAULT VALUES", [])?;
        reader.execute_batch("ROLLBACK;")?;
        connection.execute("INSERT INTO b DEFAULT VALUES", [])?;
    }
    // PASSIVE copied the durable pages into the main file while still open.
    let page_size: i64 = connection.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    assert_eq!(
        fs::metadata(root.join("checkpoint.sqlite3"))?.len(),
        u64::try_from(pages * page_size)?
    );
    connection.execute("INSERT INTO b DEFAULT VALUES", [])?;
    let wal_size = fs::metadata(root.join("checkpoint.sqlite3-wal"))?.len();
    assert!(wal_size < 6_000_000, "WAL was not reused: {wal_size}");
    Ok(())
}

struct Clock(AtomicU64);

impl LeaseClock for Clock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(
            LeaseDomain::new([1; 16], [2; 32]),
            self.0.load(Ordering::Relaxed),
            0,
        )
    }
}

fn run_leases(root: &Path, mode: &str) -> TestResult {
    use super::conformance::{bundle, checkpoint, id, topology};
    let clock = Arc::new(Clock(AtomicU64::new(100)));
    let store = SqliteSessionStore::open_with_clock(root.join("leases.sqlite3"), clock.clone())?;
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    runtime.block_on(async {
        let reference = SessionRef::new(id::<SessionId>(1)?, id::<BranchId>(2)?);
        let checkpoint = checkpoint(
            topology(
                reference.session_id(),
                reference.branch_id(),
                id(3)?,
                None,
                0,
                None,
                1,
            )?,
            1,
            RuntimeDurability::Required,
        )?;
        let request = CreateSession::new(
            reference,
            Timestamp::new(50, 0)?,
            id::<RunId>(4)?,
            NonZeroU64::new(10).ok_or("duration")?,
            bundle(&checkpoint, None)?,
        );
        let created = store.create(request.clone()).await?;
        store.release(created.lease().clone()).await?;
        if matches!(
            mode,
            "frameless_release" | "frameless_runtime_release_cross"
        ) {
            store.release(created.lease().clone()).await?;
        }
        if mode == "frameless_runtime_release_cross" {
            peer_runtime_create(root, clock.clone(), request.clone())?;
        }
        let lease = store
            .acquire(AcquireBranch::new(
                reference,
                id::<RunId>(5)?,
                NonZeroU64::new(10).ok_or("duration")?,
            ))
            .await?;
        if mode == "frameless_runtime_held_cross" {
            let other =
                SqliteSessionStore::open_with_clock(root.join("leases.sqlite3"), clock.clone())?;
            assert_eq!(
                other
                    .acquire(AcquireBranch::new(
                        reference,
                        id::<RunId>(6)?,
                        NonZeroU64::new(10).ok_or("duration")?
                    ))
                    .await,
                Err(SessionStoreError::LeaseHeld)
            );
            peer_runtime_create(root, clock.clone(), request.clone())?;
        }
        if mode == "frameless_runtime_renew_cross" {
            assert_eq!(store.renew(lease.clone()).await?, lease);
            peer_runtime_create(root, clock.clone(), request)?;
        }
        clock.0.store(105, Ordering::Relaxed);
        let renewed = store.renew(lease).await?;
        clock.0.store(500, Ordering::Relaxed);
        store.revive(renewed).await?;
        Ok(())
    })
}

fn peer_runtime_create(root: &Path, clock: Arc<Clock>, request: CreateSession) -> TestResult {
    let path = root.join("peer.sqlite3");
    std::thread::spawn(move || {
        let store = SqliteSessionStore::open_with_clock(path, clock)?;
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(store.create(request))?;
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .join()
    .expect("peer runtime")
    .map_err(|error| error as Box<dyn Error>)
}

fn run_format(root: &Path) -> TestResult {
    let runtime = database(root, "runtime")?;
    install(&runtime, "runtime")?;
    {
        let _label = label("quote\"\\\n");
        runtime.execute_batch("BEGIN; INSERT INTO b DEFAULT VALUES; INSERT INTO a DEFAULT VALUES; INSERT INTO w VALUES ('hidden'); COMMIT;")?;
    }
    runtime.execute("INSERT INTO a DEFAULT VALUES", [])?;
    for name in ["product", "tree"] {
        let connection = database(root, name)?;
        install(&connection, name)?;
        connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
    }
    Ok(())
}

fn run_mixed(root: &Path, mode: &str) -> TestResult {
    let runtime = database(root, "runtime")?;
    let product = database(root, "product")?;
    install(&runtime, "runtime")?;
    install(&product, "product")?;
    for _ in 0..3 {
        let order = if mode == "mixed_runtime_first" {
            [&runtime, &product]
        } else {
            [&product, &runtime]
        };
        for connection in order {
            connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
        }
    }
    Ok(())
}

fn run_two_stores(root: &Path) -> TestResult {
    let connections = [database(root, "left")?, database(root, "right")?];
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = connections
        .into_iter()
        .map(|connection| {
            let barrier = barrier.clone();
            std::thread::spawn(move || -> rusqlite::Result<()> {
                install(&connection, "runtime")?;
                barrier.wait();
                for _ in 0..128 {
                    let _label = label("commit");
                    connection.execute("INSERT INTO a DEFAULT VALUES", [])?;
                }
                Ok(())
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker")?;
    }
    Ok(())
}
