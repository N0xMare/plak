//! Imported cursors consume no old admission and skip no destination fact.
use crate::integrity_fixture::{self as fixture, TestResult};
use plakous_agent::internal::host_move::{MoveDisposition, PrepareMove};
use plakous_agent::internal::{
    AcquireBranch, CreateSession, FeedFact, FeedPage, FeedSeq, RuntimeDurability, SessionRef,
    SessionStore, SessionWorkflowId, Timestamp,
};
use std::num::{NonZeroU64, NonZeroUsize};

async fn feed_cut(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult {
    let session = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            session.session_id(),
            session.branch_id(),
            fixture::id(3)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        durability,
    )?;
    let created = source
        .create(
            CreateSession::new(
                session,
                Timestamp::new(1, 0)?,
                fixture::id(4)?,
                NonZeroU64::MIN,
                fixture::bundle(&checkpoint, None)?,
            )
            .with_feed_facts(vec![FeedFact::SessionCreated {
                operation_id: "source-admission".into(),
                seeded: false,
            }]),
        )
        .await?;
    let floor = source.feed_tail(session.session_id()).await?.tail().get();
    assert!(floor > 0);
    let operation = SessionWorkflowId::new([121; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            job_cut: plakous_agent::internal::host_move::MoveJobCut::Settled,
            operation,
            leases: vec![created.lease().clone()],
        })
        .await?;
    assert_eq!(install.sessions()[0].feed_tail_floor, floor);
    assert_eq!(
        source.recover_move(operation).await?.digest(),
        install.digest()
    );
    target.stage_move(operation, install.clone()).await?;
    assert_eq!(
        target.recover_move(operation).await?.digest(),
        install.digest()
    );
    let window = target.feed_tail(session.session_id()).await?;
    assert_eq!(window.tail().get(), floor);
    assert_eq!(window.retained_from().get(), floor + 1);
    source
        .finish_move(operation, install.digest(), MoveDisposition::Commit)
        .await?;
    target
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    assert!(matches!(
        target
            .read_feed(session.session_id(), FeedSeq::new(0), NonZeroUsize::MIN)
            .await?,
        FeedPage::BehindRetention { .. }
    ));
    let lease = target
        .acquire(AcquireBranch::new(
            session,
            fixture::id(5)?,
            NonZeroU64::MIN,
        ))
        .await?;
    target.release(lease).await?;
    let FeedPage::Records { records, window } = target
        .read_feed(
            session.session_id(),
            FeedSeq::new(floor),
            NonZeroUsize::new(10).ok_or("page")?,
        )
        .await?
    else {
        return Err("import cursor lost its new facts".into());
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].header().seq.get(), floor + 1);
    assert_eq!(window.tail().get(), floor + 1);
    assert!(
        records
            .iter()
            .all(|record| record.facts().iter().all(|fact| !matches!(
                fact,
                FeedFact::SessionCreated { .. } | FeedFact::CommandAccepted { .. }
            )))
    );
    Ok(())
}

#[tokio::test]
async fn host_move_memory_retains_the_feed_floor_without_replaying_source_admissions() -> TestResult
{
    feed_cut(
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        RuntimeDurability::Disabled,
    )
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_retains_the_feed_floor_and_reopens_the_new_facts() -> TestResult {
    let root = tempfile::tempdir()?;
    let source = crate::SqliteSessionStore::open(root.path().join("source.sqlite3"))?;
    let path = root.path().join("target.sqlite3");
    let target = crate::SqliteSessionStore::open(&path)?;
    feed_cut(&source, &target, RuntimeDurability::Required).await?;
    let before = target.feed_tail(fixture::id(1)?).await?;
    source.shutdown().await?;
    target.shutdown().await?;
    drop(target);
    let target = crate::SqliteSessionStore::open_verified(&path)?;
    assert_eq!(target.feed_tail(fixture::id(1)?).await?, before);
    target.shutdown().await?;
    Ok(())
}
