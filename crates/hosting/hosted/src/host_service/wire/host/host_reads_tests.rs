#![allow(clippy::unwrap_used, reason = "isolated host read fixtures")]

use super::*;
use plakous_host_protocol::{Never, ProtocolExtension};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Reads;

impl ProtocolExtension for Reads {
    type Intent = Never;
    type Observation = Never;
    type HostRequest = u8;
    type HostResponse = u8;
    type WelcomeExtension = Option<Never>;
}

struct Stores(Arc<std::sync::atomic::AtomicBool>);

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

impl HostSessions<Reads> for Stores {
    fn host_request(&self, context: HostRequestContext, request: u8) -> HostReadTask<Reads> {
        assert!(
            context.principal == HostPrincipal::Socket(501)
                || context.principal == HostPrincipal::Web("fixture-login".to_owned())
        );
        assert_eq!(context.host_generation, 1);
        let ready = self.0.clone();
        Box::pin(std::future::poll_fn(move |_| {
            if request != 0 || ready.load(Ordering::SeqCst) {
                std::task::Poll::Ready(Ok(request))
            } else {
                std::task::Poll::Pending
            }
        }))
    }
    fn contains(&self, _: &str) -> Result<bool, String> {
        panic!("read touched session store")
    }
    fn rows(&self, _: Option<&str>, _: Option<&str>, _: bool) -> Result<Vec<SessionRow>, String> {
        panic!("read touched directory")
    }
    fn session_of(&self, _: &NodeAddress) -> Result<Option<String>, String> {
        panic!("read resolved a node")
    }
    fn open(&self, _: &str) -> CliResult<OpenedSession<Reads>> {
        panic!("read opened a runtime")
    }
    fn refusal(&self, _: &str, _: &CliFailure) -> RequestRefusal {
        panic!("read attempted session admission")
    }
    fn create(&self, _: &Creation<'_>) -> Result<Created, RequestRefusal> {
        panic!("read created a session")
    }
}

#[test]
fn host_reads_are_private_bounded_attachment_free_and_do_not_stall_ping() {
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host = SessionHost::new(Stores(ready.clone()), "test", 1);
    let output = Output::default();
    let mut connection = host
        .connect(output.clone(), Face::Remote, || false)
        .identified("fixture-login");
    for (id, op) in [
        (10, Op::Host { request: 0 }),
        (11, Op::Host { request: 1 }),
        (12, Op::Host { request: 2 }),
        (13, Op::Ping),
    ] {
        let line =
            plakous_host_protocol::wire::encode_line(&ClientFrame::<Reads>::Request { id, op })
                .unwrap();
        assert!(connection.frame(Ok(line)).unwrap());
    }
    connection.flush().unwrap();
    let bytes = std::mem::take(&mut *output.0.lock().unwrap());
    let frames: Vec<HostFrame<Reads>> = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|s| decode_line(s).unwrap())
        .collect();
    assert_eq!(
        frames,
        vec![
            HostFrame::Response {
                id: 12,
                outcome: Outcome::Refused(RequestRefusal::RateLimited { retry_after_ms: 50 })
            },
            HostFrame::Response {
                id: 13,
                outcome: Outcome::Ok(Answer::Pong)
            },
            HostFrame::Response {
                id: 11,
                outcome: Outcome::Ok(Answer::Host { response: 1 })
            },
        ]
    );
    ready.store(true, Ordering::SeqCst);
    connection.flush().unwrap();
    let bytes = std::mem::take(&mut *output.0.lock().unwrap());
    let frame: HostFrame<Reads> = decode_line(String::from_utf8(bytes).unwrap().trim()).unwrap();
    assert_eq!(
        frame,
        HostFrame::Response {
            id: 10,
            outcome: Outcome::Ok(Answer::Host { response: 0 })
        }
    );
    connection.flush().unwrap();
    assert!(output.0.lock().unwrap().is_empty());
    let line = plakous_host_protocol::wire::encode_line(&ClientFrame::<Reads>::Request {
        id: 20,
        op: Op::Host { request: 0 },
    })
    .unwrap();
    connection.frame(Ok(line)).unwrap();
    connection.leave();
    assert!(connection.host_reads.is_empty());
}

#[test]
fn remote_face_alone_never_supplies_console_authority() {
    let host = SessionHost::new(
        Stores(Arc::new(std::sync::atomic::AtomicBool::new(true))),
        "test",
        1,
    );
    let output = Output::default();
    let mut connection = host.connect(output.clone(), Face::Remote, || false);
    let line = plakous_host_protocol::wire::encode_line(&ClientFrame::<Reads>::Request {
        id: 7,
        op: Op::Host { request: 1 },
    })
    .unwrap();
    assert!(connection.frame(Ok(line)).unwrap());
    connection.flush().unwrap();
    let bytes = std::mem::take(&mut *output.0.lock().unwrap());
    let frame: HostFrame<Reads> = decode_line(String::from_utf8(bytes).unwrap().trim()).unwrap();
    assert_eq!(
        frame,
        HostFrame::Response {
            id: 7,
            outcome: Outcome::Refused(RequestRefusal::RefusedForRole {
                row: "authenticated_console".to_owned()
            })
        }
    );
    assert!(connection.host_reads.is_empty());
}

#[test]
fn verified_socket_retains_its_uid_without_granting_a_local_face() {
    let host = SessionHost::new(
        Stores(Arc::new(std::sync::atomic::AtomicBool::new(true))),
        "test",
        1,
    );
    let output = Output::default();
    let mut connection = host
        .connect(output.clone(), Face::Remote, || false)
        .verified_socket(501);
    assert_eq!(connection.face, Face::Remote);
    let line = plakous_host_protocol::wire::encode_line(&ClientFrame::<Reads>::Request {
        id: 7,
        op: Op::Host { request: 1 },
    })
    .unwrap();
    assert!(connection.frame(Ok(line)).unwrap());
    connection.flush().unwrap();
    let bytes = std::mem::take(&mut *output.0.lock().unwrap());
    let frame: HostFrame<Reads> = decode_line(String::from_utf8(bytes).unwrap().trim()).unwrap();
    assert_eq!(
        frame,
        HostFrame::Response {
            id: 7,
            outcome: Outcome::Ok(Answer::Host { response: 1 })
        }
    );
}
