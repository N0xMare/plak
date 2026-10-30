//! Shared claimed-refresh ladder.

use std::time::{Duration, Instant};

use plakous_http::Client as HttpClient;

use super::hooks::ClaimedRefreshHooks;
use super::{
    BuildRequestFailure, CredentialRefreshDetail, CredentialRefreshObserver, RESTORE_POLL_INITIAL,
    RESTORE_POLL_MAX,
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
    super::operation::run(store, client, claim, &mut Refresh(hooks), started).await
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
    fn commit(
        store: &PrivateCredentialStore,
        response: ClaimedHttpResponse,
        record: Self::Validated,
    ) -> Result<Self::Receipt, PrivateStoreError> {
        store.complete_claimed_http(response, record)
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
    fn after_post_start_failure(&mut self, store: &PrivateCredentialStore, error: &Self::Error) {
        self.0.after_post_start_failure(store, error);
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
