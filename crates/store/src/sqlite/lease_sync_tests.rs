//! Lease renewal and revival commit unsynced; every other job runs at `FULL`.

use super::*;
use crate::integrity_fixture as fixture;
use std::sync::atomic::AtomicU64;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
type ProbeLog = Arc<Mutex<Vec<(&'static str, i64)>>>;

const NORMAL: i64 = 1;
const FULL: i64 = 2;

#[tokio::test]
async fn actor_direct_io_capacity_refusals_preserve_the_head_and_exact_retry() -> TestResult {
    for errno in [rustix::io::Errno::NOSPC, rustix::io::Errno::DQUOT] {
        let directory = tempfile::tempdir()?;
        let clock = Arc::new(StepClock(AtomicU64::new(100)));
        let store =
            SqliteSessionStore::open_with_clock(directory.path().join("runtime.sqlite3"), clock)?;
        let request = seed()?;
        let genesis = request.initial().clone();
        let created = store.create(request).await?;
        let mutation = successor(created.lease(), created.branch(), &genesis)?;
        store
            .dispatch("arm_capacity_fault", move |backend| {
                backend.capacity_fault = Some((
                    "commit",
                    std::io::Error::from_raw_os_error(errno.raw_os_error()),
                ));
                Ok(())
            })
            .await?;
        assert_eq!(
            store.commit(mutation.clone()).await,
            Err(SessionStoreError::StorageFull)
        );
        assert!(store.storage_unavailable());
        assert_eq!(
            store.load_branch(created.branch().session()).await?.head(),
            created.branch()
        );
        assert!(
            store.storage_unavailable(),
            "a read is not recovery authority"
        );
        let receipt = store.commit(mutation).await?;
        assert_eq!(receipt.previous(), created.branch());
        assert!(!store.storage_unavailable());
    }
    Ok(())
}

struct StepClock(AtomicU64);

#[tokio::test]
async fn exact_commit_reconciliation_clears_capacity_debt_without_waiting_for_heartbeat()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open_with_clock(
        directory.path().join("runtime.sqlite3"),
        Arc::new(StepClock(AtomicU64::new(100))),
    )?;
    let request = seed()?;
    let genesis = request.initial().clone();
    let created = store.create(request).await?;
    let mutation = successor(created.lease(), created.branch(), &genesis)?;
    let committed = store.commit(mutation.clone()).await?;
    assert_eq!(
        store
            .dispatch("refuse_capacity", |_| Err::<(), _>(
                SessionStoreError::StorageFull
            ))
            .await,
        Err(SessionStoreError::StorageFull)
    );
    assert!(store.storage_unavailable());
    let recovered = store
        .recover_commit(mutation.checkpoint().checkpoint_id())
        .await?;
    assert_eq!(recovered.receipt().head(), committed.head());
    assert!(!store.storage_unavailable());
    Ok(())
}

impl StepClock {
    fn set(&self, seconds: u64) {
        self.0.store(seconds, Ordering::SeqCst);
    }
}

impl LeaseClock for StepClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(
            LeaseDomain::new([1; 16], [2; 32]),
            self.0.load(Ordering::SeqCst),
            0,
        )
    }
}

fn seed() -> TestResult<CreateSession> {
    let session = fixture::id::<SessionId>(93_000)?;
    let branch = fixture::id::<plakous_agent::internal::BranchId>(93_001)?;
    let checkpoint = fixture::checkpoint(
        fixture::topology(session, branch, fixture::id(93_002)?, None, 0, None, 1)?,
        1,
        RuntimeDurability::Required,
    )?;
    Ok(CreateSession::new(
        SessionRef::new(session, branch),
        Timestamp::new(50, 0)?,
        fixture::id(93_003)?,
        NonZeroU64::new(30).ok_or("lease ttl")?,
        fixture::bundle(&checkpoint, None)?,
    ))
}

fn successor(
    lease: &BranchLease,
    head: BranchHead,
    prior: &CheckpointObjectBundle,
) -> TestResult<CommitCheckpoint> {
    let selector = head.session();
    let revision = head.revision().get() + 1;
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            selector.session_id(),
            selector.branch_id(),
            fixture::id(93_010 + u128::from(revision))?,
            Some(prior.checkpoint_id()),
            revision,
            None,
            lease.fence().get(),
        )?,
        u32::try_from(revision + 1)?,
        RuntimeDurability::Required,
    )?;
    Ok(CommitCheckpoint::new(
        lease.clone(),
        head,
        fixture::bundle(&checkpoint, Some(prior))?,
    ))
}

async fn install_probe(store: &SqliteSessionStore) -> TestResult<ProbeLog> {
    let log = ProbeLog::default();
    let sink = Arc::clone(&log);
    store
        .dispatch("install_synchronous_probe", move |b| {
            b.synchronous_probe = Some(Box::new(move |label, level| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((label, level));
            }));
            Ok(())
        })
        .await?;
    take(&log);
    Ok(log)
}

fn take(log: &ProbeLog) -> Vec<(&'static str, i64)> {
    std::mem::take(
        &mut *log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Renewal and revival, including a revival that is only a renewal, run their
/// transaction under `NORMAL` and leave the connection at `FULL`. Every other
/// job and background step finds and leaves it at `FULL`.
#[tokio::test]
async fn renewal_and_revival_commit_unsynced_and_restore_full() -> TestResult {
    let directory = tempfile::tempdir()?;
    let clock = Arc::new(StepClock(AtomicU64::new(100)));
    let store = SqliteSessionStore::open_with_clock(
        directory.path().join("runtime.sqlite3"),
        clock.clone(),
    )?;
    let log = install_probe(&store).await?;
    let request = seed()?;
    let genesis = request.initial().clone();
    let selector = request.session();
    let created = store.create(request).await?;
    assert_eq!(take(&log), [("create", FULL)]);

    clock.set(101);
    let renewed = store.renew(created.lease().clone()).await?;
    assert!(renewed.expires_at().seconds() > created.lease().expires_at().seconds());
    assert_eq!(take(&log), [("unsynced", NORMAL), ("renew", FULL)]);

    let committed = store
        .commit(successor(&renewed, created.branch(), &genesis)?)
        .await?;
    assert_eq!(take(&log), [("commit", FULL)]);

    clock.set(renewed.expires_at().seconds() + 600);
    assert_eq!(
        store.renew(renewed.clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    assert_eq!(take(&log), [("unsynced", NORMAL), ("renew", FULL)]);
    let revived = store.revive(renewed.clone()).await?;
    assert_eq!(revived.fence(), renewed.fence());
    assert_eq!(take(&log), [("unsynced", NORMAL), ("revive", FULL)]);

    // A revival of a live lease is a renewal inside the same unsynced job.
    clock.set(revived.expires_at().seconds() - 1);
    let extended = store.revive(revived.clone()).await?;
    assert!(extended.expires_at().seconds() > revived.expires_at().seconds());
    assert_eq!(take(&log), [("unsynced", NORMAL), ("revive", FULL)]);

    store.release(extended).await?;
    let acquired = store
        .acquire(AcquireBranch::new(
            selector,
            fixture::id(93_020)?,
            NonZeroU64::new(30).ok_or("lease ttl")?,
        ))
        .await?;
    let _ = store
        .prune(PruneBranch::new(
            acquired.clone(),
            committed.head(),
            BTreeSet::new(),
            1,
        ))
        .await;
    store.release(acquired).await?;
    store
        .dispatch("schedule_background_step", |b| {
            b.wal_pending = true;
            Ok(())
        })
        .await?;
    // A liveness bound only: generous, so load never trips it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut entries = Vec::new();
    while !entries.iter().any(|(label, _)| *label == "background") {
        assert!(Instant::now() < deadline, "no background step ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
        entries.extend(take(&log));
    }
    for label in ["release", "acquire", "prune"] {
        assert!(
            entries.iter().any(|(seen, _)| *seen == label),
            "{entries:?}"
        );
    }
    assert!(
        entries
            .iter()
            .all(|&(label, level)| label != "unsynced" && level == FULL),
        "{entries:?}"
    );
    Ok(())
}

/// If `FULL` cannot be restored, whatever the cause, the renewal reports
/// `BackendUnavailable` and the connection refuses every later job the same
/// way, so no commit runs weakened. A reopened
/// store starts at `FULL` again.
#[tokio::test]
async fn a_connection_that_cannot_restore_full_refuses_further_work() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("runtime.sqlite3");
    let clock = Arc::new(StepClock(AtomicU64::new(100)));
    let store = SqliteSessionStore::open_with_clock(&path, clock.clone())?;
    let request = seed()?;
    let genesis = request.initial().clone();
    let selector = request.session();
    let created = store.create(request).await?;
    let commit = successor(created.lease(), created.branch(), &genesis)?;
    store
        .dispatch("fail_full_restore", |b| {
            b.full_restore_fault = Some(SessionStoreError::BackendUnavailable);
            Ok(())
        })
        .await?;

    clock.set(101);
    let unavailable: Result<(), _> = Err(SessionStoreError::BackendUnavailable);
    assert_eq!(
        store.renew(created.lease().clone()).await.map(|_| ()),
        unavailable
    );
    assert_eq!(store.commit(commit).await.map(|_| ()), unavailable);
    assert_eq!(
        store.revive(created.lease().clone()).await.map(|_| ()),
        unavailable
    );
    assert_eq!(store.load_branch(selector).await.map(|_| ()), unavailable);
    assert_eq!(store.usage().await.map(|_| ()), unavailable);
    assert_eq!(store.flush().await.map(|_| ()), unavailable);
    assert_eq!(store.shutdown().await.map(|_| ()), unavailable);
    let wrote = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&wrote);
    assert_eq!(
        store
            .dispatch("refused_job", move |_| {
                probe.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await,
        unavailable
    );
    assert!(
        !wrote.load(Ordering::SeqCst),
        "a poisoned connection ran a job"
    );
    drop(store);

    let reopened = SqliteSessionStore::open_with_clock(&path, clock.clone())?;
    let level = reopened
        .dispatch_read("synchronous", |b| {
            b.connection
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .map_err(map_sqlite)
        })
        .await?;
    assert_eq!(level, FULL);
    assert_eq!(
        reopened.load_branch(selector).await?.head(),
        created.branch(),
        "the refused commit wrote nothing"
    );
    // The renewal committed before its restore failed; its grant is current.
    let current = reopened.renew(created.lease().clone()).await?;
    assert_eq!(current.fence(), created.lease().fence());
    assert_eq!(current.expires_at().seconds(), 131);
    reopened
        .commit(successor(&current, created.branch(), &genesis)?)
        .await?;
    Ok(())
}

#[tokio::test]
async fn capacity_during_full_restore_parks_without_permanently_poisoning_the_actor() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let clock = Arc::new(StepClock(AtomicU64::new(100)));
    let store = SqliteSessionStore::open_with_clock(
        directory.path().join("runtime.sqlite3"),
        clock.clone(),
    )?;
    let request = seed()?;
    let genesis = request.initial().clone();
    let created = store.create(request).await?;
    store
        .dispatch("arm_restore_capacity", |backend| {
            backend.full_restore_fault = Some(SessionStoreError::StorageFull);
            Ok(())
        })
        .await?;
    clock.set(101);
    assert_eq!(
        store.renew(created.lease().clone()).await,
        Err(SessionStoreError::StorageFull)
    );
    assert!(store.storage_unavailable());
    let renewed = store.renew(created.lease().clone()).await?;
    store
        .dispatch("check_full_restored", |backend| {
            let level: i64 = backend
                .connection
                .pragma_query_value(None, "synchronous", |row| row.get(0))
                .map_err(map_sqlite)?;
            assert_eq!(level, FULL);
            Ok(())
        })
        .await?;
    let receipt = store
        .commit(successor(&renewed, created.branch(), &genesis)?)
        .await?;
    assert_eq!(receipt.previous(), created.branch());
    assert!(!store.storage_unavailable());
    Ok(())
}
