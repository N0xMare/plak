#![allow(clippy::unwrap_used, reason = "isolated native job-ack fixture")]

use super::*;
use plakous_tools::{JobStatus, JoinedProcessOutputCursor};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_native_job_ack_keeps_running_and_consumes_terminal_once() {
    use plakous_tools::{
        JoinedProcessCommand, JoinedProcessInputDisposition, JoinedProcessStartRequest,
        JoinedProcessTerminalMode,
    };
    let root = tempfile::tempdir().unwrap();
    let (service, link, port, owner) = port(root.path()).await;
    let control =
        OperationContext::new(CancellationToken::new()).with_class(OperationClass::EffectCall {
            call_id: "00000000-0000-0000-0000-000000000002".parse().unwrap(),
            attempt_id: "00000000-0000-0000-0000-000000000003".parse().unwrap(),
        });
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/cat", Vec::<String>::new()).unwrap(),
        ".",
        Vec::new(),
        true,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap();
    let process = port.start(request, control.clone()).await.unwrap();
    port.promote(process).await.unwrap();
    let number = JoinedProcessSessions::job_number(process);
    let running_ack = port.acknowledge_jobs(vec![number]).await;
    let running = port.jobs().await.unwrap()[0].status;
    port.write_stdin(
        process,
        b"kept",
        JoinedProcessInputDisposition::Close,
        control,
    )
    .await
    .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let output = port
            .poll_from(
                process,
                JoinedProcessOutputCursor::new(0),
                1024,
                8,
                Duration::from_millis(20),
                OperationContext::new(CancellationToken::new()),
            )
            .await
            .unwrap();
        if output.status().is_terminal() {
            break;
        }
        assert!(Instant::now() < until);
    }
    port.acknowledge_jobs(vec![number]).await.unwrap();
    port.acknowledge_jobs(vec![number]).await.unwrap();
    assert_eq!(port.jobs().await.unwrap(), Vec::new());
    let actual = lock(&service.inner.lines)
        .get(&("home".into(), owner.line().to_string()))
        .unwrap()
        .0
        .clone();
    link.sever();
    service.shutdown().await;
    assert!(running_ack.is_err());
    assert_eq!(running, JobStatus::Running);
    assert!(!actual.has_unreported_terminal_job());
    assert_eq!(
        plakous_tools::internal::next_job_number(&actual),
        number + 1
    );
}

async fn port(
    root: &Path,
) -> (
    RunnerService,
    crate::Loopback,
    plakous_tools::internal::runner::RunnerSessions,
    plakous_tools::LineOwner,
) {
    use plakous_tools::internal::runner::{RunnerSessions, runner_environment};
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentLimits,
        ExecutionEnvironmentScope, RunnerId,
    };
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap());
    let limits = ExecutionEnvironmentLimits::new(
        1024 * 1024,
        1024 * 1024,
        2 * 1024 * 1024,
        1024 * 1024,
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
    .unwrap()
    .with_session_duration(Duration::from_secs(30))
    .unwrap();
    let config = ExecutionEnvironmentConfig::new(limits, [], []).unwrap();
    let environment = ExecutionEnvironment::host(scope, root, ".", config.clone()).unwrap();
    let version = VersionTriple::current("ack-fixture", "test");
    let service = RunnerService::new(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("ack").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let link = crate::Loopback::join(
        service.clone(),
        Bind {
            host_id: "home".into(),
            host_instance: "first".into(),
            version,
            inspect: false,
        },
        crate::ClientWindows::default(),
    )
    .await
    .unwrap();
    let backend = link.backend();
    let environment = runner_environment(scope, root, ".", config, backend.clone()).unwrap();
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        1,
    );
    let port = RunnerSessions::new(&backend, &environment, owner);
    (service, link, port, owner)
}
