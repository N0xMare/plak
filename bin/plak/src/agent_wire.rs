//! `plak agent --wire v2` (hidden): wire v2 on stdio (spec 02 §4.4) over
//! the session this process opens. Until the host's attachments exist, it
//! is an adapter over today's session mailbox: one connection, one
//! attachment at a time, on the session's root node only. Every
//! observation goes to that attachment with its class; a presentation
//! snapshot goes as pages and image bytes as blobs.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::{
    Answer, AttachmentId, BlobAssembler, ClientFrame, HandshakeRefusal, HostFrame, LineReader,
    NodeAddress, Op, Outcome, RequestRefusal, WIRE_VERSION, Welcome, WireError, admit_hello,
    decode_line, encode_line, page_snapshot, take_observation_images,
};
use plakous_host_protocol::{IntentSendError, SessionIntent, SessionObservation};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::runtime::{HostConfig, spawn_host_worker};

use crate::surface_protocol::{HostEvent, HostEventReceiver, PlakExtension, UserActionSender};

type Client = ClientFrame<PlakExtension>;
type Host = HostFrame<PlakExtension>;

const POLL: Duration = Duration::from_millis(20);
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// The only attachment this adapter makes.
const ROOT_ATTACHMENT: AttachmentId = 1;

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
    let served = Adapter::new(root, stdout).serve(&lines, &actions, &events);
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

/// The connection's state after the handshake.
pub(crate) struct Adapter<'a> {
    root: NodeAddress,
    stdout: &'a mut dyn Write,
    attached: bool,
    seq: u64,
    blobs: BlobAssembler,
    quit_sent: bool,
}

impl<'a> Adapter<'a> {
    pub(crate) fn new(root: NodeAddress, stdout: &'a mut dyn Write) -> Self {
        Self {
            root,
            stdout,
            attached: false,
            seq: 0,
            blobs: BlobAssembler::default(),
            quit_sent: false,
        }
    }

    /// Serves frames and observations until the session ends. A client that
    /// leaves (bye or end of input) quits the session first.
    pub(crate) fn serve(
        mut self,
        lines: &mpsc::Receiver<Result<String, WireError>>,
        actions: &UserActionSender,
        events: &HostEventReceiver,
    ) -> CliResult<()> {
        let mut reading = true;
        loop {
            while let Ok(event) = events.try_recv() {
                self.observe(event)?;
            }
            if reading {
                match lines.recv_timeout(POLL) {
                    Ok(line) => reading = self.frame(line, actions)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        reading = false;
                        self.quit(actions);
                    }
                }
                continue;
            }
            match events.recv_timeout(POLL) {
                Ok(event) => self.observe(event)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return write_frame(self.stdout, &Host::Bye);
                }
            }
        }
    }

    fn quit(&mut self, actions: &UserActionSender) {
        if !self.quit_sent {
            self.quit_sent = true;
            let _ = actions.send(SessionIntent::Quit);
        }
    }

    /// Answers one client line; `false` once the client has left.
    fn frame(
        &mut self,
        line: Result<String, WireError>,
        actions: &UserActionSender,
    ) -> CliResult<bool> {
        let frame = match line.and_then(|line| decode_line::<Client>(&line)) {
            // A second hello, or a line that is not a frame, names no
            // request to refuse: the connection ends, as a client that
            // broke the protocol.
            Ok(Client::Hello(_)) | Err(_) => {
                self.quit(actions);
                return Ok(false);
            }
            Ok(frame) => frame,
        };
        match frame {
            Client::Hello(_) => Ok(false),
            Client::Bye => {
                self.quit(actions);
                Ok(false)
            }
            Client::Blob(chunk) => {
                if self.blobs.push(chunk).is_err() {
                    self.quit(actions);
                    return Ok(false);
                }
                Ok(true)
            }
            Client::Request { id, op } => {
                let named = matches!(op, Op::Intent { .. });
                let outcome = self.request(op, actions);
                // Blobs precede the request that names them: any left are
                // no one's.
                if !named {
                    self.blobs.discard();
                }
                write_frame(self.stdout, &Host::Response { id, outcome })?;
                Ok(true)
            }
        }
    }

    fn request(&mut self, op: Op<PlakExtension>, actions: &UserActionSender) -> Outcome {
        match op {
            Op::Ping => Outcome::Ok(Answer::Pong),
            Op::Attach { node } => {
                if node != self.root {
                    // Child attach comes with the host's attachments.
                    return Outcome::Refused(RequestRefusal::UnknownNode);
                }
                self.attached = true;
                self.seq = 0;
                // The attachment's baseline: a fresh root snapshot.
                let _ = actions.send(SessionIntent::ResyncTranscript {
                    scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
                });
                Outcome::Ok(Answer::Attached {
                    attachment: ROOT_ATTACHMENT,
                })
            }
            Op::Detach { attachment } => {
                if !self.holds(attachment) {
                    return Outcome::Refused(RequestRefusal::NotAttached);
                }
                self.attached = false;
                Outcome::Ok(Answer::Detached)
            }
            Op::Intent {
                attachment,
                mut intent,
                blobs,
            } => {
                if !self.holds(attachment) {
                    return Outcome::Refused(RequestRefusal::NotAttached);
                }
                if let Err(error) = self.blobs.restore_images(&mut intent, &blobs) {
                    return Outcome::Refused(RequestRefusal::Malformed {
                        detail: error.to_string(),
                    });
                }
                match actions.send(intent) {
                    Ok(()) => Outcome::Ok(Answer::Accepted),
                    Err(IntentSendError::Saturated(_)) => {
                        Outcome::Refused(RequestRefusal::Backpressure)
                    }
                    Err(IntentSendError::Duplicate(_)) => {
                        Outcome::Refused(RequestRefusal::OneInFlight)
                    }
                    Err(IntentSendError::Disconnected(_)) => {
                        Outcome::Refused(RequestRefusal::Disconnected)
                    }
                }
            }
        }
    }

    fn holds(&self, attachment: AttachmentId) -> bool {
        self.attached && attachment == ROOT_ATTACHMENT
    }

    /// Sends one observation to the attachment, if there is one: a
    /// snapshot as pages, anything else as an event after its image blobs.
    fn observe(&mut self, mut event: HostEvent) -> CliResult<()> {
        if !self.attached {
            return Ok(());
        }
        if let SessionObservation::PresentationSnapshot(snapshot) = event {
            let pages = page_snapshot(snapshot, ROOT_ATTACHMENT)
                .map_err(|error| CliFailure::new(error.to_string()))?;
            for page in pages {
                write_frame(self.stdout, &Host::Snapshot(page))?;
            }
            return Ok(());
        }
        let taken = take_observation_images(&mut event, &format!("e{}", self.seq + 1));
        for chunk in taken.chunks {
            write_frame(self.stdout, &Host::Blob(chunk))?;
        }
        self.seq += 1;
        write_frame(
            self.stdout,
            &Host::Event {
                attachment: ROOT_ATTACHMENT,
                seq: self.seq,
                class: event.class(),
                observation: event,
                blobs: taken.ids,
            },
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
