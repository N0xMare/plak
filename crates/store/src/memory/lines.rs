//! Agent lines in the memory store, under the same rules as `SQLite`.

use std::collections::BTreeMap;

use plakous_agent::internal::{
    CheckpointId, FeedChange, FeedFact, LineId, LineJobTerminal, LineOrigin, LineRecord, LineStep,
    LineTerminalsReceipt, RecordLineJobTerminals, SessionId, SessionRef, SessionStoreError,
    StoreInvalidReason,
};

use super::{StoreState, branch, memory_feed, require_lease};

#[derive(Clone)]
pub(super) struct MemoryLine {
    head: SessionRef,
    generation: u64,
    origin: LineOrigin,
}

/// Line rows and unconsumed terminals keyed by line, generation and job.
#[derive(Clone, Default)]
pub(super) struct MemoryLines {
    lines: BTreeMap<LineId, MemoryLine>,
    terminals: BTreeMap<(LineId, u64, u32), LineJobTerminal>,
}

/// One validated line change, applied only after every fallible check.
pub(super) enum LineChange {
    Start(LineId, MemoryLine),
    Advance(LineId, SessionRef, u64),
}

/// A branch's fixed line membership, read from its head checkpoint.
pub(super) fn branch_line(
    state: &StoreState,
    s: SessionRef,
) -> Result<(LineId, u64), SessionStoreError> {
    let head = branch(state, s)?.head.checkpoint().checkpoint_id();
    let topology = state
        .checkpoints
        .get(&head)
        .ok_or(SessionStoreError::corrupt())?
        .checkpoint
        .manifest()
        .topology();
    Ok((topology.line, topology.line_generation))
}

pub(super) fn require_head(
    state: &StoreState,
    s: SessionRef,
    line: LineId,
    generation: u64,
) -> Result<(), SessionStoreError> {
    match state.lines.lines.get(&line) {
        Some(row) if row.head == s && row.generation == generation => Ok(()),
        _ => Err(SessionStoreError::NotLineHead),
    }
}

/// Validates a genesis's line step: a new line, or a carry's head advance.
pub(super) fn plan_genesis(
    state: &StoreState,
    request: &plakous_agent::internal::CreateSession,
    topology: plakous_agent::internal::RuntimeTopology,
) -> Result<LineChange, SessionStoreError> {
    plan_step(
        state,
        request.session(),
        topology.line,
        topology.line_generation,
        None,
        request.line_origin(),
        StoreInvalidReason::GenesisCheckpointTopology,
    )
    .map(|(change, _)| change)
}

/// Validates a new branch's line step; returns the change to apply and
/// whether the branch becomes the next head of an existing line.
pub(super) fn plan_step(
    state: &StoreState,
    target: SessionRef,
    line: LineId,
    generation: u64,
    source: Option<(SessionRef, CheckpointId)>,
    origin: &LineOrigin,
    invalid: StoreInvalidReason,
) -> Result<(LineChange, bool), SessionStoreError> {
    match LineStep::of(target.branch_id(), line, generation)
        .ok_or(SessionStoreError::Invalid(invalid))?
    {
        LineStep::NewLine => {
            if state.lines.lines.contains_key(&line) {
                return Err(SessionStoreError::IdentityConflict);
            }
            let origin = match source {
                Some((source, checkpoint)) => LineOrigin::Forked {
                    line: branch_line(state, source)?.0,
                    source,
                    checkpoint,
                },
                None => origin.clone(),
            };
            Ok((
                LineChange::Start(
                    line,
                    MemoryLine {
                        head: target,
                        generation: 1,
                        origin,
                    },
                ),
                false,
            ))
        }
        LineStep::Advance { line, from } => {
            if let Some((source, _)) = source
                && branch_line(state, source)? != (line, from)
            {
                return Err(SessionStoreError::Invalid(invalid));
            }
            let row = state
                .lines
                .lines
                .get(&line)
                .ok_or(SessionStoreError::NotLineHead)?;
            if row.generation != from || source.is_some_and(|(source, _)| source != row.head) {
                return Err(SessionStoreError::NotLineHead);
            }
            if state
                .lines
                .terminals
                .range((line, 0, 0)..=(line, u64::MAX, u32::MAX))
                .next()
                .is_some()
            {
                return Err(SessionStoreError::Conflict);
            }
            Ok((LineChange::Advance(line, target, generation), true))
        }
    }
}

pub(super) fn apply(state: &mut StoreState, change: LineChange) {
    match change {
        LineChange::Start(line, row) => {
            state.lines.lines.insert(line, row);
        }
        LineChange::Advance(line, head, generation) => {
            if let Some(row) = state.lines.lines.get_mut(&line) {
                row.head = head;
                row.generation = generation;
            }
        }
    }
}

pub(super) fn consume(state: &mut StoreState, line: LineId, generation: u64, jobs: &[u32]) {
    for job in jobs {
        state.lines.terminals.remove(&(line, generation, *job));
    }
}

pub(super) fn delete_session(state: &mut StoreState, session_id: SessionId) {
    let gone = state
        .lines
        .lines
        .iter()
        .filter(|(_, row)| row.head.session_id() == session_id)
        .map(|(line, _)| *line)
        .collect::<Vec<_>>();
    for line in gone {
        state.lines.lines.remove(&line);
        state
            .lines
            .terminals
            .retain(|(terminal_line, _, _), _| *terminal_line != line);
    }
}

pub(super) fn load(state: &StoreState, line: LineId) -> Result<LineRecord, SessionStoreError> {
    let row = state
        .lines
        .lines
        .get(&line)
        .ok_or(SessionStoreError::NotFound)?;
    Ok(LineRecord::from_store(
        line,
        row.head,
        row.generation,
        row.origin.clone(),
    ))
}

pub(super) fn terminals(
    state: &StoreState,
    line: LineId,
) -> Result<Vec<LineJobTerminal>, SessionStoreError> {
    let generation = state
        .lines
        .lines
        .get(&line)
        .ok_or(SessionStoreError::NotFound)?
        .generation;
    Ok(state
        .lines
        .terminals
        .range((line, generation, 0)..=(line, generation, u32::MAX))
        .map(|(_, terminal)| terminal.clone())
        .collect())
}

pub(super) fn record(
    state: &mut StoreState,
    request: &RecordLineJobTerminals,
    now: plakous_agent::internal::LeaseInstant,
) -> Result<LineTerminalsReceipt, SessionStoreError> {
    let head = request.lease().session();
    let session = state
        .sessions
        .get(&head.session_id())
        .ok_or(SessionStoreError::NotFound)?;
    if session.head.is_archived() {
        return Err(SessionStoreError::Archived);
    }
    let row = branch(state, head)?;
    require_lease(row, request.lease(), now)?;
    if branch_line(state, head)? != (request.line(), request.generation()) {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointCommitTopology,
        ));
    }
    require_head(state, head, request.line(), request.generation())?;
    if row.head.checkpoint().checkpoint_id() != request.head() {
        return Err(SessionStoreError::Conflict);
    }
    let mut recorded = request
        .terminals()
        .iter()
        .filter(|terminal| {
            !state.lines.terminals.contains_key(&(
                request.line(),
                request.generation(),
                terminal.number,
            ))
        })
        .map(|terminal| terminal.number)
        .collect::<Vec<_>>();
    recorded.sort_unstable();
    recorded.dedup();
    if recorded.is_empty() {
        return Ok(LineTerminalsReceipt::from_store(recorded, None));
    }
    let facts = recorded
        .iter()
        .map(|job| FeedFact::LineJobTerminalRecorded {
            line: request.line(),
            generation: request.generation(),
            job: *job,
        })
        .collect::<Vec<_>>();
    let record = memory_feed::prepare(
        &state.feeds,
        head.session_id(),
        head.branch_id(),
        FeedChange::LineJobTerminals {
            head: request.head(),
        },
        &facts,
    )?;
    let seq = record.seq();
    memory_feed::append(&mut state.feeds, record);
    for terminal in request.terminals() {
        state
            .lines
            .terminals
            .entry((request.line(), request.generation(), terminal.number))
            .or_insert_with(|| terminal.clone());
    }
    Ok(LineTerminalsReceipt::from_store(recorded, Some(seq)))
}
