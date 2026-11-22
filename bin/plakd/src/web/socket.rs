//! `/wire`: the WebSocket upgrade and one connection's wire v2 as a client
//! of the host's session directory. One frame per text message; a binary
//! message, a message past the line cap, a slow reader, a silent peer or a
//! client that floods intents ends the connection, which detaches.

#[cfg(test)]
mod principal_tests;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use http_body_util::Full;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::upgrade::Upgraded;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use plakous_host_protocol::wire::MAX_LINE_BYTES;
use plakous_hosted::host_service::wire::{Face, HELLO_DEADLINE, handshake, test_timing};
use tokio::task::JoinSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};

use super::Served;
use super::auth::ConnectionSlot;
use crate::host::connection::{
    Connection, ConnectionFlush, Frame, FrameWriter, Outbox, SendError, connection_work,
};
use crate::host::send::{ProgressIo, STOP_JOIN, TOTAL};
use crate::surface_protocol::PlakExtension;

/// The RFC 6455 key suffix.
const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// How often a connection looks for a stop.
const STOP_POLL: Duration = Duration::from_millis(200);
/// How often the host pings, and how long a ping may go unanswered.
const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(30);
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
    let socket = WebSocketStream::from_raw_socket(
        ProgressIo::new(TokioIo::new(upgraded)),
        Role::Server,
        Some(config),
    )
    .await;
    let reason = drive(socket, &served, slot.login()).await;
    tracing::info!(target: "plak::web", ended = reason.as_str(), "web connection closed");
    drop(slot);
}

/// How a connection ended; the log's words.
#[derive(Clone, Copy)]
enum Ended {
    Left,
    Bye,
    Stopping,
    Policy(&'static str),
    Unsupported,
    Lost(&'static str),
}

impl Ended {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Left | Self::Bye => "client left",
            Self::Stopping => "host stopping",
            Self::Policy(reason) | Self::Lost(reason) => reason,
            Self::Unsupported => "binary message",
        }
    }
}

impl From<SendError> for Ended {
    fn from(error: SendError) -> Self {
        match error {
            SendError::Full => Self::Lost("client fell behind"),
            SendError::Stalled => Self::Lost("client too slow"),
            SendError::Disconnected => Self::Lost("send failed"),
        }
    }
}

async fn close<S>(sink: &mut S, reason: Ended)
where
    S: Sink<Message> + Unpin,
{
    let code = match reason {
        Ended::Left | Ended::Bye | Ended::Stopping => CloseCode::Normal,
        Ended::Policy(_) => CloseCode::Policy,
        Ended::Unsupported => CloseCode::Unsupported,
        Ended::Lost(_) => CloseCode::Away,
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        sink.send(Message::Close(Some(CloseFrame {
            code,
            reason: "".into(),
        }))),
    )
    .await;
}

#[derive(Default)]
enum Ping {
    #[default]
    Idle,
    Queued,
    Sent(Instant),
}

#[derive(Clone, Default)]
struct Heartbeat(Arc<PingState>);

#[derive(Default)]
struct PingState {
    ping: Mutex<Ping>,
    changed: tokio::sync::Notify,
}

impl Heartbeat {
    fn lock(&self) -> std::sync::MutexGuard<'_, Ping> {
        self.0.ping.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tick(&self, outbox: &Outbox) -> Result<(), Ended> {
        let mut ping = self.lock();
        match *ping {
            Ping::Idle => {
                outbox
                    .control(None)
                    .map_err(|_| Ended::Lost("client fell behind"))?;
                *ping = Ping::Queued;
            }
            Ping::Queued | Ping::Sent(_) => {}
        }
        Ok(())
    }

    fn sent(&self) {
        *self.lock() = Ping::Sent(Instant::now());
        self.0.changed.notify_one();
    }

    fn pong(&self) {
        let mut ping = self.lock();
        if matches!(*ping, Ping::Sent(_)) {
            *ping = Ping::Idle;
            self.0.changed.notify_one();
        }
    }

    async fn expired(&self) {
        loop {
            let deadline = match *self.lock() {
                Ping::Sent(sent) => Some(sent + PONG_DEADLINE),
                Ping::Idle | Ping::Queued => None,
            };
            if deadline.is_some_and(|at| at <= Instant::now()) {
                return;
            }
            tokio::select! {
                () = self.0.changed.notified() => {}
                () = async {
                    if let Some(at) = deadline {
                        tokio::time::sleep_until(at).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
        }
    }
}

/// The next text message, or how the connection ended instead.
async fn next_text(
    socket: &mut (impl Stream<Item = Result<Message, WebSocketError>> + Unpin),
    heartbeat: &Heartbeat,
) -> Result<String, Ended> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let text = text.as_str();
                return Ok(text.strip_suffix('\n').unwrap_or(text).to_owned());
            }
            Some(Ok(Message::Binary(_))) => return Err(Ended::Unsupported),
            Some(Ok(Message::Pong(_))) => heartbeat.pong(),
            Some(Ok(Message::Ping(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) | None => return Err(Ended::Left),
            Some(Err(_)) => return Err(Ended::Policy("malformed or oversized message")),
        }
    }
}

struct WebWriter<S> {
    sink: S,
    heartbeat: Heartbeat,
}

impl<S: Sink<Message> + Unpin + Send> FrameWriter for WebWriter<S> {
    async fn send_frame(&mut self, frame: Frame) -> Result<(), SendError> {
        let message = if let Some(mut line) = frame {
            test_timing::writer("writer start", &line);
            line.pop(); // the outbox holds newline-terminated frames
            Message::text(String::from_utf8(line).map_err(|_| SendError::Disconnected)?)
        } else {
            // A queued ping does not start its reply deadline while a large
            // text message is still being written ahead of it.
            self.heartbeat.sent();
            Message::Ping(Bytes::new())
        };
        self.sink
            .send(message)
            .await
            .map_err(|_| SendError::Disconnected)
    }
}

async fn drive(mut socket: Socket, served: &Served, login: &str) -> Ended {
    let outbox = Outbox::default();
    match admit(&mut socket, served, login, &outbox).await {
        Ok(mut connection) => attached(socket, served, &mut connection, &outbox).await,
        Err(reason) => {
            close(&mut socket, reason).await;
            reason
        }
    }
}

async fn admit(
    socket: &mut Socket,
    served: &Served,
    login: &str,
    outbox: &Outbox,
) -> Result<Connection, Ended> {
    let heartbeat = Heartbeat::default();
    let hello = match tokio::time::timeout(HELLO_DEADLINE, next_text(socket, &heartbeat)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(ended)) => return Err(ended),
        Err(_) => return Err(Ended::Policy("no hello")),
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
    let progress = socket.get_ref().progress.clone();
    outbox
        .send_batch(
            &progress,
            || stop.stopped(),
            &mut WebWriter {
                sink: socket,
                heartbeat,
            },
        )
        .await?;
    if !matches!(admitted, Ok(true)) {
        return Err(Ended::Policy("handshake refused"));
    }
    // The web endpoint is never a local face, however its client
    // authenticated: it may not exec or hand off the host's build.
    let snapshots = outbox.clone();
    Ok(served
        .host
        .connect(outbox.clone(), Face::Remote, principal, move || {
            stop.stopped()
        })
        .buffered_output()
        .with_snapshot_output(move |attachment, snapshot| snapshots.snapshot(attachment, snapshot)))
}

fn handle_frame(
    connection: &mut Connection,
    outbox: &Outbox,
    rate: &mut Rate,
    message: Result<String, Ended>,
) -> Result<(), Ended> {
    let _timing = test_timing::phase("read ready").entered();
    let line = message?;
    if !Connection::is_host_request(&line) && !rate.admit() {
        let _line = zeroize::Zeroizing::new(line);
        return Err(Ended::Policy("message rate"));
    }
    match connection_work(|| connection.frame(Ok(line))) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let _ = connection_work(|| outbox.flush_connection(connection));
            Err(Ended::Bye)
        }
        Err(_) => Err(Ended::Lost("request failed")),
    }
}

/// Serves an admitted connection until it ends.
async fn attached<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
    socket: WebSocketStream<ProgressIo<S>>,
    served: &Served,
    connection: &mut Connection,
    outbox: &Outbox,
) -> Ended {
    let progress = socket.get_ref().progress.clone();
    let (sink, mut reader) = socket.split();
    let heartbeat = Heartbeat::default();
    let mut transport = WebWriter {
        sink,
        heartbeat: heartbeat.clone(),
    };
    let queue = outbox.clone();
    let stop = served.stop;
    let mut writer = JoinSet::new();
    writer.spawn(async move {
        let result = queue
            .write_to(&progress, || stop.stopped(), &mut transport)
            .await;
        (transport.sink, result)
    });

    let mut pinger = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    pinger.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut rate = Rate::new();
    let mut finished = None;
    let mut flush = ConnectionFlush::default();
    let mut ended = loop {
        if served.stop.stopped() {
            connection_work(|| {
                crate::host::lifecycle::stopping(&served.host);
                let _ = connection.flush_lifecycle();
            });
            break Ended::Stopping;
        }
        tokio::select! {
            biased;
            result = writer.join_next() => {
                let Some(Ok((sink, result))) = result else {
                    break Ended::Lost("send failed");
                };
                finished = Some((sink, result));
                break result.map_or_else(Ended::from, |()| Ended::Left);
            }
            message = next_text(&mut reader, &heartbeat), if flush.can_read() => {
                if let Err(ended) = handle_frame(connection, outbox, &mut rate, message) {
                    break ended;
                }
                flush.read();
            }
            () = heartbeat.expired() => break Ended::Lost("no pong"),
            _ = pinger.tick() => {
                if let Err(ended) = heartbeat.tick(outbox) {
                    break ended;
                }
            }
            () = std::future::ready(()), if flush.pending() => {
                if flush.step(connection, outbox).is_err() {
                    break Ended::Lost("client fell behind");
                }
            }
            () = connection.changed() => flush.request(),
            () = outbox.drained() => flush.request(),
            () = tokio::time::sleep(STOP_POLL) => flush.request(),
        }
    };
    outbox.close();
    let join = if matches!(ended, Ended::Bye) && !served.stop.stopped() {
        TOTAL
    } else {
        STOP_JOIN
    };
    if finished.is_none()
        && let Ok(Some(Ok(result))) = tokio::time::timeout(join, writer.join_next()).await
    {
        finished = Some(result);
    }
    if let Some((mut sink, result)) = finished {
        if result.is_ok() {
            connection.output_sent();
        } else if matches!(result, Err(SendError::Full)) {
            ended = Ended::Lost("client fell behind");
        }
        // A failed or cancelled frame may already have been partly written.
        if matches!(result, Ok(()) | Err(SendError::Full)) {
            close(&mut sink, ended).await;
        }
    }
    connection_work(|| connection.leave());
    ended
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod slow_link_tests;
