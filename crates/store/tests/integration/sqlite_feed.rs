//! The change feed over `SQLite`: the shared conformance law, bounded trim,
//! reopen, read-only inspection, and full-verification refusals.

use std::error::Error;
use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use plakous_agent::internal::{
    FeedChange, FeedPage, FeedRecord, FeedSeq, LeaseClock, LeaseDomain, LeaseInstant,
    RUNTIME_FORMAT_EPOCH, RuntimeDurability, SessionStore, SessionStoreError,
};
use plakous_model::SessionId;
use plakous_store::{
    MemoryFeedLimits, MemoryStoreLimits, SQLITE_SCHEMA_VERSION, SqliteFeedUsage,
    SqliteSessionStore, SqliteStoreOptions,
};
use rusqlite::Connection;

use crate::conformance::{PruneChain, TestResult, extend_chain, id, prune_chain};
use crate::feed::run_feed_conformance;

const DURABILITY: RuntimeDurability = RuntimeDurability::Required;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Database {
    directory: PathBuf,
    path: PathBuf,
}

impl Database {
    fn new() -> TestResult<Self> {
        let directory = std::env::temp_dir().join(format!(
            "plakous-store-feed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory)?;
        let path = directory.join("runtime.sqlite3");
        Ok(Self { directory, path })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// A lease clock that never advances, so no fixture lease expires.
struct FixedClock;

impl LeaseClock for FixedClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(LeaseDomain::new([7; 16], [9; 32]), 100, 0)
    }
}

fn open_with_feed_limits(
    db: &Database,
    feed: MemoryFeedLimits,
) -> Result<SqliteSessionStore, Box<dyn Error>> {
    Ok(SqliteSessionStore::open_with_clock_and_options(
        &db.path,
        Arc::new(FixedClock),
        SqliteStoreOptions::new(
            MemoryStoreLimits::default().with_feed_limits(feed),
            NonZeroUsize::new(8).ok_or("nonzero queue")?,
        ),
    )?)
}

fn page(limit: usize) -> TestResult<NonZeroUsize> {
    Ok(NonZeroUsize::new(limit).ok_or("nonzero page")?)
}

async fn all_records(
    store: &dyn SessionStore,
    session_id: SessionId,
    after: u64,
) -> TestResult<Vec<FeedRecord>> {
    match store
        .read_feed(session_id, FeedSeq::new(after), page(256)?)
        .await?
    {
        FeedPage::Records { records, .. } => Ok(records),
        FeedPage::BehindRetention { .. } => Err("cursor fell behind retention".into()),
    }
}

fn encoded_bytes(records: &[FeedRecord]) -> TestResult<u64> {
    let mut total = 0_u64;
    for record in records {
        total += u64::try_from(record.encode()?.len())?;
    }
    Ok(total)
}

/// A session with a genesis and three commits, its store shut down.
async fn seeded(db: &Database, base: u128) -> TestResult<(PruneChain, Vec<FeedRecord>)> {
    let store = SqliteSessionStore::open(&db.path)?;
    let chain = prune_chain(&store, DURABILITY, base, 3, 64).await?;
    let session_id = chain.created.branch().session().session_id();
    let records = all_records(&store, session_id, 0).await?;
    store.shutdown().await?;
    drop(store);
    Ok((chain, records))
}

#[tokio::test]
async fn sqlite_feed_conformance() -> TestResult {
    let db = Database::new()?;
    let store = SqliteSessionStore::open(&db.path)?;
    run_feed_conformance(&store, DURABILITY).await?;
    store.shutdown().await?;
    drop(store);
    SqliteSessionStore::open_verified(&db.path)?;
    Ok(())
}

/// Records, their sequences and the receipts naming them survive reopen and
/// full verification, and the read-only inspection reads the same page.
#[tokio::test]
async fn sqlite_feed_survives_reopen_and_reads_read_only() -> TestResult {
    let db = Database::new()?;
    let (chain, records) = seeded(&db, 1_000).await?;
    let session_id = chain.created.branch().session().session_id();
    assert_eq!(
        records
            .iter()
            .map(|record| record.header().seq.get())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );

    let inspected =
        SqliteSessionStore::inspect_feed(&db.path, session_id, FeedSeq::ZERO, page(256)?)?;
    let FeedPage::Records {
        records: read,
        window,
    } = inspected
    else {
        return Err("inspection fell behind retention".into());
    };
    assert_eq!(read, records);
    assert_eq!((window.retained_from().get(), window.tail().get()), (1, 4));
    assert_eq!(
        SqliteSessionStore::inspect_feed(&db.path, id(1_999)?, FeedSeq::ZERO, page(1)?),
        Err(SessionStoreError::NotFound)
    );
    assert_eq!(
        SqliteSessionStore::inspect_feed_usage(&db.path)?,
        Some(SqliteFeedUsage {
            records: 4,
            bytes: encoded_bytes(&records)?,
        })
    );

    let reopened = SqliteSessionStore::open_verified(&db.path)?;
    assert_eq!(all_records(&reopened, session_id, 0).await?, records);
    assert_eq!(
        reopened
            .recover_commit(chain.ids[2])
            .await?
            .receipt()
            .feed_seq(),
        FeedSeq::new(3)
    );
    assert_eq!(
        reopened.commit(chain.commits[0].clone()).await?.feed_seq(),
        FeedSeq::new(2),
        "an exact retry returns the recorded sequence"
    );
    assert_eq!(
        reopened.feed_tail(session_id).await?.tail(),
        FeedSeq::new(4)
    );
    Ok(())
}

/// Retention trims a prefix in bounded batches after the publishing commit;
/// a trimmed store reopens under full verification.
#[tokio::test]
async fn sqlite_feed_trims_the_oldest_records_in_batches() -> TestResult {
    let db = Database::new()?;
    let store = open_with_feed_limits(&db, MemoryFeedLimits::new(16, u64::MAX, page(2)?))?;
    let mut chain = prune_chain(&store, DURABILITY, 800, 15, 8).await?;
    let session_id = chain.created.branch().session().session_id();
    let window = store.feed_tail(session_id).await?;
    assert_eq!((window.retained_from().get(), window.tail().get()), (1, 16));

    chain.ids.push(id(800 + 100 + 16)?);
    let chain = extend_chain(&store, DURABILITY, chain, 16, 8).await?;
    let window = store.feed_tail(session_id).await?;
    assert_eq!((window.retained_from().get(), window.tail().get()), (4, 17));
    match store
        .read_feed(session_id, FeedSeq::new(2), page(8)?)
        .await?
    {
        FeedPage::BehindRetention { window, heads } => {
            assert_eq!(window.retained_from(), FeedSeq::new(4));
            assert_eq!(heads, [chain.head]);
        }
        FeedPage::Records { .. } => return Err("a trimmed cursor read records".into()),
    }
    let retained = all_records(&store, session_id, 3).await?;
    assert_eq!(
        retained
            .iter()
            .map(|record| record.header().seq.get())
            .collect::<Vec<_>>(),
        (4..=17).collect::<Vec<_>>()
    );
    store.shutdown().await?;
    drop(store);

    let reopened = SqliteSessionStore::open_verified(&db.path)?;
    assert_eq!(all_records(&reopened, session_id, 3).await?, retained);
    assert_eq!(
        SqliteSessionStore::inspect_feed_usage(&db.path)?,
        Some(SqliteFeedUsage {
            records: 14,
            bytes: encoded_bytes(&retained)?,
        })
    );
    Ok(())
}

/// The byte bound trims independently of the record bound.
#[tokio::test]
async fn sqlite_feed_trims_by_encoded_bytes() -> TestResult {
    let db = Database::new()?;
    let store = open_with_feed_limits(&db, MemoryFeedLimits::new(u64::MAX, 2_048, page(4_096)?))?;
    let chain = prune_chain(&store, DURABILITY, 900, 12, 8).await?;
    let session_id = chain.created.branch().session().session_id();
    let window = store.feed_tail(session_id).await?;
    assert_eq!(window.tail().get(), 13);
    assert!(window.retained_from().get() > 1);
    let retained = all_records(&store, session_id, window.retained_from().get() - 1).await?;
    assert!(encoded_bytes(&retained)? <= 2_048);
    Ok(())
}

/// Full verification refuses a gap, a sequence above `feed_tail`, a row that
/// disagrees with its seal or header, wrong byte counters, and a commit
/// receipt whose record is missing at or above `feed_retained_from`.
#[tokio::test]
async fn sqlite_feed_corruption_fails_full_verification() -> TestResult {
    for corruption in [
        "DELETE FROM session_feed WHERE seq=2",
        "UPDATE sessions SET feed_tail=feed_tail+1",
        "UPDATE session_feed SET change=5,checkpoint_id=NULL WHERE seq=3",
        "UPDATE session_feed SET integrity=zeroblob(32) WHERE seq=1",
        "UPDATE sessions SET feed_bytes=feed_bytes+1",
        "UPDATE commit_receipts SET feed_seq=1 WHERE feed_seq=2",
        "UPDATE sessions SET feed_bytes=feed_bytes-(SELECT length(record) FROM session_feed
             WHERE seq=(SELECT max(seq) FROM session_feed)),feed_tail=feed_tail-1;
         DELETE FROM session_feed WHERE seq=(SELECT max(seq) FROM session_feed);",
    ] {
        let db = Database::new()?;
        seeded(&db, 1_100).await?;
        let connection = Connection::open(&db.path)?;
        connection.execute_batch(corruption)?;
        drop(connection);
        assert!(
            matches!(
                SqliteSessionStore::open_verified(&db.path),
                Err(SessionStoreError::Corrupt(_))
            ),
            "full verification admitted feed corruption: {corruption}"
        );
    }
    Ok(())
}

/// Archive and deletion: the archive record names the initial branch, and
/// deletion removes every feed row with the session.
#[tokio::test]
async fn sqlite_feed_rows_are_deleted_with_their_session() -> TestResult {
    let db = Database::new()?;
    let store = SqliteSessionStore::open(&db.path)?;
    let chain = prune_chain(&store, DURABILITY, 1_200, 2, 8).await?;
    let selector = chain.created.branch().session();
    store.release(chain.created.lease().clone()).await?;
    let revision = store.load_branch(selector).await?.session().revision();
    let archived = store
        .archive(plakous_agent::internal::ArchiveSession::new(
            selector.session_id(),
            revision,
        ))
        .await?;
    let records = all_records(&store, selector.session_id(), 0).await?;
    let last = records.last().ok_or("archive record")?;
    assert_eq!(
        (last.header().seq.get(), last.header().branch_id),
        (5, selector.branch_id())
    );
    assert_eq!(
        last.header().change,
        FeedChange::Archived {
            revision: archived.session().revision()
        }
    );
    assert!(matches!(
        records[3].header().change,
        FeedChange::LeaseReleased { .. }
    ));
    store
        .delete(plakous_agent::internal::DeleteSession::new(
            selector.session_id(),
            archived.session().revision(),
        ))
        .await?;
    assert_eq!(
        store
            .read_feed(selector.session_id(), FeedSeq::ZERO, page(1)?)
            .await,
        Err(SessionStoreError::NotFound)
    );
    store.shutdown().await?;
    drop(store);
    let rows: i64 =
        Connection::open(&db.path)?
            .query_row("SELECT count(*) FROM session_feed", [], |row| row.get(0))?;
    assert_eq!(rows, 0);
    SqliteSessionStore::open_verified(&db.path)?;
    Ok(())
}

/// Rewrites a populated store as a build at `version` and format `epoch`
/// wrote it: the object table's codec CHECK and every object address.
fn rewrite_as_build(db: &Database, version: i64, epoch: u32) -> TestResult {
    let connection = Connection::open(&db.path)?;
    connection.execute_batch(&format!(
        "PRAGMA writable_schema=ON;
         UPDATE sqlite_schema
         SET sql=replace(sql,'CHECK(codec={RUNTIME_FORMAT_EPOCH})','CHECK(codec={epoch})')
         WHERE type='table' AND name='runtime_objects';
         PRAGMA writable_schema=OFF;"
    ))?;
    drop(connection);
    // A fresh connection loads the rewritten DDL before the rows move.
    let connection = Connection::open(&db.path)?;
    let ddl: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='runtime_objects'",
        [],
        |row| row.get(0),
    )?;
    assert!(ddl.contains(&format!("CHECK(codec={epoch})")), "{ddl}");
    connection.execute_batch(&format!(
        "PRAGMA foreign_keys=OFF;
         UPDATE runtime_objects SET codec={epoch};
         UPDATE runtime_object_edges SET parent_codec={epoch},child_codec={epoch};
         UPDATE checkpoints SET root_codec={epoch},
           base_codec=CASE WHEN base_codec IS NULL THEN NULL ELSE {epoch} END;"
    ))?;
    connection.pragma_update(None, "user_version", version)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

/// A runtime database a previous build wrote is an incompatible build, never
/// damage: the previous schema at the previous epoch refuses as
/// `UnsupportedSchema`, and this schema at another epoch as
/// `UnsupportedCodec`, through every open and read-only inspection, without
/// changing its bytes.
#[tokio::test]
async fn sqlite_previous_build_stores_refuse_as_incompatible_builds() -> TestResult {
    for (version, epoch, refusal) in [
        (
            SQLITE_SCHEMA_VERSION - 1,
            RUNTIME_FORMAT_EPOCH - 1,
            SessionStoreError::UnsupportedSchema,
        ),
        (
            SQLITE_SCHEMA_VERSION,
            RUNTIME_FORMAT_EPOCH - 1,
            SessionStoreError::UnsupportedCodec,
        ),
        (
            SQLITE_SCHEMA_VERSION,
            RUNTIME_FORMAT_EPOCH + 1,
            SessionStoreError::UnsupportedCodec,
        ),
    ] {
        let db = Database::new()?;
        let (chain, _) = seeded(&db, 1_300).await?;
        let selector = chain.created.branch().session();
        rewrite_as_build(&db, version, epoch)?;
        let before = fs::read(&db.path)?;
        let case = format!("schema {version}, epoch {epoch}");

        assert_eq!(
            SqliteSessionStore::classify_persistence(&db.path),
            Err(refusal),
            "{case}"
        );
        assert_eq!(
            SqliteSessionStore::open(&db.path).err(),
            Some(refusal),
            "{case}"
        );
        assert_eq!(
            SqliteSessionStore::open_verified(&db.path).err(),
            Some(refusal),
            "{case}"
        );
        assert_eq!(
            SqliteSessionStore::inspect_feed(
                &db.path,
                selector.session_id(),
                FeedSeq::ZERO,
                page(1)?
            )
            .err(),
            Some(refusal),
            "{case}"
        );
        assert_eq!(
            SqliteSessionStore::inspect_feed_usage(&db.path).err(),
            Some(refusal),
            "{case}"
        );
        assert_eq!(
            SqliteSessionStore::inspect_branch(&db.path, selector).err(),
            Some(refusal),
            "{case}"
        );
        assert_eq!(fs::read(&db.path)?, before, "{case} was modified");
    }
    Ok(())
}
