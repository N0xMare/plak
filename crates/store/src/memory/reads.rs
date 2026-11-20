use super::{
    BTreeSet, BranchRecord, CheckpointId, CheckpointObjectBundle, CheckpointObjectRef, FeedPage,
    FeedSeq, FeedTail, LoadedBranch, MemorySessionStore, NonZeroUsize, Page, SessionId,
    SessionQuery, SessionRef, SessionStoreError, SessionSummary, StoreCapacityBound, StoreState,
    StoredCheckpoint, capacity, count, require_open,
};
use crate::memory_feed;

pub(super) fn stored_checkpoint(
    state: &StoreState,
    reference: CheckpointObjectRef,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let record = state
        .checkpoints
        .get(&reference.checkpoint_id())
        .ok_or(SessionStoreError::corrupt())?;
    if record.reference != reference {
        return Err(SessionStoreError::corrupt());
    }
    if let Some(bundle) = state.hot_bundles.get(&reference.checkpoint_id()) {
        return StoredCheckpoint::from_store(record.reference, bundle.clone())
            .map_err(|_| SessionStoreError::corrupt());
    }
    cold_checkpoint(state, reference)
}

fn cold_checkpoint(
    state: &StoreState,
    reference: CheckpointObjectRef,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let mut reachable = BTreeSet::new();
    let mut pending = vec![reference.root()];
    while let Some(address) = pending.pop() {
        if !reachable.insert(address) {
            continue;
        }
        let object = state
            .objects
            .get(&address)
            .ok_or(SessionStoreError::corrupt())?;
        pending.extend(object.children().iter().copied());
    }
    let objects = reachable
        .into_iter()
        .map(|address| {
            state
                .objects
                .get(&address)
                .cloned()
                .ok_or(SessionStoreError::corrupt())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let bundle = CheckpointObjectBundle::from_store(
        reference.checkpoint_id(),
        reference.integrity(),
        reference.root(),
        objects,
    )
    .map_err(|_| SessionStoreError::corrupt())?;
    StoredCheckpoint::from_store(reference, bundle).map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn branch(
    state: &StoreState,
    selector: SessionRef,
) -> Result<&BranchRecord, SessionStoreError> {
    let session = state
        .sessions
        .get(&selector.session_id())
        .ok_or(SessionStoreError::NotFound)?;
    if session.head.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    session
        .branches
        .get(&selector.branch_id())
        .ok_or(SessionStoreError::NotFound)
}

impl MemorySessionStore {
    pub(super) fn load_branch_sync(
        &self,
        selector: SessionRef,
    ) -> Result<LoadedBranch, SessionStoreError> {
        let state = self.lock()?;
        require_open(&state)?;
        let session = state
            .sessions
            .get(&selector.session_id())
            .ok_or(SessionStoreError::NotFound)?;
        if session.head.is_archived() {
            return Err(SessionStoreError::Archived);
        }
        let branch = session
            .branches
            .get(&selector.branch_id())
            .ok_or(SessionStoreError::NotFound)?;
        let checkpoint = stored_checkpoint(&state, branch.head.checkpoint())?;
        let loaded = LoadedBranch::from_store(session.head, branch.head, checkpoint);
        Ok(match state.horizons.get(&selector) {
            Some(horizon) => loaded.with_horizon(*horizon),
            None => loaded,
        })
    }

    pub(super) fn load_checkpoint_sync(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<StoredCheckpoint, SessionStoreError> {
        let state = self.lock()?;
        let Some(record) = state.checkpoints.get(&checkpoint_id) else {
            return Err(if state.pruned.contains_key(&checkpoint_id) {
                SessionStoreError::Pruned
            } else {
                SessionStoreError::NotFound
            });
        };
        stored_checkpoint(&state, record.reference)
    }

    pub(super) fn list_sync(
        &self,
        query: SessionQuery,
    ) -> Result<Page<SessionSummary>, SessionStoreError> {
        let state = self.lock()?;
        require_open(&state)?;
        if query.limit().get() > self.limits.page_size.get() {
            return Err(capacity(
                StoreCapacityBound::PageSize,
                query.limit().get(),
                self.limits.page_size.get(),
            ));
        }
        let mut matched = state
            .sessions
            .iter()
            .filter(|(id, session)| {
                query.after_session().is_none_or(|after| **id > after)
                    && (query.includes_archived() || !session.head.is_archived())
            })
            .map(|(_, session)| {
                SessionSummary::from_store(session.head, count(session.branches.len()))
            })
            .take(query.limit().get() + 1)
            .collect::<Vec<_>>();
        let has_more = matched.len() > query.limit().get();
        if has_more {
            matched.pop();
        }
        let next_after = has_more
            .then(|| matched.last().map(|summary| summary.head().session_id()))
            .flatten();
        Ok(Page::from_store(matched, next_after))
    }

    pub(super) fn read_feed_sync(
        &self,
        session_id: SessionId,
        after: FeedSeq,
        max_records: NonZeroUsize,
    ) -> Result<FeedPage, SessionStoreError> {
        let state = self.lock()?;
        let session = state
            .sessions
            .get(&session_id)
            .ok_or(SessionStoreError::NotFound)?;
        let feed = state
            .feeds
            .get(&session_id)
            .ok_or(SessionStoreError::corrupt())?;
        Ok(memory_feed::read(
            feed,
            || {
                session
                    .branches
                    .values()
                    .map(|branch| branch.head)
                    .collect()
            },
            after,
            max_records,
        ))
    }

    pub(super) fn feed_tail_sync(
        &self,
        session_id: SessionId,
    ) -> Result<FeedTail, SessionStoreError> {
        let state = self.lock()?;
        if !state.sessions.contains_key(&session_id) {
            return Err(SessionStoreError::NotFound);
        }
        state
            .feeds
            .get(&session_id)
            .map(memory_feed::tail)
            .ok_or(SessionStoreError::corrupt())
    }
}
