//! One joined-process admission path with terminal-specific spawn and I/O.

mod terminal;
pub(super) use terminal::JoinedProcessResources;
use terminal::JoinedSpawn;

use std::os::unix::process::CommandExt;

use super::{
    Arc, AsRawFd, AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Command, CommandBuilder, Condvar,
    Duration, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendErrorKind, ExecutionEnvironmentLimits, ExecutionWorkspace, FOREGROUND,
    HostProcessContext, HostProcessOwner, Instant, JOINED_PROCESS_PTY_INITIAL_COLUMNS,
    JOINED_PROCESS_PTY_INITIAL_ROWS, JoinedProcessCommand, JoinedProcessOutputCursor,
    JoinedProcessOutputStream, JoinedProcessStartRequest, JoinedProcessStatus,
    JoinedProcessTerminalMode, JoinedSession, JoinedSessionOutput, JoinedSessionShared, MasterPty,
    Mutex, OperationContext, Ordering, Path, PendingJobOutput, PtySize, RawFd, Receiver, Stdio,
    SyncSender, TrySendError, Write, backend_error, cleanup_pty_setup_error, cleanup_setup_error,
    cleanup_unresolved_error, conflict_error, displayed_command, fs, host_process_home,
    host_process_path, invalid_request, io_error, join_thread_until, native_pty_system,
    resolve_existing_no_symlink, resource_error, restore_path_after_login, set_nonblocking,
    sleep_until_next_poll, spawn_joined_output_reader, supervise_joined_process, sync_channel,
    terminate_direct_child, terminate_direct_pty_child, workspace_error,
};

pub(super) enum JoinedSessionInput {
    Pipes(std::process::ChildStdin),
    Pty(PtyInputSender),
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "joined spawn, initial I/O rollback and supervisor ownership transfer form one admission boundary"
)]
pub(super) fn start_joined_host_process(
    workspace: &ExecutionWorkspace,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
    limits: ExecutionEnvironmentLimits,
    queue_limit: usize,
    request: &JoinedProcessStartRequest,
    control: OperationContext,
    pending_spool: Option<PendingJobOutput>,
) -> Result<JoinedSession, ExecutionBackendError> {
    let mut resolved = request.clone();
    if let JoinedProcessCommand::Literal {
        executable,
        arguments,
    } = &request.command
        && !executable.is_absolute()
    {
        let program = super::resolve_host_process_program(
            executable
                .to_str()
                .ok_or_else(|| invalid_request("process program is not UTF-8"))?,
            process,
            environment,
        )?;
        resolved.command = JoinedProcessCommand::literal(program, arguments.iter().cloned())?;
    }
    let request = &resolved;
    let cwd = resolve_existing_no_symlink(workspace, &request.cwd)?;
    if !cwd.is_dir() {
        return Err(workspace_error(
            "joined process cwd is not a contained directory",
        ));
    }
    let wrapped_shell = match &request.command {
        JoinedProcessCommand::Shell { command, .. } => {
            let command = restore_path_after_login(process, command)?;
            Some(match request.terminal_mode {
                JoinedProcessTerminalMode::Pipes => command,
                JoinedProcessTerminalMode::UnixPty => format!("trap '' HUP; {command}"),
            })
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
    let (mut resources, io) = JoinedSpawn {
        program,
        argv0,
        arguments,
        cwd: &cwd,
        process,
        environment,
        request,
        limits,
    }
    .spawn()?;
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
    let input = resources.start_io(io, &shared, &output_io_failed, limits, request)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let foreground_control = control.clone();
    let promoted = Arc::new(AtomicU8::new(FOREGROUND));
    let thread_promoted = Arc::clone(&promoted);
    let stop_grace_millis = Arc::new(AtomicU64::new(2000));
    let thread_stop_grace = Arc::clone(&stop_grace_millis);
    let thread_shared = Arc::clone(&shared);
    let shell_command = matches!(request.command, JoinedProcessCommand::Shell { .. });
    let (thread_name, thread_error) = match request.terminal_mode {
        JoinedProcessTerminalMode::Pipes => (
            "plak-joined-process",
            "joined process supervisor could not start",
        ),
        JoinedProcessTerminalMode::UnixPty => (
            "plak-joined-pty",
            "joined process PTY supervisor could not start",
        ),
    };
    let thread = std::thread::Builder::new()
        .name(String::from(thread_name))
        .spawn(move || {
            let mut resources = resources;
            supervise_joined_process(
                &mut resources,
                limits,
                &control,
                &thread_promoted,
                &thread_stop_grace,
                &thread_stop,
                &output_io_failed,
                &thread_shared,
                shell_command,
            );
            resources.disarm();
        })
        .map_err(|_| io_error(thread_error));
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => {
            stop.store(true, Ordering::Release);
            // Dropping the rejected closure terminates the group before joining I/O.
            return Err(error);
        }
    };
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
