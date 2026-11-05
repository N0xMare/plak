//! Product intents, feed-derived views and the one projector (spec 02 §2.6).
//!
//! The projector is a pure function of the views it read, the session's
//! intents, a page of feed records and the bounded facade reads those records
//! name. [`run`] is its only driver: it plans and loads the reads, applies the
//! page and returns the writes the store commits under the cursor CAS.

mod intents;
mod join;
mod project;
mod reads;
mod views;

#[cfg(test)]
pub(crate) mod read_fault;

#[cfg(test)]
mod tests;

pub use intents::{
    AnchoredIntent, HostPauseCause, IMPORTED_NOTICE_PREFIX, OperatorNotice, ProductIntents,
    RootContinuation, RootContinuationCause, RootContinuationDecision, RootInterrupt,
    RootInterruptCause, RootLossIntent, RootStopAnchor, Submission, SubmissionKind,
    WorkflowRequest,
};
pub use join::{
    ProductTreeJoin, new_session_completion, pending_command_ids, pending_workflow_operation_ids,
    workflow_withdrawals,
};
pub use project::{
    NOT_RETAINED_ROW, ProjectError, ProjectedAbandonment, ProjectedTurnUsage, Projection,
    Successor, ViewWrites, project, rebuild_behind_retention,
};
pub use reads::{
    CallRead, CallTarget, JobEnd, ProjectorReads, ReadActor, ReadCache, ReadItem, ReadPart,
    ReadRequest, ReadValue, ResultRead, TerminalRead, load_reads, load_reads_sqlite, plan_reads,
    read_item, record_checkpoint,
};
pub(crate) use reads::{TranscriptPart, TranscriptRead, transcript_tail};
pub use views::{
    Anchored, CallView, Counters, Delivery, Lane, LineageStep, OpenTurn, PauseCause, ProcessTrack,
    RowKey, SessionViewState, SessionViews, WorkflowCompletion, WorkflowSettlement,
};

use plakous::sessions::AgentSessions;
use plakous::sessions::feed::FeedRecord;

pub(crate) fn compaction_call_id(workflow_id: &[u8]) -> String {
    format!(
        "compaction-{}",
        plakous::model::hex_lower(&workflow_id[..8.min(workflow_id.len())])
    )
}

/// What a projector transaction did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionCommit {
    /// The writes and the cursor committed.
    Applied,
    /// Another applier moved the cursor first; nothing was written.
    CursorMoved,
}

/// Why a projector run failed.
#[derive(Debug)]
pub enum RunError {
    /// A facade read was refused.
    Read(plakous::sessions::AgentSessionsError),
    /// The page could not be applied.
    Project(ProjectError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "projector read failed: {error}"),
            Self::Project(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Project(error) => Some(error),
        }
    }
}

/// Loads reads only for followed-lineage records and applies the page to `views`.
///
/// # Errors
///
/// Returns a refused read or an unappliable page. The owned views are dropped
/// and the caller reloads the last committed views.
pub async fn run(
    sessions: &AgentSessions,
    views: SessionViews,
    intents: &ProductIntents,
    records: &[FeedRecord],
) -> Result<Projection, RunError> {
    let mut cache = ReadCache::default();
    let mut state = SessionViewState {
        lineage: views.state.lineage.clone(),
        ..SessionViewState::default()
    };
    for record in records
        .iter()
        .filter(|record| record.header().seq > views.cursor)
    {
        if !state.applies_own(record.header()) {
            continue;
        }
        if state.lineage.is_empty() {
            state.lineage.push(LineageStep {
                branch_id: record.header().branch_id,
                from_seq: 0,
            });
        }
        let requests = plan_reads(record);
        let session = record.header().session_id;
        #[cfg(test)]
        let session = read_fault::session_for_read(session, views.cursor, !requests.is_empty());
        load_reads(sessions, session, requests, &mut cache)
            .await
            .map_err(RunError::Read)?;
    }
    // A read the views ask for (a fenced turn's abandonment) is loaded on
    // demand and the page applied again from the same views.
    let mut retry = asks_reads(&views, records).then(|| Box::new(views.clone()));
    let mut views = views;
    loop {
        match project(views, intents, records, &cache) {
            Err(ProjectError::MissingRead(request)) if cache.read(&request).is_none() => {
                let (Some(again), Some(record)) = (retry.take(), records.first()) else {
                    return Err(RunError::Project(ProjectError::MissingRead(request)));
                };
                Box::pin(load_reads(
                    sessions,
                    record.header().session_id,
                    [request],
                    &mut cache,
                ))
                .await
                .map_err(RunError::Read)?;
                retry = Some(again.clone());
                views = *again;
            }
            outcome => return outcome.map_err(RunError::Project),
        }
    }
}

/// Whether applying `records` to `views` may ask for a read no record
/// names: a fenced turn's closure appends history for a turn that is not
/// open, either since before this page or after a terminal in it. Turns the
/// page itself admits are open until a terminal, which this also counts.
#[must_use]
pub fn asks_reads(views: &SessionViews, records: &[FeedRecord]) -> bool {
    use plakous::sessions::feed::FeedFact;
    let mut admitted = std::collections::BTreeSet::new();
    records.iter().any(|record| {
        record.facts().iter().any(|fact| match fact {
            FeedFact::TurnTerminal { .. } => true,
            FeedFact::CommandAccepted {
                kind: plakous::sessions::feed::FeedCommandKind::Root { .. },
                turn_id: Some(turn),
                ..
            } => {
                admitted.insert(*turn);
                false
            }
            FeedFact::HistoryAppended {
                turn_id: Some(turn),
                ..
            } => {
                !admitted.contains(turn) && !views.state.open_turns.contains_key(&turn.to_string())
            }
            _ => false,
        })
    })
}
