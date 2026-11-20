//! Local custody of the stored Codex login (spec 01 §7.1): the facts a caller
//! binds to, the credential fields under the exact Ready lease, and the
//! single writer's refresh after a rejection.

use plakous_auth::custody::{
    AccountFacts, CredentialField, CredentialFields, CredentialHold, CustodyError, CustodySlot,
    CustodyStatus, ExpectedFence, IdentityDigest,
};

use super::manager::codex_account_digest;
use super::{
    Arc, CodexSubscriptionCredentialManager, CredentialFence, CredentialStatus, Digest,
    DigestDomain, NativeCodexAuthError, credential_selection_epoch, local_credential_status,
    parse_stored, run_blocking,
};

/// How often a facts read retries when the record moves under it.
const SNAPSHOT_ATTEMPTS: usize = 3;

/// How often a fields read judges the record when it moves between the
/// facts read and the Ready lease.
const FENCE_READS: usize = 2;

/// How long a refresh after a rejection waits for another caller's claimed
/// refresh to settle before it judges the record, and how often it looks.
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

fn identity(digest: Digest) -> IdentityDigest {
    IdentityDigest::from_bytes(*digest.bytes())
}

fn store_error(error: NativeCodexAuthError) -> CustodyError {
    match error {
        NativeCodexAuthError::StoreBusy => CustodyError::Busy {
            retry_after_ms: 100,
        },
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
        Ok(self.custody_snapshot(slot).await?.0)
    }

    async fn custody_snapshot(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<CredentialFence>), CustodyError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
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
                return Ok((facts, Some(fence)));
            }
            let Ok(lease) = self.acquire_ready_lease(fence.clone()).await else {
                // The record moved between the two reads.
                if attempts < SNAPSHOT_ATTEMPTS {
                    continue;
                }
                return Err(CustodyError::Busy {
                    retry_after_ms: 100,
                });
            };
            let stored = parse_stored(lease.payload()).map_err(store_error)?;
            facts.access_expires_at_unix_ms = Some(stored.access_expires_at.saturating_mul(1000));
            let epoch = credential_selection_epoch(
                account,
                self.authority_binding,
                &stored.selection_epoch,
            )
            .map_err(|_| CustodyError::Internal)?;
            facts.selection_epoch_digest = Some(identity(epoch));
            return Ok((facts, Some(fence)));
        }
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
            if self.validate_profile_admission().is_err() {
                return Err(CustodyError::NotReady {
                    facts: Box::new(facts),
                });
            }
            let fence = fence.ok_or(CustodyError::Internal)?;
            #[cfg(test)]
            between_reads::run(fence.account_binding().as_str());
            match self.acquire_ready_lease(fence.clone()).await {
                Ok(lease) if lease.fence() == fence => break lease,
                _ if reads < FENCE_READS => reads += 1,
                _ => {
                    return Err(CustodyError::Busy {
                        retry_after_ms: 100,
                    });
                }
            }
        };
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
        Ok(fields.holding(CredentialHold::new(lease)))
    }

    /// The single writer's refresh after the provider answered `rejected`
    /// with HTTP 401: one claimed refresh, only while the store still holds
    /// that generation of the same account and login selection, then the
    /// current facts. A generation another caller already rotated is
    /// refreshed only if due, and a newer login is never claimed, rotated or
    /// quarantined. A failed refresh is reported through the facts' status.
    ///
    /// # Errors
    /// [`CustodyError::Store`] for unsafe or unreadable storage.
    pub async fn custody_refresh_after_rejection(
        &self,
        slot: &CustodySlot,
        rejected: &ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> Result<AccountFacts, CustodyError> {
        let (facts, fence) = self.settled_snapshot(slot).await?;
        let Some(fence) = fence else {
            return Ok(facts);
        };
        let Some(selection) = selection_epoch_digest.or(facts.selection_epoch_digest) else {
            return Ok(facts);
        };
        if facts.account_binding != Some(rejected.account_binding) {
            return Ok(facts);
        }
        if facts
            .selection_epoch_digest
            .is_some_and(|current| current != selection)
        {
            return Err(CustodyError::FenceMoved {
                facts: Box::new(facts),
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
        Ok(self.settled_snapshot(slot).await?.0)
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
        between_reads::install(
            fence.account_binding().as_str(),
            Box::new(move || between_reads::commit_refresh(&store)),
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
