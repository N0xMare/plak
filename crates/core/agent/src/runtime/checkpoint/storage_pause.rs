//! One owner's cooperative storage wait; no publication or retry authority.

use plakous_model::CancellationToken;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

pub(crate) type StorageInterrupt = Arc<dyn Fn() -> bool + Send + Sync>;
pub(crate) type StorageInterruptSlot = Arc<std::sync::Mutex<Option<StorageInterrupt>>>;

pub(crate) fn interrupted(slot: &StorageInterruptSlot) -> bool {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|probe| probe())
}

#[derive(Clone, Default)]
pub(crate) struct StoragePause {
    waiting: Arc<AtomicUsize>,
    cancel: CancellationToken,
    shutdown: CancellationToken,
    interrupt: StorageInterruptSlot,
}

tokio::task_local! {
    static CURRENT: StoragePause;
}

impl StoragePause {
    pub(crate) fn new(shutdown: CancellationToken) -> Self {
        Self {
            shutdown,
            ..Self::default()
        }
    }

    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT.scope(self.clone(), future).await
    }

    pub(crate) fn cancel(&self) {
        // Remember an enqueued cancel even if the in-flight write has not
        // returned its capacity refusal yet. Otherwise that race could park
        // the owner ahead of the very command that should interrupt it.
        self.cancel.cancel();
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire) != 0
    }

    pub(super) fn bind_interrupt(&self, probe: StorageInterrupt) {
        *self
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(probe);
    }

    pub(super) fn current() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_default()
    }

    pub(super) fn enter(&self) -> StorageWaiting {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        StorageWaiting(self.waiting.clone())
    }

    pub(super) async fn retry(&self, externally_cancelled: &(impl Fn() -> bool + Sync)) -> bool {
        let retry_at = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if externally_cancelled() || interrupted(&self.interrupt) {
                return false;
            }
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return false,
                () = self.shutdown.cancelled() => return false,
                () = tokio::time::sleep_until(retry_at) => return true,
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }
}

pub(super) struct StorageWaiting(Arc<AtomicUsize>);

impl Drop for StorageWaiting {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
