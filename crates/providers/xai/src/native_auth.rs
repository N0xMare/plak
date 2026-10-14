//! Plakous-owned xAI OIDC authentication and selected-slot credential authority.

mod verify;

mod dispatch;

#[cfg(test)]
use dispatch::supergrok_generation_fingerprint;
pub(crate) use dispatch::{
    SuperGrokDispatchGuard, SuperGrokResponsesExecution, revalidate_supergrok_responses,
    supergrok_credential_rejected, supergrok_responses_preflight,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::lock::Mutex as AsyncMutex;
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use plakous_auth::oauth_orchestrator::{
    BrowserLoopbackHooks, BuildRequestFailure, ClaimedRefreshHooks, ClaimedRevokeHooks,
    CredentialRefreshEvent, CredentialRefreshFailureClass, CredentialRefreshObserver,
    CredentialRefreshPhase, FetchedJwks, JwksCacheState, JwksResolvePolicy,
    LoopbackConnectionEvent, PullBodyError, bind_loopback_listener, pull_claimed_body,
    resolve_signing_key, run_browser_loopback_session, run_claimed_refresh, run_claimed_revoke,
};
use plakous_auth::oauth_workflow::{
    AuthorizeUrlRequest, CallbackQueryPolicy, DevicePollSchedule, OAuthCallbackOutcome,
    OAuthWorkflowError, access_needs_refresh, authorization_code_token_form,
    build_authorization_url, decode_json_bounded, format_http11_text_response, is_http_token_byte,
    parse_absolute_redirect_query, parse_http11_get_request, parse_remote_authorization_input,
    refresh_token_form, remaining_until, unsigned_access_token_exp, urlencoded_form,
};
use plakous_auth::subscription_limit::{
    SubscriptionRequestClass, SubscriptionRequestGate, SubscriptionRequestPermit,
};
use plakous_auth::{
    AccountBinding, CallbackBinding, ClaimedHttpDisposition, ClaimedHttpError, ClaimedHttpResponse,
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    OfflineCredentialLifecycle, PkceS256, PrivateCredentialStore, PrivateStoreError,
    ReadyCredentialLease, RefreshClaim, SecretBytes, SecretString,
};
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
/// Official OIDC discovery document pinned by the profile.
pub const XAI_OIDC_DISCOVERY: &str = XAI_OAUTH.oidc_discovery_url.unwrap();
/// Immutable upstream source revision reviewed for public-client compatibility.
pub const XAI_OPENCODE_SOURCE_REVISION: &str = XAI_PROFILE.source_revision;
/// Immutable upstream source file reviewed for public-client compatibility.
pub const XAI_OPENCODE_SOURCE: &str = XAI_PROFILE.source_locator.unwrap();
/// Pinned xAI public OAuth client identifier.
pub const XAI_PUBLIC_CLIENT_ID: &str = XAI_OAUTH.client_id;
/// Fixed loopback redirect registered for the pinned public client.
pub const XAI_LOOPBACK_REDIRECT: &str = XAI_OAUTH.loopback_redirect_uri;
/// Exact requested scope set, in its pinned wire order.
pub const XAI_OAUTH_SCOPE: &str = XAI_OAUTH.scopes;
/// Exact operator notice revision for this experimental compatibility profile.
pub const XAI_GROK_COMPATIBILITY_NOTICE_REVISION: &str = XAI_PROFILE.notice_revision;
/// Locally reviewable kill-switch revision bound into exact acceptance.
pub const XAI_GROK_KILL_SWITCH_REVISION: &str = XAI_PROFILE.kill_switch_revision;
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
/// evidence advances independently through [`XAI_GROK_MODEL_CATALOG_SOURCE_REVISION`].
pub const XAI_GROK_BUILD_SOURCE_REVISION: &str = "afbc0fb710320c7add294c2106d447ecc3e3af2e";
/// Current immutable Grok Build model-catalog evidence revision.
///
/// Model enrollment advances independently from OAuth compatibility so adding
/// a reviewed model cannot invalidate an otherwise unchanged credential.
pub const XAI_GROK_MODEL_CATALOG_SOURCE_REVISION: &str = "5163763e703c319e4554c2f455535c5adb6e51e8";
/// Mutable official documentation locator for local function calling.
///
/// This URL is diagnostic context only. It is never capability authority;
/// pinned source revisions, the fixed profile, and the authenticated exact-ID
/// catalog receipt own that decision.
pub const XAI_FUNCTION_CALLING_DOCUMENTATION_LOCATOR: &str =
    "https://docs.x.ai/docs/guides/function-calling";
/// Mutable official documentation locator for the Grok Build model page.
///
/// This URL is diagnostic context only and is not immutable model evidence.
pub const XAI_GROK_BUILD_MODEL_DOCUMENTATION_LOCATOR: &str =
    "https://docs.x.ai/docs/models/grok-build";

const XAI_ISSUER: &str = XAI_OAUTH.auth_origin;
const AUTHORIZATION_PATH: &str = XAI_OAUTH.authorize_path;
const DEVICE_PATH: &str = XAI_OAUTH.device_authorization_path;
const TOKEN_PATH: &str = XAI_OAUTH.token_path;
const USERINFO_PATH: &str = XAI_OAUTH.userinfo_path;
const REVOCATION_PATH: &str = XAI_OAUTH.revoke_path;
const JWKS_PATH: &str = XAI_OAUTH.jwks_path;
const CATALOG_PATH: &str = crate::SUPERGROK_MODELS_PATH;
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

/// Provenance of the fixed compatibility facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum XaiCompatibilityProvenance {
    /// Facts were reviewed in immutable official and upstream source revisions.
    PinnedCompatibility,
}

/// Credential-free immutable facts for the built-in `SuperGrok` integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XaiSubscriptionIntegrationProfile;

impl XaiSubscriptionIntegrationProfile {
    /// Returns the exact immutable profile identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        XAI_GROK_COMPATIBILITY_PROFILE
    }

    /// Returns the immutable public-client compatibility source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        XAI_OPENCODE_SOURCE_REVISION
    }

    /// Returns the immutable official Grok Build source revision.
    #[must_use]
    pub const fn official_model_source_revision(self) -> &'static str {
        XAI_GROK_BUILD_SOURCE_REVISION
    }

    /// Returns observed compatibility provenance, not provider endorsement.
    #[must_use]
    pub const fn provenance(self) -> XaiCompatibilityProvenance {
        XaiCompatibilityProvenance::PinnedCompatibility
    }

    /// Returns the UTC review timestamp.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        COMPATIBILITY_REVIEWED_UNIX
    }

    /// Returns the hard compatibility expiry.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        COMPATIBILITY_EXPIRES_UNIX
    }

    /// Returns the exact operator notice revision.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        XAI_GROK_COMPATIBILITY_NOTICE_REVISION
    }

    /// Returns the locally reviewable kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        XAI_GROK_KILL_SWITCH_REVISION
    }

    /// Reports the compiled local kill-switch state.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        COMPATIBILITY_KILLED
    }
}

/// A deliberate exact-revision acceptance capability.
pub struct XaiCompatibilityAcceptance {
    accepted_at: u64,
}

impl XaiCompatibilityAcceptance {
    /// Accepts exactly every immutable profile, source, notice, and kill revision.
    ///
    /// # Errors
    ///
    /// Rejects any alias, omitted revision, older revision, or future revision.
    pub fn accept_exact(
        profile_id: &str,
        source_revision: &str,
        official_model_source_revision: &str,
        notice_revision: &str,
        kill_switch_revision: &str,
        accepted_at: Timestamp,
    ) -> Result<Self, XaiNativeAuthError> {
        if profile_id != XAI_GROK_COMPATIBILITY_PROFILE
            || source_revision != XAI_OPENCODE_SOURCE_REVISION
            || official_model_source_revision != XAI_GROK_BUILD_SOURCE_REVISION
            || notice_revision != XAI_GROK_COMPATIBILITY_NOTICE_REVISION
            || kill_switch_revision != XAI_GROK_KILL_SWITCH_REVISION
            || accepted_at.seconds() < COMPATIBILITY_REVIEWED_UNIX
        {
            return Err(XaiNativeAuthError::CompatibilityNotAccepted);
        }
        Ok(Self {
            accepted_at: accepted_at.seconds(),
        })
    }
}

impl fmt::Debug for XaiCompatibilityAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiCompatibilityAcceptance(<exact pinned revision>)")
    }
}

/// Credential-free selected-slot state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiSubscriptionStatus {
    generation: CredentialGeneration,
    status: CredentialStatus,
    account: AccountBinding,
}

impl XaiSubscriptionStatus {
    /// Returns the durable generation.
    #[must_use]
    pub const fn generation(&self) -> CredentialGeneration {
        self.generation
    }

    /// Returns the credential-free lifecycle state.
    #[must_use]
    pub const fn status(&self) -> CredentialStatus {
        self.status
    }

    /// Returns the redacted provider-derived account binding.
    #[must_use]
    pub const fn account_binding(&self) -> &AccountBinding {
        &self.account
    }
}

/// Credential-free lifecycle for offline SuperGrok selected-slot maintenance.
pub type XaiOfflineCredentialStatus = OfflineCredentialLifecycle;

/// Offline maintenance handle for one explicit native xAI slot.
///
/// This handle owns no HTTP client, accepts no compatibility capability, and
/// never exposes account, generation, access-token, or refresh-token data. It
/// remains available after profile expiry or local kill-switch activation.
///
/// ```compile_fail
/// # use plakous_xai::XaiOfflineSelectedSlot;
/// fn expose(slot: &XaiOfflineSelectedSlot) {
///     let _ = slot.access_token();
/// }
/// ```
pub struct XaiOfflineSelectedSlot {
    store: PrivateCredentialStore,
}

impl XaiOfflineSelectedSlot {
    /// Opens exactly the application-supplied private slot without network or
    /// current-profile admission.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe path, slot, or foreign/malformed stored record.
    pub fn open_explicit(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
    ) -> Result<Self, XaiNativeAuthError> {
        Ok(Self {
            store: PrivateCredentialStore::open_slot(path, auth_profile_id()?, slot)?,
        })
    }

    /// Returns only the credential-free local lifecycle state.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe or malformed explicit storage.
    pub fn status(&self) -> Result<XaiOfflineCredentialStatus, XaiNativeAuthError> {
        offline_status(self.store.status()?)
    }

    /// Writes an always-available local revoked tombstone without network I/O.
    ///
    /// This operation deliberately does not check acceptance, expiry, or the
    /// compiled kill switch. An absent slot remains signed out.
    ///
    /// # Errors
    ///
    /// Rejects a stale mutation or unsafe/malformed explicit storage.
    pub fn logout_local(&self) -> Result<XaiOfflineCredentialStatus, XaiNativeAuthError> {
        Ok(self.store.logout_local()?)
    }
}

impl fmt::Debug for XaiOfflineSelectedSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiOfflineSelectedSlot(<explicit credential-free maintenance>)")
    }
}

/// Credential-free diagnostics reported for one exact xAI catalog model.
///
/// `reported_context_length` is not a capacity certificate or output
/// reservation. For the two release-enrolled literal models it may only
/// corroborate an equal-or-lower release-owned bound; it can never expand that
/// bound. Missing, zero, and values outside the nonzero `u32` domain make the
/// row ineligible for the exact authenticated context contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct XaiCatalogDiagnosticFacts {
    reported_context_length: NonZeroU32,
}

impl XaiCatalogDiagnosticFacts {
    /// Returns the diagnostic `/v1/models` `context_length` value.
    ///
    /// This value cannot authorize or expand context admission by itself.
    #[must_use]
    pub const fn reported_context_length(self) -> NonZeroU32 {
        self.reported_context_length
    }
}

/// One exact coding-model row retained from authenticated xAI discovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XaiNativeCatalogEntry {
    /// Catalog selection key (`id`, or the effective model when omitted).
    model: ModelId,
    /// Exact wire model selected by pinned field precedence.
    effective_model: ModelId,
    diagnostics: XaiCatalogDiagnosticFacts,
}

impl XaiNativeCatalogEntry {
    /// Returns the exact, unaliased catalog model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the exact effective model selected by the pinned proxy grammar.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }

    /// Returns non-authoritative diagnostics reported for this model row.
    #[must_use]
    pub const fn diagnostics(&self) -> XaiCatalogDiagnosticFacts {
        self.diagnostics
    }
}

/// Credential-free authenticated model-catalog admission receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiNativeCatalogReceipt {
    entries: Vec<XaiNativeCatalogEntry>,
    account: AccountBinding,
    observed_at: Timestamp,
    expires_at: Timestamp,
}

impl XaiNativeCatalogReceipt {
    /// Returns every exact coding-model row admitted from authenticated discovery.
    #[must_use]
    pub fn entries(&self) -> &[XaiNativeCatalogEntry] {
        &self.entries
    }

    /// Returns every exact coding model admitted from the authenticated catalog.
    ///
    /// This is a derived compatibility projection. [`Self::entries`] retains
    /// non-authoritative catalog diagnostics alongside each exact reviewed ID.
    #[must_use]
    pub fn models(&self) -> Vec<ModelId> {
        self.model_ids()
    }

    /// Returns the retained row for one exact model identity.
    #[must_use]
    pub fn entry(&self, model: &ModelId) -> Option<&XaiNativeCatalogEntry> {
        self.entries.iter().find(|entry| entry.model() == model)
    }

    /// Returns whether this receipt admits the exact model identity.
    #[must_use]
    pub fn admits(&self, model: &ModelId) -> bool {
        self.entry(model).is_some()
    }

    /// Returns the redacted account binding used for authenticated discovery.
    #[must_use]
    pub const fn account_binding(&self) -> &AccountBinding {
        &self.account
    }

    /// Returns when this exact account-scoped catalog was observed.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the bounded receipt expiry.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    fn model_ids(&self) -> Vec<ModelId> {
        self.entries
            .iter()
            .map(|entry| entry.model.clone())
            .collect()
    }
}

/// Returns whether a SuperGrok catalog ID has complete release enrollment.
///
/// Catalog observation alone is insufficient. In particular,
/// `grok-build-0.1` has a recognized upstream row but no immutable effective
/// context threshold, so it remains unavailable.
#[must_use]
pub fn is_subscription_coding_model_id(model_id: &str) -> bool {
    grok_enrollment(model_id).is_some()
}

/// Returns whether a SuperGrok coding model is treated as effort-capable.
///
/// Only exact reviewed Grok 4.5/4.6/4.7 evidence admits reasoning effort.
#[must_use]
pub fn subscription_model_supports_reasoning(model_id: &str) -> bool {
    !subscription_model_reasoning_effort_options(model_id).is_empty()
}

/// Returns exact reviewed portable effort facts for a SuperGrok catalog ID.
///
/// Only literal Grok 4.5/4.6/4.7 identities have reviewed effort-option evidence.
/// Other exact IDs fail closed with no options; aliases and string heuristics
/// are not accepted.
#[must_use]
pub fn subscription_model_reasoning_effort_options(model_id: &str) -> &'static [ReasoningEffort] {
    grok_enrollment(model_id).map_or(&[], |row| row.efforts)
}

/// Credential-free evidence for one exact native xAI model resolution.
///
/// Every field is copied from the same authenticated catalog receipt,
/// credential fence, dynamic route, and descriptor that produced the returned
/// [`LanguageModel`]. Raw account identifiers, bearer credentials, and local
/// store paths are never retained by this value.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct XaiModelResolutionEvidence {
    evidence_revision: u32,
    account_fingerprint: Digest,
    credential_authority_id: String,
    credential_authority_binding: Digest,
    selected_credential_generation: u64,
    credential_selection_epoch: String,
    route_fingerprint: String,
    descriptor: ResolvedModelDescriptor,
    descriptor_digest: Digest,
    dialect: XaiDialect,
    adapter_revision: String,
    catalog_observed_at: Timestamp,
    catalog_expires_at: Timestamp,
    catalog_diagnostics: XaiCatalogDiagnosticFacts,
    runtime_admission_witness: RuntimeAdmissionWitness,
    profile_revision: String,
}

impl XaiModelResolutionEvidence {
    /// Returns the serialized exact-resolution evidence revision.
    #[must_use]
    pub const fn evidence_revision(&self) -> u32 {
        self.evidence_revision
    }

    /// Returns the exact opaque billed-account fingerprint bound into the route.
    #[must_use]
    pub const fn account_fingerprint(&self) -> Digest {
        self.account_fingerprint
    }

    /// Returns the stable non-secret ID of the selected credential authority.
    #[must_use]
    pub fn credential_authority_id(&self) -> &str {
        &self.credential_authority_id
    }

    /// Returns the opaque binding of the selected credential authority.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the exact stored credential generation selected for resolution.
    #[must_use]
    pub const fn selected_credential_generation(&self) -> u64 {
        self.selected_credential_generation
    }

    /// Returns the exact random selection epoch stored with the selected login.
    #[must_use]
    pub fn credential_selection_epoch(&self) -> &str {
        &self.credential_selection_epoch
    }

    /// Returns the exact dynamic route fingerprint used by the returned model.
    #[must_use]
    pub fn route_fingerprint(&self) -> &str {
        &self.route_fingerprint
    }

    /// Returns the exact neutral descriptor binding digest of the returned model.
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
        if self.evidence_revision != 2
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

    /// Returns the exact xAI protocol dialect selected for the returned model.
    #[must_use]
    pub const fn dialect(&self) -> XaiDialect {
        self.dialect
    }

    /// Returns the exact xAI protocol-adapter revision.
    #[must_use]
    pub fn adapter_revision(&self) -> &str {
        &self.adapter_revision
    }

    /// Returns when the authenticated account-scoped catalog was observed.
    #[must_use]
    pub const fn catalog_observed_at(&self) -> Timestamp {
        self.catalog_observed_at
    }

    /// Returns the exclusive expiry of the authenticated catalog receipt.
    #[must_use]
    pub const fn catalog_expires_at(&self) -> Timestamp {
        self.catalog_expires_at
    }

    /// Returns authenticated but non-authoritative catalog diagnostics.
    #[must_use]
    pub const fn catalog_diagnostics(&self) -> XaiCatalogDiagnosticFacts {
        self.catalog_diagnostics
    }

    /// Returns the exact bounded selected-row/catalog-receipt witness produced
    /// by the same live route that produced the model.
    #[must_use]
    pub const fn runtime_admission_witness(&self) -> &RuntimeAdmissionWitness {
        &self.runtime_admission_witness
    }

    /// Returns the exact descriptor compatibility-profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        XAI_GROK_COMPATIBILITY_PROFILE
    }

    /// Returns the exact immutable native compatibility-profile revision.
    #[must_use]
    pub fn profile_revision(&self) -> &str {
        &self.profile_revision
    }

    /// Returns the provider-owned semantic evidence projection used by
    /// effective configuration identity.
    ///
    /// Token generation, selection epoch, dynamic receipt identity, and
    /// freshness stay in per-attempt runtime authority rather than this stable
    /// projection.
    pub fn semantic_provider_evidence(
        &self,
    ) -> Result<ProviderSemanticEvidence, ProviderConfigurationError> {
        #[derive(Serialize)]
        struct SemanticSuperGrokEvidence<'a> {
            schema: &'a str,
            evidence_revision: u32,
            adapter_revision: &'a str,
            profile_revision: &'a str,
            feature_revision: &'a str,
            dialect_revision: u32,
            source_revision: &'a str,
            reported_context_length: u32,
        }
        let bytes = serde_json::to_vec(&SemanticSuperGrokEvidence {
            schema: "plakous-xai/supergrok-semantic-evidence-v2",
            evidence_revision: self.evidence_revision,
            adapter_revision: &self.adapter_revision,
            profile_revision: &self.profile_revision,
            feature_revision: crate::SUPERGROK_FEATURE_REVISION,
            dialect_revision: crate::SUPERGROK_RESPONSES_DIALECT_REVISION,
            source_revision: model_catalog_source_revision(
                self.descriptor.requested_model().as_str(),
            ),
            reported_context_length: self.catalog_diagnostics.reported_context_length().get(),
        })
        .map_err(|_| ProviderConfigurationError::Identity)?;
        ProviderSemanticEvidence::new(2, bytes)
    }
}

impl fmt::Debug for XaiModelResolutionEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiModelResolutionEvidence")
            .field("evidence_revision", &self.evidence_revision)
            .field("account_fingerprint", &self.account_fingerprint)
            .field("credential_authority_id", &self.credential_authority_id)
            .field(
                "credential_authority_binding",
                &self.credential_authority_binding,
            )
            .field(
                "selected_credential_generation",
                &self.selected_credential_generation,
            )
            .field(
                "credential_selection_epoch",
                &self.credential_selection_epoch,
            )
            .field("route_fingerprint", &self.route_fingerprint)
            .field("descriptor", &self.descriptor)
            .field("descriptor_digest", &self.descriptor_digest)
            .field("dialect", &self.dialect)
            .field("adapter_revision", &self.adapter_revision)
            .field("catalog_observed_at", &self.catalog_observed_at)
            .field("catalog_expires_at", &self.catalog_expires_at)
            .field("catalog_diagnostics", &self.catalog_diagnostics)
            .field("runtime_admission_witness", &"<redacted>")
            .field("profile_revision", &self.profile_revision)
            .finish()
    }
}

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

/// App-owned manager for one explicit xAI selected-account slot.
///
/// Credential bytes are intentionally not part of the public surface.
///
/// ```compile_fail
/// # use plakous_xai::XaiNativeAuthManager;
/// fn expose(manager: &XaiNativeAuthManager) {
///     let _ = manager.access_token();
/// }
/// ```
pub struct XaiNativeAuthManager {
    core: Arc<ManagerCore>,
}

impl XaiNativeAuthManager {
    /// Opens one explicit private selected-slot record.
    ///
    /// No environment, home-directory, owner store, proxy, or endpoint
    /// discovery is consulted.
    ///
    /// # Errors
    ///
    /// Fails for an unaccepted/expired profile or unsafe explicit store path.
    pub fn open_selected_slot(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
        client: Client,
        acceptance: XaiCompatibilityAcceptance,
    ) -> Result<Self, XaiNativeAuthError> {
        validate_acceptance(&acceptance, system_unix_seconds()?)?;
        let profile = auth_profile_id()?;
        let store = PrivateCredentialStore::open_slot(path, profile, slot)?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                store,
                client,
                acceptance,
                endpoints: AuthEndpoints::production(),
                compatibility_expires_unix: AtomicU64::new(COMPATIBILITY_EXPIRES_UNIX),
                jwks: AsyncMutex::new(JwksCacheState::with_max_negative(MAX_JWKS_KEYS)),
                refresh_flight: AsyncMutex::new(()),
                catalog: Mutex::new(None),
                request_gate: None,
                refresh_observer: None,
                #[cfg(test)]
                resolution_pause: Mutex::new(None),
            }),
        })
    }

    /// Attaches the classified physical-request gate required for inference.
    ///
    /// Leaving the gate absent keeps maintenance and catalog workflows usable,
    /// but every physical subscription inference dispatch fails closed.
    ///
    /// # Errors
    ///
    /// Returns an internal binding error if this manager has already been shared.
    pub fn with_subscription_request_gate(
        mut self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<Self, XaiNativeAuthError> {
        let core = Arc::get_mut(&mut self.core).ok_or(XaiNativeAuthError::Request)?;
        core.request_gate = Some(gate);
        Ok(self)
    }

    /// Attaches a content-free claimed-refresh observer (product telemetry).
    ///
    /// # Errors
    ///
    /// Returns an internal binding error if this manager has already been shared.
    pub fn with_credential_refresh_observer(
        mut self,
        observer: Arc<dyn CredentialRefreshObserver>,
    ) -> Result<Self, XaiNativeAuthError> {
        let core = Arc::get_mut(&mut self.core).ok_or(XaiNativeAuthError::Request)?;
        core.refresh_observer = Some(observer);
        Ok(self)
    }

    /// Returns the immutable credential-free integration profile.
    #[must_use]
    pub const fn profile(&self) -> XaiSubscriptionIntegrationProfile {
        XaiSubscriptionIntegrationProfile
    }

    /// Returns the current credential-free status.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed or unsafe storage.
    pub fn status(&self) -> Result<Option<XaiSubscriptionStatus>, XaiNativeAuthError> {
        Ok(self.core.store.load()?.map(|fence| XaiSubscriptionStatus {
            generation: fence.generation(),
            status: fence.status(),
            account: fence.account_binding().clone(),
        }))
    }

    /// Performs exactly one read-only Grok subscription usage request.
    ///
    /// This observation acquires no inference permit and grants no catalog,
    /// admission, retry, billing, or inference authority. The exact selected
    /// Ready generation remains leased through the bounded response and decode.
    ///
    /// It never starts a refresh: the hosted usage read is bounded by a timeout
    /// that may drop it, and a refresh dropped after a rotated token was issued
    /// would leave the store on a refresh token the provider may have retired.
    /// An expired access token is reported as `RefreshRequired` without network
    /// I/O; the next turn refreshes it.
    pub async fn subscription_usage(&self) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(system_unix_seconds().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        if self.core.ensure_active().is_err() {
            return unavailable(SubscriptionUsageUnavailable::Unqualified);
        }
        // Every refusal before the HTTP exchange is local and is named as such:
        // a held refresh flight or store lease is `Busy`, a non-Ready record is
        // its exact `CredentialState`, and unsafe storage or a dead clock is
        // `LocalState`. Only the bounded exchange below may report `Transport`.
        let Some(_lifecycle) = self.core.refresh_flight.try_lock() else {
            return unavailable(SubscriptionUsageUnavailable::Busy);
        };
        let fence = match self.core.store.try_load() {
            Ok(Some(fence)) => fence,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::NotConnected),
            Err(PrivateStoreError::StoreBusy) => {
                return unavailable(SubscriptionUsageUnavailable::Busy);
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        // A durable Unknown record whose claim another process still holds is
        // a refresh in flight, not a settled quarantine.
        if fence.status() == CredentialStatus::RefreshOutcomeUnknown {
            match self.core.store.refresh_outcome_settled(&fence) {
                Ok(true) => {}
                Ok(false) => return unavailable(SubscriptionUsageUnavailable::Busy),
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            }
        }
        if let Some(reason) = usage_credential_refusal(fence.status()) {
            return unavailable(reason);
        }
        let lease = match self.core.store.try_acquire_ready(&fence) {
            Ok(Some(lease)) => lease,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        let stored: StoredTokens = match serde_json::from_slice(lease.payload().expose_secret()) {
            Ok(stored) => stored,
            Err(_) => return unavailable(SubscriptionUsageUnavailable::InvalidResponse),
        };
        if stored.validate(fence.account_binding()).is_err() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }
        let Ok(now) = system_unix_seconds() else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        if stored.expires_at <= now {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        if lease.fence() != fence || stored.subject != fence.account_binding().as_str() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }

        let mut headers = HeaderMap::new();
        let Ok(authorization) = bearer_header(&stored.access_token) else {
            return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
        };
        let Ok(subject) = sensitive_header_value(stored.subject.as_bytes().to_vec()) else {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        };
        headers.insert(HEADER_AUTHORIZATION, authorization);
        headers.insert(HEADER_X_USER_ID, subject);
        headers.insert(
            HEADER_XAI_TOKEN_AUTH,
            HeaderValue::from_static(XAI_TOKEN_AUTH_VALUE),
        );
        headers.insert(
            HEADER_GROK_CLIENT_VERSION,
            HeaderValue::from_static(SUPERGROK_CLIENT_COMPATIBILITY_VERSION),
        );
        headers.insert(
            HEADER_GROK_CLIENT_MODE,
            HeaderValue::from_static(PLAKOUS_XAI_CLIENT_MODE),
        );
        let usage_endpoint = self.core.endpoints.usage();
        let Ok(request) = Request::new(
            Origin::parse(&usage_endpoint.origin)
                .unwrap_or_else(|_| unreachable!("the reviewed usage origin is valid")),
            Method::GET,
            &usage_endpoint.path,
            headers,
            Bytes::new(),
            self.core.client.limits(),
        ) else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        let (status, is_json, body) = match self.core.execute(request).await {
            Ok(reply) => reply,
            Err(AuthTransportError::ResponseTooLarge) => {
                return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
            }
            Err(AuthTransportError::Failure(failure)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(network_failure(
                    failure,
                )));
            }
        };
        let observed_at = Timestamp::new(system_unix_seconds().unwrap_or(now), 0)
            .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp"));
        let reason = match status.as_u16() {
            200 => None,
            401 | 403 => Some(SubscriptionUsageUnavailable::Unauthorized),
            429 => Some(SubscriptionUsageUnavailable::RateLimited),
            _ => Some(SubscriptionUsageUnavailable::ProviderUnavailable),
        };
        if let Some(reason) = reason {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason,
            };
        }
        if !is_json {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            };
        }
        if lease.fence() != fence {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::AccountChanged,
            };
        }
        match crate::decode_grok_subscription_usage(&body, observed_at) {
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(snapshot),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    /// Begins PKCE/state/nonce browser login on the fixed loopback redirect.
    ///
    /// # Errors
    ///
    /// Fails if the compatibility profile is inactive, randomness is
    /// unavailable, or the fixed loopback socket cannot be exclusively bound.
    pub async fn begin_browser_login(&self) -> Result<XaiBrowserLogin, XaiNativeAuthError> {
        let flow = self.begin_browser_login_flow()?;
        let (listener, port) = bind_loopback_listener(&[XAI_OAUTH.primary_loopback_port()])
            .await
            .map_err(|_| XaiNativeAuthError::LoopbackUnavailable)?;
        if !XAI_OAUTH.allows_loopback_port(port) {
            return Err(XaiNativeAuthError::LoopbackUnavailable);
        }
        flow.ensure_active()?;
        Ok(XaiBrowserLogin { flow, listener })
    }

    /// Begins a browser login that can be completed by pasting the full fixed
    /// loopback redirect URL on a different machine or in a different process.
    ///
    /// This method never binds or listens on the callback port. The xAI
    /// authorization request still uses the pinned
    /// [`XAI_LOOPBACK_REDIRECT`], and completion accepts only that exact
    /// absolute callback origin, port, and path.
    ///
    /// # Errors
    ///
    /// Fails if the compatibility profile is inactive, secure randomness is
    /// unavailable, or the selected-slot baseline cannot be read safely.
    pub fn begin_remote_browser_login(&self) -> Result<XaiRemoteBrowserLogin, XaiNativeAuthError> {
        Ok(XaiRemoteBrowserLogin {
            flow: self.begin_browser_login_flow()?,
        })
    }

    /// Begins an RFC 8628 device authorization flow.
    ///
    /// # Errors
    ///
    /// Fails on an inactive profile or a strict device response violation.
    pub async fn begin_device_login(&self) -> Result<XaiDeviceLogin, XaiNativeAuthError> {
        self.core.ensure_active()?;
        let authorization_deadline = self.core.flow_deadline(LOGIN_TIMEOUT)?;
        let baseline = self.core.store.load()?;
        let form = urlencoded_form(&[
            ("client_id", XAI_PUBLIC_CLIENT_ID),
            ("scope", XAI_OAUTH_SCOPE),
        ]);
        let response = until_flow_deadline(
            authorization_deadline,
            self.core.send_form(self.core.endpoints.device(), form),
        )
        .await?;
        let response_received_at = Instant::now();
        self.core.ensure_flow_active(authorization_deadline)?;
        let mut device: DeviceResponse = decode_json(&response, MAX_AUTH_BODY_BYTES)?;
        device.validate()?;
        let interval = Duration::from_secs(
            device
                .interval
                .unwrap_or(5)
                .clamp(1, MAX_DEVICE_INTERVAL_SECONDS),
        );
        let expires = Duration::from_secs(device.expires_in);
        let provider_deadline = response_received_at
            .checked_add(expires)
            .ok_or(XaiNativeAuthError::LoginExpired)?;
        let deadline = provider_deadline.min(self.core.flow_deadline(expires)?);
        self.core.ensure_flow_active(deadline)?;
        Ok(XaiDeviceLogin {
            core: Arc::clone(&self.core),
            baseline,
            device_code: SecretString::from(std::mem::take(&mut device.device_code)),
            user_code: std::mem::take(&mut device.user_code),
            verification_uri: std::mem::take(&mut device.verification_uri),
            verification_uri_complete: device.verification_uri_complete.take(),
            interval,
            deadline,
        })
    }

    /// Creates a freshly account-bound native subscription route.
    ///
    /// # Errors
    ///
    /// Requires a current Ready selected login and active compatibility profile.
    pub fn subscription_route(&self) -> Result<XaiGrokSubscriptionRoute, XaiNativeAuthError> {
        self.core.ensure_active()?;
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(XaiNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        let receipt = self
            .core
            .active_catalog_receipt(&fence, &stored, system_unix_seconds()?)?;
        self.subscription_route_from_receipt(&receipt)
    }

    fn subscription_route_from_receipt(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<XaiGrokSubscriptionRoute, XaiNativeAuthError> {
        let parts = native_route_authority(&self.core, receipt)?;
        XaiGrokSubscriptionRoute::from_native_authority(parts.authority, parts.admission)
            .map_err(Into::into)
    }

    /// Fetches and validates a fresh authenticated account-scoped model catalog.
    ///
    /// # Errors
    ///
    /// Fails unless the selected credential is Ready and the strict catalog
    /// admits at least one SuperGrok coding model identity.
    pub async fn refresh_catalog(&self) -> Result<XaiNativeCatalogReceipt, XaiNativeAuthError> {
        Ok(self.core.refresh_catalog_core().await?.public)
    }

    /// Resolves the caller's exact admitted direct Responses model.
    ///
    /// The resulting descriptor carries the immutable compatibility profile and
    /// conservative route-evidenced capabilities.
    ///
    /// # Errors
    ///
    /// Rejects non-coding identities before credential or network access.
    /// Catalog marketing aliases never expand: only exact catalog `id` values
    /// admit after authenticated discovery. Then fails unless route,
    /// compatibility, store, catalog, and model identities are current.
    pub async fn resolve_model(
        &self,
        requested_model: &str,
        client: Client,
    ) -> Result<Arc<dyn LanguageModel>, XaiNativeAuthError> {
        Ok(self
            .resolve_model_with_evidence(requested_model, client, StandardXaiHttpConfig::default())
            .await?
            .0)
    }

    /// Resolves one exact model and its atomically corresponding evidence.
    ///
    /// The returned model and evidence are constructed while retaining a shared
    /// lease for the exact catalog-authenticating credential fence and the lock
    /// for that exact cached catalog receipt. A completed relogin, generation
    /// change, catalog mutation, or expiry before this snapshot is acquired
    /// therefore fails closed instead of returning mixed evidence.
    ///
    /// # Errors
    ///
    /// Rejects non-coding identities before credential or network access.
    /// After authenticated catalog discovery, rejects alias/marketing names
    /// (not present as exact `id`), stale credential fences, replaced catalog
    /// receipts, inactive profiles, or inconsistent route/descriptor identity.
    pub async fn resolve_model_with_evidence(
        &self,
        requested_model: &str,
        client: Client,
        config: StandardXaiHttpConfig,
    ) -> Result<(Arc<dyn LanguageModel>, XaiModelResolutionEvidence), XaiNativeAuthError> {
        let requested = ModelId::new(requested_model)?;
        if !is_subscription_coding_model_id(requested.as_str()) {
            return Err(XaiNativeAuthError::RequestedModelNotAdmitted);
        }
        self.core.ensure_active()?;
        let receipt = self.core.refresh_catalog_core().await?;
        #[cfg(test)]
        self.core.pause_resolution_after_catalog().await?;
        let catalog_entry = receipt
            .public
            .entry(&requested)
            .ok_or(XaiNativeAuthError::RequestedModelNotAdmitted)?;
        let catalog_diagnostics = catalog_entry.diagnostics();
        let resolution_lease = self.core.acquire_resolution_lease(&receipt)?;
        let catalog = self
            .core
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
        if catalog.as_ref() != Some(&receipt) {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let route = self.subscription_route_from_receipt(&receipt)?;
        let resolved = XaiResolvedModel::new(
            route,
            XaiModel::new(requested, Some(ModelFamilyId::new("responses-family")?)),
            GrokCodingProfile::pinned_compatibility(),
            receipt.public.observed_at,
        )?;
        let identity = resolved
            .route()
            .identity()
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?;
        if identity.credential_authority() != receipt.authority_id
            || resolved.route().dialect() != XaiDialect::GrokSubscriptionResponses2026_07
            || resolved.descriptor().profile().as_str() != XAI_GROK_COMPATIBILITY_PROFILE
        {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let evidence = XaiModelResolutionEvidence {
            evidence_revision: 2,
            account_fingerprint: identity.account_binding(),
            credential_authority_id: identity.credential_authority().to_owned(),
            credential_authority_binding: identity.credential_authority_binding(),
            selected_credential_generation: receipt.fence.generation().get(),
            credential_selection_epoch: receipt.selection_epoch.clone(),
            route_fingerprint: identity.fingerprint().to_owned(),
            descriptor: resolved.descriptor().clone(),
            descriptor_digest: resolved.descriptor().binding_digest()?,
            dialect: resolved.route().dialect(),
            adapter_revision: XAI_RESPONSES_ADAPTER_REVISION.to_owned(),
            catalog_observed_at: receipt.public.observed_at,
            catalog_expires_at: receipt.public.expires_at,
            catalog_diagnostics,
            runtime_admission_witness: resolved
                .route()
                .runtime_admission_witness(
                    resolved.descriptor(),
                    resolved.model().remote_id(),
                    resolved.descriptor().profile(),
                )
                .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?,
            profile_revision: XAI_GROK_COMPATIBILITY_PROFILE.to_owned(),
        };
        let model = XaiLanguageModel::with_config(resolved, client, config)?;
        self.core.ensure_resolution_receipt_current(&receipt)?;
        drop(catalog);
        drop(resolution_lease);
        Ok((Arc::new(model), evidence))
    }

    /// Refreshes the selected login when its exact stored expiry enters skew.
    ///
    /// # Errors
    ///
    /// Ambiguous, invalid, account-changing, or persistence failures leave the
    /// durable credential quarantined.
    pub async fn refresh_if_needed(&self) -> Result<(), XaiNativeAuthError> {
        self.core.refresh_if_needed(None, None, None).await
    }

    /// Replaces the current credential with a local revoked tombstone.
    ///
    /// # Errors
    ///
    /// Rejects stale or missing state.
    pub fn logout(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        Ok(self.core.store.logout(fence.generation())?)
    }

    /// Revokes the refresh token at the fixed issuer and then writes a local tombstone.
    ///
    /// # Errors
    ///
    /// Any non-200 or ambiguous remote result leaves the selected slot quarantined.
    pub async fn revoke(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        self.core.revoke().await
    }

    fn begin_browser_login_flow(&self) -> Result<BrowserLoginFlow, XaiNativeAuthError> {
        self.core.ensure_active()?;
        let deadline = self.core.flow_deadline(LOGIN_TIMEOUT)?;
        let baseline = self.core.store.load()?;
        let pkce = PkceS256::generate()?;
        let binding = CallbackBinding::generate()?;
        let authorization_url = authorization_url(&self.core.endpoints, &pkce, &binding)?;
        self.core.ensure_flow_active(deadline)?;
        Ok(BrowserLoginFlow {
            core: Arc::clone(&self.core),
            baseline,
            pkce,
            binding,
            authorization_url,
            deadline,
        })
    }

    #[cfg(test)]
    fn open_test_slot(
        path: impl Into<PathBuf>,
        client: Client,
        issuer_origin: &str,
    ) -> Result<Self, XaiNativeAuthError> {
        let profile = auth_profile_id()?;
        let slot = CredentialSlotId::new("test-selected")?;
        let store = PrivateCredentialStore::open_slot(path, profile, slot)?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                store,
                client,
                acceptance: XaiCompatibilityAcceptance {
                    accepted_at: COMPATIBILITY_REVIEWED_UNIX,
                },
                endpoints: AuthEndpoints::test(issuer_origin),
                compatibility_expires_unix: AtomicU64::new(COMPATIBILITY_EXPIRES_UNIX),
                jwks: AsyncMutex::new(JwksCacheState::with_max_negative(MAX_JWKS_KEYS)),
                refresh_flight: AsyncMutex::new(()),
                catalog: Mutex::new(None),
                request_gate: None,
                refresh_observer: None,
                resolution_pause: Mutex::new(None),
            }),
        })
    }

    #[cfg(test)]
    fn test_subscription_route(
        &self,
        origin: crate::XaiOrigin,
    ) -> Result<XaiGrokSubscriptionRoute, XaiNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        let stored = self.core.read_stored(&fence)?;
        let receipt = self
            .core
            .active_catalog_receipt(&fence, &stored, system_unix_seconds()?)?;
        let parts = native_route_authority(&self.core, &receipt)?;
        XaiGrokSubscriptionRoute::from_native_authority_with_origin(
            parts.authority,
            parts.admission,
            origin,
        )
        .map_err(Into::into)
    }
}

impl fmt::Debug for XaiNativeAuthManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiNativeAuthManager")
            .field("store", &self.core.store)
            .field("profile", &XAI_GROK_COMPATIBILITY_PROFILE)
            .finish_non_exhaustive()
    }
}

/// Non-cloneable redacted browser-login capability.
///
/// ```compile_fail
/// # use plakous_xai::XaiBrowserLogin;
/// fn duplicate(handle: XaiBrowserLogin) {
///     let _ = handle.clone();
/// }
/// ```
pub struct XaiBrowserLogin {
    flow: BrowserLoginFlow,
    listener: TcpListener,
}

struct BrowserLoginFlow {
    core: Arc<ManagerCore>,
    baseline: Option<CredentialFence>,
    pkce: PkceS256,
    binding: CallbackBinding,
    authorization_url: Zeroizing<String>,
    deadline: Instant,
}

impl XaiBrowserLogin {
    /// Returns the exact xAI authorization URL the app should open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        self.flow.authorization_url()
    }

    /// Accepts bounded xAI browser preflights followed by one fixed-loopback
    /// callback, validates state/nonce/ID token and `UserInfo`, and atomically
    /// installs the selected account.
    ///
    /// # Errors
    ///
    /// Fails closed for callback, token, identity, scope, account, or storage errors.
    pub async fn complete(self) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        let Self { flow, listener } = self;
        flow.ensure_active()?;
        let mut hooks = XaiBrowserLoopbackHooks {
            deadline: flow.deadline,
            flow: Some(flow),
        };
        run_browser_loopback_session(listener, &mut hooks).await
    }
}

impl BrowserLoginFlow {
    fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    fn ensure_active(&self) -> Result<(), XaiNativeAuthError> {
        self.core.ensure_flow_active(self.deadline)
    }

    async fn complete_callback(
        self,
        callback: BrowserCallback,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.ensure_active()?;
        let code = match callback {
            BrowserCallback::AuthorizationCode { code, state } => {
                self.binding
                    .verify(&state, self.binding.expose_nonce())
                    .map_err(|_| XaiNativeAuthError::InvalidCallback)?;
                code
            }
            BrowserCallback::ProviderError { state } => {
                self.binding
                    .verify(&state, self.binding.expose_nonce())
                    .map_err(|_| XaiNativeAuthError::InvalidCallback)?;
                return Err(XaiNativeAuthError::AuthorizationDenied);
            }
        };
        until_flow_deadline(
            self.deadline,
            self.core.exchange_code(
                code,
                &self.pkce,
                Some(self.binding.expose_nonce()),
                self.baseline.as_ref(),
                self.deadline,
            ),
        )
        .await
    }
}

impl fmt::Debug for XaiBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiBrowserLogin(<redacted non-cloneable login>)")
    }
}

/// Non-cloneable redacted browser-login capability for pasted callbacks.
///
/// This handle owns the same PKCE verifier, state, nonce, selected-slot
/// baseline, and deadline as [`XaiBrowserLogin`], but owns no socket.
///
/// ```compile_fail
/// # use plakous_xai::XaiRemoteBrowserLogin;
/// fn duplicate(handle: XaiRemoteBrowserLogin) {
///     let _ = handle.clone();
/// }
/// ```
pub struct XaiRemoteBrowserLogin {
    flow: BrowserLoginFlow,
}

impl XaiRemoteBrowserLogin {
    /// Returns the exact xAI authorization URL the app should open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        self.flow.authorization_url()
    }

    /// Validates and consumes one pasted authorization input, then performs the
    /// shared token, identity, and selected-slot installation.
    ///
    /// Accepts either:
    /// - the complete loopback redirect URL (`http://127.0.0.1:<port>/callback?code=…&state=…`), or
    /// - a bare authorization `code` (paired with this attempt's owned `state`).
    ///
    /// The input is never included in an error or response. Full redirect URLs
    /// must still match the pinned callback grammar and attempt-owned state.
    /// Bare codes are validated as URL-safe tokens and bound to this attempt's
    /// stored state (same CSRF protection as a state-bearing redirect).
    ///
    /// # Errors
    ///
    /// Fails closed for callback, correlation, deadline, token, identity,
    /// scope, account, or storage errors.
    pub async fn complete(
        self,
        authorization_input: &str,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.flow.ensure_active()?;
        let callback = parse_remote_authorization_input(
            authorization_input,
            self.flow.binding.expose_state(),
            XAI_LOOPBACK_REDIRECT,
            xai_callback_policy(),
        )
        .map(oauth_callback_to_browser)
        .map_err(map_oauth_workflow_error)?;
        self.flow.complete_callback(callback).await
    }
}

impl fmt::Debug for XaiRemoteBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiRemoteBrowserLogin(<redacted non-cloneable login>)")
    }
}

/// Non-cloneable redacted RFC 8628 device-login capability.
pub struct XaiDeviceLogin {
    core: Arc<ManagerCore>,
    baseline: Option<CredentialFence>,
    device_code: SecretString,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval: Duration,
    deadline: Instant,
}

impl XaiDeviceLogin {
    /// Returns the short code intended for display to the user.
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Returns the provider verification URL.
    #[must_use]
    pub fn verification_uri(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }

    /// Polls under RFC 8628 `pending/slow_down` semantics and installs the login.
    ///
    /// # Errors
    ///
    /// Fails on denial, expiry, malformed responses, identity mismatch, or storage failure.
    pub async fn complete(mut self) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        let mut schedule = DevicePollSchedule::new(
            self.interval,
            Duration::from_secs(MAX_DEVICE_INTERVAL_SECONDS),
            DEVICE_SLOW_DOWN_INCREMENT,
            self.deadline,
        )
        .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        loop {
            self.core.ensure_flow_active(self.deadline)?;
            let form = urlencoded_form(&[
                ("grant_type", DEVICE_GRANT),
                ("client_id", XAI_PUBLIC_CLIENT_ID),
                ("device_code", self.device_code.expose_secret()),
            ]);
            match until_flow_deadline(
                self.deadline,
                self.core
                    .send_login_token_form_raw(self.core.endpoints.token(), form),
            )
            .await
            {
                Ok((status, response)) if status.is_success() => {
                    return until_flow_deadline(self.deadline, self.complete_success(response))
                        .await;
                }
                Ok((_status, response)) => {
                    self.core.ensure_flow_active(self.deadline)?;
                    let error: OAuthErrorResponse = decode_json(&response, MAX_AUTH_BODY_BYTES)
                        .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
                    match error.error.as_str() {
                        "authorization_pending" => {}
                        "slow_down" => {
                            schedule.apply_slow_down();
                            self.interval = schedule.interval();
                        }
                        "access_denied" | "authorization_denied" => {
                            return Err(XaiNativeAuthError::AuthorizationDenied);
                        }
                        "expired_token" => return Err(XaiNativeAuthError::LoginExpired),
                        _ => return Err(XaiNativeAuthError::LoginTokenExchange),
                    }
                }
                Err(error) => return Err(error),
            }
            self.core.ensure_flow_active(self.deadline)?;
            tokio::time::sleep(schedule.sleep_for(Instant::now())).await;
        }
    }

    async fn complete_success(
        &self,
        response: Zeroizing<Vec<u8>>,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.core.ensure_flow_active(self.deadline)?;
        let tokens: TokenResponse = decode_json(&response, MAX_TOKEN_BYTES)
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        let stored = self
            .core
            .validate_tokens(tokens, None, None, None, None, Some(self.deadline))
            .await?;
        self.core.ensure_flow_active(self.deadline)?;
        self.core
            .persist_login(self.baseline.as_ref(), &stored, Some(self.deadline))
    }
}

impl fmt::Debug for XaiDeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiDeviceLogin(<redacted non-cloneable login>)")
    }
}

struct ManagerCore {
    store: PrivateCredentialStore,
    client: Client,
    acceptance: XaiCompatibilityAcceptance,
    endpoints: AuthEndpoints,
    compatibility_expires_unix: AtomicU64,
    jwks: AsyncMutex<JwksCacheState<Jwk>>,
    refresh_flight: AsyncMutex<()>,
    catalog: Mutex<Option<CatalogReceiptCore>>,
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    refresh_observer: Option<Arc<dyn CredentialRefreshObserver>>,
    #[cfg(test)]
    resolution_pause: Mutex<Option<ResolutionPause>>,
}

/// Prepared refresh state carried through form-body claimed refresh.
struct XaiRefreshPrepared {
    expected_subject: AccountBinding,
    refresh_token: Zeroizing<String>,
    selection_epoch: String,
}

/// Shared-orchestrator hooks for SuperGrok claimed refresh (form body, ES256).
struct XaiRefreshHooks<'a> {
    /// Declared first so a dropped refresh restores its claim before the
    /// in-process flight is released.
    cancel_guard: XaiRefreshCancelGuard<'a>,
    core: &'a ManagerCore,
    expected_account: Option<&'a AccountBinding>,
    expected_selection_epoch: Option<&'a str>,
    expected_authority: Option<&'a str>,
    _flight: futures_util::lock::MutexGuard<'a, ()>,
    request_permit: Option<Box<dyn SubscriptionRequestPermit>>,
    started: Instant,
    /// Refresh even when the stored expiry is outside the lead (HTTP 401).
    force: bool,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRefreshHooks for XaiRefreshHooks<'_> {
    type Error = XaiNativeAuthError;
    type Prepared = XaiRefreshPrepared;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if self
            .expected_account
            .is_some_and(|account| account != fence.account_binding())
        {
            return Err(XaiNativeAuthError::AccountChanged);
        }
        if fence.status() == CredentialStatus::Ready {
            self.core.validate_expected_selection(
                &fence,
                self.expected_selection_epoch,
                self.expected_authority,
            )?;
        }
        if !self.force && !self.core.needs_refresh(&fence)? {
            return Ok(None);
        }
        // Ready still pre-reads under a Ready lease. Unknown/RefreshRequired keep
        // the payload but cannot acquire_ready; epoch is checked from the claim
        // payload in build_refresh_request.
        match fence.status() {
            CredentialStatus::Ready => {
                let _ = self.core.read_stored(&fence)?;
            }
            CredentialStatus::RefreshRequired | CredentialStatus::RefreshOutcomeUnknown => {}
            _ => return Err(XaiNativeAuthError::NotLoggedIn),
        }
        self.request_permit = self
            .core
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire(SubscriptionRequestClass::AuthRefresh))
            .transpose()
            .map_err(|_| XaiNativeAuthError::NotReady)?;
        Ok(Some(fence))
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
        self.cancel_guard.claimed(claim.fence());
        let mut old: StoredTokens = match serde_json::from_slice(claim.payload().expose_secret()) {
            Ok(old) => old,
            Err(_) => {
                return Err(BuildRequestFailure::LeaveQuarantined(
                    XaiNativeAuthError::InvalidStoredCredential,
                ));
            }
        };
        if let Err(error) = old.validate(claim.fence().account_binding()) {
            return Err(BuildRequestFailure::LeaveQuarantined(error));
        }
        if let Some(expected_epoch) = self.expected_selection_epoch
            && (old.selection_epoch != expected_epoch
                || self.expected_authority
                    != Some(route_credential_authority_id(&self.core.store).as_str()))
        {
            return Err(BuildRequestFailure::LeaveQuarantined(
                XaiNativeAuthError::SelectionChanged,
            ));
        }
        let expected_subject = claim.fence().account_binding().clone();
        let refresh_token = Zeroizing::new(std::mem::take(&mut old.refresh_token));
        let selection_epoch = old.selection_epoch.clone();
        let form = urlencoded_form(&refresh_token_form(&refresh_token, XAI_PUBLIC_CLIENT_ID));
        let request = match self.core.form_request(self.core.endpoints.token(), form) {
            Ok(request) => request,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        Ok((
            XaiRefreshPrepared {
                expected_subject,
                refresh_token,
                selection_epoch,
            },
            request,
        ))
    }

    async fn authorize_dispatch(&mut self) -> Result<(), Self::Error> {
        if let Some(permit) = &mut self.request_permit
            && permit.authorize_dispatch().is_err()
        {
            return Err(XaiNativeAuthError::NotReady);
        }
        Ok(())
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT)
    }

    async fn validate_tokens(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> Result<CredentialRecord, Self::Error> {
        let status = response.status();
        if status.is_success() {
            let response_body = pull_claimed_body(response, MAX_TOKEN_BYTES)
                .await
                .map_err(map_xai_pull_body_error)?;
            let tokens: TokenResponse = decode_json(&response_body, MAX_TOKEN_BYTES)?;
            let stored = self
                .core
                .validate_tokens(
                    tokens,
                    None,
                    Some(&prepared.expected_subject),
                    Some(&prepared.refresh_token),
                    Some(&prepared.selection_epoch),
                    None,
                )
                .await?;
            let replacement_generation = response
                .refresh_fence()
                .generation()
                .next()
                .map_err(XaiNativeAuthError::from)?;
            return credential_record(prepared.expected_subject, replacement_generation, &stored);
        }
        if status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error() {
            return Err(XaiNativeAuthError::Transport(None));
        }
        Err(XaiNativeAuthError::RefreshQuarantined)
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_xai_claimed_http_error(store, error)
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        // Complete failures historically surfaced as RefreshQuarantined; begin
        // and lease failures keep the Store(…) surface.
        match error {
            PrivateStoreError::HttpResponseBodyNotComplete
            | PrivateStoreError::GenerationNotSequential
            | PrivateStoreError::GenerationExhausted
            | PrivateStoreError::CredentialNotReady
            | PrivateStoreError::ForeignAccount
            | PrivateStoreError::InvalidRecord => XaiNativeAuthError::RefreshQuarantined,
            other => XaiNativeAuthError::Store(other),
        }
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.core.refresh_observer.as_deref()
    }

    fn reclaim_unknown_refresh(&self) -> bool {
        true
    }

    fn after_post_start_failure(&mut self, store: &PrivateCredentialStore, error: &Self::Error) {
        if !xai_refresh_failure_is_retryable(error) {
            return;
        }
        let failure_class = match error {
            XaiNativeAuthError::Transport(Some(cause)) if cause.timed_out => {
                CredentialRefreshFailureClass::Timeout
            }
            XaiNativeAuthError::Store(_) => CredentialRefreshFailureClass::Store,
            XaiNativeAuthError::NotReady => CredentialRefreshFailureClass::NotReady,
            XaiNativeAuthError::InvalidTokenResponse | XaiNativeAuthError::InvalidIdToken => {
                CredentialRefreshFailureClass::Validation
            }
            _ => CredentialRefreshFailureClass::Transport,
        };
        restore_xai_unknown_to_ready(
            store,
            self.core.refresh_observer.as_deref(),
            failure_class,
            self.started,
            match error {
                XaiNativeAuthError::Transport(Some(cause)) => Some(http_phase(cause.phase)),
                _ => None,
            },
        );
    }
}

/// Shared-orchestrator hooks for SuperGrok claimed remote revoke (form body).
struct XaiRevokeHooks<'a> {
    core: &'a ManagerCore,
    _flight: futures_util::lock::MutexGuard<'a, ()>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRevokeHooks for XaiRevokeHooks<'_> {
    type Error = XaiNativeAuthError;
    type Output = CredentialGeneration;

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn fence_to_revoke(&mut self) -> Result<CredentialFence, Self::Error> {
        self.core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)
    }

    fn build_revoke_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<Request, BuildRequestFailure<Self::Error>> {
        let mut old: StoredTokens = match serde_json::from_slice(claim.payload().expose_secret()) {
            Ok(old) => old,
            Err(_) => {
                return Err(BuildRequestFailure::LeaveQuarantined(
                    XaiNativeAuthError::InvalidStoredCredential,
                ));
            }
        };
        if let Err(error) = old.validate(claim.fence().account_binding()) {
            return Err(BuildRequestFailure::LeaveQuarantined(error));
        }
        let refresh = Zeroizing::new(std::mem::take(&mut old.refresh_token));
        let form = urlencoded_form(&[
            ("token", &refresh),
            ("token_type_hint", "refresh_token"),
            ("client_id", XAI_PUBLIC_CLIENT_ID),
        ]);
        self.core
            .form_request(self.core.endpoints.revocation(), form)
            .map_err(BuildRequestFailure::Restore)
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT)
    }

    async fn accept_revoke_response(
        &mut self,
        response: &mut ClaimedHttpResponse,
    ) -> Result<(), Self::Error> {
        if !response.status().is_success() || !claimed_response_is_json(response) {
            return Err(XaiNativeAuthError::RefreshQuarantined);
        }
        let response_body = pull_claimed_body(response, MAX_AUTH_BODY_BYTES)
            .await
            .map_err(map_xai_pull_body_error)?;
        if !response_body.is_empty()
            && decode_json::<RevokeResponse>(&response_body, MAX_AUTH_BODY_BYTES).is_err()
        {
            return Err(XaiNativeAuthError::RefreshQuarantined);
        }
        Ok(())
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_xai_claimed_http_error(store, error)
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        match error {
            PrivateStoreError::CredentialQuarantined
            | PrivateStoreError::CredentialNotReady
            | PrivateStoreError::CredentialRevoked
            | PrivateStoreError::RecordMissing
            | PrivateStoreError::FenceMismatch
            | PrivateStoreError::StoreBusy
            | PrivateStoreError::LockFailed => XaiNativeAuthError::Store(error),
            _ => XaiNativeAuthError::RefreshQuarantined,
        }
    }

    fn output_from_revoked(
        &mut self,
        generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error> {
        Ok(generation)
    }

    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.core.refresh_observer.as_deref()
    }
}

pub(crate) fn network_failure(failure: Failure) -> Option<plakous_model::ModelNetworkFailure> {
    use plakous_http::{ErrorKind, Phase, TransmissionFact};
    use plakous_model::{ModelNetworkFailure, ModelNetworkTransmission, ModelRetryPhase};
    let phase = match failure.phase() {
        Phase::Resolution => ModelRetryPhase::Resolution,
        Phase::Connect => ModelRetryPhase::Connect,
        Phase::Tls => ModelRetryPhase::Tls,
        Phase::RequestWrite => ModelRetryPhase::RequestWrite,
        Phase::RequestWriteOrResponseHead => ModelRetryPhase::RequestWriteOrResponseHead,
        Phase::ResponseHead => ModelRetryPhase::ResponseHead,
        Phase::ResponseBody => ModelRetryPhase::ResponseBody,
        Phase::Cleanup => ModelRetryPhase::Cleanup,
        _ => return None,
    };
    Some(ModelNetworkFailure {
        phase,
        sent: match failure.transmission() {
            TransmissionFact::NotStarted => ModelNetworkTransmission::NotSent,
            TransmissionFact::RequestIncomplete => ModelNetworkTransmission::Incomplete,
            TransmissionFact::MayHaveTransmitted => ModelNetworkTransmission::MayHaveSent,
        },
        timed_out: failure.kind() == ErrorKind::DeadlineExceeded,
    })
}

fn map_xai_claimed_http_error(
    _store: &PrivateCredentialStore,
    error: ClaimedHttpError,
) -> XaiNativeAuthError {
    match error.disposition() {
        ClaimedHttpDisposition::RestoredNotStarted
        | ClaimedHttpDisposition::QuarantinedMayHaveTransmitted => {
            XaiNativeAuthError::Transport(error.transport_failure().and_then(network_failure))
        }
        ClaimedHttpDisposition::RestorationFailed | ClaimedHttpDisposition::ClaimRejected => {
            XaiNativeAuthError::RefreshQuarantined
        }
        ClaimedHttpDisposition::QuarantineVerificationFailed => {
            XaiNativeAuthError::RefreshQuarantined
        }
        _ => XaiNativeAuthError::RefreshQuarantined,
    }
}

fn xai_refresh_failure_is_retryable(error: &XaiNativeAuthError) -> bool {
    matches!(
        error,
        XaiNativeAuthError::Transport(_)
            | XaiNativeAuthError::InvalidTokenResponse
            | XaiNativeAuthError::InvalidIdToken
            | XaiNativeAuthError::NotReady
            // Local lock contention and lock-identity failures prove nothing
            // about the provider's view of the grant.
            | XaiNativeAuthError::Store(
                PrivateStoreError::StoreBusy | PrivateStoreError::LockFailed
            )
    )
}

fn http_phase(phase: plakous_model::ModelRetryPhase) -> plakous_http::Phase {
    use plakous_http::Phase as H;
    use plakous_model::ModelRetryPhase as M;
    match phase {
        M::Resolution => H::Resolution,
        M::Connect => H::Connect,
        M::Tls => H::Tls,
        M::RequestWrite => H::RequestWrite,
        M::RequestWriteOrResponseHead => H::RequestWriteOrResponseHead,
        M::ResponseHead => H::ResponseHead,
        M::ResponseBody => H::ResponseBody,
        M::Cleanup => H::Cleanup,
    }
}

/// Restores a durable xAI Unknown record to Ready on the same refresh token
/// and reports the restoration to the credential-lifecycle observer, so
/// `plak doctor`'s latest refresh row reflects the real last transition.
fn restore_xai_unknown_to_ready(
    store: &PrivateCredentialStore,
    observer: Option<&dyn CredentialRefreshObserver>,
    failure_class: CredentialRefreshFailureClass,
    started: Instant,
    transport_phase: Option<plakous_http::Phase>,
) {
    let Ok(Some(fence)) = store.load() else {
        return;
    };
    if fence.status() != CredentialStatus::RefreshOutcomeUnknown {
        return;
    }
    let generation = fence.generation().get();
    if store.restore_unknown_to_ready(&fence).is_err() {
        return;
    }
    if let Some(observer) = observer {
        observer.observe(CredentialRefreshEvent {
            phase: CredentialRefreshPhase::Restored,
            failure_class: Some(failure_class),
            transport_phase,
            http_status: None,
            elapsed: started.elapsed(),
            generation,
        });
    }
}

/// Restores this refresh's own claimed Unknown record when the xAI refresh
/// future is dropped without finishing (inference cancellation, before or
/// after the POST). It does nothing until `begin_refresh` has written the
/// claim, restores only that exact fence, never a peer's, and never blocks:
/// on store contention it leaves Unknown, which the next xAI refresh
/// reclaims. Every completed ladder exit calls [`Self::finished`] first so a
/// terminal Unknown stays put.
struct XaiRefreshCancelGuard<'a> {
    store: &'a PrivateCredentialStore,
    observer: Option<&'a dyn CredentialRefreshObserver>,
    started: Instant,
    claimed: Option<CredentialFence>,
}

impl<'a> XaiRefreshCancelGuard<'a> {
    fn new(
        store: &'a PrivateCredentialStore,
        observer: Option<&'a dyn CredentialRefreshObserver>,
    ) -> Self {
        Self {
            store,
            observer,
            started: Instant::now(),
            claimed: None,
        }
    }

    fn claimed(&mut self, unknown: CredentialFence) {
        self.claimed = Some(unknown);
    }

    fn finished(&mut self) {
        self.claimed = None;
    }
}

impl Drop for XaiRefreshCancelGuard<'_> {
    fn drop(&mut self) {
        let Some(claimed) = self.claimed.take() else {
            return;
        };
        let Ok(Some(_)) = self.store.try_restore_unknown_to_ready(&claimed) else {
            return;
        };
        if let Some(observer) = self.observer {
            observer.observe(CredentialRefreshEvent {
                phase: CredentialRefreshPhase::Restored,
                failure_class: Some(CredentialRefreshFailureClass::Cancelled),
                transport_phase: None,
                http_status: None,
                elapsed: self.started.elapsed(),
                generation: claimed.generation().get(),
            });
        }
    }
}

fn map_xai_pull_body_error(error: PullBodyError) -> XaiNativeAuthError {
    match error {
        PullBodyError::Transport => XaiNativeAuthError::Transport(None),
        PullBodyError::TooLarge => XaiNativeAuthError::InvalidTokenResponse,
        _ => XaiNativeAuthError::Transport(None),
    }
}

impl ManagerCore {
    fn ensure_active(&self) -> Result<(), XaiNativeAuthError> {
        validate_acceptance_until(
            &self.acceptance,
            system_unix_seconds()?,
            self.compatibility_expires_unix(),
        )
    }

    fn compatibility_expires_unix(&self) -> u64 {
        self.compatibility_expires_unix.load(Ordering::Relaxed)
    }

    fn ensure_flow_active(&self, deadline: Instant) -> Result<(), XaiNativeAuthError> {
        self.ensure_active()?;
        remaining_until(deadline, Instant::now()).ok_or(XaiNativeAuthError::LoginExpired)?;
        Ok(())
    }

    fn flow_deadline(&self, maximum: Duration) -> Result<Instant, XaiNativeAuthError> {
        self.ensure_active()?;
        let monotonic_now = Instant::now();
        let now = system_unix_elapsed()?;
        let expiry = Duration::from_secs(self.compatibility_expires_unix());
        let remaining = expiry
            .checked_sub(now)
            .filter(|remaining| !remaining.is_zero())
            .ok_or(XaiNativeAuthError::CompatibilityInactive)?;
        monotonic_now
            .checked_add(maximum.min(remaining))
            .ok_or(XaiNativeAuthError::LoginExpired)
    }

    async fn exchange_code(
        &self,
        code: Zeroizing<String>,
        pkce: &PkceS256,
        nonce: Option<&str>,
        baseline: Option<&CredentialFence>,
        deadline: Instant,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.ensure_flow_active(deadline)?;
        let form = urlencoded_form(&authorization_code_token_form(
            code.as_str(),
            XAI_LOOPBACK_REDIRECT,
            XAI_PUBLIC_CLIENT_ID,
            pkce.verifier().expose_secret(),
        ));
        let response = self
            .send_login_token_form(self.endpoints.token(), form)
            .await?;
        self.ensure_flow_active(deadline)?;
        let tokens: TokenResponse = decode_json(&response, MAX_TOKEN_BYTES)
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        let stored = self
            .validate_tokens(tokens, nonce, None, None, None, Some(deadline))
            .await?;
        self.ensure_flow_active(deadline)?;
        self.persist_login(baseline, &stored, Some(deadline))
    }

    fn ensure_login_validation_active(
        &self,
        flow_deadline: Option<Instant>,
    ) -> Result<(), XaiNativeAuthError> {
        if let Some(deadline) = flow_deadline {
            self.ensure_flow_active(deadline)
        } else {
            self.ensure_active()
        }
    }

    async fn refresh_catalog_core(&self) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        self.refresh_catalog_with(None).await
    }

    /// Fetches the authenticated catalog. A renewal binds both the refresh
    /// and the fetch to its exact selection and caps the receipt's
    /// observation at its callers' check time (see [`CatalogRenewal`]).
    async fn refresh_catalog_with(
        &self,
        renewal: Option<&CatalogRenewal<'_>>,
    ) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        self.ensure_active()?;
        match self
            .refresh_if_needed(
                renewal.map(|renewal| renewal.account),
                renewal.map(|renewal| renewal.selection_epoch),
                renewal.map(|renewal| renewal.authority_id),
            )
            .await
        {
            Ok(()) => {}
            Err(XaiNativeAuthError::NotReady) => {
                let fence = self.store.load()?.ok_or(XaiNativeAuthError::NotLoggedIn)?;
                if fence.status() != CredentialStatus::Ready {
                    return Err(XaiNativeAuthError::NotReady);
                }
            }
            Err(error) => return Err(error),
        }
        let mut request_permit = self
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire(SubscriptionRequestClass::Catalog))
            .transpose()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
        let fence = self.store.load()?.ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(XaiNativeAuthError::NotReady);
        }
        let lease = self.store.acquire_ready(&fence)?;
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(fence.account_binding())?;
        if let Some(renewal) = renewal
            && (fence.account_binding() != renewal.account
                || stored.selection_epoch != renewal.selection_epoch
                || route_credential_authority_id(&self.store) != renewal.authority_id)
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if let Some(permit) = &mut request_permit {
            permit
                .authorize_dispatch()
                .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
        }
        let requested_at = system_unix_seconds()?;
        let response = self
            .get(
                self.endpoints.catalog(),
                Some(&stored.access_token),
                Some(fence.account_binding()),
            )
            .await?;
        let catalog: CatalogResponse = decode_json(&response, MAX_AUTH_BODY_BYTES)?;
        let admitted_entries = catalog.admitted_coding_entries()?;
        let now = system_unix_seconds()?;
        let observed = renewal.map_or(now, |renewal| renewal.observed_seconds(requested_at));
        let expires = observed
            .checked_add(CATALOG_TTL_SECONDS)
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?
            .min(COMPATIBILITY_EXPIRES_UNIX);
        if expires <= now {
            return Err(XaiNativeAuthError::CompatibilityInactive);
        }
        let public = XaiNativeCatalogReceipt {
            entries: admitted_entries,
            account: fence.account_binding().clone(),
            observed_at: Timestamp::new(observed, 0)?,
            expires_at: Timestamp::new(expires, 0)?,
        };
        let receipt = CatalogReceiptCore {
            public: public.clone(),
            fence: fence.clone(),
            selection_epoch: stored.selection_epoch.clone(),
            authority_id: route_credential_authority_id(&self.store),
        };
        *self
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)? = Some(receipt.clone());
        drop(lease);
        Ok(receipt)
    }

    fn acquire_resolution_lease(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<ReadyCredentialLease, XaiNativeAuthError> {
        self.ensure_resolution_receipt_current(receipt)?;
        let current = self.store.load()?.ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if current != receipt.fence || current.status() != CredentialStatus::Ready {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let lease = self
            .store
            .acquire_ready(&receipt.fence)
            .map_err(|_| XaiNativeAuthError::SelectionChanged)?;
        if lease.fence() != receipt.fence {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(receipt.fence.account_binding())?;
        if stored.selection_epoch != receipt.selection_epoch
            || receipt.authority_id != route_credential_authority_id(&self.store)
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        Ok(lease)
    }

    fn ensure_resolution_receipt_current(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<(), XaiNativeAuthError> {
        self.ensure_active()?;
        let now = system_unix_seconds()?;
        if !valid_admitted_catalog_entries(&receipt.public.entries)
            || receipt.fence.status() != CredentialStatus::Ready
            || receipt.fence.profile_id().as_str() != XAI_GROK_COMPATIBILITY_PROFILE
            || receipt.public.account != *receipt.fence.account_binding()
            || receipt.public.observed_at.seconds() > now
            || receipt.public.expires_at.seconds() <= now
            || receipt.public.expires_at.seconds() > COMPATIBILITY_EXPIRES_UNIX
            || !valid_selection_epoch(&receipt.selection_epoch)
            || receipt.authority_id != route_credential_authority_id(&self.store)
        {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    #[cfg(test)]
    async fn pause_resolution_after_catalog(&self) -> Result<(), XaiNativeAuthError> {
        let pause = self
            .resolution_pause
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
            .clone();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.resume.notified().await;
        }
        Ok(())
    }

    fn active_catalog_receipt(
        &self,
        fence: &CredentialFence,
        stored: &StoredTokens,
        now: u64,
    ) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        let receipt = self
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
            .clone()
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?;
        if !valid_admitted_catalog_entries(&receipt.public.entries)
            || receipt.public.account != *fence.account_binding()
            || receipt.fence != *fence
            || receipt.selection_epoch != stored.selection_epoch
            || receipt.authority_id != route_credential_authority_id(&self.store)
            || receipt.public.observed_at.seconds() > now
            || receipt.public.expires_at.seconds() <= now
            || receipt.public.expires_at.seconds() > COMPATIBILITY_EXPIRES_UNIX
        {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(receipt)
    }

    fn persist_login(
        &self,
        baseline: Option<&CredentialFence>,
        stored: &StoredTokens,
        flow_deadline: Option<Instant>,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.ensure_login_validation_active(flow_deadline)?;
        let account = AccountBinding::new(stored.subject.clone())?;
        let generation = match baseline {
            Some(fence) => fence.generation().next()?,
            None => CredentialGeneration::new(1)?,
        };
        let record = credential_record(account.clone(), generation, stored)?;
        self.ensure_login_validation_active(flow_deadline)?;
        let fence = if let Some(expected) = baseline {
            self.core_replace_selected(expected, record)
                .map_err(|_| XaiNativeAuthError::LoginPersistence)?
        } else {
            self.store
                .initialize_selected(&record)
                .map_err(|_| XaiNativeAuthError::LoginPersistence)?;
            record.fence()
        };
        Ok(XaiSubscriptionStatus {
            generation: fence.generation(),
            status: fence.status(),
            account,
        })
    }

    fn core_replace_selected(
        &self,
        expected: &CredentialFence,
        record: CredentialRecord,
    ) -> Result<CredentialFence, XaiNativeAuthError> {
        self.store
            .replace_selected_login(expected, record)
            .map_err(Into::into)
    }

    async fn refresh_if_needed(
        &self,
        expected_account: Option<&AccountBinding>,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
    ) -> Result<(), XaiNativeAuthError> {
        self.refresh_with_force(
            expected_account,
            expected_selection_epoch,
            expected_authority,
            false,
        )
        .await
    }

    /// Runs the claimed refresh ladder; `force` claims a new generation even
    /// when the stored expiry is not yet inside the refresh lead (the provider
    /// rejected the access token with HTTP 401).
    async fn refresh_with_force(
        &self,
        expected_account: Option<&AccountBinding>,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
        force: bool,
    ) -> Result<(), XaiNativeAuthError> {
        let flight = self.refresh_flight.lock().await;
        let mut hooks = XaiRefreshHooks {
            cancel_guard: XaiRefreshCancelGuard::new(&self.store, self.refresh_observer.as_deref()),
            core: self,
            expected_account,
            expected_selection_epoch,
            expected_authority,
            _flight: flight,
            request_permit: None,
            started: Instant::now(),
            force,
        };
        let result = run_claimed_refresh(&self.store, &self.client, &mut hooks).await;
        hooks.cancel_guard.finished();
        result
    }

    fn validate_expected_selection(
        &self,
        fence: &CredentialFence,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
    ) -> Result<(), XaiNativeAuthError> {
        let Some(expected_epoch) = expected_selection_epoch else {
            return Ok(());
        };
        let stored = self.read_stored(fence)?;
        if stored.selection_epoch != expected_epoch
            || expected_authority != Some(route_credential_authority_id(&self.store).as_str())
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        Ok(())
    }

    fn ensure_route_current(
        &self,
        expected_receipt: &CatalogReceiptCore,
        witness_failures: bool,
    ) -> Result<(), XaiNativeAuthError> {
        self.ensure_active().inspect_err(|_| {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.catalog");
            }
        })?;
        let fence = self
            .store
            .load()
            .inspect_err(|_| {
                if witness_failures {
                    crate::route::authority_revalidation_witness("native.route_current.fence_read");
                }
            })?
            .ok_or_else(|| {
                if witness_failures {
                    crate::route::authority_revalidation_witness(
                        "native.route_current.fence_missing",
                    );
                }
                XaiNativeAuthError::NotLoggedIn
            })?;
        if fence.status() != CredentialStatus::Ready {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.fence_status");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence.generation() != expected_receipt.fence.generation() {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.generation");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence.account_binding() != &expected_receipt.public.account {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.account");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence != expected_receipt.fence {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.fence_binding");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let stored = self.read_stored(&expected_receipt.fence).inspect_err(|_| {
            if witness_failures {
                crate::route::authority_revalidation_witness(
                    "native.route_current.stored_selection",
                );
            }
        })?;
        self.validate_expected_selection(
            &fence,
            Some(&expected_receipt.selection_epoch),
            Some(&expected_receipt.authority_id),
        )
        .inspect_err(|_| {
            if witness_failures {
                crate::route::authority_revalidation_witness(
                    "native.route_current.stored_selection",
                );
            }
        })?;
        let now = system_unix_seconds().inspect_err(|_| {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.clock");
            }
        })?;
        let active = self
            .active_catalog_receipt(&fence, &stored, now)
            .inspect_err(|_| {
                if witness_failures {
                    crate::route::authority_revalidation_witness("native.route_current.catalog");
                }
            })?;
        if active != *expected_receipt {
            if witness_failures {
                crate::route::authority_revalidation_witness("native.route_current.catalog");
            }
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    fn ensure_route_receipt_current_while_leased(
        &self,
        expected_receipt: &CatalogReceiptCore,
    ) -> Result<(), XaiNativeAuthError> {
        // The caller retains the exact ReadyCredentialLease acquired for the
        // authorization header. Reopening the selected-slot file here would
        // recursively contend on that same inode and is unnecessary: the
        // shared lease itself fences relogin/rotation. Revalidate the complete
        // receipt identity and its live time bounds under the catalog lock.
        self.ensure_resolution_receipt_current(expected_receipt)?;
        let catalog = self
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
        if catalog.as_ref() != Some(expected_receipt) {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    fn needs_refresh(&self, fence: &CredentialFence) -> Result<bool, XaiNativeAuthError> {
        if fence.status() != CredentialStatus::Ready {
            return Ok(true);
        }
        let stored = self.read_stored(fence)?;
        let now = system_unix_seconds()?;
        Ok(access_needs_refresh(
            stored.expires_at,
            now,
            ACCESS_REFRESH_LEAD_SECONDS,
        ))
    }

    fn read_stored(&self, fence: &CredentialFence) -> Result<StoredTokens, XaiNativeAuthError> {
        let lease = self.store.acquire_ready(fence)?;
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(fence.account_binding())?;
        Ok(stored)
    }

    fn acquire_grant(
        &self,
        expected_receipt: &CatalogReceiptCore,
    ) -> Result<crate::XaiSubscriptionCredentialGrant, crate::XaiAuthError> {
        let fence = self
            .store
            .load()
            .map_err(|_| crate::XaiAuthError::CredentialSource)?
            .ok_or(crate::XaiAuthError::MissingCredential)?;
        if fence != expected_receipt.fence
            || fence.account_binding() != &expected_receipt.public.account
            || fence.status() != CredentialStatus::Ready
        {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let lease = self
            .store
            .acquire_ready(&expected_receipt.fence)
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        if lease.fence() != expected_receipt.fence {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let mut stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        stored
            .validate(&expected_receipt.public.account)
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        if stored.selection_epoch != expected_receipt.selection_epoch
            || route_credential_authority_id(&self.store) != expected_receipt.authority_id
        {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let now = system_unix_seconds().map_err(|_| crate::XaiAuthError::CredentialSource)?;
        let active = self
            .active_catalog_receipt(&fence, &stored, now)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)?;
        if active != *expected_receipt {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        if access_needs_refresh(stored.expires_at, now, ACCESS_REFRESH_LEAD_SECONDS) {
            return Err(crate::XaiAuthError::MissingCredential);
        }
        let access = SecretString::from(std::mem::take(&mut stored.access_token));
        Ok(crate::XaiSubscriptionCredentialGrant::native(
            &access, lease,
        ))
    }

    async fn revoke(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        self.ensure_active()?;
        self.revoke_current().await
    }

    #[cfg(test)]
    async fn revoke_at(&self, now: u64) -> Result<CredentialGeneration, XaiNativeAuthError> {
        ensure_compatibility(now)?;
        self.revoke_current().await
    }

    async fn revoke_current(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        let flight = self.refresh_flight.lock().await;
        let mut hooks = XaiRevokeHooks {
            core: self,
            _flight: flight,
        };
        run_claimed_revoke(&self.store, &self.client, &mut hooks).await
    }

    async fn send_form(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let (status, response) = self.send_form_raw(endpoint, body).await?;
        if !status.is_success() {
            return Err(status_error(status));
        }
        Ok(response)
    }

    async fn send_login_token_form(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let (status, response) = self.send_login_token_form_raw(endpoint, body).await?;
        if !status.is_success() {
            return Err(XaiNativeAuthError::LoginTokenExchange);
        }
        Ok(response)
    }

    async fn send_login_token_form_raw(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<(StatusCode, Zeroizing<Vec<u8>>), XaiNativeAuthError> {
        let request = self
            .form_request(endpoint, body)
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        let (status, _is_json, response) = self
            .execute(request)
            .await
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        // Token responses: media-type is optional for provider compatibility;
        // body JSON is still fail-closed via decode.
        Ok((status, response))
    }

    async fn send_form_raw(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<(StatusCode, Zeroizing<Vec<u8>>), XaiNativeAuthError> {
        let request = self.form_request(endpoint, body)?;
        let (status, _, response) = self.execute(request).await?;
        Ok((status, response))
    }

    fn form_request(
        &self,
        endpoint: &Endpoint,
        mut body: Zeroizing<String>,
    ) -> Result<Request, XaiNativeAuthError> {
        let mut headers = fixed_headers("application/x-www-form-urlencoded");
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        Request::new(
            Origin::parse(&endpoint.origin)?,
            Method::POST,
            &endpoint.path,
            headers,
            SensitiveBytes::new(std::mem::take(&mut *body).into_bytes()).into_bytes(),
            self.client.limits(),
        )
        .map_err(Into::into)
    }

    async fn get(
        &self,
        endpoint: &Endpoint,
        bearer_token: Option<&str>,
        catalog_subject: Option<&AccountBinding>,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let mut headers = fixed_headers("application/json");
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(token) = bearer_token {
            headers.insert(HEADER_AUTHORIZATION, bearer_header(token)?);
        }
        if endpoint.path == CATALOG_PATH {
            insert_subscription_catalog_headers(
                &mut headers,
                catalog_subject.ok_or(XaiNativeAuthError::CatalogUnavailable)?,
            )?;
        } else if catalog_subject.is_some() {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let request = Request::new(
            Origin::parse(&endpoint.origin)?,
            Method::GET,
            &endpoint.path,
            headers,
            Bytes::new(),
            self.client.limits(),
        )?;
        let (status, is_json, response) = self.execute(request).await?;
        if status != StatusCode::OK {
            return Err(status_error(status));
        }
        if !is_json {
            return Err(XaiNativeAuthError::Transport(None));
        }
        Ok(response)
    }

    async fn get_login_json(
        &self,
        endpoint: &Endpoint,
        bearer_token: Option<&str>,
        stage: LoginHttpStage,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let mut headers = fixed_headers("application/json");
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(token) = bearer_token {
            headers.insert(
                HEADER_AUTHORIZATION,
                bearer_header(token).map_err(|_| stage.error())?,
            );
        }
        let origin = Origin::parse(&endpoint.origin).map_err(|_| stage.error())?;
        let request = Request::new(
            origin,
            Method::GET,
            &endpoint.path,
            headers,
            Bytes::new(),
            self.client.limits(),
        )
        .map_err(|_| stage.error())?;
        let (status, is_json, response) = self.execute(request).await.map_err(|_| stage.error())?;
        if !status.is_success() || !is_json {
            return Err(stage.error());
        }
        Ok(response)
    }

    async fn execute(
        &self,
        request: Request,
    ) -> Result<(StatusCode, bool, Zeroizing<Vec<u8>>), AuthTransportError> {
        let control = ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(AuthTransportError::Failure)?;
        let status = response.status();
        let is_json = headers_are_json(response.headers());
        let mut body = response.into_body();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk.map_err(AuthTransportError::Failure)?;
            if bytes.len().saturating_add(chunk.len()) > MAX_AUTH_BODY_BYTES {
                let _ = body.close().await;
                return Err(AuthTransportError::limit());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, is_json, bytes))
    }
}

/// A renewal's catalog fetch: the exact selection the refresh and the fetch
/// must still name, and the floor of its callers' check times.
///
/// A renewed receipt answers revalidations that read their clock before the
/// refresh and catalog round trip, so it claims observation at the request
/// start or at the earliest waiting `checked_at`, whichever is earlier, in
/// whole seconds. Its expiry runs from that stamp, so the window is never
/// extended; credential freshness stays bounded by the stored access expiry.
struct CatalogRenewal<'a> {
    account: &'a AccountBinding,
    selection_epoch: &'a str,
    authority_id: &'a str,
    floor: &'a crate::XaiRenewalFloor,
}

impl CatalogRenewal<'_> {
    fn observed_seconds(&self, requested_at: u64) -> u64 {
        self.floor
            .earliest()
            .map_or(requested_at, |floor| floor.seconds().min(requested_at))
    }
}

/// Binds one exact catalog receipt to a new immutable route authority.
fn native_route_authority(
    core: &Arc<ManagerCore>,
    receipt: &CatalogReceiptCore,
) -> Result<crate::XaiRenewedAuthority, XaiNativeAuthError> {
    let admission = crate::NativeXaiCatalogAdmission::new(
        receipt.public.entries.clone(),
        GrokCodingProfile::pinned_compatibility().profile_id()?,
        receipt.public.observed_at,
        receipt.public.expires_at,
        receipt.fence.generation().get(),
        receipt.selection_epoch.clone(),
    );
    Ok(crate::XaiRenewedAuthority {
        authority: Arc::new(NativeRouteAuthority {
            core: Arc::clone(core),
            expected_receipt: receipt.clone(),
        }),
        admission,
    })
}

struct NativeRouteAuthority {
    core: Arc<ManagerCore>,
    expected_receipt: CatalogReceiptCore,
}

impl NativeRouteAuthority {
    async fn route_refresh(&self, force: bool) -> Result<(), crate::XaiAuthError> {
        self.core
            .refresh_with_force(
                Some(&self.expected_receipt.public.account),
                Some(&self.expected_receipt.selection_epoch),
                Some(&self.expected_receipt.authority_id),
                force,
            )
            .await
            .map_err(|error| match error {
                XaiNativeAuthError::SelectionChanged | XaiNativeAuthError::AccountChanged => {
                    crate::XaiAuthError::BindingMismatch
                }
                XaiNativeAuthError::NotLoggedIn | XaiNativeAuthError::NotReady => {
                    crate::XaiAuthError::MissingCredential
                }
                _ => crate::XaiAuthError::CredentialSource,
            })
    }

    /// The claimed refresh and the authenticated catalog resolution
    /// `resolve_model_with_evidence` performs, both bound to this exact
    /// selection, then its resolution lease. The new receipt must keep the
    /// account, selection epoch and credential authority; only the
    /// generation may advance.
    async fn renew_route(
        &self,
        floor: &crate::XaiRenewalFloor,
    ) -> Result<crate::XaiRenewedAuthority, crate::XaiAuthError> {
        let transient = |_| crate::XaiAuthError::CredentialSource;
        let expected = &self.expected_receipt;
        let receipt = self
            .core
            .refresh_catalog_with(Some(&CatalogRenewal {
                account: &expected.public.account,
                selection_epoch: &expected.selection_epoch,
                authority_id: &expected.authority_id,
                floor,
            }))
            .await
            .map_err(|error| match error {
                XaiNativeAuthError::SelectionChanged | XaiNativeAuthError::AccountChanged => {
                    crate::XaiAuthError::BindingMismatch
                }
                _ => crate::XaiAuthError::CredentialSource,
            })?;
        if receipt.public.account != expected.public.account
            || receipt.selection_epoch != expected.selection_epoch
            || receipt.authority_id != expected.authority_id
            || receipt.fence.generation() < expected.fence.generation()
        {
            crate::route::authority_revalidation_witness("native.renew.binding");
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let resolution_lease = self
            .core
            .acquire_resolution_lease(&receipt)
            .map_err(transient)?;
        let catalog = self
            .core
            .catalog
            .lock()
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        if catalog.as_ref() != Some(&receipt) {
            return Err(crate::XaiAuthError::CredentialSource);
        }
        let renewed = native_route_authority(&self.core, &receipt).map_err(transient)?;
        self.core
            .ensure_resolution_receipt_current(&receipt)
            .map_err(transient)?;
        drop(catalog);
        drop(resolution_lease);
        Ok(renewed)
    }
}

impl crate::XaiSubscriptionCredentialAuthority for NativeRouteAuthority {
    fn account_binding(&self) -> &str {
        self.expected_receipt.public.account.as_str()
    }

    fn credential_authority(&self) -> &str {
        &self.expected_receipt.authority_id
    }

    fn credential_generation(&self) -> u64 {
        self.expected_receipt.fence.generation().get()
    }

    fn selection_epoch(&self) -> &str {
        &self.expected_receipt.selection_epoch
    }

    fn revalidate_runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<crate::XaiRevalidatedSubscriptionAuthority, RuntimeModelAuthorityError> {
        self.core
            .ensure_route_current(&self.expected_receipt, true)
            .map_err(|_| RuntimeModelAuthorityError::Stale)?;
        if checked_at < self.expected_receipt.public.observed_at
            || checked_at >= self.expected_receipt.public.expires_at
        {
            crate::route::authority_revalidation_witness("native.catalog_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let stored = self
            .core
            .read_stored(&self.expected_receipt.fence)
            .map_err(|_| {
                crate::route::authority_revalidation_witness("native.stored_selection");
                RuntimeModelAuthorityError::Stale
            })?;
        if stored.selection_epoch != self.expected_receipt.selection_epoch {
            crate::route::authority_revalidation_witness("native.stored_selection");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let current_unix =
            system_unix_seconds().map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        if access_needs_refresh(stored.expires_at, current_unix, ACCESS_REFRESH_LEAD_SECONDS) {
            crate::route::authority_revalidation_witness("native.credential_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let credential_valid_until = Timestamp::new(
            stored
                .expires_at
                .saturating_sub(ACCESS_REFRESH_LEAD_SECONDS),
            0,
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        if credential_valid_until <= checked_at {
            crate::route::authority_revalidation_witness("native.credential_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        Ok(crate::XaiRevalidatedSubscriptionAuthority::new(
            credential_valid_until,
        ))
    }

    fn refresh_if_needed(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), crate::XaiAuthError>> + Send + '_>> {
        Box::pin(self.route_refresh(false))
    }

    fn refresh_after_rejection(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), crate::XaiAuthError>> + Send + '_>> {
        Box::pin(self.route_refresh(true))
    }

    fn renew<'a>(
        &'a self,
        floor: &'a crate::XaiRenewalFloor,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<crate::XaiRenewedAuthority, crate::XaiAuthError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(self.renew_route(floor))
    }

    fn preflight(&self) -> Result<(), crate::XaiAuthError> {
        self.core
            .ensure_route_current(&self.expected_receipt, false)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)
    }

    fn acquire(&self) -> Result<crate::XaiSubscriptionCredentialGrant, crate::XaiAuthError> {
        self.core.acquire_grant(&self.expected_receipt)
    }

    fn acquire_inference_permit(
        &self,
        input_liability: u64,
    ) -> Result<Option<Box<dyn SubscriptionRequestPermit>>, crate::XaiAuthError> {
        self.core
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire_inference(input_liability))
            .transpose()
            .map_err(|_| crate::XaiAuthError::CredentialSource)
    }

    fn revalidate(
        &self,
        _outgoing: &crate::XaiSubscriptionCredentialGrant,
    ) -> Result<(), crate::XaiAuthError> {
        self.core
            .ensure_route_receipt_current_while_leased(&self.expected_receipt)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)
    }
}

impl fmt::Debug for NativeRouteAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeRouteAuthority(<redacted selected slot>)")
    }
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

/// Redaction-safe native authentication failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum XaiNativeAuthError {
    /// The caller did not explicitly accept the exact pinned revision.
    #[error("the exact xAI compatibility profile was not accepted")]
    CompatibilityNotAccepted,
    /// The pinned compatibility profile expired or was killed in code.
    #[error("the pinned xAI compatibility profile is inactive")]
    CompatibilityInactive,
    /// Required local randomness was unavailable.
    #[error("secure randomness is unavailable")]
    RandomnessUnavailable,
    /// The selected slot has no credential.
    #[error("the selected xAI slot is not logged in")]
    NotLoggedIn,
    /// The selected slot is not Ready.
    #[error("the selected xAI credential is not ready")]
    NotReady,
    /// The fixed browser callback socket could not be acquired.
    #[error("the fixed xAI loopback callback is unavailable")]
    LoopbackUnavailable,
    /// The callback was malformed or failed correlation.
    #[error("the xAI OAuth callback is invalid")]
    InvalidCallback,
    /// The login or device code expired.
    #[error("the xAI login expired")]
    LoginExpired,
    /// The resource owner denied authorization.
    #[error("xAI authorization was denied")]
    AuthorizationDenied,
    /// Browser or device login failed during the bounded OAuth token exchange.
    #[error("the xAI login token exchange failed")]
    LoginTokenExchange,
    /// Browser or device login failed while resolving trusted signing keys.
    #[error("the xAI login signing-key verification failed")]
    LoginSigningKeys,
    /// Browser or device login failed while verifying the provider account.
    #[error("the xAI login account verification failed")]
    LoginAccountVerification,
    /// A verified browser or device login could not be committed durably.
    #[error("the xAI login credential could not be persisted")]
    LoginPersistence,
    /// A strict OAuth token response check failed.
    #[error("the xAI OAuth token response is invalid")]
    InvalidTokenResponse,
    /// ID-token signature, header, claims, nonce, or times failed validation.
    #[error("the xAI ID token is invalid")]
    InvalidIdToken,
    /// JWKS grammar, bounds, or key metadata was invalid.
    #[error("the xAI JWKS response is invalid")]
    InvalidJwks,
    /// The signed token referenced no current bounded signing key.
    #[error("the xAI ID token signing key is unknown")]
    UnknownSigningKey,
    /// `UserInfo` or token subject changed account.
    #[error("the xAI OAuth account changed")]
    AccountChanged,
    /// The app-owned login selection changed since a route was captured.
    #[error("the selected xAI login changed")]
    SelectionChanged,
    /// Stored opaque provider data was malformed or foreign.
    #[error("the stored xAI credential is invalid")]
    InvalidStoredCredential,
    /// The fresh account-scoped model catalog did not admit the exact pinned model.
    #[error("the exact xAI subscription model is not available in a fresh catalog")]
    CatalogUnavailable,
    /// The caller requested an alias or model outside the exact admitted tuple.
    #[error("the requested xAI subscription model is not exactly admitted")]
    RequestedModelNotAdmitted,
    /// Refresh entered durable Unknown and cannot be retried automatically.
    #[error("the xAI refresh outcome is quarantined")]
    RefreshQuarantined,
    /// A fixed HTTPS request could not be constructed.
    #[error("the fixed xAI authentication request is invalid")]
    Request,
    /// The fixed authentication exchange failed.
    #[error("the xAI authentication exchange failed")]
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// The provider rejected the fixed authentication exchange.
    #[error("xAI rejected the authentication exchange with HTTP {0}")]
    HttpStatus(u16),
    /// Provider-neutral binding generation failed.
    #[error(transparent)]
    Binding(#[from] plakous_auth::BindingError),
    /// Provider-neutral credential value validation failed.
    #[error(transparent)]
    CredentialValue(#[from] plakous_auth::CredentialValueError),
    /// Private selected-slot storage failed.
    #[error(transparent)]
    Store(#[from] PrivateStoreError),
    /// Fixed request construction failed.
    #[error(transparent)]
    HttpRequest(#[from] plakous_http::RequestError),
    /// Route identity construction failed.
    #[error(transparent)]
    Route(#[from] crate::XaiRouteError),
    /// Model resolution failed.
    #[error(transparent)]
    Model(#[from] crate::XaiModelError),
    /// Language-model construction failed.
    #[error(transparent)]
    LanguageModel(#[from] crate::XaiLanguageModelConfigError),
    /// Neutral descriptor digest construction failed.
    #[error(transparent)]
    Descriptor(#[from] plakous_model::ValidationError),
    /// Neutral identity construction failed.
    #[error(transparent)]
    Identity(#[from] plakous_model::IdentityError),
}

impl From<AuthTransportError> for XaiNativeAuthError {
    fn from(error: AuthTransportError) -> Self {
        Self::Transport(match error {
            AuthTransportError::Failure(failure) => network_failure(failure),
            AuthTransportError::ResponseTooLarge => None,
        })
    }
}

enum AuthTransportError {
    Failure(Failure),
    ResponseTooLarge,
}

#[derive(Clone, Copy)]
enum LoginHttpStage {
    SigningKeys,
    AccountVerification,
}

impl LoginHttpStage {
    const fn error(self) -> XaiNativeAuthError {
        match self {
            Self::SigningKeys => XaiNativeAuthError::LoginSigningKeys,
            Self::AccountVerification => XaiNativeAuthError::LoginAccountVerification,
        }
    }
}

impl AuthTransportError {
    const fn limit() -> Self {
        Self::ResponseTooLarge
    }
}

impl From<Failure> for AuthTransportError {
    fn from(value: Failure) -> Self {
        Self::Failure(value)
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
                path: crate::GROK_SUBSCRIPTION_USAGE_PATH.to_owned(),
            },
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
            usage: endpoint(crate::GROK_SUBSCRIPTION_USAGE_PATH),
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
            || self.expires_in.is_some_and(|value| value == 0)
        {
            return Err(XaiNativeAuthError::InvalidTokenResponse);
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
        if self.version != 2
            || self.issuer != XAI_ISSUER
            || self.client_id != XAI_PUBLIC_CLIENT_ID
            || self.subject != account.as_str()
            || self.scope != XAI_OAUTH_SCOPE
            || self.access_token.is_empty()
            || self.access_token.len() > MAX_STRING_BYTES
            || self.refresh_token.is_empty()
            || self.refresh_token.len() > MAX_STRING_BYTES
            || !valid_selection_epoch(&self.selection_epoch)
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
            return Err(XaiNativeAuthError::InvalidTokenResponse);
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

#[derive(Deserialize)]
struct CatalogResponse {
    data: Vec<serde_json::Value>,
}

impl CatalogResponse {
    fn admitted_coding_entries(&self) -> Result<Vec<XaiNativeCatalogEntry>, XaiNativeAuthError> {
        if self.data.is_empty() || self.data.len() > MAX_CATALOG_MODELS {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let mut requested_identities = BTreeSet::new();
        let mut effective_identities = BTreeSet::new();
        let mut admitted = Vec::new();
        for value in &self.data {
            let Some(model) = ProxyCatalogModel::decode(value) else {
                continue;
            };
            if !requested_identities.insert(model.requested.clone())
                || !effective_identities.insert(model.effective.clone())
            {
                return Err(XaiNativeAuthError::CatalogUnavailable);
            }
            // The catalog grammar may express aliases through a distinct `id`,
            // but aliases never enter the closed literal enrollment.
            if is_subscription_coding_model_id(&model.requested)
                && model.requested == model.effective
            {
                let reported_context_length = model
                    .context_window
                    .and_then(|value| u32::try_from(value).ok())
                    .and_then(NonZeroU32::new);
                let Some(reported_context_length) = reported_context_length else {
                    continue;
                };
                admitted.push(XaiNativeCatalogEntry {
                    model: ModelId::new(model.requested.as_str())?,
                    effective_model: ModelId::new(model.effective.as_str())?,
                    diagnostics: XaiCatalogDiagnosticFacts {
                        reported_context_length,
                    },
                });
            }
        }
        // Catalog presence confirms availability but never expands the closed
        // reviewed model table or turns reported context length into capacity.
        if admitted.is_empty() {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        admitted.sort_by(|left, right| left.model.as_str().cmp(right.model.as_str()));
        Ok(admitted)
    }
}

fn valid_admitted_catalog_entries(entries: &[XaiNativeCatalogEntry]) -> bool {
    !entries.is_empty()
        && entries.iter().all(|entry| {
            is_subscription_coding_model_id(entry.model.as_str())
                && entry.model == entry.effective_model
        })
        && entries
            .windows(2)
            .all(|pair| pair[0].model.as_str() < pair[1].model.as_str())
}

struct ProxyCatalogModel {
    requested: String,
    effective: String,
    context_window: Option<u64>,
}

impl ProxyCatalogModel {
    fn decode(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let meta = object.get("_meta").and_then(serde_json::Value::as_object);
        let id = proxy_string(object, "id");
        let effective = proxy_string(object, "model")
            .or_else(|| proxy_string(object, "modelId"))
            .or_else(|| id.clone())
            .or_else(|| meta.and_then(|meta| proxy_string(meta, "model")))
            .or_else(|| meta.and_then(|meta| proxy_string(meta, "modelId")))?;
        let requested = id.unwrap_or_else(|| effective.clone());
        if !valid_token_component(&requested, 128) || !valid_token_component(&effective, 128) {
            return None;
        }
        let context_window = proxy_u64(object, "contextWindow")
            .or_else(|| proxy_u64(object, "context_window"))
            .or_else(|| meta.and_then(|meta| proxy_u64(meta, "contextWindow")))
            .or_else(|| meta.and_then(|meta| proxy_u64(meta, "totalContextTokens")));
        Some(Self {
            requested,
            effective,
            context_window,
        })
    }
}

fn proxy_string(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(ToOwned::to_owned)
}

fn proxy_u64(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u64> {
    object.get(key)?.as_u64()
}

fn authorization_url(
    endpoints: &AuthEndpoints,
    pkce: &PkceS256,
    binding: &CallbackBinding,
) -> Result<Zeroizing<String>, XaiNativeAuthError> {
    // Dialect-owned extras stay local; shared builder owns the PKCE core.
    // Manager endpoint origin stays authoritative (including test doubles).
    build_authorization_url(AuthorizeUrlRequest {
        pins: XAI_OAUTH,
        auth_origin: Some(endpoints.authorize().origin.as_str()),
        redirect_uri: XAI_LOOPBACK_REDIRECT,
        code_challenge: pkce.challenge().as_str(),
        state: binding.expose_state(),
        nonce: Some(binding.expose_nonce()),
        extra_pairs: &[("plan", "generic"), ("referrer", "plak")],
    })
    .map_err(|_| XaiNativeAuthError::Request)
}

enum LoopbackRequest {
    Callback(BrowserCallback),
    Preflight { origin: AllowedBrowserOrigin },
}

struct XaiBrowserLoopbackHooks {
    /// Session deadline retained after the flow is consumed by complete.
    deadline: Instant,
    flow: Option<BrowserLoginFlow>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl BrowserLoopbackHooks for XaiBrowserLoopbackHooks {
    type Error = XaiNativeAuthError;
    type Callback = BrowserCallback;
    type Preflight = AllowedBrowserOrigin;
    type Output = XaiSubscriptionStatus;

    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn max_connections(&self) -> usize {
        MAX_LOOPBACK_CONNECTIONS
    }

    fn max_request_bytes(&self) -> usize {
        MAX_CALLBACK_BYTES
    }

    fn deadline_error(&self) -> Self::Error {
        XaiNativeAuthError::LoginExpired
    }

    fn callback_error(&self) -> Self::Error {
        XaiNativeAuthError::InvalidCallback
    }

    fn parse_request(
        &mut self,
        request: &[u8],
        _local_port: u16,
    ) -> Result<LoopbackConnectionEvent<Self::Callback, Self::Preflight>, Self::Error> {
        let text = std::str::from_utf8(request).map_err(|_| XaiNativeAuthError::InvalidCallback)?;
        match parse_loopback_request(text)? {
            LoopbackRequest::Callback(callback) => Ok(LoopbackConnectionEvent::Callback(callback)),
            LoopbackRequest::Preflight { origin } => Ok(LoopbackConnectionEvent::Preflight(origin)),
        }
    }

    fn preflight_response_bytes(&self, preflight: &Self::Preflight) -> Vec<u8> {
        format!(
            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {}\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            preflight.as_str()
        )
        .into_bytes()
    }

    fn callback_response_bytes(&self, complete: Result<&Self::Output, &Self::Error>) -> Vec<u8> {
        let (status, reason, body) = if complete.is_ok() {
            (
                200,
                "OK",
                "Authorization complete. Return to your application.",
            )
        } else {
            (
                400,
                "Bad Request",
                "Authorization failed. Return to your application.",
            )
        };
        format_http11_text_response(
            status,
            reason,
            body,
            &[
                ("Cache-Control", "no-store"),
                ("Pragma", "no-cache"),
                ("X-Content-Type-Options", "nosniff"),
            ],
        )
        .into_bytes()
    }

    async fn complete_callback(
        &mut self,
        callback: Self::Callback,
    ) -> Result<Self::Output, Self::Error> {
        let flow = self
            .flow
            .take()
            .ok_or(XaiNativeAuthError::InvalidCallback)?;
        flow.complete_callback(callback).await
    }
}

#[derive(Clone, Copy)]
enum AllowedBrowserOrigin {
    AccountsXai,
    AuthXai,
}

impl AllowedBrowserOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AccountsXai => "https://accounts.x.ai",
            Self::AuthXai => "https://auth.x.ai",
        }
    }
}

enum BrowserCallback {
    AuthorizationCode {
        code: Zeroizing<String>,
        state: Zeroizing<String>,
    },
    ProviderError {
        state: Zeroizing<String>,
    },
}

fn parse_loopback_request(request: &str) -> Result<LoopbackRequest, XaiNativeAuthError> {
    if request.starts_with("OPTIONS ") {
        return parse_preflight_request(request)
            .map(|origin| LoopbackRequest::Preflight { origin });
    }
    parse_callback_request(request).map(LoopbackRequest::Callback)
}

fn parse_preflight_request(request: &str) -> Result<AllowedBrowserOrigin, XaiNativeAuthError> {
    let head = request
        .strip_suffix("\r\n\r\n")
        .ok_or(XaiNativeAuthError::InvalidCallback)?;
    let mut lines = head.split("\r\n");
    if lines.next() != Some("OPTIONS /callback HTTP/1.1") {
        return Err(XaiNativeAuthError::InvalidCallback);
    }
    let mut host = None;
    let mut origin = None;
    let mut requested_method = None;
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(XaiNativeAuthError::InvalidCallback)?;
        if name.is_empty() || !name.bytes().all(is_http_token_byte) {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        let value = value.trim_matches(|character| character == ' ' || character == '\t');
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        if name.eq_ignore_ascii_case("host") && host.replace(value).is_some() {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        if name.eq_ignore_ascii_case("origin") && origin.replace(value).is_some() {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        if name.eq_ignore_ascii_case("access-control-request-method")
            && requested_method.replace(value).is_some()
        {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
    }
    if host.is_none_or(|value| !XAI_OAUTH.loopback_host_matches(value, None))
        || requested_method != Some("GET")
    {
        return Err(XaiNativeAuthError::InvalidCallback);
    }
    match origin {
        Some("https://accounts.x.ai") => Ok(AllowedBrowserOrigin::AccountsXai),
        Some("https://auth.x.ai") => Ok(AllowedBrowserOrigin::AuthXai),
        _ => Err(XaiNativeAuthError::InvalidCallback),
    }
}

fn xai_callback_policy() -> CallbackQueryPolicy {
    CallbackQueryPolicy::xai(XAI_OAUTH_SCOPE)
}

fn oauth_callback_to_browser(outcome: OAuthCallbackOutcome) -> BrowserCallback {
    match outcome {
        OAuthCallbackOutcome::AuthorizationCode { code, state } => {
            BrowserCallback::AuthorizationCode { code, state }
        }
        OAuthCallbackOutcome::ProviderError { state } => BrowserCallback::ProviderError { state },
    }
}

fn parse_callback_request(request: &str) -> Result<BrowserCallback, XaiNativeAuthError> {
    let parsed =
        parse_http11_get_request(request).map_err(|_| XaiNativeAuthError::InvalidCallback)?;
    if !XAI_OAUTH.loopback_host_matches(parsed.host, None) {
        return Err(XaiNativeAuthError::InvalidCallback);
    }
    let full_redirect_url = Zeroizing::new(format!(
        "http://{}{}",
        XAI_OAUTH.loopback_authority, parsed.target
    ));
    parse_callback_url(&full_redirect_url)
}

fn parse_callback_url(full_redirect_url: &str) -> Result<BrowserCallback, XaiNativeAuthError> {
    parse_absolute_redirect_query(
        full_redirect_url,
        XAI_LOOPBACK_REDIRECT,
        xai_callback_policy(),
    )
    .map(oauth_callback_to_browser)
    .map_err(|_| XaiNativeAuthError::InvalidCallback)
}

async fn until_flow_deadline<F, T>(deadline: Instant, future: F) -> Result<T, XaiNativeAuthError>
where
    F: Future<Output = Result<T, XaiNativeAuthError>>,
{
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .map_err(|_| XaiNativeAuthError::LoginExpired)?
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
    if let Some(slot) = store.slot_id() {
        hasher.update(slot.as_str().as_bytes());
    }
    let mut encoded = String::from("plak-native-xai-route:");
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

fn generate_selection_epoch() -> Result<String, XaiNativeAuthError> {
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|_| XaiNativeAuthError::RandomnessUnavailable)?;
    let mut encoded = String::with_capacity(64);
    for byte in random {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    Ok(encoded)
}

fn valid_selection_epoch(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    // Preserve distinct dialect errors: acceptance range vs kill/expiry.
    if acceptance.accepted_at > now || acceptance.accepted_at < XAI_PROFILE.reviewed_at_unix {
        return Err(XaiNativeAuthError::CompatibilityNotAccepted);
    }
    if XAI_PROFILE.killed || now >= expires_unix {
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
        OAuthWorkflowError::ResponseTooLarge
        | OAuthWorkflowError::InvalidJson
        | OAuthWorkflowError::InvalidUrl
        | OAuthWorkflowError::InvalidForm => XaiNativeAuthError::InvalidTokenResponse,
        _ => XaiNativeAuthError::InvalidTokenResponse,
    }
}

fn offline_status(
    status: Option<CredentialStatus>,
) -> Result<XaiOfflineCredentialStatus, XaiNativeAuthError> {
    Ok(OfflineCredentialLifecycle::classify(status))
}

fn system_unix_seconds() -> Result<u64, XaiNativeAuthError> {
    system_unix_elapsed().map(|value| value.as_secs())
}

fn system_unix_elapsed() -> Result<Duration, XaiNativeAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| XaiNativeAuthError::CompatibilityInactive)
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
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(XaiNativeAuthError::InvalidTokenResponse);
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
    deserializer
        .deserialize_map(UniqueObject)
        .map_err(|_| XaiNativeAuthError::InvalidTokenResponse)?;
    deserializer
        .end()
        .map_err(|_| XaiNativeAuthError::InvalidTokenResponse)
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
