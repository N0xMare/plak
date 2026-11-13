//! The paused host's admission verdict barrier, before planned surrender.

use super::{AdmissionPause, HostConnection, SessionHost, SlotView};
use crate::error::{CliFailure, CliResult};
use plakous_host_protocol::{
    ProtocolExtension,
    wire::{ClientFrame, HostFrame},
};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

impl<P: ProtocolExtension, W: std::io::Write> HostConnection<P, W>
where
    HostFrame<P>: serde::Serialize,
    ClientFrame<P>: serde::de::DeserializeOwned,
{
    /// Marks a fresh connection's writer as a transport buffer, not the peer.
    /// Call before its first frame, and call [`Self::output_sent`] after each
    /// transport send attempt. Dropping/detaching loses delivery, not admission.
    #[must_use]
    pub fn buffered_output(mut self) -> Self {
        self.buffered_output = true;
        self
    }

    /// Completes the buffered batch's transport send attempt. Do not call when
    /// merely taking bytes from a buffer or before the asynchronous send ends.
    pub fn output_sent(&mut self) {
        for link in [&mut self.node, &mut self.draining].into_iter().flatten() {
            link.connection.output_sent();
        }
    }
}

impl<P: ProtocolExtension> SessionHost<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: serde::Serialize,
    ClientFrame<P>: serde::de::DeserializeOwned,
{
    /// Resolves reservations made before `pause`: safely withdraw unclaimed
    /// mailbox inputs with upgrading DraftRejected, then join already-claimed
    /// inputs' normal durable verdicts and their write attempts. A disconnected
    /// attachment loses delivery, not the durable admission fact.
    ///
    /// The caller closes ingress first and supplies one absolute host-wide
    /// deadline. Failure must abort preparation, never permit surrender.
    /// Neither draft identities nor this barrier are carried across generations.
    /// This is a blocking join: connection writer tasks must keep running.
    ///
    /// # Errors
    /// A foreign/inactive pause, or unresolved admission/delivery at the deadline.
    pub fn resolve_paused_drafts(
        &self,
        pause: &AdmissionPause,
        deadline: Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> CliResult<()> {
        if !Arc::ptr_eq(&pause.gate, &self.upgrade_admission)
            || !*self
                .upgrade_admission
                .read()
                .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(CliFailure::new(
                "draft verdict resolution requires this host's upgrade admission pause",
            ));
        }
        for slot in self.slots() {
            if let SlotView::Resident(session) = slot.peek() {
                session.reject_queued_wire_drafts();
            }
        }
        self.drafts
            .wait_resolved(deadline, cancelled)
            .map_err(CliFailure::new)
    }
}
