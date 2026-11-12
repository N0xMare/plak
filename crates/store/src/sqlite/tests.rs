use std::error::Error;
use std::fs;
#[cfg(unix)]
use std::num::NonZeroU64;

#[test]
fn removed_branch_transition_tags_are_unsupported() {
    for tag in [3, 4] {
        assert_eq!(
            super::decode_transition(tag).err(),
            Some(SessionStoreError::UnsupportedCodec)
        );
    }
}

#[test]
fn missing_publication_base_or_root_leaves_sqlite_state_unchanged() -> TestResult {
    use plakous_agent::internal::StoreIntegrityReason;
    for reason in [
        StoreIntegrityReason::MissingBase,
        StoreIntegrityReason::MissingRoot,
    ] {
        let (base, next) = fixture::integrity_publication_pair()?;
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(super::SCHEMA)?;
        {
            let transaction = connection.transaction()?;
            super::insert_candidate(
                &transaction,
                &super::candidate(&base)?,
                super::MemoryStoreLimits::default(),
                plakous_agent::internal::StoreAdmission::Ordinary,
            )?;
            transaction.commit()?;
        }
        let before = super::read_stats(&connection)?;
        let mut candidate = super::candidate(&next)?;
        if reason == StoreIntegrityReason::MissingBase {
            candidate.base = Some(candidate.reference.root());
        } else {
            candidate
                .objects
                .retain(|object| object.address() != candidate.reference.root());
        }
        {
            let transaction = connection.transaction()?;
            assert_eq!(
                super::insert_candidate(
                    &transaction,
                    &candidate,
                    super::MemoryStoreLimits::default(),
                    plakous_agent::internal::StoreAdmission::Ordinary
                )
                .err(),
                Some(SessionStoreError::Corrupt(reason)),
            );
            transaction.commit()?;
        }
        assert_eq!(super::read_stats(&connection)?, before);
        assert_eq!(
            super::checkpoint_row(&connection, base.checkpoint_id())?
                .ok_or("base checkpoint")?
                .reference,
            super::bundle_ref(&base)
        );
        assert!(super::checkpoint_row(&connection, next.checkpoint_id())?.is_none());
    }
    Ok(())
}

/// An object address from another format epoch never decodes: the store
/// reports it as an unsupported codec, which the facade surfaces as an
/// incompatible build, not as corruption.
#[test]
fn an_object_from_another_format_epoch_is_an_unsupported_codec() {
    let kind = i64::from(super::kind_tag(super::RuntimeObjectKind::CheckpointRoot));
    let epoch = i64::from(plakous_agent::internal::RUNTIME_FORMAT_EPOCH);
    for other in [epoch - 1, epoch + 1] {
        assert_eq!(
            super::decode_address(kind, other, vec![0; 32]).err(),
            Some(SessionStoreError::UnsupportedCodec),
            "epoch {other}"
        );
    }
}

/// The object table admits exactly the current format epoch. The DDL
/// spells the epoch as a literal, so a bump that forgets it makes every
/// fresh store refuse its first object as "storage unavailable".
#[test]
fn the_object_table_admits_the_current_format_epoch() {
    let epoch = plakous_agent::internal::RUNTIME_FORMAT_EPOCH;
    assert!(
        super::SCHEMA.contains(&format!("CHECK(codec={epoch})")),
        "runtime_objects.codec CHECK disagrees with RUNTIME_FORMAT_EPOCH {epoch}"
    );
}

/// Release cost is bounded by construction, not by tuning: the lease
/// release is one transaction (one WAL fsync under `synchronous=FULL`),
/// `flush` is one passive checkpoint and `shutdown` one truncating
/// checkpoint. Prune schedules separate background housekeeping; release
/// itself never checkpoints (preserves `synchronous=FULL`).
///
/// Renewal and revival are the only commits with no commit fsync of their own:
/// they run under `synchronous=NORMAL` through one helper, so a publication
/// (renewal, then commit) has one commit fsync instead of two (a WAL restart or
/// an automatic checkpoint may still sync). No grant outlives its holder's
/// boot, and the next `FULL` commit syncs the renewal frame.
#[test]
fn release_flush_and_shutdown_perform_a_bounded_number_of_fsyncs() {
    let source = [
        include_str!("../sqlite.rs"),
        include_str!("actor.rs"),
        include_str!("open.rs"),
        include_str!("schema.rs"),
        include_str!("authority.rs"),
        include_str!("objects.rs"),
        include_str!("codec.rs"),
        include_str!("error.rs"),
        include_str!("rows.rs"),
        include_str!("reads.rs"),
        include_str!("ops.rs"),
        include_str!("ops/archive_delete.rs"),
        include_str!("ops/commit.rs"),
        include_str!("ops/create.rs"),
        include_str!("ops/fork.rs"),
        include_str!("ops/lease.rs"),
        include_str!("ops/prune.rs"),
    ]
    .concat();
    let release = source
        .split("fn release(b: &mut Backend, l: &BranchLease)")
        .nth(1)
        .and_then(|rest| rest.split("fn write_active(").next())
        .expect("release body");
    assert_eq!(
        release.matches("transaction_with_behavior(").count(),
        1,
        "release opens exactly one transaction"
    );
    assert_eq!(
        release.matches("tx.commit()").count(),
        2,
        "release commits once on the idempotent path and once on the write path"
    );
    assert!(
        !release.contains("checkpoint_wal("),
        "release never checkpoints; flush and shutdown own that"
    );
    let calls = source.matches("checkpoint_wal(&b.connection, ").count();
    assert_eq!(
        calls, 2,
        "two foreground checkpoint call sites: flush (passive), shutdown (truncate)"
    );
    assert!(source.contains("let _ = checkpoint_wal(&self.connection, false);"));
    assert!(source.contains("checkpoint_wal(&b.connection, false)?;"));
    assert!(source.contains("checkpoint_wal(&b.connection, true)?;"));
    assert!(
        source.contains("PRAGMA synchronous=FULL;"),
        "the durability pragma stays in force"
    );
    assert_eq!(
        source.matches("PRAGMA synchronous=NORMAL;").count(),
        1,
        "one helper weakens a commit"
    );
    let unsynced = source
        .split("fn unsynced<T>(")
        .nth(1)
        .and_then(|rest| rest.split("fn restore_full(").next())
        .expect("unsynced body");
    assert!(unsynced.contains("PRAGMA synchronous=NORMAL;"));
    assert!(unsynced.contains("self.restore_full()"));
    assert!(unsynced.contains("BackendState::Poisoned"));
    assert_eq!(
        source.matches(".unsynced(").count(),
        2,
        "renew and revive, and nothing else, commit unsynced"
    );
    assert!(source.contains("b.unsynced(|b| renew(b, &l))"));
    assert!(source.contains("b.unsynced(|b| revive(b, &l))"));
}

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use plakous_agent::internal::{AcquireBranch, BranchLease};
use plakous_agent::internal::{
    CheckpointId, CheckpointObjectBundle, CommitCheckpoint, CreateReceipt, CreateSession,
    LeaseClock, RuntimeCheckpoint, RuntimeDurability, SessionRef, SessionStore, SessionStoreError,
    Timestamp,
};
use plakous_model::{BranchId, RunId, SessionId};
use rusqlite::Connection;
#[cfg(unix)]
use rusqlite::trace::{TraceEvent, TraceEventCodes};

#[cfg(unix)]
use plakous_agent::internal::{CommitReceipt, FeedChange, FeedPage, FeedSeq};
#[cfg(unix)]
use std::num::NonZeroUsize;

#[cfg(unix)]
use super::{
    CRASH_CUT_MARKER_ENV, CRASH_CUT_STAGE_ENV, CrashCut, SqliteStoreStats, verify_database,
};
use super::{
    Job, SqliteSessionStore, StartupProgress, StartupStage, Worker, crash_fixture as fixture,
    map_sqlite, sqlite_failure_category, startup_error_category, wait_for_actor,
    wait_for_actor_or_join,
};

#[cfg(unix)]
const CRASH_DB_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_DB";
#[cfg(unix)]
const CRASH_HISTORY_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_HISTORY";
#[cfg(unix)]
const CRASH_RENEWALS_ENV: &str = "PLAKOUS_SQLITE_TEST_CRASH_RENEWALS";
#[cfg(unix)]
const CRASH_CHILD_TEST: &str = "sqlite::tests::sqlite_crash_cut_child";
#[cfg(unix)]
const CRASH_WAIT: Duration = Duration::from_secs(30);
/// Each commit cut and whether the commit is durable once it is reached.
#[cfg(unix)]
const COMMIT_CUTS: [(CrashCut, bool); 3] = [
    (CrashCut::BeforeCommit, false),
    (CrashCut::PreCommitHook, false),
    (CrashCut::PostCommitPreReply, true),
];
/// Each prune cut and whether the prune is durable once it is reached.
#[cfg(unix)]
const PRUNE_CUTS: [(CrashCut, bool); 3] = [
    (CrashCut::PruneAfterEnqueue, false),
    (CrashCut::PruneBeforeCommit, false),
    (CrashCut::PruneAfterCommit, true),
];
/// Each line-advance cut, its operation and whether it is durable once reached.
#[cfg(unix)]
const LINE_CUTS: [(&str, CrashCut, bool); 6] = [
    ("fork", CrashCut::ForkBeforeCommit, false),
    ("fork", CrashCut::ForkAfterCommit, true),
    ("carry", CrashCut::CarryBeforeCommit, false),
    ("carry", CrashCut::CarryAfterCommit, true),
    ("record", CrashCut::RecordTerminalsBeforeCommit, false),
    ("record", CrashCut::RecordTerminalsAfterCommit, true),
];
static NEXT_CRASH_DIRECTORY: AtomicU64 = AtomicU64::new(1);
#[cfg(unix)]
static STARTUP_QUERY_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
static STARTUP_QUERY_TEST: Mutex<()> = Mutex::new(());

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[cfg(unix)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "rusqlite trace_v2 requires this exact callback signature"
)]
fn count_startup_query(event: TraceEvent<'_>) {
    if matches!(event, TraceEvent::Stmt(..)) {
        STARTUP_QUERY_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

struct CrashDatabase {
    directory: PathBuf,
    path: PathBuf,
}

#[cfg(unix)]
pub(super) struct KillOnDrop(Option<Child>);

#[cfg(unix)]
impl KillOnDrop {
    pub(super) fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub(super) fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("live crash child")
    }

    pub(super) fn sigkill(mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = {
            let child = self.0.as_mut().expect("live crash child");
            child.kill()?;
            child.wait()
        };
        if status.is_ok() {
            let _ = self.0.take();
        }
        status
    }
}

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl CrashDatabase {
    fn new() -> TestResult<Self> {
        for _ in 0..1_024 {
            let directory = std::env::temp_dir().join(format!(
                "plakous-store-crash-{}-{}",
                std::process::id(),
                NEXT_CRASH_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let path = directory.join("sessions.sqlite3");
                    return Ok(Self { directory, path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique crash-test directory",
        )
        .into())
    }
}

impl Drop for CrashDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct CrashSeed {
    selector: SessionRef,
    checkpoint_id: CheckpointId,
    checkpoint: RuntimeCheckpoint,
    bundle: CheckpointObjectBundle,
    request: CreateSession,
}

#[cfg(unix)]
struct CrashCommit {
    request: CommitCheckpoint,
    checkpoint: RuntimeCheckpoint,
}

fn crash_seed(base: u128) -> TestResult<CrashSeed> {
    let session_id = fixture::id::<SessionId>(base)?;
    let branch_id = fixture::id::<BranchId>(base + 1)?;
    let checkpoint_id = fixture::id::<CheckpointId>(base + 2)?;
    let owner = fixture::id::<RunId>(base + 3)?;
    let selector = SessionRef::new(session_id, branch_id);
    let checkpoint = fixture::checkpoint(
        fixture::topology(session_id, branch_id, checkpoint_id, None, 0, None, 1)?,
        1,
        RuntimeDurability::Required,
    )?;
    let bundle = fixture::bundle(&checkpoint, None)?;
    let request = CreateSession::new(
        selector,
        Timestamp::new(50, 0)?,
        owner,
        NonZeroU64::new(3_600).ok_or("nonzero lease")?,
        bundle.clone(),
    );
    Ok(CrashSeed {
        selector,
        checkpoint_id,
        checkpoint,
        bundle,
        request,
    })
}

#[tokio::test]
async fn cold_start_query_count_is_constant_for_large_authoritative_history() -> TestResult {
    const COMMITS: u64 = 128;
    // rusqlite validates each of the five shape PRAGMAs through pragma_list,
    // adding two traced statements per PRAGMA, independent of history size.
    const PRAGMA_VALIDATION_QUERIES: u64 = 2 * 5;
    let database = CrashDatabase::new()?;
    let seed = crash_seed(81_000)?;
    let store = SqliteSessionStore::open(&database.path)?;
    let created = store.create(seed.request.clone()).await?;
    let mut head = created.branch();
    let mut parent = seed.checkpoint_id;
    let mut prior = seed.bundle.clone();
    for revision in 1..=COMMITS {
        let checkpoint_id = fixture::id::<CheckpointId>(81_100 + u128::from(revision))?;
        let checkpoint = fixture::checkpoint(
            fixture::topology(
                seed.selector.session_id(),
                seed.selector.branch_id(),
                checkpoint_id,
                Some(parent),
                revision,
                None,
                created.lease().fence().get(),
            )?,
            u32::try_from(revision + 1)?,
            RuntimeDurability::Required,
        )?;
        let bundle = fixture::bundle(&checkpoint, Some(&prior))?;
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                bundle.clone(),
            ))
            .await?
            .head();
        parent = checkpoint_id;
        prior = bundle;
    }
    store.shutdown().await?;
    drop(store);

    let _guard = STARTUP_QUERY_TEST
        .lock()
        .map_err(|_| "startup query lock")?;
    let mut connection = Connection::open(&database.path)?;
    connection.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;
         PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
    )?;
    STARTUP_QUERY_COUNT.store(0, Ordering::Relaxed);
    connection.trace_v2(
        TraceEventCodes::SQLITE_TRACE_STMT,
        Some(count_startup_query),
    )?;
    verify_database(&connection)?;
    connection.trace_v2(TraceEventCodes::empty(), None::<fn(TraceEvent<'_>)>)?;
    let query_count = STARTUP_QUERY_COUNT.load(Ordering::Relaxed);
    assert!(
        query_count <= 32 + PRAGMA_VALIDATION_QUERIES,
        "cold-start authority validation executed {query_count} statements for {COMMITS} commits"
    );
    assert_eq!(head.revision().get(), COMMITS);
    Ok(())
}

#[cfg(unix)]
fn crash_commit(
    seed: &CrashSeed,
    created: &CreateReceipt,
    lease: &BranchLease,
    checkpoint_id: CheckpointId,
    history_len: u32,
) -> TestResult<CrashCommit> {
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            seed.selector.session_id(),
            seed.selector.branch_id(),
            checkpoint_id,
            Some(seed.checkpoint_id),
            1,
            None,
            created.lease().fence().get(),
        )?,
        history_len,
        RuntimeDurability::Required,
    )?;
    let request = CommitCheckpoint::new(
        lease.clone(),
        created.branch(),
        fixture::bundle(&checkpoint, Some(&seed.bundle))?,
    );
    Ok(CrashCommit {
        request,
        checkpoint,
    })
}

#[cfg(unix)]
pub(super) fn wait_for_marker(child: &mut Child, marker: &Path) -> TestResult {
    let deadline = Instant::now() + CRASH_WAIT;
    loop {
        if marker.is_file() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(std::io::Error::other(format!(
                "crash child exited before its fence: {status}"
            ))
            .into());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "crash child did not reach its fence",
            )
            .into());
        }
        std::thread::sleep(Duration::from_micros(100));
    }
}

/// Exactly the baseline, except that each of up to `renewals` lease
/// renewals may have counted one semantic mutation.
#[cfg(unix)]
fn assert_old_stats(actual: SqliteStoreStats, baseline: SqliteStoreStats, renewals: u64) {
    if renewals == 0 {
        assert_eq!(actual, baseline);
        return;
    }
    assert_eq!(actual.sessions(), baseline.sessions());
    assert_eq!(actual.branches(), baseline.branches());
    assert_eq!(actual.checkpoints(), baseline.checkpoints());
    assert_eq!(actual.objects(), baseline.objects());
    assert_eq!(actual.object_bytes(), baseline.object_bytes());
    assert_eq!(actual.tombstones(), baseline.tombstones());
    assert!(
        (baseline.semantic_mutations()..=baseline.semantic_mutations() + renewals)
            .contains(&actual.semantic_mutations())
    );
}

#[cfg(unix)]
fn assert_new_stats(
    actual: SqliteStoreStats,
    baseline: SqliteStoreStats,
    request: &CommitCheckpoint,
    renewals: u64,
) {
    let added_objects = u64::try_from(request.checkpoint().supplied_objects().len())
        .expect("fixture object count fits u64");
    let added_bytes = u64::try_from(
        request
            .checkpoint()
            .supplied_objects()
            .iter()
            .map(plakous_agent::internal::RuntimeObject::retained_bytes)
            .sum::<usize>(),
    )
    .expect("fixture object bytes fit u64");
    assert_eq!(actual.sessions(), baseline.sessions());
    assert_eq!(actual.branches(), baseline.branches());
    assert_eq!(actual.checkpoints(), baseline.checkpoints() + 1);
    assert_eq!(actual.objects(), baseline.objects() + added_objects);
    assert_eq!(actual.object_bytes(), baseline.object_bytes() + added_bytes);
    assert!(
        (baseline.semantic_mutations() + 1..=baseline.semantic_mutations() + 1 + renewals)
            .contains(&actual.semantic_mutations())
    );
}

/// The commit's record exists exactly when its commit does: the next
/// sequence after the genesis, an `Advanced` record naming the commit,
/// and the sequence its receipt returns.
#[cfg(unix)]
async fn assert_commit_record(
    store: &SqliteSessionStore,
    seed: &CrashSeed,
    checkpoint_id: CheckpointId,
    receipt: &CommitReceipt,
) -> TestResult {
    let session_id = seed.selector.session_id();
    assert_eq!(receipt.feed_seq(), FeedSeq::new(2));
    assert_eq!(store.feed_tail(session_id).await?.tail(), FeedSeq::new(2));
    let FeedPage::Records { records, .. } = store
        .read_feed(session_id, FeedSeq::new(1), NonZeroUsize::MIN)
        .await?
    else {
        return Err("the commit record fell behind retention".into());
    };
    assert_eq!(records.len(), 1);
    assert!(matches!(
        records[0].header().change,
        FeedChange::Advanced { checkpoint, .. } if checkpoint == checkpoint_id
    ));
    Ok(())
}

/// Reopens after a cut as exactly the old or the new commit. The child may
/// have renewed its lease `renewals` times before committing: the active
/// grant is then the creation lease or one exact renewal of it, never a torn
/// row, and the commit binds the active grant.
#[cfg(unix)]
async fn assert_old_or_new_reopen(
    database: &CrashDatabase,
    seed: &CrashSeed,
    created: &CreateReceipt,
    commit: &CrashCommit,
    checkpoint_id: CheckpointId,
    baseline: SqliteStoreStats,
    renewals: u64,
) -> TestResult<bool> {
    let reopened = SqliteSessionStore::open(&database.path)?;
    let granted = created.lease();
    // The same owner and duration read the live grant back without a write.
    let active = reopened
        .acquire(AcquireBranch::new(
            seed.selector,
            granted.owner(),
            granted.lease_seconds(),
        ))
        .await?;
    assert_eq!(active.fence(), granted.fence());
    assert_ne!(
        active.expires_at().same_domain_cmp(granted.expires_at())?,
        std::cmp::Ordering::Less
    );
    if renewals == 0 {
        assert_eq!(&active, granted);
    }
    let commit = CrashCommit {
        request: CommitCheckpoint::new(
            active.clone(),
            commit.request.expected(),
            commit.request.checkpoint().clone(),
        ),
        checkpoint: commit.checkpoint.clone(),
    };
    let head = reopened.load_branch(seed.selector).await?.head();
    let committed = if head.checkpoint().checkpoint_id() == checkpoint_id {
        assert_eq!(head.revision().get(), 1);
        let recovered = reopened.recover_commit(checkpoint_id).await?;
        assert_eq!(
            reopened.commit(commit.request.clone()).await?,
            recovered.receipt()
        );
        assert_eq!(recovered.operation().lease(), commit.request.lease());
        assert_eq!(recovered.operation().expected(), commit.request.expected());
        assert_eq!(
            recovered.operation().checkpoint_commit_identity(),
            commit.checkpoint.checkpoint_commit_identity()
        );
        let loaded = reopened.load_checkpoint(checkpoint_id).await?;
        assert_eq!(loaded.reference(), recovered.receipt().checkpoint());
        assert_eq!(loaded.checkpoint().as_ref(), &commit.checkpoint);
        assert_new_stats(reopened.stats().await?, baseline, &commit.request, renewals);
        assert_commit_record(&reopened, seed, checkpoint_id, &recovered.receipt()).await?;
        true
    } else {
        assert_eq!(head, created.branch());
        assert_eq!(
            reopened.recover_commit(checkpoint_id).await,
            Err(SessionStoreError::NotFound)
        );
        assert_eq!(
            reopened.load_checkpoint(checkpoint_id).await,
            Err(SessionStoreError::NotFound)
        );
        assert_eq!(
            reopened
                .load_checkpoint(seed.checkpoint_id)
                .await?
                .checkpoint()
                .as_ref(),
            &seed.checkpoint
        );
        let before_retry = reopened.stats().await?;
        assert_old_stats(before_retry, baseline, renewals);
        assert_eq!(
            reopened.feed_tail(seed.selector.session_id()).await?.tail(),
            created.feed_seq(),
            "a cut before commit leaves no record"
        );
        let receipt = reopened.commit(commit.request.clone()).await?;
        assert_eq!(reopened.commit(commit.request.clone()).await?, receipt);
        let recovered = reopened.recover_commit(checkpoint_id).await?;
        assert_eq!(
            recovered.operation().checkpoint_commit_identity(),
            commit.checkpoint.checkpoint_commit_identity()
        );
        assert_eq!(recovered.receipt(), receipt);
        assert_eq!(
            reopened.load_branch(seed.selector).await?.head(),
            receipt.head()
        );
        let loaded = reopened.load_checkpoint(checkpoint_id).await?;
        assert_eq!(loaded.checkpoint().as_ref(), &commit.checkpoint);
        assert_new_stats(reopened.stats().await?, before_retry, &commit.request, 0);
        assert_commit_record(&reopened, seed, checkpoint_id, &receipt).await?;
        false
    };
    Ok(committed)
}

#[test]
fn actor_readiness_timeout_keeps_its_stage_after_late_completion() {
    let (_sender, receiver) = mpsc::sync_channel(1);
    let progress = StartupProgress(Mutex::new(StartupStage::Schema));
    let failure =
        wait_for_actor(&receiver, Duration::ZERO, &progress).expect_err("readiness expires");
    progress.set(StartupStage::Ready);
    assert_eq!(failure.stage, StartupStage::Schema);
    assert_eq!(failure.category, "readiness_timeout");
    assert_eq!(failure.report(), SessionStoreError::BackendUnavailable);
}

#[test]
fn actor_readiness_disconnect_is_distinct_from_timeout() {
    let (sender, receiver) = mpsc::sync_channel(1);
    drop(sender);
    let progress = StartupProgress(Mutex::new(StartupStage::Connection));
    let failure =
        wait_for_actor(&receiver, Duration::ZERO, &progress).expect_err("readiness disconnected");
    assert_eq!(failure.stage, StartupStage::Connection);
    assert_eq!(failure.category, "readiness_disconnected");
    assert_eq!(failure.report(), SessionStoreError::BackendUnavailable);
}

#[test]
fn actor_readiness_backend_failure_preserves_original_category_and_error() {
    for error in [
        SessionStoreError::BackendBusy,
        SessionStoreError::UnsupportedSchema,
    ] {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Err(error)).expect("backend failure");
        let progress = StartupProgress(Mutex::new(StartupStage::Admission));
        let failure =
            wait_for_actor(&receiver, Duration::ZERO, &progress).expect_err("backend refused open");
        assert_eq!(failure.stage, StartupStage::Admission);
        assert_eq!(failure.category, "backend");
        assert_eq!(failure.report(), error);
    }
    assert_eq!(
        startup_error_category(SessionStoreError::BackendBusy),
        "backend_busy"
    );
    assert_eq!(
        startup_error_category(SessionStoreError::BackendUnavailable),
        "backend_unavailable"
    );
}

#[test]
fn sqlite_full_and_busy_keep_distinct_safe_diagnostics() {
    assert_eq!(
        super::map_io(std::io::Error::from_raw_os_error(
            rustix::io::Errno::NOSPC.raw_os_error()
        )),
        SessionStoreError::StorageFull
    );
    let connection = Connection::open_in_memory().expect("memory SQLite");
    connection
        .execute_batch("CREATE TABLE full_test(value BLOB);")
        .expect("table");
    let pages: i64 = connection
        .pragma_query_value(None, "page_count", |row| row.get(0))
        .expect("pages");
    connection
        .pragma_update(None, "max_page_count", pages)
        .expect("fixed page budget");
    let error = connection
        .execute("INSERT INTO full_test VALUES(zeroblob(1048576))", [])
        .expect_err("the physical page budget is full");
    assert_eq!(map_sqlite(error), SessionStoreError::StorageFull);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM full_test", [], |row| row
                .get::<_, i64>(0))
            .expect("count"),
        0
    );
    for (code, category, expected) in [
        (
            rusqlite::ffi::SQLITE_FULL,
            "disk_full",
            SessionStoreError::StorageFull,
        ),
        (
            rusqlite::ffi::SQLITE_BUSY,
            "database_busy",
            SessionStoreError::BackendBusy,
        ),
    ] {
        let sqlite = rusqlite::ffi::Error::new(code);
        assert_eq!(sqlite_failure_category(sqlite.code), category);
        let error = rusqlite::Error::SqliteFailure(
            sqlite,
            Some("private database path and SQL".to_owned()),
        );
        assert_eq!(map_sqlite(error), expected);
    }
}

#[test]
fn actor_startup_timeout_is_reported_only_after_worker_exit() {
    let (job_sender, job_receiver) = mpsc::sync_channel::<Job>(1);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let (join_boundary_sender, join_boundary_receiver) = mpsc::sync_channel(1);
    let (exit_sender, exit_receiver) = mpsc::sync_channel(1);
    let exited = Arc::new(AtomicBool::new(false));
    let worker_exited = Arc::clone(&exited);
    let worker = std::thread::spawn(move || {
        assert!(job_receiver.recv().is_err());
        join_boundary_sender.send(()).expect("join boundary");
        exit_receiver.recv().expect("worker exit release");
        assert!(
            ready_sender.send(Ok(())).is_err(),
            "late readiness stays refused"
        );
        worker_exited.store(true, Ordering::SeqCst);
    });
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let progress = StartupProgress(Mutex::new(StartupStage::Schema));
        let result = wait_for_actor_or_join(
            ready_receiver,
            job_sender,
            worker,
            Duration::ZERO,
            &progress,
        );
        result_sender
            .send(result.map(|_| ()))
            .expect("startup result");
    });

    join_boundary_receiver.recv().expect("actor entered join");
    assert!(!exited.load(Ordering::SeqCst));
    assert!(matches!(
        result_receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    exit_sender.send(()).expect("release worker exit");
    assert_eq!(
        result_receiver.recv().expect("startup result after exit"),
        Err(SessionStoreError::BackendUnavailable)
    );
    assert!(exited.load(Ordering::SeqCst));
    waiter.join().expect("waiter exits");
}

#[tokio::test]
async fn actor_dispatch_reports_full_and_failed_workers_without_waiting() {
    let (full_sender, full_receiver) = super::actor_queue(1);
    assert!(
        full_sender
            .send(super::Lane::Critical, Box::new(|_| {}))
            .is_ok()
    );
    let full = SqliteSessionStore {
        storage_probe: None,
        clock: Arc::new(super::SystemLeaseClock::new().expect("system lease clock")),
        domain: super::SystemLeaseClock::new()
            .expect("system lease clock")
            .now()
            .expect("clock reading")
            .domain(),
        sender: full_sender,
        usage_snapshot: Arc::new(Mutex::new(Err(SessionStoreError::BackendUnavailable))),
        worker: Arc::new(Worker {
            join: Mutex::new(None),
        }),
        #[cfg(feature = "perf-instrumentation")]
        trace: None,
    };
    assert_eq!(
        full.dispatch(
            "actor_dispatch_reports_full_and_failed_workers_without_waiting",
            |_| Ok::<_, SessionStoreError>(())
        )
        .await,
        Err(SessionStoreError::BackendBusy)
    );
    drop(full_receiver);

    let (failed_sender, failed_receiver) = super::actor_queue(1);
    drop(failed_receiver);
    let failed = SqliteSessionStore {
        storage_probe: None,
        clock: Arc::new(super::SystemLeaseClock::new().expect("system lease clock")),
        domain: super::SystemLeaseClock::new()
            .expect("system lease clock")
            .now()
            .expect("clock reading")
            .domain(),
        sender: failed_sender,
        usage_snapshot: Arc::new(Mutex::new(Err(SessionStoreError::BackendUnavailable))),
        worker: Arc::new(Worker {
            join: Mutex::new(None),
        }),
        #[cfg(feature = "perf-instrumentation")]
        trace: None,
    };
    assert_eq!(
        failed
            .dispatch(
                "actor_dispatch_reports_full_and_failed_workers_without_waiting",
                |_| Ok::<_, SessionStoreError>(())
            )
            .await,
        Err(SessionStoreError::BackendUnavailable)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_crash_cut_child() {
    let Some(path) = std::env::var_os(CRASH_DB_ENV).map(PathBuf::from) else {
        return;
    };
    let seed = crash_seed(50_000).expect("seed");
    let checkpoint_id = fixture::id::<CheckpointId>(50_010).expect("checkpoint id");
    let history_len = std::env::var(CRASH_HISTORY_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    let renewals: u32 = std::env::var(CRASH_RENEWALS_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let store = SqliteSessionStore::open(path).expect("open child store");
    let created = store
        .recover_create(seed.request.clone())
        .await
        .expect("recover create");
    // Heartbeat-style unsynced renewals ahead of the `FULL` commit, as before
    // every runtime publication.
    let mut lease = created.lease().clone();
    for _ in 0..renewals {
        lease = store.renew(lease).await.expect("renew");
    }
    let commit =
        crash_commit(&seed, &created, &lease, checkpoint_id, history_len).expect("commit request");
    let result = store.commit(commit.request).await;
    panic!("configured crash cut did not stop the child: {result:?}");
}

/// Every crash cut has one name and is listed in exactly one `SIGKILL`
/// matrix's stage table, so a cut cannot be added that no matrix selects.
/// Whether a listed cut fires is each matrix's own assertion.
#[cfg(unix)]
#[test]
fn every_crash_cut_is_named_once_and_listed_in_one_matrix() {
    for cut in CrashCut::ALL {
        for other in CrashCut::ALL {
            assert_eq!(
                CrashCut::named(cut.name()) == Some(other),
                cut == other,
                "{cut:?} and {other:?}"
            );
        }
    }
    let mut matrices: Vec<CrashCut> = COMMIT_CUTS.iter().map(|(cut, _)| *cut).collect();
    // The async-commit test cuts inside the commit on its own.
    matrices.push(CrashCut::AsyncInCommit);
    matrices.extend(PRUNE_CUTS.iter().map(|(cut, _)| *cut));
    matrices.extend(LINE_CUTS.iter().map(|(_, cut, _)| *cut));
    matrices.extend(super::gc_tests::GC_CUTS.iter().map(|(cut, _)| *cut));
    for cut in CrashCut::ALL {
        assert_eq!(
            matrices.iter().filter(|cut_by| **cut_by == cut).count(),
            1,
            "{cut:?} is not listed in exactly one matrix"
        );
    }
    assert_eq!(matrices.len(), CrashCut::ALL.len());
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_commit_cuts_reopen_as_exact_old_or_new_authority() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    for (stage, committed) in COMMIT_CUTS {
        let database = CrashDatabase::new()?;
        let seed = crash_seed(50_000)?;
        let checkpoint_id = fixture::id::<CheckpointId>(50_010)?;
        let store = SqliteSessionStore::open(&database.path)?;
        let created = store.create(seed.request.clone()).await?;
        let commit = crash_commit(&seed, &created, created.lease(), checkpoint_id, 2)?;
        let baseline = store.stats().await?;
        drop(store);

        let marker = database.directory.join(format!("{}.reached", stage.name()));
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(CRASH_CHILD_TEST)
            .arg("--nocapture")
            .env(CRASH_DB_ENV, &database.path)
            .env(CRASH_CUT_STAGE_ENV, stage.name())
            .env(CRASH_CUT_MARKER_ENV, &marker)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = KillOnDrop::new(child);
        wait_for_marker(child.child(), &marker)?;
        let status = child.sigkill()?;
        assert_eq!(status.signal(), Some(9));

        assert_eq!(
            assert_old_or_new_reopen(
                &database,
                &seed,
                &created,
                &commit,
                checkpoint_id,
                baseline,
                0
            )
            .await?,
            committed
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    const HISTORY_ITEMS: u32 = 1_024;
    const RENEWALS: u32 = 3;
    let delays = [
        Duration::ZERO,
        Duration::from_micros(50),
        Duration::from_micros(200),
        Duration::from_micros(500),
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_millis(5),
        Duration::from_millis(10),
    ];
    // Each delay runs once on a fresh lease and once after unsynced renewals,
    // so the cut also lands in a WAL whose last frames are `NORMAL` commits.
    let attempts = [0, RENEWALS]
        .into_iter()
        .flat_map(|renewals| delays.into_iter().map(move |delay| (renewals, delay)));
    for (attempt, (renewals, delay)) in attempts.enumerate() {
        let database = CrashDatabase::new()?;
        let seed = crash_seed(50_000)?;
        let checkpoint_id = fixture::id::<CheckpointId>(50_010)?;
        let store = SqliteSessionStore::open(&database.path)?;
        let created = store.create(seed.request.clone()).await?;
        let commit = crash_commit(
            &seed,
            &created,
            created.lease(),
            checkpoint_id,
            HISTORY_ITEMS,
        )?;
        let baseline = store.stats().await?;
        drop(store);

        let marker = database
            .directory
            .join(format!("async-in-commit-{attempt}.reached"));
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(CRASH_CHILD_TEST)
            .arg("--nocapture")
            .env(CRASH_DB_ENV, &database.path)
            .env(CRASH_HISTORY_ENV, HISTORY_ITEMS.to_string())
            .env(CRASH_RENEWALS_ENV, renewals.to_string())
            .env(CRASH_CUT_STAGE_ENV, CrashCut::AsyncInCommit.name())
            .env(CRASH_CUT_MARKER_ENV, &marker)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = KillOnDrop::new(child);
        wait_for_marker(child.child(), &marker)?;
        std::thread::sleep(delay);
        let status = child.sigkill()?;
        assert_eq!(status.signal(), Some(9));

        assert_old_or_new_reopen(
            &database,
            &seed,
            &created,
            &commit,
            checkpoint_id,
            baseline,
            u64::from(renewals),
        )
        .await?;
    }
    Ok(())
}

#[cfg(unix)]
const PRUNE_CRASH_DB_ENV: &str = "PLAKOUS_SQLITE_TEST_PRUNE_CRASH_DB";
#[cfg(unix)]
const PRUNE_CRASH_CHILD_TEST: &str = "sqlite::tests::sqlite_prune_crash_cut_child";
#[cfg(unix)]
const PRUNE_CRASH_COMMITS: u64 = 8;

/// Creates the crash seed's session and commits `PRUNE_CRASH_COMMITS`
/// successors, each with one more history item than its parent.
#[cfg(unix)]
async fn prune_crash_chain(
    store: &SqliteSessionStore,
) -> TestResult<(CreateReceipt, plakous_agent::internal::BranchHead)> {
    let seed = crash_seed(60_000)?;
    let created = store.create(seed.request.clone()).await?;
    let mut head = created.branch();
    let mut prior = seed.bundle.clone();
    let mut parent = seed.checkpoint_id;
    for revision in 1..=PRUNE_CRASH_COMMITS {
        let checkpoint_id = prune_crash_id(revision)?;
        let checkpoint = fixture::checkpoint(
            fixture::topology(
                seed.selector.session_id(),
                seed.selector.branch_id(),
                checkpoint_id,
                Some(parent),
                revision,
                None,
                created.lease().fence().get(),
            )?,
            u32::try_from(revision)? + 1,
            RuntimeDurability::Required,
        )?;
        let publication = fixture::bundle(&checkpoint, Some(&prior))?;
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                publication.clone(),
            ))
            .await?
            .head();
        prior = publication;
        parent = checkpoint_id;
    }
    Ok((created, head))
}

#[cfg(unix)]
fn prune_crash_id(revision: u64) -> TestResult<CheckpointId> {
    fixture::id::<CheckpointId>(60_100 + u128::from(revision))
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_prune_crash_cut_child() {
    let Some(path) = std::env::var_os(PRUNE_CRASH_DB_ENV).map(PathBuf::from) else {
        return;
    };
    let store = SqliteSessionStore::open(path).expect("open child store");
    let (created, head) = prune_crash_chain(&store).await.expect("chain");
    let result = store
        .prune(plakous_agent::internal::PruneBranch::new(
            created.lease().clone(),
            head,
            std::collections::BTreeSet::new(),
            1,
        ))
        .await;
    panic!("configured crash cut did not stop the child: {result:?}");
}

/// A prune killed at any cut reopens, fully verified, as exactly the
/// unpruned or the pruned store, never anything between.
#[cfg(unix)]
#[tokio::test]
async fn sigkill_prune_cuts_reopen_as_exact_unpruned_or_pruned_history() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    for (stage, committed) in PRUNE_CUTS {
        let database = CrashDatabase::new()?;
        let marker = database.directory.join(format!("{}.reached", stage.name()));
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(PRUNE_CRASH_CHILD_TEST)
            .arg("--nocapture")
            .env(PRUNE_CRASH_DB_ENV, &database.path)
            .env(CRASH_CUT_STAGE_ENV, stage.name())
            .env(CRASH_CUT_MARKER_ENV, &marker)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = KillOnDrop::new(child);
        wait_for_marker(child.child(), &marker)?;
        let status = child.sigkill()?;
        assert_eq!(status.signal(), Some(9));

        drop(SqliteSessionStore::open_verified(&database.path)?);
        let reopened = SqliteSessionStore::open(&database.path)?;
        let seed = crash_seed(60_000)?;
        let head = reopened.load_branch(seed.selector).await?.head();
        assert_eq!(
            head.checkpoint().checkpoint_id(),
            prune_crash_id(PRUNE_CRASH_COMMITS)?
        );
        reopened.load_checkpoint(seed.checkpoint_id).await?;
        for revision in 1..=PRUNE_CRASH_COMMITS {
            let loaded = reopened
                .load_checkpoint(prune_crash_id(revision)?)
                .await
                .map(|_| ());
            if committed && revision + 1 < PRUNE_CRASH_COMMITS {
                assert_eq!(
                    loaded,
                    Err(SessionStoreError::Pruned),
                    "{stage:?} {revision}"
                );
            } else {
                assert_eq!(loaded, Ok(()), "{stage:?} {revision}");
            }
        }
        let retained = reopened.stats().await?;
        let expected = if committed {
            3
        } else {
            PRUNE_CRASH_COMMITS + 1
        };
        assert_eq!(retained.checkpoints(), expected, "{stage:?}");
        reopened.shutdown().await?;
    }
    Ok(())
}

#[cfg(unix)]
const LINE_CRASH_DB_ENV: &str = "PLAKOUS_SQLITE_TEST_LINE_CRASH_DB";
#[cfg(unix)]
const LINE_CRASH_CHILD_TEST: &str = "sqlite::tests::sqlite_line_advance_crash_cut_child";

/// The head advance of `seed`'s line onto a new branch: the store reads the
/// step from the target topology, so an exact fork transition carries it.
#[cfg(unix)]
fn line_advance(
    seed: &CrashSeed,
    created: &CreateReceipt,
) -> TestResult<plakous_agent::internal::ForkBranch> {
    let target_branch = fixture::id::<BranchId>(70_501)?;
    let source = seed.checkpoint.manifest().topology();
    let mut topology = fixture::topology(
        source.session_id,
        target_branch,
        fixture::id::<CheckpointId>(70_502)?,
        Some(source.checkpoint_id),
        0,
        Some(plakous_agent::internal::ForkLineage {
            session_id: source.session_id,
            branch_id: source.branch_id,
            checkpoint_id: source.checkpoint_id,
            branch_revision: 0,
        }),
        1,
    )?;
    topology.line = source.line;
    topology.line_generation = 2;
    let context = seed
        .checkpoint
        .manifest()
        .canonical()
        .context()
        .ordinary_fork(
            source.checkpoint_id,
            source.session_id,
            target_branch,
            [0x77; 32],
        )?;
    let target =
        fixture::checkpoint_with_context(topology, 1, RuntimeDurability::Required, context)?;
    Ok(plakous_agent::internal::ForkBranch::new(
        plakous_agent::internal::SessionWorkflowId::new([0x77; 32])?,
        plakous_agent::internal::Digest::sha256(
            plakous_agent::internal::DigestDomain::RequestPlan,
            1,
            &[b"line-advance"],
        )?,
        created.lease().clone(),
        created.session().revision(),
        created.branch(),
        target_branch,
        fixture::id::<RunId>(70_503)?,
        NonZeroU64::new(3_600).ok_or("nonzero lease")?,
        fixture::bundle(&target, Some(&seed.bundle))?,
    ))
}

#[cfg(unix)]
const LINE_CRASH_MODE_ENV: &str = "PLAKOUS_SQLITE_TEST_LINE_CRASH_MODE";

/// The session a cut carry would create as the line's next head.
#[cfg(unix)]
fn carried_head() -> TestResult<SessionRef> {
    Ok(SessionRef::new(
        fixture::id::<SessionId>(70_601)?,
        fixture::id::<BranchId>(70_602)?,
    ))
}

/// A same-store carry of `seed`'s head under its create lease.
#[cfg(unix)]
fn line_carry(seed: &CrashSeed, created: &CreateReceipt) -> TestResult<CreateSession> {
    let target = carried_head()?;
    let mut topology = fixture::topology(
        target.session_id(),
        target.branch_id(),
        fixture::id::<CheckpointId>(70_603)?,
        None,
        0,
        None,
        1,
    )?;
    topology.line = seed.checkpoint.manifest().topology().line;
    topology.line_generation = 2;
    let genesis = fixture::checkpoint(topology, 1, RuntimeDurability::Required)?;
    Ok(CreateSession::new(
        target,
        Timestamp::new(60, 0)?,
        fixture::id::<RunId>(70_604)?,
        NonZeroU64::new(3_600).ok_or("nonzero lease")?,
        fixture::bundle(&genesis, None)?,
    )
    .continuing_line(plakous_agent::internal::LineContinuation::new(
        created.lease().clone(),
        created.branch(),
        Vec::new(),
    )))
}

/// One stopped job's terminal evidence for the line's head.
#[cfg(unix)]
fn line_terminal() -> TestResult<plakous_agent::internal::LineJobTerminal> {
    plakous_agent::internal::LineJobTerminal::from_view(&plakous_agent::JobView {
        number: 4,
        command: "sleep 60".to_owned(),
        started_at: Timestamp::new(10, 0)?,
        owner: "driver".to_owned(),
        elapsed: Duration::from_secs(3),
        status: plakous_agent::JobStatus::Stopped(plakous_agent::JobStopReason::User),
        output: String::new(),
        session_id: [7; 16],
        owner_generation: 1,
    })?
    .ok_or_else(|| "a stopped job carries terminal evidence".into())
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_line_advance_crash_cut_child() {
    let Some(path) = std::env::var_os(LINE_CRASH_DB_ENV).map(PathBuf::from) else {
        return;
    };
    let store = SqliteSessionStore::open(path).expect("open child store");
    let seed = crash_seed(70_000).expect("seed");
    let created = store.create(seed.request.clone()).await.expect("create");
    let mode = std::env::var(LINE_CRASH_MODE_ENV).unwrap_or_default();
    let result = match mode.as_str() {
        "carry" => store
            .create(line_carry(&seed, &created).expect("carry request"))
            .await
            .map(|_| ()),
        "record" => store
            .record_line_job_terminals(plakous_agent::internal::RecordLineJobTerminals::new(
                created.lease().clone(),
                seed.checkpoint.manifest().topology().line,
                1,
                seed.checkpoint_id,
                vec![line_terminal().expect("terminal")],
            ))
            .await
            .map(|_| ()),
        _ => store
            .fork(line_advance(&seed, &created).expect("fork request"))
            .await
            .map(|_| ()),
    };
    panic!("configured crash cut did not stop the child: {result:?}");
}

/// A head move (a rebind's fork or a carry's create) killed inside or just
/// after its transaction reopens, fully verified, with exactly one head: the
/// source at generation 1 and no target, or the target at generation 2 with
/// the source fenced. A line terminal recording killed the same way is
/// recorded wholly or not at all.
#[cfg(unix)]
#[tokio::test]
async fn sigkill_line_advance_cuts_reopen_with_exactly_one_head() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    for (mode, stage, committed) in LINE_CUTS {
        let database = CrashDatabase::new()?;
        let marker = database.directory.join(format!("{}.reached", stage.name()));
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(LINE_CRASH_CHILD_TEST)
            .arg("--nocapture")
            .env(LINE_CRASH_DB_ENV, &database.path)
            .env(LINE_CRASH_MODE_ENV, mode)
            .env(CRASH_CUT_STAGE_ENV, stage.name())
            .env(CRASH_CUT_MARKER_ENV, &marker)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = KillOnDrop::new(child);
        wait_for_marker(child.child(), &marker)?;
        let status = child.sigkill()?;
        assert_eq!(status.signal(), Some(9));

        drop(SqliteSessionStore::open_verified(&database.path)?);
        let reopened = SqliteSessionStore::open(&database.path)?;
        assert_one_line_head(&reopened, mode, stage, committed).await?;
        reopened.shutdown().await?;
    }
    Ok(())
}

/// The reopened store after a cut `stage` of `mode`: exactly one head, or a
/// terminal recording wholly there or absent.
#[cfg(unix)]
async fn assert_one_line_head(
    reopened: &SqliteSessionStore,
    mode: &str,
    stage: CrashCut,
    committed: bool,
) -> TestResult {
    let seed = crash_seed(70_000)?;
    let line = seed.checkpoint.manifest().topology().line;
    let record = reopened.load_line(line).await?;
    if mode == "record" {
        assert_eq!((record.head(), record.generation()), (seed.selector, 1));
        assert_eq!(
            reopened.line_job_terminals(line).await?,
            if committed {
                vec![line_terminal()?]
            } else {
                Vec::new()
            },
            "{stage:?}"
        );
        return Ok(());
    }
    let target = if mode == "carry" {
        carried_head()?
    } else {
        SessionRef::new(seed.selector.session_id(), fixture::id(70_501)?)
    };
    let lease = reopened
        .acquire(AcquireBranch::new(
            seed.selector,
            fixture::id(70_504)?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let next = fixture::checkpoint(
        fixture::topology(
            seed.selector.session_id(),
            seed.selector.branch_id(),
            fixture::id(70_505)?,
            Some(seed.checkpoint_id),
            1,
            None,
            lease.fence().get(),
        )?,
        2,
        RuntimeDurability::Required,
    )?;
    let source_commit = reopened
        .commit(CommitCheckpoint::new(
            lease.clone(),
            reopened.load_branch(seed.selector).await?.head(),
            fixture::bundle(&next, Some(&seed.bundle))?,
        ))
        .await
        .map(|_| ());
    if committed {
        assert_eq!(
            (record.head(), record.generation()),
            (target, 2),
            "{stage:?}"
        );
        assert_eq!(
            source_commit,
            Err(SessionStoreError::NotLineHead),
            "{stage:?}"
        );
    } else {
        assert_eq!(
            (record.head(), record.generation()),
            (seed.selector, 1),
            "{stage:?}"
        );
        assert_eq!(
            reopened.load_branch(target).await.map(|_| ()),
            Err(SessionStoreError::NotFound),
            "{stage:?}"
        );
        assert_eq!(source_commit, Ok(()), "{stage:?}");
    }
    Ok(())
}

#[cfg(not(unix))]
#[test]
fn sigkill_commit_cuts_reopen_as_exact_old_or_new_authority() -> TestResult {
    eprintln!("skipped: Child::kill has SIGKILL semantics only on Unix");
    Ok(())
}

#[cfg(not(unix))]
#[test]
fn bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit() -> TestResult {
    eprintln!("skipped: Child::kill has SIGKILL semantics only on Unix");
    Ok(())
}
