use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use plakous_host_protocol::wire::{
    Answer, ClientFrame, Hello, HostFrame, Op, Outcome, WIRE_VERSION,
};
use plakous_hosted::host_service::wire::HostPrincipal;

use super::*;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn short_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Sessions as a test scripts them: none.
#[derive(Default)]
struct NoSessions {
    principals: Arc<Mutex<Vec<HostPrincipal>>>,
}

impl plakous_hosted::host_service::wire::HostSessions<PlakExtension> for NoSessions {
    fn host_request(
        &self,
        context: plakous_hosted::host_service::wire::HostRequestContext,
        _: plakd_wire_types::console::HostRequest,
    ) -> plakous_hosted::host_service::wire::HostReadTask<PlakExtension> {
        self.principals.lock().unwrap().push(context.principal);
        Box::pin(async {
            Ok(plakd_wire_types::console::HostResponse::AccountsStatus {
                as_of: "fixture".into(),
                rows: Vec::new(),
                diagnostics: Vec::new(),
                page: plakd_wire_types::console::Page {
                    next: None,
                    truncated: false,
                },
            })
        })
    }
    fn host_read_limits(&self) -> plakous_hosted::host_service::wire::HostReadLimits {
        crate::host::host_read_limits()
    }
    fn welcome_extension(
        &self,
        _: &HostPrincipal,
        _: &plakous_hosted::host_service::wire::HostReadLimits,
    ) -> <PlakExtension as plakous_host_protocol::ProtocolExtension>::WelcomeExtension {
        None
    }
    fn contains(&self, _: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn rows(
        &self,
        _workspace: Option<&str>,
        _runner: Option<&str>,
        _include_archived: bool,
    ) -> Result<Vec<plakous_host_protocol::wire::SessionRow>, String> {
        Ok(Vec::new())
    }

    fn session_of(
        &self,
        _node: &plakous_host_protocol::wire::NodeAddress,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn open(
        &self,
        _session: &str,
    ) -> CliResult<plakous_hosted::host_service::wire::OpenedSession<PlakExtension>> {
        Err(CliFailure::new("no session"))
    }

    fn refusal(
        &self,
        _session: &str,
        failure: &CliFailure,
    ) -> plakous_host_protocol::wire::RequestRefusal {
        plakous_host_protocol::wire::RequestRefusal::SessionUnavailable {
            detail: failure.message().to_owned(),
        }
    }

    fn create(
        &self,
        _request: &plakous_hosted::host_service::wire::Creation<'_>,
    ) -> Result<
        plakous_hosted::host_service::wire::Created,
        plakous_host_protocol::wire::RequestRefusal,
    > {
        Err(plakous_host_protocol::wire::RequestRefusal::UnknownSession)
    }
}

fn send(client: &mut std::os::unix::net::UnixStream, frame: &ClientFrame<PlakExtension>) {
    client
        .write_all(
            plakous_host_protocol::wire::encode_line(frame)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
}

fn next(reader: &mut BufReader<std::os::unix::net::UnixStream>) -> HostFrame<PlakExtension> {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    plakous_host_protocol::wire::decode_line(line.trim_end()).unwrap()
}

/// The host binds its socket 0600 in private directories, replacing what a
/// dead host left; a connection of its own user is welcomed to no session
/// and reads the session directory; a stop ends the accept loop and the
/// connection; and cleanup removes only the socket this host bound.
#[test]
fn an_admitted_connection_is_welcomed_to_the_directory_and_a_stop_ends_the_host() {
    let home = short_home();
    let socket = home.path().join("host/run/r1.e1.p1.t1.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, "a dead host's socket").unwrap();
    let lock = HostLock::take(&home.path().join("host.lock"), "0123456789ab").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
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
    let sessions = NoSessions::default();
    let principals = Arc::clone(&sessions.principals);
    let host = SessionHost::new(sessions, "0123456789ab", 7);
    let server = std::thread::spawn(move || {
        runtime.block_on(bound.serve(
            &host,
            move || stopping.load(Ordering::Acquire),
            None,
            Arc::new(AtomicBool::new(false)),
        ));
        bound
    });

    let mut client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    send(
        &mut client,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "client".into(),
            caps: Vec::new(),
        }),
    );
    let HostFrame::Welcome(welcome) = next(&mut reader) else {
        panic!("not welcomed");
    };
    assert_eq!(welcome.session, None, "a host of many sessions names none");
    send(
        &mut client,
        &ClientFrame::Request {
            id: 1,
            op: Op::ListSessions {
                runner: None,
                include_archived: false,
                workspace: None,
            },
        },
    );
    assert_eq!(
        next(&mut reader),
        HostFrame::Response {
            id: 1,
            outcome: Outcome::Ok(Answer::Sessions { rows: Vec::new() }),
        }
    );

    send(
        &mut client,
        &ClientFrame::Request {
            id: 2,
            op: Op::Host {
                request: plakd_wire_types::console::HostRequest::AccountsStatus {
                    account_id: None,
                    cursor: None,
                    limit: None,
                },
            },
        },
    );
    assert!(matches!(
        next(&mut reader),
        HostFrame::Response {
            id: 2,
            outcome: Outcome::Ok(Answer::Host { .. })
        }
    ));
    assert_eq!(
        *principals.lock().unwrap(),
        vec![HostPrincipal::Socket(
            std::fs::metadata(home.path()).unwrap().uid()
        )]
    );
    stop.store(true, Ordering::Release);
    let bound = server.join().unwrap();
    // The stop ended the connection too.
    let mut rest = String::new();
    assert_eq!(reader.read_line(&mut rest).unwrap(), 0, "{rest}");
    bound.remove();
    assert!(!socket.exists());
}

/// A hello of another protocol version is refused, typed, and closed.
#[test]
fn a_hello_of_another_version_is_refused() {
    let home = short_home();
    let socket = home.path().join("host/run/v.sock");
    let lock = HostLock::take(&home.path().join("host.lock"), "0123456789ab").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let bound = {
        let _entered = runtime.enter();
        Bound::bind(&socket, &lock).unwrap()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let host = SessionHost::new(NoSessions::default(), "0123456789ab", 7);
    let server = std::thread::spawn(move || {
        runtime.block_on(bound.serve(
            &host,
            move || stopping.load(Ordering::Acquire),
            None,
            Arc::new(AtomicBool::new(false)),
        ));
    });
    let mut client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    send(
        &mut client,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION - 1,
            binary_id: "client".into(),
            caps: Vec::new(),
        }),
    );
    assert_eq!(
        next(&mut reader),
        HostFrame::Refused(HandshakeRefusal::VersionMismatch {
            host: WIRE_VERSION,
            client: WIRE_VERSION - 1,
        })
    );
    let mut rest = String::new();
    assert_eq!(reader.read_line(&mut rest).unwrap(), 0, "{rest}");
    stop.store(true, Ordering::Release);
    server.join().unwrap();
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

/// At most a limit of places are taken at once; a place frees when its
/// connection ends.
#[test]
fn places_are_capped() {
    let count = Arc::new(AtomicUsize::new(0));
    let held: Vec<_> = (0..PENDING_LIMIT)
        .map(|_| Place::take(&count, PENDING_LIMIT).unwrap())
        .collect();
    assert!(Place::take(&count, PENDING_LIMIT).is_none());
    drop(held);
    assert_eq!(count.load(Ordering::Acquire), 0);
    assert!(Place::take(&count, PENDING_LIMIT).is_some());
}
