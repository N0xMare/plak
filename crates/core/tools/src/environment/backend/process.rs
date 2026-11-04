//! Owned literal and joined process resources, supervision and cleanup.

#[cfg(unix)]
mod cleanup;
#[cfg(unix)]
mod host_context;
#[cfg(unix)]
mod output_reader;
mod sessions;
#[cfg(unix)]
mod start;
#[cfg(unix)]
mod supervise;
#[cfg(not(unix))]
mod unsupported;
mod validate;

#[cfg(unix)]
pub(super) use cleanup::{GroupProbe, leader_exited, reap_when_group_empty};
#[cfg(unix)]
use cleanup::{
    cleanup_setup_error, join_thread_until, observe_leftover_group, peek_leader_exit,
    terminate_direct_child, terminate_direct_pty_child, terminate_owned_process_group,
    terminate_owned_process_group_until, terminate_owned_process_group_with_grace,
    terminate_owned_pty_group_with_grace, wait_for_output_readers_until,
};
#[cfg(unix)]
pub(in crate::environment) use host_context::HostProcessContext;
#[cfg(unix)]
use host_context::restore_path_after_login;
#[cfg(unix)]
pub(super) use host_context::{
    execute_host_literal_process, host_process_home, host_process_path,
    resolve_host_process_program,
};
#[cfg(unix)]
use output_reader::{
    set_nonblocking, spawn_joined_output_reader, spawn_output_reader, write_process_input,
};
pub(in crate::environment) use sessions::JoinedProcessServiceCache;
#[cfg(unix)]
use sessions::{
    FOREGROUND, JoinedSessionOutput, JoinedSessionShared, LeftoverJob, cancel_foreground,
    displayed_command,
};
pub use sessions::{JoinedProcessHostCapacity, JoinedProcessSessions};
use sessions::{JoinedSession, PendingJobOutput};
#[cfg(test)]
use start::joined_pty_command;
#[cfg(unix)]
use start::{
    JoinedSessionInput, close_joined_session_input, start_joined_host_process,
    write_joined_session_input,
};
#[cfg(unix)]
use supervise::{
    HostProcessOutput, HostProcessOwner, HostProcessStream, sleep_until_next_poll,
    supervise_host_process, supervise_joined_process, supervision_poll_interval,
};
#[cfg(test)]
use supervise::{joined_process_final_status, supervision_sleep};
#[cfg(not(unix))]
pub(in crate::environment) use unsupported::{HostProcessContext, host_execution_backend};
#[cfg(not(unix))]
use unsupported::{
    HostProcessOwner, JoinedSessionInput, close_joined_session_input, start_joined_host_process,
    write_joined_session_input,
};
pub(super) use validate::{
    validate_absolute_program, validate_process_value, validate_process_values,
};

use plakous_model::CancellationToken;
#[cfg(unix)]
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
#[cfg(unix)]
use rustix::fs::OFlags;
#[cfg(unix)]
use rustix::io::Errno;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{Read, Write};
use std::num::NonZeroU64;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::Condvar;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
#[cfg(unix)]
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::time::Instant;

#[cfg(test)]
use super::ExecutionEnvironmentConfig;
use super::{
    EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentError,
    ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits, ExecutionWorkspace,
    JOINED_PROCESS_SERVICE_GENERATION, JoinedProcessCommand, JoinedProcessCursorPollResult,
    JoinedProcessInputDisposition, JoinedProcessOutputCursor, JoinedProcessOutputRecord,
    JoinedProcessOutputStream, JoinedProcessPollResult, JoinedProcessSessionId,
    JoinedProcessStartRequest, JoinedProcessStatus, MAX_EXECUTION_VALUE_BYTES,
    MAX_JOINED_PROCESS_SESSION_POLL_RECORDS, MAX_JOINED_PROCESS_SESSION_QUEUE,
    MAX_JOINED_PROCESS_SESSION_WRITE_BYTES, MAX_JOINED_PROCESS_SESSIONS,
    MAX_LITERAL_PROCESS_ARGUMENTS, MAX_LITERAL_PROCESS_STDIN_BYTES, OperationContext,
    backend_error, cleanup_unresolved_error, conflict_error, invalid_request, io_error, lock,
    recovery_required_error, resource_error,
};
#[cfg(unix)]
use super::{
    ExecutionBackendFuture, JOINED_PROCESS_PTY_INITIAL_COLUMNS, JOINED_PROCESS_PTY_INITIAL_ROWS,
    JoinedProcessTerminalMode, LiteralProcessResult, PreparedLiteralProcess,
    resolve_existing_no_symlink, workspace_error,
};

#[cfg(test)]
mod cleanup_contention_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "process policy regression assertions"
)]
mod process_policy_tests;

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
