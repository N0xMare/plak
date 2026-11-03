//! Write-ahead carries (spec 02 §2.5.1, §2.6): finishing, replaying or
//! withdrawing a same-store carry a crash interrupted, and following a
//! superseded session to the successor its line moved to.
//!
//! A same-store carry saves its successor's record before the runtime create,
//! with no locator and a `CarryIntent` naming its source. Opening either
//! record, and every startup in its workspace, settles that intent first:
//! a create its operation already published binds the record; an unpublished
//! one re-runs from a fresh export of a source that is still its line's head,
//! under the same record and operation; anything else withdraws the intent
//! and leaves the source live.

use crate::store::sessions::SessionRecord;
use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
use plakous::sessions::{AgentSessions, SessionLocator, SessionOperationId};
use plakous_hosted::error::{CliFailure, CliResult};

/// How many carried successors an open follows before it stops.
const MAX_FOLLOWED: usize = 8;

/// Where an open of `record` lands, and what it says about the move: the
/// record itself; the successor a carry moved its line to; the finished
/// successor of an interrupted carry; or, for a withdrawn intent, its source.
///
/// # Errors
/// Returns a store, runtime or carry failure; nothing is guessed.
pub(crate) fn resolve_open(
    services: &crate::shell::services::Services,
    record: SessionRecord,
) -> CliResult<(SessionRecord, Option<String>)> {
    let mut record = record;
    let mut notes = Vec::new();
    for _ in 0..MAX_FOLLOWED {
        if record.carry_intent.is_some() && record.plakous_locator.is_none() {
            let (next, note) = settle(services, record)?;
            record = next;
            notes.push(note);
            continue;
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

/// The record an in-session switch to `id` opens.
///
/// # Errors
/// As [`resume`].
pub(crate) fn switch_to(
    services: &crate::shell::services::Services,
    store: &dyn crate::store::session_records::SessionRecordStore,
    id: &str,
    note: &mut Option<String>,
) -> CliResult<SessionRecord> {
    let record = store.load(id).map_err(CliFailure::from)?;
    resume(services, None, record, note)
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
                Ok((_, note)) => note,
                Err(error) => format!("an interrupted carry is still pending: {}", error.message()),
            }),
            Err(_) => None,
        })
        .collect()
}

/// Settles one interrupted carry: returns the record an open lands on and
/// the note that says what happened.
fn settle(
    services: &crate::shell::services::Services,
    mut record: SessionRecord,
) -> CliResult<(SessionRecord, String)> {
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
    let store = services.session_records()?;
    if published(&executor, &sessions, genesis)? {
        record.carry_intent = None;
        record
            .bind_plakous_locator(genesis)
            .map_err(CliFailure::new)?;
        store.save(&mut record).map_err(CliFailure::new)?;
        executor
            .block_on(plakous_hosted::runtime::project_session(
                &sessions, &store, &record,
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        let note = format!("finished the interrupted carry into session {}", record.id);
        return Ok((record, note));
    }
    let (source_id, _) = record
        .carried_forward_from()
        .ok_or_else(|| CliFailure::new("a carry intent without its source"))?;
    let source = store.load(source_id).map_err(CliFailure::new)?;
    let head = executor
        .block_on(sessions.line_head_of(intent.source))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    if head.is_ok_and(|head| {
        head.session_id() == intent.source.session_id()
            && head.branch_id() == intent.source.branch_id()
    }) {
        let (next, note) = super::rebind::resume_carry(
            services,
            &source,
            &intent.cause,
            intent.orchestration,
            record,
        )?;
        return Ok((next, format!("finished the interrupted carry: {note}")));
    }
    let mut product = crate::store::ProductStore::open(services.paths()?)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    product
        .delete_session(&record.id, record.store_revision)
        .map_err(CliFailure::new)?;
    let note = format!(
        "withdrew the interrupted carry {}: its source {} is no longer its line's head; resuming the source",
        record.id, source.id
    );
    Ok((source, note))
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
        Err(error) if error.kind() == plakous::sessions::AgentSessionsErrorKind::NotFound => {
            Ok(false)
        }
        Err(error) => Err(CliFailure::new(error.to_string())),
    }
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
