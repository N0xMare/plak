//! Shared claimed-refresh ladder.

use std::time::{Duration, Instant};

use plakous_http::Client as HttpClient;

use super::hooks::ClaimedRefreshHooks;
use super::{
    BuildRequestFailure, CredentialRefreshFailureClass, CredentialRefreshPhase,
    RESTORE_POLL_INITIAL, RESTORE_POLL_MAX, RefreshAttempt, emit_claimed_http_failure,
    emit_refresh, emit_refresh_detailed, handle_build_failure, http_status_class,
    quarantine_or_map, restore_or_map,
};
use crate::{PrivateCredentialStore, PrivateStoreError, RefreshClaim};

/// Run the shared claimed-refresh ladder for one dialect.
///
/// Ladder: profile check → needs-refresh → begin-refresh → build request →
/// authorize-dispatch → execute-claimed-http → validate → complete (or
/// quarantine on any post-start failure).
///
/// When the dialect reports no refresh is needed (`fence_needing_refresh` →
/// `None`), returns [`ClaimedRefreshHooks::fresh_output`].
///
/// # Errors
///
/// Propagates dialect-mapped failures. Post-start failures leave the durable
/// generation quarantined Unknown unless complete succeeds.
pub async fn run_claimed_refresh<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    hooks: &mut H,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRefreshHooks,
{
    hooks.check_profile().await?;
    let Some(mut fence) = hooks.fence_needing_refresh().await? else {
        return hooks.fresh_output();
    };
    let started = Instant::now();
    let deadline = started + CLAIM_WAIT;
    let mut backoff = RESTORE_POLL_INITIAL;
    let claim = loop {
        let begun = if hooks.reclaim_unknown_refresh() {
            store.begin_refresh_reclaiming_unknown(&fence)
        } else {
            store.begin_refresh(&fence)
        };
        match begun {
            Ok(claim) => break claim,
            // A peer writer or in-flight claim owns the store and nothing was
            // written. Back off instead of failing: when the peer finishes,
            // the durable fence changes and the dialect re-decides whether a
            // refresh is still needed (a peer's completed refresh is fresh).
            Err(PrivateStoreError::StoreBusy) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(hooks.map_store_error(PrivateStoreError::StoreBusy));
                }
                tokio::time::sleep(backoff.min(deadline.saturating_duration_since(now))).await;
                backoff = backoff.saturating_mul(2).min(RESTORE_POLL_MAX);
                let current = store.load().map_err(|error| hooks.map_store_error(error))?;
                if current.as_ref() != Some(&fence) {
                    match hooks.fence_needing_refresh().await? {
                        Some(next) => fence = next,
                        None => return hooks.fresh_output(),
                    }
                }
            }
            Err(error) => return Err(hooks.map_store_error(error)),
        }
    };
    run_claimed_refresh_from_claim_started(store, client, claim, hooks, started).await
}

/// Longest a claim waits behind a peer's writer or in-flight refresh before
/// reporting the local contention fact.
const CLAIM_WAIT: Duration = Duration::from_secs(30);

/// Continue the claimed-refresh ladder from an already-taken unstarted claim.
///
/// Used when a dialect (or test) must hold the claim before other store
/// activity, then finish the same linear exchange/complete path.
///
/// # Errors
///
/// Same durable failure rules as [`run_claimed_refresh`].
pub async fn run_claimed_refresh_from_claim<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    claim: RefreshClaim,
    hooks: &mut H,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRefreshHooks,
{
    run_claimed_refresh_from_claim_started(store, client, claim, hooks, Instant::now()).await
}

#[allow(
    clippy::too_many_lines,
    reason = "linear claimed-refresh exits stay in one function so telemetry cannot miss a branch"
)]
async fn run_claimed_refresh_from_claim_started<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    claim: RefreshClaim,
    hooks: &mut H,
    started: Instant,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRefreshHooks,
{
    let attempt = RefreshAttempt::new(claim.fence().generation().get(), started);
    emit_refresh(
        hooks.refresh_observer(),
        &attempt,
        CredentialRefreshPhase::Began,
        None,
        None,
        None,
    );

    let (prepared, request) = match hooks.build_refresh_request(&claim) {
        Ok(value) => value,
        Err(failure) => {
            let detail = match &failure {
                BuildRequestFailure::LeaveQuarantined(error)
                | BuildRequestFailure::Restore(error) => hooks.failure_detail(error),
            };
            let (error, phase, failure_class) =
                handle_build_failure(store, claim, failure, attempt.deadline, |error| {
                    hooks.map_store_error(error)
                })
                .await;
            emit_refresh_detailed(
                hooks.refresh_observer(),
                &attempt,
                phase,
                failure_class,
                None,
                None,
                detail,
            );
            return Err(error);
        }
    };

    if let Err(error) = hooks.authorize_dispatch().await {
        let (error, restored) = restore_or_map(store, claim, error, attempt.deadline, |e| {
            hooks.map_store_error(e)
        })
        .await;
        emit_refresh(
            hooks.refresh_observer(),
            &attempt,
            if restored {
                CredentialRefreshPhase::Restored
            } else {
                CredentialRefreshPhase::OutcomeUnknown
            },
            Some(if restored {
                CredentialRefreshFailureClass::NotReady
            } else {
                CredentialRefreshFailureClass::Store
            }),
            None,
            None,
        );
        return Err(error);
    }

    let control = hooks.exchange_control();
    let exchange_deadline = control.deadline();
    let mut response = match hooks
        .execute_claimed(store, client, claim, request, control)
        .await
    {
        Ok(response) => response,
        Err(mut error) => {
            emit_claimed_http_failure(
                store,
                hooks.refresh_observer(),
                &attempt,
                &mut error,
                exchange_deadline,
            )
            .await;
            let mapped = hooks.map_claimed_http_error(store, error).await;
            hooks.after_post_start_failure(store, &mapped);
            return Err(mapped);
        }
    };

    let http_status = Some(response.status().as_u16());
    let record = match hooks.validate_tokens(prepared, &mut response).await {
        Ok(record) => record,
        Err(error) => {
            let detail = hooks.failure_detail(&error);
            let transport_failure = response.transport_failure();
            let failure_class =
                if let Some(class) = super::transport_failure_class(transport_failure) {
                    class
                } else if response.status().is_success() {
                    CredentialRefreshFailureClass::Validation
                } else {
                    http_status_class(response.status())
                };
            let (error, quarantined) =
                quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
            emit_refresh_detailed(
                hooks.refresh_observer(),
                &attempt,
                if quarantined {
                    CredentialRefreshPhase::Quarantined
                } else {
                    CredentialRefreshPhase::OutcomeUnknown
                },
                Some(if quarantined {
                    failure_class
                } else {
                    CredentialRefreshFailureClass::Store
                }),
                http_status,
                transport_failure.map(plakous_http::Failure::phase),
                detail,
            );
            hooks.after_post_start_failure(store, &error);
            return Err(error);
        }
    };
    if let Err(error) = hooks.recheck_profile_before_complete().await {
        let (error, quarantined) =
            quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
        emit_refresh(
            hooks.refresh_observer(),
            &attempt,
            if quarantined {
                CredentialRefreshPhase::Quarantined
            } else {
                CredentialRefreshPhase::OutcomeUnknown
            },
            Some(if quarantined {
                CredentialRefreshFailureClass::Validation
            } else {
                CredentialRefreshFailureClass::Store
            }),
            http_status,
            None,
        );
        hooks.after_post_start_failure(store, &error);
        return Err(error);
    }
    match store.complete_claimed_http(response, record) {
        Ok(fence) => {
            emit_refresh(
                hooks.refresh_observer(),
                &attempt,
                CredentialRefreshPhase::Completed,
                None,
                http_status,
                None,
            );
            hooks.output_from_completed(fence)
        }
        Err(store_error) => {
            emit_refresh(
                hooks.refresh_observer(),
                &attempt,
                CredentialRefreshPhase::OutcomeUnknown,
                Some(CredentialRefreshFailureClass::Store),
                http_status,
                None,
            );
            let mapped = hooks.map_store_error(store_error);
            hooks.after_post_start_failure(store, &mapped);
            Err(mapped)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
