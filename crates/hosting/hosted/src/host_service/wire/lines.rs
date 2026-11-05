//! What a served connection waits on: its client's lines and its session's
//! wakes in one channel, so [`WireConnection::serve`](super::WireConnection::serve)
//! blocks on one receiver and takes what the session said the moment it
//! says it, rather than polling either source.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use plakous_host_protocol::wire::WireError;

/// One client line, as its reader split it.
pub type Line = Result<String, WireError>;

enum Inbound {
    Line(Line),
    Woken,
}

/// What the client's reader sends its lines with; the lines end when the
/// last of its clones is dropped.
pub struct LineSender {
    sender: mpsc::SyncSender<Inbound>,
    open: Arc<AtomicUsize>,
}

impl LineSender {
    /// Sends one line, waiting while the channel is full.
    ///
    /// # Errors
    ///
    /// The connection stopped reading.
    pub fn send(&self, line: Line) -> Result<(), Line> {
        self.sender
            .send(Inbound::Line(line))
            .map_err(|mpsc::SendError(inbound)| match inbound {
                Inbound::Line(line) => line,
                Inbound::Woken => unreachable!("a line sender sends only lines"),
            })
    }
}

impl Clone for LineSender {
    fn clone(&self) -> Self {
        self.open.fetch_add(1, Ordering::AcqRel);
        Self {
            sender: self.sender.clone(),
            open: Arc::clone(&self.open),
        }
    }
}

impl Drop for LineSender {
    fn drop(&mut self) {
        if self.open.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The receiver learns of the end by looking; a full channel
            // wakes it anyway.
            let _ = self.sender.try_send(Inbound::Woken);
        }
    }
}

/// A connection's client lines, with its session's wakes among them.
pub struct Lines {
    receiver: mpsc::Receiver<Inbound>,
    waker: mpsc::SyncSender<Inbound>,
    open: Arc<AtomicUsize>,
}

/// What [`Lines::next`] found.
pub(super) enum Next {
    /// A client line.
    Line(Line),
    /// The session said something, or nothing came within the wait.
    Woken,
    /// The client's lines ended.
    Closed,
}

/// A channel of at most `bound` client lines.
#[must_use]
pub fn lines(bound: usize) -> (LineSender, Lines) {
    let (sender, receiver) = mpsc::sync_channel(bound);
    let open = Arc::new(AtomicUsize::new(1));
    (
        LineSender {
            sender: sender.clone(),
            open: Arc::clone(&open),
        },
        Lines {
            receiver,
            waker: sender,
            open,
        },
    )
}

impl Lines {
    /// The next client line within `timeout`.
    ///
    /// # Errors
    ///
    /// `Timeout` when none came, `Disconnected` once the lines ended.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Line, mpsc::RecvTimeoutError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.next(Some(left)) {
                Next::Line(line) => return Ok(line),
                Next::Closed => return Err(mpsc::RecvTimeoutError::Disconnected),
                Next::Woken if left.is_zero() => return Err(mpsc::RecvTimeoutError::Timeout),
                Next::Woken => {}
            }
        }
    }

    /// The next client line, however long it takes; `None` once the lines
    /// ended.
    #[must_use]
    pub fn recv(&self) -> Option<Line> {
        loop {
            match self.next(None) {
                Next::Line(line) => return Some(line),
                Next::Closed => return None,
                Next::Woken => {}
            }
        }
    }

    /// The hook that wakes a connection waiting on these lines. It never
    /// blocks: a full channel already wakes the connection.
    pub(super) fn waker(&self) -> impl Fn() + Send + Sync + 'static {
        let waker = self.waker.clone();
        move || {
            let _ = waker.try_send(Inbound::Woken);
        }
    }

    /// Waits for a wake, at most `wait` (`None`: until one comes), taking
    /// any line that arrives instead: the client is gone.
    pub(super) fn woken(&self, wait: Option<Duration>) {
        let _ = match wait {
            None => self.receiver.recv().map_err(|_| ()),
            Some(wait) => self.receiver.recv_timeout(wait).map_err(|_| ()),
        };
    }

    /// The next line or wake, waiting at most `wait` (`None`: until one
    /// comes).
    pub(super) fn next(&self, wait: Option<Duration>) -> Next {
        let inbound = match self.receiver.try_recv() {
            Ok(inbound) => Ok(inbound),
            Err(_) if self.open.load(Ordering::Acquire) == 0 => return Next::Closed,
            Err(_) => match wait {
                None => self.receiver.recv().map_err(|_| ()),
                Some(wait) => self.receiver.recv_timeout(wait).map_err(|_| ()),
            },
        };
        match inbound {
            Ok(Inbound::Line(line)) => Next::Line(line),
            Ok(Inbound::Woken) | Err(()) => Next::Woken,
        }
    }
}
