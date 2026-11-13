//! Construction and lifetime pinning of the host's bounded observation sender.

use super::{Sender, worker};
use std::sync::Arc;

impl<P: plakous_host_protocol::ProtocolExtension> Sender<P> {
    /// Wrap a bounded observation channel.
    #[must_use]
    pub fn over(inner: crate::surface_protocol::HostEventSender<P>) -> Self {
        Self {
            inner,
            notices: Arc::default(),
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
    /// A clone for a publisher that outlives the host's current step (a
    /// background recovery pass, a source finalization): the notices it keeps
    /// are appended only while the host still owns `session_id`.
    pub(crate) fn pinned_to(&self, session_id: &str) -> Self {
        Self {
            inner: self.inner.clone(),
            notices: Arc::clone(&self.notices),
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
