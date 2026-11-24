//! Attach repair requests and same-revision baseline publication.

use super::{
    IntentDelivery, IntentSendError, IntentSender, OBSERVATION_QUEUE_CAPACITY, ObservationSender,
    ProtocolExtension, QueuedIntent, SessionIntent, SessionObservation, TranscriptScope,
    TranscriptStreamDelta, VecDeque, append_delta,
};

const ROOT_ATTACHMENT_REPAIR: &str = "root-attachment-repair";

pub(super) fn keyed_append_scope<P: ProtocolExtension>(
    event: &SessionObservation<P>,
) -> Option<&TranscriptScope> {
    match event {
        SessionObservation::TranscriptStream {
            scope,
            delta: TranscriptStreamDelta::Keyed { .. },
        } => Some(scope),
        _ => None,
    }
}

pub(super) fn coalesce_keyed(
    pending: &mut TranscriptStreamDelta,
    delta: &TranscriptStreamDelta,
) -> bool {
    match (pending, delta) {
        (
            TranscriptStreamDelta::Keyed {
                row: pending_row,
                end: pending_end,
                text: pending_text,
                ..
            },
            TranscriptStreamDelta::Keyed {
                row,
                offset,
                end,
                text,
            },
        ) if pending_row == row && pending_end == offset => {
            if !append_delta(pending_text, text) {
                return false;
            }
            *pending_end = *end;
            true
        }
        _ => false,
    }
}

impl<P: ProtocolExtension> IntentDelivery<P> {
    /// This repair was requested by the attachment service, not the wire peer.
    #[must_use]
    pub fn is_root_attachment_repair(&self) -> bool {
        self.origin.as_deref() == Some(ROOT_ATTACHMENT_REPAIR)
            && matches!(
                self.action,
                SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Root
                }
            )
    }
}

impl<P: ProtocolExtension> IntentSender<P> {
    /// Requests the current root baseline for attachments that need it.
    ///
    /// # Errors
    /// As for `send`, unless the same repair is already queued.
    #[allow(clippy::result_large_err, reason = "the refusal preserves the action")]
    pub fn request_root_attachment_repair(&self) -> Result<(), IntentSendError<P>> {
        self.clone()
            .tagged(ROOT_ATTACHMENT_REPAIR)
            .send(SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Root,
            })
    }
}

pub(super) fn coalesces_resync<P: ProtocolExtension>(
    origin: Option<&str>,
    action: &SessionIntent<P>,
    correlation: Option<&str>,
    queue: &VecDeque<QueuedIntent<P>>,
) -> bool {
    let SessionIntent::ResyncTranscript { scope } = action else {
        return false;
    };
    correlation.is_none()
        && queue.iter().any(|queued| {
            queued.correlation.is_none()
                && matches!(&queued.action, SessionIntent::ResyncTranscript { scope: pending } if pending == scope)
                && (origin == Some(ROOT_ATTACHMENT_REPAIR))
                    == (queued.origin.as_deref() == Some(ROOT_ATTACHMENT_REPAIR))
        })
}

impl<P: ProtocolExtension> ObservationSender<P> {
    /// Queues a same-revision root repair behind the observations that
    /// produced that revision, without discarding them or clearing gaps.
    #[must_use]
    pub fn replay_root_snapshot(&self, snapshot: crate::views::PresentationSnapshot) -> bool {
        if snapshot.scope != TranscriptScope::Root {
            return false;
        }
        let Ok(mut queue) = self.0.queue.lock() else {
            return false;
        };
        if queue.iter().any(|event| {
            matches!(event, SessionObservation::PresentationSnapshot(old)
            if old.scope == snapshot.scope && old.revision == snapshot.revision)
        }) {
            return true;
        }
        if queue.len() >= OBSERVATION_QUEUE_CAPACITY {
            return false;
        }
        queue.push_back(SessionObservation::PresentationSnapshot(snapshot));
        self.0.ready.notify_one();
        let edge = self.0.claim_edge();
        drop(queue);
        if edge {
            self.0.fire();
        }
        true
    }

    pub(super) fn supersede_baseline(
        &self,
        snapshot: &crate::views::PresentationSnapshot,
        queue: &mut VecDeque<SessionObservation<P>>,
    ) {
        // A new baseline covers every older queued patch in this scope.
        queue.retain(|queued| match queued {
            SessionObservation::PresentationSnapshot(old) => old.scope != snapshot.scope,
            SessionObservation::TranscriptPatch(patch) => patch.scope != snapshot.scope,
            SessionObservation::TranscriptStream { scope, .. } => *scope != snapshot.scope,
            _ => true,
        });
        if matches!(snapshot.scope, TranscriptScope::Agent(_)) {
            // A face has one open agent view. Replacing it retires queued
            // content and gap bookkeeping for the old agent.
            queue.retain(|queued| {
                let scope = match queued {
                    SessionObservation::PresentationSnapshot(old) => &old.scope,
                    SessionObservation::TranscriptPatch(old) => &old.scope,
                    SessionObservation::TranscriptStream { scope, .. } => scope,
                    _ => return true,
                };
                *scope == TranscriptScope::Root || *scope == snapshot.scope
            });
            self.0
                .transcript_gaps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|scope| *scope == TranscriptScope::Root || *scope == snapshot.scope);
        }
    }
}
