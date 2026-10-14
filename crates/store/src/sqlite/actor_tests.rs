use super::*;
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use std::task::{Context, Poll, Waker};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn schema_24_separates_metadata_payloads_and_persistent_gc_queue() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    store.create(seed()?).await?;
    store.dispatch_read(|b| {
        let version: i64 = b.connection.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(map_sqlite)?;
        assert_eq!(version, 24);
        let sql: String = b.connection.query_row("SELECT sql FROM sqlite_schema WHERE name='runtime_objects'", [], |r| r.get(0)).map_err(map_sqlite)?;
        assert!(!sql.contains("WITHOUT ROWID"));
        let invalid: i64 = b.connection.query_row("SELECT count(*) FROM runtime_objects o LEFT JOIN runtime_object_bytes p USING(id) WHERE p.id IS NULL OR o.byte_len<>length(p.bytes)", [], |r| r.get(0)).map_err(map_sqlite)?;
        assert_eq!(invalid, 0);
        assert_eq!(b.connection.query_row("SELECT count(*) FROM gc_candidates", [], |r| r.get::<_, i64>(0)).map_err(map_sqlite)?, 0);
        Ok(())
    }).await?;
    Ok(())
}

#[tokio::test]
async fn background_gc_publishes_usage_before_the_next_step() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let created = store.create(seed()?).await?;
    store.release(created.lease().clone()).await?;
    let archived = store
        .archive(ArchiveSession::new(
            created.session().session_id(),
            created.session().revision(),
        ))
        .await?;
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    store
        .dispatch(move |b| {
            let mut steps = 0;
            b.job_hook = Some(Box::new(move |kind| {
                if kind == ExecutedJob::Background && steps < 2 {
                    steps += 1;
                    assert!(entered_tx.send(()).is_ok());
                    assert!(release_rx.recv().is_ok());
                }
            }));
            Ok(())
        })
        .await?;
    store
        .delete(DeleteSession::new(
            archived.session().session_id(),
            archived.session().revision(),
        ))
        .await?;
    entered_rx.recv()?;
    let before = store.usage().await?;
    release_tx.send(())?;
    entered_rx.recv()?;
    let after = store.usage().await?;
    release_tx.send(())?;
    assert!(before.bounds()[1].used() > 0);
    assert_eq!(after.bounds()[1].used(), 0);
    assert_eq!(after.bounds()[2].used(), 0);
    Ok(())
}

struct FixedClock(LeaseInstant);

impl LeaseClock for FixedClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        Ok(self.0)
    }
}

fn park(store: &SqliteSessionStore) -> (StoreFuture<'_, ()>, mpsc::SyncSender<()>) {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let parked = store.dispatch(move |_| {
        entered_tx
            .send(())
            .map_err(|_| SessionStoreError::BackendUnavailable)?;
        release_rx
            .recv()
            .map_err(|_| SessionStoreError::BackendUnavailable)?;
        Ok(())
    });
    assert!(entered_rx.recv().is_ok());
    (parked, release_tx)
}

#[tokio::test]
async fn critical_jobs_overtake_queued_reads() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let (parked, release) = park(&store);
    let read = store.stats();
    let critical = store.dispatch(|b| {
        b.connection
            .execute("UPDATE store_meta SET semantic_mutations=1 WHERE id=1", [])
            .map_err(map_sqlite)?;
        Ok(())
    });
    release.send(())?;
    parked.await?;
    critical.await?;
    assert_eq!(read.await?.semantic_mutations(), 1);
    Ok(())
}

#[tokio::test]
async fn usage_snapshot_does_not_dispatch_or_wait_for_actor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let expected = store.usage().await?;
    let (parked, release) = park(&store);
    let mut usage = store.usage();
    let result = usage.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    release.send(())?;
    parked.await?;
    assert_eq!(result, Poll::Ready(Ok(expected)));
    Ok(())
}

#[tokio::test]
async fn actor_lanes_have_independent_bounded_capacity() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open_with_clock_and_options(
        directory.path().join("runtime.sqlite3"),
        Arc::new(SystemLeaseClock::new()?),
        SqliteStoreOptions::new(MemoryStoreLimits::default(), NonZeroUsize::MIN),
    )?;
    let (parked, release) = park(&store);
    let critical = store.dispatch(|_| Ok(()));
    let read = store.stats();
    assert_eq!(
        store.dispatch(|_| Ok(())).await,
        Err(SessionStoreError::BackendBusy)
    );
    assert_eq!(store.stats().await, Err(SessionStoreError::BackendBusy));
    assert!(store.usage().await.is_ok());
    release.send(())?;
    parked.await?;
    critical.await?;
    read.await?;
    Ok(())
}

#[tokio::test]
async fn background_aging_runs_one_step_after_32_foreground_jobs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let executed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&executed);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    store
        .dispatch(move |b| {
            let mut first = true;
            b.job_hook = Some(Box::new(move |kind| {
                if let Ok(mut log) = log.lock() {
                    log.push(kind);
                }
                if kind == ExecutedJob::Background && first {
                    first = false;
                    assert!(entered_tx.send(()).is_ok());
                    assert!(release_rx.recv().is_ok());
                }
            }));
            b.wal_pending = true;
            b.vacuum_pending = true;
            Ok(())
        })
        .await?;
    entered_rx.recv()?;
    let jobs: Vec<_> = (0..40).map(|_| store.dispatch(|_| Ok(()))).collect();
    release_tx.send(())?;
    for job in jobs {
        job.await?;
    }
    let log = executed.lock().map_err(|_| "execution log")?;
    assert_eq!(log[0], ExecutedJob::Background);
    assert!(log[1..33].iter().all(|kind| *kind == ExecutedJob::Critical));
    assert_eq!(log[33], ExecutedJob::Background);
    assert!(
        log[34..42]
            .iter()
            .all(|kind| *kind == ExecutedJob::Critical)
    );
    Ok(())
}

fn seed() -> Result<CreateSession, Box<dyn std::error::Error>> {
    use crate::integrity_fixture as fixture;
    let session = fixture::id::<SessionId>(91_000)?;
    let branch = fixture::id::<plakous_agent::internal::BranchId>(91_001)?;
    let checkpoint = fixture::checkpoint(
        fixture::topology(session, branch, fixture::id(91_002)?, None, 0, None, 1)?,
        1,
        RuntimeDurability::Required,
    )?;
    Ok(CreateSession::new(
        SessionRef::new(session, branch),
        Timestamp::new(50, 0)?,
        fixture::id(91_003)?,
        NonZeroU64::new(3600).ok_or("lease ttl")?,
        fixture::bundle(&checkpoint, None)?,
    ))
}

fn next_checkpoint(
    created: &CreateReceipt,
    prior: &CheckpointObjectBundle,
    revision: u64,
) -> Result<CommitCheckpoint, Box<dyn std::error::Error>> {
    use crate::integrity_fixture as fixture;
    let selector = created.branch().session();
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            selector.session_id(),
            selector.branch_id(),
            fixture::id(91_010 + u128::from(revision))?,
            Some(prior.checkpoint_id()),
            revision,
            None,
            created.lease().fence().get(),
        )?,
        u32::try_from(revision + 1)?,
        RuntimeDurability::Required,
    )?;
    Ok(CommitCheckpoint::new(
        created.lease().clone(),
        created.branch(),
        fixture::bundle(&checkpoint, Some(prior))?,
    ))
}

#[tokio::test]
async fn queued_renew_and_commit_run_before_next_background_step() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open_with_clock(
        directory.path().join("runtime.sqlite3"),
        Arc::new(FixedClock(SystemLeaseClock::new()?.now()?)),
    )?;
    let request = seed()?;
    let bundle = request.initial().clone();
    let created = store.create(request).await?;
    let commit = next_checkpoint(&created, &bundle, 1)?;
    let executed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&executed);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    store
        .dispatch(move |b| {
            let mut first = true;
            b.job_hook = Some(Box::new(move |kind| {
                if let Ok(mut log) = log.lock() {
                    log.push(kind);
                }
                if kind == ExecutedJob::Background && first {
                    first = false;
                    assert!(entered_tx.send(()).is_ok());
                    assert!(release_rx.recv().is_ok());
                }
            }));
            b.wal_pending = true;
            b.vacuum_pending = true;
            Ok(())
        })
        .await?;
    entered_rx.recv()?;
    let read = store.stats();
    let renew = store.renew(created.lease().clone());
    let commit = store.commit(commit);
    release_tx.send(())?;
    renew.await?;
    commit.await?;
    assert_eq!(read.await?.checkpoints(), 2);
    let log = executed.lock().map_err(|_| "execution log")?;
    assert_eq!(
        &log[..4],
        &[
            ExecutedJob::Background,
            ExecutedJob::Critical,
            ExecutedJob::Critical,
            ExecutedJob::Read
        ]
    );
    Ok(())
}

#[tokio::test]
async fn usage_snapshot_is_published_before_mutation_acknowledgment() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("runtime.sqlite3");
    let store = SqliteSessionStore::open(&path)?;
    assert_eq!(store.usage().await?.bounds()[0].used(), 0);
    store.create(seed()?).await?;
    let expected = store
        .dispatch_read(|b| usage(&b.connection, b.limits))
        .await?;
    assert_eq!(expected.bounds()[0].used(), 1);
    assert_eq!(store.usage().await?, expected);
    drop(store);
    assert_eq!(SqliteSessionStore::open(&path)?.usage().await?, expected);
    Ok(())
}

#[tokio::test]
async fn sqlite_page_cache_is_64_mib_and_default_auto_checkpoint_is_enabled() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let pragmas = store
        .dispatch_read(|b| {
            Ok((
                b.connection
                    .pragma_query_value(None, "cache_size", |r| r.get::<_, i64>(0))
                    .map_err(map_sqlite)?,
                b.connection
                    .pragma_query_value(None, "wal_autocheckpoint", |r| r.get::<_, i64>(0))
                    .map_err(map_sqlite)?,
            ))
        })
        .await?;
    assert_eq!(pragmas, (-65536, 1000));
    Ok(())
}

#[tokio::test]
async fn usage_snapshot_catches_mutations_that_commit_before_an_error() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let request = seed()?;
    let result = store
        .dispatch(move |b| {
            create(b, &request)?;
            Err::<(), _>(SessionStoreError::BackendUnavailable)
        })
        .await;
    assert_eq!(result, Err(SessionStoreError::BackendUnavailable));
    assert_eq!(store.usage().await?.bounds()[0].used(), 1);
    Ok(())
}

#[tokio::test]
async fn vacuum_background_step_frees_at_most_1024_pages() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let (before, after) = store
        .dispatch(|b| {
            b.connection
                .execute_batch(
                    "CREATE TABLE housekeeping_test(payload BLOB);
             INSERT INTO housekeeping_test VALUES(zeroblob(8388608));
             DROP TABLE housekeeping_test;",
                )
                .map_err(map_sqlite)?;
            let free_pages = |c: &Connection| {
                c.pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0))
                    .map_err(map_sqlite)
            };
            let before = free_pages(&b.connection)?;
            b.vacuum_pending = true;
            assert!(b.background_step());
            let after = free_pages(&b.connection)?;
            b.vacuum_pending = false;
            b.wal_pending = false;
            Ok((before, after))
        })
        .await?;
    assert!(before > i64::from(BACKGROUND_VACUUM_PAGES));
    assert!(after < before);
    assert!(before - after <= i64::from(BACKGROUND_VACUUM_PAGES));
    Ok(())
}

thread_local! {
    static STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static READ_OBJECTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static WAL_CHECKPOINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VACUUM_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "SQLite's trace callback takes its event by value"
)]
fn count_statement(event: TraceEvent<'_>) {
    let TraceEvent::Stmt(_, sql) = event else {
        return;
    };
    STATEMENTS.with(|count| count.set(count.get() + 1));
    if sql.contains("wal_checkpoint") {
        WAL_CHECKPOINTS.with(|count| count.set(count.get() + 1));
    }
    if sql.contains("incremental_vacuum(") {
        VACUUM_STEPS.with(|count| count.set(count.get() + 1));
    }
    if sql.contains("runtime_objects") || sql.contains("WITH RECURSIVE") {
        READ_OBJECTS.with(|read| read.set(true));
    }
}

#[tokio::test]
async fn shutdown_with_pending_vacuum_runs_only_one_checkpoint() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let before = store
        .dispatch(|b| {
            b.connection
                .execute_batch(
                    "CREATE TABLE housekeeping_test(payload BLOB);
             INSERT INTO housekeeping_test VALUES(zeroblob(8388608));
             DROP TABLE housekeeping_test;",
                )
                .map_err(map_sqlite)?;
            b.connection
                .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
            WAL_CHECKPOINTS.with(|count| count.set(0));
            VACUUM_STEPS.with(|count| count.set(0));
            b.connection
                .pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0))
                .map_err(map_sqlite)
        })
        .await?;
    let (parked, release) = park(&store);
    let pending = store.dispatch(|b| {
        b.vacuum_pending = true;
        b.wal_pending = true;
        Ok(())
    });
    let shutdown = store.shutdown();
    release.send(())?;
    parked.await?;
    pending.await?;
    shutdown.await?;
    let (after, wal, vacuum) = store
        .dispatch_read(|b| {
            b.connection.trace_v2(TraceEventCodes::empty(), None);
            Ok((
                b.connection
                    .pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0))
                    .map_err(map_sqlite)?,
                WAL_CHECKPOINTS.with(std::cell::Cell::get),
                VACUUM_STEPS.with(std::cell::Cell::get),
            ))
        })
        .await?;
    assert!(before > i64::from(BACKGROUND_VACUUM_PAGES));
    assert_eq!(
        after, before,
        "shutdown leaves the vacuum backlog untouched"
    );
    assert_eq!((wal, vacuum), (1, 0));
    Ok(())
}

#[tokio::test]
async fn shutdown_with_pending_gc_leaves_candidates_and_counters_unchanged() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let (parked, release_gate) = park(&store);
    let pending = store.dispatch(|b| {
        let created = create(b, &seed().map_err(|_| SessionStoreError::corrupt())?)?;
        release(b, created.lease())?;
        let archived = archive(
            b,
            ArchiveSession::new(created.session().session_id(), created.session().revision()),
        )?;
        delete(
            b,
            DeleteSession::new(
                archived.session().session_id(),
                archived.session().revision(),
            ),
        )?;
        b.connection
            .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
        WAL_CHECKPOINTS.with(|count| count.set(0));
        VACUUM_STEPS.with(|count| count.set(0));
        Ok(read_stats(&b.connection)?.objects())
    });
    let shutdown = store.shutdown();
    release_gate.send(())?;
    parked.await?;
    let before = pending.await?;
    shutdown.await?;
    let (after, queued, wal, vacuum) = store
        .dispatch_read(|b| {
            b.connection.trace_v2(TraceEventCodes::empty(), None);
            Ok((
                read_stats(&b.connection)?.objects(),
                gc::pending(&b.connection)?,
                WAL_CHECKPOINTS.with(std::cell::Cell::get),
                VACUUM_STEPS.with(std::cell::Cell::get),
            ))
        })
        .await?;
    assert!(before > 0);
    assert_eq!(after, before);
    assert!(queued);
    assert_eq!((wal, vacuum), (1, 0));
    Ok(())
}

#[tokio::test]
async fn vacuum_backlog_is_scheduled_again_on_open() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("runtime.sqlite3");
    let store = SqliteSessionStore::open(&path)?;
    store
        .dispatch(|b| {
            b.connection
                .execute_batch(
                    "CREATE TABLE housekeeping_test(payload BLOB);
             INSERT INTO housekeeping_test VALUES(zeroblob(8388608));
             DROP TABLE housekeeping_test;",
                )
                .map_err(map_sqlite)
        })
        .await?;
    store.shutdown().await?;
    drop(store);
    let clock: Arc<dyn LeaseClock> = Arc::new(SystemLeaseClock::new()?);
    let progress = StartupProgress(Mutex::new(StartupStage::Connection));
    let mut backend = Backend::open(
        &path,
        Arc::clone(&clock),
        clock.now()?.domain(),
        SqliteStoreOptions::default(),
        &progress,
    )?;
    assert!(backend.vacuum_pending && backend.wal_pending);
    let pages = u64::try_from(backend.connection.pragma_query_value(
        None,
        "freelist_count",
        |r| r.get::<_, i64>(0),
    )?)?;
    let steps = pages.div_ceil(u64::from(BACKGROUND_VACUUM_PAGES)) * 2 + 2;
    for _ in 0..steps {
        backend.background_step();
    }
    assert!(!backend.vacuum_pending && !backend.wal_pending);
    assert_eq!(
        backend
            .connection
            .pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}

async fn traced_branch_load(
    store: &SqliteSessionStore,
    selector: SessionRef,
) -> Result<(LoadedBranch, usize, bool), Box<dyn std::error::Error>> {
    store
        .dispatch_read(|b| {
            b.connection
                .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
            STATEMENTS.with(|count| count.set(0));
            READ_OBJECTS.with(|read| read.set(false));
            Ok(())
        })
        .await?;
    let loaded = store.load_branch(selector).await?;
    let (count, read_objects) = store
        .dispatch_read(|b| {
            b.connection.trace_v2(TraceEventCodes::empty(), None);
            Ok((
                STATEMENTS.with(std::cell::Cell::get),
                READ_OBJECTS.with(std::cell::Cell::get),
            ))
        })
        .await?;
    Ok((loaded, count, read_objects))
}

#[tokio::test]
async fn validated_checkpoint_cache_bounds_second_branch_load_to_three_statements() -> TestResult {
    use crate::integrity_fixture as fixture;
    for history_items in [1, 1000] {
        let directory = tempfile::tempdir()?;
        let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
        let request = seed()?;
        let checkpoint = fixture::checkpoint(
            request
                .initial()
                .validated_checkpoint()?
                .manifest()
                .topology(),
            history_items,
            RuntimeDurability::Required,
        )?;
        let request = CreateSession::new(
            request.session(),
            request.created_at(),
            request.owner(),
            request.lease_seconds(),
            fixture::bundle(&checkpoint, None)?,
        );
        let selector = request.session();
        store.create(request).await?;
        let (cold, cold_count, cold_objects) = traced_branch_load(&store, selector).await?;
        let (cached, cached_count, cached_objects) = traced_branch_load(&store, selector).await?;
        assert_eq!(cached.checkpoint(), cold.checkpoint());
        assert!(cold_count > 3 && cold_objects);
        assert!(
            cached_count <= 3,
            "{history_items} history items: {cached_count} cached statements"
        );
        assert!(
            !cached_objects,
            "a cache hit never reads object payloads or walks edges"
        );
    }
    Ok(())
}

async fn checkpoint_chain(
    store: &SqliteSessionStore,
) -> Result<(CreateReceipt, BranchHead, Vec<StoredCheckpoint>), Box<dyn std::error::Error>> {
    let request = seed()?;
    let mut prior = request.initial().clone();
    let created = store.create(request).await?;
    let mut head = created.branch();
    let mut checkpoints = vec![store.load_checkpoint(prior.checkpoint_id()).await?];
    for revision in 1..=3 {
        let next = next_checkpoint(&created, &prior, revision)?;
        prior = next.checkpoint().clone();
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                prior.clone(),
            ))
            .await?
            .head();
        checkpoints.push(store.load_checkpoint(prior.checkpoint_id()).await?);
    }
    Ok((created, head, checkpoints))
}

#[tokio::test]
async fn validated_checkpoint_cache_is_byte_bounded_and_evicts_least_recently_used() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let (_, _, checkpoints) = checkpoint_chain(&store).await?;
    let checkpoints = &checkpoints[..3];
    let weights: Vec<_> = checkpoints.iter().map(checkpoint_cache::weight).collect();
    let total: usize = weights.iter().sum();
    let budget = total - weights.iter().min().ok_or("weights")?;
    let mut cache = CheckpointCache::new(budget);
    let ids: Vec<_> = checkpoints
        .iter()
        .map(|c| c.reference().checkpoint_id())
        .collect();
    cache.insert(checkpoints[0].clone());
    cache.insert(checkpoints[1].clone());
    assert!(cache.get(ids[0]).is_some());
    cache.insert(checkpoints[2].clone());
    assert!(
        cache.get(ids[1]).is_none(),
        "least recently used entry is evicted"
    );
    assert!(cache.get(ids[0]).is_some());
    assert!(cache.get(ids[2]).is_some());
    cache.remove(ids[0]);
    assert!(cache.get(ids[0]).is_none());
    cache.insert(checkpoints[1].clone());
    assert!(cache.get(ids[1]).is_some());
    let mut oversized = CheckpointCache::new(weights[0] - 1);
    oversized.insert(checkpoints[0].clone());
    assert!(oversized.get(ids[0]).is_none());
    cache.clear();
    assert!(ids.iter().all(|id| cache.get(*id).is_none()));
    Ok(())
}

#[tokio::test]
async fn validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact() -> TestResult {
    for bypass_eviction in [false, true] {
        let directory = tempfile::tempdir()?;
        let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
        let (created, head, checkpoints) = checkpoint_chain(&store).await?;
        let pruned_id = checkpoints[1].reference().checkpoint_id();
        let head_id = head.checkpoint().checkpoint_id();
        let request = PruneBranch::new(created.lease().clone(), head, BTreeSet::new(), 1);
        if bypass_eviction {
            // Leave a stale entry deliberately: even a hit must probe authority.
            store.dispatch(move |b| prune(b, &request)).await?;
        } else {
            store.prune(request).await?;
            assert!(
                store
                    .dispatch_read(move |b| Ok(b.checkpoints.get(head_id).is_none()))
                    .await?
            );
        }
        assert_eq!(
            store.load_checkpoint(pruned_id).await.err(),
            Some(SessionStoreError::Pruned)
        );
        assert_eq!(store.load_checkpoint(head_id).await?, checkpoints[3]);
        let unknown = crate::integrity_fixture::id::<CheckpointId>(99_999)?;
        assert_eq!(
            store.load_checkpoint(unknown).await.err(),
            Some(SessionStoreError::NotFound)
        );
        store.release(created.lease().clone()).await?;
        let archived = store
            .archive(ArchiveSession::new(
                created.session().session_id(),
                created.session().revision(),
            ))
            .await?;
        let request = DeleteSession::new(
            archived.session().session_id(),
            archived.session().revision(),
        );
        if bypass_eviction {
            store.dispatch(move |b| delete(b, request)).await?;
        } else {
            store.delete(request).await?;
            assert!(
                store
                    .dispatch_read(move |b| Ok(b.checkpoints.get(head_id).is_none()))
                    .await?
            );
        }
        for checkpoint in &checkpoints {
            assert_eq!(
                store
                    .load_checkpoint(checkpoint.reference().checkpoint_id())
                    .await
                    .err(),
                Some(SessionStoreError::NotFound)
            );
        }
        assert_eq!(store.usage().await?.bounds()[0].used(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn validated_checkpoint_cache_hit_still_verifies_checkpoint_row_seal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let created = store.create(seed()?).await?;
    store.load_branch(created.branch().session()).await?;
    store
        .dispatch(|b| {
            b.connection
                .execute("UPDATE checkpoints SET validation_seal=zeroblob(32)", [])
                .map_err(map_sqlite)?;
            Ok(())
        })
        .await?;
    assert_eq!(
        store.load_branch(created.branch().session()).await.err(),
        Some(SessionStoreError::corrupt())
    );
    Ok(())
}
