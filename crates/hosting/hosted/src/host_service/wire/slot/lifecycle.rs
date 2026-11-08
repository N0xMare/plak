//! Release the actor and retain the same slot fence through a product mutation.

use plakous_host_protocol::wire::{LifecycleReason, RequestRefusal};

use super::{
    Arc, BUSY_NONE, BUSY_RELEASING, Duration, Instant, Ordering, ProtocolExtension, SessionSlot,
    Slot, WireSession, join,
};

impl<P: ProtocolExtension> SessionSlot<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
{
    pub(in crate::host_service::wire) fn mutate_released<T: Send>(
        &self,
        expected: &str,
        change: impl FnOnce() -> Result<T, RequestRefusal> + Send,
    ) -> Result<T, RequestRefusal> {
        let mut slot = self.lock();
        let source = match &*slot {
            Slot::Resident { session, .. } => session.session_id(),
            Slot::Dormant { session_id } => session_id.clone(),
        };
        if source != expected {
            return Err(RequestRefusal::SessionLifecycleRefused {
                session: expected.to_owned(),
                reason: LifecycleReason::NotFound,
            });
        }
        if let Slot::Resident { session, .. } = &*slot {
            let old = Arc::clone(session);
            if !old.ended() {
                if let Err(failure) = self.release_actor(&old, expected) {
                    old.end_reopen(None);
                    return Err(failure);
                }
            }
            self.busy.store(BUSY_RELEASING, Ordering::Release);
            let Slot::Resident { worker, .. } =
                std::mem::replace(&mut *slot, Slot::Dormant { session_id: source })
            else {
                unreachable!("the slot lock retains its worker");
            };
            let result = join(worker, self.release_join);
            old.end_reopen(None);
            self.busy.store(BUSY_NONE, Ordering::Release);
            result.map_err(|failure| RequestRefusal::SessionUnavailable {
                detail: failure.message().to_owned(),
            })?;
        }
        // The actor and its lease are gone; no local trigger can reopen until
        // this product operation has acquired its cross-process branch fences.
        std::thread::scope(|scope| scope.spawn(change).join()).unwrap_or_else(|_| {
            Err(RequestRefusal::SessionUnavailable {
                detail: "the lifecycle operation panicked".to_owned(),
            })
        })
    }

    fn release_actor(&self, old: &WireSession<P>, expected: &str) -> Result<(), RequestRefusal> {
        let busy = || RequestRefusal::SessionLifecycleRefused {
            session: expected.to_owned(),
            reason: old.lifecycle_busy(),
        };
        let ticket = old.begin_reopen(expected).map_err(|_| busy())?;
        let until = Instant::now() + self.release_join;
        loop {
            if let Some(result) = old.reopen_settled(ticket) {
                return result.map_err(|_| busy());
            }
            if Instant::now() >= until {
                return old
                    .cancel_reopen(ticket)
                    .unwrap_or_else(|| Err("release acknowledgement timed out".to_owned()))
                    .map_err(|_| busy());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
