//! A wire v2 client (spec 02 §4.4): the face API over [`ClientFrame`] and
//! [`HostFrame`] lines on any byte transport.
//!
//! [`connect`] says hello, takes the welcome and attaches the session's
//! root. From then on a reader thread owns the transport's read half: it
//! follows the host as a face does in process, so what the face receives is
//! the session's observations in the receiver's own shape
//! ([`WireReceiver`]). A `moved` attaches the root it names, and a
//! `resync_required` attaches the root again; either marks the root
//! transcript as needing its fresh baseline until that baseline's snapshot
//! has arrived ([`WireReceiver::transcript_resnapshot_required_for`]).
//! Snapshot pages are assembled and event images restored before the face
//! sees them.
//!
//! A face sends with [`WireSender`]: an intent goes as a request on the
//! current attachment, waiting for one while the client re-attaches, and
//! its refusal comes back as the [`IntentSendError`] a mailbox would give,
//! with the intent. A session request ([`WireSender::request_session`])
//! names a `request_id` of the client's own; its answer arrives as a
//! [`SessionAnswer`].
//!
//! The client is one connection: when the transport ends, the receiver
//! reports `Disconnected` once it is drained, and a new connection is a
//! new [`connect`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::views::transcript::TranscriptScope;
use crate::wire::{
    Answer, AttachmentId, BlobAssembler, ClientFrame, HandshakeRefusal, Hello, HostFrame,
    LineReader, NodeAddress, Op, Outcome, RequestId, RequestRefusal, SnapshotAssembler, Welcome,
    WireError, decode_host_line, encode_line, take_images,
};
use crate::{IntentSendError, ProtocolExtension, SessionIntent, SessionObservation};

/// How long a send waits for the host to answer it, or for an attachment
/// while the client re-attaches.
const ANSWER_DEADLINE: Duration = Duration::from_secs(30);

/// The two halves of a byte transport to a host: a pipe pair, a child's
/// stdio, a unix socket.
pub struct Transport {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

impl Transport {
    /// The transport that reads from `reader` and writes to `writer`.
    #[must_use]
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
        }
    }
}

/// Why a connection was not made.
#[derive(Debug)]
pub enum ConnectError {
    /// The transport failed.
    Io(std::io::Error),
    /// A frame could not be written or read.
    Wire(WireError),
    /// The host refused the handshake.
    Refused(HandshakeRefusal),
    /// The host closed before it welcomed the client, or answered the
    /// hello with something other than a welcome.
    Closed,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "the wire transport failed: {error}"),
            Self::Wire(error) => write!(formatter, "a wire frame failed: {error}"),
            Self::Refused(refusal) => write!(formatter, "the host refused the hello: {refusal:?}"),
            Self::Closed => formatter.write_str("the host closed before welcoming the client"),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Wire(error) => Some(error),
            Self::Refused(_) | Self::Closed => None,
        }
    }
}

/// A session change a client asks the host for (spec 02 §4.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionChange {
    /// Re-resolve the binding and open the session again.
    RetryAttach,
    /// Switch to the user's session with this id.
    Resume(String),
    /// Carry the history into a new session, for this reason.
    Rebind(String),
}

/// The settled answer to a session request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAnswer {
    /// The client's id for the request.
    pub request_id: String,
    /// The root of the session it switched to, or why it did not.
    pub outcome: Result<NodeAddress, RequestRefusal>,
}

/// What a face sends with; its clones are the same connection.
pub struct WireSender<P: ProtocolExtension> {
    connection: Arc<Connection<P>>,
}

impl<P: ProtocolExtension> Clone for WireSender<P> {
    fn clone(&self) -> Self {
        Self {
            connection: Arc::clone(&self.connection),
        }
    }
}

/// What a face receives: the session's observations, as the host's
/// attachment of this connection takes them.
pub struct WireReceiver<P: ProtocolExtension> {
    connection: Arc<Connection<P>>,
}

/// The client's side of the connection; it says goodbye when the last
/// half goes.
struct Connection<P: ProtocolExtension> {
    inner: Arc<Inner<P>>,
}

/// The `bye` frame's line, written without the extension's bounds.
const BYE_LINE: &str = "{\"frame\":\"bye\"}\n";

impl<P: ProtocolExtension> Drop for Connection<P> {
    fn drop(&mut self) {
        let _ = self.inner.write_line(BYE_LINE);
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

struct Inner<P: ProtocolExtension> {
    writer: Mutex<Box<dyn Write + Send>>,
    state: Mutex<State<P>>,
    changed: Condvar,
    notify: Mutex<Option<Notify>>,
}

struct State<P: ProtocolExtension> {
    next_request: RequestId,
    next_session_request: u64,
    /// The root to attach: the welcome's, then each `moved` node's.
    root: NodeAddress,
    attachment: Option<AttachmentId>,
    /// The attach in flight.
    attaching: Option<RequestId>,
    queue: VecDeque<SessionObservation<P>>,
    answers: VecDeque<SessionAnswer>,
    /// Requests whose sender waits for their outcome.
    waiting: BTreeMap<RequestId, mpsc::Sender<Outcome>>,
    /// Session requests by their frame's id.
    sessions: BTreeMap<RequestId, String>,
    /// Transcripts whose fresh baseline has not arrived since the client
    /// lost their content.
    gaps: BTreeSet<TranscriptScope>,
    snapshots: SnapshotAssembler,
    blobs: BlobAssembler,
    closed: bool,
    /// Why the connection ended, once it has.
    ended: Option<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Connects over `transport`: says `hello`, takes the welcome and attaches
/// the session's root, then follows the host on a thread of its own.
///
/// # Errors
///
/// The transport failing, a malformed frame, the host refusing the hello,
/// or the host closing first.
pub fn connect<P>(
    transport: Transport,
    hello: Hello,
) -> Result<(WireSender<P>, WireReceiver<P>, Welcome), ConnectError>
where
    P: ProtocolExtension,
    P::Intent: Serialize + DeserializeOwned + Send,
    P::Observation: Serialize + DeserializeOwned + Send,
{
    let Transport { reader, writer } = transport;
    let mut lines = Lines::new(reader);
    let inner = Arc::new(Inner {
        writer: Mutex::new(writer),
        state: Mutex::new(State {
            next_request: 1,
            next_session_request: 1,
            root: NodeAddress {
                tree_id: String::new(),
                agent_id: String::new(),
            },
            attachment: None,
            attaching: None,
            queue: VecDeque::new(),
            answers: VecDeque::new(),
            waiting: BTreeMap::new(),
            sessions: BTreeMap::new(),
            gaps: BTreeSet::from([TranscriptScope::Root]),
            snapshots: SnapshotAssembler::default(),
            blobs: BlobAssembler::default(),
            closed: false,
            ended: None,
        }),
        changed: Condvar::new(),
        notify: Mutex::new(None),
    });
    inner
        .write(&ClientFrame::<P>::Hello(hello))
        .map_err(ConnectError::Io)?;
    let welcome = match lines.next_line() {
        Some(Ok(line)) => match decode_host_line::<P>(&line).map_err(ConnectError::Wire)? {
            HostFrame::Welcome(welcome) => welcome,
            HostFrame::Refused(refusal) => return Err(ConnectError::Refused(refusal)),
            _ => return Err(ConnectError::Closed),
        },
        Some(Err(error)) => return Err(ConnectError::Wire(error)),
        None => return Err(ConnectError::Closed),
    };
    {
        let mut state = lock(&inner.state);
        state.root = welcome.root.clone();
        inner.attach(&mut state).map_err(ConnectError::Io)?;
    }
    let follower = Arc::clone(&inner);
    thread::spawn(move || follower.follow(&mut lines));
    let connection = Arc::new(Connection { inner });
    Ok((
        WireSender {
            connection: Arc::clone(&connection),
        },
        WireReceiver { connection },
        welcome,
    ))
}

impl<P: ProtocolExtension> WireSender<P>
where
    P::Intent: Serialize,
{
    /// Sends `intent` on the current attachment and waits for the host to
    /// take it; while the client re-attaches it waits for the attachment.
    ///
    /// # Errors
    ///
    /// The intent back, as a mailbox refuses it: [`IntentSendError::Saturated`]
    /// for backpressure, [`IntentSendError::Duplicate`] for one in flight,
    /// [`IntentSendError::Disconnected`] once the connection is gone or no
    /// attachment came in time, and [`IntentSendError::Refused`] for the
    /// host's refusal of it for this connection's role or face.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected intent back so the caller can restore it"
    )]
    pub fn send(&self, intent: SessionIntent<P>) -> Result<(), IntentSendError<P>> {
        let Some((id, Some(answer))) = self.request(None, |attachment, id| {
            let mut sent = intent.clone();
            let taken = take_images(&mut sent, &format!("r{id}"));
            let mut frames: Vec<ClientFrame<P>> =
                taken.chunks.into_iter().map(ClientFrame::Blob).collect();
            frames.push(ClientFrame::Request {
                id,
                op: Op::Intent {
                    attachment,
                    intent: sent,
                    blobs: taken.ids,
                },
            });
            frames
        }) else {
            return Err(IntentSendError::Disconnected(intent));
        };
        let outcome = answer.recv_timeout(ANSWER_DEADLINE);
        lock(&self.connection.inner.state).waiting.remove(&id);
        match outcome {
            Ok(Outcome::Ok(_)) => Ok(()),
            Ok(Outcome::Refused(refusal)) => Err(match refusal {
                RequestRefusal::Backpressure => IntentSendError::Saturated(intent),
                RequestRefusal::OneInFlight => IntentSendError::Duplicate(intent),
                RequestRefusal::Disconnected | RequestRefusal::NotAttached => {
                    IntentSendError::Disconnected(intent)
                }
                _ => IntentSendError::Refused(intent),
            }),
            Err(_) => Err(IntentSendError::Disconnected(intent)),
        }
    }

    /// Asks the host for `change` and returns the request's id; its
    /// [`SessionAnswer`] arrives on the receiver. `None` once the
    /// connection is gone or no attachment came in time.
    #[must_use]
    pub fn request_session(&self, change: SessionChange) -> Option<String> {
        let request_id = {
            let mut state = lock(&self.connection.inner.state);
            let request_id = format!("request-{}", state.next_session_request);
            state.next_session_request += 1;
            request_id
        };
        let named = request_id.clone();
        self.request(Some(request_id.clone()), move |attachment, id| {
            let op = match change {
                SessionChange::RetryAttach => Op::RetryAttach {
                    attachment,
                    request_id: named,
                },
                SessionChange::Resume(session) => Op::ResumeSession {
                    attachment,
                    request_id: named,
                    session,
                },
                SessionChange::Rebind(cause) => Op::Rebind {
                    attachment,
                    request_id: named,
                    cause,
                },
            };
            vec![ClientFrame::Request { id, op }]
        })?;
        Some(request_id)
    }

    /// Writes the frames `build` makes for the current attachment and the
    /// next request id, once there is an attachment. The request waits for
    /// its outcome on the returned receiver, or, as the session request
    /// `session`, for its [`SessionAnswer`]. `None` when the connection is
    /// gone or no attachment came in time.
    fn request(
        &self,
        session: Option<String>,
        build: impl FnOnce(AttachmentId, RequestId) -> Vec<ClientFrame<P>>,
    ) -> Option<(RequestId, Option<mpsc::Receiver<Outcome>>)> {
        let inner = &self.connection.inner;
        let deadline = Instant::now() + ANSWER_DEADLINE;
        let mut state = lock(&inner.state);
        let attachment = loop {
            if state.closed {
                return None;
            }
            if let Some(attachment) = state.attachment {
                break attachment;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            state = inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        let id = state.next_request;
        state.next_request += 1;
        let answered = if let Some(request_id) = session {
            state.sessions.insert(id, request_id);
            None
        } else {
            let (answer, answered) = mpsc::channel();
            state.waiting.insert(id, answer);
            Some(answered)
        };
        // Registered before it is written, so its answer always finds it;
        // written without the state lock, so the reader thread goes on
        // reading while a large request is written.
        drop(state);
        if inner.write_all(&build(attachment, id)).is_err() {
            let mut state = lock(&inner.state);
            state.waiting.remove(&id);
            state.sessions.remove(&id);
            return None;
        }
        Some((id, answered))
    }
}

impl<P: ProtocolExtension> WireReceiver<P> {
    /// As [`crate::ObservationReceiver::try_recv`].
    ///
    /// # Errors
    ///
    /// `Empty` when none is ready, `Disconnected` once the connection is
    /// gone and everything it delivered was taken.
    pub fn try_recv(&self) -> Result<SessionObservation<P>, TryRecvError> {
        let mut state = lock(&self.connection.inner.state);
        match state.queue.pop_front() {
            Some(observation) => Ok(observation),
            None if state.closed => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    /// As [`crate::ObservationReceiver::recv_timeout`].
    ///
    /// # Errors
    ///
    /// `Timeout` when the deadline passes first, `Disconnected` as for
    /// [`Self::try_recv`].
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SessionObservation<P>, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.connection.inner.state);
        loop {
            if let Some(observation) = state.queue.pop_front() {
                return Ok(observation);
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Err(RecvTimeoutError::Timeout);
            };
            state = self
                .connection
                .inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// As [`crate::ObservationReceiver::try_iter`].
    pub fn try_iter(&self) -> impl Iterator<Item = SessionObservation<P>> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// Installs the hook that wakes a face waiting outside this receiver:
    /// it runs on the reader thread whenever an observation or a session
    /// answer arrives, and once when the connection ends. It must not
    /// block.
    pub fn set_notify(&self, notify: impl Fn() + Send + Sync + 'static) {
        *lock(&self.connection.inner.notify) = Some(Arc::new(notify));
    }

    /// Whether `scope`'s transcript lost content (the attachment ended
    /// with `moved` or `resync_required`) and its fresh baseline has not
    /// arrived yet.
    #[must_use]
    pub fn transcript_resnapshot_required_for(&self, scope: &TranscriptScope) -> bool {
        lock(&self.connection.inner.state).gaps.contains(scope)
    }

    /// Why the connection ended, once it has.
    #[must_use]
    pub fn ended(&self) -> Option<String> {
        lock(&self.connection.inner.state).ended.clone()
    }

    /// The next settled session request, if any.
    #[must_use]
    pub fn try_session_answer(&self) -> Option<SessionAnswer> {
        lock(&self.connection.inner.state).answers.pop_front()
    }
}

impl<P: ProtocolExtension> Inner<P> {
    fn write(&self, frame: &ClientFrame<P>) -> std::io::Result<()>
    where
        ClientFrame<P>: Serialize,
    {
        self.write_line(&encode_line(frame).map_err(std::io::Error::other)?)
    }

    /// Writes `frames` together: a request's blobs stay just before it.
    fn write_all(&self, frames: &[ClientFrame<P>]) -> std::io::Result<()>
    where
        ClientFrame<P>: Serialize,
    {
        let lines = frames
            .iter()
            .map(encode_line)
            .collect::<Result<String, _>>()
            .map_err(std::io::Error::other)?;
        self.write_line(&lines)
    }

    fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut writer = lock(&self.writer);
        writer.write_all(line.as_bytes())?;
        writer.flush()
    }

    /// Asks to attach `state.root`.
    fn attach(&self, state: &mut State<P>) -> std::io::Result<()>
    where
        ClientFrame<P>: Serialize,
    {
        let id = state.next_request;
        state.next_request += 1;
        state.attachment = None;
        state.attaching = Some(id);
        self.write(&ClientFrame::Request {
            id,
            op: Op::Attach {
                node: state.root.clone(),
            },
        })
    }

    /// Reads and follows the host until the transport ends.
    fn follow(&self, lines: &mut Lines)
    where
        P::Intent: Serialize + DeserializeOwned,
        P::Observation: DeserializeOwned,
    {
        let mut ended = "the host closed the transport".to_owned();
        while let Some(line) = lines.next_line() {
            // A line the client cannot read leaves the connection.
            let frame = match line.and_then(|line| decode_host_line::<P>(&line)) {
                Ok(frame) => frame,
                Err(error) => {
                    ended = format!("an unreadable host frame: {error}");
                    break;
                }
            };
            let taken = {
                let mut state = lock(&self.state);
                self.take(&mut state, frame)
            };
            let delivered = match taken {
                Ok(delivered) => delivered,
                Err(error) => {
                    ended = error.to_string();
                    break;
                }
            };
            self.changed.notify_all();
            if delivered {
                self.wake();
            }
        }
        let mut state = lock(&self.state);
        state.closed = true;
        state.ended = Some(ended);
        state.attachment = None;
        // Every waiting sender sees its answer channel close.
        state.waiting.clear();
        drop(state);
        self.changed.notify_all();
        self.wake();
    }

    fn wake(&self) {
        let notify = lock(&self.notify).clone();
        if let Some(notify) = notify {
            notify();
        }
    }

    /// Takes one host frame; whether the face has something new. `Err` when
    /// the connection must end.
    fn take(&self, state: &mut State<P>, frame: HostFrame<P>) -> std::io::Result<bool>
    where
        P::Intent: Serialize,
    {
        match frame {
            HostFrame::Response { id, outcome } => {
                if state.attaching == Some(id) {
                    state.attaching = None;
                    return match outcome {
                        Outcome::Ok(Answer::Attached { attachment }) => {
                            state.attachment = Some(attachment);
                            Ok(false)
                        }
                        // The session left the root this client knew.
                        Outcome::Refused(RequestRefusal::Moved { node }) => {
                            state.root = node;
                            self.attach(state).map(|()| false)
                        }
                        Outcome::Ok(_) | Outcome::Refused(_) => Err(std::io::Error::other(
                            "the host refused the attach of the session's root",
                        )),
                    };
                }
                if let Some(request_id) = state.sessions.remove(&id) {
                    state.answers.push_back(SessionAnswer {
                        request_id,
                        outcome: match outcome {
                            Outcome::Ok(Answer::Moved { node }) => Ok(node),
                            Outcome::Ok(_) => Err(RequestRefusal::Malformed {
                                detail: "a session request answered as another".to_owned(),
                            }),
                            Outcome::Refused(refusal) => Err(refusal),
                        },
                    });
                    return Ok(true);
                }
                if let Some(waiting) = state.waiting.remove(&id) {
                    let _ = waiting.send(outcome);
                }
                Ok(false)
            }
            HostFrame::Event {
                attachment,
                mut observation,
                blobs,
                ..
            } => {
                if state.attachment != Some(attachment) {
                    return Ok(false);
                }
                if !blobs.is_empty() {
                    state
                        .blobs
                        .restore_observation_images(&mut observation, &blobs)
                        .map_err(std::io::Error::other)?;
                }
                if let SessionObservation::PresentationSnapshot(snapshot) = &observation {
                    state.gaps.remove(&snapshot.scope);
                }
                state.queue.push_back(observation);
                Ok(true)
            }
            HostFrame::Snapshot(page) => {
                if state.attachment != Some(page.attachment) {
                    return Ok(false);
                }
                let Some(snapshot) = state.snapshots.push(page).map_err(std::io::Error::other)?
                else {
                    return Ok(false);
                };
                state.gaps.remove(&snapshot.scope);
                state
                    .queue
                    .push_back(SessionObservation::PresentationSnapshot(snapshot));
                Ok(true)
            }
            HostFrame::Blob(chunk) => {
                state.blobs.push(chunk).map_err(std::io::Error::other)?;
                Ok(false)
            }
            HostFrame::ResyncRequired { attachment } => {
                if state.attachment != Some(attachment) {
                    return Ok(false);
                }
                state.gaps.insert(TranscriptScope::Root);
                self.attach(state).map(|()| false)
            }
            HostFrame::Moved {
                attachment, node, ..
            } => {
                if state.attachment != Some(attachment) {
                    return Ok(false);
                }
                state.root = node;
                state.gaps.insert(TranscriptScope::Root);
                self.attach(state).map(|()| false)
            }
            HostFrame::Bye | HostFrame::Welcome(_) | HostFrame::Refused(_) => {
                Err(std::io::Error::other("the host closed the connection"))
            }
        }
    }
}

/// The lines of a byte stream.
struct Lines {
    reader: Box<dyn Read + Send>,
    split: LineReader,
    ready: VecDeque<Result<String, WireError>>,
    buffer: Vec<u8>,
}

impl Lines {
    fn new(reader: Box<dyn Read + Send>) -> Self {
        Self {
            reader,
            split: LineReader::default(),
            ready: VecDeque::new(),
            buffer: vec![0_u8; 64 * 1024],
        }
    }

    /// The next line; `None` once the stream ended or failed.
    fn next_line(&mut self) -> Option<Result<String, WireError>> {
        loop {
            if let Some(line) = self.ready.pop_front() {
                return Some(line);
            }
            match self.reader.read(&mut self.buffer) {
                Ok(0) | Err(_) => return None,
                Ok(read) => self.ready.extend(self.split.push(&self.buffer[..read])),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
