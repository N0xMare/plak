//! The transcript render cache: exact rows for a window of blocks over
//! estimated ones, its byte budget, and the settled assistant tail.

use ratatui::text::Line;

use super::estimate::estimate_block_rows;
use super::{
    assistant_markdown_theme, assistant_markdown_width, assistant_rows, block_rows, plain_row,
    selectable_row,
};
use crate::tui::blocks::{
    LineLayout, RenderPrefs, RowPlan, ScrollBlock, block_span, plan_rows, row_order,
    wants_separator,
};
use crate::tui::markdown::RowCopy;
use crate::tui::theme::Theme;

/// Width-sensitive, fully styled transcript projection.
#[derive(Clone, Debug, Default)]
pub struct TranscriptRender {
    /// Styled visual lines, ready for viewport slicing.
    pub lines: Vec<Line<'static>>,
    /// Plain visual lines and their source blocks.
    pub layout: LineLayout,
    #[cfg(test)]
    measurements: usize,
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

/// Width/theme cache with a safe trailing-assistant splice path.
///
/// Bounded by [`TRANSCRIPT_RENDER_CACHE_BYTES`]; see that constant for the
/// eviction rule.
#[derive(Clone, Debug, Default)]
pub struct TranscriptRenderCache {
    revision: Option<u64>,
    plans_revision: Option<u64>,
    plans: Vec<RowPlan>,
    width: usize,
    theme: Option<Theme>,
    prefs: RenderPrefs,
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
    /// Every block is measured exactly for a copy action.
    exact_all: bool,
    /// Selection geometry stays exact even while the viewport is elsewhere.
    required_blocks: Option<std::ops::RangeInclusive<usize>>,
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
    #[cfg(test)]
    measurements: usize,
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

    pub(crate) fn row_plans(&self) -> &[RowPlan] {
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
        prefs: RenderPrefs,
    ) -> &TranscriptRender {
        if self.plans_revision != Some(revision) || self.prefs != prefs {
            self.plans = plan_rows(blocks, prefs);
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
                    self.measurements += self.render.measurements;
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

    pub(crate) fn require_blocks(&mut self, span: Option<std::ops::RangeInclusive<usize>>) {
        self.exact_all = false;
        self.required_blocks = span;
    }

    /// Measure only selected blocks; the rest keeps its windowed estimates.
    /// Evicted text is projected temporarily without exempting it from the
    /// ordinary cache byte ceiling. The selection retains its digest, not text.
    pub(crate) fn materialize_required(&mut self, blocks: &[ScrollBlock]) {
        let Some(span) = self.required_blocks.clone() else {
            return;
        };
        if self
            .render
            .layout
            .estimated
            .range(span.clone())
            .next()
            .is_some()
        {
            self.rebuild(blocks);
        }
        let wanted: Vec<_> = self.evicted.range(span).copied().collect();
        self.reproject(blocks, &wanted);
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
        let Some((start, end)) = block_span(&self.render.layout, block) else {
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
        if let Some(span) = &self.required_blocks {
            exact.extend(span.clone());
        }
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
            self.measurements += self.render.measurements;
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
    /// Transcript copy reads the complete plain projection, without estimated
    /// geometry or blanks eviction leaves behind. Windowed rebuilding resumes
    /// when the face next configures its selected span; the byte ceiling
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

    /// Actual block formatter invocations, excluding estimated placeholders.
    #[cfg(test)]
    pub const fn measurements(&self) -> usize {
        self.measurements
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
            #[cfg(test)]
            {
                self.measurements += 1;
            }
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
        prefs: RenderPrefs,
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
        #[cfg(test)]
        {
            self.measurements += 1;
        }
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

    #[allow(
        clippy::too_many_lines,
        reason = "Tail splicing updates cached rows, block indexes, and scroll geometry together so partial redraws stay coherent."
    )]
    fn splice_tail(
        &mut self,
        blocks: &[ScrollBlock],
        theme: &Theme,
        content_width: usize,
        prefs: RenderPrefs,
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
                &RowPlan::default(),
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
    prefs: RenderPrefs,
    plans: &[RowPlan],
    exact: Option<&std::collections::BTreeSet<usize>>,
) -> TranscriptRender {
    let mut lines = Vec::new();
    let mut plain = Vec::new();
    let mut block_index = Vec::new();
    let mut copy = Vec::new();
    let mut estimated = std::collections::BTreeSet::new();
    let mut previous_drawn: Option<&ScrollBlock> = None;
    #[cfg(test)]
    let mut measurements = 0;
    for bi in row_order(plans) {
        let block = &blocks[bi];
        let separated = wants_separator(previous_drawn, block);
        if exact.is_none_or(|exact| exact.contains(&bi)) {
            #[cfg(test)]
            {
                measurements += 1;
            }
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
        #[cfg(test)]
        measurements,
        layout: LineLayout {
            plain,
            block_index,
            estimated,
            copy,
        },
    }
}
