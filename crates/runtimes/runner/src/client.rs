//! The host end: a [`RunnerLink`] that frames, keys and fences each call over
//! one runner connection, carries streams, watches the link's liveness, and
//! survives a reconnection.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_model::CancellationToken;
use plakous_runner_protocol::frame::FrameError;
use plakous_runner_protocol::{
    Bind, Hello, HostFrame, LineOwner, Operation, Refusal, Request, RequestClass, RunnerFrame,
    StreamEvent, StreamFrame, TransferOwner, VersionTriple,
};
use plakous_tools::ToolPreempt;
use plakous_tools::internal::runner::{
    RunnerCall, RunnerCallFuture, RunnerClass, RunnerFailure, RunnerLink, RunnerOpenFuture,
    RunnerStreams, RunnerVerb, encode_preempt,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::connection::{FrameStream, fits, write_frames};
use crate::streams::{
    Credit, INITIAL_CREDIT, Outbound, StreamEnds, Window, receive_stream, send_stream,
};

/// How long the host waits on its runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientWindows {
    /// How long a call that is not an effect call waits for its answer from
    /// when it was sent, across a lost link, before its outcome is unknown
    /// (spec 01 §8.4: 30 s).
    pub read: Duration,
    /// The same for an effect call (120 s).
    pub effect: Duration,
    /// How often the host pings the runner (5 s).
    pub ping: Duration,
    /// How long the link may stay silent before it is lost (15 s).
    pub dead: Duration,
}

impl Default for ClientWindows {
    fn default() -> Self {
        Self {
            read: Duration::from_secs(30),
            effect: Duration::from_secs(120),
            ping: Duration::from_secs(5),
            dead: Duration::from_secs(15),
        }
    }
}

/// Why a connection could not be bound.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConnectError {
    /// The stream ended before the runner's hello.
    Closed,
    /// The runner's first frame was not a hello.
    Malformed(String),
    /// The runner speaks another version triple.
    VersionMismatch {
        /// The runner's triple.
        runner: VersionTriple,
    },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => {
                formatter.write_str("the runner closed the connection before its hello")
            }
            Self::Malformed(detail) => {
                write!(formatter, "the runner's hello is malformed: {detail}")
            }
            Self::VersionMismatch { runner } => write!(
                formatter,
                "the runner speaks protocol {} at {} ({})",
                runner.protocol, runner.source_revision, runner.flavor
            ),
        }
    }
}

impl std::error::Error for ConnectError {}

/// One host's link to one runner.
#[derive(Clone)]
pub struct RunnerClient {
    inner: Arc<Inner>,
}

struct Inner {
    bind: Bind,
    windows: ClientWindows,
    /// Unique to this client, so its request and control ids never collide
    /// with another host process's under the same host id.
    ids: String,
    next: AtomicU64,
    state: Mutex<State>,
    transferred: Mutex<Transferred>,
}

struct State {
    sender: Option<mpsc::UnboundedSender<HostFrame>>,
    connection: u64,
    /// Ends the current connection: a newer one, or a silent link.
    closer: CancellationToken,
    last_heard: Instant,
    instance: String,
    pending: HashMap<String, Pending>,
    transfers: HashMap<String, oneshot::Sender<RunnerFrame>>,
    streams: HashMap<u64, StreamEnds>,
}

struct Pending {
    request: Request,
    answer: oneshot::Sender<Result<Value, RunnerFailure>>,
}

/// The owner last transferred to the current runner instance, per line. Each
/// line's lock is held across its transfer so two calls of one new owner
/// transfer once, without making other lines wait. A new instance has
/// accepted no owner.
#[derive(Default)]
struct Transferred {
    instance: String,
    lines: HashMap<String, Arc<tokio::sync::Mutex<Option<LineOwner>>>>,
}

impl RunnerClient {
    /// Reads the runner's hello from `stream`, binds as `bind`, and starts
    /// answering calls.
    ///
    /// # Errors
    ///
    /// Refuses a stream without a hello, or a runner of another version.
    pub async fn connect<S>(
        stream: S,
        bind: Bind,
        windows: ClientWindows,
    ) -> Result<(Self, Hello), ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let client = Self {
            inner: Arc::new(Inner {
                bind,
                windows,
                ids: uuid::Uuid::new_v4().simple().to_string(),
                next: AtomicU64::new(0),
                state: Mutex::new(State {
                    sender: None,
                    connection: 0,
                    closer: CancellationToken::new(),
                    last_heard: Instant::now(),
                    instance: String::new(),
                    pending: HashMap::new(),
                    transfers: HashMap::new(),
                    streams: HashMap::new(),
                }),
                transferred: Mutex::default(),
            }),
        };
        let hello = client.attach(stream, false).await?;
        Ok((client, hello))
    }

    /// Binds a new connection to the same runner. Calls still waiting are
    /// replayed by key and digest when the runner is the same instance; when
    /// it restarted, their outcome is unknown and every line is transferred
    /// again before its next call. Streams do not survive a lost link.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub async fn reconnect<S>(&self, stream: S) -> Result<Hello, ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        self.attach(stream, true).await
    }

    async fn attach<S>(&self, stream: S, again: bool) -> Result<Hello, ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let mut reader = FrameStream::new(reader);
        let hello = match reader.next::<RunnerFrame>().await {
            None => return Err(ConnectError::Closed),
            Some(Ok(RunnerFrame::Hello(hello))) => hello,
            Some(Ok(_)) => return Err(ConnectError::Malformed("not a hello".into())),
            Some(Err(error)) => return Err(ConnectError::Malformed(error.to_string())),
        };
        if hello.version != self.inner.bind.version {
            return Err(ConnectError::VersionMismatch {
                runner: hello.version,
            });
        }
        let (sender, frames) = mpsc::unbounded_channel();
        let closer = CancellationToken::new();
        tokio::spawn(write_frames(writer, frames, closer.clone()));
        let _ = sender.send(HostFrame::Bind(self.inner.bind.clone()));
        let connection = {
            let mut state = lock(&self.inner.state);
            let restarted = again && state.instance != hello.instance;
            state.closer.cancel();
            state.lose(|request| restarted || !replayable(request), restarted);
            state.instance.clone_from(&hello.instance);
            state.connection += 1;
            state.closer = closer.clone();
            state.last_heard = Instant::now();
            for pending in state.pending.values() {
                let mut request = pending.request.clone();
                request.replay = true;
                let _ = sender.send(HostFrame::Request(request));
            }
            state.sender = Some(sender);
            state.connection
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.read(reader, connection, closer).await });
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.ping(connection).await });
        Ok(hello)
    }
}

impl State {
    /// Ends the current connection's transfers and streams, and fails each
    /// pending call `lost` selects as of unknown outcome.
    fn lose(&mut self, lost: impl Fn(&Request) -> bool, restarted: bool) {
        self.transfers.clear();
        for (_, mut stream) in self.streams.drain() {
            stream.close();
        }
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| lost(&pending.request))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(pending) = self.pending.remove(&id) {
                let _ = pending
                    .answer
                    .send(Err(RunnerFailure::OutcomeUnknown(if restarted {
                        "the runner restarted with the call outstanding".into()
                    } else {
                        "the link was lost with the call outstanding".into()
                    })));
            }
        }
    }
}

impl Inner {
    async fn read<R: AsyncRead + Unpin>(
        &self,
        mut reader: FrameStream<R>,
        connection: u64,
        closer: CancellationToken,
    ) {
        loop {
            let frame = tokio::select! {
                frame = reader.next::<RunnerFrame>() => frame,
                () = closer.cancelled() => break,
            };
            match frame {
                // An oversized prefix leaves the stream unframed: the link is
                // lost.
                None | Some(Err(FrameError::TooLarge)) => break,
                Some(Ok(frame)) => {
                    lock(&self.state).last_heard = Instant::now();
                    self.receive(frame);
                }
                Some(Err(_)) => {}
            }
        }
        closer.cancel();
        let mut state = lock(&self.state);
        if state.connection != connection {
            return;
        }
        state.sender = None;
        // Only keyed calls, observations and reads are replayed on a
        // reconnection: an unkeyed mutation or process may have run, and has
        // no key to ask by.
        state.lose(|request| !replayable(request), false);
    }

    /// Pings the runner while `connection` is current, and loses the link
    /// once it was silent for the dead window.
    async fn ping(&self, connection: u64) {
        let mut nonce = 0_u64;
        loop {
            tokio::time::sleep(self.windows.ping).await;
            let (silent, closer) = {
                let state = lock(&self.state);
                if state.connection != connection || state.sender.is_none() {
                    return;
                }
                (state.last_heard.elapsed(), state.closer.clone())
            };
            if silent >= self.windows.dead {
                closer.cancel();
                return;
            }
            nonce += 1;
            self.send(HostFrame::Ping { nonce });
        }
    }

    fn receive(&self, frame: RunnerFrame) {
        let control = match &frame {
            RunnerFrame::TransferAccepted { control_id }
            | RunnerFrame::StaleOwner {
                control_id: Some(control_id),
                ..
            } => Some(control_id.clone()),
            _ => None,
        };
        if let Some(control) = control {
            if let Some(waiter) = lock(&self.state).transfers.remove(&control) {
                let _ = waiter.send(frame);
            }
            return;
        }
        let (request_id, answer) = match frame {
            RunnerFrame::Stream(frame) => {
                // Taken out of the state while it runs: an abort sends a
                // close, and sending takes the state's lock.
                let stream = frame.stream;
                let Some(mut ends) = lock(&self.state).streams.remove(&stream) else {
                    return;
                };
                if ends.receive(frame) {
                    let mut state = lock(&self.state);
                    if state.sender.is_some() {
                        state.streams.insert(stream, ends);
                    } else {
                        ends.close();
                    }
                }
                return;
            }
            RunnerFrame::Result {
                request_id,
                payload,
            } => {
                self.send(HostFrame::Ack {
                    request_id: request_id.clone(),
                });
                (request_id, Ok(payload))
            }
            RunnerFrame::Error { request_id, error } => {
                self.send(HostFrame::Ack {
                    request_id: request_id.clone(),
                });
                (request_id, Err(RunnerFailure::Backend(error.error)))
            }
            RunnerFrame::NotSeen { request_id } => (
                request_id,
                Err(RunnerFailure::Unavailable(
                    "the runner never saw the call; nothing ran".into(),
                )),
            ),
            RunnerFrame::Spent { request_id } => (
                request_id,
                Err(RunnerFailure::OutcomeUnknown(
                    "the runner ran this call and its answer was already acknowledged".into(),
                )),
            ),
            RunnerFrame::StaleRequest { request_id } => (
                request_id,
                Err(RunnerFailure::Refused(
                    "the runner holds another request under this call's key".into(),
                )),
            ),
            RunnerFrame::StaleOwner {
                request_id: Some(request_id),
                ..
            } => (request_id, Err(RunnerFailure::StaleOwner)),
            RunnerFrame::Refused {
                request_id: Some(request_id),
                refusal,
            } => (
                request_id,
                Err(match refusal {
                    Refusal::Unsupported { .. } => RunnerFailure::Unsupported,
                    other => RunnerFailure::Refused(format!("{other:?}")),
                }),
            ),
            // `InProgress`: the answer still comes. `Pong` only proves the
            // link is alive. Frames without a request id refuse the
            // connection, which then ends.
            _ => return,
        };
        if let Some(pending) = lock(&self.state).pending.remove(&request_id) {
            let _ = pending.answer.send(answer);
        }
    }

    fn send(&self, frame: HostFrame) -> bool {
        lock(&self.state)
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(frame).is_ok())
    }

    fn next_id(&self, prefix: &str) -> String {
        format!(
            "{}.{prefix}{}",
            self.ids,
            self.next.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Sends `owner`'s transfer before its first call, unless it is the
    /// owner last transferred for its line.
    async fn transfer(&self, owner: &LineOwner) -> Result<(), RunnerFailure> {
        let line = {
            let instance = lock(&self.state).instance.clone();
            let mut transferred = lock(&self.transferred);
            if transferred.instance != instance {
                *transferred = Transferred {
                    instance,
                    lines: HashMap::new(),
                };
            }
            Arc::clone(transferred.lines.entry(owner.line.clone()).or_default())
        };
        let mut transferred = line.lock().await;
        if transferred.as_ref() == Some(owner) {
            return Ok(());
        }
        let control_id = self.next_id("t");
        let (waiter, answer) = oneshot::channel();
        lock(&self.state)
            .transfers
            .insert(control_id.clone(), waiter);
        let sent = self.send(HostFrame::TransferOwner(TransferOwner {
            control_id: control_id.clone(),
            from: transferred.clone(),
            to: owner.clone(),
        }));
        if !sent {
            lock(&self.state).transfers.remove(&control_id);
            return Err(RunnerFailure::Unavailable(
                "the runner is not connected".into(),
            ));
        }
        match tokio::time::timeout(self.windows.read, answer).await {
            Ok(Ok(RunnerFrame::TransferAccepted { .. })) => {
                *transferred = Some(owner.clone());
                Ok(())
            }
            Ok(Ok(_)) => Err(RunnerFailure::StaleOwner),
            Ok(Err(_)) | Err(_) => {
                lock(&self.state).transfers.remove(&control_id);
                Err(RunnerFailure::Unavailable(
                    "the runner did not answer the owner transfer".into(),
                ))
            }
        }
    }

    fn request(&self, call: &RunnerCall, owner: Option<LineOwner>) -> Request {
        let operation = Operation {
            verb: call.verb().as_str().to_owned(),
            payload: call.payload().clone(),
        };
        Request {
            request_id: self.next_id("r"),
            class: match call.class() {
                RunnerClass::Effect {
                    call_id,
                    attempt_id,
                } => RequestClass::EffectCall {
                    call_id: call_id.to_string(),
                    attempt_id: attempt_id.to_string(),
                },
                RunnerClass::Observation { job, cursor } => {
                    RequestClass::ProcessObservation { job, cursor }
                }
                RunnerClass::Control => RequestClass::OwnerControl {
                    control_id: self.next_id("c"),
                },
                RunnerClass::Unkeyed => RequestClass::BuildRead,
            },
            owner,
            deadline_ms: call.budget().map(|deadline| {
                deadline
                    .seconds()
                    .saturating_mul(1000)
                    .saturating_add(u64::from(deadline.nanoseconds() / 1_000_000))
                    .saturating_sub(plakous_model::now_unix_millis())
            }),
            digest: digest(&operation),
            replay: false,
            operation,
        }
    }

    async fn call(&self, call: RunnerCall) -> Result<Value, RunnerFailure> {
        let owner = call.owner().map(wire_owner);
        if let Some(owner) = &owner {
            self.transfer(owner).await?;
        }
        let request = self.request(&call, owner);
        let window = if matches!(request.class, RequestClass::EffectCall { .. }) {
            self.windows.effect
        } else {
            self.windows.read
        };
        let frame = HostFrame::Request(request.clone());
        if !fits(&frame) {
            return Err(RunnerFailure::Refused(
                "the call exceeds the runner frame limit".into(),
            ));
        }
        let request_id = request.request_id.clone();
        let (answer, mut answered) = oneshot::channel();
        {
            let mut state = lock(&self.state);
            let Some(sender) = state.sender.clone() else {
                return Err(RunnerFailure::Unavailable(
                    "the runner is not connected".into(),
                ));
            };
            state
                .pending
                .insert(request_id.clone(), Pending { request, answer });
            if sender.send(frame).is_err() {
                state.pending.remove(&request_id);
                return Err(RunnerFailure::Unavailable(
                    "the runner is not connected".into(),
                ));
            }
        }
        let expiry = tokio::time::sleep(window);
        tokio::pin!(expiry);
        let cancellation = call.cancellation().clone();
        let mut cancel_sent = false;
        let mut preempt = call.preempt().clone();
        let mut yields = true;
        let initial = *preempt.borrow_and_update();
        if initial != ToolPreempt::None {
            self.send_preempt(&request_id, initial);
        }
        loop {
            tokio::select! {
                answer = &mut answered => {
                    return answer.unwrap_or_else(|_| Err(RunnerFailure::OutcomeUnknown(
                        "the call was abandoned".into(),
                    )));
                }
                () = cancellation.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    if !self.send(HostFrame::Cancel { request_id: request_id.clone() }) {
                        lock(&self.state).pending.remove(&request_id);
                        return Err(RunnerFailure::OutcomeUnknown(
                            "the call was cancelled while the runner was unreachable".into(),
                        ));
                    }
                }
                changed = preempt.changed(), if yields => {
                    yields = changed.is_ok();
                    if yields {
                        let value = *preempt.borrow_and_update();
                        self.send_preempt(&request_id, value);
                    }
                }
                () = &mut expiry => {
                    lock(&self.state).pending.remove(&request_id);
                    return Err(RunnerFailure::OutcomeUnknown(
                        "the runner did not answer within the call's window".into(),
                    ));
                }
            }
        }
    }

    fn send_preempt(&self, request_id: &str, value: ToolPreempt) {
        self.send(HostFrame::Preempt {
            request_id: request_id.to_owned(),
            value: encode_preempt(value),
        });
    }

    /// Makes the call, then joins the stream its result names: the host
    /// grants the runner its first credit only once it holds the stream.
    async fn open(
        self: Arc<Self>,
        call: RunnerCall,
    ) -> Result<(Value, RunnerStreams), RunnerFailure> {
        let value = self.call(call).await?;
        let Some(stream) = value.get("stream").and_then(Value::as_u64) else {
            return Err(RunnerFailure::Refused(
                "the runner's answer names no stream".into(),
            ));
        };
        let (input, input_pump) = tokio::io::duplex(64 * 1024);
        let (output_pump, output) = tokio::io::duplex(64 * 1024);
        let (chunks, received) = mpsc::unbounded_channel();
        let credit = Credit::new(INITIAL_CREDIT);
        let window = Window::new(INITIAL_CREDIT);
        let weak = Arc::downgrade(&self);
        let out = Outbound::new(stream, move |frame: StreamFrame| {
            weak.upgrade()
                .is_some_and(|inner| inner.send(HostFrame::Stream(frame)))
        });
        {
            let mut state = lock(&self.state);
            if state.sender.is_none() {
                return Err(RunnerFailure::OutcomeUnknown(
                    "the link was lost as the stream opened".into(),
                ));
            }
            state.streams.insert(
                stream,
                StreamEnds::new(
                    chunks,
                    Arc::clone(&credit),
                    Arc::clone(&window),
                    Arc::clone(&out),
                ),
            );
        }
        tokio::spawn(send_stream(input_pump, credit, Arc::clone(&out)));
        out.send(StreamEvent::Credit {
            bytes: INITIAL_CREDIT,
        });
        tokio::spawn(receive_stream(output_pump, received, window, out));
        Ok((
            value,
            RunnerStreams {
                input: Box::pin(input),
                output: Box::pin(output),
            },
        ))
    }
}

impl RunnerLink for RunnerClient {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.call(call).await })
    }

    fn open(&self, call: RunnerCall) -> RunnerOpenFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(inner.open(call))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a request outstanding at a lost link may be sent again: a keyed
/// call or control, an observation, or a read.
fn replayable(request: &Request) -> bool {
    matches!(
        request.class,
        RequestClass::EffectCall { .. }
            | RequestClass::OwnerControl { .. }
            | RequestClass::ProcessObservation { .. }
    ) || RunnerVerb::parse(&request.operation.verb).is_some_and(RunnerVerb::is_read)
}

fn wire_owner(owner: &plakous_tools::LineOwner) -> LineOwner {
    LineOwner {
        line: owner.line().to_string(),
        generation: owner.generation(),
        branch: owner.branch().to_string(),
        fence: owner.fence(),
    }
}

/// Lowercase hex SHA-256 of the operation's canonical (RFC 8785) JSON.
pub(crate) fn digest(operation: &Operation) -> String {
    use std::fmt::Write as _;
    let bytes = serde_json_canonicalizer::to_vec(operation).unwrap_or_default();
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(&bytes) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}
