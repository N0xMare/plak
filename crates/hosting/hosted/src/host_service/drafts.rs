//! Process-lifetime durable-admission registry; mailbox acceptance is not a verdict.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

use super::{ANSWER, Outbound, attachment_of};
use plakous_host_protocol::wire::{AttachmentId, ClientDraftId, NodeAddress};
use plakous_host_protocol::{ProtocolExtension, SessionObservation};

mod barrier;

/// Maximum pending plus durably admitted wire submissions in one host generation.
pub(crate) const MAX_DRAFT_IDS: usize = 100_000;

/// Why reserving a wire submission did not create a new admission opportunity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DraftReservationError {
    /// The same submission is still waiting for durable admission.
    Pending,
    /// The id was used with different session, node, kind or exact content.
    Conflict,
    /// New ids cannot enter; existing admitted duplicates still work.
    Full,
}

#[derive(Clone, Copy)]
struct Entry {
    digest: [u8; 32],
    admitted: bool,
}

struct Registry {
    entries: BTreeMap<ClientDraftId, Entry>,
    limit: usize,
    deliveries: usize,
    unresolved_drop: bool,
}

struct SharedRegistry {
    state: Mutex<Registry>,
    changed: Condvar,
}

/// One host's registry, shared by sessions and independent of their attachments.
#[derive(Clone)]
pub(crate) struct DraftRegistry(Arc<SharedRegistry>);

impl Default for DraftRegistry {
    fn default() -> Self {
        Self(Arc::new(SharedRegistry {
            state: Mutex::new(Registry {
                entries: BTreeMap::new(),
                limit: MAX_DRAFT_IDS,
                deliveries: 0,
                unresolved_drop: false,
            }),
            changed: Condvar::new(),
        }))
    }
}

/// A reservation or an already durable verdict; neither means mailbox acceptance.
pub(crate) enum DraftReservation {
    /// The caller must keep this through durable admission or definite refusal/drop.
    Pending(PendingDraft),
    /// An identical submission was durably admitted earlier in this host generation.
    Duplicate,
}

impl DraftRegistry {
    /// Atomically reserve `id` against the digest of the complete addressed submission.
    ///
    /// The caller hashes session, target node, intent kind and exact content/images;
    /// mail includes its target, body and urgency. No TTL, queue consumption or
    /// disconnect evicts admitted entries. A fresh registry is a new host generation.
    ///
    /// # Errors
    /// A conflicting id, an identical pending submission, or exhausted capacity.
    pub(crate) fn reserve(
        &self,
        id: ClientDraftId,
        digest: [u8; 32],
    ) -> Result<DraftReservation, DraftReservationError> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = state.entries.get(&id) {
            if entry.digest != digest {
                return Err(DraftReservationError::Conflict);
            }
            return if entry.admitted {
                Ok(DraftReservation::Duplicate)
            } else {
                Err(DraftReservationError::Pending)
            };
        }
        if state.entries.len() >= state.limit {
            return Err(DraftReservationError::Full);
        }
        state.entries.insert(
            id,
            Entry {
                digest,
                admitted: false,
            },
        );
        Ok(DraftReservation::Pending(PendingDraft {
            registry: self.clone(),
            id,
        }))
    }

    pub(crate) fn wait_resolved(
        &self,
        deadline: Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(), &'static str> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(
                    "host_upgrade_cancelled: requester left before admission verdicts settled",
                );
            }
            if state.unresolved_drop {
                return Err(
                    "host_upgrade_draft_verdict_unknown: a service ended without resolving a reserved input",
                );
            }
            if state.deliveries == 0 && state.entries.values().all(|entry| entry.admitted) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(
                    "host_upgrade_draft_verdicts_pending: admission or verdict delivery did not settle before the deadline",
                );
            }
            state = self
                .0
                .changed
                .wait_timeout(
                    state,
                    (deadline - now).min(std::time::Duration::from_millis(100)),
                )
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A durable verdict still owed a write attempt, or lost with its attachment.
pub(super) struct DraftDelivery(DraftRegistry);

impl Drop for DraftDelivery {
    fn drop(&mut self) {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.deliveries -= 1;
        self.0.0.changed.notify_all();
    }
}

/// Process-local pending admission owned by the service, not the submitting connection.
/// Dropping it before durable admission frees the id, including mailbox drop/refusal.
pub(crate) struct PendingDraft {
    registry: DraftRegistry,
    id: ClientDraftId,
}

impl PendingDraft {
    /// Record durable admission before delivering its addressed observation.
    /// Mail calls this only after its translated input/mail is durably admitted.
    #[cfg(test)]
    pub(crate) fn admit(self) {
        let mut state = self
            .registry
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = state.entries.get_mut(&self.id) {
            entry.admitted = true;
        }
        self.registry.0.changed.notify_all();
    }

    fn verdict(self, admitted: bool) -> DraftDelivery {
        let mut state = self
            .registry
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.deliveries += 1;
        if admitted {
            if let Some(entry) = state.entries.get_mut(&self.id) {
                entry.admitted = true;
            }
        } else {
            state.entries.remove(&self.id);
        }
        drop(state);
        DraftDelivery(self.registry.clone())
    }
}

impl Drop for PendingDraft {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state
            .entries
            .get(&self.id)
            .is_some_and(|entry| !entry.admitted)
        {
            state.entries.remove(&self.id);
        }
        self.registry.0.changed.notify_all();
    }
}

impl<P: ProtocolExtension> super::HostService<P> {
    pub(super) fn begin_wire_draft(
        &self,
        id: ClientDraftId,
        pending: PendingDraft,
        attachment: AttachmentId,
        intent: plakous_host_protocol::SessionIntent<P>,
    ) {
        let mut state = self.lock();
        state.pending_drafts.insert(id, pending);
        state.draft_inputs.insert(id, (attachment, intent));
    }

    pub(super) fn refuse_wire_draft(&self, id: ClientDraftId) {
        let mut state = self.lock();
        state.draft_inputs.remove(&id);
        state.pending_drafts.remove(&id);
    }

    pub(super) fn clear_wire_drafts(&self) {
        let mut state = self.lock();
        for pending in state.pending_drafts.values() {
            pending
                .registry
                .0
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unresolved_drop = true;
            pending.registry.0.changed.notify_all();
        }
        state.pending_drafts.clear();
        state.draft_inputs.clear();
        state.draft_window = None;
        state.draft_answer = false;
    }

    pub(super) fn wire_target(&self, attachment: AttachmentId) -> NodeAddress {
        let state = self.lock();
        NodeAddress {
            tree_id: self.root.tree_id.clone(),
            agent_id: state
                .attachments
                .get(&attachment)
                .and_then(|held| held.node.clone())
                .unwrap_or_else(|| self.root.agent_id.clone()),
        }
    }
}

impl<P: ProtocolExtension> super::State<P> {
    pub(super) fn observe_draft(
        &mut self,
        observation: &SessionObservation<P>,
    ) -> Option<ClientDraftId> {
        match observation {
            SessionObservation::IntentStarted(started) => {
                // Sender::answering nests an answer window and then reopens
                // the interrupted one. Neither boundary is an admission verdict.
                self.draft_answer = started
                    .as_deref()
                    .is_some_and(|value| value.starts_with(ANSWER));
                self.draft_window = started
                    .as_deref()
                    .map(|value| value.strip_prefix(ANSWER).unwrap_or(value))
                    .and_then(attachment_of)
                    .and_then(|(_, client)| client)
                    .and_then(|client| client.parse().ok());
                None
            }
            SessionObservation::DraftAccepted(_) => {
                let id = self.draft_window?;
                self.draft_inputs.remove(&id);
                if let Some(pending) = self.pending_drafts.remove(&id) {
                    self.draft_delivery = Some(pending.verdict(true));
                }
                Some(id)
            }
            SessionObservation::DraftRejected(_) => {
                let id = self.draft_window?;
                self.draft_inputs.remove(&id);
                self.draft_delivery = self
                    .pending_drafts
                    .remove(&id)
                    .map(|pending| pending.verdict(false));
                Some(id)
            }
            SessionObservation::DraftRestored(_) => self.draft_window,
            SessionObservation::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice),
            ) if self.draft_answer
                && notice.tone == plakous_host_protocol::views::transcript::NoticeTone::Error =>
            {
                if let Some(id) = self.draft_window {
                    self.draft_inputs.remove(&id);
                    self.draft_delivery = self
                        .pending_drafts
                        .remove(&id)
                        .map(|pending| pending.verdict(false));
                }
                self.draft_window
            }
            SessionObservation::TopologyNavigatorUnavailable { .. } => {
                if let Some(id) = self.draft_window {
                    self.draft_inputs.remove(&id);
                    self.draft_delivery = self
                        .pending_drafts
                        .remove(&id)
                        .map(|pending| pending.verdict(false));
                }
                self.draft_window
            }
            _ => None,
        }
    }
}

impl<P: ProtocolExtension> super::Attachment<P> {
    pub(super) fn correlate_draft(&mut self, origin: Option<ClientDraftId>) {
        if let Some(id) = origin
            && let Some(Outbound::Event { client_id, .. }) = self.queue.back_mut()
        {
            *client_id = Some(id);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    fn id(number: u32) -> ClientDraftId {
        format!("00000000-0000-4000-8000-{number:012}")
            .parse()
            .unwrap()
    }

    fn pending(registry: &DraftRegistry, number: u32, digest: u8) -> PendingDraft {
        match registry.reserve(id(number), [digest; 32]).unwrap() {
            DraftReservation::Pending(pending) => pending,
            DraftReservation::Duplicate => panic!("unexpected duplicate"),
        }
    }

    #[test]
    fn mailbox_ack_is_pending_until_durable_admission_even_on_another_device() {
        let host = DraftRegistry::default();
        let service = host.clone();
        let admission = pending(&service, 1, 1);
        assert!(matches!(
            host.reserve(id(1), [1; 32]),
            Err(DraftReservationError::Pending)
        ));
        // Losing the submitting connection drops its handle, not the service's reservation.
        drop(host);
        let second_device = service.clone();
        assert!(matches!(
            second_device.reserve(id(1), [1; 32]),
            Err(DraftReservationError::Pending)
        ));
        admission.admit();
        drop(service);
        assert!(matches!(
            second_device.reserve(id(1), [1; 32]),
            Ok(DraftReservation::Duplicate)
        ));
        assert!(matches!(
            second_device.reserve(id(1), [2; 32]),
            Err(DraftReservationError::Conflict)
        ));
    }

    #[test]
    fn refusal_and_mailbox_drop_free_the_id_without_remembering_a_verdict() {
        let host = DraftRegistry::default();
        let refused = pending(&host, 1, 1);
        assert!(matches!(
            host.reserve(id(1), [2; 32]),
            Err(DraftReservationError::Conflict)
        ));
        drop(refused);
        let retry = pending(&host, 1, 2);
        drop(retry);
        pending(&host, 1, 3).admit();
    }

    #[test]
    fn full_registry_never_evicts_durable_duplicates_and_drop_releases_capacity() {
        let host = DraftRegistry(Arc::new(SharedRegistry {
            state: Mutex::new(Registry {
                entries: BTreeMap::new(),
                limit: 2,
                deliveries: 0,
                unresolved_drop: false,
            }),
            changed: Condvar::new(),
        }));
        pending(&host, 1, 1).admit();
        let second = pending(&host, 2, 2);
        assert!(matches!(
            host.reserve(id(3), [3; 32]),
            Err(DraftReservationError::Full)
        ));
        assert!(matches!(
            host.reserve(id(1), [1; 32]),
            Ok(DraftReservation::Duplicate)
        ));
        assert!(matches!(
            host.reserve(id(2), [2; 32]),
            Err(DraftReservationError::Pending)
        ));
        drop(second);
        pending(&host, 3, 3).admit();
        assert!(matches!(
            host.reserve(id(1), [1; 32]),
            Ok(DraftReservation::Duplicate)
        ));
    }

    #[test]
    fn restart_has_no_verdict_and_simultaneous_devices_reserve_once() {
        let host = DraftRegistry::default();
        let contenders = (0..8)
            .map(|_| {
                let host = host.clone();
                std::thread::spawn(move || match host.reserve(id(1), [1; 32]) {
                    Ok(DraftReservation::Pending(pending)) => {
                        pending.admit();
                        true
                    }
                    Ok(DraftReservation::Duplicate) | Err(DraftReservationError::Pending) => false,
                    Err(reason) => panic!("unexpected refusal: {reason:?}"),
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            contenders
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        drop(host);
        let restarted = DraftRegistry::default();
        drop(pending(&restarted, 1, 1));
    }
}
