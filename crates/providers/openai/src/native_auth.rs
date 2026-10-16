//! Plak-owned, revision-pinned ChatGPT/Codex authentication and model discovery.

mod catalog;
mod dispatch;
mod login;
mod refresh;
mod verify;

use catalog::{
    CatalogDocument, CatalogInputModality, CatalogModelWire, CatalogShellType,
    CodexReasoningEffortSet, CodexResponsesWireMode, ResolvedCatalog, WitnessedDescriptor,
    catalog_content_filter_guidance, catalog_enum_fingerprint, catalog_service_tiers,
    executable_codex_catalog_model, reference_client_envelope_context_contract,
};
pub use catalog::{
    CodexCatalogApiSupport, CodexCatalogCapacityFacts, CodexCatalogEntry, CodexCatalogServiceTier,
    CodexCatalogToolMode, CodexCatalogVisibility, CodexModelCatalog, RELEASE_ENROLLED_CODEX_MODELS,
    release_enrolled_codex_model, release_enrolled_codex_tool_mode,
};
pub use login::{CodexBrowserLogin, CodexDeviceAuthorization, CodexDeviceLogin};
#[cfg(test)]
use login::{
    DeviceCodeResponse, DeviceUserCodeResponse, parse_browser_callback, write_callback_response,
};
use refresh::RefreshForce;
use verify::{RsaJwk, validate_exact_scopes};
#[cfg(test)]
use verify::{rsa_components_are_acceptable, validate_jose_header};

pub(crate) use dispatch::{
    CodexDispatchGuard, CodexResponsesDialect, codex_credential_rejected,
    codex_responses_preflight, codex_side_request, revalidate_codex_responses,
};
#[cfg(test)]
use dispatch::{
    codex_compact_compat_headers, remote_compaction_v2_header, validate_codex_request_authority,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::{NonZeroU8, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
#[cfg(test)]
#[cfg(test)]
use plakous_auth::oauth_orchestrator::write_all_before_deadline;
use plakous_auth::oauth_orchestrator::{
    BrowserLoopbackHooks, BuildRequestFailure, ClaimedRefreshHooks, ClaimedRevokeHooks,
    CredentialRefreshDetail, CredentialRefreshFailureReason, CredentialRefreshObserver,
    FetchedJwks, JwksCache, JwksResolvePolicy, LoopbackConnectionEvent, PullBodyError,
    bind_loopback_listener, parse_cache_control_max_age, pull_claimed_body,
    run_browser_loopback_session, run_claimed_refresh, run_claimed_revoke,
};
#[cfg(test)]
use plakous_auth::oauth_workflow::format_http11_text_response;
use plakous_auth::oauth_workflow::{
    AuthorizeUrlRequest, CallbackQueryPolicy, DevicePollSchedule, LoopbackCallbackOutcome,
    LoopbackCallbackPage, OAuthCallbackOutcome, OAuthWorkflowError, access_needs_refresh,
    authorization_code_token_form, build_authorization_url, capped_deadline, decode_json_bounded,
    parse_http11_get_request, parse_loopback_callback_target, remaining_until,
    unsigned_access_token_exp, urlencoded_form_bytes,
};
use plakous_auth::subscription_limit::{
    SubscriptionLimitError, SubscriptionRequestClass, SubscriptionRequestGate,
    SubscriptionRequestPermit,
};
use plakous_auth::{
    AccountBinding, AuthSingleFlight, AuthSingleFlightGuard, ClaimedHttpDisposition,
    ClaimedHttpError, ClaimedHttpResponse, CredentialFence, CredentialGeneration, CredentialRecord,
    CredentialSlotId, CredentialStatus, OfflineCredentialLifecycle, PrivateCredentialStore,
    PrivateStoreError, ReadyCredentialLease, RefreshClaim, SecretBytes,
    SecretString as AuthSecretString, SubscriptionIntegrationProfileId as AuthProfileId,
    generate_login_selection_epoch, generate_oauth_correlation_token, valid_login_selection_epoch,
};
use plakous_exchange::network_failure;
use plakous_http::{
    CancellationToken, Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Limits, Method,
    Origin, Request, SensitiveBytes, StatusCode, sensitive_header_value,
};
use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule, Clock,
    ContextContract, ContextEvidenceSource, CountCertificateKind, Digest, DigestDomain,
    EvidenceFreshness, FactProvenance, FactSource, FeatureSupport, LanguageModel,
    MeasurementCertificate, ModelAdapterProfileId, ModelFamilyId, ModelId, NamespaceId, ProtocolId,
    ProviderConfigurationError, ProviderSemanticEvidence, ReasoningEffort, ResolvedModelDescriptor,
    RouteId, RuntimeModelAuthority, RuntimeModelAuthorityError, SubscriptionUsageCredentialState,
    SubscriptionUsageRead, SubscriptionUsageUnavailable, Support, SystemClock, Timestamp,
    now_unix_secs,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use subtle::ConstantTimeEq as _;
use thiserror::Error;
#[cfg(test)]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;
use zeroize::{Zeroize as _, Zeroizing};

use crate::oauth_pins::{CODEX_OAUTH, CODEX_PROFILE};
use crate::{
    AuthError, CodexSubscriptionRouteConfig, EndpointOrigin, NativeCodexCatalogWitness,
    NativeCodexCredentialAuthority, NativeCodexForcedRefresh, NativeCodexPreflight,
    NativeCodexPreflightRequest, NativeCodexRuntimeAuthority, OPENAI_CHATGPT_ORIGIN,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_SOURCE_REVISION,
    OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiLanguageModel, OpenAiRoute, OpenAiTarget,
    StandardOpenAiHttpConfig, StorageMode, config::OPENAI_CODEX_MODELS_PATH,
};
use plakous_responses_dialect::ResponsesPhysicalRequestBinding;
use plakous_responses_dialect::{
    CredentialGenerationFingerprint, ResponsesAccountSlot, ResponsesAuthSlot,
    ResponsesCredentialAuthority, ResponsesDispatchRejection, ResponsesHeaderBinding,
    ResponsesPreflightFuture, ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest,
    ResponsesRequestAuthority, ResponsesRevalidationRequest,
};

/// Exact `OpenAI` authentication origin pinned by the native profile.
pub(crate) const OPENAI_AUTH_ORIGIN: &str = CODEX_OAUTH.auth_origin;
/// Public/native client registration observed in the pinned official Codex source.
pub(crate) const OPENAI_CODEX_NATIVE_CLIENT_ID: &str = CODEX_OAUTH.client_id;
/// Exact scope request observed in the pinned official Codex browser flow.
pub(crate) const OPENAI_CODEX_OAUTH_SCOPES: &str = CODEX_OAUTH.scopes;
/// Compatibility notice that must be accepted without coercion.
pub(crate) const OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION: &str = CODEX_PROFILE.notice_revision;
/// Locally reviewable kill-switch revision bound into the profile.
pub(crate) const OPENAI_CODEX_KILL_SWITCH_REVISION: &str = CODEX_PROFILE.kill_switch_revision;

#[cfg(test)]
const AUTHORIZE_PATH: &str = CODEX_OAUTH.authorize_path;
const TOKEN_PATH: &str = CODEX_OAUTH.token_path;
const REVOKE_PATH: &str = CODEX_OAUTH.revoke_path;
const JWKS_PATH: &str = CODEX_OAUTH.jwks_path;
const USER_INFO_PATH: &str = CODEX_OAUTH.userinfo_path;
const DEVICE_USER_CODE_PATH: &str = CODEX_OAUTH.device_authorization_path;
const DEVICE_TOKEN_PATH: &str = CODEX_OAUTH.device_token_path.unwrap();
const DEVICE_VERIFICATION_PATH: &str = CODEX_OAUTH.device_verification_path.unwrap();
const DEVICE_REDIRECT_PATH: &str = CODEX_OAUTH.device_redirect_path.unwrap();
const BROWSER_CALLBACK_PATH: &str = CODEX_OAUTH.browser_callback_path;
const SUBSCRIPTION_INPUT_TOKEN_OVERHEAD: u64 = 512;
const CODEX_REFERENCE_CAPACITY_REVISION: &str = "codex-reference-input-envelope-v2";
const CODEX_REFERENCE_MEASUREMENT_REVISION: &str =
    "codex-complete-input-utf8-bytes-div4-compaction-v5";
const RESPONSES_FAMILY_ID: &str = "responses-family";
const RESPONSES_CANONICAL_PROJECTION_REVISION: &str = "responses-canonical-projection-v4";
const RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION: &str = "responses-exact-wire-accounting-v3";
const PROFILE_REVIEWED_AT: u64 = CODEX_PROFILE.reviewed_at_unix;
const PROFILE_EXPIRES_AT: u64 = CODEX_PROFILE.expires_at_unix;
const PROFILE_KILLED: bool = CODEX_PROFILE.killed;
const USER_AGENT: &str = concat!("plak/", env!("CARGO_PKG_VERSION"));
/// Exact native-option namespace for Codex OAuth remote compact.
const CODEX_NATIVE_COMPACTION_NAMESPACE: &str = "plakous.compaction";
const MAX_AUTH_BODY_BYTES: usize = 256 * 1024;
/// ChatGPT Codex catalog documents embed large instruction blobs; 256 KiB is
/// insufficient for a full admitted model list under current client_version pins.
const MAX_CATALOG_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_CALLBACK_BYTES: usize = 8 * 1024;
const MAX_OAUTH_VALUE_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 128 * 1024;
const MAX_JWKS_KEYS: usize = 16;
const MAX_CATALOG_MODELS: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;
const MAX_SERVICE_TIER_ID_BYTES: usize = 64;
const MAX_SERVICE_TIER_NAME_BYTES: usize = 128;
const MAX_SERVICE_TIER_DESCRIPTION_BYTES: usize = 512;
const MAX_DEVICE_INTERVAL_SECONDS: u64 = 30;
const MIN_DEVICE_INTERVAL_SECONDS: u64 = 1;
const DEVICE_DEADLINE: Duration = Duration::from_secs(15 * 60);
const BROWSER_DEADLINE: Duration = Duration::from_secs(10 * 60);
const AUTH_REQUEST_CAP: Duration = Duration::from_secs(30);
#[cfg(test)]
const CALLBACK_RESPONSE_CAP: Duration = Duration::from_secs(1);
const ACCESS_REFRESH_SKEW: u64 = 5 * 60;
/// Authenticated catalog evidence window, from its observation (SuperGrok parity).
const CATALOG_TTL_SECONDS: u64 = 6 * 60 * 60;
const DEFAULT_ACCESS_TOKEN_LIFETIME: u64 = 60 * 60;
const JWKS_MAX_AGE: Duration = Duration::from_secs(60 * 60);
const MAX_JWKS_NEGATIVE_KEYS: usize = 64;
const MAX_ID_TOKEN_LIFETIME: u64 = 24 * 60 * 60;

/// Non-coercible provenance for this integration profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SubscriptionIntegrationProvenance {
    /// Observed compatibility, not provider support or endorsement.
    PinnedCompatibility,
}

/// Credential-free immutable facts for the built-in native profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexSubscriptionIntegrationProfile;

impl CodexSubscriptionIntegrationProfile {
    /// Returns the immutable profile identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
    }

    /// Returns the exact official source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        OPENAI_CODEX_SOURCE_REVISION
    }

    /// Returns the exact compatibility notice revision an operator must accept
    /// before any online authentication or inference admission.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION
    }

    /// Returns compatibility provenance; this is not provider authorization.
    #[must_use]
    pub const fn provenance(self) -> SubscriptionIntegrationProvenance {
        SubscriptionIntegrationProvenance::PinnedCompatibility
    }

    /// Returns the review timestamp.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        PROFILE_REVIEWED_AT
    }

    /// Returns the hard compatibility expiry.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        PROFILE_EXPIRES_AT
    }

    /// Returns the local kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        OPENAI_CODEX_KILL_SWITCH_REVISION
    }

    /// Reports the compiled kill-switch state.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        PROFILE_KILLED
    }
}

/// Exact operator acceptance of the experimental compatibility notice.
pub struct CodexCompatibilityAcceptance {
    accepted_at: u64,
}

impl CodexCompatibilityAcceptance {
    /// Accepts only the exact immutable profile, source, and notice revisions.
    ///
    /// # Errors
    ///
    /// Rejects any drift instead of silently accepting a later profile.
    pub fn accept_exact(
        profile_id: &str,
        source_revision: &str,
        notice_revision: &str,
        accepted_at: Timestamp,
    ) -> Result<Self, NativeCodexAuthError> {
        if profile_id != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || source_revision != OPENAI_CODEX_SOURCE_REVISION
            || notice_revision != OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION
        {
            return Err(NativeCodexAuthError::ProfileNotAccepted);
        }
        Ok(Self {
            accepted_at: accepted_at.seconds(),
        })
    }
}

impl fmt::Debug for CodexCompatibilityAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexCompatibilityAcceptance(<exact notice accepted>)")
    }
}

/// Credential-free lifecycle for the selected Codex slot (online + offline admin).
pub type CodexCredentialStatus = OfflineCredentialLifecycle;

/// Redaction-safe native authentication failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum NativeCodexAuthError {
    /// The exact experimental notice was not accepted.
    #[error("native Codex compatibility profile was not accepted exactly")]
    ProfileNotAccepted,
    /// The pinned profile is expired, killed, or accepted from the future.
    #[error("native Codex compatibility profile is not currently admissible")]
    ProfileNotCurrent,
    /// Explicit private storage could not be opened or mutated safely.
    #[error("native Codex credential store failed")]
    Store,
    /// A peer owns the credential record's writer lock.
    #[error("native Codex credential store is busy")]
    StoreBusy,
    /// Secure randomness or a local login binding failed.
    #[error("native Codex login binding failed")]
    Binding,
    /// A bounded local callback was invalid or expired.
    #[error("native Codex browser callback was invalid")]
    Callback,
    /// The provider does not offer device-code login for this account or server.
    #[error("native Codex device-code login is not available for this account/server")]
    DeviceLoginUnavailable,
    /// The device code expired before the operator approved it.
    #[error("native Codex device code expired before it was approved")]
    DeviceCodeExpired,
    /// The login token request failed before a complete response was available.
    #[error("native Codex login token transport failed")]
    LoginTokenTransport,
    /// The login token endpoint rejected the authorization-code exchange.
    #[error("native Codex login token endpoint rejected the exchange")]
    LoginTokenRejected,
    /// The successful login token response was not provider-compatible.
    #[error("native Codex login token response was invalid")]
    LoginTokenResponseInvalid,
    /// Login signing keys could not be fetched or resolved safely.
    #[error("native Codex login signing keys failed validation")]
    LoginSigningKeys,
    /// Login identity, claims, or account verification failed.
    #[error("native Codex login account verification failed")]
    LoginAccountVerification,
    /// A validated login could not be committed safely.
    #[error("native Codex login persistence failed")]
    LoginPersistence,
    /// Provider transport failed without exposing provider data.
    #[error("native Codex authentication transport failed")]
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// Provider response status or framing was not accepted.
    #[error("native Codex authentication response was rejected")]
    Provider,
    /// OAuth/OIDC/JWKS data failed strict validation.
    #[error("native Codex authentication response failed validation")]
    Validation,
    /// Credential is absent, revoked, expired, or quarantined.
    #[error("native Codex credential is unavailable")]
    Unavailable,
    /// Credential account continuity no longer matches the admitted route.
    #[error("native Codex credential account changed")]
    AccountChanged,
    /// In-process authentication coordination failed.
    #[error("native Codex authentication coordination failed")]
    Coordination,
    /// Model catalog data was invalid or stale.
    #[error("native Codex model catalog failed validation")]
    Catalog,
    /// The stored login cannot be used by this build (for example it was
    /// made by an earlier build without a login selection epoch); log in again.
    #[error("native Codex login must be renewed with a new login")]
    LoginRequired,
    /// The selected login changed (a new login) under an admitted receipt.
    #[error("native Codex login selection changed")]
    SelectionChanged,
    /// Stored catalog credential payload could not be decoded.
    #[error("native Codex catalog credential payload failed validation")]
    CatalogCredential,
    /// Catalog authorization header could not be constructed safely.
    #[error("native Codex catalog authorization header failed validation")]
    CatalogAuthorizationHeader,
    /// Catalog account header could not be constructed safely.
    #[error("native Codex catalog account header failed validation")]
    CatalogAccountHeader,
    /// Pre-catalog credential refresh failed strict local validation.
    #[error("native Codex catalog refresh failed validation")]
    CatalogRefresh,
    /// Catalog response media type was not the pinned JSON form.
    #[error("native Codex catalog media type failed validation")]
    CatalogMediaType,
    /// Catalog JSON did not match the pinned document schema.
    #[error("native Codex catalog document failed validation")]
    CatalogDocument,
    /// One catalog model identifier was outside the pinned grammar.
    #[error("native Codex catalog model identifier failed validation")]
    CatalogIdentifier,
}

#[derive(Clone)]
struct ProfileEndpoints {
    auth_origin: EndpointOrigin,
    chatgpt_origin: EndpointOrigin,
    /// Browser callback listener candidates. Production binds exactly the
    /// pinned redirect ports; tests bind an ephemeral port (`[0]`).
    loopback_ports: &'static [u16],
}

impl ProfileEndpoints {
    fn production() -> Self {
        Self {
            auth_origin: EndpointOrigin::parse(OPENAI_AUTH_ORIGIN)
                .unwrap_or_else(|error| unreachable!("static auth origin is valid: {error}")),
            chatgpt_origin: EndpointOrigin::parse(OPENAI_CHATGPT_ORIGIN)
                .unwrap_or_else(|error| unreachable!("static ChatGPT origin is valid: {error}")),
            loopback_ports: CODEX_OAUTH.loopback_ports,
        }
    }
}

struct SelectedAccount {
    binding: Option<AccountBinding>,
}

/// Offline local administration for the exact built-in native Codex slot.
///
/// This facade owns no HTTP client, endpoint, acceptance, account identifier,
/// or credential material. It can therefore inspect credential-free state and
/// commit a local tombstone even after the online compatibility profile expires
/// or its kill switch is activated. The explicit path is accepted only at open
/// time and is never exposed again.
pub struct CodexSubscriptionLocalAdmin {
    store: Arc<PrivateCredentialStore>,
    lifecycle: AsyncMutex<()>,
}

impl CodexSubscriptionLocalAdmin {
    /// Opens the exact application-selected `primary` slot for the pinned
    /// native Codex profile without consulting ambient state or the network.
    ///
    /// Existing state is reloaded during open so a foreign profile, slot scope,
    /// malformed record, or unsafe path fails before the facade is returned.
    /// Current compatibility acceptance, expiry, and kill-switch state are
    /// deliberately irrelevant to this local cleanup authority.
    ///
    /// # Errors
    ///
    /// Rejects relative or unsafe paths and any foreign or malformed record.
    pub async fn open(slot_path: impl Into<PathBuf>) -> Result<Self, NativeCodexAuthError> {
        let slot_path = slot_path.into();
        let store = run_blocking(move || {
            let profile = AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
                .map_err(|_| NativeCodexAuthError::Validation)?;
            let slot =
                CredentialSlotId::new("primary").map_err(|_| NativeCodexAuthError::Validation)?;
            let store = Arc::new(
                PrivateCredentialStore::open_slot(&slot_path, profile, slot)
                    .map_err(|_| NativeCodexAuthError::Store)?,
            );
            let _validated = store.load().map_err(|_| NativeCodexAuthError::Store)?;
            Ok(store)
        })
        .await?;
        Ok(Self {
            store,
            lifecycle: AsyncMutex::new(()),
        })
    }

    /// Returns immutable, credential-free facts for the pinned integration.
    #[must_use]
    pub const fn profile(&self) -> CodexSubscriptionIntegrationProfile {
        CodexSubscriptionIntegrationProfile
    }

    /// Reloads and returns only the credential-free lifecycle classification.
    ///
    /// # Errors
    ///
    /// Fails closed if the explicit slot is replaced, malformed, foreign, or
    /// otherwise unsafe.
    pub async fn status(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let store = Arc::clone(&self.store);
        run_blocking(move || local_credential_status(&store)).await
    }

    /// Commits an offline local revoked tombstone without acceptance or network I/O.
    ///
    /// This never contacts the provider and does not claim remote revocation.
    /// It is safe to use for local cleanup after profile expiry or kill-switch
    /// activation.
    ///
    /// # Errors
    ///
    /// Fails closed if the explicit slot cannot be reloaded and changed by its
    /// exact generation under the pinned profile and slot scope.
    pub async fn logout_local(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let store = Arc::clone(&self.store);
        run_blocking(move || local_logout(&store)).await
    }
}

impl fmt::Debug for CodexSubscriptionLocalAdmin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "CodexSubscriptionLocalAdmin(<pinned profile; explicit slot; credential redacted>)",
        )
    }
}

/// Plak-owned credential manager for one explicit selected-account slot.
pub struct CodexSubscriptionCredentialManager {
    store: Arc<PrivateCredentialStore>,
    client: Client,
    endpoints: ProfileEndpoints,
    acceptance: CodexCompatibilityAcceptance,
    selected: Mutex<SelectedAccount>,
    lifecycle: AsyncMutex<()>,
    flights: AuthSingleFlight,
    jwks: JwksCache<RsaJwk>,
    authority_binding: Digest,
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    refresh_observer: Option<Arc<dyn CredentialRefreshObserver>>,
}

impl CodexSubscriptionCredentialManager {
    /// Opens one application-selected private slot. No ambient path, home,
    /// environment, external login, or process credential is consulted.
    ///
    /// # Errors
    ///
    /// Rejects a relative/unsafe path, profile drift, expiry, kill switch, or
    /// malformed existing record.
    pub fn new(
        slot_path: impl Into<PathBuf>,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
    ) -> Result<Self, NativeCodexAuthError> {
        Self::new_with_endpoints(
            &slot_path.into(),
            client,
            acceptance,
            ProfileEndpoints::production(),
        )
    }

    fn new_with_endpoints(
        slot_path: &Path,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
        endpoints: ProfileEndpoints,
    ) -> Result<Self, NativeCodexAuthError> {
        Self::new_with_endpoints_and_policy(
            slot_path,
            client,
            acceptance,
            endpoints,
            now_unix_secs(),
            PROFILE_KILLED,
        )
    }

    fn new_with_endpoints_and_policy(
        slot_path: &Path,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
        endpoints: ProfileEndpoints,
        now: u64,
        killed: bool,
    ) -> Result<Self, NativeCodexAuthError> {
        validate_profile_policy_at(acceptance.accepted_at, now, killed)?;
        let auth_profile = AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .map_err(|_| NativeCodexAuthError::Validation)?;
        let slot =
            CredentialSlotId::new("primary").map_err(|_| NativeCodexAuthError::Validation)?;
        let store = Arc::new(
            PrivateCredentialStore::open_slot(slot_path, auth_profile, slot)
                .map_err(|_| NativeCodexAuthError::Store)?,
        );
        let existing = store.load().map_err(|_| NativeCodexAuthError::Store)?;
        let selected = SelectedAccount {
            binding: existing
                .as_ref()
                .map(|fence| fence.account_binding().clone()),
        };
        let authority_binding = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-authority",
                OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
                slot_path.as_os_str().as_encoded_bytes(),
            ],
        )
        .map_err(|_| NativeCodexAuthError::Validation)?;
        Ok(Self {
            store,
            client,
            endpoints,
            acceptance,
            selected: Mutex::new(selected),
            lifecycle: AsyncMutex::new(()),
            flights: AuthSingleFlight::new(),
            jwks: JwksCache::new(JwksResolvePolicy::codex(), MAX_JWKS_NEGATIVE_KEYS),
            authority_binding,
            request_gate: None,
            refresh_observer: None,
        })
    }

    /// Binds browser-login callbacks on an ephemeral loopback port instead of
    /// the pinned redirect ports, so parallel tests never contend for them.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_ephemeral_loopback_for_tests(mut self) -> Self {
        self.endpoints.loopback_ports = &[0];
        self
    }

    /// Attaches the classified physical-request gate required for inference.
    ///
    /// Leaving the gate absent keeps maintenance and catalog workflows usable,
    /// but every physical subscription inference dispatch fails closed.
    #[must_use]
    pub fn with_subscription_request_gate(
        mut self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Self {
        self.request_gate = Some(gate);
        self
    }

    /// Attaches a content-free claimed-refresh observer (product telemetry).
    #[must_use]
    pub fn with_credential_refresh_observer(
        mut self,
        observer: Arc<dyn CredentialRefreshObserver>,
    ) -> Self {
        self.refresh_observer = Some(observer);
        self
    }

    /// Returns immutable credential-free profile facts.
    #[must_use]
    pub const fn profile(&self) -> CodexSubscriptionIntegrationProfile {
        CodexSubscriptionIntegrationProfile
    }

    /// Returns the exact explicit slot path selected by the application.
    #[must_use]
    pub fn slot_path(&self) -> &Path {
        self.store.path()
    }

    /// Returns credential-free lifecycle status.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe or malformed storage.
    pub fn status(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        local_credential_status(&self.store)
    }

    /// Stored access-token expiry (Unix seconds) of the Ready credential.
    ///
    /// # Errors
    ///
    /// Fails for a missing, non-Ready, or unreadable record.
    pub fn access_expires_at(&self) -> Result<u64, NativeCodexAuthError> {
        let fence = self
            .store
            .load()
            .map_err(|_| NativeCodexAuthError::Store)?
            .ok_or(NativeCodexAuthError::Unavailable)?;
        let lease = self
            .store
            .acquire_ready(&fence)
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        Ok(parse_stored(lease.payload())?.access_expires_at)
    }

    /// Performs exactly one read-only Codex subscription usage request.
    ///
    /// This control-plane observation does not acquire an inference permit,
    /// dispatch a model request, retry, redirect, or fall back to another auth
    /// route. The selected Ready generation remains leased through the complete
    /// bounded response and is rechecked before the snapshot is returned.
    ///
    /// It never starts a refresh: Codex refresh tokens rotate, so a claimed
    /// refresh dropped mid-exchange (the hosted usage read is bounded by a
    /// timeout) would leave a durable Unknown fence that only a new login
    /// clears. An expired access token is reported as `RefreshRequired`
    /// without network I/O; the next turn refreshes it.
    pub async fn subscription_usage(&self) -> SubscriptionUsageRead {
        let deadline = Instant::now() + AUTH_REQUEST_CAP;
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(now_unix_secs(), 0)
                .unwrap_or_else(|_| unreachable!("system epoch is a valid timestamp")),
            reason,
        };
        if self.validate_profile_admission().is_err() {
            return unavailable(SubscriptionUsageUnavailable::Unqualified);
        }
        // Every refusal before the HTTP exchange is local and is named as such:
        // a held lifecycle or store lease is `Busy`, a non-Ready record is its
        // exact `CredentialState`, and unsafe storage is `LocalState`. Only the
        // bounded exchange below may report `Transport`.
        let Ok(_lifecycle) = self.lifecycle.try_lock() else {
            return unavailable(SubscriptionUsageUnavailable::Busy);
        };
        let fence = match self.try_load_fence().await {
            Ok(fence) => fence,
            Err(NativeCodexAuthError::Unavailable) => {
                return unavailable(SubscriptionUsageUnavailable::NotConnected);
            }
            Err(NativeCodexAuthError::StoreBusy) => {
                return unavailable(SubscriptionUsageUnavailable::Busy);
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        if fence.status() == CredentialStatus::RefreshOutcomeUnknown {
            match self.refresh_outcome_settled(fence.clone()).await {
                Ok(true) => {}
                Ok(false) => return unavailable(SubscriptionUsageUnavailable::Busy),
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            }
        }
        if let Some(reason) = usage_credential_refusal(fence.status()) {
            return unavailable(reason);
        }
        let lease = match self.try_acquire_ready_lease(fence.clone()).await {
            Ok(Some(lease)) => lease,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        let credential = match parse_stored(lease.payload()) {
            Ok(credential) => credential,
            Err(_) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::LoginRequired,
                ));
            }
        };
        if credential.access_expires_at <= now_unix_secs() {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        if lease.fence() != fence
            || fence.account_binding().as_str() != credential.account_id
            || self.selected_account().as_ref() != Ok(fence.account_binding())
        {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("application/json"),
        );
        let mut bearer = Vec::with_capacity(7 + credential.access_token.len());
        bearer.extend_from_slice(b"Bearer ");
        bearer.extend_from_slice(credential.access_token.as_bytes());
        let Ok(authorization) = sensitive_header_value(bearer) else {
            return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
        };
        let Ok(account) = sensitive_header_value(credential.account_id.as_bytes().to_vec()) else {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        };
        headers.insert(HeaderName::from_static("authorization"), authorization);
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(USER_AGENT),
        );
        let reply = match execute_bounded(
            &self.client,
            &self.endpoints.chatgpt_origin,
            Method::GET,
            crate::subscription_usage::CODEX_SUBSCRIPTION_USAGE_PATH,
            headers,
            Vec::new(),
            deadline,
        )
        .await
        {
            Ok(reply) => reply,
            Err(NativeCodexAuthError::Transport(cause)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(cause));
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::Transport(None)),
        };
        let observed_at = Timestamp::new(now_unix_secs(), 0)
            .unwrap_or_else(|_| unreachable!("system epoch is a valid timestamp"));
        let reason = match reply.status.as_u16() {
            401 | 403 => Some(SubscriptionUsageUnavailable::Unauthorized),
            429 => Some(SubscriptionUsageUnavailable::RateLimited),
            200 => None,
            _ => Some(SubscriptionUsageUnavailable::ProviderUnavailable),
        };
        if let Some(reason) = reason {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason,
            };
        }
        if require_json_media_type(&reply).is_err() {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            };
        }
        // The lease retains the exact Ready record and its cross-process
        // shared lock through this observation. Reopening the credential file
        // here would recursively contend on that same inode; the lifecycle
        // guard and selected-account check cover this manager while the lease
        // fences replacement, refresh, logout, and revoke elsewhere.
        if lease.fence() != fence || self.selected_account().as_ref() != Ok(fence.account_binding())
        {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::AccountChanged,
            };
        }
        match crate::decode_codex_subscription_usage(&reply.body, observed_at) {
            Ok(snapshot) if snapshot.contains_ended_window() => {
                SubscriptionUsageRead::Unavailable {
                    observed_at,
                    reason: SubscriptionUsageUnavailable::Stale,
                }
            }
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(snapshot),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    pub(crate) fn validate_profile_admission(&self) -> Result<(), NativeCodexAuthError> {
        validate_profile(&self.acceptance, now_unix_secs())
    }

    pub(crate) fn validate_route_resolution(&self) -> Result<(), NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let _selected = self.selected_account()?;
        Ok(())
    }

    fn selected_account(&self) -> Result<AccountBinding, NativeCodexAuthError> {
        self.selected
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .binding
            .clone()
            .ok_or(NativeCodexAuthError::Unavailable)
    }

    fn install_selected_account(
        &self,
        account: AccountBinding,
    ) -> Result<(), NativeCodexAuthError> {
        self.selected
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .binding = Some(account);
        Ok(())
    }

    fn clear_selected_account(&self) -> Result<(), NativeCodexAuthError> {
        self.selected
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .binding = None;
        Ok(())
    }

    fn route_account_digest(&self) -> Digest {
        let selected = self.selected_account().ok();
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-account",
                OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
                selected
                    .as_ref()
                    .map_or(b"unselected".as_slice(), |value| value.as_str().as_bytes()),
            ],
        )
        .unwrap_or_else(|error| unreachable!("static digest version is valid: {error}"))
    }

    fn catalog_witness_binding(
        &self,
        descriptor: &ResolvedModelDescriptor,
        credential_fence: &CredentialFence,
        credential_selection_epoch: Digest,
        observed_at: Timestamp,
        expires_at: Timestamp,
        responses_lite: bool,
    ) -> Result<Digest, NativeCodexAuthError> {
        let descriptor_binding = descriptor
            .binding_digest()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let account_binding = self.route_account_digest();
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-catalog-witness",
                self.authority_binding.bytes(),
                account_binding.bytes(),
                OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
                credential_selection_epoch.bytes(),
                &credential_fence.generation().get().to_be_bytes(),
                &observed_at.seconds().to_be_bytes(),
                &observed_at.nanoseconds().to_be_bytes(),
                &expires_at.seconds().to_be_bytes(),
                &expires_at.nanoseconds().to_be_bytes(),
                descriptor.requested_model().as_str().as_bytes(),
                descriptor_binding.bytes(),
                &[u8::from(responses_lite)],
            ],
        )
        .map_err(|_| NativeCodexAuthError::Catalog)
    }

    fn issue_catalog_witness(
        &self,
        descriptor: &ResolvedModelDescriptor,
        credential_fence: &CredentialFence,
        selection_epoch: Digest,
        observed_at: Timestamp,
        expires_at: Timestamp,
        responses_lite: bool,
    ) -> Result<NativeCodexCatalogWitness, NativeCodexAuthError> {
        if credential_fence.status() != CredentialStatus::Ready
            || credential_fence.profile_id().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || credential_fence.account_binding() != &self.selected_account()?
        {
            return Err(NativeCodexAuthError::Catalog);
        }
        Ok(NativeCodexCatalogWitness::new(
            self.catalog_witness_binding(
                descriptor,
                credential_fence,
                selection_epoch,
                observed_at,
                expires_at,
                responses_lite,
            )?,
            credential_fence.clone(),
            selection_epoch,
            observed_at,
            expires_at,
            responses_lite,
        ))
    }
}

impl fmt::Debug for CodexSubscriptionCredentialManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexSubscriptionCredentialManager")
            .field("profile", &OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .field("slot", &"<explicit private path>")
            .field("credential", &"<redacted>")
            .finish()
    }
}

fn validate_profile(
    acceptance: &CodexCompatibilityAcceptance,
    now: u64,
) -> Result<(), NativeCodexAuthError> {
    validate_profile_at(acceptance.accepted_at, now)
}

fn validate_profile_at(accepted_at: u64, now: u64) -> Result<(), NativeCodexAuthError> {
    validate_profile_policy_at(accepted_at, now, PROFILE_KILLED)
}

fn validate_profile_policy_at(
    accepted_at: u64,
    now: u64,
    killed: bool,
) -> Result<(), NativeCodexAuthError> {
    if CODEX_PROFILE.is_acceptance_current_at(accepted_at, now, killed) {
        Ok(())
    } else {
        Err(NativeCodexAuthError::ProfileNotCurrent)
    }
}

fn map_oauth_workflow_error(error: OAuthWorkflowError) -> NativeCodexAuthError {
    match error {
        OAuthWorkflowError::Expired => NativeCodexAuthError::Callback,
        OAuthWorkflowError::Binding => NativeCodexAuthError::Binding,
        OAuthWorkflowError::Callback => NativeCodexAuthError::Callback,
        OAuthWorkflowError::ResponseTooLarge
        | OAuthWorkflowError::InvalidJson
        | OAuthWorkflowError::InvalidUrl
        | OAuthWorkflowError::InvalidForm => NativeCodexAuthError::Validation,
        _ => NativeCodexAuthError::Validation,
    }
}

/// Local lifecycle without network I/O. A Ready record whose payload this
/// build cannot parse (an earlier build's login) is `LoginRequired`; a record
/// whose lease is busy keeps its stored status.
fn local_credential_status(
    store: &PrivateCredentialStore,
) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
    let fence = store.load().map_err(|_| NativeCodexAuthError::Store)?;
    if let Some(fence) = fence
        .as_ref()
        .filter(|fence| fence.status() == CredentialStatus::Ready)
        && let Ok(Some(lease)) = store.try_acquire_ready(fence)
        && parse_stored(lease.payload()).is_err()
    {
        return Ok(OfflineCredentialLifecycle::LoginRequired);
    }
    Ok(OfflineCredentialLifecycle::classify(
        fence.as_ref().map(CredentialFence::status),
    ))
}

/// Names the exact local refusal for a record that is not Ready; `None` means
/// the record is Ready and the read may proceed to lease it. A lifecycle this
/// build does not know is refused as local state rather than guessed.
const fn usage_credential_refusal(
    status: CredentialStatus,
) -> Option<SubscriptionUsageUnavailable> {
    match status {
        CredentialStatus::Ready => None,
        CredentialStatus::RefreshRequired => Some(SubscriptionUsageUnavailable::CredentialState(
            SubscriptionUsageCredentialState::RefreshRequired,
        )),
        CredentialStatus::RefreshOutcomeUnknown => {
            Some(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined,
            ))
        }
        CredentialStatus::Revoked => Some(SubscriptionUsageUnavailable::CredentialState(
            SubscriptionUsageCredentialState::Revoked,
        )),
        _ => Some(SubscriptionUsageUnavailable::LocalState),
    }
}

fn local_logout(
    store: &PrivateCredentialStore,
) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
    store
        .logout_local()
        .map_err(|_| NativeCodexAuthError::Store)
}

async fn run_blocking<T, F>(operation: F) -> Result<T, NativeCodexAuthError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, NativeCodexAuthError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| NativeCodexAuthError::Coordination)?
}

fn remaining_login_time(
    deadline: Instant,
    expired: NativeCodexAuthError,
) -> Result<Duration, NativeCodexAuthError> {
    remaining_until(deadline, Instant::now()).ok_or(expired)
}

fn capped_login_deadline(
    deadline: Instant,
    cap: Duration,
    expired: NativeCodexAuthError,
) -> Result<Instant, NativeCodexAuthError> {
    capped_deadline(deadline, cap, Instant::now()).ok_or(expired)
}

/// Private one-use inference authorization retaining the exact Ready lease.
pub(crate) struct NativeCodexAuthorization {
    lease: ReadyCredentialLease,
    expected_fence: CredentialFence,
    expected_selection_epoch: Digest,
    credential: StoredCredential,
    preflight: NativeCodexPreflightRequest,
    physical_request: Option<ResponsesPhysicalRequestBinding>,
    headers_consumed: bool,
}

impl NativeCodexAuthorization {
    pub(crate) const fn target(&self) -> &OpenAiTarget {
        self.preflight.target()
    }

    pub(crate) fn credential_generation(&self) -> CredentialGenerationFingerprint {
        // The login selection is stable across refreshes; the generation
        // keeps one fingerprint per stored credential generation.
        let mut binding = self.expected_selection_epoch.bytes().to_vec();
        binding.extend_from_slice(&self.expected_fence.generation().get().to_be_bytes());
        match crate::config::mint_openai_generation(&binding) {
            Ok(fingerprint) => fingerprint,
            Err(_) => unreachable!("a digest is a valid credential-generation binding"),
        }
    }

    pub(crate) fn revalidate(
        &self,
        target: &OpenAiTarget,
        physical: &ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        if physical != self.preflight.physical()
            || self.lease.fence() != self.expected_fence
            || self.expected_fence.account_binding().as_str() != self.credential.account_id
        {
            return Err(AuthError::BindingMismatch);
        }
        validate_bound_credential_selection(
            target,
            &self.expected_fence,
            self.expected_selection_epoch,
        )
    }

    pub(crate) fn authorize_headers(
        &mut self,
        target: &OpenAiTarget,
        headers: &mut HeaderMap,
    ) -> Result<(), AuthError> {
        if self.headers_consumed {
            return Err(AuthError::AuthorizationAlreadyResolved);
        }
        let physical_request = self
            .physical_request
            .as_ref()
            .ok_or(AuthError::NoActivePhysicalRequest)?;
        if physical_request != self.preflight.physical() {
            return Err(AuthError::BindingMismatch);
        }
        validate_bound_credential_selection(
            target,
            &self.expected_fence,
            self.expected_selection_epoch,
        )?;
        validate_bound_credential_selection(
            self.preflight.target(),
            &self.expected_fence,
            self.expected_selection_epoch,
        )?;
        let fence = self.lease.fence();
        if fence != self.expected_fence
            || fence.account_binding().as_str() != self.credential.account_id
        {
            return Err(AuthError::BindingMismatch);
        }
        let mut bearer = Vec::with_capacity(7 + self.credential.access_token.len());
        bearer.extend_from_slice(b"Bearer ");
        bearer.extend_from_slice(self.credential.access_token.as_bytes());
        let authorization = sensitive_header_value(bearer).map_err(|_| AuthError::SourceFailure)?;
        let account = sensitive_header_value(self.credential.account_id.as_bytes().to_vec())
            .map_err(|_| AuthError::BindingMismatch)?;
        headers.insert(HeaderName::from_static("authorization"), authorization);
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        headers.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static("plak"),
        );
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(USER_AGENT),
        );
        self.headers_consumed = true;
        Ok(())
    }

    fn bind_physical(
        &mut self,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        if self.physical_request.is_some() || physical_request != *self.preflight.physical() {
            return Err(AuthError::BindingMismatch);
        }
        validate_bound_credential_selection(
            self.preflight.target(),
            &self.expected_fence,
            self.expected_selection_epoch,
        )?;
        self.physical_request = Some(physical_request);
        Ok(())
    }
}

fn validate_bound_credential_selection(
    target: &OpenAiTarget,
    expected_fence: &CredentialFence,
    expected_selection_epoch: Digest,
) -> Result<(), AuthError> {
    let Some((target_fence, target_selection_epoch)) = target.native_codex_credential_selection()
    else {
        return Err(AuthError::BindingMismatch);
    };
    if target_fence != expected_fence || target_selection_epoch != expected_selection_epoch {
        return Err(AuthError::BindingMismatch);
    }
    Ok(())
}

impl fmt::Debug for NativeCodexAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeCodexAuthorization(<redacted lease-bound grant>)")
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCredential {
    access_token: String,
    refresh_token: String,
    id_token: String,
    subject: String,
    account_id: String,
    scopes: Vec<String>,
    access_expires_at: u64,
    /// Random, non-secret login selection: minted by every successful login and
    /// carried unchanged by refresh, so a refreshed generation is told apart
    /// from a same-account relogin.
    selection_epoch: String,
}

impl Drop for StoredCredential {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
        self.subject.zeroize();
        self.account_id.zeroize();
        self.scopes.zeroize();
    }
}

/// Every payload this build cannot parse, whether a login stored by an
/// earlier build without a selection epoch or a corrupt record, is
/// `LoginRequired`: it needs a new login, and nothing is synthesized or
/// parsed in an older shape.
fn parse_stored(payload: &SecretBytes) -> Result<StoredCredential, NativeCodexAuthError> {
    if payload.is_empty() || payload.len() > MAX_AUTH_BODY_BYTES {
        return Err(NativeCodexAuthError::Validation);
    }
    let credential: StoredCredential = serde_json::from_slice(payload.expose_secret())
        .map_err(|_| NativeCodexAuthError::LoginRequired)?;
    if !valid_login_selection_epoch(&credential.selection_epoch) {
        return Err(NativeCodexAuthError::LoginRequired);
    }
    Ok(credential)
}

fn generate_selection_epoch() -> Result<String, NativeCodexAuthError> {
    generate_login_selection_epoch().map_err(|_| NativeCodexAuthError::Binding)
}

fn stored_record(
    credential: &StoredCredential,
    generation: CredentialGeneration,
) -> Result<CredentialRecord, NativeCodexAuthError> {
    let mut encoded =
        serde_json::to_vec(credential).map_err(|_| NativeCodexAuthError::Validation)?;
    let account = AccountBinding::new(&credential.account_id)
        .map_err(|_| NativeCodexAuthError::Validation)?;
    let record = CredentialRecord::new(
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .map_err(|_| NativeCodexAuthError::Validation)?,
        account,
        generation,
        CredentialStatus::Ready,
        SecretBytes::new(std::mem::take(&mut encoded)),
    )
    .map_err(|_| NativeCodexAuthError::Validation);
    encoded.zeroize();
    record
}

fn constant_time_value_eq(expected: &str, actual: &str) -> bool {
    // Fixed-window CT compare: always process a full MAX_OAUTH_VALUE_BYTES pad
    // and length digits so a length mismatch cannot short-circuit before the
    // byte work (matches plakous_auth::binding::constant_time_bounded_eq).
    let mut expected_padded = Zeroizing::new(vec![0_u8; MAX_OAUTH_VALUE_BYTES]);
    let mut actual_padded = Zeroizing::new(vec![0_u8; MAX_OAUTH_VALUE_BYTES]);
    let expected_bytes = expected.as_bytes();
    let actual_bytes = actual.as_bytes();
    let expected_bounded =
        !expected_bytes.is_empty() && expected_bytes.len() <= MAX_OAUTH_VALUE_BYTES;
    let actual_bounded = !actual_bytes.is_empty() && actual_bytes.len() <= MAX_OAUTH_VALUE_BYTES;
    if expected_bounded {
        expected_padded[..expected_bytes.len()].copy_from_slice(expected_bytes);
    }
    if actual_bounded {
        actual_padded[..actual_bytes.len()].copy_from_slice(actual_bytes);
    }
    let bytes_equal = expected_padded.ct_eq(actual_padded.as_slice());
    let lengths_equal = expected_bytes
        .len()
        .to_le_bytes()
        .ct_eq(&actual_bytes.len().to_le_bytes());
    expected_bounded && actual_bounded && bool::from(bytes_equal & lengths_equal)
}

fn validate_oauth_value(value: &str, maximum: usize) -> Result<(), NativeCodexAuthError> {
    if value.is_empty()
        || value.len() > maximum
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(NativeCodexAuthError::Validation);
    }
    Ok(())
}

fn validate_user_code(value: &str) -> Result<(), NativeCodexAuthError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(NativeCodexAuthError::Validation);
    }
    Ok(())
}

// Match the provider-owned Codex client's authorization-code response
// contract exactly. OpenAI Codex, Nanocodex/Tact, and OpenCode require these
// three values and deliberately ignore additive token-endpoint metadata.
#[derive(Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "the fields intentionally preserve the provider's exact OAuth wire names"
)]
struct OAuthLoginTokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
}

impl Drop for OAuthLoginTokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
    }
}

// The pinned official Codex client treats every refresh field as an optional
// replacement and preserves the previous value when it is omitted.
#[derive(Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "the fields intentionally preserve the provider's exact OAuth wire names"
)]
struct OAuthRefreshTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

impl Drop for OAuthRefreshTokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
    }
}

fn scheduled_access_expiry(
    access_token: &str,
    fallback: u64,
    now: u64,
) -> Result<u64, NativeCodexAuthError> {
    let expires_at = unsigned_access_token_exp(access_token, MAX_TOKEN_BYTES).unwrap_or(fallback);
    if access_needs_refresh(expires_at, now, ACCESS_REFRESH_SKEW) {
        return Err(NativeCodexAuthError::Validation);
    }
    Ok(expires_at)
}

fn login_token_response_or(login: bool, fallback: NativeCodexAuthError) -> NativeCodexAuthError {
    if login {
        NativeCodexAuthError::LoginTokenResponseInvalid
    } else {
        fallback
    }
}

fn login_signing_keys_or(login: bool, fallback: NativeCodexAuthError) -> NativeCodexAuthError {
    if login {
        NativeCodexAuthError::LoginSigningKeys
    } else {
        fallback
    }
}

fn login_account_verification_or(
    login: bool,
    fallback: NativeCodexAuthError,
) -> NativeCodexAuthError {
    if login {
        NativeCodexAuthError::LoginAccountVerification
    } else {
        fallback
    }
}

fn validate_identifier(value: &str, maximum: usize) -> Result<(), NativeCodexAuthError> {
    if value.is_empty()
        || value.len() > maximum
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(NativeCodexAuthError::Validation);
    }
    Ok(())
}

struct HttpReply {
    status: StatusCode,
    cache_control: Option<String>,
    content_type: Option<String>,
    body: Zeroizing<Vec<u8>>,
}

struct HttpAttemptError(Option<plakous_model::ModelNetworkFailure>);

async fn execute_bounded(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    deadline: Instant,
) -> Result<HttpReply, NativeCodexAuthError> {
    execute_classified(
        client,
        origin,
        method,
        path,
        headers,
        body,
        deadline,
        MAX_AUTH_BODY_BYTES,
    )
    .await
    .map_err(|error| NativeCodexAuthError::Transport(error.0))
}

async fn execute_catalog_bounded(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    deadline: Instant,
) -> Result<HttpReply, NativeCodexAuthError> {
    execute_classified(
        client,
        origin,
        method,
        path,
        headers,
        body,
        deadline,
        MAX_CATALOG_BODY_BYTES,
    )
    .await
    .map_err(|error| NativeCodexAuthError::Transport(error.0))
}

async fn execute_classified(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    deadline: Instant,
    max_body_bytes: usize,
) -> Result<HttpReply, HttpAttemptError> {
    // The exact Vec allocation remains the backing owner across transport
    // Bytes clones and is wiped when the final shared owner drops.
    let body = SensitiveBytes::new(body).into_bytes();
    let origin = Origin::parse(origin.as_str()).map_err(|_| HttpAttemptError(None))?;
    let request = Request::new(origin, method, path, headers, body, client.limits())
        .map_err(|_| HttpAttemptError(None))?;
    let cancellation = CancellationToken::new();
    let response = client
        .execute(
            request,
            ExchangeControl::with_cancellation(deadline, cancellation),
        )
        .await
        .map_err(|failure| HttpAttemptError(network_failure(failure)))?;
    let status = response.status();
    let cache_control = response
        .headers()
        .get(&HeaderName::from_static("cache-control"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 1024)
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get(&HeaderName::from_static("content-type"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128)
        .map(str::to_owned);
    let mut response_body = response.into_body();
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response_body.next_chunk().await {
        let chunk = chunk.map_err(|failure| HttpAttemptError(network_failure(failure)))?;
        if bytes.len().saturating_add(chunk.len()) > max_body_bytes {
            let _ = response_body.close().await;
            return Err(HttpAttemptError(None));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(HttpReply {
        status,
        cache_control,
        content_type,
        body: bytes,
    })
}

fn claimed_request(
    client: &Client,
    origin: &EndpointOrigin,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<Request, NativeCodexAuthError> {
    let origin = Origin::parse(origin.as_str()).map_err(|_| NativeCodexAuthError::Validation)?;
    Request::new(
        origin,
        method,
        path,
        headers,
        SensitiveBytes::new(body).into_bytes(),
        client.limits(),
    )
    .map_err(|_| NativeCodexAuthError::Validation)
}

async fn pull_claimed_body_cancellable(
    response: &mut ClaimedHttpResponse,
    cancellation: Option<&plakous_model::CancellationToken>,
) -> Result<Zeroizing<Vec<u8>>, NativeCodexAuthError> {
    let Some(cancellation) = cancellation else {
        return pull_claimed_body(response, MAX_AUTH_BODY_BYTES)
            .await
            .map_err(|error| match error {
                PullBodyError::Transport => NativeCodexAuthError::Transport(None),
                PullBodyError::TooLarge => NativeCodexAuthError::Validation,
                _ => NativeCodexAuthError::Transport(None),
            });
    };
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        let chunk = {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(NativeCodexAuthError::Transport(None));
                }
                chunk = response.next_chunk() => chunk,
            }
        };
        match chunk {
            None => {
                if !response.body_completed_cleanly() {
                    return Err(NativeCodexAuthError::Transport(None));
                }
                return Ok(bytes);
            }
            Some(Err(_)) => return Err(NativeCodexAuthError::Transport(None)),
            Some(Ok(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > MAX_AUTH_BODY_BYTES {
                    return Err(NativeCodexAuthError::Validation);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
    }
}

fn require_status(reply: &HttpReply, expected: StatusCode) -> Result<(), NativeCodexAuthError> {
    if reply.status == expected {
        Ok(())
    } else {
        Err(NativeCodexAuthError::Provider)
    }
}

fn require_json_media_type(reply: &HttpReply) -> Result<(), NativeCodexAuthError> {
    let is_json = reply.content_type.as_deref().is_some_and(|value| {
        value
            .split_once(';')
            .map_or(value, |(media_type, _)| media_type)
            .trim()
            .eq_ignore_ascii_case("application/json")
    });
    if is_json {
        Ok(())
    } else {
        Err(NativeCodexAuthError::Validation)
    }
}

fn parse_strict_json<T>(bytes: &[u8]) -> Result<T, NativeCodexAuthError>
where
    T: for<'de> Deserialize<'de>,
{
    parse_json_bounded(bytes, MAX_AUTH_BODY_BYTES)
}

fn parse_catalog_json<T>(bytes: &[u8]) -> Result<T, NativeCodexAuthError>
where
    T: for<'de> Deserialize<'de>,
{
    parse_json_bounded(bytes, MAX_CATALOG_BODY_BYTES)
}

fn parse_json_bounded<T>(bytes: &[u8], max_body_bytes: usize) -> Result<T, NativeCodexAuthError>
where
    T: for<'de> Deserialize<'de>,
{
    decode_json_bounded(bytes, max_body_bytes).map_err(map_oauth_workflow_error)
}

fn endpoint_url(origin: &EndpointOrigin, path: &str) -> Result<Url, NativeCodexAuthError> {
    let mut url = Url::parse(origin.as_str()).map_err(|_| NativeCodexAuthError::Validation)?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static("plak"),
    );
    headers.insert(
        HeaderName::from_static("user-agent"),
        HeaderValue::from_static(USER_AGENT),
    );
    headers
}

fn json_headers(bearer: Option<&str>) -> Result<HeaderMap, NativeCodexAuthError> {
    let mut headers = if let Some(token) = bearer {
        bearer_headers(token, false)?
    } else {
        base_headers()
    };
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

fn form_headers() -> HeaderMap {
    // Keep the authorization-code exchange wire-compatible with the
    // provider-owned Codex client. `originator` belongs to the authorization
    // URL, not to the token endpoint request.
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    headers
}

fn bearer_headers(token: &str, json: bool) -> Result<HeaderMap, NativeCodexAuthError> {
    validate_oauth_value(token, MAX_TOKEN_BYTES)?;
    let mut headers = base_headers();
    let mut bearer = Vec::with_capacity(7 + token.len());
    bearer.extend_from_slice(b"Bearer ");
    bearer.extend_from_slice(token.as_bytes());
    let value = sensitive_header_value(bearer).map_err(|_| NativeCodexAuthError::Validation)?;
    headers.insert(HeaderName::from_static("authorization"), value);
    if json {
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
    }
    Ok(headers)
}

fn form_body(values: &[(&str, &str)]) -> Vec<u8> {
    let mut encoded = urlencoded_form_bytes(values);
    std::mem::take(&mut *encoded)
}

impl NativeCodexCredentialAuthority for CodexSubscriptionCredentialManager {
    fn route_account_binding(&self) -> Digest {
        self.route_account_digest()
    }

    fn authority_binding(&self) -> Digest {
        self.authority_binding
    }

    fn validate_catalog_witness(
        &self,
        descriptor: &ResolvedModelDescriptor,
        witness: &NativeCodexCatalogWitness,
    ) -> Result<(), AuthError> {
        self.validate_profile_admission()
            .map_err(|_| AuthError::Unavailable)?;
        let selected = self
            .selected_account()
            .map_err(|_| AuthError::Unavailable)?;
        // The witness binds its own login selection; the stored selection is
        // compared wherever the payload is read (revalidation, preflight).
        let expected = self
            .catalog_witness_binding(
                descriptor,
                witness.credential_fence(),
                witness.credential_selection_epoch(),
                witness.observed_at(),
                witness.expires_at(),
                witness.responses_lite(),
            )
            .map_err(|_| AuthError::BindingMismatch)?;
        if witness.binding() != expected
            || descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || witness.credential_fence().status() != CredentialStatus::Ready
            || witness.credential_fence().account_binding() != &selected
        {
            return Err(AuthError::BindingMismatch);
        }
        Ok(())
    }

    fn revalidate_runtime_authority<'a>(
        &'a self,
        descriptor: &'a ResolvedModelDescriptor,
        witness: &'a NativeCodexCatalogWitness,
        checked_at: Timestamp,
    ) -> NativeCodexRuntimeAuthority<'a> {
        Box::pin(self.revalidate_model_runtime_authority(descriptor, witness, checked_at))
    }

    fn preflight(&self, request: NativeCodexPreflightRequest) -> NativeCodexPreflight<'_> {
        Box::pin(self.preflight_authorization(request))
    }

    fn refresh_after_rejection<'a>(
        &'a self,
        rejected: &'a CredentialFence,
        selection_epoch: Digest,
    ) -> NativeCodexForcedRefresh<'a> {
        Box::pin(async move {
            self.refresh_rejected_selection(rejected, selection_epoch)
                .await
                .map_err(|_| AuthError::SourceFailure)
        })
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, AuthError> {
        self.request_gate
            .as_ref()
            .ok_or(AuthError::Unavailable)?
            .acquire_inference(input_token_upper_bound)
            .map_err(|error| match error {
                SubscriptionLimitError::InferenceInputLimitExceeded => {
                    AuthError::InferenceInputLimit
                }
                SubscriptionLimitError::ClassLimitExceeded {
                    reserved,
                    allowance,
                    ..
                } => AuthError::InferenceClassLimit {
                    reserved,
                    allowance,
                },
                SubscriptionLimitError::InferenceScopeUnavailable => {
                    AuthError::InferenceClassLimit {
                        reserved: 0,
                        allowance: 0,
                    }
                }
                _ => AuthError::Unavailable,
            })
    }

    fn bind(
        &self,
        authorization: &mut NativeCodexAuthorization,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        self.validate_profile_admission()
            .map_err(|_| AuthError::Unavailable)?;
        let target = authorization.target();
        let fence = authorization.lease.fence();
        let expected_epoch = credential_selection_epoch(
            target.account_binding(),
            target.credential_authority_binding(),
            &authorization.credential.selection_epoch,
        )
        .map_err(|_| AuthError::BindingMismatch)?;
        if target.route_id().as_str() != crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID
            || target.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || target.account_binding() != self.route_account_digest()
            || target.credential_authority_binding() != self.authority_binding
            || fence != authorization.expected_fence
            || expected_epoch != authorization.expected_selection_epoch
            || fence.account_binding().as_str() != authorization.credential.account_id
            || fence.account_binding()
                != &self
                    .selected_account()
                    .map_err(|_| AuthError::Unavailable)?
        {
            return Err(AuthError::BindingMismatch);
        }
        authorization.bind_physical(physical_request)
    }
}
/// Credential-free exact evidence for one native Codex model resolution.
///
/// Identifying account and storage-path inputs are represented only by
/// domain-separated digests. The value contains no credential material and
/// serializes no raw account label, origin, or resource path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexModelResolutionEvidence {
    evidence_revision: u32,
    account_fingerprint: Digest,
    credential_authority_binding: Digest,
    selected_credential_generation: u64,
    credential_selection_epoch: Digest,
    route_fingerprint: Digest,
    descriptor: ResolvedModelDescriptor,
    descriptor_digest: Digest,
    catalog_receipt: Digest,
    capacity_facts: CodexCatalogCapacityFacts,
    #[serde(rename = "supported_reasoning_levels")]
    reasoning_efforts: CodexReasoningEffortSet,
    tool_mode: Option<CodexCatalogToolMode>,
    responses_lite: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_filter_guidance: Option<String>,
    catalog_enum_fingerprint: Digest,
    adapter_revision: &'static str,
    catalog_observed_at: Timestamp,
    catalog_expires_at: Timestamp,
    profile_revision: &'static str,
}

impl CodexModelResolutionEvidence {
    /// Returns the bounded, nonblank model-message override, when advertised.
    #[must_use]
    pub fn content_filter_guidance(&self) -> Option<&str> {
        self.content_filter_guidance.as_deref()
    }

    /// Returns the serialized exact-resolution evidence revision.
    #[must_use]
    pub const fn evidence_revision(&self) -> u32 {
        self.evidence_revision
    }

    /// Returns the route-scoped fingerprint of the exact selected account.
    #[must_use]
    pub const fn account_fingerprint(&self) -> Digest {
        self.account_fingerprint
    }

    /// Returns the exact explicit credential-store authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the selected credential generation authenticated by discovery.
    #[must_use]
    pub const fn selected_credential_generation(&self) -> u64 {
        self.selected_credential_generation
    }

    /// Returns a deterministic epoch for this authority, account, and generation.
    #[must_use]
    pub const fn credential_selection_epoch(&self) -> Digest {
        self.credential_selection_epoch
    }

    /// Returns the exact route/account/model/profile/capacity binding.
    #[must_use]
    pub const fn route_fingerprint(&self) -> Digest {
        self.route_fingerprint
    }

    /// Returns the immutable neutral resolved-descriptor digest.
    #[must_use]
    pub const fn descriptor_digest(&self) -> Digest {
        self.descriptor_digest
    }

    /// Returns the exact immutable descriptor resolved from the authenticated catalog.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Returns the descriptor after rechecking its retained binding digest.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the descriptor cannot be digested or no
    /// longer matches the digest captured atomically during resolution.
    pub fn validated_descriptor(
        &self,
    ) -> Result<&ResolvedModelDescriptor, ProviderConfigurationError> {
        if self.evidence_revision != 5
            || self
                .descriptor
                .binding_digest()
                .map_err(|_| ProviderConfigurationError::Identity)?
                != self.descriptor_digest
        {
            return Err(ProviderConfigurationError::Identity);
        }
        Ok(&self.descriptor)
    }

    /// Returns the exact authenticated catalog/reference receipt enrolled by
    /// the descriptor's reference-client-envelope input envelope.
    #[must_use]
    pub const fn catalog_receipt(&self) -> Digest {
        self.catalog_receipt
    }

    /// Returns the exact authenticated catalog capacity facts in this receipt.
    ///
    /// The receipt binds these facts to the selected account, credential
    /// authority, model, route, observation interval, and adapter revision.
    #[must_use]
    pub const fn capacity_facts(&self) -> CodexCatalogCapacityFacts {
        self.capacity_facts
    }

    /// Reports whether the resolved catalog row advertised the named effort.
    ///
    /// The result is reconstructed from credential-free resolution evidence;
    /// token budgets and unknown future effort labels fail closed.
    #[must_use]
    pub const fn supports_reasoning_effort(&self, effort: ReasoningEffort) -> bool {
        self.reasoning_efforts.supports(effort)
    }

    /// Returns the exact recognized authenticated catalog `tool_mode`.
    ///
    /// Omission resolves to Direct under the pinned 0.162.1 contract. `None`
    /// is deliberately non-qualifying and covers unknown future values.
    #[must_use]
    pub const fn tool_mode(&self) -> Option<CodexCatalogToolMode> {
        self.tool_mode
    }

    /// Returns the authenticated Codex Responses Lite request disposition.
    #[must_use]
    pub const fn uses_responses_lite(&self) -> bool {
        self.responses_lite
    }

    /// Reports whether this exact resolved row advertised Code Mode.
    #[must_use]
    pub const fn supports_code_mode(&self) -> bool {
        matches!(
            self.tool_mode,
            Some(CodexCatalogToolMode::CodeMode | CodexCatalogToolMode::CodeModeOnly)
        )
    }

    /// Returns the provider-owned semantic evidence projection used by
    /// effective configuration identity.
    ///
    /// Credential generation, selection epoch, receipt nonce, observation,
    /// and expiry remain fresh execution authority and are intentionally not
    /// present in this projection.
    pub fn semantic_provider_evidence(
        &self,
    ) -> Result<ProviderSemanticEvidence, ProviderConfigurationError> {
        #[derive(Serialize)]
        struct SemanticCodexEvidence<'a> {
            schema: &'a str,
            evidence_revision: u32,
            adapter_revision: &'a str,
            profile_revision: &'a str,
            reference_client_version: &'a str,
            catalog_source_revision: &'a str,
            capacity_facts: CodexCatalogCapacityFacts,
            reasoning_efforts: u8,
            tool_mode: Option<CodexCatalogToolMode>,
            responses_lite: bool,
            catalog_enum_fingerprint: Digest,
        }
        let bytes = serde_json::to_vec(&SemanticCodexEvidence {
            schema: "plakous-openai/codex-semantic-evidence-v6",
            evidence_revision: self.evidence_revision,
            adapter_revision: self.adapter_revision,
            profile_revision: self.profile_revision,
            reference_client_version: crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION,
            catalog_source_revision: crate::config::OPENAI_CODEX_CATALOG_SOURCE_REVISION,
            capacity_facts: self.capacity_facts,
            reasoning_efforts: self.reasoning_efforts.identity_byte(),
            tool_mode: self.tool_mode,
            responses_lite: self.responses_lite,
            catalog_enum_fingerprint: self.catalog_enum_fingerprint,
        })
        .map_err(|_| ProviderConfigurationError::Identity)?;
        ProviderSemanticEvidence::new(6, bytes)
    }

    /// Returns the protocol adapter revision that will prepare requests.
    #[must_use]
    pub const fn adapter_revision(&self) -> &'static str {
        self.adapter_revision
    }

    /// Returns when the exact authenticated catalog was observed.
    #[must_use]
    pub const fn catalog_observed_at(&self) -> Timestamp {
        self.catalog_observed_at
    }

    /// Returns the hard expiry of the exact authenticated catalog evidence.
    #[must_use]
    pub const fn catalog_expires_at(&self) -> Timestamp {
        self.catalog_expires_at
    }

    /// Returns the exact revisioned native compatibility-profile identity.
    #[must_use]
    pub const fn profile_revision(&self) -> &'static str {
        self.profile_revision
    }

    /// Returns the exact descriptor profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        self.profile_revision
    }
}

fn storage_binding(storage: StorageMode) -> &'static [u8] {
    match storage {
        StorageMode::ProviderStored => b"provider-stored",
        StorageMode::Stateless => b"stateless",
    }
}

/// One-way digest of the stored login selection epoch. A refreshed generation
/// keeps it; any new login, even of the same account, changes it.
fn credential_selection_epoch(
    account_fingerprint: Digest,
    credential_authority_binding: Digest,
    login_selection_epoch: &str,
) -> Result<Digest, NativeCodexAuthError> {
    let account = account_fingerprint.identity_bytes();
    let authority = credential_authority_binding.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        2,
        &[
            b"plak-native-codex-credential-selection-epoch-v2",
            OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
            &account,
            &authority,
            login_selection_epoch.as_bytes(),
        ],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

fn resolution_route_fingerprint(
    route: &OpenAiRoute,
    descriptor: &ResolvedModelDescriptor,
    account_fingerprint: Digest,
    credential_authority_binding: Digest,
    capacity_facts: CodexCatalogCapacityFacts,
    reasoning_efforts: CodexReasoningEffortSet,
    catalog_enum_fingerprint: Digest,
    responses_lite: bool,
) -> Result<Digest, NativeCodexAuthError> {
    let account = account_fingerprint.identity_bytes();
    let authority = credential_authority_binding.identity_bytes();
    let has_context_window = [u8::from(capacity_facts.context_window().is_some())];
    let context_window = capacity_facts
        .context_window()
        .map_or(0, NonZeroU32::get)
        .to_be_bytes();
    let has_max_context_window = [u8::from(capacity_facts.max_context_window().is_some())];
    let max_context_window = capacity_facts
        .max_context_window()
        .map_or(0, NonZeroU32::get)
        .to_be_bytes();
    let effective_context_window_percent =
        [capacity_facts.effective_context_window_percent().get()];
    let has_auto_compact_token_limit = [u8::from(
        capacity_facts.auto_compact_token_limit().is_some(),
    )];
    let auto_compact_token_limit = capacity_facts
        .auto_compact_token_limit()
        .map_or(0, NonZeroU32::get)
        .to_be_bytes();
    let reasoning_efforts = [reasoning_efforts.identity_byte()];
    let catalog_enum_fingerprint = catalog_enum_fingerprint.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        4,
        &[
            b"plak-native-codex-resolution-route-v4",
            route.id().as_str().as_bytes(),
            route.origin().as_str().as_bytes(),
            route.responses_path().as_bytes(),
            route.dialect().protocol().as_str().as_bytes(),
            route.dialect().revision().as_bytes(),
            storage_binding(route.storage()),
            &account,
            &authority,
            descriptor.requested_model().as_str().as_bytes(),
            descriptor.profile().as_str().as_bytes(),
            crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION.as_bytes(),
            crate::config::OPENAI_CODEX_CATALOG_SOURCE_REVISION.as_bytes(),
            &has_context_window,
            &context_window,
            &has_max_context_window,
            &max_context_window,
            &effective_context_window_percent,
            &has_auto_compact_token_limit,
            &auto_compact_token_limit,
            &reasoning_efforts,
            &catalog_enum_fingerprint,
            &[u8::from(responses_lite)],
        ],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

/// The exact login selection a receipt renewal may refresh and fetch under.
#[derive(Clone, Copy)]
struct ExpectedSelection<'a> {
    account: &'a AccountBinding,
    selection_epoch: Digest,
}

impl ExpectedSelection<'_> {
    fn check(
        &self,
        account: &AccountBinding,
        selection_epoch: Digest,
    ) -> Result<(), NativeCodexAuthError> {
        if account != self.account {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if selection_epoch != self.selection_epoch {
            return Err(NativeCodexAuthError::SelectionChanged);
        }
        Ok(())
    }
}

/// A catalog fetch that renews one receipt: bound to its login selection, and
/// stamped no later than the earliest check it answers (see [`CodexRenewalFloor`]).
struct CatalogRenewal<'a> {
    selection: ExpectedSelection<'a>,
    floor: &'a CodexRenewalFloor,
}

impl CatalogRenewal<'_> {
    fn check(
        &self,
        account: &AccountBinding,
        selection_epoch: Digest,
    ) -> Result<(), NativeCodexAuthError> {
        self.selection.check(account, selection_epoch)
    }

    /// Callers read their check time before the refresh and catalog round
    /// trip, so the renewed receipt claims observation at the request start or
    /// the earliest waiting check, whichever is earlier, in whole seconds.
    fn observed_at(&self, requested_at: Timestamp) -> Result<Timestamp, NativeCodexAuthError> {
        let earliest = self
            .floor
            .earliest()
            .map_or(requested_at, |floor| floor.min(requested_at));
        Timestamp::new(earliest.seconds(), 0).map_err(|_| NativeCodexAuthError::Catalog)
    }
}

/// The earliest `checked_at` among the revalidations one renewal answers.
#[derive(Debug, Default)]
pub(crate) struct CodexRenewalFloor(Mutex<Option<Timestamp>>);

impl CodexRenewalFloor {
    pub(crate) fn note(&self, checked_at: Timestamp) {
        if let Ok(mut floor) = self.0.lock() {
            *floor = Some(floor.map_or(checked_at, |earliest| earliest.min(checked_at)));
        }
    }

    fn earliest(&self) -> Option<Timestamp> {
        self.0.lock().ok().and_then(|floor| *floor)
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut floor) = self.0.lock() {
            *floor = None;
        }
    }
}

/// The behavior-bearing facts of one admitted catalog row. A renewed receipt
/// must carry the same row, or it installs nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CodexRowSemantics {
    descriptor: Digest,
    capacity_facts: CodexCatalogCapacityFacts,
    reasoning_efforts: CodexReasoningEffortSet,
    tool_mode: Option<CodexCatalogToolMode>,
    responses_lite: bool,
    content_filter_guidance: Option<String>,
    catalog_enum_fingerprint: Digest,
    catalog_receipt: Digest,
}

impl CodexRowSemantics {
    fn of(issued: &WitnessedDescriptor) -> Result<Self, NativeCodexAuthError> {
        Ok(Self {
            descriptor: issued
                .descriptor
                .semantic_binding_digest()
                .map_err(|_| NativeCodexAuthError::Catalog)?,
            capacity_facts: issued.capacity_facts,
            reasoning_efforts: issued.reasoning_efforts,
            tool_mode: issued.tool_mode,
            responses_lite: issued.responses_lite,
            content_filter_guidance: issued.content_filter_guidance.clone(),
            catalog_enum_fingerprint: issued.catalog_enum_fingerprint,
            catalog_receipt: issued.catalog_receipt,
        })
    }
}

/// The exact catalog inputs a resolver and its models share, so a running
/// model renews its receipt through the same resolution that admitted it.
#[derive(Clone)]
pub(crate) struct CodexCatalogSource {
    manager: Arc<CodexSubscriptionCredentialManager>,
    route: OpenAiRoute,
    client: Client,
    clock: Arc<dyn Clock>,
}

/// What a running Codex model needs to renew its receipt: the shared catalog
/// source and the exact admitted row.
#[derive(Clone)]
pub(crate) struct CodexReceiptRenewal {
    source: CodexCatalogSource,
    model_id: String,
    semantics: CodexRowSemantics,
}

impl CodexReceiptRenewal {
    /// Builds a new receipt for `stale`'s admitted descriptor: one claimed
    /// refresh when due and one authenticated catalog fetch, both bound to the
    /// stale receipt's account and login selection; installs nothing unless
    /// route identity and the admitted row are unchanged and the generation
    /// did not decrease. The new receipt never outlives `admitted_until`.
    pub(crate) async fn renew(
        &self,
        stale: &OpenAiTarget,
        admitted_until: Timestamp,
        floor: &CodexRenewalFloor,
    ) -> Result<OpenAiTarget, RuntimeModelAuthorityError> {
        let source = &self.source;
        let (expected_fence, selection_epoch) = stale
            .native_codex_credential_selection()
            .map(|(fence, epoch)| (fence.clone(), epoch))
            .ok_or(RuntimeModelAuthorityError::Invalid)?;
        if stale.validate_route(&source.route).is_err()
            || stale.account_binding() != source.manager.route_account_digest()
            || stale.credential_authority_binding() != source.manager.authority_binding
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        refresh_detached(
            Arc::clone(&source.manager),
            expected_fence.account_binding().clone(),
            selection_epoch,
        )
        .await
        .map_err(renewal_error)?;
        let renewal = CatalogRenewal {
            selection: ExpectedSelection {
                account: expected_fence.account_binding(),
                selection_epoch,
            },
            floor,
        };
        let resolved = source
            .fetch_catalog(Some(&renewal))
            .await
            .map_err(renewal_error)?;
        validate_catalog_fence(&source.manager, &resolved.credential_fence)
            .map_err(renewal_error)?;
        if resolved.credential_fence.account_binding() != expected_fence.account_binding()
            || resolved.credential_fence.generation() < expected_fence.generation()
            || resolved.ambiguous_slugs.contains(&self.model_id)
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        let issued = resolved
            .descriptors
            .get(&self.model_id)
            .ok_or(RuntimeModelAuthorityError::ForeignAuthority)?;
        if issued.witness.credential_selection_epoch() != selection_epoch
            || CodexRowSemantics::of(issued).map_err(|_| RuntimeModelAuthorityError::Invalid)?
                != self.semantics
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        let witness = source
            .manager
            .issue_catalog_witness(
                stale.descriptor(),
                &resolved.credential_fence,
                selection_epoch,
                issued.observed_at,
                issued.expires_at.min(admitted_until),
                issued.responses_lite,
            )
            .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
        OpenAiTarget::new_native_catalog_target(
            source.route.clone(),
            stale.descriptor().clone(),
            witness,
            Arc::clone(&source.clock),
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }
}

/// Runs a renewal's claimed refresh (when due) as its own task, bound to one
/// login selection. Revalidation callers may be dropped (a compaction timeout,
/// shutdown, an aborted driver); the claimed exchange still completes or fails
/// within its 30 s refresh deadline and records its outcome, so dropping a
/// waiting caller never strands a started claim as an ambiguous quarantine.
async fn refresh_detached(
    manager: Arc<CodexSubscriptionCredentialManager>,
    account: AccountBinding,
    selection_epoch: Digest,
) -> Result<(), NativeCodexAuthError> {
    tokio::spawn(async move {
        let _lifecycle = manager.lifecycle.lock().await;
        manager
            .refresh_locked_with_force(
                Instant::now() + Duration::from_secs(30),
                None,
                RefreshForce::IfDue,
                Some(ExpectedSelection {
                    account: &account,
                    selection_epoch,
                }),
            )
            .await
    })
    .await
    .map_err(|_| NativeCodexAuthError::Coordination)?
}

fn renewal_error(error: NativeCodexAuthError) -> RuntimeModelAuthorityError {
    match error {
        NativeCodexAuthError::AccountChanged
        | NativeCodexAuthError::SelectionChanged
        | NativeCodexAuthError::LoginRequired => RuntimeModelAuthorityError::ForeignAuthority,
        NativeCodexAuthError::Unavailable => RuntimeModelAuthorityError::Unavailable,
        _ => RuntimeModelAuthorityError::Stale,
    }
}

fn validate_catalog_fence(
    manager: &CodexSubscriptionCredentialManager,
    expected: &CredentialFence,
) -> Result<(), NativeCodexAuthError> {
    let current = manager
        .store
        .load()
        .map_err(|_| NativeCodexAuthError::Store)?
        .ok_or(NativeCodexAuthError::Unavailable)?;
    if current.status() != CredentialStatus::Ready {
        return Err(NativeCodexAuthError::Unavailable);
    }
    if current.account_binding() != &manager.selected_account()? {
        return Err(NativeCodexAuthError::AccountChanged);
    }
    if current != *expected {
        return Err(NativeCodexAuthError::Catalog);
    }
    Ok(())
}

/// Provider-local catalog and model resolver for the native Codex route.
pub struct CodexModelResolver {
    manager: Arc<CodexSubscriptionCredentialManager>,
    route: OpenAiRoute,
    client: Client,
    inference_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    config: StandardOpenAiHttpConfig,
    catalog: Mutex<Option<ResolvedCatalog>>,
    clock: Arc<dyn Clock>,
}

impl CodexModelResolver {
    fn validate_route_account(&self) -> Result<(), NativeCodexAuthError> {
        let route_account = self
            .route
            .account_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        if route_account != self.manager.route_account_digest() {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        Ok(())
    }

    fn validate_catalog_fence(
        &self,
        expected: &CredentialFence,
    ) -> Result<(), NativeCodexAuthError> {
        validate_catalog_fence(&self.manager, expected)
    }

    /// The exact catalog inputs this resolver's models renew their receipts with.
    fn catalog_source(&self) -> CodexCatalogSource {
        CodexCatalogSource {
            manager: Arc::clone(&self.manager),
            route: self.route.clone(),
            client: self.client.clone(),
            clock: Arc::clone(&self.clock),
        }
    }

    async fn fetch_catalog(&self) -> Result<ResolvedCatalog, NativeCodexAuthError> {
        self.catalog_source().fetch_catalog(None).await
    }

    /// Binds a ready native manager to the exact `ChatGPT` route without network I/O.
    ///
    /// # Errors
    ///
    /// Rejects a missing selected account, stale profile acceptance, or an
    /// invalid exact native route. Credential readiness is checked before each
    /// catalog request and inference preflight.
    pub fn new(
        manager: Arc<CodexSubscriptionCredentialManager>,
        client: Client,
        config: StandardOpenAiHttpConfig,
    ) -> Result<Self, NativeCodexAuthError> {
        let origin = EndpointOrigin::parse(OPENAI_CHATGPT_ORIGIN)
            .map_err(|_| NativeCodexAuthError::Validation)?;
        let route = CodexSubscriptionRouteConfig::new(origin)
            .resolve_native(Arc::clone(&manager))
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        Ok(Self {
            manager,
            route,
            client,
            inference_observer: None,
            config,
            catalog: Mutex::new(None),
            clock: Arc::new(SystemClock),
        })
    }

    /// Observes only inference exchanges, never catalog or credential traffic.
    /// `None` leaves the caller-supplied client configuration unchanged.
    /// Transport identity and dispatch policy remain unchanged.
    #[must_use]
    pub fn with_inference_observer(
        mut self,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Self {
        self.inference_observer = observer;
        self
    }

    /// Replaces the clock used for catalog provenance and resolved-target expiry.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    #[cfg(test)]
    fn new_for_test(
        manager: Arc<CodexSubscriptionCredentialManager>,
        client: Client,
        config: StandardOpenAiHttpConfig,
    ) -> Result<Self, NativeCodexAuthError> {
        let route = crate::config::native_codex_transport_conformance_route(
            manager.endpoints.chatgpt_origin.clone(),
            Arc::clone(&manager),
        )
        .map_err(|_| NativeCodexAuthError::Unavailable)?;
        Ok(Self {
            manager,
            route,
            client,
            inference_observer: None,
            config,
            catalog: Mutex::new(None),
            clock: Arc::new(SystemClock),
        })
    }

    /// Refreshes credentials before one authenticated catalog request and
    /// atomically replaces the credential-free resolver snapshot.
    ///
    /// # Errors
    ///
    /// Rejects unavailable credentials, stale profile acceptance, non-JSON or
    /// malformed catalog data, and exact store/account races.
    pub async fn refresh_catalog(&self) -> Result<CodexModelCatalog, NativeCodexAuthError> {
        if let Err(error) = self.validate_route_account() {
            *self
                .catalog
                .lock()
                .map_err(|_| NativeCodexAuthError::Coordination)? = None;
            return Err(error);
        }
        let resolved = self.fetch_catalog().await?;
        self.manager.validate_profile_admission()?;
        if let Err(error) = self.validate_route_account() {
            *self
                .catalog
                .lock()
                .map_err(|_| NativeCodexAuthError::Coordination)? = None;
            return Err(error);
        }
        self.validate_catalog_fence(&resolved.credential_fence)?;
        let public = resolved.public.clone();
        *self
            .catalog
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)? = Some(resolved);
        Ok(public)
    }

    /// Resolves a previously discovered exact slug without network access.
    ///
    /// # Errors
    ///
    /// Rejects missing, foreign, or expired evidence.
    pub fn resolve(&self, model_id: &str) -> Result<Arc<dyn LanguageModel>, NativeCodexAuthError> {
        self.resolve_with_evidence(model_id).map(|(model, _)| model)
    }

    /// Atomically resolves a model and its credential-free exact-resolution evidence.
    ///
    /// The evidence and returned model are built from the same catalog
    /// descriptor, route, authenticated credential fence, and adapter revision.
    /// No network request is made.
    ///
    /// # Errors
    ///
    /// Rejects missing or expired catalog evidence, a changed account or
    /// credential generation, and any foreign route, descriptor, or witness.
    pub fn resolve_with_evidence(
        &self,
        model_id: &str,
    ) -> Result<(Arc<dyn LanguageModel>, CodexModelResolutionEvidence), NativeCodexAuthError> {
        let (model, evidence) = self.resolve_exact(model_id)?;
        let model: Arc<dyn LanguageModel> = model;
        Ok((model, evidence))
    }

    /// Resolves the model as [`Self::resolve_with_evidence`] does, with the
    /// hosted web search side-request executor and the image generation
    /// executor on the same route, login, model and client.
    ///
    /// # Errors
    ///
    /// Same failures as [`Self::resolve_with_evidence`].
    pub fn resolve_with_retrieval(
        &self,
        model_id: &str,
    ) -> Result<
        (
            Arc<dyn LanguageModel>,
            CodexModelResolutionEvidence,
            crate::CodexRetrieval,
            crate::CodexImages,
        ),
        NativeCodexAuthError,
    > {
        let (model, evidence) = self.resolve_exact(model_id)?;
        let retrieval = model.codex_retrieval();
        let images = model.codex_images();
        let model: Arc<dyn LanguageModel> = model;
        Ok((model, evidence, retrieval, images))
    }

    fn resolve_exact(
        &self,
        model_id: &str,
    ) -> Result<(Arc<OpenAiLanguageModel>, CodexModelResolutionEvidence), NativeCodexAuthError>
    {
        self.validate_route_account()?;
        let catalog = self
            .catalog
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?;
        let catalog = catalog.as_ref().ok_or(NativeCodexAuthError::Catalog)?;
        self.validate_catalog_fence(&catalog.credential_fence)?;
        if catalog.ambiguous_slugs.contains(model_id) {
            return Err(NativeCodexAuthError::Catalog);
        }
        let issued = catalog
            .descriptors
            .get(model_id)
            .ok_or(NativeCodexAuthError::Catalog)?
            .clone();
        let renewal = CodexReceiptRenewal {
            source: self.catalog_source(),
            model_id: model_id.to_owned(),
            semantics: CodexRowSemantics::of(&issued)?,
        };
        let credential_selection_epoch = issued.witness.credential_selection_epoch();
        let descriptor = issued.descriptor;
        let capacity_facts = issued.capacity_facts;
        let reasoning_efforts = issued.reasoning_efforts;
        let tool_mode = issued.tool_mode;
        let responses_lite = issued.responses_lite;
        let catalog_enum_fingerprint = issued.catalog_enum_fingerprint;
        let now = self.clock.now();
        let compaction_namespace = NamespaceId::new(CODEX_NATIVE_COMPACTION_NAMESPACE)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        if descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || descriptor.requested_model().as_str() != model_id
            || issued.observed_at > now
            || issued.expires_at <= now
            || descriptor
                .capabilities()
                .features()
                .iter()
                .any(|(capability, fact)| {
                    if *capability == Capability::ParallelTools {
                        fact.provenance().source() != FactSource::BuiltIn
                            || fact.provenance().expires_at().is_some()
                    } else {
                        fact.provenance().source() != FactSource::RouteEvidence
                            || fact.provenance().is_expired(now)
                    }
                })
            || !descriptor
                .capabilities()
                .native_namespace(&compaction_namespace)
                .is_some_and(|fact| {
                    fact.support() == Support::Native
                        && fact.provenance().source() == FactSource::RouteEvidence
                        && !fact.provenance().is_expired(now)
                })
        {
            return Err(NativeCodexAuthError::Catalog);
        }
        let account_fingerprint = self
            .route
            .account_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let credential_authority_binding = self
            .route
            .credential_authority_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let evidence = CodexModelResolutionEvidence {
            evidence_revision: 5,
            account_fingerprint,
            credential_authority_binding,
            selected_credential_generation: catalog.credential_fence.generation().get(),
            credential_selection_epoch,
            route_fingerprint: resolution_route_fingerprint(
                &self.route,
                &descriptor,
                account_fingerprint,
                credential_authority_binding,
                capacity_facts,
                reasoning_efforts,
                catalog_enum_fingerprint,
                responses_lite,
            )?,
            descriptor: descriptor.clone(),
            descriptor_digest: descriptor
                .binding_digest()
                .map_err(|_| NativeCodexAuthError::Catalog)?,
            catalog_receipt: issued.catalog_receipt,
            capacity_facts,
            reasoning_efforts,
            tool_mode,
            responses_lite,
            content_filter_guidance: issued.content_filter_guidance,
            catalog_enum_fingerprint,
            adapter_revision: OPENAI_RESPONSES_ADAPTER_REVISION,
            catalog_observed_at: issued.observed_at,
            catalog_expires_at: issued.expires_at,
            profile_revision: OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        };
        let target = OpenAiTarget::new_native_catalog_target(
            self.route.clone(),
            descriptor,
            issued.witness,
            Arc::clone(&self.clock),
        )
        .map_err(|_| NativeCodexAuthError::Catalog)?;
        // Auth keeps the 30s REST stall. Inference SSE (including compaction)
        // waits the streaming idle bound between frames: a 30s body idle
        // killed Codex auto-compact at ~32s while a later manual compact
        // needed 81s. Rebuild from the caller client so test TLS and
        // inherited observers stay attached.
        let mut inference_client = self
            .client
            .with_limits(Limits::for_streaming_inference())
            .map_err(|_| NativeCodexAuthError::Catalog)?
            .pooled();
        if let Some(observer) = &self.inference_observer {
            inference_client = inference_client.with_observer(Some(Arc::clone(observer)));
        }
        let model = OpenAiLanguageModel::new_codex(target, inference_client, self.config, renewal)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        Ok((Arc::new(model), evidence))
    }
}

impl CodexCatalogSource {
    #[allow(
        clippy::too_many_lines,
        reason = "catalog admission keeps exact lease, wire, enum, evidence, and witness checks together"
    )]
    async fn fetch_catalog(
        &self,
        renewal: Option<&CatalogRenewal<'_>>,
    ) -> Result<ResolvedCatalog, NativeCodexAuthError> {
        let _lifecycle = self.manager.lifecycle.lock().await;
        // A renewal already ran its own detached refresh (see
        // `refresh_detached`); this fetch only reads the result.
        if renewal.is_none() {
            self.manager
                .refresh_locked_with_force(
                    Instant::now() + Duration::from_secs(30),
                    None,
                    RefreshForce::IfDue,
                    None,
                )
                .await
                .map_err(|error| match error {
                    NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogRefresh,
                    other => other,
                })?;
        }
        let mut request_permit = self
            .manager
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire(SubscriptionRequestClass::Catalog))
            .transpose()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let fence = self.manager.load_fence().await?;
        if fence.status() != CredentialStatus::Ready
            || fence.account_binding() != &self.manager.selected_account()?
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let lease = self.manager.acquire_ready_lease(fence.clone()).await?;
        let credential = parse_stored(lease.payload())?;
        if credential.access_expires_at <= now_unix_secs().saturating_add(ACCESS_REFRESH_SKEW)
            || credential.account_id != fence.account_binding().as_str()
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let selection_epoch = credential_selection_epoch(
            self.manager.route_account_digest(),
            self.manager.authority_binding,
            &credential.selection_epoch,
        )?;
        if let Some(renewal) = renewal {
            renewal.check(fence.account_binding(), selection_epoch)?;
        }
        let requested_at = self.clock.now();
        // The backend versions the catalog projection from the exact native
        // client release. A fabricated future version can hide otherwise
        // admitted models, so this must remain bound to the reviewed profile.
        let path = format!(
            "{OPENAI_CODEX_MODELS_PATH}?client_version={}",
            crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION
        );
        let mut headers =
            bearer_headers(&credential.access_token, false).map_err(|error| match error {
                NativeCodexAuthError::Validation => {
                    NativeCodexAuthError::CatalogAuthorizationHeader
                }
                other => other,
            })?;
        let account = sensitive_header_value(credential.account_id.as_bytes().to_vec())
            .map_err(|_| NativeCodexAuthError::CatalogAccountHeader)?;
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        if let Some(permit) = &mut request_permit {
            permit
                .authorize_dispatch()
                .map_err(|_| NativeCodexAuthError::Catalog)?;
        }
        let reply = execute_catalog_bounded(
            &self.client,
            &self.manager.endpoints.chatgpt_origin,
            Method::GET,
            &path,
            headers,
            Vec::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .await;
        drop(credential);
        drop(lease);
        let reply = reply?;
        require_status(&reply, StatusCode::OK)?;
        require_json_media_type(&reply).map_err(|error| match error {
            NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogMediaType,
            other => other,
        })?;
        let document: CatalogDocument =
            parse_catalog_json(&reply.body).map_err(|error| match error {
                NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogDocument,
                other => other,
            })?;
        if document.models.is_empty() || document.models.len() > MAX_CATALOG_MODELS {
            return Err(NativeCodexAuthError::Catalog);
        }
        // Catalog evidence lasts a fixed window from its observation, as
        // SuperGrok's does (spec 01 §7.4), independent of the access token:
        // runtime authority is separately bounded by the access expiry, and a
        // running model renews its credential receipt inside this window.
        let observed = renewal.map_or_else(
            || Ok(self.clock.now()),
            |renewal| renewal.observed_at(requested_at),
        )?;
        let expiry_seconds = observed
            .seconds()
            .saturating_add(CATALOG_TTL_SECONDS)
            .min(PROFILE_EXPIRES_AT);
        let expires =
            Timestamp::new(expiry_seconds, 0).map_err(|_| NativeCodexAuthError::Catalog)?;
        if expires <= self.clock.now() {
            return Err(NativeCodexAuthError::Catalog);
        }
        let provenance = FactProvenance::new(FactSource::RouteEvidence, observed, Some(expires));
        let release_provenance = FactProvenance::new(FactSource::BuiltIn, observed, None);
        let account_fingerprint = self
            .route
            .account_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let credential_authority_binding = self
            .route
            .credential_authority_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let context_authority_binding = RuntimeModelAuthority::authority_class_for(
            account_fingerprint,
            credential_authority_binding,
        )
        .map_err(|_| NativeCodexAuthError::Catalog)?;
        #[derive(Deserialize)]
        struct CatalogSlug {
            slug: String,
        }
        let mut slug_counts = BTreeMap::<String, usize>::new();
        for raw in &document.models {
            if let Ok(row) = parse_catalog_json::<CatalogSlug>(raw.get().as_bytes()) {
                *slug_counts.entry(row.slug).or_default() += 1;
            }
        }
        let ambiguous_slugs = slug_counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(slug, _)| slug.clone())
            .collect::<BTreeSet<_>>();
        let mut entries = Vec::new();
        let mut descriptors = BTreeMap::new();
        for raw in document.models {
            let Ok(model) = parse_catalog_json::<CatalogModelWire>(raw.get().as_bytes()) else {
                continue;
            };
            let Some(visibility) = model.visibility.recognized() else {
                // Visibility is a required closed enum in the pinned reference
                // client. An unknown row is observational only, never executable.
                continue;
            };
            if ambiguous_slugs.contains(&model.slug)
                || validate_identifier(&model.slug, MAX_MODEL_ID_BYTES).is_err()
                || !executable_codex_catalog_model(&model.slug)
            {
                continue;
            }
            let Ok(capacity_facts) = CodexCatalogCapacityFacts::from_wire(&model) else {
                continue;
            };
            if model.input_modalities.iter().collect::<BTreeSet<_>>().len()
                != model.input_modalities.len()
                || model
                    .supported_reasoning_levels
                    .iter()
                    .map(|level| &level.effort)
                    .collect::<BTreeSet<_>>()
                    .len()
                    != model.supported_reasoning_levels.len()
            {
                continue;
            }
            let text = model.input_modalities.contains(&CatalogInputModality::Text);
            if !text {
                continue;
            }
            let tools = matches!(
                model.shell_type,
                CatalogShellType::Default
                    | CatalogShellType::Local
                    | CatalogShellType::UnifiedExec
                    | CatalogShellType::ShellCommand
            );
            // Codex 0.162.1 omits parallel tool calls for Responses Lite rows.
            // The standard request builder can still enable them with tools.
            let parallel_tools = tools && !model.use_responses_lite;
            // The descriptor records the coarse existence of reasoning. The
            // provider compiler separately admits the exact requested effort
            // against this witnessed set, so a partial list grants no tiers.
            let reasoning_efforts =
                CodexReasoningEffortSet::from_catalog(&model.supported_reasoning_levels);
            let reasoning_effort_options: Vec<ReasoningEffort> = model
                .supported_reasoning_levels
                .iter()
                .filter_map(|level| level.effort.portable())
                .filter(|effort| *effort != ReasoningEffort::Ultra)
                .collect();
            let default_reasoning_effort = match model.default_reasoning_level.as_ref() {
                None => None,
                Some(effort) => match effort.portable() {
                    Some(ReasoningEffort::Ultra) => None,
                    Some(effort) if reasoning_effort_options.contains(&effort) => Some(effort),
                    Some(_) | None => continue,
                },
            };
            let Ok((service_tiers, default_service_tier)) = catalog_service_tiers(&model) else {
                continue;
            };
            let catalog_enum_fingerprint = catalog_enum_fingerprint(&model)?;
            let tool_mode = model.tool_mode.recognized();
            let reasoning = reasoning_efforts.supports_named_reasoning();
            if let Some(expected_tool_mode) = release_enrolled_codex_tool_mode(&model.slug) {
                if !tools || !reasoning {
                    continue;
                }
                let tool_mode_matches = match expected_tool_mode {
                    CodexCatalogToolMode::CodeModeOnly => matches!(
                        tool_mode,
                        Some(CodexCatalogToolMode::CodeModeOnly)
                            | Some(CodexCatalogToolMode::Direct)
                    ),
                    _ => tool_mode == Some(expected_tool_mode),
                };
                if !tool_mode_matches {
                    continue;
                }
            }
            let mut features = BTreeMap::new();
            features.insert(
                Capability::Streaming,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
            if tools {
                features.insert(
                    Capability::Tools,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                );
            }
            // The live Codex catalog row's `input_modalities` is the evidence,
            // on Standard and Responses Lite rows alike.
            if model
                .input_modalities
                .contains(&CatalogInputModality::Image)
            {
                features.insert(
                    Capability::ImageInput,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                );
            }
            if parallel_tools {
                features.insert(
                    Capability::ParallelTools,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        release_provenance,
                    ),
                );
            }
            if reasoning {
                features.insert(
                    Capability::Reasoning,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                );
            }
            let requested = ModelId::new(&model.slug).map_err(|_| NativeCodexAuthError::Catalog)?;
            let compaction_namespace = NamespaceId::new(CODEX_NATIVE_COMPACTION_NAMESPACE)
                .map_err(|_| NativeCodexAuthError::Catalog)?;
            let mut native_namespaces = BTreeMap::new();
            native_namespaces.insert(
                compaction_namespace,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
            let descriptor_without_context = ResolvedModelDescriptor::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                self.route.dialect().protocol().clone(),
                RouteId::new(crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)
                    .map_err(|_| NativeCodexAuthError::Catalog)?,
                requested.clone(),
                Some(requested),
                Some(
                    ModelFamilyId::new(RESPONSES_FAMILY_ID)
                        .map_err(|_| NativeCodexAuthError::Catalog)?,
                ),
                ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
                    .map_err(|_| NativeCodexAuthError::Catalog)?,
                CapabilitySet::new(features, native_namespaces),
            );
            let catalog_receipt = resolution_route_fingerprint(
                &self.route,
                &descriptor_without_context,
                account_fingerprint,
                credential_authority_binding,
                capacity_facts,
                reasoning_efforts,
                catalog_enum_fingerprint,
                model.use_responses_lite,
            )?;
            let descriptor = descriptor_without_context.with_context_contract(
                reference_client_envelope_context_contract(
                    context_authority_binding,
                    capacity_facts,
                    observed,
                    expires,
                    catalog_receipt,
                )?,
            );
            let witness = self.manager.issue_catalog_witness(
                &descriptor,
                &fence,
                selection_epoch,
                observed,
                expires,
                model.use_responses_lite,
            )?;
            descriptors.insert(
                model.slug.clone(),
                WitnessedDescriptor {
                    descriptor,
                    witness,
                    capacity_facts,
                    reasoning_efforts,
                    tool_mode,
                    responses_lite: model.use_responses_lite,
                    content_filter_guidance: catalog_content_filter_guidance(&model)
                        .map(str::to_owned),
                    catalog_enum_fingerprint,
                    catalog_receipt,
                    observed_at: observed,
                    expires_at: expires,
                },
            );
            entries.push(CodexCatalogEntry {
                model_id: model.slug,
                visibility: Some(visibility),
                priority: model.priority,
                api_support: if model.supported_in_api {
                    CodexCatalogApiSupport::PlatformEligible
                } else {
                    CodexCatalogApiSupport::SubscriptionOnly
                },
                text,
                tools,
                parallel_tools,
                wire_mode: if model.use_responses_lite {
                    CodexResponsesWireMode::Lite
                } else {
                    CodexResponsesWireMode::Standard
                },
                tool_mode,
                reasoning_effort_options,
                default_reasoning_effort,
                reasoning_efforts,
                service_tiers,
                default_service_tier,
                capacity_facts,
                observed_at: observed,
                expires_at: expires,
            });
        }
        entries.sort_by_key(CodexCatalogEntry::priority);
        if entries.is_empty() {
            return Err(NativeCodexAuthError::Catalog);
        }
        Ok(ResolvedCatalog {
            public: CodexModelCatalog {
                entries,
                profile_id: OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
                source_revision: crate::config::OPENAI_CODEX_CATALOG_SOURCE_REVISION,
            },
            descriptors,
            ambiguous_slugs,
            credential_fence: fence,
        })
    }
}

impl fmt::Debug for CodexModelResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexModelResolver")
            .field("profile", &OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .field("catalog", &"<credential-free exact snapshot>")
            .field("clock", &"<injected clock>")
            .finish()
    }
}

#[cfg(test)]
fn system_timestamp() -> Timestamp {
    SystemClock.now()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
