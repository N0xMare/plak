#![allow(clippy::unwrap_used, reason = "isolated retained-job workers")]

use super::*;
use crate::{
    ClientWindows, FileOwnerFloors, Loopback, RunnerIdentity, RunnerLimits, RunnerOptions,
};
use plakous_model::CancellationToken;
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{RunnerSessions, runner_environment};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JobStatus,
    JoinedProcessCommand, JoinedProcessInputDisposition, JoinedProcessOutputCursor,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStartRequest,
    JoinedProcessTerminalMode, OperationClass, OperationContext, RunnerId,
};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod loss;
mod review;

struct Worker {
    service: RunnerService,
    directory: PathBuf,
    serving: tokio::task::JoinHandle<()>,
}

impl Worker {
    fn service(root: &Path, revision: &str) -> (RunnerService, PathBuf) {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let version = VersionTriple::current(revision, "retention-fixture");
        let directory = root
            .join("builds")
            .join(crate::daemon::triple_key(&version));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let environment = ExecutionEnvironment::host(
            run_scope(),
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap();
        let service = RunnerService::with_options(
            environment,
            RunnerIdentity {
                runner_id: RunnerId::new("fixture").unwrap(),
                version,
                boot_id: "boot".into(),
                binary_id: None,
            },
            RunnerOptions {
                floors: Some(Arc::new(
                    FileOwnerFloors::open(directory.join("owners.json")).unwrap(),
                )),
                limits: RunnerLimits {
                    job_bound: 1,
                    ..RunnerLimits::default()
                },
                ..RunnerOptions::default()
            },
        )
        .unwrap();
        (service, directory)
    }

    async fn new(root: &Path, revision: &str) -> Self {
        Self::with_cut(root, revision, None).await
    }

    async fn with_cut(root: &Path, revision: &str, cut: Option<Arc<loss::ReplyCut>>) -> Self {
        let (service, directory) = Self::service(root, revision);
        let endpoint = crate::retention::Endpoint::bind(&directory, &service)
            .await
            .unwrap();
        let serving_service = service.clone();
        let serving = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = endpoint.listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let service = serving_service.clone();
                        let origin = endpoint.origin.clone();
                        let cut = cut.clone();
                        clients.spawn(async move {
                            if let Some(cut) = cut { loss::serve_cut(service, stream, origin, cut).await; }
                            else { let _ = crate::retention::serve(service, stream, origin).await; }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Self {
            service,
            directory,
            serving,
        }
    }

    async fn port(&self, root: &Path, instance: &str, fence: u64) -> (Loopback, RunnerSessions) {
        self.port_for_line(
            root,
            instance,
            fence,
            "00000000-0000-0000-0000-0000000000a1",
        )
        .await
    }

    async fn port_for_line(
        &self,
        root: &Path,
        instance: &str,
        fence: u64,
        owner_line: &str,
    ) -> (Loopback, RunnerSessions) {
        let link = Loopback::join(
            self.service.clone(),
            Bind {
                host_id: "home".into(),
                host_instance: instance.into(),
                version: self.service.hello().version.clone(),
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let backend = link.backend();
        let environment = runner_environment(
            run_scope(),
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
            backend.clone(),
        )
        .unwrap();
        let owner = plakous_tools::LineOwner::new(
            owner_line.parse().unwrap(),
            1,
            "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
            fence,
        );
        (link, RunnerSessions::new(&backend, &environment, owner))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.service.stop_retention_maintenance();
        self.serving.abort();
        for (_, (sessions, _)) in std::mem::take(&mut *lock(&self.service.inner.lines)) {
            let _ = sessions.stop_foreground();
            let _ = sessions.stop_jobs(plakous_tools::JobStopReason::SessionExit, Duration::ZERO);
        }
    }
}

fn run_scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap())
}

fn control(call: u128) -> OperationContext {
    OperationContext::new(CancellationToken::new()).with_class(OperationClass::EffectCall {
        call_id: format!("00000000-0000-0000-0000-{call:012x}")
            .parse()
            .unwrap(),
        attempt_id: "00000000-0000-0000-0000-000000000003".parse().unwrap(),
    })
}

fn command() -> JoinedProcessStartRequest {
    JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/cat", Vec::<String>::new()).unwrap(),
        ".",
        Vec::new(),
        true,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap()
}

async fn finish(port: &RunnerSessions, process: JoinedProcessSessionId) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cursor = JoinedProcessOutputCursor::new(0);
    let mut bytes = Vec::new();
    loop {
        let output = port
            .poll_from(
                process,
                cursor,
                1024,
                64,
                Duration::from_millis(20),
                OperationContext::new(CancellationToken::new()),
            )
            .await
            .unwrap();
        for record in output.records() {
            bytes.extend_from_slice(record.bytes());
        }
        cursor = output.next_cursor();
        if output.status().is_terminal() && !output.more_output() {
            return bytes;
        }
        assert!(Instant::now() < deadline);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_three_workers_delegate_directly_fence_old_control_and_count_once() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(2)).await.unwrap();
    old.promote(process).await.unwrap();
    let number = JoinedProcessSessions::job_number(process);
    let second = Worker::new(root.path(), "second").await;
    let (middle_link, middle) = second.port(root.path(), "middle-host", 2).await;
    assert_eq!(middle.jobs().await.unwrap()[0].status, JobStatus::Running);
    assert!(
        old.write_stdin(
            process,
            b"must-not-write",
            JoinedProcessInputDisposition::KeepOpen,
            control(4)
        )
        .await
        .is_err()
    );
    assert!(middle.start(command(), control(5)).await.is_err());
    let third = Worker::new(root.path(), "third").await;
    let (new_link, new) = third.port(root.path(), "new-host", 3).await;
    let jobs = new.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].number, number);
    let entries = third
        .service
        .retention_context()
        .unwrap()
        .registry
        .entries("home", None);
    assert!(
        entries
            .iter()
            .any(|entry| entry.inventory.origin.instance.as_str()
                == first.service.hello().instance
                && !entry.inventory.jobs.is_empty())
    );
    second.serving.abort();
    assert!(
        middle
            .write_stdin(
                process,
                b"must-not-write",
                JoinedProcessInputDisposition::KeepOpen,
                control(6)
            )
            .await
            .is_err()
    );
    new.write_stdin(
        process,
        b"once",
        JoinedProcessInputDisposition::Close,
        control(7),
    )
    .await
    .unwrap();
    assert_eq!(finish(&new, process).await, b"once");
    let context = third.service.retention_context().unwrap();
    let stale = context
        .registry
        .entries("home", None)
        .into_iter()
        .find(|entry| !entry.inventory.jobs.is_empty())
        .unwrap();
    new.acknowledge_jobs(vec![number]).await.unwrap();
    context
        .registry
        .update(
            &scope("home", "new-host").unwrap(),
            &stale.inventory.origin,
            &stale.to,
            stale.inventory.jobs,
            stale.inventory.next_number,
        )
        .unwrap();
    assert_eq!(new.jobs().await.unwrap(), Vec::new());
    let replacement = new.start(command(), control(8)).await.unwrap();
    assert!(JoinedProcessSessions::job_number(replacement) > number);
    new.write_stdin(
        replacement,
        b"",
        JoinedProcessInputDisposition::Close,
        control(9),
    )
    .await
    .unwrap();
    assert_eq!(finish(&new, replacement).await, Vec::<u8>::new());
    old_link.sever();
    middle_link.sever();
    new_link.sever();
    assert!(!first.service.retention_keeps_alive(Instant::now()));
    assert!(first.directory.join("owners.json").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_three_workers_consumed_original_can_exit_before_the_next_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(20)).await.unwrap();
    old.promote(process).await.unwrap();
    let second = Worker::new(root.path(), "second").await;
    let (_middle_link, middle) = second.port(root.path(), "middle-host", 2).await;
    let number = middle.jobs().await.unwrap()[0].number;
    middle
        .write_stdin(
            process,
            b"",
            JoinedProcessInputDisposition::Close,
            control(21),
        )
        .await
        .unwrap();
    assert_eq!(finish(&middle, process).await, Vec::<u8>::new());
    middle.acknowledge_jobs(vec![number]).await.unwrap();
    first.serving.abort();
    while !first.serving.is_finished() {
        tokio::task::yield_now().await;
    }
    let third = Worker::new(root.path(), "third").await;
    let (_new_link, new) = third.port(root.path(), "new-host", 3).await;
    assert_eq!(new.jobs().await.unwrap(), Vec::new());
    let next = new.start(command(), control(22)).await.unwrap();
    assert!(JoinedProcessSessions::job_number(next) > number);
    new.write_stdin(next, b"", JoinedProcessInputDisposition::Close, control(23))
        .await
        .unwrap();
    finish(&new, next).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_lifetime_never_expires_a_running_job_and_uses_original_terminal_time() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(30)).await.unwrap();
    old.promote(process).await.unwrap();
    let second = Worker::new(root.path(), "second").await;
    let (_new_link, new) = second.port(root.path(), "new-host", 2).await;
    assert_eq!(new.jobs().await.unwrap().len(), 1);
    let future = Instant::now() + Duration::from_secs(25 * 3600);
    assert!(first.service.retention_keeps_alive(future));
    new.write_stdin(
        process,
        b"alive",
        JoinedProcessInputDisposition::Close,
        control(31),
    )
    .await
    .unwrap();
    assert_eq!(finish(&new, process).await, b"alive");
    assert!(first.service.retention_keeps_alive(Instant::now()));
    assert!(!first.service.retention_keeps_alive(future));
    // The host may still hold its already-observed terminal fact until commit.
    new.jobs().await.unwrap();
    assert!(
        first
            .service
            .inner
            .lines
            .lock()
            .unwrap()
            .values()
            .all(|(sessions, _)| sessions.jobs().is_empty())
    );
}
