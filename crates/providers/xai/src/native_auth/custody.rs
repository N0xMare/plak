//! Local custody of the stored SuperGrok login (spec 01 §7.1): the facts a
//! caller binds to, the credential fields under the exact Ready lease, and
//! the single writer's refresh after a rejection.

use plakous_auth::ReadyCredentialLease;
use plakous_auth::custody::local::{self, LocalCustody, Snapshot};
use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};

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
        XaiNativeAuthError::Store(error) => local::store_error(&error),
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

impl XaiNativeAuthManager {
    /// Non-secret facts of the stored login, reported for `slot`.
    ///
    /// The identity digests are the exact account and credential-authority
    /// bindings (model-binding digests, version 1) the SuperGrok route binds
    /// a session to; the selection digest names the login selection epoch,
    /// which a refresh keeps and a new login changes. The profile is the
    /// caller's to report.
    ///
    /// # Errors
    /// [`CustodyError::Store`] for unsafe or unreadable storage.
    pub async fn custody_facts(&self, slot: &CustodySlot) -> Result<AccountFacts, CustodyError> {
        local::facts(self, slot).await
    }

    /// The credential fields of one request prepared against `expected`:
    /// `authorization` and `x-userid`, read under the exact Ready lease they
    /// came from, which they hold until the caller drops it.
    ///
    /// # Errors
    /// [`AccountFacts::check_fence`]'s refusals, and
    /// [`CustodyError::NotReady`] outside the compatibility window.
    pub async fn custody_fields(
        &self,
        slot: &CustodySlot,
        expected: &ExpectedFence,
    ) -> Result<CredentialFields, CustodyError> {
        local::fields(self, slot, expected).await
    }

    /// The single writer's refresh after the provider answered `rejected`
    /// with HTTP 401, then the current facts. A generation another caller
    /// already rotated is refreshed only if due; another login is never refreshed.
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

impl LocalCustody for XaiNativeAuthManager {
    type Selection = String;

    async fn snapshot_once(&self, slot: &CustodySlot) -> Result<Snapshot<String>, CustodyError> {
        let core = Arc::clone(&self.core);
        let read = tokio::task::spawn_blocking(move || {
            let store = core.store()?;
            let lifecycle = store.offline_lifecycle()?;
            let Some(fence) = store.load()? else {
                return Ok::<_, XaiNativeAuthError>((lifecycle, None));
            };
            let stored = (lifecycle == plakous_auth::OfflineCredentialLifecycle::Ready)
                .then(|| core.read_stored(&fence))
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
        let epoch = stored.selection_epoch.clone();
        Ok((facts, Some((fence, epoch))))
    }

    fn profile_active(&self) -> bool {
        self.core.ensure_active().is_ok()
    }

    async fn fields_lease(
        &self,
        fence: CredentialFence,
    ) -> Result<Option<ReadyCredentialLease>, CustodyError> {
        #[cfg(test)]
        between_reads::run(fence.account_binding().as_str());
        let core = Arc::clone(&self.core);
        tokio::task::spawn_blocking(move || core.store().ok()?.acquire_ready(&fence).ok())
            .await
            .map_err(|_| CustodyError::Internal)
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
        (fence, epoch): &(CredentialFence, String),
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<bool, CustodyError> {
        if facts.status != CustodyStatus::Ready {
            return Ok(false);
        }
        if selection_epoch_digest
            .is_some_and(|selection| facts.selection_epoch_digest != Some(selection))
        {
            return Err(CustodyError::FenceMoved {
                facts: Box::new(facts.clone()),
            });
        }
        let authority = self.core.authority_id().map_err(store_error)?;
        if let Err(error @ XaiNativeAuthError::Store(_)) = self
            .core
            .refresh_with_force(
                Some(fence.account_binding()),
                Some(epoch),
                Some(&authority),
                RefreshForce::WhileCurrent(rejected.generation),
            )
            .await
        {
            return Err(store_error(error));
        }
        Ok(true)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test fixture's store write")]
impl XaiNativeAuthManager {
    /// Commits a refresh of the stored login between the next fields read's
    /// facts and its Ready lease.
    pub(super) fn refresh_between_custody_reads(&self) -> Result<(), XaiNativeAuthError> {
        let fence = self
            .core
            .store()?
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        let core = Arc::clone(&self.core);
        between_reads::install(
            fence.account_binding().as_str(),
            Box::new(move || {
                between_reads::commit_refresh(core.store().expect("a local store"));
            }),
        );
        Ok(())
    }
}

/// A test's action between a fields read's facts and its Ready lease, such
/// as committing a refresh there.
#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test fixture's store write")]
pub(super) mod between_reads {
    type Hook = Box<dyn FnOnce() + Send>;

    static HOOKS: std::sync::Mutex<Vec<(String, Hook)>> = std::sync::Mutex::new(Vec::new());

    /// Runs `hook` once, in the next fields read of `account`.
    pub(super) fn install(account: &str, hook: Hook) {
        HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((account.to_owned(), hook));
    }

    pub(super) fn run(account: &str) {
        let hook = {
            let mut hooks = HOOKS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hooks
                .iter()
                .position(|(owner, _)| owner == account)
                .map(|index| hooks.remove(index).1)
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Commits `store`'s next generation with its payload unchanged, as
    /// another holder's completed refresh leaves the record.
    pub(super) fn commit_refresh(store: &plakous_auth::PrivateCredentialStore) {
        let fence = store.load().ok().flatten().expect("a stored record");
        let payload = {
            let lease = store.acquire_ready(&fence).expect("a Ready record");
            plakous_auth::SecretBytes::new(lease.payload().expose_secret().to_vec())
        };
        let record = plakous_auth::CredentialRecord::new(
            fence.profile_id().clone(),
            fence.account_binding().clone(),
            fence.generation().next().expect("a next generation"),
            plakous_auth::CredentialStatus::Ready,
            payload,
        )
        .expect("a record");
        store
            .replace(fence.generation(), &record)
            .expect("the refresh commits");
    }
}
