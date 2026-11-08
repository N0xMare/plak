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
async fn reports_survive_a_new_owner_until_the_consumers_receipt_is_acknowledged() {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, _link) = queued_link(root.path(), scope).await;
    let report = StoppedForeground {
        call_key: "call:attempt".into(),
        pid: 123,
        ended: "Stopped".into(),
    };
    let mut owner = LineOwner {
        line: "line".into(),
        generation: 1,
        branch: "owner".into(),
        fence: 1,
    };
    lock(&service.inner.foreground.reports).insert(
        ("home".into(), "line".into()),
        Reports {
            stopped: vec![report.clone()],
            delivered: HashSet::new(),
        },
    );
    let receipt = plakous_runner_protocol::foreground_report_id("line", &report);
    service
        .inner
        .foreground
        .acknowledge("home", "first", &receipt);
    assert_eq!(
        service.foreground_report("home", "first", &owner),
        vec![report.clone()]
    );
    owner.fence += 1;
    assert_eq!(
        service.foreground_report("home", "second", &owner),
        vec![report.clone()]
    );
    service
        .inner
        .foreground
        .acknowledge("other-home", "second", &receipt);
    service
        .inner
        .foreground
        .acknowledge("home", "undelivered", &receipt);
    let wrong_pid = StoppedForeground {
        pid: 456,
        ..report.clone()
    };
    service.inner.foreground.acknowledge(
        "home",
        "second",
        &plakous_runner_protocol::foreground_report_id("line", &wrong_pid),
    );
    assert!(service.inner.foreground.pending());
    service
        .inner
        .foreground
        .acknowledge("home", "second", &receipt);
    assert_eq!(service.foreground_report("home", "second", &owner), []);
    assert!(!service.inner.foreground.pending());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answered_start_survives_live_reconnect_but_terminal_driver_cleanup_preserves_jobs() {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, mut link) = link_with_lease(root.path(), scope, Duration::from_secs(300)).await;
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
    let start = || {
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal("/bin/sh", ["-c".into(), "read line".into()]).unwrap(),
            ".",
            Vec::new(),
            true,
            JoinedProcessTerminalMode::Pipes,
        )
        .unwrap()
    };
    let first: plakous_model::CallId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
    let second: plakous_model::CallId = "00000000-0000-0000-0000-000000000002".parse().unwrap();
    let attempt: plakous_model::AttemptId = "00000000-0000-0000-0000-000000000003".parse().unwrap();
    let control = |call_id| {
        OperationContext::new(plakous_model::CancellationToken::new()).with_class(
            OperationClass::EffectCall {
                call_id,
                attempt_id: attempt,
            },
        )
    };
    let job = sessions.start(start(), control(first)).await.unwrap();
    sessions.promote(job).await.unwrap();
    sessions.start(start(), control(second)).await.unwrap();
    link.reconnect().await.unwrap();
    sessions.jobs().await.unwrap();
    assert!(
        service.lines(Some("home")).await[0].foreground,
        "answer/Ack and a reconnect inside the effect window do not settle a driver operation"
    );
    let pid = lock(&service.inner.lines)
        .get(&("home".into(), owner.line().to_string()))
        .map(|(sessions, _)| plakous_tools::internal::live_foreground(sessions)[0].1)
        .unwrap();
    link.reconnect().await.unwrap();
    let gate = service
        .inner
        .foreground
        .gate("home", &owner.line().to_string());
    let transfer = gate.lock().await;
    let next_control =
        control("00000000-0000-0000-0000-000000000004".parse().unwrap()).with_owner(owner);
    let next_environment = environment.clone();
    let following = tokio::spawn(async move {
        next_environment.execute_literal_process(plakous_tools::LiteralProcessRequest::new(
            "/bin/sh", ["-c".to_owned(), format!("if kill -0 {pid} 2>/dev/null; then printf escaped > effect-before-cleanup; fi")],
            ".", Vec::new(), plakous_tools::LiteralProcessIntent::default(),
        ).unwrap(), next_control).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::strong_count(&gate) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The next effect already passed its first cleanup snapshot and is now
    // awaiting transfer. Settlement must still close its dispatch fence.
    backend
        .link()
        .driver_operation_settled(owner, second, attempt);
    drop(transfer);
    following.await.unwrap().unwrap();
    assert!(!root.path().join("effect-before-cleanup").exists());
    tokio::time::timeout(Duration::from_secs(5), async {
        while service.lines(Some("home")).await[0].foreground {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service.lines(Some("home")).await[0].jobs["jobs"][0]["status"]["status"],
        "running"
    );
    sessions
        .stop_jobs(plakous_tools::JobStopReason::User, Duration::ZERO)
        .await
        .unwrap();
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_admitted_after_closing_is_refused() {
    queued_start_loses_authority(3).await;
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
    if change == 3 {
        service
            .inner
            .closing
            .store(true, std::sync::atomic::Ordering::Release);
    }
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
    } else if change == 2 {
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
    link_with_lease(root, scope, Duration::ZERO).await
}

async fn link_with_lease(
    root: &std::path::Path,
    scope: ExecutionEnvironmentScope,
    lease: Duration,
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
            owner_lease: lease,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_joins_foreground_and_promoted_processes() {
    for promoted in [false, true] {
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
        let id = sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal(
                        "/bin/sh",
                        ["-c".to_owned(), "read line".to_owned()],
                    )
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
        if promoted {
            sessions.promote(id).await.unwrap();
        }
        let local = lock(&service.inner.lines)
            .values()
            .next()
            .unwrap()
            .0
            .clone();
        let ids = local.active_session_ids();
        assert_eq!(ids.len(), 1);
        assert!(local.joined_terminal_status(ids[0]).is_none());
        service.shutdown().await;
        assert!(local.joined_terminal_status(ids[0]).is_some());
    }
}

#[tokio::test]
async fn shutdown_stops_a_process_published_after_its_first_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let (service, link) = queued_link(root.path(), scope).await;
    let backend = link.backend();
    // Construction probes the runner synchronously. Keep the sole async
    // thread driving its link while the blocking probe waits for answers.
    let environment = {
        let root = root.path().to_owned();
        let backend = backend.clone();
        tokio::task::spawn_blocking(move || {
            runner_environment(
                scope,
                root,
                ".",
                ExecutionEnvironmentConfig::default(),
                backend,
            )
        })
        .await
        .unwrap()
        .unwrap()
    };
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        1,
    );
    let sessions = RunnerSessions::new(&backend, &environment, owner);
    sessions
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/sh", ["-c".to_owned(), "read line".to_owned()])
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
    // Model a start that passed its fence but has not published its port yet.
    let (key, port) = lock(&service.inner.lines).drain().next().unwrap();
    let local = port.0.clone();
    let ids = local.active_session_ids();
    let pending = (
        "home".into(),
        super::super::RunKey::Request("publishing".into()),
    );
    let cancellation = plakous_model::CancellationToken::new();
    lock(&service.inner.running).insert(
        pending.clone(),
        super::super::Running {
            instance: "before".into(),
            cancellation: cancellation.clone(),
            preempt: tokio::sync::watch::channel(plakous_tools::ToolPreempt::default()).0,
        },
    );
    let stopping = {
        let service = service.clone();
        tokio::spawn(async move { service.shutdown().await })
    };
    // On this single-thread runtime the empty first snapshot cannot yield;
    // the cancellation wait resumes us only after shutdown starts its drain.
    tokio::time::timeout(Duration::from_secs(10), async {
        cancellation.cancelled().await;
        lock(&service.inner.lines).insert(key, port);
        lock(&service.inner.running).remove(&pending);
        stopping.await.unwrap();
    })
    .await
    .unwrap();
    let ended = local.joined_terminal_status(ids[0]).is_some();
    local.stop_foreground().unwrap();
    assert!(
        ended,
        "a late-published process escaped the second snapshot"
    );
}

async fn socket_link(
    root: &std::path::Path,
    scope: ExecutionEnvironmentScope,
) -> (
    RunnerService,
    Arc<plakous_tools::internal::runner::RunnerBackend>,
    std::path::PathBuf,
    tokio::task::JoinHandle<Result<(), crate::daemon::DaemonError>>,
) {
    use plakous_tools::internal::runner::RunnerBackend;
    let environment =
        ExecutionEnvironment::host(scope, root, ".", ExecutionEnvironmentConfig::default())
            .unwrap();
    let version = VersionTriple::current("endpoint", "test");
    let service = RunnerService::new(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("endpoint").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let socket = root.join("run/runner.sock");
    let serving = {
        let (service, socket) = (service.clone(), socket.clone());
        tokio::spawn(async move {
            crate::daemon::serve_socket(service, &socket, Duration::from_secs(300)).await
        })
    };
    // The socket file appears at bind(), before listen(); retry until the
    // daemon accepts rather than racing that window.
    let stream = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("runner socket: {error}"),
            }
        }
    })
    .await
    .unwrap();
    let (client, hello) = crate::RunnerClient::connect(
        stream,
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
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client)).unwrap());
    (service, backend, socket, serving)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_loss_joins_processes_started_through_its_socket() {
    for promoted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let scope =
            ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
        let (service, backend, socket, serving) = socket_link(root.path(), scope).await;
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
        let id = sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal(
                        "/bin/sh",
                        ["-c".to_owned(), "read line".to_owned()],
                    )
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
        if promoted {
            sessions.promote(id).await.unwrap();
        }
        let local = lock(&service.inner.lines)
            .values()
            .next()
            .unwrap()
            .0
            .clone();
        let ids = local.active_session_ids();
        assert_eq!(ids.len(), 1);
        assert!(local.joined_terminal_status(ids[0]).is_none());
        std::fs::remove_file(&socket).unwrap();
        tokio::time::timeout(Duration::from_secs(10), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ended = local.joined_terminal_status(ids[0]).is_some();
        // A killed mutant must not leave its deliberately live process behind.
        local.stop_foreground().unwrap();
        local
            .stop_jobs(plakous_tools::JobStopReason::User, Duration::ZERO)
            .unwrap();
        assert!(
            ended,
            "endpoint loss returned while an owned process was still live"
        );
    }
}
