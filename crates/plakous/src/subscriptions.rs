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

#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::too_many_lines,
    clippy::collapsible_if,
    clippy::single_match_else,
    reason = "SuperGrok/Codex product names; admit Result surface; status helpers"
)]

mod catalog;
mod guidance;
mod state;
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

fn codex_native_http_config()
-> Result<plakous_openai::StandardOpenAiHttpConfig, NativeSubscriptionError> {
    plakous_openai::StandardOpenAiHttpConfig::new(
        NATIVE_INFERENCE_OPERATION_TIMEOUT,
        plakous_openai::StandardOpenAiHttpConfig::default().event_channel_capacity(),
        plakous_openai::StandardOpenAiHttpConfig::default().responses_limits(),
    )
    .map_err(|_| NativeSubscriptionError::Internal)
}

fn xai_native_http_config() -> Result<plakous_xai::StandardXaiHttpConfig, NativeSubscriptionError> {
    plakous_xai::StandardXaiHttpConfig::new(
        NATIVE_INFERENCE_OPERATION_TIMEOUT,
        plakous_xai::StandardXaiHttpConfig::default().event_channel_capacity(),
        plakous_xai::StandardXaiHttpConfig::default().responses_limits(),
    )
    .map_err(|_| NativeSubscriptionError::Internal)
}

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
const MODEL_CATALOG_DEADLINE: Duration = Duration::from_secs(30);

/// A built-in Plak-owned subscription integration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeProvider {
    /// `ChatGPT` subscription access through the pinned Codex compatibility path.
    Codex,
    /// `SuperGrok` subscription access through the pinned xAI compatibility path.
    Xai,
    /// Claude Max OAuth access through the pinned Messages Claude OAuth path.
    Claude,
}

impl NativeProvider {
    /// Returns the stable command-line identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Xai => "xai",
            Self::Claude => "claude",
        }
    }

    /// Every enrolled coding model id for this integration.
    ///
    /// The answer is the provider crate's own enrollment table; no other list
    /// of native model ids exists. Catalog observation is still required
    /// before a listed model can run.
    #[must_use]
    pub fn enrolled_models(self) -> Vec<&'static str> {
        match self {
            Self::Codex => plakous_openai::RELEASE_ENROLLED_CODEX_MODELS
                .iter()
                .map(|(model, _)| *model)
                .collect(),
            Self::Xai => plakous_xai::subscription_coding_model_ids().collect(),
            Self::Claude => plakous_anthropic::claude_oauth_enrolled_models().collect(),
        }
    }

    /// The source-backed native tool mode an enrolled literal is pinned to.
    /// Only Codex rows carry one.
    #[must_use]
    pub fn enrolled_tool_mode(self, model: &str) -> Option<NativeToolMode> {
        match self {
            Self::Codex => {
                facade_native_tool_mode(plakous_openai::release_enrolled_codex_tool_mode(model))
            }
            Self::Xai | Self::Claude => None,
        }
    }

    /// Whether an exact literal is an enrolled coding model for this integration.
    #[must_use]
    pub fn enrolls(self, model: &str) -> bool {
        match self {
            Self::Codex => plakous_openai::release_enrolled_codex_model(model),
            Self::Xai => plakous_xai::is_subscription_coding_model_id(model),
            Self::Claude => plakous_anthropic::claude_oauth_enrolled_model(model),
        }
    }
}

/// Materializes the complete pinned route/auth tuple for one explicit native
/// provider and literal model selection.
///
/// This does not choose a provider, credential slot, or model. It exposes the
/// immutable integration facts needed to construct a complete
/// [`SelectedProviderConfiguration`] without duplicating provider pins in an
/// application.
pub fn native_provider_route_selection(
    provider: NativeProvider,
    requested_model: ModelId,
) -> Result<ProviderRouteSelection, NativeSubscriptionError> {
    let ids = match provider {
        NativeProvider::Codex => (
            plakous_openai::OPENAI_CODEX_RESPONSES_PROTOCOL_ID,
            plakous_openai::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
            plakous_openai::OPENAI_RESPONSES_ADAPTER_REVISION,
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
            plakous_openai::OPENAI_CODEX_SOURCE_REVISION,
        ),
        NativeProvider::Xai => (
            plakous_xai::SUPERGROK_RESPONSES_DIALECT.protocol_id(),
            plakous_xai::SUPERGROK_RESPONSES_DIALECT.route().route_id(),
            plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE,
            plakous_xai::XAI_RESPONSES_ADAPTER_REVISION,
            plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE,
            plakous_xai::XAI_GROK_BUILD_SOURCE_REVISION,
        ),
        NativeProvider::Claude => (
            plakous_anthropic::CLAUDE_OAUTH_PROTOCOL_ID,
            plakous_anthropic::CLAUDE_OAUTH_ROUTE_ID,
            plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            plakous_anthropic::CLAUDE_OAUTH_ADAPTER_REVISION,
            plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            plakous_anthropic::CLAUDE_OAUTH_SOURCE_REVISION,
        ),
    };
    Ok(ProviderRouteSelection::new(
        ProtocolId::new(ids.0).map_err(|_| NativeSubscriptionError::Internal)?,
        RouteId::new(ids.1).map_err(|_| NativeSubscriptionError::Internal)?,
        ModelAdapterProfileId::new(ids.2).map_err(|_| NativeSubscriptionError::Internal)?,
        ProviderProfileRevisionId::new(ids.3).map_err(|_| NativeSubscriptionError::Internal)?,
        ModelAdapterProfileId::new(ids.4).map_err(|_| NativeSubscriptionError::Internal)?,
        ProviderProfileRevisionId::new(ids.5).map_err(|_| NativeSubscriptionError::Internal)?,
        ProviderAuthorityClassId::new("oauth-subscription")
            .map_err(|_| NativeSubscriptionError::Internal)?,
        CredentialSelectionId::new(PRIMARY_SLOT).map_err(|_| NativeSubscriptionError::Internal)?,
        requested_model,
    ))
}

/// Credential-free normalized subscription state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeAuthStatus {
    /// No selected credential is present.
    SignedOut,
    /// A ready credential generation is present.
    Ready,
    /// A definitely refreshable credential requires refresh before inference.
    RefreshRequired,
    /// A rotating-token outcome is unknown and inference is blocked.
    Quarantined,
    /// A local revoked tombstone prevents stale resurrection.
    Revoked,
    /// A stored login this build cannot use (made by an earlier build); a new
    /// login is required.
    LoginRequired,
}

/// Credential-free compatibility acceptance state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompatibilityStatus {
    /// No exact current acceptance receipt is present.
    Required,
    /// The stored receipt matches every current immutable fact.
    Current,
    /// The profile is expired, killed, or not yet valid.
    Inactive,
}

/// Complete credential-free local status for one native integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeSubscriptionStatus {
    compatibility: CompatibilityStatus,
    credential: NativeAuthStatus,
}

impl NativeSubscriptionStatus {
    /// Returns exact compatibility-receipt state.
    #[must_use]
    pub const fn compatibility(self) -> CompatibilityStatus {
        self.compatibility
    }

    /// Returns normalized local credential state.
    #[must_use]
    pub const fn credential(self) -> NativeAuthStatus {
        self.credential
    }
}

/// Credential-free summary of one exact account-admitted native model.
///
/// The capability bits are limited to Plak's portable request surface. A
/// false bit means the current authenticated catalog and pinned route did not
/// affirmatively admit that capability; it is not a claim about every
/// provider-native API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelSummary {
    exact_id: String,
    visibility: NativeModelVisibility,
    native_tool_mode: Option<NativeToolMode>,
    streaming: bool,
    tools: bool,
    reasoning_efforts: Vec<ReasoningEffort>,
    default_reasoning_effort: Option<ReasoningEffort>,
    service_tiers: Vec<NativeServiceTierSummary>,
    default_service_tier: Option<String>,
    reported_context_length: Option<u64>,
}

/// Exact provider-authored presentation for one native service tier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeServiceTierSummary {
    id: String,
    name: String,
    description: String,
}

impl NativeServiceTierSummary {
    /// Returns the exact provider tier ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the exact provider-authored tier name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the exact provider-authored tier description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

impl NativeModelSummary {
    /// Returns the exact authenticated catalog identifier, never an alias.
    #[must_use]
    pub fn exact_id(&self) -> &str {
        &self.exact_id
    }

    /// Returns the provider-authored catalog visibility for this exact row.
    #[must_use]
    pub const fn visibility(&self) -> NativeModelVisibility {
        self.visibility
    }

    /// Returns the exact authenticated model-specific tool mode when the
    /// provider catalog has such a concept and this release recognizes it.
    #[must_use]
    pub const fn native_tool_mode(&self) -> Option<NativeToolMode> {
        self.native_tool_mode
    }

    /// Reports portable streaming support admitted by current route evidence.
    #[must_use]
    pub const fn supports_streaming(&self) -> bool {
        self.streaming
    }

    /// Reports portable client-side function-tool support admitted by current route evidence.
    #[must_use]
    pub const fn supports_tools(&self) -> bool {
        self.tools
    }

    /// Reports portable reasoning controls admitted by current route evidence.
    #[must_use]
    pub const fn supports_reasoning(&self) -> bool {
        !self.reasoning_efforts.is_empty()
    }

    /// Returns exact portable effort options admitted for this catalog row.
    ///
    /// Ordering is provider-authored for Codex and fixed from the reviewed
    /// Grok Build row for each exact SuperGrok model (`Low` through `High` for
    /// 4.5; `Low` through `XHigh` for 4.6). Minimal, unknown, aliased, default,
    /// and token-budget values are omitted.
    #[must_use]
    pub fn reasoning_efforts(&self) -> &[ReasoningEffort] {
        &self.reasoning_efforts
    }

    /// Returns the exact provider-authored default reasoning effort, if any.
    #[must_use]
    pub const fn default_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.default_reasoning_effort
    }

    /// Returns exact provider-authored service tiers in catalog order.
    #[must_use]
    pub fn service_tiers(&self) -> &[NativeServiceTierSummary] {
        &self.service_tiers
    }

    /// Returns the exact provider-authored default service-tier ID, if any.
    #[must_use]
    pub fn default_service_tier(&self) -> Option<&str> {
        self.default_service_tier.as_deref()
    }

    /// Returns an authenticated provider-reported context length for operator
    /// diagnosis. This is not itself a context-admission certificate.
    #[must_use]
    pub const fn reported_context_length(&self) -> Option<u64> {
        self.reported_context_length
    }
}

/// Fresh credential-free model catalog for one native integration.
///
/// Entries retain authenticated provider order. The catalog does not contain
/// aliases or designate a default model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelCatalog {
    provider: NativeProvider,
    models: Vec<NativeModelSummary>,
}

/// How to select one account-admitted model during gated catalog resolution.
///
/// Resolution consumes it through `NativeSubscriptions::resolve_gated_native_model`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeModelSelector {
    /// Resolve exactly the supplied model identifier.
    Exact(ModelId),
    /// Select the first backend-order Codex entry with text, tools, and reasoning.
    CodexFirstCompatible,
}

/// Fail-closed constructor error for [`NativeModelSelector`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeModelSelectorError;

impl fmt::Display for NativeModelSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("native model selector is invalid")
    }
}

impl Error for NativeModelSelectorError {}

impl NativeModelSelector {
    /// Creates an exact selector from one validated model identifier.
    ///
    /// # Errors
    ///
    /// Rejects malformed exact model identifiers.
    pub fn exact(model: impl Into<String>) -> Result<Self, NativeModelSelectorError> {
        ModelId::new(model.into())
            .map(Self::Exact)
            .map_err(|_| NativeModelSelectorError)
    }

    /// Creates the named deterministic Codex catalog selector.
    #[must_use]
    pub const fn codex_first_compatible() -> Self {
        Self::CodexFirstCompatible
    }

    /// Returns the exact identifier carried by an exact selector.
    #[must_use]
    pub const fn exact_model(&self) -> Option<&ModelId> {
        match self {
            Self::Exact(model) => Some(model),
            Self::CodexFirstCompatible => None,
        }
    }

    /// Returns whether this selector accepts `selected` after catalog resolution.
    #[must_use]
    pub fn admits_selected(&self, selected: &ModelId) -> bool {
        match self {
            Self::Exact(exact) => exact == selected,
            Self::CodexFirstCompatible => true,
        }
    }

    /// Rejects selectors that are undefined for `provider`.
    ///
    /// # Errors
    ///
    /// Returns [`NativeModelSelectorError`] when the selector is not defined for
    /// the provider.
    pub fn validate_for_provider(
        &self,
        provider: NativeProvider,
    ) -> Result<(), NativeModelSelectorError> {
        if matches!(self, Self::CodexFirstCompatible) && provider != NativeProvider::Codex {
            return Err(NativeModelSelectorError);
        }
        Ok(())
    }
}

/// One gated native model resolution with fresh account-scoped evidence.
///
/// Credential material stays inside the returned [`LanguageModel`].
pub struct ResolvedNativeModel {
    pub(crate) selected_model: ModelId,
    pub(crate) descriptor: ResolvedModelDescriptor,
    pub(crate) model: Arc<dyn LanguageModel>,
    pub(crate) evidence: SubscriptionResolutionEvidence,
}

/// One exact native provider resolution carrying both the executable model and
/// the complete selected-to-effective configuration authority.
pub struct ResolvedProvider {
    model: ResolvedNativeModel,
    configured: ConfiguredLanguageModel,
    native_tool_mode: Option<NativeToolMode>,
    reresolution: Option<Arc<reresolve::Reresolution>>,
}

/// Exact provider-advertised native tool presentation admitted for a resolved model.
///
/// This facade-owned type deliberately does not expose provider catalog types. A
/// value is present only when fresh, authenticated evidence for the exact
/// resolved model supplied one of the closed modes understood by this release.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeToolMode {
    /// Expose the ordinary qualified direct tool catalog.
    Direct,
    /// Expose both the ordinary direct catalog and local Code Mode controls.
    CodeMode,
    /// Expose only the local Code Mode controls.
    CodeModeOnly,
}

/// Provider-authored visibility of an authenticated native model row.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeModelVisibility {
    /// Present in the ordinary product model picker.
    List,
    /// Addressable by an exact provider workflow but hidden from the picker.
    Hide,
    /// Not exposed through picker or public model-list UI.
    None,
}

fn facade_codex_visibility(
    visibility: plakous_openai::CodexCatalogVisibility,
) -> NativeModelVisibility {
    match visibility {
        plakous_openai::CodexCatalogVisibility::List => NativeModelVisibility::List,
        plakous_openai::CodexCatalogVisibility::Hide => NativeModelVisibility::Hide,
        plakous_openai::CodexCatalogVisibility::None => NativeModelVisibility::None,
    }
}

/// Failure to satisfy an exact native tool-mode requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeToolModeRequirementError {
    required: NativeToolMode,
    available: Option<NativeToolMode>,
}

impl NativeToolModeRequirementError {
    /// Returns the exact mode required by the caller.
    #[must_use]
    pub const fn required(self) -> NativeToolMode {
        self.required
    }

    /// Returns the exact admitted mode, or `None` when evidence did not admit one.
    #[must_use]
    pub const fn available(self) -> Option<NativeToolMode> {
        self.available
    }
}

impl fmt::Display for NativeToolModeRequirementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "required native tool mode {:?} is not available; resolved mode is {:?}",
            self.required, self.available
        )
    }
}

impl Error for NativeToolModeRequirementError {}

fn codex_native_tool_mode(
    evidence: &plakous_openai::CodexModelResolutionEvidence,
) -> Option<NativeToolMode> {
    facade_native_tool_mode(evidence.tool_mode())
}

fn facade_native_tool_mode(
    mode: Option<plakous_openai::CodexCatalogToolMode>,
) -> Option<NativeToolMode> {
    mode.map(|mode| match mode {
        plakous_openai::CodexCatalogToolMode::Direct => NativeToolMode::Direct,
        plakous_openai::CodexCatalogToolMode::CodeMode => NativeToolMode::CodeMode,
        plakous_openai::CodexCatalogToolMode::CodeModeOnly => NativeToolMode::CodeModeOnly,
    })
}

const fn require_exact_native_tool_mode(
    available: Option<NativeToolMode>,
    required: NativeToolMode,
) -> Result<(), NativeToolModeRequirementError> {
    if matches!(
        (available, required),
        (Some(NativeToolMode::Direct), NativeToolMode::Direct)
            | (Some(NativeToolMode::CodeMode), NativeToolMode::CodeMode)
            | (
                Some(NativeToolMode::CodeModeOnly),
                NativeToolMode::CodeModeOnly
            )
    ) {
        Ok(())
    } else {
        Err(NativeToolModeRequirementError {
            required,
            available,
        })
    }
}

impl ResolvedProvider {
    /// Returns the exact resolved model handle.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        self.configured.model()
    }

    /// Returns the complete provider configuration authority.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        self.configured.configuration()
    }

    /// Returns the exact catalog-selected model.
    #[must_use]
    pub const fn selected_model(&self) -> &ModelId {
        self.model.selected_model()
    }

    /// Returns the fresh execution authority obtained during this resolution.
    #[must_use]
    pub const fn initial_authority(&self) -> &RuntimeModelAuthority {
        self.configured.initial_authority()
    }

    /// Returns a one-way binding to the exact selected login epoch. A fresh
    /// credential generation preserves this value; relogin does not.
    pub fn credential_selection_binding(&self) -> Result<Digest, DigestError> {
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous/native-credential-selection-v1",
                self.model.evidence.credential_selection_epoch.as_bytes(),
            ],
        )
    }

    /// Returns the sealed model/configuration aggregate for agent construction.
    #[must_use]
    pub fn configured_model(&self) -> ConfiguredLanguageModel {
        self.configured.clone()
    }

    /// Returns the exact native tool mode admitted by fresh resolution evidence.
    ///
    /// Non-Codex providers and missing or unknown catalog values return `None`.
    /// No model-name, shell, route, or neighboring-catalog inference is used.
    #[must_use]
    pub const fn native_tool_mode(&self) -> Option<NativeToolMode> {
        self.native_tool_mode
    }

    /// Requires one exact native tool mode before product dispatch is constructed.
    ///
    /// # Errors
    ///
    /// Returns [`NativeToolModeRequirementError`] when evidence admitted a
    /// different mode or did not admit a recognized mode.
    pub const fn require_native_tool_mode(
        &self,
        required: NativeToolMode,
    ) -> Result<(), NativeToolModeRequirementError> {
        require_exact_native_tool_mode(self.native_tool_mode, required)
    }
}

impl ResolvedNativeModel {
    /// Returns the exact catalog model that was selected.
    #[must_use]
    pub const fn selected_model(&self) -> &ModelId {
        &self.selected_model
    }

    /// Returns the resolved model handle admitted for inference.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        Arc::clone(&self.model)
    }

    /// Returns the exact immutable descriptor retained by resolution evidence.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubscriptionResolutionEvidence {
    pub(crate) account_fingerprint: String,
    pub(crate) credential_authority_binding: String,
    pub(crate) credential_generation: u64,
    pub(crate) credential_selection_epoch: String,
    pub(crate) route_fingerprint: String,
    pub(crate) descriptor_digest: String,
    pub(crate) dialect: String,
    pub(crate) adapter_revision: String,
    pub(crate) catalog_observed_at: Timestamp,
    pub(crate) catalog_expires_at: Timestamp,
    pub(crate) runtime_admission_witness: Option<RuntimeAdmissionWitness>,
    pub(crate) profile_revision: String,
    pub(crate) codex_exact: Option<plakous_openai::CodexModelResolutionEvidence>,
    pub(crate) xai_exact: Option<plakous_xai::XaiModelResolutionEvidence>,
}

/// Credential-free failure from gated native model resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelResolutionError {
    /// Set when failure happened after a concrete model id was chosen.
    pub selected_model: Option<ModelId>,
    /// Underlying credential-free subscription failure (transport, model, etc.).
    pub kind: NativeSubscriptionError,
}

impl NativeModelResolutionError {
    fn before_selection(error: NativeSubscriptionError) -> Self {
        Self {
            selected_model: None,
            kind: error,
        }
    }

    fn after_selection(selected_model: ModelId, error: NativeSubscriptionError) -> Self {
        Self {
            selected_model: Some(selected_model),
            kind: error,
        }
    }

    /// Returns the underlying subscription failure kind.
    #[must_use]
    pub const fn kind(&self) -> NativeSubscriptionError {
        self.kind
    }
}

fn first_codex_compatible_model<'a>(
    entries: impl IntoIterator<Item = (&'a str, bool, bool, bool)>,
) -> Option<&'a str> {
    entries
        .into_iter()
        .find_map(|(model, text, tools, reasoning)| (text && tools && reasoning).then_some(model))
}

fn facade_xai_model_is_selectable(model_id: &str) -> bool {
    plakous_xai::is_subscription_coding_model_id(model_id)
}

impl NativeModelCatalog {
    /// Returns the integration whose authenticated catalog was queried.
    #[must_use]
    pub const fn provider(&self) -> NativeProvider {
        self.provider
    }

    /// Returns exact model summaries in authenticated catalog order.
    #[must_use]
    pub fn models(&self) -> &[NativeModelSummary] {
        &self.models
    }
}

/// Supported native login interaction modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLoginMode {
    /// Authorization-code-with-PKCE through a fixed loopback callback.
    Browser,
    /// The browser runs on another machine: Codex uses the device-code flow;
    /// xAI and Claude complete with the code their hosted page displays.
    Remote,
}

/// How one displayed native-login instruction is completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLoginCompletion {
    /// The provider loopback callback completes without more input.
    Automatic,
    /// The operator enters the displayed one-time code at the verification
    /// URL; completion polls the provider without more local input.
    DeviceCode,
    /// The operator may paste the displayed authorization code or final URL.
    AuthorizationCodeOrRedirectUrl,
}

/// Display-only instructions for one pending native login.
///
/// The authorization URL is deliberately accessible because the operator must
/// act on it. Debug output remains redacted.
pub struct NativeLoginInstructions<'a> {
    verification_uri: &'a str,
    completion: NativeLoginCompletion,
    device_code: Option<NativeDeviceCode<'a>>,
}

/// The one-time code of a pending device-code login.
///
/// The code is short-lived and shown only to the operator who started the
/// login. Debug output is redacted.
#[derive(Clone, Copy)]
pub struct NativeDeviceCode<'a> {
    user_code: &'a str,
    deadline: Instant,
}

impl NativeDeviceCode<'_> {
    /// Returns the code the operator enters at the verification URL.
    #[must_use]
    pub const fn user_code(&self) -> &str {
        self.user_code
    }

    /// Returns how long the code stays valid from now.
    #[must_use]
    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl fmt::Debug for NativeDeviceCode<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeDeviceCode(<redacted one-time code>)")
    }
}

impl NativeLoginInstructions<'_> {
    /// Returns the exact provider URL the operator should open.
    #[must_use]
    pub const fn verification_uri(&self) -> &str {
        self.verification_uri
    }

    /// Returns whether completion is automatic, polls a device code, or needs
    /// pasted input.
    #[must_use]
    pub const fn completion(&self) -> NativeLoginCompletion {
        self.completion
    }

    /// Returns the one-time code when completion is
    /// [`NativeLoginCompletion::DeviceCode`].
    #[must_use]
    pub const fn device_code(&self) -> Option<NativeDeviceCode<'_>> {
        self.device_code
    }
}

impl fmt::Debug for NativeLoginInstructions<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeLoginInstructions(<redacted operator instructions>)")
    }
}

/// Immutable, credential-free facts requiring explicit compatibility consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompatibilityNotice {
    provider: NativeProvider,
    profile_id: &'static str,
    source_revision: &'static str,
    secondary_source_revision: Option<&'static str>,
    notice_revision: &'static str,
    provenance: &'static str,
    reviewed_at: u64,
    expires_at: u64,
    kill_switch_revision: &'static str,
    killed: bool,
}

impl CompatibilityNotice {
    /// Returns the integration this notice governs.
    #[must_use]
    pub const fn provider(self) -> NativeProvider {
        self.provider
    }

    /// Returns the exact compatibility profile identity.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        self.profile_id
    }

    /// Returns the immutable reviewed source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        self.source_revision
    }

    /// Returns an additional immutable official source revision when the
    /// integration joins two independently reviewed sources.
    #[must_use]
    pub const fn secondary_source_revision(self) -> Option<&'static str> {
        self.secondary_source_revision
    }

    /// Returns the exact text the operator must enter to consent.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        self.notice_revision
    }

    /// Returns the compatibility provenance label.
    #[must_use]
    pub const fn provenance(self) -> &'static str {
        self.provenance
    }

    /// Returns the Unix timestamp of the compatibility review.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        self.reviewed_at
    }

    /// Returns the hard Unix expiry of this compatibility profile.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        self.expires_at
    }

    /// Returns the local kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        self.kill_switch_revision
    }

    /// Reports whether this build has disabled the profile.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        self.killed
    }
}

/// One non-cloneable native login operation bound to its exact manager.
pub struct PendingNativeLogin {
    inner: PendingNativeLoginKind,
    consent: PendingCompatibilityConsent,
}

#[derive(Clone)]
struct PendingCompatibilityConsent {
    state: SubscriptionState,
    notice: CompatibilityNotice,
    exact_receipt: Vec<u8>,
    generation: String,
}

impl PendingCompatibilityConsent {
    async fn validate_at(&self, now: u64) -> Result<(), NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = self.notice;
        let expected = self.exact_receipt.clone();
        let expected_generation = self.generation.clone();
        run_blocking(move || {
            let current = paths.load_acceptance_exact(notice, now)?;
            if current.exact_bytes != expected || current.generation != expected_generation {
                return Err(NativeSubscriptionError::AcceptanceRequired);
            }
            Ok(())
        })
        .await?
    }

    async fn revoke_local_after_change(&self) -> Result<(), NativeSubscriptionError> {
        let status = NativeSubscriptions::from_state(self.state.clone())
            .logout_local_unlocked(self.notice.provider())
            .await?;
        if status != NativeAuthStatus::Revoked {
            return Err(NativeSubscriptionError::CredentialUnavailable);
        }
        Ok(())
    }

    fn arm_recovery(&self) -> Result<LoginRecoveryOperation, NativeSubscriptionError> {
        self.state
            .arm_login_recovery(self.notice.provider(), &self.generation)
    }

    fn clear_recovery(
        &self,
        operation: &LoginRecoveryOperation,
        credential_generation: u64,
    ) -> Result<(), NativeSubscriptionError> {
        self.state
            .clear_login_recovery(self.notice.provider(), operation, credential_generation)
    }

    fn restore_recovery(
        &self,
        operation: &LoginRecoveryOperation,
    ) -> Result<(), NativeSubscriptionError> {
        self.state
            .restore_login_recovery(self.notice.provider(), operation)
    }
}

enum PendingNativeLoginKind {
    CodexLocal {
        manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
        login: plakous_openai::CodexBrowserLogin,
    },
    CodexDevice {
        manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
        login: plakous_openai::CodexDeviceLogin,
    },
    XaiRemote(plakous_xai::XaiRemoteBrowserLogin),
    Claude(plakous_anthropic::ClaudeOAuthLogin),
}

impl PendingNativeLogin {
    /// Returns the exact authorization URL intended for display.
    #[must_use]
    pub fn instructions(&self) -> NativeLoginInstructions<'_> {
        match &self.inner {
            PendingNativeLoginKind::CodexLocal { login, .. } => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::Automatic,
                device_code: None,
            },
            PendingNativeLoginKind::CodexDevice { login, .. } => NativeLoginInstructions {
                verification_uri: login.verification_url(),
                completion: NativeLoginCompletion::DeviceCode,
                device_code: Some(NativeDeviceCode {
                    user_code: login.user_code(),
                    deadline: login.deadline(),
                }),
            },
            PendingNativeLoginKind::XaiRemote(login) => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
                device_code: None,
            },
            PendingNativeLoginKind::Claude(login) => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
                device_code: None,
            },
        }
    }

    /// Consumes the pending operation, validates the provider identity, and
    /// commits a Ready selected credential. A device-code login first waits
    /// for the operator's approval without touching local state, so dropping
    /// the future while it waits leaves no credential and no login recovery.
    ///
    /// The exact local compatibility-acceptance bytes captured when login
    /// began are revalidated immediately before provider completion and again
    /// before `Ready` is reported. Provider protocols cannot revoke an OAuth
    /// exchange already in flight when local consent changes mid-flow, so a
    /// post-completion local change fails closed without claiming readiness.
    ///
    /// # Errors
    ///
    /// Returns a redacted authentication, transport, profile, or storage error.
    pub async fn complete(self) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let Self { inner, consent } = self;
        match inner {
            PendingNativeLoginKind::CodexLocal { manager, login } => {
                complete_pending_with_consent(&consent, unix_now, || async move {
                    manager
                        .complete_browser_login(login)
                        .await
                        .map(map_codex_status)
                        .map_err(map_codex_error)
                })
                .await
            }
            PendingNativeLoginKind::CodexDevice { manager, login } => {
                let poller = Arc::clone(&manager);
                Box::pin(complete_approved_with_consent(
                    &consent,
                    unix_now,
                    async move {
                        poller
                            .poll_device_authorization(login)
                            .await
                            .map_err(map_codex_error)
                    },
                    |authorization| async move {
                        manager
                            .complete_device_authorization(authorization)
                            .await
                            .map(map_codex_status)
                            .map_err(map_codex_error)
                    },
                ))
                .await
            }
            PendingNativeLoginKind::XaiRemote(_) | PendingNativeLoginKind::Claude(_) => {
                Err(NativeSubscriptionError::Internal)
            }
        }
    }

    /// Completes one paste-complete browser login from the provider's result.
    ///
    /// xAI (local and remote) accepts the displayed Grok Build authorization code or exact
    /// callback URL — xAI's hosted page does not complete via loopback.
    /// Claude accepts the HTTPS callback URL, `code=` query, `code#state`, or
    /// a bare authorization code. Attempt-owned PKCE is retained.
    ///
    /// # Errors
    ///
    /// Returns a redacted authentication, transport, profile, or storage error.
    pub async fn complete_remote(
        self,
        authorization_input: &str,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let Self { inner, consent } = self;
        complete_pending_with_consent(&consent, unix_now, || async move {
            match inner {
                PendingNativeLoginKind::XaiRemote(login) => login
                    .complete(authorization_input)
                    .await
                    .map(|status| map_credential_status(status.status()))
                    .map_err(map_xai_error),
                PendingNativeLoginKind::Claude(login) => login
                    .complete(authorization_input)
                    .await
                    .map(|status| map_credential_status(status.status()))
                    .map_err(map_claude_error),
                PendingNativeLoginKind::CodexLocal { .. }
                | PendingNativeLoginKind::CodexDevice { .. } => {
                    Err(NativeSubscriptionError::Internal)
                }
            }
        })
        .await
    }
}

/// Waits for an out-of-band approval before any local state changes, then
/// completes it under the same consent, lock and recovery rules as every login.
async fn complete_approved_with_consent<A, T, F, Fut, C>(
    consent: &PendingCompatibilityConsent,
    now: C,
    approval: A,
    operation: F,
) -> Result<NativeAuthStatus, NativeSubscriptionError>
where
    A: Future<Output = Result<T, NativeSubscriptionError>>,
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = Result<NativeAuthStatus, NativeSubscriptionError>>,
    C: FnMut() -> Result<u64, NativeSubscriptionError>,
{
    let approved = approval.await?;
    complete_pending_with_consent(consent, now, || operation(approved)).await
}

async fn complete_pending_with_consent<F, Fut, C>(
    consent: &PendingCompatibilityConsent,
    mut now: C,
    operation: F,
) -> Result<NativeAuthStatus, NativeSubscriptionError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<NativeAuthStatus, NativeSubscriptionError>>,
    C: FnMut() -> Result<u64, NativeSubscriptionError>,
{
    let subscriptions = NativeSubscriptions::from_state(consent.state.clone());
    let lock = subscriptions
        .acquire_compatibility_lock(consent.notice.provider())
        .await?;
    consent.validate_at(now()?).await?;
    lock.verify()?;
    let recovery = consent.arm_recovery()?;
    if let Err(error) = consent.validate_at(now()?).await {
        lock.verify()?;
        consent.restore_recovery(&recovery)?;
        return Err(error);
    }
    lock.verify()?;
    let status = match operation().await {
        Ok(status) => status,
        Err(error) => {
            lock.verify()?;
            if consent
                .state
                .current_credential_generation(consent.notice.provider())?
                == recovery.credential_generation_before
            {
                consent.restore_recovery(&recovery)?;
                lock.verify()?;
            }
            return Err(error);
        }
    };
    lock.verify()?;
    if let Err(error) = consent.validate_at(now()?).await {
        consent.revoke_local_after_change().await?;
        lock.verify()?;
        consent
            .state
            .retire_login_recovery(consent.notice.provider())?;
        return Err(error);
    }
    if status != NativeAuthStatus::Ready {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    lock.verify()?;
    let generation = consent
        .state
        .current_credential_generation(consent.notice.provider())?
        .ok_or(NativeSubscriptionError::CredentialUnavailable)?;
    consent.clear_recovery(&recovery, generation)?;
    lock.verify()?;
    Ok(NativeAuthStatus::Ready)
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

async fn run_classified_model_catalog<F, Fut>(
    provider: NativeProvider,
    status: NativeSubscriptionStatus,
    operation: F,
) -> Result<Vec<NativeModelSummary>, NativeSubscriptionError>
where
    F: FnOnce(Arc<dyn SubscriptionRequestGate>) -> Fut,
    Fut: Future<Output = Result<Vec<NativeModelSummary>, NativeSubscriptionError>>,
{
    match status.compatibility() {
        CompatibilityStatus::Current => {}
        CompatibilityStatus::Inactive => return Err(NativeSubscriptionError::ProfileInactive),
        CompatibilityStatus::Required => {
            return Err(NativeSubscriptionError::AcceptanceRequired);
        }
    }
    if !dispatch_credential_admitted(provider, status.credential()) {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    let limits = SubscriptionRequestLimits::new(0, 1, 0, 0, Some(MODEL_CATALOG_DEADLINE), 0)
        .map_err(|_| NativeSubscriptionError::Internal)?;
    let gate: Arc<dyn SubscriptionRequestGate> = Arc::new(InMemorySubscriptionRequestGate::new(
        limits,
        Arc::new(SystemSubscriptionGateClock::default()),
    ));
    let models = tokio::time::timeout(MODEL_CATALOG_DEADLINE, operation(Arc::clone(&gate)))
        .await
        .map_err(|_| NativeSubscriptionError::Transport(None))??;
    let snapshot = gate
        .snapshot()
        .map_err(|_| NativeSubscriptionError::Internal)?;
    validate_model_catalog_snapshot(snapshot)?;
    Ok(models)
}

fn validate_model_catalog_snapshot(
    snapshot: SubscriptionLimitSnapshot,
) -> Result<(), NativeSubscriptionError> {
    let auth = snapshot.class(SubscriptionRequestClass::AuthRefresh);
    let catalog = snapshot.class(SubscriptionRequestClass::Catalog);
    let inference = snapshot.class(SubscriptionRequestClass::Inference);
    let exact_zero = |class: plakous_auth::subscription_limit::SubscriptionClassSnapshot| {
        class.allowance() == 0
            && class.reserved() == 0
            && class.dispatched() == 0
            && class.abandoned() == 0
            && class.denied() == 0
            && class.input_token_upper_bound() == 0
    };
    if !exact_zero(auth)
        || catalog.allowance() != 1
        || catalog.reserved() != 1
        || catalog.dispatched() != 1
        || catalog.abandoned() != 0
        || catalog.denied() != 0
        || catalog.input_token_upper_bound() != 0
        || !exact_zero(inference)
        || snapshot.retries() != 0
        || snapshot.max_elapsed() != Some(MODEL_CATALOG_DEADLINE)
        || snapshot.deadline_reached()
        || snapshot.elapsed() >= MODEL_CATALOG_DEADLINE
    {
        return Err(NativeSubscriptionError::Internal);
    }
    Ok(())
}

impl fmt::Debug for PendingNativeLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingNativeLogin(<redacted non-cloneable login>)")
    }
}

/// Curated owner of Plak's native subscription operations.
///
/// Construction is offline and accepts only fixed Plak-owned paths. Every
/// online operation validates exact persisted compatibility consent, creates a
/// provider-blind bounded client internally, and exposes no credential or
/// provider-native route object.
#[derive(Clone)]
pub struct NativeSubscriptions {
    state: SubscriptionState,
    exchange_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
}

impl NativeSubscriptions {
    fn from_state(state: SubscriptionState) -> Self {
        Self {
            state,
            exchange_observer: None,
        }
    }
    /// Creates the curated facade without filesystem or network I/O.
    #[must_use]
    pub const fn new(paths: SubscriptionPaths) -> Self {
        Self {
            state: SubscriptionState::new(paths),
            exchange_observer: None,
        }
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

    async fn acquire_compatibility_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<PrivateFilesystemLock, NativeSubscriptionError> {
        self.state.acquire_compatibility_lock(provider).await
    }

    async fn try_acquire_compatibility_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<PrivateFilesystemLock>, NativeSubscriptionError> {
        self.state.try_acquire_compatibility_lock(provider).await
    }

    /// Returns every immutable fact requiring explicit operator consent.
    #[must_use]
    pub fn notice(provider: NativeProvider) -> CompatibilityNotice {
        SubscriptionState::notice(provider)
    }

    /// Persists explicit consent only when the operator supplied the exact
    /// current notice revision.
    ///
    /// # Errors
    ///
    /// Rejects an inexact response, inactive profile, unsafe path, or failed
    /// atomic receipt write. No network operation occurs.
    pub async fn acknowledge_exact(
        &self,
        provider: NativeProvider,
        operator_response: &str,
    ) -> Result<(), NativeSubscriptionError> {
        let notice = Self::notice(provider);
        if operator_response != notice.notice_revision() {
            return Err(NativeSubscriptionError::AcceptanceRequired);
        }
        let lock = self.acquire_compatibility_lock(provider).await?;
        let paths = self.state.clone();
        run_blocking(move || {
            lock.verify()?;
            let now = unix_now()?;
            let previous = paths.stored_acceptance_generation(provider)?;
            let next = paths.store_acceptance(notice, now)?;
            if let Some(previous) = previous {
                paths.rebind_login_recovery_consent(provider, &previous, &next)?;
            }
            lock.verify()
        })
        .await?
    }

    /// Returns compatibility and credential state without provider network I/O.
    ///
    /// Absent storage is reported as signed out without creating any
    /// directory, lock, or record.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe, foreign, or malformed explicit storage.
    pub async fn status(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        if !run_blocking(move || paths.layout_exists()).await?? {
            return Ok(NativeSubscriptionStatus {
                compatibility: SubscriptionState::absent_compatibility(
                    Self::notice(provider),
                    unix_now()?,
                ),
                credential: NativeAuthStatus::SignedOut,
            });
        }
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
        lock.verify()?;
        Ok(status)
    }

    /// Performs one explicit, observational subscription allowance read.
    ///
    /// Codex, Grok, and Claude each use their reviewed provider-owned
    /// control-plane transport. No inferred request or inference dispatch
    /// occurs.
    pub async fn subscription_usage(&self, provider: NativeProvider) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(unix_now().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        // The compatibility lock is an `flock` whose parent-directory guard is
        // shared by every provider, so a concurrent dispatch validation, another
        // Plak process, or a sibling provider's read makes this non-blocking
        // attempt miss. That is contention, never a transport failure.
        let lock = match self.try_acquire_compatibility_lock(provider).await {
            Ok(Some(lock)) => lock,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        // Opening the provider manager performs no network I/O: every failure
        // here is local state, and a fenced credential is named by its exact
        // lifecycle rather than reported as absent or as a transport fault.
        let observation = match provider {
            NativeProvider::Codex => match self.codex_online().await {
                Ok(online) => online.manager.subscription_usage().await,
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    return unavailable(self.fenced_credential_reason(provider).await);
                }
                // A record this build refuses needs a new login.
                Err(NativeSubscriptionError::CredentialProfileChanged) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::LoginRequired,
                    ));
                }
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            },
            NativeProvider::Xai => match self.xai_online().await {
                Ok(online) => online.manager.subscription_usage().await,
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    return unavailable(self.fenced_credential_reason(provider).await);
                }
                // A record this build refuses needs a new login.
                Err(NativeSubscriptionError::CredentialProfileChanged) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::LoginRequired,
                    ));
                }
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            },
            NativeProvider::Claude => match self.claude_online().await {
                Ok(online) => online.manager.subscription_usage().await,
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    return unavailable(self.fenced_credential_reason(provider).await);
                }
                // A record this build refuses needs a new login.
                Err(NativeSubscriptionError::CredentialProfileChanged) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::LoginRequired,
                    ));
                }
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            },
        };
        if lock.verify().is_err() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }
        observation
    }

    /// Names why the selected credential was refused before any network I/O:
    /// absent is `NotConnected`; a present record is its exact lifecycle; a
    /// Ready record fenced by armed login recovery is local state.
    async fn fenced_credential_reason(
        &self,
        provider: NativeProvider,
    ) -> SubscriptionUsageUnavailable {
        match self
            .status_unlocked(provider)
            .await
            .map(NativeSubscriptionStatus::credential)
        {
            Ok(NativeAuthStatus::SignedOut) => SubscriptionUsageUnavailable::NotConnected,
            Ok(NativeAuthStatus::RefreshRequired) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ),
            Ok(NativeAuthStatus::Quarantined) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined,
            ),
            Ok(NativeAuthStatus::Revoked) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Revoked,
            ),
            Ok(NativeAuthStatus::LoginRequired) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::LoginRequired,
            ),
            Ok(NativeAuthStatus::Ready) | Err(_) => SubscriptionUsageUnavailable::LocalState,
        }
    }

    async fn status_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        run_blocking(move || paths.prepare()).await??;
        let credential = match provider {
            NativeProvider::Codex => plakous_openai::CodexSubscriptionLocalAdmin::open(
                self.state.provider_record(provider),
            )
            .await
            .map_err(map_codex_error)?
            .status()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error)?,
            NativeProvider::Xai => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let admin = plakous_xai::XaiOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_xai_error)?;
                    admin
                        .status()
                        .map(map_xai_offline_status)
                        .map_err(map_xai_error)
                })
                .await??
            }
            NativeProvider::Claude => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let admin =
                        plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(path, slot)
                            .map_err(map_claude_error)?;
                    admin
                        .status()
                        .map(map_claude_offline_status)
                        .map_err(map_claude_error)
                })
                .await??
            }
        };
        let notice = Self::notice(provider);
        let paths = self.state.clone();
        let consent_generation = run_blocking(move || {
            let now = unix_now()?;
            match paths.load_acceptance_exact(notice, now) {
                Ok(acceptance) => Ok(Some(acceptance.generation)),
                // A stale or inactive acceptance is reported as compatibility;
                // the login stays judged against the acceptance it was made under.
                Err(
                    NativeSubscriptionError::AcceptanceRequired
                    | NativeSubscriptionError::ProfileInactive,
                ) => paths.stored_acceptance_generation(provider),
                Err(error) => Err(error),
            }
        })
        .await??;
        let paths = self.state.clone();
        let recovery_required = run_blocking(move || {
            paths.login_recovery_required_for_status(
                provider,
                consent_generation.as_deref(),
                credential,
            )
        })
        .await??;
        let compatibility = self.compatibility_status_unlocked(provider).await?;
        Ok(NativeSubscriptionStatus {
            compatibility,
            credential: if recovery_required {
                NativeAuthStatus::Quarantined
            } else {
                credential
            },
        })
    }

    /// Starts one exact local or remote login after compatibility admission.
    ///
    /// # Errors
    ///
    /// Rejects missing consent, unsafe storage, unavailable profile, local
    /// binding failure, or provider transport failure.
    pub async fn begin_login(
        &self,
        provider: NativeProvider,
        mode: NativeLoginMode,
    ) -> Result<PendingNativeLogin, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        // Login must open even when a prior credential is quarantined / recovery
        // is armed — the operator's explicit login is how they heal that state.
        // Retire only a quarantined slot. A healthy credential remains in place
        // until the new login commits, while an ambiguous prior login cannot
        // block its own explicit recovery path.
        if self.status_unlocked(provider).await?.credential() == NativeAuthStatus::Quarantined {
            let status = self.logout_local_unlocked(provider).await?;
            if !matches!(
                status,
                NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
            ) {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
            let paths = self.state.clone();
            run_blocking(move || paths.retire_login_recovery(provider)).await??;
            lock.verify()?;
        }
        let pending = match provider {
            NativeProvider::Codex => {
                let online = self.codex_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: self.state.clone(),
                    notice: Self::notice(provider),
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                let manager = online.manager;
                let inner = match mode {
                    NativeLoginMode::Browser => PendingNativeLoginKind::CodexLocal {
                        login: manager
                            .begin_browser_login()
                            .await
                            .map_err(map_codex_error)?,
                        manager,
                    },
                    NativeLoginMode::Remote => PendingNativeLoginKind::CodexDevice {
                        login: manager
                            .begin_device_login()
                            .await
                            .map_err(map_codex_error)?,
                        manager,
                    },
                };
                Ok(PendingNativeLogin { inner, consent })
            }
            NativeProvider::Xai => {
                let online = self.xai_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: self.state.clone(),
                    notice: Self::notice(provider),
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                // xAI's hosted page always shows a code to paste into Grok Build,
                // local or remote. Do not bind or wait on loopback.
                let inner = match mode {
                    NativeLoginMode::Browser | NativeLoginMode::Remote => {
                        PendingNativeLoginKind::XaiRemote(
                            online
                                .manager
                                .begin_remote_browser_login()
                                .map_err(map_xai_error)?,
                        )
                    }
                };
                Ok(PendingNativeLogin { inner, consent })
            }
            NativeProvider::Claude => {
                let online = self.claude_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: self.state.clone(),
                    notice: Self::notice(provider),
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                Ok(PendingNativeLogin {
                    inner: PendingNativeLoginKind::Claude(
                        online.manager.begin_login().map_err(map_claude_error)?,
                    ),
                    consent,
                })
            }
        }?;
        lock.verify()?;
        Ok(pending)
    }

    /// Admits one provider for a dispatch-shaped use (child spawn) exactly as
    /// the model-catalog and inference paths do: refresh first, then read the
    /// durable status, then apply [`dispatch_credential_admitted`].
    ///
    /// The refresh is best effort: its failure is reported through the
    /// fail-closed status read that follows, never swallowed into Ready. A
    /// durable xAI Unknown is reclaimed on the stored refresh token here, so a
    /// crash mid-refresh does not refuse the next child.
    ///
    /// # Errors
    ///
    /// Returns [`NativeSubscriptionError::CredentialUnavailable`] when the
    /// admission rule refuses the credential, and the status read's own
    /// storage errors otherwise.
    pub async fn admit_for_dispatch(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let _ = self.refresh(provider).await;
        let status = self.status(provider).await?;
        if !dispatch_credential_admitted(provider, status.credential()) {
            return Err(NativeSubscriptionError::CredentialUnavailable);
        }
        Ok(status)
    }

    /// Ensures the exact selected subscription credential is fresh enough for
    /// inference, without claiming that an already-fresh credential rotated.
    ///
    /// # Errors
    ///
    /// Codex/Claude leave an ambiguous rotating-token result quarantined.
    /// xAI retries the still-stored refresh token on a new claimed generation.
    ///
    pub async fn refresh(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = match provider {
            NativeProvider::Codex => {
                let online = self.codex_online().await?;
                online
                    .manager
                    .refresh_if_needed()
                    .await
                    .map_err(map_codex_error)?;
                online
                    .manager
                    .status()
                    .map(map_codex_status)
                    .map_err(map_codex_error)
            }
            NativeProvider::Claude => {
                let online = self.claude_online().await?;
                online
                    .manager
                    .refresh_if_needed()
                    .await
                    .map_err(map_claude_error)?;
                online
                    .manager
                    .status()
                    .map(|status| {
                        status.map_or(NativeAuthStatus::SignedOut, |value| {
                            map_credential_status(value.status())
                        })
                    })
                    .map_err(map_claude_error)
            }
            NativeProvider::Xai => {
                let online = self.xai_online().await?;
                online
                    .manager
                    .refresh_if_needed()
                    .await
                    .map_err(map_xai_error)?;
                online
                    .manager
                    .status()
                    .map(|status| {
                        status.map_or(NativeAuthStatus::SignedOut, |value| {
                            map_credential_status(value.status())
                        })
                    })
                    .map_err(map_xai_error)
            }
        }?;
        // Refresh legitimately advances durable credential generations. Re-pin
        // the cleared login-recovery marker so the recovery gate does not treat
        // the owner-managed rotation as a foreign slot takeover.
        if matches!(
            status,
            NativeAuthStatus::Ready | NativeAuthStatus::RefreshRequired
        ) {
            let paths = self.state.clone();
            let consent = {
                let paths = self.state.clone();
                let notice = Self::notice(provider);
                let now = unix_now()?;
                run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
            };
            let consent_generation = consent.generation.clone();
            run_blocking(move || paths.repin_after_refresh(provider, &consent_generation))
                .await??;
        }
        lock.verify()?;
        Ok(status)
    }

    /// Attempts exact remote revocation and commits the provider-defined local
    /// tombstone only after the claimed transaction reaches that terminal.
    ///
    /// # Errors
    ///
    /// Ambiguous remote outcomes remain quarantined; they are not reported as
    /// local logout success.
    pub async fn revoke(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = match provider {
            NativeProvider::Codex => self
                .codex_online()
                .await?
                .manager
                .revoke()
                .await
                .map(map_codex_status)
                .map_err(map_codex_error),
            NativeProvider::Xai => {
                self.xai_online()
                    .await?
                    .manager
                    .revoke()
                    .await
                    .map_err(map_xai_error)?;
                Ok(NativeAuthStatus::Revoked)
            }
            NativeProvider::Claude => {
                self.claude_online()
                    .await?
                    .manager
                    .revoke()
                    .map_err(map_claude_error)?;
                Ok(NativeAuthStatus::Revoked)
            }
        }?;
        lock.verify()?;
        Ok(status)
    }

    /// Retires local credential and login-recovery state without acceptance or
    /// network I/O.
    ///
    /// An existing credential is replaced by a revoked tombstone; an absent
    /// credential remains signed out. In both cases, any paired login-recovery
    /// marker is retired. No remote revocation claim is made.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe, malformed, or stale selected-slot state.
    pub async fn logout_local(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.logout_local_unlocked(provider).await?;
        lock.verify()?;
        if matches!(
            status,
            NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
        ) {
            let paths = self.state.clone();
            run_blocking(move || paths.retire_login_recovery(provider)).await??;
        }
        lock.verify()?;
        Ok(status)
    }

    async fn logout_local_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        run_blocking(move || paths.prepare()).await??;
        match provider {
            NativeProvider::Codex => plakous_openai::CodexSubscriptionLocalAdmin::open(
                self.state.provider_record(provider),
            )
            .await
            .map_err(map_codex_error)?
            .logout_local()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error),
            NativeProvider::Xai => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    plakous_xai::XaiOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_xai_error)?
                        .logout_local()
                        .map(map_xai_offline_status)
                        .map_err(map_xai_error)
                })
                .await?
            }
            NativeProvider::Claude => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_claude_error)?
                        .logout_local()
                        .map(map_claude_offline_status)
                        .map_err(map_claude_error)
                })
                .await?
            }
        }
    }

    /// Discovers the current authenticated account-scoped model catalog.
    ///
    /// Compatibility acceptance is validated before constructing an online
    /// provider manager or attempting authenticated discovery. Returned
    /// entries contain exact IDs, conservative portable capability bits, and
    /// exact provider-authored catalog defaults. A reported default is
    /// observational metadata, never a Plakous selection or request default.
    ///
    /// # Errors
    ///
    /// Rejects missing or inactive compatibility consent, unavailable
    /// credentials, malformed or stale catalogs, and bounded transport
    /// failures. No unauthenticated or static fallback is attempted.
    pub async fn model_catalog(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
        let models = run_classified_model_catalog(provider, status, |gate| async move {
            match provider {
                NativeProvider::Codex => {
                    let online = self.codex_online_with_gate(Some(gate), true).await?;
                    let resolver = plakous_openai::CodexModelResolver::new(
                        Arc::clone(&online.manager),
                        online.client,
                        codex_native_http_config()?,
                    )
                    .map_err(map_codex_error)?;
                    let catalog = resolver.refresh_catalog().await.map_err(map_codex_error)?;
                    Ok(catalog
                        .entries()
                        .iter()
                        .filter(|entry| entry.supports_text())
                        .map(|entry| NativeModelSummary {
                            exact_id: entry.model_id().to_owned(),
                            visibility: entry
                                .visibility()
                                .map_or(NativeModelVisibility::None, facade_codex_visibility),
                            native_tool_mode: facade_native_tool_mode(entry.tool_mode()),
                            streaming: true,
                            tools: entry.supports_tools(),
                            reasoning_efforts: entry.reasoning_effort_options().to_vec(),
                            default_reasoning_effort: entry.default_reasoning_effort(),
                            service_tiers: entry
                                .service_tiers()
                                .iter()
                                .map(|tier| NativeServiceTierSummary {
                                    id: tier.id().to_owned(),
                                    name: tier.name().to_owned(),
                                    description: tier.description().to_owned(),
                                })
                                .collect(),
                            default_service_tier: entry.default_service_tier().map(str::to_owned),
                            reported_context_length: entry
                                .capacity_facts()
                                .context_window()
                                .map(|window| u64::from(window.get())),
                        })
                        .collect())
                }
                NativeProvider::Xai => {
                    {
                        let online = self.xai_online_with_gate(None, true).await?;
                        online
                            .manager
                            .refresh_if_needed()
                            .await
                            .map_err(map_xai_error)?;
                    }
                    let online = self.xai_online_with_gate(Some(gate), true).await?;
                    let receipt = online
                        .manager
                        .refresh_catalog()
                        .await
                        .map_err(map_xai_error)?;
                    Ok(receipt
                        .entries()
                        .iter()
                        .filter(|entry| facade_xai_model_is_selectable(entry.model().as_str()))
                        .map(|entry| NativeModelSummary {
                            exact_id: entry.model().as_str().to_owned(),
                            visibility: NativeModelVisibility::List,
                            native_tool_mode: None,
                            streaming: true,
                            tools: true,
                            reasoning_efforts:
                                plakous_xai::subscription_model_reasoning_effort_options(
                                    entry.model().as_str(),
                                )
                                .to_vec(),
                            default_reasoning_effort: None,
                            service_tiers: Vec::new(),
                            default_service_tier: None,
                            reported_context_length: Some(u64::from(
                                entry.diagnostics().reported_context_length().get(),
                            )),
                        })
                        .collect())
                }
                NativeProvider::Claude => {
                    let online = self
                        .claude_online_with_gate(Some(gate.clone()), true)
                        .await?;
                    let mut permit = gate
                        .acquire(SubscriptionRequestClass::Catalog)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    permit
                        .authorize_dispatch()
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    Ok(online
                        .manager
                        .enrolled_catalog()
                        .map_err(map_claude_error)?
                        .into_iter()
                        .map(|entry| NativeModelSummary {
                            exact_id: entry.exact_id().to_owned(),
                            visibility: NativeModelVisibility::List,
                            native_tool_mode: None,
                            streaming: true,
                            tools: true,
                            reasoning_efforts:
                                plakous_anthropic::claude_oauth_reasoning_effort_options(
                                    entry.exact_id(),
                                )
                                .to_vec(),
                            default_reasoning_effort: None,
                            service_tiers: Vec::new(),
                            default_service_tier: None,
                            // Claude publishes no OAuth catalog window; this is
                            // the enrolled row's window, the one the session runs at.
                            reported_context_length: Some(entry.input_tokens()),
                        })
                        .collect())
                }
            }
        })
        .await?;
        if models.is_empty() {
            return Err(NativeSubscriptionError::ModelUnavailable);
        }
        lock.verify()?;
        let catalog = NativeModelCatalog { provider, models };
        let _ = self
            .state
            .persist_catalog_snapshot(provider, catalog.models());
        Ok(catalog)
    }

    /// Hydrates the last persisted authenticated catalog without network I/O.
    ///
    /// Used for local-first TUI attach. A missing snapshot fails closed; stale
    /// snapshots remain displayable until a background refresh succeeds.
    ///
    /// # Errors
    ///
    /// Rejects missing compatibility consent, unavailable credentials, absent
    /// snapshots, and malformed persisted data. Never performs catalog refresh.
    pub async fn model_catalog_persisted(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
        match status.compatibility() {
            CompatibilityStatus::Current => {}
            CompatibilityStatus::Inactive => return Err(NativeSubscriptionError::ProfileInactive),
            CompatibilityStatus::Required => {
                return Err(NativeSubscriptionError::AcceptanceRequired);
            }
        }
        if status.credential() != NativeAuthStatus::Ready {
            return Err(NativeSubscriptionError::CredentialUnavailable);
        }
        let paths = self.state.clone();
        let catalog = run_blocking(move || paths.load_persisted_catalog(provider)).await??;
        lock.verify()?;
        Ok(catalog)
    }

    fn attach_compile_authority_from_codex(
        requested_model: &ModelId,
        descriptor: &ResolvedModelDescriptor,
        evidence: &plakous_openai::CodexModelResolutionEvidence,
    ) -> Result<RuntimeModelAuthority, NativeModelResolutionError> {
        RuntimeModelAuthority::new(
            descriptor,
            evidence.account_fingerprint(),
            evidence.credential_authority_binding(),
            evidence.selected_credential_generation(),
            evidence.catalog_observed_at(),
            Some(evidence.catalog_expires_at()),
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
            )
        })
    }

    fn attach_compile_authority_from_xai(
        requested_model: &ModelId,
        descriptor: &ResolvedModelDescriptor,
        evidence: &plakous_xai::XaiModelResolutionEvidence,
    ) -> Result<RuntimeModelAuthority, NativeModelResolutionError> {
        RuntimeModelAuthority::new(
            descriptor,
            evidence.account_fingerprint(),
            evidence.credential_authority_binding(),
            evidence.selected_credential_generation(),
            evidence.catalog_observed_at(),
            Some(evidence.catalog_expires_at()),
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
            )
        })
        .map(|authority| {
            authority.with_admission_witness(evidence.runtime_admission_witness().clone())
        })
    }

    /// Resolves one exact native model under a classified physical-request gate.
    ///
    /// This is the library **admit** seam for any caller that needs the
    /// fail-closed catalog + transport gate. It is not a general uncapped model
    /// client: the same gate is installed on auth refresh, catalog discovery, and
    /// every resolved inference transport.
    ///
    /// # Errors
    ///
    /// Returns [`NativeModelResolutionError`] when compatibility locks, online
    /// admission, catalog selection, or evidence binding fail. `selected_model`
    /// is set only after a concrete id was chosen.
    #[allow(
        clippy::too_many_lines,
        reason = "the provider branches keep exact selection and pre/post-selection evidence explicit"
    )]
    pub(crate) async fn resolve_gated_native_model(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        lineage: SessionLineage,
    ) -> Result<ResolvedNativeModel, NativeModelResolutionError> {
        self.resolve_gated_native_model_phased(provider, model_selector, gate, false, lineage)
            .await
            .map(|(model, _)| model)
    }

    async fn resolve_gated_native_model_for_attach(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<(ResolvedNativeModel, RuntimeModelAuthority), NativeModelResolutionError> {
        let (model, compile_authority) = self
            .resolve_gated_native_model_phased(
                provider,
                model_selector,
                gate,
                true,
                SessionLineage::Root,
            )
            .await?;
        let compile_authority = compile_authority.ok_or_else(|| {
            NativeModelResolutionError::after_selection(
                model.selected_model.clone(),
                NativeSubscriptionError::Internal,
            )
        })?;
        Ok((model, compile_authority))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the provider branches keep exact selection and pre/post-selection evidence explicit"
    )]
    async fn resolve_gated_native_model_phased(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        attach: bool,
        lineage: SessionLineage,
    ) -> Result<(ResolvedNativeModel, Option<RuntimeModelAuthority>), NativeModelResolutionError>
    {
        model_selector
            .validate_for_provider(provider)
            .map_err(|_| {
                NativeModelResolutionError::before_selection(
                    NativeSubscriptionError::ModelUnavailable,
                )
            })?;
        let _lock = self
            .acquire_compatibility_lock(provider)
            .await
            .map_err(NativeModelResolutionError::before_selection)?;
        match provider {
            NativeProvider::Claude => {
                let NativeModelSelector::Exact(selected_model) = model_selector else {
                    return Err(NativeModelResolutionError::before_selection(
                        NativeSubscriptionError::ModelUnavailable,
                    ));
                };
                let selected_model = selected_model.clone();
                let online = self
                    .claude_online_with_gate(Some(Arc::clone(&gate)), true)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let client = plakous_http::Client::new_http1(
                    plakous_http::Limits::for_streaming_inference(),
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let client = client
                    .pooled()
                    .with_observer(self.exchange_observer.clone());
                if !attach {
                    online.manager.refresh_if_needed().await.map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_claude_error(error),
                        )
                    })?;
                }
                let mut permit = gate
                    .acquire(SubscriptionRequestClass::Catalog)
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::Internal,
                        )
                    })?;
                permit.authorize_dispatch().map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let (model, authority, descriptor) = online
                    .manager
                    .resolve_model(selected_model.as_str(), client, lineage)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_claude_error(error),
                        )
                    })?;
                drop(permit);
                let catalog_expires_at = authority.valid_until().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::ResolutionEvidenceRejected,
                    )
                })?;
                if authority.observed_at() >= catalog_expires_at {
                    return Err(NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::ResolutionEvidenceRejected,
                    ));
                }
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: authority.account_binding().to_string(),
                    credential_authority_binding: authority
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: authority.credential_generation(),
                    credential_selection_epoch: "primary".to_owned(),
                    route_fingerprint: plakous_anthropic::CLAUDE_OAUTH_ROUTE_ID.to_owned(),
                    descriptor_digest: descriptor.binding_digest().map_or_else(
                        |_| "claude-oauth-descriptor".to_owned(),
                        |digest| digest.to_string(),
                    ),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: plakous_anthropic::CLAUDE_OAUTH_ADAPTER_REVISION.to_owned(),
                    catalog_observed_at: authority.observed_at(),
                    catalog_expires_at,
                    runtime_admission_witness: None,
                    profile_revision: plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
                        .to_owned(),
                    codex_exact: None,
                    xai_exact: None,
                };
                let resolved = ResolvedNativeModel {
                    selected_model,
                    descriptor,
                    model,
                    evidence,
                };
                let compile_authority = attach.then_some(authority);
                Ok((resolved, compile_authority))
            }
            NativeProvider::Codex => {
                let exact_selection = model_selector.exact_model().cloned();
                let online = self
                    .codex_online_with_gate(Some(gate), true)
                    .await
                    .map_err(|error| match exact_selection.clone() {
                        Some(selected) => {
                            NativeModelResolutionError::after_selection(selected, error)
                        }
                        None => NativeModelResolutionError::before_selection(error),
                    })?;
                let resolver = plakous_openai::CodexModelResolver::new(
                    Arc::clone(&online.manager),
                    online.client,
                    codex_native_http_config().map_err(|error| {
                        exact_selection.clone().map_or_else(
                            || NativeModelResolutionError::before_selection(error),
                            |selected| NativeModelResolutionError::after_selection(selected, error),
                        )
                    })?,
                )
                .map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::ProviderClientConstructionRejected
                        }
                        other => map_codex_error(other),
                    };
                    exact_selection.clone().map_or_else(
                        || NativeModelResolutionError::before_selection(error),
                        |selected| NativeModelResolutionError::after_selection(selected, error),
                    )
                })?
                .with_inference_observer(self.exchange_observer.clone());
                let catalog = resolver.refresh_catalog().await.map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::CatalogValidationRejected
                        }
                        other => map_codex_error(other),
                    };
                    exact_selection.clone().map_or_else(
                        || NativeModelResolutionError::before_selection(error),
                        |selected| NativeModelResolutionError::after_selection(selected, error),
                    )
                })?;
                let selected_model = match model_selector {
                    NativeModelSelector::Exact(model) => model.clone(),
                    NativeModelSelector::CodexFirstCompatible => {
                        let selected =
                            first_codex_compatible_model(catalog.entries().iter().map(|entry| {
                                (
                                    entry.model_id(),
                                    entry.supports_text(),
                                    entry.supports_tools(),
                                    entry.supports_reasoning(),
                                )
                            }))
                            .ok_or_else(|| {
                                NativeModelResolutionError::before_selection(
                                    NativeSubscriptionError::ModelUnavailable,
                                )
                            })?;
                        ModelId::new(selected.to_owned()).map_err(|_| {
                            NativeModelResolutionError::before_selection(
                                NativeSubscriptionError::ModelUnavailable,
                            )
                        })?
                    }
                };
                let (model, resolution) = resolver
                    .resolve_with_evidence(selected_model.as_str())
                    .map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::ResolutionEvidenceRejected
                        }
                        other => map_codex_error(other),
                    };
                    NativeModelResolutionError::after_selection(selected_model.clone(), error)
                })?;
                let model = guidance::with_content_filter_guidance(
                    model,
                    resolution.content_filter_guidance(),
                );
                let descriptor = resolution
                    .validated_descriptor()
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::ResolutionEvidenceRejected,
                        )
                    })?
                    .clone();
                let compile_authority = if attach {
                    Some(Self::attach_compile_authority_from_codex(
                        &selected_model,
                        &descriptor,
                        &resolution,
                    )?)
                } else {
                    None
                };
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: resolution.account_fingerprint().to_string(),
                    credential_authority_binding: resolution
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: resolution.selected_credential_generation(),
                    credential_selection_epoch: resolution.credential_selection_epoch().to_string(),
                    route_fingerprint: resolution.route_fingerprint().to_string(),
                    descriptor_digest: resolution.descriptor_digest().to_string(),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: resolution.adapter_revision().to_owned(),
                    catalog_observed_at: resolution.catalog_observed_at(),
                    catalog_expires_at: resolution.catalog_expires_at(),
                    runtime_admission_witness: None,
                    profile_revision: resolution.profile_revision().to_owned(),
                    codex_exact: Some(resolution),
                    xai_exact: None,
                };
                Ok((
                    ResolvedNativeModel {
                        selected_model,
                        descriptor,
                        model,
                        evidence,
                    },
                    compile_authority,
                ))
            }
            NativeProvider::Xai => {
                let NativeModelSelector::Exact(selected_model) = model_selector else {
                    return Err(NativeModelResolutionError::before_selection(
                        NativeSubscriptionError::ModelUnavailable,
                    ));
                };
                let selected_model = selected_model.clone();
                let online =
                    self.xai_online_with_gate(Some(gate), true)
                        .await
                        .map_err(|error| {
                            NativeModelResolutionError::after_selection(
                                selected_model.clone(),
                                error,
                            )
                        })?;
                let config = xai_native_http_config().map_err(|error| {
                    NativeModelResolutionError::after_selection(selected_model.clone(), error)
                })?;
                let inference_client = xai_inference_client(self.exchange_observer.clone())
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let (model, resolution) = online
                    .manager
                    .resolve_model_with_evidence(selected_model.as_str(), inference_client, config)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_xai_error(error),
                        )
                    })?;
                let descriptor = resolution
                    .validated_descriptor()
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::ResolutionEvidenceRejected,
                        )
                    })?
                    .clone();
                let compile_authority = if attach {
                    Some(Self::attach_compile_authority_from_xai(
                        &selected_model,
                        &descriptor,
                        &resolution,
                    )?)
                } else {
                    None
                };
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: resolution.account_fingerprint().to_string(),
                    credential_authority_binding: resolution
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: resolution.selected_credential_generation(),
                    credential_selection_epoch: resolution.credential_selection_epoch().to_owned(),
                    route_fingerprint: resolution.route_fingerprint().to_owned(),
                    descriptor_digest: resolution.descriptor_digest().to_string(),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: resolution.adapter_revision().to_owned(),
                    catalog_observed_at: resolution.catalog_observed_at(),
                    catalog_expires_at: resolution.catalog_expires_at(),
                    runtime_admission_witness: Some(resolution.runtime_admission_witness().clone()),
                    profile_revision: resolution.profile_revision().to_owned(),
                    codex_exact: None,
                    xai_exact: Some(resolution),
                };
                Ok((
                    ResolvedNativeModel {
                        selected_model,
                        descriptor,
                        model,
                        evidence,
                    },
                    compile_authority,
                ))
            }
        }
    }

    /// Resolves one complete explicit native provider configuration.
    ///
    /// The literal provider route, model, credential selection, behavior, and
    /// product policy are supplied by the caller. Resolution accepts only an
    /// exact pinned native route, obtains fresh authority from the same model
    /// object that will execute, and compiles through that provider's actual
    /// Responses policy table.
    ///
    /// # Errors
    ///
    /// Returns a typed native resolution failure when the selected route is not
    /// exact, live authority cannot be revalidated, catalog facts reject the
    /// requested behavior, or family compilation fails.
    pub async fn resolve_gated_provider(
        &self,
        selected: SelectedProviderConfiguration,
        gate: Arc<dyn SubscriptionRequestGate>,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let requested_model = selected.route().requested_model().clone();
        let provider = [
            NativeProvider::Codex,
            NativeProvider::Xai,
            NativeProvider::Claude,
        ]
        .into_iter()
        .find(|provider| {
            native_provider_route_selection(*provider, requested_model.clone())
                .is_ok_and(|route| &route == selected.route())
        })
        .ok_or_else(|| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderConfigurationRejected,
            )
        })?;
        let selector = NativeModelSelector::Exact(requested_model.clone());
        let resolved = self
            .resolve_gated_native_model(provider, &selector, gate, lineage)
            .await?;
        let native_tool_mode = resolved
            .evidence
            .codex_exact
            .as_ref()
            .and_then(codex_native_tool_mode);
        let checked_at = timestamp_now().map_err(|error| {
            NativeModelResolutionError::after_selection(requested_model.clone(), error)
        })?;
        let authority = resolved
            .model()
            .revalidate_runtime_authority(checked_at)
            .await
            .map_err(|_| {
                NativeModelResolutionError::after_selection(
                    requested_model.clone(),
                    NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
                )
            })?;
        let configuration = match provider {
            NativeProvider::Codex => {
                let evidence = resolved.evidence.codex_exact.as_ref().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                plakous_openai::compile_codex_subscription_configuration(
                    &selected, &authority, evidence,
                )
            }
            NativeProvider::Xai => {
                let evidence = resolved.evidence.xai_exact.as_ref().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                plakous_xai::compile_supergrok_subscription_configuration(
                    &selected, &authority, evidence,
                )
            }
            NativeProvider::Claude => {
                let beta = plakous_anthropic::claude_oauth_beta_header(requested_model.as_str());
                let evidence = plakous_model::ProviderSemanticEvidence::new(
                    1,
                    b"plakous-anthropic/claude-oauth-semantic-evidence-v1".to_vec(),
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ProviderConfigurationIdentityRejected,
                    )
                })?;
                let binding = ResolvedProviderAuthorityBinding::new(
                    &resolved.descriptor,
                    &authority,
                    evidence,
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ProviderConfigurationIdentityRejected,
                    )
                })?;
                let configuration = plakous_anthropic::compile_messages_oauth_configuration(
                    &selected,
                    resolved.descriptor.clone(),
                    binding,
                    plakous_anthropic::ANTHROPIC_API_VERSION,
                    beta,
                )
                .map_err(|error| {
                    let error = match error {
                        plakous_anthropic::MessagesProviderConfigurationError::Identity => {
                            NativeSubscriptionError::ProviderConfigurationIdentityRejected
                        }
                        plakous_anthropic::MessagesProviderConfigurationError::PolicyMismatch
                        | plakous_anthropic::MessagesProviderConfigurationError::Provider(_) => {
                            NativeSubscriptionError::ProviderConfigurationRejected
                        }
                    };
                    NativeModelResolutionError::after_selection(requested_model.clone(), error)
                })?;
                let configured = ConfiguredLanguageModel::new(
                    &plakous_model::StandardIntegrationAuthority::release_owned(),
                    resolved.model(),
                    configuration,
                    authority,
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ConfiguredAggregateRejected,
                    )
                })?;
                return Ok(ResolvedProvider {
                    model: resolved,
                    configured,
                    native_tool_mode,
                    reresolution: None,
                });
            }
        }
        .map_err(|error| {
            let error = match error {
                plakous_openai::ResponsesProviderConfigurationError::PolicyMismatch(field) => {
                    NativeSubscriptionError::ProviderConfigurationPolicyMismatch(field)
                }
                plakous_openai::ResponsesProviderConfigurationError::Identity => {
                    NativeSubscriptionError::ProviderConfigurationIdentityRejected
                }
                plakous_openai::ResponsesProviderConfigurationError::ProviderConfiguration(
                    error,
                ) => NativeSubscriptionError::ProviderConfigurationCompileRejected(error),
                _ => NativeSubscriptionError::ProviderConfigurationRejected,
            };
            NativeModelResolutionError::after_selection(requested_model, error)
        })?;
        let configured = ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            resolved.model(),
            configuration,
            authority,
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                resolved.selected_model.clone(),
                NativeSubscriptionError::ConfiguredAggregateRejected,
            )
        })?;
        Ok(ResolvedProvider {
            model: resolved,
            configured,
            native_tool_mode,
            reresolution: None,
        })
    }

    /// Local-first attach variant of [`Self::resolve_gated_provider`].
    ///
    /// Skips attach-time `revalidate_runtime_authority` and Claude OAuth
    /// `refresh_if_needed`. Codex and Grok still perform authenticated catalog
    /// discovery. First dispatch still revalidates.
    pub async fn resolve_gated_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let requested_model = selected.route().requested_model().clone();
        let provider = [
            NativeProvider::Codex,
            NativeProvider::Xai,
            NativeProvider::Claude,
        ]
        .into_iter()
        .find(|provider| {
            native_provider_route_selection(*provider, requested_model.clone())
                .is_ok_and(|route| &route == selected.route())
        })
        .ok_or_else(|| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderConfigurationRejected,
            )
        })?;
        let selector = NativeModelSelector::Exact(requested_model.clone());
        let (resolved, authority) = self
            .resolve_gated_native_model_for_attach(provider, &selector, gate)
            .await?;
        let native_tool_mode = resolved
            .evidence
            .codex_exact
            .as_ref()
            .and_then(codex_native_tool_mode);
        let configuration = match provider {
            NativeProvider::Codex => {
                let evidence = resolved.evidence.codex_exact.as_ref().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                plakous_openai::compile_codex_subscription_configuration(
                    &selected, &authority, evidence,
                )
            }
            NativeProvider::Xai => {
                let evidence = resolved.evidence.xai_exact.as_ref().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                plakous_xai::compile_supergrok_subscription_configuration(
                    &selected, &authority, evidence,
                )
            }
            NativeProvider::Claude => {
                let beta = plakous_anthropic::claude_oauth_beta_header(requested_model.as_str());
                let evidence = plakous_model::ProviderSemanticEvidence::new(
                    1,
                    b"plakous-anthropic/claude-oauth-semantic-evidence-v1".to_vec(),
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ProviderConfigurationIdentityRejected,
                    )
                })?;
                let binding = ResolvedProviderAuthorityBinding::new(
                    &resolved.descriptor,
                    &authority,
                    evidence,
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ProviderConfigurationIdentityRejected,
                    )
                })?;
                let configuration = plakous_anthropic::compile_messages_oauth_configuration(
                    &selected,
                    resolved.descriptor.clone(),
                    binding,
                    plakous_anthropic::ANTHROPIC_API_VERSION,
                    beta,
                )
                .map_err(|error| {
                    let error = match error {
                        plakous_anthropic::MessagesProviderConfigurationError::Identity => {
                            NativeSubscriptionError::ProviderConfigurationIdentityRejected
                        }
                        plakous_anthropic::MessagesProviderConfigurationError::PolicyMismatch
                        | plakous_anthropic::MessagesProviderConfigurationError::Provider(_) => {
                            NativeSubscriptionError::ProviderConfigurationRejected
                        }
                    };
                    NativeModelResolutionError::after_selection(requested_model.clone(), error)
                })?;
                let configured = ConfiguredLanguageModel::new(
                    &plakous_model::StandardIntegrationAuthority::release_owned(),
                    resolved.model(),
                    configuration,
                    authority,
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ConfiguredAggregateRejected,
                    )
                })?;
                return Ok(ResolvedProvider {
                    model: resolved,
                    configured,
                    native_tool_mode,
                    reresolution: None,
                });
            }
        }
        .map_err(|error| {
            let error = match error {
                plakous_openai::ResponsesProviderConfigurationError::PolicyMismatch(field) => {
                    NativeSubscriptionError::ProviderConfigurationPolicyMismatch(field)
                }
                plakous_openai::ResponsesProviderConfigurationError::Identity => {
                    NativeSubscriptionError::ProviderConfigurationIdentityRejected
                }
                plakous_openai::ResponsesProviderConfigurationError::ProviderConfiguration(
                    error,
                ) => NativeSubscriptionError::ProviderConfigurationCompileRejected(error),
                _ => NativeSubscriptionError::ProviderConfigurationRejected,
            };
            NativeModelResolutionError::after_selection(requested_model, error)
        })?;
        let configured = ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            resolved.model(),
            configuration,
            authority,
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                resolved.selected_model.clone(),
                NativeSubscriptionError::ConfiguredAggregateRejected,
            )
        })?;
        Ok(ResolvedProvider {
            model: resolved,
            configured,
            native_tool_mode,
            reresolution: None,
        })
    }

    /// Resolves an ordinary native provider with one nonrenewable inference
    /// budget per admitted logical root.
    ///
    /// Authentication and catalog requests share the supplied process-level
    /// limits. Every root turn or session workflow receives an isolated copy of
    /// the inference allowance and retains it until the runtime publishes that
    /// root's terminal result.
    pub async fn resolve_turn_scoped_provider(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let gate: Arc<dyn SubscriptionRequestGate> = turn_gate.clone();
        let resolved = self
            .resolve_gated_provider(selected.clone(), gate, lineage)
            .await?;
        let resolved = apply_turn_scoped_provider_gate(resolved, Arc::clone(&turn_gate))?;
        Ok(self.with_reresolution(resolved, selected, lineage, turn_gate))
    }

    /// Local-first attach variant of [`Self::resolve_turn_scoped_provider`].
    ///
    /// Same turn gate and signature as dispatch resolution. Attach skips Claude
    /// `refresh_if_needed` and runtime authority revalidation; Codex/Grok catalog
    /// HTTP may still run. First dispatch still revalidates.
    pub async fn resolve_turn_scoped_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let gate: Arc<dyn SubscriptionRequestGate> = turn_gate.clone();
        let resolved = self
            .resolve_gated_provider_for_attach(selected.clone(), gate)
            .await?;
        let resolved = apply_turn_scoped_provider_gate(resolved, Arc::clone(&turn_gate))?;
        Ok(self.with_reresolution(resolved, selected, SessionLineage::Root, turn_gate))
    }

    async fn compatibility_status_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<CompatibilityStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(provider);
        match run_blocking(move || {
            let now = unix_now()?;
            paths.load_acceptance(notice, now)
        })
        .await?
        {
            Ok(_) => Ok(CompatibilityStatus::Current),
            Err(NativeSubscriptionError::AcceptanceRequired) => Ok(CompatibilityStatus::Required),
            Err(NativeSubscriptionError::ProfileInactive) => Ok(CompatibilityStatus::Inactive),
            Err(error) => Err(error),
        }
    }

    async fn codex_online(&self) -> Result<CodexOnline, NativeSubscriptionError> {
        self.codex_online_with_gate(None, true).await
    }

    /// Open Codex online surface for an explicit login (skips recovery quarantine).
    async fn codex_online_for_login(&self) -> Result<CodexOnline, NativeSubscriptionError> {
        self.codex_online_with_gate(None, false).await
    }

    async fn codex_online_with_gate(
        &self,
        request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
        enforce_login_recovery: bool,
    ) -> Result<CodexOnline, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(NativeProvider::Codex);
        let now = unix_now()?;
        run_blocking(move || paths.prepare()).await??;
        let loaded_acceptance = {
            let paths = self.state.clone();
            run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
        };
        let accepted_at = loaded_acceptance.accepted_at;
        let path = self.state.provider_record(NativeProvider::Codex);
        let local_status = plakous_openai::CodexSubscriptionLocalAdmin::open(path.clone())
            .await
            .map_err(map_codex_error)?
            .status()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error)?;
        if enforce_login_recovery {
            let paths = self.state.clone();
            let consent_generation = loaded_acceptance.generation.clone();
            if run_blocking(move || {
                paths.login_recovery_required_for_status(
                    NativeProvider::Codex,
                    Some(&consent_generation),
                    local_status,
                )
            })
            .await??
            {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
        }
        run_blocking(move || {
            let timestamp =
                Timestamp::new(accepted_at, 0).map_err(|_| NativeSubscriptionError::Internal)?;
            let provider_acceptance = plakous_openai::CodexCompatibilityAcceptance::accept_exact(
                notice.profile_id,
                notice.source_revision,
                notice.notice_revision,
                timestamp,
            )
            .map_err(map_codex_error)?;
            // The strict transport opens one connection per exchange, so H2
            // provides no multiplexing benefit here. Pin H1 before dispatch:
            // an OAuth POST cannot be retried safely after an H2 stream reset.
            let client = plakous_http::Client::new_http1(plakous_http::Limits::default())
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let mut manager = plakous_openai::CodexSubscriptionCredentialManager::new(
                path,
                client.clone(),
                provider_acceptance,
            )
            .map_err(map_codex_error)?;
            if let Some(gate) = request_gate {
                manager = manager.with_subscription_request_gate(gate);
            }
            if let Some(observer) = credential_refresh_observer(NativeProvider::Codex) {
                manager = manager.with_credential_refresh_observer(observer);
            }
            Ok(CodexOnline {
                manager: Arc::new(manager),
                client,
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }

    async fn xai_online(&self) -> Result<XaiOnline, NativeSubscriptionError> {
        self.xai_online_with_gate(None, true).await
    }

    async fn xai_online_for_login(&self) -> Result<XaiOnline, NativeSubscriptionError> {
        self.xai_online_with_gate(None, false).await
    }

    async fn xai_online_with_gate(
        &self,
        request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
        enforce_login_recovery: bool,
    ) -> Result<XaiOnline, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(NativeProvider::Xai);
        let now = unix_now()?;
        run_blocking(move || paths.prepare()).await??;
        let loaded_acceptance = {
            let paths = self.state.clone();
            run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
        };
        let accepted_at = loaded_acceptance.accepted_at;
        let path = self.state.provider_record(NativeProvider::Xai);
        let status_path = path.clone();
        let local_status = run_blocking(move || {
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let admin = plakous_xai::XaiOfflineSelectedSlot::open_explicit(status_path, slot)
                .map_err(map_xai_error)?;
            admin
                .status()
                .map(map_xai_offline_status)
                .map_err(map_xai_error)
        })
        .await??;
        if enforce_login_recovery {
            let paths = self.state.clone();
            let consent_generation = loaded_acceptance.generation.clone();
            if run_blocking(move || {
                paths.login_recovery_required_for_status(
                    NativeProvider::Xai,
                    Some(&consent_generation),
                    local_status,
                )
            })
            .await??
            {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
        }
        run_blocking(move || {
            let timestamp =
                Timestamp::new(accepted_at, 0).map_err(|_| NativeSubscriptionError::Internal)?;
            let secondary = notice
                .secondary_source_revision
                .ok_or(NativeSubscriptionError::Internal)?;
            let provider_acceptance = plakous_xai::XaiCompatibilityAcceptance::accept_exact(
                notice.profile_id,
                notice.source_revision,
                secondary,
                notice.notice_revision,
                notice.kill_switch_revision,
                timestamp,
            )
            .map_err(map_xai_error)?;
            // Auth and catalog keep the default phase bounds. Inference uses
            // its own H1 client with the model's longer response-head budget.
            let client = plakous_http::Client::new_http1(plakous_http::Limits::default())
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let mut manager = plakous_xai::XaiNativeAuthManager::open_selected_slot(
                path,
                slot,
                client,
                provider_acceptance,
            )
            .map_err(map_xai_error)?;
            if let Some(gate) = request_gate {
                manager = manager
                    .with_subscription_request_gate(gate)
                    .map_err(map_xai_error)?;
            }
            if let Some(observer) = credential_refresh_observer(NativeProvider::Xai) {
                manager = manager
                    .with_credential_refresh_observer(observer)
                    .map_err(map_xai_error)?;
            }
            Ok(XaiOnline {
                manager: Arc::new(manager),
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }

    async fn claude_online(&self) -> Result<ClaudeOnline, NativeSubscriptionError> {
        self.claude_online_with_gate(None, true).await
    }

    async fn claude_online_for_login(&self) -> Result<ClaudeOnline, NativeSubscriptionError> {
        self.claude_online_with_gate(None, false).await
    }

    async fn claude_online_with_gate(
        &self,
        request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
        enforce_login_recovery: bool,
    ) -> Result<ClaudeOnline, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(NativeProvider::Claude);
        let now = unix_now()?;
        run_blocking(move || paths.prepare()).await??;
        let loaded_acceptance = {
            let paths = self.state.clone();
            run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
        };
        let accepted_at = loaded_acceptance.accepted_at;
        let path = self.state.provider_record(NativeProvider::Claude);
        let status_path = path.clone();
        let local_status = run_blocking(move || {
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let admin =
                plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(status_path, slot)
                    .map_err(map_claude_error)?;
            admin
                .status()
                .map(map_claude_offline_status)
                .map_err(map_claude_error)
        })
        .await??;
        if enforce_login_recovery {
            let paths = self.state.clone();
            let consent_generation = loaded_acceptance.generation.clone();
            if run_blocking(move || {
                paths.login_recovery_required_for_status(
                    NativeProvider::Claude,
                    Some(&consent_generation),
                    local_status,
                )
            })
            .await??
            {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
        }
        let identity_paths = self.state.clone();
        run_blocking(move || {
            let timestamp =
                Timestamp::new(accepted_at, 0).map_err(|_| NativeSubscriptionError::Internal)?;
            let provider_acceptance =
                plakous_anthropic::ClaudeCompatibilityAcceptance::accept_exact(
                    notice.profile_id,
                    notice.source_revision,
                    notice.notice_revision,
                    notice.kill_switch_revision,
                    timestamp,
                )
                .map_err(map_claude_error)?;
            let client = plakous_http::Client::new_http1(plakous_http::Limits::default())
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let mut manager = plakous_anthropic::ClaudeNativeAuthManager::open_selected_slot(
                path,
                slot,
                client,
                provider_acceptance,
                identity_paths.claude_client_device_id()?,
            )
            .map_err(map_claude_error)?;
            if let Some(gate) = request_gate {
                manager = manager.with_subscription_request_gate(gate);
            }
            if let Some(observer) = credential_refresh_observer(NativeProvider::Claude) {
                manager = manager.with_credential_refresh_observer(observer);
            }
            Ok(ClaudeOnline {
                manager: Arc::new(manager),
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }
}

impl fmt::Debug for NativeSubscriptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSubscriptions(<redacted fixed Plak paths>)")
    }
}

struct CodexOnline {
    manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
    client: plakous_http::Client,
    acceptance: LoadedAcceptance,
}

struct XaiOnline {
    manager: Arc<plakous_xai::XaiNativeAuthManager>,
    acceptance: LoadedAcceptance,
}

fn xai_inference_client(
    observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
) -> Result<plakous_http::Client, NativeSubscriptionError> {
    plakous_http::Client::new_http1(plakous_http::Limits::for_streaming_inference())
        .map(|client| client.pooled().with_observer(observer))
        .map_err(|_| NativeSubscriptionError::Internal)
}

struct ClaudeOnline {
    manager: Arc<plakous_anthropic::ClaudeNativeAuthManager>,
    acceptance: LoadedAcceptance,
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

async fn run_blocking<F, T>(operation: F) -> Result<T, NativeSubscriptionError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| NativeSubscriptionError::Internal)
}
