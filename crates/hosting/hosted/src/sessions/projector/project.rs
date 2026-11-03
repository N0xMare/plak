//! The one pure projector (spec 02 §2.6). It applies feed records to a
//! session's views, reading only the facade reads a record names, and reads
//! no clock, configuration, environment or event: the same records over the
//! same intents produce the same views, page by page or all at once.

use std::collections::BTreeMap;

use plakous::sessions::feed::{
    FeedBranchTransition, FeedCallerLineage, FeedCancelCause, FeedChange, FeedCommandKind,
    FeedFact, FeedHeadKind, FeedLocator, FeedOperationKind, FeedOperationState, FeedRecord,
    FeedSeq, FeedTurnOutcome, FeedWorkflowKind, FeedWorkflowOutcome,
};
use plakous_host_protocol::views::presentation::{ActivityKind, ActivityState, row_bytes};
use plakous_host_protocol::views::transcript::{ProcessNotice, ProcessOutcome};

use super::intents::ProductIntents;
use super::reads::{
    CallRead, CallTarget, ProjectorReads, ReadActor, ReadItem, ReadPart, ReadRequest, ReadValue,
    ResultRead, TerminalRead, derives_rows, item_reads, operation_read, record_checkpoint,
};
use super::views::{
    Anchored, CallView, Delivery, Lane, LineageStep, OpenTurn, PauseCause, ProcessTrack, RowKey,
    SessionViews, WorkflowCompletion, WorkflowSettlement,
};
use super::{compaction_call_id, intents};
use crate::runtime::events::{
    abandoned_turn_operator_detail, abandoned_turn_operator_line, compact_terminal_subject,
    completed_tool_state, compose_counts_subject, host_owns_subject, projected_tool_subject,
};
use crate::runtime::{Derived, item_rows, projected_tool_row as tool_row, settle_tool_row};
use crate::sessions::{
    MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES, NoticeScope, ProductCompactReason, ToolItem,
    TranscriptRow, bound_row,
};

mod commands;
mod effects;
mod tools;
mod turns;
mod workflows;
mod writes;

pub use writes::ViewWrites;
use writes::{bound_transcript, changed_entries, counted_notices, put_row};

/// The first ordinal of a model round's prose, keyed at the record of the
/// round's provider decision: after any row that record derives itself.
const ROUND_PROSE_ORDINAL: u32 = 1 << 31;

/// The row a pruned read derives instead of the items a compaction removed.
pub const NOT_RETAINED_ROW: &str = "earlier output not retained";

/// Why a projector run could not apply its records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectError {
    /// A read the record names was not performed; run [`super::plan_reads`]
    /// and load it first. A read the views ask for (an abandonment) is
    /// reported here too: load it and apply the page again.
    MissingRead(ReadRequest),
    /// A record of another runtime session.
    ForeignSession,
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRead(request) => {
                write!(formatter, "projector read not loaded: {request:?}")
            }
            Self::ForeignSession => formatter.write_str("feed record of another runtime session"),
        }
    }
}

impl std::error::Error for ProjectError {}

/// A fork or rebind successor published by a `Branched` record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Successor {
    /// The fork or rebind request's operation id, from the record's
    /// `WorkflowSettled`.
    pub operation_id: Option<String>,
    /// Fork or rebind.
    pub transition: FeedBranchTransition,
    /// The published branch and checkpoint.
    pub target: FeedLocator,
    /// The sequence anchoring inherited notices.
    pub published_seq: FeedSeq,
    /// The branch commit's timestamp, not the projector's clock.
    pub created_at_unix: u64,
    /// Exact target views at the branch record, for atomic publication.
    pub seed: Option<SessionViews>,
}

/// One projector run's result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    /// The views after the run.
    pub views: SessionViews,
    /// The writes that turn the stored views into them.
    pub writes: ViewWrites,
    /// Terminal usage observations, emitted only after this run wins its CAS.
    pub terminal_usages: Vec<ProjectedTurnUsage>,
    /// Fenced turns whose abandonment this run applied, emitted only after
    /// it wins its CAS.
    pub abandonments: Vec<ProjectedAbandonment>,
}

/// The abandonment of a fenced root turn, from its terminal record: the
/// commit that closed it carries no fact of its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedAbandonment {
    /// Runtime session named by the closing record.
    pub session_id: plakous::model::SessionId,
    /// The fenced turn.
    pub turn_id: plakous::model::TurnId,
    /// What the closure abandoned.
    pub report: plakous::agent::AbandonedTurnReport,
}

/// A terminal's exact usage and stable feed identity, independent of live results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectedTurnUsage {
    /// Runtime session named by the terminal's feed record.
    pub session_id: plakous::model::SessionId,
    /// Branch on which the terminal committed.
    pub branch_id: plakous::model::BranchId,
    /// Stable telemetry key, including for rebuilt and recovered terminals.
    pub turn_id: plakous::model::TurnId,
    /// Terminal commit time, never the applying process's clock.
    pub committed_at_ms: u64,
    /// Exact terminal ledger aggregate, including partial reported usage.
    pub usage: plakous::agent::TurnUsage,
}

/// Applies `records` to `views`. A record at or below the cursor is skipped,
/// so a replayed page changes nothing.
///
/// # Errors
///
/// Refuses a record of another runtime session, or a record whose read was
/// not loaded into `reads`. On error the owned views are dropped; retry from
/// the last committed views, not a partially applied page.
pub fn project(
    views: SessionViews,
    intents: &ProductIntents,
    records: &[FeedRecord],
    reads: &dyn ProjectorReads,
) -> Result<Projection, ProjectError> {
    let expected_cursor = views.cursor;
    let calls = views.state.calls.clone();
    let open_turns = views.state.open_turns.clone();
    let processes = views.state.processes.clone();
    let mut next = views;
    let mut touched = BTreeMap::new();
    let mut successors = Vec::new();
    let mut terminal_usages = Vec::new();
    let mut abandonments = Vec::new();
    for record in records {
        let header = record.header();
        if header.seq <= next.cursor {
            continue;
        }
        match next.state.runtime_session_id {
            Some(session) if session != header.session_id => {
                return Err(ProjectError::ForeignSession);
            }
            Some(_) => {}
            None => next.state.runtime_session_id = Some(header.session_id),
        }
        let mut applier = Applier {
            views: &mut next,
            intents,
            reads,
            record,
            ordinal: 0,
            touched: &mut touched,
            terminal_usages: &mut terminal_usages,
            abandonments: &mut abandonments,
            resolved_calls: Vec::new(),
        };
        if let Some(successor) = applier.apply()? {
            successors.push(successor);
        }
        // A record can name the same call in both its canonical history and
        // operation facts. Retire tombstones only after all those facts ran.
        for call_id in applier.resolved_calls {
            if next.state.calls.get(&call_id).is_some_and(|call| {
                call.turn_id
                    .as_ref()
                    .is_some_and(|turn| !next.state.open_turns.contains_key(turn))
            }) {
                next.state.calls.remove(&call_id);
            }
        }
        next.cursor = header.seq;
        if next
            .state
            .history_through
            .is_some_and(|through| header.seq.get() >= through)
        {
            next.state.history_through = None;
        }
        bound_transcript(
            &mut next,
            &mut touched,
            &counted_notices(intents, header.seq.get()),
        );
    }
    next.consume_before_retention(intents);
    let writes = ViewWrites {
        expected_cursor,
        cursor: next.cursor,
        calls: changed_entries(&calls, &next.state.calls),
        open_turns: changed_entries(&open_turns, &next.state.open_turns),
        processes: changed_entries(&processes, &next.state.processes),
        successors,
        upserts: touched
            .keys()
            .filter_map(|key| next.rows.get(key).map(|row| (*key, row.clone())))
            .collect(),
        deletes: touched
            .iter()
            .filter_map(|(key, existed)| (*existed && !next.rows.contains_key(key)).then_some(*key))
            .collect(),
    };
    Ok(Projection {
        views: next,
        writes,
        terminal_usages,
        abandonments,
    })
}

/// The views of a session whose cursor is behind the feed's retention: the
/// head locator from the current head, counters from zero, the transcript
/// from the head's canonical history tail behind a truncation marker, and the
/// cursor immediately before the retained window, which the normal Records
/// path then applies (including its terminal usage facts).
#[must_use]
pub fn rebuild_behind_retention(
    lineage: Vec<LineageStep>,
    runtime_session_id: plakous::model::SessionId,
    retained_from: FeedSeq,
    tail: FeedSeq,
    head: FeedLocator,
    history: &[ReadItem],
) -> SessionViews {
    let mut views = SessionViews::empty_with_lineage(lineage);
    views.state.runtime_session_id = Some(runtime_session_id);
    views.state.head = Some(head);
    views.state.retained_from = Some(retained_from.get());
    views.cursor = FeedSeq::new(retained_from.get().saturating_sub(1));
    views.state.history_through = (views.cursor < tail).then_some(tail.get());
    let mut ordinal = 0_u32;
    for (index, item) in history.iter().enumerate() {
        for row in item_rows(item, index as u64, None)
            .into_iter()
            .filter_map(|derived| match derived {
                Derived::Row(row) => Some(*row),
                Derived::Call(_) => None,
            })
        {
            let key = RowKey::new(tail.get(), Lane::Runtime, ordinal);
            ordinal = ordinal.saturating_add(1);
            put_row(&mut views, key, row);
        }
    }
    views.state.truncated = true;
    let mut touched = BTreeMap::new();
    bound_transcript(&mut views, &mut touched, &[]);
    views
}

struct Applier<'a> {
    views: &'a mut SessionViews,
    intents: &'a ProductIntents,
    reads: &'a dyn ProjectorReads,
    record: &'a FeedRecord,
    ordinal: u32,
    // Whether each touched key existed before this page's first mutation.
    touched: &'a mut BTreeMap<RowKey, bool>,
    terminal_usages: &'a mut Vec<ProjectedTurnUsage>,
    abandonments: &'a mut Vec<ProjectedAbandonment>,
    resolved_calls: Vec<String>,
}

impl Applier<'_> {
    fn seq(&self) -> u64 {
        self.record.header().seq.get()
    }

    fn committed_at(&self) -> u64 {
        self.record.header().committed_at_ms
    }

    fn apply(&mut self) -> Result<Option<Successor>, ProjectError> {
        let header = *self.record.header();
        if matches!(header.change, FeedChange::Archived { .. }) {
            self.views.state.archived = true;
            return Ok(None);
        }
        if self.views.state.applies_own(&header) {
            if self.views.state.lineage.is_empty() {
                self.views.state.lineage.push(LineageStep {
                    branch_id: header.branch_id,
                    from_seq: 0,
                });
            }
            self.apply_own()?;
            return Ok(None);
        }
        let ours = self.views.state.branch_at(header.seq.get());
        match header.change {
            FeedChange::Branched {
                source, transition, ..
            } if Some(source.branch_id) == ours => self.publish_successor(transition).map(Some),
            _ => Ok(None),
        }
    }

    /// A record on this session's branch: its change and every fact.
    fn apply_own(&mut self) -> Result<(), ProjectError> {
        let header = *self.record.header();
        let locator = |checkpoint_id| FeedLocator {
            session_id: header.session_id,
            branch_id: header.branch_id,
            checkpoint_id,
        };
        match header.change {
            FeedChange::Created { genesis, .. } => {
                self.views.state.head = Some(locator(genesis));
                self.views.state.head_working = false;
                self.views.state.released = false;
            }
            FeedChange::Advanced {
                checkpoint, head, ..
            } => {
                self.views.state.head = Some(locator(checkpoint));
                self.views.state.head_working = head == FeedHeadKind::Working;
                self.views.state.released = false;
            }
            FeedChange::Branched { target, .. } => {
                self.views.state.head = Some(locator(target));
                self.views.state.head_working = false;
                self.views.state.released = false;
            }
            FeedChange::LeaseReleased { .. } => self.views.state.released = true,
            _ => {}
        }
        for fact in self.record.facts() {
            self.apply_fact(fact)?;
        }
        self.fenced_abandonment()
    }

    fn read(&self, request: ReadRequest) -> Result<&ReadValue, ProjectError> {
        self.reads
            .read(&request)
            .ok_or(ProjectError::MissingRead(request))
    }

    fn call_read(&self, target: CallTarget) -> Result<CallRead, ProjectError> {
        let header = self.record.header();
        let Some(checkpoint) = record_checkpoint(self.record) else {
            return Ok(CallRead::NotRetained);
        };
        match self.read(ReadRequest::Call {
            branch: header.branch_id,
            checkpoint,
            target,
        })? {
            ReadValue::Call(call) => Ok(call.clone()),
            _ => Ok(CallRead::NotRetained),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Feed-fact dispatch preserves exact history-range loading, per-kind bookkeeping, and terminal usage attribution on the followed branch."
    )]
    fn apply_fact(&mut self, fact: &FeedFact) -> Result<(), ProjectError> {
        match fact {
            FeedFact::SessionCreated { operation_id, .. } => {
                self.views.state.created_operation_id = Some(operation_id.clone());
                Ok(())
            }
            FeedFact::CommandAccepted {
                command_id,
                kind,
                turn_id,
                ..
            } => {
                self.command_accepted(command_id.as_str(), *kind, *turn_id);
                Ok(())
            }
            FeedFact::CommandRejected {
                command_id,
                kind,
                error,
            } => {
                self.command_rejected(command_id.as_str(), *kind, *error);
                Ok(())
            }
            FeedFact::HistoryAppended {
                turn_id,
                segment,
                first_ordinal,
                count,
                first_entry,
                last_entry,
                tool_results,
            } => {
                let header = self.record.header();
                let checkpoint = record_checkpoint(self.record);
                if derives_rows(*segment)
                    && let Some(checkpoint) = checkpoint
                {
                    // A range read in several chunks is retained only whole.
                    let mut items = Some(Vec::new());
                    for request in item_reads(
                        header.branch_id,
                        checkpoint,
                        *first_ordinal,
                        *count,
                        *first_entry,
                        *last_entry,
                    ) {
                        let chunk = match self.read(request)? {
                            ReadValue::Items(chunk) => chunk.as_ref(),
                            _ => None,
                        };
                        items = items.zip(chunk).map(|(mut items, chunk)| {
                            items.extend(chunk.iter().cloned());
                            items
                        });
                    }
                    self.items(
                        turn_id.map(|turn| turn.to_string()),
                        *first_ordinal,
                        items,
                        *segment,
                    );
                }
                for call_id in tool_results {
                    self.tool_result(*call_id, turn_id.map(|turn| turn.to_string()))?;
                }
                Ok(())
            }
            FeedFact::OperationAdvanced {
                operation_id,
                turn_id,
                kind,
                state,
            } => self.operation(
                *operation_id,
                turn_id.map(|turn| turn.to_string()),
                kind,
                *state,
            ),
            FeedFact::TurnTerminal {
                turn_id,
                outcome,
                steering_settled_unapplied,
                usage,
                ..
            } => {
                self.turn_terminal(*turn_id, *outcome, *steering_settled_unapplied)?;
                let header = self.record.header();
                // Inherited terminals rebuild the view, not a second usage event.
                if self.views.state.branch() == Some(header.branch_id) {
                    self.terminal_usages.push(ProjectedTurnUsage {
                        session_id: header.session_id,
                        branch_id: header.branch_id,
                        turn_id: *turn_id,
                        committed_at_ms: header.committed_at_ms,
                        usage: *usage,
                    });
                }
                Ok(())
            }
            FeedFact::WorkflowAdmitted {
                workflow_id,
                operation_id,
                kind,
            } => {
                self.workflow_admitted(workflow_id.as_bytes(), operation_id.as_ref(), *kind);
                Ok(())
            }
            FeedFact::WorkflowSettled {
                workflow_id,
                operation_id,
                kind,
                outcome,
            } => {
                self.workflow_settled(
                    workflow_id.as_bytes(),
                    operation_id.as_deref(),
                    *kind,
                    *outcome,
                );
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn settle(tool: &mut ToolItem, result: &ResultRead) {
    settle_tool_row(tool, result.succeeded, &result.text, None);
}

fn workflow_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

const fn compact_pass_label(pass: plakous::sessions::feed::FeedCompactPass) -> &'static str {
    match pass {
        plakous::sessions::feed::FeedCompactPass::Native => "native",
        plakous::sessions::feed::FeedCompactPass::Summary => "summary",
        plakous::sessions::feed::FeedCompactPass::Placeholders => "placeholders",
        plakous::sessions::feed::FeedCompactPass::ReplayOmission => "replay omission",
        plakous::sessions::feed::FeedCompactPass::Residency => "residency",
        _ => "compaction",
    }
}

/// The snake_case spelling of a recorded refusal kind.
fn control_error_label(kind: plakous::agent::ControlErrorKind) -> String {
    let debug = format!("{kind:?}");
    let mut label = String::with_capacity(debug.len() + 4);
    for (index, character) in debug.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                label.push('_');
            }
            label.push(character.to_ascii_lowercase());
        } else {
            label.push(character);
        }
    }
    label
}

/// A refusal no retry of the same command can pass: it pauses the queue.
fn session_wide(error: &str) -> bool {
    matches!(
        error,
        "storage_pressure" | "ledger_full" | "shutdown" | "internal" | "reclaimed"
    )
}

// Clone-based reference retained only to check the owned projector over recorded feeds.
#[cfg(test)]
pub(super) fn project_cloned(
    views: &SessionViews,
    intents: &ProductIntents,
    records: &[FeedRecord],
    reads: &dyn ProjectorReads,
) -> Result<Projection, ProjectError> {
    let mut next = views.clone();
    let mut touched = BTreeMap::new();
    let mut successors = Vec::new();
    let mut terminal_usages = Vec::new();
    let mut abandonments = Vec::new();
    for record in records {
        let header = record.header();
        if header.seq <= next.cursor {
            continue;
        }
        match next.state.runtime_session_id {
            Some(session) if session != header.session_id => {
                return Err(ProjectError::ForeignSession);
            }
            Some(_) => {}
            None => next.state.runtime_session_id = Some(header.session_id),
        }
        let mut applier = Applier {
            views: &mut next,
            intents,
            reads,
            record,
            ordinal: 0,
            touched: &mut touched,
            terminal_usages: &mut terminal_usages,
            abandonments: &mut abandonments,
            resolved_calls: Vec::new(),
        };
        if let Some(successor) = applier.apply()? {
            successors.push(successor);
        }
        // A record can name the same call in both its canonical history and
        // operation facts. Retire tombstones only after all those facts ran.
        for call_id in applier.resolved_calls {
            if next.state.calls.get(&call_id).is_some_and(|call| {
                call.turn_id
                    .as_ref()
                    .is_some_and(|turn| !next.state.open_turns.contains_key(turn))
            }) {
                next.state.calls.remove(&call_id);
            }
        }
        next.cursor = header.seq;
        if next
            .state
            .history_through
            .is_some_and(|through| header.seq.get() >= through)
        {
            next.state.history_through = None;
        }
        bound_transcript(
            &mut next,
            &mut touched,
            &counted_notices(intents, header.seq.get()),
        );
    }
    next.consume_before_retention(intents);
    let writes = ViewWrites {
        expected_cursor: views.cursor,
        cursor: next.cursor,
        calls: changed_entries(&views.state.calls, &next.state.calls),
        open_turns: changed_entries(&views.state.open_turns, &next.state.open_turns),
        processes: changed_entries(&views.state.processes, &next.state.processes),
        successors,
        upserts: touched
            .keys()
            .filter_map(|key| next.rows.get(key).map(|row| (*key, row.clone())))
            .collect(),
        deletes: touched
            .keys()
            .filter(|key| !next.rows.contains_key(*key) && views.rows.contains_key(*key))
            .copied()
            .collect(),
    };
    Ok(Projection {
        views: next,
        writes,
        terminal_usages,
        abandonments,
    })
}

#[cfg(test)]
mod image_row_tests {
    use super::*;

    #[test]
    fn a_user_item_with_images_is_one_row_with_a_chip_line_per_image() {
        let item = ReadItem {
            actor: ReadActor::User,
            harness_authored: false,
            parts: vec![
                ReadPart::Text("what is this?".to_owned()),
                ReadPart::Image(crate::sessions::image_chip(
                    "image/png",
                    412 * 1024,
                    Some((1280, 720)),
                )),
                ReadPart::Image(crate::sessions::image_chip(
                    "image/webp",
                    2 * 1024 * 1024,
                    None,
                )),
            ],
        };
        let rows = item_rows(&item, 0, Some(7));
        let [Derived::Row(row)] = rows.as_slice() else {
            panic!("one user row")
        };
        assert_eq!(
            **row,
            TranscriptRow::User {
                text: "what is this?\n[image: png 1280x720 412 KB]\n[image: webp 2.0 MB]"
                    .to_owned(),
                sent_at_ms: Some(7),
            }
        );
    }
}
