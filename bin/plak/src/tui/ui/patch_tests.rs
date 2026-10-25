#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::collections::BTreeMap;

use super::App;
use super::test_support::{launch_basic, pin_clock, restore_snapshot, serialize_frame, tool_item};
use crate::tui::{HostEvent, UserAction, user_action_channel};
use plakous_host_protocol::views::presentation::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptRow, TranscriptScope,
};
use ratatui::{Terminal, backend::TestBackend};

fn snapshot(revision: u64, rows: &BTreeMap<RowKey, TranscriptRow>) -> PresentationSnapshot {
    PresentationSnapshot {
        revision,
        items: rows
            .iter()
            .map(|(key, row)| TranscriptItem {
                key: *key,
                row: row.clone(),
            })
            .collect(),
        ..PresentationSnapshot::default()
    }
}

fn paint(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    serialize_frame(terminal.backend().buffer())
}

#[test]
fn keyed_patch_frames_match_full_restore_without_resetting_tool_expansion() {
    let mut patched = App::new(launch_basic(), true);
    let mut restored = App::new(launch_basic(), true);
    pin_clock(&mut patched);
    pin_clock(&mut restored);
    patched.render_prefs.reduced_motion = true;
    restored.render_prefs.reduced_motion = true;
    let user = RowKey::new(2, Lane::Runtime, 0);
    let call = RowKey::new(4, Lane::Runtime, 0);
    let answer = RowKey::new(8, Lane::Runtime, 0);
    let mut tool = tool_item("call-1", "read_file");
    tool.detail = Some("source text".into());
    let before = BTreeMap::from([
        (user, TranscriptRow::user("read the source")),
        (call, TranscriptRow::Tool(tool.clone())),
    ]);
    patched.apply_host_event(HostEvent::PresentationSnapshot(snapshot(1, &before)));
    patched.toggle_tool_expand_at(1);
    tool.detail = Some("updated source text".into());
    let after = BTreeMap::from([
        (user, TranscriptRow::user("read the source")),
        (call, TranscriptRow::Tool(tool)),
        (answer, TranscriptRow::assistant("finished")),
    ]);
    patched.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch::between(
        TranscriptScope::Root,
        1,
        2,
        &before,
        &after,
    )));
    restored.apply_host_event(HostEvent::PresentationSnapshot(snapshot(2, &after)));
    restored.toggle_tool_expand_at(1);
    assert_eq!(paint(&mut patched), paint(&mut restored));
    let final_rows = BTreeMap::from([(answer, TranscriptRow::assistant("finished"))]);
    patched.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch::between(
        TranscriptScope::Root,
        2,
        3,
        &after,
        &final_rows,
    )));
    restored.apply_host_event(HostEvent::PresentationSnapshot(snapshot(3, &final_rows)));
    assert_eq!(paint(&mut patched), paint(&mut restored));
}

#[test]
fn a_revision_gap_keeps_the_frame_and_requests_one_resync_until_replaced() {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(restore_snapshot(7, vec![TranscriptRow::assistant("kept")]));
    let before = paint(&mut app);
    let patch = TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 8,
        to_revision: 9,
        upserts: vec![TranscriptItem {
            key: RowKey::new(3, Lane::Runtime, 0),
            row: TranscriptRow::assistant("not applied"),
        }],
        deletes: Vec::new(),
    };
    let (tx, rx) = user_action_channel();
    for _ in 0..3 {
        app.apply_host_event(HostEvent::TranscriptPatch(patch.clone()));
        app.sync_agent_transcript(Some(&tx));
        assert_eq!(paint(&mut app), before);
    }
    assert!(matches!(
        rx.try_recv().unwrap().into_parts().0,
        UserAction::ResyncTranscript {
            scope: TranscriptScope::Root
        }
    ));
    assert!(rx.try_recv().is_err());
    app.apply_host_event(restore_snapshot(
        12,
        vec![TranscriptRow::assistant("resynced")],
    ));
    assert!(app.pane.resync.is_none());
    assert_eq!(app.pane.presentation.revision(), 12);
}

#[test]
fn an_empty_canonical_patch_reconciles_live_only_previews() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::assistant("committed")],
    ));
    app.apply_host_event(HostEvent::AssistantDelta("discarded preview".into()));
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 1,
        to_revision: 2,
        upserts: Vec::new(),
        deletes: Vec::new(),
    }));
    assert!(app.pane.blocks.iter().any(|block| matches!(block, crate::tui::blocks::ScrollBlock::Assistant(text) if text == "committed")));
    assert!(!app.pane.blocks.iter().any(|block| matches!(block, crate::tui::blocks::ScrollBlock::Assistant(text) if text.contains("discarded preview"))));
}

#[test]
fn root_patches_reach_the_hidden_root_without_touching_the_agent_pane() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::assistant("root before")],
    ));
    app.open_agent_view("c1".into(), "child".into());
    let child = app.pane.blocks.clone();
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 1,
        to_revision: 2,
        upserts: vec![TranscriptItem {
            key: RowKey::new(1, Lane::Runtime, 0),
            row: TranscriptRow::assistant("root after"),
        }],
        deletes: Vec::new(),
    }));
    assert_eq!(app.pane.blocks, child);
    app.close_agent_view();
    assert!(app.pane.blocks.iter().any(|block| matches!(block, crate::tui::blocks::ScrollBlock::Assistant(text) if text == "root after")));
}

#[test]
fn losing_the_last_patch_requests_resync_without_waiting_for_a_later_revision() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(1, vec![TranscriptRow::assistant("kept")]));
    let (events, receiver) = crate::tui::host_event_channel();
    for index in 0..plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY - 1 {
        events
            .send(HostEvent::JobStarted {
                job_id: format!("p{index}"),
                owner: "root".into(),
                command: "fixture".into(),
            })
            .unwrap();
    }
    events
        .send(HostEvent::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision: 1,
            to_revision: 2,
            upserts: Vec::new(),
            deletes: Vec::new(),
        }))
        .unwrap();
    events.send(HostEvent::SetBusy(false)).unwrap();
    let mut connected = true;
    super::terminal::drain_host_events(&mut app, &receiver, &mut connected, 64);
    let (actions, intents) = user_action_channel();
    app.sync_agent_transcript(Some(&actions));
    assert!(matches!(
        intents.try_recv().unwrap().into_parts().0,
        UserAction::ResyncTranscript {
            scope: TranscriptScope::Root
        }
    ));
    assert_eq!(app.pane.presentation.revision(), 1);
}
