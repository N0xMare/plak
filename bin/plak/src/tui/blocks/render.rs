use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::{LineLayout, ScrollBlock};
use crate::tui::markdown::RowCopy;
use crate::tui::presentation::ActivityKind;
use crate::tui::theme::Theme;

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let line_style = line.style;
    let items = line.spans.into_iter().flat_map(|span| {
        let style = span.style;
        span.content
            .graphemes(true)
            .map(move |grapheme| {
                if grapheme == "\n" {
                    super::CellItem::Break
                } else {
                    super::CellItem::Grapheme {
                        value: (grapheme.to_owned(), style),
                        cells: grapheme.width().max(1),
                        whitespace: grapheme.chars().all(char::is_whitespace),
                    }
                }
            })
            .collect::<Vec<_>>()
    });
    super::wrap_cell_items(items, width)
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (grapheme, style) in row {
                if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
                    last.content.to_mut().push_str(&grapheme);
                } else {
                    spans.push(Span::styled(grapheme, style));
                }
            }
            Line::from(spans).style(line_style)
        })
        .collect()
}

/// Wraps one line and re-lays the rail on every continuation row.
///
/// A long program-source line inside an expanded Compose detail
/// wrapped to column 0, outside the `│` rail, so the continuation read as
/// transcript rather than as cell content. The prefix is measured out of
/// the budget and re-drawn, so the rail is unbroken at 80 columns.
fn wrap_styled_line_railed(
    prefix: &str,
    continuation: &str,
    prefix_style: Style,
    body: Line<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    let gutter = prefix.width().max(continuation.width());
    let body_width = width.saturating_sub(gutter).max(1);
    let rows = wrap_styled_line(body, body_width);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let rail = if index == 0 { prefix } else { continuation };
            let mut spans = vec![Span::styled(rail.to_owned(), prefix_style)];
            spans.extend(row.spans);
            Line::from(spans)
        })
        .collect()
}

/// The selectable text of one styled row.
///
/// Band and panel backgrounds are painted by padding the row with spaces;
/// the plain projection copy and selection read must not carry that paint.
fn plain_row(line: &Line<'static>) -> String {
    line.to_string().trim_end().to_owned()
}

/// Selectable text of one styled row of `block`.
///
/// A prompt or steer band pads out to its send time and, for a steer, its
/// tag. Those are paint. A copy of the row is the prompt text, gutter
/// included so a partial selection still lines up with the painted cells.
fn selectable_row(block: &ScrollBlock, line: &Line<'static>) -> String {
    if matches!(block, ScrollBlock::User { .. } | ScrollBlock::Steer { .. }) {
        prompt_selectable(line)
    } else {
        plain_row(line)
    }
}

/// Gutter and prompt text, stopping before the unstyled padding that the
/// send time and steer tag sit on. The prompt's own spaces keep the user
/// style, so they stay; a row with no chrome is the whole line.
fn prompt_selectable(line: &Line<'static>) -> String {
    let mut text = String::new();
    for span in &line.spans {
        if span.style == Style::default() {
            break;
        }
        text.push_str(span.content.as_ref());
    }
    text
}

fn wrap_styled_lines(
    lines: impl IntoIterator<Item = Line<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| wrap_styled_line(line, width))
        .collect()
}

/// Width-sensitive, fully styled transcript projection.
#[derive(Clone, Debug, Default)]
pub struct TranscriptRender {
    /// Styled visual lines, ready for viewport slicing.
    pub lines: Vec<Line<'static>>,
    /// Plain visual lines and their source blocks.
    pub layout: LineLayout,
}

/// Mutation known by the presentation owner since the preceding draw.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TranscriptChange {
    /// No block changed; the cached projection stands as it is.
    #[default]
    Unchanged,
    /// A stream appended to the trailing assistant block and nothing else.
    TailAssistant,
    /// Blocks were added, removed, or reordered: every row index moves.
    Structural,
    /// One block's content changed in place, at this index.
    Block(usize),
}

impl TranscriptChange {
    /// The weakest change covering both, so a frame that saw two of them
    /// re-projects only what neither could promise on its own.
    #[must_use]
    pub const fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Structural, _) | (_, Self::Structural) => Self::Structural,
            (Self::Block(a), Self::Block(b)) if a != b => Self::Structural,
            (Self::Block(_), Self::TailAssistant) | (Self::TailAssistant, Self::Block(_)) => {
                Self::Structural
            }
            (Self::Block(i), _) | (_, Self::Block(i)) => Self::Block(i),
            (Self::TailAssistant, _) | (_, Self::TailAssistant) => Self::TailAssistant,
            _ => Self::Unchanged,
        }
    }
}

/// Stated ceiling for the transcript projection cache.
///
/// The cache holds two copies of every projected row — the styled spans a
/// frame paints and the plain string selection and copy read — and both
/// are counted against this number. Crossing it evicts whole block
/// projections, furthest from the visible window first; the rows the
/// operator can see and the trailing block a stream is appending to are
/// never evicted. Evicted rows keep their slot, so row geometry (and with
/// it every scroll anchor) is unchanged by eviction; the content is
/// re-projected on demand when the viewport reaches it.
///
/// This bounds what the cache *retains*. A full rebuild still projects the
/// whole transcript once before the first sweep runs, because
/// `render_transcript_windowed` is a single pass.
pub const TRANSCRIPT_RENDER_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// Byte mark a sweep evicts down to once the ceiling is crossed.
///
/// The gap below the ceiling is hysteresis: sweeping to exactly the
/// ceiling would re-sweep on the very next appended row.
const TRANSCRIPT_RENDER_CACHE_LOW_WATER: usize = TRANSCRIPT_RENDER_CACHE_BYTES / 2;

/// Budget slice above which one block's own projection is *oversized*.
///
/// Such a block goes on a side-list and is evicted ahead of every ordinary
/// block, so a single huge paste cannot repeatedly push the ordinary
/// neighbourhood of the viewport out and thrash the cache.
const OVERSIZED_BLOCK_PROJECTION_BYTES: usize = TRANSCRIPT_RENDER_CACHE_BYTES / 8;

/// Bytes one projected row retains: styled spans, the plain copy, and an
/// unclipped code source when the row carries one.
fn row_bytes(line: &Line<'static>, plain: &str, copy: &RowCopy) -> usize {
    plain.len()
        + line
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>()
        + copy.code.as_ref().map_or(0, String::len)
}

/// Contiguous `(block, start, end)` row ranges of a projection, in row
/// order. Blank separator rows carry no block and are skipped.
fn block_ranges(layout: &LineLayout) -> Vec<(usize, usize, usize)> {
    let mut out: Vec<(usize, usize, usize)> = Vec::new();
    for (row, index) in layout.block_index.iter().enumerate() {
        let Some(block) = *index else { continue };
        match out.last_mut() {
            Some(last) if last.0 == block && last.2 == row => last.2 = row + 1,
            _ => out.push((block, row, row + 1)),
        }
    }
    out
}

/// Blocks below which the whole transcript is measured in one exact pass.
///
/// Measuring this many costs well under a millisecond; estimating them
/// would only buy a promotion round on the operator's first scroll.
const EXACT_LAYOUT_BLOCKS: usize = 512;

/// Trailing blocks always measured exactly.
///
/// The streaming tail is where every delta lands and where the viewport
/// sits by default, so it is never an estimate: the bottom of the
/// transcript cannot move under a promotion.
const EXACT_TAIL_BLOCKS: usize = 64;

/// Blocks kept exactly measured at once.
///
/// The viewport and its overscan are a few dozen blocks; the rest of this
/// budget is scrollback the operator already visited, kept exact so
/// scrolling back over it costs nothing. Past the budget the blocks
/// furthest from the view fall back to estimates, which bounds what a
/// resize has to re-measure however far the operator has scrolled.
const MAX_EXACT_BLOCKS: usize = 1_024;

/// Promotion rounds [`TranscriptRenderCache::settle_window`] runs.
///
/// One round measures the estimated blocks the window currently reads.
/// Correcting their heights can pull a further block into the window, so
/// it iterates; two rounds settle every realistic case and the bound only
/// stops a pathological one from looping.
const SETTLE_ROUNDS: usize = 4;

/// System rows §5.1 hides outright, so they contribute no visual rows.
///
/// Stated once because both the exact projection and the row-count
/// estimate have to agree on whether a block is drawn at all: a block the
/// estimate gives rows and the measurement gives none would shift every
/// row below it the first time the viewport reached it.
fn system_row_is_hidden(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.starts_with("status: session=")
        || trimmed.starts_with("keybinds:")
}

/// Rows `text` plausibly wraps to at `width`, from byte length alone.
///
/// No wrapping and no allocation: this is the whole point of the estimate.
/// Byte length overcounts multi-byte text, which errs toward a taller
/// transcript — the harmless direction, because the operator can still
/// reach every row and the measurement that follows only shortens it.
fn estimate_wrapped_rows(text: &str, width: usize) -> usize {
    let width = width.max(1);
    text.split('\n')
        .map(|line| line.len().div_ceil(width).max(1))
        .fold(0usize, usize::saturating_add)
        .max(1)
}

/// Test hook for the estimator's only load-bearing property.
///
/// The estimate must agree with [`block_lines`] on whether a block draws
/// any rows at all; everything else about it is deliberately approximate.
#[cfg(test)]
pub(crate) fn estimate_rows_for_test(
    block: &ScrollBlock,
    content_width: usize,
    prefs: super::RenderPrefs,
) -> usize {
    estimate_block_rows(
        block,
        &super::RowPlan::default(),
        content_width,
        prefs,
        std::slice::from_ref(block),
    )
}

/// Cheap row-count estimate for one block's styled projection.
///
/// Agrees with [`block_lines`] on the only thing geometry correctness
/// depends on — whether the block draws any rows at all — and is
/// deliberately approximate about how many.
pub(super) fn estimate_block_rows(
    block: &ScrollBlock,
    plan: &super::RowPlan,
    content_width: usize,
    prefs: super::RenderPrefs,
    blocks: &[ScrollBlock],
) -> usize {
    if plan.hidden {
        return 0;
    }
    let body_width = content_width.saturating_sub(2);
    match block {
        // The markdown projection pops trailing empty lines, so an empty
        // or whitespace assistant block — an empty first delta produces one
        // — draws nothing. Estimating a row for it broke the one property
        // this estimate has to hold: a block the anchor lands on vanished
        // when it was measured, and the viewport fell to the tail.
        ScrollBlock::Assistant(text) if text.trim().is_empty() => 0,
        ScrollBlock::User { text, .. }
        | ScrollBlock::Steer { text, .. }
        | ScrollBlock::Assistant(text)
        | ScrollBlock::Error(text)
        | ScrollBlock::Header(text) => estimate_wrapped_rows(text, body_width),
        ScrollBlock::Milestone { milestone, open } => {
            let detail = if *open {
                super::milestone_detail(milestone)
                    .iter()
                    .map(|row| estimate_wrapped_rows(row, content_width.saturating_sub(6)))
                    .fold(0usize, usize::saturating_add)
            } else {
                0
            };
            1usize
                .saturating_add(detail)
                .saturating_add(usize::from(super::tool_block_closes_with_blank(plan)))
        }
        ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => 0,
        ScrollBlock::Discarded { text, .. } => {
            estimate_wrapped_rows(text, body_width).saturating_add(1)
        }
        ScrollBlock::Mail { .. } => super::mail_lines(block, plan, content_width).len(),
        ScrollBlock::System(text) => {
            if system_row_is_hidden(text) {
                0
            } else {
                estimate_wrapped_rows(text, body_width)
            }
        }
        ScrollBlock::Item { .. } => 1 + usize::from(super::tool_block_closes_with_blank(plan)),
        // The reasoning, approval, and tool projections all close on one
        // blank row, which the estimate has to carry too.
        ScrollBlock::Thinking {
            text, sealed, mode, ..
        } => {
            if !prefs.show_thinking
                || (*sealed && text.trim().is_empty() && plan.activity_summary.is_none())
            {
                return 0;
            }
            let detail = if mode.shows_detail() && !plan.group_folded {
                let rail = super::thinking_detail_rail(plan).width();
                estimate_wrapped_rows(text, content_width.saturating_sub(rail))
            } else {
                0
            };
            detail
                .saturating_add(
                    1 + usize::from(plan.activity_summary.is_some() && !plan.group_folded),
                )
                .saturating_add(usize::from(super::tool_block_closes_with_blank(plan)))
        }
        ScrollBlock::Approval {
            subject,
            prompt,
            resolution,
            ..
        } => [subject, prompt, resolution]
            .into_iter()
            .flatten()
            .map(|text| estimate_wrapped_rows(text, content_width))
            .fold(2usize, usize::saturating_add),
        ScrollBlock::Tool {
            kind,
            name,
            state,
            subject,
            detail,
            mode,
            ..
        } => {
            let source = if plan.group_folded {
                0
            } else {
                plan.cell_tails
                    .iter()
                    .map(|tail| estimate_cell_footer(tail, blocks, content_width))
                    .sum::<usize>()
            };
            let header = usize::from(plan.activity_summary.is_some());
            let item_detail = if plan.group_folded
                || matches!(kind, ActivityKind::Compact | ActivityKind::Compose)
            {
                0
            } else {
                usize::from(plan.members_open).saturating_add(match mode {
                    super::DisplayMode::Expanded => detail
                        .as_deref()
                        .map_or(0, |text| text.lines().count().min(32))
                        .saturating_add(2),
                    super::DisplayMode::Collapsed => 0,
                })
            };
            let action = if plan.group_folded {
                0
            } else if *kind == ActivityKind::Compact {
                let action =
                    crate::tui::activity::action(name, state, *kind, subject.as_deref(), plan);
                estimate_wrapped_rows(
                    action.trim_start_matches(super::row_indent(plan)),
                    content_width.saturating_sub(4),
                )
            } else {
                1
            };
            header
                .saturating_add(action)
                .saturating_add(item_detail)
                .saturating_add(source)
                .saturating_add(usize::from(super::tool_block_closes_with_blank(plan)))
        }
    }
}

fn estimate_cell_footer(tail: &super::CellTail, blocks: &[ScrollBlock], width: usize) -> usize {
    let more = usize::from(tail.hidden_calls > 0);
    let Some(ScrollBlock::Tool {
        name,
        kind,
        state,
        subject,
        detail,
        mode: super::DisplayMode::Expanded,
        view,
        ..
    }) = blocks.get(tail.owner)
    else {
        return more;
    };
    let source = detail
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map_or(0, |s| s.lines().count() + 2);
    let output = if view.output_open {
        let facts = ItemFacts {
            name,
            kind: *kind,
            state,
            subject: subject.as_deref(),
            detail: detail.as_deref(),
            view,
        };
        let text = detail_text(&facts);
        let output = text.output.trim_end_matches('\n');
        if output.trim().is_empty() {
            usize::from(text.preview)
        } else {
            let count = output.split('\n').count();
            let head = super::TOOL_DETAIL_HEAD_LINES;
            let tail_lines = super::TOOL_DETAIL_TAIL_LINES;
            let windowed = !view.full_output && count > head + tail_lines + 1;
            let body_width = width.saturating_sub(super::detail_rail(tail.rail).width() + 2);
            let rows = output
                .split('\n')
                .enumerate()
                .filter(|(index, _)| !windowed || *index < head || *index >= count - tail_lines)
                .map(|(_, line)| estimate_wrapped_rows(line, body_width))
                .sum::<usize>();
            usize::from(text.preview)
                + 1
                + rows
                + usize::from(windowed)
                + usize::from(text.output_truncated)
        }
    } else {
        0
    };
    more + source + 1 + output
}

/// Width/theme cache with a safe trailing-assistant splice path.
///
/// Bounded by [`TRANSCRIPT_RENDER_CACHE_BYTES`]; see that constant for the
/// eviction rule.
#[derive(Clone, Debug, Default)]
pub struct TranscriptRenderCache {
    revision: Option<u64>,
    plans_revision: Option<u64>,
    plans: Vec<super::RowPlan>,
    width: usize,
    theme: Option<Theme>,
    prefs: super::RenderPrefs,
    block_count: usize,
    tail_row_start: Option<usize>,
    render: TranscriptRender,
    /// Bytes the projection currently retains (styled plus plain).
    projected_bytes: usize,
    /// Blocks whose rows are blanked and must be re-projected on demand.
    evicted: std::collections::BTreeSet<usize>,
    /// Side-list of blocks larger than [`OVERSIZED_BLOCK_PROJECTION_BYTES`].
    oversized: std::collections::BTreeSet<usize>,
    /// Row window the last paint showed; never evicted.
    visible_rows: Option<(usize, usize)>,
    /// Blocks promoted to an exact measurement, newest window first.
    ///
    /// Bounded by [`MAX_EXACT_BLOCKS`]; the trailing tail band and a short
    /// transcript are exact without being listed here.
    exact_blocks: std::collections::BTreeSet<usize>,
    /// Every block is measured exactly because selection or copy asked.
    exact_all: bool,
    /// The streaming assistant tail's settled markdown prefix.
    tail_markdown: crate::tui::markdown::StreamingMarkdown,
    /// Where the tail's settled-prefix rows sit, while they are current.
    tail_settled: Option<SettledTail>,
    #[cfg(test)]
    rebuilds: usize,
    #[cfg(test)]
    full_rows: usize,
    #[cfg(test)]
    tail_rows: usize,
    #[cfg(test)]
    evictions: usize,
    #[cfg(test)]
    reprojections: usize,
    #[cfg(test)]
    promotions: usize,
}

/// The rows a streaming assistant tail's settled prefix occupies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SettledTail {
    block: usize,
    start: usize,
    /// Settled markdown rows these projected rows came from.
    lines: usize,
    rows: usize,
    bytes: usize,
}

impl TranscriptRenderCache {
    /// Complete cached plain projection.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn plain(&self) -> &[String] {
        &self.render.layout.plain
    }

    /// The cached projection as it stands, without touching the cache.
    #[must_use]
    pub const fn render(&self) -> &TranscriptRender {
        &self.render
    }

    /// Cached visual layout used to pin scroll anchors across rebuilds.
    #[must_use]
    pub fn layout(&self) -> &LineLayout {
        &self.render.layout
    }

    pub(crate) fn row_plans(&self) -> &[super::RowPlan] {
        &self.plans
    }

    /// Rows whose identity is proven unchanged by the pending mutation.
    #[must_use]
    pub fn stable_prefix_rows(&self, change: TranscriptChange) -> Option<usize> {
        match change {
            TranscriptChange::Unchanged => Some(self.render.lines.len()),
            TranscriptChange::TailAssistant => Some(match self.tail_row_start {
                Some(start) => start,
                None => self.render.lines.len(),
            }),
            TranscriptChange::Block(idx) => self
                .render
                .layout
                .block_index
                .iter()
                .position(|row| *row == Some(idx)),
            TranscriptChange::Structural => None,
        }
    }

    /// Return the current projection, rebuilding only when content or width changed.
    pub fn get(
        &mut self,
        revision: u64,
        blocks: &[ScrollBlock],
        theme: &Theme,
        content_width: usize,
        change: TranscriptChange,
        prefs: super::RenderPrefs,
    ) -> &TranscriptRender {
        if self.plans_revision != Some(revision) || self.prefs != prefs {
            self.plans = super::plan_rows(blocks, prefs);
            self.plans_revision = Some(revision);
        }
        if self.revision == Some(revision)
            && self.width == content_width
            && self.theme.as_ref() == Some(theme)
            && self.prefs == prefs
        {
            return &self.render;
        }
        let width_theme_match = self.width == content_width
            && self.theme.as_ref() == Some(theme)
            && self.prefs == prefs;
        let spliced = width_theme_match
            && if let TranscriptChange::Block(idx) = change {
                idx < blocks.len()
                    && !self
                        .plans
                        .iter()
                        .any(|plan| plan.cell_tails.iter().any(|tail| tail.owner == idx))
                    && self.splice_block(blocks, theme, content_width, idx, prefs)
            } else {
                false
            };
        if !spliced {
            let can_splice_tail = change == TranscriptChange::TailAssistant
                && width_theme_match
                && matches!(blocks.last(), Some(ScrollBlock::Assistant(_)))
                && (self.block_count == blocks.len()
                    || (self.block_count.saturating_add(1) == blocks.len()
                        && !matches!(
                            blocks.get(blocks.len().saturating_sub(2)),
                            Some(ScrollBlock::Tool { .. })
                        )))
                && (self.block_count != blocks.len() || self.tail_row_start.is_some());
            if can_splice_tail {
                self.splice_tail(blocks, theme, content_width, prefs);
            } else {
                let exact = self.exact_set(blocks.len());
                self.render = render_transcript_windowed(
                    blocks,
                    theme,
                    content_width,
                    prefs,
                    &self.plans,
                    exact.as_ref(),
                );
                self.tail_row_start = trailing_assistant_row_start(&self.render, blocks);
                self.tail_settled = None;
                self.recount_projection();
                #[cfg(test)]
                {
                    self.rebuilds += 1;
                    self.full_rows += self.render.lines.len();
                }
            }
        }
        self.revision = Some(revision);
        self.width = content_width;
        self.theme = Some(theme.clone());
        self.prefs = prefs;
        self.block_count = blocks.len();
        self.enforce_byte_ceiling();
        &self.render
    }

    /// Measure exactly everything the viewport reads, and report the row
    /// the anchor now sits on.
    ///
    /// The anchor is `(block, sub-row)`, never a row number: promoting a
    /// block corrects its height and with it every row index below it, and
    /// only a block-space anchor survives that — which is the same reason
    /// it survives the re-wrap a resize forces. Promotion rebuilds, and a
    /// corrected height can pull a further block into the window, so this
    /// iterates to a fixed point.
    ///
    /// `rows` is the viewport height; a band of the same size above and
    /// below is promoted with it, so a scroll of up to one screen in
    /// either direction lands on rows that were already measured.
    pub fn settle_window(
        &mut self,
        blocks: &[ScrollBlock],
        anchor: Option<(usize, usize)>,
        rows: usize,
    ) -> usize {
        let rows = rows.max(1);
        for _ in 0..SETTLE_ROUNDS {
            let start = self.anchor_row(anchor, rows);
            if self.render.layout.estimated.is_empty() {
                return start;
            }
            let low = start.saturating_sub(rows);
            let high = start.saturating_add(rows.saturating_mul(2));
            let wanted: Vec<usize> = block_ranges(&self.render.layout)
                .into_iter()
                .filter(|(block, first, last)| {
                    *first < high && *last > low && self.render.layout.estimated.contains(block)
                })
                .map(|(block, _, _)| block)
                .collect();
            if wanted.is_empty() {
                return start;
            }
            self.exact_blocks.extend(wanted);
            self.trim_exact_blocks(blocks.len(), anchor);
            self.rebuild(blocks);
        }
        self.anchor_row(anchor, rows)
    }

    /// Rebases every block-indexed set after retention evicted head blocks.
    ///
    /// Retention drains from the front, so a block that survives moves down
    /// by a fixed count and a block below `dropped_below` is gone. Without
    /// this the measured set, the eviction list and the oversized side-list
    /// went on naming indices that now belong to different blocks, so the
    /// cache went stale at its edges exactly at the retention cap
    /// (render budget).
    pub fn shift_blocks(&mut self, dropped_below: usize, shift: usize) {
        if dropped_below == 0 && shift == 0 {
            return;
        }
        let rebase = |set: &std::collections::BTreeSet<usize>| {
            set.iter()
                .filter(|block| **block >= dropped_below)
                .filter_map(|block| block.checked_sub(shift))
                .collect()
        };
        self.exact_blocks = rebase(&self.exact_blocks);
        self.evicted = rebase(&self.evicted);
        self.oversized = rebase(&self.oversized);
        self.tail_settled = None;
        // Row numbers moved with the blocks; the next paint recomputes the
        // window it must not evict.
        self.visible_rows = None;
    }

    /// Demand — or release — an exactly measured projection.
    ///
    /// Selection carries content rows from one frame to the next and copy
    /// reads them all, so while either is live nothing may be an estimate.
    /// Set this before asking for the projection, so the rebuild it may
    /// trigger measures everything in the one pass. Releasing it leaves
    /// the projection in hand alone; the next rebuild is windowed again.
    pub const fn require_exact(&mut self, exact: bool) {
        self.exact_all = exact;
    }

    /// Row the anchor occupies in the projection as it now stands.
    ///
    /// Falls back to the tail when there is no anchor or the anchored
    /// block has gone, which is what following the stream wants anyway.
    fn anchor_row(&self, anchor: Option<(usize, usize)>, rows: usize) -> usize {
        let bottom = self.render.lines.len().saturating_sub(rows);
        let Some((block, sub_row)) = anchor else {
            return bottom;
        };
        let Some((start, end)) = super::block_span(&self.render.layout, block) else {
            // The anchored block draws no rows any more — it was measured
            // to nothing, or retention dropped it. Falling to the tail
            // threw the operator out of the passage they were reading; the
            // next block that still draws rows is where that place is now.
            return block_ranges(&self.render.layout)
                .into_iter()
                .find(|(candidate, _, _)| *candidate > block)
                .map_or(bottom, |(_, first, _)| first.min(bottom));
        };
        start
            .saturating_add(sub_row.min(end.saturating_sub(start).saturating_sub(1)))
            .min(bottom)
    }

    /// The blocks a projection must measure exactly, or `None` when every
    /// block must be.
    fn exact_set(&self, len: usize) -> Option<std::collections::BTreeSet<usize>> {
        if self.exact_all || len <= EXACT_LAYOUT_BLOCKS {
            return None;
        }
        let mut exact = self.exact_blocks.clone();
        exact.extend(len.saturating_sub(EXACT_TAIL_BLOCKS)..len);
        Some(exact)
    }

    /// Drop the exact measurements furthest from the view once the budget
    /// is spent.
    ///
    /// A demoted block reverts to an estimate, which moves the rows below
    /// it — never the anchored row, which is resolved in block space after
    /// every rebuild.
    fn trim_exact_blocks(&mut self, len: usize, anchor: Option<(usize, usize)>) {
        if self.exact_blocks.len() <= MAX_EXACT_BLOCKS {
            return;
        }
        let centre = anchor.map_or(len.saturating_sub(1), |(block, _)| block);
        let mut kept: Vec<usize> = self.exact_blocks.iter().copied().collect();
        kept.sort_by_key(|block| block.abs_diff(centre));
        kept.truncate(MAX_EXACT_BLOCKS);
        self.exact_blocks = kept.into_iter().collect();
    }

    /// Re-project at the width, theme and preferences already in hand.
    fn rebuild(&mut self, blocks: &[ScrollBlock]) {
        let Some(theme) = self.theme.clone() else {
            return;
        };
        let exact = self.exact_set(blocks.len());
        self.render = render_transcript_windowed(
            blocks,
            &theme,
            self.width,
            self.prefs,
            &self.plans,
            exact.as_ref(),
        );
        self.tail_row_start = trailing_assistant_row_start(&self.render, blocks);
        self.tail_settled = None;
        self.block_count = blocks.len();
        self.recount_projection();
        #[cfg(test)]
        {
            self.promotions += 1;
        }
    }

    /// Re-project every evicted block that intersects rows `start..end`.
    ///
    /// The paint path calls this once the viewport rows are known, so the
    /// operator never sees a row eviction blanked. Re-projection uses the
    /// width, theme, and preferences the cache was last filled at, so it
    /// reproduces exactly the rows that were evicted.
    pub fn ensure_rows(&mut self, blocks: &[ScrollBlock], start: usize, end: usize) {
        self.visible_rows = Some((start, end));
        if !self.evicted.is_empty() {
            let wanted: Vec<usize> = block_ranges(&self.render.layout)
                .into_iter()
                .filter(|(block, first, last)| {
                    *first < end && *last > start && self.evicted.contains(block)
                })
                .map(|(block, _, _)| block)
                .collect();
            self.reproject(blocks, &wanted);
        }
        self.enforce_byte_ceiling();
    }

    /// Measure and project every block.
    ///
    /// Transcript copy reads the complete plain projection and selection
    /// carries row identities across frames, so neither may run against
    /// estimated geometry or the blanks eviction leaves behind. Windowed
    /// rebuilds resume at the next [`Self::relax_exact`]; the byte ceiling
    /// is re-applied by the next paint.
    pub fn materialize_all(&mut self, blocks: &[ScrollBlock]) {
        self.exact_all = true;
        if !self.render.layout.estimated.is_empty() {
            self.rebuild(blocks);
            return;
        }
        if self.evicted.is_empty() {
            return;
        }
        let wanted: Vec<usize> = self.evicted.iter().copied().collect();
        self.reproject(blocks, &wanted);
    }

    /// Bytes the projection retains right now (styled plus plain).
    #[cfg(test)]
    #[must_use]
    pub const fn projected_bytes(&self) -> usize {
        self.projected_bytes
    }

    /// Blocks currently evicted, in block order.
    #[cfg(test)]
    #[must_use]
    pub fn evicted_blocks(&self) -> Vec<usize> {
        self.evicted.iter().copied().collect()
    }

    /// Blocks on the oversized side-list, in block order.
    #[cfg(test)]
    #[must_use]
    pub fn oversized_blocks(&self) -> Vec<usize> {
        self.oversized.iter().copied().collect()
    }

    /// `(evictions, re-projections)` performed so far.
    #[cfg(test)]
    #[must_use]
    pub const fn cache_traffic(&self) -> (usize, usize) {
        (self.evictions, self.reprojections)
    }

    /// Blocks still carrying an estimated row count, in block order.
    #[cfg(test)]
    #[must_use]
    pub fn estimated_blocks(&self) -> Vec<usize> {
        self.render.layout.estimated.iter().copied().collect()
    }

    /// Promotion rebuilds performed so far.
    #[cfg(test)]
    #[must_use]
    pub const fn promotions(&self) -> usize {
        self.promotions
    }

    /// Rebuild the byte account and the oversized side-list from scratch.
    fn recount_projection(&mut self) {
        self.evicted.clear();
        self.oversized.clear();
        self.projected_bytes = self
            .render
            .lines
            .iter()
            .zip(&self.render.layout.plain)
            .zip(&self.render.layout.copy)
            .map(|((line, plain), copy)| row_bytes(line, plain, copy))
            .sum();
        for (block, start, end) in block_ranges(&self.render.layout) {
            let bytes = self.range_bytes(start, end);
            if bytes > OVERSIZED_BLOCK_PROJECTION_BYTES {
                self.oversized.insert(block);
            }
        }
    }

    fn range_bytes(&self, start: usize, end: usize) -> usize {
        (start..end)
            .filter_map(|row| {
                let line = self.render.lines.get(row)?;
                let plain = self.render.layout.plain.get(row)?;
                let copy = self.render.layout.copy.get(row)?;
                Some(row_bytes(line, plain, copy))
            })
            .sum()
    }

    /// Account one block's projection after its rows changed in place.
    fn note_block_bytes(&mut self, block: usize, start: usize, end: usize) {
        let bytes = self.range_bytes(start, end);
        if bytes > OVERSIZED_BLOCK_PROJECTION_BYTES {
            self.oversized.insert(block);
        } else {
            self.oversized.remove(&block);
        }
    }

    /// Evict projections until the account is back under the low-water
    /// mark, oversized blocks first and then furthest from the visible
    /// window. Never touches the visible rows or the trailing block.
    fn enforce_byte_ceiling(&mut self) {
        if self.projected_bytes <= TRANSCRIPT_RENDER_CACHE_BYTES {
            return;
        }
        let total = self.render.lines.len();
        // With no painted window yet the product is following the tail, so
        // the last row is what must survive.
        let (visible_start, visible_end) = self
            .visible_rows
            .unwrap_or((total.saturating_sub(1), total));
        let tail_block = self.block_count.saturating_sub(1);
        let mut candidates: Vec<(bool, usize, usize, usize, usize)> =
            block_ranges(&self.render.layout)
                .into_iter()
                .filter(|(block, start, end)| {
                    // An estimated block holds blank rows already: there
                    // are no bytes in it to reclaim, and evicting it would
                    // lose the fact that it still has to be measured.
                    let empty = self.evicted.contains(block)
                        || self.render.layout.estimated.contains(block);
                    let protected =
                        *block == tail_block || (*start < visible_end && *end > visible_start);
                    !(empty || protected)
                })
                .map(|(block, start, end)| {
                    let distance = if end <= visible_start {
                        visible_start - end
                    } else {
                        start.saturating_sub(visible_end)
                    };
                    (self.oversized.contains(&block), distance, block, start, end)
                })
                .collect();
        candidates.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        for (_, _, block, start, end) in candidates {
            if self.projected_bytes <= TRANSCRIPT_RENDER_CACHE_LOW_WATER {
                break;
            }
            for row in start..end {
                let Some(line) = self.render.lines.get_mut(row) else {
                    continue;
                };
                let Some(plain) = self.render.layout.plain.get_mut(row) else {
                    continue;
                };
                let Some(copy) = self.render.layout.copy.get_mut(row) else {
                    continue;
                };
                self.projected_bytes = self
                    .projected_bytes
                    .saturating_sub(row_bytes(line, plain, copy));
                // Replace rather than clear: clearing would keep the
                // allocation and evict nothing the allocator can reuse.
                *line = Line::default();
                *plain = String::new();
                *copy = RowCopy::default();
            }
            self.evicted.insert(block);
            #[cfg(test)]
            {
                self.evictions += 1;
            }
        }
    }

    /// Re-render `wanted` evicted blocks back into their existing rows.
    ///
    /// Rendering is deterministic at a fixed width, theme, and preference
    /// set, so a re-projected block occupies exactly the rows it left
    /// behind. A row-count mismatch would move every anchor below it, so
    /// it is answered by dropping the cache instead of splicing.
    fn reproject(&mut self, blocks: &[ScrollBlock], wanted: &[usize]) {
        let Some(theme) = self.theme.clone() else {
            return;
        };
        if wanted.is_empty() {
            return;
        }
        let requested: std::collections::BTreeSet<usize> = wanted.iter().copied().collect();
        let ranges: Vec<(usize, usize, usize)> = block_ranges(&self.render.layout)
            .into_iter()
            .filter(|(block, _, _)| requested.contains(block))
            .collect();
        // Every requested block leaves the evicted set: either it is
        // projected below, or it no longer has rows to project into.
        self.evicted.retain(|block| !requested.contains(block));
        for (block, start, end) in ranges {
            let (Some(source), Some(plan)) = (blocks.get(block), self.plans.get(block)) else {
                continue;
            };
            let (styled, copy) = block_rows(source, plan, &theme, self.width, self.prefs, blocks);
            if styled.len() != end - start || copy.len() != styled.len() {
                // Dropping the cached revision forces the next `get` to
                // rebuild rather than paint rows that no longer line up.
                self.revision = None;
                continue;
            }
            for (offset, (line, copy)) in styled.into_iter().zip(copy).enumerate() {
                let row = start + offset;
                let text = selectable_row(source, &line);
                self.projected_bytes = self
                    .projected_bytes
                    .saturating_add(row_bytes(&line, &text, &copy));
                self.render.lines[row] = line;
                self.render.layout.plain[row] = text;
                self.render.layout.copy[row] = copy;
            }
            self.note_block_bytes(block, start, end);
            #[cfg(test)]
            {
                self.reprojections += 1;
            }
        }
    }

    fn splice_block(
        &mut self,
        blocks: &[ScrollBlock],
        theme: &Theme,
        content_width: usize,
        idx: usize,
        prefs: super::RenderPrefs,
    ) -> bool {
        let Some((start, end)) = block_row_range(&self.render.layout, idx) else {
            return false;
        };
        let (styled, copy) = block_rows(
            &blocks[idx],
            &self.plans[idx],
            theme,
            content_width,
            prefs,
            blocks,
        );
        // A row that folds away is a structural change, not a splice.
        if styled.is_empty() {
            return false;
        }
        self.tail_settled = None;
        let plain: Vec<String> = styled
            .iter()
            .map(|line| selectable_row(&blocks[idx], line))
            .collect();
        let old_len = end - start;
        let new_len = styled.len();
        self.projected_bytes = self
            .projected_bytes
            .saturating_sub(self.range_bytes(start, end));
        self.render.lines.splice(start..end, styled);
        self.render.layout.plain.splice(start..end, plain);
        self.render.layout.copy.splice(start..end, copy);
        self.render
            .layout
            .block_index
            .splice(start..end, std::iter::repeat_n(Some(idx), new_len));
        if let Some(tail_start) = self.tail_row_start {
            if tail_start >= end {
                self.tail_row_start =
                    Some(tail_start.saturating_sub(old_len).saturating_add(new_len));
            } else if tail_start >= start {
                self.tail_row_start = trailing_assistant_row_start(&self.render, blocks);
            }
        }
        self.projected_bytes = self
            .projected_bytes
            .saturating_add(self.range_bytes(start, start + new_len));
        self.evicted.remove(&idx);
        self.render.layout.estimated.remove(&idx);
        self.exact_blocks.insert(idx);
        self.note_block_bytes(idx, start, start + new_len);
        #[cfg(test)]
        {
            self.tail_rows += new_len;
        }
        true
    }

    fn splice_tail(
        &mut self,
        blocks: &[ScrollBlock],
        theme: &Theme,
        content_width: usize,
        prefs: super::RenderPrefs,
    ) {
        let tail_index = blocks.len() - 1;
        let row_start = self.tail_row_start.unwrap_or(self.render.lines.len());
        let streamed = match &blocks[tail_index] {
            ScrollBlock::Assistant(text) => {
                let streamed = crate::tui::markdown::render_markdown_streaming(
                    text,
                    assistant_markdown_width(content_width),
                    &assistant_markdown_theme(theme),
                    &mut self.tail_markdown,
                );
                let held = self.tail_settled.filter(|settled| {
                    streamed.settled_kept >= settled.lines
                        && self.block_count == blocks.len()
                        && settled.block == tail_index
                        && Some(settled.start) == self.tail_row_start
                        && settled.start + settled.rows <= self.render.lines.len()
                });
                let from = held.map_or(0, |settled| settled.lines);
                let fresh_lines = streamed.settled[from..].to_vec();
                let fresh_copy = streamed.settled_copy[from..].to_vec();
                Some((
                    held,
                    fresh_lines,
                    fresh_copy,
                    streamed.rest,
                    streamed.rest_copy,
                ))
            }
            _ => None,
        };
        // The settled rows already projected stay in place: only newly
        // settled rows and the unsettled rest are spliced after them.
        if let Some((Some(mut settled), fresh, fresh_copy, rest, rest_copy)) = streamed {
            let keep = settled.start + settled.rows;
            self.projected_bytes = self
                .projected_bytes
                .saturating_sub(self.range_bytes(keep, self.render.lines.len()));
            self.render.lines.truncate(keep);
            self.render.layout.plain.truncate(keep);
            self.render.layout.block_index.truncate(keep);
            self.render.layout.copy.truncate(keep);
            settled.lines += fresh.len();
            let fresh = assistant_rows(fresh, fresh_copy, theme, content_width);
            settled.rows += fresh.0.len();
            self.push_tail_rows(tail_index, fresh.0, fresh.1);
            let fresh_bytes = self.range_bytes(keep, self.render.lines.len());
            settled.bytes = settled.bytes.saturating_add(fresh_bytes);
            let rest_start = self.render.lines.len();
            let rest = assistant_rows(rest, rest_copy, theme, content_width);
            self.push_tail_rows(tail_index, rest.0, rest.1);
            let rest_bytes = self.range_bytes(rest_start, self.render.lines.len());
            self.projected_bytes = self
                .projected_bytes
                .saturating_add(fresh_bytes)
                .saturating_add(rest_bytes);
            self.tail_settled = Some(settled);
            self.note_tail_bytes(tail_index, settled.bytes.saturating_add(rest_bytes));
            return;
        }
        self.projected_bytes = self
            .projected_bytes
            .saturating_sub(self.range_bytes(row_start, self.render.lines.len()));
        self.render.lines.truncate(row_start);
        self.render.layout.plain.truncate(row_start);
        self.render.layout.block_index.truncate(row_start);
        self.render.layout.copy.truncate(row_start);
        if self.block_count < blocks.len() && tail_index > 0 {
            self.render.lines.push(Line::from(""));
            self.render.layout.plain.push(String::new());
            self.render.layout.block_index.push(None);
            self.render.layout.copy.push(RowCopy::default());
        }
        let tail_start = self.render.lines.len();
        self.tail_row_start = Some(tail_start);
        self.tail_settled = None;
        if let Some((_, settled, settled_copy, rest, rest_copy)) = streamed {
            let lines = settled.len();
            let settled = assistant_rows(settled, settled_copy, theme, content_width);
            let rows = settled.0.len();
            self.push_tail_rows(tail_index, settled.0, settled.1);
            let bytes = self.range_bytes(tail_start, tail_start + rows);
            self.tail_settled = Some(SettledTail {
                block: tail_index,
                start: tail_start,
                lines,
                rows,
                bytes,
            });
            let rest = assistant_rows(rest, rest_copy, theme, content_width);
            self.push_tail_rows(tail_index, rest.0, rest.1);
        } else {
            let (styled, copy) = block_rows(
                &blocks[tail_index],
                &super::RowPlan::default(),
                theme,
                content_width,
                prefs,
                blocks,
            );
            self.push_tail_rows(tail_index, styled, copy);
        }
        let tail_end = self.render.lines.len();
        let bytes = self.range_bytes(tail_start, tail_end);
        self.projected_bytes = self.projected_bytes.saturating_add(bytes);
        self.note_tail_bytes(tail_index, bytes);
    }

    fn push_tail_rows(
        &mut self,
        tail_index: usize,
        styled: Vec<Line<'static>>,
        copy: Vec<RowCopy>,
    ) {
        #[cfg(test)]
        {
            self.tail_rows += styled.len();
        }
        self.render
            .layout
            .plain
            .extend(styled.iter().map(plain_row));
        self.render.layout.copy.extend(copy);
        self.render
            .layout
            .block_index
            .extend(std::iter::repeat_n(Some(tail_index), styled.len()));
        self.render.lines.extend(styled);
    }

    fn note_tail_bytes(&mut self, tail_index: usize, bytes: usize) {
        self.evicted.remove(&tail_index);
        self.render.layout.estimated.remove(&tail_index);
        if bytes > OVERSIZED_BLOCK_PROJECTION_BYTES {
            self.oversized.insert(tail_index);
        } else {
            self.oversized.remove(&tail_index);
        }
    }

    /// Full re-projections performed so far.
    #[cfg(test)]
    #[must_use]
    pub const fn rebuilds(&self) -> usize {
        self.rebuilds
    }

    /// `(rows projected in full passes, rows projected by splices)`.
    #[cfg(test)]
    #[must_use]
    pub const fn work_rows(&self) -> (usize, usize) {
        (self.full_rows, self.tail_rows)
    }
}

fn block_row_range(layout: &LineLayout, idx: usize) -> Option<(usize, usize)> {
    let start = layout
        .block_index
        .iter()
        .position(|row| *row == Some(idx))?;
    let end = start
        + layout.block_index[start..]
            .iter()
            .take_while(|row| **row == Some(idx))
            .count();
    Some((start, end))
}

fn trailing_assistant_row_start(
    render: &TranscriptRender,
    blocks: &[ScrollBlock],
) -> Option<usize> {
    let tail_index = blocks.len().checked_sub(1)?;
    matches!(blocks.last(), Some(ScrollBlock::Assistant(_))).then(|| {
        render
            .layout
            .block_index
            .iter()
            .position(|index| *index == Some(tail_index))
            .unwrap_or(render.lines.len())
    })
}

#[cfg(test)]
#[test]
fn expanded_command_rails_are_muted_without_muting_the_command() {
    let theme = Theme::dark();
    let line = activity_line("  └ $ cargo test".into(), &theme, theme.success);
    assert_eq!(line.spans[0].content, "  └ ");
    assert_eq!(line.spans[0].style, theme.tool_label);
    assert_eq!(line.spans[1].content, "$ cargo test");
    assert_eq!(line.spans[1].style, theme.tool);
}

/// A multi-line command is one highlighted unit: a string opened on the
/// first row keeps its colour on the next, and only the first row's
/// program takes the accent.
#[cfg(test)]
#[test]
fn a_multi_line_command_highlights_as_one_unit() {
    let theme = Theme::dark();
    let lines = command_lines("cargo test \"a\nb\" &&\n  ls -la", &theme);
    let styled = |line: &Line<'static>, text: &str| {
        line.spans
            .iter()
            .filter(|span| span.content.contains(text))
            .map(|span| span.style)
            .collect::<Vec<_>>()
    };
    assert_eq!(lines.len(), 3);
    assert_eq!(styled(&lines[0], "$"), vec![theme.command_prompt]);
    assert_eq!(styled(&lines[0], "cargo"), vec![theme.command_program]);
    assert_eq!(styled(&lines[1], "b"), vec![theme.command_string]);
    assert!(!lines[2].to_string().contains('$'));
    assert!(
        lines[2]
            .spans
            .iter()
            .all(|span| span.style != theme.command_program),
        "{:?}",
        lines[2]
    );
}

fn activity_line(text: String, theme: &Theme, status: Style) -> Line<'static> {
    let glyph = text
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '├' | '└' | '─' | '▶' | '▼'));
    let Some((offset, ch)) = glyph else {
        return Line::from(Span::styled(text, theme.tool));
    };
    if !matches!(ch, '✓' | '✗' | '⠼') {
        return Line::from(vec![
            Span::styled(text[..offset].to_owned(), theme.tool_label),
            Span::styled(text[offset..].to_owned(), theme.tool),
        ]);
    }
    Line::from(vec![
        Span::styled(text[..offset].to_owned(), theme.tool_label),
        Span::styled(ch.to_string(), status),
        Span::styled(text[offset + ch.len_utf8()..].to_owned(), theme.tool),
    ])
}

/// Source is the sole admitted detail beneath an activity action.
fn assistant_markdown_theme(theme: &Theme) -> crate::tui::markdown::MarkdownTheme {
    crate::tui::markdown::MarkdownTheme {
        text: theme.assistant,
        headings: [theme.assistant_label; 6],
        strong: theme.assistant_label,
        emphasis: theme.assistant,
        strikethrough: theme.system,
        inline_code: theme.tool,
        code_block: theme.tool,
        code_language: theme.tool_label.add_modifier(Modifier::BOLD),
        syntax: theme.syntax_styles_for_code(theme.tool),
        link: theme.markdown_link,
        path: theme.tool,
        quote_marker: theme.tool_label,
        list_marker: theme.tool_label,
        rule: theme.system,
    }
}

const fn assistant_markdown_width(content_width: usize) -> u16 {
    let width = content_width.saturating_sub(2);
    if width == 0 {
        1
    } else if width > u16::MAX as usize {
        u16::MAX
    } else {
        #[allow(clippy::cast_possible_truncation, reason = "bounded just above")]
        {
            width as u16
        }
    }
}

/// Indent and wrap rendered markdown rows as an assistant block's rows.
/// Rows are independent, so any split of the input maps to the same split
/// of the output. The two-space block indent is decoration, added to the
/// markdown row's own rail width.
fn assistant_rows(
    lines: Vec<Line<'static>>,
    copies: Vec<RowCopy>,
    theme: &Theme,
    content_width: usize,
) -> (Vec<Line<'static>>, Vec<RowCopy>) {
    let mut out_lines = Vec::with_capacity(lines.len());
    let mut out_copy = Vec::with_capacity(lines.len());
    for (mut line, mut copy) in lines.into_iter().zip(copies) {
        line.spans.insert(0, Span::styled("  ", theme.assistant));
        copy.decoration = copy.decoration.saturating_add(2);
        let wrapped = wrap_styled_line(line, content_width.max(1));
        if wrapped.len() <= 1 {
            out_lines.extend(wrapped);
            out_copy.push(copy);
            continue;
        }
        // Markdown already fitted the row to the pane minus this indent.
        // A split here means the pane is narrower than that; later
        // fragments are leftover content, not a second copy of the source.
        let mut wrapped = wrapped.into_iter();
        if let Some(first) = wrapped.next() {
            copy.decoration = copy.decoration.min(cells_u16(first.width()));
            copy.code = None;
            copy.language = false;
            out_lines.push(first);
            out_copy.push(copy);
        }
        for extra in wrapped {
            out_lines.push(extra);
            out_copy.push(RowCopy::default());
        }
    }
    (out_lines, out_copy)
}

fn cells_u16(cells: usize) -> u16 {
    u16::try_from(cells).unwrap_or(u16::MAX)
}

/// Text columns a prompt row keeps before its send time is dropped.
const MIN_PROMPT_COLUMNS: usize = 20;

/// The prompt band of a user or steer row: the `›` gutter and the text,
/// with the steer tag and the dimmed send time right-aligned on the first
/// row (spec 02 §5.4). At narrow widths the send time is dropped first,
/// then the tag.
fn prompt_band_lines(
    text: &str,
    tag: Option<&'static str>,
    sent: Option<String>,
    theme: &Theme,
    content_width: usize,
) -> Vec<Line<'static>> {
    let w = content_width.saturating_sub(2).max(1);
    let tag_width = tag.map_or(0, unicode_width::UnicodeWidthStr::width);
    let sent = sent
        .map(|label| format!(" {label}"))
        .filter(|label| w >= tag_width + label.width() + 1 + MIN_PROMPT_COLUMNS);
    let trailing_width = tag_width + sent.as_ref().map_or(0, |label| label.width());
    let rows = if trailing_width == 0 {
        super::wrap_plain(text, w)
    } else {
        // The first row keeps room for the trailing labels and a gap.
        super::steer_rows(text, w, w.saturating_sub(trailing_width + 1).max(1))
    };
    let lines = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let gutter = if index == 0 {
                super::PROMPT_PREFIX
            } else {
                "  "
            };
            let mut line = Line::from(vec![
                Span::styled(gutter, theme.user_label),
                Span::styled(row, theme.user),
            ]);
            let padding = content_width.saturating_sub(line.width());
            let mut trailing: Vec<Span<'static>> = Vec::new();
            if index == 0 {
                let shown_sent = sent.as_ref().filter(|_| padding > trailing_width);
                if let Some(tag) = tag.filter(|_| padding > tag_width) {
                    trailing.push(Span::styled(tag, theme.tool_label));
                }
                if let Some(label) = shown_sent {
                    trailing.push(Span::styled(label.clone(), theme.chrome_dim));
                }
            }
            let used: usize = trailing.iter().map(Span::width).sum();
            if padding > used {
                line.spans.push(Span::raw(" ".repeat(padding - used)));
            }
            line.spans.extend(trailing);
            line.style(Style::default().bg(theme.user_bg))
        })
        .collect::<Vec<_>>();
    wrap_styled_lines(lines, content_width)
}

fn assistant_lines(text: &str, theme: &Theme, content_width: usize) -> Vec<Line<'static>> {
    assistant_block(text, theme, content_width).0
}

/// Rendered assistant rows and the copy metadata parallel to them.
fn assistant_block(
    text: &str,
    theme: &Theme,
    content_width: usize,
) -> (Vec<Line<'static>>, Vec<RowCopy>) {
    // Markdown is parsed only for presentation. Source remains the
    // durable assistant text and malformed input stays inert.
    let (lines, copy) = crate::tui::markdown::render_markdown_rows(
        text,
        assistant_markdown_width(content_width),
        &assistant_markdown_theme(theme),
    );
    assistant_rows(lines, copy, theme, content_width)
}

/// [`block_lines`] plus copy metadata for each painted row.
fn block_rows(
    block: &ScrollBlock,
    plan: &super::RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: super::RenderPrefs,
    blocks: &[ScrollBlock],
) -> (Vec<Line<'static>>, Vec<RowCopy>) {
    if plan.hidden {
        return (Vec::new(), Vec::new());
    }
    if let ScrollBlock::Assistant(text) = block {
        return assistant_block(text, theme, content_width);
    }
    let mut lines = block_content_lines(block, plan, theme, content_width, prefs);
    let mut copy = vec![RowCopy::default(); lines.len()];
    if !plan.group_folded && !plan.cell_tails.is_empty() {
        if lines.last().is_some_and(|row| row.width() == 0) {
            lines.pop();
            copy.pop();
        }
        for tail in &plan.cell_tails {
            let (footer, metadata) = cell_footer_rows(tail, blocks, theme, content_width);
            lines.extend(footer);
            copy.extend(metadata);
        }
        if super::tool_block_closes_with_blank(plan) {
            lines.push(Line::from(""));
            copy.push(RowCopy::default());
        }
    }
    (lines, copy)
}

/// Render one isolated block for tests.
#[cfg(test)]
#[must_use]
pub fn block_lines(
    block: &ScrollBlock,
    plan: &super::RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: super::RenderPrefs,
) -> Vec<Line<'static>> {
    block_rows(
        block,
        plan,
        theme,
        content_width,
        prefs,
        std::slice::from_ref(block),
    )
    .0
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn block_lines_with(
    block: &ScrollBlock,
    plan: &super::RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: super::RenderPrefs,
    blocks: &[ScrollBlock],
) -> Vec<Line<'static>> {
    block_rows(block, plan, theme, content_width, prefs, blocks).0
}

/// Render one block as styled lines.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "localized rendering for all transcript block variants"
)]
fn block_content_lines(
    block: &ScrollBlock,
    plan: &super::RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: super::RenderPrefs,
) -> Vec<Line<'static>> {
    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::User { text, sent_at_ms } => prompt_band_lines(
            text,
            None,
            sent_at_ms.and_then(|sent| prefs.prompt_clock.label(sent)),
            theme,
            content_width,
        ),
        ScrollBlock::Assistant(text) => assistant_lines(text, theme, content_width),
        ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => Vec::new(),
        ScrollBlock::Discarded { text, thinking } => {
            let width = content_width.saturating_sub(2).max(1);
            std::iter::once(Line::from(Span::styled(
                format!("  {}", super::discarded_label(*thinking)),
                theme.chrome_dim,
            )))
            .chain(
                super::wrap_plain(text, width)
                    .into_iter()
                    .map(|line| Line::from(Span::styled(format!("  {line}"), theme.chrome_dim))),
            )
            .collect()
        }
        ScrollBlock::Thinking {
            text, sealed, mode, ..
        } => {
            if !prefs.show_thinking {
                return Vec::new();
            }
            if *sealed && text.trim().is_empty() && plan.activity_summary.is_none() {
                return Vec::new();
            }
            let mut logical: Vec<_> = lead_rows(block, plan, theme)
                .iter()
                .map(|lead| lead.line(theme, content_width))
                .collect();
            if !plan.group_folded && mode.shows_detail() && super::thinking_has_detail(text) {
                let rail = super::thinking_detail_rail(plan);
                for row in super::thinking_detail_rows(text, *mode) {
                    logical.extend(wrap_styled_line_railed(
                        &rail,
                        &rail,
                        theme.system,
                        Line::from(Span::styled(row, theme.system)),
                        content_width,
                    ));
                }
            }
            if super::tool_block_closes_with_blank(plan) {
                logical.push(Line::from(""));
            }
            logical
        }
        ScrollBlock::Approval {
            tool_name,
            subject,
            prompt,
            resolution,
            ..
        } => {
            let rows = super::format_approval_block(
                tool_name,
                subject.as_deref(),
                prompt.as_deref(),
                resolution.as_deref(),
            );
            let head_style = match resolution.as_deref() {
                None => theme.busy,
                Some("allowed") => theme.success,
                Some(_) => theme.error_label,
            };
            let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 1);
            for (index, row) in rows.into_iter().enumerate() {
                let style = if index == 0 { head_style } else { theme.system };
                lines.push(Line::from(Span::styled(row, style)));
            }
            let mut logical = wrap_styled_lines(lines, content_width);
            logical.push(Line::from(""));
            logical
        }
        ScrollBlock::Tool {
            kind: ActivityKind::Compact,
            name,
            state,
            subject,
            ..
        } => {
            // Compaction rows keep their own grammar (§5.4).
            let running = super::tool_is_running(state);
            let style = if running {
                theme.busy
            } else if super::tool_is_failed(state) {
                theme.error_label
            } else {
                theme.success
            };
            let action = crate::tui::activity::action(
                name,
                state,
                ActivityKind::Compact,
                subject.as_deref(),
                plan,
            );
            let mut logical = wrap_styled_line_railed(
                "  ",
                "    ",
                theme.tool_label,
                activity_line(action.trim_start().to_owned(), theme, style),
                content_width.max(1),
            );
            if super::tool_block_closes_with_blank(plan) {
                logical.push(Line::from(""));
            }
            logical
        }
        ScrollBlock::Tool {
            kind,
            name,
            state,
            subject,
            detail,
            mode,
            view,
            ..
        } => {
            let leads = lead_rows(block, plan, theme);
            let mut logical: Vec<Line<'static>> = leads
                .iter()
                .map(|lead| lead.line(theme, content_width))
                .collect();
            if !plan.group_folded {
                if *kind != ActivityKind::Compose && *mode == super::DisplayMode::Expanded {
                    let rail = leads.last().map_or("  ", |lead| lead.rail);
                    push_item_detail(
                        &mut logical,
                        &ItemFacts {
                            name,
                            kind: *kind,
                            state,
                            subject: subject.as_deref(),
                            detail: detail.as_deref(),
                            view,
                        },
                        rail,
                        theme,
                        content_width,
                    );
                }
            }
            if super::tool_block_closes_with_blank(plan) {
                logical.push(Line::from(""));
            }
            logical
        }
        ScrollBlock::Milestone { milestone, open } => {
            let mut logical: Vec<Line<'static>> = lead_rows(block, plan, theme)
                .iter()
                .map(|lead| lead.line(theme, content_width))
                .collect();
            if *open {
                for row in super::milestone_detail(milestone) {
                    logical.extend(wrap_styled_line_railed(
                        "    │ ",
                        "    │   ",
                        theme.tool_label,
                        Line::from(Span::styled(row, theme.system)),
                        content_width.max(1),
                    ));
                }
            }
            if super::tool_block_closes_with_blank(plan) {
                logical.push(Line::from(""));
            }
            logical
        }
        ScrollBlock::Steer { text, sent_at_ms } => prompt_band_lines(
            text,
            Some(" steer"),
            sent_at_ms.and_then(|sent| prefs.prompt_clock.label(sent)),
            theme,
            content_width,
        ),
        ScrollBlock::Header(text) => wrap_styled_line_railed(
            "  ",
            "    ",
            theme.chrome_dim,
            Line::from(Span::styled(text.clone(), theme.chrome_dim)),
            content_width.max(1),
        ),
        ScrollBlock::Item { .. } => {
            let mut logical: Vec<Line<'static>> = lead_rows(block, plan, theme)
                .iter()
                .map(|lead| lead.line(theme, content_width))
                .collect();
            if super::tool_block_closes_with_blank(plan) {
                logical.push(Line::from(""));
            }
            logical
        }
        ScrollBlock::Mail { .. } => super::mail_lines(block, plan, content_width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, theme.system)))
            .collect(),
        ScrollBlock::System(text) => {
            // Hide pure-noise lines that used to pollute the session log.
            if system_row_is_hidden(text) {
                return Vec::new();
            }
            // Every notice keeps the transcript gutter (§5.4); a marker
            // glyph takes its own colour.
            let glyph_style = match text.chars().next() {
                Some('⊘') => Some(theme.warning),
                Some('✓') => Some(theme.success),
                _ => None,
            };
            let line = match glyph_style {
                Some(glyph) => {
                    let split = text.chars().next().map_or(0, char::len_utf8);
                    Line::from(vec![
                        Span::styled(text[..split].to_owned(), glyph),
                        Span::styled(text[split..].to_owned(), theme.system),
                    ])
                }
                None => Line::from(Span::styled(text.clone(), theme.system)),
            };
            wrap_styled_line_railed("  ", "    ", theme.system, line, content_width.max(1))
        }
        ScrollBlock::Error(text) => wrap_styled_line_railed(
            "  ",
            "    ",
            theme.error_label,
            Line::from(vec![
                Span::styled("⚠ ", theme.error_label),
                Span::styled(text.clone(), theme.error),
            ]),
            content_width.max(1),
        ),
    }
}

/// One clickable one-line row at the top of a block: a group summary, a
/// merged row, an item, a thinking or a milestone row.
pub(crate) struct LeadRow {
    /// Rail and indent before the glyph.
    pub(crate) rail: &'static str,
    glyph: (&'static str, Style),
    label: Vec<Span<'static>>,
    /// A failure count the cut never hides (`(1 failed)`).
    problems: Option<String>,
    meta: Vec<String>,
    /// Cells the label keeps before a meta part drops.
    label_floor: usize,
    /// Leading meta parts that never drop; the label is cut instead.
    kept_meta: usize,
    caret: &'static str,
    /// The whole label a copy carries: uncut, with the subject's own
    /// newlines rather than `↵`, failure count included.
    copy: String,
    thinking: bool,
}

impl LeadRow {
    fn line(&self, theme: &Theme, width: usize) -> Line<'static> {
        let meta: Vec<&str> = self.meta.iter().map(String::as_str).collect();
        item_row(
            vec![
                Span::styled(self.rail, theme.tool_label),
                Span::styled(self.glyph.0, self.glyph.1),
                Span::styled(" ", theme.tool),
            ],
            self.label.clone(),
            self.problems
                .as_ref()
                .map(|problems| vec![Span::styled(format!(" {problems}"), theme.tool)])
                .unwrap_or_default(),
            &meta,
            (self.label_floor, self.kept_meta),
            self.caret,
            theme,
            width,
        )
    }

    /// The row's untruncated text, which a copy of the row carries.
    pub(crate) fn full_text(&self) -> String {
        if self.thinking {
            return format!("◆ {}", self.copy);
        }
        let mut text = format!("{}{} {}", self.rail, self.glyph.0, self.copy);
        // The tip slot is room for paint, not text.
        for meta in self.meta.iter().filter(|meta| !meta.trim().is_empty()) {
            text.push_str(" · ");
            text.push_str(meta);
        }
        text
    }
}

/// Cells a running item keeps free at its right edge for the tip's
/// elapsed (`59m59s` and its gap); dropped like any meta under the
/// 12-cell subject rule.
const TIP_META_SLOT: &str = "     ";

/// A label and the failure count at its end, which a cut must not hide.
fn split_problems(label: String) -> (String, Option<String>) {
    if (label.ends_with(" failed)") || label.ends_with(" interrupted)"))
        && let Some(at) = label.rfind(" (")
    {
        let problems = label[at + 1..].to_owned();
        let mut body = label;
        body.truncate(at);
        return (body, Some(problems));
    }
    (label, None)
}

fn split_meta(meta: Option<&str>) -> Vec<String> {
    meta.map(|meta| meta.split(" · ").map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The one-line rows a block starts with, in order (spec 02 §5.4).
pub(crate) fn lead_rows(block: &ScrollBlock, plan: &super::RowPlan, theme: &Theme) -> Vec<LeadRow> {
    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::Mail {
            text,
            open,
            group_open,
        } => {
            let row = |label: String, copy: String, open: bool| LeadRow {
                rail: "  ",
                glyph: (if open { "▾" } else { "▸" }, theme.system),
                label: vec![Span::styled(label, theme.system)],
                problems: None,
                meta: Vec::new(),
                label_floor: MIN_SUBJECT_CELLS,
                kept_meta: 0,
                caret: "",
                copy,
                thinking: false,
            };
            let mut rows = Vec::new();
            if let Some(summary) = &plan.mail_summary {
                rows.push(row(summary.clone(), summary.clone(), *group_open));
            }
            if !plan.group_folded {
                rows.push(row(
                    text.lines().next().unwrap_or_default().to_owned(),
                    text.clone(),
                    *open,
                ));
            }
            rows
        }

        ScrollBlock::Tool {
            kind,
            name,
            state,
            subject,
            mode,
            wait,
            ..
        } if *kind != ActivityKind::Compact => {
            let mut rows = Vec::new();
            rows.extend(summary_row(plan, theme));
            if plan.group_folded {
                return rows;
            }
            let merged = plan.activity_action.is_some() && *kind != ActivityKind::Compose;
            let item = |plan: &super::RowPlan, caret| {
                let glyph = crate::tui::activity::item_glyph(state, *kind, plan);
                let glyph_style = match glyph {
                    "⠼" => theme.busy,
                    "✗" => theme.error_label,
                    _ => theme.success,
                };
                let copy = crate::tui::activity::item_label_raw(
                    name,
                    state,
                    *kind,
                    subject.as_deref(),
                    wait.as_ref(),
                    plan,
                );
                let label = crate::tui::activity::one_line(&copy);
                let (label, problems) = if let Some(command) = label.strip_prefix("$ ") {
                    (command_spans(command, theme), None)
                } else {
                    let (label, problems) = split_problems(label);
                    (vec![Span::styled(label, theme.tool)], problems)
                };
                let mut meta = split_meta(
                    crate::tui::activity::item_meta(name, state, *kind, subject.as_deref(), plan)
                        .as_deref(),
                );
                if super::tool_is_running(state) || plan.activity_running {
                    meta.push(TIP_META_SLOT.to_owned());
                }
                LeadRow {
                    rail: super::row_indent(plan),
                    glyph: (glyph, glyph_style),
                    label,
                    problems,
                    meta,
                    label_floor: MIN_SUBJECT_CELLS,
                    kept_meta: 0,
                    caret,
                    copy,
                    thinking: false,
                }
            };
            if merged {
                let caret = if plan.members_open { "▾" } else { "▸" };
                rows.push(item(plan, caret));
                if plan.members_open {
                    let own = super::RowPlan {
                        activity_action: None,
                        activity_running: false,
                        activity_failed: false,
                        activity_meta: None,
                        compose_child: Some(false),
                        ..plan.clone()
                    };
                    rows.push(item(&own, crate::tui::activity::item_caret(*mode)));
                }
            } else {
                rows.push(item(plan, crate::tui::activity::item_caret(*mode)));
            }
            rows
        }
        ScrollBlock::Milestone { milestone, open } => {
            let (glyph, label) = super::milestone_parts(milestone);
            let glyph_style = match glyph {
                "✓" => theme.success,
                "✗" => theme.error_label,
                "⚠" | "?" => theme.warning,
                _ => theme.command_program,
            };
            let caret = if !super::milestone_has_detail(milestone) {
                ""
            } else if *open {
                "▾"
            } else {
                "▸"
            };
            vec![LeadRow {
                rail: "  ",
                glyph: (glyph, glyph_style),
                label: vec![Span::styled(
                    crate::tui::activity::one_line(&label),
                    theme.tool,
                )],
                problems: None,
                meta: Vec::new(),
                label_floor: MIN_SUBJECT_CELLS,
                kept_meta: 0,
                caret,
                copy: label,
                thinking: false,
            }]
        }
        ScrollBlock::Thinking {
            text,
            sealed,
            elapsed_ms,
            mode,
            ..
        } => {
            let mut rows: Vec<_> = summary_row(plan, theme).into_iter().collect();
            if !plan.group_folded && (!*sealed || !text.trim().is_empty()) {
                let label = if *sealed {
                    super::thinking_summary(text)
                } else {
                    "Thinking…".to_owned()
                };
                rows.push(LeadRow {
                    rail: super::row_indent(plan),
                    glyph: (
                        if *sealed { "◆" } else { "⠼" },
                        if *sealed {
                            theme.tool_label
                        } else {
                            theme.busy
                        },
                    ),
                    label: vec![Span::styled(label.clone(), theme.user_label)],
                    problems: None,
                    meta: if *sealed {
                        elapsed_ms
                            .map(super::format_group_elapsed)
                            .into_iter()
                            .collect()
                    } else {
                        Vec::new()
                    },
                    label_floor: MIN_SUBJECT_CELLS,
                    kept_meta: 0,
                    caret: if !*sealed || !super::thinking_has_detail(text) {
                        ""
                    } else if mode.shows_detail() {
                        "▾"
                    } else {
                        "▸"
                    },
                    copy: label,
                    thinking: true,
                });
            }
            rows
        }
        ScrollBlock::Item { text, meta } => vec![LeadRow {
            rail: super::row_indent(plan),
            glyph: ("·", theme.system),
            label: vec![Span::styled(
                crate::tui::activity::one_line(text),
                theme.system,
            )],
            problems: None,
            meta: meta.clone(),
            // Only a process row has meta: its job id and outcome
            // outlast the command and the duration.
            label_floor: text.split(' ').next().map_or(0, |id| id.width() + 1),
            kept_meta: 1,
            caret: "",
            copy: text.clone(),
            thinking: false,
        }],
        _ => Vec::new(),
    }
}

fn summary_row(plan: &super::RowPlan, theme: &Theme) -> Option<LeadRow> {
    let copy = plan.activity_summary.as_ref()?.clone();
    let (label, problems) = split_problems(crate::tui::activity::one_line(&copy));
    Some(LeadRow {
        rail: "  ",
        glyph: if plan.activity_failed {
            ("✗", theme.error_label)
        } else {
            ("✓", theme.success)
        },
        label: vec![Span::styled(label, theme.tool)],
        problems,
        meta: split_meta(plan.summary_meta.as_deref()),
        label_floor: MIN_SUBJECT_CELLS,
        kept_meta: 0,
        caret: crate::tui::activity::header_caret(plan),
        copy,
        thinking: false,
    })
}

fn cell_footer_rows(
    tail: &super::CellTail,
    blocks: &[ScrollBlock],
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<RowCopy>) {
    let Some(ScrollBlock::Tool {
        name,
        kind,
        state,
        subject,
        detail,
        mode,
        view,
        ..
    }) = blocks.get(tail.owner)
    else {
        return (Vec::new(), Vec::new());
    };
    let rail = super::detail_rail(tail.rail);
    let mut rows = Vec::new();
    let mut copy = Vec::new();
    if tail.hidden_calls > 0 {
        let label = format!("… {} more", tail.hidden_calls);
        let more_rail = super::row_indent(&super::RowPlan {
            group_child: match tail.rail {
                "  ├─  " => Some(false),
                "  └─  " => Some(true),
                _ => None,
            },
            compose_child: Some(true),
            ..super::RowPlan::default()
        });
        let mut line = item_row(
            vec![Span::styled(more_rail, theme.tool_label)],
            vec![Span::styled(label, theme.tool_label)],
            Vec::new(),
            &[],
            (0, 0),
            "",
            theme,
            width,
        );
        line.alignment = Some(ratatui::layout::Alignment::Center);
        rows.push(line);
        copy.push(RowCopy::default());
    }
    if !mode.shows_detail() {
        return (rows, copy);
    }
    if let Some(source) = detail.as_deref().filter(|source| !source.trim().is_empty()) {
        let language = super::compose_program_language(name).unwrap_or("javascript");
        let fence = "`".repeat(
            source
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1,
        );
        let fenced = format!("{fence}{language}\n{source}\n{fence}");
        let (source_rows, source_copy) = crate::tui::markdown::render_markdown_rows(
            &fenced,
            u16::try_from(width.saturating_sub(rail.width()).max(1)).unwrap_or(u16::MAX),
            &assistant_markdown_theme(theme),
        );
        for (mut row, mut metadata) in source_rows.into_iter().zip(source_copy) {
            row.spans
                .insert(0, Span::styled(rail.clone(), theme.tool_label));
            metadata.decoration = metadata.decoration.saturating_add(cells_u16(rail.width()));
            rows.push(row);
            copy.push(metadata);
        }
    }
    let mut output = Line::from(truncate_spans(
        vec![Span::styled(
            format!("{rail}output {}", if view.output_open { "▾" } else { "▸" }),
            theme.tool_label,
        )],
        width,
    ));
    output.alignment = Some(ratatui::layout::Alignment::Right);
    rows.push(output);
    copy.push(RowCopy {
        decoration: cells_u16(rail.width()),
        ..RowCopy::default()
    });
    if view.output_open {
        let first = rows.len();
        push_item_detail(
            &mut rows,
            &ItemFacts {
                name,
                kind: *kind,
                state,
                subject: subject.as_deref(),
                detail: detail.as_deref(),
                view,
            },
            tail.rail,
            theme,
            width,
        );
        copy.extend(std::iter::repeat_n(RowCopy::default(), rows.len() - first));
    }
    (rows, copy)
}

/// One tool item's fields the detail reads.
struct ItemFacts<'a> {
    name: &'a str,
    kind: ActivityKind,
    state: &'a str,
    subject: Option<&'a str>,
    detail: Option<&'a str>,
    view: &'a super::ItemView,
}

/// What the detail shows: the host's canonical answer, or the retained
/// preview (labelled) when the answer is not in yet or the call is no
/// longer in canonical history.
struct DetailText<'a> {
    input: &'a str,
    output: &'a str,
    input_truncated: bool,
    output_truncated: bool,
    preview: bool,
    /// Host-parsed sources of a web search or fetch result.
    sources: &'a [plakous_host_protocol::views::SourceLink],
}

fn detail_text<'a>(item: &'a ItemFacts<'a>) -> DetailText<'a> {
    use plakous_host_protocol::views::{CallDetailOutcome, CallDetailSource};
    if let Some(fetched) = item.view.fetched.as_deref()
        && let CallDetailOutcome::Detail {
            input,
            output,
            input_truncated,
            output_truncated,
            source,
            sources,
        } = fetched
    {
        let preview = *source == CallDetailSource::Preview;
        return DetailText {
            input,
            output,
            input_truncated: *input_truncated && !preview,
            output_truncated: *output_truncated && !preview,
            preview,
            sources,
        };
    }
    // The host's own preview rule: a running row's detail is its
    // arguments; a finished row's subject stands for the input and its
    // detail is the result preview.
    let running = super::tool_is_running(item.state);
    // A cell's retained detail is its source, drawn above; it has no
    // output preview.
    let (input, output) = if item.kind == ActivityKind::Compose {
        ("", "")
    } else if running {
        (item.detail.or(item.subject).unwrap_or_default(), "")
    } else {
        (
            item.subject.unwrap_or_default(),
            item.detail.unwrap_or_default(),
        )
    };
    // Only a settled answer, or the lack of anything to ask, names the
    // preview; a pending fetch shows the preview without the label.
    let preview = item.view.fetched.is_some();
    DetailText {
        input,
        output,
        input_truncated: false,
        output_truncated: false,
        preview,
        sources: &[],
    }
}

/// Lines past which a write's detail stops showing the content.
const WRITE_HEAD_LINES: usize = 20;

/// Input first, then the output window, every row under the item's rail
/// with a hanging indent (spec 02 §5.4).
fn push_item_detail(
    logical: &mut Vec<Line<'static>>,
    item: &ItemFacts<'_>,
    item_rail: &str,
    theme: &Theme,
    width: usize,
) {
    let rail = super::detail_rail(item_rail);
    let hanging = format!("{rail}  ");
    let push = |logical: &mut Vec<Line<'static>>, line: Line<'static>| {
        logical.extend(wrap_styled_line_railed(
            &rail,
            &hanging,
            theme.tool_label,
            line,
            width.max(1),
        ));
    };
    let text = detail_text(item);
    if text.preview {
        push(
            logical,
            Line::from(Span::styled("preview", theme.chrome_dim)),
        );
    }
    let verb = super::subject_tool_verb(item.name, item.kind, item.subject);
    // A code cell's source is already drawn above its detail.
    if item.kind != ActivityKind::Compose {
        for line in input_lines(verb, text.input, theme) {
            push(logical, line);
        }
        if text.input_truncated {
            push(
                logical,
                Line::from(Span::styled("… input truncated", theme.chrome_dim)),
            );
        }
    }
    if matches!(
        verb,
        super::ToolVerb::WebSearch | super::ToolVerb::XSearch | super::ToolVerb::Fetch
    ) && !text.sources.is_empty()
    {
        push(
            logical,
            Line::from(Span::styled("sources", theme.chrome_dim)),
        );
        for source in text.sources {
            let mut spans = Vec::new();
            if !source.title.is_empty() {
                spans.push(Span::styled(format!("{}  ", source.title), theme.tool));
            }
            spans.push(Span::styled(source.url.clone(), theme.chrome_dim));
            push(logical, Line::from(spans));
        }
    }
    let output = text.output.trim_end_matches('\n');
    if output.trim().is_empty() {
        return;
    }
    push(logical, Line::from(Span::styled("┄", theme.tool_label)));
    let lines: Vec<&str> = output.split('\n').collect();
    let kept = super::TOOL_DETAIL_HEAD_LINES + super::TOOL_DETAIL_TAIL_LINES;
    let style = theme.detail_output;
    if item.view.full_output || lines.len() <= kept + 1 {
        for line in &lines {
            push(logical, Line::from(Span::styled((*line).to_owned(), style)));
        }
    } else {
        for line in &lines[..super::TOOL_DETAIL_HEAD_LINES] {
            push(logical, Line::from(Span::styled((*line).to_owned(), style)));
        }
        let tail_start = lines.len() - super::TOOL_DETAIL_TAIL_LINES;
        // A spill line names where the whole result went; folding the
        // window must never hide it.
        let spills: Vec<&str> = lines[super::TOOL_DETAIL_HEAD_LINES..tail_start]
            .iter()
            .copied()
            .filter(|line| line.trim_start().starts_with(super::FULL_OUTPUT_PREFIX))
            .collect();
        let hidden = lines.len() - kept - spills.len();
        if hidden > 0 {
            push(
                logical,
                Line::from(Span::styled(
                    super::hidden_lines_label(hidden),
                    theme.tool_label,
                )),
            );
            if let Some(row) = logical.last_mut() {
                super::mark_hidden_lines(row);
            }
        }
        for line in spills {
            push(logical, Line::from(Span::styled(line.to_owned(), style)));
        }
        for line in &lines[tail_start..] {
            push(logical, Line::from(Span::styled((*line).to_owned(), style)));
        }
    }
    if text.output_truncated {
        push(
            logical,
            Line::from(Span::styled("… output truncated", theme.chrome_dim)),
        );
    }
}

/// The input part of an item's detail: the full command, a diff, a
/// write's first lines, or `key: value` arguments, never raw JSON.
fn input_lines(verb: super::ToolVerb, input: &str, theme: &Theme) -> Vec<Line<'static>> {
    let parsed = serde_json::from_str::<serde_json::Value>(input).ok();
    let Some(serde_json::Value::Object(mut args)) = parsed else {
        return match verb {
            super::ToolVerb::Run => command_lines(input, theme),
            _ => input
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), theme.tool)))
                .collect(),
        };
    };
    let take_str = |args: &mut serde_json::Map<String, serde_json::Value>, keys: &[&str]| {
        keys.iter().find_map(|key| match args.get(*key) {
            Some(serde_json::Value::String(text)) => {
                let text = text.clone();
                args.remove(*key);
                Some(text)
            }
            _ => None,
        })
    };
    let mut lines = Vec::new();
    match verb {
        super::ToolVerb::Run => {
            let command = match args.get("cmd").or_else(|| args.get("command")) {
                Some(serde_json::Value::Array(parts)) => Some(
                    parts
                        .iter()
                        .map(|part| {
                            part.as_str()
                                .map_or_else(|| part.to_string(), str::to_owned)
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            };
            let command = command.or_else(|| take_str(&mut args, &["cmd", "command"]));
            args.remove("cmd");
            args.remove("command");
            if let Some(command) = command {
                lines.extend(command_lines(&command, theme));
            }
        }
        super::ToolVerb::Edit => {
            let path = take_str(&mut args, &["file_path", "path"]);
            if let Some(patch) = take_str(&mut args, &["patch", "input"]) {
                lines.extend(diff_lines(patch.lines().map(str::to_owned), theme));
            } else if let (Some(old), Some(new)) = (
                take_str(&mut args, &["old_string", "old_text"]),
                take_str(&mut args, &["new_string", "new_text"]),
            ) {
                let mut diff = Vec::new();
                if let Some(path) = &path {
                    diff.push(format!("--- {path}"));
                    diff.push(format!("+++ {path}"));
                }
                diff.extend(super::line_diff(&old, &new));
                lines.extend(diff_lines(diff.into_iter(), theme));
            } else if let Some(path) = path {
                lines.push(Line::from(Span::styled(path, theme.tool)));
            }
        }
        super::ToolVerb::Write => {
            if let Some(path) = take_str(&mut args, &["file_path", "path"]) {
                lines.push(Line::from(Span::styled(path, theme.tool)));
            }
            if let Some(content) = take_str(&mut args, &["content", "contents", "text"]) {
                let all: Vec<&str> = content.lines().collect();
                for line in all.iter().take(WRITE_HEAD_LINES) {
                    lines.push(Line::from(Span::styled(
                        format!("+{line}"),
                        theme.diff_added,
                    )));
                }
                if all.len() > WRITE_HEAD_LINES {
                    lines.push(Line::from(Span::styled(
                        format!("… {} more lines", all.len() - WRITE_HEAD_LINES),
                        theme.chrome_dim,
                    )));
                }
            }
        }
        _ => {}
    }
    push_key_values(&mut lines, &args, 0, theme);
    lines
}

/// `key: value` rows for JSON arguments; nested values indent by two.
fn push_key_values(
    lines: &mut Vec<Line<'static>>,
    args: &serde_json::Map<String, serde_json::Value>,
    depth: usize,
    theme: &Theme,
) {
    let indent = "  ".repeat(depth);
    for (key, value) in args {
        match value {
            serde_json::Value::Object(map) if !map.is_empty() => {
                lines.push(Line::from(Span::styled(
                    format!("{indent}{key}:"),
                    theme.tool_label,
                )));
                push_key_values(lines, map, depth + 1, theme);
            }
            serde_json::Value::Array(items)
                if items.iter().any(|item| item.is_object() || item.is_array()) =>
            {
                lines.push(Line::from(Span::styled(
                    format!("{indent}{key}:"),
                    theme.tool_label,
                )));
                for item in items {
                    match item {
                        serde_json::Value::Object(map) => {
                            lines.push(Line::from(Span::styled(
                                format!("{indent}  -"),
                                theme.tool_label,
                            )));
                            push_key_values(lines, map, depth + 2, theme);
                        }
                        other => lines.push(Line::from(vec![
                            Span::styled(format!("{indent}  - "), theme.tool_label),
                            Span::styled(scalar_text(other), theme.tool),
                        ])),
                    }
                }
            }
            serde_json::Value::String(text) if text.contains('\n') => {
                lines.push(Line::from(Span::styled(
                    format!("{indent}{key}:"),
                    theme.tool_label,
                )));
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("{indent}  {line}"),
                        theme.tool,
                    )));
                }
            }
            other => lines.push(Line::from(vec![
                Span::styled(format!("{indent}{key}: "), theme.tool_label),
                Span::styled(scalar_text(other), theme.tool),
            ])),
        }
    }
}

fn scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => {
            items.iter().map(scalar_text).collect::<Vec<_>>().join(", ")
        }
        other => other.to_string(),
    }
}

/// A full command, multi-line, coloured like its item row.
/// A command's detail rows: highlighted as one unit, so quoting and
/// heredocs carry across lines, with `$` and the program accent on the
/// first row only.
fn command_lines(command: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows =
        crate::tui::highlight::lines(command, Some("bash"), &command_styles(theme)).into_iter();
    let first = rows.next().unwrap_or_default();
    let first_text = command.lines().next().unwrap_or_default();
    std::iter::once(Line::from(prompted_command(first, first_text, theme)))
        .chain(rows.map(Line::from))
        .collect()
}

/// Unified-diff rows in the diff colours.
fn diff_lines(rows: impl Iterator<Item = String>, theme: &Theme) -> Vec<Line<'static>> {
    rows.map(|row| {
        let style = if row.starts_with("+++")
            || row.starts_with("---")
            || row.starts_with("@@")
            || row.starts_with("***")
        {
            theme.diff_hunk
        } else if row.starts_with('+') {
            theme.diff_added
        } else if row.starts_with('-') {
            theme.diff_removed
        } else {
            theme.tool
        };
        Line::from(Span::styled(row, style))
    })
    .collect()
}

/// Fewest cells the subject keeps before the meta is dropped (§5.4).
const MIN_SUBJECT_CELLS: usize = 12;

/// One item row, never wrapped: `prefix` (rail and glyph), the label cut
/// with `…`, right-aligned `meta` parts, then `caret`. At narrow widths
/// meta parts drop from the right first so the label keeps `label_floor`
/// cells (usually [`MIN_SUBJECT_CELLS`]), except the first `kept_meta`,
/// which stay while the label is cut further; the caret always stays.
fn item_row(
    prefix: Vec<Span<'static>>,
    label: Vec<Span<'static>>,
    suffix: Vec<Span<'static>>,
    meta: &[&str],
    (label_floor, kept_meta): (usize, usize),
    caret: &str,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let spans_width =
        |spans: &[Span<'static>]| -> usize { spans.iter().map(|span| span.content.width()).sum() };
    let prefix_width = spans_width(&prefix);
    let caret_width = if caret.is_empty() {
        0
    } else {
        1 + caret.width()
    };
    // The suffix (a failure count) is never cut; the label gives way.
    let room = width.saturating_sub(prefix_width + caret_width + spans_width(&suffix));
    let label_width = spans_width(&label);
    let mut shown = meta.len();
    let meta_text = loop {
        let text = meta[..shown].join(" · ");
        let needed = if text.is_empty() { 0 } else { text.width() + 2 };
        if shown <= kept_meta || label_width + needed <= room || room >= needed + label_floor {
            break text;
        }
        shown -= 1;
    };
    let meta_width = if meta_text.is_empty() {
        0
    } else {
        meta_text.width() + 2
    };
    let label = truncate_spans(label, room.saturating_sub(meta_width));
    let used = prefix_width + spans_width(&label) + spans_width(&suffix);
    let mut spans = prefix;
    spans.extend(label);
    spans.extend(suffix);
    let tail_width = meta_width.saturating_sub(2).saturating_add(caret_width);
    let padding = width.saturating_sub(used + tail_width);
    if padding > 0 && (tail_width > 0) {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    if meta_text.trim().is_empty() {
        // A tip's reserved slot paints like padding.
        spans.push(Span::raw(meta_text));
    } else {
        spans.push(Span::styled(meta_text, theme.tool_label));
    }
    if !caret.is_empty() {
        spans.push(Span::styled(format!(" {caret}"), theme.tool_label));
    }
    Line::from(spans)
}

/// `spans` cut to `budget` cells, ending `…` when anything was cut.
fn truncate_spans(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|span| span.content.width()).sum();
    if total <= budget {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    let keep = budget.saturating_sub(1);
    let mut last_style = Style::default();
    'spans: for span in spans {
        last_style = span.style;
        let mut kept = String::new();
        for grapheme in span.content.graphemes(true) {
            let cells = grapheme.width();
            if used + cells > keep {
                if !kept.is_empty() {
                    out.push(Span::styled(kept, span.style));
                }
                break 'spans;
            }
            used += cells;
            kept.push_str(grapheme);
        }
        out.push(Span::styled(kept, span.style));
    }
    if budget > 0 {
        out.push(Span::styled("…", last_style));
    }
    out
}

fn command_styles(theme: &Theme) -> crate::tui::highlight::HighlightStyles {
    crate::tui::highlight::HighlightStyles {
        default: theme.command_arg,
        comment: theme.command_arg,
        string: theme.command_string,
        constant: theme.command_arg,
        keyword: theme.command_arg,
        function: theme.command_arg,
        type_name: theme.command_arg,
        parameter: theme.command_arg,
        invalid: theme.command_arg,
    }
}

/// A one-line command coloured with syntect's bash grammar: `$` muted,
/// the program in the accent, arguments in body text, quoted strings in
/// the string colour (§5.4).
fn command_spans(command: &str, theme: &Theme) -> Vec<Span<'static>> {
    let highlighted = crate::tui::highlight::lines(command, Some("bash"), &command_styles(theme))
        .into_iter()
        .next()
        .unwrap_or_default();
    prompted_command(highlighted, command, theme)
}

/// `$ ` and the highlighted first row of a command, its first token (the
/// program) in the accent.
fn prompted_command(
    highlighted: Vec<Span<'static>>,
    command: &str,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let program_start = command.len() - command.trim_start().len();
    let program_end = command[program_start..]
        .find(char::is_whitespace)
        .map_or(command.len(), |end| program_start + end);
    let mut spans = vec![
        Span::styled("$", theme.command_prompt),
        Span::styled(" ", theme.command_arg),
    ];
    let mut offset = 0;
    for span in highlighted {
        let text = span.content.into_owned();
        let (from, to) = (offset, offset + text.len());
        offset = to;
        // Only plain argument text in the program's range takes the accent;
        // a quoted program keeps the string colour.
        if span.style != theme.command_arg || to <= program_start || from >= program_end {
            spans.push(Span::styled(text, span.style));
            continue;
        }
        let cut_start = program_start.max(from) - from;
        let cut_end = program_end.min(to) - from;
        if cut_start > 0 {
            spans.push(Span::styled(text[..cut_start].to_owned(), span.style));
        }
        spans.push(Span::styled(
            text[cut_start..cut_end].to_owned(),
            theme.command_program,
        ));
        if cut_end < text.len() {
            spans.push(Span::styled(text[cut_end..].to_owned(), span.style));
        }
    }
    spans
}

/// Project `blocks`, measuring exactly only the blocks named by `exact`.
///
/// `None` measures everything. Otherwise every other block contributes
/// [`estimate_block_rows`] blank rows: they hold geometry so the scroll
/// bar and the anchor have a transcript to address, and the paint path
/// promotes to an exact measurement everything the viewport reads, so a
/// blank placeholder is never drawn.
#[must_use]
fn render_transcript_windowed(
    blocks: &[ScrollBlock],
    theme: &Theme,
    content_width: usize,
    prefs: super::RenderPrefs,
    plans: &[super::RowPlan],
    exact: Option<&std::collections::BTreeSet<usize>>,
) -> TranscriptRender {
    let mut lines = Vec::new();
    let mut plain = Vec::new();
    let mut block_index = Vec::new();
    let mut copy = Vec::new();
    let mut estimated = std::collections::BTreeSet::new();
    let mut previous_drawn: Option<&ScrollBlock> = None;
    for bi in super::row_order(plans) {
        let block = &blocks[bi];
        let separated = super::wants_separator(previous_drawn, block);
        if exact.is_none_or(|exact| exact.contains(&bi)) {
            let (styled, row_copy) =
                block_rows(block, &plans[bi], theme, content_width, prefs, blocks);
            // Hidden system noise intentionally contributes no visual rows.
            if styled.is_empty() {
                continue;
            }
            if separated {
                lines.push(Line::from(""));
                plain.push(String::new());
                block_index.push(None);
                copy.push(RowCopy::default());
            }
            let block_plain = styled
                .iter()
                .map(|line| selectable_row(block, line))
                .collect::<Vec<_>>();
            block_index.extend(std::iter::repeat_n(Some(bi), styled.len()));
            lines.extend(styled);
            plain.extend(block_plain);
            copy.extend(row_copy);
            previous_drawn = Some(block);
            continue;
        }
        let rows = estimate_block_rows(block, &plans[bi], content_width, prefs, blocks);
        if rows == 0 {
            continue;
        }
        previous_drawn = Some(block);
        if separated {
            lines.push(Line::from(""));
            plain.push(String::new());
            block_index.push(None);
            copy.push(RowCopy::default());
        }
        estimated.insert(bi);
        block_index.extend(std::iter::repeat_n(Some(bi), rows));
        lines.extend(std::iter::repeat_n(Line::default(), rows));
        plain.extend(std::iter::repeat_n(String::new(), rows));
        copy.extend(std::iter::repeat_n(RowCopy::default(), rows));
    }
    TranscriptRender {
        lines,
        layout: LineLayout {
            plain,
            block_index,
            estimated,
            copy,
        },
    }
}
