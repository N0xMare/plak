//! Agent event fan-out and the turn projection it maintains.

mod compaction;
mod compose;
mod decisions;
mod model;
mod notes;
mod projection;
mod reasoning;
mod tools;

use super::admission::commit_session_update;
use super::logging::log_event;
// Row derivations the feed projector shares with the live path.
pub(crate) use super::tool_facts::{wait_settled, wait_started};
use super::turn::flush_assistant_delta;
pub(crate) use super::turn::operator_facing_agent_error;
use super::{HostEvent, Sender, ToolSubjectStash, TopologyRows, TreeSnapshotPush, TurnProjection};
use crate::session_home::SessionHome;
use crate::session_log::{SessionEvent, SessionLog};
use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
pub(super) use crate::sessions::live_tool_subject;
use plakous::agent::{AgentEvent, AgentEventKind};
use std::time::Instant;

pub(crate) use compaction::compact_terminal_subject;
pub(super) use compaction::{
    compact_row_token_pair, compact_terminal_state, compaction_summary_clause,
    publish_compact_activity,
};
pub(crate) use compose::{compose_counts_subject, host_owns_subject, projected_tool_subject};
pub(super) use decisions::resolve_approval_intent;
pub(crate) use notes::{
    abandoned_turn_operator_detail, abandoned_turn_operator_line, output_bound_rederived_note,
};
pub(super) use reasoning::{
    reasoning_location_key, reasoning_visibility_admits, settle_live_thinking,
};
pub(super) use tools::{
    ProcessContext, StartedToolText, completed_tool_summary, observe_cancelled_tool_activity,
    observe_terminal_tool_activity, presented_later_name, presented_start_name,
    settled_process_wait_subject, started_tool_text,
};
pub(crate) use tools::{completed_tool_detail, completed_tool_state};

pub(super) fn timestamp_ms(timestamp: plakous::model::Timestamp) -> u64 {
    timestamp
        .seconds()
        .saturating_mul(1_000)
        .saturating_add(u64::from(timestamp.nanoseconds()) / 1_000_000)
}

pub(super) fn event_timestamp_ms(event: &AgentEvent) -> u64 {
    timestamp_ms(event.timestamp())
}

/// Commits separately authored notice intents for the session the host owns
/// now, so the projector can render the same notices on rebuild
/// (spec 02 §5.4). A notice a background publisher kept for another session
/// (the host has since cleared, resumed, forked or rebound) is dropped and
/// logged: that session is no longer shown, and this one never showed it.
pub(super) fn commit_notice_intents<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    log: Option<&SessionLog>,
    session: &mut SessionRecord,
) {
    let mut rows = Vec::new();
    for kept in events.take_notice_intents() {
        match kept.session_id {
            Some(published_for) if *published_for != *session.id => log_event(
                log,
                &session.id,
                SessionEvent::new("transcript.notice_dropped")
                    .str("published_for", published_for.to_string()),
            ),
            _ => rows.push(kept.row),
        }
    }
    if rows.is_empty() {
        return;
    }
    for row in rows {
        session.record_notice(row);
    }
    let _ = commit_session_update(store, home, session, |_| true);
}

pub(super) fn apply_event<P: plakous_host_protocol::ProtocolExtension>(
    event: &AgentEvent,
    roots: &crate::agent::ProductStateRoots,
    rows: &dyn TopologyRows,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
    assistant: &mut String,
    delta_buf: &mut String,
    last_flush: &mut Instant,
    tool_subjects: &mut ToolSubjectStash,
    projection: &mut TurnProjection<P>,
    log: Option<&SessionLog>,
    observed_session_id: &str,
    last_tree_revision: &mut TreeSnapshotPush,
    compacted_mid_turn: &mut bool,
    peak_prepared_input_tokens: &mut u64,
) {
    let applier = EventApplier {
        event,
        roots,
        rows,
        store,
        home,
        session,
        events,
        assistant,
        delta_buf,
        last_flush,
        tool_subjects,
        projection,
        log,
        observed_session_id,
        last_tree_revision,
        compacted_mid_turn,
        peak_prepared_input_tokens,
        call_id_str: event.call_id().map(|id| id.as_uuid().to_string()),
    };
    match event.kind() {
        AgentEventKind::ResponseSettled { .. } => applier.response_settled(),
        AgentEventKind::Model { event: Some(_), .. } => applier.model(),
        AgentEventKind::ToolStarted { .. } => applier.tool_started(),
        AgentEventKind::ToolProgress { .. } => applier.tool_progress(),
        AgentEventKind::ToolCompleted { .. } => applier.tool_completed(),
        AgentEventKind::ToolPreparationRejected { .. } => applier.tool_preparation_rejected(),
        AgentEventKind::EffectDecisionRequired { .. } => applier.effect_decision_required(),
        AgentEventKind::EffectDecisionResolved { .. } => applier.effect_decision_resolved(),
        AgentEventKind::ObservationClipped { .. } => applier.observation_clipped(),
        AgentEventKind::ObservationReplaced { .. } => applier.observation_replaced(),
        AgentEventKind::SteeringQueued { .. } => applier.steering_queued(),
        AgentEventKind::SteeringApplied => applier.steering_applied(),
        AgentEventKind::ModelRequestQueued { .. } => applier.model_request_queued(),
        AgentEventKind::OperationBudgetExhausted => applier.operation_budget_exhausted(),
        AgentEventKind::ModelStarted => applier.model_started(),
        AgentEventKind::OutputLimitRetry { .. } => applier.output_limit_retry(),
        AgentEventKind::OutputBoundRederived { .. } => applier.output_bound_rederived(),
        AgentEventKind::ToolResultsReplaced { .. } => applier.tool_results_replaced(),
        AgentEventKind::CompactionStarted { .. } => applier.compaction_started(),
        AgentEventKind::CompactionFinished { .. } => applier.compaction_finished(),
        _ => applier.unobserved(),
    }
}

/// One agent event and the turn state it updates. Each event kind is one
/// method, grouped by concern in the child modules; the method takes the
/// fields it uses.
struct EventApplier<'a, P: plakous_host_protocol::ProtocolExtension> {
    event: &'a AgentEvent,
    roots: &'a crate::agent::ProductStateRoots,
    rows: &'a dyn TopologyRows,
    store: &'a dyn SessionRecordStore,
    home: &'a SessionHome,
    session: &'a mut SessionRecord,
    events: &'a Sender<P>,
    assistant: &'a mut String,
    delta_buf: &'a mut String,
    last_flush: &'a mut Instant,
    tool_subjects: &'a mut ToolSubjectStash,
    projection: &'a mut TurnProjection<P>,
    log: Option<&'a SessionLog>,
    observed_session_id: &'a str,
    last_tree_revision: &'a mut TreeSnapshotPush,
    compacted_mid_turn: &'a mut bool,
    peak_prepared_input_tokens: &'a mut u64,
    call_id_str: Option<String>,
}

impl<P: plakous_host_protocol::ProtocolExtension> EventApplier<'_, P> {
    /// A steer queued behind the running step.
    fn steering_queued(self) {
        let Self {
            event,
            events,
            delta_buf,
            last_flush,
            ..
        } = self;
        let AgentEventKind::SteeringQueued { position } = event.kind() else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        let _ = events.send(HostEvent::SteerAck {
            state: "queued".into(),
            position: Some(*position),
        });
    }

    /// A queued steer applied to the turn.
    fn steering_applied(self) {
        let Self {
            events,
            delta_buf,
            last_flush,
            ..
        } = self;
        flush_assistant_delta(delta_buf, last_flush, events, true);
        let _ = events.send(HostEvent::SteerAck {
            state: "applied".into(),
            position: None,
        });
    }

    /// Events the host does not present: flush the pending text.
    fn unobserved(self) {
        let Self {
            events,
            delta_buf,
            last_flush,
            ..
        } = self;
        flush_assistant_delta(delta_buf, last_flush, events, true);
    }
}

#[cfg(test)]
mod tests;
