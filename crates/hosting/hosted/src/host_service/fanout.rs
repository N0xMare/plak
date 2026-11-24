//! Size each publication once, then route it at the attachment boundary.

use super::{
    ANSWER, AttachmentId, HostService, Mailbox, ObservationClass, Payload, ProtocolExtension,
    SessionObservation, attachment_of, baseline_scope,
};

#[derive(Clone)]
pub(super) struct Publication<P: ProtocolExtension> {
    pub(super) observation: SessionObservation<P>,
    pub(super) size: usize,
}

impl<P: ProtocolExtension> Publication<P> {
    pub(super) fn prepare(observation: SessionObservation<P>) -> Self {
        let size = Payload::measure(&observation);
        Self { observation, size }
    }
}

impl<P: ProtocolExtension> HostService<P> {
    /// Takes one observation from the session: keeps what an attach needs
    /// and queues it for the attachments its class names.
    pub fn observe(&self, observation: SessionObservation<P>) {
        let size = self
            .actions
            .as_ref()
            .map_or(0, |_| Payload::measure(&observation));
        self.publish(Publication { observation, size });
    }

    pub(super) fn publish(&self, Publication { observation, size }: Publication<P>) {
        {
            let mut state = self.lock();
            let repair_root = state.observe_revision(&observation)
                && matches!(self.actions, Some(Mailbox::Sender(_)));
            let client_id = state.observe_draft(&observation);
            let mut draft_delivery = state.draft_delivery.take();
            let class = observation.class();
            // Inside a child's answer window everything but a transcript is
            // that child's answer, and none of it is the root's to keep.
            let child_answer = state.requester.filter(|owner| {
                !matches!(observation, SessionObservation::IntentStarted(_))
                    && baseline_scope(&observation).is_none()
                    && state
                        .attachments
                        .get(owner)
                        .is_some_and(|held| held.node.is_some())
            });
            let mut observation = observation;
            let targets: Vec<AttachmentId> = match (class, baseline_scope(&observation)) {
                (_, Some(scope)) => state
                    .attachments
                    .iter()
                    .filter(|(_, held)| {
                        held.scopes.contains(scope) && (!repair_root || held.fresh.contains(scope))
                    })
                    .map(|(id, _)| *id)
                    .collect(),
                _ if child_answer.is_some() => child_answer.into_iter().collect(),
                (ObservationClass::Addressed, None) => {
                    if let SessionObservation::IntentStarted(started) = &observation {
                        let (answer, named) = match started.as_deref() {
                            Some(started) => match started.strip_prefix(ANSWER) {
                                Some(named) => (true, Some(named.to_owned())),
                                None => (false, Some(started.to_owned())),
                            },
                            None => (false, None),
                        };
                        // A faces' mailbox may carry another sender's
                        // correlation that only looks like an attachment's:
                        // one naming no attachment the service holds is
                        // passed through as no attachment's.
                        let owner = named.as_deref().and_then(attachment_of).filter(|(id, _)| {
                            matches!(self.actions, Some(Mailbox::Sender(_)))
                                || state.attachments.contains_key(id)
                        });
                        if let Some((owner, client)) = owner {
                            let owner = state.reply_recipient(owner).unwrap_or(owner);
                            // The attachment sees its own correlation.
                            observation = SessionObservation::IntentStarted(client);
                            let child = state
                                .attachments
                                .get(&owner)
                                .is_some_and(|held| held.node.is_some());
                            // A child attachment's window holds only its own
                            // control's answers, which come marked; the rest
                            // of an unmarked window is the session's.
                            state.requester = (answer || !child).then_some(owner);
                            vec![owner]
                        } else {
                            state.requester = None;
                            observation = SessionObservation::IntentStarted(named);
                            state.root_attachments()
                        }
                    } else {
                        // An answer to no attachment's intent belongs to
                        // none: every root attachment gets it.
                        match state.requester {
                            Some(owner) => vec![owner],
                            None => state.root_attachments(),
                        }
                    }
                }
                _ => state.root_attachments(),
            };
            let observation = Payload::from(observation);
            // A face's service takes no later attachment to keep anything for.
            if self.actions.is_some() && child_answer.is_none() {
                state.snapshot.remember(&observation, class, size);
                if let Payload::Other(event) = &observation {
                    let state = &mut *state;
                    state.directory_attention.observe(&state.snapshot, event);
                }
            }
            for id in targets {
                if let Some(held) = state.attachments.get_mut(&id) {
                    let capacity = held.capacity(self.capacity);
                    held.offer(observation.clone(), class, capacity, size);
                    held.correlate_draft(client_id);
                }
                state.track_draft_delivery(id, &mut draft_delivery);
            }
        }
        self.repair_gaps();
    }
}
