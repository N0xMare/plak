//! Pipe and PTY process start. The two paths stay separate.

use std::os::unix::process::CommandExt;

use super::{
    Arc, AsRawFd, AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Command, CommandBuilder, Condvar,
    Duration, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionEnvironmentLimits, ExecutionOperationControl,
    ExecutionWorkspace, FOREGROUND, HostProcessContext, HostProcessOwner, Instant,
    JOINED_PROCESS_PTY_INITIAL_COLUMNS, JOINED_PROCESS_PTY_INITIAL_ROWS, JoinedProcessCommand,
    JoinedProcessOutputCursor, JoinedProcessOutputStream, JoinedProcessStartRequest,
    JoinedProcessStatus, JoinedProcessTerminalMode, JoinedSession, JoinedSessionInput,
    JoinedSessionOutput, JoinedSessionShared, MAX_EXECUTION_VALUE_BYTES,
    MAX_LITERAL_PROCESS_ARGUMENTS, MasterPty, Mutex, Ordering, Path, PendingJobOutput, PtySize,
    RawFd, Receiver, Stdio, SyncSender, TrySendError, Write, backend_error,
    cleanup_pty_setup_error, cleanup_setup_error, cleanup_unresolved_error, conflict_error,
    displayed_command, fs, host_process_home, host_process_path, invalid_request, io_error,
    join_thread_until, lock, native_pty_system, resolve_existing_no_symlink, resource_error,
    restore_path_after_login, set_nonblocking, sleep_until_next_poll, spawn_joined_output_reader,
    supervise_joined_process, supervise_joined_pty, sync_channel, terminate_direct_child,
    terminate_direct_pty_child, terminate_owned_process_group_until,
    terminate_owned_pty_group_until, workspace_error,
};

pub(in crate::environment::backend) fn validate_absolute_program(
    executable: &Path,
    message: &'static str,
) -> Result<(), ExecutionBackendError> {
    if !executable.is_absolute() {
        return Err(invalid_request(message));
    }
    let value = executable
        .to_str()
        .ok_or_else(|| invalid_request("joined process executable is not portable UTF-8"))?;
    validate_process_value(value)
}

pub(in crate::environment::backend) fn validate_process_values(
    values: &[String],
) -> Result<(), ExecutionBackendError> {
    if values.len() > MAX_LITERAL_PROCESS_ARGUMENTS {
        return Err(resource_error(
            "joined process argument count exceeds its bound",
        ));
    }
    values
        .iter()
        .try_for_each(|value| validate_process_value(value))
}

pub(in crate::environment::backend) fn validate_process_value(
    value: &str,
) -> Result<(), ExecutionBackendError> {
    if value.len() > MAX_EXECUTION_VALUE_BYTES || value.as_bytes().contains(&0) {
        return Err(invalid_request("joined process literal value is invalid"));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "spawn setup and every rollback edge stay with the single joined cleanup owner"
)]
pub(super) fn start_joined_host_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: ExecutionOperationControl,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    match request.terminal_mode {
        JoinedProcessTerminalMode::Pipes => start_joined_pipes_process(
            workspace,
            process,
            environment,
            limits,
            queue_limit,
            request,
            control,
            pending_spool,
        ),
        JoinedProcessTerminalMode::UnixPty => start_joined_pty_process(
            workspace,
            process,
            environment,
            limits,
            queue_limit,
            request,
            control,
            pending_spool,
        ),
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "spawn setup and every rollback edge stay with the single joined cleanup owner"
)]
fn start_joined_pipes_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: ExecutionOperationControl,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    let cwd = resolve_existing_no_symlink(workspace, &request.cwd)?;
    if !cwd.is_dir() {
        return Err(workspace_error(
            "joined process cwd is not a contained directory",
        ));
    }
    let wrapped_shell = match &request.command {
        JoinedProcessCommand::Shell { command, .. } => {
            Some(restore_path_after_login(process, command)?)
        }
        JoinedProcessCommand::Literal { .. } => None,
    };
    let argv0 = match &request.command {
        JoinedProcessCommand::Shell { profile, .. } => Some(profile.executable()),
        JoinedProcessCommand::Literal { .. } => None,
    };
    let (program, arguments): (&Path, Vec<&str>) = match &request.command {
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => (executable, arguments.iter().map(String::as_str).collect()),
        JoinedProcessCommand::Shell { profile, .. } => {
            let mut arguments = profile
                .arguments()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            arguments.push(wrapped_shell.as_deref().unwrap_or(""));
            (profile.revalidated_executable()?, arguments)
        }
    };
    let mut command = Command::new(program);
    command.args(arguments);
    command.arg0(argv0.unwrap_or(program));
    command
        .current_dir(cwd)
        .env_clear()
        .stdin(if request.keep_stdin_open || !request.stdin.is_empty() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (name, value) in environment {
        if name.as_str() == "PATH" {
            continue;
        }
        command.env(name.as_str(), value.0.as_ref());
    }
    command.env("PATH", host_process_path(process, environment)?);
    if let Some(ceiling) = &process.discovery_ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    if let Some(home) = host_process_home(process, environment) {
        command.env("HOME", home);
    }
    let mut child = command
        .spawn()
        .map_err(|_| invalid_request("joined process executable could not be started"))?;
    let process_group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io_error("joined process returned an invalid pid"));
    let process_group = match process_group {
        Ok(process_group) => process_group,
        Err(error) => {
            return Err(
                terminate_direct_child(&mut child, limits.cleanup_duration())
                    .err()
                    .unwrap_or(error),
            );
        }
    };
    let mut stdin = child.stdin.take();
    if (request.keep_stdin_open || !request.stdin.is_empty()) && stdin.is_none() {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            io_error("joined process stdin was unavailable"),
        ));
    }
    let Some(stdout) = child.stdout.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            io_error("joined process stdout was unavailable"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            io_error("joined process stderr was unavailable"),
        ));
    };
    if let Some(stdin) = &stdin
        && let Err(error) = set_nonblocking(stdin)
    {
        return Err(cleanup_setup_error(
            &mut child,
            process_group,
            limits.cleanup_duration(),
            error,
        ));
    }

    let shared = Arc::new(Mutex::new(JoinedSessionShared {
        output: JoinedSessionOutput::default(),
        status: JoinedProcessStatus::Running,
        elapsed: None,
        terminal_changes: None,
        changes: tokio::sync::watch::channel(0).0,
        terminal: Arc::new(Condvar::new()),
        released: false,
        spool: None,
        pending_spool,
        leftover: None,
        disowned: false,
    }));
    let output_io_failed = Arc::new(AtomicBool::new(false));
    let io_stop = Arc::new(AtomicBool::new(false));
    let stdout_thread = spawn_joined_output_reader(
        stdout,
        JoinedProcessOutputStream::Stdout,
        Arc::clone(&shared),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    )
    .map_err(|error| {
        cleanup_setup_error(&mut child, process_group, limits.cleanup_duration(), error)
    })?;
    let stderr_thread = match spawn_joined_output_reader(
        stderr,
        JoinedProcessOutputStream::Stderr,
        Arc::clone(&shared),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    ) {
        Ok(thread) => thread,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let deadline = Instant::now() + limits.cleanup_duration();
            let cleanup =
                terminate_owned_process_group_until(&mut child, process_group, None, deadline);
            let joined = join_thread_until(stdout_thread, deadline);
            return Err(match (cleanup, joined) {
                (Err(cleanup), _) => cleanup,
                (Ok(_), false) => cleanup_unresolved_error(),
                (Ok(_), true) => error,
            });
        }
    };
    let initial_input = match stdin.as_mut() {
        Some(stdin) => {
            write_joined_initial_input(stdin, &request.stdin, limits.execution_duration())
        }
        None => Ok(()),
    };
    if let Err(error) = initial_input {
        io_stop.store(true, Ordering::Release);
        let deadline = Instant::now() + limits.cleanup_duration();
        let cleanup =
            terminate_owned_process_group_until(&mut child, process_group, None, deadline);
        let joined = (
            join_thread_until(stdout_thread, deadline),
            join_thread_until(stderr_thread, deadline),
        );
        return Err(if let Err(cleanup) = cleanup {
            cleanup
        } else if !joined.0 || !joined.1 {
            cleanup_unresolved_error()
        } else {
            error
        });
    }
    let input = if request.keep_stdin_open { stdin } else { None };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let foreground_control = control.clone();
    let promoted = Arc::new(AtomicU8::new(FOREGROUND));
    let thread_promoted = Arc::clone(&promoted);
    let stop_grace_millis = Arc::new(AtomicU64::new(2000));
    let thread_stop_grace = Arc::clone(&stop_grace_millis);
    let thread_shared = Arc::clone(&shared);
    let shell_command = matches!(request.command, JoinedProcessCommand::Shell { .. });
    let resources = JoinedProcessResources {
        child,
        process_group,
        stdout_thread: Some(stdout_thread),
        stderr_thread: Some(stderr_thread),
        io_stop,
        cleanup_duration: limits.cleanup_duration(),
        armed: true,
    };
    let thread = std::thread::Builder::new()
        .name(String::from("plak-joined-process"))
        .spawn(move || {
            let mut resources = resources;
            let (Some(stdout_thread), Some(stderr_thread)) = (
                resources.stdout_thread.take(),
                resources.stderr_thread.take(),
            ) else {
                let mut shared = lock(&thread_shared);
                shared.status = JoinedProcessStatus::Failed;
                shared.terminal_changed();
                return;
            };
            let process_group = resources.process_group;
            supervise_joined_process(
                &mut resources.child,
                process_group,
                stdout_thread,
                stderr_thread,
                limits,
                &control,
                &thread_promoted,
                &thread_stop_grace,
                &thread_stop,
                &resources.io_stop,
                &output_io_failed,
                &thread_shared,
                shell_command,
            );
            resources.armed = false;
        })
        .map_err(|_| io_error("joined process supervisor could not start"));
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => {
            stop.store(true, Ordering::Release);
            // Dropping the rejected closure runs JoinedProcessResources'
            // rollback owner, which terminates/reaps and joins both readers.
            return Err(error);
        }
    };
    Ok(JoinedSession {
        operation: Mutex::new(()),
        queued: AtomicUsize::new(0),
        queue_limit,
        input: Mutex::new(input.map(JoinedSessionInput::Pipes)),
        input_bytes: AtomicUsize::new(request.stdin.len()),
        input_timeout: limits.execution_duration(),
        cleanup_duration: limits.cleanup_duration(),
        shared,
        consumer_cursor: Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
        consumer_pending_utf8: Arc::new(Mutex::new(Vec::new())),
        consumer_quiet_polls: Arc::default(),
        consumed: AtomicBool::new(false),
        promoted,
        foreground_control,
        stop_grace_millis,
        command: displayed_command(&request.command),
        started_at: plakous_model::Timestamp::new(plakous_model::now_unix_secs(), 0)
            .map_err(|_| invalid_request("process start timestamp is invalid"))?,
        started: Instant::now(),
        ended: Mutex::new(None),
        stopped_by: Mutex::new(None),
        owner: Mutex::new(Some(HostProcessOwner {
            stop,
            thread: Some(thread),
        })),
    })
}

pub(super) struct PtyMasterDescriptor<'a> {
    pub(super) raw: RawFd,
    pub(super) _master: std::marker::PhantomData<&'a dyn MasterPty>,
}

impl AsRawFd for PtyMasterDescriptor<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.raw
    }
}

fn clone_linux_pty_master(master: &dyn MasterPty) -> Result<fs::File, ExecutionBackendError> {
    let raw = master
        .as_raw_fd()
        .ok_or_else(|| io_error("joined process PTY master identity was unavailable"))?;
    filedescriptor::FileDescriptor::dup(&PtyMasterDescriptor {
        raw,
        _master: std::marker::PhantomData,
    })
    .and_then(|descriptor| descriptor.as_file())
    .map_err(|_| io_error("joined process PTY master could not be safely duplicated"))
}

pub(super) fn joined_pty_command(
    program: &Path,
    argv0: Option<&Path>,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> CommandBuilder {
    // portable-pty couples the executable to argv[0], so the revalidated
    // enrolled alias is the program name. No extra shell is required.
    let mut command = CommandBuilder::new(argv0.unwrap_or(program));
    command.args(arguments);
    command
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "PTY spawn and every rollback edge stay with the single joined cleanup owner"
)]
fn start_joined_pty_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: ExecutionOperationControl,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    let cwd = resolve_existing_no_symlink(workspace, &request.cwd)?;
    if !cwd.is_dir() {
        return Err(workspace_error(
            "joined process cwd is not a contained directory",
        ));
    }
    let wrapped_shell = match &request.command {
        JoinedProcessCommand::Shell { command, .. } => Some(format!(
            "trap '' HUP; {}",
            restore_path_after_login(process, command)?
        )),
        JoinedProcessCommand::Literal { .. } => None,
    };
    let argv0 = match &request.command {
        JoinedProcessCommand::Shell { profile, .. } => Some(profile.executable()),
        JoinedProcessCommand::Literal { .. } => None,
    };
    let (program, arguments): (&Path, Vec<&str>) = match &request.command {
        JoinedProcessCommand::Literal {
            executable,
            arguments,
        } => (executable, arguments.iter().map(String::as_str).collect()),
        JoinedProcessCommand::Shell { profile, .. } => {
            let mut arguments = profile
                .arguments()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            arguments.push(wrapped_shell.as_deref().unwrap_or(""));
            (profile.revalidated_executable()?, arguments)
        }
    };
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: JOINED_PROCESS_PTY_INITIAL_ROWS,
            cols: JOINED_PROCESS_PTY_INITIAL_COLUMNS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|_| io_error("joined process PTY could not be opened"))?;
    let portable_pty::PtyPair { master, slave } = pair;
    let mut command = joined_pty_command(program, argv0, arguments);
    command.cwd(&cwd);
    command.env_clear();
    for (name, value) in environment {
        if name.as_str() == "PATH" {
            continue;
        }
        command.env(name.as_str(), value.0.as_ref());
    }
    command.env("PATH", host_process_path(process, environment)?);
    if let Some(ceiling) = &process.discovery_ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    if let Some(home) = host_process_home(process, environment) {
        command.env("HOME", home);
    }
    // The session's terminal is a duplicate of the master descriptor, and the
    // duplicate is what keeps the master side open. The original is dropped
    // with this function; closing one of two duplicates closes neither end.
    let terminal = clone_linux_pty_master(master.as_ref())?;
    set_nonblocking(&terminal)?;
    let reader = terminal
        .try_clone()
        .map_err(|_| io_error("joined process PTY reader was unavailable"))?;
    let writer = terminal;
    let mut child = slave
        .spawn_command(command)
        .map_err(|_| invalid_request("joined process PTY command could not be started"))?;
    drop(slave);
    let process_group = child
        .process_id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io_error("joined process PTY returned an invalid pid"));
    let process_group = match process_group {
        Ok(process_group) => process_group,
        Err(error) => {
            return Err(
                terminate_direct_pty_child(child.as_mut(), limits.cleanup_duration())
                    .err()
                    .unwrap_or(error),
            );
        }
    };
    let shared = Arc::new(Mutex::new(JoinedSessionShared {
        output: JoinedSessionOutput::default(),
        status: JoinedProcessStatus::Running,
        elapsed: None,
        terminal_changes: None,
        changes: tokio::sync::watch::channel(0).0,
        terminal: Arc::new(Condvar::new()),
        released: false,
        spool: None,
        pending_spool,
        leftover: None,
        disowned: false,
    }));
    let output_io_failed = Arc::new(AtomicBool::new(false));
    let io_stop = Arc::new(AtomicBool::new(false));
    let reader_thread = match spawn_joined_output_reader(
        reader,
        JoinedProcessOutputStream::Terminal,
        Arc::clone(&shared),
        Arc::clone(&output_io_failed),
        Arc::clone(&io_stop),
        limits,
    ) {
        Ok(thread) => thread,
        Err(error) => {
            return Err(cleanup_pty_setup_error(
                child.as_mut(),
                process_group,
                limits.cleanup_duration(),
                error,
            ));
        }
    };
    let (input, input_owner, input_thread) = match spawn_pty_input_writer(Box::new(writer)) {
        Ok(input) => input,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            let deadline = Instant::now() + limits.cleanup_duration();
            let cleanup = cleanup_pty_setup_error(
                child.as_mut(),
                process_group,
                limits.cleanup_duration(),
                error,
            );
            return Err(if join_thread_until(reader_thread, deadline) {
                cleanup
            } else {
                cleanup_unresolved_error()
            });
        }
    };
    if let Err(error) = input.write(&request.stdin, limits.execution_duration()) {
        io_stop.store(true, Ordering::Release);
        let deadline = Instant::now() + limits.cleanup_duration();
        let cleanup = cleanup_pty_setup_error(
            child.as_mut(),
            process_group,
            limits.cleanup_duration(),
            error,
        );
        let _ = input_owner.try_send(PtyInputCommand::Stop);
        let joined = (
            join_thread_until(reader_thread, deadline),
            join_thread_until(input_thread, deadline),
        );
        return Err(if !joined.0 || !joined.1 {
            cleanup_unresolved_error()
        } else {
            cleanup
        });
    }
    let input = if request.keep_stdin_open {
        Some(JoinedSessionInput::Pty(input))
    } else {
        if let Err(error) = input.close(limits.execution_duration()) {
            io_stop.store(true, Ordering::Release);
            let deadline = Instant::now() + limits.cleanup_duration();
            let cleanup = cleanup_pty_setup_error(
                child.as_mut(),
                process_group,
                limits.cleanup_duration(),
                error,
            );
            let _ = input_owner.try_send(PtyInputCommand::Stop);
            let joined = (
                join_thread_until(reader_thread, deadline),
                join_thread_until(input_thread, deadline),
            );
            return Err(if !joined.0 || !joined.1 {
                cleanup_unresolved_error()
            } else {
                cleanup
            });
        }
        None
    };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let foreground_control = control.clone();
    let promoted = Arc::new(AtomicU8::new(FOREGROUND));
    let thread_promoted = Arc::clone(&promoted);
    let stop_grace_millis = Arc::new(AtomicU64::new(2000));
    let thread_stop_grace = Arc::clone(&stop_grace_millis);
    let thread_shared = Arc::clone(&shared);
    let shell_command = matches!(request.command, JoinedProcessCommand::Shell { .. });
    let resources = JoinedPtyResources {
        child,
        process_group,
        reader_thread: Some(reader_thread),
        input_thread: Some(input_thread),
        input_owner,
        io_stop,
        cleanup_duration: limits.cleanup_duration(),
        armed: true,
    };
    let thread = std::thread::Builder::new()
        .name(String::from("plak-joined-pty"))
        .spawn(move || {
            let mut resources = resources;
            let (Some(reader_thread), Some(input_thread)) = (
                resources.reader_thread.take(),
                resources.input_thread.take(),
            ) else {
                let mut shared = lock(&thread_shared);
                shared.status = JoinedProcessStatus::Failed;
                shared.terminal_changed();
                return;
            };
            supervise_joined_pty(
                resources.child.as_mut(),
                resources.process_group,
                reader_thread,
                input_thread,
                &resources.input_owner,
                limits,
                &control,
                &thread_promoted,
                &thread_stop_grace,
                &thread_stop,
                &resources.io_stop,
                &output_io_failed,
                &thread_shared,
                shell_command,
            );
            resources.armed = false;
        })
        .map_err(|_| io_error("joined process PTY supervisor could not start"))?;
    Ok(JoinedSession {
        operation: Mutex::new(()),
        queued: AtomicUsize::new(0),
        queue_limit,
        input: Mutex::new(input),
        input_bytes: AtomicUsize::new(request.stdin.len()),
        input_timeout: limits.execution_duration(),
        cleanup_duration: limits.cleanup_duration(),
        shared,
        consumer_cursor: Arc::new(Mutex::new(JoinedProcessOutputCursor::new(0))),
        consumer_pending_utf8: Arc::new(Mutex::new(Vec::new())),
        consumer_quiet_polls: Arc::default(),
        consumed: AtomicBool::new(false),
        promoted,
        foreground_control,
        stop_grace_millis,
        command: displayed_command(&request.command),
        started_at: plakous_model::Timestamp::new(plakous_model::now_unix_secs(), 0)
            .map_err(|_| invalid_request("process start timestamp is invalid"))?,
        started: Instant::now(),
        ended: Mutex::new(None),
        stopped_by: Mutex::new(None),
        owner: Mutex::new(Some(HostProcessOwner {
            stop,
            thread: Some(thread),
        })),
    })
}

fn write_joined_initial_input(
    stdin: &mut std::process::ChildStdin,
    bytes: &[u8],
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    write_joined_input(stdin, bytes, duration)
}

fn write_joined_input(
    stdin: &mut std::process::ChildStdin,
    bytes: &[u8],
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    let deadline = Instant::now() + duration;
    let mut written = 0;
    let mut blocked_since = Instant::now();
    while written < bytes.len() {
        if Instant::now() >= deadline {
            return Err(backend_error(
                ExecutionBackendErrorKind::TimedOut,
                "joined process initial input timed out",
            ));
        }
        match stdin.write(&bytes[written..]) {
            Ok(0) => return Err(io_error("joined process stdin closed during initial input")),
            Ok(count) => {
                written = written.saturating_add(count);
                blocked_since = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                sleep_until_next_poll(blocked_since, deadline);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(io_error("joined process initial input failed")),
        }
    }
    Ok(())
}

pub(super) fn write_joined_session_input(
    input: &mut JoinedSessionInput,
    bytes: &[u8],
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    match input {
        JoinedSessionInput::Pipes(stdin) => {
            write_joined_input(stdin, bytes, duration)?;
            stdin
                .flush()
                .map_err(|_| io_error("joined process stdin flush failed"))
        }
        JoinedSessionInput::Pty(sender) => sender.write(bytes, duration),
    }
}

pub(super) fn close_joined_session_input(
    input: &mut JoinedSessionInput,
    duration: Duration,
) -> Result<(), ExecutionBackendError> {
    match input {
        JoinedSessionInput::Pipes(_) => Ok(()),
        JoinedSessionInput::Pty(sender) => sender.close(duration),
    }
}

pub(super) enum PtyInputCommand {
    Write(Vec<u8>, SyncSender<Result<(), ()>>),
    Close(SyncSender<Result<(), ()>>),
    Stop,
}

pub(super) struct PtyInputSender {
    pub(super) commands: SyncSender<PtyInputCommand>,
}

impl PtyInputSender {
    pub(super) fn write(
        &self,
        bytes: &[u8],
        duration: Duration,
    ) -> Result<(), ExecutionBackendError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let (acknowledge, acknowledged) = sync_channel(1);
        self.commands
            .try_send(PtyInputCommand::Write(bytes.to_vec(), acknowledge))
            .map_err(|error| match error {
                TrySendError::Full(_) => resource_error("joined process PTY input queue is full"),
                TrySendError::Disconnected(_) => {
                    conflict_error("joined process PTY input is closed")
                }
            })?;
        acknowledged
            .recv_timeout(duration)
            .map_err(|_| io_error("joined process PTY input did not complete within its bound"))?
            .map_err(|()| io_error("joined process PTY input failed"))
    }

    pub(super) fn close(&self, duration: Duration) -> Result<(), ExecutionBackendError> {
        let (acknowledge, acknowledged) = sync_channel(1);
        self.commands
            .try_send(PtyInputCommand::Close(acknowledge))
            .map_err(|error| match error {
                TrySendError::Full(_) => resource_error("joined process PTY input queue is full"),
                TrySendError::Disconnected(_) => {
                    conflict_error("joined process PTY input is closed")
                }
            })?;
        acknowledged
            .recv_timeout(duration)
            .map_err(|_| io_error("joined process PTY EOF did not complete within its bound"))?
            .map_err(|()| io_error("joined process PTY EOF failed"))
    }
}

fn spawn_pty_input_writer(
    mut writer: Box<dyn Write + Send>,
) -> Result<
    (
        PtyInputSender,
        SyncSender<PtyInputCommand>,
        std::thread::JoinHandle<()>,
    ),
    ExecutionBackendError,
> {
    let (commands, incoming): (SyncSender<PtyInputCommand>, Receiver<PtyInputCommand>) =
        sync_channel(1);
    let owner_commands = commands.clone();
    let thread = std::thread::Builder::new()
        .name(String::from("plak-joined-pty-input"))
        .spawn(move || {
            while let Ok(command) = incoming.recv() {
                match command {
                    PtyInputCommand::Write(bytes, acknowledge) => {
                        let result = writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .map_err(|_| ());
                        let failed = result.is_err();
                        let _ = acknowledge.send(result);
                        if failed {
                            return;
                        }
                    }
                    PtyInputCommand::Close(acknowledge) => {
                        let result = writer
                            .write_all(b"\n\x04")
                            .and_then(|()| writer.flush())
                            .map_err(|_| ());
                        let _ = acknowledge.send(result);
                        return;
                    }
                    PtyInputCommand::Stop => return,
                }
            }
        })
        .map_err(|_| io_error("joined process PTY input worker could not start"))?;
    Ok((PtyInputSender { commands }, owner_commands, thread))
}

pub(super) struct JoinedProcessResources {
    pub(super) child: std::process::Child,
    pub(super) process_group: rustix::process::Pid,
    pub(super) stdout_thread: Option<std::thread::JoinHandle<()>>,
    pub(super) stderr_thread: Option<std::thread::JoinHandle<()>>,
    pub(super) io_stop: Arc<AtomicBool>,
    pub(super) cleanup_duration: Duration,
    pub(super) armed: bool,
}

impl Drop for JoinedProcessResources {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let deadline = Instant::now() + self.cleanup_duration;
        let _ = terminate_owned_process_group_until(
            &mut self.child,
            self.process_group,
            None,
            deadline,
        );
        self.io_stop.store(true, Ordering::Release);
        if let Some(thread) = self.stdout_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
    }
}

pub(super) struct JoinedPtyResources {
    pub(super) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(super) process_group: rustix::process::Pid,
    pub(super) reader_thread: Option<std::thread::JoinHandle<()>>,
    pub(super) input_thread: Option<std::thread::JoinHandle<()>>,
    pub(super) input_owner: SyncSender<PtyInputCommand>,
    pub(super) io_stop: Arc<AtomicBool>,
    pub(super) cleanup_duration: Duration,
    pub(super) armed: bool,
}

impl Drop for JoinedPtyResources {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let deadline = Instant::now() + self.cleanup_duration;
        let _ = terminate_owned_pty_group_until(
            self.child.as_mut(),
            self.process_group,
            None,
            deadline,
        );
        self.io_stop.store(true, Ordering::Release);
        let _ = self.input_owner.try_send(PtyInputCommand::Stop);
        if let Some(thread) = self.reader_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
        if let Some(thread) = self.input_thread.take() {
            let _ = join_thread_until(thread, deadline);
        }
    }
}
