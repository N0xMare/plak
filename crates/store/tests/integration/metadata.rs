use std::num::NonZeroU64;

use plakous_agent::internal::{
    ArchiveSession, CommitCheckpoint, CreateReceipt, CreateSession, RuntimeCheckpoint,
    RuntimeDurability, RuntimeLifecycle, RuntimeWorkingRoot, SessionRef, SessionStore,
    SessionStoreError,
};
use plakous_model::{SessionWorkflowRootId, Timestamp};
use plakous_store::{MemorySessionStore, MemoryStoreLimits};

use super::conformance::{TestResult, bundle, checkpoint, id, topology, working_checkpoint};

const OPERATION: &str = "metadata_conformance";

#[tokio::test]
async fn memory_branch_metadata_conformance() -> TestResult {
    let store = MemorySessionStore::new(MemoryStoreLimits::default())?;
    run(&store, RuntimeDurability::Disabled).await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_branch_metadata_conformance() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = plakous_store::SqliteSessionStore::open(dir.path().join("runtime.sqlite3"))?;
    run(&store, RuntimeDurability::Required).await?;
    store.shutdown().await?;
    Ok(())
}

async fn run(store: &dyn SessionStore, durability: RuntimeDurability) -> TestResult {
    let selector = SessionRef::new(id(90_001)?, id(90_002)?);
    assert_eq!(
        store.load_branch_metadata(selector, OPERATION).await.err(),
        Some(SessionStoreError::NotFound)
    );
    let (created, _) = create(store, durability, 90_000).await?;
    store.release(created.lease().clone()).await?;
    let (head, metadata) = store.load_branch_metadata(selector, OPERATION).await?;
    assert_eq!(head, created.branch());
    assert_eq!(metadata.lifecycle, RuntimeLifecycle::Quiescent);
    assert_eq!(metadata.durability, durability);
    assert_eq!(metadata.requests, [] as [(u64, plakous_model::Digest); 0]);
    assert_eq!(
        store
            .load_branch_metadata(
                SessionRef::new(selector.session_id(), id(90_005)?),
                OPERATION
            )
            .await
            .err(),
        Some(SessionStoreError::NotFound)
    );
    store
        .archive(ArchiveSession::new(
            selector.session_id(),
            created.session().revision(),
        ))
        .await?;
    assert_eq!(
        store.load_branch_metadata(selector, OPERATION).await.err(),
        Some(SessionStoreError::Archived)
    );

    assert_working(
        store,
        durability,
        90_010,
        RuntimeWorkingRoot::Turn {
            turn_id: id(90_015)?,
            next_round: 1,
        },
    )
    .await?;
    assert_working(
        store,
        durability,
        90_020,
        RuntimeWorkingRoot::SessionWorkflow(SessionWorkflowRootId::new([0x56; 32])?),
    )
    .await
}

async fn create(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
) -> TestResult<(CreateReceipt, RuntimeCheckpoint)> {
    let session = id(base + 1)?;
    let branch = id(base + 2)?;
    let genesis = checkpoint(
        topology(session, branch, id(base + 3)?, None, 0, None, 1)?,
        1,
        durability,
    )?;
    let created = store
        .create(CreateSession::new(
            SessionRef::new(session, branch),
            Timestamp::new(50, 0)?,
            id(base + 4)?,
            NonZeroU64::new(60).ok_or("lease duration")?,
            bundle(&genesis, None)?,
        ))
        .await?;
    Ok((created, genesis))
}

async fn assert_working(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
    base: u128,
    root: RuntimeWorkingRoot,
) -> TestResult {
    let (created, genesis) = create(store, durability, base).await?;
    let old = created.branch();
    let selector = old.session();
    let next = checkpoint(
        topology(
            selector.session_id(),
            selector.branch_id(),
            id(base + 6)?,
            Some(old.checkpoint().checkpoint_id()),
            old.revision().get() + 1,
            None,
            created.lease().fence().get(),
        )?,
        2,
        durability,
    )?;
    let next = working_checkpoint(&next, root)?;
    let receipt = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            old,
            bundle(&next, Some(&bundle(&genesis, None)?))?,
        ))
        .await?;
    store.release(created.lease().clone()).await?;
    let (head, metadata) = store.load_branch_metadata(selector, OPERATION).await?;
    // The port returns the current head, not the superseded checkpoint a
    // caller may still hold. Exact-locator checks belong to the facade.
    assert_ne!(head.checkpoint(), old.checkpoint());
    assert_eq!(head, receipt.head());
    assert_eq!(metadata.topology, next.manifest().topology());
    assert_eq!(metadata.lifecycle, *next.manifest().lifecycle());
    assert_eq!(metadata.durability, durability);
    let operations = next.manifest().ledger().operations();
    let [provider] = operations.as_slice() else {
        return Err("fixture must contain one provider request".into());
    };
    assert_eq!(
        metadata.requests,
        [(provider.operation_id(), provider.request_identity())]
    );
    Ok(())
}
