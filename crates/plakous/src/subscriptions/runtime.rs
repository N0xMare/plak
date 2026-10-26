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
    #[cfg(test)]
    pub(super) child_backoff: Option<child::BackoffHook>,
    #[cfg(test)]
    pub(super) codex_test_transport: Option<(plakous_http::Client, plakous_openai::EndpointOrigin)>,
    pub(super) catalog_retries: bool,
    pub(super) state: SubscriptionState,
    pub(super) exchange_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
}

impl NativeSubscriptions {
    pub(super) fn from_state(state: SubscriptionState) -> Self {
        Self {
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
