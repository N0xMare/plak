//! The OAuth credentials of one running HTTP endpoint.
//!
//! Every request resolves its bearer here, refreshing first when the access
//! token is about to expire. A rejected token is renewed at most once, bound
//! to the generation the rejected request carried; the request itself is
//! never re-sent. Refusal texts name the embedder's login remedy, because
//! nothing in a session starts a login.

use std::sync::Arc;
use std::time::Instant;

use http::{HeaderValue, StatusCode};
use plakous_auth::oauth_orchestrator::{
    BuildRequestFailure, ClaimedRefreshHooks, pull_claimed_body, run_claimed_refresh,
};
use plakous_auth::oauth_workflow::{
    BearerChallenge, MAX_RESOURCE_OAUTH_RESPONSE_BYTES, oauth_error_code,
    parse_resource_token_response, resource_refresh_form, union_scopes,
};
use plakous_auth::{
    ClaimedHttpError, ClaimedHttpResponse, CredentialFence, CredentialGeneration, CredentialRecord,
    CredentialStatus, PrivateCredentialStore, PrivateStoreError, RefreshClaim,
};
use plakous_http::{ExchangeControl, SensitiveBytes, sensitive_header_value};
use plakous_tools::McpServerName;
use zeroize::Zeroize as _;

use super::McpOAuthSettings;
use super::credential::{
    LoginExpectation, StoredLogin, TokenEndpointAuth, open_store, read_ready, status, store_failed,
    unix_now,
};
use super::exchange::{ClientAuth, EXCHANGE_TIMEOUT, OAuthHttp};
use super::login::client_secret;
use crate::McpOAuthStatus;
use crate::error::{McpError, McpErrorKind};

/// The OAuth state of one server start.
pub(crate) struct OAuthSession {
    server: McpServerName,
    settings: McpOAuthSettings,
    resource: String,
    http: OAuthHttp,
    /// Serializes this process's refreshes; the store's claim serializes
    /// them across processes.
    refresh: tokio::sync::Mutex<()>,
}

/// The `Authorization` value of one request and the login generation it
/// came from.
pub(crate) struct Bearer {
    pub(crate) header: HeaderValue,
    pub(crate) generation: CredentialGeneration,
}

impl OAuthSession {
    pub(crate) fn new(
        server: McpServerName,
        settings: McpOAuthSettings,
        resource: String,
        http: OAuthHttp,
    ) -> Self {
        Self {
            server,
            settings,
            resource,
            http,
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    /// The bearer for the next request, refreshed first when it expires
    /// within the lesser of five minutes and half its issued lifetime.
    ///
    /// # Errors
    ///
    /// Returns operator text naming the login remedy when there is no usable
    /// login or it could not be renewed; nothing was sent.
    pub(crate) async fn bearer(&self) -> Result<Bearer, String> {
        let store = self.store()?;
        let due = {
            let (_, stored) = self.ready(&store)?;
            stored.refresh_token.is_some() && stored.refresh_due(unix_now())
        };
        if due {
            self.refresh(&store, None).await.map_err(|error| {
                self.refusal(&format!("could not renew its login ({error})"), None)
            })?;
        }
        let (fence, stored) = self.ready(&store)?;
        if stored.expires_in(unix_now()) == Some(0) {
            return Err(self.refusal("has an expired login", None));
        }
        let mut value = Vec::with_capacity(7 + stored.access_token.len());
        value.extend_from_slice(b"Bearer ");
        value.extend_from_slice(stored.access_token.as_bytes());
        let header = sensitive_header_value(SensitiveBytes::new(value)).map_err(|_| {
            self.refusal("has a stored access token that is not a header value", None)
        })?;
        Ok(Bearer {
            header,
            generation: fence.generation(),
        })
    }

    /// After a 401 on a request that carried `generation`: one claimed
    /// refresh, as its own task so a cancelled call cannot cut it short,
    /// and only while the store still holds that generation. Returns the
    /// failure text and whether the credentials were renewed.
    pub(crate) async fn rejected(
        self: &Arc<Self>,
        generation: CredentialGeneration,
    ) -> (String, bool) {
        let session = Arc::clone(self);
        let renewal = tokio::spawn(async move {
            let store = session.store()?;
            session
                .refresh(&store, Some(generation))
                .await
                .map_err(|error| {
                    session.refusal(
                        &format!(
                            "rejected its credentials and they could not be renewed ({error})"
                        ),
                        None,
                    )
                })
        })
        .await;
        match renewal {
            Ok(Ok(())) => (
                format!(
                    "mcp server {}: not executed: credentials were rejected and have been renewed; call again",
                    self.server
                ),
                true,
            ),
            Ok(Err(text)) => (text, false),
            Err(_) => (
                self.refusal(
                    "rejected its credentials and the renewal did not finish",
                    None,
                ),
                false,
            ),
        }
    }

    /// After a 403: with `insufficient_scope`, the step-up login with the
    /// union of the stored requested scope and the challenged one.
    pub(crate) fn forbidden(&self, challenge: Option<&BearerChallenge>) -> String {
        let Some(challenge) =
            challenge.filter(|challenge| challenge.error.as_deref() == Some("insufficient_scope"))
        else {
            return self.refusal("refused its credentials (HTTP 403)", None);
        };
        let challenged = challenge.scope.as_deref();
        let stored = self
            .store()
            .ok()
            .and_then(|store| self.ready(&store).ok())
            .and_then(|(_, stored)| stored.scope.clone());
        if [stored.as_deref(), challenged]
            .into_iter()
            .flatten()
            .any(|scope| !plakous_auth::oauth_workflow::valid_scope_list(scope))
        {
            return self.refusal("needs additional scope", None);
        }
        let scope = union_scopes([stored.as_deref(), challenged]);
        let needed = scope.as_deref().map_or_else(
            || "needs more scope".to_owned(),
            |scope| format!("needs scope \"{scope}\""),
        );
        self.refusal(&needed, scope.as_deref())
    }

    fn store(&self) -> Result<PrivateCredentialStore, String> {
        match open_store(self.settings.store()) {
            Ok(Some(store)) => Ok(store),
            Ok(None) => Err(self.refusal("is not logged in", None)),
            Err(error) => Err(self.refusal(&format!("could not read its login ({error})"), None)),
        }
    }

    /// The Ready login for this endpoint, or the refusal that says why
    /// there is none.
    fn ready(
        &self,
        store: &PrivateCredentialStore,
    ) -> Result<(CredentialFence, StoredLogin), String> {
        store
            .load()
            .ok()
            .flatten()
            .filter(|fence| fence.status() == CredentialStatus::Ready)
            .and_then(|fence| {
                read_ready(store, &fence)
                    .ok()
                    .filter(|stored| stored.belongs_to(&self.expectation()))
                    .map(|stored| (fence, stored))
            })
            .ok_or_else(|| self.unusable())
    }

    fn unusable(&self) -> String {
        let reason = match status(&self.settings, &self.resource) {
            Ok(McpOAuthStatus::NotLoggedIn) => "is not logged in",
            Ok(McpOAuthStatus::Refreshing) => "is renewing its login in another process",
            Ok(McpOAuthStatus::Quarantined) => "has a login whose renewal outcome is unknown",
            Ok(McpOAuthStatus::OtherServer) => "has a login made for another URL, issuer or client",
            Ok(McpOAuthStatus::WrittenByNewerBuild) => "has a login written by a newer build",
            _ => "has a login that cannot be used",
        };
        self.refusal(reason, None)
    }

    fn refusal(&self, reason: &str, scope: Option<&str>) -> String {
        format!(
            "mcp server {} {reason}; {}",
            self.server,
            self.settings.login_remedy(scope)
        )
    }

    fn expectation(&self) -> LoginExpectation<'_> {
        LoginExpectation::of(&self.settings, &self.resource)
    }

    /// One claimed refresh: when the token is due (`rejected` is `None`),
    /// or of exactly the `rejected` generation.
    async fn refresh(
        &self,
        store: &PrivateCredentialStore,
        rejected: Option<CredentialGeneration>,
    ) -> Result<(), McpError> {
        let _flight = self.refresh.lock().await;
        let mut hooks = RefreshHooks {
            session: self,
            store,
            rejected,
            answered: Answered::NotYet,
        };
        run_claimed_refresh(store, self.http.client(), &mut hooks).await
    }
}

/// How the token endpoint answered a refresh that failed.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Answered {
    NotYet,
    /// `invalid_grant`: the grant is gone and the login is retired.
    InvalidGrant,
    Otherwise,
}

struct RefreshHooks<'a> {
    session: &'a OAuthSession,
    store: &'a PrivateCredentialStore,
    rejected: Option<CredentialGeneration>,
    answered: Answered,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting"
)]
impl ClaimedRefreshHooks for RefreshHooks<'_> {
    type Error = McpError;
    type Prepared = StoredLogin;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let fence = self
            .store
            .load()
            .map_err(|error| store_failed("mcp login could not be read", error))?
            .ok_or_else(|| authorization("no login is stored"))?;
        if fence.status() != CredentialStatus::Ready {
            return Err(authorization("the login is not usable"));
        }
        // Another task or process renewed the rejected generation already.
        if self
            .rejected
            .is_some_and(|rejected| rejected != fence.generation())
        {
            return Ok(None);
        }
        let stored = read_ready(self.store, &fence)?;
        if !stored.belongs_to(&self.session.expectation()) {
            return Err(authorization("the login belongs to another server"));
        }
        if self.rejected.is_some() && stored.renewed_after_rejection {
            return Err(authorization(
                "the renewed credentials were rejected; a new login is required",
            ));
        }
        if stored.refresh_token.is_none() {
            return Err(authorization("the login has no refresh token"));
        }
        let due = self.rejected.is_some() || stored.refresh_due(unix_now());
        Ok(due.then_some(fence))
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, plakous_http::Request), BuildRequestFailure<Self::Error>> {
        let stored = StoredLogin::decode(claim.payload().expose_secret(), &claim.fence())
            .map_err(BuildRequestFailure::LeaveQuarantined)?;
        let secret =
            client_secret(&self.session.settings, &stored).map_err(BuildRequestFailure::Restore)?;
        let request = {
            let refresh_token = stored.refresh_token.as_deref().ok_or_else(|| {
                BuildRequestFailure::Restore(authorization("the login has no refresh token"))
            })?;
            let fields =
                resource_refresh_form(refresh_token, &stored.client_id, &stored.resource_indicator);
            self.session
                .http
                .form_request(
                    &stored.token_endpoint,
                    &fields,
                    &ClientAuth {
                        id: &stored.client_id,
                        method: if secret.is_some() {
                            stored.token_endpoint_auth
                        } else {
                            TokenEndpointAuth::None
                        },
                        secret: secret.as_deref().map(Vec::as_slice),
                    },
                )
                .map_err(BuildRequestFailure::Restore)?
        };
        Ok((stored, request))
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + EXCHANGE_TIMEOUT)
    }

    async fn validate_tokens(
        &mut self,
        mut stored: StoredLogin,
        response: &mut ClaimedHttpResponse,
    ) -> Result<CredentialRecord, Self::Error> {
        let status = response.status();
        let body = pull_claimed_body(response, MAX_RESOURCE_OAUTH_RESPONSE_BYTES)
            .await
            .map_err(|_| authorization("the refresh response could not be read"))?;
        if !status.is_success() {
            let code = oauth_error_code(&body);
            self.answered =
                if status == StatusCode::BAD_REQUEST && code.as_deref() == Some("invalid_grant") {
                    Answered::InvalidGrant
                } else {
                    Answered::Otherwise
                };
            return Err(authorization(&match code {
                Some(code) => format!(
                    "the token endpoint refused the refresh: {code} (HTTP {})",
                    status.as_u16()
                ),
                None => format!(
                    "the token endpoint refused the refresh with HTTP {}",
                    status.as_u16()
                ),
            }));
        }
        let tokens = parse_resource_token_response(&body)
            .map_err(|_| authorization("the refresh response is invalid"))?;
        stored.access_token.zeroize();
        tokens
            .access_token
            .expose_secret()
            .clone_into(&mut stored.access_token);
        stored.lifetime = tokens.expires_in;
        stored.renewed_after_rejection = self.rejected.is_some();
        stored.expires_at = tokens
            .expires_in
            .map(|seconds| unix_now().saturating_add(seconds));
        // A rotated refresh token replaces the stored one; without one the
        // stored token stays valid.
        if let Some(rotated) = &tokens.refresh_token {
            if let Some(old) = &mut stored.refresh_token {
                old.zeroize();
            }
            stored.refresh_token = Some(rotated.expose_secret().to_owned());
        }
        let generation = response
            .refresh_fence()
            .generation()
            .next()
            .map_err(|error| store_failed("mcp login generation is exhausted", error))?;
        stored.record(generation)
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        McpError::with_source(
            McpErrorKind::Authorization,
            format!("the refresh request failed: {error}"),
            error,
        )
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        store_failed("mcp login could not be refreshed", error)
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn after_post_start_failure(&mut self, store: &PrivateCredentialStore, _error: &Self::Error) {
        match self.answered {
            Answered::InvalidGrant => {
                if let Err(error) = store.logout_local() {
                    tracing::warn!(server = %self.session.server, %error, "mcp login was not retired after invalid_grant");
                }
            }
            Answered::NotYet | Answered::Otherwise => {}
        }
    }
}

fn authorization(message: &str) -> McpError {
    McpError::new(McpErrorKind::Authorization, message)
}
