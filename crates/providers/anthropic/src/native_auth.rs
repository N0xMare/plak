//! Plakous-owned Claude Code OAuth login and selected-slot credential authority.
//!
//! Authorize is HTTPS-callback PKCE (`state` is the verifier). There is no
//! loopback listener: local and remote product modes share this paste-complete
//! handle. Token exchange is JSON against the pinned Platform token origin.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::lock::Mutex as AsyncMutex;
use plakous_auth::oauth_orchestrator::{
    BuildRequestFailure, ClaimedRefreshHooks, CredentialRefreshDetail,
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

/// Credential-free immutable facts for the built-in Claude OAuth integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeSubscriptionIntegrationProfile;

impl ClaudeSubscriptionIntegrationProfile {
    /// Returns the exact immutable profile identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
    }

    /// Returns the immutable source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        CLAUDE_OAUTH_SOURCE_REVISION
    }

    /// Returns the exact operator notice revision.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        CLAUDE_OAUTH_NOTICE_REVISION
    }

    /// Returns the review timestamp.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        CLAUDE_OAUTH_REVIEWED_AT_UNIX
    }

    /// Returns the hard compatibility expiry.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        CLAUDE_OAUTH_EXPIRES_AT_UNIX
    }

    /// Returns the locally reviewable kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        CLAUDE_OAUTH_KILL_SWITCH_REVISION
    }

    /// Reports the compiled kill-switch state.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        false
    }
}

/// A deliberate exact-revision acceptance capability.
pub struct ClaudeCompatibilityAcceptance {
    accepted_at: u64,
}

impl ClaudeCompatibilityAcceptance {
    /// Accepts exactly every immutable profile, source, notice, and kill revision.
    ///
    /// # Errors
    ///
    /// Rejects any alias, omitted revision, or older review timestamp.
    pub fn accept_exact(
        profile_id: &str,
        source_revision: &str,
        notice_revision: &str,
        kill_switch_revision: &str,
        accepted_at: Timestamp,
    ) -> Result<Self, ClaudeNativeAuthError> {
        if profile_id != CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
            || source_revision != CLAUDE_OAUTH_SOURCE_REVISION
            || notice_revision != CLAUDE_OAUTH_NOTICE_REVISION
            || kill_switch_revision != CLAUDE_OAUTH_KILL_SWITCH_REVISION
            || accepted_at.seconds() < CLAUDE_OAUTH_REVIEWED_AT_UNIX
        {
            return Err(ClaudeNativeAuthError::CompatibilityNotAccepted);
        }
        Ok(Self {
            accepted_at: accepted_at.seconds(),
        })
    }
}

impl fmt::Debug for ClaudeCompatibilityAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaudeCompatibilityAcceptance(<exact pinned revision>)")
    }
}

/// Credential-free selected-slot state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeSubscriptionStatus {
    generation: CredentialGeneration,
    status: CredentialStatus,
}

impl ClaudeSubscriptionStatus {
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
}

/// One enrolled Claude OAuth coding model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeEnrolledModel {
    exact_id: &'static str,
    input_tokens: u64,
}

impl ClaudeEnrolledModel {
    /// Exact catalog model id.
    #[must_use]
    pub const fn exact_id(self) -> &'static str {
        self.exact_id
    }

    /// Offline reference-client catalog value for presentation only.
    ///
    /// This value is not account entitlement or context-admission evidence.
    #[must_use]
    pub const fn input_tokens(self) -> u64 {
        self.input_tokens
    }
}

/// Offline maintenance handle for one explicit Claude selected slot.
pub struct ClaudeOfflineSelectedSlot {
    store: PrivateCredentialStore,
}

impl ClaudeOfflineSelectedSlot {
    /// Opens exactly the application-supplied private slot without network.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe path, slot, or foreign/malformed stored record.
    pub fn open_explicit(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
    ) -> Result<Self, ClaudeNativeAuthError> {
        Ok(Self {
            store: PrivateCredentialStore::open_slot(path, auth_profile_id()?, slot)?,
        })
    }

    /// Returns only the credential-free local lifecycle state.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe or malformed explicit storage.
    pub fn status(&self) -> Result<OfflineCredentialLifecycle, ClaudeNativeAuthError> {
        let status = validate_current_ready_record(&self.store)
            .and_then(|()| Ok(self.store.offline_lifecycle()?));
        match status {
            Err(
                ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild
                | ClaudeNativeAuthError::Store(PrivateStoreError::WrittenByNewerBuild),
            ) => Ok(OfflineCredentialLifecycle::WrittenByNewerBuild),
            status => status,
        }
    }

    /// Writes a local revoked tombstone without network I/O.
    ///
    /// # Errors
    ///
    /// Rejects a stale mutation or unsafe/malformed explicit storage.
    pub fn logout_local(&self) -> Result<OfflineCredentialLifecycle, ClaudeNativeAuthError> {
        Ok(self.store.logout_local()?)
    }
}

impl fmt::Debug for ClaudeOfflineSelectedSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaudeOfflineSelectedSlot(<explicit credential-free maintenance>)")
    }
}

/// App-owned manager for one explicit Claude selected-account slot.
#[derive(Clone)]
pub struct ClaudeNativeAuthManager {
    core: Arc<ManagerCore>,
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    refresh_observer: Option<Arc<dyn CredentialRefreshObserver>>,
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

impl ClaudeNativeAuthManager {
    #[cfg(test)]
    pub(crate) fn with_test_token_origin(
        mut self,
        origin: &Origin,
    ) -> Result<Self, ClaudeNativeAuthError> {
        let address = origin
            .binding_authority()
            .parse::<std::net::SocketAddr>()
            .map_err(|_| ClaudeNativeAuthError::NotReady)?;
        if !address.ip().is_loopback() {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let core = Arc::get_mut(&mut self.core).ok_or(ClaudeNativeAuthError::NotReady)?;
        core.token_origin = format!("https://{}", origin.binding_authority());
        Ok(self)
    }

    /// Opens one explicit private selected-slot record.
    ///
    /// # Errors
    ///
    /// Fails for an unaccepted/expired profile or unsafe explicit store path.
    pub fn open_selected_slot(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
        client: Client,
        acceptance: ClaudeCompatibilityAcceptance,
        device_id: crate::ClaudeClientDeviceId,
    ) -> Result<Self, ClaudeNativeAuthError> {
        validate_acceptance(&acceptance, system_unix_seconds()?)?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                device_id,
                store: PrivateCredentialStore::open_slot(path, auth_profile_id()?, slot)?,
                client,
                acceptance,
                token_origin: CLAUDE_OAUTH_TOKEN_ORIGIN.to_owned(),
                #[cfg(test)]
                control_origin: None,
                refresh_flight: AsyncMutex::new(()),
            }),
            request_gate: None,
            refresh_observer: None,
        })
    }

    /// Attaches the classified physical-request gate required for inference.
    ///
    /// Catalog observation is classified by the caller. Leaving the gate absent
    /// keeps login/status usable; smoke and campaign dispatch attach it.
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

    pub(crate) fn request_gate(&self) -> Option<Arc<dyn SubscriptionRequestGate>> {
        self.request_gate.clone()
    }

    /// Returns the current credential-free status.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed or unsafe storage.
    pub fn status(&self) -> Result<Option<ClaudeSubscriptionStatus>, ClaudeNativeAuthError> {
        validate_current_ready_record(&self.core.store)?;
        Ok(self
            .core
            .store
            .load()?
            .map(|fence| ClaudeSubscriptionStatus {
                generation: fence.generation(),
                status: fence.status(),
            }))
    }

    /// Performs the Claude Code `/usage` control-plane reads.
    ///
    /// `GET /api/oauth/usage` supplies the session, all-models weekly, and
    /// Fable-scoped meters. `GET /api/oauth/profile` supplies the account
    /// e-mail Claude Code's `/status` shows. Profile failure does not fail
    /// the meters. This observation acquires no inference permit and grants
    /// no catalog, admission, retry, billing, or inference authority.
    pub async fn subscription_usage(&self) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(system_unix_seconds().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        if self.core.ensure_active().is_err() {
            return unavailable(SubscriptionUsageUnavailable::Unqualified);
        }
        // Usage never starts a refresh. Claude refresh tokens rotate and a
        // claimed refresh cannot be restored once it may have reached the
        // provider, so a usage read dropped by its caller's timeout mid-claim
        // would leave a durable Unknown fence that only a new login clears.
        // An expired access token is named `RefreshRequired` below; the next
        // turn refreshes it.
        let Some(_lifecycle) = self.core.refresh_flight.try_lock() else {
            return unavailable(SubscriptionUsageUnavailable::Busy);
        };
        match validate_current_ready_record(&self.core.store) {
            Ok(()) => {}
            Err(
                ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild
                | ClaudeNativeAuthError::Store(PrivateStoreError::WrittenByNewerBuild),
            ) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::WrittenByNewerBuild,
                ));
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        }
        let fence = match self.core.store.load() {
            Ok(Some(fence)) => fence,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::NotConnected),
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
        if stored.validate().is_err() {
            return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
        }
        let Ok(now) = system_unix_seconds() else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        if stored.expires_at <= now {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        if lease.fence() != fence || stored.grant_binding != fence.account_binding().as_str() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }

        let headers = match oauth_control_headers(&stored) {
            Ok(headers) => headers,
            Err(reason) => return unavailable(reason),
        };
        let Ok(usage_request) = self
            .core
            .oauth_control_get(CLAUDE_SUBSCRIPTION_USAGE_PATH, headers.clone())
        else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        let profile_request = self
            .core
            .oauth_control_get(CLAUDE_SUBSCRIPTION_PROFILE_PATH, headers)
            .ok();
        let (usage_reply, profile_reply) =
            tokio::join!(self.core.execute_usage(usage_request), async {
                match profile_request {
                    Some(request) => self.core.execute_usage(request).await.ok(),
                    None => None,
                }
            });
        let (status, is_json, body) = match usage_reply {
            Ok(reply) => reply,
            Err(UsageTransportError::TooLarge) => {
                return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
            }
            Err(UsageTransportError::Failure(failure)) => {
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
        match crate::decode_claude_subscription_usage(&body, observed_at) {
            Ok(snapshot) if snapshot.contains_ended_window() => {
                SubscriptionUsageRead::Unavailable {
                    observed_at,
                    reason: SubscriptionUsageUnavailable::Stale,
                }
            }
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(attach_claude_profile(
                snapshot,
                profile_reply.as_ref(),
            )),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    /// Begins PKCE Claude OAuth. Completion is always paste of code or callback URL.
    ///
    /// # Errors
    ///
    /// Fails if the compatibility profile is inactive or randomness is unavailable.
    pub fn begin_login(&self) -> Result<ClaudeOAuthLogin, ClaudeNativeAuthError> {
        self.core.ensure_active()?;
        let deadline = ManagerCore::flow_deadline(LOGIN_TIMEOUT)?;
        let baseline = self.core.store.load()?;
        let pkce = PkceS256::generate()?;
        let authorization_url =
            claude_authorize_url(pkce.challenge().as_str(), pkce.verifier().expose_secret());
        self.core.ensure_flow_active(deadline)?;
        Ok(ClaudeOAuthLogin {
            core: Arc::clone(&self.core),
            baseline,
            pkce,
            authorization_url,
            deadline,
        })
    }

    /// Refreshes the selected login when its exact stored expiry enters skew.
    ///
    /// # Errors
    ///
    /// Ambiguous rotating-token results remain quarantined.
    pub async fn refresh_if_needed(&self) -> Result<(), ClaudeNativeAuthError> {
        self.refresh_with_force(false).await
    }

    /// Forces one claimed refresh after the provider answered a Messages
    /// dispatch with HTTP 401, regardless of the stored expiry.
    ///
    /// # Errors
    ///
    /// Same fail-closed failures as [`Self::refresh_if_needed`]; a rejected
    /// grant leaves the credential not Ready and is never retried here.
    pub async fn refresh_after_rejection(&self) -> Result<(), ClaudeNativeAuthError> {
        self.refresh_with_force(true).await
    }

    /// Stored access-token expiry (Unix seconds) of the Ready credential.
    ///
    /// # Errors
    ///
    /// Fails for a missing, non-Ready, or unreadable record.
    pub fn access_expires_at(&self) -> Result<u64, ClaudeNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        Ok(self.core.read_stored(&fence)?.expires_at)
    }

    async fn refresh_with_force(&self, force: bool) -> Result<(), ClaudeNativeAuthError> {
        validate_current_ready_record(&self.core.store)?;
        let flight = self.core.refresh_flight.lock().await;
        let mut hooks = ClaudeRefreshHooks {
            core: &self.core,
            request_gate: self.request_gate.as_ref(),
            observer: self.refresh_observer.as_deref(),
            _flight: flight,
            request_permit: None,
            force,
            detail: CredentialRefreshDetail::default(),
        };
        run_claimed_refresh(&self.core.store, &self.core.client, &mut hooks).await
    }

    /// Replaces the current credential with a local revoked tombstone.
    ///
    /// Claude Code OAuth has no pinned remote revoke endpoint.
    ///
    /// # Errors
    ///
    /// Rejects stale or missing state.
    pub fn logout_local(&self) -> Result<CredentialGeneration, ClaudeNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        Ok(self.core.store.logout(fence.generation())?)
    }

    /// Local-only retirement. There is no pinned Claude remote revoke.
    ///
    /// # Errors
    ///
    /// Rejects stale or missing state.
    pub fn revoke(&self) -> Result<CredentialGeneration, ClaudeNativeAuthError> {
        self.logout_local()
    }

    /// Returns the pinned enrolled coding catalog when the selected slot is Ready.
    ///
    /// # Errors
    ///
    /// Requires a Ready selected credential.
    pub fn enrolled_catalog(&self) -> Result<Vec<ClaudeEnrolledModel>, ClaudeNativeAuthError> {
        validate_current_ready_record(&self.core.store)?;
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        Ok(crate::oauth::CLAUDE_ENROLLMENTS
            .iter()
            .filter(|row| row.oauth)
            .map(|row| ClaudeEnrolledModel {
                exact_id: row.model,
                input_tokens: row.context_window,
            })
            .collect())
    }

    /// Resolves missing account metadata using the authenticated profile endpoint.
    /// The credential lease fences the GET; the existing generation CAS fences
    /// persistence after releasing that lease. A concurrent login/refresh wins.
    pub(crate) async fn ensure_account_identity(&self) -> Result<(), ClaudeNativeAuthError> {
        let _flight = self.core.refresh_flight.lock().await;
        self.core.ensure_active()?;
        let mut observed_account = None;
        let mut observed_uuid = None;
        for _ in 0..3 {
            let core = Arc::clone(&self.core);
            let (fence, lease) = tokio::task::spawn_blocking(move || {
                let fence = core
                    .store
                    .load()?
                    .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
                let lease = core.store.acquire_ready(&fence)?;
                Ok::<_, ClaudeNativeAuthError>((fence, lease))
            })
            .await
            .map_err(|_| ClaudeNativeAuthError::NotReady)??;
            if observed_account
                .as_ref()
                .is_some_and(|account| account != fence.account_binding())
            {
                return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
            }
            observed_account = Some(fence.account_binding().clone());
            let mut stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
                .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
            stored.validate()?;
            if stored.grant_binding != fence.account_binding().as_str() {
                return Err(ClaudeNativeAuthError::InvalidStoredCredential);
            }
            if let Some(uuid) = &stored.account_uuid {
                if observed_uuid
                    .as_ref()
                    .is_some_and(|observed| observed != uuid)
                {
                    return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
                }
                return Ok(());
            }
            if stored.expires_at <= system_unix_seconds()? {
                return Err(ClaudeNativeAuthError::NotReady);
            }
            let headers = oauth_control_headers(&stored)
                .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
            let request = self
                .core
                .oauth_control_get(CLAUDE_SUBSCRIPTION_PROFILE_PATH, headers)?;
            let (status, is_json, body) =
                self.core.execute_usage(request).await.map_err(|error| {
                    ClaudeNativeAuthError::Transport(match error {
                        UsageTransportError::Failure(failure) => network_failure(failure),
                        UsageTransportError::TooLarge => None,
                    })
                })?;
            if status.as_u16() != 200 || !is_json {
                return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
            }
            let profile = crate::decode_claude_oauth_profile(&body)
                .map_err(|_| ClaudeNativeAuthError::AccountIdentityUnavailable)?;
            stored.account_uuid = Some(
                profile
                    .account_uuid
                    .ok_or(ClaudeNativeAuthError::AccountIdentityUnavailable)?,
            );
            if observed_uuid
                .as_ref()
                .is_some_and(|observed| Some(observed) != stored.account_uuid.as_ref())
            {
                return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
            }
            observed_uuid = stored.account_uuid.clone();
            drop(lease);
            let replacement = credential_record(fence.generation().next()?, &stored)?;
            // Another manager can retain a Ready lease across its profile GET.
            // Never block the executor while waiting for that reader to finish.
            let core = Arc::clone(&self.core);
            let replaced = tokio::task::spawn_blocking(move || {
                core.store.replace(fence.generation(), &replacement)
            })
            .await
            .map_err(|_| ClaudeNativeAuthError::NotReady)?;
            match replaced {
                Ok(()) => return Ok(()),
                Err(PrivateStoreError::StaleWriter) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(ClaudeNativeAuthError::NotReady)
    }

    #[cfg(test)]
    pub(crate) fn request_identity(
        &self,
    ) -> Result<crate::ClaudeOAuthRequestIdentity, ClaudeNativeAuthError> {
        self.identity_credentials().map(|(_, identity)| identity)
    }

    pub(crate) fn identity_credentials(
        &self,
    ) -> Result<
        (
            (String, u64, AccountBinding),
            crate::ClaudeOAuthRequestIdentity,
        ),
        ClaudeNativeAuthError,
    > {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        let account = stored
            .account_uuid
            .clone()
            .ok_or(ClaudeNativeAuthError::AccountIdentityUnavailable)?;
        Ok((
            (
                stored.access_token.clone(),
                fence.generation().get(),
                fence.account_binding().clone(),
            ),
            crate::ClaudeOAuthRequestIdentity::new(account, self.core.device_id.clone()),
        ))
    }

    /// Returns the Ready access token and credential generation for dispatch.
    pub(crate) fn dispatch_credentials(
        &self,
    ) -> Result<(String, u64, AccountBinding), ClaudeNativeAuthError> {
        validate_current_ready_record(&self.core.store)?;
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        Ok((
            stored.access_token.clone(),
            fence.generation().get(),
            fence.account_binding().clone(),
        ))
    }

    #[cfg(test)]
    pub(crate) fn persist_test_grant(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_in: i64,
        scope: &str,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        let body = format!(
            r#"{{"access_token":"{access_token}","refresh_token":"{refresh_token}","expires_in":{expires_in},"scope":"{scope}","account":{{"uuid":"12345678-1234-4234-8234-123456789abc"}}}}"#
        );
        let grant = parse_claude_oauth_token_grant(body.as_bytes())
            .map_err(|_| ClaudeNativeAuthError::InvalidTokenResponse)?;
        self.core
            .persist_grant(self.core.store.load()?.as_ref(), &grant)
    }
}

impl fmt::Debug for ClaudeNativeAuthManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeNativeAuthManager")
            .field("profile", &CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID)
            .finish_non_exhaustive()
    }
}

/// Non-cloneable paste-complete Claude OAuth login.
pub struct ClaudeOAuthLogin {
    core: Arc<ManagerCore>,
    baseline: Option<CredentialFence>,
    pkce: PkceS256,
    authorization_url: String,
    deadline: Instant,
}

impl ClaudeOAuthLogin {
    /// Returns the exact authorization URL the app should open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Validates pasted authorization input and exchanges the code for tokens.
    ///
    /// # Errors
    ///
    /// Fails closed for callback, deadline, token, scope, or storage errors.
    pub async fn complete(
        self,
        authorization_input: &str,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        self.core.ensure_flow_active(self.deadline)?;
        let verifier = self.pkce.verifier().expose_secret();
        let code = parse_claude_oauth_authorization_input(authorization_input, verifier)
            .ok_or(ClaudeNativeAuthError::InvalidCallback)?;
        let body = claude_token_code_body(&code, CLAUDE_OAUTH_REDIRECT_URI, verifier, verifier);
        let response = until_flow_deadline(self.deadline, self.core.post_json(body)).await?;
        let grant = parse_claude_oauth_token_grant(&response)
            .map_err(|_| ClaudeNativeAuthError::LoginTokenExchange)?;
        if grant
            .scope()
            .is_none_or(|scope| !claude_oauth_scope_admits_inference(scope))
        {
            return Err(ClaudeNativeAuthError::LoginAccountVerification);
        }
        if grant.refresh_token().is_none() {
            return Err(ClaudeNativeAuthError::LoginTokenExchange);
        }
        self.core.ensure_flow_active(self.deadline)?;
        self.core.persist_grant(self.baseline.as_ref(), &grant)
    }
}

impl fmt::Debug for ClaudeOAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaudeOAuthLogin(<redacted non-cloneable login>)")
    }
}

impl ManagerCore {
    fn ensure_active(&self) -> Result<(), ClaudeNativeAuthError> {
        validate_acceptance(&self.acceptance, system_unix_seconds()?)
    }

    fn flow_deadline(limit: Duration) -> Result<Instant, ClaudeNativeAuthError> {
        Instant::now()
            .checked_add(limit)
            .ok_or(ClaudeNativeAuthError::LoginExpired)
    }

    fn ensure_flow_active(&self, deadline: Instant) -> Result<(), ClaudeNativeAuthError> {
        self.ensure_active()?;
        remaining_until(deadline, Instant::now()).ok_or(ClaudeNativeAuthError::LoginExpired)?;
        Ok(())
    }

    fn persist_grant(
        &self,
        baseline: Option<&CredentialFence>,
        grant: &ClaudeOAuthTokenGrant,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        let generation = match baseline {
            Some(fence) => fence.generation().next()?,
            None => CredentialGeneration::new(1)?,
        };
        let record = credential_record_from_grant(generation, grant)?;
        let fence = if let Some(expected) = baseline {
            self.store.replace_selected_login(expected, record)?
        } else {
            self.store.initialize_selected(&record)?;
            record.fence()
        };
        Ok(ClaudeSubscriptionStatus {
            generation: fence.generation(),
            status: fence.status(),
        })
    }

    async fn post_json(&self, body: String) -> Result<Zeroizing<Vec<u8>>, ClaudeNativeAuthError> {
        let request = self.json_request(body)?;
        let control = ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(|failure| ClaudeNativeAuthError::Transport(network_failure(failure)))?;
        let status = response.status();
        let mut body = response.into_body();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk
                .map_err(|failure| ClaudeNativeAuthError::Transport(network_failure(failure)))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_TOKEN_BYTES {
                let _ = body.close().await;
                return Err(ClaudeNativeAuthError::LoginTokenExchange);
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(ClaudeNativeAuthError::LoginTokenExchange);
        }
        Ok(bytes)
    }

    fn json_request(&self, body: String) -> Result<Request, ClaudeNativeAuthError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(HEADER_USER_AGENT, HeaderValue::from_static(USER_AGENT));
        let mut body = Zeroizing::new(body);
        Request::new(
            Origin::parse(&self.token_origin)?,
            Method::POST,
            CLAUDE_OAUTH_TOKEN_PATH,
            headers,
            SensitiveBytes::new(std::mem::take(&mut *body).into_bytes()).into_bytes(),
            self.client.limits(),
        )
        .map_err(Into::into)
    }

    fn oauth_control_get(
        &self,
        path: &'static str,
        headers: HeaderMap,
    ) -> Result<Request, ClaudeNativeAuthError> {
        let origin = CLAUDE_OAUTH_MESSAGES_ORIGIN;
        #[cfg(test)]
        let origin = self.control_origin.as_deref().unwrap_or(origin);
        Request::new(
            Origin::parse(origin)?,
            Method::GET,
            path,
            headers,
            Bytes::new(),
            self.client.limits(),
        )
        .map_err(Into::into)
    }

    async fn execute_usage(
        &self,
        request: Request,
    ) -> Result<(StatusCode, bool, Zeroizing<Vec<u8>>), UsageTransportError> {
        let control = ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(UsageTransportError::Failure)?;
        let status = response.status();
        let is_json = response.headers().iter().any(|(name, value)| {
            *name == HEADER_CONTENT_TYPE
                && value
                    .to_str()
                    .ok()
                    .and_then(|media| media.split(';').next())
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        });
        let mut body = response.into_body();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk.map_err(UsageTransportError::Failure)?;
            if bytes.len().saturating_add(chunk.len()) > MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES {
                let _ = body.close().await;
                return Err(UsageTransportError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, is_json, bytes))
    }

    fn read_stored(&self, fence: &CredentialFence) -> Result<StoredTokens, ClaudeNativeAuthError> {
        let lease = self.store.acquire_ready(fence)?;
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        stored.validate()?;
        if stored.grant_binding != fence.account_binding().as_str() {
            return Err(ClaudeNativeAuthError::InvalidStoredCredential);
        }
        Ok(stored)
    }
}

struct ClaudeRefreshHooks<'a> {
    core: &'a ManagerCore,
    request_gate: Option<&'a Arc<dyn SubscriptionRequestGate>>,
    observer: Option<&'a dyn CredentialRefreshObserver>,
    _flight: futures_util::lock::MutexGuard<'a, ()>,
    request_permit: Option<Box<dyn SubscriptionRequestPermit>>,
    /// Refresh even when the stored expiry is outside the skew (HTTP 401).
    force: bool,
    /// Numbers and rejecting check of the received response, for diagnostics.
    detail: CredentialRefreshDetail,
}

struct ClaudeRefreshPrepared {
    account_uuid: Option<crate::ClaudeAccountUuid>,
    refresh_token: Zeroizing<String>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRefreshHooks for ClaudeRefreshHooks<'_> {
    type Error = ClaudeNativeAuthError;
    type Prepared = ClaudeRefreshPrepared;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() == CredentialStatus::RefreshRequired {
            self.request_permit = acquire_refresh_permit(self.request_gate)?;
            return Ok(Some(fence));
        }
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        if self.force
            || access_needs_refresh(
                stored.expires_at,
                system_unix_seconds()?,
                REFRESH_SKEW_SECONDS,
            )
        {
            self.request_permit = acquire_refresh_permit(self.request_gate)?;
            Ok(Some(fence))
        } else {
            Ok(None)
        }
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
        let mut old: StoredTokens = match serde_json::from_slice(claim.payload().expose_secret()) {
            Ok(old) => old,
            Err(_) => {
                return Err(BuildRequestFailure::LeaveQuarantined(
                    ClaudeNativeAuthError::InvalidStoredCredential,
                ));
            }
        };
        if let Err(error) = old.validate() {
            return Err(BuildRequestFailure::LeaveQuarantined(error));
        }
        if old.grant_binding != claim.fence().account_binding().as_str() {
            return Err(BuildRequestFailure::LeaveQuarantined(
                ClaudeNativeAuthError::InvalidStoredCredential,
            ));
        }
        let refresh_token = Zeroizing::new(std::mem::take(&mut old.refresh_token));
        let request = match self
            .core
            .json_request(claude_token_refresh_body(&refresh_token))
        {
            Ok(request) => request,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        Ok((
            ClaudeRefreshPrepared {
                refresh_token,
                account_uuid: old.account_uuid.clone(),
            },
            request,
        ))
    }

    async fn authorize_dispatch(&mut self) -> Result<(), Self::Error> {
        if let Some(permit) = &mut self.request_permit
            && permit.authorize_dispatch().is_err()
        {
            return Err(ClaudeNativeAuthError::NotReady);
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
        let mut reject = |reason| {
            self.detail.reason = Some(reason);
            ClaudeNativeAuthError::RefreshQuarantined
        };
        if !response.status().is_success() {
            return Err(reject(CredentialRefreshFailureReason::ProviderRejected));
        }
        let body = pull_claimed_body(response, MAX_TOKEN_BYTES)
            .await
            .map_err(map_pull_body_error)?;
        let grant = parse_claude_oauth_token_grant(&body)
            .map_err(|_| reject(CredentialRefreshFailureReason::TokenShape))?;
        self.detail = CredentialRefreshDetail::from_response(
            system_unix_seconds().unwrap_or(0),
            u64::try_from(grant.expires_in()).ok(),
            None,
            MAX_TOKEN_BYTES,
        );
        if grant
            .scope()
            .is_none_or(|scope| !claude_oauth_scope_admits_inference(scope))
        {
            self.detail.reason = Some(CredentialRefreshFailureReason::TokenShape);
            return Err(ClaudeNativeAuthError::RefreshQuarantined);
        }
        if let (Some(old), Some(new)) = (&prepared.account_uuid, grant.account_uuid())
            && old != new
        {
            self.detail.reason = Some(CredentialRefreshFailureReason::SubjectMismatch);
            return Err(ClaudeNativeAuthError::RefreshQuarantined);
        }
        let account_uuid = grant.account_uuid().cloned().or(prepared.account_uuid);
        let refresh = grant
            .refresh_token()
            .unwrap_or(prepared.refresh_token.as_str());
        let stored = stored_tokens(
            grant.access_token(),
            refresh,
            grant.expires_in(),
            grant.scope().unwrap_or(CLAUDE_OAUTH_INFERENCE_SCOPE),
            response.refresh_fence().account_binding().as_str(),
            account_uuid,
        )?;
        let generation = response
            .refresh_fence()
            .generation()
            .next()
            .map_err(ClaudeNativeAuthError::from)?;
        credential_record(generation, &stored)
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        match error.disposition() {
            ClaimedHttpDisposition::RestoredNotStarted => ClaudeNativeAuthError::Transport(
                error.transport_failure().and_then(network_failure),
            ),
            ClaimedHttpDisposition::RestorationFailed | ClaimedHttpDisposition::ClaimRejected => {
                ClaudeNativeAuthError::RefreshQuarantined
            }
            _ => ClaudeNativeAuthError::RefreshQuarantined,
        }
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        match error {
            PrivateStoreError::HttpResponseBodyNotComplete
            | PrivateStoreError::GenerationNotSequential
            | PrivateStoreError::GenerationExhausted
            | PrivateStoreError::CredentialNotReady
            | PrivateStoreError::ForeignAccount
            | PrivateStoreError::InvalidRecord => ClaudeNativeAuthError::RefreshQuarantined,
            other => ClaudeNativeAuthError::Store(other),
        }
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.observer
    }

    fn failure_detail(&mut self, error: &Self::Error) -> CredentialRefreshDetail {
        let fallback = match error {
            ClaudeNativeAuthError::InvalidTokenResponse => {
                Some(CredentialRefreshFailureReason::TokenShape)
            }
            ClaudeNativeAuthError::InvalidStoredCredential => {
                Some(CredentialRefreshFailureReason::StoredCredential)
            }
            ClaudeNativeAuthError::StoredCredentialFormatRetired => {
                Some(CredentialRefreshFailureReason::RetiredFormat)
            }
            _ => None,
        };
        let reason = self.detail.reason.or(fallback);
        self.detail.with_reason(reason)
    }
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
        || acceptance.accepted_at < CLAUDE_OAUTH_REVIEWED_AT_UNIX
    {
        return Err(ClaudeNativeAuthError::CompatibilityInactive);
    }
    Ok(())
}

fn system_unix_seconds() -> Result<u64, ClaudeNativeAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(|_| ClaudeNativeAuthError::CompatibilityInactive)
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

enum UsageTransportError {
    Failure(plakous_http::Failure),
    TooLarge,
}

fn map_pull_body_error(error: PullBodyError) -> ClaudeNativeAuthError {
    match error {
        PullBodyError::TooLarge => ClaudeNativeAuthError::InvalidTokenResponse,
        PullBodyError::Transport | _ => ClaudeNativeAuthError::Transport(None),
    }
}

/// Redaction-safe native Claude OAuth failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClaudeNativeAuthError {
    /// The caller did not explicitly accept the exact pinned revision.
    #[error("the exact Claude OAuth compatibility profile was not accepted")]
    CompatibilityNotAccepted,
    /// The pinned compatibility profile expired or was killed in code.
    #[error("the pinned Claude OAuth compatibility profile is inactive")]
    CompatibilityInactive,
    /// The selected slot has no credential.
    #[error("the selected Claude OAuth slot is not logged in")]
    NotLoggedIn,
    /// The selected slot is not Ready.
    #[error("the selected Claude OAuth credential is not ready")]
    NotReady,
    /// The pasted authorization input failed correlation or grammar.
    #[error("the Claude OAuth authorization input is invalid")]
    InvalidCallback,
    /// The login expired before completion.
    #[error("the Claude OAuth login expired")]
    LoginExpired,
    /// Authorization-code token exchange failed.
    #[error("the Claude OAuth login token exchange failed")]
    LoginTokenExchange,
    /// Token scope did not admit inference.
    #[error("the Claude OAuth login account verification failed")]
    LoginAccountVerification,
    /// Authenticated account metadata is missing, malformed, or conflicting.
    #[error("the authenticated Claude OAuth account identity is unavailable")]
    AccountIdentityUnavailable,
    /// A verified login could not be committed durably.
    #[error("the Claude OAuth login credential could not be persisted")]
    LoginPersistence,
    /// A strict OAuth token response check failed.
    #[error("the Claude OAuth token response is invalid")]
    InvalidTokenResponse,
    /// Stored opaque provider data was malformed or foreign.
    #[error("the stored Claude OAuth credential is invalid")]
    InvalidStoredCredential,
    /// The stored credential was written in a format this build no longer reads.
    #[error("the stored Claude OAuth credential format is retired; log in again")]
    StoredCredentialFormatRetired,
    /// A newer build wrote the stored credential; this build neither uses
    /// nor rewrites it.
    #[error("the stored Claude OAuth credential was written by a newer build")]
    StoredCredentialWrittenByNewerBuild,
    /// Refresh entered durable Unknown and cannot be retried automatically.
    #[error("the Claude OAuth refresh outcome is quarantined")]
    RefreshQuarantined,
    /// The fixed authentication exchange failed.
    #[error("the Claude OAuth authentication exchange failed")]
    Transport(Option<plakous_model::ModelNetworkFailure>),
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
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid fixtures"
)]
mod tests {
    use super::*;
    use plakous_auth::CredentialRecord;

    #[tokio::test]
    async fn usage_expired_access_reports_refresh_required_without_claiming_refresh() {
        use plakous_auth::subscription_limit::{
            InMemorySubscriptionRequestGate, SubscriptionRequestLimits, SystemSubscriptionGateClock,
        };
        let temp = tempfile::tempdir().unwrap();
        let mut manager = manager(private_store(&temp));
        // Fail closed with no network: a regressed refresh finds no permit,
        // and any request at all would connect to this local listener.
        manager.request_gate = Some(Arc::new(InMemorySubscriptionRequestGate::new(
            SubscriptionRequestLimits::new(0, 0, 0, 0, None, 0).unwrap(),
            Arc::new(SystemSubscriptionGateClock::default()),
        )));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("https://{}", listener.local_addr().unwrap());
        let core = Arc::get_mut(&mut manager.core).unwrap();
        core.token_origin = origin.clone();
        core.control_origin = Some(origin);
        manager
            .persist_test_grant("offline-access", "offline-refresh", 3600, "user:inference")
            .unwrap();
        let fence = manager.core.store.load().unwrap().unwrap();
        let mut stored = manager.core.read_stored(&fence).unwrap();
        stored.expires_at = 1;
        manager
            .core
            .store
            .replace(
                fence.generation(),
                &credential_record(fence.generation().next().unwrap(), &stored).unwrap(),
            )
            .unwrap();
        let before = manager.core.store.load().unwrap().unwrap();
        // Dropped after its first poll, as the hosted usage timeout drops a
        // slow read; then read to completion.
        let _ = tokio::time::timeout(Duration::ZERO, manager.subscription_usage()).await;
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::RefreshRequired
                ),
                ..
            }
        ));
        let after = manager.core.store.load().unwrap().unwrap();
        assert_eq!(after, before);
        assert_eq!(after.status(), CredentialStatus::Ready);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "a usage read with an expired token must attempt no request"
        );
    }

    /// Another process's in-flight refresh claim makes the durable fence
    /// Unknown only transiently; it is retryable contention, not a quarantine.
    #[tokio::test]
    async fn usage_reports_peer_refresh_claim_as_busy_not_quarantined() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_store(&temp);
        let mut manager = manager(path.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("https://{}", listener.local_addr().unwrap());
        let core = Arc::get_mut(&mut manager.core).unwrap();
        core.token_origin = origin.clone();
        core.control_origin = Some(origin);
        manager
            .persist_test_grant("offline-access", "offline-refresh", 3600, "user:inference")
            .unwrap();
        let peer = PrivateCredentialStore::open_slot(
            path,
            auth_profile_id().unwrap(),
            CredentialSlotId::new("primary").unwrap(),
        )
        .unwrap();
        let fence = peer.load().unwrap().unwrap();
        let claim = peer.begin_refresh(&fence).unwrap().start();
        assert_eq!(
            manager.core.store.load().unwrap().unwrap().status(),
            CredentialStatus::RefreshOutcomeUnknown
        );
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::Busy,
                ..
            }
        ));
        drop(claim);
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::Quarantined
                ),
                ..
            }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn grant_lineage_binding_is_stable_and_not_the_route_name() {
        let first = oauth_grant_binding("refresh-token-lineage-one").expect("first binding");
        let same = oauth_grant_binding("refresh-token-lineage-one").expect("same binding");
        let other = oauth_grant_binding("refresh-token-lineage-two").expect("other binding");
        assert_eq!(first, same);
        assert_ne!(first, other);
        assert_ne!(first.as_str(), "claude-oauth");
        assert!(!first.as_str().contains("refresh-token"));
    }
    use plakous_http::Limits;

    pub(super) fn private_store(temp: &tempfile::TempDir) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = temp.path().join("credentials");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        directory.join("claude.json")
    }

    pub(super) fn manager(path: PathBuf) -> ClaudeNativeAuthManager {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
        let acceptance = ClaudeCompatibilityAcceptance::accept_exact(
            CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            CLAUDE_OAUTH_SOURCE_REVISION,
            CLAUDE_OAUTH_NOTICE_REVISION,
            CLAUDE_OAUTH_KILL_SWITCH_REVISION,
            now,
        )
        .unwrap();
        let slot = CredentialSlotId::new("primary").unwrap();
        let client = Client::new_http1(Limits::default()).unwrap();
        ClaudeNativeAuthManager::open_selected_slot(
            path,
            slot,
            client,
            acceptance,
            crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
        )
        .unwrap()
    }

    fn write_retired(path: PathBuf, payload: Vec<u8>) -> PrivateCredentialStore {
        let store = PrivateCredentialStore::open_slot(
            path,
            auth_profile_id().unwrap(),
            CredentialSlotId::new("primary").unwrap(),
        )
        .unwrap();
        let record = CredentialRecord::new(
            auth_profile_id().unwrap(),
            AccountBinding::new("claude-oauth").unwrap(),
            CredentialGeneration::new(1).unwrap(),
            CredentialStatus::Ready,
            SecretBytes::new(payload),
        )
        .unwrap();
        store.initialize_selected(&record).unwrap();
        store
    }

    #[test]
    fn begin_login_uses_cai_authorize_and_verifier_state() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(private_store(&temp));
        let login = manager.begin_login().unwrap();
        let url = login.authorization_url();
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
        assert!(url.contains("code=true"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        assert!(!url.contains("platform.claude.com/oauth/authorize"));
        assert_eq!(
            format!("{login:?}"),
            "ClaudeOAuthLogin(<redacted non-cloneable login>)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_paste_fails_before_token_exchange() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(private_store(&temp));
        let login = manager.begin_login().unwrap();
        assert!(matches!(
            login.complete("nope").await,
            Err(ClaudeNativeAuthError::InvalidCallback)
        ));
        assert!(manager.status().unwrap().is_none());
    }

    #[test]
    fn retired_ready_records_are_refused_and_left_unchanged() {
        for version in [1, 2] {
            let temp = tempfile::tempdir().unwrap();
            let path = private_store(&temp);
            let retired = serde_json::to_vec(&serde_json::json!({
                "version": version,
                "client_id": CLAUDE_OAUTH_CLIENT_ID,
                "scope": CLAUDE_OAUTH_INFERENCE_SCOPE,
                "access_token": "retired-access-token",
                "refresh_token": "retired-refresh-token",
                "expires_at": system_unix_seconds().unwrap() + 3_600,
            }))
            .unwrap();
            let store = write_retired(path.clone(), retired);
            let manager = manager(path.clone());
            assert!(matches!(
                manager.status(),
                Err(ClaudeNativeAuthError::StoredCredentialFormatRetired)
            ));
            let unchanged = store.load().unwrap().expect("retired fence remains");
            assert_eq!(unchanged.generation().get(), 1);
            assert_eq!(unchanged.account_binding().as_str(), "claude-oauth");
            let offline = ClaudeOfflineSelectedSlot::open_explicit(
                path,
                CredentialSlotId::new("primary").unwrap(),
            )
            .unwrap();
            assert!(matches!(
                offline.status(),
                Err(ClaudeNativeAuthError::StoredCredentialFormatRetired)
            ));
        }
    }

    #[test]
    fn persist_and_logout_round_trip_offline() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_store(&temp);
        let manager = manager(path.clone());
        let status = manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                "user:inference",
            )
            .unwrap();
        assert_eq!(status.status(), CredentialStatus::Ready);
        assert_eq!(manager.enrolled_catalog().unwrap().len(), 6);
        assert_eq!(
            manager.enrolled_catalog().unwrap()[0].exact_id(),
            "claude-sonnet-5"
        );
        assert!(
            !manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-fable-5")
        );
        assert!(
            manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-fable-5-1")
        );
        assert!(
            manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-opus-5")
        );
        assert!(
            manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-opus-5-5")
        );
        assert!(
            manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-sonnet-5-5")
        );
        assert!(
            manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-haiku-5-5")
        );
        assert!(
            !manager
                .enrolled_catalog()
                .unwrap()
                .iter()
                .any(|entry| entry.exact_id() == "claude-haiku-4-5")
        );
        manager.logout_local().unwrap();
        let slot = ClaudeOfflineSelectedSlot::open_explicit(
            path,
            CredentialSlotId::new("primary").unwrap(),
        )
        .unwrap();
        assert_eq!(slot.status().unwrap(), OfflineCredentialLifecycle::Revoked);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline fixture assertions")]
#[path = "native_auth_identity_tests.rs"]
mod identity_tests;
