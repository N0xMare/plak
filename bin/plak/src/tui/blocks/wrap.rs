//! Cell-width wrapping of plain text.

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

pub(crate) enum CellItem<T> {
    Grapheme {
        value: T,
        cells: usize,
        whitespace: bool,
    },
    Break,
}

/// The sole terminal wrapping primitive. Words that fit a complete row move
/// intact; longer words split only between extended grapheme clusters.
pub(crate) fn wrap_cell_items<T>(
    items: impl IntoIterator<Item = CellItem<T>>,
    width: usize,
) -> Vec<Vec<T>> {
    let width = width.max(1);
    let mut rows = vec![Vec::new()];
    let mut used = 0usize;
    let mut word = Vec::new();
    let flush_word = |word: &mut Vec<(T, usize)>, rows: &mut Vec<Vec<T>>, used: &mut usize| {
        let word_width = word
            .iter()
            .fold(0usize, |total, (_, cells)| total.saturating_add(*cells));
        if *used > 0 && used.saturating_add(word_width) > width {
            rows.push(Vec::new());
            *used = 0;
        }
        for (value, cells) in word.drain(..) {
            if *used > 0 && cells > 0 && used.saturating_add(cells) > width {
                rows.push(Vec::new());
                *used = 0;
            }
            if let Some(row) = rows.last_mut() {
                row.push(value);
            } else {
                rows.push(vec![value]);
            }
            *used = used.saturating_add(cells);
        }
    };
    for item in items {
        match item {
            CellItem::Break => {
                flush_word(&mut word, &mut rows, &mut used);
                rows.push(Vec::new());
                used = 0;
            }
            CellItem::Grapheme {
                value,
                cells,
                whitespace,
            } => {
                if !whitespace {
                    word.push((value, cells));
                    continue;
                }
                flush_word(&mut word, &mut rows, &mut used);
                if used > 0 && cells > 0 && used.saturating_add(cells) > width {
                    rows.push(Vec::new());
                    used = 0;
                }
                if let Some(row) = rows.last_mut() {
                    row.push(value);
                } else {
                    rows.push(vec![value]);
                }
                used = used.saturating_add(cells);
            }
        }
    }
    flush_word(&mut word, &mut rows, &mut used);
    rows
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlainRow {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Word-aware rows with char-sequence offsets for editor geometry.
pub(crate) fn wrap_plain_rows(text: &str, width: usize) -> Vec<PlainRow> {
    let mut char_index = 0usize;
    let items = text.graphemes(true).map(|grapheme| {
        let start = char_index;
        char_index = char_index.saturating_add(grapheme.chars().count());
        if grapheme == "\n" || grapheme == "\r\n" {
            CellItem::Break
        } else {
            CellItem::Grapheme {
                value: (start, char_index, grapheme),
                cells: grapheme.width().max(1),
                whitespace: grapheme.chars().all(char::is_whitespace),
            }
        }
    });
    wrap_cell_items(items, width)
        .into_iter()
        .scan(0usize, |next_start, row| {
            let start = row.first().map_or(*next_start, |(start, _, _)| *start);
            let end = row.last().map_or(start, |(_, end, _)| *end);
            let mut following = text.chars().skip(end);
            *next_start = end
                + match (following.next(), following.next()) {
                    (Some('\r'), Some('\n')) => 2,
                    (Some('\n'), _) => 1,
                    _ => 0,
                };
            Some(PlainRow {
                start,
                end,
                text: row.into_iter().map(|(_, _, grapheme)| grapheme).collect(),
            })
        })
        .collect()
}

/// Wrap plain text to terminal-cell width without changing whitespace.
#[must_use]
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    wrap_plain_rows(text, width)
        .into_iter()
        .map(|row| row.text)
        .collect()
}

/// A steer's band rows: the first wrapped at `first_width`, the rest at
/// `width`.
pub(crate) fn steer_rows(text: &str, width: usize, first_width: usize) -> Vec<String> {
    let mut rows = wrap_plain_rows(text, first_width).into_iter();
    let Some(first) = rows.next() else {
        return Vec::new();
    };
    // Only the single space a soft wrap broke on. A hard newline is not that
    // space, so the next line's indentation stays.
    let rest = rows.next().map(|next| {
        let mut rest: String = text.chars().skip(next.start).collect();
        let broke_on_space = next.start > 0
            && text
                .chars()
                .nth(next.start - 1)
                .is_some_and(|ch| ch != '\n' && ch != '\r')
            && rest.starts_with(' ');
        if broke_on_space {
            rest.remove(0);
        }
        rest
    });
    std::iter::once(first.text)
        .chain(rest.iter().flat_map(|rest| wrap_plain(rest, width)))
        .collect()
}
