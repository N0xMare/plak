//! Retained mutation ownership without holding the live tree mutex while parked.

use crate::tree::ProductAgentTree;
use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

pub(crate) struct TreeMutationGuard<'a> {
    live: &'a Mutex<ProductAgentTree>,
    locked: Option<MutexGuard<'a, ProductAgentTree>>,
    candidate: Option<ProductAgentTree>,
    _owner: MutexGuard<'a, ()>,
}

impl<'a> TreeMutationGuard<'a> {
    pub(super) fn lock(
        live: &'a Mutex<ProductAgentTree>,
        owner: &'a Mutex<()>,
    ) -> Result<Self, ()> {
        let owner = blocking(|| owner.lock()).map_err(|_| ())?;
        let locked = blocking(|| live.lock()).map_err(|_| ())?;
        Ok(Self {
            live,
            locked: Some(locked),
            candidate: None,
            _owner: owner,
        })
    }

    pub(super) fn detach(&mut self) {
        if let Some(locked) = self.locked.take() {
            self.candidate = Some(locked.clone());
        }
    }
}

impl Deref for TreeMutationGuard<'_> {
    type Target = ProductAgentTree;
    fn deref(&self) -> &Self::Target {
        match (&self.candidate, &self.locked) {
            (Some(tree), _) => tree,
            (_, Some(tree)) => tree,
            _ => unreachable!("a mutation owns a live or detached tree"),
        }
    }
}
impl DerefMut for TreeMutationGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match (&mut self.candidate, &mut self.locked) {
            (Some(tree), _) => tree,
            (_, Some(tree)) => tree,
            _ => unreachable!("a mutation owns a live or detached tree"),
        }
    }
}
impl Drop for TreeMutationGuard<'_> {
    fn drop(&mut self) {
        if let Some(candidate) = &mut self.candidate {
            std::mem::swap(
                &mut *self
                    .live
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                candidate,
            );
        }
    }
}

#[derive(Default)]
pub(super) struct StorageParking {
    stopped: std::sync::atomic::AtomicBool,
    active: Mutex<Option<(String, std::sync::Arc<std::sync::atomic::AtomicBool>)>>,
    wake: Condvar,
}

impl StorageParking {
    pub(super) fn shutdown(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_all();
    }

    pub(super) fn begin(&self, agent: &str) -> StorageWait<'_> {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = Some((agent.to_owned(), cancelled.clone()));
        StorageWait {
            parking: self,
            cancelled,
        }
    }

    pub(super) fn cancel(&self, agent: &str) {
        if let Some((owner, cancelled)) = &*self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            && owner == agent
        {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            self.wake.notify_all();
        }
    }
}

pub(super) struct StorageWait<'a> {
    parking: &'a StorageParking,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for StorageWait<'_> {
    fn drop(&mut self) {
        self.parking
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

impl StorageWait<'_> {
    pub(super) fn wait(&self, interval: Duration, shutdown: impl Fn() -> bool) -> bool {
        blocking(|| {
            let next = Instant::now() + interval;
            let mut active = self
                .parking
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if self.cancelled.load(std::sync::atomic::Ordering::Acquire)
                    || self
                        .parking
                        .stopped
                        .load(std::sync::atomic::Ordering::Acquire)
                    || shutdown()
                {
                    return false;
                }
                let remaining = next.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return true;
                }
                active = self
                    .parking
                    .wake
                    .wait_timeout(active, remaining.min(Duration::from_millis(100)))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        })
    }
}
