//! Re-adoption over a loopback pair of processes: the runner daemon in its
//! own process, and a real session store on the host side. A runner job of
//! the session's line outlives the host link that started it; the returning
//! host holds the line's head lease-only, re-adopts the line on the runner
//! under that lease's fence, and leaves the head free for a full open.

#![allow(clippy::unwrap_used, reason = "process fixture")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{
    ClientWindows, FileOwnerFloors, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService,
    daemon,
};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionOperationId,
};
use plakous::tools::runner::{RunnerBackend, RunnerSessions, runner_environment};
use plakous::tools::{
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, JobStatus, JoinedProcessCommand, JoinedProcessStartRequest,
    JoinedProcessTerminalMode, LineOwner, OperationClass, OperationContext, RunnerId,
};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

const CHILD: &str = "PLAKOUS_HOSTED_READOPT_DAEMON";

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000061".parse().unwrap())
}

fn version() -> VersionTriple {
    VersionTriple::current("readoption", "debug")
}

fn socket(directory: &Path) -> PathBuf {
    daemon::socket_path(&directory.join("run"), &version())
}

/// The daemon process: the test binary again, running only this test.
#[test]
fn readoption_daemon_process() {
    let Ok(directory) = std::env::var(CHILD) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let service = RunnerService::with_options(
            ExecutionEnvironment::host(
                scope(),
                directory.join("workspace"),
                ".",
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            RunnerIdentity {
                runner_id: RunnerId::new("readoption").unwrap(),
                version: version(),
                boot_id: "boot-1".into(),
                binary_id: None,
            },
            RunnerOptions {
                floors: Some(Arc::new(
                    FileOwnerFloors::open(directory.join("owners.json")).unwrap(),
                )),
                ..RunnerOptions::default()
            },
        )
        .unwrap();
        daemon::serve_socket(service, &socket(&directory), Duration::from_secs(600))
            .await
            .unwrap();
    });
}

async fn bind(directory: &Path, instance: &str) -> (RunnerClient, Arc<RunnerBackend>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let stream = loop {
        match tokio::net::UnixStream::connect(socket(directory)).await {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the daemon never listened: {error}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let (client, hello) = RunnerClient::connect(
        stream,
        Bind {
            host_id: "host-1".into(),
            host_instance: instance.into(),
            version: version(),
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    (client, backend)
}

/// An earlier host process of `host-1`: it starts a job of `owner`'s line
/// on the runner and loses its link.
async fn start_job_and_lose_the_link(directory: &Path, workspace: &Path, owner: LineOwner) {
    let (client, backend) = bind(directory, "instance-1").await;
    let environment = runner_environment(
        scope(),
        workspace,
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let jobs = RunnerSessions::new(&backend, &environment, owner);
    let id = jobs
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/sh", ["-c".to_owned(), "sleep 30".to_owned()])
                    .unwrap(),
                ".",
                Vec::new(),
                false,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            OperationContext::new(plakous::model::CancellationToken::new())
                .with_owner(owner)
                .with_class(OperationClass::EffectCall {
                    call_id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    attempt_id: "00000000-0000-0000-0000-0000000000c1".parse().unwrap(),
                }),
        )
        .await
        .unwrap();
    jobs.promote(id).await.unwrap();
    drop(jobs);
    drop(client);
}

/// Another store of the same home (another state key's host) sweeps the
/// same runner: the line is not its own, so it is skipped, neither
/// transferred nor reported, and stays held by its owner at `fence`.
async fn another_store_skips_the_line(directory: &Path, client: &RunnerClient, fence: u64) {
    let other = AgentSessions::new(
        SessionBackend::sqlite(directory.join("other-runtime.sqlite3")).unwrap(),
    )
    .unwrap();
    let (sibling, _) = bind(directory, "instance-3").await;
    let (readopted, failed) =
        crate::runner_adoption::readopt_lines(&sibling, &other, "plak:readopt")
            .await
            .unwrap();
    assert_eq!((readopted, failed), (Vec::new(), Vec::new()));
    let lines = client.lines().await.unwrap();
    assert!(lines[0].adopted);
    assert_eq!(
        lines[0].owner.as_ref().map(|owner| owner.fence),
        Some(fence)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_returning_host_readopts_its_line_under_a_lease_only_head() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut runner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runner_adoption::tests::readoption_daemon_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, directory.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // The session whose line the runner job belongs to.
    let sessions = AgentSessions::new(
        SessionBackend::sqlite(directory.path().join("runtime.sqlite3")).unwrap(),
    )
    .unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        directory.path().join("evidence.jsonl"),
    )
    .unwrap();
    let binding = || runtime_binding(&workspace, controller.clone()).unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("readopt:create").unwrap(),
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("a new session");
    };
    let line = session.line();
    let branch = session.locator().branch_id();
    let locator = session.release().await.unwrap();

    // An earlier host process started a job of the line, then lost its link.
    let earlier = LineOwner::new(line, 1, branch, 0);
    start_job_and_lose_the_link(directory.path(), &workspace, earlier).await;

    // The same host identity returns.
    let (client, backend) = bind(directory.path(), "instance-2").await;
    let (readopted, failed) =
        crate::runner_adoption::readopt_lines(&client, &sessions, "plak:readopt")
            .await
            .unwrap();
    assert_eq!(failed, Vec::new());
    assert_eq!(readopted.len(), 1);
    let adopted = &readopted[0];
    assert_eq!(adopted.line, line);
    assert_eq!(adopted.running, 1);
    assert!(
        adopted.owner.fence() > earlier.fence(),
        "a fresh lease fence"
    );
    assert_eq!(adopted.owner.generation(), 1);
    let lines = client.lines().await.unwrap();
    assert!(lines[0].adopted);
    assert_eq!(
        lines[0].owner.as_ref().map(|owner| owner.fence),
        Some(adopted.owner.fence())
    );

    // The lost host's owner is fenced off the line on the runner.
    let environment = runner_environment(
        scope(),
        &workspace,
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let stale = RunnerSessions::new(&backend, &environment, earlier);
    assert_eq!(
        stale.jobs().await.unwrap_err().kind(),
        ExecutionBackendErrorKind::IdentityMismatch
    );
    // The adopted owner sees the job running; the lease-only hold is gone,
    // so the head opens fully.
    let adopted_jobs = RunnerSessions::new(&backend, &environment, adopted.owner);
    let jobs = adopted_jobs.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::Running);
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("readopt:open").unwrap(),
            locator,
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("the head opens");
    };
    session.release().await.unwrap();

    another_store_skips_the_line(directory.path(), &client, adopted.owner.fence()).await;
    adopted_jobs
        .stop_jobs(
            plakous::tools::JobStopReason::User,
            Duration::from_millis(200),
        )
        .await
        .unwrap();
    runner.kill().unwrap();
    runner.wait().unwrap();
}
