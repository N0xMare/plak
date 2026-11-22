//! Keep reads available without admitting new work during a host pause.

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
    pub(super) fn allowed_during_pause(&self, frame: &ClientFrame<P>) -> bool {
        match frame {
            ClientFrame::Request {
                op:
                    Op::Ping
                    | Op::ListSessions { .. }
                    | Op::WatchSessions { .. }
                    | Op::UnwatchSessions { .. }
                    | Op::Attach { .. }
                    | Op::Detach { .. },
                ..
            } => return true,
            ClientFrame::Request {
                op: Op::Host { request },
                ..
            } => {
                return self.host.sessions.host_request_during_pause(request);
            }
            _ => {}
        }
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
        let controls = self
            .host
            .upgrade_controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &*controls {
            super::admission::PausedControls::Refuse => false,
            super::admission::PausedControls::Allow => allowed,
            super::admission::PausedControls::Cancel(cancelled) => {
                if allowed {
                    cancelled.store(true, Ordering::Release);
                }
                allowed
            }
        }
    }
}
