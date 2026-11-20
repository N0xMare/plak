//! Shared claimed-refresh ladder.

use std::time::Instant;

use plakous_http::Client as HttpClient;

use super::hooks::ClaimedRefreshHooks;
use super::{
    BuildRequestFailure, ClaimBackoff, CredentialRefreshDetail, CredentialRefreshObserver,
};
use std::future::Future;

use crate::{
    ClaimedHttpError, ClaimedHttpResponse, PrivateCredentialStore, PrivateStoreError, RefreshClaim,
};

/// Run the shared claimed-refresh ladder for one dialect.
///
/// Ladder: profile check → needs-refresh → begin-refresh → build request →
/// authorize-dispatch → execute-claimed-http → validate → complete (or
/// quarantine on any post-start failure).
///
/// When the dialect reports no refresh is needed (`fence_needing_refresh` →
/// `None`), returns [`ClaimedRefreshHooks::fresh_output`].
///
/// Requires a Tokio runtime with its blocking pool and time driver enabled.
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
    let mut backoff = ClaimBackoff::new();
    let claim = loop {
        match store
            .try_begin_claim(&fence, hooks.reclaim_unknown_refresh())
            .await
        {
            Ok(claim) => break claim,
            // Only the comparison under both claim locks can settle that a
            // peer changed the record. A separate read races the next claim.
            Err(PrivateStoreError::FenceMismatch) => match hooks.fence_needing_refresh().await? {
                Some(next) => fence = next,
                None => return hooks.fresh_output(),
            },
            Err(PrivateStoreError::StoreBusy) => {}
            Err(error) => return Err(hooks.map_store_error(error)),
        }
        if !backoff.retry().await {
            return Err(hooks.map_store_error(PrivateStoreError::StoreBusy));
        }
    };
    super::operation::run(store, client, claim, &mut Refresh(hooks), started).await
}

/// Continue the claimed-refresh ladder from an already-taken unstarted claim.
///
/// Used when a dialect (or test) must hold the claim before other store
/// activity, then finish the same linear exchange/complete path.
///
/// Requires a Tokio runtime with its blocking pool and time driver enabled.
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
    super::operation::run(store, client, claim, &mut Refresh(hooks), Instant::now()).await
}

struct Refresh<'a, H>(&'a mut H);

impl<H: ClaimedRefreshHooks> super::operation::Operation for Refresh<'_, H> {
    type Error = H::Error;
    type Prepared = H::Prepared;
    type Validated = crate::CredentialRecord;
    type Receipt = crate::CredentialFence;
    type Output = H::Output;

    super::operation::delegate_operation!();

    fn build(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, plakous_http::Request), BuildRequestFailure<Self::Error>> {
        self.0.build_refresh_request(claim)
    }
    fn validate(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> impl Future<Output = Result<Self::Validated, Self::Error>> + Send {
        self.0.validate_tokens(prepared, response)
    }
    async fn commit(
        store: &PrivateCredentialStore,
        response: ClaimedHttpResponse,
        record: Self::Validated,
    ) -> Result<Self::Receipt, PrivateStoreError> {
        store
            .on_disk(move |store| store.complete_claimed_http(response, record))
            .await
    }
    fn output(&mut self, fence: Self::Receipt) -> Result<Self::Output, Self::Error> {
        self.0.output_from_completed(fence)
    }
    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.0.refresh_observer()
    }
    fn failure_detail(&mut self, error: &Self::Error) -> CredentialRefreshDetail {
        self.0.failure_detail(error)
    }
    fn after_post_start_failure(
        &mut self,
        store: &PrivateCredentialStore,
        error: &Self::Error,
    ) -> impl Future<Output = ()> + Send {
        self.0.after_post_start_failure(store, error)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
