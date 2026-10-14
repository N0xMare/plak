//! Dialect hooks for the claimed OAuth refresh/revoke orchestrator.

use std::future::Future;
use std::time::Duration;

use plakous_http::{Client as HttpClient, ExchangeControl, Request as HttpRequest};

use crate::{
    ClaimedHttpError, ClaimedHttpResponse, CredentialFence, CredentialGeneration, CredentialRecord,
    PrivateCredentialStore, PrivateStoreError, RefreshClaim,
};

/// Credential-refresh ladder phase (content-free).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CredentialRefreshPhase {
    /// Durable Unknown claim was taken before provider I/O.
    Began,
    /// Replacement Ready (or revoke tombstone) was committed.
    Completed,
    /// Post-send failure left the durable generation Unknown.
    OutcomeUnknown,
    /// The claim was left quarantined without a restore.
    Quarantined,
    /// Pre-send failure restored the pre-claim record.
    Restored,
}

/// Closed failure class for a credential-refresh observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CredentialRefreshFailureClass {
    /// Transport or protocol failure after the claim started.
    Transport,
    /// Absolute or phase deadline elapsed.
    Timeout,
    /// Provider HTTP status class (never a reason body).
    HttpStatus(HttpStatusClass),
    /// Local token/response validation rejected a received body.
    Validation,
    /// Private-store transition failed.
    Store,
    /// Caller cancellation was observed.
    Cancelled,
    /// Pre-send request-gate or readiness denied dispatch.
    NotReady,
}

/// HTTP status class carried on credential-refresh telemetry (never a URL).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpStatusClass {
    /// `4xx` client error.
    ClientError,
    /// `5xx` server error.
    ServerError,
    /// Any other status (1xx/2xx/3xx) when a class is still recorded.
    Other,
}

impl HttpStatusClass {
    /// Classifies a raw HTTP status code.
    #[must_use]
    pub const fn from_status(status: u16) -> Self {
        match status {
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            _ => Self::Other,
        }
    }
}

/// One content-free claimed-refresh (or revoke) ladder observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialRefreshEvent {
    /// Ladder phase reached by this observation.
    pub phase: CredentialRefreshPhase,
    /// Closed failure class when the phase is not success.
    pub failure_class: Option<CredentialRefreshFailureClass>,
    /// Physical transport phase when a network failure was observed.
    pub transport_phase: Option<plakous_http::Phase>,
    /// Raw HTTP status when a response was received.
    pub http_status: Option<u16>,
    /// Wall time from `begin_refresh` (claim take) through this phase.
    ///
    /// The clock starts before the durable claim is taken so the claim-take
    /// itself is included.
    pub elapsed: Duration,
    /// Durable credential generation at the claim.
    pub generation: u64,
}

/// Dialect-supplied observer for claimed-refresh ladder exits.
///
/// The auth crate does not depend on `plakous-agent`. Product code maps these
/// events onto `plakous_agent::TelemetryObservationKind::CredentialRefresh`.
pub trait CredentialRefreshObserver: Send + Sync {
    /// Accepts one content-free ladder observation. Must not block.
    fn observe(&self, event: CredentialRefreshEvent);
}

/// How to dispose of an unstarted claim when request construction fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum BuildRequestFailure<E> {
    /// Drop the claim and leave the durable Unknown quarantine in place.
    ///
    /// Use for corrupt payloads or binding mismatches discovered after the
    /// claim was taken (the old generation is already Unknown).
    LeaveQuarantined(E),
    /// Restore the pre-claim record via
    /// [`PrivateCredentialStore::restore_refresh_definitely_not_started`].
    ///
    /// Use only when provider I/O definitely did not start (local encode /
    /// header / request-build / pre-dispatch failures).
    Restore(E),
}

/// Dialect hooks for the shared claimed-refresh ladder.
///
/// The orchestrator owns claim lifecycle, execute, quarantine, and complete.
/// Dialects own wire shape, token crypto, and typed errors.
pub trait ClaimedRefreshHooks {
    /// Dialect error type (redaction-safe).
    type Error;
    /// State carried from claim payload through response validation.
    type Prepared: Send;
    /// Success value returned to the manager (usually `()`).
    type Output;

    /// Profile / kill-switch / acceptance admission (entry of the ladder).
    ///
    /// # Errors
    ///
    /// Returns a dialect error when the compatibility profile is inactive.
    fn check_profile(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Load the fence that needs refresh, or `Ok(None)` when access is still fresh.
    ///
    /// Dialects hold flight locks and optional request-gate permits here.
    ///
    /// # Errors
    ///
    /// Returns a dialect error for missing, changed, or unavailable credentials.
    fn fence_needing_refresh(
        &mut self,
    ) -> impl Future<Output = Result<Option<CredentialFence>, Self::Error>> + Send;

    /// Success when `fence_needing_refresh` reports no refresh is needed.
    ///
    /// # Errors
    ///
    /// Returns a dialect error only if constructing the success value fails.
    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error>;

    /// Build the provider refresh request from the unstarted claim payload.
    ///
    /// # Errors
    ///
    /// Returns [`BuildRequestFailure::LeaveQuarantined`] for corrupt payloads
    /// or binding mismatches, and [`BuildRequestFailure::Restore`] for local
    /// encode / header / request construction failures before transport.
    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, HttpRequest), BuildRequestFailure<Self::Error>>;

    /// Pre-start dispatch authorization (rate gate, external cancel).
    ///
    /// Failure restores the unstarted claim.
    ///
    /// # Errors
    ///
    /// Returns a dialect error when dispatch is denied or already cancelled.
    fn authorize_dispatch(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }

    /// Deadline / transport cancellation for the claimed exchange.
    fn exchange_control(&mut self) -> ExchangeControl;

    /// Execute the claimed HTTP exchange.
    ///
    /// Default calls [`PrivateCredentialStore::execute_claimed_http`]. Dialects
    /// may wrap with cancel-select while still awaiting the store future.
    ///
    /// # Errors
    ///
    /// Returns a store-owned claimed HTTP failure disposition.
    fn execute_claimed(
        &mut self,
        store: &PrivateCredentialStore,
        client: &HttpClient,
        claim: RefreshClaim,
        request: HttpRequest,
        control: ExchangeControl,
    ) -> impl Future<Output = Result<ClaimedHttpResponse, ClaimedHttpError>> + Send {
        async move {
            store
                .execute_claimed_http(claim, client, request, control)
                .await
        }
    }

    /// Parse the response, validate tokens, and build the next Ready record.
    ///
    /// On `Err`, the orchestrator quarantines the claimed response.
    ///
    /// # Errors
    ///
    /// Returns a dialect error for provider, validation, or crypto failures.
    fn validate_tokens(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> impl Future<Output = Result<CredentialRecord, Self::Error>> + Send;

    /// Re-check profile/acceptance immediately before complete.
    ///
    /// # Errors
    ///
    /// Returns a dialect error when the profile became inactive mid-refresh.
    fn recheck_profile_before_complete(
        &mut self,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }

    /// Map a store-owned claimed HTTP failure after the orchestrator has
    /// already restored any unstarted
    /// [`crate::ClaimedHttpDisposition::ClaimRejected`] claim.
    ///
    /// Dialects must not restore again; the claim is already taken or restored.
    fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> impl Future<Output = Self::Error> + Send;

    /// Map a bare private-store failure.
    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error;

    /// Map a successful complete fence into the dialect output.
    ///
    /// # Errors
    ///
    /// Returns a dialect error if post-complete bookkeeping fails.
    fn output_from_completed(
        &mut self,
        fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error>;

    /// Optional content-free observer for ladder exits. Default: none.
    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        None
    }

    /// When true, [`PrivateCredentialStore::begin_refresh_reclaiming_unknown`]
    /// is used instead of [`PrivateCredentialStore::begin_refresh`].
    fn reclaim_unknown_refresh(&self) -> bool {
        false
    }

    /// Dialect hook after a post-start failure that leaves durable Unknown.
    ///
    /// Default is a no-op. xAI restores retryable failures to Ready so the
    /// stored refresh token can be exchanged on the next claim.
    fn after_post_start_failure(&mut self, store: &PrivateCredentialStore, error: &Self::Error) {
        let _ = (store, error);
    }
}

/// Dialect hooks for the shared claimed-revoke ladder.
pub trait ClaimedRevokeHooks {
    /// Dialect error type (redaction-safe).
    type Error;
    /// Success value (status or generation).
    type Output;

    /// Profile admission before revoke.
    ///
    /// # Errors
    ///
    /// Returns a dialect error when the compatibility profile is inactive.
    fn check_profile(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Fence to revoke after flight / account checks.
    ///
    /// # Errors
    ///
    /// Returns a dialect error for missing or changed credentials.
    fn fence_to_revoke(
        &mut self,
    ) -> impl Future<Output = Result<CredentialFence, Self::Error>> + Send;

    /// Build the provider revoke request from the unstarted claim payload.
    ///
    /// # Errors
    ///
    /// Same disposition rules as
    /// [`ClaimedRefreshHooks::build_refresh_request`].
    fn build_revoke_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<HttpRequest, BuildRequestFailure<Self::Error>>;

    /// Pre-start dispatch authorization.
    ///
    /// # Errors
    ///
    /// Returns a dialect error when dispatch is denied.
    fn authorize_dispatch(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }

    /// Deadline / transport cancellation for the claimed exchange.
    fn exchange_control(&mut self) -> ExchangeControl;

    /// Execute the claimed HTTP exchange (default: store bridge).
    ///
    /// # Errors
    ///
    /// Returns a store-owned claimed HTTP failure disposition.
    fn execute_claimed(
        &mut self,
        store: &PrivateCredentialStore,
        client: &HttpClient,
        claim: RefreshClaim,
        request: HttpRequest,
        control: ExchangeControl,
    ) -> impl Future<Output = Result<ClaimedHttpResponse, ClaimedHttpError>> + Send {
        async move {
            store
                .execute_claimed_http(claim, client, request, control)
                .await
        }
    }

    /// Accept provider revoke response (status / body). Failure → quarantine.
    ///
    /// # Errors
    ///
    /// Returns a dialect error for non-success status or invalid body.
    fn accept_revoke_response(
        &mut self,
        response: &mut ClaimedHttpResponse,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Re-check profile before tombstone commit.
    ///
    /// # Errors
    ///
    /// Returns a dialect error when the profile became inactive mid-revoke.
    fn recheck_profile_before_complete(
        &mut self,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }

    /// Map a store-owned claimed HTTP failure after the orchestrator has
    /// already restored any unstarted
    /// [`crate::ClaimedHttpDisposition::ClaimRejected`] claim.
    fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> impl Future<Output = Self::Error> + Send;

    /// Map a bare private-store failure.
    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error;

    /// Map the revoked generation into dialect output (clear selection, etc.).
    ///
    /// # Errors
    ///
    /// Returns a dialect error if post-revoke bookkeeping fails.
    fn output_from_revoked(
        &mut self,
        generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error>;

    /// Optional content-free observer for ladder exits. Default: none.
    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        None
    }
}
