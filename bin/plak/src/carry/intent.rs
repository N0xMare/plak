//! Write-ahead carries (spec 02 §2.5.1, §2.6): finishing, replaying or
//! withdrawing a same-store carry a crash interrupted, and following a
//! superseded session to the successor its line moved to.
//!
//! A same-store carry saves its successor's record before the runtime create,
//! with no locator and a `CarryIntent` naming its source. Opening either
//! record, and every startup in its workspace, settles that intent first, and
//! only while holding the source branch's lease, which is the carry's own
//! fence: a live carry holds it, so its intent is pending, not settled. Under
//! that lease a create its operation already published binds the record; an
//! unpublished one re-runs from a fresh export of a source that is still its
//! line's head, under the same record and operation; anything else withdraws
//! the intent and leaves the source live. A source whose record or runtime is
//! gone withdraws an unpublished intent too.
//!
//! Carries are their own write-ahead record here. When live switching lands
//! they may become a kind of product workflow intent instead.

use crate::store::sessions::SessionRecord;
use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, BranchHold, SessionLocator, SessionOperationId,
};
use plakous_hosted::error::{CliFailure, CliResult};

/// How many carried successors an open follows before it stops.
const MAX_FOLLOWED: usize = 8;

/// The outcome of settling one interrupted carry.
enum Settled {
    /// The open lands on this record.
    Landed(SessionRecord, String),
    /// A live owner holds the source: the carry is still running or its
    /// crashed owner's lease has not lapsed.
    Pending(String),
    /// The intent was withdrawn; the open lands on its source, when one is
    /// left.
    Withdrawn(Option<SessionRecord>, String),
}

/// The record a headless face (`plak run --session`, `plak agent
/// --session`) works on: the requested record after its workspace's
/// interrupted carries are settled and a superseded session is followed to
/// its successor. The note goes to `stderr`.
///
/// # Errors
/// As [`resume`].
pub(crate) fn load_for_work(
    services: &crate::shell::services::Services,
    record: SessionRecord,
    stderr: &mut dyn std::io::Write,
) -> CliResult<SessionRecord> {
    let workspace = record.workspace_root.clone();
    let mut note = None;
    let record = resume(services, Some(&workspace), record, &mut note)?;
    if let Some(note) = note {
        let _ = stderr.write_all(format!("{note}\n").as_bytes());
    }
    Ok(record)
}

/// The session id `plak run --session <id>` works on, through
/// [`load_for_work`]; an id the store cannot load is returned as given, for
/// the run's own lookup to refuse.
///
/// # Errors
/// As [`load_for_work`].
pub(crate) fn work_id(
    services: &crate::shell::services::Services,
    id: &str,
    stderr: &mut dyn std::io::Write,
) -> CliResult<String> {
    let Ok(record) = services
        .session_records()
        .and_then(|store| store.load(id).map_err(CliFailure::from))
    else {
        return Ok(id.to_owned());
    };
    load_for_work(services, record, stderr).map(|record| record.id)
}

/// The record a resume of `record` opens, after the startup settlement of
/// `workspace`'s interrupted carries; the notes join `note`.
///
/// # Errors
/// As [`resolve_open`], and the build's refusal of the landed session.
pub(crate) fn resume(
    services: &crate::shell::services::Services,
    workspace: Option<&std::path::Path>,
    record: SessionRecord,
    note: &mut Option<String>,
) -> CliResult<SessionRecord> {
    let mut notes = note.take().into_iter().collect::<Vec<_>>();
    if let Some(workspace) = workspace {
        notes.extend(settle_workspace(services, workspace));
    }
    let (record, landed) = resolve_open(services, record)?;
    notes.extend(landed);
    *note = (!notes.is_empty()).then(|| notes.join("; "));
    crate::build_features::admit_session(&record)?;
    Ok(record)
}

/// Where an open of `record` lands, and what it says about the move: the
/// record itself; the successor a carry moved its line to; the finished
/// successor of an interrupted carry; or, for a withdrawn intent, its source.
///
/// # Errors
/// Returns a store, runtime or carry failure; a carry another process still
/// owns answers that it is pending, naming the lease period to wait.
pub(crate) fn resolve_open(
    services: &crate::shell::services::Services,
    record: SessionRecord,
) -> CliResult<(SessionRecord, Option<String>)> {
    let mut record = record;
    let mut notes = Vec::new();
    for _ in 0..MAX_FOLLOWED {
        if record.carry_intent.is_some() && record.plakous_locator.is_none() {
            match settle(services, record)? {
                Settled::Landed(next, note) | Settled::Withdrawn(Some(next), note) => {
                    record = next;
                    notes.push(note);
                    continue;
                }
                Settled::Pending(note) | Settled::Withdrawn(None, note) => {
                    return Err(CliFailure::new(note));
                }
            }
        }
        let Some(locator) = record.plakous_locator else {
            break;
        };
        if !superseded(services, locator)? {
            break;
        }
        let store = services.session_records()?;
        let Some(next) = super::already_carried(
            store.as_ref(),
            &record.workspace_root,
            &record.id,
            &crate::home::state::state_key(),
        )
        .map_err(CliFailure::new)?
        else {
            break;
        };
        notes.push(format!(
            "session {} was carried forward as {}; resuming it",
            record.id, next.id
        ));
        record = next;
    }
    Ok((record, (!notes.is_empty()).then(|| notes.join("; "))))
}

/// The carried successor of `source` that is still an interrupted carry,
/// if any: deleting the source would strand it.
///
/// # Errors
/// Returns the store's failure.
pub(crate) fn pending_successor(
    services: &crate::shell::services::Services,
    source: &SessionRecord,
) -> CliResult<Option<SessionRecord>> {
    let store = services.session_records()?;
    Ok(super::already_carried(
        store.as_ref(),
        &source.workspace_root,
        &source.id,
        &crate::home::state::state_key(),
    )
    .map_err(CliFailure::new)?
    .filter(|successor| successor.carry_intent.is_some() && successor.plakous_locator.is_none()))
}

/// Settles every interrupted carry in `workspace` at startup, so a reload
/// carry that never relaunched is not left pending. Returns their notes.
pub(crate) fn settle_workspace(
    services: &crate::shell::services::Services,
    workspace: &std::path::Path,
) -> Vec<String> {
    let Ok(store) = services.session_records() else {
        return Vec::new();
    };
    let Ok(records) = store.list_for_workspace(workspace) else {
        return Vec::new();
    };
    records
        .into_iter()
        .filter(|record| record.carry_intent.is_some() && record.plakous_locator.is_none())
        .filter_map(|record| match store.load(&record.id) {
            Ok(record) => Some(match settle(services, record) {
                Ok(
                    Settled::Landed(_, note) | Settled::Pending(note) | Settled::Withdrawn(_, note),
                ) => note,
                Err(error) => format!("an interrupted carry is still pending: {}", error.message()),
            }),
            Err(_) => None,
        })
        .collect()
}

fn settle(
    services: &crate::shell::services::Services,
    record: SessionRecord,
) -> CliResult<Settled> {
    settle_with(services, record, &|| {})
}

/// Settles one interrupted carry under its source branch's lease.
/// `after_head` runs between seeing that the source's line moved and the
/// final published check; a test races a create in there.
fn settle_with(
    services: &crate::shell::services::Services,
    record: SessionRecord,
    after_head: &dyn Fn(),
) -> CliResult<Settled> {
    let intent = record
        .carry_intent
        .clone()
        .ok_or_else(|| CliFailure::new("record has no carry intent"))?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("application executor failed"))?;
    let operation = SessionOperationId::new(&record.create_operation_id)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let genesis = AgentSessions::genesis_locator_for(&operation)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if published(&executor, &sessions, genesis)? {
        return bind(services, &executor, &sessions, record, genesis);
    }
    let store = services.session_records()?;
    let source = record
        .carried_forward_from()
        .and_then(|(source_id, _)| store.load(source_id).ok());
    let hold_operation = SessionOperationId::new(format!("plak:carry-settle:{}", record.id))
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let hold = match executor
        .block_on(sessions.hold_branch(&hold_operation, intent.source))
        .map_err(|_| CliFailure::new("application execution failed"))?
    {
        Ok(hold) => Some(hold),
        Err(error) if error.kind() == AgentSessionsErrorKind::Ownership => {
            return Ok(Settled::Pending(format!(
                "the carry into session {} is pending: another plak holds its source; retry in {} s",
                record.id,
                sessions.lease_period_seconds()
            )));
        }
        // The source runtime is archived or deleted: only a published
        // create could still be this carry's successor.
        Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => None,
        Err(error) => return Err(CliFailure::new(error.to_string())),
    };
    let release = |hold: Option<BranchHold>| {
        if let Some(hold) = hold {
            let _ = executor.block_on(hold.release());
        }
    };
    // Under the source's lease no carry can commit, so these reads are final.
    if published(&executor, &sessions, genesis)? {
        release(hold);
        return bind(services, &executor, &sessions, record, genesis);
    }
    if let (Some(_), Some(source)) = (&hold, &source)
        && is_head(&executor, &sessions, intent.source)?
    {
        // The re-run takes the source's lease itself.
        release(hold);
        let (next, note) = super::rebind::resume_carry(
            services,
            source,
            &intent.cause,
            intent.orchestration,
            record,
        )?;
        return Ok(Settled::Landed(
            next,
            format!("finished the interrupted carry: {note}"),
        ));
    }
    after_head();
    if published(&executor, &sessions, genesis)? {
        release(hold);
        return bind(services, &executor, &sessions, record, genesis);
    }
    let reason = match (&source, &hold) {
        (None, _) => "its source session no longer exists",
        (Some(_), None) => "its source's runtime is archived or deleted",
        (Some(_), Some(_)) => "its source is no longer its line's head",
    };
    let withdrawn = withdraw(services, &record);
    release(hold);
    withdrawn?;
    let note = match &source {
        Some(source) => format!(
            "withdrew the interrupted carry {}: {reason}; resuming its source {}",
            record.id, source.id
        ),
        None => format!("withdrew the interrupted carry {}: {reason}", record.id),
    };
    Ok(Settled::Withdrawn(source, note))
}

/// Binds an intent record to the genesis its create published.
fn bind(
    services: &crate::shell::services::Services,
    executor: &plakous::application::ApplicationExecutor,
    sessions: &std::sync::Arc<AgentSessions>,
    mut record: SessionRecord,
    genesis: SessionLocator,
) -> CliResult<Settled> {
    let store = services.session_records()?;
    record.carry_intent = None;
    record
        .bind_plakous_locator(genesis)
        .map_err(CliFailure::new)?;
    store.save(&mut record).map_err(CliFailure::new)?;
    executor
        .block_on(plakous_hosted::runtime::project_session(
            sessions, &store, &record,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(CliFailure::new)?;
    let note = format!("finished the interrupted carry into session {}", record.id);
    Ok(Settled::Landed(record, note))
}

/// Deletes an intent record; its create identity stays bound, so a replay of
/// the withdrawn carry is refused.
fn withdraw(services: &crate::shell::services::Services, record: &SessionRecord) -> CliResult<()> {
    let mut product = crate::store::ProductStore::open(services.paths()?)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    product
        .delete_session(&record.id, record.store_revision)
        .map_err(CliFailure::new)
}

/// Whether the create of `genesis`'s operation committed.
fn published(
    executor: &plakous::application::ApplicationExecutor,
    sessions: &AgentSessions,
    genesis: SessionLocator,
) -> CliResult<bool> {
    let page = executor
        .block_on(sessions.feed_page(FeedPageRequest::after(FeedCursor::start(
            genesis.session_id(),
        ))))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    match page {
        Ok(FeedPage::Records { records, .. }) => Ok(!records.is_empty()),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => Ok(false),
        Err(error) => Err(CliFailure::new(error.to_string())),
    }
}

/// Whether `locator`'s branch is still its line's head.
fn is_head(
    executor: &plakous::application::ApplicationExecutor,
    sessions: &AgentSessions,
    locator: SessionLocator,
) -> CliResult<bool> {
    let head = executor
        .block_on(sessions.line_head_of(locator))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    Ok(head.is_ok_and(|head| {
        head.session_id() == locator.session_id() && head.branch_id() == locator.branch_id()
    }))
}

/// Whether the line of `locator`'s branch moved to another branch.
fn superseded(
    services: &crate::shell::services::Services,
    locator: SessionLocator,
) -> CliResult<bool> {
    let sessions = services.state_roots()?.runtime_sessions()?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("application executor failed"))?;
    let head = executor
        .block_on(sessions.line_head_of(locator))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    Ok(head.is_ok_and(|head| {
        head.session_id() != locator.session_id() || head.branch_id() != locator.branch_id()
    }))
}

#[cfg(test)]
#[path = "intent/tests.rs"]
mod tests;
