//! Transcript, tool block, and selection tests.

use super::terminal::osc52_sequence;
use super::test_support::{
    PINNED_UNIX_MS, assert_visible_transcript_rows_match_projection, blocks_contain,
    compose_transcript_app, key, key_alt, key_ctrl, key_ctrl_shift, launch_basic, left_click,
    left_mouse, live_compose, live_compose_child, pin_clock, queue_item, restore_snapshot,
    resume_fixture, say, serialize_frame, settings_row, status_row, tool_item, tool_line,
    transcript_fixture,
};
use super::{
    App, DisplayMode, MAX_LIVE_TRANSCRIPT_BLOCKS, MAX_LIVE_TRANSCRIPT_BYTES,
    TRANSCRIPT_TRUNCATION_MARKER, TranscriptChange,
};
use crate::tui::blocks::RenderPrefs;
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::{ActivityKind, ActivityState};
use crate::tui::{HostEvent, QueueDrainState, SettingsSnapshot, UserAction, user_action_channel};
use crate::tui::{SurfaceIntent, SurfaceObservation};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use plakous_host_protocol::views::transcript::{NoticeScope, ToolItem, TranscriptRow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::Duration;
use unicode_width::UnicodeWidthStr as _;

#[test]
fn row_plans_are_reused_for_viewport_and_width_only_repaints() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "a",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("a.rs"),
    ));
    say(&mut app, "Answer");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    crate::tui::activity::PLAN_CALLS.set(0);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(crate::tui::activity::PLAN_CALLS.get(), 0);
    let mut narrow = Terminal::new(TestBackend::new(40, 24)).unwrap();
    narrow.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(crate::tui::activity::PLAN_CALLS.get(), 0);
    app.render_prefs.group_tool_calls = false;
    narrow.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(crate::tui::activity::PLAN_CALLS.get(), 1);
}

#[test]
fn hidden_thinking_and_group_items_stay_inside_activity_groups() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.show_thinking = false;
    app.apply_host_event(tool_line(
        "a",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("a.rs"),
    ));
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "r".into(),
        text: "Thinking".into(),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "r".into(),
    });
    app.push_block(ScrollBlock::Discarded {
        text: "old thought".into(),
        thinking: true,
    });
    app.push_block(ScrollBlock::item("notice"));
    app.apply_host_event(tool_line(
        "b",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("b.rs"),
    ));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.apply_host_event(HostEvent::AssistantDelta("Answer".into()));
    let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert_eq!(
        plans
            .iter()
            .filter(|p| p.activity_summary.is_some())
            .count(),
        1
    );
    assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 2 files"));
    assert!(plans[1..5].iter().all(|p| p.hidden));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let live = serialize_frame(terminal.backend().buffer());
    assert!(live.contains("Read·2·files"));
    assert!(!live.contains("notice"));
    app.mark_transcript_replaced();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(live, serialize_frame(terminal.backend().buffer()));
    app.pane.focused_block = Some(0);
    app.handle_key(key(KeyCode::Enter));
    let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert!(plans[1..5].iter().all(|p| !p.hidden));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(serialize_frame(terminal.backend().buffer()).contains("notice"));
}

#[test]
fn shown_thinking_joins_and_notice_rows_end_activity_groups() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.show_thinking = true;
    for (id, path) in [("a", "a.rs"), ("b", "b.rs")] {
        app.apply_host_event(tool_line(
            id,
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "done",
            Some(path),
        ));
    }
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "r".into(),
        text: "Thinking".into(),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "r".into(),
    });
    app.apply_host_event(tool_line(
        "c",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("c.rs"),
    ));
    app.push_block(ScrollBlock::System("notice".into()));
    app.apply_host_event(tool_line(
        "d",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("d.rs"),
    ));
    let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert_eq!(
        plans[0].activity_summary.as_deref(),
        Some("Read 3 files, 1 thought")
    );
    assert!(
        plans[3].activity_summary.is_none(),
        "one call keeps its item"
    );
    assert!(plans[1].hidden, "the finished call folds into the summary");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let frame = serialize_frame(terminal.backend().buffer());
    for text in ["Read·3·files,·1·thought", "··notice", "Read·d.rs"] {
        assert!(frame.contains(text), "{text}");
    }
}

#[test]
fn assistant_after_reasoning_repaints_the_live_summary() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "a",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("a.rs"),
    ));
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "r".into(),
        text: "Thinking".into(),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "r".into(),
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.apply_host_event(HostEvent::AssistantDelta("Answer".into()));
    assert_eq!(
        app.pane.pending_transcript_change,
        TranscriptChange::Structural
    );
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let live = serialize_frame(terminal.backend().buffer());
    assert!(live.contains("Read·a.rs"));
    assert!(live.contains("Thinking"));
    app.mark_transcript_replaced();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(live, serialize_frame(terminal.backend().buffer()));
}

#[test]
fn transcript_selection_released_over_scrollbar_finishes_and_copies() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        (0..40)
            .map(|i| TranscriptRow::info(NoticeScope::Turn, format!("row-{i:02}")))
            .collect(),
    ));
    let mut terminal = Terminal::new(TestBackend::new(48, 14)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_body_area.unwrap();
    let track_x = body.right();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.bottom() - 1,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        track_x,
        body.y,
    ));
    assert_eq!(app.transcript_drag_pointer, Some((track_x, body.y)));
    assert!(app.advance_transcript_drag_autoscroll());
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        track_x,
        body.y,
    ));
    assert!(app.transcript_drag_pointer.is_none());
    assert!(!app.advance_transcript_drag_autoscroll());
    assert!(app.copy_selection_after_draw);
    assert!(app.take_transcript_selection_text().is_some());
}

#[test]
fn waiting_for_model_clock_ends_at_streaming_and_tool_activity() {
    for event in [
        tool_line(
            "a",
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "running",
            Some("a.rs"),
        ),
        tool_line(
            "c",
            None,
            None,
            ActivityKind::Compact,
            "compact",
            "running",
            None,
        ),
        HostEvent::AssistantDelta("Answer".into()),
        HostEvent::ReasoningDelta {
            location_key: "r".into(),
            text: "Thought".into(),
        },
    ] {
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(HostEvent::ModelRequestStarted);
        assert!(app.model_request_started.is_some());
        app.apply_host_event(event);
        assert!(app.model_request_started.is_none());
    }
}

#[test]
fn scrollbar_and_prose_frames_cover_width_classes_and_all_theme_pairs() {
    use crate::tui::theme::{Theme, ThemeId};
    for id in ThemeId::ALL {
        for width in [40, 60, 80, 120] {
            let mut app = App::new(launch_basic(), true);
            app.theme = Theme::for_id(id);
            app.pane.blocks = vec![ScrollBlock::Assistant(
                "# Heading\n\n**Bold** and [Linked](https://example.invalid)".into(),
            )];
            app.pane.pending_transcript_change = TranscriptChange::Structural;
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let area = app.transcript_area.unwrap();
            let body = app.transcript_body_area.unwrap();
            assert_eq!(body.right(), area.right() - 1, "{id:?} {width}");
            for y in area.y..area.bottom() {
                assert_eq!(
                    buffer[(area.right() - 1, y)].bg,
                    app.theme.colors.scrollbar_thumb.terminal()
                );
            }
            for (label, style) in [
                ("Heading", app.theme.assistant_label),
                ("Bold", app.theme.assistant_label),
                ("Linked", app.theme.markdown_link),
            ] {
                let cell = (body.y..body.bottom())
                    .find_map(|y| {
                        let row = (body.x..body.right())
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>();
                        row.find(label)
                            .map(|column| &buffer[(body.x + u16::try_from(column).unwrap(), y)])
                    })
                    .expect("markdown label visible");
                assert_eq!(cell.fg, style.fg.unwrap(), "{id:?} {width} {label}");
                assert_ne!(cell.fg, app.theme.success.fg.unwrap());
            }
        }
    }
}

#[test]
fn transcript_scrollbar_reserves_column_and_track_click_pages() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![ScrollBlock::Assistant(
        (0..100).map(|i| format!("row {i}\n")).collect(),
    )];
    app.pane.pending_transcript_change = TranscriptChange::Structural;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let content = app.transcript_body_area.unwrap();
    let area = app.transcript_area.unwrap();
    assert_eq!(content.right(), area.right() - 1);
    let before = app.pane.scroll_from_bottom;
    app.handle_mouse(left_click(area.right() - 1, area.y));
    assert!(app.pane.scroll_from_bottom > before);
    assert!(!app.selection.has_range());
}

#[test]
fn transcript_scrollbar_drag_reaches_both_ends_without_selecting() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![ScrollBlock::Assistant(
        (0..100).map(|i| format!("row {i}\n")).collect(),
    )];
    app.pane.pending_transcript_change = TranscriptChange::Structural;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let area = app.transcript_area.unwrap();
    let x = area.right() - 1;
    app.handle_mouse(left_click(x, area.bottom() - 1));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        area.y,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.pane.scroll_from_bottom > 70);
    assert!(!app.selection.has_range());
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        area.bottom() - 1,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.pane.scroll_from_bottom, 0);
}

fn activity_frame(app: &mut App, theme: &str, width: u16, story: &str) {
    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let actual = serialize_frame(terminal.backend().buffer());
    if story == "collapsed" {
        assert!(
            actual.contains("✓·Read·1·file"),
            "collapsed summary must be painted"
        );
    } else if story == "failure" {
        assert!(
            actual.contains("✗·Read·1·file"),
            "failure summary must be painted"
        );
        assert!(actual.contains("exit·7"), "failure exit stays in the meta");
    }
    let path = format!("tests/goldens/activity-{story}-{theme}-{width}x40.frame");
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(
            actual,
            std::fs::read_to_string(&path).expect("activity golden"),
            "{path}"
        );
    }
}

#[test]
fn the_one_line_activity_summary_is_clickable() {
    let mut app = transcript_fixture();
    for (id, name, subject) in [
        ("read", "read_file", "a.rs"),
        ("search", "grep", "query"),
        ("edit", "apply_patch", "a.rs +12/-3"),
        ("run", "exec_command", "cargo test"),
    ] {
        app.apply_host_event(tool_line(
            id,
            None,
            None,
            ActivityKind::Tool,
            name,
            "done",
            Some(subject),
        ));
    }
    say(&mut app, "Finished.");
    let mut terminal = Terminal::new(TestBackend::new(40, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_body_area.unwrap();
    let summary = app.pane.transcript_cache.plain()[0].clone();
    assert!(summary.ends_with('▸'), "one summary line: {summary}");
    app.handle_mouse(left_click(3, body.y));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 3, body.y));
    assert!(!crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[0].group_folded);
}

#[test]
fn restored_activity_groups_match_live_collapsed_and_expanded_frames() {
    let theme = crate::tui::Theme::dark();
    for width in [40, 120] {
        let mut live = transcript_fixture();
        live.theme = theme.clone();
        live.apply_host_event(HostEvent::UserMessage {
            text: "Inspect.".into(),
            sent_at_ms: None,
        });
        live.apply_host_event(tool_line(
            "read",
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "done",
            Some("a.rs"),
        ));
        live.apply_host_event(tool_line(
            "run",
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "failed:exit 7",
            Some("cargo test"),
        ));
        say(&mut live, "Finished.");
        let item = |id: &str, name: &str, subject: &str, state: &str| {
            TranscriptRow::Tool(ToolItem {
                subject: Some(subject.into()),
                detail: Some("RAW RESULT".into()),
                state: state.into(),
                ..tool_item(id, name)
            })
        };
        let mut restored = transcript_fixture();
        restored.theme = theme.clone();
        restored.apply_host_event(restore_snapshot(
            1,
            vec![
                TranscriptRow::user("Inspect."),
                item("read", "read_file", "a.rs", "done"),
                item("run", "exec_command", "cargo test", "failed:exit 7"),
                TranscriptRow::assistant("Finished."),
            ],
        ));
        // The host sets one activity kind per tool for both paths; a live
        // `exec_command` is a process row exactly like its restore.
        let kinds = |app: &App| {
            app.pane
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ScrollBlock::Tool { name, kind, .. } => Some((name.clone(), *kind)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&live), kinds(&restored));
        assert!(kinds(&live).contains(&("exec_command".to_owned(), ActivityKind::Process)));
        for expanded in [false, true] {
            if expanded {
                live.toggle_tool_expand_at(1);
                restored.toggle_tool_expand_at(1);
            }
            let paint = |app: &mut App| {
                let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                serialize_frame(terminal.backend().buffer())
            };
            assert_eq!(
                paint(&mut live),
                paint(&mut restored),
                "width {width}, expanded {expanded}"
            );
        }
    }
}

#[test]
fn process_activity_stories_match_live_and_restored() {
    let (theme_name, theme) = ("dark", crate::tui::Theme::dark());
    for width in [40, 120] {
        let mut live = transcript_fixture();
        live.theme = theme.clone();
        let rows = [
            ("start", "Shell", "cargo test", ActivityState::Succeeded),
            (
                "wait",
                "process_session_poll",
                "Waiting on process 7 · cargo test",
                ActivityState::Running,
            ),
            (
                "exited",
                "process_session_poll",
                "Waited on process 8 · cargo check · exited 0",
                ActivityState::Succeeded,
            ),
            (
                "poll",
                "process_session_write",
                "Waited on process 9 · sleep 60 · still running",
                ActivityState::Succeeded,
            ),
            (
                "read",
                "process_session_poll",
                "Read output of process 9 · sleep 60",
                ActivityState::Succeeded,
            ),
            (
                "input",
                "process_session_write",
                "Sent input to process 10 · 4 bytes",
                ActivityState::Succeeded,
            ),
            (
                "interrupt",
                "process_session_write",
                "Sent interrupt to process 10 · cargo test",
                ActivityState::Succeeded,
            ),
            (
                "stop",
                "process_session_terminate",
                "Stopped process 9 · sleep 60",
                ActivityState::Succeeded,
            ),
            (
                "unknown",
                "process_session_poll",
                "Waiting on process 11",
                ActivityState::Running,
            ),
        ];
        let mut activity = Vec::new();
        for (id, name, subject, state) in rows {
            let kind = if name == "Shell" {
                ActivityKind::Process
            } else {
                ActivityKind::Tool
            };
            live.apply_host_event(tool_line(
                id,
                None,
                None,
                kind,
                name,
                if state == ActivityState::Running {
                    "running"
                } else {
                    "done"
                },
                Some(subject),
            ));
            activity.push(TranscriptRow::Tool(ToolItem {
                subject: Some(subject.into()),
                state: if state == ActivityState::Running {
                    "running"
                } else {
                    "done"
                }
                .into(),
                ..tool_item(id, name)
            }));
        }
        let mut restored = transcript_fixture();
        restored.theme = theme.clone();
        restored.apply_host_event(restore_snapshot(1, activity));
        let paint = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            serialize_frame(terminal.backend().buffer())
        };
        assert_eq!(paint(&mut live), paint(&mut restored));
        activity_frame(&mut live, theme_name, width, "process-operations");
        say(&mut live, "Finished.");
        say(&mut restored, "Finished.");
        assert_eq!(paint(&mut live), paint(&mut restored));
        live.toggle_tool_expand_at(0);
        restored.toggle_tool_expand_at(0);
        assert_eq!(paint(&mut live), paint(&mut restored));
        let plain = live.pane.transcript_cache.plain().join("\n");
        for label in [
            "Waiting on process",
            "Waited on process",
            "Read output of process",
            "Sent input to process",
            "Sent interrupt to process",
            "Stopped process",
        ] {
            assert!(plain.contains(label), "{label}: {plain}");
        }
    }
}

#[test]
fn interrupted_activity_stories_match_live_and_restored() {
    let (theme_name, theme) = ("dark", crate::tui::Theme::dark());
    for state in ["interrupted", "cancelled"] {
        for width in [40, 120] {
            let mut live = transcript_fixture();
            live.theme = theme.clone();
            live.apply_host_event(tool_line(
                "run",
                None,
                None,
                ActivityKind::Process,
                "exec_command",
                state,
                Some("sleep 60"),
            ));
            live.apply_host_event(HostEvent::Row(TranscriptRow::Interrupted));
            let mut restored = transcript_fixture();
            restored.theme = theme.clone();
            restored.apply_host_event(restore_snapshot(
                1,
                vec![
                    TranscriptRow::Tool(ToolItem {
                        subject: Some("sleep 60".into()),
                        state: "cancelled".into(),
                        ..tool_item("run", "exec_command")
                    }),
                    TranscriptRow::Interrupted,
                ],
            ));
            for stage in ["live", "collapsed", "expanded"] {
                if stage == "collapsed" {
                    say(&mut live, "Next answer.");
                    say(&mut restored, "Next answer.");
                } else if stage == "expanded" {
                    live.toggle_tool_expand_at(0);
                    restored.toggle_tool_expand_at(0);
                }
                let paint = |app: &mut App| {
                    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                    terminal.draw(|frame| app.draw(frame)).unwrap();
                    serialize_frame(terminal.backend().buffer())
                };
                assert_eq!(paint(&mut live), paint(&mut restored), "{state}, {stage}");
                let plain = live.pane.transcript_cache.plain().join("\n");
                assert!(plain.contains("⊘ interrupted"), "{plain}");
                assert!(plain.contains('✗'), "{plain}");
                assert!(!plain.contains('✓'), "{plain}");
                activity_frame(&mut live, theme_name, width, &format!("{state}-{stage}"));
            }
        }
    }
}

#[test]
fn activity_group_story_goldens_cover_every_width() {
    let (theme_name, theme) = ("dark", crate::tui::Theme::dark());
    for width in [120, 40] {
        let mut app = transcript_fixture();
        app.theme = theme.clone();
        app.render_prefs.reduced_motion = true;
        for (id, name, state, subject) in [
            ("read", "read_file", "done", "a.rs"),
            ("search", "grep", "done", "query"),
            ("edit", "apply_patch", "done", "a.rs +12/-3"),
            ("run", "exec_command", "running", "cargo test"),
        ] {
            app.apply_host_event(tool_line(
                id,
                None,
                None,
                ActivityKind::Tool,
                name,
                state,
                Some(subject),
            ));
        }
        activity_frame(&mut app, theme_name, width, "live");
        // The host stamps no duration here; a wall-clock measurement would
        // make the frame depend on the test machine.
        app.pane.tool_started.clear();
        app.apply_host_event(tool_line(
            "run",
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "done",
            Some("cargo test"),
        ));
        say(&mut app, "Finished.");
        activity_frame(&mut app, theme_name, width, "collapsed");
        app.pane.focused_block = Some(0);
        app.handle_key(key(KeyCode::Enter));
        activity_frame(&mut app, theme_name, width, "expanded");
        app.handle_key(key(KeyCode::Enter));
        app.apply_host_event(tool_line(
            "run",
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "failed:exit 7",
            Some("cargo test"),
        ));
        activity_frame(&mut app, theme_name, width, "failure");
        let (_, y) = app.tool_hit_rows[0];
        app.handle_mouse(left_mouse(MouseEventKind::Moved, 3, y));
        activity_frame(&mut app, theme_name, width, "hover");

        let mut cell = transcript_fixture();
        cell.theme = theme.clone();
        super::test_support::compose_cell(
            &mut cell,
            "cell",
            "exec_js",
            "done",
            "2 tools",
            "const answer = 42;\ntext(answer);",
            &[
                ("exec_command", "done", "pwd", "RAW OUTPUT"),
                ("read_file", "done", "a.rs", "RAW FILE CONTENT"),
            ],
        );
        activity_frame(&mut cell, theme_name, width, "script-collapsed");
        cell.toggle_tool_expand_at(0);
        activity_frame(&mut cell, theme_name, width, "script-expanded");
        let plain = cell.pane.transcript_cache.plain().join("\n");
        assert!(plain.contains("const answer"));
        assert!(plain.contains("$ pwd"));
        assert!(!plain.contains("RAW"));
    }
}

#[test]
fn activity_group_live_collapsed_expanded_and_failure_frames() {
    let theme = crate::tui::Theme::dark();
    for width in [120, 40] {
        let mut app = transcript_fixture();
        app.theme = theme.clone();
        for (id, name, state, subject) in [
            ("read", "read_file", "done", "a.rs"),
            ("search", "grep", "done", "needle"),
            ("edit", "apply_patch", "done", "a.rs +12/-3"),
            ("run", "exec_command", "running", "cargo test"),
        ] {
            let mut event = tool_line(
                id,
                None,
                None,
                ActivityKind::Tool,
                name,
                state,
                Some(subject),
            );
            if let HostEvent::ToolLine { detail, .. } = &mut event {
                *detail = Some("RAW PAYLOAD MUST NEVER APPEAR".into());
            }
            app.apply_host_event(event);
        }
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let live = serialize_frame(terminal.backend().buffer()).replace('·', " ");
        assert!(live.contains("✓ Read 1 file, ran 1 search"), "{live}");
        assert!(!live.contains("needle"), "finished calls fold: {live}");
        assert!(live.contains("$ cargo test"), "{live}");
        assert!(!live.contains("RAW PAYLOAD"), "{live}");
        app.apply_host_event(tool_line(
            "run",
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "failed:exit 7",
            Some("cargo test"),
        ));
        say(&mut app, "Finished.");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let collapsed = serialize_frame(terminal.backend().buffer()).replace('·', " ");
        assert!(!collapsed.contains("$ cargo test"), "{collapsed}");
        assert!(collapsed.contains("exit 7"), "{collapsed}");
        app.pane.focused_block = Some(0);
        app.handle_key(key(KeyCode::Enter));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let expanded = serialize_frame(terminal.backend().buffer()).replace('·', " ");
        assert!(expanded.contains("$ cargo test"), "{expanded}");
        assert!(!expanded.contains("RAW PAYLOAD"), "{expanded}");
        let (_, y) = app.tool_hit_rows[0];
        app.handle_mouse(left_click(3, y));
        app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 3, y));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(
            !serialize_frame(terminal.backend().buffer())
                .replace('·', " ")
                .contains("$ cargo test")
        );
    }
}

/// Spec 02 §5.5: a centered modal (the resume picker) scrims the transcript
/// behind it and the modal body itself stays opaque. Menu and Activity are
/// the full-page frame, so they draw no backdrop to scrim.
#[test]
fn the_modal_scrim_dims_the_transcript_and_the_body_stays_opaque() {
    let mut app = App::new(launch_basic(), true);
    app.push_block(ScrollBlock::user(
        "a transcript line behind the modal".into(),
    ));
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let before = terminal.backend().buffer().clone();

    app.resume_menu = Some(crate::tui::ResumeMenu::open(
        super::test_support::resume_fixture_entries(),
        super::test_support::FIXED_NOW,
        jiff::tz::TimeZone::UTC,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let after = terminal.backend().buffer().clone();
    let popup = app.resume_popup.expect("resume popup");

    let mut dimmed = 0usize;
    for y in 0..40u16 {
        for x in 0..120u16 {
            if popup.contains((x, y).into()) {
                continue;
            }
            let cell = &after[(x, y)];
            assert!(
                cell.modifier.contains(ratatui::style::Modifier::DIM),
                "the scrim must dim every cell outside the modal at ({x},{y})"
            );
            if before[(x, y)].style() != cell.style() {
                dimmed += 1;
            }
        }
    }
    assert!(dimmed > 0, "the scrim must visibly change the backdrop");

    // Inside the modal nothing is dimmed: the body is opaque.
    let inner_y = popup.y + popup.height / 2;
    let inner_x = popup.x + popup.width / 2;
    assert!(
        !after[(inner_x, inner_y)]
            .modifier
            .contains(ratatui::style::Modifier::DIM),
        "the modal body must stay opaque"
    );
}

#[test]
fn transcript_wheel_is_scoped_to_the_transcript_rect() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![ScrollBlock::Assistant(
        (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )];
    app.mark_transcript_replaced();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let composer = app.composer_inner.expect("composer");
    app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, composer.x, composer.y));
    assert_eq!(app.pane.scroll_from_bottom, 0);

    let transcript = app.transcript_area.expect("transcript");
    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollUp,
        transcript.x,
        transcript.y,
    ));
    app.drain_mouse_scroll();
    assert_eq!(app.pane.scroll_from_bottom, 3);
}

#[test]
fn transcript_wheel_coalesces_before_drain() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![ScrollBlock::Assistant(
        (0..80)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )];
    app.mark_transcript_replaced();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let transcript = app.transcript_area.expect("transcript");
    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollUp,
        transcript.x,
        transcript.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollUp,
        transcript.x,
        transcript.y,
    ));
    assert_eq!(app.pane.scroll_from_bottom, 0);
    app.drain_mouse_scroll();
    assert!(app.pane.scroll_from_bottom >= 2);
    assert!(app.mouse_scroll_queue != 0 || app.pane.scroll_from_bottom >= 3);
}

#[test]
fn tool_expand_splices_without_full_rebuild() {
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "one",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some("true"),
    ));
    say(&mut app, "Finished.");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.toggle_tool_expand_at(0);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(!crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[0].group_folded);
    assert!(
        app.pane
            .transcript_cache
            .plain()
            .iter()
            .any(|row| row.contains("$ true"))
    );
}

#[test]
fn wheel_during_tail_stream_keeps_unfollowed_offset() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = (0..40)
        .map(|index| ScrollBlock::System(format!("row-{index:02}")))
        .collect();
    app.mark_transcript_replaced();
    let backend = TestBackend::new(48, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.pane.transcript_total_rows > 10);
    let transcript = app.transcript_area.expect("transcript");
    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollUp,
        transcript.x,
        transcript.y,
    ));
    app.drain_mouse_scroll();
    let after_wheel = app.pane.scroll_from_bottom;
    assert!(after_wheel > 0);
    app.apply_host_event(HostEvent::AssistantDelta("more streamed text\n".into()));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        app.pane.scroll_from_bottom >= after_wheel,
        "streaming must not snap a just-scrolled viewport back to the tail"
    );
}

#[test]
fn wheel_down_during_tail_stream_moves_by_the_notch() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = (0..80)
        .map(|index| ScrollBlock::System(format!("row-{index:02}")))
        .collect();
    app.mark_transcript_replaced();
    let backend = TestBackend::new(48, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let transcript = app.transcript_area.expect("transcript");
    for _ in 0..20 {
        app.handle_mouse(left_mouse(
            MouseEventKind::ScrollUp,
            transcript.x,
            transcript.y,
        ));
        app.drain_mouse_scroll();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }
    assert!(
        app.pane.scroll_from_bottom > 20,
        "the viewport must be unfollowed well above the tail"
    );
    let start = app.pane.transcript_visible_start;

    for _ in 0..12 {
        app.apply_host_event(HostEvent::AssistantDelta(
            "streamed line of assistant text\n".into(),
        ));
    }
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.pane.transcript_visible_start, start,
        "stream-only frames hold the anchored row"
    );
    let from_bottom = app.pane.scroll_from_bottom;

    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollDown,
        transcript.x,
        transcript.y,
    ));
    app.drain_mouse_scroll();
    for _ in 0..12 {
        app.apply_host_event(HostEvent::AssistantDelta(
            "more streamed assistant text\n".into(),
        ));
    }
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let moved = app.pane.transcript_visible_start.saturating_sub(start);
    assert!(
        moved <= 8,
        "a down-notch during a growing stream must not follow the tail: start={start} now={} moved={moved}",
        app.pane.transcript_visible_start
    );
    let after = app.pane.scroll_from_bottom;
    assert!(after > 0, "a small down-scroll must not snap follow-live");
    // Stream added ~12 rows below the held place; the notch spends ~3 of
    // them. Holding from-bottom against the new total would shrink it by the
    // notch only (and jump the reading row toward live).
    assert!(
        after + 3 >= from_bottom + 8,
        "the unfollowed camera must keep the extra streamed rows below the viewport: before={from_bottom} after={after}"
    );
}

#[test]
fn the_turn_status_row_names_thinking_tools_queue_and_approval() {
    for width in [80u16, 120] {
        // Busy with no tool and no unsealed thinking block is waiting on the model.
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(HostEvent::SetBusy(true));
        let row = status_row(&mut app, width);
        assert!(row.contains("waiting for model"), "{row}");
        assert!(!row.contains("thinking"), "{row}");
        assert!(!row.contains("[stop]"), "{row}");

        // Unsealed thinking names the row; a sealed thought does not.
        app.apply_host_event(HostEvent::ReasoningDelta {
            location_key: "0:0".into(),
            text: "weighing the shapes".into(),
        });
        let row = status_row(&mut app, width);
        assert!(row.contains("thinking"), "{row}");
        app.apply_host_event(HostEvent::ReasoningSealed {
            location_key: "0:0".into(),
        });
        let row = status_row(&mut app, width);
        assert!(row.contains("waiting for model"), "{row}");
        assert!(!row.contains("thinking"), "{row}");

        // A running tool names the row with its own §5.2 header.
        app.apply_host_event(tool_line(
            "call-1",
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "running",
            Some("cargo test --lib"),
        ));
        let row = status_row(&mut app, width);
        assert!(row.contains("running command"), "{row}");

        // A queued follow-up shows in the queue panel; the HUD does not
        // repeat it.
        app.apply_host_event(HostEvent::QueueSnapshot {
            items: vec![queue_item("q1", 1, "next")],
            drain: QueueDrainState::LiveAutoDrain,
        });
        let row = status_row(&mut app, width);
        assert!(!row.contains("queued"), "{row}");

        // A pending approval pulses instead of spinning and waits.
        app.apply_host_event(HostEvent::ApprovalRequired {
            approval_id: "approval-1".into(),
            call_id: Some("call-1".into()),
            tool_name: "exec_command".into(),
            subject: Some("rm -rf build".into()),
            prompt: None,
            expires_at_ms: 0,
        });
        let row = status_row(&mut app, width);
        assert!(row.contains("approval needed"), "{row}");
        assert!(row.contains("◆") || row.contains("◇"), "{row}");
    }
}

/// §5.5: a folded verb-group head expands to its member rows on `Ctrl-O`,
/// even though the run's last block — the fold keymap's fallback target —
/// is a hidden member drawn nowhere on screen.
#[test]
fn a_folded_verb_group_expands_from_the_keymap_fallback() {
    let mut app = App::new(launch_basic(), true);
    for (index, file) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        app.apply_host_event(tool_line(
            &format!("call-{index}"),
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "ok",
            Some(file),
        ));
    }
    say(&mut app, "Finished.");
    let folded = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert_eq!(folded[0].group_count, 3, "three reads fold into one head");
    assert!(folded[0].group_folded);
    assert!(folded[1].hidden && folded[2].hidden);

    // No row is focused, so the fold keymap falls back to the last
    // foldable block - which is a hidden member of this run.
    app.pane.focused_block = None;
    assert!(app.handle_key(key_ctrl(KeyCode::Char('o'))).is_none());
    let opened = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert!(!opened[0].group_folded, "the head opened");
    assert!(
        opened[0].activity_action.as_deref() == Some("Explored 3 files"),
        "read actions remain merged under the open summary"
    );

    // The same key folds the run back up.
    assert!(app.handle_key(key_ctrl(KeyCode::Char('o'))).is_none());
    let refolded = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert!(refolded[0].group_folded);
    assert!(refolded[1].hidden && refolded[2].hidden);
}

/// `group tool calls = off` had no effect on the
/// running session. Flipping the row re-plans the transcript on the same
/// frame, both while the draft previews and once the host applies it.
#[test]
fn toggling_group_tool_calls_replans_the_transcript_live() {
    let mut app = App::new(launch_basic(), true);
    for (index, file) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        app.apply_host_event(tool_line(
            &format!("call-{index}"),
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "ok",
            Some(file),
        ));
    }
    say(&mut app, "Finished.");
    assert_eq!(
        crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[0].group_count,
        3,
        "the run folds while grouping is on"
    );

    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let row = settings_row(crate::tui::settings_registry::GROUP_TOOL_CALLS);
    app.settings_menu.as_mut().expect("settings").selected = row;
    app.pane.pending_transcript_change = TranscriptChange::Unchanged;
    // A switch applies on change, so the flip is also a save.
    assert!(matches!(
        app.handle_key(key(KeyCode::Char(' '))),
        Some(UserAction::Extension(SurfaceIntent::SaveSettings(_)))
    ));

    assert!(
        !app.render_prefs.group_tool_calls,
        "the draft previews live"
    );
    assert_eq!(
        app.pane.pending_transcript_change,
        TranscriptChange::Structural,
        "a re-plan is a structural transcript change"
    );
    let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert!(
        plans.iter().all(|plan| plan.group_count == 0),
        "completed runs stay unfolded; exploration still merges"
    );

    // The applied snapshot keeps the ungrouped plan after the modal closes.
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsApplied(
        SettingsSnapshot {
            group_tool_calls: false,
            ..SettingsSnapshot::default()
        },
    )));
    assert!(!app.render_prefs.group_tool_calls);
    let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert!(plans.iter().all(|plan| plan.group_count == 0));
}

#[test]
fn a_manual_fold_survives_streaming_and_ctrl_shift_o_clears_it() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "call-1",
        None,
        None,
        ActivityKind::Compose,
        "exec_js",
        "running",
        Some("cargo test"),
    ));
    app.pane.focused_block = Some(0);
    app.toggle_tool_expand_at(0);
    let opened = app.pane.blocks[0].display_mode();
    app.apply_host_event(tool_line(
        "call-1",
        None,
        None,
        ActivityKind::Compose,
        "exec_js",
        "done",
        Some("cargo test"),
    ));
    assert_eq!(
        app.pane.blocks[0].display_mode(),
        opened,
        "a block the operator opened must not snap shut under them"
    );

    assert!(app.handle_key(key_ctrl_shift(KeyCode::Char('O'))).is_none());
    assert!(
        !app.pane.blocks[0].is_manual(),
        "Ctrl+Shift+O clears overrides"
    );
}

#[test]
fn reasoning_deltas_stream_one_thinking_block_that_seals_shut() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "0:0".into(),
        text: "weighing ".into(),
    });
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "0:0".into(),
        text: "the shapes".into(),
    });
    assert_eq!(app.pane.blocks.len(), 1, "one block per reasoning location");
    assert!(matches!(
        app.pane.blocks[0],
        ScrollBlock::Thinking {
            sealed: false,
            mode: DisplayMode::Collapsed,
            ..
        }
    ));
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "0:0".into(),
    });
    assert!(matches!(
        app.pane.blocks[0],
        ScrollBlock::Thinking {
            sealed: true,
            mode: DisplayMode::Collapsed,
            ..
        }
    ));
}

#[test]
fn an_approval_resolves_only_by_chord_never_by_typed_letters() {
    let request = HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: Some("call-1".into()),
        tool_name: "exec_command".into(),
        subject: Some("rm -rf build".into()),
        prompt: Some("this deletes files".into()),
        expires_at_ms: 0,
    };
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(request.clone());
    // An unfocused approval row does not own the bare letters: they are
    // ordinary composer input, empty draft or not.
    assert!(app.handle_key(key(KeyCode::Char('a'))).is_none());
    assert_eq!(app.input, "a");
    app.set_input(String::new());
    // `Alt-a` / `Alt-d` ask in the destructive pop-up; a pause, `→` and
    // `Enter` resolve.
    assert_eq!(app.handle_key(key_alt(KeyCode::Char('a'))), None);
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::ResolveApproval {
            approval_id: "approval-1".into(),
            choice: crate::tui::ApprovalChoice::Allow,
        })
    );
    // A focused approval row does not own them either: a reply typed after
    // clicking the row to read it is a draft (spec 02 §5.2).
    app.pane.focused_block = Some(0);
    assert!(app.handle_key(key(KeyCode::Char('a'))).is_none());
    assert!(app.handle_key(key(KeyCode::Char('d'))).is_none());
    assert_eq!(app.input, "ad");
    assert_eq!(app.handle_key(key_alt(KeyCode::Char('d'))), None);
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::ResolveApproval {
            approval_id: "approval-1".into(),
            choice: crate::tui::ApprovalChoice::Deny,
        })
    );
    // A draft in the composer keeps `a`/`d` as ordinary typing.
    app.pane.focused_block = None;
    app.set_input("ad".into());
    assert!(app.handle_key(key(KeyCode::Char('a'))).is_none());

    app.apply_host_event(HostEvent::ApprovalResolved {
        approval_id: "approval-1".into(),
        resolution: "denied".into(),
    });
    assert!(app.pending_approval().is_none());
    assert!(matches!(
        app.pane.blocks[0],
        ScrollBlock::Approval {
            resolution: Some(_),
            ..
        }
    ));
}

#[test]
fn enter_on_a_focused_subagent_row_opens_the_child_transcript() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "agent-1",
        None,
        None,
        ActivityKind::Agent,
        "spawn_agent",
        "running",
        Some("implement · payments-refactor"),
    ));
    app.pane.focused_block = Some(0);
    // A running Agent row is Collapsed, so Enter opens the child instead of
    // cycling a fold it has no detail for.
    app.pane.blocks[0].set_display_mode(DisplayMode::Collapsed, false);
    app.pane.blocks.push(ScrollBlock::System("spacer".into()));
    app.pane.focused_block = None;
    app.pane.focused_block = Some(0);
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::OpenTopologyNavigator)
    );
}

#[test]
fn enter_on_wait_agent_does_not_open_the_child_transcript() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "wait-1",
        None,
        None,
        ActivityKind::Agent,
        "wait_agent",
        "done:41s",
        Some("01J8"),
    ));
    app.pane.focused_block = Some(0);
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    assert_eq!(
        app.pane.blocks[0].display_mode(),
        Some(DisplayMode::Expanded),
        "Enter opens the wait's own detail"
    );
    assert_eq!(
        crate::tui::blocks::display_tool_title_for("wait_agent", ActivityKind::Agent),
        "Wait Agent"
    );
}

/// A Codex Code Mode shape: two-line prompts, a
/// markdown answer with wrapped bullets, then three collapsed Compose cells
/// whose outlines stay visible. Every hit row lands on the painted cell
/// header or its own outline row, and header clicks open the cell source.
#[test]
fn collapsed_compose_headers_after_hidden_children_stay_clickable_and_foldable() {
    let mut app = transcript_fixture();
    for index in 0..4 {
        app.push_block(live_compose(&format!("cell-{index}"), "1 tools"));
        app.push_block(live_compose_child(
            &format!("child-{index}"),
            &format!("cell-{index}"),
        ));
        say(&mut app, "Note between groups.");
    }
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_visible_transcript_rows_match_projection(&app, terminal.backend().buffer());
    assert_eq!(app.tool_hit_rows.len(), 8);
    let (block, y) = app
        .tool_hit_rows
        .iter()
        .copied()
        .rfind(|(i, _)| {
            matches!(
                &app.pane.blocks[*i],
                ScrollBlock::Tool {
                    kind: ActivityKind::Compose,
                    ..
                }
            )
        })
        .unwrap();
    app.handle_mouse(left_click(5, y));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 5, y));
    let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
    assert!(!plans[block].group_folded && !plans[block + 1].hidden);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_visible_transcript_rows_match_projection(&app, terminal.backend().buffer());
}

#[test]
fn submit_idle_is_prompt() {
    let mut app = App::new(launch_basic(), true);
    app.input = "hello world".into();
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::Prompt("hello world".into())));
    // Hosted agent: no local echo — host will emit UserMessage.
    assert!(!blocks_contain(&app, "hello world"));
    // Simulate host echo once.
    app.apply_host_event(HostEvent::UserMessage {
        text: "hello world".into(),
        sent_at_ms: None,
    });
    assert!(blocks_contain(&app, "hello world"));
    assert_eq!(app.input, "");
    assert!(!app.should_quit);
}

#[test]
fn hosted_prompt_does_not_double_echo() {
    let mut app = App::new(launch_basic(), true);
    app.input = "hi".into();
    let _ = app.handle_key(key(KeyCode::Enter));
    app.apply_host_event(HostEvent::UserMessage {
        text: "hi".into(),
        sent_at_ms: None,
    });
    let count = app
        .pane
        .blocks
        .iter()
        .filter(|b| matches!(b, ScrollBlock::User { text: t, .. } if t == "hi"))
        .count();
    assert_eq!(count, 1, "exactly one user bubble");
}

#[test]
fn hosted_idle_prompt_user_message_does_not_create_queue_chip() {
    let mut app = App::new(launch_basic(), true);
    app.input = "hello world".into();
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::Prompt("hello world".into())));
    app.apply_host_event(HostEvent::UserMessage {
        text: "hello world".into(),
        sent_at_ms: None,
    });
    assert_eq!(app.local_queue, [] as [plakous_host_protocol::QueueItem; 0]);
    assert_eq!(app.queue_len, 0);
    assert!(
        app.pane
            .blocks
            .iter()
            .any(|block| matches!(block, ScrollBlock::User { text, .. } if text == "hello world"))
    );
    assert!(!blocks_contain(&app, "[queue]"));
}

#[test]
fn pending_prompt_flushes_on_attach() {
    let mut launch = launch_basic();
    launch.agent_enabled = false;
    let mut app = App::new(launch, true);
    app.input = "early prompt".into();
    assert_eq!(app.submit_line(false), None);
    assert_eq!(app.pending_prompt.as_deref(), Some("early prompt"));

    let (tx, rx) = user_action_channel();
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.maybe_flush_pending_prompt_after_attach(Some(&tx));
    assert!(app.pending_prompt.is_none());
    assert_eq!(
        rx.try_recv()
            .expect("attach should flush the queued prompt"),
        UserAction::Prompt("early prompt".into())
    );
}

#[test]
fn live_transcript_updates_do_not_snap_a_scrolled_viewport_to_tail() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = (0..40)
        .map(|index| ScrollBlock::System(format!("row-{index:02}")))
        .collect();
    app.mark_transcript_replaced();
    app.pane.scroll_from_bottom = 7;
    let backend = TestBackend::new(48, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.pane.scroll_from_bottom, 7);
    let visible_start = app.pane.transcript_visible_start;
    assert!(visible_start > 0);

    app.apply_host_event(HostEvent::AssistantDelta(" delta".into()));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.pane.transcript_visible_start, visible_start);

    app.apply_host_event(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Host,
        "system",
    )));
    app.apply_host_event(HostEvent::Row(TranscriptRow::error(
        NoticeScope::Host,
        "error",
    )));
    app.apply_host_event(HostEvent::ToolLine {
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        call_id: Some("call".into()),
        name: "tool".into(),
        state: "start".into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.pane.transcript_visible_start, visible_start);
    assert!(app.pane.scroll_from_bottom >= 7);
}

#[test]
fn detached_camera_survives_snapshots_and_resumes_only_at_tip() {
    for (rekey, return_to_tip) in [(false, 0), (true, 1), (true, 2)] {
        let mut app = App::new(launch_basic(), true);
        let mut rows: Vec<_> = (0..40)
            .map(|i| TranscriptRow::assistant(format!("row-{i:02}\n\nsecond line\n\nthird line")))
            .collect();
        // Attach/resume starts at the tail.
        app.apply_host_event(restore_snapshot(1, rows.clone()));
        let mut terminal = Terminal::new(TestBackend::new(48, 14)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.pane.scroll_from_bottom, 0);
        app.pane.scroll_from_bottom = 37;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let anchor = app.pane.transcript_view_anchor.unwrap();
        let body = app.pane.blocks[anchor.0].body().to_owned();
        let assert_held = |app: &App| {
            let held = app.pane.transcript_view_anchor.unwrap();
            assert_eq!(app.pane.blocks[held.0].body(), body);
            assert_eq!(held.1, anchor.1);
            assert!(app.pane.scroll_from_bottom > 0);
            assert!(app.jump_to_live_hit.is_some());
        };
        // An identical turn-end replacement must not reset follow mode.
        app.apply_host_event(restore_snapshot(2, rows.clone()));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_held(&app);
        // A host wake has no user row; it starts streaming directly.
        app.apply_host_event(HostEvent::SetBusy(true));
        app.apply_host_event(HostEvent::AssistantDelta("wake output\n".repeat(20)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_held(&app);
        rows.push(TranscriptRow::assistant("wake output\n".repeat(20)));
        if rekey {
            // Rebuild the row vector with shifted ordering keys: content
            // before the camera can be inserted or evicted by projection.
            rows.remove(0);
            rows.insert(0, TranscriptRow::info(NoticeScope::Host, "new prefix"));
            rows.insert(0, TranscriptRow::info(NoticeScope::Host, "another prefix"));
        }
        app.apply_host_event(restore_snapshot(3, rows.clone()));
        app.apply_host_event(HostEvent::SetBusy(false));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_held(&app);
        // Scrolling down to the actual tip resumes following.
        match return_to_tip {
            0 => {
                while app.pane.scroll_from_bottom > 0 {
                    app.handle_key(key(KeyCode::PageDown));
                    terminal.draw(|frame| app.draw(frame)).unwrap();
                }
            }
            1 => {
                app.handle_key(key(KeyCode::End));
            }
            _ => {
                let hit = app.jump_to_live_hit.unwrap();
                app.handle_mouse(left_mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit.x,
                    hit.y,
                ));
            }
        }
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.jump_to_live_hit.is_none());
        let before = app.pane.transcript_visible_start;
        app.apply_host_event(HostEvent::AssistantDelta("new tail\n".repeat(20)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.pane.scroll_from_bottom, 0);
        assert!(app.pane.transcript_visible_start > before);
        rows.push(TranscriptRow::assistant("new tail\n".repeat(20)));
        app.apply_host_event(restore_snapshot(4, rows));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.pane.scroll_from_bottom, 0);
    }
}

#[test]
fn snapshot_rekeys_a_thought_without_moving_its_camera() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.group_tool_calls = false;
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "live-key".into(),
        text: "thought\n\n".repeat(40),
    });
    if let ScrollBlock::Thinking { mode, manual, .. } = &mut app.pane.blocks[0] {
        *mode = DisplayMode::Expanded;
        *manual = true;
    }
    say(&mut app, &"tail\n\n".repeat(30));
    let mut terminal = Terminal::new(TestBackend::new(48, 14)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.pane.scroll_from_bottom = 80;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let anchor = app.pane.transcript_view_anchor.unwrap();
    assert_eq!(anchor.0, 0);
    app.apply_host_event(restore_snapshot(
        1,
        vec![
            TranscriptRow::Thinking(plakous_host_protocol::views::transcript::ThinkingRow {
                location_key: "projected-key".into(),
                text: "thought\n\n".repeat(40),
                elapsed_ms: None,
            }),
            TranscriptRow::assistant("tail\n\n".repeat(30)),
        ],
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.pane.transcript_view_anchor, Some(anchor));
}

#[test]
fn replace_transcript_and_scroll_do_not_quit() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![
            TranscriptRow::info(NoticeScope::Turn, "line-a"),
            TranscriptRow::info(NoticeScope::Turn, "line-b"),
        ],
    ));
    assert_eq!(app.pane.blocks.len(), 2);
    assert!(matches!(&app.pane.blocks[0], ScrollBlock::System(_)));
    // Non-empty history empty → with empty input Up uses history or scroll.
    app.prompt_history.clear();
    assert!(app.handle_key(key(KeyCode::Up)).is_none());
    assert!(app.pane.scroll_from_bottom >= 1);
    assert!(!app.should_quit);
}

#[test]
fn child_result_notice_is_a_compact_structured_agent_row() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "child-result:m1",
        None,
        None,
        ActivityKind::Agent,
        "child_result",
        "done",
        Some("m1 · c2 completed"),
    ));
    assert!(matches!(
        app.pane.blocks.as_slice(),
        [ScrollBlock::Tool {
            kind: ActivityKind::Agent,
            name,
            state,
            subject: Some(subject),
            mode: DisplayMode::Collapsed,
            ..
        }] if name == "child_result" && state == "done" && subject == "m1 · c2 completed"
    ));
}

#[test]
fn narrow_frame_keeps_table_and_structured_activity_readable() {
    let mut app = App::new(launch_basic(), true);
    app.push_block(ScrollBlock::Assistant(
        concat!(
            "| Name | Description | State |\n",
            "| --- | --- | --- |\n",
            "| alpha | a very long explanation that remains readable | ready |\n",
            "| beta | short |\n",
        )
        .into(),
    ));
    app.apply_host_event(tool_line(
        "compact-1",
        None,
        None,
        ActivityKind::Compact,
        "compact",
        "done:29.3s",
        Some("199.7k → 12.4k"),
    ));
    app.apply_host_event(tool_line(
        "child-result:m1",
        None,
        None,
        ActivityKind::Agent,
        "child_result",
        "done",
        Some("m1 · c2 completed"),
    ));
    let mut terminal = Terminal::new(TestBackend::new(42, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let frame = serialize_frame(terminal.backend().buffer());
    assert!(frame.contains("Description"), "{frame}");
    assert!(frame.contains("explanation"), "{frame}");
    assert!(
        !frame
            .lines()
            .any(|line| line.contains('│') && line.contains('…') && !line.contains('❯')),
        "table cells wrap, never truncate: {frame}"
    );
    assert!(frame.contains("readable"), "{frame}");
    assert!(frame.contains("Compact"), "{frame}");
    assert!(frame.contains("Agent·m1"), "{frame}");
    assert!(!frame.contains("child result available"), "{frame}");
}

#[test]
fn model_refusal_restores_published_view_without_discarded_preview() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::UserMessage {
        text: "fixture request".into(),
        sent_at_ms: None,
    });
    app.apply_host_event(HostEvent::AssistantDelta(
        "discarded provisional output".into(),
    ));
    assert!(blocks_contain(&app, "discarded provisional output"));
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::user("fixture request")],
    ));
    app.apply_host_event(HostEvent::ModelRefusal { detail: None });
    assert!(!blocks_contain(&app, "discarded provisional output"));
    assert!(blocks_contain(&app, "fixture request"));
    assert!(blocks_contain(&app, "model refused the request"));
    assert!(!app.should_quit);
}

#[test]
fn host_status_and_transcript_events() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Status {
        phase: "running".into(),
        queue_len: 2,
        detail: Some("tool:edit".into()),
    });
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-1", 1, "one"), queue_item("q-2", 1, "two")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert_eq!(app.phase, "running");
    assert_eq!(app.queue_len, 2);
    assert_eq!(app.status_detail.as_deref(), Some("tool:edit"));

    app.apply_host_event(HostEvent::Row(TranscriptRow::assistant("hello")));
    assert!(blocks_contain(&app, "hello"));

    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::info(NoticeScope::Turn, "seeded")],
    ));
    assert_eq!(app.pane.blocks.len(), 1);
    assert_eq!(app.pane.blocks[0].body(), "seeded");

    app.apply_host_event(HostEvent::UserMessage {
        text: "u".into(),
        sent_at_ms: None,
    });
    app.apply_host_event(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Host,
        "sys",
    )));
    app.apply_host_event(HostEvent::Row(TranscriptRow::error(
        NoticeScope::Host,
        "err",
    )));
    app.apply_host_event(HostEvent::ToolLine {
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        call_id: None,
        name: "edit".into(),
        state: "done".into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    // Collapse running → done into a single tool row (no start/done spam).
    let before = app.pane.blocks.len();
    app.apply_host_event(HostEvent::ToolLine {
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        call_id: Some("cid-1".into()),
        name: "run_process".into(),
        state: "running".into(),
        subject: Some("cargo test".into()),
        detail: Some("{\"program\":\"cargo\"}".into()),
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    app.apply_host_event(HostEvent::ToolLine {
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        call_id: Some("cid-1".into()),
        name: "run_process".into(),
        state: "done".into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    assert_eq!(app.pane.blocks.len(), before + 1);
    assert!(
        matches!(
            app.pane.blocks.last(),
            Some(ScrollBlock::Tool {
                name,
                state,
                subject: Some(subj),
                detail: Some(_),
                parent_call_id: None, cell_id: None, kind: ActivityKind::Tool, manual: false, group_open: false, mode: DisplayMode::Collapsed,
                ..
            }) if name == "run_process" && state.starts_with("done:") && subj == "cargo test"
        ),
        "expected collapsed tool row with subject, got {:?}",
        app.pane.blocks.last()
    );
    app.apply_host_event(HostEvent::Binding {
        session_id: Some("s2".into()),
        account: Some("a2".into()),
        model: Some("m2".into()),
        effort: Some("low".into()),
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Messages,
        effective_presentation: "direct".into(),
    });
    assert_eq!(app.session_id.as_deref(), Some("s2"));
    assert_eq!(app.account.as_deref(), Some("a2"));
    assert_eq!(app.effort.as_deref(), Some("low"));
    assert_eq!(app.protocol, crate::tui::ProtocolSelection::Messages);
    app.apply_host_event(HostEvent::ClearView);
    assert_eq!(app.pane.blocks, [] as [ScrollBlock; 0]);
}

#[test]
fn restored_typed_tool_activity_renders_correlated_detail_and_duration() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::Tool(ToolItem {
            subject: Some("cargo test".into()),
            detail: Some("Arguments:\n{}\n\nResult:\nok".into()),
            started_at_ms: Some(1_000),
            finished_at_ms: Some(1_275),
            ..tool_item("call-42", "exec_command")
        })],
    ));
    assert!(matches!(
        app.pane.blocks.as_slice(),
        [ScrollBlock::Tool {
            call_id: Some(call_id),
            name,
            state,
            subject: Some(subject),
            detail: Some(detail),
            ..
        }] if call_id == "call-42"
            && name == "exec_command"
            && state == "done:275ms"
            && subject == "cargo test"
            && detail.contains("Result:\nok")
    ));
}

#[test]
fn restored_presentation_keeps_conversation_tool_and_notice_chronology() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![
            TranscriptRow::user("first"),
            TranscriptRow::Tool(tool_item("call-1", "exec_command")),
            TranscriptRow::info(NoticeScope::Turn, "between"),
            TranscriptRow::assistant("last"),
        ],
    ));

    assert!(matches!(
        app.pane.blocks.as_slice(),
        [
            ScrollBlock::User { text: first, .. },
            ScrollBlock::Tool { call_id: Some(call_id), .. },
            ScrollBlock::System(between),
            ScrollBlock::Assistant(last),
        ] if first == "first" && call_id == "call-1" && between == "between" && last == "last"
    ));
}

#[test]
fn fragmented_assistant_stream_appends_exactly_without_snapshot_clones() {
    let mut app = App::new(launch_basic(), true);
    let fragment = "abcdefghij";
    for _ in 0..10_000 {
        app.apply_host_event(HostEvent::AssistantDelta(fragment.into()));
    }
    app.apply_host_event(HostEvent::AssistantDone);

    let expected = fragment.repeat(10_000);
    assert!(matches!(
        app.pane.blocks.last(),
        Some(ScrollBlock::Assistant(text)) if text == &expected
    ));
    assert_eq!(
        app.pane
            .blocks
            .iter()
            .filter(|block| matches!(block, ScrollBlock::Assistant(_)))
            .count(),
        1
    );
    let revision = app.pane.transcript_revision;
    let _ = app.pane.transcript_cache.get(
        revision,
        &app.pane.blocks,
        &app.theme,
        100,
        TranscriptChange::Structural,
        RenderPrefs::default(),
    );
    let _ = app.pane.transcript_cache.get(
        revision,
        &app.pane.blocks,
        &app.theme,
        100,
        TranscriptChange::Unchanged,
        RenderPrefs::default(),
    );
    assert_eq!(app.pane.transcript_cache.rebuilds(), 1);
}

#[test]
fn live_transcript_retention_bounds_blocks_bytes_and_cache_indices() {
    let mut app = App::new(launch_basic(), true);
    let lines = (0..MAX_LIVE_TRANSCRIPT_BLOCKS + 50)
        .map(|index| TranscriptRow::assistant(format!("row {index}")))
        .collect();
    app.apply_host_event(restore_snapshot(1, lines));
    assert_eq!(app.pane.blocks.len(), MAX_LIVE_TRANSCRIPT_BLOCKS);
    assert_eq!(
        app.pane.blocks.first().map(ScrollBlock::body),
        Some(TRANSCRIPT_TRUNCATION_MARKER)
    );

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let render = app.pane.transcript_cache.get(
        app.pane.transcript_revision,
        &app.pane.blocks,
        &app.theme,
        100,
        TranscriptChange::Unchanged,
        RenderPrefs::default(),
    );
    assert!(
        render
            .layout
            .block_index
            .iter()
            .flatten()
            .all(|index| *index < app.pane.blocks.len())
    );

    app.apply_host_event(restore_snapshot(
        2,
        vec![TranscriptRow::assistant(format!(
            "{}tail",
            "x".repeat(MAX_LIVE_TRANSCRIPT_BYTES + 128)
        ))],
    ));
    assert!(app.pane.blocks.len() <= MAX_LIVE_TRANSCRIPT_BLOCKS);
    assert!(app.pane.transcript_bytes <= MAX_LIVE_TRANSCRIPT_BYTES);
    assert!(app.pane.blocks.last().unwrap().body().ends_with("tail"));
}

#[test]
fn user_and_sticky_prompt_bands_fill_the_complete_testbackend_row() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::user("highlighted prompt")],
    ));
    let backend = TestBackend::new(32, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let transcript = app.transcript_area.unwrap();
    let row = transcript.y;
    let buffer = terminal.backend().buffer();
    assert!((0..transcript.width.saturating_sub(1)).all(|x| {
        let index = usize::from(row) * usize::from(buffer.area.width) + usize::from(x);
        buffer.content()[index].bg == app.theme.user_bg
    }));

    app.apply_host_event(restore_snapshot(
        2,
        vec![
            TranscriptRow::user("highlighted prompt"),
            TranscriptRow::assistant(
                "one\n\ntwo\n\nthree\n\nfour\n\nfive\n\nsix\n\nseven\n\neight\n\nnine\n\nten",
            ),
        ],
    ));
    app.pane.scroll_from_bottom = 0;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.pane.scroll_from_bottom = 1;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let transcript = app.transcript_area.unwrap();
    let row = transcript.y;
    let buffer = terminal.backend().buffer();
    assert!((0..transcript.width.saturating_sub(1)).all(|x| {
        let index = usize::from(row) * usize::from(buffer.area.width) + usize::from(x);
        buffer.content()[index].bg == app.theme.sticky_user_bg
    }));
}

/// A prompt wider than the row loses its tail to a marked clip, never a
/// grapheme cut in half and never the id or age that name the row.
#[test]
fn a_long_unicode_prompt_clips_on_the_row_and_never_the_identity() {
    let mut app = resume_fixture();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let unicode_row = app.resume_hits[1].rect;
    let mut painted = String::new();
    for x in unicode_row.x..unicode_row.x.saturating_add(unicode_row.width) {
        painted.push_str(buffer[(x, unicode_row.y)].symbol());
    }
    assert!(
        painted.contains("7c1d0e94") && painted.contains("3h ago"),
        "identity survives the clip: {painted:?}"
    );
    assert!(
        painted.contains('…'),
        "a clipped summary states its own cut: {painted:?}"
    );
    assert!(
        !painted.contains("very long opening prompt"),
        "the tail is what is dropped: {painted:?}"
    );
    assert_eq!(
        buffer[(unicode_row.x + unicode_row.width, unicode_row.y)].symbol(),
        " ",
        "a wide grapheme never overflows the row it was clipped into"
    );
    assert!(
        painted.ends_with("grok-4.6"),
        "the binding keeps its column against a long prompt: {painted:?}"
    );

    // Wide enough for both halves of the binding, and the summary keeps
    // the emoji and full-width glyphs it opened with.
    let mut app = resume_fixture();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let unicode_row = app.resume_hits[1].rect;
    let mut painted = String::new();
    for x in unicode_row.x..unicode_row.x.saturating_add(unicode_row.width) {
        painted.push_str(buffer[(x, unicode_row.y)].symbol());
    }
    assert!(
        painted.contains("xai · grok-4.6"),
        "a wide row names the account and the model: {painted:?}"
    );
    assert!(
        painted.contains("Résumé the ünicode 🚀"),
        "combining and wide graphemes survive intact: {painted:?}"
    );
    assert!(painted.contains('…'), "and the tail is still marked");
    // The clip counts columns, so a cut beside a wide grapheme still leaves
    // the modal's own border standing to the right of the row.
    assert_eq!(
        buffer[(unicode_row.x + unicode_row.width, unicode_row.y)].symbol(),
        " ",
        "a wide grapheme pushed the row past its rect"
    );
}

/// Hover reveals a row's description without moving focus (spec 02 §5.5).
#[test]
fn hover_reveals_without_moving_focus() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let mouse_row = settings_row(crate::tui::settings_registry::MOUSE);
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == mouse_row)
        .copied()
        .expect("the Mouse capture row is drawn");
    let selected = app.settings_menu.as_ref().expect("settings").selected;
    assert_ne!(selected, mouse_row, "it starts unfocused");

    app.handle_control_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: hit.rect.x,
        row: hit.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.settings_hover, Some(mouse_row));
    assert_eq!(
        app.settings_menu.as_ref().expect("settings").selected,
        selected,
        "hover must not move focus"
    );
    let before = app.settings_menu.as_ref().expect("settings").draft.mouse;

    app.handle_control_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.rect.x,
        row: hit.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.settings_menu.as_ref().expect("settings").selected,
        mouse_row
    );
    assert_eq!(
        app.settings_menu.as_ref().expect("settings").draft.mouse,
        before
    );

    app.handle_control_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.rect.x,
        row: hit.rect.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_ne!(
        app.settings_menu.as_ref().expect("settings").draft.mouse,
        before
    );
}

/// Resolving an effect is an explicit gesture, and an approval the
/// host stops talking about retires locally so the loop can park.
#[test]
fn an_unresolved_approval_retires_when_the_turn_ends() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "approval-9".into(),
        call_id: None,
        tool_name: "exec_command".into(),
        subject: None,
        prompt: None,
        expires_at_ms: 0,
    });
    assert!(app.redraw_demand().animating);
    app.apply_host_event(HostEvent::SetBusy(false));
    assert!(app.pending_approval().is_none());
    assert!(
        !app.redraw_demand().animating,
        "the loop no longer animates for a decision nobody can make"
    );
    assert!(matches!(
        app.pane.blocks.last(),
        Some(ScrollBlock::Approval {
            resolution: Some(text),
            ..
        }) if text == "unresolved · turn ended"
    ));
}

#[test]
fn transcript_starts_without_static_application_banner() {
    let app = App::new(launch_basic(), false);
    assert_eq!(app.pane.blocks, [] as [ScrollBlock; 0]);
}

#[test]
fn completed_long_response_pins_clickable_user_prompt_at_viewport_top() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![
        ScrollBlock::user("inspect this repository".into()),
        ScrollBlock::Assistant(
            (1..=40)
                .map(|index| format!("answer line {index:02}"))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    ];
    app.mark_transcript_replaced();
    let backend = TestBackend::new(60, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let (hit, target) = app
        .sticky_prompt_jump
        .expect("long completed response should retain its prompt header");
    assert!(
        target > 0,
        "jump target must move from response tail to prompt"
    );
    assert_eq!(app.pane.scroll_from_bottom, 0);
    assert!(
        app.handle_mouse(left_click(hit.x.saturating_add(1), hit.y))
            .is_none()
    );
    assert_eq!(app.pane.scroll_from_bottom, target);
}

/// Exactly one blank row after a Compose block, every time.
///
/// The estimated projection charged every tool block a trailing blank,
/// including the open Compose head and its non-last children, so a Compose
/// whose geometry had not been promoted to an exact measurement stood two
/// rows taller than the paint path gives it — and settled from two blank
/// rows to one on a later repaint.
#[test]
fn one_blank_row_follows_a_compose_block_in_every_projection() {
    const WIDTH: usize = 116;
    let mut cache = crate::tui::TranscriptRenderCache::default();
    let mut revision = 0_u64;
    let mut painted = |app: &App, cache: &mut crate::tui::TranscriptRenderCache| {
        revision += 1;
        let _ = cache.get(
            revision,
            &app.pane.blocks,
            &app.theme,
            WIDTH,
            TranscriptChange::Structural,
            app.render_prefs,
        );
        cache.plain().to_vec()
    };
    let gap = |rows: &[String]| {
        let head = rows
            .iter()
            .position(|row| row.contains("CodeMode"))
            .expect("the Compose head is drawn");
        let text = rows
            .iter()
            .position(|row| row.contains("And that is what the run found."))
            .unwrap_or_else(|| panic!("the assistant text is drawn: {rows:#?}"));
        assert!(head < text, "the text follows the Compose block");
        rows[head..text]
            .iter()
            .rev()
            .take_while(|row| row.trim().is_empty())
            .count()
    };

    let mut app = compose_transcript_app();
    say(&mut app, "And that is what the run found.");
    let rows = painted(&app, &mut cache);
    assert_eq!(
        gap(&rows),
        1,
        "exactly one blank row after Compose: {rows:#?}"
    );

    // Folding the whole transcript changes the Compose block's shape but
    // not the one blank row that closes it.
    app.toggle_all_blocks();
    let folded = painted(&app, &mut cache);
    assert_eq!(gap(&folded), 1, "folded too: {folded:#?}");
}

/// The cursor comes back when the wheel burst settles.
///
/// It was correctly hidden on the first wheel event but stayed hidden after
/// scrolling stopped — even back at the bottom with the composer focused —
/// until the operator typed. A drag-select still holds it: that gesture
/// ends on the button release, not on an empty scroll queue.
#[test]
fn the_cursor_returns_when_a_wheel_burst_settles() {
    let mut app = transcript_fixture();
    for index in 0..40 {
        say(&mut app, &format!("Paragraph {index} of a long answer."));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    for _ in 0..6 {
        app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, 10, 2));
    }
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !terminal.backend().cursor_visible(),
        "the burst hides the cursor"
    );

    // Drain the queue the way the event loop does, one frame at a time.
    for _ in 0..64 {
        if app.mouse_scroll_queue == 0 {
            break;
        }
        app.drain_mouse_scroll();
        app.settle_wheel_gesture();
    }
    assert_eq!(app.mouse_scroll_queue, 0, "the burst drained");
    app.last_wheel_at = Some(app.now().checked_sub(Duration::from_millis(110)).unwrap());
    app.settle_wheel_gesture();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        terminal.backend().cursor_visible(),
        "a settled burst hands the cursor back without a keystroke"
    );

    // A drag-select is a different gesture and must not be settled away.
    app.handle_mouse(left_mouse(MouseEventKind::Down(MouseButton::Left), 10, 2));
    app.handle_mouse(left_mouse(MouseEventKind::Drag(MouseButton::Left), 20, 3));
    app.drain_mouse_scroll();
    app.settle_wheel_gesture();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !terminal.backend().cursor_visible(),
        "a drag keeps the cursor hidden until it is released"
    );
}

#[test]
fn transcript_drag_autoscrolls_on_ticks_and_copies_full_plain_projection() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(
        1,
        (0..40)
            .map(|index| TranscriptRow::info(NoticeScope::Turn, format!("row-{index:02}")))
            .collect(),
    ));
    let backend = TestBackend::new(48, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_body_area.expect("transcript body");
    let initial_start = app.pane.transcript_visible_start;
    // Notices sit behind the two-cell gutter; column 4 is the `w` of `row`.
    let x = body.x.saturating_add(4);
    let bottom = body.bottom().saturating_sub(1);
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        x,
        bottom,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        body.y,
    ));

    for _ in 0..5 {
        assert!(app.advance_transcript_drag_autoscroll());
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }
    assert!(
        initial_start.saturating_sub(app.pane.transcript_visible_start) >= 2,
        "each autoscroll tick moves two rendered rows"
    );
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), x, body.y));
    let copied = app
        .take_transcript_selection_text()
        .expect("reverse drag selection");
    assert!(copied.lines().count() > usize::from(body.height));
    assert!(
        copied.ends_with("row"),
        "inclusive end cell comes from tail row"
    );
}

#[test]
fn unicode_transcript_projection_matches_exact_buffer_cells() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![ScrollBlock::System(
        "界🙂e\u{301}  alpha 界🙂e\u{301}  omega".into(),
    )];
    app.mark_transcript_replaced();
    let backend = TestBackend::new(16, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    assert!(app.pane.transcript_cache.plain().len() >= 2);
    assert!(
        app.pane
            .transcript_cache
            .plain()
            .iter()
            .all(|line| line.width() <= 16)
    );
    assert_visible_transcript_rows_match_projection(&app, terminal.backend().buffer());
}

#[test]
fn osc52_hook_encodes_unicode_and_rejects_oversized_payloads() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    assert_eq!(
        osc52_sequence("界\n").as_deref(),
        Some(format!("\x1b]52;c;{}\x07", B64.encode("界\n")).as_str())
    );
    assert!(osc52_sequence(&"x".repeat(50_000)).is_none());
}

#[test]
fn a_running_tool_row_carries_a_static_state_glyph_not_a_spinner() {
    let mut app = transcript_fixture();
    app.pane.blocks = vec![ScrollBlock::tool("exec_command", "running")];
    app.mark_transcript_replaced();
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let transcript = app.transcript_area.unwrap();
    let before = terminal.backend().buffer()[(transcript.x + 2, transcript.y)]
        .symbol()
        .to_owned();
    app.status_spinner_frame += 1;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_ne!(
        terminal.backend().buffer()[(transcript.x + 2, transcript.y)].symbol(),
        before
    );
    app.render_prefs.reduced_motion = true;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let frozen = terminal.backend().buffer()[(transcript.x + 2, transcript.y)]
        .symbol()
        .to_owned();
    app.status_spinner_frame += 1;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        terminal.backend().buffer()[(transcript.x + 2, transcript.y)].symbol(),
        frozen
    );
}

#[test]
fn clipped_expanded_tool_detail_is_not_a_hit_or_spinner_row() {
    let mut app = transcript_fixture();
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "running",
        "0 tools",
        &(0..16)
            .map(|i| format!("const detail_{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n"),
        &[],
    );
    app.toggle_tool_expand_at(0);
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        app.tool_hit_targets
            .iter()
            .all(|target| matches!(target, crate::tui::blocks::RowTarget::CellOutput(_))),
        "source is not clickable; its separate output row is"
    );
    let body = app.transcript_body_area.unwrap();
    let line = (body.x..body.right())
        .map(|x| terminal.backend().buffer()[(x, body.y)].symbol())
        .collect::<String>();
    assert!(
        !line.contains(app.status_spinner()),
        "source rows are not animated: {line}"
    );
    app.pane.scroll_from_bottom = usize::MAX;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.tool_hit_rows.len(), 1);
}

/// Spec 02: the transcript carries no scroll-position badge; the scrollbar
/// shows the position.
#[test]
fn a_scrolled_transcript_paints_no_position_badge() {
    let mut app = App::new(launch_basic(), true);
    for index in 0..60 {
        app.push_block(ScrollBlock::System(format!("note {index}")));
    }
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    for from_bottom in [0, 5, 1_000] {
        app.pane.scroll_from_bottom = from_bottom;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let body = app.transcript_body_area().unwrap();
        let buffer = terminal.backend().buffer();
        for y in body.y..body.bottom() {
            let row = (body.x..body.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            assert!(
                !row.contains('%') && !row.contains("·new"),
                "{from_bottom}: {row}"
            );
        }
    }
}

#[test]
fn tool_progress_marks_one_block_not_the_whole_transcript() {
    let mut app = App::new(launch_basic(), true);
    app.push_block(ScrollBlock::user("run the suite".into()));
    app.apply_host_event(HostEvent::ToolLine {
        call_id: Some("call-1".into()),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: "shell".into(),
        state: "running".into(),
        subject: Some("cargo test".into()),
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let rebuilds = app.pane.transcript_cache.rebuilds();
    let idx = app.pane.blocks.len() - 1;

    app.apply_host_event(HostEvent::ToolLine {
        call_id: Some("call-1".into()),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: "shell".into(),
        state: "running".into(),
        subject: None,
        detail: Some("42 tests".into()),
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    assert_eq!(
        app.pane.pending_transcript_change,
        TranscriptChange::Block(idx),
        "tool progress is one block, not a structural relayout"
    );
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.pane.transcript_cache.rebuilds(),
        rebuilds,
        "a tool progress update must splice, not rebuild"
    );
}

/// A yielded Compose head keeps the duration measured at the `exec`
/// terminal when the host re-emits the head for a late child (§5.8).
#[test]
fn a_late_compose_update_does_not_erase_the_head_duration() {
    let mut app = App::new(launch_basic(), true);
    let compose = |state: &str, subject: &str| HostEvent::ToolLine {
        call_id: Some("cell-1".into()),
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Compose,
        name: "exec".into(),
        state: state.into(),
        subject: Some(subject.to_owned()),
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    };
    app.apply_host_event(compose("running", "2 tools"));
    app.apply_host_event(compose("done", "3 tools"));
    let timed = match app.pane.blocks.last() {
        Some(ScrollBlock::Tool { state, .. }) => state.clone(),
        other => panic!("expected a Compose row, got {other:?}"),
    };
    assert!(timed.starts_with("done:"), "{timed}");
    // The `wait*` continuation's late child update: a bare `done` head.
    app.apply_host_event(compose("done", "5 tools"));
    match app.pane.blocks.last() {
        Some(ScrollBlock::Tool { state, subject, .. }) => {
            assert_eq!(state, &timed, "the recorded duration must survive");
            assert_eq!(subject.as_deref(), Some("5 tools"), "counts still advance");
        }
        other => panic!("expected a Compose row, got {other:?}"),
    }
}

/// Two incorrect states on one row,
/// from a cell that yields,
/// dispatches again under `wait`, and then ends:
///
/// (a) the reopen `running` was ignored, so `✓ Compose` stayed painted over
///     children that were still running (and, after a cancel, over `⊘`);
/// (b) the head's elapsed came from a renderer clock that stopped at the
///     yield, so the live row read `16.2s` where the persisted row — same
///     cell, same call — read `45.3s`. The host's `started_at_ms` is the
///     one authority both paths read.
#[test]
fn a_reopened_compose_head_runs_again_and_reports_the_host_cell_duration() {
    let mut app = App::new(launch_basic(), true);
    let compose = |state: &str, subject: &str, duration_ms: Option<u64>| HostEvent::ToolLine {
        call_id: Some("exec-1".into()),
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Compose,
        name: "exec".into(),
        state: state.into(),
        subject: Some(subject.to_owned()),
        detail: None,
        duration_ms,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    };
    let row_state = |app: &App| match app.pane.blocks.last() {
        Some(ScrollBlock::Tool { state, .. }) => state.clone(),
        other => panic!("expected a Compose row, got {other:?}"),
    };

    app.apply_host_event(compose("running", "1 tools", None));
    // The yield: the marker terminalises 16.2 s after the cell started.
    app.apply_host_event(compose("done", "3 tools", Some(16_200)));
    assert_eq!(row_state(&app), "done:16.2s");

    // (a) The host puts the yielded cell back in flight.
    app.apply_host_event(compose("running", "4 tools", None));
    assert_eq!(
        row_state(&app),
        "running",
        "a Compose head the host says is running paints running"
    );
    match app.pane.blocks.last() {
        Some(ScrollBlock::Tool { subject, .. }) => {
            assert_eq!(subject.as_deref(), Some("4 tools"));
        }
        other => panic!("expected a Compose row, got {other:?}"),
    }

    // (b) The cell ends 45.3 s after it started, and that is the head's
    // number — not the pre-yield fragment, and not the last segment.
    app.apply_host_event(compose("done", "5 tools", Some(45_300)));
    assert_eq!(row_state(&app), "done:45.3s");
}

/// Anchoring must not drift: at the retention cap every
/// appended block shifts every index by one, and the anchor was a raw block
/// index that retention never rebased — so a pinned viewport drifted one
/// block per host event.
#[test]
fn a_pinned_anchor_does_not_drift_when_retention_evicts_head_blocks() {
    let mut app = App::new(launch_basic(), true);
    for index in 0..MAX_LIVE_TRANSCRIPT_BLOCKS {
        app.push_block(ScrollBlock::System(format!("note {index}")));
    }
    assert_eq!(app.pane.blocks.len(), MAX_LIVE_TRANSCRIPT_BLOCKS);
    // Pin the viewport on a block well inside the retained window.
    let anchored = MAX_LIVE_TRANSCRIPT_BLOCKS / 2;
    let body = match &app.pane.blocks[anchored] {
        ScrollBlock::System(text) => text.clone(),
        other => panic!("expected a system block, got {other:?}"),
    };
    app.pane.transcript_view_anchor = Some((anchored, 0));

    for _ in 0..16 {
        app.push_block(ScrollBlock::System("appended".into()));
        let (block, _) = app
            .pane
            .transcript_view_anchor
            .expect("a pinned anchor survives retention");
        assert_eq!(
            match &app.pane.blocks[block] {
                ScrollBlock::System(text) => text.as_str(),
                other => panic!("expected a system block, got {other:?}"),
            },
            body,
            "the anchor must name the same block it was pinned to"
        );
    }
}

/// On a terminal without the kitty keyboard protocol Ctrl+Shift+O
/// arrives as Ctrl+O and runs the *wrong* action, so the toggle-all chord
/// needs a plain-encoding alias and the sheet has to name it.
#[test]
fn alt_o_toggles_every_block_on_a_terminal_without_csi_u() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ToolLine {
        call_id: Some("call-1".into()),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: "shell".into(),
        state: "done".into(),
        subject: Some("cargo test".into()),
        detail: Some("300 passed".into()),
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    let open = |app: &App| match app.pane.blocks.last() {
        Some(ScrollBlock::Tool { group_open, .. }) => *group_open,
        other => panic!("expected a tool row, got {other:?}"),
    };
    assert!(!open(&app));
    let mut alt = key(KeyCode::Char('o'));
    alt.modifiers = KeyModifiers::ALT;
    assert!(app.handle_key(alt).is_none());
    assert!(open(&app), "Alt+O must open every group");
    assert!(app.handle_key(alt).is_none());
    assert!(!open(&app), "and fold everything again");
    assert!(
        crate::tui::keymap::BINDINGS
            .iter()
            .any(|binding| binding.chord == "Alt+O"),
        "the keybinds sheet must document the alias"
    );
    assert!(
        crate::tui::keymap::BINDINGS
            .iter()
            .any(|binding| binding.chord == "Ctrl+Shift+O" && binding.effect.contains("kitty")),
        "the sheet must carry the caveat on the CSI-u chord"
    );
}

#[test]
fn transcript_bytes_stay_exact_without_rescanning_every_block() {
    let mut app = App::new(launch_basic(), true);
    for index in 0..64 {
        app.push_block(ScrollBlock::System(format!("note {index}")));
    }
    app.apply_host_event(HostEvent::ToolLine {
        call_id: Some("call-1".into()),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: "shell".into(),
        state: "running".into(),
        subject: Some("cargo test".into()),
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    app.apply_host_event(HostEvent::ToolLine {
        call_id: Some("call-1".into()),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: "shell".into(),
        state: "done".into(),
        subject: Some("cargo test".into()),
        detail: Some("300 passed".into()),
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    app.apply_host_event(HostEvent::AssistantDelta("streamed".into()));
    app.apply_host_event(HostEvent::AssistantDelta(" tail".into()));
    let summed: usize = app
        .pane
        .blocks
        .iter()
        .map(ScrollBlock::retained_bytes)
        .sum();
    assert_eq!(
        app.pane.transcript_bytes, summed,
        "incremental accounting must match a full re-sum"
    );
}

/// Benchmark-shaped, but deterministic: no wall-clock assertion, only a
/// count of full layout passes against the number of frames drawn.
#[test]
fn ten_thousand_blocks_and_a_thousand_deltas_cost_at_most_one_pass_per_frame() {
    const HISTORY: usize = 10_000;
    const DELTAS: usize = 1_000;
    const DELTAS_PER_FRAME: usize = 8;
    let frames = DELTAS / DELTAS_PER_FRAME;

    let mut app = App::new(launch_basic(), true);
    let history = (0..HISTORY)
        .map(|index| TranscriptRow::assistant(format!("history row {index}")))
        .collect();
    app.apply_host_event(restore_snapshot(1, history));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let baseline = app.pane.transcript_cache.rebuilds();

    for _ in 0..frames {
        for _ in 0..DELTAS_PER_FRAME {
            app.apply_host_event(HostEvent::AssistantDelta("token ".into()));
        }
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    let passes = app.pane.transcript_cache.rebuilds() - baseline;
    assert!(
        passes <= frames,
        "{passes} layout passes over {frames} frames: one pass per frame is the ceiling"
    );
    assert!(
        passes <= 1,
        "{passes} layout passes: streaming must splice the tail, not relayout the history"
    );
}

/// Spec 02 §5.5 `tui.scroll_lines`: the row moves the wheel, and previews.
///
/// The wheel handler carried its own 3/5 literals, so the row previewed and
/// persisted a number nothing ever read.
#[test]
fn the_scroll_lines_row_sets_the_wheel_notch() {
    let mut app = App::new(launch_basic(), true);
    // Pinned, so the second notch below is inside the flick window however
    // long the machine takes between the two calls.
    pin_clock(&mut app);
    app.enqueue_transcript_scroll(false);
    assert_eq!(
        app.mouse_scroll_queue,
        -i16::try_from(super::WHEEL_NOTCH_DEFAULT).unwrap(),
        "with no snapshot the notch is the schema default"
    );

    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot {
            scroll_lines: 7,
            ..SettingsSnapshot::default()
        },
    )));
    app.mouse_scroll_queue = 0;
    app.last_wheel_at = None;
    app.enqueue_transcript_scroll(true);
    assert_eq!(app.mouse_scroll_queue, 7, "the configured notch is used");
    app.mouse_scroll_queue = 0;
    // A second notch inside the fast window flicks further, from the
    // configured notch rather than from a literal.
    app.enqueue_transcript_scroll(false);
    assert_eq!(
        app.mouse_scroll_queue,
        -(7 + super::WHEEL_FAST_BONUS),
        "the flick bonus is relative to the configured notch"
    );

    // The open draft previews before it is saved.
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app.settings_menu.as_mut().unwrap().draft.scroll_lines = 10;
    assert_eq!(app.wheel_notch(false), 10);
}

/// The close glyph is `[×]` at the top right, so the cross sits in the
/// middle of the brackets. It brightens under the pointer and closes the
/// Menu when clicked.
#[test]
fn the_close_glyph_brightens_on_hover_and_closes_on_click() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.activate_control_tab(super::ControlTab::Keybinds);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let cold = serialize_frame(terminal.backend().buffer());
    assert!(cold.contains("[\u{00d7}]"), "{cold}");
    assert!(
        !cold.contains("[x]"),
        "a latin x sits too low in the brackets: {cold}"
    );

    let close = app.control_close_hit.expect("the close glyph is painted");
    app.handle_control_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: close.x + 1,
        row: close.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(app.control_close_hover, "hover is tracked");
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let hot = serialize_frame(terminal.backend().buffer());
    assert_ne!(cold, hot, "the glyph must brighten under the pointer");
    let path = "tests/goldens/menu-close-hover-120x40.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(path, &hot).expect("write reviewed frame golden");
    } else {
        assert_eq!(
            hot,
            include_str!("../../../tests/goldens/menu-close-hover-120x40.frame")
        );
    }

    app.handle_control_mouse(left_click(close.x + 1, close.y));
    assert_eq!(app.control_tab, None, "the click closes");
}

/// A transcript scrolled to the very top, with the session banner occupying
/// the first rows of the document.
fn banner_top_app() -> (App, Terminal<TestBackend>) {
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    app.pane.blocks = (0..40)
        .map(|index| ScrollBlock::System(format!("row-{index:02}")))
        .collect();
    app.mark_transcript_replaced();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.pane.scroll_from_bottom = usize::MAX;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.pane.transcript_visible_start, 0,
        "the camera must be parked at the top of the document"
    );
    assert_eq!(
        visible_banner_rows(&app),
        crate::tui::launch_intro::BANNER_HEIGHT,
        "the whole banner must be on screen at the top of the document"
    );
    (app, terminal)
}

/// Rows of the session banner still on screen: the body is pushed down by
/// exactly the banner rows the camera has not scrolled past.
fn visible_banner_rows(app: &App) -> u16 {
    app.transcript_body_area
        .expect("body")
        .y
        .saturating_sub(app.transcript_area.expect("transcript").y)
}

/// The wheel must climb out of the session banner.
///
/// The banner is a fixed-height prefix of the document, so a viewport that
/// starts inside it has no message row to anchor on. The camera used to be
/// remapped to document row zero every frame and only this frame's notch was
/// re-applied, so the view snapped back and the wheel read as locked.
#[test]
fn a_wheel_down_at_the_top_scrolls_out_of_the_session_banner() {
    let (mut app, mut terminal) = banner_top_app();

    for _ in 0..6 {
        app.enqueue_transcript_scroll(false);
        app.drain_mouse_scroll();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    assert!(
        app.pane.transcript_visible_start > 0,
        "wheeling down from the top must scroll the first content rows away"
    );
    assert_eq!(
        visible_banner_rows(&app),
        0,
        "the banner must leave the pane once it is scrolled past"
    );
}

/// The Down key takes the same camera path as the wheel.
#[test]
fn the_down_key_at_the_top_scrolls_out_of_the_session_banner() {
    let (mut app, mut terminal) = banner_top_app();

    for _ in 0..16 {
        app.handle_key(key(KeyCode::Down));
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    assert!(
        app.pane.transcript_visible_start > 0,
        "the Down key must move the camera past the banner"
    );
    assert_eq!(
        visible_banner_rows(&app),
        0,
        "the banner must leave the pane once it is scrolled past"
    );
}

/// A camera parked inside the banner holds its row while the tail grows.
#[test]
fn a_viewport_inside_the_banner_holds_its_row_while_the_stream_grows() {
    let (mut app, mut terminal) = banner_top_app();
    app.enqueue_transcript_scroll(false);
    app.drain_mouse_scroll();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let banner_rows = visible_banner_rows(&app);
    let start = app.pane.transcript_visible_start;
    assert_eq!(
        banner_rows,
        crate::tui::launch_intro::BANNER_HEIGHT - super::WHEEL_NOTCH_DEFAULT,
        "one notch scrolls exactly one notch of banner off the top"
    );

    for _ in 0..12 {
        app.apply_host_event(HostEvent::AssistantDelta(
            "streamed line of assistant text\n".into(),
        ));
    }
    terminal.draw(|frame| app.draw(frame)).unwrap();

    assert_eq!(
        app.pane.transcript_visible_start, start,
        "a growing tail must not drag a camera parked in the banner"
    );
    assert_eq!(
        visible_banner_rows(&app),
        banner_rows,
        "the visible banner rows must not change either"
    );
}

/// Wheel intent the paint has already refused is dropped at both ends.
///
/// The queue is drained a few rows a frame and caps at 30, so notches sent
/// after the camera reached an end used to have to be burned off before a
/// reversal moved anything.
#[test]
fn wheel_intent_past_an_end_is_not_banked_against_the_reversal() {
    let (mut app, mut terminal) = banner_top_app();
    for _ in 0..8 {
        app.enqueue_transcript_scroll(true);
    }
    app.drain_mouse_scroll();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.mouse_scroll_queue, 0,
        "a paint clamped at the top must drop further up-intent"
    );

    let before = visible_banner_rows(&app);
    assert_eq!(
        before,
        crate::tui::launch_intro::BANNER_HEIGHT,
        "the camera is still parked at the top of the banner"
    );
    // Outside the flick window, so the notch is the plain configured one and a
    // single drain spends all of it.
    app.last_wheel_at = None;
    app.enqueue_transcript_scroll(false);
    app.drain_mouse_scroll();
    assert_eq!(app.mouse_scroll_queue, 0, "one drain spends one notch");
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        visible_banner_rows(&app),
        before - super::WHEEL_NOTCH_DEFAULT,
        "the first notch after the reversal must move the camera one whole notch"
    );

    app.pane.scroll_from_bottom = 0;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    for _ in 0..8 {
        app.enqueue_transcript_scroll(false);
    }
    app.drain_mouse_scroll();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.mouse_scroll_queue, 0,
        "a paint clamped at the tail must drop further down-intent"
    );
}

/// A stored auto-continue row (written before the host stopped writing them)
/// renders nothing: the woken turn's reply is the only block.
#[test]
fn a_stored_auto_continue_row_renders_nothing() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Row(TranscriptRow::AutoContinue(
        plakous_host_protocol::views::transcript::AutoContinue {
            agents_finished: 2,
            processes_exited: 0,
            agents: vec!["c1".into(), "c2".into()],
        },
    )));
    app.apply_host_event(HostEvent::AssistantDelta("Continuing the task.".into()));
    app.apply_host_event(HostEvent::AssistantDone);
    assert!(
        matches!(
            app.pane.blocks.as_slice(),
            [ScrollBlock::Assistant(text)] if text == "Continuing the task."
        ),
        "{:?}",
        app.pane.blocks
    );
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
}

/// One story over the v2 grammar: a prompt, a sealed thought that ends no
/// group of its own, and a live group of two finished calls under one
/// summary plus a tip per call in flight (two parallel commands and a code
/// cell naming its nested call). Live and restored paint the same frame.
#[test]
fn live_tips_and_thinking_match_restored_at_every_width() {
    let tool = |id: &str, name: &str, kind: ActivityKind, state: &str, subject: &str| {
        tool_line(id, None, None, kind, name, state, Some(subject))
    };
    let (theme_name, theme) = ("dark", crate::tui::Theme::dark());
    for width in [120, 40] {
        let mut live = transcript_fixture();
        live.theme = theme.clone();
        live.render_prefs.reduced_motion = true;
        live.render_prefs.show_thinking = true;
        live.apply_host_event(HostEvent::UserMessage {
            text: "Ship it.".into(),
            sent_at_ms: None,
        });
        live.apply_host_event(HostEvent::ReasoningDelta {
            location_key: "r".into(),
            text: "check the build first".into(),
        });
        live.apply_host_event(HostEvent::ReasoningSealed {
            location_key: "r".into(),
        });
        if let ScrollBlock::Thinking { elapsed_ms, .. } = &mut live.pane.blocks[1] {
            *elapsed_ms = Some(2_100);
        }
        live.apply_host_event(tool(
            "read",
            "read_file",
            ActivityKind::Tool,
            "done",
            "src/lib.rs",
        ));
        live.apply_host_event(tool("grep", "grep", ActivityKind::Tool, "done", "needle"));
        live.apply_host_event(tool(
            "test",
            "exec_command",
            ActivityKind::Process,
            "running",
            "cargo test --workspace",
        ));
        live.apply_host_event(tool(
            "build",
            "exec_command",
            ActivityKind::Process,
            "running",
            "cargo build --release && echo \"built ok\"",
        ));
        super::test_support::compose_cell(
            &mut live,
            "cell",
            "exec_js",
            "running",
            "1 tools",
            "await tools.exec_command({cmd: 'pwd'});",
            &[("exec_command", "running", "pwd", "RAW OUTPUT")],
        );
        let running = |id: &str, name: &str, subject: &str| {
            TranscriptRow::Tool(ToolItem {
                subject: Some(subject.into()),
                state: "running".into(),
                ..tool_item(id, name)
            })
        };
        let mut restored = transcript_fixture();
        restored.theme = theme.clone();
        restored.render_prefs = live.render_prefs;
        restored.apply_host_event(restore_snapshot(
            1,
            vec![
                TranscriptRow::user("Ship it."),
                TranscriptRow::Thinking(plakous_host_protocol::views::transcript::ThinkingRow {
                    location_key: "r".into(),
                    text: "check the build first".into(),
                    elapsed_ms: Some(2_100),
                }),
                TranscriptRow::Tool(ToolItem {
                    subject: Some("src/lib.rs".into()),
                    ..tool_item("read", "read_file")
                }),
                TranscriptRow::Tool(ToolItem {
                    subject: Some("needle".into()),
                    ..tool_item("grep", "grep")
                }),
                running("test", "exec_command", "cargo test --workspace"),
                running(
                    "build",
                    "exec_command",
                    "cargo build --release && echo \"built ok\"",
                ),
                TranscriptRow::Tool(ToolItem {
                    subject: Some("1 tools".into()),
                    detail: Some("await tools.exec_command({cmd: 'pwd'});".into()),
                    state: "running".into(),
                    cell_id: Some("cell".into()),
                    ..tool_item("cell", "exec_js")
                }),
                TranscriptRow::Tool(ToolItem {
                    subject: Some("pwd".into()),
                    state: "running".into(),
                    parent_call_id: Some("cell".into()),
                    cell_id: Some("cell".into()),
                    ..tool_item("cell-child-0", "exec_command")
                }),
            ],
        ));
        let paint = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            serialize_frame(terminal.backend().buffer())
        };
        assert_eq!(
            paint(&mut live),
            paint(&mut restored),
            "{theme_name} {width}"
        );
        let plain = live.pane.transcript_cache.plain().join("\n");
        assert!(
            live.pane
                .transcript_cache
                .row_plans()
                .iter()
                .any(|plan| plan
                    .activity_summary
                    .as_deref()
                    .is_some_and(|s| s.contains("1 thought"))),
            "{plain}"
        );
        assert!(plain.contains("✓ Read 1 file, ran 1 search"), "{plain}");
        assert!(plain.contains("$ cargo test"), "{plain}");
        assert!(
            plain.contains("CodeMode · 1 call") && plain.contains("$ pwd"),
            "{plain}"
        );
        assert!(!plain.contains("needle"), "{plain}");
        assert!(!plain.contains("RAW"), "{plain}");
        for row in live.pane.transcript_cache.plain() {
            assert!(row.width() <= usize::from(width), "{row}");
        }
        activity_frame(&mut live, theme_name, width, "tips");
    }
}

#[test]
fn a_command_item_colours_dollar_program_arguments_and_strings() {
    use crate::tui::theme::{Theme, ThemeId};
    for id in ThemeId::ALL {
        let mut app = transcript_fixture();
        app.theme = Theme::for_id(id);
        app.apply_host_event(tool_line(
            "run",
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "done",
            Some("grep -n \"needle\" src"),
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let body = app.transcript_body_area.unwrap();
        let row: String = (body.x..body.right())
            .map(|x| buffer[(x, body.y)].symbol().to_owned())
            .collect();
        let at = |needle: &str| {
            let column = row.find(needle).expect(needle);
            let x = body.x + u16::try_from(row[..column].width()).unwrap();
            buffer[(x, body.y)].fg
        };
        assert_eq!(at("$"), app.theme.command_prompt.fg.unwrap(), "{id:?}");
        assert_eq!(at("grep"), app.theme.command_program.fg.unwrap(), "{id:?}");
        assert_eq!(at("-n"), app.theme.command_arg.fg.unwrap(), "{id:?}");
        assert_eq!(
            at("\"needle\""),
            app.theme.command_string.fg.unwrap(),
            "{id:?}"
        );
    }
}

#[test]
fn a_live_tip_paints_its_elapsed_without_replanning_the_group() {
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    for index in 0..200 {
        app.apply_host_event(tool_line(
            &format!("read-{index}"),
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "done",
            Some(&format!("src/file_{index}.rs")),
        ));
    }
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "running",
        Some("cargo test"),
    ));
    let started = app.now().checked_sub(Duration::from_secs(5)).unwrap();
    app.pane.tool_started.insert("run".into(), started);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let plain = app.pane.transcript_cache.plain().to_vec();
    assert_eq!(plain.iter().filter(|row| !row.trim().is_empty()).count(), 2);
    assert!(plain[0].contains("Read 200 files"), "{plain:?}");
    crate::tui::activity::PLAN_CALLS.set(0);
    for frame in 0..30 {
        app.status_spinner_frame = frame;
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }
    assert_eq!(
        crate::tui::activity::PLAN_CALLS.get(),
        0,
        "a 200-call group must not be replanned per frame"
    );
    let screen = serialize_frame(terminal.backend().buffer());
    // Five seconds have elapsed when the 30 draws start; under load they can
    // take a few more.
    assert!(
        (5..=9).any(|seconds| screen.contains(&format!("{seconds}s·▸"))),
        "{screen}"
    );
}

#[test]
fn the_queue_panel_sits_on_the_transcript_background_in_every_theme() {
    use crate::tui::theme::{Theme, ThemeId};
    for id in ThemeId::ALL {
        let mut app = App::new(launch_basic(), true);
        app.theme = Theme::for_id(id);
        app.apply_host_event(HostEvent::QueueSnapshot {
            items: vec![queue_item("q-theme", 1, "queued follow-up")],
            drain: QueueDrainState::LiveAutoDrain,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let area = app.queue_area.expect("queue panel drawn");
        let buffer = terminal.backend().buffer();
        let background = app.theme.app_bg;
        for y in area.y + 1..area.bottom() - 1 {
            for x in area.x + 1..area.right() - 1 {
                assert_eq!(buffer[(x, y)].bg, background, "{id:?} at {x},{y}");
            }
        }
        let row: String = (area.x..area.right())
            .map(|x| buffer[(x, area.y + 1)].symbol().to_owned())
            .collect();
        let column = row.find("queued").expect("preview painted");
        let x = area.x + u16::try_from(row[..column].width()).unwrap();
        assert_eq!(
            buffer[(x, area.y + 1)].fg,
            id.colors().text.terminal(),
            "{id:?}"
        );
    }
}

// Transcript v2 review regressions (spec 02 §5.4).

fn failed_group(width: u16) -> (App, Terminal<TestBackend>) {
    let mut app = transcript_fixture();
    app.apply_host_event(HostEvent::UserMessage {
        text: "Inspect.".into(),
        sent_at_ms: None,
    });
    app.apply_host_event(tool_line(
        "read",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("a.rs"),
    ));
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "failed:exit 7",
        Some("cargo test"),
    ));
    app.apply_host_event(tool_line(
        "edit",
        None,
        None,
        ActivityKind::Tool,
        "apply_patch",
        "done",
        Some("crates/plak/tui/src/blocks.rs +12/-3"),
    ));
    say(&mut app, "Finished.");
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    (app, terminal)
}

#[test]
fn a_narrow_failure_summary_keeps_its_failure_count() {
    let (app, _terminal) = failed_group(40);
    let plain = app.pane.transcript_cache.plain().join("\n");
    assert!(
        plain.contains("(1 failed)"),
        "spec 02 §5.4: failures are never hidden by a fold; got:\n{plain}"
    );
}

#[test]
fn copying_summary_item_and_thinking_rows_carries_no_padding_or_caret() {
    let (mut app, mut terminal) = failed_group(100);
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "think".into(),
        text: "weighing it".into(),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "think".into(),
    });
    for block in &mut app.pane.blocks {
        if let ScrollBlock::Thinking { elapsed_ms, .. } = block {
            *elapsed_ms = Some(2_100);
        }
    }
    say(&mut app, "Done.");
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_body_area.unwrap();
    let last = u16::try_from(app.pane.transcript_cache.plain().len()).unwrap() - 1;
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.right() - 1,
        body.y + last,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.right() - 1,
        body.y + last,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let copied = app.take_transcript_selection_text().expect("copied");
    assert!(
        copied
            .lines()
            .any(|line| line.starts_with("  ✗ Read 1 file")
                && line.contains("(1 failed)")
                && line.ends_with(" · exit 7")),
        "the summary copies whole:\n{copied}"
    );
    assert!(
        copied.lines().any(|line| line == "◆ weighing it"),
        "the thinking row copies whole:\n{copied}"
    );
    assert!(
        !copied.contains("          ") && !copied.contains('▸'),
        "copy carries no layout padding or caret:\n{copied}"
    );
}

#[test]
fn a_long_running_command_tip_still_shows_its_elapsed() {
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(HostEvent::UserMessage {
        text: "Test.".into(),
        sent_at_ms: None,
    });
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "running",
        Some(
            "cd /home/user/dev/project && cargo nextest run --workspace --all-features --profile offline -E 'package(plak)'",
        ),
    ));
    let started = app.now().checked_sub(Duration::from_secs(5)).unwrap();
    app.pane.tool_started.insert("run".into(), started);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = serialize_frame(terminal.backend().buffer());
    assert!(
        screen.contains("5s·▸"),
        "every tip shows spinner and elapsed; a long command shows none:\n{screen}"
    );
}

#[test]
fn a_restored_running_tip_shows_its_elapsed_like_live() {
    let mut restored = transcript_fixture();
    restored.render_prefs.reduced_motion = true;
    restored.apply_host_event(restore_snapshot(
        1,
        vec![
            TranscriptRow::user("Test."),
            TranscriptRow::Tool(ToolItem {
                subject: Some("cargo test".into()),
                state: "running".into(),
                started_at_ms: Some(PINNED_UNIX_MS - 5_000),
                ..tool_item("run", "exec_command")
            }),
        ],
    ));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| restored.draw(frame)).unwrap();
    let screen = serialize_frame(terminal.backend().buffer());
    assert!(
        screen.contains("5s·▸"),
        "live tips paint elapsed; a restored (/reload) tip with the host's started_at_ms shows none:\n{screen}"
    );
}
