//! Bounded readers for joined and literal process output.

use std::io::Write;
use std::os::unix::process::CommandExt;

use rustix::event::{PollFd, PollFlags};

use super::{
    Arc, AsFd, AtomicBool, Command, Duration, Errno, ExecutionBackendError,
    ExecutionEnvironmentLimits, HostProcessOutput, HostProcessStream, Instant,
    JoinedProcessOutputRecord, JoinedProcessOutputStream, JoinedSessionShared, Mutex, OFlags,
    Ordering, Read, Stdio, fs, io_error, lock, supervision_poll_interval,
};

#[cfg(test)]
mod tests;

/// Descriptors a session leaves free under the soft open-file limit before
/// its readers take a wake pipe; with less room they poll instead, so the
/// wakes never starve a later spawn of its pipes.
const WAKE_HEADROOM: u64 = 64;

/// Stops a joined session's output readers. Stopping also wakes every reader
/// blocked waiting for output, so an idle job's readers never poll.
pub(super) struct ReaderStop {
    stopped: AtomicBool,
    // A single byte is written on the first stop and never read, so the wake
    // stays readable for every reader that waits after it. `None` when the
    // process had no room for it: the readers poll every 2 ms instead.
    wake: Option<(std::io::PipeReader, std::io::PipeWriter)>,
}

impl ReaderStop {
    /// A stop with a wake pipe (two descriptors) when the soft open-file
    /// limit leaves room for one, and without one otherwise.
    pub(super) fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            wake: room_for_wake().then(std::io::pipe).and_then(Result::ok),
        }
    }

    pub(super) fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel)
            && let Some((_, waker)) = &self.wake
        {
            // `write_all` retries an interrupted write. A one-byte write to
            // a pipe whose read end this holds cannot otherwise fail.
            let _ = (&*waker).write_all(&[0]);
        }
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Blocks until `reader` is readable or hung up, or the readers stop.
    fn wait_for_output(&self, reader: &impl AsFd, pollable: &mut bool) {
        self.wait_with(reader, pollable, |fds| rustix::event::poll(fds, None));
    }

    /// [`Self::wait_for_output`] through `poll`. Without a wake, or for a
    /// descriptor `poll` reports `NVAL` for or fails on, `pollable` turns
    /// false and each pass sleeps 2 ms instead.
    fn wait_with(
        &self,
        reader: &impl AsFd,
        pollable: &mut bool,
        poll: impl FnOnce(&mut [PollFd<'_>]) -> rustix::io::Result<usize>,
    ) {
        if *pollable && let Some((wake, _)) = &self.wake {
            let mut fds = [
                PollFd::new(reader, PollFlags::IN),
                PollFd::new(wake, PollFlags::IN),
            ];
            match poll(&mut fds) {
                Ok(_) if !fds[0].revents().contains(PollFlags::NVAL) => return,
                Err(Errno::INTR) => return,
                _ => *pollable = false,
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Whether a wake pipe leaves [`WAKE_HEADROOM`] descriptors free under the
/// soft open-file limit. An open-descriptor count that cannot be taken
/// answers no.
fn room_for_wake() -> bool {
    use rustix::process::{Resource, getrlimit};
    let Some(soft) = getrlimit(Resource::Nofile).current else {
        return true;
    };
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    fs::read_dir(directory).is_ok_and(|entries| {
        let open = u64::try_from(entries.count()).unwrap_or(u64::MAX);
        open.saturating_add(2 + WAKE_HEADROOM) <= soft
    })
}

pub(super) fn spawn_joined_output_reader<R>(
    reader: R,
    stream: JoinedProcessOutputStream,
    shared: Arc<Mutex<JoinedSessionShared>>,
    io_failed: Arc<AtomicBool>,
    stop: Arc<ReaderStop>,
    limits: ExecutionEnvironmentLimits,
) -> Result<std::thread::JoinHandle<()>, ExecutionBackendError>
where
    R: AsFd + Read + Send + 'static,
{
    set_nonblocking(&reader)?;
    std::thread::Builder::new()
        .name(String::from("plak-joined-output"))
        .spawn(move || {
            let mut reader = reader;
            let mut chunk = [0_u8; 16 * 1024];
            let mut pollable = true;
            loop {
                // A release is followed by the supervisor's stop, which wakes
                // a reader waiting here; the release is checked first.
                if lock(&shared).disowned {
                    if detach_output_reader(&reader, stream, &shared).is_err() {
                        io_failed.store(true, Ordering::Release);
                    }
                    return;
                }
                if stop.is_stopped() {
                    return;
                }
                let read = match reader.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        stop.wait_for_output(&reader, &mut pollable);
                        continue;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if stream == JoinedProcessOutputStream::Terminal
                            && error.raw_os_error() == Some(Errno::IO.raw_os_error()) =>
                    {
                        return;
                    }
                    Err(_) => {
                        io_failed.store(true, Ordering::Release);
                        return;
                    }
                };
                if stop.is_stopped() && !lock(&shared).disowned {
                    return;
                }
                let mut state = lock(&shared);
                state
                    .changes
                    .send_modify(|revision| *revision = revision.wrapping_add(1));
                let read_bytes = u64::try_from(read).unwrap_or(u64::MAX);
                let sequence = state.output.next_sequence;
                state.output.next_sequence = sequence.saturating_add(1);
                let cursor = state.output.next_cursor;
                state.output.next_cursor = cursor.saturating_add(read_bytes);
                state.output.retained_bytes =
                    state.output.retained_bytes.saturating_add(read_bytes);
                match stream {
                    JoinedProcessOutputStream::Stdout | JoinedProcessOutputStream::Terminal => {
                        state.output.total_stdout_bytes =
                            state.output.total_stdout_bytes.saturating_add(read_bytes);
                    }
                    JoinedProcessOutputStream::Stderr => {
                        state.output.total_stderr_bytes =
                            state.output.total_stderr_bytes.saturating_add(read_bytes);
                    }
                }
                state.output.records.push_back(JoinedProcessOutputRecord {
                    sequence,
                    cursor,
                    stream,
                    bytes: Arc::from(&chunk[..read]),
                });
                if let Some(spool) = &mut state.spool {
                    spool.append(&chunk[..read]);
                } else {
                    state.maybe_spool(&chunk[..read], false);
                }
                // A session keeps a bounded window of its newest output rather
                // than dying at a byte count: a long build's failure is at the
                // end. Evicted bytes stay counted in the cursor space, so a
                // reader behind the window learns exactly how much it missed.
                let window = limits.max_combined_output_bytes();
                while state.output.retained_bytes > window {
                    let excess = state.output.retained_bytes - window;
                    let Some(front) = state.output.records.pop_front() else {
                        break;
                    };
                    let front_bytes = u64::try_from(front.bytes.len()).unwrap_or(u64::MAX);
                    if front_bytes > excess {
                        let keep_from = usize::try_from(excess).unwrap_or(front.bytes.len());
                        state.output.records.push_front(JoinedProcessOutputRecord {
                            sequence: front.sequence,
                            cursor: front.cursor.saturating_add(excess),
                            stream: front.stream,
                            bytes: Arc::from(&front.bytes[keep_from..]),
                        });
                        state.output.retained_bytes = window;
                    } else {
                        state.output.retained_bytes -= front_bytes;
                    }
                }
            }
        })
        .map_err(|_| io_error("joined process output worker could not start"))
}

fn detach_output_reader(
    reader: &impl AsFd,
    stream: JoinedProcessOutputStream,
    shared: &Mutex<JoinedSessionShared>,
) -> Result<(), ExecutionBackendError> {
    let input = rustix::io::dup(reader)
        .map(fs::File::from)
        .map_err(|_| io_error("released output could not be transferred"))?;
    let flags = rustix::fs::fcntl_getfl(&input)
        .map_err(|_| io_error("released output flags unavailable"))?;
    rustix::fs::fcntl_setfl(&input, flags & !OFlags::NONBLOCK)
        .map_err(|_| io_error("released output could not become blocking"))?;
    let (output, room) = lock(shared)
        .spool
        .as_mut()
        .and_then(|spool| {
            spool.detached_output(if stream == JoinedProcessOutputStream::Terminal {
                1
            } else {
                2
            })
        })
        .map_or_else(
            || (Stdio::null(), 0),
            |(file, room)| (Stdio::from(file), room),
        );
    // A thread would die with `plak run`. This deliberately unowned, EOF-bound
    // drain process keeps the service's pipe readable after the product exits.
    let mut drain = Command::new("/bin/sh")
        .args([
            "-c",
            "trap '' HUP; head -c \"$1\"; exec cat >/dev/null",
            "released-output",
            &room.to_string(),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .process_group(0)
        .stdin(input)
        .stdout(output)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| io_error("released output drainer could not start"))?;
    std::thread::Builder::new()
        .name(String::from("plak-released-output"))
        .spawn(move || {
            let _ = drain.wait();
        })
        .map_err(|_| io_error("released output drainer could not be reaped"))?;
    Ok(())
}

pub(super) fn spawn_output_reader<R>(
    reader: R,
    stream: HostProcessStream,
    output: Arc<Mutex<HostProcessOutput>>,
    exceeded: Arc<AtomicBool>,
    io_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    limits: ExecutionEnvironmentLimits,
) -> Result<std::thread::JoinHandle<()>, ExecutionBackendError>
where
    R: AsFd + Read + Send + 'static,
{
    set_nonblocking(&reader)?;
    std::thread::Builder::new()
        .name(match stream {
            HostProcessStream::Stdout => String::from("plak-process-stdout"),
            HostProcessStream::Stderr => String::from("plak-process-stderr"),
        })
        .spawn(move || {
            read_bounded_process_output(
                reader, stream, &output, &exceeded, &io_failed, &stop, limits,
            );
        })
        .map_err(|_| io_error("host process output worker could not start"))
}

fn read_bounded_process_output<R: Read>(
    mut reader: R,
    stream: HostProcessStream,
    output: &Mutex<HostProcessOutput>,
    exceeded: &AtomicBool,
    io_failed: &AtomicBool,
    stop: &AtomicBool,
    limits: ExecutionEnvironmentLimits,
) {
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                io_failed.store(true, Ordering::Release);
                return;
            }
        };
        // A nonblocking pipe can remain perpetually readable when a detached
        // descendant writes continuously. Observe a stop that raced the
        // successful read before accounting or retaining that chunk.
        if stop.load(Ordering::Acquire) {
            return;
        }
        let mut collected = lock(output);
        let stdout_len = u64::try_from(collected.stdout.len()).unwrap_or(u64::MAX);
        let stderr_len = u64::try_from(collected.stderr.len()).unwrap_or(u64::MAX);
        let read_len = u64::try_from(read).unwrap_or(u64::MAX);
        let stream_would_exceed = match stream {
            HostProcessStream::Stdout => {
                stdout_len.saturating_add(read_len) > limits.max_stdout_bytes()
            }
            HostProcessStream::Stderr => {
                stderr_len.saturating_add(read_len) > limits.max_stderr_bytes()
            }
        };
        let combined_would_exceed = stdout_len
            .saturating_add(stderr_len)
            .saturating_add(read_len)
            > limits.max_combined_output_bytes();
        if stream_would_exceed || combined_would_exceed {
            exceeded.store(true, Ordering::Release);
            return;
        }
        match stream {
            HostProcessStream::Stdout => collected.stdout.extend_from_slice(&chunk[..read]),
            HostProcessStream::Stderr => collected.stderr.extend_from_slice(&chunk[..read]),
        }
    }
}

pub(super) fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> Result<(), ExecutionBackendError> {
    let flags = rustix::fs::fcntl_getfl(fd)
        .map_err(|_| io_error("host process pipe flags could not be read"))?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)
        .map_err(|_| io_error("host process pipe could not be made nonblocking"))
}

pub(super) fn write_process_input(
    mut stdin: std::process::ChildStdin,
    bytes: &[u8],
    stop: &AtomicBool,
) {
    let mut written = 0_usize;
    let mut blocked_since = Instant::now();
    while written < bytes.len() && !stop.load(Ordering::Acquire) {
        match stdin.write(&bytes[written..]) {
            Ok(0) => return,
            Ok(count) => {
                written = written.saturating_add(count);
                blocked_since = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(supervision_poll_interval(blocked_since.elapsed()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}
