//! The relay server core over local custody: the provider-blind
//! `plakous_http` server with each kind's custodian and pin table.
//!
//! [`LocalCustody`] checks one relay request's exact upstream pin, its
//! custodied header names against the pin row and the credential's fence,
//! then lowers the custodian's fields; it answers the facts, refresh and
//! operator-refresh calls. [`InProcessGateway`] serves it to the process's
//! own host as the local administrator; a product gateway wraps
//! [`LocalCustody`] in its own [`GatewayService`] with callers, flags and
//! audit.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use plakous_auth::CredentialGeneration;
use plakous_auth::custody::gateway::{gateway_refusal, wire_facts};
use plakous_auth::custody::{
    CredentialCustodian, CustodySlot, ExpectedFence, IdentityDigest, RefreshRequest, UpstreamPin,
    pinned,
};
use plakous_gateway_protocol::admin::{AdminRequest, AdminResponse};
use plakous_gateway_protocol::envelope::{self, RelayEnvelope};
use plakous_gateway_protocol::facts::{self, FactsReply, FactsRequest, RefreshMode, RefreshReply};
use plakous_gateway_protocol::refusal::GatewayRefusal;
use plakous_gateway_protocol::{Endpoint, GatewayAccount, PREVIOUS_VERSION, UuidText, call};
pub use plakous_http::CancellationToken;
use plakous_http::SensitiveBytes;
pub use plakous_http::relay_server::{
    DEFAULT_MAX_REQUEST_BODY_BYTES, GatewayService, Injection, Peer, RelayOutcome, RelayRecord,
    RelayServer, ServeError, ServiceFuture, new_instance,
};

use super::{GatewayEndpoint, InMemoryGateway};

struct Kind {
    name: String,
    custodian: Arc<dyn CredentialCustodian>,
    v1: Option<Arc<dyn CredentialCustodian>>,
    pins: &'static [UpstreamPin],
}

impl Kind {
    fn custodian(&self, version: u16) -> &dyn CredentialCustodian {
        if version == PREVIOUS_VERSION
            && let Some(v1) = &self.v1
        {
            return v1.as_ref();
        }
        self.custodian.as_ref()
    }
}

/// Each custodied kind's local custodian and exact upstream pin table.
pub struct LocalCustody {
    kinds: Vec<Kind>,
    instance: UuidText,
}

impl std::fmt::Debug for LocalCustody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalCustody")
            .field(
                "kinds",
                &self
                    .kinds
                    .iter()
                    .map(|kind| kind.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl LocalCustody {
    /// No kinds yet, for gateway process `instance` ([`new_instance`]).
    #[must_use]
    pub const fn new(instance: UuidText) -> Self {
        Self {
            kinds: Vec::new(),
            instance,
        }
    }

    /// Custodies `kind` (an account reference's kind segment) with
    /// `custodian`, relaying only to `pins`.
    #[must_use]
    pub fn with_kind(
        self,
        kind: impl Into<String>,
        custodian: Arc<dyn CredentialCustodian>,
        pins: &'static [UpstreamPin],
    ) -> Self {
        self.with_kind_versions(kind, custodian, None, pins)
    }

    /// Like [`Self::with_kind`], with a v1 view when the kind's authority changed.
    #[must_use]
    pub fn with_kind_versions(
        mut self,
        kind: impl Into<String>,
        custodian: Arc<dyn CredentialCustodian>,
        v1: Option<Arc<dyn CredentialCustodian>>,
        pins: &'static [UpstreamPin],
    ) -> Self {
        self.kinds.push(Kind {
            name: kind.into(),
            custodian,
            v1,
            pins,
        });
        self
    }

    /// The gateway process instance its facts name.
    #[must_use]
    pub const fn instance(&self) -> &UuidText {
        &self.instance
    }

    fn kind(&self, account: &GatewayAccount) -> Result<(&Kind, CustodySlot), GatewayRefusal> {
        let kind = self
            .kinds
            .iter()
            .find(|kind| kind.name == account.kind())
            .ok_or(GatewayRefusal::AccountUnknown {})?;
        let slot =
            CustodySlot::new(account.as_str()).map_err(|_| GatewayRefusal::AccountUnknown {})?;
        Ok((kind, slot))
    }

    fn refusal(&self, error: &plakous_auth::custody::CustodyError) -> GatewayRefusal {
        gateway_refusal(error, &self.instance)
    }

    /// Checks one relay request and supplies the exact values of its
    /// custodied entries, in order: the upstream must match a pin row
    /// exactly, the custodied names must equal that row's credential
    /// headers, and the credential must still hold the request's fence.
    ///
    /// # Errors
    /// The refusal the first failed check names.
    pub async fn inject(
        &self,
        version: u16,
        envelope: &RelayEnvelope,
    ) -> Result<Injection, GatewayRefusal> {
        let (kind, slot) = self.kind(&envelope.account)?;
        let (origin, path_and_query) = envelope::split_upstream_url(&envelope.url)
            .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        let pin = pinned(kind.pins, &envelope.method, origin, path_and_query)
            .ok_or(GatewayRefusal::UpstreamNotPinned {})?;
        let names: Vec<&str> = envelope.custodied_names().collect();
        if names != pin.credential_headers {
            return Err(GatewayRefusal::CustodiedHeadersMismatch {});
        }
        let expected = ExpectedFence {
            account_binding: IdentityDigest::from_hex(envelope.expected.account_binding.as_str())
                .ok_or(GatewayRefusal::EnvelopeInvalid {})?,
            generation: CredentialGeneration::new(envelope.expected.generation)
                .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?,
        };
        let mut fields = kind
            .custodian(version)
            .fields(&slot, &expected)
            .await
            .map_err(|error| self.refusal(&error))?;
        let values = names
            .iter()
            .map(|name| {
                fields
                    .take(name)
                    .map(|field| SensitiveBytes::new(field.into_header_bytes()))
                    .ok_or(GatewayRefusal::CustodiedHeadersMismatch {})
            })
            .collect::<Result<Vec<_>, _>>()?;
        let injection = Injection::new(values);
        let injection = if pin.pool_inference {
            injection.reuse_upstream()
        } else {
            injection
        };
        Ok(match fields.take_hold() {
            Some(hold) => injection.holding(hold.into_inner()),
            None => injection,
        })
    }

    /// Current facts for each requested account, in order.
    ///
    /// # Errors
    /// The first account's custody refusal.
    pub async fn facts(
        &self,
        version: u16,
        request: &FactsRequest,
    ) -> Result<FactsReply, GatewayRefusal> {
        if request.accounts.len() > call::MAX_FACTS_ACCOUNTS {
            return Err(GatewayRefusal::LimitExceeded {});
        }
        let mut facts = Vec::with_capacity(request.accounts.len());
        for account in &request.accounts {
            let (kind, slot) = self.kind(account)?;
            let current = kind
                .custodian(version)
                .facts(&slot)
                .await
                .map_err(|error| self.refusal(&error))?;
            facts.push(
                wire_facts(&current, &self.instance, now_unix_ms())
                    .map_err(|error| self.refusal(&error))?,
            );
        }
        Ok(FactsReply { facts })
    }

    /// One single-writer refresh under the request's condition.
    ///
    /// # Errors
    /// The custody refusal.
    pub async fn refresh(
        &self,
        version: u16,
        request: &facts::RefreshRequest,
    ) -> Result<RefreshReply, GatewayRefusal> {
        let fence = |fence: &envelope::ExpectedFence| {
            Ok::<_, GatewayRefusal>(ExpectedFence {
                account_binding: IdentityDigest::from_hex(fence.account_binding.as_str())
                    .ok_or(GatewayRefusal::EnvelopeInvalid {})?,
                generation: CredentialGeneration::new(fence.generation)
                    .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?,
            })
        };
        let refresh = match &request.mode {
            RefreshMode::IfDue { expected } => RefreshRequest::IfDue {
                expected: fence(expected)?,
            },
            RefreshMode::AfterRejection {
                rejected,
                selection_epoch_digest,
            } => RefreshRequest::AfterRejection {
                rejected: fence(rejected)?,
                selection_epoch_digest: selection_epoch_digest
                    .as_ref()
                    .map(|digest| {
                        IdentityDigest::from_hex(digest.as_str())
                            .ok_or(GatewayRefusal::EnvelopeInvalid {})
                    })
                    .transpose()?,
            },
        };
        let facts = self
            .refresh_with(version, &request.account, refresh)
            .await?;
        Ok(RefreshReply { facts })
    }

    /// The operator's forced refresh of `account`.
    ///
    /// # Errors
    /// The custody refusal.
    pub async fn operator_refresh(
        &self,
        version: u16,
        account: &GatewayAccount,
    ) -> Result<facts::AccountFacts, GatewayRefusal> {
        self.refresh_with(version, account, RefreshRequest::Operator)
            .await
    }

    async fn refresh_with(
        &self,
        version: u16,
        account: &GatewayAccount,
        request: RefreshRequest,
    ) -> Result<facts::AccountFacts, GatewayRefusal> {
        let (kind, slot) = self.kind(account)?;
        let current = kind
            .custodian(version)
            .refresh(&slot, request)
            .await
            .map_err(|error| self.refusal(&error))?;
        wire_facts(&current, &self.instance, now_unix_ms()).map_err(|error| self.refusal(&error))
    }

    /// Answers one JSON call: facts, refresh, or the admin operator refresh.
    /// Other admin calls are the product gateway's and are refused
    /// `scope_denied` here.
    ///
    /// # Errors
    /// A malformed body is `envelope_invalid`; otherwise the call's refusal.
    pub async fn call(
        &self,
        version: u16,
        endpoint: Endpoint,
        body: &[u8],
    ) -> Result<Vec<u8>, GatewayRefusal> {
        let invalid = |_| GatewayRefusal::EnvelopeInvalid {};
        let encoded = match endpoint {
            Endpoint::Facts => call::encode(
                &self
                    .facts(version, &call::decode(body).map_err(invalid)?)
                    .await?,
            ),
            Endpoint::Refresh => call::encode(
                &self
                    .refresh(version, &call::decode(body).map_err(invalid)?)
                    .await?,
            ),
            Endpoint::Admin => match call::decode::<AdminRequest>(body).map_err(invalid)? {
                AdminRequest::Refresh { account } => call::encode(&AdminResponse::Refreshed {
                    facts: Box::new(self.operator_refresh(version, &account).await?),
                }),
                _ => return Err(GatewayRefusal::ScopeDenied {}),
            },
            Endpoint::Relay => return Err(GatewayRefusal::EnvelopeInvalid {}),
        };
        encoded.map_err(|_| GatewayRefusal::Internal {})
    }
}

/// The HTTP/1.1 client a product gateway uses for its upstream hop.
///
/// The client has streaming-inference limits. The relay server owns its
/// upstream connection pool and only uses it for eligible requests. Each relay
/// narrows them to the caller's own, so a bound here only ever shortens a
/// caller's: the default 30-second head and idle bounds would cut every
/// model stream that a caller allows to stay silent longer. A product
/// gateway passes it to [`RelayServer::new`]; plakd does not name the HTTP
/// crate.
///
/// # Errors
///
/// Returns the transport configuration error.
pub fn http1_upstream() -> Result<plakous_http::Client, plakous_http::ConfigError> {
    plakous_http::Client::new_http1(plakous_http::Limits::for_streaming_inference())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The in-process gateway: [`LocalCustody`] served to this process's own
/// host, and to a same-user Unix socket peer, as the local administrator.
/// Every other peer is refused.
#[derive(Debug)]
pub struct InProcessGateway {
    custody: LocalCustody,
}

impl InProcessGateway {
    /// Serves `custody`.
    #[must_use]
    pub const fn new(custody: LocalCustody) -> Self {
        Self { custody }
    }

    /// The in-memory endpoint of a gateway serving `custody`, whose upstream
    /// exchanges use `upstream` (its fault plan applies to that hop).
    #[must_use]
    pub fn endpoint(custody: LocalCustody, upstream: plakous_http::Client) -> GatewayEndpoint {
        Self::endpoint_for(&Self::server(custody, upstream))
    }

    /// The relay server core over `custody`.
    #[must_use]
    pub fn server(custody: LocalCustody, upstream: plakous_http::Client) -> RelayServer<Self> {
        let instance = custody.instance().clone();
        RelayServer::new(Arc::new(Self::new(custody)), upstream, instance)
    }

    /// An in-memory endpoint serving each connection with `server`.
    #[must_use]
    pub fn endpoint_for(server: &RelayServer<Self>) -> GatewayEndpoint {
        let server = server.clone();
        GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.serve(io, Peer::InProcess).await;
            });
        }))
    }
}

impl GatewayService for InProcessGateway {
    type Principal = ();

    fn authenticate(&self, peer: &Peer, _key: Option<&[u8]>) -> Result<(), GatewayRefusal> {
        match peer {
            Peer::InProcess | Peer::Unix { same_user: true } => Ok(()),
            Peer::Unix { same_user: false } | Peer::Tcp(_) => {
                Err(GatewayRefusal::Unauthenticated {})
            }
        }
    }

    fn inject<'a>(
        &'a self,
        (): &'a (),
        version: u16,
        envelope: &'a RelayEnvelope,
    ) -> ServiceFuture<'a, Injection> {
        Box::pin(self.custody.inject(version, envelope))
    }

    fn call<'a>(
        &'a self,
        (): &'a (),
        version: u16,
        endpoint: Endpoint,
        body: &'a [u8],
    ) -> ServiceFuture<'a, Vec<u8>> {
        Box::pin(self.custody.call(version, endpoint, body))
    }
}

#[cfg(all(test, any(feature = "openai", feature = "codex")))]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
