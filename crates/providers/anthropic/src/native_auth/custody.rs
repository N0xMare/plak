//! Local custody of the stored Claude login (spec 01 §7.1): the facts a
//! caller binds to, the credential field under the exact Ready lease, and the
//! single writer's refresh after a rejection.

use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CredentialHold, CustodyError, CustodySlot,
    CustodyStatus, ExpectedFence, IdentityDigest,
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

/// How often a facts read retries when the record moves under it.
const SNAPSHOT_ATTEMPTS: usize = 3;

/// How often a fields read judges the record when it moves between the
/// facts read and the Ready lease.
const FENCE_READS: usize = 2;

/// How long a refresh after a rejection waits for another caller's claimed
/// refresh to settle before it judges the record, and how often it looks.
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Whether a snapshot may still hydrate a Ready login's missing account UUID.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Hydration {
    Once,
    Done,
}

fn store_error(_: ClaudeNativeAuthError) -> CustodyError {
    CustodyError::Store
}

fn identity(digest: plakous_model::Digest) -> IdentityDigest {
    IdentityDigest::from_bytes(*digest.bytes())
}

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
        Ok(self.custody_snapshot(slot).await?.0)
    }

    /// The snapshot, read again when the record moved between its reads
    /// (a concurrent refresh), at most [`SNAPSHOT_ATTEMPTS`] times.
    async fn custody_snapshot(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<CredentialFence>), CustodyError> {
        self.custody_snapshot_with(slot, Hydration::Once).await
    }

    async fn custody_snapshot_with(
        &self,
        slot: &CustodySlot,
        hydration: Hydration,
    ) -> Result<(AccountFacts, Option<CredentialFence>), CustodyError> {
        let mut attempts = 1;
        loop {
            match self.custody_snapshot_once(slot, hydration).await {
                Err(CustodyError::Store) if attempts < SNAPSHOT_ATTEMPTS => attempts += 1,
                result => return result,
            }
        }
    }

    async fn custody_snapshot_once(
        &self,
        slot: &CustodySlot,
        hydration: Hydration,
    ) -> Result<(AccountFacts, Option<CredentialFence>), CustodyError> {
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
            return Box::pin(self.custody_snapshot_with(slot, Hydration::Done)).await;
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
        tokio::task::spawn_blocking(move || core.read_stored(&fence))
            .await
            .map_err(|_| CustodyError::Internal)?
            .map_err(store_error)
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
        let mut reads = 1;
        let lease = loop {
            // A record that moved after the first read is judged again once
            // any claimed refresh settles, so a refusal names current facts.
            let (facts, fence) = if reads == 1 {
                self.custody_snapshot(slot).await?
            } else {
                self.settled_snapshot(slot).await?
            };
            facts.check_fence(expected)?;
            if self.core.ensure_active().is_err() {
                return Err(CustodyError::NotReady {
                    facts: Box::new(facts),
                });
            }
            let fence = fence.ok_or(CustodyError::Internal)?;
            #[cfg(test)]
            between_reads::run(fence.account_binding().as_str());
            let core = std::sync::Arc::clone(&self.core);
            let leased = tokio::task::spawn_blocking(move || {
                let lease = core.store().ok()?.acquire_ready(&fence).ok()?;
                (lease.fence() == fence).then_some(lease)
            })
            .await
            .map_err(|_| CustodyError::Internal)?;
            match leased {
                Some(lease) => break lease,
                None if reads < FENCE_READS => reads += 1,
                None => {
                    return Err(CustodyError::Busy {
                        retry_after_ms: 100,
                    });
                }
            }
        };
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
        Ok(fields.holding(CredentialHold::new(lease)))
    }

    /// The single writer's refresh after the provider answered `rejected`
    /// with HTTP 401: one claimed refresh, only while the store still holds
    /// that generation of the same account and login grant, then the current
    /// facts. A generation another caller already rotated is refreshed only
    /// if due; another login is never refreshed. A failed refresh is reported
    /// through the facts' status.
    ///
    /// # Errors
    /// [`CustodyError::Store`] for unsafe or unreadable storage.
    pub async fn custody_refresh_after_rejection(
        &self,
        slot: &CustodySlot,
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<AccountFacts, CustodyError> {
        let facts = self.settled_facts(slot).await?;
        if facts.status != CustodyStatus::Ready
            || facts.account_binding != Some(rejected.account_binding)
            || selection_epoch_digest
                .is_some_and(|selection| facts.selection_epoch_digest != Some(selection))
        {
            return Ok(facts);
        }
        if let Err(ClaudeNativeAuthError::Store(_)) = self
            .refresh_with_force(RefreshForce::WhileCurrent(rejected.generation))
            .await
        {
            return Err(CustodyError::Store);
        }
        self.settled_facts(slot).await
    }

    /// The facts once no claimed refresh is in flight, waiting at most
    /// [`SETTLE_WAIT`].
    async fn settled_facts(&self, slot: &CustodySlot) -> Result<AccountFacts, CustodyError> {
        Ok(self.settled_snapshot(slot).await?.0)
    }

    /// The snapshot once no claimed refresh is in flight, waiting at most
    /// [`SETTLE_WAIT`].
    async fn settled_snapshot(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<CredentialFence>), CustodyError> {
        let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
        loop {
            let snapshot = self.custody_snapshot(slot).await?;
            if snapshot.0.status != CustodyStatus::Refreshing
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(snapshot);
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
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
