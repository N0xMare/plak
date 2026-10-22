use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_model::{CancellationToken, RunId};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::*;
use crate::{
    DeterministicExecutionBackend, DeterministicProcessPolicy, DeterministicServiceStep,
    EnvironmentDelegation, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentLimits, ExecutionEnvironmentScope,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn run_id() -> TestResult<RunId> {
    Ok(RunId::from_str("00000000-0000-0000-0000-0000000000d0")?)
}

fn host(workspace: &Path, config: ExecutionEnvironmentConfig) -> TestResult<ExecutionEnvironment> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id()?),
        workspace,
        ".",
        config,
    )?)
}

fn control() -> ExecutionOperationControl {
    ExecutionOperationControl::new(CancellationToken::new())
}

fn name(value: &str) -> TestResult<EnvironmentVariableName> {
    Ok(EnvironmentVariableName::new(value)?)
}

fn request(program: &str, arguments: &[&str]) -> TestResult<ServiceProcessRequest> {
    Ok(ServiceProcessRequest::new(
        program,
        arguments.iter().map(|argument| (*argument).to_owned()),
        [],
        [],
        MAX_SERVICE_STDERR_TAIL_BYTES,
    )?)
}

fn environment_lines(output: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// One variable of this test process that a child must not inherit unasked.
fn parent_variable() -> TestResult<(EnvironmentVariableName, String)> {
    std::env::vars()
        .find(|(key, value)| {
            key != "PATH"
                && key != "HOME"
                && !value.is_empty()
                && EnvironmentVariableName::new(key.as_str()).is_ok()
        })
        .map_or_else(
            || Err("test process has no portable environment variable".into()),
            |(key, value)| Ok((EnvironmentVariableName::new(key)?, value)),
        )
}

async fn run_env(
    environment: &ExecutionEnvironment,
    secret: Option<&str>,
) -> TestResult<BTreeMap<String, String>> {
    let secret_environment = secret
        .map(|value| -> TestResult<_> {
            Ok((
                name("SERVICE_SECRET")?,
                ServiceSecretValue::new(value.to_owned())?,
            ))
        })
        .transpose()?;
    let service = environment
        .spawn_service(
            ServiceProcessRequest::new(
                "env",
                [],
                [(name("SERVICE_DECLARED")?, String::from("declared-value"))],
                secret_environment,
                MAX_SERVICE_STDERR_TAIL_BYTES,
            )?,
            control(),
        )
        .await?;
    let ServiceProcess {
        stdin,
        mut stdout,
        handle,
        ..
    } = service;
    drop(stdin);
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await?;
    handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
    Ok(environment_lines(&output))
}

#[tokio::test]
async fn host_service_echoes_stdio_in_the_workspace_root() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let environment = host(workspace.path(), ExecutionEnvironmentConfig::default())?;
    let service = environment
        .spawn_service(request("sh", &["-c", "pwd; exec cat"])?, control())
        .await?;
    let ServiceProcess {
        mut stdin,
        stdout,
        handle,
        ..
    } = service;
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    stdout.read_line(&mut line).await?;
    assert_eq!(
        Path::new(line.trim_end()),
        std::fs::canonicalize(workspace.path())?
    );
    stdin.write_all(b"ping\n").await?;
    stdin.flush().await?;
    line.clear();
    stdout.read_line(&mut line).await?;
    assert_eq!(line, "ping\n");
    handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
    Ok(())
}

#[tokio::test]
async fn host_service_environment_is_curated() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let (parent_name, parent_value) = parent_variable()?;

    let curated = host(workspace.path(), ExecutionEnvironmentConfig::default())?;
    let seen = run_env(&curated, Some("secret-value")).await?;
    let mut names = seen.keys().map(String::as_str).collect::<Vec<_>>();
    names.retain(|name| *name != "HOME");
    assert_eq!(names, ["PATH", "SERVICE_DECLARED", "SERVICE_SECRET"]);
    assert_eq!(seen["SERVICE_DECLARED"], "declared-value");
    assert_eq!(seen["SERVICE_SECRET"], "secret-value");
    assert!(!seen.contains_key(parent_name.as_str()));

    let without_secret = run_env(&curated, None).await?;
    assert!(!without_secret.contains_key("SERVICE_SECRET"));

    let delegated = host(
        workspace.path(),
        ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::default(),
            [EnvironmentDelegation::host_snapshot(parent_name.clone())?],
            [],
        )?,
    )?;
    let seen = run_env(&delegated, None).await?;
    assert_eq!(
        seen.get(parent_name.as_str()).map(String::as_str),
        Some(parent_value.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn host_service_terminate_joins_a_grandchild() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let environment = host(workspace.path(), ExecutionEnvironmentConfig::default())?;
    let service = environment
        .spawn_service(
            request("sh", &["-c", "sleep 60 & echo $!; wait"])?,
            control(),
        )
        .await?;
    let ServiceProcess { stdout, handle, .. } = service;
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    stdout.read_line(&mut line).await?;
    let grandchild =
        rustix::process::Pid::from_raw(line.trim().parse()?).ok_or("grandchild pid is invalid")?;
    assert!(rustix::process::test_kill_process(grandchild).is_ok());
    handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
    assert!(rustix::process::test_kill_process(grandchild).is_err());
    Ok(())
}

#[tokio::test]
async fn host_service_ignoring_sigterm_is_killed_after_the_grace() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let environment = host(workspace.path(), ExecutionEnvironmentConfig::default())?;
    let service = environment
        .spawn_service(
            request(
                "sh",
                &["-c", "trap '' TERM; echo ready; while :; do sleep 1; done"],
            )?,
            control(),
        )
        .await?;
    let ServiceProcess { stdout, handle, .. } = service;
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    stdout.read_line(&mut line).await?;
    assert_eq!(line, "ready\n");
    let started = Instant::now();
    handle.terminate(Duration::from_millis(100)).await?;
    assert!(started.elapsed() < Duration::from_secs(5));
    Ok(())
}

#[tokio::test]
async fn host_service_stderr_tail_is_bounded_and_sanitized() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let environment = host(workspace.path(), ExecutionEnvironmentConfig::default())?;
    let script = "head -c 20000 /dev/zero | tr '\\000' a >&2; printf '\\033end' >&2; exec cat";
    let service = environment
        .spawn_service(
            ServiceProcessRequest::new(
                "sh",
                [String::from("-c"), script.to_owned()],
                [],
                [],
                1024,
            )?,
            control(),
        )
        .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let tail = loop {
        let tail = service.handle.stderr_tail();
        if tail.ends_with("end") {
            break tail;
        }
        assert!(Instant::now() < deadline, "stderr tail never completed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(tail.chars().count(), 1024);
    assert!(tail.ends_with("a\u{fffd}end"), "{tail:?}");
    assert!(tail.chars().take(1020).all(|ch| ch == 'a'));
    service
        .handle
        .terminate(SERVICE_PROCESS_TERMINATE_GRACE)
        .await?;
    Ok(())
}

#[tokio::test]
async fn host_service_startup_cancellation_leaves_no_process() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let environment = host(workspace.path(), ExecutionEnvironmentConfig::default())?;
    let marker = workspace.path().join("started");
    let script = "touch started; sleep 60";

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = environment
        .spawn_service(
            request("sh", &["-c", script])?,
            ExecutionOperationControl::new(cancelled),
        )
        .await
        .err()
        .ok_or("cancelled startup returned a service")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Cancelled);
    assert!(error.proven_no_effect());

    let token = CancellationToken::new();
    let startup = environment.spawn_service(
        request("sh", &["-c", script])?,
        ExecutionOperationControl::new(token.clone()),
    );
    token.cancel();
    let error = startup
        .await
        .err()
        .ok_or("cancelled startup returned a service")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Cancelled);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn deterministic_service_round_trips_through_its_duplex_peer() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let (step, mut peer) = DeterministicServiceStep::duplex(b"boot\x07\n".to_vec());
    let backend = Arc::new(
        DeterministicExecutionBackend::new([], DeterministicProcessPolicy::default(), [])?
            .with_service_steps([step]),
    );
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id()?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        Arc::clone(&backend),
    )?;
    let service = environment
        .spawn_service(
            ServiceProcessRequest::new(
                "server",
                [String::from("--stdio")],
                [(name("SERVICE_DECLARED")?, String::from("declared-value"))],
                [(
                    name("SERVICE_SECRET")?,
                    ServiceSecretValue::new(String::from("secret-value"))?,
                )],
                MAX_SERVICE_STDERR_TAIL_BYTES,
            )?,
            control(),
        )
        .await?;
    assert_eq!(backend.status().active_services(), 1);
    assert!(!format!("{service:?}").contains("secret-value"));
    assert_eq!(service.handle.stderr_tail(), "boot\u{fffd}\n");
    let ServiceProcess {
        mut stdin,
        mut stdout,
        handle,
        ..
    } = service;

    stdin.write_all(b"request").await?;
    let mut received = [0_u8; 7];
    peer.read_exact(&mut received).await?;
    assert_eq!(&received, b"request");
    peer.write_all(b"response").await?;
    let mut answered = [0_u8; 8];
    stdout.read_exact(&mut answered).await?;
    assert_eq!(&answered, b"response");

    let calls = backend.service_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program(), "server");
    assert_eq!(calls[0].arguments(), ["--stdio"]);
    assert_eq!(calls[0].environment_names(), [name("SERVICE_DECLARED")?]);
    assert_eq!(
        calls[0].secret_environment_names(),
        [name("SERVICE_SECRET")?]
    );
    assert!(!format!("{calls:?}").contains("secret-value"));

    handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
    assert!(backend.status().is_drained());

    let error = environment
        .spawn_service(request("server", &[])?, control())
        .await
        .err()
        .ok_or("fake without a step returned a service")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Unsupported);
    assert!(backend.status().is_drained());
    Ok(())
}

#[test]
fn service_request_refuses_invalid_parts() -> TestResult {
    let secret = || ServiceSecretValue::new(String::from("value"));
    let cases: Vec<(&str, Result<ServiceProcessRequest, ExecutionBackendError>)> = vec![
        (
            "empty program",
            ServiceProcessRequest::new("", [], [], [], 1),
        ),
        (
            "relative program path",
            ServiceProcessRequest::new("bin/server", [], [], [], 1),
        ),
        (
            "NUL argument",
            ServiceProcessRequest::new("server", [String::from("a\0b")], [], [], 1),
        ),
        (
            "too many arguments",
            ServiceProcessRequest::new(
                "server",
                (0..=MAX_LITERAL_PROCESS_ARGUMENTS).map(|_| String::new()),
                [],
                [],
                1,
            ),
        ),
        (
            "declared and secret share a name",
            ServiceProcessRequest::new(
                "server",
                [],
                [(name("SHARED")?, String::from("value"))],
                [(name("SHARED")?, secret()?)],
                1,
            ),
        ),
        (
            "NUL declared value",
            ServiceProcessRequest::new("server", [], [(name("A")?, String::from("\0"))], [], 1),
        ),
        (
            "zero stderr tail",
            ServiceProcessRequest::new("server", [], [], [], 0),
        ),
        (
            "oversized stderr tail",
            ServiceProcessRequest::new("server", [], [], [], MAX_SERVICE_STDERR_TAIL_BYTES + 1),
        ),
    ];
    for (label, result) in cases {
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::InvalidRequest),
            "{label}"
        );
    }
    assert!(ServiceSecretValue::new(String::new()).is_err());
    assert!(ServiceSecretValue::new(String::from("a\0")).is_err());
    assert_eq!(format!("{:?}", secret()?), "ServiceSecretValue(<redacted>)");
    assert!(ServiceProcessRequest::new("/bin/sh", [], [], [], 1).is_ok());
    Ok(())
}

#[test]
fn stderr_tail_keeps_only_the_newest_bytes() {
    let tail = ServiceStderrTail::new(4);
    tail.push(b"ab");
    tail.push(b"cdef");
    assert_eq!(tail.sanitized(), "cdef");
    tail.push(b"0123456789");
    assert_eq!(tail.sanitized(), "6789");
    tail.push(b"\r");
    assert_eq!(tail.sanitized(), "789\u{fffd}");
}

#[tokio::test]
async fn services_enforce_discovery_ceiling_after_declared_and_secret_values() -> TestResult {
    let root = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host_with_policy_and_process_options(
        ExecutionEnvironmentScope::Run(run_id()?),
        crate::WorkspacePolicy::host(root.path(), ".")?,
        ExecutionEnvironmentConfig::default(),
        crate::HostProcessOptions::default().with_discovery_ceiling(true),
    )?;
    for secret in [false, true] {
        let declared = (!secret)
            .then(|| {
                Ok::<_, Box<dyn std::error::Error>>((
                    name("GIT_CEILING_DIRECTORIES")?,
                    "/wrong".to_owned(),
                ))
            })
            .transpose()?;
        let sensitive = secret
            .then(|| {
                Ok::<_, Box<dyn std::error::Error>>((
                    name("GIT_CEILING_DIRECTORIES")?,
                    ServiceSecretValue::new("/wrong".to_owned())?,
                ))
            })
            .transpose()?;
        let service = environment
            .spawn_service(
                ServiceProcessRequest::new(
                    "env",
                    [],
                    declared,
                    sensitive,
                    MAX_SERVICE_STDERR_TAIL_BYTES,
                )?,
                control(),
            )
            .await?;
        let ServiceProcess {
            stdin,
            mut stdout,
            handle,
            ..
        } = service;
        drop(stdin);
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        handle.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
        assert_eq!(
            environment_lines(&output)
                .get("GIT_CEILING_DIRECTORIES")
                .map(String::as_str),
            environment.root().parent().and_then(Path::to_str)
        );
    }
    Ok(())
}
