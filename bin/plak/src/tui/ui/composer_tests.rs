//! Composer, slash palette, and prompt history tests.

use super::terminal::{commit_sent_action, dispatch_action};
use super::test_support::{
    assert_visible_transcript_rows_match_projection, key, key_ctrl, launch_basic, left_click,
    left_mouse, pin_clock, queue_item, render_stream_slope, render_to_string, say, screen_text,
    transcript_fixture, typed, visual_fixture,
};
use super::{App, DisplayMode};
use crate::tui::blocks::{ScrollBlock, layout_anchor};
use crate::tui::presentation::{ActivityKind, ContextUsage, PreparedInputUsage};
use crate::tui::{HostEvent, QueueDrainState, UserAction, user_action_channel};
use crossterm::event::KeyEvent;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::fmt::Write as _;
use std::time::Duration;
use unicode_width::UnicodeWidthStr as _;

#[test]
fn mark_dirty_does_not_mutate_composer_gestures_under_a_modal() {
    let mut app = App::new(launch_basic(), true);
    app.open_control_tab(super::ControlTab::Keybinds);
    app.composer_drag_pointer = Some((3, 4));
    app.composer_down_cell = Some((1, 2));
    app.composer_dragging = true;
    app.mark_dirty();
    assert_eq!(app.composer_drag_pointer, Some((3, 4)));
    assert_eq!(app.composer_down_cell, Some((1, 2)));
    assert!(app.composer_dragging);
    assert!(!app.advance_composer_drag_autoscroll());
    assert!(app.composer_drag_pointer.is_none());
}

#[test]
fn opening_a_modal_during_composer_drag_stops_selection_even_if_release_is_swallowed() {
    for control_center in [false, true] {
        let mut app = App::new(launch_basic(), true);
        app.set_input((0..40).map(|_| "abcdef").collect::<Vec<_>>().join("\n"));
        app.input_cursor = 0;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let inner = app.composer_inner.unwrap();
        let x = inner.x + 2;
        app.handle_mouse(left_click(x, inner.y));
        app.handle_mouse(left_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            x + 3,
            inner.bottom(),
        ));
        assert!(app.advance_composer_drag_autoscroll());
        if control_center {
            app.open_control_tab(super::ControlTab::Keybinds);
        } else {
            app.handle_key(key_ctrl(KeyCode::Char('t')));
        }
        assert!(app.a_modal_owns_input());
        let scroll = app.composer_visual_scroll;
        let selected = app.composer_selection.range(&app.input);
        assert!(!app.advance_composer_drag_autoscroll());
        app.handle_mouse(left_mouse(
            MouseEventKind::Up(MouseButton::Left),
            x + 3,
            inner.bottom(),
        ));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.advance_composer_drag_autoscroll());
        assert_eq!(app.composer_visual_scroll, scroll);
        assert_eq!(app.composer_selection.range(&app.input), selected);
        assert!(app.composer_drag_pointer.is_none());
        assert!(app.composer_down_cell.is_none());
        assert!(!app.composer_dragging);
    }
}

#[test]
fn switching_panes_clears_a_composer_drag() {
    let mut app = App::new(launch_basic(), true);
    app.set_input((0..40).map(|_| "abcdef").collect::<Vec<_>>().join("\n"));
    app.input_cursor = 0;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.unwrap();
    app.handle_mouse(left_click(inner.x + 2, inner.y));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        inner.x + 2,
        inner.bottom(),
    ));
    app.open_agent_view("c1".into(), "worker".into());
    assert!(app.composer_drag_pointer.is_none());
    assert!(app.composer_down_cell.is_none());
    assert!(!app.composer_dragging);
}

#[test]
fn composer_drag_autoscrolls_to_both_ends_and_copies_the_full_range() {
    let mut app = App::new(launch_basic(), true);
    let draft = (0..40).map(|_| "abcdef").collect::<Vec<_>>().join("\n");
    app.set_input(draft.clone());
    app.input_cursor = 0;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.unwrap();
    let x = inner.x + 2;
    app.handle_mouse(left_click(x, inner.y));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x + 6,
        inner.bottom(),
    ));
    for _ in 0..40 {
        app.advance_composer_drag_autoscroll();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }
    assert_eq!(
        app.composer_selection.range(&draft),
        Some(0..draft.chars().count())
    );
    let mut copied = draft.clone();
    assert_eq!(
        app.composer_selection.clone().cut(&mut copied),
        Some(draft.clone())
    );
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        x + 6,
        inner.bottom(),
    ));
    assert!(!app.advance_composer_drag_autoscroll());

    app.handle_mouse(left_click(x + 6, inner.bottom() - 1));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        inner.y - 1,
    ));
    for _ in 0..40 {
        app.advance_composer_drag_autoscroll();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }
    assert_eq!(
        app.composer_selection.range(&draft),
        Some(0..draft.chars().count())
    );
}

#[test]
fn composer_click_and_return_inside_stop_drag_autoscroll() {
    let mut app = App::new(launch_basic(), true);
    app.set_input((0..40).map(|_| "abcdef").collect::<Vec<_>>().join("\n"));
    app.input_cursor = 0;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.unwrap();
    let x = inner.x + 2;
    app.handle_mouse(left_click(x, inner.y));
    assert!(!app.advance_composer_drag_autoscroll());
    assert!(!app.redraw_demand().pointer_drag);
    assert_eq!(app.composer_visual_scroll, 0);
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        inner.bottom(),
    ));
    assert!(app.redraw_demand().pointer_drag);
    assert!(app.advance_composer_drag_autoscroll());
    assert_eq!(app.composer_visual_scroll, 1);
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        inner.y + 1,
    ));
    assert!(!app.advance_composer_drag_autoscroll());
    assert_eq!(app.composer_visual_scroll, 1);
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        x,
        inner.y + 1,
    ));
    assert!(!app.advance_composer_drag_autoscroll());
    assert!(!app.redraw_demand().pointer_drag);
}

#[test]
fn composer_mouse_drag_and_double_click_drive_selection_first_edits() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("hello world".into());
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.expect("composer interior");
    let x = inner.x.saturating_add(2);
    let y = inner.y;
    app.handle_mouse(left_click(x, y));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x.saturating_add(5),
        y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        x.saturating_add(5),
        y,
    ));
    assert_eq!(app.composer_selection.range(&app.input), Some(0..5));
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.input, " world");

    app.set_input("hello world".into());
    app.handle_mouse(left_click(x.saturating_add(7), y));
    app.handle_mouse(left_click(x.saturating_add(7), y));
    assert_eq!(app.composer_selection.range(&app.input), Some(6..11));
    app.handle_key(key(KeyCode::Delete));
    assert_eq!(app.input, "hello ");
}

#[test]
fn composer_geometry_has_two_row_floor_and_thirty_five_percent_cap() {
    let mut app = App::new(launch_basic(), true);
    let backend = TestBackend::new(80, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.composer_inner.expect("empty composer").height, 2);

    app.set_input(
        (0..80)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.composer_inner.expect("capped composer").height, 12);
}

#[test]
fn small_bottom_stack_preserves_the_composer_before_optional_panels() {
    for height in [8, 10, 12] {
        let mut app = App::new(launch_basic(), true);
        app.replace_queue_snapshot(
            vec![queue_item("q-1", 1, "queued")],
            QueueDrainState::RestoredPaused,
        );
        app.prompt_history = vec!["history".into()];
        app.history_browse = Some(0);
        app.set_input("draft".into());
        let backend = TestBackend::new(40, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let composer = app.composer_inner.expect("composer remains visible");
        assert_eq!(composer.height, 2, "terminal height {height}");
        assert_eq!(
            composer.bottom(),
            height.saturating_sub(if height > 4 { 2 } else { 1 })
        );
    }
}

#[test]
fn exact_width_caret_wraps_inside_the_composer() {
    use ratatui::backend::Backend as _;
    let mut app = App::new(launch_basic(), true);
    let backend = TestBackend::new(10, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.set_input("abcdef".into());
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.expect("composer");
    let wrap_width = usize::from(inner.width.saturating_sub(2));
    let (column, row) = super::visual_cursor_xy(&app.input, wrap_width, app.input_cursor);
    assert_eq!((column, row), (0, 1));
    assert!(inner.x.saturating_add(2) < inner.right());
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        (inner.x + 2, inner.y + 1).into()
    );
}

#[test]
fn painted_caret_owns_hard_line_ends_but_not_soft_wrap_ends() {
    use ratatui::backend::Backend as _;
    for (text, cursor, col, row) in [
        ("hello\nworld", 5, 5, 0),
        ("hello\nworld", 6, 0, 1),
        ("hello\r\nworld", 5, 5, 0),
        ("hello\r\nworld", 7, 0, 1),
        ("abcdefgh", 6, 0, 1),
        ("abcdef", 6, 0, 1),
    ] {
        let mut app = App::new(launch_basic(), true);
        app.input = text.into();
        app.input_cursor = cursor;
        let mut terminal = Terminal::new(TestBackend::new(10, 12)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let inner = app.composer_inner.unwrap();
        assert!(terminal.backend().cursor_visible());
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            (inner.x + 2 + col, inner.y + row).into(),
            "{text:?} at {cursor}"
        );
    }
}

#[test]
fn stray_drag_keeps_the_composer_caret_and_transcript_click_waits_for_a_key() {
    use ratatui::backend::Backend as _;
    let mut app = transcript_fixture();
    app.set_input("draft".into());
    say(&mut app, "a selectable transcript row");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let expected = terminal.backend_mut().get_cursor_position().unwrap();
    app.handle_mouse(left_mouse(MouseEventKind::Drag(MouseButton::Left), 10, 2));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(terminal.backend().cursor_visible());
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        expected
    );
    app.handle_mouse(left_click(10, 2));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 10, 2));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(!terminal.backend().cursor_visible());
    app.handle_key(key(KeyCode::Left));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(terminal.backend().cursor_visible());
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        (expected.x - 1, expected.y).into()
    );
}

#[test]
fn capped_composer_keys_move_caret_and_extend_selection_by_visual_rows() {
    let mut app = App::new(launch_basic(), true);
    app.set_input(
        (0..12)
            .map(|index| format!("r{index:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let backend = TestBackend::new(24, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let end = app.input_cursor;
    assert!(app.composer_visual_scroll > 0);

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input_cursor, end - 4);
    let mut shifted_up = key(KeyCode::Up);
    shifted_up.modifiers = KeyModifiers::SHIFT;
    app.handle_key(shifted_up);
    assert_eq!(
        app.composer_selection.range(&app.input),
        Some(end - 8..end - 4)
    );

    app.handle_key(key(KeyCode::PageUp));
    assert!(app.input_cursor < end - 7);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let (_, caret_row) = super::visual_cursor_xy(
        &app.input,
        usize::from(app.composer_inner.unwrap().width.saturating_sub(2)),
        app.input_cursor,
    );
    assert!(caret_row >= app.composer_visual_scroll);
    assert!(
        caret_row < app.composer_visual_scroll + usize::from(app.composer_inner.unwrap().height)
    );

    app.handle_key(key(KeyCode::PageDown));
    assert!(app.input_cursor > 0);
}

#[test]
fn capped_composer_wheel_scrolls_internally_and_click_uses_scrolled_geometry() {
    let mut app = App::new(launch_basic(), true);
    app.set_input(
        (0..20)
            .map(|index| format!("r{index:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let backend = TestBackend::new(24, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.expect("composer");
    let tail_scroll = app.composer_visual_scroll;
    app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, inner.x, inner.y));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.composer_visual_scroll, tail_scroll.saturating_sub(3));

    let visible_start = app.composer_visual_scroll;
    app.handle_mouse(left_click(inner.x.saturating_add(2), inner.y));
    assert_eq!(app.input_cursor, visible_start * 4);
    assert!(
        app.pane.scroll_from_bottom == 0,
        "composer wheel must not scroll transcript"
    );
}

#[test]
fn slash_palette_matches_composer_outer_geometry_and_touches_its_top_edge() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("/".into());
    app.sync_slash_open();
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.expect("composer");
    let palette = app.slash_popup.expect("slash palette");
    assert_eq!(palette.x, inner.x.saturating_sub(1));
    assert_eq!(palette.width, inner.width.saturating_add(2));
    assert_eq!(palette.bottom(), inner.y.saturating_sub(1));
}

/// Spec 02 §5.5 mouse parity: a click on a slash entry selects it; a
/// double-click runs it exactly as Enter does.
#[test]
fn a_slash_entry_selects_on_click_and_runs_like_enter_on_double_click() {
    let open = || {
        let mut app = App::new(launch_basic(), true);
        app.set_input("/".into());
        app.sync_slash_open();
        app
    };
    let mut app = open();
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let popup = app.slash_popup.expect("slash popup");
    let (x, y) = (popup.x + 2, popup.y + 2);
    let index = app.slash_view_start + 1;
    let click = left_click(x, y);
    assert_eq!(app.handle_mouse(click), None);
    assert_eq!(app.slash_selected, index);
    assert!(app.slash_open && app.input == "/", "a click only selects");

    let mut by_key = open();
    by_key.slash_selected = index;
    let entered = by_key.handle_key(key(KeyCode::Enter));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.handle_mouse(click), entered);
    assert_eq!(app.input, by_key.input);
    assert_eq!(app.slash_open, by_key.slash_open);
}

#[test]
fn slash_palette_scrolls_selection_and_mouse_maps_through_viewport() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("/".into());
    app.sync_slash_open();
    app.slash_selected = 15;
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let popup = app.slash_popup.expect("slash popup");
    let visible_rows = usize::from(popup.height.saturating_sub(2));
    assert!(app.slash_view_start > 0);
    assert!(app.slash_selected >= app.slash_view_start);
    assert!(app.slash_selected < app.slash_view_start + visible_rows);

    let expected = app.slash_view_start;
    app.handle_mouse(left_mouse(
        MouseEventKind::Moved,
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
    ));
    assert_eq!(app.slash_selected, expected);
}

#[test]
fn slash_palette_does_not_block_composer_mouse_interaction() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("/clear trailing".into());
    app.sync_slash_open();
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.expect("composer");

    app.handle_mouse(left_click(inner.x.saturating_add(2), inner.y));

    assert_eq!(app.input_cursor, 0);
    assert!(app.slash_open);
}

#[test]
fn shift_enter_inserts_newline_not_send() {
    let mut launch = launch_basic();
    launch.agent_enabled = true;
    let mut app = App::new(launch, false);
    app.set_input("line1".into());
    let mut ev = key(KeyCode::Enter);
    ev.modifiers = KeyModifiers::SHIFT;
    assert!(app.handle_key(ev).is_none());
    assert_eq!(app.input, "line1\n");
    assert_eq!(app.input_cursor, 6);
}

#[test]
fn ctrl_j_is_not_newline() {
    let mut launch = launch_basic();
    launch.agent_enabled = true;
    let mut app = App::new(launch, false);
    app.set_input("x".into());
    let mut ev = key(KeyCode::Char('j'));
    ev.modifiers = KeyModifiers::CONTROL;
    let _ = app.handle_key(ev);
    assert_eq!(app.input, "x", "Ctrl+J must not insert newline");
}

#[test]
fn composer_input_is_utf8_safe_and_bounded_before_rendering() {
    let mut app = App::new(launch_basic(), true);
    app.set_input(format!("{}é", "x".repeat(super::MAX_COMPOSER_BYTES)));
    assert_eq!(app.input.len(), super::MAX_COMPOSER_BYTES);
    assert!(app.input.is_char_boundary(app.input.len()));

    app.composer_selection.begin_drag(&app.input, 0);
    app.composer_selection.drag_to(&app.input, 10);
    app.insert_chars("ééé");
    assert!(app.input.len() <= super::MAX_COMPOSER_BYTES);
    assert!(app.input.starts_with("ééé"));
}

#[test]
fn ctrl_c_clears_nonempty_input() {
    let mut app = App::new(launch_basic(), true);
    app.input = "partial".into();
    app.busy = true;
    let action = app.handle_key(key_ctrl(KeyCode::Char('c')));
    assert_eq!(action, None);
    assert!(app.input.is_empty());
    assert!(!app.should_quit);
    // Second Ctrl+C while busy asks in the turn stop pop-up.
    let action = app.handle_key(key_ctrl(KeyCode::Char('c')));
    assert_eq!(action, None);
    assert!(app.destructive_confirm.is_some());
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::Interrupt)
    );
}

#[test]
fn esc_esc_clears_an_idle_draft_and_one_undo_restores_it() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("keep me".into());
    app.composer_undo.reset();
    for ch in "!".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.input, "keep me!");
    assert_eq!(
        app.handle_key(key(KeyCode::Esc)),
        None,
        "an Esc that edits the draft never interrupts"
    );
    assert!(!app.input.is_empty(), "first Esc only arms the clear");
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(app.input.is_empty());
    assert!(!app.should_quit, "Esc never quits");
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(app.input, "keep me!");
}

#[test]
fn esc_closes_slash_palette() {
    let mut app = App::new(launch_basic(), true);
    app.input = "/he".into();
    app.sync_slash_open();
    assert!(app.slash_open);
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(!app.slash_open);
    assert!(!app.should_quit);
}

#[test]
fn idle_escape_cancels_automatic_resume_without_changing_the_draft_or_view() {
    for pending in [true, false] {
        let mut app = App::new(launch_basic(), true);
        if pending {
            app.apply_host_event(HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Turn,
                "connection unavailable; will continue automatically when the network returns (1/1)",
            )));
        }
        let before = render_to_string(&mut app, 80, 24);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Some(UserAction::Interrupt)
        );
        assert_eq!(render_to_string(&mut app, 80, 24), before);
        assert!(!app.cancel_pending);
        assert!(!app.should_quit);
        app.set_input("keep this draft".into());
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.input, "keep this draft");
    }
}

#[test]
fn idle_escape_under_a_modal_only_closes_the_modal() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("keep this draft".into());
    app.open_control_tab(super::ControlTab::Keybinds);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.input, "keep this draft");
    assert!(!app.cancel_pending);
    assert!(app.control_tab.is_none());
}

#[test]
fn fragmented_stream_work_is_tail_sized_not_history_times_fragments() {
    for history_len in [20, 5_000] {
        let baseline = render_stream_slope(history_len, 1);
        for fragments in [10, 100, 1_000] {
            let observed = render_stream_slope(history_len, fragments);
            assert_eq!(observed.0, baseline.0, "final TestBackend screen drifted");
            assert_eq!(observed.1, baseline.1, "plain projection drifted");
            assert_eq!(
                observed.2, baseline.2,
                "render work must follow changed tail rows, not fragmentation"
            );
        }
    }
    assert_eq!(
        render_stream_slope(20, 1).2,
        render_stream_slope(5_000, 1).2,
        "tail update work must not scale with retained history"
    );
}

#[test]
fn prompt_history_up_when_empty() {
    let mut app = App::new(launch_basic(), true);
    app.input = "one".into();
    let first = app.handle_key(key(KeyCode::Enter));
    assert_eq!(first, Some(UserAction::Prompt("one".into())));
    commit_sent_action(&mut app, first.as_ref().unwrap());
    app.input = "two".into();
    let second = app.handle_key(key(KeyCode::Enter));
    assert_eq!(second, Some(UserAction::Prompt("two".into())));
    commit_sent_action(&mut app, second.as_ref().unwrap());
    assert!(app.prompt_history.is_empty());
    app.apply_host_event(HostEvent::PromptHistorySnapshot(vec![
        "one".into(),
        "two".into(),
    ]));
    assert!(app.input.is_empty());
    assert!(app.handle_key(key(KeyCode::Up)).is_none());
    assert!(
        app.input.is_empty(),
        "history selection must not edit the draft"
    );
    assert_eq!(app.history_browse, Some(1));
    assert!(app.handle_key(key(KeyCode::Up)).is_none());
    assert_eq!(app.history_browse, Some(0));
    assert!(app.handle_key(key(KeyCode::Down)).is_none());
    assert_eq!(app.history_browse, Some(1));
    assert!(app.handle_key(key(KeyCode::Left)).is_none());
    assert_eq!(app.input, "two");
    assert_eq!(app.history_browse, None);
}

#[test]
fn prompt_history_ctrl_c_clears_and_typing_detaches() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::PromptHistorySnapshot(vec!["old".into()]));
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.history_browse, Some(0));
    assert!(app.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
    assert_eq!(app.history_browse, None);
    assert!(app.input.is_empty());

    app.handle_key(key(KeyCode::Up));
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(app.history_browse, None);
    assert_eq!(app.input, "n");
}

#[test]
fn prompt_history_click_loads_exact_snapshot_row_at_narrow_width() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::PromptHistorySnapshot(vec![
        "first wide prompt".into(),
        "second".into(),
    ]));
    app.handle_key(key(KeyCode::Up));
    let backend = TestBackend::new(24, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let (_, hit) = app.prompt_history_hits[0];
    assert!(app.handle_mouse(left_click(hit.x, hit.y)).is_none());
    assert_eq!(app.input, "first wide prompt");
    assert_eq!(app.history_browse, None);
}

#[test]
fn prompt_history_is_bounded_and_host_snapshot_authoritative() {
    let mut app = App::new(launch_basic(), true);
    let prompts = (0..505).map(|i| format!("prompt-{i}")).collect();
    app.apply_host_event(HostEvent::PromptHistorySnapshot(prompts));
    assert_eq!(app.prompt_history.len(), 500);
    assert_eq!(
        app.prompt_history.first().map(String::as_str),
        Some("prompt-5")
    );

    // A locally sent-but-not-yet-published prompt cannot enter browsing.
    let (sender, _receiver) = user_action_channel();
    dispatch_action(
        &mut app,
        &UserAction::Prompt("prompt-505".into()),
        Some(&sender),
    );
    assert_eq!(app.prompt_history.len(), 500);
    assert_eq!(
        app.prompt_history.first().map(String::as_str),
        Some("prompt-5")
    );
    assert_eq!(
        app.prompt_history.last().map(String::as_str),
        Some("prompt-504")
    );

    app.apply_host_event(HostEvent::PromptHistorySnapshot(
        (1..=505).map(|i| format!("prompt-{i}")).collect(),
    ));
    assert_eq!(
        app.prompt_history.first().map(String::as_str),
        Some("prompt-6")
    );
    assert_eq!(
        app.prompt_history.last().map(String::as_str),
        Some("prompt-505")
    );
}

/// Round-3 brief: the terminal cursor lives in the composer and is hidden
/// while the operator scrolls or drags the transcript, decided once per
/// frame — a wheel frame never emits show-cursor.
#[test]
fn the_cursor_hides_during_transcript_scroll_and_returns_on_typing() {
    let mut app = transcript_fixture();
    for index in 0..40 {
        say(&mut app, &format!("Paragraph {index} of a long answer."));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        terminal.backend().cursor_visible(),
        "idle composer shows its cursor"
    );

    app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, 10, 2));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !terminal.backend().cursor_visible(),
        "a wheel frame hides the cursor"
    );
    app.handle_mouse(left_mouse(MouseEventKind::Down(MouseButton::Left), 10, 2));
    app.handle_mouse(left_mouse(MouseEventKind::Drag(MouseButton::Left), 20, 3));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !terminal.backend().cursor_visible(),
        "a drag frame hides the cursor"
    );

    app.handle_key(key(KeyCode::Char('a')));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        terminal.backend().cursor_visible(),
        "typing hands the cursor back to the composer"
    );
}

#[test]
fn resize_history_sticky_and_tool_mouse_geometry_remain_coherent() {
    let mut app = visual_fixture(false);
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(screen.contains("prompt history · ↑ older · ↓ newer · Enter use/edit"));

    app.dismiss_history();
    app.pane
        .blocks
        .push(ScrollBlock::Assistant("Finished.".into()));
    app.mark_transcript_replaced();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let &(tool_index, tool_row) = app
        .tool_hit_rows
        .iter()
        .find(|(index, _)| matches!(app.pane.blocks[*index], ScrollBlock::Tool { .. }))
        .expect("visible tool row");
    assert!(app.handle_mouse(left_click(5, tool_row)).is_none());
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        5,
        tool_row,
    ));
    assert!(
        !crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[tool_index].group_folded,
        "a toggled row must open past its header"
    );
    assert!(!app.selection.is_dragging());
    assert!(!app.selection.has_range());

    if let ScrollBlock::Assistant(text) = &mut app.pane.blocks[1] {
        for line in 1..=18 {
            let _ = write!(text, "\nresize evidence {line:02}");
        }
        app.mark_transcript_replaced();
    }
    terminal.backend_mut().resize(48, 18);
    terminal.autoresize().unwrap();
    app.pane.scroll_from_bottom = 0;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.pane.scroll_from_bottom = 2;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let transcript = app.transcript_area.expect("transcript after resize");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.area.width, 48);
    assert!(app.composer_inner.is_some_and(|inner| inner.right() <= 48));
    assert!((0..transcript.width.saturating_sub(1)).all(|x| {
        let index = usize::from(transcript.y) * usize::from(buffer.area.width) + usize::from(x);
        buffer.content()[index].bg == app.theme.sticky_user_bg
    }));
}

#[test]
#[allow(clippy::too_many_lines, reason = "one end-to-end geometry assertion")]
fn long_tool_rows_share_buffer_hit_selection_and_copy_geometry() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = vec![ScrollBlock::Tool {
        call_id: Some("unicode-tool".into()),
        name: "exec_js".into(),
        state: "done:123ms".into(),
        subject: Some("0 tools".into()),
        detail: Some(
            "第一🙂e\u{301}  detail with   spaces and a very long suffix\nsecond界🙂 line  ".into(),
        ),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Compose,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: crate::tui::blocks::ItemView::default(),
        mode: DisplayMode::Collapsed,
    }];
    app.mark_transcript_replaced();
    let backend = TestBackend::new(18, 32);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    assert!(
        app.pane.transcript_cache.plain().len() > 1,
        "collapsed summary wraps"
    );
    assert_eq!(
        app.tool_hit_rows
            .iter()
            .map(|(block, _)| *block)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "one summary owns all of its wrapped hit rows"
    );
    assert_visible_transcript_rows_match_projection(&app, terminal.backend().buffer());

    let (tool_index, summary_row) = app.tool_hit_rows[0];
    assert!(app.handle_mouse(left_click(5, summary_row)).is_none());
    // A press on a clickable row arms its toggle; only a drag selects.
    assert!(!app.selection.has_range(), "a press alone selects nothing");
    assert_eq!(
        app.pending_tool_click,
        Some((tool_index, crate::tui::blocks::RowTarget::Item))
    );
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        5,
        summary_row,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        app.pane.blocks[0]
            .display_mode()
            .is_some_and(DisplayMode::shows_detail),
        "a toggled row must open past its header"
    );
    assert!(
        app.pane.transcript_cache.plain().len() > 4,
        "expanded detail wraps"
    );
    assert!(
        app.pane
            .transcript_cache
            .plain()
            .iter()
            .all(|line| line.width() <= 18)
    );
    assert_visible_transcript_rows_match_projection(&app, terminal.backend().buffer());

    let body = app.transcript_body_area.expect("body");
    let top = body.y;
    let bottom =
        top + u16::try_from(app.pane.transcript_cache.plain().len() - 1).expect("test rows");
    let right = body.right() - 1;
    // A copy through the cut cell row carries its full text.
    let expected = "  ✓ CodeMode · 123ms\n第一🙂e\u{301}  detail with   spaces and a very long suffix\nsecond界🙂 line  \noutput ▸\n".to_owned();
    // Reverse-drag from a detail row onto the cell row copies the full
    // projection.
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        right,
        bottom,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.x,
        top,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.x,
        top,
    ));
    assert_eq!(app.take_transcript_selection_text(), Some(expected.clone()));

    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        right,
        bottom,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.x,
        top,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.x,
        top,
    ));
    assert_eq!(app.take_transcript_selection_text(), Some(expected));
}

#[test]
fn a_transcript_selection_pins_the_projection_to_an_exact_measurement() {
    // Off-window blocks carry an estimated row count until the viewport
    // reaches them. Selection cannot live with that: it carries content
    // rows from one frame to the next, and measuring a block renumbers
    // every row below it. So a live selection measures the transcript.
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = (0..2_000)
        .map(|index| {
            if index % 2 == 0 {
                ScrollBlock::user(format!("turn {index} asks a question"))
            } else {
                ScrollBlock::Assistant(format!("turn {index} answers it at some length"))
            }
        })
        .collect();
    app.mark_transcript_replaced();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !app.pane.transcript_cache.estimated_blocks().is_empty(),
        "a 2k-block transcript must lay out its history as estimates"
    );

    let body = app.transcript_body_area.expect("a painted transcript body");
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.right() - 1,
        body.bottom() - 1,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        app.pane.transcript_cache.estimated_blocks().is_empty(),
        "a live selection was left reading estimated geometry"
    );

    let copied = app
        .take_transcript_selection_text()
        .expect("a dragged selection copies");
    assert!(
        copied.lines().any(|line| line.contains("turn 1999")),
        "the copy did not reach the rows the drag covered: {copied:?}"
    );

    // Releasing the selection lets the layout go back to estimating.
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.right() - 1,
        body.bottom() - 1,
    ));
    app.selection.clear();
    app.mark_transcript_structural();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !app.pane.transcript_cache.estimated_blocks().is_empty(),
        "the layout stayed measured after the selection ended"
    );
}

#[test]
fn sticky_preview_is_navigation_only_and_width_reflow_clears_selection() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(super::test_support::restore_snapshot(
        1,
        vec![
            plakous_host_protocol::views::transcript::TranscriptRow::user("sticky source"),
            plakous_host_protocol::views::transcript::TranscriptRow::assistant(
                "one\n\ntwo\n\nthree\n\nfour\n\nfive\n\nsix\n\nseven\n\neight\n\nnine\n\nten",
            ),
        ],
    ));
    app.pane.scroll_from_bottom = 1;
    let backend = TestBackend::new(30, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let (sticky, target) = app.sticky_prompt_jump.expect("sticky preview");
    app.handle_mouse(left_click(sticky.x, sticky.y));
    assert_eq!(app.pane.scroll_from_bottom, target);
    assert!(!app.selection.is_dragging());

    let body = app.transcript_body_area.expect("body");
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.x.saturating_add(3),
        body.y,
    ));
    assert!(app.selection.is_dragging());

    let backend = TestBackend::new(22, 10);
    let mut resized = Terminal::new(backend).unwrap();
    resized.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !app.selection.is_dragging(),
        "width reflow clears row coordinates"
    );
}

/// Restore order (§6): mouse reporting and the other input modes go off
/// while raw mode is still on, queued input is discarded next, and only
/// then does raw mode end and the alternate screen get left. Anything
/// else hands buffered mouse reports to the shell.
#[test]
fn terminal_restore_disables_mouse_and_discards_input_before_leaving_raw_mode() {
    let source = include_str!("terminal.rs");
    let body = source
        .split("fn restore(&mut self) -> Vec<String> {")
        .nth(1)
        .and_then(|rest| rest.split("\nimpl Drop for TerminalRestoration").next())
        .expect("TerminalRestoration::restore");
    let index = |needle: &str| {
        body.find(needle)
            .unwrap_or_else(|| panic!("restore must {needle}"))
    };
    let mouse = index("DisableMouseCapture");
    let paste = index("DisableBracketedPaste");
    let discard = index("discard_pending_terminal_input()");
    let leave = index("ratatui::try_restore()");
    let cursor = index("ShowCursor");
    assert!(
        mouse < discard && paste < discard,
        "modes off before the discard"
    );
    assert!(
        discard < leave,
        "queued input discarded before raw mode ends"
    );
    assert!(
        leave < cursor,
        "cursor shown after the alternate screen is left"
    );
}

#[test]
fn shaping_joiners_survive_paste_and_typing_through_the_app() {
    // The blanket `Cf` reject used to mangle these on every path.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let devanagari = "\u{915}\u{94d}\u{200c}\u{937}";
    let persian = "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}";

    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.handle_paste(family), None);
    assert_eq!(app.input, family, "a ZWJ emoji sequence survives paste");
    app.clear_input();

    assert_eq!(
        app.handle_paste(&format!("{devanagari} {persian}\u{202e}")),
        None
    );
    assert_eq!(
        app.input,
        format!("{devanagari} {persian}"),
        "ZWNJ survives paste; the bidi override alone is dropped"
    );
    app.clear_input();

    // Typed one scalar at a time, the joiners land too.
    for ch in family.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.input, family, "a ZWJ emoji sequence survives typing");
    app.clear_input();

    for ch in devanagari.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Char('\u{202e}')));
    assert_eq!(
        app.input, devanagari,
        "ZWNJ survives typing, U+202E does not"
    );
}

#[test]
fn readline_ops_are_grapheme_correct_over_combining_text() {
    let mut app = App::new(launch_basic(), true);
    // "e" + combining acute, then a regional-indicator flag pair.
    app.set_input("e\u{301}x \u{1f1e9}\u{1f1ea}".into());
    let end = app.input.chars().count();
    assert_eq!(app.input_cursor, end);
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.input, "e\u{301}x ", "one flag is one grapheme");
    app.handle_key(key_ctrl(KeyCode::Char('a')));
    assert_eq!(app.input_cursor, 0);
}

#[test]
fn undo_redo_walks_coalesced_typing_and_discrete_kills() {
    let mut app = App::new(launch_basic(), true);
    typed(&mut app, "hello");
    assert_eq!(app.input, "hello");
    // A whole typing run collapses to one undo step.
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(app.input, "");
    app.handle_key(key_ctrl(KeyCode::Char('y')));
    assert_eq!(app.input, "hello");

    // A kill is its own step.
    app.handle_key(key_ctrl(KeyCode::Char('u')));
    assert_eq!(app.input, "");
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(app.input, "hello");

    // Ctrl+Shift+Z also redoes.
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(app.input, "");
    let mut redo = key_ctrl(KeyCode::Char('z'));
    redo.modifiers = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    app.handle_key(redo);
    assert_eq!(app.input, "hello");
}

#[test]
fn the_first_ctrl_c_draft_clear_is_recoverable_by_one_undo() {
    let mut app = App::new(launch_basic(), true);
    typed(&mut app, "a long prompt");
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.input.is_empty());
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(app.input, "a long prompt");
}

/// The composer meter divides by the
/// model's total window — luna renders `…/272k` — and the 258.4k admitted
/// envelope never becomes the denominator. `/usage` quotes the same
/// window in its Context header and lists the ceiling as its own fact.
#[test]
fn composer_meter_denominator_is_the_total_window_on_luna() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Usage(ContextUsage {
        total_window: Some(272_000),
        admitted_ceiling: Some(crate::tui::presentation::AdmittedContextCeiling {
            tokens: 258_400,
            basis: "envelope".into(),
        }),
        prepared_input: Some(PreparedInputUsage::UpperBound(16_320)),
        display_percent: Some(6),
        evidence: "reference-client-envelope".into(),
        status: "fits".into(),
        ..super::unknown_context_usage()
    }));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = screen_text(&terminal);
    assert!(screen.contains("6%/272k"), "{screen}");
    assert!(!screen.contains("/258.4k"), "{screen}");

    let rows = app.session_info_rows();
    let context = rows
        .iter()
        .find(|row| row.label == "Context")
        .expect("session rows carry Context");
    assert!(context.value.contains("/ 272k tokens"), "{}", context.value);
}

/// Without a total window the meter says so instead of borrowing the
/// admitted ceiling as a stand-in denominator.
#[test]
fn composer_meter_without_a_window_is_unknown_not_the_ceiling() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Usage(ContextUsage {
        total_window: None,
        admitted_ceiling: Some(crate::tui::presentation::AdmittedContextCeiling {
            tokens: 258_400,
            basis: "envelope".into(),
        }),
        display_percent: Some(6),
        evidence: "reference-client-envelope".into(),
        status: "fits".into(),
        ..super::unknown_context_usage()
    }));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = screen_text(&terminal);
    assert!(screen.contains("awaiting context"), "{screen}");
    assert!(!screen.contains("/258.4k"), "{screen}");
}

#[test]
fn over_limit_queue_refusal_keeps_unrecorded_text_on_the_local_history_shelf() {
    let mut app = App::new(launch_basic(), true);
    let newer = "n".repeat(super::MAX_COMPOSER_BYTES - 4);
    app.set_input(newer.clone());
    let refused = "queue refused draft".to_owned();
    app.apply_host_event(HostEvent::DraftRejected(UserAction::Prompt(
        refused.clone(),
    )));
    assert_eq!(app.input, newer);
    assert!(app.prompt_history.contains(&refused));
    app.apply_host_event(HostEvent::PromptHistorySnapshot(vec!["recorded".into()]));
    assert!(app.prompt_history.contains(&refused));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.set_input(String::new());
    app.handle_key(key(KeyCode::Up));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(screen_text(&terminal).contains("queue refused draft"));
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.input, refused);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(screen_text(&terminal).contains("queue refused draft"));
}

#[test]
fn clicking_above_caret_does_not_scroll_or_select_on_release() {
    let mut app = App::new(launch_basic(), true);
    app.set_input((0..24).map(|i| format!("line {i}\n")).collect());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let inner = app.composer_inner.unwrap();
    assert!(inner.height > 2);
    let scroll = app.composer_visual_scroll;
    let cell = (inner.x + 3, inner.y + 1);
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        cell.0,
        cell.1,
    ));
    let caret = app.input_cursor;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.composer_visual_scroll, scroll);
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        cell.0,
        cell.1,
    ));
    assert_eq!(app.input_cursor, caret);
    assert!(app.composer_selection.range(&app.input).is_none());
}

#[test]
fn trackpad_packets_keep_caret_hidden_across_drained_queues() {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_area.unwrap();
    for _ in 0..6 {
        app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, body.x, body.y));
        app.mouse_scroll_queue = 0;
        app.last_wheel_at = Some(app.now().checked_sub(Duration::from_millis(16)).unwrap());
        app.settle_wheel_gesture();
        assert!(
            app.transcript_gesture,
            "16ms trackpad packet gap is not gesture end"
        );
    }
    app.last_wheel_at = Some(app.now().checked_sub(Duration::from_millis(110)).unwrap());
    app.settle_wheel_gesture();
    assert!(!app.transcript_gesture);
}

#[test]
fn super_c_over_composer_selection_copies_without_editing() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("selected draft".into());
    app.select_all_composer();
    let range = app.composer_selection.range(&app.input);
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
    assert_eq!(app.input, "selected draft");
    assert_eq!(app.composer_selection.range(&app.input), range);
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER));
    assert_eq!(app.input, "selected draft", "SUPER keys never insert text");
}

#[test]
fn selection_anchor_survives_estimated_to_exact_rebuild_over_512_blocks() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = (0..800)
        .map(|i| {
            ScrollBlock::Assistant(format!(
                "# block {i}\n\n```text\nline one\nline two\n```\n\n---"
            ))
        })
        .collect();
    app.mark_transcript_replaced();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(!app.pane.transcript_cache.estimated_blocks().is_empty());
    let body = app.transcript_body_area.unwrap();
    let position = app.transcript_position_at(body.x + 2, body.y + 2).unwrap();
    let anchor = layout_anchor(app.pane.transcript_cache.layout(), position.row).unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x + 2,
        body.y + 2,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.x + 5,
        body.y + 2,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let selected = app.selection.endpoints_for_test().unwrap().0;
    assert_eq!(
        layout_anchor(app.pane.transcript_cache.layout(), selected.row),
        Some(anchor)
    );
    assert_eq!(selected.column, 2);
}

#[test]
fn transcript_drag_slop_ignores_one_cell_jitter() {
    let mut app = transcript_fixture();
    say(&mut app, "a paragraph for selection");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_body_area.unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x + 2,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.x + 3,
        body.y,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(!app.selection.has_range());
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.x + 3,
        body.y,
    ));
    assert!(!app.selection.has_range());
}

#[test]
fn super_editing_bindings_select_cut_undo_and_redo() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("whole draft".into());
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER));
    assert_eq!(app.composer_selection.range(&app.input), Some(0..11));
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SUPER));
    assert!(app.input.is_empty());
    assert!(app.control_tab.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::SUPER));
    assert_eq!(app.input, "whole draft");
    app.handle_key(KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::SUPER | KeyModifiers::SHIFT,
    ));
    assert!(app.input.is_empty());
}
