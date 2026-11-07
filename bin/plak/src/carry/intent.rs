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
    if record.archived {
        return Err(CliFailure::new(
            "session_archived: unarchive this session before settling a carry",
        ));
    }
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
    // A build that cannot run the session refuses before following or settling
    // anything, so the refusal opens no store for writing.
    crate::build_features::admit_session(&record)?;
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
    if record.archived {
        return Err(CliFailure::new(
            "session_archived: unarchive this session before settling a carry",
        ));
    }
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
        if record.archived {
            return Err(CliFailure::new(
                "session_archived: unarchive this session before settling a carry",
            ));
        }
        if record.carry_intent.is_some() && record.plakous_locator.is_none() {
            record = current_record(services, &record)?;
        }
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
        if let Some(next) = super::already_carried(
            services,
            &record.workspace_root,
            record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str()),
            &record.id,
            &crate::home::state::state_key(),
        )
        .map_err(CliFailure::new)?
        {
            notes.push(format!(
                "session {} was carried forward as {}; resuming it",
                record.id, next.id
            ));
            record = next;
            continue;
        }
        let Some(next) = switch_successor(services, &record)? else {
            break;
        };
        notes.push(format!(
            "session {} switched profile and continues as {}; resuming it",
            record.id, next.id
        ));
        record = next;
    }
    Ok((record, (!notes.is_empty()).then(|| notes.join("; "))))
}

/// The store's current copy of an intent record the startup settlement may
/// already have bound, or, when it withdrew the intent, the intent's source.
///
/// # Errors
/// Returns the store's failure, or a refusal when a withdrawn intent's source
/// is gone too.
fn current_record(
    services: &crate::shell::services::Services,
    record: &SessionRecord,
) -> CliResult<SessionRecord> {
    let store = services.session_records()?;
    let error = match store.load(&record.id) {
        Ok(current) => return Ok(current),
        Err(error) => error,
    };
    let listed = store
        .list_for_workspace(&record.workspace_root)
        .map_err(CliFailure::new)?;
    if listed.iter().any(|listed| listed.id == record.id) {
        return Err(CliFailure::new(error));
    }
    record
        .carried_forward_from()
        .and_then(|(source_id, _)| store.load(source_id).ok())
        .ok_or_else(|| {
            CliFailure::new(format!(
                "the interrupted carry {} was withdrawn and its source no longer exists",
                record.id
            ))
        })
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
    Ok(super::already_carried(
        services,
        &source.workspace_root,
        source
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
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
        .filter(|record| {
            !record.archived && record.carry_intent.is_some() && record.plakous_locator.is_none()
        })
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
    settle_with(services, record, &|| Ok(()))
}

/// The settlement's hold on the source branch: released on every exit, so
/// an early refusal never leaves the source leased for a lease period.
struct SourceHold<'a> {
    executor: &'a plakous::application::ApplicationExecutor,
    hold: Option<BranchHold>,
}

impl SourceHold<'_> {
    const fn is_held(&self) -> bool {
        self.hold.is_some()
    }

    fn release(&mut self) {
        if let Some(hold) = self.hold.take() {
            // A release that fails leaves a lease that lapses on its own.
            let _ = self.executor.block_on(hold.release());
        }
    }
}

impl Drop for SourceHold<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

fn pending(record: &SessionRecord, sessions: &AgentSessions) -> Settled {
    Settled::Pending(format!(
        "the carry into session {} is pending: another plak holds its source; retry in {} s",
        record.id,
        sessions.lease_period_seconds()
    ))
}

/// Settles one interrupted carry under its source branch's lease.
/// `after_head` runs between seeing that the source's line moved and the
/// final published check; a test races a create, or a failure, in there.
fn settle_with(
    services: &crate::shell::services::Services,
    record: SessionRecord,
    after_head: &dyn Fn() -> CliResult<()>,
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
        return bind(services, &executor, &sessions, record, &intent, genesis);
    }
    let store = services.session_records()?;
    let source = record
        .carried_forward_from()
        .and_then(|(source_id, _)| store.load(source_id).ok());
    let hold_operation = SessionOperationId::new(format!("plak:carry-settle:{}", record.id))
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut hold = SourceHold {
        executor: &executor,
        hold: match executor
            .block_on(sessions.hold_branch(&hold_operation, intent.source))
            .map_err(|_| CliFailure::new("application execution failed"))?
        {
            Ok(hold) => Some(hold),
            Err(error) if error.kind() == AgentSessionsErrorKind::Ownership => {
                return Ok(pending(&record, &sessions));
            }
            // The source runtime is archived or deleted: only a published
            // create could still be this carry's successor.
            Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => None,
            Err(error) => return Err(CliFailure::new(error.to_string())),
        },
    };
    // Under the source's lease no carry can commit, so these reads are final.
    if published(&executor, &sessions, genesis)? {
        hold.release();
        return bind(services, &executor, &sessions, record, &intent, genesis);
    }
    if let (true, Some(source)) = (hold.is_held(), &source)
        && is_head(&executor, &sessions, intent.source)?
    {
        // The re-run takes the source's lease itself; another owner that
        // takes it first leaves the carry pending.
        hold.release();
        let busy = pending(&record, &sessions);
        return match super::rebind::resume_carry(
            services,
            source,
            &intent.cause,
            intent.orchestration,
            record,
        ) {
            Ok((next, note)) => Ok(Settled::Landed(
                next,
                format!("finished the interrupted carry: {note}"),
            )),
            // The create path names its refusal's kind.
            Err(error) if error.message().contains("(Ownership)") => Ok(busy),
            Err(error) => Err(error),
        };
    }
    after_head()?;
    if published(&executor, &sessions, genesis)? {
        hold.release();
        return bind(services, &executor, &sessions, record, &intent, genesis);
    }
    let reason = match (&source, hold.is_held()) {
        (None, _) => "its source session no longer exists",
        (Some(_), false) => "its source's runtime is archived or deleted",
        (Some(_), true) => "its source is no longer its line's head",
    };
    withdraw(services, &record)?;
    drop(hold);
    let note = match &source {
        Some(source) => format!(
            "withdrew the interrupted carry {}: {reason}; resuming its source {}",
            record.id, source.id
        ),
        None => format!("withdrew the interrupted carry {}: {reason}", record.id),
    };
    Ok(Settled::Withdrawn(source, note))
}

/// Binds an intent record to the genesis its create published, once that
/// genesis is a carry of the intent's source: the next head of the source's
/// line, or a new line whose provenance names the source. Any other session
/// under the carry's operation is an integrity failure, never a finished
/// carry.
fn bind(
    services: &crate::shell::services::Services,
    executor: &plakous::application::ApplicationExecutor,
    sessions: &std::sync::Arc<AgentSessions>,
    mut record: SessionRecord,
    intent: &plakous_hosted::sessions::CarryIntent,
    genesis: SessionLocator,
) -> CliResult<Settled> {
    if !carries_source(executor, sessions, intent.source, genesis)? {
        return Err(CliFailure::new(format!(
            "session {}'s create published a session that is not a carry of its source {}; the record is left as it is",
            record.id, intent.source
        )));
    }
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

/// Whether `genesis` continues `source`'s line, or starts a line carried
/// from `source` because it was already superseded.
fn carries_source(
    executor: &plakous::application::ApplicationExecutor,
    sessions: &AgentSessions,
    source: SessionLocator,
    genesis: SessionLocator,
) -> CliResult<bool> {
    let read = |locator| {
        executor
            .block_on(sessions.line_head_of(locator))
            .map_err(|_| CliFailure::new("application execution failed"))
    };
    let Ok(target) = read(genesis)? else {
        return Ok(false);
    };
    if read(source)?.is_ok_and(|head| head.line() == target.line()) {
        return Ok(true);
    }
    let provenance = executor
        .block_on(sessions.line_provenance(target.line()))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    Ok(provenance
        .ok()
        .flatten()
        .is_some_and(|text| text == plakous::sessions::carried_from_provenance(source)))
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
/// The session a profile switch of `record` published as its line's next
/// head, once the source's projection has created it.
fn switch_successor(
    services: &crate::shell::services::Services,
    record: &SessionRecord,
) -> CliResult<Option<SessionRecord>> {
    use plakous_hosted::sessions::SessionWorkflowKind;
    use plakous_hosted::sessions::projector::WorkflowSettlement;
    let store = services.session_records()?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("application executor failed"))?;
    executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &store, record,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(CliFailure::new)?;
    let intents = store.load_intents(&record.id).map_err(CliFailure::new)?;
    let Some(views) = store.load_views(&record.id).map_err(CliFailure::new)? else {
        return Ok(None);
    };
    let published = intents.workflow_requests.iter().rev().find(|request| {
        matches!(request.kind, SessionWorkflowKind::Switch(_))
            && views
                .state
                .workflows
                .get(&request.operation_id)
                .is_some_and(|workflow| {
                    matches!(workflow.settled, Some(WorkflowSettlement::Published { .. }))
                })
    });
    let Some(request) = published else {
        return Ok(None);
    };
    match store.load(&request.successor_product_id()) {
        Ok(next) => Ok(Some(next)),
        Err(error) if error == "session not found" => Ok(None),
        Err(error) => Err(CliFailure::new(error)),
    }
}

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
