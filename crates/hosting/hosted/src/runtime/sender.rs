//! Construction and lifetime pinning of the host's bounded observation sender.

use super::projection::transcript::{self, Presentations};
use super::{Sender, worker};
use plakous_host_protocol::views::transcript::{RowKey, TranscriptItem};
use std::sync::{Arc, Mutex, PoisonError};

/// A read-only view of the keyed root transcript, with no producer authority.
#[derive(Clone)]
pub struct RootTranscript(Arc<Mutex<Presentations>>);

/// The requested slice of one provisional row's rendered text.
#[derive(Debug, Eq, PartialEq)]
pub struct LiveRowSlice {
    /// Start offset in UTF-8 bytes.
    pub offset: u64,
    /// Exclusive end offset in UTF-8 bytes.
    pub end: u64,
    /// The requested text.
    pub text: String,
    /// The slice reaches the currently accumulated end, not a seal.
    pub last: bool,
}

/// Why a provisional row could not be read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveRowReadError {
    /// The key is not in the live root view or has already been replaced.
    RowGone,
    /// The requested offset is outside the body or splits a scalar.
    InvalidOffset,
}

impl RootTranscript {
    /// Whether this key belongs to the host's process-local row namespace.
    #[must_use]
    pub const fn is_provisional(key: RowKey) -> bool {
        transcript::provisional(key)
    }

    /// Byte length of a provisional text row in this baseline part.
    #[must_use]
    pub fn live_row_len(item: &TranscriptItem) -> Option<u64> {
        transcript::live_row_len(item)
    }

    /// Read one row for a holder of this root attachment. The product must
    /// authorize the attachment first. Read settled rows from history instead.
    ///
    /// # Errors
    /// The row is absent or the offset is not a scalar boundary in its body.
    pub fn read(&self, key: RowKey, offset: u64) -> Result<LiveRowSlice, LiveRowReadError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .root_row_slice(key, offset)
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> Sender<P> {
    /// Wrap a bounded observation channel.
    #[must_use]
    pub fn over(inner: crate::surface_protocol::HostEventSender<P>) -> Self {
        Self {
            inner,
            notices: Arc::default(),
            notice_ready: Arc::default(),
            presentations: Arc::default(),
            request_tools: Arc::default(),
            session_id: None,
            quit_receipt: worker::QuitReceipt::default(),
            step_control: Arc::default(),
            window: Arc::default(),
            answers: None,
            usage: Arc::default(),
            processes: Arc::default(),
            session_info: Arc::default(),
            #[cfg(test)]
            recorded: None,
        }
    }

    /// Wrap an observation channel for a product consuming keyed root
    /// appends. Do not use this on the v12 or JSONL serving path.
    #[must_use]
    pub fn over_keyed_root(
        inner: crate::surface_protocol::HostEventSender<P>,
    ) -> (Self, RootTranscript) {
        let mut sender = Self::over(inner);
        sender.presentations = Arc::new(Mutex::new(Presentations::keyed_root()));
        let root = RootTranscript(Arc::clone(&sender.presentations));
        (sender, root)
    }
    /// A clone for a publisher that outlives the host's current step (a
    /// background recovery pass, a source finalization): the notices it keeps
    /// are appended only while the host still owns `session_id`.
    pub(crate) fn pinned_to(&self, session_id: &str) -> Self {
        Self {
            inner: self.inner.clone(),
            notices: Arc::clone(&self.notices),
            notice_ready: Arc::clone(&self.notice_ready),
            presentations: Arc::clone(&self.presentations),
            request_tools: Arc::clone(&self.request_tools),
            session_id: Some(Arc::from(session_id)),
            quit_receipt: self.quit_receipt.clone(),
            step_control: Arc::clone(&self.step_control),
            window: Arc::clone(&self.window),
            answers: self.answers.clone(),
            usage: Arc::clone(&self.usage),
            processes: Arc::clone(&self.processes),
            session_info: Arc::clone(&self.session_info),
            #[cfg(test)]
            recorded: self.recorded.clone(),
        }
    }
}
