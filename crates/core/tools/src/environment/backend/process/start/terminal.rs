use super::{
    Arc, AtomicBool, Command, CommandExt, Duration, EnvironmentVariableName,
    EnvironmentVariableValue, ExecutionBackendError, ExecutionEnvironmentLimits,
    HostProcessContext, Instant, JoinedProcessOutputStream, JoinedProcessStartRequest,
    JoinedProcessStatus, JoinedProcessTerminalMode, JoinedSessionInput, JoinedSessionShared, Mutex,
    Path, PtyInputCommand, ReaderStop, Stdio, SyncSender, cleanup_setup_error,
    cleanup_unresolved_error, clone_linux_pty_master, host_process_home, host_process_path,
    invalid_request, io_error, join_thread_until, joined_pty_command, native_pty_system,
    reap_when_group_empty, set_nonblocking, sleep_until_next_poll, spawn_joined_output_reader,
    spawn_pty_input_writer, terminate_direct_child, terminate_direct_pty_child,
    write_joined_initial_input,
};
use super::{JOINED_PROCESS_PTY_INITIAL_COLUMNS, JOINED_PROCESS_PTY_INITIAL_ROWS, PtySize};

#[cfg(test)]
mod tests;
use crate::environment::backend::process::{
    terminate_owned_process_group_with_grace, terminate_owned_pty_group_with_grace,
};

/// The leader and, once reaped, its status.
enum JoinedChild {
    Pipes(std::process::Child, Option<std::process::ExitStatus>),
    Pty(
        Box<dyn portable_pty::Child + Send + Sync>,
        Option<portable_pty::ExitStatus>,
    ),
    /// An unreaped leader was handed to the reaper as its owner dropped.
    HandedOff,
}

pub(super) enum JoinedIo {
    Pipes {
        stdin: Option<std::process::ChildStdin>,
        stdout: std::process::ChildStdout,
        stderr: std::process::ChildStderr,
    },
    Pty {
        reader: std::fs::File,
        writer: std::fs::File,
    },
}

pub(super) struct JoinedSpawn<'a> {
    pub(super) program: &'a Path,
    pub(super) argv0: Option<&'a Path>,
    pub(super) arguments: Vec<&'a str>,
    pub(super) cwd: &'a Path,
    pub(super) process: &'a HostProcessContext,
    pub(super) environment: &'a [(EnvironmentVariableName, EnvironmentVariableValue)],
    pub(super) request: &'a JoinedProcessStartRequest,
    pub(super) limits: ExecutionEnvironmentLimits,
}

pub(in crate::environment::backend::process) struct JoinedProcessResources {
    child: JoinedChild,
    pub(super) process_group: rustix::process::Pid,
    readers: Vec<std::thread::JoinHandle<()>>,
    input_thread: Option<std::thread::JoinHandle<()>>,
    input_owner: Option<SyncSender<PtyInputCommand>>,
    io_stop: Arc<ReaderStop>,
    cleanup_duration: Duration,
    armed: bool,
    #[cfg(test)]
    before_rollback: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    before_drain: Option<SyncSender<()>>,
}

impl JoinedSpawn<'_> {
    pub(super) fn spawn(
        &self,
    ) -> Result<(JoinedProcessResources, JoinedIo), ExecutionBackendError> {
        let (child, process_group, io) = match self.request.terminal_mode {
            JoinedProcessTerminalMode::Pipes => self.spawn_pipes()?,
            JoinedProcessTerminalMode::UnixPty => self.spawn_pty()?,
        };
        // After the child, so its pipes are counted first: a wake never
        // takes the room a spawn needs.
        let io_stop = Arc::new(ReaderStop::new());
        Ok((
            JoinedProcessResources {
                child,
                process_group,
                readers: Vec::new(),
                input_thread: None,
                input_owner: None,
                io_stop,
                cleanup_duration: self.limits.cleanup_duration(),
                armed: true,
                #[cfg(test)]
                before_rollback: None,
                #[cfg(test)]
                before_drain: None,
            },
            io,
        ))
    }

    fn spawn_pipes(
        &self,
    ) -> Result<(JoinedChild, rustix::process::Pid, JoinedIo), ExecutionBackendError> {
        let mut command = Command::new(self.program);
        command.args(&self.arguments);
        command.arg0(self.argv0.unwrap_or(self.program));
        command
            .current_dir(self.cwd)
            .env_clear()
            .stdin(
                if self.request.keep_stdin_open || !self.request.stdin.is_empty() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                },
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for (name, value) in self.environment {
            if name.as_str() == "PATH" {
                continue;
            }
            command.env(name.as_str(), value.0.as_ref());
        }
        command.env("PATH", host_process_path(self.process, self.environment)?);
        if let Some(ceiling) = &self.process.discovery_ceiling {
            command.env("GIT_CEILING_DIRECTORIES", ceiling);
        }
        if let Some(home) = host_process_home(self.process, self.environment) {
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
            Ok(group) => group,
            Err(error) => {
                return Err(
                    terminate_direct_child(&mut child, self.limits.cleanup_duration())
                        .err()
                        .unwrap_or(error),
                );
            }
        };
        let (child, io) = self.prepare_pipes_io(child, process_group)?;
        Ok((JoinedChild::Pipes(child, None), process_group, io))
    }

    fn prepare_pipes_io(
        &self,
        mut child: std::process::Child,
        process_group: rustix::process::Pid,
    ) -> Result<(std::process::Child, JoinedIo), ExecutionBackendError> {
        let stdin = child.stdin.take();
        if (self.request.keep_stdin_open || !self.request.stdin.is_empty()) && stdin.is_none() {
            return Err(cleanup_setup_error(
                child,
                process_group,
                self.limits.cleanup_duration(),
                io_error("joined process stdin was unavailable"),
            ));
        }
        let Some(stdout) = child.stdout.take() else {
            return Err(cleanup_setup_error(
                child,
                process_group,
                self.limits.cleanup_duration(),
                io_error("joined process stdout was unavailable"),
            ));
        };
        let Some(stderr) = child.stderr.take() else {
            return Err(cleanup_setup_error(
                child,
                process_group,
                self.limits.cleanup_duration(),
                io_error("joined process stderr was unavailable"),
            ));
        };
        if let Some(stdin) = &stdin
            && let Err(error) = set_nonblocking(stdin)
        {
            return Err(cleanup_setup_error(
                child,
                process_group,
                self.limits.cleanup_duration(),
                error,
            ));
        }
        Ok((
            child,
            JoinedIo::Pipes {
                stdin,
                stdout,
                stderr,
            },
        ))
    }

    fn spawn_pty(
        &self,
    ) -> Result<(JoinedChild, rustix::process::Pid, JoinedIo), ExecutionBackendError> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: JOINED_PROCESS_PTY_INITIAL_ROWS,
                cols: JOINED_PROCESS_PTY_INITIAL_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| io_error("joined process PTY could not be opened"))?;
        let portable_pty::PtyPair { master, slave } = pair;
        let mut command = joined_pty_command(self.program, self.argv0, &self.arguments);
        command.cwd(self.cwd);
        command.env_clear();
        for (name, value) in self.environment {
            if name.as_str() == "PATH" {
                continue;
            }
            command.env(name.as_str(), value.0.as_ref());
        }
        command.env("PATH", host_process_path(self.process, self.environment)?);
        if let Some(ceiling) = &self.process.discovery_ceiling {
            command.env("GIT_CEILING_DIRECTORIES", ceiling);
        }
        if let Some(home) = host_process_home(self.process, self.environment) {
            command.env("HOME", home);
        }
        // Only the descriptor duplicate keeps the master open after setup.
        let terminal = clone_linux_pty_master(master.as_ref())?;
        set_nonblocking(&terminal)?;
        let reader = terminal
            .try_clone()
            .map_err(|_| io_error("joined process PTY reader was unavailable"))?;
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
            Ok(group) => group,
            Err(error) => {
                return Err(terminate_direct_pty_child(
                    child.as_mut(),
                    self.limits.cleanup_duration(),
                )
                .err()
                .unwrap_or(error));
            }
        };
        Ok((
            JoinedChild::Pty(child, None),
            process_group,
            JoinedIo::Pty {
                reader,
                writer: terminal,
            },
        ))
    }
}

impl JoinedProcessResources {
    pub(super) fn start_io(
        &mut self,
        io: JoinedIo,
        shared: &Arc<Mutex<JoinedSessionShared>>,
        failed: &Arc<AtomicBool>,
        limits: ExecutionEnvironmentLimits,
        request: &JoinedProcessStartRequest,
    ) -> Result<Option<JoinedSessionInput>, ExecutionBackendError> {
        // Roll back before dropping pending stdin, so EOF cannot precede group termination.
        let mut input = match io {
            JoinedIo::Pipes {
                stdin,
                stdout,
                stderr,
            } => {
                self.add_reader(
                    stdout,
                    JoinedProcessOutputStream::Stdout,
                    shared,
                    failed,
                    limits,
                )
                .map_err(|error| self.rollback(error))?;
                self.add_reader(
                    stderr,
                    JoinedProcessOutputStream::Stderr,
                    shared,
                    failed,
                    limits,
                )
                .map_err(|error| self.rollback(error))?;
                stdin.map(JoinedSessionInput::Pipes)
            }
            JoinedIo::Pty { reader, writer } => {
                self.add_reader(
                    reader,
                    JoinedProcessOutputStream::Terminal,
                    shared,
                    failed,
                    limits,
                )
                .map_err(|error| self.rollback(error))?;
                let (input, owner, thread) = spawn_pty_input_writer(Box::new(writer))
                    .map_err(|error| self.rollback(error))?;
                self.input_owner = Some(owner);
                self.input_thread = Some(thread);
                Some(JoinedSessionInput::Pty(input))
            }
        };
        match &mut input {
            Some(JoinedSessionInput::Pipes(stdin)) => {
                write_joined_initial_input(stdin, &request.stdin, limits.execution_duration())
                    .map_err(|error| self.rollback(error))?;
            }
            Some(JoinedSessionInput::Pty(sender)) => {
                sender
                    .write(&request.stdin, limits.execution_duration())
                    .map_err(|error| self.rollback(error))?;
            }
            None => {}
        }
        if !request.keep_stdin_open {
            if let Some(JoinedSessionInput::Pty(sender)) = &input {
                sender
                    .close(limits.execution_duration())
                    .map_err(|error| self.rollback(error))?;
            }
            input = None;
        }
        Ok(input)
    }

    fn add_reader<R: std::os::fd::AsFd + std::io::Read + Send + 'static>(
        &mut self,
        reader: R,
        stream: JoinedProcessOutputStream,
        shared: &Arc<Mutex<JoinedSessionShared>>,
        failed: &Arc<AtomicBool>,
        limits: ExecutionEnvironmentLimits,
    ) -> Result<(), ExecutionBackendError> {
        self.readers.push(spawn_joined_output_reader(
            reader,
            stream,
            Arc::clone(shared),
            Arc::clone(failed),
            Arc::clone(&self.io_stop),
            limits,
        )?);
        Ok(())
    }

    pub(in crate::environment::backend::process) fn process_group(&self) -> rustix::process::Pid {
        self.process_group
    }

    pub(in crate::environment::backend::process) fn try_wait(
        &mut self,
    ) -> Result<JoinedProcessStatus, ()> {
        match &mut self.child {
            JoinedChild::Pipes(child, status) => child
                .try_wait()
                .map(|exit| {
                    exit.map_or(JoinedProcessStatus::Running, |exit| {
                        let code = exit.code();
                        *status = Some(exit);
                        JoinedProcessStatus::Exited(code)
                    })
                })
                .map_err(|_| ()),
            JoinedChild::Pty(child, status) => child
                .try_wait()
                .map(|exit| {
                    exit.map_or(JoinedProcessStatus::Running, |exit| {
                        let code = i32::try_from(exit.exit_code()).ok();
                        *status = Some(exit);
                        JoinedProcessStatus::Exited(code)
                    })
                })
                .map_err(|_| ()),
            JoinedChild::HandedOff => Err(()),
        }
    }

    pub(in crate::environment::backend::process) fn terminate(
        &mut self,
        deadline: Instant,
        grace: Duration,
    ) -> Result<(), ExecutionBackendError> {
        // The reaped status is kept, so a later call signals nothing.
        match &mut self.child {
            JoinedChild::Pipes(child, status) => {
                let exit = terminate_owned_process_group_with_grace(
                    child,
                    self.process_group,
                    status.take(),
                    deadline,
                    grace,
                )?;
                *status = Some(exit);
            }
            JoinedChild::Pty(child, status) => {
                let exit = terminate_owned_pty_group_with_grace(
                    child.as_mut(),
                    self.process_group,
                    status.take(),
                    deadline,
                    grace,
                )?;
                *status = Some(exit);
            }
            JoinedChild::HandedOff => {}
        }
        Ok(())
    }

    pub(in crate::environment::backend::process) fn drain_readers_until(&self, deadline: Instant) {
        #[cfg(test)]
        if let Some(release) = &self.before_drain {
            let _ = release.send(());
        }
        let started = Instant::now();
        while Instant::now() < deadline && self.readers.iter().any(|reader| !reader.is_finished()) {
            sleep_until_next_poll(started, deadline);
        }
    }

    pub(in crate::environment::backend::process) fn stop_io(&self) {
        self.io_stop.stop();
        if let Some(owner) = &self.input_owner {
            let _ = owner.try_send(PtyInputCommand::Stop);
        }
    }

    pub(in crate::environment::backend::process) fn join_workers_until(
        &mut self,
        deadline: Instant,
    ) -> bool {
        let mut joined = true;
        for reader in self.readers.drain(..) {
            joined &= join_thread_until(reader, deadline);
        }
        if let Some(input) = self.input_thread.take() {
            joined &= join_thread_until(input, deadline);
        }
        joined
    }

    fn rollback(&mut self, error: ExecutionBackendError) -> ExecutionBackendError {
        self.io_stop.stop();
        #[cfg(test)]
        if let Some(probe) = self.before_rollback.take() {
            probe();
        }
        let deadline = Instant::now() + self.cleanup_duration;
        let pty = matches!(self.child, JoinedChild::Pty(..));
        let (cleanup, error) = match self.terminate(deadline, Duration::from_secs(2)) {
            // A PTY setup failure reports its cleanup failure in its place.
            Err(cleanup) if pty => (Ok(()), cleanup),
            cleanup => (cleanup, error),
        };
        if let Some(owner) = &self.input_owner {
            let _ = owner.try_send(PtyInputCommand::Stop);
        }
        let joined = self.join_workers_until(deadline);
        self.armed = false;
        if let Err(cleanup) = cleanup {
            return cleanup;
        }
        if !joined {
            return cleanup_unresolved_error();
        }
        error
    }

    pub(in crate::environment::backend::process) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for JoinedProcessResources {
    fn drop(&mut self) {
        if self.armed {
            let deadline = Instant::now() + self.cleanup_duration;
            let _ = self.terminate(deadline, Duration::from_secs(2));
            self.stop_io();
            let _ = self.join_workers_until(deadline);
        }
        // A leader without a status was never reaped: its group outlived
        // cleanup, or it was released. The reaper takes it over.
        match std::mem::replace(&mut self.child, JoinedChild::HandedOff) {
            JoinedChild::Pipes(child, None) => {
                reap_when_group_empty(self.process_group, Box::new(child));
            }
            JoinedChild::Pty(child, None) => {
                reap_when_group_empty(self.process_group, Box::new(child));
            }
            JoinedChild::Pipes(_, Some(_))
            | JoinedChild::Pty(_, Some(_))
            | JoinedChild::HandedOff => {}
        }
    }
}
