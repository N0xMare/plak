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

#[tokio::test(start_paused = true)]
async fn slow_link_stop_preempts_a_progressing_host_send() {
    let (write, mut read) = tokio::io::duplex(8192);
    let mut write = ProgressIo::new(write);
    let outbox = Outbox::default();
    outbox
        .clone()
        .write_all(&vec![42; 2 * 1024 * 1024])
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let reader = tokio::spawn(async move {
        let mut chunk = [0; 8192];
        while read.read(&mut chunk).await.unwrap_or(0) != 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        stopping.store(true, Ordering::Release);
    });
    let began = tokio::time::Instant::now();
    assert_eq!(
        super::send(&mut write, &outbox, || stop.load(Ordering::Acquire)).await,
        Err(())
    );
    assert!(began.elapsed() <= Duration::from_millis(2050));
    drop(write);
    reader.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn slow_link_socket_stalled_reader_detaches_without_affecting_healthy_client() {
    let (write, _unread) = tokio::io::duplex(1);
    let mut write = ProgressIo::new(write);
    let outbox = Outbox::default();
    outbox.clone().write_all(&[42; 100]).unwrap();
    let (healthy, mut reading) = tokio::io::duplex(64);
    let mut healthy = ProgressIo::new(healthy);
    let healthy_outbox = Outbox::default();
    healthy_outbox.clone().write_all(b"healthy").unwrap();
    let began = tokio::time::Instant::now();
    let (stalled, delivered) = tokio::join!(
        super::send(&mut write, &outbox, || false),
        super::send(&mut healthy, &healthy_outbox, || false)
    );
    assert_eq!(stalled, Err(()));
    assert_eq!(delivered, Ok(()));
    assert_eq!(
        tokio::time::Instant::now().duration_since(began),
        super::super::send::IDLE
    );
    let mut received = [0; 7];
    reading.read_exact(&mut received).await.unwrap();
    assert_eq!(&received, b"healthy");
}

#[tokio::test(start_paused = true)]
async fn slow_link_socket_large_baseline_completes_at_low_bandwidth() {
    let baseline = vec![42; 2 * 1024 * 1024];
    let outbox = Outbox::default();
    outbox.clone().write_all(&baseline).unwrap();
    let (write, mut read) = tokio::io::duplex(8192);
    let mut write = ProgressIo::new(write);
    let draining = tokio::spawn(async move {
        let mut received = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            let count = read.read(&mut chunk).await.unwrap();
            if count == 0 {
                return received;
            }
            received.extend_from_slice(&chunk[..count]);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let began = tokio::time::Instant::now();
    super::send(&mut write, &outbox, || false).await.unwrap();
    assert!(tokio::time::Instant::now().duration_since(began) > super::super::send::IDLE);
    drop(write);
    assert_eq!(draining.await.unwrap(), baseline);
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn short_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Sessions as a test scripts them: none.
#[derive(Default)]
pub(in crate::host) struct NoSessions {
    principals: Arc<Mutex<Vec<HostPrincipal>>>,
    durable: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl NoSessions {
    pub(in crate::host) fn with_durable(check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            durable: Some(Arc::new(check)),
            ..Self::default()
        }
    }
}

impl plakous_hosted::host_service::wire::HostSessions<PlakExtension> for NoSessions {
    fn durable_idle_exit_blocked(&self) -> bool {
        self.durable.as_ref().is_some_and(|check| check())
    }
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
    let mut bound = {
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
    let mut bound = {
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

#[test]
fn serve_idle_expiry_hides_discovery_before_releasing_the_home_lock() {
    for missing in [false, true] {
        let home = short_home();
        let socket = home.path().join("host/run/idle.sock");
        let lock_path = home.path().join("host.lock");
        let lock = HostLock::take(&lock_path, "idle").unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut bound = Bound::bind(&socket, &lock).unwrap();
            if missing {
                std::fs::remove_file(&socket).unwrap();
            }
            let host = SessionHost::new(NoSessions::default(), "idle", 7);
            tokio::time::timeout(
                Duration::from_secs(8),
                bound.serve(
                    &host,
                    || false,
                    Some(super::super::idle::IdleExit::with_window(
                        Duration::from_millis(1),
                    )),
                    Arc::new(AtomicBool::new(false)),
                ),
            )
            .await
            .unwrap();
            assert!(!socket.exists(), "serve returned before hiding discovery");
            assert!(matches!(
                std::fs::File::open(&lock_path).unwrap().try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        });
    }
}

#[test]
fn serve_welcomes_a_connection_queued_before_the_expiry_tick() {
    let home = short_home();
    let socket = home.path().join("host/run/queued.sock");
    let lock = HostLock::take(&home.path().join("host.lock"), "idle").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut bound = {
        let _entered = runtime.enter();
        Bound::bind(&socket, &lock).unwrap()
    };
    let checks = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&checks);
    let sessions = NoSessions::with_durable(move || {
        count.fetch_add(1, Ordering::AcqRel);
        false
    });
    let host = SessionHost::new(sessions, "idle", 7);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let (queued, receiver) = std::sync::mpsc::channel();
    let did_queue = AtomicBool::new(false);
    let server = std::thread::spawn(move || {
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                bound.serve(
                    &host,
                    move || {
                        // This callback runs inside the tick arm, after select has
                        // chosen expiry rather than the ordinary accept branch.
                        if checks.load(Ordering::Acquire) >= 2
                            && !did_queue.swap(true, Ordering::AcqRel)
                        {
                            let mut client =
                                std::os::unix::net::UnixStream::connect(&socket).unwrap();
                            client
                                .set_read_timeout(Some(Duration::from_secs(5)))
                                .unwrap();
                            send(
                                &mut client,
                                &ClientFrame::Hello(Hello {
                                    proto: WIRE_VERSION,
                                    binary_id: "queued".into(),
                                    caps: Vec::new(),
                                }),
                            );
                            queued.send(client).unwrap();
                        }
                        stopping.load(Ordering::Acquire)
                    },
                    Some(super::super::idle::IdleExit::with_window(
                        Duration::from_millis(1),
                    )),
                    Arc::new(AtomicBool::new(false)),
                ),
            )
            .await
            .unwrap();
        });
    });
    let client = receiver.recv_timeout(Duration::from_secs(8)).unwrap();
    assert!(matches!(
        next(&mut BufReader::new(client)),
        HostFrame::Welcome(_)
    ));
    stop.store(true, Ordering::Release);
    server.join().unwrap();
}

#[test]
fn a_failed_idle_rebind_is_an_error_not_an_invisible_live_listener() {
    let home = short_home();
    let socket = home.path().join("host/run/rebind.sock");
    let lock = HostLock::take(&home.path().join("host.lock"), "idle").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mut bound = Bound::bind(&socket, &lock).unwrap();
    let _queued = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    assert!(
        bound
            .retire_idle_listener_with(|_| Err(CliFailure::new("bind unavailable")))
            .is_err()
    );
    assert!(!socket.exists());
}

#[test]
fn serve_stops_on_idle_retirement_failure() {
    let home = short_home();
    let socket = home.path().join("host/run/failure.sock");
    let lock = HostLock::take(&home.path().join("host.lock"), "idle").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut bound = Bound::bind(&socket, &lock).unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::create_dir(&socket).unwrap();
        let host = SessionHost::new(NoSessions::default(), "idle", 7);
        tokio::time::timeout(
            Duration::from_secs(8),
            bound.serve(
                &host,
                || false,
                Some(super::super::idle::IdleExit::with_window(
                    Duration::from_millis(1),
                )),
                Arc::new(AtomicBool::new(false)),
            ),
        )
        .await
        .unwrap();
    });
}

#[test]
fn a_connection_queued_at_idle_expiry_is_welcomed_and_discovery_can_retry() {
    let home = short_home();
    let socket = home.path().join("host/run/expiry.sock");
    let lock = HostLock::take(&home.path().join("host.lock"), "0123456789ab").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    let mut bound = Bound::bind(&socket, &lock).unwrap();
    let mut idle = super::super::idle::IdleExit::with_window(Duration::from_millis(1));
    let now = Instant::now();
    assert!(!idle.expired(now, false));
    // Queue after the last ordinary accept, exactly as the clock expires.
    let mut client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    assert!(idle.expired(now + Duration::from_millis(1), false));
    let mut backlog = bound.retire_idle_listener().unwrap();
    assert_eq!(backlog.len(), 1);
    assert!(
        socket.exists(),
        "cancelled retirement must rebind under the lock"
    );
    idle.activity();
    assert!(!idle.expired(now + Duration::from_millis(2), false));
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send(
        &mut client,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "expiry".into(),
            caps: Vec::new(),
        }),
    );
    let host = SessionHost::new(NoSessions::default(), "0123456789ab", 7);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let place = Place::take(&Arc::new(AtomicUsize::new(0)), PENDING_LIMIT).unwrap();
    let admitted = Arc::new(AtomicUsize::new(0));
    let stream = backlog.pop().unwrap();
    let serving = runtime.spawn(async move {
        connection(
            stream,
            &host,
            Arc::new(move || stopping.load(Ordering::Acquire)),
            place,
            &admitted,
        )
        .await;
    });
    assert!(matches!(
        next(&mut BufReader::new(client)),
        HostFrame::Welcome(_)
    ));
    stop.store(true, Ordering::Release);
    runtime.block_on(serving).unwrap();
    // With no backlog, discovery gets an absent path, never a connected EOF.
    assert!(bound.retire_idle_listener().unwrap().is_empty());
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&socket)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(matches!(
        std::fs::File::open(home.path().join("host.lock"))
            .unwrap()
            .try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(entered);
}
