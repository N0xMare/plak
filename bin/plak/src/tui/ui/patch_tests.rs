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

fn scoped_app(scope: &TranscriptScope) -> App {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.render_prefs.group_tool_calls = false;
    app.render_prefs.reduced_motion = true;
    if let TranscriptScope::Agent(id) = scope {
        app.open_agent_view(id.clone(), "child".into());
    }
    app
}

fn scoped_snapshot(
    scope: &TranscriptScope,
    revision: u64,
    rows: &BTreeMap<RowKey, TranscriptRow>,
) -> HostEvent {
    let mut snapshot = snapshot(revision, rows);
    snapshot.scope = scope.clone();
    HostEvent::PresentationSnapshot(snapshot)
}

fn select_blocks(app: &mut App, first: usize, last: usize) {
    use super::ContentPosition;
    let _ = paint(app);
    let layout = app.pane.transcript_cache.layout();
    let row = |block| {
        layout
            .block_index
            .iter()
            .position(|at| *at == Some(block))
            .unwrap()
    };
    app.pane.selection.begin(
        ContentPosition {
            row: row(first),
            column: 0,
        },
        app.last_transcript_width,
        app.pane.transcript_revision,
    );
    app.pane.selection.drag(ContentPosition {
        row: row(last),
        column: 4,
    });
    app.pane.selection.capture_blocks(layout);
}

fn bind_session(app: &mut App, id: &str) {
    app.apply_host_event(HostEvent::Binding {
        session_id: Some(id.to_owned()),
        account: app.account.clone(),
        model: app.model.clone(),
        effort: app.effort.clone(),
        extra: app.model_extra.clone(),
        composition: app.composition,
        protocol: app.protocol,
        effective_presentation: app.effective_presentation.clone(),
    });
}

#[test]
fn resync_matches_duplicate_mail_by_key_and_keeps_its_focus_camera_and_selection() {
    use crate::tui::blocks::{RowTarget, ScrollBlock};
    use plakous_host_protocol::views::transcript::NoticeScope;
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let mut app = scoped_app(&scope);
        let first = RowKey::new(2, Lane::Runtime, 0);
        let separator = RowKey::new(3, Lane::Runtime, 0);
        let kept = RowKey::new(4, Lane::Runtime, 0);
        let mail = TranscriptRow::info(NoticeScope::Tree, "result · #c1 · repeated\nfull body");
        let before = BTreeMap::from([
            (first, mail.clone()),
            (separator, TranscriptRow::assistant("between notices")),
            (kept, mail.clone()),
        ]);
        app.apply_host_event(scoped_snapshot(&scope, 1, &before));
        app.apply_host_event(HostEvent::SetBusy(true));
        let first_at = app.pane.keyed.get(first).unwrap();
        let kept_at = app.pane.keyed.get(kept).unwrap();
        app.toggle_row(first_at, RowTarget::Item);
        select_blocks(&mut app, kept_at, kept_at);
        app.pane.focused_block = Some(kept_at);
        app.pane.transcript_view_anchor = Some((kept_at, 0));
        app.apply_host_event(scoped_snapshot(&scope, 2, &BTreeMap::from([(kept, mail)])));
        let kept_at = app.pane.keyed.get(kept).unwrap();
        assert!(matches!(
            app.pane.blocks[kept_at],
            ScrollBlock::Mail { open: false, .. }
        ));
        assert_eq!(app.pane.focused_block, Some(kept_at));
        assert_eq!(app.pane.transcript_view_anchor, Some((kept_at, 0)));
        let _ = paint(&mut app);
        assert!(app.pane.selection.has_range());
        let row = app
            .pane
            .transcript_cache
            .layout()
            .block_index
            .iter()
            .position(|at| *at == Some(kept_at))
            .unwrap();
        let (anchor, head) = app.pane.selection.endpoints_for_test().unwrap();
        assert_eq!((anchor.row, head.row), (row, row));
        assert!(app.take_transcript_selection_text().is_some());
    }
}

#[test]
fn busy_resync_moves_leading_local_notices_without_cloning_or_reversing_them() {
    use crate::tui::blocks::ScrollBlock;
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let mut app = scoped_app(&scope);
        app.apply_host_event(scoped_snapshot(&scope, 1, &BTreeMap::new()));
        app.apply_host_event(HostEvent::SetBusy(true));
        let first = "local A ".repeat(128);
        let second = "local B ".repeat(128);
        let allocations = [first.as_ptr(), second.as_ptr()];
        app.push_block(ScrollBlock::System(first));
        app.push_block(ScrollBlock::System(second));
        let second_at = app.pane.blocks.len() - 1;
        select_blocks(&mut app, second_at, second_at);
        app.pane.focused_block = Some(second_at);
        app.apply_host_event(scoped_snapshot(
            &scope,
            2,
            &BTreeMap::from([(
                RowKey::new(9, Lane::Runtime, 0),
                TranscriptRow::assistant("canonical row"),
            )]),
        ));
        let locals: Vec<_> = app
            .pane
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(at, block)| match block {
                ScrollBlock::System(text) if text.starts_with("local ") => Some((at, text)),
                _ => None,
            })
            .collect();
        assert_eq!(locals.len(), 2);
        assert!(locals[0].1.starts_with("local A "));
        assert!(locals[1].1.starts_with("local B "));
        for ((_, text), original) in locals.iter().zip(allocations) {
            assert!(
                std::ptr::eq(text.as_ptr(), original),
                "local payload cloned"
            );
        }
        assert_eq!(app.pane.focused_block, Some(locals[1].0));
        assert!(app.pane.selection.has_range());
    }
}

#[test]
fn opening_notes_move_into_the_attached_transcript_without_a_block_copy() {
    use crate::tui::blocks::ScrollBlock;
    let mut app = scoped_app(&TranscriptScope::Root);
    app.apply_host_event(scoped_snapshot(&TranscriptScope::Root, 1, &BTreeMap::new()));
    app.apply_host_event(HostEvent::SessionOpening);
    let text = "opening diagnostic ".repeat(128);
    let original = text.as_ptr();
    app.push_root_block(ScrollBlock::Error(text));
    app.apply_host_event(scoped_snapshot(
        &TranscriptScope::Root,
        2,
        &BTreeMap::from([(
            RowKey::new(2, Lane::Runtime, 0),
            TranscriptRow::assistant("attached"),
        )]),
    ));
    let diagnostic = app.pane.blocks.iter().find_map(|block| match block {
        ScrollBlock::Error(text) if text.starts_with("opening diagnostic ") => Some(text),
        _ => None,
    });
    assert!(std::ptr::eq(diagnostic.unwrap().as_ptr(), original));
}

#[test]
fn an_earlier_upsert_reflows_without_losing_a_later_unchanged_selection() {
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let mut app = scoped_app(&scope);
        let first = RowKey::new(2, Lane::Runtime, 0);
        let kept = RowKey::new(4, Lane::Runtime, 0);
        let before = BTreeMap::from([
            (first, TranscriptRow::assistant("before")),
            (kept, TranscriptRow::assistant("selected text")),
        ]);
        app.apply_host_event(scoped_snapshot(&scope, 1, &before));
        let kept_at = app.pane.keyed.get(kept).unwrap();
        select_blocks(&mut app, kept_at, kept_at);
        app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
            scope,
            from_revision: 1,
            to_revision: 2,
            upserts: vec![TranscriptItem {
                key: first,
                row: TranscriptRow::assistant("earlier expanded line\n".repeat(20)),
            }],
            deletes: Vec::new(),
        }));
        let _ = paint(&mut app);
        assert!(app.pane.selection.has_range());
        let row = app
            .pane
            .transcript_cache
            .layout()
            .block_index
            .iter()
            .position(|at| *at == Some(kept_at))
            .unwrap();
        let (anchor, head) = app.pane.selection.endpoints_for_test().unwrap();
        assert_eq!((anchor.row, head.row), (row, row));
        assert!(app.take_transcript_selection_text().is_some());
    }
}

#[test]
fn active_selection_patch_and_resync_measure_only_a_bounded_block_span() {
    use super::ContentPosition;
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        for count in [0_u64, 8_000, 10_000] {
            let mut app = scoped_app(&scope);
            let selected = RowKey::new(count + 2, Lane::Runtime, 0);
            let tool = RowKey::new(count + 3, Lane::Runtime, 0);
            let mut rows: BTreeMap<_, _> = (0..count.saturating_sub(2))
                .map(|n| {
                    (
                        RowKey::new(n + 1, Lane::Runtime, 0),
                        TranscriptRow::assistant(format!("history {n}\n\nmore prose")),
                    )
                })
                .collect();
            rows.insert(selected, TranscriptRow::assistant("selected text"));
            rows.insert(tool, TranscriptRow::Tool(tool_item("tail", "read_file")));
            app.apply_host_event(scoped_snapshot(&scope, 1, &rows));
            for resync in [false, true] {
                let at = app.pane.keyed.get(selected).unwrap();
                select_blocks(&mut app, at, at);
                let (_, head) = app.pane.selection.endpoints_for_test().unwrap();
                app.pane
                    .selection
                    .drag(ContentPosition { column: 99, ..head });
                app.pane
                    .selection
                    .capture_blocks(app.pane.transcript_cache.layout());
                let _ = paint(&mut app);
                let before = app.pane.transcript_cache.measurements();
                if resync {
                    app.apply_host_event(scoped_snapshot(&scope, 3, &rows));
                } else {
                    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
                        scope: scope.clone(),
                        from_revision: 1,
                        to_revision: 2,
                        upserts: vec![TranscriptItem {
                            key: tool,
                            row: rows[&tool].clone(),
                        }],
                        deletes: Vec::new(),
                    }));
                }
                let _ = paint(&mut app);
                let work = app.pane.transcript_cache.measurements() - before;
                assert!(
                    work <= 1_024,
                    "scope={scope:?} count={count} resync={resync} work={work}"
                );
                if count >= 8_000 {
                    assert!(
                        app.pane.transcript_cache.estimated_blocks().len() > count as usize - 1_024
                    );
                }
                let copied = app
                    .take_transcript_selection_text()
                    .expect("surviving selection copies");
                assert!(copied.contains("selected text"), "{copied:?}");
                assert!(!copied.contains("history"), "{copied:?}");
            }
        }
    }
}

#[test]
fn ceiling_trim_keeps_stable_key_camera_focus_and_copy_selection() {
    use crate::tui::blocks::ScrollBlock;
    let scope = TranscriptScope::Root;
    let mut app = scoped_app(&scope);
    let rows: BTreeMap<_, _> = (1..=10_000)
        .map(|n| {
            (
                RowKey::new(n, Lane::Runtime, 0),
                TranscriptRow::assistant(format!("row {n}")),
            )
        })
        .collect();
    let key = RowKey::new(9_990, Lane::Runtime, 0);
    app.apply_host_event(scoped_snapshot(&scope, 1, &rows));
    let at = app.pane.keyed.get(key).unwrap();
    select_blocks(&mut app, at, at);
    app.pane.focused_block = Some(at);
    app.pane.transcript_view_anchor = Some((at, 0));
    app.pane.scroll_from_bottom = 3;
    app.push_block(ScrollBlock::System("new tail".into()));
    let at = app.pane.keyed.get(key).unwrap();
    assert_eq!(app.pane.focused_block, Some(at));
    assert_eq!(app.pane.transcript_view_anchor, Some((at, 0)));
    let _ = paint(&mut app);
    assert!(app.pane.selection.has_range());
    assert!(app.take_transcript_selection_text().is_some());
}

#[test]
fn a_changed_or_removed_block_inside_a_selected_range_invalidates_it() {
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        for change in ["replace", "delete", "insert"] {
            let mut app = scoped_app(&scope);
            let first = RowKey::new(2, Lane::Runtime, 0);
            let middle = RowKey::new(4, Lane::Runtime, 0);
            let last = RowKey::new(6, Lane::Runtime, 0);
            let before = BTreeMap::from([
                (first, TranscriptRow::assistant("first")),
                (middle, TranscriptRow::assistant("middle")),
                (last, TranscriptRow::assistant("last")),
            ]);
            app.apply_host_event(scoped_snapshot(&scope, 1, &before));
            let first_at = app.pane.keyed.get(first).unwrap();
            let last_at = app.pane.keyed.get(last).unwrap();
            select_blocks(&mut app, first_at, last_at);
            let mut after = before.clone();
            match change {
                "replace" => {
                    after.insert(middle, TranscriptRow::assistant("changed"));
                }
                "delete" => {
                    after.remove(&middle);
                }
                _ => {
                    after.insert(
                        RowKey::new(3, Lane::Runtime, 0),
                        TranscriptRow::assistant("inserted"),
                    );
                }
            }
            app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch::between(
                scope.clone(),
                1,
                2,
                &before,
                &after,
            )));
            let _ = paint(&mut app);
            assert!(
                !app.pane.selection.has_range(),
                "scope={scope:?} change={change}"
            );
        }
    }
}

#[test]
fn appending_mail_invalidates_a_selected_group_summary_inside_unchanged_endpoints() {
    use crate::tui::blocks::RowTarget;
    use plakous_host_protocol::views::transcript::NoticeScope;
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let mut app = scoped_app(&scope);
        let first = RowKey::new(1, Lane::Runtime, 0);
        let head = RowKey::new(2, Lane::Runtime, 0);
        let last = RowKey::new(3, Lane::Runtime, 0);
        let before = BTreeMap::from([
            (first, TranscriptRow::assistant("selected text")),
            (
                head,
                TranscriptRow::info(NoticeScope::Tree, "result · #c1 · first\nbody"),
            ),
            (
                last,
                TranscriptRow::info(NoticeScope::Tree, "result · #c2 · second\nbody"),
            ),
        ]);
        app.apply_host_event(scoped_snapshot(&scope, 1, &before));
        let head_at = app.pane.keyed.get(head).unwrap();
        app.toggle_row(head_at, RowTarget::Group);
        let first_at = app.pane.keyed.get(first).unwrap();
        let last_at = app.pane.keyed.get(last).unwrap();
        select_blocks(&mut app, first_at, last_at);
        app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
            scope,
            from_revision: 1,
            to_revision: 2,
            upserts: vec![TranscriptItem {
                key: RowKey::new(4, Lane::Runtime, 0),
                row: TranscriptRow::info(NoticeScope::Tree, "result · #c3 · third\nbody"),
            }],
            deletes: Vec::new(),
        }));
        let _ = paint(&mut app);
        assert!(!app.pane.selection.has_range());
    }
}

#[test]
fn hidden_root_resync_does_not_touch_child_selection_or_pending_tool_click() {
    use crate::tui::blocks::RowTarget;
    let mut app = scoped_app(&TranscriptScope::Root);
    app.apply_host_event(scoped_snapshot(&TranscriptScope::Root, 1, &BTreeMap::new()));
    app.open_agent_view("c1".into(), "child".into());
    let child = TranscriptScope::Agent("c1".into());
    let key = RowKey::new(2, Lane::Runtime, 0);
    app.apply_host_event(scoped_snapshot(
        &child,
        1,
        &BTreeMap::from([(
            key,
            TranscriptRow::Tool(tool_item("child-call", "read_file")),
        )]),
    ));
    let at = app.pane.keyed.get(key).unwrap();
    select_blocks(&mut app, at, at);
    let selection = app.pane.selection.endpoints_for_test();
    app.pane.pending_tool_click = Some((at, RowTarget::Item));
    app.apply_host_event(scoped_snapshot(
        &TranscriptScope::Root,
        2,
        &BTreeMap::from([(key, TranscriptRow::assistant("hidden root"))]),
    ));
    let _ = paint(&mut app);
    assert_eq!(app.pane.selection.endpoints_for_test(), selection);
    assert_eq!(app.pane.pending_tool_click, Some((at, RowTarget::Item)));
}

#[test]
fn another_root_session_cannot_inherit_ui_state_for_reused_row_keys() {
    use crate::tui::blocks::{RowTarget, ScrollBlock};
    use plakous_host_protocol::views::transcript::NoticeScope;
    let mut app = scoped_app(&TranscriptScope::Root);
    let key = RowKey::new(2, Lane::Runtime, 0);
    let rows = BTreeMap::from([(
        key,
        TranscriptRow::info(NoticeScope::Tree, "result · #c1 · repeat\nfull body"),
    )]);
    bind_session(&mut app, "first-session");
    app.apply_host_event(scoped_snapshot(&TranscriptScope::Root, 9, &rows));
    let at = app.pane.keyed.get(key).unwrap();
    app.toggle_row(at, RowTarget::Item);
    select_blocks(&mut app, at, at);
    bind_session(&mut app, "second-session");
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 9,
        to_revision: 10,
        upserts: Vec::new(),
        deletes: vec![key],
    }));
    assert_eq!(app.pane.presentation.revision(), 9);
    assert_eq!(app.pane.resync, Some(TranscriptScope::Root));
    app.apply_host_event(scoped_snapshot(&TranscriptScope::Root, 1, &rows));
    assert_eq!(app.pane.presentation.revision(), 1);
    assert!(app.pane.resync.is_none());
    let at = app.pane.keyed.get(key).unwrap();
    assert!(matches!(
        app.pane.blocks[at],
        ScrollBlock::Mail { open: false, .. }
    ));
    assert!(app.pane.focused_block.is_none());
    assert!(!app.pane.selection.has_range());
}

#[test]
fn successor_snapshot_before_binding_keeps_new_profile_notices_and_accepts_patches() {
    use crate::surface_protocol::{ProfileOutcome, SurfaceObservation};
    use crate::tui::blocks::{RowTarget, ScrollBlock};
    use plakous_host_protocol::views::transcript::NoticeScope;
    let mut app = scoped_app(&TranscriptScope::Root);
    let header_key = RowKey::new(0, Lane::Runtime, 0);
    let key = RowKey::new(2, Lane::Runtime, 0);
    let header = |id: &str| TranscriptRow::SessionHeader {
        session_id: id.to_owned(),
        carried_from: None,
    };
    let mut rows = BTreeMap::from([
        (header_key, header("s1")),
        (
            key,
            TranscriptRow::info(NoticeScope::Tree, "result · #c1 · mail\nbody"),
        ),
    ]);
    app.apply_host_event(scoped_snapshot(&TranscriptScope::Root, 1, &rows));
    let at = app.pane.keyed.get(key).unwrap();
    app.toggle_row(at, RowTarget::Item);
    rows.insert(header_key, header("successor"));
    app.apply_host_event(scoped_snapshot(&TranscriptScope::Root, 2, &rows));
    let at = app.pane.keyed.get(key).unwrap();
    assert!(matches!(
        app.pane.blocks[at],
        ScrollBlock::Mail { open: false, .. }
    ));
    bind_session(&mut app, "successor");
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::ProfileApplied {
        name: "target-profile".into(),
        outcome: ProfileOutcome::Forked,
    }));
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 2,
        to_revision: 3,
        upserts: vec![TranscriptItem {
            key: RowKey::new(3, Lane::Runtime, 0),
            row: TranscriptRow::info(NoticeScope::Settings, "Model switched"),
        }],
        deletes: Vec::new(),
    }));
    assert!(app.pane.resync.is_none());
    assert_eq!(app.pane.presentation.revision(), 3);
    assert!(app.pane.blocks.iter().any(|block| matches!(block,
        ScrollBlock::System(text) if text.starts_with("forked this session onto profile target-profile")
    )));
}

#[test]
fn keyed_payloads_move_into_root_and_child_blocks_without_a_second_content_copy() {
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let mut app = App::new(launch_basic(), true);
        if matches!(scope, TranscriptScope::Agent(_)) {
            app.open_agent_view("c1".into(), "child".into());
        }
        let key = RowKey::new(9, Lane::Runtime, 0);
        let original = "snapshot payload ".repeat(256);
        let original_allocation = original.as_ptr();
        app.apply_host_event(HostEvent::PresentationSnapshot(PresentationSnapshot {
            scope: scope.clone(),
            revision: 1,
            items: vec![TranscriptItem {
                key,
                row: TranscriptRow::assistant(original),
            }],
            ..PresentationSnapshot::default()
        }));
        let at = app.pane.keyed.get(key).unwrap();
        let super::ScrollBlock::Assistant(text) = &app.pane.blocks[at] else {
            panic!("assistant block");
        };
        assert!(
            std::ptr::eq(text.as_ptr(), original_allocation),
            "snapshot payload was cloned"
        );
        let updated = "patch payload ".repeat(256);
        let updated_allocation = updated.as_ptr();
        app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
            scope,
            from_revision: 1,
            to_revision: 2,
            upserts: vec![TranscriptItem {
                key,
                row: TranscriptRow::assistant(updated),
            }],
            deletes: Vec::new(),
        }));
        let at = app.pane.keyed.get(key).unwrap();
        let super::ScrollBlock::Assistant(text) = &app.pane.blocks[at] else {
            panic!("assistant block");
        };
        assert!(
            std::ptr::eq(text.as_ptr(), updated_allocation),
            "patch payload was cloned"
        );
        assert_eq!(app.pane.presentation.revision(), 2);
    }
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
    let mut baseline = snapshot(
        3,
        &BTreeMap::from([(
            RowKey::new((1_u64 << 63) + 20, Lane::Runtime, 0),
            row.clone(),
        )]),
    );
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

#[test]
fn a_refused_snapshot_note_survives_the_resync_snapshot_once_per_cause() {
    use crate::tui::blocks::ScrollBlock;
    let mut app = scoped_app(&TranscriptScope::Root);
    let kept = RowKey::new(2, Lane::Runtime, 0);
    let rows = BTreeMap::from([(kept, TranscriptRow::assistant("kept"))]);
    app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(1, &rows)));
    let mut invalid = snapshot(2, &rows);
    invalid.items.push(invalid.items[0].clone());
    app.apply_host_event(HostEvent::PresentationSnapshot(invalid.clone()));
    invalid.revision = 3;
    app.apply_host_event(HostEvent::PresentationSnapshot(invalid));
    let refusals = |app: &App| {
        app.pane
            .blocks
            .iter()
            .filter(|block| {
                matches!(block, ScrollBlock::Error(text)
                    if text.starts_with("invalid presentation snapshot refused (KeyOrder)"))
            })
            .count()
    };
    assert_eq!(refusals(&app), 1, "one note per cause");
    assert!(app.pane.resync.is_some());

    let after = RowKey::new(3, Lane::Runtime, 0);
    let mut resynced = rows;
    resynced.insert(after, TranscriptRow::assistant("after the resync"));
    app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(4, &resynced)));
    assert!(app.pane.keyed.get(after).is_some());
    assert_eq!(refusals(&app), 1, "the resync snapshot keeps the note");
    assert!(paint(&mut app).contains("invalid·presentation·snapshot·refused·(KeyOrder)"));
}

#[test]
fn a_committed_row_sharing_a_call_id_stays_a_block_of_its_own() {
    let mut app = scoped_app(&TranscriptScope::Root);
    let result = |subject: &str| {
        let mut tool = tool_item("child-result:m1", "child_result");
        tool.subject = Some(subject.to_owned());
        TranscriptRow::Tool(tool)
    };
    // An imported result, then the first result of the tree the session runs
    // now, which numbers its mail from the start again.
    let imported = RowKey::new(0, Lane::Product, 1);
    let later = RowKey::new(6, Lane::Product, 0);
    app.apply_host_event(HostEvent::PresentationSnapshot(snapshot(
        1,
        &BTreeMap::from([(imported, result("m1 · c1 completed"))]),
    )));
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 1,
        to_revision: 2,
        upserts: vec![TranscriptItem {
            key: later,
            row: result("m1 · c2 completed"),
        }],
        deletes: Vec::new(),
    }));
    assert_eq!(app.pane.keyed.get(imported), Some(0));
    assert_eq!(app.pane.keyed.get(later), Some(1));
    let subjects: Vec<_> = app
        .pane
        .blocks
        .iter()
        .filter_map(|block| match block {
            crate::tui::blocks::ScrollBlock::Tool { subject, .. } => subject.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(subjects, ["m1 · c1 completed", "m1 · c2 completed"]);
}
