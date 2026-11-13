//! Real daemon death uses an independently connected deterministic job, never
//! a remembered PID. Dropping its private control socket releases the fixture.

use super::*;
use plakous_tools::internal::runner::RunnerBackend;
use std::io::{Read as _, Write as _};
use std::process::{Child, Command, Stdio};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

#[derive(Clone, Copy)]
enum CutKind {
    Transfer,
    Write,
}

pub(super) struct ReplyCut {
    kind: CutKind,
    fired: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ReplyCut {
    fn new(kind: CutKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            fired: std::sync::atomic::AtomicBool::new(false),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
}

pub(super) async fn serve_cut(
    service: RunnerService,
    mut stream: UnixStream,
    origin: wire::Origin,
    cut: Arc<ReplyCut>,
) {
    use crate::retention::{receive, send};
    send(
        &mut stream,
        &wire::Hello {
            origin: origin.clone(),
        },
    )
    .await
    .unwrap();
    let scope: wire::Bind = receive(&mut stream).await.unwrap();
    send(&mut stream, &Answer::Ok {}).await.unwrap();
    while let Ok(call) = receive::<_, wire::Call>(&mut stream).await {
        let selected = matches!(
            (&call.request, cut.kind),
            (wire::Request::Transfer { .. }, CutKind::Transfer)
                | (wire::Request::Write { .. }, CutKind::Write)
        );
        let id = call.id.clone();
        let answer = service.retention_call(&scope, call, &origin).await;
        if selected && !cut.fired.swap(true, std::sync::atomic::Ordering::AcqRel) {
            cut.reached.notify_one();
            cut.release.notified().await;
        }
        if send(&mut stream, &wire::Reply { id, answer })
            .await
            .is_err()
        {
            return;
        }
    }
}

struct Daemon {
    child: Child,
    directory: PathBuf,
    revision: String,
}

impl Daemon {
    fn spawn(root: &Path, revision: &str) -> Self {
        let version = VersionTriple::current(revision, "retention-fixture");
        let directory = root
            .join("builds")
            .join(crate::daemon::triple_key(&version));
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service::delegation::tests::loss::retention_fixture_daemon",
                "--ignored",
            ])
            .env("PLAK_RETENTION_FIXTURE_ROOT", root)
            .env("PLAK_RETENTION_FIXTURE_REVISION", revision)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            child,
            directory,
            revision: revision.into(),
        }
    }

    async fn port(
        &mut self,
        root: &Path,
        instance: &str,
        fence: u64,
    ) -> (crate::RunnerClient, RunnerSessions) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            if let Ok(stream) = UnixStream::connect(self.directory.join("native.sock")).await {
                break stream;
            }
            assert_eq!(
                self.child.try_wait().unwrap(),
                None,
                "fixture daemon exited"
            );
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let bind = Bind {
            host_id: "home".into(),
            host_instance: instance.into(),
            version: VersionTriple::current(&self.revision, "retention-fixture"),
            inspect: false,
        };
        let (client, hello) = crate::RunnerClient::connect(stream, bind, ClientWindows::default())
            .await
            .unwrap();
        let backend =
            Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
        let environment = runner_environment(
            run_scope(),
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
            backend.clone(),
        )
        .unwrap();
        let owner = plakous_tools::LineOwner::new(
            "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
            1,
            "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
            fence,
        );
        (client, RunnerSessions::new(&backend, &environment, owner))
    }

    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "subprocess daemon fixture"]
fn retention_fixture_daemon() {
    let root = PathBuf::from(std::env::var_os("PLAK_RETENTION_FIXTURE_ROOT").unwrap());
    let revision = std::env::var("PLAK_RETENTION_FIXTURE_REVISION").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (service, directory) = Worker::service(&root, &revision);
        crate::daemon::serve_socket_with_retention(
            service,
            &directory.join("native.sock"),
            &directory,
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    });
}

#[test]
#[ignore = "subprocess job fixture"]
fn retention_fixture_job() {
    let path = std::env::var_os("PLAK_RETENTION_FIXTURE_CONTROL").unwrap();
    let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    stream.write_all(b"r").unwrap();
    let mut byte = [0];
    while stream.read_exact(&mut byte).is_ok() {
        if byte == *b"q" {
            return;
        }
        stream.write_all(&byte).unwrap();
    }
}

async fn controlled_job(
    root: &Path,
    port: &RunnerSessions,
) -> (JoinedProcessSessionId, UnixStream) {
    let path = root.join("job.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal(
            "/usr/bin/env",
            vec![
                format!("PLAK_RETENTION_FIXTURE_CONTROL={}", path.display()),
                std::env::current_exe().unwrap().to_str().unwrap().into(),
                "--exact".into(),
                "service::delegation::tests::loss::retention_fixture_job".into(),
                "--ignored".into(),
            ],
        )
        .unwrap(),
        ".",
        Vec::new(),
        true,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap();
    let process = port.start(request, control(20)).await.unwrap();
    port.promote(process).await.unwrap();
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), b'r');
    (process, stream)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_loss_original_daemon_restart_reports_unknown_without_adopting_or_killing_job() {
    let root = tempfile::tempdir().unwrap();
    let mut original = Daemon::spawn(root.path(), "original");
    let (old_client, old) = original.port(root.path(), "old-host", 1).await;
    let (process, mut job) = controlled_job(root.path(), &old).await;
    let number = JoinedProcessSessions::job_number(process);
    let successor = Worker::new(root.path(), "successor").await;
    let (link, port) = successor.port(root.path(), "new-host", 2).await;
    assert_eq!(port.jobs().await.unwrap()[0].status, JobStatus::Running);
    original.kill();
    let mut replacement = Daemon::spawn(root.path(), "original");
    let (replacement_client, replacement_port) =
        replacement.port(root.path(), "replacement-host", 3).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if port.jobs().await.unwrap()[0].status == JobStatus::OutcomeUnknown {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(
        replacement_port
            .start(command(), control(21))
            .await
            .is_err()
    );
    job.write_all(b"p").await.unwrap();
    assert_eq!(job.read_u8().await.unwrap(), b'p');
    // Loss is an unknown terminal fact, not a still-supervised allocation.
    // It stays visible until consumed; only an explicit new call starts work.
    let next = port.start(command(), control(22)).await.unwrap();
    port.promote(next).await.unwrap();
    assert!(JoinedProcessSessions::job_number(next) > number);
    port.acknowledge_jobs(vec![number]).await.unwrap();
    assert_eq!(port.jobs().await.unwrap().len(), 1);
    port.write_stdin(next, b"", JoinedProcessInputDisposition::Close, control(24))
        .await
        .unwrap();
    assert_eq!(finish(&port, next).await, Vec::<u8>::new());
    job.write_all(b"q").await.unwrap();
    old_client.disconnect();
    replacement_client.disconnect();
    link.sever();
}

async fn receiver_cut(kind: CutKind) {
    let root = tempfile::tempdir().unwrap();
    let cut = ReplyCut::new(kind);
    let first = Worker::with_cut(root.path(), "first", Some(cut.clone())).await;
    let (old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(30)).await.unwrap();
    old.promote(process).await.unwrap();
    let mut receiving = Daemon::spawn(root.path(), "receiving");
    let (client, port) = receiving.port(root.path(), "new-host", 2).await;
    let port = Arc::new(port);
    if matches!(kind, CutKind::Write) {
        assert_eq!(port.jobs().await.unwrap().len(), 1);
    }
    let pending_port = port.clone();
    let pending = tokio::spawn(async move {
        match kind {
            CutKind::Transfer => {
                let _ = pending_port.jobs().await;
            }
            CutKind::Write => {
                let _ = pending_port
                    .write_stdin(
                        process,
                        b"once",
                        JoinedProcessInputDisposition::KeepOpen,
                        control(31),
                    )
                    .await;
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(10), cut.reached.notified())
        .await
        .unwrap();
    receiving.kill();
    pending.abort();
    cut.release.notify_one();
    let mut restarted = Daemon::spawn(root.path(), "receiving");
    let (next_client, next) = restarted.port(root.path(), "new-host", 2).await;
    let jobs = next.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::Running);
    if matches!(kind, CutKind::Write) {
        next.write_stdin(
            process,
            b"once",
            JoinedProcessInputDisposition::KeepOpen,
            control(31),
        )
        .await
        .unwrap();
        next.write_stdin(
            process,
            b"",
            JoinedProcessInputDisposition::Close,
            control(32),
        )
        .await
        .unwrap();
    } else {
        next.write_stdin(
            process,
            b"once",
            JoinedProcessInputDisposition::Close,
            control(32),
        )
        .await
        .unwrap();
    }
    assert_eq!(finish(&next, process).await, b"once");
    next.acknowledge_jobs(vec![JoinedProcessSessions::job_number(process)])
        .await
        .unwrap();
    assert_eq!(next.jobs().await.unwrap(), Vec::new());
    client.disconnect();
    next_client.disconnect();
    old_link.sever();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_loss_receiving_daemon_mid_input_recovers_original_receipt_without_replay() {
    receiver_cut(CutKind::Write).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_loss_transfer_reply_recovers_only_the_persisted_own_tuple() {
    receiver_cut(CutKind::Transfer).await;
}
