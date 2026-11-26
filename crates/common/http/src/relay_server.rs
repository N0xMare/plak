//! The credential gateway relay server core (spec 01 §7.5): one HTTP/1.1
//! connection's versions, relay and JSON calls, provider-blind.
//!
//! The caller's [`GatewayService`] authenticates, authorizes, checks a relay
//! request's pins and fence and supplies the exact credential values; this
//! core reads each request whole before it validates anything or dials
//! upstream, answers refusals before any upstream byte, fills the custodied
//! entries at their own positions, makes exactly one upstream exchange (no
//! retry, no redirect) and streams it back as frames: one data frame per
//! event of an event stream (split only above the frame bound), flushed at
//! each event boundary, and no keepalive frames. Dropping the relay
//! connection or its response cancels the upstream exchange.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt as _;
use hyper::body::{Body as _, Incoming};
use hyper_util::rt::TokioIo;
use plakous_gateway_protocol::envelope::{self, RelayEnvelope};
use plakous_gateway_protocol::frame::{Data, Frame, Head, MAX_DATA_BYTES, RelayEnd};
use plakous_gateway_protocol::header::RequestHeader;
use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous_gateway_protocol::{
    Endpoint, GatewayAccount, SUPPORTED_VERSIONS, UuidText, VersionsReply, call,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::transport::pool::{Pool, PoolScope, PoolUse};
use crate::{
    CancellationToken, Client, ErrorKind, ExchangeControl, Failure, Origin, Phase, Request,
    SensitiveBytes, TransmissionFact, sensitive_header_value,
};
use plakous_gateway_protocol::frame::{WireFailureKind, WirePhase, WireTransmission};
use plakous_gateway_protocol::header::WireHeader;

mod stream;
#[cfg(feature = "fault-injection")]
mod test_dial_override;

use stream::Chunker;

/// The largest upstream request body a relay request may carry by default.
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 32_000_000;
const RELAY_BUFFER_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const MAX_UPSTREAM_IDLE_KEYS: usize = 64;
const MAX_RELAY_DURATION: Duration = Duration::from_secs(3600);
const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_CONNECTIONS: usize = 512;
const BUSY_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
const INITIAL_READ_CAPACITY_CAP: usize = 64 * 1024;
const EOF_DRAIN_TIMEOUT: Duration = Duration::from_millis(75);
const EOF_DRAIN_MAX_BYTES: usize = 4096;

/// A fresh random gateway process instance id; `None` only when the system
/// random source fails.
#[must_use]
pub fn new_instance() -> Option<UuidText> {
    crate::gateway_wire::random_uuid()
}

/// How the caller of one connection reached the gateway.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Peer {
    /// A Unix socket peer; `same_user` when its UID equals the gateway's.
    Unix {
        /// Whether the peer runs as the gateway's own user.
        same_user: bool,
    },
    /// A TCP peer at this address.
    Tcp(SocketAddr),
    /// The in-process gateway's own host.
    InProcess,
}

/// A boxed gateway service future.
pub type ServiceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, GatewayRefusal>> + Send + 'a>>;

/// The exact value of each custodied entry, in the envelope's custodied
/// order, and what the custodian holds while they are in use.
pub struct Injection {
    values: Vec<SensitiveBytes>,
    hold: Option<Box<dyn std::any::Any + Send>>,
    cut: Option<CancellationToken>,
    relay_guard: Option<Box<dyn std::any::Any + Send>>,
    reuse_upstream: bool,
}

impl Injection {
    /// Values for the custodied entries, in order.
    #[must_use]
    pub const fn new(values: Vec<SensitiveBytes>) -> Self {
        Self {
            values,
            hold: None,
            cut: None,
            relay_guard: None,
            reuse_upstream: false,
        }
    }

    /// Allows this relay to reuse an upstream TLS connection within the
    /// validated credential fence. Only set this for inference with
    /// request-scoped credentials, after validating the pin and exact fence.
    #[must_use]
    pub const fn reuse_upstream(mut self) -> Self {
        self.reuse_upstream = true;
        self
    }

    /// Keeps `hold` (a credential store's Ready lease, say) until the upstream
    /// exchange yields its response head or fails.
    #[must_use]
    pub fn holding(mut self, hold: Box<dyn std::any::Any + Send>) -> Self {
        self.hold = Some(hold);
        self
    }

    /// Ends this relay when `cut` is cancelled (the gateway's kill switch):
    /// the upstream exchange is dropped and the caller gets an `End`
    /// `cancelled` frame.
    #[must_use]
    pub fn cut_by(mut self, cut: CancellationToken) -> Self {
        self.cut = Some(cut);
        self
    }

    /// Keeps `guard` alive for the entire relay lifecycle (including response
    /// streaming), dropping it only after the relay completes, fails, is cut,
    /// or is abandoned.
    #[must_use]
    pub fn guarded_by(mut self, guard: Box<dyn std::any::Any + Send>) -> Self {
        self.relay_guard = Some(guard);
        self
    }
}

impl std::fmt::Debug for Injection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Injection")
            .field("values", &self.values.len())
            .field("holding", &self.hold.is_some())
            .field("cut", &self.cut.is_some())
            .field("guarded", &self.relay_guard.is_some())
            .field("reuse_upstream", &self.reuse_upstream)
            .finish()
    }
}

/// How one relay ended, for the gateway's audit line. Never carries header
/// values, bodies, queries or credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayRecord {
    /// The account, when the envelope named one.
    pub account: Option<GatewayAccount>,
    /// The caller's operation id and physical attempt.
    pub operation: Option<(UuidText, u32)>,
    /// The upstream origin and path, without the query.
    pub upstream: Option<(String, String)>,
    /// The upstream method.
    pub method: Option<String>,
    /// The upstream status, once a head arrived.
    pub upstream_status: Option<u16>,
    /// Upstream body bytes relayed.
    pub bytes_down: u64,
    /// Redacted transport failure, for the local audit only.
    pub upstream_failure: Option<Failure>,
    /// Time from the whole request read to the end.
    pub duration: Duration,
    /// How the relay ended.
    pub outcome: RelayOutcome,
}

/// How one relay ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayOutcome {
    /// Refused before any upstream byte.
    Refused(&'static str),
    /// The upstream body ended cleanly.
    Complete,
    /// The upstream exchange failed with this transmission fact.
    UpstreamFailed(TransmissionFact),
    /// The caller left; the upstream exchange was dropped.
    Abandoned,
    /// The gateway's kill switch cut the exchange.
    Cut,
}

/// The gateway's own policy: who may call, which upstreams a credential may
/// reach and its exact values, and the JSON calls.
pub trait GatewayService: Send + Sync + 'static {
    /// One authenticated caller.
    type Principal: Send + Sync + 'static;

    /// Authenticates one call from its caller key (`None` when absent) and
    /// how the caller connected. Every failure is the uniform
    /// [`GatewayRefusal::Unauthenticated`].
    ///
    /// # Errors
    /// Refuses an unknown key, a wrong source or a peer that needs a key.
    fn authenticate(
        &self,
        peer: &Peer,
        key: Option<&[u8]>,
    ) -> Result<Self::Principal, GatewayRefusal>;

    /// Reserves an in-flight admission slot for `principal` before the relay
    /// request body is buffered against the shared memory budget. The returned
    /// guard stays alive until the relay finishes, fails, is cut, or is
    /// abandoned.
    ///
    /// # Errors
    /// Refuses when draining or when global or per-caller in-flight bounds are
    /// reached.
    fn admit_relay(
        &self,
        _principal: &Self::Principal,
    ) -> Result<Option<Box<dyn std::any::Any + Send>>, GatewayRefusal> {
        Ok(None)
    }

    /// Checks one decoded relay request for `principal`: flags and relay
    /// scope for its account, its exact upstream pin, its custodied names
    /// against the credential's injection rule and the credential's fence,
    /// then supplies the exact values of its custodied entries.
    fn inject<'a>(
        &'a self,
        principal: &'a Self::Principal,
        version: u16,
        envelope: &'a RelayEnvelope,
    ) -> ServiceFuture<'a, Injection>;

    /// One JSON call (`facts`, `refresh` or `admin`): the decoded request
    /// body's answer, encoded.
    fn call<'a>(
        &'a self,
        principal: &'a Self::Principal,
        version: u16,
        endpoint: Endpoint,
        body: &'a [u8],
    ) -> ServiceFuture<'a, Vec<u8>>;

    /// Records how one relay ended. The default records nothing.
    fn audit(&self, _record: RelayRecord) {}

    /// Records how one relay ended with its authenticated principal. The
    /// default forwards to [`Self::audit`].
    fn audit_for(&self, _principal: &Self::Principal, record: RelayRecord) {
        self.audit(record);
    }
}

/// One gateway's relay server core over its upstream client.
pub struct RelayServer<S> {
    inner: Arc<ServerInner<S>>,
}

impl<S> Clone for RelayServer<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct ServerInner<S> {
    service: Arc<S>,
    upstream: Client,
    upstream_pool: Arc<Pool>,
    instance: UuidText,
    max_request_body_bytes: usize,
    header_read_timeout: Duration,
    request_body_timeout: Duration,
    connection_limit: Arc<Semaphore>,
    request_budget: Arc<Semaphore>,
    draining: CancellationToken,
    /// Repository tests' loopback dial targets for pinned origins; always
    /// empty in product builds, which cannot set them.
    dial_overrides: Vec<(String, Origin)>,
}

/// Why serving one connection stopped early.
#[derive(Debug, thiserror::Error)]
#[error("credential gateway connection failed")]
pub struct ServeError(#[source] hyper::Error);

impl<S: GatewayService> RelayServer<S> {
    /// A server for gateway process `instance` ([`new_instance`]) making
    /// upstream exchanges with `upstream`; that client's fault plan, when it
    /// has one, applies to the upstream hop.
    #[must_use]
    pub fn new(service: Arc<S>, upstream: Client, instance: UuidText) -> Self {
        Self {
            inner: Arc::new(ServerInner {
                service,
                upstream,
                upstream_pool: Arc::new(Pool::new(Some(MAX_UPSTREAM_IDLE_KEYS))),
                instance,
                max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
                header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
                request_body_timeout: DEFAULT_REQUEST_BODY_TIMEOUT,
                connection_limit: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
                request_budget: Arc::new(Semaphore::new(RELAY_BUFFER_BUDGET_BYTES)),
                draining: CancellationToken::new(),
                dial_overrides: Vec::new(),
            }),
        }
    }

    /// The same server with another upstream body bound.
    #[must_use]
    pub fn with_max_request_body_bytes(self, bytes: usize) -> Self {
        self.rebuild(|inner| inner.max_request_body_bytes = bytes)
    }

    /// The same server with another HTTP/1.1 header read timeout.
    #[must_use]
    pub fn with_header_read_timeout(self, timeout: Duration) -> Self {
        self.rebuild(|inner| inner.header_read_timeout = timeout)
    }

    /// The same server with another request body read timeout.
    #[must_use]
    pub fn with_request_body_timeout(self, timeout: Duration) -> Self {
        self.rebuild(|inner| inner.request_body_timeout = timeout)
    }

    /// The same server with another concurrent connection cap.
    #[must_use]
    pub fn with_max_connections(self, limit: usize) -> Self {
        self.rebuild(|inner| inner.connection_limit = Arc::new(Semaphore::new(limit.max(1))))
    }

    fn rebuild(self, change: impl FnOnce(&mut ServerInner<S>)) -> Self {
        let inner = &self.inner;
        let mut next = ServerInner {
            service: Arc::clone(&inner.service),
            upstream: inner.upstream.clone(),
            upstream_pool: Arc::clone(&inner.upstream_pool),
            instance: inner.instance.clone(),
            max_request_body_bytes: inner.max_request_body_bytes,
            header_read_timeout: inner.header_read_timeout,
            request_body_timeout: inner.request_body_timeout,
            connection_limit: Arc::clone(&inner.connection_limit),
            request_budget: Arc::clone(&inner.request_budget),
            draining: inner.draining.clone(),
            dial_overrides: inner.dial_overrides.clone(),
        };
        change(&mut next);
        Self {
            inner: Arc::new(next),
        }
    }

    /// This gateway process instance; it changes when the gateway restarts.
    #[must_use]
    pub fn instance(&self) -> &UuidText {
        &self.inner.instance
    }

    /// Starts graceful shutdown on connections served by this instance.
    /// They stop accepting requests but flush responses already in flight.
    /// The listener remains the caller's responsibility.
    pub fn drain(&self) {
        self.inner.draining.cancel();
    }

    /// Serves one connection until the caller closes it or the server drains.
    ///
    /// # Errors
    /// Returns the HTTP/1.1 connection failure.
    pub async fn serve<IO>(&self, io: IO, peer: Peer) -> Result<(), ServeError>
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let Ok(_connection_permit) = Arc::clone(&self.inner.connection_limit).try_acquire_owned()
        else {
            return Self::serve_busy(io).await;
        };
        let inner = Arc::clone(&self.inner);
        let service = hyper::service::service_fn(move |request| {
            let inner = Arc::clone(&inner);
            async move { Ok::<_, Infallible>(inner.handle(request, peer).await) }
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .keep_alive(true)
            .header_read_timeout(self.inner.header_read_timeout)
            .timer(hyper_util::rt::TokioTimer::new())
            .serve_connection(TokioIo::new(io), service);
        tokio::pin!(connection);
        tokio::select! {
            biased;
            () = self.inner.draining.cancelled() => {
                connection.as_mut().graceful_shutdown();
                connection.await.map_err(ServeError)
            }
            result = &mut connection => result.map_err(ServeError),
        }
    }

    /// Parses a request head and sends one typed capacity refusal without
    /// authenticating or dispatching. Then half-closes and briefly drains
    /// in-flight bytes. The caller bounds concurrent rejection tasks.
    ///
    /// # Errors
    /// Returns the HTTP/1.1 connection failure, if any.
    pub async fn serve_busy<IO>(io: IO) -> Result<(), ServeError>
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let service = hyper::service::service_fn(|_: http::Request<Incoming>| async {
            Ok::<_, Infallible>(refusal_reply(&GatewayRefusal::Busy {
                retry_after_ms: 500,
            }))
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .keep_alive(false)
            .header_read_timeout(None)
            .serve_connection(TokioIo::new(io), service);
        let answer = async {
            let parts = connection.without_shutdown().await?;
            let mut io = parts.io.into_inner();
            if io.shutdown().await.is_ok() {
                let mut remainder = [0; 4096];
                let _ = io.read(&mut remainder).await;
            }
            Ok(())
        };
        match tokio::time::timeout(BUSY_RESPONSE_TIMEOUT, answer).await {
            Ok(result) => result.map_err(ServeError),
            Err(_) => Ok(()),
        }
    }
}

type Reply = http::Response<FrameBody>;

impl<S: GatewayService> ServerInner<S> {
    async fn handle(self: Arc<Self>, request: http::Request<Incoming>, peer: Peer) -> Reply {
        match self.route(request, peer).await {
            Ok(reply) => reply,
            Err(refusal) => refusal_reply(&refusal),
        }
    }

    async fn route(
        self: &Arc<Self>,
        request: http::Request<Incoming>,
        peer: Peer,
    ) -> Result<Reply, GatewayRefusal> {
        let key = request
            .headers()
            .get(plakous_gateway_protocol::KEY_HEADER)
            .map(HeaderValue::as_bytes);
        let principal = self.service.authenticate(&peer, key)?;
        let path = request.uri().path().to_owned();
        if path == plakous_gateway_protocol::VERSIONS_PATH {
            if request.method() != Method::GET {
                return Err(GatewayRefusal::EnvelopeInvalid {});
            }
            let reply = VersionsReply {
                versions: SUPPORTED_VERSIONS.to_vec(),
                instance: self.instance.clone(),
            };
            let body = call::encode(&reply).map_err(|_| GatewayRefusal::Internal {})?;
            return Ok(bytes_reply(StatusCode::OK, "application/json", body));
        }
        let (version, endpoint) =
            Endpoint::parse(&path).ok_or(GatewayRefusal::EnvelopeInvalid {})?;
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(GatewayRefusal::VersionUnsupported {
                supported: SUPPORTED_VERSIONS.to_vec(),
            });
        }
        if request.method() != Method::POST || request.uri().query().is_some() {
            return Err(GatewayRefusal::EnvelopeInvalid {});
        }
        match endpoint {
            Endpoint::Relay => self.relay(request, principal, version).await,
            Endpoint::Facts | Endpoint::Refresh | Endpoint::Admin => {
                let Ok(body) = tokio::time::timeout(
                    self.request_body_timeout,
                    read_whole(request.into_body(), call::MAX_CALL_BYTES),
                )
                .await
                else {
                    return Ok(body_timeout_reply());
                };
                let body = body?;
                let reply = self
                    .service
                    .call(&principal, version, endpoint, &body)
                    .await?;
                Ok(bytes_reply(StatusCode::OK, "application/json", reply))
            }
        }
    }

    async fn relay(
        self: &Arc<Self>,
        request: http::Request<Incoming>,
        principal: S::Principal,
        version: u16,
    ) -> Result<Reply, GatewayRefusal> {
        let content_type = request.headers().get(http::header::CONTENT_TYPE);
        if content_type.map(HeaderValue::as_bytes)
            != Some(plakous_gateway_protocol::RELAY_REQUEST_CONTENT_TYPE.as_bytes())
        {
            return Err(GatewayRefusal::EnvelopeInvalid {});
        }
        // Reserve per-caller and global in-flight capacity before reserving
        // from the shared buffer budget or polling the request body.
        let admission = self.service.admit_relay(&principal)?;
        // The whole request is read before anything is validated or dialed,
        // so a relay request that did not arrive whole was never sent.
        let max_bound = self
            .max_request_body_bytes
            .saturating_add(4 + envelope::MAX_ENVELOPE_BYTES);
        let exact = request.body().size_hint().exact();
        let bound = match exact {
            Some(bytes) => usize::try_from(bytes)
                .ok()
                .filter(|bytes| *bytes <= max_bound)
                .ok_or(GatewayRefusal::LimitExceeded {})?,
            None => max_bound,
        };
        if self.request_budget.available_permits() == 0
            || exact.is_some_and(|bytes| {
                usize::try_from(bytes)
                    .is_ok_and(|bytes| bytes > self.request_budget.available_permits())
            })
        {
            return Err(GatewayRefusal::Busy {
                retry_after_ms: 500,
            });
        }
        let Ok(body) = tokio::time::timeout(
            self.request_body_timeout,
            read_whole_budgeted(request.into_body(), bound, &self.request_budget),
        )
        .await
        else {
            return Ok(body_timeout_reply());
        };
        let (whole, buffer_hold) = body?;
        let started = Instant::now();
        let (envelope, upstream_body) =
            envelope::decode_request(&whole).map_err(|error| match error {
                envelope::EnvelopeError::ReservedHeader => GatewayRefusal::ReservedHeader {},
                envelope::EnvelopeError::EnvelopeTooLarge => GatewayRefusal::LimitExceeded {},
                _ => GatewayRefusal::EnvelopeInvalid {},
            })?;
        let body_start = whole.len() - upstream_body.len();
        let upstream_body = whole.slice(body_start..);
        if upstream_body.len() > self.max_request_body_bytes {
            return Err(GatewayRefusal::LimitExceeded {});
        }
        let mut record = RelayRecord {
            account: Some(envelope.account.clone()),
            operation: Some((envelope.operation.clone(), envelope.attempt)),
            upstream: None,
            method: Some(envelope.method.clone()),
            upstream_status: None,
            bytes_down: 0,
            upstream_failure: None,
            duration: Duration::ZERO,
            outcome: RelayOutcome::Complete,
        };
        let prepared = match self
            .prepare(&principal, version, &envelope, upstream_body)
            .await
        {
            Ok(prepared) => prepared,
            Err(refusal) => {
                record.outcome = RelayOutcome::Refused(refusal.code());
                record.duration = started.elapsed();
                self.service.audit_for(&principal, record);
                return Err(refusal);
            }
        };
        record.upstream = Some(prepared.audit_target.clone());
        let (sender, receiver) = mpsc::channel(4);
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner
                .pump(
                    principal,
                    admission,
                    buffer_hold,
                    prepared,
                    sender,
                    record,
                    started,
                )
                .await;
        });
        Ok(http::Response::builder()
            .status(StatusCode::OK)
            .header(
                http::header::CONTENT_TYPE,
                plakous_gateway_protocol::RELAY_FRAMES_CONTENT_TYPE,
            )
            .body(FrameBody(receiver))
            .unwrap_or_else(|_| refusal_reply(&GatewayRefusal::Internal {})))
    }

    /// Validates through the service and builds the exact upstream request.
    async fn prepare(
        &self,
        principal: &S::Principal,
        version: u16,
        envelope: &RelayEnvelope,
        body: Bytes,
    ) -> Result<Prepared, GatewayRefusal> {
        let (origin_text, path_and_query) = envelope::split_upstream_url(&envelope.url)
            .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        let method = Method::from_bytes(envelope.method.as_bytes())
            .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        let Injection {
            values,
            hold,
            cut,
            relay_guard,
            reuse_upstream,
        } = self.service.inject(principal, version, envelope).await?;
        let mut values = values.into_iter();
        let mut headers = HeaderMap::with_capacity(envelope.headers.len());
        for header in &envelope.headers {
            let (name, value) = match header {
                RequestHeader::Value(header) => (
                    header.name(),
                    HeaderValue::from_bytes(header.value())
                        .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?,
                ),
                RequestHeader::Custodied { name } => (
                    name.as_str(),
                    sensitive_header_value(values.next().ok_or(GatewayRefusal::Internal {})?)
                        .map_err(|_| GatewayRefusal::Internal {})?,
                ),
            };
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
            headers.append(name, value);
        }
        if values.next().is_some() {
            return Err(GatewayRefusal::Internal {});
        }
        let origin = self.dial_origin(origin_text)?;
        let pool_scope = (reuse_upstream && !origin.is_loopback_http()).then(|| {
            Arc::new(PoolScope {
                logical_origin: origin_text.to_owned(),
                version,
                account: envelope.account.as_str().to_owned(),
                account_binding: envelope.expected.account_binding.as_str().to_owned(),
                generation: envelope.expected.generation,
            })
        });
        let limits = upstream_limits(self.upstream.limits(), envelope);
        let upstream = self
            .upstream
            .with_limits(limits)
            .map_err(|_| GatewayRefusal::LimitExceeded {})?;
        let request = Request::new(
            origin,
            method,
            path_and_query,
            headers,
            body,
            upstream.limits(),
        )
        .map_err(|error| match error {
            crate::RequestError::LimitExceeded(_) => GatewayRefusal::LimitExceeded {},
            crate::RequestError::ControlledHeader(_) => GatewayRefusal::ReservedHeader {},
            _ => GatewayRefusal::EnvelopeInvalid {},
        })?;
        let path = path_and_query
            .split_once('?')
            .map_or(path_and_query, |(path, _)| path);
        Ok(Prepared {
            upstream,
            request,
            pool_scope,
            hold,
            cut,
            relay_guard,
            deadline: envelope_deadline(envelope),
            max_chunk: usize::try_from(envelope.limits.max_chunk_bytes)
                .unwrap_or(MAX_DATA_BYTES)
                .min(MAX_DATA_BYTES),
            audit_target: (origin_text.to_owned(), path.to_owned()),
        })
    }

    fn dial_origin(&self, logical: &str) -> Result<Origin, GatewayRefusal> {
        if let Some((_, dial)) = self.dial_overrides.iter().find(|(from, _)| from == logical) {
            return Ok(dial.clone());
        }
        Origin::parse(&format!("{logical}/")).map_err(|_| GatewayRefusal::UpstreamNotPinned {})
    }

    /// Makes the upstream exchange and streams it as frames until it ends or
    /// the caller leaves, which drops the upstream exchange.
    #[allow(clippy::too_many_arguments, reason = "internal relay pump state")]
    async fn pump(
        &self,
        principal: S::Principal,
        admission: Option<Box<dyn std::any::Any + Send>>,
        buffer_hold: Option<OwnedSemaphorePermit>,
        prepared: Prepared,
        sender: mpsc::Sender<Bytes>,
        mut record: RelayRecord,
        started: Instant,
    ) {
        let _admission = admission;
        let Prepared {
            upstream,
            request,
            pool_scope,
            hold,
            cut,
            relay_guard,
            deadline,
            max_chunk,
            ..
        } = prepared;
        let _relay_guard = relay_guard;
        let cancellation = CancellationToken::new();
        let control = ExchangeControl::streaming(deadline, cancellation.clone());
        let pool = pool_scope.map(|scope| PoolUse {
            pool: &self.upstream_pool,
            scope: Some(scope),
        });
        let outcome = tokio::select! {
            biased;
            () = sender.closed() => {
                cancellation.cancel();
                None
            }
            () = cut_signal(cut.as_ref()) => {
                cancellation.cancel();
                let _ = send(&sender, &Frame::End(RelayEnd::Cancelled {})).await;
                Some(None)
            }
            result = upstream.execute_with_pool(request, control, pool) => Some(Some(result)),
        };
        // The upstream request body has finished uploading (or failed/aborted);
        // release both the custodian lease and the buffered request budget
        // before streaming the response.
        drop(hold);
        drop(buffer_hold);
        record.outcome = match outcome {
            None => RelayOutcome::Abandoned,
            Some(None) => RelayOutcome::Cut,
            Some(Some(Err(failure))) => {
                record.upstream_failure = Some(failure);
                let _ = send(&sender, &upstream_failed(failure)).await;
                RelayOutcome::UpstreamFailed(failure.transmission())
            }
            Some(Some(Ok(response))) => {
                record.upstream_status = Some(response.status().as_u16());
                let control = StreamControl {
                    sender: &sender,
                    cancellation: &cancellation,
                    cut: cut.as_ref(),
                };
                stream(response, max_chunk, &control, &mut record).await
            }
        };
        record.duration = started.elapsed();
        self.service.audit_for(&principal, record);
    }
}

/// Resolves when `cut` is cancelled; never without one.
async fn cut_signal(cut: Option<&CancellationToken>) {
    match cut {
        Some(cut) => cut.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Where one relay's frames go and what ends it early.
struct StreamControl<'a> {
    sender: &'a mpsc::Sender<Bytes>,
    cancellation: &'a CancellationToken,
    cut: Option<&'a CancellationToken>,
}

/// Streams one upstream response as `Head`, `Data` and `End` frames.
async fn stream(
    response: crate::Response,
    max_chunk: usize,
    control: &StreamControl<'_>,
    record: &mut RelayRecord,
) -> RelayOutcome {
    let StreamControl {
        sender,
        cancellation,
        cut,
    } = *control;
    let event_stream = response
        .headers()
        .get(&http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        });
    let Some(headers) = wire_headers(
        response
            .headers()
            .iter()
            .filter(|(name, _)| !is_hop_by_hop(name)),
    ) else {
        let _ = send(sender, &Frame::End(RelayEnd::GatewayFailed {})).await;
        return RelayOutcome::UpstreamFailed(TransmissionFact::MayHaveTransmitted);
    };
    // A head the frame bounds cannot carry is the gateway's failure after
    // the upstream may have acted, not the caller leaving.
    let Ok(head) = Frame::Head(Head {
        status: response.status().as_u16(),
        headers,
    })
    .encode() else {
        let _ = send(sender, &Frame::End(RelayEnd::GatewayFailed {})).await;
        return RelayOutcome::UpstreamFailed(TransmissionFact::MayHaveTransmitted);
    };
    if sender.send(Bytes::from(head)).await.is_err() {
        cancellation.cancel();
        return RelayOutcome::Abandoned;
    }
    let mut body = response.into_body();
    let mut chunker = Chunker::new(event_stream, max_chunk);
    let mut seq = 0;
    loop {
        let next = tokio::select! {
            biased;
            () = sender.closed() => {
                if record.bytes_down > 0 {
                    return drain_upstream_eof(&mut body, cancellation, cut).await;
                }
                cancellation.cancel();
                return RelayOutcome::Abandoned;
            }
            () = cut_signal(cut) => {
                cancellation.cancel();
                let _ = send(sender, &Frame::End(RelayEnd::Cancelled {})).await;
                return RelayOutcome::Cut;
            }
            next = body.next_chunk() => next,
        };
        let ended = match next {
            Some(Ok(bytes)) => {
                chunker.push(&bytes);
                None
            }
            Some(Err(failure)) => Some(Err(failure)),
            None => Some(Ok(())),
        };
        if ended.is_some() {
            chunker.finish();
        }
        while let Some(bytes) = chunker.next_frame() {
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let frame = Frame::Data(Data {
                seq,
                offset: record.bytes_down,
                bytes,
            });
            if send(sender, &frame).await.is_err() {
                if ended.is_none() && record.bytes_down > 0 {
                    return drain_upstream_eof(&mut body, cancellation, cut).await;
                }
                if matches!(ended, Some(Ok(()))) && record.bytes_down > 0 {
                    return RelayOutcome::Complete;
                }
                cancellation.cancel();
                return RelayOutcome::Abandoned;
            }
            seq += 1;
            record.bytes_down += len;
        }
        match ended {
            None => {}
            Some(Ok(())) => {
                let _ = send(sender, &Frame::End(RelayEnd::Complete {})).await;
                return RelayOutcome::Complete;
            }
            Some(Err(failure)) => {
                record.upstream_failure = Some(failure);
                let _ = send(sender, &upstream_failed(failure)).await;
                return RelayOutcome::UpstreamFailed(failure.transmission());
            }
        }
    }
}

/// Gives a nearly-finished upstream response body a short bounded window to
/// reach clean EOF after the caller disconnects (for example on a protocol
/// typed-terminal cutoff) so the upstream connection returns to the pool
/// cleanly instead of RST-abandoning.
async fn drain_upstream_eof(
    body: &mut crate::ResponseBody,
    cancellation: &CancellationToken,
    cut: Option<&CancellationToken>,
) -> RelayOutcome {
    let deadline = tokio::time::Instant::now() + EOF_DRAIN_TIMEOUT;
    let mut drained = 0usize;
    loop {
        let next = tokio::select! {
            biased;
            () = cut_signal(cut) => {
                cancellation.cancel();
                return RelayOutcome::Cut;
            }
            () = tokio::time::sleep_until(deadline) => {
                cancellation.cancel();
                return RelayOutcome::Abandoned;
            }
            next = body.next_chunk() => next,
        };
        match next {
            None => return RelayOutcome::Complete,
            Some(Ok(bytes)) => {
                drained = drained.saturating_add(bytes.len());
                if drained > EOF_DRAIN_MAX_BYTES {
                    cancellation.cancel();
                    return RelayOutcome::Abandoned;
                }
            }
            Some(Err(_)) => {
                cancellation.cancel();
                return RelayOutcome::Abandoned;
            }
        }
    }
}

struct Prepared {
    upstream: Client,
    request: Request,
    pool_scope: Option<Arc<PoolScope>>,
    hold: Option<Box<dyn std::any::Any + Send>>,
    cut: Option<CancellationToken>,
    relay_guard: Option<Box<dyn std::any::Any + Send>>,
    deadline: Instant,
    max_chunk: usize,
    audit_target: (String, String),
}

/// The upstream bounds: the gateway client's own, narrowed by the caller's.
fn upstream_limits(limits: &crate::Limits, envelope: &RelayEnvelope) -> crate::Limits {
    let millis = |value: u64| Duration::from_millis(value);
    let caller = &envelope.limits;
    crate::Limits {
        response_head_timeout: limits
            .response_head_timeout
            .min(millis(caller.head_timeout_ms)),
        response_body_timeout: limits
            .response_body_timeout
            .min(millis(caller.body_idle_timeout_ms)),
        max_response_body_bytes: limits
            .max_response_body_bytes
            .min(usize::try_from(caller.max_response_bytes).unwrap_or(usize::MAX)),
        ..limits.clone()
    }
}

/// The caller's hard deadline, clamped to [`MAX_RELAY_DURATION`] so an
/// untrusted `deadline_unix_ms` (including `u64::MAX`) never panics
/// `Instant::add` or holds a relay indefinitely.
fn envelope_deadline(envelope: &RelayEnvelope) -> Instant {
    let now_instant = Instant::now();
    let max_deadline = now_instant
        .checked_add(MAX_RELAY_DURATION)
        .unwrap_or(now_instant);
    let Some(deadline) = envelope.deadline_unix_ms else {
        return max_deadline;
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    let remaining = Duration::from_millis(deadline.saturating_sub(now_ms)).min(MAX_RELAY_DURATION);
    now_instant.checked_add(remaining).unwrap_or(max_deadline)
}

fn upstream_failed(failure: Failure) -> Frame {
    Frame::End(RelayEnd::UpstreamFailed {
        phase: phase_to_wire(failure.phase()),
        transmission: transmission_to_wire(failure.transmission()),
        kind: kind_to_wire(failure.kind()),
        timeout: failure.kind() == ErrorKind::DeadlineExceeded,
    })
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "te" | "trailer" | "transfer-encoding" | "upgrade"
    ) || name.as_str().starts_with("proxy-")
}

async fn send(sender: &mpsc::Sender<Bytes>, frame: &Frame) -> Result<(), ()> {
    let encoded = frame.encode().map_err(|_| ())?;
    sender.send(Bytes::from(encoded)).await.map_err(|_| ())
}

/// Reads a whole request body within its byte bound.
async fn read_whole(mut body: Incoming, bound: usize) -> Result<Bytes, GatewayRefusal> {
    let capacity = body
        .size_hint()
        .exact()
        .and_then(|bytes| usize::try_from(bytes).ok())
        .map_or(0, |bytes| bytes.min(bound).min(INITIAL_READ_CAPACITY_CAP));
    let mut whole = Vec::with_capacity(capacity);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        if let Ok(data) = frame.into_data() {
            let Some(new_len) = whole.len().checked_add(data.len()) else {
                return Err(GatewayRefusal::LimitExceeded {});
            };
            if new_len > bound {
                return Err(GatewayRefusal::LimitExceeded {});
            }
            whole.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(whole))
}

/// Reads a whole relay request body while acquiring permits incrementally from
/// `budget` as bytes arrive, so chunked or stalled requests never eagerly
/// allocate or lock the full maximum body size up front.
async fn read_whole_budgeted(
    mut body: Incoming,
    bound: usize,
    budget: &Arc<Semaphore>,
) -> Result<(Bytes, Option<OwnedSemaphorePermit>), GatewayRefusal> {
    let capacity = body
        .size_hint()
        .exact()
        .and_then(|bytes| usize::try_from(bytes).ok())
        .map_or(0, |bytes| bytes.min(bound).min(INITIAL_READ_CAPACITY_CAP));
    let mut whole = Vec::with_capacity(capacity);
    let mut held: Option<OwnedSemaphorePermit> = None;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        if let Ok(data) = frame.into_data() {
            if data.is_empty() {
                continue;
            }
            let Some(new_len) = whole.len().checked_add(data.len()) else {
                return Err(GatewayRefusal::LimitExceeded {});
            };
            if new_len > bound {
                return Err(GatewayRefusal::LimitExceeded {});
            }
            let permits =
                u32::try_from(data.len()).map_err(|_| GatewayRefusal::LimitExceeded {})?;
            let chunk_hold = Arc::clone(budget)
                .try_acquire_many_owned(permits)
                .map_err(|_| GatewayRefusal::Busy {
                    retry_after_ms: 500,
                })?;
            match &mut held {
                Some(existing) => existing.merge(chunk_hold),
                None => held = Some(chunk_hold),
            }
            whole.extend_from_slice(&data);
        }
    }
    Ok((Bytes::from(whole), held))
}

fn body_timeout_reply() -> Reply {
    // Older callers classify by the existing refusal marker and body.
    let mut reply = refusal_reply(&GatewayRefusal::Busy {
        retry_after_ms: 500,
    });
    *reply.status_mut() = StatusCode::REQUEST_TIMEOUT;
    reply
        .headers_mut()
        .insert(http::header::CONNECTION, HeaderValue::from_static("close"));
    reply
}

fn bytes_reply(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Reply {
    let (sender, receiver) = mpsc::channel(1);
    let _ = sender.try_send(Bytes::from(body));
    let mut reply = http::Response::new(FrameBody(receiver));
    *reply.status_mut() = status;
    reply.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    reply
}

/// The refusal envelope: its status, the marker header naming its code and
/// its JSON body.
fn refusal_reply(refusal: &GatewayRefusal) -> Reply {
    let body = call::encode(&RefusalBody {
        gateway_error: refusal.clone(),
    })
    .unwrap_or_default();
    let status =
        StatusCode::from_u16(refusal.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut reply = bytes_reply(status, "application/json", body);
    reply.headers_mut().insert(
        HeaderName::from_static(plakous_gateway_protocol::ERROR_HEADER),
        HeaderValue::from_static(refusal.code()),
    );
    reply
}

/// A response body fed by the relay pump; dropping it tells the pump the
/// caller left.
struct FrameBody(mpsc::Receiver<Bytes>);

impl http_body::Body for FrameBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|bytes| bytes.map(|bytes| Ok(http_body::Frame::data(bytes))))
    }
}

const fn phase_to_wire(phase: Phase) -> WirePhase {
    match phase {
        Phase::Resolution => WirePhase::Resolution,
        Phase::Connect => WirePhase::Connect,
        Phase::Tls => WirePhase::Tls,
        Phase::RequestWrite => WirePhase::RequestWrite,
        Phase::RequestWriteOrResponseHead => WirePhase::RequestWriteOrResponseHead,
        Phase::ResponseHead => WirePhase::ResponseHead,
        Phase::ResponseBody => WirePhase::ResponseBody,
        Phase::Cleanup => WirePhase::Cleanup,
    }
}

const fn transmission_to_wire(transmission: TransmissionFact) -> WireTransmission {
    match transmission {
        TransmissionFact::NotStarted => WireTransmission::NotStarted,
        TransmissionFact::RequestIncomplete => WireTransmission::RequestIncomplete,
        TransmissionFact::MayHaveTransmitted => WireTransmission::MayHaveTransmitted,
    }
}

const fn kind_to_wire(kind: ErrorKind) -> WireFailureKind {
    match kind {
        ErrorKind::Cancelled => WireFailureKind::Cancelled,
        ErrorKind::DeadlineExceeded => WireFailureKind::DeadlineExceeded,
        ErrorKind::LimitExceeded(_) => WireFailureKind::LimitExceeded,
        ErrorKind::Resolution => WireFailureKind::Resolution,
        ErrorKind::AddressRefused => WireFailureKind::AddressRefused,
        ErrorKind::Io => WireFailureKind::Io,
        ErrorKind::Tls => WireFailureKind::Tls,
        ErrorKind::Protocol => WireFailureKind::Protocol,
        ErrorKind::InvalidResponse => WireFailureKind::InvalidResponse,
        ErrorKind::Internal => WireFailureKind::Internal,
    }
}

/// A header map as a wire header list, in iteration order.
fn wire_headers<'a>(
    headers: impl Iterator<Item = (&'a HeaderName, &'a HeaderValue)>,
) -> Option<Vec<WireHeader>> {
    headers
        .map(|(name, value)| WireHeader::new(name.as_str(), value.as_bytes().to_vec()).ok())
        .collect()
}

#[cfg(all(test, feature = "gateway-client", feature = "fault-injection"))]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
