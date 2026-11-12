//! Agent lines in `SQLite` (spec 01 §5.1): the `lines` head rows, each
//! branch's fixed line membership and the `line_job_terminals` table, all
//! read and written inside the transaction of the mutation they fence.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;
use plakous_agent::internal::{
    FeedFact, LineContinuation, LineId, LineJobTerminal, LineOrigin, LineRecord, LineStep,
    LineTerminalsReceipt, RecordLineJobTerminals,
};

/// A branch's fixed line membership.
pub(super) fn branch_line(
    c: &Connection,
    s: SessionRef,
) -> Result<Option<(LineId, u64)>, SessionStoreError> {
    c.query_row(
        "SELECT line_id,line_generation FROM branches WHERE session_id=?1 AND branch_id=?2",
        params![id(s.session_id()), id(s.branch_id())],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|(line, generation)| Ok((parse(&line)?, read_u64(&generation)?)))
    .transpose()
}

fn line_row(
    c: &Connection,
    line: LineId,
) -> Result<Option<(SessionRef, u64, Option<String>)>, SessionStoreError> {
    c.query_row(
        "SELECT head_session,head_branch,generation,forked_from FROM lines WHERE line_id=?1",
        [id(line)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        Ok((
            SessionRef::new(parse(&v.0)?, parse(&v.1)?),
            read_u64(&v.2)?,
            v.3,
        ))
    })
    .transpose()
}

/// Refuses unless `s` is `line`'s head at exactly `generation`.
pub(super) fn require_line_head(
    c: &Connection,
    s: SessionRef,
    line: LineId,
    generation: u64,
) -> Result<(), SessionStoreError> {
    match line_row(c, line)? {
        Some((head, current, _)) if head == s && current == generation => Ok(()),
        _ => Err(SessionStoreError::NotLineHead),
    }
}

/// Requires that the branch `s` names exactly `line` at `generation`, then
/// that it is still that line's head.
pub(super) fn require_branch_is_head(
    c: &Connection,
    s: SessionRef,
    line: LineId,
    generation: u64,
    invalid: StoreInvalidReason,
) -> Result<(), SessionStoreError> {
    if branch_line(c, s)? != Some((line, generation)) {
        return Err(SessionStoreError::Invalid(invalid));
    }
    require_line_head(c, s, line, generation)
}

/// One new branch's line step, applied inside its publishing transaction
/// after the branch row (which already names `line` and `generation`) exists.
pub(super) struct Step<'a> {
    pub(super) target: SessionRef,
    pub(super) line: LineId,
    pub(super) generation: u64,
    /// The branch the target publishes from: a fork's source, or the head a
    /// carry takes over. A genesis that starts a line has none.
    pub(super) source: Option<(SessionRef, CheckpointId)>,
    /// The pending line job terminals an advancing target folded.
    pub(super) consumed: &'a [u32],
    pub(super) origin: &'a LineOrigin,
    pub(super) invalid: StoreInvalidReason,
}

/// Applies `step`. Returns whether the branch became the next head of an
/// existing line.
pub(super) fn apply_step(tx: &Transaction<'_>, step: &Step<'_>) -> Result<bool, SessionStoreError> {
    let Step {
        target,
        line,
        generation,
        source,
        consumed,
        origin,
        invalid,
    } = *step;
    match LineStep::of(target.branch_id(), line, generation)
        .ok_or(SessionStoreError::Invalid(invalid))?
    {
        LineStep::NewLine => {
            if line_row(tx, line)?.is_some() {
                return Err(SessionStoreError::IdentityConflict);
            }
            let origin = match source {
                Some((source, checkpoint)) => {
                    let (source_line, _) =
                        branch_line(tx, source)?.ok_or(SessionStoreError::corrupt())?;
                    LineOrigin::Forked {
                        line: source_line,
                        source,
                        checkpoint,
                    }
                }
                None => origin.clone(),
            };
            exactly_one(
                tx.execute(
                    "INSERT INTO lines VALUES(?1,?2,?3,?4,?5)",
                    params![
                        id(line),
                        id(target.session_id()),
                        id(target.branch_id()),
                        u64b(1),
                        origin.encode()
                    ],
                )
                .map_err(map_sqlite)?,
            )?;
            Ok(false)
        }
        LineStep::Advance { line, from } => {
            let Some((source, _)) = source else {
                return Err(SessionStoreError::Invalid(invalid));
            };
            if branch_line(tx, source)? != Some((line, from)) {
                return Err(SessionStoreError::Invalid(invalid));
            }
            let Some((head, current, _)) = line_row(tx, line)? else {
                return Err(SessionStoreError::NotLineHead);
            };
            if current != from || source != head {
                return Err(SessionStoreError::NotLineHead);
            }
            // Pending evidence belongs to the moving head: the target folded
            // exactly it into the ledger it takes over, so it leaves the
            // table in this transaction.
            if pending_jobs(tx, line)? != sorted(consumed) {
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::LineTerminalsNotFolded,
                ));
            }
            tx.execute(
                "DELETE FROM line_job_terminals WHERE line_id=?1",
                [id(line)],
            )
            .map_err(map_sqlite)?;
            exactly_one(
                tx.execute(
                    "UPDATE lines SET head_session=?2,head_branch=?3,generation=?4 WHERE line_id=?1",
                    params![
                        id(line),
                        id(target.session_id()),
                        id(target.branch_id()),
                        u64b(generation)
                    ],
                )
                .map_err(map_sqlite)?,
            )?;
            Ok(true)
        }
    }
}

fn sorted(jobs: &[u32]) -> Vec<u32> {
    let mut jobs = jobs.to_vec();
    jobs.sort_unstable();
    jobs.dedup();
    jobs
}

fn pending_jobs(c: &Connection, line: LineId) -> Result<Vec<u32>, SessionStoreError> {
    let mut statement = c
        .prepare("SELECT DISTINCT job FROM line_job_terminals WHERE line_id=?1 ORDER BY job")
        .map_err(map_sqlite)?;
    let mut rows = statement.query([id(line)]).map_err(map_sqlite)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let job: i64 = row.get(0).map_err(map_sqlite)?;
        jobs.push(u32::try_from(job).map_err(|_| SessionStoreError::corrupt())?);
    }
    Ok(jobs)
}

/// Fences the head a line-continuing genesis takes over, exactly as a
/// rebind fences its source: the presented lease is the head's current one,
/// the head did not move since the ledger was read, and it is quiescent.
/// Returns the head's branch and checkpoint for the line step.
pub(super) fn require_carry_source(
    tx: &Transaction<'_>,
    continuation: &LineContinuation,
    now: LeaseInstant,
) -> Result<(SessionRef, CheckpointId), SessionStoreError> {
    let source = continuation.source();
    live_session(tx, source.session().session_id())?;
    let row = branch_row(tx, source.session())?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, continuation.lease(), now)?;
    if row.head != source {
        return Err(SessionStoreError::Conflict);
    }
    let checkpoint = source.checkpoint().checkpoint_id();
    let head = checkpoint_row(tx, checkpoint)?.ok_or_else(SessionStoreError::corrupt)?;
    if head.kind != RuntimeCheckpointKind::Quiescent {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CarrySourceNotQuiescent,
        ));
    }
    Ok((source.session(), checkpoint))
}

pub(in super::super) fn load_line(
    c: &Connection,
    line: LineId,
) -> Result<LineRecord, SessionStoreError> {
    let (head, generation, origin) = line_row(c, line)?.ok_or(SessionStoreError::NotFound)?;
    let origin = LineOrigin::decode(origin.as_deref()).ok_or(SessionStoreError::corrupt())?;
    Ok(LineRecord::from_store(line, head, generation, origin))
}

pub(in super::super) fn line_job_terminals(
    c: &Connection,
    line: LineId,
) -> Result<Vec<LineJobTerminal>, SessionStoreError> {
    let Some((_, generation, _)) = line_row(c, line)? else {
        return Err(SessionStoreError::NotFound);
    };
    let mut statement = c
        .prepare(
            "SELECT job,evidence FROM line_job_terminals WHERE line_id=?1 AND generation=?2
             ORDER BY job",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement
        .query(params![id(line), u64b(generation)])
        .map_err(map_sqlite)?;
    let mut terminals = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let job: i64 = row.get(0).map_err(map_sqlite)?;
        let evidence: Vec<u8> = row.get(1).map_err(map_sqlite)?;
        let terminal =
            LineJobTerminal::decode(&evidence).map_err(|_| SessionStoreError::corrupt())?;
        if i64::from(terminal.number) != job {
            return Err(SessionStoreError::corrupt());
        }
        terminals.push(terminal);
    }
    Ok(terminals)
}

/// Deletes consumed line terminals of the head's own line and generation.
pub(super) fn consume_terminals(
    tx: &Transaction<'_>,
    line: LineId,
    generation: u64,
    jobs: &[u32],
) -> Result<(), SessionStoreError> {
    for job in jobs {
        tx.execute(
            "DELETE FROM line_job_terminals WHERE line_id=?1 AND generation=?2 AND job=?3",
            params![id(line), u64b(generation), i64::from(*job)],
        )
        .map_err(map_sqlite)?;
    }
    Ok(())
}

pub(in super::super) fn record_line_job_terminals(
    b: &mut Backend,
    r: &RecordLineJobTerminals,
) -> Result<LineTerminalsReceipt, SessionStoreError> {
    b.require_open()?;
    let encoded = r
        .terminals()
        .iter()
        .map(|terminal| {
            terminal
                .encode()
                .map(|bytes| (terminal.number, bytes))
                .map_err(|_| {
                    SessionStoreError::Invalid(StoreInvalidReason::CheckpointCommitTopology)
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tx = b
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    require_bound_domain(&tx, b.domain)?;
    let now = sample_clock(b.clock.as_ref(), b.domain)?;
    let head = r.lease().session();
    live_session(&tx, head.session_id())?;
    let row = branch_row(&tx, head)?.ok_or(SessionStoreError::NotFound)?;
    require_lease(&row, r.lease(), now)?;
    require_branch_is_head(
        &tx,
        head,
        r.line(),
        r.generation(),
        StoreInvalidReason::CheckpointCommitTopology,
    )?;
    if row.head.checkpoint().checkpoint_id() != r.head() {
        return Err(SessionStoreError::Conflict);
    }
    let mut recorded = Vec::new();
    for (job, bytes) in encoded {
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO line_job_terminals VALUES(?1,?2,?3,?4)",
                params![id(r.line()), u64b(r.generation()), i64::from(job), bytes],
            )
            .map_err(map_sqlite)?;
        if inserted == 1 && !recorded.contains(&job) {
            recorded.push(job);
        }
    }
    recorded.sort_unstable();
    let feed_seq = if recorded.is_empty() {
        None
    } else {
        let facts = recorded
            .iter()
            .map(|job| FeedFact::LineJobTerminalRecorded {
                line: r.line(),
                generation: r.generation(),
                job: *job,
            })
            .collect::<Vec<_>>();
        Some(feed::append(
            &tx,
            head,
            FeedChange::LineJobTerminals { head: r.head() },
            &facts,
        )?)
    };
    #[cfg(test)]
    test_crash_cut(CrashCut::RecordTerminalsBeforeCommit);
    tx.commit().map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut(CrashCut::RecordTerminalsAfterCommit);
    Ok(LineTerminalsReceipt::from_store(recorded, feed_seq))
}
