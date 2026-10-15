//! Plak-owned, revision-pinned ChatGPT/Codex authentication and model discovery.

mod verify;
use verify::{RsaJwk, validate_exact_scopes};
#[cfg(test)]
use verify::{rsa_components_are_acceptable, validate_jose_header};

mod dispatch;

pub(crate) use dispatch::{
    CodexDispatchGuard, CodexResponsesDialect, codex_credential_rejected,
    codex_responses_preflight, revalidate_codex_responses,
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
    CredentialRefreshObserver, FetchedJwks, JwksCache, JwksResolvePolicy, LoopbackConnectionEvent,
    PullBodyError, bind_loopback_listener, parse_cache_control_max_age, pull_claimed_body,
    run_browser_loopback_session, run_claimed_refresh, run_claimed_revoke,
};
#[cfg(test)]
use plakous_auth::oauth_workflow::format_http11_text_response;
use plakous_auth::oauth_workflow::{
    AuthorizeUrlRequest, CallbackQueryPolicy, DevicePollSchedule, LoopbackCallbackOutcome,
    LoopbackCallbackPage, OAuthCallbackOutcome, OAuthWorkflowError, access_needs_refresh,
    authorization_code_token_form, build_authorization_url, capped_deadline, decode_json_bounded,
    parse_http11_get_request, parse_oauth_callback_query, parse_remote_authorization_input,
    remaining_until, unsigned_access_token_exp, urlencoded_form_bytes,
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
const REMOTE_BROWSER_REDIRECT_URI: &str = CODEX_OAUTH.loopback_redirect_uri;
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
}

impl ProfileEndpoints {
    fn production() -> Self {
        Self {
            auth_origin: EndpointOrigin::parse(OPENAI_AUTH_ORIGIN)
                .unwrap_or_else(|error| unreachable!("static auth origin is valid: {error}")),
            chatgpt_origin: EndpointOrigin::parse(OPENAI_CHATGPT_ORIGIN)
                .unwrap_or_else(|error| unreachable!("static ChatGPT origin is valid: {error}")),
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

/// Non-cloneable browser login handle. Diagnostics redact URL correlation data.
pub struct CodexBrowserLogin {
    listener: TcpListener,
    attempt: BrowserLoginAttempt,
}

struct BrowserLoginAttempt {
    authorization_url: Zeroizing<String>,
    redirect_uri: String,
    credential_baseline: Option<CredentialFence>,
    state: AuthSecretString,
    pkce: plakous_auth::PkceS256,
    deadline: Instant,
}

impl CodexBrowserLogin {
    /// Returns the exact provider URL the application may open for the operator.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.attempt.authorization_url
    }
}

impl fmt::Debug for CodexBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexBrowserLogin(<redacted one-use login>)")
    }
}

/// Non-cloneable browser login handle for a callback pasted from another machine.
/// Diagnostics redact URL correlation data.
pub struct CodexRemoteBrowserLogin {
    attempt: BrowserLoginAttempt,
}

impl CodexRemoteBrowserLogin {
    /// Returns the exact provider URL the application may show to the operator.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.attempt.authorization_url
    }
}

impl fmt::Debug for CodexRemoteBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexRemoteBrowserLogin(<redacted one-use login>)")
    }
}

/// Non-cloneable pinned custom-device-flow login handle.
pub struct CodexDeviceLogin {
    verification_url: String,
    user_code: AuthSecretString,
    device_auth_id: AuthSecretString,
    interval: Duration,
    deadline: Instant,
    credential_baseline: Option<CredentialFence>,
}

impl CodexDeviceLogin {
    /// Returns the exact verification origin/path.
    #[must_use]
    pub fn verification_url(&self) -> &str {
        &self.verification_url
    }

    /// Exposes the short-lived operator code only to the explicit login UI.
    #[must_use]
    pub fn user_code(&self) -> &str {
        self.user_code.expose_secret()
    }
}

impl fmt::Debug for CodexDeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexDeviceLogin(<redacted one-use login>)")
    }
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

impl CodexSubscriptionCredentialManager {
    fn new_browser_login_attempt(
        &self,
        redirect_uri: String,
    ) -> Result<BrowserLoginAttempt, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let credential_baseline = self.store.load().map_err(|_| NativeCodexAuthError::Store)?;
        let state =
            generate_oauth_correlation_token().map_err(|_| NativeCodexAuthError::Binding)?;
        let pkce = plakous_auth::PkceS256::generate().map_err(|_| NativeCodexAuthError::Binding)?;
        // Dialect-owned extras stay local; shared builder owns the PKCE core.
        // Manager endpoint origin stays authoritative (including test doubles).
        let authorization_url = build_authorization_url(AuthorizeUrlRequest {
            pins: CODEX_OAUTH,
            auth_origin: Some(self.endpoints.auth_origin.as_str()),
            redirect_uri: &redirect_uri,
            code_challenge: pkce.challenge().as_str(),
            state: state.expose_secret(),
            nonce: None,
            extra_pairs: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "plak"),
            ],
        })
        .map_err(|_| NativeCodexAuthError::Binding)?;
        Ok(BrowserLoginAttempt {
            authorization_url,
            redirect_uri,
            credential_baseline,
            state,
            pkce,
            deadline: Instant::now() + BROWSER_DEADLINE,
        })
    }

    /// Starts an exact loopback authorization-code-with-PKCE login.
    ///
    /// The returned handle owns the listener, state, verifier, and deadline and
    /// can be completed only once by consuming it.
    ///
    /// # Errors
    ///
    /// Rejects stale profile acceptance, unavailable pinned loopback ports, or
    /// failure to create strong state and PKCE material.
    pub async fn begin_browser_login(&self) -> Result<CodexBrowserLogin, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let (listener, port) = bind_loopback_listener(CODEX_OAUTH.loopback_ports)
            .await
            .map_err(|_| NativeCodexAuthError::Binding)?;
        if !CODEX_OAUTH.allows_loopback_port(port) {
            return Err(NativeCodexAuthError::Binding);
        }
        let redirect_uri = format!("http://localhost:{port}{BROWSER_CALLBACK_PATH}");
        Ok(CodexBrowserLogin {
            listener,
            attempt: self.new_browser_login_attempt(redirect_uri)?,
        })
    }

    /// Starts an authorization-code-with-PKCE login whose final redirect URL
    /// can be pasted back from a browser running on another machine.
    ///
    /// This does not bind or listen on a local socket. The provider redirect is
    /// pinned to `http://localhost:1455/auth/callback` for exact manual review.
    ///
    /// # Errors
    ///
    /// Rejects stale profile acceptance or failure to create strong state and
    /// PKCE material.
    pub fn begin_remote_browser_login(
        &self,
    ) -> Result<CodexRemoteBrowserLogin, NativeCodexAuthError> {
        Ok(CodexRemoteBrowserLogin {
            attempt: self.new_browser_login_attempt(REMOTE_BROWSER_REDIRECT_URI.to_owned())?,
        })
    }

    /// Completes one exact browser callback, validates OIDC and `UserInfo`, then
    /// atomically installs the selected account generation.
    ///
    /// # Errors
    ///
    /// Rejects an expired or inexact callback and any transport, token,
    /// identity, account-continuity, or persistence failure.
    pub async fn complete_browser_login(
        &self,
        login: CodexBrowserLogin,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let mut hooks = CodexBrowserLoopbackHooks {
            manager: self,
            deadline: login.attempt.deadline,
            attempt: Some(login.attempt),
        };
        run_browser_loopback_session(login.listener, &mut hooks).await
    }

    /// Completes one exact pasted browser redirect, validates OIDC and
    /// `UserInfo`, then atomically installs the selected account generation.
    ///
    /// # Errors
    ///
    /// Rejects an expired handle; an oversized or inexact redirect URL; an
    /// invalid bare authorization code; invalid callback query grammar or
    /// state; and any transport, token, identity, account-continuity, or
    /// persistence failure.
    pub async fn complete_remote_browser_login(
        &self,
        login: CodexRemoteBrowserLogin,
        authorization_input: &str,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let callback = parse_remote_authorization_input(
            authorization_input,
            login.attempt.state.expose_secret(),
            REMOTE_BROWSER_REDIRECT_URI,
            codex_callback_policy(),
        )
        .map_err(map_oauth_workflow_error)?;
        self.complete_browser_callback(login.attempt, callback)
            .await
    }

    async fn complete_browser_callback(
        &self,
        login: BrowserLoginAttempt,
        callback: OAuthCallbackOutcome,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        remaining_login_time(login.deadline, NativeCodexAuthError::Callback)?;
        let code = match callback {
            OAuthCallbackOutcome::AuthorizationCode { code, state } => {
                if !constant_time_value_eq(login.state.expose_secret(), state.as_str()) {
                    return Err(NativeCodexAuthError::Callback);
                }
                code
            }
            OAuthCallbackOutcome::ProviderError { state } => {
                if !constant_time_value_eq(login.state.expose_secret(), state.as_str()) {
                    return Err(NativeCodexAuthError::Callback);
                }
                return Err(NativeCodexAuthError::Provider);
            }
        };
        let remaining = remaining_login_time(login.deadline, NativeCodexAuthError::Callback)?;
        let _lifecycle = tokio::time::timeout(remaining, self.lifecycle.lock())
            .await
            .map_err(|_| NativeCodexAuthError::Callback)?;
        self.validate_profile_admission()?;
        let credential = self
            .exchange_authorization_code(
                &code,
                &login.redirect_uri,
                login.pkce.verifier().expose_secret(),
                None,
                login.deadline,
            )
            .await?;
        self.commit_login(credential, login.credential_baseline, login.deadline)
            .await?;
        Ok(CodexCredentialStatus::Ready)
    }

    /// Starts the pinned Codex custom device flow.
    ///
    /// # Errors
    ///
    /// Rejects stale profile acceptance and malformed or failed pinned device
    /// authorization responses.
    pub async fn begin_device_login(&self) -> Result<CodexDeviceLogin, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let credential_baseline = self.store.load().map_err(|_| NativeCodexAuthError::Store)?;
        let body = serde_json::to_vec(&DeviceUserCodeRequest {
            client_id: OPENAI_CODEX_NATIVE_CLIENT_ID,
        })
        .map_err(|_| NativeCodexAuthError::Validation)?;
        let reply = execute_bounded(
            &self.client,
            &self.endpoints.auth_origin,
            Method::POST,
            DEVICE_USER_CODE_PATH,
            json_headers(None)?,
            body,
            Instant::now() + Duration::from_secs(30),
        )
        .await?;
        require_status(&reply, StatusCode::OK)?;
        require_json_media_type(&reply)?;
        let mut response: DeviceUserCodeResponse = parse_strict_json(&reply.body)?;
        validate_oauth_value(&response.device_auth_id, MAX_OAUTH_VALUE_BYTES)?;
        validate_user_code(&response.user_code)?;
        let interval = response
            .interval
            .parse::<u64>()
            .map_err(|_| NativeCodexAuthError::Validation)?;
        if interval == 0 || interval > MAX_DEVICE_INTERVAL_SECONDS {
            return Err(NativeCodexAuthError::Validation);
        }
        Ok(CodexDeviceLogin {
            verification_url: endpoint_url(&self.endpoints.auth_origin, DEVICE_VERIFICATION_PATH)?
                .into(),
            user_code: AuthSecretString::from(std::mem::take(&mut response.user_code)),
            device_auth_id: AuthSecretString::from(std::mem::take(&mut response.device_auth_id)),
            interval: Duration::from_secs(interval),
            deadline: Instant::now() + DEVICE_DEADLINE,
            credential_baseline,
        })
    }

    /// Completes one pinned custom device flow and atomically installs it.
    ///
    /// # Errors
    ///
    /// Rejects expiration, malformed verifier/challenge material, and any
    /// transport, token, identity, account-continuity, or persistence failure.
    pub async fn complete_device_login(
        &self,
        login: CodexDeviceLogin,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let mut code = loop {
            if Instant::now() >= login.deadline {
                return Err(NativeCodexAuthError::Provider);
            }
            let body = serde_json::to_vec(&DevicePollRequest {
                device_auth_id: login.device_auth_id.expose_secret(),
                user_code: login.user_code.expose_secret(),
            })
            .map_err(|_| NativeCodexAuthError::Validation)?;
            let reply = execute_bounded(
                &self.client,
                &self.endpoints.auth_origin,
                Method::POST,
                DEVICE_TOKEN_PATH,
                json_headers(None)?,
                body,
                capped_login_deadline(
                    login.deadline,
                    AUTH_REQUEST_CAP,
                    NativeCodexAuthError::Provider,
                )?,
            )
            .await?;
            match reply.status {
                StatusCode::OK => {
                    require_json_media_type(&reply)?;
                    break parse_strict_json::<DeviceCodeResponse>(&reply.body)?;
                }
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                    // Custom Codex device flow has no RFC 8628 slow_down; shared
                    // schedule still owns remaining-time-clamped sleep math.
                    let schedule = DevicePollSchedule::new(
                        login.interval,
                        Duration::from_secs(MAX_DEVICE_INTERVAL_SECONDS),
                        Duration::ZERO,
                        login.deadline,
                    )
                    .map_err(|_| NativeCodexAuthError::Validation)?;
                    if schedule.is_expired(Instant::now()) {
                        return Err(NativeCodexAuthError::Provider);
                    }
                    tokio::time::sleep(schedule.sleep_for(Instant::now())).await;
                }
                _ => return Err(NativeCodexAuthError::Provider),
            }
        };
        validate_oauth_value(&code.authorization_code, MAX_OAUTH_VALUE_BYTES)?;
        validate_oauth_value(&code.code_verifier, 128)?;
        validate_oauth_value(&code.code_challenge, 128)?;
        let verifier = plakous_auth::PkceVerifier::new(AuthSecretString::from(std::mem::take(
            &mut code.code_verifier,
        )))
        .map_err(|_| NativeCodexAuthError::Validation)?;
        let pkce = plakous_auth::PkceS256::from_verifier(verifier);
        if !constant_time_value_eq(pkce.challenge().as_str(), &code.code_challenge) {
            return Err(NativeCodexAuthError::Validation);
        }
        let redirect_uri = endpoint_url(&self.endpoints.auth_origin, DEVICE_REDIRECT_PATH)?;
        let remaining = remaining_login_time(login.deadline, NativeCodexAuthError::Provider)?;
        let _lifecycle = tokio::time::timeout(remaining, self.lifecycle.lock())
            .await
            .map_err(|_| NativeCodexAuthError::Provider)?;
        self.validate_profile_admission()?;
        let credential = self
            .exchange_authorization_code(
                &code.authorization_code,
                redirect_uri.as_str(),
                pkce.verifier().expose_secret(),
                None,
                login.deadline,
            )
            .await?;
        self.commit_login(credential, login.credential_baseline, login.deadline)
            .await?;
        Ok(CodexCredentialStatus::Ready)
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        expected_identity: Option<(&str, &str)>,
        deadline: Instant,
    ) -> Result<StoredCredential, NativeCodexAuthError> {
        remaining_login_time(deadline, NativeCodexAuthError::LoginTokenTransport)?;
        validate_oauth_value(code, MAX_OAUTH_VALUE_BYTES)
            .map_err(|_| NativeCodexAuthError::LoginTokenTransport)?;
        let body = form_body(&authorization_code_token_form(
            code,
            redirect_uri,
            OPENAI_CODEX_NATIVE_CLIENT_ID,
            verifier,
        ));
        let reply = execute_bounded(
            &self.client,
            &self.endpoints.auth_origin,
            Method::POST,
            TOKEN_PATH,
            form_headers(),
            body,
            capped_login_deadline(
                deadline,
                AUTH_REQUEST_CAP,
                NativeCodexAuthError::LoginTokenTransport,
            )?,
        )
        .await
        .map_err(|_| NativeCodexAuthError::LoginTokenTransport)?;
        if !reply.status.is_success() {
            return Err(NativeCodexAuthError::LoginTokenRejected);
        }
        // The provider-owned Codex client parses a bounded successful body as
        // JSON without making token validity depend on an optional media-type
        // header. Preserve that compatibility contract here.
        let tokens: OAuthLoginTokenResponse = parse_strict_json(&reply.body)
            .map_err(|_| NativeCodexAuthError::LoginTokenResponseInvalid)?;
        self.validate_token_response(tokens, expected_identity, true, deadline)
            .await
    }

    async fn commit_login(
        &self,
        credential: StoredCredential,
        credential_baseline: Option<CredentialFence>,
        deadline: Instant,
    ) -> Result<(), NativeCodexAuthError> {
        remaining_login_time(deadline, NativeCodexAuthError::LoginPersistence)?;
        let store = Arc::clone(&self.store);
        let accepted_at = self.acceptance.accepted_at;
        let (account, committed) = run_blocking(move || {
            validate_profile_at(accepted_at, now_unix_secs())?;
            remaining_login_time(deadline, NativeCodexAuthError::LoginPersistence)?;
            let account = AccountBinding::new(&credential.account_id)
                .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
            remaining_login_time(deadline, NativeCodexAuthError::LoginPersistence)?;
            let committed = if let Some(fence) = credential_baseline {
                let next = fence
                    .generation()
                    .next()
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                let record = stored_record(&credential, next)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                store
                    .replace_selected_login(&fence, record)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?
            } else {
                let first = CredentialGeneration::new(1)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                let record = stored_record(&credential, first)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                let committed = record.fence();
                store
                    .initialize_selected(&record)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                committed
            };
            if Instant::now() >= deadline {
                store
                    .logout(committed.generation())
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                return Err(NativeCodexAuthError::LoginPersistence);
            }
            Ok((account, committed))
        })
        .await
        .map_err(login_persistence_error)?;
        if Instant::now() >= deadline {
            let store = Arc::clone(&self.store);
            run_blocking(move || {
                store
                    .logout(committed.generation())
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)
            })
            .await
            .map_err(login_persistence_error)?;
            return Err(NativeCodexAuthError::LoginPersistence);
        }
        self.install_selected_account(account)
            .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
        if Instant::now() >= deadline {
            let store = Arc::clone(&self.store);
            run_blocking(move || {
                store
                    .logout(committed.generation())
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)
            })
            .await
            .map_err(login_persistence_error)?;
            return Err(NativeCodexAuthError::LoginPersistence);
        }
        Ok(())
    }
}

fn login_persistence_error(error: NativeCodexAuthError) -> NativeCodexAuthError {
    match error {
        NativeCodexAuthError::ProfileNotAccepted | NativeCodexAuthError::ProfileNotCurrent => error,
        _ => NativeCodexAuthError::LoginPersistence,
    }
}

#[derive(Serialize)]
struct DeviceUserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct DeviceUserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    interval: String,
}

impl Drop for DeviceUserCodeResponse {
    fn drop(&mut self) {
        self.device_auth_id.zeroize();
        self.user_code.zeroize();
        self.interval.zeroize();
    }
}

#[derive(Serialize)]
struct DevicePollRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

impl Drop for DeviceCodeResponse {
    fn drop(&mut self) {
        self.authorization_code.zeroize();
        self.code_challenge.zeroize();
        self.code_verifier.zeroize();
    }
}

fn codex_callback_policy() -> CallbackQueryPolicy {
    CallbackQueryPolicy::codex(OPENAI_CODEX_OAUTH_SCOPES)
}

fn parse_browser_callback(
    request: &[u8],
    expected_port: u16,
) -> Result<OAuthCallbackOutcome, NativeCodexAuthError> {
    let text = std::str::from_utf8(request).map_err(|_| NativeCodexAuthError::Callback)?;
    let parsed = parse_http11_get_request(text).map_err(|_| NativeCodexAuthError::Callback)?;
    if !CODEX_OAUTH.loopback_host_matches(parsed.host, Some(expected_port)) {
        return Err(NativeCodexAuthError::Callback);
    }
    let (path, query) = parsed
        .target
        .split_once('?')
        .ok_or(NativeCodexAuthError::Callback)?;
    if path != BROWSER_CALLBACK_PATH || query.is_empty() {
        return Err(NativeCodexAuthError::Callback);
    }
    parse_oauth_callback_query(query, codex_callback_policy()).map_err(map_oauth_workflow_error)
}

const CODEX_CALLBACK_PAGE: LoopbackCallbackPage<'static> = LoopbackCallbackPage {
    complete: "Login complete",
    rejected: "Login rejected",
    failed: "Login failed",
    headers: &[("Cache-Control", "no-store")],
};

struct CodexBrowserLoopbackHooks<'a> {
    manager: &'a CodexSubscriptionCredentialManager,
    /// Session deadline retained after the attempt is consumed by complete.
    deadline: Instant,
    attempt: Option<BrowserLoginAttempt>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl BrowserLoopbackHooks for CodexBrowserLoopbackHooks<'_> {
    type Error = NativeCodexAuthError;
    type Callback = OAuthCallbackOutcome;
    type Preflight = ();
    type Output = CodexCredentialStatus;

    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn max_request_bytes(&self) -> usize {
        MAX_CALLBACK_BYTES
    }

    fn deadline_error(&self) -> Self::Error {
        NativeCodexAuthError::Callback
    }

    fn callback_error(&self) -> Self::Error {
        NativeCodexAuthError::Callback
    }

    fn parse_request(
        &mut self,
        request: &[u8],
        local_port: u16,
    ) -> Result<LoopbackConnectionEvent<Self::Callback, Self::Preflight>, Self::Error> {
        parse_browser_callback(request, local_port).map(LoopbackConnectionEvent::Callback)
    }

    fn preflight_response_bytes(&self, _preflight: &Self::Preflight) -> Vec<u8> {
        Vec::new()
    }

    fn callback_response_bytes(&self, complete: Result<&Self::Output, &Self::Error>) -> Vec<u8> {
        CODEX_CALLBACK_PAGE.response_bytes(match complete {
            Ok(_) => LoopbackCallbackOutcome::Complete,
            Err(NativeCodexAuthError::Callback) => LoopbackCallbackOutcome::Rejected,
            Err(_) => LoopbackCallbackOutcome::Failed,
        })
    }

    async fn complete_callback(
        &mut self,
        callback: Self::Callback,
    ) -> Result<Self::Output, Self::Error> {
        let attempt = self.attempt.take().ok_or(NativeCodexAuthError::Callback)?;
        self.manager
            .complete_browser_callback(attempt, callback)
            .await
    }
}

#[cfg(test)]
async fn write_callback_response<W>(
    stream: &mut W,
    status: StatusCode,
    body: &str,
    deadline: Instant,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let Ok(remaining) = remaining_login_time(deadline, NativeCodexAuthError::Callback) else {
        return;
    };
    let remaining = remaining.min(CALLBACK_RESPONSE_CAP);
    let response = format_http11_text_response(
        status.as_u16(),
        status.canonical_reason().unwrap_or("Result"),
        body,
        &[("Cache-Control", "no-store")],
    );
    let _ =
        write_all_before_deadline(stream, response.as_bytes(), Instant::now() + remaining).await;
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

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Serialize)]
struct RevokeRequest<'a> {
    token: &'a str,
    token_type_hint: &'a str,
    client_id: &'a str,
}

/// Shared-orchestrator hooks for Codex claimed refresh (JSON body, RS256).
struct CodexRefreshHooks<'a> {
    manager: &'a CodexSubscriptionCredentialManager,
    /// A receipt renewal refreshes only its own login selection.
    expected: Option<ExpectedSelection<'a>>,
    deadline: Instant,
    cancellation: Option<&'a plakous_model::CancellationToken>,
    flight: Option<AuthSingleFlightGuard>,
    request_permit: Option<Box<dyn SubscriptionRequestPermit>>,
    transport_cancellation: Option<CancellationToken>,
    cancelled_during_exchange: bool,
    /// Whether to refresh even when the stored expiry is outside the lead.
    force: RefreshForce<'a>,
}

/// When a claimed refresh runs although the stored access token is not yet
/// inside its refresh lead (the provider rejected it with HTTP 401).
#[derive(Clone, Copy)]
enum RefreshForce<'a> {
    /// Only when the access token is due.
    IfDue,
    /// Always (an explicit refresh).
    Always,
    /// Only while the store, read under the flight, still holds this exact
    /// rejected generation: a concurrent rotation of the same login is not
    /// rotated again.
    WhileCurrent(&'a CredentialFence),
}

impl RefreshForce<'_> {
    fn forces(self, current: &CredentialFence) -> bool {
        match self {
            Self::IfDue => false,
            Self::Always => true,
            Self::WhileCurrent(rejected) => rejected == current,
        }
    }
}

impl CodexRefreshHooks<'_> {
    fn check_expected(
        &self,
        fence: &CredentialFence,
        credential: &StoredCredential,
    ) -> Result<(), NativeCodexAuthError> {
        let Some(expected) = self.expected else {
            return Ok(());
        };
        expected.check(
            fence.account_binding(),
            credential_selection_epoch(
                self.manager.route_account_digest(),
                self.manager.authority_binding,
                &credential.selection_epoch,
            )?,
        )
    }
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRefreshHooks for CodexRefreshHooks<'_> {
    type Error = NativeCodexAuthError;
    type Prepared = StoredCredential;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let initial = self.manager.load_fence().await?;
        let selected = self.manager.selected_account()?;
        if initial.account_binding() != &selected {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if initial.status() == CredentialStatus::Ready {
            let lease = self.manager.acquire_ready_lease(initial.clone()).await?;
            let credential = parse_stored(lease.payload())?;
            self.check_expected(&initial, &credential)?;
            let fresh = !access_needs_refresh(
                credential.access_expires_at,
                now_unix_secs(),
                ACCESS_REFRESH_SKEW,
            );
            drop(credential);
            drop(lease);
            if fresh && !self.force.forces(&initial) {
                return Ok(None);
            }
        } else if initial.status() != CredentialStatus::RefreshRequired {
            return Err(NativeCodexAuthError::Unavailable);
        }

        self.flight = Some(self.manager.enter_flight(&initial).await?);
        let current = self.manager.load_fence().await?;
        if current.account_binding() != &selected {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if current.status() == CredentialStatus::Ready {
            let lease = self.manager.acquire_ready_lease(current.clone()).await?;
            let credential = parse_stored(lease.payload())?;
            self.check_expected(&current, &credential)?;
            let fresh = !access_needs_refresh(
                credential.access_expires_at,
                now_unix_secs(),
                ACCESS_REFRESH_SKEW,
            );
            drop(credential);
            drop(lease);
            if fresh && !self.force.forces(&current) {
                return Ok(None);
            }
        } else if current.status() != CredentialStatus::RefreshRequired {
            return Err(NativeCodexAuthError::Unavailable);
        }

        self.request_permit = self
            .manager
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire(SubscriptionRequestClass::AuthRefresh))
            .transpose()
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        Ok(Some(current))
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
        let claim_fence = claim.fence();
        // Nothing was sent: a payload this build cannot use is restored as it
        // was, never quarantined, and needs a new login.
        let old = match parse_stored(claim.payload()) {
            Ok(old) => old,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        let scopes = match validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES) {
            Ok(scopes) => scopes,
            Err(error) => return Err(BuildRequestFailure::LeaveQuarantined(error)),
        };
        if old.account_id != claim_fence.account_binding().as_str() || old.scopes != scopes {
            return Err(BuildRequestFailure::LeaveQuarantined(
                NativeCodexAuthError::Validation,
            ));
        }
        // Nothing was sent: a renewal that finds another login restores it.
        if let Err(error) = self.check_expected(&claim_fence, &old) {
            return Err(BuildRequestFailure::Restore(error));
        }
        let body = match serde_json::to_vec(&RefreshRequest {
            client_id: OPENAI_CODEX_NATIVE_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: &old.refresh_token,
        }) {
            Ok(body) => body,
            Err(_) => {
                return Err(BuildRequestFailure::Restore(
                    NativeCodexAuthError::Validation,
                ));
            }
        };
        let headers = match json_headers(None) {
            Ok(headers) => headers,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        let request = match claimed_request(
            &self.manager.client,
            &self.manager.endpoints.auth_origin,
            Method::POST,
            TOKEN_PATH,
            headers,
            body,
        ) {
            Ok(request) => request,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        Ok((old, request))
    }

    async fn authorize_dispatch(&mut self) -> Result<(), Self::Error> {
        if self
            .cancellation
            .is_some_and(plakous_model::CancellationToken::is_cancelled)
        {
            return Err(NativeCodexAuthError::Transport(None));
        }
        if let Some(permit) = &mut self.request_permit
            && permit.authorize_dispatch().is_err()
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        Ok(())
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        let transport = CancellationToken::new();
        self.transport_cancellation = Some(transport.clone());
        ExchangeControl::with_cancellation(
            self.deadline.min(Instant::now() + Duration::from_secs(30)),
            transport,
        )
    }

    async fn execute_claimed(
        &mut self,
        store: &PrivateCredentialStore,
        client: &Client,
        claim: RefreshClaim,
        request: Request,
        control: ExchangeControl,
    ) -> Result<ClaimedHttpResponse, ClaimedHttpError> {
        let mut exchange = Box::pin(store.execute_claimed_http(claim, client, request, control));
        if let Some(cancellation) = self.cancellation {
            tokio::select! {
                reply = &mut exchange => reply,
                () = cancellation.cancelled() => {
                    self.cancelled_during_exchange = true;
                    if let Some(transport) = &self.transport_cancellation {
                        transport.cancel();
                    }
                    exchange.await
                }
            }
        } else {
            exchange.await
        }
    }

    async fn validate_tokens(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> Result<CredentialRecord, Self::Error> {
        if self.cancelled_during_exchange {
            return Err(NativeCodexAuthError::Transport(None));
        }
        let body = pull_claimed_body_cancellable(response, self.cancellation).await?;
        if !response.status().is_success() {
            return Err(NativeCodexAuthError::Provider);
        }
        let tokens: OAuthRefreshTokenResponse = parse_strict_json(&body)?;
        let mut validation = Box::pin(self.manager.validate_refresh_response(
            tokens,
            prepared,
            self.deadline,
        ));
        let validated = if let Some(cancellation) = self.cancellation {
            tokio::select! {
                biased;
                result = &mut validation => Some(result),
                () = cancellation.cancelled() => None,
                () = tokio::time::sleep_until(self.deadline.into()) => None,
            }
        } else {
            Some(validation.await)
        };
        let replacement = match validated {
            None => return Err(NativeCodexAuthError::Transport(None)),
            Some(Ok(credential)) => credential,
            Some(Err(error)) => return Err(error),
        };
        let generation = response
            .refresh_fence()
            .generation()
            .next()
            .map_err(|_| NativeCodexAuthError::Store)?;
        stored_record(&replacement, generation)
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()?;
        validate_profile_at(self.manager.acceptance.accepted_at, now_unix_secs())
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_codex_claimed_http_error(error)
    }

    fn map_store_error(&mut self, _error: PrivateStoreError) -> Self::Error {
        NativeCodexAuthError::Store
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.manager.refresh_observer.as_deref()
    }
}

/// Shared-orchestrator hooks for Codex claimed remote revoke (JSON body).
struct CodexRevokeHooks<'a> {
    manager: &'a CodexSubscriptionCredentialManager,
    flight: Option<AuthSingleFlightGuard>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRevokeHooks for CodexRevokeHooks<'_> {
    type Error = NativeCodexAuthError;
    type Output = CodexCredentialStatus;

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()
    }

    async fn fence_to_revoke(&mut self) -> Result<CredentialFence, Self::Error> {
        let current = self.manager.load_fence().await?;
        if current.account_binding() != &self.manager.selected_account()? {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        self.flight = Some(self.manager.enter_flight(&current).await?);
        let current = self.manager.load_fence().await?;
        if current.account_binding() != &self.manager.selected_account()? {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        Ok(current)
    }

    fn build_revoke_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<Request, BuildRequestFailure<Self::Error>> {
        let claim_fence = claim.fence();
        // A login this build cannot parse (an earlier build's) is not
        // revoked remotely here; nothing was sent, so it is restored as it was.
        let credential = match parse_stored(claim.payload()) {
            Ok(credential) => credential,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        if credential.account_id != claim_fence.account_binding().as_str() {
            return Err(BuildRequestFailure::LeaveQuarantined(
                NativeCodexAuthError::AccountChanged,
            ));
        }
        let body = match serde_json::to_vec(&RevokeRequest {
            token: &credential.refresh_token,
            token_type_hint: "refresh_token",
            client_id: OPENAI_CODEX_NATIVE_CLIENT_ID,
        }) {
            Ok(body) => body,
            Err(_) => {
                return Err(BuildRequestFailure::Restore(
                    NativeCodexAuthError::Validation,
                ));
            }
        };
        let headers = match json_headers(None) {
            Ok(headers) => headers,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        claimed_request(
            &self.manager.client,
            &self.manager.endpoints.auth_origin,
            Method::POST,
            REVOKE_PATH,
            headers,
            body,
        )
        .map_err(BuildRequestFailure::Restore)
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + Duration::from_secs(30))
    }

    async fn accept_revoke_response(
        &mut self,
        response: &mut ClaimedHttpResponse,
    ) -> Result<(), Self::Error> {
        let _body = pull_claimed_body(response, MAX_AUTH_BODY_BYTES)
            .await
            .map_err(|error| match error {
                PullBodyError::Transport => NativeCodexAuthError::Transport(None),
                PullBodyError::TooLarge => NativeCodexAuthError::Validation,
                _ => NativeCodexAuthError::Transport(None),
            })?;
        if !matches!(response.status(), StatusCode::OK | StatusCode::NO_CONTENT) {
            return Err(NativeCodexAuthError::Provider);
        }
        Ok(())
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()?;
        validate_profile_at(self.manager.acceptance.accepted_at, now_unix_secs())
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_codex_claimed_http_error(error)
    }

    fn map_store_error(&mut self, _error: PrivateStoreError) -> Self::Error {
        NativeCodexAuthError::Store
    }

    fn output_from_revoked(
        &mut self,
        _generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error> {
        self.manager.clear_selected_account()?;
        Ok(CodexCredentialStatus::Revoked)
    }

    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.manager.refresh_observer.as_deref()
    }
}

fn map_codex_claimed_http_error(error: ClaimedHttpError) -> NativeCodexAuthError {
    match error.disposition() {
        ClaimedHttpDisposition::RestoredNotStarted
        | ClaimedHttpDisposition::QuarantinedMayHaveTransmitted => {
            NativeCodexAuthError::Transport(error.transport_failure().and_then(network_failure))
        }
        ClaimedHttpDisposition::RestorationFailed | ClaimedHttpDisposition::ClaimRejected => {
            NativeCodexAuthError::Store
        }
        _ => NativeCodexAuthError::Store,
    }
}

impl CodexSubscriptionCredentialManager {
    /// Refreshes only when the current access generation is near expiry.
    ///
    /// The old generation is durably Unknown before token I/O. After payload
    /// exposure, every transport, validation, `UserInfo`, or persistence failure
    /// leaves that generation quarantined and is never retried automatically.
    ///
    /// # Errors
    ///
    /// Rejects unavailable, changed, quarantined, or invalid credentials and
    /// any transport, provider, identity, profile, or persistence failure.
    pub async fn refresh_if_needed(&self) -> Result<(), NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.refresh_if_needed_locked(Instant::now() + Duration::from_secs(30), None)
            .await
    }

    async fn refresh_if_needed_locked(
        &self,
        deadline: Instant,
        cancellation: Option<&plakous_model::CancellationToken>,
    ) -> Result<(), NativeCodexAuthError> {
        self.refresh_locked_with_force(deadline, cancellation, RefreshForce::IfDue, None)
            .await
    }

    /// Runs the claimed refresh ladder; `force` claims a new generation even
    /// when the stored access token is not yet inside the refresh skew (the
    /// provider rejected that access token with HTTP 401).
    async fn refresh_locked_with_force(
        &self,
        deadline: Instant,
        cancellation: Option<&plakous_model::CancellationToken>,
        force: RefreshForce<'_>,
        expected: Option<ExpectedSelection<'_>>,
    ) -> Result<(), NativeCodexAuthError> {
        let mut hooks = CodexRefreshHooks {
            manager: self,
            expected,
            deadline,
            cancellation,
            flight: None,
            request_permit: None,
            transport_cancellation: None,
            cancelled_during_exchange: false,
            force,
        };
        run_claimed_refresh(&self.store, &self.client, &mut hooks).await
    }

    /// Forces one claimed refresh after the provider answered an inference
    /// dispatch with HTTP 401, regardless of the stored expiry.
    ///
    /// # Errors
    ///
    /// Same fail-closed failures as [`Self::refresh_if_needed`]; a rejected
    /// grant leaves the generation not Ready and is never retried here.
    pub async fn refresh_after_rejection(&self) -> Result<(), NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.refresh_locked_with_force(
            Instant::now() + Duration::from_secs(30),
            None,
            RefreshForce::Always,
            None,
        )
        .await
    }

    /// The 401 recovery of one receipt: forces a claimed refresh only while
    /// the store, read under the refresh flight, still holds the rejected
    /// generation, and only for its account and login selection. A generation
    /// another process already rotated is refreshed only if due; another login
    /// fails `SelectionChanged` or `AccountChanged` before any claim.
    async fn refresh_rejected_selection(
        &self,
        rejected: &CredentialFence,
        selection_epoch: Digest,
    ) -> Result<(), NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.refresh_locked_with_force(
            Instant::now() + Duration::from_secs(30),
            None,
            RefreshForce::WhileCurrent(rejected),
            Some(ExpectedSelection {
                account: rejected.account_binding(),
                selection_epoch,
            }),
        )
        .await
    }

    /// Revokes the rotating refresh token and commits a local tombstone.
    /// Any post-start failure remains quarantined.
    ///
    /// # Errors
    ///
    /// Rejects unavailable or changed credentials and any failed remote revoke
    /// or exact local tombstone transition.
    pub async fn logout(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let mut hooks = CodexRevokeHooks {
            manager: self,
            flight: None,
        };
        run_claimed_revoke(&self.store, &self.client, &mut hooks).await
    }

    /// Alias making explicit that logout performs provider revocation first.
    ///
    /// # Errors
    ///
    /// Returns the same redaction-safe failures as [`Self::logout`].
    pub async fn revoke(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.logout().await
    }

    /// Commits an always-available local revoked tombstone without network I/O.
    ///
    /// This cleanup remains available after profile expiry or kill-switch
    /// activation and is deliberately distinct from [`Self::revoke`].
    ///
    /// # Errors
    ///
    /// Fails only when the explicit private slot cannot be reloaded or changed
    /// by exact generation CAS.
    pub async fn logout_local(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let store = Arc::clone(&self.store);
        let status = run_blocking(move || local_logout(&store)).await?;
        self.clear_selected_account()?;
        Ok(status)
    }

    async fn load_fence(&self) -> Result<CredentialFence, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .load()
                .map_err(|_| NativeCodexAuthError::Store)?
                .ok_or(NativeCodexAuthError::Unavailable)
        })
        .await
    }

    async fn try_load_fence(&self) -> Result<CredentialFence, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .try_load()
                .map_err(|error| match error {
                    PrivateStoreError::StoreBusy => NativeCodexAuthError::StoreBusy,
                    _ => NativeCodexAuthError::Store,
                })?
                .ok_or(NativeCodexAuthError::Unavailable)
        })
        .await
    }

    /// Whether a durable Unknown `fence` is a settled outcome rather than a
    /// refresh still in flight in this or another process; see
    /// [`PrivateCredentialStore::refresh_outcome_settled`].
    async fn refresh_outcome_settled(
        &self,
        fence: CredentialFence,
    ) -> Result<bool, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .refresh_outcome_settled(&fence)
                .map_err(|_| NativeCodexAuthError::Store)
        })
        .await
    }

    async fn acquire_ready_lease(
        &self,
        fence: CredentialFence,
    ) -> Result<ReadyCredentialLease, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .acquire_ready(&fence)
                .map_err(|_| NativeCodexAuthError::Store)
        })
        .await
    }

    /// Attempts a shared Ready lease without waiting. `Ok(None)` means another
    /// holder currently has the store lease; it is contention, not a fault.
    async fn try_acquire_ready_lease(
        &self,
        fence: CredentialFence,
    ) -> Result<Option<ReadyCredentialLease>, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .try_acquire_ready(&fence)
                .map_err(|_| NativeCodexAuthError::Store)
        })
        .await
    }

    async fn enter_flight(
        &self,
        fence: &CredentialFence,
    ) -> Result<AuthSingleFlightGuard, NativeCodexAuthError> {
        let flights = self.flights.clone();
        let profile = fence.profile_id().clone();
        let account = fence.account_binding().clone();
        tokio::task::spawn_blocking(move || flights.enter(&profile, &account))
            .await
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .map_err(|_| NativeCodexAuthError::Coordination)
    }

    async fn revalidate_model_runtime_authority(
        &self,
        descriptor: &ResolvedModelDescriptor,
        witness: &NativeCodexCatalogWitness,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        if witness.observed_at() > checked_at || witness.expires_at() <= checked_at {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        validate_profile(&self.acceptance, checked_at.seconds())
            .map_err(|_| RuntimeModelAuthorityError::Stale)?;
        if descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || self.validate_catalog_witness(descriptor, witness).is_err()
        {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let _lifecycle = self.lifecycle.lock().await;
        validate_profile(&self.acceptance, checked_at.seconds())
            .map_err(|_| RuntimeModelAuthorityError::Stale)?;
        if self.validate_catalog_witness(descriptor, witness).is_err() {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let expected = witness.credential_fence();
        let current = self
            .load_fence()
            .await
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let selected = self
            .selected_account()
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let account_binding = self.route_account_digest();
        if current != *expected
            || current.status() != CredentialStatus::Ready
            || current.profile_id().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || current.account_binding() != &selected
        {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let lease = self
            .acquire_ready_lease(current.clone())
            .await
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let credential =
            parse_stored(lease.payload()).map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let stored_epoch = credential_selection_epoch(
            account_binding,
            self.authority_binding,
            &credential.selection_epoch,
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let usable_until_seconds = credential
            .access_expires_at
            .saturating_sub(ACCESS_REFRESH_SKEW);
        if lease.fence() != current
            || witness.credential_selection_epoch() != stored_epoch
            || credential.account_id != selected.as_str()
            || usable_until_seconds <= checked_at.seconds()
            || validate_exact_scopes(&credential.scopes.join(" ")).is_err()
        {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let access_until = Timestamp::new(usable_until_seconds, 0)
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let profile_until = Timestamp::new(PROFILE_EXPIRES_AT, 0)
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let valid_until = witness.expires_at().min(access_until).min(profile_until);
        if valid_until <= checked_at {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        RuntimeModelAuthority::new(
            descriptor,
            account_binding,
            self.authority_binding,
            current.generation().get(),
            checked_at,
            Some(valid_until),
        )
    }

    fn validate_preflight_target(
        &self,
        request: &NativeCodexPreflightRequest,
    ) -> Result<(), NativeCodexAuthError> {
        self.validate_profile_admission()
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        let target = request.target();
        if target.route_id().as_str() != crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID
            || target.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || target.account_binding() != self.route_account_digest()
            || target.credential_authority_binding() != self.authority_binding
            || target.model().as_str().is_empty()
        {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
            return Err(NativeCodexAuthError::Transport(None));
        }
        let Some((expected_fence, _)) = target.native_codex_credential_selection() else {
            return Err(NativeCodexAuthError::Unavailable);
        };
        let selected_account = self.selected_account()?;
        if expected_fence.status() != CredentialStatus::Ready
            || expected_fence.profile_id().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || expected_fence.account_binding() != &selected_account
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        Ok(())
    }

    async fn preflight_authorization(
        &self,
        request: NativeCodexPreflightRequest,
    ) -> Result<NativeCodexAuthorization, NativeCodexAuthError> {
        let deadline = request.deadline();
        let cancellation = request.cancellation().clone();
        let operation = self.preflight_authorization_inner(request);
        tokio::pin!(operation);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(NativeCodexAuthError::Transport(None)),
            () = tokio::time::sleep_until(deadline.into()) => Err(NativeCodexAuthError::Transport(None)),
            result = &mut operation => result,
        }
    }

    async fn preflight_authorization_inner(
        &self,
        request: NativeCodexPreflightRequest,
    ) -> Result<NativeCodexAuthorization, NativeCodexAuthError> {
        self.validate_preflight_target(&request)?;
        let lifecycle = self.lifecycle.lock();
        let _lifecycle = tokio::select! {
            guard = lifecycle => guard,
            () = request.cancellation().cancelled() => {
                return Err(NativeCodexAuthError::Transport(None));
            }
        };
        self.validate_preflight_target(&request)?;
        // Official Codex refreshes the access JWT before every authenticated
        // catalog or Responses dispatch. Catalog fetch already refreshes; an
        // expired generation must not reach inference preflight as Unavailable.
        self.refresh_if_needed_locked(request.deadline(), Some(request.cancellation()))
            .await?;
        self.validate_preflight_target(&request)?;
        let (expected_fence, expected_selection_epoch) = request
            .target()
            .native_codex_credential_selection()
            .map(|(fence, epoch)| (fence.clone(), epoch))
            .ok_or(NativeCodexAuthError::Unavailable)?;
        let current = self.load_fence().await?;
        if current != expected_fence {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let lease = self.acquire_ready_lease(expected_fence.clone()).await?;
        let credential = parse_stored(lease.payload()).map_err(|error| match error {
            NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogCredential,
            other => other,
        })?;
        let stored_epoch = credential_selection_epoch(
            request.target().account_binding(),
            request.target().credential_authority_binding(),
            &credential.selection_epoch,
        )?;
        if lease.fence() != expected_fence
            || stored_epoch != expected_selection_epoch
            || credential.account_id != expected_fence.account_binding().as_str()
            || credential.access_expires_at <= now_unix_secs().saturating_add(ACCESS_REFRESH_SKEW)
            || validate_exact_scopes(&credential.scopes.join(" ")).is_err()
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        self.validate_preflight_target(&request)?;
        Ok(NativeCodexAuthorization {
            lease,
            expected_fence,
            expected_selection_epoch,
            credential,
            preflight: request,
            physical_request: None,
            headers_consumed: false,
        })
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexResponsesWireMode {
    Standard,
    Lite,
}

/// Credential-free exact catalog entry admitted by the pinned profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCatalogEntry {
    model_id: String,
    visibility: Option<CodexCatalogVisibility>,
    priority: i32,
    api_support: CodexCatalogApiSupport,
    text: bool,
    tools: bool,
    parallel_tools: bool,
    wire_mode: CodexResponsesWireMode,
    tool_mode: Option<CodexCatalogToolMode>,
    reasoning_effort_options: Vec<ReasoningEffort>,
    default_reasoning_effort: Option<ReasoningEffort>,
    reasoning_efforts: CodexReasoningEffortSet,
    service_tiers: Vec<CodexCatalogServiceTier>,
    default_service_tier: Option<String>,
    capacity_facts: CodexCatalogCapacityFacts,
    observed_at: Timestamp,
    expires_at: Timestamp,
}

impl CodexCatalogEntry {
    /// Returns the exact backend slug.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Returns the exact recognized picker visibility advertised by this row.
    /// Unknown future values are retained in identity but return `None`.
    #[must_use]
    pub const fn visibility(&self) -> Option<CodexCatalogVisibility> {
        self.visibility
    }

    /// Returns the provider-authored picker priority (lower is earlier).
    #[must_use]
    pub const fn priority(&self) -> i32 {
        self.priority
    }

    /// Reports API-key Platform eligibility. ChatGPT OAuth eligibility is
    /// independent, so a false value does not remove this row from Codex OAuth.
    #[must_use]
    pub const fn supported_in_api(&self) -> bool {
        matches!(self.api_support, CodexCatalogApiSupport::PlatformEligible)
    }

    /// Reports explicitly advertised text input.
    #[must_use]
    pub const fn supports_text(&self) -> bool {
        self.text
    }

    /// Reports tool support conservatively derived from a non-disabled shell type.
    #[must_use]
    pub const fn supports_tools(&self) -> bool {
        self.tools
    }

    /// Reports parallel tools when the pinned reference-client release enables
    /// them for a catalog row that independently advertises tools.
    #[must_use]
    pub const fn supports_parallel_tools(&self) -> bool {
        self.parallel_tools
    }

    /// Whether this authenticated row uses Codex Responses Lite.
    #[must_use]
    pub const fn uses_responses_lite(&self) -> bool {
        matches!(self.wire_mode, CodexResponsesWireMode::Lite)
    }

    /// Returns the exact recognized `tool_mode` advertised by this row.
    ///
    /// The pinned Codex 0.162.1 reference client defaults an omitted value to
    /// Direct. Unknown future values return `None`; callers must not infer a
    /// mode from the model slug, shell type, or neighboring rows.
    #[must_use]
    pub const fn tool_mode(&self) -> Option<CodexCatalogToolMode> {
        self.tool_mode
    }

    /// Reports exact authenticated Code Mode advertisement for this row.
    #[must_use]
    pub const fn supports_code_mode(&self) -> bool {
        matches!(
            self.tool_mode,
            Some(CodexCatalogToolMode::CodeMode | CodexCatalogToolMode::CodeModeOnly)
        )
    }

    /// Reports the coarse capability when at least one named neutral effort
    /// was advertised. Exact effort admission uses the full witnessed set.
    #[must_use]
    pub const fn supports_reasoning(&self) -> bool {
        self.reasoning_efforts.supports_named_reasoning()
    }

    /// Returns recognized portable efforts in authenticated catalog order.
    ///
    /// Provider-only `minimal`, unknown future labels, and token budgets are
    /// never projected into this portable option list.
    #[must_use]
    pub fn reasoning_effort_options(&self) -> &[ReasoningEffort] {
        &self.reasoning_effort_options
    }

    /// Returns the exact provider-authored default reasoning effort when the
    /// authenticated row supplies one understood by this pinned release.
    #[must_use]
    pub const fn default_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.default_reasoning_effort
    }

    /// Returns service tiers in exact authenticated provider order.
    #[must_use]
    pub fn service_tiers(&self) -> &[CodexCatalogServiceTier] {
        &self.service_tiers
    }

    /// Returns the exact authenticated default service-tier ID, if present.
    #[must_use]
    pub fn default_service_tier(&self) -> Option<&str> {
        self.default_service_tier.as_deref()
    }

    /// Reports whether this exact catalog row advertised the named effort.
    ///
    /// Provider-neutral token budgets and efforts unknown to this pinned
    /// integration fail closed. This query performs no credential or network
    /// access.
    #[must_use]
    pub const fn supports_reasoning_effort(&self, effort: ReasoningEffort) -> bool {
        self.reasoning_efforts.supports(effort)
    }

    /// Returns exact authenticated catalog capacity facts for this entry.
    ///
    /// These facts are discovery evidence only. They do not prove complete
    /// request measurement or an enforced output reservation and therefore do
    /// not constitute a neutral context-admission contract.
    #[must_use]
    pub const fn capacity_facts(&self) -> CodexCatalogCapacityFacts {
        self.capacity_facts
    }

    /// Returns the exact route-observation time.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the hard evidence expiry.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}

/// Exact provider-authored service-tier presentation from one Codex row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodexCatalogServiceTier {
    id: String,
    name: String,
    description: String,
}

impl CodexCatalogServiceTier {
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

/// Exact credential-free context and compaction thresholds from one catalog row.
///
/// Construction is private so every public value has passed the provider-wire
/// integer bounds and cross-field coherence checks. The value is intentionally
/// not a [`plakous_model::ContextContract`]: the catalog supplies no certified
/// counter for the complete prepared request and no enforced output maximum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CodexCatalogCapacityFacts {
    context_window: Option<NonZeroU32>,
    max_context_window: Option<NonZeroU32>,
    effective_context_window_percent: NonZeroU8,
    auto_compact_token_limit: Option<NonZeroU32>,
}

impl CodexCatalogCapacityFacts {
    fn from_wire(model: &CatalogModelWire) -> Result<Self, NativeCodexAuthError> {
        let context_window = model
            .context_window
            .map(positive_catalog_tokens)
            .transpose()?;
        let max_context_window = model
            .max_context_window
            .map(positive_catalog_tokens)
            .transpose()?;
        let resolved_context_window = context_window
            .or(max_context_window)
            .ok_or(NativeCodexAuthError::Catalog)?;
        let effective_context_window_percent = u8::try_from(model.effective_context_window_percent)
            .ok()
            .filter(|percent| (1..=100).contains(percent))
            .and_then(NonZeroU8::new)
            .ok_or(NativeCodexAuthError::Catalog)?;
        let auto_compact_token_limit = model
            .auto_compact_token_limit
            .map(positive_catalog_tokens)
            .transpose()?;
        if context_window
            .zip(max_context_window)
            .is_some_and(|(context, maximum)| context > maximum)
        {
            return Err(NativeCodexAuthError::Catalog);
        }
        let _ = resolved_context_window;
        Ok(Self {
            context_window,
            max_context_window,
            effective_context_window_percent,
            auto_compact_token_limit,
        })
    }

    /// Returns the exact catalog context window in tokens.
    #[must_use]
    pub const fn context_window(self) -> Option<NonZeroU32> {
        self.context_window
    }

    /// Returns the exact catalog maximum context-window override in tokens.
    #[must_use]
    pub const fn max_context_window(self) -> Option<NonZeroU32> {
        self.max_context_window
    }

    /// Returns Codex's operative context window (`context_window`, otherwise
    /// `max_context_window`). Admitted facts always contain at least one.
    #[must_use]
    pub const fn resolved_context_window(self) -> NonZeroU32 {
        match self.context_window {
            Some(window) => window,
            None => match self.max_context_window {
                Some(window) => window,
                None => unreachable!(),
            },
        }
    }

    /// Returns the exact catalog usable-input percentage.
    #[must_use]
    pub const fn effective_context_window_percent(self) -> NonZeroU8 {
        self.effective_context_window_percent
    }

    /// Returns the exact optional catalog auto-compaction threshold in tokens.
    #[must_use]
    pub const fn auto_compact_token_limit(self) -> Option<NonZeroU32> {
        self.auto_compact_token_limit
    }

    /// Returns Codex's effective 90%-derived, explicitly clamped threshold.
    #[must_use]
    pub fn effective_auto_compact_token_limit(self) -> u32 {
        let derived = self.resolved_context_window().get().saturating_mul(9) / 10;
        self.auto_compact_token_limit
            .map_or(derived, |explicit| explicit.get().min(derived))
    }

    fn provider_reserved_input(self) -> Result<u64, NativeCodexAuthError> {
        u64::from(self.resolved_context_window().get())
            .checked_mul(u64::from(self.effective_context_window_percent.get()))
            .map(|scaled| scaled / 100)
            .filter(|tokens| *tokens != 0)
            .ok_or(NativeCodexAuthError::Catalog)
    }
}

fn reference_client_envelope_context_contract(
    authority_binding: Digest,
    capacity_facts: CodexCatalogCapacityFacts,
    observed: Timestamp,
    expires: Timestamp,
    catalog_receipt: Digest,
) -> Result<ContextContract, NativeCodexAuthError> {
    let freshness = EvidenceFreshness::new(observed, Some(expires))
        .map_err(|_| NativeCodexAuthError::Catalog)?;
    let capacity = CapacityCertificate::reference_client_envelope(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "codex-authenticated-catalog-input-envelope",
        CODEX_REFERENCE_CAPACITY_REVISION,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(u64::from(
                capacity_facts.resolved_context_window().get(),
            )),
            maximum_projected_input: capacity_facts.provider_reserved_input()?,
            auto_compact_input: Some(u64::from(
                capacity_facts.effective_auto_compact_token_limit(),
            )),
        },
        authority_binding,
        freshness,
        catalog_receipt,
        OPENAI_CODEX_SOURCE_REVISION,
    )
    .map_err(|_| NativeCodexAuthError::Catalog)?;
    let measurement = MeasurementCertificate::standard(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "codex-complete-input-token-upper-bound",
        CODEX_REFERENCE_MEASUREMENT_REVISION,
        "ceil-utf8-wire-bytes-div-4-compaction-reasoning-length",
        CODEX_REFERENCE_MEASUREMENT_REVISION,
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt },
        Some(authority_binding),
        freshness,
    )
    .map_err(|_| NativeCodexAuthError::Catalog)?;
    ContextContract::new(
        ProtocolId::new(RESPONSES_FAMILY_ID).map_err(|_| NativeCodexAuthError::Catalog)?,
        RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
        RESPONSES_CANONICAL_PROJECTION_REVISION,
        "reference-client-envelope-no-output-authority-v1",
        capacity,
        vec![measurement],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

fn positive_catalog_tokens(value: i64) -> Result<NonZeroU32, NativeCodexAuthError> {
    u32::try_from(value)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or(NativeCodexAuthError::Catalog)
}

/// A fresh credential-free catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexModelCatalog {
    entries: Vec<CodexCatalogEntry>,
    profile_id: &'static str,
    source_revision: &'static str,
}

impl CodexModelCatalog {
    /// Returns catalog entries in backend order.
    #[must_use]
    pub fn entries(&self) -> &[CodexCatalogEntry] {
        &self.entries
    }

    /// Returns the immutable profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        self.profile_id
    }

    /// Returns the immutable official source revision.
    #[must_use]
    pub const fn source_revision(&self) -> &'static str {
        self.source_revision
    }
}

#[derive(Deserialize)]
struct CatalogDocument {
    models: Vec<Box<RawValue>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CatalogReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    /// Provider-published maximum reasoning tier.
    Max,
    /// Provider-published ultra reasoning tier.
    Ultra,
    /// Preserve forward compatibility for newly published efforts.
    Other(String),
}

impl CatalogReasoningEffort {
    fn label(&self) -> &str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::Other(label) => label,
        }
    }

    const fn portable(&self) -> Option<ReasoningEffort> {
        match self {
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::Xhigh => Some(ReasoningEffort::XHigh),
            Self::Max => Some(ReasoningEffort::Max),
            Self::Ultra => Some(ReasoningEffort::Ultra),
            Self::Minimal | Self::Other(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for CatalogReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            "ultra" => Self::Ultra,
            other => Self::Other(other.to_owned()),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct CodexReasoningEffortSet(u8);

impl CodexReasoningEffortSet {
    const MINIMAL: u8 = 1 << 0;
    const LOW: u8 = 1 << 1;
    const MEDIUM: u8 = 1 << 2;
    const HIGH: u8 = 1 << 3;
    const XHIGH: u8 = 1 << 4;
    const MAX: u8 = 1 << 5;
    const ULTRA: u8 = 1 << 6;
    const OTHER: u8 = 1 << 7;

    fn from_catalog(levels: &[CatalogReasoningLevel]) -> Self {
        Self(levels.iter().fold(0, |bits, level| {
            bits | match &level.effort {
                CatalogReasoningEffort::Minimal => Self::MINIMAL,
                CatalogReasoningEffort::Low => Self::LOW,
                CatalogReasoningEffort::Medium => Self::MEDIUM,
                CatalogReasoningEffort::High => Self::HIGH,
                CatalogReasoningEffort::Xhigh => Self::XHIGH,
                CatalogReasoningEffort::Max => Self::MAX,
                CatalogReasoningEffort::Ultra => Self::ULTRA,
                CatalogReasoningEffort::Other(_) => Self::OTHER,
            }
        }))
    }

    const fn supports(self, effort: ReasoningEffort) -> bool {
        let required = match effort {
            ReasoningEffort::Low => Self::LOW,
            ReasoningEffort::Medium => Self::MEDIUM,
            ReasoningEffort::High => Self::HIGH,
            ReasoningEffort::XHigh => Self::XHIGH,
            ReasoningEffort::Max => Self::MAX,
            ReasoningEffort::Ultra => Self::ULTRA,
            ReasoningEffort::TokenBudget(_) => return false,
            _ => return false,
        };
        self.0 & required != 0
    }

    const fn supports_named_reasoning(self) -> bool {
        self.0 & (Self::LOW | Self::MEDIUM | Self::HIGH | Self::XHIGH | Self::MAX | Self::ULTRA)
            != 0
    }

    const fn identity_byte(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct CatalogReasoningLevel {
    effort: CatalogReasoningEffort,
    /// Provider-facing description text; ignored by Plak capability admission.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CatalogShellType {
    Default,
    Disabled,
    Local,
    UnifiedExec,
    ShellCommand,
    /// Preserve unrelated future catalog rows without assigning capability.
    Other(String),
}

impl<'de> Deserialize<'de> for CatalogShellType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "default" => Self::Default,
            "disabled" => Self::Disabled,
            "local" => Self::Local,
            "unified_exec" => Self::UnifiedExec,
            "shell_command" => Self::ShellCommand,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogShellType {
    fn label(&self) -> &str {
        match self {
            Self::Default => "default",
            Self::Disabled => "disabled",
            Self::Local => "local",
            Self::UnifiedExec => "unified_exec",
            Self::ShellCommand => "shell_command",
            Self::Other(label) => label,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CatalogInputModality {
    Text,
    Image,
    Audio,
    /// Preserve unrelated future catalog rows without assigning capability.
    Other(String),
}

/// Closed authenticated Codex catalog tool-mode facts understood by this
/// integration revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCatalogToolMode {
    /// The provider's `direct` catalog value.
    Direct,
    /// The provider's `code_mode` catalog value.
    CodeMode,
    /// The provider's `code_mode_only` catalog value.
    CodeModeOnly,
}

/// Closed authenticated Codex picker-visibility facts understood by this
/// integration revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexCatalogVisibility {
    /// Present in an ordinary model picker.
    List,
    /// Retained for exact internal selection but hidden from the picker.
    Hide,
    /// Not presented through picker or public model-list surfaces.
    None,
}

/// Route-scoped meaning of Codex's `supported_in_api` catalog flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCatalogApiSupport {
    /// The row may also be presented on the separately authenticated API route.
    PlatformEligible,
    /// The row remains valid for ChatGPT OAuth but is not an API-key row.
    SubscriptionOnly,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum CatalogVisibility {
    List,
    Hide,
    None,
    #[default]
    Missing,
    Other(String),
}

impl<'de> Deserialize<'de> for CatalogVisibility {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "list" => Self::List,
            "hide" => Self::Hide,
            "none" => Self::None,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogVisibility {
    fn label(&self) -> &str {
        match self {
            Self::List => "list",
            Self::Hide => "hide",
            Self::None => "none",
            Self::Missing => "<missing>",
            Self::Other(label) => label,
        }
    }

    const fn recognized(&self) -> Option<CodexCatalogVisibility> {
        match self {
            Self::List => Some(CodexCatalogVisibility::List),
            Self::Hide => Some(CodexCatalogVisibility::Hide),
            Self::None => Some(CodexCatalogVisibility::None),
            Self::Missing | Self::Other(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum CatalogToolMode {
    Direct,
    CodeMode,
    CodeModeOnly,
    #[default]
    Missing,
    Other(String),
}

impl<'de> Deserialize<'de> for CatalogToolMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let Some(value) = Option::<String>::deserialize(deserializer)? else {
            return Ok(Self::Missing);
        };
        Ok(match value.as_str() {
            "direct" => Self::Direct,
            "code_mode" => Self::CodeMode,
            "code_mode_only" => Self::CodeModeOnly,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogToolMode {
    fn label(&self) -> &str {
        match self {
            Self::Direct => "direct",
            Self::CodeMode => "code_mode",
            Self::CodeModeOnly => "code_mode_only",
            Self::Missing => "<missing>",
            Self::Other(label) => label,
        }
    }

    const fn recognized(&self) -> Option<CodexCatalogToolMode> {
        match self {
            Self::Direct => Some(CodexCatalogToolMode::Direct),
            Self::CodeMode => Some(CodexCatalogToolMode::CodeMode),
            Self::CodeModeOnly => Some(CodexCatalogToolMode::CodeModeOnly),
            // Exact Codex 0.162.1 reference-client semantics.
            Self::Missing => Some(CodexCatalogToolMode::Direct),
            Self::Other(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for CatalogInputModality {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "text" => Self::Text,
            "image" => Self::Image,
            "audio" => Self::Audio,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogInputModality {
    fn label(&self) -> &str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Other(label) => label,
        }
    }
}

fn default_catalog_input_modalities() -> Vec<CatalogInputModality> {
    // This is the provider's documented legacy wire default, not a Plakous
    // capability guess.
    vec![CatalogInputModality::Text, CatalogInputModality::Image]
}

const fn default_catalog_effective_context_window_percent() -> i64 {
    // Kept in lock-step with the official Codex protocol default.
    95
}

#[derive(Deserialize)]
struct CatalogModelWire {
    slug: String,
    visibility: CatalogVisibility,
    priority: i32,
    supported_in_api: bool,
    supported_reasoning_levels: Vec<CatalogReasoningLevel>,
    #[serde(default)]
    default_reasoning_level: Option<CatalogReasoningEffort>,
    #[serde(default)]
    service_tiers: Vec<CatalogServiceTierWire>,
    #[serde(default)]
    default_service_tier: Option<String>,
    shell_type: CatalogShellType,
    #[serde(default)]
    tool_mode: CatalogToolMode,
    #[serde(default = "default_catalog_input_modalities")]
    input_modalities: Vec<CatalogInputModality>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
    #[serde(default = "default_catalog_effective_context_window_percent")]
    effective_context_window_percent: i64,
    #[serde(default)]
    auto_compact_token_limit: Option<i64>,
    #[serde(default)]
    use_responses_lite: bool,
    #[serde(default)]
    model_messages: Option<CatalogModelMessagesWire>,
}

#[derive(Deserialize)]
struct CatalogModelMessagesWire {
    #[serde(default)]
    content_filter_guidance: Option<String>,
}

fn catalog_content_filter_guidance(model: &CatalogModelWire) -> Option<&str> {
    model
        .model_messages
        .as_ref()?
        .content_filter_guidance
        .as_deref()
        .filter(|text| !text.trim().is_empty() && text.len() <= 512)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CatalogServiceTierWire {
    id: String,
    name: String,
    description: String,
}

fn catalog_service_tiers(
    model: &CatalogModelWire,
) -> Result<(Vec<CodexCatalogServiceTier>, Option<String>), NativeCodexAuthError> {
    let mut ids = BTreeSet::new();
    let mut tiers = Vec::with_capacity(model.service_tiers.len());
    for tier in &model.service_tiers {
        validate_identifier(&tier.id, MAX_SERVICE_TIER_ID_BYTES)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        validate_oauth_value(&tier.name, MAX_SERVICE_TIER_NAME_BYTES)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        validate_oauth_value(&tier.description, MAX_SERVICE_TIER_DESCRIPTION_BYTES)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        if tier.id != "priority" || !ids.insert(tier.id.as_str()) {
            return Err(NativeCodexAuthError::Catalog);
        }
        tiers.push(CodexCatalogServiceTier {
            id: tier.id.clone(),
            name: tier.name.clone(),
            description: tier.description.clone(),
        });
    }
    if model
        .default_service_tier
        .as_ref()
        .is_some_and(|default| !ids.contains(default.as_str()))
    {
        return Err(NativeCodexAuthError::Catalog);
    }
    Ok((tiers, model.default_service_tier.clone()))
}

fn catalog_enum_fingerprint(model: &CatalogModelWire) -> Result<Digest, NativeCodexAuthError> {
    #[derive(Serialize)]
    struct EnumLabels<'a> {
        visibility: &'a str,
        priority: i32,
        supported_in_api: bool,
        shell_type: &'a str,
        input_modalities: Vec<&'a str>,
        supported_reasoning_levels: Vec<&'a str>,
        default_reasoning_level: Option<&'a str>,
        service_tiers: &'a [CatalogServiceTierWire],
        default_service_tier: Option<&'a str>,
        tool_mode: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_filter_guidance: Option<&'a str>,
    }
    let bytes = serde_json::to_vec(&EnumLabels {
        visibility: model.visibility.label(),
        priority: model.priority,
        supported_in_api: model.supported_in_api,
        shell_type: model.shell_type.label(),
        input_modalities: model
            .input_modalities
            .iter()
            .map(CatalogInputModality::label)
            .collect(),
        supported_reasoning_levels: model
            .supported_reasoning_levels
            .iter()
            .map(|level| level.effort.label())
            .collect(),
        default_reasoning_level: model
            .default_reasoning_level
            .as_ref()
            .map(CatalogReasoningEffort::label),
        service_tiers: &model.service_tiers,
        default_service_tier: model.default_service_tier.as_deref(),
        tool_mode: model.tool_mode.label(),
        content_filter_guidance: catalog_content_filter_guidance(model),
    })
    .map_err(|_| NativeCodexAuthError::Catalog)?;
    Digest::sha256(
        DigestDomain::ModelBinding,
        4,
        &[b"plak-native-codex-catalog-row-facts-v4", &bytes],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

#[derive(Clone)]
struct WitnessedDescriptor {
    descriptor: ResolvedModelDescriptor,
    witness: NativeCodexCatalogWitness,
    capacity_facts: CodexCatalogCapacityFacts,
    reasoning_efforts: CodexReasoningEffortSet,
    tool_mode: Option<CodexCatalogToolMode>,
    responses_lite: bool,
    content_filter_guidance: Option<String>,
    catalog_enum_fingerprint: Digest,
    catalog_receipt: Digest,
    observed_at: Timestamp,
    expires_at: Timestamp,
}

struct ResolvedCatalog {
    public: CodexModelCatalog,
    descriptors: BTreeMap<String, WitnessedDescriptor>,
    ambiguous_slugs: BTreeSet<String>,
    credential_fence: CredentialFence,
}

/// Exact Codex 0.162.1 model rows accepted for executable catalog intersection.
///
/// This list is bound to the reviewed reference-client/catalog contract.
/// Authenticated catalog observation remains independently required, including for server-delivered rows that
/// are not baked into the reference client's fallback catalog.
///
/// Each row pairs the literal with its source-backed tool mode. It is the only
/// place a Codex model is enrolled; presentation, Code Mode eligibility and
/// catalog admission all read it. Window and effort facts come from the
/// authenticated catalog, never from here.
pub const RELEASE_ENROLLED_CODEX_MODELS: &[(&str, CodexCatalogToolMode)] = &[
    ("gpt-6-astra", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-6.1-sol", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-6-sol", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-6-luna", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-5.6-sol", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-5.6-terra", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-5.6-luna", CodexCatalogToolMode::CodeModeOnly),
];

/// Returns whether an exact model literal is enrolled by the reviewed release.
#[must_use]
pub fn release_enrolled_codex_model(model: &str) -> bool {
    release_enrolled_codex_tool_mode(model).is_some()
}

/// Returns the exact source-backed tool mode for an enrolled model literal.
#[must_use]
pub fn release_enrolled_codex_tool_mode(model: &str) -> Option<CodexCatalogToolMode> {
    RELEASE_ENROLLED_CODEX_MODELS
        .iter()
        .find_map(|(enrolled, mode)| (*enrolled == model).then_some(*mode))
}

fn executable_codex_catalog_model(model: &str) -> bool {
    release_enrolled_codex_model(model)
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
        let model: Arc<dyn LanguageModel> = Arc::new(model);
        Ok((model, evidence))
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
