//! Disposable-workspace conformance for execution backend authority.

#![cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        unused_imports,
        reason = "Linux-only PTY and namespace host tests are cfg-gated"
    )
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_model::{CancellationToken, RunId};
use plakous_tools::{
    CredentialDelegation, CredentialSlotId, DelegatedAuthorityId, DelegationSourceAuthorityId,
    DelegationSourceRevision, DeterministicExecutionBackend, DeterministicProcessPolicy,
    DeterministicProcessStep, EnvironmentDelegation, EnvironmentVariableName,
    ExecutionBackendErrorKind, ExecutionBackendKind, ExecutionEnvironment,
    ExecutionEnvironmentConfig, ExecutionEnvironmentLimits, ExecutionEnvironmentScope,
    ExecutionOperationControl, FilesystemContainment, JoinedProcessCommand,
    JoinedProcessInputDisposition, JoinedProcessShellProfile, JoinedProcessStartRequest,
    JoinedProcessStatus, JoinedProcessTerminalMode, LiteralProcessIntent, LiteralProcessRequest,
    LiteralProcessResult, NetworkContainment, ProcessContainment, RepositoryMatchCase,
    RepositoryQuery, RepositoryQueryLimits, WorkspaceContentIdentity, WorkspaceExpectedFileState,
    WorkspaceMutation, WorkspaceMutationDisposition, WorkspaceMutationTransaction,
    WorkspaceReadSliceRequest, WorkspaceReplacement,
};

use plakous_tools::JoinedProcessOutputCursor;
#[cfg(target_os = "linux")]
use plakous_tools::JoinedProcessOutputStream;

type TestError = Box<dyn std::error::Error>;

fn run_id(value: u8) -> Result<RunId, TestError> {
    Ok(RunId::from_str(&format!(
        "00000000-0000-0000-0000-{value:012x}"
    ))?)
}

fn source(value: &str) -> Result<DelegationSourceAuthorityId, TestError> {
    Ok(DelegationSourceAuthorityId::new(value)?)
}

fn environment_binding(name: &str, revision: u64) -> Result<EnvironmentDelegation, TestError> {
    Ok(EnvironmentDelegation::new(
        EnvironmentVariableName::new(name)?,
        source("source.environment.backend-test")?,
        DelegationSourceRevision::new(revision)?,
    ))
}

fn credential_binding() -> Result<CredentialDelegation, TestError> {
    Ok(CredentialDelegation::new(
        CredentialSlotId::new("credential.backend-test/default")?,
        source("source.credential.backend-test")?,
        DelegationSourceRevision::new(1)?,
        DelegatedAuthorityId::new("authority.backend-test.secret")?,
    ))
}

fn control() -> ExecutionOperationControl {
    ExecutionOperationControl::new(CancellationToken::new())
}

fn limits(
    stdout: u64,
    stderr: u64,
    combined: u64,
    artifact: u64,
) -> Result<ExecutionEnvironmentLimits, TestError> {
    Ok(ExecutionEnvironmentLimits::new(
        stdout,
        stderr,
        combined,
        artifact,
        Duration::from_secs(30),
        Duration::from_secs(2),
    )?)
}

fn unix_program(candidates: &'static [&'static str]) -> Result<&'static str, TestError> {
    candidates
        .iter()
        .copied()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or_else(|| "required Unix test program is unavailable".into())
}

/// How long a wait for an eventual child-process event may take. The waits
/// return as soon as the event happens; the deadline only bounds a hang, so it
/// is sized for a loaded gate host rather than an idle one, while staying well
/// below the 30 second lifetime of the fixtures whose exit it observes.
const EVENT_DEADLINE: Duration = Duration::from_secs(10);

fn before_event_deadline(started: Instant) -> bool {
    started.elapsed() < EVENT_DEADLINE
}

async fn wait_for_file(path: &Path) -> Result<(), TestError> {
    let started = Instant::now();
    while before_event_deadline(started) {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("timed out waiting for child process marker".into())
}

async fn wait_for_pid_file(path: &Path) -> Result<rustix::process::Pid, TestError> {
    let started = Instant::now();
    while before_event_deadline(started) {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(raw_pid) = contents.trim().parse::<i32>()
            && let Some(pid) = rustix::process::Pid::from_raw(raw_pid)
        {
            return Ok(pid);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("timed out waiting for an atomically published child process identity".into())
}

async fn wait_for_process_exit(pid: rustix::process::Pid) -> bool {
    let started = Instant::now();
    while before_event_deadline(started) {
        if rustix::process::test_kill_process(pid).is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

async fn collect_joined_output(
    sessions: &plakous_tools::JoinedProcessSessions,
    id: plakous_tools::JoinedProcessSessionId,
) -> Result<(Vec<u8>, JoinedProcessStatus), TestError> {
    let mut output = Vec::new();
    let started = Instant::now();
    while before_event_deadline(started) {
        let polled = sessions.poll(id, 8 * 1024, 256)?;
        for record in polled.records() {
            output.extend_from_slice(record.bytes());
        }
        if polled.status().is_terminal() && !polled.more_output() {
            return Ok((output, polled.status()));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("timed out collecting joined process output".into())
}

fn joined_request(
    command: JoinedProcessCommand,
    keep_stdin_open: bool,
) -> Result<JoinedProcessStartRequest, TestError> {
    Ok(JoinedProcessStartRequest::new(
        command,
        ".",
        [],
        keep_stdin_open,
        JoinedProcessTerminalMode::Pipes,
    )?)
}

struct DetachedProcessGuard {
    pid: Option<rustix::process::Pid>,
    stop_path: PathBuf,
}

impl DetachedProcessGuard {
    fn new(pid: rustix::process::Pid, stop_path: PathBuf) -> Self {
        Self {
            pid: Some(pid),
            stop_path,
        }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for DetachedProcessGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        let _ = fs::write(&self.stop_path, b"");
        for _ in 0..100 {
            if rustix::process::test_kill_process(pid).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::TERM);
        for _ in 0..100 {
            if rustix::process::test_kill_process(pid).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

/// Each way a path can fail says which one it was.
///
/// All of these used to be one error: `SymlinkEscape` carrying "workspace
/// descriptor resolution refused the path", because the open mapped
/// `Err(_) =>` and threw the `Errno` away. A missing file was reported as a
/// symlink escape, and a model that misspelled a filename was told nothing it
/// could act on.
///
/// Every one of them is decided by looking, before anything is opened for
/// writing, so every one is proven to have had no effect.
#[tokio::test]
async fn each_workspace_path_refusal_names_its_own_cause() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("data.txt"), b"abcdef")?;
    fs::create_dir(workspace.path().join("a-directory"))?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(61)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 128, 8)?, [], [])?,
    )?;

    let read = async |path: &str| -> Result<plakous_tools::ExecutionBackendError, TestError> {
        environment
            .read_workspace(WorkspaceReadSliceRequest::start(path, 8)?, control())
            .await
            .err()
            .ok_or_else(|| TestError::from("read admitted a refused path"))
    };

    let missing = read("no-such-file.txt").await?;
    assert_eq!(missing.kind(), ExecutionBackendErrorKind::NotFound);
    assert!(
        missing.message().as_str().contains("no-such-file.txt"),
        "the cause names the path: {:?}",
        missing.message()
    );
    assert!(missing.proven_no_effect());

    let directory = read("a-directory").await?;
    assert_eq!(directory.kind(), ExecutionBackendErrorKind::IsADirectory);
    assert!(directory.message().as_str().contains("a-directory"));
    assert!(directory.proven_no_effect());

    // A parent that is not a directory cannot hold the child, and saying which
    // is more useful than "not found" alone.
    let through_file = read("data.txt/child").await?;
    assert_eq!(through_file.kind(), ExecutionBackendErrorKind::NotFound);
    assert!(through_file.proven_no_effect());

    {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("secret"), b"not-readable")?;
        symlink(outside.path().join("secret"), workspace.path().join("link"))?;
        let escaped = read("link").await?;
        assert_eq!(
            escaped.kind(),
            ExecutionBackendErrorKind::SymlinkEscape,
            "a symbolic link is the one case that stays a symlink escape"
        );
        assert!(escaped.proven_no_effect());
    }

    {
        // A socket is present and is not a directory; it is simply not a file
        // anyone can read bytes out of, and the error says that much.
        let _listener = std::os::unix::net::UnixListener::bind(workspace.path().join("socket"))?;
        let socket = read("socket").await?;
        assert_eq!(
            socket.kind(),
            ExecutionBackendErrorKind::NotARegularFile,
            "a socket is neither missing nor a directory"
        );
        assert!(socket.message().as_str().contains("socket"));
        assert!(socket.proven_no_effect());
    }

    // A path that names its way out never reaches the backend: the request
    // refuses it, and says why.
    assert_eq!(
        WorkspaceReadSliceRequest::start("../outside", 8)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::OutsideWorkspace)
    );
    Ok(())
}

#[tokio::test]
async fn host_backend_preserves_constructor_and_is_truthful() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("data.txt"), b"abcdef")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(1)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(8, 8, 12, 3)?, [], [])?,
    )?;

    assert_eq!(environment.backend().kind(), ExecutionBackendKind::Host);
    let containment = environment.backend().containment();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            containment.filesystem(),
            FilesystemContainment::LinuxDescriptorRelativeNoFollow
        );
        assert_eq!(
            containment.process(),
            ProcessContainment::InheritsHostAuthority
        );
        assert_eq!(
            containment.network(),
            NetworkContainment::InheritsHostAuthority
        );
    }
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        containment.filesystem(),
        FilesystemContainment::HostPathPolicyOnly
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        containment.process(),
        ProcessContainment::InheritsHostAuthority
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        containment.network(),
        NetworkContainment::InheritsHostAuthority
    );

    let first = environment
        .read_workspace(
            WorkspaceReadSliceRequest::start("./data.txt", 3)?,
            control(),
        )
        .await?;
    assert_eq!(first.offset(), 0);
    assert_eq!(first.bytes().as_ref(), b"abc");
    assert_eq!(first.content().byte_len(), 6);
    assert_eq!(
        first.content(),
        WorkspaceContentIdentity::from_bytes(b"abcdef")?
    );
    let cursor = first.next().cloned().ok_or("missing read cursor")?;
    assert_eq!(cursor.path(), Path::new("data.txt"));
    assert_eq!(cursor.offset(), 3);
    assert_eq!(cursor.content(), first.content());
    let second = environment
        .read_workspace(WorkspaceReadSliceRequest::resume(cursor, 3)?, control())
        .await?;
    assert_eq!(second.offset(), 3);
    assert_eq!(second.bytes().as_ref(), b"def");
    assert_eq!(second.content(), first.content());
    assert!(second.next().is_none());

    let process = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                "/bin/echo",
                [String::from("literal")],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await;
    {
        let process = process?;
        assert_eq!(process.exit_code(), Some(0));
        assert_eq!(process.stdout().as_ref(), b"literal\n");
        assert!(process.stderr().is_empty());
    }
    Ok(())
}

#[test]
fn joined_process_session_construction_is_bounded_and_fake_backend_fails_closed()
-> Result<(), TestError> {
    assert_eq!(
        JoinedProcessCommand::literal("relative-program", [])
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );
    assert_eq!(
        JoinedProcessShellProfile::new("sh", [String::from("-c")])
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );
    let workspace = tempfile::tempdir()?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(40)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend,
    )?;
    assert_eq!(
        environment
            .joined_process_sessions()
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::Unsupported)
    );
    Ok(())
}

#[test]
fn execution_environment_clones_share_one_joined_process_service() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(39)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let first = environment.joined_process_sessions()?;
    let second = environment.clone().joined_process_sessions()?;
    let printf = unix_program(&["/usr/bin/printf", "/bin/printf"])?;
    let id = first.start(
        joined_request(
            JoinedProcessCommand::literal(printf, [String::from("shared")])?,
            false,
        )?,
        control(),
    )?;
    let mut output = Vec::new();
    loop {
        let poll = second.poll(id, 4096, 16)?;
        for record in poll.records() {
            output.extend_from_slice(record.bytes());
        }
        if poll.status().is_terminal() && !poll.more_output() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(output, b"shared");
    assert_eq!(second.terminate(id)?, JoinedProcessStatus::Exited(Some(0)));
    assert!(first.is_drained());
    Ok(())
}

#[tokio::test]
async fn joined_process_shell_multicall_dispatch_preserves_enrolled_name() -> Result<(), TestError>
{
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir()?;
    let aliases = tempfile::tempdir()?;
    let multicall = Path::new("/bin/bash").canonicalize()?;
    let enrolled = aliases.path().join("sh");
    // Bash, like BusyBox, dispatches on the actual exec argv[0]: "sh" enables
    // POSIX mode. A shebang dispatcher would instead see the script path as $0
    // regardless of arg0, and therefore cannot test this boundary.
    let command = "case :$SHELLOPTS: in *:posix:*) printf '%s' \"$0\"; exit 7;; *) exit 127;; esac";
    symlink(&multicall, &enrolled)?;
    let wrong_applet = std::process::Command::new(&multicall)
        .args(["-c", command])
        .output()?;
    assert_eq!(wrong_applet.status.code(), Some(127));
    assert_eq!(wrong_applet.stdout, [] as [u8; 0]);
    let profile = JoinedProcessShellProfile::new(&enrolled, [String::from("-c")])?;
    assert_eq!(
        profile.identity().canonical_executable(),
        multicall.canonicalize()?
    );
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(40)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    for mode in [
        JoinedProcessTerminalMode::Pipes,
        JoinedProcessTerminalMode::UnixPty,
    ] {
        let id = sessions.start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::shell(profile.clone(), command)?,
                ".",
                [],
                true,
                mode,
            )?,
            control(),
        )?;
        let (output, status) = collect_joined_output(&sessions, id).await?;
        assert_eq!(
            output,
            enrolled
                .to_str()
                .ok_or("alias path is not UTF-8")?
                .as_bytes()
        );
        assert_eq!(status, JoinedProcessStatus::Exited(Some(7)));
        sessions.terminate(id)?;
    }

    // Retargeting the enrolled alias must still refuse before any execution.
    let replacement = workspace.path().join("replacement");
    fs::copy(&multicall, &replacement)?;
    fs::remove_file(&enrolled)?;
    symlink(&replacement, &enrolled)?;
    for mode in [
        JoinedProcessTerminalMode::Pipes,
        JoinedProcessTerminalMode::UnixPty,
    ] {
        let error = sessions
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::shell(profile.clone(), "printf unsafe > spawned")?,
                    ".",
                    [],
                    false,
                    mode,
                )?,
                control(),
            )
            .err()
            .ok_or("retargeted shell unexpectedly started")?;
        assert_eq!(error.kind(), ExecutionBackendErrorKind::InvalidRequest);
        assert!(!workspace.path().join("spawned").exists());
    }
    assert!(sessions.is_drained());
    Ok(())
}

#[test]
fn joined_process_shell_replacement_fails_before_spawn() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let source = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let enrolled = workspace.path().join("enrolled-sh");
    let replacement = workspace.path().join("replacement-sh");
    fs::copy(source, &enrolled)?;
    fs::copy(source, &replacement)?;
    let profile = JoinedProcessShellProfile::new(&enrolled, [String::from("-c")])?;
    fs::rename(&replacement, &enrolled)?;

    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(40)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let error = sessions
        .start(
            joined_request(JoinedProcessCommand::shell(profile, "exit 0")?, false)?,
            control(),
        )
        .err()
        .ok_or("replaced shell unexpectedly started")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::InvalidRequest);
    assert!(sessions.is_drained());
    Ok(())
}

#[tokio::test]
async fn joined_process_noninteractive_stdin_is_null_and_initial_input_is_delivered()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(43)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let shell = JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?;
    for command in ["cat", "test -c /dev/stdin"] {
        let id = sessions.start(
            joined_request(JoinedProcessCommand::shell(shell.clone(), command)?, false)?,
            control(),
        )?;
        let (output, status) = collect_joined_output(&sessions, id).await?;
        assert_eq!(status, JoinedProcessStatus::Exited(Some(0)), "{command}");
        assert_eq!(output, [] as [u8; 0]);
    }
    let id = sessions.start(
        joined_request(
            JoinedProcessCommand::shell(shell.clone(), "read x || echo eof")?,
            false,
        )?,
        control(),
    )?;
    let (output, status) = collect_joined_output(&sessions, id).await?;
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    assert_eq!(output, b"eof\n");
    let id = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(shell, "cat")?,
            ".",
            b"initial input".to_vec(),
            false,
            JoinedProcessTerminalMode::Pipes,
        )?,
        control(),
    )?;
    let (output, status) = collect_joined_output(&sessions, id).await?;
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    assert_eq!(output, b"initial input");
    Ok(())
}

#[tokio::test]
async fn joined_process_sessions_preserve_literal_and_explicit_shell_semantics()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let path_binding = EnvironmentDelegation::host_snapshot(EnvironmentVariableName::new("PATH")?)?;
    let expected_path = std::env::var("PATH")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(41)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [path_binding], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let printf = unix_program(&["/usr/bin/printf", "/bin/printf"])?;
    let literal = sessions.start(
        joined_request(
            JoinedProcessCommand::literal(printf, [String::from("$HOME;echo nope")])?,
            false,
        )?,
        control(),
    )?;
    let (literal_output, status) = collect_joined_output(&sessions, literal).await?;
    assert_eq!(literal_output, b"$HOME;echo nope");
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    sessions.terminate(literal)?;

    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let profile = JoinedProcessShellProfile::new(shell, [String::from("-c")])?;
    let shell_session = sessions.start(
        joined_request(
            JoinedProcessCommand::shell(profile, "printf '%s|%s' shell-interpreted \"$PATH\"")?,
            false,
        )?,
        control(),
    )?;
    let (shell_output, status) = collect_joined_output(&sessions, shell_session).await?;
    assert_eq!(
        shell_output,
        format!("shell-interpreted|{expected_path}").as_bytes()
    );
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    sessions.terminate(shell_session)?;
    sessions.join_all()?;
    assert!(sessions.is_drained());
    Ok(())
}

#[tokio::test]
async fn joined_process_sessions_write_poll_terminate_and_diagnose_stale_foreign_ids()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(42)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let cat = unix_program(&["/bin/cat", "/usr/bin/cat"])?;
    let id = sessions.start(
        joined_request(JoinedProcessCommand::literal(cat, [])?, true)?,
        control(),
    )?;
    sessions.write_stdin(id, b"joined-input", JoinedProcessInputDisposition::Close)?;
    let (output, status) = collect_joined_output(&sessions, id).await?;
    assert_eq!(output, b"joined-input");
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    sessions.terminate(id)?;
    assert_eq!(
        sessions.poll(id, 128, 4).err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::Conflict)
    );

    let foreign_environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(43)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let foreign_sessions = foreign_environment.joined_process_sessions()?;
    assert_eq!(
        foreign_sessions
            .poll(id, 128, 4)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::IdentityMismatch)
    );
    sessions.join_all()?;
    foreign_sessions.join_all()?;
    assert!(sessions.is_drained());
    assert!(foreign_sessions.is_drained());
    Ok(())
}

/// A finished session stays in the live set until it is explicitly retired.
///
/// This is the retention that makes polling a finished session work, and it is
/// the fact the turn-boundary cleanup was misreading: a child that has exited
/// and been joined still answers `active_session_ids`, while its durable record
/// has already left `Dispatched`. The driver compared the two sets for equality
/// and fenced the session over the product's own bookkeeping.
#[tokio::test]
async fn a_finished_session_stays_live_until_it_is_retired() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(52)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let cat = unix_program(&["/bin/cat", "/usr/bin/cat"])?;
    let id = sessions.start(
        joined_request(JoinedProcessCommand::literal(cat, [])?, true)?,
        control(),
    )?;
    sessions.write_stdin(id, b"done", JoinedProcessInputDisposition::Close)?;
    let (output, status) = collect_joined_output(&sessions, id).await?;
    assert_eq!(output, b"done");
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));

    // The child is gone and reaped, and the session is still live. Cleanup sees
    // exactly this: a live identity with no dispatched durable record left.
    assert!(
        sessions.active_session_ids().contains(&id),
        "a finished session is retained so its output stays drainable"
    );
    assert!(!sessions.is_drained());
    assert_eq!(
        sessions.joined_terminal_status(id),
        Some(JoinedProcessStatus::Exited(Some(0))),
        "the outcome is already definite, which is why cleanup has nothing to resolve"
    );

    // Cleanup retires it, and only then is the backend drained.
    sessions.join_all()?;
    assert_eq!(
        sessions.active_session_ids(),
        [] as [plakous_tools::JoinedProcessSessionId; 0]
    );
    assert!(sessions.is_drained());
    Ok(())
}

/// A working directory that is not there says so, and nothing ran.
///
/// The message always read "choose an existing directory" while the kind said
/// policy, so a consumer branching on the kind saw a refusal where a user had
/// made a typo. It is the same defect the read path had, on the process path.
#[tokio::test]
async fn a_missing_process_working_directory_is_not_found_and_nothing_started()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("a-file"), b"contents")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(0x6d)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let start = |cwd: &str| -> Result<JoinedProcessStartRequest, TestError> {
        Ok(JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                shell,
                [String::from("-c"), String::from("printf ran > marker")],
            )?,
            cwd,
            [],
            false,
            JoinedProcessTerminalMode::Pipes,
        )?)
    };

    let missing = sessions
        .start(start("missing/workdir")?, control())
        .err()
        .ok_or_else(|| TestError::from("a missing working directory started a process"))?;
    assert_eq!(missing.kind(), ExecutionBackendErrorKind::NotFound);
    assert!(missing.message().as_str().contains("missing"));
    assert!(missing.proven_no_effect());

    // A parent that is a file cannot hold the directory, and that is why the
    // path does not exist rather than a separate kind of refusal.
    let through_file = sessions
        .start(start("a-file/workdir")?, control())
        .err()
        .ok_or_else(|| TestError::from("a working directory under a file started a process"))?;
    assert_eq!(through_file.kind(), ExecutionBackendErrorKind::NotFound);
    assert!(through_file.proven_no_effect());

    assert!(!workspace.path().join("marker").exists());
    Ok(())
}

#[tokio::test]
async fn joined_process_sessions_join_cancelled_group() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let pid_path = workspace.path().join("child.pid");
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(44)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let command = format!("echo $$ > '{}'; sleep 30", pid_path.display());
    let id = sessions.start(
        joined_request(
            JoinedProcessCommand::literal(shell, [String::from("-c"), command])?,
            false,
        )?,
        control(),
    )?;
    let pid = wait_for_pid_file(&pid_path).await?;
    let status = sessions.terminate(id)?;
    assert_eq!(status, JoinedProcessStatus::Terminated);
    assert!(wait_for_process_exit(pid).await);

    let cancellation = CancellationToken::new();
    let cancelled_marker = workspace.path().join("cancelled.pid");
    let cancelled_command = format!("echo $$ > '{}'; sleep 30", cancelled_marker.display());
    let cancelled_id = sessions.start(
        joined_request(
            JoinedProcessCommand::literal(shell, [String::from("-c"), cancelled_command])?,
            false,
        )?,
        ExecutionOperationControl::new(cancellation.clone()),
    )?;
    let cancelled_process = wait_for_pid_file(&cancelled_marker).await?;
    cancellation.cancel();
    let (_, cancelled_status) = collect_joined_output(&sessions, cancelled_id).await?;
    assert_eq!(cancelled_status, JoinedProcessStatus::Cancelled);
    assert!(wait_for_process_exit(cancelled_process).await);
    sessions.terminate(cancelled_id)?;
    sessions.join_all()?;
    assert!(sessions.is_drained());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn joined_process_pty_supports_initial_size_input_eof_and_contained_cwd()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("subdir"))?;
    let path_binding = EnvironmentDelegation::host_snapshot(EnvironmentVariableName::new("PATH")?)?;
    let expected_path = std::env::var("PATH")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(46)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(16_384, 16_384, 32_768, 4096)?, [path_binding], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let initial = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(shell, [String::from("-c"), String::from("stty size")])?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        control(),
    )?;
    let (initial_output, initial_status) = collect_joined_output(&sessions, initial).await?;
    assert_eq!(initial_status, JoinedProcessStatus::Exited(Some(0)));
    assert!(String::from_utf8_lossy(&initial_output).contains("24 80"));
    sessions.terminate(initial)?;

    let script = "IFS= read -r first; stty size; printf 'got=%s\\npath=%s\\ncwd=%s\\n' \"$first\" \"$PATH\" \"$PWD\"; while IFS= read -r rest; do :; done; printf 'eof\\n'";
    let id = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(shell, [String::from("-c"), String::from(script)])?,
            "subdir",
            [],
            true,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        control(),
    )?;
    sessions.write_stdin(id, b"hello\n", JoinedProcessInputDisposition::Close)?;
    let mut output = Vec::new();
    let mut terminal = JoinedProcessStatus::Running;
    let started = Instant::now();
    while before_event_deadline(started) {
        let polled = sessions.poll(id, 8192, 256)?;
        assert!(
            polled
                .records()
                .iter()
                .all(|record| record.stream() == JoinedProcessOutputStream::Terminal)
        );
        for record in polled.records() {
            output.extend_from_slice(record.bytes());
        }
        terminal = polled.status();
        if terminal.is_terminal() && !polled.more_output() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let output = String::from_utf8_lossy(&output);
    assert_eq!(terminal, JoinedProcessStatus::Exited(Some(0)));
    // The initial size is the only size: nothing in the frozen tool catalog
    // can resize a session's terminal, so the session keeps what it opened with.
    assert!(
        output.contains(&format!(
            "{} {}",
            plakous_tools::JOINED_PROCESS_PTY_INITIAL_ROWS,
            plakous_tools::JOINED_PROCESS_PTY_INITIAL_COLUMNS
        )),
        "{output:?}"
    );
    assert!(output.contains("got=hello"), "{output:?}");
    assert!(
        output.contains(&format!("path={expected_path}")),
        "{output:?}"
    );
    assert!(
        output.contains(&format!(
            "cwd={}",
            workspace.path().join("subdir").display()
        )),
        "{output:?}"
    );
    assert!(output.contains("eof"), "{output:?}");
    sessions.terminate(id)?;
    sessions.join_all()?;
    assert!(sessions.is_drained());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn joined_process_pty_terminate_and_drop_clean_descendants() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(47)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let first_pid_path = workspace.path().join("pty-child.pid");
    let sessions = environment.joined_process_sessions()?;
    let first = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                shell,
                [
                    String::from("-c"),
                    format!("sleep 30 & echo $! > '{}'; wait", first_pid_path.display()),
                ],
            )?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        control(),
    )?;
    let first_pid = wait_for_pid_file(&first_pid_path).await?;
    assert_eq!(sessions.terminate(first)?, JoinedProcessStatus::Terminated);
    assert!(wait_for_process_exit(first_pid).await);

    let dropped_pid_path = workspace.path().join("pty-dropped-child.pid");
    let dropped = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                shell,
                [
                    String::from("-c"),
                    format!(
                        "sleep 30 & echo $! > '{}'; wait",
                        dropped_pid_path.display()
                    ),
                ],
            )?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        control(),
    )?;
    let dropped_pid = wait_for_pid_file(&dropped_pid_path).await?;
    let _ = dropped;
    drop(sessions);
    drop(environment);
    assert!(wait_for_process_exit(dropped_pid).await);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn joined_process_pty_output_limit_keeps_the_tail_and_joins_the_group()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(48)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4, 4, 8, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let printf = unix_program(&["/usr/bin/printf", "/bin/printf"])?;
    let id = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(printf, [String::from("output-too-large")])?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        control(),
    )?;
    let mut status = JoinedProcessStatus::Running;
    let started = Instant::now();
    while before_event_deadline(started) {
        let polled = sessions.poll(id, 8, 4)?;
        status = polled.status();
        if status.is_terminal() && !polled.more_output() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
    assert_eq!(
        sessions.terminate(id)?,
        JoinedProcessStatus::Exited(Some(0))
    );
    sessions.join_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn joined_process_pty_cancellation_and_timeout_join_the_group() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let cancelled_pid_path = workspace.path().join("pty-cancelled.pid");
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(49)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4096, 4096, 8192, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let cancellation = CancellationToken::new();
    let cancelled = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                shell,
                [
                    String::from("-c"),
                    format!("echo $$ > '{}'; sleep 30", cancelled_pid_path.display()),
                ],
            )?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        ExecutionOperationControl::new(cancellation.clone()),
    )?;
    let cancelled_pid = wait_for_pid_file(&cancelled_pid_path).await?;
    cancellation.cancel();
    let (_, cancelled_status) = collect_joined_output(&sessions, cancelled).await?;
    assert_eq!(cancelled_status, JoinedProcessStatus::Cancelled);
    assert!(wait_for_process_exit(cancelled_pid).await);
    sessions.terminate(cancelled)?;

    let timed_out_pid_path = workspace.path().join("pty-timed-out.pid");
    let timeout_limits = ExecutionEnvironmentLimits::new(
        4096,
        4096,
        8192,
        4096,
        Duration::from_millis(100),
        Duration::from_secs(2),
    )?;
    let timeout_environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(50)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(timeout_limits, [], [])?,
    )?;
    let timeout_sessions = timeout_environment.joined_process_sessions()?;
    let timed_out = timeout_sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                shell,
                [
                    String::from("-c"),
                    format!("echo $$ > '{}'; sleep 30", timed_out_pid_path.display()),
                ],
            )?,
            ".",
            [],
            false,
            JoinedProcessTerminalMode::UnixPty,
        )?,
        control(),
    )?;
    let timed_out_pid = wait_for_pid_file(&timed_out_pid_path).await?;
    let (_, timed_out_status) = collect_joined_output(&timeout_sessions, timed_out).await?;
    assert_eq!(timed_out_status, JoinedProcessStatus::TimedOut);
    assert!(wait_for_process_exit(timed_out_pid).await);
    timeout_sessions.terminate(timed_out)?;
    sessions.join_all()?;
    timeout_sessions.join_all()?;
    Ok(())
}

#[tokio::test]
async fn joined_process_sessions_keep_the_newest_output_window() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(45)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4, 4, 8, 4096)?, [], [])?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let printf = unix_program(&["/usr/bin/printf", "/bin/printf"])?;
    let id = sessions.start(
        joined_request(
            JoinedProcessCommand::literal(printf, [String::from("output-too-large")])?,
            false,
        )?,
        control(),
    )?;
    let mut polled = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 8, 16)?;
    let started = Instant::now();
    while before_event_deadline(started) {
        if polled.status().is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        polled = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 8, 16)?;
    }
    // Output past the window does not kill the process: it runs to its own
    // exit, and a reader behind the window gets the newest bytes plus an
    // exact count of what it missed.
    assert_eq!(polled.status(), JoinedProcessStatus::Exited(Some(0)));
    assert_eq!(polled.skipped_bytes(), 8);
    let text = polled
        .records()
        .iter()
        .flat_map(|record| record.bytes().iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(text, b"oo-large");
    assert_eq!(polled.next_cursor().get(), 16);
    assert_eq!(
        sessions.terminate(id)?,
        JoinedProcessStatus::Exited(Some(0))
    );
    sessions.join_all()?;
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one query conformance matrix")]
async fn workspace_operations_reject_traversal_and_query_stably_without_symlinks()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join("z.txt"), b"needle\n")?;
    fs::write(workspace.path().join("src/a.txt"), b"Needle then needle\n")?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(2)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend,
    )?;

    // A path that names its way out says so. It used to arrive as the same
    // `WorkspacePolicy` as a file-type refusal.
    for path in ["../outside", "/absolute"] {
        assert_eq!(
            WorkspaceReadSliceRequest::start(path, 8)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::OutsideWorkspace)
        );
        assert_eq!(
            RepositoryQuery::new(
                path,
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?,
            )
            .err()
            .map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::OutsideWorkspace)
        );
    }

    {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("secret"), b"not-readable")?;
        symlink(
            outside.path().join("secret"),
            workspace.path().join("escape"),
        )?;
        let escaped = environment
            .read_workspace(WorkspaceReadSliceRequest::start("escape", 32)?, control())
            .await;
        assert_eq!(
            escaped.err().map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::SymlinkEscape)
        );
    }

    let queried = environment
        .query_repository(
            RepositoryQuery::new(
                ".",
                b"needle".to_vec(),
                RepositoryMatchCase::AsciiInsensitive,
                RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?,
            )?,
            control(),
        )
        .await?;
    let matches = queried
        .matches()
        .iter()
        .map(|found| {
            (
                found.path().to_path_buf(),
                found.line(),
                found.column(),
                found.bytes().as_ref().to_vec(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches,
        vec![
            ("src/a.txt".into(), 1, 1, b"Needle then needle".to_vec()),
            ("src/a.txt".into(), 1, 13, b"Needle then needle".to_vec()),
            ("z.txt".into(), 1, 1, b"needle".to_vec()),
        ]
    );
    assert_eq!(queried.files_considered(), 2);
    assert_eq!(queried.files_scanned(), 2);
    assert_eq!(queried.bytes_scanned(), 26);
    assert!(!queried.incomplete());

    let bounded = environment
        .query_repository(
            RepositoryQuery::new(
                ".",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                RepositoryQueryLimits::new(1, 32, 128, 256, 1, 6, 64)?,
            )?,
            control(),
        )
        .await?;
    assert_eq!(bounded.files_considered(), 1);
    assert_eq!(bounded.matches().len(), 1);
    assert_eq!(bounded.matches()[0].column(), 13);
    assert_eq!(bounded.matches()[0].bytes().as_ref(), b"Needle");
    assert!(bounded.matches()[0].truncated());
    assert!(bounded.incomplete());
    assert_eq!(
        RepositoryQuery::new(
            ".",
            "nëedle".as_bytes().to_vec(),
            RepositoryMatchCase::AsciiInsensitive,
            RepositoryQueryLimits::new(1, 1, 8, 8, 1, 8, 8)?,
        )
        .err()
        .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );
    Ok(())
}

#[tokio::test]
async fn repository_query_reports_scan_stop_before_byte_ceiling() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("docs"))?;
    fs::write(
        workspace.path().join("docs/a.md"),
        b"needle in architecture\n",
    )?;
    fs::write(workspace.path().join("docs/b.md"), b"unrelated\n")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(51)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let scan_bounded = environment
        .query_repository(
            RepositoryQuery::new(
                "docs",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                RepositoryQueryLimits::new(16, 32, 24, 24, 16, 128, 256)?,
            )?,
            control(),
        )
        .await?;
    assert!(scan_bounded.bytes_scanned() < 24);
    assert!(scan_bounded.incomplete());
    assert_eq!(
        scan_bounded.stop_reason(),
        Some(plakous_tools::RepositoryQueryStopReason::ScannedBytes)
    );
    Ok(())
}

#[tokio::test]
async fn repository_query_searches_a_regular_file_root_and_still_walks_directories()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::create_dir(workspace.path().join("docs"))?;
    fs::write(
        workspace.path().join("docs/foo.md"),
        b"needle in architecture\n",
    )?;
    fs::write(workspace.path().join("docs/other.md"), b"unrelated\n")?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(51)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend,
    )?;
    let limits = RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?;

    let file_query = environment
        .query_repository(
            RepositoryQuery::new(
                "docs/foo.md",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                limits,
            )?,
            control(),
        )
        .await?;
    assert_eq!(file_query.matches().len(), 1);
    assert_eq!(file_query.matches()[0].path(), Path::new("docs/foo.md"));
    assert_eq!(file_query.matches()[0].line(), 1);
    assert_eq!(file_query.matches()[0].column(), 1);
    assert_eq!(
        file_query.matches()[0].bytes().as_ref(),
        b"needle in architecture"
    );
    assert_eq!(file_query.files_considered(), 1);
    assert_eq!(file_query.files_scanned(), 1);
    assert!(!file_query.incomplete());

    let directory_query = environment
        .query_repository(
            RepositoryQuery::new(
                "docs",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                limits,
            )?,
            control(),
        )
        .await?;
    assert_eq!(directory_query.matches().len(), 1);
    assert_eq!(
        directory_query.matches()[0].path(),
        Path::new("docs/foo.md")
    );
    assert_eq!(directory_query.files_considered(), 2);
    assert_eq!(directory_query.files_scanned(), 2);
    assert!(!directory_query.incomplete());

    let listed = environment
        .query_repository(
            RepositoryQuery::glob("docs/foo.md", "*.md", limits)?,
            control(),
        )
        .await?;
    assert_eq!(listed.matches().len(), 1);
    assert_eq!(listed.matches()[0].path(), Path::new("docs/foo.md"));
    assert_eq!(listed.files_considered(), 1);
    assert_eq!(listed.files_scanned(), 0);

    std::os::unix::fs::symlink("foo.md", workspace.path().join("docs/link.md"))?;
    let linked = environment
        .query_repository(
            RepositoryQuery::new(
                "docs/link.md",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?,
            )?,
            control(),
        )
        .await;
    assert_eq!(
        linked.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::SymlinkEscape)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn repository_search_truncates_only_at_utf8_boundaries_and_yields_runtime()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let mut content = vec![b'a'; 2 * 1024 * 1024];
    content.extend_from_slice(" needle 🙂\n".as_bytes());
    fs::write(workspace.path().join("large.txt"), content)?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(47)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        backend,
    )?;
    let timer = tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        true
    });
    let result = environment
        .query_repository(
            RepositoryQuery::new(
                ".",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                RepositoryQueryLimits::new(
                    2,
                    4,
                    3 * 1024 * 1024,
                    3 * 1024 * 1024,
                    2,
                    2 * 1024 * 1024 + 10,
                    3 * 1024 * 1024,
                )?,
            )?,
            control(),
        )
        .await?;
    assert!(timer.await?);
    let retained = result.matches()[0].bytes().as_ref();
    assert!(std::str::from_utf8(retained).is_ok());
    assert!(result.matches()[0].truncated());
    Ok(())
}

#[tokio::test]
async fn deterministic_read_resume_fails_closed_when_content_changes_and_enforces_bounds()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("data.txt"), b"abcdef")?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(46)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 128, 4)?, [], [])?,
        backend,
    )?;

    let first = environment
        .read_workspace(WorkspaceReadSliceRequest::start("data.txt", 3)?, control())
        .await?;
    let cursor = first.next().cloned().ok_or("missing read cursor")?;
    fs::write(workspace.path().join("data.txt"), b"abcXYZ")?;
    let changed = environment
        .read_workspace(WorkspaceReadSliceRequest::resume(cursor, 3)?, control())
        .await;
    assert_eq!(
        changed.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::Conflict)
    );

    let over_environment_limit = environment
        .read_workspace(WorkspaceReadSliceRequest::start("data.txt", 5)?, control())
        .await;
    assert_eq!(
        over_environment_limit.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::ResourceLimit)
    );
    assert_eq!(
        WorkspaceReadSliceRequest::start("data.txt", 0)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );
    assert_eq!(
        WorkspaceReadSliceRequest::start("data.txt", u64::MAX)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one transaction conformance matrix")]
async fn workspace_mutation_preflights_all_items_and_returns_typed_results() -> Result<(), TestError>
{
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("update.txt"), b"before")?;
    fs::write(workspace.path().join("delete.txt"), b"remove")?;
    fs::write(workspace.path().join("same.txt"), b"same")?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(20)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 128, 64)?, [], [])?,
        backend,
    )?;
    let result = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([
                WorkspaceMutation::new(
                    "update.txt",
                    WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(
                        b"before",
                    )?),
                    WorkspaceReplacement::content(b"after".to_vec()),
                )?,
                WorkspaceMutation::new(
                    "create.txt",
                    WorkspaceExpectedFileState::Missing,
                    WorkspaceReplacement::content(b"created".to_vec()),
                )?,
                WorkspaceMutation::new(
                    "delete.txt",
                    WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(
                        b"remove",
                    )?),
                    WorkspaceReplacement::Missing,
                )?,
                WorkspaceMutation::new(
                    "same.txt",
                    WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(
                        b"same",
                    )?),
                    WorkspaceReplacement::content(b"same".to_vec()),
                )?,
            ])?,
            control(),
        )
        .await?;
    assert_eq!(
        result
            .items()
            .iter()
            .map(plakous_tools::WorkspaceMutationResultItem::disposition)
            .collect::<Vec<_>>(),
        vec![
            WorkspaceMutationDisposition::Created,
            WorkspaceMutationDisposition::Deleted,
            WorkspaceMutationDisposition::Unchanged,
            WorkspaceMutationDisposition::Updated,
        ]
    );
    assert_eq!(
        result
            .items()
            .iter()
            .map(|item| item.path().to_path_buf())
            .collect::<Vec<_>>(),
        vec![
            PathBuf::from("create.txt"),
            PathBuf::from("delete.txt"),
            PathBuf::from("same.txt"),
            PathBuf::from("update.txt"),
        ]
    );
    assert_eq!(fs::read(workspace.path().join("update.txt"))?, b"after");
    assert_eq!(fs::read(workspace.path().join("create.txt"))?, b"created");
    assert!(!workspace.path().join("delete.txt").exists());

    fs::write(workspace.path().join("first.txt"), b"first")?;
    fs::write(workspace.path().join("z-conflict.txt"), b"actual")?;
    let conflict = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([
                WorkspaceMutation::new(
                    "first.txt",
                    WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(
                        b"first",
                    )?),
                    WorkspaceReplacement::content(b"changed".to_vec()),
                )?,
                WorkspaceMutation::new(
                    "z-conflict.txt",
                    WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(
                        b"expected",
                    )?),
                    WorkspaceReplacement::content(b"replacement".to_vec()),
                )?,
            ])?,
            control(),
        )
        .await;
    assert_eq!(
        conflict.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::Conflict)
    );
    assert_eq!(fs::read(workspace.path().join("first.txt"))?, b"first");
    assert_eq!(
        fs::read(workspace.path().join("z-conflict.txt"))?,
        b"actual"
    );
    Ok(())
}

#[tokio::test]
async fn workspace_mutation_any_creates_or_overwrites() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("existing.txt"), b"before")?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(52)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 128, 64)?, [], [])?,
        backend,
    )?;
    let created = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                "fresh.txt",
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::content(b"created".to_vec()),
            )?])?,
            control(),
        )
        .await?;
    assert_eq!(
        created.items()[0].disposition(),
        WorkspaceMutationDisposition::Created
    );
    assert_eq!(fs::read(workspace.path().join("fresh.txt"))?, b"created");
    let overwritten = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                "existing.txt",
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::content(b"after".to_vec()),
            )?])?,
            control(),
        )
        .await?;
    assert_eq!(
        overwritten.items()[0].disposition(),
        WorkspaceMutationDisposition::Updated
    );
    assert_eq!(fs::read(workspace.path().join("existing.txt"))?, b"after");
    Ok(())
}

#[tokio::test]
async fn workspace_mutation_any_rejects_symlink_overwrite() -> Result<(), TestError> {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, b"outside")?;
    symlink(&outside_file, workspace.path().join("linked.txt"))?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(53)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 128, 64)?, [], [])?,
        backend,
    )?;
    let linked = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                "linked.txt",
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::content(b"replacement".to_vec()),
            )?])?,
            control(),
        )
        .await;
    assert_eq!(
        linked.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::SymlinkEscape)
    );
    assert_eq!(fs::read(outside_file)?, b"outside");
    Ok(())
}

#[tokio::test]
async fn workspace_mutation_any_rejects_missing_parent_directory() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let parent = workspace.path().join("missing-dir");
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(54)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 128, 64)?, [], [])?,
        backend,
    )?;
    let missing_parent = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                "missing-dir/fresh.txt",
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::content(b"created".to_vec()),
            )?])?,
            control(),
        )
        .await;
    // A create makes the directories it needs.
    assert!(missing_parent.is_ok(), "{missing_parent:?}");
    assert_eq!(fs::read(parent.join("fresh.txt"))?, b"created");

    // A transaction that fails after staging removes the directories it made.
    fs::write(workspace.path().join("kept.txt"), b"old")?;
    let stale = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([
                WorkspaceMutation::new(
                    "deep/er/new.txt",
                    WorkspaceExpectedFileState::Any,
                    WorkspaceReplacement::content(b"new".to_vec()),
                )?,
                WorkspaceMutation::new(
                    "kept.txt",
                    WorkspaceExpectedFileState::Missing,
                    WorkspaceReplacement::content(b"clobber".to_vec()),
                )?,
            ])?,
            control(),
        )
        .await;
    assert!(stale.is_err());
    assert!(!workspace.path().join("deep").exists());
    assert_eq!(fs::read(workspace.path().join("kept.txt"))?, b"old");

    // A symbolic link in the parent chain is refused, never followed.
    let outside = tempfile::tempdir()?;
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked"))?;
    let through_link = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                "linked/sub/escape.txt",
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::content(b"x".to_vec()),
            )?])?,
            control(),
        )
        .await;
    assert!(through_link.is_err());
    assert!(!outside.path().join("sub").exists());
    Ok(())
}

#[tokio::test]
async fn workspace_mutation_rejects_duplicates_traversal_symlinks_and_limits()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("small.txt"), b"old")?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(21)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(8, 8, 16, 4)?, [], [])?,
        backend,
    )?;

    assert_eq!(
        WorkspaceMutation::new(
            "../outside",
            WorkspaceExpectedFileState::Missing,
            WorkspaceReplacement::content(b"bad".to_vec()),
        )
        .err()
        .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::OutsideWorkspace)
    );
    let duplicate = WorkspaceMutationTransaction::new([
        WorkspaceMutation::new(
            "small.txt",
            WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?),
            WorkspaceReplacement::content(b"one".to_vec()),
        )?,
        WorkspaceMutation::new(
            "./small.txt",
            WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?),
            WorkspaceReplacement::content(b"two".to_vec()),
        )?,
    ]);
    assert_eq!(
        duplicate.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );

    let oversized = environment
        .mutate_workspace(
            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                "small.txt",
                WorkspaceExpectedFileState::Content(WorkspaceContentIdentity::from_bytes(b"old")?),
                WorkspaceReplacement::content(b"12345".to_vec()),
            )?])?,
            control(),
        )
        .await;
    assert_eq!(
        oversized.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::ResourceLimit)
    );
    assert_eq!(fs::read(workspace.path().join("small.txt"))?, b"old");

    {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir()?;
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, b"outside")?;
        symlink(&outside_file, workspace.path().join("linked.txt"))?;
        let linked = environment
            .mutate_workspace(
                WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                    "linked.txt",
                    WorkspaceExpectedFileState::Missing,
                    WorkspaceReplacement::content(b"x".to_vec()),
                )?])?,
                control(),
            )
            .await;
        assert_eq!(
            linked.err().map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::SymlinkEscape)
        );
        assert_eq!(fs::read(outside_file)?, b"outside");
    }
    Ok(())
}

#[tokio::test]
async fn fake_process_preserves_literal_argv_and_exact_noncredential_allowlist()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let delegated = environment_binding("SAFE_VALUE", 1)?;
    let unselected = environment_binding("UNSELECTED_VALUE", 1)?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [
            (delegated.clone(), String::from("delegated-value")),
            (unselected, String::from("must-not-appear")),
        ],
        DeterministicProcessPolicy::new(true, true, true),
        [DeterministicProcessStep::Complete(
            LiteralProcessResult::new(Some(0), b"ok".to_vec(), b"warn".to_vec())?,
        )],
    )?);
    let config = ExecutionEnvironmentConfig::new(
        limits(8, 8, 12, 32)?,
        [delegated],
        [credential_binding()?],
    )?;
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(3)?),
        workspace.path(),
        ".",
        config,
        Arc::clone(&backend),
    )?;
    let arguments = [
        "space value",
        "-leading-option",
        "; touch nope",
        "`touch nope`",
        "$(touch nope)",
        "*.rs",
    ]
    .map(String::from);
    let result = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                "literal-program",
                arguments.clone(),
                ".",
                b"stdin".to_vec(),
                LiteralProcessIntent::new(true, true),
            )?,
            control(),
        )
        .await?;
    assert_eq!(result.exit_code(), Some(0));
    assert_eq!(result.stdout().as_ref(), b"ok");
    assert_eq!(result.stderr().as_ref(), b"warn");

    let calls = backend.process_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program(), "literal-program");
    assert_eq!(calls[0].arguments(), arguments.as_slice());
    assert_eq!(calls[0].stdin_bytes(), 5);
    assert_eq!(
        calls[0]
            .environment_names()
            .iter()
            .map(EnvironmentVariableName::as_str)
            .collect::<Vec<_>>(),
        vec!["SAFE_VALUE"]
    );
    let debug = format!("{backend:?} {:?}", calls[0]);
    assert!(!debug.contains("delegated-value"));
    assert!(!debug.contains("must-not-appear"));
    assert!(!debug.contains("credential.backend-test"));
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn host_literal_missing_cwd_is_no_effect_and_a_corrected_request_can_execute()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(83)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let request = |cwd| {
        LiteralProcessRequest::new(
            "/bin/sh",
            [
                String::from("-c"),
                String::from("printf executed > process-marker"),
            ],
            cwd,
            [],
            LiteralProcessIntent::new(true, false),
        )
    };
    let refused = environment
        .execute_literal_process(request("missing/workdir")?, control())
        .await;
    assert_eq!(
        refused.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::NotFound)
    );
    assert!(!workspace.path().join("process-marker").exists());
    let corrected = environment
        .execute_literal_process(request(".")?, control())
        .await?;
    assert_eq!(corrected.exit_code(), Some(0));
    assert_eq!(
        fs::read(workspace.path().join("process-marker"))?,
        b"executed"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one host-process conformance schedule retains literal argv, cwd, and environment assertions together"
)]
async fn host_process_uses_literal_argv_absolute_program_and_explicit_environment()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let printf = unix_program(&["/usr/bin/printf", "/bin/printf"])?;
    let env_program = unix_program(&["/usr/bin/env", "/bin/env"])?;
    let cat = unix_program(&["/usr/bin/cat", "/bin/cat"])?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(22)?),
        workspace.path(),
        ".",
        // The caps hold the full host PATH and the operator HOME.
        ExecutionEnvironmentConfig::new(limits(65_536, 64, 65_600, 64)?, [], [])?,
    )?;
    let literal_arguments = [
        String::from("%s|%s|%s|%s|%s"),
        String::from("space value"),
        String::from("; touch nope"),
        String::from("`touch nope`"),
        String::from("$(touch nope)"),
        String::from("*.rs"),
    ];
    let literal = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                printf,
                literal_arguments,
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    assert_eq!(literal.exit_code(), Some(0));
    assert_eq!(
        literal.stdout().as_ref(),
        b"space value|; touch nope|`touch nope`|$(touch nope)|*.rs"
    );
    assert!(literal.stderr().is_empty());
    assert!(!workspace.path().join("nope").exists());

    let stdin = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                cat,
                Vec::<String>::new(),
                ".",
                b"exact\0stdin".to_vec(),
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    assert_eq!(stdin.stdout().as_ref(), b"exact\0stdin");

    // Basename programs resolve on the host's minimal PATH (not ambient env).
    let relative = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                "printf",
                [String::from("not-run")],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    assert_eq!(relative.exit_code(), Some(0));
    assert_eq!(relative.stdout().as_ref(), b"not-run");

    let missing = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                "plak-host-binary-does-not-exist-xyz",
                [String::from("not-run")],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await;
    assert_eq!(
        missing.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::InvalidRequest)
    );

    let empty_environment = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                env_program,
                Vec::<String>::new(),
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    // The child keeps the host PATH and the operator's real `$HOME`.
    let empty_environment = std::str::from_utf8(empty_environment.stdout())?;
    let host_path = std::env::var("PATH").unwrap_or_default();
    let expected_default_path = if host_path.is_empty() {
        String::from("/usr/local/bin:/usr/bin:/bin")
    } else {
        host_path
    };
    assert!(empty_environment.contains(&format!("PATH={expected_default_path}\n")));
    assert_home_line(empty_environment);

    let path_binding = EnvironmentDelegation::host_snapshot(EnvironmentVariableName::new("PATH")?)?;
    let delegated_environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(23)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(
            limits(65_536, 64, 65_600, 64)?,
            [path_binding],
            [credential_binding()?],
        )?,
    )?;
    let delegated = delegated_environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                env_program,
                Vec::<String>::new(),
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    let expected_path = std::env::var("PATH")?;
    let delegated = std::str::from_utf8(delegated.stdout())?;
    assert!(delegated.contains(&format!("PATH={expected_path}\n")));
    assert_home_line(delegated);
    Ok(())
}

/// Every host spawn carries a usable `HOME`, on every Unix, not just Linux.
///
/// `env_clear` plus a curated re-apply used to leave `HOME` unset off Linux.
/// Bash still reads `~/.profile` from the passwd entry, so a profile line like
/// `. "$HOME/.cargo/env"` resolved against an empty `HOME` and every shell the
/// model ran printed an error before its own command.
#[tokio::test]
async fn host_process_exports_the_captured_home() -> Result<(), TestError> {
    let Some(expected) = captured_host_home() else {
        return Ok(());
    };
    let workspace = tempfile::tempdir()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(96)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(65_536, 64, 65_600, 64)?, [], [])?,
    )?;
    let home = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                shell,
                [String::from("-c"), String::from("printf %s \"$HOME\"")],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    assert_eq!(home.exit_code(), Some(0));
    assert_eq!(
        std::str::from_utf8(home.stdout())?,
        expected.to_string_lossy()
    );
    Ok(())
}

/// The captured host home under the same filter the backend applies, or `None`
/// when this host has no usable one and the assertion would not be meaningful.
fn captured_host_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| fs::canonicalize(&home).unwrap_or(home))
        .filter(|home| home.is_dir() && home != Path::new("/"))
}

fn assert_home_line(environment: &str) {
    match captured_host_home() {
        Some(home) => assert!(environment.contains(&format!("HOME={}\n", home.display()))),
        None => assert!(!environment.lines().any(|line| line.starts_with("HOME="))),
    }
}

/// Process tools carry the operator's full host authority on every platform:
/// a file outside the workspace is readable and writable from the child.
#[tokio::test]
async fn host_process_reaches_paths_outside_the_workspace() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let marker = outside.path().join("outside.txt");
    fs::write(&marker, b"host")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(0x93)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let script = format!("cat '{0}' && printf written > '{0}.out'", marker.display());
    let outcome = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                unix_program(&["/bin/sh", "/usr/bin/sh"])?,
                [String::from("-c"), script],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    assert_eq!(outcome.exit_code(), Some(0));
    assert_eq!(&outcome.stdout()[..], b"host");
    assert_eq!(
        fs::read(outside.path().join("outside.txt.out"))?,
        b"written"
    );
    Ok(())
}

#[test]
fn file_tools_refuse_credentials_outside_the_workspace() -> Result<(), TestError> {
    let outer = tempfile::tempdir()?;
    let workspace = outer.path().join("workspace");
    let credentials = outer.path().join("state/plak/secrets/provider.key");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(credentials.parent().ok_or("credential parent missing")?)?;
    fs::write(&credentials, b"HOST_CREDENTIAL_SENTINEL")?;
    // Both of these name their way out of the workspace: one absolutely, one
    // through a parent. That is what the refusal says now, rather than the
    // catch-all policy kind that used to cover every refused path.
    for forbidden in [
        credentials.to_string_lossy().into_owned(),
        String::from("../state/plak/secrets/provider.key"),
    ] {
        assert_eq!(
            WorkspaceReadSliceRequest::start(&forbidden, 32)
                .err()
                .map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::OutsideWorkspace)
        );
        assert_eq!(
            RepositoryQuery::new(
                &forbidden,
                b"HOST_CREDENTIAL_SENTINEL".to_vec(),
                RepositoryMatchCase::Sensitive,
                RepositoryQueryLimits::new(4, 32, 128, 256, 4, 32, 128)?,
            )
            .err()
            .map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::OutsideWorkspace)
        );
        assert_eq!(
            WorkspaceMutation::new(
                &forbidden,
                WorkspaceExpectedFileState::Missing,
                WorkspaceReplacement::content(b"overwrite".to_vec()),
            )
            .err()
            .map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::OutsideWorkspace)
        );
    }
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the paired output-cap and timeout schedules share one exact bounded host fixture"
)]
async fn host_process_bounds_live_output_and_enforces_timeout() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let yes = unix_program(&["/usr/bin/yes", "/bin/yes"])?;
    let sleep = unix_program(&["/usr/bin/sleep", "/bin/sleep"])?;
    let output_environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(24)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::new(
                32,
                32,
                64,
                64,
                Duration::from_secs(5),
                Duration::from_millis(100),
            )?,
            [],
            [],
        )?,
    )?;
    let output_error = output_environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                yes,
                Vec::<String>::new(),
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await;
    assert_eq!(
        output_error.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::ResourceLimit)
    );

    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let combined_environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(27)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::new(
                8,
                8,
                8,
                64,
                Duration::from_secs(5),
                Duration::from_millis(100),
            )?,
            [],
            [],
        )?,
    )?;
    let combined_error = combined_environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                shell,
                [
                    String::from("-c"),
                    String::from("printf 123456; printf abcdef >&2"),
                ],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await;
    assert_eq!(
        combined_error.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::ResourceLimit)
    );

    let timeout_environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(25)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::new(
                32,
                32,
                64,
                64,
                Duration::from_millis(50),
                Duration::from_millis(100),
            )?,
            [],
            [],
        )?,
    )?;
    let timeout = timeout_environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                sleep,
                [String::from("30")],
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await;
    assert_eq!(
        timeout.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::TimedOut)
    );
    Ok(())
}

#[tokio::test]
async fn host_cancel_and_future_drop_join_the_owned_process_group() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(26)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::new(
                128,
                128,
                256,
                128,
                Duration::from_secs(30),
                Duration::from_millis(200),
            )?,
            [],
            [],
        )?,
    )?;

    let cancellation = CancellationToken::new();
    let operation = tokio::spawn(environment.execute_literal_process(
        LiteralProcessRequest::new(
            shell,
            [
                String::from("-c"),
                String::from("sleep 30 & child=$!; printf '%s' \"$child\" > child.pid; wait"),
            ],
            ".",
            [],
            LiteralProcessIntent::default(),
        )?,
        ExecutionOperationControl::new(cancellation.clone()),
    ));
    let child_path = workspace.path().join("child.pid");
    let child_pid = wait_for_pid_file(&child_path).await?;
    cancellation.cancel();
    let cancelled = operation.await?;
    assert_eq!(
        cancelled.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::Cancelled)
    );
    assert!(wait_for_process_exit(child_pid).await);

    let dropped = tokio::spawn(environment.execute_literal_process(
        LiteralProcessRequest::new(
            shell,
            [
                String::from("-c"),
                String::from("printf '%s' \"$$\" > owner.pid; sleep 30"),
            ],
            ".",
            [],
            LiteralProcessIntent::default(),
        )?,
        control(),
    ));
    let owner_path = workspace.path().join("owner.pid");
    let owner_pid = wait_for_pid_file(&owner_path).await?;
    dropped.abort();
    let _ = dropped.await;
    assert!(wait_for_process_exit(owner_pid).await);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::too_many_lines,
    reason = "the detached-process fixture keeps launch, bounded owner cleanup, and failure-safe escaped-process cleanup auditable together"
)]
async fn host_future_drop_is_bounded_with_a_detached_continuous_writer() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let setsid = unix_program(&["/usr/bin/setsid", "/bin/setsid"])?;
    let cleanup_duration = Duration::from_millis(100);
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(28)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(
            ExecutionEnvironmentLimits::new(
                65_536,
                65_536,
                65_536,
                128,
                Duration::from_secs(30),
                cleanup_duration,
            )?,
            [],
            [],
        )?,
    )?;
    let writer_path = workspace.path().join("writer.pid");
    let started_path = workspace.path().join("writer.started");
    let stop_path = workspace.path().join("writer.stop");
    let activate_path = workspace.path().join("writer.activate");
    let writer_argument = writer_path
        .to_str()
        .ok_or("detached writer PID path was not UTF-8")?
        .to_owned();
    let started_argument = started_path
        .to_str()
        .ok_or("detached writer marker path was not UTF-8")?
        .to_owned();
    let stop_argument = stop_path
        .to_str()
        .ok_or("detached writer stop path was not UTF-8")?
        .to_owned();
    let activate_argument = activate_path
        .to_str()
        .ok_or("detached writer activation path was not UTF-8")?
        .to_owned();
    let detached_script = String::from(
        "\"$1\" \"$2\" -c 'set -e; pid_file=$1; started_file=$2; stop_file=$3; \
         activate_file=$4; \
         trap \"\" PIPE; \
         temporary=${pid_file}.tmp; printf \"%s\\n\" \"$$\" > \"$temporary\"; \
         mv \"$temporary\" \"$pid_file\"; waits=0; \
         while [ ! -f \"$activate_file\" ]; do waits=$((waits + 1)); \
         [ \"$waits\" -lt 1000 ] || exit 70; sleep 0.01; done; \
         printf started > \"$started_file\"; writes=0; \
         while [ ! -f \"$stop_file\" ]; do printf 0123456789abcdef 2>/dev/null || :; \
         writes=$((writes + 1)); [ \"$writes\" -lt 1000 ] || exit 71; sleep 0.01; done' \
         plak-detached-writer \"$3\" \"$4\" \"$5\" \"$6\" & sleep 30",
    );
    let mut operation = tokio::spawn(environment.execute_literal_process(
        LiteralProcessRequest::new(
            shell,
            [
                String::from("-c"),
                detached_script,
                String::from("plak-owner"),
                String::from(setsid),
                String::from(shell),
                writer_argument,
                started_argument,
                stop_argument,
                activate_argument,
            ],
            ".",
            [],
            LiteralProcessIntent::default(),
        )?,
        control(),
    ));
    let writer_pid = match wait_for_pid_file(&writer_path).await {
        Ok(pid) => pid,
        Err(error) => {
            operation.abort();
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut operation).await;
            return Err(error);
        }
    };
    let mut writer_guard = DetachedProcessGuard::new(writer_pid, stop_path.clone());
    fs::write(&activate_path, b"")?;
    if let Err(error) = wait_for_file(&started_path).await {
        operation.abort();
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut operation).await;
        return Err(error);
    }

    let cleanup_started = Instant::now();
    operation.abort();
    let completion = tokio::time::timeout(Duration::from_secs(1), &mut operation).await;
    let cleanup_elapsed = cleanup_started.elapsed();

    let completion = completion.map_err(|_| "owner cleanup exceeded its bounded allowance")?;
    let Err(join_error) = completion else {
        return Err("aborted owner operation completed successfully".into());
    };
    assert!(
        join_error.is_cancelled(),
        "owner operation was not cancelled"
    );

    fs::write(&stop_path, b"")?;
    assert!(
        cleanup_elapsed < cleanup_duration + Duration::from_millis(900),
        "owner cleanup exceeded the cleanup budget plus scheduling allowance"
    );
    assert!(wait_for_process_exit(writer_pid).await);
    writer_guard.disarm();
    Ok(())
}

#[tokio::test]
async fn fake_policy_and_output_caps_fail_before_unsafe_success() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::new(true, false, false),
        [DeterministicProcessStep::Complete(
            LiteralProcessResult::new(Some(0), b"12345".to_vec(), Vec::new())?,
        )],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(4)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(4, 4, 6, 8)?, [], [])?,
        Arc::clone(&backend),
    )?;

    for intent in [
        LiteralProcessIntent::new(true, false),
        LiteralProcessIntent::new(false, true),
    ] {
        let denied = environment
            .execute_literal_process(
                LiteralProcessRequest::new("program", [], ".", [], intent)?,
                control(),
            )
            .await;
        assert_eq!(
            denied.err().map(|error| error.kind()),
            Some(ExecutionBackendErrorKind::Denied)
        );
    }
    assert_eq!(
        backend.process_calls(),
        [] as [plakous_tools::RecordedLiteralProcess; 0]
    );

    let oversized = environment
        .execute_literal_process(
            LiteralProcessRequest::new("program", [], ".", [], LiteralProcessIntent::default())?,
            control(),
        )
        .await;
    assert_eq!(
        oversized.err().map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::ResourceLimit)
    );
    assert_eq!(backend.process_calls().len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancellation_error_is_terminal_only_after_cleanup() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::new(true, false, false),
        [DeterministicProcessStep::WaitForCancellation { descendants: 2 }],
    )?);
    let environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(6)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        Arc::clone(&backend),
    )?;
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(environment.execute_literal_process(
        LiteralProcessRequest::new(
            "program",
            Vec::<String>::new(),
            ".",
            Vec::new(),
            LiteralProcessIntent::default(),
        )?,
        ExecutionOperationControl::new(cancellation.clone()),
    ));
    tokio::task::yield_now().await;
    assert!(!backend.status().is_drained());
    cancellation.cancel();
    let error = task
        .await?
        .err()
        .ok_or("cancellation unexpectedly succeeded")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Cancelled);
    assert!(backend.status().is_drained());

    let second_backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::new(true, false, false),
        [DeterministicProcessStep::WaitForCancellation { descendants: 1 }],
    )?);
    let second_environment = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(7)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        Arc::clone(&second_backend),
    )?;
    let pending = tokio::spawn(second_environment.execute_literal_process(
        LiteralProcessRequest::new(
            "program",
            Vec::<String>::new(),
            ".",
            Vec::new(),
            LiteralProcessIntent::default(),
        )?,
        control(),
    ));
    tokio::task::yield_now().await;
    assert!(!second_backend.status().is_drained());
    pending.abort();
    let _ = pending.await;
    assert!(second_backend.status().is_drained());
    Ok(())
}

#[test]
fn fake_authority_instance_is_identity_bound_and_not_reconstructed_by_serde()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let first_backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let second_backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    let first = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(8)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        first_backend,
    )?;
    let second = ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(8)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        second_backend,
    )?;
    assert_eq!(
        first.backend().kind(),
        ExecutionBackendKind::DeterministicFake
    );
    assert_eq!(first.descriptor(), second.descriptor());
    assert_ne!(
        first.backend().authority_id(),
        second.backend().authority_id()
    );
    let comparison = serde_json::from_value(serde_json::to_value(first.descriptor())?)?;
    assert!(first.matches_descriptor(&comparison));
    assert!(second.matches_descriptor(&comparison));
    Ok(())
}

#[tokio::test]
async fn host_process_path_prepends_required_workspace_prefix() -> Result<(), TestError> {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir()?;
    let bin = workspace.path().join("process-bin");
    fs::create_dir_all(&bin)?;
    let shim = bin.join("git");
    fs::write(&shim, "#!/bin/sh\nprintf FENCED\n")?;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    let environment = ExecutionEnvironment::host_with_policy_and_process_options(
        ExecutionEnvironmentScope::Run(run_id(91)?),
        plakous_tools::WorkspacePolicy::host(workspace.path(), ".")?,
        ExecutionEnvironmentConfig::default(),
        plakous_tools::HostProcessOptions::new(Some("process-bin".into()))?,
    )?;
    let result = environment
        .execute_literal_process(
            LiteralProcessRequest::new("git", [], ".", [], LiteralProcessIntent::default())?,
            control(),
        )
        .await?;
    assert_eq!(result.exit_code(), Some(0));
    assert_eq!(result.stdout().as_ref(), b"FENCED");
    let sessions = environment.joined_process_sessions()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let modes = [
        JoinedProcessTerminalMode::Pipes,
        JoinedProcessTerminalMode::UnixPty,
    ];
    for mode in modes {
        let profile = JoinedProcessShellProfile::new(shell, [String::from("-c")])?;
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(profile, "git")?,
            ".",
            [],
            false,
            mode,
        )?;
        let id = sessions.start(request, control())?;
        let (output, status) = collect_joined_output(&sessions, id).await?;
        assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
        // Closing PTY input may echo a newline before or after the command
        // output, and an EOF keystroke before it.
        let payload = if mode == JoinedProcessTerminalMode::UnixPty {
            let mut payload = output.as_slice();
            for echo in [
                b"\r\n".as_slice(),
                b"^D\x08\x08".as_slice(),
                b"^D".as_slice(),
            ] {
                if let Some(rest) = payload.strip_prefix(echo) {
                    payload = rest;
                }
            }
            payload.strip_suffix(b"\r\n").unwrap_or(payload)
        } else {
            &output
        };
        assert_eq!(payload, b"FENCED");
        sessions.terminate(id)?;
    }
    fs::rename(&bin, workspace.path().join("old-process-bin"))?;
    let missing = environment
        .execute_literal_process(
            LiteralProcessRequest::new("git", [], ".", [], LiteralProcessIntent::default())?,
            control(),
        )
        .await
        .err()
        .ok_or("missing required prefix fell back to host git")?;
    assert_eq!(missing.kind(), ExecutionBackendErrorKind::InvalidRequest);
    fs::create_dir(&bin)?;
    let replaced = sessions
        .start(
            joined_request(
                JoinedProcessCommand::literal(shell, [String::from("-c"), String::from("exit 0")])?,
                false,
            )?,
            control(),
        )
        .err()
        .ok_or("replacement prefix admitted a process")?;
    assert_eq!(replaced.kind(), ExecutionBackendErrorKind::InvalidRequest);
    assert!(sessions.is_drained());
    Ok(())
}

#[tokio::test]
async fn login_shell_process_restores_required_workspace_prefix() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let bin = workspace.path().join("process-bin");
    fs::create_dir_all(&bin)?;
    let environment = ExecutionEnvironment::host_with_policy_and_process_options(
        ExecutionEnvironmentScope::Run(run_id(92)?),
        plakous_tools::WorkspacePolicy::host(workspace.path(), ".")?,
        ExecutionEnvironmentConfig::default(),
        plakous_tools::HostProcessOptions::new(Some("process-bin".into()))?,
    )?;
    let sessions = environment.joined_process_sessions()?;
    let shell = unix_program(&["/bin/sh", "/usr/bin/sh"])?;
    let modes = [
        JoinedProcessTerminalMode::Pipes,
        JoinedProcessTerminalMode::UnixPty,
    ];
    for (index, mode) in modes.into_iter().enumerate() {
        // A login shell sources the operator profile, which may print to stdout
        // and rewrite PATH; the prefix is asserted through a file instead.
        let profile =
            JoinedProcessShellProfile::new(shell, [String::from("-l"), String::from("-c")])?;
        let recorded = format!("recorded-path-{index}");
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(profile, format!("printf %s \"$PATH\" > {recorded}"))?,
            ".",
            [],
            false,
            mode,
        )?;
        let id = sessions.start(request, control())?;
        let (_, status) = collect_joined_output(&sessions, id).await?;
        assert_eq!(status, JoinedProcessStatus::Exited(Some(0)));
        sessions.terminate(id)?;
        let path = fs::read_to_string(workspace.path().join(&recorded))?;
        let first = path
            .split(':')
            .next()
            .ok_or("login shell recorded an empty PATH")?;
        assert_eq!(Path::new(first).canonicalize()?, bin.canonicalize()?);
    }
    assert!(sessions.is_drained());
    Ok(())
}

fn query_environment(workspace: &Path, id: u8) -> Result<ExecutionEnvironment, TestError> {
    let backend = Arc::new(DeterministicExecutionBackend::new(
        [],
        DeterministicProcessPolicy::default(),
        [],
    )?);
    Ok(ExecutionEnvironment::deterministic(
        ExecutionEnvironmentScope::Run(run_id(id)?),
        workspace,
        ".",
        ExecutionEnvironmentConfig::default(),
        backend,
    )?)
}

/// A repository query surveys a live tree. An entry it cannot open is skipped
/// and counted, never a reason to abandon the whole query: a socket bound by a
/// neighbouring process must not make search stop working.
#[tokio::test]
async fn repository_query_surveys_past_sockets_and_fifos_and_counts_them() -> Result<(), TestError>
{
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("a.txt"), b"needle here\n")?;
    let _listener = std::os::unix::net::UnixListener::bind(workspace.path().join("socket"))?;
    // `mkfifoat` is unavailable on Apple targets, so the test makes the fifo
    // the way a shell would.
    let made = std::process::Command::new("mkfifo")
        .arg(workspace.path().join("pipe"))
        .status()?;
    assert!(made.success(), "mkfifo did not create the test fifo");
    let environment = query_environment(workspace.path(), 71)?;
    let limits = RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?;
    let queried = environment
        .query_repository(
            RepositoryQuery::new(
                ".",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                limits,
            )?,
            control(),
        )
        .await?;
    assert_eq!(queried.matches().len(), 1);
    assert_eq!(queried.matches()[0].path(), Path::new("a.txt"));
    assert_eq!(queried.files_scanned(), 1);
    assert_eq!(queried.entries_skipped(), 2);
    assert!(!queried.incomplete());
    Ok(())
}

/// A glob with a literal leading directory prefix starts its walk there.
///
/// The sibling holds an entry the walk could never open, so an `entries_skipped`
/// of zero is proof that the sibling was never visited.
#[tokio::test]
async fn repository_query_glob_prefix_prunes_the_walk_to_its_literal_directory()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::create_dir_all(workspace.path().join("docs/spec"))?;
    fs::write(
        workspace.path().join("docs/spec/01.md"),
        b"needle in the spec\n",
    )?;
    fs::create_dir(workspace.path().join("sibling"))?;
    fs::write(workspace.path().join("sibling/other.md"), b"needle too\n")?;
    let _listener =
        std::os::unix::net::UnixListener::bind(workspace.path().join("sibling/socket"))?;
    let environment = query_environment(workspace.path(), 72)?;
    let limits = RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?;

    let listed = environment
        .query_repository(
            RepositoryQuery::glob(".", "docs/spec/*.md", limits)?,
            control(),
        )
        .await?;
    assert_eq!(listed.matches().len(), 1);
    assert_eq!(listed.matches()[0].path(), Path::new("docs/spec/01.md"));
    assert_eq!(listed.entries_skipped(), 0);

    let searched = environment
        .query_repository(
            RepositoryQuery::new(
                ".",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                limits,
            )?
            .with_path_glob("docs/**")?,
            control(),
        )
        .await?;
    assert_eq!(searched.matches().len(), 1);
    assert_eq!(searched.matches()[0].path(), Path::new("docs/spec/01.md"));
    assert_eq!(searched.entries_skipped(), 0);

    // Without a literal prefix the walk still covers the whole tree, and the
    // sibling's socket is counted rather than fatal.
    let everywhere = environment
        .query_repository(
            RepositoryQuery::new(
                ".",
                b"needle".to_vec(),
                RepositoryMatchCase::Sensitive,
                limits,
            )?
            .with_path_glob("*.md")?,
            control(),
        )
        .await?;
    assert_eq!(everywhere.matches().len(), 2);
    assert_eq!(everywhere.entries_skipped(), 1);
    Ok(())
}

/// A glob prefix that names nothing yields an empty result, not a failure.
#[tokio::test]
async fn repository_query_glob_prefix_without_a_directory_is_empty() -> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("a.md"), b"needle\n")?;
    let environment = query_environment(workspace.path(), 73)?;
    let limits = RepositoryQueryLimits::new(16, 32, 128, 256, 16, 128, 256)?;
    let listed = environment
        .query_repository(
            RepositoryQuery::glob(".", "absent/directory/*.md", limits)?,
            control(),
        )
        .await?;
    assert_eq!(listed.matches(), []);
    assert!(!listed.incomplete());
    Ok(())
}

/// A bound noticed after the child has already run is not a no-effect refusal.
///
/// `ResourceLimit` defaults to no-effect because most bounds are checked on the
/// way in. The output bound is not: it is noticed once the child has produced
/// more than the environment allows, which means it ran. A classifier that
/// reads the category alone would tell the model nothing happened.
#[tokio::test]
async fn an_output_bound_noticed_after_the_child_ran_is_not_a_no_effect_refusal()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(74)?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits(64, 64, 64, 64)?, [], [])?,
    )?;
    let overflowed = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                "/bin/sh",
                [
                    String::from("-c"),
                    // Writes the marker first, then overflows the bound: the
                    // effect is real and already done.
                    String::from("printf executed > process-marker; printf '%0.sx' $(seq 1 4096)"),
                ],
                ".",
                [],
                LiteralProcessIntent::new(true, false),
            )?,
            control(),
        )
        .await;
    let error = overflowed
        .err()
        .ok_or("the output bound did not refuse an overflowing child")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::ResourceLimit);
    assert!(
        !error.proven_no_effect(),
        "an output bound noticed after the child ran claimed no effect"
    );
    // The category still defaults the other way, which is why the raise site
    // has to say so.
    assert!(ExecutionBackendErrorKind::ResourceLimit.proven_no_effect());
    assert!(workspace.path().join("process-marker").exists());
    Ok(())
}

#[tokio::test]
async fn repository_queries_honour_gitignore_scan_by_matches_and_glob_from_the_root()
-> Result<(), TestError> {
    let workspace = tempfile::tempdir()?;
    let root = workspace.path();
    fs::create_dir_all(root.join("target/debug"))?;
    fs::create_dir_all(root.join("crates/foo/src"))?;
    fs::create_dir_all(root.join(".github/workflows"))?;
    fs::create_dir_all(root.join(".git"))?;
    fs::write(root.join(".gitignore"), "target/\n*.log\n!keep.log\n")?;
    fs::write(root.join("crates/foo/.gitignore"), "generated.rs\n")?;
    // Thousands of ignored build files come first in walk order and would
    // exhaust a walk that did not prune them.
    for index in 0..3_000 {
        fs::write(root.join(format!("target/debug/{index:04}.d")), "NEEDLE\n")?;
    }
    fs::write(root.join("crates/foo/src/lib.rs"), "fn needle() {}\n")?;
    fs::write(root.join("crates/foo/src/generated.rs"), "fn needle() {}\n")?;
    fs::write(root.join(".github/workflows/ci.yml"), "needle: true\n")?;
    fs::write(root.join(".git/config"), "needle\n")?;
    fs::write(root.join("noise.log"), "needle\n")?;
    fs::write(root.join("keep.log"), "needle\n")?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id(61)?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let limits = RepositoryQueryLimits::new(10_000, 10_000, 1 << 20, 16 << 20, 64, 4_096, 65_536)?;
    let found = environment
        .query_repository(
            RepositoryQuery::regex(".", "need.e", RepositoryMatchCase::Sensitive, limits)?,
            control(),
        )
        .await?;
    let paths = found
        .matches()
        .iter()
        .map(|found| found.path().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            ".github/workflows/ci.yml",
            "crates/foo/src/lib.rs",
            "keep.log"
        ]
    );
    assert!(!found.incomplete());

    // A search rooted in a subdirectory still honours the root's rules, and a
    // glob with `/` is relative to that root.
    let scoped = environment
        .query_repository(
            RepositoryQuery::glob("crates/foo", "src/*.rs", limits)?,
            control(),
        )
        .await?;
    let paths = scoped
        .matches()
        .iter()
        .map(|found| found.path().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["crates/foo/src/lib.rs"]);

    // Character classes are glob syntax.
    let classes = environment
        .query_repository(RepositoryQuery::glob(".", "[kn]*.log", limits)?, control())
        .await?;
    assert_eq!(classes.matches().len(), 1);

    // A regex stops on matches, not on a file count.
    let capped = environment
        .query_repository(
            RepositoryQuery::regex(
                ".",
                "needle",
                RepositoryMatchCase::AsciiInsensitive,
                RepositoryQueryLimits::new(10_000, 10_000, 1 << 20, 16 << 20, 1, 4_096, 65_536)?,
            )?,
            control(),
        )
        .await?;
    assert_eq!(capped.matches().len(), 1);
    assert!(capped.incomplete());
    Ok(())
}

#[tokio::test]
async fn host_repository_discovery_ceiling_is_reconstructive_and_preserves_path()
-> Result<(), TestError> {
    let launch = tempfile::tempdir()?;
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(launch.path())
            .status()?
            .success()
    );
    let copy = launch.path().join("copies/child");
    fs::create_dir_all(copy.join("sub"))?;
    let make = |options| -> Result<ExecutionEnvironment, TestError> {
        Ok(ExecutionEnvironment::host_with_policy_and_process_options(
            ExecutionEnvironmentScope::Run(run_id(92)?),
            plakous_tools::WorkspacePolicy::host(&copy, ".")?,
            ExecutionEnvironmentConfig::default(),
            options,
        )?)
    };
    let plain = make(plakous_tools::HostProcessOptions::default())?;
    let fenced = make(plakous_tools::HostProcessOptions::default().with_discovery_ceiling(true))?;
    assert_ne!(plain.descriptor().digest(), fenced.descriptor().digest());
    let encoded = serde_json::to_string(fenced.descriptor())?;
    let descriptor: plakous_tools::ExecutionEnvironmentDescriptor = serde_json::from_str(&encoded)?;
    let rebuilt = ExecutionEnvironment::host_with_policy(
        descriptor.scope(),
        plakous_tools::WorkspacePolicy::host(&copy, ".")?,
        descriptor.config().clone(),
    )?;
    assert!(rebuilt.matches_descriptor(&descriptor));
    for (index, environment) in [&plain, &fenced, &rebuilt].into_iter().enumerate() {
        let result = environment
            .execute_literal_process(
                LiteralProcessRequest::new(
                    "/usr/bin/git",
                    ["rev-parse".to_owned(), "--show-toplevel".to_owned()],
                    "sub",
                    [],
                    LiteralProcessIntent::default(),
                )?,
                control(),
            )
            .await?;
        assert_eq!(result.exit_code() == Some(0), index == 0);
        let path = environment
            .execute_literal_process(
                LiteralProcessRequest::new(
                    "/bin/sh",
                    ["-c".to_owned(), "command -v git".to_owned()],
                    ".",
                    [],
                    LiteralProcessIntent::default(),
                )?,
                control(),
            )
            .await?;
        assert_eq!(path.exit_code(), Some(0));
        if index != 0 {
            let sessions = environment.joined_process_sessions()?;
            for mode in [
                JoinedProcessTerminalMode::Pipes,
                JoinedProcessTerminalMode::UnixPty,
            ] {
                let profile = JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")])?;
                let request = JoinedProcessStartRequest::new(
                    JoinedProcessCommand::shell(profile, "/usr/bin/git rev-parse --show-toplevel")?,
                    "sub",
                    [],
                    false,
                    mode,
                )?;
                let id = sessions.start(request, control())?;
                let (_, status) = collect_joined_output(&sessions, id).await?;
                assert_eq!(status, JoinedProcessStatus::Exited(Some(128)));
                sessions.terminate(id)?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn cargo_package_in_a_discovery_ceiling_copy_lists_its_own_source() -> Result<(), TestError> {
    let launch = tempfile::tempdir()?;
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(launch.path())
            .status()?
            .success()
    );
    fs::write(launch.path().join(".gitignore"), "copies/\n")?;
    fs::write(launch.path().join("launch-only"), "not the copy")?;
    assert!(
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(launch.path())
            .status()?
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "seed"
            ])
            .current_dir(launch.path())
            .status()?
            .success()
    );
    let copy = launch.path().join("copies/child");
    fs::create_dir_all(copy.join("src"))?;
    fs::write(
        copy.join("Cargo.toml"),
        "[package]\nname='discovery-ceiling-fixture'\nversion='0.1.0'\nedition='2024'\n",
    )?;
    fs::write(
        copy.join("src/lib.rs"),
        "pub const COPY_SOURCE: bool = true;\n",
    )?;
    let before = fs::read(launch.path().join(".git/index"))?;
    let environment = ExecutionEnvironment::host_with_policy_and_process_options(
        ExecutionEnvironmentScope::Run(run_id(93)?),
        plakous_tools::WorkspacePolicy::host(&copy, ".")?,
        ExecutionEnvironmentConfig::default(),
        plakous_tools::HostProcessOptions::default().with_discovery_ceiling(true),
    )?;
    let result = environment
        .execute_literal_process(
            LiteralProcessRequest::new(
                // The toolchain's own cargo: a rustup proxy needs a default
                // toolchain the filtered child environment may not carry.
                env!("CARGO"),
                [
                    "package",
                    "--list",
                    "--allow-dirty",
                    "--no-verify",
                    "--offline",
                ]
                .map(str::to_owned),
                ".",
                [],
                LiteralProcessIntent::default(),
            )?,
            control(),
        )
        .await?;
    assert_eq!(
        result.exit_code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(result.stderr())
    );
    let listed = String::from_utf8_lossy(result.stdout());
    assert!(listed.lines().any(|path| path == "src/lib.rs"), "{listed}");
    assert!(!listed.contains("launch-only"), "{listed}");
    assert_eq!(fs::read(launch.path().join(".git/index"))?, before);
    Ok(())
}
