//! A retired branch is frozen by its current line head, not a new obsolete lease.
use crate::integrity_fixture::{self as fixture, TestResult};
use plakous_agent::internal::host_move::{MoveDisposition, PrepareMove};
use plakous_agent::internal::{
    BranchLease, BranchTransitionMode, CreateSession, Digest, DigestDomain, ForkBranch,
    ForkLineage, ReconstructiveComponent, RuntimeCheckpoint, RuntimeComponentKind,
    RuntimeDurability, RuntimeManifest, SessionRef, SessionStore, SessionStoreError,
    SessionWorkflowId, Timestamp, ToolRuntimeManifest,
};
use std::num::NonZeroU64;

async fn rebound_leases(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult<(BranchLease, BranchLease)> {
    let original = SessionRef::new(fixture::id(1)?, fixture::id(2)?);
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            original.session_id(),
            original.branch_id(),
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
        original,
        Timestamp::new(1, 0)?,
        fixture::id(4)?,
        NonZeroU64::new(300).ok_or("lease")?,
        fixture::bundle(&checkpoint, None)?,
    );
    let created = store.create(create.clone()).await?;
    let loaded = store.load_branch(original).await?;
    let branch = fixture::id(22)?;
    let context = checkpoint.manifest().canonical().context().rebind(
        checkpoint.id(),
        original.session_id(),
        branch,
        [117; 32],
    )?;
    let mut topology = fixture::topology(
        original.session_id(),
        branch,
        fixture::id(23)?,
        Some(checkpoint.id()),
        0,
        Some(ForkLineage {
            session_id: original.session_id(),
            branch_id: original.branch_id(),
            checkpoint_id: checkpoint.id(),
            branch_revision: 0,
        }),
        1,
    )?;
    topology.line = checkpoint.manifest().topology().line;
    topology.line_generation = 2;
    let rebound = fixture::checkpoint_with_context(topology, 1, durability, context)?;
    let rebound = changed_environment(&rebound)?;
    let receipt = store
        .fork(
            ForkBranch::new(
                SessionWorkflowId::new([117; 32])?,
                Digest::sha256(DigestDomain::RequestPlan, 1, &[b"rebind"])?,
                created.lease().clone(),
                loaded.session().revision(),
                loaded.head(),
                branch,
                fixture::id(24)?,
                NonZeroU64::new(300).ok_or("lease")?,
                fixture::bundle(&rebound, Some(create.initial()))?,
            )
            .with_transition(BranchTransitionMode::Rebind),
        )
        .await?;
    Ok((created.lease().clone(), receipt.lease().clone()))
}

fn changed_environment(checkpoint: &RuntimeCheckpoint) -> TestResult<RuntimeCheckpoint> {
    let manifest = checkpoint.manifest();
    let tools = manifest.tools();
    let tools = ToolRuntimeManifest::new(
        tools.registry().clone(),
        tools.exposure().clone(),
        ReconstructiveComponent::new(
            RuntimeComponentKind::ExecutionEnvironment,
            1,
            b"rebound-environment".to_vec(),
        )?,
        tools.effect_policy().clone(),
        tools.policy().clone(),
    )?;
    Ok(RuntimeCheckpoint::new(RuntimeManifest::new(
        manifest.provenance().clone(),
        manifest.topology(),
        manifest.lifecycle().clone(),
        manifest.harness_plan_component().clone(),
        manifest.provider_configuration().clone(),
        manifest.canonical().clone(),
        manifest.binding().clone(),
        tools,
        manifest.ledger().clone(),
        None,
        None,
        manifest.jobs().clone(),
        manifest.named_state().clone(),
        manifest.durability(),
    )?)?)
}

async fn rebind(store: &dyn SessionStore, durability: RuntimeDurability) -> TestResult {
    let (predecessor, current) = rebound_leases(store, durability).await?;
    let operation = SessionWorkflowId::new([118; 32])?;
    assert_eq!(
        store
            .prepare_move(PrepareMove {
                job_cut: plakous_agent::internal::host_move::MoveJobCut::Settled,
                operation,
                leases: vec![predecessor.clone(), current.clone()],
            })
            .await
            .err(),
        Some(SessionStoreError::NotLineHead)
    );
    let install = store
        .prepare_move(PrepareMove {
            job_cut: plakous_agent::internal::host_move::MoveJobCut::Settled,
            operation,
            leases: vec![current.clone()],
        })
        .await?;
    assert_eq!(install.sessions().len(), 1);
    assert_eq!(install.sessions()[0].branches.len(), 2);
    assert_eq!(install.lines().len(), 1);
    assert_eq!(install.lines()[0].line.head(), current.session());
    assert_eq!(install.lines()[0].line.generation(), 2);
    assert_eq!(
        store.recover_move(operation).await?.digest(),
        install.digest()
    );
    store
        .finish_move(operation, install.digest(), MoveDisposition::Abort)
        .await?;
    assert!(store.load_branch(current.session()).await.is_ok());
    assert_eq!(
        store
            .load_line(install.lines()[0].line.line())
            .await?
            .head(),
        current.session()
    );
    let next = SessionWorkflowId::new([119; 32])?;
    let install = store
        .prepare_move(PrepareMove {
            job_cut: plakous_agent::internal::host_move::MoveJobCut::Settled,
            operation: next,
            leases: vec![current.clone()],
        })
        .await?;
    store
        .finish_move(next, install.digest(), MoveDisposition::Commit)
        .await?;
    for selector in [predecessor.session(), current.session()] {
        assert_eq!(
            store.load_branch(selector).await.err(),
            Some(SessionStoreError::OwnershipMoved)
        );
    }
    Ok(())
}

#[tokio::test]
async fn host_move_memory_rebound_session_requires_only_its_current_head_guard() -> TestResult {
    let store = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
    rebind(&store, RuntimeDurability::Disabled).await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_rebound_session_requires_only_its_current_head_guard() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("runtime.sqlite3");
    let store = crate::SqliteSessionStore::open(&path)?;
    rebind(&store, RuntimeDurability::Required).await?;
    store.shutdown().await?;
    drop(store);
    let reopened = crate::SqliteSessionStore::open_verified(&path)?;
    reopened.shutdown().await?;
    Ok(())
}
