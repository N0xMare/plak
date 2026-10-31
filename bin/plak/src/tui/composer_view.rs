//! Floating chat-block geometry and chrome labels.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::blocks::wrap_plain_rows;

/// Minimum draft content rows inside the floating block.
pub const MIN_CONTENT_ROWS: u16 = 2;

/// Format occupancy as `n%/window`.
///
/// `window` is the model's total context window the percent is measured
/// against (272k on Codex luna/sol/terra, 1M on Claude, 500k on Grok), so the
/// meter and the `/usage` Context header agree. Occupancy is the Plakous
/// context measure: the last provider `input_tokens` plus `part_bytes÷4` of
/// the suffix appended since, or `part_bytes÷4` of the prepared body
/// unanchored; it is not the admission bound. The admitted input ceiling below that window is never
/// the denominator. Without a window the meter reads
/// `awaiting context`; with a window and no occupancy yet it reads `0%/…`.
#[must_use]
pub fn format_context_meter(display_percent: Option<u128>, window: Option<u64>) -> String {
    match (display_percent, window) {
        (_, Some(window)) if window > 0 => {
            format!(
                "{}%/{}",
                display_percent.unwrap_or(0),
                format_window_label(window)
            )
        }
        _ => "awaiting context".to_owned(),
    }
}

/// Compact window label without discarding meaningful sub-thousand precision.
#[must_use]
pub fn format_window_label(window: u64) -> String {
    if window >= 1_000_000 {
        format!("{}M", window / 1_000_000)
    } else if window >= 1000 {
        let whole = window / 1000;
        let tenths = (window % 1000) / 100;
        if tenths == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{tenths}k")
        }
    } else {
        window.to_string()
    }
}

/// Count terminal-cell visual wrap lines for a Unicode draft.
#[cfg(test)]
#[must_use]
pub fn visual_line_count(text: &str, width: usize) -> usize {
    visual_rows(text, width).len().max(1)
}

/// Floating block height for `rows` of wrapped draft, capped near 35% of
/// the terminal. An empty draft keeps two content rows plus the borders.
#[must_use]
pub fn block_height_for_rows(term_h: u16, rows: usize) -> u16 {
    let max_total = term_h.saturating_mul(35).div_ceil(100).max(4);
    let max_content = max_total.saturating_sub(2).max(MIN_CONTENT_ROWS);
    let content = u16::try_from(rows.max(1))
        .unwrap_or(u16::MAX)
        .clamp(MIN_CONTENT_ROWS, max_content);
    content.saturating_add(2)
}

/// Desired floating block height for terminal `term_h` and content width.
///
/// Max total height is approximately **35%** of the terminal. An empty or
/// single-line draft retains two content rows plus the two border rows.
#[cfg(test)]
#[must_use]
pub fn desired_block_height(term_h: u16, content_width: u16, draft: &str) -> u16 {
    let wrap_w = usize::from(content_width.max(1));
    block_height_for_rows(term_h, visual_line_count(draft, wrap_w))
}

/// Split draft into visual lines for rendering (width in cells).
#[cfg(test)]
#[must_use]
pub fn wrap_draft_lines(text: &str, width: usize) -> Vec<String> {
    visual_rows(text, width)
        .into_iter()
        .map(|(_, _, s)| s)
        .collect()
}

#[cfg(test)]
thread_local! {
    static VISUAL_ROW_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Layouts of `visual_rows` on this thread since the last take.
///
/// Counts the function itself, so a frame that wraps by calling it again
/// instead of the draft cache fails the one-layout assertion.
#[cfg(test)]
pub(crate) fn take_visual_row_calls() -> usize {
    VISUAL_ROW_CALLS.with(|calls| calls.replace(0))
}

/// Visual rows as `(start_char_idx, end_char_idx, line_text)`.
///
/// Indices are into the **char** sequence of `text` (not bytes). End is exclusive.
/// Soft-wrap rows do not consume a newline; hard-break rows end just before `\n`.
#[must_use]
pub fn visual_rows(text: &str, width: usize) -> Vec<(usize, usize, String)> {
    #[cfg(test)]
    VISUAL_ROW_CALLS.with(|calls| {
        calls.set(calls.get().saturating_add(1));
    });
    let width = width.max(1);
    let mut out: Vec<_> = wrap_plain_rows(text, width)
        .into_iter()
        .map(|row| (row.start, row.end, row.text))
        .collect();
    if let Some((_, end, last)) = out.last()
        && *end == text.chars().count()
        && !text.ends_with('\n')
        && UnicodeWidthStr::width(last.as_str()) >= width
    {
        out.push((*end, *end, String::new()));
    }
    out
}

/// Cached wrap of one composer draft. Reused across drag events while the
/// text and wrap width are unchanged.
#[derive(Clone, Debug, Default)]
pub struct DraftWrapCache {
    width: usize,
    text: String,
    rows: Vec<(usize, usize, String)>,
}

impl DraftWrapCache {
    /// Rows for `text` at `width`, recomputing only when either changes.
    pub fn rows(&mut self, text: &str, width: usize) -> &[(usize, usize, String)] {
        let width = width.max(1);
        if self.width != width || self.text != text {
            self.width = width;
            text.clone_into(&mut self.text);
            self.rows = visual_rows(text, width);
        }
        &self.rows
    }
}

/// Char index of the cursor for a click on visual row `row`, content col `col`.
#[cfg(test)]
#[must_use]
pub fn cursor_from_visual_click(text: &str, width: usize, row: usize, col: usize) -> usize {
    cursor_from_visual_rows(&visual_rows(text, width), text.chars().count(), row, col)
}

/// Click hit-test against already-wrapped composer rows.
#[must_use]
pub fn cursor_from_visual_rows(
    rows: &[(usize, usize, String)],
    text_chars: usize,
    row: usize,
    col: usize,
) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let row = row.min(rows.len() - 1);
    let (start, end, row_text) = &rows[row];
    let mut char_index = *start;
    let mut cells = 0usize;
    for grapheme in row_text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme).max(1);
        if col < cells.saturating_add(width) {
            return char_index;
        }
        cells = cells.saturating_add(width);
        char_index = char_index.saturating_add(grapheme.chars().count());
    }
    char_index.min(*end).min(text_chars)
}

/// Visual (col, row) for a char cursor index.
#[cfg(test)]
#[must_use]
pub fn visual_cursor_xy(text: &str, width: usize, cursor: usize) -> (usize, usize) {
    visual_cursor_xy_from_rows(&visual_rows(text, width), cursor)
}

/// Map a char index onto already-wrapped composer rows.
#[must_use]
pub fn visual_cursor_xy_from_rows(
    rows: &[(usize, usize, String)],
    cursor: usize,
) -> (usize, usize) {
    if rows.is_empty() {
        return (0, 0);
    }
    for (ri, (start, end, row_text)) in rows.iter().enumerate() {
        let owns_end = rows.get(ri + 1).is_none_or(|next| next.0 > *end);
        if cursor < *end || (cursor == *end && owns_end) {
            let chars = cursor.saturating_sub(*start);
            let prefix: String = row_text.chars().take(chars).collect();
            return (UnicodeWidthStr::width(prefix.as_str()), ri);
        }
    }
    let last = rows.len().saturating_sub(1);
    let row_text = &rows[last].2;
    (UnicodeWidthStr::width(row_text.as_str()), last)
}

/// Previous extended-grapheme boundary at or before one logical left move.
#[must_use]
pub fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.chars().count());
    text.graphemes(true)
        .scan(0usize, |index, grapheme| {
            let start = *index;
            *index = index.saturating_add(grapheme.chars().count());
            Some(start)
        })
        .take_while(|start| *start < cursor)
        .last()
        .unwrap_or(0)
}

/// Next extended-grapheme boundary after the caret.
#[must_use]
pub fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.chars().count());
    let mut index = 0usize;
    for grapheme in text.graphemes(true) {
        index = index.saturating_add(grapheme.chars().count());
        if index > cursor {
            return index;
        }
    }
    index
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Char index of the first character of the word before the caret.
///
/// Skips any run of separators immediately left of the caret, then the word
/// itself — the readline `Ctrl-W` / `Alt-B` boundary.
#[must_use]
pub fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = cursor.min(chars.len());
    while index > 0 && !is_word_char(chars[index - 1]) {
        index -= 1;
    }
    while index > 0 && is_word_char(chars[index - 1]) {
        index -= 1;
    }
    index
}

/// Char index one past the end of the word after the caret (`Alt-F`).
#[must_use]
pub fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = cursor.min(chars.len());
    while index < chars.len() && !is_word_char(chars[index]) {
        index += 1;
    }
    while index < chars.len() && is_word_char(chars[index]) {
        index += 1;
    }
    index
}

/// Char index of the start of the logical line holding the caret (`Ctrl-A`).
#[must_use]
pub fn logical_line_start(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = cursor.min(chars.len());
    while index > 0 && chars[index - 1] != '\n' {
        index -= 1;
    }
    index
}

/// Char index of the end of the logical line holding the caret (`Ctrl-E`).
#[must_use]
pub fn logical_line_end(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = cursor.min(chars.len());
    while index < chars.len() && chars[index] != '\n' {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests;
