//! Withdraw only unclaimed inputs; join durable verdicts and their wire writes.

use super::DraftDelivery;
use crate::host_service::HostService;
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_host_protocol::wire::{AttachmentId, ClientDraftId};
use plakous_host_protocol::{ProtocolExtension, SessionObservation};
use std::collections::BTreeSet;

impl<P: ProtocolExtension> HostService<P> {
    pub(in crate::host_service) fn reject_queued_wire_drafts(&self) {
        let Some(sender) = self.mailbox() else { return };
        let mut state = self.lock();
        let ids: BTreeSet<_> = state
            .pending_drafts
            .keys()
            .map(ToString::to_string)
            .collect();
        for (correlation, _delivery) in sender.withdraw_queued_correlations(&ids) {
            if let Ok(id) = correlation.parse() {
                state.reject_upgrading_draft(id, self.capacity);
            }
        }
    }

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
        if let Some(super::super::Outbound::Event {
            seq,
            client_id: Some(_),
            ..
        }) = held.queue.back()
            && let Some(delivery) = delivery.take()
        {
            self.draft_outputs.insert((attachment, *seq), delivery);
        }
    }

    fn reject_upgrading_draft(&mut self, id: ClientDraftId, capacity: usize) {
        let Some(pending) = self.pending_drafts.remove(&id) else {
            return;
        };
        let mut delivery = Some(pending.verdict(false));
        let Some((attachment, intent)) = self.draft_inputs.remove(&id) else {
            return;
        };
        let Some(held) = self
            .attachments
            .get_mut(&attachment)
            .filter(|held| !held.dropped)
        else {
            return;
        };
        let capacity = held.capacity(capacity);
        for observation in [
            SessionObservation::Row(TranscriptRow::error(
                NoticeScope::Session,
                "the host is upgrading; this draft was not admitted",
            )),
            SessionObservation::DraftRejected(intent),
        ] {
            let class = observation.class();
            held.offer(observation, class, capacity);
            held.correlate_draft(Some(id));
        }
        self.track_draft_delivery(attachment, &mut delivery);
    }
}
