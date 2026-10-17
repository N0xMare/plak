//! A shared bound on concurrent model requests.
//!
//! The host hands one [`RequestSlots`] to every agent whose requests should
//! share a budget. A request takes a slot before its operation deadline
//! starts and holds it until the response stream ends, so waiting for a slot
//! never counts against the model operation timeout. Waiters are served in
//! arrival order, and a cancelled wait leaves the queue without taking a slot.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A cloneable, FIFO-fair bound on concurrent model requests.
///
/// Clones share one budget. The label is opaque to the runtime: it is carried
/// in [`crate::AgentEventKind::ModelRequestQueued`] so a host can say what the
/// request is waiting for.
#[derive(Clone, Debug)]
pub struct RequestSlots {
    label: Arc<str>,
    limit: NonZeroUsize,
    semaphore: Arc<Semaphore>,
}

impl RequestSlots {
    /// A budget of `limit` concurrent requests, named `label`.
    #[must_use]
    pub fn new(label: impl Into<Arc<str>>, limit: NonZeroUsize) -> Self {
        Self {
            label: label.into(),
            limit,
            semaphore: Arc::new(Semaphore::new(limit.get())),
        }
    }

    /// The host-supplied name of this budget.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// How many requests may be in flight at once.
    #[must_use]
    pub const fn limit(&self) -> NonZeroUsize {
        self.limit
    }

    /// Requests holding a slot right now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.limit.get() - self.semaphore.available_permits()
    }

    /// A slot without waiting, unless the budget is full or others wait.
    pub(crate) fn try_take(&self) -> Option<RequestSlot> {
        Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .ok()
            .map(|permit| RequestSlot { _permit: permit })
    }

    /// Waits in arrival order for a slot. Dropping the future leaves the queue.
    pub(crate) async fn take(&self) -> RequestSlot {
        // The semaphore is never closed, so acquisition only waits.
        match Arc::clone(&self.semaphore).acquire_owned().await {
            Ok(permit) => RequestSlot { _permit: permit },
            Err(_) => unreachable!("request slots are never closed"),
        }
    }
}

/// One held slot; dropping it frees the slot for the next waiter.
#[derive(Debug)]
pub(crate) struct RequestSlot {
    _permit: OwnedSemaphorePermit,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waiters_are_served_in_arrival_order_and_a_cancelled_wait_takes_nothing() {
        let slots = RequestSlots::new("acct", NonZeroUsize::new(1).unwrap());
        let held = slots.try_take().unwrap();
        assert!(slots.try_take().is_none());
        assert_eq!(slots.in_flight(), 1);

        let first = tokio::spawn({
            let slots = slots.clone();
            async move {
                let _slot = slots.take().await;
                1
            }
        });
        tokio::task::yield_now().await;
        let abandoned = tokio::spawn({
            let slots = slots.clone();
            async move {
                let _slot = slots.take().await;
            }
        });
        tokio::task::yield_now().await;
        abandoned.abort();
        assert!(abandoned.await.unwrap_err().is_cancelled());

        drop(held);
        assert_eq!(first.await.unwrap(), 1);
        assert_eq!(slots.in_flight(), 0);
        assert!(slots.try_take().is_some());
    }
}
