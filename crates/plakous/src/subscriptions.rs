//! Curated, Plak-owned subscription authentication and model admission.
//!
//! This application-facing module deliberately hides provider transports,
//! credentials, account identifiers, generations, endpoints, and native route
//! objects. Its public values are credential-free capabilities and status.
//!
//! ## Admit surface
//!
//! Library-owned admission is the typed gate for catalog resolution and
//! revalidation before an ordinary run.

mod catalog;
mod claude;
mod codex;
mod compatibility;
mod credential;
mod guidance;
mod login;
mod model_catalog;
mod models;
mod provider;
mod resolution;
mod resolve;
mod state;
mod status;
mod usage;
mod xai;

use codex::codex_native_http_config;
pub use credential::dispatch_credential_admitted;
pub use login::{
    CompatibilityNotice, NativeDeviceCode, NativeLoginCompletion, NativeLoginInstructions,
    NativeLoginMode, PendingNativeLogin,
};
#[cfg(test)]
use login::{
    PendingCompatibilityConsent, complete_approved_with_consent, complete_pending_with_consent,
};
use model_catalog::catalog_operation_deadline;
#[cfg(test)]
use model_catalog::run_classified_model_catalog;
pub use models::{
    NativeModelCatalog, NativeModelSelector, NativeModelSelectorError, NativeModelSummary,
    NativeModelVisibility, NativeServiceTierSummary, NativeToolMode,
    NativeToolModeRequirementError,
};
use models::{
    codex_native_tool_mode, facade_codex_visibility, facade_native_tool_mode,
    facade_xai_model_is_selectable, first_codex_compatible_model, require_exact_native_tool_mode,
};
pub use provider::{NativeProvider, native_provider_route_selection};
pub(crate) use resolution::SubscriptionResolutionEvidence;
pub use resolution::{NativeModelResolutionError, ResolvedNativeModel, ResolvedProvider};
pub use status::{CompatibilityStatus, ForcedRefresh, NativeAuthStatus, NativeSubscriptionStatus};
use xai::{xai_inference_client, xai_native_http_config};

pub use state::SubscriptionPaths;
use state::{LoadedAcceptance, LoginRecoveryOperation, PrivateFilesystemLock, SubscriptionState};
mod error;
mod reresolve;
mod scope;
mod telemetry;
use scope::apply_turn_scoped_provider_gate;
#[cfg(test)]
pub(crate) use scope::wrap_turn_scoped_native_model;

pub use error::NativeSubscriptionError;
use error::{
    map_claude_error, map_claude_offline_status, map_codex_error, map_codex_status,
    map_credential_status, map_xai_error, map_xai_offline_status,
};
use telemetry::credential_refresh_observer;
pub use telemetry::install_credential_telemetry;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The product's per-model operation budget is 300 seconds. Native inference
// must not silently inherit the shorter provider-library API default.
const NATIVE_INFERENCE_OPERATION_TIMEOUT: Duration = Duration::from_secs(300);

use plakous_auth::subscription_limit::{
    InMemorySubscriptionRequestGate, SubscriptionInferenceScopeHandle,
    SubscriptionInferenceScopeLease, SubscriptionLimitSnapshot,
};
pub use plakous_auth::subscription_limit::{
    SubscriptionInferenceScope, SubscriptionRequestClass, SubscriptionRequestGate,
    SubscriptionRequestLimits, SystemSubscriptionGateClock, TurnScopedSubscriptionRequestGate,
};
use plakous_model::{
    CompiledProviderConfiguration, ConfiguredLanguageModel, CredentialSelectionId, Digest,
    DigestDomain, DigestError, LanguageModel, ModelAdapterProfileId, ModelCallRecoveryOperation,
    ModelCallRecoveryRequest, ModelError, ModelErrorKind, ModelId, ModelOperationRoot,
    ModelOperationScope, ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind,
    PreparedModelCall, ProtocolId, ProviderAuthorityClassId, ProviderProfileRevisionId,
    ProviderRouteSelection, ReasoningEffort, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, RouteId, RuntimeAdmissionWitness, RuntimeModelAuthority,
    RuntimeModelAuthorityOperation, SelectedProviderConfiguration, SessionLineage,
    SubscriptionUsageCredentialState, SubscriptionUsageRead, SubscriptionUsageUnavailable,
    Timestamp, ValidatedRequestPlan,
};

const PRIMARY_SLOT: &str = "primary";
// Refresh/setup (up to 30 s) plus the shared 60 s lookup cap and cleanup margin.
const MODEL_CATALOG_DEADLINE: Duration = Duration::from_secs(120);

/// Curated owner of Plak's native subscription operations.
///
/// Construction is offline and accepts only fixed Plak-owned paths. Every
/// online operation validates exact persisted compatibility consent, creates a
/// provider-blind bounded client internally, and exposes no credential or
/// provider-native route object.
#[derive(Clone)]
pub struct NativeSubscriptions {
    catalog_retries: bool,
    state: SubscriptionState,
    exchange_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
}

impl NativeSubscriptions {
    fn from_state(state: SubscriptionState) -> Self {
        Self {
            state,
            exchange_observer: None,
            catalog_retries: true,
        }
    }
    /// Creates the curated facade without filesystem or network I/O.
    #[must_use]
    pub const fn new(paths: SubscriptionPaths) -> Self {
        Self {
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

    /// Uses single-attempt model lookups for background refresh or child binding
    /// while a session is already attached. Receipt renewals are always single-attempt.
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

fn unix_now() -> Result<u64, NativeSubscriptionError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| NativeSubscriptionError::Internal)
}

fn timestamp_now() -> Result<Timestamp, NativeSubscriptionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NativeSubscriptionError::Internal)?;
    Timestamp::new(duration.as_secs(), duration.subsec_nanos())
        .map_err(|_| NativeSubscriptionError::Internal)
}

/// Runs a claimed refresh as its own task: a dropped caller (a usage-read
/// timeout, a closed menu) never strands a started claim mid-exchange.
async fn detached<F>(refresh: F) -> Result<F::Output, NativeSubscriptionError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(refresh)
        .await
        .map_err(|_| NativeSubscriptionError::Internal)
}

async fn run_blocking<F, T>(operation: F) -> Result<T, NativeSubscriptionError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| NativeSubscriptionError::Internal)
}
