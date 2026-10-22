//! Feed-derived views (spec 02 §2.6): rebuildable from empty, written only by
//! the projector.

use std::collections::BTreeMap;

use plakous::sessions::feed::{FeedLocator, FeedSeq};
use serde::{Deserialize, Serialize};

use super::intents::{HostPauseCause, ProductIntents};
use crate::sessions::TranscriptRow;

/// Which writer a row key belongs to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// A row the projector derived from a feed record.
    Runtime,
    /// An operator notice or an imported row.
    Product,
}

impl Lane {
    /// The stored lane number.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Runtime => 0,
            Self::Product => 1,
        }
    }

    /// Parses a stored lane number.
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Runtime),
            1 => Some(Self::Product),
            _ => None,
        }
    }
}

/// A transcript row's immutable ordering key: (anchor sequence, lane,
/// ordinal). The transcript is ordered by it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RowKey {
    /// The record sequence (lane 0) or the applied cursor (lane 1).
    pub anchor: u64,
    /// The writer.
    pub lane: Lane,
    /// The position among the rows of that anchor and lane.
    pub ordinal: u32,
}

impl RowKey {
    /// The key of the truncation marker: before every other row.
    pub const TRUNCATION: Self = Self::new(0, Lane::Runtime, 0);

    /// A key.
    #[must_use]
    pub const fn new(anchor: u64, lane: Lane, ordinal: u32) -> Self {
        Self {
            anchor,
            lane,
            ordinal,
        }
    }
}

/// Delivery done-ness of one submission, by its command id.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Delivery {
    /// `CommandAccepted`: the item was consumed.
    Accepted,
    /// Retention consumed the intent; no acceptance fact or row is derived.
    Consumed,
    /// `CommandRejected` with its typed kind.
    Rejected {
        /// The recorded refusal kind.
        error: String,
    },
}

/// A workflow's completion, by its request's operation id (a turn's own fold
/// by its derived identity).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCompletion {
    /// The record that admitted it, when retained.
    pub admitted_at: Option<u64>,
    /// How it settled, once settled.
    pub settled: Option<WorkflowSettlement>,
}

/// The settled outcome class of a workflow.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowSettlement {
    /// Retention consumed the request, without a withdrawal or notice.
    Consumed,
    /// A compaction published its successor.
    Compacted {
        /// Measured input tokens before.
        tokens_before: u64,
        /// Measured input tokens after.
        tokens_after: u64,
    },
    /// Nothing could be compacted.
    NoEligibleRange,
    /// A live failure.
    Failed,
    /// A closure commit settled it.
    Abandoned,
    /// A fork or rebind published its target.
    Published {
        /// The target.
        target: FeedLocator,
    },
}

/// Why the queue is paused, from facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PauseCause {
    /// A root turn ended on context capacity.
    ContextCapacity {
        /// The turn.
        turn_id: String,
    },
    /// A root turn ended recovery-required.
    RecoveryRequired {
        /// The turn.
        turn_id: String,
    },
    /// A command was refused session-wide.
    SessionRefusal {
        /// The refused command.
        command_id: String,
        /// The recorded refusal kind.
        error: String,
    },
    /// A scheduled or capacity-recovery compaction did not compact.
    CompactionNotApplied {
        /// The request's operation id.
        operation_id: String,
    },
    /// A cause the host wrote as an intent.
    Host(HostPauseCause),
}

/// A view value with the anchor that set it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Anchored<T> {
    /// The record sequence that set it.
    pub anchor: u64,
    /// The value.
    pub value: T,
}

/// The session counters, monotonic over applied records.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Counters {
    /// Root `CommandAccepted`.
    pub root_turns: u64,
    /// Distinct call ids of effects and committed tool results.
    pub tool_calls: u64,
    /// `WorkflowSettled { Compacted }`.
    pub compactions: u64,
}

/// A root turn the projector has seen admitted and not yet terminal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTurn {
    /// The root command id that admitted it.
    pub command_id: String,
    /// Whether the host woke it.
    pub host_wake: bool,
    /// The record that committed the current model round's provider
    /// decision, until the round's response is appended: the round's prose
    /// is keyed there, ahead of its tool rows, as it streamed live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_anchor: Option<u64>,
    /// An operator cancel naming the turn was accepted: its `Cancelled`
    /// terminal is the operator's interrupt, whatever closure settled the
    /// work it left in flight.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel_requested: bool,
}

/// The bookkeeping of one tool row, by call id.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallView {
    /// The key of the row, fixed by the record that created it.
    pub key: RowKey,
    /// The owning turn.
    pub turn_id: Option<String>,
    /// Whether it counts in [`Counters::tool_calls`] already.
    pub counted: bool,
    /// Whether an operation backs it (a provider-hosted or rejected call has
    /// none).
    pub operated: bool,
    /// Whether it reached a terminal state.
    pub settled: bool,
}

/// One process session the projector follows.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTrack {
    /// When it crossed its dispatch fence.
    pub dispatched_at_ms: Option<u64>,
    /// Whether its terminal row was derived.
    pub reported: bool,
}

/// One branch the session follows, from the record sequence that published
/// it. A successor follows its source's branch below the `Branched` record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LineageStep {
    /// The branch.
    pub branch_id: plakous::model::BranchId,
    /// The `Branched` record that published it; 0 for the session's own
    /// first branch.
    pub from_seq: u64,
}

/// Every non-row view of one product session. The skipped bookkeeping maps
/// are stored in keyed tables and hydrated separately from this JSON state.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionViewState {
    /// The runtime session whose feed the cursor reads.
    pub runtime_session_id: Option<plakous::model::SessionId>,
    /// The branches this session follows, oldest first. Empty until the
    /// session's `Created` record is applied.
    pub lineage: Vec<LineageStep>,
    /// The checkpoint of the newest applied record on the session's branch.
    pub head: Option<FeedLocator>,
    /// The head carries unresolved work.
    pub head_working: bool,
    /// The newest applied record released the branch's lease.
    pub released: bool,
    /// The session was archived.
    pub archived: bool,
    /// Delivery done-ness by command id.
    pub deliveries: BTreeMap<String, Delivery>,
    /// The immutable creation fact, retained for successor intent joins.
    pub created_operation_id: Option<String>,
    /// Workflow completion by operation id.
    pub workflows: BTreeMap<String, WorkflowCompletion>,
    /// The newest fact-derived pause transition; `None` inside means a
    /// user prompt resumed the queue.
    pub pause: Option<Anchored<Option<PauseCause>>>,
    /// The newest fact-derived child-wake transition: `true` suppresses.
    pub wake: Option<Anchored<bool>>,
    /// The counters.
    pub counters: Counters,
    /// Admitted root turns without a terminal.
    #[serde(skip)]
    pub open_turns: BTreeMap<String, OpenTurn>,
    /// Tool row bookkeeping by call id.
    #[serde(skip)]
    pub calls: BTreeMap<String, CallView>,
    /// Compaction rows by derived workflow identity.
    pub compactions: BTreeMap<String, RowKey>,
    /// Process sessions by operation id.
    #[serde(skip)]
    pub processes: BTreeMap<u64, ProcessTrack>,
    /// Rows were dropped by the transcript bound or a behind-retention
    /// rebuild.
    pub truncated: bool,
    /// Encoded bytes of the projector's rows, for the transcript bound.
    pub row_bytes: u64,
    /// Set by a behind-retention rebuild: the cursor proves nothing below
    /// this sequence.
    pub retained_from: Option<u64>,
    /// A behind-retention history seed already contains prose through this
    /// feed sequence. Retained facts still derive usage, calls and counters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_through: Option<u64>,
}

impl SessionViewState {
    /// Settles historical intents without inventing feed facts.
    pub fn consume_before_retention(&mut self, intents: &ProductIntents) {
        for submission in &intents.submissions {
            if intents.submission_consumed(&submission.command_id, self.retained_from) {
                self.deliveries
                    .entry(submission.command_id.clone())
                    .or_insert(Delivery::Consumed);
            }
        }
        for request in &intents.workflow_requests {
            if intents.workflow_consumed(&request.operation_id, self.retained_from) {
                self.workflows
                    .entry(request.operation_id.clone())
                    .or_insert(WorkflowCompletion {
                        admitted_at: None,
                        settled: Some(WorkflowSettlement::Consumed),
                    });
            }
        }
    }

    /// Updates a binding only from its own branch's projected head.
    #[must_use]
    pub fn projected_locator(
        &self,
        recorded: Option<plakous::sessions::SessionLocator>,
    ) -> Option<plakous::sessions::SessionLocator> {
        let head = self.head.map(|head| {
            plakous::sessions::SessionLocator::new(
                head.session_id,
                head.branch_id,
                head.checkpoint_id,
            )
        });
        head.filter(|head| {
            recorded.is_none_or(|recorded| {
                head.session_id() == recorded.session_id()
                    && head.branch_id() == recorded.branch_id()
            })
        })
        .or(recorded)
    }

    pub(super) fn applies_own(&self, header: &plakous::sessions::feed::FeedHeader) -> bool {
        self.branch_at(header.seq.get()) == Some(header.branch_id)
            || (self.lineage.is_empty()
                && matches!(
                    header.change,
                    plakous::sessions::feed::FeedChange::Created { .. }
                ))
    }

    /// The branch a record at `seq` must be on to apply.
    #[must_use]
    pub fn branch_at(&self, seq: u64) -> Option<plakous::model::BranchId> {
        self.lineage
            .iter()
            .rev()
            .find(|step| step.from_seq <= seq)
            .map(|step| step.branch_id)
    }

    /// The branch this session follows now.
    #[must_use]
    pub fn branch(&self) -> Option<plakous::model::BranchId> {
        self.lineage.last().map(|step| step.branch_id)
    }
}

/// One product session's views: the cursor, the non-row state and the
/// projector's transcript rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionViews {
    /// The last applied record.
    pub cursor: FeedSeq,
    /// Everything but rows.
    pub state: SessionViewState,
    /// The projector's rows (lane 0) by key.
    pub rows: BTreeMap<RowKey, TranscriptRow>,
}

impl SessionViews {
    /// Updates a binding only from its own branch's projected head. An
    /// ancestor page of a partially rebuilt successor cannot redirect it.
    #[must_use]
    pub fn projected_locator(
        &self,
        recorded: Option<plakous::sessions::SessionLocator>,
    ) -> Option<plakous::sessions::SessionLocator> {
        self.state.projected_locator(recorded)
    }

    /// Settles intents older than the retained window without inventing facts
    /// or withdrawing a historical clear's successor reservation.
    pub fn consume_before_retention(&mut self, intents: &ProductIntents) {
        self.state.consume_before_retention(intents);
    }

    /// The views of a session nothing was applied to yet.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            cursor: FeedSeq::ZERO,
            state: SessionViewState::default(),
            rows: BTreeMap::new(),
        }
    }

    /// The empty views of a fork or rebind successor, to rebuild it from its
    /// branch lineage.
    #[must_use]
    pub fn empty_with_lineage(lineage: Vec<LineageStep>) -> Self {
        let mut views = Self::empty();
        views.state.lineage = lineage;
        views
    }

    /// The transcript: the truncation marker when rows were dropped, then the
    /// projector's rows, the operator notices and imported rows merged in key
    /// order. A notice or imported row older than the oldest kept projector
    /// row of a truncated transcript went with it.
    #[must_use]
    pub fn transcript(&self, intents: &ProductIntents) -> Vec<(RowKey, TranscriptRow)> {
        let mut merged: BTreeMap<RowKey, TranscriptRow> = self.rows.clone();
        for (ordinal, row) in intents.imported.iter().enumerate() {
            let ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
            merged.insert(RowKey::new(0, Lane::Product, ordinal), row.clone());
        }
        for notice in &intents.notices {
            merged.insert(notice.key(), notice.row.clone());
        }
        if self.state.truncated {
            let floor = self
                .rows
                .keys()
                .find(|key| **key != RowKey::TRUNCATION)
                .copied();
            if let Some(floor) = floor {
                merged.retain(|key, _| *key >= floor);
            }
            merged.insert(RowKey::TRUNCATION, TranscriptRow::Truncated);
        }
        merged.into_iter().collect()
    }

    /// Whether child wakes are suppressed: the newest of the suppression
    /// intents and the fact-derived transitions, by anchor. An intent at
    /// anchor `a` follows record `a` and precedes record `a + 1`.
    #[must_use]
    pub fn wake_suppressed(&self, intents: &ProductIntents) -> bool {
        let fact = self
            .state
            .wake
            .as_ref()
            .map(|wake| (wake.anchor, 0_u8, wake.value));
        let intent = intents
            .wake_suppressions
            .iter()
            .map(|intent| (intent.anchor, 1_u8, true))
            .max_by_key(|(anchor, lane, _)| (*anchor, *lane));
        fact.into_iter()
            .chain(intent)
            .max_by_key(|(anchor, lane, _)| (*anchor, *lane))
            .is_some_and(|(_, _, suppressed)| suppressed)
    }

    /// The newest pause cause, by anchor, from facts and host intents.
    #[must_use]
    pub fn pause_cause(&self, intents: &ProductIntents) -> Option<PauseCause> {
        let fact = self
            .state
            .pause
            .as_ref()
            .map(|pause| ((pause.anchor, 0_u8, 0_u32), pause.value.clone()));
        let intent = intents
            .pause_causes
            .iter()
            .map(|intent| {
                (
                    (intent.anchor, 1_u8, intent.ordinal),
                    Some(PauseCause::Host(intent.value.clone())),
                )
            })
            .max_by_key(|(key, _)| *key);
        fact.into_iter()
            .chain(intent)
            .max_by_key(|(key, _)| *key)
            .and_then(|(_, cause)| cause)
    }
}
