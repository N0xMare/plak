//! Local custody of the stored Codex login (spec 01 §7.1): the facts a caller
//! binds to, the credential fields under the exact Ready lease, and the
//! single writer's refresh after a rejection.

use plakous_auth::ReadyCredentialLease;
use plakous_auth::custody::local::{LeaseSource, LocalCustody, Snapshot};
use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};
#[cfg(test)]
use plakous_test_support::native_auth::custody::{LeaseRead, run_before_lease};

use super::manager::codex_account_digest;
use super::{
    Arc, CodexSubscriptionCredentialManager, CredentialFence, CredentialStatus, Digest,
    DigestDomain, NativeCodexAuthError, PrivateCredentialStore, PrivateStoreError,
    credential_selection_epoch, local_credential_status, parse_stored, run_blocking,
};

fn identity(digest: Digest) -> IdentityDigest {
    IdentityDigest::from_bytes(*digest.bytes())
}

fn store_error(error: NativeCodexAuthError) -> CustodyError {
    match error {
        NativeCodexAuthError::StoreBusy => PrivateStoreError::StoreBusy.into(),
        _ => CustodyError::Store,
    }
}

impl LocalCustody for CodexSubscriptionCredentialManager {
    async fn snapshot_once(&self, slot: &CustodySlot) -> Result<Snapshot, CustodyError> {
        let store = self.store().map_err(store_error)?;
        let status_store = Arc::clone(store);
        let lifecycle = run_blocking(move || local_credential_status(&status_store))
            .await
            .map_err(store_error)?;
        let mut facts = AccountFacts::signed_out(slot.clone());
        facts.status = CustodyStatus::from_lifecycle(lifecycle);
        let Some(fence) = store.on_disk(PrivateCredentialStore::load).await? else {
            if facts.status == CustodyStatus::Ready {
                return Err(PrivateStoreError::RecordMissing.into());
            }
            return Ok((facts, None));
        };
        let account = codex_account_digest(Some(fence.account_binding()));
        facts.generation = Some(fence.generation());
        facts.account_binding = Some(identity(account));
        facts.credential_authority = Some(identity(self.authority_binding));
        if facts.status != CustodyStatus::Ready {
            return Ok((facts, Some(fence)));
        }
        let expected = fence.clone();
        let lease = store
            .on_disk(move |store| {
                #[cfg(test)]
                run_before_lease(store, LeaseRead::Snapshot);
                store.acquire_ready(&expected)
            })
            .await?;
        let stored = parse_stored(lease.payload()).map_err(store_error)?;
        facts.access_expires_at_unix_ms = Some(stored.access_expires_at.saturating_mul(1000));
        let epoch =
            credential_selection_epoch(account, self.authority_binding, &stored.selection_epoch)
                .map_err(|_| CustodyError::Internal)?;
        facts.selection_epoch_digest = Some(identity(epoch));
        Ok((facts, Some(fence)))
    }

    fn profile_active(&self) -> bool {
        self.validate_profile_admission().is_ok()
    }

    fn lease_source(&self) -> Option<LeaseSource<'_>> {
        Some(LeaseSource::new(self.store().ok()?))
    }

    #[cfg(test)]
    fn before_fields_lease(store: &PrivateCredentialStore) {
        run_before_lease(store, LeaseRead::Fields);
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
        _facts: &AccountFacts,
        fence: &CredentialFence,
        rejected: &ExpectedFence,
        selection: IdentityDigest,
    ) -> Result<(), CustodyError> {
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
        Ok(())
    }
}
