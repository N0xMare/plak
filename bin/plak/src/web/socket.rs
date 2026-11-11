//! `/wire`: the WebSocket upgrade and one connection's wire v2 as a client
//! of the host's session directory. One frame per text message; a binary
//! message, a message past the line cap, a slow reader, a silent peer or a
//! client that floods intents ends the connection, which detaches.

#[cfg(test)]
mod principal_tests;

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use http_body_util::Full;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::upgrade::Upgraded;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use plakous_host_protocol::wire::MAX_LINE_BYTES;
use plakous_hosted::host_service::wire::{Face, HELLO_DEADLINE, HostConnection, handshake};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

use super::Served;
use super::auth::ConnectionSlot;
use crate::host::send::{self as progress, MAX_PENDING_BYTES, ProgressIo};
use crate::surface_protocol::PlakExtension;

/// The RFC 6455 key suffix.
const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// How often a connection looks for a stop.
const STOP_POLL: Duration = Duration::from_millis(200);
/// How often the host pings, and how long a ping may go unanswered.
const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(30);
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
    bytes: usize,
}

impl Outbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self) -> (VecDeque<String>, bool) {
        let mut pending = self.lock();
        pending.bytes = 0;
        (std::mem::take(&mut pending.lines), pending.overflowed)
    }
}

impl Write for Outbox {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut pending = self.lock();
        if pending
            .bytes
            .saturating_add(pending.partial.len())
            .saturating_add(buf.len())
            > MAX_PENDING_BYTES
        {
            pending.overflowed = true;
            return Err(std::io::Error::other("the client fell behind"));
        }
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
            pending.bytes += line.len();
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

type Socket = WebSocketStream<ProgressIo<TokioIo<Upgraded>>>;

/// Serves one upgraded connection until it leaves, misbehaves, or the
/// session ends.
pub(crate) async fn serve(upgraded: Upgraded, served: Arc<Served>, slot: ConnectionSlot) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_LINE_BYTES))
        .max_frame_size(Some(MAX_LINE_BYTES));
    let mut socket = WebSocketStream::from_raw_socket(
        ProgressIo::new(TokioIo::new(upgraded)),
        Role::Server,
        Some(config),
    )
    .await;
    let reason = drive(&mut socket, &served, slot.login()).await;
    let code = match reason {
        Ended::Left | Ended::Stopping => CloseCode::Normal,
        Ended::Policy(_) => CloseCode::Policy,
        Ended::Unsupported => CloseCode::Unsupported,
        Ended::Lost(_) => CloseCode::Away,
    };
    let _ = tokio::time::timeout(
        progress::IDLE,
        socket.close(Some(CloseFrame {
            code,
            reason: "".into(),
        })),
    )
    .await;
    tracing::info!(target: "plak::web", ended = reason.as_str(), "web connection closed");
    drop(slot);
}

/// How a connection ended; the log's words.
enum Ended {
    Left,
    Stopping,
    Policy(&'static str),
    Unsupported,
    Lost(&'static str),
}

impl Ended {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Left => "client left",
            Self::Stopping => "host stopping",
            Self::Policy(reason) | Self::Lost(reason) => reason,
            Self::Unsupported => "binary message",
        }
    }
}

/// The next text message, or how the connection ended instead.
async fn next_text<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    pong: &mut Option<Instant>,
) -> Result<String, Ended> {
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
async fn send_pending<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<ProgressIo<S>>,
    outbox: &Outbox,
    stopped: impl Fn() -> bool,
) -> Result<(), Ended> {
    let (lines, overflowed) = outbox.take();
    if overflowed {
        return Err(Ended::Lost("client fell behind"));
    }
    let progress = socket.get_ref().progress.clone();
    match progress
        .stoppable(
            async {
                for line in lines {
                    socket
                        .send(Message::text(line))
                        .await
                        .map_err(|_| Ended::Lost("send failed"))?;
                }
                Ok(())
            },
            &stopped,
        )
        .await
    {
        Ok(result) => result,
        Err(()) if stopped() => Err(Ended::Stopping),
        Err(()) => Err(Ended::Lost("client too slow")),
    }
}

async fn drive(socket: &mut Socket, served: &Arc<Served>, login: &str) -> Ended {
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
    let principal = plakous_hosted::host_service::wire::HostPrincipal::Web(login.to_owned());
    let admitted = handshake::<PlakExtension>(
        &first,
        served.host.welcome_for(&principal),
        &mut outbox.clone(),
        &move || stop.stopped(),
    );
    if let Err(ended) = send_pending(socket, &outbox, || served.stop.stopped()).await {
        return ended;
    }
    if !matches!(admitted, Ok(true)) {
        return Ended::Policy("handshake refused");
    }
    // The web endpoint is never a local face, however its client
    // authenticated: it may not exec or hand off the host's build.
    let mut connection = served
        .host
        .connect(outbox.clone(), Face::Remote, principal, move || {
            stop.stopped()
        });
    let ended = attached(socket, served, &mut connection, &outbox, &mut pong).await;
    // Every way out detaches, so no session holds a dead attachment.
    connection.leave();
    ended
}

type Connection = HostConnection<PlakExtension, Outbox>;

/// Serves an admitted connection until it ends.
async fn attached<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<ProgressIo<S>>,
    served: &Served,
    connection: &mut Connection,
    outbox: &Outbox,
    pong: &mut Option<Instant>,
) -> Ended {
    let mut pinger = tokio::time::interval(PING_EVERY);
    pinger.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    pinger.tick().await;
    let mut rate = Rate::new();
    loop {
        // A flush may read the session directory from the stores.
        if connection_work(|| connection.flush()).is_err() {
            return Ended::Lost("client fell behind");
        }
        let began = Instant::now();
        if let Err(ended) = send_pending(socket, outbox, || served.stop.stopped()).await {
            return ended;
        }
        if began.elapsed() >= PING_EVERY {
            if let Some(sent) = pong {
                *sent = Instant::now();
            }
        }
        if served.stop.stopped() {
            return Ended::Stopping;
        }
        tokio::select! {
            biased;
            message = next_text(socket, pong) => {
                let mut line = match message {
                    Ok(line) => zeroize::Zeroizing::new(line),
                    Err(ended) => return ended,
                };
                if !Connection::is_host_request(&line) && !rate.admit() {
                    return Ended::Policy("message rate");
                }
                // An attach may open a session, which takes a while.
                if !connection_work(|| connection.frame(Ok(std::mem::take(&mut *line)))).unwrap_or(false) {
                    let _ = connection.flush();
                    let _ = send_pending(socket, outbox, || served.stop.stopped()).await;
                    return Ended::Left;
                }
            }
            () = connection.changed() => {}
            // A stop ends the connection as a leaving client's.
            () = tokio::time::sleep(STOP_POLL) => {}
            _ = pinger.tick() => {
                if pong.is_some_and(|sent| sent.elapsed() >= PONG_DEADLINE) {
                    return Ended::Lost("no pong");
                }
                if pong.is_none() {
                    *pong = Some(Instant::now());
                }
                // A peer that stopped reading must not hold the slot: the
                // ping is bounded like every other send.
                match socket.get_ref().progress.clone().stoppable(socket.send(Message::Ping(Bytes::new())), || served.stop.stopped())
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return Ended::Lost("ping failed"),
                    Err(()) if served.stop.stopped() => return Ended::Stopping,
                    Err(()) => return Ended::Lost("client too slow"),
                }
            }
        }
    }
}

fn connection_work<T>(run: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::CurrentThread
    {
        return run();
    }
    tokio::task::block_in_place(run)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod slow_link_tests;
