//! The one-line item row: subject budgeting and command highlighting.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui::theme::Theme;

/// Fewest cells the subject keeps before the meta is dropped (§5.4).
pub(super) const MIN_SUBJECT_CELLS: usize = 12;

/// One item row, never wrapped: `prefix` (rail and glyph), the label cut
/// with `…`, right-aligned `meta` parts, then `caret`. At narrow widths
/// meta parts drop from the right first so the label keeps `label_floor`
/// cells (usually [`MIN_SUBJECT_CELLS`]), except the first `kept_meta`,
/// which stay while the label is cut further; the caret always stays.
#[allow(
    clippy::too_many_arguments,
    reason = "An item row budgets its prefix, label, suffix, metadata, caret, theme, and width independently before clipping."
)]
pub(super) fn item_row(
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
pub(super) fn truncate_spans(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
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

pub(super) fn command_styles(theme: &Theme) -> crate::tui::highlight::HighlightStyles {
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
pub(super) fn command_spans(command: &str, theme: &Theme) -> Vec<Span<'static>> {
    let highlighted = crate::tui::highlight::lines(command, Some("bash"), &command_styles(theme))
        .into_iter()
        .next()
        .unwrap_or_default();
    prompted_command(highlighted, command, theme)
}

/// `$ ` and the highlighted first row of a command, its first token (the
/// program) in the accent.
pub(super) fn prompted_command(
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
