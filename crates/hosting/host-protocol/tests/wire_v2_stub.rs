//! Wire v2 end to end against a stub host over a real socket pair: the
//! handshake, attach, an intent with image blobs, events with their class
//! and sequence, a paged snapshot, a typed refusal, and a version refused.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use plakous_host_protocol::intent::{Core, Draft, DraftImage, SessionIntent};
use plakous_host_protocol::observation::{ObservationClass, SessionObservation};
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::wire::{
    Answer, BlobAssembler, ClientFrame, HandshakeRefusal, Hello, HostFrame, LineReader,
    NodeAddress, Op, Outcome, RequestRefusal, SnapshotAssembler, WIRE_VERSION, Welcome,
    WelcomeSession, admit_hello, decode_line, encode_line, page_snapshot, take_images,
};

type Client = ClientFrame<Core>;
type Host = HostFrame<Core>;

/// One end of the connection: writes frames, reads them line by line.
struct End {
    stream: UnixStream,
    reader: LineReader,
    lines: std::collections::VecDeque<String>,
}

impl End {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            reader: LineReader::default(),
            lines: std::collections::VecDeque::new(),
        }
    }

    fn send<T: serde::Serialize>(&mut self, frame: &T) {
        self.stream
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    /// The next frame, or `None` at the end of the stream.
    fn next<T: serde::de::DeserializeOwned>(&mut self) -> Option<T> {
        loop {
            if let Some(line) = self.lines.pop_front() {
                return Some(decode_line(&line).unwrap());
            }
            let mut buffer = vec![0_u8; 64 * 1024];
            let read = self.stream.read(&mut buffer).unwrap();
            if read == 0 {
                return None;
            }
            for line in self.reader.push(&buffer[..read]) {
                self.lines.push_back(line.unwrap());
            }
        }
    }
}

/// The stub: admits one hello, answers attach and intents for node
/// `t1/root`, refuses anything else typed, echoes an accepted prompt's
/// draft back with its images, and sends a paged snapshot on attach.
fn stub_host(stream: UnixStream) {
    let mut end = End::new(stream);
    let Some(Client::Hello(hello)) = end.next::<Client>() else {
        panic!("the first frame is a hello");
    };
    let welcome = Welcome {
        proto: WIRE_VERSION,
        binary_id: "host".into(),
        host_generation: 1,
        caps: Vec::new(),
        session: Some(WelcomeSession {
            session_id: "s1".into(),
            tree_id: "t1".into(),
            root: NodeAddress {
                tree_id: "t1".into(),
                agent_id: "t1".into(),
            },
        }),
    };
    match admit_hello(&hello, welcome) {
        Ok(welcome) => end.send(&Host::Welcome(welcome)),
        Err(refusal) => {
            end.send(&Host::Refused(refusal));
            return;
        }
    }
    let mut blobs = BlobAssembler::default();
    let mut seq = 0;
    while let Some(frame) = end.next::<Client>() {
        match frame {
            Client::Blob(chunk) => blobs.push(chunk).unwrap(),
            Client::Request { id, op } => match op {
                Op::Attach { node } if node.tree_id == "t1" && node.agent_id == "root" => {
                    end.send(&Host::Response {
                        id,
                        outcome: Outcome::Ok(Answer::Attached { attachment: 1 }),
                    });
                    for page in page_snapshot(PresentationSnapshot::empty(3), 1).unwrap() {
                        end.send(&Host::Snapshot(page));
                    }
                }
                Op::Attach { .. } => end.send(&Host::Response {
                    id,
                    outcome: Outcome::Refused(RequestRefusal::UnknownNode),
                }),
                Op::Intent {
                    client_id: _,
                    attachment,
                    intent,
                    blobs: ids,
                } => answer_intent(
                    &mut end,
                    &mut blobs,
                    &mut seq,
                    (id, attachment),
                    intent,
                    &ids,
                ),
                Op::Detach { .. } => end.send(&Host::Response {
                    id,
                    outcome: Outcome::Ok(Answer::Detached),
                }),
                Op::Ping => end.send(&Host::Response {
                    id,
                    outcome: Outcome::Ok(Answer::Pong),
                }),
                Op::StopSession { .. }
                | Op::ListSessions { .. }
                | Op::WatchSessions { .. }
                | Op::UnwatchSessions { .. }
                | Op::NewSession { .. }
                | Op::SessionOptions { .. }
                | Op::RenameSession { .. }
                | Op::ArchiveSession { .. }
                | Op::DeleteSession { .. }
                | Op::ReleaseSession { .. } => end.send(&Host::Response {
                    id,
                    outcome: Outcome::Refused(RequestRefusal::Unsupported {
                        detail: "the stub serves one session".to_owned(),
                    }),
                }),
                Op::RetryAttach { .. } | Op::ResumeSession { .. } | Op::Rebind { .. } => {
                    end.send(&Host::Response {
                        id,
                        outcome: Outcome::Refused(RequestRefusal::SessionUnavailable {
                            detail: "the stub serves one session".to_owned(),
                        }),
                    });
                }
            },
            Client::Hello(_) => panic!("one hello"),
            Client::Bye => {
                end.send(&Host::Bye);
                return;
            }
        }
    }
}

/// The stub's answer to an intent: accepted, then the draft echoed back
/// with its images as blobs, then a busy state.
fn answer_intent(
    end: &mut End,
    blobs: &mut BlobAssembler,
    seq: &mut u64,
    (id, attachment): (u64, u64),
    mut intent: SessionIntent<Core>,
    ids: &[String],
) {
    blobs.restore_images(&mut intent, ids).unwrap();
    end.send(&Host::Response {
        id,
        outcome: Outcome::Ok(Answer::Accepted { duplicate: false }),
    });
    let mut observation = SessionObservation::DraftAccepted(intent);
    let taken = plakous_host_protocol::wire::take_observation_images(
        &mut observation,
        &format!("e{}", *seq),
    );
    for chunk in taken.chunks {
        end.send(&Host::Blob(chunk));
    }
    for observation in [observation, SessionObservation::SetBusy(true)] {
        *seq += 1;
        let blobs = if *seq == 1 {
            taken.ids.clone()
        } else {
            Vec::new()
        };
        end.send(&Host::Event {
            client_id: None,
            attachment,
            seq: *seq,
            class: observation.class(),
            observation,
            blobs,
        });
    }
}

fn hello(proto: u32) -> Client {
    Client::Hello(Hello {
        proto,
        binary_id: "client".into(),
        caps: Vec::new(),
    })
}

/// Attaches to `t1/<agent>` as request `id` and returns its outcome.
fn attach(end: &mut End, id: u64, agent: &str) -> Outcome {
    end.send(&Client::Request {
        id,
        op: Op::Attach {
            node: NodeAddress {
                tree_id: "t1".into(),
                agent_id: agent.into(),
            },
        },
    });
    match end.next::<Host>() {
        Some(Host::Response {
            id: answered,
            outcome,
        }) if answered == id => outcome,
        other => panic!("{other:?}"),
    }
}

/// The next `count` events, their images put back from the blobs that
/// precede them.
fn events(end: &mut End, count: usize) -> Vec<(u64, ObservationClass, SessionObservation<Core>)> {
    let mut blobs = BlobAssembler::default();
    let mut events = Vec::new();
    while events.len() < count {
        match end.next::<Host>().unwrap() {
            Host::Blob(chunk) => blobs.push(chunk).unwrap(),
            Host::Event {
                seq,
                class,
                mut observation,
                blobs: ids,
                ..
            } => {
                blobs
                    .restore_observation_images(&mut observation, &ids)
                    .unwrap();
                events.push((seq, class, observation));
            }
            other => panic!("{other:?}"),
        }
    }
    events
}

/// A client attaches, sends a prompt with a large image, and gets the
/// accepted answer, its events in order with their classes, the image back
/// byte-exact, and the snapshot; another node is refused typed.
#[test]
fn a_client_attaches_and_prompts_a_stub_host_over_wire_v2() {
    let (client, host) = UnixStream::pair().unwrap();
    let stub = std::thread::spawn(move || stub_host(host));
    let mut end = End::new(client);
    end.send(&hello(WIRE_VERSION));
    assert!(matches!(end.next::<Host>(), Some(Host::Welcome(_))));

    assert_eq!(
        attach(&mut end, 1, "c9"),
        Outcome::Refused(RequestRefusal::UnknownNode)
    );
    assert_eq!(
        attach(&mut end, 2, "root"),
        Outcome::Ok(Answer::Attached { attachment: 1 })
    );
    let mut pages = SnapshotAssembler::default();
    let Some(Host::Snapshot(page)) = end.next::<Host>() else {
        panic!("the attach snapshot follows");
    };
    assert_eq!(
        pages.push(page).unwrap(),
        Some(PresentationSnapshot::empty(3))
    );

    let image = DraftImage {
        name: "big.png".into(),
        media_type: "image/png".into(),
        bytes: (0..3_000_000_u32)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect(),
        source_dimensions: None,
    };
    let prompt = SessionIntent::Prompt(Draft::new("look".into(), vec![image]));
    let mut sent = prompt.clone();
    let taken = take_images(&mut sent, "r3");
    for chunk in taken.chunks {
        end.send(&Client::Blob(chunk));
    }
    end.send(&Client::Request {
        id: 3,
        op: Op::Intent {
            client_id: None,
            attachment: 1,
            intent: sent,
            blobs: taken.ids,
        },
    });
    assert_eq!(
        end.next::<Host>(),
        Some(Host::Response {
            id: 3,
            outcome: Outcome::Ok(Answer::Accepted { duplicate: false })
        })
    );
    let events = events(&mut end, 2);
    assert_eq!(events[0].0, 1);
    assert_eq!(events[0].2, SessionObservation::DraftAccepted(prompt));
    assert_eq!(events[0].1, ObservationClass::Addressed);
    assert_eq!(events[1].0, 2);
    assert_eq!(events[1].1, ObservationClass::State);

    end.send(&Client::Bye);
    assert_eq!(end.next::<Host>(), Some(Host::Bye));
    stub.join().unwrap();
}

/// A client of another version is refused typed, and nothing else is read.
#[test]
fn another_version_is_refused_at_the_handshake() {
    let (client, host) = UnixStream::pair().unwrap();
    let stub = std::thread::spawn(move || stub_host(host));
    let mut end = End::new(client);
    end.send(&hello(WIRE_VERSION + 1));
    assert_eq!(
        end.next::<Host>(),
        Some(Host::Refused(HandshakeRefusal::VersionMismatch {
            host: WIRE_VERSION,
            client: WIRE_VERSION + 1,
        }))
    );
    stub.join().unwrap();
    assert_eq!(end.next::<Host>(), None);
}
