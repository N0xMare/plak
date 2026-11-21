//! Bounded output shared by a connection's protocol driver and transport writer.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use plakous_hosted::error::CliResult;
use plakous_hosted::host_service::wire::HostConnection;
use tokio::sync::Notify;

use super::send::{MAX_PENDING_BYTES, Progress};
use crate::surface_protocol::PlakExtension;

const MAX_PENDING_FRAMES: usize = 8192;

pub(crate) type Connection = HostConnection<PlakExtension, Outbox>;

#[derive(Clone, Default)]
pub(crate) struct Outbox(Arc<Shared>);

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    ready: Notify,
    drained: Notify,
}

#[derive(Default)]
struct Pending {
    control: VecDeque<Frame>,
    ordered: VecDeque<Frame>,
    bytes: usize,
    frames: usize,
    // The transport still owns a frame until its send completes.
    ordered_frames: usize,
    overflowed: bool,
    closed: bool,
}

pub(crate) enum Frame {
    Line(Vec<u8>),
    Ping,
}

impl Frame {
    fn len(&self) -> usize {
        match self {
            Self::Line(line) => line.len(),
            Self::Ping => 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SendError {
    Full,
    Stalled,
    Disconnected,
}

pub(crate) trait FrameWriter: Send {
    fn send_frame(&mut self, frame: Frame) -> impl Future<Output = Result<(), SendError>> + Send;
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.0
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, frame: Frame, control: bool) -> io::Result<()> {
        let len = frame.len();
        let mut queue = self.lock();
        if queue.closed || queue.overflowed {
            return Err(io::Error::other("the client fell behind or left"));
        }
        if queue.bytes + len > MAX_PENDING_BYTES || queue.frames == MAX_PENDING_FRAMES {
            queue.overflowed = true;
            drop(queue);
            self.0.ready.notify_one();
            return Err(io::Error::other("the client fell behind"));
        }
        queue.bytes += len;
        queue.frames += 1;
        if control {
            queue.control.push_back(frame);
        } else {
            queue.ordered_frames += 1;
            queue.ordered.push_back(frame);
        }
        drop(queue);
        self.0.ready.notify_one();
        Ok(())
    }

    pub(crate) fn ping(&self) -> io::Result<()> {
        self.push(Frame::Ping, true)
    }

    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.0.ready.notify_one();
    }

    pub(crate) fn overflowed(&self) -> bool {
        self.lock().overflowed
    }

    pub(crate) async fn drained(&self) {
        self.0.drained.notified().await;
    }

    pub(crate) fn flush_connection(&self, connection: &mut Connection) -> CliResult<()> {
        if self.lock().ordered_frames == 0 {
            // Draft verdicts and lifecycle notices are in the ordered lane.
            // Only the actual end of their transport sends can settle delivery.
            connection.output_sent();
            connection.flush()
        } else {
            // Leave attachment observations in their bounded service queues
            // while the previous batch drains. Those queues can coalesce and
            // signal a gap; the outbox must not become a second replay store.
            connection.flush_host_reads()
        }
    }

    pub(crate) async fn write_to(
        &self,
        progress: &Progress,
        stopped: impl Fn() -> bool + Send + Sync,
        writer: &mut impl FrameWriter,
    ) -> Result<(), SendError> {
        loop {
            self.send_batch(progress, &stopped, writer).await?;
            let (pending, closed) = {
                let queue = self.lock();
                (queue.frames > 0 || queue.overflowed, queue.closed)
            };
            if pending {
                continue;
            }
            if closed {
                return Ok(());
            }
            self.0.ready.notified().await;
        }
    }

    pub(crate) async fn send_batch(
        &self,
        progress: &Progress,
        stopped: impl Fn() -> bool + Send + Sync,
        writer: &mut impl FrameWriter,
    ) -> Result<(), SendError> {
        progress
            .finishing(
                async {
                    while let Some((frame, ordered)) = self.pop()? {
                        let len = frame.len();
                        writer.send_frame(frame).await?;
                        if self.sent(len, ordered) {
                            // The actor can now produce another ordered batch.
                            // Its progress deadline is independent of this one.
                            break;
                        }
                    }
                    Ok(())
                },
                stopped,
            )
            .await
            .map_err(|()| SendError::Stalled)?
    }

    fn pop(&self) -> Result<Option<(Frame, bool)>, SendError> {
        let mut queue = self.lock();
        if queue.overflowed {
            return Err(SendError::Full);
        }
        Ok(queue
            .control
            .pop_front()
            .map(|frame| (frame, false))
            .or_else(|| queue.ordered.pop_front().map(|frame| (frame, true))))
    }

    fn sent(&self, len: usize, ordered: bool) -> bool {
        let mut queue = self.lock();
        queue.bytes -= len;
        queue.frames -= 1;
        if ordered {
            queue.ordered_frames -= 1;
            if queue.ordered_frames == 0 {
                self.0.drained.notify_one();
                return true;
            }
        }
        false
    }
}

impl Write for Outbox {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        // Hosted's wire writer supplies the entire encoded line in one write.
        if !line.ends_with(b"\n") {
            return Err(io::Error::other("incomplete wire frame"));
        }
        self.push(Frame::Line(line.to_vec()), independent_reply(line))?;
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Classify only the short, fixed prefix of our canonical serializer's
/// output, not untrusted client JSON or the body of large frames. Unknown
/// frames default to FIFO, as do any replies that open, end or move an
/// attachment or terminal stream, and picker progress with its final reply.
fn independent_reply(line: &[u8]) -> bool {
    let Some(id) = line.strip_prefix(br#"{"frame":"response","id":"#) else {
        return false;
    };
    let digits = id
        .iter()
        .take(20)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let Some(outcome) = id[digits..].strip_prefix(br#","outcome":{"#) else {
        return false;
    };
    if let Some(answer) = outcome.strip_prefix(br#""ok":{"answer":""#) {
        return answer.starts_with(b"pong\"")
            || answer.starts_with(b"accepted\"")
            || answer
                .strip_prefix(br#"host","response":{"response":""#)
                .is_some_and(|response| !response.starts_with(b"terminal"));
    }
    outcome
        .strip_prefix(br#""refused":{"refusal":""#)
        .is_some_and(|refusal| {
            [
                b"rate_limited\"".as_slice(),
                b"capability_disabled\"",
                b"deadline_exceeded\"",
            ]
            .iter()
            .any(|prefix| refusal.starts_with(prefix))
        })
}

// A frame handler can open a session; a flush can still read product state.
// The writer is a separate task, so neither operation parks it or a runtime
// worker. Current-thread tests use only in-memory host fixtures.
pub(crate) fn connection_work<T>(run: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::CurrentThread
    {
        return run();
    }
    tokio::task::block_in_place(run)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "canonical wire fixtures")]
mod tests {
    use super::*;
    use plakous_host_protocol::wire::{Answer, HostFrame, Outcome, encode_line};

    #[test]
    fn control_overtakes_bulk_without_crossing_ordered_attachment_boundaries() {
        let mut outbox = Outbox::default();
        let reply = |id, answer| {
            encode_line(&HostFrame::<PlakExtension>::Response {
                id,
                outcome: Outcome::Ok(answer),
            })
            .unwrap()
        };
        let with_bulk = [
            reply(1, Answer::Attached { attachment: 4 }),
            "{\"frame\":\"snapshot\",\"page\":0}\n".to_owned(),
            "{\"frame\":\"snapshot\",\"page\":1}\n".to_owned(),
            "{\"frame\":\"blob\"}\n".to_owned(),
            "{\"frame\":\"event\"}\n".to_owned(),
            reply(2, Answer::Detached),
            reply(u64::MAX, Answer::Pong),
            reply(3, Answer::Accepted { duplicate: false }),
            reply(
                4,
                Answer::Host {
                    response: plakd_wire_types::console::HostResponse::TerminalAccepted {},
                },
            ),
            reply(
                5,
                Answer::Host {
                    response: plakd_wire_types::console::HostResponse::OperationOutcomeUnknown {},
                },
            ),
        ];
        for line in &with_bulk {
            outbox.write_all(line.as_bytes()).unwrap();
        }
        for index in [6, 7, 9, 0, 1, 2, 3, 4, 5, 8] {
            let (Frame::Line(line), ordered) = outbox.pop().unwrap().unwrap() else {
                panic!("expected a text frame");
            };
            assert_eq!(line, with_bulk[index].as_bytes());
            outbox.sent(line.len(), ordered);
        }
        assert!(outbox.pop().unwrap().is_none());
    }

    #[test]
    fn an_in_flight_frame_still_counts_towards_backpressure() {
        let mut outbox = Outbox::default();
        for _ in 0..MAX_PENDING_FRAMES {
            outbox.write_all(b"{}\n").unwrap();
        }
        assert!(outbox.pop().unwrap().is_some());
        assert!(outbox.write_all(b"{}\n").is_err());
        assert!(matches!(outbox.pop(), Err(SendError::Full)));
    }
}
