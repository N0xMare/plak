//! Attachment correlation tags and attach/replay classification.

use plakous_host_protocol::observation::Delivery;
use plakous_host_protocol::views::transcript::TranscriptStreamDelta;

use super::queue::MAX_QUEUE_BYTES;
use super::{
    Attachment, AttachmentId, Face, LOCAL_ATTACHMENT, ObservationClass, Payload, Pending,
    ProtocolExtension, Queue, REMOTE_ATTACHMENT, SessionObservation, TranscriptScope, same_kind,
};

/// The origin tag an attachment's intents carry, so the session's
/// `IntentStarted` names the attachment its answers belong to.
pub(super) fn origin(attachment: AttachmentId, face: Face) -> String {
    let prefix = if face == Face::Remote {
        REMOTE_ATTACHMENT
    } else {
        LOCAL_ATTACHMENT
    };
    format!("{prefix}{attachment}")
}

/// The attachment a correlation names, and the client's own correlation
/// inside it.
pub(super) fn attachment_of(correlation: &str) -> Option<(AttachmentId, Option<String>)> {
    let rest = correlation
        .strip_prefix(REMOTE_ATTACHMENT)
        .or_else(|| correlation.strip_prefix(LOCAL_ATTACHMENT))?;
    let (id, client) = match rest.split_once(':') {
        Some((id, client)) => (id, Some(client.to_owned())),
        None => (rest, None),
    };
    Some((id.parse().ok()?, client))
}

/// Whether an attach re-emits `observation` from what the service keeps
/// (states, the root baseline, approvals, the credential wait), so a
/// replay must not send it again.
pub(super) fn kept_for_attach<P: ProtocolExtension>(observation: &SessionObservation<P>) -> bool {
    observation.class() == ObservationClass::State
        || keyed_append_scope(observation).is_some()
        || matches!(
            observation,
            SessionObservation::ApprovalRequired { .. }
                | SessionObservation::ApprovalResolved { .. }
                | SessionObservation::CredentialWait { .. }
                | SessionObservation::ClearView
                | SessionObservation::SessionOpening
                | SessionObservation::SessionOpened { .. }
        )
        || matches!(
            observation,
            SessionObservation::PresentationSnapshot(snapshot)
                if snapshot.scope == TranscriptScope::Root
        )
        || matches!(
            observation,
            SessionObservation::TranscriptPatch(patch) if patch.scope == TranscriptScope::Root
        )
}

pub(super) const fn keyed_append_scope<P: ProtocolExtension>(
    observation: &SessionObservation<P>,
) -> Option<&TranscriptScope> {
    match observation {
        SessionObservation::TranscriptStream {
            scope,
            delta: TranscriptStreamDelta::Keyed { .. },
        } => Some(scope),
        _ => None,
    }
}

impl<P: ProtocolExtension> Attachment<P> {
    pub(super) fn miss_patch(&mut self, scope: &TranscriptScope) {
        self.gapped.insert(scope.clone());
        if self.fresh.insert(scope.clone()) {
            self.discard_transcript(scope);
        }
    }

    pub(super) fn discard_transcript(&mut self, scope: &TranscriptScope) {
        self.queue.retain(|item| queued_scope(item) != Some(scope));
    }

    pub(super) fn queued_baseline(&self, scope: &TranscriptScope) -> bool {
        self.queue.iter().any(|item| {
            matches!(item, Pending::Event {
            observation: Payload::Snapshot(snapshot), ..
        } if snapshot.scope == *scope)
        })
    }

    /// How many items this attachment's queue holds: a face's is unbounded,
    /// since the face reads it on its own thread and the session's mailbox
    /// is the bound it meets.
    pub(super) const fn capacity(&self, service: usize) -> usize {
        if self.sender.is_some() {
            service
        } else {
            usize::MAX
        }
    }

    pub(super) const fn byte_limit(&self) -> usize {
        if self.sender.is_some() {
            MAX_QUEUE_BYTES
        } else {
            usize::MAX
        }
    }

    /// Records the transcript a dropped observation belonged to, so the
    /// attachment's view of it is repaid with a fresh baseline.
    fn note_loss(&mut self, lost: &SessionObservation<P>) {
        let scope = match lost {
            SessionObservation::TranscriptStream { scope, .. } => scope.clone(),
            SessionObservation::AssistantDelta(_)
            | SessionObservation::AssistantDone
            | SessionObservation::ReasoningDelta { .. }
            | SessionObservation::ReasoningSealed { .. }
            | SessionObservation::UserMessage { .. }
            | SessionObservation::Row(_) => TranscriptScope::Root,
            _ => return,
        };
        if !self.keyed.contains(&scope) {
            self.gapped.insert(scope);
        }
    }

    /// Queues `observation`, making room by its class, or drops the
    /// attachment when what it may not lose finds none.
    pub(super) fn offer(
        &mut self,
        observation: Payload<P>,
        class: ObservationClass,
        capacity: usize,
        encoded: usize,
    ) {
        if self.dropped || self.waiting || !observation.received_by(self.face) {
            return;
        }
        if self.fence_root_preview && observation.is_root_preview() {
            // An attaching viewer cannot use a suffix before the first baseline
            // that will cover it. Do not number observations it was never owed.
            return;
        }
        if let Payload::Other(event) = &observation
            && let Some(scope) = keyed_append_scope(event)
            && (!self.keyed.contains(scope) || self.fresh.contains(scope))
        {
            // A delta cannot open a row or bridge a missing revision.
            return;
        }
        let scope = payload_scope(&observation);
        if matches!(&observation, Payload::Patch(patch) if self.fresh.contains(&patch.scope)) {
            self.seq += 1;
            return;
        }
        if let Payload::Snapshot(snapshot) = &observation {
            // The snapshot covers everything queued for its scope. Its
            // replacement takes its own turn after surviving observations.
            self.discard_transcript(&snapshot.scope);
        }
        let droppable = class == ObservationClass::Event(Delivery::Droppable);
        let replacement = if class == ObservationClass::State
            && let Payload::Other(new) = &observation
        {
            self.queue.iter().position(|item| {
                matches!(item, Pending::Event { class: ObservationClass::State, observation: Payload::Other(held), .. }
                    if same_kind(held, new))
            })
        } else {
            None
        };
        if let Some(index) = replacement {
            // A queued state of its kind is superseded: the old one goes,
            // and the new one takes its turn after what came before it.
            self.queue.remove(index);
        }
        self.seq += 1;
        let cost = Queue::<P>::charge(encoded);
        if !self.make_room(cost, capacity, !droppable && scope.is_none()) {
            if let Some(scope) = scope {
                self.miss_patch(scope);
            } else if droppable {
                if let Payload::Other(lost) = &observation {
                    self.note_loss(lost);
                }
            } else {
                self.dropped = true;
                self.queue.clear();
                self.queue.push_boundary(Pending::ResyncRequired);
            }
            return;
        }
        if let Payload::Snapshot(snapshot) = &observation {
            // The baseline includes any transcript evicted to make room for it.
            self.gapped.remove(&snapshot.scope);
            self.fresh.remove(&snapshot.scope);
            self.repairing.remove(&snapshot.scope);
            self.keyed.insert(snapshot.scope.clone());
            if snapshot.scope == TranscriptScope::Root {
                self.fence_root_preview = false;
            }
        }
        if matches!(
            observation,
            Payload::Other(SessionObservation::ClearView | SessionObservation::SessionOpening)
        ) {
            self.keyed.clear();
        }
        self.queue.push(
            Pending::Event {
                client_id: None,
                seq: self.seq,
                class,
                observation,
            },
            cost,
        );
    }

    fn make_room(&mut self, cost: usize, capacity: usize, may_discard_transcript: bool) -> bool {
        let limit = self.byte_limit();
        if cost > limit {
            return false;
        }
        while !self.queue.fits(cost, capacity, limit) {
            let oldest = self.queue.iter().position(|item| {
                matches!(
                    item,
                    Pending::Event {
                        class: ObservationClass::Event(Delivery::Droppable),
                        ..
                    }
                )
            });
            if let Some(index) = oldest {
                if let Some(Pending::Event {
                    observation: Payload::Other(lost),
                    ..
                }) = self.queue.remove(index)
                {
                    self.note_loss(&lost);
                }
            } else if let Some(scope) = may_discard_transcript
                .then(|| self.queue.iter().find_map(queued_scope).cloned())
                .flatten()
            {
                // Keep counted answers and ordered boundaries even if
                // their admission requires dropping a transcript replay.
                self.discard_transcript(&scope);
                self.miss_patch(&scope);
            } else {
                return false;
            }
        }
        true
    }
}

fn payload_scope<P: ProtocolExtension>(payload: &Payload<P>) -> Option<&TranscriptScope> {
    match payload {
        Payload::Snapshot(snapshot) => Some(&snapshot.scope),
        Payload::Patch(patch) => Some(&patch.scope),
        Payload::Other(_) => None,
    }
}

fn queued_scope<P: ProtocolExtension>(item: &Pending<P>) -> Option<&TranscriptScope> {
    match item {
        Pending::Event {
            observation: Payload::Other(event),
            ..
        } => keyed_append_scope(event),
        Pending::Event { observation, .. } => payload_scope(observation),
        _ => None,
    }
}
