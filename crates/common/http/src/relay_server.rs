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
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use plakous_gateway_protocol::envelope::{self, RelayEnvelope};
use plakous_gateway_protocol::frame::{Data, Frame, Head, MAX_DATA_BYTES, RelayEnd};
use plakous_gateway_protocol::header::RequestHeader;
use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous_gateway_protocol::{
    Endpoint, GatewayAccount, SUPPORTED_VERSIONS, UuidText, VersionsReply, call,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

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
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

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

/// The exact value of each custodied entry, in the envelope's custodied order.
pub struct Injection(Vec<SensitiveBytes>);

impl Injection {
    /// Values for the custodied entries, in order.
    #[must_use]
    pub const fn new(values: Vec<SensitiveBytes>) -> Self {
        Self(values)
    }
}

impl std::fmt::Debug for Injection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Injection")
            .field("values", &self.0.len())
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

    /// Checks one decoded relay request for `principal`: flags and relay
    /// scope for its account, its exact upstream pin, its custodied names
    /// against the credential's injection rule and the credential's fence,
    /// then supplies the exact values of its custodied entries.
    fn inject<'a>(
        &'a self,
        principal: &'a Self::Principal,
        envelope: &'a RelayEnvelope,
    ) -> ServiceFuture<'a, Injection>;

    /// One JSON call (`facts`, `refresh` or `admin`): the decoded request
    /// body's answer, encoded.
    fn call<'a>(
        &'a self,
        principal: &'a Self::Principal,
        endpoint: Endpoint,
        body: &'a [u8],
    ) -> ServiceFuture<'a, Vec<u8>>;

    /// Records how one relay ended. The default records nothing.
    fn audit(&self, _record: RelayRecord) {}
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
    instance: UuidText,
    max_request_body_bytes: usize,
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
                instance,
                max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
                dial_overrides: Vec::new(),
            }),
        }
    }

    /// The same server with another upstream body bound.
    #[must_use]
    pub fn with_max_request_body_bytes(self, bytes: usize) -> Self {
        self.rebuild(|inner| inner.max_request_body_bytes = bytes)
    }

    fn rebuild(self, change: impl FnOnce(&mut ServerInner<S>)) -> Self {
        let inner = &self.inner;
        let mut next = ServerInner {
            service: Arc::clone(&inner.service),
            upstream: inner.upstream.clone(),
            instance: inner.instance.clone(),
            max_request_body_bytes: inner.max_request_body_bytes,
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

    /// Serves one connection until the caller closes it.
    ///
    /// # Errors
    /// Returns the HTTP/1.1 connection failure.
    pub async fn serve<IO>(&self, io: IO, peer: Peer) -> Result<(), ServeError>
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        let service = hyper::service::service_fn(move |request| {
            let inner = Arc::clone(&inner);
            async move { Ok::<_, Infallible>(inner.handle(request, peer).await) }
        });
        hyper::server::conn::http1::Builder::new()
            .keep_alive(true)
            .serve_connection(TokioIo::new(io), service)
            .await
            .map_err(ServeError)
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
            .map(|value| value.as_bytes().to_vec());
        let principal = self.service.authenticate(&peer, key.as_deref())?;
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
            Endpoint::Relay => self.relay(request, principal).await,
            Endpoint::Facts | Endpoint::Refresh | Endpoint::Admin => {
                let body = read_whole(request.into_body(), call::MAX_CALL_BYTES).await?;
                let reply = self.service.call(&principal, endpoint, &body).await?;
                Ok(bytes_reply(StatusCode::OK, "application/json", reply))
            }
        }
    }

    async fn relay(
        self: &Arc<Self>,
        request: http::Request<Incoming>,
        principal: S::Principal,
    ) -> Result<Reply, GatewayRefusal> {
        let content_type = request.headers().get(http::header::CONTENT_TYPE);
        if content_type.map(HeaderValue::as_bytes)
            != Some(plakous_gateway_protocol::RELAY_REQUEST_CONTENT_TYPE.as_bytes())
        {
            return Err(GatewayRefusal::EnvelopeInvalid {});
        }
        // The whole request is read before anything is validated or dialed,
        // so a relay request that did not arrive whole was never sent.
        let bound = self
            .max_request_body_bytes
            .saturating_add(4 + envelope::MAX_ENVELOPE_BYTES);
        let whole = read_whole(request.into_body(), bound).await?;
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
            duration: Duration::ZERO,
            outcome: RelayOutcome::Complete,
        };
        let prepared = match self.prepare(&principal, &envelope, upstream_body).await {
            Ok(prepared) => prepared,
            Err(refusal) => {
                record.outcome = RelayOutcome::Refused(refusal.code());
                record.duration = started.elapsed();
                self.service.audit(record);
                return Err(refusal);
            }
        };
        record.upstream = Some(prepared.audit_target.clone());
        let (sender, receiver) = mpsc::channel(4);
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.pump(prepared, sender, record, started).await;
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
        envelope: &RelayEnvelope,
        body: Bytes,
    ) -> Result<Prepared, GatewayRefusal> {
        let (origin_text, path_and_query) = envelope::split_upstream_url(&envelope.url)
            .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        let method = Method::from_bytes(envelope.method.as_bytes())
            .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        let injection = self.service.inject(principal, envelope).await?;
        let mut values = injection.0.into_iter();
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
    async fn pump(
        &self,
        prepared: Prepared,
        sender: mpsc::Sender<Bytes>,
        mut record: RelayRecord,
        started: Instant,
    ) {
        let Prepared {
            upstream,
            request,
            deadline,
            max_chunk,
            ..
        } = prepared;
        let cancellation = CancellationToken::new();
        let control = ExchangeControl::streaming(deadline, cancellation.clone());
        let outcome = tokio::select! {
            () = sender.closed() => {
                cancellation.cancel();
                None
            }
            result = upstream.execute(request, control) => Some(result),
        };
        record.outcome = match outcome {
            None => RelayOutcome::Abandoned,
            Some(Err(failure)) => {
                let _ = send(&sender, &upstream_failed(failure)).await;
                RelayOutcome::UpstreamFailed(failure.transmission())
            }
            Some(Ok(response)) => {
                record.upstream_status = Some(response.status().as_u16());
                stream(response, max_chunk, &sender, &cancellation, &mut record).await
            }
        };
        record.duration = started.elapsed();
        self.service.audit(record);
    }
}

/// Streams one upstream response as `Head`, `Data` and `End` frames.
async fn stream(
    response: crate::Response,
    max_chunk: usize,
    sender: &mpsc::Sender<Bytes>,
    cancellation: &CancellationToken,
    record: &mut RelayRecord,
) -> RelayOutcome {
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
    let head = Frame::Head(Head {
        status: response.status().as_u16(),
        headers,
    });
    if send(sender, &head).await.is_err() {
        cancellation.cancel();
        return RelayOutcome::Abandoned;
    }
    let mut body = response.into_body();
    let mut chunker = Chunker::new(event_stream, max_chunk);
    let mut seq = 0;
    loop {
        let next = tokio::select! {
            () = sender.closed() => {
                cancellation.cancel();
                return RelayOutcome::Abandoned;
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
                let _ = send(sender, &upstream_failed(failure)).await;
                return RelayOutcome::UpstreamFailed(failure.transmission());
            }
        }
    }
}

struct Prepared {
    upstream: Client,
    request: Request,
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

/// The caller's hard deadline, or an hour from now when it gave none.
fn envelope_deadline(envelope: &RelayEnvelope) -> Instant {
    let fallback = Instant::now() + Duration::from_secs(3600);
    let Some(deadline) = envelope.deadline_unix_ms else {
        return fallback;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    Instant::now() + Duration::from_millis(deadline.saturating_sub(now))
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

/// Reads a whole request body; over `bound` is a refusal.
async fn read_whole(mut body: Incoming, bound: usize) -> Result<Bytes, GatewayRefusal> {
    let mut whole = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| GatewayRefusal::EnvelopeInvalid {})?;
        if let Ok(data) = frame.into_data() {
            if whole.len() + data.len() > bound {
                return Err(GatewayRefusal::LimitExceeded {});
            }
            whole.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(whole))
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
