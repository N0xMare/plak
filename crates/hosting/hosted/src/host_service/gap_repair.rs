//! Bounded transcript repair with retry after an unanswered snapshot request.

use super::{HostService, ProtocolExtension, SessionIntent, State, TranscriptScope, wake_stats};
use std::time::{Duration, Instant};

const REPAIR_RETRY: Duration = Duration::from_secs(5);

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
                        (TranscriptScope::Root, Some((baseline, patches))) => {
                            for item in std::iter::once(baseline).chain(patches) {
                                held.offer(item.clone(), item.class(), self.capacity);
                            }
                        }
                        _ => {
                            if let Some(sender) = held.sender.clone() {
                                held.repairing.insert(scope.clone(), now + REPAIR_RETRY);
                                held.gapped.insert(scope.clone());
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
