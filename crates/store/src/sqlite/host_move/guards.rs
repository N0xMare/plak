use super::*;
use plakous_agent::internal::host_move::AbandonUnopenedMove;

pub(in crate::sqlite) fn record_execution_open(
    b: &mut Backend,
    lease: &BranchLease,
) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    let row = branch_row(&tx, lease.session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, lease, now)?;
    tx.execute(
        "UPDATE imported_sessions SET execution_open=1 WHERE session_id=?1",
        [id(lease.session().session_id())],
    )
    .map_err(map_sqlite)?;
    tx.commit().map_err(map_sqlite)
}

pub(in crate::sqlite) fn abandon_unopened(
    b: &mut Backend,
    request: &AbandonUnopenedMove,
) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let (digest, phase) = receipt(&tx, request.operation)?.ok_or(SessionStoreError::NotFound)?;
    if request.digest.domain() != DigestDomain::Blob
        || request.digest.version() != 1
        || digest != request.digest.bytes()
    {
        return Err(SessionStoreError::IdentityConflict);
    }
    if phase == MoveState::Abandoned {
        return Ok(());
    }
    if phase != MoveState::Published {
        return Err(SessionStoreError::Conflict);
    }
    let held = request
        .leases
        .iter()
        .map(|lease| (lease.session(), lease))
        .collect::<BTreeMap<_, _>>();
    if held.len() != request.leases.len() {
        return Err(SessionStoreError::LeaseHeld);
    }
    check_session_headers(&tx, request.operation)?;
    let selected = check_published_heads(&tx, request.operation)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    prove_current_lines(&tx, request.operation, &held, now)?;
    tx.execute(
        "UPDATE runtime_moves SET phase=?2 WHERE operation=?1",
        params![
            workflow_id_bytes(request.operation),
            MoveState::Abandoned as i64
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute("UPDATE branches SET active_owner=NULL,active_fence=NULL,active_time=NULL,active_ns=NULL,active_duration=NULL,previous_time=NULL,previous_ns=NULL,released_owner=NULL,released_fence=NULL,released_time=NULL,released_ns=NULL,released_duration=NULL WHERE session_id IN(SELECT session_id FROM runtime_move_sessions WHERE operation=?1)", [workflow_id_bytes(request.operation)]).map_err(map_sqlite)?;
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    for selector in selected {
        b.holders.release(selector);
    }
    Ok(())
}

pub(super) fn check_session_headers(
    c: &Connection,
    operation: SessionWorkflowId,
) -> Result<(), SessionStoreError> {
    let invalid: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM runtime_move_sessions m LEFT JOIN sessions s USING(session_id) LEFT JOIN imported_sessions i USING(session_id) WHERE m.operation=?1 AND (s.session_id IS NULL OR s.archived<>0 OR s.revision<>m.revision OR i.operation IS NULL OR i.operation<>m.operation OR i.execution_open<>0))", [workflow_id_bytes(operation)], |row| row.get(0)).map_err(map_sqlite)?;
    if invalid {
        return Err(SessionStoreError::Conflict);
    }
    let missing: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM branches b JOIN runtime_move_sessions m USING(session_id) LEFT JOIN runtime_move_heads h ON h.operation=m.operation AND h.session_id=b.session_id AND h.branch_id=b.branch_id WHERE m.operation=?1 AND h.branch_id IS NULL)", [workflow_id_bytes(operation)], |row| row.get(0)).map_err(map_sqlite)?;
    if missing {
        return Err(SessionStoreError::Conflict);
    }
    Ok(())
}

pub(super) fn check_published_heads(
    c: &Connection,
    operation: SessionWorkflowId,
) -> Result<Vec<SessionRef>, SessionStoreError> {
    let invalid: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM runtime_move_heads h LEFT JOIN branches b USING(session_id,branch_id) WHERE h.operation=?1 AND (b.branch_id IS NULL OR b.checkpoint_id<>h.checkpoint_id OR b.revision<>h.revision))", [workflow_id_bytes(operation)], |row| row.get(0)).map_err(map_sqlite)?;
    if invalid {
        return Err(SessionStoreError::Conflict);
    }
    let mut query = c.prepare("SELECT session_id,branch_id FROM runtime_move_heads WHERE operation=?1 ORDER BY session_id,branch_id").map_err(map_sqlite)?;
    let selected = query
        .query_map([workflow_id_bytes(operation)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    if selected.is_empty() {
        return Err(SessionStoreError::Conflict);
    }
    selected
        .into_iter()
        .map(|(session, branch)| Ok(SessionRef::new(parse(&session)?, parse(&branch)?)))
        .collect()
}

fn prove_current_lines(
    c: &Connection,
    operation: SessionWorkflowId,
    held: &BTreeMap<SessionRef, &BranchLease>,
    now: LeaseInstant,
) -> Result<(), SessionStoreError> {
    let mut query = c
        .prepare("SELECT line_id FROM runtime_move_members WHERE operation=?1 ORDER BY line_id")
        .map_err(map_sqlite)?;
    let lines = query
        .query_map([workflow_id_bytes(operation)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    if lines.is_empty() || lines.len() != held.len() {
        return Err(SessionStoreError::LeaseHeld);
    }
    for line in lines {
        let line_id = parse(&line)?;
        require_line(c, line_id)?;
        let owner: Vec<u8> = c
            .query_row(
                "SELECT operation FROM runtime_move_lines WHERE line_id=?1",
                [&line],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        if owner != workflow_id_bytes(operation) {
            return Err(SessionStoreError::Conflict);
        }
        let current = lines::load_line_unchecked(c, line_id)?;
        let lease = held
            .get(&current.head())
            .ok_or(SessionStoreError::LeaseHeld)?;
        let row = branch_row(c, current.head())?.ok_or(SessionStoreError::NotFound)?;
        require_lease(&row, lease, now)?;
        let expected: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM runtime_move_heads WHERE operation=?1 AND session_id=?2 AND branch_id=?3 AND checkpoint_id=?4)", params![workflow_id_bytes(operation), id(current.head().session_id()), id(current.head().branch_id()), id(row.head.checkpoint().checkpoint_id())], |row| row.get(0)).map_err(map_sqlite)?;
        let topology = load_checkpoint(c, row.head.checkpoint().checkpoint_id())?
            .checkpoint()
            .manifest()
            .topology();
        if !expected || topology.line != line_id || topology.line_generation != current.generation()
        {
            return Err(SessionStoreError::Conflict);
        }
    }
    Ok(())
}
