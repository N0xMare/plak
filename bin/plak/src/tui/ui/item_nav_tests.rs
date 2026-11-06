//! Keyboard navigation between transcript items.

use super::item_nav::item_row_span;
use super::test_support::{key, key_alt, key_ctrl, launch_basic, queue_item, serialize_frame};
use super::{App, ContentPosition, DisplayMode, ScrollBlock};
use crate::tui::presentation::ActivityKind;
use crate::tui::{HostEvent, UserAction};
use crossterm::event::KeyCode;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::{Terminal, buffer::Buffer};

fn tool(name: &str) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: Some("call-1".into()),
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: name.into(),
        state: "done".into(),
        subject: Some("src/lib.rs".into()),
        detail: Some("1 file".into()),
        mode: DisplayMode::Collapsed,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: crate::tui::blocks::ItemView::default(),
    }
}

fn sample() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.pane.blocks = vec![
        ScrollBlock::Header("session s1".into()),
        ScrollBlock::user("PROMPT-ALPHA".into()),
        ScrollBlock::Assistant("ANSWER-BETA".into()),
        tool("read_file"),
        ScrollBlock::item("p9 make"),
        ScrollBlock::System("NOTICE-GAMMA".into()),
    ];
    app.mark_transcript_structural();
    app
}

fn paint(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn mode_of(app: &App, index: usize) -> Option<DisplayMode> {
    app.pane
        .blocks
        .get(index)
        .and_then(ScrollBlock::display_mode)
}

fn drag_rows(app: &mut App, first: usize, last: usize) -> String {
    let width = app.last_transcript_width.max(1);
    app.pane.selection.begin(
        ContentPosition {
            row: first,
            column: 0,
        },
        width,
        app.pane.transcript_revision,
    );
    app.pane.selection.drag(ContentPosition {
        row: last,
        column: width.saturating_sub(1),
    });
    app.pane
        .selection
        .capture_blocks(app.pane.transcript_cache.layout());
    app.take_transcript_selection_text().unwrap()
}

/// Rows the test believes belong to `block`, scanned independently of the
/// navigation helper's callers.
fn owned_rows(app: &App, block: usize) -> (usize, usize) {
    let layout = app.pane.transcript_cache.layout();
    let mut rows = layout
        .block_index
        .iter()
        .enumerate()
        .filter_map(|(row, owner)| (*owner == Some(block)).then_some(row));
    let first = rows.next().unwrap();
    (first, rows.next_back().unwrap_or(first))
}

#[test]
fn alt_arrows_move_between_items_and_clamp_at_the_ends() {
    let mut app = sample();
    paint(&mut app, 80, 24);
    assert!(app.handle_key(key_alt(KeyCode::Down)).is_none());
    // The header is not an item. Down from the composer lands on the prompt.
    assert_eq!(app.pane.focused_block, Some(1));
    assert!(app.pane.item_nav);
    app.handle_key(key_alt(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(2), "down moves to the answer");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.pane.focused_block,
        Some(3),
        "bare down moves while navigating"
    );
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.pane.focused_block,
        Some(5),
        "a process row folded into the tool is skipped"
    );
    if let ScrollBlock::Tool { group_open, .. } = &mut app.pane.blocks[3] {
        *group_open = true;
    }
    app.mark_transcript_structural();
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(
        app.pane.focused_block,
        Some(4),
        "opening the group makes the process row an item"
    );
    app.handle_key(key_alt(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(5), "the notice is an item");
    app.handle_key(key_alt(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(5), "the last item clamps");
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(
        app.pane.focused_block,
        Some(4),
        "up moves to the previous item"
    );
    // From the composer, up lands on the last item, not the first.
    app.leave_item_nav();
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(5));
    while app.pane.focused_block != Some(1) {
        app.handle_key(key(KeyCode::Up));
    }
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(1), "the first item clamps");
}

#[test]
fn an_empty_transcript_does_not_enter_navigation_and_up_still_recalls_history() {
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks.clear();
    app.mark_transcript_structural();
    app.prompt_history = vec!["earlier prompt".into()];
    assert!(app.handle_key(key_alt(KeyCode::Up)).is_none());
    assert!(!app.pane.item_nav);
    assert!(app.pane.focused_block.is_none());
    assert!(app.history_browse.is_none());
    app.handle_key(key(KeyCode::Up));
    assert!(app.history_browse.is_some(), "bare up still opens history");
    assert!(!app.pane.item_nav);

    let mut header_only = App::new(launch_basic(), true);
    header_only.pane.blocks = vec![ScrollBlock::Header("session s1".into())];
    header_only.mark_transcript_structural();
    header_only.handle_key(key_alt(KeyCode::Down));
    assert!(!header_only.pane.item_nav);
}

#[test]
fn enter_and_space_toggle_a_fold_and_do_not_type_or_send() {
    let mut app = sample();
    paint(&mut app, 80, 24);
    app.handle_key(key_alt(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(3));
    assert_eq!(mode_of(&app, 3), Some(DisplayMode::Collapsed));
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert_eq!(mode_of(&app, 3), Some(DisplayMode::Expanded));
    assert_eq!(app.input, "");
    assert!(app.pane.item_nav);
    app.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(mode_of(&app, 3), Some(DisplayMode::Collapsed));
    assert_eq!(app.input, "", "space toggles instead of inserting");
    // A prompt does not fold and must not send.
    app.handle_key(key_alt(KeyCode::Up));
    app.handle_key(key_alt(KeyCode::Up));
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(1));
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert_eq!(app.input, "");
    assert!(app.handle_key(key(KeyCode::Char(' '))).is_none());
    assert_eq!(app.input, "");
}

#[test]
fn alt_c_copies_the_focused_item_like_a_whole_row_drag() {
    let mut app = sample();
    paint(&mut app, 100, 24);
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(5));
    let (first, last) = owned_rows(&app, 5);
    let (helper_first, helper_last) = item_row_span(app.pane.transcript_cache.layout(), 5).unwrap();
    assert_eq!((first, last), (helper_first, helper_last));
    let dragged = drag_rows(&mut app, first, last);
    assert!(dragged.contains("NOTICE-GAMMA"), "{dragged}");
    assert!(app.handle_key(key_alt(KeyCode::Char('c'))).is_none());
    assert_eq!(app.last_nav_copy.as_deref(), Some(dragged.as_str()));
    assert!(app.pane.item_nav, "copy stays in navigation");
    assert_eq!(app.input, "", "alt+c does not type");
    assert!(
        !app.pane.selection.has_range(),
        "the copy does not leave a selection"
    );
    let notice = app.last_nav_copy.clone();
    app.handle_key(key_alt(KeyCode::Up));
    app.handle_key(key_alt(KeyCode::Char('C')));
    assert_ne!(
        app.last_nav_copy, notice,
        "a different item copies different text"
    );
    assert!(app.pane.item_nav);
}

#[test]
fn typing_returns_to_the_composer_and_esc_only_leaves_navigation() {
    let mut app = sample();
    app.set_input("keep".into());
    app.handle_key(key_alt(KeyCode::Up));
    assert!(app.pane.item_nav);
    assert_eq!(app.input, "keep");
    let cursor = app.input_cursor;
    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.input_cursor, cursor,
        "up moves the marker, not the caret"
    );
    assert!(app.history_browse.is_none());
    app.handle_key(key(KeyCode::Char('y')));
    assert!(!app.pane.item_nav);
    assert!(app.pane.focused_block.is_none());
    assert_eq!(app.input, "keepy");
    assert!(app.last_nav_copy.is_none(), "y types; it does not copy");
    app.handle_key(key_alt(KeyCode::Up));
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.pane.item_nav);
    assert_eq!(app.input, "keepy", "esc does not clear the draft");
    assert!(app.destructive_confirm.is_none());
    assert!(app.esc_clear_arm.is_none());
    // Back in the composer, up recalls history instead of moving a marker.
    app.prompt_history = vec!["earlier prompt".into()];
    app.set_input(String::new());
    app.handle_key(key(KeyCode::Up));
    assert!(app.history_browse.is_some());
    assert!(!app.pane.item_nav);
}

#[test]
fn esc_and_ctrl_c_keep_the_sent_prompt_window() {
    for stage in 0..3 {
        let (mut plain, action) = sent_prompt();
        answer_prompt(&mut plain, &action, stage);
        assert!(plain.handle_key(key(KeyCode::Esc)).is_none());
        assert!(
            plain.destructive_confirm.is_some(),
            "stage {stage}: esc still asks to stop"
        );

        let (mut copying, action) = sent_prompt();
        answer_prompt(&mut copying, &action, stage);
        assert!(copying.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
        assert!(
            copying.destructive_confirm.is_some(),
            "stage {stage}: ctrl+c still asks to stop"
        );
        assert!(copying.quit_arm.is_none(), "stage {stage}");

        let (mut navigating, action) = sent_prompt();
        answer_prompt(&mut navigating, &action, stage);
        navigating.pane.blocks = vec![
            ScrollBlock::user("older".into()),
            ScrollBlock::System("note".into()),
        ];
        navigating.mark_transcript_structural();
        navigating.handle_key(key_alt(KeyCode::Up));
        assert!(navigating.pane.item_nav, "stage {stage}");
        assert!(navigating.handle_key(key(KeyCode::Esc)).is_none());
        assert!(
            !navigating.pane.item_nav,
            "stage {stage}: esc leaves navigation"
        );
        assert!(
            navigating.destructive_confirm.is_none(),
            "stage {stage}: leaving navigation does not ask to stop"
        );
        assert!(navigating.handle_key(key(KeyCode::Esc)).is_none());
        assert!(
            navigating.destructive_confirm.is_some(),
            "stage {stage}: the next esc still asks to stop the sent prompt"
        );

        let (mut ladder, action) = sent_prompt();
        answer_prompt(&mut ladder, &action, stage);
        ladder.pane.blocks = vec![ScrollBlock::System("note".into())];
        ladder.mark_transcript_structural();
        ladder.handle_key(key_alt(KeyCode::Up));
        assert!(ladder.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
        assert!(
            ladder.destructive_confirm.is_some(),
            "stage {stage}: ctrl+c ignores the marker"
        );
        assert!(ladder.last_nav_copy.is_none(), "stage {stage}");
        assert!(ladder.pane.item_nav, "stage {stage}: ctrl+c is not esc");
    }
}

#[test]
fn idle_ctrl_c_still_arms_quit_while_navigating() {
    let mut app = sample();
    paint(&mut app, 80, 24);
    app.handle_key(key_alt(KeyCode::Up));
    assert!(app.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
    assert!(app.quit_arm.is_some(), "the idle ladder still arms");
    assert!(app.destructive_confirm.is_none());
    assert!(app.last_nav_copy.is_none());
    assert!(app.pane.item_nav);
}

#[test]
fn items_arriving_keep_the_focused_identity() {
    let mut app = sample();
    app.handle_key(key_alt(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(1));
    app.pane.blocks.push(ScrollBlock::System("later".into()));
    app.mark_transcript_structural();
    assert_eq!(app.pane.focused_block, Some(1));
    assert!(app.pane.item_nav);
    app.pane
        .blocks
        .insert(0, ScrollBlock::System("before".into()));
    app.splice_keyed_indices(0, 0, 1);
    assert_eq!(app.pane.focused_block, Some(2));
    assert!(app.pane.item_nav);
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(3));
}

#[test]
fn the_marker_stays_on_screen_across_a_resize() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.pane.blocks = (0..40)
        .map(|index| ScrollBlock::System(format!("row-{index:02}")))
        .collect();
    app.mark_transcript_structural();
    paint(&mut app, 100, 24);
    app.handle_key(key_alt(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(0));
    let wide = paint(&mut app, 100, 24);
    assert_focused_visible(&app);
    assert_focus_background(&wide, "row-00");
    let index = app.pane.focused_block;
    paint(&mut app, 40, 18);
    assert_eq!(app.pane.focused_block, index);
    assert!(app.pane.item_nav);
    assert_focused_visible(&app);
}

#[test]
fn the_focus_marker_matches_its_frame() {
    let mut app = sample();
    app.handle_key(key_alt(KeyCode::Up));
    let frame = paint(&mut app, 80, 24);
    let actual = serialize_frame(&frame);
    assert_focus_background(&frame, "NOTICE-GAMMA");
    let path = "tests/goldens/transcript-item-focus-80x24.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(path, &actual).unwrap();
    } else {
        assert_eq!(
            actual,
            include_str!("../../../tests/goldens/transcript-item-focus-80x24.frame")
        );
    }
}

#[test]
fn agent_navigation_is_separate_from_the_root_and_from_agent_letters() {
    let mut app = sample();
    paint(&mut app, 80, 24);
    app.handle_key(key_alt(KeyCode::Up));
    let root_focus = app.pane.focused_block;
    assert!(app.pane.item_nav);
    app.open_agent_view("agent-1".into(), "#a · worker".into());
    app.pane.blocks = vec![
        ScrollBlock::user("agent prompt".into()),
        tool("read_file"),
        ScrollBlock::System("agent notice".into()),
    ];
    app.mark_transcript_structural();
    paint(&mut app, 80, 24);
    assert!(
        !app.pane.item_nav,
        "the agent pane starts without the root marker"
    );
    app.handle_key(key_alt(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(2));
    app.handle_key(key_alt(KeyCode::Char('c')));
    assert!(app.last_nav_copy.is_some(), "alt+c copies the item");
    assert!(
        app.destructive_confirm.is_none(),
        "alt+c does not close the agent"
    );
    assert!(app.pane.item_nav);
    let scrolled = app.pane.scroll_from_bottom;
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(1));
    assert_eq!(
        app.pane.scroll_from_bottom, scrolled,
        "up moves, it does not scroll"
    );
    let copied = app.last_nav_copy.clone();
    app.handle_key(key(KeyCode::Char('y')));
    assert!(
        app.pane.item_nav,
        "y still copies the agent id, not the item"
    );
    assert_eq!(
        app.last_nav_copy, copied,
        "y does not copy the transcript item"
    );
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(mode_of(&app, 1), Some(DisplayMode::Expanded));
    app.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(mode_of(&app, 1), Some(DisplayMode::Collapsed));
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.pane.item_nav);
    assert_eq!(app.viewed_agent_id(), Some("agent-1"));
    app.handle_key(key(KeyCode::Up));
    assert!(app.pane.focused_block.is_none());
    assert!(app.pane.scroll_from_bottom > scrolled);
    app.handle_key(key(KeyCode::Esc));
    assert!(
        app.viewed_agent_id().is_none(),
        "the next esc leaves the agent"
    );
    assert!(app.pane.item_nav, "the root marker is still there");
    assert_eq!(app.pane.focused_block, root_focus);
}

#[test]
fn the_sheet_names_the_navigation_chords() {
    let sheet = crate::tui::keymap::sheet_text();
    for chord in ["Alt+↑ / Alt+↓", "Alt+C"] {
        assert!(sheet.contains(chord), "{chord} missing:\n{sheet}");
    }
    assert!(sheet.contains("copy the focused transcript item"));
    assert!(sheet.contains("while navigating items, open or close the focused block"));
}

/// Swapping `Alt+Up` with `Alt+Down`, or letting `Shift+Up` move or open
/// history, fails here.
#[test]
fn alt_up_and_alt_down_are_not_the_same_key() {
    let mut app = sample();
    paint(&mut app, 80, 24);
    app.handle_key(key_alt(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(1), "alt+down is not alt+up");
    let down_landed = app.pane.focused_block;
    app.leave_item_nav();
    app.handle_key(key_alt(KeyCode::Up));
    assert_ne!(
        app.pane.focused_block, down_landed,
        "alt+up from the composer is the last item, not the first"
    );
    app.handle_key(key_shift(KeyCode::Up));
    assert_eq!(app.pane.focused_block, Some(5), "shift+up does not move");
    assert!(
        app.history_browse.is_none(),
        "shift+up does not open history"
    );
    app.handle_key(key_shift(KeyCode::Down));
    assert_eq!(app.pane.focused_block, Some(5), "shift+down does not move");
}

/// Page keys leave the marker only when they move the caret.
#[test]
fn page_keys_keep_the_marker_on_an_empty_draft() {
    let mut app = sample();
    paint(&mut app, 80, 24);
    app.handle_key(key_alt(KeyCode::Up));
    app.set_input("keep".into());
    app.handle_key(key(KeyCode::PageDown));
    assert!(
        !app.pane.item_nav,
        "page keys leave navigation when the draft has text"
    );
    assert_eq!(app.input, "keep");
    app.handle_key(key_alt(KeyCode::Up));
    let focus = app.pane.focused_block;
    app.set_input(String::new());
    let scrolled = app.pane.scroll_from_bottom;
    app.handle_key(key(KeyCode::PageUp));
    assert_eq!(
        app.pane.focused_block, focus,
        "page-up does not move the marker"
    );
    assert!(
        app.pane.item_nav,
        "an empty draft pages without leaving navigation"
    );
    assert!(app.pane.scroll_from_bottom > scrolled);
    app.handle_key(key(KeyCode::Char('/')));
    assert!(
        app.slash_open,
        "typing slash left navigation and opened the palette"
    );
    assert!(!app.pane.item_nav);
    app.handle_key(key_alt(KeyCode::Down));
    assert!(!app.pane.item_nav, "the palette keeps alt+down");
}

/// The queue keeps its arrows. Leaving it, or sending with `Enter`, also
/// leaves the marker. A mutant that toggles a block instead fails the send.
#[test]
fn the_queue_keeps_its_keys_and_leaving_it_leaves_navigation() {
    let mut queued = sample();
    queued.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-one", 2, "one"), queue_item("q-two", 4, "two")],
        drain: crate::tui::QueueDrainState::LiveAutoDrain,
    });
    queued.handle_key(key_alt(KeyCode::Down));
    let marker = queued.pane.focused_block;
    queued.queue_focused = true;
    queued.queue_selected = 0;
    queued.handle_key(key(KeyCode::Down));
    assert_eq!(queued.queue_selected, 1, "the queue keeps down");
    assert_eq!(
        queued.pane.focused_block, marker,
        "down did not move the marker"
    );
    assert!(queued.pane.item_nav);
    assert_eq!(queued.input, "");
    queued.handle_key(key(KeyCode::PageUp));
    assert!(
        !queued.queue_focused,
        "page-up leaves the queue for the transcript"
    );
    assert!(
        !queued.pane.item_nav,
        "a key that leaves the queue also leaves navigation"
    );
    let mut sending = sample();
    sending.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-one", 2, "one")],
        drain: crate::tui::QueueDrainState::LiveAutoDrain,
    });
    sending.handle_key(key_alt(KeyCode::Down));
    sending.queue_focused = true;
    assert!(
        sending.handle_key(key(KeyCode::Enter)).is_some(),
        "enter sends the queued prompt instead of toggling a block"
    );
    assert!(
        !sending.pane.item_nav,
        "enter leaves navigation with the queue"
    );
    assert_eq!(sending.input, "", "enter does not type");
}

/// `Alt+s` is not `s`, and `i` still asks to interrupt without copying.
#[test]
fn agent_letters_stay_agent_letters_while_navigating() {
    let mut agent = sample();
    paint(&mut agent, 80, 24);
    agent.open_agent_view("agent-1".into(), "#a · worker".into());
    agent.pane.blocks = vec![ScrollBlock::System("agent notice".into())];
    agent.mark_transcript_structural();
    paint(&mut agent, 80, 24);
    assert!(agent.handle_key(key_alt(KeyCode::Char('s'))).is_none());
    assert!(!agent.pane.item_nav, "alt+s is not a navigation chord");
    assert!(agent.destructive_confirm.is_none(), "alt+s is not s");
    agent.handle_key(key_alt(KeyCode::Up));
    assert_eq!(agent.pane.focused_block, Some(0));
    agent.handle_key(key(KeyCode::Char('i')));
    assert!(
        agent.destructive_confirm.is_some(),
        "i still asks to interrupt"
    );
    assert!(agent.pane.item_nav, "i does not clear the marker");
    assert!(agent.last_nav_copy.is_none(), "i does not copy");
}

fn key_shift(code: KeyCode) -> crossterm::event::KeyEvent {
    let mut event = key(code);
    event.modifiers = crossterm::event::KeyModifiers::SHIFT;
    event
}

fn assert_focused_visible(app: &App) {
    let block = app.pane.focused_block.unwrap();
    let (first, last) = item_row_span(app.pane.transcript_cache.layout(), block).unwrap();
    let start = app.pane.transcript_visible_start;
    let body = usize::from(app.transcript_body_area.unwrap().height);
    assert!(
        first < start + body && last.saturating_add(1) > start,
        "item {first}..={last} outside {start}..{}",
        start + body
    );
}

fn assert_focus_background(frame: &Buffer, label: &str) {
    let mut focused_row = None;
    let mut other_row = None;
    for y in 0..frame.area.height {
        let text: String = (0..frame.area.width)
            .map(|x| frame[(x, y)].symbol())
            .collect();
        if text.contains(label) {
            focused_row = Some(y);
        } else if text.contains("PROMPT-ALPHA") || text.contains("row-01") {
            other_row = Some(y);
        }
    }
    let focused = focused_row.expect(label);
    let other = other_row.expect("another transcript row");
    let focus_bg = frame[(2, focused)].bg;
    let other_bg = frame[(2, other)].bg;
    assert_ne!(focus_bg, other_bg, "the marker is a different background");
    assert_ne!(focus_bg, Color::Reset);
}

fn sent_prompt() -> (App, UserAction) {
    let mut app = App::new(launch_basic(), true);
    app.set_input("stop me".into());
    let action = app.submit_line(false).unwrap();
    let (sender, _receiver) = crate::config::user_action_channel();
    super::terminal::dispatch_action(&mut app, &action, Some(&sender));
    (app, action)
}

fn answer_prompt(app: &mut App, action: &UserAction, stage: usize) {
    let item = queue_item("q1", 1, "stop me");
    if stage > 0 {
        app.apply_host_event(HostEvent::DraftAccepted(action.clone()));
        app.apply_host_event(HostEvent::PromptAdmitted(item.clone()));
    }
    if stage > 1 {
        app.apply_host_event(HostEvent::PromptStarted(Some(item)));
        app.apply_host_event(HostEvent::UserMessage {
            text: "stop me".into(),
            sent_at_ms: None,
        });
    }
}
