//! Session-log emission and the presentation snapshot projection.

use std::time::Instant;

use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::TranscriptItem;

use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{self, TranscriptRow};

use super::HostConfig;
use super::resources::process_resources;

/// The muted first row of a session's presentation: its id and, for a
/// carried session, the source it was imported from. Derived from the record,
/// never stored in the bounded transcript, so a truncated transcript keeps it.
pub(super) fn session_header(session: &sessions::SessionRecord) -> TranscriptRow {
    TranscriptRow::SessionHeader {
        session_id: session.id.clone(),
        carried_from: session
            .carried_forward_from()
            .map(
                |(session_id, state_key)| plakous_host_protocol::views::transcript::CarriedFrom {
                    session_id: session_id.to_owned(),
                    state_key: state_key.to_owned(),
                },
            ),
    }
}

/// A session's restore snapshot: its header, then the restore window of its
/// transcript.
pub fn project_session_snapshot(session: &sessions::SessionRecord) -> PresentationSnapshot {
    let mut items = session.transcript.items();
    items.insert(
        0,
        TranscriptItem {
            key: sessions::projector::RowKey::TRUNCATION,
            row: session_header(session),
        },
    );
    project_keyed_snapshot(items)
}

pub(crate) fn project_keyed_snapshot(items: Vec<TranscriptItem>) -> PresentationSnapshot {
    use plakous_host_protocol::views::presentation::{
        MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, row_bytes,
    };
    let header = items
        .first()
        .filter(|item| matches!(item.row, TranscriptRow::SessionHeader { .. }))
        .cloned();
    let reserved = usize::from(header.is_some());
    let mut kept = Vec::new();
    let mut bytes = header.as_ref().map_or(0, |item| row_bytes(&item.row));
    for mut item in items.into_iter().rev() {
        if matches!(item.row, TranscriptRow::SessionHeader { .. }) {
            continue;
        }
        bytes = bytes.saturating_add(row_bytes(&item.row));
        if bytes > MAX_PRESENTATION_BYTES || kept.len() == MAX_SNAPSHOT_ROWS - reserved {
            break;
        }
        if let TranscriptRow::Tool(tool) = &mut item.row
            && crate::activity::is_process_start_alias(&tool.name)
        {
            crate::activity::PROCESS_ACTIVITY_LABEL.clone_into(&mut tool.name);
        }
        kept.push(item);
    }
    kept.reverse();
    if let Some(header) = header {
        kept.insert(0, header);
    }
    let retained_calls: std::collections::BTreeSet<String> = kept
        .iter()
        .filter_map(|item| item.row.call_id().map(str::to_owned))
        .collect();
    for item in &mut kept {
        if let TranscriptRow::Tool(tool) = &mut item.row
            && tool
                .parent_call_id
                .as_ref()
                .is_some_and(|parent| !retained_calls.contains(parent))
        {
            tool.parent_call_id = None;
            tool.cell_id = None;
        }
    }
    let mut snapshot = PresentationSnapshot::empty(0);
    snapshot.items = kept;
    snapshot
}

pub(super) fn log_event(log: Option<&SessionLog>, session_id: &str, event: SessionEvent) {
    if let Some(log) = log {
        let _ = log.append_for(session_id, event);
    }
}

pub(super) fn log_ev<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    mut event: SessionEvent,
) {
    if let Some(render) = config.event_renderer {
        render(&mut event);
    }
    log_event(log, &config.session.id, event);
}

pub(super) fn elapsed_ms_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(super) const fn compact_reason_label(reason: sessions::ProductCompactReason) -> &'static str {
    match reason {
        sessions::ProductCompactReason::Manual => "manual",
        sessions::ProductCompactReason::Scheduled => "scheduled",
        sessions::ProductCompactReason::CapacityRecovery => "capacity",
    }
}

pub(super) fn log_compact_started<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    reason: sessions::ProductCompactReason,
) {
    log_ev(
        log,
        config,
        SessionEvent::new("compact.started").str("reason", compact_reason_label(reason)),
    );
}

pub(super) fn log_compact_finished<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    started: Instant,
    outcome: &str,
) {
    log_compact_finished_with_detail(log, config, started, outcome, None, false);
}

/// `detail` is the diagnosable compact reason. The transcript may show a
/// plainer sentence for the same outcome; this field keeps the diagnostic,
/// including a shrink refusal. Without it a `recovery_required` compact
/// leaves no trace in the session log (2026-09-03 incident).
pub(super) fn log_compact_finished_with_detail<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    started: Instant,
    outcome: &str,
    detail: Option<&str>,
    retained_overflow: bool,
) {
    let mut event = SessionEvent::new("compact.finished")
        .u64("elapsed_ms", elapsed_ms_since(started))
        .str("outcome", outcome);
    if let Some(detail) = detail {
        event = event.str("detail", detail);
    }
    if retained_overflow {
        event.presentation =
            Some(crate::session_log::SessionEventPresentation::CompactCommittedOverflow);
    }
    log_ev(log, config, event);
}

pub(super) fn log_host_attached<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    attach_started: Instant,
) {
    config
        .state_roots
        .log_telemetry_unavailable_once(&config.session.id);
    log_ev(
        log,
        config,
        SessionEvent::new("host.attached")
            .str("account", config.session.account.as_deref().unwrap_or("-"))
            .str("model", &config.model_id)
            .bool("ok", true)
            .u64("elapsed_ms", elapsed_ms_since(attach_started)),
    );
}

pub(super) fn log_session_ready<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    attach_started: Instant,
) {
    log_ev(
        log,
        config,
        SessionEvent::new("session.ready")
            .str("id", &config.session.id)
            .u64("elapsed_ms", elapsed_ms_since(attach_started)),
    );
}

pub(super) fn log_host_ready<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    session_open_ms: u64,
    host_loop_ms: u64,
) {
    let presentation_ms = config.ready_path.presentation_ms;
    let resolve_ms = config.ready_path.resolve_ms;
    let ready_ms = presentation_ms
        .saturating_add(resolve_ms)
        .saturating_add(host_loop_ms);
    let mut event = SessionEvent::new("host.ready")
        .str("account", config.session.account.as_deref().unwrap_or("-"))
        .str("model", &config.model_id)
        .u64("ready_ms", ready_ms)
        .u64("presentation_ms", presentation_ms)
        .u64("resolve_ms", resolve_ms)
        .u64("session_open_ms", session_open_ms)
        .u64("host_loop_ms", host_loop_ms);
    if let Some(resources) = process_resources() {
        event = event
            .u64("rss_kb", resources.rss_kb)
            .u64("peak_rss_kb", resources.peak_rss_kb)
            .u64("threads", resources.threads)
            .u64("fds", resources.fds);
    }
    log_ev(log, config, event);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
