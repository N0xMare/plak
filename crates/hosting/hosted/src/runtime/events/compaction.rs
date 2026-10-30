//! Compaction, observation clipping and replacement events.

use super::notes::{observation_clipped_note, observation_replaced_record};
use super::tools::observe_tool_activity;
use super::{EventApplier, timestamp_ms};
use crate::runtime::logging::log_event;
use crate::runtime::turn::flush_assistant_delta;
use crate::runtime::{
    COMPACT_COMMITTED_DETAIL, COMPACT_FAILED_DETAIL, COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
    COMPACT_NOT_NEEDED_DETAIL, COMPACT_PROVIDER_QUIET_NOTE, COMPACT_RECOVERY_REQUIRED_DETAIL,
    COMPACT_UNAVAILABLE_DETAIL, COMPACTING_ACTIVITY_LABEL, Sender,
};
use crate::session_log::{SessionEvent, redact_text_preview};
use crate::sessions;
use crate::sessions::{NoticeScope, SessionRecord, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous::agent::{
    AgentEventKind, CompactionEventOutcome, CompactionEventReason, CompactionRung,
    CompactionSummary,
};

/// Occupancy figures on a Compact row (`199.7k`, `400`).
///
/// Same quantity as the composer meter and Context header: the Plakous
/// context measure. Prepared-input stays on the Context tab as its own
/// labelled fact.
fn format_compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        let tenths = tokens.saturating_add(50) / 100;
        format!("{}.{}k", tenths / 10, tenths % 10)
    } else {
        tokens.to_string()
    }
}

/// Occupancy for Compact chrome when both sides exist. Prepared-input only
/// when the byte split was not measured (offline loopback).
pub(in crate::runtime) fn compact_row_token_pair(
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

pub(in crate::runtime) const fn compact_terminal_state(
    compacted: bool,
    successor_fits: bool,
) -> &'static str {
    if compacted && successor_fits {
        "done"
    } else if compacted {
        "overflow"
    } else {
        "failed"
    }
}

pub(in crate::runtime) fn publish_compact_activity<P: plakous_host_protocol::ProtocolExtension>(
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
    let _ = events.send(ContentEvent::ToolLine {
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
fn compact_finished_record(
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
const fn compaction_rung_label(rung: CompactionRung) -> &'static str {
    match rung {
        CompactionRung::NativeFacet => "native_facet",
        CompactionRung::SemanticFold => "semantic_fold",
        CompactionRung::ToolResultPlaceholders => "tool_result_placeholders",
        CompactionRung::ReplayOmission => "replay_omission",
        CompactionRung::Residency => "residency",
    }
}

/// The operator clause naming what a committed compaction did.
///
/// "compact workflow committed" tells a reader nothing about whether it bought
/// anything. The figures do.
pub(in crate::runtime) fn compaction_summary_clause(summary: CompactionSummary) -> String {
    let unit = if summary.rung == CompactionRung::Residency {
        "segment(s)"
    } else {
        "exchange(s)"
    };
    format!(
        "compaction {} {} {unit}: {} -> {} prepared input tokens",
        summary.rung.verb(),
        summary.segments,
        summary.input_tokens_before,
        summary.input_tokens_after
    )
}

/// The operator-facing note for a compaction terminal.
fn compact_finished_note(
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

impl<P: plakous_host_protocol::ProtocolExtension> EventApplier<'_, P> {
    /// A tool observation clipped to its bound.
    pub(super) fn observation_clipped(self) {
        let Self {
            event,
            call_id_str,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::ObservationClipped {
            name,
            bytes_kept,
            bytes_dropped,
            bound,
        } = event.kind()
        else {
            return;
        };
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

    /// A tool observation replaced in history.
    pub(super) fn observation_replaced(self) {
        let Self {
            event,
            call_id_str,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::ObservationReplaced {
            bytes_before,
            bytes_after,
        } = event.kind()
        else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        log_event(
            log,
            observed_session_id,
            observation_replaced_record(call_id_str.as_deref(), *bytes_before, *bytes_after),
        );
    }

    /// Tool results replaced to free context.
    pub(super) fn tool_results_replaced(self) {
        let Self {
            event,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::ToolResultsReplaced {
            results,
            bytes,
            tokens_before,
            tokens_after,
        } = event.kind()
        else {
            return;
        };
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

    /// A compaction started.
    pub(super) fn compaction_started(self) {
        let Self {
            event,
            session,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::CompactionStarted {
            workflow_id,
            reason,
        } = event.kind()
        else {
            return;
        };
        let reason_label = match reason {
            CompactionEventReason::Manual => "manual",
            CompactionEventReason::Scheduled => "scheduled",
            CompactionEventReason::Residency => "residency",
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
        let _ = events.send(ContentEvent::Activity {
            label: COMPACTING_ACTIVITY_LABEL.into(),
            active: true,
        });
        let call_id = sessions::projector::compaction_call_id(workflow_id.as_bytes());
        publish_compact_activity(session, events, &call_id, "running", None, None);
    }

    /// A compaction finished.
    pub(super) fn compaction_finished(self) {
        let Self {
            event,
            session,
            events,
            delta_buf,
            last_flush,
            log,
            observed_session_id,
            compacted_mid_turn,
            ..
        } = self;
        let AgentEventKind::CompactionFinished {
            workflow_id,
            outcome,
            elapsed_ms,
            summary,
            detail: outcome_detail,
        } = event.kind()
        else {
            return;
        };
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
        let _ = events.send(ContentEvent::Activity {
            label: COMPACTING_ACTIVITY_LABEL.into(),
            active: false,
        });
        let quiet = outcome_detail.is_some_and(|detail| {
            detail.contains("deadline elapsed") || detail.contains("no authoritative completion")
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
            publish_compact_activity(session, events, &call_id, "failed", None, Some(*elapsed_ms));
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
            publish_compact_activity(session, events, &call_id, state, subject, Some(*elapsed_ms));
        }
    }
}

/// The one distinct operator text per fence-driven compaction outcome (spec 02 §5.6).
const fn compaction_event_outcome_detail(outcome: CompactionEventOutcome) -> &'static str {
    match outcome {
        CompactionEventOutcome::Committed => COMPACT_COMMITTED_DETAIL,
        CompactionEventOutcome::NotNeeded => COMPACT_NOT_NEEDED_DETAIL,
        CompactionEventOutcome::NoEligibleRange => COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
        CompactionEventOutcome::Unavailable => COMPACT_UNAVAILABLE_DETAIL,
        CompactionEventOutcome::Failed => COMPACT_FAILED_DETAIL,
        CompactionEventOutcome::RecoveryRequired => COMPACT_RECOVERY_REQUIRED_DETAIL,
    }
}

#[cfg(test)]
mod tests;
