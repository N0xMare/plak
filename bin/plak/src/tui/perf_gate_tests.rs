//! Smoothness gate for the transcript viewport.
//!
//! §11 asks for "an anchor-stability metric gate: scroll anchoring must not
//! drift while blocks above the viewport change size". This file drives a
//! synthetic streaming session through the crate's public layout surface,
//! slices a 120x40 viewport out of every frame, and scores the sequence with
//! [`crate::tui::stability`].
//!
//! Wall time is evidence, not a gate: the timing assertions run only under
//! `PLAK_PERF_WALLCLOCK=1`, so a loaded CI box cannot turn a smoothness
//! regression into a flake or vice versa. `benches/tui_render.rs` reports the
//! numbers.
//!
//! Tail-follow and pinned-stream gates use the product render cache in
//! `perf_gate.rs`; this module covers tool-state changes and the metric itself.
//!
//! Scope note. `App` and its `draw` are crate-private, so an integration test
//! cannot paint a composed frame. What it *can* reach is the transcript layout
//! that dominates that frame, which is what the anchor property is about. The
//! composed-frame variants of these two gates, plus the incremental render
//! cache, retention, and byte-bounded cache assertions, need crate-private
//! types and live in `src/perf_gate.rs`.

#![allow(
    missing_docs,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test crate may unwrap; frame counts are small integers"
)]

use std::time::Instant;

use crate::tui::stability::{AnchorStability, StabilityBudget, ViewportMotion};
use crate::tui::{LineLayout, ScrollBlock, layout_blocks};

const COLS: usize = 120;
const ROWS: usize = 40;

/// True only when the operator opted into wall-clock assertions.
///
/// These assume a release build; an unoptimized layout pass is an order of
/// magnitude slower and would fail budgets the product never sees.
fn wallclock_enabled() -> bool {
    std::env::var("PLAK_PERF_WALLCLOCK").is_ok_and(|value| value == "1")
}

/// A transcript shaped like a real session.
fn synthetic_transcript(blocks: usize) -> Vec<ScrollBlock> {
    (0..blocks)
        .map(|index| match index % 4 {
            0 => ScrollBlock::user(format!(
                "turn {index}: please look at the redraw cadence and report back"
            )),
            1 => ScrollBlock::Assistant(format!(
                "Reading the schedule for turn {index}. The frame timer is armed by demand, \
                 not by a fixed tick, so an idle session parks and consumes no CPU."
            )),
            2 => ScrollBlock::Tool {
                call_id: Some(format!("call-{index}")),
                name: "read_file".to_owned(),
                state: "done".to_owned(),
                subject: Some(format!("crates/plak/tui/src/ui.rs:{index}")),
                detail: None,
                parent_call_id: None,
                cell_id: None,
                kind: plakous_host_protocol::views::ActivityKind::Tool,
                mode: crate::tui::DisplayMode::Collapsed,
                manual: false,
                group_open: false,
                wait: None,
                started_at_ms: None,
                finished_at_ms: None,
                view: crate::tui::blocks::ItemView::default(),
            },
            _ => ScrollBlock::System(format!("checkpoint {index} recorded")),
        })
        .collect()
}

/// Rows `start..start + ROWS` of a laid-out transcript, padded to the viewport.
fn viewport(layout: &LineLayout, start: usize) -> Vec<String> {
    (start..start + ROWS)
        .map(|row| layout.plain.get(row).cloned().unwrap_or_default())
        .collect()
}

#[test]
fn a_tool_row_changing_state_moves_nothing_above_it() {
    // §11: "a tool row changing state marks that block dirty" and must not
    // reflow the transcript. From the viewport's side, the observable claim is
    // that rows above the tool row are byte-identical across the tick.
    let mut blocks = synthetic_transcript(400);
    let tool_index = blocks
        .iter()
        .rposition(|block| matches!(block, ScrollBlock::Tool { .. }))
        .expect("fixture carries tool rows");

    let mut metric = AnchorStability::for_viewport(ROWS);
    let before = layout_blocks(&blocks, COLS);
    let tool_row = before
        .block_index
        .iter()
        .position(|owner| *owner == Some(tool_index))
        .expect("tool row is laid out");
    // The window sits strictly above the tool row: that is the claim, and it
    // keeps the tool row's own changing text out of the metric.
    assert!(
        tool_row >= ROWS,
        "fixture must leave a full window above the row"
    );
    let pinned_start = tool_row - ROWS;
    metric.observe(&viewport(&before, pinned_start), ViewportMotion::Pinned);

    for state in ["running", "done", "rejected (policy)"] {
        if let Some(ScrollBlock::Tool { state: slot, .. }) = blocks.get_mut(tool_index) {
            *slot = state.to_owned();
        }
        let layout = layout_blocks(&blocks, COLS);
        metric.observe(&viewport(&layout, pinned_start), ViewportMotion::Pinned);
    }

    let report = metric.report();
    assert!(
        report.violations(&StabilityBudget::perfect()).is_empty(),
        "a tool state tick disturbed the viewport: {report:?}"
    );
}

#[test]
fn a_resize_is_the_only_transition_allowed_to_reflow() {
    // A resize legitimately rewrites every row. The metric must still be able
    // to see it, or the gate would be blind to a reflow that fires without a
    // resize. This asserts the instrument, not the product.
    let blocks = synthetic_transcript(400);
    let mut metric = AnchorStability::for_viewport(ROWS);

    let wide = layout_blocks(&blocks, 200);
    metric.observe(&viewport(&wide, 0), ViewportMotion::Deliberate);
    let narrow = layout_blocks(&blocks, 40);
    metric.observe(&viewport(&narrow, 0), ViewportMotion::Deliberate);

    let report = metric.report();
    assert_eq!(report.repositions, 0, "deliberate moves are not drift");
    assert!(
        report.mass_reflows >= 1,
        "the metric must notice a full-width reflow: {report:?}"
    );
}

#[test]
fn first_frame_layout_of_an_empty_transcript_is_immediate() {
    let blocks: Vec<ScrollBlock> = Vec::new();
    let started = Instant::now();
    let layout = layout_blocks(&blocks, COLS);
    let elapsed = started.elapsed();
    assert_eq!(layout.plain, [] as [String; 0]);

    if wallclock_enabled() {
        assert!(
            elapsed.as_millis() < 50,
            "first-frame transcript layout took {elapsed:?}, above the 50 ms budget"
        );
    }
}
