//! `plakd host wake --session <id> --text <text>` (spec 02 §4.6): one prompt
//! to a session through this home's running host, as a client would send it.
//!
//! It connects to the control socket of this build's state key, attaches to
//! the session's root, submits the text once with a fresh submission id and
//! waits for the host's verdict on the draft. A busy root queues it. A lost
//! answer is reported as unknown and never resent.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use plakd_wire_types::v13::{self, HostFrameV13 as Frame};
use plakous_host_protocol::wire::{
    Answer, ClientDraftId, ClientFrame, Hello, HostFrame, MAX_LINE_BYTES, NodeAddress, Op,
    RequestId, decode_line, encode_line,
};
use plakous_host_protocol::{Draft, SessionIntent, SessionObservation};

use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;
use crate::{CliFailure, CliResult, ProcessOutcome};

const DEADLINE: Duration = Duration::from_secs(60);
const MAX_TEXT_BYTES: usize = 64 * 1024;

pub(crate) fn wake(services: &Services, session: &str, text: &str) -> CliResult<ProcessOutcome> {
    if uuid::Uuid::parse_str(session).is_err() {
        return Err(CliFailure::new("host wake: the session must be a uuid"));
    }
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        return Err(CliFailure::new(
            "host wake: the text must be non-empty and at most 64 KiB",
        ));
    }
    let paths = services.paths()?;
    let node = crate::store::ProductStore::inspect_wake_node(paths, session)
        .map_err(|error| CliFailure::new(format!("host wake: {error}")))?;
    let socket = paths.host_socket();
    let stream = UnixStream::connect(&socket).map_err(|error| {
        CliFailure::new(format!(
            "host wake: no host serves this home's state key ({error}); start plakd host first"
        ))
    })?;
    let mut client = Client::new(stream)?;
    let outcome = client.wake(node, text);
    client.leave();
    outcome?;
    writeln!(std::io::stdout().lock(), "woke session {session}")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    deadline: Instant,
    submitted: bool,
}

impl Client {
    fn new(stream: UnixStream) -> CliResult<Self> {
        let reader = BufReader::new(stream.try_clone().map_err(io_failure)?);
        let mut client = Self {
            stream,
            reader,
            deadline: Instant::now() + DEADLINE,
            submitted: false,
        };
        client.send(&ClientFrame::Hello(Hello {
            proto: v13::WIRE_VERSION,
            binary_id: crate::build_identity::binary_id().to_owned(),
            caps: Vec::new(),
        }))?;
        match client.next()? {
            v13::HostFrame::Shared(frame) => match frame.into_inner() {
                HostFrame::Welcome(_) => Ok(client),
                HostFrame::Refused(refusal) => Err(CliFailure::new(format!(
                    "host wake: the host refused the connection: {refusal:?}"
                ))),
                _ => Err(CliFailure::new("host wake: the host did not welcome")),
            },
            v13::HostFrame::Draft(_) => Err(CliFailure::new("host wake: the host did not welcome")),
        }
    }

    fn wake(&mut self, node: NodeAddress, text: &str) -> CliResult<()> {
        let attachment = match self.request(1, Op::Attach { node })? {
            v13::Outcome::Ok(answer) => match *answer {
                v13::Answer::Draft(v13::AnswerV13::Attached { attachment, .. }) => attachment,
                other => return Err(refused("attach", &other)),
            },
            v13::Outcome::Refused(reason) => return Err(refused("attach", &reason)),
        };
        let draft_id = ClientDraftId::new();
        match self.request(
            2,
            Op::Intent {
                attachment,
                intent: SessionIntent::Prompt(Draft::from(text)),
                client_id: Some(draft_id),
                blobs: Vec::new(),
            },
        )? {
            v13::Outcome::Ok(answer) => match *answer {
                v13::Answer::Shared(shared) => {
                    let answer = shared.into_inner();
                    if !matches!(answer, Answer::Accepted { .. }) {
                        return Err(refused("submit", &answer));
                    }
                }
                v13::Answer::Draft(other) => return Err(refused("submit", &other)),
            },
            v13::Outcome::Refused(reason) => return Err(refused("submit", &reason)),
        }
        loop {
            if let v13::HostFrame::Draft(frame) = self.next()?
                && let Frame::Event {
                    attachment: observed,
                    observation,
                    client_id: Some(observed_id),
                    ..
                } = *frame
                && observed == attachment
                && observed_id == draft_id
                && let v13::Observation::Shared(observation) = *observation
            {
                match observation.into_inner().0 {
                    SessionObservation::DraftAccepted(_) => return Ok(()),
                    SessionObservation::DraftRejected(_) => {
                        return Err(CliFailure::new(
                            "host wake: the session rejected the prompt",
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    fn request(&mut self, id: RequestId, op: Op<PlakExtension>) -> CliResult<v13::Outcome> {
        let submits = matches!(&op, Op::Intent { .. });
        self.send(&ClientFrame::Request { id, op })?;
        self.submitted |= submits;
        loop {
            if let v13::HostFrame::Draft(frame) = self.next()?
                && let Frame::Response {
                    id: answered,
                    outcome,
                } = *frame
                && answered == id
            {
                return Ok(outcome);
            }
        }
    }

    fn send(&mut self, frame: &ClientFrame<PlakExtension>) -> CliResult<()> {
        let line = encode_line(frame).map_err(|error| CliFailure::new(error.to_string()))?;
        let mut bytes = line.as_bytes();
        while !bytes.is_empty() {
            self.stream
                .set_write_timeout(Some(self.remaining()?))
                .map_err(|_| connection_lost(self.submitted))?;
            let written = self
                .stream
                .write(bytes)
                .map_err(|_| connection_lost(self.submitted))?;
            if written == 0 {
                return Err(connection_lost(self.submitted));
            }
            bytes = &bytes[written..];
        }
        Ok(())
    }

    fn remaining(&self) -> CliResult<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| connection_lost(self.submitted))
    }

    fn next(&mut self) -> CliResult<v13::HostFrame> {
        let mut line = Vec::new();
        let lost = || connection_lost(self.submitted);
        loop {
            self.stream
                .set_read_timeout(Some(self.remaining()?))
                .map_err(|_| lost())?;
            let bytes = self.reader.fill_buf().map_err(|_| lost())?;
            if bytes.is_empty() {
                return Err(lost());
            }
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(bytes.len(), |index| index + 1);
            if count > MAX_LINE_BYTES.saturating_sub(line.len()) {
                return Err(lost());
            }
            line.extend_from_slice(&bytes[..count]);
            self.reader.consume(count);
            if newline.is_some() {
                let line = std::str::from_utf8(&line).map_err(|_| lost())?;
                return decode_line(line.trim_end()).map_err(|_| lost());
            }
        }
    }

    fn leave(&mut self) {
        let _ = self.send(&ClientFrame::Bye);
    }
}

fn refused(step: &str, outcome: &impl std::fmt::Debug) -> CliFailure {
    CliFailure::new(format!("host wake: {step} was refused: {outcome:?}"))
}

fn connection_lost(submitted: bool) -> CliFailure {
    CliFailure::new(if submitted {
        "host wake: the host's answer was lost; the prompt may have been admitted, check the session before sending again"
    } else {
        "host wake: the host connection failed before the prompt was sent"
    })
}

fn io_failure(error: std::io::Error) -> CliFailure {
    CliFailure::new(format!("host wake: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "local socket fixtures")]
mod tests {
    use super::*;
    use plakous_host_protocol::wire::Welcome;
    use std::net::Shutdown;

    #[test]
    fn only_a_lost_answer_after_sending_the_prompt_warns_of_possible_admission() {
        for replies in 0..3 {
            let (stream, mut host) = UnixStream::pair().unwrap();
            if replies >= 1 {
                let welcome = HostFrame::Welcome(Welcome::<PlakExtension> {
                    extension: None,
                    proto: v13::WIRE_VERSION,
                    binary_id: "fixture".into(),
                    host_generation: 1,
                    state_key: String::new(),
                    caps: Vec::new(),
                    session: None,
                });
                host.write_all(encode_line(&welcome).unwrap().as_bytes())
                    .unwrap();
            }
            if replies >= 2 {
                let attached = v13::HostFrame::Draft(Box::new(Frame::Response {
                    id: 1,
                    outcome: v13::Outcome::Ok(Box::new(v13::Answer::Draft(
                        v13::AnswerV13::Attached {
                            attachment: 1,
                            transcript: v13::AttachTranscript::FirstPage,
                        },
                    ))),
                }));
                host.write_all(encode_line(&attached).unwrap().as_bytes())
                    .unwrap();
            }
            host.shutdown(Shutdown::Write).unwrap();
            let address = NodeAddress {
                tree_id: "fixture-tree".into(),
                agent_id: "root".into(),
            };
            let error = Client::new(stream)
                .and_then(|mut client| client.wake(address, "fixture prompt"))
                .unwrap_err()
                .to_string();
            let sent = replies == 2;
            assert_eq!(
                error.contains("the prompt may have been admitted"),
                sent,
                "{error}"
            );
            assert_eq!(
                error.contains("before the prompt was sent"),
                !sent,
                "{error}"
            );
        }
    }
}
