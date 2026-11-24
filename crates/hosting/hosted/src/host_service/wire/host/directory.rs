use crate::host_service::{KeptActivity, KeptDirectoryAttention};

use super::{
    BTreeMap, ClientFrame, DeserializeOwned, HostFrame, ProtocolExtension, Serialize, SessionHost,
    SessionResidency, SlotView, watch,
};

/// Host-owned overlay of one session, independent of a wire version.
pub struct DirectorySlot {
    /// Where the current service is in its open or release cycle.
    pub residency: SessionResidency,
    /// Open failed and the host is not automatically retrying.
    pub open_failed: bool,
    /// Live workload and typed attention, only while resident.
    pub resident: Option<(KeptActivity, KeptDirectoryAttention)>,
}

impl<P: ProtocolExtension> SessionHost<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// Snapshot the current slots for the draft directory, without opening a session.
    #[must_use]
    pub fn directory_slots(&self) -> BTreeMap<String, DirectorySlot> {
        self.slots()
            .iter()
            .map(|slot| {
                use SessionResidency::{Dormant, Opening, Releasing, Resident};
                let (id, residency, resident) = match slot.peek() {
                    SlotView::Dormant(id) => (id, Dormant, None),
                    SlotView::Opening(id) => (id, Opening, None),
                    SlotView::Releasing(id) => (id, Releasing, None),
                    SlotView::Resident(service) => {
                        let (id, facts) = service.directory_resident();
                        (id, Resident, Some(facts))
                    }
                };
                (
                    id,
                    DirectorySlot {
                        residency,
                        resident,
                        open_failed: slot.open_failure().is_some() && !slot.startup_retrying(),
                    },
                )
            })
            .collect()
    }

    /// Notify a draft directory watch of changes to the host overlay.
    #[must_use]
    pub fn directory_slot_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}
