use super::{
    BTreeMap, BranchLease, MemorySessionStore, MoveInstall, MoveState, SessionStore,
    SessionStoreError, SessionWorkflowId, StoreState, lines, require_branch, require_lease,
    require_open,
};
use plakous_agent::internal::host_move::AbandonUnopenedMove;

impl MemorySessionStore {
    pub(in crate::memory) fn record_execution_open_sync(
        &self,
        lease: &BranchLease,
    ) -> Result<(), SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        require_branch(&state, lease.session())?;
        let row = state
            .sessions
            .get(&lease.session().session_id())
            .and_then(|session| session.branches.get(&lease.session().branch_id()))
            .ok_or(SessionStoreError::NotFound)?;
        require_lease(row, lease, self.lease_now()?)?;
        let imported = state.moves.values().any(|receipt| {
            matches!(
                receipt.state,
                MoveState::Staged | MoveState::Published | MoveState::Abandoned
            ) && receipt
                .install
                .sessions()
                .iter()
                .any(|session| session.session.session_id() == lease.session().session_id())
        });
        if imported {
            state
                .move_execution_open
                .insert(lease.session().session_id());
        }
        Ok(())
    }

    pub(in crate::memory) fn abandon_unopened_move_sync(
        &self,
        request: &AbandonUnopenedMove,
    ) -> Result<(), SessionStoreError> {
        let mut state = self.lock()?;
        require_open(&state)?;
        let receipt = state
            .moves
            .get(&request.operation)
            .ok_or(SessionStoreError::NotFound)?;
        if receipt.install.digest() != request.digest {
            return Err(SessionStoreError::IdentityConflict);
        }
        if receipt.state == MoveState::Abandoned {
            return Ok(());
        }
        if receipt.state != MoveState::Published {
            return Err(SessionStoreError::Conflict);
        }
        let install = receipt.install.clone();
        require_unopened(&state, request.operation, &install)?;
        let held = request
            .leases
            .iter()
            .map(|lease| (lease.session(), lease))
            .collect::<BTreeMap<_, _>>();
        if held.len() != request.leases.len() || held.len() != install.lines().len() {
            return Err(SessionStoreError::LeaseHeld);
        }
        let now = self.lease_now()?;
        for line in install.lines() {
            if state.move_lines.get(&line.line.line()) != Some(&request.operation)
                || lines::load_unchecked(&state, line.line.line())? != line.line
            {
                return Err(SessionStoreError::Conflict);
            }
            let lease = held
                .get(&line.line.head())
                .ok_or(SessionStoreError::LeaseHeld)?;
            let branch = state
                .sessions
                .get(&lease.session().session_id())
                .and_then(|s| s.branches.get(&lease.session().branch_id()))
                .ok_or(SessionStoreError::NotFound)?;
            require_lease(branch, lease, now)?;
        }
        state
            .moves
            .get_mut(&request.operation)
            .ok_or(SessionStoreError::corrupt())?
            .state = MoveState::Abandoned;
        for session in install.sessions() {
            for branch in state
                .sessions
                .get_mut(&session.session.session_id())
                .ok_or(SessionStoreError::corrupt())?
                .branches
                .values_mut()
            {
                branch.active_lease = None;
                branch.previous_renewal = None;
                branch.released_lease = None;
            }
        }
        Ok(())
    }
}

pub(super) fn require_unopened(
    state: &StoreState,
    operation: SessionWorkflowId,
    install: &MoveInstall,
) -> Result<(), SessionStoreError> {
    for line in install.lines() {
        if state.move_lines.get(&line.line.line()) != Some(&operation)
            || lines::load_unchecked(state, line.line.line())? != line.line
        {
            return Err(SessionStoreError::Conflict);
        }
    }
    for session in install.sessions() {
        let id = session.session.session_id();
        let current = state.sessions.get(&id).ok_or(SessionStoreError::NotFound)?;
        if current.head != session.session
            || state.move_execution_open.contains(&id)
            || current.branches.len() != session.branches.len()
        {
            return Err(SessionStoreError::Conflict);
        }
        for branch in &session.branches {
            let current = current
                .branches
                .get(&branch.session.branch_id())
                .ok_or(SessionStoreError::NotFound)?;
            if current.head.revision() != branch.revision
                || current.head.checkpoint().checkpoint_id() != branch.checkpoint
            {
                return Err(SessionStoreError::Conflict);
            }
        }
    }
    Ok(())
}
