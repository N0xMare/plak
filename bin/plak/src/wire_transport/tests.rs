use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::Duration;

use plakous_host_protocol::SessionIntent;
use plakous_host_protocol::client;
use plakous_host_protocol::wire::{Hello, LineReader, WIRE_VERSION};
use plakous_hosted::host_service::wire::{Face, WireSession, handshake, session_welcome};

use super::*;
use crate::surface_protocol::{PlakExtension, SurfaceIntent, user_action_channel};

fn private_run() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let run = root.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    (root, run)
}

#[test]
fn lifecycle_forwarding_never_turns_a_host_refusal_or_unknown_outcome_into_offline_permission() {
    use plakous_host_protocol::wire::{Answer, Op, Outcome};

    let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    assert!(
        forward_lifecycle(
            &paths,
            Op::ArchiveSession {
                session: "s".into(),
                archived: true
            }
        )
        .unwrap()
        .is_none()
    );
    let directory = paths.host_socket().parent().unwrap().to_owned();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let operations = [
        Op::RenameSession {
            session: "s".into(),
            title: None,
        },
        Op::ArchiveSession {
            session: "s".into(),
            archived: true,
        },
        Op::ArchiveSession {
            session: "s".into(),
            archived: false,
        },
        Op::DeleteSession {
            session: "s".into(),
            abandon_children: false,
        },
    ];
    for (index, op) in operations.into_iter().enumerate() {
        let listener = UnixListener::bind(paths.host_socket()).unwrap();
        let expected = op.clone();
        let peer = std::thread::spawn(move || lifecycle_peer(listener, expected, index));
        let result = forward_lifecycle(&paths, op);
        peer.join().unwrap();
        match index {
            0 => assert!(matches!(
                result.unwrap(),
                Some(Outcome::Ok(Answer::Renamed { .. }))
            )),
            3 => assert!(result.is_err()),
            _ => assert!(matches!(result.unwrap(), Some(Outcome::Refused(_)))),
        }
        std::fs::remove_file(paths.host_socket()).unwrap();
    }
}

fn lifecycle_peer(
    listener: UnixListener,
    expected: plakous_host_protocol::wire::Op<PlakExtension>,
    index: usize,
) {
    use plakous_host_protocol::wire::{
        Answer, ClientFrame, HostFrame, Outcome, RequestRefusal, Welcome, decode_line, encode_line,
    };
    use std::io::{BufRead as _, Write as _};
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = io::BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(matches!(
        decode_line::<ClientFrame<PlakExtension>>(&line).unwrap(),
        ClientFrame::Hello(_)
    ));
    stream
        .write_all(
            encode_line(&HostFrame::<PlakExtension>::Welcome(Welcome {
                proto: WIRE_VERSION,
                binary_id: "fixture".into(),
                host_generation: 1,
                caps: Vec::new(),
                session: None,
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let ClientFrame::Request { id, op } = decode_line::<ClientFrame<PlakExtension>>(&line).unwrap()
    else {
        panic!("expected lifecycle request")
    };
    assert_eq!(op, expected);
    if index == 3 {
        return;
    }
    let outcome = if index == 0 {
        Outcome::Ok(Answer::Renamed {
            session: "s".into(),
            title: None,
        })
    } else {
        Outcome::Refused(RequestRefusal::SessionUnavailable {
            detail: "refused by fixture".into(),
        })
    };
    stream
        .write_all(
            encode_line(&HostFrame::<PlakExtension>::Response { id, outcome })
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
}

/// The host socket is used only in a directory private to this user and
/// only when it is a socket.
#[test]
fn the_host_socket_must_be_this_users_and_private() {
    let (_root, run) = private_run();
    let path = run.join("h.sock");
    std::fs::write(&path, b"").unwrap();
    let refused = |path: &Path| host_socket(path).err().map(|error| error.kind());
    assert_eq!(refused(&path), Some(io::ErrorKind::PermissionDenied));
    std::fs::remove_file(&path).unwrap();
    let _listener = UnixListener::bind(&path).unwrap();
    assert!(host_socket(&path).is_ok());
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert_eq!(refused(&path), Some(io::ErrorKind::PermissionDenied));
}

fn lines_of(stream: UnixStream) -> plakous_hosted::host_service::wire::Lines {
    use std::io::Read as _;
    let (sender, lines) = plakous_hosted::host_service::wire::lines(64);
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut reader = LineReader::default();
        let mut buffer = vec![0_u8; 64 * 1024];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                return;
            }
            for line in reader.push(&buffer[..read]) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        }
    });
    lines
}

/// Over the host socket, a session served to a remote face refuses the
/// installed-build check, a local-face-only intent: the client gives the
/// intent back as refused, and the session never sees it. The client
/// itself does not know its face; the acceptor decides.
#[test]
fn a_remote_face_is_refused_a_local_only_intent_over_the_socket() {
    let (_root, run) = private_run();
    let path = run.join("h.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (actions, intents) = user_action_channel();
    let (_published, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let root = plakous_host_protocol::wire::NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    };
    let session = WireSession::start(root, "s1".to_owned(), actions, events, |_| None);
    let (accepted, served) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        accepted.send(stream).unwrap();
    });
    let transport = host_socket(&path).unwrap();
    let stream = served.recv().unwrap();
    let mut connection = session.connect(stream.try_clone().unwrap(), Face::Remote, || false);
    let (id, node) = connection.welcomed().unwrap();
    let welcome = session_welcome("host".into(), 1, id, node);
    let lines = lines_of(stream.try_clone().unwrap());
    let host = std::thread::spawn(move || {
        let mut out = stream.try_clone().unwrap();
        assert!(handshake::<PlakExtension>(&lines, welcome, &mut out, &|| false).unwrap());
        while let Ok(line) = lines.recv_timeout(Duration::from_secs(10)) {
            if !connection.frame(line).unwrap() {
                break;
            }
            connection.flush().unwrap();
        }
    });
    let (sender, receiver, _) = client::connect::<PlakExtension>(transport, hello()).unwrap();
    let check = SessionIntent::Extension(SurfaceIntent::CheckBuild);
    sender.send(check.clone()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let refused = loop {
        if let Some(refused) = receiver.try_unsent() {
            break refused;
        }
        assert!(std::time::Instant::now() < deadline, "no verdict came");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(
        matches!(&refused, client::SendError::RefusedForRole { intent, .. } if *intent == check),
        "{refused:?}"
    );
    while let Ok(delivery) = intents.try_recv() {
        assert!(
            !matches!(*delivery, SessionIntent::Extension(_)),
            "the session took {:?}",
            *delivery
        );
    }
    drop((sender, receiver));
    host.join().unwrap();
}

fn hello() -> Hello {
    Hello {
        proto: WIRE_VERSION,
        binary_id: "client".into(),
        caps: Vec::new(),
    }
}

/// The peer decides: only this user's uid is admitted, and a peer whose
/// credentials cannot be read is refused.
#[test]
fn only_this_users_peer_is_admitted() {
    assert!(peer_is(Ok(501), 501).is_ok());
    assert_eq!(
        peer_is(Ok(502), 501).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        peer_is(Err(io::Error::other("no credentials")), 501)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let (ours, theirs) = UnixStream::pair().unwrap();
    assert_eq!(
        peer_uid(&ours).unwrap(),
        rustix::process::geteuid().as_raw()
    );
    drop(theirs);
}
