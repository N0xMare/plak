//! Local custody of the stored SuperGrok login (spec 01 §7.1): the facts a
//! caller binds to, the credential fields under the exact Ready lease, and
//! the single writer's refresh after a rejection.

use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CredentialHold, CustodyError, CustodySlot,
    CustodyStatus, ExpectedFence, IdentityDigest,
};

use super::{
    Arc, CredentialFence, RefreshForce, StoredTokens, XaiNativeAuthError, XaiNativeAuthManager,
    route_credential_authority_id,
};

const SELECTION_DOMAIN: &[u8] = b"plak.custody.grok-selection.v1";

/// How long a refresh after a rejection waits for another caller's claimed
/// refresh to settle before it judges the record, and how often it looks.
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

fn store_error(_: XaiNativeAuthError) -> CustodyError {
    CustodyError::Store
}

/// The account and credential-authority bindings the SuperGrok route binds a
/// session to, for a stored `account` under its route `authority`.
fn grok_subscription_bindings(
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

fn identity(digest: plakous_model::Digest) -> IdentityDigest {
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

    async fn custody_snapshot(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<(CredentialFence, String)>), CustodyError> {
        let core = Arc::clone(&self.core);
        let read = tokio::task::spawn_blocking(move || {
            let lifecycle = core.store.offline_lifecycle()?;
            let Some(fence) = core.store.load()? else {
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
        let authority =
            route_credential_authority_id(&self.core.store, self.core.source_scope.as_deref());
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
        let (facts, fence) = self.custody_snapshot(slot).await?;
        facts.check_fence(expected)?;
        if self.core.ensure_active().is_err() {
            return Err(CustodyError::NotReady {
                facts: Box::new(facts),
            });
        }
        let (fence, _) = fence.ok_or(CustodyError::Internal)?;
        let core = Arc::clone(&self.core);
        let leased = tokio::task::spawn_blocking(move || {
            let lease = core.store.acquire_ready(&fence).ok()?;
            (lease.fence() == fence).then_some(lease)
        })
        .await
        .map_err(|_| CustodyError::Internal)?;
        let Some(lease) = leased else {
            return Err(CustodyError::FenceMoved {
                facts: Box::new(facts),
            });
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
            || selection_epoch_digest
                .is_some_and(|selection| facts.selection_epoch_digest != Some(selection))
        {
            return Ok(facts);
        }
        let authority =
            route_credential_authority_id(&self.core.store, self.core.source_scope.as_deref());
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
