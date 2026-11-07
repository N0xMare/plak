use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
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
    std::fs::set_permissions(
        paths.host_socket().parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
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
    face.restore_new_request(Some("an-unknown-earlier-creation".into()));
    face.select(Some(SESSION)).unwrap();
    assert!(
        face.state().new_request.is_none(),
        "explicit selection retires an unknown creation"
    );
    assert_eq!(event(&face), HostEvent::SetBusy(false));
    face.send(UserAction::NewSession).unwrap();
    assert_eq!(event(&face), HostEvent::SetBusy(true));
    assert_eq!(face.session().as_deref(), Some(NEXT));
    face.restore_new_request(Some("another-unknown-creation".into()));
    face.send(UserAction::ResumeSession { id: None }).unwrap();
    assert!(
        face.state().new_request.is_none(),
        "explicit resume retires an unknown creation"
    );
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
fn save_settings_over_the_socket_preserves_terminal_preferences() {
    use crate::shell::settings::{ProductSurface, settings_view};
    use crate::surface_protocol::{SurfaceIntent, SurfaceObservation};
    use plakous_hosted::runtime::{Sender, SurfaceFacts, SurfaceHandler, SurfacePhase, UsageReads};

    let (_root, paths) = paths();
    let mut config = crate::product_config::ProductConfig::default_product();
    config.theme = crate::product_config::ProductTheme::CatppuccinLatte;
    config.tui_mouse = false;
    config.scroll_lines = 9;
    config.show_thinking = false;
    config.group_tool_calls = false;
    config.reduced_motion = true;
    config.save(&paths).unwrap();
    let expected = settings_view(&config).tui;
    let mut update = settings_view(&config);
    update.product.compaction = !config.compaction_enabled;
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let server_paths = paths.clone();
    let host = thread::spawn(move || {
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        let (id, _) = peer.request();
        peer.answer(id, Answer::Sessions { rows: vec![row()] });
        peer.attach(SESSION, false);
        let (
            _,
            Op::Intent {
                attachment,
                intent: UserAction::Extension(SurfaceIntent::SaveSettings(decoded)),
                ..
            },
        ) = peer.request()
        else {
            panic!("not save_settings")
        };
        assert_eq!(
            decoded.tui,
            crate::tui_preferences::TuiPreferences::default()
        );
        let (events, observed) = crate::surface_protocol::host_event_channel();
        ProductSurface::new(server_paths.clone()).dispatch(
            SurfaceIntent::SaveSettings(decoded),
            SurfaceFacts {
                phase: SurfacePhase::Idle,
                attached_one_shot: false,
                catalog: &[],
                queue_len: 0,
                session: None,
                usage_reads: UsageReads::default(),
            },
            &Sender::over(events),
        );
        let saved = crate::product_config::ProductConfig::load(&server_paths).unwrap();
        assert_eq!(settings_view(&saved).tui, expected);
        assert_ne!(saved.compaction_enabled, config.compaction_enabled);
        let applied = observed
            .try_iter()
            .find(|event| {
                matches!(
                    event,
                    HostEvent::Extension(SurfaceObservation::SettingsViewApplied(_))
                )
            })
            .unwrap();
        peer.send(HostFrame::Event {
            attachment,
            seq: 2,
            class: plakous_host_protocol::observation::ObservationClass::Addressed,
            observation: applied,
            blobs: Vec::new(),
        });
        assert!(matches!(peer.read(), ClientFrame::Bye));
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    face.select(Some(SESSION)).unwrap();
    assert_eq!(event(&face), HostEvent::SetBusy(false));
    face.send(UserAction::Extension(SurfaceIntent::SaveSettings(update)))
        .unwrap();
    assert!(matches!(
        event(&face),
        HostEvent::Extension(SurfaceObservation::SettingsViewApplied(_))
    ));
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

#[test]
fn a_non_private_run_directory_refuses_instead_of_embedding() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_root, paths) = paths();
    std::fs::set_permissions(
        paths.host_socket().parent().unwrap(),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let error = SocketFace::discover(&paths)
        .err()
        .expect("not an embedded fallback");
    assert!(error.message().contains("private to this user"), "{error}");
}

#[test]
fn an_unbindable_socket_path_uses_the_embedded_launcher() {
    let (root, _) = paths();
    let paths = product_paths::PlakPaths::explicit(root.path().join("h".repeat(120)))
        .for_workspace(root.path());
    assert!(SocketFace::discover(&paths).unwrap().is_none());
}

#[test]
fn a_locked_host_without_a_socket_refuses_until_its_lock_is_free() {
    let (_root, paths) = paths();
    std::fs::create_dir_all(paths.host_lock().parent().unwrap()).unwrap();
    std::fs::write(paths.host_lock(), "4242 fixture\n").unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.host_lock())
        .unwrap();
    lock.try_lock().unwrap();
    for stale in [false, true] {
        if stale {
            drop(UnixListener::bind(paths.host_socket()).unwrap());
        }
        let error = discovery::connect(&paths, Duration::ZERO).err().unwrap();
        assert!(error.message().contains("pid 4242"), "{error}");
        assert!(
            error.message().contains("starting or stopping; retry"),
            "{error}"
        );
    }
    drop(lock);
    assert!(SocketFace::discover(&paths).unwrap().is_none());
}

#[test]
fn a_host_that_answers_during_its_start_window_is_used() {
    let (_root, paths) = paths();
    std::fs::create_dir_all(paths.host_lock().parent().unwrap()).unwrap();
    std::fs::write(paths.host_lock(), "4242 fixture\n").unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.host_lock())
        .unwrap();
    lock.try_lock().unwrap();
    let socket = paths.host_socket();
    let peer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let listener = UnixListener::bind(socket).unwrap();
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        assert!(matches!(peer.read(), ClientFrame::Bye));
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    drop(face);
    peer.join().unwrap();
}

#[test]
fn transport_loss_settles_new_and_stop_immediately_without_forgetting_new_identity() {
    let (_root, paths) = paths();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let host = thread::spawn(move || {
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        assert!(matches!(peer.request().1, Op::NewSession { .. }));
        assert!(matches!(peer.request().1, Op::StopSession { .. }));
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    face.send(UserAction::NewSession).unwrap();
    let request = face.state().new_request.clone().unwrap();
    assert!(face.stop_session(SESSION));
    host.join().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while face.receiver.ended().is_none() {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    assert!(matches!(
        face.try_unsent(),
        Some(client::SendError::OutcomeUnknown(UserAction::NewSession))
    ));
    assert!(format!("{:?}", event(&face)).contains("stop outcome unknown"));
    assert_eq!(face.take_new_request().as_deref(), Some(request.as_str()));
    face.restore_new_request(Some(request.clone()));
    let Op::NewSession { request_id, .. } = face.new_session() else {
        panic!("not new")
    };
    assert_eq!(request_id, request);
}

#[test]
fn a_hosted_elsewhere_row_refuses_locally_and_keeps_the_current_session() {
    let (_root, paths) = paths();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let host = thread::spawn(move || {
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        let (id, _) = peer.request();
        let mut held = row();
        held.hosted_elsewhere = Some(plakous_host_protocol::wire::HostedElsewhere {
            host_name: "holder".into(),
            binary: None,
            pid: 4242,
        });
        peer.answer(
            id,
            Answer::Sessions {
                rows: vec![held.clone()],
            },
        );
        let (id, _) = peer.request();
        peer.answer(id, Answer::Sessions { rows: vec![held] });
        assert!(
            matches!(peer.read(), ClientFrame::Bye),
            "no refused row is attached"
        );
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    face.state().session = Some(NEXT.into());
    let error = face.select(Some(SESSION)).unwrap_err();
    assert!(
        error.message().contains("pid 4242") && error.message().contains("release_session"),
        "{error}"
    );
    face.send(UserAction::ResumeSession {
        id: Some(SESSION.into()),
    })
    .unwrap();
    assert!(format!("{:?}", event(&face)).contains("pid 4242"));
    assert_eq!(face.session().as_deref(), Some(NEXT));
    drop(face);
    host.join().unwrap();
}

#[test]
fn two_remote_faces_preserve_the_live_announcement_and_either_refuses_its_request() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_root, paths) = paths();
    paths.ensure_layout().unwrap();
    let config = paths.home().join("config.toml");
    std::fs::write(&config, "[tui]\nreload_requests = true\n").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let host = thread::spawn(move || {
        let mut peers = Vec::new();
        for _ in 0..2 {
            let mut peer = Peer::accept(&listener);
            peer.send(HostFrame::Welcome(welcome()));
            peers.push(peer);
        }
        for mut peer in peers {
            assert!(matches!(peer.read(), ClientFrame::Bye));
        }
    });
    let first = SocketFace::discover(&paths).unwrap().unwrap();
    let second = SocketFace::discover(&paths).unwrap().unwrap();
    first.state().session = Some(SESSION.into());
    second.state().session = Some(SESSION.into());
    let check = UserAction::Extension(crate::surface_protocol::SurfaceIntent::CheckBuild);
    first.send(check.clone()).unwrap();
    let announcement = paths
        .run_dir()
        .join("reload-requests")
        .join(format!("{SESSION}.listener"));
    let before = std::fs::read(&announcement).unwrap();
    second.send(check.clone()).unwrap();
    assert_eq!(std::fs::read(&announcement).unwrap(), before);
    let (_, outcome) =
        crate::reload_request::request(&paths, SESSION, "0123456789ab", "0123456789ab").unwrap();
    second.send(check.clone()).unwrap();
    first.send(check).unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(outcome).unwrap()).unwrap();
    assert_eq!(written["outcome"], "refused");
    assert!(
        written["reason"]
            .as_str()
            .unwrap()
            .contains("this session runs in plak host")
    );
    let notes: Vec<_> = std::iter::from_fn(|| second.try_recv().ok()).collect();
    assert!(format!("{notes:?}").contains("requested reload refused"));
    assert_eq!(std::fs::read(&announcement).unwrap(), before);
    let (_, outcome) =
        crate::reload_request::request(&paths, SESSION, "0123456789ab", "0123456789ab").unwrap();
    let mut dead: serde_json::Value = serde_json::from_slice(&before).unwrap();
    dead["pid"] = serde_json::json!(i32::MAX);
    std::fs::write(&announcement, serde_json::to_vec(&dead).unwrap()).unwrap();
    second
        .send(UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::CheckBuild,
        ))
        .unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(outcome).unwrap()).unwrap();
    assert_eq!(
        written["outcome"], "refused",
        "re-announcement must not erase a pending request"
    );
    drop((first, second));
    host.join().unwrap();
}

#[test]
fn an_attach_refusal_keeps_the_previous_selection_and_names_the_request_failure() {
    let (_root, paths) = paths();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let (seen, seen_attach) = std::sync::mpsc::channel();
    let (allow, proceed) = std::sync::mpsc::channel();
    let host = thread::spawn(move || {
        let mut peer = Peer::accept(&listener);
        peer.send(HostFrame::Welcome(welcome()));
        let (id, _) = peer.request();
        peer.answer(id, Answer::Sessions { rows: vec![row()] });
        peer.attach(SESSION, false);
        let (id, _) = peer.request();
        let mut next = row();
        next.session = NEXT.into();
        next.node = node(NEXT);
        peer.answer(id, Answer::Sessions { rows: vec![next] });
        let (id, op) = peer.request();
        assert!(matches!(op, Op::Attach { .. }));
        seen.send(()).unwrap();
        proceed.recv().unwrap();
        peer.send(HostFrame::Response {
            id,
            outcome: Outcome::Refused(
                plakous_host_protocol::wire::RequestRefusal::SessionUnavailable {
                    detail: "node retiring".into(),
                },
            ),
        });
        let (id, op) = peer.request();
        assert!(matches!(op, Op::ListSessions { .. }));
        peer.answer(id, Answer::Sessions { rows: Vec::new() });
        assert!(matches!(peer.read(), ClientFrame::Bye));
    });
    let face = SocketFace::discover(&paths).unwrap().unwrap();
    face.select(Some(SESSION)).unwrap();
    assert_eq!(event(&face), HostEvent::SetBusy(false));
    face.send(UserAction::ResumeSession {
        id: Some(NEXT.into()),
    })
    .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while seen_attach.try_recv().is_err() {
        face.responses();
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    assert_eq!(
        face.session().as_deref(),
        Some(SESSION),
        "a requested attach is not an attached session"
    );
    allow.send(()).unwrap();
    let note = format!("{:?}", event(&face));
    assert!(
        note.contains("attach refused") && note.contains("node retiring"),
        "{note}"
    );
    assert_eq!(face.session().as_deref(), Some(SESSION));
    assert_eq!(face.ended(), None);
    face.send(UserAction::ResumeSession {
        id: Some("missing".into()),
    })
    .unwrap();
    assert!(format!("{:?}", event(&face)).contains("resume refused"));
    drop(face);
    host.join().unwrap();
}
