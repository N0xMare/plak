//! The memory store's read-only and archive-time retirement proof.

use super::{
    CheckpointId, RuntimeLifecycle, SessionRef, SessionRevision, SessionStoreError, StoreState,
    lines, require_open,
};

pub(super) fn probe(
    state: &StoreState,
    selector: SessionRef,
    checkpoint: CheckpointId,
) -> Result<SessionRevision, SessionStoreError> {
    require_open(state)?;
    let row = state
        .sessions
        .get(&selector.session_id())
        .ok_or(SessionStoreError::NotFound)?;
    if row.head.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    verify_head(state, selector, checkpoint)?;
    Ok(row.head.revision())
}

pub(super) fn verify_head(
    state: &StoreState,
    selector: SessionRef,
    checkpoint: CheckpointId,
) -> Result<(), SessionStoreError> {
    let session_id = selector.session_id();
    let session = state
        .sessions
        .get(&session_id)
        .ok_or(SessionStoreError::NotFound)?;
    let head = session
        .branches
        .get(&selector.branch_id())
        .ok_or(SessionStoreError::NotFound)?;
    if head.active_lease.is_some() {
        return Err(SessionStoreError::LeaseHeld);
    }
    if head.head.checkpoint().checkpoint_id() != checkpoint {
        return Err(SessionStoreError::Conflict);
    }
    let record = state
        .checkpoints
        .get(&checkpoint)
        .ok_or(SessionStoreError::corrupt())?;
    let manifest = record.checkpoint.manifest();
    let topology = manifest.topology();
    lines::require_head(state, selector, topology.line, topology.line_generation)?;
    let depends_on_session = |id: CheckpointId| {
        state
            .checkpoints
            .get(&id)
            .is_some_and(|row| row.checkpoint.manifest().topology().session_id == session_id)
    };
    if session.branches.len() != 1
        || !matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent)
        || manifest
            .jobs()
            .rows()
            .iter()
            .any(|job| job.terminal.is_none())
        || !lines::terminals(state, topology.line)?.is_empty()
        || state.checkpoints.values().any(|row| {
            let topology = row.checkpoint.manifest().topology();
            topology.session_id != session_id
                && (topology
                    .parent_checkpoint_id
                    .is_some_and(depends_on_session)
                    || topology
                        .fork
                        .is_some_and(|fork| fork.session_id == session_id))
        })
    {
        return Err(SessionStoreError::Conflict);
    }
    Ok(())
}
