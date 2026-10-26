//! The content-independent snapshot-and-tail attach contract.

use super::super::types::{MAX_LIFETIME_IDENTITIES, ToolAuditIdentity};
use super::super::{
    AgentEvent, AgentEventKind, EventCursor, EventDelivery, EventLostRange, EventVisibility,
};
use crate::EffectDecisionRef;
use plakous_model::{Timestamp, TurnId};
use plakous_tools::EffectDecisionPrompt;
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use thiserror::Error;

/// Lifecycle of the process-local event source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AgentViewState {
    /// The actor may accept or finish work.
    Running,
    /// The event source has closed.
    Closed,
}

/// Lifecycle of one observed turn, not a substitute for its authoritative result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnViewState {
    /// The turn is active.
    Running,
    /// The turn completed after semantic commitment.
    Completed,
    /// The turn failed or was cancelled.
    Failed,
}

/// A turn identity and lifecycle at the attach boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnView {
    turn_id: TurnId,
    state: TurnViewState,
}

impl TurnView {
    /// Returns the turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }
    /// Returns the observed lifecycle state.
    #[must_use]
    pub const fn state(&self) -> TurnViewState {
        self.state
    }
}

/// A binding-addressed decision still pending at the attach boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingEffectDecisionView {
    identity: ToolAuditIdentity,
    decision: EffectDecisionRef,
    prompt: Option<EffectDecisionPrompt>,
    expires_at: Timestamp,
}

impl PendingEffectDecisionView {
    /// Returns the canonical/exposed tool identity.
    #[must_use]
    pub const fn identity(&self) -> &ToolAuditIdentity {
        &self.identity
    }
    /// Returns the exact binding-addressed decision reference.
    #[must_use]
    pub const fn decision(&self) -> &EffectDecisionRef {
        &self.decision
    }
    /// Returns descriptive content only under content visibility.
    #[must_use]
    pub const fn prompt(&self) -> Option<&EffectDecisionPrompt> {
        self.prompt.as_ref()
    }
    /// Returns the display timestamp for the monotonic deadline.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::event) struct AttachState {
    pub(in crate::event) lifecycle: AgentViewState,
    pub(in crate::event) turns: Vec<TurnView>,
    pending: Vec<PendingEffectDecisionView>,
    completed: VecDeque<TurnId>,
    terminal: BTreeSet<TurnId>,
    retained_turns: usize,
}

pub(super) enum AttachDelta {
    Close,
    Turn(TurnView),
    Required(Box<PendingEffectDecisionView>),
    Resolved(EffectDecisionRef),
    Noop,
}

impl AttachState {
    pub(in crate::event) fn new(retained_turns: usize) -> Self {
        Self {
            lifecycle: AgentViewState::Running,
            turns: Vec::new(),
            pending: Vec::new(),
            completed: VecDeque::new(),
            terminal: BTreeSet::new(),
            retained_turns,
        }
    }

    pub(super) fn plan(&self, event: &AgentEvent) -> Result<AttachDelta, ReducerError> {
        if self.lifecycle == AgentViewState::Closed {
            return Err(ReducerError::InvalidLifecycle);
        }
        match event.kind() {
            AgentEventKind::AgentClosed => Ok(AttachDelta::Close),
            AgentEventKind::TurnStarted => {
                let turn_id = event.turn_id.ok_or(ReducerError::InvalidLifecycle)?;
                let running = self
                    .turns
                    .iter()
                    .filter(|turn| turn.state == TurnViewState::Running)
                    .count();
                if self.terminal.contains(&turn_id)
                    || self.turns.iter().any(|turn| turn.turn_id == turn_id)
                    || running >= self.retained_turns
                {
                    return Err(ReducerError::InvalidLifecycle);
                }
                // Reserve the terminal fence before admitting another live identity.
                if self.terminal.len().saturating_add(running) >= MAX_LIFETIME_IDENTITIES {
                    return Err(ReducerError::FenceSaturated);
                }
                Ok(AttachDelta::Turn(TurnView {
                    turn_id,
                    state: TurnViewState::Running,
                }))
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
                let turn_id = event.turn_id.ok_or(ReducerError::InvalidLifecycle)?;
                if self.terminal.contains(&turn_id) {
                    return Err(ReducerError::InvalidLifecycle);
                }
                if self.terminal.len() >= MAX_LIFETIME_IDENTITIES {
                    return Err(ReducerError::FenceSaturated);
                }
                let state = if matches!(event.kind(), AgentEventKind::TurnCompleted { .. }) {
                    TurnViewState::Completed
                } else {
                    TurnViewState::Failed
                };
                Ok(AttachDelta::Turn(TurnView { turn_id, state }))
            }
            AgentEventKind::EffectDecisionRequired {
                identity,
                decision,
                prompt,
                expires_at,
            } => {
                if !self.turns.iter().any(|turn| {
                    turn.turn_id == decision.turn_id() && turn.state == TurnViewState::Running
                }) {
                    return Err(ReducerError::InvalidLifecycle);
                }
                Ok(
                    if self
                        .pending
                        .iter()
                        .any(|pending| pending.decision == *decision)
                    {
                        AttachDelta::Noop
                    } else {
                        AttachDelta::Required(Box::new(PendingEffectDecisionView {
                            identity: identity.clone(),
                            decision: decision.clone(),
                            prompt: prompt.clone(),
                            expires_at: *expires_at,
                        }))
                    },
                )
            }
            AgentEventKind::EffectDecisionResolved { decision, .. } => {
                Ok(AttachDelta::Resolved(decision.clone()))
            }
            _ => Ok(AttachDelta::Noop),
        }
    }

    pub(super) fn apply(&mut self, delta: AttachDelta) {
        match delta {
            AttachDelta::Close => {
                self.lifecycle = AgentViewState::Closed;
                self.pending.clear();
            }
            AttachDelta::Turn(turn) => {
                let turn_id = turn.turn_id;
                if turn.state != TurnViewState::Running {
                    self.pending
                        .retain(|pending| pending.decision.turn_id() != turn_id);
                    self.terminal.insert(turn_id);
                    self.completed.push_back(turn_id);
                }
                match self.turns.binary_search_by_key(&turn_id, TurnView::turn_id) {
                    Ok(index) => self.turns[index] = turn,
                    Err(index) => self.turns.insert(index, turn),
                }
                while self.completed.len() > self.retained_turns {
                    if let Some(evicted) = self.completed.pop_front()
                        && let Ok(index) =
                            self.turns.binary_search_by_key(&evicted, TurnView::turn_id)
                    {
                        self.turns.remove(index);
                    }
                }
            }
            AttachDelta::Required(decision) => self.pending.push(*decision),
            AttachDelta::Resolved(decision) => {
                self.pending.retain(|pending| pending.decision != decision);
            }
            AttachDelta::Noop => {}
        }
    }
}

/// Atomic process-local attach state through one cursor.
///
/// Contains lifecycle and pending decisions, not transcript, usage, tool activity,
/// or presentation counters. The registered tail starts strictly after the cursor.
/// Read committed history through the facade's atomic transcript attachment.
///
/// Presentation caches are intentionally not part of this contract:
///
/// ```compile_fail
/// fn presentation(snapshot: plakous_agent::AgentViewSnapshot) {
///     let _ = snapshot.tool_activity();
/// }
/// ```
///
/// ```compile_fail
/// fn history(snapshot: plakous_agent::AgentViewSnapshot) {
///     let _ = snapshot.conversation();
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct AgentViewSnapshot {
    pub(in crate::event) cursor: EventCursor,
    pub(in crate::event) presentation_incomplete: bool,
    attach: AttachState,
}

impl AgentViewSnapshot {
    pub(in crate::event) fn new(cursor: EventCursor, mut attach: AttachState) -> Self {
        if cursor.visibility() == EventVisibility::Metadata {
            for decision in &mut attach.pending {
                decision.prompt = None;
            }
        }
        Self {
            cursor,
            presentation_incomplete: false,
            attach,
        }
    }
    /// Returns the exact snapshot/tail boundary and observation scope.
    #[must_use]
    pub const fn cursor(&self) -> EventCursor {
        self.cursor
    }
    /// Returns the event source lifecycle.
    #[must_use]
    pub const fn state(&self) -> AgentViewState {
        self.attach.lifecycle
    }
    /// Reports skipped presentation-only observations, not missing attach state.
    #[must_use]
    pub const fn presentation_incomplete(&self) -> bool {
        self.presentation_incomplete
    }
    /// Returns active and bounded recently terminal turns in identity order.
    #[must_use]
    pub fn turns(&self) -> &[TurnView] {
        &self.attach.turns
    }
    /// Returns all currently pending decisions in admission order.
    #[must_use]
    pub fn pending_effect_decisions(&self) -> &[PendingEffectDecisionView] {
        &self.attach.pending
    }
}

impl fmt::Debug for AgentViewSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentViewSnapshot")
            .field("cursor", &self.cursor)
            .field("state", &self.state())
            .field("presentation_incomplete", &self.presentation_incomplete)
            .field("turns", &self.turns())
            .field("pending_decision_count", &self.attach.pending.len())
            .finish()
    }
}

/// Error advancing an attach snapshot over its observation tail.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ReducerError {
    /// Event and snapshot scopes differ.
    #[error("event is incompatible with the attach scope")]
    IncompatibleScope,
    /// The event is not the exact next sequence.
    #[error("event sequence is not the exact next attach sequence")]
    NonContiguous,
    /// An event extends a terminal source or turn.
    #[error("event extends a terminal attach lifecycle")]
    InvalidLifecycle,
    /// The process-local lifetime fence requires a new session.
    #[error("event lifetime fence requires a new session")]
    FenceSaturated,
}

/// Advances only the attach contract; hosts own all presentation reduction.
#[derive(Clone, Debug)]
pub struct AgentViewReducer {
    snapshot: AgentViewSnapshot,
}

impl AgentViewReducer {
    /// Starts from an atomic attachment.
    #[must_use]
    pub const fn from_snapshot(snapshot: AgentViewSnapshot) -> Self {
        Self { snapshot }
    }
    /// Advances lifecycle and pending decisions over an exact next event.
    ///
    /// # Errors
    /// Rejects incompatible scopes, discontinuities and invalid attach lifecycles
    /// without changing state. Event payload validity belongs to the publisher.
    pub fn apply(&mut self, event: &AgentEvent) -> Result<(), ReducerError> {
        let cursor = self.snapshot.cursor;
        let mut expected = event.cursor();
        expected.sequence = cursor.sequence;
        if expected != cursor {
            return Err(ReducerError::IncompatibleScope);
        }
        if cursor.sequence.0.checked_add(1) != Some(event.branch_sequence.0) {
            return Err(ReducerError::NonContiguous);
        }
        let delta = self.snapshot.attach.plan(event)?;
        self.snapshot.attach.apply(delta);
        self.snapshot.cursor = event.cursor();
        Ok(())
    }
    /// Advances over an exact event or an adjacent presentation-only loss range.
    ///
    /// # Errors
    /// Rejects incompatible or discontinuous deliveries without mutation.
    pub fn apply_delivery(&mut self, delivery: &EventDelivery) -> Result<(), ReducerError> {
        match delivery {
            EventDelivery::Event(event) => self.apply(event),
            EventDelivery::Lost(lost) => self.apply_lost(*lost),
        }
    }
    /// Marks omitted presentation observations while preserving attach state.
    ///
    /// # Errors
    /// Rejects closed sources and incompatible or discontinuous ranges.
    pub fn apply_lost(&mut self, lost: EventLostRange) -> Result<(), ReducerError> {
        if self.snapshot.state() == AgentViewState::Closed {
            return Err(ReducerError::InvalidLifecycle);
        }
        let cursor = self.snapshot.cursor;
        if lost.after != cursor {
            return Err(ReducerError::IncompatibleScope);
        }
        let expected = cursor
            .sequence
            .0
            .checked_add(1)
            .ok_or(ReducerError::NonContiguous)?;
        if lost.first().0 != expected || lost.through.0 < expected {
            return Err(ReducerError::NonContiguous);
        }
        self.snapshot.presentation_incomplete = true;
        self.snapshot.cursor = lost.cursor();
        Ok(())
    }
    /// Returns the current attach state.
    #[must_use]
    pub const fn snapshot(&self) -> &AgentViewSnapshot {
        &self.snapshot
    }
    /// Returns the final attach state.
    #[must_use]
    pub fn into_snapshot(self) -> AgentViewSnapshot {
        self.snapshot
    }
}
