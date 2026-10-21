use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Request as WireRequest, Response as WireResponse, Uri, Version};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper::rt::Executor;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::{JoinError, JoinHandle};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::observation::Observation;
use crate::request::header_bytes;
use crate::{
    AddressPolicy, Bytes as PublicBytes, CancellationToken, ConfigError, ConnectionUse, ErrorKind,
    ExchangeControl, Failure, Headers, LimitKind, Origin, Phase, Request, RequestError, StatusCode,
    TransmissionFact, WireProtocol,
};
use crate::{ExchangeObserver, ExchangeTerminal};

#[cfg(test)]
mod address_tests;
#[cfg(feature = "fault-injection")]
pub(crate) mod fault;
mod pool;

use pool::{ConnectionSettings, IdleConnection, Pool, PoolKey};

const HTTP1_MINIMUM_BUFFER_BYTES: usize = 8_192;
const TRANSPORT_BEHAVIOR_REVISION: u32 = 4;
/// How long a cleanly finished exchange waits for its HTTP/1.1 connection to
/// accept another request before giving up on keeping it.
const RELEASE_READY_BOUND: Duration = Duration::from_secs(1);

/// Strict memory and phase-time bounds for physical exchanges.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum number of caller-supplied request header values.
    pub max_request_headers: usize,
    /// Maximum encoded bytes in caller-supplied request headers.
    pub max_request_header_bytes: usize,
    /// Maximum buffered request body bytes.
    pub max_request_body_bytes: usize,
    /// Maximum number of response head or trailer values.
    pub max_response_headers: usize,
    /// Maximum encoded response head or trailer bytes.
    pub max_response_header_bytes: usize,
    /// Maximum bytes yielded by one response data frame.
    pub max_response_chunk_bytes: usize,
    /// Maximum total response body bytes.
    pub max_response_body_bytes: usize,
    /// Maximum DNS resolution time, further capped by the exchange deadline.
    pub resolution_timeout: Duration,
    /// Maximum TCP connection time, further capped by the exchange deadline.
    pub connect_timeout: Duration,
    /// Maximum TLS and HTTP connection negotiation time.
    pub tls_timeout: Duration,
    /// Maximum time without H1 stream-accepted writes or H2 body consumption.
    /// H1 uses TLS, or plain TCP for loopback HTTP origins. The final flush must
    /// finish within the idle bound measured from the last accepted write.
    /// Further capped by the exchange deadline.
    pub request_write_idle_timeout: Duration,
    /// Maximum time from H1 request flush (H2 consumption) to a complete response head.
    pub response_head_timeout: Duration,
    /// Maximum idle time for each caller-driven response-body pull.
    pub response_body_timeout: Duration,
}

/// Closed construction policy for one immutable transport client.
///
/// This value describes the protocol negotiation and trust-source policy that
/// can change physical exchange behavior. Callers may bind it together with
/// [`Limits`] without relying on an object address or implementation detail.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum TransportPolicy {
    /// Mozilla `WebPKI` roots, HTTP/2 preferred with HTTP/1.1 fallback.
    WebPkiHttp2Preferred,
    /// Mozilla `WebPKI` roots, HTTP/1.1 only.
    WebPkiHttp1Only,
    /// Caller-supplied immutable Rustls configuration, with the same ALPN
    /// policy as [`Self::WebPkiHttp2Preferred`].
    CallerTlsHttp2Preferred,
}

/// Exact non-secret identity supplied with one caller-built TLS configuration.
///
/// Callers must frame every behavior-bearing trust, client-certificate,
/// verifier, provider, and protocol-policy datum. Secret key bytes must never
/// be included; bind their independently managed non-secret generation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TlsConfigIdentity([u8; 32]);

impl TlsConfigIdentity {
    /// Hashes one versioned exact caller TLS behavior table.
    ///
    /// # Errors
    ///
    /// Rejects revision zero, which is reserved for unversioned behavior.
    pub fn from_parts(revision: u32, parts: &[&[u8]]) -> Result<Self, ConfigError> {
        if revision == 0 {
            return Err(ConfigError::UnversionedTlsIdentity);
        }
        let mut hasher = framed_identity_hasher(b"plakous-http/caller-tls-identity-v1");
        update_identity_frame(&mut hasher, &revision.to_be_bytes());
        for part in parts {
            update_identity_frame(&mut hasher, part);
        }
        Ok(Self(hasher.finalize().into()))
    }

    /// Returns the stable identity bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for TlsConfigIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TlsConfigIdentity(<non-secret sha256>)")
    }
}

/// Exact immutable transport behavior identity available to attempt hosts.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TransportBehaviorIdentity([u8; 32]);

impl TransportBehaviorIdentity {
    /// Returns the stable identity bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for TransportBehaviorIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransportBehaviorIdentity(<non-secret sha256>)")
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_headers: 64,
            max_request_header_bytes: 32 * 1_024,
            max_request_body_bytes: 8 * 1_024 * 1_024,
            max_response_headers: 128,
            max_response_header_bytes: 64 * 1_024,
            max_response_chunk_bytes: 1024 * 1_024,
            max_response_body_bytes: 32 * 1_024 * 1_024,
            resolution_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            tls_timeout: Duration::from_secs(10),
            request_write_idle_timeout: Duration::from_secs(30),
            response_head_timeout: Duration::from_secs(30),
            response_body_timeout: Duration::from_secs(30),
        }
    }
}

/// Silence a streaming model exchange may hold before its response head or
/// between response-body frames.
pub const STREAMING_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

impl Limits {
    /// Limits for a streaming model exchange: response-head and per-pull
    /// response-body waits are [`STREAMING_IDLE_TIMEOUT`] rather than the
    /// 30-second REST stall, because reasoning and compaction routinely go
    /// quiet longer than that between SSE frames. Pair it with
    /// [`ExchangeControl::streaming`] so the body idle bound restarts on every
    /// frame with no absolute cap on a stream that keeps making progress.
    #[must_use]
    pub fn for_streaming_inference() -> Self {
        Self {
            response_head_timeout: STREAMING_IDLE_TIMEOUT,
            response_body_timeout: STREAMING_IDLE_TIMEOUT,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_request_headers == 0
            || self.max_response_headers == 0
            || self.max_response_chunk_bytes == 0
            || self.resolution_timeout.is_zero()
            || self.connect_timeout.is_zero()
            || self.tls_timeout.is_zero()
            || self.request_write_idle_timeout.is_zero()
            || self.response_head_timeout.is_zero()
            || self.response_body_timeout.is_zero()
        {
            return Err(ConfigError::ZeroLimit);
        }
        if self.max_response_header_bytes < HTTP1_MINIMUM_BUFFER_BYTES {
            return Err(ConfigError::ResponseHeaderBufferTooSmall);
        }
        if u32::try_from(self.max_response_header_bytes).is_err() {
            return Err(ConfigError::Http2LimitTooLarge);
        }
        Ok(())
    }
}

/// An immutable HTTPS client: one physical exchange per [`Self::execute`], on
/// a fresh connection unless the client is [`Self::pooled`].
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
    observer: Option<Arc<dyn ExchangeObserver>>,
    #[cfg(feature = "fault-injection")]
    faults: Option<fault::FaultPlan>,
}

struct ClientInner {
    limits: Limits,
    policy: TransportPolicy,
    trust_identity: Option<TlsConfigIdentity>,
    behavior_identity: Option<TransportBehaviorIdentity>,
    tls_config: Arc<ClientConfig>,
    /// Idle connections, shared with every client derived from this one.
    pool: Option<Arc<Pool>>,
    address_policy: Option<AddressPolicy>,
    resolver: Resolver,
}

/// How a fresh connection resolves its origin host.
#[derive(Clone, Default)]
enum Resolver {
    #[default]
    System,
    /// Answers each lookup from a test script instead of DNS.
    #[cfg(test)]
    Scripted(ScriptedLookup),
}

#[cfg(test)]
type ScriptedLookup = Arc<dyn Fn(&str, u16) -> Vec<SocketAddr> + Send + Sync>;

impl Resolver {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, Failure> {
        match self {
            Self::System => tokio::net::lookup_host((host, port))
                .await
                .map(Iterator::collect)
                .map_err(|_| resolution_failure(ErrorKind::Resolution)),
            #[cfg(test)]
            Self::Scripted(script) => Ok(script(host, port)),
        }
    }
}

const fn resolution_failure(kind: ErrorKind) -> Failure {
    Failure::new(Phase::Resolution, TransmissionFact::NotStarted, kind)
}

/// Resolves `origin` and picks the address to connect: the first one, or with
/// a policy the first one the policy admits.
async fn resolve(
    resolver: Resolver,
    origin: &Origin,
    policy: Option<AddressPolicy>,
) -> Result<SocketAddr, Failure> {
    if policy.is_some_and(|policy| policy.refuses_host_name(origin.host())) {
        return Err(resolution_failure(ErrorKind::AddressRefused));
    }
    let addresses = resolver
        .lookup(origin.resolution_host(), origin.port())
        .await?;
    let first = addresses
        .first()
        .copied()
        .ok_or_else(|| resolution_failure(ErrorKind::Resolution))?;
    let Some(policy) = policy else {
        return Ok(first);
    };
    addresses
        .into_iter()
        .find(|address| policy.admits(address.ip()))
        .ok_or_else(|| resolution_failure(ErrorKind::AddressRefused))
}

impl Client {
    /// Publishes completion of a caller-owned lookup to the observer.
    pub fn observe_lookup_finished(&self) {
        if let Some(observer) = &self.observer {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.lookup_finished();
            }));
        }
    }

    /// Publishes a caller-owned lookup retry to the diagnostic observer.
    pub fn observe_lookup_retry(&self, attempt: u8, cause: &str) {
        if let Some(observer) = &self.observer {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.lookup_retry(attempt, cause);
            }));
        }
    }

    /// Creates a client using Mozilla `WebPKI` roots and Rustls.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a limit is invalid.
    pub fn new(limits: Limits) -> Result<Self, ConfigError> {
        let mut tls_config = default_tls_config()?;
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let trust = default_trust_identity();
        Self::with_tls_config_and_policy(
            limits,
            tls_config,
            TransportPolicy::WebPkiHttp2Preferred,
            Some(trust),
        )
    }

    /// Creates an HTTP/1.1-only client using Mozilla `WebPKI` roots and
    /// Rustls.
    ///
    /// This is intended for one-shot protocol endpoints where an HTTP/2
    /// failure cannot be retried safely, such as OAuth token exchange.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a limit is invalid.
    pub fn new_http1(limits: Limits) -> Result<Self, ConfigError> {
        let mut tls_config = default_tls_config()?;
        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let trust = default_trust_identity();
        Self::with_tls_config_and_policy(
            limits,
            tls_config,
            TransportPolicy::WebPkiHttp1Only,
            Some(trust),
        )
    }

    /// Creates a client with caller-supplied immutable Rustls configuration.
    ///
    /// ALPN is set to prefer HTTP/2 and permit HTTP/1.1. Supplying roots or a
    /// client certificate is explicit configuration; no process-global TLS or
    /// proxy state is consulted.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a limit is invalid.
    pub fn with_tls_config(
        limits: Limits,
        mut tls_config: ClientConfig,
    ) -> Result<Self, ConfigError> {
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Self::with_tls_config_and_policy(
            limits,
            tls_config,
            TransportPolicy::CallerTlsHttp2Preferred,
            None,
        )
    }

    /// Creates a client with caller TLS and an exact non-secret behavior identity.
    ///
    /// Attempt hosts that persist or compare executable policy require this
    /// constructor. [`Self::with_tls_config`] remains available to callers that
    /// do not use TLS configuration as semantic operation authority.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a limit is invalid.
    pub fn with_tls_config_and_identity(
        limits: Limits,
        mut tls_config: ClientConfig,
        identity: TlsConfigIdentity,
    ) -> Result<Self, ConfigError> {
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Self::with_tls_config_and_policy(
            limits,
            tls_config,
            TransportPolicy::CallerTlsHttp2Preferred,
            Some(identity),
        )
    }

    fn with_tls_config_and_policy(
        limits: Limits,
        tls_config: ClientConfig,
        policy: TransportPolicy,
        trust_identity: Option<TlsConfigIdentity>,
    ) -> Result<Self, ConfigError> {
        limits.validate()?;
        let behavior_identity = trust_identity
            .map(|trust| transport_behavior_identity(&limits, policy, trust, false, None));
        Ok(Self {
            observer: None,
            #[cfg(feature = "fault-injection")]
            faults: fault::from_env(),
            inner: Arc::new(ClientInner {
                limits,
                policy,
                trust_identity,
                behavior_identity,
                tls_config: Arc::new(tls_config),
                pool: None,
                address_policy: None,
                resolver: Resolver::System,
            }),
        })
    }

    /// Rebuilds this client with new phase limits, keeping TLS, policy, the
    /// attached observer and any connection pool, which the two then share.
    /// Inference SSE uses this so the body stall can wait the operation
    /// deadline without replacing the caller's TLS trust.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a limit is invalid.
    pub fn with_limits(&self, limits: Limits) -> Result<Self, ConfigError> {
        limits.validate()?;
        let inner = &self.inner;
        let behavior_identity = inner.trust_identity.map(|trust| {
            transport_behavior_identity(
                &limits,
                inner.policy,
                trust,
                inner.pool.is_some(),
                inner.address_policy,
            )
        });
        Ok(Self {
            observer: self.observer.clone(),
            #[cfg(feature = "fault-injection")]
            faults: self.faults.clone(),
            inner: Arc::new(ClientInner {
                limits,
                policy: inner.policy,
                trust_identity: inner.trust_identity,
                behavior_identity,
                tls_config: Arc::clone(&inner.tls_config),
                pool: inner.pool.clone(),
                address_policy: inner.address_policy,
                resolver: inner.resolver.clone(),
            }),
        })
    }

    /// Returns this client with `policy` deciding which resolved addresses a
    /// fresh connection may use, keeping TLS, limits, observer and any pool.
    ///
    /// The policy is applied on every connection, to the exact address then
    /// connected: the first resolved address it admits is used, and a refused
    /// host name or a resolution with no admitted address fails
    /// [`ErrorKind::AddressRefused`] in [`Phase::Resolution`] before any
    /// connection is attempted. Pooled connections are kept per policy. A
    /// caller following redirects sends each hop through this client, so every
    /// hop is checked. `None` restores connecting to the first resolved address.
    #[must_use]
    pub fn with_address_policy(&self, policy: Option<AddressPolicy>) -> Self {
        let inner = &self.inner;
        Self {
            observer: self.observer.clone(),
            #[cfg(feature = "fault-injection")]
            faults: self.faults.clone(),
            inner: Arc::new(ClientInner {
                limits: inner.limits.clone(),
                policy: inner.policy,
                trust_identity: inner.trust_identity,
                behavior_identity: inner.trust_identity.map(|trust| {
                    transport_behavior_identity(
                        &inner.limits,
                        inner.policy,
                        trust,
                        inner.pool.is_some(),
                        policy,
                    )
                }),
                tls_config: Arc::clone(&inner.tls_config),
                pool: inner.pool.clone(),
                address_policy: policy,
                resolver: inner.resolver.clone(),
            }),
        }
    }

    /// Returns the destination address policy, if one is installed.
    #[must_use]
    pub fn address_policy(&self) -> Option<AddressPolicy> {
        self.inner.address_policy
    }

    #[cfg(test)]
    fn with_resolver(&self, resolver: Resolver) -> Self {
        let inner = &self.inner;
        Self {
            observer: self.observer.clone(),
            #[cfg(feature = "fault-injection")]
            faults: self.faults.clone(),
            inner: Arc::new(ClientInner {
                limits: inner.limits.clone(),
                policy: inner.policy,
                trust_identity: inner.trust_identity,
                behavior_identity: inner.behavior_identity,
                tls_config: Arc::clone(&inner.tls_config),
                pool: inner.pool.clone(),
                address_policy: inner.address_policy,
                resolver,
            }),
        }
    }

    /// Returns a client with the same TLS, policy, limits, observer and fault
    /// plan that keeps cleanly finished connections for reuse.
    ///
    /// The new client owns a new, empty pool. Its clones and every client
    /// derived from it by [`Self::with_limits`] or [`Self::with_observer`]
    /// share that pool, which ends, closing its idle connections, with the
    /// last of them. A connection carries one exchange at a time, returns to
    /// the pool only after a clean, fully read response, and is closed after
    /// 5 seconds idle. Inference routes use it: their turn requests may be
    /// replayed after the rare race with a server closing an idle connection.
    /// One-shot endpoints such as OAuth, catalogs and discovery stay unpooled.
    #[must_use]
    pub fn pooled(&self) -> Self {
        let inner = &self.inner;
        Self {
            observer: self.observer.clone(),
            #[cfg(feature = "fault-injection")]
            faults: self.faults.clone(),
            inner: Arc::new(ClientInner {
                limits: inner.limits.clone(),
                policy: inner.policy,
                trust_identity: inner.trust_identity,
                behavior_identity: inner.trust_identity.map(|trust| {
                    transport_behavior_identity(
                        &inner.limits,
                        inner.policy,
                        trust,
                        true,
                        inner.address_policy,
                    )
                }),
                tls_config: Arc::clone(&inner.tls_config),
                pool: Some(Arc::new(Pool::default())),
                address_policy: inner.address_policy,
                resolver: inner.resolver.clone(),
            }),
        }
    }

    /// Whether this client keeps cleanly finished connections for reuse.
    #[must_use]
    pub fn is_pooled(&self) -> bool {
        self.inner.pool.is_some()
    }

    /// Attaches an explicit diagnostic observer without changing transport identity.
    #[must_use]
    pub fn with_observer(mut self, observer: Option<Arc<dyn ExchangeObserver>>) -> Self {
        self.observer = observer;
        self
    }

    /// Replaces the fault plan this client took from `PLAK_TEST_FAULTS`.
    /// Repository tests only; see [`FaultPlan`](crate::FaultPlan).
    #[cfg(feature = "fault-injection")]
    #[must_use]
    pub fn with_faults(mut self, faults: Option<fault::FaultPlan>) -> Self {
        self.faults = faults;
        self
    }

    /// Returns the strict limits enforced by this client.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }

    /// Returns the closed construction policy enforced by this client.
    #[must_use]
    pub fn transport_policy(&self) -> TransportPolicy {
        self.inner.policy
    }

    /// Returns exact executable transport identity when TLS policy was sealed.
    #[must_use]
    pub fn behavior_identity(&self) -> Option<TransportBehaviorIdentity> {
        self.inner.behavior_identity
    }

    /// Resolves, connects and verifies TLS/ALPN for this exact origin (a
    /// loopback HTTP origin stops after the TCP connect).
    ///
    /// Sends no HTTP bytes, uses no idle pooled connection, and drops the fresh
    /// connection after the handshake. No headers, credentials or request are built.
    ///
    /// # Errors
    ///
    /// Returns a closed transport failure on cancellation, deadline, DNS,
    /// connection, TLS verification or unsupported ALPN.
    pub async fn probe(&self, origin: &Origin, control: ExchangeControl) -> Result<(), Failure> {
        let _ = self.connect_stream(origin, &control).await?;
        Ok(())
    }

    /// Performs exactly one physical HTTPS exchange.
    ///
    /// The request goes out on one connection and is never redirected. An
    /// unpooled client opens a fresh connection for it. A [`Self::pooled`]
    /// client first takes an idle connection it kept; if that connection fails
    /// before accepting any byte of the request, the request is sent once more
    /// on a fresh connection and reports [`ConnectionUse::ReplacedStale`]. No
    /// other failure is retried. Dropping this future aborts any owned
    /// connection task. Cancellation and timeouts abort and join owned work
    /// before returning.
    ///
    /// # Errors
    ///
    /// Returns a phase- and transmission-classified [`Failure`] when request
    /// validation or any physical-exchange phase fails.
    pub async fn execute(
        &self,
        request: Request,
        control: ExchangeControl,
    ) -> Result<Response, Failure> {
        let mut observation = Observation::start(self.observer.as_ref(), request.body())?;
        // Observation failure cleanup must not duplicate the large physical
        // exchange future inside every caller's async stack.
        match Box::pin(self.execute_inner(request, control)).await {
            Ok(mut response) => {
                if !observation.head(response.status.as_u16(), response.connection) {
                    let _ = response.body.close().await;
                    let failure = Failure::new(
                        Phase::ResponseHead,
                        TransmissionFact::MayHaveTransmitted,
                        ErrorKind::Internal,
                    )
                    .with_connection_use(response.connection);
                    observation.finish(ExchangeTerminal::Failed(failure));
                    return Err(failure);
                }
                response.body.observation = observation;
                Ok(response)
            }
            Err(failure) => {
                observation.finish(ExchangeTerminal::Failed(failure));
                Err(failure)
            }
        }
    }

    async fn execute_inner(
        &self,
        request: Request,
        control: ExchangeControl,
    ) -> Result<Response, Failure> {
        request.validate(&self.inner.limits).map_err(|error| {
            Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                request_error_kind(&error),
            )
        })?;
        if control.cancellation().is_cancelled() {
            return Err(Failure::new(
                Phase::Resolution,
                TransmissionFact::NotStarted,
                ErrorKind::Cancelled,
            ));
        }
        if Instant::now() >= control.deadline() {
            return Err(Failure::new(
                Phase::Resolution,
                TransmissionFact::NotStarted,
                ErrorKind::DeadlineExceeded,
            ));
        }

        let (origin, method, path_and_query, headers, body) = request.into_parts();
        #[cfg(feature = "fault-injection")]
        let stream_fault = match self
            .faults
            .as_ref()
            .and_then(|plan| plan.select(origin.host(), path_and_query.as_str()))
        {
            None => None,
            Some(fault::Selected::Invalid) => {
                return Err(Failure::new(
                    Phase::Resolution,
                    TransmissionFact::NotStarted,
                    ErrorKind::Internal,
                ));
            }
            Some(fault::Selected::Connect) => {
                return Err(Failure::new(
                    Phase::Connect,
                    TransmissionFact::NotStarted,
                    ErrorKind::Io,
                )
                .with_io_cause(&std::io::Error::from(std::io::ErrorKind::ConnectionRefused)));
            }
            Some(fault::Selected::WriteReset) => {
                return Err(Failure::new(
                    Phase::RequestWriteOrResponseHead,
                    TransmissionFact::RequestIncomplete,
                    ErrorKind::Protocol,
                ));
            }
            Some(fault::Selected::RecordRejected) => {
                return Err(Failure::new(
                    Phase::RequestWriteOrResponseHead,
                    TransmissionFact::RequestIncomplete,
                    ErrorKind::Protocol,
                )
                .with_peer_rejected_record()
                .with_wire_protocol(WireProtocol::Http1));
            }
            Some(fault::Selected::Status {
                status,
                retry_after,
            }) => {
                return fault::injected_status(status, retry_after, control, &self.inner.limits)
                    .await;
            }
            Some(fault::Selected::Stream(stream_fault)) => Some(stream_fault),
        };
        let parts = RequestParts {
            origin,
            method,
            path_and_query,
            headers: headers.into_inner(),
            body,
        };
        #[cfg_attr(
            not(feature = "fault-injection"),
            allow(unused_mut, reason = "set only by faults")
        )]
        let mut response = self.dispatch(&parts, &control).await?;
        #[cfg(feature = "fault-injection")]
        {
            response.body.fault = stream_fault;
        }
        Ok(response)
    }

    /// Sends the request on an idle pooled connection when there is one,
    /// replacing that once with a fresh connection when it proves stale.
    async fn dispatch(
        &self,
        parts: &RequestParts,
        control: &ExchangeControl,
    ) -> Result<Response, Failure> {
        let pool = self
            .inner
            .pool
            .as_ref()
            .map(|pool| (pool, self.pool_key(&parts.origin)));
        let mut connection_use = ConnectionUse::Fresh;
        let mut idle_age = None;
        if let Some((pool, key)) = &pool
            && !control.fresh_connection()
            && let Some((idle, age)) = pool.checkout(key)
        {
            idle_age = Some(age);
            let progress = parts.progress(idle.protocol);
            idle.slot.replace(Arc::clone(&progress));
            let live = Live {
                connection: idle.connection,
                slot: idle.slot,
                progress,
                protocol: idle.protocol,
            };
            match self
                .exchange(
                    live,
                    parts,
                    control,
                    ConnectionUse::Reused,
                    Some((pool, key)),
                )
                .await
            {
                Ok(mut response) => {
                    response.body.idle_age = idle_age;
                    return Ok(response);
                }
                Err(Exchanged::Stale(_)) => connection_use = ConnectionUse::ReplacedStale,
                Err(Exchanged::Failed(failure)) => return Err(failure.with_idle_age(idle_age)),
            }
        }
        let live = self.connect(parts, control).await.map_err(|failure| {
            failure
                .with_connection_use(connection_use)
                .with_idle_age(idle_age)
        })?;
        let pool = pool.as_ref().map(|(pool, key)| (*pool, key));
        self.exchange(live, parts, control, connection_use, pool)
            .await
            .map(|mut response| {
                response.body.idle_age = idle_age;
                response
            })
            .map_err(|(Exchanged::Stale(failure) | Exchanged::Failed(failure))| {
                failure.with_idle_age(idle_age)
            })
    }

    fn pool_key(&self, origin: &Origin) -> PoolKey {
        PoolKey {
            host: origin.host().to_owned(),
            port: origin.port(),
            plain_loopback: origin.is_loopback_http(),
            address_policy: self.inner.address_policy,
            tls_config: Arc::as_ptr(&self.inner.tls_config) as usize,
            policy: self.inner.policy,
            settings: ConnectionSettings::new(&self.inner.limits),
        }
    }

    /// Resolves, connects and, for HTTPS, verifies TLS/ALPN for a fresh
    /// connection.
    async fn connect_stream(
        &self,
        origin: &Origin,
        control: &ExchangeControl,
    ) -> Result<(WireStream, WireProtocol), Failure> {
        let resolver = self.inner.resolver.clone();
        let policy = self.inner.address_policy;
        let target = origin.clone();
        let address = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                resolve(resolver, &target, policy).await
            })),
            control,
            self.inner.limits.resolution_timeout,
            Phase::Resolution,
            TransmissionFact::NotStarted,
        )
        .await?;

        let stream = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                let failed = |error: std::io::Error| {
                    Failure::new(Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io)
                        .with_io_cause(&error)
                };
                let stream = TcpStream::connect(address).await.map_err(failed)?;
                // HTTP/2 writes a request as separate HEADERS and DATA frames;
                // Nagle would hold the second for the peer's delayed ACK.
                stream.set_nodelay(true).map_err(failed)?;
                Ok(stream)
            })),
            control,
            self.inner.limits.connect_timeout,
            Phase::Connect,
            TransmissionFact::NotStarted,
        )
        .await?;
        if origin.is_loopback_http() {
            return Ok((WireStream::Plain(stream), WireProtocol::Http1));
        }

        let server_name = ServerName::try_from(origin.host().to_owned())
            .map_err(|_| Failure::new(Phase::Tls, TransmissionFact::NotStarted, ErrorKind::Tls))?;
        let tls_config = Arc::clone(&self.inner.tls_config);
        let tls_stream = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                TlsConnector::from(tls_config)
                    .connect(server_name, stream)
                    .await
                    .map_err(|error| {
                        Failure::new(Phase::Tls, TransmissionFact::NotStarted, ErrorKind::Tls)
                            .with_io_cause(&error)
                    })
            })),
            control,
            self.inner.limits.tls_timeout,
            Phase::Tls,
            TransmissionFact::NotStarted,
        )
        .await?;

        let protocol = match tls_stream.get_ref().1.alpn_protocol() {
            Some(b"h2") => WireProtocol::Http2,
            Some(b"http/1.1") | None => WireProtocol::Http1,
            Some(_) => {
                return Err(Failure::new(
                    Phase::Tls,
                    TransmissionFact::NotStarted,
                    ErrorKind::Tls,
                ));
            }
        };
        Ok((WireStream::Tls(Box::new(tls_stream)), protocol))
    }

    async fn connect(
        &self,
        parts: &RequestParts,
        control: &ExchangeControl,
    ) -> Result<Live, Failure> {
        let (stream, protocol) = self.connect_stream(&parts.origin, control).await?;
        // A fresh exchange's progress also counts the HTTP/2 preface and
        // settings the handshake writes.
        let progress = parts.progress(protocol);
        let slot = ProgressSlot::new(Arc::clone(&progress));
        let wire_slot = Arc::clone(&slot);
        let limits = self.inner.limits.clone();
        let connection = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                establish(stream, protocol, &limits, wire_slot).await
            })),
            control,
            self.inner.limits.tls_timeout,
            Phase::Tls,
            TransmissionFact::NotStarted,
        )
        .await
        .map_err(|failure| failure.with_wire_protocol(protocol))?;
        Ok(Live {
            connection,
            slot,
            progress,
            protocol,
        })
    }

    /// Sends the request on one negotiated connection and validates the head.
    ///
    /// Only a reused connection fails [`Exchanged::Stale`]: one whose stream
    /// failed before accepting a single byte of this request, so the peer
    /// cannot have seen any of it.
    async fn exchange(
        &self,
        live: Live,
        parts: &RequestParts,
        control: &ExchangeControl,
        connection_use: ConnectionUse,
        pool: Option<(&Arc<Pool>, &PoolKey)>,
    ) -> Result<Response, Exchanged> {
        let Live {
            mut connection,
            slot,
            progress,
            protocol,
        } = live;
        let failed = |failure: Failure| {
            Exchanged::Failed(
                failure
                    .with_wire_protocol(protocol)
                    .with_connection_use(connection_use),
            )
        };
        let limits = &self.inner.limits;
        if protocol == WireProtocol::Http2 && parts.body.is_empty() {
            progress.mark_complete();
        }
        let mut wire_request =
            WireRequest::new(RequestBody::new(parts.body.clone(), Arc::clone(&progress)));
        *wire_request.method_mut() = parts.method.clone();
        *wire_request.headers_mut() = parts.headers.clone();
        let sent = match connection.sender.take() {
            Some(Sender::Http1(mut sender)) => {
                *wire_request.uri_mut() = parts.http1_target().map_err(failed)?;
                let host = parts.host_header().map_err(failed)?;
                wire_request.headers_mut().insert(http::header::HOST, host);
                wait_response(
                    sender.send_request(wire_request),
                    connection,
                    Arc::clone(&progress),
                    control,
                    limits,
                )
                .await
                .map(|(response, mut connection)| {
                    connection.sender = Some(Sender::Http1(sender));
                    (response, connection)
                })
            }
            Some(Sender::Http2(mut sender)) => {
                *wire_request.uri_mut() = parts.http2_target().map_err(failed)?;
                wait_response(
                    sender.send_request(wire_request),
                    connection,
                    Arc::clone(&progress),
                    control,
                    limits,
                )
                .await
                .map(|(response, mut connection)| {
                    // Hyper's H2 dispatch future ends when its sender drops,
                    // even while executor-owned tasks still carry the
                    // response stream.
                    connection.sender = Some(Sender::Http2(sender));
                    (response, connection)
                })
            }
            None => Err(Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                ErrorKind::Internal,
            )),
        };
        let (wire_response, connection) = match sent {
            Ok(sent) => sent,
            Err(failure)
                if connection_use == ConnectionUse::Reused
                    && failure.kind() == ErrorKind::Protocol
                    && progress.accepted() == 0 =>
            {
                return Err(Exchanged::Stale(failure));
            }
            Err(failure) => return Err(failed(failure)),
        };
        let reuse = pool.map(|(pool, key)| Reuse {
            pool: Arc::downgrade(pool),
            key: key.clone(),
            slot,
        });
        response_from_wire(
            wire_response,
            connection,
            control.clone(),
            protocol,
            Some(connection_use),
            reuse,
            limits,
        )
        .await
        .map_err(failed)
    }
}

/// One validated request, kept whole so a stale reused connection can be
/// replaced by a fresh one carrying the identical request.
struct RequestParts {
    origin: Origin,
    method: http::Method,
    path_and_query: http::uri::PathAndQuery,
    headers: HeaderMap,
    body: Bytes,
}

impl RequestParts {
    fn progress(&self, protocol: WireProtocol) -> Arc<RequestProgress> {
        Arc::new(RequestProgress::new(
            self.body.is_empty(),
            protocol == WireProtocol::Http2,
            u64::try_from(self.body.len()).unwrap_or(u64::MAX),
        ))
    }

    fn http1_target(&self) -> Result<Uri, Failure> {
        Uri::builder()
            .path_and_query(self.path_and_query.clone())
            .build()
            .map_err(internal_request_failure)
    }

    fn http2_target(&self) -> Result<Uri, Failure> {
        Uri::builder()
            .scheme("https")
            .authority(self.origin.authority().clone())
            .path_and_query(self.path_and_query.clone())
            .build()
            .map_err(internal_request_failure)
    }

    fn host_header(&self) -> Result<http::HeaderValue, Failure> {
        let mut host =
            http::HeaderValue::from_str(self.origin.authority().as_str()).map_err(|_| {
                Failure::new(
                    Phase::RequestWrite,
                    TransmissionFact::NotStarted,
                    ErrorKind::Internal,
                )
            })?;
        host.set_sensitive(true);
        Ok(host)
    }
}

/// A negotiated connection about to carry one exchange.
struct Live {
    connection: ConnectionTask,
    slot: Arc<ProgressSlot>,
    progress: Arc<RequestProgress>,
    protocol: WireProtocol,
}

enum Exchanged {
    /// A reused connection failed before accepting any byte of the request.
    Stale(Failure),
    Failed(Failure),
}

/// Where a cleanly finished exchange returns its connection.
struct Reuse {
    pool: Weak<Pool>,
    key: PoolKey,
    slot: Arc<ProgressSlot>,
}

fn default_trust_identity() -> TlsConfigIdentity {
    let mut hasher = framed_identity_hasher(b"plakous-http/webpki-ring-safe-defaults-v1");
    for anchor in webpki_roots::TLS_SERVER_ROOTS {
        update_identity_frame(&mut hasher, anchor.subject.as_ref());
        update_identity_frame(&mut hasher, anchor.subject_public_key_info.as_ref());
        match &anchor.name_constraints {
            Some(constraints) => update_identity_frame(&mut hasher, constraints.as_ref()),
            None => update_identity_frame(&mut hasher, b"<no-name-constraints>"),
        }
    }
    TlsConfigIdentity(hasher.finalize().into())
}

fn transport_behavior_identity(
    limits: &Limits,
    policy: TransportPolicy,
    trust: TlsConfigIdentity,
    pooled: bool,
    address_policy: Option<AddressPolicy>,
) -> TransportBehaviorIdentity {
    let mut hasher = framed_identity_hasher(b"plakous-http/transport-behavior-v1");
    update_identity_frame(&mut hasher, &TRANSPORT_BEHAVIOR_REVISION.to_be_bytes());
    update_identity_frame(
        &mut hasher,
        match policy {
            TransportPolicy::WebPkiHttp2Preferred => b"webpki-http2-preferred",
            TransportPolicy::WebPkiHttp1Only => b"webpki-http1-only",
            TransportPolicy::CallerTlsHttp2Preferred => b"caller-tls-http2-preferred",
        },
    );
    update_identity_frame(
        &mut hasher,
        if pooled {
            b"pooled-connections"
        } else {
            b"one-shot-connections"
        },
    );
    for value in [
        limits.max_request_headers,
        limits.max_request_header_bytes,
        limits.max_request_body_bytes,
        limits.max_response_headers,
        limits.max_response_header_bytes,
        limits.max_response_chunk_bytes,
        limits.max_response_body_bytes,
    ] {
        update_identity_frame(&mut hasher, &(value as u128).to_be_bytes());
    }
    for duration in [
        limits.resolution_timeout,
        limits.connect_timeout,
        limits.tls_timeout,
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
        limits.response_body_timeout,
    ] {
        update_identity_frame(&mut hasher, &duration.as_secs().to_be_bytes());
        update_identity_frame(&mut hasher, &duration.subsec_nanos().to_be_bytes());
    }
    update_identity_frame(&mut hasher, &trust.as_bytes());
    // Appended only when installed, so clients without a policy keep the
    // identity they had before address policies existed.
    if let Some(address_policy) = address_policy {
        update_identity_frame(&mut hasher, address_policy.identity_label());
    }
    TransportBehaviorIdentity(hasher.finalize().into())
}

/// A hasher already primed with one identity's length-framed domain separator.
/// Every identity in this module is domain-separated the same way, so the
/// separator is framed exactly once, here.
fn framed_identity_hasher(domain: &[u8]) -> Sha256 {
    let mut hasher = Sha256::new();
    update_identity_frame(&mut hasher, domain);
    hasher
}

fn update_identity_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u128).to_be_bytes());
    hasher.update(bytes);
}

fn default_tls_config() -> Result<ClientConfig, ConfigError> {
    let roots = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .cloned()
        .collect::<RootCertStore>();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| ConfigError::Http2LimitTooLarge)
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("limits", &self.inner.limits)
            .field("policy", &self.inner.policy)
            .field("pooled", &self.inner.pool.is_some())
            .field("tls_config", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// A bounded response head and pull-driven body.
pub struct Response {
    status: StatusCode,
    version: Version,
    headers: Headers,
    connection: Option<ConnectionUse>,
    body: ResponseBody,
}

impl Response {
    /// Returns the wire status without interpreting it.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the negotiated response HTTP version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns bounded, redaction-marked response headers.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Whether the exchange ran on a fresh, reused or stale-replaced
    /// connection; `None` for a response injected without a connection.
    #[must_use]
    pub const fn connection_use(&self) -> Option<ConnectionUse> {
        self.connection
    }

    /// Consumes the response and returns its pull-driven body.
    #[must_use]
    pub fn into_body(self) -> ResponseBody {
        self.body
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Response")
            .field("status", &self.status)
            .field("version", &self.version)
            .field("headers", &self.headers)
            .field("connection", &self.connection)
            .field("body", &self.body)
            .finish()
    }
}

/// A caller-pulled, chunk- and total-bounded response byte stream.
pub struct ResponseBody {
    observation: Observation,
    incoming: Incoming,
    connection: ConnectionTask,
    control: ExchangeControl,
    protocol: WireProtocol,
    connection_use: Option<ConnectionUse>,
    idle_age: Option<Duration>,
    /// Where a clean end returns the connection; `None` once that cannot happen.
    reuse: Option<Reuse>,
    /// The connection came back from its supervisor ready for another request
    /// and waits only for the terminal observation before it is pooled.
    reusable: bool,
    limits: Limits,
    bytes_seen: usize,
    pending: Option<Bytes>,
    terminal: bool,
    #[cfg(feature = "fault-injection")]
    fault: Option<Box<fault::StreamFault>>,
}

impl ResponseBody {
    /// Pulls one bounded data chunk.
    ///
    /// Trailers are validated and consumed internally. `None` is returned only
    /// after a clean end of body. A failure is returned at most once.
    pub async fn next_chunk(&mut self) -> Option<Result<PublicBytes, Failure>> {
        if self.terminal {
            return None;
        }
        let result = self.next_chunk_inner().await;
        match &result {
            Some(Ok(bytes)) => {
                if !self.observation.chunk(bytes) {
                    let failure = self.fail(ErrorKind::Internal).await;
                    self.observation.finish(ExchangeTerminal::Failed(failure));
                    return Some(Err(failure));
                }
            }
            Some(Err(failure)) => {
                if !self.observation.finish(ExchangeTerminal::Failed(*failure)) {
                    return Some(Err(Failure::new(
                        failure.phase(),
                        failure.transmission(),
                        ErrorKind::Internal,
                    )
                    .with_connection_use(self.connection_use)
                    .with_idle_age(self.idle_age)));
                }
            }
            None => {
                if !self.observation.finish(ExchangeTerminal::Complete) {
                    return Some(Err(self.fail(ErrorKind::Internal).await));
                }
                self.park().await;
            }
        }
        result
    }

    async fn next_chunk_inner(&mut self) -> Option<Result<PublicBytes, Failure>> {
        if self.terminal {
            return None;
        }
        if self.control.cancellation().is_cancelled() {
            return Some(Err(self.fail(ErrorKind::Cancelled).await));
        }
        if self
            .control
            .body_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Some(Err(self.fail(ErrorKind::DeadlineExceeded).await));
        }
        #[cfg(feature = "fault-injection")]
        if let Some(ended) = self.injected_stream_fault().await {
            return ended;
        }
        if let Some(mut pending) = self.pending.take() {
            if pending.len() > self.limits.max_response_chunk_bytes {
                let remainder = pending.split_off(self.limits.max_response_chunk_bytes);
                self.pending = Some(remainder);
            }
            return Some(Ok(self.delivered(pending)));
        }
        loop {
            let deadline = body_pull_deadline(&self.control, self.limits.response_body_timeout);
            let cancellation = self.control.cancellation().clone();
            let frame = tokio::select! {
                () = cancellation.cancelled() => {
                    return Some(Err(self.fail(ErrorKind::Cancelled).await));
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Some(Err(self.fail(ErrorKind::DeadlineExceeded).await));
                }
                frame = self.incoming.frame() => frame,
            };
            match frame {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        let Some(total) = self.bytes_seen.checked_add(data.len()) else {
                            return Some(Err(self
                                .fail(ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes))
                                .await));
                        };
                        self.bytes_seen = total;
                        if self.bytes_seen > self.limits.max_response_body_bytes {
                            return Some(Err(self
                                .fail(ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes))
                                .await));
                        }
                        let mut data = data;
                        if data.len() > self.limits.max_response_chunk_bytes {
                            let remainder = data.split_off(self.limits.max_response_chunk_bytes);
                            self.pending = Some(remainder);
                        }
                        return Some(Ok(self.delivered(data)));
                    }
                    Err(frame) => match frame.into_trailers() {
                        Ok(trailers) => {
                            if let Err(kind) = validate_response_headers(&trailers, &self.limits) {
                                return Some(Err(self.fail(kind).await));
                            }
                        }
                        Err(_) => return Some(Err(self.fail(ErrorKind::Protocol).await)),
                    },
                },
                Some(Err(error)) => {
                    return Some(Err(self
                        .fail(ErrorKind::Protocol)
                        .await
                        .with_protocol_cause(&error)));
                }
                None => {
                    self.terminal = true;
                    return self.finish_clean_eof().await;
                }
            }
        }
    }

    /// Stops and joins the owned connection task without consuming more bytes.
    ///
    /// # Errors
    ///
    /// Returns a cleanup [`Failure`] if an owned task panicked.
    pub async fn close(self) -> Result<(), Failure> {
        self.close_with_terminal(ExchangeTerminal::ClosedEarly)
            .await
    }

    /// Closes after the consumer has validated its protocol terminal.
    /// This only enriches observation; it grants no retry or task-success authority.
    ///
    /// A pooled client never waits here: only an end of body already
    /// received, with no further data, lets the connection carry another
    /// request. Otherwise the connection is closed.
    ///
    /// # Errors
    /// Returns a cleanup or required-observation failure, just like [`Self::close`].
    pub async fn close_after_protocol_terminal(self) -> Result<(), Failure> {
        self.close_with_terminal(ExchangeTerminal::ProtocolComplete)
            .await
    }

    async fn close_with_terminal(mut self, terminal: ExchangeTerminal) -> Result<(), Failure> {
        let keep = !self.terminal
            && terminal == ExchangeTerminal::ProtocolComplete
            && self.may_keep_connection()
            && self.body_ended_now();
        self.terminal = true;
        let result = if keep {
            self.release_connection(false).await
        } else {
            self.connection.abort_and_join().await
        }
        .map_err(|failure| {
            failure
                .with_connection_use(self.connection_use)
                .with_idle_age(self.idle_age)
        });
        let captured = self.observation.finish(match result {
            Ok(()) => terminal,
            Err(failure) => ExchangeTerminal::Failed(failure),
        });
        if result.is_ok() && !captured {
            self.reusable = false;
            let _ = self.connection.abort_and_join().await;
            return Err(Failure::new(
                Phase::Cleanup,
                TransmissionFact::MayHaveTransmitted,
                ErrorKind::Internal,
            )
            .with_connection_use(self.connection_use)
            .with_idle_age(self.idle_age));
        }
        self.park().await;
        result
    }

    /// Whether a clean end may return the connection to its pool: the
    /// request went out whole, so no part of it is still being sent, and the
    /// exchange was neither cancelled nor fault-injected.
    fn may_keep_connection(&self) -> bool {
        #[cfg(feature = "fault-injection")]
        if self.fault.is_some() {
            return false;
        }
        self.reuse
            .as_ref()
            .is_some_and(|reuse| reuse.slot.current().is_complete())
            && !self.control.cancellation().is_cancelled()
    }

    /// Whether the end of the body is already buffered, past nothing but
    /// empty data or valid trailers. Never waits.
    fn body_ended_now(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match Pin::new(&mut self.incoming).poll_frame(&mut context) {
                Poll::Ready(None) => return true,
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) if data.is_empty() => {}
                    Ok(_) => return false,
                    Err(frame) => match frame.into_trailers() {
                        Ok(trailers)
                            if validate_response_headers(&trailers, &self.limits).is_ok() => {}
                        _ => return false,
                    },
                },
                Poll::Ready(Some(Err(_))) | Poll::Pending => return false,
            }
        }
    }

    async fn finish_clean_eof(&mut self) -> Option<Result<PublicBytes, Failure>> {
        let result = if self.may_keep_connection() {
            self.release_connection(true).await
        } else {
            self.connection.abort_and_join().await
        };
        result.err().map(|failure| {
            Err(failure
                .with_connection_use(self.connection_use)
                .with_idle_age(self.idle_age))
        })
    }

    /// Takes the connection back from its supervisor once the body ended
    /// cleanly, keeping it only when it can carry another request. With
    /// `wait`, readiness may take up to `RELEASE_READY_BOUND`, cut short by
    /// cancellation or the body deadline; without it only readiness already
    /// at hand counts.
    async fn release_connection(&mut self, wait: bool) -> Result<(), Failure> {
        if !self.connection.release().await? || self.connection.panicked() {
            return self.connection.abort_and_join().await;
        }
        let ready = if wait {
            let deadline = body_pull_deadline(&self.control, self.limits.response_body_timeout)
                .min(tokio::time::Instant::now() + RELEASE_READY_BOUND);
            let cancellation = self.control.cancellation().clone();
            tokio::select! {
                () = cancellation.cancelled() => false,
                () = tokio::time::sleep_until(deadline) => false,
                ready = self.connection.ready() => ready,
            }
        } else {
            self.connection.ready_now()
        };
        if ready {
            self.reusable = true;
            return Ok(());
        }
        self.connection.abort_and_join().await
    }

    /// Hands a released connection to its pool, or closes it when the pool
    /// is gone.
    async fn park(&mut self) {
        if !std::mem::take(&mut self.reusable) {
            return;
        }
        let Some(reuse) = self.reuse.take() else {
            return;
        };
        let Some(pool) = reuse.pool.upgrade() else {
            let _ = self.connection.abort_and_join().await;
            return;
        };
        let connection = std::mem::replace(&mut self.connection, ConnectionTask::closed());
        pool.checkin(
            reuse.key,
            IdleConnection {
                connection,
                slot: reuse.slot,
                protocol: self.protocol,
                since: Instant::now(),
            },
        );
    }

    #[cfg(not(feature = "fault-injection"))]
    #[allow(
        clippy::unused_self,
        reason = "the fault-injection build inspects the chunk"
    )]
    fn delivered(&mut self, data: Bytes) -> Bytes {
        data
    }

    /// Counts a chunk against the stream fault and cuts it at the threshold.
    /// A stall keeps the bytes past the cut for after the pause.
    #[cfg(feature = "fault-injection")]
    fn delivered(&mut self, mut data: Bytes) -> Bytes {
        let Some(fault) = self.fault.as_mut() else {
            return data;
        };
        if let Some(cut) = fault.cut_point(&data) {
            let rest = data.split_off(cut);
            if fault.keeps_remainder() && !rest.is_empty() {
                self.pending = Some(match self.pending.take() {
                    Some(pending) => [rest, pending].concat().into(),
                    None => rest,
                });
            }
        }
        data
    }

    /// Applies a due stream fault: the same terminal a reset connection or a
    /// clean end of body produces, or a pause the body deadlines still bound.
    #[cfg(feature = "fault-injection")]
    async fn injected_stream_fault(&mut self) -> Option<Option<Result<PublicBytes, Failure>>> {
        match self.fault.as_mut()?.due()? {
            fault::StreamAction::Reset => Some(Some(Err(self.fail(ErrorKind::Protocol).await))),
            fault::StreamAction::Eof => {
                self.terminal = true;
                Some(self.finish_clean_eof().await)
            }
            fault::StreamAction::Stall(pause) => {
                let deadline = body_pull_deadline(&self.control, self.limits.response_body_timeout);
                let cancellation = self.control.cancellation().clone();
                tokio::select! {
                    () = cancellation.cancelled() => {
                        Some(Some(Err(self.fail(ErrorKind::Cancelled).await)))
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        Some(Some(Err(self.fail(ErrorKind::DeadlineExceeded).await)))
                    }
                    () = tokio::time::sleep(pause) => None,
                }
            }
        }
    }

    async fn fail(&mut self, kind: ErrorKind) -> Failure {
        self.terminal = true;
        self.reuse = None;
        self.reusable = false;
        let _ = self.connection.abort_and_join().await;
        Failure::new(
            Phase::ResponseBody,
            TransmissionFact::MayHaveTransmitted,
            kind,
        )
        .with_wire_protocol(self.protocol)
        .with_connection_use(self.connection_use)
        .with_idle_age(self.idle_age)
    }
}

impl fmt::Debug for ResponseBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseBody")
            .field("bytes_seen", &self.bytes_seen)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

/// The request half of one negotiated connection.
enum Sender {
    Http1(http1::SendRequest<RequestBody>),
    Http2(http2::SendRequest<RequestBody>),
}

impl Sender {
    fn is_ready(&self) -> bool {
        match self {
            Self::Http1(sender) => sender.is_ready(),
            Self::Http2(sender) => sender.is_ready(),
        }
    }

    async fn ready(&mut self) -> bool {
        match self {
            Self::Http1(sender) => sender.ready().await.is_ok(),
            Self::Http2(sender) => sender.ready().await.is_ok(),
        }
    }
}

async fn establish(
    stream: WireStream,
    protocol: WireProtocol,
    limits: &Limits,
    slot: Arc<ProgressSlot>,
) -> Result<ConnectionTask, Failure> {
    let io = TokioIo::new(RequestIo { stream, slot });
    let settings = ConnectionSettings::new(limits);
    let internal = || {
        Failure::new(
            Phase::Tls,
            TransmissionFact::NotStarted,
            ErrorKind::Internal,
        )
    };
    let handshake_failure = |error: hyper::Error| {
        Failure::new(
            Phase::Tls,
            TransmissionFact::NotStarted,
            ErrorKind::Protocol,
        )
        .with_protocol_cause(&error)
    };
    if protocol == WireProtocol::Http2 {
        let registry = TaskRegistry::new();
        let mut builder = http2::Builder::new(TrackedExecutor {
            registry: Arc::clone(&registry),
        });
        let receive_window = u32::try_from(settings.receive_window).map_err(|_| internal())?;
        builder
            .max_header_list_size(
                u32::try_from(settings.max_response_header_bytes).map_err(|_| internal())?,
            )
            .initial_stream_window_size(receive_window)
            .initial_connection_window_size(receive_window);
        let (sender, connection) = builder.handshake(io).await.map_err(handshake_failure)?;
        let mut connection = ConnectionTask::new(tokio::spawn(connection), Some(registry));
        connection.sender = Some(Sender::Http2(sender));
        Ok(connection)
    } else {
        let mut builder = http1::Builder::new();
        builder
            .max_headers(settings.max_response_headers)
            .max_buf_size(settings.max_response_header_bytes);
        let (sender, connection) = builder.handshake(io).await.map_err(handshake_failure)?;
        let mut connection = ConnectionTask::new(tokio::spawn(connection), None);
        connection.sender = Some(Sender::Http1(sender));
        Ok(connection)
    }
}

fn response_wait_deadline(
    last_write_progress: Instant,
    completed: Option<Instant>,
    http2: bool,
    absolute: Instant,
    write_idle: Duration,
    response_head_timeout: Duration,
) -> (tokio::time::Instant, Phase) {
    let write_deadline = fixed_phase_deadline(absolute, last_write_progress, write_idle);
    let timely_completion = completed.filter(|completed| *completed <= write_deadline.into_std());
    match timely_completion {
        Some(completed) => (
            fixed_phase_deadline(absolute, completed, response_head_timeout),
            if http2 {
                Phase::RequestWriteOrResponseHead
            } else {
                Phase::ResponseHead
            },
        ),
        None => (
            write_deadline,
            if http2 {
                Phase::RequestWriteOrResponseHead
            } else {
                Phase::RequestWrite
            },
        ),
    }
}

async fn wait_response<F>(
    response: F,
    mut connection: ConnectionTask,
    progress: Arc<RequestProgress>,
    control: &ExchangeControl,
    limits: &Limits,
) -> Result<(WireResponse<Incoming>, ConnectionTask), Failure>
where
    F: Future<Output = Result<WireResponse<Incoming>, hyper::Error>>,
{
    tokio::pin!(response);
    progress.start_write_clock(limits.request_write_idle_timeout);
    loop {
        // Register before reading completion so its notification cannot be lost.
        let completed = progress.completed.notified();
        tokio::pin!(completed);
        let complete = progress.is_complete();
        let (deadline, deadline_phase) = progress.response_deadline(control.deadline(), limits);
        let cancellation = control.cancellation().clone();
        tokio::select! {
            () = cancellation.cancelled() => {
                connection.abort_and_join().await?;
                return Err(Failure::new(
                    Phase::RequestWriteOrResponseHead,
                    TransmissionFact::MayHaveTransmitted,
                    ErrorKind::Cancelled,
                ));
            }
            () = tokio::time::sleep_until(deadline) => {
                // Progress or a timely flush may race the old idle timer.
                // Recorded times, not select ordering, own the deadline.
                // Progress after an expired idle budget cannot revive it.
                if progress.response_deadline(control.deadline(), limits) != (deadline, deadline_phase) {
                    continue;
                }
                connection.abort_and_join().await?;
                return Err(Failure::new(
                    deadline_phase,
                    TransmissionFact::MayHaveTransmitted,
                    ErrorKind::DeadlineExceeded,
                ));
            }
            () = &mut completed, if !complete => {}
            result = &mut response => {
                // A response becoming ready cannot revive an expired phase.
                // Re-read the flush timestamp: the old write timer may have
                // raced either a timely flush or this response's publication.
                let (deadline, phase) = progress.response_deadline(control.deadline(), limits);
                if tokio::time::Instant::now() >= deadline {
                    connection.abort_and_join().await?;
                    return Err(Failure::new(
                        phase,
                        TransmissionFact::MayHaveTransmitted,
                        ErrorKind::DeadlineExceeded,
                    ));
                }
                return match result {
                    Ok(response) => Ok((response, connection)),
                    Err(error) => {
                        connection.abort_and_join().await?;
                        let phase = if error.is_parse() {
                            Phase::ResponseHead
                        } else {
                            Phase::RequestWriteOrResponseHead
                        };
                        let transmission = if progress.peer_lacks_body() {
                            TransmissionFact::RequestIncomplete
                        } else {
                            TransmissionFact::MayHaveTransmitted
                        };
                        let failure = Failure::new(phase, transmission, ErrorKind::Protocol)
                            .with_protocol_cause(&error);
                        Err(if progress.peer_rejected_record.load(Ordering::Acquire) {
                            failure.with_peer_rejected_record()
                        } else {
                            failure
                        })
                    }
                }
            }
        }
    }
}

async fn response_from_wire(
    response: WireResponse<Incoming>,
    mut connection: ConnectionTask,
    control: ExchangeControl,
    protocol: WireProtocol,
    connection_use: Option<ConnectionUse>,
    reuse: Option<Reuse>,
    limits: &Limits,
) -> Result<Response, Failure> {
    let (parts, incoming) = response.into_parts();
    let head_failure = validate_response_version(parts.version)
        .err()
        .or_else(|| {
            validate_response_headers(&parts.headers, limits)
                .err()
                .map(|kind| {
                    Failure::new(
                        Phase::ResponseHead,
                        TransmissionFact::MayHaveTransmitted,
                        kind,
                    )
                })
        })
        .or_else(|| {
            (parts.status == StatusCode::SWITCHING_PROTOCOLS).then(|| {
                Failure::new(
                    Phase::ResponseHead,
                    TransmissionFact::MayHaveTransmitted,
                    ErrorKind::InvalidResponse,
                )
            })
        })
        .or_else(|| {
            content_length(&parts.headers)
                .filter(|length| {
                    *length > u64::try_from(limits.max_response_body_bytes).unwrap_or(u64::MAX)
                })
                .map(|_| {
                    Failure::new(
                        Phase::ResponseHead,
                        TransmissionFact::MayHaveTransmitted,
                        ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes),
                    )
                })
        });
    if let Some(failure) = head_failure {
        connection.abort_and_join().await?;
        return Err(failure);
    }
    let connection = connection.into_supervised(control.clone());
    Ok(Response {
        status: parts.status,
        version: parts.version,
        headers: Headers::new(parts.headers),
        connection: connection_use,
        body: ResponseBody {
            observation: Observation::default(),
            incoming,
            connection,
            control,
            protocol,
            connection_use,
            idle_age: None,
            reuse,
            reusable: false,
            limits: limits.clone(),
            bytes_seen: 0,
            pending: None,
            terminal: false,
            #[cfg(feature = "fault-injection")]
            fault: None,
        },
    })
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn validate_response_version(version: Version) -> Result<(), Failure> {
    if matches!(
        version,
        Version::HTTP_10 | Version::HTTP_11 | Version::HTTP_2
    ) {
        Ok(())
    } else {
        Err(Failure::new(
            Phase::ResponseHead,
            TransmissionFact::MayHaveTransmitted,
            ErrorKind::InvalidResponse,
        ))
    }
}

fn validate_response_headers(headers: &HeaderMap, limits: &Limits) -> Result<(), ErrorKind> {
    if headers.len() > limits.max_response_headers {
        return Err(ErrorKind::LimitExceeded(LimitKind::ResponseHeaderCount));
    }
    if header_bytes(headers).is_none_or(|bytes| bytes > limits.max_response_header_bytes) {
        return Err(ErrorKind::LimitExceeded(LimitKind::ResponseHeaderBytes));
    }
    Ok(())
}

fn request_error_kind(error: &RequestError) -> ErrorKind {
    match error {
        RequestError::LimitExceeded(kind) => ErrorKind::LimitExceeded(*kind),
        _ => ErrorKind::Internal,
    }
}

struct RequestProgress {
    http2: bool,
    consumed: AtomicBool,
    body_len: u64,
    /// Bytes the TLS stream accepted while this exchange held the connection:
    /// head, framing and body together, so it never undercounts what the peer
    /// can hold.
    accepted: AtomicU64,
    response_started: AtomicBool,
    peer_rejected_record: AtomicBool,
    complete: OnceLock<Instant>,
    completed: Notify,
    write_clock: Mutex<Option<(Instant, Duration)>>,
}

impl RequestProgress {
    fn new(consumed: bool, http2: bool, body_len: u64) -> Self {
        Self {
            body_len,
            accepted: AtomicU64::new(0),
            response_started: AtomicBool::new(false),
            peer_rejected_record: AtomicBool::new(false),
            http2,
            consumed: AtomicBool::new(consumed),
            complete: OnceLock::new(),
            completed: Notify::new(),
            write_clock: Mutex::new(None),
        }
    }

    fn start_write_clock(&self, idle: Duration) {
        *self
            .write_clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some((tokio::time::Instant::now().into_std(), idle));
    }

    fn response_deadline(
        &self,
        absolute: Instant,
        limits: &Limits,
    ) -> (tokio::time::Instant, Phase) {
        let mut clock = self
            .write_clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (last, idle) = *clock.get_or_insert_with(|| {
            (
                tokio::time::Instant::now().into_std(),
                limits.request_write_idle_timeout,
            )
        });
        response_wait_deadline(
            last,
            self.complete.get().copied(),
            self.http2,
            absolute,
            idle,
            limits.response_head_timeout,
        )
    }

    fn record_write_progress(&self) {
        let mut clock = self
            .write_clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !self.is_complete()
            && let Some((last, idle)) = clock.as_mut()
        {
            let now = tokio::time::Instant::now().into_std();
            // Once expired, freeze the clock even if this task runs before the waiter.
            if now.saturating_duration_since(*last) < *idle {
                *last = now;
            }
        }
    }

    fn mark_consumed(&self) {
        self.consumed.store(true, Ordering::Release);
        if self.http2 {
            self.mark_complete();
        }
    }

    fn mark_complete(&self) {
        let _clock = self
            .write_clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self
            .complete
            .set(tokio::time::Instant::now().into_std())
            .is_ok()
        {
            self.completed.notify_waiters();
        }
    }

    fn is_complete(&self) -> bool {
        self.complete.get().is_some()
    }

    /// Whether the peer provably lacks part of the body. A successful flush
    /// is not required evidence the other way: bytes handed to the socket may
    /// all have arrived even when their flush was never observed. Only bytes
    /// the stream never accepted cannot have arrived, so a failure with fewer
    /// accepted bytes than the body alone left the request incomplete.
    fn peer_lacks_body(&self) -> bool {
        self.accepted.load(Ordering::Acquire) < self.body_len
    }

    fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Acquire)
    }

    fn record_accepted(&self, bytes: usize) {
        if bytes > 0 && !self.http2 {
            self.record_write_progress();
        }
        self.accepted
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::AcqRel);
    }

    fn record_io_error(&self, error: &std::io::Error, tls13: bool) {
        // H1 never pipelines. Rustls 0.23 defers TLS 1.3 KeyUpdate replies
        // until the next outgoing message, ahead of its request records.
        // Post-request key-exhaustion KeyUpdate or close_notify requires about
        // 2^24 records on one connection, unreachable in practice with the
        // five-second pool idle bound.
        // TLS 1.2 may send no_renegotiation after the request, so excludes
        // this proof; H2 multiplexing does not establish it either.
        if tls13
            && !self.http2
            && !self.response_started.load(Ordering::Acquire)
            && matches!(
                error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<rustls::Error>()),
                Some(rustls::Error::AlertReceived(
                    rustls::AlertDescription::BadRecordMac
                ))
            )
        {
            self.peer_rejected_record.store(true, Ordering::Release);
        }
    }
}

/// The progress of the exchange a connection currently carries. A pooled
/// connection gets a new one for each exchange, so accepted-byte accounting
/// and the flush that completes an HTTP/1.1 request stay per exchange.
struct ProgressSlot(Mutex<Arc<RequestProgress>>);

impl ProgressSlot {
    fn new(progress: Arc<RequestProgress>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(progress)))
    }

    fn current(&self) -> Arc<RequestProgress> {
        Arc::clone(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn replace(&self, progress: Arc<RequestProgress>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = progress;
    }
}

// H1 flush follows Hyper draining its full buffered request. Body polling alone
// cannot establish this: Hyper may still be blocked writing the returned frame.
struct RequestIo {
    stream: WireStream,
    slot: Arc<ProgressSlot>,
}

/// A connected byte stream: TLS for HTTPS origins, plain TCP for loopback
/// HTTP origins.
enum WireStream {
    Tls(Box<TlsStream<TcpStream>>),
    Plain(TcpStream),
}

impl RequestIo {
    fn record_io_error(&mut self, error: &std::io::Error) {
        let progress = self.slot.current();
        let WireStream::Tls(stream) = &mut self.stream else {
            progress.record_io_error(error, false);
            return;
        };
        // A head and alert may share a socket read: Rustls can retain plaintext
        // even when processing the later alert failed before Hyper saw it.
        if stream
            .get_mut()
            .1
            .reader()
            .into_first_chunk()
            .is_ok_and(|bytes| !bytes.is_empty())
        {
            progress.response_started.store(true, Ordering::Release);
        }
        progress.record_io_error(
            error,
            stream.get_ref().1.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
        );
    }
}

impl AsyncRead for WireStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for WireStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tls(stream) => stream.is_write_vectored(),
            Self::Plain(stream) => stream.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write_vectored(cx, bufs),
            Self::Plain(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for RequestIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        let progress = self.slot.current();
        if buf.filled().len() > before {
            progress.response_started.store(true, Ordering::Release);
        }
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        result
    }
}

impl AsyncWrite for RequestIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        if let Poll::Ready(Ok(n)) = result
            && n > 0
        {
            self.slot.current().record_accepted(n);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        if let Poll::Ready(Ok(n)) = result
            && n > 0
        {
            self.slot.current().record_accepted(n);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        if matches!(result, Poll::Ready(Ok(()))) {
            let progress = self.slot.current();
            if !progress.http2
                && progress.accepted() > 0
                && progress.consumed.load(Ordering::Acquire)
            {
                progress.mark_complete();
            }
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct RequestBody {
    bytes: Option<Bytes>,
    progress: Arc<RequestProgress>,
    length: u64,
}

impl RequestBody {
    fn new(bytes: Bytes, progress: Arc<RequestProgress>) -> Self {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let bytes = if bytes.is_empty() { None } else { Some(bytes) };
        Self {
            bytes,
            progress,
            length,
        }
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(mut bytes) = self.bytes.take() {
            // H2 polls the body as flow-control capacity becomes available.
            // Bounded frames expose that progress instead of consuming the whole
            // request in one poll before it has left the dispatcher.
            let frame = if self.progress.http2 {
                let frame = bytes.split_to(bytes.len().min(16 * 1024));
                self.progress.record_write_progress();
                frame
            } else {
                bytes.split_to(bytes.len())
            };
            self.length -= u64::try_from(frame.len()).unwrap_or(u64::MAX);
            if bytes.is_empty() {
                self.progress.mark_consumed();
            } else {
                self.bytes = Some(bytes);
            }
            Poll::Ready(Some(Ok(Frame::data(frame))))
        } else {
            self.progress.mark_consumed();
            Poll::Ready(None)
        }
    }

    fn is_end_stream(&self) -> bool {
        self.bytes.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.length)
    }
}

struct ConnectionTask {
    state: Option<ConnectionState>,
    sender: Option<Sender>,
}

/// A connection driver no supervisor holds. Dropping it aborts the driver
/// and its HTTP/2 tasks, so a driver handed back by a supervisor whose owner
/// went away is never left detached.
struct DirectConnection {
    handle: Option<JoinHandle<Result<(), hyper::Error>>>,
    registry: Option<Arc<TaskRegistry>>,
}

impl DirectConnection {
    fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    async fn abort_and_join(mut self) -> Result<(), Failure> {
        abort_direct_connection(self.handle.take(), self.registry.take()).await
    }
}

impl Drop for DirectConnection {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if let Some(registry) = self.registry.take() {
            registry.abort_all();
        }
    }
}

enum ConnectionState {
    Direct(DirectConnection),
    Supervised {
        handle: JoinHandle<Result<Option<DirectConnection>, Failure>>,
        stop: CancellationToken,
        release: CancellationToken,
    },
}

impl ConnectionTask {
    fn new(
        handle: JoinHandle<Result<(), hyper::Error>>,
        registry: Option<Arc<TaskRegistry>>,
    ) -> Self {
        Self {
            state: Some(ConnectionState::Direct(DirectConnection {
                handle: Some(handle),
                registry,
            })),
            sender: None,
        }
    }

    const fn closed() -> Self {
        Self {
            state: None,
            sender: None,
        }
    }

    fn into_supervised(mut self, control: ExchangeControl) -> Self {
        let Some(ConnectionState::Direct(direct)) = self.state.take() else {
            return self;
        };
        let stop = CancellationToken::new();
        let release = CancellationToken::new();
        let handle = tokio::spawn(supervise_connection(
            direct,
            control,
            stop.clone(),
            release.clone(),
        ));
        Self {
            state: Some(ConnectionState::Supervised {
                handle,
                stop,
                release,
            }),
            sender: self.sender.take(),
        }
    }

    /// Whether an idle connection can take another request right now.
    fn is_ready(&self) -> bool {
        matches!(&self.state, Some(ConnectionState::Direct(direct)) if direct.is_running())
            && self.sender.as_ref().is_some_and(Sender::is_ready)
    }

    /// Whether a released connection can take another request right now,
    /// without waiting for it.
    fn ready_now(&mut self) -> bool {
        let ready = std::pin::pin!(self.ready());
        matches!(
            ready.poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(true)
        )
    }

    /// Whether a task this connection spawned for its streams panicked.
    fn panicked(&self) -> bool {
        matches!(
            &self.state,
            Some(ConnectionState::Direct(DirectConnection { registry: Some(registry), .. }))
                if registry.panicked()
        )
    }

    /// Waits until a released connection can take another request; `false`
    /// once it closed instead.
    async fn ready(&mut self) -> bool {
        if !matches!(&self.state, Some(ConnectionState::Direct(direct)) if direct.is_running()) {
            return false;
        }
        match self.sender.as_mut() {
            Some(sender) => sender.ready().await,
            None => false,
        }
    }

    /// Takes the driver back from the supervisor without stopping it.
    /// `false` when the supervisor already stopped it, after cancellation,
    /// the deadline or the connection's own end.
    async fn release(&mut self) -> Result<bool, Failure> {
        match self.state.take() {
            None => Ok(false),
            Some(ConnectionState::Direct(direct)) => {
                self.state = Some(ConnectionState::Direct(direct));
                Ok(true)
            }
            Some(ConnectionState::Supervised {
                handle, release, ..
            }) => {
                release.cancel();
                match handle.await {
                    Ok(Ok(Some(direct))) => {
                        self.state = Some(ConnectionState::Direct(direct));
                        Ok(true)
                    }
                    Ok(Ok(None)) => Ok(false),
                    Ok(Err(failure)) => Err(failure),
                    Err(_) => Err(cleanup_failure()),
                }
            }
        }
    }

    async fn abort_and_join(&mut self) -> Result<(), Failure> {
        self.sender = None;
        let Some(state) = self.state.take() else {
            return Ok(());
        };
        match state {
            ConnectionState::Direct(direct) => direct.abort_and_join().await,
            ConnectionState::Supervised { handle, stop, .. } => {
                stop.cancel();
                match handle.await {
                    Ok(Ok(Some(direct))) => direct.abort_and_join().await,
                    Ok(Ok(None)) => Ok(()),
                    Ok(Err(failure)) => Err(failure),
                    Err(_) => Err(cleanup_failure()),
                }
            }
        }
    }
}

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        // A direct driver aborts itself as it drops.
        if let Some(ConnectionState::Supervised { stop, .. }) = &self.state {
            stop.cancel();
        }
    }
}

/// Owns a driver while its response body is read: stops it at once on
/// cancellation or the body deadline, and on the owner's stop. On the owner's
/// release it hands a still-running driver back instead.
async fn supervise_connection(
    mut direct: DirectConnection,
    control: ExchangeControl,
    stop: CancellationToken,
    release: CancellationToken,
) -> Result<Option<DirectConnection>, Failure> {
    let cancellation = control.cancellation().clone();
    // Head-phase waits stop the connection at the head deadline themselves;
    // a streaming body is stopped by its owner's idle bound instead.
    let deadline = control.body_deadline().map(tokio::time::Instant::from_std);
    let expired = || async move {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    let Some(handle) = direct.handle.as_mut() else {
        return Ok(None);
    };
    let mut main_result = None;
    let mut released = false;
    tokio::select! {
        biased;
        () = stop.cancelled() => {}
        () = cancellation.cancelled() => {}
        () = expired() => {}
        () = release.cancelled() => released = true,
        result = handle => main_result = Some(result),
    }
    if released {
        return Ok(Some(direct));
    }
    if direct.registry.is_some() && matches!(main_result, Some(Ok(Ok(())))) {
        // A healthy H2 dispatcher can end on GOAWAY while accepted response
        // streams still use executor-owned connection work. The body owner,
        // cancellation or deadline decides when that work may be stopped.
        tokio::select! {
            () = stop.cancelled() => {}
            () = cancellation.cancelled() => {}
            () = expired() => {}
            () = release.cancelled() => {}
        }
    }
    let handle = direct.handle.take();
    let (extra_handles, pruned_panic) = direct.registry.take().map_or_else(
        || (Vec::new(), false),
        |registry| (registry.take_handles(), registry.panicked()),
    );
    if main_result.is_none()
        && let Some(handle) = &handle
    {
        handle.abort();
    }
    for extra in &extra_handles {
        extra.abort();
    }
    let main_failed = match (main_result, handle) {
        (Some(Err(error)), _) => !error.is_cancelled(),
        (Some(Ok(_)), _) | (None, None) => false,
        (None, Some(handle)) => matches!(handle.await, Err(error) if !error.is_cancelled()),
    };
    let mut extra_failed = false;
    for extra in extra_handles {
        if matches!(extra.await, Err(error) if !error.is_cancelled()) {
            extra_failed = true;
        }
    }
    if main_failed || extra_failed || pruned_panic {
        Err(cleanup_failure())
    } else {
        Ok(None)
    }
}

async fn abort_direct_connection(
    handle: Option<JoinHandle<Result<(), hyper::Error>>>,
    registry: Option<Arc<TaskRegistry>>,
) -> Result<(), Failure> {
    let (extra_handles, pruned_panic) = registry.map_or_else(
        || (Vec::new(), false),
        |registry| (registry.take_handles(), registry.panicked()),
    );
    if let Some(handle) = &handle {
        handle.abort();
    }
    for extra in &extra_handles {
        extra.abort();
    }
    let main_failed = match handle {
        Some(handle) => matches!(handle.await, Err(error) if !error.is_cancelled()),
        None => false,
    };
    let mut extra_failed = false;
    for extra in extra_handles {
        if matches!(extra.await, Err(error) if !error.is_cancelled()) {
            extra_failed = true;
        }
    }
    if main_failed || extra_failed || pruned_panic {
        Err(cleanup_failure())
    } else {
        Ok(())
    }
}

const fn cleanup_failure() -> Failure {
    Failure::new(
        Phase::Cleanup,
        TransmissionFact::MayHaveTransmitted,
        ErrorKind::Internal,
    )
}

struct TaskRegistry {
    closing: AtomicBool,
    /// A finished task pruned from `handles` had panicked.
    panicked: AtomicBool,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl TaskRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            closing: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
            handles: Mutex::new(Vec::new()),
        })
    }

    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut handles = match self.handles.lock() {
            Ok(handles) => handles,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !self.closing.load(Ordering::Acquire) {
            // A pooled connection spawns stream work for every exchange it
            // carries. Finished work is dropped here, its panic remembered.
            let mut context = Context::from_waker(Waker::noop());
            handles.retain_mut(|handle| {
                if !handle.is_finished() {
                    return true;
                }
                match Pin::new(handle).poll(&mut context) {
                    Poll::Ready(Ok(())) => false,
                    Poll::Ready(Err(error)) => {
                        if error.is_panic() {
                            self.panicked.store(true, Ordering::Release);
                        }
                        false
                    }
                    // The cooperative budget can defer even a finished task.
                    Poll::Pending => true,
                }
            });
            handles.push(tokio::spawn(future));
        }
    }

    fn panicked(&self) -> bool {
        self.panicked.load(Ordering::Acquire)
    }

    fn take_handles(&self) -> Vec<JoinHandle<()>> {
        let mut handles = match self.handles.lock() {
            Ok(handles) => handles,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.closing.store(true, Ordering::Release);
        std::mem::take(&mut *handles)
    }

    fn abort_all(&self) {
        for handle in self.take_handles() {
            handle.abort();
        }
    }
}

#[derive(Clone)]
struct TrackedExecutor {
    registry: Arc<TaskRegistry>,
}

impl<F> Executor<F> for TrackedExecutor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: F) {
        self.registry.spawn(future);
    }
}

struct TaskGuard<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> TaskGuard<T> {
    const fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

async fn wait_task<T>(
    mut task: TaskGuard<Result<T, Failure>>,
    control: &ExchangeControl,
    timeout: Duration,
    phase: Phase,
    transmission: TransmissionFact,
) -> Result<T, Failure> {
    let deadline = phase_deadline(control.deadline(), timeout);
    let cancellation = control.cancellation().clone();
    let handle = task
        .handle
        .as_mut()
        .ok_or_else(|| Failure::new(phase, transmission, ErrorKind::Internal))?;
    let result = tokio::select! {
        () = cancellation.cancelled() => {
            handle.abort();
            let _ = handle.await;
            return Err(Failure::new(phase, transmission, ErrorKind::Cancelled));
        }
        () = tokio::time::sleep_until(deadline) => {
            handle.abort();
            let _ = handle.await;
            return Err(Failure::new(
                phase,
                transmission,
                ErrorKind::DeadlineExceeded,
            ));
        }
        result = &mut *handle => result,
    };
    task.handle.take();
    result.map_err(|error| join_failure(&error, phase, transmission))?
}

fn join_failure(error: &JoinError, phase: Phase, transmission: TransmissionFact) -> Failure {
    let kind = if error.is_cancelled() {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Internal
    };
    Failure::new(phase, transmission, kind)
}

fn internal_request_failure(_error: http::Error) -> Failure {
    Failure::new(
        Phase::RequestWrite,
        TransmissionFact::NotStarted,
        ErrorKind::Internal,
    )
}

fn phase_deadline(absolute: Instant, timeout: Duration) -> tokio::time::Instant {
    fixed_phase_deadline(absolute, tokio::time::Instant::now().into_std(), timeout)
}

fn body_pull_deadline(control: &ExchangeControl, idle: Duration) -> tokio::time::Instant {
    match control.body_deadline() {
        Some(absolute) => phase_deadline(absolute, idle),
        None => tokio::time::Instant::now() + idle,
    }
}

fn fixed_phase_deadline(
    absolute: Instant,
    phase_started: Instant,
    timeout: Duration,
) -> tokio::time::Instant {
    let phase = phase_started.checked_add(timeout).unwrap_or(absolute);
    tokio::time::Instant::from_std(absolute.min(phase))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn steady_h1_and_h2_progress_outlives_whole_write_budget() {
        use http_body::Body;
        use std::{
            pin::Pin,
            sync::Arc,
            task::{Context, Waker},
        };
        for http2 in [false, true] {
            let limits = Limits {
                request_write_idle_timeout: Duration::from_millis(100),
                ..Limits::default()
            };
            let progress = Arc::new(super::RequestProgress::new(false, http2, 64 * 1024));
            let mut body =
                super::RequestBody::new(vec![b'x'; 64 * 1024].into(), Arc::clone(&progress));
            let start = tokio::time::Instant::now();
            let absolute = (start + Duration::from_secs(1)).into_std();
            progress.start_write_clock(limits.request_write_idle_timeout);
            for _ in 0..4 {
                tokio::time::advance(Duration::from_millis(60)).await;
                if http2 {
                    let mut cx = Context::from_waker(Waker::noop());
                    assert!(Pin::new(&mut body).poll_frame(&mut cx).is_ready());
                } else {
                    progress.record_accepted(16 * 1024);
                }
                let (deadline, _) = progress.response_deadline(absolute, &limits);
                assert!(deadline > tokio::time::Instant::now());
            }
            assert!(tokio::time::Instant::now() - start > limits.request_write_idle_timeout);
            if !http2 {
                progress.mark_consumed();
                progress.mark_complete();
            }
            let (deadline, phase) = progress.response_deadline(absolute, &limits);
            assert_eq!(deadline.into_std(), absolute);
            assert_eq!(
                phase,
                if http2 {
                    Phase::RequestWriteOrResponseHead
                } else {
                    Phase::ResponseHead
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn late_progress_and_flush_cannot_revive_idle_write() {
        for http2 in [false, true] {
            let limits = Limits {
                request_write_idle_timeout: Duration::from_millis(100),
                ..Limits::default()
            };
            let progress = super::RequestProgress::new(false, http2, 100);
            let start = tokio::time::Instant::now();
            let absolute = (start + Duration::from_secs(5)).into_std();
            progress.start_write_clock(limits.request_write_idle_timeout);
            tokio::time::advance(Duration::from_millis(60)).await;
            if http2 {
                progress.record_write_progress();
            } else {
                progress.record_accepted(10);
            }
            let mut waiter_limits = limits.clone();
            waiter_limits.request_write_idle_timeout = Duration::from_secs(5);
            let expected = progress.response_deadline(absolute, &waiter_limits);
            assert_eq!(expected.0, start + Duration::from_millis(160));
            tokio::time::advance(Duration::from_millis(101)).await;
            if http2 {
                progress.record_write_progress();
            } else {
                progress.record_accepted(10);
            }
            progress.mark_consumed();
            progress.mark_complete();
            assert_eq!(
                progress.response_deadline(absolute, &waiter_limits),
                expected
            );
            assert_eq!(
                expected.1,
                if http2 {
                    Phase::RequestWriteOrResponseHead
                } else {
                    Phase::RequestWrite
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn absolute_deadline_caps_steady_h1_and_h2_write() {
        for http2 in [false, true] {
            let limits = Limits {
                request_write_idle_timeout: Duration::from_millis(100),
                ..Limits::default()
            };
            let progress = super::RequestProgress::new(false, http2, 100);
            let start = tokio::time::Instant::now();
            let absolute = (start + Duration::from_millis(180)).into_std();
            progress.start_write_clock(limits.request_write_idle_timeout);
            for _ in 0..3 {
                tokio::time::advance(Duration::from_millis(60)).await;
                if http2 {
                    progress.record_write_progress();
                } else {
                    progress.record_accepted(10);
                }
            }
            assert_eq!(
                progress.response_deadline(absolute, &limits).0.into_std(),
                absolute
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_and_absolute_write_failures_keep_phase_and_transmission() {
        use std::sync::Arc;
        for http2 in [false, true] {
            for absolute_wins in [false, true] {
                let limits = Limits {
                    request_write_idle_timeout: Duration::from_millis(100),
                    ..Limits::default()
                };
                let progress = Arc::new(super::RequestProgress::new(false, http2, 100));
                let writer = Arc::clone(&progress);
                let start = tokio::time::Instant::now();
                let absolute = start
                    + if absolute_wins {
                        Duration::from_millis(180)
                    } else {
                        Duration::from_secs(5)
                    };
                let waiting = tokio::spawn(async move {
                    let connection =
                        ConnectionTask::new(tokio::spawn(std::future::pending()), None);
                    super::wait_response(
                        std::future::pending(),
                        connection,
                        progress,
                        &ExchangeControl::new(absolute.into_std()),
                        &limits,
                    )
                    .await
                });
                tokio::task::yield_now().await;
                for _ in 0..2 {
                    tokio::time::advance(Duration::from_millis(60)).await;
                    if http2 {
                        writer.record_write_progress();
                    } else {
                        writer.record_accepted(10);
                    }
                }
                let failure = waiting.await.unwrap().err().unwrap();
                assert_eq!(
                    tokio::time::Instant::now(),
                    start + Duration::from_millis(if absolute_wins { 180 } else { 220 })
                );
                assert_eq!(
                    failure.phase(),
                    if http2 {
                        Phase::RequestWriteOrResponseHead
                    } else {
                        Phase::RequestWrite
                    }
                );
                assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
                assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
            }
        }
    }

    use super::response_wait_deadline;

    fn expired_response_deadline(
        last_write_progress: Instant,
        completed: Option<Instant>,
        http2: bool,
        absolute: Instant,
        limits: &Limits,
        now: Instant,
    ) -> Option<Phase> {
        let (deadline, phase) = response_wait_deadline(
            last_write_progress,
            completed,
            http2,
            absolute,
            limits.request_write_idle_timeout,
            limits.response_head_timeout,
        );
        (now >= deadline.into_std()).then_some(phase)
    }

    #[test]
    fn ready_response_cannot_revive_expired_write_or_head_budget() {
        let start = Instant::now();
        let limits = Limits {
            request_write_idle_timeout: Duration::from_millis(150),
            response_head_timeout: Duration::from_secs(5),
            ..Limits::default()
        };
        let absolute = start + Duration::from_secs(10);
        let late = Some(start + Duration::from_millis(160));
        let timely = Some(start + Duration::from_millis(140));
        let ready = start + Duration::from_millis(180);
        assert_eq!(
            expired_response_deadline(start, late, false, absolute, &limits, ready),
            Some(Phase::RequestWrite)
        );
        assert_eq!(
            expired_response_deadline(start, timely, false, absolute, &limits, ready),
            None
        );
        assert_eq!(
            expired_response_deadline(
                start,
                timely,
                false,
                absolute,
                &limits,
                start + Duration::from_millis(5140)
            ),
            Some(Phase::ResponseHead)
        );
        assert_eq!(
            expired_response_deadline(
                start,
                timely,
                false,
                start + Duration::from_millis(170),
                &limits,
                ready
            ),
            Some(Phase::ResponseHead)
        );
    }

    #[test]
    fn recorded_flush_time_owns_write_to_head_deadline_transition() {
        let start = Instant::now();
        let limits = Limits {
            request_write_idle_timeout: Duration::from_millis(150),
            response_head_timeout: Duration::from_secs(5),
            ..Limits::default()
        };
        let absolute = start + Duration::from_secs(10);
        let before = response_wait_deadline(
            start,
            None,
            false,
            absolute,
            limits.request_write_idle_timeout,
            limits.response_head_timeout,
        );
        assert_eq!(
            before,
            (
                (start + Duration::from_millis(150)).into(),
                Phase::RequestWrite
            )
        );
        // Even if the waiter resumes after its old timer, a flush at 140ms
        // owns a new head budget; notification/timer polling order is irrelevant.
        let timely = response_wait_deadline(
            start,
            Some(start + Duration::from_millis(140)),
            false,
            absolute,
            limits.request_write_idle_timeout,
            limits.response_head_timeout,
        );
        assert_eq!(
            timely,
            (
                (start + Duration::from_millis(5140)).into(),
                Phase::ResponseHead
            )
        );
        let late = response_wait_deadline(
            start,
            Some(start + Duration::from_millis(160)),
            false,
            absolute,
            limits.request_write_idle_timeout,
            limits.response_head_timeout,
        );
        assert_eq!(
            late, before,
            "late flush must not revive the expired write budget"
        );
        let capped = response_wait_deadline(
            start,
            Some(start + Duration::from_millis(140)),
            false,
            start + Duration::from_millis(200),
            limits.request_write_idle_timeout,
            limits.response_head_timeout,
        );
        assert_eq!(
            capped,
            (
                (start + Duration::from_millis(200)).into(),
                Phase::ResponseHead
            )
        );
    }

    use super::{Client, ConnectionTask, Limits, TransportPolicy};
    use crate::{ErrorKind, ExchangeControl, Phase, TransmissionFact};
    use std::time::{Duration, Instant};
    use tokio::sync::oneshot;

    async fn panicking_driver(ready: oneshot::Sender<()>) -> Result<(), hyper::Error> {
        let _ = ready.send(());
        tokio::task::yield_now().await;
        panic!("injected connection-driver panic");
    }

    #[tokio::test]
    async fn completed_http2_dispatcher_does_not_abort_active_connection_task() {
        let registry = super::TaskRegistry::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        registry.spawn(async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
        });
        started_rx.await.expect("child started");
        let handle = tokio::spawn(async { Ok::<_, hyper::Error>(()) });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let stop = crate::CancellationToken::new();
        let supervisor = super::supervise_connection(
            super::DirectConnection {
                handle: Some(handle),
                registry: Some(registry),
            },
            ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
            stop.clone(),
            crate::CancellationToken::new(),
        );
        tokio::pin!(supervisor);
        std::future::poll_fn(|cx| {
            use std::future::Future as _;
            assert!(supervisor.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        // Process an already-completed dispatcher, then prove its body task was
        // not aborted while the response owner still requires it.
        tokio::task::yield_now().await;
        assert!(!release_tx.is_closed(), "active H2 connection was aborted");
        stop.cancel();
        supervisor.await.expect("owned cleanup");
        assert!(
            release_tx.is_closed(),
            "child must be joined after owner close"
        );
    }

    #[tokio::test]
    async fn clean_eof_surfaces_owned_task_panic_as_cleanup_failure() {
        let (ready_tx, ready_rx) = oneshot::channel();
        let handle = tokio::spawn(panicking_driver(ready_tx));
        assert!(ready_rx.await.is_ok());
        let mut connection = ConnectionTask::new(handle, None).into_supervised(
            ExchangeControl::new(Instant::now() + Duration::from_secs(1)),
        );
        let result = connection.abort_and_join().await;
        assert!(matches!(
            result,
            Err(failure)
                if failure.phase() == Phase::Cleanup
                    && failure.kind() == ErrorKind::Internal
                    && failure.transmission() == TransmissionFact::MayHaveTransmitted
        ));
    }

    #[tokio::test]
    async fn a_panicked_stream_task_pruned_early_still_fails_cleanup() {
        let registry = super::TaskRegistry::new();
        registry.spawn(async { panic!("stream task panic") });
        for _ in 0..1_000 {
            let finished = registry
                .handles
                .lock()
                .unwrap()
                .iter()
                .all(tokio::task::JoinHandle::is_finished);
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Spawning more stream work prunes the finished task.
        registry.spawn(async {});
        assert!(registry.panicked());
        assert_eq!(registry.handles.lock().unwrap().len(), 1);
        let result = super::abort_direct_connection(None, Some(registry)).await;
        assert!(matches!(
            result,
            Err(failure) if failure.phase() == Phase::Cleanup && failure.kind() == ErrorKind::Internal
        ));
    }

    #[test]
    fn one_shot_http1_client_pins_alpn_without_fallback() -> Result<(), super::ConfigError> {
        let client = Client::new_http1(Limits::default())?;
        assert_eq!(
            client.inner.tls_config.alpn_protocols,
            [b"http/1.1".to_vec()]
        );
        assert_eq!(client.transport_policy(), TransportPolicy::WebPkiHttp1Only);

        let default = Client::new(Limits::default())?;
        assert_eq!(
            default.inner.tls_config.alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert_eq!(
            default.transport_policy(),
            TransportPolicy::WebPkiHttp2Preferred
        );
        Ok(())
    }

    #[test]
    fn streaming_inference_limits_wait_the_idle_bound_not_the_rest_stall() {
        let limits = Limits::for_streaming_inference();
        let defaults = Limits::default();
        assert_eq!(limits.response_head_timeout, super::STREAMING_IDLE_TIMEOUT);
        assert_eq!(limits.response_body_timeout, super::STREAMING_IDLE_TIMEOUT);
        assert_eq!(super::STREAMING_IDLE_TIMEOUT, Duration::from_secs(180));
        assert_eq!(limits.connect_timeout, defaults.connect_timeout);
        assert_eq!(limits.tls_timeout, defaults.tls_timeout);
        assert_eq!(
            limits.request_write_idle_timeout,
            defaults.request_write_idle_timeout
        );
        assert_eq!(defaults.response_body_timeout, Duration::from_secs(30));
    }

    #[test]
    fn with_limits_keeps_tls_and_policy_and_recomputes_identity() -> Result<(), super::ConfigError>
    {
        let defaults = Limits::default();
        let client = Client::new_http1(defaults.clone())?;
        let rebuilt = client.with_limits(Limits::for_streaming_inference())?;
        assert_eq!(rebuilt.transport_policy(), client.transport_policy());
        assert_eq!(
            rebuilt.inner.tls_config.alpn_protocols,
            client.inner.tls_config.alpn_protocols
        );
        assert_eq!(
            rebuilt.limits().response_body_timeout,
            super::STREAMING_IDLE_TIMEOUT
        );
        assert_ne!(rebuilt.behavior_identity(), client.behavior_identity());
        let same = client.with_limits(defaults)?;
        assert_eq!(same.behavior_identity(), client.behavior_identity());
        Ok(())
    }
}
