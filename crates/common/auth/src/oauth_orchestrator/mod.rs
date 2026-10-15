//! Claimed OAuth + browser loopback + JWKS orchestrators for subscription dialects.
//!
//! ## Claimed refresh / revoke
//!
//! Linear durable ladder: profile check → needs-refresh → begin-refresh →
//! execute-claimed-http → quarantine-on-fail / complete-on-success (and the
//! symmetric revoke terminal).
//!
//! ## Browser loopback
//!
//! Accept session ladder: bind → accept → loopback peer → read HTTP/1.1 head →
//! dialect parse → optional preflight → complete callback → write response.
//!
//! ## JWKS
//!
//! Cache ladder under a dialect-held mutex: purge negatives → fresh hit →
//! negative miss → fetch rounds → install → hit or record negative.
//!
//! Dialects supply hooks only: request construction, response parse, token
//! validation, redirect grammar, signing-key crypto, and error mapping.
//! Intentional wire deltas stay dialect-owned. Device authorization wire
//! remains dialect-owned (shared poll only).

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use plakous_http::{ErrorKind, Failure as HttpFailure, StatusCode};

use crate::{
    ClaimedHttpDisposition, ClaimedHttpError, ClaimedHttpResponse, PrivateCredentialStore,
    PrivateStoreError, RefreshClaim,
};

mod browser;
mod hooks;
mod jwks;
mod refresh;
mod revoke;

pub use browser::{
    BrowserLoopbackHooks, LoopbackConnectionEvent, ReadRequestHeadError, bind_loopback_listener,
    read_http11_request_head, run_browser_loopback_session, write_all_before_deadline,
};
pub use hooks::{
    BuildRequestFailure, ClaimedRefreshHooks, ClaimedRevokeHooks, CredentialRefreshDetail,
    CredentialRefreshEvent, CredentialRefreshFailureClass, CredentialRefreshFailureReason,
    CredentialRefreshObserver, CredentialRefreshPhase, HttpStatusClass,
};
pub use jwks::{FetchedJwks, JwksCache, JwksResolvePolicy, parse_cache_control_max_age};
pub use refresh::{run_claimed_refresh, run_claimed_refresh_from_claim};
pub use revoke::run_claimed_revoke;

struct RefreshAttempt {
    generation: u64,
    started: Instant,
    deadline: Instant,
}

impl RefreshAttempt {
    fn new(generation: u64, started: Instant) -> Self {
        Self {
            generation,
            started,
            deadline: started + Duration::from_secs(30),
        }
    }
}

fn emit_refresh(
    observer: Option<&dyn CredentialRefreshObserver>,
    attempt: &RefreshAttempt,
    phase: CredentialRefreshPhase,
    failure_class: Option<CredentialRefreshFailureClass>,
    http_status: Option<u16>,
    transport_phase: Option<plakous_http::Phase>,
) {
    emit_refresh_detailed(
        observer,
        attempt,
        phase,
        failure_class,
        http_status,
        transport_phase,
        CredentialRefreshDetail::default(),
    );
}

fn emit_refresh_detailed(
    observer: Option<&dyn CredentialRefreshObserver>,
    attempt: &RefreshAttempt,
    phase: CredentialRefreshPhase,
    failure_class: Option<CredentialRefreshFailureClass>,
    http_status: Option<u16>,
    transport_phase: Option<plakous_http::Phase>,
    detail: CredentialRefreshDetail,
) {
    let Some(observer) = observer else {
        return;
    };
    let event = CredentialRefreshEvent {
        phase,
        failure_class,
        http_status,
        transport_phase,
        elapsed: attempt.started.elapsed(),
        generation: attempt.generation,
        detail,
    };
    let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(event)));
}

fn transport_failure_class(failure: Option<HttpFailure>) -> Option<CredentialRefreshFailureClass> {
    let kind = failure?.kind();
    Some(match kind {
        ErrorKind::Cancelled => CredentialRefreshFailureClass::Cancelled,
        ErrorKind::DeadlineExceeded => CredentialRefreshFailureClass::Timeout,
        _ => CredentialRefreshFailureClass::Transport,
    })
}

fn http_status_class(status: StatusCode) -> CredentialRefreshFailureClass {
    CredentialRefreshFailureClass::HttpStatus(HttpStatusClass::from_status(status.as_u16()))
}

fn claimed_http_phase(
    error: &ClaimedHttpError,
) -> (
    CredentialRefreshPhase,
    Option<CredentialRefreshFailureClass>,
) {
    let failure = transport_failure_class(error.transport_failure());
    #[allow(
        unreachable_patterns,
        reason = "ClaimedHttpDisposition is non-exhaustive"
    )]
    match error.disposition() {
        ClaimedHttpDisposition::RestoredNotStarted => (
            CredentialRefreshPhase::Restored,
            failure.or(Some(CredentialRefreshFailureClass::Transport)),
        ),
        ClaimedHttpDisposition::ClaimRejected
        | ClaimedHttpDisposition::QuarantinedMayHaveTransmitted
        | ClaimedHttpDisposition::RestorationFailed
        | ClaimedHttpDisposition::QuarantineVerificationFailed => (
            CredentialRefreshPhase::OutcomeUnknown,
            failure.or(Some(CredentialRefreshFailureClass::Store)),
        ),
        _ => (
            CredentialRefreshPhase::OutcomeUnknown,
            Some(CredentialRefreshFailureClass::Store),
        ),
    }
}

/// Restores a `ClaimRejected` unstarted claim, then emits from the real durable result.
///
/// Polls [`PrivateCredentialStore::try_restore_refresh_definitely_not_started`]
/// with async backoff until `deadline`. Never blocks the runtime thread. A
/// never-sent claim is restored when the sibling releases the directory lock;
/// `OutcomeUnknown` is recorded only if the deadline passes still busy.
async fn emit_claimed_http_failure(
    store: &PrivateCredentialStore,
    observer: Option<&dyn CredentialRefreshObserver>,
    attempt: &RefreshAttempt,
    error: &mut ClaimedHttpError,
    deadline: Instant,
) {
    let (phase, failure_class) = if let Some(claim) = error.take_unstarted_claim() {
        if let Ok(true) = restore_unstarted_until(store, claim, deadline).await {
            error.set_disposition(ClaimedHttpDisposition::RestoredNotStarted);
            (
                CredentialRefreshPhase::Restored,
                transport_failure_class(error.transport_failure())
                    .or(Some(CredentialRefreshFailureClass::Transport)),
            )
        } else {
            error.set_disposition(ClaimedHttpDisposition::RestorationFailed);
            (
                CredentialRefreshPhase::OutcomeUnknown,
                Some(CredentialRefreshFailureClass::Store),
            )
        }
    } else {
        claimed_http_phase(error)
    };
    emit_refresh(
        observer,
        attempt,
        phase,
        failure_class,
        None,
        error.transport_failure().map(HttpFailure::phase),
    );
}

const RESTORE_POLL_INITIAL: Duration = Duration::from_millis(2);
const RESTORE_POLL_MAX: Duration = Duration::from_millis(50);

/// Polls a non-blocking restore until success, a non-busy failure, or `deadline`.
///
/// Returns `Ok(true)` when the pre-claim record was restored.
async fn restore_unstarted_until(
    store: &PrivateCredentialStore,
    mut claim: RefreshClaim,
    deadline: Instant,
) -> Result<bool, PrivateStoreError> {
    let mut backoff = RESTORE_POLL_INITIAL;
    loop {
        match store.try_restore_refresh_definitely_not_started(&mut claim) {
            Ok(Some(_)) => {
                drop(claim);
                return Ok(true);
            }
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    drop(claim);
                    return Ok(false);
                }
                let sleep_for = backoff.min(deadline.saturating_duration_since(now));
                tokio::time::sleep(sleep_for).await;
                backoff = backoff.saturating_mul(2).min(RESTORE_POLL_MAX);
            }
            Err(error) => {
                drop(claim);
                return Err(error);
            }
        }
    }
}

/// Runs `future` until `deadline`; past it, the future is dropped and the
/// flow fails with `expired`.
///
/// # Errors
///
/// `expired` at the deadline, otherwise the error `future` returns.
pub async fn until_deadline<F, T, E>(
    deadline: Instant,
    expired: impl FnOnce() -> E,
    future: F,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .unwrap_or_else(|_| Err(expired()))
}

/// Pull the claimed response body through clean EOF with a hard byte bound.
///
/// Dialects call this from validate/accept hooks. Cancellation policy is
/// dialect-owned (wrap this future or interleave `next_chunk` with a token).
///
/// # Errors
///
/// Returns [`PullBodyError::Transport`] on chunk failure or incomplete EOF, and
/// [`PullBodyError::TooLarge`] when the body would exceed `max_bytes`.
pub async fn pull_claimed_body(
    response: &mut ClaimedHttpResponse,
    max_bytes: usize,
) -> Result<zeroize::Zeroizing<Vec<u8>>, PullBodyError> {
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = response.next_chunk().await {
        let chunk = chunk.map_err(|_| PullBodyError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(PullBodyError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    if !response.body_completed_cleanly() {
        return Err(PullBodyError::Transport);
    }
    Ok(bytes)
}

/// Bounded body-pull failure (credential-free).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PullBodyError {
    /// Chunk transport failure or incomplete EOF.
    Transport,
    /// Body would exceed the dialect byte bound.
    TooLarge,
}

/// Restores an unstarted claim with bounded async polling, or maps a store failure.
///
/// The bool is `true` when the pre-claim record was restored. A still-busy
/// store at `deadline` is mapped as [`PrivateStoreError::StoreBusy`].
async fn restore_or_map<E>(
    store: &PrivateCredentialStore,
    claim: RefreshClaim,
    error: E,
    deadline: Instant,
    map_store: impl FnOnce(PrivateStoreError) -> E,
) -> (E, bool) {
    match restore_unstarted_until(store, claim, deadline).await {
        Ok(true) => (error, true),
        Ok(false) => (map_store(PrivateStoreError::StoreBusy), false),
        Err(store_error) => (map_store(store_error), false),
    }
}

/// Quarantines a claimed response; store failure replaces the original error.
///
/// The bool is `true` when quarantine verification succeeded.
fn quarantine_or_map<E>(
    store: &PrivateCredentialStore,
    response: ClaimedHttpResponse,
    error: E,
    map_store: impl FnOnce(PrivateStoreError) -> E,
) -> (E, bool) {
    match store.quarantine_claimed_http(response) {
        Ok(_) => (error, true),
        Err(store_error) => (map_store(store_error), false),
    }
}

/// Handles a pre-start build failure against an unstarted claim.
async fn handle_build_failure<E>(
    store: &PrivateCredentialStore,
    claim: RefreshClaim,
    failure: BuildRequestFailure<E>,
    deadline: Instant,
    map_store: impl FnOnce(PrivateStoreError) -> E,
) -> (
    E,
    CredentialRefreshPhase,
    Option<CredentialRefreshFailureClass>,
) {
    match failure {
        BuildRequestFailure::LeaveQuarantined(error) => {
            drop(claim);
            (
                error,
                CredentialRefreshPhase::Quarantined,
                Some(CredentialRefreshFailureClass::Validation),
            )
        }
        BuildRequestFailure::Restore(error) => {
            let (error, restored) = restore_or_map(store, claim, error, deadline, map_store).await;
            if restored {
                (
                    error,
                    CredentialRefreshPhase::Restored,
                    Some(CredentialRefreshFailureClass::Validation),
                )
            } else {
                (
                    error,
                    CredentialRefreshPhase::OutcomeUnknown,
                    Some(CredentialRefreshFailureClass::Store),
                )
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn until_deadline_drops_the_future_at_the_deadline() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let expired: Result<(), &str> =
            until_deadline(deadline, || "expired", std::future::pending()).await;
        assert_eq!(expired, Err("expired"));
        let later = Instant::now() + Duration::from_secs(30);
        let finished = until_deadline(later, || "expired", async { Ok::<_, &str>(7) }).await;
        assert_eq!(finished, Ok(7));
    }

    #[test]
    fn pull_body_error_is_redacted() {
        assert_eq!(format!("{:?}", PullBodyError::Transport), "Transport");
        assert_eq!(format!("{:?}", PullBodyError::TooLarge), "TooLarge");
    }
}
