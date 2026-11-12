//! A deliberately small test client, not a supported connection scheduler.

use super::*;
use plakous_host_protocol::wire::{
    Answer, ClientFrame, NodeAddress, Op, Outcome, SessionRow, decode_line, encode_line,
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::{
    Created, Creation, Face, HostConnection, HostReadLimits, HostReadTask, HostSessions,
    OpenedSession, SessionHost,
};
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

struct Stores {
    console: Arc<Console>,
}

impl HostSessions<PlakExtension> for Stores {
    fn host_read_limits(&self) -> HostReadLimits {
        crate::host::console::admission_limits()
    }
    fn welcome_extension(&self, p: &HostPrincipal, l: &HostReadLimits) -> Option<dto::HostWelcome> {
        Some(self.console.welcome(p, l))
    }
    fn host_stream(&self, c: &HostRequestContext) -> Option<HostFrame<PlakExtension>> {
        self.console.terminal_output(c)
    }
    fn host_request(&self, c: HostRequestContext, r: HostRequest) -> HostReadTask<PlakExtension> {
        let console = Arc::clone(&self.console);
        Box::pin(async move {
            let response = match r {
                HostRequest::TerminalInput {
                    stream,
                    offset,
                    bytes_b64,
                } => console.terminal_input(&c, stream, offset, &bytes_b64).await,
                HostRequest::TerminalCreditBatch { grants } => console.terminal_credit(&c, grants),
                HostRequest::FilesList { .. } => Ok(HostResponse::FilesList {
                    entries: Vec::new(),
                    summary: dto::FileQuerySummary {
                        skipped_entries: 0,
                        skipped_binary: 0,
                        skipped_oversize: 0,
                        skipped_changed: 0,
                        truncated: false,
                        stop_reason: None,
                    },
                }),
                _ => panic!("unexpected contract request"),
            };
            Ok(response.unwrap_or_else(|reason| HostResponse::Refused { reason }))
        })
    }
    fn contains(&self, _: &str) -> Result<bool, String> {
        panic!("agent store used")
    }
    fn rows(&self, _: Option<&str>, _: Option<&str>, _: bool) -> Result<Vec<SessionRow>, String> {
        Ok(Vec::new())
    }
    fn session_of(&self, _: &NodeAddress) -> Result<Option<String>, String> {
        panic!("agent node resolved")
    }
    fn open(&self, _: &str) -> CliResult<OpenedSession<PlakExtension>> {
        panic!("agent opened")
    }
    fn refusal(&self, _: &str, _: &CliFailure) -> RequestRefusal {
        panic!("agent admitted")
    }
    fn create(&self, _: &Creation<'_>) -> Result<Created, RequestRefusal> {
        panic!("agent created")
    }
}

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Output {
    fn take(&self) -> Vec<HostFrame<PlakExtension>> {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap()))
            .unwrap()
            .lines()
            .map(|line| decode_line(line).unwrap())
            .collect()
    }
}

type Connection = HostConnection<PlakExtension, Output>;

fn send(connection: &mut Connection, id: u64, op: Op<PlakExtension>) {
    assert!(
        connection
            .frame(Ok(encode_line(&ClientFrame::<PlakExtension>::Request {
                id,
                op
            })
            .unwrap()))
            .unwrap()
    );
    connection.flush().unwrap();
}

fn expansion() -> HostRequest {
    HostRequest::FilesList {
        target: dto::WorkspaceTarget {
            session: "fixture".into(),
            node: None,
        },
        path: ".".into(),
    }
}

struct Producer {
    receiver: mpsc::Receiver<Command>,
    credit: u64,
    next: u64,
    typed: Vec<u8>,
}

impl Producer {
    fn step(&mut self, console: &Console, stream: u64) {
        while let Ok(command) = self.receiver.try_recv() {
            match command {
                Command::Credit(bytes) => self.credit += bytes,
                Command::Input {
                    offset,
                    bytes,
                    reply,
                } => {
                    assert_eq!(offset, self.typed.len() as u64);
                    self.typed.extend(bytes);
                    assert!(console.publish_terminal(
                        stream,
                        dto::TerminalEvent::InputAccepted {
                            next: self.typed.len() as u64,
                        }
                    ));
                    reply.send(Ok(())).unwrap();
                }
            }
        }
        if self.credit >= CHUNK as u64 {
            assert!(console.publish_terminal(
                stream,
                dto::TerminalEvent::Data {
                    offset: self.next,
                    bytes_b64: STANDARD.encode(vec![0xff; CHUNK]),
                }
            ));
            self.next += CHUNK as u64;
            self.credit -= CHUNK as u64;
        }
    }
}

#[derive(Default)]
struct Client {
    offsets: [u64; 4],
    sequences: [u64; 4],
    consumed: [u64; 4],
    typed: Vec<u8>,
    reads: usize,
    accepted: usize,
}

impl Client {
    fn read(&mut self, frames: Vec<HostFrame<PlakExtension>>) {
        for frame in frames {
            match frame {
                HostFrame::TerminalStream {
                    stream, seq, event, ..
                } => {
                    let i = usize::try_from(stream - 1).unwrap();
                    assert_eq!(seq, self.sequences[i]);
                    self.sequences[i] += 1;
                    match event {
                        dto::TerminalEvent::Data { offset, bytes_b64 } => {
                            assert_eq!(offset, self.offsets[i]);
                            let bytes = STANDARD.decode(bytes_b64).unwrap();
                            assert_eq!(bytes, vec![0xff; CHUNK]);
                            self.offsets[i] += bytes.len() as u64;
                            self.consumed[i] += bytes.len() as u64;
                        }
                        dto::TerminalEvent::InputAccepted { next } => {
                            assert_eq!(next, self.typed.len() as u64);
                        }
                        _ => panic!("stream detached or refused"),
                    }
                }
                HostFrame::Response {
                    outcome: Outcome::Ok(Answer::Host { response }),
                    ..
                } => match response {
                    HostResponse::FilesList { .. } => self.reads += 1,
                    HostResponse::TerminalAccepted {} => self.accepted += 1,
                    _ => panic!("host request refused"),
                },
                _ => panic!("unexpected scheduled frame: {frame:?}"),
            }
        }
    }
    fn request(&mut self, step: u64) -> HostRequest {
        if step.is_multiple_of(5) {
            expansion()
        } else if self.consumed.iter().all(|bytes| *bytes >= WINDOW) {
            let grants = (1..=4)
                .map(|stream| dto::TerminalCredit {
                    stream,
                    bytes: WINDOW,
                })
                .collect();
            for bytes in &mut self.consumed {
                *bytes -= WINDOW;
            }
            HostRequest::TerminalCreditBatch { grants }
        } else {
            let bytes = format!("key-{step};").into_bytes();
            let offset = self.typed.len() as u64;
            self.typed.extend(&bytes);
            HostRequest::TerminalInput {
                stream: 1,
                offset,
                bytes_b64: STANDARD.encode(bytes),
            }
        }
    }
}

#[test]
fn operator_console_four_terminals_typing_tree_and_overrun_keep_connection_alive() {
    let console = fixture();
    let host = SessionHost::new(
        Stores {
            console: Arc::clone(&console),
        },
        "fixture",
        9,
    );
    let output = Output::default();
    let mut connection = host.connect(output.clone(), Face::Remote, context().principal, || false);
    let mut producers: Vec<_> = (1..=4)
        .map(|id| Producer {
            receiver: add(&console, id),
            credit: WINDOW,
            next: 0,
            typed: Vec::new(),
        })
        .collect();
    for stream in console.terminals.lock().unwrap().streams.values_mut() {
        stream.owner.1 = 1;
    }
    let mut connected = context();
    connected.connection_id = 1;
    console
        .terminals
        .lock()
        .unwrap()
        .notifications
        .entry(owner(&connected))
        .or_default();
    send(
        &mut connection,
        1,
        Op::WatchSessions {
            include_archived: false,
        },
    );
    let attachment = output
        .take()
        .into_iter()
        .find_map(|frame| match frame {
            HostFrame::Response {
                outcome: Outcome::Ok(Answer::Watching { attachment }),
                ..
            } => Some(attachment),
            _ => None,
        })
        .unwrap();
    let mut client = Client::default();
    for step in 0..40 {
        // One envelope per tick shares typing, four-row credits and tree expansion.
        let request = client.request(step);
        send(&mut connection, step + 2, Op::Host { request });
        for (i, producer) in producers.iter_mut().enumerate() {
            producer.step(&console, i as u64 + 1);
        }
        connection.flush().unwrap();
        connection.flush().unwrap();
        client.read(output.take());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(client.offsets.iter().all(|bytes| *bytes >= WINDOW * 2));
    assert!(
        client.reads >= 4 && client.accepted >= 10,
        "tree reads: {}, accepted terminal actions: {}",
        client.reads,
        client.accepted
    );
    assert_eq!(producers[0].typed, client.typed);
    overrun(
        &console,
        attachment,
        &mut connection,
        &output,
        &mut producers,
        &mut client,
    );
}

fn overrun(
    console: &Console,
    attachment: u64,
    connection: &mut Connection,
    output: &Output,
    producers: &mut [Producer],
    client: &mut Client,
) {
    for id in 100..180 {
        send(
            connection,
            id,
            Op::Host {
                request: expansion(),
            },
        );
    }
    let before = producers[0].typed.len() as u64;
    send(
        connection,
        180,
        Op::Host {
            request: HostRequest::TerminalInput {
                stream: 1,
                offset: before,
                bytes_b64: STANDARD.encode(b"must-not-enter-pty"),
            },
        },
    );
    let frames = output.take();
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HostFrame::Response {
            id: 180,
            outcome: Outcome::Refused(RequestRefusal::RateLimited { .. }),
        }
    )));
    assert_eq!(
        console.terminals.lock().unwrap().streams[&1].next_input,
        before
    );
    for producer in producers.iter_mut() {
        assert!(producer.receiver.try_recv().is_err());
    }
    send(connection, 181, Op::Ping);
    assert!(output.take().iter().any(|frame| matches!(
        frame,
        HostFrame::Response {
            id: 181,
            outcome: Outcome::Ok(Answer::Pong),
        }
    )));
    // The existing directory attachment remains owned, not implicitly detached.
    send(connection, 183, Op::UnwatchSessions { attachment });
    assert!(output.take().iter().any(|frame| matches!(
        frame,
        HostFrame::Response {
            id: 183,
            outcome: Outcome::Ok(Answer::Detached),
        }
    )));
    std::thread::sleep(Duration::from_millis(100));
    let grants = (1..=4)
        .map(|stream| dto::TerminalCredit {
            stream,
            bytes: client.consumed[usize::try_from(stream - 1).unwrap()].min(WINDOW),
        })
        .filter(|grant| grant.bytes != 0)
        .collect();
    send(
        connection,
        182,
        Op::Host {
            request: HostRequest::TerminalCreditBatch { grants },
        },
    );
    for (i, producer) in producers.iter_mut().enumerate() {
        producer.step(console, i as u64 + 1);
    }
    connection.flush().unwrap();
    client.read(output.take());
    assert!(
        console
            .terminals
            .lock()
            .unwrap()
            .streams
            .values()
            .all(|stream| !stream.ended)
    );
}
