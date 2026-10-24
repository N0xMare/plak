//! Owned literal and joined process resources, supervision and cleanup.

mod cleanup;
mod host_context;
mod output_reader;
mod sessions;
mod start;
mod supervise;

pub(super) use cleanup::process_group_exists;
use cleanup::{
    GroupProbe, cleanup_pty_setup_error, cleanup_setup_error, join_thread_until,
    observe_leftover_group, peek_leader_exit, terminate_direct_child, terminate_direct_pty_child,
    terminate_owned_process_group, terminate_owned_process_group_until,
    terminate_owned_process_group_with_grace, terminate_owned_pty_group_until,
    terminate_owned_pty_group_with_grace, wait_for_output_readers_until,
};
pub(in crate::environment) use host_context::HostProcessContext;
use host_context::restore_path_after_login;
pub(super) use host_context::{
    execute_host_literal_process, host_process_home, host_process_path,
    resolve_host_process_program,
};
use output_reader::{
    set_nonblocking, spawn_joined_output_reader, spawn_output_reader, write_process_input,
};
pub(in crate::environment) use sessions::JoinedProcessServiceCache;
use sessions::{
    FOREGROUND, HostProcessOutput, HostProcessOwner, HostProcessStop, HostProcessStream,
    JoinedSession, JoinedSessionInput, JoinedSessionOutput, JoinedSessionShared, LeftoverJob,
    PendingJobOutput, cancel_foreground, displayed_command,
};
pub use sessions::{JoinedProcessHostCapacity, JoinedProcessSessions};
#[cfg(test)]
use start::joined_pty_command;
use start::{
    PtyInputCommand, PtyInputSender, close_joined_session_input, start_joined_host_process,
    write_joined_session_input,
};
pub(super) use start::{
    validate_absolute_program, validate_process_value, validate_process_values,
};
#[cfg(test)]
use supervise::{joined_process_final_status, supervision_sleep};
use supervise::{
    sleep_until_next_poll, supervise_host_process, supervise_joined_process, supervise_joined_pty,
    supervision_poll_interval,
};

use plakous_model::CancellationToken;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::fs::OFlags;
use rustix::io::Errno;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Condvar;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::time::Instant;
use std::{fmt, fs};

#[cfg(test)]
use super::ExecutionEnvironmentConfig;
use super::{
    EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionEnvironment,
    ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
    ExecutionOperationControl, ExecutionWorkspace, JOINED_PROCESS_PTY_INITIAL_COLUMNS,
    JOINED_PROCESS_PTY_INITIAL_ROWS, JOINED_PROCESS_SERVICE_GENERATION, JoinedProcessCommand,
    JoinedProcessCursorPollResult, JoinedProcessInputDisposition, JoinedProcessOutputCursor,
    JoinedProcessOutputRecord, JoinedProcessOutputStream, JoinedProcessPollResult,
    JoinedProcessSessionId, JoinedProcessStartRequest, JoinedProcessStatus,
    JoinedProcessTerminalMode, LiteralProcessResult, MAX_EXECUTION_VALUE_BYTES,
    MAX_JOINED_PROCESS_SESSION_POLL_RECORDS, MAX_JOINED_PROCESS_SESSION_QUEUE,
    MAX_JOINED_PROCESS_SESSION_WRITE_BYTES, MAX_JOINED_PROCESS_SESSIONS,
    MAX_LITERAL_PROCESS_ARGUMENTS, MAX_LITERAL_PROCESS_STDIN_BYTES, PreparedLiteralProcess,
    backend_error, cleanup_unresolved_error, conflict_error, invalid_request, io_error, lock,
    recovery_required_error, resolve_existing_no_symlink, resource_error, workspace_error,
};

#[cfg(test)]
mod cleanup_contention_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "process policy regression assertions"
)]
mod process_policy_tests {
    use super::*;

    #[test]
    fn direct_pty_uses_revalidated_alias_as_literal_program_name() {
        let temporary = tempfile::tempdir().expect("workspace");
        let startup = temporary.path().join("startup");
        fs::write(&startup, "printf unsafe; exit 99").expect("startup trap");
        // The direct host sees the enrolled alias; shell metacharacters in its
        // path are literal executable-name data, never launcher code.
        let alias = temporary.path().join("not mounted; $(exit 98)/sh");
        let program = Path::new("/bin/bash").canonicalize().expect("bash");
        fs::create_dir(alias.parent().expect("alias parent")).expect("alias directory");
        std::os::unix::fs::symlink(&program, &alias).expect("shell alias");
        let builder = joined_pty_command(
            &program,
            Some(&alias),
            [
                "-c",
                "case :$SHELLOPTS: in *:posix:*) printf '%s' \"$0\";; *) exit 97;; esac",
            ],
        );
        let argv = builder.get_argv();
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .env("BASH_ENV", &startup)
            .env("ENV", &startup)
            .output()
            .expect("exec launcher");
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            output.stdout,
            alias.to_str().expect("alias text").as_bytes()
        );
        assert_eq!(output.stderr, [] as [u8; 0]);
    }

    #[test]
    fn login_profile_cannot_unset_the_discovery_ceiling() {
        let root = tempfile::tempdir().expect("profile root");
        let profile = root.path().join("profile");
        fs::write(&profile, "unset GIT_CEILING_DIRECTORIES\n").expect("profile");
        let process = HostProcessContext {
            fallback_path: "/usr/bin:/bin".into(),
            home: None,
            prefix: None,
            discovery_ceiling: Some(root.path().to_path_buf()),
        };
        let wrapped =
            restore_path_after_login(&process, "printf '%s' \"$GIT_CEILING_DIRECTORIES\"")
                .expect("wrapper");
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                &format!(". \"$1\";\n{wrapped}"),
                "profile",
                profile.to_str().expect("path"),
            ])
            .output()
            .expect("login profile");
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            root.path().to_str().expect("root").as_bytes()
        );
    }

    #[test]
    fn prepared_path_uses_snapshot_and_nonempty_delegation_precedence() {
        let process = HostProcessContext {
            fallback_path: "/captured/bin".into(),
            home: Some("/captured/home".into()),
            prefix: None,
            discovery_ceiling: None,
        };
        let name = EnvironmentVariableName::new("PATH").expect("PATH");
        assert_eq!(
            host_process_path(&process, &[]).expect("fallback"),
            "/captured/bin"
        );
        assert_eq!(
            host_process_path(
                &process,
                &[(name.clone(), EnvironmentVariableValue("".into()))]
            )
            .expect("empty delegation"),
            "/captured/bin"
        );
        assert_eq!(
            host_process_path(
                &process,
                &[(name, EnvironmentVariableValue("/delegated/bin".into()))]
            )
            .expect("delegation"),
            "/delegated/bin"
        );
        assert_eq!(
            restore_path_after_login(&process, "printf plain").expect("no prefix"),
            "printf plain"
        );
    }

    #[test]
    fn prepared_home_prefers_a_curated_value_over_the_captured_host_home() {
        let process = HostProcessContext {
            fallback_path: "/captured/bin".into(),
            home: Some("/captured/home".into()),
            prefix: None,
            discovery_ceiling: None,
        };
        let name = EnvironmentVariableName::new("HOME").expect("HOME");
        assert_eq!(
            host_process_home(&process, &[]),
            Some(Path::new("/captured/home"))
        );
        assert_eq!(
            host_process_home(
                &process,
                &[(name.clone(), EnvironmentVariableValue("".into()))]
            ),
            Some(Path::new("/captured/home"))
        );
        assert_eq!(
            host_process_home(
                &process,
                &[(name, EnvironmentVariableValue("/delegated/home".into()))]
            ),
            None
        );
        let homeless = HostProcessContext {
            fallback_path: "/captured/bin".into(),
            home: None,
            prefix: None,
            discovery_ceiling: None,
        };
        assert_eq!(host_process_home(&homeless, &[]), None);
    }

    #[test]
    fn program_lookup_skips_a_non_executable_file_earlier_on_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let shadow = root.path().join("shadow");
        let real = root.path().join("real");
        fs::create_dir(&shadow).expect("shadow dir");
        fs::create_dir(&real).expect("real dir");
        fs::write(shadow.join("tool"), "# sourced, not run\n").expect("shadow file");
        fs::set_permissions(shadow.join("tool"), fs::Permissions::from_mode(0o644))
            .expect("shadow mode");
        fs::write(real.join("tool"), "#!/bin/sh\n").expect("real file");
        fs::set_permissions(real.join("tool"), fs::Permissions::from_mode(0o755))
            .expect("real mode");
        let process = HostProcessContext {
            fallback_path: format!("{}:{}", shadow.display(), real.display()),
            home: None,
            prefix: None,
            discovery_ceiling: None,
        };
        assert_eq!(
            resolve_host_process_program("tool", &process, &[]).expect("resolved"),
            real.join("tool")
        );
        let only_shadow = HostProcessContext {
            fallback_path: shadow.display().to_string(),
            home: None,
            prefix: None,
            discovery_ceiling: None,
        };
        assert!(resolve_host_process_program("tool", &only_shadow, &[]).is_err());
    }
}

#[cfg(test)]
mod job_tests;

/// Retries `join_all` for a few of this service's cleanup windows.
///
/// Joins run concurrently under one window. A busy operation stays active and
/// the drain reports `CleanupUnresolved`; that is a retry, not one window per
/// admitted session.
#[cfg(test)]
pub(super) fn join_until_drained(
    sessions: &JoinedProcessSessions,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + sessions.cleanup_duration().saturating_mul(3);
    loop {
        match sessions.join_all() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ExecutionBackendErrorKind::CleanupUnresolved => {
                assert!(!sessions.is_drained());
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err("process cleanup did not converge across bounded retries".into())
}

#[cfg(test)]
mod supervision_poll_tests {
    use super::{supervision_poll_interval, supervision_sleep};
    use std::time::Duration;

    #[test]
    fn bounded_waits_back_off_but_never_sleep_past_their_deadline() {
        let long = Duration::from_secs(5);
        assert_eq!(
            supervision_sleep(Duration::ZERO, long),
            Duration::from_millis(2)
        );
        assert_eq!(supervision_sleep(long, long), Duration::from_millis(32));
        assert_eq!(
            supervision_sleep(long, Duration::from_millis(5)),
            Duration::from_millis(5)
        );
        assert_eq!(supervision_sleep(long, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn long_lived_children_are_polled_slowly_and_young_ones_promptly() {
        assert_eq!(
            supervision_poll_interval(Duration::ZERO),
            Duration::from_millis(2)
        );
        assert_eq!(
            supervision_poll_interval(Duration::from_millis(500)),
            Duration::from_millis(8)
        );
        assert_eq!(
            supervision_poll_interval(Duration::from_secs(36_000)),
            Duration::from_millis(32)
        );
    }
}
