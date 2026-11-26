//! Buffered transport output tracking for a host connection.

use super::HostConnection;
use crate::error::CliResult;
use plakous_host_protocol::{
    ProtocolExtension,
    wire::{ClientFrame, HostFrame},
};
use std::sync::Arc;

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

    /// Lets a buffered transport page and encode snapshots separately from
    /// request dispatch. The sink must retain the snapshot's place among
    /// ordered frames and complete delivery tracking only after transport
    /// sends, just as for lines passed to the connection's writer.
    #[must_use]
    pub fn with_snapshot_output(
        mut self,
        write: impl Fn(
            plakous_host_protocol::wire::AttachmentId,
            plakous_host_protocol::views::presentation::PresentationSnapshot,
        ) -> CliResult<()>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.snapshot_output = Some(Arc::new(write));
        self
    }

    /// Completes the buffered batch's transport send attempt. Do not call when
    /// merely taking bytes from a buffer or before the asynchronous send ends.
    pub fn output_sent(&mut self) {
        self.lifecycle_output_sent();
        for link in [&mut self.node, &mut self.draining].into_iter().flatten() {
            link.connection.output_sent();
        }
        if let Some(link) = &mut self.readonly {
            link.connection.output_sent();
        }
    }
}
