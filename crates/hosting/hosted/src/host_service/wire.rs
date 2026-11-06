//! Wire v2 over one connection to a session's host service (spec 02
//! §4.4): the handshake, then one attachment at a time on the session's
//! root node or a child of its tree. Every observation reaches the
//! attachment as the service routes it, with its class; a presentation
//! snapshot goes as pages and image bytes as blobs. The transport only
//! carries lines: stdio's `plak agent --wire v2` and the web endpoint's
//! WebSocket both drive this connection.
//!
//! The session opens before the handshake is answered, so the welcome names
//! it ([`session_welcome`]) and a session that does not open is refused in a
//! v2 frame ([`refuse`] with `unavailable`). A client that sends no hello
//! within [`HELLO_DEADLINE`] is refused, so a silent peer never holds the
//! opened session. Every attach is a fresh attachment of the connection's
//! service, answered before any event of it, with its sequence from 1. When
//! the session switches to another (`/new`, `/resume`, a rebind), the
//! attachment ends with `moved {node}` naming the new session's root, and
//! the client attaches it fresh.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use plakous_host_protocol::views::transcript::{NoticeTone, TranscriptRow, TranscriptScope};
use plakous_host_protocol::wire::{
    Answer, AttachmentId, BlobAssembler, ClientFrame, HandshakeRefusal, HostFrame, NodeAddress, Op,
    Outcome, RequestId, RequestRefusal, WIRE_VERSION, Welcome, WelcomeSession, WireError,
    admit_hello, decode_line, encode_line, page_snapshot, take_observation_images,
};
use plakous_host_protocol::{
    IntentSendError, IntentSender, ObservationReceiver, ProtocolExtension, SessionIntent,
    SessionObservation, StopTicket,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{HostService, Outbound, TreeNodes};
use crate::error::{CliFailure, CliResult};
use lines::Next;
use served::{ServedSession, Shared};

/// How often a connection with a stop hook looks at it, and how often the
/// handshake looks at its own; nothing else is polled.
const POLL: Duration = Duration::from_millis(20);
/// How far an attachment may fall behind before it is dropped.
const ATTACHMENT_CAPACITY: usize = 4096;
/// How long the host waits for a client's hello before it refuses the
/// connection.
pub const HELLO_DEADLINE: Duration = Duration::from_secs(10);
/// The `unavailable` detail a transport sends before it authenticated its
/// peer: a failure's own text can name state paths, accounts and sessions.
pub const UNAVAILABLE_UNAUTHENTICATED: &str = "the session could not be opened";

/// Writes `frame` as one line and flushes it.
///
/// # Errors
///
/// A frame over the line cap, or a lost output.
fn write_frame<P: ProtocolExtension>(stdout: &mut dyn Write, frame: &HostFrame<P>) -> CliResult<()>
where
    HostFrame<P>: Serialize,
{
    let line = encode_line(frame).map_err(|error| CliFailure::new(error.to_string()))?;
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|_| CliFailure::new("stdout failed"))
}

/// The root node of the session `config` opens: its tree is the session's
/// durable topology tree (not always the session id: a rebound session's
/// tree is its lineage's), and the tree's root is named by the tree id.
#[must_use]
pub fn session_root<P: ProtocolExtension>(config: &crate::runtime::HostConfig<P>) -> NodeAddress {
    root_of(&config.state_roots, &config.session)
}

/// The root node of the session `session_id` in `roots`' store, or `None`
/// when it cannot be read.
#[must_use]
pub fn session_root_of(
    roots: &crate::agent::ProductStateRoots,
    session_id: &str,
) -> Option<NodeAddress> {
    let session = roots.session_records().load(session_id).ok()?;
    Some(root_of(roots, &session))
}

fn root_of(
    roots: &crate::agent::ProductStateRoots,
    session: &crate::sessions::SessionRecord,
) -> NodeAddress {
    let tree_id = crate::runtime::durable_topology_tree_id(
        &crate::runtime::HostRows::new(roots, roots.session_records().as_ref()),
        session,
    );
    NodeAddress {
        tree_id: tree_id.clone(),
        agent_id: tree_id,
    }
}

/// The welcome of a connection to the session `session_id` whose root is
/// `root`, from the host binary `binary_id` in its process generation
/// `host_generation`.
#[must_use]
pub fn session_welcome(
    binary_id: String,
    host_generation: u64,
    session_id: String,
    root: NodeAddress,
) -> Welcome {
    Welcome {
        proto: WIRE_VERSION,
        binary_id,
        host_generation,
        caps: Vec::new(),
        session: Some(WelcomeSession {
            session_id,
            tree_id: root.tree_id.clone(),
            root,
        }),
    }
}

/// The welcome of a connection to a host of many sessions, from the host
/// binary `binary_id` in its process generation `host_generation`: it names
/// no session, and the client finds them in the session directory.
#[must_use]
pub const fn host_welcome(binary_id: String, host_generation: u64) -> Welcome {
    Welcome {
        proto: WIRE_VERSION,
        binary_id,
        host_generation,
        caps: Vec::new(),
        session: None,
    }
}

/// Refuses the connection with `refusal` in a v2 frame; a failure before
/// the session opened is `unavailable`, never a JSONL record. An
/// `unavailable` refusal carries the failure's own text only to an
/// authenticated peer (stdio's parent process, a web client past its
/// gate); before authentication its detail is
/// [`UNAVAILABLE_UNAUTHENTICATED`].
///
/// # Errors
///
/// A lost output.
pub fn refuse<P: ProtocolExtension>(
    stdout: &mut dyn Write,
    refusal: HandshakeRefusal,
) -> CliResult<()>
where
    HostFrame<P>: Serialize,
{
    write_frame(stdout, &HostFrame::<P>::Refused(refusal))
}

/// Reads the client's hello and answers it; `false` when it was refused,
/// never came within [`HELLO_DEADLINE`] (refused `malformed`), the input
/// ended, or `stopped` turned true (the process was asked to stop).
///
/// # Errors
///
/// A lost output.
pub fn handshake<P: ProtocolExtension>(
    lines: &Lines,
    welcome: Welcome,
    stdout: &mut dyn Write,
    stopped: &dyn Fn() -> bool,
) -> CliResult<bool>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    handshake_within::<P>(lines, welcome, stdout, stopped, HELLO_DEADLINE)
}

/// [`handshake`] with the hello due within `wait`.
fn handshake_within<P: ProtocolExtension>(
    lines: &Lines,
    welcome: Welcome,
    stdout: &mut dyn Write,
    stopped: &dyn Fn() -> bool,
    wait: Duration,
) -> CliResult<bool>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    let deadline = Instant::now() + wait;
    let first = loop {
        if stopped() {
            return Ok(false);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let refusal = HandshakeRefusal::Malformed {
                detail: format!("no hello within {} ms", wait.as_millis()),
            };
            write_frame(stdout, &HostFrame::<P>::Refused(refusal))?;
            return Ok(false);
        }
        match lines.recv_timeout(remaining.min(POLL)) {
            Ok(line) => break line,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
        }
    };
    let refusal = match first.and_then(|line| decode_line::<ClientFrame<P>>(&line)) {
        Ok(ClientFrame::Hello(hello)) => match admit_hello(&hello, welcome) {
            Ok(welcome) => {
                write_frame(stdout, &HostFrame::<P>::Welcome(welcome))?;
                return Ok(true);
            }
            Err(refusal) => refusal,
        },
        Ok(_) => HandshakeRefusal::Malformed {
            detail: "the first frame must be a hello".into(),
        },
        Err(error) => HandshakeRefusal::Malformed {
            detail: error.to_string(),
        },
    };
    write_frame(stdout, &HostFrame::<P>::Refused(refusal))?;
    Ok(false)
}

/// What a connection's leaving (its bye, its end of input, a frame that
/// breaks the protocol, or its transport's end) does to the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Leave {
    /// The session quits: the connection was its only client (stdio).
    Quit,
    /// The connection detaches; the session runs on for the next one.
    Detach,
}

/// Whether a connection's transport is a local face of the host (stdio's
/// parent, an in-process face) or a remote one (the web endpoint). A remote
/// face is refused the intents only a local face may send
/// ([`ProtocolExtension::local_only`]) and is never delivered, nor replayed
/// on attach, an observation only a local face may receive
/// ([`plakous_host_protocol::ObservationExtension::local_only`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Face {
    /// A process of this user on this machine.
    Local,
    /// A client reached over the network, however authenticated.
    Remote,
}

impl Face {
    /// Whether this face may receive `observation`.
    pub(super) fn receives<P: ProtocolExtension>(
        self,
        observation: &SessionObservation<P>,
    ) -> bool {
        self == Self::Local || !observation.local_only()
    }
}

/// The connection's state after the handshake: one attachment at a time
/// on the session's host service.
pub struct WireConnection<P: ProtocolExtension, W: Write> {
    stdout: W,
    actions: IntentSender<P>,
    service: HostService<P>,
    attachment: Option<AttachmentId>,
    /// The attachment just answered `attached` that must resync at once.
    resync_after_attach: Option<AttachmentId>,
    /// The session this connection's service handle serves, when the
    /// transport named it: what its welcome names, with the handle's root.
    session: Option<String>,
    /// Every root this connection told its client of (the welcome's and
    /// each `moved` node): an attach of one the session has since left is
    /// refused `moved` with the root now.
    told: Vec<NodeAddress>,
    /// The attachment a switch last moved, owed that switch's replay on
    /// this connection's next root attach.
    moved: Option<AttachmentId>,
    blobs: BlobAssembler,
    leave: Leave,
    face: Face,
    quit_sent: bool,
    /// Whether the process was asked to stop, when something can ask it.
    stopped: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    /// The session the connection serves, when the transport named it.
    served: Option<Shared<P>>,
    /// The served session's generation this connection's service handle is
    /// rooted at.
    generation: u64,
    /// A child's mail requests, by the correlation their intents carry,
    /// answered once the mail is written.
    mails: BTreeMap<String, RequestId>,
    /// The client correlation of the attachment's current answer window.
    window: Option<String>,
    /// Responses decided outside their request's frame.
    settled: Vec<(RequestId, Outcome)>,
    next_mail: u64,
    /// The session request in flight: its frame's id, its client's id and
    /// the attachment that asked.
    session_request: Option<(RequestId, String, AttachmentId)>,
    /// The `stop_session` in flight: its frame's id, its ticket, and when
    /// it is answered without the stop.
    stop: Option<(RequestId, StopTicket, Instant)>,
    /// Who the transport admitted, when it names a login.
    login: Option<String>,
    lost_transcripts: LostTranscripts,
    /// The next root attach need not ask the session for a root baseline.
    baseline_follows: bool,
    /// Wakes [`Self::serve`], once it serves, for a publication from
    /// outside the session.
    wake: Waker,
}

/// The hook that wakes a serving connection, installed when it serves.
type Waker = std::sync::Arc<std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>>;

/// How long a stop may take before its request is answered without it: the
/// root's cancel bound and the tree's release bound, with room.
const STOP_DEADLINE: Duration = Duration::from_secs(90);

/// How many roots a connection remembers telling its client: an attach of
/// an older one is unknown, as a root never told is.
const TOLD_ROOTS: usize = 16;

/// The client correlation a session request's intent carries, before its
/// client's id.
const SESSION_REQUEST: &str = "session-request:";

/// The session request an answer window's correlation names: the
/// attachment that asked and its client's id for it. Client ids are only
/// unique per connection, so the asker tells two connections' apart.
fn session_request_of(correlation: &str) -> Option<(AttachmentId, String)> {
    let correlation = correlation
        .strip_prefix(super::ANSWER)
        .unwrap_or(correlation);
    let (requester, client) = super::attachment_of(correlation)?;
    let id = client?.strip_prefix(SESSION_REQUEST)?.to_owned();
    Some((requester, id))
}

/// The transcripts the session's mailbox lost content of that the service
/// already ended its holders for, until the session queues their fresh
/// baseline.
#[derive(Default)]
pub(super) struct LostTranscripts(BTreeSet<TranscriptScope>);

impl LostTranscripts {
    /// Ends the attachments holding a transcript the session's mailbox
    /// newly lost content of with `resync_required`: their clients attach
    /// again, and the attach asks the session for a fresh baseline.
    pub(super) fn check<P: ProtocolExtension>(
        &mut self,
        service: &HostService<P>,
        events: &ObservationReceiver<P>,
    ) {
        self.0
            .retain(|scope| events.transcript_resnapshot_required_for(scope));
        let mut newly = BTreeSet::new();
        service.resync_lost_transcripts(|scope| {
            let lost = !self.0.contains(scope) && events.transcript_resnapshot_required_for(scope);
            if lost {
                newly.insert(scope.clone());
            }
            lost
        });
        self.0.extend(newly);
    }
}

/// What a session request asks for.
enum SessionChange {
    RetryAttach,
    Resume(String),
    Rebind,
}

impl SessionChange {
    /// The op's wire name.
    const fn name(&self) -> &'static str {
        match self {
            Self::RetryAttach => "retry_attach",
            Self::Resume(_) => "resume_session",
            Self::Rebind => "rebind",
        }
    }

    /// Its row in the role table, which refuses it to a child.
    const fn row(&self) -> &'static str {
        match self {
            Self::RetryAttach => "readiness",
            Self::Resume(_) | Self::Rebind => "workflow",
        }
    }
}

/// What a wire face is shown of `observation`. A background job's
/// lifecycle carries the runtime's displayed command and retained output
/// tail, which are the model's process notices too; a face is a display
/// surface, so they reach it redacted as a process row's are (spec 04
/// §2.10). JSONL v1 projects them as they are.
fn shown<P: ProtocolExtension>(observation: SessionObservation<P>) -> SessionObservation<P> {
    use crate::jobs::display::{redacted_job_command, redacted_job_tail};
    match observation {
        SessionObservation::JobStarted {
            job_id,
            owner,
            command,
        } => SessionObservation::JobStarted {
            job_id,
            owner,
            command: redacted_job_command(&command),
        },
        SessionObservation::JobTerminal {
            job_id,
            owner,
            status,
            exit_code,
            stop_reason,
            elapsed_ms,
            output,
        } => SessionObservation::JobTerminal {
            job_id,
            owner,
            status,
            exit_code,
            stop_reason,
            elapsed_ms,
            output: redacted_job_tail(&output),
        },
        observation => observation,
    }
}

/// Whether `intent` is tree mail on a child's attachment.
const fn is_mail<P: ProtocolExtension>(intent: &SessionIntent<P>) -> bool {
    matches!(
        intent,
        SessionIntent::Prompt(_) | SessionIntent::FollowUp(_) | SessionIntent::Steer(_)
    )
}

impl<P: ProtocolExtension, W: Write> WireConnection<P, W>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// A connection over a fresh host service for the session `actions`
    /// sends to, whose root is `root`.
    pub fn new(root: NodeAddress, stdout: W, actions: IntentSender<P>) -> Self {
        let service = HostService::new(actions.clone(), root.clone(), ATTACHMENT_CAPACITY)
            .with_nodes(std::sync::Arc::new(TreeNodes::new(root.tree_id)));
        Self::on_service(service, stdout, actions, Leave::Quit)
    }

    /// A connection on a service other connections share, for the session
    /// `actions` sends to; `leave` says what its leaving does.
    pub fn on_service(
        service: HostService<P>,
        stdout: W,
        actions: IntentSender<P>,
        leave: Leave,
    ) -> Self {
        let told = vec![service.root().clone()];
        Self {
            stdout,
            service,
            actions,
            attachment: None,
            resync_after_attach: None,
            session: None,
            told,
            moved: None,
            blobs: BlobAssembler::default(),
            leave,
            face: Face::Remote,
            quit_sent: false,
            stopped: None,
            served: None,
            generation: 0,
            mails: BTreeMap::new(),
            window: None,
            settled: Vec::new(),
            next_mail: 0,
            session_request: None,
            stop: None,
            login: None,
            lost_transcripts: LostTranscripts::default(),
            baseline_follows: false,
            wake: Waker::default(),
        }
    }

    /// This connection, its transport having admitted `login`: the log of
    /// a request that acts without an attachment names it.
    #[must_use]
    pub fn identified(mut self, login: impl Into<String>) -> Self {
        self.login = Some(login.into());
        self
    }

    /// This connection, its transport marked `face`; a connection is a
    /// remote face until its acceptor says otherwise.
    #[must_use]
    pub const fn face(mut self, face: Face) -> Self {
        self.face = face;
        self
    }

    /// This connection, whose first root attach asks the session for no
    /// root baseline when `follows`: the caller takes nothing from the
    /// session before that attach, and the session publishes its root
    /// baseline when it opens, so that baseline reaches the attachment (an
    /// embedded face connected to the session it started).
    #[must_use]
    pub const fn session_baseline_follows(mut self, follows: bool) -> Self {
        self.baseline_follows = follows;
        self
    }

    /// This connection, ending as a leaving client does once `stopped`
    /// turns true (the process was asked to stop).
    #[must_use]
    pub fn with_stop(mut self, stopped: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.stopped = Some(Box::new(stopped));
        self
    }

    /// This connection, serving the session `session_id` its welcome named;
    /// `roots` finds another session's root. When the session switches to
    /// another, the attachment ends with `moved` naming that session's root,
    /// or the connection ends when the root cannot be found.
    #[must_use]
    pub fn serving(
        mut self,
        session_id: impl Into<String>,
        roots: impl Fn(&str) -> Option<NodeAddress> + Send + Sync + 'static,
    ) -> Self {
        let session_id = session_id.into();
        self.served = Some(ServedSession::shared(
            session_id.clone(),
            Box::new(roots),
            self.service.clone(),
        ));
        self.session = Some(session_id);
        self
    }

    /// This connection, serving the session `served` that other
    /// connections serve too, its service handle rooted at `generation`,
    /// that of the session `session`.
    fn on_served(mut self, served: Shared<P>, generation: u64, session: String) -> Self {
        self.served = Some(served);
        self.generation = generation;
        self.session = Some(session);
        self
    }

    /// A publisher of this connection's service for observations from
    /// outside the session ([`HostService::publisher`]); a connection that
    /// follows its session's switches publishes through it to the service
    /// it was made on only. Each publication wakes the connection once it
    /// serves.
    #[must_use]
    pub fn publisher(&self) -> super::FacePublisher<P> {
        let wake = std::sync::Arc::clone(&self.wake);
        self.service.publisher().waking(move || {
            if let Some(wake) = wake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                wake();
            }
        })
    }

    /// The session this connection serves and its root, both of the same
    /// generation: what its welcome names. `None` when the transport named
    /// no session.
    #[must_use]
    pub fn welcomed(&self) -> Option<(String, NodeAddress)> {
        self.session
            .clone()
            .map(|session| (session, self.service.root().clone()))
    }

    /// The node attachment this connection holds, if any.
    #[must_use]
    pub const fn attachment(&self) -> Option<AttachmentId> {
        self.attachment
    }

    /// Whether the connection holds an attachment, or owes one a switch's
    /// replay on its next root attach.
    #[must_use]
    pub const fn holding(&self) -> bool {
        self.attachment.is_some() || self.moved.is_some()
    }

    /// Records that the client was told of `root`, keeping the last
    /// [`TOLD_ROOTS`] of them.
    fn tell(&mut self, root: &NodeAddress) {
        if !self.told.contains(root) {
            if self.told.len() == TOLD_ROOTS {
                self.told.remove(0);
            }
            self.told.push(root.clone());
        }
    }

    /// Takes one observation, moving the connection to the session it
    /// switched to; `false` when the switch's root cannot be found and the
    /// connection must end.
    fn observe(&mut self, event: SessionObservation<P>) -> CliResult<bool> {
        let Some(served) = self.served.clone() else {
            self.service.observe(event);
            return Ok(true);
        };
        let mut guard = served::lock(&served);
        let found = guard.observe(event);
        let switched = guard.generation() != self.generation;
        drop(guard);
        if switched {
            // The attachment's `moved` goes out now, after what it held.
            self.flush()?;
            self.follow(&served::lock(&served));
        }
        Ok(found)
    }

    /// Roots this connection's service handle where the served session's
    /// is, once a switch moved it.
    fn follow(&mut self, served: &ServedSession<P>) {
        if served.generation() != self.generation {
            self.service = served.service();
            self.generation = served.generation();
            self.session = Some(served.session().to_owned());
        }
    }

    /// Serves frames and observations until the session ends. A client that
    /// leaves (bye or end of input) leaves first, as its [`Leave`] says; so
    /// does a process asked to stop, and a connection whose session switched
    /// to one whose root cannot be found.
    ///
    /// The session's observations wake the connection through `lines`
    /// ([`ObservationReceiver::set_notify`]), so it blocks on that one
    /// channel and forwards what the session says the moment it says it.
    /// Only a stop hook ([`Self::with_stop`]) is polled.
    ///
    /// # Errors
    ///
    /// A lost output.
    pub fn serve(mut self, lines: &Lines, events: &ObservationReceiver<P>) -> CliResult<()> {
        events.set_notify(lines.waker());
        // A stop's settle is not an observation. Wake the blocked read so
        // the answer does not wait for a later frame.
        self.actions.on_stop_settled(lines.waker());
        *self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(lines.waker()));
        let wait = self.stopped.is_some().then_some(POLL);
        let mut reading = true;
        loop {
            let mut lost = false;
            let mut ended = false;
            loop {
                match events.try_recv() {
                    Ok(event) => lost |= !self.observe(event)?,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
            // A session that ended on its own ends the connection while its
            // client still reads, as one its client left does.
            if ended {
                return self.end();
            }
            self.lost_transcripts.check(&self.service, events);
            self.flush()?;
            let stopped = self.stopped.as_ref().is_some_and(|stopped| stopped());
            if reading && (lost || stopped) {
                reading = false;
                self.leave();
            }
            if !reading {
                // The client is gone: only the session's end is awaited.
                lines.woken(wait);
                continue;
            }
            match lines.next(wait) {
                Next::Line(line) => {
                    reading = self.frame(line)?;
                    self.flush()?;
                }
                Next::Woken => {}
                Next::Closed => {
                    reading = false;
                    self.leave();
                }
            }
        }
    }

    /// The session ended: what it said last goes out, then `bye`.
    fn end(&mut self) -> CliResult<()> {
        if let Some(served) = &self.served {
            served::lock(served).end();
        }
        self.flush()?;
        write_frame(&mut self.stdout, &HostFrame::<P>::Bye)
    }

    /// The client left: the session quits, or the connection detaches,
    /// as its [`Leave`] says. Leaving twice does nothing more.
    pub fn leave(&mut self) {
        match self.leave {
            Leave::Quit => {
                if !self.quit_sent {
                    self.quit_sent = true;
                    let _ = self.actions.send(SessionIntent::Quit);
                }
            }
            Leave::Detach => {
                let held = self.attachment.take();
                if let Some(held) = held {
                    let _ = self.service.detach(held);
                    self.unanswered();
                }
                if let Some(served) = &self.served {
                    let mut served = served::lock(served);
                    for moved in held.into_iter().chain(self.moved.take()) {
                        served.forget(moved);
                    }
                }
            }
        }
    }

    /// Answers one client line; `false` once the client has left.
    ///
    /// # Errors
    ///
    /// A lost output.
    pub fn frame(&mut self, line: Result<String, WireError>) -> CliResult<bool> {
        let frame = match line.and_then(|line| decode_line::<ClientFrame<P>>(&line)) {
            // A second hello, or a line that is not a frame, names no
            // request to refuse: the connection ends, as a client that
            // broke the protocol.
            Ok(ClientFrame::Hello(_)) | Err(_) => {
                self.leave();
                return Ok(false);
            }
            Ok(frame) => frame,
        };
        match frame {
            ClientFrame::Hello(_) => Ok(false),
            ClientFrame::Bye => {
                self.leave();
                Ok(false)
            }
            ClientFrame::Blob(chunk) => {
                if self.blobs.push(chunk).is_err() {
                    self.leave();
                    return Ok(false);
                }
                Ok(true)
            }
            ClientFrame::Request { id, op } => {
                let named = matches!(op, Op::Intent { .. });
                let outcome = self.request(id, op);
                // Blobs precede the request that names them: any left are
                // no one's.
                if !named {
                    self.blobs.discard();
                }
                if let Some(outcome) = outcome {
                    write_frame(&mut self.stdout, &HostFrame::<P>::Response { id, outcome })?;
                }
                if let Some(attachment) = self.resync_after_attach.take() {
                    let _ = self.service.detach(attachment);
                    if self.attachment == Some(attachment) {
                        self.attachment = None;
                        self.unanswered();
                    }
                    write_frame(
                        &mut self.stdout,
                        &HostFrame::<P>::ResyncRequired { attachment },
                    )?;
                }
                Ok(true)
            }
        }
    }

    /// The request's outcome, or `None` for a child's mail, answered once
    /// the mail is written.
    fn request(&mut self, id: RequestId, op: Op<P>) -> Option<Outcome> {
        let refused = |refusal| Some(Outcome::Refused(refusal));
        match op {
            Op::Ping => Some(Outcome::Ok(Answer::Pong)),
            op @ (Op::RetryAttach { .. } | Op::ResumeSession { .. } | Op::Rebind { .. }) => {
                self.session_request(id, op)
            }
            Op::StopSession { session } => self.stop_request(id, &session),
            // A connection that serves one session has no directory.
            op @ (Op::ListSessions { .. }
            | Op::WatchSessions
            | Op::UnwatchSessions { .. }
            | Op::NewSession { .. }
            | Op::ReleaseSession { .. }) => refused(RequestRefusal::Unsupported {
                detail: format!("{} needs a host of many sessions", op.name()),
            }),
            Op::Attach { node } => {
                // An attach and the replay it takes are one step against
                // the session's observations.
                let served = self.served.clone();
                let mut guard = served.as_ref().map(served::lock);
                if let Some(held) = self.attachment.take() {
                    let _ = self.service.detach(held);
                    self.unanswered();
                    // Moved, with its `moved` not yet sent.
                    if let Some(guard) = guard.as_mut()
                        && guard.owes(held)
                        && let Some(previous) = self.moved.replace(held)
                        && previous != held
                    {
                        guard.forget(previous);
                    }
                }
                if let Some(guard) = &guard {
                    self.follow(guard);
                }
                // A connection welcomed before a switch, or told of a root
                // the session left again before it attached, names a root
                // it was told.
                if node != *self.service.root() && self.told.contains(&node) {
                    let root = self.service.root().clone();
                    self.tell(&root);
                    return refused(RequestRefusal::Moved { node: root });
                }
                let ask = !std::mem::take(&mut self.baseline_follows);
                match self.service.attach_asking(&node, self.face, ask) {
                    Ok(attachment) => {
                        self.attachment = Some(attachment);
                        // A child attach leaves the replay owed to the next
                        // root attach.
                        if node == *self.service.root()
                            && let Some(guard) = guard.as_mut()
                            && let Some(moved) = self.moved.take()
                            && guard.attached(moved, attachment)
                        {
                            self.resync_after_attach = Some(attachment);
                        }
                        Some(Outcome::Ok(Answer::Attached { attachment }))
                    }
                    Err(refusal) => refused(refusal),
                }
            }
            Op::Detach { attachment } => {
                if self.attachment != Some(attachment) {
                    return refused(RequestRefusal::NotAttached);
                }
                self.attachment = None;
                self.unanswered();
                match self.service.detach(attachment) {
                    Ok(()) => Some(Outcome::Ok(Answer::Detached)),
                    Err(refusal) => refused(refusal),
                }
            }
            Op::Intent {
                attachment,
                intent,
                blobs,
            } => self.intent_request(id, attachment, intent, &blobs),
        }
    }

    /// An intent from `attachment`: its answer, or `None` for a child's
    /// mail, answered once the mail is written.
    fn intent_request(
        &mut self,
        id: RequestId,
        attachment: AttachmentId,
        mut intent: SessionIntent<P>,
        blobs: &[String],
    ) -> Option<Outcome> {
        let refused = |refusal| Some(Outcome::Refused(refusal));
        if self.attachment != Some(attachment) {
            return refused(RequestRefusal::NotAttached);
        }
        if self.face == Face::Remote
            && matches!(&intent, SessionIntent::Extension(extension) if P::local_only(extension))
        {
            return refused(RequestRefusal::RefusedForRole {
                row: "local_face".to_owned(),
            });
        }
        if let Err(error) = self.blobs.restore_images(&mut intent, blobs) {
            return refused(RequestRefusal::Malformed {
                detail: error.to_string(),
            });
        }
        let quit = matches!(intent, SessionIntent::Quit);
        // A host's session outlives its clients: quitting a root
        // attachment detaches it, and only a stop ends the session.
        if quit && self.leave == Leave::Detach && !self.service.is_child(attachment) {
            self.attachment = None;
            self.unanswered();
            return match self.service.detach(attachment) {
                Ok(()) => Some(Outcome::Ok(Answer::Detached)),
                Err(refusal) => refused(refusal),
            };
        }
        let mail = (is_mail(&intent) && self.service.is_child(attachment)).then(|| {
            self.next_mail += 1;
            format!("mail-{}", self.next_mail)
        });
        match self
            .service
            .intent_correlated(attachment, intent, mail.as_deref())
        {
            Ok(()) => {
                if let Some(mail) = mail {
                    self.mails.insert(mail, id);
                    return None;
                }
                // A child attachment's quit is its detach.
                if !self.service.attached(attachment) {
                    self.attachment = None;
                    self.unanswered();
                    if quit {
                        return Some(Outcome::Ok(Answer::Detached));
                    }
                }
                Some(Outcome::Ok(Answer::Accepted))
            }
            Err(refusal) => refused(refusal),
        }
    }

    /// The mail requests and session request of an attachment that ended
    /// are not attached. A session request may still take effect: the
    /// `moved` it causes names it.
    fn unanswered(&mut self) {
        self.window = None;
        let session = self.session_request.take().map(|(id, _, _)| id);
        for id in std::mem::take(&mut self.mails).into_values().chain(session) {
            self.settled
                .push((id, Outcome::Refused(RequestRefusal::NotAttached)));
        }
    }

    /// Takes a session request from `attachment`, the connection's root
    /// attachment: a resume goes to the session as its own intent, answered
    /// once the session switched or refused it. A retry or a rebind reopens
    /// the session, which only a launcher that owns the session's host can
    /// do; this host serves one session, so it keeps it.
    fn session_request(&mut self, id: RequestId, op: Op<P>) -> Option<Outcome> {
        let (attachment, request_id, change) = match op {
            Op::RetryAttach {
                attachment,
                request_id,
            } => (attachment, request_id, SessionChange::RetryAttach),
            Op::ResumeSession {
                attachment,
                request_id,
                session,
            } => (attachment, request_id, SessionChange::Resume(session)),
            Op::Rebind {
                attachment,
                request_id,
                cause: _,
            } => (attachment, request_id, SessionChange::Rebind),
            // `request` answers every other op itself.
            _ => return None,
        };
        let refused = |refusal| Some(Outcome::Refused(refusal));
        if self.attachment != Some(attachment) {
            return refused(RequestRefusal::NotAttached);
        }
        if self.service.is_child(attachment) {
            return refused(RequestRefusal::RefusedForRole {
                row: change.row().to_owned(),
            });
        }
        if self.session_request.is_some() {
            return refused(RequestRefusal::OneInFlight);
        }
        let SessionChange::Resume(session) = change else {
            return refused(RequestRefusal::SessionUnavailable {
                detail: format!(
                    "{} reopens the session, which this host cannot do; it keeps the session it serves",
                    change.name()
                ),
            });
        };
        let correlation = format!("{SESSION_REQUEST}{request_id}");
        match self.service.intent_correlated(
            attachment,
            SessionIntent::ResumeSession { id: Some(session) },
            Some(&correlation),
        ) {
            Ok(()) => {
                self.session_request = Some((id, request_id, attachment));
                None
            }
            Err(refusal) => refused(refusal),
        }
    }

    /// A `stop_session` for `session`: asked of the session it names when
    /// this connection serves it, answered once the stop is durable
    /// ([`Self::settle_stop`]). It acts without an attachment, so it is
    /// logged with the connection's face and login.
    fn stop_request(&mut self, id: RequestId, session: &str) -> Option<Outcome> {
        let serves = self.served_session().as_deref() == Some(session);
        tracing::info!(
            target: "plak::host",
            face = ?self.face,
            login = self.login.as_deref().unwrap_or("-"),
            session,
            served = serves,
            "stop_session"
        );
        let refused = |refusal| Some(Outcome::Refused(refusal));
        if !serves {
            return refused(RequestRefusal::UnknownSession);
        }
        if self.stop.is_some() {
            return refused(RequestRefusal::OneInFlight);
        }
        match self.actions.request_stop() {
            Ok(ticket) => {
                self.stop = Some((id, ticket, Instant::now() + STOP_DEADLINE));
                None
            }
            Err(IntentSendError::Saturated(_)) => refused(RequestRefusal::Backpressure),
            Err(_) => refused(RequestRefusal::Disconnected),
        }
    }

    /// The session this connection serves now: the shared served session's
    /// after a switch, else the one its transport named.
    fn served_session(&self) -> Option<String> {
        match &self.served {
            Some(served) => Some(served::lock(served).session().to_owned()),
            None => self.session.clone(),
        }
    }

    /// Answers the stop in flight once the session settled it, or once its
    /// bound passed: `stopped` when it is durable, else
    /// `session_unavailable` with why.
    fn settle_stop(&mut self) {
        let Some((id, ticket, deadline)) = self.stop else {
            return;
        };
        let outcome = match self.actions.stop_settled(ticket) {
            Some(Ok(())) => Outcome::Ok(Answer::Stopped),
            Some(Err(detail)) => Outcome::Refused(RequestRefusal::SessionUnavailable { detail }),
            None if Instant::now() >= deadline => {
                Outcome::Refused(RequestRefusal::SessionUnavailable {
                    detail: "the stop did not finish in time; it may still take effect".to_owned(),
                })
            }
            None => return,
        };
        self.stop = None;
        self.settled.push((id, outcome));
    }

    /// The response a child's mail request gets from `observation`, the
    /// answer in its window: accepted once written, or the tree's refusal.
    fn mail_answer(&mut self, observation: &SessionObservation<P>) {
        let outcome = match observation {
            SessionObservation::IntentStarted(correlation) => {
                self.window.clone_from(correlation);
                return;
            }
            // A session request is refused by the error its window shows;
            // its switch answers it as its `moved` goes out.
            SessionObservation::Row(TranscriptRow::Notice(notice))
                if notice.tone == NoticeTone::Error
                    && self
                        .session_request
                        .as_ref()
                        .is_some_and(|(_, request, _)| {
                            self.window
                                .as_deref()
                                .and_then(|window| window.strip_prefix(SESSION_REQUEST))
                                == Some(request.as_str())
                        }) =>
            {
                if let Some((id, _, _)) = self.session_request.take() {
                    self.settled.push((
                        id,
                        Outcome::Refused(RequestRefusal::SessionUnavailable {
                            detail: notice.text.clone(),
                        }),
                    ));
                }
                return;
            }
            SessionObservation::DraftAccepted(_) => Outcome::Ok(Answer::Accepted),
            SessionObservation::Row(TranscriptRow::Notice(notice))
                if notice.tone == NoticeTone::Error =>
            {
                Outcome::Refused(RequestRefusal::MailRefused {
                    detail: notice.text.clone(),
                })
            }
            SessionObservation::TopologyNavigatorUnavailable { reason } => {
                Outcome::Refused(RequestRefusal::MailRefused {
                    detail: reason.clone(),
                })
            }
            _ => return,
        };
        if let Some(id) = self
            .window
            .as_ref()
            .and_then(|window| self.mails.remove(window))
        {
            self.settled.push((id, outcome));
        }
    }

    /// Writes what the service queued for the attachment: a snapshot as
    /// pages, anything else as an event after its image blobs, and a
    /// resync-required that ends the attachment. A child's mail request is
    /// answered before the event that settles it, and a stop once the
    /// session settled it.
    ///
    /// # Errors
    ///
    /// A lost output, or a snapshot that cannot be paged.
    pub fn flush(&mut self) -> CliResult<()> {
        self.settle_stop();
        for (id, outcome) in std::mem::take(&mut self.settled) {
            write_frame(&mut self.stdout, &HostFrame::<P>::Response { id, outcome })?;
        }
        let Some(attachment) = self.attachment else {
            return Ok(());
        };
        for item in self.service.take(attachment) {
            if let Outbound::Event { observation, .. } = &item {
                self.mail_answer(observation);
            }
            for (id, outcome) in std::mem::take(&mut self.settled) {
                write_frame(&mut self.stdout, &HostFrame::<P>::Response { id, outcome })?;
            }
            match item {
                Outbound::ResyncRequired => {
                    self.attachment = None;
                    self.unanswered();
                    for (id, outcome) in std::mem::take(&mut self.settled) {
                        write_frame(&mut self.stdout, &HostFrame::<P>::Response { id, outcome })?;
                    }
                    write_frame(
                        &mut self.stdout,
                        &HostFrame::<P>::ResyncRequired { attachment },
                    )?;
                }
                Outbound::Moved { node, request } => {
                    self.tell(&node);
                    self.attachment = None;
                    // A connection moved again before attaching the root is
                    // owed only the last switch's replay.
                    if let Some(previous) = self.moved.replace(attachment)
                        && previous != attachment
                        && let Some(served) = &self.served
                    {
                        served::lock(served).forget(previous);
                    }
                    // Answered only when this connection's own attachment
                    // asked: another's request may share the client id.
                    if let Some((id, _, _)) = self.session_request.take_if(|(_, asked, asker)| {
                        request
                            .as_ref()
                            .is_some_and(|(requester, id)| requester == asker && id == asked)
                    }) {
                        self.settled
                            .push((id, Outcome::Ok(Answer::Moved { node: node.clone() })));
                    }
                    let request_id = request.map(|(_, id)| id);
                    self.unanswered();
                    for (id, outcome) in std::mem::take(&mut self.settled) {
                        write_frame(&mut self.stdout, &HostFrame::<P>::Response { id, outcome })?;
                    }
                    write_frame(
                        &mut self.stdout,
                        &HostFrame::<P>::Moved {
                            attachment,
                            node,
                            request_id,
                        },
                    )?;
                    if let Some(served) = self.served.clone() {
                        self.follow(&served::lock(&served));
                    }
                }
                Outbound::Event {
                    observation: SessionObservation::PresentationSnapshot(snapshot),
                    ..
                } => {
                    let pages = page_snapshot(snapshot, attachment)
                        .map_err(|error| CliFailure::new(error.to_string()))?;
                    for page in pages {
                        write_frame(&mut self.stdout, &HostFrame::<P>::Snapshot(page))?;
                    }
                }
                Outbound::Event {
                    seq,
                    class,
                    observation,
                } => {
                    let mut observation = shown(observation);
                    let taken = take_observation_images(&mut observation, &format!("e{seq}"));
                    for chunk in taken.chunks {
                        write_frame(&mut self.stdout, &HostFrame::<P>::Blob(chunk))?;
                    }
                    write_frame(
                        &mut self.stdout,
                        &HostFrame::<P>::Event {
                            attachment,
                            seq,
                            class,
                            observation,
                            blobs: taken.ids,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
}

mod host;
mod lines;
mod served;
mod session;
mod slot;

pub use host::{Created, HostConnection, HostSessions, SessionHost};
pub use lines::{Line, LineSender, Lines, lines};
pub use session::WireSession;
pub use slot::{OpenedSession, RootResolver, SessionSlot, SlotView};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod client_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
