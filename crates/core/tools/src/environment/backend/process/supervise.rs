//! Supervision polls and the joined-process wait loops.

use std::os::unix::process::CommandExt;

use super::start::JoinedProcessResources;

use super::{
    Arc, AtomicBool, AtomicU8, AtomicU64, Command, Duration, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionEnvironmentLimits, ExecutionOperationControl, GroupProbe,
    HostProcessContext, HostProcessOutput, HostProcessStop, HostProcessStream, Instant,
    JoinedProcessStatus, JoinedSessionShared, LiteralProcessResult, Mutex, Ordering, Path, PathBuf,
    PreparedLiteralProcess, Stdio, backend_error, cancel_foreground, cleanup_setup_error,
    host_process_home, host_process_path, io_error, lock, observe_leftover_group, peek_leader_exit,
    resource_error, set_nonblocking, spawn_output_reader, terminate_direct_child,
    terminate_owned_process_group, terminate_owned_process_group_until,
    wait_for_output_readers_until, write_process_input,
};

/// How long a supervisor sleeps between checks of a live child. A young child
/// is checked every 2 ms so short commands keep their exit latency; a child
/// past one second is checked every 32 ms, so a long-lived job wakes about 30
/// times a second rather than 500 for its whole life.
pub(super) fn supervision_poll_interval(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_millis(100) {
        Duration::from_millis(2)
    } else if elapsed < Duration::from_secs(1) {
        Duration::from_millis(8)
    } else {
        Duration::from_millis(32)
    }
}

/// One backoff step of a wait that has lasted `elapsed`, cut short so that it
/// never sleeps past a deadline `remaining` away.
pub(super) fn supervision_sleep(elapsed: Duration, remaining: Duration) -> Duration {
    supervision_poll_interval(elapsed).min(remaining)
}

pub(super) fn sleep_until_next_poll(started: Instant, deadline: Instant) {
    std::thread::sleep(supervision_sleep(
        started.elapsed(),
        deadline.saturating_duration_since(Instant::now()),
    ));
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared supervisor owns the terminal strategy, limits, cancellation and publication inputs"
)]
pub(super) fn supervise_joined_process(
    resources: &mut JoinedProcessResources,
    limits: ExecutionEnvironmentLimits,
    control: &ExecutionOperationControl,
    promoted: &AtomicU8,
    stop_grace_millis: &AtomicU64,
    stopped: &AtomicBool,
    output_io_failed: &AtomicBool,
    shared: &Mutex<JoinedSessionShared>,
    shell_command: bool,
) {
    let started = Instant::now();
    let mut group = GroupProbe::new(resources.process_group());
    let mut disowned = false;
    let reason = loop {
        if stopped.load(Ordering::Acquire) {
            break JoinedProcessStatus::Terminated;
        }
        if lock(shared).disowned {
            disowned = true;
            break JoinedProcessStatus::OutcomeUnknown;
        }
        if cancel_foreground(promoted, control) {
            break JoinedProcessStatus::Cancelled;
        }
        if output_io_failed.load(Ordering::Acquire) {
            break JoinedProcessStatus::Failed;
        }
        if started.elapsed() >= limits.session_duration() {
            break JoinedProcessStatus::TimedOut;
        }
        match peek_leader_exit(resources.process_group()) {
            Ok(Some(exit)) => {
                if shell_command && observe_leftover_group(&mut group, exit.code, shared) {
                    std::thread::sleep(supervision_poll_interval(started.elapsed()));
                    continue;
                }
                match resources.try_wait() {
                    Ok(JoinedProcessStatus::Exited(code)) => {
                        break JoinedProcessStatus::Exited(code);
                    }
                    Ok(_) => {}
                    Err(()) => break JoinedProcessStatus::Failed,
                }
            }
            Ok(None) => std::thread::sleep(supervision_poll_interval(started.elapsed())),
            Err(_) => break JoinedProcessStatus::Failed,
        }
    };
    let deadline = Instant::now() + limits.cleanup_duration();
    let mut cleanup = if disowned {
        Ok(())
    } else {
        resources.terminate(
            deadline,
            Duration::from_millis(stop_grace_millis.load(Ordering::Acquire)),
        )
    };
    if !disowned && matches!(reason, JoinedProcessStatus::Exited(_)) && cleanup.is_ok() {
        resources.drain_readers_until(deadline);
    }
    resources.stop_io();
    let workers_joined = resources.join_workers_until(deadline);
    if disowned {
        cleanup = if workers_joined && !output_io_failed.load(Ordering::Acquire) {
            resources
                .try_wait()
                .map(|_| ())
                .map_err(|()| io_error("released process leader could not be reaped"))
        } else {
            resources.terminate(deadline, Duration::from_millis(200))
        };
    }
    let final_status = joined_process_final_status(
        reason,
        cleanup.is_ok(),
        workers_joined,
        output_io_failed.load(Ordering::Acquire),
    );
    let mut shared = lock(shared);
    shared.status = final_status;
    shared.elapsed = Some(started.elapsed());
    shared.terminal_changed();
}

pub(super) const fn joined_process_final_status(
    reason: JoinedProcessStatus,
    cleanup_resolved: bool,
    workers_joined: bool,
    output_io_failed: bool,
) -> JoinedProcessStatus {
    if !cleanup_resolved || !workers_joined {
        JoinedProcessStatus::CleanupUnresolved
    } else if output_io_failed {
        JoinedProcessStatus::Failed
    } else {
        reason
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "process setup rollback and joined cleanup are kept in one auditable supervisor"
)]
pub(super) fn supervise_host_process(
    process: &HostProcessContext,
    cwd: PathBuf,
    program: &Path,
    request: &PreparedLiteralProcess,
    limits: ExecutionEnvironmentLimits,
    control: &ExecutionOperationControl,
    dropped: &AtomicBool,
) -> Result<LiteralProcessResult, ExecutionBackendError> {
    let started = Instant::now();
    let mut command = Command::new(program);
    command.args(request.request.arguments());
    command
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // Host processes intentionally clear the ambient environment. Inject a
    // minimal PATH so child processes inherit a usable default lookup path.
    // Full host credential/env inheritance remains opt-in via delegations.
    // The explicit required workspace prefix is revalidated before spawn.
    for (name, value) in request.environment.iter() {
        if name.as_str() == "PATH" {
            continue;
        }
        command.env(name.as_str(), value.0.as_ref());
    }
    command.env(
        "PATH",
        host_process_path(process, request.environment.as_ref())?,
    );
    if let Some(ceiling) = &process.discovery_ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    if let Some(home) = host_process_home(process, request.environment.as_ref()) {
        command.env("HOME", home);
    }
    // Spawn failures never create a supervised child: map to InvalidRequest so
    // one-shot callers can surface a model-visible Failure instead of OutcomeUnknown.
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "literal process program was not found on the host PATH",
            )
        } else {
            backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "host literal process spawn failed",
            )
        }
    })?;
    let raw_pid = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io_error("host literal process returned an invalid pid"));
    let raw_pid = match raw_pid {
        Ok(raw_pid) => raw_pid,
        Err(error) => {
            return Err(
                terminate_direct_child(&mut child, limits.cleanup_duration())
                    .err()
                    .unwrap_or(error),
            );
        }
    };
    let Some(stdin) = child.stdin.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            io_error("host literal process stdin was unavailable"),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            io_error("host literal process stdout was unavailable"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            io_error("host literal process stderr was unavailable"),
        ));
    };
    if let Err(error) = set_nonblocking(&stdin) {
        return Err(cleanup_setup_error(
            &mut child,
            raw_pid,
            limits.cleanup_duration(),
            error,
        ));
    }

    let output = Arc::new(Mutex::new(HostProcessOutput::default()));
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let output_io_failed = Arc::new(AtomicBool::new(false));
    let io_stop = Arc::new(AtomicBool::new(false));
    let stdout_thread = spawn_output_reader(
        stdout,
        HostProcessStream::Stdout,
        Arc::clone(&output),
        Arc::clone(&output_exceeded),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    );
    let stdout_thread = match stdout_thread {
        Ok(thread) => thread,
        Err(error) => {
            return Err(cleanup_setup_error(
                &mut child,
                raw_pid,
                limits.cleanup_duration(),
                error,
            ));
        }
    };
    let stderr_thread = spawn_output_reader(
        stderr,
        HostProcessStream::Stderr,
        Arc::clone(&output),
        Arc::clone(&output_exceeded),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    );
    let stderr_thread = match stderr_thread {
        Ok(thread) => thread,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let cleanup =
                terminate_owned_process_group(&mut child, raw_pid, None, limits.cleanup_duration());
            let joined = stdout_thread.join();
            return Err(match (cleanup, joined) {
                (Err(cleanup), _) => cleanup,
                (Ok(_), Err(_)) => io_error("host process output worker failed during rollback"),
                (Ok(_), Ok(())) => error,
            });
        }
    };
    let stdin_bytes = Arc::clone(&request.request.stdin);
    let stdin_stop = Arc::clone(&io_stop);
    let stdin_thread = std::thread::Builder::new()
        .name(String::from("plak-process-stdin"))
        .spawn(move || {
            write_process_input(stdin, &stdin_bytes, &stdin_stop);
        })
        .map_err(|_| io_error("host process stdin worker could not start"));
    let stdin_thread = match stdin_thread {
        Ok(thread) => thread,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let cleanup =
                terminate_owned_process_group(&mut child, raw_pid, None, limits.cleanup_duration());
            let stdout_joined = stdout_thread.join();
            let stderr_joined = stderr_thread.join();
            return Err(if let Err(cleanup) = cleanup {
                cleanup
            } else if stdout_joined.is_err() || stderr_joined.is_err() {
                io_error("host process output worker failed during rollback")
            } else {
                error
            });
        }
    };

    let mut terminal_status = None;
    let stop_reason = loop {
        if dropped.load(Ordering::Acquire) || control.cancellation().is_cancelled() {
            break HostProcessStop::Cancelled;
        }
        if output_exceeded.load(Ordering::Acquire) {
            break HostProcessStop::OutputLimit;
        }
        if output_io_failed.load(Ordering::Acquire) {
            break HostProcessStop::Io;
        }
        if started.elapsed() >= limits.execution_duration() {
            break HostProcessStop::TimedOut;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                terminal_status = Some(status);
                break HostProcessStop::Completed;
            }
            Ok(None) => std::thread::sleep(supervision_poll_interval(started.elapsed())),
            Err(_) => break HostProcessStop::Io,
        }
    };

    let cleanup_deadline = Instant::now() + limits.cleanup_duration();
    let cleanup_result =
        terminate_owned_process_group_until(&mut child, raw_pid, terminal_status, cleanup_deadline);
    if stop_reason == HostProcessStop::Completed && cleanup_result.is_ok() {
        // Preserve all bounded output from a normally completed owned group.
        // An escaped descendant may keep the pipes so this EOF drain
        // shares, rather than extends, the process cleanup deadline.
        wait_for_output_readers_until(&stdout_thread, &stderr_thread, cleanup_deadline);
    }
    io_stop.store(true, Ordering::Release);
    let stdin_joined = stdin_thread.join();
    let stdout_joined = stdout_thread.join();
    let stderr_joined = stderr_thread.join();
    terminal_status = Some(cleanup_result?);
    if stdin_joined.is_err() || stdout_joined.is_err() || stderr_joined.is_err() {
        return Err(io_error("host process I/O worker failed"));
    }

    if output_exceeded.load(Ordering::Acquire) || stop_reason == HostProcessStop::OutputLimit {
        // The child ran, and ran far enough to overflow the bound, before this
        // was noticed. Whatever it did, it did.
        return Err(
            resource_error("literal process output exceeds environment limits")
                .with_proven_no_effect(false),
        );
    }
    if output_io_failed.load(Ordering::Acquire) || stop_reason == HostProcessStop::Io {
        return Err(io_error("host process output or wait failed"));
    }
    match stop_reason {
        HostProcessStop::Cancelled => {
            return Err(backend_error(
                ExecutionBackendErrorKind::Cancelled,
                "host process group was cancelled and joined",
            ));
        }
        HostProcessStop::TimedOut => {
            return Err(backend_error(
                ExecutionBackendErrorKind::TimedOut,
                "host process execution timed out and its group was joined",
            ));
        }
        HostProcessStop::Completed => {}
        HostProcessStop::OutputLimit | HostProcessStop::Io => {
            return Err(io_error("host process stopped without a terminal result"));
        }
    }

    let status =
        terminal_status.ok_or_else(|| io_error("host process had no terminal exit status"))?;
    let collected = lock(&output);
    LiteralProcessResult::new(
        status.code(),
        collected.stdout.clone(),
        collected.stderr.clone(),
    )
}
