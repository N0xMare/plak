//! Mouse drag-selection over the transcript's complete rendered projection.
//!
//! Coordinates are visual row/cell offsets in the cached plain projection,
//! never terminal-buffer coordinates.  That keeps an anchor stable while the
//! viewport scrolls or redraws and lets OSC52 copy include off-screen rows.

use crate::tui::blocks::{LineLayout, RowCopy, layout_anchor, resolve_layout_anchor};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
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
}

impl TranscriptSelection {
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
    }

    /// Begin a potential drag in absolute rendered-content coordinates.
    pub(crate) fn begin(&mut self, position: ContentPosition, width: usize, revision: u64) {
        self.anchor = Some(position);
        self.head = None;
        self.block_anchor = None;
        self.block_head = None;
        self.projection_width = Some(width);
        self.projection_revision = Some(revision);
    }

    /// Update the drag head; returns true while a gesture is owned.
    pub(crate) fn drag(&mut self, position: ContentPosition) -> bool {
        if self.anchor.is_none() {
            return false;
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
    /// structural replacement or reflow invalidates absolute row coordinates.
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
    }

    pub(crate) fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.block_anchor = None;
        self.block_head = None;
        self.projection_width = None;
        self.projection_revision = None;
    }

    /// Highlight the selected part of the currently visible real transcript
    /// body. Sticky preview rows are intentionally outside this mapping.
    pub(crate) fn render(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        visible_start: usize,
        lines: &[String],
        style: Style,
    ) {
        let Some((start, end)) = self.ordered() else {
            return;
        };
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
            for column in first..=last.min(usize::from(area.width).saturating_sub(1)) {
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
    /// at or before it. A fenced or indented code row selected through its end
    /// copies `RowCopy::code`, the unclipped source. A language-label row
    /// covered the same way is omitted. `full` names the untruncated text of a
    /// one-line tool row (spec 02 §5.4) and is used only when the row has no
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
        for (row, line) in lines
            .iter()
            .enumerate()
            .take(last_row.saturating_add(1))
            .skip(start.row)
        {
            let first = if row == start.row { start.column } else { 0 };
            let last = if row == end.row {
                end.column
            } else {
                line.width().saturating_sub(1)
            };
            let meta = copy.get(row);
            let decoration = usize::from(meta.map(|meta| meta.decoration).unwrap_or(0));
            // Through the end of the visible row, and starting at or before
            // the decoration, so the whole content is covered. Decoration 0
            // keeps the old rule: the selection starts at the first cell.
            let through_end = row != end.row || last.saturating_add(1) >= line.width();
            let whole = first <= decoration && through_end;
            if whole && meta.is_some_and(|meta| meta.language) {
                continue;
            }
            if whole && let Some(source) = meta.and_then(|meta| meta.code.clone()) {
                selected.push(source);
                continue;
            }
            if whole && let Some(text) = full(row) {
                selected.push(text);
                continue;
            }
            // Drop the decoration when the selection starts inside it. A
            // selection that starts in the content copies the cells it covers.
            let start_cell = if first <= decoration {
                decoration
            } else {
                first
            };
            if start_cell > last {
                // A blank row, or one whose visible text is only decoration,
                // still contributes its line when the selection covers it.
                if whole && line.width() <= decoration {
                    selected.push(String::new());
                }
                continue;
            }
            selected.push(slice_cells(line, start_cell, last));
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
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect};

    #[test]
    fn reverse_drag_copies_offscreen_rows_from_full_projection() {
        let lines = vec!["zero".into(), "one".into(), "two".into(), "three".into()];
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 3, column: 2 }, 20, 7);
        assert!(selection.drag(ContentPosition { row: 0, column: 1 }));
        assert_eq!(
            selection
                .take_text(7, &lines, &[], &mut |_| None)
                .as_deref(),
            Some("ero\none\ntwo\nthr")
        );
    }

    #[test]
    fn wide_and_combining_graphemes_are_never_split() {
        assert_eq!(slice_cells("a界e\u{301}z", 2, 2), "界");
        assert_eq!(slice_cells("a界e\u{301}z", 3, 3), "e\u{301}");
    }

    #[test]
    fn width_change_clears_but_same_width_redraw_preserves_anchor() {
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 4, column: 2 }, 30, 1);
        selection.drag(ContentPosition { row: 5, column: 3 });
        selection.reconcile_projection(1, 30, 10, None);
        assert!(selection.ordered().is_some());
        selection.reconcile_projection(1, 29, 10, None);
        assert!(selection.ordered().is_none());
    }

    #[test]
    fn every_theme_paints_visible_selection_cells() {
        for id in crate::tui::theme::ThemeId::ALL {
            let theme = crate::tui::theme::Theme::for_id(id);
            let area = Rect::new(0, 0, 2, 1);
            let mut buffer = Buffer::empty(area);
            let mut selection = TranscriptSelection::default();
            selection.begin(ContentPosition { row: 0, column: 0 }, 2, 1);
            selection.drag(ContentPosition { row: 0, column: 1 });
            selection.render(
                &mut buffer,
                area,
                0,
                &["ok".into()],
                theme.transcript_selection,
            );
            for x in 0..2 {
                assert_eq!(Some(buffer[(x, 0)].fg), theme.transcript_selection.fg);
                assert_eq!(Some(buffer[(x, 0)].bg), theme.transcript_selection.bg);
            }
        }
    }

    #[test]
    fn revision_change_requires_a_proven_stable_prefix() {
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 2, column: 0 }, 20, 4);
        selection.drag(ContentPosition { row: 3, column: 1 });
        selection.reconcile_projection(5, 20, 8, Some(4));
        assert!(selection.ordered().is_some());

        selection.reconcile_projection(6, 20, 8, None);
        assert!(selection.ordered().is_none());

        selection.begin(ContentPosition { row: 3, column: 0 }, 20, 6);
        selection.drag(ContentPosition { row: 4, column: 1 });
        selection.reconcile_projection(7, 20, 8, Some(4));
        assert!(selection.ordered().is_none());
    }

    #[test]
    fn copy_preserves_selected_trailing_spaces_exactly() {
        let lines = vec!["safe  ".to_owned(), "tail ".to_owned()];
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 0, column: 0 }, 8, 9);
        selection.drag(ContentPosition { row: 1, column: 4 });
        assert_eq!(
            selection
                .take_text(9, &lines, &[], &mut |_| None)
                .as_deref(),
            Some("safe  \ntail ")
        );

        selection.begin(ContentPosition { row: 0, column: 4 }, 8, 10);
        selection.drag(ContentPosition { row: 0, column: 5 });
        assert_eq!(
            selection
                .take_text(10, &lines, &[], &mut |_| None)
                .as_deref(),
            Some("  ")
        );
    }

    #[test]
    fn copy_drops_leading_decoration_and_keeps_wide_graphemes() {
        // Gutter plus a code rail, then a wide character and a combining mark.
        let lines = vec!["  │ 界e\u{301}".to_owned(), "  │ next".to_owned()];
        let copy = vec![
            RowCopy {
                decoration: 4,
                code: Some("界e\u{301}".to_owned()),
                language: false,
            },
            RowCopy {
                decoration: 4,
                code: Some("next".to_owned()),
                language: false,
            },
        ];
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 0, column: 0 }, 20, 1);
        selection.drag(ContentPosition { row: 1, column: 10 });
        assert_eq!(
            selection
                .take_text(1, &lines, &copy, &mut |_| None)
                .as_deref(),
            Some("界e\u{301}\nnext")
        );

        // A selection that starts on the wide character keeps it whole, and
        // does not expand to a clipped source.
        selection.begin(ContentPosition { row: 0, column: 4 }, 20, 2);
        selection.drag(ContentPosition { row: 0, column: 5 });
        assert_eq!(
            selection
                .take_text(2, &lines, &copy, &mut |_| None)
                .as_deref(),
            Some("界")
        );

        // Starting inside the content leaves the cells alone.
        selection.begin(ContentPosition { row: 0, column: 6 }, 20, 3);
        selection.drag(ContentPosition { row: 0, column: 8 });
        assert_eq!(
            selection
                .take_text(3, &lines, &copy, &mut |_| None)
                .as_deref(),
            Some("e\u{301}")
        );
    }

    #[test]
    fn copy_omits_a_language_label_covered_through_its_end() {
        let lines = vec!["  ┌ bash".to_owned(), "  │ echo".to_owned()];
        let copy = vec![
            RowCopy {
                decoration: 4,
                code: None,
                language: true,
            },
            RowCopy {
                decoration: 4,
                code: Some("echo".to_owned()),
                language: false,
            },
        ];
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 0, column: 0 }, 20, 1);
        selection.drag(ContentPosition { row: 1, column: 10 });
        assert_eq!(
            selection
                .take_text(1, &lines, &copy, &mut |_| None)
                .as_deref(),
            Some("echo")
        );

        // A selection inside the label still copies those cells.
        selection.begin(ContentPosition { row: 0, column: 4 }, 20, 2);
        selection.drag(ContentPosition { row: 0, column: 6 });
        assert_eq!(
            selection
                .take_text(2, &lines, &copy, &mut |_| None)
                .as_deref(),
            Some("bas")
        );
    }
}
