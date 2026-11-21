//! Local custody of the stored SuperGrok login (spec 01 §7.1): the facts a
//! caller binds to, the credential fields under the exact Ready lease, and
//! the single writer's refresh after a rejection.

use plakous_auth::custody::local::{LeaseSource, LocalCustody, Snapshot};
use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};
use plakous_auth::{PrivateCredentialStore, PrivateStoreError, ReadyCredentialLease};
#[cfg(test)]
use plakous_test_support::native_auth::custody::{LeaseRead, run_before_lease};

use super::{
    Arc, CredentialFence, RefreshForce, StoredTokens, XaiNativeAuthError, XaiNativeAuthManager,
};

pub(super) const SELECTION_DOMAIN: &[u8] = b"plak.custody.grok-selection.v1";

/// The wire-identity key of the login's raw selection epoch, which a
/// SuperGrok route binds a session to (non-secret identity).
pub const SUPERGROK_SELECTION_EPOCH_IDENTITY: &str = "grok_selection_epoch";
/// The wire-identity key of the account subject the catalog and billing
/// reads carry as `x-userid` (non-secret identity).
pub const SUPERGROK_SUBJECT_IDENTITY: &str = "grok_subject";
/// The wire-identity key of the route credential-authority id the custodian
/// computed over its own store, which a SuperGrok route binds a session to
/// (non-secret identity).
pub const SUPERGROK_AUTHORITY_ID_IDENTITY: &str = "grok_authority_id";

fn store_error(error: XaiNativeAuthError) -> CustodyError {
    match error {
        XaiNativeAuthError::Store(error) => error.into(),
        _ => CustodyError::Store,
    }
}

/// The account and credential-authority bindings the SuperGrok route binds a
/// session to, for a stored `account` under its route `authority`.
pub(super) fn grok_subscription_bindings(
    account: &str,
    authority: &str,
) -> Option<(plakous_model::Digest, plakous_model::Digest)> {
    let identity = crate::XaiRouteIdentity::new(
        crate::XaiRouteKind::GrokSubscription,
        crate::XaiOrigin::new(crate::SUPERGROK_RESPONSES_ORIGIN).ok()?,
        crate::XaiAccessMode::GrokSubscriptionOAuth,
        &crate::XaiCredentialBinding::grok_subscription(account, authority).ok()?,
    )
    .ok()?;
    Some((
        identity.account_binding(),
        identity.credential_authority_binding(),
    ))
}

pub(super) fn identity(digest: plakous_model::Digest) -> IdentityDigest {
    IdentityDigest::from_bytes(*digest.bytes())
}

impl LocalCustody for XaiNativeAuthManager {
    async fn snapshot_once(&self, slot: &CustodySlot) -> Result<Snapshot, CustodyError> {
        let core = Arc::clone(&self.core);
        let read = tokio::task::spawn_blocking(move || {
            let store = core.store()?;
            let lifecycle = store.offline_lifecycle()?;
            let ready = lifecycle == plakous_auth::OfflineCredentialLifecycle::Ready;
            let Some(fence) = store.load()? else {
                if ready {
                    return Err(PrivateStoreError::RecordMissing.into());
                }
                return Ok::<_, XaiNativeAuthError>((lifecycle, None));
            };
            let stored = ready
                .then(|| {
                    #[cfg(test)]
                    run_before_lease(store, LeaseRead::Snapshot);
                    core.read_stored(&fence)
                })
                .transpose();
            Ok((lifecycle, Some((fence, stored))))
        })
        .await
        .map_err(|_| CustodyError::Internal)?
        .map_err(store_error)?;
        let (lifecycle, fence) = read;
        let mut facts = AccountFacts::signed_out(slot.clone());
        facts.status = CustodyStatus::from_lifecycle(lifecycle);
        let Some((fence, stored)) = fence else {
            return Ok((facts, None));
        };
        facts.generation = Some(fence.generation());
        let stored = match stored {
            Ok(Some(stored)) => stored,
            Ok(None) => return Ok((facts, None)),
            Err(
                XaiNativeAuthError::StoredCredentialFormatRetired
                | XaiNativeAuthError::InvalidStoredCredential,
            ) => {
                facts.status = CustodyStatus::LoginRequired;
                return Ok((facts, None));
            }
            Err(error) => return Err(store_error(error)),
        };
        let authority = self.core.authority_id().map_err(store_error)?;
        let (account_binding, authority_binding) =
            grok_subscription_bindings(fence.account_binding().as_str(), &authority)
                .ok_or(CustodyError::Internal)?;
        facts.account_binding = Some(identity(account_binding));
        facts.credential_authority = Some(identity(authority_binding));
        facts.access_expires_at_unix_ms = Some(stored.expires_at.saturating_mul(1000));
        facts.selection_epoch_digest = Some(IdentityDigest::derive(
            SELECTION_DOMAIN,
            &[authority.as_bytes(), stored.selection_epoch.as_bytes()],
        ));
        facts.wire_identity.insert(
            SUPERGROK_SELECTION_EPOCH_IDENTITY.to_owned(),
            stored.selection_epoch.clone(),
        );
        facts.wire_identity.insert(
            SUPERGROK_SUBJECT_IDENTITY.to_owned(),
            fence.account_binding().as_str().to_owned(),
        );
        facts
            .wire_identity
            .insert(SUPERGROK_AUTHORITY_ID_IDENTITY.to_owned(), authority);
        Ok((facts, Some(fence)))
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
        stored
            .validate(lease.fence().account_binding())
            .map_err(|_| CustodyError::AccountChanged)?;
        let mut fields = CredentialFields::new();
        fields.push(
            "authorization",
            CredentialField::bearer(stored.access_token.as_bytes()),
        );
        fields.push(
            "x-userid",
            CredentialField::exact(stored.subject.as_bytes().to_vec()),
        );
        Ok(fields)
    }

    async fn refresh_rejected(
        &self,
        facts: &AccountFacts,
        fence: &CredentialFence,
        rejected: &ExpectedFence,
        _selection: IdentityDigest,
    ) -> Result<(), CustodyError> {
        let epoch = facts
            .wire_identity
            .get(SUPERGROK_SELECTION_EPOCH_IDENTITY)
            .ok_or(CustodyError::Internal)?;
        let authority = facts
            .wire_identity
            .get(SUPERGROK_AUTHORITY_ID_IDENTITY)
            .ok_or(CustodyError::Internal)?;
        if let Err(error @ XaiNativeAuthError::Store(_)) = self
            .core
            .refresh_with_force(
                Some(fence.account_binding()),
                Some(epoch),
                Some(authority),
                RefreshForce::WhileCurrent(rejected.generation),
            )
            .await
        {
            return Err(store_error(error));
        }
        Ok(())
    }
}
