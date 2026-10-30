//! What a compact pass shows: its activity row, notes, views and meter.

use crate::agent::AttachedCodingSession;
use crate::runtime::events::publish_compact_activity;
use crate::runtime::logging::log_ev;
use crate::runtime::usage::{emit_context_projection, last_enrolled_source_bytes, query_context};
use crate::runtime::{
    COMPACT_COMMITTED_DETAIL, COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL,
    COMPACT_PROVIDER_QUIET_NOTE, CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE,
    CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE, CONTEXT_CAPACITY_PROTECTED_GUIDANCE,
    CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE, CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE, HostConfig,
    Sender,
};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions;
use crate::sessions::TranscriptRow;
use crate::surface_protocol::ContentEvent;
use plakous::application::ApplicationExecutor;
use plakous::sessions::ContextQuery;
use plakous_host_protocol::status::HostPhase;

/// Publishes the §5.4 activity for a compaction, and clears it.
///
/// A compaction — manual `/compact` or the capacity-triggered scheduled one — is one
/// paid round-trip the operator waits on, and the host published nothing
/// while it ran: the HUD stayed empty for the whole dispatch and
/// the TUI's `Compacting…` branch was unreachable. The
/// row has zero height unless the host says it is working, so busy is set
/// with the label and cleared on every exit arm — including the refusals
/// `handle_compact` returns.
pub(in crate::runtime) fn observe_compact<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    queue_len: usize,
    running: bool,
) {
    if running {
        let _ = events.send(ContentEvent::SetBusy(true));
        let _ = events.send(ContentEvent::Status {
            phase: HostPhase::Compacting,
            queue_len,
            detail: None,
        });
    } else {
        let _ = events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len,
            detail: None,
        });
        let _ = events.send(ContentEvent::SetBusy(false));
    }
}

pub(super) fn refresh_compact_views<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    config: &mut HostConfig<P>,
    log: Option<&SessionLog>,
    operation: &str,
    call_id: &str,
    unadmitted: bool,
) -> Option<TranscriptRow> {
    let views = executor
        .block_on(super::super::projection::project_to_tail(
            opened.sessions(),
            &config.store,
            &config.session.id,
            opened.locator(),
        ))
        .unwrap_or_else(|_| Err("application execution failed".to_owned().into()))
        .map_err(String::from);
    let settled_row = views.as_ref().ok().and_then(|views| {
        views.rows.values().find(|row| matches!(row,
            TranscriptRow::Tool(tool) if tool.call_id == call_id && tool.finished_at_ms.is_some()
        )).cloned()
    });
    apply_compact_views(config, log, operation, unadmitted, views);
    settled_row
}

fn apply_compact_views<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    log: Option<&SessionLog>,
    operation: &str,
    unadmitted: bool,
    views: Result<sessions::projector::SessionViews, String>,
) {
    let result = views.and_then(|views| {
        config.session.root_turn_count = views.state.counters.root_turns;
        config.session.tool_call_count = views.state.counters.tool_calls;
        config.session.compaction_count = views.state.counters.compactions;
        if unadmitted {
            config
                .store
                .discard_unadmitted_workflow(&mut config.session, operation)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        log_ev(
            log,
            config,
            SessionEvent::new("projector.deferred").str("error", &error),
        );
    }
}

pub(super) fn publish_compact_row<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    events: &Sender<P>,
    call_id: &str,
    state: &str,
    subject: Option<String>,
    duration_ms: Option<u64>,
) {
    publish_compact_activity(
        &mut config.session,
        events,
        call_id,
        state,
        subject,
        duration_ms,
    );
}

pub(super) const fn compact_committed_detail(successor_fits: bool) -> &'static str {
    if successor_fits {
        COMPACT_COMMITTED_DETAIL
    } else {
        COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL
    }
}

fn compact_operator_detail(base: &str, diagnostic: Option<&str>) -> String {
    match diagnostic {
        Some(text) if !text.is_empty() => format!("{base}: {text}"),
        _ => base.to_owned(),
    }
}

/// Transcript copy: never dump a Responses deadline diagnostic into the TUI.
pub(super) fn compact_operator_note(base: &str, diagnostic: Option<&str>) -> String {
    if diagnostic.is_some_and(compact_diagnostic_is_provider_quiet) {
        COMPACT_PROVIDER_QUIET_NOTE.to_owned()
    } else {
        compact_operator_detail(base, diagnostic)
    }
}

/// Core wording for a paid fold that measured no shrink. The session log keeps
/// this diagnostic; the transcript uses `manual_compact_operator_text`.
const COMPACT_SHRINK_REFUSAL_DIAGNOSTIC: &str =
    "compaction successor did not prove net input shrink";

const COMPACT_SHRINK_REFUSAL_NOTE: &str =
    "nothing was compacted: the summary would not be smaller than the conversation it replaces";

/// Operator sentence for a manual compact note. A shrink refusal is told in
/// plain words; every other detail is unchanged. The session log keeps the
/// diagnostic string.
pub(super) fn manual_compact_operator_text(detail: &str) -> String {
    if detail.contains(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC) {
        COMPACT_SHRINK_REFUSAL_NOTE.to_owned()
    } else {
        detail.to_owned()
    }
}

fn compact_diagnostic_is_provider_quiet(diagnostic: &str) -> bool {
    diagnostic.contains("deadline elapsed") || diagnostic.contains("no authoritative completion")
}

pub(super) fn unanchored_occupancy(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
) -> Option<u64> {
    query_context(executor, opened, ContextQuery::Current)
        .ok()?
        .unanchored_occupied_tokens()
}

pub(super) fn emit_compact_meter<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    config: &HostConfig<P>,
    events: &Sender<P>,
) {
    if let Ok(projection) = query_context(executor, opened, ContextQuery::Current) {
        emit_context_projection(
            events,
            &projection,
            config.session.context_counters(),
            last_enrolled_source_bytes(opened),
        );
    }
}

/// `segment_limit`: the attached head holds at least the segment count at
/// which Plakous refuses the next round (chapter 01 §10.3).
pub(in crate::runtime) const fn context_capacity_recovery_guidance(
    recovery: plakous_host_protocol::ContextCapacityRecovery,
    exact_source: bool,
    native_compact_facet: bool,
    segment_limit: bool,
) -> &'static str {
    use plakous_host_protocol::ContextCapacityRecovery;
    match recovery {
        ContextCapacityRecovery::ResendShorter => {
            "prompt withheld at the context limit; shorten the restored draft and resend explicitly"
        }
        ContextCapacityRecovery::Protected => CONTEXT_CAPACITY_PROTECTED_GUIDANCE,
        ContextCapacityRecovery::Compactable if !exact_source => {
            CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE
        }
        ContextCapacityRecovery::Compactable if segment_limit => {
            CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE
        }
        ContextCapacityRecovery::Compactable if native_compact_facet => {
            CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE
        }
        ContextCapacityRecovery::Compactable => CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE,
    }
}

#[cfg(test)]
mod tests;
