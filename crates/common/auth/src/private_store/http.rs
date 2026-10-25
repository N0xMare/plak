//! The claimed HTTP exchange and its consuming terminal operations.

use super::{
    ClaimedHttpDisposition, ClaimedHttpError, ClaimedHttpResponse, PrivateCredentialStore,
    PrivateStoreError, RefreshClaim, platform,
};
use crate::{
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialStatus, SecretBytes,
};
use plakous_http::{
    Bytes as HttpBytes, Client as HttpClient, ExchangeControl, Failure as HttpFailure,
    Headers as HttpHeaders, Request as HttpRequest, StatusCode, TransmissionFact, Version,
};
use std::fmt;

#[cfg(feature = "http-refresh")]
impl ClaimedHttpResponse {
    /// Returns the wire status without interpreting it.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the negotiated HTTP version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns bounded, redaction-marked response headers.
    #[must_use]
    pub const fn headers(&self) -> &HttpHeaders {
        &self.headers
    }

    /// Returns the credential-free Unknown fence paired with this response.
    #[must_use]
    pub fn refresh_fence(&self) -> CredentialFence {
        self.claim.fence()
    }

    /// Returns whether the caller has pulled the response body through clean EOF.
    #[must_use]
    pub const fn body_completed_cleanly(&self) -> bool {
        self.clean_eof
    }

    /// Returns the content-free cause of the last failed body pull, for telemetry.
    #[must_use]
    pub const fn transport_failure(&self) -> Option<HttpFailure> {
        self.transport_failure
    }

    /// Pulls one bounded body chunk while retaining the claim and store lock.
    ///
    /// `None` marks clean EOF only when [`Self::body_completed_cleanly`] becomes
    /// true. After a failed or cancelled pull, later calls also return `None`
    /// but completion remains permanently disabled. A body failure leaves the
    /// durable credential Unknown and prevents
    /// [`PrivateCredentialStore::complete_claimed_http`].
    pub async fn next_chunk(&mut self) -> Option<Result<HttpBytes, HttpFailure>> {
        if self.clean_eof || self.body_failed {
            return None;
        }
        self.body_failed = true;
        match self.body.next_chunk().await {
            None => {
                self.clean_eof = true;
                self.body_failed = false;
                None
            }
            Some(Err(error)) => {
                self.transport_failure = Some(error);
                Some(Err(error))
            }
            Some(Ok(chunk)) => {
                self.body_failed = false;
                Some(Ok(chunk))
            }
        }
    }
}

#[cfg(feature = "http-refresh")]
impl fmt::Debug for ClaimedHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimedHttpResponse(<redacted response and refresh claim>)")
    }
}

#[cfg(feature = "http-refresh")]
impl ClaimedHttpError {
    /// Returns the durable disposition reached by the transaction.
    #[must_use]
    pub const fn disposition(&self) -> ClaimedHttpDisposition {
        self.disposition
    }

    /// Returns the exact redacted failure from `Client::execute`, if it ran.
    #[must_use]
    pub const fn transport_failure(&self) -> Option<HttpFailure> {
        self.transport
    }

    /// Returns a fail-closed store transition error, when one occurred.
    #[must_use]
    pub const fn store_error(&self) -> Option<PrivateStoreError> {
        self.store
    }

    /// Recovers the exact unstarted claim when execution was rejected before
    /// [`HttpClient::execute`] was called.
    ///
    /// This is `None` after any transport attempt. The recovered capability is
    /// suitable only for retry or the existing exact
    /// [`PrivateCredentialStore::restore_refresh_definitely_not_started`]
    /// transition; it is never accepted as proof that a transport attempt did
    /// not start.
    #[must_use]
    pub fn take_unstarted_claim(&mut self) -> Option<RefreshClaim> {
        self.unstarted_claim.take()
    }

    /// Consumes the error and recovers the unstarted claim, when present.
    #[must_use]
    pub fn into_unstarted_claim(mut self) -> Option<RefreshClaim> {
        self.take_unstarted_claim()
    }

    /// Records the durable result of an orchestrator restore attempt.
    pub(crate) fn set_disposition(&mut self, disposition: ClaimedHttpDisposition) {
        self.disposition = disposition;
    }
}

#[cfg(feature = "http-refresh")]
impl fmt::Debug for ClaimedHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimedHttpError(<redacted claimed HTTP failure>)")
    }
}

#[cfg(feature = "http-refresh")]
impl fmt::Display for ClaimedHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("claimed HTTP refresh transaction failed")
    }
}

#[cfg(feature = "http-refresh")]
impl std::error::Error for ClaimedHttpError {}

impl PrivateCredentialStore {
    /// Executes one concrete HTTP exchange under an exact durable refresh claim.
    ///
    /// The claim already owns the exclusive execution lock taken by
    /// `begin_refresh`; this method revalidates the exact scope, explicit-path
    /// authority, Unknown fence, and payload under it. The claim is
    /// irreversibly started immediately before [`HttpClient::execute`].
    /// If that exact call reports [`TransmissionFact::NotStarted`], this method
    /// alone converts the started capability back to its internal unstarted form
    /// and restores the exact old payload and status at the next generation. If
    /// it reports [`TransmissionFact::MayHaveTransmitted`], this method consumes
    /// the claim while verifying that its exact durable Unknown record remains.
    /// Dropping this future after the claim is started leaves that durable
    /// Unknown record unchanged.
    ///
    /// A successful response remains inseparably paired with the started claim
    /// and retained lock while its body is pulled. Any response-body,
    /// validation, or persistence failure leaves Unknown unless a consuming
    /// terminal method explicitly revokes it.
    ///
    /// Local failures that happen while building `request` occur before this
    /// method consumes `claim`; callers retain the separate
    /// [`Self::restore_refresh_definitely_not_started`] path for those failures.
    ///
    /// # Errors
    ///
    /// Returns a redacted error if the claim is foreign, the concrete exchange
    /// fails, or the exact restoration/quarantine verification fails closed.
    #[cfg(feature = "http-refresh")]
    #[allow(
        clippy::result_large_err,
        reason = "the error carries the claim disposition and transport cause back to the refresh owner; it is returned once from an async exchange whose future already holds it"
    )]
    pub async fn execute_claimed_http(
        &self,
        claim: RefreshClaim,
        client: &HttpClient,
        request: HttpRequest,
        control: ExchangeControl,
    ) -> Result<ClaimedHttpResponse, ClaimedHttpError> {
        if let Err(error) = self.verify_claimed_http_quarantine_under_lock(&claim.core) {
            return Err(ClaimedHttpError {
                disposition: ClaimedHttpDisposition::ClaimRejected,
                transport: None,
                store: Some(error),
                unstarted_claim: Some(claim),
            });
        }

        let started = claim.start();
        match client.execute(request, control).await {
            Ok(response) => {
                let status = response.status();
                let version = response.version();
                let headers = response.headers().clone();
                let body = response.into_body();
                Ok(ClaimedHttpResponse {
                    status,
                    version,
                    headers,
                    body,
                    claim: started,
                    clean_eof: false,
                    body_failed: false,
                    transport_failure: None,
                })
            }
            Err(failure) => match failure.transmission() {
                TransmissionFact::NotStarted | TransmissionFact::RequestIncomplete => {
                    let mut unstarted = RefreshClaim { core: started.core };
                    match self.restore_claimed_http_not_started_under_lock(&mut unstarted) {
                        Ok(_) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::RestoredNotStarted,
                            transport: Some(failure),
                            store: None,
                            unstarted_claim: None,
                        }),
                        Err(error) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::RestorationFailed,
                            transport: Some(failure),
                            store: Some(error),
                            unstarted_claim: None,
                        }),
                    }
                }
                TransmissionFact::MayHaveTransmitted => {
                    match self.verify_claimed_http_quarantine_under_lock(&started.core) {
                        Ok(_) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::QuarantinedMayHaveTransmitted,
                            transport: Some(failure),
                            store: None,
                            unstarted_claim: None,
                        }),
                        Err(error) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::QuarantineVerificationFailed,
                            transport: Some(failure),
                            store: Some(error),
                            unstarted_claim: None,
                        }),
                    }
                }
            },
        }
    }

    /// Completes a claimed HTTP refresh after its response body reached clean EOF.
    ///
    /// This consumes the inseparable response/claim/lock value and installs one
    /// exact next-generation Ready record while the execution lock remains held.
    /// A response that was not pulled through clean EOF cannot clear Unknown.
    ///
    /// # Errors
    ///
    /// Rejects an incomplete or failed body, a response from another exact
    /// store scope, an invalid replacement, changed path authority or record,
    /// and unsafe storage.
    #[cfg(feature = "http-refresh")]
    pub fn complete_claimed_http(
        &self,
        response: ClaimedHttpResponse,
        replacement: CredentialRecord,
    ) -> Result<CredentialFence, PrivateStoreError> {
        if !response.clean_eof {
            return Err(PrivateStoreError::HttpResponseBodyNotComplete);
        }
        self.require_claim_scope(&response.claim.core)?;
        self.require_scope(&replacement)?;
        if replacement.account_binding() != response.claim.core.record.account_binding() {
            return Err(PrivateStoreError::ForeignAccount);
        }
        if replacement.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        let completion_generation = response
            .claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        if replacement.generation() != completion_generation {
            return Err(PrivateStoreError::GenerationNotSequential);
        }
        let completed_fence = replacement.fence();
        let result = self.with_claim_record_lock(&response.claim.core, |store| {
            store.verify_claimed_http_quarantine_under_lock(&response.claim.core)?;
            platform::write_record(store, &replacement)
        });
        drop(response);
        drop(replacement);
        result?;
        Ok(completed_fence)
    }

    /// Consumes a claimed HTTP response while preserving and verifying Unknown.
    ///
    /// This is the terminal path for response validation, body, or persistence
    /// failures that must not clear the refresh quarantine.
    ///
    /// # Errors
    ///
    /// Rejects a response from another exact store scope, changed path
    /// authority or claimed record, and unsafe storage.
    #[cfg(feature = "http-refresh")]
    pub fn quarantine_claimed_http(
        &self,
        response: ClaimedHttpResponse,
    ) -> Result<CredentialFence, PrivateStoreError> {
        let fence = self.verify_claimed_http_quarantine_under_lock(&response.claim.core)?;
        drop(response);
        Ok(fence)
    }

    /// Consumes a claimed HTTP response and installs a revoked tombstone.
    ///
    /// Revocation is allowed regardless of response-body completion and
    /// advances exactly once from the retained Unknown claim generation.
    ///
    /// # Errors
    ///
    /// Rejects a response from another exact store scope, changed path
    /// authority or claimed record, exhausted generation space, and unsafe
    /// storage.
    #[cfg(feature = "http-refresh")]
    pub fn logout_claimed_http(
        &self,
        response: ClaimedHttpResponse,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.require_claim_scope(&response.claim.core)?;
        let next = response
            .claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        let tombstone = CredentialRecord::new(
            self.profile_id.clone(),
            response.claim.core.record.account_binding().clone(),
            next,
            CredentialStatus::Revoked,
            SecretBytes::empty(),
        )
        .map_err(|_| PrivateStoreError::InvalidRecord)?;
        let result = self.with_claim_record_lock(&response.claim.core, |store| {
            store.verify_claimed_http_quarantine_under_lock(&response.claim.core)?;
            platform::write_record(store, &tombstone)
        });
        drop(response);
        result?;
        Ok(next)
    }

    /// Alias for [`Self::logout_claimed_http`] naming local revocation.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::logout_claimed_http`].
    #[cfg(feature = "http-refresh")]
    pub fn revoke_claimed_http(
        &self,
        response: ClaimedHttpResponse,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.logout_claimed_http(response)
    }
}

#[cfg(all(
    test,
    any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
#[cfg(test)]
mod tests;
