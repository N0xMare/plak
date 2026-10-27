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
mod tests {
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::time::{Duration, Instant};

    fn input() -> (OwnedFd, OwnedFd) {
        let (read, write) = std::io::pipe().expect("input pipe");
        (read.into(), write.into())
    }

    /// Wakes written before the wait return it at once and are consumed
    /// together, so the next bounded wait runs out its bound.
    #[test]
    fn pending_wakes_return_the_wait_and_are_consumed() {
        let pipe = super::open_pipe().expect("wake pipe");
        let (input, _writer) = input();
        pipe.ring();
        pipe.ring();
        let started = Instant::now();
        pipe.wait_on(Some(input.as_fd()), None).expect("wait");
        assert!(started.elapsed() < Duration::from_secs(5));
        let started = Instant::now();
        pipe.wait_on(Some(input.as_fd()), Some(Duration::from_millis(50)))
            .expect("wait");
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    /// A wake from another thread ends an unbounded wait.
    #[test]
    fn a_wake_from_another_thread_ends_an_unbounded_wait() {
        let pipe = std::sync::Arc::new(super::open_pipe().expect("wake pipe"));
        let (input, _writer) = input();
        let ringer = std::sync::Arc::clone(&pipe);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            ringer.ring();
        });
        pipe.wait_on(Some(input.as_fd()), None).expect("wait");
        thread.join().expect("ringer");
    }

    /// Readable input ends the wait without a wake, and says so; a wait
    /// that does not watch the input times out over the same bytes.
    #[test]
    fn readable_input_ends_a_watching_wait_only() {
        let pipe = super::open_pipe().expect("wake pipe");
        let (input, writer) = input();
        rustix::io::write(&writer, b"x").expect("input byte");
        assert_eq!(
            pipe.wait_on(Some(input.as_fd()), None).expect("wait"),
            super::Input::Readable
        );
        let started = Instant::now();
        assert_eq!(
            pipe.wait_on(None, Some(Duration::from_millis(30)))
                .expect("wait"),
            super::Input::Quiet
        );
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    /// A readable wake whose drain takes no event leaves bytes crossterm
    /// reads only on the next arrival: the loop stops polling them and looks
    /// again on the bounded tick until an event comes through, so stranded
    /// input never spins the loop and never parks it deaf.
    #[test]
    fn stranded_input_is_looked_at_on_a_tick_until_it_drains() {
        use super::{Input, InputMode, InputWatch, STRANDED_INPUT_TICK};
        let tick = Some(STRANDED_INPUT_TICK);
        let mut watch = InputWatch::default();
        assert_eq!(watch.next_wait(None), (None, InputMode::Watch));
        // Bytes that yield no event: stop waiting on them, look on a tick.
        watch.observe(Input::Readable, 0);
        assert_eq!(watch.next_wait(None), (tick, InputMode::Look));
        let soon = Duration::from_millis(5);
        assert_eq!(watch.next_wait(Some(soon)), (Some(soon), InputMode::Look));
        watch.observe(Input::Readable, 0);
        assert_eq!(watch.next_wait(None), (tick, InputMode::Look));
        // An arrival lets crossterm read: watch again, one look a tick on.
        watch.observe(Input::Readable, 2);
        assert_eq!(watch.next_wait(None), (tick, InputMode::Watch));
        // A look that finds the kernel empty ends the ticking.
        watch.observe(Input::Readable, 0);
        assert_eq!(watch.next_wait(None), (tick, InputMode::Look));
        watch.observe(Input::Quiet, 0);
        assert_eq!(watch.next_wait(None), (None, InputMode::Watch));
        // An unwatchable or hung-up descriptor is ticked for good.
        watch.observe(Input::Unpollable, 0);
        assert_eq!(watch.next_wait(None), (tick, InputMode::Ignore));
        let mut hung = InputWatch::default();
        hung.observe(Input::HungUp, 0);
        assert_eq!(hung.next_wait(None), (tick, InputMode::Ignore));
    }

    /// Crossterm's zero-timeout poll can stop at an internal reply it set
    /// aside (a cursor-position report, a keyboard-flags answer) while real
    /// events stay parsed behind it, and the drain then counts some events
    /// and stops. A readable wake therefore earns one more look a tick
    /// later, while still watching the descriptor, and then parks.
    #[test]
    fn every_readable_wake_earns_one_more_look() {
        use super::{Input, InputMode, InputWatch, STRANDED_INPUT_TICK};
        let mut watch = InputWatch::default();
        watch.observe(Input::Readable, 1);
        assert_eq!(
            watch.next_wait(None),
            (Some(STRANDED_INPUT_TICK), InputMode::Watch)
        );
        watch.observe(Input::Quiet, 1);
        assert_eq!(watch.next_wait(None), (None, InputMode::Watch));
    }

    /// A descriptor whose other end is gone reports a hang-up, not
    /// readable input crossterm would spin reading.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_closed_input_reports_a_hang_up() {
        let pipe = super::open_pipe().expect("wake pipe");
        let (input, writer) = input();
        drop(writer);
        assert_eq!(
            pipe.wait_on(Some(input.as_fd()), None).expect("wait"),
            super::Input::HungUp
        );
        assert_eq!(
            super::look(input.as_fd()).expect("look"),
            super::Input::HungUp
        );
    }
}
