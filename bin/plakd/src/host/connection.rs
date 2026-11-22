//! Bounded output shared by a connection's protocol driver and transport writer.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use plakous_host_protocol::views::presentation::PresentationSnapshot;
use plakous_host_protocol::wire::AttachmentId;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::HostConnection;
use tokio::sync::Notify;

use super::send::{MAX_PENDING_BYTES, Progress};
use crate::surface_protocol::PlakExtension;

mod snapshot;

const MAX_PENDING_FRAMES: usize = 8192;
const MAX_SNAPSHOT_JOBS: usize = 2;

pub(crate) type Connection = HostConnection<PlakExtension, Outbox>;

#[derive(Clone, Default)]
pub(crate) struct Outbox(Arc<Shared>);

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    ready: Arc<Notify>,
    drained: Notify,
}

#[derive(Default)]
struct Pending {
    control: VecDeque<Frame>,
    ordered: VecDeque<Ordered>,
    bytes: usize,
    frames: usize,
    // The transport still owns a frame until its send completes.
    ordered_frames: usize,
    snapshots: usize,
    overflowed: bool,
    closed: bool,
}

enum Ordered {
    Line(Vec<u8>),
    Snapshot(snapshot::Job),
}

impl Ordered {
    fn ready(&self) -> bool {
        match self {
            Self::Line(_) => true,
            Self::Snapshot(job) => job.ready(),
        }
    }
}

impl Shared {
    fn charge_snapshot(&self, pages: &snapshot::Pages) -> Result<(), SendError> {
        let mut queue = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        // Replace the placeholder before publishing the completed job. The
        // writer may have drained preceding lines while encoding was running.
        let frames = queue.frames - 1 + pages.lines.len();
        let bytes = queue.bytes + pages.bytes;
        if queue.overflowed || frames > MAX_PENDING_FRAMES || bytes > MAX_PENDING_BYTES {
            queue.overflowed = true;
            return Err(SendError::Full);
        }
        queue.frames = frames;
        queue.bytes = bytes;
        queue.ordered_frames = queue.ordered_frames - 1 + pages.lines.len();
        queue.snapshots -= 1;
        Ok(())
    }
}

/// A complete wire line, or a transport-native heartbeat (no wire payload).
pub(crate) type Frame = Option<Vec<u8>>;

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

    fn reserve(&self, queue: &mut Pending, bytes: usize, frames: usize) -> io::Result<()> {
        if queue.closed || queue.overflowed {
            return Err(io::Error::other("the client fell behind or left"));
        }
        if queue.bytes + bytes > MAX_PENDING_BYTES || queue.frames + frames > MAX_PENDING_FRAMES {
            queue.overflowed = true;
            self.0.ready.notify_one();
            return Err(io::Error::other("the client fell behind"));
        }
        queue.bytes += bytes;
        queue.frames += frames;
        Ok(())
    }

    pub(crate) fn control(&self, frame: Frame) -> io::Result<()> {
        let mut queue = self.lock();
        self.reserve(&mut queue, frame.as_ref().map_or(0, Vec::len), 1)?;
        queue.control.push_back(frame);
        self.0.ready.notify_one();
        Ok(())
    }

    pub(crate) fn snapshot(
        &self,
        attachment: AttachmentId,
        snapshot: PresentationSnapshot,
    ) -> CliResult<()> {
        self.snapshot_with(attachment, snapshot, &snapshot::ENCODERS)
    }

    fn snapshot_with(
        &self,
        attachment: AttachmentId,
        snapshot: PresentationSnapshot,
        encoders: &'static tokio::sync::Semaphore,
    ) -> CliResult<()> {
        let mut queue = self.lock();
        if queue.snapshots == MAX_SNAPSHOT_JOBS {
            // The driver is the sole producer of ordered output. Let the
            // writer drain earlier frames while the driver encodes this one.
            drop(queue);
            return self.snapshot_inline(attachment, snapshot);
        }
        self.reserve(&mut queue, 0, 1)
            .map_err(|error| CliFailure::new(error.to_string()))?;
        queue.ordered_frames += 1;
        queue.snapshots += 1;
        let output = Arc::downgrade(&self.0);
        queue
            .ordered
            .push_back(Ordered::Snapshot(snapshot::Job::start(
                attachment,
                snapshot,
                encoders,
                Arc::clone(&self.0.ready),
                move |pages| {
                    output
                        .upgrade()
                        .ok_or(SendError::Disconnected)?
                        .charge_snapshot(pages)
                },
            )));
        // Preparation wakes the writer once this entry can be taken.
        Ok(())
    }

    fn snapshot_inline(
        &self,
        attachment: AttachmentId,
        snapshot: PresentationSnapshot,
    ) -> CliResult<()> {
        let pages = snapshot::encode(attachment, snapshot).map_err(|_| {
            CliFailure::new("the snapshot exceeded its wire bounds or could not be encoded")
        })?;
        let mut queue = self.lock();
        self.reserve(&mut queue, pages.bytes, pages.lines.len())
            .map_err(|error| CliFailure::new(error.to_string()))?;
        queue.ordered_frames += pages.lines.len();
        queue
            .ordered
            .extend(pages.lines.into_iter().map(Ordered::Line));
        self.0.ready.notify_one();
        Ok(())
    }

    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.0.ready.notify_one();
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
            connection.flush_control()
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
            let (runnable, drained) = {
                let queue = self.lock();
                (
                    queue.overflowed
                        || !queue.control.is_empty()
                        || queue.ordered.front().is_some_and(Ordered::ready),
                    queue.closed && queue.frames == 0,
                )
            };
            if runnable {
                continue;
            }
            if drained {
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
                        let len = frame.as_ref().map_or(0, Vec::len);
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
        if let Some(frame) = queue.control.pop_front() {
            return Ok(Some((frame, false)));
        }
        loop {
            if queue.ordered.front().is_some_and(|entry| !entry.ready()) {
                return Ok(None);
            }
            let pages = match queue.ordered.pop_front() {
                Some(Ordered::Snapshot(job)) => job.take()?,
                Some(Ordered::Line(line)) => return Ok(Some((Some(line), true))),
                None => return Ok(None),
            };
            for line in pages.lines.into_iter().rev() {
                queue.ordered.push_front(Ordered::Line(line));
            }
        }
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
        if independent_reply(line) {
            plakous_hosted::host_service::wire::test_timing::writer("enqueue", line);
            self.control(Some(line.to_vec()))?;
        } else {
            let mut queue = self.lock();
            self.reserve(&mut queue, line.len(), 1)?;
            queue.ordered_frames += 1;
            queue.ordered.push_back(Ordered::Line(line.to_vec()));
            self.0.ready.notify_one();
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Classify only the short, fixed prefix of our canonical serializer's
/// output, not untrusted client JSON or the body of large frames. Unknown
/// frames default to FIFO. Ambiguous, refused and terminal replies stay
/// FIFO because the same refusal or unknown outcome can answer a terminal
/// operation. Attachment boundaries and picker progress also stay FIFO.
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
    let Some(answer) = outcome.strip_prefix(br#""ok":{"answer":""#) else {
        return false;
    };
    answer.starts_with(b"pong\"")
        || answer.starts_with(b"accepted\"")
        || answer
            .strip_prefix(br#"host","response":{"response":""#)
            .is_some_and(|response| {
                !response.starts_with(b"terminal")
                    && !response.starts_with(b"refused\"")
                    && !response.starts_with(b"operation_outcome_unknown\"")
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
    use plakd_wire_types::console::{HostResponse, ReadRefusal};
    use plakous_host_protocol::views::transcript::TranscriptScope;
    use plakous_host_protocol::wire::{Answer, HostFrame, Outcome, RequestRefusal, encode_line};
    use std::time::Duration;

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
                    response: HostResponse::TerminalAccepted {},
                },
            ),
            reply(
                5,
                Answer::Host {
                    response: HostResponse::TerminalList { terminals: vec![] },
                },
            ),
            reply(
                6,
                Answer::Host {
                    response: HostResponse::ProfilesList {
                        etag: String::new(),
                        current: None,
                        rows: vec![],
                        diagnostics: vec![],
                        page: plakd_wire_types::console::Page {
                            next: None,
                            truncated: false,
                        },
                    },
                },
            ),
        ];
        for line in &with_bulk {
            outbox.write_all(line.as_bytes()).unwrap();
        }
        for index in [6, 7, 10, 0, 1, 2, 3, 4, 5, 8, 9] {
            let (line, ordered) = outbox.pop().unwrap().unwrap();
            let line = line.unwrap();
            assert_eq!(line, with_bulk[index].as_bytes());
            outbox.sent(line.len(), ordered);
        }
        assert!(outbox.pop().unwrap().is_none());
    }

    #[test]
    fn terminal_and_ambiguous_refusals_cannot_pass_ordered_output() {
        let outcomes = [
            Outcome::Refused(RequestRefusal::RateLimited { retry_after_ms: 1 }),
            Outcome::Refused(RequestRefusal::CapabilityDisabled {
                capability: plakous_host_protocol::wire::ConsoleCapability::AccountsRead,
            }),
            Outcome::Refused(RequestRefusal::DeadlineExceeded),
            Outcome::Ok(Answer::Host {
                response: HostResponse::OperationOutcomeUnknown {},
            }),
            Outcome::Ok(Answer::Host {
                response: HostResponse::Refused {
                    reason: ReadRefusal::TerminalBackpressure {
                        retry_after_ms: Some(1),
                    },
                },
            }),
        ];
        for outcome in outcomes {
            let line =
                encode_line(&HostFrame::<PlakExtension>::Response { id: 1, outcome }).unwrap();
            assert!(!independent_reply(line.as_bytes()), "{line}");
        }
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

    #[tokio::test]
    async fn a_pending_snapshot_leaves_control_writable_but_fences_following_events() {
        static TEST_ENCODER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let encoders = TEST_ENCODER.try_acquire().unwrap();
        let mut outbox = Outbox::default();
        let event = b"{\"frame\":\"event\"}\n";
        outbox
            .snapshot_with(4, PresentationSnapshot::default(), &TEST_ENCODER)
            .unwrap();
        outbox.write_all(event).unwrap();
        let pong = encode_line(&HostFrame::<PlakExtension>::Response {
            id: 42,
            outcome: Outcome::Ok(Answer::Pong),
        })
        .unwrap();
        outbox.write_all(pong.as_bytes()).unwrap();
        let (line, ordered) = outbox.pop().unwrap().unwrap();
        assert_eq!(line.unwrap(), pong.as_bytes());
        assert!(!ordered);
        outbox.sent(pong.len(), ordered);
        assert!(outbox.pop().unwrap().is_none());
        // Discard the notifications for the two lines already enqueued.
        tokio::time::timeout(Duration::from_secs(3), outbox.0.ready.notified())
            .await
            .unwrap();
        drop(encoders);
        tokio::time::timeout(Duration::from_secs(3), outbox.0.ready.notified())
            .await
            .unwrap();
        let (page, ordered) = outbox.pop().unwrap().unwrap();
        let page = page.unwrap();
        assert!(page.starts_with(br#"{"frame":"snapshot","attachment":4,"#));
        assert!(ordered);
        outbox.sent(page.len(), ordered);
        let (line, ordered) = outbox.pop().unwrap().unwrap();
        assert_eq!(line.unwrap(), event);
        assert!(ordered);
    }

    async fn next_ordered(outbox: &Outbox) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some((line, ordered)) = outbox.pop().unwrap() {
                    let line = line.unwrap();
                    assert!(ordered);
                    outbox.sent(line.len(), ordered);
                    return line;
                }
                outbox.0.ready.notified().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn one_flush_can_queue_root_and_agent_snapshots() {
        let outbox = Outbox::default();
        for scope in [
            TranscriptScope::Root,
            TranscriptScope::Agent("child".into()),
        ] {
            outbox
                .snapshot(
                    4,
                    PresentationSnapshot {
                        scope,
                        ..PresentationSnapshot::default()
                    },
                )
                .unwrap();
        }
        for scope in [
            TranscriptScope::Root,
            TranscriptScope::Agent("child".into()),
        ] {
            let line = next_ordered(&outbox).await;
            let page: HostFrame<PlakExtension> = serde_json::from_slice(&line).unwrap();
            assert!(matches!(page, HostFrame::Snapshot(page) if page.scope == scope && page.last));
        }
        assert!(outbox.pop().unwrap().is_none());
    }

    async fn assert_inline_snapshot_order(
        encoders: &'static tokio::sync::Semaphore,
        scopes: [TranscriptScope; 3],
    ) {
        let permit = encoders.try_acquire().unwrap();
        let outbox = Outbox::default();
        for (revision, scope) in (0..).zip(&scopes) {
            outbox
                .snapshot_with(
                    4,
                    PresentationSnapshot {
                        scope: scope.clone(),
                        revision,
                        ..PresentationSnapshot::default()
                    },
                    encoders,
                )
                .unwrap();
        }
        assert!(
            outbox.pop().unwrap().is_none(),
            "inline output must not bypass an earlier snapshot"
        );
        drop(permit);
        for (revision, scope) in (0..).zip(scopes) {
            let line = next_ordered(&outbox).await;
            let page: HostFrame<PlakExtension> = serde_json::from_slice(&line).unwrap();
            assert!(
                matches!(page, HostFrame::Snapshot(page) if page.scope == scope
                && page.revision == revision && page.last)
            );
        }
        assert!(outbox.pop().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_third_snapshot_in_one_flush_uses_the_ordered_inline_path() {
        static ENCODER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        assert_inline_snapshot_order(
            &ENCODER,
            [
                TranscriptScope::Root,
                TranscriptScope::Agent("current".into()),
                TranscriptScope::Root,
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_snapshot_for_a_retired_agent_does_not_block_a_new_scope() {
        static ENCODER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        // The first child was already queued before the client selected the
        // second one. Its ordered output does not make that switch overflow.
        assert_inline_snapshot_order(
            &ENCODER,
            [
                TranscriptScope::Agent("retired".into()),
                TranscriptScope::Root,
                TranscriptScope::Agent("selected".into()),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_small_snapshot_fits_after_thirty_mib_of_ordered_lines() {
        let mut outbox = Outbox::default();
        let mut bulk = vec![b'x'; 10 * 1024 * 1024];
        bulk.push(b'\n');
        for _ in 0..3 {
            outbox.write_all(&bulk).unwrap();
        }
        outbox.snapshot(4, PresentationSnapshot::default()).unwrap();
        // Wait for the encoder while all the earlier bytes still count.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                {
                    let queue = outbox.lock();
                    if queue.ordered.back().is_some_and(Ordered::ready) {
                        assert!(queue.bytes > bulk.len() * 3);
                        break;
                    }
                }
                outbox.0.ready.notified().await;
            }
        })
        .await
        .unwrap();
        for _ in 0..3 {
            assert_eq!(next_ordered(&outbox).await, bulk);
        }
        assert!(
            next_ordered(&outbox)
                .await
                .starts_with(br#"{"frame":"snapshot","#)
        );
    }
}
