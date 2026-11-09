//! Bounded, atomic blob/request batches. Withdrawal and starting a write
//! share a lock: once withdrawn, no byte of that batch can reach the pipe.

use super::{Arc, Condvar, Mutex, Write, lock};
use std::collections::VecDeque;
use std::io;
use std::sync::Weak;
use std::time::Instant;

pub(super) const MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_FRAMES: usize = 64;

#[derive(Default)]
pub(super) struct Queue {
    pending: Mutex<Pending>,
    changed: Condvar,
}

#[derive(Default)]
struct Pending {
    batches: VecDeque<Batch>,
    bytes: usize,
    frames: usize,
    closed: bool,
    finishing: bool,
}

struct Batch {
    lines: String,
    frames: usize,
    status: Arc<Mutex<Status>>,
    deadline: Option<Instant>,
}

enum Status {
    Queued,
    Writing,
    Written,
    Withdrawn,
}

pub(super) struct Ticket {
    queue: Weak<Queue>,
    status: Arc<Mutex<Status>>,
}

impl Ticket {
    /// False means writing already began; even a partial line is unknown.
    pub(super) fn withdraw(&self) -> bool {
        let Some(queue) = self.queue.upgrade() else {
            return false;
        };
        let mut pending = lock(&queue.pending);
        let mut status = lock(&self.status);
        if !matches!(*status, Status::Queued) {
            return matches!(*status, Status::Withdrawn);
        }
        let Some(index) = pending
            .batches
            .iter()
            .position(|batch| Arc::ptr_eq(&batch.status, &self.status))
        else {
            return false;
        };
        if let Some(batch) = pending.batches.remove(index) {
            pending.bytes -= batch.lines.len();
            pending.frames -= batch.frames;
        }
        *status = Status::Withdrawn;
        true
    }
}

impl Queue {
    pub(super) fn push(self: &Arc<Self>, lines: String) -> io::Result<Ticket> {
        self.reserve(lines, None)
    }

    pub(super) fn reserve(
        self: &Arc<Self>,
        lines: String,
        deadline: Option<Instant>,
    ) -> io::Result<Ticket> {
        let frames = lines.bytes().filter(|byte| *byte == b'\n').count();
        let mut pending = lock(&self.pending);
        if pending.closed || pending.finishing {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the writer stopped",
            ));
        }
        if pending.bytes.saturating_add(lines.len()) > MAX_BYTES
            || pending.frames.saturating_add(frames) > MAX_FRAMES
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the writer is full",
            ));
        }
        let status = Arc::new(Mutex::new(Status::Queued));
        pending.bytes += lines.len();
        pending.frames += frames;
        pending.batches.push_back(Batch {
            lines,
            frames,
            status: Arc::clone(&status),
            deadline,
        });
        self.changed.notify_one();
        Ok(Ticket {
            queue: Arc::downgrade(self),
            status,
        })
    }

    pub(super) fn close(&self) {
        let mut pending = lock(&self.pending);
        pending.closed = true;
        for batch in pending.batches.drain(..) {
            *lock(&batch.status) = Status::Withdrawn;
        }
        self.changed.notify_all();
    }

    pub(super) fn finish(&self) {
        lock(&self.pending).finishing = true;
        self.changed.notify_all();
    }

    pub(super) fn run(&self, mut writer: Box<dyn Write + Send>) -> io::Result<()> {
        loop {
            let batch = {
                let mut pending = lock(&self.pending);
                while pending.batches.is_empty() && !pending.closed && !pending.finishing {
                    pending = self
                        .changed
                        .wait(pending)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if pending.closed || (pending.finishing && pending.batches.is_empty()) {
                    return Ok(());
                }
                let Some(batch) = pending.batches.pop_front() else {
                    continue;
                };
                if batch
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    *lock(&batch.status) = Status::Withdrawn;
                    pending.bytes -= batch.lines.len();
                    pending.frames -= batch.frames;
                    continue;
                }
                *lock(&batch.status) = Status::Writing;
                batch
            };
            writer.write_all(batch.lines.as_bytes())?;
            writer.flush()?;
            let mut pending = lock(&self.pending);
            *lock(&batch.status) = Status::Written;
            pending.bytes -= batch.lines.len();
            pending.frames -= batch.frames;
        }
    }
}
