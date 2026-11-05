//! Link-fault shapes over a real pair of processes (spec 05 §2): the runner
//! daemon in one process on a private socket, a host in another, and each
//! killed in turn. They run in the durability lane: each waits on real
//! processes across a killed peer.
//!
//! - `runner_killed_mid_job_restarts_with_its_owner_floor`: SIGKILL the
//!   daemon with a job running. The restarted daemon is a new instance: the
//!   job's outcome is unknown, and the owner floor it recorded keeps every
//!   older owner of the line off it.
//! - `host_killed_mid_call_is_recovered_and_readopted_by_its_host_id`:
//!   SIGKILL a host with a job running and an effect call in flight. Its
//!   owner lease runs out and the line is orphaned; the same host identity
//!   returns as a new process, transfers the line to a newer owner, replays
//!   the journaled call (which ran once) and reads the job to its end.
//! - `host_crash_with_a_sibling_connected_orphans_only_its_lines`: another
//!   process of the same home stays bound while a host is killed holding a
//!   line. The sibling cannot stop the line's job with the owner it can read,
//!   the line is orphaned at the dead holder's lease though the sibling is
//!   connected, and a successor takes it with a newer owner.
//! - `runner_restart_sibling_races_floor_owner`: after a runner restart a
//!   sibling binds first and presents the floor owner; it is refused, and the
//!   floor's holder takes its line back.
//! - `crash_after_takeover_before_transfer`: a successor's newer owner was
//!   durable in its store but never reached the runner before it crashed.
//!   Nothing on the runner is ahead of the store, so the next successor's
//!   newer owner takes the line; nothing wedges.

#![allow(clippy::unwrap_used, reason = "integration tests")]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{
    ClientWindows, FileCallJournal, FileOwnerFloors, RunnerClient, RunnerIdentity, RunnerLimits,
    RunnerOptions, RunnerService, daemon,
};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{RunnerBackend, RunnerSessions, runner_environment};
use plakous_tools::{
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, JobStatus, JoinedProcessCommand, JoinedProcessStartRequest,
    JoinedProcessTerminalMode, LineOwner, LiteralProcessIntent, LiteralProcessRequest,
    OperationClass, OperationContext, RunnerId,
};

const CHILD: &str = "PLAKOUS_RUNNER_LINK_FAULT_CHILD";
const DIRECTORY: &str = "PLAKOUS_RUNNER_LINK_FAULT_DIRECTORY";
const LEASE_MS: &str = "PLAKOUS_RUNNER_LINK_FAULT_LEASE_MS";
const WAIT: Duration = Duration::from_secs(20);

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000051").unwrap())
}

fn version() -> VersionTriple {
    VersionTriple::current("link-faults", "debug")
}

fn workspace(directory: &Path) -> PathBuf {
    directory.join("workspace")
}

fn socket(directory: &Path) -> PathBuf {
    daemon::socket_path(&directory.join("run"), &version())
}

fn owner(generation: u64, fence: u64) -> LineOwner {
    LineOwner::new(
        LineId::from_str("00000000-0000-0000-0000-0000000000a1").unwrap(),
        generation,
        BranchId::from_str("00000000-0000-0000-0000-0000000000b1").unwrap(),
        fence,
    )
}

fn effect(owner: LineOwner, call: u8) -> OperationContext {
    OperationContext::new(CancellationToken::new())
        .with_owner(owner)
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str(&format!("00000000-0000-0000-0000-0000000000{call:02x}"))
                .unwrap(),
            attempt_id: AttemptId::from_str("00000000-0000-0000-0000-0000000000c1").unwrap(),
        })
}

fn job(script: &str) -> JoinedProcessStartRequest {
    JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/sh", ["-c".to_owned(), script.to_owned()]).unwrap(),
        ".",
        Vec::new(),
        false,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap()
}

/// One role of a shape, run in its own process: the test binary again,
/// selecting only `link_fault_child`.
fn spawn(role: &str, directory: &Path, lease: Duration) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "link_fault_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, role)
        .env(DIRECTORY, directory)
        .env(LEASE_MS, lease.as_millis().to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

fn kill(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

/// The child process: a daemon on the directory's socket, or a host that
/// starts a job and a slow effect call, says so, and waits to be killed.
#[test]
fn link_fault_child() {
    let Ok(role) = std::env::var(CHILD) else {
        return;
    };
    let directory = PathBuf::from(std::env::var(DIRECTORY).unwrap());
    let lease = Duration::from_millis(std::env::var(LEASE_MS).unwrap().parse().unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    match role.as_str() {
        "daemon" => runtime.block_on(run_daemon(&directory, lease)),
        "host" => runtime.block_on(run_host(&directory)),
        other => panic!("unknown role {other}"),
    }
}

async fn run_daemon(directory: &Path, lease: Duration) {
    let root = workspace(directory);
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(scope(), &root, ".", ExecutionEnvironmentConfig::default())
            .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("link-faults").unwrap(),
            version: version(),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        RunnerOptions {
            limits: RunnerLimits {
                owner_lease: lease,
                ..RunnerLimits::default()
            },
            floors: Some(Arc::new(
                FileOwnerFloors::open(directory.join("owners.json")).unwrap(),
            )),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    daemon::serve_socket(service, &socket(directory), Duration::from_secs(600))
        .await
        .unwrap();
}

async fn run_host(directory: &Path) {
    let (client, environment, backend) = connect(directory, "instance-1", true).await;
    let sessions = RunnerSessions::new(&backend, &environment, owner(1, 1));
    let id = sessions
        .start(job("sleep 2; echo done"), effect(owner(1, 1), 1))
        .await
        .unwrap();
    sessions.promote(id).await.unwrap();
    // A slow effect call the host dies inside: it runs on, once.
    let slow = tokio::spawn({
        let environment = environment.clone();
        async move {
            let _ = environment
                .execute_literal_process(
                    LiteralProcessRequest::new(
                        "/bin/sh",
                        [
                            "-c".to_owned(),
                            "sleep 1; echo ran >> effect.log".to_owned(),
                        ],
                        ".",
                        Vec::new(),
                        LiteralProcessIntent::default(),
                    )
                    .unwrap(),
                    effect(owner(1, 1), 2),
                )
                .await;
        }
    });
    // Ready once the call is journaled and on the wire.
    let journal = directory.join("journal");
    while std::fs::read_dir(&journal).unwrap().count() == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::write(directory.join("host-ready"), b"").unwrap();
    slow.await.unwrap();
    drop(client);
    std::future::pending::<()>().await;
}

/// A host bound to the directory's daemon as `instance` of `host-1`.
async fn connect(
    directory: &Path,
    instance: &str,
    journaled: bool,
) -> (RunnerClient, ExecutionEnvironment, Arc<RunnerBackend>) {
    let stream = connect_socket(directory).await;
    let journal: Option<Arc<dyn plakous_runner::CallJournal>> = journaled.then(|| {
        Arc::new(FileCallJournal::open(directory.join("journal")).unwrap())
            as Arc<dyn plakous_runner::CallJournal>
    });
    let (client, hello) = RunnerClient::connect_journaled(
        stream,
        Bind {
            host_id: "host-1".into(),
            host_instance: instance.into(),
            version: version(),
            inspect: false,
        },
        windows(),
        journal,
    )
    .await
    .unwrap();
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    let environment = runner_environment(
        scope(),
        workspace(directory),
        ".",
        ExecutionEnvironmentConfig::default(),
        Arc::clone(&backend),
    )
    .unwrap();
    (client, environment, backend)
}

async fn connect_socket(directory: &Path) -> tokio::net::UnixStream {
    let deadline = Instant::now() + WAIT;
    loop {
        match tokio::net::UnixStream::connect(socket(directory)).await {
            Ok(stream) => return stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the daemon never listened: {error}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

fn windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_secs(10),
        effect: Duration::from_secs(10),
        ..ClientWindows::default()
    }
}

async fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The line's one job once it ended, read through `sessions`.
async fn ended(sessions: &RunnerSessions) -> plakous_tools::JobView {
    let deadline = Instant::now() + WAIT;
    loop {
        let jobs = sessions.jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        if jobs[0].status != JobStatus::Running {
            return jobs[0].clone();
        }
        assert!(Instant::now() < deadline, "the job did not end");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_killed_mid_job_restarts_with_its_owner_floor() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let first = spawn("daemon", directory.path(), Duration::from_secs(300));
    let (client, environment, backend) = connect(directory.path(), "instance-1", true).await;
    let before = client.instance();
    let sessions = RunnerSessions::new(&backend, &environment, owner(2, 1));
    let id = sessions
        .start(job("sleep 3"), effect(owner(2, 1), 1))
        .await
        .unwrap();
    sessions.promote(id).await.unwrap();
    assert_eq!(sessions.jobs().await.unwrap().len(), 1);

    kill(first);
    let second = spawn("daemon", directory.path(), Duration::from_secs(300));
    let hello = client
        .reconnect(connect_socket(directory.path()).await)
        .await
        .unwrap();
    assert_ne!(
        hello.instance, before,
        "a restarted runner is a new instance"
    );
    let jobs = sessions.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::OutcomeUnknown);
    // The floor the first instance recorded holds on the second: an older
    // owner of the line is refused there.
    let stale = RunnerSessions::new(&backend, &environment, owner(1, 9));
    let refused = stale.jobs().await.unwrap_err();
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::IdentityMismatch);
    kill(second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_killed_mid_call_is_recovered_and_readopted_by_its_host_id() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let runner = spawn("daemon", directory.path(), Duration::from_millis(300));
    let host = spawn("host", directory.path(), Duration::ZERO);
    wait_for(&directory.path().join("host-ready")).await;
    kill(host);

    // Past the owner lease, with no connection, the line is orphaned. An
    // inspector sees its own host's lines only, with jobs redacted.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (stranger, _) = RunnerClient::connect(
        connect_socket(directory.path()).await,
        Bind {
            host_id: "anyone".into(),
            host_instance: "inspect".into(),
            version: version(),
            inspect: true,
        },
        windows(),
    )
    .await
    .unwrap();
    assert_eq!(stranger.lines().await.unwrap(), Vec::new());
    let (inspector, _) = RunnerClient::connect(
        connect_socket(directory.path()).await,
        Bind {
            host_id: "host-1".into(),
            host_instance: "inspect".into(),
            version: version(),
            inspect: true,
        },
        windows(),
    )
    .await
    .unwrap();
    let lines = inspector.lines().await.unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].host_id, "host-1");
    assert!(
        !lines[0].adopted,
        "the expired lease leaves the line orphaned"
    );
    assert!(
        lines[0].jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|job| job["command"] == "" && job["output"] == ""),
        "{lines:?}"
    );

    // The same host identity returns as a new process. A newer owner of the
    // line re-adopts it first: only the line's holder may replay its keys.
    let (client, environment, backend) = connect(directory.path(), "instance-2", true).await;
    let sessions = RunnerSessions::new(&backend, &environment, owner(1, 2));
    let jobs = sessions.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    let lines = client.lines().await.unwrap();
    assert!(lines[0].adopted, "the transfer re-adopted the line");
    // It replays the call its predecessor died inside, which ran exactly
    // once.
    let recovered = client.recover().await;
    assert_eq!(recovered.len(), 1, "{recovered:?}");
    assert!(recovered[0].answer.is_ok(), "{recovered:?}");
    assert!(client.recover().await.is_empty(), "the journal is settled");
    let log = std::fs::read_to_string(workspace(directory.path()).join("effect.log")).unwrap();
    assert_eq!(log, "ran\n");
    let job = ended(&sessions).await;
    assert_eq!(job.status, JobStatus::Exited(Some(0)));
    assert_eq!(job.output, "done");
    // The killed host's owner is fenced off the line.
    let stale = RunnerSessions::new(&backend, &environment, owner(1, 1));
    assert_eq!(
        stale.jobs().await.unwrap_err().kind(),
        ExecutionBackendErrorKind::IdentityMismatch
    );
    kill(runner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_crash_with_a_sibling_connected_orphans_only_its_lines() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let runner = spawn("daemon", directory.path(), Duration::from_millis(300));
    let host = spawn("host", directory.path(), Duration::ZERO);
    wait_for(&directory.path().join("host-ready")).await;
    // A sibling process of the same home, bound throughout.
    let (sibling, environment, backend) = connect(directory.path(), "instance-2", false).await;
    kill(host);

    // The owner is readable from the shared store, but it authorizes nothing:
    // the sibling is not the line's holder.
    let read_owner = RunnerSessions::new(&backend, &environment, owner(1, 1));
    assert_eq!(
        read_owner
            .stop_jobs(plakous_tools::JobStopReason::User, Duration::ZERO)
            .await
            .unwrap_err()
            .kind(),
        ExecutionBackendErrorKind::IdentityMismatch
    );

    // Past the dead holder's lease the line is orphaned, though the sibling
    // stayed connected.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let lines = sibling.lines().await.unwrap();
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].adopted, "the dead holder's line is orphaned");

    // A successor, holding the line's lease with a newer owner, takes it.
    let (_successor, environment, backend) = connect(directory.path(), "instance-3", false).await;
    let sessions = RunnerSessions::new(&backend, &environment, owner(1, 2));
    let job = ended(&sessions).await;
    assert_eq!(job.status, JobStatus::Exited(Some(0)));
    kill(runner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_restart_sibling_races_floor_owner() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let first = spawn("daemon", directory.path(), Duration::from_secs(300));
    let (holder, environment, backend) = connect(directory.path(), "instance-1", false).await;
    let sessions = RunnerSessions::new(&backend, &environment, owner(1, 1));
    let id = sessions
        .start(job("sleep 3"), effect(owner(1, 1), 1))
        .await
        .unwrap();
    sessions.promote(id).await.unwrap();
    assert_eq!(sessions.jobs().await.unwrap().len(), 1);

    kill(first);
    let second = spawn("daemon", directory.path(), Duration::from_secs(300));
    // The sibling binds to the restarted runner first, with the floor owner.
    let (_sibling, sibling_environment, sibling_backend) =
        connect(directory.path(), "instance-2", false).await;
    let racing = RunnerSessions::new(&sibling_backend, &sibling_environment, owner(1, 1));
    assert_eq!(
        racing.jobs().await.unwrap_err().kind(),
        ExecutionBackendErrorKind::IdentityMismatch
    );
    // The floor's holder returns and takes its line back.
    holder
        .reconnect(connect_socket(directory.path()).await)
        .await
        .unwrap();
    let jobs = sessions.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::OutcomeUnknown);
    kill(second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_takeover_before_transfer() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let runner = spawn("daemon", directory.path(), Duration::from_secs(300));
    let host = spawn("host", directory.path(), Duration::ZERO);
    wait_for(&directory.path().join("host-ready")).await;
    kill(host);
    // A successor took the line's lease in its store, minting owner (1, 2),
    // and crashed before its transfer: the runner never saw that owner. The
    // next successor's lease mints (1, 3), newer than anything the runner
    // accepted, so it takes the line from the dead holder's floor.
    let (_successor, environment, backend) = connect(directory.path(), "instance-3", false).await;
    let sessions = RunnerSessions::new(&backend, &environment, owner(1, 3));
    let job = ended(&sessions).await;
    assert_eq!(job.status, JobStatus::Exited(Some(0)));
    // The owner the crashed successor minted is older now, and fenced off.
    let crashed = RunnerSessions::new(&backend, &environment, owner(1, 2));
    assert_eq!(
        crashed.jobs().await.unwrap_err().kind(),
        ExecutionBackendErrorKind::IdentityMismatch
    );
    kill(runner);
}
