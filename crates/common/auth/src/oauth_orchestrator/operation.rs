//! The claimed HTTP ladder shared by refresh and revoke.

use std::future::Future;
use std::time::Instant;

use super::{
    BuildRequestFailure, CredentialRefreshDetail, CredentialRefreshFailureClass,
    CredentialRefreshObserver, CredentialRefreshPhase, RefreshAttempt, emit_claimed_http_failure,
    emit_refresh, emit_refresh_detailed, handle_build_failure, http_status_class,
    quarantine_or_map, restore_or_map, transport_failure_class,
};
use crate::{
    ClaimedHttpError, ClaimedHttpResponse, PrivateCredentialStore, PrivateStoreError, RefreshClaim,
};
use plakous_http::{Client, ExchangeControl, Request};

pub(super) trait Operation {
    type Error;
    type Prepared: Send;
    type Validated;
    type Receipt;
    type Output;

    fn build(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>>;
    fn authorize_dispatch(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn exchange_control(&mut self) -> ExchangeControl;
    fn execute_claimed(
        &mut self,
        store: &PrivateCredentialStore,
        client: &Client,
        claim: RefreshClaim,
        request: Request,
        control: ExchangeControl,
    ) -> impl Future<Output = Result<ClaimedHttpResponse, ClaimedHttpError>> + Send;
    fn validate(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> impl Future<Output = Result<Self::Validated, Self::Error>> + Send;
    fn recheck_profile_before_complete(
        &mut self,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn commit(
        store: &PrivateCredentialStore,
        response: ClaimedHttpResponse,
        validated: Self::Validated,
    ) -> Result<Self::Receipt, PrivateStoreError>;
    fn output(&mut self, receipt: Self::Receipt) -> Result<Self::Output, Self::Error>;
    fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> impl Future<Output = Self::Error> + Send;
    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error;
    fn observer(&self) -> Option<&dyn CredentialRefreshObserver>;
    fn failure_detail(&mut self, _error: &Self::Error) -> CredentialRefreshDetail {
        CredentialRefreshDetail::default()
    }
    fn after_post_start_failure(&mut self, _store: &PrivateCredentialStore, _error: &Self::Error) {}
}

// These hooks have identical contracts on both public dialect traits. Keep the
// operation-specific build, validation and durable terminal outside this list.
macro_rules! delegate_operation {
    () => {
        fn authorize_dispatch(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.authorize_dispatch()
        }
        fn exchange_control(&mut self) -> plakous_http::ExchangeControl {
            self.0.exchange_control()
        }
        fn execute_claimed(
            &mut self,
            store: &PrivateCredentialStore,
            client: &plakous_http::Client,
            claim: RefreshClaim,
            request: plakous_http::Request,
            control: plakous_http::ExchangeControl,
        ) -> impl Future<Output = Result<ClaimedHttpResponse, ClaimedHttpError>> + Send {
            self.0
                .execute_claimed(store, client, claim, request, control)
        }
        fn recheck_profile_before_complete(
            &mut self,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.recheck_profile_before_complete()
        }
        fn map_claimed_http_error(
            &mut self,
            store: &PrivateCredentialStore,
            error: ClaimedHttpError,
        ) -> impl Future<Output = Self::Error> + Send {
            self.0.map_claimed_http_error(store, error)
        }
        fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
            self.0.map_store_error(error)
        }
    };
}
pub(super) use delegate_operation;

#[allow(
    clippy::too_many_lines,
    reason = "linear claimed-operation exits stay in one function so telemetry cannot miss a branch"
)]
pub(super) async fn run<H>(
    store: &PrivateCredentialStore,
    client: &Client,
    claim: RefreshClaim,
    hooks: &mut H,
    started: Instant,
) -> Result<H::Output, H::Error>
where
    H: Operation,
{
    let attempt = RefreshAttempt::new(claim.fence().generation().get(), started);
    emit_refresh(
        hooks.observer(),
        &attempt,
        CredentialRefreshPhase::Began,
        None,
        None,
        None,
    );

    let (prepared, request) = match hooks.build(&claim) {
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
                hooks.observer(),
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
            let mapped = hooks.map_claimed_http_error(store, error).await;
            hooks.after_post_start_failure(store, &mapped);
            return Err(mapped);
        }
    };

    let http_status = Some(response.status().as_u16());
    let record = match hooks.validate(prepared, &mut response).await {
        Ok(record) => record,
        Err(error) => {
            let detail = hooks.failure_detail(&error);
            let transport_failure = response.transport_failure();
            let failure_class = if let Some(class) = transport_failure_class(transport_failure) {
                class
            } else if response.status().is_success() {
                CredentialRefreshFailureClass::Validation
            } else {
                http_status_class(response.status())
            };
            let (error, quarantined) =
                quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
            emit_refresh_detailed(
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
        hooks.after_post_start_failure(store, &error);
        return Err(error);
    }
    match H::commit(store, response, record) {
        Ok(fence) => {
            emit_refresh(
                hooks.observer(),
                &attempt,
                CredentialRefreshPhase::Completed,
                None,
                http_status,
                None,
            );
            hooks.output(fence)
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
            let mapped = hooks.map_store_error(store_error);
            hooks.after_post_start_failure(store, &mapped);
            Err(mapped)
        }
    }
}
