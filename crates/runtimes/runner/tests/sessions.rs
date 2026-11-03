//! Process sessions and services through the loopback runner, compared with
//! the direct local backend; yields, the job bound and owner controls.

#![allow(clippy::unwrap_used, reason = "integration tests")]

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{ClientWindows, Loopback, RunnerIdentity, RunnerLimits, RunnerService};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{RunnerSessions, runner_environment};
use plakous_tools::{
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentLimits, ExecutionEnvironmentScope, JobStatus, JobStopReason,
    JoinedProcessCommand, JoinedProcessInputDisposition, JoinedProcessOutputCursor,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessShellProfile,
    JoinedProcessStartRequest, JoinedProcessStatus, JoinedProcessTerminalMode, LineOwner,
    OperationClass, OperationContext, RunnerId, ServiceProcessRequest, ToolPreempt,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LINE: &str = "00000000-0000-0000-0000-0000000000a1";
const BRANCH: &str = "00000000-0000-0000-0000-0000000000b1";

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap())
}

fn config() -> ExecutionEnvironmentConfig {
    ExecutionEnvironmentConfig::new(ExecutionEnvironmentLimits::default(), [], []).unwrap()
}

fn host(root: &Path) -> ExecutionEnvironment {
    ExecutionEnvironment::host(scope(), root, ".", config()).unwrap()
}

fn service(root: &Path, limits: RunnerLimits) -> RunnerService {
    RunnerService::with_limits(
        host(root),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: VersionTriple::current("conformance", "debug"),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        limits,
    )
    .unwrap()
}

fn bind() -> Bind {
    Bind {
        host_id: "host-1".into(),
        host_instance: "instance-1".into(),
        version: VersionTriple::current("conformance", "debug"),
    }
}

fn windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_secs(10),
        effect: Duration::from_secs(10),
        ..ClientWindows::default()
    }
}

fn owner(generation: u64, fence: u64) -> LineOwner {
    LineOwner::new(
        LineId::from_str(LINE).unwrap(),
        generation,
        BranchId::from_str(BRANCH).unwrap(),
        fence,
    )
}

fn effect(call: u8) -> OperationContext {
    OperationContext::new(CancellationToken::new())
        .with_owner(owner(1, 1))
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str(&format!("00000000-0000-0000-0000-0000000000{call:02x}"))
                .unwrap(),
            attempt_id: AttemptId::from_str("00000000-0000-0000-0000-0000000000c1").unwrap(),
        })
}

fn literal(script: &str, keep_stdin_open: bool) -> JoinedProcessStartRequest {
    JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/sh", ["-c".to_owned(), script.to_owned()]).unwrap(),
        ".",
        Vec::new(),
        keep_stdin_open,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap()
}

/// The direct sessions of a local environment, or a runner line's sessions
/// through the loopback, behind one async surface.
enum Sessions {
    Direct(JoinedProcessSessions),
    Runner(RunnerSessions),
}

impl Sessions {
    async fn start(
        &self,
        request: JoinedProcessStartRequest,
        control: OperationContext,
    ) -> Result<JoinedProcessSessionId, ExecutionBackendErrorKind> {
        match self {
            Self::Direct(sessions) => sessions.start(request, control),
            Self::Runner(sessions) => sessions.start(request, control).await,
        }
        .map_err(|error| error.kind())
    }

    async fn write(
        &self,
        id: JoinedProcessSessionId,
        bytes: &[u8],
        call: u8,
    ) -> Result<(), ExecutionBackendErrorKind> {
        let close = JoinedProcessInputDisposition::Close;
        match self {
            Self::Direct(sessions) => sessions.write_stdin(id, bytes, close),
            Self::Runner(sessions) => sessions.write_stdin(id, bytes, close, effect(call)).await,
        }
        .map_err(|error| error.kind())
    }

    /// Every byte the session writes, and how it ended.
    async fn drain(&self, id: JoinedProcessSessionId) -> (String, JoinedProcessStatus) {
        let mut output = Vec::new();
        let mut cursor = JoinedProcessOutputCursor::new(0);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let polled = match self {
                Self::Direct(sessions) => {
                    let polled = sessions.poll_from(id, cursor, 1 << 16, 64).unwrap();
                    if polled.records().is_empty() && !polled.status().is_terminal() {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    polled
                }
                Self::Runner(sessions) => sessions
                    .poll_from(
                        id,
                        cursor,
                        1 << 16,
                        64,
                        Duration::from_secs(1),
                        OperationContext::new(CancellationToken::new()),
                    )
                    .await
                    .unwrap(),
            };
            for record in polled.records() {
                output.extend_from_slice(record.bytes());
            }
            cursor = polled.next_cursor();
            if polled.status().is_terminal() && polled.records().is_empty() {
                return (
                    String::from_utf8_lossy(&output).into_owned(),
                    polled.status(),
                );
            }
            assert!(Instant::now() < deadline, "the session did not end");
        }
    }

    async fn promote(&self, id: JoinedProcessSessionId) {
        match self {
            Self::Direct(sessions) => plakous_tools::internal::promote(sessions, id).unwrap(),
            Self::Runner(sessions) => sessions.promote(id).await.unwrap(),
        }
    }

    /// Each job's number, command and status.
    async fn jobs(&self) -> Vec<(u32, String, JobStatus)> {
        let jobs = match self {
            Self::Direct(sessions) => sessions.jobs(),
            Self::Runner(sessions) => sessions.jobs().await.unwrap(),
        };
        jobs.into_iter()
            .map(|job| (job.number, job.command, job.status))
            .collect()
    }

    async fn stop_jobs(&self) {
        let by = JobStopReason::Model;
        let grace = Duration::from_millis(200);
        match self {
            Self::Direct(sessions) => sessions.stop_jobs(by, grace).unwrap(),
            Self::Runner(sessions) => sessions.stop_jobs(by, grace).await.unwrap(),
        }
    }

    async fn disown(&self) {
        match self {
            Self::Direct(sessions) => sessions.disown_leftovers().unwrap(),
            Self::Runner(sessions) => sessions.disown_leftovers().await.unwrap(),
        }
    }
}

/// One run of every session operation, observed as text.
async fn suite(sessions: &Sessions) -> Vec<String> {
    let mut seen = Vec::new();
    let echo = sessions
        .start(literal("cat; echo done", true), effect(1))
        .await
        .unwrap();
    sessions.write(echo, b"hello\n", 2).await.unwrap();
    let drained = sessions.drain(echo).await;
    assert_eq!(
        drained,
        ("hello\ndone\n".into(), JoinedProcessStatus::Exited(Some(0)))
    );
    seen.push(format!("{drained:?}"));
    seen.push(format!("{:?}", sessions.write(echo, b"late", 3).await));

    let shell = JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()]).unwrap();
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::shell(shell, "echo $((40 + 2)); exit 7").unwrap(),
        ".",
        Vec::new(),
        false,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap();
    let id = sessions.start(request, effect(4)).await.unwrap();
    let drained = sessions.drain(id).await;
    assert_eq!(
        drained,
        ("42\n".into(), JoinedProcessStatus::Exited(Some(7)))
    );
    seen.push(format!("{drained:?}"));

    let job = sessions
        .start(literal("sleep 30", false), effect(5))
        .await
        .unwrap();
    sessions.promote(job).await;
    let running = sessions.jobs().await;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].2, JobStatus::Running);
    seen.push(format!("{running:?}"));
    sessions.stop_jobs().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let stopped = loop {
        let jobs = sessions.jobs().await;
        if jobs.iter().all(|job| job.2 != JobStatus::Running) {
            break jobs;
        }
        assert!(Instant::now() < deadline, "the job did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    seen.push(format!("{stopped:?}"));
    sessions.disown().await;
    seen
}

async fn loopback(root: &Path, limits: RunnerLimits) -> (Loopback, ExecutionEnvironment) {
    let loopback = Loopback::join(service(root, limits), bind(), windows())
        .await
        .unwrap();
    let environment = runner_environment(scope(), root, ".", config(), loopback.backend()).unwrap();
    (loopback, environment)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_sessions_behave_through_the_loopback_as_direct() {
    let direct_root = tempfile::tempdir().unwrap();
    let direct = host(direct_root.path());
    let expected = suite(&Sessions::Direct(direct.joined_process_sessions().unwrap())).await;

    let root = tempfile::tempdir().unwrap();
    let (loopback, environment) = loopback(root.path(), RunnerLimits::default()).await;
    let sessions = RunnerSessions::new(&loopback.backend(), &environment, owner(1, 1));
    assert_eq!(suite(&Sessions::Runner(sessions)).await, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_yield_ends_a_waiting_poll_early() {
    let root = tempfile::tempdir().unwrap();
    let (loopback, environment) = loopback(root.path(), RunnerLimits::default()).await;
    let sessions = RunnerSessions::new(&loopback.backend(), &environment, owner(1, 1));
    let id = sessions
        .start(literal("sleep 30", false), effect(1))
        .await
        .unwrap();
    let (yields, signal) = tokio::sync::watch::channel(ToolPreempt::None);
    let control = OperationContext::new(CancellationToken::new()).with_preempt(signal);
    let preempt = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        yields.send(ToolPreempt::Background).unwrap();
        yields
    });
    let started = Instant::now();
    let polled = sessions
        .poll_from(
            id,
            JoinedProcessOutputCursor::new(0),
            1 << 16,
            64,
            Duration::from_secs(20),
            control,
        )
        .await
        .unwrap();
    assert_eq!(polled.status(), JoinedProcessStatus::Running);
    assert!(started.elapsed() < Duration::from_secs(10));
    drop(preempt.await.unwrap());
    sessions
        .stop_with_grace(id, JobStopReason::Model, Duration::from_millis(200))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_job_bound_holds_across_lines() {
    let root = tempfile::tempdir().unwrap();
    let limits = RunnerLimits {
        job_bound: 1,
        ..RunnerLimits::default()
    };
    let (loopback, environment) = loopback(root.path(), limits).await;
    let first = RunnerSessions::new(&loopback.backend(), &environment, owner(1, 1));
    let other_line = LineOwner::new(
        LineId::from_str("00000000-0000-0000-0000-0000000000a2").unwrap(),
        1,
        BranchId::from_str("00000000-0000-0000-0000-0000000000b2").unwrap(),
        1,
    );
    let second = RunnerSessions::new(&loopback.backend(), &environment, other_line);
    let running = first
        .start(literal("sleep 30", false), effect(1))
        .await
        .unwrap();
    let refused = second
        .start(
            literal("true", false),
            OperationContext::new(CancellationToken::new()).with_owner(other_line),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::ResourceLimit);
    assert!(refused.proven_no_effect());
    first
        .stop_with_grace(running, JobStopReason::Model, Duration::from_millis(200))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_owner_cannot_control_the_line_after_a_transfer() {
    let root = tempfile::tempdir().unwrap();
    let (loopback, environment) = loopback(root.path(), RunnerLimits::default()).await;
    let old = RunnerSessions::new(&loopback.backend(), &environment, owner(1, 1));
    let id = old
        .start(literal("sleep 30", false), effect(1))
        .await
        .unwrap();
    let new = RunnerSessions::new(&loopback.backend(), &environment, owner(2, 1));
    assert_eq!(new.jobs().await.unwrap().len(), 0);
    let refused = old
        .stop_with_grace(id, JobStopReason::Model, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::IdentityMismatch);
    assert!(refused.proven_no_effect());
    // The line's session is the new owner's: it stops it.
    let status = new
        .stop_with_grace(id, JobStopReason::Model, Duration::from_millis(200))
        .await
        .unwrap();
    assert!(status.is_terminal());
}

/// Echoes a payload larger than the initial stream credit through `cat`.
async fn echo(environment: &ExecutionEnvironment) -> Vec<u8> {
    let request = ServiceProcessRequest::new(
        "/bin/cat",
        Vec::<String>::new(),
        Vec::new(),
        Vec::new(),
        1024,
    )
    .unwrap();
    let service = environment
        .spawn_service(request, OperationContext::new(CancellationToken::new()))
        .await
        .unwrap();
    let payload: Vec<u8> = (0..600 * 1024_u32)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect();
    let mut stdin = service.stdin;
    let mut stdout = service.stdout;
    let written = payload.clone();
    let writer = tokio::spawn(async move {
        stdin.write_all(&written).await.unwrap();
        stdin.shutdown().await.unwrap();
    });
    let mut echoed = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stdout.read_to_end(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    writer.await.unwrap();
    service
        .handle
        .terminate(Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(echoed, payload);
    echoed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_streams_through_the_loopback_as_direct() {
    let direct_root = tempfile::tempdir().unwrap();
    let expected = echo(&host(direct_root.path())).await;
    let root = tempfile::tempdir().unwrap();
    let (_loopback, environment) = loopback(root.path(), RunnerLimits::default()).await;
    assert_eq!(echo(&environment).await, expected);

    let secret = ServiceProcessRequest::new(
        "/bin/cat",
        Vec::<String>::new(),
        Vec::new(),
        [(
            plakous_tools::EnvironmentVariableName::new("TOKEN").unwrap(),
            plakous_tools::ServiceSecretValue::new("s3cret".to_owned()).unwrap(),
        )],
        1024,
    )
    .unwrap();
    let refused = environment
        .spawn_service(secret, OperationContext::new(CancellationToken::new()))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::Unsupported);
    assert!(refused.proven_no_effect());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runner_port_mirrors_the_lines_jobs_and_stops_them_on_the_runner() {
    let root = tempfile::tempdir().unwrap();
    let (loopback, environment) = loopback(root.path(), RunnerLimits::default()).await;
    let sessions = std::sync::Arc::new(RunnerSessions::new(
        &loopback.backend(),
        &environment,
        owner(1, 1),
    ));
    let port = plakous_tools::JoinedSessionsPort::runner(
        std::sync::Arc::clone(&sessions),
        tokio::runtime::Handle::current(),
    );
    assert!(port.as_local().is_none());
    let id = sessions
        .start(literal("sleep 30", false), effect(1))
        .await
        .unwrap();
    sessions.promote(id).await.unwrap();
    assert!(!port.has_jobs(), "the mirror holds what the last read saw");
    sessions.jobs().await.unwrap();
    assert_eq!(port.running_job_count(), 1);
    assert_eq!(port.session_for_number(port.jobs()[0].number), Some(id));
    let generation = port.job_terminal_generation();
    // The port's effects wait on a thread of their own, from any caller.
    port.stop_jobs(JobStopReason::Model, Duration::from_millis(200))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while sessions
        .jobs()
        .await
        .unwrap()
        .iter()
        .any(|job| job.status == JobStatus::Running)
    {
        assert!(Instant::now() < deadline, "the job did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(port.running_job_count(), 0);
    assert!(port.job_terminal_generation() > generation);
}
