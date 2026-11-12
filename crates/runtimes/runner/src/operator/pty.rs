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

enum Command {
    Input(Vec<u8>),
}

struct PtyProcess {
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
        // On a thread-spawn error Worker::drop still starts bounded session cleanup.
        let started = std::thread::Builder::new()
            .name("operator-pty".into())
            .spawn(move || {
                worker.run(&receive, &close, &ended, &sizes);
            });
        if started.is_err() {
            *lock(&result) = Some((TerminalStatus::TerminationUnknown, None));
        }
        Ok(Box::new(PtyProcess {
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
        drop(pair.slave);
        let identity = child
            .process_id()
            .and_then(|pid| Identity::capture(pid).ok());
        Ok(Self {
            master: Some(pair.master),
            io: Some(io),
            child,
            identity,
            output,
            cleaned: false,
        })
    }

    fn run(
        mut self,
        receive: &std::sync::mpsc::Receiver<Command>,
        closing: &AtomicBool,
        result: &Mutex<Option<(TerminalStatus, Option<u32>)>>,
        resize: &Mutex<Option<TerminalDimensions>>,
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
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
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
            if last_scan.elapsed() >= Duration::from_millis(50) {
                if let Ok(rows) = scan::observation() {
                    if let Some(cleanup) = &mut cleanup {
                        cleanup.observe(&rows);
                    }
                    if rows.iter().any(|r| {
                        self.identity.as_ref().is_some_and(|id| r.pid == id.pid) && r.zombie
                    }) {
                        break;
                    }
                } else {
                    if let Some(cleanup) = &mut cleanup {
                        cleanup.uncertain = true;
                    }
                    break;
                }
                last_scan = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.io.take();
        self.master.take(); // HUP by closing every master descriptor.
        let mut code = None;
        let proven = cleanup.as_mut().is_some_and(|cleanup| {
            cleanup.finish(|| {
                code = self
                    .child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|exit| exit.exit_code());
                code.is_some()
            })
        });
        self.cleaned = true;
        *lock(result) = Some((
            if proven {
                TerminalStatus::Exited
            } else {
                TerminalStatus::TerminationUnknown
            },
            code,
        ));
        // A still-unreaped leader is retained by a reaper; never signal its bare PID.
        if code.is_none() {
            let _ = self.child.wait();
        }
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
            if let Some(identity) = self.identity.take() {
                let _ = SessionCleanup::new(identity)
                    .finish(|| self.child.try_wait().ok().flatten().is_some());
            }
            let _ = self.child.try_wait();
        }
    }
}

fn command(launch: OperatorLaunch) -> CommandBuilder {
    let mut command = CommandBuilder::new(&launch.shell);
    command.arg("-i");
    command.cwd(&launch.cwd);
    command.env_clear();
    command.env("PATH", launch.path);
    command.env("HOME", launch.home);
    command.env("USER", &launch.user);
    command.env("LOGNAME", &launch.user);
    command.env("TERM", "xterm-256color");
    command.env("LANG", "C.UTF-8");
    if let Some(ceiling) = launch.discovery_ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    command
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
        .flatten();
    let (shell, home, user) = if let Some(account) = account {
        (account.shell, account.dir, account.name)
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
