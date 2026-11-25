//! Transcript requests use the existing attachment and sole transport writer.

use std::{io::Write, sync::Arc};

use plakous_host_protocol::views::{PresentationSnapshot, transcript::RowKey};
use plakous_host_protocol::wire::{
    AttachmentId, ClientFrame, HostFrame, RequestId, RequestRefusal,
};

use super::{
    CliResult, DeserializeOwned, HostConnection, Outcome, ProtocolExtension, Serialize,
    ViewportRevision, ViewportStart,
};
use crate::runtime::RootTranscript;

/// An input decoded by the product's frame contract.
#[allow(
    clippy::large_enum_variant,
    reason = "the dispatcher consumes only one frame at a time"
)]
pub enum ClientInput<P: ProtocolExtension> {
    /// Shared dispatcher traffic, with the optional viewport offered on Attach.
    Frame(ClientFrame<P>, Option<ViewportRevision>),
    /// Read-only operation scoped to the connection's current attachment.
    Transcript(RequestId, AttachmentId, TranscriptRead),
}

/// Transcript operations not interpreted by the common dispatcher.
pub enum TranscriptRead {
    /// Fetch the page before an opaque cursor.
    Older(String),
    /// Read a rendered body from a UTF-8 byte offset.
    Row(RowKey, u64),
    /// Request an immediate attachment status sample.
    Status,
}

/// Product history attached to a particular line and prune fence.
pub trait HistoryBinding: Send {
    /// The viewport token already given to the client for this binding.
    fn retained_token(&self) -> uuid::Uuid;

    /// Write a paged replacement for this exact root snapshot, if coherent.
    ///
    /// # Errors
    /// The replacement could not be written.
    fn repair(&self, out: &mut dyn Write, snapshot: &PresentationSnapshot) -> CliResult<bool>;

    /// Write one addressed history response, after hosted ownership checks.
    ///
    /// # Errors
    /// The response could not be written.
    fn read(
        &self,
        out: &mut dyn Write,
        id: RequestId,
        root: Option<&RootTranscript>,
        request: TranscriptRead,
    ) -> CliResult<()>;
}

/// Source of the viewport prefix prepared without the connection writer.
pub enum ViewportMode {
    /// Try this exact start before preparing a first page.
    Replay(ViewportRevision),
    /// The revision of the prepared first page.
    Page(u64),
}

/// Product-owned paged transcript awaiting hosted attachment admission.
pub trait PreparedViewport: Send {
    /// Exact product binding, including its prune fence.
    fn fence(&self) -> Arc<str>;
    /// The proposed start revision and whether it needs a baseline.
    fn mode(&self) -> ViewportMode;
    /// Write the answer and ordered prefix, binding later reads to it.
    ///
    /// # Errors
    /// The response could not be written.
    fn write(
        self: Box<Self>,
        out: &mut dyn Write,
        id: RequestId,
        start: ViewportStart,
    ) -> CliResult<Box<dyn HistoryBinding>>;
}

impl<P, W> HostConnection<P, W>
where
    P: ProtocolExtension + 'static,
    P::Intent: Send,
    P::Observation: Send,
    W: Write + Clone,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    pub(super) fn read_transcript(
        &mut self,
        id: RequestId,
        attachment: AttachmentId,
        request: TranscriptRead,
    ) -> CliResult<bool> {
        let refused = |reason| HostFrame::Response {
            id,
            outcome: Outcome::Refused(reason),
        };
        self.discard_blob_staging()?;
        if self.attach_in_flight(id) || self.host_reads.contains_key(&id) {
            self.write(&refused(RequestRefusal::Malformed {
                detail: "request id is already in flight".into(),
            }))?;
            return Ok(true);
        }
        if matches!(request, TranscriptRead::Status) {
            if let Some(reason) = self.reply_attachment_status(id, attachment)? {
                self.write(&refused(reason))?;
            }
            return Ok(true);
        }
        if let Some(link) = &self.node
            && link.connection.attachment() == Some(attachment)
            && link.connection.service.attached(attachment)
            && let Some(binding) = &link.connection.history
        {
            binding.read(
                &mut self.out,
                id,
                link.session.root_transcript.as_ref(),
                request,
            )?;
        } else if let Some(link) = &self.readonly
            && link.connection.attachment() == Some(attachment)
            && link.connection.service.attached(attachment)
            && let Some(binding) = &link.connection.history
        {
            binding.read(&mut self.out, id, None, request)?;
        } else {
            self.write(&refused(RequestRefusal::NotAttached))?;
        }
        Ok(true)
    }
}
