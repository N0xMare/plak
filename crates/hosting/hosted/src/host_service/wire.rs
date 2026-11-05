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

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use plakous_host_protocol::views::transcript::{NoticeTone, TranscriptRow};
use plakous_host_protocol::wire::{
    Answer, AttachmentId, BlobAssembler, ClientFrame, HandshakeRefusal, HostFrame, NodeAddress, Op,
    Outcome, RequestId, RequestRefusal, WIRE_VERSION, Welcome, WireError, admit_hello, decode_line,
    encode_line, page_snapshot, take_observation_images,
};
use plakous_host_protocol::{
    IntentSender, ObservationReceiver, ProtocolExtension, SessionIntent, SessionObservation,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{HostService, Outbound, TreeNodes};
use crate::error::{CliFailure, CliResult};
use served::{ServedSession, Shared};

/// How often a connection polls its input and the session's observations.
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
        session_id,
        tree_id: root.tree_id.clone(),
        root,
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
    lines: &mpsc::Receiver<Result<String, WireError>>,
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
    lines: &mpsc::Receiver<Result<String, WireError>>,
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
    blobs: BlobAssembler,
    leave: Leave,
    face: Face,
    quit_sent: bool,
    /// Whether the process was asked to stop.
    stopped: Box<dyn Fn() -> bool + Send + Sync>,
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
        Self {
            stdout,
            service,
            actions,
            attachment: None,
            resync_after_attach: None,
            blobs: BlobAssembler::default(),
            leave,
            face: Face::Remote,
            quit_sent: false,
            stopped: Box::new(|| false),
            served: None,
            generation: 0,
            mails: BTreeMap::new(),
            window: None,
            settled: Vec::new(),
            next_mail: 0,
        }
    }

    /// This connection, its transport marked `face`; a connection is a
    /// remote face until its acceptor says otherwise.
    #[must_use]
    pub const fn face(mut self, face: Face) -> Self {
        self.face = face;
        self
    }

    /// This connection, ending as a leaving client does once `stopped`
    /// turns true (the process was asked to stop).
    #[must_use]
    pub fn with_stop(mut self, stopped: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.stopped = Box::new(stopped);
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
        self.served = Some(ServedSession::shared(
            session_id.into(),
            Box::new(roots),
            self.service.clone(),
        ));
        self
    }

    /// This connection, serving the session `served` that other
    /// connections serve too, its service handle rooted at `generation`.
    fn on_served(mut self, served: Shared<P>, generation: u64) -> Self {
        self.served = Some(served);
        self.generation = generation;
        self
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
        }
    }

    /// Serves frames and observations until the session ends. A client that
    /// leaves (bye or end of input) leaves first, as its [`Leave`] says; so
    /// does a process asked to stop, and a connection whose session switched
    /// to one whose root cannot be found.
    ///
    /// # Errors
    ///
    /// A lost output.
    pub fn serve(
        mut self,
        lines: &mpsc::Receiver<Result<String, WireError>>,
        events: &ObservationReceiver<P>,
    ) -> CliResult<()> {
        let mut reading = true;
        loop {
            let mut lost = false;
            while let Ok(event) = events.try_recv() {
                lost |= !self.observe(event)?;
            }
            self.flush()?;
            if reading && (lost || (self.stopped)()) {
                reading = false;
                self.leave();
            }
            if reading {
                match lines.recv_timeout(POLL) {
                    Ok(line) => reading = self.frame(line)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        reading = false;
                        self.leave();
                    }
                }
                self.flush()?;
                continue;
            }
            match events.recv_timeout(POLL) {
                Ok(event) => {
                    self.observe(event)?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(served) = &self.served {
                        served::lock(served).end();
                    }
                    self.flush()?;
                    return write_frame(&mut self.stdout, &HostFrame::<P>::Bye);
                }
            }
        }
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
                if let Some(held) = self.attachment.take() {
                    let _ = self.service.detach(held);
                    self.unanswered();
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
            Op::Attach { node } => {
                if let Some(held) = self.attachment.take() {
                    let _ = self.service.detach(held);
                    self.unanswered();
                }
                // An attach and the replay it takes are one step against
                // the session's observations.
                let served = self.served.clone();
                let mut guard = served.as_ref().map(served::lock);
                let known = self.service.root().clone();
                if let Some(guard) = &guard {
                    self.follow(guard);
                }
                // A connection welcomed before a switch, or that missed its
                // `moved`, still names the root it knew.
                if node == known && node != *self.service.root() {
                    return refused(RequestRefusal::Moved {
                        node: self.service.root().clone(),
                    });
                }
                match self.service.attach(&node, self.face) {
                    Ok(attachment) => {
                        self.attachment = Some(attachment);
                        if let Some(guard) = guard.as_mut()
                            && guard.attached()
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
                mut intent,
                blobs,
            } => {
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
                if let Err(error) = self.blobs.restore_images(&mut intent, &blobs) {
                    return refused(RequestRefusal::Malformed {
                        detail: error.to_string(),
                    });
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
                        }
                        Some(Outcome::Ok(Answer::Accepted))
                    }
                    Err(refusal) => refused(refusal),
                }
            }
        }
    }

    /// The mail requests of an attachment that ended are not attached.
    fn unanswered(&mut self) {
        self.window = None;
        for (_, id) in std::mem::take(&mut self.mails) {
            self.settled
                .push((id, Outcome::Refused(RequestRefusal::NotAttached)));
        }
    }

    /// The response a child's mail request gets from `observation`, the
    /// answer in its window: accepted once written, or the tree's refusal.
    fn mail_answer(&mut self, observation: &SessionObservation<P>) {
        let outcome = match observation {
            SessionObservation::IntentStarted(correlation) => {
                self.window.clone_from(correlation);
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
    /// answered before the event that settles it.
    ///
    /// # Errors
    ///
    /// A lost output, or a snapshot that cannot be paged.
    pub fn flush(&mut self) -> CliResult<()> {
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
                Outbound::Moved { node } => {
                    self.attachment = None;
                    self.unanswered();
                    for (id, outcome) in std::mem::take(&mut self.settled) {
                        write_frame(&mut self.stdout, &HostFrame::<P>::Response { id, outcome })?;
                    }
                    write_frame(
                        &mut self.stdout,
                        &HostFrame::<P>::Moved { attachment, node },
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
                    mut observation,
                } => {
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

mod served;
mod session;

pub use session::WireSession;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
