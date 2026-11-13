//! Atomic host-only retirement and invisible semantic installation.
#![allow(clippy::wildcard_imports, reason = "SQLite module vocabulary")]
use super::*;
use plakous_agent::internal::LineId;
use plakous_agent::internal::host_move::{
    self, MoveBranch, MoveDisposition, MoveInstall, MoveLine, MoveSession, MoveState, PrepareMove,
};

mod guards;
pub(super) use guards::{abandon_unopened, record_execution_open};

pub(super) fn require_line(c: &Connection, line: LineId) -> Result<(), SessionStoreError> {
    let phase = c.query_row("SELECT m.phase FROM runtime_move_lines l JOIN runtime_moves m USING(operation) WHERE l.line_id=?1", [id(line)], |r| r.get::<_,i64>(0)).optional().map_err(map_sqlite)?;
    require_visible(phase)
}

pub(super) fn require_visible(phase: Option<i64>) -> Result<(), SessionStoreError> {
    if let Some(phase) = phase.map(MoveState::decode).transpose()?
        && !phase.visible()
    {
        return Err(host_move::blocked_state(phase));
    }
    Ok(())
}

pub(super) fn require_branch(
    c: &Connection,
    selector: SessionRef,
) -> Result<(), SessionStoreError> {
    if let Some((line, _)) = lines::branch_line(c, selector)? {
        require_line(c, line)?;
    }
    Ok(())
}

fn receipt(
    c: &Connection,
    operation: SessionWorkflowId,
) -> Result<Option<(Vec<u8>, MoveState)>, SessionStoreError> {
    c.query_row(
        "SELECT digest,phase FROM runtime_moves WHERE operation=?1",
        [workflow_id_bytes(operation)],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|(digest, phase)| Ok((digest, MoveState::decode(phase)?)))
    .transpose()
}

fn require_held_head(
    c: &Connection,
    row: &BranchRow,
    held: &BTreeMap<SessionRef, &BranchLease>,
    now: LeaseInstant,
) -> Result<LineId, SessionStoreError> {
    let selector = row.head.session();
    let (line, _) = lines::branch_line(c, selector)?.ok_or(SessionStoreError::corrupt())?;
    let current = lines::load_line_unchecked(c, line)?.head();
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
    Ok(line)
}

pub(super) fn prepare(
    b: &mut Backend,
    request: &PrepareMove,
) -> Result<MoveInstall, SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    let held = request
        .leases
        .iter()
        .map(|l| (l.session(), l))
        .collect::<BTreeMap<_, _>>();
    if held.is_empty() || held.len() != request.leases.len() {
        return Err(SessionStoreError::Conflict);
    }
    let sessions = held.keys().map(|s| s.session_id()).collect::<BTreeSet<_>>();
    let mut snapshot = Vec::new();
    let mut line_ids = BTreeSet::new();
    for sid in sessions {
        let session = session_head(&tx, sid)?.ok_or(SessionStoreError::NotFound)?;
        let mut branches = Vec::new();
        for branch in branch_ids(&tx, sid)? {
            let selector = SessionRef::new(sid, parse(&branch)?);
            let row = branch_row_unchecked(&tx, selector)?.ok_or(SessionStoreError::NotFound)?;
            let line = require_held_head(&tx, &row, &held, now)?;
            let own = tx
                .query_row(
                    "SELECT operation FROM runtime_move_lines WHERE line_id=?1",
                    [id(line)],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(map_sqlite)?;
            if own.as_deref() != Some(request.operation.as_bytes().as_slice()) {
                require_line(&tx, line)?;
            }
            line_ids.insert(line);
            branches.push(MoveBranch {
                session: selector,
                revision: row.head.revision(),
                checkpoint: row.head.checkpoint().checkpoint_id(),
                fence_floor: row.last_fence,
            });
        }
        let checkpoints = session_checkpoints(&tx, sid)?;
        snapshot.push(MoveSession {
            session,
            feed_tail_floor: feed::tail(&tx, sid)?.tail().get(),
            branches,
            checkpoints,
        });
    }
    let lines = line_ids
        .into_iter()
        .map(|line| {
            Ok(MoveLine {
                line: lines::load_line_unchecked(&tx, line)?,
                terminals: lines::line_job_terminals(&tx, line)?,
            })
        })
        .collect::<Result<Vec<_>, SessionStoreError>>()?;
    let install = MoveInstall::new(snapshot, lines)?;
    if let Some((digest, phase)) = receipt(&tx, request.operation)? {
        return if digest == install.digest().bytes() && phase == MoveState::Prepared {
            Ok(install)
        } else {
            Err(SessionStoreError::Conflict)
        };
    }
    tx.execute(
        "INSERT INTO runtime_moves VALUES(?1,?2,0)",
        params![
            workflow_id_bytes(request.operation),
            install.digest().bytes().as_slice()
        ],
    )
    .map_err(map_sqlite)?;
    for session in install.sessions() {
        record_session(&tx, request.operation, session)?;
    }
    for line in host_move::line_ids(&install) {
        tx.execute("INSERT INTO runtime_move_lines VALUES(?1,?2) ON CONFLICT(line_id) DO UPDATE SET operation=excluded.operation", params![id(line), workflow_id_bytes(request.operation)]).map_err(map_sqlite)?;
        record_member(&tx, request.operation, line)?;
    }
    bump(&tx)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(install)
}

pub(super) fn branch_ids(
    c: &Connection,
    session: SessionId,
) -> Result<Vec<String>, SessionStoreError> {
    c.prepare("SELECT branch_id FROM branches WHERE session_id=?1 ORDER BY branch_id")
        .map_err(map_sqlite)?
        .query_map([id(session)], |row| row.get(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)
}

pub(super) fn recover_backend(
    b: &mut Backend,
    operation: SessionWorkflowId,
) -> Result<MoveInstall, SessionStoreError> {
    b.require_open()?;
    let tx = b.connection.transaction().map_err(map_sqlite)?;
    recover(&tx, operation)
}

fn recover(c: &Connection, operation: SessionWorkflowId) -> Result<MoveInstall, SessionStoreError> {
    let (digest, phase) = receipt(c, operation)?.ok_or(SessionStoreError::NotFound)?;
    if phase == MoveState::Published {
        guards::check_session_headers(c, operation)?;
        guards::check_published_heads(c, operation)?;
    } else if !matches!(phase, MoveState::Prepared | MoveState::Staged) {
        return Err(SessionStoreError::Conflict);
    }
    let mut statement = c
        .prepare(
            "SELECT session_id FROM runtime_move_sessions WHERE operation=?1 ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let ids = statement
        .query_map([workflow_id_bytes(operation)], |r| r.get::<_, String>(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    let mut sessions = Vec::new();
    let mut line_ids = BTreeSet::new();
    for idv in ids {
        let sid = parse(&idv)?;
        let session = session_head(c, sid)?.ok_or(SessionStoreError::corrupt())?;
        let mut statement = c
            .prepare("SELECT branch_id FROM branches WHERE session_id=?1 ORDER BY branch_id")
            .map_err(map_sqlite)?;
        let branches = statement
            .query_map([idv], |r| r.get::<_, String>(0))
            .map_err(map_sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite)?
            .into_iter()
            .map(|branch| {
                let selector = SessionRef::new(sid, parse(&branch)?);
                let row = branch_row_unchecked(c, selector)?.ok_or(SessionStoreError::corrupt())?;
                line_ids.insert(
                    lines::branch_line(c, selector)?
                        .ok_or(SessionStoreError::corrupt())?
                        .0,
                );
                Ok(MoveBranch {
                    session: selector,
                    revision: row.head.revision(),
                    checkpoint: row.head.checkpoint().checkpoint_id(),
                    fence_floor: if phase == MoveState::Published {
                        let floor: Vec<u8> = c.query_row(
                            "SELECT fence_floor FROM runtime_move_heads WHERE operation=?1 AND session_id=?2 AND branch_id=?3",
                            params![workflow_id_bytes(operation), id(sid), id(selector.branch_id())],
                            |row| row.get(0),
                        ).map_err(map_sqlite)?;
                        read_u64(&floor)?
                    } else { row.last_fence },
                })
            })
            .collect::<Result<Vec<_>, SessionStoreError>>()?;
        sessions.push(MoveSession {
            session,
            feed_tail_floor: if phase == MoveState::Published {
                let floor: i64 = c.query_row("SELECT feed_floor FROM runtime_move_sessions WHERE operation=?1 AND session_id=?2", params![workflow_id_bytes(operation), id(sid)], |row| row.get(0)).map_err(map_sqlite)?;
                nonnegative(floor)?
            } else { feed::tail(c, sid)?.tail().get() },
            branches,
            checkpoints: session_checkpoints(c, sid)?,
        });
    }
    let lines = line_ids
        .into_iter()
        .map(|line| {
            Ok(MoveLine {
                line: lines::load_line_unchecked(c, line)?,
                terminals: lines::line_job_terminals(c, line)?,
            })
        })
        .collect::<Result<Vec<_>, SessionStoreError>>()?;
    let install = MoveInstall::new(sessions, lines)?;
    if digest != install.digest().bytes() {
        return Err(SessionStoreError::IdentityConflict);
    }
    Ok(install)
}

pub(super) fn stage(
    b: &mut Backend,
    operation: SessionWorkflowId,
    install: &MoveInstall,
) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let encoded = crate::move_install::bundles(install)?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    if let Some((digest, phase)) = receipt(&tx, operation)? {
        return if digest == install.digest().bytes()
            && matches!(phase, MoveState::Staged | MoveState::Published)
        {
            Ok(())
        } else {
            Err(SessionStoreError::Conflict)
        };
    }
    for session in install.sessions() {
        require_unused_session_identity(&tx, session.session.session_id())?;
        enforce(
            StoreCapacityBound::BranchesPerSession,
            u64n(session.branches.len()),
            b.limits.max_branches_per_session().get(),
        )?;
    }
    for line in install.lines() {
        if tx
            .query_row(
                "SELECT 1 FROM lines WHERE line_id=?1",
                [id(line.line.line())],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite)?
            .is_some()
        {
            return Err(SessionStoreError::IdentityConflict);
        }
    }
    enforce(
        StoreCapacityBound::Sessions,
        read_stats(&tx)?.sessions + u64n(install.sessions().len()),
        b.limits.max_sessions().get(),
    )?;
    tx.execute(
        "INSERT INTO runtime_moves VALUES(?1,?2,2)",
        params![
            workflow_id_bytes(operation),
            install.digest().bytes().as_slice()
        ],
    )
    .map_err(map_sqlite)?;
    for bundle in &encoded {
        if pruned_checkpoint(&tx, bundle.checkpoint_id())? {
            return Err(SessionStoreError::IdentityConflict);
        }
        insert_candidate(&tx, &candidate(bundle)?, b.limits, StoreAdmission::Ordinary)?;
        tx.execute(
            "INSERT INTO imported_checkpoints VALUES(?1)",
            [id(bundle.checkpoint_id())],
        )
        .map_err(map_sqlite)?;
    }
    for session in install.sessions() {
        stage_session(&tx, operation, session)?;
    }
    for imported in install.lines() {
        stage_line(&tx, operation, imported)?;
    }
    tx.commit().map_err(map_sqlite)?;
    Ok(())
}

fn stage_line(
    tx: &Transaction<'_>,
    operation: SessionWorkflowId,
    imported: &MoveLine,
) -> Result<(), SessionStoreError> {
    let line = &imported.line;
    tx.execute(
        "INSERT INTO lines VALUES(?1,?2,?3,?4,?5)",
        params![
            id(line.line()),
            id(line.head().session_id()),
            id(line.head().branch_id()),
            u64b(line.generation()),
            line.origin().encode()
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "INSERT INTO runtime_move_lines VALUES(?1,?2)",
        params![id(line.line()), workflow_id_bytes(operation)],
    )
    .map_err(map_sqlite)?;
    record_member(tx, operation, line.line())?;
    for terminal in &imported.terminals {
        tx.execute(
            "INSERT INTO line_job_terminals VALUES(?1,?2,?3,?4)",
            params![
                id(line.line()),
                u64b(line.generation()),
                i64::from(terminal.number),
                terminal
                    .encode()
                    .map_err(|_| SessionStoreError::corrupt())?
            ],
        )
        .map_err(map_sqlite)?;
    }
    Ok(())
}

fn stage_session(
    tx: &Transaction<'_>,
    operation: SessionWorkflowId,
    session: &MoveSession,
) -> Result<(), SessionStoreError> {
    let s = session.session;
    tx.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8,0)",
        params![
            id(s.session_id()),
            id(s.initial_branch()),
            u64b(s.revision().get()),
            u64b(s.created_at().seconds()),
            i64::from(s.created_at().nanoseconds()),
            durability_tag(s.durability())?,
            i64n(session.feed_tail_floor)?,
            i64n(
                session
                    .feed_tail_floor
                    .checked_add(1)
                    .ok_or(SessionStoreError::IdentityConflict)?
            )?
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "INSERT INTO imported_sessions(session_id,operation) VALUES(?1,?2)",
        params![id(s.session_id()), workflow_id_bytes(operation)],
    )
    .map_err(map_sqlite)?;
    record_session(tx, operation, session)?;
    for branch in &session.branches {
        let row = checkpoint_row(tx, branch.checkpoint)?.ok_or(SessionStoreError::corrupt())?;
        let topology = session
            .checkpoints
            .iter()
            .find(|c| c.id() == branch.checkpoint)
            .ok_or(SessionStoreError::corrupt())?
            .manifest()
            .topology();
        insert_branch(
            tx,
            BranchHead::from_store(branch.session, branch.revision, row.reference),
            None,
            branch.fence_floor,
            None,
            (topology.line, topology.line_generation),
        )?;
        tx.execute(
            "INSERT INTO imported_branches VALUES(?1,?2)",
            params![
                id(branch.session.session_id()),
                id(branch.session.branch_id())
            ],
        )
        .map_err(map_sqlite)?;
        if let Some(fork) = topology.fork {
            tx.execute("UPDATE branches SET fork_session=?3,fork_branch=?4,fork_checkpoint=?5,fork_revision=?6 WHERE session_id=?1 AND branch_id=?2", params![id(branch.session.session_id()), id(branch.session.branch_id()), id(fork.session_id), id(fork.branch_id), id(fork.checkpoint_id), u64b(fork.branch_revision)]).map_err(map_sqlite)?;
        }
    }
    for checkpoint in &session.checkpoints {
        if let Some(fork) = checkpoint.manifest().topology().fork
            && checkpoint_row(tx, fork.checkpoint_id)?.is_none()
        {
            mark_provenance(tx, fork.checkpoint_id, fork.session_id, fork.branch_id)?;
        }
        if let Some(parent) = checkpoint.manifest().topology().parent_checkpoint_id
            && checkpoint_row(tx, parent)?.is_none()
        {
            let topology = checkpoint.manifest().topology();
            mark_provenance(tx, parent, topology.session_id, topology.branch_id)?;
        }
    }
    meta(tx, 1, u64n(session.branches.len()))?;
    Ok(())
}

fn mark_provenance(
    c: &Connection,
    checkpoint: CheckpointId,
    session: SessionId,
    branch: plakous_agent::internal::BranchId,
) -> Result<(), SessionStoreError> {
    let count = c.execute(
        "INSERT INTO pruned_checkpoints VALUES(?1,?2,?3) ON CONFLICT(checkpoint_id) DO UPDATE SET checkpoint_id=excluded.checkpoint_id WHERE pruned_checkpoints.session_id=excluded.session_id AND pruned_checkpoints.branch_id=excluded.branch_id",
        params![id(checkpoint), id(session), id(branch)],
    ).map_err(map_sqlite)?;
    if count != 1 {
        return Err(SessionStoreError::IdentityConflict);
    }
    Ok(())
}

fn record_session(
    c: &Connection,
    operation: SessionWorkflowId,
    session: &MoveSession,
) -> Result<(), SessionStoreError> {
    c.execute(
        "INSERT INTO runtime_move_sessions VALUES(?1,?2,?3,?4)",
        params![
            workflow_id_bytes(operation),
            id(session.session.session_id()),
            u64b(session.session.revision().get()),
            i64n(session.feed_tail_floor)?
        ],
    )
    .map_err(map_sqlite)?;
    for branch in &session.branches {
        c.execute(
            "INSERT INTO runtime_move_heads VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                workflow_id_bytes(operation),
                id(branch.session.session_id()),
                id(branch.session.branch_id()),
                id(branch.checkpoint),
                u64b(branch.revision.get()),
                u64b(branch.fence_floor)
            ],
        )
        .map_err(map_sqlite)?;
    }
    Ok(())
}

fn record_member(
    c: &Connection,
    operation: SessionWorkflowId,
    line: LineId,
) -> Result<(), SessionStoreError> {
    c.execute(
        "INSERT INTO runtime_move_members VALUES(?1,?2)",
        params![workflow_id_bytes(operation), id(line)],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

fn session_checkpoints(
    c: &Connection,
    sid: SessionId,
) -> Result<Vec<RuntimeCheckpoint>, SessionStoreError> {
    let mut statement = c
        .prepare("SELECT checkpoint_id FROM branches WHERE session_id=?1 ORDER BY checkpoint_id")
        .map_err(map_sqlite)?;
    let ids = statement
        .query_map([id(sid)], |r| r.get::<_, String>(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    ids.into_iter()
        .map(|id| {
            Ok(load_checkpoint(c, parse(&id)?)?
                .checkpoint()
                .as_ref()
                .clone())
        })
        .collect()
}

pub(super) fn finish(
    b: &mut Backend,
    operation: SessionWorkflowId,
    digest: Digest,
    disposition: MoveDisposition,
) -> Result<(), SessionStoreError> {
    b.require_open()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let (old, phase) = receipt(&tx, operation)?.ok_or(SessionStoreError::NotFound)?;
    if digest.domain() != DigestDomain::Blob || digest.version() != 1 || old != digest.bytes() {
        return Err(SessionStoreError::IdentityConflict);
    }
    let next = phase.transition(disposition)?;
    if next == phase {
        return Ok(());
    }
    if tx.query_row("SELECT 1 FROM runtime_move_members members LEFT JOIN runtime_move_lines current USING(line_id) WHERE members.operation=?1 AND (current.operation IS NULL OR current.operation<>members.operation) LIMIT 1", [workflow_id_bytes(operation)], |_| Ok(())).optional().map_err(map_sqlite)?.is_some() {
        return Err(SessionStoreError::Conflict);
    }
    let mut statement = tx.prepare("SELECT b.session_id,b.branch_id FROM branches b JOIN runtime_move_lines l USING(line_id) WHERE l.operation=?1").map_err(map_sqlite)?;
    let selected = statement
        .query_map([workflow_id_bytes(operation)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    drop(statement);
    if selected.is_empty() {
        return Err(SessionStoreError::Conflict);
    }
    tx.execute(
        "UPDATE runtime_moves SET phase=?2 WHERE operation=?1",
        params![workflow_id_bytes(operation), next as i64],
    )
    .map_err(map_sqlite)?;
    bump(&tx)?;
    if matches!(next, MoveState::Retired | MoveState::Abandoned) {
        tx.execute("UPDATE branches SET active_owner=NULL,active_fence=NULL,active_time=NULL,active_ns=NULL,active_duration=NULL,previous_time=NULL,previous_ns=NULL,released_owner=NULL,released_fence=NULL,released_time=NULL,released_ns=NULL,released_duration=NULL WHERE line_id IN(SELECT line_id FROM runtime_move_lines WHERE operation=?1)", [workflow_id_bytes(operation)]).map_err(map_sqlite)?;
    }
    tx.commit().map_err(map_sqlite)?;
    if matches!(next, MoveState::Retired | MoveState::Abandoned) {
        for (sid, branch) in selected {
            b.holders
                .release(SessionRef::new(parse(&sid)?, parse(&branch)?));
        }
    }
    Ok(())
}
