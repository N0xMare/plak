use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::thread;

use plakous_host_protocol::wire::{
    ClientFrame, HandshakeRefusal, HostFrame, NodeAddress, SessionActivity, SessionEnvironment,
    SessionResidency, Welcome, decode_line, encode_line,
};

use super::*;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const NEXT: &str = "00000000-0000-4000-8000-000000000002";

fn paths() -> (tempfile::TempDir, product_paths::PlakPaths) {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::Builder::new()
        .prefix("tf")
        .tempdir_in("/tmp")
        .unwrap();
    let paths =
        product_paths::PlakPaths::explicit(root.path().join("h")).for_workspace(root.path());
    std::fs::create_dir_all(paths.host_socket().parent().unwrap()).unwrap();
    std::fs::set_permissions(paths.home(), std::fs::Permissions::from_mode(0o700)).unwrap();
    (root, paths)
}

fn welcome() -> Welcome {
    Welcome {
        proto: WIRE_VERSION,
        binary_id: "fixture".into(),
        host_generation: 1,
        caps: Vec::new(),
        session: None,
    }
}

struct Peer {
    read: BufReader<UnixStream>,
    write: UnixStream,
}

impl Peer {
    fn accept(listener: &UnixListener) -> Self {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut peer = Self {
            read: BufReader::new(stream.try_clone().unwrap()),
            write: stream,
        };
        assert!(matches!(
            peer.read(),
            ClientFrame::Hello(Hello {
                proto: WIRE_VERSION,
                ..
            })
        ));
        peer
    }

    fn read(&mut self) -> ClientFrame<PlakExtension> {
        let mut line = String::new();
        self.read.read_line(&mut line).unwrap();
        decode_line(&line).unwrap()
    }

    fn send(&mut self, frame: HostFrame<PlakExtension>) {
        self.write
            .write_all(encode_line(&frame).unwrap().as_bytes())
            .unwrap();
    }

    fn request(&mut self) -> (RequestId, Op<PlakExtension>) {
        let ClientFrame::Request { id, op } = self.read() else {
            panic!("not a request")
        };
        (id, op)
    }

    fn answer(&mut self, id: RequestId, answer: Answer) {
        self.send(HostFrame::Response {
            id,
            outcome: Outcome::Ok(answer),
        });
    }

    fn attach(&mut self, session: &str, busy: bool) {
        let (id, op) = self.request();
        assert_eq!(
            op,
            Op::Attach {
                node: node(session)
            }
        );
        self.answer(id, Answer::Attached { attachment: id });
        self.send(HostFrame::Event {
            attachment: id,
            seq: 1,
            class: plakous_host_protocol::observation::ObservationClass::State,
            observation: HostEvent::SetBusy(busy),
            blobs: Vec::new(),
        });
    }
}

fn node(session: &str) -> NodeAddress {
    NodeAddress {
        tree_id: session.into(),
        agent_id: "root".into(),
    }
}

fn row() -> SessionRow {
    SessionRow {
        session: SESSION.into(),
        node: node(SESSION),
        title: Some("shared transcript".into()),
        workspace: "/workspace".into(),
        profile: None,
        updated_at: 12,
        residency: SessionResidency::Resident,
        activity: SessionActivity::Idle,
        live_work: plakous_host_protocol::wire::LiveWork::default(),
        environment: SessionEnvironment::Host,
        hosted_elsewhere: None,
    }
}

fn event(face: &SocketFace) -> HostEvent {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match face.try_recv() {
            Ok(event) => return event,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(error) => panic!("{error}"),
        }
        assert!(Instant::now() < until, "no face event");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn no_answering_host_selects_the_embedded_launch() {
    let (_root, paths) = paths();
    assert!(SocketFace::discover(&paths).unwrap().is_none());
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    drop(listener);
    assert!(
        SocketFace::discover(&paths).unwrap().is_none(),
        "a stale socket is not a host"
    );
}

#[test]
fn another_wire_version_refuses_with_a_remedy_instead_of_embedding() {
    for refusal in [true, false] {
        let (_root, paths) = paths();
        let listener = UnixListener::bind(paths.host_socket()).unwrap();
        let host = thread::spawn(move || {
            let mut peer = Peer::accept(&listener);
            peer.send(if refusal {
                HostFrame::Refused(HandshakeRefusal::VersionMismatch {
                    host: WIRE_VERSION + 1,
                    client: WIRE_VERSION,
                })
            } else {
                HostFrame::Welcome(Welcome {
                    proto: WIRE_VERSION + 1,
                    ..welcome()
                })
            });
        });
        let error = SocketFace::discover(&paths).err().unwrap();
        assert!(error.message().contains("stop"), "{error}");
        assert!(error.message().contains("run its build"), "{error}");
        assert!(error.message().contains("no embedded session"), "{error}");
        host.join().unwrap();
    }
}

#[test]
fn directory_new_resume_stop_and_leaving_use_the_host_socket() {
    let (_root, paths) = paths();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let host = thread::spawn(move || {
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        let (id, op) = peer.request();
        assert!(matches!(op, Op::ListSessions { workspace: Some(_) }));
        peer.answer(id, Answer::Sessions { rows: vec![row()] });
        peer.attach(SESSION, false);
        let (id, op) = peer.request();
        let Op::NewSession { request_id, .. } = op else {
            panic!("not new_session")
        };
        assert!(uuid::Uuid::parse_str(&request_id).is_ok());
        peer.answer(
            id,
            Answer::Created {
                session: NEXT.into(),
                node: node(NEXT),
                environment: SessionEnvironment::Host,
            },
        );
        peer.attach(NEXT, true);
        let (id, op) = peer.request();
        assert!(matches!(op, Op::ListSessions { .. }));
        peer.answer(id, Answer::Sessions { rows: vec![row()] });
        let (id, op) = peer.request();
        assert_eq!(
            op,
            Op::StopSession {
                session: NEXT.into()
            }
        );
        peer.answer(id, Answer::Stopped);
        assert!(
            matches!(peer.read(), ClientFrame::Bye),
            "leaving never sends stop or release"
        );
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    face.select(Some(SESSION)).unwrap();
    assert_eq!(event(&face), HostEvent::SetBusy(false));
    face.send(UserAction::NewSession).unwrap();
    assert_eq!(event(&face), HostEvent::SetBusy(true));
    assert_eq!(face.session().as_deref(), Some(NEXT));
    face.send(UserAction::ResumeSession { id: None }).unwrap();
    let HostEvent::ResumeSessions(rows) = event(&face) else {
        panic!("no picker rows")
    };
    assert_eq!(rows[0].id, SESSION);
    assert!(rows[0].in_place);
    assert!(face.stop_session(NEXT));
    assert!(format!("{:?}", event(&face)).contains("session work stopped"));
    drop(face);
    host.join().unwrap();
}

#[test]
fn requested_reload_is_settled_locally_with_the_known_host_build_or_no_parenthetical() {
    use std::os::unix::fs::PermissionsExt as _;
    for binary in ["abcdef012345", "unknown", ""] {
        let (_root, paths) = paths();
        paths.ensure_layout().unwrap();
        let config = paths.home().join("config.toml");
        std::fs::write(&config, "[tui]\nreload_requests = true\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = UnixListener::bind(paths.host_socket()).unwrap();
        let host = thread::spawn(move || {
            let mut peer = Peer::accept(&listener);
            peer.send(HostFrame::Welcome(Welcome {
                binary_id: binary.into(),
                ..welcome()
            }));
            assert!(
                matches!(peer.read(), ClientFrame::Bye),
                "a reload request never reaches the host"
            );
        });
        let face = SocketFace::discover(&paths).unwrap().unwrap();
        face.state().session = Some(SESSION.into());
        let check = UserAction::Extension(crate::surface_protocol::SurfaceIntent::CheckBuild);
        face.send(check.clone()).unwrap();
        let _ = event(&face);
        let (_, path) =
            crate::reload_request::request(&paths, SESSION, "0123456789ab", "0123456789ab")
                .unwrap();
        face.send(check).unwrap();
        let note = format!("{:?}", event(&face));
        assert!(note.contains("requested reload refused: this session runs in plak host"));
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(written["outcome"], "refused");
        let reason = written["reason"].as_str().unwrap();
        assert_eq!(
            reason.contains("(build abcdef012345)"),
            binary == "abcdef012345"
        );
        assert!(!reason.contains("(build unknown)") && !reason.contains("(build )"));
        assert!(reason.contains("plak host stop, or restart its unit"));
        assert_eq!(crate::reload_request::take_reload_continuation(), None);
        drop(face);
        host.join().unwrap();
    }
}

#[test]
fn an_unanswered_new_session_has_unknown_outcome_and_is_never_replayed() {
    let (_root, paths) = paths();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let (sent, read) = std::sync::mpsc::channel();
    let host = thread::spawn(move || {
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        assert!(matches!(peer.request().1, Op::NewSession { .. }));
        sent.send(()).unwrap();
        assert!(matches!(peer.read(), ClientFrame::Bye));
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    face.send(UserAction::NewSession).unwrap();
    read.recv_timeout(Duration::from_secs(5)).unwrap();
    face.state().pending.values_mut().next().unwrap().1 =
        Instant::now().checked_sub(DEADLINE).unwrap();
    assert!(matches!(
        face.try_unsent(),
        Some(client::SendError::OutcomeUnknown(UserAction::NewSession))
    ));
    drop(face);
    host.join().unwrap();
}
