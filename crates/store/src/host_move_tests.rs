//! Retirement, staged visibility and imported floors under both store authorities.
use crate::integrity_fixture::{self as fixture, TestResult};
use plakous_agent::internal::host_move::{MoveDisposition, MoveInstall, PrepareMove};
use plakous_agent::internal::{
    AcquireBranch, BranchLease, CreateSession, RuntimeDurability, SessionRef, SessionStore,
    SessionStoreError, SessionWorkflowId, Timestamp,
};
use std::num::NonZeroU64;
use std::sync::Arc;

mod guards;
mod jobs;
#[cfg(all(unix, feature = "sqlite"))]
mod preflight;
mod recovery;

async fn seed(store: &dyn SessionStore, durability: RuntimeDurability) -> TestResult<BranchLease> {
    seed_at(store, durability, 0).await
}

async fn seed_at(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
) -> TestResult<BranchLease> {
    let selector = SessionRef::new(fixture::id(base + 1)?, fixture::id(base + 2)?);
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            selector.session_id(),
            selector.branch_id(),
            fixture::id(base + 3)?,
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
            selector,
            Timestamp::new(1, 0)?,
            fixture::id(base + 4)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
            fixture::bundle(&checkpoint, None)?,
        ))
        .await?;
    let mut lease = created.lease().clone();
    for _ in 0..3 {
        store.release(lease.clone()).await?;
        lease = store
            .acquire(AcquireBranch::new(
                selector,
                fixture::id(base + 4)?,
                NonZeroU64::new(300).ok_or("nonzero")?,
            ))
            .await?;
    }
    Ok(lease)
}

async fn authority_contract(
    source: &dyn SessionStore,
    destination: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult {
    let lease = seed(source, durability).await?;
    let operation = SessionWorkflowId::new([41; 32])?;
    let prepare = PrepareMove {
        operation,
        leases: vec![lease.clone()],
    };
    let install = source.prepare_move(prepare.clone()).await?;
    assert_eq!(
        source
            .prepare_move(PrepareMove {
                operation: SessionWorkflowId::new([46; 32])?,
                leases: vec![lease.clone()]
            })
            .await
            .err(),
        Some(SessionStoreError::NotLineHead),
        "another operation cannot steal a prepared line"
    );
    let line = install.lines()[0].line.line();
    assert_eq!(
        source.prepare_move(prepare).await?.digest(),
        install.digest()
    );
    assert_eq!(
        source.load_line(line).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        source.load_branch(lease.session()).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        source.renew(lease.clone()).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        source.revive(lease.clone()).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        source
            .acquire(AcquireBranch::new(
                lease.session(),
                fixture::id(9)?,
                NonZeroU64::MIN
            ))
            .await
            .err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert!(
        source
            .finish_move(
                SessionWorkflowId::new([42; 32])?,
                install.digest(),
                MoveDisposition::Abort
            )
            .await
            .is_err()
    );
    source
        .finish_move(operation, install.digest(), MoveDisposition::Abort)
        .await?;
    assert_eq!(source.load_line(line).await?.line(), line);
    assert!(
        source
            .prepare_move(PrepareMove {
                operation,
                leases: vec![lease.clone()]
            })
            .await
            .is_err()
    );
    let operation = SessionWorkflowId::new([43; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            operation,
            leases: vec![lease.clone()],
        })
        .await?;
    source
        .finish_move(operation, install.digest(), MoveDisposition::Commit)
        .await?;
    assert!(
        source
            .finish_move(operation, install.digest(), MoveDisposition::Abort)
            .await
            .is_err()
    );
    stage_contract(destination, operation, install, &lease).await?;
    assert_eq!(
        source.load_line(line).await.err(),
        Some(SessionStoreError::OwnershipMoved)
    );
    Ok(())
}

async fn stage_contract(
    destination: &dyn SessionStore,
    operation: SessionWorkflowId,
    install: MoveInstall,
    lease: &BranchLease,
) -> TestResult {
    let line = install.lines()[0].line.line();
    destination.stage_move(operation, install.clone()).await?;
    destination.stage_move(operation, install.clone()).await?;
    assert_eq!(
        destination.load_line(line).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        destination.load_branch(lease.session()).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        destination
            .acquire(AcquireBranch::new(
                lease.session(),
                fixture::id(10)?,
                NonZeroU64::MIN
            ))
            .await
            .err(),
        Some(SessionStoreError::NotLineHead)
    );
    let checkpoint = install.sessions()[0].branches[0].checkpoint;
    assert_eq!(
        destination
            .load_checkpoint(checkpoint)
            .await?
            .checkpoint()
            .id(),
        checkpoint,
        "historical inspection is not execution"
    );
    destination
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    assert_eq!(
        destination.load_line(line).await?.generation(),
        install.lines()[0].line.generation()
    );
    let acquired = destination
        .acquire(AcquireBranch::new(
            lease.session(),
            fixture::id(10)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
        ))
        .await?;
    assert!(acquired.fence() > lease.fence());
    assert_ne!(acquired.owner(), lease.owner());
    assert!(
        destination
            .finish_move(operation, install.digest(), MoveDisposition::Abandon)
            .await
            .is_err(),
        "published ownership cannot be abandoned, even while its owner holds a lease"
    );
    let renewed = destination.renew(acquired).await?;
    destination.release(renewed).await?;
    assert!(
        destination
            .stage_move(SessionWorkflowId::new([44; 32])?, install)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn host_move_memory_authority_contract() -> TestResult {
    authority_contract(
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        RuntimeDurability::Disabled,
    )
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_authority_contract_and_permanent_restart_gates() -> TestResult {
    let temp = tempfile::tempdir()?;
    let source_path = temp.path().join("source.sqlite3");
    let destination_path = temp.path().join("destination.sqlite3");
    let source = crate::SqliteSessionStore::open(&source_path)?;
    let destination = crate::SqliteSessionStore::open(&destination_path)?;
    authority_contract(&source, &destination, RuntimeDurability::Required).await?;
    source.shutdown().await?;
    destination.shutdown().await?;
    drop(source);
    drop(destination);
    for path in [&source_path, &destination_path] {
        let reopened = crate::SqliteSessionStore::open_verified(path)?;
        let line = fixture::id(2)?;
        if path == &destination_path {
            assert!(reopened.load_line(line).await.is_ok());
            reopened.shutdown().await?;
            continue;
        }
        assert_eq!(
            reopened.load_line(line).await.err(),
            Some(SessionStoreError::OwnershipMoved)
        );
        let selector = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
        assert_eq!(
            reopened
                .acquire(AcquireBranch::new(
                    selector,
                    fixture::id(15)?,
                    NonZeroU64::MIN
                ))
                .await
                .err(),
            Some(SessionStoreError::OwnershipMoved)
        );
        reopened.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn host_move_rejects_missing_holds_conflicts_and_overflow() -> TestResult {
    let source = Arc::new(crate::MemorySessionStore::new(
        crate::MemoryStoreLimits::default(),
    )?);
    let lease = seed(source.as_ref(), RuntimeDurability::Disabled).await?;
    let operation = SessionWorkflowId::new([41; 32])?;
    assert!(
        source
            .prepare_move(PrepareMove {
                operation,
                leases: vec![]
            })
            .await
            .is_err()
    );
    let install = source
        .prepare_move(PrepareMove {
            operation,
            leases: vec![lease],
        })
        .await?;
    let mut sessions = install.sessions().to_vec();
    sessions[0].branches[0].fence_floor = 0;
    assert!(
        MoveInstall::new(sessions, install.lines().to_vec()).is_err(),
        "an imported floor cannot precede its checkpoint's recorded lease fence"
    );
    let mut sessions = install.sessions().to_vec();
    sessions[0].feed_tail_floor = u64::MAX;
    assert!(MoveInstall::new(sessions, install.lines().to_vec()).is_err());
    let mut sessions = install.sessions().to_vec();
    sessions[0].branches[0].fence_floor = u64::MAX;
    assert!(MoveInstall::new(sessions, install.lines().to_vec()).is_err());
    let mut lines = install.lines().to_vec();
    lines[0].line = plakous_agent::internal::LineRecord::from_store(
        lines[0].line.line(),
        lines[0].line.head(),
        u64::MAX,
        lines[0].line.origin().clone(),
    );
    assert!(MoveInstall::new(install.sessions().to_vec(), lines).is_err());
    let mut sessions = install.sessions().to_vec();
    let branch = sessions[0].branches[0].clone();
    sessions[0].branches.push(branch);
    assert!(MoveInstall::new(sessions, install.lines().to_vec()).is_err());
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_preparation_and_staging_stay_inert_after_restart() -> TestResult {
    let temp = tempfile::tempdir()?;
    let source_path = temp.path().join("source.sqlite3");
    let target_path = temp.path().join("target.sqlite3");
    let source = crate::SqliteSessionStore::open(&source_path)?;
    let lease = seed(&source, RuntimeDurability::Required).await?;
    let operation = SessionWorkflowId::new([71; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            operation,
            leases: vec![lease.clone()],
        })
        .await?;
    let line = install.lines()[0].line.line();
    let target = crate::SqliteSessionStore::open(&target_path)?;
    target.stage_move(operation, install.clone()).await?;
    source.shutdown().await?;
    target.shutdown().await?;
    drop(source);
    drop(target);
    let source = crate::SqliteSessionStore::open_verified(&source_path)?;
    let target = crate::SqliteSessionStore::open_verified(&target_path)?;
    assert_eq!(
        source.recover_move(operation).await?.digest(),
        install.digest()
    );
    assert_eq!(
        target.recover_move(operation).await?.digest(),
        install.digest()
    );
    assert_eq!(
        source.load_line(line).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        target.load_line(line).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert!(
        target
            .load_checkpoint(install.sessions()[0].branches[0].checkpoint)
            .await
            .is_ok()
    );
    let wrong = plakous_agent::internal::Digest::sha256(
        plakous_agent::internal::DigestDomain::Blob,
        1,
        &[b"different"],
    )?;
    assert!(
        source
            .finish_move(operation, wrong, MoveDisposition::Abort)
            .await
            .is_err()
    );
    let retagged = plakous_agent::internal::Digest::from_sha256_bytes(
        plakous_agent::internal::DigestDomain::Blob,
        2,
        *install.digest().bytes(),
    )?;
    assert!(
        source
            .finish_move(operation, retagged, MoveDisposition::Abort)
            .await
            .is_err()
    );
    source
        .finish_move(operation, install.digest(), MoveDisposition::Abort)
        .await?;
    assert!(source.load_line(line).await.is_ok());
    target
        .finish_move(operation, install.digest(), MoveDisposition::Abandon)
        .await?;
    assert_eq!(
        target.load_line(line).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    source.shutdown().await?;
    target.shutdown().await?;
    Ok(())
}

async fn generation_floors(store: &dyn SessionStore, durability: RuntimeDurability) -> TestResult {
    use plakous_agent::internal::host_move::{MoveBranch, MoveLine, MoveSession};
    use plakous_agent::internal::{
        BranchRevision, LineOrigin, LineRecord, SessionHead, SessionRevision,
    };
    let sid = fixture::id(1)?;
    let old = SessionRef::new(sid, fixture::id(2)?);
    let head = SessionRef::new(sid, fixture::id(7)?);
    let retired = fixture::checkpoint(
        fixture::topology(sid, old.branch_id(), fixture::id(30)?, None, 0, None, 1)?,
        1,
        durability,
    )?;
    let mut topology =
        fixture::topology(sid, head.branch_id(), fixture::id(31)?, None, 0, None, 9)?;
    topology.line = fixture::id(2)?;
    topology.line_generation = 3;
    let current = fixture::checkpoint(topology, 2, durability)?;
    let install = MoveInstall::new(
        vec![MoveSession {
            feed_tail_floor: 0,
            session: SessionHead::from_store(
                sid,
                old.branch_id(),
                SessionRevision::from_store(2),
                Timestamp::new(1, 0)?,
                durability,
                false,
            ),
            branches: vec![
                MoveBranch {
                    session: old,
                    revision: BranchRevision::INITIAL,
                    checkpoint: retired.id(),
                    fence_floor: 50,
                },
                MoveBranch {
                    session: head,
                    revision: BranchRevision::INITIAL,
                    checkpoint: current.id(),
                    fence_floor: 12,
                },
            ],
            checkpoints: vec![retired, current],
        }],
        vec![MoveLine {
            line: LineRecord::from_store(fixture::id(2)?, head, 3, LineOrigin::Created),
            terminals: vec![],
        }],
    )?;
    rejects_future_branch(&install, durability)?;
    let operation = SessionWorkflowId::new([99; 32])?;
    store.stage_move(operation, install.clone()).await?;
    for selector in [old, head] {
        assert_eq!(
            store
                .acquire(AcquireBranch::new(
                    selector,
                    fixture::id(80)?,
                    NonZeroU64::MIN
                ))
                .await
                .err(),
            Some(SessionStoreError::NotLineHead)
        );
    }
    store
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    assert_eq!(store.load_line(fixture::id(2)?).await?.generation(), 3);
    for (selector, expected) in [(old, 51), (head, 13)] {
        let lease = store
            .acquire(AcquireBranch::new(
                selector,
                fixture::id(80)?,
                NonZeroU64::MIN,
            ))
            .await?;
        assert_eq!(lease.fence().get(), expected);
    }
    assert!(
        store
            .finish_move(operation, install.digest(), MoveDisposition::Abandon)
            .await
            .is_err()
    );
    Ok(())
}

fn rejects_future_branch(install: &MoveInstall, durability: RuntimeDurability) -> TestResult {
    use plakous_agent::internal::{BranchRevision, host_move::MoveBranch};
    let mut sessions = install.sessions().to_vec();
    let sid = sessions[0].session.session_id();
    let mut topology = fixture::topology(sid, fixture::id(8)?, fixture::id(39)?, None, 0, None, 9)?;
    topology.line = fixture::id(2)?;
    topology.line_generation = 4;
    let future = fixture::checkpoint(topology, 2, durability)?;
    sessions[0].branches.push(MoveBranch {
        session: SessionRef::new(sid, fixture::id(8)?),
        revision: BranchRevision::INITIAL,
        checkpoint: future.id(),
        fence_floor: 12,
    });
    sessions[0].checkpoints.push(future);
    assert!(
        MoveInstall::new(sessions, install.lines().to_vec()).is_err(),
        "a retired branch cannot lower the imported line generation"
    );
    Ok(())
}

async fn superseded_members(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult {
    let first = seed_at(source, durability, 0).await?;
    let second = seed_at(source, durability, 100).await?;
    let whole = SessionWorkflowId::new([91; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            operation: whole,
            leases: vec![first.clone(), second],
        })
        .await?;
    source
        .finish_move(whole, install.digest(), MoveDisposition::Commit)
        .await?;
    target.stage_move(whole, install.clone()).await?;
    target
        .finish_move(whole, install.digest(), MoveDisposition::Publish)
        .await?;
    let lease = target
        .acquire(AcquireBranch::new(
            first.session(),
            fixture::id(80)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
        ))
        .await?;
    let newer = SessionWorkflowId::new([92; 32])?;
    let frozen = target
        .prepare_move(PrepareMove {
            operation: newer,
            leases: vec![lease],
        })
        .await?;
    assert_eq!(
        target
            .finish_move(whole, install.digest(), MoveDisposition::Abandon)
            .await
            .err(),
        Some(SessionStoreError::Conflict)
    );
    assert_eq!(
        target.load_line(fixture::id(2)?).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert!(
        target.load_line(fixture::id(102)?).await.is_ok(),
        "an old operation must not partially abandon its remaining member"
    );
    target
        .finish_move(newer, frozen.digest(), MoveDisposition::Abort)
        .await?;
    assert!(target.load_line(fixture::id(2)?).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn host_move_memory_disposition_requires_all_original_batch_members() -> TestResult {
    superseded_members(
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        RuntimeDurability::Disabled,
    )
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_disposition_requires_all_original_batch_members() -> TestResult {
    let temp = tempfile::tempdir()?;
    let source = crate::SqliteSessionStore::open(temp.path().join("source.sqlite3"))?;
    let target = crate::SqliteSessionStore::open(temp.path().join("target.sqlite3"))?;
    superseded_members(&source, &target, RuntimeDurability::Required).await?;
    source.shutdown().await?;
    target.shutdown().await?;
    Ok(())
}

async fn installed_session_deletion(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult {
    use plakous_agent::internal::{ArchiveSession, DeleteSession};
    let lease = seed(source, durability).await?;
    let operation = SessionWorkflowId::new([81; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            operation,
            leases: vec![lease.clone()],
        })
        .await?;
    source
        .finish_move(operation, install.digest(), MoveDisposition::Commit)
        .await?;
    target.stage_move(operation, install.clone()).await?;
    target
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    let session = install.sessions()[0].session;
    let archived = target
        .archive(ArchiveSession::new(
            session.session_id(),
            session.revision(),
        ))
        .await?;
    target
        .delete(DeleteSession::new(
            session.session_id(),
            archived.session().revision(),
        ))
        .await?;
    assert_eq!(
        target.load_branch(lease.session()).await.err(),
        Some(SessionStoreError::NotFound)
    );
    assert_eq!(
        source.load_line(install.lines()[0].line.line()).await.err(),
        Some(SessionStoreError::OwnershipMoved)
    );
    Ok(())
}

#[tokio::test]
async fn host_move_memory_installed_session_can_be_archived_and_deleted() -> TestResult {
    installed_session_deletion(
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        RuntimeDurability::Disabled,
    )
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_installed_session_can_be_archived_and_deleted() -> TestResult {
    let temp = tempfile::tempdir()?;
    let source = crate::SqliteSessionStore::open(temp.path().join("source.sqlite3"))?;
    let path = temp.path().join("target.sqlite3");
    let target = crate::SqliteSessionStore::open(&path)?;
    installed_session_deletion(&source, &target, RuntimeDurability::Required).await?;
    source.shutdown().await?;
    target.shutdown().await?;
    drop(target);
    let reopened = crate::SqliteSessionStore::open_verified(&path)?;
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn host_move_memory_preserves_current_generation_and_retired_branch_floors() -> TestResult {
    generation_floors(
        &crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?,
        RuntimeDurability::Disabled,
    )
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_preserves_current_generation_and_retired_branch_floors() -> TestResult {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("store.sqlite3");
    let store = crate::SqliteSessionStore::open(&path)?;
    generation_floors(&store, RuntimeDurability::Required).await?;
    store.shutdown().await?;
    drop(store);
    let reopened = crate::SqliteSessionStore::open_verified(&path)?;
    assert_eq!(reopened.load_line(fixture::id(2)?).await?.generation(), 3);
    reopened.shutdown().await?;
    Ok(())
}
