//! Fail-closed storage operations on unsupported platforms.

use super::{PrivateCredentialStore, PrivateStoreError, StoreAuthority};
use crate::CredentialRecord;
use std::path::Path;

/// Uninhabited: no store handle can be constructed on this target.
pub(super) type DirectoryHandle = std::convert::Infallible;

pub(super) struct StoreLock;

pub(super) fn open_private_directory(
    _path: &Path,
) -> Result<(DirectoryHandle, StoreAuthority), PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn lock(_store: &PrivateCredentialStore) -> Result<StoreLock, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn lock_shared(
    _store: &PrivateCredentialStore,
    _nonblocking: bool,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

impl StoreLock {
    pub(super) fn file_identity(&self) -> Result<StoreAuthority, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }
}

pub(super) fn try_lock_execution(
    _store: &PrivateCredentialStore,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn execution_lock_held(
    _store: &PrivateCredentialStore,
) -> Result<bool, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn try_lock(
    _store: &PrivateCredentialStore,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn lock_record_for_claim(
    _store: &PrivateCredentialStore,
    _nonblocking: bool,
) -> Result<Option<StoreLock>, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn verify_lock_authority(
    _store: &PrivateCredentialStore,
    _lock: &StoreLock,
) -> Result<(), PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn load_record(
    _store: &PrivateCredentialStore,
) -> Result<Option<CredentialRecord>, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn load_record_for_local_retirement(
    _store: &PrivateCredentialStore,
) -> Result<Option<CredentialRecord>, PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn remove_record(_store: &PrivateCredentialStore) -> Result<(), PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}

pub(super) fn write_record(
    _store: &PrivateCredentialStore,
    _record: &CredentialRecord,
) -> Result<(), PrivateStoreError> {
    Err(PrivateStoreError::UnsupportedPlatform)
}
