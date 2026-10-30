//! Follow-live must show the true tail, including after a windowed settle.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::test_support::{launch_basic, left_mouse};
use super::{App, ScrollBlock};

const TAIL_MARKER: &str = "VISIBLE_TAIL_LINE";

/// Markdown whose exact render is taller than the byte estimate: headings,
/// lists and a paragraph with no blank lines between them, so the renderer
/// inserts the separator rows the estimate does not count.
fn underestimated_block(index: usize) -> String {
    format!(
        "# Block {index}\n## Detail\n- alpha item\n- beta item\n1. first step\n2. second step\nParagraph with `code` about block {index}."
    )
}

/// Long final assistant message: headings, bullets, ordered items, inline
/// code, and a one-line last paragraph. Lines stay short so width changes
/// the wrap by a row or two, not by a screen.
fn final_markdown() -> String {
    let mut text = String::new();
    for index in 0..16 {
        text.push_str(&format!(
            "# Section {index}\n## Notes\n- bullet one\n- bullet two\n1. ordered step\n2. second step\nA paragraph with `inline code` number {index}.\n"
        ));
    }
    text.push_str(&format!("\n{TAIL_MARKER}"));
    text
}

fn paint(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal
}

fn assert_follow_live_shows_tail(app: &App, where_: &str) {
    let plain = app.pane.transcript_cache.plain();
    let marker_at = plain
        .iter()
        .rposition(|line| line.contains(TAIL_MARKER))
        .unwrap_or_else(|| panic!("{where_}: marker missing from the projection"));
    let body = app.transcript_body_area.expect("transcript body");
    let visible_end = app
        .pane
        .transcript_visible_start
        .saturating_add(usize::from(body.height));
    assert_eq!(
        app.pane.scroll_from_bottom,
        0,
        "{where_}: follow-live left scroll_from_bottom={}, marker row {marker_at}, visible {}..{visible_end}, rows {}",
        app.pane.scroll_from_bottom,
        app.pane.transcript_visible_start,
        plain.len()
    );
    assert!(
        app.jump_to_live_hit.is_none(),
        "{where_}: the jump arrow is showing at the tail"
    );
    assert!(
        marker_at >= app.pane.transcript_visible_start && marker_at < visible_end,
        "{where_}: tail row {marker_at} is outside the body {}..{visible_end}",
        app.pane.transcript_visible_start
    );
}

/// 460 estimated markdown blocks, then a short exact tail: 62 notes, the
/// user prompt, and the long final answer. The tail is short enough that a
/// tall pane's follow-live window reaches the estimates.
fn windowed_tail_app() -> App {
    let mut blocks = Vec::with_capacity(524);
    for index in 0..460 {
        blocks.push(ScrollBlock::Assistant(underestimated_block(index)));
    }
    for index in 0..62 {
        blocks.push(ScrollBlock::System(format!("note {index}")));
    }
    blocks.push(ScrollBlock::user("please answer".to_owned()));
    blocks.push(ScrollBlock::Assistant(final_markdown()));
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    app.pane.blocks = blocks;
    app.mark_transcript_replaced();
    app
}

#[test]
fn follow_live_shows_the_markdown_tail_after_windowed_settle() {
    for width in [72u16, 100, 120] {
        let mut app = windowed_tail_app();
        let _terminal = paint(&mut app, width, 160);
        assert!(
            !app.pane.transcript_cache.estimated_blocks().is_empty(),
            "width {width}: history must stay estimated so settle can correct it"
        );
        assert_follow_live_shows_tail(&app, &format!("width {width} first paint"));
        let _terminal = paint(&mut app, width, 160);
        assert_follow_live_shows_tail(&app, &format!("width {width} second paint"));
    }
}

#[test]
fn clicking_jump_and_resizing_return_to_the_markdown_tail() {
    for width in [72u16, 100, 120] {
        let mut app = windowed_tail_app();
        // The first paint is detached, so the tail stays estimated until the
        // jump. A follow-live paint first would already have settled it.
        app.pane.scroll_from_bottom = 4_000;
        let mut terminal = paint(&mut app, width, 160);
        assert!(
            !app.pane.transcript_cache.estimated_blocks().is_empty(),
            "width {width}: the tail must still be estimated when the jump is clicked"
        );
        assert!(
            app.pane.scroll_from_bottom > 0,
            "width {width}: the first paint is detached"
        );
        let hit = app
            .jump_to_live_hit
            .unwrap_or_else(|| panic!("width {width}: no jump arrow while detached"));
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            hit.x,
            hit.y,
        ));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_follow_live_shows_tail(&app, &format!("width {width} after click"));

        // A new width has not settled this tail. Follow-live there is the
        // resize path, not a second paint of an already exact window.
        let mut resized = windowed_tail_app();
        resized.pane.scroll_from_bottom = 4_000;
        let _detached = paint(&mut resized, width, 160);
        assert!(
            !resized.pane.transcript_cache.estimated_blocks().is_empty(),
            "width {width}: the resize width must start from estimates"
        );
        resized.pane.scroll_from_bottom = 0;
        let _follow = paint(&mut resized, width.saturating_add(17), 140);
        assert_follow_live_shows_tail(&resized, &format!("width {width} after resize"));
    }
}

#[test]
fn follow_live_shows_a_fully_measured_markdown_tail() {
    for width in [40u16, 72, 110] {
        let mut blocks = Vec::new();
        for index in 0..40 {
            blocks.push(ScrollBlock::System(format!("note {index}")));
        }
        blocks.push(ScrollBlock::user("please answer".to_owned()));
        blocks.push(ScrollBlock::Assistant(final_markdown()));
        let mut app = App::new(launch_basic(), true);
        app.arm_launch_intro_cells();
        app.pane.blocks = blocks;
        app.mark_transcript_replaced();
        let _terminal = paint(&mut app, width, 36);
        assert!(
            app.pane.transcript_cache.estimated_blocks().is_empty(),
            "width {width}: a short transcript is measured exactly"
        );
        assert_follow_live_shows_tail(&app, &format!("exact width {width}"));
    }
}
