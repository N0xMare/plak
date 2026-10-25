//! The fixed cross-process lock helper entrypoint.

use crate::private_store::{
    LOCK_HELPER_PATH, LOCK_HELPER_READY, LOCK_HELPER_RELEASE, LOCK_HELPER_SHARED_LEASE,
    PrivateCredentialStore, platform,
};
use crate::{CredentialSlotId, SubscriptionIntegrationProfileId};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

#[test]
fn cross_process_lock_helper() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::var_os(LOCK_HELPER_PATH) else {
        return Ok(());
    };
    let ready = std::env::var_os(LOCK_HELPER_READY).ok_or("missing ready path")?;
    let release = std::env::var_os(LOCK_HELPER_RELEASE).ok_or("missing release path")?;
    let store = PrivateCredentialStore::open_slot(
        PathBuf::from(path),
        SubscriptionIntegrationProfileId::new("profile")?,
        CredentialSlotId::new("primary")?,
    )?;
    if std::env::var_os(LOCK_HELPER_SHARED_LEASE).is_some() {
        let fence = store.load()?.ok_or("missing helper credential")?;
        let _lease = store.acquire_ready(&fence)?;
        fs::write(ready, b"ready")?;
        wait_for_release(Path::new(&release))
    } else {
        let _guard = platform::lock(&store)?;
        fs::write(ready, b"ready")?;
        wait_for_release(Path::new(&release))
    }
}

fn wait_for_release(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..400 {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err("timed out waiting for lock release".into())
}
