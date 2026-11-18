//! Host-side custody through a credential gateway: the relayed route
//! transport, the remote custodian, and API-key sources answering from the
//! gateway's facts.

use std::sync::{Arc, Mutex, PoisonError};

use plakous_auth::api_key::{
    ApiKey, ApiKeyError, ApiKeyFuture, ApiKeyGenerationGrant, ApiKeyRequest, ApiKeySource,
    CredentialGenerationFingerprint, CredentialProviderTag,
};
use plakous_auth::custody::gateway::GatewayCustodian;
use plakous_auth::custody::{
    CredentialCustodian, CredentialField, CustodyError, CustodySlot, CustodyStatus, ExpectedFence,
    KeyHeaderForm,
};
use plakous_http::{RouteTransport, SensitiveBytes};

use super::{GatewayEndpoint, RelayClient};

/// Custody through one credential gateway: every route exchange relays
/// through it, and its facts answer every credential question.
#[derive(Clone)]
pub struct GatewayCustody {
    relay: Arc<RelayClient>,
    custodian: Arc<GatewayCustodian>,
}

impl std::fmt::Debug for GatewayCustody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GatewayCustody(<gateway>)")
    }
}

impl GatewayCustody {
    /// Custody through the gateway at `endpoint`, presenting `caller_key`
    /// when given; `credential_headers` names each account kind's
    /// credential header names in injection order (the providers'
    /// `*_CREDENTIAL_HEADERS` tables, keyed by the product's kinds).
    ///
    /// # Errors
    /// Returns [`plakous_http::ConfigError`] when the key is not a header value.
    pub fn connect(
        endpoint: GatewayEndpoint,
        caller_key: Option<SensitiveBytes>,
        credential_headers: fn(&str) -> Option<&'static [&'static str]>,
    ) -> Result<Self, plakous_http::ConfigError> {
        let relay = Arc::new(RelayClient::new(endpoint, caller_key)?);
        let custodian = Arc::new(GatewayCustodian::new(
            Arc::clone(&relay),
            credential_headers,
        ));
        Ok(Self { relay, custodian })
    }

    /// Where route clients send their exchanges: through the gateway.
    #[must_use]
    pub fn transport(&self) -> RouteTransport {
        RouteTransport::relayed(Arc::clone(&self.relay))
    }

    /// Route custody through this gateway, for every route constructor and
    /// credentials provider that takes a [`crate::custody::Custody`].
    #[must_use]
    pub fn custody(&self) -> crate::custody::Custody {
        crate::custody::Custody::relayed(self.transport())
    }

    /// The remote custodian: facts, refresh and custodied fields.
    #[must_use]
    pub const fn custodian(&self) -> &Arc<GatewayCustodian> {
        &self.custodian
    }

    /// An API-key source for `account` (`kind[/name]`) whose key stays at
    /// the gateway. It admits only requests for `provider` at exactly
    /// `origin` and `reference`, like a local key source, and keeps the
    /// caller's non-secret `authority`.
    ///
    /// # Errors
    /// [`CustodyError::UnknownAccount`] for a malformed account reference.
    pub fn key_source<A>(
        &self,
        account: &str,
        provider: CredentialProviderTag,
        origin: impl Into<String>,
        reference: Option<String>,
        authority: A,
    ) -> Result<CustodiedKeySource<A>, CustodyError> {
        Ok(CustodiedKeySource {
            custodian: Arc::clone(&self.custodian),
            slot: CustodySlot::new(account)?,
            provider,
            origin: origin.into(),
            reference,
            authority,
            current: Mutex::new(None),
        })
    }
}

#[derive(Clone, Copy)]
struct KeyGeneration {
    fingerprint: CredentialGenerationFingerprint,
    fence: ExpectedFence,
}

/// An API-key source whose key a credential gateway holds.
///
/// Its generation is the fingerprint the gateway reports, so a route admits
/// exactly the generation a local source of the same key would; its field
/// is a custodied placeholder the gateway fills at that header's position.
/// [`ApiKeySource::prepare`] fetches the gateway's facts once per physical
/// attempt; the synchronous reads use those facts and the gateway checks
/// the fence again at injection.
pub struct CustodiedKeySource<A> {
    custodian: Arc<GatewayCustodian>,
    slot: CustodySlot,
    provider: CredentialProviderTag,
    origin: String,
    reference: Option<String>,
    authority: A,
    current: Mutex<Option<KeyGeneration>>,
}

impl<A> std::fmt::Debug for CustodiedKeySource<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CustodiedKeySource")
            .field("account", &self.slot.as_str())
            .finish_non_exhaustive()
    }
}

impl<A> CustodiedKeySource<A> {
    fn accepts(&self, request: &(impl ApiKeyRequest + ?Sized)) -> bool {
        request.provider() == self.provider
            && request.origin() == self.origin
            && request.credential_reference() == self.reference.as_deref()
    }

    fn current(&self) -> Result<KeyGeneration, ApiKeyError> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ok_or(ApiKeyError::Unavailable)
    }
}

impl<R, A> ApiKeySource<R, A> for CustodiedKeySource<A>
where
    R: ApiKeyRequest + ?Sized,
    A: Clone + Send + Sync,
{
    fn authority(&self) -> &A {
        &self.authority
    }

    fn generation_grant(
        &self,
        selection: &dyn ApiKeyRequest,
    ) -> Result<ApiKeyGenerationGrant<A>, ApiKeyError> {
        if !self.accepts(selection) {
            return Err(ApiKeyError::BindingMismatch);
        }
        Ok(ApiKeyGenerationGrant::from_fingerprint(
            self.provider,
            self.authority.clone(),
            self.current()?.fingerprint,
        ))
    }

    fn api_key(
        &self,
        _request: &R,
        _expected: &ApiKeyGenerationGrant<A>,
    ) -> Result<ApiKey, ApiKeyError> {
        // The key never reaches this process; only custodied fields do.
        Err(ApiKeyError::Unavailable)
    }

    fn prepare(&self) -> ApiKeyFuture<'_> {
        Box::pin(async move {
            let facts = self
                .custodian
                .facts(&self.slot)
                .await
                .map_err(|_| ApiKeyError::Unavailable)?;
            let generation = (facts.status == CustodyStatus::Ready)
                .then(|| {
                    Some(KeyGeneration {
                        fingerprint: CredentialGenerationFingerprint::from_hex(
                            facts.key.as_ref()?.fingerprint.as_deref()?,
                        )?,
                        fence: facts.expected_fence()?,
                    })
                })
                .flatten();
            *self.current.lock().unwrap_or_else(PoisonError::into_inner) = generation;
            generation.map(|_| ()).ok_or(ApiKeyError::Unavailable)
        })
    }

    fn credential_field(
        &self,
        request: &R,
        expected: &ApiKeyGenerationGrant<A>,
        _form: KeyHeaderForm,
    ) -> Result<CredentialField, ApiKeyError> {
        let current = self.current()?;
        if !self.accepts(request)
            || expected.provider() != self.provider
            || expected.fingerprint() != current.fingerprint
        {
            return Err(ApiKeyError::BindingMismatch);
        }
        Ok(CredentialField::custodied(&self.slot, &current.fence))
    }
}

#[cfg(all(test, feature = "openai"))]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
