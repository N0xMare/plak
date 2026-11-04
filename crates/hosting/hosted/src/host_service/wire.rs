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
//! service, answered before any event of it, with its sequence from 1. A
//! connection serves the session its welcome names: when the session
//! switches to another (`/new`, `/resume`, a rebind), the connection ends
//! and the client handshakes again.

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
    let tree_id = crate::runtime::durable_topology_tree_id(
        &crate::runtime::HostRows::new(
            &config.state_roots,
            config.state_roots.session_records().as_ref(),
        ),
        &config.session,
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

/// The connection's state after the handshake: one attachment at a time
/// on the session's host service.
pub struct WireConnection<P: ProtocolExtension, W: Write> {
    stdout: W,
    actions: IntentSender<P>,
    service: HostService<P>,
    attachment: Option<AttachmentId>,
    blobs: BlobAssembler,
    quit_sent: bool,
    /// Whether the process was asked to stop.
    stopped: Box<dyn Fn() -> bool + Send + Sync>,
    /// The session the welcome named, when the transport named one.
    session: Option<String>,
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
        Self {
            stdout,
            service: HostService::new(actions.clone(), root.clone(), ATTACHMENT_CAPACITY)
                .with_nodes(std::sync::Arc::new(TreeNodes::new(root.tree_id))),
            actions,
            attachment: None,
            blobs: BlobAssembler::default(),
            quit_sent: false,
            stopped: Box::new(|| false),
            session: None,
            mails: BTreeMap::new(),
            window: None,
            settled: Vec::new(),
            next_mail: 0,
        }
    }

    /// This connection, ending as a leaving client does once `stopped`
    /// turns true (the process was asked to stop).
    #[must_use]
    pub fn with_stop(mut self, stopped: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.stopped = Box::new(stopped);
        self
    }

    /// This connection, serving the session `session_id` its welcome named:
    /// it ends when the session switches to another.
    #[must_use]
    pub fn serving(mut self, session_id: impl Into<String>) -> Self {
        self.session = Some(session_id.into());
        self
    }

    /// Takes one observation; whether it switched the session away from
    /// the one this connection serves.
    fn observe(&mut self, event: SessionObservation<P>) -> bool {
        let switched = matches!(
            &event,
            SessionObservation::Binding { session_id: Some(next), .. }
                if self.session.as_ref().is_some_and(|served| served != next)
        );
        self.service.observe(event);
        switched
    }

    /// Serves frames and observations until the session ends. A client that
    /// leaves (bye or end of input) quits the session first; so does a
    /// process asked to stop, and a connection whose session switched to
    /// another.
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
            let mut switched = false;
            while let Ok(event) = events.try_recv() {
                switched |= self.observe(event);
            }
            self.flush()?;
            if reading && (switched || (self.stopped)()) {
                reading = false;
                self.quit();
            }
            if reading {
                match lines.recv_timeout(POLL) {
                    Ok(line) => reading = self.frame(line)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        reading = false;
                        self.quit();
                    }
                }
                self.flush()?;
                continue;
            }
            match events.recv_timeout(POLL) {
                Ok(event) => {
                    self.observe(event);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.flush()?;
                    return write_frame(&mut self.stdout, &HostFrame::<P>::Bye);
                }
            }
        }
    }

    fn quit(&mut self) {
        if !self.quit_sent {
            self.quit_sent = true;
            let _ = self.actions.send(SessionIntent::Quit);
        }
    }

    /// Answers one client line; `false` once the client has left.
    fn frame(&mut self, line: Result<String, WireError>) -> CliResult<bool> {
        let frame = match line.and_then(|line| decode_line::<ClientFrame<P>>(&line)) {
            // A second hello, or a line that is not a frame, names no
            // request to refuse: the connection ends, as a client that
            // broke the protocol.
            Ok(ClientFrame::Hello(_)) | Err(_) => {
                self.quit();
                return Ok(false);
            }
            Ok(frame) => frame,
        };
        match frame {
            ClientFrame::Hello(_) => Ok(false),
            ClientFrame::Bye => {
                self.quit();
                Ok(false)
            }
            ClientFrame::Blob(chunk) => {
                if self.blobs.push(chunk).is_err() {
                    self.quit();
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
                match self.service.attach(&node) {
                    Ok(attachment) => {
                        self.attachment = Some(attachment);
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
    fn flush(&mut self) -> CliResult<()> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
