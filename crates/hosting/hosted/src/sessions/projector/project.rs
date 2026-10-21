//! The one pure projector (spec 02 §2.6). It applies feed records to a
//! session's views, reading only the facade reads a record names, and reads
//! no clock, configuration, environment or event: the same records over the
//! same intents produce the same views, page by page or all at once.

use std::collections::{BTreeMap, BTreeSet};

use plakous::sessions::feed::{
    FeedBranchTransition, FeedCallerLineage, FeedCancelCause, FeedChange, FeedCommandKind,
    FeedFact, FeedHeadKind, FeedLocator, FeedOperationKind, FeedOperationState, FeedRecord,
    FeedSeq, FeedTurnOutcome, FeedWorkflowKind, FeedWorkflowOutcome,
};
use plakous_host_protocol::views::presentation::{ActivityKind, ActivityState, row_bytes};
use plakous_host_protocol::views::transcript::{ProcessNotice, ProcessOutcome, WaitFacts};

use super::intents::ProductIntents;
use super::reads::{
    CallRead, CallTarget, ProjectorReads, ReadActor, ReadItem, ReadPart, ReadRequest, ReadValue,
    ResultRead, TerminalRead, derives_rows, item_reads, operation_read, record_checkpoint,
};
use super::views::{
    Anchored, CallView, Delivery, Lane, LineageStep, OpenTurn, PauseCause, ProcessTrack, RowKey,
    SessionViews, WorkflowCompletion, WorkflowSettlement,
};
use crate::runtime::events::{
    abandoned_turn_operator_detail, compact_terminal_subject, completed_tool_detail,
    completed_tool_state, compose_counts_subject, host_owns_subject, projected_tool_subject,
    wait_settled, wait_started,
};
use crate::sessions::{
    MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES, NoticeScope, ProductCompactReason, ThinkingRow,
    ToolItem, TranscriptRow, bound_row,
};

/// The first ordinal of a model round's prose, keyed at the record of the
/// round's provider decision: after any row that record derives itself.
const ROUND_PROSE_ORDINAL: u32 = 1 << 31;

/// The row a pruned read derives instead of the items a compaction removed.
pub const NOT_RETAINED_ROW: &str = "earlier output not retained";

/// Why a projector run could not apply its records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectError {
    /// A read the record names was not performed; run [`super::plan_reads`]
    /// and load it first.
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

/// The view rows one run changed, and the cursor CAS that commits them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewWrites {
    /// The cursor the run read; the store's CAS compares against it.
    pub expected_cursor: FeedSeq,
    /// The cursor after the run.
    pub cursor: FeedSeq,
    /// Rows to insert or replace at their keys.
    pub upserts: Vec<(RowKey, TranscriptRow)>,
    /// Rows the transcript bound dropped.
    pub deletes: Vec<RowKey>,
    /// Changed call bookkeeping; `None` deletes the key.
    pub calls: Vec<(String, Option<CallView>)>,
    /// Changed open turns; `None` deletes the key.
    pub open_turns: Vec<(String, Option<OpenTurn>)>,
    /// Changed process bookkeeping; `None` deletes the key.
    pub processes: Vec<(u64, Option<ProcessTrack>)>,
    /// Successor records to publish under this cursor CAS.
    pub successors: Vec<Successor>,
}

impl ViewWrites {
    /// The writes that replace stored views, including keyed bookkeeping.
    #[must_use]
    pub fn between(from: &SessionViews, to: &SessionViews) -> Self {
        Self {
            expected_cursor: from.cursor,
            cursor: to.cursor,
            upserts: to
                .rows
                .iter()
                .filter(|(key, row)| from.rows.get(key) != Some(row))
                .map(|(key, row)| (*key, row.clone()))
                .collect(),
            deletes: from
                .rows
                .keys()
                .filter(|key| !to.rows.contains_key(key))
                .copied()
                .collect(),
            calls: changed_entries(&from.state.calls, &to.state.calls),
            open_turns: changed_entries(&from.state.open_turns, &to.state.open_turns),
            processes: changed_entries(&from.state.processes, &to.state.processes),
            successors: Vec::new(),
        }
    }

    /// Whether the run changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.expected_cursor == self.cursor
            && self.upserts.is_empty()
            && self.deletes.is_empty()
            && self.calls.is_empty()
            && self.open_turns.is_empty()
            && self.processes.is_empty()
            && self.successors.is_empty()
    }
}

fn changed_entries<K: Clone + Ord, V: Clone + PartialEq>(
    from: &BTreeMap<K, V>,
    to: &BTreeMap<K, V>,
) -> Vec<(K, Option<V>)> {
    to.iter()
        .filter(|(key, value)| from.get(key) != Some(value))
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .chain(
            from.keys()
                .filter(|key| !to.contains_key(key))
                .map(|key| (key.clone(), None)),
        )
        .collect()
}

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
/// not loaded into `reads`.
pub fn project(
    views: &SessionViews,
    intents: &ProductIntents,
    records: &[FeedRecord],
    reads: &dyn ProjectorReads,
) -> Result<Projection, ProjectError> {
    let mut next = views.clone();
    let mut touched = BTreeSet::new();
    let mut successors = Vec::new();
    let mut terminal_usages = Vec::new();
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
            .iter()
            .filter_map(|key| next.rows.get(key).map(|row| (*key, row.clone())))
            .collect(),
        deletes: touched
            .iter()
            .filter(|key| !next.rows.contains_key(*key) && views.rows.contains_key(*key))
            .copied()
            .collect(),
    };
    Ok(Projection {
        views: next,
        writes,
        terminal_usages,
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
    let mut touched = BTreeSet::new();
    bound_transcript(&mut views, &mut touched, &[]);
    views
}

struct Applier<'a> {
    views: &'a mut SessionViews,
    intents: &'a ProductIntents,
    reads: &'a dyn ProjectorReads,
    record: &'a FeedRecord,
    ordinal: u32,
    touched: &'a mut BTreeSet<RowKey>,
    terminal_usages: &'a mut Vec<ProjectedTurnUsage>,
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
        Ok(())
    }

    /// A `Branched` record whose source is this session's branch: the source
    /// settles its request, and the successor starts from a copy of these
    /// views with the record applied on its own branch.
    fn publish_successor(
        &mut self,
        transition: FeedBranchTransition,
    ) -> Result<Successor, ProjectError> {
        let header = *self.record.header();
        let FeedChange::Branched { target, .. } = header.change else {
            unreachable!("only a branch record publishes a successor")
        };
        let mut operation_id = None;
        for fact in self.record.facts() {
            if let FeedFact::WorkflowAdmitted { .. } | FeedFact::WorkflowSettled { .. } = fact {
                self.apply_fact(fact)?;
            }
            if let FeedFact::WorkflowSettled {
                operation_id: Some(id),
                ..
            } = fact
            {
                operation_id = Some(id.clone());
            }
        }
        // Retention seeds and intent-only pause joins are not a complete
        // fact-derived prefix for the successor's independently owned intents.
        let can_seed = self.views.state.retained_from.is_none()
            && !matches!(
                &self.views.state.pause,
                Some(Anchored {
                    value: Some(PauseCause::CompactionNotApplied { .. }),
                    ..
                })
            );
        let seed = if can_seed {
            let mut inherited = self.views.clone();
            for request in &self.intents.workflow_requests {
                if request.kind == crate::sessions::SessionWorkflowKind::Clear {
                    inherited.state.workflows.remove(&request.operation_id);
                }
            }
            inherited.state.lineage.push(LineageStep {
                branch_id: header.branch_id,
                from_seq: header.seq.get(),
            });
            project(
                &inherited,
                self.intents,
                std::slice::from_ref(self.record),
                self.reads,
            )
            .ok()
            .map(|run| run.views)
        } else {
            None
        };
        Ok(Successor {
            operation_id,
            transition,
            target: FeedLocator {
                session_id: header.session_id,
                branch_id: header.branch_id,
                checkpoint_id: target,
            },
            published_seq: header.seq,
            created_at_unix: header.committed_at_ms / 1_000,
            seed,
        })
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

    fn next_key(&mut self) -> RowKey {
        let key = RowKey::new(self.seq(), Lane::Runtime, self.ordinal);
        self.ordinal = self.ordinal.saturating_add(1);
        key
    }

    fn push(&mut self, row: TranscriptRow) {
        let key = self.next_key();
        self.replace(key, row);
    }

    fn replace(&mut self, key: RowKey, row: TranscriptRow) {
        // The head's canonical seed already covers this entire replay window.
        if self
            .views
            .state
            .history_through
            .is_some_and(|through| self.seq() <= through)
        {
            return;
        }
        self.touched.insert(key);
        put_row(self.views, key, row);
    }

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
                operation_id,
                ..
            } => self.command_accepted(command_id.as_str(), *kind, *turn_id, *operation_id),
            FeedFact::CommandRejected {
                command_id,
                kind,
                error,
            } => {
                let error = control_error_label(*error);
                self.views.state.deliveries.insert(
                    command_id.as_str().to_owned(),
                    Delivery::Rejected {
                        error: error.clone(),
                    },
                );
                if matches!(kind, FeedCommandKind::Root { .. } | FeedCommandKind::Steer) {
                    self.push(TranscriptRow::warning(
                        NoticeScope::Queue,
                        format!(
                            "{}: {}",
                            crate::runtime::PROMPT_NOT_ADMITTED,
                            error.replace('_', " ")
                        ),
                    ));
                    if session_wide(&error) {
                        self.views.state.pause = Some(Anchored {
                            anchor: self.seq(),
                            value: Some(PauseCause::SessionRefusal {
                                command_id: command_id.as_str().to_owned(),
                                error,
                            }),
                        });
                    }
                }
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
                    self.items(turn_id.map(|turn| turn.to_string()), *first_ordinal, items);
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
                let hex = workflow_hex(workflow_id.as_bytes());
                let key = operation_id.clone().unwrap_or_else(|| hex.clone());
                self.views
                    .state
                    .workflows
                    .entry(key)
                    .or_insert(WorkflowCompletion {
                        admitted_at: None,
                        settled: None,
                    })
                    .admitted_at = Some(self.seq());
                if matches!(kind, FeedWorkflowKind::Compact { .. }) {
                    self.compact_row(workflow_id.as_bytes());
                }
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

    fn command_accepted(
        &mut self,
        command_id: &str,
        kind: FeedCommandKind,
        turn_id: Option<plakous::model::TurnId>,
        operation_id: Option<u64>,
    ) -> Result<(), ProjectError> {
        self.views
            .state
            .deliveries
            .insert(command_id.to_owned(), Delivery::Accepted);
        match kind {
            FeedCommandKind::Root { origin } => {
                let host_wake = origin == plakous::model::TurnOrigin::HostWake;
                self.views.state.counters.root_turns += 1;
                if let Some(turn_id) = turn_id {
                    self.views.state.open_turns.insert(
                        turn_id.to_string(),
                        OpenTurn {
                            command_id: command_id.to_owned(),
                            host_wake,
                            round_anchor: None,
                            cancel_requested: false,
                        },
                    );
                }
                if !host_wake {
                    let anchor = self.seq();
                    self.views.state.wake = Some(Anchored {
                        anchor,
                        value: false,
                    });
                    self.views.state.pause = Some(Anchored {
                        anchor,
                        value: None,
                    });
                }
            }
            FeedCommandKind::Steer => {
                let text = match operation_id {
                    Some(operation_id) => {
                        match self.call_read(CallTarget::Operation(operation_id))? {
                            CallRead::Steer(text) => Some(text),
                            _ => None,
                        }
                    }
                    None => None,
                };
                if let Some(text) = text {
                    let sent_at_ms = Some(
                        self.intents
                            .submission(command_id)
                            .map_or_else(|| self.committed_at(), |item| item.submitted_at_ms),
                    );
                    self.push(TranscriptRow::Steer { text, sent_at_ms });
                }
            }
            FeedCommandKind::Cancel => {
                let open =
                    turn_id.and_then(|turn| self.views.state.open_turns.get_mut(&turn.to_string()));
                let host_wake = open.is_some_and(|turn| {
                    turn.cancel_requested = true;
                    turn.host_wake
                });
                if host_wake {
                    self.views.state.wake = Some(Anchored {
                        anchor: self.seq(),
                        value: true,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn items(&mut self, turn_id: Option<String>, first_ordinal: u64, items: Option<Vec<ReadItem>>) {
        let Some(items) = items else {
            self.push(TranscriptRow::info(NoticeScope::Session, NOT_RETAINED_ROW));
            return;
        };
        let sent_at_ms = turn_id
            .as_ref()
            .and_then(|turn| self.views.state.open_turns.get(turn))
            .and_then(|turn| self.intents.submission(&turn.command_id))
            .map(|submission| submission.submitted_at_ms);
        let round_anchor = if items.iter().any(|item| item.actor == ReadActor::Assistant) {
            turn_id
                .as_ref()
                .and_then(|turn| self.views.state.open_turns.get_mut(turn))
                .and_then(|turn| turn.round_anchor.take())
        } else {
            None
        };
        let mut prose = ROUND_PROSE_ORDINAL;
        for (index, item) in items.iter().enumerate() {
            let ordinal = first_ordinal.saturating_add(index as u64);
            for derived in item_rows(item, ordinal, sent_at_ms) {
                match derived {
                    Derived::Row(row) => match round_anchor {
                        Some(anchor) if item.actor == ReadActor::Assistant => {
                            let key = RowKey::new(anchor, Lane::Runtime, prose);
                            prose = prose.saturating_add(1);
                            self.replace(key, *row);
                        }
                        _ => self.push(*row),
                    },
                    Derived::Call(ReadPart::Call {
                        call_id,
                        canonical,
                        exposed,
                        arguments,
                        parent_call_id,
                        cell_id,
                        subject,
                    }) if !self.views.state.calls.contains_key(call_id) => {
                        let mut tool = tool_row(call_id, canonical, exposed, arguments.as_deref());
                        tool.parent_call_id.clone_from(parent_call_id);
                        tool.cell_id.clone_from(cell_id);
                        if subject.is_some()
                            && !(tool.subject.is_some() && host_owns_subject(canonical, exposed))
                        {
                            tool.subject.clone_from(subject);
                        }
                        self.call_key(call_id, turn_id.clone(), || tool);
                    }
                    // A result committed in the same item as its call is a
                    // provider-hosted call's: it has no operation, so the
                    // segment's commit settles it.
                    Derived::Call(ReadPart::Result { call_id, result })
                        if item.parts.iter().any(|part| {
                            matches!(part, ReadPart::Call { call_id: called, .. } if called == call_id)
                        }) =>
                    {
                        self.hosted_result(call_id, result);
                    }
                    Derived::Call(_) => {}
                }
            }
        }
    }

    fn hosted_result(&mut self, call_id: &str, result: &ResultRead) {
        let Some(call) = self.views.state.calls.get_mut(call_id) else {
            return;
        };
        if call.operated || call.settled {
            return;
        }
        call.settled = true;
        let key = call.key;
        let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() else {
            return;
        };
        settle(&mut tool, result);
        self.replace(key, TranscriptRow::Tool(tool));
    }

    fn tool_result(
        &mut self,
        call: plakous::model::CallId,
        turn_id: Option<String>,
    ) -> Result<(), ProjectError> {
        let read = self.call_read(CallTarget::Call(call))?;
        let call_id = call.to_string();
        let CallRead::Tool {
            canonical,
            exposed,
            arguments,
            result,
        } = read
        else {
            return Ok(());
        };
        let key = self.call_key(&call_id, turn_id, || {
            tool_row(&call_id, &canonical, &exposed, arguments.as_deref())
        });
        self.count(&call_id);
        if result.is_some()
            && self
                .views
                .state
                .calls
                .get(&call_id)
                .is_some_and(|call| call.settled)
        {
            self.resolved_calls.push(call_id.clone());
        }
        let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() else {
            if result.is_some()
                && let Some(call) = self.views.state.calls.get_mut(&call_id)
            {
                call.settled = true;
            }
            return Ok(());
        };
        // The committed call block names the canonical tool and its
        // arguments; the effect's facts named only what the model called.
        if tool.name != canonical {
            let fresh = tool_row(&call_id, &canonical, &exposed, arguments.as_deref());
            tool.name = fresh.name;
            tool.kind = fresh.kind;
            tool.exposed_name = fresh.exposed_name;
            tool.detail = tool.detail.or(fresh.detail);
            tool.wait = tool.wait.or(fresh.wait);
        }
        if tool.subject.is_none() {
            tool.subject = projected_tool_subject(&canonical, arguments.as_deref());
        }
        let call = self.views.state.calls.get_mut(&call_id);
        if let Some(result) = result.as_ref() {
            if let Some(call) = call {
                call.settled = true;
            }
            settle(&mut tool, result);
        }
        if tool.kind == ActivityKind::Compose {
            let mut count = 0;
            let mut failed = 0;
            for child in self.views.rows.values().filter_map(TranscriptRow::as_tool) {
                if child.parent_call_id.as_deref() == Some(&call_id) {
                    count += 1;
                    failed += usize::from(matches!(
                        child.activity_state(),
                        ActivityState::Failed { .. }
                    ));
                }
            }
            tool.subject = Some(compose_counts_subject(
                count,
                failed,
                self.views.state.truncated,
            ));
        }
        self.replace(key, TranscriptRow::Tool(tool));
        Ok(())
    }

    /// The key of the call's row, creating it at this record when no fact
    /// mentioned the call before.
    fn call_key(
        &mut self,
        call_id: &str,
        turn_id: Option<String>,
        create: impl FnOnce() -> ToolItem,
    ) -> RowKey {
        if let Some(call) = self.views.state.calls.get(call_id) {
            return call.key;
        }
        // A later fact can mention a canonical call whose live bookkeeping
        // was already pruned. Its retained row still owns the key.
        let existing = self.views.rows.iter().find_map(|(key, row)| {
            row.as_tool()
                .filter(|tool| tool.call_id == call_id)
                .map(|tool| {
                    (
                        *key,
                        tool.started_at_ms.is_some()
                            || matches!(
                                tool.activity_state(),
                                ActivityState::Succeeded | ActivityState::Failed { .. }
                            ),
                    )
                })
        });
        let key = existing.map_or_else(|| self.next_key(), |(key, _)| key);
        self.views.state.calls.insert(
            call_id.to_owned(),
            CallView {
                key,
                turn_id,
                counted: existing.is_some_and(|(_, counted)| counted),
                operated: false,
                settled: false,
            },
        );
        if existing.is_none() {
            self.replace(key, TranscriptRow::Tool(create()));
        }
        key
    }

    fn count(&mut self, call_id: &str) {
        if let Some(call) = self.views.state.calls.get_mut(call_id)
            && !call.counted
        {
            call.counted = true;
            self.views.state.counters.tool_calls += 1;
        }
    }

    fn operation(
        &mut self,
        operation_id: u64,
        turn_id: Option<String>,
        kind: &FeedOperationKind,
        state: FeedOperationState,
    ) -> Result<(), ProjectError> {
        match kind {
            FeedOperationKind::Effect {
                call_id,
                exposed_name,
                lineage,
                subject,
            } => {
                let (parent_call_id, cell_id) = match lineage {
                    FeedCallerLineage::LocalProgrammatic {
                        parent_call_id,
                        cell_id,
                        ..
                    } => (
                        parent_call_id.map(|id| id.to_string()),
                        Some(cell_id.clone()),
                    ),
                    _ => (None, None),
                };
                self.effect(
                    operation_read(operation_id, kind, state),
                    turn_id,
                    &call_id.to_string(),
                    exposed_name.as_str(),
                    subject.as_deref(),
                    (parent_call_id, cell_id),
                    state,
                )
            }
            FeedOperationKind::CodeCell {
                call_id,
                exposed_name,
                subject,
            } => self.effect(
                operation_read(operation_id, kind, state),
                turn_id,
                &call_id.to_string(),
                exposed_name.as_str(),
                subject.as_deref(),
                (None, None),
                state,
            ),
            FeedOperationKind::ProcessSession { .. } => self.process(operation_id, state),
            FeedOperationKind::ProviderAttempt { .. } if state == FeedOperationState::Completed => {
                let anchor = self.seq();
                if let Some(turn) = turn_id
                    .as_ref()
                    .and_then(|turn| self.views.state.open_turns.get_mut(turn))
                {
                    turn.round_anchor = Some(anchor);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    #[allow(clippy::too_many_arguments, reason = "one fact's fields")]
    fn effect(
        &mut self,
        read: Option<CallTarget>,
        turn_id: Option<String>,
        call_id: &str,
        exposed: &str,
        subject: Option<&str>,
        lineage_parent_cell: (Option<String>, Option<String>),
        state: FeedOperationState,
    ) -> Result<(), ProjectError> {
        let nested = lineage_parent_cell.1.is_some();
        let (owning_parent, owning_cell) = lineage_parent_cell;
        let read = match read {
            Some(target) => self.call_read(target)?,
            None => CallRead::NotRetained,
        };
        let (canonical, arguments, result) = match read {
            CallRead::Tool {
                canonical,
                arguments,
                result,
                ..
            } => (canonical, arguments, result),
            CallRead::Nested { name, arguments } => (name, arguments, None),
            _ => {
                let canonical = self
                    .views
                    .state
                    .calls
                    .get(call_id)
                    .and_then(|call| self.views.rows.get(&call.key))
                    .and_then(TranscriptRow::as_tool)
                    .map_or_else(|| exposed.to_owned(), |tool| tool.name.clone());
                (canonical, None, None)
            }
        };
        let key = self.call_key(call_id, turn_id, || {
            tool_row(call_id, &canonical, exposed, arguments.as_deref())
        });
        self.count(call_id);
        let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() else {
            if let Some(call) = self.views.state.calls.get_mut(call_id) {
                call.operated = true;
                call.settled |= state != FeedOperationState::Dispatched
                    && (state != FeedOperationState::Completed || nested);
            }
            return Ok(());
        };
        tool.name.clone_from(&canonical);
        tool.kind = crate::activity::tool_activity_kind(&canonical);
        tool.exposed_name = (exposed != canonical).then(|| exposed.to_owned());
        if let Some(subject) = subject
            && !(tool.subject.is_some() && host_owns_subject(&canonical, exposed))
        {
            tool.subject = Some(subject.to_owned());
        }
        if owning_parent.is_some() {
            tool.parent_call_id = owning_parent;
        }
        if owning_cell.is_some() {
            tool.cell_id = owning_cell;
        }
        let committed_at = self.committed_at();
        let Some(call) = self.views.state.calls.get_mut(call_id) else {
            return Ok(());
        };
        call.operated = true;
        match state {
            FeedOperationState::Dispatched => {
                tool.started_at_ms.get_or_insert(committed_at);
            }
            FeedOperationState::Completed
            | FeedOperationState::OutcomeUnknown
            | FeedOperationState::ProvenNotDispatched => {
                if tool.started_at_ms.is_some() {
                    tool.finished_at_ms.get_or_insert(committed_at);
                }
                // A history call settles from its committed result; only a
                // nested call, which has none, settles on completion.
                let settles = state != FeedOperationState::Completed || nested;
                if !call.settled && settles {
                    call.settled = true;
                    match (state, result) {
                        (FeedOperationState::Completed, Some(result)) => settle(&mut tool, &result),
                        (FeedOperationState::Completed, None) => {
                            tool.state = completed_tool_state(&tool.name, Some((true, "")));
                        }
                        (FeedOperationState::ProvenNotDispatched, _) => {
                            "cancelled".clone_into(&mut tool.state);
                        }
                        _ => "interrupted".clone_into(&mut tool.state),
                    }
                }
            }
            _ => {}
        }
        let waited_ms = tool.duration_ms();
        if let Some(wait) = tool.wait.as_mut() {
            wait.waited_ms = waited_ms;
        }
        self.replace(key, TranscriptRow::Tool(tool));
        Ok(())
    }

    fn process(
        &mut self,
        operation_id: u64,
        state: FeedOperationState,
    ) -> Result<(), ProjectError> {
        let committed_at = self.committed_at();
        let track = self
            .views
            .state
            .processes
            .entry(operation_id)
            .or_insert(ProcessTrack {
                dispatched_at_ms: None,
                reported: false,
            });
        match state {
            FeedOperationState::Dispatched => {
                track.dispatched_at_ms.get_or_insert(committed_at);
                return Ok(());
            }
            FeedOperationState::Completed | FeedOperationState::OutcomeUnknown => {}
            _ => return Ok(()),
        }
        if track.reported {
            return Ok(());
        }
        let dispatched_at = track.dispatched_at_ms;
        let CallRead::Job {
            number: Some(number),
            command,
            end: Some(end),
        } = self.call_read(CallTarget::Operation(operation_id))?
        else {
            return Ok(());
        };
        if let Some(track) = self.views.state.processes.get_mut(&operation_id) {
            track.reported = true;
        }
        self.push(TranscriptRow::Process(ProcessNotice {
            job_id: format!("p{number}"),
            outcome: if end.stopped {
                ProcessOutcome::Stopped
            } else {
                ProcessOutcome::Exited {
                    code: end.exit_code,
                }
            },
            duration_ms: dispatched_at.and_then(|start| committed_at.checked_sub(start)),
            subject: command,
        }));
        Ok(())
    }

    fn turn_terminal(
        &mut self,
        turn_id: plakous::model::TurnId,
        outcome: FeedTurnOutcome,
        steering_unapplied: bool,
    ) -> Result<(), ProjectError> {
        let turn = turn_id.to_string();
        let open_turn = self.views.state.open_turns.remove(&turn);
        // The operator's interrupt: a requested cancel, or any cancel of a
        // turn an accepted operator cancel named (its in-flight work may be
        // settled by an abandonment closure; the live row is the marker).
        let interrupted = match outcome {
            FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Requested,
            } => true,
            FeedTurnOutcome::Cancelled { .. } => {
                open_turn.as_ref().is_some_and(|turn| turn.cancel_requested)
            }
            _ => false,
        };
        let completed = matches!(outcome, FeedTurnOutcome::Completed { .. });
        let open: Vec<(String, RowKey, bool)> = self
            .views
            .state
            .calls
            .iter()
            .filter(|(_, call)| call.turn_id.as_deref() == Some(turn.as_str()) && !call.settled)
            .map(|(id, call)| (id.clone(), call.key, call.operated))
            .collect();
        for (call_id, key, operated) in open {
            if let Some(call) = self.views.state.calls.get_mut(&call_id) {
                call.settled = true;
            }
            if let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() {
                tool.state = if interrupted {
                    "cancelled".to_owned()
                } else if completed && !operated {
                    completed_tool_state(&tool.name, Some((true, "")))
                } else {
                    "interrupted".to_owned()
                };
                self.replace(key, TranscriptRow::Tool(tool));
            }
        }
        // Provisional terminal presentation must retain both the row key and
        // counted bit, including a tombstone for an already truncated row.
        self.views.state.calls.retain(|_, call| {
            !call.counted
                || call.turn_id.as_deref() != Some(turn.as_str())
                || self.views.rows.get(&call.key).is_none_or(|row| {
                    row.as_tool().is_some_and(|tool| {
                        matches!(tool.state.as_str(), "interrupted" | "cancelled")
                    })
                })
        });
        let anchor = self.seq();
        match outcome {
            FeedTurnOutcome::Completed {
                finish: plakous::model::FinishReason::Length,
            } => {
                let bound = match self.terminal_read(turn_id)? {
                    TerminalRead::Truncated(bound) => bound,
                    _ => None,
                };
                self.push(TranscriptRow::warning(
                    NoticeScope::Turn,
                    crate::runtime::truncation_note(bound),
                ));
            }
            FeedTurnOutcome::Failed { context, .. } => {
                let text = match self.terminal_read(turn_id)? {
                    TerminalRead::Failed(error) => error,
                    _ => "agent turn failed".to_owned(),
                };
                self.push(TranscriptRow::error(NoticeScope::Turn, text));
                if context.is_some() {
                    self.views.state.pause = Some(Anchored {
                        anchor,
                        value: Some(PauseCause::ContextCapacity {
                            turn_id: turn.clone(),
                        }),
                    });
                    if open_turn.as_ref().is_some_and(|turn| turn.host_wake) {
                        self.views.state.wake = Some(Anchored {
                            anchor,
                            value: true,
                        });
                    }
                }
            }
            FeedTurnOutcome::Cancelled { .. } if interrupted => {
                self.push(TranscriptRow::Interrupted);
            }
            FeedTurnOutcome::Cancelled {
                cause:
                    FeedCancelCause::Abandoned {
                        claim,
                        cause,
                        model_round_completed,
                        tool_round_completed,
                    },
            } => {
                let mut line = abandoned_turn_operator_detail(
                    claim,
                    cause,
                    tool_round_completed,
                    model_round_completed,
                )
                .to_owned();
                if steering_unapplied {
                    line.push_str(crate::runtime::STEERING_NOT_APPLIED_CLAUSE);
                }
                self.push(TranscriptRow::warning(NoticeScope::Recovery, line));
            }
            FeedTurnOutcome::RecoveryRequired => {
                self.push(TranscriptRow::error(
                    NoticeScope::Recovery,
                    crate::runtime::RECOVERY_PENDING_NOTE,
                ));
                self.views.state.pause = Some(Anchored {
                    anchor,
                    value: Some(PauseCause::RecoveryRequired { turn_id: turn }),
                });
            }
            _ => {}
        }
        // Spec 02 §4.2: a completed turn says so in one recovery notice, an
        // abandonment in its own line; a recorded failure or cancel says
        // nothing more.
        if steering_unapplied && matches!(outcome, FeedTurnOutcome::Completed { .. }) {
            self.push(TranscriptRow::info(
                NoticeScope::Recovery,
                crate::runtime::RECOVERED_COMPLETED_STEERING_NOTICE,
            ));
        }
        Ok(())
    }

    fn terminal_read(&self, turn_id: plakous::model::TurnId) -> Result<TerminalRead, ProjectError> {
        let Some(checkpoint) = record_checkpoint(self.record) else {
            return Ok(TerminalRead::NotRetained);
        };
        match self.read(ReadRequest::Terminal {
            branch: self.record.header().branch_id,
            checkpoint,
            turn_id,
        })? {
            ReadValue::Terminal(terminal) => Ok(terminal.clone()),
            _ => Ok(TerminalRead::NotRetained),
        }
    }

    fn compact_row(&mut self, workflow_id: &[u8]) -> RowKey {
        let hex = workflow_hex(workflow_id);
        if let Some(key) = self.views.state.compactions.get(&hex) {
            return *key;
        }
        let key = self.next_key();
        self.views.state.compactions.insert(hex, key);
        let mut tool = tool_row(
            &super::compaction_call_id(workflow_id),
            "compact",
            "compact",
            None,
        );
        tool.started_at_ms = Some(self.committed_at());
        self.replace(key, TranscriptRow::Tool(tool));
        key
    }

    fn workflow_settled(
        &mut self,
        workflow_id: &[u8],
        operation_id: Option<&str>,
        kind: FeedWorkflowKind,
        outcome: FeedWorkflowOutcome,
    ) {
        let hex = workflow_hex(workflow_id);
        let settlement = match outcome {
            FeedWorkflowOutcome::Compacted {
                tokens_before,
                tokens_after,
                ..
            } => WorkflowSettlement::Compacted {
                tokens_before,
                tokens_after,
            },
            FeedWorkflowOutcome::NoEligibleRange => WorkflowSettlement::NoEligibleRange,
            FeedWorkflowOutcome::Abandoned { .. } => WorkflowSettlement::Abandoned,
            FeedWorkflowOutcome::Published { target } => WorkflowSettlement::Published { target },
            _ => WorkflowSettlement::Failed,
        };
        let key = operation_id.map_or_else(|| hex.clone(), str::to_owned);
        self.views
            .state
            .workflows
            .entry(key)
            .or_insert(WorkflowCompletion {
                admitted_at: None,
                settled: None,
            })
            .settled = Some(settlement.clone());
        if !matches!(kind, FeedWorkflowKind::Compact { .. }) {
            return;
        }
        let row_key = self.compact_row(workflow_id);
        let committed_at = self.committed_at();
        if let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&row_key).cloned() {
            tool.finished_at_ms = Some(committed_at);
            match outcome {
                FeedWorkflowOutcome::Compacted {
                    pass,
                    selected_segments,
                    tokens_before,
                    tokens_after,
                    ..
                } => {
                    "succeeded".clone_into(&mut tool.state);
                    tool.subject = compact_terminal_subject(
                        Some(tokens_before),
                        Some(tokens_after),
                        true,
                        true,
                    );
                    tool.detail = Some(format!(
                        "{} pass · {selected_segments} segments",
                        compact_pass_label(pass)
                    ));
                }
                FeedWorkflowOutcome::NoEligibleRange => "succeeded".clone_into(&mut tool.state),
                FeedWorkflowOutcome::Abandoned { .. } => "interrupted".clone_into(&mut tool.state),
                _ => "failed".clone_into(&mut tool.state),
            }
            self.replace(row_key, TranscriptRow::Tool(tool));
        }
        match outcome {
            FeedWorkflowOutcome::Compacted { .. } => self.views.state.counters.compactions += 1,
            FeedWorkflowOutcome::NoEligibleRange => self.push(TranscriptRow::info(
                NoticeScope::Compaction,
                crate::runtime::COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
            )),
            FeedWorkflowOutcome::Abandoned { summary_unsent } => {
                self.push(TranscriptRow::warning(
                    NoticeScope::Recovery,
                    if summary_unsent {
                        crate::runtime::COMPACT_ABANDONED_UNSENT_NOTE
                    } else {
                        crate::runtime::COMPACT_ABANDONED_BILLED_NOTE
                    },
                ));
            }
            _ => self.push(TranscriptRow::warning(
                NoticeScope::Compaction,
                crate::runtime::COMPACT_FAILED_DETAIL,
            )),
        }
        let pass_pauses = operation_id
            .and_then(|id| self.intents.workflow_request(id))
            .and_then(super::intents::WorkflowRequest::compact_reason)
            .is_some_and(|reason| {
                matches!(
                    reason,
                    ProductCompactReason::Scheduled | ProductCompactReason::CapacityRecovery
                )
            });
        if pass_pauses
            && !matches!(outcome, FeedWorkflowOutcome::Compacted { .. })
            && let Some(operation_id) = operation_id
        {
            self.views.state.pause = Some(Anchored {
                anchor: self.seq(),
                value: Some(PauseCause::CompactionNotApplied {
                    operation_id: operation_id.to_owned(),
                }),
            });
        }
    }
}

/// What one history item derives, in part order.
enum Derived<'a> {
    /// A prose, mail or reasoning row.
    Row(Box<TranscriptRow>),
    /// A tool call or result block: its row is keyed by call id.
    Call(&'a ReadPart),
}

/// What one history item derives: user and assistant prose, inbound mail as
/// one tree notice (a prompt's child-mail batch as one per child), visible reasoning, and its call blocks in order.
fn item_rows(item: &ReadItem, ordinal: u64, sent_at_ms: Option<u64>) -> Vec<Derived<'_>> {
    let mut rows = Vec::new();
    let mut text = String::new();
    let flush = |rows: &mut Vec<Derived<'_>>, text: &mut String| {
        if text.trim().is_empty() {
            text.clear();
            return;
        }
        let value = std::mem::take(text);
        if let Some(mail) = crate::topology_navigator::mail_row(&value) {
            rows.push(Derived::Row(Box::new(TranscriptRow::info(
                NoticeScope::Tree,
                mail,
            ))));
            return;
        }
        if item.harness_authored {
            return;
        }
        match item.actor {
            ReadActor::User => {
                let (own, mail) = crate::topology_navigator::split_child_mail(&value)
                    .unwrap_or_else(|| (value, Vec::new()));
                if !own.trim().is_empty() {
                    rows.push(Derived::Row(Box::new(TranscriptRow::User {
                        text: own,
                        sent_at_ms,
                    })));
                }
                rows.extend(mail.into_iter().map(|mail| {
                    Derived::Row(Box::new(TranscriptRow::info(NoticeScope::Tree, mail)))
                }));
            }
            ReadActor::Assistant => {
                rows.push(Derived::Row(Box::new(TranscriptRow::assistant(value))));
            }
            ReadActor::Other => {}
        }
    };
    for (block, part) in item.parts.iter().enumerate() {
        match part {
            ReadPart::Text(value) | ReadPart::Image(value) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
            ReadPart::Reasoning(value) => {
                flush(&mut rows, &mut text);
                if !value.trim().is_empty() {
                    rows.push(Derived::Row(Box::new(TranscriptRow::Thinking(
                        ThinkingRow {
                            location_key: format!("{ordinal}:{block}"),
                            text: value.clone(),
                            elapsed_ms: None,
                        },
                    ))));
                }
            }
            ReadPart::Call { .. } | ReadPart::Result { .. } => {
                flush(&mut rows, &mut text);
                rows.push(Derived::Call(part));
            }
            ReadPart::Compaction => flush(&mut rows, &mut text),
        }
    }
    flush(&mut rows, &mut text);
    rows
}

fn tool_row(call_id: &str, canonical: &str, exposed: &str, arguments: Option<&str>) -> ToolItem {
    let args =
        arguments.and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok());
    ToolItem {
        call_id: call_id.to_owned(),
        name: canonical.to_owned(),
        exposed_name: (exposed != canonical).then(|| exposed.to_owned()),
        kind: crate::activity::tool_activity_kind(canonical),
        state: "running".to_owned(),
        subject: projected_tool_subject(canonical, arguments),
        detail: arguments
            .and_then(|arguments| crate::activity::tool_detail_text(Some(arguments), None)),
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: wait_started(canonical, None, args.as_ref()),
    }
}

fn settle(tool: &mut ToolItem, result: &ResultRead) {
    tool.state = completed_tool_state(&tool.name, Some((result.succeeded, &result.text)));
    if let Some(detail) = completed_tool_detail(tool.detail.as_deref(), Some(&result.text)) {
        tool.detail = Some(detail);
    }
    let value = serde_json::from_str::<serde_json::Value>(&result.text).ok();
    let waited_ms = tool.duration_ms();
    tool.wait = wait_settled(
        &tool.name,
        tool.wait.take(),
        value.as_ref(),
        result.succeeded,
        waited_ms,
        None,
    )
    .map(|facts| WaitFacts { waited_ms, ..facts });
}

fn put_row(views: &mut SessionViews, key: RowKey, row: TranscriptRow) {
    let row = bound_row(row);
    let added = row_bytes(&row) as u64;
    if let Some(old) = views.rows.insert(key, row) {
        views.state.row_bytes = views.state.row_bytes.saturating_sub(row_bytes(&old) as u64);
    }
    views.state.row_bytes = views.state.row_bytes.saturating_add(added);
}

/// Past either transcript bound, drops the oldest rows down to seven eighths
/// of both, in key order; the newest row always stays. A function of the rows
/// alone, so a rebuild trims identically.
/// The product-lane rows the transcript bound counts after record `seq`:
/// imported rows and the operator notices anchored before it, with their
/// encoded sizes. A notice anchored at or after `seq` was written after this
/// record was applied, so it is not counted here, incrementally or in a
/// rebuild.
fn counted_notices(intents: &ProductIntents, seq: u64) -> Vec<(RowKey, u64)> {
    let imported = intents.imported.iter().enumerate().map(|(ordinal, row)| {
        (
            RowKey::new(0, Lane::Product, u32::try_from(ordinal).unwrap_or(u32::MAX)),
            row_bytes(row) as u64,
        )
    });
    let notices = intents
        .notices
        .iter()
        .filter(|notice| notice.anchor < seq)
        .map(|notice| (notice.key(), row_bytes(&notice.row) as u64));
    imported.chain(notices).collect()
}

/// The transcript bound over the merged transcript: the projector's rows and
/// the counted product-lane rows, in key order. Only projector rows are
/// dropped; a product row older than the oldest kept projector row of a
/// truncated transcript goes with it ([`SessionViews::transcript`]).
fn bound_transcript(
    views: &mut SessionViews,
    touched: &mut BTreeSet<RowKey>,
    notices: &[(RowKey, u64)],
) {
    let measured = |views: &SessionViews| {
        let floor = views
            .state
            .truncated
            .then(|| views.rows.keys().find(|key| **key != RowKey::TRUNCATION))
            .flatten()
            .copied();
        let (lines, bytes) = notices
            .iter()
            .filter(|(key, _)| floor.is_none_or(|floor| *key >= floor))
            .fold((0_usize, 0_u64), |(lines, bytes), (_, size)| {
                (lines + 1, bytes.saturating_add(*size))
            });
        (
            views.rows.len() + lines,
            views.state.row_bytes.saturating_add(bytes),
        )
    };
    let (lines, bytes) = measured(views);
    if lines <= MAX_TRANSCRIPT_LINES && bytes <= MAX_TRANSCRIPT_BYTES as u64 {
        return;
    }
    let target_lines = MAX_TRANSCRIPT_LINES - MAX_TRANSCRIPT_LINES / 8;
    let target_bytes = (MAX_TRANSCRIPT_BYTES - MAX_TRANSCRIPT_BYTES / 8) as u64;
    while views.rows.len() > 1 {
        let (lines, bytes) = measured(views);
        if lines <= target_lines && bytes <= target_bytes {
            break;
        }
        let Some((key, row)) = views.rows.pop_first() else {
            break;
        };
        views.state.row_bytes = views.state.row_bytes.saturating_sub(row_bytes(&row) as u64);
        if let Some(tool) = row.as_tool()
            && let Some(call) = views.state.calls.get_mut(&tool.call_id)
        {
            // Keep a settled tombstone until its canonical result. Late results
            // cannot resurrect this key or count the call a second time.
            call.settled = true;
        }
        touched.insert(key);
        views.state.truncated = true;
    }
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
