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
mod tests;

pub use intents::{
    AnchoredIntent, HostPauseCause, OperatorNotice, ProductIntents, Submission, SubmissionKind,
    WorkflowRequest,
};
pub use join::{
    ProductTreeJoin, clear_completion, pending_command_ids, pending_workflow_operation_ids,
    workflow_withdrawals,
};
pub use project::{
    NOT_RETAINED_ROW, ProjectError, ProjectedTurnUsage, Projection, Successor, ViewWrites, project,
    rebuild_behind_retention,
};
pub use reads::{
    CallRead, CallTarget, JobEnd, ProjectorReads, ReadActor, ReadCache, ReadItem, ReadPart,
    ReadRequest, ReadValue, ResultRead, TerminalRead, load_reads, load_reads_sqlite, plan_reads,
    read_item, record_checkpoint,
};
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
/// Returns a refused read or an unappliable page; nothing is applied.
pub async fn run(
    sessions: &AgentSessions,
    views: &SessionViews,
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
        load_reads(
            sessions,
            record.header().session_id,
            plan_reads(record),
            &mut cache,
        )
        .await
        .map_err(RunError::Read)?;
    }
    project(views, intents, records, &cache).map_err(RunError::Project)
}
