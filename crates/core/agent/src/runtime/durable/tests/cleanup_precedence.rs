//! A failed operation keeps its own cause when its lease cleanup fails too.

use super::*;
use crate::runtime::DurableRuntimeBootstrap;

#[tokio::test]
async fn a_refused_bootstrap_keeps_its_cause_when_the_first_release_hangs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let builder = || {
        durable_test_builder(
            Arc::clone(&workspace),
            ToolRegistryBuilder::default().freeze(),
            "test.route",
            b"alpha",
            false,
        )
    };
    let agent = runtime
        .create("create:refused-bootstrap", builder()?)
        .await?;
    let session = SessionRef::new(agent.session_id(), agent.branch_id());
    agent.shutdown().await?;
    let admission = sealed_runtime_admission(&builder()?).await?;
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(uuid::Uuid::from_u128(0x0b57_0001))?,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        ))
        .await?;
    store.hang_release_on(store.releases.load(Ordering::Acquire) + 1);
    let stale = DurableSessionAddress::new(
        session.session_id(),
        session.branch_id(),
        CheckpointId::try_from(uuid::Uuid::from_u128(0x0b57_0002))?,
    );
    let Err(error) = DurableRuntimeBootstrap::open(
        store.clone(),
        lease,
        stale,
        RuntimeDurability::Required,
        admission,
        crate::runtime::StoreTimeouts {
            operation: Duration::from_millis(500),
            cleanup: Duration::from_millis(25),
        },
    )
    .await
    else {
        return Err("a stale address opened".into());
    };
    assert_ne!(
        error.store_error(),
        Some(SessionStoreError::BackendBusy),
        "the release deadline replaced the refusal: {}",
        error.message()
    );
    assert!(
        store.released_lease().is_some(),
        "the bounded release retry did not release the lease"
    );
    Ok(())
}
