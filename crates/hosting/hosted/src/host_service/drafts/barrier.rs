//! Track durable draft verdicts until their wire writes are attempted.

use super::DraftDelivery;
use crate::host_service::HostService;
use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::wire::AttachmentId;

impl<P: ProtocolExtension> HostService<P> {
    pub(in crate::host_service) fn take_draft_delivery(
        &self,
        attachment: AttachmentId,
        seq: u64,
    ) -> Option<DraftDelivery> {
        self.lock().draft_outputs.remove(&(attachment, seq))
    }

    pub(in crate::host_service) fn wire_drafts_lost(&self, attachment: AttachmentId) {
        self.lock()
            .draft_outputs
            .retain(|(id, _), _| *id != attachment);
    }
}

impl<P: ProtocolExtension> crate::host_service::State<P> {
    pub(in crate::host_service) fn track_draft_delivery(
        &mut self,
        attachment: AttachmentId,
        delivery: &mut Option<DraftDelivery>,
    ) {
        let Some(held) = self
            .attachments
            .get(&attachment)
            .filter(|held| !held.dropped)
        else {
            return;
        };
        if let Some(super::super::Pending::Event {
            seq,
            client_id: Some(_),
            ..
        }) = held.queue.back()
            && let Some(delivery) = delivery.take()
        {
            self.draft_outputs.insert((attachment, *seq), delivery);
        }
    }
}
