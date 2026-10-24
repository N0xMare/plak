//! The styled transcript renderer: one block's rows under a theme at a
//! content width. Its children hold the windowed render cache (`cache`),
//! row estimates for unmeasured blocks (`estimate`), the lead rows of tool,
//! item and group blocks (`lead`), expanded item detail (`detail`) and the
//! one-line item row (`item_row`).

mod cache;
mod detail;
mod estimate;
mod item_row;
mod lead;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::tui) use cache::TRANSCRIPT_RENDER_CACHE_BYTES;
pub(in crate::tui) use cache::{TranscriptChange, TranscriptRenderCache};
#[cfg(test)]
pub(super) use estimate::{estimate_block_rows, estimate_rows_for_test};
pub(in crate::tui) use lead::{LeadRow, lead_rows};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui::blocks::{
    CellItem, DisplayMode, PROMPT_PREFIX, RenderPrefs, RowPlan, ScrollBlock, discarded_label,
    format_approval_block, mail_lines, milestone_detail, steer_rows, thinking_detail_rail,
    thinking_detail_rows, thinking_has_detail, tool_block_closes_with_blank, tool_is_failed,
    tool_is_running, wrap_cell_items, wrap_plain,
};
use crate::tui::markdown::RowCopy;
use crate::tui::presentation::ActivityKind;
use crate::tui::theme::Theme;
use detail::{ItemFacts, push_item_detail};
use estimate::system_row_is_hidden;
use lead::cell_footer_rows;

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let line_style = line.style;
    let items = line.spans.into_iter().flat_map(|span| {
        let style = span.style;
        span.content
            .graphemes(true)
            .map(move |grapheme| {
                if grapheme == "\n" {
                    CellItem::Break
                } else {
                    CellItem::Grapheme {
                        value: (grapheme.to_owned(), style),
                        cells: grapheme.width().max(1),
                        whitespace: grapheme.chars().all(char::is_whitespace),
                    }
                }
            })
            .collect::<Vec<_>>()
    });
    wrap_cell_items(items, width)
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
        wrap_plain(text, w)
    } else {
        // The first row keeps room for the trailing labels and a gap.
        steer_rows(text, w, w.saturating_sub(trailing_width + 1).max(1))
    };
    let lines = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let gutter = if index == 0 { PROMPT_PREFIX } else { "  " };
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
    plan: &RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: RenderPrefs,
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
        if tool_block_closes_with_blank(plan) {
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
    plan: &RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: RenderPrefs,
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
    plan: &RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: RenderPrefs,
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
    plan: &RowPlan,
    theme: &Theme,
    content_width: usize,
    prefs: RenderPrefs,
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
                format!("  {}", discarded_label(*thinking)),
                theme.chrome_dim,
            )))
            .chain(
                wrap_plain(text, width)
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
            if !plan.group_folded && mode.shows_detail() && thinking_has_detail(text) {
                let rail = thinking_detail_rail(plan);
                for row in thinking_detail_rows(text, *mode) {
                    logical.extend(wrap_styled_line_railed(
                        &rail,
                        &rail,
                        theme.system,
                        Line::from(Span::styled(row, theme.system)),
                        content_width,
                    ));
                }
            }
            if tool_block_closes_with_blank(plan) {
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
            let rows = format_approval_block(
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
            let running = tool_is_running(state);
            let style = if running {
                theme.busy
            } else if tool_is_failed(state) {
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
            if tool_block_closes_with_blank(plan) {
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
                if *kind != ActivityKind::Compose && *mode == DisplayMode::Expanded {
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
            if tool_block_closes_with_blank(plan) {
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
                for row in milestone_detail(milestone) {
                    logical.extend(wrap_styled_line_railed(
                        "    │ ",
                        "    │   ",
                        theme.tool_label,
                        Line::from(Span::styled(row, theme.system)),
                        content_width.max(1),
                    ));
                }
            }
            if tool_block_closes_with_blank(plan) {
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
            if tool_block_closes_with_blank(plan) {
                logical.push(Line::from(""));
            }
            logical
        }
        ScrollBlock::Mail { .. } => mail_lines(block, plan, content_width)
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
