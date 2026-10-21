use super::*;
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use std::task::{Context, Poll, Waker};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn schema_25_separates_metadata_payloads_and_persistent_gc_queue() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    store.create(seed()?).await?;
    store.dispatch_read("schema_25_separates_metadata_payloads_and_persistent_gc_queue", |b| {
        let version: i64 = b.connection.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(map_sqlite)?;
        assert_eq!(version, 25);
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
        .dispatch(
            "background_gc_publishes_usage_before_the_next_step",
            move |b| {
                let mut steps = 0;
                b.job_hook = Some(Box::new(move |kind| {
                    if kind == ExecutedJob::Background && steps < 2 {
                        steps += 1;
                        assert!(entered_tx.send(()).is_ok());
                        assert!(release_rx.recv().is_ok());
                    }
                }));
                Ok(())
            },
        )
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
    let parked = store.dispatch("park", move |_| {
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
    let critical = store.dispatch("critical_jobs_overtake_queued_reads", |b| {
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
    let critical = store.dispatch("actor_lanes_have_independent_bounded_capacity", |_| Ok(()));
    let read = store.stats();
    assert_eq!(
        store
            .dispatch("actor_lanes_have_independent_bounded_capacity", |_| Ok(()))
            .await,
        Err(SessionStoreError::BackendBusy)
    );
    let mut backpressured = store.stats();
    assert!(
        backpressured
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(store.usage().await.is_ok());
    release.send(())?;
    parked.await?;
    critical.await?;
    read.await?;
    backpressured.await?;
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
        .dispatch(
            "background_aging_runs_one_step_after_32_foreground_jobs",
            move |b| {
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
            },
        )
        .await?;
    entered_rx.recv()?;
    let jobs: Vec<_> = (0..40)
        .map(|_| {
            store.dispatch(
                "background_aging_runs_one_step_after_32_foreground_jobs",
                |_| Ok(()),
            )
        })
        .collect();
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
        .dispatch(
            "queued_renew_and_commit_run_before_next_background_step",
            move |b| {
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
            },
        )
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
        .dispatch_read(
            "usage_snapshot_is_published_before_mutation_acknowledgment",
            |b| usage(&b.connection, b.limits),
        )
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
        .dispatch_read(
            "sqlite_page_cache_is_64_mib_and_default_auto_checkpoint_is_enabled",
            |b| {
                Ok((
                    b.connection
                        .pragma_query_value(None, "cache_size", |r| r.get::<_, i64>(0))
                        .map_err(map_sqlite)?,
                    b.connection
                        .pragma_query_value(None, "wal_autocheckpoint", |r| r.get::<_, i64>(0))
                        .map_err(map_sqlite)?,
                ))
            },
        )
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
        .dispatch(
            "usage_snapshot_catches_mutations_that_commit_before_an_error",
            move |b| {
                create(b, &request)?;
                Err::<(), _>(SessionStoreError::BackendUnavailable)
            },
        )
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
        .dispatch("vacuum_background_step_frees_at_most_1024_pages", |b| {
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
        .dispatch(
            "shutdown_with_pending_vacuum_runs_only_one_checkpoint",
            |b| {
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
            },
        )
        .await?;
    let (parked, release) = park(&store);
    let pending = store.dispatch(
        "shutdown_with_pending_vacuum_runs_only_one_checkpoint",
        |b| {
            b.vacuum_pending = true;
            b.wal_pending = true;
            Ok(())
        },
    );
    let shutdown = store.shutdown();
    release.send(())?;
    parked.await?;
    pending.await?;
    shutdown.await?;
    let (after, wal, vacuum) = store
        .dispatch_read(
            "shutdown_with_pending_vacuum_runs_only_one_checkpoint",
            |b| {
                b.connection.trace_v2(TraceEventCodes::empty(), None);
                Ok((
                    b.connection
                        .pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0))
                        .map_err(map_sqlite)?,
                    WAL_CHECKPOINTS.with(std::cell::Cell::get),
                    VACUUM_STEPS.with(std::cell::Cell::get),
                ))
            },
        )
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
    let pending = store.dispatch(
        "shutdown_with_pending_gc_leaves_candidates_and_counters_unchanged",
        |b| {
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
        },
    );
    let shutdown = store.shutdown();
    release_gate.send(())?;
    parked.await?;
    let before = pending.await?;
    shutdown.await?;
    let (after, queued, wal, vacuum) = store
        .dispatch_read(
            "shutdown_with_pending_gc_leaves_candidates_and_counters_unchanged",
            |b| {
                b.connection.trace_v2(TraceEventCodes::empty(), None);
                Ok((
                    read_stats(&b.connection)?.objects(),
                    gc::pending(&b.connection)?,
                    WAL_CHECKPOINTS.with(std::cell::Cell::get),
                    VACUUM_STEPS.with(std::cell::Cell::get),
                ))
            },
        )
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
        .dispatch("vacuum_backlog_is_scheduled_again_on_open", |b| {
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
        .dispatch_read("traced_branch_load", |b| {
            b.connection
                .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
            STATEMENTS.with(|count| count.set(0));
            READ_OBJECTS.with(|read| read.set(false));
            Ok(())
        })
        .await?;
    let loaded = store.load_branch(selector).await?;
    let (count, read_objects) = store
        .dispatch_read("traced_branch_load", |b| {
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
async fn validated_checkpoint_cache_bounds_second_branch_load_to_four_statements() -> TestResult {
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
        store
            .dispatch(
                "validated_checkpoint_cache_bounds_second_branch_load_to_four_statements",
                |b| {
                    b.checkpoints = CheckpointCache::new(1);
                    Ok(())
                },
            )
            .await?;
        let (cold, cold_count, cold_objects) = traced_branch_load(&store, selector).await?;
        let (cached, cached_count, cached_objects) = traced_branch_load(&store, selector).await?;
        assert_eq!(cached.checkpoint(), cold.checkpoint());
        assert!(cold_count > 3 && cold_objects);
        // Session, branch, horizon and checkpoint rows; never the objects.
        assert!(
            cached_count <= 4,
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
    oversized.insert(checkpoints[2].clone());
    oversized.insert(checkpoints[0].clone());
    assert!(oversized.get(ids[2]).is_none());
    assert!(oversized.get(ids[0]).is_some());
    oversized.insert(checkpoints[1].clone());
    assert!(oversized.get(ids[0]).is_none());
    assert!(oversized.get(ids[1]).is_some());
    assert!(oversized.get(ids[2]).is_none());
    oversized.clear();
    assert!(ids.iter().all(|id| oversized.get(*id).is_none()));
    cache.clear();
    assert!(ids.iter().all(|id| cache.get(*id).is_none()));
    Ok(())
}

#[tokio::test]
async fn validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact() -> TestResult {
    for (bypass_eviction, budget) in [
        (false, CHECKPOINT_CACHE_BYTES),
        (true, CHECKPOINT_CACHE_BYTES),
        (false, 1),
        (true, 1),
    ] {
        let directory = tempfile::tempdir()?;
        let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
        store
            .dispatch(
                "validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact",
                move |b| {
                    b.checkpoints = CheckpointCache::new(budget);
                    Ok(())
                },
            )
            .await?;
        let (created, head, checkpoints) = checkpoint_chain(&store).await?;
        let pruned_id = checkpoints[1].reference().checkpoint_id();
        let head_id = head.checkpoint().checkpoint_id();
        let request = PruneBranch::new(created.lease().clone(), head, BTreeSet::new(), 1);
        store.load_checkpoint(pruned_id).await?;
        assert!(
            store
                .dispatch_read(
                    "validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact",
                    move |b| Ok(b.checkpoints.get(pruned_id).is_some())
                )
                .await?
        );
        if bypass_eviction {
            // Leave a stale entry deliberately: even a hit must probe authority.
            store
                .dispatch(
                    "validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact",
                    move |b| prune(b, &request),
                )
                .await?;
        } else {
            store.prune(request).await?;
            assert!(
                store
                    .dispatch_read(
                        "validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact",
                        move |b| Ok(b.checkpoints.get(head_id).is_none()
                            && b.checkpoints.get(pruned_id).is_none())
                    )
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
            store
                .dispatch(
                    "validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact",
                    move |b| delete(b, request),
                )
                .await?;
        } else {
            store.delete(request).await?;
            assert!(
                store
                    .dispatch_read(
                        "validated_checkpoint_cache_keeps_pruned_and_deleted_lookups_exact",
                        move |b| Ok(b.checkpoints.get(head_id).is_none())
                    )
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
    for budget in [CHECKPOINT_CACHE_BYTES, 1] {
        let directory = tempfile::tempdir()?;
        let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
        let created = store.create(seed()?).await?;
        store
            .dispatch(
                "validated_checkpoint_cache_hit_still_verifies_checkpoint_row_seal",
                move |b| {
                    b.checkpoints = CheckpointCache::new(budget);
                    Ok(())
                },
            )
            .await?;
        store.load_branch(created.branch().session()).await?;
        let head_id = created.branch().checkpoint().checkpoint_id();
        assert!(
            store
                .dispatch_read(
                    "validated_checkpoint_cache_hit_still_verifies_checkpoint_row_seal",
                    move |b| Ok(b.checkpoints.get(head_id).is_some())
                )
                .await?
        );
        store
            .dispatch(
                "validated_checkpoint_cache_hit_still_verifies_checkpoint_row_seal",
                |b| {
                    b.connection
                        .execute("UPDATE checkpoints SET validation_seal=zeroblob(32)", [])
                        .map_err(map_sqlite)?;
                    Ok(())
                },
            )
            .await?;
        assert_eq!(
            store.load_branch(created.branch().session()).await.err(),
            Some(SessionStoreError::corrupt())
        );
    }
    Ok(())
}

#[test]
fn lane_overflow_logs_the_first_then_once_per_interval_with_the_count() {
    let mut log = OverflowLog::default();
    let start = Instant::now();
    assert_eq!(log.record(start), Some(1));
    assert_eq!(log.record(start + Duration::from_secs(1)), None);
    assert_eq!(log.record(start + OVERFLOW_LOG_INTERVAL / 2), None);
    assert_eq!(log.record(start + OVERFLOW_LOG_INTERVAL), Some(3));
    assert_eq!(log.record(start + OVERFLOW_LOG_INTERVAL * 2), Some(1));
}

const ACTOR_TARGET: &str = "plakous_store::sqlite_actor";

/// Captures WARN events of one target as `field=value` lines.
#[derive(Clone)]
struct Warnings {
    target: &'static str,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Warnings {
    fn of(target: &'static str) -> Self {
        Self {
            target,
            lines: Arc::default(),
        }
    }

    fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl tracing::Subscriber for Warnings {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(self.0, "{}={value:?} ", field.name());
            }
        }
        let metadata = event.metadata();
        if *metadata.level() == tracing::Level::WARN && metadata.target() == self.target {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(fields.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn a_full_lane_warns_with_its_name_capacity_and_count() -> TestResult {
    let warnings = Warnings::of(ACTOR_TARGET);
    let (sender, _receiver) = actor_queue(1);
    tracing::subscriber::with_default(warnings.clone(), || -> TestResult {
        sender.send(Lane::Read, Box::new(|_| {}))?;
        for _ in 0..3 {
            assert_eq!(
                sender.send(Lane::Read, Box::new(|_| {})).err(),
                Some(SessionStoreError::BackendBusy)
            );
        }
        sender.send(Lane::Critical, Box::new(|_| {}))?;
        assert_eq!(
            sender.send(Lane::Critical, Box::new(|_| {})).err(),
            Some(SessionStoreError::BackendBusy)
        );
        Ok(())
    })?;
    let logged = warnings.lines();
    assert_eq!(logged.len(), 2, "{logged:?}");
    assert!(
        logged[0].contains("lane=\"read\"")
            && logged[0].contains("capacity=1")
            && logged[0].contains("overflows=1")
            && logged[0].contains("critical_depth=0")
            && logged[0].contains("read_depth=1")
            && logged[0].contains("running_job=")
            && logged[0].contains("running_ms="),
        "{logged:?}"
    );
    assert!(logged[1].contains("lane=\"critical\""), "{logged:?}");
    Ok(())
}

#[test]
fn a_send_to_a_gone_actor_warns_and_refuses_as_unavailable() {
    let warnings = Warnings::of(ACTOR_TARGET);
    let (sender, receiver) = actor_queue(1);
    drop(receiver);
    let refused = tracing::subscriber::with_default(warnings.clone(), || {
        sender.send(Lane::Critical, Box::new(|_| {}))
    });
    assert_eq!(refused.err(), Some(SessionStoreError::BackendUnavailable));
    let logged = warnings.lines();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].contains("lane=\"critical\"") && logged[0].contains("reason=\"disconnected\""),
        "{logged:?}"
    );
}

#[test]
fn a_reply_lost_with_the_actor_warns_and_refuses_as_unavailable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let warnings = Warnings::of(ACTOR_TARGET);
    let lost = tracing::subscriber::with_default(warnings.clone(), || {
        runtime.block_on(store.dispatch_read(
            "a_reply_lost_with_the_actor_warns_and_refuses_as_unavailable",
            |_| -> Result<(), SessionStoreError> { panic!("the actor job panics before replying") },
        ))
    });
    assert_eq!(lost.err(), Some(SessionStoreError::BackendUnavailable));
    let join = store
        .worker
        .join
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .ok_or("actor join handle")?;
    assert!(join.join().is_err(), "the actor thread did not panic");
    let refused =
        tracing::subscriber::with_default(warnings.clone(), || runtime.block_on(store.stats()));
    assert_eq!(refused.err(), Some(SessionStoreError::BackendUnavailable));
    let logged = warnings.lines();
    assert_eq!(logged.len(), 2, "{logged:?}");
    assert!(
        logged[0].contains("lane=\"read\"") && logged[0].contains("reason=\"reply_dropped\""),
        "{logged:?}"
    );
    assert!(
        logged[1].contains("lane=\"read\"") && logged[1].contains("reason=\"disconnected\""),
        "{logged:?}"
    );
    Ok(())
}

/// Errors outside SQLite's own failure codes keep a content-free category in
/// the log; a point lookup that finds no row is an invariant breach.
#[test]
fn non_sqlite_failures_warn_with_a_static_category() {
    let warnings = Warnings::of("plakous_store::sqlite_operation");
    let mapped = tracing::subscriber::with_default(warnings.clone(), || {
        [
            rusqlite::Error::QueryReturnedNoRows,
            rusqlite::Error::InvalidQuery,
            rusqlite::Error::InvalidColumnName("private column".to_owned()),
            rusqlite::Error::InvalidColumnType(
                0,
                "private column".to_owned(),
                rusqlite::types::Type::Blob,
            ),
            rusqlite::Error::InvalidPath("private/database/path".into()),
        ]
        .map(map_sqlite)
    });
    assert_eq!(
        mapped,
        [
            SessionStoreError::corrupt(),
            SessionStoreError::BackendUnavailable,
            SessionStoreError::BackendUnavailable,
            SessionStoreError::corrupt(),
            SessionStoreError::BackendUnavailable,
        ]
    );
    let logged = warnings.lines();
    let categories: Vec<_> = logged
        .iter()
        .map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("error_category="))
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        categories,
        [
            "\"no_rows\"",
            "\"invalid_query\"",
            "\"invalid_query\"",
            "\"invalid_value\"",
            "\"other_rusqlite\"",
        ],
        "{logged:?}"
    );
    assert!(
        logged.iter().all(|line| !line.contains("private")),
        "{logged:?}"
    );
}

/// A tree's pollers await each read they issue, so a long critical job
/// queues one read per polling reader, however many cadence ticks it spans.
#[test]
fn polling_readers_queue_one_read_each_behind_a_long_critical_job() -> TestResult {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    // A root and eight children, three store-reading tasks each.
    const READERS: usize = 27;
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let (parked, release) = park(&store);
    let issued = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    thread::scope(|scope| -> TestResult {
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                scope.spawn(|| -> Result<(), SessionStoreError> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .map_err(|_| SessionStoreError::BackendUnavailable)?;
                    while !stop.load(Ordering::SeqCst) {
                        let read = store.stats();
                        issued.fetch_add(1, Ordering::SeqCst);
                        runtime.block_on(read)?;
                        thread::sleep(Duration::from_millis(2));
                    }
                    Ok(())
                })
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        while issued.load(Ordering::SeqCst) < READERS {
            assert!(Instant::now() < deadline, "readers did not start");
            thread::sleep(Duration::from_millis(1));
        }
        // Many cadence ticks pass while the critical job holds the actor.
        thread::sleep(Duration::from_millis(100));
        assert_eq!(issued.load(Ordering::SeqCst), READERS);
        // Even reads beyond the bounded queue wait in their caller futures.
        let mut probes: Vec<_> = (0..DEFAULT_QUEUE_CAPACITY).map(|_| store.stats()).collect();
        {
            let _entered = runtime.enter();
            for probe in &mut probes {
                assert!(
                    probe
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
        }
        assert_eq!(
            store.sender.activity.depths[Lane::Read as usize].load(Ordering::Relaxed),
            DEFAULT_QUEUE_CAPACITY
        );
        stop.store(true, Ordering::SeqCst);
        release.send(())?;
        runtime.block_on(parked)?;
        for probe in probes {
            runtime.block_on(probe)?;
        }
        for reader in readers {
            reader.join().map_err(|_| "reader panicked")??;
        }
        Ok(())
    })
}

#[tokio::test]
async fn commit_cache_seed_is_lazy_and_first_hit_equals_cold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let request = seed()?;
    let prior = request.initial().clone();
    let created = store.create(request).await?;
    let next = next_checkpoint(&created, &prior, 1)?;
    let receipt = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            next.checkpoint().clone(),
        ))
        .await?;
    store
        .dispatch_read("commit_does_not_normalize_cache_seed", |b| {
            assert_eq!(b.checkpoints.normalizations, 0);
            Ok(())
        })
        .await?;
    let before = store
        .dispatch_read("commit_cache_seed_is_lazy_and_first_hit_equals_cold", |b| {
            Ok(b.checkpoint_decodes)
        })
        .await?;
    let (_, statements, objects) = traced_branch_load(&store, receipt.head().session()).await?;
    let after = store
        .dispatch_read("commit_cache_seed_is_lazy_and_first_hit_equals_cold", |b| {
            Ok(b.checkpoint_decodes)
        })
        .await?;
    assert_eq!(after, before, "post-commit load performs zero decodes");
    store
        .dispatch_read("first_hit_normalizes_cache_seed_once", |b| {
            assert_eq!(b.checkpoints.normalizations, 1);
            Ok(())
        })
        .await?;
    assert!(!objects, "post-commit load must not decode object payloads");
    assert!(statements <= 4, "{statements}");
    let warm = store.load_branch(receipt.head().session()).await?;
    store
        .dispatch("clear_cache", |b| {
            b.checkpoints.clear();
            Ok(())
        })
        .await?;
    let cold = store.load_branch(receipt.head().session()).await?;
    assert_eq!(warm.checkpoint(), cold.checkpoint());
    assert_eq!(warm.checkpoint().bundle(), cold.checkpoint().bundle());
    Ok(())
}

#[test]
fn forked_head_seed_respects_the_existing_cache_budget() -> TestResult {
    for budget in [CHECKPOINT_CACHE_BYTES, 1] {
        let directory = tempfile::tempdir()?;
        let mut backend = gc_tests::backend(&directory.path().join("runtime.sqlite3"))?;
        backend.checkpoints = CheckpointCache::new(budget);
        let request = gc_tests::seed(100_000)?;
        let bundle = request.initial().clone();
        let created = create(&mut backend, &request)?;
        let source = gc_tests::Branch {
            head: created.branch(),
            lease: created.lease().clone(),
            bundle,
            value: 1,
        };
        let mut graph = BTreeMap::new();
        let forked = gc_tests::fork_branch(&source, &mut backend, &mut graph, 200_000)?;
        let before = backend.checkpoint_decodes;
        assert_eq!(backend.checkpoints.normalizations, 0);
        let loaded = backend.load_branch(forked.head.session())?;
        assert_eq!(backend.checkpoints.normalizations, usize::from(budget > 1));
        assert_eq!(loaded.head(), forked.head);
        assert_eq!(
            backend.checkpoint_decodes,
            before + usize::from(budget == 1)
        );
        backend.checkpoints.clear();
        let cold = backend.load_branch(forked.head.session())?;
        assert_eq!(loaded.checkpoint(), cold.checkpoint());
        assert_eq!(loaded.checkpoint().bundle(), cold.checkpoint().bundle());
    }
    Ok(())
}

#[tokio::test]
async fn a_turn_boundary_read_flood_waits_then_completes() -> TestResult {
    const READS: usize = DEFAULT_QUEUE_CAPACITY * 4;
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let (parked, release) = park(&store);
    let mut reads: Vec<_> = (0..READS).map(|_| store.stats()).collect();
    for read in &mut reads {
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert_eq!(
        store.sender.activity.depths[1].load(Ordering::Relaxed),
        DEFAULT_QUEUE_CAPACITY
    );
    // Critical work keeps independent capacity and runs ahead of the read burst.
    let critical = store.dispatch("a_turn_boundary_read_flood_waits_then_completes", |b| {
        b.connection
            .execute("UPDATE store_meta SET semantic_mutations=1 WHERE id=1", [])
            .map_err(map_sqlite)?;
        Ok(())
    });
    let started = Instant::now();
    release.send(())?;
    parked.await?;
    critical.await?;
    for read in reads {
        assert_eq!(read.await?.semantic_mutations(), 1);
    }
    eprintln!(
        "read-burst: reads={READS} capacity={DEFAULT_QUEUE_CAPACITY} busy=0 elapsed_us={}",
        started.elapsed().as_micros()
    );
    Ok(())
}

#[test]
fn slow_actor_jobs_warn_with_content_free_labels_and_depths() -> TestResult {
    let warnings = Warnings::of(ACTOR_TARGET);
    let activity = ActorActivity::default();
    let started = Instant::now()
        .checked_sub(SLOW_JOB_THRESHOLD)
        .ok_or("monotonic clock")?;
    tracing::subscriber::with_default(warnings.clone(), || {
        activity.finish("read", "load_branch", started);
    });
    let logged = warnings.lines();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].contains("job=\"load_branch\"")
            && logged[0].contains("elapsed_ms=")
            && logged[0].contains("critical_depth=0")
            && logged[0].contains("read_depth=0"),
        "{logged:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "opt-in large synthetic turn-boundary measurement"]
#[allow(
    clippy::too_many_lines,
    reason = "the opt-in measurement keeps fixture setup, timed commit and warm/cold probes together"
)]
async fn measure_large_session_turn_boundary_reads() -> TestResult {
    use crate::integrity_fixture as fixture;
    const READS: usize = DEFAULT_QUEUE_CAPACITY * 4;
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let request = seed()?;
    let mut prior = request.initial().clone();
    let created = store.create(request).await?;
    let history_len =
        std::env::var("PLAK_MEASURE_HISTORY").map_or(Ok(3098), |value| value.parse::<u32>())?;
    let payload_len =
        std::env::var("PLAK_MEASURE_PAYLOAD").map_or(Ok(7600), |value| value.parse::<usize>())?;
    let generations =
        std::env::var("PLAK_MEASURE_GENERATIONS").map_or(Ok(1), |value| value.parse::<u64>())?;
    assert!(generations > 0);
    let mut head = created.branch();
    let mut commit_us = 0;
    for generation in 1..=generations {
        let payload = if generations == 1 {
            "x".repeat(payload_len)
        } else {
            format!("generation {generation}:{}", "x".repeat(payload_len))
        };
        let checkpoint = fixture::checkpoint_with_payload(
            fixture::topology(
                head.session().session_id(),
                head.session().branch_id(),
                fixture::id(91_010 + u128::from(generation))?,
                Some(prior.checkpoint_id()),
                generation,
                None,
                created.lease().fence().get(),
            )?,
            history_len,
            RuntimeDurability::Required,
            prior
                .validated_checkpoint()?
                .manifest()
                .canonical()
                .context()
                .clone(),
            &payload,
        )?;
        let bundle = fixture::bundle(&checkpoint, Some(&prior))?;
        let commit_started = Instant::now();
        head = store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                head,
                bundle.clone(),
            ))
            .await?
            .head();
        commit_us = commit_started.elapsed().as_micros();
        prior = bundle;
    }
    let bytes = prior.stats().retained_bytes();
    let objects = prior.stats().object_count();
    let before = store
        .dispatch_read("measure_large_session_turn_boundary_reads", |b| {
            Ok(b.checkpoint_decodes)
        })
        .await?;
    let started = Instant::now();
    let warm = store.load_branch(head.session()).await?;
    let current_objects = warm.checkpoint().bundle().stats().object_count();
    let warm_us = started.elapsed().as_micros();
    let after = store
        .dispatch_read("measure_large_session_turn_boundary_reads", |b| {
            Ok(b.checkpoint_decodes)
        })
        .await?;
    assert_eq!(before, after);
    store
        .dispatch("measure_large_session_turn_boundary_reads", |b| {
            b.checkpoints.clear();
            Ok(())
        })
        .await?;
    let started = Instant::now();
    store.load_branch(head.session()).await?;
    let cold_us = started.elapsed().as_micros();
    let cold_decodes = store
        .dispatch_read("measure_large_session_turn_boundary_reads", |b| {
            Ok(b.checkpoint_decodes)
        })
        .await?;
    assert_eq!(cold_decodes, after + 1);
    let (parked, release) = park(&store);
    let mut reads: Vec<_> = (0..READS)
        .map(|_| store.load_branch(head.session()))
        .collect();
    for read in &mut reads {
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    let started = Instant::now();
    release.send(())?;
    parked.await?;
    for read in reads {
        assert_eq!(read.await?.head(), head);
    }
    let burst_us = started.elapsed().as_micros();
    assert_eq!(
        store
            .dispatch_read("measure_large_session_turn_boundary_reads", |b| Ok(
                b.checkpoint_decodes
            ))
            .await?,
        cold_decodes
    );
    eprintln!(
        "large-session: bytes={bytes} objects={objects} current_objects={current_objects} generations={generations} commit_us={commit_us} post_commit_us={warm_us} post_commit_decodes=0 cold_us={cold_us} cold_decodes=1 burst_reads={READS} capacity={DEFAULT_QUEUE_CAPACITY} burst_busy=0 burst_decodes=0 burst_us={burst_us}"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn read_lane_deadline_refuses_a_stalled_actor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open_with_clock_and_options(
        directory.path().join("runtime.sqlite3"),
        Arc::new(SystemLeaseClock::new()?),
        SqliteStoreOptions::new(MemoryStoreLimits::default(), NonZeroUsize::MIN),
    )?;
    let (parked, release) = park(&store);
    let queued = store.stats();
    let mut excess = store.stats();
    assert!(
        excess
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    tokio::time::advance(READ_QUEUE_TIMEOUT).await;
    assert_eq!(excess.await, Err(SessionStoreError::BackendBusy));
    release.send(())?;
    parked.await?;
    queued.await?;
    Ok(())
}

#[tokio::test]
async fn a_waiting_read_observes_actor_exit_as_unavailable() -> TestResult {
    let (sender, receiver) = actor_queue(1);
    sender.send(Lane::Read, Box::new(|_| {}))?;
    let mut waiting = sender.send_read(Box::new(|_| {}));
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(receiver);
    assert_eq!(waiting.await, Err(SessionStoreError::BackendUnavailable));
    Ok(())
}

#[tokio::test]
async fn read_admission_is_fifo_under_saturation() -> TestResult {
    let (sender, receiver) = actor_queue(1);
    sender.send(Lane::Read, Box::new(|_| {}))?;
    let mut first = sender.send_read(Box::new(|_| {}));
    let mut second = sender.send_read(Box::new(|_| {}));
    let mut context = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut context).is_pending());
    assert!(second.as_mut().poll(&mut context).is_pending());
    drop(receiver.read.try_recv()?);
    sender.activity.read_space.add_permits(1);
    let mut newcomer = sender.send_read(Box::new(|_| {}));
    assert!(newcomer.as_mut().poll(&mut context).is_pending());
    assert!(second.as_mut().poll(&mut context).is_pending());
    first.await?;
    drop(receiver.read.try_recv()?);
    sender.activity.read_space.add_permits(1);
    assert!(newcomer.as_mut().poll(&mut context).is_pending());
    second.await?;
    drop(receiver.read.try_recv()?);
    sender.activity.read_space.add_permits(1);
    newcomer.await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_reports_the_explicit_running_label() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let activity = Arc::clone(&store.sender.activity);
    store
        .dispatch("explicit_dispatch_label", move |_| {
            assert_eq!(
                activity
                    .running
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|v| v.0),
                Some("explicit_dispatch_label")
            );
            Ok(())
        })
        .await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn abandoned_read_waiters_release_all_admission_capacity() -> TestResult {
    for timed_out in [false, true] {
        let (sender, receiver) = actor_queue(1);
        sender.send(Lane::Read, Box::new(|_| {}))?;
        let mut waiting = sender.send_read(Box::new(|_| {}));
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        if timed_out {
            tokio::time::advance(READ_QUEUE_TIMEOUT).await;
            assert_eq!(waiting.await, Err(SessionStoreError::BackendBusy));
            drop(receiver.read.try_recv()?);
            sender.activity.read_space.add_permits(1);
        } else {
            // Cancel after the waiter was assigned the newly available permit,
            // but before it can publish its job.
            drop(receiver.read.try_recv()?);
            sender.activity.read_space.add_permits(1);
            drop(waiting);
        }
        assert_eq!(
            sender.activity.read_space.available_permits(),
            sender.capacity
        );
        let mut next = sender.send_read(Box::new(|_| {}));
        assert!(matches!(
            next.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        drop(next);
        drop(receiver.read.try_recv()?);
        sender.activity.read_space.add_permits(1);
        assert_eq!(
            sender.activity.read_space.available_permits(),
            sender.capacity
        );
    }
    Ok(())
}

#[tokio::test]
async fn prune_drops_pending_seed_without_normalizing_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let request = seed()?;
    let prior = request.initial().clone();
    let created = store.create(request).await?;
    let next = next_checkpoint(&created, &prior, 1)?;
    let receipt = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            next.checkpoint().clone(),
        ))
        .await?;
    let old_head = receipt.head();
    let second = next_checkpoint(&created, next.checkpoint(), 2)?;
    let head = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            old_head,
            second.checkpoint().clone(),
        ))
        .await?
        .head();
    store
        .dispatch_read("pending_seed_replaced", move |b| {
            assert!(
                b.checkpoints
                    .get(old_head.checkpoint().checkpoint_id())
                    .is_none()
            );
            assert_eq!(b.checkpoints.normalizations, 0);
            Ok(())
        })
        .await?;
    store
        .dispatch_read("pending_before_prune", move |b| {
            assert!(b.checkpoints.has_pending(head.session()));
            assert_eq!(b.checkpoints.normalizations, 0);
            Ok(())
        })
        .await?;
    store
        .prune(PruneBranch::new(
            created.lease().clone(),
            head,
            BTreeSet::new(),
            1,
        ))
        .await?;
    store
        .dispatch_read("pending_after_prune", move |b| {
            assert!(!b.checkpoints.has_pending(head.session()));
            assert_eq!(b.checkpoints.normalizations, 0);
            assert!(
                b.checkpoints
                    .get(head.checkpoint().checkpoint_id())
                    .is_none()
            );
            Ok(())
        })
        .await?;
    let cold = store.load_branch(head.session()).await?;
    assert_eq!(cold.head(), head);
    Ok(())
}

#[tokio::test]
async fn explicit_reclaim_clears_ready_and_pending_entries() -> TestResult {
    for mode in [StoreReclaimMode::Bounded, StoreReclaimMode::Drain] {
        let directory = tempfile::tempdir()?;
        let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
        let request = seed()?;
        let prior = request.initial().clone();
        let created = store.create(request).await?;
        let next = next_checkpoint(&created, &prior, 1)?;
        store
            .commit(CommitCheckpoint::new(
                created.lease().clone(),
                created.branch(),
                next.checkpoint().clone(),
            ))
            .await?;
        let old = created.branch().checkpoint();
        store.load_checkpoint(old.checkpoint_id()).await?;
        let branch = created.branch().session();
        store
            .dispatch_read("check_ready_and_pending", move |b| {
                assert!(b.checkpoints.has_pending(branch));
                assert!(b.checkpoints.get(old.checkpoint_id()).is_some());
                Ok(())
            })
            .await?;
        store.reclaim(mode).await?;
        store
            .dispatch_read("reclaim_cleared_ready_and_pending", move |b| {
                assert!(!b.checkpoints.has_pending(branch));
                assert!(b.checkpoints.get(old.checkpoint_id()).is_none());
                assert!(
                    b.checkpoints
                        .get(next.checkpoint().checkpoint_id())
                        .is_none()
                );
                Ok(())
            })
            .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "opt-in release-profile small-delta cache-seeding measurement"]
#[allow(
    clippy::too_many_lines,
    reason = "the opt-in paired measurement keeps untimed fixture construction and timed probes together"
)]
async fn measure_lazy_cache_seeding_small_delta_commits() -> TestResult {
    use crate::integrity_fixture as fixture;
    // F1 defaults below; F2: HISTORY=4080, PAYLOAD=64, GENERATIONS=4.
    // Load each published head, as bootstrap and terminal-result paths do.
    let history_len =
        std::env::var("PLAK_MEASURE_HISTORY").map_or(Ok(3098), |value| value.parse::<u32>())?;
    let payload_len =
        std::env::var("PLAK_MEASURE_PAYLOAD").map_or(Ok(7600), |value| value.parse::<usize>())?;
    let generations =
        std::env::var("PLAK_MEASURE_GENERATIONS").map_or(Ok(1), |value| value.parse::<u64>())?;
    for rep in 0..4 {
        let arms = if rep % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        };
        for enabled in arms {
            let directory = tempfile::tempdir()?;
            let store = SqliteSessionStore::open_with_clock(
                directory.path().join("runtime.sqlite3"),
                Arc::new(FixedClock(SystemLeaseClock::new()?.now()?)),
            )?;
            let request = seed()?;
            let mut prior = request.initial().clone();
            let created = store.create(request).await?;
            let mut head = created.branch();
            let mut payload = String::new();
            for generation in 1..=generations {
                payload = if generations == 1 {
                    "x".repeat(payload_len)
                } else {
                    format!("generation {generation}:{}", "x".repeat(payload_len))
                };
                let checkpoint = fixture::checkpoint_with_payload(
                    fixture::topology(
                        head.session().session_id(),
                        head.session().branch_id(),
                        fixture::id(91_010 + u128::from(generation))?,
                        Some(prior.checkpoint_id()),
                        generation,
                        None,
                        created.lease().fence().get(),
                    )?,
                    history_len,
                    RuntimeDurability::Required,
                    prior
                        .validated_checkpoint()?
                        .manifest()
                        .canonical()
                        .context()
                        .clone(),
                    &payload,
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
                prior = bundle;
            }
            eprintln!(
                "f11 fixture rep={rep} enabled={enabled} objects={} bytes={}",
                prior.stats().object_count(),
                prior.stats().retained_bytes()
            );
            store
                .dispatch("measure_seed_arm", move |b| {
                    b.checkpoints.clear();
                    b.checkpoints.seed_enabled = enabled;
                    Ok(())
                })
                .await?;
            store.load_branch(head.session()).await?;
            store
                .dispatch("reset_measure_counters", |b| {
                    b.checkpoint_decodes = 0;
                    b.checkpoints.normalizations = 0;
                    Ok(())
                })
                .await?;
            let mut commits = Vec::new();
            let mut reads = Vec::new();
            for index in 0..40 {
                let revision = generations + index + 1;
                let checkpoint = fixture::checkpoint_with_payload(
                    fixture::topology(
                        head.session().session_id(),
                        head.session().branch_id(),
                        fixture::id(91_010 + u128::from(revision))?,
                        Some(prior.checkpoint_id()),
                        revision,
                        None,
                        created.lease().fence().get(),
                    )?,
                    history_len,
                    RuntimeDurability::Required,
                    prior
                        .validated_checkpoint()?
                        .manifest()
                        .canonical()
                        .context()
                        .clone(),
                    &payload,
                )?;
                let bundle = fixture::bundle(&checkpoint, Some(&prior))?;
                let delta = bundle.supplied_objects().len();
                assert!(delta <= 16, "measurement must use a small object delta");
                let started = Instant::now();
                head = store
                    .commit(CommitCheckpoint::new(
                        created.lease().clone(),
                        head,
                        bundle.clone(),
                    ))
                    .await?
                    .head();
                let commit_us = started.elapsed().as_micros();
                eprintln!(
                    "f11 commit rep={rep} enabled={enabled} index={index} delta={delta} us={commit_us}"
                );
                commits.push(commit_us);
                let started = Instant::now();
                let loaded = store.load_branch(head.session()).await?;
                let read_us = started.elapsed().as_micros();
                reads.push(read_us);
                eprintln!(
                    "f11 post_commit_first rep={rep} enabled={enabled} index={index} us={read_us}"
                );
                assert_eq!(loaded.head(), head);
                prior = bundle;
            }
            commits.sort_unstable();
            reads.sort_unstable();
            eprintln!(
                "f11 summary history={history_len} generations={generations} rep={rep} enabled={enabled} commit_median_us={} commit_p90_us={} first_read_median_us={} first_read_p90_us={}",
                commits[20], commits[35], reads[20], reads[35]
            );
            let started = Instant::now();
            let first = store.load_branch(head.session()).await?;
            eprintln!(
                "f11 first rep={rep} enabled={enabled} us={}",
                started.elapsed().as_micros()
            );
            for index in 0..40 {
                let started = Instant::now();
                let warm = store.load_branch(head.session()).await?;
                let us = started.elapsed().as_micros();
                assert_eq!(warm.checkpoint(), first.checkpoint());
                eprintln!("f11 warm rep={rep} enabled={enabled} index={index} us={us}");
            }
            store
                .dispatch_read("measure_seed_arm_counters", move |b| {
                    assert_eq!(b.checkpoint_decodes, 40 * usize::from(!enabled));
                    assert_eq!(b.checkpoints.normalizations, 40 * usize::from(enabled));
                    Ok(())
                })
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn delete_drops_pending_seed_without_normalizing_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().join("runtime.sqlite3"))?;
    let request = seed()?;
    let prior = request.initial().clone();
    let created = store.create(request).await?;
    let next = next_checkpoint(&created, &prior, 1)?;
    store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            next.checkpoint().clone(),
        ))
        .await?;
    let selector = created.branch().session();
    store
        .dispatch_read("pending_before_delete", move |b| {
            assert!(b.checkpoints.has_pending(selector));
            assert_eq!(b.checkpoints.normalizations, 0);
            Ok(())
        })
        .await?;
    store.release(created.lease().clone()).await?;
    let archived = store
        .archive(ArchiveSession::new(
            created.session().session_id(),
            created.session().revision(),
        ))
        .await?;
    store
        .delete(DeleteSession::new(
            archived.session().session_id(),
            archived.session().revision(),
        ))
        .await?;
    store
        .dispatch_read("pending_after_delete", move |b| {
            assert!(!b.checkpoints.has_pending(selector));
            assert_eq!(b.checkpoints.normalizations, 0);
            Ok(())
        })
        .await?;
    Ok(())
}
