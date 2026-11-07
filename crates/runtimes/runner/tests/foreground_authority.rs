//! Foreground calls keep a reconnecting instance, never a lost authority.

#![allow(clippy::unwrap_used, reason = "isolated process fixture")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_runner::{ClientWindows, RunnerClient, RunnerIdentity, RunnerLimits, RunnerService};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{RunnerBackend, RunnerSessions, runner_environment};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    JoinedProcessCommand, JoinedProcessOutputCursor, JoinedProcessStartRequest,
    JoinedProcessTerminalMode, LineOwner, OperationClass, OperationContext, RunnerId,
};
use tokio::task::AbortHandle;

fn version() -> VersionTriple {
    VersionTriple::current("foreground-authority", "test")
}
fn owner(fence: u64) -> LineOwner {
    LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        fence,
    )
}
fn control() -> OperationContext {
    OperationContext::new(plakous_model::CancellationToken::new())
}
fn environment(root: &Path) -> ExecutionEnvironment {
    ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    service: RunnerService,
}
impl Fixture {
    fn new(lease: Duration) -> Self {
        let root = tempfile::tempdir().unwrap();
        let service = RunnerService::with_limits(
            environment(root.path()),
            RunnerIdentity {
                runner_id: RunnerId::new("foreground").unwrap(),
                version: version(),
                boot_id: "boot".into(),
                binary_id: None,
            },
            RunnerLimits {
                owner_lease: lease,
                ..RunnerLimits::default()
            },
        )
        .unwrap();
        Self { root, service }
    }
    fn marker(&self) -> PathBuf {
        self.root.path().join("foreground.marker")
    }
    fn pid(&self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(self.marker())
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "the process did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    async fn start(
        &self,
        link: &Link,
    ) -> (Arc<RunnerSessions>, plakous_tools::JoinedProcessSessionId) {
        let env = runner_environment(
            environment(self.root.path()).descriptor().scope(),
            self.root.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
            link.backend.clone(),
        )
        .unwrap();
        let sessions = Arc::new(RunnerSessions::new(&link.backend, &env, owner(1)));
        let script = format!(
            "echo $$ > '{}'; n=0; while [ ! -f '{}' ]; do [ \"$n\" -lt 3000 ] || exit 1; n=$((n + 1)); sleep 0.01; done",
            self.marker().display(),
            self.root.path().join("release").display()
        );
        let id = sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal("/bin/sh", ["-c".into(), script]).unwrap(),
                    ".",
                    Vec::new(),
                    false,
                    JoinedProcessTerminalMode::Pipes,
                )
                .unwrap(),
                control().with_class(OperationClass::EffectCall {
                    call_id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    attempt_id: "00000000-0000-0000-0000-000000000002".parse().unwrap(),
                }),
            )
            .await
            .unwrap();
        self.pid();
        (sessions, id)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(text) = std::fs::read_to_string(self.marker())
            && let Ok(pid) = text.trim().parse::<i32>()
            && let Some(pid) = rustix::process::Pid::from_raw(pid)
        {
            let command = std::process::Command::new("ps")
                .args(["-o", "command=", "-p", &pid.as_raw_pid().to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&command.stdout)
                .contains(&self.marker().display().to_string())
            {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

struct Link {
    client: RunnerClient,
    backend: Arc<RunnerBackend>,
    relay: AbortHandle,
}
impl Link {
    async fn new(service: &RunnerService, instance: &str) -> Self {
        let (stream, relay) = transport(service);
        let (client, hello) = RunnerClient::connect(
            stream,
            Bind {
                host_id: "home".into(),
                host_instance: instance.into(),
                version: version(),
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let backend =
            Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
        Self {
            client,
            backend,
            relay,
        }
    }
    async fn reconnect(&mut self, service: &RunnerService) {
        self.relay.abort();
        let (stream, relay) = transport(service);
        self.relay = relay;
        self.client.reconnect(stream).await.unwrap();
    }
}
impl Drop for Link {
    fn drop(&mut self) {
        self.relay.abort();
    }
}
fn transport(service: &RunnerService) -> (tokio::io::DuplexStream, AbortHandle) {
    let (host, mut from_host) = tokio::io::duplex(65536);
    let (runner, mut from_runner) = tokio::io::duplex(65536);
    let service = service.clone();
    tokio::spawn(async move { service.serve_connection(runner).await });
    let relay = tokio::spawn(async move {
        let _ = tokio::io::copy_bidirectional(&mut from_host, &mut from_runner).await;
    });
    (host, relay.abort_handle())
}
fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_owner_stops_and_reports_foreground_but_preserves_promoted_jobs() {
    for promoted in [false, true] {
        let fixture = Fixture::new(Duration::from_secs(10));
        let before = Link::new(&fixture.service, "before").await;
        let (sessions, id) = fixture.start(&before).await;
        if promoted {
            sessions.promote(id).await.unwrap();
        }
        let after = Link::new(&fixture.service, "after").await;
        let stopped = after.client.transfer_owner(&owner(2)).await.unwrap();
        assert_eq!(stopped.len(), usize::from(!promoted));
        assert_eq!(alive(fixture.pid()), promoted);
        if promoted {
            let env = runner_environment(
                environment(fixture.root.path()).descriptor().scope(),
                fixture.root.path(),
                ".",
                ExecutionEnvironmentConfig::default(),
                after.backend.clone(),
            )
            .unwrap();
            RunnerSessions::new(&after.backend, &env, owner(2))
                .stop_jobs(
                    plakous_tools::JobStopReason::User,
                    Duration::from_millis(200),
                )
                .await
                .unwrap();
        } else {
            assert_eq!(stopped[0].pid, fixture.pid().cast_unsigned());
            assert_eq!(
                stopped[0].call_key,
                "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002"
            );
            let third = Link::new(&fixture.service, "third").await;
            let repeated = third.client.transfer_owner(&owner(3)).await.unwrap();
            assert!(
                repeated.is_empty(),
                "a newer accepted owner retires delivered reports"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_owner_lease_expiry_stops_without_a_successor() {
    let fixture = Fixture::new(Duration::from_millis(80));
    let link = Link::new(&fixture.service, "before").await;
    let _ = fixture.start(&link).await;
    link.relay.abort();
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(fixture.pid()) {
        let _ = fixture.service.is_idle();
        assert!(
            Instant::now() < deadline,
            "foreground survived its authority's lease"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The next bind itself must notice a lapsed lease even without idle ticks.
    let fixture = Fixture::new(Duration::from_millis(80));
    let mut link = Link::new(&fixture.service, "late-reconnect").await;
    let _ = fixture.start(&link).await;
    link.relay.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    link.reconnect(&fixture.service).await;
    let stopped = link.client.transfer_owner(&owner(1)).await.unwrap();
    assert_eq!(stopped.len(), 1);
    assert!(!alive(fixture.pid()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_implicit_transfer_delivers_each_foreground_report_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = Fixture::new(Duration::from_secs(10));
    let before = Link::new(&fixture.service, "before").await;
    fixture.start(&before).await;
    let mut after = Link::new(&fixture.service, "after").await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    after.client.report_foreground_with(move |line, stopped| {
        assert_eq!(line, owner(2).line().to_string());
        assert_ne!(stopped.call_key, "");
        assert_ne!(stopped.pid, 0);
        seen.fetch_add(1, Ordering::Relaxed);
    });
    let env = runner_environment(
        environment(fixture.root.path()).descriptor().scope(),
        fixture.root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        after.backend.clone(),
    )
    .unwrap();
    RunnerSessions::new(&after.backend, &env, owner(2))
        .jobs()
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(after.client.transfer_owner(&owner(2)).await.unwrap(), []);
    after.reconnect(&fixture.service).await;
    assert_eq!(after.client.transfer_owner(&owner(2)).await.unwrap(), []);
    let third = Link::new(&fixture.service, "third").await;
    assert_eq!(third.client.transfer_owner(&owner(3)).await.unwrap(), []);
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert!(!alive(fixture.pid()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_blip_within_the_lease_preserves_the_foreground_call() {
    let fixture = Fixture::new(Duration::from_secs(10));
    let mut link = Link::new(&fixture.service, "same-instance").await;
    let (sessions, id) = fixture.start(&link).await;
    let poll = sessions.poll_from(
        id,
        JoinedProcessOutputCursor::new(0),
        8192,
        64,
        Duration::from_secs(5),
        control(),
    );
    tokio::pin!(poll);
    std::future::poll_fn(|cx| {
        assert!(poll.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    link.reconnect(&fixture.service).await;
    assert!(alive(fixture.pid()));
    std::fs::write(fixture.root.path().join("release"), b"done").unwrap();
    assert_eq!(
        poll.await.unwrap().status(),
        plakous_tools::JoinedProcessStatus::Exited(Some(0))
    );
}
