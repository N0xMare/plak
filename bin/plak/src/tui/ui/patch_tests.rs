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
    assert!(matches!(
        rx.try_recv().unwrap().into_parts().0,
        UserAction::ResyncTranscript {
            scope: TranscriptScope::Root
        }
    ));
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
fn a_repeated_busy_baseline_does_not_clear_the_live_preview() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::assistant("committed")],
    ));
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::AssistantDelta("in flight".into()));
    let before = app.pane.blocks.clone();
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::assistant("committed")],
    ));
    assert_eq!(app.pane.blocks, before);
    assert_eq!(app.pane.presentation.revision(), 1);
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

#[test]
fn agent_reasoning_keeps_its_live_clock_across_resync_and_seals_on_patch() {
    use crate::tui::blocks::ScrollBlock;
    use plakous_host_protocol::views::transcript::ThinkingRow;
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.open_agent_view("c1".into(), "child".into());
    app.apply_host_event(super::test_support::agent_snapshot("c1", 1, Vec::new()));
    let row = TranscriptRow::Thinking(ThinkingRow {
        location_key: "thought".into(),
        text: "still considering".into(),
        elapsed_ms: None,
    });
    app.apply_host_event(super::test_support::agent_patch(
        "c1",
        1,
        vec![(20, row.clone())],
    ));
    let started = app.pane.thinking_started["thought"];
    assert!(matches!(
        app.pane.blocks.as_slice(),
        [ScrollBlock::Thinking { sealed: false, .. }]
    ));
    let mut baseline = snapshot(3, app.pane.presentation.items());
    baseline.scope = TranscriptScope::Agent("c1".into());
    app.apply_host_event(HostEvent::PresentationSnapshot(baseline));
    assert_eq!(app.pane.thinking_started["thought"], started);
    assert!(matches!(
        app.pane.blocks.as_slice(),
        [ScrollBlock::Thinking { sealed: false, .. }]
    ));
    let TranscriptRow::Thinking(mut sealed) = row else {
        unreachable!()
    };
    sealed.elapsed_ms = Some(1200);
    app.apply_host_event(super::test_support::agent_patch(
        "c1",
        3,
        vec![(20, TranscriptRow::Thinking(sealed))],
    ));
    assert!(matches!(
        app.pane.blocks.as_slice(),
        [ScrollBlock::Thinking {
            sealed: true,
            elapsed_ms: Some(1200),
            ..
        }]
    ));
    assert!(app.pane.thinking_started.is_empty());
}

#[test]
fn expanded_live_tool_keeps_its_state_when_settlement_moves_its_key() {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.render_prefs.reduced_motion = true;
    let prompt = RowKey::new(2, Lane::Runtime, 0);
    let preview = RowKey::new(1_u64 << 63, Lane::Runtime, 0);
    let committed = RowKey::new(5, Lane::Runtime, 0);
    let mut tool = tool_item("stable-call", "read_file");
    tool.state = "running".into();
    tool.subject = Some("src/parser.rs".into());
    tool.detail = Some("the source text".into());
    let before = BTreeMap::from([
        (prompt, TranscriptRow::user("inspect the parser")),
        (preview, TranscriptRow::Tool(tool.clone())),
    ]);
    app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(1, &before)));
    app.toggle_tool_expand_at(1);
    tool.state = "succeeded".into();
    let after = BTreeMap::from([
        (prompt, TranscriptRow::user("inspect the parser")),
        (committed, TranscriptRow::Tool(tool)),
        (
            RowKey::new(8, Lane::Runtime, 0),
            TranscriptRow::assistant("The parser is correct."),
        ),
    ]);
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch::between(
        TranscriptScope::Root,
        1,
        2,
        &before,
        &after,
    )));
    let mut restored = App::new(launch_basic(), true);
    pin_clock(&mut restored);
    restored.render_prefs.reduced_motion = true;
    restored.apply_host_event(HostEvent::PresentationSnapshot(snapshot(2, &after)));
    restored.toggle_tool_expand_at(1);
    let actual = paint(&mut app);
    assert_eq!(actual, paint(&mut restored));
    let path = "tests/goldens/transcript-patch-settlement-100x30.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(path, &actual).unwrap();
    } else {
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}
