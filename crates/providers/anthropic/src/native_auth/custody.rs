//! Local custody of the stored Claude login (spec 01 §7.1): the facts a
//! caller binds to, the credential field under the exact Ready lease, and the
//! single writer's refresh after a rejection.

use plakous_auth::custody::local::{self, LeaseSource, LocalCustody, Snapshot};
use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};
use plakous_auth::{PrivateCredentialStore, PrivateStoreError, ReadyCredentialLease};
#[cfg(test)]
use plakous_test_support::native_auth::custody::{LeaseRead, run_before_lease};

use super::refresh::RefreshForce;
use super::{
    ClaudeNativeAuthError, ClaudeNativeAuthManager, CredentialFence, StoredTokens,
    validate_current_ready_record,
};

/// The wire-identity keys a Claude request body carries (`metadata.user_id`).
pub const CLAUDE_ACCOUNT_UUID_IDENTITY: &str = "claude_account_uuid";
/// The per-install client device id key.
pub const CLAUDE_DEVICE_ID_IDENTITY: &str = "claude_device_id";

const GRANT_DOMAIN: &[u8] = b"plak.custody.claude-grant.v1";

/// Whether a snapshot may still hydrate a Ready login's missing account UUID.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Hydration {
    Once,
    Done,
}

fn store_error(error: ClaudeNativeAuthError) -> CustodyError {
    match error {
        ClaudeNativeAuthError::Store(plakous_auth::PrivateStoreError::ForeignAccount) => {
            CustodyError::AccountChanged
        }
        ClaudeNativeAuthError::Store(error) => error.into(),
        _ => CustodyError::Store,
    }
}

fn identity(digest: plakous_model::Digest) -> IdentityDigest {
    IdentityDigest::from_bytes(*digest.bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "fixture-controlled credential stores")]
mod revoke_tests;

impl ClaudeNativeAuthManager {
    async fn custody_snapshot_once(
        &self,
        slot: &CustodySlot,
        hydration: Hydration,
    ) -> Result<Snapshot, CustodyError> {
        let mut facts = AccountFacts::signed_out(slot.clone());
        let core = std::sync::Arc::clone(&self.core);
        let read = tokio::task::spawn_blocking(move || {
            let lifecycle = match validate_current_ready_record(core.store()?) {
                Ok(()) => CustodyStatus::from_lifecycle(core.store()?.offline_lifecycle()?),
                Err(ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild) => {
                    CustodyStatus::WrittenByNewerBuild
                }
                Err(
                    ClaudeNativeAuthError::StoredCredentialFormatRetired
                    | ClaudeNativeAuthError::InvalidStoredCredential,
                ) => CustodyStatus::LoginRequired,
                Err(error) => return Err(error),
            };
            Ok::<_, ClaudeNativeAuthError>((lifecycle, core.store()?.load()?))
        })
        .await
        .map_err(|_| CustodyError::Internal)?;
        let (status, fence) = read.map_err(store_error)?;
        facts.status = status;
        let Some(fence) = fence else {
            if status == CustodyStatus::Ready {
                return Err(PrivateStoreError::RecordMissing.into());
            }
            return Ok((facts, None));
        };
        facts.generation = Some(fence.generation());
        if facts.status != CustodyStatus::Ready {
            return Ok((facts, Some(fence)));
        }
        let mut stored = self.custody_stored(&fence).await?;
        if stored.account_uuid.is_none() {
            // Hydrate once, then read once more; a record that still names
            // no account is unavailable rather than hydrated again.
            if hydration == Hydration::Done {
                return Err(CustodyError::Unavailable);
            }
            self.ensure_account_identity()
                .await
                .map_err(|_| CustodyError::Unavailable)?;
            return Box::pin(local::retry_snapshot(|| {
                self.custody_snapshot_once(slot, Hydration::Done)
            }))
            .await;
        }
        let account = stored.account_uuid.take().ok_or(CustodyError::Internal)?;
        let (account_binding, authority) =
            crate::oauth_model::claude_route_bindings(&account, self.credential_source_scope())
                .map_err(|_| CustodyError::Internal)?;
        facts.account_binding = Some(identity(account_binding));
        facts.credential_authority = Some(identity(authority));
        facts.access_expires_at_unix_ms = Some(stored.expires_at.saturating_mul(1000));
        facts.selection_epoch_digest = Some(IdentityDigest::derive(
            GRANT_DOMAIN,
            &[fence.account_binding().as_str().as_bytes()],
        ));
        facts.wire_identity.insert(
            CLAUDE_ACCOUNT_UUID_IDENTITY.to_owned(),
            account.as_str().to_owned(),
        );
        facts.wire_identity.insert(
            CLAUDE_DEVICE_ID_IDENTITY.to_owned(),
            self.core.device_id.as_str().to_owned(),
        );
        Ok((facts, Some(fence)))
    }

    async fn custody_stored(&self, fence: &CredentialFence) -> Result<StoredTokens, CustodyError> {
        let core = std::sync::Arc::clone(&self.core);
        let fence = fence.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            run_before_lease(core.store()?, LeaseRead::Snapshot);
            core.read_stored(&fence)
        })
        .await
        .map_err(|_| CustodyError::Internal)?
        .map_err(store_error)
    }
}

impl LocalCustody for ClaudeNativeAuthManager {
    async fn snapshot_once(&self, slot: &CustodySlot) -> Result<Snapshot, CustodyError> {
        self.custody_snapshot_once(slot, Hydration::Once).await
    }

    fn profile_active(&self) -> bool {
        self.core.ensure_active().is_ok()
    }

    fn lease_source(&self) -> Option<LeaseSource<'_>> {
        self.core.store().ok().map(LeaseSource::new)
    }

    #[cfg(test)]
    fn before_fields_lease(store: &PrivateCredentialStore) {
        run_before_lease(store, LeaseRead::Fields);
    }

    fn fields_from_lease(lease: &ReadyCredentialLease) -> Result<CredentialFields, CustodyError> {
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| CustodyError::Store)?;
        if stored.grant_binding != lease.fence().account_binding().as_str() {
            return Err(CustodyError::AccountChanged);
        }
        let mut fields = CredentialFields::new();
        fields.push(
            "authorization",
            CredentialField::bearer(stored.access_token.as_bytes()),
        );
        Ok(fields)
    }

    async fn refresh_rejected(
        &self,
        _facts: &AccountFacts,
        current: &CredentialFence,
        rejected: &ExpectedFence,
        _selection: IdentityDigest,
    ) -> Result<(), CustodyError> {
        if let Err(error @ ClaudeNativeAuthError::Store(_)) = self
            .refresh_with_force(
                RefreshForce::WhileCurrent(rejected.generation),
                Some(current.account_binding()),
            )
            .await
        {
            return Err(store_error(error));
        }
        Ok(())
    }
}
