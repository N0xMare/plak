//! Local custody of the stored Claude login (spec 01 §7.1): the facts a
//! caller binds to, the credential field under the exact Ready lease, and the
//! single writer's refresh after a rejection.

use plakous_auth::ReadyCredentialLease;
use plakous_auth::custody::local::{self, LocalCustody, Snapshot};
use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};

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
        ClaudeNativeAuthError::Store(error) => local::store_error(&error),
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
    /// Non-secret facts of the stored login, reported for `slot`.
    ///
    /// The identity digests are the exact account and credential-authority
    /// bindings (model-binding digests, version 1) a Claude route binds a
    /// session to; the selection digest names the login grant, so a refresh
    /// keeps it and a new login changes it. The account UUID and the client
    /// device id travel as wire identity. A Ready login whose account UUID
    /// is not yet stored is hydrated first, as resolution does. The profile
    /// is the caller's to report.
    ///
    /// # Errors
    /// [`CustodyError::Store`] for unsafe or unreadable storage.
    pub async fn custody_facts(&self, slot: &CustodySlot) -> Result<AccountFacts, CustodyError> {
        local::facts(self, slot).await
    }

    /// The credential field of one request prepared against `expected`:
    /// `authorization`, read under the exact Ready lease it came from, which
    /// it holds until the caller drops it.
    ///
    /// # Errors
    /// [`AccountFacts::check_fence`]'s refusals.
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
    /// Returns a storage refusal.
    pub async fn custody_refresh_after_rejection(
        &self,
        slot: &CustodySlot,
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<AccountFacts, CustodyError> {
        local::after_rejection(self, slot, rejected, selection_epoch_digest).await
    }

    async fn custody_snapshot_once(
        &self,
        slot: &CustodySlot,
        hydration: Hydration,
    ) -> Result<Snapshot<()>, CustodyError> {
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
            return Ok((facts, None));
        };
        facts.generation = Some(fence.generation());
        if facts.status != CustodyStatus::Ready {
            return Ok((facts, Some((fence, ()))));
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
            return Box::pin(local::retry_snapshot(
                || self.custody_snapshot_once(slot, Hydration::Done),
                Self::retry_snapshot,
            ))
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
        Ok((facts, Some((fence, ()))))
    }

    async fn custody_stored(&self, fence: &CredentialFence) -> Result<StoredTokens, CustodyError> {
        let core = std::sync::Arc::clone(&self.core);
        let fence = fence.clone();
        tokio::task::spawn_blocking(move || core.read_stored(&fence))
            .await
            .map_err(|_| CustodyError::Internal)?
            .map_err(store_error)
    }
}

impl LocalCustody for ClaudeNativeAuthManager {
    type Selection = ();

    async fn snapshot_once(&self, slot: &CustodySlot) -> Result<Snapshot<()>, CustodyError> {
        self.custody_snapshot_once(slot, Hydration::Once).await
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
        let core = std::sync::Arc::clone(&self.core);
        tokio::task::spawn_blocking(move || core.store().ok()?.acquire_ready(&fence).ok())
            .await
            .map_err(|_| CustodyError::Internal)
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
        facts: &AccountFacts,
        _current: &(CredentialFence, ()),
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<bool, CustodyError> {
        if facts.status != CustodyStatus::Ready
            || selection_epoch_digest
                .is_some_and(|selection| facts.selection_epoch_digest != Some(selection))
        {
            return Ok(false);
        }
        if let Err(error @ ClaudeNativeAuthError::Store(_)) = self
            .refresh_with_force(RefreshForce::WhileCurrent(rejected.generation))
            .await
        {
            return Err(store_error(error));
        }
        Ok(true)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test fixture's store write")]
impl ClaudeNativeAuthManager {
    /// Commits a refresh of the stored login between the next fields read's
    /// facts and its Ready lease.
    pub(crate) fn refresh_between_custody_reads(&self) -> Result<(), ClaudeNativeAuthError> {
        let fence = self
            .core
            .store()?
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        let core = std::sync::Arc::clone(&self.core);
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
