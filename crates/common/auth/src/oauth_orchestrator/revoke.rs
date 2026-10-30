//! Shared claimed-revoke ladder.

use std::time::Instant;

use plakous_http::Client as HttpClient;

use super::hooks::ClaimedRevokeHooks;
use super::{BuildRequestFailure, CredentialRefreshObserver};
use crate::{
    ClaimedHttpError, ClaimedHttpResponse, PrivateCredentialStore, PrivateStoreError, RefreshClaim,
};
use std::future::Future;

/// Run the shared claimed-revoke ladder for one dialect.
///
/// Ladder: profile check → fence → begin-refresh (claim) → build revoke request →
/// execute-claimed-http → accept response → revoke tombstone (or quarantine).
///
/// # Errors
///
/// Propagates dialect-mapped failures. Post-start failures leave the durable
/// generation quarantined Unknown unless revoke succeeds.
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
    super::operation::run(store, client, claim, &mut Revoke(hooks), started).await
}

struct Revoke<'a, H>(&'a mut H);

impl<H: ClaimedRevokeHooks> super::operation::Operation for Revoke<'_, H> {
    type Error = H::Error;
    type Prepared = ();
    type Validated = ();
    type Receipt = crate::CredentialGeneration;
    type Output = H::Output;

    super::operation::delegate_operation!();

    fn build(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<((), plakous_http::Request), BuildRequestFailure<Self::Error>> {
        self.0
            .build_revoke_request(claim)
            .map(|request| ((), request))
    }
    fn validate(
        &mut self,
        (): (),
        response: &mut ClaimedHttpResponse,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.0.accept_revoke_response(response)
    }
    fn commit(
        store: &PrivateCredentialStore,
        response: ClaimedHttpResponse,
        (): (),
    ) -> Result<Self::Receipt, PrivateStoreError> {
        store.revoke_claimed_http(response)
    }
    fn output(&mut self, generation: Self::Receipt) -> Result<Self::Output, Self::Error> {
        self.0.output_from_revoked(generation)
    }
    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.0.observer()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
