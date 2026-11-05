use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn short_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// The host binds its socket 0600 in private directories, replacing what a
/// dead host left; a connection of its own user is refused `unavailable`
/// after its hello; a stop ends the accept loop; and cleanup removes only
/// the socket this host bound.
#[test]
fn an_admitted_connection_is_refused_unavailable_and_a_stop_ends_the_host() {
    let home = short_home();
    let socket = home.path().join("host/run/r1.e1.p1.t1.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, "a dead host's socket").unwrap();
    let lock = HostLock::take(&home.path().join("host.lock"), "0123456789ab").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let bound = {
        let _entered = runtime.enter();
        Bound::bind(&socket, &lock).unwrap()
    };
    assert_eq!(mode(&home.path().join("host")), 0o700);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
    assert_eq!(mode(&socket), 0o600);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        runtime.block_on(bound.serve(move || stopping.load(Ordering::Acquire)));
        bound
    });

    let mut client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    client.write_all(b"{\"hello\":{\"proto\":2}}\n").unwrap();
    let mut line = String::new();
    BufReader::new(&client).read_line(&mut line).unwrap();
    let frame: plakous_host_protocol::wire::HostFrame<PlakExtension> =
        plakous_host_protocol::wire::decode_line(line.trim_end()).unwrap();
    assert!(
        matches!(
            &frame,
            plakous_host_protocol::wire::HostFrame::Refused(HandshakeRefusal::Unavailable { detail })
                if detail == NO_SESSION
        ),
        "{line}"
    );

    stop.store(true, Ordering::Release);
    let bound = server.join().unwrap();
    bound.remove();
    assert!(!socket.exists());
}

/// Cleanup leaves a socket file another host bound at the path since.
#[test]
fn cleanup_leaves_a_socket_it_did_not_bind() {
    let home = short_home();
    let socket = home.path().join("host/run/k.sock");
    let lock = HostLock::take(&home.path().join("host.lock"), "0123456789ab").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let bound = Bound::bind(&socket, &lock).unwrap();
    std::fs::remove_file(&socket).unwrap();
    std::fs::write(&socket, "another host's").unwrap();
    bound.remove();
    assert!(socket.exists());
}

#[test]
fn only_the_directory_owner_is_admitted() {
    assert!(admits(Some(501), 501));
    assert!(!admits(Some(0), 501));
    assert!(!admits(None, 501));
}

#[test]
fn a_socket_path_past_the_platform_limit_is_refused_by_name() {
    assert!(check_length(Path::new("/tmp/h/host/run/r1.e1.p1.t1.sock")).is_ok());
    let long = PathBuf::from(format!("/tmp/{}/host/run/k.sock", "d".repeat(100)));
    let refused = check_length(&long).unwrap_err();
    assert!(refused.message().contains(&long.display().to_string()));
}

/// At most the pending limit of connections wait for their hello at once;
/// a place frees when its connection ends.
#[test]
fn pending_connections_are_capped() {
    let count = Arc::new(AtomicUsize::new(0));
    let held: Vec<_> = (0..PENDING_LIMIT)
        .map(|_| Pending::take(&count).unwrap())
        .collect();
    assert!(Pending::take(&count).is_none());
    drop(held);
    assert_eq!(count.load(Ordering::Acquire), 0);
    assert!(Pending::take(&count).is_some());
}
