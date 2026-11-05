//! `/wire`: the WebSocket upgrade and one connection's wire v2 on the
//! session's shared host service. One frame per text message; a binary
//! message, a message past the line cap, a slow reader, a silent peer or a
//! client that floods intents ends the connection, which detaches.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use http_body_util::Full;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::upgrade::Upgraded;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use plakous_host_protocol::wire::{HandshakeRefusal, MAX_LINE_BYTES};
use plakous_hosted::host_service::wire::{
    Face, HELLO_DEADLINE, WireConnection, WireSession, handshake, refuse,
};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

use super::Served;
use super::auth::ConnectionSlot;
use crate::surface_protocol::PlakExtension;

/// The RFC 6455 key suffix.
const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// How often the host pings, and how long a ping may go unanswered.
const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(30);
/// How long one frame may take to reach a slow client before it is dropped.
const SEND_DEADLINE: Duration = Duration::from_secs(10);
/// The most frames waiting for one client before it is dropped as slow.
const MAX_PENDING_FRAMES: usize = 8192;
/// Client messages per second a connection may send, with this burst.
const MESSAGES_PER_SECOND: f64 = 20.0;
const MESSAGE_BURST: f64 = 60.0;

/// The `Sec-WebSocket-Accept` for a valid upgrade request, or `None` when
/// the request is not one.
pub(crate) fn accept_key(headers: &HeaderMap) -> Option<String> {
    let has_token = |name: header::HeaderName, token: &str| {
        headers.get_all(name).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case(token))
            })
        })
    };
    if !has_token(header::UPGRADE, "websocket") || !has_token(header::CONNECTION, "upgrade") {
        return None;
    }
    if headers.get(header::SEC_WEBSOCKET_VERSION)?.as_bytes() != b"13" {
        return None;
    }
    let key = headers.get(header::SEC_WEBSOCKET_KEY)?;
    if STANDARD.decode(key.as_bytes()).ok()?.len() != 16 {
        return None;
    }
    let mut context = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    context.update(key.as_bytes());
    context.update(ACCEPT_GUID);
    Some(STANDARD.encode(context.finish()))
}

/// The `101` that completes the upgrade.
pub(crate) fn switching_protocols(accept: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(accept) = HeaderValue::from_str(accept) {
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    }
    response
}

/// The frames one connection has written and not yet sent: the connection
/// writes whole lines, and each line goes out as one text message.
#[derive(Clone, Default)]
struct Outbox(Arc<Mutex<Pending>>);

#[derive(Default)]
struct Pending {
    partial: Vec<u8>,
    lines: VecDeque<String>,
    overflowed: bool,
}

impl Outbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self) -> (VecDeque<String>, bool) {
        let mut pending = self.lock();
        (std::mem::take(&mut pending.lines), pending.overflowed)
    }
}

impl Write for Outbox {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut pending = self.lock();
        pending.partial.extend_from_slice(buf);
        while let Some(end) = pending.partial.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.partial.drain(..=end).collect();
            let text = String::from_utf8(line[..end].to_vec())
                .map_err(|_| std::io::Error::other("a frame is not UTF-8"))?;
            if pending.lines.len() >= MAX_PENDING_FRAMES {
                pending.overflowed = true;
                return Err(std::io::Error::other("the client fell behind"));
            }
            pending.lines.push_back(text);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A token bucket over a connection's messages.
struct Rate {
    tokens: f64,
    at: Instant,
}

impl Rate {
    fn new() -> Self {
        Self {
            tokens: MESSAGE_BURST,
            at: Instant::now(),
        }
    }

    fn admit(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.at).as_secs_f64() * MESSAGES_PER_SECOND;
        self.tokens = (self.tokens + refill).min(MESSAGE_BURST);
        self.at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

type Socket = WebSocketStream<TokioIo<Upgraded>>;

/// Serves one upgraded connection until it leaves, misbehaves, or the
/// session ends.
pub(crate) async fn serve(upgraded: Upgraded, served: Arc<Served>, slot: ConnectionSlot) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_LINE_BYTES))
        .max_frame_size(Some(MAX_LINE_BYTES));
    let mut socket =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config)).await;
    let reason = drive(&mut socket, &served).await;
    let code = match reason {
        Ended::Left | Ended::SessionClosed => CloseCode::Normal,
        Ended::Policy(_) => CloseCode::Policy,
        Ended::Unsupported => CloseCode::Unsupported,
        Ended::Lost(_) => CloseCode::Away,
    };
    let _ = tokio::time::timeout(
        SEND_DEADLINE,
        socket.close(Some(CloseFrame {
            code,
            reason: "".into(),
        })),
    )
    .await;
    tracing::info!(target: "plak::web", ended = reason.as_str(), "plak web connection closed");
    drop(slot);
}

/// How a connection ended; the log's words.
enum Ended {
    Left,
    SessionClosed,
    Policy(&'static str),
    Unsupported,
    Lost(&'static str),
}

impl Ended {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Left => "client left",
            Self::SessionClosed => "session ended",
            Self::Policy(reason) | Self::Lost(reason) => reason,
            Self::Unsupported => "binary message",
        }
    }
}

/// The next text message, or how the connection ended instead.
async fn next_text(socket: &mut Socket, pong: &mut Option<Instant>) -> Result<String, Ended> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let text = text.as_str();
                return Ok(text.strip_suffix('\n').unwrap_or(text).to_owned());
            }
            Some(Ok(Message::Binary(_))) => return Err(Ended::Unsupported),
            Some(Ok(Message::Pong(_))) => *pong = None,
            Some(Ok(Message::Ping(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) | None => return Err(Ended::Left),
            Some(Err(_)) => return Err(Ended::Policy("malformed or oversized message")),
        }
    }
}

/// Sends what the connection wrote; `Err` when the client is too slow.
async fn send_pending(socket: &mut Socket, outbox: &Outbox) -> Result<(), Ended> {
    let (lines, overflowed) = outbox.take();
    if overflowed {
        return Err(Ended::Lost("client fell behind"));
    }
    for line in lines {
        match tokio::time::timeout(SEND_DEADLINE, socket.send(Message::text(line))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(Ended::Lost("send failed")),
            Err(_) => return Err(Ended::Lost("client too slow")),
        }
    }
    Ok(())
}

async fn drive(socket: &mut Socket, served: &Served) -> Ended {
    let mut pong = None;
    let outbox = Outbox::default();
    let hello = match tokio::time::timeout(HELLO_DEADLINE, next_text(socket, &mut pong)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(ended)) => return ended,
        Err(_) => return Ended::Policy("no hello"),
    };
    let (lines, first) = plakous_hosted::host_service::wire::lines(1);
    let _ = lines.send(Ok(hello));
    drop(lines);
    let stop = served.stop;
    // The connection is the session's trigger: a session that released
    // itself is opened again for it. One that does not open is refused in
    // a v2 frame; the connection passed the gate, so the reason is its own.
    let session = match tokio::task::block_in_place(|| served.slot.resident()) {
        Ok(session) => session,
        Err(failure) => {
            let _ = refuse::<PlakExtension>(
                &mut outbox.clone(),
                HandshakeRefusal::Unavailable {
                    detail: failure.message().to_owned(),
                },
            );
            let _ = send_pending(socket, &outbox).await;
            return Ended::SessionClosed;
        }
    };
    // Connected before the welcome, which names the session and root of
    // the connection's own handle: a switch after it moves the connection.
    let mut connection = session
        // The web endpoint is never a local face, however its client
        // authenticated: it may not exec or hand off the host's build.
        .connect(outbox.clone(), Face::Remote, move || stop.stopped());
    let admitted = handshake::<PlakExtension>(
        &first,
        served.welcome(&connection, &session),
        &mut outbox.clone(),
        &move || stop.stopped(),
    );
    if let Err(ended) = send_pending(socket, &outbox).await {
        return ended;
    }
    if !matches!(admitted, Ok(true)) {
        return Ended::Policy("handshake refused");
    }
    let ended = attached(
        socket,
        served,
        &session,
        &mut connection,
        &outbox,
        &mut pong,
    )
    .await;
    // Every way out detaches, so the session never holds a dead attachment.
    connection.leave();
    ended
}

type Connection = WireConnection<PlakExtension, Outbox>;

/// Serves an admitted connection until it ends.
async fn attached(
    socket: &mut Socket,
    served: &Served,
    session: &WireSession<PlakExtension>,
    connection: &mut Connection,
    outbox: &Outbox,
    pong: &mut Option<Instant>,
) -> Ended {
    let mut changes = session.changes();
    let mut pinger = tokio::time::interval(PING_EVERY);
    pinger.tick().await;
    let mut rate = Rate::new();
    loop {
        if connection.flush().is_err() {
            return Ended::Lost("client fell behind");
        }
        if let Err(ended) = send_pending(socket, outbox).await {
            return ended;
        }
        // A session that released itself ends its connections; the next
        // one wakes it.
        if session.ended() || served.stop.stopped() {
            return Ended::SessionClosed;
        }
        tokio::select! {
            message = next_text(socket, pong) => {
                let line = match message {
                    Ok(line) => line,
                    Err(ended) => return ended,
                };
                if !rate.admit() {
                    return Ended::Policy("message rate");
                }
                if !connection.frame(Ok(line)).unwrap_or(false) {
                    let _ = connection.flush();
                    let _ = send_pending(socket, outbox).await;
                    return Ended::Left;
                }
            }
            _ = changes.changed() => {}
            _ = pinger.tick() => {
                if pong.is_some_and(|sent| sent.elapsed() >= PONG_DEADLINE) {
                    return Ended::Lost("no pong");
                }
                if pong.is_none() {
                    *pong = Some(Instant::now());
                }
                // A peer that stopped reading must not hold the slot: the
                // ping is bounded like every other send.
                match tokio::time::timeout(SEND_DEADLINE, socket.send(Message::Ping(Bytes::new())))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return Ended::Lost("ping failed"),
                    Err(_) => return Ended::Lost("client too slow"),
                }
            }
        }
    }
}
