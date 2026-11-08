#[cfg(test)]
use super::child;
use super::{Arc, Duration, NativeSubscriptionError, SubscriptionPaths, fmt};
pub use super::{login::PendingNativeLogin, telemetry::install_credential_telemetry};
use support::{Future, SubscriptionState, SystemTime, Timestamp, UNIX_EPOCH};

// Refresh/setup plus the bounded catalog lookup and cleanup margin.
pub(super) const MODEL_CATALOG_DEADLINE: Duration = Duration::from_secs(120);
/// Curated owner of Plak's native subscription operations.
///
/// Construction is offline and accepts only fixed Plak-owned paths. Every
/// online operation validates exact persisted compatibility consent, creates a
/// provider-blind bounded client internally, and exposes no credential or
/// provider-native route object.
#[derive(Clone)]
pub struct NativeSubscriptions {
    pub(super) usage_fixture: super::usage::test_usage::Fixture,
    #[cfg(test)]
    pub(super) child_backoff: Option<child::BackoffHook>,
    #[cfg(test)]
    pub(super) codex_test_transport: Option<(plakous_http::Client, plakous_openai::EndpointOrigin)>,
    pub(super) catalog_retries: bool,
    pub(super) state: SubscriptionState,
    pub(super) exchange_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
}

impl NativeSubscriptions {
    /// Reads only an authenticated upstream-account digest for local comparison.
    /// This never refreshes, contacts a provider or exposes account/token text.
    /// Missing, busy or unreadable evidence cannot establish account equality.
    ///
    /// # Errors
    /// Refuses an omitted/different vertical and unsafe or foreign state.
    pub async fn upstream_account_fingerprint(
        &self,
        provider: super::NativeProvider,
    ) -> Result<Option<plakous_model::Digest>, NativeSubscriptionError> {
        let vertical = super::vertical::require_vertical(provider)?;
        self.state.require_provider(provider)?;
        let state = self.state.clone();
        if !run_blocking(move || state.layout_exists()).await?? {
            return Ok(None);
        }
        let Some(lock) = self.try_acquire_compatibility_lock(provider).await? else {
            return Ok(None);
        };
        let fingerprint = vertical.upstream_account_fingerprint(self).await?;
        lock.verify()?;
        Ok(fingerprint)
    }
    /// Lists local names from the exact vertical's private account directory.
    /// Hidden trash and invalid names are not accounts. No credentials are read.
    ///
    /// # Errors
    /// Refuses an omitted vertical and unsafe directory ancestors.
    pub async fn named_accounts(
        &self,
        provider: super::NativeProvider,
    ) -> Result<Vec<String>, NativeSubscriptionError> {
        super::vertical::require_vertical(provider)?;
        self.state.require_provider(provider)?;
        let state = self.state.clone();
        run_blocking(move || state.named_accounts(provider)).await?
    }

    /// Lists at most `maximum` directory entries, reporting a truncated scan.
    /// Invalid names count toward the scan bound and no credentials are read.
    ///
    /// # Errors
    /// Refuses an omitted vertical and unsafe directory ancestors.
    pub async fn named_accounts_bounded(
        &self,
        provider: super::NativeProvider,
        maximum: usize,
    ) -> Result<(Vec<String>, usize, bool), NativeSubscriptionError> {
        super::vertical::require_vertical(provider)?;
        self.state.require_provider(provider)?;
        let state = self.state.clone();
        run_blocking(move || state.named_accounts_bounded(provider, maximum)).await?
    }
    /// Reports whether this selected named account exists, without creating it.
    /// Bare defaults always exist as selections, even when signed out.
    ///
    /// # Errors
    /// Refuses an omitted/different vertical and unsafe storage.
    pub async fn account_exists(
        &self,
        provider: super::NativeProvider,
    ) -> Result<bool, NativeSubscriptionError> {
        super::vertical::require_vertical(provider)?;
        self.state.require_provider(provider)?;
        if !self.state.is_named() {
            return Ok(true);
        }
        let state = self.state.clone();
        run_blocking(move || state.layout_exists()).await?
    }
    /// Selects one named account without creating its directory or reading credentials.
    ///
    /// # Errors
    /// Rejects invalid names and mismatched subscription selections.
    pub fn for_named_account(
        &self,
        provider: super::NativeProvider,
        name: &str,
    ) -> Result<Self, NativeSubscriptionError> {
        let mut selected = self.clone();
        selected.state = self.state.for_named_account(provider, name)?;
        Ok(selected)
    }

    /// Creates a missing named directory for an explicit login attempt.
    /// An abandoned new directory is removed after its last pending authority
    /// is dropped, unless a later writer or credential publication won.
    /// Defaults and pre-existing directories are never removed by this attempt.
    ///
    /// # Errors
    /// Refuses unsafe paths or a different selected vertical.
    pub async fn create_account_for_login(
        &self,
        provider: super::NativeProvider,
    ) -> Result<Self, NativeSubscriptionError> {
        super::vertical::require_vertical(provider)?;
        self.state.require_provider(provider)?;
        let mut selected = self.clone();
        let state = self.state.clone();
        selected.state = run_blocking(move || state.create_account_for_login()).await??;
        Ok(selected)
    }

    /// Removes only a named slot proved signed out, under its compatibility lock.
    ///
    /// # Errors
    /// Defaults, credential-bearing slots, unsafe paths and busy writers refuse.
    pub async fn remove_signed_out_account(
        &self,
        provider: super::NativeProvider,
    ) -> Result<(), NativeSubscriptionError> {
        self.state.require_named()?;
        let lock = self
            .try_acquire_compatibility_lock(provider)
            .await?
            .ok_or(NativeSubscriptionError::AccountBusy)?;
        let state = self.state.clone();
        let profile = Self::notice(provider)?.profile_id();
        run_blocking(move || state.require_signed_out_for_removal(provider, profile)).await??;
        let state = self.state.clone();
        run_blocking(move || state.remove_named_directory(&lock)).await?
    }

    pub(super) fn from_state(state: SubscriptionState) -> Self {
        Self {
            usage_fixture: super::usage::test_usage::EMPTY,
            #[cfg(test)]
            child_backoff: None,
            #[cfg(test)]
            codex_test_transport: None,
            state,
            exchange_observer: None,
            catalog_retries: true,
        }
    }
    /// Creates the curated facade without filesystem or network I/O.
    #[must_use]
    pub const fn new(paths: SubscriptionPaths) -> Self {
        Self {
            usage_fixture: super::usage::test_usage::EMPTY,
            #[cfg(test)]
            child_backoff: None,
            #[cfg(test)]
            codex_test_transport: None,
            state: SubscriptionState::new(paths),
            exchange_observer: None,
            catalog_retries: true,
        }
    }

    /// Test-only replacement for compatibility and credential time.
    #[cfg(any(test, feature = "test-clock"))]
    #[doc(hidden)]
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn plakous_model::Clock>) -> Self {
        self.state = self.state.with_clock(clock);
        self
    }

    /// Uses one physical attempt per model lookup, including background refresh.
    /// Child binding may retry the whole operation after releasing its authority;
    /// receipt renewals are always single-attempt.
    #[must_use]
    pub fn without_model_lookup_retries(mut self) -> Self {
        self.catalog_retries = false;
        self
    }

    /// Attaches passive observation to Claude and Grok inference, never auth exchanges.
    #[must_use]
    pub fn with_exchange_observer(
        mut self,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Self {
        self.exchange_observer = observer;
        self
    }
}

impl fmt::Debug for NativeSubscriptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSubscriptions(<redacted fixed Plak paths>)")
    }
}

pub(super) fn unix_now() -> Result<u64, NativeSubscriptionError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| NativeSubscriptionError::Internal)
}

pub(super) fn timestamp_now() -> Result<Timestamp, NativeSubscriptionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NativeSubscriptionError::Internal)?;
    Timestamp::new(duration.as_secs(), duration.subsec_nanos())
        .map_err(|_| NativeSubscriptionError::Internal)
}

/// Runs a claimed refresh as its own task: a dropped caller (a usage-read
/// timeout, a closed menu) never strands a started claim mid-exchange.
pub(super) async fn detached<F>(refresh: F) -> Result<F::Output, NativeSubscriptionError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(refresh)
        .await
        .map_err(|_| NativeSubscriptionError::Internal)
}

pub(super) async fn run_blocking<F, T>(operation: F) -> Result<T, NativeSubscriptionError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| NativeSubscriptionError::Internal)
}

pub(super) mod support {
    pub(in crate::subscriptions) use super::super::model_catalog::catalog_operation_deadline;
    pub(in crate::subscriptions) use super::super::scope::apply_turn_scoped_provider_gate;
    pub(in crate::subscriptions) use super::super::state::{
        LoadedAcceptance, LoginRecoveryOperation, PrivateFilesystemLock, SubscriptionState,
    };
    pub(in crate::subscriptions) use super::super::telemetry::credential_refresh_observer;
    pub(in crate::subscriptions) use super::{
        MODEL_CATALOG_DEADLINE, detached, run_blocking, timestamp_now, unix_now,
    };
    pub(in crate::subscriptions) use plakous_model::subscription_limit::{
        InMemorySubscriptionRequestGate, SubscriptionInferenceScopeHandle,
        SubscriptionInferenceScopeLease, SubscriptionLimitSnapshot,
    };
    pub(in crate::subscriptions) use plakous_model::{
        CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, DigestDomain, DigestError,
        LanguageModel, ModelCallRecoveryOperation, ModelCallRecoveryRequest, ModelError,
        ModelErrorKind, ModelOperationRoot, ModelOperationScope, ModelPreparationContext,
        ModelPreparationError, ModelPreparationErrorKind, PreparedModelCall,
        ResolvedModelDescriptor, RuntimeAdmissionWitness, RuntimeModelAuthority,
        RuntimeModelAuthorityOperation, SubscriptionUsageCredentialState,
        SubscriptionUsageUnavailable, Timestamp, ValidatedRequestPlan,
    };
    pub(in crate::subscriptions) use std::{
        future::Future,
        time::{SystemTime, UNIX_EPOCH},
    };
}
