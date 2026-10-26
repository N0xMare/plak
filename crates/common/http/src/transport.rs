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
use http::{HeaderMap, Uri, Version};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use pool::{ConnectionSettings, IdleConnection, Pool, PoolKey};
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_rustls::client::TlsStream;

use crate::observation::Observation;
use crate::{
    AddressPolicy, CancellationToken, ConfigError, ConnectionUse, ErrorKind, ExchangeControl,
    Failure, Headers, Origin, Phase, Request, StatusCode, TransmissionFact, WireProtocol,
};
use crate::{ExchangeObserver, ExchangeTerminal};

mod connect;
mod connection;
mod exchange;
mod identity;
mod io;
mod response;

use connection::{cleanup_failure, establish, wait_task};
pub use identity::{TlsConfigIdentity, TransportBehaviorIdentity};
use identity::{default_tls_config, default_trust_identity, transport_behavior_identity};
use response::{request_error_kind, response_from_wire, response_wait_deadline, wait_response};

#[cfg(test)]
mod address_tests;
#[cfg(feature = "fault-injection")]
pub(crate) mod fault;
mod pool;

const HTTP1_MINIMUM_BUFFER_BYTES: usize = 8_192;
const TRANSPORT_BEHAVIOR_REVISION: u32 = 5;
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

/// Resolves every address, filtering by policy before family interleaving.
async fn resolve(
    resolver: Resolver,
    origin: &Origin,
    policy: Option<AddressPolicy>,
) -> Result<Vec<SocketAddr>, Failure> {
    if policy.is_some_and(|policy| policy.refuses_host_name(origin.host())) {
        return Err(resolution_failure(ErrorKind::AddressRefused));
    }
    let addresses = resolver
        .lookup(origin.resolution_host(), origin.port())
        .await?;
    if addresses.is_empty() {
        return Err(resolution_failure(ErrorKind::Resolution));
    }
    let addresses: Vec<_> = addresses
        .into_iter()
        .filter(|address| policy.is_none_or(|policy| policy.admits(address.ip())))
        .collect();
    if addresses.is_empty() {
        return Err(resolution_failure(ErrorKind::AddressRefused));
    }
    Ok(connect::interleave(addresses))
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

/// The request half of one negotiated connection.
enum Sender {
    Http1(http1::SendRequest<RequestBody>),
    Http2(http2::SendRequest<RequestBody>),
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

struct TaskGuard<T> {
    handle: Option<JoinHandle<T>>,
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
mod tests;
