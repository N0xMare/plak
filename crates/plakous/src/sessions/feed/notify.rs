//! Tail notification after the store acknowledged a mutation.
//!
//! The service's store is wrapped once, so every runtime, driver and workflow
//! write passes through it. A notification is sent only after the store
//! answered success, never inside a commit, and costs one map lookup; a
//! subscriber reads the tail itself after it wakes.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Poll, Waker};

use crate::sessions::RuntimeCheckpoint;
use plakous_agent::internal::{
    AcquireBranch, ArchiveReceipt, ArchiveSession, BranchLease, CheckpointId, CommitCheckpoint,
    CommitReceipt, CommittedCheckpoint, CreateReceipt, CreateSession, DeleteReceipt, DeleteSession,
    DurabilityReport, FeedPage, FeedSeq, FeedTail, ForkBranch, ForkReceipt, LeaseInstant,
    LoadedBranch, Page, PruneBranch, PruneReceipt, RecoverFork, SessionQuery, SessionRef,
    SessionStore, SessionStoreError, SessionSummary, StoreFuture, StoreUsage, StoredCheckpoint,
};
use plakous_model::SessionId;

#[derive(Default)]
struct Channel {
    state: Mutex<ChannelState>,
}

#[derive(Default)]
struct ChannelState {
    generation: u64,
    wakers: Vec<Waker>,
}

impl Channel {
    fn lock(&self) -> MutexGuard<'_, ChannelState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Most checkpoints [`FeedNotifier`] keeps per session: the newest ones the
/// live runtime committed, which the projector's next pass reads.
const PUBLISHED_CHECKPOINTS_PER_SESSION: usize = 8;

/// Per-session coalescing change signals, and the checkpoints the live
/// runtime committed most recently. Channels live while a subscription holds
/// them.
#[derive(Default)]
pub(in crate::sessions) struct FeedNotifier {
    channels: Mutex<HashMap<SessionId, Weak<Channel>>>,
    checkpoints: Mutex<HashMap<SessionId, VecDeque<Arc<RuntimeCheckpoint>>>>,
}

impl FeedNotifier {
    fn channels(&self) -> MutexGuard<'_, HashMap<SessionId, Weak<Channel>>> {
        self.channels.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn checkpoints(&self) -> MutexGuard<'_, HashMap<SessionId, VecDeque<Arc<RuntimeCheckpoint>>>> {
        self.checkpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps a checkpoint the store acknowledged, evicting the oldest past
    /// the bound.
    fn publish_checkpoint(&self, session: SessionId, checkpoint: Arc<RuntimeCheckpoint>) {
        let mut checkpoints = self.checkpoints();
        let kept = checkpoints.entry(session).or_default();
        if kept.len() == PUBLISHED_CHECKPOINTS_PER_SESSION {
            kept.pop_front();
        }
        kept.push_back(checkpoint);
    }

    /// A checkpoint of `session` the live runtime committed, while it is
    /// still kept: byte-identical to what the store would load.
    pub(in crate::sessions) fn published_checkpoint(
        &self,
        session: SessionId,
        checkpoint: CheckpointId,
    ) -> Option<Arc<RuntimeCheckpoint>> {
        self.checkpoints()
            .get(&session)?
            .iter()
            .find(|kept| kept.manifest().topology().checkpoint_id == checkpoint)
            .cloned()
    }

    fn forget_checkpoints(&self, session: SessionId) {
        self.checkpoints().remove(&session);
    }

    fn forget_before(&self, branch: SessionRef, horizon: u64, retained: &BTreeSet<CheckpointId>) {
        if let Some(kept) = self.checkpoints().get_mut(&branch.session_id()) {
            kept.retain(|checkpoint| {
                let topology = checkpoint.manifest().topology();
                topology.branch_id != branch.branch_id()
                    || topology.branch_revision >= horizon
                    || retained.contains(&topology.checkpoint_id)
            });
        }
    }

    pub(super) fn subscribe(&self, session: SessionId) -> FeedChanges {
        let mut channels = self.channels();
        let channel = channels
            .get(&session)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let channel = Arc::new(Channel::default());
                channels.insert(session, Arc::downgrade(&channel));
                channel
            });
        let seen = channel.lock().generation;
        FeedChanges { channel, seen }
    }

    fn notify(&self, session: SessionId) {
        let channel = {
            let mut channels = self.channels();
            let Some(channel) = channels.get(&session).map(Weak::upgrade) else {
                return;
            };
            if channel.is_none() {
                channels.remove(&session);
            }
            channel
        };
        let Some(channel) = channel else {
            return;
        };
        let wakers = {
            let mut state = channel.lock();
            state.generation = state.generation.wrapping_add(1);
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

/// One subscriber's view of a session's change signal.
pub(super) struct FeedChanges {
    channel: Arc<Channel>,
    seen: u64,
}

impl FeedChanges {
    /// Resolves once a mutation was acknowledged after the last resolution.
    pub(super) async fn changed(&mut self) {
        poll_fn(|context| {
            let mut state = self.channel.lock();
            if state.generation != self.seen {
                self.seen = state.generation;
                return Poll::Ready(());
            }
            if !state
                .wakers
                .iter()
                .any(|waker| waker.will_wake(context.waker()))
            {
                state.wakers.push(context.waker().clone());
            }
            Poll::Pending
        })
        .await;
    }
}

/// Forwards every store call and signals the session after each
/// acknowledged mutation.
pub(in crate::sessions) struct NotifyingStore {
    inner: Arc<dyn SessionStore>,
    notifier: Arc<FeedNotifier>,
}

impl NotifyingStore {
    pub(in crate::sessions) fn wrap(
        inner: Arc<dyn SessionStore>,
        notifier: Arc<FeedNotifier>,
    ) -> Arc<dyn SessionStore> {
        Arc::new(Self { inner, notifier })
    }

    fn after<T>(&self, result: &Result<T, SessionStoreError>, session: SessionId) {
        if result.is_ok() {
            self.notifier.notify(session);
        }
    }
}

impl SessionStore for NotifyingStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        self.inner.lease_now()
    }

    fn create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        let session = request.session().session_id();
        Box::pin(async move {
            let result = self.inner.create(request).await;
            self.after(&result, session);
            result
        })
    }

    fn recover_create(&self, request: CreateSession) -> StoreFuture<'_, CreateReceipt> {
        self.inner.recover_create(request)
    }

    fn load_branch(&self, session: SessionRef) -> StoreFuture<'_, LoadedBranch> {
        self.inner.load_branch(session)
    }

    fn load_checkpoint(&self, checkpoint: CheckpointId) -> StoreFuture<'_, StoredCheckpoint> {
        self.inner.load_checkpoint(checkpoint)
    }

    fn acquire(&self, request: AcquireBranch) -> StoreFuture<'_, BranchLease> {
        self.inner.acquire(request)
    }

    fn renew(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        self.inner.renew(lease)
    }

    fn revive(&self, lease: BranchLease) -> StoreFuture<'_, BranchLease> {
        self.inner.revive(lease)
    }

    fn release(&self, lease: BranchLease) -> StoreFuture<'_, ()> {
        let session = lease.session().session_id();
        Box::pin(async move {
            let result = self.inner.release(lease).await;
            if result.is_ok() {
                self.notifier.forget_checkpoints(session);
            }
            self.after(&result, session);
            result
        })
    }

    fn release_many(&self, leases: Vec<BranchLease>) -> StoreFuture<'_, ()> {
        let sessions: Vec<_> = leases
            .iter()
            .map(|lease| lease.session().session_id())
            .collect();
        Box::pin(async move {
            let result = self.inner.release_many(leases).await;
            for session in sessions {
                if result.is_ok() {
                    self.notifier.forget_checkpoints(session);
                }
                self.after(&result, session);
            }
            result
        })
    }

    fn commit(&self, request: CommitCheckpoint) -> StoreFuture<'_, CommitReceipt> {
        let session = request.expected().session().session_id();
        let bundle = request.checkpoint().clone();
        Box::pin(async move {
            let result = self.inner.commit(request).await;
            if let Ok(receipt) = &result
                && let Ok(stored) = StoredCheckpoint::from_store(receipt.checkpoint(), bundle)
            {
                self.notifier
                    .publish_checkpoint(session, Arc::clone(stored.checkpoint()));
            }
            self.after(&result, session);
            result
        })
    }

    fn recover_commit(&self, checkpoint: CheckpointId) -> StoreFuture<'_, CommittedCheckpoint> {
        self.inner.recover_commit(checkpoint)
    }

    fn fork(&self, request: ForkBranch) -> StoreFuture<'_, ForkReceipt> {
        let session = request.source().session().session_id();
        Box::pin(async move {
            let result = self.inner.fork(request).await;
            self.after(&result, session);
            result
        })
    }

    fn recover_fork(&self, request: RecoverFork) -> StoreFuture<'_, ForkReceipt> {
        self.inner.recover_fork(request)
    }

    fn list(&self, query: SessionQuery) -> StoreFuture<'_, Page<SessionSummary>> {
        self.inner.list(query)
    }

    fn reclaim(
        &self,
        mode: plakous_agent::internal::StoreReclaimMode,
    ) -> StoreFuture<'_, plakous_agent::internal::StoreReclaimReport> {
        self.inner.reclaim(mode)
    }

    fn usage(&self) -> StoreFuture<'_, StoreUsage> {
        self.inner.usage()
    }

    fn archive(&self, request: ArchiveSession) -> StoreFuture<'_, ArchiveReceipt> {
        let session = request.session_id();
        Box::pin(async move {
            let result = self.inner.archive(request).await;
            self.after(&result, session);
            result
        })
    }

    fn delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        let session = request.session_id();
        Box::pin(async move {
            self.notifier.forget_checkpoints(session);
            let result = self.inner.delete(request).await;
            self.after(&result, session);
            result
        })
    }

    fn prune(&self, request: PruneBranch) -> StoreFuture<'_, PruneReceipt> {
        let head = request.expected();
        let branch = head.session();
        let session = branch.session_id();
        let retained = request.retain().clone();
        Box::pin(async move {
            // Prevent an in-flight prune from serving removed ancestors from
            // memory. The prospective horizon still keeps the head and parent.
            self.notifier
                .forget_before(branch, head.revision().get().saturating_sub(1), &retained);
            let result = self.inner.prune(request).await;
            if let Ok(receipt) = &result {
                self.notifier
                    .forget_before(branch, receipt.horizon().get(), &retained);
            }
            self.after(&result, session);
            result
        })
    }

    fn recover_delete(&self, request: DeleteSession) -> StoreFuture<'_, DeleteReceipt> {
        self.inner.recover_delete(request)
    }

    fn read_feed(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> StoreFuture<'_, FeedPage> {
        self.inner.read_feed(session_id, after, max_records)
    }

    fn feed_tail(&self, session_id: SessionId) -> StoreFuture<'_, FeedTail> {
        self.inner.feed_tail(session_id)
    }

    fn flush(&self) -> StoreFuture<'_, DurabilityReport> {
        self.inner.flush()
    }

    fn shutdown(&self) -> StoreFuture<'_, DurabilityReport> {
        self.notifier.checkpoints().clear();
        self.inner.shutdown()
    }
}
