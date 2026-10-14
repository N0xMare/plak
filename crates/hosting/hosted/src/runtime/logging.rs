//! Session-log emission and the presentation snapshot projection.

use std::time::Instant;

use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{self, TranscriptRow};

use super::resources::process_resources;
use super::{HostConfig, HostEvent};

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
pub(super) fn project_session_snapshot<P: plakous_host_protocol::ProtocolExtension>(
    session: &sessions::SessionRecord,
) -> HostEvent<P> {
    let HostEvent::PresentationSnapshot(mut snapshot) =
        project_presentation_snapshot::<P>(&session.transcript)
    else {
        unreachable!("restore projection always produces a snapshot")
    };
    if snapshot.rows.len() == plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS {
        snapshot.rows.remove(0);
    }
    snapshot.rows.insert(0, session_header(session));
    HostEvent::PresentationSnapshot(snapshot)
}

/// The restore snapshot: the newest suffix of the product transcript's typed
/// rows that fits the presentation bounds, oldest first. Rows are the ones the
/// host emitted live; only lineage to a tool row outside the window and the
/// process-start alias label are normalized, exactly as the live path does.
pub(super) fn project_presentation_snapshot<P: plakous_host_protocol::ProtocolExtension>(
    transcript: &[TranscriptRow],
) -> HostEvent<P> {
    use plakous_host_protocol::views::presentation::{
        MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, PresentationSnapshot, row_bytes,
    };

    let mut rows = Vec::new();
    let mut bytes = 0_usize;
    for row in transcript.iter().rev() {
        bytes = bytes.saturating_add(row_bytes(row));
        if bytes > MAX_PRESENTATION_BYTES || rows.len() == MAX_SNAPSHOT_ROWS {
            break;
        }
        let mut row = row.clone();
        if let TranscriptRow::Tool(tool) = &mut row
            && crate::activity::is_process_start_alias(&tool.name)
        {
            crate::activity::PROCESS_ACTIVITY_LABEL.clone_into(&mut tool.name);
        }
        rows.push(row);
    }
    rows.reverse();
    let retained_calls: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.call_id().map(str::to_owned))
        .collect();
    for row in &mut rows {
        if let TranscriptRow::Tool(tool) = row
            && tool
                .parent_call_id
                .as_ref()
                .is_some_and(|parent| !retained_calls.contains(parent))
        {
            // A composition parent outside the window cannot own a visible row.
            tool.parent_call_id = None;
            tool.cell_id = None;
        }
    }
    let mut snapshot = PresentationSnapshot::empty(0);
    snapshot.rows = rows;
    HostEvent::PresentationSnapshot(snapshot)
}

pub(super) fn log_event(log: Option<&SessionLog>, session_id: &str, event: SessionEvent) {
    if let Some(log) = log {
        let _ = log.append_for(session_id, event);
    }
}

pub(super) fn log_ev<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
    event: SessionEvent,
) {
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
    log_compact_finished_with_detail(log, config, started, outcome, None);
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
) {
    let mut event = SessionEvent::new("compact.finished")
        .u64("elapsed_ms", elapsed_ms_since(started))
        .str("outcome", outcome);
    if let Some(detail) = detail {
        event = event.str("detail", detail);
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
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;

    use crate::runtime::*;

    use crate::runtime::startup::*;

    use crate::sessions::TranscriptRow;
    use std::time::Instant;

    fn snapshot_rows(transcript: &[TranscriptRow]) -> Vec<TranscriptRow> {
        let HostEvent::PresentationSnapshot(snapshot) =
            project_presentation_snapshot::<TestExtension>(transcript)
        else {
            panic!("snapshot");
        };
        snapshot.validate().unwrap();
        snapshot.rows
    }

    fn tool(index: usize) -> sessions::ToolItem {
        sessions::ToolItem {
            call_id: format!("call-{index}"),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "succeeded".into(),
            subject: Some(format!("command-{index}")),
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            parent_call_id: (index == 1).then(|| "call-0".into()),
            cell_id: (index == 1).then(|| "cell".into()),
            wait: None,
        }
    }

    #[test]
    fn restore_snapshot_is_the_persisted_rows_in_order() {
        let transcript = vec![
            TranscriptRow::user("go"),
            TranscriptRow::steer("change course"),
            TranscriptRow::Tool(tool(7)),
            TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Turn,
                "note",
            ),
            TranscriptRow::Interrupted,
        ];
        assert_eq!(snapshot_rows(&transcript), transcript);
    }

    #[test]
    fn restored_rows_keep_an_ordered_bounded_suffix_and_detach_missing_parents() {
        use plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS;
        for count in [MAX_SNAPSHOT_ROWS, MAX_SNAPSHOT_ROWS + 1] {
            let transcript: Vec<_> = (0..count)
                .map(|index| TranscriptRow::Tool(tool(index)))
                .collect();
            let rows = snapshot_rows(&transcript);
            assert_eq!(rows.len(), MAX_SNAPSHOT_ROWS);
            let first = count - MAX_SNAPSHOT_ROWS;
            assert_eq!(rows[0].call_id(), Some(format!("call-{first}").as_str()));
            let child = rows[1 - first].as_tool().unwrap();
            assert_eq!(child.parent_call_id.is_some(), first == 0);
            assert_eq!(child.cell_id.is_some(), first == 0);
        }
    }

    #[test]
    fn restored_byte_window_keeps_the_newest_answer_without_mutating_source() {
        use plakous_host_protocol::views::presentation::MAX_PRESENTATION_BYTES;
        let transcript = vec![
            TranscriptRow::user("x".repeat(MAX_PRESENTATION_BYTES)),
            TranscriptRow::assistant("retained"),
        ];
        assert_eq!(
            snapshot_rows(&transcript),
            [TranscriptRow::assistant("retained")]
        );
    }

    #[test]
    fn restored_process_start_alias_takes_the_live_shell_label() {
        let mut start = tool(3);
        "process_session_start".clone_into(&mut start.name);
        let rows = snapshot_rows(&[TranscriptRow::Tool(start)]);
        assert_eq!(
            rows[0].as_tool().unwrap().name,
            crate::activity::PROCESS_ACTIVITY_LABEL
        );
    }

    #[test]
    fn lifecycle_milestones_name_cumulative_elapsed_time() {
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(event_tx);
        observe_phase(
            &events,
            HostLifecyclePhase::RebindResolveTarget,
            Instant::now(),
            0,
        );
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(
            event,
            HostEvent::Status { detail: Some(detail), .. }
                if detail.contains("cumulative elapsed")
        ));
    }
}
