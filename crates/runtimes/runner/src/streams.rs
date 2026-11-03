//! Credit-paced byte streams over a connection (spec 01 §8.4). Each end of a
//! stream sends data only within the credit the other end granted, grants
//! credit back as it writes what it received, and refuses data beyond the
//! credit it gave, so neither end buffers more than that. Every frame one end
//! sends on a stream, data or credit, takes that direction's next sequence
//! number. A close with a reason aborts the stream in both directions: an end
//! that finds a gap, undecodable data or an overrun sends one and stops.

use std::sync::{Arc, Mutex, PoisonError};

use plakous_runner_protocol::{StreamEvent, StreamFrame};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};

/// The credit a host may use on a new stream before the runner grants any.
/// A runner sends nothing until the host grants.
pub(crate) const INITIAL_CREDIT: u64 = 256 * 1024;

/// The largest data frame payload, in bytes before encoding.
const CHUNK: usize = 64 * 1024;

/// Bytes the other end allows this end to send, until the stream closes.
pub(crate) struct Credit {
    state: Mutex<(u64, bool)>,
    changed: Notify,
}

impl Credit {
    pub(crate) fn new(initial: u64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((initial, false)),
            changed: Notify::new(),
        })
    }

    fn grant(&self, bytes: u64) {
        let mut state = lock(&self.state);
        state.0 = state.0.saturating_add(bytes);
        drop(state);
        self.changed.notify_one();
    }

    pub(crate) fn close(&self) {
        lock(&self.state).1 = true;
        self.changed.notify_one();
    }

    /// The bytes available now, waiting while there are none; `None` once
    /// the stream closed.
    async fn available(&self) -> Option<u64> {
        loop {
            let notified = self.changed.notified();
            {
                let state = lock(&self.state);
                if state.1 {
                    return None;
                }
                if state.0 > 0 {
                    return Some(state.0);
                }
            }
            notified.await;
        }
    }

    fn consume(&self, bytes: u64) {
        let mut state = lock(&self.state);
        state.0 = state.0.saturating_sub(bytes);
    }
}

/// One direction of one stream as this end sends it. Data and credit frames
/// come from two tasks; each frame takes its number and its place in the
/// connection's queue under one lock, so the numbers arrive in order.
pub(crate) struct Outbound {
    stream: u64,
    seq: Mutex<u64>,
    send: Box<dyn Fn(StreamFrame) -> bool + Send + Sync>,
}

impl Outbound {
    pub(crate) fn new(
        stream: u64,
        send: impl Fn(StreamFrame) -> bool + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            stream,
            seq: Mutex::new(0),
            send: Box::new(send),
        })
    }

    pub(crate) fn send(&self, event: StreamEvent) -> bool {
        let mut seq = lock(&self.seq);
        let sent = (self.send)(StreamFrame {
            stream: self.stream,
            seq: *seq,
            event,
        });
        *seq += 1;
        sent
    }
}

/// Sends `reader`'s bytes as data frames within `credit`, then a close.
pub(crate) async fn send_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    credit: Arc<Credit>,
    out: Arc<Outbound>,
) {
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        let Some(available) = credit.available().await else {
            return;
        };
        let room = usize::try_from(available).unwrap_or(CHUNK).min(CHUNK);
        let read = match reader.read(&mut buffer[..room]).await {
            Ok(0) => {
                out.send(StreamEvent::Close { reason: None });
                return;
            }
            Ok(read) => read,
            Err(error) => {
                out.send(StreamEvent::Close {
                    reason: Some(error.kind().to_string()),
                });
                return;
            }
        };
        credit.consume(u64::try_from(read).unwrap_or(u64::MAX));
        if !out.send(StreamEvent::Data {
            payload: Value::String(hex(&buffer[..read])),
        }) {
            return;
        }
    }
}

/// The bytes this end granted the other and has not received yet, or none
/// once the stream ended in both directions: an ended stream grants nothing.
pub(crate) struct Window(Mutex<Option<u64>>);

impl Window {
    pub(crate) fn new(granted: u64) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Some(granted))))
    }

    /// Takes `bytes` of received data from the window; `false` when they
    /// exceed what was granted.
    fn take(&self, bytes: u64) -> bool {
        let mut window = lock(&self.0);
        let Some(left) = window.and_then(|window| window.checked_sub(bytes)) else {
            return false;
        };
        *window = Some(left);
        true
    }

    /// Grants `bytes` back through `out`, unless the stream ended. The grant
    /// is sent under the window's lock, so it cannot follow a close.
    fn give(&self, bytes: u64, out: &Outbound) {
        let mut window = lock(&self.0);
        let Some(granted) = window.as_mut() else {
            return;
        };
        *granted = granted.saturating_add(bytes);
        out.send(StreamEvent::Credit { bytes });
    }

    fn close(&self) {
        *lock(&self.0) = None;
    }
}

/// Writes each received chunk to `writer` and grants its bytes back; shuts
/// `writer` down when the other end closes the stream or the link is lost.
pub(crate) async fn receive_stream<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut chunks: mpsc::UnboundedReceiver<Vec<u8>>,
    window: Arc<Window>,
    out: Arc<Outbound>,
) {
    while let Some(chunk) = chunks.recv().await {
        if writer.write_all(&chunk).await.is_err() || writer.flush().await.is_err() {
            break;
        }
        window.give(u64::try_from(chunk.len()).unwrap_or(u64::MAX), &out);
    }
    let _ = writer.shutdown().await;
}

/// What this end holds of one stream: where received data goes, the credit
/// its own sending uses, what it granted the other end, and its own sending
/// direction for an abort.
pub(crate) struct StreamEnds {
    inbound: Option<mpsc::UnboundedSender<Vec<u8>>>,
    credit: Arc<Credit>,
    window: Arc<Window>,
    out: Arc<Outbound>,
    expected: u64,
}

impl StreamEnds {
    pub(crate) const fn new(
        inbound: mpsc::UnboundedSender<Vec<u8>>,
        credit: Arc<Credit>,
        window: Arc<Window>,
        out: Arc<Outbound>,
    ) -> Self {
        Self {
            inbound: Some(inbound),
            credit,
            window,
            out,
            expected: 0,
        }
    }

    /// Takes one frame the other end sent; `false` when the stream ended in
    /// both directions: this end found it broken (and told the other end),
    /// or the other end aborted it.
    pub(crate) fn receive(&mut self, frame: StreamFrame) -> bool {
        if frame.seq != self.expected {
            return self.abort("the stream skipped a frame");
        }
        self.expected += 1;
        match frame.event {
            StreamEvent::Data { payload } => {
                let Some(bytes) = payload.as_str().and_then(unhex) else {
                    return self.abort("the stream carried undecodable data");
                };
                if !self
                    .window
                    .take(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                {
                    return self.abort("the stream sent beyond its credit");
                }
                if let Some(inbound) = &self.inbound {
                    let _ = inbound.send(bytes);
                }
            }
            StreamEvent::Credit { bytes } => self.credit.grant(bytes),
            StreamEvent::Close { reason: None } => self.inbound = None,
            StreamEvent::Close { reason: Some(_) } => {
                self.close();
                return false;
            }
        }
        true
    }

    fn abort(&mut self, reason: &str) -> bool {
        // Closed first, so no grant follows the abort.
        self.close();
        self.out.send(StreamEvent::Close {
            reason: Some(reason.to_owned()),
        });
        false
    }

    /// Ends both directions locally.
    pub(crate) fn close(&mut self) {
        self.inbound = None;
        self.credit.close();
        self.window.close();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            text.get(index..index + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}
