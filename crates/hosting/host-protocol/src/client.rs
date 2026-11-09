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
//! A face sends with [`WireSender`], whose intent submission never blocks:
//! an intent goes as
//! a request on the current attachment, or waits in a bounded buffer for the
//! next one while it re-attaches. The host's refusal, or no verdict within
//! [`ANSWER_DEADLINE`], comes back on the receiver with the intent
//! ([`WireReceiver::try_unsent`], [`SendError`]). [`WireSender::settle`]
//! waits for every sent intent's verdict. A session request
//! ([`WireSender::request_session`]) names a `request_id` of the client's
//! own; its answer arrives as a [`SessionAnswer`].
//!
//! One writer thread alone writes the transport, from a queue of encoded
//! lines: the reader thread and the face never block on a write, so a
//! large request does not hold up what the host says meanwhile. Intent
//! batches leave fixed control headroom; internal control overflow retains
//! the failed control without ending a progressing connection.
//!
//! What the face has not taken waits in an observation mailbox with the
//! session's own rules ([`crate::observation_channel`]): a state supersedes
//! its older value, deltas coalesce, and only what a later baseline repays
//! is dropped, its transcript then read as lost. An observation the mailbox
//! cannot hold even so is a real overflow: the client lets its attachment
//! go and attaches again. Every re-attach (that one, `resync_required`,
//! an overflow the host saw) reads each transcript the attachment held as
//! lost until its fresh baseline, and opens again the agent transcript the
//! face had open.
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

mod control;
mod writer;
use control::Control;

/// The connected sender, observation/response receiver and typed welcome.
pub type Connected<P> = (WireSender<P>, WireReceiver<P>, Welcome<P>);

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::channel::transcript_observation;
use crate::views::transcript::{NoticeScope, TranscriptRow, TranscriptScope};
use crate::wire::{
    Answer, AttachmentId, BlobAssembler, ClientFrame, HandshakeRefusal, Hello, HostFrame,
    LineReader, NodeAddress, Op, Outcome, RequestId, RequestRefusal, SnapshotAssembler, Welcome,
    WireError, decode_host_line, encode_line, take_images,
};
use crate::{
    IntentSendError, ObservationReceiver, ObservationSender, ProtocolExtension, SessionIntent,
    SessionObservation, observation_channel,
};

/// How long a sent intent waits for the host's verdict, or for an
/// attachment while the client re-attaches, before it is given back.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(30);

/// The two halves of a byte transport to a host: a pipe pair, a child's
/// stdio, a unix socket.
pub struct Transport {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    welcome_deadline: Option<(Duration, Box<dyn FnOnce() + Send>)>,
}

impl Transport {
    /// The transport that reads from `reader` and writes to `writer`.
    #[must_use]
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            welcome_deadline: None,
        }
    }

    /// Bounds only the welcome. `interrupt` must unblock the reader when
    /// the deadline expires; the watchdog is cancelled and joined before
    /// the follower starts.
    #[must_use]
    pub fn with_welcome_deadline(
        mut self,
        timeout: Duration,
        interrupt: impl FnOnce() + Send + 'static,
    ) -> Self {
        self.welcome_deadline = Some((timeout, Box::new(interrupt)));
        self
    }
}

/// Why a connection was not made.
#[derive(Debug)]
pub enum ConnectError {
    /// The welcome did not arrive within its transport's deadline.
    HandshakeTimeout(Duration),
    /// The transport failed.
    Io(std::io::Error),
    /// A frame could not be written or read.
    Wire(WireError),
    /// The host refused the handshake.
    Refused(HandshakeRefusal),
    /// The host closed before it welcomed the client, or answered the
    /// hello with something other than a welcome.
    Closed,
    /// The host serves many sessions and its welcome named none to attach.
    NoSession,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HandshakeTimeout(timeout) => write!(
                formatter,
                "the host did not answer its handshake in {} s",
                timeout.as_secs()
            ),
            Self::Io(error) => write!(formatter, "the wire transport failed: {error}"),
            Self::Wire(error) => write!(formatter, "a wire frame failed: {error}"),
            Self::Refused(refusal) => write!(formatter, "the host refused the hello: {refusal:?}"),
            Self::Closed => formatter.write_str("the host closed before welcoming the client"),
            Self::NoSession => formatter
                .write_str("the host serves many sessions and welcomed this client to none"),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Wire(error) => Some(error),
            Self::HandshakeTimeout(_) | Self::Refused(_) | Self::Closed | Self::NoSession => None,
        }
    }
}

/// Why a sent intent was not taken, with the intent.
#[derive(Debug)]
pub enum SendError<P: ProtocolExtension> {
    /// It was not taken, as a mailbox refuses an intent: the host refused
    /// it for backpressure or as one in flight, its attachment was gone or
    /// none came in time, or the host refused it otherwise
    /// ([`IntentSendError::Refused`]).
    Refused(IntentSendError<P>),
    /// The host refused it for this attachment's role or face; `row` is its
    /// row in the role table.
    RefusedForRole {
        /// The intent's row.
        row: String,
        /// The intent.
        intent: SessionIntent<P>,
    },
    /// It was sent, but no verdict came: the connection ended first, or
    /// none came within [`ANSWER_DEADLINE`]. The host may yet take it, so
    /// sending it with a new identity may run it twice. Retain these fields
    /// for an explicit [`WireSender::send_draft`] check in the same generation.
    OutcomeUnknown {
        /// The exact submitted payload.
        intent: SessionIntent<P>,
        /// Identity for an explicit same-id check, when this was a draft.
        client_id: Option<crate::wire::ClientDraftId>,
        /// Process generation in which the write began.
        host_generation: u64,
    },
}

impl<P: ProtocolExtension> SendError<P> {
    /// The intent that was sent.
    #[must_use]
    pub fn into_intent(self) -> SessionIntent<P> {
        match self {
            Self::Refused(refused) => refused.into_action(),
            Self::RefusedForRole { intent, .. } | Self::OutcomeUnknown { intent, .. } => intent,
        }
    }

    /// The host's refusal of `intent`, as the mailbox's.
    fn of(refusal: RequestRefusal, intent: SessionIntent<P>) -> Self {
        Self::Refused(match refusal {
            RequestRefusal::Backpressure => IntentSendError::Saturated(intent),
            RequestRefusal::OneInFlight => IntentSendError::Duplicate(intent),
            RequestRefusal::Disconnected | RequestRefusal::NotAttached => {
                IntentSendError::Disconnected(intent)
            }
            RequestRefusal::RefusedForRole { row } => return Self::RefusedForRole { row, intent },
            _ => IntentSendError::Refused(intent),
        })
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
        self.inner.outbox.finish();
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

struct Inner<P: ProtocolExtension> {
    /// Encoded lines for the writer thread, which alone writes the
    /// transport: a large request never holds up the reader or the face.
    outbox: Arc<writer::Queue>,
    state: Mutex<State<P>>,
    changed: Condvar,
    notify: Mutex<Option<Notify>>,
    /// What the face has not taken yet.
    observations: ObservationReceiver<P>,
}

/// An intent sent and not yet answered.
struct Sent<P: ProtocolExtension> {
    intent: SessionIntent<P>,
    at: Instant,
    ticket: Option<writer::Ticket>,
    client_id: Option<crate::wire::ClientDraftId>,
    report: bool,
    bytes: usize,
    host_generation: u64,
}

struct State<P: ProtocolExtension> {
    next_request: RequestId,
    next_session_request: u64,
    host_generation: u64,
    /// The root to attach: the welcome's, then each `moved` node's.
    root: NodeAddress,
    attachment: Option<AttachmentId>,
    previous_attachment: Option<AttachmentId>,
    /// The attach in flight.
    attaching: Option<RequestId>,
    /// The face's mailbox, until the connection ends.
    mailbox: Option<ObservationSender<P>>,
    answers: VecDeque<SessionAnswer>,
    operations: BTreeSet<RequestId>,
    selections: BTreeMap<RequestId, NodeAddress>,
    responses: VecDeque<(RequestId, Outcome<P>)>,
    /// Intents sent on an attachment, by request id, until their verdict.
    sent: BTreeMap<RequestId, Sent<P>>,
    /// Intents waiting for the next attachment, in the order sent.
    unsent: VecDeque<Sent<P>>,
    unsent_bytes: usize,
    control_retry: Option<Control>,
    control_error: Option<String>,
    /// Intents the host did not take, for the face.
    refused: VecDeque<SendError<P>>,
    /// The agent transcript the face opened on the attachment, which a
    /// re-attach opens again.
    agent: Option<String>,
    /// Session requests by their frame's id.
    sessions: BTreeMap<RequestId, String>,
    /// The root attach was refused `backpressure` once and asked again.
    attach_retried: bool,
    /// Transcripts whose fresh baseline has not arrived since the client
    /// lost their content.
    gaps: BTreeSet<TranscriptScope>,
    snapshots: SnapshotAssembler,
    blobs: BlobAssembler,
    closed: bool,
    /// Why the connection ended, once it has.
    ended: Option<String>,
}

impl<P: ProtocolExtension> State<P> {
    fn clear_control_retry(&mut self) {
        self.control_retry = None;
        self.control_error = None;
    }

    fn current_attachment(&self) -> Option<AttachmentId> {
        self.attachment.filter(|_| {
            self.attaching.is_none() && self.selections.is_empty() && self.control_retry.is_none()
        })
    }

    fn mark_gaps(&mut self) {
        self.gaps.insert(TranscriptScope::Root);
        if let Some(agent) = &self.agent {
            self.gaps.insert(TranscriptScope::Agent(agent.clone()));
        }
    }
    /// Gives back overdue intents, withdrawing queued writes before they
    /// can land. Only a batch whose write began has an unknown outcome.
    fn expire(&mut self, now: Instant) {
        let overdue = |sent: &Sent<P>| now.duration_since(sent.at) >= ANSWER_DEADLINE;
        let ids: Vec<_> = self.sent.keys().copied().collect();
        let mut suffix = false;
        for id in ids {
            if !self.sent.get(&id).is_some_and(|sent| {
                overdue(sent) || sent.ticket.as_ref().is_some_and(writer::Ticket::withdrawn)
            }) {
                continue;
            }
            if let Some(sent) = self.sent.remove(&id) {
                let error = sent.unanswered();
                suffix |= matches!(error, SendError::Refused(_));
                self.refused.push_back(error);
            }
        }
        if suffix || self.unsent.front().is_some_and(overdue) {
            for sent in self.take_unsent() {
                self.refused
                    .push_back(SendError::Refused(IntentSendError::Disconnected(
                        sent.intent,
                    )));
            }
        }
    }

    fn take_unsent(&mut self) -> VecDeque<Sent<P>> {
        self.unsent_bytes = 0;
        std::mem::take(&mut self.unsent)
    }
}

impl<P: ProtocolExtension> Sent<P> {
    fn unanswered(self) -> SendError<P> {
        if self.ticket.as_ref().is_none_or(writer::Ticket::withdraw) {
            SendError::Refused(IntentSendError::Disconnected(self.intent))
        } else {
            SendError::OutcomeUnknown {
                intent: self.intent,
                client_id: self.client_id,
                host_generation: self.host_generation,
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_draft<P: ProtocolExtension>(intent: &SessionIntent<P>) -> bool {
    matches!(
        intent,
        SessionIntent::Prompt(_)
            | SessionIntent::FollowUp(_)
            | SessionIntent::Steer(_)
            | SessionIntent::MailTopologyNode { .. }
    )
}

impl<P: ProtocolExtension> Drop for Inner<P> {
    fn drop(&mut self) {
        self.outbox.close();
    }
}

/// Connects over `transport`: says `hello`, takes the welcome and attaches
/// the session's root, then follows the host on a thread of its own.
///
/// # Errors
///
/// The transport failing, a malformed frame, the host refusing the hello,
/// or the host closing first.
pub fn connect<P>(transport: Transport, hello: Hello) -> Result<Connected<P>, ConnectError>
where
    P: ProtocolExtension,
    P::Intent: Serialize + DeserializeOwned + Send,
    P::Observation: Serialize + DeserializeOwned + Send,
{
    connect_rooted(transport, hello, None, false)
}

/// [`connect`] to a host of many sessions, whose welcome names none: the
/// client attaches `root`, a session's root node from the host's session
/// directory, whatever the welcome names.
///
/// # Errors
///
/// As [`connect`].
pub fn connect_to<P>(
    transport: Transport,
    hello: Hello,
    root: NodeAddress,
) -> Result<Connected<P>, ConnectError>
where
    P: ProtocolExtension,
    P::Intent: Serialize + DeserializeOwned + Send,
    P::Observation: Serialize + DeserializeOwned + Send,
{
    connect_rooted(transport, hello, Some(root), false)
}

/// Says hello to a session directory without attaching a node. The caller
/// selects a root from `list_sessions` or `new_session`, then uses
/// [`WireSender::attach_to`].
///
/// # Errors
///
/// As [`connect`].
pub fn connect_directory<P>(
    transport: Transport,
    hello: Hello,
) -> Result<Connected<P>, ConnectError>
where
    P: ProtocolExtension,
    P::Intent: Serialize + DeserializeOwned + Send,
    P::Observation: Serialize + DeserializeOwned + Send,
{
    connect_rooted(transport, hello, None, true)
}

fn connect_rooted<P>(
    transport: Transport,
    hello: Hello,
    root: Option<NodeAddress>,
    directory: bool,
) -> Result<Connected<P>, ConnectError>
where
    P: ProtocolExtension,
    P::Intent: Serialize + DeserializeOwned + Send,
    P::Observation: Serialize + DeserializeOwned + Send,
{
    let Transport {
        reader,
        writer,
        welcome_deadline,
    } = transport;
    let watchdog = welcome_deadline.map(|(timeout, interrupt)| {
        let (cancel, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            if wait.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
                interrupt();
                true
            } else {
                false
            }
        });
        (cancel, worker, timeout)
    });
    let mut lines = Lines::new(reader);
    let inner = Inner::new();
    let writing = Arc::downgrade(&inner);
    let outgoing = Arc::clone(&inner.outbox);
    thread::spawn(move || {
        if let Err(error) = outgoing.run(writer, || {
            if let Some(inner) = writing.upgrade() {
                inner.retry_control();
            }
        }) {
            outgoing.close();
            if let Some(inner) = writing.upgrade() {
                inner.close(format!("the transport failed to write: {error}"));
            }
        }
    });
    inner
        .write(&ClientFrame::<P>::Hello(hello))
        .map_err(ConnectError::Io)?;
    let first = lines.next_line();
    if let Some((cancel, worker, timeout)) = watchdog {
        let _ = cancel.send(());
        if worker.join().unwrap_or(true) {
            return Err(ConnectError::HandshakeTimeout(timeout));
        }
    }
    let welcome = match first {
        Some(Ok(line)) => match decode_host_line::<P>(&line).map_err(ConnectError::Wire)? {
            HostFrame::Welcome(welcome) => welcome,
            HostFrame::Refused(refusal) => return Err(ConnectError::Refused(refusal)),
            _ => return Err(ConnectError::Closed),
        },
        Some(Err(error)) => return Err(ConnectError::Wire(error)),
        None => return Err(ConnectError::Closed),
    };
    // A root the caller names is the one it asked for.
    let root = root.or_else(|| welcome.session.as_ref().map(|served| served.root.clone()));
    lock(&inner.state).host_generation = welcome.host_generation;
    if root.is_none() && !directory {
        return Err(ConnectError::NoSession);
    }
    if let Some(root) = root {
        let mut state = lock(&inner.state);
        state.root = root;
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

impl<P: ProtocolExtension> Inner<P> {
    fn new() -> Arc<Self> {
        let (mailbox, observations) = observation_channel();
        Arc::new(Self {
            outbox: Arc::new(writer::Queue::default()),
            state: Mutex::new(State {
                next_request: 1,
                next_session_request: 1,
                host_generation: 0,
                root: NodeAddress {
                    tree_id: String::new(),
                    agent_id: String::new(),
                },
                attachment: None,
                previous_attachment: None,
                attaching: None,
                mailbox: Some(mailbox),
                answers: VecDeque::new(),
                operations: BTreeSet::new(),
                selections: BTreeMap::new(),
                responses: VecDeque::new(),
                sent: BTreeMap::new(),
                unsent: VecDeque::new(),
                unsent_bytes: 0,
                control_retry: None,
                control_error: None,
                refused: VecDeque::new(),
                agent: None,
                sessions: BTreeMap::new(),
                attach_retried: false,
                // Not yet having a baseline is not having lost one.
                gaps: BTreeSet::new(),
                snapshots: SnapshotAssembler::default(),
                blobs: BlobAssembler::default(),
                closed: false,
                ended: None,
            }),
            changed: Condvar::new(),
            notify: Mutex::new(None),
            observations,
        })
    }
}

impl<P: ProtocolExtension> WireSender<P>
where
    P::Intent: Serialize,
{
    /// Sends an existing wire operation without waiting. Its response is
    /// returned by [`WireReceiver::try_response`].
    ///
    /// # Errors
    ///
    /// `WouldBlock` means control backpressure, not disconnection; `BrokenPipe`
    /// means the connection's writer ended. No frame was queued on error.
    pub fn request_operation(&self, op: Op<P>) -> std::io::Result<RequestId> {
        let inner = &self.connection.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the connection ended",
            ));
        }
        let id = state.next_request;
        state.next_request += 1;
        state.operations.insert(id);
        if let Err(error) = inner.write(&ClientFrame::Request { id, op }) {
            state.operations.remove(&id);
            return Err(error);
        }
        Ok(id)
    }

    /// Attaches a root selected from the session directory. This switches
    /// only this connection; other faces keep their attachments. Its request
    /// id identifies the answer on [`WireReceiver::try_response`]. A refused
    /// selection keeps the previous attachment and does not end the transport.
    ///
    /// # Errors
    ///
    /// `WouldBlock` means the control queue is full; other errors mean the
    /// writer stopped or encoding failed. An overflow never disconnects.
    pub fn attach_to(&self, root: NodeAddress) -> std::io::Result<RequestId> {
        let inner = &self.connection.inner;
        let mut state = lock(&inner.state);
        if let Some(Control::Detach(attachment)) = state.control_retry.as_ref() {
            let attachment = *attachment;
            inner.write_control(&mut state, Control::Detach(attachment))?;
            // If the selection cannot be queued yet, finish the old recovery.
            state.control_retry = Some(Control::Attach);
        }
        let id = state.next_request;
        state.next_request += 1;
        inner.write(&ClientFrame::Request {
            id,
            op: Op::Attach { node: root.clone() },
        })?;
        state.clear_control_retry();
        state.selections.insert(id, root);
        Ok(id)
    }
    /// Sends `intent` without waiting: on the current attachment, or on the
    /// next one while the client re-attaches. The host's verdict comes
    /// later: a refusal, or none within [`ANSWER_DEADLINE`], is given back
    /// on the receiver ([`WireReceiver::try_unsent`]).
    ///
    /// # Errors
    ///
    /// The never-sent intent: disconnected when the connection ended,
    /// saturated when the attachment-wait buffer is full, or refused when
    /// sizing fails. On an attachment, writer saturation or encoding failure
    /// arrives instead through [`WireReceiver::try_unsent`]. Attached intents
    /// are encoded once, without a preliminary size-only serialization.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected intent back so the caller can restore it"
    )]
    pub fn send(&self, intent: SessionIntent<P>) -> Result<(), SendError<P>> {
        let inner = &self.connection.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return Err(SendError::Refused(IntentSendError::Disconnected(intent)));
        }
        let bytes = if state.current_attachment().is_some() {
            0
        } else {
            drop(state);
            let bytes = match serde_json::to_vec(&intent) {
                Ok(encoded) => encoded.len(),
                Err(_) => return Err(SendError::Refused(IntentSendError::Refused(intent))),
            };
            state = lock(&inner.state);
            if state.closed {
                return Err(SendError::Refused(IntentSendError::Disconnected(intent)));
            }
            bytes
        };
        let sent = Sent {
            client_id: is_draft(&intent).then(crate::wire::ClientDraftId::new),
            intent,
            at: Instant::now(),
            ticket: None,
            report: false,
            bytes,
            host_generation: state.host_generation,
        };
        if let Some(attachment) = state.current_attachment() {
            inner.write_intent(&mut state, attachment, sent);
        } else {
            if state.unsent.len() >= writer::INTENT_FRAMES
                || state.unsent_bytes.saturating_add(bytes) > writer::INTENT_BYTES
            {
                return Err(SendError::Refused(IntentSendError::Saturated(sent.intent)));
            }
            state.unsent_bytes += sent.bytes;
            state.unsent.push_back(sent);
        }
        Ok(())
    }

    /// Explicitly submits or checks the exact same draft and `client_id`.
    /// Retain both across reconnects. Never call automatically after an
    /// unknown outcome. A changed generation returns `None`: reconcile
    /// queue/row evidence instead. The response (including `duplicate:true`
    /// or `draft_pending`) arrives on [`WireReceiver::try_response`].
    /// `None` also means no current attachment or unsent backpressure; no
    /// second saturation refusal is queued. Deadline/disconnect outcomes
    /// arrive on [`WireReceiver::try_unsent`], carrying this identity.
    #[must_use]
    pub fn send_draft(
        &self,
        host_generation: u64,
        client_id: crate::wire::ClientDraftId,
        intent: SessionIntent<P>,
    ) -> Option<RequestId> {
        let inner = &self.connection.inner;
        let mut state = lock(&inner.state);
        if state.closed
            || state.host_generation != host_generation
            || state.attaching.is_some()
            || !state.selections.is_empty()
            || state.control_retry.is_some()
            || !is_draft(&intent)
        {
            return None;
        }
        let attachment = state.attachment?;
        let generation = state.host_generation;
        inner.write_intent(
            &mut state,
            attachment,
            Sent {
                intent,
                at: Instant::now(),
                ticket: None,
                client_id: Some(client_id),
                report: true,
                bytes: 0,
                host_generation: generation,
            },
        )
    }

    /// Waits up to `timeout` for the host's verdict on every intent sent so
    /// far; whether none is left waiting.
    #[must_use]
    pub fn settle(&self, timeout: Duration) -> bool {
        let inner = &self.connection.inner;
        let deadline = Instant::now() + timeout;
        let mut state = lock(&inner.state);
        loop {
            if state.closed || (state.sent.is_empty() && state.unsent.is_empty()) {
                return true;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            state = inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Asks the host for `change` and returns the request's id; its
    /// [`SessionAnswer`] arrives on the receiver.
    ///
    /// # Errors
    ///
    /// `WouldBlock` is control backpressure, `TimedOut` means no attachment
    /// arrived in time, and `BrokenPipe` means the connection ended.
    pub fn request_session(&self, change: SessionChange) -> std::io::Result<String> {
        let request_id = {
            let mut state = lock(&self.connection.inner.state);
            let request_id = format!("request-{}", state.next_session_request);
            state.next_session_request += 1;
            request_id
        };
        let named = request_id.clone();
        self.request(request_id.clone(), move |attachment, id| {
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
        Ok(request_id)
    }

    /// Writes the frames `build` makes for the current attachment and the
    /// next request id, once there is an attachment; the session request
    /// `session`'s [`SessionAnswer`] arrives on the receiver.
    fn request(
        &self,
        session: String,
        build: impl FnOnce(AttachmentId, RequestId) -> Vec<ClientFrame<P>>,
    ) -> std::io::Result<RequestId> {
        let inner = &self.connection.inner;
        let deadline = Instant::now() + ANSWER_DEADLINE;
        let mut state = lock(&inner.state);
        let attachment = loop {
            if state.closed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the connection ended",
                ));
            }
            if let Some(attachment) = state.current_attachment() {
                break attachment;
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "no attachment before the deadline",
                    )
                })?;
            state = inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        let id = state.next_request;
        state.next_request += 1;
        state.sessions.insert(id, session);
        // Registered before it is written, so its answer always finds it;
        // written without the state lock, so the reader thread goes on
        // reading while a large request is written.
        drop(state);
        if let Err(error) = inner.write_all(&build(attachment, id)) {
            lock(&inner.state).sessions.remove(&id);
            return Err(error);
        }
        Ok(id)
    }
}

impl<P: ProtocolExtension> WireReceiver<P> {
    /// The next answer to [`WireSender::request_operation`].
    #[must_use]
    pub fn try_response(&self) -> Option<(RequestId, Outcome<P>)> {
        lock(&self.connection.inner.state).responses.pop_front()
    }

    /// A deferred internal control write hit the full control budget.
    /// The connection stays open and retries that control as space frees.
    #[must_use]
    pub fn try_control_error(&self) -> Option<String> {
        lock(&self.connection.inner.state).control_error.take()
    }

    /// Waits for an operation's next response.
    ///
    /// # Errors
    ///
    /// `Timeout` at the deadline, or `Disconnected` once the transport ends.
    pub fn recv_response(
        &self,
        timeout: Duration,
    ) -> Result<(RequestId, Outcome<P>), RecvTimeoutError> {
        let inner = &self.connection.inner;
        let deadline = Instant::now() + timeout;
        let mut state = lock(&inner.state);
        loop {
            if let Some(response) = state.responses.pop_front() {
                return Ok(response);
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or(RecvTimeoutError::Timeout)?;
            state = inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
    /// As [`crate::ObservationReceiver::try_recv`].
    ///
    /// # Errors
    ///
    /// `Empty` when none is ready, `Disconnected` once the connection is
    /// gone and everything it delivered was taken.
    pub fn try_recv(&self) -> Result<SessionObservation<P>, TryRecvError> {
        self.connection.inner.observations.try_recv()
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
        self.connection.inner.observations.recv_timeout(timeout)
    }

    /// As [`crate::ObservationReceiver::try_iter`].
    pub fn try_iter(&self) -> impl Iterator<Item = SessionObservation<P>> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// The next intent sent with this connection that the host did not
    /// take, oldest first: refused, never sent (the connection ended, or no
    /// attachment came within [`ANSWER_DEADLINE`]), a withdrawn queued
    /// suffix, or a write begun with no verdict within [`ANSWER_DEADLINE`].
    #[must_use]
    pub fn try_unsent(&self) -> Option<SendError<P>> {
        let mut state = lock(&self.connection.inner.state);
        state.expire(Instant::now());
        let refused = state.refused.pop_front();
        drop(state);
        if refused.is_some() {
            self.connection.inner.changed.notify_all();
        }
        refused
    }

    /// Installs the hook that wakes a face waiting outside this receiver:
    /// it runs on the reader thread whenever an observation or a session
    /// answer arrives, and once when the connection ends. It must not
    /// block.
    pub fn set_notify(&self, notify: impl Fn() + Send + Sync + 'static) {
        *lock(&self.connection.inner.notify) = Some(Arc::new(notify));
    }

    /// Whether `scope`'s transcript lost content (the attachment ended,
    /// or the mailbox dropped what a baseline repays) and its fresh
    /// baseline has not arrived yet.
    #[must_use]
    pub fn transcript_resnapshot_required_for(&self, scope: &TranscriptScope) -> bool {
        lock(&self.connection.inner.state).gaps.contains(scope)
            || self
                .connection
                .inner
                .observations
                .transcript_resnapshot_required_for(scope)
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

    /// Queues `line` for the writer thread; it never blocks.
    fn write_line(&self, line: &str) -> std::io::Result<()> {
        self.outbox.push(line.to_owned()).map(|_| ())
    }

    /// Ends the connection for `reason`: an intent sent and unanswered has
    /// an unknown outcome, one never sent is refused `disconnected`, the
    /// mailbox ends once drained, and the face is woken.
    fn close(&self, reason: String) {
        let mut state = lock(&self.state);
        if !state.closed {
            state.closed = true;
            state.ended = Some(reason);
        }
        state.attachment = None;
        let sent = std::mem::take(&mut state.sent);
        let unsent = state.take_unsent();
        state.refused.extend(
            sent.into_values().map(Sent::unanswered).chain(
                unsent
                    .into_iter()
                    .map(|sent| SendError::Refused(IntentSendError::Disconnected(sent.intent))),
            ),
        );
        state.mailbox = None;
        self.outbox.close();
        drop(state);
        self.changed.notify_all();
        self.wake();
    }

    /// Writes `sent` as a request on `attachment`, its images as blobs just
    /// before it, and keeps it until its verdict.
    fn write_intent(
        &self,
        state: &mut State<P>,
        attachment: AttachmentId,
        mut sent: Sent<P>,
    ) -> Option<RequestId>
    where
        P::Intent: Serialize,
    {
        let id = state.next_request;
        state.next_request += 1;
        let mut intent = sent.intent.clone();
        let client_id = sent.client_id;
        let taken = take_images(&mut intent, &format!("r{id}"));
        let mut frames: Vec<ClientFrame<P>> =
            taken.chunks.into_iter().map(ClientFrame::Blob).collect();
        frames.push(ClientFrame::Request {
            id,
            op: Op::Intent {
                client_id,
                attachment,
                intent,
                blobs: taken.ids,
            },
        });
        // A transport whose writer stopped ends the connection, which gives
        // the intent back.
        let encoded = frames
            .iter()
            .map(encode_line)
            .collect::<Result<String, _>>()
            .map_err(std::io::Error::other);
        let deadline = (!matches!(
            sent.intent,
            SessionIntent::OpenAgentTranscript { .. } | SessionIntent::CloseAgentTranscript
        ))
        .then_some(sent.at + ANSWER_DEADLINE);
        match encoded.and_then(|lines| self.outbox.reserve(lines, deadline)) {
            Ok(ticket) => {
                sent.ticket = Some(ticket);
                state.sent.insert(id, sent);
                Some(id)
            }
            Err(error) => {
                if sent.report {
                    return None;
                }
                let refusal = match error.kind() {
                    std::io::ErrorKind::WouldBlock => IntentSendError::Saturated(sent.intent),
                    std::io::ErrorKind::BrokenPipe => IntentSendError::Disconnected(sent.intent),
                    _ => IntentSendError::Refused(sent.intent),
                };
                state.refused.push_back(SendError::Refused(refusal));
                None
            }
        }
    }

    /// Asks to attach `state.root`.
    fn attach(&self, state: &mut State<P>) -> std::io::Result<()>
    where
        P::Intent: Serialize,
    {
        self.write_control(state, Control::Attach)
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
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.changed.notify_all();
                    self.retry_control();
                    self.wake();
                    continue;
                }
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
        self.close(ended);
    }

    fn wake(&self) {
        let notify = lock(&self.notify).clone();
        if let Some(notify) = notify {
            notify();
        }
    }

    /// Takes the host's answer to the root attach in flight.
    fn attached(
        &self,
        state: &mut State<P>,
        id: RequestId,
        outcome: Outcome<P>,
    ) -> std::io::Result<()>
    where
        P::Intent: Serialize,
    {
        match outcome {
            Outcome::Ok(Answer::Attached { attachment }) => {
                state.attachment = Some(attachment);
                state.previous_attachment = None;
                state.attach_retried = false;
                // The face's agent transcript, which the last attachment's
                // end closed, opens on this one first.
                if let Some(agent_id) = state.agent.clone() {
                    self.write_control(state, Control::OpenTranscript(agent_id))?;
                }
                for sent in state.take_unsent() {
                    self.write_intent(state, attachment, sent);
                }
                Ok(())
            }
            // The session left the root this client knew.
            Outcome::Refused(RequestRefusal::Moved { node }) => {
                state.root = node;
                self.attach(state)
            }
            // Once more, for a mailbox that was full a moment ago.
            Outcome::Refused(RequestRefusal::Backpressure) if !state.attach_retried => {
                state.attach_retried = true;
                self.attach(state)
            }
            Outcome::Ok(answer) => Err(std::io::Error::other(format!(
                "the host answered the attach of the session's root with {answer:?}"
            ))),
            Outcome::Refused(refusal) => {
                state.attachment = state.previous_attachment.take();
                if let Some(attachment) = state.attachment {
                    for sent in state.take_unsent() {
                        self.write_intent(state, attachment, sent);
                    }
                }
                self.deliver(
                    state,
                    SessionObservation::Row(TranscriptRow::warning(
                        NoticeScope::Host,
                        format!("attach refused: {refusal:?}"),
                    )),
                )?;
                state.responses.push_back((id, Outcome::Refused(refusal)));
                Ok(())
            }
        }
    }

    /// Queues `observation` in the face's mailbox. One the mailbox drops
    /// as a later baseline repays it leaves its transcript read as lost;
    /// any other it cannot hold is a real overflow, and the client attaches
    /// again.
    fn deliver(
        &self,
        state: &mut State<P>,
        observation: SessionObservation<P>,
    ) -> std::io::Result<bool>
    where
        P::Intent: Serialize,
    {
        let Some(mailbox) = &state.mailbox else {
            return Ok(false);
        };
        if let SessionObservation::PresentationSnapshot(snapshot) = &observation {
            state.gaps.remove(&snapshot.scope);
        }
        match mailbox.send(observation) {
            Ok(()) => Ok(true),
            Err(dropped) if transcript_observation(&dropped) => Ok(true),
            Err(_) => {
                state.mark_gaps();
                if let Some(attachment) = state.attachment.take() {
                    self.write_control(state, Control::Detach(attachment))?;
                }
                self.attach_again(state)?;
                Ok(true)
            }
        }
    }

    /// Attaches the same root again, reading every transcript the last
    /// attachment held as lost until its fresh baseline.
    fn attach_again(&self, state: &mut State<P>) -> std::io::Result<()>
    where
        P::Intent: Serialize,
    {
        state.mark_gaps();
        self.attach(state)
    }

    /// Takes the host's verdict on the intent sent as `id`; whether the
    /// face has something new.
    fn verdict(state: &mut State<P>, id: RequestId, outcome: Outcome<P>) -> bool {
        let Some(sent) = state.sent.remove(&id) else {
            return false;
        };
        if sent.report {
            state.responses.push_back((id, outcome));
            return true;
        }
        match outcome {
            Outcome::Ok(_) => {
                match &sent.intent {
                    SessionIntent::OpenAgentTranscript { agent_id } => {
                        state.agent = Some(agent_id.clone());
                    }
                    SessionIntent::CloseAgentTranscript => state.agent = None,
                    _ => {}
                }
                false
            }
            Outcome::Refused(refusal) => {
                state.refused.push_back(SendError::of(refusal, sent.intent));
                true
            }
        }
    }

    fn selected(&self, state: &mut State<P>, root: NodeAddress, outcome: &Outcome<P>)
    where
        P::Intent: Serialize,
    {
        if let Outcome::Ok(Answer::Attached { attachment }) = outcome {
            state.root = root;
            state.agent = None;
            state.attachment = Some(*attachment);
            state.previous_attachment = None;
            state.attach_retried = false;
            state.gaps.insert(TranscriptScope::Root);
        }
        if let Some(attachment) = state.attachment {
            for sent in state.take_unsent() {
                self.write_intent(state, attachment, sent);
            }
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
                if let Some(root) = state.selections.remove(&id) {
                    self.selected(state, root, &outcome);
                    state.responses.push_back((id, outcome));
                    return Ok(true);
                }
                if state.operations.remove(&id) {
                    state.responses.push_back((id, outcome));
                    return Ok(true);
                }
                if state.attaching == Some(id) {
                    state.attaching = None;
                    return self.attached(state, id, outcome).map(|()| true);
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
                Ok(Self::verdict(state, id, outcome))
            }
            HostFrame::Event {
                attachment,
                mut observation,
                blobs,
                ..
            } => {
                if state.attachment != Some(attachment) {
                    // Its images' blobs are no one's now.
                    if !blobs.is_empty() {
                        state.blobs.discard();
                    }
                    return Ok(false);
                }
                if !blobs.is_empty() {
                    state
                        .blobs
                        .restore_observation_images(&mut observation, &blobs)
                        .map_err(std::io::Error::other)?;
                }
                self.deliver(state, observation)
            }
            HostFrame::Snapshot(page) => {
                if state.attachment != Some(page.attachment) {
                    return Ok(false);
                }
                let Some(snapshot) = state.snapshots.push(page).map_err(std::io::Error::other)?
                else {
                    return Ok(false);
                };
                self.deliver(state, SessionObservation::PresentationSnapshot(snapshot))
            }
            HostFrame::Blob(chunk) => {
                state.blobs.push(chunk).map_err(std::io::Error::other)?;
                Ok(false)
            }
            HostFrame::ResyncRequired { attachment } => {
                if state.attachment != Some(attachment) {
                    return Ok(false);
                }
                self.attach_again(state).map(|()| false)
            }
            HostFrame::Moved {
                attachment, node, ..
            } => {
                if state.attachment != Some(attachment) {
                    return Ok(false);
                }
                // Another session: the face's agent view was the old one's.
                state.root = node;
                state.agent = None;
                state.gaps.insert(TranscriptScope::Root);
                self.attach(state).map(|()| false)
            }
            HostFrame::Detached { attachment } => {
                if state.attachment != Some(attachment) {
                    return Ok(false);
                }
                Err(std::io::Error::other("the host released the session"))
            }
            // This client never opens a host attachment.
            HostFrame::OptionsProgress { .. } => Ok(false),
            HostFrame::Directory { .. } => Err(std::io::Error::other(
                "the host sent a session directory this client did not watch",
            )),
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
