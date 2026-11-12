//! The home-local runner (spec 01 §8.4): it binds only its own home's host,
//! from any build of its protocol version; it carries only process sessions;
//! and a process started through it sees what one started directly by the
//! local runner would.

#![allow(clippy::unwrap_used, reason = "integration tests")]

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{ClientWindows, ConnectError, RunnerClient, RunnerIdentity, RunnerService};
use plakous_runner_protocol::{Bind, Refusal, VersionTriple};
use plakous_tools::internal::runner::{
    RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, RunnerOpenFuture, RunnerVerb,
    local_daemon_environment,
};
use plakous_tools::{
    EnvironmentDelegation, EnvironmentVariableName, ExecutionBackendErrorKind,
    ExecutionBackendKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits, ExecutionEnvironmentScope,
    HostProcessOptions, JoinedProcessCommand, JoinedProcessOutputCursor, JoinedProcessSessions,
    JoinedProcessShellProfile, JoinedProcessStartRequest, JoinedProcessTerminalMode, LineOwner,
    OperationClass, OperationContext, RunnerId, WorkspacePolicy,
};
use serde_json::Value;

const HOME_HOST: &str = "00000000-0000-0000-0000-00000000c0de";
const LINE: &str = "00000000-0000-0000-0000-0000000000a1";
const BRANCH: &str = "00000000-0000-0000-0000-0000000000b1";

#[tokio::test]
async fn changes_stay_in_process_and_the_home_local_daemon_refuses_the_verb() {
    use plakous_tools::internal::changes::{
        ChangesAnswer, ChangesRefusal, ChangesRequest, workspace_changes,
    };
    let daemon = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let service = service(daemon.path(), "rev-a", true);
    let client = connect(&service, bind(HOME_HOST, revision("rev-a"), false), true)
        .await
        .unwrap();
    let recorded = Arc::new(Recorded {
        link: client.clone(),
        calls: Mutex::default(),
    });
    let environment = daemon_environment(root.path(), Arc::clone(&recorded) as _);
    let answer = workspace_changes(
        &environment,
        ChangesRequest::status(),
        OperationContext::new(CancellationToken::new()),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        ChangesAnswer::Refused(ChangesRefusal::NotARepository)
    );
    assert!(recorded.calls.lock().unwrap().is_empty());
    assert!(matches!(
        client
            .call(RunnerCall::unkeyed(
                RunnerVerb::WorkspaceChanges,
                serde_json::json!({})
            ))
            .await,
        Err(RunnerFailure::Unavailable(_))
    ));
    // An ordinary client bypasses that transport preflight, so this refusal
    // must come from the daemon's process-only dispatch boundary as well.
    let unfiltered = connect(&service, bind(HOME_HOST, revision("rev-a"), false), false)
        .await
        .unwrap();
    assert!(matches!(
        unfiltered
            .call(RunnerCall::unkeyed(
                RunnerVerb::WorkspaceChanges,
                serde_json::json!({})
            ))
            .await,
        Err(RunnerFailure::Unsupported)
    ));
}

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap())
}

fn config() -> ExecutionEnvironmentConfig {
    ExecutionEnvironmentConfig::new(ExecutionEnvironmentLimits::default(), [], []).unwrap()
}

fn revision(source: &str) -> VersionTriple {
    VersionTriple::current(source, "debug")
}

/// A runner over `root`, home-local to `HOME_HOST` or not, built from
/// `source`.
fn service(root: &Path, source: &str, home_local: bool) -> RunnerService {
    let workspaces = Arc::new(|root: &Path, cwd: &Path| {
        ExecutionEnvironment::host(scope(), root, cwd, config())
    });
    RunnerService::with_options(
        ExecutionEnvironment::host(scope(), root, ".", config()).unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("home-local").unwrap(),
            version: revision(source),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        plakous_runner::RunnerOptions {
            workspaces: Some(workspaces),
            local_host: Some(HOME_HOST.into()),
            home_local: home_local.then(|| HOME_HOST.to_owned()),
            ..plakous_runner::RunnerOptions::default()
        },
    )
    .unwrap()
}

fn bind(host_id: &str, version: VersionTriple, inspect: bool) -> Bind {
    Bind {
        host_id: host_id.into(),
        host_instance: "1-0000000000000001".into(),
        version,
        inspect,
    }
}

fn windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_secs(10),
        effect: Duration::from_secs(10),
        ..ClientWindows::default()
    }
}

/// A host connected to `service` over memory, as a home-local client or not.
async fn connect(
    service: &RunnerService,
    bind: Bind,
    home_local: bool,
) -> Result<RunnerClient, ConnectError> {
    let (host, runner) = tokio::io::duplex(1 << 20);
    let served = service.clone();
    tokio::spawn(async move { served.serve_connection(runner).await });
    let connected = if home_local {
        RunnerClient::connect_home_local(host, bind, windows(), None).await
    } else {
        RunnerClient::connect(host, bind, windows()).await
    };
    connected.map(|(client, _)| client)
}

fn owner() -> LineOwner {
    LineOwner::new(
        LineId::from_str(LINE).unwrap(),
        1,
        BranchId::from_str(BRANCH).unwrap(),
        1,
    )
}

fn effect(call: u8) -> OperationContext {
    OperationContext::new(CancellationToken::new())
        .with_owner(owner())
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str(&format!("00000000-0000-0000-0000-0000000000{call:02x}"))
                .unwrap(),
            attempt_id: AttemptId::from_str("00000000-0000-0000-0000-0000000000c1").unwrap(),
        })
}

/// A workspace whose process context differs from the runner's own: a
/// required `PATH` prefix inside it and a repository discovery ceiling.
fn workspace(root: &Path) -> (WorkspacePolicy, HostProcessOptions) {
    std::fs::create_dir(root.join("tools")).unwrap();
    (
        WorkspacePolicy::host(root, ".").unwrap(),
        HostProcessOptions::new(Some("tools".into()))
            .unwrap()
            .with_discovery_ceiling(true),
    )
}

/// The local runner's environment over `root`, with its processes carried
/// by the home-local runner behind `link`.
fn daemon_environment(root: &Path, link: Arc<dyn RunnerLink>) -> ExecutionEnvironment {
    let (policy, options) = workspace(root);
    local_daemon_environment(
        scope(),
        policy,
        config(),
        options,
        link,
        tokio::runtime::Handle::current(),
    )
    .unwrap()
    .with_line_owner(owner())
}

fn shell(script: &str, terminal: JoinedProcessTerminalMode) -> JoinedProcessStartRequest {
    shell_with_stdin(script, terminal, false)
}

fn shell_with_stdin(
    script: &str,
    terminal: JoinedProcessTerminalMode,
    keep_stdin_open: bool,
) -> JoinedProcessStartRequest {
    let profile = JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()]).unwrap();
    JoinedProcessStartRequest::new(
        JoinedProcessCommand::shell(profile, script).unwrap(),
        ".",
        Vec::new(),
        keep_stdin_open,
        terminal,
    )
    .unwrap()
}

/// Runs `request` to its end in `sessions` and returns what it wrote.
fn run(sessions: &JoinedProcessSessions, request: JoinedProcessStartRequest, call: u8) -> String {
    let id = sessions.start(request, effect(call)).unwrap();
    let mut output = Vec::new();
    let mut cursor = JoinedProcessOutputCursor::new(0);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let polled = sessions.poll_from(id, cursor, 1 << 16, 64).unwrap();
        for record in polled.records() {
            output.extend_from_slice(record.bytes());
        }
        cursor = polled.next_cursor();
        if polled.status().is_terminal() && polled.records().is_empty() {
            return String::from_utf8_lossy(&output).into_owned();
        }
        assert!(Instant::now() < deadline, "the process did not end");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// What a process sees of its context: its environment, cwd, umask and
/// whether its stdin is a terminal.
const CONTEXT: &str =
    "env | LC_ALL=C sort; pwd; umask; if [ -t 0 ]; then echo tty; else echo no-tty; fi";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_through_the_home_local_runner_see_what_direct_ones_do() {
    let daemon_root = tempfile::tempdir().unwrap();
    // One workspace, so the cwd, prefix and ceiling read the same both ways.
    let shared = tempfile::tempdir().unwrap();
    let root = shared.path().join("workspace");
    let service = service(daemon_root.path(), "rev-a", true);
    let client = connect(&service, bind(HOME_HOST, revision("rev-b"), false), true)
        .await
        .unwrap();
    std::fs::create_dir(&root).unwrap();
    let daemon = daemon_environment(&root, Arc::new(client));
    // The descriptor is the local runner's: the daemon is a transport.
    assert_eq!(daemon.backend().kind(), ExecutionBackendKind::Runner);
    assert!(daemon.backend().runner_id().is_some_and(RunnerId::is_local));
    let direct = {
        let (policy, options) = (
            WorkspacePolicy::host(&root, ".").unwrap(),
            HostProcessOptions::new(Some("tools".into()))
                .unwrap()
                .with_discovery_ceiling(true),
        );
        ExecutionEnvironment::host_with_policy_and_process_options(
            scope(),
            policy,
            config(),
            options,
        )
        .unwrap()
    };
    let seen = tokio::task::spawn_blocking(move || {
        let through = daemon.joined_process_sessions().unwrap();
        let local = direct.joined_process_sessions().unwrap();
        let mut seen = Vec::new();
        for (call, terminal) in [
            (1, JoinedProcessTerminalMode::Pipes),
            (2, JoinedProcessTerminalMode::UnixPty),
        ] {
            // Closing a PTY injects an EOF newline whose echo races child exit.
            // Compare process context, not that timing-dependent input echo.
            let carried = run(&through, shell_with_stdin(CONTEXT, terminal, true), call);
            let own = run(&local, shell_with_stdin(CONTEXT, terminal, true), call + 10);
            assert_eq!(carried, own, "{terminal:?}");
            seen.push(carried);
        }
        seen
    })
    .await
    .unwrap();
    // The comparison means something: the context is the host's, not the
    // runner's.
    let prefix = format!(
        "PATH={}",
        root.canonicalize().unwrap().join("tools").display()
    );
    assert!(seen[0].contains(&prefix), "{}", seen[0]);
    assert!(seen[0].contains("GIT_CEILING_DIRECTORIES="), "{}", seen[0]);
    assert!(seen[0].trim_end().ends_with("no-tty"), "{}", seen[0]);
    assert!(seen[1].trim_end().ends_with("tty"), "{}", seen[1]);
}

/// A link that records each call's verb and payload before sending it on.
struct Recorded {
    link: RunnerClient,
    calls: Mutex<Vec<(RunnerVerb, Value)>>,
}

impl RunnerLink for Recorded {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        self.calls
            .lock()
            .unwrap()
            .push((call.verb(), call.payload().clone()));
        self.link.call(call)
    }

    fn open(&self, call: RunnerCall) -> RunnerOpenFuture {
        self.calls
            .lock()
            .unwrap()
            .push((call.verb(), call.payload().clone()));
        self.link.open(call)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_carries_the_hosts_process_context_and_no_value_of_its_environment() {
    let runner_root = tempfile::tempdir().unwrap();
    let workspace_root = tempfile::tempdir().unwrap();
    let service = service(runner_root.path(), "rev-a", true);
    let client = connect(&service, bind(HOME_HOST, revision("rev-a"), false), true)
        .await
        .unwrap();
    let recorded = Arc::new(Recorded {
        link: client,
        calls: Mutex::default(),
    });
    let environment = daemon_environment(workspace_root.path(), Arc::clone(&recorded) as _);
    tokio::task::spawn_blocking(move || {
        let sessions = environment.joined_process_sessions().unwrap();
        assert_eq!(
            run(
                &sessions,
                shell("echo ran", JoinedProcessTerminalMode::Pipes),
                1
            ),
            "ran\n"
        );
    })
    .await
    .unwrap();
    let calls = recorded.calls.lock().unwrap();
    let (_, start) = calls
        .iter()
        .find(|(verb, _)| *verb == RunnerVerb::ProcessStart)
        .unwrap();
    let start = start.to_string();
    let process: Value =
        serde_json::from_str::<Value>(&start).unwrap()["request"]["process"].clone();
    let mut keys: Vec<&str> = process
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["ceiling", "home", "path", "prefix"], "{start}");
    // Every call went to the process sessions: nothing else crosses.
    assert!(
        calls.iter().all(|(verb, _)| verb.family()
            == plakous_tools::internal::runner::RunnerVerbFamily::Sessions),
        "{calls:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_home_local_runner_binds_its_home_across_revisions_of_one_protocol() {
    let root = tempfile::tempdir().unwrap();
    let local = service(root.path(), "rev-a", true);
    // Another revision of the same protocol binds the home-local runner.
    let client = connect(&local, bind(HOME_HOST, revision("rev-b"), false), true)
        .await
        .unwrap();
    assert_eq!(client.lines().await.unwrap(), Vec::new());
    assert_eq!(client.refusal(), None);
    // An ordinary client keeps the exact triple, and so does a runner for
    // paired hosts.
    assert!(matches!(
        connect(&local, bind(HOME_HOST, revision("rev-b"), false), false).await,
        Err(ConnectError::VersionMismatch { .. })
    ));
    let paired = service(root.path(), "rev-a", false);
    let refused = connect(&paired, bind(HOME_HOST, revision("rev-b"), false), true)
        .await
        .unwrap();
    assert!(refused.lines().await.is_err());
    assert!(matches!(
        refused.refusal(),
        Some(Refusal::VersionMismatch { .. })
    ));
    // Another protocol is another home-local runner.
    let mut other = revision("rev-a");
    other.protocol += 1;
    assert!(matches!(
        connect(&local, bind(HOME_HOST, other, false), true).await,
        Err(ConnectError::VersionMismatch { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_home_local_runner_binds_only_its_own_home() {
    let root = tempfile::tempdir().unwrap();
    let local = service(root.path(), "rev-a", true);
    for inspect in [false, true] {
        let stranger = connect(
            &local,
            bind(
                "00000000-0000-0000-0000-00000000beef",
                revision("rev-a"),
                inspect,
            ),
            true,
        )
        .await
        .unwrap();
        assert!(stranger.lines().await.is_err());
        assert!(
            matches!(stranger.refusal(), Some(Refusal::NotPaired { .. })),
            "{:?}",
            stranger.refusal()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_home_local_runner_carries_process_sessions_and_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let local = service(root.path(), "rev-a", true);
    let read = || {
        RunnerCall::unkeyed(
            RunnerVerb::ReadWorkspace,
            serde_json::json!({"root": root.path(), "cwd": root.path()}),
        )
    };
    // The home-local client sends no other call.
    let client = connect(&local, bind(HOME_HOST, revision("rev-a"), false), true)
        .await
        .unwrap();
    assert!(matches!(
        client.call(read()).await,
        Err(RunnerFailure::Unavailable(_))
    ));
    // And the runner answers none, whoever sends it.
    let plain = connect(&local, bind(HOME_HOST, revision("rev-a"), false), false)
        .await
        .unwrap();
    assert!(matches!(
        plain.call(read()).await,
        Err(RunnerFailure::Unsupported)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_and_its_runner_must_agree_on_whose_context_it_runs_in() {
    let runner_root = tempfile::tempdir().unwrap();
    let workspace_root = tempfile::tempdir().unwrap();
    // A runner for paired hosts starts processes in its own environment and
    // refuses a host's context.
    let paired = service(runner_root.path(), "rev-a", false);
    let client = connect(&paired, bind(HOME_HOST, revision("rev-a"), false), true)
        .await
        .unwrap();
    let environment = daemon_environment(workspace_root.path(), Arc::new(client));
    let refused = tokio::task::spawn_blocking(move || {
        environment
            .joined_process_sessions()
            .unwrap()
            .start(
                shell("echo ran", JoinedProcessTerminalMode::Pipes),
                effect(1),
            )
            .map(drop)
            .map_err(|error| error.kind())
    })
    .await
    .unwrap();
    assert_eq!(refused, Err(ExecutionBackendErrorKind::InvalidRequest));
}

#[test]
fn a_delegated_value_never_reaches_the_home_local_runner() {
    struct Nowhere;
    impl RunnerLink for Nowhere {
        fn call(&self, _call: RunnerCall) -> RunnerCallFuture {
            Box::pin(std::future::ready(Err(RunnerFailure::Unavailable(
                "nowhere".into(),
            ))))
        }
    }
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let delegated = ExecutionEnvironmentConfig::new(
        ExecutionEnvironmentLimits::default(),
        [
            EnvironmentDelegation::host_snapshot(EnvironmentVariableName::new("TOKEN").unwrap())
                .unwrap(),
        ],
        [],
    )
    .unwrap();
    let refused = local_daemon_environment(
        scope(),
        WorkspacePolicy::host(root.path(), ".").unwrap(),
        delegated,
        HostProcessOptions::new(None).unwrap(),
        Arc::new(Nowhere),
        runtime.handle().clone(),
    )
    .unwrap_err();
    assert_eq!(
        refused.kind(),
        ExecutionEnvironmentErrorKind::InvalidDelegationSourceAuthority
    );
}
