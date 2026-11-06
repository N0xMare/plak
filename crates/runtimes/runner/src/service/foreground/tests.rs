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
    queued_start_loses_authority(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admitted_start_queued_behind_a_newer_transfer_has_no_effect() {
    queued_start_loses_authority(false).await;
}

async fn queued_start_loses_authority(expire: bool) {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let environment = ExecutionEnvironment::host(
        scope,
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let version = VersionTriple::current("queued-authority", "test");
    let service = RunnerService::new(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("queued").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
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
    if expire {
        let key = ("home".to_owned(), "before".to_owned());
        let mut leases = lock(&service.inner.leases);
        let lease = leases.get_mut(&key).unwrap();
        lease.connections = 0;
        lease.released = Some(Instant::now() - service.inner.owner_lease - Duration::from_secs(1));
    } else {
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
