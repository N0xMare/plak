//! Bounded transcript repair with retry after an unanswered snapshot request.

use super::{
    Attachment, AttachmentId, HostService, IntentSendError, IntentSender, ProtocolExtension, Queue,
    SessionIntent, SessionObservation, State, TranscriptScope, wake_stats,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const REPAIR_RETRY: Duration = Duration::from_secs(5);

impl<P: ProtocolExtension> Attachment<P> {
    pub(super) fn await_baseline(&mut self, scope: &TranscriptScope, now: Instant) {
        self.miss_patch(scope);
        self.defer_repair(scope.clone(), now);
    }

    fn defer_repair(&mut self, scope: TranscriptScope, now: Instant) {
        self.repairing.insert(scope.clone(), now + REPAIR_RETRY);
        self.gapped.insert(scope);
    }
}

fn reserve(
    waiting: &mut BTreeMap<TranscriptScope, Instant>,
    scope: &TranscriptScope,
    now: Instant,
) -> Option<Instant> {
    if waiting.get(scope).is_some_and(|until| now < *until) {
        return None;
    }
    let deadline = now + REPAIR_RETRY;
    waiting.insert(scope.clone(), deadline);
    Some(deadline)
}

impl<P: ProtocolExtension> State<P> {
    #[allow(
        clippy::result_large_err,
        reason = "intent refusals hand back the unsent action"
    )]
    pub(super) fn request_attached_root_repair(
        &mut self,
        attachment: AttachmentId,
        sender: &IntentSender<P>,
    ) -> Result<(), IntentSendError<P>> {
        let scope = TranscriptScope::Root;
        let now = Instant::now();
        if let Some(deadline) = self.reserve_repair(&scope, now)
            && let Err(error) = sender.request_root_attachment_repair()
        {
            self.cancel_repair(&scope, deadline);
            return Err(error);
        }
        if let Some(held) = self.attachments.get_mut(&attachment) {
            held.await_baseline(&scope, now);
        }
        Ok(())
    }

    pub(super) fn reserve_repair(
        &mut self,
        scope: &TranscriptScope,
        now: Instant,
    ) -> Option<Instant> {
        reserve(&mut self.resync_wait, scope, now)
    }

    pub(super) fn cancel_repair(&mut self, scope: &TranscriptScope, deadline: Instant) {
        if self.resync_wait.get(scope) == Some(&deadline) {
            self.resync_wait.remove(scope);
        }
    }

    // A same-revision root snapshot fills a hole for a waiting attachment;
    // an advancing snapshot changes the base of every holder's next patch.
    pub(super) fn observe_revision(&mut self, observation: &SessionObservation<P>) -> bool {
        match observation {
            SessionObservation::SessionOpening | SessionObservation::ClearView => {
                self.root_revision = None;
                self.resync_wait.clear();
            }
            SessionObservation::TranscriptPatch(patch) if patch.scope == TranscriptScope::Root => {
                self.root_revision = Some(patch.to_revision);
            }
            SessionObservation::PresentationSnapshot(snapshot) => {
                self.resync_wait.remove(&snapshot.scope);
                if snapshot.scope == TranscriptScope::Root {
                    return self.root_revision.replace(snapshot.revision)
                        == Some(snapshot.revision)
                        && snapshot.revision != 0;
                }
            }
            _ => {}
        }
        false
    }
}

impl<P: ProtocolExtension> HostService<P> {
    pub(super) fn repair_gaps(&self) {
        self.repair_gaps_at(Instant::now());
    }

    pub(super) fn repair_gaps_at(&self, now: Instant) {
        let mut asks = Vec::new();
        {
            let mut state = self.lock();
            let State {
                attachments,
                snapshot,
                resync_wait,
                ..
            } = &mut *state;
            for held in attachments.values_mut() {
                let capacity = held.capacity(self.capacity);
                let limit = held.byte_limit();
                if held.dropped || !held.queue.fits(0, capacity, limit) {
                    continue;
                }
                for scope in std::mem::take(&mut held.gapped) {
                    if held.queued_baseline(&scope) {
                        continue;
                    }
                    if held.repairing.get(&scope).is_some_and(|until| now < *until) {
                        held.gapped.insert(scope);
                        continue;
                    }
                    if scope == TranscriptScope::Root
                        && !held.fresh.contains(&scope)
                        && let Some(baseline) = &snapshot.baseline
                    {
                        if held.retained_after.is_some_and(|until| now < until) {
                            held.gapped.insert(scope);
                            continue;
                        }
                        let bytes = baseline
                            .observations::<P>()
                            .map(|(_, size)| Queue::<P>::charge(size))
                            .sum();
                        if held
                            .queue
                            .can_append(baseline.patch_count() + 1, bytes, capacity, limit)
                        {
                            held.retained_after = Some(now + REPAIR_RETRY);
                            for (item, size) in baseline.observations() {
                                let class = item.class();
                                held.offer(item, class, capacity, size);
                            }
                            continue;
                        }
                    }
                    if let Some(sender) = held.sender.clone() {
                        held.miss_patch(&scope);
                        if reserve(resync_wait, &scope, now).is_some() {
                            held.defer_repair(scope.clone(), now);
                            asks.push((sender, scope));
                        }
                    }
                }
            }
        }
        for (sender, scope) in asks {
            let requested = if scope == TranscriptScope::Root {
                sender.request_root_attachment_repair()
            } else {
                sender.send(SessionIntent::ResyncTranscript {
                    scope: scope.clone(),
                })
            };
            if requested.is_ok() {
                wake_stats::record(&scope);
            }
        }
    }
}
