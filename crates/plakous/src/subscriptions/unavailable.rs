//! Native-operation refusal when no subscription vertical was linked.
use super::{
    Arc, CompatibilityStatus, ForcedRefresh, Instant, NativeAuthStatus, NativeLoginInstructions,
    NativeLoginMode, NativeModelCatalog, NativeModelResolutionError, NativeProvider,
    NativeSubscriptionError, NativeSubscriptionStatus, ResolvedProvider,
    SelectedProviderConfiguration, SessionLineage, SubscriptionPaths, SubscriptionRequestLimits,
    SubscriptionUsageRead, fmt,
};
/// Native subscription operations with no compiled subscription route.
#[derive(Clone)]
pub struct NativeSubscriptions {
    _state_root: std::path::PathBuf,
}
#[allow(
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    reason = "disabled routes retain the compiled async API contract"
)]
impl NativeSubscriptions {
    /// No local native observation exists when native routes are omitted.
    ///
    /// # Errors
    /// Always reports that this provider was not compiled, before I/O.
    pub async fn account_observation_revision(
        &self,
        _provider: NativeProvider,
    ) -> Result<Option<[u8; 32]>, plakous_model::SubscriptionUsageUnavailable> {
        Err(plakous_model::SubscriptionUsageUnavailable::NotConnected)
    }
    /// Refuses account comparison for an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn upstream_account_fingerprint(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<plakous_model::Digest>, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses enumeration of an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn named_accounts(
        &self,
        provider: NativeProvider,
    ) -> Result<Vec<String>, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }

    /// Refuses bounded enumeration before I/O when the native route is omitted.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn named_accounts_bounded(
        &self,
        provider: NativeProvider,
        _maximum: usize,
    ) -> Result<(Vec<String>, usize, bool), NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an existence read for an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn account_exists(
        &self,
        provider: NativeProvider,
    ) -> Result<bool, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses selection of an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub fn for_named_account(
        &self,
        provider: NativeProvider,
        _name: &str,
    ) -> Result<Self, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }

    /// Refuses creation of an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn create_account_for_login(
        &self,
        provider: NativeProvider,
    ) -> Result<Self, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }

    /// Refuses removal of an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn remove_signed_out_account(
        &self,
        provider: NativeProvider,
    ) -> Result<(), NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses a login to an omitted native route before I/O.
    ///
    /// # Errors
    /// Returns the exact omitted route.
    pub async fn begin_login(
        &self,
        provider: NativeProvider,
        _mode: NativeLoginMode,
    ) -> Result<PendingNativeLogin, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Creates a subscription facade without I/O.
    #[must_use]
    pub fn new(paths: SubscriptionPaths) -> Self {
        let SubscriptionPaths { state_root, named } = paths;
        if let Some(slot) = named {
            let _ = (slot.provider, slot.vertical, slot.name);
        }
        Self {
            _state_root: state_root,
        }
    }
    /// Disables native model-lookup retries.
    #[must_use]
    pub const fn without_model_lookup_retries(self) -> Self {
        self
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn acknowledge_exact(
        &self,
        provider: NativeProvider,
        operator_response: &str,
    ) -> Result<(), NativeSubscriptionError> {
        let _ = operator_response;
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn status(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn compatibility_status(
        &self,
        provider: NativeProvider,
    ) -> Result<CompatibilityStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn admit_for_dispatch(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn refresh(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn refresh_now(
        &self,
        provider: NativeProvider,
    ) -> Result<ForcedRefresh, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn access_token_expired(
        &self,
        provider: NativeProvider,
    ) -> Result<bool, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn access_token_expires_within(
        &self,
        provider: NativeProvider,
        _window: std::time::Duration,
    ) -> Result<bool, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn revoke(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn logout_local(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn replace_newer_build_state(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn model_catalog(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn model_catalog_persisted(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        Err(NativeSubscriptionError::NotCompiled(provider))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn resolve_turn_scoped_provider(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let _ = limits;
        let _ = lineage;
        Err(refused_selection(selected))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn resolve_spawned_child_provider(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        startup_deadline: Instant,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let _ = limits;
        let _ = startup_deadline;
        Err(refused_selection(selected))
    }
    /// Refuses an exact native operation without a compiled vertical.
    ///
    /// # Errors
    /// Returns the exact omitted route without I/O.
    pub async fn resolve_turn_scoped_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let _ = limits;
        Err(refused_selection(selected))
    }

    /// Reports no connected subscription when no native route was linked.
    pub async fn subscription_usage(&self, _provider: NativeProvider) -> SubscriptionUsageRead {
        SubscriptionUsageRead::Unavailable {
            observed_at: plakous_model::Clock::now(&plakous_model::SystemClock),
            reason: plakous_model::SubscriptionUsageUnavailable::NotConnected,
        }
    }

    /// Reports no connected subscription without reading or refreshing credentials.
    pub async fn subscription_usage_read_only<Permit>(
        &self,
        provider: NativeProvider,
        _before_get: impl Future<Output = Permit>,
    ) -> SubscriptionUsageRead {
        self.subscription_usage(provider).await
    }
}
/// No provider login can be created without a compiled vertical.
#[derive(Debug)]
pub struct PendingNativeLogin(std::convert::Infallible);
#[allow(
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    reason = "disabled logins retain the compiled async API contract"
)]
impl PendingNativeLogin {
    /// Reports whether this login retired an unusable record.
    #[must_use]
    pub fn replaced_unusable_record(&self) -> bool {
        match self.0 {}
    }
    /// Returns the exact operator instructions.
    #[must_use]
    pub fn instructions(&self) -> NativeLoginInstructions<'_> {
        match self.0 {}
    }
    /// Completes an automatic or device login.
    ///
    /// # Errors
    /// Returns the provider authentication or storage error.
    pub async fn complete(self) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        match self.0 {}
    }
    /// Completes a paste-complete browser login.
    ///
    /// # Errors
    /// Returns the provider authentication or storage error.
    pub async fn complete_remote(
        self,
        _authorization_input: &str,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        match self.0 {}
    }
}
/// No native credential managers exist in this build.
pub fn install_credential_telemetry(_emitter: Arc<plakous_agent::TelemetryEmitter>) {}

impl fmt::Debug for NativeSubscriptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSubscriptions(<redacted fixed Plak paths>)")
    }
}

#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "anthropic",
    feature = "openrouter",
    feature = "web-fetch"
))]
mod observation {
    use super::{Arc, NativeSubscriptions};

    impl NativeSubscriptions {
        /// Attaches an inference observer; no native inference exists in this build.
        #[must_use]
        pub fn with_exchange_observer(
            self,
            _observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
        ) -> Self {
            self
        }
    }
}

fn refused_selection(selected: SelectedProviderConfiguration) -> NativeModelResolutionError {
    let requested_model = selected.route().requested_model().clone();
    drop(selected);
    NativeModelResolutionError::after_selection(
        requested_model,
        NativeSubscriptionError::ProviderConfigurationRejected,
    )
}

#[cfg(any(test, feature = "test-clock"))]
mod clock {
    use super::{Arc, NativeSubscriptions};

    impl NativeSubscriptions {
        /// Supplies credential time; no native credential work exists in this build.
        #[must_use]
        pub fn with_clock(self, _clock: Arc<dyn plakous_model::Clock>) -> Self {
            self
        }
    }
}
