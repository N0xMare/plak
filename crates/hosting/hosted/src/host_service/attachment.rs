//! Attachment correlation tags and attach/replay classification.

use plakous_host_protocol::observation::Delivery;

use super::{
    Attachment, AttachmentId, Face, LOCAL_ATTACHMENT, ObservationClass, Outbound,
    ProtocolExtension, REMOTE_ATTACHMENT, SessionObservation, TranscriptScope, same_kind,
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

impl<P: ProtocolExtension> Attachment<P> {
    pub(super) fn miss_patch(&mut self, scope: &TranscriptScope) {
        self.gapped.insert(scope.clone());
        if !self.fresh.insert(scope.clone()) {
            return;
        }
        // A patch chain with a missing revision cannot be repaired by
        // replaying later deltas. Keep all other observations in order.
        let mut index = 0;
        let mut kept_snapshot = 0;
        self.queue.retain(|item| {
            let keep = !matches!(item, Outbound::Event {
                observation: SessionObservation::TranscriptPatch(patch), ..
            } if patch.scope == *scope);
            kept_snapshot += usize::from(keep && index < self.snapshot_held);
            index += 1;
            keep
        });
        self.snapshot_held = kept_snapshot;
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
        self.gapped.insert(scope);
    }

    /// Queues `observation`, making room by its class, or drops the
    /// attachment when what it may not lose finds none.
    pub(super) fn offer(
        &mut self,
        observation: SessionObservation<P>,
        class: ObservationClass,
        capacity: usize,
    ) {
        if self.dropped || self.waiting || !self.face.receives(&observation) {
            return;
        }
        let patch_scope = match &observation {
            SessionObservation::TranscriptPatch(patch) => Some(&patch.scope),
            _ => None,
        };
        if patch_scope.is_some_and(|scope| self.fresh.contains(scope)) {
            self.seq += 1;
            return;
        }
        let droppable = class == ObservationClass::Event(Delivery::Droppable);
        if class == ObservationClass::State
            && let Some(index) = self.queue.iter().position(|item| {
                matches!(item, Outbound::Event { class: ObservationClass::State, observation: held, .. }
                    if same_kind(held, &observation))
            })
        {
            // A queued state of its kind is superseded: the old one goes,
            // and the new one takes its turn after what came before it.
            self.queue.remove(index);
            if index < self.snapshot_held {
                self.snapshot_held -= 1;
            }
        } else if self.queue.len() - self.snapshot_held.min(self.queue.len()) >= capacity {
            let oldest_droppable = self.queue.iter().position(|item| {
                matches!(
                    item,
                    Outbound::Event {
                        class: ObservationClass::Event(Delivery::Droppable),
                        ..
                    }
                )
            });
            match (oldest_droppable, droppable) {
                (Some(index), _) => {
                    if let Some(Outbound::Event {
                        observation: lost, ..
                    }) = self.queue.remove(index)
                    {
                        self.note_loss(&lost);
                    }
                }
                (None, true) => {
                    // Dropped: the gap in the sequence tells the client.
                    self.note_loss(&observation);
                    self.seq += 1;
                    return;
                }
                (None, false) => {
                    if let Some(scope) = patch_scope
                        && self.sender.is_some()
                    {
                        self.miss_patch(scope);
                        self.seq += 1;
                        return;
                    }
                    self.dropped = true;
                    self.queue.clear();
                    self.queue.push_back(Outbound::ResyncRequired);
                    return;
                }
            }
        }
        self.seq += 1;
        if let SessionObservation::PresentationSnapshot(snapshot) = &observation {
            // The baseline includes any transcript evicted to make room for it.
            self.gapped.remove(&snapshot.scope);
            self.fresh.remove(&snapshot.scope);
            self.repairing.remove(&snapshot.scope);
        }
        self.queue.push_back(Outbound::Event {
            client_id: None,
            seq: self.seq,
            class,
            observation,
        });
    }
}
