//! A destination's capacity and identity refusal precede its first write.
use super::*;
use crate::{MemoryStoreLimits, SqliteSessionStore, SqliteStoreOptions};

#[tokio::test]
async fn move_preflight_checks_capacity_and_identity_without_creating_or_changing_a_store()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let source = SqliteSessionStore::open(directory.path().join("source.sqlite3"))?;
    let lease = seed(&source, RuntimeDurability::Required).await?;
    let operation = SessionWorkflowId::new([141; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            operation,
            leases: vec![lease],
        })
        .await?;
    let path = directory.path().join("destination.sqlite3");
    let defaults = SqliteStoreOptions::default();
    SqliteSessionStore::inspect_move_install(&path, &install, defaults)?;
    assert!(
        !path.exists(),
        "a preflight must not create its destination"
    );
    let limits = MemoryStoreLimits::new(
        std::num::NonZeroUsize::new(100).ok_or("bound")?,
        std::num::NonZeroUsize::new(100).ok_or("bound")?,
        std::num::NonZeroUsize::new(100).ok_or("bound")?,
        std::num::NonZeroUsize::MIN,
        std::num::NonZeroUsize::MIN,
        std::num::NonZeroUsize::MIN,
    );
    assert!(matches!(
        SqliteSessionStore::inspect_move_install(
            &path,
            &install,
            SqliteStoreOptions::new(limits, defaults.queue_capacity())
        ),
        Err(SessionStoreError::Capacity(_))
    ));
    assert!(!path.exists());
    let target = SqliteSessionStore::open(&path)?;
    target.stage_move(operation, install.clone()).await?;
    target.shutdown().await?;
    drop(target);
    let before = std::fs::read(&path)?;
    assert_eq!(
        SqliteSessionStore::inspect_move_install(&path, &install, defaults),
        Err(SessionStoreError::IdentityConflict)
    );
    assert_eq!(before, std::fs::read(&path)?);
    source
        .finish_move(operation, install.digest(), MoveDisposition::Abort)
        .await?;
    source.shutdown().await?;
    Ok(())
}
