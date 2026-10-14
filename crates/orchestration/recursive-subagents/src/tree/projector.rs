//! The tree projector (spec 03 §14.3): one pure function that applies a page
//! of one runtime session's change feed to the tree's views.
//!
//! It reads only the records, the tree's intents and external-effect records,
//! and the bounded `feed_terminal` reads the caller loaded for the page
//! ([`TreeFeedReads`]). Records at or below the session's cursor are skipped,
//! so a page applied twice, or a page whose tree save lost the revision CAS
//! and is applied again, derives the same rows. The caller saves the tree
//! afterwards: the whole-tree revision fence is the cursor's CAS.
//!
//! What it derives:
//! - wait settlement (spec 03 §4.4) from `HistoryAppended` call ids and
//!   closed or unclosed `TurnTerminal` facts;
//! - command facts, batch acknowledgement and child turn binding (§14.1) from
//!   `CommandAccepted` and `CommandRejected`;
//! - the withheld-mail return (§8) from a withheld `TurnTerminal`;
//! - the active-turn view and the `turn_abandoned` attempt outcome, with the
//!   abandonment's operator line read through `feed_terminal`;
//! - a failed child turn's attempt outcome (kind, status, a block's
//!   `resets_at`) from its `feed_terminal` read, joined with the host's
//!   failed-turn notice.

use std::collections::{BTreeMap, BTreeSet};

use plakous::sessions::feed::{
    FeedCancelCause, FeedChange, FeedFact, FeedHeadKind, FeedHeader, FeedOperationState,
    FeedRecord, FeedTurnOutcome,
};

use super::commands::{AgentCommandKind, CommandFact};
use super::turn_outcome::{FailedTurnDisposition, TurnRefusal, classify_provider_refusal};
use super::{
    AbandonedChildTurn, AttemptStatus, BlockedReason, NodeStatus, ProductAgentTree, RecoveryNeed,
    RecoveryResolution, TURN_ABANDONED, TopologyError, TopologyErrorCode,
};

#[cfg(debug_assertions)]
mod applier_diff;
mod child_finalization;
mod compaction;
mod fold;
pub(in crate::tree) mod host_decision;
pub(in crate::tree) mod host_recovery;
mod mail_join;
pub use child_finalization::{ChildFinalizationDecision, ChildTerminal};

/// The operation-ledger kind of a failed turn's host notice: what the host
/// observed when the turn failed (the failed-worktree snapshot line), keyed by
/// agent, attempt and turn. Feed data never carries it.
pub(crate) const FAILED_TURN_NOTICE_KIND: &str = "failed_turn_notice";

pub(crate) const HOST_TURN_FAILURE_KIND: &str = "host_turn_failure";

pub(crate) const CHILD_SESSION_KIND: &str = "child_session";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum HostFailureDisposition {
    Abandoned,
    Failed,
    RecoveryRequired,
    Blocked(BlockedReason),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct HostTurnFailure {
    attempt_id: crate::protocol::AttemptId,
    turn_id: Option<String>,
    disposition: HostFailureDisposition,
    message: String,
    observed_at_ms: u64,
    waits: Vec<(String, bool)>,
}

fn failed_turn_notice_id(agent_id: &str, attempt_id: &str, turn_id: &str) -> String {
    format!("{FAILED_TURN_NOTICE_KIND}:{agent_id}:{attempt_id}:{turn_id}")
}

/// What a `feed_terminal` read of an abandoned turn said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRead {
    /// The abandonment's operator line.
    pub message: String,
    /// The fact the caller's face reports once, when the closure's report
    /// was read.
    pub abandoned: Option<AbandonedChildTurn>,
}

/// What a `feed_terminal` read of a failed turn said: the error the
/// terminal record holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureRead {
    /// The agent error's class.
    pub kind: plakous::agent::AgentErrorKind,
    /// The provider's retry disposition, when the model error carried one.
    pub retryability: Option<plakous::model::ModelRetryability>,
    /// The operation phase retained in the canonical model error.
    pub phase: Option<plakous::model::ModelErrorPhase>,
    /// The validated HTTP status retained in the canonical model error.
    pub http_status: Option<plakous::model::ModelHttpStatus>,
    /// The bounded provider error code retained in the canonical model error.
    pub provider_code: Option<String>,
    /// The error's diagnostic message.
    pub message: String,
}

/// The `feed_terminal` reads a page needs, loaded by the caller and keyed by
/// root turn id.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeFeedReads {
    terminals: BTreeMap<String, TerminalRead>,
    failures: BTreeMap<String, FailureRead>,
    waits: BTreeMap<String, Vec<(String, bool)>>,
}

impl TreeFeedReads {
    /// Records canonical wait evidence read at a quiescent head. Its exact
    /// locator must be authenticated by the applied feed or retention head.
    pub fn insert_waits(
        &mut self,
        locator: plakous::sessions::SessionLocator,
        evidence: Vec<(String, bool)>,
    ) {
        self.waits.insert(locator.to_string(), evidence);
    }

    /// Records the terminal read for `turn_id`.
    pub fn insert_terminal(&mut self, turn_id: impl Into<String>, read: TerminalRead) {
        self.terminals.insert(turn_id.into(), read);
    }

    /// Records the failure read of `turn_id`'s failed terminal.
    pub fn insert_failure(&mut self, turn_id: impl Into<String>, read: FailureRead) {
        self.failures.insert(turn_id.into(), read);
    }

    /// Carries authenticated head wait evidence into a fresh feed reread.
    pub fn extend_waits_from(&mut self, prior: &Self) {
        self.waits.extend(prior.waits.clone());
    }

    fn terminal(&self, turn_id: &str) -> Option<&TerminalRead> {
        self.terminals.get(turn_id)
    }

    fn failure(&self, turn_id: &str) -> Option<&FailureRead> {
        self.failures.get(turn_id)
    }
}

/// The `feed_terminal` reads a page needs, as `(record index, turn)`: an
/// abandoned or failed turn's terminal, and a recovery closure (a record that
/// finalizes a turn's operations and appends its history with no terminal of
/// its own, which is how a reopen closes a turn whose terminal was
/// `RecoveryRequired`).
#[must_use]
pub fn terminal_reads_needed(records: &[FeedRecord]) -> Vec<(usize, plakous::model::TurnId)> {
    let mut last_records = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        for turn in record.facts().iter().filter_map(fact_turn) {
            last_records.insert(turn, index);
        }
    }
    let mut needed = Vec::new();
    for (index, record) in records.iter().enumerate() {
        for fact in record.facts() {
            if let FeedFact::TurnTerminal {
                turn_id,
                outcome:
                    FeedTurnOutcome::Cancelled {
                        cause: FeedCancelCause::Abandoned { .. },
                    }
                    | FeedTurnOutcome::Failed { .. },
                ..
            } = fact
            {
                needed.push((index, *turn_id));
            }
        }
        if let Some(turn_id) = closure_turn(record)
            && last_records.get(&turn_id) == Some(&index)
        {
            needed.push((index, turn_id));
        }
    }
    needed
}

fn fact_turn(fact: &FeedFact) -> Option<plakous::model::TurnId> {
    match fact {
        FeedFact::CommandAccepted { turn_id, .. }
        | FeedFact::HistoryAppended { turn_id, .. }
        | FeedFact::HistoryRewritten { turn_id, .. }
        | FeedFact::OperationAdvanced { turn_id, .. } => *turn_id,
        FeedFact::TurnTerminal { turn_id, .. } => Some(*turn_id),
        _ => None,
    }
}

/// The turn a recovery closure record closes, when the record is one.
fn closure_turn(record: &FeedRecord) -> Option<plakous::model::TurnId> {
    if !matches!(
        record.header().change,
        FeedChange::Advanced {
            head: FeedHeadKind::Quiescent,
            ..
        }
    ) {
        return None;
    }
    let facts = record.facts();
    if facts
        .iter()
        .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
    {
        return None;
    }
    facts.iter().find_map(|fact| match fact {
        FeedFact::OperationAdvanced {
            turn_id: Some(turn_id),
            state: FeedOperationState::Finalized,
            ..
        } if facts.iter().any(|other| {
            matches!(other, FeedFact::HistoryAppended { turn_id: Some(appended), .. } if appended == turn_id)
        }) =>
        {
            Some(*turn_id)
        }
        _ => None,
    })
}

/// The round a submit-harness corrective command spends: its identity ends
/// `{attempt}:{round}`.
fn corrective_round(command_id: &str) -> Option<super::SubmitNudgeBudget> {
    let round = command_id.rsplit(':').next()?.parse::<u8>().ok()?;
    super::SubmitNudgeBudget::spent(round).filter(|nudge| nudge.rounds() > 0)
}

/// An accepted command whose attempt already binds another running turn
/// (spec 03 §14.1): the binding is untouched and the node is left
/// `recovery_required`; the host cancels and releases the recovered turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindConflict {
    /// The child.
    pub agent_id: String,
    /// The command whose fact named the turn.
    pub command_id: String,
    /// The turn the attempt already binds.
    pub bound_turn_id: String,
    /// The recovered turn the command admitted.
    pub recovered_turn_id: String,
}

/// A child terminal's disposable observation, keyed by the runtime turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectedChildTurnUsage {
    /// The terminal fact's runtime session.
    pub session_id: plakous::model::SessionId,
    /// The branch whose record holds the terminal.
    pub branch_id: plakous::model::BranchId,
    /// The authoritative usage key.
    pub turn_id: plakous::model::TurnId,
    /// The terminal record's commit timestamp.
    pub committed_at_ms: u64,
    /// Exact aggregate from the terminal fact.
    pub usage: plakous::agent::TurnUsage,
}

/// What one projector pass did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeProjection {
    /// Newly applied child terminals; publish only after the tree save succeeds.
    pub terminal_usages: Vec<ProjectedChildTurnUsage>,
    /// Records applied (records at or below the cursor are not counted).
    pub applied: usize,
    /// The session's cursor after the pass.
    pub cursor: u64,
    /// Accepted commands that could not bind; the host cancels their turns.
    pub conflicts: Vec<BindConflict>,
    /// Exact terminal evidence in this pass, including explicitly reread records
    /// below the saved cursor, as `(agent_id, terminal)`.
    pub ended_turns: Vec<(String, ChildTerminal)>,
    /// Children whose current attempt the pass settled `turn_abandoned`.
    pub abandoned: Vec<String>,
    /// Semantically refused host consequences; other facts and intents still join.
    pub refused_intents: Vec<IntentJoinRefusal>,
}

impl TreeProjection {
    pub(in crate::tree) fn retain_new_usage(
        &mut self,
        records: &[FeedRecord],
        previous_cursor: u64,
    ) {
        self.applied = records
            .iter()
            .filter(|record| {
                let seq = record.header().seq.get();
                seq > previous_cursor && seq <= self.cursor
            })
            .count();
        // Usage entries follow terminal fact order, not distinct turn ids.
        let mut terminal_sequences = records
            .iter()
            .filter(|record| record.header().seq.get() <= self.cursor)
            .flat_map(|record| {
                record.facts().iter().filter_map(move |fact| {
                    matches!(fact, FeedFact::TurnTerminal { .. })
                        .then_some(record.header().seq.get())
                })
            });
        self.terminal_usages.retain(|_| {
            terminal_sequences
                .next()
                .is_some_and(|seq| seq > previous_cursor)
        });
    }
}

/// A host intent whose derived consequence was refused by the current views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentJoinRefusal {
    /// The immutable intent that was skipped.
    pub operation_id: String,
    /// The typed semantic refusal, not an integrity or decoding failure.
    pub error: TopologyError,
}

/// Why a page was refused; nothing was applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeProjectorError {
    /// A record belongs to another session.
    ForeignSession {
        /// The record's session.
        found: String,
    },
    /// The page is not in ascending sequence order, or skips a record after
    /// the cursor.
    Gap {
        /// The sequence the page needed next.
        expected: u64,
        /// The sequence it held.
        found: u64,
    },
    /// The agent is not a node of this tree.
    UnknownAgent,
    /// A late read needs the session from its start (or its retention seed).
    RefoldRequired,
    /// A derived write was refused by the tree.
    Tree(TopologyError),
}

impl std::fmt::Display for TreeProjectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForeignSession { found } => {
                write!(
                    formatter,
                    "feed record of session {found} applied to another session"
                )
            }
            Self::Gap { expected, found } => write!(
                formatter,
                "feed page skips from sequence {expected} to {found}"
            ),
            Self::UnknownAgent => formatter.write_str("feed applied to an unknown agent"),
            Self::RefoldRequired => {
                formatter.write_str("late read requires a feed reread from the session start")
            }
            Self::Tree(error) => write!(formatter, "tree projection refused: {}", error.message),
        }
    }
}

impl std::error::Error for TreeProjectorError {}

impl From<TopologyError> for TreeProjectorError {
    fn from(error: TopologyError) -> Self {
        Self::Tree(error)
    }
}

fn record_locator(header: &FeedHeader) -> Option<plakous::sessions::SessionLocator> {
    let checkpoint = match header.change {
        FeedChange::Created { genesis, .. } => genesis,
        FeedChange::Advanced { checkpoint, .. } => checkpoint,
        FeedChange::Branched { target, .. } => target,
        FeedChange::LeaseReleased { head, .. } | FeedChange::Pruned { head, .. } => head,
        _ => return None,
    };
    Some(plakous::sessions::SessionLocator::new(
        header.session_id,
        header.branch_id,
        checkpoint,
    ))
}

impl ProductAgentTree {
    pub(in crate::tree) fn project_retained_head_waits(
        &mut self,
        agent_id: &str,
        heads: &[plakous::sessions::SessionLocator],
        reads: &TreeFeedReads,
    ) -> Result<(), TopologyError> {
        for head in heads {
            if let Some(evidence) = reads.waits.get(&head.to_string()) {
                self.settle_wait_receipts(agent_id, evidence)?;
            }
        }
        Ok(())
    }

    /// Records which runtime session the child's create effect allocated.
    /// The association is immutable; only the projector writes its locator view.
    ///
    /// # Errors
    /// Refuses an unknown child, an invalid locator or a different allocation.
    pub fn record_child_session(
        &mut self,
        agent_id: &str,
        locator: &str,
    ) -> Result<(), TopologyError> {
        self.node(agent_id)?;
        if agent_id == self.root_agent_id
            || locator
                .parse::<plakous::sessions::SessionLocator>()
                .is_err()
        {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "child session allocation is invalid",
            ));
        }
        let id = format!("{CHILD_SESSION_KIND}:{agent_id}");
        if let Some(existing) = self.operations.get(&id) {
            let recorded = existing
                .canonical_request
                .parse::<plakous::sessions::SessionLocator>()
                .ok();
            let requested = locator.parse::<plakous::sessions::SessionLocator>().ok();
            if recorded
                .zip(requested)
                .is_some_and(|(recorded, requested)| {
                    recorded.session_id() == requested.session_id()
                        && recorded.branch_id() == requested.branch_id()
                })
            {
                return self.record_session_bound_after_failure(agent_id);
            }
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "child session allocation differs from its recorded create",
            ));
        }
        self.operations.insert(
            id.clone(),
            super::OperationRecord {
                fold: None,
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: CHILD_SESSION_KIND.to_owned(),
                request_digest: super::digest_text(locator),
                canonical_request: locator.to_owned(),
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        self.record_session_bound_after_failure(agent_id)
    }

    pub(super) fn join_child_session(
        &mut self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<(), TopologyError> {
        if self.node(agent_id)?.session_locator.is_some() {
            return Ok(());
        }
        let id = format!("{CHILD_SESSION_KIND}:{agent_id}");
        let Some(operation) = self.operations.get(&id) else {
            return Ok(());
        };
        let locator = operation.canonical_request.clone();
        let parsed = locator
            .parse::<plakous::sessions::SessionLocator>()
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "child session allocation is invalid",
                )
            })?;
        if parsed.session_id().to_string() == session_id {
            self.bind_session_locator(agent_id, locator)?;
        }
        Ok(())
    }

    fn project_locator(
        &mut self,
        agent_id: &str,
        header: &FeedHeader,
    ) -> Result<(), TopologyError> {
        if agent_id == self.root_agent_id {
            return Ok(());
        }
        let Some(current) = self
            .node(agent_id)?
            .session_locator
            .as_deref()
            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
        else {
            return Ok(());
        };
        if current.session_id() != header.session_id || current.branch_id() != header.branch_id {
            return Ok(());
        }
        let checkpoint = match header.change {
            FeedChange::Created { genesis, .. } => genesis,
            FeedChange::Advanced { checkpoint, .. } => checkpoint,
            FeedChange::Branched { target, .. } => target,
            FeedChange::LeaseReleased { head, .. } | FeedChange::Pruned { head, .. } => head,
            _ => return Ok(()),
        };
        let locator =
            plakous::sessions::SessionLocator::new(header.session_id, header.branch_id, checkpoint);
        if locator != current {
            self.advance_session_locator(agent_id, locator.to_string())?;
        }
        Ok(())
    }

    /// Seeds the attempt's binding and active-turn view from a retained head.
    /// This is a head observation, not an admission, so it spends no turn.
    ///
    /// # Errors
    /// Refuses an unknown agent.
    pub fn seed_head_turn(
        &mut self,
        agent_id: &str,
        turn_id: Option<String>,
    ) -> Result<(), TopologyError> {
        self.node(agent_id)?;
        if agent_id == self.root_agent_id {
            return Ok(());
        }
        let active = turn_id.is_some();
        if let Some(attempt) = self.current_attempt_mut(agent_id) {
            attempt.turn_id = turn_id;
        }
        // Observing an unfinished head is not starting another turn. An older
        // close or interrupt requests retirement without erasing that binding.
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.active_turn = active;
        }
        self.bump();
        Ok(())
    }

    /// Applies a page of `session_id`'s feed, the session of `agent_id`
    /// (the root or a child), to this tree's views and advances the session's
    /// cursor. The caller saves the tree afterwards; a save refused for a
    /// stale revision changes nothing, and the next pass applies the same
    /// records again.
    ///
    /// # Errors
    ///
    /// Refuses a foreign or out-of-order page, an unknown agent, and a
    /// derived write the tree refuses. A refused page changes nothing.
    pub fn project_feed(
        &mut self,
        agent_id: &str,
        session_id: &str,
        records: &[FeedRecord],
        reads: &TreeFeedReads,
    ) -> Result<TreeProjection, TreeProjectorError> {
        let mut next = self.clone();
        let mut reads = reads.clone();
        reads.waits.retain(|locator, _| {
            locator
                .parse::<plakous::sessions::SessionLocator>()
                .is_ok_and(|head| {
                    head.session_id().to_string() == session_id
                        && (records
                            .iter()
                            .any(|record| record_locator(record.header()).as_ref() == Some(&head))
                            || (agent_id != next.root_agent_id
                                && next.nodes.get(agent_id).is_some_and(|node| {
                                    node.session_locator.as_ref() == Some(locator)
                                })))
                })
        });
        next.join_child_session(agent_id, session_id)?;
        for record in records {
            let found = record.header().session_id.to_string();
            if found != session_id {
                return Err(TreeProjectorError::ForeignSession { found });
            }
        }
        next.ensure_fold_started(agent_id)?;
        let intents = next.index_fold_intents(agent_id);
        let refused = next.fold_agent_due_indexed(agent_id, Some(&intents))?;
        let mut projection = match next
            .project_page(agent_id, session_id, records, &reads, &intents)
        {
            Err(TreeProjectorError::RefoldRequired)
                if records
                    .first()
                    .is_some_and(|record| record.header().seq.get() == 1)
                    && records.last().is_some_and(|record| {
                        record.header().seq.get() >= self.feed_cursor(session_id)
                    }) =>
            {
                // Refold only on the private scratch tree; a live reader never
                // observes deleted views or records against a reset cursor.
                next = self.clone();
                next.clear_agent_feed_views(agent_id, session_id);
                let mut projection = next.project_feed(agent_id, session_id, records, &reads)?;
                if projection.cursor < self.feed_cursor(session_id) {
                    return Err(TreeProjectorError::RefoldRequired);
                }
                projection.retain_new_usage(records, self.feed_cursor(session_id));
                projection
            }
            result => result?,
        };
        projection.refused_intents.extend(refused);
        projection
            .refused_intents
            .extend(next.fold_reserved_closes_in_place()?);
        let mut refused_ids = BTreeSet::new();
        projection
            .refused_intents
            .retain(|refusal| refused_ids.insert(refusal.operation_id.clone()));
        next.join_mail_decisions(agent_id)?;
        for (locator, evidence) in &reads.waits {
            let authenticated = records.iter().any(|record| {
                let header = record.header();
                let checkpoint = match header.change {
                    FeedChange::Created { genesis, .. } => genesis,
                    FeedChange::Advanced { checkpoint, .. } => checkpoint,
                    FeedChange::Branched { target, .. } => target,
                    FeedChange::LeaseReleased { head, .. } | FeedChange::Pruned { head, .. } => {
                        head
                    }
                    _ => return false,
                };
                plakous::sessions::SessionLocator::new(
                    header.session_id,
                    header.branch_id,
                    checkpoint,
                )
                .to_string()
                    == *locator
                    && header.seq.get() <= projection.cursor
            }) || (agent_id != next.root_agent_id
                && next.node(agent_id)?.session_locator.as_ref() == Some(locator));
            if authenticated {
                next.settle_wait_receipts(agent_id, evidence)?;
            }
        }
        *self = next;
        Ok(projection)
    }

    /// Records a host-decided failure as an intent, never as an attempt view.
    /// The first observation of this attempt and turn stands.
    ///
    /// # Errors
    /// Refuses an unknown agent, missing attempt or unsupported host failure kind.
    pub fn record_host_turn_failure(
        &mut self,
        agent_id: &str,
        disposition: FailedTurnDisposition,
        message: String,
        waits: Vec<(String, bool)>,
    ) -> Result<(), TopologyError> {
        self.require_fold_started(agent_id)?;
        let attempt = self.current_attempt(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "agent has no attempt")
        })?;
        let id = format!(
            "{HOST_TURN_FAILURE_KIND}:{agent_id}:{}:{}",
            attempt.attempt_id,
            attempt.turn_id.as_deref().unwrap_or("unbound")
        );
        if self.operations.contains_key(&id) {
            return Ok(());
        }
        let disposition = match disposition {
            FailedTurnDisposition::Failed {
                kind: TURN_ABANDONED,
            } => HostFailureDisposition::Abandoned,
            FailedTurnDisposition::Failed {
                kind: "turn_failed",
            } => HostFailureDisposition::Failed,
            FailedTurnDisposition::RecoveryRequired => HostFailureDisposition::RecoveryRequired,
            FailedTurnDisposition::Blocked(reason) => HostFailureDisposition::Blocked(reason),
            FailedTurnDisposition::Failed { .. } => {
                return Err(TopologyError::new(
                    TopologyErrorCode::GrantDenied,
                    "runtime failure kinds must come from the feed",
                ));
            }
        };
        let intent = HostTurnFailure {
            attempt_id: attempt.attempt_id.clone(),
            turn_id: attempt.turn_id.clone(),
            disposition,
            message,
            waits,
            observed_at_ms: super::now_unix_millis(),
        };
        let canonical = super::canonical_json(&intent)?;
        self.operations.insert(
            id.clone(),
            super::OperationRecord {
                fold: Some(self.fold_anchor_now(agent_id)?),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: HOST_TURN_FAILURE_KIND.to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(())
    }

    fn remember_command_acceptance(&mut self, agent_id: &str, record: &FeedRecord) {
        for fact in record.facts() {
            if let FeedFact::CommandAccepted { command_id, .. } = fact
                && let Some(command) = self
                    .commands
                    .get_mut(command_id.as_str())
                    .filter(|command| command.recipient == agent_id)
            {
                command.accepted_seq = Some(record.header().seq.get());
            }
        }
    }

    fn join_host_turn_failure(
        &mut self,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), TopologyError> {
        let intents = self
            .operations
            .get(operation_id)
            .into_iter()
            .filter(|operation| {
                operation.operation_id == operation_id
                    && operation.caller_agent_id == agent_id
                    && operation.tool_name == HOST_TURN_FAILURE_KIND
            })
            .map(|operation| {
                serde_json::from_str::<HostTurnFailure>(&operation.canonical_request).map(
                    |intent| {
                        (
                            operation.operation_id.clone(),
                            operation.terminal_response.is_none(),
                            intent,
                        )
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "host failure intent is invalid",
                )
            })?;
        for (id, reserve_effects, intent) in intents {
            if let Some(turn) = &intent.turn_id
                && let Some(attempt) = self.attempts.get_mut(&intent.attempt_id)
            {
                attempt.turn_id = Some(turn.clone());
            }
            let disposition = match intent.disposition {
                HostFailureDisposition::Abandoned => FailedTurnDisposition::Failed {
                    kind: TURN_ABANDONED,
                },
                HostFailureDisposition::Failed => FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                HostFailureDisposition::RecoveryRequired => FailedTurnDisposition::RecoveryRequired,
                HostFailureDisposition::Blocked(reason) => FailedTurnDisposition::Blocked(reason),
            };
            self.record_attempt_failure(
                agent_id,
                Some(&intent.attempt_id),
                disposition,
                intent.message,
                reserve_effects,
                intent.observed_at_ms,
            )?;
            if self.node(agent_id)?.attempt_id.as_ref() == Some(&intent.attempt_id)
                && self.node(agent_id)?.status != NodeStatus::RecoveryRequired
            {
                self.settle_wait_receipts(agent_id, &intent.waits)?;
            }
            if let Some(operation) = self.operations.get_mut(&id)
                && operation.terminal_response.is_none()
            {
                operation.terminal_response = Some("effects_reserved".to_owned());
                self.bump();
            }
        }
        Ok(())
    }

    fn project_page(
        &mut self,
        agent_id: &str,
        session_id: &str,
        records: &[FeedRecord],
        reads: &TreeFeedReads,
        intents: &[fold::FoldIntent],
    ) -> Result<TreeProjection, TreeProjectorError> {
        if !self.nodes.contains_key(agent_id) {
            return Err(TreeProjectorError::UnknownAgent);
        }
        self.join_child_session(agent_id, session_id)?;
        let mut cursor = self.feed_cursor(session_id);
        let mut projection = TreeProjection {
            cursor,
            ..TreeProjection::default()
        };
        for record in records {
            let header = record.header();
            let session = header.session_id.to_string();
            if session != session_id {
                return Err(TreeProjectorError::ForeignSession { found: session });
            }
            let seq = header.seq.get();
            let read_attempt = self.current_attempt(agent_id).and_then(|attempt| {
                let turn = attempt.turn_id.as_deref()?;
                let has_read = reads.terminal(turn).is_some()
                    && (closure_turn(record).is_some_and(|id| id.to_string() == turn)
                        || record.facts().iter().any(|fact| matches!(fact,
                            FeedFact::TurnTerminal { turn_id, outcome: FeedTurnOutcome::Cancelled { .. }, .. }
                                if turn_id.to_string() == turn)));
                has_read.then(|| attempt.attempt_id.clone())
            });
            if seq <= cursor {
                self.project_compactions(agent_id, session_id, record);
                self.remember_command_acceptance(agent_id, record);
                projection.ended_turns.extend(
                    self.child_terminal_evidence(agent_id, record)
                        .into_iter()
                        .map(|terminal| (agent_id.to_owned(), terminal)),
                );
                // A recovery read can arrive after the first terminal pass.
                // Rejoin only its still-bound recovery obligation, not usage.
                if matches!(self.recovery_need(agent_id)?, RecoveryNeed::Session { .. }) {
                    let has_read = record.facts().iter().any(|fact| {
                        matches!(fact,
                        FeedFact::TurnTerminal { turn_id, outcome: FeedTurnOutcome::Cancelled { .. }, .. }
                            if reads.terminal(&turn_id.to_string()).is_some())
                    }) || closure_turn(record).is_some_and(|turn| {
                        reads
                            .terminal(&turn.to_string())
                            .is_some_and(|read| read.abandoned.is_some())
                    });
                    let already_applied = self.attempts.values().any(|attempt| {
                        attempt.agent_id == agent_id
                            && attempt.applied_terminal_read_seqs.contains(&seq)
                    });
                    if has_read
                        && !already_applied
                        && (seq < cursor
                            || self
                                .node(agent_id)?
                                .fold_order
                                .is_some_and(|order| order > 0))
                    {
                        return Err(TreeProjectorError::RefoldRequired);
                    }
                    if already_applied {
                        continue;
                    }
                    for fact in record.facts() {
                        if let FeedFact::TurnTerminal {
                            turn_id,
                            command_id,
                            outcome,
                            closed_exchange,
                            ..
                        } = fact
                            && reads.terminal(&turn_id.to_string()).is_some()
                        {
                            self.apply_terminal(
                                agent_id,
                                &turn_id.to_string(),
                                command_id.as_str(),
                                *outcome,
                                *closed_exchange,
                                header.committed_at_ms,
                                reads,
                                &mut projection,
                            )?;
                        }
                    }
                    if let Some(turn_id) = closure_turn(record) {
                        self.apply_closure(
                            agent_id,
                            &turn_id.to_string(),
                            header.committed_at_ms,
                            reads,
                            &mut projection,
                        )?;
                    }
                    if let Some(attempt) = read_attempt.and_then(|id| self.attempts.get_mut(&id)) {
                        attempt.applied_terminal_read_seqs.insert(seq);
                        self.bump();
                    }
                }
                continue;
            }
            if seq != cursor + 1 {
                return Err(TreeProjectorError::Gap {
                    expected: cursor + 1,
                    found: seq,
                });
            }
            // The live waiter records its host observation before anyone
            // settles this failure. Do not move the cursor past that join.
            if self.has_turn_waiter(agent_id)
                && record.facts().iter().any(|fact| {
                    matches!(fact, FeedFact::TurnTerminal {
                        turn_id, outcome: FeedTurnOutcome::Failed { .. }, ..
                    } if self.current_attempt(agent_id).is_some_and(|attempt| {
                        attempt.turn_id.as_deref() == Some(turn_id.to_string().as_str())
                            && !self.operations.contains_key(&failed_turn_notice_id(
                                agent_id, attempt.attempt_id.as_str(), &turn_id.to_string(),
                            ))
                    }))
                })
            {
                break;
            }
            self.project_locator(agent_id, header)?;
            self.remember_command_acceptance(agent_id, record);
            projection.ended_turns.extend(
                self.child_terminal_evidence(agent_id, record)
                    .into_iter()
                    .map(|terminal| (agent_id.to_owned(), terminal)),
            );
            // Fact-reserved effects belong to this record, not its predecessor.
            self.feed_cursors.insert(session_id.to_owned(), seq);
            if let Some(node) = self.nodes.get_mut(agent_id) {
                node.fold_order = Some(0);
            }
            for fact in record.facts() {
                self.apply_fact(
                    agent_id,
                    header.committed_at_ms,
                    fact,
                    reads,
                    &mut projection,
                )?;
                if agent_id != self.root_agent_id
                    && let FeedFact::TurnTerminal { turn_id, usage, .. } = fact
                {
                    projection.terminal_usages.push(ProjectedChildTurnUsage {
                        session_id: header.session_id,
                        branch_id: header.branch_id,
                        turn_id: *turn_id,
                        committed_at_ms: header.committed_at_ms,
                        usage: *usage,
                    });
                }
            }
            if let Some(turn_id) = closure_turn(record) {
                self.apply_closure(
                    agent_id,
                    &turn_id.to_string(),
                    header.committed_at_ms,
                    reads,
                    &mut projection,
                )?;
            }
            self.project_compactions(agent_id, session_id, record);
            if let Some(attempt) = read_attempt.and_then(|id| self.attempts.get_mut(&id)) {
                attempt.applied_terminal_read_seqs.insert(seq);
                self.bump();
            }
            projection
                .refused_intents
                .extend(self.fold_after_record(agent_id, session_id, seq, intents)?);
            cursor = seq;
            projection.applied += 1;
        }
        if cursor != self.feed_cursor(session_id) {
            self.feed_cursors.insert(session_id.to_owned(), cursor);
            self.bump();
        }
        projection.cursor = cursor;
        Ok(projection)
    }

    fn apply_fact(
        &mut self,
        agent_id: &str,
        committed_at_ms: u64,
        fact: &FeedFact,
        reads: &TreeFeedReads,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        match fact {
            FeedFact::HistoryAppended { tool_results, .. } => {
                let committed: Vec<(String, bool)> = tool_results
                    .iter()
                    .map(|call| (call.to_string(), true))
                    .collect();
                self.settle_wait_receipts(agent_id, &committed)?;
            }
            FeedFact::CommandAccepted {
                command_id,
                turn_id,
                ..
            } => self.apply_accepted(
                agent_id,
                command_id.as_str(),
                turn_id.map(|turn| turn.to_string()),
                projection,
            )?,
            FeedFact::CommandRejected { command_id, .. } => {
                if let Some(command) = self
                    .commands
                    .get_mut(command_id.as_str())
                    .filter(|command| command.recipient == agent_id && command.fact.is_none())
                {
                    command.fact = Some(CommandFact::Rejected);
                    self.remove_command_retention_marker(command_id.as_str());
                    self.bump();
                }
            }
            FeedFact::TurnTerminal {
                turn_id,
                command_id,
                outcome,
                closed_exchange,
                usage,
                ..
            } => {
                self.fold_turn_usage(
                    agent_id,
                    &turn_id.to_string(),
                    command_id.as_str(),
                    *usage,
                    committed_at_ms,
                );
                self.apply_terminal(
                    agent_id,
                    &turn_id.to_string(),
                    command_id.as_str(),
                    *outcome,
                    *closed_exchange,
                    committed_at_ms,
                    reads,
                    projection,
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_accepted(
        &mut self,
        agent_id: &str,
        command_id: &str,
        turn_id: Option<String>,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        let Some(command) = self.commands.get_mut(command_id).filter(|command| {
            command.recipient == agent_id
                && (command.fact.is_none()
                    || (command.fact == Some(CommandFact::Accepted) && command.turn_id.is_none()))
        }) else {
            return Ok(());
        };
        // A command the runtime admitted carried its batch into history, so
        // its batch is acknowledged even if the tree had withdrawn it.
        command.fact = Some(CommandFact::Accepted);
        command.turn_id.clone_from(&turn_id);
        let attempt_id = command.attempt_id.clone();
        let followup = command.kind == AgentCommandKind::Followup;
        let message_ids = command.message_ids.clone();
        let corrective_round = (command.kind == AgentCommandKind::Corrective)
            .then(|| corrective_round(command_id))
            .flatten();
        self.remove_command_retention_marker(command_id);
        self.bump();
        self.acknowledge_admitted_mail(command_id, agent_id, &message_ids)?;
        let (Some(attempt_id), Some(turn_id)) = (attempt_id, turn_id) else {
            return Ok(());
        };
        let already_counted = self.commands.values().any(|command| {
            command.command_id != command_id
                && command.attempt_id.as_ref() == Some(&attempt_id)
                && command.turn_id.as_deref() == Some(turn_id.as_str())
        });
        if !already_counted && let Some(attempt) = self.attempts.get_mut(&attempt_id) {
            attempt.usage.turns = attempt.usage.turns.saturating_add(1);
        }
        if let Some(nudge) = corrective_round
            && let Some(attempt) = self.attempts.get_mut(&attempt_id)
        {
            attempt.submit_harness_nudge = nudge;
        }
        let node_attempt = self.node(agent_id)?.attempt_id.clone();
        // A command written for an earlier attempt never binds the current one.
        if node_attempt.as_ref() != Some(&attempt_id) {
            if let Some(attempt) = self.attempts.get_mut(&attempt_id) {
                attempt.turn_id = Some(turn_id);
            }
            return Ok(());
        }
        let Some(attempt) = self.current_attempt(agent_id) else {
            return Ok(());
        };
        // An interrupt or close intent outranks the binding: the recovered
        // turn is cancelled, never resumed onto the node.
        let status = self.node(agent_id)?.status;
        if attempt.status.is_stopped() || status.is_terminal() || status == NodeStatus::Interrupted
        {
            if let Some(attempt) = self.current_attempt_mut(agent_id) {
                attempt.turn_id = Some(turn_id);
            }
            return Ok(());
        }
        let active = self.node(agent_id)?.active_turn;
        match attempt.turn_id.clone() {
            Some(bound) if bound == turn_id => {
                if !active {
                    self.set_active_turn(agent_id, true)?;
                }
            }
            Some(bound) if active && !followup => {
                projection.conflicts.push(BindConflict {
                    agent_id: agent_id.to_owned(),
                    command_id: command_id.to_owned(),
                    bound_turn_id: bound,
                    recovered_turn_id: turn_id,
                });
                if let Some(node) = self.nodes.get_mut(agent_id)
                    && !node.status.is_finished()
                {
                    node.status = NodeStatus::RecoveryRequired;
                }
                self.record_fault(
                    agent_id,
                    "an accepted command named a second turn for an attempt with a running turn",
                );
                self.bump();
            }
            _ => {
                if let Some(attempt) = self.current_attempt_mut(agent_id) {
                    attempt.turn_id = Some(turn_id);
                }
                self.set_active_turn(agent_id, true)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments, reason = "one terminal fact's fields")]
    fn apply_terminal(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        command_id: &str,
        outcome: FeedTurnOutcome,
        closed_exchange: bool,
        committed_at_ms: u64,
        reads: &TreeFeedReads,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        // Wait settlement (spec 03 §4.4).
        match outcome {
            FeedTurnOutcome::RecoveryRequired => {}
            _ if closed_exchange => {
                self.retire_wait_receipts(agent_id);
            }
            _ => {
                let unclosed: Vec<(String, bool)> = self
                    .retained_wait_receipts(agent_id)
                    .into_iter()
                    .map(|id| {
                        // Retention can drop HistoryAppended but retain this
                        // terminal. Its quiescent head proves committed calls.
                        let committed = reads
                            .waits
                            .values()
                            .flatten()
                            .any(|(call, committed)| *call == id && *committed);
                        (id, committed)
                    })
                    .collect();
                self.settle_wait_receipts(agent_id, &unclosed)?;
            }
        }
        // The withheld-mail return (spec 03 §8).
        let withheld = matches!(
            outcome,
            FeedTurnOutcome::Failed {
                context: Some(context),
                ..
            } if context.withheld
        );
        if withheld
            && let Some(command) = self.commands.get(command_id).filter(|command| {
                command.recipient == agent_id && command.fact == Some(CommandFact::Accepted)
            })
        {
            let message_ids = command.message_ids.clone();
            self.return_admitted_mail(command_id, agent_id, &message_ids)?;
            if let Some(command) = self.commands.get_mut(command_id) {
                command.fact = Some(CommandFact::Returned);
            }
            self.bump();
        }
        if agent_id == self.root_agent_id {
            return Ok(());
        }
        if let Some(attempt_id) = self
            .commands
            .get(command_id)
            .filter(|command| command.recipient == agent_id)
            .and_then(|command| command.attempt_id.clone())
            && self.node(agent_id)?.attempt_id.as_ref() != Some(&attempt_id)
        {
            if let FeedTurnOutcome::Failed {
                kind, model_kind, ..
            } = outcome
            {
                self.settle_failed(
                    agent_id,
                    &attempt_id,
                    turn_id,
                    (kind, model_kind),
                    committed_at_ms,
                    reads.failure(turn_id),
                )?;
            }
            if outcome != FeedTurnOutcome::RecoveryRequired
                && let Some(attempt) = self.attempts.get_mut(&attempt_id)
                && attempt.turn_id.as_deref() == Some(turn_id)
            {
                attempt.turn_id = None;
                self.bump();
            }
            return Ok(());
        }
        // The child's bound turn ended.
        let Some(attempt) = self.current_attempt(agent_id) else {
            return Ok(());
        };
        if attempt.turn_id.as_deref() != Some(turn_id) {
            return Ok(());
        }
        let attempt_id = attempt.attempt_id.clone();
        let stopped = attempt.status.is_stopped();
        if self.node(agent_id)?.active_turn {
            self.set_active_turn(agent_id, false)?;
        }
        if let FeedTurnOutcome::Failed {
            kind, model_kind, ..
        } = outcome
        {
            if !self.has_transient_resume(agent_id, &attempt_id, turn_id)? {
                self.settle_failed(
                    agent_id,
                    &attempt_id,
                    turn_id,
                    (kind, model_kind),
                    committed_at_ms,
                    reads.failure(turn_id),
                )?;
            }
            if let Some(attempt) = self.current_attempt_mut(agent_id) {
                attempt.turn_id = None;
            }
            self.bump();
            return Ok(());
        }
        // A lost owner's turn closed by a reopen settles `turn_abandoned`; so
        // does a resumable turn a recovery reopen cancelled, for a node
        // waiting on recovery. A live interrupt's cancel settles nothing here.
        let recovering = matches!(self.recovery_need(agent_id)?, RecoveryNeed::Session { .. });
        let fallback = match outcome {
            FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Abandoned { .. },
            } => format!("plak: {agent_id} recovered an interrupted turn"),
            FeedTurnOutcome::Cancelled { .. } if recovering => format!(
                "plak: {agent_id} recovered a resumable turn and cancelled it; nothing from it \
                 is replayed"
            ),
            FeedTurnOutcome::RecoveryRequired => return Ok(()),
            _ => {
                if let Some(attempt) = self.current_attempt_mut(agent_id) {
                    attempt.turn_id = None;
                }
                self.bump();
                return Ok(());
            }
        };
        self.settle_abandoned(
            agent_id,
            &attempt_id,
            stopped,
            recovering,
            fallback,
            reads.terminal(turn_id),
            committed_at_ms,
            projection,
        )
    }

    /// A failed terminal of a running (or submitted) attempt's bound turn ends
    /// the attempt (spec 03 §10, §14.3): the model error's kind and the read's
    /// retryability classify it, a block's `resets_at` counts from the
    /// record's commit, and the end stamp is that commit. The message is the
    /// read error's, joined with the host's failed-turn notice. A terminal
    /// whose read is gone still ends the attempt, on the fact's kinds alone.
    /// An attempt already settled (stopped, recovering, replayed) is left; a
    /// closed node's running attempt still records its failure.
    fn settle_failed(
        &mut self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
        turn_id: &str,
        (kind, model_kind): (
            plakous::agent::AgentErrorKind,
            Option<plakous::model::ModelErrorKind>,
        ),
        committed_at_ms: u64,
        read: Option<&FailureRead>,
    ) -> Result<(), TopologyError> {
        let Some(attempt) = self.attempts.get(attempt_id).filter(|attempt| {
            match attempt.status {
                AttemptStatus::Running | AttemptStatus::Submitted => true,
                // A close fenced the node while the failed turn unwound.
                AttemptStatus::Closed => attempt.failure.is_none() && attempt.blocked.is_none(),
                _ => false,
            }
        }) else {
            return Ok(());
        };
        let attempt_id = attempt.attempt_id.clone();
        let refusal = model_kind.and_then(|model_kind| {
            classify_provider_refusal(
                model_kind,
                read.and_then(|read| read.retryability),
                committed_at_ms,
            )
        });
        let disposition = match refusal {
            Some(TurnRefusal::Blocked { quota, resets_at }) => {
                FailedTurnDisposition::Blocked(BlockedReason {
                    quota: quota.into(),
                    account: self.node(agent_id)?.account.clone(),
                    resets_at,
                })
            }
            Some(TurnRefusal::Failure { kind }) => FailedTurnDisposition::Failed { kind },
            None if model_kind == Some(plakous::model::ModelErrorKind::ContextLimit) => {
                FailedTurnDisposition::Failed {
                    kind: "context_exhausted",
                }
            }
            None => FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
        };
        let mut message = match read {
            Some(read) => {
                let mut detail = format!("child turn failed: {:?}", read.kind);
                if let Some(phase) = read.phase {
                    detail.push_str(&format!(" at {phase:?}"));
                }
                let mut provider = Vec::new();
                if let Some(status) = read.http_status {
                    provider.push(format!("HTTP {}", status.get()));
                }
                if let Some(code) = &read.provider_code {
                    provider.push(code.clone());
                }
                if !provider.is_empty() {
                    detail.push_str(&format!(" ({})", provider.join(" ")));
                }
                format!("{detail}: {}", read.message)
            }
            None => format!("child turn failed: {kind:?}"),
        };
        if matches!(
            disposition,
            FailedTurnDisposition::Failed {
                kind: "context_exhausted" | "refused"
            }
        ) {
            message.push_str(
                "; respawn with a handoff of completed work and the remaining task, not a \
                 follow-up into the same session",
            );
        }
        if let Some(notice) = self
            .operations
            .get(&failed_turn_notice_id(
                agent_id,
                attempt_id.as_str(),
                turn_id,
            ))
            .filter(|operation| operation.tool_name == FAILED_TURN_NOTICE_KIND)
            .and_then(|operation| operation.terminal_response.as_deref())
        {
            message.push_str(notice);
        }
        if let Some(attempt) = self.attempts.get_mut(&attempt_id) {
            attempt.ended_at.get_or_insert(committed_at_ms);
        }
        self.record_attempt_failure(
            agent_id,
            Some(&attempt_id),
            disposition,
            message,
            true,
            committed_at_ms,
        )
    }

    /// Records what the host observed when `turn_id` of `attempt_id` failed
    /// (its failed-worktree snapshot line), before the turn's terminal is
    /// projected: the projector joins it into the failure message on every
    /// pass. The first notice of a turn stands.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent.
    pub fn record_failed_turn_notice(
        &mut self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
        turn_id: &str,
        notice: String,
    ) -> Result<(), TopologyError> {
        self.node(agent_id)?;
        let id = failed_turn_notice_id(agent_id, attempt_id.as_str(), turn_id);
        if self.operations.contains_key(&id) {
            return Ok(());
        }
        let canonical = super::canonical_json(&notice)?;
        self.operations.insert(
            id.clone(),
            super::OperationRecord {
                fold: None,
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: FAILED_TURN_NOTICE_KIND.to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: Some(notice),
            },
        );
        self.bump();
        Ok(())
    }

    /// Folds a child turn's usage onto the attempt its command served (spec
    /// 03 §10, §14.3): tool calls and tokens add up, and the attempt's elapsed
    /// time reaches at least the terminal's commit. The turn itself was
    /// counted when its command bound it. A turn no recorded command of
    /// `agent_id` started folds onto the current attempt only while that
    /// attempt binds it; the root's turns fold nothing.
    fn fold_turn_usage(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        command_id: &str,
        usage: plakous::agent::TurnUsage,
        committed_at_ms: u64,
    ) {
        if agent_id == self.root_agent_id {
            return;
        }
        let commanded = self
            .commands
            .get(command_id)
            .filter(|command| command.recipient == agent_id)
            .and_then(|command| command.attempt_id.clone());
        let Some(attempt_id) = commanded.or_else(|| {
            self.current_attempt(agent_id)
                .filter(|attempt| attempt.turn_id.as_deref() == Some(turn_id))
                .map(|attempt| attempt.attempt_id.clone())
        }) else {
            return;
        };
        let Some(attempt) = self.attempts.get_mut(&attempt_id) else {
            return;
        };
        let model = usage.model();
        let row = &mut attempt.usage;
        row.tool_calls = row
            .tool_calls
            .saturating_add(u32::try_from(usage.local_tool_attempts()).unwrap_or(u32::MAX));
        row.input_tokens = row.input_tokens.saturating_add(model.input_tokens());
        row.output_tokens = row.output_tokens.saturating_add(model.output_tokens());
        row.elapsed_ms = row
            .elapsed_ms
            .max(committed_at_ms.saturating_sub(attempt.started_at));
        self.bump();
    }

    /// A recovery closure of the bound turn of a child waiting on recovery
    /// settles `turn_abandoned` when its terminal read reports the
    /// abandonment.
    fn apply_closure(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        committed_at_ms: u64,
        reads: &TreeFeedReads,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        let Some(read) = reads
            .terminal(turn_id)
            .filter(|read| read.abandoned.is_some())
        else {
            return Ok(());
        };
        if agent_id == self.root_agent_id
            || !matches!(self.recovery_need(agent_id)?, RecoveryNeed::Session { .. })
        {
            return Ok(());
        }
        let Some(attempt) = self
            .current_attempt(agent_id)
            .filter(|attempt| attempt.turn_id.as_deref() == Some(turn_id))
        else {
            return Ok(());
        };
        let attempt_id = attempt.attempt_id.clone();
        let stopped = attempt.status.is_stopped();
        if self.node(agent_id)?.active_turn {
            self.set_active_turn(agent_id, false)?;
        }
        self.settle_abandoned(
            agent_id,
            &attempt_id,
            stopped,
            true,
            String::new(),
            Some(read),
            committed_at_ms,
            projection,
        )
    }

    #[allow(clippy::too_many_arguments, reason = "one settlement's inputs")]
    fn settle_abandoned(
        &mut self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
        stopped: bool,
        recovering: bool,
        fallback: String,
        read: Option<&TerminalRead>,
        committed_at_ms: u64,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        let message = read.map_or(fallback, |read| read.message.clone());
        if recovering {
            self.apply_host_recovery(
                agent_id,
                &host_recovery::HostRecovery {
                    attempt_id: attempt_id.clone(),
                    resolution: RecoveryResolution::TurnAbandoned { message },
                    observed_at_ms: committed_at_ms,
                    closed: self
                        .current_attempt(agent_id)
                        .is_some_and(|attempt| attempt.status == AttemptStatus::Closed),
                    submitted_retirement: false,
                },
                true,
            )?;
        } else if !stopped
            && self.node(agent_id)?.status != NodeStatus::Closed
            && self
                .current_attempt(agent_id)
                .is_some_and(|attempt| attempt.status == AttemptStatus::Running)
        {
            self.record_attempt_failure(
                agent_id,
                Some(attempt_id),
                FailedTurnDisposition::Failed {
                    kind: TURN_ABANDONED,
                },
                message,
                true,
                committed_at_ms,
            )?;
        } else {
            return Ok(());
        }
        projection.abandoned.push(agent_id.to_owned());
        if let Some(abandoned) = read.and_then(|read| read.abandoned.clone()) {
            self.record_abandoned_turn(agent_id, abandoned)?;
        }
        Ok(())
    }
}

/// The error a refused projection reports through the topology tools.
impl From<TreeProjectorError> for TopologyError {
    fn from(error: TreeProjectorError) -> Self {
        match error {
            TreeProjectorError::Tree(error) => error,
            other => Self::new(
                TopologyErrorCode::OperationRecoveryRequired,
                other.to_string(),
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
