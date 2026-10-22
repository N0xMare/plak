//! Agent event fan-out and the turn projection it maintains.

use crate::session_home::SessionHome;
use crate::session_store::SessionRecordStore;
use std::collections::HashMap;
use std::time::Instant;

use plakous::agent::{
    AgentEvent, AgentEventKind, CompactionEventOutcome, CompactionEventReason,
    EffectDecisionResolution, ToolCallObservation, TurnControl,
};
use plakous::model::ModelEvent;

use crate::session_log::{
    SessionEvent, SessionLog, redact_text_preview, tool_rejected_reason, tool_rejected_record,
};
use crate::sessions::{self, SessionRecord};
use plakous::agent::{CompactionRung, CompactionSummary};

use super::admission::commit_session_update;
use super::logging::log_event;
use super::tools::{harvest_assistant_text, stash_tool_subjects_from_response};
use super::tree::{
    continuation_parent_call_id, emit_tree_snapshot_if_revision_advanced, lineage_parent_call_id,
    nested_parent_call_id,
};
use super::turn::{flush_assistant_delta, take_pending_approval};
use super::usage::record_operation_prepared_input;
use super::{
    APPROVAL_REFUSED_NOTE, COMPACT_COMMITTED_DETAIL, COMPACT_FAILED_DETAIL,
    COMPACT_NO_ELIGIBLE_RANGE_DETAIL, COMPACT_NOT_NEEDED_DETAIL, COMPACT_PROVIDER_QUIET_NOTE,
    COMPACT_RECOVERY_REQUIRED_DETAIL, COMPACT_UNAVAILABLE_DETAIL, COMPACTING_ACTIVITY_LABEL,
    ComposeProjection, HostEvent, MAX_PENDING_REASONING_BYTES, PendingApproval, ReasoningLocation,
    SealedReasoning, Sender, ToolSubjectStash, TopologyRows, TreeSnapshotPush, TurnProjection,
};
use crate::sessions::{NoticeScope, TranscriptRow};

fn sealed_reasoning(location_key: String, text: String, started: Instant) -> SealedReasoning {
    SealedReasoning {
        location_key,
        text,
        elapsed_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
    }
}

fn observe_thinking_activity(session: &mut SessionRecord, sealed: &SealedReasoning) {
    if sealed.text.trim().is_empty() {
        return;
    }
    let activity = sessions::ThinkingRow {
        location_key: sealed.location_key.clone(),
        text: sealed.text.clone(),
        elapsed_ms: sealed.elapsed_ms,
    };
    session.append_thinking_activity(activity);
}

/// Seals one reasoning block for the surface and holds it until its response
/// settles.
fn seal_reasoning_block<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    events: &Sender<P>,
    location: plakous::model::OutputLocation,
) {
    if let Some(sealed) = projection.seal_reasoning(location) {
        let location_key = sealed.location_key.clone();
        projection.hold_thinking(sealed);
        let _ = events.send(HostEvent::ReasoningSealed { location_key });
    }
}

/// Observes the reasoning a completed response or an ended turn settled: the
/// blocks it sealed, then every admitted location still open, which the stream
/// will now never complete. An unbound location never becomes displayable, so
/// its buffer is dropped rather than flushed.
pub(super) fn settle_live_thinking<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    session: &mut SessionRecord,
    events: &Sender<P>,
) {
    for sealed in projection.settle_thinking() {
        observe_thinking_activity(session, &sealed);
    }
    for sealed in projection.seal_all_reasoning() {
        observe_thinking_activity(session, &sealed);
        let _ = events.send(HostEvent::ReasoningSealed {
            location_key: sealed.location_key,
        });
    }
}

/// Settles one completed model response. A Content view carries only the
/// response's kind, so this is all the host can do with it there.
///
/// Its reasoning settles before
/// any row of the tools it requested, so a later discarding replay can only
/// drop the response after it.
fn settle_model_response<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    session: &mut SessionRecord,
    events: &Sender<P>,
    delta_buf: &mut String,
    last_flush: &mut Instant,
) {
    flush_assistant_delta(delta_buf, last_flush, events, true);
    settle_live_thinking(projection, session, events);
}

/// Drops the reasoning of a response a replay discarded, closing any block a
/// surface still shows open.
fn discard_response_reasoning<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    events: &Sender<P>,
) {
    for location_key in projection.discard_reasoning() {
        let _ = events.send(HostEvent::ReasoningSealed { location_key });
    }
}

/// Maps one typed approval intent back onto the runtime decision handle.
///
/// Decision authority stays in Plakous. The TUI only
/// ever names a host-minted `approval_id`; the `EffectDecisionRef` and the
/// audit id are held here, and an unknown or already-submitted handle is
/// refused without contacting the runtime at all.
pub(super) async fn resolve_approval_intent<P: plakous_host_protocol::ProtocolExtension>(
    control: &TurnControl,
    events: &Sender<P>,
    projection: &mut TurnProjection<P>,
    approval_id: &str,
    choice: plakous_host_protocol::ApprovalChoice,
) {
    let Some(pending) = take_pending_approval(projection, approval_id) else {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Approval,
            APPROVAL_REFUSED_NOTE,
        ));
        return;
    };
    // The decision→handle map survives the intent so the runtime's own
    // resolution event still retires the row truthfully.
    let outcome = match choice {
        plakous_host_protocol::ApprovalChoice::Allow => {
            let Ok(audit) = plakous::tools::EffectDecisionAuditId::new(approval_id.to_owned())
            else {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Approval,
                    "approval refused: the host decision identity was not portable",
                ));
                return;
            };
            control.allow_effect(pending.decision, audit).await
        }
        plakous_host_protocol::ApprovalChoice::Deny => {
            control
                .deny_tool(
                    pending.decision,
                    plakous::tools::ToolPolicyDenial::new("operator denied this tool"),
                )
                .await
        }
    };
    if let Err(error) = outcome {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Approval,
            format!("approval was not applied: {error}"),
        ));
    }
}

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

/// Retain historical projection notices as bounded presentation acknowledgements.
/// This never authorizes replay; ordinary diagnostics remain live and unsuppressed.
fn publish_model_diagnostic<P: plakous_host_protocol::ProtocolExtension>(
    diagnostic: &plakous::model::ModelDiagnostic,
    log: Option<&SessionLog>,
    session_id: &str,
    events: &Sender<P>,
) {
    // A history projection notice is an internal diagnostic: session log
    // only, never persisted or rendered.
    if let Some(identity) = diagnostic.history_projection_identity() {
        log_event(
            log,
            session_id,
            SessionEvent::new("model.history_projection")
                .str("identity", identity.to_string())
                .str("message", diagnostic.message().to_string()),
        );
        return;
    }
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Turn,
        diagnostic.message().to_string(),
    ));
}

/// Update one live activity row. No store is involved.
pub(super) fn observe_tool_activity(session: &mut SessionRecord, activity: sessions::ToolItem) {
    session.upsert_tool_activity(activity);
}

/// Occupancy figures on a Compact row (`199.7k`, `400`).
///
/// Same quantity as the composer meter and Context header: the Plakous
/// context measure. Prepared-input stays on the Context tab as its own
/// labelled fact.
pub(super) fn format_compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        let tenths = tokens.saturating_add(50) / 100;
        format!("{}.{}k", tenths / 10, tenths % 10)
    } else {
        tokens.to_string()
    }
}

/// Occupancy for Compact chrome when both sides exist. Prepared-input only
/// when the byte split was not measured (offline loopback).
pub(super) fn compact_row_token_pair(
    summary: Option<CompactionSummary>,
) -> (Option<u64>, Option<u64>) {
    let occupancy_before = summary.and_then(|item| item.occupancy_tokens_before);
    let occupancy_after = summary.and_then(|item| item.occupancy_tokens_after);
    match (occupancy_before, occupancy_after) {
        (Some(before), Some(after)) => (Some(before), Some(after)),
        _ => (
            summary.map(|item| item.input_tokens_before),
            summary.map(|item| item.input_tokens_after),
        ),
    }
}

pub(crate) fn compact_terminal_subject(
    before: Option<u64>,
    after: Option<u64>,
    compacted: bool,
    successor_fits: bool,
) -> Option<String> {
    let sizes = match (before, after) {
        (Some(before), Some(after)) => Some(format!(
            "{} → {}",
            format_compact_tokens(before),
            format_compact_tokens(after),
        )),
        _ => None,
    };
    match (sizes, compacted && !successor_fits) {
        (Some(mut sizes), true) => {
            sizes.push_str(" · still over capacity");
            Some(sizes)
        }
        (Some(sizes), false) => Some(sizes),
        (None, true) => Some("still over capacity".to_owned()),
        (None, false) => None,
    }
}

pub(super) const fn compact_terminal_state(compacted: bool, successor_fits: bool) -> &'static str {
    if compacted && successor_fits {
        "done"
    } else if compacted {
        "overflow"
    } else {
        "failed"
    }
}

pub(super) fn publish_compact_activity<P: plakous_host_protocol::ProtocolExtension>(
    session: &mut SessionRecord,
    events: &Sender<P>,
    call_id: &str,
    state: &str,
    subject: Option<String>,
    duration_ms: Option<u64>,
) {
    let now = timestamp_ms(crate::product_observation::wall_timestamp());
    let prior = session.transcript.iter().rev().find_map(|line| {
        let activity = line.as_tool().cloned()?;
        (activity.call_id == call_id).then_some(activity)
    });
    let started_at_ms = prior
        .as_ref()
        .and_then(|activity| activity.started_at_ms)
        .or_else(|| (state == "running" || state == "start").then_some(now));
    let finished_at_ms = (!matches!(state, "start" | "running")).then(|| {
        started_at_ms
            .zip(duration_ms)
            .and_then(|(started, elapsed)| started.checked_add(elapsed))
            .unwrap_or(now)
    });
    observe_tool_activity(
        session,
        sessions::ToolItem {
            call_id: call_id.to_owned(),
            name: "compact".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("compact"),
            state: state.to_owned(),
            subject: subject.clone(),
            detail: None,
            started_at_ms,
            finished_at_ms,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        },
    );
    let _ = events.send(HostEvent::ToolLine {
        call_id: Some(call_id.to_owned()),
        parent_call_id: None,
        cell_id: None,
        kind: plakous_host_protocol::views::presentation::ActivityKind::Compact,
        name: "compact".into(),
        state: state.to_owned(),
        subject,
        detail: None,
        duration_ms,
        started_at_ms,
        finished_at_ms,
        wait: None,
    });
}

pub(super) fn observe_cancelled_tool_activity<P: plakous_host_protocol::ProtocolExtension>(
    session: &mut SessionRecord,
    events: &Sender<P>,
    projection: Option<&TurnProjection<P>>,
) {
    observe_terminal_tool_activity(session, events, "cancelled", projection);
}

pub(super) fn observe_terminal_tool_activity<P: plakous_host_protocol::ProtocolExtension>(
    session: &mut SessionRecord,
    events: &Sender<P>,
    state: &str,
    projection: Option<&TurnProjection<P>>,
) {
    let running = session
        .transcript
        .iter()
        .filter_map(|row| row.as_tool().cloned())
        .filter(|activity| activity.state == "running")
        .collect::<Vec<_>>();
    let closed_at_ms = timestamp_ms(crate::product_observation::wall_timestamp());
    for mut activity in running {
        state.clone_into(&mut activity.state);
        // The sweep ends the call, so the row records when, as every other
        // terminal does: a group's elapsed span reads it live and restored.
        activity.finished_at_ms = Some(closed_at_ms);
        // carried on every line the host can stamp. The sweep can
        // — the row it is closing holds the call's start.
        let duration_ms = activity
            .started_at_ms
            .and_then(|started| closed_at_ms.checked_sub(started));
        observe_tool_activity(session, activity.clone());
        let compose = projection.and_then(|projection| projection.cells.get(&activity.call_id));
        // a cell that terminalises with accepted requests that
        // never dispatched states the remainder it *observed*, or no ratio.
        let line_state = match compose {
            Some(compose) if state == "cancelled" => compose_cancelled_state(compose),
            _ => state.to_owned(),
        };
        let _ = events.send(HostEvent::ToolLine {
            call_id: Some(activity.call_id),
            // Persisted lineage, so a gap-closed child still folds under its
            // cell instead of surfacing as a stray flat row.
            parent_call_id: activity.parent_call_id,
            cell_id: activity
                .cell_id
                .or_else(|| compose.and_then(|compose| compose.cell_id.clone())),
            kind: compose.map_or_else(
                || crate::activity::tool_activity_kind(&activity.name),
                |_| plakous_host_protocol::views::presentation::ActivityKind::Compose,
            ),
            name: activity.name,
            state: line_state,
            subject: compose.map_or(activity.subject, |compose| {
                Some(compose_counts_label(
                    compose,
                    projection.is_some_and(|projection| projection.gap_observed),
                ))
            }),
            detail: activity.detail,
            duration_ms,
            started_at_ms: activity.started_at_ms,
            finished_at_ms: activity.finished_at_ms,
            wait: None,
        });
    }
}

impl ComposeProjection {
    /// The row state to re-emit with a new tally.
    ///
    /// Spec 02 §5.4: "in flight, the running glyph appears on both the compose and
    /// the child that is running". An `exec*` that yielded with a `wait*`
    /// continuation has an observed terminal of its own, but children dispatched
    /// during the wait phase are still running beneath it — painting `✓` there
    /// would claim an outcome over children the host has not observed
    /// terminating (§5.1 "no invented state"), and would leave `✓ Compose` sitting
    /// over `⊘ cancelled` children after a cancel mid-`wait*`. The row is
    /// terminal only when the marker terminalised *and* every observed child is
    /// terminal; the `wait*` terminal that closes the last child re-terminalises
    /// it. Both halves are host-observed, never inferred from the program.
    pub(super) fn row_state(&self) -> &str {
        match self.terminal_state.as_deref() {
            Some(state) if self.all_observed_children_terminal() => state,
            _ => "running",
        }
    }

    /// The persisted state for a settled marker row, or `None` while the row
    /// is still in flight.
    ///
    /// The transcript vocabulary is `succeeded` / `failed`, not the row states
    /// [`ComposeProjection::row_state`] paints.
    pub(super) fn settled_persisted_state(&self) -> Option<&'static str> {
        match self.row_state() {
            "running" => None,
            "failed" => Some("failed"),
            _ => Some("succeeded"),
        }
    }

    /// Host-observed only: every child call id seen at all has also been seen
    /// reaching a terminal state.
    pub(super) fn all_observed_children_terminal(&self) -> bool {
        self.observed_children
            .iter()
            .all(|child| self.terminal_children.contains_key(child))
    }

    pub(super) fn child_count(&self) -> usize {
        self.observed_children.len()
    }

    pub(super) fn failed_count(&self) -> usize {
        self.terminal_children
            .values()
            .filter(|failed| **failed)
            .count()
    }

    pub(super) fn terminal_count(&self) -> usize {
        self.terminal_children.len()
    }
}

pub(super) fn reasoning_location_key(location: plakous::model::OutputLocation) -> String {
    format!("{}:{}", location.item_index(), location.block_index())
}

pub(super) const fn reasoning_visibility_admits(
    visibility: plakous::model::ReasoningVisibility,
) -> bool {
    matches!(
        visibility,
        plakous::model::ReasoningVisibility::Visible | plakous::model::ReasoningVisibility::Summary
    )
}

impl<P: plakous_host_protocol::ProtocolExtension> TurnProjection<P> {
    pub(super) fn observe_block_start(&mut self, started: &plakous::model::BlockStarted) {
        let key = (
            started.location().item_index(),
            started.location().block_index(),
        );
        match started.block() {
            plakous::model::BlockStart::Reasoning { visibility, .. } => {
                let state = if reasoning_visibility_admits(*visibility) {
                    ReasoningLocation::Admitted {
                        emitted: false,
                        text: String::new(),
                        started: Instant::now(),
                    }
                } else {
                    ReasoningLocation::Suppressed
                };
                self.reasoning.insert(key, state);
            }
            plakous::model::BlockStart::ReasoningPending { .. } => {
                self.reasoning.insert(
                    key,
                    ReasoningLocation::Pending {
                        buffered: Vec::new(),
                    },
                );
            }
            _ => {}
        }
    }

    /// Returns the text to project, or `None` when the location does not admit.
    pub(super) fn observe_reasoning_delta(
        &mut self,
        delta: &plakous::model::ReasoningDelta,
    ) -> Option<String> {
        let key = (
            delta.location().item_index(),
            delta.location().block_index(),
        );
        let state = self
            .reasoning
            .entry(key)
            // A delta for a location the host never saw start has no bound, and
            // an unbound location is non-admitting.
            .or_insert(ReasoningLocation::Suppressed);
        match state {
            ReasoningLocation::Pending { buffered } => {
                if buffered.len().saturating_add(delta.bytes().len()) > MAX_PENDING_REASONING_BYTES
                {
                    *state = ReasoningLocation::Suppressed;
                } else {
                    buffered.extend_from_slice(delta.bytes());
                }
                None
            }
            ReasoningLocation::Admitted {
                emitted,
                text,
                started,
            } => {
                let piece = std::str::from_utf8(delta.bytes()).ok()?;
                if piece.is_empty() {
                    return None;
                }
                let preview = redact_text_preview(piece);
                if !*emitted {
                    *started = Instant::now();
                }
                *emitted = true;
                text.push_str(&preview);
                Some(preview)
            }
            ReasoningLocation::Suppressed => None,
        }
    }

    /// Resolves one pending location, releasing or dropping its buffer.
    ///
    /// Only a `Pending` location transitions. A `Suppressed` location stays
    /// suppressed for the remainder of the block: re-admitting it
    /// on a later bound would emit a partial reasoning tail whose head the host
    /// already dropped. An `Admitted` location keeps the `emitted` flag it
    /// earned, so a redundant bound cannot cost it its seal.
    pub(super) fn bind_reasoning_visibility(
        &mut self,
        bound: plakous::model::ReasoningVisibilityBound,
    ) -> Option<String> {
        let key = (
            bound.location().item_index(),
            bound.location().block_index(),
        );
        let admits = reasoning_visibility_admits(bound.visibility());
        match self.reasoning.get(&key) {
            Some(ReasoningLocation::Suppressed) => return None,
            // Already admitted at `BlockStarted`; a later bound cannot un-emit
            // what shipped, and must not cost the block its seal.
            Some(ReasoningLocation::Admitted { .. }) => return None,
            Some(ReasoningLocation::Pending { .. }) | None => {}
        }
        let previous = self.reasoning.remove(&key);
        if !admits {
            // Buffered bytes are dropped, never flushed.
            self.reasoning.insert(key, ReasoningLocation::Suppressed);
            return None;
        }
        let released = match previous {
            Some(ReasoningLocation::Pending { buffered }) => String::from_utf8(buffered)
                .ok()
                .filter(|text| !text.is_empty()),
            _ => None,
        };
        let preview = released.map(|text| redact_text_preview(&text));
        self.reasoning.insert(
            key,
            ReasoningLocation::Admitted {
                emitted: preview.is_some(),
                text: preview.clone().unwrap_or_default(),
                started: Instant::now(),
            },
        );
        preview
    }

    /// Retires one location, returning its body when a seal is owed.
    pub(super) fn seal_reasoning(
        &mut self,
        location: plakous::model::OutputLocation,
    ) -> Option<SealedReasoning> {
        let key = (location.item_index(), location.block_index());
        match self.reasoning.remove(&key) {
            Some(ReasoningLocation::Admitted {
                emitted: true,
                text,
                started,
            }) => Some(sealed_reasoning(
                reasoning_location_key(location),
                text,
                started,
            )),
            _ => None,
        }
    }

    /// Holds a sealed block until its response settles.
    pub(super) fn hold_thinking(&mut self, sealed: SealedReasoning) {
        self.unsettled_thinking.push(sealed);
    }

    /// Releases the held blocks of a response that completed or a turn that
    /// ended, for persisting.
    pub(super) fn settle_thinking(&mut self) -> Vec<SealedReasoning> {
        std::mem::take(&mut self.unsettled_thinking)
    }

    /// Drops the held blocks and open locations of the in-flight response the
    /// retry policy discarded, returning the open keys a renderer had opened.
    /// Earlier responses of the turn settled at their completion and are not
    /// here.
    pub(super) fn discard_reasoning(&mut self) -> Vec<String> {
        self.unsettled_thinking.clear();
        let mut keys = self
            .reasoning
            .drain()
            .filter_map(|(key, state)| {
                matches!(state, ReasoningLocation::Admitted { emitted: true, .. })
                    .then(|| format!("{}:{}", key.0, key.1))
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    /// Retires every location at response completion.
    ///
    /// A location that never received a bound is non-admitting, so its buffer
    /// is dropped rather than flushed.
    pub(super) fn seal_all_reasoning(&mut self) -> Vec<SealedReasoning> {
        let mut sealed = self
            .reasoning
            .drain()
            .filter_map(|(key, state)| match state {
                ReasoningLocation::Admitted {
                    emitted: true,
                    text,
                    started,
                } => Some(sealed_reasoning(
                    format!("{}:{}", key.0, key.1),
                    text,
                    started,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        sealed.sort_by(|left, right| left.location_key.cmp(&right.location_key));
        sealed
    }

    /// The marker call owning `cell_id`, when the host has observed both.
    pub(super) fn marker_for_cell(&self, cell_id: Option<&str>) -> Option<String> {
        self.marker_by_cell.get(cell_id?).cloned()
    }

    pub(super) fn learn_cell_owner(&mut self, marker_call: &str, cell_id: Option<&str>) {
        if let Some(cell_id) = cell_id {
            self.marker_by_cell
                .insert(cell_id.to_owned(), marker_call.to_owned());
        }
    }

    pub(super) fn open_compose(&mut self, marker_call: &str, name: &str, cell_id: Option<&str>) {
        self.learn_cell_owner(marker_call, cell_id);
        self.cells.insert(
            marker_call.to_owned(),
            ComposeProjection {
                name: name.to_owned(),
                cell_id: cell_id.map(str::to_owned),
                observed_children: std::collections::BTreeSet::new(),
                terminal_children: HashMap::new(),
                highest_ordinal: 0,
                terminal_state: None,
            },
        );
    }

    /// Learns the cell a marker owns from the marker's own lineage.
    ///
    /// A cell that yields before it dispatches anything named no cell on its
    /// start event — the interpreter mints the cell after the marker begins —
    /// so `open_compose` learnt nothing, the `wait*` continuation had no row to
    /// fold under, and the wait terminal never re-terminalised the compose. The
    /// terminal lineage carries the retired cell.
    pub(super) fn learn_marker_cell(&mut self, marker_call: &str, cell_id: Option<&str>) {
        let Some(compose) = self.cells.get_mut(marker_call) else {
            return;
        };
        if compose.cell_id.is_none() {
            compose.cell_id = cell_id.map(str::to_owned);
        }
        let learnt = compose.cell_id.clone();
        self.learn_cell_owner(marker_call, learnt.as_deref());
    }

    /// Records the marker's own observed terminal row state.
    ///
    /// A `wait*` continuation of the same cell reports the cell's later state,
    /// so the last observed terminal wins and the row never regresses to
    /// `running` behind it.
    pub(super) fn observe_compose_terminal(&mut self, marker_call: &str, state: &str) {
        if let Some(compose) = self.cells.get_mut(marker_call) {
            compose.terminal_state = Some(state.to_owned());
        }
    }

    /// Records one child start, and reports whether it put a yielded cell back
    /// in flight — a compose whose marker already terminalised now owns a
    /// non-terminal child, so its row must paint `running` again (§5.8).
    pub(super) fn observe_child_start(
        &mut self,
        parent_call_id: &str,
        child_call_id: Option<&str>,
        cell_id: Option<&str>,
        ordinal: Option<u32>,
    ) -> bool {
        let Some(compose) = self.cells.get_mut(parent_call_id) else {
            return false;
        };
        if compose.cell_id.is_none() {
            compose.cell_id = cell_id.map(str::to_owned);
        }
        let learn = compose.cell_id.clone();
        if let Some(child) = child_call_id {
            compose.observed_children.insert(child.to_owned());
            compose.terminal_children.remove(child);
        }
        if let Some(ordinal) = ordinal {
            compose.highest_ordinal = compose.highest_ordinal.max(ordinal);
        }
        let reopened =
            compose.terminal_state.is_some() && !compose.all_observed_children_terminal();
        self.learn_cell_owner(parent_call_id, learn.as_deref());
        reopened
    }

    /// Records one child's terminal state and returns the owning compose.
    pub(super) fn observe_child_terminal(
        &mut self,
        parent_call_id: &str,
        child_call_id: Option<&str>,
        failed: bool,
    ) -> Option<&ComposeProjection> {
        let compose = self.cells.get_mut(parent_call_id)?;
        let child = child_call_id?.to_owned();
        compose.observed_children.insert(child.clone());
        compose.terminal_children.insert(child, failed);
        Some(&*compose)
    }
}

/// The program a Compose cell ran, as the operator wrote it (§5.8).
///
/// Codex `exec` arguments are raw UTF-8 program source, and the observed
/// preview carries them as one JSON *string* — rendering that form put a
/// leading quote and escaped newlines behind the rail. A Plak PTC marker
/// instead carries a JSON body whose `source` / `code` / `program` field holds
/// the program. Anything else is taken verbatim. Nothing is invented: a marker
/// without a program yields `None`.
pub(super) fn compose_program_source(arguments: Option<&str>) -> Option<String> {
    let arguments = arguments?.trim();
    if arguments.is_empty() {
        return None;
    }
    let source = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(serde_json::Value::String(program)) => program,
        Ok(serde_json::Value::Object(body)) => body
            .get("source")
            .or_else(|| body.get("code"))
            .or_else(|| body.get("program"))
            .and_then(serde_json::Value::as_str)?
            .to_owned(),
        _ => arguments.to_owned(),
    };
    let source = source.trim_matches('\n');
    (!source.trim().is_empty()).then(|| source.to_owned())
}

/// Renders one Compose row's host-counted, host-observed tally.
///
/// These are counts over a droppable observation stream, never a claim about
/// what the cell executed and never a billing or capacity claim.
pub(super) fn compose_counts_label(compose: &ComposeProjection, gap_observed: bool) -> String {
    compose_counts_subject(compose.child_count(), compose.failed_count(), gap_observed)
}

pub(crate) fn compose_counts_subject(count: usize, failed: usize, gap_observed: bool) -> String {
    let mut label = format!("{count} tools");
    if failed > 0 {
        label.push_str(&format!(" · {failed} failed"));
    }
    if gap_observed {
        label.push_str(" · counts partial after gap");
    }
    label
}

/// Whether a row's subject is the host's own (web, image and MCP rows), so
/// the runtime's generic subject must not replace it.
pub(crate) fn host_owns_subject(canonical: &str, exposed: &str) -> bool {
    super::tools::is_web_or_mcp_row(canonical) || super::tools::is_web_or_mcp_row(exposed)
}

pub(crate) fn projected_tool_subject(name: &str, arguments: Option<&str>) -> Option<String> {
    let arguments = serde_json::from_str(arguments?).ok()?;
    super::tools::tool_subject_from_args(name, &arguments)
        .map(|subject| sessions::bound_subject(&redact_text_preview(&subject)))
}

/// Terminal state text for a cancelled Compose row.
///
/// The remainder is only ever rendered from what the host observed: without an
/// observed ordinal there is no ratio to state.
pub(super) fn compose_cancelled_state(compose: &ComposeProjection) -> String {
    if compose.highest_ordinal == 0 {
        return "cancelled".to_owned();
    }
    format!(
        "cancelled after {} of {}",
        compose.terminal_count(),
        compose.highest_ordinal
    )
}

/// Re-persists a Compose head's counts after a child terminal moved them.
///
/// The head's own `ToolCompleted` persists the tally it had at that moment; a
/// child that terminalises later — during a `wait*` continuation, or a
/// rejected accepted request — is counted live by `emit_compose_update` but was
/// never written back, so a resumed transcript showed the stale `3 tools`
/// against the live `5 tools`.
pub(super) fn observe_compose_subject(
    session: &mut SessionRecord,
    marker_call: &str,
    subject: String,
) {
    let Some(mut activity) = session.transcript.tool_activity(marker_call) else {
        return;
    };
    if activity.subject.as_deref() == Some(subject.as_str()) {
        return;
    }
    activity.subject = Some(subject);
    observe_tool_activity(session, activity);
}

/// Re-persists a reopened Compose marker as terminal once its last observed
/// child terminalises.
///
/// A cell that yields and then dispatches during its `wait*` continuation is
/// written back to `running`, so a cancel in that window sweeps it. When the
/// wait phase instead closes cleanly nothing wrote the marker terminal again,
/// so the `Completed` sweep over persisted `running` rows rewrote a cell that
/// had succeeded to `failed:observation-gap` — live and on resume. The row is
/// re-persisted only from what the host observed: the
/// marker's own terminal plus every observed child terminalising.
pub(super) fn observe_settled_compose_marker(
    session: &mut SessionRecord,
    compose: &ComposeProjection,
    gap_observed: bool,
    marker_call: &str,
    finished_at_ms: u64,
) {
    let Some(state) = compose.settled_persisted_state() else {
        return;
    };
    let Some(mut marker) = session.transcript.tool_activity(marker_call) else {
        return;
    };
    let subject = compose_counts_label(compose, gap_observed);
    if marker.state == state
        && marker.finished_at_ms.is_some()
        && marker.subject.as_deref() == Some(subject.as_str())
    {
        return;
    }
    state.clone_into(&mut marker.state);
    marker.finished_at_ms = Some(finished_at_ms);
    marker.subject = Some(subject);
    observe_tool_activity(session, marker);
}

pub(super) fn emit_compose_update<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
    compose: &ComposeProjection,
    call_id: &str,
    gap_observed: bool,
    duration_ms: Option<u64>,
) {
    // The cell's persisted start; a terminal head ends where its duration,
    // measured from that start, does.
    let started_at_ms = session
        .transcript
        .tool_activity(call_id)
        .and_then(|row| row.started_at_ms);
    let finished_at_ms = (compose.row_state() != "running")
        .then(|| started_at_ms.zip(duration_ms))
        .flatten()
        .map(|(started, elapsed)| started.saturating_add(elapsed));
    let _ = events.send(HostEvent::ToolLine {
        call_id: Some(call_id.to_owned()),
        parent_call_id: None,
        cell_id: compose.cell_id.clone(),
        kind: plakous_host_protocol::views::presentation::ActivityKind::Compose,
        name: compose.name.clone(),
        state: compose.row_state().into(),
        subject: Some(compose_counts_label(compose, gap_observed)),
        detail: None,
        duration_ms,
        started_at_ms,
        finished_at_ms,
        wait: None,
    });
}

/// Wall time of one call from the `started_at_ms` the host persisted for it.
///
/// This is the same pair of stamps a resumed transcript derives its duration
/// from (`ActivityItem::with_derived_duration`), so a live row and the row a
/// `/resume` rebuilds report the same number for the same call. A cell that
/// yields is the case that made the difference visible: its marker
/// terminalises, reopens on the next dispatch and terminalises again, and only
/// the first stamp describes when the cell started.
pub(super) fn observed_call_duration_ms(
    session: &SessionRecord,
    call_id: &str,
    observed_at_ms: u64,
) -> Option<u64> {
    session
        .transcript
        .tool_activity(call_id)
        .and_then(|activity| activity.started_at_ms)
        .map(|started_at_ms| observed_at_ms.saturating_sub(started_at_ms))
}

pub(super) fn project_live_usage<P: plakous_host_protocol::ProtocolExtension>(
    update: &plakous::model::UsageUpdate,
) -> HostEvent<P> {
    let usage = update.usage();
    HostEvent::TurnUsage {
        input_tokens: usage.input_tokens(),
        output_tokens: usage.output_tokens(),
        reasoning_tokens: usage.reasoning_tokens(),
        cached_input_tokens: usage.cached_input_tokens(),
        cache_write_tokens: usage.cache_write_tokens(),
        // A live report is one provider-reported cumulative snapshot; the host
        // does not know how many operations it covers and never estimates, so
        // coverage is unknown rather than a fabricated `1/1`.
        model_operations: None,
        reporting_model_operations: None,
        peak_prepared_input_tokens: None,
        terminal: false,
    }
}

/// The label a compaction terminal is recorded and reported under.
const fn compaction_outcome_label(outcome: CompactionEventOutcome) -> &'static str {
    match outcome {
        CompactionEventOutcome::Committed => "committed",
        CompactionEventOutcome::Failed => "failed",
        CompactionEventOutcome::RecoveryRequired => "recovery_required",
        CompactionEventOutcome::NotNeeded => "not_needed",
        CompactionEventOutcome::NoEligibleRange => "no_eligible_range",
        CompactionEventOutcome::Unavailable => "unavailable",
    }
}

/// Builds the `compact.finished` record, carrying the reason when there is one.
///
/// A session log held five
/// `compact.started`/`compact.finished` pairs, two of them `outcome=failed`,
/// and the failed records carried nothing but the outcome and the elapsed
/// time. Nobody could say afterwards what had failed. The classified provider
/// failure, or the driver's own refusal, is recorded here.
pub(super) fn compact_finished_record(
    outcome: CompactionEventOutcome,
    elapsed_ms: u64,
    summary: Option<CompactionSummary>,
    detail: Option<&str>,
) -> SessionEvent {
    let mut record = SessionEvent::new("compact.finished")
        .u64("elapsed_ms", elapsed_ms)
        .str("outcome", compaction_outcome_label(outcome));
    // Additive: a reader that does not know these fields is unaffected, and a
    // terminal with no figures still writes exactly what it always wrote.
    if let Some(summary) = summary {
        record = record
            .str("rung", compaction_rung_label(summary.rung))
            .u64("segments", u64::from(summary.segments))
            .u64("input_tokens_before", summary.input_tokens_before)
            .u64("input_tokens_after", summary.input_tokens_after);
    }
    match detail {
        Some(detail) if !detail.trim().is_empty() => {
            record.str("detail", redact_text_preview(detail))
        }
        _ => record,
    }
}

/// The stable label for a compaction rung in the session log.
pub(super) const fn compaction_rung_label(rung: CompactionRung) -> &'static str {
    match rung {
        CompactionRung::NativeFacet => "native_facet",
        CompactionRung::SemanticFold => "semantic_fold",
        CompactionRung::ToolResultPlaceholders => "tool_result_placeholders",
        CompactionRung::ReplayOmission => "replay_omission",
    }
}

/// The operator clause naming what a committed compaction did.
///
/// "compact workflow committed" tells a reader nothing about whether it bought
/// anything. The figures do.
pub(super) fn compaction_summary_clause(summary: CompactionSummary) -> String {
    format!(
        "compaction {} {} exchange(s): {} -> {} prepared input tokens",
        summary.rung.verb(),
        summary.segments,
        summary.input_tokens_before,
        summary.input_tokens_after
    )
}

/// The operator-facing note for a compaction terminal.
pub(super) fn compact_finished_note(
    outcome: CompactionEventOutcome,
    summary: Option<CompactionSummary>,
    detail: Option<&str>,
) -> String {
    let mut note = summary.map_or_else(
        || compaction_event_outcome_detail(outcome).to_owned(),
        compaction_summary_clause,
    );
    if let Some(detail) = detail {
        if !detail.trim().is_empty() {
            note.push_str(": ");
            note.push_str(&redact_text_preview(detail));
        }
    }
    note
}

/// What a process-shaped call's subject is read against.
pub(super) struct ProcessContext<'a> {
    pub(super) label: Option<&'static str>,
    pub(super) args: Option<&'a serde_json::Value>,
    pub(super) locator: Option<plakous::sessions::SessionLocator>,
    pub(super) processes: &'a [plakous_host_protocol::views::ProcessView],
    pub(super) commands: &'a HashMap<String, String>,
}

/// A starting tool row's subject and detail.
pub(super) struct StartedToolText {
    pub(super) subject: Option<String>,
    pub(super) detail: Option<String>,
}

/// The subject and detail a tool row starts with, from the facts its start
/// carries. The root's live path and an agent transcript read both call this,
/// so a call reads the same in either.
pub(super) fn started_tool_text(
    name: &str,
    exposed_name: Option<&str>,
    observation: Option<&ToolCallObservation>,
    info: Option<&super::PendingToolInfo>,
    process: ProcessContext<'_>,
) -> StartedToolText {
    let kind = crate::activity::tool_activity_kind(name);
    // A Compose head's subject is its host-counted tally (§5.8); the
    // program source is its detail, never its subject.
    let subject = (kind != plakous_host_protocol::views::presentation::ActivityKind::Compose)
        .then(|| super::tools::started_tool_subject(name, exposed_name, observation, info))
        .flatten()
        .map(|value| live_tool_subject(&value));
    let subject = process
        .label
        .zip(process.args)
        .map(|(label, args)| {
            let subject = if label == "Shell" {
                super::tools::process_subject(label, args, process.locator, process.processes, None)
            } else {
                super::tools::process_subject_for_ids(
                    label,
                    &super::tools::process_ids(args),
                    args.get("input")
                        .or_else(|| args.get("chars"))
                        .and_then(serde_json::Value::as_str),
                    process.locator,
                    process.processes,
                    None,
                    process.commands,
                )
            };
            super::tools::process_action_subject(label, &subject, false)
        })
        .or(subject);
    // §5.8: a Compose head's detail is the program itself, so the
    // expanded rail shows the code the cell ran rather than its JSON
    // encoding.
    let detail = (kind == plakous_host_protocol::views::presentation::ActivityKind::Compose)
        .then(|| {
            compose_program_source(observation.map(ToolCallObservation::arguments_preview))
                .map(|program| redact_text_preview(&program))
        })
        .flatten()
        .or_else(|| {
            observation.and_then(|item| {
                crate::activity::tool_detail_text(
                    Some(&redact_text_preview(item.arguments_preview())),
                    None,
                )
            })
        })
        .or_else(|| info.map(|item| item.detail.clone()));
    StartedToolText { subject, detail }
}

/// A settled process wait's subject, `Waited on process <ids> · <outcome>`,
/// from the ids its arguments named and its result preview. The root's live
/// path and an agent transcript read both call this.
pub(super) fn settled_process_wait_subject(
    ids: &[String],
    preview: Option<&str>,
    locator: Option<plakous::sessions::SessionLocator>,
    processes: &[plakous_host_protocol::views::ProcessView],
    commands: &HashMap<String, String>,
) -> String {
    let result = preview
        .and_then(|preview| serde_json::from_str::<serde_json::Value>(preview).ok())
        .unwrap_or(serde_json::Value::Null);
    let subject = super::tools::process_subject_for_ids(
        "process_session_wait",
        ids,
        None,
        locator,
        processes,
        Some(&result),
        commands,
    );
    super::tools::process_action_subject("process_session_wait", &subject, true)
}

/// A completed call's state label: `failed` when the call failed, and for a
/// process call that succeeded, how the command it ran ended (C8).
pub(crate) fn completed_tool_state(name: &str, result: Option<(bool, &str)>) -> String {
    match result {
        Some((false, _)) => "failed".to_owned(),
        Some((true, preview))
            if crate::activity::tool_activity_kind(name)
                == plakous_host_protocol::views::presentation::ActivityKind::Process =>
        {
            crate::activity::process_failure(preview)
                .map_or_else(|| "succeeded".to_owned(), |how| format!("failed:{how}"))
        }
        _ => "succeeded".to_owned(),
    }
}

/// A completed call's detail: its start detail joined with its redacted
/// result preview. Spec 02 §5.4: readable text, never `Arguments:` / `Result:`
/// JSON dumps.
pub(crate) fn completed_tool_detail(
    started_detail: Option<&str>,
    preview: Option<&str>,
) -> Option<String> {
    preview.and_then(|preview| {
        crate::activity::tool_detail_text(started_detail, Some(&redact_text_preview(preview)))
    })
}

/// §5.2: a completed header carries the host's own result-derived counts
/// after its start subject. Only the host parses tool output for them.
pub(super) fn completed_tool_summary(
    name: &str,
    started_subject: Option<&str>,
    preview: Option<&str>,
) -> Option<String> {
    let preview = preview?;
    let subject = started_subject?;
    let summary = super::tools::tool_result_summary(name, Some(preview))?;
    Some(format!("{subject} {summary}"))
}

pub(super) use crate::sessions::live_tool_subject;
// Row derivations the feed projector shares with the live path.
pub(crate) use super::tool_facts::{wait_settled, wait_started};
pub(crate) use super::turn::operator_facing_agent_error;

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
    let call_id_str = event.call_id().map(|id| id.as_uuid().to_string());
    match event.kind() {
        AgentEventKind::ResponseSettled { discardable } => {
            if *discardable {
                discard_response_reasoning(projection, events);
            } else {
                settle_model_response(projection, session, events, delta_buf, last_flush);
                projection.settled_assistant_len = assistant.len();
            }
        }
        AgentEventKind::Model {
            event: Some(model_event),
            ..
        } => {
            match model_event.as_ref() {
                ModelEvent::TextDelta(delta) => {
                    if let Ok(text) = std::str::from_utf8(delta.bytes()) {
                        assistant.push_str(text);
                        if !text.is_empty() {
                            delta_buf.push_str(text);
                            flush_assistant_delta(delta_buf, last_flush, events, false);
                        }
                    }
                }
                ModelEvent::BlockStarted(started) => {
                    flush_assistant_delta(delta_buf, last_flush, events, true);
                    projection.observe_block_start(started);
                }
                ModelEvent::ReasoningDelta(delta) => {
                    if let Some(text) = projection.observe_reasoning_delta(delta) {
                        flush_assistant_delta(delta_buf, last_flush, events, true);
                        let _ = events.send(HostEvent::ReasoningDelta {
                            location_key: reasoning_location_key(delta.location()),
                            text,
                        });
                    }
                }
                ModelEvent::ReasoningVisibilityBound(bound) => {
                    if let Some(text) = projection.bind_reasoning_visibility(*bound) {
                        flush_assistant_delta(delta_buf, last_flush, events, true);
                        let _ = events.send(HostEvent::ReasoningDelta {
                            location_key: reasoning_location_key(bound.location()),
                            text,
                        });
                    }
                }
                ModelEvent::BlockCompleted(completed) => {
                    seal_reasoning_block(projection, events, completed.location());
                }
                ModelEvent::Usage(update) => {
                    record_operation_prepared_input(
                        peak_prepared_input_tokens,
                        session,
                        update,
                        log,
                    );
                    let _ = events.send(project_live_usage(update));
                }
                ModelEvent::ResponseCompleted(response) => {
                    // One model operation ends here, whether or not the turn
                    // that contains it ever reaches a terminal result.
                    // Some providers deliver final text only on completed output
                    // (no/partial TextDelta). Harvest so the TUI is not blank.
                    if assistant.is_empty() {
                        let harvested = harvest_assistant_text(response);
                        if !harvested.is_empty() {
                            assistant.push_str(&harvested);
                            let _ = events.send(HostEvent::AssistantDelta(harvested));
                            let _ = events.send(HostEvent::AssistantDone);
                        }
                    }
                    projection.settled_assistant_len = assistant.len();
                    // Stash tool subjects from completed model output before tools run.
                    stash_tool_subjects_from_response(tool_subjects, response);
                }
                ModelEvent::Retry(retry) => {
                    if retry.loss().declined_reason.is_some() {
                        return;
                    }
                    if retry.output_discarded() {
                        // The replay streams a fresh response: what the failed
                        // request showed never reaches history or the tally.
                        // Earlier responses of the turn already settled.
                        delta_buf.clear();
                        assistant.truncate(projection.settled_assistant_len);
                        discard_response_reasoning(projection, events);
                    } else {
                        flush_assistant_delta(delta_buf, last_flush, events, true);
                    }
                    let notice = plakous_host_protocol::ModelRetryNotice {
                        next_attempt: retry.next_physical(),
                        max_attempts: retry.max_physical(),
                        delay_ms: u64::try_from(retry.delay().as_millis()).unwrap_or(u64::MAX),
                        cause_kind: retry.cause().kind().to_owned(),
                        cause: retry.human_cause(),
                        output_discarded: retry.output_discarded(),
                    };
                    // The retry item persists so a restore shows it inside the
                    // same group the live turn did.
                    let row =
                        TranscriptRow::Retry(plakous_host_protocol::views::transcript::RetryItem {
                            attempt: notice.next_attempt,
                            max_attempts: notice.max_attempts,
                            cause: notice.cause.clone(),
                        });
                    let _ = events.send(HostEvent::ModelRetry(notice));
                    let _ = commit_session_update(store, home, session, |candidate| {
                        candidate.record_notice(row.clone());
                        true
                    });
                    let _ = events.send(HostEvent::Row(row));
                }
                // Projection acknowledgements are presentation only, never
                // canonical history or provider replay authority.
                ModelEvent::Diagnostic(diagnostic) => {
                    flush_assistant_delta(delta_buf, last_flush, events, true);
                    publish_model_diagnostic(diagnostic, log, &session.id, events);
                }
                _ => {
                    flush_assistant_delta(delta_buf, last_flush, events, true);
                }
            }
        }
        AgentEventKind::ToolStarted {
            identity,
            lineage,
            observation,
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let canonical = identity.canonical_name().as_str().to_owned();
            let exposed_name = identity
                .exposed_name()
                .map(plakous::model::ToolName::as_str);
            let info = tool_subjects.take(&canonical, call_id_str.as_deref());
            let process_args = observation.as_ref().and_then(|item| {
                serde_json::from_str::<serde_json::Value>(item.arguments_preview()).ok()
            });
            // A Grok output or kill call naming a child reads as the await or
            // the interrupt it is, from here on (presentation only).
            let name = presented_start_name(
                projection,
                call_id_str.as_deref(),
                &canonical,
                process_args.as_ref(),
                plakous_host_protocol::topology::is_topology_registration(
                    identity.registered_tool_id().as_str(),
                ),
            );
            // Once presentation renamed the call, the model-facing name is the
            // process tool it replaced. The restored row has no exposed name;
            // the live row must not keep one either, or the subject is a
            // process wait over an id the process tools cannot parse.
            let label_exposed = (name == canonical).then_some(exposed_name).flatten();
            let process_label = process_args.as_ref().and_then(|args| {
                super::tools::process_observation_label(&name, label_exposed, args)
            });
            let wait = wait_started(&name, process_label, process_args.as_ref());
            if let Some(call_id) = call_id_str.as_ref()
                && let Some(candidate) =
                    super::tool_facts::adopt_candidate_arg(&name, process_args.as_ref())
            {
                projection
                    .adopt_candidates
                    .insert(call_id.clone(), candidate);
            }
            let kind = crate::activity::tool_activity_kind(&name);
            let processes = process_label
                .map(|_| roots.job_registry().snapshot())
                .unwrap_or_default();
            if let (Some(args), Some(locator)) = (
                process_label.and(process_args.as_ref()),
                session.plakous_locator,
            ) {
                for id in &super::tools::process_ids(args) {
                    let key = super::tools::process_key(locator, id);
                    if let Some(process) = processes
                        .iter()
                        .find(|process| process.id == key && !process.command.is_empty())
                    {
                        projection
                            .process_commands
                            .insert(key, process.command.clone());
                    }
                }
            }
            let StartedToolText { subject, detail } = started_tool_text(
                &name,
                label_exposed,
                observation.as_ref(),
                info.as_ref(),
                ProcessContext {
                    label: process_label,
                    args: process_args.as_ref(),
                    locator: session.plakous_locator,
                    processes: &processes,
                    commands: &projection.process_commands,
                },
            );
            if process_label == Some("process_session_wait")
                && let Some(id) = call_id_str.as_ref()
                && let Some(args) = process_args.as_ref()
            {
                projection
                    .process_waits
                    .insert(id.clone(), super::tools::process_ids(args));
            }
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("tool.started")
                    .str("name", &name)
                    .opt_str(
                        "exposed_name",
                        identity
                            .exposed_name()
                            .map(plakous::model::ToolName::as_str),
                    )
                    .opt_str("call_id", call_id_str.as_deref())
                    .opt_str("subject", subject.as_deref())
                    .opt_str("arguments", detail.as_deref()),
            );
            let nested_parent = nested_parent_call_id(lineage, projection);
            let parent_call_id = nested_parent
                .clone()
                .or_else(|| continuation_parent_call_id(&name, lineage, projection));
            let cell_id = lineage.cell_id().map(str::to_owned);
            if let Some(call_id) = call_id_str.as_ref() {
                observe_tool_activity(
                    session,
                    sessions::ToolItem {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        exposed_name: row_exposed_name(
                            identity.canonical_name(),
                            identity.exposed_name(),
                            &name,
                        ),
                        kind: crate::activity::tool_activity_kind(&name),
                        state: "running".into(),
                        subject: subject.clone(),
                        detail: detail.clone(),
                        started_at_ms: Some(event_timestamp_ms(event)),
                        finished_at_ms: None,
                        parent_call_id: parent_call_id.clone(),
                        cell_id: cell_id.clone(),
                        wait: wait.clone(),
                    },
                );
            }
            // The Compose row is created on the cell's own `ToolStarted`, which
            // strictly precedes any nested start, so the renderer folds children
            // into an existing row and never re-parents.
            if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose
                && lineage.is_composition_marker()
                && let Some(call_id) = call_id_str.as_deref()
            {
                projection.open_compose(call_id, &name, cell_id.as_deref());
            }
            if let Some(parent) = nested_parent.as_deref() {
                let reopened = projection.observe_child_start(
                    parent,
                    call_id_str.as_deref(),
                    cell_id.as_deref(),
                    lineage.nested_ordinal(),
                );
                // §5.8: a child dispatched during the `wait*` phase puts the
                // yielded cell back in flight. Re-paint the marker as running
                // and persist it so, when the turn is cancelled, the sweep over
                // persisted `running` rows reaches this compose instead of leaving
                // a `✓ Compose` header over cancelled children. The persisted
                // `started_at_ms` is kept, so the cell's accumulated duration
                // survives the yield.
                if reopened {
                    let gap = projection.gap_observed;
                    if let Some(compose) = projection.cells.get(parent) {
                        emit_compose_update(events, session, compose, parent, gap, None);
                    }
                    if let Some(mut marker) = session.transcript.tool_activity(parent) {
                        "running".clone_into(&mut marker.state);
                        marker.finished_at_ms = None;
                        observe_tool_activity(session, marker);
                    }
                }
            }
            // The start stamp the row persisted above, when it has a call id.
            let started_at_ms = call_id_str.as_ref().map(|_| event_timestamp_ms(event));
            let _ = events.send(HostEvent::ToolLine {
                call_id: call_id_str,
                parent_call_id,
                cell_id,
                kind,
                name,
                state: "running".into(),
                subject,
                detail,
                duration_ms: None,
                started_at_ms,
                finished_at_ms: None,
                wait,
            });
        }
        AgentEventKind::ToolProgress {
            identity,
            lineage,
            progress,
            ..
        } => {
            // Keep row alive; never clobber subject. Optional progress detail only
            // when we have no expand body yet (TUI merges carefully). The row
            // keeps the name its start gave it (a Grok await reads as one).
            let name = presented_later_name(
                projection,
                call_id_str.as_deref(),
                identity.canonical_name().as_str().to_owned(),
                false,
            );
            let progress_detail = progress.as_ref().and_then(|p| {
                let msg = p.message().trim();
                if msg.is_empty() {
                    None
                } else {
                    Some(msg.chars().take(200).collect::<String>())
                }
            });
            let kind = crate::activity::tool_activity_kind(&name);
            let _ = events.send(HostEvent::ToolLine {
                call_id: call_id_str
                    .or_else(|| progress.as_ref().map(|p| p.call_id().as_uuid().to_string())),
                parent_call_id: lineage_parent_call_id(lineage),
                cell_id: lineage.cell_id().map(str::to_owned),
                kind,
                name,
                state: "running".into(),
                subject: None,
                detail: progress_detail,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            });
        }
        AgentEventKind::ToolCompleted {
            identity,
            lineage,
            observation,
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let name = identity.canonical_name().as_str().to_owned();
            let prior = call_id_str
                .as_ref()
                .and_then(|call_id| session.transcript.tool_activity(call_id));
            let name = presented_later_name(projection, call_id_str.as_deref(), name, true);
            // A bare pi `wait` that ran the tree's await returned its receipt
            // (a process await returns `tasks`): it reads as the await from
            // here on, as its restored row does (presentation only).
            let settled_name = plakous_host_protocol::topology::presented_settled_name(
                &name,
                None,
                observation
                    .as_ref()
                    .filter(|item| item.succeeded())
                    .and_then(|item| serde_json::from_str::<serde_json::Value>(item.preview()).ok())
                    .as_ref(),
                plakous_host_protocol::topology::is_topology_registration(
                    identity.registered_tool_id().as_str(),
                ),
            );
            let bare_await = settled_name != name;
            let name = if bare_await {
                if let Some(call_id) = call_id_str.as_ref() {
                    projection.process_waits.remove(call_id);
                }
                "wait_agent".to_owned()
            } else {
                name
            };
            if name == "process_session_start"
                && let Some(locator) = session.plakous_locator
                && let Some(command) = prior.as_ref().and_then(|row| row.subject.as_ref())
                && let Some(result) = observation
                    .as_ref()
                    .and_then(|item| serde_json::from_str::<serde_json::Value>(item.preview()).ok())
            {
                for id in super::tools::process_ids(&result) {
                    if id != "(unknown)" {
                        projection
                            .process_commands
                            .insert(super::tools::process_key(locator, &id), command.clone());
                    }
                }
            }
            // Spec 02 §5.4: the expand body is readable text, never `Arguments:` /
            // `Result:` JSON dumps.
            let result = observation
                .as_ref()
                .map(|item| (item.succeeded(), item.preview()));
            let result_detail = completed_tool_detail(
                prior
                    .as_ref()
                    .and_then(|activity| activity.detail.as_deref()),
                result.map(|(_, preview)| preview),
            );
            let kind = crate::activity::tool_activity_kind(&name);
            // A call that succeeded may still report a command that failed:
            // the row names how it ended rather than painting a check (C8).
            let state = completed_tool_state(&name, result);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("tool.completed")
                    .str("name", &name)
                    .opt_str(
                        "exposed_name",
                        identity
                            .exposed_name()
                            .map(plakous::model::ToolName::as_str),
                    )
                    .opt_str("call_id", call_id_str.as_deref())
                    .str("state", &state)
                    .opt_str("detail", result_detail.as_deref()),
            );
            // A cell that yields before it dispatches anything never named its
            // cell on a start event — the interpreter mints the cell after the
            // marker begins — so `open_compose` learnt nothing and the `wait*`
            // continuation had no row to fold under. The marker's own terminal
            // lineage carries the retired cell, so the ownership is learnt here,
            // before anything reads it.
            if lineage.is_composition_marker()
                && let Some(call_id) = call_id_str.as_deref()
            {
                projection.learn_marker_cell(call_id, lineage.cell_id());
            }
            let nested_parent = nested_parent_call_id(lineage, projection);
            let parent_call_id = nested_parent
                .clone()
                .or_else(|| continuation_parent_call_id(&name, lineage, projection));
            let mut cell_id = lineage.cell_id().map(str::to_owned);
            // the marker owns the cell, so its own terminal line
            // carries the cell the host learnt even when the driver has already
            // retired it.
            if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose
                && cell_id.is_none()
                && let Some(compose) = call_id_str
                    .as_deref()
                    .and_then(|call_id| projection.cells.get(call_id))
            {
                cell_id.clone_from(&compose.cell_id);
            }
            // §5.2: the completed header carries the host's own result-derived
            // counts. Only the host parses tool output for them.
            let completed_subject =
                if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose {
                    call_id_str
                        .as_deref()
                        .and_then(|call_id| projection.cells.get(call_id))
                        .map(|compose| compose_counts_label(compose, projection.gap_observed))
                } else {
                    completed_tool_summary(
                        &name,
                        prior.as_ref().and_then(|row| row.subject.as_deref()),
                        result.map(|(_, preview)| preview),
                    )
                };
            let result_succeeded = matches!(observation.as_ref(), Some(item) if item.succeeded());
            let started_wait = prior.as_ref().and_then(|row| row.wait.clone());
            let started_wait = if bare_await {
                Some(super::tool_facts::bare_await_started(started_wait))
            } else {
                started_wait
            };
            // A topology call's whole result, as its tool left it: the
            // observation preview is bounded and stops being JSON on a
            // fan-out wait or a long conflict list.
            let call_facts = call_id_str
                .as_deref()
                .filter(|_| matches!(name.as_str(), "wait_agent" | "adopt_candidate"))
                .and_then(plakous_recursive_subagents::call_facts::take);
            let wait_result = match &call_facts {
                Some(plakous_recursive_subagents::call_facts::TopologyCallFacts::Wait(result)) => {
                    Some(result.clone())
                }
                // A bare pi `wait` renamed here always leaves facts; the
                // preview is only the fallback its restored row reads too.
                _ if name == "wait_agent" && !bare_await => None,
                _ => observation.as_ref().and_then(|item| {
                    serde_json::from_str::<serde_json::Value>(item.preview()).ok()
                }),
            };
            let wait = wait_settled(
                &name,
                started_wait,
                wait_result.as_ref(),
                result_succeeded,
                prior
                    .as_ref()
                    .and_then(|row| row.started_at_ms)
                    .and_then(|started| event_timestamp_ms(event).checked_sub(started)),
                call_id_str
                    .as_ref()
                    .and_then(|id| projection.process_waits.get(id))
                    .map(Vec::as_slice),
            );
            let completed_subject = if bare_await {
                wait_result
                    .as_ref()
                    .and_then(super::tool_facts::await_children)
            } else {
                completed_subject
            };
            let completed_subject = call_id_str
                .as_ref()
                .and_then(|id| projection.process_waits.remove(id))
                .map(|ids| {
                    settled_process_wait_subject(
                        &ids,
                        result.map(|(_, preview)| preview),
                        session.plakous_locator,
                        &roots.job_registry().snapshot(),
                        &projection.process_commands,
                    )
                })
                .or_else(|| completed_subject.map(|subject| live_tool_subject(&subject)));
            // A tool that joins a cancel reports a failed observation like any
            // other. Under a requested cancel the row is `cancelled`, so the
            // observed path and `observe_terminal_tool_activity`'s settlement
            // sweep cannot disagree about the same call.
            let row_state = if state == "succeeded" {
                "done"
            } else if projection.cancel_requested {
                "cancelled"
            } else {
                state.as_str()
            };
            // The Compose row this terminal settles: the marker's own call for an
            // `exec*`, and the cell it names for a `wait*` continuation of a
            // cell the host already opened a row for.
            let settled_compose = lineage.is_composition_marker().then(|| {
                call_id_str
                    .as_deref()
                    .filter(|call_id| projection.cells.contains_key(*call_id))
                    .map(str::to_owned)
                    .or_else(|| projection.marker_for_cell(cell_id.as_deref()))
            });
            // A marker terminal settles its Compose row's state so a child
            // dispatched during a later `wait*` cannot flip it back to running.
            // It is recorded *before* the row is painted or persisted: a cell
            // that yields while a child is still in flight has not ended, so
            // `row_state()` — not the local terminal — is what the line and the
            // transcript row carry. Otherwise a cancel in that window left
            // `✓ Compose` sitting over `⊘` children, the case `row_state` exists
            // to prevent (§5.8).
            let marker_settled_here = match settled_compose.as_ref() {
                Some(Some(marker_call)) => {
                    let gap = projection.gap_observed;
                    let own = call_id_str.as_deref() == Some(marker_call.as_str());
                    projection.observe_compose_terminal(marker_call, row_state);
                    if !own && let Some(compose) = projection.cells.get(marker_call) {
                        emit_compose_update(
                            events,
                            session,
                            compose,
                            marker_call,
                            gap,
                            observed_call_duration_ms(
                                session,
                                marker_call,
                                event_timestamp_ms(event),
                            ),
                        );
                    }
                    own
                }
                _ => false,
            };
            let own_compose_row_state = marker_settled_here
                .then(|| {
                    call_id_str
                        .as_deref()
                        .and_then(|call_id| projection.cells.get(call_id))
                        .map(|compose| compose.row_state().to_owned())
                })
                .flatten();
            let persisted_state = match own_compose_row_state.as_deref() {
                // The marker yielded with children still running: the cell is
                // not over, so the transcript keeps it `running` and the
                // re-terminalisation below writes its outcome.
                Some("running") => "running",
                // A cancelled call is recorded as cancelled, so a reopen paints
                // the same row the live surface saw and the sweep does not find
                // it still running.
                _ if projection.cancel_requested && state != "succeeded" => "cancelled",
                _ => state.as_str(),
            };
            if let Some(call_id) = call_id_str.as_ref() {
                observe_tool_activity(
                    session,
                    sessions::ToolItem {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        exposed_name: row_exposed_name(
                            identity.canonical_name(),
                            identity.exposed_name(),
                            &name,
                        ),
                        kind: crate::activity::tool_activity_kind(&name),
                        state: persisted_state.into(),
                        subject: completed_subject.clone().or_else(|| {
                            prior
                                .as_ref()
                                .filter(|_| !bare_await)
                                .and_then(|a| a.subject.clone())
                        }),
                        detail: result_detail.clone().or_else(|| {
                            prior.as_ref().and_then(|activity| activity.detail.clone())
                        }),
                        started_at_ms: prior.as_ref().and_then(|activity| activity.started_at_ms),
                        finished_at_ms: (persisted_state != "running")
                            .then(|| event_timestamp_ms(event)),
                        parent_call_id: parent_call_id.clone(),
                        cell_id: cell_id.clone(),
                        wait: wait.clone(),
                    },
                );
            }
            let compose_subject = completed_subject;
            // The row's own wall time, from the `started_at_ms` this terminal
            // just persisted alongside. A yielded cell keeps its first start,
            // so the head reports the whole cell and not the last segment
            let duration_ms = call_id_str.as_deref().and_then(|call_id| {
                observed_call_duration_ms(session, call_id, event_timestamp_ms(event))
            });
            let line = HostEvent::ToolLine {
                call_id: call_id_str.clone(),
                parent_call_id: parent_call_id.clone(),
                cell_id,
                kind,
                name,
                state: own_compose_row_state.unwrap_or_else(|| row_state.to_owned()),
                subject: compose_subject,
                detail: result_detail,
                duration_ms,
                started_at_ms: prior.as_ref().and_then(|activity| activity.started_at_ms),
                finished_at_ms: (persisted_state != "running").then(|| event_timestamp_ms(event)),
                wait,
            };
            // Inside a cancel window the cancel path publishes, so the line and
            // the `interrupted` status it precedes reach the surface in one
            // order however fast the tool reported its own terminal.
            if projection.cancel_requested {
                projection.cancel_window_terminals.push(line);
            } else {
                let _ = events.send(line);
            }
            if let Some(candidate) = call_id_str
                .as_ref()
                .and_then(|id| projection.adopt_candidates.remove(id))
                && let Some(facts) = call_facts
                && let Some(milestone) =
                    super::tool_facts::adopt_milestone(&candidate, facts, |candidate| {
                        last_tree_revision
                            .runtime(rows, session)?
                            .read(|tree| {
                                let agent = tree
                                    .candidates()
                                    .values()
                                    .find(|record| record.candidate_id.as_str() == candidate)?
                                    .producing_agent_id
                                    .clone();
                                let template = tree
                                    .nodes()
                                    .get(&agent)
                                    .and_then(|node| node.template.as_ref())
                                    .map(ToString::to_string);
                                Some((agent, template))
                            })
                            .ok()
                            .flatten()
                    })
            {
                let row = TranscriptRow::Milestone(milestone);
                let _ = commit_session_update(store, home, session, |candidate| {
                    candidate.record_notice(row.clone());
                    true
                });
                let _ = events.send(HostEvent::Row(row));
            }
            // A `wait*` terminal that closed the last child re-terminalises the
            // marker's persisted row, so the `Completed` sweep never rewrites a
            // cell that succeeded to `failed:observation-gap`.
            if let Some(Some(marker_call)) = settled_compose.as_ref()
                && !marker_settled_here
                && let Some(compose) = projection.cells.get(marker_call)
            {
                observe_settled_compose_marker(
                    session,
                    compose,
                    projection.gap_observed,
                    marker_call,
                    event_timestamp_ms(event),
                );
            }
            // A child's terminal state updates its owning Compose row's tally,
            // live and in the persisted row a resume reloads.
            if let Some(parent) = nested_parent.as_deref() {
                let gap = projection.gap_observed;
                let tally = projection
                    .observe_child_terminal(parent, call_id_str.as_deref(), state != "succeeded")
                    .map(|compose| {
                        emit_compose_update(
                            events,
                            session,
                            compose,
                            parent,
                            gap,
                            observed_call_duration_ms(session, parent, event_timestamp_ms(event)),
                        );
                        compose_counts_label(compose, gap)
                    });
                if let Some(tally) = tally {
                    observe_compose_subject(session, parent, tally);
                    // The last observed child of a yielded cell closing is what
                    // ends the cell; write its outcome back.
                    if let Some(compose) = projection.cells.get(parent) {
                        observe_settled_compose_marker(
                            session,
                            compose,
                            gap,
                            parent,
                            event_timestamp_ms(event),
                        );
                    }
                }
            }
            emit_tree_snapshot_if_revision_advanced(
                rows,
                store,
                home,
                session,
                last_tree_revision,
                events,
                false,
            );
        }
        AgentEventKind::ToolPreparationRejected {
            attempt,
            lineage,
            reason,
            detail,
            observation,
            ..
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let name = attempt.model_facing_name().as_str().to_owned();
            // A rejected call never starts, so the subject stash is empty for
            // it and both identifying fields used to come out null. The event
            // now carries its own bounded redacted subject and arguments; the
            // stash stays the fallback for a call that did start.
            let info = tool_subjects.take(&name, call_id_str.as_deref());
            let rejected_subject = observation
                .as_ref()
                .and_then(ToolCallObservation::subject)
                .map(str::to_owned)
                .or_else(|| info.as_ref().map(|item| item.subject.clone()));
            let rejected_arguments = observation
                .as_ref()
                .map(|observation| observation.arguments_preview().to_owned())
                .or_else(|| info.as_ref().map(|item| item.detail.clone()));
            // The category alone does not diagnose a rejection: every schema
            // refusal is `InvalidArguments`. The descriptive message says
            // which constraint the call missed.
            let rejected_reason = tool_rejected_reason(&format!("{reason:?}"), detail.as_deref());
            let call_id = call_id_str.clone().unwrap_or_else(|| {
                format!(
                    "rejected-{}-{}",
                    event_timestamp_ms(event),
                    session.transcript.len()
                )
            });
            observe_tool_activity(
                session,
                sessions::ToolItem {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    exposed_name: None,
                    kind: crate::activity::tool_activity_kind(&name),
                    state: format!("failed:rejected:{reason:?}"),
                    subject: rejected_subject.clone(),
                    detail: rejected_arguments.clone(),
                    started_at_ms: None,
                    finished_at_ms: Some(event_timestamp_ms(event)),
                    parent_call_id: lineage_parent_call_id(lineage),
                    cell_id: lineage.cell_id().map(str::to_owned),
                    wait: None,
                },
            );
            log_event(
                log,
                observed_session_id,
                tool_rejected_record(
                    &name,
                    &rejected_reason,
                    &call_id,
                    rejected_subject.as_deref(),
                    rejected_arguments.as_deref(),
                ),
            );
            let parent_call_id = lineage_parent_call_id(lineage);
            let _ = events.send(HostEvent::ToolLine {
                call_id: Some(call_id.clone()),
                parent_call_id: parent_call_id.clone(),
                cell_id: lineage.cell_id().map(str::to_owned),
                kind: crate::activity::tool_activity_kind(&name),
                name,
                state: format!("rejected:{reason:?}"),
                subject: info.as_ref().map(|i| i.subject.clone()),
                detail: info.map(|i| i.detail),
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: Some(event_timestamp_ms(event)),
                wait: None,
            });
            if let Some(parent) = parent_call_id.as_deref() {
                let gap = projection.gap_observed;
                let tally = projection
                    .observe_child_terminal(parent, Some(&call_id), true)
                    .map(|compose| {
                        emit_compose_update(
                            events,
                            session,
                            compose,
                            parent,
                            gap,
                            observed_call_duration_ms(session, parent, event_timestamp_ms(event)),
                        );
                        compose_counts_label(compose, gap)
                    });
                if let Some(tally) = tally {
                    observe_compose_subject(session, parent, tally);
                }
            }
        }
        AgentEventKind::EffectDecisionRequired {
            identity,
            decision,
            prompt,
            expires_at,
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            // this is the *only* constructor of an approval row.
            // The decision address stays host-side; the TUI receives an opaque,
            // session-scoped handle it can only echo back.
            let approval_id = format!("approval-{}", uuid::Uuid::new_v4());
            projection.approvals.insert(
                approval_id.clone(),
                PendingApproval {
                    decision: decision.clone(),
                },
            );
            projection
                .approval_by_decision
                .insert(decision.clone(), approval_id.clone());
            let tool_name = identity.canonical_name().as_str().to_owned();
            let info = tool_subjects.peek(&tool_name, call_id_str.as_deref());
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("effect.decision.required")
                    .str("name", &tool_name)
                    .opt_str("call_id", call_id_str.as_deref()),
            );
            let _ = events.send(HostEvent::ApprovalRequired {
                approval_id,
                call_id: call_id_str,
                tool_name,
                subject: info.map(|item| redact_text_preview(&item.subject)),
                prompt: prompt
                    .as_ref()
                    .map(|prompt| redact_text_preview(prompt.description().as_str())),
                expires_at_ms: timestamp_ms(*expires_at),
            });
        }
        AgentEventKind::EffectDecisionResolved {
            decision,
            resolution,
            ..
        } => {
            let Some(approval_id) = projection.approval_by_decision.remove(decision) else {
                return;
            };
            projection.approvals.remove(&approval_id);
            let resolution = match resolution {
                EffectDecisionResolution::Allowed => "allowed",
                EffectDecisionResolution::Denied => "denied",
                EffectDecisionResolution::Expired => "expired",
                EffectDecisionResolution::Cancelled => "cancelled",
                EffectDecisionResolution::Shutdown => "shutdown",
                EffectDecisionResolution::Invalidated => "invalidated",
                // The runtime enum is `#[non_exhaustive]`. A resolution this
                // host does not know is reported as unknown rather than
                // mislabelled as one of the six it does.
                _ => "unknown",
            };
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("effect.decision.resolved").str("resolution", resolution),
            );
            let _ = events.send(HostEvent::ApprovalResolved {
                approval_id,
                resolution: resolution.to_owned(),
            });
        }
        AgentEventKind::ObservationClipped {
            name,
            bytes_kept,
            bytes_dropped,
            bound,
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("observation.clipped")
                    .str("name", name.as_ref())
                    .opt_str("call_id", call_id_str.as_deref())
                    .u64("bytes_kept", *bytes_kept)
                    .u64("bytes_dropped", *bytes_dropped)
                    .u64("bound", *bound),
            );
            let _ = events.notice(TranscriptRow::info(
                NoticeScope::Clipped,
                observation_clipped_note(name, *bytes_kept, *bytes_dropped, *bound),
            ));
        }
        AgentEventKind::ObservationReplaced {
            bytes_before,
            bytes_after,
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            log_event(
                log,
                observed_session_id,
                observation_replaced_record(call_id_str.as_deref(), *bytes_before, *bytes_after),
            );
        }
        AgentEventKind::SteeringQueued { position } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let _ = events.send(HostEvent::SteerAck {
                state: "queued".into(),
                position: Some(*position),
            });
        }
        AgentEventKind::SteeringApplied => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let _ = events.send(HostEvent::SteerAck {
                state: "applied".into(),
                position: None,
            });
        }
        AgentEventKind::ModelRequestQueued { slots } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("model.request_queued").str("account", slots),
            );
            let _ = events.send(HostEvent::ModelRequestQueued {
                slots: slots.clone(),
            });
        }
        AgentEventKind::OperationBudgetExhausted => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("turn.operation_budget_exhausted")
                    .str("detail", OPERATION_BUDGET_EXHAUSTED_NOTE),
            );
            let _ = events.notice(TranscriptRow::warning(
                NoticeScope::Turn,
                OPERATION_BUDGET_EXHAUSTED_NOTE.to_owned(),
            ));
        }
        AgentEventKind::ModelStarted => {
            settle_live_thinking(projection, session, events);
        }
        AgentEventKind::OutputLimitRetry { bound } => {
            discard_response_reasoning(projection, events);
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let note = output_limit_retry_note(*bound);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("turn.output_limit_retry").str("detail", &note),
            );
            let _ = events.notice(TranscriptRow::info(NoticeScope::Retry, note));
        }
        AgentEventKind::OutputBoundRederived { previous, current } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let figure =
                |bound: Option<std::num::NonZeroU32>| bound.map_or(0, std::num::NonZeroU32::get);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("session.output_bound_rederived")
                    .u64("previous", u64::from(figure(*previous)))
                    .u64("current", u64::from(figure(*current))),
            );
            let _ = events.notice(TranscriptRow::warning(
                NoticeScope::Settings,
                output_bound_rederived_note(*previous, *current),
            ));
        }
        AgentEventKind::ToolResultsReplaced {
            results,
            bytes,
            tokens_before,
            tokens_after,
        } => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("context.placeholders")
                    .u64("results", u64::from(*results))
                    .u64("bytes", *bytes)
                    .u64("tokens_before", *tokens_before)
                    .u64("tokens_after", *tokens_after),
            );
            // The Compact row already carries before → after sizes. Dumping
            // the operator essay as a grey SystemNote was the live TUI
            // transcript noise on Grok growth turns.
        }
        AgentEventKind::CompactionStarted {
            workflow_id,
            reason,
        } => {
            let reason_label = match reason {
                CompactionEventReason::Manual => "manual",
                CompactionEventReason::Scheduled => "scheduled",
            };
            log_event(
                log,
                observed_session_id,
                SessionEvent::new("compact.started").str("reason", reason_label),
            );
            // Spec 02 §5.6: a compaction the operator did not type still has to be
            // visible while it runs. Only `/compact` returns a
            // `CompactSessionOutcome` the idle loop can report; the tool-group
            // fence publishes its start and end as turn events, and logging
            // them alone made an automatic compaction a silent no-op on screen.
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let _ = events.send(HostEvent::Activity {
                label: COMPACTING_ACTIVITY_LABEL.into(),
                active: true,
            });
            let call_id = sessions::projector::compaction_call_id(workflow_id.as_bytes());
            publish_compact_activity(session, events, &call_id, "running", None, None);
        }
        AgentEventKind::CompactionFinished {
            workflow_id,
            outcome,
            elapsed_ms,
            summary,
            detail: outcome_detail,
        } => {
            let outcome_detail = outcome_detail.as_deref();
            log_event(
                log,
                observed_session_id,
                compact_finished_record(*outcome, *elapsed_ms, *summary, outcome_detail),
            );
            let (occupancy_before, occupancy_after) = compact_row_token_pair(*summary);
            if matches!(outcome, CompactionEventOutcome::Committed) {
                *compacted_mid_turn = true;
            }
            flush_assistant_delta(delta_buf, last_flush, events, true);
            let _ = events.send(HostEvent::Activity {
                label: COMPACTING_ACTIVITY_LABEL.into(),
                active: false,
            });
            let quiet = outcome_detail.is_some_and(|detail| {
                detail.contains("deadline elapsed")
                    || detail.contains("no authoritative completion")
            });
            if matches!(outcome, CompactionEventOutcome::RecoveryRequired) {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Compaction,
                    compact_finished_note(*outcome, *summary, outcome_detail),
                ));
            } else if quiet {
                let _ = events.notice(TranscriptRow::info(
                    NoticeScope::Compaction,
                    COMPACT_PROVIDER_QUIET_NOTE,
                ));
                let call_id = sessions::projector::compaction_call_id(workflow_id.as_bytes());
                publish_compact_activity(
                    session,
                    events,
                    &call_id,
                    "failed",
                    None,
                    Some(*elapsed_ms),
                );
            } else {
                let call_id = sessions::projector::compaction_call_id(workflow_id.as_bytes());
                let (state, subject) = match *outcome {
                    CompactionEventOutcome::Committed => (
                        compact_terminal_state(true, true),
                        compact_terminal_subject(occupancy_before, occupancy_after, true, true),
                    ),
                    CompactionEventOutcome::NotNeeded => ("done", Some("already fits".to_owned())),
                    _ => ("failed", None),
                };
                publish_compact_activity(
                    session,
                    events,
                    &call_id,
                    state,
                    subject,
                    Some(*elapsed_ms),
                );
            }
        }
        _ => {
            flush_assistant_delta(delta_buf, last_flush, events, true);
        }
    }
}

/// What the operator sees when a turn spends its model-operation budget.
///
/// The runtime injects a note for the model at the same moment, but canonical
/// history is not what the transcript or the session log reads. Without this
/// line the user gets a wrap-up answer and no reason for it, and a resumed
/// session shows nothing at all.
pub(super) const OPERATION_BUDGET_EXHAUSTED_NOTE: &str =
    "turn reached its model-operation budget; answering with findings so far";

fn output_limit_retry_note(bound: Option<std::num::NonZeroU32>) -> String {
    bound.map_or_else(
        || "response reached the provider output limit before completion; retrying with less output".to_owned(),
        |bound| {
            format!(
                "response reached its {}-token output limit before completion; retrying with less output",
                bound.get()
            )
        },
    )
}

/// The one line an operator sees when a reopened session's route moved its budget.
///
/// The session keeps the figure it was created with. That figure reaches the
/// configuration identity and from there the persisted harness plan, the tool
/// projection's provider target, the request prefix and the admission evidence,
/// none of which can be re-signed on reopen, so adopting the route's new figure
/// would make the session unopenable rather than roomier. An old session on a
/// route that has since grown is therefore starved, and the operator needs to
/// be told both figures and the one action that gets the larger one.
pub(crate) fn output_bound_rederived_note(
    previous: Option<std::num::NonZeroU32>,
    current: Option<std::num::NonZeroU32>,
) -> String {
    let render = |bound: Option<std::num::NonZeroU32>| {
        bound.map_or_else(|| "none".to_owned(), |bound| bound.get().to_string())
    };
    format!(
        "plak: this session keeps its output budget of {} tokens; the route now attests {}; \
         start a new session to use it",
        render(previous),
        render(current)
    )
}

/// The operator line for a tool result the observation budget truncated.
///
/// Says which tool, how much went and how much stayed, and against what
/// budget, because "truncated" without a figure tells a reader nothing about
/// whether the model saw enough. The model is answering from what remains, so
/// the line also says what to do about it.
pub(super) fn observation_clipped_note(
    name: &str,
    bytes_kept: u64,
    bytes_dropped: u64,
    bound: u64,
) -> String {
    format!(
        "observation clipped: {name} returned more than the remaining observation budget; \
         {bytes_kept} bytes kept, {bytes_dropped} dropped, against a {bound}-byte bound. \
         The model is answering from the kept portion; narrow the call to see the rest."
    )
}

fn observation_replaced_record(
    call_id: Option<&str>,
    bytes_before: u64,
    bytes_after: u64,
) -> SessionEvent {
    SessionEvent::new("observation.replaced")
        .opt_str("call_id", call_id)
        .u64("bytes_before", bytes_before)
        .u64("bytes_after", bytes_after)
}

/// The one distinct operator text per compaction outcome (spec 02 §5.6).
///
/// Shared with `handle_compact` so the fence-driven and the `/compact` paths
/// never describe the same outcome two different ways.
pub(super) const fn compaction_event_outcome_detail(
    outcome: CompactionEventOutcome,
) -> &'static str {
    match outcome {
        CompactionEventOutcome::Committed => COMPACT_COMMITTED_DETAIL,
        CompactionEventOutcome::NotNeeded => COMPACT_NOT_NEEDED_DETAIL,
        CompactionEventOutcome::NoEligibleRange => COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
        CompactionEventOutcome::Unavailable => COMPACT_UNAVAILABLE_DETAIL,
        CompactionEventOutcome::Failed => COMPACT_FAILED_DETAIL,
        CompactionEventOutcome::RecoveryRequired => COMPACT_RECOVERY_REQUIRED_DETAIL,
    }
}

/// The one line the operator reads when an unfinished turn is closed. Exactly
/// one of these is true of a cut, and each says only its own thing: a
/// workspace warning over a command that provably never started would teach
/// the operator to skip the warning that matters.
pub(crate) const fn abandoned_turn_operator_line(
    report: &plakous::agent::AbandonedTurnReport,
) -> &'static str {
    abandoned_turn_operator_detail(
        report.claim(),
        report.cause(),
        report.tool_round_completed(),
        report.model_round_completed(),
    )
}

pub(crate) const fn abandoned_turn_operator_detail(
    claim: plakous::agent::AbandonedTurnClaim,
    cause: plakous::agent::AbandonmentCause,
    tool_round_completed: bool,
    model_round_completed: bool,
) -> &'static str {
    if tool_round_completed && matches!(claim, plakous::agent::AbandonedTurnClaim::RunningTool) {
        return "plak: recovered an interrupted turn; its tool round completed before the next \
                model request, but a process it started has an unknown outcome; completed tool \
                results were kept; check the workspace before carrying on";
    }
    match (claim, cause) {
        (plakous::agent::AbandonedTurnClaim::ToolRoundCompleted, _) if !model_round_completed => {
            "plak: recovered an interrupted turn; its tool round completed, but the turn was \
             interrupted before the next model request; completed tool results were kept"
        }
        (plakous::agent::AbandonedTurnClaim::ToolRoundCompleted, _) => {
            "plak: recovered an interrupted turn; its last round completed, but the turn was \
             interrupted before the next model request; completed work was kept"
        }
        (plakous::agent::AbandonedTurnClaim::RunningTool, _) => {
            "plak: recovered an interrupted turn; a tool may have changed this workspace; \
             its result was not added to the conversation and it is not retried - check \
             the workspace before carrying on"
        }
        (plakous::agent::AbandonedTurnClaim::ModelRequest, _) => {
            "plak: recovered an interrupted turn; its model request may have been processed \
             and billed; nothing from it was kept"
        }
        (plakous::agent::AbandonedTurnClaim::CompactionNotApplied, _) => {
            "plak: recovered an interrupted turn; compaction was not applied (it may have been \
             billed) and the conversation is unchanged"
        }
        (_, plakous::agent::AbandonmentCause::ProcessLost) => {
            "plak: recovered an interrupted turn; exact evidence proves its pending work \
             never started, so nothing after that boundary ran"
        }
        _ => {
            "plak: recovered an interrupted turn; nothing it asked for had started, so nothing \
             ran and nothing was changed"
        }
    }
}

/// The row name a tool start shows: a Grok output or kill call naming a child
/// reads as the await or the interrupt it is (presentation only), and the
/// call's later lines keep that name.
/// The exposed name a live row carries: the name the model called, when it
/// differs from the canonical row name and presentation did not rename the
/// call (a renamed call keeps no exposed name, as its restored row does).
fn row_exposed_name(
    canonical: &plakous::model::ToolName,
    exposed: Option<&plakous::model::ToolName>,
    name: &str,
) -> Option<String> {
    exposed
        .map(plakous::model::ToolName::as_str)
        .filter(|exposed| *exposed != name && name == canonical.as_str())
        .map(str::to_owned)
}

pub(super) fn presented_start_name<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    call_id: Option<&str>,
    canonical: &str,
    arguments: Option<&serde_json::Value>,
    topology_call: bool,
) -> String {
    let name =
        plakous_host_protocol::topology::presented_slot_name(canonical, arguments, topology_call);
    if name != canonical
        && let Some(call_id) = call_id
    {
        projection
            .presented_names
            .insert(call_id.to_owned(), name.to_owned());
    }
    name.to_owned()
}

/// The row name a progress or terminal line of a call shows: the one its
/// start gave it. A terminal line forgets it.
pub(super) fn presented_later_name<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    call_id: Option<&str>,
    canonical: String,
    terminal: bool,
) -> String {
    let Some(call_id) = call_id else {
        return canonical;
    };
    let presented = if terminal {
        projection.presented_names.remove(call_id)
    } else {
        projection.presented_names.get(call_id).cloned()
    };
    presented.unwrap_or(canonical)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;

    #[test]
    fn retry_notice_names_the_network_phase() {
        use plakous::model::{ModelRetry, ModelRetryCause, ModelRetryLoss, ModelRetryPhase};
        let dir = tempfile::tempdir().unwrap();
        let roots = crate::agent::ProductStateRoots::with_doubles(
            dir.path().to_path_buf(),
            dir.path().join("state"),
        );
        let rows = test_kit::MemoryRows::new();
        let store = crate::session_store::MemoryStore::new();
        let home = SessionHome::new(dir.path().join("sessions"));
        let mut session = SessionRecord::new(dir.path().to_path_buf());
        let uuid = uuid::Uuid::from_u128(1);
        let retry = ModelRetry::new(
            2,
            4,
            Duration::from_secs(2),
            ModelRetryCause::Connect,
            false,
            "raw transport detail",
        )
        .with_loss(ModelRetryLoss {
            transport_phase: Some(ModelRetryPhase::Resolution),
            ..ModelRetryLoss::default()
        });
        let event: AgentEvent = serde_json::from_value(serde_json::json!({
            "branch_sequence": 1, "event_id": uuid, "session_id": uuid, "branch_id": uuid,
            "agent_id": uuid, "run_id": uuid, "turn_id": null, "attempt_id": null, "call_id": null,
            "timestamp": {"seconds": 1, "nanoseconds": 0}, "causal_parent": null,
            "origin": "Model", "classification": "Public", "event_schema": 3,
            "projection_revision": 6, "visibility": "Content", "scope": "Agent", "event_kind_version": 11,
            "kind": {"Model": {"kind": "Retry", "event": {"Retry": retry}}},
            "retained_bytes": 512, "metadata_retained_bytes": 512, "content_retained_bytes": 0
        })).unwrap();
        let (events, rx) = host_event_sink();
        apply_event(
            &event,
            &roots,
            &rows,
            &store,
            &home,
            &mut session,
            &events,
            &mut String::new(),
            &mut String::new(),
            &mut Instant::now(),
            &mut ToolSubjectStash::new(),
            &mut TurnProjection::default(),
            None,
            "test",
            &mut TreeSnapshotPush::default(),
            &mut false,
            &mut 0,
        );
        let notice = rx
            .try_iter()
            .find_map(|event| match event {
                HostEvent::ModelRetry(notice) => Some(notice),
                _ => None,
            })
            .expect("retry notice");
        assert_eq!(
            notice.cause,
            "provider unreachable (DNS lookup failed before anything was sent); waiting for network, try 2 in 2s"
        );
        assert_eq!(notice.cause_kind, "connect");
    }

    #[test]
    fn completed_round_recovery_notice_does_not_invent_tool_results() {
        let line = abandoned_turn_operator_detail(
            plakous::agent::AbandonedTurnClaim::ToolRoundCompleted,
            plakous::agent::AbandonmentCause::ProcessLost,
            true,
            true,
        );
        assert!(line.contains("last round completed"));
        assert!(line.contains("before the next model request"));
        assert!(line.contains("completed work was kept"));
        assert!(!line.contains("tool results"));
    }

    #[test]
    fn a2_real_tool_round_keeps_original_operator_line() {
        assert_eq!(
            abandoned_turn_operator_detail(
                plakous::agent::AbandonedTurnClaim::ToolRoundCompleted,
                plakous::agent::AbandonmentCause::ProcessLost,
                true,
                false
            ),
            "plak: recovered an interrupted turn; its tool round completed, but the turn was interrupted before the next model request; completed tool results were kept"
        );
    }

    #[test]
    fn recovery_review_operator_line_distinguishes_ledger_from_conversation() {
        for cause in [
            plakous::agent::AbandonmentCause::ProcessLost,
            plakous::agent::AbandonmentCause::Interrupted,
        ] {
            assert_eq!(
                abandoned_turn_operator_detail(
                    plakous::agent::AbandonedTurnClaim::RunningTool,
                    cause,
                    false,
                    false
                ),
                "plak: recovered an interrupted turn; a tool may have changed this workspace; \
                 its result was not added to the conversation and it is not retried - check \
                 the workspace before carrying on"
            );
        }
    }

    #[test]
    fn process_wait_uses_its_result_after_timeout_exit_race() {
        use plakous::model::{Digest, DigestDomain};
        use plakous::tools::{JobHost, JobOwner, JobStatus, JobView};
        let dir = tempfile::tempdir().unwrap();
        let roots = crate::agent::ProductStateRoots::with_doubles(
            dir.path().to_path_buf(),
            dir.path().join("state"),
        );
        let empty_roots = crate::agent::ProductStateRoots::with_doubles(
            dir.path().to_path_buf(),
            dir.path().join("empty-state"),
        );
        let rows = test_kit::MemoryRows::new();
        let store = crate::session_store::MemoryStore::new();
        let home = SessionHome::new(dir.path().join("sessions"));
        let mut session = SessionRecord::new(dir.path().to_path_buf());
        let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
        session.plakous_locator = Some(locator);
        let (events, rx) = host_event_sink();
        let mut projection = TurnProjection::default();
        let mut stash = ToolSubjectStash::new();
        let mut assistant = String::new();
        let mut delta = String::new();
        let mut last_flush = Instant::now();
        let mut tree = TreeSnapshotPush::default();
        let mut compacted = false;
        let mut peak = 0;
        let uuid = uuid::Uuid::from_u128(1);
        let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
        {
            let mut dispatch = |roots: &crate::agent::ProductStateRoots,
                                name: &str,
                                call: u128,
                                kind: &str,
                                observation: serde_json::Value| {
                let identity = serde_json::json!({
                    "canonical_name": name,
                    "exposed_name": null,
                    "exposure_map_digest": null,
                    "registered_tool_id": "test.process",
                    "executor_id": "test.process",
                    "definition_digest": digest,
                    "schema_digest": digest,
                    "effect_metadata_digest": digest,
                    "argument_limits_digest": digest,
                });
                let event: AgentEvent = serde_json::from_value(serde_json::json!({
                    "branch_sequence": 1,
                    "event_id": uuid,
                    "session_id": locator.session_id(),
                    "branch_id": locator.branch_id(),
                    "agent_id": uuid,
                    "run_id": uuid,
                    "turn_id": null,
                    "attempt_id": null,
                    "call_id": uuid::Uuid::from_u128(call),
                    "timestamp": {"seconds": 1, "nanoseconds": 0},
                    "causal_parent": null,
                    "origin": "Tool",
                    "classification": "Public",
                    "event_schema": 3,
                    "projection_revision": 6,
                    "visibility": "Content",
                    "scope": "Agent",
                    "event_kind_version": 11,
                    "kind": {kind: {
                        "identity": identity,
                        "lineage": plakous::agent::ToolCallerLineageView::direct(),
                        "observation": observation,
                    }},
                    "retained_bytes": 512,
                    "metadata_retained_bytes": 512,
                    "content_retained_bytes": 0,
                }))
                .unwrap();
                apply_event(
                    &event,
                    roots,
                    &rows,
                    &store,
                    &home,
                    &mut session,
                    &events,
                    &mut assistant,
                    &mut delta,
                    &mut last_flush,
                    &mut stash,
                    &mut projection,
                    None,
                    "test",
                    &mut tree,
                    &mut compacted,
                    &mut peak,
                );
            };
            let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
            let completed = |result: serde_json::Value| serde_json::json!({"succeeded":true,"preview":result.to_string()});
            dispatch(
                &roots,
                "process_session_start",
                10,
                "ToolStarted",
                started(serde_json::json!({"command":"cargo test"})),
            );
            dispatch(
                &roots,
                "process_session_start",
                10,
                "ToolCompleted",
                completed(serde_json::json!({"session_id":7,"status":"running","exit_code":null})),
            );
            dispatch(
                &roots,
                "process_session_poll",
                11,
                "ToolStarted",
                started(serde_json::json!({"session_id":7,"yield_time_ms":1000})),
            );
            // The wait timed out, then the process exited before the host consumed it.
            roots.job_registry().observe_terminal(
                JobOwner {
                    session_id: locator.session_id(),
                    branch_id: locator.branch_id(),
                    agent_id: plakous::model::AgentId::try_from(uuid).unwrap(),
                },
                &[JobView {
                    number: 7,
                    command: "cargo test".into(),
                    started_at: plakous::model::Timestamp::new(1, 0).unwrap(),
                    owner: "root".into(),
                    elapsed: Duration::ZERO,
                    status: JobStatus::Exited(Some(0)),
                    output: String::new(),
                    session_id: [7; 16],
                    owner_generation: 1,
                }],
            );
            dispatch(
                &roots,
                "process_session_poll",
                11,
                "ToolCompleted",
                completed(serde_json::json!({"session_id":7,"status":"running","exit_code":null})),
            );
            // A later wait still knows the start row's command after registry eviction.
            dispatch(
                &empty_roots,
                "process_session_poll",
                12,
                "ToolStarted",
                started(serde_json::json!({"session_id":7,"yield_time_ms":1000})),
            );
            dispatch(
                &empty_roots,
                "process_session_poll",
                12,
                "ToolCompleted",
                completed(
                    serde_json::json!({"tasks":[{"task_id":"p7","status":"exited","exit_code":0}]}),
                ),
            );
        }
        let emitted = rx.try_iter().collect::<Vec<_>>();
        for (call, expected) in [
            (11, "Waited on process 7 · cargo test · still running"),
            (12, "Waited on process 7 · cargo test · exited 0"),
        ] {
            let id = uuid::Uuid::from_u128(call).to_string();
            assert_eq!(
                session
                    .transcript
                    .tool_activity(&id)
                    .unwrap()
                    .subject
                    .as_deref(),
                Some(expected)
            );
            assert!(emitted.iter().any(|event| matches!(event, HostEvent::ToolLine { call_id:Some(call_id), subject:Some(subject), .. } if call_id == &id && subject == expected)));
        }
    }

    #[test]
    fn live_tool_subject_redaction_is_idempotent() {
        let subject = live_tool_subject("GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list");
        assert_eq!(subject, "GITHUB_TOKEN=[redacted] gh pr list");
        assert_eq!(live_tool_subject(&subject), subject);
    }

    #[test]
    fn live_and_restored_subjects_match_at_the_byte_bound() {
        for (raw, expected) in [
            (
                format!("password={} safe subject", "x".repeat(230)),
                Some("password=[redacted] safe subject"),
            ),
            (
                "GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list".into(),
                Some("GITHUB_TOKEN=[redacted] gh pr list"),
            ),
            ("password=a ".repeat(30), None),
        ] {
            let subject = live_tool_subject(&raw);
            let encoded = sessions::ToolItem {
                call_id: "call-1".into(),
                parent_call_id: None,
                cell_id: None,
                name: "read_file".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("read_file"),
                state: "done".into(),
                subject: Some(subject.clone()),
                detail: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            };
            let restored: sessions::ToolItem =
                serde_json::from_value(serde_json::to_value(&encoded).unwrap()).unwrap();
            assert_eq!(restored.subject.as_deref(), Some(subject.as_str()));
            if let Some(expected) = expected {
                assert_eq!(subject, expected);
            } else {
                assert!(subject.ends_with('…'));
            }
            assert!(subject.len() <= 240);
        }
    }

    /// A reopen whose route moved says both figures and the one way to the new one.
    ///
    /// The session keeps the budget it was created with, because that figure is
    /// identity-bearing and the persisted artefacts cannot be re-signed. So an
    /// old session on a route that has since grown is starved, and the line has
    /// to say so: without the larger figure and the instruction, the operator
    /// has no way to learn the budget exists or how to reach it.
    #[test]
    #[allow(
        unused_qualifications,
        reason = "the test module has no NonZeroU32 import of its own"
    )]
    fn the_rederived_bound_note_names_both_figures() {
        let previous = std::num::NonZeroU32::new(4_096);
        let current = std::num::NonZeroU32::new(32_768);
        let note = output_bound_rederived_note(previous, current);
        assert!(note.contains("keeps its output budget"), "{note}");
        assert!(note.contains("4096"), "{note}");
        assert!(note.contains("32768"), "{note}");
        assert!(note.contains("start a new session"), "{note}");
        // A session created with no bound at all reads honestly, not as zero.
        let unbounded = output_bound_rederived_note(None, current);
        assert!(unbounded.contains("none"), "{unbounded}");
        assert!(unbounded.contains("32768"), "{unbounded}");
    }

    /// A route that reports usage only on its terminal response still peaks.
    ///
    /// 2026-09-18, live attempt 3: `peak_prepared_input_tokens` was null on
    /// every position on every route while the other new fields populated.
    /// The peak was recorded only from a mid-stream `ModelEvent::Usage`, which
    /// several routes never emit — the driver itself prefers the terminal
    /// response's counts (`response.usage().or(observed)`). Recording at the
    /// terminal response is what makes it route-independent.
    #[test]
    fn the_prepared_input_peak_records_a_terminal_response_usage() {
        let mut session = SessionRecord::new(std::env::temp_dir());
        let mut peak = 0_u64;
        // No mid-stream update at all, exactly like the routes that reported
        // null: only the operation's terminal counts.
        for input in [12_000_u64, 31_500, 27_000] {
            record_operation_prepared_input(
                &mut peak,
                &mut session,
                &plakous::model::UsageUpdate::new(plakous::model::Usage::new(input, 1, 0, 0, 0)),
                None,
            );
        }
        assert_eq!(peak, 31_500, "the largest single operation, not the sum");
        assert_ne!(peak, 0, "a turn with model operations must not report null");
    }

    use super::*;
    use crate::runtime::*;

    use crate::runtime::test_support::*;

    use crate::sessions::{self, SessionRecord};

    #[test]
    fn a_history_projection_diagnostic_goes_to_the_session_log_only() {
        use plakous::model::{Digest, DigestDomain, ModelDiagnostic};

        let dir = tempfile::tempdir().unwrap();
        let home = SessionHome::new(dir.path().join("sessions"));
        let session = SessionRecord::new(dir.path().to_path_buf());
        let log = SessionLog::open(&home, &session.id).unwrap();
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let identity = Digest::sha256(DigestDomain::HistoryProjection, 1, &[b"first"]).unwrap();
        publish_model_diagnostic(
            &ModelDiagnostic::historical_projection(identity, "projected history"),
            Some(&log),
            &session.id,
            &events,
        );
        assert!(rx.try_iter().next().is_none(), "never rendered");
        assert!(session.transcript.is_empty(), "never persisted");
        let logged = crate::session_log::read_tail_for_kind(
            &home,
            &session.id,
            4,
            Some("model.history_projection"),
        )
        .unwrap();
        assert_eq!(logged.len(), 1);
        assert!(logged[0].contains(&identity.to_string()), "{logged:?}");
    }

    #[test]
    fn a_generic_model_diagnostic_is_a_live_turn_notice_and_never_persisted() {
        use plakous::model::ModelDiagnostic;

        let dir = tempfile::tempdir().unwrap();
        let session = SessionRecord::new(dir.path().to_path_buf());
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        for _ in 0..2 {
            publish_model_diagnostic(
                &ModelDiagnostic::new("same text"),
                None,
                &session.id,
                &events,
            );
        }
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![
                HostEvent::Row(TranscriptRow::info(NoticeScope::Turn, "same text")),
                HostEvent::Row(TranscriptRow::info(NoticeScope::Turn, "same text")),
            ]
        );
        assert!(session.transcript.is_empty());
    }

    #[test]
    fn the_compact_row_subject_uses_occupancy_not_prepared_padding() {
        let occupancy = compact_terminal_subject(Some(44_900), Some(44_900), true, true)
            .expect("occupancy sizes");
        assert_eq!(occupancy, "44.9k → 44.9k");
        let dropped = compact_terminal_subject(Some(395_000), Some(44_900), true, true)
            .expect("overlay drop");
        assert_eq!(dropped, "395.0k → 44.9k");
        // The row states the successor's occupancy figures: the anchored
        // context measure before, the successor's `bytes÷4` after. The
        // prepared-input bound (395k → 223k) is the Context tab's admission
        // fact, not the row.
        let summary = CompactionSummary {
            rung: CompactionRung::ToolResultPlaceholders,
            segments: 1,
            input_tokens_before: 395_000,
            input_tokens_after: 222_700,
            occupancy_tokens_before: Some(138_600),
            occupancy_tokens_after: Some(44_900),
        };
        let (before, after) = compact_row_token_pair(Some(summary));
        let subject = compact_terminal_subject(before, after, true, true).expect("live overlay");
        assert_eq!(subject, "138.6k → 44.9k");
        let (prepared_before, prepared_after) = compact_row_token_pair(Some(CompactionSummary {
            occupancy_tokens_before: None,
            occupancy_tokens_after: None,
            ..summary
        }));
        assert_eq!(prepared_before, Some(395_000));
        assert_eq!(prepared_after, Some(222_700));
        assert_ne!(
            compact_terminal_subject(prepared_before, prepared_after, true, true).as_deref(),
            Some(subject.as_str()),
            "prepared-input padding is not Compact chrome when occupancy is known"
        );
    }

    #[test]
    fn the_compact_row_subject_names_before_and_after_sizes() {
        let subject = compact_terminal_subject(Some(36_617), Some(852), true, true).expect("sizes");
        assert!(
            subject.contains('→') || subject.contains("36.6k"),
            "{subject}"
        );
        assert!(
            subject.contains("852") || subject.contains("0.9k"),
            "{subject}"
        );
    }

    /// The `wait` row sat flat between
    /// children three and four of its cell. The host knows the cell's marker,
    /// so the continuation folds under it — without being counted as a child.
    /// Codex `exec` arguments are raw program
    /// source carried as one JSON string; a Plak PTC body carries the program
    /// under `source`. The Compose rail shows the program, never its encoding.
    #[test]
    fn a_compose_row_carries_its_program_source_as_detail() {
        assert_eq!(
            compose_program_source(Some(
                "// yield_time_ms=30000\n\nconst r = await tools.read_file({path: \"src/lib.rs\"});"
            ))
            .as_deref(),
            Some(
                "// yield_time_ms=30000\n\nconst r = await tools.read_file({path: \"src/lib.rs\"});"
            )
        );
        assert_eq!(
            compose_program_source(Some(
                r#""const spawned = await tools.spawn_agent({\n  class: \"review\"\n});""#
            ))
            .as_deref(),
            Some("const spawned = await tools.spawn_agent({\n  class: \"review\"\n});")
        );
        assert_eq!(
            compose_program_source(Some(
                r#"{"description":"add gcd and its test","source":"const x = 1;"}"#
            ))
            .as_deref(),
            Some("const x = 1;")
        );
        assert_eq!(
            compose_program_source(Some(r#"{"source":"\n# pragma\nprint(1)"}"#)).as_deref(),
            Some("# pragma\nprint(1)")
        );
        // Nothing to say stays nothing: no invented detail.
        assert_eq!(compose_program_source(None), None);
        assert_eq!(compose_program_source(Some("   \n\n")), None);

        // §5.8: the header is the host-counted tally alone.
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose("exec-1", "exec", Some("cell-1"));
        projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
        assert_eq!(
            compose_counts_label(projection.cells.get("exec-1").expect("compose"), false),
            "1 tools"
        );
        projection.open_compose("exec-bare", "exec", Some("cell-bare"));
        assert_eq!(
            compose_counts_label(projection.cells.get("exec-bare").expect("compose"), false),
            "0 tools"
        );
    }

    #[test]
    fn wait_continuation_folds_under_its_cell_marker_without_counting() {
        use plakous::agent::ToolCallerLineageView;

        let mut projection = TurnProjection::<TestExtension>::default();
        let wait = ToolCallerLineageView::local_composition_marker(Some("1"));
        assert_eq!(
            continuation_parent_call_id("wait", &wait, &projection),
            None,
            "a cell the host never opened stays a flat row"
        );
        projection.open_compose("exec-1", "exec", Some("1"));
        assert_eq!(
            continuation_parent_call_id("wait", &wait, &projection).as_deref(),
            Some("exec-1")
        );
        assert_eq!(
            continuation_parent_call_id("wait_js", &wait, &projection).as_deref(),
            Some("exec-1")
        );
        assert_eq!(
            continuation_parent_call_id("wait_py", &wait, &projection).as_deref(),
            Some("exec-1")
        );
        // Only a continuation folds: the marker itself and a plain tool do not.
        assert_eq!(
            continuation_parent_call_id("exec", &wait, &projection),
            None
        );
        assert_eq!(
            continuation_parent_call_id("read_file", &wait, &projection),
            None
        );
        let other_cell = ToolCallerLineageView::local_composition_marker(Some("2"));
        assert_eq!(
            continuation_parent_call_id("wait", &other_cell, &projection),
            None
        );
        assert_eq!(
            projection.cells["exec-1"].child_count(),
            0,
            "projecting a parent onto the row tallies nothing"
        );
    }

    /// A cell that yields before any nested call never named
    /// its cell on a start event, so the `wait*` continuation was a flat row on
    /// a broken rail and never re-terminalised its compose. The marker's own
    /// terminal lineage carries the retired cell.
    #[test]
    fn a_cell_that_yields_before_any_child_learns_its_cell_from_the_marker_terminal() {
        use plakous::agent::ToolCallerLineageView;

        let mut projection = TurnProjection::<TestExtension>::default();
        // The interpreter had not minted the cell when the marker started.
        projection.open_compose("exec-1", "exec", None);
        let wait = ToolCallerLineageView::local_composition_marker(Some("cell-9"));
        assert_eq!(
            continuation_parent_call_id("wait", &wait, &projection),
            None,
            "nothing is guessed before the host has observed the ownership"
        );

        projection.learn_marker_cell("exec-1", Some("cell-9"));
        assert_eq!(
            projection.cells["exec-1"].cell_id.as_deref(),
            Some("cell-9")
        );
        assert_eq!(
            continuation_parent_call_id("wait", &wait, &projection).as_deref(),
            Some("exec-1"),
            "the wait continuation folds under the cell's own row"
        );
        // A cell the host already knows is never overwritten by a later one.
        projection.learn_marker_cell("exec-1", Some("cell-other"));
        assert_eq!(
            projection.cells["exec-1"].cell_id.as_deref(),
            Some("cell-9")
        );
    }

    /// A cell yielded, a child dispatched during the wait put the marker's
    /// persisted row back to `running`, and nothing wrote it terminal again, so
    /// the `Completed` sweep rewrote a turn that succeeded to
    /// `failed:observation-gap` — live and on resume.
    /// A Compose head's line carries the cell's persisted start and, once the
    /// head is terminal, the finish its duration measures to: the stamps a
    /// group's elapsed span reads live, as the restored row has them.
    #[test]
    fn a_compose_update_carries_the_cell_stamps() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.upsert_tool_activity(sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1_000),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: Some("cell-1".into()),
            wait: None,
        });
        let (events, rx) = host_event_sink();
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose("exec-1", "exec", Some("cell-1"));
        projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
        let stamps = |rx: &crate::surface_protocol::HostEventReceiver<TestExtension>| match rx
            .try_iter()
            .last()
        {
            Some(HostEvent::ToolLine {
                started_at_ms,
                finished_at_ms,
                ..
            }) => (started_at_ms, finished_at_ms),
            other => panic!("a compose line expected, got {other:?}"),
        };
        emit_compose_update(
            &events,
            &session,
            &projection.cells["exec-1"],
            "exec-1",
            false,
            None,
        );
        assert_eq!(stamps(&rx), (Some(1_000), None), "in flight");
        projection.observe_child_terminal("exec-1", Some("child-1"), false);
        projection.observe_compose_terminal("exec-1", "done");
        emit_compose_update(
            &events,
            &session,
            &projection.cells["exec-1"],
            "exec-1",
            false,
            Some(4_500),
        );
        assert_eq!(stamps(&rx), (Some(1_000), Some(5_500)), "terminal");
    }

    #[test]
    fn a_reopened_compose_marker_re_terminalises_and_the_completed_sweep_skips_it() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let (events, rx) = host_event_sink();

        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose("exec-1", "exec", Some("cell-1"));
        observe_tool_activity(
            &mut session,
            sessions::ToolItem {
                call_id: "exec-1".into(),
                name: "exec".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("exec"),
                state: "running".into(),
                subject: None,
                detail: None,
                started_at_ms: Some(1),
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: Some("cell-1".into()),
                wait: None,
            },
        );
        projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
        projection.observe_child_terminal("exec-1", Some("child-1"), false);
        // The cell yields with every observed child terminal, so its own
        // terminal paints and persists.
        projection.observe_compose_terminal("exec-1", "done");
        assert_eq!(
            projection.cells["exec-1"].settled_persisted_state(),
            Some("succeeded")
        );
        observe_settled_compose_marker(
            &mut session,
            &projection.cells["exec-1"],
            false,
            "exec-1",
            2,
        );

        // A child dispatched during the `wait*` puts the cell back in flight.
        assert!(projection.observe_child_start("exec-1", Some("child-2"), Some("cell-1"), Some(2)));
        assert_eq!(projection.cells["exec-1"].row_state(), "running");
        assert_eq!(
            projection.cells["exec-1"].settled_persisted_state(),
            None,
            "a cell with a running child has no terminal to persist"
        );
        let mut marker = session.transcript.tool_activity("exec-1").unwrap();
        "running".clone_into(&mut marker.state);
        marker.finished_at_ms = None;
        observe_tool_activity(&mut session, marker);

        // The wait-phase child closes: that is the cell's end.
        projection.observe_child_terminal("exec-1", Some("child-2"), false);
        observe_settled_compose_marker(
            &mut session,
            &projection.cells["exec-1"],
            false,
            "exec-1",
            9,
        );
        let settled = session.transcript.tool_activity("exec-1").unwrap();
        assert_eq!(settled.state, "succeeded");
        assert_eq!(settled.finished_at_ms, Some(9));
        assert_eq!(settled.subject.as_deref(), Some("2 tools"));

        // The `Completed` sweep rewrites persisted `running` rows only, so a
        // re-terminalised marker is not touched and emits no line.
        observe_terminal_tool_activity(
            &mut session,
            &events,
            "failed:observation-gap",
            Some(&projection),
        );
        let swept = session.transcript.tool_activity("exec-1").unwrap();
        assert_eq!(swept.state, "succeeded", "a cell that succeeded stays so");
        assert!(
            !rx.try_iter()
                .any(|event| matches!(event, HostEvent::ToolLine { .. })),
            "the sweep has nothing to rewrite"
        );
    }

    /// A cell that yields while a child is still in flight has
    /// not ended, so its own terminal row stays `running` and a cancel in that
    /// window reaches it — instead of leaving `✓ Compose` over `⊘` children.
    #[test]
    fn a_cancel_in_the_yield_window_sweeps_the_marker_and_states_the_remainder() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let (events, rx) = host_event_sink();

        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose("exec-1", "exec", Some("cell-1"));
        projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
        projection.observe_child_terminal("exec-1", Some("child-1"), false);
        projection.observe_child_start("exec-1", Some("child-2"), Some("cell-1"), Some(2));
        // The marker terminalises while child-2 runs: `row_state` is what the
        // line and the persisted row carry, not the local `done`.
        projection.observe_compose_terminal("exec-1", "done");
        assert_eq!(projection.cells["exec-1"].row_state(), "running");
        observe_tool_activity(
            &mut session,
            sessions::ToolItem {
                call_id: "exec-1".into(),
                name: "exec".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("exec"),
                state: "running".into(),
                subject: Some("2 tools".into()),
                detail: None,
                started_at_ms: Some(1),
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: Some("cell-1".into()),
                wait: None,
            },
        );

        observe_cancelled_tool_activity(&mut session, &events, Some(&projection));
        let line = rx
            .try_iter()
            .find_map(|event| match event {
                HostEvent::ToolLine { call_id, state, .. }
                    if call_id.as_deref() == Some("exec-1") =>
                {
                    Some(state)
                }
                _ => None,
            })
            .expect("the cancelled marker paints its own row");
        assert_eq!(line, "cancelled after 1 of 2");
    }

    /// The exec terminal persisted `3 tools`, two more children completed
    /// under the `wait` continuation, the live row said `5 tools` and the
    /// resumed one still `3 tools`.
    #[test]
    fn compose_counts_moved_by_wait_children_survive_resume() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        observe_tool_activity(
            &mut session,
            sessions::ToolItem {
                call_id: "exec-1".into(),
                name: "exec".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("exec"),
                state: "succeeded".into(),
                subject: Some("3 tools".into()),
                detail: None,
                started_at_ms: Some(1),
                finished_at_ms: Some(2),
                parent_call_id: None,
                cell_id: Some("1".into()),
                wait: None,
            },
        );
        observe_compose_subject(&mut session, "exec-1", "5 tools".into());
        // A marker the host never persisted has nothing to move.
        observe_compose_subject(&mut session, "missing", "9 tools".into());

        let HostEvent::PresentationSnapshot(snapshot) =
            project_presentation_snapshot(&session.transcript)
        else {
            unreachable!("snapshot projection")
        };
        let tools: Vec<_> = snapshot
            .rows
            .iter()
            .filter_map(TranscriptRow::as_tool)
            .collect();
        let compose = tools
            .iter()
            .find(|item| item.call_id == "exec-1")
            .expect("the Compose head row is reloaded");
        assert_eq!(compose.subject.as_deref(), Some("5 tools"));
        assert_eq!(
            crate::activity::tool_activity_kind(&compose.name),
            plakous_host_protocol::views::presentation::ActivityKind::Compose
        );
        assert_eq!(compose.cell_id.as_deref(), Some("1"));
        assert_eq!(
            tools.len(),
            1,
            "re-persisting the tally rewrites the row rather than adding one"
        );
    }

    /// The exact host activity classification table.
    #[test]
    fn activity_kind_mapping_table_is_host_authoritative() {
        use crate::activity::tool_activity_kind;
        use plakous_host_protocol::views::presentation::ActivityKind;

        for marker in ["exec", "exec_js", "exec_py"] {
            assert_eq!(
                tool_activity_kind(marker),
                ActivityKind::Compose,
                "{marker}"
            );
        }
        // `wait*` yields belong to an existing cell; they never open a Compose.
        for yielded in ["wait", "wait_js", "wait_py"] {
            assert_eq!(tool_activity_kind(yielded), ActivityKind::Tool, "{yielded}");
        }
        for topology in [
            "spawn_agent",
            "send_agent_message",
            "wait_agent",
            "interrupt_agent",
            "close_agent",
            "recover_agent",
            "submit_result",
            "adopt_candidate",
            "discard_workspace",
            "stop_all",
        ] {
            assert_eq!(
                tool_activity_kind(topology),
                ActivityKind::Agent,
                "{topology}"
            );
        }
        for process in [
            "process_session_start",
            "process-session-start",
            "plakous.process-session-start.v2",
            "plakous.process-session-start.v3",
            "exec_command",
            "run_process",
        ] {
            assert_eq!(
                tool_activity_kind(process),
                ActivityKind::Process,
                "{process}"
            );
        }
        for tool in ["read_file", "apply_patch", "grep", ""] {
            assert_eq!(tool_activity_kind(tool), ActivityKind::Tool, "{tool}");
        }
        assert_eq!(tool_activity_kind("compact"), ActivityKind::Compact);
        // Process rows normalize onto one label, host-side.
        let row = crate::activity::normalize_process_start(
            plakous_host_protocol::views::presentation::ActivityItem {
                parent_call_id: None,
                cell_id: None,
                id: "a".into(),
                correlation_id: None,
                kind: ActivityKind::Tool,
                label: "exec_command".into(),
                subject: None,
                detail: None,
                state: plakous_host_protocol::views::presentation::ActivityState::Running,
                started_at_ms: None,
                finished_at_ms: None,
                duration_ms: None,
            },
        );
        assert_eq!(row.kind, ActivityKind::Process);
        assert_eq!(row.label, "Shell");
    }

    /// One nested cell's Compose row: created on the cell's own start, tallied on
    /// each child terminal, never re-parented.
    #[test]
    fn compose_row_opens_on_the_cell_and_counts_observed_children() {
        let parent = plakous::model::CallId::try_from(uuid::Uuid::from_u128(0x0104)).unwrap();
        let parent_id = parent.as_uuid().to_string();
        for marker in ["exec", "exec_js", "exec_py"] {
            let mut projection = TurnProjection::<TestExtension>::default();
            projection.open_compose(&parent_id, marker, Some("cell-1"));
            for ordinal in 1..=3_u32 {
                let child = plakous::model::CallId::try_from(uuid::Uuid::from_u128(
                    u128::from(ordinal) + 1,
                ))
                .unwrap();
                let lineage = nested_lineage("cell-1", ordinal, parent, child);
                projection.observe_child_start(
                    &parent_id,
                    Some(&child.as_uuid().to_string()),
                    lineage.cell_id(),
                    lineage.nested_ordinal(),
                );
            }
            for ordinal in 1..=3_u32 {
                let child = plakous::model::CallId::try_from(uuid::Uuid::from_u128(
                    u128::from(ordinal) + 1,
                ))
                .unwrap();
                projection.observe_child_terminal(
                    &parent_id,
                    Some(&child.as_uuid().to_string()),
                    ordinal == 2,
                );
            }
            let compose = projection.cells.get(&parent_id).unwrap();
            assert_eq!(compose.name, marker);
            assert_eq!(compose.cell_id.as_deref(), Some("cell-1"));
            assert_eq!(compose_counts_label(compose, false), "3 tools · 1 failed");
            assert_eq!(
                compose_counts_label(compose, true),
                "3 tools · 1 failed · counts partial after gap"
            );
        }
    }

    /// A cell that cancels states only the remainder it observed.
    #[test]
    fn cancelled_compose_states_only_the_observed_remainder() {
        let parent = "outer-call";
        let mut projection = TurnProjection::<TestExtension>::default();

        // Nothing observed: no ratio may be invented.
        projection.open_compose(parent, "exec_js", Some("cell-1"));
        assert_eq!(
            compose_cancelled_state(projection.cells.get(parent).unwrap()),
            "cancelled"
        );

        // Four accepted, two reached a terminal state.
        for ordinal in 1..=4_u32 {
            projection.observe_child_start(
                parent,
                Some(&format!("child-{ordinal}")),
                Some("cell-1"),
                Some(ordinal),
            );
        }
        for ordinal in 1..=2_u32 {
            projection.observe_child_terminal(parent, Some(&format!("child-{ordinal}")), false);
        }
        assert_eq!(
            compose_cancelled_state(projection.cells.get(parent).unwrap()),
            "cancelled after 2 of 4"
        );
    }

    /// A child whose parent the host never observed is never folded or guessed.
    #[test]
    fn an_unknown_parent_never_mints_a_compose() {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_child_start("never-seen", Some("child-1"), Some("cell-9"), Some(1));
        assert!(
            projection
                .observe_child_terminal("never-seen", Some("child-1"), false)
                .is_none()
        );
        assert!(projection.cells.is_empty());
    }

    /// A yielded Compose keeps `running` while any observed child is not
    /// terminal, and paints its terminal only when they all are.
    ///
    /// The outer `exec*` completes when the cell *yields*, and children
    /// dispatched during the later `wait*` still carry the exec's
    /// `parent_call_id`. Spec 02 §5.4 puts the running glyph on the compose and on
    /// the child that is running, and §5.1 forbids inventing state: a `✓ Compose`
    /// over `⊘ cancelled` children is exactly the lie those two clauses name.
    #[test]
    fn a_yielded_compose_stays_running_until_every_observed_child_is_terminal() {
        let marker = "outer-exec";
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose(marker, "exec_js", Some("cell-7"));
        assert!(!projection.observe_child_start(marker, Some("child-1"), Some("cell-7"), Some(1)));
        assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

        // The cell yields with `child-1` still in flight: the marker has its own
        // observed terminal, but the row must not claim the cell is finished.
        projection.observe_compose_terminal(marker, "done");
        assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

        // `child-1` reaches its terminal: every observed child is terminal, so
        // the marker's own observed state is what the row now paints.
        let compose = projection
            .observe_child_terminal(marker, Some("child-1"), false)
            .unwrap();
        assert_eq!(compose.row_state(), "done");
        assert_eq!(compose_counts_label(compose, false), "1 tools");

        // A second child dispatched during the `wait*` puts the cell back in
        // flight, and `observe_child_start` says so, so the caller re-paints the
        // marker row and persists it back to `running`.
        assert!(projection.observe_child_start(marker, Some("child-2"), Some("cell-7"), Some(2)));
        assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

        // A `wait*` continuation names the cell and no parent; the host
        // attributes it to the row that cell already opened.
        assert_eq!(
            projection.marker_for_cell(Some("cell-7")).as_deref(),
            Some(marker)
        );
        assert!(projection.marker_for_cell(Some("cell-unknown")).is_none());
        assert!(projection.marker_for_cell(None).is_none());

        // The `wait*` terminal alone does not close a row with a live child.
        projection.observe_compose_terminal(marker, "failed");
        assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");
        let compose = projection
            .observe_child_terminal(marker, Some("child-2"), true)
            .unwrap();
        assert_eq!(compose.row_state(), "failed");
        assert_eq!(compose_counts_label(compose, false), "2 tools · 1 failed");
    }

    /// Reasoning projects only under an admitting bound.
    #[test]
    fn reasoning_projects_only_under_an_admitting_visibility_bound() {
        use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};

        let location = OutputLocation::new(0, 0);
        for visibility in [ReasoningVisibility::Visible, ReasoningVisibility::Summary] {
            let mut projection = TurnProjection::<TestExtension>::default();
            projection.observe_block_start(&reasoning_block(location, visibility));
            assert_eq!(
                projection
                    .observe_reasoning_delta(&ReasoningDelta::new(location, b"thinking".to_vec())),
                Some("thinking".to_owned()),
                "{visibility:?}"
            );
            let sealed = projection
                .seal_reasoning(location)
                .expect("admitted reasoning seals");
            assert_eq!(sealed.location_key, "0:0", "{visibility:?}");
            assert_eq!(sealed.text, "thinking", "{visibility:?}");
        }
        for visibility in [
            ReasoningVisibility::Omitted,
            ReasoningVisibility::Redacted,
            ReasoningVisibility::OpaqueReplay,
        ] {
            let mut projection = TurnProjection::<TestExtension>::default();
            projection.observe_block_start(&reasoning_block(location, visibility));
            assert_eq!(
                projection
                    .observe_reasoning_delta(&ReasoningDelta::new(location, b"secret".to_vec())),
                None,
                "{visibility:?}"
            );
            assert_eq!(projection.seal_reasoning(location), None, "{visibility:?}");
            assert!(projection.seal_all_reasoning().is_empty(), "{visibility:?}");
        }
    }

    /// A replay that discards its response discards the reasoning that
    /// response sealed: the session keeps only the replay's thinking.
    #[test]
    fn a_discarded_response_never_persists_its_sealed_thinking() {
        use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};

        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let (events, _rx) = host_event_sink();
        let location = OutputLocation::new(0, 0);
        let mut projection = TurnProjection::<TestExtension>::default();
        for text in ["first try", "replayed"] {
            projection
                .observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
            let _ = projection
                .observe_reasoning_delta(&ReasoningDelta::new(location, text.as_bytes().to_vec()));
            seal_reasoning_block(&mut projection, &events, location);
            if text == "first try" {
                discard_response_reasoning(&mut projection, &events);
            }
        }
        settle_live_thinking(&mut projection, &mut session, &events);
        let thinking = session
            .transcript
            .rows()
            .into_iter()
            .filter_map(|(line, _)| line.as_thinking().cloned())
            .map(|activity| activity.text)
            .collect::<Vec<_>>();
        assert_eq!(thinking, ["replayed"]);
    }

    #[test]
    fn output_limit_discard_then_interrupted_retry_does_not_save_old_thinking() {
        use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let (events, _rx) = host_event_sink();
        let location = OutputLocation::new(0, 0);
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"discarded thought"));
        seal_reasoning_block(&mut projection, &events, location);
        discard_response_reasoning(&mut projection, &events);
        // The retry is interrupted before it emits anything. The ordinary
        // terminal flush must not resurrect the discarded response.
        settle_live_thinking(&mut projection, &mut session, &events);
        assert!(
            session
                .transcript
                .rows()
                .into_iter()
                .all(|(line, _)| line.as_thinking().is_none())
        );
    }

    /// A pending location's buffer is released only by an admitting bound.
    #[test]
    fn pending_reasoning_buffer_is_released_or_dropped_but_never_leaked() {
        use plakous::model::{
            OutputLocation, ReasoningDelta, ReasoningVisibility, ReasoningVisibilityBound,
        };

        let location = OutputLocation::new(1, 2);

        // Admitting bound releases exactly the buffered bytes, once.
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&pending_reasoning_block(location));
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec())),
            None
        );
        assert_eq!(
            projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
                location,
                ReasoningVisibility::Summary
            )),
            Some("held".to_owned())
        );
        let sealed = projection
            .seal_reasoning(location)
            .expect("released pending reasoning seals");
        assert_eq!(sealed.location_key, "1:2");
        assert_eq!(sealed.text, "held");

        // Non-admitting bound drops the buffer without emitting it.
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&pending_reasoning_block(location));
        let _ =
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec()));
        assert_eq!(
            projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
                location,
                ReasoningVisibility::Redacted
            )),
            None
        );
        assert_eq!(projection.seal_reasoning(location), None);

        // Overflow drops the buffer and the location stays non-displayable even
        // under a later admitting bound: a partial body is never emitted.
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&pending_reasoning_block(location));
        let oversized = vec![b'x'; MAX_PENDING_REASONING_BYTES + 1];
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, oversized)),
            None
        );
        assert_eq!(
            projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
                location,
                ReasoningVisibility::Visible
            )),
            None
        );
        // …and every delta after that bound stays suppressed. Re-admitting the
        // location would emit the tail of a body whose head was dropped, which
        // The transcript contract forbids for the remainder of the block.
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"tail".to_vec())),
            None
        );
        assert_eq!(projection.seal_reasoning(location), None);

        // A bound arriving after an already-emitting admitted block does not
        // reset it: the block still seals.
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"shown".to_vec())),
            Some("shown".to_owned())
        );
        assert_eq!(
            projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
                location,
                ReasoningVisibility::Visible
            )),
            None
        );
        let sealed = projection
            .seal_reasoning(location)
            .expect("already-emitting admitted block still seals");
        assert_eq!(sealed.location_key, "1:2");
        assert_eq!(sealed.text, "shown");

        // An unbound pending location at response completion emits nothing.
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&pending_reasoning_block(location));
        let _ =
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec()));
        assert_eq!(projection.seal_all_reasoning(), [] as [SealedReasoning; 0]);
        assert!(projection.reasoning.is_empty());

        // A delta for a location that never started is non-admitting.
        let mut projection = TurnProjection::<TestExtension>::default();
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"stray".to_vec())),
            None
        );
    }

    /// A live usage report is a snapshot, never a turn total.
    #[test]
    fn live_usage_projects_as_a_non_terminal_snapshot() {
        let update = plakous::model::UsageUpdate::new(plakous::model::Usage::new(11, 3, 2, 7, 0));
        assert_eq!(
            project_live_usage(&update),
            HostEvent::TurnUsage {
                input_tokens: 11,
                output_tokens: 3,
                reasoning_tokens: 2,
                cached_input_tokens: 7,
                cache_write_tokens: 0,
                // Coverage is unknown: a live report is one provider-reported
                // cumulative snapshot over an unknown number of operations, and
                // the host never fabricates a `1/1`.
                model_operations: None,
                reporting_model_operations: None,
                peak_prepared_input_tokens: None,
                terminal: false,
            }
        );
    }

    /// Spec 02 §5.4: a published notice in a persisted scope joins the
    /// session's transcript in publication order, so a restore shows it as it
    /// was shown live; settings, approval and clipped feedback stay live only,
    /// a diagnostic is never persisted, a child's live rows never enter the
    /// root transcript, a row its producer already stored is not kept twice,
    /// a re-announced state note is never kept, and a background publisher's
    /// notice for a session the host no longer owns is dropped.
    #[test]
    fn live_tool_observations_never_commit_product_records_or_views() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::session_store::MemoryStore::new();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        store.save(&mut session).unwrap();
        let revision = session.store_revision;
        observe_tool_activity(
            &mut session,
            sessions::ToolItem {
                call_id: "live-call".into(),
                name: "read_file".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("read_file"),
                state: "running".into(),
                subject: None,
                detail: None,
                started_at_ms: Some(1),
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: None,
                wait: None,
            },
        );
        assert!(session.transcript.tool_activity("live-call").is_some());
        assert_eq!(session.store_revision, revision);
        assert_eq!(store.load(&session.id).unwrap().store_revision, revision);
        assert!(store.load_views(&session.id).unwrap().is_none());
        assert_eq!(
            store.load_intents(&session.id).unwrap().notices,
            [] as [sessions::projector::OperatorNotice; 0]
        );
    }

    #[test]
    fn notice_intents_commit_by_scope_and_session_not_by_observation() {
        use crate::sessions::NoticeScope;
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        let store = crate::session_store::MemoryStore::new();
        store.save(&mut session).unwrap();
        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let scopes = [
            NoticeScope::Retry,
            NoticeScope::Clipped,
            NoticeScope::Turn,
            NoticeScope::Queue,
            NoticeScope::Recovery,
            NoticeScope::Tree,
            NoticeScope::Compaction,
            NoticeScope::Session,
            NoticeScope::Settings,
            NoticeScope::Approval,
            NoticeScope::Host,
            NoticeScope::Diagnostic,
        ];
        for scope in scopes {
            events.notice(TranscriptRow::warning(scope, format!("{scope:?}")));
        }
        events.send(HostEvent::ChildLive {
            agent_id: "c1".into(),
            turn_id: "t1".into(),
            op: Box::new(plakous_host_protocol::SessionObservation::Row(
                TranscriptRow::info(NoticeScope::Tree, "a child's row"),
            )),
        });
        events.send(HostEvent::Row(TranscriptRow::info(
            NoticeScope::Turn,
            "stored by its producer",
        )));
        events.send(HostEvent::Row(TranscriptRow::warning(
            NoticeScope::Queue,
            "re-announced on every open",
        )));
        events
            .pinned_to("a session the host has left")
            .notice(TranscriptRow::info(
                NoticeScope::Tree,
                "a stale background pass",
            ));
        let current = session.id.clone();
        events.pinned_to(&current).notice(TranscriptRow::info(
            NoticeScope::Tree,
            "this session's background pass",
        ));
        commit_notice_intents(
            &events,
            &store,
            &SessionHome::new(root.path().join("sessions")),
            None,
            &mut session,
        );
        let mut expected: Vec<_> = scopes
            .into_iter()
            .filter(|scope| scope.persisted())
            .map(|scope| TranscriptRow::warning(scope, format!("{scope:?}")))
            .collect();
        expected.push(TranscriptRow::info(
            NoticeScope::Tree,
            "this session's background pass",
        ));
        assert_eq!(store.load(&session.id).unwrap().transcript, expected);
        assert!(
            events.take_notice_intents().is_empty(),
            "each notice persists once"
        );
    }

    #[test]
    fn row_observations_never_author_notice_intents() {
        let (events, receiver) = host_event_sink();
        let row = TranscriptRow::warning(NoticeScope::Turn, "live observation only");
        events.send(HostEvent::Row(row.clone()));
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            [HostEvent::Row(row)]
        );
        assert!(events.take_notice_intents().is_empty());
    }

    /// A definite provider failure during compaction is diagnosable from the log.
    ///
    /// Five `compact.started`/`compact.finished`
    /// pairs, three committed and two failed, and the failed records carried
    /// only `kind`, `outcome`, `elapsed_ms`, `seq` and `ts`. Nothing said what
    /// had failed, so nothing could be acted on.
    #[test]
    fn a_failed_compaction_records_and_reports_why_it_failed() {
        let dir = tempfile::tempdir().unwrap();
        let home = SessionHome::new(dir.path().join("sessions"));
        home.ensure().unwrap();
        let id = "9f9f9f9f-1111-2222-3333-444444444444";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(compact_finished_record(
            CompactionEventOutcome::Failed,
            4_210,
            None,
            Some("provider rejected the compaction request: context_capacity"),
        ))
        .unwrap();
        log.flush().unwrap();
        let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
        let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["kind"], "compact.finished");
        assert_eq!(record["outcome"], "failed");
        assert_eq!(record["elapsed_ms"], 4_210);
        assert_eq!(
            record["detail"], "provider rejected the compaction request: context_capacity",
            "a failed compaction must say what failed"
        );

        let note = compact_finished_note(
            CompactionEventOutcome::Failed,
            None,
            Some("provider rejected the compaction request: context_capacity"),
        );
        assert!(
            note.contains("context_capacity"),
            "the operator note must carry the failure text too: {note}"
        );

        // An outcome with nothing to explain gains no empty field.
        log.append(compact_finished_record(
            CompactionEventOutcome::Committed,
            12,
            None,
            None,
        ))
        .unwrap();
        log.flush().unwrap();
        let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
        assert!(
            !lines[1].contains("\"detail\""),
            "a committed compaction must not carry an empty detail: {}",
            lines[1]
        );
    }

    /// A truncated tool result is visible without reading the model's prose.
    ///
    /// Until this existed the observation budget clipped a result silently: a
    /// surface saw the tool succeed and had no way to know its output had been
    /// cut, so the only trace was whatever the model said about it.
    #[test]
    fn a_clipped_observation_records_and_reports_what_was_cut() {
        let dir = tempfile::tempdir().unwrap();
        let home = SessionHome::new(dir.path().join("sessions"));
        home.ensure().unwrap();
        let id = "5c5c5c5c-1111-2222-3333-444444444444";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(
            SessionEvent::new("observation.clipped")
                .str("name", "read")
                .opt_str("call_id", Some("call-9"))
                .u64("bytes_kept", 4_096)
                .u64("bytes_dropped", 118_784)
                .u64("bound", 4_096),
        )
        .unwrap();
        log.flush().unwrap();
        let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
        let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["kind"], "observation.clipped");
        assert_eq!(record["name"], "read");
        assert_eq!(record["call_id"], "call-9");
        assert_eq!(record["bytes_kept"], 4_096);
        assert_eq!(record["bytes_dropped"], 118_784);
        assert_eq!(record["bound"], 4_096);

        let note = observation_clipped_note("read", 4_096, 118_784, 4_096);
        for figure in ["read", "4096", "118784"] {
            assert!(
                note.contains(figure),
                "the note must carry {figure}: {note}"
            );
        }
        assert!(
            note.contains("narrow the call"),
            "the note must say what to do about it: {note}"
        );
    }

    #[test]
    fn a_replaced_observation_records_its_distinct_canonical_counts() {
        let dir = tempfile::tempdir().unwrap();
        let home = SessionHome::new(dir.path().join("sessions"));
        home.ensure().unwrap();
        let id = "5c5c5c5c-1111-2222-3333-444444444445";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(observation_replaced_record(Some("call-10"), 4_157, 161))
            .unwrap();
        log.flush().unwrap();
        let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
        let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["kind"], "observation.replaced");
        assert_eq!(record["call_id"], "call-10");
        assert_eq!(record["bytes_before"], 4_157);
        assert_eq!(record["bytes_after"], 161);
        assert!(record.get("bytes_dropped").is_none());
    }

    /// A committed compaction says what it did, not merely that it committed.
    ///
    /// "compact workflow committed" tells an operator nothing about whether the
    /// compaction bought anything, and the fold's note said only that for as
    /// long as the fold has existed. The figures come from the successor
    /// checkpoint's own compaction provenance, so the line and the record agree
    /// with what the checkpoint commits to.
    #[test]
    fn a_committed_compaction_records_and_reports_what_it_did() {
        let dir = tempfile::tempdir().unwrap();
        let home = SessionHome::new(dir.path().join("sessions"));
        home.ensure().unwrap();
        let id = "7a7a7a7a-1111-2222-3333-444444444444";
        let log = SessionLog::open(&home, id).unwrap();
        let summary = CompactionSummary {
            rung: CompactionRung::SemanticFold,
            segments: 14,
            input_tokens_before: 182_400,
            input_tokens_after: 61_200,
            occupancy_tokens_before: None,
            occupancy_tokens_after: None,
        };
        log.append(compact_finished_record(
            CompactionEventOutcome::Committed,
            3_120,
            Some(summary),
            None,
        ))
        .unwrap();
        log.flush().unwrap();
        let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
        let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["kind"], "compact.finished");
        assert_eq!(record["outcome"], "committed");
        assert_eq!(record["rung"], "semantic_fold");
        assert_eq!(record["segments"], 14);
        assert_eq!(record["input_tokens_before"], 182_400);
        assert_eq!(record["input_tokens_after"], 61_200);

        let note = compact_finished_note(CompactionEventOutcome::Committed, Some(summary), None);
        for figure in ["folded", "14", "182400", "61200"] {
            assert!(
                note.contains(figure),
                "the note must carry {figure}: {note}"
            );
        }
        assert!(
            !note.contains("compact workflow committed"),
            "the figures replace the bare wording rather than trailing it: {note}"
        );

        // Each rung is named distinctly, so a reader can tell which one ran.
        for (rung, label, verb) in [
            (CompactionRung::NativeFacet, "native_facet", "natively"),
            (CompactionRung::SemanticFold, "semantic_fold", "folded"),
            (
                CompactionRung::ToolResultPlaceholders,
                "tool_result_placeholders",
                "replaced",
            ),
        ] {
            assert_eq!(compaction_rung_label(rung), label);
            assert!(rung.verb().contains(verb), "{rung:?}");
        }
    }

    /// On a live Grok hub the claimed call carries `exposed_name` =
    /// `get_command_or_subagent_output` (or `kill_command_or_subagent`). A
    /// call naming only a child must read as the await/interrupt over that
    /// child, not as a process wait/stop over an `(unknown)` process id. The
    /// restored transcript (exposed name absent) already reads it right; the
    /// live row must match.
    #[test]
    fn live_grok_child_calls_do_not_read_as_process_calls() {
        use plakous::model::{Digest, DigestDomain};
        let dir = tempfile::tempdir().unwrap();
        let roots = crate::agent::ProductStateRoots::with_doubles(
            dir.path().to_path_buf(),
            dir.path().join("state"),
        );
        let rows = test_kit::MemoryRows::new();
        let store = crate::session_store::MemoryStore::new();
        let home = SessionHome::new(dir.path().join("sessions"));
        let mut session = SessionRecord::new(dir.path().to_path_buf());
        let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
        session.plakous_locator = Some(locator);
        let (events, _rx) = host_event_sink();
        let mut projection = TurnProjection::default();
        let mut stash = ToolSubjectStash::new();
        let mut assistant = String::new();
        let mut delta = String::new();
        let mut last_flush = Instant::now();
        let mut tree = TreeSnapshotPush::default();
        let mut compacted = false;
        let mut peak = 0;
        let uuid = uuid::Uuid::from_u128(1);
        let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
        {
            let mut dispatch =
                |name: &str, call: u128, kind: &str, observation: serde_json::Value| {
                    let identity = serde_json::json!({
                        "canonical_name": name,
                        "exposed_name": name,
                        "exposure_map_digest": null,
                        "registered_tool_id": format!("plak.topology.{name}.v1"),
                        "executor_id": format!("plak.topology.{name}.executor.v1"),
                        "definition_digest": digest,
                        "schema_digest": digest,
                        "effect_metadata_digest": digest,
                        "argument_limits_digest": digest,
                    });
                    let event: AgentEvent = serde_json::from_value(serde_json::json!({
                        "branch_sequence": 1,
                        "event_id": uuid,
                        "session_id": locator.session_id(),
                        "branch_id": locator.branch_id(),
                        "agent_id": uuid,
                        "run_id": uuid,
                        "turn_id": null,
                        "attempt_id": null,
                        "call_id": uuid::Uuid::from_u128(call),
                        "timestamp": {"seconds": 1, "nanoseconds": 0},
                        "causal_parent": null,
                        "origin": "Tool",
                        "classification": "Public",
                        "event_schema": 3,
                        "projection_revision": 6,
                        "visibility": "Content",
                        "scope": "Agent",
                        "event_kind_version": 11,
                        "kind": {kind: {
                            "identity": identity,
                            "lineage": plakous::agent::ToolCallerLineageView::direct(),
                            "observation": observation,
                        }},
                        "retained_bytes": 512,
                        "metadata_retained_bytes": 512,
                        "content_retained_bytes": 0,
                    }))
                    .unwrap();
                    apply_event(
                        &event,
                        &roots,
                        &rows,
                        &store,
                        &home,
                        &mut session,
                        &events,
                        &mut assistant,
                        &mut delta,
                        &mut last_flush,
                        &mut stash,
                        &mut projection,
                        None,
                        "test",
                        &mut tree,
                        &mut compacted,
                        &mut peak,
                    );
                };
            let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
            dispatch(
                "get_command_or_subagent_output",
                21,
                "ToolStarted",
                started(serde_json::json!({"task_ids": ["c1"], "timeout_ms": 5000})),
            );
            dispatch(
                "kill_command_or_subagent",
                22,
                "ToolStarted",
                started(serde_json::json!({"task_id": "c1"})),
            );
        }
        for (call, slot) in [(21, "wait_agent"), (22, "interrupt_agent")] {
            let id = uuid::Uuid::from_u128(call).to_string();
            let row = session.transcript.tool_activity(&id).unwrap();
            assert_eq!(row.name, slot);
            let subject = row.subject.clone().unwrap_or_default();
            assert!(
                subject.contains("c1") && !subject.contains("process"),
                "{slot} row on a live Grok hub reads as a process call: {subject:?}"
            );
        }
        assert!(
            !projection
                .process_waits
                .contains_key(&uuid::Uuid::from_u128(21).to_string()),
            "a child await is recorded as a process wait"
        );
    }

    /// A bare pi `wait` on a live hub starts as the process wait (only its
    /// run decides) and, once it settles with the tree's await receipt,
    /// reads as the await: `wait_agent`, the children it listed, what woke it
    /// and its 300000 ms ceiling, persisted as the restored row reads it. One
    /// that ran the process tool's bare await stays the process wait.
    #[test]
    fn live_bare_pi_wait_that_ran_the_await_reads_as_the_await() {
        use plakous::model::{Digest, DigestDomain};
        let dir = tempfile::tempdir().unwrap();
        let roots = crate::agent::ProductStateRoots::with_doubles(
            dir.path().to_path_buf(),
            dir.path().join("state"),
        );
        let rows = test_kit::MemoryRows::new();
        let store = crate::session_store::MemoryStore::new();
        let home = SessionHome::new(dir.path().join("sessions"));
        let mut session = SessionRecord::new(dir.path().to_path_buf());
        let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
        session.plakous_locator = Some(locator);
        let (events, _rx) = host_event_sink();
        let mut projection = TurnProjection::default();
        let mut stash = ToolSubjectStash::new();
        let mut assistant = String::new();
        let mut delta = String::new();
        let mut last_flush = Instant::now();
        let mut tree = TreeSnapshotPush::default();
        let mut compacted = false;
        let mut peak = 0;
        let uuid = uuid::Uuid::from_u128(1);
        let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
        {
            let mut dispatch =
                |name: &str, call: u128, kind: &str, observation: serde_json::Value| {
                    let identity = serde_json::json!({
                        "canonical_name": name,
                        "exposed_name": name,
                        "exposure_map_digest": null,
                        "registered_tool_id": format!("plak.topology.{name}.v1"),
                        "executor_id": format!("plak.topology.{name}.executor.v1"),
                        "definition_digest": digest,
                        "schema_digest": digest,
                        "effect_metadata_digest": digest,
                        "argument_limits_digest": digest,
                    });
                    let event: AgentEvent = serde_json::from_value(serde_json::json!({
                        "branch_sequence": 1,
                        "event_id": uuid,
                        "session_id": locator.session_id(),
                        "branch_id": locator.branch_id(),
                        "agent_id": uuid,
                        "run_id": uuid,
                        "turn_id": null,
                        "attempt_id": null,
                        "call_id": uuid::Uuid::from_u128(call),
                        "timestamp": {"seconds": 1, "nanoseconds": 0},
                        "causal_parent": null,
                        "origin": "Tool",
                        "classification": "Public",
                        "event_schema": 3,
                        "projection_revision": 6,
                        "visibility": "Content",
                        "scope": "Agent",
                        "event_kind_version": 11,
                        "kind": {kind: {
                            "identity": identity,
                            "lineage": plakous::agent::ToolCallerLineageView::direct(),
                            "observation": observation,
                        }},
                        "retained_bytes": 512,
                        "metadata_retained_bytes": 512,
                        "content_retained_bytes": 0,
                    }))
                    .unwrap();
                    apply_event(
                        &event,
                        &roots,
                        &rows,
                        &store,
                        &home,
                        &mut session,
                        &events,
                        &mut assistant,
                        &mut delta,
                        &mut last_flush,
                        &mut stash,
                        &mut projection,
                        None,
                        "test",
                        &mut tree,
                        &mut compacted,
                        &mut peak,
                    );
                };
            let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
            let completed = |result: serde_json::Value| serde_json::json!({"succeeded":true,"preview":result.to_string()});
            let receipt = serde_json::json!({
                "mail": [{"message_id": "m1", "from": "c1", "purpose": "status", "body": "halfway"}],
                "woke_by": "mail",
                "agents": [{"agent_id": "c1", "status": "running"}],
                "timed_out": false,
            });
            for (call, result) in [
                (31, serde_json::json!({"result": receipt.to_string()})),
                (
                    32,
                    serde_json::json!({"tasks": [{"task_id": "p1", "status": "exited", "exit_code": 0}]}),
                ),
            ] {
                dispatch("wait", call, "ToolStarted", started(serde_json::json!({})));
                dispatch("wait", call, "ToolCompleted", completed(result));
            }
        }
        let row = session
            .transcript
            .tool_activity(&uuid::Uuid::from_u128(31).to_string())
            .unwrap();
        assert_eq!(row.name, "wait_agent", "{row:?}");
        assert_eq!(row.kind, crate::activity::tool_activity_kind("wait_agent"));
        assert_eq!(row.subject.as_deref(), Some("c1"), "{row:?}");
        let wait = row.wait.clone().unwrap();
        assert_eq!(wait.woke_by.as_deref(), Some("mail from c1"), "{row:?}");
        assert_eq!(wait.timeout_ms, Some(300_000), "{row:?}");
        assert!(
            !projection
                .process_waits
                .contains_key(&uuid::Uuid::from_u128(31).to_string())
        );
        let process = session
            .transcript
            .tool_activity(&uuid::Uuid::from_u128(32).to_string())
            .unwrap();
        assert_eq!(process.name, "wait", "{process:?}");
    }
}
