//! An admitted start queued behind authority changes cannot escape its lease.

#![allow(clippy::unwrap_used, reason = "isolated authority fixture")]

use super::*;
use crate::{ClientWindows, Loopback, RunnerIdentity};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{RunnerSessions, runner_environment};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    JoinedProcessCommand, JoinedProcessStartRequest, JoinedProcessTerminalMode, OperationClass,
    RunnerId,
};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admitted_start_queued_past_its_instance_lease_has_no_effect() {
    queued_start_loses_authority(0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admitted_start_queued_behind_a_newer_transfer_has_no_effect() {
    queued_start_loses_authority(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_instance_cannot_run_a_queued_start_before_readopting_its_orphan() {
    queued_start_loses_authority(2).await;
}

async fn queued_start_loses_authority(change: u8) {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, link) = queued_link(root.path(), scope).await;
    let backend = link.backend();
    let environment = runner_environment(
        scope,
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        1,
    );
    let sessions = RunnerSessions::new(&backend, &environment, owner);
    sessions.jobs().await.unwrap();
    let gate = service
        .inner
        .foreground
        .gate("home", &owner.line().to_string());
    let guard = gate.lock().await;
    let marker = root.path().join("must-not-start");
    let command = format!("printf started > '{}'", marker.display());
    let start = tokio::spawn(async move {
        sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal("/bin/sh", ["-c".to_owned(), command]).unwrap(),
                    ".",
                    Vec::new(),
                    false,
                    JoinedProcessTerminalMode::Pipes,
                )
                .unwrap(),
                OperationContext::new(plakous_model::CancellationToken::new()).with_class(
                    OperationClass::EffectCall {
                        call_id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        attempt_id: "00000000-0000-0000-0000-000000000002".parse().unwrap(),
                    },
                ),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while lock(&service.inner.running).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Its request crossed admission, but could not acquire the execution gate.
    if change == 0 {
        let key = ("home".to_owned(), "before".to_owned());
        let mut leases = lock(&service.inner.leases);
        let lease = leases.get_mut(&key).unwrap();
        lease.connections = 0;
        lease.released = Some(Instant::now());
    } else if change == 1 {
        let mut tables = lock(&service.inner.tables);
        let previous = tables.owner("home", &owner.line().to_string()).unwrap().0;
        let mut newer = previous.clone();
        newer.fence += 1;
        assert!(matches!(
            tables.transfer(
                "home",
                "after",
                "queued-transfer".into(),
                Some(&previous),
                newer
            ),
            RunnerFrame::TransferAccepted { .. }
        ));
    } else {
        // The late bind renews the instance lease, not its orphaned line.
        lock(&service.inner.tables).orphan("home", "before");
        assert!(service.instance_authority("home", "before"));
    }
    drop(guard);
    let refused = tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        refused.kind(),
        plakous_tools::ExecutionBackendErrorKind::IdentityMismatch
    );
    assert!(refused.proven_no_effect());
    assert!(!marker.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_foreground_cleanup_prevents_idle_exit_and_releases_its_gate() {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, link) = queued_link(root.path(), scope).await;
    let backend = link.backend();
    let environment = runner_environment(
        scope,
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        1,
    );
    RunnerSessions::new(&backend, &environment, owner)
        .jobs()
        .await
        .unwrap();
    let gate = service
        .inner
        .foreground
        .gate("home", &owner.line().to_string());
    let guard = gate.lock().await;
    link.sever();
    tokio::time::timeout(Duration::from_secs(5), async {
        while service
            .inner
            .open
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !service.is_idle(),
        "the cleanup is queued behind its line gate"
    );
    assert!(service.inner.foreground.pending());
    drop(guard);
    drop(gate);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !service.is_idle() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(lock(&service.inner.foreground.gates).is_empty());
}

async fn queued_link(
    root: &std::path::Path,
    scope: ExecutionEnvironmentScope,
) -> (RunnerService, Loopback) {
    let environment =
        ExecutionEnvironment::host(scope, root, ".", ExecutionEnvironmentConfig::default())
            .unwrap();
    let version = VersionTriple::current("queued-authority", "test");
    let service = RunnerService::with_limits(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("queued").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
        crate::RunnerLimits {
            owner_lease: Duration::ZERO,
            ..crate::RunnerLimits::default()
        },
    )
    .unwrap();
    let link = Loopback::join(
        service.clone(),
        Bind {
            host_id: "home".into(),
            host_instance: "before".into(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    (service, link)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_inspection_distinguishes_foreground_jobs_and_terminal_work() {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, link) = queued_link(root.path(), scope).await;
    let backend = link.backend();
    let environment = runner_environment(
        scope,
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        1,
    );
    let sessions = RunnerSessions::new(&backend, &environment, owner);
    sessions.jobs().await.unwrap();
    assert!(!service.lines(Some("home")).await[0].foreground);
    let id = sessions
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/sh", ["-c".into(), "read line".into()])
                    .unwrap(),
                ".",
                Vec::new(),
                true,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            OperationContext::new(plakous_model::CancellationToken::new()).with_class(
                OperationClass::EffectCall {
                    call_id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    attempt_id: "00000000-0000-0000-0000-000000000002".parse().unwrap(),
                },
            ),
        )
        .await
        .unwrap();
    let status = &service.lines(Some("home")).await[0];
    assert!(status.foreground);
    assert_eq!(status.jobs["jobs"].as_array().unwrap().len(), 0);
    sessions.promote(id).await.unwrap();
    let status = &service.lines(Some("home")).await[0];
    assert!(!status.foreground);
    assert_eq!(status.jobs["jobs"][0]["status"]["status"], "running");
    sessions
        .stop_jobs(plakous_tools::JobStopReason::User, Duration::ZERO)
        .await
        .unwrap();
    let status = &service.lines(Some("home")).await[0];
    assert!(!status.foreground);
    assert_eq!(status.jobs["jobs"][0]["status"]["status"], "stopped");
}
