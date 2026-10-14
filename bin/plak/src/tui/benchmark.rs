//! Named layout/render benchmark for the Plak TUI.
//!
//! The workspace carries no benchmark harness dependency, so this is a plain
//! `harness = false` binary over [`std::time::Instant`]. It reports a wall-time
//! table on stdout and exits non-zero only if a case panics; budgets are
//! asserted by `tests/perf_gate.rs`, not here, because wall time on a shared
//! machine is evidence, not a gate.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p plak --bench tui_render --features test-support
//! ```
//!
//! Cases exercise the private layout implementation through the one
//! `test-support` benchmark entry point. Transcript layout is the dominant
//! frame cost; composed-frame stability remains covered by `perf_gate.rs`.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "standalone benchmark binary, not library surface"
)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

use super::markdown::{MarkdownTheme, render_markdown};
use super::{
    RenderPrefs, ScrollBlock, Theme, TranscriptChange, TranscriptRenderCache, format_tool_plain,
    layout_anchor, layout_blocks, wrap_plain,
};

/// Product reference viewport for the report.
const WIDE_COLS: usize = 120;
const WIDE_ROWS: usize = 40;

pub(crate) fn run() -> String {
    let mut report = Report::default();

    report.push(first_frame_empty());
    report.push(transcript_10k_at_rest());
    report.push(transcript_20k_at_rest());
    report.push(transcript_10k_at_rest_windowed());
    report.push(streamed_deltas_1k());
    report.push(streamed_deltas_1k_windowed());
    report.push(streamed_long_answer(
        "streaming · 2k-line code answer · windowed",
        "Here is the module:\n\n```rust\n",
        |line| format!("    let value_{line} = compute({line}).unwrap_or_default();\n"),
    ));
    report.push(streamed_long_answer(
        "streaming · 2k-line prose answer · windowed",
        "",
        |line| {
            if line % 6 == 5 {
                "\n".to_owned()
            } else {
                format!("Paragraph line {line} keeps explaining the change in **plain** words.\n")
            }
        },
    ));
    report.push(markdown_table_and_code());
    report.push(tool_subject_budgeting());
    report.push(resize_relayout());
    report.push(resize_relayout_10k());
    report.push(resize_relayout_10k_windowed());

    report.render()
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// First frame of an empty transcript at 120x40.
fn first_frame_empty() -> Case {
    let blocks: Vec<ScrollBlock> = Vec::new();
    time("first frame · empty transcript · 120x40", 2_000, || {
        let layout = layout_blocks(black_box(&blocks), WIDE_COLS);
        black_box(layout.plain.len())
    })
    .with_note(format!("viewport {WIDE_COLS}x{WIDE_ROWS}"))
}

/// Full layout of a 10k-block transcript at rest.
fn transcript_10k_at_rest() -> Case {
    let blocks = synthetic_transcript(10_000);
    let rows = layout_blocks(&blocks, WIDE_COLS).plain.len();
    time("transcript at rest · 10k blocks · full layout", 5, || {
        let layout = layout_blocks(black_box(&blocks), WIDE_COLS);
        black_box(layout.plain.len())
    })
    .with_note(format!("{rows} visual rows"))
}

/// The same full layout over a transcript twice the reference size.
///
/// The offline performance smoke reads this row: a transcript that large is
/// what a long session actually accumulates, and layout is superlinear in
/// nothing here, so a regression shows up as a changed ratio against the 10k
/// row rather than as an absolute number on a shared machine.
fn transcript_20k_at_rest() -> Case {
    let blocks = synthetic_transcript(20_000);
    let rows = layout_blocks(&blocks, WIDE_COLS).plain.len();
    time("transcript at rest · 20k blocks · full layout", 5, || {
        let layout = layout_blocks(black_box(&blocks), WIDE_COLS);
        black_box(layout.plain.len())
    })
    .with_note(format!("{rows} visual rows"))
}

/// 1000 streamed deltas coalesced at 8 per frame, laid out once per frame.
///
/// Reports both the wall time and the number of layout passes, because §11
/// budgets passes as tightly as it budgets milliseconds.
fn streamed_deltas_1k() -> Case {
    const DELTAS: usize = 1_000;
    const PER_FRAME: usize = 8;

    let mut blocks = synthetic_transcript(400);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let tail = blocks.len() - 1;

    let mut passes = 0_usize;
    let started = Instant::now();
    for delta in 0..DELTAS {
        if let Some(ScrollBlock::Assistant(text)) = blocks.get_mut(tail) {
            text.push_str(DELTA_CHUNK);
        }
        if delta % PER_FRAME == PER_FRAME - 1 {
            passes += 1;
            let layout = layout_blocks(black_box(&blocks), WIDE_COLS);
            black_box(layout.plain.len());
        }
    }
    let elapsed = started.elapsed();

    Case {
        name: "streaming · 1k deltas @ 8/frame".to_owned(),
        iterations: passes,
        total: elapsed,
        note: format!(
            "{passes} layout passes for {DELTAS} deltas ({:.3} ms/frame)",
            per_iter_ms(elapsed, passes)
        ),
    }
}

/// Markdown table plus fenced code block, the two most expensive block kinds.
fn markdown_table_and_code() -> Case {
    let theme = MarkdownTheme::default();
    let source = markdown_source();
    let lines = render_markdown(&source, 100, &theme).lines.len();
    time("markdown · table + code block · 100 cols", 2_000, || {
        let text = render_markdown(black_box(&source), 100, black_box(&theme));
        black_box(text.lines.len())
    })
    .with_note(format!("{lines} rendered rows"))
}

/// Tool-row subject budgeting at a narrow, a default, and a wide terminal.
fn tool_subject_budgeting() -> Case {
    const WIDTHS: [usize; 3] = [48, 80, 160];
    let subject = "crates/plakous/providers/openrouter/src/key_usage.rs:412 \
                   --with-a-long-flag --and=another/value/that/keeps/going";
    let summary = format_tool_plain("apply_patch", "running", Some(subject));

    time("tool row · subject budgeting · 3 widths", 20_000, || {
        let mut rows = 0;
        for width in WIDTHS {
            rows += wrap_plain(black_box(&summary), width).len();
        }
        black_box(rows)
    })
    .with_note(format!("widths {WIDTHS:?}"))
}

/// Relayout of a settled transcript from 200x50 down to 80x24.
fn resize_relayout() -> Case {
    let blocks = synthetic_transcript(2_000);
    // Warm the wide layout the way a settled session would have it.
    black_box(layout_blocks(&blocks, 200).plain.len());
    let narrow_rows = layout_blocks(&blocks, 80).plain.len();

    time("resize · 200x50 -> 80x24 · 2k blocks", 20, || {
        let layout = layout_blocks(black_box(&blocks), 80);
        black_box(layout.plain.len())
    })
    .with_note(format!("{narrow_rows} rows at 80 cols"))
}

/// Relayout of a full 10k-block transcript from 200x50 down to 80x24.
///
/// A resize invalidates every cached projection at once, so this is the
/// worst frame the product can be asked to paint; §11 exempts resize from
/// dirty redraw, and the working target for it is 50 ms.
fn resize_relayout_10k() -> Case {
    let blocks = synthetic_transcript(10_000);
    black_box(layout_blocks(&blocks, 200).plain.len());
    let narrow_rows = layout_blocks(&blocks, 80).plain.len();

    time("resize · 200x50 -> 80x24 · 10k blocks", 5, || {
        let layout = layout_blocks(black_box(&blocks), 80);
        black_box(layout.plain.len())
    })
    .with_note(format!("{narrow_rows} rows at 80 cols · 50 ms target"))
}

/// A cache holding `blocks` at `width`, settled on the tail.
///
/// This is the projection the product actually paints from: styled spans plus
/// the plain rows selection reads, measured exactly only where the viewport
/// looks.
fn settled_cache(blocks: &[ScrollBlock], width: usize) -> TranscriptRenderCache {
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        blocks,
        &Theme::dark(),
        width,
        TranscriptChange::Structural,
        RenderPrefs::default(),
    );
    let start = cache.settle_window(blocks, None, WIDE_ROWS);
    cache.ensure_rows(blocks, start, start + WIDE_ROWS);
    cache
}

/// The product's own at-rest frame: a windowed projection of 10k blocks with
/// the viewport on the tail.
fn transcript_10k_at_rest_windowed() -> Case {
    let blocks = synthetic_transcript(10_000);
    let theme = Theme::dark();
    let prefs = RenderPrefs::default();
    let mut cache = settled_cache(&blocks, WIDE_COLS);
    let rows = cache.plain().len();
    let mut revision = 1_u64;

    time("transcript at rest · 10k blocks · windowed", 200, || {
        revision += 1;
        let _ = cache.get(
            revision,
            black_box(&blocks),
            &theme,
            WIDE_COLS,
            TranscriptChange::Unchanged,
            prefs,
        );
        let start = cache.settle_window(&blocks, None, WIDE_ROWS);
        cache.ensure_rows(&blocks, start, start + WIDE_ROWS);
        black_box(start)
    })
    .with_note(format!("{rows} rows · viewport {WIDE_COLS}x{WIDE_ROWS}"))
}

/// The product's own resize frame: 200x50 down to 80x24 with the operator
/// scrolled into the middle of a 10k-block transcript.
fn resize_relayout_10k_windowed() -> Case {
    let blocks = synthetic_transcript(10_000);
    let theme = Theme::dark();
    let prefs = RenderPrefs::default();

    // Settle the wide layout the way a session at rest holds it, with the
    // operator's place held in block space half way up the transcript.
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(1, &blocks, &theme, 200, TranscriptChange::Structural, prefs);
    let middle = cache.plain().len() / 2;
    let anchor = layout_anchor(cache.layout(), middle);
    let start = cache.settle_window(&blocks, anchor, 50);
    let anchor = layout_anchor(cache.layout(), start);

    let mut revision = 1_u64;
    let mut rows = 0;
    let case = time(
        "resize · 200x50 -> 80x24 · 10k blocks · windowed",
        200,
        || {
            revision += 1;
            let _ = cache.get(
                revision,
                black_box(&blocks),
                &theme,
                80,
                TranscriptChange::Unchanged,
                prefs,
            );
            let start = cache.settle_window(&blocks, anchor, 24);
            cache.ensure_rows(&blocks, start, start + 24);
            rows = cache.plain().len();
            black_box(start)
        },
    );
    case.with_note(format!("{rows} rows at 80 cols · 50 ms target"))
}

/// 1000 streamed deltas through the product's incremental cache.
fn streamed_deltas_1k_windowed() -> Case {
    const DELTAS: usize = 1_000;
    const PER_FRAME: usize = 8;

    let mut blocks = synthetic_transcript(400);
    blocks.push(ScrollBlock::Assistant(String::new()));
    let tail = blocks.len() - 1;
    let theme = Theme::dark();
    let prefs = RenderPrefs::default();
    let mut cache = settled_cache(&blocks, WIDE_COLS);

    let mut passes = 0_usize;
    let mut revision = 1_u64;
    let started = Instant::now();
    for delta in 0..DELTAS {
        if let Some(ScrollBlock::Assistant(text)) = blocks.get_mut(tail) {
            text.push_str(DELTA_CHUNK);
        }
        if delta % PER_FRAME == PER_FRAME - 1 {
            passes += 1;
            revision += 1;
            let _ = cache.get(
                revision,
                black_box(&blocks),
                &theme,
                WIDE_COLS,
                TranscriptChange::TailAssistant,
                prefs,
            );
            let start = cache.settle_window(&blocks, None, WIDE_ROWS);
            cache.ensure_rows(&blocks, start, start + WIDE_ROWS);
            black_box(start);
        }
    }
    let elapsed = started.elapsed();

    Case {
        name: "streaming · 1k deltas @ 8/frame · windowed".to_owned(),
        iterations: passes,
        total: elapsed,
        note: format!(
            "{passes} composed passes for {DELTAS} deltas ({:.3} ms/frame)",
            per_iter_ms(elapsed, passes)
        ),
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A 2,000-line answer streamed one line per delta at eight deltas a frame,
/// through the product's incremental cache: the long-answer case whose tail
/// re-render must not grow with everything already streamed.
fn streamed_long_answer(name: &str, opening: &str, line: impl Fn(usize) -> String) -> Case {
    const LINES: usize = 2_000;
    const PER_FRAME: usize = 8;

    let mut blocks = synthetic_transcript(400);
    blocks.push(ScrollBlock::Assistant(opening.to_owned()));
    let tail = blocks.len() - 1;
    let theme = Theme::dark();
    let prefs = RenderPrefs::default();
    let mut cache = settled_cache(&blocks, WIDE_COLS);

    let mut passes = 0_usize;
    let mut revision = 1_u64;
    let mut last_quarter = Duration::ZERO;
    let started = Instant::now();
    for index in 0..LINES {
        if let Some(ScrollBlock::Assistant(text)) = blocks.get_mut(tail) {
            text.push_str(&line(index));
        }
        if index % PER_FRAME == PER_FRAME - 1 {
            passes += 1;
            revision += 1;
            let frame = Instant::now();
            let _ = cache.get(
                revision,
                black_box(&blocks),
                &theme,
                WIDE_COLS,
                TranscriptChange::TailAssistant,
                prefs,
            );
            let start = cache.settle_window(&blocks, None, WIDE_ROWS);
            cache.ensure_rows(&blocks, start, start + WIDE_ROWS);
            black_box(start);
            if index >= LINES * 3 / 4 {
                last_quarter += frame.elapsed();
            }
        }
    }
    let elapsed = started.elapsed();
    let last_passes = passes / 4;

    Case {
        name: name.to_owned(),
        iterations: passes,
        total: elapsed,
        note: format!(
            "{passes} frames ({:.3} ms/frame; last quarter {:.3} ms/frame)",
            per_iter_ms(elapsed, passes),
            per_iter_ms(last_quarter, last_passes)
        ),
    }
}

const DELTA_CHUNK: &str = "the assistant keeps writing prose one small chunk at a time. ";

/// A transcript shaped like a real session: user turns, assistant prose, tool
/// rows with subjects, and the occasional system note.
fn synthetic_transcript(blocks: usize) -> Vec<ScrollBlock> {
    let mut out = Vec::with_capacity(blocks);
    for index in 0..blocks {
        out.push(match index % 4 {
            0 => ScrollBlock::user(format!(
                "turn {index}: please look at the redraw cadence and report back"
            )),
            1 => ScrollBlock::Assistant(format!(
                "Reading the schedule for turn {index}. The frame timer is armed by demand, \
                 not by a fixed tick, so an idle session parks and consumes no CPU. \
                 Streaming coalesces deltas inside one frame before layout runs."
            )),
            2 => ScrollBlock::Tool {
                call_id: Some(format!("call-{index}")),
                name: "read_file".to_owned(),
                state: if index % 8 == 2 { "running" } else { "done" }.to_owned(),
                subject: Some(format!("crates/plak/tui/src/ui.rs:{index}")),
                detail: None,
                parent_call_id: None,
                cell_id: None,
                kind: plakous_host_protocol::views::ActivityKind::Tool,
                mode: super::DisplayMode::Collapsed,
                manual: false,
                group_open: false,
                wait: None,
                started_at_ms: None,
                finished_at_ms: None,
                view: crate::tui::blocks::ItemView::default(),
            },
            _ => ScrollBlock::System(format!("checkpoint {index} recorded")),
        });
    }
    out
}

fn markdown_source() -> String {
    let mut out = String::from(
        "## Cadence table\n\n\
         | state | interval | paints |\n\
         | --- | ---: | :---: |\n",
    );
    for row in 0..40 {
        let _ = writeln!(out, "| state-{row} | {} ms | {} |", row * 8, row % 2 == 0);
    }
    out.push_str("\n```rust\n");
    for line in 0..60 {
        let _ = writeln!(
            out,
            "    let value_{line} = compute(&state, {line}); // a comment that pushes the width"
        );
    }
    out.push_str(
        "```\n\nTrailing prose with `inline code` and a [link](https://example.invalid).\n",
    );
    out
}

// ---------------------------------------------------------------------------
// Timing plumbing
// ---------------------------------------------------------------------------

struct Case {
    name: String,
    iterations: usize,
    total: Duration,
    note: String,
}

impl Case {
    fn with_note(mut self, note: String) -> Self {
        self.note = note;
        self
    }

    fn per_iter_ms(&self) -> f64 {
        per_iter_ms(self.total, self.iterations)
    }
}

fn per_iter_ms(total: Duration, iterations: usize) -> f64 {
    if iterations == 0 {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "iteration counts are small integers"
    )]
    let iterations = iterations as f64;
    total.as_secs_f64() * 1_000.0 / iterations
}

fn time<F, T>(name: &str, iterations: usize, mut body: F) -> Case
where
    F: FnMut() -> T,
{
    // One untimed pass so the first allocation is not charged to the mean.
    black_box(body());
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(body());
    }
    Case {
        name: name.to_owned(),
        iterations,
        total: started.elapsed(),
        note: String::new(),
    }
}

#[derive(Default)]
struct Report {
    cases: Vec<Case>,
}

impl Report {
    fn push(&mut self, case: Case) {
        self.cases.push(case);
    }

    fn render(&self) -> String {
        let width = self
            .cases
            .iter()
            .map(|case| case.name.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "\n{:<width$}  {:>7}  {:>12}  note",
            "case", "iters", "ms/iter"
        );
        for case in &self.cases {
            let _ = writeln!(
                out,
                "{:<width$}  {:>7}  {:>12.4}  {}",
                case.name,
                case.iterations,
                case.per_iter_ms(),
                case.note
            );
        }
        out
    }
}
