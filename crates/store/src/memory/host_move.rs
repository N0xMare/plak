//! Host-only semantic batches and the shared runtime visibility gate.
use super::{
    BTreeMap, BTreeSet, BranchHead, BranchRecord, MemorySessionStore, SessionRecord, SessionRef,
    SessionStore, SessionStoreError, SessionWorkflowId, StoreAdmission, StoreCapacityBound,
    StoreState, capacity, install_candidate, lines, publication_candidate, require_lease,
    require_open,
};
use plakous_agent::internal::host_move::{
    self, MoveBranch, MoveDisposition, MoveInstall, MoveLine, MoveSession, MoveState, PrepareMove,
};
use plakous_agent::internal::{BranchLease, Digest, LeaseInstant, LineId};

mod guards;

#[derive(Clone)]
pub(super) struct Receipt {
    pub(super) install: MoveInstall,
    pub(super) state: MoveState,
}

pub(super) fn require_line(state: &StoreState, line: LineId) -> Result<(), SessionStoreError> {
    if let Some(operation) = state.move_lines.get(&line)
        && !state
            .moves
            .get(operation)
            .ok_or(SessionStoreError::corrupt())?
            .state
            .visible()
    {
        return Err(host_move::blocked_state(
            state
                .moves
                .get(operation)
                .ok_or(SessionStoreError::corrupt())?
                .state,
        ));
    }
    Ok(())
}

pub(super) fn require_branch(
    state: &StoreState,
    selector: SessionRef,
) -> Result<(), SessionStoreError> {
    let row = state
        .sessions
        .get(&selector.session_id())
        .and_then(|s| s.branches.get(&selector.branch_id()))
        .ok_or(SessionStoreError::NotFound)?;
    let checkpoint = state
        .checkpoints
        .get(&row.head.checkpoint().checkpoint_id())
        .ok_or(SessionStoreError::corrupt())?;
    require_line(state, checkpoint.checkpoint.manifest().topology().line)
}

pub(super) fn session_visible(
    state: &StoreState,
    session: plakous_agent::internal::SessionId,
) -> bool {
    !state.moves.values().any(|receipt| {
        !receipt.state.visible()
            && receipt
                .install
                .sessions()
                .iter()
                .any(|member| member.session.session_id() == session)
    })
}

fn require_held_head(
    state: &StoreState,
    row: &BranchRecord,
    held: &BTreeMap<SessionRef, &BranchLease>,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    let selector = row.head.session();
    let checkpoint = state
        .checkpoints
        .get(&row.head.checkpoint().checkpoint_id())
        .ok_or(SessionStoreError::corrupt())?;
    let line = checkpoint.checkpoint.manifest().topology().line;
    let current = lines::load_unchecked(state, line)?.head();
    if current == selector {
        require_lease(
            row,
            held.get(&selector).ok_or(SessionStoreError::LeaseHeld)?,
            now,
        )?;
    } else {
        if held.contains_key(&selector) {
            return Err(SessionStoreError::NotLineHead);
        }
        if !held.contains_key(&current) {
            return Err(SessionStoreError::LeaseHeld);
        }
    }
    Ok(())
}

impl MemorySessionStore {
    pub(super) fn recover_move_sync(
        &self,
        operation: SessionWorkflowId,
    ) -> Result<MoveInstall, SessionStoreError> {
        let state = self.lock()?;
        require_open(&state)?;
        let receipt = state
            .moves
            .get(&operation)
            .ok_or(SessionStoreError::NotFound)?;
        if receipt.state == MoveState::Published {
            guards::require_unopened(&state, operation, &receipt.install)?;
        } else if !matches!(receipt.state, MoveState::Prepared | MoveState::Staged) {
            return Err(SessionStoreError::Conflict);
        }
        Ok(receipt.install.clone())
    }
    pub(super) fn prepare_move_sync(
        &self,
        request: &PrepareMove,
    ) -> Result<MoveInstall, SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        if request.leases.is_empty() {
            return Err(SessionStoreError::Conflict);
        }
        let now = self.lease_now()?;
        let held = request
            .leases
            .iter()
            .map(|l| (l.session(), l))
            .collect::<BTreeMap<_, _>>();
        if held.len() != request.leases.len() {
            return Err(SessionStoreError::Conflict);
        }
        let sessions = held.keys().map(|s| s.session_id()).collect::<BTreeSet<_>>();
        let mut snapshot = Vec::new();
        let mut line_ids = BTreeSet::new();
        for id in sessions {
            let session = state.sessions.get(&id).ok_or(SessionStoreError::NotFound)?;
            let mut branches = Vec::new();
            for row in session.branches.values() {
                let selector = row.head.session();
                require_held_head(&state, row, &held, now)?;
                let checkpoint = state
                    .checkpoints
                    .get(&row.head.checkpoint().checkpoint_id())
                    .ok_or(SessionStoreError::corrupt())?;
                let line = checkpoint.checkpoint.manifest().topology().line;
                if state.move_lines.get(&line) != Some(&request.operation) {
                    require_line(&state, line)?;
                }
                line_ids.insert(line);
                branches.push(MoveBranch {
                    session: selector,
                    revision: row.head.revision(),
                    checkpoint: checkpoint.checkpoint.id(),
                    fence_floor: row.last_fence,
                });
            }
            let checkpoints = branches
                .iter()
                .map(|branch| {
                    state
                        .checkpoints
                        .get(&branch.checkpoint)
                        .map(|row| row.checkpoint.as_ref().clone())
                        .ok_or(SessionStoreError::corrupt())
                })
                .collect::<Result<_, _>>()?;
            snapshot.push(MoveSession {
                session: session.head,
                feed_tail_floor: crate::memory_feed::tail(
                    state.feeds.get(&id).ok_or(SessionStoreError::corrupt())?,
                )
                .tail()
                .get(),
                branches,
                checkpoints,
            });
        }
        let lines = line_ids
            .into_iter()
            .map(|line| {
                Ok(MoveLine {
                    line: lines::load_unchecked(&state, line)?,
                    terminals: lines::terminals(&state, line)?,
                })
            })
            .collect::<Result<Vec<_>, SessionStoreError>>()?;
        let install = MoveInstall::new(snapshot, lines)?;
        if let Some(old) = state.moves.get(&request.operation) {
            return if old.install.digest() == install.digest() && old.state == MoveState::Prepared {
                Ok(old.install.clone())
            } else {
                Err(SessionStoreError::Conflict)
            };
        }
        for line in host_move::line_ids(&install) {
            state.move_lines.insert(line, request.operation);
        }
        state.moves.insert(
            request.operation,
            Receipt {
                install: install.clone(),
                state: MoveState::Prepared,
            },
        );
        Ok(install)
    }

    pub(super) fn stage_move_sync(
        &self,
        operation: SessionWorkflowId,
        install: MoveInstall,
    ) -> Result<(), SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        if let Some(old) = state.moves.get(&operation) {
            return if old.install.digest() == install.digest()
                && matches!(old.state, MoveState::Staged | MoveState::Published)
            {
                Ok(())
            } else {
                Err(SessionStoreError::Conflict)
            };
        }
        let mut next = state.clone();
        for line in host_move::line_ids(&install) {
            if next.lines.lines.contains_key(&line) {
                return Err(SessionStoreError::IdentityConflict);
            }
        }
        for bundle in crate::move_install::bundles(&install)? {
            if next.checkpoints.contains_key(&bundle.checkpoint_id())
                || next.pruned.contains_key(&bundle.checkpoint_id())
            {
                return Err(SessionStoreError::IdentityConflict);
            }
            install_candidate(
                &mut next,
                publication_candidate(&bundle)?,
                self.limits,
                StoreAdmission::Ordinary,
            )?;
        }
        for session in install.sessions() {
            if session.branches.len() > self.limits.max_branches_per_session().get() {
                return Err(capacity(
                    StoreCapacityBound::BranchesPerSession,
                    session.branches.len(),
                    self.limits.max_branches_per_session().get(),
                ));
            }
            if next.sessions.contains_key(&session.session.session_id())
                || next.deletes.contains_key(&session.session.session_id())
            {
                return Err(SessionStoreError::IdentityConflict);
            }
            if next.sessions.len() >= self.limits.max_sessions().get() {
                return Err(capacity(
                    StoreCapacityBound::Sessions,
                    next.sessions.len() + 1,
                    self.limits.max_sessions().get(),
                ));
            }
            let mut branches = BTreeMap::new();
            for branch in &session.branches {
                let reference = next
                    .checkpoints
                    .get(&branch.checkpoint)
                    .ok_or(SessionStoreError::corrupt())?
                    .reference;
                branches.insert(
                    branch.session.branch_id(),
                    BranchRecord {
                        head: BranchHead::from_store(branch.session, branch.revision, reference),
                        last_fence: branch.fence_floor,
                        active_lease: None,
                        previous_renewal: None,
                        released_lease: None,
                    },
                );
            }
            next.feeds.insert(
                session.session.session_id(),
                crate::memory_feed::SessionFeed::retained_empty(session.feed_tail_floor),
            );
            next.sessions.insert(
                session.session.session_id(),
                SessionRecord {
                    head: session.session,
                    branches,
                    archive_receipt: None,
                },
            );
        }
        for line in install.lines() {
            lines::install_move(&mut next, line);
            next.move_lines.insert(line.line.line(), operation);
        }
        next.moves.insert(
            operation,
            Receipt {
                install,
                state: MoveState::Staged,
            },
        );
        *state = next;
        Ok(())
    }

    pub(super) fn finish_move_sync(
        &self,
        operation: SessionWorkflowId,
        digest: Digest,
        disposition: MoveDisposition,
    ) -> Result<(), SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        let row = state
            .moves
            .get(&operation)
            .ok_or(SessionStoreError::NotFound)?;
        if row.install.digest() != digest {
            return Err(SessionStoreError::IdentityConflict);
        }
        let next = row.state.transition(disposition)?;
        if next == row.state {
            return Ok(());
        }
        let lines = host_move::line_ids(&row.install).collect::<BTreeSet<_>>();
        if lines
            .iter()
            .any(|line| state.move_lines.get(line) != Some(&operation))
        {
            return Err(SessionStoreError::Conflict);
        }
        state
            .moves
            .get_mut(&operation)
            .ok_or(SessionStoreError::corrupt())?
            .state = next;
        if matches!(next, MoveState::Retired | MoveState::Abandoned) {
            let selected = state
                .checkpoints
                .values()
                .filter(|c| lines.contains(&c.checkpoint.manifest().topology().line))
                .map(|c| {
                    let t = c.checkpoint.manifest().topology();
                    SessionRef::new(t.session_id, t.branch_id)
                })
                .collect::<BTreeSet<_>>();
            for selector in selected {
                let branch = state
                    .sessions
                    .get_mut(&selector.session_id())
                    .and_then(|s| s.branches.get_mut(&selector.branch_id()))
                    .ok_or(SessionStoreError::corrupt())?;
                branch.active_lease = None;
                branch.previous_renewal = None;
                branch.released_lease = None;
            }
        }
        Ok(())
    }
}
