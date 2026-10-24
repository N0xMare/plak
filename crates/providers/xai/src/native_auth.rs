//! Plakous-owned xAI OIDC authentication and selected-slot credential authority.

mod catalog;
mod dispatch;
mod error;
mod evidence;
mod login;
mod manager;
mod manager_core;
mod profile;
mod refresh;
mod route_authority;
mod verify;

pub use error::XaiNativeAuthError;
use error::{AuthTransportError, LoginHttpStage};
pub use evidence::XaiModelResolutionEvidence;
pub use manager::XaiNativeAuthManager;
use manager_core::CatalogRenewal;
pub use profile::{
    XaiCompatibilityAcceptance, XaiCompatibilityProvenance, XaiOfflineCredentialStatus,
    XaiOfflineSelectedSlot, XaiSubscriptionIntegrationProfile, XaiSubscriptionStatus,
    is_subscription_coding_model_id, subscription_model_reasoning_effort_options,
    subscription_model_supports_reasoning,
};
use route_authority::native_route_authority;

#[cfg(test)]
use catalog::ProxyCatalogModel;
use catalog::{CatalogResponse, valid_admitted_catalog_entries};
pub use catalog::{XaiCatalogDiagnosticFacts, XaiNativeCatalogEntry, XaiNativeCatalogReceipt};
use login::BrowserLoginFlow;
#[cfg(test)]
use login::{
    BrowserCallback, oauth_callback_to_browser, parse_callback_request, xai_callback_policy,
};
pub use login::{XaiBrowserLogin, XaiDeviceLogin, XaiRemoteBrowserLogin};
#[cfg(test)]
use refresh::xai_refresh_failure_is_retryable;
use refresh::{XaiRefreshCancelGuard, XaiRefreshHooks, XaiRevokeHooks};

#[cfg(test)]
use dispatch::supergrok_generation_fingerprint;
pub(crate) use dispatch::{
    SuperGrokDispatchGuard, SuperGrokResponsesExecution, revalidate_supergrok_responses,
    supergrok_credential_rejected, supergrok_responses_preflight, supergrok_side_request,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use futures_util::lock::Mutex as AsyncMutex;
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use plakous_auth::oauth_orchestrator::{
    BrowserLoopbackHooks, BuildRequestFailure, ClaimedRefreshHooks, ClaimedRevokeHooks,
    CredentialRefreshDetail, CredentialRefreshEvent, CredentialRefreshFailureClass,
    CredentialRefreshFailureReason, CredentialRefreshObserver, CredentialRefreshPhase, FetchedJwks,
    JwksCache, JwksResolvePolicy, LoopbackConnectionEvent, PullBodyError, bind_loopback_listener,
    pull_claimed_body, run_browser_loopback_session, run_claimed_refresh, run_claimed_revoke,
    until_deadline,
};
use plakous_auth::oauth_workflow::{
    AuthorizeUrlRequest, CallbackQueryPolicy, DevicePollSchedule, LoopbackCallbackOutcome,
    LoopbackCallbackPage, OAuthCallbackOutcome, OAuthWorkflowError, access_needs_refresh,
    authorization_code_token_form, build_authorization_url, decode_json_bounded,
    is_http_token_byte, loopback_redirect_authority, parse_http11_get_request,
    parse_loopback_callback_target, parse_remote_authorization_input, refresh_token_form,
    remaining_until, unsigned_access_token_exp, urlencoded_form,
};
use plakous_auth::subscription_limit::{
    SubscriptionRequestClass, SubscriptionRequestGate, SubscriptionRequestPermit,
};
use plakous_auth::{
    AccountBinding, CallbackBinding, ClaimedHttpDisposition, ClaimedHttpError, ClaimedHttpResponse,
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    OfflineCredentialLifecycle, PkceS256, PrivateCredentialStore, PrivateStoreError,
    ReadyCredentialLease, RecordAge, RefreshClaim, SecretBytes, SecretString,
    generate_login_selection_epoch, valid_login_selection_epoch,
};
use plakous_exchange::network_failure;
use plakous_http::{
    Bytes, Client, ExchangeControl, Failure, HeaderMap, HeaderName, HeaderValue, Headers, Method,
    Origin, Request, SensitiveBytes, StatusCode, sensitive_header_value,
};
#[cfg(test)]
use plakous_model::DigestDomain;
use plakous_model::{
    Digest, LanguageModel, ModelFamilyId, ModelId, ProviderConfigurationError,
    ProviderSemanticEvidence, ReasoningEffort, ResolvedModelDescriptor, RuntimeAdmissionWitness,
    RuntimeModelAuthorityError, SubscriptionUsageCredentialState, SubscriptionUsageRead,
    SubscriptionUsageUnavailable, Timestamp,
};
use plakous_responses_dialect::{
    CredentialGenerationFingerprint, ResponsesAuthSlot, ResponsesCredentialAuthority,
    ResponsesDispatchRejection, ResponsesHeaderBinding, ResponsesPreflightFuture,
    ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest, ResponsesRevalidationRequest,
};
use serde::de::{self, DeserializeOwned, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::net::TcpListener;
use url::Url;
use zeroize::{Zeroize as _, Zeroizing};

use crate::oauth_pins::{XAI_OAUTH, XAI_PROFILE};
use crate::{
    GrokCodingProfile, StandardXaiHttpConfig, XAI_RESPONSES_ADAPTER_REVISION, XaiDialect,
    XaiGrokSubscriptionRoute, XaiLanguageModel, XaiModel, XaiResolvedModel,
};

/// Exact immutable compatibility profile accepted by this implementation.
pub const XAI_GROK_COMPATIBILITY_PROFILE: &str = XAI_PROFILE.profile_id;
/// Immutable upstream source revision reviewed for public-client compatibility.
pub(crate) const XAI_OPENCODE_SOURCE_REVISION: &str = XAI_PROFILE.source_revision;
/// Pinned xAI public OAuth client identifier.
pub(crate) const XAI_PUBLIC_CLIENT_ID: &str = XAI_OAUTH.client_id;
/// Fixed loopback redirect registered for the pinned public client.
pub(crate) const XAI_LOOPBACK_REDIRECT: &str = XAI_OAUTH.loopback_redirect_uri;
/// Exact requested scope set, in its pinned wire order.
pub(crate) const XAI_OAUTH_SCOPE: &str = XAI_OAUTH.scopes;
/// Exact operator notice revision for this experimental compatibility profile.
pub(crate) const XAI_GROK_COMPATIBILITY_NOTICE_REVISION: &str = XAI_PROFILE.notice_revision;
/// Locally reviewable kill-switch revision bound into exact acceptance.
pub(crate) const XAI_GROK_KILL_SWITCH_REVISION: &str = XAI_PROFILE.kill_switch_revision;
/// Historical SuperGrok coding model still admitted when present in catalog.
pub const XAI_GROK_BUILD_MODEL: &str = "grok-build-0.1";
/// Primary SuperGrok agentic coding model for ordinary smoke and harness work.
pub const XAI_GROK_PRIMARY_CODING_MODEL: &str = "grok-4.5";
/// Reviewed Grok 4.6 subscription model published by Grok Build.
pub const XAI_GROK_46_CODING_MODEL: &str = "grok-4.6";
pub(crate) const XAI_GROK_47_CODING_MODEL: &str = "grok-4.7";
/// Exact Grok 4.7 fast catalog id. Grok Build 1.0.41 lists this as its own
/// model, not as a service tier and not as an alias of `grok-4.7`.
pub(crate) const XAI_GROK_47_FAST_CODING_MODEL: &str = "grok-4.7-build-fast";
/// SuperGrok answers `grok-4.7` requests with this served id (live captures,
/// 2026-10-08). It is not an enrolled or selectable model.
const XAI_GROK_47_SERVED_MODEL: &str = "grok-4.7-build";
pub(crate) const XAI_GROK_47_ENROLLMENT_REVISION: &str = "grok-4.7-model-enrollment.2026-09-21.r1";
pub(crate) const XAI_GROK_47_FAST_ENROLLMENT_REVISION: &str =
    "grok-4.7-build-fast-enrollment.2026-09-26.r1";

/// One enrolled SuperGrok coding model. This table is the only place a
/// Grok subscription model's facts live: admission, efforts, catalog
/// evidence and the context contract all read it. The context window is not
/// here; it is always the authenticated catalog's own value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GrokEnrollment {
    pub(crate) model: &'static str,
    pub(crate) efforts: &'static [ReasoningEffort],
    /// Model-catalog evidence revision bound into the resolution receipt.
    pub(crate) catalog_source_revision: &'static str,
    pub(crate) capacity_evidence_id: &'static str,
    pub(crate) reference_revision: &'static str,
    /// Model-specific compact trigger, as a percent of the catalog window.
    /// `None` inherits the runtime's generic fence.
    pub(crate) auto_compact_percent: Option<u64>,
    /// The model's own per-response output ceiling, reasoning included. The
    /// catalog reports none, so this is xAI's published Grok 4 ceiling, the
    /// same figure the API route enforces.
    pub(crate) max_output_tokens: u32,
    /// Exact served-model ids the subscription reports for this same model.
    /// Their usage anchors context admission; they stay unselectable labels.
    pub(crate) same_served_models: &'static [&'static str],
}

const GROK_45_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const GROK_46_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
/// Grok Build source `5163763e` pins an 80% compact trigger for 4.5 and 4.6
/// rather than inheriting the configurable resolver's final 85% fallback.
const GROK_BUILD_COMPACT_PERCENT: u64 = 80;
/// Grok Build source revision whose effective 4.5/4.6 rows are reproduced.
pub(crate) const GROK_REFERENCE_REVISION: &str =
    "grok-build-5163763e703c319e4554c2f455535c5adb6e51e8";

pub(crate) const GROK_ENROLLMENTS: [GrokEnrollment; 4] = [
    GrokEnrollment {
        model: XAI_GROK_47_FAST_CODING_MODEL,
        efforts: GROK_46_EFFORTS,
        catalog_source_revision: XAI_GROK_47_FAST_ENROLLMENT_REVISION,
        capacity_evidence_id: "xai-grok-4.7-build-fast-subscription-capacity",
        reference_revision: XAI_GROK_47_FAST_ENROLLMENT_REVISION,
        auto_compact_percent: None,
        max_output_tokens: crate::XAI_PLATFORM_MAX_OUTPUT_TOKENS,
        same_served_models: &[],
    },
    GrokEnrollment {
        model: XAI_GROK_47_CODING_MODEL,
        efforts: GROK_46_EFFORTS,
        catalog_source_revision: XAI_GROK_47_ENROLLMENT_REVISION,
        capacity_evidence_id: "xai-grok-4.7-subscription-capacity",
        reference_revision: XAI_GROK_47_ENROLLMENT_REVISION,
        auto_compact_percent: None,
        max_output_tokens: crate::XAI_PLATFORM_MAX_OUTPUT_TOKENS,
        same_served_models: &[XAI_GROK_47_SERVED_MODEL],
    },
    GrokEnrollment {
        model: XAI_GROK_46_CODING_MODEL,
        efforts: GROK_46_EFFORTS,
        catalog_source_revision: XAI_GROK_MODEL_CATALOG_SOURCE_REVISION,
        capacity_evidence_id: "xai-grok-4.6-subscription-capacity",
        reference_revision: GROK_REFERENCE_REVISION,
        auto_compact_percent: Some(GROK_BUILD_COMPACT_PERCENT),
        max_output_tokens: crate::XAI_PLATFORM_MAX_OUTPUT_TOKENS,
        same_served_models: &[],
    },
    GrokEnrollment {
        model: XAI_GROK_PRIMARY_CODING_MODEL,
        efforts: GROK_45_EFFORTS,
        catalog_source_revision: XAI_GROK_MODEL_CATALOG_SOURCE_REVISION,
        capacity_evidence_id: "xai-grok-4.5-subscription-capacity",
        reference_revision: GROK_REFERENCE_REVISION,
        auto_compact_percent: Some(GROK_BUILD_COMPACT_PERCENT),
        max_output_tokens: crate::XAI_PLATFORM_MAX_OUTPUT_TOKENS,
        same_served_models: &[],
    },
];

/// The enrollment row for one exact literal model id; aliases never match.
pub(crate) fn grok_enrollment(model: &str) -> Option<&'static GrokEnrollment> {
    GROK_ENROLLMENTS.iter().find(|row| row.model == model)
}

pub(crate) fn model_catalog_source_revision(model: &str) -> &'static str {
    grok_enrollment(model).map_or(XAI_GROK_MODEL_CATALOG_SOURCE_REVISION, |row| {
        row.catalog_source_revision
    })
}

/// The enrolled model's own per-response output ceiling, reasoning included.
/// `None` for an id outside the enrollment.
#[must_use]
pub fn grok_max_output_tokens(model: &str) -> Option<u32> {
    grok_enrollment(model).map(|row| row.max_output_tokens)
}

/// Every enrolled SuperGrok subscription coding model id, in table order.
pub fn subscription_coding_model_ids() -> impl Iterator<Item = &'static str> {
    GROK_ENROLLMENTS.iter().map(|row| row.model)
}

/// Immutable `Grok Build` source revision reviewed for OAuth compatibility.
///
/// This remains part of credential/route compatibility authority. Model-row
/// evidence advances independently through `XAI_GROK_MODEL_CATALOG_SOURCE_REVISION`.
pub const XAI_GROK_BUILD_SOURCE_REVISION: &str = "afbc0fb710320c7add294c2106d447ecc3e3af2e";
/// Current immutable Grok Build model-catalog evidence revision.
///
/// Model enrollment advances independently from OAuth compatibility so adding
/// a reviewed model cannot invalidate an otherwise unchanged credential.
pub(crate) const XAI_GROK_MODEL_CATALOG_SOURCE_REVISION: &str =
    "5163763e703c319e4554c2f455535c5adb6e51e8";
/// Mutable official documentation locator for local function calling.
///
/// This URL is diagnostic context only. It is never capability authority;
/// pinned source revisions, the fixed profile, and the authenticated exact-ID
/// catalog receipt own that decision.
pub(crate) const XAI_FUNCTION_CALLING_DOCUMENTATION_LOCATOR: &str =
    "https://docs.x.ai/docs/guides/function-calling";
/// Mutable official documentation locator for the Grok Build model page.
///
/// This URL is diagnostic context only and is not immutable model evidence.
pub(crate) const XAI_GROK_BUILD_MODEL_DOCUMENTATION_LOCATOR: &str =
    "https://docs.x.ai/docs/models/grok-build";

const XAI_ISSUER: &str = XAI_OAUTH.auth_origin;
const AUTHORIZATION_PATH: &str = XAI_OAUTH.authorize_path;
const DEVICE_PATH: &str = XAI_OAUTH.device_authorization_path;
const TOKEN_PATH: &str = XAI_OAUTH.token_path;
const USERINFO_PATH: &str = XAI_OAUTH.userinfo_path;
const REVOCATION_PATH: &str = XAI_OAUTH.revoke_path;
const JWKS_PATH: &str = XAI_OAUTH.jwks_path;
const CATALOG_PATH: &str = crate::responses_dialect::SUPERGROK_MODELS_PATH;
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const USER_AGENT: &str = concat!("plak/", env!("CARGO_PKG_VERSION"));
const COMPATIBILITY_REVIEWED_UNIX: u64 = XAI_PROFILE.reviewed_at_unix;
const COMPATIBILITY_EXPIRES_UNIX: u64 = XAI_PROFILE.expires_at_unix;
const COMPATIBILITY_KILLED: bool = XAI_PROFILE.killed;
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(not(test))]
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
#[cfg(test)]
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
// Native `grok` parity (`GROK_AUTH_EARLY_INVALIDATION_SECS` = 300). A 6 h
// access token then refreshes once per ~5.9 h instead of ~5.6 h, and the
// pre-expiry window in which a peer process can observe an in-flight claim
// shrinks accordingly.
const ACCESS_REFRESH_LEAD_SECONDS: u64 = 300;
const TOKEN_CLOCK_LEEWAY_SECONDS: u64 = 60;
// Pi and OpenCode both tolerate an omitted xAI `expires_in`; use the same
// conservative provider-default lifetime instead of rejecting usable tokens.
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3_600;
const MAX_AUTH_BODY_BYTES: usize = 256 * 1024;
const MAX_TOKEN_BYTES: usize = 128 * 1024;
const MAX_CALLBACK_BYTES: usize = 8 * 1024;
const MAX_LOOPBACK_CONNECTIONS: usize = 4;
const MAX_STRING_BYTES: usize = 64 * 1024;
const MAX_JWKS_KEYS: usize = 16;
/// Stored token payload version. Version 2 carried a hex login selection
/// epoch; it is refused as retired and needs a new login.
const STORED_TOKENS_VERSION: u32 = 3;
const MAX_CATALOG_MODELS: usize = 256;
// Catalog evidence is bound into the descriptor and rechecked at every admit.
// A 30-minute fence (e2e `plak run` headroom) expired in the middle of a TUI
// session: idle `/compact` and the next turn then failed closed with stale
// runtime authority / EvidenceExpired. Keep the receipt for the SuperGrok
// access-token window so a coding session can compact and continue.
const CATALOG_TTL_SECONDS: u64 = 6 * 60 * 60;
const MAX_DEVICE_INTERVAL_SECONDS: u64 = 60;
const MAX_DEVICE_EXPIRES_SECONDS: u64 = 900;
#[cfg(not(test))]
const DEVICE_SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
#[cfg(test)]
const DEVICE_SLOW_DOWN_INCREMENT: Duration = Duration::from_millis(5);
const JWKS_TTL: Duration = Duration::from_secs(600);
const SUBSCRIPTION_INPUT_TOKEN_OVERHEAD: u64 = 512;

const HEADER_ACCEPT: HeaderName = HeaderName::from_static("accept");
const HEADER_AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const HEADER_CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const HEADER_USER_AGENT: HeaderName = HeaderName::from_static("user-agent");
const HEADER_XAI_TOKEN_AUTH: HeaderName = HeaderName::from_static("x-xai-token-auth");
const HEADER_AUTHENTICATE_RESPONSE: HeaderName = HeaderName::from_static("x-authenticateresponse");
const HEADER_GROK_CLIENT_VERSION: HeaderName = HeaderName::from_static("x-grok-client-version");
const HEADER_GROK_CLIENT_IDENTIFIER: HeaderName =
    HeaderName::from_static("x-grok-client-identifier");
const HEADER_GROK_CLIENT_MODE: HeaderName = HeaderName::from_static("x-grok-client-mode");
const HEADER_GROK_CONV_ID: HeaderName = HeaderName::from_static("x-grok-conv-id");
const HEADER_X_USER_ID: HeaderName = HeaderName::from_static("x-userid");
const XAI_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
const AUTHENTICATE_RESPONSE_VALUE: &str = "authenticate-response";
// Grok Build 1.0.46 (2765805b9442) derives this
// version-gate header from its CLI version. It is not a fixed protocol
// version. Product identity stays separate: Plakous does not claim grok-shell.
const SUPERGROK_CLIENT_COMPATIBILITY_VERSION: &str = "1.0.46";
const PLAKOUS_XAI_CLIENT_IDENTIFIER: &str = "plakous";
// Plakous runs a continuing agent session rather than Grok Build's one-shot
// `-p` path. This is the corresponding pinned client-mode value.
const PLAKOUS_XAI_CLIENT_MODE: &str = "interactive";
pub(crate) const GROK_PROXY_CATALOG_DECODER_REVISION: &str = "grok-build-afbc0fb-proxy-catalog-v1";
pub(crate) const GROK_PROXY_CATALOG_HEADER_PROFILE_REVISION: &str =
    "grok-build-2765805-catalog-headers-v4";
pub(crate) const GROK_PROXY_INFERENCE_HEADER_PROFILE_REVISION: &str =
    "grok-build-2765805-inference-headers-v5";

#[derive(Clone, Eq, PartialEq)]
struct CatalogReceiptCore {
    public: XaiNativeCatalogReceipt,
    fence: CredentialFence,
    selection_epoch: String,
    authority_id: String,
}

#[cfg(test)]
#[derive(Clone)]
struct ResolutionPause {
    reached: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

struct ManagerCore {
    catalog_retries: bool,
    store: PrivateCredentialStore,
    client: Client,
    catalog_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    acceptance: XaiCompatibilityAcceptance,
    endpoints: AuthEndpoints,
    compatibility_expires_unix: AtomicU64,
    jwks: JwksCache<Jwk>,
    refresh_flight: AsyncMutex<()>,
    catalog: Mutex<Option<CatalogReceiptCore>>,
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    refresh_observer: Option<Arc<dyn CredentialRefreshObserver>>,
    #[cfg(test)]
    resolution_pause: Mutex<Option<ResolutionPause>>,
}

fn headers_are_json(headers: &Headers) -> bool {
    let mut values = headers
        .iter()
        .filter(|(name, _)| **name == HEADER_CONTENT_TYPE)
        .map(|(_, value)| value);
    values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
        && values.next().is_none()
}

fn claimed_response_is_json(response: &ClaimedHttpResponse) -> bool {
    headers_are_json(response.headers())
}

fn login_token_exchange_or(login: bool, fallback: XaiNativeAuthError) -> XaiNativeAuthError {
    if login {
        XaiNativeAuthError::LoginTokenExchange
    } else {
        fallback
    }
}

fn login_signing_keys_or(login: bool, fallback: XaiNativeAuthError) -> XaiNativeAuthError {
    if login {
        XaiNativeAuthError::LoginSigningKeys
    } else {
        fallback
    }
}

fn login_account_verification_or(login: bool, fallback: XaiNativeAuthError) -> XaiNativeAuthError {
    if login {
        XaiNativeAuthError::LoginAccountVerification
    } else {
        fallback
    }
}

#[derive(Clone)]
struct Endpoint {
    origin: String,
    path: String,
}

#[derive(Clone)]
struct AuthEndpoints {
    authorization: Endpoint,
    device: Endpoint,
    token: Endpoint,
    userinfo: Endpoint,
    revocation: Endpoint,
    jwks: Endpoint,
    catalog: Endpoint,
    usage: Endpoint,
    /// Browser callback listener candidates. Production binds exactly the
    /// pinned redirect port; tests bind an ephemeral port (`[0]`).
    loopback_ports: &'static [u16],
}

impl AuthEndpoints {
    fn production() -> Self {
        let endpoint = |path: &str| Endpoint {
            origin: XAI_ISSUER.to_owned(),
            path: path.to_owned(),
        };
        Self {
            authorization: endpoint(AUTHORIZATION_PATH),
            device: endpoint(DEVICE_PATH),
            token: endpoint(TOKEN_PATH),
            userinfo: endpoint(USERINFO_PATH),
            revocation: endpoint(REVOCATION_PATH),
            jwks: endpoint(JWKS_PATH),
            catalog: Endpoint {
                origin: crate::SUPERGROK_RESPONSES_ORIGIN.to_owned(),
                path: CATALOG_PATH.to_owned(),
            },
            usage: Endpoint {
                origin: crate::SUPERGROK_RESPONSES_ORIGIN.to_owned(),
                path: crate::subscription_usage::GROK_SUBSCRIPTION_USAGE_PATH.to_owned(),
            },
            loopback_ports: XAI_OAUTH.loopback_ports,
        }
    }

    #[cfg(test)]
    fn test(origin: &str) -> Self {
        let endpoint = |path: &str| Endpoint {
            origin: origin.to_owned(),
            path: path.to_owned(),
        };
        Self {
            authorization: endpoint(AUTHORIZATION_PATH),
            device: endpoint(DEVICE_PATH),
            token: endpoint(TOKEN_PATH),
            userinfo: endpoint(USERINFO_PATH),
            revocation: endpoint(REVOCATION_PATH),
            jwks: endpoint(JWKS_PATH),
            catalog: endpoint(CATALOG_PATH),
            usage: endpoint(crate::subscription_usage::GROK_SUBSCRIPTION_USAGE_PATH),
            loopback_ports: &[0],
        }
    }

    const fn authorize(&self) -> &Endpoint {
        &self.authorization
    }
    const fn device(&self) -> &Endpoint {
        &self.device
    }
    const fn token(&self) -> &Endpoint {
        &self.token
    }
    const fn userinfo(&self) -> &Endpoint {
        &self.userinfo
    }
    const fn revocation(&self) -> &Endpoint {
        &self.revocation
    }
    const fn jwks(&self) -> &Endpoint {
        &self.jwks
    }
    const fn catalog(&self) -> &Endpoint {
        &self.catalog
    }
    const fn usage(&self) -> &Endpoint {
        &self.usage
    }
}

struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn validate(&self) -> Result<(), XaiNativeAuthError> {
        if self.access_token.is_empty()
            || self.access_token.len() > MAX_STRING_BYTES
            || self.refresh_token.len() > MAX_STRING_BYTES
            || self
                .id_token
                .as_ref()
                .is_some_and(|value| value.len() > MAX_STRING_BYTES)
        {
            return Err(XaiNativeAuthError::InvalidTokenResponse(
                CredentialRefreshFailureReason::TokenShape,
            ));
        }
        if self.expires_in.is_some_and(|value| value == 0) {
            return Err(XaiNativeAuthError::InvalidTokenResponse(
                CredentialRefreshFailureReason::ExpiresIn,
            ));
        }
        Ok(())
    }
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        if let Some(value) = &mut self.id_token {
            value.zeroize();
        }
    }
}

impl<'de> Deserialize<'de> for TokenResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TokenVisitor;
        impl<'de> Visitor<'de> for TokenVisitor {
            type Value = TokenResponse;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bounded unique-key OAuth token object")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut seen = BTreeSet::new();
                let mut access_token = None;
                let mut refresh_token = None;
                let mut id_token = None;
                let mut expires_in = None;
                let mut count = 0usize;
                while let Some(key) = map.next_key::<String>()? {
                    count += 1;
                    if count > 32 || !seen.insert(key.clone()) {
                        return Err(de::Error::custom("duplicate or excessive OAuth fields"));
                    }
                    match key.as_str() {
                        "access_token" => access_token = Some(map.next_value()?),
                        "refresh_token" => refresh_token = Some(map.next_value()?),
                        "id_token" => id_token = Some(map.next_value()?),
                        "expires_in" => expires_in = Some(map.next_value()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(TokenResponse {
                    access_token: access_token
                        .ok_or_else(|| de::Error::missing_field("access_token"))?,
                    refresh_token: refresh_token.unwrap_or_default(),
                    id_token,
                    expires_in,
                })
            }
        }
        deserializer.deserialize_map(TokenVisitor)
    }
}

#[derive(Serialize, Deserialize)]
struct StoredTokens {
    version: u32,
    issuer: String,
    client_id: String,
    subject: String,
    scope: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    selection_epoch: String,
}

impl StoredTokens {
    fn validate(&self, account: &AccountBinding) -> Result<(), XaiNativeAuthError> {
        match RecordAge::of_version(self.version.into(), STORED_TOKENS_VERSION.into()) {
            RecordAge::Older => return Err(XaiNativeAuthError::StoredCredentialFormatRetired),
            RecordAge::Newer => {
                return Err(XaiNativeAuthError::StoredCredentialWrittenByNewerBuild);
            }
            RecordAge::Current => {}
        }
        if self.version != STORED_TOKENS_VERSION
            || self.issuer != XAI_ISSUER
            || self.client_id != XAI_PUBLIC_CLIENT_ID
            || self.subject != account.as_str()
            || self.scope != XAI_OAUTH_SCOPE
            || self.access_token.is_empty()
            || self.access_token.len() > MAX_STRING_BYTES
            || self.refresh_token.is_empty()
            || self.refresh_token.len() > MAX_STRING_BYTES
            || !valid_login_selection_epoch(&self.selection_epoch)
        {
            return Err(XaiNativeAuthError::InvalidStoredCredential);
        }
        Ok(())
    }
}

impl Drop for StoredTokens {
    fn drop(&mut self) {
        self.subject.zeroize();
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.selection_epoch.zeroize();
    }
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

impl DeviceResponse {
    fn validate(&self) -> Result<(), XaiNativeAuthError> {
        if self.device_code.is_empty()
            || self.device_code.len() > MAX_STRING_BYTES
            || !valid_display_value(&self.user_code, 128)
            || !valid_https_url(&self.verification_uri)
            || !(1..=MAX_DEVICE_EXPIRES_SECONDS).contains(&self.expires_in)
            || self
                .verification_uri_complete
                .as_deref()
                .is_some_and(|value| !valid_https_url(value))
        {
            return Err(XaiNativeAuthError::InvalidTokenResponse(
                CredentialRefreshFailureReason::TokenShape,
            ));
        }
        Ok(())
    }
}

impl Drop for DeviceResponse {
    fn drop(&mut self) {
        self.device_code.zeroize();
    }
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeResponse {}

/// Tests never bind the pinned xAI callback port.
#[cfg(test)]
fn refuse_pinned_test_loopback(ports: &[u16]) {
    assert_ne!(
        ports, XAI_OAUTH.loopback_ports,
        "tests never bind the pinned xAI callback port"
    );
}

fn authorization_url(
    endpoints: &AuthEndpoints,
    redirect_uri: &str,
    pkce: &PkceS256,
    binding: &CallbackBinding,
) -> Result<Zeroizing<String>, XaiNativeAuthError> {
    // Dialect-owned extras stay local; shared builder owns the PKCE core.
    // Manager endpoint origin stays authoritative (including test doubles).
    build_authorization_url(AuthorizeUrlRequest {
        pins: XAI_OAUTH,
        auth_origin: Some(endpoints.authorize().origin.as_str()),
        redirect_uri,
        code_challenge: pkce.challenge().as_str(),
        state: binding.expose_state(),
        nonce: Some(binding.expose_nonce()),
        extra_pairs: crate::oauth_pins::XAI_AUTHORIZE_EXTRA_PAIRS,
    })
    .map_err(|_| XaiNativeAuthError::Request)
}

fn until_flow_deadline<F, T>(
    deadline: Instant,
    future: F,
) -> impl Future<Output = Result<T, XaiNativeAuthError>>
where
    F: Future<Output = Result<T, XaiNativeAuthError>>,
{
    until_deadline(deadline, || XaiNativeAuthError::LoginExpired, future)
}

fn credential_record(
    account: AccountBinding,
    generation: CredentialGeneration,
    stored: &StoredTokens,
) -> Result<CredentialRecord, XaiNativeAuthError> {
    let bytes =
        serde_json::to_vec(&stored).map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
    Ok(CredentialRecord::new(
        auth_profile_id()?,
        account,
        generation,
        CredentialStatus::Ready,
        SecretBytes::new(bytes),
    )?)
}

fn auth_profile_id() -> Result<plakous_auth::SubscriptionIntegrationProfileId, XaiNativeAuthError> {
    plakous_auth::SubscriptionIntegrationProfileId::new(XAI_GROK_COMPATIBILITY_PROFILE)
        .map_err(Into::into)
}

fn route_credential_authority_id(store: &PrivateCredentialStore) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(store.path().as_os_str().as_encoded_bytes());
    hasher.update(b"\0xai-route-credential-authority-v2\0");
    hasher.update(store.slot_id().as_str().as_bytes());
    let mut encoded = String::from("plak-native-xai-route:");
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

/// Only the version of a stored payload.
#[derive(Deserialize)]
struct StoredTokensVersion {
    version: u32,
}

/// Refuses a record in any lifecycle state whose payload a newer build wrote,
/// then a Ready record whose payload predates [`STORED_TOKENS_VERSION`].
/// A lease held elsewhere is not waited for; the next read names it.
fn refuse_retired_payload(
    store: &PrivateCredentialStore,
    fence: &CredentialFence,
) -> Result<(), XaiNativeAuthError> {
    let newer = store.inspect_payload(|payload| {
        serde_json::from_slice::<StoredTokensVersion>(payload.expose_secret()).is_ok_and(|stored| {
            RecordAge::of_version(stored.version.into(), STORED_TOKENS_VERSION.into())
                == RecordAge::Newer
        })
    })?;
    if newer == Some(true) {
        return Err(XaiNativeAuthError::StoredCredentialWrittenByNewerBuild);
    }
    if fence.status() != CredentialStatus::Ready {
        return Ok(());
    }
    let Ok(Some(lease)) = store.try_acquire_ready(fence) else {
        return Ok(());
    };
    match serde_json::from_slice::<StoredTokens>(lease.payload().expose_secret()) {
        Ok(stored) if stored.version < STORED_TOKENS_VERSION => {
            Err(XaiNativeAuthError::StoredCredentialFormatRetired)
        }
        _ => Ok(()),
    }
}

/// A retired payload is restored (nothing was sent, and the record needs a new
/// login rather than quarantine); any other invalid payload stays quarantined.
fn retired_or_quarantined(error: XaiNativeAuthError) -> BuildRequestFailure<XaiNativeAuthError> {
    match error {
        XaiNativeAuthError::StoredCredentialFormatRetired => BuildRequestFailure::Restore(error),
        error => BuildRequestFailure::LeaveQuarantined(error),
    }
}

fn generate_selection_epoch() -> Result<String, XaiNativeAuthError> {
    generate_login_selection_epoch().map_err(|_| XaiNativeAuthError::RandomnessUnavailable)
}

#[cfg(test)]
fn ensure_compatibility(now: u64) -> Result<(), XaiNativeAuthError> {
    ensure_compatibility_state(now, COMPATIBILITY_KILLED)
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

#[cfg(test)]
fn ensure_compatibility_state(now: u64, killed: bool) -> Result<(), XaiNativeAuthError> {
    if killed || now >= COMPATIBILITY_EXPIRES_UNIX {
        Err(XaiNativeAuthError::CompatibilityInactive)
    } else {
        Ok(())
    }
}

fn validate_acceptance(
    acceptance: &XaiCompatibilityAcceptance,
    now: u64,
) -> Result<(), XaiNativeAuthError> {
    validate_acceptance_until(acceptance, now, COMPATIBILITY_EXPIRES_UNIX)
}

fn validate_acceptance_until(
    acceptance: &XaiCompatibilityAcceptance,
    now: u64,
    expires_unix: u64,
) -> Result<(), XaiNativeAuthError> {
    // Preserve distinct dialect errors: acceptance from the future vs
    // kill/expiry/clock before the review. Consent binds the contract
    // revisions, so an acceptance made before the review stays current.
    if acceptance.accepted_at > now {
        return Err(XaiNativeAuthError::CompatibilityNotAccepted);
    }
    if XAI_PROFILE.killed || now < COMPATIBILITY_REVIEWED_UNIX || now >= expires_unix {
        return Err(XaiNativeAuthError::CompatibilityInactive);
    }
    // Matches CompatibilityProfilePins::is_acceptance_current_until with
    // error-class split retained for dialect callers.
    Ok(())
}

fn map_oauth_workflow_error(error: OAuthWorkflowError) -> XaiNativeAuthError {
    match error {
        OAuthWorkflowError::Expired => XaiNativeAuthError::LoginExpired,
        OAuthWorkflowError::Binding | OAuthWorkflowError::Callback => {
            XaiNativeAuthError::InvalidCallback
        }
        OAuthWorkflowError::ResponseTooLarge => XaiNativeAuthError::InvalidTokenResponse(
            CredentialRefreshFailureReason::ResponseTooLarge,
        ),
        _ => XaiNativeAuthError::InvalidTokenResponse(CredentialRefreshFailureReason::TokenShape),
    }
}

fn system_unix_seconds() -> Result<u64, XaiNativeAuthError> {
    system_unix_elapsed().map(|value| value.as_secs())
}

/// The clock the Grok compatibility profile, its credentials and its catalog
/// receipts are checked against.
///
/// Supported builds read the system clock. Offline tests and `test-clock`
/// builds read a clock that starts one minute after the compiled review and
/// advances with elapsed monotonic time, so they never change with the
/// calendar (spec 05 sec. 2).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SubscriptionClock;

impl plakous_model::Clock for SubscriptionClock {
    fn now(&self) -> Timestamp {
        system_unix_elapsed()
            .ok()
            .and_then(|elapsed| Timestamp::new(elapsed.as_secs(), elapsed.subsec_nanos()).ok())
            .unwrap_or_else(|| plakous_model::Clock::now(&plakous_model::SystemClock))
    }
}

fn system_unix_elapsed() -> Result<Duration, XaiNativeAuthError> {
    subscription_time::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| XaiNativeAuthError::CompatibilityInactive)
}

/// Supported builds: the system clock.
#[cfg(not(any(test, feature = "test-clock")))]
mod subscription_time {
    pub(super) fn now() -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}

/// Offline tests and `test-clock` builds: one minute after the compiled
/// review, advancing with elapsed monotonic time.
#[cfg(any(test, feature = "test-clock"))]
mod subscription_time {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    pub(super) fn now() -> SystemTime {
        static START: OnceLock<Instant> = OnceLock::new();
        UNIX_EPOCH
            + Duration::from_secs(super::COMPATIBILITY_REVIEWED_UNIX + 60)
            + START.get_or_init(Instant::now).elapsed()
    }
}

#[cfg(test)]
fn now_timestamp() -> Result<Timestamp, XaiNativeAuthError> {
    Timestamp::new(system_unix_seconds()?, 0).map_err(Into::into)
}

fn fixed_headers(content_type: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(HEADER_CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(HEADER_USER_AGENT, HeaderValue::from_static(USER_AGENT));
    headers
}

fn insert_subscription_inference_headers(headers: &mut HeaderMap) {
    headers.insert(
        HEADER_XAI_TOKEN_AUTH,
        HeaderValue::from_static(XAI_TOKEN_AUTH_VALUE),
    );
    headers.insert(
        HEADER_AUTHENTICATE_RESPONSE,
        HeaderValue::from_static(AUTHENTICATE_RESPONSE_VALUE),
    );
    headers.insert(
        HEADER_GROK_CLIENT_VERSION,
        HeaderValue::from_static(SUPERGROK_CLIENT_COMPATIBILITY_VERSION),
    );
    headers.insert(
        HEADER_GROK_CLIENT_IDENTIFIER,
        HeaderValue::from_static(PLAKOUS_XAI_CLIENT_IDENTIFIER),
    );
    headers.insert(
        HEADER_GROK_CLIENT_MODE,
        HeaderValue::from_static(PLAKOUS_XAI_CLIENT_MODE),
    );
}

fn insert_subscription_catalog_headers(
    headers: &mut HeaderMap,
    subject: &AccountBinding,
) -> Result<(), XaiNativeAuthError> {
    let subject = HeaderValue::from_str(subject.as_str())
        .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
    headers.insert(
        HEADER_XAI_TOKEN_AUTH,
        HeaderValue::from_static(XAI_TOKEN_AUTH_VALUE),
    );
    headers.insert(HEADER_X_USER_ID, subject);
    headers.insert(
        HEADER_GROK_CLIENT_VERSION,
        HeaderValue::from_static(SUPERGROK_CLIENT_COMPATIBILITY_VERSION),
    );
    headers.insert(
        HEADER_GROK_CLIENT_IDENTIFIER,
        HeaderValue::from_static(PLAKOUS_XAI_CLIENT_IDENTIFIER),
    );
    headers.insert(
        HEADER_GROK_CLIENT_MODE,
        HeaderValue::from_static(PLAKOUS_XAI_CLIENT_MODE),
    );
    Ok(())
}

fn bearer_header(token: &str) -> Result<HeaderValue, XaiNativeAuthError> {
    let mut value = Vec::with_capacity(7 + token.len());
    value.extend_from_slice(b"Bearer ");
    value.extend_from_slice(token.as_bytes());
    sensitive_header_value(value).map_err(|_| XaiNativeAuthError::Request)
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8], maximum: usize) -> Result<T, XaiNativeAuthError> {
    // Size bound first so unique-key walk never scans oversized bodies.
    if bytes.len() > maximum {
        return Err(XaiNativeAuthError::InvalidTokenResponse(
            CredentialRefreshFailureReason::ResponseTooLarge,
        ));
    }
    if bytes.is_empty() {
        return Err(XaiNativeAuthError::InvalidTokenResponse(
            CredentialRefreshFailureReason::TokenShape,
        ));
    }
    validate_unique_object(bytes)?;
    decode_json_bounded(bytes, maximum).map_err(map_oauth_workflow_error)
}

fn validate_unique_object(bytes: &[u8]) -> Result<(), XaiNativeAuthError> {
    struct UniqueObject;
    impl<'de> Visitor<'de> for UniqueObject {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded unique-key JSON object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut seen = BTreeSet::new();
            let mut count = 0usize;
            while let Some(key) = map.next_key::<String>()? {
                count += 1;
                if count > 64 || !seen.insert(key) {
                    return Err(de::Error::custom("duplicate or excessive JSON fields"));
                }
                map.next_value::<IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    deserializer.deserialize_map(UniqueObject).map_err(|_| {
        XaiNativeAuthError::InvalidTokenResponse(CredentialRefreshFailureReason::TokenShape)
    })?;
    deserializer.end().map_err(|_| {
        XaiNativeAuthError::InvalidTokenResponse(CredentialRefreshFailureReason::TokenShape)
    })
}

fn valid_token_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':')
        })
}

fn valid_display_value(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_https_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn status_error(status: StatusCode) -> XaiNativeAuthError {
    XaiNativeAuthError::HttpStatus(status.as_u16())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
