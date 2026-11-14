use super::*;
use plakous_agent::internal::host_move::AbandonUnopenedMove;

async fn check_guards(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
    opened: bool,
) -> TestResult {
    let source_lease = seed(source, durability).await?;
    let operation = SessionWorkflowId::new([91; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            job_cut: plakous_agent::internal::host_move::MoveJobCut::Settled,
            operation,
            leases: vec![source_lease.clone()],
        })
        .await?;
    target.stage_move(operation, install.clone()).await?;
    target
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    assert_eq!(
        target
            .finish_move(operation, install.digest(), MoveDisposition::Abandon)
            .await,
        Err(SessionStoreError::Conflict)
    );
    let selector = source_lease.session();
    let lease = target
        .acquire(AcquireBranch::new(
            selector,
            fixture::id(19)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
        ))
        .await?;
    target.release(lease).await?;
    assert_eq!(
        target.recover_move(operation).await?.digest(),
        install.digest(),
        "lease-only reconciliation must recover the original floor, not its new feed tail or fence"
    );
    let lease = target
        .acquire(AcquireBranch::new(
            selector,
            fixture::id(19)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
        ))
        .await?;
    let missing = AbandonUnopenedMove {
        operation,
        digest: install.digest(),
        leases: vec![],
    };
    assert!(target.abandon_unopened_move(missing).await.is_err());
    let lease = if opened {
        target.record_execution_open(lease.clone()).await?;
        assert_eq!(
            target.recover_move(operation).await.err(),
            Some(SessionStoreError::Conflict)
        );
        target.release(lease).await?;
        target
            .acquire(AcquireBranch::new(
                selector,
                fixture::id(20)?,
                NonZeroU64::new(300).ok_or("nonzero")?,
            ))
            .await?
    } else {
        lease
    };
    let request = AbandonUnopenedMove {
        operation,
        digest: install.digest(),
        leases: vec![lease.clone()],
    };
    if opened {
        assert_eq!(
            target.abandon_unopened_move(request).await,
            Err(SessionStoreError::Conflict)
        );
        assert!(
            target
                .load_line(plakous_agent::internal::LineId::started_by(
                    selector.branch_id()
                ))
                .await
                .is_ok()
        );
        target.release(lease).await?;
    } else {
        target.abandon_unopened_move(request.clone()).await?;
        target.abandon_unopened_move(request).await?;
        assert!(
            target
                .load_line(plakous_agent::internal::LineId::started_by(
                    selector.branch_id()
                ))
                .await
                .is_err()
        );
        assert!(target.renew(lease).await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn host_move_memory_published_abandon_requires_exact_holds_and_no_execution_open()
-> TestResult {
    for opened in [false, true] {
        let source = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
        let target = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
        check_guards(&source, &target, RuntimeDurability::Disabled, opened).await?;
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_published_abandon_requires_exact_holds_and_no_execution_open()
-> TestResult {
    for opened in [false, true] {
        let root = tempfile::tempdir()?;
        let source = crate::SqliteSessionStore::open(root.path().join("source.sqlite3"))?;
        let target = crate::SqliteSessionStore::open(root.path().join("target.sqlite3"))?;
        check_guards(&source, &target, RuntimeDurability::Required, opened).await?;
        source.shutdown().await?;
        target.shutdown().await?;
    }
    Ok(())
}
