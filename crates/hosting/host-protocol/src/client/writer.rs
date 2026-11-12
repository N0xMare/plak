//! Bounded, atomic blob/request batches. Withdrawal and starting a write
//! share a lock: once withdrawn, no byte of that batch can reach the pipe.

use super::{Arc, Condvar, Duration, Mutex, Write, lock};
use std::collections::VecDeque;
use std::io;
use std::sync::Weak;
use std::time::Instant;

pub(super) const MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_FRAMES: usize = 64;
pub(super) const INTENT_BYTES: usize = MAX_BYTES - 1024 * 1024;
pub(super) const INTENT_FRAMES: usize = MAX_FRAMES - 8;

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
    goodbye: bool,
    completion: WriterCompletion,
}

#[derive(Default)]
enum WriterCompletion {
    #[default]
    Pending,
    Finished,
}

pub(super) struct Completion<'a>(&'a Queue);

impl Drop for Completion<'_> {
    fn drop(&mut self) {
        lock(&self.0.pending).completion = WriterCompletion::Finished;
        self.0.changed.notify_all();
    }
}

struct Batch {
    lines: String,
    frames: usize,
    status: Arc<Mutex<Status>>,
    deadline: Option<Instant>,
}

impl Pending {
    fn withdraw_suffix(&mut self, start: usize) {
        let mut index = start;
        while index < self.batches.len() {
            if self.batches[index].deadline.is_none() {
                index += 1;
                continue;
            }
            if let Some(batch) = self.batches.remove(index) {
                self.bytes -= batch.lines.len();
                self.frames -= batch.frames;
                *lock(&batch.status) = Status::Withdrawn;
            }
        }
    }
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
        let status = lock(&self.status);
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
        drop(status);
        if pending.batches[index].deadline.is_none() {
            if let Some(batch) = pending.batches.remove(index) {
                pending.bytes -= batch.lines.len();
                pending.frames -= batch.frames;
                *lock(&batch.status) = Status::Withdrawn;
            }
        } else {
            pending.withdraw_suffix(index);
        }
        true
    }

    pub(super) fn withdrawn(&self) -> bool {
        matches!(*lock(&self.status), Status::Withdrawn)
    }
}

impl Queue {
    pub(super) fn completion(&self) -> Completion<'_> {
        Completion(self)
    }

    pub(super) fn wait_finished(&self, timeout: Duration) {
        drop(
            self.changed
                .wait_timeout_while(lock(&self.pending), timeout, |pending| {
                    !matches!(pending.completion, WriterCompletion::Finished) && !pending.closed
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

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
        let (bytes_limit, frames_limit) = if deadline.is_some() {
            (INTENT_BYTES, INTENT_FRAMES)
        } else {
            (MAX_BYTES, MAX_FRAMES)
        };
        if pending.bytes.saturating_add(lines.len()) > bytes_limit
            || pending.frames.saturating_add(frames) > frames_limit
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                if deadline.is_some() {
                    "the intent writer is full"
                } else {
                    "the control writer is full"
                },
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

    #[cfg(test)]
    pub(super) fn finish(&self) {
        lock(&self.pending).finishing = true;
        self.changed.notify_all();
    }

    pub(super) fn goodbye(&self) {
        let mut pending = lock(&self.pending);
        if !pending.closed && !pending.finishing {
            pending.finishing = true;
            pending.goodbye = true;
        }
        self.changed.notify_all();
    }

    pub(super) fn run(
        &self,
        mut writer: Box<dyn Write + Send>,
        written: impl Fn(),
    ) -> io::Result<()> {
        loop {
            let batch = {
                let mut pending = lock(&self.pending);
                while pending.batches.is_empty() && !pending.closed && !pending.finishing {
                    pending = self
                        .changed
                        .wait(pending)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if pending.closed {
                    return Ok(());
                }
                if pending.finishing && pending.batches.is_empty() {
                    let goodbye = pending.goodbye;
                    drop(pending);
                    if goodbye {
                        writer.write_all(super::BYE_LINE.as_bytes())?;
                        writer.flush()?;
                    }
                    return Ok(());
                }
                if pending
                    .batches
                    .front()
                    .and_then(|batch| batch.deadline)
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    pending.withdraw_suffix(0);
                    continue;
                }
                let Some(batch) = pending.batches.pop_front() else {
                    continue;
                };
                *lock(&batch.status) = Status::Writing;
                batch
            };
            writer.write_all(batch.lines.as_bytes())?;
            writer.flush()?;
            let mut pending = lock(&self.pending);
            *lock(&batch.status) = Status::Written;
            pending.bytes -= batch.lines.len();
            pending.frames -= batch.frames;
            drop(pending);
            written();
        }
    }
}
