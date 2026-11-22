//! The credential gateway relay client (spec 01 §7.5, §13.1).
//!
//! A [`Client`] made with [`Client::with_relay`] sends each exchange to a
//! credential gateway as one relay request instead of to the provider. The
//! request's custodied credential fields go out as `[name, null]` at their
//! own position, the gateway fills them and makes the upstream exchange, and
//! its frames come back as an ordinary [`Response`], so every protocol
//! kernel parses relayed bytes with its usual code. How far each hop got is
//! composed into one transmission fact (§13.1): the gateway reads the whole
//! request before it dials upstream, so a relay request that was not sent
//! whole was never sent upstream either.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use plakous_gateway_protocol::envelope::{ExpectedFence, RelayEnvelope, RelayLimits};
use plakous_gateway_protocol::frame::{Frame, FrameDecoder, MAX_DATA_BYTES, RelayEnd};
use plakous_gateway_protocol::header::{RequestHeader, WireHeader};
use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous_gateway_protocol::{
    Endpoint, GatewayAccount, HexDigest, SUPPORTED_VERSIONS, VersionsReply, call, envelope,
};
use tokio::sync::OnceCell;

use crate::{
    Client, ConfigError, ErrorKind, ExchangeControl, Failure, Headers, LimitKind, Limits, Origin,
    Phase, Request, Response, ResponseBody, SensitiveBytes, StatusCode, TransmissionFact,
};
use plakous_gateway_protocol::frame::{WireFailureKind, WirePhase, WireTransmission};

mod frames;

use crate::transport::test_hop::{self, BeforeSend};
use frames::RelayFrames;

/// How many times one relay request is prepared again after the gateway
/// proves it unsent because the credential's generation moved.
pub const MAX_FENCE_REPREPARATIONS: u32 = 2;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a credential gateway listens.
#[derive(Clone)]
pub enum GatewayEndpoint {
    /// A same-user Unix socket.
    Unix(PathBuf),
    /// A TCP address (loopback or tailnet).
    Tcp(SocketAddr),
    /// An in-process gateway over an in-memory duplex.
    InMemory(InMemoryGateway),
}

impl std::fmt::Debug for GatewayEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unix(_) => "GatewayEndpoint::Unix(<path>)",
            Self::Tcp(_) => "GatewayEndpoint::Tcp(<address>)",
            Self::InMemory(_) => "GatewayEndpoint::InMemory",
        })
    }
}

/// An in-process gateway: each connection is one end of a fresh in-memory
/// duplex, and the gateway is handed the other end to serve.
#[derive(Clone)]
pub struct InMemoryGateway {
    accept: Arc<dyn Fn(tokio::io::DuplexStream) + Send + Sync>,
}

impl InMemoryGateway {
    /// Bytes buffered in each direction of one in-memory connection.
    pub const BUFFER_BYTES: usize = 256 * 1024;

    /// A gateway that serves each new connection with `accept`, which must
    /// not block (spawn the connection's server task).
    #[must_use]
    pub fn new(accept: impl Fn(tokio::io::DuplexStream) + Send + Sync + 'static) -> Self {
        Self {
            accept: Arc::new(accept),
        }
    }
}

/// What a gateway hop's connection dials instead of resolving its origin.
struct Dial(GatewayEndpoint);

impl crate::transport::hooks::Dialer for Dial {
    fn dial(
        &self,
    ) -> crate::transport::hooks::HookFuture<
        'static,
        Result<Box<dyn crate::transport::hooks::DialedIo>, Failure>,
    > {
        let endpoint = self.0.clone();
        Box::pin(async move {
            let failed = |error: std::io::Error| {
                Failure::new(Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io)
                    .with_io_cause(&error)
            };
            Ok(match endpoint {
                GatewayEndpoint::Unix(path) => Box::new(
                    tokio::net::UnixStream::connect(path)
                        .await
                        .map_err(failed)?,
                )
                    as Box<dyn crate::transport::hooks::DialedIo>,
                GatewayEndpoint::Tcp(address) => {
                    let stream = tokio::net::TcpStream::connect(address)
                        .await
                        .map_err(failed)?;
                    stream.set_nodelay(true).map_err(failed)?;
                    Box::new(stream)
                }
                GatewayEndpoint::InMemory(gateway) => Box::new(gateway.open()),
            })
        })
    }
}

impl Client {
    /// Returns this client relaying every exchange through a credential
    /// gateway (spec 01 §13.1), keeping its limits, policy, identity and
    /// observer. Custodied credential fields ([`custodied_placeholder`]) go
    /// out as `[name, null]` for the gateway to fill; a request without one
    /// is refused unsent.
    #[must_use]
    pub fn with_relay(&self, relay: Arc<RelayClient>) -> Self {
        self.with_relay_hook(relay)
    }
}

impl crate::RouteTransport {
    /// Through the credential gateway `relay` serves.
    #[must_use]
    pub fn relayed(relay: Arc<RelayClient>) -> Self {
        Self(Some(relay))
    }
}

impl crate::transport::hooks::Relay for RelayClient {
    fn execute<'a>(
        &'a self,
        limits: &'a Limits,
        request: Request,
        control: ExchangeControl,
        faults: test_hop::HopFaults,
    ) -> crate::transport::hooks::HookFuture<'a, Result<Response, Failure>> {
        Box::pin(self.relay(limits, request, control, faults))
    }
}

/// The placeholder bytes for `account` at the fence (`account_binding` as
/// 64 lowercase hex digits, `generation`). Pass them to
/// [`crate::sensitive_header_value`] at the credential's header position.
#[must_use]
pub fn custodied_placeholder(account: &str, account_binding: &str, generation: u64) -> Vec<u8> {
    let mut value = crate::custodied::PREFIX.to_vec();
    value.extend_from_slice(account.as_bytes());
    value.push(b' ');
    value.extend_from_slice(account_binding.as_bytes());
    value.push(b' ');
    value.extend_from_slice(generation.to_string().as_bytes());
    value
}

/// The account and fence one placeholder names.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Placeholder {
    account: String,
    account_binding: String,
    generation: u64,
}

impl Placeholder {
    fn parse(value: &[u8]) -> Option<Self> {
        let rest = std::str::from_utf8(value.strip_prefix(crate::custodied::PREFIX)?).ok()?;
        let mut parts = rest.split(' ');
        let account = parts.next()?.to_owned();
        let account_binding = parts.next()?.to_owned();
        let generation = parts.next()?.parse().ok()?;
        if parts.next().is_some() || account.is_empty() || account_binding.len() != 64 {
            return None;
        }
        Some(Self {
            account,
            account_binding,
            generation,
        })
    }
}

impl InMemoryGateway {
    /// Opens one in-memory connection and hands the gateway its far end.
    pub(crate) fn open(&self) -> tokio::io::DuplexStream {
        let (ours, theirs) = tokio::io::duplex(Self::BUFFER_BYTES);
        (self.accept)(theirs);
        ours
    }
}

/// Why a facts, refresh, admin or versions call failed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RelayCallError {
    /// The gateway refused the call.
    #[error("credential gateway refused the call: {}", .0.code())]
    Refused(GatewayRefusal),
    /// The gateway speaks no version this client speaks.
    #[error("credential gateway protocol version mismatch")]
    VersionMismatch {
        /// The versions the gateway serves.
        supported: Vec<u16>,
    },
    /// The gateway could not be reached or the call broke.
    #[error("credential gateway call failed: {0}")]
    Transport(Failure),
    /// The gateway's answer was malformed.
    #[error("credential gateway answered malformed bytes")]
    InvalidReply,
}

/// A client of one credential gateway: its endpoint, the caller key and the
/// negotiated protocol version.
pub struct RelayClient {
    hop: Client,
    key: Option<HeaderValue>,
    version: OnceCell<NegotiatedVersion>,
}

struct NegotiatedVersion {
    selected: u16,
    served: Vec<u16>,
}

impl std::fmt::Debug for RelayClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayClient")
            .field("keyed", &self.key.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, feature = "gateway-server", feature = "fault-injection"))]
mod test_support {
    use super::{NegotiatedVersion, OnceCell, RelayClient, SUPPORTED_VERSIONS};

    impl RelayClient {
        pub(crate) fn with_test_version(mut self, version: u16) -> Self {
            self.version = OnceCell::new_with(Some(NegotiatedVersion {
                selected: version,
                served: SUPPORTED_VERSIONS.to_vec(),
            }));
            self
        }
    }
}

impl RelayClient {
    /// A client of the gateway at `endpoint`, presenting `key` (a caller key,
    /// `plakgw1.<id>.<secret>`) when one is given. A same-user Unix socket
    /// peer without a key is the gateway's local administrator.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when the key is not a header value.
    pub fn new(
        endpoint: GatewayEndpoint,
        key: Option<SensitiveBytes>,
    ) -> Result<Self, ConfigError> {
        let key = key
            .map(|key| {
                let mut value = HeaderValue::from_maybe_shared(key.into_bytes())
                    .map_err(|_| ConfigError::InvalidCallerKey)?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        let hop = Client::new_http1(hop_limits(&Limits::default()))?;
        Ok(Self {
            // A carried version is not a live connection to that instance.
            // Never dispatch a new call into a gateway socket that may be
            // closing; a missing reply cannot prove the gateway did not act.
            hop: hop.with_dialer(Arc::new(Dial(endpoint))),
            key,
            version: OnceCell::new(),
        })
    }

    /// The protocol version both ends speak, negotiated once per client.
    ///
    /// # Errors
    /// Returns the `GET /versions` failure or [`RelayCallError::VersionMismatch`].
    pub async fn version(&self, control: &ExchangeControl) -> Result<u16, RelayCallError> {
        Ok(self.negotiated_version(control).await?.selected)
    }

    /// Requires a version new enough for the operation's binding inputs.
    ///
    /// # Errors
    /// Returns discovery failure or a mismatch naming all advertised versions.
    pub async fn require_version(
        &self,
        minimum: u16,
        control: &ExchangeControl,
    ) -> Result<(), RelayCallError> {
        let version = self.negotiated_version(control).await?;
        if version.selected < minimum {
            return Err(RelayCallError::VersionMismatch {
                supported: version.served.clone(),
            });
        }
        Ok(())
    }

    async fn negotiated_version(
        &self,
        control: &ExchangeControl,
    ) -> Result<&NegotiatedVersion, RelayCallError> {
        self.version
            .get_or_try_init(|| self.discover_version(control))
            .await
    }

    async fn discover_version(
        &self,
        control: &ExchangeControl,
    ) -> Result<NegotiatedVersion, RelayCallError> {
        let reply: VersionsReply = call::decode(
            &self
                .send(
                    Method::GET,
                    plakous_gateway_protocol::VERSIONS_PATH,
                    None,
                    Vec::new(),
                    control,
                )
                .await?,
        )
        .map_err(|_| RelayCallError::InvalidReply)?;
        let Some(selected) =
            plakous_gateway_protocol::negotiate(SUPPORTED_VERSIONS, &reply.versions)
        else {
            return Err(RelayCallError::VersionMismatch {
                supported: reply.versions,
            });
        };
        Ok(NegotiatedVersion {
            selected,
            served: reply.versions,
        })
    }

    /// One JSON call (`facts`, `refresh` or `admin`) at the negotiated version.
    ///
    /// # Errors
    /// Returns the gateway's refusal, a transport failure or a malformed reply.
    pub async fn call(
        &self,
        endpoint: Endpoint,
        body: Vec<u8>,
        control: &ExchangeControl,
    ) -> Result<Vec<u8>, RelayCallError> {
        let version = self.version(control).await?;
        self.send(
            Method::POST,
            &endpoint.path(version),
            Some("application/json"),
            body,
            control,
        )
        .await
    }

    /// A control for one call with the default call timeout.
    #[must_use]
    pub fn call_control() -> ExchangeControl {
        ExchangeControl::new(Instant::now() + CALL_TIMEOUT)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&'static str>,
        body: Vec<u8>,
        control: &ExchangeControl,
    ) -> Result<Vec<u8>, RelayCallError> {
        let request = Request::new(
            Origin::gateway(),
            method,
            path,
            self.headers(content_type),
            Bytes::from(body),
            self.hop.limits(),
        )
        .map_err(|_| RelayCallError::InvalidReply)?;
        let response = self
            .hop
            .execute(request, control.clone())
            .await
            .map_err(RelayCallError::Transport)?;
        let status = response.status();
        let refused = refusal_code(response.headers());
        let body = read_all(response.into_body(), call::MAX_CALL_BYTES)
            .await
            .map_err(RelayCallError::Transport)?;
        if let Some(code) = refused {
            return Err(RelayCallError::Refused(decode_refusal(
                &code,
                &body.unwrap_or_default(),
            )));
        }
        if status != StatusCode::OK {
            return Err(RelayCallError::InvalidReply);
        }
        body.ok_or(RelayCallError::InvalidReply)
    }

    fn headers(&self, content_type: Option<&'static str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static(content_type),
            );
        }
        if let Some(key) = &self.key {
            headers.insert(
                HeaderName::from_static(plakous_gateway_protocol::KEY_HEADER),
                key.clone(),
            );
        }
        headers
    }

    /// Relays one exchange; see the module documentation. Each relay
    /// request takes the gateway-hop fault `faults` selects for the logical
    /// upstream, through the same hop mapping as a real hop event.
    async fn relay(
        &self,
        limits: &Limits,
        request: Request,
        control: ExchangeControl,
        faults: test_hop::HopFaults,
    ) -> Result<Response, Failure> {
        let (origin, method, path_and_query, headers, body) = request.into_parts();
        if origin.is_loopback_http() {
            return Err(unsent(ErrorKind::Internal, "relay_plain_origin"));
        }
        let prepared = Prepared::new(&headers)?;
        let url = format!("https://{}{}", origin.binding_authority(), path_and_query);
        let operation = crate::gateway_wire::random_uuid()
            .ok_or_else(|| unsent(ErrorKind::Internal, "relay_random"))?;
        let version = self
            .version(&control)
            .await
            .map_err(|error| call_failure(&error))?;
        let hop = self
            .hop
            .with_limits(hop_limits(limits))
            .map_err(|_| unsent(ErrorKind::Internal, "relay_limits"))?;
        let mut expected = prepared.expected.clone();
        let mut attempt = 0;
        loop {
            let envelope = RelayEnvelope {
                operation: operation.clone(),
                attempt,
                account: prepared.account.clone(),
                expected: expected.clone(),
                method: method.as_str().to_owned(),
                url: url.clone(),
                headers: prepared.headers.clone(),
                body_len: u64::try_from(body.len()).unwrap_or(u64::MAX),
                limits: relay_limits(limits),
                deadline_unix_ms: deadline_unix_ms(control.deadline()),
            };
            let encoded = envelope::encode_request(&envelope, &body)
                .map_err(|_| unsent(ErrorKind::Internal, "relay_envelope"))?;
            let hop_request = Request::new(
                Origin::gateway(),
                Method::POST,
                &Endpoint::Relay.path(version),
                self.headers(Some(plakous_gateway_protocol::RELAY_REQUEST_CONTENT_TYPE)),
                SensitiveBytes::new(encoded).into_bytes(),
                hop.limits(),
            )
            .map_err(|_| {
                unsent(
                    ErrorKind::LimitExceeded(LimitKind::RequestBodyBytes),
                    "relay_request",
                )
            })?;
            let fault = test_hop::select(
                &faults,
                origin.host(),
                path_and_query.as_str(),
                &control,
                hop.limits(),
            )
            .await?;
            match exchange_hop(&hop, hop_request, limits, &control, fault).await? {
                Answer::Relayed(response) => return Ok(*response),
                Answer::Refused(GatewayRefusal::FenceMoved { facts })
                    if attempt < MAX_FENCE_REPREPARATIONS =>
                {
                    let Some(moved) = facts.expected_fence().filter(|moved| {
                        moved.account_binding == expected.account_binding
                            && moved.generation > expected.generation
                    }) else {
                        return Err(refusal_failure(&GatewayRefusal::FenceMoved { facts }));
                    };
                    expected = moved;
                    attempt += 1;
                }
                Answer::Refused(refusal) => return Err(refusal_failure(&refusal)),
            }
        }
    }
}

/// The relay form of one prepared request's headers and its custody.
struct Prepared {
    account: GatewayAccount,
    expected: ExpectedFence,
    headers: Vec<RequestHeader>,
}

impl Prepared {
    fn new(headers: &Headers) -> Result<Self, Failure> {
        let invalid = || unsent(ErrorKind::Internal, "relay_custody");
        let mut custody: Option<Placeholder> = None;
        let mut entries = Vec::with_capacity(headers.len());
        for (name, value) in headers.iter() {
            if crate::custodied::is_placeholder(value.as_bytes()) {
                let placeholder = Placeholder::parse(value.as_bytes()).ok_or_else(invalid)?;
                if custody.as_ref().is_some_and(|known| *known != placeholder) {
                    return Err(invalid());
                }
                custody = Some(placeholder);
                entries.push(RequestHeader::custodied(name.as_str()).map_err(|_| invalid())?);
            } else {
                entries.push(RequestHeader::Value(
                    WireHeader::new(name.as_str(), value.as_bytes().to_vec())
                        .map_err(|_| invalid())?,
                ));
            }
        }
        let custody =
            custody.ok_or_else(|| unsent(ErrorKind::Internal, "relay_without_custody"))?;
        Ok(Self {
            account: GatewayAccount::new(custody.account).map_err(|_| invalid())?,
            expected: ExpectedFence {
                account_binding: HexDigest::new(custody.account_binding).map_err(|_| invalid())?,
                generation: custody.generation,
            },
            headers: entries,
        })
    }
}

enum Answer {
    Relayed(Box<Response>),
    Refused(GatewayRefusal),
}

/// Sends one relay request on `hop` and reads the gateway's answer, with
/// the gateway-hop fault `fault` in place of, or on top of, the real event.
async fn exchange_hop(
    hop: &Client,
    request: Request,
    limits: &Limits,
    control: &ExchangeControl,
    mut fault: test_hop::Selection,
) -> Result<Answer, Failure> {
    let response = match test_hop::before_send(&mut fault) {
        BeforeSend::Fail(failure) => return Err(hop_failure(failure)),
        BeforeSend::Answer(answer) => *answer,
        BeforeSend::Send => hop
            .execute(request, control.clone())
            .await
            .map_err(hop_failure)?,
    };
    if test_hop::lost_before_head(&fault) {
        let _ = response.into_body().close().await;
        return Err(lost(ErrorKind::Io).with_cause("gateway_lost"));
    }
    Ok(
        match answer(response, limits, control, test_hop::cut_after_head(&fault)).await? {
            Answer::Relayed(response) => {
                Answer::Relayed(Box::new(test_hop::relayed(fault, *response)))
            }
            refused @ Answer::Refused(_) => refused,
        },
    )
}

/// Reads the gateway's answer to one relay request: frames up to the
/// upstream head, or its refusal. `cut_after_head` drops the hop right after
/// the head (a gateway-hop fault).
async fn answer(
    response: Response,
    limits: &Limits,
    control: &ExchangeControl,
    cut_after_head: bool,
) -> Result<Answer, Failure> {
    if let Some(code) = refusal_code(response.headers()) {
        let body = read_all(response.into_body(), call::MAX_CALL_BYTES)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        return Ok(Answer::Refused(decode_refusal(&code, &body)));
    }
    let relayed_frames = response.status() == StatusCode::OK
        && response
            .headers()
            .get(&http::header::CONTENT_TYPE)
            .is_some_and(|value| {
                value.as_bytes() == plakous_gateway_protocol::RELAY_FRAMES_CONTENT_TYPE.as_bytes()
            });
    if !relayed_frames {
        let _ = response.into_body().close().await;
        return Err(lost(ErrorKind::InvalidResponse).with_cause("gateway_unmarked"));
    }
    let mut frames = RelayFrames::new(
        response.into_body(),
        FrameDecoder::new(u64::try_from(limits.max_response_body_bytes).unwrap_or(u64::MAX)),
    );
    let head_deadline = (Instant::now() + limits.response_head_timeout).min(control.deadline());
    let first = tokio::time::timeout_at(
        tokio::time::Instant::from_std(head_deadline),
        frames.first(),
    )
    .await
    .map_err(|_| lost(ErrorKind::DeadlineExceeded))??;
    match first {
        Frame::Head(head) => {
            if cut_after_head {
                frames.cut();
            }
            let status =
                StatusCode::from_u16(head.status).map_err(|_| lost(ErrorKind::InvalidResponse))?;
            let headers =
                header_map(&head.headers).ok_or_else(|| lost(ErrorKind::InvalidResponse))?;
            crate::transport::hooks::hooked_response(
                status,
                headers,
                Box::new(frames),
                control.clone(),
                limits,
            )
            .map(|response| Answer::Relayed(Box::new(response)))
        }
        Frame::End(RelayEnd::UpstreamFailed {
            phase,
            transmission,
            kind,
            timeout,
        }) => Err(Failure::new(
            phase_from_wire(phase),
            transmission_from_wire(transmission),
            if timeout {
                ErrorKind::DeadlineExceeded
            } else {
                kind_from_wire(kind)
            },
        )
        .with_cause("gateway_upstream")),
        Frame::End(_) | Frame::Data(_) | Frame::Trailers(_) => {
            Err(lost(ErrorKind::Protocol).with_cause("gateway_ended"))
        }
    }
}

fn refusal_code(headers: &Headers) -> Option<String> {
    headers
        .get(&HeaderName::from_static(
            plakous_gateway_protocol::ERROR_HEADER,
        ))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The refusal a marked answer's body names, or the closest one its code
/// names when the body is unreadable.
fn decode_refusal(code: &str, body: &[u8]) -> GatewayRefusal {
    if let Ok(decoded) = call::decode::<RefusalBody>(body)
        && decoded.gateway_error.code() == code
    {
        return decoded.gateway_error;
    }
    match code {
        "busy" => GatewayRefusal::Busy { retry_after_ms: 0 },
        "unauthenticated" => GatewayRefusal::Unauthenticated {},
        _ => GatewayRefusal::Internal {},
    }
}

/// Reads a whole bounded body; `None` when it is longer than `limit`.
async fn read_all(mut body: ResponseBody, limit: usize) -> Result<Option<Vec<u8>>, Failure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > limit {
            let _ = body.close().await;
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

/// The bounds of a gateway hop that carries one exchange under `limits`:
/// room for the envelope and frame overhead, and a body wait that covers
/// the upstream head wait as well, since relay silence is upstream silence.
fn hop_limits(limits: &Limits) -> Limits {
    let overhead = 2 * 1024 * 1024;
    Limits {
        max_request_headers: 8,
        max_request_header_bytes: 16 * 1024,
        max_request_body_bytes: limits
            .max_request_body_bytes
            .saturating_add(4 + envelope::MAX_ENVELOPE_BYTES),
        max_response_headers: 32,
        max_response_header_bytes: 64 * 1024,
        max_response_chunk_bytes: MAX_DATA_BYTES + overhead,
        max_response_body_bytes: limits
            .max_response_body_bytes
            .saturating_add(limits.max_response_body_bytes / 8)
            .saturating_add(overhead),
        response_body_timeout: limits
            .response_body_timeout
            .max(limits.response_head_timeout)
            .saturating_add(Duration::from_secs(1)),
        ..limits.clone()
    }
}

fn relay_limits(limits: &Limits) -> RelayLimits {
    let millis = |duration: Duration| {
        u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    };
    RelayLimits {
        head_timeout_ms: millis(limits.response_head_timeout),
        body_idle_timeout_ms: millis(limits.response_body_timeout),
        max_response_bytes: u64::try_from(limits.max_response_body_bytes).unwrap_or(u64::MAX),
        max_chunk_bytes: u32::try_from(limits.max_response_chunk_bytes.min(MAX_DATA_BYTES))
            .unwrap_or(1)
            .max(1),
    }
}

fn deadline_unix_ms(deadline: Instant) -> Option<u64> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let at = SystemTime::now().checked_add(remaining)?;
    u64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

fn unsent(kind: ErrorKind, cause: &'static str) -> Failure {
    Failure::new(Phase::RequestWrite, TransmissionFact::NotStarted, kind).with_cause(cause)
}

fn lost(kind: ErrorKind) -> Failure {
    Failure::new(
        Phase::ResponseHead,
        TransmissionFact::MayHaveTransmitted,
        kind,
    )
}

/// A gateway hop failure as the upstream fact. The gateway dials upstream
/// only after reading the whole relay request, so a hop request the gateway
/// cannot hold whole was not sent upstream.
fn hop_failure(failure: Failure) -> Failure {
    if !failure.transmission().peer_cannot_have_acted() {
        return failure;
    }
    let unsent = Failure::new(
        failure.phase(),
        TransmissionFact::NotStarted,
        failure.kind(),
    );
    match failure.cause() {
        Some(cause) => unsent.with_cause(cause),
        None => unsent,
    }
}

/// A call failure before any relay request was sent.
fn call_failure(error: &RelayCallError) -> Failure {
    match error {
        RelayCallError::Refused(refusal) => refusal_failure(refusal),
        RelayCallError::VersionMismatch { .. } => {
            unsent(ErrorKind::InvalidResponse, "gateway_version_mismatch")
        }
        RelayCallError::Transport(failure) => Failure::new(
            failure.phase(),
            TransmissionFact::NotStarted,
            failure.kind(),
        )
        .with_cause("gateway_versions"),
        RelayCallError::InvalidReply => unsent(ErrorKind::InvalidResponse, "gateway_versions"),
    }
}

/// A gateway refusal as an unsent failure. `busy` is an unreachable
/// upstream for the retry budget; every other refusal keeps its own class
/// and is not a transport fault a turn replays.
fn refusal_failure(refusal: &GatewayRefusal) -> Failure {
    if let GatewayRefusal::Busy { .. } = refusal {
        return Failure::new(Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io)
            .with_cause("gateway_busy");
    }
    unsent(ErrorKind::InvalidResponse, refusal_cause(refusal))
}

const fn refusal_cause(refusal: &GatewayRefusal) -> &'static str {
    match refusal {
        GatewayRefusal::Unauthenticated {} => "gateway_unauthenticated",
        GatewayRefusal::ScopeDenied {} => "gateway_scope_denied",
        GatewayRefusal::VersionUnsupported { .. } => "gateway_version_unsupported",
        GatewayRefusal::GatewayDisabled {} => "gateway_disabled",
        GatewayRefusal::AccountDisabled {} => "gateway_account_disabled",
        GatewayRefusal::AccountUnknown {} => "gateway_account_unknown",
        GatewayRefusal::CredentialNotReady { .. } => "gateway_credential_not_ready",
        GatewayRefusal::FenceMoved { .. } => "gateway_fence_moved",
        GatewayRefusal::AccountChanged {} => "gateway_account_changed",
        GatewayRefusal::UpstreamNotPinned {} => "gateway_upstream_not_pinned",
        GatewayRefusal::ReservedHeader {} => "gateway_reserved_header",
        GatewayRefusal::CustodiedHeadersMismatch {} => "gateway_custodied_headers_mismatch",
        GatewayRefusal::EnvelopeInvalid {} => "gateway_envelope_invalid",
        GatewayRefusal::LimitExceeded {} => "gateway_limit_exceeded",
        GatewayRefusal::Busy { .. } => "gateway_busy",
        GatewayRefusal::Internal {} => "gateway_internal",
    }
}

pub(crate) const fn phase_from_wire(phase: WirePhase) -> Phase {
    match phase {
        WirePhase::Resolution => Phase::Resolution,
        WirePhase::Connect => Phase::Connect,
        WirePhase::Tls => Phase::Tls,
        WirePhase::RequestWrite => Phase::RequestWrite,
        WirePhase::RequestWriteOrResponseHead => Phase::RequestWriteOrResponseHead,
        WirePhase::ResponseHead => Phase::ResponseHead,
        WirePhase::ResponseBody => Phase::ResponseBody,
        WirePhase::Cleanup => Phase::Cleanup,
    }
}

pub(crate) const fn transmission_from_wire(transmission: WireTransmission) -> TransmissionFact {
    match transmission {
        WireTransmission::NotStarted => TransmissionFact::NotStarted,
        WireTransmission::RequestIncomplete => TransmissionFact::RequestIncomplete,
        WireTransmission::MayHaveTransmitted => TransmissionFact::MayHaveTransmitted,
    }
}

pub(crate) const fn kind_from_wire(kind: WireFailureKind) -> ErrorKind {
    match kind {
        WireFailureKind::Cancelled => ErrorKind::Cancelled,
        WireFailureKind::DeadlineExceeded => ErrorKind::DeadlineExceeded,
        WireFailureKind::LimitExceeded => ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes),
        WireFailureKind::Resolution => ErrorKind::Resolution,
        WireFailureKind::AddressRefused => ErrorKind::AddressRefused,
        WireFailureKind::Io => ErrorKind::Io,
        WireFailureKind::Tls => ErrorKind::Tls,
        WireFailureKind::Protocol => ErrorKind::Protocol,
        WireFailureKind::InvalidResponse => ErrorKind::InvalidResponse,
        WireFailureKind::Internal => ErrorKind::Internal,
    }
}

/// A wire header list as a header map, in order. `None` when a name or
/// value is not a valid HTTP field.
pub(crate) fn header_map(headers: &[WireHeader]) -> Option<HeaderMap> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for header in headers {
        let name = HeaderName::from_bytes(header.name().as_bytes()).ok()?;
        let value = HeaderValue::from_bytes(header.value()).ok()?;
        map.append(name, value);
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_round_trip_and_are_recognized() {
        let binding = "ab".repeat(32);
        let value = custodied_placeholder("codex/work", &binding, 7);
        assert!(crate::custodied::is_placeholder(&value));
        assert!(HeaderValue::from_bytes(&value).is_ok());
        assert_eq!(
            Placeholder::parse(&value),
            Some(Placeholder {
                account: "codex/work".into(),
                account_binding: binding,
                generation: 7,
            })
        );
        assert!(!crate::custodied::is_placeholder(b"Bearer token"));
        assert_eq!(Placeholder::parse(b"\xffplak-custodied a b"), None);
    }
}
