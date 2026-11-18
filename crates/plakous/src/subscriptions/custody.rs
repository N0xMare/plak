//! Local custody of the subscription logins under one store (spec 01 §7.1):
//! what a gateway serves for the Codex, Claude and SuperGrok kinds.

use plakous_auth::custody::{
    AccountFacts, Consent, CredentialCustodian, CredentialFields, CustodyError, CustodyFuture,
    CustodySlot, CustodyStatus, ExpectedFence, IdentityDigest, ProfileFacts, RefreshRequest,
};

use super::{
    Arc, CompatibilityStatus, NativeAuthStatus, NativeProvider, NativeSubscriptionError,
    NativeSubscriptions,
};

/// Implements [`LoginCustody`] for one provider manager over its custody methods.
macro_rules! login_custody {
    ($manager:ty) => {
        impl super::custody::LoginCustody for $manager {
            fn facts<'a>(
                &'a self,
                slot: &'a plakous_auth::custody::CustodySlot,
            ) -> plakous_auth::custody::CustodyFuture<'a, plakous_auth::custody::AccountFacts> {
                Box::pin(self.custody_facts(slot))
            }

            fn fields<'a>(
                &'a self,
                slot: &'a plakous_auth::custody::CustodySlot,
                expected: &'a plakous_auth::custody::ExpectedFence,
            ) -> plakous_auth::custody::CustodyFuture<'a, plakous_auth::custody::CredentialFields>
            {
                Box::pin(self.custody_fields(slot, expected))
            }

            fn refresh_after_rejection<'a>(
                &'a self,
                slot: &'a plakous_auth::custody::CustodySlot,
                rejected: &'a plakous_auth::custody::ExpectedFence,
                selection_epoch_digest: Option<plakous_auth::custody::IdentityDigest>,
            ) -> plakous_auth::custody::CustodyFuture<'a, plakous_auth::custody::AccountFacts> {
                Box::pin(self.custody_refresh_after_rejection(
                    slot,
                    rejected,
                    selection_epoch_digest,
                ))
            }
        }
    };
}
pub(super) use login_custody;

/// What a gateway needs of one opened provider login manager.
pub(super) trait LoginCustody: Send + Sync {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts>;

    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields>;

    fn refresh_after_rejection<'a>(
        &'a self,
        slot: &'a CustodySlot,
        rejected: &'a ExpectedFence,
        selection_epoch_digest: Option<IdentityDigest>,
    ) -> CustodyFuture<'a, AccountFacts>;
}

/// Local custody of one subscription kind's logins under a store: the
/// default account and its named accounts (`<kind>/<name>`).
///
/// Every call opens the slot's login exactly as the library's own
/// operations do, so consent, profile windows and newer-build refusals
/// apply unchanged; the facts' identity digests are the bytes a route binds
/// a session to. Refresh is the single writer's: `IfDue` runs only while the
/// caller's fence is current, `AfterRejection` claims one grant only while
/// the store still holds the rejected generation, and `Operator` forces one.
#[derive(Clone)]
pub struct SubscriptionCustodian {
    subscriptions: NativeSubscriptions,
    provider: NativeProvider,
    /// One refresh at a time per account in this process; other processes
    /// serialize on the store's refresh claim.
    refreshing: Arc<
        std::sync::Mutex<std::collections::BTreeMap<String, Arc<futures_util::lock::Mutex<()>>>>,
    >,
}

impl std::fmt::Debug for SubscriptionCustodian {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionCustodian")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl SubscriptionCustodian {
    /// Custody of `provider`'s logins under `subscriptions`' store.
    #[must_use]
    pub fn new(subscriptions: NativeSubscriptions, provider: NativeProvider) -> Self {
        Self {
            subscriptions,
            provider,
            refreshing: Arc::default(),
        }
    }

    fn refresh_lock(&self, slot: &CustodySlot) -> Arc<futures_util::lock::Mutex<()>> {
        Arc::clone(
            self.refreshing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(slot.as_str().to_owned())
                .or_default(),
        )
    }

    fn account(&self, slot: &CustodySlot) -> Result<NativeSubscriptions, CustodyError> {
        match slot.name() {
            None => Ok(self.subscriptions.clone()),
            Some(name) => self
                .subscriptions
                .for_named_account(self.provider, name)
                .map_err(|_| CustodyError::UnknownAccount),
        }
    }

    async fn login(
        &self,
        subscriptions: &NativeSubscriptions,
    ) -> Result<Arc<dyn LoginCustody>, CustodyError> {
        super::vertical::require_vertical(self.provider)
            .map_err(|_| CustodyError::UnknownAccount)?
            .login_custody(subscriptions)
            .await
            .map_err(|error| custody_error(&error))
    }

    /// The facts with the profile, from the login when consent is current
    /// and otherwise from the offline status alone.
    async fn current(&self, slot: &CustodySlot) -> Result<AccountFacts, CustodyError> {
        let subscriptions = self.account(slot)?;
        let status = subscriptions
            .status(self.provider)
            .await
            .map_err(|error| custody_error(&error))?;
        let mut facts = if status.compatibility() == CompatibilityStatus::Current
            && status.credential() != NativeAuthStatus::SignedOut
        {
            self.login(&subscriptions).await?.facts(slot).await?
        } else {
            let mut facts = AccountFacts::signed_out(slot.clone());
            facts.status = auth_status(status.credential());
            facts
        };
        facts.profile = Some(self.profile(status.compatibility())?);
        Ok(facts)
    }

    fn profile(&self, compatibility: CompatibilityStatus) -> Result<ProfileFacts, CustodyError> {
        let notice =
            NativeSubscriptions::notice(self.provider).map_err(|error| custody_error(&error))?;
        Ok(ProfileFacts {
            id: notice.profile_id.to_owned(),
            contract_revision: notice.notice_revision.to_owned(),
            consent: match compatibility {
                CompatibilityStatus::Current => Consent::Current,
                CompatibilityStatus::Required => Consent::Required,
                CompatibilityStatus::Inactive => Consent::Inactive,
            },
            review_expires_at_unix_ms: Some(notice.expires_at.saturating_mul(1000)),
        })
    }

    async fn with_profile(&self, mut facts: AccountFacts) -> Result<AccountFacts, CustodyError> {
        let subscriptions = self.account(&facts.slot)?;
        let compatibility = subscriptions
            .status(self.provider)
            .await
            .map_err(|error| custody_error(&error))?
            .compatibility();
        facts.profile = Some(self.profile(compatibility)?);
        Ok(facts)
    }
}

impl CredentialCustodian for SubscriptionCustodian {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(self.current(slot))
    }

    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let subscriptions = self.account(slot)?;
            let lock = self.refresh_lock(slot);
            let _refreshing = lock.lock().await;
            match request {
                RefreshRequest::IfDue { expected } => {
                    if self.current(slot).await?.check_fence(&expected).is_ok() {
                        // A failed refresh shows in the facts' status.
                        let _ = subscriptions.refresh(self.provider).await;
                    }
                    self.current(slot).await
                }
                RefreshRequest::AfterRejection {
                    rejected,
                    selection_epoch_digest,
                } => {
                    let facts = self
                        .login(&subscriptions)
                        .await?
                        .refresh_after_rejection(slot, &rejected, selection_epoch_digest)
                        .await?;
                    self.with_profile(facts).await
                }
                RefreshRequest::Operator => {
                    subscriptions
                        .refresh_now(self.provider)
                        .await
                        .map_err(|error| custody_error(&error))?;
                    self.current(slot).await
                }
            }
        })
    }

    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields> {
        Box::pin(async move {
            let subscriptions = self.account(slot)?;
            let status = subscriptions
                .status(self.provider)
                .await
                .map_err(|error| custody_error(&error))?;
            if status.compatibility() != CompatibilityStatus::Current {
                return Err(CustodyError::NotReady {
                    facts: Box::new(self.current(slot).await?),
                });
            }
            self.login(&subscriptions)
                .await?
                .fields(slot, expected)
                .await
        })
    }
}

const fn auth_status(status: NativeAuthStatus) -> CustodyStatus {
    match status {
        NativeAuthStatus::SignedOut => CustodyStatus::SignedOut,
        NativeAuthStatus::Ready => CustodyStatus::Ready,
        NativeAuthStatus::RefreshRequired => CustodyStatus::RefreshRequired,
        NativeAuthStatus::Refreshing => CustodyStatus::Refreshing,
        NativeAuthStatus::Revoked => CustodyStatus::Revoked,
        NativeAuthStatus::LoginRequired => CustodyStatus::LoginRequired,
        NativeAuthStatus::WrittenByNewerBuild => CustodyStatus::WrittenByNewerBuild,
        NativeAuthStatus::Quarantined => CustodyStatus::Quarantined,
    }
}

/// A library refusal as the custody refusal a gateway answers with.
fn custody_error(error: &NativeSubscriptionError) -> CustodyError {
    match error {
        NativeSubscriptionError::NotCompiled(_)
        | NativeSubscriptionError::AuthenticationBinding => CustodyError::UnknownAccount,
        NativeSubscriptionError::AccountBusy => CustodyError::Busy {
            retry_after_ms: 100,
        },
        NativeSubscriptionError::UnsafeStorage => CustodyError::Store,
        NativeSubscriptionError::Transport(_) => CustodyError::Unavailable,
        _ => CustodyError::Internal,
    }
}
