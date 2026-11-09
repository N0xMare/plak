//! Opt-in strict-SSH placement smoke against an operator-provisioned test runner.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "opt-in runner fixture"
)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{
    ClientWindows, ConfiguredRunner, RunnerClient, RunnerRegistry, RunnerServices,
};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::{
    RunnerBackend, RunnerCall, RunnerLink as _, RunnerSessions, RunnerVerb, runner_environment,
};
use plakous_tools::{
    ExecutionEnvironmentConfig, ExecutionEnvironmentScope, LineOwner, LiteralProcessIntent,
    LiteralProcessRequest, OperationClass, OperationContext, WorkspaceReadSliceRequest,
};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("the opt-in smoke requires {name}"))
}

fn ssh(destination: &str, command: &str) -> tokio::process::Command {
    let mut ssh = tokio::process::Command::new("ssh");
    ssh.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "ForwardAgent=no",
        "-o",
        "GSSAPIDelegateCredentials=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ForwardX11Trusted=no",
        "-o",
        "Tunnel=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "SendEnv=-*",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "RemoteCommand=none",
        "-o",
        "RequestTTY=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=10",
        "--",
        destination,
        command,
        "runner",
        "connect",
        "--stdio",
    ]);
    ssh.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    ssh
}

fn control(owner: LineOwner, call_id: CallId, attempt_id: AttemptId) -> OperationContext {
    OperationContext::new(CancellationToken::new())
        .with_owner(owner)
        .with_class(OperationClass::EffectCall {
            call_id,
            attempt_id,
        })
}

struct RemoteRoot {
    destination: String,
    command: String,
    root: String,
    link: tokio::process::Child,
    client: RunnerClient,
    selected: Arc<RunnerBackend>,
    environment: plakous_tools::ExecutionEnvironment,
}

async fn remote_root() -> RemoteRoot {
    let destination = required("PLAK_RUNNER_SMOKE_DESTINATION");
    let command = required("PLAK_RUNNER_SMOKE_COMMAND");
    assert!(
        Path::new(&command).is_absolute()
            && command
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._+-".contains(&byte))
    );
    let root = required("PLAK_RUNNER_SMOKE_ROOT");
    if std::env::var("PLAK_RUNNER_SMOKE_ABSENT_ON_HOST").is_ok_and(|value| value == "1") {
        assert!(
            !Path::new(&root).exists(),
            "cross-machine smoke root must be absent on the host"
        );
    }
    let version = VersionTriple::current(
        required("PLAK_RUNNER_SMOKE_REVISION"),
        required("PLAK_RUNNER_SMOKE_FLAVOR"),
    );
    let mut link = ssh(&destination, &command).spawn().unwrap();
    let stream = tokio::io::join(link.stdout.take().unwrap(), link.stdin.take().unwrap());
    let (client, hello) = RunnerClient::connect(
        stream,
        Bind {
            host_id: required("PLAK_RUNNER_SMOKE_HOST_ID"),
            host_instance: uuid::Uuid::new_v4().to_string(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    assert!(hello.machine_id.is_some());
    let options = client.workspace_options().await.unwrap();
    assert!(options.roots.as_ref().is_some_and(|roots| {
        roots
            .iter()
            .any(|admitted| Path::new(&root).starts_with(admitted))
    }));
    let discovery = client.workspace_discover().await.unwrap();
    let registry = RunnerRegistry::default();
    registry.configure(ConfiguredRunner {
        runner_id: hello.runner_id.clone(),
        names: vec!["smoke".into()],
    });
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    registry
        .ready(
            &hello.runner_id,
            client.connection_revision(),
            RunnerServices::new(backend, tokio::runtime::Handle::current()),
            options,
            discovery,
            0,
        )
        .unwrap();
    let bound = registry
        .bind_session_root(&hello.runner_id, &root)
        .await
        .unwrap();
    assert_eq!(bound.root, root);
    let selected = bound.services.unwrap().backend();
    let environment = runner_environment(
        ExecutionEnvironmentScope::Run(RunId::try_from(uuid::Uuid::new_v4()).unwrap()),
        PathBuf::from(&bound.root),
        ".",
        ExecutionEnvironmentConfig::default(),
        selected.clone(),
    )
    .unwrap();
    assert_eq!(
        client
            .call(RunnerCall::unkeyed(
                RunnerVerb::VcsRead,
                serde_json::json!({"op":"path_exists", "path":root})
            ))
            .await
            .unwrap(),
        serde_json::json!({"Ok":true})
    );
    RemoteRoot {
        destination,
        command,
        root,
        link,
        client,
        selected,
        environment,
    }
}

async fn promote_job(sessions: &RunnerSessions, owner: LineOwner, attempt: AttemptId) {
    let job = sessions
        .start(
            plakous_tools::JoinedProcessStartRequest::new(
                plakous_tools::JoinedProcessCommand::literal(
                    "/bin/sh",
                    ["-c".to_owned(), "read line".to_owned()],
                )
                .unwrap(),
                ".",
                Vec::new(),
                true,
                plakous_tools::JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            control(
                owner,
                CallId::try_from(uuid::Uuid::new_v4()).unwrap(),
                attempt,
            ),
        )
        .await
        .unwrap();
    sessions.promote(job).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit strict-SSH endpoint, paired host and disposable admitted root required"]
async fn remote_root_uses_the_pinned_runner_and_keyed_effect_survives_link_rebind() {
    let RemoteRoot {
        destination,
        command,
        root,
        mut link,
        client,
        selected,
        environment,
    } = remote_root().await;
    let owner = LineOwner::new(
        LineId::try_from(uuid::Uuid::new_v4()).unwrap(),
        1,
        BranchId::try_from(uuid::Uuid::new_v4()).unwrap(),
        1,
    );
    let call = CallId::try_from(uuid::Uuid::new_v4()).unwrap();
    let attempt = AttemptId::try_from(uuid::Uuid::new_v4()).unwrap();
    let marker = format!(".runner-smoke-{}", uuid::Uuid::new_v4());
    let sessions = RunnerSessions::new(&selected, &environment, owner);
    promote_job(&sessions, owner, attempt).await;
    let script = format!("printf once >> '{marker}'; pwd");
    let request = || {
        LiteralProcessRequest::new(
            "/bin/sh",
            ["-c".to_owned(), script.clone()],
            ".",
            Vec::new(),
            LiteralProcessIntent::default(),
        )
        .unwrap()
    };
    let answer = environment
        .execute_literal_process(request(), control(owner, call, attempt))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(answer.stdout().to_vec()).unwrap().trim(),
        root
    );
    link.kill().await.unwrap();
    client.closed().await;
    let mut replacement = ssh(&destination, &command).spawn().unwrap();
    let stream = tokio::io::join(
        replacement.stdout.take().unwrap(),
        replacement.stdin.take().unwrap(),
    );
    client.reconnect(stream).await.unwrap();
    client.recover().await;
    let _ = environment
        .execute_literal_process(request(), control(owner, call, attempt))
        .await;
    let read = environment
        .read_workspace(
            WorkspaceReadSliceRequest::start(&marker, 64).unwrap(),
            OperationContext::new(CancellationToken::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        read.bytes().as_ref(),
        b"once",
        "rebind/replay cannot dispatch the effect twice"
    );
    assert_eq!(
        sessions.jobs().await.unwrap()[0].status,
        plakous_tools::JobStatus::Running
    );
    sessions
        .stop_jobs(
            plakous_tools::JobStopReason::User,
            std::time::Duration::ZERO,
        )
        .await
        .unwrap();
    let remove = LiteralProcessRequest::new(
        "/bin/rm",
        [marker],
        ".",
        Vec::new(),
        LiteralProcessIntent::default(),
    )
    .unwrap();
    environment
        .execute_literal_process(
            remove,
            control(
                owner,
                CallId::try_from(uuid::Uuid::new_v4()).unwrap(),
                attempt,
            ),
        )
        .await
        .unwrap();
    client.disconnect();
    replacement.kill().await.unwrap();
}
