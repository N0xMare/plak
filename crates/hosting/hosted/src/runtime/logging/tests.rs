use super::{TranscriptItem, project_keyed_snapshot};
use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;

use crate::runtime::*;

use crate::runtime::startup::*;

use crate::sessions::TranscriptRow;
use std::time::Instant;

fn snapshot_rows(transcript: &[TranscriptRow]) -> Vec<TranscriptRow> {
    let snapshot = project_keyed_snapshot(
        transcript
            .iter()
            .enumerate()
            .map(|(index, row)| TranscriptItem {
                key: sessions::projector::RowKey::new(
                    index as u64 + 1,
                    sessions::projector::Lane::Runtime,
                    0,
                ),
                row: row.clone(),
            })
            .collect(),
    );
    snapshot.validate().unwrap();
    snapshot.items.into_iter().map(|item| item.row).collect()
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
            if matches!(detail, plakous_host_protocol::status::HostStatusDetail::TransitionStage { stage: HostLifecyclePhase::RebindResolveTarget, .. })
    ));
}
