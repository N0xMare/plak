//! Mouse drag-selection over the transcript's complete rendered projection.
//!
//! Coordinates are visual row/cell offsets in the cached plain projection,
//! never terminal-buffer coordinates.  That keeps an anchor stable while the
//! viewport scrolls or redraws and lets OSC52 copy include off-screen rows.

use crate::tui::blocks::{LineLayout, layout_anchor, resolve_layout_anchor};
use crate::tui::markdown::RowCopy;
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use sha2::{Digest as _, Sha256};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// One absolute position in the width-sensitive rendered transcript.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ContentPosition {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

/// Transcript text selection state.
#[derive(Default)]
pub(crate) struct TranscriptSelection {
    anchor: Option<ContentPosition>,
    head: Option<ContentPosition>,
    block_anchor: Option<(usize, usize)>,
    block_head: Option<(usize, usize)>,
    projection_width: Option<usize>,
    projection_revision: Option<u64>,
    restore_projection: Option<[u8; 32]>,
}

impl TranscriptSelection {
    /// A key match alone does not prove the same rendered text: grouping can
    /// change a row's label even when its own content did not change.
    pub(crate) fn prepare_keyed_restore(&mut self, layout: &LineLayout) {
        if self.anchor.is_none() || self.restore_projection.is_some() {
            return;
        }
        if self.block_anchor.is_none() || self.head.is_some() && self.block_head.is_none() {
            self.capture_blocks(layout);
        }
        if self.block_anchor.is_none() || self.head.is_some() && self.block_head.is_none() {
            self.clear();
            return;
        }
        self.restore_projection = self.projection_digest(layout);
        if self.restore_projection.is_none() {
            self.clear();
        }
    }

    fn projection_digest(&self, layout: &LineLayout) -> Option<[u8; 32]> {
        let anchor = self.anchor?;
        let head = self.head.unwrap_or(anchor);
        let rows = layout
            .plain
            .get(anchor.row.min(head.row)..=anchor.row.max(head.row))?;
        let mut digest = Sha256::new();
        for row in rows {
            digest.update((row.len() as u64).to_le_bytes());
            digest.update(row.as_bytes());
        }
        Some(digest.finalize().into())
    }

    pub(crate) fn invalidate_block(&mut self, block: usize) {
        if self.block_span().is_some_and(|span| span.contains(&block)) {
            self.clear();
        }
    }

    pub(crate) fn block_span(&self) -> Option<std::ops::RangeInclusive<usize>> {
        let (anchor, _) = self.block_anchor?;
        let (head, _) = self.block_head.unwrap_or((anchor, 0));
        Some(anchor.min(head)..=anchor.max(head))
    }

    pub(crate) fn remap_blocks(&mut self, remap: impl Fn(usize) -> Option<usize>) {
        if self.anchor.is_some() && self.block_anchor.is_none()
            || self.head.is_some() && self.block_head.is_none()
        {
            self.clear();
            return;
        }
        if let Some(span) = self.block_span() {
            let Some(first) = remap(*span.start()) else {
                self.clear();
                return;
            };
            if span.enumerate().any(|(offset, block)| {
                first
                    .checked_add(offset)
                    .is_none_or(|expected| remap(block) != Some(expected))
            }) {
                self.clear();
                return;
            }
        }
        let map = |anchor: Option<(usize, usize)>| {
            anchor.and_then(|(block, row)| remap(block).map(|block| (block, row)))
        };
        let anchor = map(self.block_anchor);
        let head = map(self.block_head);
        if self.block_anchor.is_some() && anchor.is_none()
            || self.block_head.is_some() && head.is_none()
        {
            self.clear();
            return;
        }
        self.block_anchor = anchor;
        self.block_head = head;
        self.projection_revision = None;
    }

    pub(crate) fn capture_blocks(&mut self, layout: &LineLayout) {
        self.block_anchor = self
            .anchor
            .and_then(|position| layout_anchor(layout, position.row));
        self.block_head = self
            .head
            .and_then(|position| layout_anchor(layout, position.row));
    }

    pub(crate) fn resolve_blocks(&mut self, layout: &LineLayout) {
        if let Some(position) = &mut self.anchor
            && let Some(anchor) = self.block_anchor
        {
            position.row = resolve_layout_anchor(layout, anchor);
        }
        if let Some(position) = &mut self.head
            && let Some(anchor) = self.block_head
        {
            position.row = resolve_layout_anchor(layout, anchor);
        }
        let digest = self.projection_digest(layout);
        if self
            .restore_projection
            .is_some_and(|guard| digest != Some(guard))
        {
            self.clear();
        } else {
            self.restore_projection = digest;
        }
    }

    /// Begin a potential drag in absolute rendered-content coordinates.
    pub(crate) fn begin(&mut self, position: ContentPosition, width: usize, revision: u64) {
        self.anchor = Some(position);
        self.head = None;
        self.block_anchor = None;
        self.block_head = None;
        self.projection_width = Some(width);
        self.projection_revision = Some(revision);
        self.restore_projection = None;
    }

    /// Update the drag head; returns true while a gesture is owned.
    pub(crate) fn drag(&mut self, position: ContentPosition) -> bool {
        if self.anchor.is_none() {
            return false;
        }
        if self.head != Some(position) {
            self.restore_projection = None;
        }
        self.head = Some(position);
        true
    }

    /// Finish the gesture. A click without movement is not a selection.
    pub(crate) fn finish(&mut self, position: ContentPosition) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        if self.head.is_none() && position == anchor {
            self.clear();
            return false;
        }
        if self.head != Some(position) {
            self.restore_projection = None;
        }
        self.head = Some(position);
        true
    }

    #[cfg(test)]
    pub(crate) const fn is_dragging(&self) -> bool {
        self.anchor.is_some()
    }

    pub(crate) const fn has_range(&self) -> bool {
        self.anchor.is_some() && self.head.is_some()
    }

    /// Preserve only a selection proven to address the same projection. A tail
    /// splice may retain positions wholly inside its unchanged prefix; every
    /// replacement without a proven key remap, or a reflow, invalidates
    /// absolute row coordinates.
    pub(crate) fn reconcile_projection(
        &mut self,
        revision: u64,
        width: usize,
        rows: usize,
        stable_prefix_rows: Option<usize>,
    ) {
        if self.projection_width.is_some_and(|prior| prior != width) || rows == 0 {
            self.clear();
            return;
        }
        if self
            .projection_revision
            .is_some_and(|prior| prior != revision)
        {
            let stable = stable_prefix_rows.is_some_and(|prefix| {
                self.anchor.is_some_and(|position| position.row < prefix)
                    && self.head.is_none_or(|position| position.row < prefix)
            });
            if !stable {
                self.clear();
                return;
            }
            self.projection_revision = Some(revision);
        }
        let clamp = |mut position: ContentPosition| {
            position.row = position.row.min(rows.saturating_sub(1));
            position
        };
        self.anchor = self.anchor.map(clamp);
        self.head = self.head.map(clamp);
        if self.anchor.is_some() {
            self.projection_revision = Some(revision);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.block_anchor = None;
        self.block_head = None;
        self.projection_width = None;
        self.projection_revision = None;
        self.restore_projection = None;
    }

    /// Highlight the cells [`Self::take_text`] will copy on the visible
    /// transcript body. Sticky preview rows are intentionally outside this
    /// mapping. Columns come from the same row bounds as the copy: a leading
    /// decoration, an omitted language label, and cells past the plain row
    /// are not painted.
    pub(crate) fn render(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        visible_start: usize,
        lines: &[String],
        copy: &[RowCopy],
        style: Style,
    ) {
        let Some((start, end)) = self.ordered() else {
            return;
        };
        let max_column = usize::from(area.width).saturating_sub(1);
        for screen_row in 0..usize::from(area.height) {
            let content_row = visible_start.saturating_add(screen_row);
            if content_row < start.row || content_row > end.row {
                continue;
            }
            let Some(line) = lines.get(content_row) else {
                continue;
            };
            let first = if content_row == start.row {
                start.column
            } else {
                0
            };
            let last = if content_row == end.row {
                end.column
            } else {
                line.width().saturating_sub(1)
            };
            let Some((from, to)) = row_bounds(line, copy.get(content_row), first, last).cells
            else {
                continue;
            };
            for column in from..=to.min(max_column) {
                let x = area
                    .x
                    .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
                let y = area
                    .y
                    .saturating_add(u16::try_from(screen_row).unwrap_or(u16::MAX));
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_style(style);
                }
            }
        }
    }

    /// Copy from the complete cached plain projection, including off-screen
    /// rows. Endpoint columns are inclusive terminal-cell coordinates.
    ///
    /// `copy` is parallel to `lines`: leading decoration (the transcript block
    /// indent plus a code or quote rail) is dropped when the selection starts
    /// at or before it. A fenced or indented source line copies
    /// `RowCopy::code` once when the selection covers every visual row of that
    /// line through the end of the last one. Continuation rows of a line
    /// already copied that way are skipped. A partial selection that crosses
    /// those wrapped rows appends the covered cells to the previous painted
    /// slice, so one source line stays one copied line. A language-label row
    /// covered through its end is omitted. `full` names the untruncated text of
    /// a one-line tool row (spec 02 §5.4) and is used only when the row has no
    /// code source of its own.
    pub(crate) fn take_text(
        &mut self,
        revision: u64,
        lines: &[String],
        copy: &[RowCopy],
        full: &mut dyn FnMut(usize) -> Option<String>,
    ) -> Option<String> {
        if self.projection_revision != Some(revision) {
            self.clear();
            return None;
        }
        let (start, end) = self.ordered()?;
        let mut selected = Vec::new();
        let last_row = end.row.min(lines.len().saturating_sub(1));
        let mut skip_through = None;
        // The previous emitted row was a painted slice, not a whole source
        // line. A soft-wrap continuation joins that slice instead of starting
        // a new copied line.
        let mut painted_open = false;
        for (row, line) in lines
            .iter()
            .enumerate()
            .take(last_row.saturating_add(1))
            .skip(start.row)
        {
            if skip_through.is_some_and(|end_row| row <= end_row) {
                painted_open = false;
                continue;
            }
            skip_through = None;
            let first = if row == start.row { start.column } else { 0 };
            let last = if row == end.row {
                end.column
            } else {
                line.width().saturating_sub(1)
            };
            let meta = copy.get(row);
            let bounds = row_bounds(line, meta, first, last);
            if bounds.omitted {
                painted_open = false;
                continue;
            }
            if let Some(source) = meta.and_then(|meta| meta.code.clone()) {
                let group_end = code_group_end(copy, row);
                if first <= bounds.decoration && selection_reaches(lines, end, group_end) {
                    selected.push(source);
                    skip_through = Some(group_end);
                    painted_open = false;
                    continue;
                }
            }
            let code_visual =
                meta.is_some_and(|meta| meta.code.is_some() || meta.code_continuation);
            if bounds.whole
                && !code_visual
                && let Some(text) = full(row)
            {
                selected.push(text);
                painted_open = false;
                continue;
            }
            // Drop the decoration when the selection starts inside it. A
            // selection that starts in the content copies the cells it covers.
            if bounds.origin > last {
                // A blank row, or one whose visible text is only decoration,
                // still contributes its line when the selection covers it.
                if bounds.whole && line.width() <= bounds.decoration {
                    selected.push(String::new());
                }
                painted_open = false;
                continue;
            }
            let slice = match bounds.cells {
                Some((from, to)) => slice_cells(line, from, to),
                None => String::new(),
            };
            let join =
                painted_open && row > start.row && meta.is_some_and(|meta| meta.code_continuation);
            if join && let Some(last) = selected.last_mut() {
                last.push_str(&slice);
            } else {
                selected.push(slice);
            }
            painted_open = true;
        }
        self.clear();
        let text = selected.join("\n");
        (!text.is_empty()).then_some(text)
    }

    #[cfg(test)]
    pub(crate) fn endpoints_for_test(&self) -> Option<(ContentPosition, ContentPosition)> {
        Some((self.anchor?, self.head?))
    }

    fn ordered(&self) -> Option<(ContentPosition, ContentPosition)> {
        let anchor = self.anchor?;
        let head = self.head?;
        Some(if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }
}

/// Which cells of one plain row a selection copies, and the flags the
/// copy string still needs.
///
/// `cells` is the inclusive column range of glyphs both the highlight and
/// the cell-slice copy use. A whole code line or one-line tool row may
/// substitute its source or full text for that slice; the highlight still
/// paints these columns, which are the painted form of that text. `None`
/// means the row contributes no glyphs (omitted language label, a selection
/// that never leaves the decoration, or columns past the plain row such as
/// a prompt's send time).
struct RowBounds {
    decoration: usize,
    /// The selection covers this row from its decoration through its last cell.
    whole: bool,
    /// A language label covered through its end. Copy skips the row.
    omitted: bool,
    /// First cell the slice would read. May sit past `last` or past the line.
    origin: usize,
    cells: Option<(usize, usize)>,
}

fn row_bounds(line: &str, meta: Option<&RowCopy>, first: usize, last: usize) -> RowBounds {
    let width = line.width();
    let decoration = usize::from(meta.map(|meta| meta.decoration).unwrap_or(0));
    // A row that is not the selection's end uses `last == width - 1` (or 0
    // when the row is empty), so covering that last cell means through the end.
    let through_end = last.saturating_add(1) >= width;
    let whole = first <= decoration && through_end;
    let omitted = whole && meta.is_some_and(|meta| meta.language);
    let origin = if first <= decoration {
        decoration
    } else {
        first
    };
    let cells = if omitted || width == 0 || origin > last || origin >= width {
        None
    } else {
        let end = last.min(width.saturating_sub(1));
        (origin <= end).then_some((origin, end))
    };
    RowBounds {
        decoration,
        whole,
        omitted,
        origin,
        cells,
    }
}

/// Last visual row of a soft-wrapped code line that starts at `row`.
fn code_group_end(copy: &[RowCopy], row: usize) -> usize {
    let mut end = row;
    while copy
        .get(end.saturating_add(1))
        .is_some_and(|meta| meta.code_continuation)
    {
        end = end.saturating_add(1);
    }
    end
}

/// Whether `end` covers `group_end` through that row's last cell.
fn selection_reaches(lines: &[String], end: ContentPosition, group_end: usize) -> bool {
    if end.row > group_end {
        return true;
    }
    if end.row < group_end {
        return false;
    }
    let width = lines.get(group_end).map(|line| line.width()).unwrap_or(0);
    width == 0 || end.column.saturating_add(1) >= width
}

/// Include a whole grapheme when any of its occupied terminal cells intersects
/// the inclusive selection. This avoids splitting combining or wide glyphs.
fn slice_cells(line: &str, first: usize, last: usize) -> String {
    let mut column = 0usize;
    line.graphemes(true)
        .filter(|grapheme| {
            let width = grapheme.width();
            let start = column;
            let end = column.saturating_add(width.max(1)).saturating_sub(1);
            column = column.saturating_add(width);
            end >= first && start <= last
        })
        .collect()
}

#[cfg(test)]
mod tests;
