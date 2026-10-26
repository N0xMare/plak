//! Model stream events and diagnostics.

use super::EventApplier;
use super::notes::{
    OPERATION_BUDGET_EXHAUSTED_NOTE, output_bound_rederived_note, output_limit_retry_note,
};
use super::reasoning::{
    discard_response_reasoning, reasoning_location_key, seal_reasoning_block, settle_live_thinking,
    settle_model_response,
};
use crate::runtime::Sender;
use crate::runtime::admission::commit_session_update;
use crate::runtime::logging::log_event;
use crate::runtime::tools::{harvest_assistant_text, stash_tool_subjects_from_response};
use crate::runtime::turn::flush_assistant_delta;
use crate::runtime::usage::record_operation_prepared_input;
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous::agent::AgentEventKind;
use plakous::model::ModelEvent;

/// Log historical projections without creating transcript rows or acknowledgements.
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

fn project_live_usage<P: plakous_host_protocol::ProtocolExtension>(
    update: &plakous::model::UsageUpdate,
) -> ContentEvent<P> {
    let usage = update.usage();
    ContentEvent::TurnUsage {
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

impl<P: plakous_host_protocol::ProtocolExtension> EventApplier<'_, P> {
    /// A settled response seals its reasoning; a discardable one drops it.
    pub(super) fn response_settled(self) {
        let Self {
            event,
            session,
            events,
            assistant,
            delta_buf,
            last_flush,
            projection,
            ..
        } = self;
        let AgentEventKind::ResponseSettled { discardable } = event.kind() else {
            return;
        };
        if *discardable {
            discard_response_reasoning(projection, events);
        } else {
            settle_model_response(projection, session, events, delta_buf, last_flush);
            projection.settled_assistant_len = assistant.len();
        }
    }

    /// One model stream event: text and reasoning deltas, blocks, usage, retries.
    pub(super) fn model(self) {
        let Self {
            event,
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
            peak_prepared_input_tokens,
            ..
        } = self;
        let AgentEventKind::Model {
            event: Some(model_event),
            ..
        } = event.kind()
        else {
            return;
        };
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
                    let _ = events.send(ContentEvent::ReasoningDelta {
                        location_key: reasoning_location_key(delta.location()),
                        text,
                    });
                }
            }
            ModelEvent::ReasoningVisibilityBound(bound) => {
                if let Some(text) = projection.bind_reasoning_visibility(*bound) {
                    flush_assistant_delta(delta_buf, last_flush, events, true);
                    let _ = events.send(ContentEvent::ReasoningDelta {
                        location_key: reasoning_location_key(bound.location()),
                        text,
                    });
                }
            }
            ModelEvent::BlockCompleted(completed) => {
                seal_reasoning_block(projection, events, completed.location());
            }
            ModelEvent::Usage(update) => {
                record_operation_prepared_input(peak_prepared_input_tokens, session, update, log);
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
                        let _ = events.send(ContentEvent::AssistantDelta(harvested));
                        let _ = events.send(ContentEvent::AssistantDone);
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
                let _ = events.send(ContentEvent::ModelRetry(notice));
                let _ = commit_session_update(store, home, session, |candidate| {
                    candidate.record_notice(row.clone());
                    true
                });
                let _ = events.send(ContentEvent::Row(row));
            }
            // Diagnostics never authorize replay or change canonical history.
            ModelEvent::Diagnostic(diagnostic) => {
                flush_assistant_delta(delta_buf, last_flush, events, true);
                publish_model_diagnostic(diagnostic, log, &session.id, events);
            }
            _ => {
                flush_assistant_delta(delta_buf, last_flush, events, true);
            }
        }
    }

    /// A model request waiting for a slot.
    pub(super) fn model_request_queued(self) {
        let Self {
            event,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::ModelRequestQueued { slots } = event.kind() else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        log_event(
            log,
            observed_session_id,
            SessionEvent::new("model.request_queued").str("account", slots),
        );
        let _ = events.send(ContentEvent::ModelRequestQueued {
            slots: slots.clone(),
        });
    }

    /// The turn spent its model-operation budget.
    pub(super) fn operation_budget_exhausted(self) {
        let Self {
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
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

    /// A model request started: settle the previous response's thinking.
    pub(super) fn model_started(self) {
        let Self {
            session,
            events,
            projection,
            ..
        } = self;
        settle_live_thinking(projection, session, events);
    }

    /// A response cut at its output bound and retried.
    pub(super) fn output_limit_retry(self) {
        let Self {
            event,
            events,
            delta_buf,
            last_flush,
            projection,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::OutputLimitRetry { bound } = event.kind() else {
            return;
        };
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

    /// The output bound derived again from the model's limits.
    pub(super) fn output_bound_rederived(self) {
        let Self {
            event,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::OutputBoundRederived { previous, current } = event.kind() else {
            return;
        };
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
}

#[cfg(test)]
mod tests;
