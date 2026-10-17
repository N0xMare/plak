//! Turn outcome publication, cancel settlement, and approvals.

use crate::session_home::SessionHome;
use crate::session_log::{SessionEvent, SessionLog};
use crate::session_store::SessionRecordStore;
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
use super::models::humanize_turn_error;
use super::rebind::durable_tree_id;
use super::reconcile::{
    commit_definite_failure_projection, commit_exceptional_terminal_projection,
};
use super::{
    DriveTurnError, DrivenTurn, ExceptionalTerminalProjection, HOST_CANCEL_SETTLE_TIMEOUT,
    HostEvent, PendingApproval, Sender, TopologyRows, TurnHostOutcome, TurnProjection,
    topology_state,
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
        }
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

pub(super) fn terminal_turn_outcome(result: &TurnResult) -> TurnHostOutcome {
    match result.outcome() {
        TurnResultOutcome::Completed(response) => TurnHostOutcome::Completed {
            assistant: super::tools::joined_assistant_text(response),
            finish_reason: response.finish().reason(),
            finish_detail: response.finish().detail().map(safe_finish_detail),
            reasoning_tokens: result.usage().model().reasoning_tokens(),
            output_tokens: result.usage().model().output_tokens(),
            tool_calls: result.tool_calls(),
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
                    transient: error.model_error().and_then(|model| {
                        let context = model.context()?;
                        crate::agent::transient::classify(
                            model.kind(),
                            context.phase(),
                            context.retryability(),
                        )
                        .map(|gate| super::TransientFailure {
                            failed_turn: result.turn_id(),
                            gate,
                        })
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
    let _ = events.send(HostEvent::TurnLifecycle(lifecycle));
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

pub(super) fn publish_recovered_turn_result<P: plakous_host_protocol::ProtocolExtension>(
    result: Result<std::sync::Arc<TurnResult>, AgentError>,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
) -> bool {
    let outcome = match result {
        Ok(result) => {
            emit_turn_lifecycle(events, None, session, turn_lifecycle(&result));
            terminal_turn_outcome(&result)
        }
        Err(error) => {
            let error = operator_facing_agent_error("restored agent turn failed", &error);
            let saved = commit_exceptional_terminal_projection(
                session,
                ExceptionalTerminalProjection::UnresolvedError(&error),
                |candidate| store.save(candidate),
            );
            if saved.is_ok() {
                let _ = events.send(HostEvent::Row(TranscriptRow::error(
                    NoticeScope::Turn,
                    error,
                )));
            }
            return true;
        }
    };

    publish_recovered_turn_outcome(outcome, store, home, session, events)
}

pub(super) fn publish_recovered_turn_outcome<P: plakous_host_protocol::ProtocolExtension>(
    outcome: TurnHostOutcome,
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    events: &Sender<P>,
) -> bool {
    match outcome {
        TurnHostOutcome::ConsumedDelivery => false,
        TurnHostOutcome::Completed {
            assistant,
            finish_reason,
            finish_detail,
            ..
        } => {
            let transcript_line = completed_turn_row(&assistant);
            let saved = commit_session_update(store, home, session, |candidate| {
                candidate.append_transcript(transcript_line);
                if finish_reason == plakous::model::FinishReason::Refusal {
                    candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                }
                true
            });
            if saved.is_err() {
                return true;
            }
            if assistant.is_empty() && finish_reason != plakous::model::FinishReason::Refusal {
                let _ = events.notice(TranscriptRow::info(
                    NoticeScope::Turn,
                    "restored turn finished with no visible text",
                ));
            }
            if !assistant.is_empty() {
                let _ = events.send(HostEvent::AssistantDelta(assistant));
                let _ = events.send(HostEvent::AssistantDone);
            }
            if finish_reason == plakous::model::FinishReason::Refusal {
                let _ = events.send(super::logging::project_session_snapshot(session));
                let _ = events.send(HostEvent::ModelRefusal {
                    detail: finish_detail,
                });
            }
            false
        }
        TurnHostOutcome::AdmissionRejected { error, .. } => {
            let _ = events.notice(TranscriptRow::error(NoticeScope::Turn, error));
            false
        }
        TurnHostOutcome::ContextCapacity { error, .. } | TurnHostOutcome::Failed { error, .. } => {
            if commit_definite_failure_projection(session, &error, |candidate| {
                store.save(candidate)
            })
            .is_ok()
            {
                let _ = events.send(HostEvent::Row(TranscriptRow::error(
                    NoticeScope::Turn,
                    humanize_turn_error(&error),
                )));
                false
            } else {
                true
            }
        }
        TurnHostOutcome::RecoveryRequired { error } => {
            let saved = commit_exceptional_terminal_projection(
                session,
                ExceptionalTerminalProjection::RecoveryRequired(&error),
                |candidate| store.save(candidate),
            );
            if saved.is_ok() {
                let _ = events.send(HostEvent::Row(TranscriptRow::error(
                    NoticeScope::Turn,
                    error,
                )));
            }
            true
        }
        TurnHostOutcome::Interrupted { assistant, .. } => {
            let saved = commit_exceptional_terminal_projection(
                session,
                ExceptionalTerminalProjection::Interrupted(&assistant),
                |candidate| store.save(candidate),
            );
            if saved.is_ok() {
                let _ = events.send(HostEvent::Row(TranscriptRow::Interrupted));
                false
            } else {
                true
            }
        }
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

/// The exact provider-attested quota notice, passed through unframed.
const FABLE_LIMIT_NOTICE: &str = "You've reached your Fable limit.";

pub(crate) fn operator_facing_agent_error(prefix: &str, error: &AgentError) -> String {
    let mut raw = format!("{prefix}: {}", error.message());
    if error.is_storage_full() {
        return crate::storage_pressure::storage_full_message(&raw);
    }
    if let Some(pressure) = plakous::sessions::SessionStorePressure::from_agent_error(error) {
        return crate::storage_pressure::storage_pressure_message(Some(pressure), &raw);
    }
    if let Some(context) = error.model_context() {
        raw.push_str(&operator_facing_model_context_suffix(context));
    }
    operator_facing_render(raw, error.model_kind())
}

fn operator_facing_render(raw: String, kind: Option<plakous::model::ModelErrorKind>) -> String {
    // Attested provider notices pass through unframed, exactly as the text
    // path treats them; the typed kind must not fabricate quota over one.
    if raw.contains(FABLE_LIMIT_NOTICE) {
        return raw;
    }
    if let Some(rendered) = typed_model_kind_error(&raw, kind) {
        return rendered;
    }
    if kind.is_some() {
        return raw;
    }
    operator_facing_error(&raw)
}

/// The typed classification for an error carrying a model kind.
///
/// The kind is the authority for these categories: a diagnostic that merely
/// contains "429" — a request id, a suffix field — is not a rate limit. A
/// typed kind outside this table still suppresses the text heuristics: the
/// caller renders the raw diagnostic rather than guessing a category.
fn typed_model_kind_error(
    raw: &str,
    kind: Option<plakous::model::ModelErrorKind>,
) -> Option<String> {
    use plakous::model::ModelErrorKind;
    match kind? {
        ModelErrorKind::RateLimited => Some(format!(
            "the provider returned a rate-limit response; the exact cause is unavailable{}\n{raw}",
            retry_after_detail(raw).map_or_else(String::new, |hint| format!(" · {hint}")),
        )),
        ModelErrorKind::QuotaExceeded => Some(format!(
            "the provider reported that quota is exhausted{}\n{raw}",
            retry_after_detail(raw).map_or_else(String::new, |hint| format!(" · {hint}")),
        )),
        ModelErrorKind::UnsupportedCapability => Some(format!(
            "request uses a capability not admitted by the selected model; inspect its supported options\n{raw}"
        )),
        ModelErrorKind::Authentication => Some(format!(
            "provider authentication failed — the account credential was refused\n{raw}"
        )),
        ModelErrorKind::ReauthenticationRequired => Some(format!(
            "the selected subscription requires interactive reauthentication\n{raw}"
        )),
        ModelErrorKind::ProtectedHistory => Some(format!(
            "protected history cannot be replayed under the selected binding; clear context or fork from a checkpoint without incompatible native history, or switch back to the model and account that produced it\n{raw}"
        )),
        ModelErrorKind::ContextLimit => Some(format!(
            "the request exceeded the model or route context limit\n{raw}"
        )),
        ModelErrorKind::Policy => Some(format!(
            "the provider refused this request under its usage policy; this is not a credential failure\n{raw}"
        )),
        _ => None,
    }
}

/// The provider-reported reset hint, when the diagnostic carries one.
fn retry_after_detail(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    for key in [
        "resets_at",
        "reset_at",
        "resets at",
        "retry after",
        "retry-after",
    ] {
        if let Some(idx) = lower.find(key) {
            let slice = raw
                .get(idx..)
                .unwrap_or("")
                .chars()
                .take(96)
                .collect::<String>();
            return Some(slice.trim().to_owned());
        }
    }
    None
}

/// Make provider failures readable for operators (Codex CLI–style framing).
pub(super) fn operator_facing_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("server_is_overloaded") || lower.contains("server is overloaded") {
        return format!(
            "the selected provider/model is temporarily overloaded — retry shortly or select another model\n{raw}"
        );
    }
    // This exact notice is emitted by the provider adapter after quota attestation.
    if raw.contains(FABLE_LIMIT_NOTICE) {
        return raw.to_owned();
    }
    let quota = lower.contains("quotaexceeded")
        || lower.contains("quota_exceeded")
        || lower.contains("usage_limit")
        || lower.contains("usage limit");
    let rateish = lower.contains("ratelimited")
        || lower.contains("rate_limited")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
        || quota;
    if rateish {
        let mut out = if quota {
            String::from("the provider reported that quota is exhausted")
        } else {
            String::from(
                "the provider returned a rate-limit response; the exact cause is unavailable",
            )
        };
        if let Some(hint) = retry_after_detail(raw) {
            out.push_str(" · ");
            out.push_str(&hint);
        }
        out.push('\n');
        out.push_str(raw);
        return out;
    }
    // Adapter rejected effort / options before the network (common: xhigh/max/ultra
    // on models that only admit low|medium|high, or mistyped capability).
    if lower.contains("admitted model/adapter subset")
        || lower.contains("unsupportedreasoningeffort")
        || lower.contains("unsupported capability")
        || lower.contains("unsupportedreasoning")
    {
        return format!(
            "request not supported for this model/effort — try effort low|medium|high (or another model)\n{raw}"
        );
    }
    if lower.contains("unresolved working turn") {
        return format!(
            "session has unresolved work; inspect recovery state before continuing\n{raw}"
        );
    }
    if lower.contains("responses error") || lower.contains("terminal responses") {
        return format!(
            "provider rejected or aborted the Responses stream (network, quota, or protocol)\n{raw}"
        );
    }
    raw.to_owned()
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
                            "stopped {children} running child agent{}; mail resumes {}",
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
            let _ = events.send(HostEvent::Status {
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
    let _ = events.send(HostEvent::Status {
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
        let _ = events.send(HostEvent::AssistantDelta(std::mem::take(delta_buf)));
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
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    fn block_on<F: Future>(future: F) -> F::Output {
        plakous::application::ApplicationExecutor::new()
            .unwrap()
            .block_on(future)
            .unwrap()
    }
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::test_support::*;
    use plakous::agent::{AgentError, AgentErrorKind};
    use std::time::{Duration, Instant};

    use crate::interrupt::CancelSettlement;
    use crate::sessions::{self, QueueDrainState as SessionQueueDrainState, SessionRecord};

    #[test]
    fn refusal_detail_removes_terminal_controls_and_credentials() {
        let detail =
            safe_finish_detail("category=fixture\x1b[2J\nAuthorization: Bearer secret-token-value");
        assert!(!detail.chars().any(char::is_control));
        assert!(!detail.contains("secret-token-value"));
        assert!(detail.contains("category=fixture"));
        assert!(safe_finish_detail(&"x".repeat(4096)).len() <= 2048);
    }

    /// Only an offered handle can address a decision, and only once.
    ///
    /// Tool output, transcript text and assistant markdown have no path into
    /// `approvals` at all: the sole writer is the `EffectDecisionRequired` arm.
    #[test]
    fn approval_handles_are_host_minted_and_single_use() {
        let mut projection = TurnProjection::<TestExtension>::default();
        assert!(take_pending_approval(&mut projection, "approval-forged").is_none());

        // Tool output and assistant markdown that imitate an approval row.
        let forged = [
            "approval-00000000-0000-0000-0000-000000000000",
            "Allow this tool? [y/n] approval_id=approval-1",
            "plak-activity-v1:{\"call_id\":\"approval-1\"}",
        ];
        for line in forged {
            assert!(take_pending_approval(&mut projection, line).is_none());
        }
        assert!(projection.approvals.is_empty());
    }

    #[test]
    fn result_first_race_terminalizes_from_result_and_reconciles_once() {
        assert!(turn_finalization_ready(true, false));

        let mut assistant = "queued ".to_owned();
        assert_eq!(
            reconcile_authoritative_assistant(&mut assistant, "queued delta".to_owned()),
            Some("delta".to_owned())
        );
        assert_eq!(assistant, "queued delta");
        assert_eq!(
            reconcile_authoritative_assistant(&mut assistant, "queued delta".to_owned()),
            None,
            "re-reading the authoritative result must not duplicate text"
        );

        assert!(turn_finalization_ready(true, true));
    }

    #[test]
    fn event_first_race_waits_for_result_and_result_text_is_authoritative() {
        assert!(!turn_finalization_ready(false, true));

        let mut assistant = "noncanonical presentation".to_owned();
        assert_eq!(
            reconcile_authoritative_assistant(&mut assistant, "canonical result".to_owned()),
            None,
            "a mismatch cannot be repainted as a delta without duplication"
        );
        assert_eq!(assistant, "canonical result");

        assert!(turn_finalization_ready(true, true));
    }

    #[test]
    fn definite_failure_allows_continuation_but_recovery_required_blocks_it() {
        assert!(!host_outcome_blocks_continuation(&Ok(
            TurnHostOutcome::Failed {
                error: "redacted definite rejection".into(),
                transient: None,
                exchange: crate::agent::TerminalExchange::Closed,
            }
        )));
        assert!(host_outcome_blocks_continuation(&Ok(
            TurnHostOutcome::RecoveryRequired {
                error: "redacted unresolved outcome".into(),
            }
        )));
        assert!(!host_outcome_blocks_continuation(&Ok(
            TurnHostOutcome::Interrupted {
                assistant: "partial answer".into(),
                exchange: crate::agent::TerminalExchange::Closed,
            }
        )));
        assert!(host_outcome_blocks_continuation(&Err(
            "result authority unavailable".into()
        )));
    }

    #[test]
    fn unavailable_restored_result_blocks_admission_and_emits_one_terminal_error() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        let blocked = publish_recovered_turn_result(
            Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "restored result authority unavailable",
            )),
            &crate::session_store::MemoryStore::new(),
            &SessionHome::new(root.path().join("sessions")),
            &mut session,
            &events,
        );

        assert!(blocked);
        assert_eq!(
            session.queue_drain_state,
            SessionQueueDrainState::RestoredPaused
        );
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    HostEvent::Row(TranscriptRow::Notice(
                        plakous_host_protocol::views::transcript::NoticeRow {
                            tone: plakous_host_protocol::views::transcript::NoticeTone::Error,
                            ..
                        }
                    ))
                ))
                .count(),
            1
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, HostEvent::AssistantDone))
        );
    }

    #[test]
    fn restored_completed_outcome_is_committed_and_presented_once() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        assert!(!publish_recovered_turn_outcome(
            TurnHostOutcome::Completed {
                assistant: "authoritative restored output".into(),
                finish_reason: plakous::model::FinishReason::Stop,
                finish_detail: None,
                reasoning_tokens: 0,
                output_tokens: 8,
                tool_calls: 0,
            },
            &crate::session_store::MemoryStore::new(),
            &SessionHome::new(root.path().join("sessions")),
            &mut session,
            &events,
        ));

        let observed = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, HostEvent::AssistantDone))
                .count(),
            1
        );
        assert_eq!(
            session
                .transcript
                .iter()
                .filter(|line| matches!(line, TranscriptRow::Assistant { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn operator_facing_model_context_suffix_includes_http_status_and_provider_code() {
        use plakous::model::{
            ModelErrorContext, ModelErrorPhase, ModelId, ModelProviderCode, ModelRetryability,
            ProtocolId, RouteId,
        };

        let context = ModelErrorContext::new(
            ProtocolId::new("example.responses").unwrap(),
            RouteId::new("example.platform").unwrap(),
            ModelId::new("test-model").unwrap(),
            ModelErrorPhase::FirstEvent,
            ModelRetryability::Permanent,
        )
        .unwrap()
        .with_http_status(serde_json::from_value(serde_json::json!(400)).unwrap())
        .with_provider_code(ModelProviderCode::new("invalid_request").unwrap());
        let suffix = operator_facing_model_context_suffix(&context);
        assert!(suffix.contains("HTTP 400"), "{suffix}");
        assert!(
            suffix.contains("provider code: invalid_request"),
            "{suffix}"
        );
    }

    #[test]
    fn saturated_observation_channel_resnapshots_authoritative_assistant() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        for index in 0..2_000 {
            assert!(events.notice(TranscriptRow::error(
                NoticeScope::Host,
                format!("diagnostic-{index}")
            )));
        }
        assert!(!events.send(HostEvent::AssistantDelta("incomplete".into())));

        let mut session = SessionRecord::new(PathBuf::from("/workspace"));
        session.append_transcript(TranscriptRow::user("prompt"));
        session.append_transcript(TranscriptRow::assistant("authoritative TurnResult output"));
        events.resnapshot_transcript_if_required(&session);
        assert!(events.send(HostEvent::SetBusy(false)));
        assert!(events.send(HostEvent::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: Some("terminal".into()),
        }));
        let drained = rx.try_iter().collect::<Vec<_>>();
        assert!(drained.contains(&logging::project_session_snapshot(&session)));
        assert!(drained.contains(&HostEvent::SetBusy(false)));
        assert!(drained.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, detail: Some(detail), .. }
                if phase == "idle" && detail == "terminal"
        )));
    }

    #[test]
    fn trusted_fable_limit_notice_is_not_reframed_as_unknown_rate_limit() {
        let raw =
            "agent turn failed: Anthropic RateLimited HTTP 429: You've reached your Fable limit.";
        assert_eq!(operator_facing_error(raw), raw);
    }

    #[test]
    fn rate_limit_presentation_does_not_invent_subscription_exhaustion() {
        let rendered =
            operator_facing_error("anthropic/RateLimited status=Some(429) request_id=req_safe");
        assert!(rendered.starts_with(
            "the provider returned a rate-limit response; the exact cause is unavailable"
        ));
        assert!(rendered.contains("request_id=req_safe"));
        assert!(!rendered.contains("plan window"));
        assert!(!rendered.contains("quota is exhausted"));
    }

    #[test]
    fn exact_quota_presentation_remains_distinct_from_generic_429() {
        let rendered = operator_facing_error("model QuotaExceeded: provider quota exhausted");
        assert!(rendered.starts_with("the provider reported that quota is exhausted"));
    }

    #[test]
    fn store_capacity_presentation_is_typed_and_branches_on_the_refused_class() {
        let raw = "agent turn failed: checkpoints 14/16";
        let ordinary = crate::storage_pressure::storage_pressure_message(None, raw);
        assert!(ordinary.contains("new work is refused"));
        let gc = ordinary.find("`plak session gc --all`").unwrap();
        assert!(gc < ordinary.find("`plak session delete <id>`").unwrap());
        assert!(ordinary.contains("do not reclaim it"));
        assert!(ordinary.ends_with(raw));
        // Text alone is not a capacity classification.
        let untyped = "session store capacity was reached: checkpoints 14/16";
        assert_eq!(operator_facing_error(untyped), untyped);
    }

    #[test]
    fn provider_overload_presentation_preserves_actionable_typed_code() {
        let rendered = operator_facing_error(
            "agent turn failed: Responses provider returned a typed terminal error · provider code: server_is_overloaded",
        );
        assert!(rendered.starts_with(
            "the selected provider/model is temporarily overloaded — retry shortly or select another model"
        ));
        assert!(rendered.contains("server_is_overloaded"));
    }

    #[test]
    fn unsettled_cancel_paints_interrupting_and_never_asserts_interrupted() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();

        block_on(apply_cancel_settlement(
            &HostRows::new(&config.state_roots, config.store.as_ref()),
            &*config.store,
            &config.home,
            &mut config.session,
            &events,
            &CancelSettlement::Unsettled(
                "cancel did not settle within 5000ms; the turn is still running".into(),
            ),
            None,
        ));

        let observed: Vec<HostEvent> = rx.try_iter().collect();
        assert_eq!(status_phases(&observed), ["interrupting"]);
        assert!(
            observed.iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("did not settle")
            )),
            "an unsettled cancel must be projected as a truthful note: {observed:?}"
        );
        // The session stays open: no terminal claim, no restored-paused write.
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::LiveAutoDrain
        );
    }

    #[test]
    fn settled_cancel_asserts_interrupted_and_restores_paused_queue() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();

        block_on(apply_cancel_settlement(
            &HostRows::new(&config.state_roots, config.store.as_ref()),
            &*config.store,
            &config.home,
            &mut config.session,
            &events,
            &CancelSettlement::Settled,
            None,
        ));

        let observed: Vec<HostEvent> = rx.try_iter().collect();
        assert!(
            status_phases(&observed).contains(&"interrupted"),
            "a settled cancel asserts the interrupted phase: {observed:?}"
        );
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::RestoredPaused
        );
    }

    /// A cancel that never settled in the bounded window, but whose turn later
    /// terminalises `Cancelled`, gets the settled bookkeeping from the terminal.
    ///
    /// Without it the queue stays `LiveAutoDrain` and the next queued item
    /// auto-starts immediately after the operator's Esc.
    /// The strip may not say `interrupted` over a tool row the host is still
    /// holding open: the settled cancel defers its status and something else
    /// terminalising the row publishes it.
    #[test]
    fn a_settled_cancel_defers_interrupted_behind_an_open_tool_row() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session.upsert_tool_activity(sessions::ToolItem {
            call_id: "call-1".into(),
            name: "loopback_effect".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("loopback_effect"),
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        });
        let (events, rx) = host_event_sink();
        assert_eq!(
            block_on(apply_cancel_settlement(
                &HostRows::new(&config.state_roots, config.store.as_ref()),
                &*config.store,
                &config.home,
                &mut config.session,
                &events,
                &CancelSettlement::Settled,
                None,
            )),
            CancelStatusPublication::Deferred
        );
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(
            !observed.iter().any(|event| matches!(
                event,
                HostEvent::Status { phase, .. } if phase == "interrupted"
            )),
            "the status must wait for the open tool row: {observed:?}"
        );

        // Nothing is owed while the row is open.
        let mut projection = TurnProjection::<TestExtension>::default();
        let mut pending = true;
        flush_deferred_interrupted_status(&mut pending, &config.session, &events, &mut projection);
        assert!(pending);
        assert!(rx.try_iter().next().is_none());

        // Terminalising it releases the status, exactly once.
        config.session.upsert_tool_activity(sessions::ToolItem {
            call_id: "call-1".into(),
            name: "loopback_effect".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("loopback_effect"),
            state: "cancelled".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            parent_call_id: None,
            cell_id: None,
            wait: None,
        });
        flush_deferred_interrupted_status(&mut pending, &config.session, &events, &mut projection);
        assert!(!pending);
        let released = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            released
                .iter()
                .filter(|event| matches!(
                    event,
                    HostEvent::Status { phase, .. } if phase == "interrupted"
                ))
                .count(),
            1,
            "{released:?}"
        );
        flush_deferred_interrupted_status(&mut pending, &config.session, &events, &mut projection);
        assert!(rx.try_iter().next().is_none(), "the status is owed once");
    }

    #[test]
    fn a_late_cancelled_terminal_applies_the_settled_bookkeeping() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();

        // No cancel was ever requested: a plain terminal writes nothing.
        block_on(settle_late_cancel(
            &HostRows::new(&config.state_roots, config.store.as_ref()),
            &*config.store,
            &config.home,
            &mut config.session,
            &events,
            None,
        ));
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::LiveAutoDrain
        );
        assert!(rx.try_iter().next().is_none());

        block_on(settle_late_cancel(
            &HostRows::new(&config.state_roots, config.store.as_ref()),
            &*config.store,
            &config.home,
            &mut config.session,
            &events,
            Some("cancel did not settle within 5000ms; the turn is still running"),
        ));
        let observed: Vec<HostEvent> = rx.try_iter().collect();
        assert!(
            status_phases(&observed).contains(&"interrupted"),
            "the terminal is the settlement evidence: {observed:?}"
        );
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::RestoredPaused
        );
    }

    #[test]
    fn cancel_settle_timeout_detail_uses_the_host_timeout() {
        let detail = cancel_settle_timeout_detail();
        assert!(
            detail.contains("still running"),
            "unsettled detail must stay truthful: {detail}"
        );
        assert!(
            detail.contains(&format!("{}ms", HOST_CANCEL_SETTLE_TIMEOUT.as_millis())),
            "{detail}"
        );
    }

    /// Deadline observation stays non-blocking and paints unsettled truthfully.
    #[test]
    fn observe_pending_cancel_settlement_paints_unsettled_after_deadline() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();
        let mut cancel_settle_deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .map(Some)
            .expect("clock can subtract 1ms");
        let detail = cancel_settle_deadline_elapsed(cancel_settle_deadline)
            .expect("an elapsed deadline must produce unsettled detail");
        let cancel_unsettled = Some(detail.clone());
        block_on(apply_cancel_settlement(
            &HostRows::new(&config.state_roots, config.store.as_ref()),
            &*config.store,
            &config.home,
            &mut config.session,
            &events,
            &CancelSettlement::Unsettled(detail),
            None,
        ));
        cancel_settle_deadline = None;
        assert!(cancel_unsettled.is_some());
        assert!(cancel_settle_deadline.is_none());
        let observed: Vec<HostEvent> = rx.try_iter().collect();
        assert_eq!(status_phases(&observed), ["interrupting"]);
        assert!(
            observed.iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("did not settle")
            )),
            "deadline observation must stay unsettled: {observed:?}"
        );
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::LiveAutoDrain
        );
    }

    /// Lifecycle phases come from the canonical `TurnResult` outcome; the
    /// loopback scenarios pin which outcome each fixture produces.
    #[test]
    fn turn_lifecycle_maps_the_canonical_result_outcomes() {
        let root = tempfile::tempdir().unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let results = executor
            .block_on(async {
                use plakous::sessions::{
                    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
                    SessionOperationId,
                };
                use plakous_offline_crash_harness::{
                    BoundaryController, terminal_failure_runtime_binding,
                    text_runtime_binding_with_finish,
                };
                let sessions = AgentSessions::new(
                    SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
                )
                .unwrap();
                let controller = BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.path().join("events.jsonl"),
                )
                .unwrap();
                let mut results = Vec::new();
                for name in ["completed", "refused", "failed"] {
                    let workspace = root.path().join(name);
                    std::fs::create_dir_all(&workspace).unwrap();
                    let binding = match name {
                        "completed" => text_runtime_binding_with_finish(
                            &workspace,
                            controller.clone(),
                            "",
                            "done",
                            plakous::model::FinishReason::Stop,
                        )
                        .unwrap(),
                        "refused" => text_runtime_binding_with_finish(
                            &workspace,
                            controller.clone(),
                            "",
                            "declined",
                            plakous::model::FinishReason::Refusal,
                        )
                        .unwrap(),
                        _ => terminal_failure_runtime_binding(
                            &workspace,
                            controller.clone(),
                            plakous::model::ModelErrorKind::RateLimited,
                        )
                        .unwrap(),
                    };
                    let CreateSessionOutcome::Created { session, .. } = sessions
                        .create(CreateSessionRequest::new(
                            SessionOperationId::new(format!("lifecycle:{name}")).unwrap(),
                            binding,
                        ))
                        .await
                        .unwrap()
                    else {
                        panic!("new session for {name}")
                    };
                    let result = session
                        .agent()
                        .start(CommandId::new(format!("lifecycle:{name}")).unwrap(), "turn")
                        .await
                        .unwrap()
                        .into_turn()
                        .unwrap()
                        .result()
                        .await
                        .unwrap();
                    session.release().await.unwrap();
                    results.push((name, result));
                }
                results
            })
            .unwrap();
        for (name, result) in results {
            let lifecycle = turn_lifecycle(&result);
            assert_eq!(lifecycle.turn_id, result.turn_id().to_string());
            match (name, result.outcome()) {
                ("completed", TurnResultOutcome::Completed(_)) => {
                    assert_eq!(lifecycle.phase, TurnPhase::Completed);
                    assert_eq!(lifecycle.model_error_kind, None);
                    assert_eq!(lifecycle.http_status, None);
                }
                ("refused", TurnResultOutcome::Completed(_)) => {
                    assert_eq!(lifecycle.phase, TurnPhase::Refused);
                }
                ("failed", TurnResultOutcome::Failed(_)) => {
                    assert_eq!(lifecycle.phase, TurnPhase::Failed);
                    assert_eq!(lifecycle.model_error_kind.as_deref(), Some("RateLimited"));
                }
                (name, outcome) => panic!("{name} produced an unexpected outcome: {outcome:?}"),
            }
        }
    }

    /// `Unresolved` and `Admitted` markers carry no terminal evidence and are
    /// delivered through the observation sender unchanged.
    #[test]
    fn turn_lifecycle_reports_admitted_and_unresolved_without_terminal_evidence() {
        use std::str::FromStr as _;
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let session = SessionRecord::new(PathBuf::from("lifecycle-workspace"));
        let turn_id =
            plakous::model::TurnId::from_str("00000000-0000-0000-0000-0000000000c7").unwrap();
        emit_turn_lifecycle(&events, None, &session, admitted_turn_lifecycle(turn_id));
        emit_turn_lifecycle(
            &events,
            None,
            &session,
            unresolved_turn_lifecycle(turn_id, None),
        );
        for expected in [TurnPhase::Admitted, TurnPhase::Unresolved] {
            let HostEvent::TurnLifecycle(lifecycle) = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("a lifecycle observation")
            else {
                panic!("expected a turn lifecycle observation");
            };
            assert_eq!(lifecycle.phase, expected);
            assert_eq!(lifecycle.turn_id, turn_id.to_string());
            assert_eq!(lifecycle.model_error_kind, None);
            assert_eq!(lifecycle.http_status, None);
        }
    }

    /// A prompt emits exactly one admitted marker and one canonical terminal
    /// marker, correlated by turn id; busy and usage stay observations only.
    #[test]
    fn a_prompt_turn_lifecycle_is_admitted_then_completed_on_one_turn_id() {
        let root = tempfile::tempdir().unwrap();
        let (config, _injection) = fresh_injected_attached_host(root.path());
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        send_attached_action(&actions, UserAction::Prompt("hello".into()));
        let mut lifecycles = Vec::new();
        loop {
            let event = events
                .recv_timeout(Duration::from_secs(30))
                .expect("a host event");
            let HostEvent::TurnLifecycle(lifecycle) = event else {
                continue;
            };
            let terminal = lifecycle.phase != TurnPhase::Admitted;
            lifecycles.push(lifecycle);
            if terminal {
                break;
            }
        }
        send_attached_action(&actions, UserAction::Quit);
        worker.join().unwrap().unwrap();
        let remaining = events.try_iter().collect::<Vec<_>>();
        assert_eq!(lifecycles.len(), 2, "admitted plus one terminal");
        assert_eq!(lifecycles[0].phase, TurnPhase::Admitted);
        assert_eq!(lifecycles[1].phase, TurnPhase::Completed);
        assert_eq!(lifecycles[0].turn_id, lifecycles[1].turn_id);
        assert!(
            remaining
                .iter()
                .all(|event| !matches!(event, HostEvent::TurnLifecycle(_))),
            "a settled turn publishes no further markers: {remaining:?}"
        );
    }

    #[test]
    fn protected_history_render_offers_clear_or_fork_without_text_classification() {
        let raw = "native item has no compatible replay target".to_owned();
        let rendered = operator_facing_render(
            raw.clone(),
            Some(plakous::model::ModelErrorKind::ProtectedHistory),
        );
        assert!(rendered.contains("protected history"));
        assert!(rendered.contains("clear context or fork"));
        assert!(rendered.contains("switch back to the model and account that produced it"));
        assert!(rendered.ends_with(&raw));
        assert_eq!(
            operator_facing_render(
                raw.clone(),
                Some(plakous::model::ModelErrorKind::InvalidRequest)
            ),
            raw
        );
    }

    /// The typed kind is the category authority: an unrelated diagnostic
    /// containing "429" is not reframed as a rate limit, and an unmapped
    /// typed kind renders raw rather than borrowing a text guess.
    #[test]
    fn operator_facing_render_does_not_call_a_request_id_429_a_rate_limit() {
        let raw = "agent turn failed: protocol violation request_id=req_429abc".to_owned();
        let rendered =
            operator_facing_render(raw.clone(), Some(plakous::model::ModelErrorKind::Protocol));
        assert_eq!(rendered, raw);
        assert!(!rendered.contains("rate-limit"));
        assert!(!rendered.contains("quota"));
    }

    /// Typed quota exhaustion renders the quota category, not a generic 429.
    #[test]
    fn operator_facing_render_reports_typed_quota_exhaustion() {
        let rendered = operator_facing_render(
            "agent turn failed: provider refused the request".to_owned(),
            Some(plakous::model::ModelErrorKind::QuotaExceeded),
        );
        assert!(
            rendered.starts_with("the provider reported that quota is exhausted"),
            "{rendered}"
        );
    }

    /// A provider content refusal is not framed as a credential problem.
    #[test]
    fn operator_facing_render_reports_a_policy_refusal_as_a_refusal() {
        let rendered = operator_facing_render(
            "agent turn failed: Responses HTTP request failed: I'm sorry, I can't help with that request. · HTTP 403 · provider code: permission-denied".to_owned(),
            Some(plakous::model::ModelErrorKind::Policy),
        );
        assert!(
            rendered.starts_with("the provider refused this request under its usage policy"),
            "{rendered}"
        );
        assert!(!rendered.contains("authentication failed"));
    }

    /// An unknown outcome never earns a recovery promise from its words.
    #[test]
    fn operator_facing_agent_error_never_promises_recovery_for_unknown_outcomes() {
        let error = AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "model request outcome is unknown",
        );
        let rendered = operator_facing_agent_error("agent turn requires recovery", &error);
        assert!(!rendered.contains("will recover"));
        assert!(!rendered.contains("rate-limit"));
        assert!(rendered.contains("outcome is unknown"));
    }

    /// A real `AgentError` from a scripted `Protocol` failure carries the
    /// typed category into the helper, so a request id containing "429" is
    /// never reframed as a rate limit.
    #[test]
    fn operator_facing_agent_error_keeps_protocol_category_over_a_429_request_id() {
        let root = tempfile::tempdir().unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let error = executor
            .block_on(async {
                use plakous::sessions::{
                    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
                    SessionOperationId,
                };
                use plakous_offline_crash_harness::{
                    BoundaryController,
                    script::{SCRIPT_VERSION, ScriptPlan},
                    scripted_runtime_binding,
                };
                let sessions = AgentSessions::new(
                    SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
                )
                .unwrap();
                let controller = BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.path().join("events.jsonl"),
                )
                .unwrap();
                let workspace = root.path().join("protocol");
                std::fs::create_dir_all(&workspace).unwrap();
                let plan: ScriptPlan = serde_json::from_value(serde_json::json!({
                    "version": SCRIPT_VERSION,
                    "steps": [{
                        "fault": {
                            "at": "pre_stream",
                            "kind": "protocol",
                            "message": "provider diagnostic request_id=req_429 stream closed"
                        }
                    }]
                }))
                .unwrap();
                let binding =
                    scripted_runtime_binding(&workspace, controller, Arc::new(plan)).unwrap();
                let CreateSessionOutcome::Created { session, .. } = sessions
                    .create(CreateSessionRequest::new(
                        SessionOperationId::new("protocol:req429").unwrap(),
                        binding,
                    ))
                    .await
                    .unwrap()
                else {
                    panic!("new session")
                };
                let result = session
                    .agent()
                    .start(CommandId::new("protocol:req429").unwrap(), "turn")
                    .await
                    .unwrap()
                    .into_turn()
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                session.release().await.unwrap();
                result.error().cloned().expect("scripted protocol failure")
            })
            .unwrap();
        assert_eq!(
            error.model_kind(),
            Some(plakous::model::ModelErrorKind::Protocol)
        );
        let rendered = operator_facing_agent_error("agent turn failed", &error);
        assert!(rendered.contains("request_id=req_429"), "{rendered}");
        assert!(!rendered.contains("rate-limit"), "{rendered}");
        assert!(!rendered.contains("quota is exhausted"), "{rendered}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod reasoning_only_tests {
    use super::{TurnHostOutcome, terminal_turn_outcome};
    use crate::runtime::host_loop::publication::spent_budget_on_reasoning;

    #[test]
    fn terminal_overflow_uses_the_canonical_remedy_even_without_a_capacity_tuple() {
        use plakous::agent::ContextOverflowRemedy;
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
            SessionOperationId,
        };
        use plakous_host_protocol::ContextCapacityRecovery;
        use plakous_offline_crash_harness::{
            BoundaryController, projected_terminal_failure_runtime_binding,
        };

        let root = tempfile::tempdir().unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::None,
            root.path().join("overflow.jsonl"),
        )
        .unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        executor
            .block_on(async {
                let sessions = AgentSessions::new(
                    SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
                )
                .unwrap();
                let CreateSessionOutcome::Created { session, .. } = sessions
                    .create(CreateSessionRequest::new(
                        SessionOperationId::new("overflow:create").unwrap(),
                        projected_terminal_failure_runtime_binding(
                            root.path(),
                            controller,
                            ModelErrorKind::ContextLimit,
                        )
                        .unwrap(),
                    ))
                    .await
                    .unwrap()
                else {
                    panic!("new session");
                };
                let result = session
                    .agent()
                    .prompt("withheld provider-overflow prompt")
                    .await
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                assert!(result.context_capacity().is_none());
                assert_eq!(
                    result.context_overflow_remedy(),
                    Some(ContextOverflowRemedy::ResendShorter)
                );
                let TurnHostOutcome::ContextCapacity { recovery, .. } =
                    terminal_turn_outcome(&result)
                else {
                    panic!("overflow must not become a generic failure");
                };
                assert_eq!(recovery, ContextCapacityRecovery::ResendShorter);
                session.release().await.unwrap();
            })
            .unwrap();
    }

    /// The peak is populated on a route that reports usage only at the end.
    ///
    /// 2026-09-18, live attempt 3: `peak_prepared_input_tokens` was null on
    /// every position on every route. The peak was read only from a mid-stream
    /// `ModelEvent::Usage`, and several routes never emit one — they report an
    /// operation's counts on its terminal response, which never reaches a
    /// Content view the host could observe. The growth loopback has exactly
    /// that shape, and this drives it through a real multi-operation turn.
    #[test]
    fn a_terminal_only_usage_route_still_reports_a_per_operation_peak() {
        let root = tempfile::tempdir().unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let usage = executor
            .block_on(async {
                use plakous::sessions::{
                    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
                    SessionOperationId,
                };
                use plakous_offline_crash_harness::{
                    BoundaryController, placeholder_runtime_binding,
                };
                let workspace = root.path().join("workspace");
                std::fs::create_dir_all(&workspace).unwrap();
                let controller = BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.path().join("peak-events.jsonl"),
                )
                .unwrap();
                let sessions = AgentSessions::new(
                    SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
                )
                .unwrap();
                let CreateSessionOutcome::Created { session: live, .. } = sessions
                    .create(CreateSessionRequest::new(
                        SessionOperationId::new("peak:create").unwrap(),
                        placeholder_runtime_binding(&workspace, controller).unwrap(),
                    ))
                    .await
                    .unwrap()
                else {
                    panic!("new session")
                };
                let result = live
                    .agent()
                    .start(
                        plakous::agent::CommandId::new("peak:turn").unwrap(),
                        "read the big file",
                    )
                    .await
                    .unwrap()
                    .into_turn()
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                let usage = result.usage();
                live.release().await.unwrap();
                usage
            })
            .unwrap();
        assert!(
            usage.model_operations_reporting_usage() > 1,
            "a multi-operation turn is what makes the peak distinct from the sum"
        );
        let peak = usage
            .peak_input_tokens()
            .expect("a turn with reporting operations must publish a peak");
        assert!(peak > 0, "the peak must not be null on this route");
        assert!(
            peak < usage.model().input_tokens(),
            "the peak is one operation's prompt, never the sum: {peak} vs {}",
            usage.model().input_tokens()
        );
    }

    /// A budget-exhausted completion reaches the host as the shape the
    /// reasoning-only branch keys on.
    ///
    /// Drives the offline loopback provider end to end: empty visible text, no
    /// tool call, `FinishReason::Length`, and provider-reported counts showing
    /// the whole output bound spent on reasoning. This shape can prevent
    /// a task from completing.
    #[test]
    fn a_budget_exhausted_completion_arrives_as_empty_toolless_and_length() {
        let root = tempfile::tempdir().unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let result = executor
            .block_on(async {
                use plakous::sessions::{
                    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
                    SessionOperationId,
                };
                use plakous_offline_crash_harness::{
                    BoundaryController, text_runtime_binding_with_finish,
                };
                let workspace = root.path().join("workspace");
                std::fs::create_dir_all(&workspace).unwrap();
                let controller = BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.path().join("reasoning-only-events.jsonl"),
                )
                .unwrap();
                let sessions = AgentSessions::new(
                    SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
                )
                .unwrap();
                let CreateSessionOutcome::Created { session: live, .. } = sessions
                    .create(CreateSessionRequest::new(
                        SessionOperationId::new("reasoning-only:create").unwrap(),
                        // Empty prefix and empty terminal: the model thinks and
                        // emits nothing visible.
                        text_runtime_binding_with_finish(
                            &workspace,
                            controller.clone(),
                            "",
                            "",
                            plakous::model::FinishReason::Length,
                        )
                        .unwrap(),
                    ))
                    .await
                    .unwrap()
                else {
                    panic!("new session")
                };
                let result = live
                    .agent()
                    .start(
                        plakous::agent::CommandId::new("reasoning-only:turn").unwrap(),
                        "think about it",
                    )
                    .await
                    .unwrap()
                    .into_turn()
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                let outcome = terminal_turn_outcome(&result);
                live.release().await.unwrap();
                outcome
            })
            .unwrap();
        let TurnHostOutcome::Completed {
            assistant,
            finish_reason,
            reasoning_tokens,
            output_tokens,
            tool_calls,
            ..
        } = result
        else {
            panic!("expected a completed turn");
        };
        assert!(assistant.is_empty(), "no visible text: {assistant:?}");
        assert_eq!(tool_calls, 0);
        assert_eq!(finish_reason, plakous::model::FinishReason::Length);
        assert_eq!(reasoning_tokens, 4_095);
        assert_eq!(output_tokens, 4_096);
        assert!(
            spent_budget_on_reasoning(assistant.is_empty(), tool_calls, finish_reason),
            "the host must classify this as budget-exhausted, not an empty stream"
        );
    }
}
