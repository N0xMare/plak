//! Named-directory authority, creation cleanup and signed-out removal.

use super::{
    Arc, File, Mutex, NativeProvider, NativeSubscriptionError, OsStr, PrivateFileIdentity,
    PrivateFilesystemLock, SubscriptionPaths, SubscriptionState, acquire_private_filesystem_lock,
    ensure_state_root, new_random_identity, open_or_create_private_child, open_private_parent,
    private_file_identity, try_acquire_private_filesystem_lock,
};

pub(super) struct CreatedDirectory {
    paths: SubscriptionPaths,
    provider: NativeProvider,
    identity: PrivateFileIdentity,
    consent: Mutex<Option<String>>,
}

impl CreatedDirectory {
    pub(super) fn record_consent(&self, generation: &str) -> Result<(), NativeSubscriptionError> {
        *self
            .consent
            .lock()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)? = Some(generation.to_owned());
        Ok(())
    }
}

impl Drop for CreatedDirectory {
    fn drop(&mut self) {
        let Ok(expected) = self.consent.get_mut() else {
            return;
        };
        let expected = expected.clone();
        if matches!(
            cleanup(
                &self.paths,
                self.provider,
                self.identity,
                expected.as_deref(),
                false
            ),
            Ok(false)
        ) {
            let paths = self.paths.clone();
            let provider = self.provider;
            let captured = self.identity;
            // Waiting on another async writer from Drop could block its own
            // executor. Recheck after that writer releases compatibility instead.
            let _ = std::thread::Builder::new()
                .name("account-directory-cleanup".into())
                .spawn(move || {
                    let _ = cleanup(&paths, provider, captured, expected.as_deref(), true);
                });
        }
    }
}

fn cleanup(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    captured: PrivateFileIdentity,
    expected_consent: Option<&str>,
    blocking: bool,
) -> Result<bool, NativeSubscriptionError> {
    let path = paths.compatibility_lock_path(provider);
    let lock = if blocking {
        acquire_private_filesystem_lock(&path)?
    } else if let Some(lock) = try_acquire_private_filesystem_lock(&path)? {
        lock
    } else {
        return Ok(false);
    };
    let (directory, _) = open_private_parent(&paths.provider_record(provider))?;
    if identity(&directory)? != captured
        || entry_exists(&paths.provider_record(provider))?
        || entry_exists(&paths.login_recovery_path(provider))?
        || super::stored_acceptance_generation(paths, provider)?.as_deref() != expected_consent
    {
        return Ok(true);
    }
    detach_and_remove(paths, &lock, captured)?;
    Ok(true)
}

fn entry_exists(path: &std::path::Path) -> Result<bool, NativeSubscriptionError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(NativeSubscriptionError::UnsafeStorage),
    }
}

fn identity(directory: &File) -> Result<PrivateFileIdentity, NativeSubscriptionError> {
    directory
        .metadata()
        .map(|metadata| private_file_identity(&metadata))
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)
}

fn detach_and_remove(
    paths: &SubscriptionPaths,
    lock: &PrivateFilesystemLock,
    expected: PrivateFileIdentity,
) -> Result<(), NativeSubscriptionError> {
    let selected = paths
        .named
        .as_ref()
        .ok_or(NativeSubscriptionError::DefaultAccountNotRemovable)?;
    let root = paths.layout_root();
    let (directory, _) = open_private_parent(&root.join("credential.json"))?;
    let (parent, name) = open_private_parent(&root)?;
    lock.verify()?;
    if identity(&directory)? != expected {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let trash_name = format!(".{}.trash-{}", selected.name, new_random_identity()?);
    rustix::fs::renameat(&parent, &name, &parent, OsStr::new(&trash_name))
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    parent
        .sync_all()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    // The lock inode is no longer at a selectable path. Waiting writers must
    // verify their original authority and cannot publish through this rename.
    let trash = root
        .parent()
        .ok_or(NativeSubscriptionError::UnsafeStorage)?
        .join(trash_name);
    std::fs::remove_dir_all(trash).map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    parent
        .sync_all()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)
}

impl SubscriptionState {
    pub(in crate::subscriptions) fn require_signed_out_for_removal(
        &self,
        provider: NativeProvider,
        profile: &str,
    ) -> Result<(), NativeSubscriptionError> {
        use plakous_auth::{
            CredentialStatus, PrivateCredentialStore, SubscriptionIntegrationProfileId,
        };
        let profile = SubscriptionIntegrationProfileId::new(profile)
            .map_err(|_| NativeSubscriptionError::Internal)?;
        let store = PrivateCredentialStore::open_slot(
            self.provider_record(provider),
            profile,
            self.slot_id()?,
        )
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        let fence = store.load().map_err(|error| match error {
            plakous_auth::PrivateStoreError::UnsupportedVersion
            | plakous_auth::PrivateStoreError::WrittenByNewerBuild
            | plakous_auth::PrivateStoreError::ForeignProfile
            | plakous_auth::PrivateStoreError::ForeignAccount
            | plakous_auth::PrivateStoreError::ForeignSlot => {
                NativeSubscriptionError::AccountSignedIn
            }
            _ => NativeSubscriptionError::UnsafeStorage,
        })?;
        if fence.is_some_and(|fence| fence.status() != CredentialStatus::Revoked) {
            return Err(NativeSubscriptionError::AccountSignedIn);
        }
        Ok(())
    }

    pub(in crate::subscriptions) fn named_accounts(
        &self,
        provider: NativeProvider,
    ) -> Result<Vec<String>, NativeSubscriptionError> {
        self.named_accounts_bounded(provider, usize::MAX)
            .map(|(names, _, _)| names)
    }

    pub(in crate::subscriptions) fn named_accounts_bounded(
        &self,
        provider: NativeProvider,
        maximum: usize,
    ) -> Result<(Vec<String>, usize, bool), NativeSubscriptionError> {
        let selected = self.paths.for_named_account(provider, "account")?;
        let slot = selected
            .named
            .as_ref()
            .ok_or(NativeSubscriptionError::Internal)?;
        let Some(directory) = super::filesystem::named_parent(&selected.state_root, slot.vertical)?
        else {
            return Ok((Vec::new(), 0, false));
        };
        let entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        let mut names = Vec::new();
        let mut truncated = false;
        let mut count = 0;
        for (scanned, entry) in entries.enumerate() {
            if scanned >= maximum {
                truncated = true;
                break;
            }
            let entry = entry.map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
            count += 1;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if selected.for_named_account(provider, name).is_ok() {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok((names, count, truncated))
    }

    pub(in crate::subscriptions) fn for_named_account(
        &self,
        provider: NativeProvider,
        name: &str,
    ) -> Result<Self, NativeSubscriptionError> {
        Ok(Self {
            paths: self.paths.for_named_account(provider, name)?,
            clock: self.clock.clone(),
            creation: None,
        })
    }

    pub(in crate::subscriptions) fn require_named(&self) -> Result<(), NativeSubscriptionError> {
        self.paths
            .named
            .as_ref()
            .map(|_| ())
            .ok_or(NativeSubscriptionError::DefaultAccountNotRemovable)
    }

    pub(in crate::subscriptions) fn create_account_for_login(
        mut self,
    ) -> Result<Self, NativeSubscriptionError> {
        let Some(selected) = &self.paths.named else {
            return Ok(self);
        };
        let state = ensure_state_root(&self.paths.state_root)?;
        let plak = open_or_create_private_child(&state, OsStr::new("plak"))?;
        let subscriptions = open_or_create_private_child(&plak, OsStr::new("subscriptions"))?;
        let accounts = open_or_create_private_child(&subscriptions, OsStr::new("accounts"))?;
        let vertical = open_or_create_private_child(&accounts, OsStr::new(selected.vertical))?;
        let created = match rustix::fs::mkdirat(
            &vertical,
            OsStr::new(&selected.name),
            rustix::fs::Mode::from_raw_mode(0o700),
        ) {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(_) => return Err(NativeSubscriptionError::UnsafeStorage),
        };
        let directory = open_or_create_private_child(&vertical, OsStr::new(&selected.name))?;
        let captured = identity(&directory)?;
        for name in ["compatibility", "catalog"] {
            let _ = open_or_create_private_child(&directory, OsStr::new(name))?;
        }
        vertical
            .sync_all()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        let lock = acquire_private_filesystem_lock(
            &self.paths.compatibility_lock_path(selected.provider),
        )?;
        let (active, _) = open_private_parent(&self.paths.provider_record(selected.provider))?;
        lock.verify()?;
        if identity(&active)? != captured {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        if created {
            self.creation = Some(Arc::new(CreatedDirectory {
                paths: self.paths.clone(),
                provider: selected.provider,
                identity: captured,
                consent: Mutex::new(None),
            }));
        }
        Ok(self)
    }

    pub(in crate::subscriptions) fn remove_named_directory(
        &self,
        lock: &PrivateFilesystemLock,
    ) -> Result<(), NativeSubscriptionError> {
        let selected = self
            .paths
            .named
            .as_ref()
            .ok_or(NativeSubscriptionError::DefaultAccountNotRemovable)?;
        let (directory, _) = open_private_parent(&self.paths.provider_record(selected.provider))?;
        detach_and_remove(&self.paths, lock, identity(&directory)?)
    }
}
