use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::{
    Arc, Backend, CHUNK_BYTES, Duration, Instant, Mutex, OperatorAccountFallback, OperatorLaunch,
    OperatorRefusal, Output, PathBuf, Process, TerminalDimensions, TerminalStatus, VecDeque, lock,
};

mod scan;
#[cfg(test)]
use scan::scan;
use scan::{Identity, SessionCleanup};

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "deterministic PTY process fixtures")]
mod tests;

pub(super) struct PtyBackend;

pub(super) fn probe_sessions() -> Result<(), &'static str> {
    let pair = native_pty_system()
        .openpty(size(TerminalDimensions::new(24, 80)?))
        .map_err(|_| "operator PTY probe unavailable")?;
    let mut command = CommandBuilder::new("/bin/sleep");
    command.arg("1");
    command.env_clear();
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|_| "operator PTY probe unavailable")?;
    let checked = (|| {
        let pid = child
            .process_id()
            .ok_or("operator PTY probe has no identity")?;
        let own = Identity::capture(std::process::id())
            .map_err(|()| "operator session scan unavailable")?;
        let separate =
            Identity::capture(pid).map_err(|()| "operator child session scan unavailable")?;
        let session =
            rustix::process::getsid(None).map_err(|_| "operator session id unavailable")?;
        if own.session != session.as_raw_nonzero().get().to_string()
            || separate.session == own.session
            || separate.session != pid.to_string()
        {
            return Err("operator session scan failed isolation probe");
        }
        Ok(())
    })();
    drop(pair);
    let _ = child.wait();
    checked
}

enum Command {
    Input(Vec<u8>),
    Reprove { acknowledge_loss: bool },
}

struct PtyProcess {
    survivors: Arc<Mutex<Vec<super::TerminalProcessIdentity>>>,
    commands: SyncSender<Command>,
    closing: Arc<AtomicBool>,
    result: Arc<Mutex<ProcessResult>>,
    resize: Arc<Mutex<Option<TerminalDimensions>>>,
}

type ProcessResult = Option<(TerminalStatus, Option<u32>)>;

struct Worker {
    master: Option<Box<dyn MasterPty + Send>>,
    io: Option<File>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    identity: Option<Identity>,
    output: Arc<Mutex<Output>>,
    cleaned: bool,
    #[cfg(test)]
    observation_failures: usize,
}

struct RawMaster(RawFd);
impl AsRawFd for RawMaster {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Backend for PtyBackend {
    fn spawn(
        &self,
        launch: OperatorLaunch,
        dimensions: TerminalDimensions,
        output: Arc<Mutex<Output>>,
    ) -> Result<Box<dyn Process>, OperatorRefusal> {
        let worker = Worker::spawn(launch, dimensions, output)?;
        let (commands, receive) = sync_channel(3);
        let closing = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let close = Arc::clone(&closing);
        let ended = Arc::clone(&result);
        let resize = Arc::new(Mutex::new(None));
        let sizes = Arc::clone(&resize);
        let survivors = Arc::new(Mutex::new(Vec::new()));
        let escaped = survivors.clone();
        // On a thread-spawn error Worker::drop still starts bounded session cleanup.
        let started = std::thread::Builder::new()
            .name("operator-pty".into())
            .spawn(move || {
                worker.run(&receive, &close, &ended, &sizes, &escaped);
            });
        if started.is_err() {
            *lock(&result) = Some((TerminalStatus::TerminationUnknown, None));
        }
        Ok(Box::new(PtyProcess {
            survivors,
            commands,
            closing,
            result,
            resize,
        }))
    }
}

impl Worker {
    fn spawn(
        launch: OperatorLaunch,
        dimensions: TerminalDimensions,
        output: Arc<Mutex<Output>>,
    ) -> Result<Self, OperatorRefusal> {
        Self::spawn_command(command(launch), dimensions, output)
    }

    fn spawn_command(
        command: CommandBuilder,
        dimensions: TerminalDimensions,
        output: Arc<Mutex<Output>>,
    ) -> Result<Self, OperatorRefusal> {
        let pair = native_pty_system()
            .openpty(size(dimensions))
            .map_err(|_| OperatorRefusal::Backpressure)?;
        let raw = pair
            .master
            .as_raw_fd()
            .ok_or(OperatorRefusal::Backpressure)?;
        let io = filedescriptor::FileDescriptor::dup(&RawMaster(raw))
            .and_then(|fd| fd.as_file())
            .map_err(|_| OperatorRefusal::Backpressure)?;
        let flags = rustix::fs::fcntl_getfl(&io).map_err(|_| OperatorRefusal::Backpressure)?;
        rustix::fs::fcntl_setfl(&io, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|_| OperatorRefusal::Backpressure)?;
        // portable-pty resets SIGHUP and other dispositions before setsid/exec.
        // No command wrapper, trap, host environment or arbitrary bootstrap command.
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|_| OperatorRefusal::Backpressure)?;
        let identity = child
            .process_id()
            .and_then(|pid| Identity::capture_child(pid).ok());
        let mut worker = Self {
            master: Some(pair.master),
            io: Some(io),
            child,
            identity,
            output,
            cleaned: false,
            #[cfg(test)]
            observation_failures: 0,
        };
        // Keep our slave reference through this bounded nonblocking drain so a
        // fast-exiting child's startup bytes survive Darwin's terminal revoke.
        let _ = worker.io_round(&mut VecDeque::new());
        drop(pair.slave);
        if worker.identity.is_none() {
            worker.io.take();
            worker.master.take();
            // This is our unreaped direct child, not a remembered external PID.
            // Without session identity no group signal is permitted.
            if let Some(pid) = worker
                .child
                .process_id()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
            let _ = worker.child.wait();
            worker.cleaned = true;
            return Err(OperatorRefusal::Backpressure);
        }
        Ok(worker)
    }

    fn run(
        mut self,
        receive: &std::sync::mpsc::Receiver<Command>,
        closing: &AtomicBool,
        result: &Mutex<Option<(TerminalStatus, Option<u32>)>>,
        resize: &Mutex<Option<TerminalDimensions>>,
        survivors: &Mutex<Vec<super::TerminalProcessIdentity>>,
    ) {
        let mut pending: VecDeque<u8> = VecDeque::new();
        let mut cleanup = self.identity.clone().map(SessionCleanup::new);
        let mut last_scan = Instant::now();
        loop {
            if cleanup.is_none() {
                break;
            }
            if closing.load(Ordering::Acquire) {
                break;
            }
            if pending.is_empty() {
                match receive.try_recv() {
                    Ok(Command::Input(bytes)) => pending.extend(bytes),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Ok(Command::Reprove { .. }) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            if let Some(dimensions) = lock(resize).take()
                && self
                    .master
                    .as_ref()
                    .is_none_or(|master| master.resize(size(dimensions)).is_err())
            {
                break;
            }
            if self.io_round(&mut pending).is_err() {
                break;
            }
            if self.leader_exited() {
                break;
            }
            if last_scan.elapsed() >= Duration::from_secs(1) {
                if let Ok(rows) = self.observation()
                    && let Some(cleanup) = &mut cleanup
                {
                    cleanup.observe(&rows);
                }
                last_scan = Instant::now();
            }
            if let Some(io) = &self.io {
                let flags = if pending.is_empty() {
                    rustix::event::PollFlags::IN
                } else {
                    rustix::event::PollFlags::IN | rustix::event::PollFlags::OUT
                };
                let _ = rustix::event::poll(
                    &mut [rustix::event::PollFd::new(io, flags)],
                    Some(&rustix::event::Timespec {
                        tv_sec: 0,
                        tv_nsec: 20_000_000,
                    }),
                );
            }
        }
        self.io.take();
        self.master.take(); // HUP by closing every master descriptor.
        let mut code = None;
        let proven = cleanup.as_mut().is_some_and(|cleanup| {
            cleanup.finish(|| {
                code = self.reap_child();
                code.is_some()
            })
        });
        self.cleaned = true;
        if let Some(cleanup) = &cleanup {
            *lock(survivors) = cleanup.survivors();
        }
        *lock(result) = Some((
            if proven {
                TerminalStatus::Exited
            } else {
                TerminalStatus::TerminationUnknown
            },
            code,
        ));
        if !proven && let Some(mut cleanup) = cleanup {
            self.await_resolution(&mut cleanup, receive, result, survivors, code);
        }
    }

    fn await_resolution(
        &mut self,
        cleanup: &mut SessionCleanup,
        receive: &std::sync::mpsc::Receiver<Command>,
        result: &Mutex<ProcessResult>,
        survivors: &Mutex<Vec<super::TerminalProcessIdentity>>,
        code: Option<u32>,
    ) {
        *lock(survivors) = cleanup.survivors();
        loop {
            let acknowledge_loss = match receive.recv_timeout(Duration::from_secs(10)) {
                Ok(Command::Reprove { acknowledge_loss }) => acknowledge_loss,
                Ok(Command::Input(_)) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // A reproof never signals: the leader may already have been reaped.
            let _ = self.reap_child();
            let proven = cleanup.reprove();
            *lock(survivors) = cleanup.survivors();
            if proven || acknowledge_loss {
                *lock(result) = Some((
                    if proven {
                        TerminalStatus::Exited
                    } else {
                        TerminalStatus::Lost
                    },
                    code,
                ));
                break;
            }
        }
    }

    fn leader_exited(&self) -> bool {
        self.child.process_id().is_some_and(scan::child_exited)
    }

    fn reap_child(&mut self) -> Option<u32> {
        if let Some(identity) = &self.identity {
            identity.retire_anchor();
        }
        self.child
            .try_wait()
            .ok()
            .flatten()
            .map(|exit| exit.exit_code())
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "per-worker injected observation failures in tests"
        )
    )]
    fn observation(&mut self) -> Result<Arc<Vec<Identity>>, ()> {
        #[cfg(test)]
        if self.observation_failures > 0 {
            self.observation_failures -= 1;
            return Err(());
        }
        scan::observation()
    }

    fn io_round(&mut self, pending: &mut VecDeque<u8>) -> Result<(), ()> {
        let io = self.io.as_mut().ok_or(())?;
        let mut buffer = [0_u8; CHUNK_BYTES];
        for _ in 0..4 {
            match io.read(&mut buffer) {
                Ok(0) => return Err(()),
                Ok(n) => lock(&self.output).push(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(()),
            }
        }
        if !pending.is_empty() {
            match io.write(pending.make_contiguous()) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(()),
            }
        }
        Ok(())
    }
}

impl Process for PtyProcess {
    fn input(&mut self, bytes: Vec<u8>) -> Result<(), OperatorRefusal> {
        self.commands
            .try_send(Command::Input(bytes))
            .map_err(|_| OperatorRefusal::Backpressure)
    }
    fn resize(&mut self, dimensions: TerminalDimensions) -> Result<(), OperatorRefusal> {
        *lock(&self.resize) = Some(dimensions);
        Ok(())
    }
    fn poll(&mut self) -> Option<(TerminalStatus, Option<u32>)> {
        *lock(&self.result)
    }
    fn close(&mut self) {
        self.closing.store(true, Ordering::Release);
    }
    fn reprove(&mut self, acknowledge_loss: bool) {
        let sent = self
            .commands
            .try_send(Command::Reprove { acknowledge_loss });
        if matches!(sent, Err(std::sync::mpsc::TrySendError::Disconnected(_))) && acknowledge_loss {
            *lock(&self.result) = Some((TerminalStatus::Lost, None));
        }
    }
    fn survivors(&self) -> Vec<super::TerminalProcessIdentity> {
        lock(&self.survivors).clone()
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !self.cleaned {
            self.io.take();
            self.master.take();
            if let Some(identity) = self.identity.clone() {
                let _ = SessionCleanup::new(identity).finish(|| self.reap_child().is_some());
            }
            let _ = self.reap_child();
        }
    }
}

fn command(launch: OperatorLaunch) -> CommandBuilder {
    let mut command = CommandBuilder::new(&launch.shell);
    command.arg("-i");
    curate(&mut command, launch);
    command
}

fn curate(command: &mut CommandBuilder, launch: OperatorLaunch) {
    command.cwd(&launch.cwd);
    command.env_clear();
    command.env("PATH", launch.path);
    command.env("HOME", launch.home);
    // portable-pty also sets SHELL. Pin it to the shell we validated, never
    // its inherited/default-shell selection.
    command.env("SHELL", launch.shell);
    command.env("USER", &launch.user);
    command.env("LOGNAME", &launch.user);
    command.env("TERM", "xterm-256color");
    command.env("LANG", "C.UTF-8");
    if let Some(ceiling) = launch.discovery_ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
}

fn size(dimensions: TerminalDimensions) -> PtySize {
    PtySize {
        rows: dimensions.rows(),
        cols: dimensions.columns(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Produces curated launch policy from the runner account, never the host's env.
///
/// # Errors
/// Account lookup, home, absolute executable shell, or curated PATH is unavailable.
pub fn account_launch(
    cwd: PathBuf,
    path: String,
    fallback: Option<OperatorAccountFallback>,
) -> Result<OperatorLaunch, OperatorRefusal> {
    let account = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|account| (account.shell, account.dir, account.name));
    launch_with_account(cwd, path, account, fallback)
}

fn launch_with_account(
    cwd: PathBuf,
    path: String,
    account: Option<(PathBuf, PathBuf, String)>,
    fallback: Option<OperatorAccountFallback>,
) -> Result<OperatorLaunch, OperatorRefusal> {
    let (shell, home, user) = if let Some(account) = account {
        account
    } else {
        let fallback = fallback.ok_or(OperatorRefusal::WorkspaceUnavailable)?;
        (fallback.shell, fallback.home, fallback.user)
    };
    if !shell.is_absolute()
        || !home.is_absolute()
        || path.is_empty()
        || rustix::fs::access(&shell, rustix::fs::Access::EXEC_OK).is_err()
    {
        return Err(OperatorRefusal::WorkspaceUnavailable);
    }
    Ok(OperatorLaunch {
        cwd,
        path,
        home,
        user,
        shell,
        discovery_ceiling: None,
    })
}
