//! The interactive loop's one wait point: terminal input and a wake pipe.
//!
//! The loop drains the crossterm reader and the host mailbox before every
//! wait, so waiting on the terminal descriptor misses no event crossterm has
//! already parsed. Everything else that must wake a parked loop writes one
//! byte to the pipe: the host mailbox's notifier, the stop-signal watcher,
//! `SIGWINCH`, the background binary hash, and usage-decoration completion.
//!
//! Crossterm's reader is edge-triggered and returns after the first event of
//! each 1 KiB chunk it reads, so a burst larger than that (a long unbracketed
//! paste, a flood of mouse reports behind a slow frame) can leave bytes in
//! the kernel that crossterm reads only after the next arrival. The level
//! `poll` here keeps reporting them. [`InputWatch`] notices a readable wake
//! that yields no event and stops waiting on the descriptor: it waits on the
//! pipe for a bounded tick and then looks at the descriptor without waiting,
//! until crossterm yields an event or the kernel holds nothing, so stranded
//! bytes cost a 20 ms tick and never a spin. Crossterm's zero-timeout poll
//! can also report nothing while it still holds parsed events (it stops at
//! an internal reply it set aside), so every readable wake is followed by
//! one more look a tick later. A descriptor `poll` cannot watch (macOS
//! answers `POLLNVAL` for `/dev/tty`) gets the tick for the session, and a
//! hung-up descriptor is reported so the loop quits instead of reading it.

use std::io::IsTerminal as _;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::sync::OnceLock;
use std::time::Duration;

struct WakePipe {
    read: OwnedFd,
    write: OwnedFd,
}

static PIPE: OnceLock<Option<WakePipe>> = OnceLock::new();

fn open_pipe() -> Option<WakePipe> {
    let (read, write) = std::io::pipe().ok()?;
    let (read, write) = (OwnedFd::from(read), OwnedFd::from(write));
    for end in [&read, &write] {
        rustix::fs::fcntl_setfl(end, rustix::fs::OFlags::NONBLOCK).ok()?;
    }
    Some(WakePipe { read, write })
}

/// Create the pipe before anything can need to wake the loop.
pub(super) fn arm() -> std::io::Result<()> {
    PIPE.get_or_init(open_pipe)
        .as_ref()
        .map(|_| ())
        .ok_or_else(|| std::io::Error::other("the interactive wake pipe could not be created"))
}

/// Wake a parked interactive loop. A no-op before [`arm`], and when a wake is
/// already pending (a full pipe refuses the byte).
pub(crate) fn wake() {
    if let Some(pipe) = PIPE.get().and_then(Option::as_ref) {
        pipe.ring();
    }
}

impl WakePipe {
    fn ring(&self) {
        let _ = rustix::io::write(&self.write, &[1]);
    }

    /// Block until `input` (when given) is readable, the pipe is written, a
    /// signal interrupts the wait, or `timeout` passes, then consume the
    /// pipe. Reports what the input descriptor showed.
    fn wait_on(
        &self,
        input: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> std::io::Result<Input> {
        use rustix::event::{PollFd, PollFlags, Timespec};
        // A bound too large for a timespec is no bound at all.
        let timeout = timeout.and_then(|timeout| Timespec::try_from(timeout).ok());
        let mut fds = vec![PollFd::new(&self.read, PollFlags::IN)];
        if let Some(input) = &input {
            fds.push(PollFd::new(input, PollFlags::IN));
        }
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
        let seen = fds
            .get(1)
            .map_or(Input::Quiet, |input| classify(input.revents()));
        // Drained before the caller drains the mailbox: a byte is written
        // only after its observation is queued, so none is consumed unseen.
        let mut sink = [0_u8; 64];
        while matches!(rustix::io::read(&self.read, &mut sink), Ok(count) if count == sink.len()) {}
        Ok(seen)
    }
}

/// What one wait saw on the terminal input descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Input {
    /// Not readable, or not looked at by this wait.
    Quiet,
    /// Readable: crossterm has bytes to read, or bytes it will not read
    /// until more arrive.
    Readable,
    /// The terminal hung up: an error, or a hang-up with nothing to read.
    /// Crossterm's reader would spin on it.
    HungUp,
    /// `poll` cannot watch this descriptor.
    Unpollable,
}

fn classify(revents: rustix::event::PollFlags) -> Input {
    use rustix::event::PollFlags;
    if revents.contains(PollFlags::NVAL) {
        Input::Unpollable
    } else if revents.contains(PollFlags::ERR)
        || (revents.contains(PollFlags::HUP) && !revents.contains(PollFlags::IN))
    {
        Input::HungUp
    } else if revents.contains(PollFlags::IN) {
        Input::Readable
    } else {
        Input::Quiet
    }
}

/// How one wait treats the terminal input descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InputMode {
    /// Wait on it with the pipe.
    Watch,
    /// Wait on the pipe only, then look at it without waiting.
    Look,
    /// Leave it alone: `poll` cannot watch it.
    Ignore,
}

/// The bounded look at stranded input while the descriptor is not polled.
pub(super) const STRANDED_INPUT_TICK: Duration = Duration::from_millis(20);

/// How the next wait treats the terminal descriptor.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct InputWatch {
    stranded: bool,
    look_again: bool,
    unpollable: bool,
}

impl InputWatch {
    /// Record what the last wait saw and how many events the drain after it
    /// took. A readable descriptor that yields nothing holds bytes crossterm
    /// reads only after the next arrival; an event means it is reading again,
    /// and a quiet look means the kernel holds nothing more. Any readable
    /// wake earns one more look a tick later, for events crossterm parsed
    /// but did not report.
    pub(super) fn observe(&mut self, seen: Input, events: usize) {
        match seen {
            Input::Unpollable | Input::HungUp => self.unpollable = true,
            Input::Readable if events == 0 => self.stranded = true,
            Input::Quiet => self.stranded = false,
            Input::Readable => {}
        }
        if events > 0 {
            self.stranded = false;
        }
        self.look_again = seen == Input::Readable;
    }

    /// The next wait's bound and how it treats the descriptor.
    pub(super) fn next_wait(self, bound: Option<Duration>) -> (Option<Duration>, InputMode) {
        let tick = Some(bound.map_or(STRANDED_INPUT_TICK, |bound| bound.min(STRANDED_INPUT_TICK)));
        if self.unpollable {
            (tick, InputMode::Ignore)
        } else if self.stranded {
            (tick, InputMode::Look)
        } else if self.look_again {
            (tick, InputMode::Watch)
        } else {
            (bound, InputMode::Watch)
        }
    }
}

/// The descriptor crossterm reads: standard input when it is the terminal,
/// otherwise the controlling terminal it opens itself.
fn with_input<T>(wait: impl FnOnce(BorrowedFd<'_>) -> std::io::Result<T>) -> std::io::Result<T> {
    static TTY: OnceLock<Option<std::fs::File>> = OnceLock::new();
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return wait(stdin.as_fd());
    }
    let tty = TTY
        .get_or_init(|| std::fs::File::open("/dev/tty").ok())
        .as_ref()
        .ok_or_else(|| std::io::Error::other("no terminal to read input from"))?;
    wait(tty.as_fd())
}

/// Block until terminal input is readable (in [`InputMode::Watch`]), a wake
/// arrives, a signal interrupts the wait, or `timeout` passes. `None` waits
/// without a bound: the parked loop consumes no CPU until one of those
/// happens. In [`InputMode::Look`] the descriptor is looked at once the
/// wait ends.
pub(super) fn wait(timeout: Option<Duration>, mode: InputMode) -> std::io::Result<Input> {
    let pipe = PIPE
        .get()
        .and_then(Option::as_ref)
        .ok_or_else(|| std::io::Error::other("the interactive wake pipe is not armed"))?;
    match mode {
        InputMode::Watch => with_input(|input| pipe.wait_on(Some(input), timeout)),
        InputMode::Ignore => pipe.wait_on(None, timeout),
        InputMode::Look => {
            pipe.wait_on(None, timeout)?;
            with_input(look)
        }
    }
}

/// What the descriptor shows right now, without waiting.
fn look(input: BorrowedFd<'_>) -> std::io::Result<Input> {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let zero = Timespec::try_from(Duration::ZERO).map_err(|_| std::io::Error::other("timespec"))?;
    let mut fds = [PollFd::new(&input, PollFlags::IN)];
    match rustix::event::poll(&mut fds, Some(&zero)) {
        Ok(_) => Ok(classify(fds[0].revents())),
        Err(rustix::io::Errno::INTR) => Ok(Input::Readable),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests;
