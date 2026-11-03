//! The loopback crash shapes (spec 01 §8.4): a job outlives the host process
//! that started it, and a runner restart leaves the jobs it held of unknown
//! outcome. They run in the durability lane: each waits on real processes
//! across a lost host or runner.

#![allow(clippy::unwrap_used, reason = "integration tests")]

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{ClientWindows, Loopback, RunnerIdentity, RunnerService};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{RunnerSessions, runner_environment};
use plakous_tools::{
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentLimits, ExecutionEnvironmentScope, JobStatus, JobStopReason,
    JoinedProcessCommand, JoinedProcessOutputCursor, JoinedProcessStartRequest,
    JoinedProcessStatus, JoinedProcessTerminalMode, LineOwner, OperationClass, OperationContext,
    RunnerId,
};

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap())
}

fn config() -> ExecutionEnvironmentConfig {
    ExecutionEnvironmentConfig::new(ExecutionEnvironmentLimits::default(), [], []).unwrap()
}

fn service(root: &Path) -> RunnerService {
    RunnerService::new(
        ExecutionEnvironment::host(scope(), root, ".", config()).unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: VersionTriple::current("shapes", "debug"),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
    )
    .unwrap()
}

fn bind(host_instance: &str) -> Bind {
    Bind {
        host_id: "host-1".into(),
        host_instance: host_instance.into(),
        version: VersionTriple::current("shapes", "debug"),
        inspect: false,
    }
}

fn windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_secs(10),
        effect: Duration::from_secs(10),
        ..ClientWindows::default()
    }
}

fn owner(generation: u64) -> LineOwner {
    LineOwner::new(
        LineId::from_str("00000000-0000-0000-0000-0000000000a1").unwrap(),
        generation,
        BranchId::from_str("00000000-0000-0000-0000-0000000000b1").unwrap(),
        1,
    )
}

fn effect(owner: LineOwner) -> OperationContext {
    OperationContext::new(CancellationToken::new())
        .with_owner(owner)
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
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

async fn host(
    service: &RunnerService,
    root: &Path,
    instance: &str,
) -> (Loopback, ExecutionEnvironment) {
    let loopback = Loopback::join(service.clone(), bind(instance), windows())
        .await
        .unwrap();
    let environment = runner_environment(scope(), root, ".", config(), loopback.backend()).unwrap();
    (loopback, environment)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_job_survives_host_loss() {
    let root = tempfile::tempdir().unwrap();
    let runner = service(root.path());
    let (first, environment) = host(&runner, root.path(), "instance-1").await;
    let before = RunnerSessions::new(&first.backend(), &environment, owner(1));
    let id = before
        .start(job("sleep 1; echo done"), effect(owner(1)))
        .await
        .unwrap();
    before.promote(id).await.unwrap();
    // The host process is lost: its link and its handle go with it.
    drop(before);
    drop(first);

    let (second, environment) = host(&runner, root.path(), "instance-2").await;
    let after = RunnerSessions::new(&second.backend(), &environment, owner(2));
    let jobs = after.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::Running);
    let mut output = Vec::new();
    let mut cursor = JoinedProcessOutputCursor::new(0);
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        let polled = after
            .poll_from(
                id,
                cursor,
                1 << 16,
                64,
                Duration::from_secs(1),
                OperationContext::new(CancellationToken::new()),
            )
            .await
            .unwrap();
        for record in polled.records() {
            output.extend_from_slice(record.bytes());
        }
        cursor = polled.next_cursor();
        if polled.status().is_terminal() && polled.records().is_empty() {
            break polled.status();
        }
        assert!(Instant::now() < deadline, "the job did not end");
    };
    assert_eq!(output, b"done\n");
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    // The lost host's owner is fenced off the line.
    let stale = RunnerSessions::new(&second.backend(), &environment, owner(1));
    let refused = stale.jobs().await.unwrap_err();
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::IdentityMismatch);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_restart_unknown() {
    let root = tempfile::tempdir().unwrap();
    let (mut loopback, environment) = host(&service(root.path()), root.path(), "instance-1").await;
    let sessions = RunnerSessions::new(&loopback.backend(), &environment, owner(1));
    let id = sessions
        .start(job("sleep 3"), effect(owner(1)))
        .await
        .unwrap();
    sessions.promote(id).await.unwrap();
    let number = sessions.jobs().await.unwrap()[0].number;

    loopback.restart(service(root.path())).await.unwrap();
    let jobs = sessions.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].number, number);
    assert_eq!(jobs[0].status, JobStatus::OutcomeUnknown);
    // The new instance knows no session of the old one, and the line still
    // works there: it is transferred again first.
    sessions
        .poll_from(
            id,
            JoinedProcessOutputCursor::new(0),
            1 << 16,
            64,
            Duration::ZERO,
            OperationContext::new(CancellationToken::new()),
        )
        .await
        .unwrap_err();
    // A job of the new instance takes the next number, never the old
    // job's; the old job's unknown outcome stays beside it.
    let fresh = sessions
        .start(
            job("sleep 3"),
            OperationContext::new(CancellationToken::new()).with_owner(owner(1)),
        )
        .await
        .unwrap();
    sessions.promote(fresh).await.unwrap();
    let jobs = sessions.jobs().await.unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].number, number);
    assert_eq!(jobs[0].status, JobStatus::OutcomeUnknown);
    assert_eq!(jobs[1].number, number + 1);
    assert_eq!(jobs[1].status, JobStatus::Running);
    sessions
        .stop_jobs(JobStopReason::Model, Duration::from_millis(200))
        .await
        .unwrap();
}
