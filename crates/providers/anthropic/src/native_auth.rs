//! Plakous-owned Claude Code OAuth login and selected-slot credential authority.
//!
//! Authorize is HTTPS-callback PKCE (`state` is the verifier). There is no
//! loopback listener: local and remote product modes share this paste-complete
//! handle. Token exchange is JSON against the pinned Platform token origin.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use futures_util::lock::Mutex as AsyncMutex;
use plakous_auth::oauth_orchestrator::{
    AuthBodyError, BuildRequestFailure, ClaimedRefreshHooks, CredentialRefreshDetail,
    CredentialRefreshFailureReason, CredentialRefreshObserver, PullBodyError, pull_claimed_body,
    run_claimed_refresh,
};
use plakous_auth::oauth_workflow::{access_needs_refresh, remaining_until};
use plakous_auth::subscription_limit::{
    SubscriptionRequestClass, SubscriptionRequestGate, SubscriptionRequestPermit,
};
use plakous_auth::{
    AccountBinding, ClaimedHttpDisposition, ClaimedHttpError, ClaimedHttpResponse, CredentialFence,
    CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    OfflineCredentialLifecycle, PkceS256, PrivateCredentialStore, PrivateStoreError, RecordAge,
    RefreshClaim, SecretBytes, SubscriptionIntegrationProfileId,
};
use plakous_http::{
    Bytes, Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request,
    SensitiveBytes, StatusCode, sensitive_header_value,
};
use plakous_model::{
    Digest, DigestDomain, SubscriptionUsageCredentialState, SubscriptionUsageRead,
    SubscriptionUsageSnapshot, SubscriptionUsageUnavailable, Timestamp,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize as _, Zeroizing};

use crate::oauth::{
    CLAUDE_OAUTH_CLIENT_ID, CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID, CLAUDE_OAUTH_EXPIRES_AT_UNIX,
    CLAUDE_OAUTH_INFERENCE_SCOPE, CLAUDE_OAUTH_KILL_SWITCH_REVISION, CLAUDE_OAUTH_MESSAGES_ORIGIN,
    CLAUDE_OAUTH_MESSAGES_USER_AGENT, CLAUDE_OAUTH_NOTICE_REVISION, CLAUDE_OAUTH_REDIRECT_URI,
    CLAUDE_OAUTH_REVIEWED_AT_UNIX, CLAUDE_OAUTH_SOURCE_REVISION, CLAUDE_OAUTH_TOKEN_ORIGIN,
    CLAUDE_OAUTH_TOKEN_PATH, CLAUDE_OAUTH_USER_AGENT, ClaudeOAuthTokenGrant, claude_authorize_url,
    claude_oauth_scope_admits_inference, claude_token_code_body, claude_token_refresh_body,
    parse_claude_oauth_authorization_input, parse_claude_oauth_token_grant,
};
use crate::{
    subscription_usage::CLAUDE_SUBSCRIPTION_PROFILE_PATH,
    subscription_usage::CLAUDE_SUBSCRIPTION_USAGE_PATH,
    subscription_usage::MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES,
};
use plakous_exchange::network_failure;

mod error;
mod login;
mod manager;
mod manager_core;
mod profile;
mod refresh;

pub use error::ClaudeNativeAuthError;
pub use login::ClaudeOAuthLogin;
pub use manager::ClaudeNativeAuthManager;
pub use profile::{
    ClaudeCompatibilityAcceptance, ClaudeEnrolledModel, ClaudeOfflineSelectedSlot,
    ClaudeSubscriptionIntegrationProfile, ClaudeSubscriptionStatus,
};
use refresh::ClaudeRefreshHooks;

const USER_AGENT: &str = CLAUDE_OAUTH_USER_AGENT;
const STORED_VERSION: u32 = 3;
const REFRESH_SKEW_SECONDS: u64 = 120;
const MAX_TOKEN_BYTES: usize = 128 * 1024;
const MAX_STRING_BYTES: usize = 64 * 1024;
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(not(test))]
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
#[cfg(test)]
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

const HEADER_ACCEPT: HeaderName = HeaderName::from_static("accept");
const HEADER_AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const HEADER_ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
const HEADER_CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const HEADER_USER_AGENT: HeaderName = HeaderName::from_static("user-agent");
const CLAUDE_USAGE_BETA: &str = "oauth-2025-04-20";

fn oauth_control_headers(stored: &StoredTokens) -> Result<HeaderMap, SubscriptionUsageUnavailable> {
    let mut headers = HeaderMap::new();
    let mut bearer = Vec::with_capacity(7 + stored.access_token.len());
    bearer.extend_from_slice(b"Bearer ");
    bearer.extend_from_slice(stored.access_token.as_bytes());
    let Ok(authorization) = sensitive_header_value(bearer) else {
        return Err(SubscriptionUsageUnavailable::InvalidResponse);
    };
    headers.insert(HEADER_AUTHORIZATION, authorization);
    headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        HEADER_CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HEADER_USER_AGENT,
        HeaderValue::from_static(CLAUDE_OAUTH_MESSAGES_USER_AGENT),
    );
    headers.insert(
        HEADER_ANTHROPIC_BETA,
        HeaderValue::from_static(CLAUDE_USAGE_BETA),
    );
    headers.insert(
        HeaderName::from_static("anthropic-dangerous-direct-browser-access"),
        HeaderValue::from_static("true"),
    );
    headers.insert(
        HeaderName::from_static("x-app"),
        HeaderValue::from_static("cli"),
    );
    Ok(headers)
}

fn attach_claude_profile(
    mut snapshot: SubscriptionUsageSnapshot,
    profile_reply: Option<&(StatusCode, bool, Zeroizing<Vec<u8>>)>,
) -> SubscriptionUsageSnapshot {
    let Some((status, is_json, body)) = profile_reply else {
        return snapshot;
    };
    if status.as_u16() != 200 || !*is_json {
        return snapshot;
    }
    let Ok(profile) = crate::decode_claude_oauth_profile(body) else {
        return snapshot;
    };
    if let Some(email) = profile.email {
        if let Ok(attached) = snapshot.clone().with_account(Some(email)) {
            snapshot = attached;
        }
    }
    if let Some(plan) = profile.plan {
        if let Ok(attached) = snapshot.clone().with_plan(Some(plan)) {
            snapshot = attached;
        }
    }
    snapshot
}

struct ManagerCore {
    device_id: crate::ClaudeClientDeviceId,
    store: PrivateCredentialStore,
    client: Client,
    acceptance: ClaudeCompatibilityAcceptance,
    token_origin: String,
    #[cfg(test)]
    control_origin: Option<String>,
    refresh_flight: AsyncMutex<()>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredTokens {
    #[serde(default)]
    account_uuid: Option<crate::ClaudeAccountUuid>,
    version: u32,
    client_id: String,
    scope: String,
    pub(crate) access_token: String,
    refresh_token: String,
    expires_at: u64,
    grant_binding: String,
}

/// Only the version of a stored payload, read when the current shape fails.
#[derive(Deserialize)]
struct StoredVersion {
    version: u32,
}

impl StoredTokens {
    fn validate(&self) -> Result<(), ClaudeNativeAuthError> {
        if self.version != STORED_VERSION
            || self.client_id != CLAUDE_OAUTH_CLIENT_ID
            || !claude_oauth_scope_admits_inference(&self.scope)
            || self.access_token.is_empty()
            || self.access_token.len() > MAX_STRING_BYTES
            || self.refresh_token.is_empty()
            || self.refresh_token.len() > MAX_STRING_BYTES
            || self.expires_at == 0
            || AccountBinding::new(self.grant_binding.clone()).is_err()
        {
            return Err(ClaudeNativeAuthError::InvalidStoredCredential);
        }
        Ok(())
    }
}

impl Drop for StoredTokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

fn stored_tokens(
    access_token: &str,
    refresh_token: &str,
    expires_in: i64,
    scope: &str,
    grant_binding: &str,
    account_uuid: Option<crate::ClaudeAccountUuid>,
) -> Result<StoredTokens, ClaudeNativeAuthError> {
    let expires_in =
        u64::try_from(expires_in).map_err(|_| ClaudeNativeAuthError::InvalidTokenResponse)?;
    let expires_at = system_unix_seconds()?
        .checked_add(expires_in)
        .ok_or(ClaudeNativeAuthError::InvalidTokenResponse)?;
    let stored = StoredTokens {
        account_uuid,
        version: STORED_VERSION,
        client_id: CLAUDE_OAUTH_CLIENT_ID.to_owned(),
        scope: scope.to_owned(),
        access_token: access_token.to_owned(),
        refresh_token: refresh_token.to_owned(),
        expires_at,
        grant_binding: grant_binding.to_owned(),
    };
    stored.validate()?;
    Ok(stored)
}

fn credential_record_from_grant(
    generation: CredentialGeneration,
    grant: &ClaudeOAuthTokenGrant,
) -> Result<CredentialRecord, ClaudeNativeAuthError> {
    let refresh = grant
        .refresh_token()
        .ok_or(ClaudeNativeAuthError::LoginTokenExchange)?;
    let grant_binding = oauth_grant_binding(refresh)?;
    let stored = stored_tokens(
        grant.access_token(),
        refresh,
        grant.expires_in(),
        grant.scope().unwrap_or(CLAUDE_OAUTH_INFERENCE_SCOPE),
        grant_binding.as_str(),
        grant.account_uuid().cloned(),
    )?;
    credential_record(generation, &stored)
}

fn credential_record(
    generation: CredentialGeneration,
    stored: &StoredTokens,
) -> Result<CredentialRecord, ClaudeNativeAuthError> {
    let payload =
        serde_json::to_vec(stored).map_err(|_| ClaudeNativeAuthError::LoginPersistence)?;
    Ok(CredentialRecord::new(
        auth_profile_id()?,
        AccountBinding::new(stored.grant_binding.clone())?,
        generation,
        CredentialStatus::Ready,
        SecretBytes::new(payload),
    )?)
}

/// Refuses a record in any lifecycle state whose payload a newer build
/// wrote, then validates a Ready record's payload in this build's format.
fn validate_current_ready_record(
    store: &PrivateCredentialStore,
) -> Result<(), ClaudeNativeAuthError> {
    let newer = store.inspect_payload(|payload| {
        serde_json::from_slice::<StoredVersion>(payload.expose_secret()).is_ok_and(|stored| {
            RecordAge::of_version(stored.version.into(), STORED_VERSION.into()) == RecordAge::Newer
        })
    })?;
    if newer == Some(true) {
        return Err(ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild);
    }
    let Some(fence) = store.load()? else {
        return Ok(());
    };
    if fence.status() != CredentialStatus::Ready {
        return Ok(());
    }
    let lease = store.acquire_ready(&fence)?;
    let payload = lease.payload().expose_secret();
    let Ok(current) = serde_json::from_slice::<StoredTokens>(payload) else {
        return Err(match serde_json::from_slice::<StoredVersion>(payload) {
            Ok(stored) if stored.version < STORED_VERSION => {
                ClaudeNativeAuthError::StoredCredentialFormatRetired
            }
            _ => ClaudeNativeAuthError::InvalidStoredCredential,
        });
    };
    current.validate()?;
    if current.grant_binding != fence.account_binding().as_str() {
        return Err(ClaudeNativeAuthError::InvalidStoredCredential);
    }
    Ok(())
}

fn oauth_grant_binding(refresh_token: &str) -> Result<AccountBinding, ClaudeNativeAuthError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderAuthorityBinding,
        1,
        &[
            b"anthropic-claude-oauth-grant-lineage",
            CLAUDE_OAUTH_CLIENT_ID.as_bytes(),
            refresh_token.as_bytes(),
        ],
    )
    .map_err(|_| ClaudeNativeAuthError::InvalidTokenResponse)?;
    AccountBinding::new(format!("grant-{digest}"))
        .map_err(|_| ClaudeNativeAuthError::InvalidTokenResponse)
}

fn auth_profile_id() -> Result<SubscriptionIntegrationProfileId, ClaudeNativeAuthError> {
    Ok(SubscriptionIntegrationProfileId::new(
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
    )?)
}

fn validate_acceptance(
    acceptance: &ClaudeCompatibilityAcceptance,
    now: u64,
) -> Result<(), ClaudeNativeAuthError> {
    if !(CLAUDE_OAUTH_REVIEWED_AT_UNIX..CLAUDE_OAUTH_EXPIRES_AT_UNIX).contains(&now)
        || acceptance.accepted_at > now
    {
        return Err(ClaudeNativeAuthError::CompatibilityInactive);
    }
    Ok(())
}

fn system_unix_seconds() -> Result<u64, ClaudeNativeAuthError> {
    subscription_unix_elapsed().map(|value| value.as_secs())
}

/// The clock the Claude OAuth compatibility profile, its credentials and its
/// reviewed admission are checked against.
///
/// Supported builds read the system clock. Offline tests and `test-clock`
/// builds read a clock that starts one minute after the compiled review and
/// advances with elapsed monotonic time, so they never change with the
/// calendar (spec 05 sec. 2).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SubscriptionClock;

impl plakous_model::Clock for SubscriptionClock {
    fn now(&self) -> Timestamp {
        subscription_unix_elapsed()
            .ok()
            .and_then(|elapsed| Timestamp::new(elapsed.as_secs(), elapsed.subsec_nanos()).ok())
            .unwrap_or_else(|| plakous_model::Clock::now(&plakous_model::SystemClock))
    }
}

fn subscription_unix_elapsed() -> Result<Duration, ClaudeNativeAuthError> {
    subscription_time::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ClaudeNativeAuthError::CompatibilityInactive)
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
            + Duration::from_secs(super::CLAUDE_OAUTH_REVIEWED_AT_UNIX + 60)
            + START.get_or_init(Instant::now).elapsed()
    }
}

async fn until_flow_deadline<T, F>(
    deadline: Instant,
    operation: F,
) -> Result<T, ClaudeNativeAuthError>
where
    F: Future<Output = Result<T, ClaudeNativeAuthError>>,
{
    let remaining =
        remaining_until(deadline, Instant::now()).ok_or(ClaudeNativeAuthError::LoginExpired)?;
    match tokio::time::timeout(remaining, operation).await {
        Ok(result) => result,
        Err(_) => Err(ClaudeNativeAuthError::LoginExpired),
    }
}

fn acquire_refresh_permit(
    gate: Option<&Arc<dyn SubscriptionRequestGate>>,
) -> Result<Option<Box<dyn SubscriptionRequestPermit>>, ClaudeNativeAuthError> {
    gate.map(|gate| gate.acquire(SubscriptionRequestClass::AuthRefresh))
        .transpose()
        .map_err(|_| ClaudeNativeAuthError::NotReady)
}

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

fn map_pull_body_error(error: PullBodyError) -> ClaudeNativeAuthError {
    match error {
        PullBodyError::TooLarge => ClaudeNativeAuthError::InvalidTokenResponse,
        PullBodyError::Transport | _ => ClaudeNativeAuthError::Transport(None),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid fixtures"
)]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline fixture assertions")]
mod identity_tests;
