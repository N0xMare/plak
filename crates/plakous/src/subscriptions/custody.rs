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

/// How long an `IfDue` refresh waits for another caller's claimed refresh to
/// settle before it judges due-ness, and how often it looks.
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

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
/// a session to. Refresh is the single writer's: `IfDue` waits for a claimed
/// refresh in flight to settle and then runs only while the caller's fence is
/// current, `AfterRejection` claims one grant only while the store still
/// holds the rejected generation, and `Operator` forces one.
#[derive(Clone)]
pub struct SubscriptionCustodian {
    subscriptions: NativeSubscriptions,
    provider: NativeProvider,
    gateway_v1_authority: bool,
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
            gateway_v1_authority: false,
            refreshing: Arc::default(),
        }
    }

    /// The previous gateway protocol's view of the same store and refresh lock.
    #[must_use]
    pub fn for_gateway_v1(&self) -> Self {
        Self {
            gateway_v1_authority: true,
            ..self.clone()
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
            .login_custody(subscriptions, self.gateway_v1_authority)
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
            && opens_online(status.credential())
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

    /// The facts once no claimed refresh is in flight, waiting at most
    /// [`SETTLE_WAIT`].
    async fn settled(&self, slot: &CustodySlot) -> Result<AccountFacts, CustodyError> {
        let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
        loop {
            let facts = self.current(slot).await?;
            if facts.status != CustodyStatus::Refreshing || tokio::time::Instant::now() >= deadline
            {
                return Ok(facts);
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
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
                    let current = self.settled(slot).await?;
                    if current.check_fence(&expected).is_err() {
                        return Ok(current);
                    }
                    // A failed refresh shows in the facts' status.
                    let _ = subscriptions.refresh(self.provider).await;
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
            if status.compatibility() != CompatibilityStatus::Current
                || status.credential() != NativeAuthStatus::Ready
            {
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

/// Whether a login in `status` opens online for its facts: a Ready login,
/// and one whose claimed refresh is in flight (its identity is still the
/// stored one). A quarantined, revoked or otherwise unusable login refuses to
/// open, so its facts are the offline status alone.
const fn opens_online(status: NativeAuthStatus) -> bool {
    matches!(
        status,
        NativeAuthStatus::Ready | NativeAuthStatus::Refreshing
    )
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

/// Subscription logins a credential gateway holds (spec 01 §7.1): status,
/// consent and refresh answer from the gateway's facts, and every route
/// exchange relays through its transport. The store is never opened.
mod gateway {
    use plakous_auth::custody::{
        AccountFacts, Consent, CredentialCustodian, CustodyError, CustodySlot, CustodyStatus,
        RefreshRequest,
    };
    use plakous_http::RouteTransport;

    use super::super::{
        Arc, CompatibilityStatus, ForcedRefresh, NativeAuthStatus, NativeProvider,
        NativeSubscriptionError, NativeSubscriptionStatus, NativeSubscriptions,
    };

    /// Who holds this facade's logins when it is not the local store.
    #[derive(Clone)]
    pub(in crate::subscriptions) struct SubscriptionGateway {
        pub(in crate::subscriptions) custodian: Arc<dyn CredentialCustodian>,
        pub(in crate::subscriptions) transport: RouteTransport,
        pub(in crate::subscriptions) catalog_cache: Option<std::path::PathBuf>,
    }

    /// The account kind a gateway names `provider`'s logins by.
    const fn kind(provider: NativeProvider) -> &'static str {
        match provider {
            NativeProvider::Codex => "codex",
            NativeProvider::Xai => "grok",
            NativeProvider::Claude => "claude",
        }
    }

    /// A gateway refusal as the facade's error.
    pub(in crate::subscriptions) fn gateway_error(error: &CustodyError) -> NativeSubscriptionError {
        match error {
            CustodyError::Unavailable => NativeSubscriptionError::Transport(None),
            CustodyError::Busy { .. } => NativeSubscriptionError::AccountBusy,
            CustodyError::AccountChanged => NativeSubscriptionError::AuthenticationAccountChanged,
            CustodyError::NotReady { .. }
            | CustodyError::FenceMoved { .. }
            | CustodyError::UnknownAccount
            | CustodyError::Disabled
            | CustodyError::ScopeDenied => NativeSubscriptionError::CredentialUnavailable,
            _ => NativeSubscriptionError::Internal,
        }
    }

    const fn auth_status(status: CustodyStatus) -> NativeAuthStatus {
        match status {
            CustodyStatus::SignedOut => NativeAuthStatus::SignedOut,
            CustodyStatus::Ready => NativeAuthStatus::Ready,
            CustodyStatus::RefreshRequired => NativeAuthStatus::RefreshRequired,
            CustodyStatus::Refreshing => NativeAuthStatus::Refreshing,
            CustodyStatus::Quarantined => NativeAuthStatus::Quarantined,
            CustodyStatus::LoginRequired | CustodyStatus::ProfileChanged => {
                NativeAuthStatus::LoginRequired
            }
            CustodyStatus::WrittenByNewerBuild => NativeAuthStatus::WrittenByNewerBuild,
            CustodyStatus::Revoked => NativeAuthStatus::Revoked,
        }
    }

    impl NativeSubscriptions {
        /// These subscriptions under `custody`. Local custody leaves them
        /// unchanged. Under gateway custody the gateway holds every login:
        /// status, consent, model catalogs, resolution, usage and refresh
        /// answer from its facts for the account `<kind>` or `<kind>/<name>`
        /// (kinds `codex`, `grok`, `claude`; the name of
        /// [`Self::for_named_account`]), route exchanges relay through it with
        /// custodied credential fields, and refresh is its single writer's.
        /// Nothing opens, creates or locks the subscription store; login,
        /// logout, consent and account listing refuse
        /// [`NativeSubscriptionError::GatewayCustody`].
        #[doc(hidden)]
        #[must_use]
        pub fn with_custody(mut self, custody: &crate::custody::Custody) -> Self {
            self.gateway = custody.custodian().map(|custodian| SubscriptionGateway {
                custodian: Arc::clone(custodian),
                transport: custody.transport(),
                catalog_cache: None,
            });
            self
        }

        /// `client` taking this facade's route transport: relayed through the
        /// gateway under gateway custody, unchanged otherwise.
        pub(in crate::subscriptions) fn route_client(
            &self,
            client: plakous_http::Client,
        ) -> plakous_http::Client {
            match &self.gateway {
                Some(gateway) => gateway.transport.client(client),
                None => client,
            }
        }

        /// Refuses unless the gateway reports this build's consent as current.
        pub(in crate::subscriptions) async fn require_gateway_consent(
            &self,
            gateway: &SubscriptionGateway,
            provider: NativeProvider,
        ) -> Result<(), NativeSubscriptionError> {
            match self
                .gateway_status(gateway, provider)
                .await?
                .compatibility()
            {
                CompatibilityStatus::Current => Ok(()),
                CompatibilityStatus::Required => Err(NativeSubscriptionError::AcceptanceRequired),
                CompatibilityStatus::Inactive => Err(NativeSubscriptionError::ProfileInactive),
            }
        }

        /// Refuses a store operation under gateway custody.
        pub(in crate::subscriptions) const fn require_local_custody(
            &self,
        ) -> Result<(), NativeSubscriptionError> {
            if self.gateway.is_some() {
                return Err(NativeSubscriptionError::GatewayCustody);
            }
            Ok(())
        }

        /// The gateway account of `provider`'s selected login.
        pub(in crate::subscriptions) fn custody_slot(
            &self,
            provider: NativeProvider,
        ) -> Result<CustodySlot, NativeSubscriptionError> {
            self.state.require_provider(provider)?;
            CustodySlot::new(
                self.state
                    .source_scope()
                    .unwrap_or_else(|| kind(provider).to_owned()),
            )
            .map_err(|_| NativeSubscriptionError::AuthenticationBinding)
        }

        /// The gateway's current facts for `provider`'s selected login.
        pub(in crate::subscriptions) async fn gateway_facts(
            &self,
            gateway: &SubscriptionGateway,
            provider: NativeProvider,
        ) -> Result<AccountFacts, NativeSubscriptionError> {
            super::super::vertical::require_vertical(provider)?;
            let slot = self.custody_slot(provider)?;
            gateway
                .custodian
                .facts(&slot)
                .await
                .map_err(|error| gateway_error(&error))
        }

        /// The status of `facts`: consent is current only for this build's
        /// exact profile and notice revision.
        pub(in crate::subscriptions) fn gateway_status_of(
            provider: NativeProvider,
            facts: &AccountFacts,
        ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
            let notice = Self::notice(provider)?;
            let compatibility = match &facts.profile {
                Some(profile)
                    if profile.id == notice.profile_id
                        && profile.contract_revision == notice.notice_revision =>
                {
                    match profile.consent {
                        Consent::Current => CompatibilityStatus::Current,
                        Consent::Required => CompatibilityStatus::Required,
                        Consent::Inactive => CompatibilityStatus::Inactive,
                    }
                }
                _ => CompatibilityStatus::Required,
            };
            Ok(NativeSubscriptionStatus {
                compatibility,
                credential: auth_status(facts.status),
            })
        }

        pub(in crate::subscriptions) async fn gateway_status(
            &self,
            gateway: &SubscriptionGateway,
            provider: NativeProvider,
        ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
            let facts = self.gateway_facts(gateway, provider).await?;
            Self::gateway_status_of(provider, &facts)
        }

        /// The gateway's refresh: `IfDue` for the current generation, or the
        /// operator's forced one, then the status it answers with.
        pub(in crate::subscriptions) async fn gateway_refresh(
            &self,
            gateway: &SubscriptionGateway,
            provider: NativeProvider,
            force: bool,
        ) -> Result<(NativeAuthStatus, AccountFacts), NativeSubscriptionError> {
            let slot = self.custody_slot(provider)?;
            let request = if force {
                RefreshRequest::Operator
            } else {
                let facts = self.gateway_facts(gateway, provider).await?;
                let Some(expected) = facts.expected_fence() else {
                    return Ok((auth_status(facts.status), facts));
                };
                RefreshRequest::IfDue { expected }
            };
            let facts = gateway
                .custodian
                .refresh(&slot, request)
                .await
                .map_err(|error| gateway_error(&error))?;
            Ok((auth_status(facts.status), facts))
        }

        /// [`Self::refresh_now`] under gateway custody.
        pub(in crate::subscriptions) async fn gateway_refresh_now(
            &self,
            gateway: &SubscriptionGateway,
            provider: NativeProvider,
        ) -> Result<ForcedRefresh, NativeSubscriptionError> {
            let (status, facts) = self.gateway_refresh(gateway, provider, true).await?;
            Ok(ForcedRefresh {
                status,
                access_expires_at: facts
                    .access_expires_at_unix_ms
                    .ok_or(NativeSubscriptionError::CredentialUnavailable)?
                    / 1000,
            })
        }
    }
}

pub(super) use gateway::SubscriptionGateway;

#[cfg(all(test, feature = "gateway", feature = "codex"))]
mod tests;
