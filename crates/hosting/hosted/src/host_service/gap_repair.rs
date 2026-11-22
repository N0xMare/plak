//! Bounded transcript repair with retry after an unanswered snapshot request.

use super::{
    Attachment, HostService, ProtocolExtension, SessionIntent, State, TranscriptScope, wake_stats,
};
use std::time::{Duration, Instant};

const REPAIR_RETRY: Duration = Duration::from_secs(5);

impl<P: ProtocolExtension> Attachment<P> {
    pub(super) fn await_root_baseline(&mut self) {
        let scope = TranscriptScope::Root;
        self.fresh.insert(scope.clone());
        self.defer_repair(scope, Instant::now());
    }

    fn defer_repair(&mut self, scope: TranscriptScope, now: Instant) {
        self.repairing.insert(scope.clone(), now + REPAIR_RETRY);
        self.gapped.insert(scope);
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
                ..
            } = &mut *state;
            for (id, held) in attachments.iter_mut() {
                if held.dropped
                    || held.queue.len().saturating_sub(held.snapshot_held)
                        >= held.capacity(self.capacity)
                {
                    continue;
                }
                for scope in std::mem::take(&mut held.gapped) {
                    if held.repairing.get(&scope).is_some_and(|until| now < *until) {
                        held.gapped.insert(scope);
                        continue;
                    }
                    match (&scope, &snapshot.baseline) {
                        (TranscriptScope::Root, Some(baseline))
                            if !held.fresh.contains(&scope)
                                && baseline.patch_count()
                                    < held.capacity(self.capacity).saturating_sub(
                                        held.queue.len().saturating_sub(held.snapshot_held),
                                    ) =>
                        {
                            for item in baseline.observations() {
                                let class = item.class();
                                held.offer(item, class, self.capacity);
                            }
                        }
                        _ => {
                            if let Some(sender) = held.sender.clone() {
                                held.defer_repair(scope.clone(), now);
                                asks.push((*id, sender, scope));
                            }
                        }
                    }
                }
            }
        }
        for (id, sender, scope) in asks {
            if sender
                .send(SessionIntent::ResyncTranscript {
                    scope: scope.clone(),
                })
                .is_ok()
            {
                wake_stats::record(&scope);
            } else if let Some(held) = self.lock().attachments.get_mut(&id) {
                held.repairing.remove(&scope);
                held.gapped.insert(scope);
            }
        }
    }
}
