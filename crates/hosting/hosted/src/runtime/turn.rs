//! Turn outcome publication, cancel settlement, and approvals.

use crate::session_home::SessionHome;
use crate::session_log::{SessionEvent, SessionLog};
use crate::session_store::SessionRecordStore;
use crate::surface_protocol::ContentEvent;
use std::time::{Duration, Instant};

use plakous::agent::{AgentError, TurnControl, TurnResult, TurnResultOutcome};
use plakous_host_protocol::{TurnLifecycle, TurnPhase};

use crate::interrupt::CancelSettlement;
use crate::sessions::{QueueDrainState as SessionQueueDrainState, SessionRecord};

use super::admission::{commit_session_update, emit_queue_snapshot};
use super::logging::log_event;

/// The row a completed turn persists: its assistant prose, or a turn notice
/// when the turn published no visible text.
pub(super) fn completed_turn_row(assistant: &str) -> TranscriptRow {
    if assistant.is_empty() {
        TranscriptRow::info(NoticeScope::Turn, "turn completed with no visible text")
    } else {
        TranscriptRow::assistant(assistant)
    }
}
use super::rebind::durable_tree_id;
use super::{
    DriveTurnError, DrivenTurn, HOST_CANCEL_SETTLE_TIMEOUT, PendingApproval, Sender, TopologyRows,
    TurnHostOutcome, TurnProjection, topology_state,
};
use crate::sessions::{NoticeScope, TranscriptRow};

impl DrivenTurn {
    pub(super) const fn continuing(outcome: TurnHostOutcome) -> Self {
        Self {
            outcome,
            quit_after_terminal: false,
        }
    }
}

impl From<String> for DriveTurnError {
    fn from(message: String) -> Self {
        Self {
            message,
            quit_after_terminal: false,
            store_refusal: None,
        }
    }
}

impl From<crate::session_store::RecordStoreError> for DriveTurnError {
    fn from(error: crate::session_store::RecordStoreError) -> Self {
        Self {
            store_refusal: error.refusal(),
            message: error.to_string(),
            quit_after_terminal: false,
        }
    }
}

impl DriveTurnError {
    pub(super) fn storage_failure(&self) -> Option<crate::error::CliFailure> {
        (self.store_refusal == Some(crate::error::StoreRefusal::StorageFull)).then(|| {
            crate::session_store::RecordStoreError::refused(
                crate::error::StoreRefusal::StorageFull,
                self.message.clone(),
            )
            .into()
        })
    }
}

impl From<&str> for DriveTurnError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

pub(super) const fn host_outcome_blocks_continuation(
    outcome: &Result<TurnHostOutcome, String>,
) -> bool {
    matches!(
        outcome,
        Ok(TurnHostOutcome::RecoveryRequired { .. }) | Err(_)
    )
}

fn safe_finish_detail(detail: &str) -> String {
    let printable: String = detail
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    plakous::model::SafeDiagnostic::new(printable).to_string()
}

pub(crate) fn truncation_note(bound: Option<std::num::NonZeroU32>) -> String {
    let limit = bound.map_or_else(
        || "output limit".to_owned(),
        |bound| format!("{bound}-token output limit"),
    );
    format!("answer truncated at its {limit}; ask to continue")
}

pub(super) fn terminal_turn_outcome(result: &TurnResult) -> TurnHostOutcome {
    match result.outcome() {
        TurnResultOutcome::Completed(response) => TurnHostOutcome::Completed {
            assistant: super::tools::joined_assistant_text(response),
            finish_reason: response.finish().reason(),
            finish_detail: response.finish().detail().map(safe_finish_detail),
            wire_output_limit: result.wire_output_limit(),
        },
        TurnResultOutcome::Cancelled(_) => TurnHostOutcome::Interrupted {
            assistant: String::new(),
            exchange: crate::agent::TerminalExchange::Closed,
        },
        TurnResultOutcome::Failed(error) => {
            let rendered = operator_facing_agent_error("agent turn failed", error);
            if let Some(remedy) = result.context_overflow_remedy() {
                TurnHostOutcome::ContextCapacity {
                    source_checkpoint: result.checkpoint_id(),
                    recovery: match remedy {
                        plakous::agent::ContextOverflowRemedy::ResendShorter => {
                            plakous_host_protocol::ContextCapacityRecovery::ResendShorter
                        }
                        plakous::agent::ContextOverflowRemedy::Compact => {
                            plakous_host_protocol::ContextCapacityRecovery::Compactable
                        }
                        _ => plakous_host_protocol::ContextCapacityRecovery::Protected,
                    },
                    error: rendered,
                }
            } else {
                TurnHostOutcome::Failed {
                    error: rendered,
                    transient: crate::driver::transient::failure_gate(result).map(|gate| {
                        super::TransientFailure {
                            failed_turn: result.turn_id(),
                            gate,
                        }
                    }),
                    exchange: crate::agent::TerminalExchange::of(result),
                }
            }
        }
        TurnResultOutcome::RecoveryRequired(error) => TurnHostOutcome::RecoveryRequired {
            error: operator_facing_agent_error("agent turn requires recovery", error),
        },
    }
}

/// Projects the canonical terminal into the observational lifecycle marker.
///
/// `phase` follows `TurnResultOutcome` exactly: it is never inferred from
/// busy or usage observations.
pub(super) fn turn_lifecycle(result: &TurnResult) -> TurnLifecycle {
    let phase = match result.outcome() {
        TurnResultOutcome::Completed(response) => {
            if response.finish().reason() == plakous::model::FinishReason::Refusal {
                TurnPhase::Refused
            } else {
                TurnPhase::Completed
            }
        }
        TurnResultOutcome::Cancelled(_) => TurnPhase::Cancelled,
        TurnResultOutcome::Failed(_) => TurnPhase::Failed,
        TurnResultOutcome::RecoveryRequired(_) => TurnPhase::RecoveryRequired,
    };
    let (model_error_kind, http_status) = lifecycle_error_parts(result.error());
    TurnLifecycle {
        turn_id: result.turn_id().to_string(),
        phase,
        model_error_kind,
        http_status,
    }
}

/// The observable error context a lifecycle marker may carry.
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "ModelHttpStatus is outside this package's approved facade symbol set, so the method path cannot be named"
)]
fn lifecycle_error_parts(error: Option<&AgentError>) -> (Option<String>, Option<u16>) {
    (
        error
            .and_then(AgentError::model_kind)
            .map(|kind| format!("{kind:?}")),
        error
            .and_then(AgentError::model_context)
            .and_then(plakous::model::ModelErrorContext::http_status)
            .map(|status| status.get()),
    )
}

/// The `Unresolved` marker: the turn ended without canonical terminal
/// evidence. It reports the gap; it never invents an outcome.
pub(super) fn unresolved_turn_lifecycle(
    turn_id: plakous::model::TurnId,
    error: Option<&AgentError>,
) -> TurnLifecycle {
    let (model_error_kind, http_status) = lifecycle_error_parts(error);
    TurnLifecycle {
        turn_id: turn_id.to_string(),
        phase: TurnPhase::Unresolved,
        model_error_kind,
        http_status,
    }
}

/// Publishes one lifecycle marker and records it in the session log.
pub(super) fn emit_turn_lifecycle<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    log: Option<&SessionLog>,
    session: &SessionRecord,
    lifecycle: TurnLifecycle,
) {
    log_event(
        log,
        &session.id,
        SessionEvent::new("turn.lifecycle")
            .str("turn_id", lifecycle.turn_id.clone())
            .str("phase", lifecycle.phase.as_str()),
    );
    let _ = events.send(ContentEvent::TurnLifecycle(lifecycle));
}

/// The `Admitted` marker minted when the host takes ownership of a new turn.
pub(super) fn admitted_turn_lifecycle(turn_id: plakous::model::TurnId) -> TurnLifecycle {
    TurnLifecycle {
        turn_id: turn_id.to_string(),
        phase: TurnPhase::Admitted,
        model_error_kind: None,
        http_status: None,
    }
}

pub(super) fn operator_facing_model_context_suffix(
    context: &plakous::model::ModelErrorContext,
) -> String {
    let mut suffix = String::new();
    if let Some(status) = context.http_status() {
        suffix.push_str(&format!(" · HTTP {}", status.get()));
    }
    if let Some(code) = context.provider_code() {
        suffix.push_str(" · provider code: ");
        suffix.push_str(code);
    }
    suffix
}

pub(crate) fn operator_facing_agent_error(prefix: &str, error: &AgentError) -> String {
    let mut raw = format!("{prefix}: {}", error.message());
    if error.is_storage_full() {
        return crate::storage_pressure::storage_full_message(&raw);
    }
    if let Some(pressure) = plakous::sessions::SessionStorePressure::from_agent_error(error) {
        return crate::storage_pressure::storage_pressure_message(Some(pressure), &raw);
    }
    let retry_after = error.model_context().and_then(|context| {
        raw.push_str(&operator_facing_model_context_suffix(context));
        match context.retryability() {
            plakous::model::ModelRetryability::Eligible { retry_after } => {
                retry_after.map(|delay| delay.as_millis().div_ceil(1000))
            }
            _ => None,
        }
    });
    operator_facing_render(raw, error.model_kind(), retry_after)
}

/// The operator text for a failure: the typed model kind is the only
/// classifier. A diagnostic without a kind, or with a kind outside this table,
/// renders raw; its words are never read for a category.
fn operator_facing_render(
    raw: String,
    kind: Option<plakous::model::ModelErrorKind>,
    retry_after_secs: Option<u64>,
) -> String {
    use plakous::model::ModelErrorKind;
    let retry_hint =
        retry_after_secs.map_or_else(String::new, |secs| format!(" · retry after {secs}s"));
    let headline = match kind {
        Some(ModelErrorKind::RateLimited) => format!(
            "the provider returned a rate-limit response; the exact cause is unavailable{retry_hint}"
        ),
        Some(ModelErrorKind::QuotaExceeded) => {
            format!("the provider reported that quota is exhausted{retry_hint}")
        }
        Some(ModelErrorKind::Unavailable) => format!(
            "the selected provider/model is temporarily unavailable — retry shortly or select another model{retry_hint}"
        ),
        Some(ModelErrorKind::UnsupportedCapability) => "request uses a capability not admitted by the selected model; inspect its supported options".to_owned(),
        Some(ModelErrorKind::Authentication) => {
            "provider authentication failed — the account credential was refused".to_owned()
        }
        Some(ModelErrorKind::ReauthenticationRequired) => {
            "the selected subscription requires interactive reauthentication".to_owned()
        }
        Some(ModelErrorKind::ProtectedHistory) => "protected history cannot be replayed under the selected binding; clear context or fork from a checkpoint without incompatible native history, or switch back to the model and account that produced it".to_owned(),
        Some(ModelErrorKind::ContextLimit) => {
            "the request exceeded the model or route context limit".to_owned()
        }
        Some(ModelErrorKind::Policy) => "the provider refused this request under its usage policy; this is not a credential failure".to_owned(),
        _ => return raw,
    };
    format!("{headline}\n{raw}")
}

/// Observes one in-flight cancel request against its settle deadline.
///
/// Called at the top of every drive-loop iteration so settlement and the
/// bounded timeout are checked without blocking action or event drain.
pub(super) async fn observe_pending_cancel_settlement<
    P: plakous_host_protocol::ProtocolExtension,
>(
    control: &TurnControl,
    cancel_pending: bool,
    cancel_unsettled: &mut Option<String>,
    cancel_settle_deadline: &mut Option<Instant>,
    rows: &dyn TopologyRows,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
    projection: &mut TurnProjection<P>,
    interrupted_status_pending: &mut bool,
) -> bool {
    if !cancel_pending || cancel_unsettled.is_some() {
        return cancel_pending;
    }
    if control.is_settled() {
        *interrupted_status_pending = matches!(
            apply_cancel_settlement(
                rows,
                store,
                home,
                session,
                events,
                &CancelSettlement::Settled,
                Some(projection),
            )
            .await,
            CancelStatusPublication::Deferred
        );
        *cancel_settle_deadline = None;
        return false;
    }
    if let Some(detail) = cancel_settle_deadline_elapsed(*cancel_settle_deadline) {
        *cancel_unsettled = Some(detail.clone());
        apply_cancel_settlement(
            rows,
            store,
            home,
            session,
            events,
            &CancelSettlement::Unsettled(detail),
            Some(projection),
        )
        .await;
        *cancel_settle_deadline = None;
    }
    cancel_pending
}

pub(super) fn cancel_settle_deadline_elapsed(deadline: Option<Instant>) -> Option<String> {
    deadline
        .filter(|deadline| Instant::now() >= *deadline)
        .map(|_| cancel_settle_timeout_detail())
}

pub(super) fn cancel_settle_timeout_detail() -> String {
    let timeout = HOST_CANCEL_SETTLE_TIMEOUT;
    format!(
        "cancel did not settle within {}ms; the turn is still running",
        timeout.as_millis()
    )
}

/// Applies the settled-cancel bookkeeping when a cancel the host requested but
/// never observed settle later reaches a `Cancelled` terminal.
///
/// The terminal *is* the settlement evidence the bounded wait did not get, so
/// the root is marked interrupted and the queue is restored paused exactly as
/// an in-window settlement would have done. Without this the queue stays
/// `LiveAutoDrain` and the next queued item auto-starts right after the
/// operator's Esc.
pub(super) async fn settle_late_cancel<P: plakous_host_protocol::ProtocolExtension>(
    rows: &dyn TopologyRows,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
    cancel_unsettled: Option<&str>,
) {
    if cancel_unsettled.is_none() {
        return;
    }
    // The turn's own terminal already swept its tool rows before this runs, so
    // nothing is left to defer behind.
    let _ = apply_cancel_settlement(
        rows,
        store,
        home,
        session,
        events,
        &CancelSettlement::Settled,
        None,
    )
    .await;
}

/// Projects one bounded cancel settlement.
///
/// A settled cancel terminalises every tool row it is still holding open,
/// marks the root interrupted, restores the queue paused and paints
/// `interrupted`. An unsettled cancel writes none of that: it paints
/// `interrupting` with the observed condition, keeps the session open and
/// leaves the turn to reach its own terminal `TurnResult`.
///
/// The tool sweep runs *before* `interrupted` is published, and it is the only
/// ordering guarantee available: the observation stream is not allowed to delay
/// the authoritative turn result, so the host cannot wait for a tool's own
/// cancelled event before settling. Settlement is the moment the runtime says
/// nothing is still running, so any row the host still holds `running` at that
/// point is one whose terminal event it will never get in time — publishing it
/// here, once, is what stops a surface from being left with a tool row that
/// never terminalises and from seeing `interrupted` before the tool it
/// cancelled. A tool whose own terminal *was* observed first has already
/// written a terminal row, and the sweep skips it.
pub(super) async fn apply_cancel_settlement<P: plakous_host_protocol::ProtocolExtension>(
    rows: &dyn TopologyRows,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
    settlement: &CancelSettlement,
    projection: Option<&mut TurnProjection<P>>,
) -> CancelStatusPublication {
    match settlement {
        CancelSettlement::Settled => {
            let tree_id = durable_tree_id(rows, session);
            match topology_state::mark_root_interrupted(&tree_id).await {
                Ok(0) => {}
                Ok(children) => {
                    let _ = events.notice(TranscriptRow::info(
                        NoticeScope::Tree,
                        format!(
                            "interrupting {children} running child agent{}; mail resumes {} after release",
                            if children == 1 { "" } else { "s" },
                            if children == 1 { "it" } else { "them" }
                        ),
                    ));
                }
                Err(error) => {
                    let _ = events.notice(TranscriptRow::error(
                        NoticeScope::Tree,
                        format!("tree interrupt marker failed: {error}"),
                    ));
                }
            }
            let _ = commit_session_update(store, home, session, |candidate| {
                candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                true
            });
            emit_queue_snapshot(events, session);
            // The strip must not say `interrupted` over a tool row the host is
            // still holding open. Publication waits for the last one to
            // terminalise — through its own observed terminal if it reports
            // one, or through the sweep at the turn's terminal if it never
            // does. Only the presentation waits: the tree marker and the
            // paused queue above are authority and are written now.
            if let Some(projection) = projection {
                if has_running_tool_activity(session) {
                    return CancelStatusPublication::Deferred;
                }
                publish_cancel_window_tool_lines(events, projection);
            } else if has_running_tool_activity(session) {
                return CancelStatusPublication::Deferred;
            }
            publish_interrupted_status(events, session);
        }
        CancelSettlement::Unsettled(detail) => {
            let _ = events.send(ContentEvent::Status {
                phase: "interrupting".into(),
                queue_len: session.pending_queue.len(),
                detail: Some(detail.clone()),
            });
            let _ = events.notice(TranscriptRow::error(NoticeScope::Turn, detail.clone()));
        }
    }
    CancelStatusPublication::Published
}

/// Whether the settled-cancel `interrupted` status has been published yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CancelStatusPublication {
    /// Nothing is waiting: the strip already carries the settled state.
    Published,
    /// A tool row is still open; the status is owed once it terminalises.
    Deferred,
}

/// Whether the host is still holding a tool row open in this session.
pub(super) fn has_running_tool_activity(session: &SessionRecord) -> bool {
    session
        .transcript
        .iter()
        .filter_map(|row| row.as_tool().cloned())
        .any(|activity| activity.state == "running")
}

/// Publishes the settled-cancel status the host owes.
pub(super) fn publish_interrupted_status<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
) {
    let _ = events.send(ContentEvent::Status {
        phase: "interrupted".into(),
        queue_len: session.pending_queue.len(),
        detail: Some("interrupt requested".into()),
    });
}

/// Publishes, in one order, the terminal tool lines the cancel window owes and
/// then the deferred `interrupted` status — once no tool row is still open.
pub(super) fn flush_deferred_interrupted_status<P: plakous_host_protocol::ProtocolExtension>(
    pending: &mut bool,
    session: &SessionRecord,
    events: &Sender<P>,
    projection: &mut TurnProjection<P>,
) {
    if *pending && !has_running_tool_activity(session) {
        publish_cancel_window_tool_lines(events, projection);
        publish_interrupted_status(events, session);
        *pending = false;
    }
}

/// Publishes the terminal tool lines the observed path handed over.
pub(super) fn publish_cancel_window_tool_lines<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    projection: &mut TurnProjection<P>,
) {
    for line in std::mem::take(&mut projection.cancel_window_terminals) {
        let _ = events.send(line);
    }
}

pub(super) const fn turn_finalization_ready(result_done: bool, _events_done: bool) -> bool {
    result_done
}

/// Reconcile streamed presentation with the successful result's canonical text.
///
/// A normal stream is already exact and emits nothing. If the result raced ahead
/// of a final queued delta, only the missing suffix is emitted, never the full
/// response a second time. A non-prefix mismatch still adopts the authoritative
/// text for persistence without duplicating already-painted presentation bytes.
pub(super) fn reconcile_authoritative_assistant(
    streamed: &mut String,
    authoritative: String,
) -> Option<String> {
    if *streamed == authoritative {
        return None;
    }
    let missing = authoritative
        .strip_prefix(streamed.as_str())
        .map(str::to_owned);
    *streamed = authoritative;
    missing.filter(|suffix| !suffix.is_empty())
}

/// Flush coalesced assistant text deltas to the TUI.
///
/// When `force` is false, only flushes if the buffer is non-empty and at least
/// 20ms have elapsed since the last flush.
pub(super) fn flush_assistant_delta<P: plakous_host_protocol::ProtocolExtension>(
    delta_buf: &mut String,
    last_flush: &mut Instant,
    events: &Sender<P>,
    force: bool,
) {
    if delta_buf.is_empty() {
        return;
    }
    if force || last_flush.elapsed() >= Duration::from_millis(20) {
        let _ = events.send(ContentEvent::AssistantDelta(std::mem::take(delta_buf)));
        *last_flush = Instant::now();
    }
}

/// Claims one pending approval by its host-minted handle.
///
/// Returning `None` is the whole point: an unknown, forged, or already-claimed
/// handle never reaches the runtime, and a second intent for the same handle
/// cannot submit a second decision.
pub(super) fn take_pending_approval<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    approval_id: &str,
) -> Option<PendingApproval> {
    projection.approvals.remove(approval_id)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reasoning_only_tests;
