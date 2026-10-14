//! Deterministic JSONL workload for semantic-store growth and recovery.

#![allow(
    clippy::expect_used,
    clippy::print_stdout,
    reason = "benchmark fixtures fail at exact assertions"
)]

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use plakous_agent::internal::{
    BranchHead, CheckpointId, CheckpointObjectBundle, CommitCheckpoint, CreateSession,
    RuntimeDurability, RuntimeObject, SessionRef, SessionStore, SessionStoreError, Timestamp,
};
use plakous_model::{BranchId, Digest, DigestDomain, RunId, SessionId};
use plakous_store::{
    MemorySessionStore, MemoryStoreLimits, SqliteSessionStore, SqliteStoreOptions,
};
use rusqlite::Connection;
use serde_json::{Value, json};

#[allow(
    dead_code,
    unused_imports,
    reason = "the shared fixture module also contains imports used only by its integration-test entry points"
)]
#[path = "../tests/integration/conformance.rs"]
mod conformance;

const SCHEMA: &str = "plakous.perf-store.v6";
const FIXTURE: &str = "linear-semantic-checkpoint-trace-v3";
const BASELINE: &str = "unratified-raw-distribution-v1";
const DEFAULT_TURN_SWEEP: &[u64] = &[32, 64, 128];
const SMALL_TURN_SWEEP: &[u64] = &[4, 8];
const LONG_TURN_SWEEP: &[u64] = &[1_024];
const MAX_OBJECTS_PER_COMMIT: usize = 12;
const SATURATION_QUEUE_CAPACITY: usize = 1;
const SATURATION_ATTEMPTS: u64 = 8;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

type BenchResult<T> = Result<T, Box<dyn Error>>;

struct Database {
    directory: PathBuf,
    path: PathBuf,
}

impl Database {
    fn new() -> BenchResult<Self> {
        let directory = std::env::temp_dir().join(format!(
            "plakous-perf-store-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory)?;
        let path = directory.join("sessions.sqlite3");
        Ok(Self { directory, path })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct Seed {
    selector: SessionRef,
    checkpoint_id: CheckpointId,
    bundle: CheckpointObjectBundle,
    request: CreateSession,
}

struct Trace {
    head: BranchHead,
    last_checkpoint: CheckpointId,
    object_count: usize,
    object_bytes: usize,
    new_object_count: usize,
    new_object_bytes: usize,
    commit_ns: Vec<u64>,
    publication_build_ns: Vec<u64>,
    publication_clone_ns: Vec<u64>,
    post_create_storage_bytes: Option<u64>,
    post_create_database_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SqliteRowCounts {
    runtime_objects: u64,
    runtime_object_edges: u64,
    checkpoints: u64,
    sessions: u64,
    branches: u64,
    create_receipts: u64,
    commit_receipts: u64,
    fork_receipts: u64,
    archive_receipts: u64,
}

impl SqliteRowCounts {
    fn total(self) -> u64 {
        self.runtime_objects
            .saturating_add(self.runtime_object_edges)
            .saturating_add(self.checkpoints)
            .saturating_add(self.sessions)
            .saturating_add(self.branches)
            .saturating_add(self.create_receipts)
            .saturating_add(self.commit_receipts)
            .saturating_add(self.fork_receipts)
            .saturating_add(self.archive_receipts)
    }

    fn json(self) -> Value {
        json!({
            "runtime_objects": self.runtime_objects,
            "runtime_object_edges": self.runtime_object_edges,
            "checkpoints": self.checkpoints,
            "sessions": self.sessions,
            "branches": self.branches,
            "create_receipts": self.create_receipts,
            "commit_receipts": self.commit_receipts,
            "fork_receipts": self.fork_receipts,
            "archive_receipts": self.archive_receipts,
            "total_rows_in_semantic_tables": self.total(),
        })
    }
}

struct Options {
    turn_sweep: &'static [u64],
    dry_run: bool,
    large_store_admission: bool,
    restart_probe: Option<(PathBuf, u64)>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> BenchResult<()> {
    let options = options()?;
    if let Some((path, turns)) = &options.restart_probe {
        return run_sqlite_restart_probe(path, *turns).await;
    }
    emit_metadata(&options);
    if options.dry_run {
        println!(
            "{}",
            json!({
                "schema": SCHEMA,
                "kind": "dry-run",
                "fixture": FIXTURE,
                "turn_sweep": options.turn_sweep,
                "sqlite_actor_saturation_attempts": SATURATION_ATTEMPTS,
                "sqlite_actor_queue_capacity": SATURATION_QUEUE_CAPACITY,
            })
        );
        return Ok(());
    }
    if options.large_store_admission {
        return run_large_store_admission().await;
    }
    for &turns in options.turn_sweep {
        let memory = run_memory(turns).await?;
        let sqlite = run_sqlite(turns).await?;
        assert_eq!(memory.head.revision(), sqlite.head.revision());
        assert_eq!(memory.object_count, sqlite.object_count);
        assert_eq!(memory.new_object_count, sqlite.new_object_count);
        emit_comparison(turns, &memory, &sqlite);
    }
    for &turns in options.turn_sweep {
        run_sqlite_instrumented_trace(turns).await?;
    }
    run_sqlite_actor_saturation().await?;
    emit_observation_schema_proof().await?;
    Ok(())
}

fn options() -> BenchResult<Options> {
    let mut turn_sweep = DEFAULT_TURN_SWEEP;
    let mut dry_run = false;
    let mut large_store_admission = false;
    let mut restart_probe = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--small" => turn_sweep = SMALL_TURN_SWEEP,
            "--long" => turn_sweep = LONG_TURN_SWEEP,
            "--dry-run" => dry_run = true,
            "--large-store-admission" => large_store_admission = true,
            "--bench" => {}
            "--restart-probe" => {
                let path = arguments
                    .next()
                    .ok_or("--restart-probe requires a database path")?;
                let turns = arguments
                    .next()
                    .ok_or("--restart-probe requires a turn count")?
                    .parse()?;
                restart_probe = Some((PathBuf::from(path), turns));
            }
            _ => return Err(format!("unknown perf-store argument: {argument}").into()),
        }
    }
    Ok(Options {
        turn_sweep,
        dry_run,
        large_store_admission,
        restart_probe,
    })
}

fn emit_metadata(options: &Options) {
    let rustc_vv = command_output("rustc", &["-Vv"]);
    let git_status = git_output(&["status", "--porcelain=v1"]);
    let host_id = perf_host_id();
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "metadata",
            "fixture": FIXTURE,
            "host": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "family": std::env::consts::FAMILY,
                "available_parallelism": std::thread::available_parallelism().map(usize::from).ok(),
                "host_id": host_id,
            },
            "toolchain": {
                "rustc_vv": rustc_vv,
                "cargo_v": command_output("cargo", &["-V"]),
                "sqlite": rusqlite::version(),
            },
            "target": rustc_host(&rustc_vv),
            "cargo": {
                "package": "plakous-store",
                "bench": "perf_store",
                "profile": "bench",
                "features": ["sqlite"],
                "locked": true,
                "offline": true,
            },
            "source": {
                "git_commit": git_output(&["rev-parse", "HEAD"]),
                "git_dirty": git_status.as_ref().is_some_and(|status| !status.is_empty()),
                "worktree_status_digest": git_status.as_deref().and_then(command_bytes_digest),
                "tracked_worktree_patch_digest": git_stdout_digest(&["diff", "--binary", "HEAD"]),
                "non_ignored_worktree_content_closure": worktree_content_closure_metadata(),
                "benchmark_source_git_blob": git_output(&["hash-object", "crates/store/benches/perf_store.rs"]),
                "workspace_manifest_git_blob": git_output(&["hash-object", "Cargo.toml"]),
                "package_manifest_git_blob": git_output(&["hash-object", "crates/store/Cargo.toml"]),
                "cargo_lock_git_blob": git_output(&["hash-object", "Cargo.lock"]),
            },
            "baseline_identity": BASELINE,
            "turn_sweep": options.turn_sweep,
            "dry_run": options.dry_run,
            "sqlite_actor_saturation_attempts": SATURATION_ATTEMPTS,
            "sqlite_actor_queue_capacity": SATURATION_QUEUE_CAPACITY,
            "samples": "primary raw commit latency rows and summaries first; separate explicitly instrumented SQLite object/hash/transaction/queue/row/file traces second; typed actor-saturation outcomes last",
            "cold_warm_rule": "each scale uses a fresh backend; create/open is cold and commits share only that scale's ancestry",
            "assertions": {
                "same_trace_shape": "memory and SQLite retain identical object counts and final branch revision; byte equality is not claimed because the memory and SQLite contracts require different semantic durability dispositions",
                "accepted_semantic_mutations": "each successful create or commit returns its exact public receipt; SQLite's side-effect-free counter advances once for each such mutation",
                "durable_acknowledgment_boundary": "required-durability SQLite mutation receipts return only after the synchronous=FULL transaction commit boundary",
                "delta_object_limit": MAX_OBJECTS_PER_COMMIT,
                "sqlite_actor_bound": "a one-slot actor queue reports excess public create operations as SessionStoreError::BackendBusy",
                "recovery": "latest checkpoint, commit receipt, and branch head must agree after reopen",
                "instrumented_sqlite_commit": "the separate trace records actual actor-start queue wait, one explicit immediate transaction begin and successful commit call, and the CAS objects/bytes actually inserted",
            },
            "unavailable": {
                "fsync_syscall_trace": "FULL durable acknowledgment boundaries are counted; kernel fsync/fdatasync calls and latency are not traced",
                "allocator_counts": "workspace unsafe-code policy excludes an in-process global allocator counter",
                "exact_sql_object_write_latency": "the separate trace counts actual inserted CAS objects/bytes and total transaction/commit-call latency, but does not time each SQL object-row statement",
                "retained_rss": "a process-isolated SQLite restart probe emits Linux /proc/self/status VmRSS when available; other hosts emit null",
                "event_transaction_trace": "not measured; the separate schema-proof row is structural evidence only",
            }
        })
    );
}

fn perf_host_id() -> String {
    let value = std::env::var("PLAKOUS_PERF_HOST_ID")
        .expect("PLAKOUS_PERF_HOST_ID must identify the benchmark host");
    assert!(
        is_stable_host_id(&value),
        "PLAKOUS_PERF_HOST_ID must be a stable, non-placeholder host identifier"
    );
    value
}

fn is_stable_host_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "localhost" | "unknown" | "host" | "stable-host-id" | "<stable-host-id>"
        )
}

async fn run_memory(turns: u64) -> BenchResult<Trace> {
    let seed = seed(turns, RuntimeDurability::Disabled)?;
    let opened = Instant::now();
    let store = MemorySessionStore::new(MemoryStoreLimits::default())?;
    let open_ns = elapsed_ns(opened);
    let trace = run_trace(&store, &seed, turns, "memory", None).await?;
    let load_started = Instant::now();
    let loaded = store.load_checkpoint(trace.last_checkpoint).await?;
    let load_ns = elapsed_ns(load_started);
    let recovery_started = Instant::now();
    let recovered = store.recover_commit(trace.last_checkpoint).await?;
    let recovery_ns = elapsed_ns(recovery_started);
    assert_eq!(loaded.reference(), trace.head.checkpoint());
    assert_eq!(recovered.receipt().head(), trace.head);
    assert_eq!(store.load_branch(seed.selector).await?.head(), trace.head);
    let stats = store.stats()?;
    assert_eq!(stats.sessions(), 1);
    assert_eq!(stats.branches(), 1);
    assert_eq!(stats.checkpoints(), usize::try_from(turns + 1)?);
    assert_eq!(stats.unique_objects(), trace.object_count);
    assert_eq!(stats.unique_object_bytes(), trace.object_bytes);
    store.shutdown().await?;
    emit_summary(
        "memory",
        turns,
        open_ns,
        load_ns,
        recovery_ns,
        &Value::Null,
        &Value::Null,
        turns + 1,
        &trace,
    );
    Ok(trace)
}

async fn run_sqlite(turns: u64) -> BenchResult<Trace> {
    let database = Database::new()?;
    let seed = seed(turns, RuntimeDurability::Required)?;
    let opened = Instant::now();
    let store = SqliteSessionStore::open(&database.path)?;
    let open_ns = elapsed_ns(opened);
    let trace = run_trace(&store, &seed, turns, "sqlite", Some(&database.path)).await?;
    let baseline = trace
        .post_create_storage_bytes
        .ok_or("SQLite trace omitted its post-create storage baseline")?;
    let checkpointed_baseline = trace
        .post_create_database_bytes
        .ok_or("SQLite trace omitted its post-create database baseline")?;
    let active_bytes = storage_bytes(&database.path).saturating_sub(baseline);
    let stats = store.stats().await?;
    assert_eq!(stats.sessions(), 1);
    assert_eq!(stats.branches(), 1);
    assert_eq!(stats.checkpoints(), turns + 1);
    assert_eq!(stats.objects(), u64::try_from(trace.object_count)?);
    assert_eq!(stats.object_bytes(), u64::try_from(trace.object_bytes)?);
    assert_eq!(stats.semantic_mutations(), turns + 1);
    store.shutdown().await?;
    let checkpointed_bytes = database_bytes(&database.path).saturating_sub(checkpointed_baseline);
    drop(store);

    let reopen_started = Instant::now();
    let reopened = SqliteSessionStore::open(&database.path)?;
    let reopen_ns = elapsed_ns(reopen_started);
    let load_started = Instant::now();
    let loaded = reopened.load_checkpoint(trace.last_checkpoint).await?;
    let load_ns = elapsed_ns(load_started);
    let recovery_started = Instant::now();
    let recovered = reopened.recover_commit(trace.last_checkpoint).await?;
    let recovery_ns = elapsed_ns(recovery_started);
    assert_eq!(loaded.reference(), trace.head.checkpoint());
    assert_eq!(recovered.receipt().head(), trace.head);
    assert_eq!(
        reopened.load_branch(seed.selector).await?.head(),
        trace.head
    );
    reopened.shutdown().await?;
    drop(reopened);
    emit_restart_probe(&database.path, turns)?;
    emit_summary(
        "sqlite",
        turns,
        open_ns,
        load_ns,
        recovery_ns,
        &json!(reopen_ns),
        &json!({
            "active_db_wal_growth_bytes": active_bytes,
            "checkpointed_db_growth_bytes": checkpointed_bytes,
            "active_db_wal_growth_ratio": storage_growth_ratio(active_bytes, trace.new_object_bytes),
            "checkpointed_db_growth_ratio": storage_growth_ratio(checkpointed_bytes, trace.new_object_bytes),
        }),
        stats.semantic_mutations(),
        &trace,
    );
    Ok(trace)
}

fn emit_restart_probe(path: &Path, turns: u64) -> BenchResult<()> {
    let output = Command::new(std::env::current_exe()?)
        .arg("--restart-probe")
        .arg(path)
        .arg(turns.to_string())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "restart probe failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    print!("{}", String::from_utf8(output.stdout)?);
    Ok(())
}

async fn run_sqlite_restart_probe(path: &Path, turns: u64) -> BenchResult<()> {
    let host_id = perf_host_id();
    let rss_before_open = linux_current_rss_bytes();
    let opened = Instant::now();
    let store = SqliteSessionStore::open(path)?;
    let open_ns = elapsed_ns(opened);
    let rss_after_open = linux_current_rss_bytes();
    let seed = seed(turns, RuntimeDurability::Required)?;
    let expected_checkpoint = checkpoint_id(turns, turns)?;
    let loaded_started = Instant::now();
    let loaded = store.load_checkpoint(expected_checkpoint).await?;
    let load_ns = elapsed_ns(loaded_started);
    let recovered = store.recover_commit(expected_checkpoint).await?;
    let branch = store.load_branch(seed.selector).await?;
    assert_eq!(loaded.reference().checkpoint_id(), expected_checkpoint);
    assert_eq!(recovered.receipt().head(), branch.head());
    let stats = store.stats().await?;
    assert_eq!(stats.checkpoints(), turns + 1);
    let rss_after_load_and_recovery = linux_current_rss_bytes();
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "sqlite-restart-memory-probe",
            "fixture": FIXTURE,
            "turns": turns,
            "process_isolated_from_writer": true,
            "host_id": host_id,
            "rss_source": if cfg!(target_os = "linux") { "linux-/proc/self/status-VmRSS" } else { "unavailable-on-this-target" },
            "rss_before_open_bytes": rss_before_open,
            "rss_after_open_bytes": rss_after_open,
            "rss_after_latest_load_and_recovery_bytes": rss_after_load_and_recovery,
            "rss_delta_open_to_loaded_bytes": rss_after_open.zip(rss_after_load_and_recovery).map(|(before, after)| i128::from(after) - i128::from(before)),
            "reopen_ns": open_ns,
            "latest_checkpoint_load_ns": load_ns,
            "retained_objects": stats.objects(),
            "retained_conservative_cas_object_and_edge_bytes": stats.object_bytes(),
            "claim_scope": "raw current resident-set observations in a fresh reader process after writer shutdown; not allocator-retained bytes, peak RSS, or a portable threshold",
        })
    );
    black_box((&loaded, &recovered, &branch));
    store.shutdown().await?;
    Ok(())
}

fn linux_current_rss_bytes() -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kibibytes| kibibytes.saturating_mul(1_024))
}

#[allow(
    clippy::too_many_lines,
    reason = "one trace loop keeps timing scopes and exact publication assertions adjacent"
)]
async fn run_trace(
    store: &dyn SessionStore,
    seed: &Seed,
    turns: u64,
    backend: &str,
    storage_path: Option<&Path>,
) -> BenchResult<Trace> {
    let created = store.create(seed.request.clone()).await?;
    let post_create_storage_bytes = storage_path.map(storage_bytes);
    let post_create_database_bytes = storage_path.map(database_bytes);
    let mut head = created.branch();
    let mut prior = seed.bundle.clone();
    let mut parent = seed.checkpoint_id;
    let mut objects = BTreeSet::new();
    let mut object_bytes = 0_usize;
    for object in prior.supplied_objects() {
        assert!(objects.insert(object.address()));
        object_bytes = object_bytes.saturating_add(object.retained_bytes());
    }
    let genesis_objects = objects.len();
    let genesis_bytes = object_bytes;
    let mut commit_ns = Vec::with_capacity(usize::try_from(turns)?);
    let mut publication_build_ns = Vec::with_capacity(usize::try_from(turns)?);
    let mut publication_clone_ns = Vec::with_capacity(usize::try_from(turns)?);
    for turn in 1..=turns {
        let checkpoint_id = checkpoint_id(turns, turn)?;
        let publication_started = Instant::now();
        let publication = successor(seed, checkpoint_id, parent, turn, &prior)?;
        let publication_build_elapsed_ns = elapsed_ns(publication_started);
        assert_eq!(publication.base_root(), Some(prior.root()));
        assert!(publication.supplied_objects().len() <= MAX_OBJECTS_PER_COMMIT);
        assert_eq!(
            publication.stats().new_object_count(),
            publication.supplied_objects().len()
        );
        for object in publication.supplied_objects() {
            assert!(objects.insert(object.address()));
            object_bytes = object_bytes.saturating_add(object.retained_bytes());
        }
        let clone_started = Instant::now();
        let commit_publication = publication.clone();
        let publication_clone_elapsed_ns = elapsed_ns(clone_started);
        assert_eq!(commit_publication.root(), publication.root());
        assert_eq!(commit_publication.stats(), publication.stats());
        assert_eq!(
            commit_publication.supplied_objects().len(),
            publication.supplied_objects().len()
        );
        assert!(
            commit_publication
                .supplied_objects()
                .iter()
                .zip(publication.supplied_objects())
                .all(|(cloned, source)| cloned.address() == source.address())
        );
        publication_build_ns.push(publication_build_elapsed_ns);
        publication_clone_ns.push(publication_clone_elapsed_ns);
        let started = Instant::now();
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                commit_publication,
            ))
            .await?
            .head();
        let elapsed = elapsed_ns(started);
        commit_ns.push(elapsed);
        let cumulative_new_object_bytes = object_bytes.saturating_sub(genesis_bytes);
        let storage_growth_bytes = storage_path
            .zip(post_create_storage_bytes)
            .map(|(path, baseline)| storage_bytes(path).saturating_sub(baseline));
        println!(
            "{}",
            json!({
                "schema": SCHEMA,
                "kind": "commit-sample",
                "fixture": FIXTURE,
                "backend": backend,
                "turns_in_trace": turns,
                "turn": turn,
                "elapsed_ns": elapsed,
                "publication_build_elapsed_ns": publication_build_elapsed_ns,
                "publication_clone_elapsed_ns": publication_clone_elapsed_ns,
                "new_objects": publication.stats().new_object_count(),
                "new_conservative_cas_object_and_edge_bytes": publication.stats().new_retained_bytes(),
                "cumulative_new_conservative_cas_object_and_edge_bytes": cumulative_new_object_bytes,
                "active_db_wal_growth_bytes": storage_growth_bytes,
                "active_db_wal_growth_ratio": storage_growth_bytes
                    .map_or(Value::Null, |bytes| storage_growth_ratio(bytes, cumulative_new_object_bytes)),
                "branch_revision": head.revision().get(),
                "accepted_semantic_mutation_ordinal": turn + 1,
                "acknowledgment_boundary": if backend == "sqlite" { "sqlite_synchronous_full_commit" } else { "in_process_memory_mutation" },
                "object_write_probe_scope": "primary public commit latency paired with exact supplied content-addressed object count/bytes; no revalidation or SQL row-count query runs between primary timed commits",
            })
        );
        prior = publication;
        parent = checkpoint_id;
    }
    assert_eq!(head.revision().get(), turns);
    Ok(Trace {
        head,
        last_checkpoint: parent,
        object_count: objects.len(),
        object_bytes,
        new_object_count: objects.len().saturating_sub(genesis_objects),
        new_object_bytes: object_bytes.saturating_sub(genesis_bytes),
        commit_ns,
        publication_build_ns,
        publication_clone_ns,
        post_create_storage_bytes,
        post_create_database_bytes,
    })
}

fn sqlite_row_counts(path: &Path) -> BenchResult<SqliteRowCounts> {
    let connection = Connection::open(path)?;
    let counts: [i64; 9] = connection.query_row(
        "SELECT
            (SELECT count(*) FROM runtime_objects),
            (SELECT count(*) FROM runtime_object_edges),
            (SELECT count(*) FROM checkpoints),
            (SELECT count(*) FROM sessions),
            (SELECT count(*) FROM branches),
            (SELECT count(*) FROM create_receipts),
            (SELECT count(*) FROM commit_receipts),
            (SELECT count(*) FROM fork_receipts),
            (SELECT count(*) FROM archive_receipts)",
        [],
        |row| {
            Ok([
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ])
        },
    )?;
    Ok(SqliteRowCounts {
        runtime_objects: u64::try_from(counts[0])?,
        runtime_object_edges: u64::try_from(counts[1])?,
        checkpoints: u64::try_from(counts[2])?,
        sessions: u64::try_from(counts[3])?,
        branches: u64::try_from(counts[4])?,
        create_receipts: u64::try_from(counts[5])?,
        commit_receipts: u64::try_from(counts[6])?,
        fork_receipts: u64::try_from(counts[7])?,
        archive_receipts: u64::try_from(counts[8])?,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one instrumented trace keeps every cross-checked phase on the same commit row"
)]
async fn run_sqlite_instrumented_trace(turns: u64) -> BenchResult<()> {
    let database = Database::new()?;
    let seed = seed(turns, RuntimeDurability::Required)?;
    let (store, traces) = SqliteSessionStore::open_instrumented(&database.path)?;
    let created = store.create(seed.request.clone()).await?;
    let baseline_rows = sqlite_row_counts(&database.path)?;
    let baseline_files = sqlite_file_bytes(&database.path);
    let mut head = created.branch();
    let mut prior = seed.bundle.clone();
    let mut parent = seed.checkpoint_id;
    for turn in 1..=turns {
        let checkpoint_id = checkpoint_id(turns, turn)?;
        let publication = successor(&seed, checkpoint_id, parent, turn, &prior)?;
        let revalidation_started = Instant::now();
        let mut canonical_bytes = 0_usize;
        for object in publication.supplied_objects() {
            canonical_bytes = canonical_bytes.saturating_add(object.canonical_bytes().len());
            black_box(RuntimeObject::from_store(
                object.address(),
                object.canonical_bytes().to_vec(),
                object.children().to_vec(),
            )?);
        }
        let revalidation_elapsed_ns = elapsed_ns(revalidation_started);
        let commit_started = Instant::now();
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                publication.clone(),
            ))
            .await?
            .head();
        let instrumented_commit_elapsed_ns = elapsed_ns(commit_started);
        let [commit_trace] = traces
            .take_commits()
            .try_into()
            .map_err(|unexpected: Vec<_>| {
                format!("expected one SQLite commit trace, got {}", unexpected.len())
            })?;
        assert_eq!(
            commit_trace.inserted_objects(),
            u64::try_from(publication.stats().new_object_count())?
        );
        assert_eq!(
            commit_trace.inserted_object_bytes(),
            u64::try_from(publication.stats().new_retained_bytes())?
        );
        let rows = sqlite_row_counts(&database.path)?;
        let files = sqlite_file_bytes(&database.path);
        println!(
            "{}",
            json!({
                "schema": SCHEMA,
                "kind": "sqlite-instrumented-commit-sample",
                "fixture": "sqlite-object-revalidation-and-row-growth-v1",
                "turns_in_trace": turns,
                "turn": turn,
                "branch_revision": head.revision().get(),
                "new_objects": publication.stats().new_object_count(),
                "new_conservative_cas_object_and_edge_bytes": publication.stats().new_retained_bytes(),
                "externally_revalidated_objects": publication.supplied_objects().len(),
                "externally_revalidated_canonical_bytes": canonical_bytes,
                "external_object_revalidation_elapsed_ns": revalidation_elapsed_ns,
                "instrumented_commit_elapsed_ns": instrumented_commit_elapsed_ns,
                "actor_queue_wait_ns": commit_trace.queue_wait_ns(),
                "sqlite_transaction_begin_count": commit_trace.transaction_begin_count(),
                "sqlite_transaction_commit_count": commit_trace.transaction_commit_count(),
                "sqlite_transaction_elapsed_ns": commit_trace.transaction_elapsed_ns(),
                "sqlite_commit_call_elapsed_ns": commit_trace.commit_call_elapsed_ns(),
                "sqlite_inserted_runtime_objects": commit_trace.inserted_objects(),
                "sqlite_inserted_conservative_cas_object_and_edge_bytes": commit_trace.inserted_object_bytes(),
                "database_bytes": files.database,
                "wal_bytes": files.wal,
                "database_growth_since_create_bytes": files.database.saturating_sub(baseline_files.database),
                "wal_growth_since_create_bytes": files.wal.saturating_sub(baseline_files.wal),
                "semantic_table_rows": rows.json(),
                "semantic_table_row_growth_since_create": {
                    "runtime_objects": rows.runtime_objects.saturating_sub(baseline_rows.runtime_objects),
                    "runtime_object_edges": rows.runtime_object_edges.saturating_sub(baseline_rows.runtime_object_edges),
                    "checkpoints": rows.checkpoints.saturating_sub(baseline_rows.checkpoints),
                    "commit_receipts": rows.commit_receipts.saturating_sub(baseline_rows.commit_receipts),
                    "total_rows_in_semantic_tables": rows.total().saturating_sub(baseline_rows.total()),
                },
                "instrumentation_scope": "separate fresh SQLite trace after every primary timing trace; explicit store instrumentation measures actor-start queue wait and the successful immediate transaction/commit call, while row queries and supplied-object hash/decode revalidation deliberately perturb only this workload",
            })
        );
        prior = publication;
        parent = checkpoint_id;
    }
    let rows_before_reopen = sqlite_row_counts(&database.path)?;
    store.shutdown().await?;
    drop(store);
    let reopen_started = Instant::now();
    let reopened = SqliteSessionStore::open(&database.path)?;
    let reopen_ns = elapsed_ns(reopen_started);
    let load_started = Instant::now();
    let loaded = reopened.load_checkpoint(parent).await?;
    let latest_checkpoint_load_and_decode_ns = elapsed_ns(load_started);
    assert_eq!(loaded.reference().checkpoint_id(), parent);
    let rows_after_reopen = sqlite_row_counts(&database.path)?;
    assert_eq!(rows_before_reopen, rows_after_reopen);
    assert_eq!(reopened.load_branch(seed.selector).await?.head(), head);
    reopened.shutdown().await?;
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "sqlite-instrumented-summary",
            "fixture": "sqlite-object-revalidation-and-row-growth-v1",
            "turns": turns,
            "final_branch_revision": head.revision().get(),
            "semantic_table_rows_before_reopen": rows_before_reopen.json(),
            "semantic_table_rows_after_reopen": rows_after_reopen.json(),
            "semantic_table_rows_stable_across_reopen": true,
            "reopen_ns": reopen_ns,
            "latest_checkpoint_load_ns": latest_checkpoint_load_and_decode_ns,
            "latest_checkpoint_load_and_decode_ns": latest_checkpoint_load_and_decode_ns,
            "decode_scope": "public checkpoint load after reopen, including SQL row reads, canonical object address/hash validation, graph decode/materialization, and semantic cross-checks; not pure codec CPU time",
        })
    );
    Ok(())
}

async fn run_sqlite_actor_saturation() -> BenchResult<()> {
    let database = Database::new()?;
    let queue_capacity = NonZeroUsize::new(SATURATION_QUEUE_CAPACITY)
        .ok_or("SQLite actor queue capacity must be nonzero")?;
    let store = SqliteSessionStore::open_with_clock_and_options(
        &database.path,
        Arc::new(conformance::ManualLeaseClock::new(Timestamp::new(100, 0)?)),
        SqliteStoreOptions::new(MemoryStoreLimits::default(), queue_capacity),
    )?;
    let blocker = Connection::open(&database.path)?;
    blocker.execute_batch("PRAGMA busy_timeout=0; BEGIN IMMEDIATE;")?;

    let mut pending = Vec::with_capacity(usize::try_from(SATURATION_ATTEMPTS)?);
    for attempt in 0..SATURATION_ATTEMPTS {
        let request = seed(50_000 + attempt, RuntimeDurability::Required)?.request;
        let submitted = Instant::now();
        let result = store.create(request);
        pending.push((attempt, elapsed_ns(submitted), result));
    }
    drop(blocker);

    let mut acknowledged = 0_u64;
    let mut backend_busy = 0_u64;
    for (attempt, submission_ns, result) in pending {
        let completed = Instant::now();
        let outcome = match result.await {
            Ok(_) => {
                acknowledged = acknowledged.saturating_add(1);
                "ok_create_receipt"
            }
            Err(SessionStoreError::BackendBusy) => {
                backend_busy = backend_busy.saturating_add(1);
                "error_backend_busy"
            }
            Err(error) => return Err(error.into()),
        };
        println!(
            "{}",
            json!({
                "schema": SCHEMA,
                "kind": "sqlite-actor-saturation-sample",
                "fixture": FIXTURE,
                "attempt": attempt,
                "queue_capacity": SATURATION_QUEUE_CAPACITY,
                "submission_elapsed_ns": submission_ns,
                "await_elapsed_ns": elapsed_ns(completed),
                "public_store_outcome": outcome,
            })
        );
    }
    let maximum_accepted_work = u64::try_from(SATURATION_QUEUE_CAPACITY)?.saturating_add(1);
    assert!(
        acknowledged <= maximum_accepted_work,
        "one actor operation plus its bounded queue is the accepted-work bound"
    );
    assert!(
        backend_busy >= 1,
        "excess work must return typed backpressure"
    );
    assert_eq!(
        acknowledged.saturating_add(backend_busy),
        SATURATION_ATTEMPTS
    );
    assert_eq!(store.stats().await?.semantic_mutations(), acknowledged);
    store.shutdown().await?;
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "sqlite-actor-saturation-summary",
            "fixture": FIXTURE,
            "attempts": SATURATION_ATTEMPTS,
            "queue_capacity": SATURATION_QUEUE_CAPACITY,
            "maximum_actor_operation_plus_queued_work": maximum_accepted_work,
            "durably_acknowledged_create_receipts": acknowledged,
            "backend_busy_public_outcomes": backend_busy,
            "typed_acceptance": "Ok(CreateReceipt)",
            "typed_backpressure": "Err(SessionStoreError::BackendBusy)",
            "backend_busy_scope": "the public variant covers a full actor queue or exhausted SQLite lock wait; this workload does not instrument the private dispatch branch",
        })
    );
    Ok(())
}

async fn emit_observation_schema_proof() -> BenchResult<()> {
    let database = Database::new()?;
    let store = SqliteSessionStore::open(&database.path)?;
    store.shutdown().await?;
    drop(store);
    let connection = Connection::open(&database.path)?;
    let mut statement = connection.prepare(
        "SELECT type || ':' || name FROM sqlite_schema
         WHERE type IN ('table','view','trigger') AND (
             lower(name) LIKE '%event%' OR
             lower(name) LIKE '%transcript%' OR
             lower(name) LIKE '%snapshot%' OR
             lower(COALESCE(sql,'')) LIKE '%event%' OR
             lower(COALESCE(sql,'')) LIKE '%transcript%' OR
             lower(COALESCE(sql,'')) LIKE '%snapshot%')
         ORDER BY type,name",
    )?;
    let matches = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(matches, [] as [String; 0]);
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "sqlite-observation-schema-proof",
            "fixture": FIXTURE,
            "inspected_schema_object_types": ["table", "view", "trigger"],
            "observation_named_schema_objects": matches,
            "observation_named_schema_object_count": 0,
            "claim_scope": "fresh-schema structure only; SessionStore also exposes no event mutation method",
            "actual_sql_transaction_trace": false,
        })
    );
    Ok(())
}

fn seed(turns: u64, durability: RuntimeDurability) -> conformance::TestResult<Seed> {
    let base = 100_000 + u128::from(turns) * 1_000;
    let session_id = conformance::id::<SessionId>(base)?;
    let branch_id = conformance::id::<BranchId>(base + 1)?;
    let checkpoint_id = conformance::id::<CheckpointId>(base + 2)?;
    let owner = conformance::id::<RunId>(base + 3)?;
    let selector = SessionRef::new(session_id, branch_id);
    let checkpoint = conformance::checkpoint(
        conformance::topology(session_id, branch_id, checkpoint_id, None, 0, None, 1)?,
        1,
        durability,
    )?;
    let bundle = conformance::bundle(&checkpoint, None)?;
    let request = CreateSession::new(
        selector,
        Timestamp::new(50, 0)?,
        owner,
        NonZeroU64::new(3_600).ok_or("nonzero lease")?,
        bundle.clone(),
    );
    Ok(Seed {
        selector,
        checkpoint_id,
        bundle,
        request,
    })
}

fn successor(
    seed: &Seed,
    checkpoint_id: CheckpointId,
    parent: CheckpointId,
    revision: u64,
    prior: &CheckpointObjectBundle,
) -> conformance::TestResult<CheckpointObjectBundle> {
    let checkpoint = conformance::checkpoint(
        conformance::topology(
            seed.selector.session_id(),
            seed.selector.branch_id(),
            checkpoint_id,
            Some(parent),
            revision,
            None,
            1,
        )?,
        u32::try_from(revision + 1)?,
        seed.request
            .initial()
            .validated_checkpoint()?
            .manifest()
            .durability(),
    )?;
    conformance::bundle(&checkpoint, Some(prior))
}

fn checkpoint_id(turns: u64, turn: u64) -> conformance::TestResult<CheckpointId> {
    conformance::id(200_000 + u128::from(turns) * 1_000 + u128::from(turn))
}

fn storage_bytes(path: &Path) -> u64 {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    fs::metadata(path).map_or(0, |metadata| metadata.len())
        + fs::metadata(PathBuf::from(wal)).map_or(0, |metadata| metadata.len())
}

fn database_bytes(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

#[derive(Clone, Copy)]
struct SqliteFileBytes {
    database: u64,
    wal: u64,
}

fn sqlite_file_bytes(path: &Path) -> SqliteFileBytes {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    SqliteFileBytes {
        database: database_bytes(path),
        wal: fs::metadata(PathBuf::from(wal)).map_or(0, |metadata| metadata.len()),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one JSONL schema row keeps raw counters aligned"
)]
fn emit_summary(
    backend: &str,
    turns: u64,
    open_ns: u64,
    load_ns: u64,
    recovery_ns: u64,
    reopen_ns: &Value,
    storage: &Value,
    semantic_mutations: u64,
    trace: &Trace,
) {
    let mut sorted = trace.commit_ns.clone();
    sorted.sort_unstable();
    let mut publication_build = trace.publication_build_ns.clone();
    publication_build.sort_unstable();
    let mut publication_clone = trace.publication_clone_ns.clone();
    publication_clone.sort_unstable();
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "summary",
            "fixture": FIXTURE,
            "backend": backend,
            "turns": turns,
            "open_ns": open_ns,
            "reopen_ns": reopen_ns,
            "latest_checkpoint_load_ns": load_ns,
            "latest_receipt_recovery_ns": recovery_ns,
            "commit_samples": sorted.len(),
            "commit_median_ns": percentile(&sorted, 50),
            "commit_p95_ns": percentile(&sorted, 95),
            "commit_max_ns": sorted.last().copied(),
            "publication_build_median_ns": percentile(&publication_build, 50),
            "publication_build_p95_ns": percentile(&publication_build, 95),
            "publication_clone_median_ns": percentile(&publication_clone, 50),
            "publication_clone_p95_ns": percentile(&publication_clone, 95),
            "accepted_semantic_mutations": semantic_mutations,
            "sqlite_synchronous_full_commit_acknowledgments": if backend == "sqlite" { json!(turns + 1) } else { Value::Null },
            "sqlite_transaction_count": Value::Null,
            "retained_objects": trace.object_count,
            "retained_conservative_cas_object_and_edge_bytes": trace.object_bytes,
            "new_objects": trace.new_object_count,
            "new_conservative_cas_object_and_edge_bytes": trace.new_object_bytes,
            "storage": storage,
            "exact_recovery": true,
        })
    );
}

fn emit_comparison(turns: u64, memory: &Trace, sqlite: &Trace) {
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "comparison",
            "fixture": FIXTURE,
            "turns": turns,
            "same_final_revision": memory.head.revision() == sqlite.head.revision(),
            "same_retained_objects": memory.object_count == sqlite.object_count,
            "same_delta_objects": memory.new_object_count == sqlite.new_object_count,
            "retained_conservative_cas_object_and_edge_byte_delta_sqlite_minus_memory": i128::try_from(sqlite.object_bytes).unwrap_or(i128::MAX) - i128::try_from(memory.object_bytes).unwrap_or(i128::MAX),
            "new_conservative_cas_object_and_edge_byte_delta_sqlite_minus_memory": i128::try_from(sqlite.new_object_bytes).unwrap_or(i128::MAX) - i128::try_from(memory.new_object_bytes).unwrap_or(i128::MAX),
            "byte_equality_not_claimed_reason": "memory requires Disabled local durability while SQLite supplies Required local durability; that semantic checkpoint difference may change canonical retained bytes",
        })
    );
}

fn percentile(sorted: &[u64], percentile: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let index = sorted
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted.get(index).copied()
}

fn storage_growth_ratio(numerator: u64, denominator: usize) -> Value {
    u64::try_from(denominator)
        .ok()
        .filter(|value| *value != 0)
        .map_or(Value::Null, |denominator| {
            json!({
                "numerator_storage_growth_bytes": numerator,
                "denominator_new_conservative_cas_object_and_edge_bytes": denominator,
                "milli_ratio": numerator.saturating_mul(1_000) / denominator,
            })
        })
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn command_output(program: &str, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unavailable".to_owned(),
            |output| bounded_text(&output.stdout, 16 * 1024),
        )
}

fn git_output(arguments: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_stdout_digest(arguments: &[&str]) -> Option<Digest> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| command_bytes_digest(&output.stdout))?
}

fn worktree_content_closure_metadata() -> Value {
    let root = workspace_root();
    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output();
    let Ok(output) = output else {
        return Value::Null;
    };
    if !output.status.success() {
        return Value::Null;
    }
    let mut framed = Vec::new();
    let mut file_count = 0_usize;
    let mut content_bytes = 0_u64;
    let mut missing_files = 0_usize;
    for raw_path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let Ok(path) = std::str::from_utf8(raw_path) else {
            return Value::Null;
        };
        let Ok(path_len) = u64::try_from(raw_path.len()) else {
            return Value::Null;
        };
        framed.extend_from_slice(&path_len.to_be_bytes());
        framed.extend_from_slice(raw_path);
        match fs::read(root.join(path)) {
            Ok(bytes) => {
                let Ok(byte_len) = u64::try_from(bytes.len()) else {
                    return Value::Null;
                };
                framed.push(b'F');
                framed.extend_from_slice(&byte_len.to_be_bytes());
                framed.extend_from_slice(&bytes);
                content_bytes = content_bytes.saturating_add(byte_len);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                framed.push(b'M');
                missing_files = missing_files.saturating_add(1);
            }
            Err(_) => return Value::Null,
        }
        file_count = file_count.saturating_add(1);
    }
    json!({
        "identity": Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous/non-ignored-worktree-content-closure-v1", &framed],
        ).ok(),
        "file_count": file_count,
        "content_bytes": content_bytes,
        "missing_tracked_files": missing_files,
        "rule": "ordered git ls-files cached plus non-ignored untracked paths, framed with each current regular-file content or a missing marker; file modes are covered separately by commit/diff provenance",
    })
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn command_bytes_digest(bytes: impl AsRef<[u8]>) -> Option<Digest> {
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"plakous/perf-command-stdout-v1", bytes.as_ref()],
    )
    .ok()
}

fn bounded_text(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)])
        .trim()
        .to_owned();
    if bytes.len() > limit {
        format!("{text}\n[truncated at {limit} bytes]")
    } else {
        text
    }
}

fn rustc_host(rustc_vv: &str) -> Option<&str> {
    rustc_vv
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
}

async fn run_large_store_admission() -> BenchResult<()> {
    let database = Database::new()?;
    let (store, traces) = SqliteSessionStore::open_instrumented(&database.path)?;
    let padding = "x".repeat(64 * 1024);
    let mut target = None;
    for session in 0..200 {
        let seed = seed(10_000 + session, RuntimeDurability::Required)?;
        let created = store.create(seed.request).await?;
        let mut head = created.branch();
        let mut prior = seed.bundle;
        for revision in 1..10 {
            let checkpoint = conformance::checkpoint_with_payload(
                conformance::topology(
                    seed.selector.session_id(),
                    seed.selector.branch_id(),
                    conformance::id(
                        seed.selector.session_id().as_uuid().as_u128() + 10 + u128::from(revision),
                    )?,
                    Some(prior.checkpoint_id()),
                    revision,
                    None,
                    1,
                )?,
                1,
                RuntimeDurability::Required,
                prior
                    .validated_checkpoint()?
                    .manifest()
                    .canonical()
                    .context()
                    .clone(),
                false,
                &format!("{session}:{revision}:{padding}"),
            )?;
            let next = conformance::bundle(&checkpoint, Some(&prior))?;
            head = store
                .commit(CommitCheckpoint::new(
                    created.lease().clone(),
                    head,
                    next.clone(),
                ))
                .await?
                .head();
            prior = next;
        }
        target = Some((created.lease().clone(), head, prior));
    }
    let (lease, head, prior) = target.ok_or("large-store target")?;
    let checkpoint = conformance::checkpoint_with_payload(
        conformance::topology(
            head.session().session_id(),
            head.session().branch_id(),
            conformance::id(head.session().session_id().as_uuid().as_u128() + 99)?,
            Some(prior.checkpoint_id()),
            10,
            None,
            1,
        )?,
        1,
        RuntimeDurability::Required,
        prior
            .validated_checkpoint()?
            .manifest()
            .canonical()
            .context()
            .clone(),
        false,
        "fixed admission delta",
    )?;
    let delta = conformance::bundle(&checkpoint, Some(&prior))?;
    let _ = traces.take_commits();
    let started = Instant::now();
    black_box(store.usage().await?);
    let lease = store.renew(lease).await?;
    store
        .commit(CommitCheckpoint::new(lease, head, delta))
        .await?;
    let elapsed = elapsed_ns(started);
    let [trace] = traces
        .take_commits()
        .try_into()
        .map_err(|_: Vec<_>| "one admission trace")?;
    println!(
        "{}",
        json!({"schema": SCHEMA, "kind": "large-store-admission", "sessions": 200,
        "retained_checkpoints_before_admission": 2000, "payload_bytes": 64 * 1024,
        "admission_elapsed_ns": elapsed, "queue_wait_ns": trace.queue_wait_ns(), "informational": true})
    );
    store.shutdown().await?;
    Ok(())
}
