//! Shared claimed-revoke ladder.

use std::time::Instant;

use plakous_http::Client as HttpClient;

use super::hooks::ClaimedRevokeHooks;
use super::{
    CredentialRefreshFailureClass, CredentialRefreshPhase, RefreshAttempt,
    emit_claimed_http_failure, emit_refresh, handle_build_failure, http_status_class,
    quarantine_or_map, restore_or_map,
};
use crate::PrivateCredentialStore;

/// Run the shared claimed-revoke ladder for one dialect.
///
/// Ladder: profile check → fence → begin-refresh (claim) → build revoke request →
/// execute-claimed-http → accept response → revoke tombstone (or quarantine).
///
/// # Errors
///
/// Propagates dialect-mapped failures. Post-start failures leave the durable
/// generation quarantined Unknown unless revoke succeeds.
#[allow(
    clippy::too_many_lines,
    reason = "linear claimed-revoke exits stay in one function so telemetry cannot miss a branch"
)]
pub async fn run_claimed_revoke<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    hooks: &mut H,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRevokeHooks,
{
    hooks.check_profile().await?;
    let fence = hooks.fence_to_revoke().await?;
    let started = Instant::now();
    let claim = store
        .begin_refresh(&fence)
        .map_err(|error| hooks.map_store_error(error))?;
    let attempt = RefreshAttempt::new(claim.fence().generation().get(), started);
    emit_refresh(
        hooks.observer(),
        &attempt,
        CredentialRefreshPhase::Began,
        None,
        None,
        None,
    );
    let request = match hooks.build_revoke_request(&claim) {
        Ok(request) => request,
        Err(failure) => {
            let (error, phase, failure_class) =
                handle_build_failure(store, claim, failure, attempt.deadline, |error| {
                    hooks.map_store_error(error)
                })
                .await;
            emit_refresh(hooks.observer(), &attempt, phase, failure_class, None, None);
            return Err(error);
        }
    };

    if let Err(error) = hooks.authorize_dispatch().await {
        let (error, restored) = restore_or_map(store, claim, error, attempt.deadline, |e| {
            hooks.map_store_error(e)
        })
        .await;
        emit_refresh(
            hooks.observer(),
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
                hooks.observer(),
                &attempt,
                &mut error,
                exchange_deadline,
            )
            .await;
            return Err(hooks.map_claimed_http_error(store, error).await);
        }
    };

    let http_status = Some(response.status().as_u16());
    if let Err(error) = hooks.accept_revoke_response(&mut response).await {
        let transport_failure = response.transport_failure();
        let failure_class = if let Some(class) = super::transport_failure_class(transport_failure) {
            class
        } else if response.status().is_success() {
            CredentialRefreshFailureClass::Validation
        } else {
            http_status_class(response.status())
        };
        let (error, quarantined) =
            quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
        emit_refresh(
            hooks.observer(),
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
        );
        return Err(error);
    }
    if let Err(error) = hooks.recheck_profile_before_complete().await {
        let (error, quarantined) =
            quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
        emit_refresh(
            hooks.observer(),
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
        return Err(error);
    }
    match store.revoke_claimed_http(response) {
        Ok(generation) => {
            emit_refresh(
                hooks.observer(),
                &attempt,
                CredentialRefreshPhase::Completed,
                None,
                http_status,
                None,
            );
            hooks.output_from_revoked(generation)
        }
        Err(store_error) => {
            emit_refresh(
                hooks.observer(),
                &attempt,
                CredentialRefreshPhase::OutcomeUnknown,
                Some(CredentialRefreshFailureClass::Store),
                http_status,
                None,
            );
            Err(hooks.map_store_error(store_error))
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
