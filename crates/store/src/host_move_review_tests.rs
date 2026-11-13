//! Continued-line membership and receipt gates under both store authorities.
use crate::integrity_fixture::{self as fixture, TestResult};
use plakous_agent::internal::host_move::{MoveDisposition, PrepareMove};
use plakous_agent::internal::{
    BranchLease, CreateSession, LineContinuation, RecoverFork, RuntimeDurability, SessionQuery,
    SessionRef, SessionStore, SessionStoreError, SessionWorkflowId, Timestamp,
};
use std::num::{NonZeroU64, NonZeroUsize};

async fn continued(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult<(BranchLease, BranchLease)> {
    let predecessor = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            predecessor.session_id(),
            predecessor.branch_id(),
            fixture::id(3)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        durability,
    )?;
    let created = store
        .create(CreateSession::new(
            predecessor,
            Timestamp::new(1, 0)?,
            fixture::id(4)?,
            NonZeroU64::new(300).ok_or("lease")?,
            fixture::bundle(&checkpoint, None)?,
        ))
        .await?;
    let target = SessionRef::new(fixture::id(11)?, fixture::id(12)?);
    let mut topology = fixture::topology(
        target.session_id(),
        target.branch_id(),
        fixture::id(13)?,
        None,
        0,
        None,
        1,
    )?;
    topology.line = checkpoint.manifest().topology().line;
    topology.line_generation = 2;
    let carried = fixture::checkpoint(topology, 1, durability)?;
    let request = CreateSession::new(
        target,
        Timestamp::new(2, 0)?,
        fixture::id(14)?,
        NonZeroU64::new(300).ok_or("lease")?,
        fixture::bundle(&carried, None)?,
    )
    .continuing_line(LineContinuation::new(
        created.lease().clone(),
        created.branch(),
        Vec::new(),
    ));
    let carried = store.create(request).await?;
    Ok((created.lease().clone(), carried.lease().clone()))
}

async fn continued_recovery(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    disposition: MoveDisposition,
) -> TestResult {
    let (predecessor, head) = continued(store, durability).await?;
    let operation = SessionWorkflowId::new([112; 32])?;
    // The predecessor still holds its historical lease, but is not a batch
    // member. Only the current head's session is owned by this move.
    let install = store
        .prepare_move(PrepareMove {
            operation,
            leases: vec![head.clone()],
        })
        .await?;
    assert_eq!(install.sessions().len(), 1);
    assert_eq!(
        install.sessions()[0].session.session_id(),
        head.session().session_id()
    );
    assert_ne!(
        install.sessions()[0].session.session_id(),
        predecessor.session().session_id()
    );
    let recovered = store.recover_move(operation).await?;
    assert_eq!(recovered.digest(), install.digest());
    assert_eq!(recovered.sessions().len(), 1);
    let listed = store
        .list(SessionQuery::new(NonZeroUsize::new(8).ok_or("limit")?).include_archived(true))
        .await?;
    assert!(
        listed
            .items()
            .iter()
            .all(|row| row.head().session_id() != head.session().session_id())
    );
    store
        .finish_move(operation, recovered.digest(), disposition)
        .await?;
    let branch = store.load_branch(head.session()).await;
    match disposition {
        MoveDisposition::Commit => {
            assert_eq!(branch.err(), Some(SessionStoreError::OwnershipMoved));
        }
        MoveDisposition::Abort => assert!(branch.is_ok()),
        _ => return Err("source disposition".into()),
    }
    Ok(())
}

#[tokio::test]
async fn host_move_memory_continued_head_recovers_exact_members_for_commit_and_abort() -> TestResult
{
    for disposition in [MoveDisposition::Commit, MoveDisposition::Abort] {
        let store = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
        continued_recovery(&store, RuntimeDurability::Disabled, disposition).await?;
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_continued_head_recovers_exact_members_for_commit_and_abort() -> TestResult
{
    for disposition in [MoveDisposition::Commit, MoveDisposition::Abort] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("runtime.sqlite3");
        let store = crate::SqliteSessionStore::open(&path)?;
        continued_recovery(&store, RuntimeDurability::Required, disposition).await?;
        store.shutdown().await?;
        drop(store);
        let reopened = crate::SqliteSessionStore::open_verified(&path)?;
        reopened.shutdown().await?;
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_commit_clears_all_line_leases_and_host_claims_including_predecessor()
-> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("runtime.sqlite3");
    let store = crate::SqliteSessionStore::open(&path)?;
    let (predecessor, head) = continued(&store, RuntimeDurability::Required).await?;
    let claims = path.with_file_name("runtime.sqlite3-holders");
    let claimed = |lease: &BranchLease| {
        claims.join(format!(
            "{}.{}.holder",
            lease.session().session_id(),
            lease.session().branch_id()
        ))
    };
    assert!(claimed(&head).exists());
    assert!(claimed(&predecessor).exists());
    let operation = SessionWorkflowId::new([113; 32])?;
    let install = store
        .prepare_move(PrepareMove {
            operation,
            leases: vec![head.clone()],
        })
        .await?;
    store
        .finish_move(operation, install.digest(), MoveDisposition::Commit)
        .await?;
    for lease in [&head, &predecessor] {
        assert!(!claimed(lease).exists());
    }
    let connection = rusqlite::Connection::open(&path)?;
    let outstanding:i64=connection.query_row("SELECT count(*) FROM branches WHERE active_owner IS NOT NULL OR active_fence IS NOT NULL OR active_time IS NOT NULL OR active_ns IS NOT NULL OR active_duration IS NOT NULL OR previous_time IS NOT NULL OR previous_ns IS NOT NULL OR released_owner IS NOT NULL OR released_fence IS NOT NULL OR released_time IS NOT NULL OR released_ns IS NOT NULL OR released_duration IS NOT NULL",[],|row|row.get(0))?;
    assert_eq!(outstanding, 0);
    drop(connection);
    store.shutdown().await?;
    Ok(())
}

struct ReceiptFixture {
    create: CreateSession,
    recover: RecoverFork,
    leases: Vec<BranchLease>,
}

async fn receipt_fixture(
    source: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult<ReceiptFixture> {
    use plakous_agent::internal::{Digest, DigestDomain, ForkBranch, ForkLineage};
    let selector = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            selector.session_id(),
            selector.branch_id(),
            fixture::id(3)?,
            None,
            0,
            None,
            1,
        )?,
        1,
        durability,
    )?;
    let create = CreateSession::new(
        selector,
        Timestamp::new(1, 0)?,
        fixture::id(4)?,
        NonZeroU64::new(300).ok_or("lease")?,
        fixture::bundle(&checkpoint, None)?,
    );
    let created = source.create(create.clone()).await?;
    let loaded = source.load_branch(selector).await?;
    let branch = fixture::id(22)?;
    let workflow = SessionWorkflowId::new([115; 32])?;
    let semantic = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"fork"])?;
    let context = checkpoint.manifest().canonical().context().ordinary_fork(
        checkpoint.id(),
        selector.session_id(),
        branch,
        [115; 32],
    )?;
    let topology = fixture::topology(
        selector.session_id(),
        branch,
        fixture::id(23)?,
        Some(checkpoint.id()),
        0,
        Some(ForkLineage {
            session_id: selector.session_id(),
            branch_id: selector.branch_id(),
            checkpoint_id: checkpoint.id(),
            branch_revision: 0,
        }),
        1,
    )?;
    let forked = fixture::checkpoint_with_context(topology, 1, durability, context)?;
    let fork = ForkBranch::new(
        workflow,
        semantic,
        created.lease().clone(),
        loaded.session().revision(),
        loaded.head(),
        branch,
        fixture::id(24)?,
        NonZeroU64::new(300).ok_or("lease")?,
        fixture::bundle(&forked, Some(create.initial()))?,
    );
    let receipt = source.fork(fork).await?;
    let recover = RecoverFork::new(
        workflow,
        semantic,
        created.lease().clone(),
        loaded.head(),
        branch,
    );
    assert!(source.recover_create(create.clone()).await.is_ok());
    assert!(source.recover_fork(recover.clone()).await.is_ok());
    Ok(ReceiptFixture {
        create,
        recover,
        leases: vec![created.lease().clone(), receipt.lease().clone()],
    })
}

async fn hidden_receipts(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult {
    let ReceiptFixture {
        create,
        recover,
        leases,
    } = receipt_fixture(source, durability).await?;
    let operation = SessionWorkflowId::new([116; 32])?;
    let install = source
        .prepare_move(PrepareMove { operation, leases })
        .await?;
    target.stage_move(operation, install.clone()).await?;
    for store in [source, target] {
        assert_eq!(
            store.recover_create(create.clone()).await.err(),
            Some(SessionStoreError::NotLineHead)
        );
        assert_eq!(
            store.recover_fork(recover.clone()).await.err(),
            Some(SessionStoreError::NotLineHead)
        );
    }
    assert_eq!(
        target
            .list(SessionQuery::new(NonZeroUsize::new(8).ok_or("limit")?))
            .await?
            .items(),
        []
    );
    target
        .finish_move(operation, install.digest(), MoveDisposition::Abandon)
        .await?;
    assert_eq!(
        target
            .list(SessionQuery::new(NonZeroUsize::new(8).ok_or("limit")?).include_archived(true))
            .await?
            .items(),
        []
    );
    source
        .finish_move(operation, install.digest(), MoveDisposition::Commit)
        .await?;
    assert_eq!(
        source.recover_create(create).await.err(),
        Some(SessionStoreError::OwnershipMoved)
    );
    Ok(())
}

#[tokio::test]
async fn host_move_memory_recovery_receipts_never_open_hidden_source_or_target() -> TestResult {
    hidden_receipts(
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        RuntimeDurability::Disabled,
    )
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_recovery_receipts_never_open_hidden_source_or_target() -> TestResult {
    let root = tempfile::tempdir()?;
    let source = crate::SqliteSessionStore::open(root.path().join("source.sqlite3"))?;
    let target = crate::SqliteSessionStore::open(root.path().join("target.sqlite3"))?;
    hidden_receipts(&source, &target, RuntimeDurability::Required).await?;
    source.shutdown().await?;
    target.shutdown().await?;
    Ok(())
}
