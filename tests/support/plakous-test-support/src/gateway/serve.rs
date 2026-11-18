//! One HTTP/1.1 exchange. Framing bytes come from the protocol crate.

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use plakous_gateway_protocol::admin::{AdminRefusal, AdminRequest, AdminResponse};
use plakous_gateway_protocol::call::{self, MAX_CALL_BYTES};
use plakous_gateway_protocol::envelope;
use plakous_gateway_protocol::facts::{FactsReply, FactsRequest, RefreshReply, RefreshRequest};
use plakous_gateway_protocol::frame::{Data, Frame, Head, RelayEnd, Trailers};
use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous_gateway_protocol::{
    ERROR_HEADER, Endpoint, KEY_HEADER, RELAY_FRAMES_CONTENT_TYPE, SUPPORTED_VERSIONS,
    VERSIONS_PATH, VersionsReply,
};

use super::{FixtureError, FixtureGateway, GatewayScript, UpstreamReply, dial};

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;

/// What [`serve_one`] did. `dialed` is this exchange only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServeReport {
    /// An upstream request was recorded.
    pub dialed: bool,
    /// The caller went away after the request was in hand.
    pub caller_cancelled: bool,
}

/// Serves one request on `stream`.
///
/// The whole body is read before a scripted upstream dial. An incomplete
/// body returns [`FixtureError::Incomplete`] and leaves the dial counter
/// unchanged.
///
/// # Errors
/// Returns [`FixtureError`] when the request is incomplete, not HTTP, or a
/// scripted credential or frame cannot be built. A gateway refusal is a
/// normal response, not an error.
pub async fn serve_one<S>(
    mut stream: S,
    gateway: &FixtureGateway,
) -> Result<ServeReport, FixtureError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let incoming = read_request(&mut stream).await?;
    if incoming.incomplete {
        return Err(FixtureError::Incomplete);
    }
    dispatch(&mut stream, gateway, incoming).await
}

struct Incoming {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    incomplete: bool,
}

async fn dispatch<S>(
    stream: &mut S,
    gateway: &FixtureGateway,
    incoming: Incoming,
) -> Result<ServeReport, FixtureError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Some(refusal) = authenticate(gateway, &incoming.headers) {
        write_refusal(stream, refusal).await?;
        return Ok(ServeReport {
            dialed: false,
            caller_cancelled: false,
        });
    }
    if incoming.method == "GET" && incoming.path == VERSIONS_PATH {
        return versions(stream, gateway).await;
    }
    let Some((version, endpoint)) = Endpoint::parse(&incoming.path) else {
        write_refusal(stream, GatewayRefusal::EnvelopeInvalid {}).await?;
        return Ok(undialed());
    };
    if !SUPPORTED_VERSIONS.contains(&version) {
        write_refusal(
            stream,
            GatewayRefusal::VersionUnsupported {
                supported: SUPPORTED_VERSIONS.to_vec(),
            },
        )
        .await?;
        return Ok(undialed());
    }
    if incoming.method != "POST" {
        write_refusal(stream, GatewayRefusal::EnvelopeInvalid {}).await?;
        return Ok(undialed());
    }
    let action = pop_action(gateway);
    match action {
        GatewayScript::Refuse(refusal) => {
            write_refusal(stream, refusal).await?;
            Ok(undialed())
        }
        GatewayScript::DropBeforeResponse => Ok(undialed()),
        GatewayScript::Stall(duration) => {
            if caller_left(stream, duration).await? {
                return Ok(ServeReport {
                    dialed: false,
                    caller_cancelled: true,
                });
            }
            serve_endpoint(stream, gateway, endpoint, &incoming.body, None).await
        }
        GatewayScript::DropAfterHead => {
            serve_endpoint(
                stream,
                gateway,
                endpoint,
                &incoming.body,
                Some(Cut::AfterHead),
            )
            .await
        }
        GatewayScript::Relay(reply) => {
            serve_endpoint(
                stream,
                gateway,
                endpoint,
                &incoming.body,
                Some(Cut::None(reply)),
            )
            .await
        }
    }
}

enum Cut {
    None(UpstreamReply),
    AfterHead,
}

async fn serve_endpoint<S>(
    stream: &mut S,
    gateway: &FixtureGateway,
    endpoint: Endpoint,
    body: &[u8],
    relay: Option<Cut>,
) -> Result<ServeReport, FixtureError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match endpoint {
        Endpoint::Relay => serve_relay(stream, gateway, body, relay).await,
        Endpoint::Facts => {
            let _ = relay;
            serve_facts(stream, gateway, body).await
        }
        Endpoint::Refresh => {
            let _ = relay;
            serve_refresh(stream, gateway, body).await
        }
        Endpoint::Admin => {
            let _ = relay;
            serve_admin(stream, body).await
        }
    }
}

async fn serve_relay<S>(
    stream: &mut S,
    gateway: &FixtureGateway,
    body: &[u8],
    relay: Option<Cut>,
) -> Result<ServeReport, FixtureError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (envelope, upstream_body) = match envelope::decode_request(body) {
        Ok(decoded) => decoded,
        Err(_) => {
            write_refusal(stream, GatewayRefusal::EnvelopeInvalid {}).await?;
            return Ok(undialed());
        }
    };
    let ordinal = next_relay(gateway);
    if let Some(facts) = take_fence(gateway, ordinal) {
        write_refusal(
            stream,
            GatewayRefusal::FenceMoved {
                facts: Box::new(facts),
            },
        )
        .await?;
        return Ok(undialed());
    }
    let before = gateway.dials();
    if let Err(error) = dial(gateway, &envelope, upstream_body) {
        if matches!(error, FixtureError::MissingCredential) {
            write_refusal(stream, GatewayRefusal::CustodiedHeadersMismatch {}).await?;
            return Ok(undialed());
        }
        return Err(error);
    }
    let after_head = matches!(relay, Some(Cut::AfterHead));
    let reply = match relay {
        Some(Cut::None(reply)) => reply,
        _ => UpstreamReply::default(),
    };
    let frames = encode_frames(&reply, after_head)?;
    if write_bytes(stream, &frames).await.is_err() {
        return Ok(ServeReport {
            dialed: gateway.dials() > before,
            caller_cancelled: true,
        });
    }
    if after_head {
        return Ok(ServeReport {
            dialed: true,
            caller_cancelled: false,
        });
    }
    let _ = stream.shutdown().await;
    Ok(ServeReport {
        dialed: true,
        caller_cancelled: false,
    })
}

fn next_relay(gateway: &FixtureGateway) -> u64 {
    gateway
        .inner
        .relay_ordinal
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1
}

fn take_fence(
    gateway: &FixtureGateway,
    ordinal: u64,
) -> Option<plakous_gateway_protocol::facts::AccountFacts> {
    let fence = gateway.lock(&gateway.inner.fence);
    match fence.as_ref() {
        Some((nth, facts)) if *nth == ordinal => Some(facts.clone()),
        _ => None,
    }
}

async fn serve_facts<S>(
    stream: &mut S,
    gateway: &FixtureGateway,
    body: &[u8],
) -> Result<ServeReport, FixtureError>
where
    S: AsyncWrite + Unpin,
{
    let request: FactsRequest = match call::decode(body) {
        Ok(request) => request,
        Err(_) => {
            write_refusal(stream, GatewayRefusal::EnvelopeInvalid {}).await?;
            return Ok(undialed());
        }
    };
    let Some(facts) = gateway.lock(&gateway.inner.facts).clone() else {
        write_refusal(stream, GatewayRefusal::AccountUnknown {}).await?;
        return Ok(undialed());
    };
    if request
        .accounts
        .iter()
        .any(|account| account != &facts.account)
    {
        write_refusal(stream, GatewayRefusal::AccountUnknown {}).await?;
        return Ok(undialed());
    }
    let reply = FactsReply {
        facts: vec![facts; request.accounts.len()],
    };
    write_json(stream, &reply).await?;
    Ok(undialed())
}

async fn serve_refresh<S>(
    stream: &mut S,
    gateway: &FixtureGateway,
    body: &[u8],
) -> Result<ServeReport, FixtureError>
where
    S: AsyncWrite + Unpin,
{
    if call::decode::<RefreshRequest>(body).is_err() {
        write_refusal(stream, GatewayRefusal::EnvelopeInvalid {}).await?;
        return Ok(undialed());
    }
    let Some(facts) = gateway.lock(&gateway.inner.facts).clone() else {
        write_refusal(stream, GatewayRefusal::AccountUnknown {}).await?;
        return Ok(undialed());
    };
    write_json(stream, &RefreshReply { facts }).await?;
    Ok(undialed())
}

async fn serve_admin<S>(stream: &mut S, body: &[u8]) -> Result<ServeReport, FixtureError>
where
    S: AsyncWrite + Unpin,
{
    let request: AdminRequest = match call::decode(body) {
        Ok(request) => request,
        Err(_) => {
            write_refusal(stream, GatewayRefusal::EnvelopeInvalid {}).await?;
            return Ok(undialed());
        }
    };
    let response = match request {
        AdminRequest::List {} => AdminResponse::Accounts {
            accounts: Vec::new(),
        },
        _ => AdminResponse::Refused {
            refusal: AdminRefusal::LocalAdminRequired {},
        },
    };
    write_json(stream, &response).await?;
    Ok(undialed())
}

async fn versions<S>(stream: &mut S, gateway: &FixtureGateway) -> Result<ServeReport, FixtureError>
where
    S: AsyncWrite + Unpin,
{
    let reply = VersionsReply {
        versions: SUPPORTED_VERSIONS.to_vec(),
        instance: gateway.instance(),
    };
    write_json(stream, &reply).await?;
    Ok(undialed())
}

fn pop_action(gateway: &FixtureGateway) -> GatewayScript {
    gateway
        .lock(&gateway.inner.script)
        .pop_front()
        .unwrap_or(GatewayScript::Relay(UpstreamReply::default()))
}

fn authenticate(gateway: &FixtureGateway, headers: &[(String, String)]) -> Option<GatewayRefusal> {
    let Some(key) = gateway.inner.key.as_deref() else {
        return None;
    };
    let presented = headers
        .iter()
        .find(|(name, _)| name == KEY_HEADER)
        .map(|(_, value)| value.as_str());
    if presented == Some(key) {
        None
    } else {
        Some(GatewayRefusal::Unauthenticated {})
    }
}

fn undialed() -> ServeReport {
    ServeReport {
        dialed: false,
        caller_cancelled: false,
    }
}

async fn caller_left<S>(stream: &mut S, duration: std::time::Duration) -> Result<bool, FixtureError>
where
    S: AsyncRead + Unpin,
{
    let mut byte = [0_u8; 1];
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(false),
        read = stream.read(&mut byte) => match read {
            Ok(0) | Err(_) => Ok(true),
            Ok(_) => Ok(false),
        },
    }
}

fn encode_frames(reply: &UpstreamReply, head_only: bool) -> Result<Vec<u8>, FixtureError> {
    let mut frames = Vec::new();
    frames.extend(
        Frame::Head(Head {
            status: reply.status,
            headers: reply.headers.clone(),
        })
        .encode()
        .map_err(|_| FixtureError::Frame)?,
    );
    if head_only {
        return Ok(http_response(
            200,
            "OK",
            &[("content-type", RELAY_FRAMES_CONTENT_TYPE)],
            &frames,
            frames.len() + 64,
        ));
    }
    if !reply.body.is_empty() {
        frames.extend(
            Frame::Data(Data {
                seq: 0,
                offset: 0,
                bytes: reply.body.clone(),
            })
            .encode()
            .map_err(|_| FixtureError::Frame)?,
        );
    }
    if !reply.trailers.is_empty() {
        frames.extend(
            Frame::Trailers(Trailers {
                headers: reply.trailers.clone(),
            })
            .encode()
            .map_err(|_| FixtureError::Frame)?,
        );
    }
    frames.extend(
        Frame::End(RelayEnd::Complete {})
            .encode()
            .map_err(|_| FixtureError::Frame)?,
    );
    Ok(http_response(
        200,
        "OK",
        &[("content-type", RELAY_FRAMES_CONTENT_TYPE)],
        &frames,
        frames.len(),
    ))
}

async fn write_refusal<S>(stream: &mut S, refusal: GatewayRefusal) -> Result<(), FixtureError>
where
    S: AsyncWrite + Unpin,
{
    let status = refusal.http_status();
    let code = refusal.code();
    let body = call::encode(&RefusalBody {
        gateway_error: refusal,
    })
    .map_err(|_| FixtureError::Frame)?;
    let bytes = http_response(
        status,
        reason(status),
        &[("content-type", "application/json"), (ERROR_HEADER, code)],
        &body,
        body.len(),
    );
    write_bytes(stream, &bytes).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_json<S>(stream: &mut S, value: &impl serde::Serialize) -> Result<(), FixtureError>
where
    S: AsyncWrite + Unpin,
{
    let body = call::encode(value).map_err(|_| FixtureError::Frame)?;
    if body.len() > MAX_CALL_BYTES {
        return write_refusal(stream, GatewayRefusal::LimitExceeded {}).await;
    }
    let bytes = http_response(
        200,
        "OK",
        &[("content-type", "application/json")],
        &body,
        body.len(),
    );
    write_bytes(stream, &bytes).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_bytes<S>(stream: &mut S, bytes: &[u8]) -> Result<(), FixtureError>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(bytes).await.map_err(FixtureError::Io)
}

fn http_response(
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    claimed: usize,
) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(
        format!("content-length: {claimed}\r\nconnection: close\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(body);
    out
}

fn reason(status: u16) -> &'static str {
    match status {
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

async fn read_request<S>(stream: &mut S) -> Result<Incoming, FixtureError>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 1024];
    let head_end = loop {
        if let Some(index) = find_subsequence(&buf, b"\r\n\r\n") {
            break index;
        }
        if buf.len() > MAX_HEAD {
            return Err(FixtureError::Http);
        }
        let read = stream.read(&mut tmp).await.map_err(FixtureError::Io)?;
        if read == 0 {
            return Err(FixtureError::Incomplete);
        }
        buf.extend_from_slice(&tmp[..read]);
    };
    let head = &buf[..head_end];
    let mut rest = buf[head_end + 4..].to_vec();
    let head = std::str::from_utf8(head).map_err(|_| FixtureError::Http)?;
    let mut lines = head.split("\r\n");
    let request = lines.next().ok_or(FixtureError::Http)?;
    let mut parts = request.split(' ');
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    if method.is_empty() || path.is_empty() || parts.next().is_none() {
        return Err(FixtureError::Http);
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(FixtureError::Http);
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = content_length(&headers)?;
    if length > MAX_BODY {
        return Err(FixtureError::Http);
    }
    let incomplete = if rest.len() >= length {
        rest.truncate(length);
        false
    } else {
        let mut pending = vec![0_u8; length - rest.len()];
        match stream.read_exact(&mut pending).await {
            Ok(_) => {
                rest.extend_from_slice(&pending);
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => true,
            Err(error) => return Err(FixtureError::Io(error)),
        }
    };
    Ok(Incoming {
        method,
        path,
        headers,
        body: rest,
        incomplete,
    })
}

fn content_length(headers: &[(String, String)]) -> Result<usize, FixtureError> {
    let mut found = None;
    for (name, value) in headers {
        if name == "content-length" {
            if found.is_some() {
                return Err(FixtureError::Http);
            }
            found = Some(value.parse::<usize>().map_err(|_| FixtureError::Http)?);
        }
    }
    Ok(found.unwrap_or(0))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
