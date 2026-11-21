//! Local custody of the stored Codex login (spec 01 §7.1): the facts a caller
//! binds to, the credential fields under the exact Ready lease, and the
//! single writer's refresh after a rejection.

use plakous_auth::ReadyCredentialLease;
use plakous_auth::custody::local::{self, LocalCustody, Snapshot};
use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};
#[cfg(test)]
use plakous_test_support::native_auth::custody::{
    advance_ready_unchanged, install_before_lease, run_before_lease,
};

use super::manager::codex_account_digest;
use super::{
    Arc, CodexSubscriptionCredentialManager, CredentialFence, CredentialStatus, Digest,
    DigestDomain, NativeCodexAuthError, credential_selection_epoch, local_credential_status,
    parse_stored, run_blocking,
};

fn identity(digest: Digest) -> IdentityDigest {
    IdentityDigest::from_bytes(*digest.bytes())
}

fn store_error(error: NativeCodexAuthError) -> CustodyError {
    match error {
        NativeCodexAuthError::StoreBusy => local::busy(),
        _ => CustodyError::Store,
    }
}

impl CodexSubscriptionCredentialManager {
    /// Non-secret facts of the stored login, reported for `slot`.
    ///
    /// The identity digests are the exact bytes a route binds a session to:
    /// the account fingerprint and credential authority (model-binding
    /// digests, version 1) and the login selection epoch (version 2). A
    /// session bound under direct custody therefore revalidates unchanged
    /// through a gateway for the same profile and slot, wherever the store
    /// file lives. The profile is the caller's
    /// to report.
    ///
    /// # Errors
    /// [`CustodyError::Store`] for unsafe or unreadable storage.
    pub async fn custody_facts(&self, slot: &CustodySlot) -> Result<AccountFacts, CustodyError> {
        local::facts(self, slot).await
    }

    /// The credential fields of one request prepared against `expected`:
    /// `authorization` and `chatgpt-account-id`, read under the exact Ready
    /// lease they came from, which they hold until the caller drops it.
    ///
    /// # Errors
    /// [`AccountFacts::check_fence`]'s refusals, and
    /// [`CustodyError::NotReady`] outside the compatibility profile's window.
    pub async fn custody_fields(
        &self,
        slot: &CustodySlot,
        expected: &ExpectedFence,
    ) -> Result<CredentialFields, CustodyError> {
        local::fields(self, slot, expected).await
    }

    /// The single writer's refresh after the provider answered `rejected`
    /// with HTTP 401, then the current facts. A generation another caller
    /// already rotated is refreshed only if due, and a newer login is never
    /// claimed, rotated or quarantined.
    ///
    /// # Errors
    /// Returns a storage or selection refusal.
    pub async fn custody_refresh_after_rejection(
        &self,
        slot: &CustodySlot,
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<AccountFacts, CustodyError> {
        local::after_rejection(self, slot, rejected, selection_epoch_digest).await
    }
}

impl LocalCustody for CodexSubscriptionCredentialManager {
    type Selection = ();

    async fn snapshot_once(&self, slot: &CustodySlot) -> Result<Snapshot<()>, CustodyError> {
        let store = Arc::clone(self.store().map_err(store_error)?);
        let lifecycle = run_blocking(move || local_credential_status(&store))
            .await
            .map_err(store_error)?;
        let mut facts = AccountFacts::signed_out(slot.clone());
        facts.status = CustodyStatus::from_lifecycle(lifecycle);
        let fence = match self.load_fence().await {
            Ok(fence) => fence,
            Err(NativeCodexAuthError::Unavailable) => return Ok((facts, None)),
            Err(error) => return Err(store_error(error)),
        };
        let account = codex_account_digest(Some(fence.account_binding()));
        facts.generation = Some(fence.generation());
        facts.account_binding = Some(identity(account));
        facts.credential_authority = Some(identity(self.authority_binding));
        if facts.status != CustodyStatus::Ready {
            return Ok((facts, Some((fence, ()))));
        }
        let lease = self
            .acquire_ready_lease(fence.clone())
            .await
            .map_err(|_| local::busy())?;
        let stored = parse_stored(lease.payload()).map_err(store_error)?;
        facts.access_expires_at_unix_ms = Some(stored.access_expires_at.saturating_mul(1000));
        let epoch =
            credential_selection_epoch(account, self.authority_binding, &stored.selection_epoch)
                .map_err(|_| CustodyError::Internal)?;
        facts.selection_epoch_digest = Some(identity(epoch));
        Ok((facts, Some((fence, ()))))
    }

    fn retry_snapshot(error: &CustodyError) -> bool {
        matches!(error, CustodyError::Busy { .. })
    }

    fn profile_active(&self) -> bool {
        self.validate_profile_admission().is_ok()
    }

    async fn fields_lease(
        &self,
        fence: CredentialFence,
    ) -> Result<Option<ReadyCredentialLease>, CustodyError> {
        #[cfg(test)]
        {
            let account = fence.account_binding().clone();
            run_blocking(move || {
                run_before_lease(account.as_str());
                Ok(())
            })
            .await
            .map_err(store_error)?;
        }
        Ok(self.acquire_ready_lease(fence).await.ok())
    }

    fn fields_from_lease(lease: &ReadyCredentialLease) -> Result<CredentialFields, CustodyError> {
        let fence = lease.fence();
        let stored = parse_stored(lease.payload()).map_err(store_error)?;
        if stored.account_id != fence.account_binding().as_str() {
            return Err(CustodyError::AccountChanged);
        }
        let mut fields = CredentialFields::new();
        fields.push(
            "authorization",
            CredentialField::bearer(stored.access_token.as_bytes()),
        );
        fields.push(
            "chatgpt-account-id",
            CredentialField::exact(stored.account_id.as_bytes().to_vec()),
        );
        Ok(fields)
    }

    async fn refresh_rejected(
        &self,
        facts: &AccountFacts,
        (fence, ()): &(CredentialFence, ()),
        rejected: &ExpectedFence,
        selection: Option<IdentityDigest>,
    ) -> Result<bool, CustodyError> {
        let Some(selection) = selection.or(facts.selection_epoch_digest) else {
            return Ok(false);
        };
        if facts
            .selection_epoch_digest
            .is_some_and(|current| current != selection)
        {
            return Err(CustodyError::FenceMoved {
                facts: Box::new(facts.clone()),
            });
        }
        let selection =
            Digest::from_sha256_bytes(DigestDomain::ModelBinding, 2, *selection.as_bytes())
                .map_err(|_| CustodyError::Internal)?;
        let rejected = CredentialFence::new(
            fence.profile_id().clone(),
            fence.account_binding().clone(),
            rejected.generation,
            CredentialStatus::Ready,
        );
        if let Err(error @ (NativeCodexAuthError::Store | NativeCodexAuthError::StoreBusy)) =
            self.refresh_rejected_selection(&rejected, selection).await
        {
            return Err(store_error(error));
        }
        Ok(true)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test fixture's store write")]
impl CodexSubscriptionCredentialManager {
    /// Commits a refresh of the stored login between the next fields read's
    /// facts and its Ready lease.
    pub(super) fn refresh_between_custody_reads(&self) -> Result<(), NativeCodexAuthError> {
        let store = Arc::clone(self.store()?);
        let fence = store
            .load()
            .map_err(|_| NativeCodexAuthError::Store)?
            .ok_or(NativeCodexAuthError::Unavailable)?;
        install_before_lease(
            fence.account_binding().as_str(),
            Box::new(move || advance_ready_unchanged(&store)),
        );
        Ok(())
    }
}
