use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::Duration;

use plakous_host_protocol::client;
use plakous_host_protocol::wire::{ClientLocality, Hello, LineReader, WIRE_VERSION, WireError};
use plakous_host_protocol::{IntentSendError, SessionIntent};
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

fn lines_of(stream: UnixStream) -> mpsc::Receiver<Result<String, WireError>> {
    use std::io::Read as _;
    let (sender, lines) = mpsc::sync_channel(64);
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
    assert!(matches!(
        sender.send(check.clone()),
        Err(IntentSendError::Refused(refused)) if refused == check
    ));
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
        locality: ClientLocality::default(),
    }
}
