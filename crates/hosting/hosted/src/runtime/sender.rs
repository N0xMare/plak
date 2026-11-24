//! Construction and lifetime pinning of the host's bounded observation sender.

use super::projection::transcript::{self, Presentations};
use super::{Sender, worker};
use plakous::sessions::feed::FeedSeq;
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::views::transcript::{RowKey, TranscriptItem};
use std::sync::{Arc, Mutex, PoisonError};

/// A read-only view of the keyed root transcript, with no producer authority.
#[derive(Clone)]
pub struct RootTranscript(Arc<Mutex<Presentations>>);

/// One resident transcript revision and its uncommitted contribution to a page.
pub struct RootHistoryCapture {
    /// The feed sequence used to assemble the last canonical publication.
    pub applied: FeedSeq,
    /// Current presentation revision at the capture fence.
    pub revision: u64,
    /// Provisional rows and live overrides of durable rows.
    pub items: Vec<TranscriptItem>,
}

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
    /// Capture from the same producer fence as keyed appends and canonical
    /// replacement. No capture is available until this session publishes a
    /// known canonical prefix.
    #[must_use]
    pub fn capture(&self, session: &str) -> Option<RootHistoryCapture> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .capture_root(session)
    }

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
    pub(crate) fn publish_root_projection(
        &self,
        session: &str,
        applied: FeedSeq,
        snapshot: PresentationSnapshot,
    ) -> bool {
        self.root_projection(session, Some(applied), snapshot, false)
    }

    pub(crate) fn replace_root_session(&self, session: &crate::sessions::SessionRecord) -> bool {
        self.root_projection(
            &session.id,
            session.transcript.applied(),
            super::logging::project_session_snapshot(session),
            true,
        )
    }

    fn root_projection(
        &self,
        session: &str,
        applied: Option<FeedSeq>,
        snapshot: PresentationSnapshot,
        replace: bool,
    ) -> bool {
        if snapshot.scope != TranscriptScope::Root {
            return false;
        }
        let mut presentations = self
            .presentations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let published = if replace {
            presentations.snapshot(snapshot, &self.inner)
        } else {
            presentations.publish(snapshot, &self.inner)
        };
        presentations.mark_root_publication(session, applied.filter(|_| published));
        published
    }

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
