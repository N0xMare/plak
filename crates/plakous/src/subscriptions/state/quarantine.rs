use super::{
    HashSet, Mutex, NativeProvider, NativeSubscriptionError, OnceLock, PathBuf, SubscriptionPaths,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ProcessQuarantineKey {
    provider: NativeProvider,
    path: PathBuf,
}

pub(super) static LOGIN_RECOVERY_QUARANTINE: OnceLock<Mutex<HashSet<ProcessQuarantineKey>>> =
    OnceLock::new();

pub(super) fn process_quarantine_key(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    path: PathBuf,
) -> ProcessQuarantineKey {
    let _ = paths;
    ProcessQuarantineKey { provider, path }
}

pub(super) fn quarantined(
    registry: &OnceLock<Mutex<HashSet<ProcessQuarantineKey>>>,
    key: &ProcessQuarantineKey,
) -> Result<bool, NativeSubscriptionError> {
    registry
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|entries| entries.contains(key))
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)
}

pub(super) fn quarantine(
    registry: &OnceLock<Mutex<HashSet<ProcessQuarantineKey>>>,
    key: ProcessQuarantineKey,
) -> Result<(), NativeSubscriptionError> {
    registry
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?
        .insert(key);
    Ok(())
}

pub(super) fn clear_quarantine(
    registry: &OnceLock<Mutex<HashSet<ProcessQuarantineKey>>>,
    key: &ProcessQuarantineKey,
) -> Result<(), NativeSubscriptionError> {
    registry
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?
        .remove(key);
    Ok(())
}
