//! `plak agent --wire v2` (hidden): wire v2 on stdio (spec 02 §4.4) over
//! the session this process opens, through its host service
//! ([`HostService`]): one connection, one attachment at a time, on the
//! session's root node or a child of its tree. Every observation reaches the attachment as the
//! service routes it, with its class; a presentation snapshot goes as pages
//! and image bytes as blobs.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use plakous_host_protocol::views::transcript::{NoticeTone, TranscriptRow};
use plakous_host_protocol::wire::{
    Answer, AttachmentId, BlobAssembler, ClientFrame, HandshakeRefusal, HostFrame, LineReader,
    NodeAddress, Op, Outcome, RequestId, RequestRefusal, WIRE_VERSION, Welcome, WireError,
    admit_hello, decode_line, encode_line, page_snapshot, take_observation_images,
};
use plakous_host_protocol::{ObservationReceiver, SessionIntent, SessionObservation};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::host_service::{HostService, Outbound, TreeNodes};
use plakous_hosted::runtime::{HostConfig, spawn_host_worker};

use crate::surface_protocol::{PlakExtension, UserActionSender};

type Client = ClientFrame<PlakExtension>;
type Host = HostFrame<PlakExtension>;

const POLL: Duration = Duration::from_millis(20);
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// The only attachment this adapter makes.
/// How far an attachment may fall behind before it is dropped.
const ATTACHMENT_CAPACITY: usize = 4096;

/// Runs wire v2 on `input` and `stdout` over the session `config` opens.
///
/// # Errors
///
/// A refused handshake, a session that does not open, or a lost stdout.
pub(crate) fn run_agent_wire(
    config: HostConfig<PlakExtension>,
    input: impl Read + Send + 'static,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let lines = read_lines(input);
    let welcome = Welcome {
        proto: WIRE_VERSION,
        binary_id: crate::build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
        host_generation: host_generation(),
        caps: Vec::new(),
    };
    if !handshake(&lines, welcome, stdout)? {
        return Ok(ProcessOutcome::Success);
    }
    let tree_id =
        crate::topology_product::durable_topology_tree_id(&config.state_roots, &config.session);
    // The root node of a session's tree is named by the tree id.
    let root = NodeAddress {
        tree_id: tree_id.clone(),
        agent_id: tree_id,
    };
    let (actions, events, worker) = spawn_host_worker(config)?;
    let served = Adapter::new(root, stdout, actions.clone()).serve(&lines, &events);
    // A connection that failed still ends its session.
    let _ = actions.send(SessionIntent::Quit);
    let deadline = Instant::now() + JOIN_TIMEOUT;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err(CliFailure::new(
                "plak agent host worker exceeded its shutdown deadline",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| CliFailure::new("plak agent host worker failed"))??;
    served.map(|()| ProcessOutcome::Success)
}

/// This process's generation: it differs on every start.
fn host_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// Lines from `input`, read on a thread of their own; the channel closes at
/// the end of input.
fn read_lines(mut input: impl Read + Send + 'static) -> mpsc::Receiver<Result<String, WireError>> {
    let (sender, lines) = mpsc::sync_channel(64);
    thread::spawn(move || {
        let mut reader = LineReader::default();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = match input.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            for line in reader.push(&buffer[..read]) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        }
    });
    lines
}

fn write_frame(stdout: &mut dyn Write, frame: &Host) -> CliResult<()> {
    let line = encode_line(frame).map_err(|error| CliFailure::new(error.to_string()))?;
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|_| CliFailure::new("stdout failed"))
}

/// Reads the client's hello and answers it; `false` when it was refused
/// or never came.
pub(crate) fn handshake(
    lines: &mpsc::Receiver<Result<String, WireError>>,
    welcome: Welcome,
    stdout: &mut dyn Write,
) -> CliResult<bool> {
    let Ok(first) = lines.recv() else {
        return Ok(false);
    };
    let refusal = match first.and_then(|line| decode_line::<Client>(&line)) {
        Ok(Client::Hello(hello)) => match admit_hello(&hello, welcome) {
            Ok(welcome) => {
                write_frame(stdout, &Host::Welcome(welcome))?;
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
    write_frame(stdout, &Host::Refused(refusal))?;
    Ok(false)
}

/// The connection's state after the handshake: one attachment at a time
/// on the session's host service.
pub(crate) struct Adapter<'a> {
    stdout: &'a mut dyn Write,
    actions: UserActionSender,
    service: HostService<PlakExtension>,
    attachment: Option<AttachmentId>,
    blobs: BlobAssembler,
    quit_sent: bool,
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
const fn is_mail(intent: &SessionIntent<PlakExtension>) -> bool {
    matches!(
        intent,
        SessionIntent::Prompt(_) | SessionIntent::FollowUp(_) | SessionIntent::Steer(_)
    )
}

impl<'a> Adapter<'a> {
    pub(crate) fn new(
        root: NodeAddress,
        stdout: &'a mut dyn Write,
        actions: UserActionSender,
    ) -> Self {
        Self {
            stdout,
            service: HostService::new(actions.clone(), root.clone(), ATTACHMENT_CAPACITY)
                .with_nodes(std::sync::Arc::new(TreeNodes::new(root.tree_id))),
            actions,
            attachment: None,
            blobs: BlobAssembler::default(),
            quit_sent: false,
            mails: BTreeMap::new(),
            window: None,
            settled: Vec::new(),
            next_mail: 0,
        }
    }

    /// Serves frames and observations until the session ends. A client that
    /// leaves (bye or end of input) quits the session first.
    pub(crate) fn serve(
        mut self,
        lines: &mpsc::Receiver<Result<String, WireError>>,
        events: &ObservationReceiver<PlakExtension>,
    ) -> CliResult<()> {
        let mut reading = true;
        loop {
            while let Ok(event) = events.try_recv() {
                self.service.observe(event);
            }
            self.flush()?;
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
                Ok(event) => self.service.observe(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.flush()?;
                    return write_frame(self.stdout, &Host::Bye);
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
        let frame = match line.and_then(|line| decode_line::<Client>(&line)) {
            // A second hello, or a line that is not a frame, names no
            // request to refuse: the connection ends, as a client that
            // broke the protocol.
            Ok(Client::Hello(_)) | Err(_) => {
                self.quit();
                return Ok(false);
            }
            Ok(frame) => frame,
        };
        match frame {
            Client::Hello(_) => Ok(false),
            Client::Bye => {
                self.quit();
                Ok(false)
            }
            Client::Blob(chunk) => {
                if self.blobs.push(chunk).is_err() {
                    self.quit();
                    return Ok(false);
                }
                Ok(true)
            }
            Client::Request { id, op } => {
                let named = matches!(op, Op::Intent { .. });
                let outcome = self.request(id, op);
                // Blobs precede the request that names them: any left are
                // no one's.
                if !named {
                    self.blobs.discard();
                }
                if let Some(outcome) = outcome {
                    write_frame(self.stdout, &Host::Response { id, outcome })?;
                }
                Ok(true)
            }
        }
    }

    /// The request's outcome, or `None` for a child's mail, answered once
    /// the mail is written.
    fn request(&mut self, id: RequestId, op: Op<PlakExtension>) -> Option<Outcome> {
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
    fn mail_answer(&mut self, observation: &SessionObservation<PlakExtension>) {
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
            write_frame(self.stdout, &Host::Response { id, outcome })?;
        }
        let Some(attachment) = self.attachment else {
            return Ok(());
        };
        for item in self.service.take(attachment) {
            if let Outbound::Event { observation, .. } = &item {
                self.mail_answer(observation);
            }
            for (id, outcome) in std::mem::take(&mut self.settled) {
                write_frame(self.stdout, &Host::Response { id, outcome })?;
            }
            match item {
                Outbound::ResyncRequired => {
                    self.attachment = None;
                    self.unanswered();
                    for (id, outcome) in std::mem::take(&mut self.settled) {
                        write_frame(self.stdout, &Host::Response { id, outcome })?;
                    }
                    write_frame(self.stdout, &Host::ResyncRequired { attachment })?;
                }
                Outbound::Event {
                    observation: SessionObservation::PresentationSnapshot(snapshot),
                    ..
                } => {
                    let pages = page_snapshot(snapshot, attachment)
                        .map_err(|error| CliFailure::new(error.to_string()))?;
                    for page in pages {
                        write_frame(self.stdout, &Host::Snapshot(page))?;
                    }
                }
                Outbound::Event {
                    seq,
                    class,
                    mut observation,
                } => {
                    let taken = take_observation_images(&mut observation, &format!("e{seq}"));
                    for chunk in taken.chunks {
                        write_frame(self.stdout, &Host::Blob(chunk))?;
                    }
                    write_frame(
                        self.stdout,
                        &Host::Event {
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
