//! One row renderer shared by every Menu tab (spec 02 §5.5).
//!
//! A row is exactly one line: a glyph, a label, and a right-aligned value
//! column. Focus and hover are a highlight and nothing else — the row's
//! description belongs to the floating panel at the foot of the Menu
//! body (§5.5), never to an inline row that reflows the list under the pointer.
//! A click focuses a row and a double-click activates it exactly as Enter
//! does, except in the value column, which focuses and activates in one
//! click. [`ListClicks`] pairs clicks for every list in the TUI.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::tui::composer_selection::MULTI_CLICK_INTERVAL;
use crate::tui::theme::Theme;
use std::time::Instant;

/// Columns reserved for the value column's hit rect beyond the value text, so
/// a click just right of a short value still lands on it.
pub(crate) const VALUE_HIT_SLACK: u16 = 2;

/// One rendered row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PickerRow {
    /// Leading glyph, 1 cell (`▸`/`▾`/`◆`/a readiness chip).
    pub glyph: &'static str,
    /// Row label.
    pub label: String,
    /// Right-aligned value, empty for action rows.
    pub value: String,
    /// One-sentence description, shown in the floating description panel while
    /// the row is focused (§5.5). Never drawn on the row itself.
    pub description: String,
    /// Trailing pill shown only on the focused row, e.g. `· restart`.
    pub pill: Option<&'static str>,
    /// Renders the value in the recessive style (a `false` bool, or a row that
    /// cannot be activated).
    pub recessive: bool,
}

/// Hit geometry for one drawn row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RowHit {
    /// Index into the caller's row list.
    pub index: usize,
    /// Whole-row rect.
    pub rect: Rect,
    /// Value-column rect, a subset of `rect`.
    pub value: Rect,
}

/// What a click on a row means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClickOutcome {
    /// A single click: it only focuses the row.
    Focus(usize),
    /// A double-click, or a click on the value column.
    Activate(usize),
}

/// Resolves the click rule (spec 02 §5.5) for a click that `double` says
/// completed a double-click on this row.
pub(crate) const fn classify_click(double: bool, hit: &RowHit, column: u16) -> ClickOutcome {
    let on_value = column >= hit.value.x && column < hit.value.x.saturating_add(hit.value.width);
    if on_value || double {
        ClickOutcome::Activate(hit.index)
    } else {
        ClickOutcome::Focus(hit.index)
    }
}

/// The list a click landed in, so a double-click pairs only within one list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClickList {
    Slash,
    Resume,
    Settings,
    SettingsChoice,
    Models,
    Agents,
    Processes,
    Queue,
}

/// Pairs left clicks into double-clicks: a second click on the same row of
/// the same list within [`MULTI_CLICK_INTERVAL`]. Every selectable list uses
/// it, so click selects and double-click activates alike everywhere.
#[derive(Debug, Default)]
pub(crate) struct ListClicks {
    last: Option<(ClickList, String, Instant)>,
}

impl ListClicks {
    /// Records a click on `row` of `list` at `at` and reports whether it
    /// completes a double-click. A completed pair is consumed, so a third
    /// click starts a new pair rather than activating again.
    pub(crate) fn click(&mut self, list: ClickList, row: &str, at: Instant) -> bool {
        let double = self
            .last
            .as_ref()
            .is_some_and(|(last_list, last_row, last_at)| {
                *last_list == list
                    && last_row == row
                    && at.saturating_duration_since(*last_at) <= MULTI_CLICK_INTERVAL
            });
        self.last = (!double).then(|| (list, row.to_owned(), at));
        double
    }
}

/// Finds the row under `(column, row)`, if any.
pub(crate) fn hit_at(hits: &[RowHit], column: u16, row: u16) -> Option<RowHit> {
    hits.iter()
        .copied()
        .find(|hit| hit.rect.contains((column, row).into()))
}

/// Clips `text` to `width` columns, marking the cut with `…`.
///
/// A row that ends mid-word with no mark reads as content, not as a clip
/// (§5.5). Callers that can afford another row should wrap instead.
#[must_use]
pub(crate) fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let cell = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + cell > width.saturating_sub(1) {
            break;
        }
        used += cell;
        out.push(character);
    }
    out.push('…');
    out
}

/// Word-wraps `text` to `width` columns.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.width()
        } else {
            current.width() + 1 + word.width()
        };
        if !current.is_empty() && candidate > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Draws one row at `rect`, returning its hit geometry. A row is always one
/// line high.
///
/// `focused` is the keyboard/click focus, and it carries the bold label plus
/// the row's pill.
/// `highlighted` is the wider "this row is the one being read" state, which
/// hover also sets: it paints the band and nothing else, so hovering says which
/// row the pointer is on without moving focus (spec 02 §5.5).
pub(crate) fn render_row(
    frame: &mut Frame<'_>,
    rect: Rect,
    theme: &Theme,
    row: &PickerRow,
    index: usize,
    focused: bool,
    highlighted: bool,
) -> RowHit {
    let base = if focused || highlighted {
        theme.row_hover
    } else {
        theme.slash_item
    };
    let label_style = if focused {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    // A `false` bool and a locked row recede: the value column is help-styled
    // even under the focus band, so "off" never reads as an active choice.
    let value_style = if row.recessive || !focused {
        theme.slash_item_help
    } else {
        base
    };

    let pill = if focused { row.pill.unwrap_or("") } else { "" };
    let width = usize::from(rect.width);
    let left = format!("{} {}", row.glyph, row.label);
    let right = if pill.is_empty() {
        row.value.clone()
    } else {
        format!("{} {pill}", row.value)
    };
    // The value column is the row's answer, so the label yields to it and
    // states its own cut rather than sliding off the border unmarked (§5.5).
    let left = clip(
        &left,
        width.saturating_sub(right.width()).saturating_sub(1).max(1),
    );
    let gap = width
        .saturating_sub(left.width())
        .saturating_sub(right.width())
        .max(1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, label_style),
            Span::styled(" ".repeat(gap), base),
            Span::styled(right.clone(), value_style),
        ])),
        Rect { height: 1, ..rect },
    );

    let value_width = u16::try_from(right.width())
        .unwrap_or(0)
        .saturating_add(VALUE_HIT_SLACK)
        .min(rect.width);
    RowHit {
        index,
        rect,
        value: Rect {
            x: rect
                .x
                .saturating_add(rect.width)
                .saturating_sub(value_width),
            y: rect.y,
            width: value_width,
            height: 1,
        },
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests {
    use super::{
        ClickList, ClickOutcome, ListClicks, MULTI_CLICK_INTERVAL, RowHit, classify_click, hit_at,
    };
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};

    fn hit(index: usize, y: u16) -> RowHit {
        RowHit {
            index,
            rect: Rect {
                x: 0,
                y,
                width: 40,
                height: 1,
            },
            value: Rect {
                x: 32,
                y,
                width: 8,
                height: 1,
            },
        }
    }

    #[test]
    fn a_click_only_focuses_and_a_double_click_activates() {
        let row = hit(3, 5);
        assert_eq!(classify_click(false, &row, 4), ClickOutcome::Focus(3));
        assert_eq!(classify_click(true, &row, 4), ClickOutcome::Activate(3));
    }

    #[test]
    fn the_value_column_focuses_and_activates_in_one_click() {
        let row = hit(3, 5);
        assert_eq!(classify_click(false, &row, 35), ClickOutcome::Activate(3));
    }

    /// Spec 02 §5.5: only a second click on the same row of the same list
    /// within the multi-click interval is a double-click; a pair is consumed.
    #[test]
    fn clicks_pair_on_one_row_of_one_list_within_the_interval() {
        let start = Instant::now();
        let mut clicks = ListClicks::default();
        assert!(!clicks.click(ClickList::Settings, "3", start));
        assert!(clicks.click(ClickList::Settings, "3", start + Duration::from_millis(200)));
        assert!(
            !clicks.click(ClickList::Settings, "3", start + Duration::from_millis(300)),
            "a third click starts a new pair"
        );

        let mut clicks = ListClicks::default();
        assert!(!clicks.click(ClickList::Settings, "3", start));
        assert!(
            !clicks.click(ClickList::Settings, "4", start),
            "another row"
        );
        assert!(!clicks.click(ClickList::Resume, "4", start), "another list");
        let late = start + MULTI_CLICK_INTERVAL + Duration::from_millis(1);
        assert!(!clicks.click(ClickList::Resume, "4", late), "too slow");
        assert!(clicks.click(ClickList::Resume, "4", late));
    }

    #[test]
    fn hit_testing_finds_the_row_under_the_pointer() {
        let hits = [hit(0, 4), hit(1, 5), hit(2, 6)];
        assert_eq!(hit_at(&hits, 10, 5).map(|h| h.index), Some(1));
        assert!(hit_at(&hits, 10, 9).is_none());
        assert!(hit_at(&hits, 99, 5).is_none());
    }
}
