//! Plak-owned, revision-pinned ChatGPT/Codex authentication and model discovery.

mod authority;
mod authorization;
mod catalog;
mod catalog_source;
mod dispatch;
mod error;
mod evidence;
mod http;
mod local_admin;
mod login;
mod manager;
mod profile;
mod refresh;
mod renewal;
mod resolver;
mod verify;

pub(crate) use authorization::NativeCodexAuthorization;
pub(crate) use catalog_source::CodexCatalogSource;
pub use error::NativeCodexAuthError;
pub use evidence::CodexModelResolutionEvidence;
use plakous_auth::native_auth::usage_credential_refusal;

#[cfg(test)]
use http::HttpReply;
use http::{
    base_headers, bearer_headers, claimed_request, endpoint_url, execute_bounded,
    execute_catalog_bounded, form_body, form_headers, json_headers, parse_catalog_json,
    parse_strict_json, pull_claimed_body_cancellable, require_json_media_type, require_status,
};
pub use local_admin::CodexSubscriptionLocalAdmin;
pub use manager::CodexSubscriptionCredentialManager;
pub use profile::{
    CodexCompatibilityAcceptance, CodexCredentialStatus, CodexSubscriptionIntegrationProfile,
    SubscriptionIntegrationProvenance,
};
use renewal::{
    CatalogRenewal, ExpectedSelection, credential_selection_epoch, resolution_route_fingerprint,
    validate_catalog_fence,
};
pub(crate) use renewal::{CodexReceiptRenewal, CodexRenewalFloor, CodexRowSemantics};
pub use resolver::CodexModelResolver;

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
    codex_responses_preflight, codex_side_request, codex_side_request_rejected,
    revalidate_codex_responses,
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
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode_header};
use plakous_auth::oauth_orchestrator::jwt::decode;
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
    PrivateStoreError, ReadyCredentialLease, RecordAge, RefreshClaim, SecretBytes,
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

use crate::oauth_pins::{CODEX_OAUTH, CODEX_ORIGINATOR, CODEX_PROFILE};
use crate::{
    AuthError, CodexSubscriptionRouteConfig, EndpointOrigin, NativeCodexCatalogWitness,
    NativeCodexCredentialAuthority, NativeCodexForcedRefresh, NativeCodexPreflight,
    NativeCodexPreflightRequest, NativeCodexRuntimeAuthority, OPENAI_CHATGPT_ORIGIN,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_SOURCE_REVISION,
    OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiLanguageModel, OpenAiRoute, OpenAiTarget,
    StandardOpenAiHttpConfig, StorageMode, config::OPENAI_CODEX_MODELS_PATH,
};
use plakous_auth::api_key::CredentialGenerationFingerprint;
use plakous_responses::ResponsesPhysicalRequestBinding;
use plakous_responses::{
    ResponsesAccountSlot, ResponsesAuthSlot, ResponsesCredentialAuthority,
    ResponsesDispatchRejection, ResponsesHeaderBinding, ResponsesPreflightFuture,
    ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest, ResponsesRequestAuthority,
    ResponsesRevalidationRequest,
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
    "codex-complete-input-utf8-bytes-div4-encrypted-v6";
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
/// build cannot parse (an earlier build's login) is `LoginRequired`, one
/// written under another compatibility profile is `ProfileChanged`; a record
/// whose lease is busy keeps its stored status.
fn local_credential_status(
    store: &PrivateCredentialStore,
) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
    let fence = match store.load() {
        Ok(fence) => fence,
        Err(PrivateStoreError::WrittenByNewerBuild) => {
            return Ok(OfflineCredentialLifecycle::WrittenByNewerBuild);
        }
        Err(PrivateStoreError::ForeignProfile) => return Err(NativeCodexAuthError::ProfileChanged),
        Err(_) => return Err(NativeCodexAuthError::Store),
    };
    if fence.is_some()
        && store
            .inspect_payload(|payload| payload_age(payload) == RecordAge::Newer)
            .map_err(|_| NativeCodexAuthError::Store)?
            == Some(true)
    {
        return Ok(OfflineCredentialLifecycle::WrittenByNewerBuild);
    }
    if let Some(fence) = fence
        .as_ref()
        .filter(|fence| fence.status() == CredentialStatus::Ready)
        && let Ok(Some(lease)) = store.try_acquire_ready(fence)
        && parse_stored(lease.payload()).is_err()
    {
        return Ok(OfflineCredentialLifecycle::LoginRequired);
    }
    if fence
        .as_ref()
        .is_some_and(|fence| fence.status() == CredentialStatus::RefreshOutcomeUnknown)
    {
        return store
            .offline_lifecycle()
            .map_err(|_| NativeCodexAuthError::Store);
    }
    Ok(OfflineCredentialLifecycle::classify(
        fence.as_ref().map(CredentialFence::status),
    ))
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

/// The payload version this build writes. Its shape carries no `version`
/// field; the next build that changes it writes `"version": 2`.
const STORED_CREDENTIAL_VERSION: u64 = 1;

/// Only the optional top-level version, read before the strict parse.
#[derive(Deserialize)]
struct StoredVersionProbe {
    #[serde(default)]
    version: Option<u64>,
}

/// An absent version is this build's own shape. A present version greater
/// than this build's is a newer build's; any other present version is
/// malformed (no build writes it) and fails the strict parse as before.
fn payload_age(payload: &SecretBytes) -> RecordAge {
    match serde_json::from_slice::<StoredVersionProbe>(payload.expose_secret()) {
        Ok(StoredVersionProbe {
            version: Some(version),
        }) if version > STORED_CREDENTIAL_VERSION => RecordAge::Newer,
        _ => RecordAge::Current,
    }
}

/// Every payload this build cannot parse, whether a login stored by an
/// earlier build without a selection epoch or a corrupt record, is
/// `LoginRequired`: it needs a new login, and nothing is synthesized or
/// parsed in an older shape. A newer build's payload is never parsed.
fn parse_stored(payload: &SecretBytes) -> Result<StoredCredential, NativeCodexAuthError> {
    if payload.is_empty() || payload.len() > MAX_AUTH_BODY_BYTES {
        return Err(NativeCodexAuthError::Validation);
    }
    if payload_age(payload) == RecordAge::Newer {
        return Err(NativeCodexAuthError::WrittenByNewerBuild);
    }
    let credential: StoredCredential = serde_json::from_slice(payload.expose_secret())
        .map_err(|_| NativeCodexAuthError::LoginRequired)?;
    if !valid_login_selection_epoch(&credential.selection_epoch) {
        return Err(NativeCodexAuthError::LoginRequired);
    }
    Ok(credential)
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
