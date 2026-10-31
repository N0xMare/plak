//! Follow-live must show the true tail, including after a windowed settle.

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use plakous_host_protocol::views::transcript::TranscriptRow;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde::Deserialize;

use super::test_support::{key, launch_basic, left_mouse, queue_item};
use super::{App, ScrollBlock};
use crate::tui::{ContextCapacityRecovery, QueueDrainState};

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

/// Last line of the scrambled session's final assistant message.
const SCRAMBLED_TAIL: &str = "Xxxx xx xx xxx xxxx xx xxx xxxxxxxx xxxx, xx xxxx xxx xxxxxx?";

#[derive(Deserialize)]
struct ScrambledRow {
    anchor: u64,
    lane: u64,
    ordinal: u64,
    row: TranscriptRow,
}

#[derive(Clone, Copy)]
enum Shelf {
    None,
    Queue,
    Recovery,
}

fn scrambled_session(banner: bool, shelf: Shelf) -> App {
    let fixture: Vec<ScrambledRow> =
        serde_json::from_str(include_str!("fixtures/follow_live_tail.json")).unwrap();
    assert_eq!(fixture.len(), 74);
    assert!(fixture.iter().all(|row| row.lane == 0));
    assert!(
        fixture
            .windows(2)
            .all(|pair| { (pair[0].anchor, pair[0].ordinal) < (pair[1].anchor, pair[1].ordinal) })
    );
    let mut blocks = Vec::with_capacity(fixture.len());
    for item in fixture {
        blocks.push(ScrollBlock::from_row(item.row).expect("every row renders"));
    }
    let ScrollBlock::Assistant(text) = blocks.last().expect("tail") else {
        panic!("the fixture ends on an assistant row");
    };
    assert!(
        text.ends_with(SCRAMBLED_TAIL),
        "scrambled tail line missing"
    );
    let mut app = App::new(launch_basic(), true);
    if banner {
        app.arm_launch_intro_cells();
    }
    match shelf {
        Shelf::None => {}
        Shelf::Queue => app.replace_queue_snapshot(
            vec![queue_item("q-1", 1, "xxxx")],
            QueueDrainState::RestoredPaused,
        ),
        Shelf::Recovery => {
            app.capacity_recovery = Some(ContextCapacityRecovery::Compactable);
        }
    }
    app.pane.blocks = blocks;
    app.mark_transcript_replaced();
    app
}

/// `None` when follow-live shows the final assistant line and hides the arrow.
fn follow_live_gap(app: &App, where_: &str) -> Option<String> {
    let assistant = app
        .pane
        .blocks
        .iter()
        .rposition(|block| matches!(block, ScrollBlock::Assistant(_)))
        .expect("assistant");
    let layout = app.pane.transcript_cache.layout();
    let mut marker_at = layout
        .block_index
        .iter()
        .rposition(|owner| *owner == Some(assistant))
        .expect("assistant rows");
    while marker_at > 0
        && layout
            .plain
            .get(marker_at)
            .is_some_and(|line| line.trim().is_empty())
        && layout.block_index.get(marker_at - 1).copied().flatten() == Some(assistant)
    {
        marker_at -= 1;
    }
    let body = app.transcript_body_area.expect("transcript body");
    let visible_end = app
        .pane
        .transcript_visible_start
        .saturating_add(usize::from(body.height));
    let mut problems = Vec::new();
    if app.pane.scroll_from_bottom != 0 {
        problems.push(format!("from_bottom={}", app.pane.scroll_from_bottom));
    }
    if app.jump_to_live_hit.is_some() {
        problems.push("arrow".to_owned());
    }
    if marker_at < app.pane.transcript_visible_start || marker_at >= visible_end {
        problems.push(format!(
            "tail {marker_at} outside {}..{visible_end}",
            app.pane.transcript_visible_start
        ));
    }
    if problems.is_empty() {
        None
    } else {
        Some(format!(
            "{where_}: {} (rows {})",
            problems.join(", "),
            layout.plain.len()
        ))
    }
}

fn scan_scrambled(
    banner: bool,
    shelf: Shelf,
    width_lo: u16,
    width_hi: u16,
    width_step: usize,
    height_step: usize,
) -> Vec<String> {
    let shelf_label = match shelf {
        Shelf::None => "plain",
        Shelf::Queue => "queue",
        Shelf::Recovery => "recovery",
    };
    let banner_label = if banner { "banner" } else { "no-banner" };
    let mut failures = Vec::new();
    let mut width = width_lo;
    while width <= width_hi {
        let mut app = scrambled_session(banner, shelf);
        let mut height = 20u16;
        while height <= 90 {
            app.pane.scroll_from_bottom = 0;
            app.pane.transcript_view_anchor = None;
            let _terminal = paint(&mut app, width, height);
            if let Some(gap) = follow_live_gap(
                &app,
                &format!("{banner_label}/{shelf_label} {width}x{height}"),
            ) {
                failures.push(gap);
            }
            height = height.saturating_add(u16::try_from(height_step).unwrap_or(1));
        }
        width = width.saturating_add(u16::try_from(width_step).unwrap_or(1));
    }
    failures
}

fn replay_scrambled_actions(width: u16, height: u16) -> Vec<String> {
    let mut app = scrambled_session(true, Shelf::None);
    let mut terminal = paint(&mut app, width, height);
    let mut failures = Vec::new();
    app.handle_key(key(KeyCode::PageUp));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    if let Some(hit) = app.jump_to_live_hit {
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            hit.x,
            hit.y,
        ));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        if let Some(gap) = follow_live_gap(&app, &format!("click {width}x{height}")) {
            failures.push(gap);
        }
    } else if let Some(gap) = follow_live_gap(&app, &format!("page-up {width}x{height}")) {
        failures.push(gap);
    }
    app.pane.scroll_from_bottom = 0;
    let _resized = paint(
        &mut app,
        width.saturating_add(17),
        height.saturating_sub(6).max(20),
    );
    if let Some(gap) = follow_live_gap(&app, &format!("resize {width}x{height}")) {
        failures.push(gap);
    }
    // Scroll back to the end by key after leaving it, not only by the click.
    let mut again = scrambled_session(true, Shelf::None);
    let mut terminal = paint(&mut again, width, height);
    again.handle_key(key(KeyCode::PageUp));
    terminal.draw(|frame| again.draw(frame)).unwrap();
    again.handle_key(key(KeyCode::PageDown));
    terminal.draw(|frame| again.draw(frame)).unwrap();
    if let Some(gap) = follow_live_gap(&again, &format!("page-down {width}x{height}")) {
        failures.push(gap);
    }
    failures
}

/// Ten notes, user `w`, a one-line assistant filler, a user prompt that wraps
/// to three lines, and user `y` with nothing between them. The answer is `n`
/// short paragraphs. A reserve that shrinks after it grows leaves the tail
/// below the body when the height is `11 + 2n`.
fn consecutive_user_prompts_app(paragraphs: usize) -> App {
    let mut blocks = Vec::new();
    for index in 0..10 {
        blocks.push(ScrollBlock::System(format!("note {index}")));
    }
    blocks.push(ScrollBlock::user("w".to_owned()));
    blocks.push(ScrollBlock::Assistant("filler".to_owned()));
    blocks.push(ScrollBlock::user("X".repeat(180)));
    blocks.push(ScrollBlock::user("y".to_owned()));
    let answer = (0..paragraphs)
        .map(|index| {
            if index + 1 == paragraphs {
                TAIL_MARKER.to_owned()
            } else {
                format!("paragraph {index}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    blocks.push(ScrollBlock::Assistant(answer));
    let mut app = App::new(launch_basic(), true);
    app.pane.blocks = blocks;
    app.mark_transcript_replaced();
    app
}

#[test]
fn follow_live_stays_on_the_tail_when_consecutive_user_prompts_change_the_sticky_bar() {
    let mut failures = Vec::new();
    for paragraphs in 1..=39 {
        for width in [60u16, 90] {
            let mut app = consecutive_user_prompts_app(paragraphs);
            let mut height = 10u16;
            while height <= 48 {
                app.pane.scroll_from_bottom = 0;
                app.pane.transcript_view_anchor = None;
                let _terminal = paint(&mut app, width, height);
                if let Some(gap) =
                    follow_live_gap(&app, &format!("prompts n={paragraphs} {width}x{height}"))
                {
                    failures.push(gap);
                }
                height = height.saturating_add(1);
            }
        }
    }
    let shown: Vec<_> = failures.iter().take(24).cloned().collect();
    assert!(
        failures.is_empty(),
        "consecutive user prompts left the tail at {} sizes; first {}:\n{}",
        failures.len(),
        shown.len(),
        shown.join("\n")
    );
}

#[test]
fn follow_live_shows_the_scrambled_session_tail() {
    // Dense only on the widths that missed the tail. Elsewhere a coarse grid.
    let mut failures = scan_scrambled(true, Shelf::None, 60, 240, 20, 10);
    failures.extend(scan_scrambled(true, Shelf::None, 92, 108, 1, 1));
    failures.extend(scan_scrambled(true, Shelf::None, 180, 204, 1, 1));
    failures.extend(scan_scrambled(false, Shelf::None, 80, 220, 40, 20));
    failures.extend(scan_scrambled(true, Shelf::Queue, 80, 220, 40, 20));
    failures.extend(scan_scrambled(true, Shelf::Recovery, 80, 220, 40, 20));
    for width in [96u16, 104, 184, 200] {
        failures.extend(replay_scrambled_actions(width, 48));
    }
    let shown: Vec<_> = failures.iter().take(40).cloned().collect();
    assert!(
        failures.is_empty(),
        "follow-live left the tail at {} sizes; first {}:\n{}",
        failures.len(),
        shown.len(),
        shown.join("\n")
    );
}
