//! Local custody of the stored SuperGrok login (spec 01 §7.1): the facts a
//! caller binds to, the credential fields under the exact Ready lease, and
//! the single writer's refresh after a rejection.

use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CredentialHold, CustodyError, CustodySlot,
    CustodyStatus, ExpectedFence, IdentityDigest,
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

/// How often a facts read retries when the record moves under it.
const SNAPSHOT_ATTEMPTS: usize = 3;

/// How often a fields read judges the record when it moves between the
/// facts read and the Ready lease.
const FENCE_READS: usize = 2;

/// How long a refresh after a rejection waits for another caller's claimed
/// refresh to settle before it judges the record, and how often it looks.
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

fn store_error(_: XaiNativeAuthError) -> CustodyError {
    CustodyError::Store
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
        Ok(self.custody_snapshot(slot).await?.0)
    }

    /// The snapshot, read again when the record moved between its reads
    /// (a concurrent refresh), at most [`SNAPSHOT_ATTEMPTS`] times.
    async fn custody_snapshot(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<(CredentialFence, String)>), CustodyError> {
        let mut attempts = 1;
        loop {
            match self.custody_snapshot_once(slot).await {
                Err(CustodyError::Store) if attempts < SNAPSHOT_ATTEMPTS => attempts += 1,
                result => return result,
            }
        }
    }

    async fn custody_snapshot_once(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<(CredentialFence, String)>), CustodyError> {
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
            let (fence, _) = fence.ok_or(CustodyError::Internal)?;
            #[cfg(test)]
            between_reads::run(fence.account_binding().as_str());
            let core = Arc::clone(&self.core);
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
        Ok(fields.holding(CredentialHold::new(lease)))
    }

    /// The single writer's refresh after the provider answered `rejected`
    /// with HTTP 401: one claimed refresh, only while the store still holds
    /// that generation of the same account and login selection, then the
    /// current facts. A generation another caller already rotated is
    /// refreshed only if due; another login is never refreshed. A failed
    /// refresh is reported through the facts' status.
    ///
    /// # Errors
    /// [`CustodyError::Store`] for unsafe or unreadable storage.
    pub async fn custody_refresh_after_rejection(
        &self,
        slot: &CustodySlot,
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<AccountFacts, CustodyError> {
        let (facts, current) = self.settled_snapshot(slot).await?;
        let Some((fence, epoch)) = current else {
            return Ok(facts);
        };
        if facts.status != CustodyStatus::Ready
            || facts.account_binding != Some(rejected.account_binding)
        {
            return Ok(facts);
        }
        if selection_epoch_digest
            .is_some_and(|selection| facts.selection_epoch_digest != Some(selection))
        {
            return Err(CustodyError::FenceMoved {
                facts: Box::new(facts),
            });
        }
        let authority = self.core.authority_id().map_err(store_error)?;
        if let Err(XaiNativeAuthError::Store(_)) = self
            .core
            .refresh_with_force(
                Some(fence.account_binding()),
                Some(&epoch),
                Some(&authority),
                RefreshForce::WhileCurrent(rejected.generation),
            )
            .await
        {
            return Err(CustodyError::Store);
        }
        Ok(self.settled_snapshot(slot).await?.0)
    }

    /// The snapshot once no claimed refresh is in flight, waiting at most
    /// [`SETTLE_WAIT`].
    async fn settled_snapshot(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<(CredentialFence, String)>), CustodyError> {
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
