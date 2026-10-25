//! Credential reads, Ready leases, login replacement and local retirement.

use super::{PrivateCredentialStore, PrivateStoreError, ReadyCredentialLease, platform};
use crate::{
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialStatus, SecretBytes,
};

impl PrivateCredentialStore {
    /// Loads only the credential-free fence under the shared record lock.
    ///
    /// This method never returns provider payload bytes. A Ready payload is
    /// available only through [`Self::acquire_ready`], whose returned lease
    /// retains the shared cross-process lock. A read waits only for a
    /// writer's brief record replacement; it never waits behind a refresh
    /// exchange, whose claim owns the execution lock rather than the record
    /// lock, so the durable Unknown fence of an in-flight refresh is visible.
    ///
    /// # Errors
    ///
    /// Rejects unsafe filesystem objects, malformed or unknown formats,
    /// oversized data, and foreign profile/slot records.
    pub fn load(&self) -> Result<Option<CredentialFence>, PrivateStoreError> {
        self.with_shared_lock(
            |store| Ok(platform::load_record(store)?.map(|record| record.fence())),
        )
    }

    /// Reports whether a durable [`CredentialStatus::RefreshOutcomeUnknown`]
    /// `unknown` fence is a settled outcome rather than a refresh claim still
    /// in flight in this or another process.
    ///
    /// A claim writes its outcome before it releases the execution lock, so a
    /// free lock followed by an unchanged re-read is settled. A held lock, a
    /// changed record, or writer contention is unsettled: callers report
    /// retryable contention, never a quarantine that demands a new login. The
    /// probe never waits and never creates or writes a file.
    ///
    /// # Errors
    ///
    /// Rejects unsafe storage objects and lock failures.
    pub fn refresh_outcome_settled(
        &self,
        unknown: &CredentialFence,
    ) -> Result<bool, PrivateStoreError> {
        if self.refresh_claim_in_flight()? {
            return Ok(false);
        }
        match self.try_load() {
            Ok(current) => Ok(current.as_ref() == Some(unknown)),
            Err(PrivateStoreError::StoreBusy) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Attempts to load the credential-free fence without waiting for another
    /// process that currently owns mutation authority.
    ///
    /// # Errors
    ///
    /// Returns [`PrivateStoreError::StoreBusy`] when the store lock is
    /// contended and otherwise preserves [`Self::load`]'s validation errors.
    pub fn try_load(&self) -> Result<Option<CredentialFence>, PrivateStoreError> {
        let lock = platform::lock_shared(self, true)?.ok_or(PrivateStoreError::StoreBusy)?;
        platform::verify_lock_authority(self, &lock)?;
        let result = platform::load_record(self).map(|record| record.map(|value| value.fence()));
        platform::verify_lock_authority(self, &lock)?;
        result
    }

    /// Classifies the stored record for offline status surfaces.
    ///
    /// A durable Unknown record whose refresh claim a live process still owns
    /// is [`crate::OfflineCredentialLifecycle::Refreshing`], never a settled
    /// quarantine. A claim writes its outcome before it releases the execution
    /// lock, so a free lock is followed by a re-read that sees that outcome.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`] and rejects unsafe lock files.
    pub fn offline_lifecycle(
        &self,
    ) -> Result<crate::OfflineCredentialLifecycle, PrivateStoreError> {
        let status = self.status()?;
        if status != Some(CredentialStatus::RefreshOutcomeUnknown) {
            return Ok(crate::OfflineCredentialLifecycle::classify(status));
        }
        if self.refresh_claim_in_flight()? {
            return Ok(crate::OfflineCredentialLifecycle::Refreshing);
        }
        Ok(crate::OfflineCredentialLifecycle::classify(self.status()?))
    }

    /// Loads only the current credential lifecycle status under lock.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`].
    pub fn status(&self) -> Result<Option<CredentialStatus>, PrivateStoreError> {
        self.load().map(|fence| fence.map(|value| value.status()))
    }

    /// Reads the current payload in any lifecycle state under the shared
    /// lock, without a lease, claim or write, so a provider can tell its own
    /// payload format version apart before any operation that writes.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`].
    pub fn inspect_payload<T>(
        &self,
        inspect: impl FnOnce(&SecretBytes) -> T,
    ) -> Result<Option<T>, PrivateStoreError> {
        self.with_shared_lock(|store| {
            Ok(platform::load_record(store)?.map(|record| inspect(record.payload())))
        })
    }

    /// Initializes an absent selected-account slot with its actual account.
    ///
    /// # Errors
    ///
    /// Rejects foreign profiles, non-Ready records, existing records, or
    /// unsafe storage.
    pub fn initialize_selected(&self, record: &CredentialRecord) -> Result<(), PrivateStoreError> {
        self.require_scope(record)?;
        if record.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        self.with_lock(|store| {
            if platform::load_record(store)?.is_some() {
                return Err(PrivateStoreError::AlreadyInitialized);
            }
            platform::write_record(store, record)
        })
    }

    /// Acquires a blocking shared lease for one exact Ready fence.
    ///
    /// The record is reloaded and its profile, account, generation, and Ready
    /// status are checked while the shared cross-process lock is held. The
    /// returned lease retains that lock until drop.
    ///
    /// # Errors
    ///
    /// Rejects a stale/foreign fence, a non-Ready record, missing data, or
    /// unsafe storage.
    pub fn acquire_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<ReadyCredentialLease, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        if expected.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }

        let lock = platform::lock_shared(self, false)?.ok_or(PrivateStoreError::LockFailed)?;
        self.ready_lease_under_lock(expected, lock)
    }

    /// Attempts to acquire a shared Ready lease without waiting.
    ///
    /// Returns `Ok(None)` only when an exclusive holder currently blocks the
    /// shared lock. Any acquired record is fully reloaded and fence-validated.
    ///
    /// # Errors
    ///
    /// Returns the same validation and storage errors as [`Self::acquire_ready`].
    pub fn try_acquire_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<Option<ReadyCredentialLease>, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        if expected.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }

        let Some(lock) = platform::lock_shared(self, true)? else {
            return Ok(None);
        };
        self.ready_lease_under_lock(expected, lock).map(Some)
    }

    /// Atomically replaces the record using monotonic generation CAS.
    ///
    /// The current record is reloaded under the cross-process lock. Its
    /// generation must equal `expected_generation`, and the replacement
    /// generation must be strictly greater. Generic CAS may preserve an
    /// Unknown quarantine but can never clear it; clearing requires a
    /// consuming refresh restoration/completion or selected-login operation.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, rollback/equal generations, missing records,
    /// foreign scope, and unsafe storage.
    pub fn replace(
        &self,
        expected_generation: CredentialGeneration,
        replacement: &CredentialRecord,
    ) -> Result<(), PrivateStoreError> {
        self.require_scope(replacement)?;
        self.with_lock(move |store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            if replacement.generation() <= current.generation() {
                return Err(PrivateStoreError::GenerationRollback);
            }
            if current.status() == CredentialStatus::RefreshOutcomeUnknown
                && replacement.status() != CredentialStatus::RefreshOutcomeUnknown
            {
                return Err(PrivateStoreError::CredentialQuarantined);
            }
            if replacement.account_binding() != current.account_binding() {
                return Err(PrivateStoreError::ForeignAccount);
            }
            platform::write_record(store, replacement)
        })
    }

    /// Atomically logs in or switches the actual account in a selected slot.
    ///
    /// The current record must match `expected` exactly. The fully formed,
    /// non-cloneable Ready replacement is consumed as the caller's fresh-login
    /// capability and must advance by exactly one generation. This is the only
    /// slot operation that may change the actual account binding.
    ///
    /// # Errors
    ///
    /// Rejects stale fences, non-Ready records, generation jumps, foreign
    /// profiles, or unsafe storage.
    pub fn replace_selected_login(
        &self,
        expected: &CredentialFence,
        replacement: CredentialRecord,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        self.require_scope(&replacement)?;
        if replacement.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        self.with_lock(move |store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.fence() != *expected {
                return Err(PrivateStoreError::FenceMismatch);
            }
            let next = current
                .generation()
                .next()
                .map_err(|_| PrivateStoreError::GenerationExhausted)?;
            if replacement.generation() != next {
                return Err(PrivateStoreError::GenerationNotSequential);
            }
            platform::write_record(store, &replacement)?;
            Ok(replacement.fence())
        })
    }

    /// Logs out by atomically replacing the payload with a revoked tombstone.
    ///
    /// Keeping the next-generation tombstone prevents a stale writer from
    /// resurrecting the credential after logout.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, a missing record, exhausted generation space, or
    /// unsafe storage.
    pub fn logout(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.with_lock(|store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            let next = current
                .generation()
                .next()
                .map_err(|_| PrivateStoreError::GenerationExhausted)?;
            let tombstone = CredentialRecord::new(
                self.profile_id.clone(),
                current.account_binding().clone(),
                next,
                CredentialStatus::Revoked,
                SecretBytes::empty(),
            )
            .map_err(|_| PrivateStoreError::InvalidRecord)?;
            platform::write_record(store, &tombstone)?;
            Ok(next)
        })
    }

    /// Offline local logout tombstone (no network). Absent → `SignedOut`.
    ///
    /// An explicitly selected slot may contain a prior profile. Local
    /// logout retires that record into an empty current-profile tombstone; it
    /// never grants authority to load, refresh, or reuse the old payload.
    ///
    /// # Errors
    ///
    /// Stale writers, malformed records, or unsafe storage.
    pub fn logout_local(&self) -> Result<crate::OfflineCredentialLifecycle, PrivateStoreError> {
        self.with_lock(|store| {
            let current = match platform::load_record_for_local_retirement(store) {
                Ok(Some(current)) => current,
                Ok(None) => return Ok(crate::OfflineCredentialLifecycle::SignedOut),
                // A newer envelope has no account or generation this build can
                // read to tombstone; retiring it removes it.
                Err(PrivateStoreError::WrittenByNewerBuild) => {
                    platform::remove_record(store)?;
                    return Ok(crate::OfflineCredentialLifecycle::SignedOut);
                }
                Err(error) => return Err(error),
            };
            if current.status() == CredentialStatus::Revoked
                && current.profile_id() == &store.profile_id
            {
                return Ok(crate::OfflineCredentialLifecycle::Revoked);
            }
            let next = current
                .generation()
                .next()
                .map_err(|_| PrivateStoreError::GenerationExhausted)?;
            let tombstone = CredentialRecord::new(
                store.profile_id.clone(),
                current.account_binding().clone(),
                next,
                CredentialStatus::Revoked,
                SecretBytes::empty(),
            )
            .map_err(|_| PrivateStoreError::InvalidRecord)?;
            platform::write_record(store, &tombstone)?;
            Ok(crate::OfflineCredentialLifecycle::Revoked)
        })
    }

    /// Revokes locally using the same stale-writer-safe tombstone as logout.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::logout`].
    pub fn revoke(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.logout(expected_generation)
    }
}

#[cfg(all(
    test,
    any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
#[cfg(test)]
mod tests;
