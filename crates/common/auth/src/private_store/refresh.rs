//! Durable refresh claims, restoration, completion and quarantine.

use super::{
    PrivateCredentialStore, PrivateStoreError, RefreshClaim, StartedRefreshClaim, platform,
};
use crate::{CredentialFence, CredentialGeneration, CredentialRecord, CredentialStatus};

impl PrivateCredentialStore {
    /// Begins refresh from one exact credential fence.
    ///
    /// Before returning, this method exclusively reloads the exact fenced
    /// record, advances its generation, and durably writes its old payload as
    /// [`CredentialStatus::RefreshOutcomeUnknown`]. The returned non-cloneable
    /// claim owns but does not expose the same old payload. Provider-owned code
    /// must consume [`RefreshClaim::start`] immediately before network work.
    ///
    /// # Errors
    ///
    /// Rejects a stale/foreign fence, unresolved or revoked state, exhausted
    /// generation space, or unsafe storage.
    pub fn begin_refresh(
        &self,
        expected: &CredentialFence,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.begin_refresh_with_options(expected, false)
    }

    /// Begins refresh from an exact fence, allowing a durable
    /// [`CredentialStatus::RefreshOutcomeUnknown`] record to be reclaimed.
    ///
    /// Only dialects that retry the still-stored refresh token after an
    /// unconfirmed exchange may call this. Codex and Claude must use
    /// [`Self::begin_refresh`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::begin_refresh`], except Unknown is claimable when
    /// `expected` matches the loaded record.
    pub fn begin_refresh_reclaiming_unknown(
        &self,
        expected: &CredentialFence,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.begin_refresh_with_options(expected, true)
    }

    /// Begins refresh using generation CAS.
    ///
    /// The profile is fixed by this store handle; the current record must be
    /// Ready or `RefreshRequired`.
    ///
    /// # Errors
    ///
    /// Returns the same lifecycle and storage errors as [`Self::begin_refresh`].
    pub fn begin_refresh_generation(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.claim_under_execution_lock(|current| {
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            Ok(false)
        })
    }

    /// Restores a durable [`CredentialStatus::RefreshOutcomeUnknown`] record to
    /// [`CredentialStatus::Ready`] at the next generation with the same payload.
    ///
    /// xAI retries the stored refresh token after an unconfirmed exchange; Codex
    /// and Claude must not call this.
    ///
    /// # Errors
    ///
    /// Rejects a stale/foreign fence, non-Unknown status, exhausted generation
    /// space, or unsafe storage.
    pub fn restore_unknown_to_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        self.with_lock(|store| store.restore_unknown_record(expected))
    }

    /// Attempts [`Self::restore_unknown_to_ready`] once without waiting.
    ///
    /// Returns `Ok(None)` and leaves the record unchanged when a writer, a
    /// reader, a Ready lease, or a refresh claim currently holds the store.
    /// For cancellation paths, which must never block.
    ///
    /// # Errors
    ///
    /// Same as [`Self::restore_unknown_to_ready`].
    pub fn try_restore_unknown_to_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<Option<CredentialFence>, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        let Some(lock) = platform::try_lock(self)? else {
            return Ok(None);
        };
        platform::verify_lock_authority(self, &lock)?;
        let result = self.restore_unknown_record(expected);
        platform::verify_lock_authority(self, &lock)?;
        result.map(Some)
    }

    /// Completes refresh by consuming an exact claim and a later Ready record.
    ///
    /// The durable store must still contain the claim's exact Unknown fence and
    /// old payload. Wrong, stale, or cross-store claims cannot clear Unknown.
    ///
    /// # Errors
    ///
    /// Rejects a mismatched claim, non-Ready/foreign replacement, a replacement
    /// that is not exactly one generation after the Unknown claim, or unsafe
    /// storage.
    pub fn complete_refresh(
        &self,
        claim: StartedRefreshClaim,
        replacement: CredentialRecord,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        self.require_scope(&replacement)?;
        if replacement.account_binding() != claim.core.record.account_binding() {
            return Err(PrivateStoreError::ForeignAccount);
        }
        if replacement.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        let completion_generation = claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        if replacement.generation() != completion_generation {
            return Err(PrivateStoreError::GenerationNotSequential);
        }
        let completed_fence = replacement.fence();
        let result = self.with_claim_record_lock(&claim.core, |store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            Self::verify_claim_record(&claim.core, &current)?;
            platform::write_record(store, &replacement)
        });
        drop(claim);
        drop(replacement);
        result.map(|()| completed_fence)
    }

    /// Restores the exact old payload/status after proving refresh never began.
    ///
    /// This narrowly named escape hatch consumes the claim and writes the old
    /// payload and pre-claim Ready/RefreshRequired status at a new generation.
    /// It must not be called once provider I/O might have started.
    ///
    /// # Errors
    ///
    /// Rejects stale, wrong, or cross-store claims and unsafe storage.
    pub fn restore_refresh_definitely_not_started(
        &self,
        mut claim: RefreshClaim,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let result = self.restore_claimed_http_not_started_under_lock(&mut claim);
        drop(claim);
        result
    }

    /// Attempts one non-blocking restore of an unstarted claim.
    ///
    /// Returns `Ok(None)` when shared readers or Ready leases currently hold
    /// the record lock; the claim is unchanged so the caller can poll. Never
    /// waits. The claim already owns the private directory, so no other
    /// writer can intervene.
    ///
    /// # Errors
    ///
    /// Same scope and record checks as
    /// [`Self::restore_refresh_definitely_not_started`].
    pub fn try_restore_refresh_definitely_not_started(
        &self,
        claim: &mut RefreshClaim,
    ) -> Result<Option<CredentialFence>, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let Some(record_lock) = self.claim_record_lock(&claim.core, true)? else {
            return Ok(None);
        };
        let restored = self.restore_claim_record(claim)?;
        platform::verify_lock_authority(self, &record_lock)?;
        Ok(Some(restored))
    }

    /// Consumes and verifies a claim while deliberately leaving it quarantined.
    ///
    /// Dropping a claim has the same durable safety property; this explicit
    /// operation additionally proves that the exact Unknown record still exists.
    ///
    /// # Errors
    ///
    /// Rejects stale, wrong, or cross-store claims and unsafe storage.
    pub fn leave_refresh_quarantined(
        &self,
        claim: StartedRefreshClaim,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let result = self.verify_claimed_http_quarantine_under_lock(&claim.core);
        drop(claim);
        result
    }

    /// Persists refresh intent before provider-owned code sends a request.
    ///
    /// This is an exclusive generation-advancing CAS claim. The record moves
    /// to [`CredentialStatus::RefreshOutcomeUnknown`] at the next generation
    /// before this method returns. Provider-owned code must use that returned
    /// generation for completion or quarantine. A crash therefore leaves the
    /// credential quarantined, and another caller using the old generation
    /// cannot send or clear the claim.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, revoked or missing records, and unsafe storage.
    pub fn mark_refresh_intent(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialFence, PrivateStoreError> {
        let claim = self.begin_refresh_generation(expected_generation)?;
        Ok(claim.fence())
    }

    /// Quarantines a refresh whose network outcome is ambiguous.
    ///
    /// This operation is idempotent for the same generation.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, revoked or missing records, and unsafe storage.
    pub fn quarantine_refresh_outcome(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.with_lock(|store| {
            let mut current =
                platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            if current.status() == CredentialStatus::Revoked {
                return Err(PrivateStoreError::CredentialRevoked);
            }
            if current.status() != CredentialStatus::RefreshOutcomeUnknown {
                current.set_status(CredentialStatus::RefreshOutcomeUnknown);
                platform::write_record(store, &current)?;
            }
            Ok(current.fence())
        })
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
