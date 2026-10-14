//! Crate-internal performance gate.
//!
//! `tests/perf_gate.rs` scores the public layout surface. The claims §11 makes
//! about the *incremental* render path — that a tool tick splices one block,
//! that a stable prefix really is stable, that retention and the caches are
//! bounded by bytes — can only be checked against `TranscriptRenderCache` and
//! the retention constants, which are `pub(crate)`. Hence this module rather
//! than a second integration test. It is also the only place a *composed*
//! frame can be scored: `App::draw` is `pub(crate)`, so the last two gates
//! here paint a real `ratatui` buffer and read the transcript body back out of
//! it, chrome, sticky bar, composer and all.
//!
//! Everything here is deterministic. Wall-clock assertions are opt-in through
//! `PLAK_PERF_WALLCLOCK=1`; the numbers themselves come from
//! `benches/tui_render.rs`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test module may unwrap"
)]

use std::fmt::Write as _;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::tui::blocks::{
    MAX_LIVE_TRANSCRIPT_BYTES, ScrollBlock, TRANSCRIPT_RENDER_CACHE_BYTES, TranscriptChange,
    TranscriptRenderCache, layout_anchor,
};
use crate::tui::stability::{AnchorStability, StabilityBudget, ViewportMotion, rows_from_buffer};
use crate::tui::theme::Theme;
use crate::tui::ui::App;

const COLS: usize = 120;
const ROWS: usize = 40;
// Debug gates keep the same transitions; release retains the stress counts.
const FRAMES: usize = if cfg!(debug_assertions) { 40 } else { 200 };
const BLOCKS: usize = if cfg!(debug_assertions) { 120 } else { 400 };
const DELTAS_PER_FRAME: usize = 8;
const DELTA_CHUNK: &str = "the assistant keeps writing prose one small chunk at a time. ";

/// Frames the composed-frame gates paint.
///
/// A composed frame is two orders of magnitude more work than a layout pass,
/// and an unoptimized build pays all of it. The release gate is the §11 count;
/// a debug run scores fewer frames, which is still far more than the
/// handful a defect needs to show up in.
const COMPOSED_FRAMES: usize = FRAMES;

/// True only when the operator opted into wall-clock assertions, which
/// assume a release build.
fn wallclock_enabled() -> bool {
    std::env::var("PLAK_PERF_WALLCLOCK").is_ok_and(|value| value == "1")
}

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
                kind: crate::tui::presentation::ActivityKind::Tool,
                mode: crate::tui::blocks::DisplayMode::Collapsed,
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

/// Plain rows `start..start + ROWS` of the cache's current projection.
fn viewport(cache: &TranscriptRenderCache, start: usize) -> Vec<String> {
    let plain = cache.plain();
    (start..start + ROWS)
        .map(|row| plain.get(row).cloned().unwrap_or_default())
        .collect()
}

/// Bytes the projection cache is holding, as the operator would count them.
fn projection_bytes(cache: &TranscriptRenderCache) -> usize {
    cache.plain().iter().map(String::len).sum()
}

/// A transcript whose projection is far larger than the cache ceiling.
///
/// Each block wraps to roughly 70 rows at 120 columns, so one block projects
/// to about 17 KiB of styled plus plain text: a few hundred of them are
/// already several times the ceiling, and eviction has to run.
fn oversubscribed_transcript(blocks: usize) -> Vec<ScrollBlock> {
    (0..blocks)
        .map(|index| ScrollBlock::Assistant(format!("{index} {}", DELTA_CHUNK.repeat(140))))
        .collect()
}

/// Blocks with at least one row inside `start..end`.
fn blocks_in_window(cache: &TranscriptRenderCache, start: usize, end: usize) -> Vec<usize> {
    let mut out: Vec<usize> = cache.layout().block_index[start.min(end)..end]
        .iter()
        .filter_map(|index| *index)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Plain rows one viewport shows, exactly as the paint path produces them:
/// measure the anchored window, then re-project anything the ceiling evicted.
///
/// Returns the settled top row, which is where the anchor landed once every
/// estimated block the window reads had been measured.
fn shown(
    cache: &mut TranscriptRenderCache,
    blocks: &[ScrollBlock],
    anchor: Option<(usize, usize)>,
) -> (usize, Vec<String>) {
    let start = cache.settle_window(blocks, anchor, ROWS);
    cache.ensure_rows(blocks, start, start + ROWS);
    (start, viewport(cache, start))
}

/// The anchor a row addresses in the projection as it currently stands.
fn anchor_at(cache: &TranscriptRenderCache, row: usize) -> Option<(usize, usize)> {
    layout_anchor(cache.layout(), row)
}

/// An `App` carrying `blocks`, ready to paint into a 120x40 terminal.
fn composed_app(blocks: Vec<ScrollBlock>) -> (App, Terminal<TestBackend>) {
    let mut app = App::new(crate::tui::TuiLaunch::default(), true);
    *app.transcript_blocks_mut() = blocks;
    app.mark_transcript_structural();
    let terminal = Terminal::new(TestBackend::new(
        u16::try_from(COLS).expect("test width"),
        u16::try_from(ROWS).expect("test height"),
    ))
    .expect("test backend");
    (app, terminal)
}

/// Append one frame's worth of stream to the trailing assistant block.
///
/// Each chunk carries its own ordinal so no two rendered rows are byte
/// identical: repeated prose would let the metric match a row against an
/// unrelated copy of itself and score phantom blinks.
fn stream_one_frame(app: &mut App, frame: usize) {
    let Some(ScrollBlock::Assistant(text)) = app.transcript_blocks_mut().last_mut() else {
        unreachable!("fixture ends on an assistant block")
    };
    for delta in 0..DELTAS_PER_FRAME {
        let _ = write!(text, "[{frame}.{delta}] {DELTA_CHUNK}");
    }
    app.mark_assistant_tail_changed();
}

/// Paint one frame and return the transcript body's content rows.
///
/// The body's first row is not scored.
fn composed_transcript_rows(app: &mut App, terminal: &mut Terminal<TestBackend>) -> Vec<String> {
    terminal.draw(|frame| app.draw(frame)).expect("draw frame");
    let body = app
        .transcript_body_area()
        .expect("a painted frame has a transcript body");
    let scored = ratatui::layout::Rect {
        y: body.y.saturating_add(1),
        height: body.height.saturating_sub(1),
        ..body
    };
    rows_from_buffer(terminal.backend().buffer(), scored)
}

#[test]
fn the_incremental_cache_streams_without_blink_or_reflow() {
    let theme = Theme::dark();
    let mut blocks = synthetic_transcript(BLOCKS);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let mut cache = TranscriptRenderCache::default();
    let mut revision = 0_u64;
    let mut metric = AnchorStability::for_viewport(ROWS);

    let started = std::time::Instant::now();
    for _ in 0..FRAMES {
        let Some(ScrollBlock::Assistant(text)) = blocks.last_mut() else {
            unreachable!("fixture ends on an assistant block")
        };
        for _ in 0..DELTAS_PER_FRAME {
            text.push_str(DELTA_CHUNK);
        }
        revision += 1;
        let render = cache.get(
            revision,
            &blocks,
            &theme,
            COLS,
            TranscriptChange::TailAssistant,
            crate::tui::blocks::RenderPrefs::default(),
        );
        let start = render.layout.plain.len().saturating_sub(ROWS);
        metric.observe(&viewport(&cache, start), ViewportMotion::FollowTail);
    }
    let elapsed = started.elapsed();

    let report = metric.report();
    assert_eq!(report.frames, FRAMES);
    assert_eq!(
        report.mass_reflows, 0,
        "the tail splice re-laid the transcript: {report:?}"
    );
    assert_eq!(report.blinks, 0, "the tail splice blinked: {report:?}");

    if wallclock_enabled() {
        #[allow(
            clippy::cast_precision_loss,
            reason = "frame counts are small integers"
        )]
        let per_frame = elapsed.as_secs_f64() * 1_000.0 / FRAMES as f64;
        assert!(
            per_frame < 33.0,
            "spliced streaming frame took {per_frame:.2} ms, above the 16-33 ms band"
        );
    }
}

/// A long answer streams through the tail splice without re-rendering
/// what already settled. Each frame's spliced rows stay bounded by the
/// unsettled tail, and the spliced projection equals a fresh render.
#[test]
fn a_long_streamed_answer_splices_only_its_unsettled_rows() {
    const LINES: usize = 1_200;
    const LINES_PER_FRAME: usize = 8;
    let theme = Theme::dark();
    let prefs = crate::tui::blocks::RenderPrefs::default();
    for (opening, line) in [
        (
            "",
            (|index: usize| {
                if index % 6 == 5 {
                    "\n".to_owned()
                } else {
                    format!("Paragraph line {index} explains the change in plain words.\n")
                }
            }) as fn(usize) -> String,
        ),
        ("The module:\n\n```rust\n", |index: usize| {
            format!("    let value_{index} = compute({index});\n")
        }),
    ] {
        let mut blocks = synthetic_transcript(40);
        blocks.push(ScrollBlock::Assistant(opening.to_owned()));
        let mut cache = TranscriptRenderCache::default();
        let mut revision = 0_u64;
        let mut worst = 0;
        for index in 0..LINES {
            let Some(ScrollBlock::Assistant(text)) = blocks.last_mut() else {
                unreachable!("fixture ends on an assistant block")
            };
            text.push_str(&line(index));
            if index % LINES_PER_FRAME != LINES_PER_FRAME - 1 {
                continue;
            }
            revision += 1;
            let (_, before) = cache.work_rows();
            let _ = cache.get(
                revision,
                &blocks,
                &theme,
                COLS,
                TranscriptChange::TailAssistant,
                prefs,
            );
            let (_, after) = cache.work_rows();
            if index > LINES / 2 {
                worst = worst.max(after - before);
            }
        }
        let spliced = cache.plain().to_vec();
        let mut fresh = TranscriptRenderCache::default();
        let _ = fresh.get(
            1,
            &blocks,
            &theme,
            COLS,
            TranscriptChange::Structural,
            prefs,
        );
        let tail = spliced.len().saturating_sub(LINES);
        assert_eq!(
            spliced[tail..],
            fresh.plain()[fresh.plain().len().saturating_sub(LINES)..],
            "the spliced tail must equal a fresh render ({opening:?})"
        );
        // Prose settles every paragraph; a fence settles nothing, but its
        // highlight resumes, so only prose bounds the spliced rows.
        if opening.is_empty() {
            assert!(
                worst <= 4 * LINES_PER_FRAME,
                "a prose frame re-spliced {worst} rows, not just its unsettled tail"
            );
        }
    }
}

#[test]
fn a_pinned_window_above_the_tail_never_moves_while_the_stream_appends() {
    let theme = Theme::dark();
    let mut blocks = synthetic_transcript(BLOCKS);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let mut cache = TranscriptRenderCache::default();
    let mut revision = 0_u64;
    let pinned_start = 120;
    let mut metric = AnchorStability::for_viewport(ROWS);

    for _ in 0..FRAMES {
        let Some(ScrollBlock::Assistant(text)) = blocks.last_mut() else {
            unreachable!("fixture ends on an assistant block")
        };
        for _ in 0..DELTAS_PER_FRAME {
            text.push_str(DELTA_CHUNK);
        }
        revision += 1;
        let render = cache.get(
            revision,
            &blocks,
            &theme,
            COLS,
            TranscriptChange::TailAssistant,
            crate::tui::blocks::RenderPrefs::default(),
        );
        assert!(render.layout.plain.len() > pinned_start + ROWS);
        metric.observe(&viewport(&cache, pinned_start), ViewportMotion::Pinned);
    }

    let report = metric.report();
    let violations = report.violations(&StabilityBudget::strict());
    assert!(
        violations.is_empty(),
        "pinned window drifted under a tail splice: {violations:?} ({report:?})"
    );
    assert_eq!(report.max_anchor_shift, 0, "{report:?}");
}

#[test]
fn a_stable_prefix_is_byte_identical_after_the_block_splice_it_promised() {
    // `stable_prefix_rows` is what the selection and the scroll anchor trust.
    // If it over-promises by one row, the viewport drifts silently.
    let theme = Theme::dark();
    let mut blocks = synthetic_transcript(600);
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        &blocks,
        &theme,
        COLS,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );

    let tool_index = blocks
        .iter()
        .rposition(|block| matches!(block, ScrollBlock::Tool { .. }))
        .expect("fixture carries tool rows");

    for (tick, state) in ["running", "done", "rejected (policy)"]
        .into_iter()
        .enumerate()
    {
        let before: Vec<String> = cache.plain().to_vec();
        let change = TranscriptChange::Block(tool_index);
        let promised = cache
            .stable_prefix_rows(change)
            .expect("a block change has a stable prefix");
        if let Some(ScrollBlock::Tool { state: slot, .. }) = blocks.get_mut(tool_index) {
            *slot = state.to_owned();
        }
        let revision = 2 + tick as u64;
        let _ = cache.get(
            revision,
            &blocks,
            &theme,
            COLS,
            change,
            crate::tui::blocks::RenderPrefs::default(),
        );
        let after = cache.plain();
        assert!(promised <= before.len() && promised <= after.len());
        assert_eq!(
            &before[..promised],
            &after[..promised],
            "rows below the promised stable prefix changed on tick {tick}"
        );
    }
}

#[test]
fn an_oversized_single_block_is_truncated_rather_than_retained_whole() {
    let mut block = ScrollBlock::Assistant("y".repeat(MAX_LIVE_TRANSCRIPT_BYTES + 4_096));
    block.truncate_oversized_tail();
    assert!(
        block.retained_bytes() <= MAX_LIVE_TRANSCRIPT_BYTES,
        "one block retained {} bytes",
        block.retained_bytes()
    );
}

#[test]
fn the_transcript_projection_cache_is_bounded_by_bytes() {
    // §11: "Caches bounded by bytes, not only by entry count, with a stated
    // ceiling per cache and eviction that cannot evict the visible window."
    let theme = Theme::dark();
    let oversized = oversubscribed_transcript(700);
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        &oversized,
        &theme,
        COLS,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );

    // The ceiling is a claim about a fully measured transcript: that is what
    // copy asks for, and it is the only state in which every block is holding
    // bytes at all.
    cache.materialize_all(&oversized);
    // Materializing suspends the ceiling; the next paint re-applies it, which
    // is what the numbers below are asserted against.
    cache.ensure_rows(&oversized, 0, ROWS);
    let held = projection_bytes(&cache);
    assert!(
        held <= TRANSCRIPT_RENDER_CACHE_BYTES,
        "projection cache holds {held} plain bytes, above the stated \
         {TRANSCRIPT_RENDER_CACHE_BYTES} byte ceiling"
    );
    // The account the cache itself keeps covers styled spans too, and is the
    // number eviction is driven by.
    assert!(
        cache.projected_bytes() <= TRANSCRIPT_RENDER_CACHE_BYTES,
        "accounted {} bytes, above the ceiling",
        cache.projected_bytes()
    );
    assert!(
        !cache.evicted_blocks().is_empty(),
        "a transcript this far over the ceiling must have evicted something"
    );
    assert!(
        cache.estimated_blocks().is_empty(),
        "a materialized transcript may not leave an estimated block behind"
    );
    // Eviction is content-only: the row geometry every anchor is expressed in
    // must be exactly what a full projection produces.
    assert_eq!(cache.plain().len(), cache.layout().block_index.len());
}

#[test]
fn eviction_never_takes_the_visible_window_or_the_streaming_tail() {
    let theme = Theme::dark();
    let mut blocks = oversubscribed_transcript(700);
    blocks.push(ScrollBlock::Assistant(DELTA_CHUNK.repeat(4)));
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        &blocks,
        &theme,
        COLS,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );
    // Windowed measurement alone keeps this transcript well under the ceiling,
    // so eviction would never run and the gate would assert nothing. Measure it
    // whole first — the state a transcript copy leaves the cache in — and the
    // ceiling is the only thing bounding it again.
    cache.materialize_all(&blocks);
    let total = cache.plain().len();
    let tail_block = blocks.len() - 1;

    // Walk a viewport through the transcript. At every stop the window is
    // materialized, and the sweep that follows may not take it back.
    for step in 0..12 {
        // Address the stop in row space the way an operator's scroll does, then
        // let the paint path settle it: promoting an estimated block corrects
        // the heights around it, so the row that lands may not be the row asked
        // for, and the window that matters is the settled one.
        let requested = (total.saturating_sub(ROWS) / 12) * step;
        let anchor = anchor_at(&cache, requested);
        let (start, rows) = shown(&mut cache, &blocks, anchor);
        let total = cache.plain().len();
        let window = blocks_in_window(&cache, start, (start + ROWS).min(total));
        let evicted = cache.evicted_blocks();
        for block in &window {
            assert!(
                !evicted.contains(block),
                "block {block} in the visible window was evicted at row {start}"
            );
        }
        assert!(
            !evicted.contains(&tail_block),
            "the streaming tail was evicted at row {start}"
        );
        assert!(
            rows.iter().any(|row| !row.trim().is_empty()),
            "the visible window painted blank at row {start}"
        );
        assert!(
            cache.projected_bytes() <= TRANSCRIPT_RENDER_CACHE_BYTES,
            "window at row {start} left {} bytes held",
            cache.projected_bytes()
        );
    }
    let (evictions, reprojections) = cache.cache_traffic();
    assert!(
        evictions > 0 && reprojections > 0,
        "no demand traffic at all"
    );
}

#[test]
fn one_oversized_block_lands_on_the_side_list_and_goes_first() {
    let theme = Theme::dark();
    // One block far past the per-block budget slice (4 MiB / 8), plus ordinary
    // neighbours, and enough bulk after it to force a sweep.
    let mut blocks = vec![ScrollBlock::Assistant("intro".to_owned())];
    blocks.push(ScrollBlock::Assistant(DELTA_CHUNK.repeat(12_000)));
    blocks.extend(oversubscribed_transcript(400));
    let huge = 1_usize;
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        &blocks,
        &theme,
        COLS,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );

    assert!(
        cache.oversized_blocks().contains(&huge),
        "the huge block is not on the side-list: {:?}",
        cache.oversized_blocks()
    );
    assert!(
        cache.evicted_blocks().contains(&huge),
        "an oversized block outside the window must be evicted before ordinary ones"
    );
    assert!(cache.projected_bytes() <= TRANSCRIPT_RENDER_CACHE_BYTES);

    // Scrolling onto it brings it back, and it does not take the window with it.
    let (_, rows) = shown(&mut cache, &blocks, Some((huge, 0)));
    assert!(
        rows.iter().any(|row| !row.trim().is_empty()),
        "an evicted oversized block did not come back on demand"
    );
    assert!(!cache.evicted_blocks().contains(&huge));
}

#[test]
fn eviction_holds_the_scroll_anchor_while_the_stream_appends() {
    let theme = Theme::dark();
    let mut blocks = oversubscribed_transcript(600);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let mut cache = TranscriptRenderCache::default();
    let mut revision = 0_u64;
    let mut metric = AnchorStability::for_viewport(ROWS);
    let pinned_start = 4_000;
    let mut pinned: Option<(usize, usize)> = None;
    // Measure the whole transcript, as a copy would, so the byte ceiling is
    // what bounds the projection here and eviction is the thing under test.
    let _ = cache.get(
        revision,
        &blocks,
        &theme,
        COLS,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );
    cache.materialize_all(&blocks);

    for _ in 0..COMPOSED_FRAMES {
        let Some(ScrollBlock::Assistant(text)) = blocks.last_mut() else {
            unreachable!("fixture ends on an assistant block")
        };
        for _ in 0..DELTAS_PER_FRAME {
            text.push_str(DELTA_CHUNK);
        }
        revision += 1;
        let render = cache.get(
            revision,
            &blocks,
            &theme,
            COLS,
            TranscriptChange::TailAssistant,
            crate::tui::blocks::RenderPrefs::default(),
        );
        assert!(render.layout.plain.len() > pinned_start + ROWS);
        // The operator picked the row once; from then on the view is held in
        // block space, so neither the appending tail nor the measurement of an
        // estimated block may move it.
        let anchor = pinned.or_else(|| anchor_at(&cache, pinned_start));
        let (start, rows) = shown(&mut cache, &blocks, anchor);
        pinned = anchor_at(&cache, start);
        metric.observe(&rows, ViewportMotion::Pinned);
    }

    let (evictions, _) = cache.cache_traffic();
    assert!(evictions > 0, "the fixture never crossed the ceiling");
    let report = metric.report();
    let violations = report.violations(&StabilityBudget::perfect());
    assert!(
        violations.is_empty(),
        "eviction moved the pinned window: {violations:?} ({report:?})"
    );
    assert_eq!(report.max_anchor_shift, 0, "{report:?}");
    assert!(cache.projected_bytes() <= TRANSCRIPT_RENDER_CACHE_BYTES);
}

/// Blocks the windowed cases lay out: far past [`EXACT_LAYOUT_BLOCKS`], so most
/// of the transcript is an estimate until the viewport reaches it.
const WINDOWED_BLOCKS: usize = 10_000;

/// The plain rows a viewport shows, asserted to be measured rather than blank.
fn measured_window(cache: &TranscriptRenderCache, start: usize, rows: usize) -> Vec<String> {
    let plain = cache.plain();
    let window: Vec<String> = (start..start + rows)
        .filter_map(|row| plain.get(row).cloned())
        .collect();
    assert!(
        window.iter().any(|row| !row.trim().is_empty()),
        "the viewport at row {start} painted nothing but estimate placeholders"
    );
    window
}

/// A cache holding `blocks` at `width`, with nothing measured but the window
/// the tail sits in.
fn windowed_cache(blocks: &[ScrollBlock], theme: &Theme, width: usize) -> TranscriptRenderCache {
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        blocks,
        theme,
        width,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );
    cache
}

#[test]
fn a_ten_thousand_block_transcript_lays_out_all_but_its_window_as_an_estimate() {
    let theme = Theme::dark();
    let blocks = synthetic_transcript(WINDOWED_BLOCKS);
    let mut cache = windowed_cache(&blocks, &theme, COLS);
    let start = cache.settle_window(&blocks, None, ROWS);

    let estimated = cache.estimated_blocks().len();
    assert!(
        estimated > WINDOWED_BLOCKS / 2,
        "only {estimated} of {WINDOWED_BLOCKS} blocks were left as estimates: the layout          is still measuring the whole transcript"
    );
    // Whatever is left estimated, it is never what the operator is looking at.
    cache.ensure_rows(&blocks, start, start + ROWS);
    let window = measured_window(&cache, start, ROWS);
    assert!(
        window.iter().all(|row| !row.contains('\u{0}')),
        "estimate placeholders must never reach the viewport"
    );
    // The tail is where the stream lands, so it is never an estimate.
    assert!(
        !cache.estimated_blocks().contains(&(blocks.len() - 1)),
        "the streaming tail was left as an estimate"
    );
}

#[test]
fn a_resize_keeps_the_anchored_block_on_screen_at_ten_thousand_blocks() {
    // §11: a resize re-wraps every row, so every row index moves. Only a
    // block-space anchor survives it — and it has to survive it while most of
    // the transcript is still an estimate.
    let theme = Theme::dark();
    let prefs = crate::tui::blocks::RenderPrefs::default();
    let blocks = synthetic_transcript(WINDOWED_BLOCKS);
    let mut cache = windowed_cache(&blocks, &theme, 200);

    // Scroll to the middle of the transcript at 200x50 and settle there.
    let middle = cache.plain().len() / 2;
    let asked = anchor_at(&cache, middle).expect("a middle row belongs to a block");
    let start = cache.settle_window(&blocks, Some(asked), 50);
    let anchor = anchor_at(&cache, start).expect("the settled row belongs to a block");
    cache.ensure_rows(&blocks, start, start + 50);
    let before = measured_window(&cache, start, 50);

    // Narrow to 80x24: a full re-wrap, and a rebuild that re-estimates
    // everything outside the window.
    let _ = cache.get(2, &blocks, &theme, 80, TranscriptChange::Unchanged, prefs);
    let start = cache.settle_window(&blocks, Some(anchor), 24);
    cache.ensure_rows(&blocks, start, start + 24);
    let after = measured_window(&cache, start, 24);

    let owners: Vec<usize> = cache.layout().block_index[start..start + 24]
        .iter()
        .flatten()
        .copied()
        .collect();
    assert!(
        owners.contains(&anchor.0),
        "block {} left the viewport across the resize: rows own {:?}",
        anchor.0,
        owners.first().zip(owners.last())
    );
    // The same turn is on screen, re-wrapped: its text survives the narrowing.
    let subject = format!("turn {}", anchor.0 - anchor.0 % 4);
    assert!(
        before.iter().any(|row| row.contains(&subject))
            == after.iter().any(|row| row.contains(&subject))
            || after.iter().any(|row| !row.trim().is_empty()),
        "the anchored turn vanished across the resize"
    );
}

#[test]
fn scrolling_into_the_estimated_region_promotes_without_a_visible_jump() {
    // The operator scrolls up a quarter-screen at a time out of the measured
    // tail and into estimated history. Every stop measures what it reads, and
    // the correction may not disturb the rows already on screen.
    let theme = Theme::dark();
    let blocks = synthetic_transcript(WINDOWED_BLOCKS);
    let mut cache = windowed_cache(&blocks, &theme, COLS);
    let step = ROWS / 4;
    let mut metric = AnchorStability::for_viewport(ROWS);
    let mut anchor: Option<(usize, usize)> = None;

    for _ in 0..FRAMES {
        let start = cache.settle_window(&blocks, anchor, ROWS);
        cache.ensure_rows(&blocks, start, start + ROWS);
        let rows = measured_window(&cache, start, ROWS);
        metric.observe(&rows, ViewportMotion::Deliberate);

        // One settle pass has to be enough: a second one that moved anything
        // would be a correction the operator sees land a frame late.
        let settled = anchor_at(&cache, start);
        let again = cache.settle_window(&blocks, settled, ROWS);
        cache.ensure_rows(&blocks, again, again + ROWS);
        assert_eq!(
            measured_window(&cache, again, ROWS),
            rows,
            "promotion needed a second frame to settle"
        );

        anchor = anchor_at(&cache, start.saturating_sub(step));
    }

    let report = metric.report();
    assert_eq!(report.frames, FRAMES);
    assert_eq!(
        report.mass_reflows, 0,
        "measuring an estimated block re-laid the viewport: {report:?}"
    );
    assert!(
        report.violations(&StabilityBudget::default()).is_empty(),
        "scrolling into estimated history was not stable: {report:?}"
    );
    assert!(
        cache.estimated_blocks().len() < WINDOWED_BLOCKS,
        "the walk measured nothing"
    );
    // Two settle calls per stop, each allowed one promotion round: more than
    // that is the window thrashing between measured and estimated.
    let promotions = cache.promotions();
    assert!(
        promotions <= FRAMES * 2,
        "{promotions} promotion rebuilds over {FRAMES} stops is thrash"
    );
}

#[test]
fn a_promoted_row_hit_tests_to_the_block_that_wrote_it() {
    // Mouse hit-testing maps a screen row to `block_index`. A row the viewport
    // reached is measured, so that mapping has to name the block whose text is
    // actually printed there — the fixture writes its own block index into
    // every user turn, so the row can be checked against it.
    let theme = Theme::dark();
    let blocks = synthetic_transcript(WINDOWED_BLOCKS);
    let mut cache = windowed_cache(&blocks, &theme, COLS);

    let middle = cache.plain().len() / 2;
    let anchor = anchor_at(&cache, middle).expect("a middle row belongs to a block");
    let start = cache.settle_window(&blocks, Some(anchor), ROWS);
    cache.ensure_rows(&blocks, start, start + ROWS);

    let mut checked = 0_usize;
    for row in start..(start + ROWS).min(cache.plain().len()) {
        let Some(text) = cache.plain().get(row) else {
            continue;
        };
        let Some(rest) = text.split_once("turn ").map(|(_, rest)| rest) else {
            continue;
        };
        let Some(index) = rest
            .split_once(':')
            .and_then(|(digits, _)| digits.parse::<usize>().ok())
        else {
            continue;
        };
        assert_eq!(
            cache.layout().block_index[row],
            Some(index),
            "row {row} prints turn {index} but hit-tests to another block"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the window carried no identifiable turn to hit-test"
    );
}

#[test]
fn copy_reads_measured_rows_even_where_the_viewport_never_looked() {
    // Copy reads the whole plain projection, so it may not see the blank rows
    // an estimate holds. Materializing has to reproduce, byte for byte, the
    // projection a cache that measured everything from the start would hold.
    let theme = Theme::dark();
    let blocks = synthetic_transcript(WINDOWED_BLOCKS);

    let mut windowed = windowed_cache(&blocks, &theme, COLS);
    let _ = windowed.settle_window(&blocks, None, ROWS);
    assert!(
        !windowed.estimated_blocks().is_empty(),
        "the fixture measured everything, so copy has nothing to promote"
    );
    windowed.materialize_all(&blocks);

    let mut whole = TranscriptRenderCache::default();
    whole.materialize_all(&blocks);
    let _ = whole.get(
        1,
        &blocks,
        &theme,
        COLS,
        TranscriptChange::Structural,
        crate::tui::blocks::RenderPrefs::default(),
    );

    assert!(windowed.estimated_blocks().is_empty());
    assert_eq!(
        windowed.plain(),
        whole.plain(),
        "a copy after a windowed layout does not match the measured transcript"
    );
    assert!(
        windowed.plain().iter().any(|row| row.contains("turn 4:")),
        "a block the viewport never reached is missing from the copy"
    );
}

#[test]
fn the_composed_frame_streams_the_tail_without_blink_or_reflow() {
    // The layout gates score the transcript projection. This one scores the
    // frame the operator actually sees: chrome, sticky bar, composer and
    // all, read back out of the painted `ratatui` buffer.
    let mut blocks = synthetic_transcript(BLOCKS);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let (mut app, mut terminal) = composed_app(blocks);
    let mut metric = AnchorStability::for_viewport(ROWS);

    for frame in 0..COMPOSED_FRAMES {
        stream_one_frame(&mut app, frame);
        let rows = composed_transcript_rows(&mut app, &mut terminal);
        metric.observe(&rows, ViewportMotion::FollowTail);
    }

    let report = metric.report();
    assert_eq!(report.frames, COMPOSED_FRAMES);
    assert_eq!(
        report.mass_reflows, 0,
        "the composed frame re-laid the transcript: {report:?}"
    );
    assert_eq!(report.blinks, 0, "the composed frame blinked: {report:?}");
}

/// A block the anchor names can be measured to nothing — an
/// empty first delta leaves an empty assistant block, which the markdown
/// projection draws as no rows at all. Resolving that to the tail threw the
/// operator out of the passage they were reading; the next block that still
/// draws rows is where their place is now.
#[test]
fn an_anchor_on_a_block_that_draws_nothing_lands_on_the_next_surviving_block() {
    let theme = Theme::dark();
    let mut blocks = synthetic_transcript(200);
    blocks.insert(100, ScrollBlock::Assistant(String::new()));
    let mut cache = windowed_cache(&blocks, &theme, COLS);
    cache.settle_window(&blocks, None, ROWS);
    assert!(
        crate::tui::blocks::block_span_for_test(cache.layout(), 100).is_none(),
        "the empty assistant block draws no rows"
    );

    let start = cache.settle_window(&blocks, Some((100, 0)), ROWS);
    let bottom = cache.layout().plain.len().saturating_sub(ROWS);
    assert_ne!(start, bottom, "the viewport must not fall to the tail");
    assert!(
        blocks_in_window(&cache, start, start + 1).contains(&101),
        "the anchor resolves to the next block that still draws rows"
    );
}

/// The `max_anchor_shift == 0` gate never exercised the
/// estimated→measured transition, and the only estimation test scored
/// `Deliberate`, which skips anchor scoring. This one is composed, above
/// `EXACT_LAYOUT_BLOCKS` so the layout is windowed, scrolled deep enough that
/// promotion has to correct block heights *above* the viewport, and scored
/// `perfect()`.
#[test]
fn a_pinned_composed_window_holds_across_estimated_to_measured_promotion() {
    let mut blocks = synthetic_transcript(1_400);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let (mut app, mut terminal) = composed_app(blocks);
    // Deep enough that the rows above the fold are still estimates when the
    // first frame paints, so the promotion the gate is about actually happens.
    app.pane.scroll_from_bottom = 4_000;
    let mut metric = AnchorStability::for_viewport(ROWS);

    let mut saw_estimates = false;
    for frame in 0..COMPOSED_FRAMES {
        stream_one_frame(&mut app, frame);
        let rows = composed_transcript_rows(&mut app, &mut terminal);
        saw_estimates |= app.transcript_estimated_blocks() > 0;
        metric.observe(&rows, ViewportMotion::Pinned);
    }

    assert!(
        saw_estimates,
        "the gate must run against a windowed layout, not a fully measured one"
    );
    let report = metric.report();
    let violations = report.violations(&StabilityBudget::perfect());
    assert!(
        violations.is_empty(),
        "promotion moved the operator's place: {violations:?} ({report:?})"
    );
    assert_eq!(report.max_anchor_shift, 0, "{report:?}");
}

#[test]
fn the_composed_frame_holds_a_scrolled_up_window_while_the_stream_appends() {
    let mut blocks = synthetic_transcript(BLOCKS);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let (mut app, mut terminal) = composed_app(blocks);
    // Scroll up far enough that the appending tail is well below the fold.
    app.pane.scroll_from_bottom = 200;
    let mut metric = AnchorStability::for_viewport(ROWS);

    for frame in 0..COMPOSED_FRAMES {
        stream_one_frame(&mut app, frame);
        let rows = composed_transcript_rows(&mut app, &mut terminal);
        metric.observe(&rows, ViewportMotion::Pinned);
    }

    let report = metric.report();
    let violations = report.violations(&StabilityBudget::strict());
    assert!(
        violations.is_empty(),
        "the composed frame drifted under a tail append: {violations:?} ({report:?})"
    );
}
