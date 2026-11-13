//! A pause admits no new work, but an existing-session stop can withdraw it.

use super::{ClientFrame, HostConnection, HostFrame, Op, PoisonError, ProtocolExtension};
use plakous_host_protocol::SessionIntent;
use std::sync::atomic::Ordering;

impl<P: ProtocolExtension, W: std::io::Write + Clone> HostConnection<P, W>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: serde::Serialize,
    ClientFrame<P>: serde::de::DeserializeOwned,
{
    // Called under the admission gate's read guard. Commit takes its write
    // guard, so a control cannot cross the surrender linearization point.
    pub(super) fn cancel_upgrade_for_control(&self, frame: &ClientFrame<P>) -> bool {
        if self.host.upgrade_committed.load(Ordering::Acquire) {
            return false;
        }
        let allowed = match frame {
            ClientFrame::Request {
                op: Op::StopSession { session },
                ..
            } => self.knows(session),
            ClientFrame::Request {
                op:
                    Op::Intent {
                        attachment,
                        intent: SessionIntent::Interrupt,
                        ..
                    },
                ..
            } => self
                .node
                .as_ref()
                .is_some_and(|node| node.connection.attachment() == Some(*attachment)),
            _ => false,
        };
        if allowed
            && let Some(cancelled) = self
                .host
                .upgrade_cancelled
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
        {
            cancelled.store(true, Ordering::Release);
        }
        allowed
    }
}
