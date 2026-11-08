//! Native subscription vocabulary and exact per-route capability dispatch.
//! An omitted route never falls back to a different provider.

/// Freshness of exact-fenced read-only allowance observations. This does not
/// change legacy fresh reads or their caller's per-session projection aging.
pub const USAGE_OBSERVATION_FRESHNESS: Duration = Duration::from_secs(30);

#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod catalog;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod child;
#[cfg(feature = "claude")]
mod claude;
#[cfg(feature = "codex")]
mod codex;
#[cfg(any(feature = "grok", feature = "claude"))]
mod common_error;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod compatibility;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod credential;
mod error;
#[cfg(feature = "grok")]
mod grok;
#[cfg(feature = "codex")]
mod guidance;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod login;
mod login_types;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod model_catalog;
mod models;
mod paths;
mod provider;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod reresolve;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod resolution;
#[cfg(not(any(feature = "codex", feature = "grok", feature = "claude")))]
#[path = "subscriptions/resolution_unavailable.rs"]
mod resolution;
mod resolution_error;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod resolve;
#[cfg(any(feature = "codex", feature = "grok"))]
mod responses_budget;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod runtime;
#[cfg(not(any(feature = "codex", feature = "grok", feature = "claude")))]
#[path = "subscriptions/unavailable.rs"]
mod runtime;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod scope;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod state;
mod status;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod telemetry;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod usage;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod vertical;
#[cfg(not(any(feature = "codex", feature = "grok", feature = "claude")))]
#[path = "subscriptions/vertical_unavailable.rs"]
mod vertical;

pub use error::NativeSubscriptionError;
pub use login_types::{
    CompatibilityNotice, NativeDeviceCode, NativeLoginCompletion, NativeLoginInstructions,
    NativeLoginMode,
};
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
use models::require_exact_native_tool_mode;
pub use models::{
    NativeModelCatalog, NativeModelSelector, NativeModelSelectorError, NativeModelSummary,
    NativeModelVisibility, NativeServiceTierSummary, NativeToolMode,
    NativeToolModeRequirementError,
};
pub use paths::SubscriptionPaths;
#[cfg(any(
    feature = "codex",
    feature = "grok",
    feature = "claude",
    feature = "auth"
))]
pub use plakous_auth::CompatibilityDrift;
pub use plakous_model::subscription_limit::{
    SubscriptionInferenceScope, SubscriptionRequestClass, SubscriptionRequestGate,
    SubscriptionRequestLimits, SystemSubscriptionGateClock, TurnScopedSubscriptionRequestGate,
};
use plakous_model::{
    CredentialSelectionId, ModelAdapterProfileId, ModelId, ProtocolId, ProviderAuthorityClassId,
    ProviderProfileRevisionId, ProviderRouteSelection, ReasoningEffort, RouteId,
    SelectedProviderConfiguration, SessionLineage, SubscriptionUsageRead,
};
pub use provider::{NativeProvider, native_provider_route_selection};
pub use resolution::{ResolvedNativeModel, ResolvedProvider};
pub use resolution_error::NativeModelResolutionError;
pub use runtime::{NativeSubscriptions, PendingNativeLogin, install_credential_telemetry};
pub use status::{CompatibilityStatus, ForcedRefresh, NativeAuthStatus, NativeSubscriptionStatus};
use std::{
    error::Error,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};
const PRIMARY_SLOT: &str = "primary";
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
use resolution::SubscriptionResolutionEvidence;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
use runtime::support::{
    CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, DigestDomain, DigestError,
    Future, InMemorySubscriptionRequestGate, LanguageModel, LoadedAcceptance,
    LoginRecoveryOperation, MODEL_CATALOG_DEADLINE, ModelCallRecoveryOperation,
    ModelCallRecoveryRequest, ModelError, ModelErrorKind, ModelOperationRoot, ModelOperationScope,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, PreparedModelCall,
    PrivateFilesystemLock, ResolvedModelDescriptor, RuntimeAdmissionWitness, RuntimeModelAuthority,
    RuntimeModelAuthorityOperation, SubscriptionInferenceScopeHandle,
    SubscriptionInferenceScopeLease, SubscriptionLimitSnapshot, SubscriptionState,
    SubscriptionUsageCredentialState, SubscriptionUsageUnavailable, Timestamp,
    ValidatedRequestPlan, apply_turn_scoped_provider_gate, catalog_operation_deadline,
    credential_refresh_observer, detached, run_blocking, timestamp_now, unix_now,
};

#[cfg(all(test, any(feature = "codex", feature = "grok", feature = "claude")))]
pub(crate) use scope::wrap_turn_scoped_native_model;

impl NativeSubscriptions {
    /// Returns the exact compatibility notice, or refuses an omitted route.
    ///
    /// # Errors
    /// Returns [`NativeSubscriptionError::NotCompiled`] before any I/O.
    pub fn notice(
        provider: NativeProvider,
    ) -> Result<CompatibilityNotice, NativeSubscriptionError> {
        Ok(vertical::require_vertical(provider)?.notice())
    }
}
/// The single credential admission rule for every dispatch-shaped path: model
/// catalog, inference, and child spawn.
///
/// Ready is admitted for every provider. xAI Quarantined is admitted because
/// its durable Unknown is retryable on the stored refresh token;
/// the next refresh reclaims it. Codex and Claude stay Ready-only.
#[must_use]
pub const fn dispatch_credential_admitted(
    provider: NativeProvider,
    credential: NativeAuthStatus,
) -> bool {
    matches!(
        (provider, credential),
        (_, NativeAuthStatus::Ready) | (NativeProvider::Xai, NativeAuthStatus::Quarantined)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::SubscriptionUsageUnavailable;

    #[tokio::test]
    async fn omitted_native_routes_refuse_without_creating_state() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("uncreated");
        let subscriptions =
            NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(&root)?);
        for provider in [
            NativeProvider::Codex,
            NativeProvider::Xai,
            NativeProvider::Claude,
        ] {
            if provider.is_compiled() {
                assert!(NativeSubscriptions::notice(provider).is_ok());
                continue;
            }
            let expected = Some(NativeSubscriptionError::NotCompiled(provider));
            assert_eq!(NativeSubscriptions::notice(provider).err(), expected);
            assert_eq!(
                native_provider_route_selection(provider, ModelId::new("unavailable-model")?).err(),
                expected
            );
            assert_eq!(subscriptions.status(provider).await.err(), expected);
            assert_eq!(
                subscriptions
                    .begin_login(provider, NativeLoginMode::Browser)
                    .await
                    .err(),
                expected
            );
            assert_eq!(subscriptions.model_catalog(provider).await.err(), expected);
            assert_eq!(subscriptions.refresh(provider).await.err(), expected);
            assert!(matches!(
                subscriptions.subscription_usage(provider).await,
                SubscriptionUsageRead::Unavailable {
                    reason: SubscriptionUsageUnavailable::NotConnected,
                    ..
                }
            ));
            assert!(
                !root.exists(),
                "an omitted route must never create its state root"
            );
        }
        Ok(())
    }
}
