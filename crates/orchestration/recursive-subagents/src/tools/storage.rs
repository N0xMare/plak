//! The tree's one writer, and the capacity park it hands to its async caller.
//!
//! Writers run one at a time, in the order they asked. Each runs on a
//! blocking worker with the live tree mutex as its guard, so no async worker
//! performs a store write or holds the tree. A capacity-refused publication
//! detaches the live tree and asks the async caller to park: a sleep raced
//! against that write's cancellation and the tree's shutdown. Readers keep the
//! live tree throughout.

use crate::tree::ProductAgentTree;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio_util::sync::CancellationToken;

pub(crate) struct TreeMutationGuard<'a> {
    live: &'a Mutex<ProductAgentTree>,
    locked: Option<MutexGuard<'a, ProductAgentTree>>,
    candidate: Option<ProductAgentTree>,
}

impl<'a> TreeMutationGuard<'a> {
    pub(super) fn lock(live: &'a Mutex<ProductAgentTree>) -> Result<Self, ()> {
        let locked = live.lock().map_err(|_| ())?;
        Ok(Self {
            live,
            locked: Some(locked),
            candidate: None,
        })
    }

    pub(crate) fn detach(&mut self) {
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
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::swap(&mut *live, candidate);
            // A detached writer's wakes fired before its copy went live: a
            // waiter that woke and read the live tree in between saw the old
            // rows, so wake it again now that they are current.
            live.waiters().notify_waiters();
        }
    }
}

/// Every registered wait of this tree, queued or parked, by the agent that
/// owns it, and the tree's shutdown.
#[derive(Default)]
pub(super) struct StorageParking {
    stopped: CancellationToken,
    waits: Mutex<Vec<(String, CancellationToken)>>,
}

impl StorageParking {
    pub(super) fn shutdown(&self) {
        self.stopped.cancel();
    }

    pub(super) fn stopped(&self) -> &CancellationToken {
        &self.stopped
    }

    /// Registers one wait of `agent` until the returned handle drops.
    pub(super) fn begin(self: &Arc<Self>, agent: &str) -> StorageWait {
        let token = CancellationToken::new();
        self.lock().push((agent.to_owned(), token.clone()));
        StorageWait {
            parking: Arc::clone(self),
            token,
        }
    }

    /// Cancels every wait `agent` registered, queued or parked.
    pub(super) fn cancel(&self, agent: &str) {
        for (owner, token) in self.lock().iter() {
            if owner == agent {
                token.cancel();
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<(String, CancellationToken)>> {
        self.waits.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One registered wait; dropping it unregisters the wait.
pub(super) struct StorageWait {
    parking: Arc<StorageParking>,
    token: CancellationToken,
}

impl StorageWait {
    pub(super) fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for StorageWait {
    fn drop(&mut self) {
        let mut waits = self.parking.lock();
        if let Some(index) = waits.iter().position(|(_, token)| token == &self.token) {
            waits.swap_remove(index);
        }
    }
}

/// A running writer's request to park: the write's cancellation, and where
/// the caller answers whether to retry.
pub(super) struct ParkRequest {
    pub(super) token: CancellationToken,
    pub(super) resume: std::sync::mpsc::SyncSender<bool>,
}

type ParkSender = tokio::sync::mpsc::UnboundedSender<ParkRequest>;

/// The order writers run in, and how the running one reaches its caller.
#[derive(Default)]
pub(super) struct TreeWriter {
    /// Tokio's mutex is fair: writers take it in the order they asked.
    order: Arc<tokio::sync::Mutex<()>>,
    /// The running writer's park channel, with the blocking worker it runs
    /// on. Only a save on that worker parks through it.
    current: Mutex<Option<(std::thread::ThreadId, ParkSender)>>,
    /// A writer panicked: the tree is refused like a poisoned mutex.
    poisoned: AtomicBool,
}

impl TreeWriter {
    pub(super) fn order(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.order)
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub(super) fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    /// Marks the current blocking worker as the running writer until the
    /// returned scope drops.
    pub(super) fn enter(&self, parks: ParkSender) -> WriterScope<'_> {
        *self.slot() = Some((std::thread::current().id(), parks));
        WriterScope(self)
    }

    /// Hands a park to the running writer's caller and blocks this worker
    /// until it answers. `false` outside a writer, or once the caller is gone.
    pub(super) fn park(&self, token: &CancellationToken) -> bool {
        let parks = match &*self.slot() {
            Some((thread, parks)) if *thread == std::thread::current().id() => parks.clone(),
            _ => return false,
        };
        let (resume, resumed) = std::sync::mpsc::sync_channel(1);
        if parks
            .send(ParkRequest {
                token: token.clone(),
                resume,
            })
            .is_err()
        {
            return false;
        }
        resumed.recv().unwrap_or(false)
    }

    fn slot(&self) -> MutexGuard<'_, Option<(std::thread::ThreadId, ParkSender)>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) struct WriterScope<'a>(&'a TreeWriter);

impl Drop for WriterScope<'_> {
    fn drop(&mut self) {
        self.0.slot().take();
    }
}
