//! Focused fresh-schema and actor lifecycle tests.

#![cfg(feature = "sqlite")]
#![allow(clippy::expect_used, reason = "test fixtures fail at exact lines")]

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::Duration;

use plakous_agent::internal::{
    BranchHead, BranchTransitionMode, CheckpointId, CheckpointObjectBundle, CommitCheckpoint,
    CreateSession, DeleteSession, ForkBranch, ForkLineage, LeaseClock, LeaseDomain, LeaseInstant,
    PruneBranch, RecoverFork, RuntimeDurability, SessionRef, SessionRevision, SessionStore,
    SessionStoreError, StoreAdmission, Timestamp,
};
use plakous_model::{BranchId, RunId, SessionId};
use plakous_store::{
    BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS, BUNDLED_SQLITE_SOURCE_ID, BUNDLED_SQLITE_VERSION,
    MemoryStoreLimits, SqliteSessionStore, SqliteStoreOptions, SqliteStoreStats,
};
use rusqlite::Connection;

use super::conformance;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct ObservedLeaseClock {
    seconds: AtomicU64,
    observer: Mutex<Option<mpsc::SyncSender<LeaseInstant>>>,
}

impl ObservedLeaseClock {
    fn new(seconds: u64) -> Self {
        Self {
            seconds: AtomicU64::new(seconds),
            observer: Mutex::new(None),
        }
    }

    fn set(&self, seconds: u64) {
        self.seconds.store(seconds, Ordering::SeqCst);
    }

    fn observe_next(&self) -> mpsc::Receiver<LeaseInstant> {
        let (sender, receiver) = mpsc::sync_channel(1);
        match self.observer.lock() {
            Ok(mut observer) => *observer = Some(sender),
            Err(poisoned) => *poisoned.into_inner() = Some(sender),
        }
        receiver
    }
}

impl LeaseClock for ObservedLeaseClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        let timestamp = LeaseInstant::new(
            LeaseDomain::new([1; 16], [2; 32]),
            self.seconds.load(Ordering::SeqCst),
            0,
        )?;
        let sender = match self.observer.lock() {
            Ok(mut observer) => observer.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(sender) = sender {
            let _ = sender.try_send(timestamp);
        }
        Ok(timestamp)
    }
}

struct Database {
    directory: PathBuf,
    path: PathBuf,
}
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "plakous-store-v13-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("directory");
        let path = directory.join("sessions.sqlite3");
        Self { directory, path }
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

fn seed(
    base: u128,
    durability: RuntimeDurability,
    history_len: u32,
) -> conformance::TestResult<Seed> {
    let session_id = conformance::id::<SessionId>(base)?;
    let branch_id = conformance::id::<BranchId>(base + 1)?;
    let checkpoint_id = conformance::id::<CheckpointId>(base + 2)?;
    let owner = conformance::id::<RunId>(base + 3)?;
    let selector = SessionRef::new(session_id, branch_id);
    let checkpoint = conformance::checkpoint(
        conformance::topology(session_id, branch_id, checkpoint_id, None, 0, None, 1)?,
        history_len,
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
        RuntimeDurability::Required,
    )?;
    conformance::bundle(&checkpoint, Some(prior))
}

fn storage_bytes(path: &PathBuf) -> u64 {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    fs::metadata(path).map_or(0, |metadata| metadata.len())
        + fs::metadata(PathBuf::from(wal)).map_or(0, |metadata| metadata.len())
}

fn assert_linear_stats(
    stats: SqliteStoreStats,
    turns: u64,
    objects: usize,
    object_bytes: usize,
) -> Result<(), Box<dyn Error>> {
    assert_eq!(stats.sessions(), 1);
    assert_eq!(stats.branches(), 1);
    assert_eq!(stats.checkpoints(), turns + 1);
    assert_eq!(stats.objects(), u64::try_from(objects)?);
    assert_eq!(stats.object_bytes(), u64::try_from(object_bytes)?);
    assert_eq!(stats.semantic_mutations(), turns + 1);
    Ok(())
}

async fn seed_shared_corpus(
    db: &Database,
    durability: RuntimeDurability,
) -> Result<(), Box<dyn Error>> {
    conformance::run_semantic_conformance(durability, RuntimeDurability::Required, |clock| {
        Ok(
            Arc::new(SqliteSessionStore::open_with_clock(&db.path, clock)?)
                as Arc<dyn SessionStore>,
        )
    })
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "the restart fixture keeps one complete publish/drop/reopen/recover proof linear"
)]
async fn assert_lost_fork_ack_reopens(
    db: &Database,
    base: u128,
    transition: BranchTransitionMode,
    rebound: bool,
    workflow_seed: u8,
) -> Result<(), Box<dyn Error>> {
    let session_id = conformance::id::<SessionId>(base)?;
    let source_branch = conformance::id::<BranchId>(base + 1)?;
    let target_branch = conformance::id::<BranchId>(base + 2)?;
    let genesis_id = conformance::id::<CheckpointId>(base + 3)?;
    let source_id = conformance::id::<CheckpointId>(base + 4)?;
    let target_id = conformance::id::<CheckpointId>(base + 5)?;
    let source_owner = conformance::id::<RunId>(base + 6)?;
    let target_owner = conformance::id::<RunId>(base + 7)?;
    let recovery_owner = conformance::id::<RunId>(base + 8)?;
    let target_reopen_owner = conformance::id::<RunId>(base + 9)?;
    let lease_seconds = NonZeroU64::new(3_600).ok_or("nonzero lease")?;
    let source_ref = SessionRef::new(session_id, source_branch);
    let genesis_checkpoint = conformance::checkpoint(
        conformance::topology(session_id, source_branch, genesis_id, None, 0, None, 1)?,
        1,
        RuntimeDurability::Required,
    )?;
    let genesis_bundle = conformance::bundle(&genesis_checkpoint, None)?;
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store
        .create(CreateSession::new(
            source_ref,
            Timestamp::new(50, 0)?,
            source_owner,
            lease_seconds,
            genesis_bundle.clone(),
        ))
        .await?;
    let source_checkpoint = conformance::checkpoint(
        conformance::topology(
            session_id,
            source_branch,
            source_id,
            Some(genesis_id),
            1,
            None,
            created.lease().fence().get(),
        )?,
        2,
        RuntimeDurability::Required,
    )?;
    let source_bundle = conformance::bundle(&source_checkpoint, Some(&genesis_bundle))?;
    let source = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            source_bundle.clone(),
        ))
        .await?;
    let lineage = ForkLineage {
        session_id,
        branch_id: source_branch,
        checkpoint_id: source_id,
        branch_revision: 1,
    };
    let context = match transition {
        BranchTransitionMode::OrdinaryFork => source_checkpoint
            .manifest()
            .canonical()
            .context()
            .ordinary_fork(source_id, session_id, target_branch, [0x91; 32])?,
        BranchTransitionMode::Rebind => source_checkpoint.manifest().canonical().context().rebind(
            source_id,
            session_id,
            target_branch,
            [0x92; 32],
        )?,
    };
    let target_checkpoint = conformance::checkpoint_with_context(
        conformance::topology(
            session_id,
            target_branch,
            target_id,
            Some(source_id),
            0,
            Some(lineage),
            1,
        )?,
        2,
        RuntimeDurability::Required,
        context,
        rebound,
    )?;
    let workflow = conformance::workflow(workflow_seed)?;
    let semantic_request = conformance::semantic_request(match transition {
        BranchTransitionMode::OrdinaryFork => b"reopen ordinary fork",
        BranchTransitionMode::Rebind => b"reopen rebind",
    })?;
    let request = ForkBranch::new(
        workflow,
        semantic_request,
        created.lease().clone(),
        SessionRevision::INITIAL,
        source.head(),
        target_branch,
        target_owner,
        lease_seconds,
        conformance::bundle(&target_checkpoint, Some(&source_bundle))?,
    )
    .with_transition(transition);
    let accepted = store.fork(request.clone()).await?;
    assert_eq!(store.fork(request).await?, accepted);
    store.release(created.lease().clone()).await?;
    drop(store);

    let reopened = SqliteSessionStore::open(&db.path)?;
    let source_lease = reopened
        .acquire(plakous_agent::internal::AcquireBranch::new(
            source_ref,
            recovery_owner,
            lease_seconds,
        ))
        .await?;
    let before_recovery = reopened.stats().await?;
    let recovered = reopened
        .recover_fork(
            RecoverFork::new(
                workflow,
                semantic_request,
                source_lease.clone(),
                source.head(),
                target_branch,
            )
            .with_transition(transition),
        )
        .await?;
    assert_eq!(recovered, accepted);
    assert_eq!(recovered.operation().semantic_request(), semantic_request);
    assert_eq!(
        reopened
            .recover_fork(
                RecoverFork::new(
                    workflow,
                    conformance::semantic_request(b"different reopened request")?,
                    source_lease.clone(),
                    source.head(),
                    target_branch,
                )
                .with_transition(transition),
            )
            .await,
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(reopened.stats().await?, before_recovery);
    reopened.release(recovered.lease().clone()).await?;
    let target_lease = reopened
        .acquire(plakous_agent::internal::AcquireBranch::new(
            SessionRef::new(session_id, target_branch),
            target_reopen_owner,
            lease_seconds,
        ))
        .await?;
    assert!(target_lease.fence() > recovered.lease().fence());
    reopened.release(target_lease).await?;
    reopened.release(source_lease).await?;
    Ok(())
}

#[tokio::test]
async fn lost_fork_ack_reopens_by_stable_workflow_without_republication()
-> Result<(), Box<dyn Error>> {
    for (base, transition, rebound, workflow_seed) in [
        (2_000, BranchTransitionMode::OrdinaryFork, false, 0xa1),
        (2_100, BranchTransitionMode::Rebind, true, 0xa2),
    ] {
        let db = Database::new();
        assert_lost_fork_ack_reopens(&db, base, transition, rebound, workflow_seed).await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_backend_satisfies_shared_semantic_conformance_and_reopens()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    seed_shared_corpus(&db, RuntimeDurability::Required).await?;
    let reopened = SqliteSessionStore::open(&db.path)?;
    let stats = reopened.stats().await?;
    assert_eq!(stats.sessions(), 1);
    assert_eq!(stats.branches(), 3);
    assert_eq!(stats.checkpoints(), 5);
    assert!(stats.objects() >= stats.checkpoints());
    assert!(stats.object_bytes() > 0);
    assert_eq!(stats.semantic_mutations(), 15);
    let committed = "00000000-0000-0000-0000-00000000000b".parse::<CheckpointId>()?;
    assert_eq!(
        reopened.load_checkpoint(committed).await?.reference(),
        reopened
            .recover_commit(committed)
            .await?
            .receipt()
            .checkpoint()
    );
    drop(reopened);
    let connection = Connection::open(&db.path)?;
    connection.execute(
        "UPDATE commit_receipts SET owner=?1 WHERE checkpoint_id=?2",
        (
            "00000000-0000-0000-0000-0000000000ff",
            committed.to_string(),
        ),
    )?;
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open_verified(&db.path).expect_err("receipt corruption"),
        SessionStoreError::corrupt()
    );
    Ok(())
}

#[test]
fn unsupported_runtime_version_in_wal_is_not_checkpointed_by_open() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    drop(SqliteSessionStore::open(&db.path)?);
    let writer = Connection::open(&db.path)?;
    writer.execute_batch("PRAGMA wal_autocheckpoint=0;")?;
    writer.pragma_update(None, "user_version", 17_i64)?;
    let before = fs::read(&db.path)?;
    let wal_path = db.path.with_extension("sqlite3-wal");
    let wal_before = fs::read(&wal_path)?;
    assert_eq!(
        SqliteSessionStore::open(&db.path).expect_err("unsupported WAL identity"),
        SessionStoreError::UnsupportedSchema
    );
    assert_eq!(fs::read(&db.path)?, before);
    assert_eq!(fs::read(&wal_path)?, wal_before);
    drop(writer);
    Ok(())
}

#[test]
fn unsupported_detached_wal_inspection_preserves_persistent_bytes() -> Result<(), Box<dyn Error>> {
    for retain_shm in [false, true] {
        let db = Database::new();
        drop(SqliteSessionStore::open(&db.path)?);
        let writer = Connection::open(&db.path)?;
        writer.execute_batch("PRAGMA wal_autocheckpoint=0;")?;
        writer.pragma_update(None, "user_version", 17_i64)?;
        let main_before = fs::read(&db.path)?;
        let wal_path = db.path.with_extension("sqlite3-wal");
        let shm_path = db.path.with_extension("sqlite3-shm");
        let wal_before = fs::read(&wal_path)?;
        let shm_before = fs::read(&shm_path)?;
        assert_eq!(&main_before[60..64], &25_u32.to_be_bytes());
        assert!(
            wal_before.len() > 32,
            "unsupported version lives in WAL frames"
        );
        drop(writer);

        // Restore a quiescent WAL cohort after the writer's closing checkpoint.
        // Inspection must read the WAL even when its disposable index is absent.
        fs::write(&db.path, &main_before)?;
        fs::write(&wal_path, &wal_before)?;
        if retain_shm {
            fs::write(&shm_path, shm_before)?;
        } else if shm_path.exists() {
            fs::remove_file(&shm_path)?;
        }
        for _ in 0..2 {
            assert_eq!(
                SqliteSessionStore::classify_persistence(&db.path),
                Err(SessionStoreError::UnsupportedSchema)
            );
            assert_eq!(
                SqliteSessionStore::open(&db.path).expect_err("unsupported WAL open"),
                SessionStoreError::UnsupportedSchema
            );
            assert_eq!(
                SqliteSessionStore::open_verified(&db.path)
                    .expect_err("unsupported verified WAL open"),
                SessionStoreError::UnsupportedSchema
            );
            assert_eq!(fs::read(&db.path)?, main_before);
            assert_eq!(fs::read(&wal_path)?, wal_before);
            // SHM may be reconstructed or updated by SQLite's read-only reader.
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn fifo_runtime_path_is_refused_without_waiting_for_a_writer() -> Result<(), Box<dyn Error>> {
    const CHILD_PATH: &str = "PLAK_STORE_FIFO_ADMISSION_TEST";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        assert_eq!(
            SqliteSessionStore::open(&path).expect_err("FIFO is not a database"),
            SessionStoreError::BackendUnavailable
        );
        assert_eq!(
            SqliteSessionStore::classify_persistence(&path)
                .expect_err("FIFO cannot be classified as a database"),
            SessionStoreError::BackendUnavailable
        );
        return Ok(());
    }

    let db = Database::new();
    // rustix::fs::mkfifoat is not exported on Apple; the host `mkfifo` is.
    let status = std::process::Command::new("mkfifo")
        .arg(&db.path)
        .status()?;
    assert!(status.success(), "mkfifo {status}");
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "sqlite::fifo_runtime_path_is_refused_without_waiting_for_a_writer",
            "--nocapture",
        ])
        .env(CHILD_PATH, &db.path)
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            assert!(
                status.success(),
                "FIFO admission subprocess failed: {status}"
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            panic!("runtime admission blocked opening a FIFO without a writer");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn unsupported_runtime_schemas_refuse_without_changing_files() -> Result<(), Box<dyn Error>> {
    for version in [13_i64, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 26, 100] {
        let db = Database::new();
        drop(SqliteSessionStore::open(&db.path)?);
        let connection = Connection::open(&db.path)?;
        connection.pragma_update(None, "user_version", version)?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
        drop(connection);
        let before = fs::read(&db.path)?;
        assert_eq!(
            SqliteSessionStore::classify_persistence(&db.path),
            Err(SessionStoreError::UnsupportedSchema),
            "schema {version}"
        );
        assert_eq!(
            SqliteSessionStore::open(&db.path).expect_err("unsupported open"),
            SessionStoreError::UnsupportedSchema,
            "schema {version}"
        );
        assert_eq!(
            SqliteSessionStore::open_verified(&db.path).expect_err("unsupported verified open"),
            SessionStoreError::UnsupportedSchema,
            "schema {version}"
        );
        assert_eq!(fs::read(&db.path)?, before, "schema {version} was modified");
        assert!(!db.path.with_extension("sqlite3-wal").exists());
        assert!(!db.path.with_extension("sqlite3-shm").exists());
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_semantic_deletion_reopens_and_recovers() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path)?;
    conformance::run_delete_conformance(&store, RuntimeDurability::Required, 20_000).await?;
    drop(store);
    let reopened = SqliteSessionStore::open(&db.path)?;
    let session_id = conformance::id::<SessionId>(20_000)?;
    let request = DeleteSession::new(session_id, SessionRevision::from_store(1));
    let receipt = reopened.recover_delete(request).await?;
    assert_eq!(receipt.request(), request);
    let stats = reopened.stats().await?;
    assert_eq!(stats.sessions(), 0);
    assert_eq!(stats.branches(), 0);
    assert_eq!(stats.checkpoints(), 0);
    assert_eq!(stats.objects(), 0);
    assert_eq!(stats.object_bytes(), 0);
    assert_eq!(stats.tombstones(), 2);
    drop(reopened);
    let connection = Connection::open(&db.path)?;
    connection.execute(
        "UPDATE delete_tombstones SET reclaimed_checkpoints=reclaimed_checkpoints+1 WHERE session_id=?1",
        [session_id.to_string()],
    )?;
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open_verified(&db.path).expect_err("tampered deletion receipt"),
        SessionStoreError::corrupt()
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_deletion_removes_complete_multi_branch_authority() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    seed_shared_corpus(&db, RuntimeDurability::Required).await?;
    let store = SqliteSessionStore::open(&db.path)?;
    let page = store
        .list(
            plakous_agent::internal::SessionQuery::new(NonZeroUsize::new(8).ok_or("nonzero page")?)
                .include_archived(true),
        )
        .await?;
    let head = page.items().first().ok_or("archived session")?.head();
    assert!(head.is_archived());
    let request = DeleteSession::new(head.session_id(), head.revision());
    let receipt = store.delete(request).await?;
    assert!(receipt.reclaimed_checkpoints() > 1);
    drop(store);

    let connection = Connection::open(&db.path)?;
    for table in [
        "sessions",
        "branches",
        "checkpoints",
        "create_receipts",
        "fork_receipts",
        "commit_receipts",
        "archive_receipts",
        "runtime_objects",
        "runtime_object_edges",
    ] {
        assert_eq!(
            connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })?,
            0,
            "retained deleted authority in {table}"
        );
    }
    assert_eq!(
        connection.query_row("SELECT count(*) FROM delete_tombstones", [], |row| {
            row.get::<_, i64>(0)
        })?,
        1
    );
    drop(connection);
    let reopened = SqliteSessionStore::open(&db.path)?;
    assert_eq!(reopened.recover_delete(request).await?, receipt);
    Ok(())
}

#[tokio::test]
async fn sqlite_best_effort_uses_the_stronger_full_commit_boundary() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    seed_shared_corpus(&db, RuntimeDurability::BestEffort).await?;
    SqliteSessionStore::open(&db.path)?;
    Ok(())
}

#[tokio::test]
async fn cached_fork_semantics_corruption_fails_closed_on_reopen() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    seed_shared_corpus(&db, RuntimeDurability::Required).await?;
    let connection = Connection::open(&db.path)?;
    connection.execute(
        "UPDATE checkpoints SET semantic_key=zeroblob(length(semantic_key))
         WHERE checkpoint_id='00000000-0000-0000-0000-00000000000b'",
        [],
    )?;
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open_verified(&db.path).expect_err("semantic key corruption"),
        SessionStoreError::corrupt()
    );
    Ok(())
}

#[tokio::test]
async fn fork_receipts_check_refuses_removed_transition_tags() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    seed_shared_corpus(&db, RuntimeDurability::Required).await?;
    let connection = Connection::open(&db.path)?;
    for tag in [3, 4] {
        let error = connection
            .execute("UPDATE fork_receipts SET transition=?1", [tag])
            .expect_err("removed transition tag must violate CHECK");
        let rusqlite::Error::SqliteFailure(code, _) = error else {
            return Err("expected SQLite constraint failure".into());
        };
        assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
    }
    Ok(())
}

#[tokio::test]
async fn fork_exact_binding_corruption_fails_closed_on_reopen() -> Result<(), Box<dyn Error>> {
    for corruption in [
        "UPDATE fork_receipts SET workflow_id=zeroblob(32)
         WHERE workflow_id=(SELECT workflow_id FROM fork_receipts LIMIT 1)",
        "UPDATE fork_receipts SET transition=CASE transition WHEN 1 THEN 2 ELSE 1 END
         WHERE workflow_id=(SELECT workflow_id FROM fork_receipts LIMIT 1)",
        "UPDATE fork_receipts SET semantic_request=zeroblob(32)
         WHERE workflow_id=(SELECT workflow_id FROM fork_receipts LIMIT 1)",
        "UPDATE fork_receipts SET semantic_request_version=semantic_request_version+1
         WHERE workflow_id=(SELECT workflow_id FROM fork_receipts LIMIT 1)",
    ] {
        let db = Database::new();
        seed_shared_corpus(&db, RuntimeDurability::Required).await?;
        let connection = Connection::open(&db.path)?;
        connection.execute(corruption, [])?;
        drop(connection);
        assert_eq!(
            SqliteSessionStore::open_verified(&db.path).expect_err("fork receipt corruption"),
            SessionStoreError::corrupt()
        );
    }
    Ok(())
}

#[tokio::test]
async fn bulk_authority_snapshot_rejects_the_same_relational_corruption_classes()
-> Result<(), Box<dyn Error>> {
    for corruption in [
        "UPDATE commit_receipts SET expected_checkpoint=checkpoint_id
         WHERE checkpoint_id=(SELECT checkpoint_id FROM commit_receipts LIMIT 1)",
        "UPDATE checkpoints SET parent_id=checkpoint_id
         WHERE checkpoint_id=(SELECT checkpoint_id FROM checkpoints WHERE parent_id IS NOT NULL LIMIT 1)",
        "UPDATE branches SET revision=zeroblob(8)
         WHERE (session_id,branch_id)=(SELECT session_id,branch_id FROM branches WHERE revision<>zeroblob(8) LIMIT 1)",
        "UPDATE checkpoints SET fork_revision=x'0000000000000063'
         WHERE checkpoint_id=(SELECT checkpoint_id FROM checkpoints WHERE fork_revision IS NOT NULL LIMIT 1)",
        // A dangling parent is admissible only when it is a recorded pruned id.
        "UPDATE checkpoints SET parent_id='00000000-0000-4000-8000-000000000001'
         WHERE checkpoint_id=(SELECT checkpoint_id FROM checkpoints WHERE parent_id IS NOT NULL LIMIT 1)",
    ] {
        let db = Database::new();
        seed_shared_corpus(&db, RuntimeDurability::Required).await?;
        let connection = Connection::open(&db.path)?;
        assert_eq!(connection.execute(corruption, [])?, 1);
        drop(connection);
        assert!(
            matches!(
                SqliteSessionStore::open_verified(&db.path),
                Err(SessionStoreError::Corrupt(_))
            ),
            "bulk snapshot admitted relational corruption: {corruption}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn content_addressed_object_corruption_fails_closed_on_reopen() -> Result<(), Box<dyn Error>>
{
    let db = Database::new();
    seed_shared_corpus(&db, RuntimeDurability::Required).await?;
    let connection = Connection::open(&db.path)?;
    connection.execute(
        "UPDATE runtime_object_bytes SET bytes=zeroblob(length(bytes))
         WHERE id=(SELECT id FROM runtime_objects ORDER BY kind,codec,digest LIMIT 1)",
        [],
    )?;
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open_verified(&db.path).expect_err("object corruption"),
        SessionStoreError::corrupt()
    );
    SqliteSessionStore::open(&db.path)
        .expect("ordinary attach admits shape without object rebuild");
    Ok(())
}

#[tokio::test]
async fn deleting_any_immutable_receipt_fails_closed_on_reopen() -> Result<(), Box<dyn Error>> {
    for deletion in [
        "DELETE FROM create_receipts",
        "DELETE FROM commit_receipts WHERE checkpoint_id=(SELECT checkpoint_id FROM commit_receipts LIMIT 1)",
        "DELETE FROM fork_receipts WHERE workflow_id=(SELECT workflow_id FROM fork_receipts LIMIT 1)",
        "DELETE FROM archive_receipts",
    ] {
        let db = Database::new();
        seed_shared_corpus(&db, RuntimeDurability::Required).await?;
        let connection = Connection::open(&db.path)?;
        connection.execute(deletion, [])?;
        drop(connection);
        assert_eq!(
            SqliteSessionStore::open_verified(&db.path).expect_err("missing receipt"),
            SessionStoreError::corrupt()
        );
    }
    Ok(())
}

#[tokio::test]
async fn fresh_schema_has_required_integrity_pragmas_and_reopens() {
    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path).expect("open");
    assert!(store.flush().await.expect("flush").is_complete());
    drop(store);
    let connection = Connection::open(&db.path).expect("inspect");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .expect("version"),
        25
    );
    assert_eq!(
        connection
            .query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
            .expect("auto_vacuum"),
        2,
        "new stores return pruned pages incrementally"
    );
    assert_eq!(
        connection
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .expect("wal"),
        "wal"
    );
    connection
        .execute(
            "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(1,7,zeroblob(32),1)",
            [],
        )
        .expect_err("predecessor object codec must violate the current schema constraint");
    connection
        .execute(
            "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(19,25,zeroblob(32),1)",
            [],
        )
        .expect("provider-configuration object must satisfy the current schema constraint");
    connection
        .execute(
            "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(20,9,zeroblob(32),1)",
            [],
        )
        .expect_err("unknown object kind must violate the current schema constraint");
    connection
        .execute(
            "DELETE FROM runtime_objects WHERE kind=19 AND codec=25 AND digest=zeroblob(32)",
            [],
        )
        .expect("remove schema probe");
    drop(connection);
    SqliteSessionStore::open(&db.path).expect("reopen");
}

#[test]
fn runtime_is_the_reviewed_fixed_bundled_sqlite() {
    let connection = Connection::open_in_memory().expect("embedded SQLite");
    let (version, source_id): (String, String) = connection
        .query_row("SELECT sqlite_version(), sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("runtime identity");
    assert_eq!(version, BUNDLED_SQLITE_VERSION);
    assert_eq!(source_id, BUNDLED_SQLITE_SOURCE_ID);
    for option in BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS {
        assert_eq!(
            connection
                .query_row("SELECT sqlite_compileoption_used(?1)", [option], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("compile option"),
            1,
            "missing reviewed bundled compile option {option}"
        );
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the WAL reset regression keeps one complete concurrent sequence auditable"
)]
fn bundled_sqlite_preserves_commits_across_concurrent_wal_reset_sequence() {
    const INITIAL_ROWS: i64 = 32;
    const ROUNDS: i64 = 32;
    const GROWTH_ROWS: i64 = 4;
    const PAYLOAD_BYTES: usize = 2_048;
    const PAYLOAD_BYTES_I64: i64 = 2_048;

    let db = Database::new();
    let mut anchor = Connection::open(&db.path).expect("anchor connection");
    anchor
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE wal_reset_probe(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);",
        )
        .expect("WAL probe schema");
    let payload = vec![0x5a_u8; PAYLOAD_BYTES];
    {
        let tx = anchor.transaction().expect("initial transaction");
        for id in 1..=INITIAL_ROWS {
            tx.execute(
                "INSERT INTO wal_reset_probe VALUES(?1, ?2)",
                rusqlite::params![id, &payload],
            )
            .expect("initial row");
        }
        tx.commit().expect("initial commit");
    }

    let mut next_id = INITIAL_ROWS + 1;
    for _ in 0..ROUNDS {
        let completed: (i64, i64, i64) = anchor
            .query_row("PRAGMA wal_checkpoint(RESTART)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("complete checkpoint before reset race");
        assert_eq!(completed.0, 0, "checkpoint must obtain reset authority");
        assert_eq!(completed.1, completed.2, "checkpoint must be complete");

        let barrier = Arc::new(Barrier::new(3));
        let checkpoint_path = db.path.clone();
        let checkpoint_barrier = Arc::clone(&barrier);
        let checkpointer = std::thread::spawn(move || {
            let connection = Connection::open(checkpoint_path).expect("checkpointer connection");
            connection
                .busy_timeout(Duration::from_secs(3))
                .expect("checkpoint timeout");
            checkpoint_barrier.wait();
            connection
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .expect("concurrent checkpoint")
        });
        let writer_path = db.path.clone();
        let writer_barrier = Arc::clone(&barrier);
        let writer_payload = payload.clone();
        let writer_id = next_id;
        next_id += 1;
        let writer = std::thread::spawn(move || {
            let connection = Connection::open(writer_path).expect("writer connection");
            connection
                .busy_timeout(Duration::from_secs(3))
                .expect("writer timeout");
            writer_barrier.wait();
            connection
                .execute(
                    "INSERT INTO wal_reset_probe VALUES(?1, ?2)",
                    rusqlite::params![writer_id, writer_payload],
                )
                .expect("WAL-reset writer commit");
        });
        barrier.wait();
        let _ = checkpointer.join().expect("checkpointer joined");
        writer.join().expect("writer joined");

        let tx = anchor.transaction().expect("post-reset growth transaction");
        for _ in 0..GROWTH_ROWS {
            tx.execute(
                "INSERT INTO wal_reset_probe VALUES(?1, ?2)",
                rusqlite::params![next_id, &payload],
            )
            .expect("post-reset growth row");
            next_id += 1;
        }
        tx.commit().expect("post-reset growth commit");
    }

    let expected_rows = INITIAL_ROWS + ROUNDS * (1 + GROWTH_ROWS);
    let completed: (i64, i64, i64) = anchor
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("final checkpoint");
    assert_eq!(completed.0, 0);
    drop(anchor);

    let reopened = Connection::open(&db.path).expect("cold reopen");
    let facts: (i64, i64, i64, i64) = reopened
        .query_row(
            "SELECT count(*), min(id), max(id), sum(length(payload)) FROM wal_reset_probe",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("all committed rows");
    assert_eq!(facts.0, expected_rows);
    assert_eq!(facts.1, 1);
    assert_eq!(facts.2, expected_rows);
    assert_eq!(facts.3, expected_rows * PAYLOAD_BYTES_I64);
    assert_eq!(
        reopened
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("integrity check"),
        "ok"
    );
}

#[tokio::test]
async fn unsupported_versions_and_wrong_database_identity_are_distinct() {
    for (application_id, version) in [
        (0x504c_4b35_i64, 3_i64),
        (0x504c_4b35_i64, 4_i64),
        (0x504c_4b35_i64, 5_i64),
        (0x504c_4b35_i64, 6_i64),
        (0x504c_4b35_i64, 7_i64),
        (0x504c_4b35_i64, 8_i64),
        (0x504c_4b35_i64, 9_i64),
        (0x504c_4b35_i64, 10_i64),
        (0x504c_4b35_i64, 11_i64),
        (0x504c_4b35_i64, 12_i64),
        (0x504c_4b35_i64, 2_i64),
        (0x504c_4b35_i64, 99_i64),
        (0x504c_414b_i64, 1_i64),
    ] {
        let db = Database::new();
        let c = Connection::open(&db.path).expect("fixture");
        c.pragma_update(None, "application_id", application_id)
            .expect("application id");
        c.pragma_update(None, "user_version", version)
            .expect("version");
        drop(c);
        assert_eq!(
            SqliteSessionStore::open(&db.path).expect_err("reject"),
            if application_id == 0x504c_4b35 {
                SessionStoreError::UnsupportedSchema
            } else {
                SessionStoreError::corrupt()
            }
        );
    }
}

#[tokio::test]
async fn shutdown_is_stable_and_flush_closes() {
    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path).expect("open");
    let report = store.shutdown().await.expect("shutdown");
    assert_eq!(store.shutdown().await, Ok(report));
    assert_eq!(store.flush().await, Err(SessionStoreError::Shutdown));
    let missing = "00000000-0000-0000-0000-000000000001"
        .parse::<CheckpointId>()
        .expect("checkpoint id");
    assert_eq!(
        store.recover_commit(missing).await,
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(
        store.load_checkpoint(missing).await,
        Err(SessionStoreError::NotFound)
    );
}

#[tokio::test]
async fn counter_reads_are_side_effect_free() {
    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path).expect("open");
    let first = store.stats().await.expect("stats");
    let second = store.stats().await.expect("stats");
    assert_eq!(first, second);
    assert_eq!(first.semantic_mutations(), 0);
}

#[test]
fn sqlite_retention_defaults_are_distinct_from_memory_bounds() {
    let sqlite = SqliteStoreOptions::default();
    let durable = sqlite.limits();
    let memory = MemoryStoreLimits::default();
    assert_eq!(durable.max_object_bytes().get(), 4_294_967_296);
    assert_eq!(durable.max_objects().get(), 4_194_304);
    assert_eq!(durable.max_checkpoints().get(), 524_288);
    assert_eq!(memory.max_object_bytes().get(), 1_073_741_824);
    assert_eq!(memory.max_objects().get(), 1_048_576);
    assert_eq!(memory.max_checkpoints().get(), 131_072);
    assert_eq!(durable.max_sessions(), memory.max_sessions());
    assert_eq!(
        durable.max_branches_per_session(),
        memory.max_branches_per_session()
    );
    assert_eq!(durable.max_page_size(), memory.max_page_size());
    assert_eq!(sqlite.queue_capacity().get(), 64);
}

fn checkpoint_options(checkpoints: usize) -> Result<SqliteStoreOptions, Box<dyn Error>> {
    let defaults = MemoryStoreLimits::default();
    Ok(SqliteStoreOptions::new(
        MemoryStoreLimits::new(
            defaults.max_sessions(),
            defaults.max_branches_per_session(),
            NonZeroUsize::new(checkpoints).ok_or("nonzero checkpoints")?,
            defaults.max_objects(),
            defaults.max_object_bytes(),
            defaults.max_page_size(),
        ),
        NonZeroUsize::new(8).ok_or("nonzero queue")?,
    ))
}

#[tokio::test]
async fn sqlite_admission_classes_stop_at_their_thresholds() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let store = SqliteSessionStore::open_with_clock_and_options(
        &db.path,
        Arc::new(ObservedLeaseClock::new(100)),
        checkpoint_options(conformance::ADMISSION_CHECKPOINTS)?,
    )?;
    conformance::run_admission_conformance(&store, RuntimeDurability::Required, 70_000).await?;
    drop(store);
    SqliteSessionStore::open_verified(&db.path)?;
    Ok(())
}

#[tokio::test]
async fn an_over_limit_store_opens_refuses_ordinary_writes_and_deletes()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let store =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    let mut kept = conformance::AdmissionChain::create(
        &store,
        RuntimeDurability::Required,
        9_000,
        StoreAdmission::Ordinary,
    )
    .await??;
    let removed = conformance::AdmissionChain::create(
        &store,
        RuntimeDurability::Required,
        9_100,
        StoreAdmission::Ordinary,
    )
    .await??;
    for _ in 0..2 {
        kept.commit(&store, StoreAdmission::Ordinary).await??;
    }
    drop(store);

    // Four retained checkpoints against a bound of two: past every class.
    let reopened = SqliteSessionStore::open_with_clock_and_options(
        &db.path,
        Arc::new(ObservedLeaseClock::new(100)),
        checkpoint_options(2)?,
    )?;
    let usage = reopened.usage().await?.bounds()[0];
    assert_eq!((usage.used(), usage.limit()), (4, 2));
    assert!(usage.ordinary_exhausted());
    for admission in [
        StoreAdmission::Ordinary,
        StoreAdmission::Settlement,
        StoreAdmission::Recovery,
    ] {
        assert!(matches!(
            kept.commit(&reopened, admission).await?,
            Err(SessionStoreError::Capacity(_))
        ));
    }
    removed.delete(&reopened).await?;
    assert_eq!(reopened.usage().await?.bounds()[0].used(), 3);
    drop(reopened);
    assert_eq!(
        SqliteSessionStore::inspect_usage(&db.path)?
            .ok_or("current store")?
            .bounds()[0]
            .used(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn bounded_actor_queue_rejects_excess_work_without_blocking_the_caller()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let store = SqliteSessionStore::open_with_clock_and_options(
        &db.path,
        Arc::new(ObservedLeaseClock::new(100)),
        SqliteStoreOptions::new(
            MemoryStoreLimits::default(),
            NonZeroUsize::new(1).ok_or("nonzero queue")?,
        ),
    )?;
    let blocker = Connection::open(&db.path)?;
    blocker.execute_batch("PRAGMA busy_timeout=0; BEGIN IMMEDIATE;")?;

    let first = store.create(seed(10_000, RuntimeDurability::Required, 1)?.request);
    let second = store.create(seed(10_010, RuntimeDurability::Required, 1)?.request);
    let third = store.create(seed(10_020, RuntimeDurability::Required, 1)?.request);
    drop(blocker);

    let results = tokio::join!(first, second, third);
    let mut accepted = 0;
    let mut saturated = 0;
    for result in [results.0, results.1, results.2] {
        match result {
            Ok(_) => accepted += 1,
            Err(SessionStoreError::BackendBusy) => saturated += 1,
            Err(error) => return Err(error.into()),
        }
    }
    assert!(
        accepted <= 2,
        "one running job plus one queued job is the bound"
    );
    assert!(
        saturated >= 1,
        "excess work must report bounded backpressure"
    );
    assert_eq!(accepted + saturated, 3);
    assert_eq!(store.stats().await?.semantic_mutations(), accepted);
    Ok(())
}

#[tokio::test]
async fn lease_expiry_is_sampled_after_waiting_for_write_authority() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let clock = Arc::new(ObservedLeaseClock::new(100));
    let initial = seed(15_000, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let created = store.create(initial.request.clone()).await?;
    let committed_id = conformance::id::<CheckpointId>(15_010)?;
    let commit = CommitCheckpoint::new(
        created.lease().clone(),
        created.branch(),
        successor(
            &initial,
            committed_id,
            initial.checkpoint_id,
            1,
            &initial.bundle,
        )?,
    );

    let blocker = Connection::open(&db.path)?;
    blocker.execute_batch("PRAGMA busy_timeout=0; BEGIN IMMEDIATE;")?;
    let observation = clock.observe_next();
    let pending = store.commit(commit);
    assert!(
        observation
            .recv_timeout(Duration::from_millis(250))
            .is_err(),
        "the actor must obtain write authority before sampling lease time"
    );
    clock.set(4_000);
    drop(blocker);

    assert_eq!(pending.await, Err(SessionStoreError::LeaseExpired));
    assert_eq!(
        store.load_branch(initial.selector).await?.head(),
        created.branch()
    );
    assert_eq!(
        store.recover_commit(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(
        store.load_checkpoint(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    Ok(())
}

#[tokio::test]
async fn renewed_token_lost_ack_is_exact_after_reopen_and_expires_closed()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let clock = Arc::new(ObservedLeaseClock::new(100));
    let initial = seed(15_500, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let created = store.create(initial.request.clone()).await?;

    assert_eq!(
        store.renew(created.lease().clone()).await?,
        created.lease().clone()
    );
    clock.set(101);
    let renewed = store.renew(created.lease().clone()).await?;
    drop(store);

    let reopened = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    assert_eq!(reopened.renew(created.lease().clone()).await?, renewed);
    clock.set(renewed.expires_at().seconds());
    assert_eq!(
        reopened.renew(created.lease().clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    assert_eq!(
        reopened.renew(renewed).await,
        Err(SessionStoreError::LeaseExpired)
    );
    Ok(())
}

/// A holder that outlived its lease without being replaced — a laptop asleep,
/// a stopped process — gets it back under the same fence. The moment anybody
/// else acquires, or this store no longer holds the host lock it took, the
/// same request is refused, so a revival can never overlap another owner.
#[tokio::test]
async fn an_expired_lease_revives_under_its_fence_only_when_nobody_took_it()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let clock = Arc::new(ObservedLeaseClock::new(100));
    let initial = seed(15_700, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let created = store.create(initial.request.clone()).await?;
    store.release(created.lease().clone()).await?;
    let acquire = |owner: u128| -> Result<_, Box<dyn Error>> {
        Ok(plakous_agent::internal::AcquireBranch::new(
            initial.selector,
            conformance::id::<RunId>(owner)?,
            NonZeroU64::new(30).ok_or("duration")?,
        ))
    };
    let held = store.acquire(acquire(15_710)?).await?;

    // Not yet expired: revival is an ordinary renewal.
    clock.set(110);
    let renewed = store.revive(held.clone()).await?;
    assert_eq!(renewed.fence(), held.fence());

    // Expired and untouched: the same fence, a fresh interval.
    clock.set(renewed.expires_at().seconds() + 600);
    assert_eq!(
        store.renew(renewed.clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    let revived = store.revive(renewed.clone()).await?;
    assert_eq!(revived.fence(), held.fence());
    assert_eq!(revived.owner(), held.owner());
    assert!(revived.expires_at().seconds() > store.lease_now()?.seconds());
    assert_eq!(store.renew(revived.clone()).await?, revived);

    // Expired again, but another owner acquired first: refused for good.
    clock.set(revived.expires_at().seconds() + 600);
    let other = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let taken = other.acquire(acquire(15_720)?).await?;
    assert!(taken.fence() > revived.fence());
    assert_eq!(
        store.revive(revived.clone()).await,
        Err(SessionStoreError::FenceSuperseded)
    );
    other.release(taken).await?;
    drop(other);

    // A store that no longer holds the host lock it took with the lease cannot
    // prove it was only suspended.
    let lease = store.acquire(acquire(15_730)?).await?;
    clock.set(lease.expires_at().seconds() + 600);
    drop(store);
    let reopened = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    assert_eq!(
        reopened.revive(lease).await,
        Err(SessionStoreError::FenceSuperseded)
    );
    Ok(())
}

/// Create and fork grant leases exactly as acquire does, host claim included.
///
/// Without the claim a suspended holder of a create- or fork-granted lease could
/// never revive it on SQLite, although the memory store would, and a holder
/// that died before releasing it would leave its branch to the lease's clock.
#[tokio::test]
async fn create_and_fork_granted_leases_revive_under_their_fence() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let clock = Arc::new(ObservedLeaseClock::new(100));
    let initial = seed(15_800, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let created = store.create(initial.request.clone()).await?;
    let target_branch = conformance::id::<BranchId>(15_810)?;
    let source_checkpoint = store.load_checkpoint(initial.checkpoint_id).await?;
    let context = source_checkpoint
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .ordinary_fork(
            initial.checkpoint_id,
            initial.selector.session_id(),
            target_branch,
            [0x93; 32],
        )?;
    let target_checkpoint = conformance::checkpoint_with_context(
        conformance::topology(
            initial.selector.session_id(),
            target_branch,
            conformance::id::<CheckpointId>(15_811)?,
            Some(initial.checkpoint_id),
            0,
            Some(ForkLineage {
                session_id: initial.selector.session_id(),
                branch_id: initial.selector.branch_id(),
                checkpoint_id: initial.checkpoint_id,
                branch_revision: 0,
            }),
            1,
        )?,
        1,
        RuntimeDurability::Required,
        context,
        false,
    )?;
    let forked = store
        .fork(ForkBranch::new(
            conformance::workflow(203)?,
            conformance::semantic_request(b"fork-granted lease revival")?,
            created.lease().clone(),
            SessionRevision::INITIAL,
            created.branch(),
            target_branch,
            conformance::id::<RunId>(15_813)?,
            NonZeroU64::new(30).ok_or("duration")?,
            conformance::bundle(&target_checkpoint, Some(&initial.bundle))?,
        ))
        .await?;

    clock.set(
        created
            .lease()
            .expires_at()
            .seconds()
            .max(forked.lease().expires_at().seconds())
            + 600,
    );
    for granted in [created.lease().clone(), forked.lease().clone()] {
        assert_eq!(
            store.renew(granted.clone()).await,
            Err(SessionStoreError::LeaseExpired)
        );
        let revived = store.revive(granted.clone()).await?;
        assert_eq!(revived.fence(), granted.fence());
        store.release(revived).await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_releases_expired_grants_without_releasing_replaced_grants()
-> Result<(), Box<dyn Error>> {
    let database = Database::new();
    let clock = Arc::new(conformance::DomainLeaseClock::new(
        LeaseDomain::new([1; 16], [2; 32]),
        100,
    )?);
    let store = SqliteSessionStore::open_with_clock(&database.path, clock.clone())?;
    conformance::run_expired_release_conformance(&store, RuntimeDurability::Required, &clock).await
}

#[tokio::test]
async fn ignored_receipt_insert_rolls_back_and_persistent_triggers_are_rejected()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let initial = seed(16_000, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store.create(initial.request.clone()).await?;
    let committed_id = conformance::id::<CheckpointId>(16_010)?;
    let commit = CommitCheckpoint::new(
        created.lease().clone(),
        created.branch(),
        successor(
            &initial,
            committed_id,
            initial.checkpoint_id,
            1,
            &initial.bundle,
        )?,
    );
    let injector = Connection::open(&db.path)?;
    injector.execute_batch(
        "CREATE TRIGGER ignore_commit_receipt
         BEFORE INSERT ON commit_receipts
         BEGIN SELECT RAISE(IGNORE); END;",
    )?;
    drop(injector);

    assert_eq!(
        store.commit(commit).await,
        Err(SessionStoreError::corrupt())
    );
    assert_eq!(
        store.load_branch(initial.selector).await?.head(),
        created.branch()
    );
    assert_eq!(
        store.recover_commit(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(
        store.load_checkpoint(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    drop(store);
    assert_eq!(
        SqliteSessionStore::open(&db.path).expect_err("unknown trigger"),
        SessionStoreError::corrupt()
    );

    let injector = Connection::open(&db.path)?;
    injector.execute_batch("DROP TRIGGER ignore_commit_receipt;")?;
    drop(injector);
    let reopened = SqliteSessionStore::open(&db.path)?;
    assert_eq!(
        reopened.load_branch(initial.selector).await?.head(),
        created.branch()
    );
    Ok(())
}

#[tokio::test]
async fn injected_delete_cut_rolls_back_all_authority_and_exact_retry_recovers()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let initial = seed(18_000, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store.create(initial.request.clone()).await?;
    store.release(created.lease().clone()).await?;
    let archived = store
        .archive(plakous_agent::internal::ArchiveSession::new(
            initial.selector.session_id(),
            created.session().revision(),
        ))
        .await?;
    let request = DeleteSession::new(initial.selector.session_id(), archived.session().revision());
    let injector = Connection::open(&db.path)?;
    injector.execute_batch(
        "CREATE TRIGGER fail_delete_tombstone
         BEFORE INSERT ON delete_tombstones
         BEGIN SELECT RAISE(ABORT, 'injected delete cut'); END;",
    )?;
    drop(injector);

    assert_eq!(
        store.delete(request).await,
        Err(SessionStoreError::BackendUnavailable)
    );
    let retained = store
        .list(
            plakous_agent::internal::SessionQuery::new(NonZeroUsize::new(1).ok_or("nonzero page")?)
                .include_archived(true),
        )
        .await?;
    assert_eq!(
        retained.items().first().map(|item| item.head()),
        Some(archived.session())
    );
    store.load_checkpoint(initial.checkpoint_id).await?;
    assert_eq!(
        store.recover_delete(request).await,
        Err(SessionStoreError::NotFound)
    );
    drop(store);
    assert_eq!(
        SqliteSessionStore::open(&db.path).expect_err("persistent trigger"),
        SessionStoreError::corrupt()
    );
    let injector = Connection::open(&db.path)?;
    injector.execute_batch("DROP TRIGGER fail_delete_tombstone;")?;
    drop(injector);
    let reopened = SqliteSessionStore::open(&db.path)?;
    let receipt = reopened.delete(request).await?;
    assert_eq!(reopened.recover_delete(request).await?, receipt);
    Ok(())
}

#[tokio::test]
async fn injected_commit_stage_cuts_roll_back_all_authority_and_exact_retry_recovers()
-> Result<(), Box<dyn Error>> {
    for (base, table) in [
        (20_000_u128, "runtime_objects"),
        (20_100_u128, "commit_receipts"),
    ] {
        assert_commit_cut(base, table).await?;
    }
    Ok(())
}

async fn assert_commit_cut(base: u128, table: &str) -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let initial = seed(base, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store.create(initial.request.clone()).await?;
    let committed_id = conformance::id::<CheckpointId>(base + 10)?;
    let next = successor(
        &initial,
        committed_id,
        initial.checkpoint_id,
        1,
        &initial.bundle,
    )?;
    let commit = CommitCheckpoint::new(created.lease().clone(), created.branch(), next);

    let injector = Connection::open(&db.path)?;
    injector.execute_batch(&format!(
        "CREATE TRIGGER fail_commit_stage
         BEFORE INSERT ON {table}
         BEGIN SELECT RAISE(ABORT, 'injected commit cut'); END;"
    ))?;
    drop(injector);
    assert_eq!(
        store.commit(commit.clone()).await,
        Err(SessionStoreError::BackendUnavailable)
    );
    assert_eq!(
        store.load_branch(initial.selector).await?.head(),
        created.branch()
    );
    assert_eq!(
        store.recover_commit(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(
        store.load_checkpoint(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(store.stats().await?.checkpoints(), 1);
    drop(store);

    let injector = Connection::open(&db.path)?;
    injector.execute_batch("DROP TRIGGER fail_commit_stage;")?;
    drop(injector);
    let reopened = SqliteSessionStore::open(&db.path)?;
    assert_eq!(
        reopened.load_branch(initial.selector).await?.head(),
        created.branch()
    );
    assert_eq!(
        reopened.recover_commit(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(
        reopened.load_checkpoint(committed_id).await,
        Err(SessionStoreError::NotFound)
    );
    let receipt = reopened.commit(commit).await?;
    assert_eq!(
        reopened.load_branch(initial.selector).await?.head(),
        receipt.head()
    );
    drop(reopened);
    let recovered = SqliteSessionStore::open(&db.path)?;
    assert_eq!(
        recovered.recover_commit(committed_id).await?.receipt(),
        receipt
    );
    assert_eq!(
        recovered.load_branch(initial.selector).await?.head(),
        receipt.head()
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear corpus proves sustained deltas and loaded-graph reopen advancement"
)]
async fn sustained_commits_publish_only_deltas_and_reopen_the_exact_head()
-> Result<(), Box<dyn Error>> {
    const TURNS: u64 = 100;
    const MAX_OBJECTS_PER_DELTA: usize = 12;
    const MAX_ACTIVE_DB_WAL_AMPLIFICATION: u64 = 32;
    const MAX_CHECKPOINTED_DB_AMPLIFICATION: u64 = 12;

    let db = Database::new();
    let initial = seed(30_000, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store.create(initial.request.clone()).await?;
    let baseline_storage = storage_bytes(&db.path);
    let mut head: BranchHead = created.branch();
    let mut parent = initial.checkpoint_id;
    let mut prior = initial.bundle.clone();
    let mut published = BTreeSet::new();
    let mut published_bytes = 0_usize;
    for object in prior.supplied_objects() {
        assert!(published.insert(object.address()));
        published_bytes = published_bytes.saturating_add(object.retained_bytes());
    }
    let genesis_objects = published.len();
    let genesis_bytes = published_bytes;

    for turn in 1..=TURNS {
        let checkpoint_id = conformance::id::<CheckpointId>(30_100 + u128::from(turn))?;
        let publication = successor(&initial, checkpoint_id, parent, turn, &prior)?;
        assert_eq!(publication.base_root(), Some(prior.root()));
        assert!(publication.supplied_objects().len() <= MAX_OBJECTS_PER_DELTA);
        assert_eq!(
            publication.stats().new_object_count(),
            publication.supplied_objects().len()
        );
        for object in publication.supplied_objects() {
            assert!(published.insert(object.address()));
            published_bytes = published_bytes.saturating_add(object.retained_bytes());
        }
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                publication.clone(),
            ))
            .await?
            .head();
        assert_linear_stats(store.stats().await?, turn, published.len(), published_bytes)?;
        prior = publication;
        parent = checkpoint_id;
    }

    assert_eq!(head.revision().get(), TURNS);
    assert!(
        published.len().saturating_sub(genesis_objects)
            <= usize::try_from(TURNS)? * MAX_OBJECTS_PER_DELTA
    );
    let new_semantic_bytes = u64::try_from(published_bytes - genesis_bytes)?;
    assert!(new_semantic_bytes > 0);
    store.flush().await?;
    let active_storage_growth = storage_bytes(&db.path).saturating_sub(baseline_storage);
    assert!(
        active_storage_growth <= new_semantic_bytes.saturating_mul(MAX_ACTIVE_DB_WAL_AMPLIFICATION),
        "active database plus WAL growth {active_storage_growth} exceeded {MAX_ACTIVE_DB_WAL_AMPLIFICATION}x newly committed semantic bytes {new_semantic_bytes}"
    );
    store.shutdown().await?;
    let checkpointed_growth = storage_bytes(&db.path).saturating_sub(baseline_storage);
    assert!(
        checkpointed_growth <= new_semantic_bytes.saturating_mul(MAX_CHECKPOINTED_DB_AMPLIFICATION),
        "checkpointed database growth {checkpointed_growth} exceeded {MAX_CHECKPOINTED_DB_AMPLIFICATION}x newly committed semantic bytes {new_semantic_bytes}"
    );
    drop(store);

    let reopened = SqliteSessionStore::open(&db.path)?;
    assert_eq!(reopened.stats().await?.semantic_mutations(), TURNS + 1);
    assert_eq!(reopened.load_branch(initial.selector).await?.head(), head);
    assert_eq!(
        reopened.recover_commit(parent).await?.receipt().head(),
        head
    );
    assert_eq!(
        reopened.load_checkpoint(parent).await?.reference(),
        head.checkpoint()
    );
    let loaded = reopened.load_checkpoint(parent).await?;
    let reopened_checkpoint_id = conformance::id::<CheckpointId>(30_300)?;
    let publication = successor(
        &initial,
        reopened_checkpoint_id,
        parent,
        TURNS + 1,
        loaded.bundle(),
    )?;
    assert_eq!(publication.base_root(), Some(loaded.reference().root()));
    assert!(publication.supplied_objects().len() <= MAX_OBJECTS_PER_DELTA);
    assert!(publication.stats().new_object_count() < publication.stats().object_count());
    let reopened_head = reopened
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            head,
            publication,
        ))
        .await?
        .head();
    drop(reopened);

    let reopened_again = SqliteSessionStore::open(&db.path)?;
    assert_eq!(
        reopened_again.load_branch(initial.selector).await?.head(),
        reopened_head
    );
    assert_eq!(
        reopened_again
            .load_checkpoint(reopened_checkpoint_id)
            .await?
            .reference(),
        reopened_head.checkpoint()
    );
    Ok(())
}

#[test]
fn semantic_schema_contains_no_event_transcript_or_snapshot_authority() {
    let db = Database::new();
    drop(SqliteSessionStore::open(&db.path).expect("open"));
    let connection = Connection::open(&db.path).expect("inspect schema");
    let count = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type='table' AND (
                 lower(name) LIKE '%event%' OR
                 lower(name) LIKE '%transcript%' OR
                 lower(name) LIKE '%snapshot%'
             )",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("observation table count");
    assert_eq!(count, 0);
}

/// Concurrent `plak` processes open one runtime store while another commits.
/// Admission must observe one snapshot: counters and `count(*)` rows read at
/// different instants would otherwise report a healthy store as `Corrupt`.
#[tokio::test]
async fn admission_under_concurrent_commits_never_reports_corruption() -> Result<(), Box<dyn Error>>
{
    let db = Database::new();
    let writer = Arc::new(SqliteSessionStore::open(&db.path)?);
    let path = db.path.clone();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opener_done = Arc::clone(&done);
    let opener = std::thread::spawn(move || -> Result<u32, SessionStoreError> {
        let mut opens = 0_u32;
        while !opener_done.load(Ordering::SeqCst) {
            drop(SqliteSessionStore::open(&path)?);
            opens += 1;
        }
        Ok(opens)
    });
    for index in 0..120_u128 {
        let seed = seed(9_000 + index * 8, RuntimeDurability::Required, 1)?;
        writer.create(seed.request).await?;
    }
    done.store(true, Ordering::SeqCst);
    let opens = opener
        .join()
        .map_err(|_| "opener thread panicked")?
        .map_err(|error| format!("concurrent open refused a healthy store: {error:?}"))?;
    assert!(opens > 0, "opener never observed the live writer");
    Ok(())
}

/// Inspection admits shape and schema once per snapshot and never recounts
/// retained rows, so a read stays O(what it reads) however large the store.
#[tokio::test]
async fn inspection_reads_without_recounting_retained_rows() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let initial = seed(16_500, RuntimeDurability::Required, 1)?;
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store.create(initial.request.clone()).await?;
    let middle = conformance::id::<CheckpointId>(16_510)?;
    let first = successor(&initial, middle, initial.checkpoint_id, 1, &initial.bundle)?;
    store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            first.clone(),
        ))
        .await?;
    let head = store.load_branch(initial.selector).await?.head();
    let newest = conformance::id::<CheckpointId>(16_511)?;
    store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            head,
            successor(&initial, newest, middle, 2, &first)?,
        ))
        .await?;
    store.release(created.lease().clone()).await?;
    store.shutdown().await?;
    drop(store);

    // A counter the writer would refuse does not stop a read.
    let connection = Connection::open(&db.path)?;
    connection.execute("UPDATE store_meta SET sessions=7 WHERE id=1", [])?;
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open(&db.path).err(),
        Some(SessionStoreError::corrupt()),
        "the writer still validates its counters"
    );
    assert_eq!(
        SqliteSessionStore::inspect_branch(&db.path, initial.selector)?
            .checkpoint()
            .checkpoint()
            .id(),
        newest
    );
    assert_eq!(
        SqliteSessionStore::inspect_checkpoint(&db.path, middle)?
            .checkpoint()
            .id(),
        middle
    );
    Ok(())
}

#[test]
fn counter_corruption_and_unknown_tables_fail_closed_on_reopen() {
    let db = Database::new();
    drop(SqliteSessionStore::open(&db.path).expect("open"));
    let connection = Connection::open(&db.path).expect("corrupt fixture");
    connection
        .execute("UPDATE store_meta SET sessions=1 WHERE id=1", [])
        .expect("corrupt counter");
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open(&db.path).expect_err("counter mismatch"),
        SessionStoreError::corrupt()
    );

    let db = Database::new();
    drop(SqliteSessionStore::open(&db.path).expect("open"));
    let connection = Connection::open(&db.path).expect("schema fixture");
    connection
        .execute("CREATE TABLE unexpected(value INTEGER)", [])
        .expect("extra table");
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open(&db.path).expect_err("unknown table"),
        SessionStoreError::corrupt()
    );
}

#[tokio::test]
async fn foreign_scope_open_preserves_database_and_wal() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let owner = Arc::new(conformance::DomainLeaseClock::new(
        LeaseDomain::new([1; 16], [2; 32]),
        100,
    )?);
    let store = SqliteSessionStore::open_with_clock(&db.path, owner)?;
    let initial = seed(41_000, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request).await?;
    let database_before = fs::read(&db.path)?;
    let wal_path = db.path.with_extension("sqlite3-wal");
    let wal_before = fs::read(&wal_path)?;
    let foreign = Arc::new(conformance::DomainLeaseClock::new(
        LeaseDomain::new([1; 16], [3; 32]),
        100,
    )?);
    assert!(matches!(
        SqliteSessionStore::open_with_clock(&db.path, foreign),
        Err(SessionStoreError::LeaseDomainMismatch)
    ));
    assert_eq!(fs::read(&db.path)?, database_before);
    assert_eq!(fs::read(&wal_path)?, wal_before);
    assert_eq!(
        store.renew(created.lease().clone()).await?,
        created.lease().clone()
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one reboot proof retains exact historical receipts and verifies both branch fences"
)]
async fn reboot_preserves_create_and_fork_receipts_without_reviving_old_authority()
-> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let old_domain = LeaseDomain::new([1; 16], [2; 32]);
    let new_domain = LeaseDomain::new([3; 16], [2; 32]);
    let old_clock = Arc::new(conformance::DomainLeaseClock::new(old_domain, 100)?);
    let old = SqliteSessionStore::open_with_clock(&db.path, old_clock.clone())?;
    let initial = seed(42_000, RuntimeDurability::Required, 1)?;
    let created = old.create(initial.request.clone()).await?;
    old_clock.set(Ok(LeaseInstant::new(old_domain, 101, 0)?));
    let renewed = old.renew(created.lease().clone()).await?;
    let target_branch = conformance::id::<BranchId>(42_010)?;
    let target_checkpoint_id = conformance::id::<CheckpointId>(42_011)?;
    let source_checkpoint = old.load_checkpoint(initial.checkpoint_id).await?;
    let context = source_checkpoint
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .ordinary_fork(
            initial.checkpoint_id,
            initial.selector.session_id(),
            target_branch,
            [0x91; 32],
        )?;
    let target_checkpoint = conformance::checkpoint_with_context(
        conformance::topology(
            initial.selector.session_id(),
            target_branch,
            target_checkpoint_id,
            Some(initial.checkpoint_id),
            0,
            Some(ForkLineage {
                session_id: initial.selector.session_id(),
                branch_id: initial.selector.branch_id(),
                checkpoint_id: initial.checkpoint_id,
                branch_revision: 0,
            }),
            1,
        )?,
        1,
        RuntimeDurability::Required,
        context,
        false,
    )?;
    let workflow = conformance::workflow(201)?;
    let semantic = conformance::semantic_request(b"reboot fork receipt")?;
    let request = ForkBranch::new(
        workflow,
        semantic,
        renewed.clone(),
        SessionRevision::INITIAL,
        created.branch(),
        target_branch,
        conformance::id::<RunId>(42_012)?,
        NonZeroU64::new(3_600).ok_or("duration")?,
        conformance::bundle(&target_checkpoint, Some(&initial.bundle))?,
    );
    let forked = old.fork(request.clone()).await?;
    let current = SqliteSessionStore::open_with_clock(
        &db.path,
        Arc::new(conformance::DomainLeaseClock::new(new_domain, 1)?),
    )?;
    assert_eq!(current.create(initial.request.clone()).await?, created);
    assert_eq!(current.fork(request).await?, forked);
    for token in [created.lease().clone(), renewed, forked.lease().clone()] {
        assert_eq!(
            current.renew(token.clone()).await,
            Err(SessionStoreError::FenceSuperseded)
        );
        assert_eq!(
            current.release(token).await,
            Err(SessionStoreError::FenceSuperseded)
        );
    }
    assert_eq!(
        old.renew(created.lease().clone()).await,
        Err(SessionStoreError::LeaseDomainMismatch)
    );
    assert_eq!(
        old.create(seed(43_000, RuntimeDurability::Required, 1)?.request)
            .await,
        Err(SessionStoreError::LeaseDomainMismatch)
    );
    for (reference, prior) in [
        (initial.selector, created.lease()),
        (
            SessionRef::new(initial.selector.session_id(), target_branch),
            forked.lease(),
        ),
    ] {
        let next = current
            .acquire(plakous_agent::internal::AcquireBranch::new(
                reference,
                conformance::id::<RunId>(42_020)?,
                NonZeroU64::new(10).ok_or("duration")?,
            ))
            .await?;
        assert!(next.fence().get() > prior.fence().get());
        assert_eq!(next.expires_at().domain(), new_domain);
    }
    assert_eq!(current.create(initial.request).await?, created);
    Ok(())
}

#[tokio::test]
async fn independent_process_clocks_share_sqlite_lease_authority() -> Result<(), Box<dyn Error>> {
    const CHILD_PATH: &str = "PLAK_STORE_CROSS_PROCESS_LEASE_PATH";
    const CHILD_PHASE: &str = "PLAK_STORE_CROSS_PROCESS_LEASE_PHASE";
    const PRIOR_FENCE: &str = "PLAK_STORE_CROSS_PROCESS_PRIOR_FENCE";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let store = SqliteSessionStore::open(path)?;
        let initial = seed(44_000, RuntimeDurability::Required, 1)?;
        let request = plakous_agent::internal::AcquireBranch::new(
            initial.selector,
            conformance::id::<RunId>(44_010)?,
            NonZeroU64::new(3_600).ok_or("duration")?,
        );
        match std::env::var(CHILD_PHASE)?.as_str() {
            "held" => assert_eq!(
                store.acquire(request).await,
                Err(SessionStoreError::LeaseHeld)
            ),
            "released" => {
                let lease = store.acquire(request).await?;
                let prior: u64 = std::env::var(PRIOR_FENCE)?.parse()?;
                assert!(lease.fence().get() > prior);
                assert_eq!(lease.expires_at().domain(), store.lease_now()?.domain());
                store.release(lease).await?;
            }
            _ => return Err("unknown cross-process lease phase".into()),
        }
        store.shutdown().await?;
        return Ok(());
    }
    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path)?;
    let initial = seed(44_000, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request).await?;
    for phase in ["held", "released"] {
        if phase == "released" {
            store.release(created.lease().clone()).await?;
        }
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "sqlite::independent_process_clocks_share_sqlite_lease_authority",
                "--nocapture",
            ])
            .env(CHILD_PATH, &db.path)
            .env(CHILD_PHASE, phase)
            .env(PRIOR_FENCE, created.lease().fence().get().to_string())
            .spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait()? {
                assert!(
                    status.success(),
                    "lease subprocess {phase} failed: {status}"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err("cross-process lease proof exceeded its join bound".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// A branch whose holder died is reclaimed at once; a live one still waits.
///
/// A lease exists to stop two live processes sharing a branch. When the holder
/// is gone the clock protects nothing, and making the next process wait it out
/// is what made a crashed session read as broken for half a minute. Proof is
/// the kernel releasing the holder's lock, which a recycled process identifier
/// cannot confuse. Anything short of proof keeps the clock exactly as it was.
#[tokio::test]
async fn a_dead_holder_releases_its_branch_at_once_and_a_live_one_does_not()
-> Result<(), Box<dyn Error>> {
    const CHILD_PATH: &str = "PLAK_STORE_DEAD_HOLDER_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let store = SqliteSessionStore::open(path)?;
        let initial = seed(46_000, RuntimeDurability::Required, 1)?;
        let _lease = store
            .acquire(plakous_agent::internal::AcquireBranch::new(
                initial.selector,
                conformance::id::<RunId>(46_010)?,
                // Long enough that nothing here can pass by expiry.
                NonZeroU64::new(3_600).ok_or("duration")?,
            ))
            .await?;
        println!("held");
        std::thread::sleep(Duration::from_secs(600));
        return Ok(());
    }

    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path)?;
    let initial = seed(46_000, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request).await?;
    store.release(created.lease().clone()).await?;
    let request = plakous_agent::internal::AcquireBranch::new(
        initial.selector,
        conformance::id::<RunId>(46_020)?,
        NonZeroU64::new(3_600).ok_or("duration")?,
    );

    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "sqlite::a_dead_holder_releases_its_branch_at_once_and_a_live_one_does_not",
            "--nocapture",
        ])
        .env(CHILD_PATH, &db.path)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut held = String::new();
    {
        use std::io::Read as _;
        let mut out = child.stdout.take().ok_or("child stdout")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut byte = [0_u8; 1];
        while !held.contains("held") {
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err("holder subprocess never took the branch".into());
            }
            if out.read(&mut byte)? == 0 {
                break;
            }
            held.push(char::from(byte[0]));
        }
    }
    assert!(held.contains("held"), "holder subprocess did not report");

    // A live holder is refused, exactly as before.
    assert_eq!(
        store.acquire(request).await,
        Err(SessionStoreError::LeaseHeld),
        "a live holder still owns its branch"
    );

    // A dead one is not, and the wait is none: the lease still has an hour on
    // its clock, so anything but proof of death would refuse here.
    child.kill()?;
    child.wait()?;
    let reclaimed = store.acquire(request).await?;
    assert_eq!(reclaimed.owner(), conformance::id::<RunId>(46_020)?);
    assert!(reclaimed.fence().get() > created.lease().fence().get());
    store.release(reclaimed).await?;
    store.shutdown().await?;
    Ok(())
}

/// A superseded holder's exit is no proof that the current holder is gone.
///
/// A holder suspended past its lease still has its lock when another process
/// acquires the expired lease, so the new holder runs without one. When the old
/// holder later exits its lock comes free, but the record still names its own
/// grant, not the live one, so a third process must keep waiting rather than
/// take a lease that is in use.
#[tokio::test]
async fn a_superseded_holders_exit_does_not_hand_the_live_lease_to_a_third_process()
-> Result<(), Box<dyn Error>> {
    const CHILD_PATH: &str = "PLAK_STORE_SUPERSEDED_HOLDER_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let store = SqliteSessionStore::open(path)?;
        let initial = seed(46_500, RuntimeDurability::Required, 1)?;
        let _lease = store
            .acquire(plakous_agent::internal::AcquireBranch::new(
                initial.selector,
                conformance::id::<RunId>(46_510)?,
                NonZeroU64::MIN,
            ))
            .await?;
        println!("held");
        std::thread::sleep(Duration::from_secs(600));
        return Ok(());
    }

    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path)?;
    let initial = seed(46_500, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request).await?;
    store.release(created.lease().clone()).await?;
    let acquire = |owner: u128| -> Result<_, Box<dyn Error>> {
        Ok(plakous_agent::internal::AcquireBranch::new(
            initial.selector,
            conformance::id::<RunId>(owner)?,
            NonZeroU64::new(3_600).ok_or("duration")?,
        ))
    };

    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "sqlite::a_superseded_holders_exit_does_not_hand_the_live_lease_to_a_third_process",
            "--nocapture",
        ])
        .env(CHILD_PATH, &db.path)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut held = String::new();
    {
        use std::io::Read as _;
        let mut out = child.stdout.take().ok_or("child stdout")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut byte = [0_u8; 1];
        while !held.contains("held") {
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err("holder subprocess never took the branch".into());
            }
            if out.read(&mut byte)? == 0 {
                break;
            }
            held.push(char::from(byte[0]));
        }
    }
    assert!(held.contains("held"), "holder subprocess did not report");

    // The suspended holder keeps its lock; its one-second lease runs out.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let live = loop {
        match store.acquire(acquire(46_520)?).await {
            Ok(lease) => break lease,
            Err(SessionStoreError::LeaseHeld) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                child.kill()?;
                child.wait()?;
                return Err(error.into());
            }
        }
    };

    child.kill()?;
    child.wait()?;
    let third = SqliteSessionStore::open(&db.path)?;
    assert_eq!(
        third.acquire(acquire(46_530)?).await,
        Err(SessionStoreError::LeaseHeld),
        "the old holder's exit proves nothing about the live lease"
    );
    let renewed = store.renew(live).await?;
    store.release(renewed).await?;
    third.shutdown().await?;
    store.shutdown().await?;
    Ok(())
}

/// A holder's own children must not keep its branch after it dies.
///
/// A `flock` belongs to the open file description, not the process, so any
/// descriptor that outlives the holder keeps the lock. A holder spawns children
/// — process sessions, sidecars, shells — and if one of them inherited the lock
/// descriptor it would hold the branch until it exited, which under load is
/// long after the holder is gone. That is the difference between a session that
/// reopens at once and one that appears stuck.
#[tokio::test]
async fn a_holders_surviving_child_does_not_keep_its_branch() -> Result<(), Box<dyn Error>> {
    const CHILD_PATH: &str = "PLAK_STORE_HOLDER_CHILD_PATH";
    const GRANDCHILD: &str = "PLAK_STORE_HOLDER_GRANDCHILD";
    if std::env::var_os(GRANDCHILD).is_some() {
        // Outlives the holder, holding whatever it inherited. Bounded, because
        // an orphan that outlives the run would linger on the gate machine.
        std::thread::sleep(Duration::from_secs(30));
        return Ok(());
    }
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let store = SqliteSessionStore::open(path)?;
        let initial = seed(47_000, RuntimeDurability::Required, 1)?;
        let _lease = store
            .acquire(plakous_agent::internal::AcquireBranch::new(
                initial.selector,
                conformance::id::<RunId>(47_010)?,
                NonZeroU64::new(3_600).ok_or("duration")?,
            ))
            .await?;
        // Spawned while the branch is held, exactly as a session spawns a
        // process tool or a sidecar.
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "sqlite::a_holders_surviving_child_does_not_keep_its_branch",
                "--nocapture",
            ])
            .env(GRANDCHILD, "1")
            .env_remove(CHILD_PATH)
            // Its own stdio: an orphan holding the harness's pipes is what
            // makes a test runner see a broken pipe while listing.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        println!("held");
        std::thread::sleep(Duration::from_secs(600));
        return Ok(());
    }

    let db = Database::new();
    let store = SqliteSessionStore::open(&db.path)?;
    let initial = seed(47_000, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request).await?;
    store.release(created.lease().clone()).await?;
    let request = plakous_agent::internal::AcquireBranch::new(
        initial.selector,
        conformance::id::<RunId>(47_020)?,
        NonZeroU64::new(3_600).ok_or("duration")?,
    );

    let mut holder = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "sqlite::a_holders_surviving_child_does_not_keep_its_branch",
            "--nocapture",
        ])
        .env(CHILD_PATH, &db.path)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    {
        use std::io::Read as _;
        let mut out = holder.stdout.take().ok_or("holder stdout")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut seen = String::new();
        let mut byte = [0_u8; 1];
        while !seen.contains("held") {
            if std::time::Instant::now() >= deadline {
                holder.kill()?;
                holder.wait()?;
                return Err("holder never took the branch".into());
            }
            if out.read(&mut byte)? == 0 {
                break;
            }
            seen.push(char::from(byte[0]));
        }
        assert!(seen.contains("held"), "holder did not report");
    }

    // The holder dies; its child does not. The branch is the holder's, so it
    // goes with the holder.
    holder.kill()?;
    holder.wait()?;
    let reclaimed = store.acquire(request).await?;
    assert_eq!(reclaimed.owner(), conformance::id::<RunId>(47_020)?);
    store.release(reclaimed).await?;
    store.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_prune_conformance_reopens_exact_and_verified() -> Result<(), Box<dyn Error>> {
    let db = Database::new();
    let store =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    conformance::run_prune_conformance(&store, RuntimeDurability::Required, 40_000).await?;
    conformance::run_reclaimed_address_conformance(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
        45_000,
    )
    .await?;
    let stats = store.stats().await?;
    let usage = store.usage().await?;
    store.shutdown().await?;
    drop(store);

    SqliteSessionStore::open_verified(&db.path)?;
    let reopened =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    assert_eq!(reopened.stats().await?, stats);
    assert_eq!(reopened.usage().await?, usage);
    let pruned = conformance::id::<CheckpointId>(40_100 + 3)?;
    assert_eq!(
        reopened.load_checkpoint(pruned).await.map(|_| ()),
        Err(SessionStoreError::Pruned)
    );
    assert_eq!(
        reopened.recover_commit(pruned).await.map(|_| ()),
        Err(SessionStoreError::Pruned)
    );
    reopened
        .load_checkpoint(conformance::id::<CheckpointId>(40_100 + 13)?)
        .await?;
    reopened.shutdown().await?;
    drop(reopened);

    // A surviving checkpoint's parent that is neither present nor a recorded
    // pruned id is dangling: full verification refuses it.
    let connection = Connection::open(&db.path)?;
    let orphaned = connection.execute(
        "DELETE FROM pruned_checkpoints WHERE checkpoint_id IN
         (SELECT parent_id FROM checkpoints)",
        [],
    )?;
    assert!(orphaned > 0);
    drop(connection);
    assert_eq!(
        SqliteSessionStore::open_verified(&db.path).map(|_| ()),
        Err(SessionStoreError::corrupt())
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered large-operation persistence, prune and cold reconstruction proof"
)]
async fn sqlite_retains_large_operation_payloads_after_prune_and_cold_reopen()
-> Result<(), Box<dyn Error>> {
    use plakous_agent::internal::{
        DurableOperationKind, DurableOperationRecord, DurableOperationState,
        DurableOperationTransition, ReconstructiveComponent, RuntimeCheckpoint,
        RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeManifest, RuntimeOperationLedger,
        RuntimeOperationRoot,
    };

    const PAYLOAD_BYTES: usize = 16 * 1024 * 1024 + 1;
    let request_bytes = vec![0x6b; PAYLOAD_BYTES];
    let terminal_bytes = vec![0x5a; PAYLOAD_BYTES];
    let db = Database::new();
    // Nothing carrying a validated checkpoint or warm object bundle escapes
    // this scope: the reopened reader must reconstruct from SQLite alone.
    let (selector, head, pruned_id, expected_ledger) = {
        let initial = seed(48_000, RuntimeDurability::Required, 1)?;
        let store = SqliteSessionStore::open(&db.path)?;
        let created = store.create(initial.request.clone()).await?;
        let evidence = ReconstructiveComponent::new(
            RuntimeComponentKind::Operation,
            1,
            terminal_bytes.clone(),
        )?;
        let ledger = RuntimeOperationLedger::new(
            Vec::new(),
            vec![DurableOperationRecord::new(
                1,
                DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::Turn(conformance::id(48_010)?),
                    attempt_id: conformance::id(48_011)?,
                },
                ReconstructiveComponent::new(
                    RuntimeComponentKind::Operation,
                    1,
                    request_bytes.clone(),
                )?,
                vec![
                    DurableOperationTransition::new(
                        initial.checkpoint_id,
                        DurableOperationState::Prepared,
                    ),
                    DurableOperationTransition::new(
                        conformance::id(48_020)?,
                        DurableOperationState::Dispatched,
                    ),
                    DurableOperationTransition::new(
                        conformance::id(48_021)?,
                        DurableOperationState::Completed(evidence.clone()),
                    ),
                    DurableOperationTransition::new(
                        conformance::id(48_022)?,
                        DurableOperationState::Finalized(evidence),
                    ),
                ],
            )?],
        )?;
        let mut head = created.branch();
        let mut prior = initial.bundle.clone();
        for revision in 1..=3_u64 {
            let fixture = conformance::checkpoint(
                conformance::topology(
                    initial.selector.session_id(),
                    initial.selector.branch_id(),
                    conformance::id(48_100 + u128::from(revision))?,
                    Some(head.checkpoint().checkpoint_id()),
                    revision,
                    None,
                    created.lease().fence().get(),
                )?,
                u32::try_from(revision + 1)?,
                RuntimeDurability::Required,
            )?;
            let manifest = fixture.manifest();
            let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
                manifest.provenance().clone(),
                manifest.topology(),
                manifest.lifecycle().clone(),
                manifest.harness_plan_component().clone(),
                manifest.provider_configuration().clone(),
                manifest.canonical().clone(),
                manifest.binding().clone(),
                manifest.tools().clone(),
                ledger.clone(),
                None,
                None,
                plakous_agent::internal::JobLedger::default(),
                RuntimeDurability::Required,
            )?)?;
            let next = RuntimeCheckpointCodec::advance(&checkpoint, &prior)?;
            head = store
                .commit(CommitCheckpoint::new(
                    created.lease().clone(),
                    head,
                    next.clone(),
                ))
                .await?
                .head();
            prior = next.trimmed()?;
        }
        let receipt = store
            .prune(PruneBranch::new(
                created.lease().clone(),
                head,
                BTreeSet::new(),
                1,
            ))
            .await?;
        assert_eq!(receipt.pruned_checkpoints(), 1);
        let pruned_id = conformance::id::<CheckpointId>(48_101)?;
        assert_eq!(
            store.load_checkpoint(pruned_id).await.map(|_| ()),
            Err(SessionStoreError::Pruned)
        );
        store.release(created.lease().clone()).await?;
        store.shutdown().await?;
        (initial.selector, head, pruned_id, ledger)
    };

    let reopened = SqliteSessionStore::open_verified(&db.path)?;
    assert_eq!(reopened.load_branch(selector).await?.head(), head);
    assert_eq!(
        reopened.load_checkpoint(pruned_id).await.map(|_| ()),
        Err(SessionStoreError::Pruned)
    );
    let loaded = reopened
        .load_checkpoint(head.checkpoint().checkpoint_id())
        .await?;
    assert_eq!(loaded.reference(), head.checkpoint());
    let checkpoint = RuntimeCheckpointCodec::materialize(loaded.bundle())?;
    assert_eq!(checkpoint, *loaded.checkpoint().as_ref());
    // Ledger equality includes the private request component's exact bytes,
    // not merely its publicly exposed request digest.
    assert_eq!(checkpoint.manifest().ledger(), &expected_ledger);
    let operations = checkpoint.manifest().ledger().operations();
    assert_eq!(operations.len(), 1);
    let DurableOperationState::Finalized(evidence) = operations[0].state() else {
        return Err("retained operation lost finalized terminal evidence".into());
    };
    assert_eq!(evidence.bytes(), terminal_bytes);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered reclaim, reconstruction and verified-reopen regression"
)]
async fn sqlite_reconstructs_reclaimed_addresses_across_prune_cycles_and_reopen()
-> Result<(), Box<dyn Error>> {
    use plakous_agent::internal::{
        AcquireBranch, RuntimeCheckpointCodec, RuntimeContextState, RuntimeObjectKind,
    };

    let db = Database::new();
    let clock = Arc::new(ObservedLeaseClock::new(100));
    let mut store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let chain = conformance::prune_chain(&store, RuntimeDurability::Required, 45_000, 0, 0).await?;
    let mut head = chain.head;
    let mut lease = chain.created.lease().clone();
    let mut cache = chain.prior;
    let session = head.session();
    let context =
        RuntimeContextState::genesis(session.session_id(), session.branch_id(), [0x51; 32])?;
    let mut old_address = None;
    let mut revision = 0_u64;
    for cycle in 0..3 {
        // Two replacement heads are needed: retention keeps the head's parent.
        for payload in [" old", " replacement", " replacement"] {
            revision += 1;
            let next = conformance::checkpoint_with_payload(
                conformance::topology(
                    session.session_id(),
                    session.branch_id(),
                    conformance::id(45_100 + u128::from(revision))?,
                    Some(head.checkpoint().checkpoint_id()),
                    revision,
                    None,
                    lease.fence().get(),
                )?,
                32,
                RuntimeDurability::Required,
                context.clone(),
                false,
                payload,
            )?;
            let publication = RuntimeCheckpointCodec::advance(&next, &cache)?;
            if payload == " old" {
                if let Some(address) = old_address {
                    assert!(
                        publication
                            .supplied_objects()
                            .iter()
                            .any(|object| object.address() == address),
                        "reclaimed address must be supplied again, cycle {cycle}"
                    );
                } else {
                    old_address = Some(
                        publication
                            .supplied_objects()
                            .iter()
                            .find(|object| {
                                object.address().kind() == RuntimeObjectKind::HistorySegment
                            })
                            .ok_or("old history object")?
                            .address(),
                    );
                }
            }
            head = store
                .commit(CommitCheckpoint::new(
                    lease.clone(),
                    head,
                    publication.clone(),
                ))
                .await?
                .head();
            cache = publication.trimmed()?;
        }
        let checkpoint = cache.validated_checkpoint()?;
        // Model the owner's unconditional pre-sweep closure reset; its actual
        // below-threshold and lost-ack path is covered in the agent owner test.
        cache = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
        let receipt = store
            .prune(PruneBranch::new(lease.clone(), head, BTreeSet::new(), 1))
            .await?;
        assert!(receipt.pruned_checkpoints() > 0);
        store
            .reclaim(plakous_agent::internal::StoreReclaimMode::Drain)
            .await?;
        let address = old_address.ok_or("old address")?;
        let connection = Connection::open(&db.path)?;
        let remaining: i64 = connection.query_row(
            "SELECT count(*) FROM runtime_objects WHERE kind=?1 AND codec=?2 AND digest=?3",
            rusqlite::params![
                address.kind() as u8,
                address.codec_revision(),
                address.digest().bytes().as_slice()
            ],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 0, "the exact old object was swept");
        drop(connection);
        store.release(lease.clone()).await?;
        store.shutdown().await?;
        drop(store);
        SqliteSessionStore::open_verified(&db.path)?;
        store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
        assert_eq!(store.load_branch(session).await?.head(), head);
        assert_eq!(
            store
                .load_checkpoint(head.checkpoint().checkpoint_id())
                .await?
                .checkpoint()
                .as_ref(),
            checkpoint.as_ref()
        );
        lease = store
            .acquire(AcquireBranch::new(
                session,
                lease.owner(),
                lease.lease_seconds(),
            ))
            .await?;
        // Keep the in-memory cache warm across reopen, rather than replacing it
        // with the loaded bundle before reconstructing the same old address.
    }
    store.release(lease).await?;
    store.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_prune_over_the_absolute_limit_frees_ordinary_headroom() -> Result<(), Box<dyn Error>>
{
    const COMMITS: u64 = 20;
    let db = Database::new();
    let store =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    let mut chain =
        conformance::prune_chain(&store, RuntimeDurability::Required, 41_000, COMMITS, 64).await?;
    store.shutdown().await?;
    drop(store);

    let store = SqliteSessionStore::open_with_clock_and_options(
        &db.path,
        Arc::new(ObservedLeaseClock::new(100)),
        checkpoint_options(8)?,
    )?;
    let usage = store.usage().await?.bounds()[0];
    assert_eq!((usage.used(), usage.limit()), (COMMITS + 1, 8));
    let receipt = store
        .prune(PruneBranch::new(
            chain.created.lease().clone(),
            chain.head,
            BTreeSet::new(),
            1,
        ))
        .await?;
    assert_eq!(receipt.pruned_checkpoints(), COMMITS - 2);
    let usage = store.usage().await?.bounds()[0];
    assert_eq!(usage.used(), 3);
    assert!(usage.used() < StoreAdmission::Ordinary.threshold(8));
    chain.ids.push(conformance::id::<CheckpointId>(
        41_100 + u128::from(COMMITS + 1),
    )?);
    conformance::extend_chain(&store, RuntimeDurability::Required, chain, COMMITS + 1, 64).await?;
    store.shutdown().await?;
    drop(store);
    SqliteSessionStore::open_verified(&db.path)?;
    Ok(())
}

#[tokio::test]
async fn sqlite_prune_returns_freed_pages_to_the_file_system() -> Result<(), Box<dyn Error>> {
    const COMMITS: u64 = 24;
    let db = Database::new();
    let store =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    let chain = conformance::prune_chain(
        &store,
        RuntimeDurability::Required,
        42_000,
        COMMITS,
        96 * 1024,
    )
    .await?;
    store.shutdown().await?;
    drop(store);
    let grown = fs::metadata(&db.path)?.len();
    let before = SqliteSessionStore::inspect_file_usage(&db.path)?.ok_or("current store")?;
    assert_eq!(before.file_bytes, grown);

    let store =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    let receipt = store
        .prune(PruneBranch::new(
            chain.created.lease().clone(),
            chain.head,
            BTreeSet::new(),
            1,
        ))
        .await?;
    assert_eq!(receipt.pruned_checkpoints(), COMMITS - 2);
    let inspection = Connection::open(&db.path)?;
    let page_size: u64 =
        u64::try_from(
            inspection.pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))?,
        )?;
    drop(inspection);
    let free_bytes = SqliteSessionStore::inspect_file_usage(&db.path)?
        .ok_or("current store")?
        .free_bytes;
    // Each vacuum batch costs at most two background steps; every 32
    // foreground jobs guarantees a step even if the actor never becomes idle.
    let background_steps = free_bytes.div_ceil(page_size * 1024) * 2 + 2;
    for _ in 0..background_steps {
        for _ in 0..32 {
            store.stats().await?;
        }
        if SqliteSessionStore::inspect_file_usage(&db.path)?
            .ok_or("current store")?
            .free_bytes
            == 0
        {
            break;
        }
    }
    assert_eq!(
        SqliteSessionStore::inspect_file_usage(&db.path)?
            .ok_or("current store")?
            .free_bytes,
        0
    );
    store.shutdown().await?;
    drop(store);
    let shrunk = fs::metadata(&db.path)?.len();
    assert!(
        shrunk.saturating_mul(2) < grown,
        "file went from {grown} to {shrunk} bytes"
    );
    let after = SqliteSessionStore::inspect_file_usage(&db.path)?.ok_or("current store")?;
    assert_eq!(after.file_bytes, shrunk);
    SqliteSessionStore::open_verified(&db.path)?;
    Ok(())
}

/// Sweep cost on a large synthetic store: many sessions share nothing, one
/// long branch is pruned. Run with `--run-ignored only` to print timings.
#[tokio::test]
#[ignore = "measurement: builds a large store"]
async fn sqlite_prune_sweep_cost_on_a_large_store() -> Result<(), Box<dyn Error>> {
    const SESSIONS: u128 = 200;
    const COMMITS_PER_SESSION: u64 = 64;
    let db = Database::new();
    let store =
        SqliteSessionStore::open_with_clock(&db.path, Arc::new(ObservedLeaseClock::new(100)))?;
    let started = std::time::Instant::now();
    let mut chains = Vec::new();
    for session in 0..SESSIONS {
        chains.push(
            conformance::prune_chain(
                &store,
                RuntimeDurability::Required,
                100_000 + session * 1_000,
                COMMITS_PER_SESSION,
                2 * 1024,
            )
            .await?,
        );
    }
    let stats = store.stats().await?;
    eprintln!(
        "built {} checkpoints, {} objects, {} object bytes in {:?}",
        stats.checkpoints(),
        stats.objects(),
        stats.object_bytes(),
        started.elapsed()
    );
    let mut timings = Vec::new();
    for chain in &chains {
        let request = PruneBranch::new(
            chain.created.lease().clone(),
            chain.head,
            BTreeSet::new(),
            1,
        );
        let started = std::time::Instant::now();
        let receipt = store.prune(request.clone()).await?;
        timings.push(started.elapsed());
        assert_eq!(receipt.pruned_checkpoints(), COMMITS_PER_SESSION - 2);
        let started = std::time::Instant::now();
        assert_eq!(store.prune(request).await?.pruned_checkpoints(), 0);
        timings.push(started.elapsed());
    }
    let (prunes, noops): (Vec<_>, Vec<_>) =
        timings.chunks(2).map(|pair| (pair[0], pair[1])).unzip();
    let max = |values: &[Duration]| values.iter().max().copied().unwrap_or_default();
    let mean = |values: &[Duration]| {
        values.iter().sum::<Duration>() / u32::try_from(values.len()).unwrap_or(1)
    };
    eprintln!(
        "prune: first {:?}, mean {:?}, max {:?}; no-op mean {:?}, max {:?}",
        prunes[0],
        mean(&prunes),
        max(&prunes),
        mean(&noops),
        max(&noops)
    );
    let stats = store.stats().await?;
    eprintln!(
        "after: {} checkpoints, {} objects, {} object bytes",
        stats.checkpoints(),
        stats.objects(),
        stats.object_bytes()
    );
    store.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn batched_exit_release_fences_late_actor_commits() -> Result<(), Box<dyn Error>> {
    let database = Database::new();
    let store = SqliteSessionStore::open(&database.path)?;
    let first = seed(90_001, RuntimeDurability::Required, 1)?;
    let second = seed(90_101, RuntimeDurability::Required, 1)?;
    let a = store.create(first.request.clone()).await?;
    let b = store.create(second.request.clone()).await?;
    let later = successor(
        &first,
        conformance::id(90_010)?,
        first.checkpoint_id,
        1,
        &first.bundle,
    )?;
    let old_commit = CommitCheckpoint::new(a.lease().clone(), a.branch(), later);
    let connection = Connection::open(&database.path)?;
    let before: i64 =
        connection.query_row("SELECT semantic_mutations FROM store_meta", [], |row| {
            row.get(0)
        })?;
    store
        .release_many(vec![a.lease().clone(), b.lease().clone()])
        .await?;
    let after: i64 =
        connection.query_row("SELECT semantic_mutations FROM store_meta", [], |row| {
            row.get(0)
        })?;
    assert_eq!(
        after,
        before + 1,
        "all leases release in one store mutation"
    );
    assert!(matches!(
        store.commit(old_commit.clone()).await,
        Err(SessionStoreError::FenceSuperseded)
    ));
    let next = SqliteSessionStore::open(&database.path)?;
    let lease = next
        .acquire(plakous_agent::internal::AcquireBranch::new(
            first.selector,
            conformance::id(90_999)?,
            NonZeroU64::new(3600).ok_or("lease duration")?,
        ))
        .await?;
    assert!(lease.fence() > a.lease().fence());
    assert!(matches!(
        store.commit(old_commit).await,
        Err(SessionStoreError::FenceSuperseded)
    ));
    next.release(lease).await?;
    Ok(())
}
