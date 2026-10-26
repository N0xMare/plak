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
fn a_mid_turn_patch_keeps_pending_approval_and_local_notices_in_place() {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.busy = true;
    app.render_prefs.group_tool_calls = false;
    let first = RowKey::new(2, Lane::Runtime, 0);
    let second = RowKey::new(4, Lane::Runtime, 0);
    let mut tool = tool_item("parallel", "read_file");
    tool.state = "running".into();
    let before = BTreeMap::from([
        (
            first,
            TranscriptRow::Tool(tool_item("approval", "exec_command")),
        ),
        (second, TranscriptRow::Tool(tool.clone())),
    ]);
    app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(1, &before)));
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "decision".into(),
        call_id: Some("approval".into()),
        tool_name: "exec_command".into(),
        subject: Some("operator decision".into()),
        prompt: None,
        expires_at_ms: u64::MAX,
    });
    app.push_root_block(super::ScrollBlock::Error("local reload diagnostic".into()));
    app.push_root_block(super::ScrollBlock::System("local attachment notice".into()));
    let local = app.pane.blocks[2..].to_vec();
    tool.state = "done".into();
    let after = BTreeMap::from([
        (first, before[&first].clone()),
        (second, TranscriptRow::Tool(tool)),
    ]);
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch::between(
        TranscriptScope::Root,
        1,
        2,
        &before,
        &after,
    )));
    assert_eq!(&app.pane.blocks[2..], local.as_slice());
    assert!(app.open_approval.is_some());
    let frame = paint(&mut app).replace('·', " ");
    assert!(frame.contains("allow") && frame.contains("deny"), "{frame}");
    assert!(
        frame.contains("local reload diagnostic") && frame.contains("local attachment notice"),
        "{frame}"
    );
    // A busy resync must not lose the same local blocks either.
    app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(3, &after)));
    assert_eq!(&app.pane.blocks[2..], local.as_slice());
}

#[test]
fn a_settling_key_moves_its_block_and_local_followers_without_losing_focus() {
    for (old, answer, expected_tool) in [(1_u64 << 63, 8, 0), (2, 4, 1)] {
        let mut app = App::new(launch_basic(), true);
        pin_clock(&mut app);
        let preview = RowKey::new(old, Lane::Runtime, 0);
        let committed = RowKey::new(5, Lane::Runtime, 0);
        let answer = RowKey::new(answer, Lane::Runtime, 0);
        let tool = TranscriptRow::Tool(tool_item("moving", "read_file"));
        let before = BTreeMap::from([
            (preview, tool.clone()),
            (answer, TranscriptRow::assistant("answer")),
        ]);
        app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(1, &before)));
        let old_tool = usize::from(old > answer.anchor);
        app.pane.blocks.insert(
            old_tool + 1,
            super::ScrollBlock::System("local follower".into()),
        );
        app.pane.keyed.splice(old_tool + 1, 0, 1);
        app.mark_transcript_replaced();
        app.toggle_tool_expand_at(old_tool);
        let after = BTreeMap::from([
            (committed, tool),
            (answer, TranscriptRow::assistant("answer")),
        ]);
        app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch::between(
            TranscriptScope::Root,
            1,
            2,
            &before,
            &after,
        )));
        assert_eq!(app.pane.focused_block, Some(expected_tool));
        assert!(matches!(
            &app.pane.blocks[expected_tool],
            super::ScrollBlock::Tool { manual: true, .. }
        ));
        assert!(
            matches!(&app.pane.blocks[expected_tool + 1], super::ScrollBlock::System(text) if text == "local follower")
        );
        assert_eq!(app.pane.blocks.len(), 3);
    }
}

#[test]
fn patch_block_work_depends_only_on_changed_keys_for_root_and_child() {
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        for count in [0_u64, 2_000, 8_000, 10_000] {
            let mut app = App::new(launch_basic(), true);
            pin_clock(&mut app);
            if matches!(scope, TranscriptScope::Agent(_)) {
                app.open_agent_view("c1".into(), "child".into());
            }
            let key = RowKey::new(count + 1, Lane::Runtime, 0);
            let mut tool = tool_item("changed", "read_file");
            tool.state = "running".into();
            let mut rows: BTreeMap<_, _> = (1..=count)
                .map(|anchor| {
                    (
                        RowKey::new(anchor, Lane::Runtime, 0),
                        TranscriptRow::assistant("untouched"),
                    )
                })
                .collect();
            rows.insert(key, TranscriptRow::Tool(tool.clone()));
            let mut baseline = snapshot(1, &rows);
            baseline.scope = scope.clone();
            app.apply_host_event(HostEvent::PresentationSnapshot(baseline));
            app.push_block(super::ScrollBlock::System("local notice".into()));
            let tool_index = app.pane.keyed.get(key).unwrap();
            if count == 10_000 {
                assert_eq!(app.pane.blocks.len(), super::MAX_LIVE_TRANSCRIPT_BLOCKS);
            }
            app.pane.pending_transcript_change = super::TranscriptChange::Unchanged;
            let work = app.pane.keyed_block_work;
            let rebuilds = app.pane.keyed_rebuilds;
            let index_work = app.pane.keyed.work();
            tool.state = "done".into();
            app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
                scope: scope.clone(),
                from_revision: 1,
                to_revision: 2,
                upserts: vec![TranscriptItem {
                    key,
                    row: TranscriptRow::Tool(tool),
                }],
                deletes: Vec::new(),
            }));
            assert_eq!(
                app.pane.keyed_block_work - work,
                1,
                "history={count} scope={scope:?}"
            );
            assert_eq!(app.pane.keyed_rebuilds, rebuilds);
            assert!(
                app.pane.keyed.work() - index_work <= 6,
                "history={count} scope={scope:?}"
            );
            assert_eq!(
                app.pane.pending_transcript_change,
                super::TranscriptChange::Block(tool_index)
            );
            assert!(
                matches!(app.pane.blocks.last(), Some(super::ScrollBlock::System(text)) if text == "local notice")
            );
            assert_tail_index_work(&mut app, &scope, count, key);
        }
    }
}

fn assert_tail_index_work(app: &mut App, scope: &TranscriptScope, count: u64, key: RowKey) {
    let index_work = app.pane.keyed.work();
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: scope.clone(),
        from_revision: 2,
        to_revision: 3,
        upserts: vec![TranscriptItem {
            key: RowKey::new(count + 2, Lane::Runtime, 0),
            row: TranscriptRow::assistant("answer"),
        }],
        deletes: Vec::new(),
    }));
    assert!(
        app.pane.keyed.work() - index_work <= if count == 10_000 { 24 } else { 8 },
        "tail append history={count} scope={scope:?}"
    );
    let index_work = app.pane.keyed.work();
    let moved = RowKey::new(count + 3, Lane::Runtime, 0);
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: scope.clone(),
        from_revision: 3,
        to_revision: 4,
        upserts: vec![TranscriptItem {
            key: moved,
            row: TranscriptRow::Tool(tool_item("changed", "read_file")),
        }],
        deletes: vec![key],
    }));
    assert!(
        app.pane.keyed.work() - index_work <= 20,
        "key move history={count} scope={scope:?}"
    );
    let index_work = app.pane.keyed.work();
    let local = app.pane.blocks.len() - 1;
    assert_eq!(app.pane.keyed.local_anchor(local), Some(moved));
    assert_eq!(app.pane.keyed.group_end(local - 1, local + 1), local + 1);
    assert_eq!(app.pane.keyed.work() - index_work, 3);
}

#[test]
fn live_and_keyed_tools_share_lifecycle_labels_and_terminal_elapsed() {
    for state in [
        "start",
        "running",
        "done",
        "succeeded",
        "failed",
        "failed:exit 1",
        "rejected:Rule",
        "cancelled",
        "cancelled after 1 of 2",
        "overflow",
        "interrupted",
    ] {
        let mut tool = tool_item("same-tool", "read_file");
        tool.state = state.into();
        tool.subject = Some("src/main.rs".into());
        tool.detail = Some("fixture detail".into());
        tool.started_at_ms = Some(1_000);
        let running = super::tool_is_running(state);
        tool.finished_at_ms = (!running).then_some(2_200);
        let mut live = App::new(launch_basic(), true);
        pin_clock(&mut live);
        live.apply_host_event(HostEvent::ToolLine {
            call_id: Some(tool.call_id.clone()),
            parent_call_id: None,
            cell_id: None,
            kind: tool.kind,
            name: tool.name.clone(),
            state: tool.state.clone(),
            subject: tool.subject.clone(),
            detail: tool.detail.clone(),
            duration_ms: (!running).then_some(1_200),
            started_at_ms: tool.started_at_ms,
            finished_at_ms: tool.finished_at_ms,
            wait: None,
        });
        let expected = live.pane.blocks.clone();
        for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
            let mut keyed = App::new(launch_basic(), true);
            pin_clock(&mut keyed);
            if matches!(scope, TranscriptScope::Agent(_)) {
                keyed.open_agent_view("c1".into(), "child".into());
            }
            let mut baseline = snapshot(
                1,
                &BTreeMap::from([(
                    RowKey::new(1, Lane::Runtime, 0),
                    TranscriptRow::Tool(tool.clone()),
                )]),
            );
            baseline.scope = scope.clone();
            keyed.apply_host_event(HostEvent::PresentationSnapshot(baseline));
            assert_eq!(keyed.pane.blocks, expected, "state={state} scope={scope:?}");
        }
        if !running {
            assert!(
                matches!(&expected[0], super::ScrollBlock::Tool { state, .. }
                if super::blocks::recorded_tool_duration(state) == Some("1.2s"))
            );
        }
    }
}

#[test]
fn raw_streaming_never_rebuilds_keyed_history_in_root_or_child_panes() {
    use plakous_host_protocol::views::transcript::TranscriptStreamDelta;
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        for count in [0, 2_000, 8_000] {
            let mut app = App::new(launch_basic(), true);
            pin_clock(&mut app);
            if matches!(scope, TranscriptScope::Agent(_)) {
                app.open_agent_view("c1".into(), "child".into());
            }
            let mut initial = restore_snapshot(
                1,
                (0..count)
                    .map(|_| TranscriptRow::assistant("history"))
                    .collect(),
            );
            if let HostEvent::PresentationSnapshot(snapshot) = &mut initial {
                snapshot.scope = scope.clone();
            }
            app.apply_host_event(initial);
            let rebuilds = app.pane.keyed_rebuilds;
            for _ in 0..100 {
                app.apply_host_event(match &scope {
                    TranscriptScope::Root => HostEvent::AssistantDelta("delta".into()),
                    TranscriptScope::Agent(_) => HostEvent::TranscriptStream {
                        scope: scope.clone(),
                        delta: TranscriptStreamDelta::Assistant("delta".into()),
                    },
                });
            }
            assert_eq!(
                app.pane.keyed_rebuilds, rebuilds,
                "per-delta history rebuilds must remain zero at {count} rows"
            );
            assert!(
                matches!(app.pane.blocks.last(), Some(super::ScrollBlock::Assistant(text)) if text == &"delta".repeat(100))
            );
        }
    }
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
    let mut newer = patch.clone();
    newer.from_revision = 10;
    newer.to_revision = 11;
    app.apply_host_event(HostEvent::TranscriptPatch(newer.clone()));
    app.sync_agent_transcript(Some(&tx));
    assert!(
        rx.try_recv().is_err(),
        "new evidence does not bypass the retry floor"
    );
    app.apply_host_event(HostEvent::TranscriptPatch(newer));
    app.sync_agent_transcript(Some(&tx));
    assert!(
        rx.try_recv().is_err(),
        "same evidence cannot repeatedly re-arm"
    );
    super::test_support::pass_time(&mut app, 1_000);
    app.sync_agent_transcript(Some(&tx));
    assert!(matches!(
        rx.try_recv().unwrap().into_parts().0,
        UserAction::ResyncTranscript { .. }
    ));
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
