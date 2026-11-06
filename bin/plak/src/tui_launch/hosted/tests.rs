use std::io::{BufRead as _, BufReader};
use std::os::unix::{
    fs::PermissionsExt as _,
    net::{UnixListener, UnixStream},
};
use std::time::{Duration, Instant};

use super::*;
use crate::surface_protocol::{PlakExtension, UserAction};
use plakous_host_protocol::wire::{
    Answer, ClientFrame, Hello, HostFrame, Op, Outcome, WIRE_VERSION, Welcome, decode_line,
    encode_line,
};

fn welcomed(listener: &UnixListener) -> (UnixStream, BufReader<UnixStream>) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(matches!(
        decode_line::<ClientFrame<PlakExtension>>(&line).unwrap(),
        ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            ..
        })
    ));
    let welcome = HostFrame::<PlakExtension>::Welcome(Welcome {
        proto: WIRE_VERSION,
        binary_id: "fixture".into(),
        host_generation: 1,
        caps: Vec::new(),
        session: None,
    });
    stream
        .write_all(encode_line(&welcome).unwrap().as_bytes())
        .unwrap();
    (stream, reader)
}

#[test]
fn a_new_with_a_lost_answer_is_retried_with_its_original_identity() {
    let root = tempfile::Builder::new()
        .prefix("tf")
        .tempdir_in("/tmp")
        .unwrap();
    let paths =
        crate::product_paths::PlakPaths::explicit(root.path().join("h")).for_workspace(root.path());
    std::fs::create_dir_all(paths.host_socket().parent().unwrap()).unwrap();
    std::fs::set_permissions(
        paths.host_socket().parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let listener = UnixListener::bind(paths.host_socket()).unwrap();
    let server = std::thread::spawn(move || {
        let mut original = None;
        for attempt in 0..2 {
            let (mut stream, mut reader) = welcomed(&listener);
            let mut line = String::new();
            if attempt == 1 {
                line.clear();
                reader.read_line(&mut line).unwrap();
                let ClientFrame::Request {
                    id,
                    op: Op::ListSessions { .. },
                } = decode_line::<ClientFrame<PlakExtension>>(&line).unwrap()
                else {
                    panic!("reconnect replayed an unknown new instead of listing")
                };
                stream
                    .write_all(
                        encode_line(&HostFrame::<PlakExtension>::Response {
                            id,
                            outcome: Outcome::Ok(Answer::Sessions { rows: Vec::new() }),
                        })
                        .unwrap()
                        .as_bytes(),
                    )
                    .unwrap();
            }
            line.clear();
            reader.read_line(&mut line).unwrap();
            let ClientFrame::Request {
                op: Op::NewSession { request_id, .. },
                ..
            } = decode_line::<ClientFrame<PlakExtension>>(&line).unwrap()
            else {
                panic!("not new_session")
            };
            if attempt == 0 {
                original = Some(request_id);
            } else {
                assert_eq!(
                    Some(request_id),
                    original,
                    "explicit retry must find the idempotent creation"
                );
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert!(matches!(
                    decode_line::<ClientFrame<PlakExtension>>(&line).unwrap(),
                    ClientFrame::Bye
                ));
            }
        }
    });
    let previous = SocketFace::discover(&paths).unwrap().unwrap();
    previous.send(UserAction::NewSession).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while previous.ended().is_none() {
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    assert!(matches!(
        previous.try_unsent(),
        Some(plakous_host_protocol::client::SendError::OutcomeUnknown(
            UserAction::NewSession
        ))
    ));
    let face = reconnect(&paths, &previous).unwrap();
    assert!(select_or_list(&face, None).unwrap());
    face.send(UserAction::NewSession).unwrap();
    drop(face);
    server.join().unwrap();
}
