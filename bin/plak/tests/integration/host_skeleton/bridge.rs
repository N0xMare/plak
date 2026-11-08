//! The real stdio bridge carries the client's handshake and detaches without custody.

use std::io::Read as _;

use super::*;

struct Bridge {
    child: Child,
    input: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Bridge {
    fn start(fixture: &Fixture) -> Self {
        let mut child = Child::spawn(
            fixture
                .command()
                .args(["host", "connect", "--stdio"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let output = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            input: child.stdin.take(),
            child,
            lines,
        }
    }

    fn send(&mut self, frame: &ClientFrame<Core>) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    fn next(&self) -> HostFrame<Core> {
        let line = self.lines.recv_timeout(WAIT).expect("bridge frame");
        decode_line(&line)
            .unwrap_or_else(|error| panic!("stdout was not protocol-only: {error}: {line}"))
    }

    fn request(&mut self, id: RequestId, op: Op<Core>) -> Outcome {
        self.send(&ClientFrame::Request { id, op });
        loop {
            if let HostFrame::Response {
                id: answered,
                outcome,
            } = self.next()
                && answered == id
            {
                return outcome;
            }
        }
    }

    fn hello(&mut self, proto: u32) {
        self.send(&ClientFrame::Hello(Hello {
            proto,
            binary_id: "bridge-fixture".into(),
            caps: Vec::new(),
        }));
    }

    fn finish(mut self) {
        drop(self.input.take());
        let status = self.child.wait().unwrap();
        let mut diagnostics = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut diagnostics)
            .unwrap();
        assert!(status.success(), "{diagnostics}");
    }
}

#[test]
fn the_stdio_bridge_relays_the_client_handshake_and_leaves_a_busy_root_running_on_eof() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(
        &json!({"version":1,"steps":[{"stream":[{"text":ANSWER},{"stall":300_000}]}]}),
    );
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(welcome) if welcome.session.is_none()));
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        bridge.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no created session")
    };
    let Outcome::Ok(Answer::Attached { attachment }) =
        bridge.request(2, Op::Attach { node: node.clone() })
    else {
        panic!("not attached")
    };
    assert_eq!(
        bridge.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("one bridged turn")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if let HostFrame::Event { observation, .. } = bridge.next()
            && format!("{observation:?}").contains(ANSWER)
        {
            break;
        }
    }
    bridge.finish();
    let mut reconnect = Client::connect(&host);
    let rows = reconnect.rows(1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session, session);
    assert_eq!(rows[0].activity, SessionActivity::Busy);
    reconnect.attach(2, &node);
    assert_eq!(
        crash::dispatches(&fixture),
        1,
        "bridge EOF redispatched work"
    );
    drop(reconnect);
    stop(&mut host);
}

#[test]
fn the_stdio_bridge_does_not_consume_a_separate_handshake_or_mask_a_version_refusal() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION - 1);
    assert!(matches!(
        bridge.next(),
        HostFrame::Refused(plakous_host_protocol::wire::HandshakeRefusal::VersionMismatch { .. })
    ));
    bridge.finish();
    let mut client = Client::connect(&host);
    assert_eq!(client.rows(1), []);
    drop(client);
    stop(&mut host);
}

#[test]
fn the_stdio_bridge_keeps_same_user_clients_remote_for_local_only_reload_intents() {
    use plakd_wire_types::{PlakExtension, SurfaceIntent};
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    let Outcome::Ok(Answer::Created { node, .. }) =
        bridge.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no session")
    };
    let Outcome::Ok(Answer::Attached { attachment }) = bridge.request(2, Op::Attach { node })
    else {
        panic!("not attached")
    };
    let line = encode_line(&ClientFrame::<PlakExtension>::Request {
        id: 3,
        op: Op::Intent {
            attachment,
            client_id: None,
            blobs: Vec::new(),
            intent: SessionIntent::Extension(SurfaceIntent::CheckBuild),
        },
    })
    .unwrap();
    bridge
        .input
        .as_mut()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    loop {
        if let HostFrame::Response { id: 3, outcome } = bridge.next() {
            assert!(matches!(outcome, Outcome::Refused(
                plakous_host_protocol::wire::RequestRefusal::RefusedForRole { row }
            ) if row == "local_face"));
            break;
        }
    }
    assert_eq!(crash::dispatches(&fixture), 0);
    bridge.finish();
    stop(&mut host);
}
