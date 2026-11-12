//! Operator PTYs cross the runner link without creating an agent line or job.

#![allow(clippy::unwrap_used, reason = "isolated runner terminal fixtures")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use plakous_runner::{ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService};
use plakous_runner_protocol::operator::{
    OperatorAnswer, OperatorId, OperatorNamespace, OperatorOperation, TerminalDimensions,
    TerminalEvent, TerminalKey, TerminalStatus,
};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

fn id(s: &str) -> OperatorId {
    OperatorId::try_from(s.to_owned()).unwrap()
}
fn namespace() -> OperatorNamespace {
    OperatorNamespace {
        state_key: id("state"),
        principal: id("operator"),
    }
}
fn key() -> TerminalKey {
    TerminalKey {
        session: id("session"),
        node: id("node"),
        terminal_id: id("terminal"),
    }
}

async fn call(client: &RunnerClient, operation: OperatorOperation) -> OperatorAnswer {
    client
        .operator_control(namespace(), operation)
        .await
        .unwrap()
}

fn service(root: &std::path::Path, home_local: bool) -> RunnerService {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        root,
        root,
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("operator-test").unwrap(),
            version: VersionTriple::current("revision", "debug"),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            home_local: home_local.then(|| "paired".into()),
            operator_workspace: Some(Arc::new(|cwd| {
                let mut launch = plakous_runner::operator::account_launch(
                    cwd.to_path_buf(),
                    "/usr/bin:/bin".into(),
                    None,
                )?;
                launch.shell = "/bin/sh".into();
                launch.home = cwd.to_path_buf();
                Ok(launch)
            })),
            ..RunnerOptions::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn operator_pty_wire_and_home_local_have_no_agent_jobs_or_lines() {
    for home_local in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let runner = service(directory.path(), home_local);
        let (client, serving) = connect(&runner).await;
        call(&client, OperatorOperation::Renew).await;
        let OperatorAnswer::Terminal { terminal, .. } = call(
            &client,
            OperatorOperation::Open {
                key: key(),
                workspace: directory.path().to_string_lossy().into_owned(),
                dimensions: TerminalDimensions::new(24, 80).unwrap(),
            },
        )
        .await
        else {
            panic!("open failed");
        };
        assert_eq!(terminal.status, TerminalStatus::Running);
        assert_eq!(client.lines().await.unwrap(), Vec::new());
        let OperatorAnswer::Terminal {
            stream: Some(stream),
            ..
        } = call(
            &client,
            OperatorOperation::Attach {
                takeover: false,
                key: key(),
                cursor: terminal.oldest,
            },
        )
        .await
        else {
            panic!("attach failed");
        };
        let mut stream = client.operator_stream(stream).unwrap();
        stream.credit(64 * 1024).unwrap();
        stream.input(0, b"printf 'OPERATOR-ONLY\\n'\n").unwrap();
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let TerminalEvent::Data { bytes_b64, .. } =
                tokio::time::timeout_at(deadline, stream.next())
                    .await
                    .unwrap()
                    .unwrap()
            {
                output.extend(STANDARD.decode(bytes_b64).unwrap());
            }
            if output
                .windows(b"OPERATOR-ONLY\r\n".len())
                .any(|b| b == b"OPERATOR-ONLY\r\n")
            {
                break;
            }
        }
        assert_eq!(client.lines().await.unwrap(), Vec::new());
        assert!(!runner.is_idle());
        assert!(
            runner
                .operator_terminals()
                .unwrap()
                .workspace_pinned(directory.path())
        );
        call(
            &client,
            OperatorOperation::Close {
                generation: None,
                key: key(),
                acknowledge_loss: false,
            },
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while runner.operator_terminals().unwrap().has_live_terminals() {
            assert!(Instant::now() < deadline, "cleanup not proven");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(stream);
        client.disconnect();
        tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .unwrap()
            .unwrap();
        assert!(runner.is_idle());
    }
}

async fn connect(runner: &RunnerService) -> (RunnerClient, tokio::task::JoinHandle<()>) {
    let served = runner.clone();
    let (host, remote) = tokio::io::duplex(4096);
    let serving = tokio::spawn(async move {
        served.serve_connection(remote).await;
    });
    let (client, hello) = RunnerClient::connect(
        host,
        Bind {
            host_id: "paired".into(),
            host_instance: "generation".into(),
            version: VersionTriple::current("revision", "debug"),
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    assert!(hello.capabilities.iter().any(|c| c == "operator-pty"));
    (client, serving)
}

#[tokio::test]
async fn operator_pty_detached_terminal_alone_prevents_daemon_idle_exit() {
    let directory = tempfile::tempdir().unwrap();
    let runner = service(directory.path(), true);
    assert!(runner.is_idle());
    let operator = runner.operator_terminals().unwrap();
    let answer = operator.execute(
        "paired",
        "generation",
        1,
        namespace(),
        "renew",
        OperatorOperation::Renew,
    );
    assert!(matches!(answer, OperatorAnswer::Table { .. }));
    let answer = operator.execute(
        "paired",
        "generation",
        1,
        namespace(),
        "open",
        OperatorOperation::Open {
            key: key(),
            workspace: directory.path().to_string_lossy().into_owned(),
            dimensions: TerminalDimensions::new(24, 80).unwrap(),
        },
    );
    assert!(
        matches!(answer,OperatorAnswer::Terminal {terminal,..} if terminal.status==TerminalStatus::Running)
    );
    assert!(
        !runner.is_idle(),
        "no connection or agent job exists; the terminal alone must block idle exit"
    );
    operator.close_all();
    let deadline = Instant::now() + Duration::from_secs(5);
    while operator.has_live_terminals() {
        assert!(
            Instant::now() < deadline,
            "detached shell cleanup not proven"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(runner.is_idle());
}
