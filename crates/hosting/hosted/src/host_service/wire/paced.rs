//! Bounded attachment batches; snapshot and patch work keeps its own turn.

use std::io::{self, Write};

use super::super::attachment::{SentBatch, SentTranscript};
use super::{
    AttachmentId, AttachmentStatusOutput, CliResult, ClientFrame, DeserializeOwned, HostFrame,
    Instant, ProtocolExtension, RequestId, RequestRefusal, Serialize, WireConnection,
};

const BATCH_BYTES: usize = 256 * 1024;
const BATCH_ITEMS: usize = 32;
const MAX_DEFERRED_STATUS: usize = 4;

#[derive(Default)]
pub(super) struct StatusWriter {
    last_written: Option<(AttachmentId, u64)>,
    batch: Option<(AttachmentId, SentBatch)>,
    prefix_in_flight: Option<AttachmentId>,
    pub(super) deferred: Vec<(AttachmentId, RequestId)>,
}

pub(super) struct Counted<W> {
    pub(super) inner: W,
    written: usize,
}

impl<W> Counted<W> {
    pub(super) const fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<P: ProtocolExtension, W: Write> WireConnection<P, W>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// Installs the product's attachment status projection.
    #[must_use]
    pub fn with_attachment_status(mut self, output: AttachmentStatusOutput) -> Self {
        self.status_output = Some(output);
        if let Some(id) = self.attachment {
            self.enable_status(id);
        }
        self
    }

    pub(super) fn enable_status(&self, id: AttachmentId) {
        if self.status_output.is_some() {
            let seed = self
                .status
                .last_written
                .filter(|(owner, _)| *owner == id)
                .map_or(0, |(_, seq)| seq);
            self.service
                .enable_attachment_status(id, Instant::now(), seed);
        }
    }

    fn status_prefix_blocked(&self, id: AttachmentId) -> bool {
        self.viewport_prefix_pending() || self.status.prefix_in_flight == Some(id)
    }

    pub(super) fn status_deadline(&self) -> Option<Instant> {
        self.status_output?;
        let attachment = self.attachment?;
        if self.status_prefix_blocked(attachment) {
            return None;
        }
        if self
            .status
            .deferred
            .iter()
            .any(|(held, _)| *held == attachment)
        {
            return Some(Instant::now());
        }
        self.service.status_deadline(attachment)
    }

    /// Write an addressed status report regardless of the periodic deadline.
    ///
    /// # Errors
    /// Bounded control output could not take the frame.
    pub fn reply_attachment_status(
        &mut self,
        id: RequestId,
        attachment: AttachmentId,
    ) -> CliResult<Option<RequestRefusal>> {
        let Some(output) = self
            .status_output
            .filter(|_| self.attachment == Some(attachment) && self.service.attached(attachment))
        else {
            return Ok(Some(RequestRefusal::NotAttached));
        };
        self.status.deferred.retain(|(held, _)| *held == attachment);
        if self
            .status
            .deferred
            .iter()
            .any(|(_, request)| *request == id)
        {
            return Ok(Some(RequestRefusal::OneInFlight));
        }
        if self.status_prefix_blocked(attachment) {
            if self.status.deferred.len() >= MAX_DEFERRED_STATUS {
                return Ok(Some(RequestRefusal::Backpressure));
            }
            self.status.deferred.push((attachment, id));
        } else {
            self.commit_status_if_unbuffered();
            let Some(status) = self
                .service
                .attachment_status(attachment, Instant::now(), true)
            else {
                return Ok(Some(RequestRefusal::NotAttached));
            };
            output(&mut self.stdout, attachment, Some(id), status)?;
        }
        Ok(None)
    }

    pub(super) fn flush_attachment_status(&mut self) -> CliResult<()> {
        self.commit_status_if_unbuffered();
        if let (Some(output), Some(attachment)) = (self.status_output, self.attachment)
            && !self.status_prefix_blocked(attachment)
        {
            let now = Instant::now();
            for (held, request) in self.status.deferred.drain(..) {
                if held == attachment
                    && let Some(status) = self.service.attachment_status(attachment, now, true)
                {
                    output(&mut self.stdout, attachment, Some(request), status)?;
                }
            }
            if let Some(status) = self.service.attachment_status(attachment, now, false) {
                output(&mut self.stdout, attachment, None, status)?;
            }
        }
        Ok(())
    }

    /// Record a complete logical baseline, never individual parts. Buffered
    /// writers commit this mark only once ordered output has drained.
    pub fn record_baseline_sent(&mut self, id: AttachmentId, seq: Option<u64>) {
        self.record_sent(id, SentTranscript::Baseline(seq));
    }

    /// Record a successfully written sequenced event, including replay patches.
    pub fn record_event_sent(&mut self, id: AttachmentId, seq: u64) {
        self.record_sent(id, SentTranscript::Sequence(seq));
    }

    fn record_sent(&mut self, id: AttachmentId, sent: SentTranscript) {
        if let Some(seq) = sent.sequence() {
            self.status.last_written = Some((id, seq));
        }
        if self.status_output.is_some() {
            let batch = &mut self.status.batch;
            if batch.as_ref().is_none_or(|(owner, _)| *owner != id) {
                *batch = Some((id, SentBatch::default()));
            }
            if let Some((_, batch)) = batch {
                batch.record(sent);
            }
        }
    }

    pub(super) fn status_prefix_queued(&mut self, id: AttachmentId) {
        if self.buffered_drafts.is_some() {
            self.status.prefix_in_flight = Some(id);
        } else {
            self.commit_status_batch();
        }
    }

    pub(super) fn status_output_sent(&mut self) {
        self.status.prefix_in_flight = None;
        self.commit_status_batch();
    }

    fn commit_status_if_unbuffered(&mut self) {
        if self.buffered_drafts.is_none() {
            self.commit_status_batch();
        }
    }

    fn commit_status_batch(&mut self) {
        if let Some((id, sent)) = self.status.batch.take() {
            self.service.transcript_sent(id, sent);
        }
    }

    pub(super) fn flush_paced(&mut self, bulk: bool) -> CliResult<()> {
        self.flush_answers()?;
        let Some(attachment) = self.attachment else {
            return Ok(());
        };
        if bulk {
            self.service.repair_gaps();
        }
        self.stdout.written = 0;
        for index in 0..BATCH_ITEMS {
            let items = self.service.take_for_wire(attachment, bulk, index != 0);
            let Some(item) = items.first() else {
                return Ok(());
            };
            let alone = item.paced_alone();
            self.write_observations(attachment, items)?;
            if !bulk
                || alone
                || self.attachment != Some(attachment)
                || self.stdout.written >= BATCH_BYTES
            {
                break;
            }
        }
        Ok(())
    }
}
