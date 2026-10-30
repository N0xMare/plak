//! Menu tab bodies for `Usage`, `Context` and `Session` (spec 02 §5.5).
//!
//! Pure rendering. Every function here takes a host-owned snapshot and returns
//! lines, so the honesty clauses are testable without a terminal: nothing in
//! this module invents a number, subtracts cached input from context
//! admission, or prints `$0` for an unknown cost.
//!
//! The Menu has three peer tabs, so
//! each renderer below is one tab body with its own scroll cursor and
//! `Left`/`Right` never means anything but "switch tab".

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui::blocks::format_duration_secs as fmt_age;
use crate::tui::presentation::{
    ContextBreakdownView, ContextUsage, SessionInfoView, TurnUsage, UsageCostView, UsageLimitsView,
    UsageMeterReading, UsageMeterView, VerticalUsageState,
};
use crate::tui::theme::Theme;

/// Width at or below which the Context grid switches to 10×10 and the Session
/// rows stop clustering onto one line.
pub(crate) const NARROW_BREAKPOINT: u16 = 50;

/// Cells in the categorical context grid. Always exactly 100 (spec 02 §5.6).
pub(crate) const CONTEXT_GRID_CELLS: usize = 100;

/// Cells in a limit bar. Always exactly 20 (spec 02 §7.2).
pub(crate) const LIMIT_BAR_CELLS: usize = 20;

/// Black square (U+25A0). Fill and empty share this glyph so the unused track
/// cannot overrun the fill. It is mid-cell like `[` `]` in Ghostty, iTerm,
/// tmux, and herdr — a lower-block sits on the baseline, a box-drawing
/// horizontal is centered but too thin.
pub(crate) const LIMIT_BAR_CELL: char = '\u{25a0}';

/// Occupying cell on the Context board (U+26C1 WHITE DRAUGHTS KING).
pub(crate) const GRID_OCCUPIED: char = '\u{26C1}';

/// Unused cell below the compaction fence (U+26F6 SQUARE FOUR CORNERS).
pub(crate) const GRID_FREE: char = '\u{26F6}';

/// Reserved compact-buffer tail past the fence (U+26DD SQUARED SALTIRE).
pub(crate) const GRID_RESERVED: char = '\u{26DD}';

/// Gap between the Context board and the facts column when they sit side by
/// side.
const CONTEXT_SIDE_GAP: usize = 3;

/// Honest string for a field the host did not report.
const NOT_REPORTED: &str = "not reported";

/// The three read-only tab bodies this module renders. The variant is only a
/// heading now: each one is its own Menu tab (spec 02 §5.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UsageSection {
    /// Canonical context projection and the 100-cell grid.
    Context,
    /// All four allowance verticals and the session ledger.
    Limits,
    /// Typed session facts, click-to-copy.
    Session,
}

impl UsageSection {
    /// Heading text.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Context => "Context",
            Self::Limits => "Limits",
            Self::Session => "Session",
        }
    }
}

/// Token formatting shared by every Usage surface.
///
/// `<1000` plain; `1000..99_500` one decimal `k`; `>=99_500` integer `k`. The
/// integer switch at 99.5k deliberately avoids a column-width discontinuity at
/// the 99.9k/100k boundary.
#[must_use]
pub(crate) fn fmt_tok(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 99_500 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "bounded below 99_500, where f64 represents the value exactly"
        )]
        let thousands = tokens as f64 / 1_000.0;
        return format!("{thousands:.1}k");
    }
    if tokens < 999_500 {
        return format!("{}k", (tokens + 500) / 1_000);
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "display rounding of a token count; no arithmetic depends on it"
    )]
    let millions = tokens as f64 / 1_000_000.0;
    format!("{millions:.1}m")
}

fn fmt_trigger_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        return format!("{}m", tokens / 1_000_000);
    }
    if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
        return format!("{}k", tokens / 1_000);
    }
    tokens.to_string()
}

const fn unreported_compaction_trigger_copy(enabled: bool) -> &'static str {
    if enabled {
        "Auto compaction is on; trigger not reported for this route."
    } else {
        "Auto compaction is off."
    }
}

/// Share of the window as a percent string: a tiny nonzero share floors to
/// `0.1%`, below 10% keeps one decimal, at or above 10% keeps none.
#[must_use]
pub(crate) fn percent_of_window(tokens: u64, total: u64) -> String {
    if total == 0 {
        return "—".to_owned();
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "presentation-only ratio of two token counts"
    )]
    let share = (tokens as f64 / total as f64) * 100.0;
    if tokens > 0 && share < 0.1 {
        return "0.1%".to_owned();
    }
    if share < 10.0 {
        format!("{share:.1}%")
    } else {
        format!("{share:.0}%")
    }
}

/// Header percent, recomputed from used/total — never read from a pre-rounded
/// field (spec 02 §5.6).
fn header_percent(used: u64, total: u64) -> String {
    if total == 0 {
        return "—".to_owned();
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "presentation-only ratio of two token counts"
    )]
    let share = (used as f64 / total as f64) * 100.0;
    format!("{share:.2}%")
}

/// Whole percent of the window, floored, for fence comparisons.
const fn floor_percent(used: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    used.saturating_mul(100) / total
}

/// One legend row's parts. Compact Claude-style `glyph label: tokens (pct)`.
struct LegendRow {
    glyph: String,
    label: String,
    tokens: String,
    percent: String,
    detail: Option<String>,
    style: Style,
}

/// Chrome the TUI owns for the Context section: the bound model label and the
/// auto-compaction enablement. The host supplies the token trigger.
pub(crate) struct ContextChrome<'a> {
    /// Model label printed under the header, when a model is bound.
    pub model_label: Option<&'a str>,
    /// Whether the host envelope's compaction fence is enabled.
    pub auto_compact_enabled: bool,
}

/// Renders the Context section (spec 02 §5.6).
#[allow(
    clippy::too_many_lines,
    reason = "The context board coordinates measured usage, compaction fence, category grid, legend, and footer under one width decision."
)]
pub(crate) fn context_lines(
    usage: &ContextUsage,
    chrome: &ContextChrome<'_>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let muted = theme.slash_item_help;
    let text = theme.slash_item;
    let strong = theme.slash_item.add_modifier(Modifier::BOLD);

    let mut lines = vec![Line::from(Span::styled(
        UsageSection::Context.label(),
        strong,
    ))];

    let (Some(prepared), Some(window)) = (usage.prepared_input, usage.total_window) else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Context usage is unavailable · {}", usage.status),
            muted,
        )));
        lines.push(Line::from(Span::styled(
            format!("Evidence: {}", usage.evidence),
            muted,
        )));
        if usage
            .breakdown
            .as_ref()
            .is_none_or(|breakdown| breakdown.auto_compact_trigger_tokens.is_none())
            || !chrome.auto_compact_enabled
        {
            lines.push(Line::from(Span::styled(
                unreported_compaction_trigger_copy(chrome.auto_compact_enabled),
                muted,
            )));
        }
        return lines;
    };
    let fallback = ContextBreakdownView {
        used_tokens: prepared.tokens(),
        total_tokens: window,
        ..ContextBreakdownView::default()
    };
    let breakdown = usage.breakdown.as_ref().unwrap_or(&fallback);
    let used = breakdown.used_tokens;
    let total = window;
    // The host envelope supplies the trigger; chrome only reflects enablement.
    let fence = chrome
        .auto_compact_enabled
        .then_some(breakdown.auto_compact_trigger_tokens)
        .flatten();

    let categories = occupying_categories(breakdown, used);
    let (free_tokens, reserved_tokens) = reserved_and_free(used, total, fence);
    let occupying: Vec<u64> = categories.iter().map(|(_, tokens)| *tokens).collect();
    let grid = grid_rows(&occupying, reserved_tokens, total, width, theme);

    let mut header = Vec::new();
    if let Some(model) = chrome.model_label {
        header.push(Line::from(Span::styled(model.to_owned(), muted)));
    }
    header.push(Line::from(Span::styled(
        format!(
            "{} / {} tokens ({})",
            fmt_tok(used),
            fmt_tok(total),
            header_percent(used, total)
        ),
        text,
    )));
    if prepared.is_upper_bound() || prepared.tokens() != used {
        let qualifier = if prepared.is_upper_bound() { "≤" } else { "" };
        header.push(Line::from(Span::styled(
            format!(
                "Prepared input {qualifier}{} tokens",
                fmt_tok(prepared.tokens())
            ),
            muted,
        )));
    }
    // An admitted ceiling under the window is the dispatch gate:
    // named beside the window as its own fact, never folded into the meter.
    if let Some(ceiling) = usage
        .admitted_ceiling
        .as_ref()
        .filter(|ceiling| ceiling.tokens < total)
    {
        header.push(Line::from(Span::styled(
            format!(
                "Admitted input {} tokens ({} of window · {})",
                fmt_tok(ceiling.tokens),
                percent_of_window(ceiling.tokens, total),
                ceiling.basis
            ),
            muted,
        )));
    }

    let legend = context_legend_lines(
        &categories,
        free_tokens,
        reserved_tokens,
        fence.is_some(),
        breakdown,
        total,
        theme,
    );

    lines.push(Line::from(""));
    if side_by_side(width, &grid, &header, &legend) {
        let mut facts = header;
        facts.push(Line::from(""));
        facts.extend(legend);
        lines.extend(join_grid_and_facts(grid, facts));
    } else {
        lines.extend(header);
        lines.push(Line::from(""));
        lines.extend(grid);
        lines.push(Line::from(""));
        lines.extend(legend);
    }

    lines.extend(context_footer_lines(
        usage, breakdown, chrome, total, fence, width, theme,
    ));
    lines
}

/// A host-counted value, or the honest string when the host counted none.
fn counted(value: Option<u64>) -> String {
    value.map_or_else(|| NOT_REPORTED.to_owned(), |count| count.to_string())
}

/// Bar and legend colours for occupying categories. Occupying cells share one
/// glyph distinguished only by colour (spec 02 §5.6).
fn category_palette(theme: &Theme) -> [Style; 4] {
    [
        theme.slash_item_help,
        theme.slash_item,
        theme.assistant,
        theme.user,
    ]
}

/// Occupying categories: the reported ones, then whatever `used` they do not
/// account for. The host emits envelope JSON as `Used (unattributed)` and any
/// tokenizer floor above occupancy as `Provider-observed (est.)`; this
/// remainder is only a safety net.
fn occupying_categories(breakdown: &ContextBreakdownView, used: u64) -> Vec<(String, u64)> {
    let attributed: u64 = breakdown
        .categories
        .iter()
        .map(|category| category.tokens)
        .sum();
    let mut categories: Vec<(String, u64)> = breakdown
        .categories
        .iter()
        .map(|category| (category.label.clone(), category.tokens))
        .collect();
    let unattributed = used.saturating_sub(attributed);
    if unattributed > 0 {
        categories.push((
            if categories.is_empty() {
                "Used".to_owned()
            } else {
                "Used (unattributed)".to_owned()
            },
            unattributed,
        ));
    }
    categories
}

/// Unused tokens split into free (below the fence) and the reserved compact
/// tail. Occupying wins when the window is already past the fence.
fn reserved_and_free(used: u64, total: u64, fence: Option<u64>) -> (u64, u64) {
    let unused = total.saturating_sub(used);
    let Some(fence) = fence else {
        return (unused, 0);
    };
    let reserved_budget = total.saturating_sub(fence);
    let reserved = unused.min(reserved_budget);
    (unused.saturating_sub(reserved), reserved)
}

fn context_legend_lines(
    categories: &[(String, u64)],
    free_tokens: u64,
    reserved_tokens: u64,
    fence_on: bool,
    breakdown: &ContextBreakdownView,
    total: u64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let muted = theme.slash_item_help;
    let text = theme.slash_item;
    let palette = category_palette(theme);
    let mut rows: Vec<LegendRow> = categories
        .iter()
        .enumerate()
        .map(|(index, (label, tokens))| LegendRow {
            glyph: GRID_OCCUPIED.to_string(),
            label: label.clone(),
            tokens: format!("{} tokens", fmt_tok(*tokens)),
            percent: format!("({})", percent_of_window(*tokens, total)),
            detail: None,
            style: palette[index % palette.len()],
        })
        .collect();
    rows.push(LegendRow {
        glyph: GRID_FREE.to_string(),
        label: "Free space".to_owned(),
        tokens: format!("{} tokens", fmt_tok(free_tokens)),
        percent: format!("({})", percent_of_window(free_tokens, total)),
        detail: None,
        style: muted,
    });
    if fence_on {
        rows.push(LegendRow {
            glyph: GRID_RESERVED.to_string(),
            label: "Autocompact buffer".to_owned(),
            tokens: format!("{} tokens", fmt_tok(reserved_tokens)),
            percent: format!("({})", percent_of_window(reserved_tokens, total)),
            detail: None,
            style: theme.warning,
        });
    }
    let informational: Vec<LegendRow> = breakdown
        .informational
        .iter()
        .map(|row| LegendRow {
            glyph: "◈".to_owned(),
            label: row.label.clone(),
            tokens: format!("{} tokens", fmt_tok(row.tokens)),
            percent: format!("({})", percent_of_window(row.tokens, total)),
            detail: row.detail.clone(),
            style: theme.tool,
        })
        .collect();

    let mut lines = vec![Line::from(Span::styled(
        "Estimated usage by category".to_owned(),
        text,
    ))];
    for row in &rows {
        lines.push(legend_line(row));
    }
    if !informational.is_empty() {
        lines.push(Line::from(""));
        for row in &informational {
            lines.push(legend_line(row));
        }
    }
    lines
}

fn legend_line(row: &LegendRow) -> Line<'static> {
    let detail = row
        .detail
        .as_ref()
        .map_or_else(String::new, |detail| format!(" · {detail}"));
    Line::from(Span::styled(
        format!(
            "{} {}: {} {}{detail}",
            row.glyph, row.label, row.tokens, row.percent
        ),
        row.style,
    ))
}

fn context_footer_lines(
    usage: &ContextUsage,
    breakdown: &ContextBreakdownView,
    chrome: &ContextChrome<'_>,
    total: u64,
    fence: Option<u64>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let used = breakdown.used_tokens;
    let muted = theme.slash_item_help;
    let mut lines = vec![Line::from("")];
    let pct = floor_percent(used, total);
    match fence {
        // The idle fence acts only on an anchored measure (spec 01 §10.3).
        Some(fence) if used >= fence && !breakdown.occupancy_anchored => {
            lines.push(Line::from(Span::styled(
                format!(
                    "Over {}: auto-compact waits for a provider token count",
                    percent_of_window(fence, total)
                ),
                muted,
            )));
        }
        Some(fence) if used >= fence => lines.push(Line::from(Span::styled(
            format!(
                "Auto-compact triggers next turn (at {})",
                percent_of_window(fence, total)
            ),
            theme.error,
        ))),
        Some(fence) => {
            lines.push(Line::from(Span::styled(
                format!(
                    "Auto-compact at {} ({}) · ~{} tokens remaining",
                    percent_of_window(fence, total),
                    fmt_trigger_tokens(fence),
                    fmt_tok(fence.saturating_sub(used))
                ),
                muted,
            )));
        }
        None => lines.push(Line::from(Span::styled(
            unreported_compaction_trigger_copy(chrome.auto_compact_enabled),
            muted,
        ))),
    }

    lines.push(Line::from(""));
    // The counters are three independent facts, so a narrow tab stacks them
    // rather than losing the last one mid-word to the paragraph clip.
    let counters = [
        format!("Turns: {}", counted(breakdown.turns)),
        format!("Tool calls: {}", counted(breakdown.tool_calls)),
        format!("Compactions: {}", counted(breakdown.compactions)),
    ];
    let joined = counters.join(" \u{b7} ");
    if joined.chars().count() <= usize::from(width) {
        lines.push(Line::from(Span::styled(joined, muted)));
    } else {
        lines.extend(
            counters
                .into_iter()
                .map(|counter| Line::from(Span::styled(counter, muted))),
        );
    }

    // A reported trigger bounds the tip band without a percentage fallback.
    if pct >= 80 && fence.is_some_and(|trigger| used < trigger) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tip: run /compact to free up context space.".to_owned(),
            theme.error,
        )));
    }

    if let Some(note) = usage.non_guarantee.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Qualification: {note}"),
            muted,
        )));
    }
    lines
}

/// Side-by-side only when every fact fits in the remaining columns, so a
/// legend row is never clipped mid-word.
fn side_by_side(
    width: u16,
    grid: &[Line<'static>],
    header: &[Line<'static>],
    legend: &[Line<'static>],
) -> bool {
    let Some(first) = grid.first() else {
        return false;
    };
    let left = first.width();
    let budget = usize::from(width)
        .saturating_sub(left)
        .saturating_sub(CONTEXT_SIDE_GAP);
    if budget < 32 {
        return false;
    }
    header
        .iter()
        .chain(legend.iter())
        .all(|line| line.width() <= budget)
}

fn join_grid_and_facts(grid: Vec<Line<'static>>, facts: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let left_width = grid.first().map(Line::width).unwrap_or(0);
    let n = grid.len().max(facts.len());
    let gap = " ".repeat(CONTEXT_SIDE_GAP);
    let mut out = Vec::with_capacity(n);
    for index in 0..n {
        let mut spans = match grid.get(index) {
            Some(line) => pad_line(line, left_width),
            None => vec![Span::raw(" ".repeat(left_width))],
        };
        spans.push(Span::raw(gap.clone()));
        if let Some(line) = facts.get(index) {
            spans.extend(line.spans.iter().cloned());
        }
        out.push(Line::from(spans));
    }
    out
}

fn pad_line(line: &Line<'static>, width: usize) -> Vec<Span<'static>> {
    let mut spans = line.spans.clone();
    let used = line.width();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

fn grid_per_row(width: u16) -> usize {
    if width <= NARROW_BREAKPOINT { 10 } else { 20 }
}

/// True when `line` begins with one Context-board row at this body width.
#[cfg(test)]
#[must_use]
pub(crate) fn is_context_grid_line(line: &str, width: u16) -> bool {
    context_grid_prefix(line, grid_per_row(width)).is_some()
}

#[cfg(test)]
fn context_grid_prefix(line: &str, per_row: usize) -> Option<String> {
    let n = per_row.saturating_mul(2).saturating_sub(1);
    let prefix: String = line.chars().take(n).collect();
    if prefix.chars().count() != n {
        return None;
    }
    if prefix.chars().filter(|ch| *ch != ' ').count() != per_row {
        return None;
    }
    if prefix.chars().all(|ch| ch == ' ' || is_grid_glyph(ch)) {
        Some(prefix)
    } else {
        None
    }
}

#[cfg(test)]
fn is_grid_glyph(ch: char) -> bool {
    ch == GRID_OCCUPIED || ch == GRID_FREE || ch == GRID_RESERVED
}

/// The 100-cell categorical grid: 5 rows × 20 at normal width, 10 × 10 narrow.
/// Occupying, then free, then the reserved compact tail.
fn grid_rows(
    occupying: &[u64],
    reserved: u64,
    total: u64,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let per_row = grid_per_row(width);
    let palette = category_palette(theme);
    let mut cells: Vec<Span<'static>> = Vec::with_capacity(CONTEXT_GRID_CELLS);
    for (index, tokens) in occupying.iter().enumerate() {
        let count = cells_for(*tokens, total).min(CONTEXT_GRID_CELLS - cells.len());
        let style = palette[index % palette.len()];
        for _ in 0..count {
            cells.push(Span::styled(GRID_OCCUPIED.to_string(), style));
        }
    }
    let remaining = CONTEXT_GRID_CELLS.saturating_sub(cells.len());
    let reserved_cells = cells_for(reserved, total).min(remaining);
    let free_cells = remaining.saturating_sub(reserved_cells);
    for _ in 0..free_cells {
        cells.push(Span::styled(GRID_FREE.to_string(), theme.slash_item_help));
    }
    for _ in 0..reserved_cells {
        cells.push(Span::styled(GRID_RESERVED.to_string(), theme.warning));
    }
    cells
        .chunks(per_row)
        .map(|chunk| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(chunk.len() * 2);
            for (position, cell) in chunk.iter().enumerate() {
                if position > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(cell.clone());
            }
            Line::from(spans)
        })
        .collect()
}

/// Cells a category earns, rounded from its share of the total.
fn cells_for(tokens: u64, total: u64) -> usize {
    if total == 0 {
        return 0;
    }
    let half_cells = tokens.saturating_mul(200) / total;
    usize::try_from(half_cells.div_ceil(2))
        .unwrap_or(CONTEXT_GRID_CELLS)
        .min(CONTEXT_GRID_CELLS)
}

/// Transient Usage refresh controls, populated only by product observations.
#[derive(Default)]
pub(crate) struct RefreshActions {
    pub accounts: Vec<String>,
    pub active: Option<String>,
    pub error: Option<(String, String)>,
    pub focus: usize,
    /// Whether Up/Down has been used since Usage opened; the keyboard focus
    /// is painted only then.
    pub focus_shown: bool,
    /// The action under the pointer. Hover highlights it without moving focus.
    pub hover: Option<String>,
    pub pending_read: bool,
    pub reveal: bool,
}

pub(crate) struct RefreshHit {
    pub account: String,
    pub line: usize,
    pub column: u16,
}

pub(crate) struct LimitsLayout {
    pub lines: Vec<Line<'static>>,
    pub refresh_hits: Vec<RefreshHit>,
}

/// Chrome the TUI owns for the Limits section.
pub(crate) struct LimitsChrome<'a> {
    pub refresh: Option<&'a RefreshActions>,
    /// Last failed refresh notes beside otherwise valid allowance facts.
    pub refresh_failure_notes: &'a [String],
    /// State of the allowance read itself, when it has not settled. This is
    /// the read's own honesty, distinct from a vertical's own state.
    pub read_note: Option<&'a str>,
    /// Latest terminal provider usage observation for the current turn
    /// (§7.2 honesty clause 1).
    pub latest_turn: Option<TurnUsage>,
}

/// Renders the Limits section (spec 02 §7.2). All four verticals, always.
#[allow(
    clippy::too_many_lines,
    reason = "one function owns the whole Limits layout the spec pins block by block"
)]
pub(crate) fn limits_layout(
    limits: &UsageLimitsView,
    chrome: &LimitsChrome<'_>,
    width: u16,
    theme: &Theme,
) -> LimitsLayout {
    let mut refresh_hits = Vec::new();
    let muted = theme.slash_item_help;
    let text = theme.slash_item;
    let strong = theme.slash_item.add_modifier(Modifier::BOLD);

    let mut lines = vec![Line::from(Span::styled(
        UsageSection::Limits.label(),
        strong,
    ))];
    if let Some(note) = chrome.read_note {
        lines.push(Line::from(Span::styled(note.to_owned(), muted)));
    }
    if let Some((account, error)) = chrome.refresh.and_then(|refresh| refresh.error.as_ref()) {
        let recorded = limits.verticals.iter().find(|vertical| vertical.vertical.account() == account)
            .is_some_and(|vertical| {
                matches!(&vertical.state, VerticalUsageState::Unavailable { reason } if reason.starts_with("refresh failed at "))
                    || chrome.refresh_failure_notes.iter().any(|note| note.starts_with(&format!("{}: refresh failed at ", vertical.vertical.provider_label())))
            });
        if !recorded {
            lines.push(Line::from(Span::styled(error.clone(), theme.error)));
        }
    }
    for note in chrome.refresh_failure_notes {
        lines.push(Line::from(Span::styled(note.clone(), muted)));
    }
    if let Some(age) = limits.stale_age_seconds {
        lines.push(Line::from(Span::styled(
            format!("Stale · this projection is {} old", fmt_age(age)),
            theme.error,
        )));
    }

    let columns = LimitsColumns::measure(limits);
    for vertical in &limits.verticals {
        lines.push(Line::from(""));
        // The header is the vertical itself; the plan or tier belongs to the
        // account line under it, never guessed into the header.
        let mut header = vec![Span::styled(
            vertical.vertical.provider_label().to_owned(),
            strong,
        )];
        if let Some(refresh) = chrome.refresh {
            let account = vertical.vertical.account();
            let column =
                u16::try_from(vertical.vertical.provider_label().width() + 1).unwrap_or(u16::MAX);
            let active = refresh.active.as_deref() == Some(account);
            if (active || refresh.accounts.iter().any(|id| id == account))
                && column.saturating_add(9) <= width
            {
                header.push(Span::raw(" "));
                header.push(Span::styled(
                    if active {
                        "[refreshing…]"
                    } else {
                        "[refresh]"
                    },
                    if refresh.active.is_none()
                        && (refresh.hover.as_deref() == Some(account)
                            || (refresh.focus_shown
                                && refresh
                                    .accounts
                                    .get(refresh.focus)
                                    .is_some_and(|id| id == account)))
                    {
                        theme.slash_selected
                    } else {
                        muted
                    },
                ));
                if refresh.active.is_none() {
                    refresh_hits.push(RefreshHit {
                        account: account.to_owned(),
                        line: lines.len(),
                        column,
                    });
                }
            }
        }
        lines.push(Line::from(header));
        if let Some(account) = account_line(
            vertical.account_email.as_ref(),
            vertical.plan_label.as_ref(),
        ) {
            lines.push(Line::from(Span::styled(account, muted)));
        }
        match &vertical.state {
            // A non-Ready vertical renders one plain line naming the CLI
            // remedy, and no bar.
            VerticalUsageState::NotConnected => {
                lines.push(Line::from(Span::styled(
                    format!(
                        "not connected \u{b7} run `{}`",
                        crate::tui::providers::account_remedy(vertical.vertical.account())
                    ),
                    muted,
                )));
            }
            VerticalUsageState::Unavailable { reason } => {
                lines.push(Line::from(Span::styled(reason.clone(), muted)));
            }
            VerticalUsageState::Ready { meters } if meters.is_empty() => {
                lines.push(Line::from(Span::styled(
                    "the provider reported no allowance meters; usage is unknown".to_owned(),
                    muted,
                )));
            }
            VerticalUsageState::Ready { meters } => {
                lines.extend(meter_lines(meters, &columns, theme, text, muted));
            }
        }
        // Provider-reported balances are amounts, not allowances: they close
        // the block and never take a bar.
        for balance in &vertical.balances {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<width$}  ", balance.label, width = columns.label),
                    text,
                ),
                Span::styled(balance.value.clone(), text),
            ]));
        }
        if let Some(observed) = vertical.observed_at.as_ref() {
            lines.push(Line::from(Span::styled(
                format!("Fetched {observed}"),
                muted,
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Session usage (since start or last resume):".to_owned(),
        strong,
    )));
    for line in session_ledger_lines(limits) {
        lines.push(Line::from(Span::styled(line, text)));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Latest turn:".to_owned(), strong)));
    match chrome.latest_turn {
        None => lines.push(Line::from(Span::styled(
            "  No provider usage report observed in this attached view.".to_owned(),
            muted,
        ))),
        Some(turn) => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  Input {} \u{b7} output {} \u{b7} reasoning {} \u{b7} cached input {} \u{b7} cache write {}",
                    group_digits(turn.input_tokens),
                    group_digits(turn.output_tokens),
                    group_digits(turn.reasoning_tokens),
                    group_digits(turn.cached_input_tokens),
                    group_digits(turn.cache_write_tokens)
                ),
                text,
            )));
            lines.push(Line::from(Span::styled(
                format!("  Coverage {}", turn_coverage(turn)),
                muted,
            )));
            lines.push(Line::from(Span::styled(
                "  Counters are provider-reported; cached input and cache writes are not savings or billing claims."
                    .to_owned(),
                muted,
            )));
        }
    }
    let mut fitted = Vec::new();
    let mut starts = Vec::with_capacity(lines.len());
    for line in lines {
        starts.push(fitted.len());
        fitted.extend(fit_limits_lines(vec![line], usize::from(width).max(1)));
    }
    for hit in &mut refresh_hits {
        hit.line = starts[hit.line];
    }
    LimitsLayout {
        lines: fitted,
        refresh_hits,
    }
}

#[cfg(test)]
pub(crate) fn limits_lines(
    limits: &UsageLimitsView,
    chrome: &LimitsChrome<'_>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    limits_layout(limits, chrome, width, theme).lines
}

fn fit_limits_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| {
            if line.width() <= width {
                return vec![line];
            }
            let style = line.style;
            let indent = line
                .spans
                .iter()
                .flat_map(|span| span.content.chars())
                .take_while(|ch| *ch == ' ')
                .count()
                .min(width.saturating_sub(1));
            let indent_style = line.spans.first().map_or(style, |span| span.style);
            let items = line.spans.into_iter().flat_map(|span| {
                span.content
                    .graphemes(true)
                    .map(|grapheme| crate::tui::blocks::CellItem::Grapheme {
                        value: (grapheme.to_owned(), span.style),
                        cells: grapheme.width().max(1),
                        whitespace: grapheme.chars().all(char::is_whitespace),
                    })
                    .collect::<Vec<_>>()
            });
            crate::tui::blocks::wrap_cell_items(items.skip(indent), width.saturating_sub(indent))
                .into_iter()
                .map(|row| {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if indent > 0 {
                        spans.push(Span::styled(" ".repeat(indent), indent_style));
                    }
                    for (grapheme, style) in row {
                        if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
                            last.content.to_mut().push_str(&grapheme);
                        } else {
                            spans.push(Span::styled(grapheme, style));
                        }
                    }
                    Line::from(spans).style(style)
                })
                .collect()
        })
        .collect()
}

/// `Account: <email> (Pro)` when the projection carries an account, or the
/// bare plan when it carries only that. Never guessed.
fn account_line(email: Option<&String>, plan: Option<&String>) -> Option<String> {
    match (
        email.filter(|value| !value.is_empty()),
        plan.filter(|value| !value.is_empty()),
    ) {
        (Some(email), Some(plan)) => Some(format!("Account: {email} ({plan})")),
        (Some(email), None) => Some(format!("Account: {email}")),
        (None, Some(plan)) => Some(format!("Account: {plan}")),
        (None, None) => None,
    }
}

/// The column widths the whole Limits section shares.
///
/// They are measured once across every vertical, so a bar starts in the same
/// column under `Codex` as under `SuperGrok`, and every amount is right-aligned
/// in one column rather than per block. Grouped sub-blocks keep the same label
/// width and shift by their own indent, which is what makes a sub-block read as
/// nested rather than as another vertical.
struct LimitsColumns {
    /// Widest meter or balance label at a vertical's own level.
    label: usize,
    /// Widest amount value in any amount row.
    amount: usize,
}

impl LimitsColumns {
    fn measure(limits: &UsageLimitsView) -> Self {
        let mut label = 0;
        let mut amount = 0;
        for vertical in &limits.verticals {
            for balance in &vertical.balances {
                label = label.max(balance.label.chars().count());
            }
            let VerticalUsageState::Ready { meters } = &vertical.state else {
                continue;
            };
            for meter in meters {
                let indent = if meter.group.is_some() { 2 } else { 0 };
                label = label.max(indent + meter.label.chars().count());
                if let Some(group) = meter.group.as_deref() {
                    label = label.max(group.chars().count());
                }
                if let UsageMeterReading::Amounts { entries } = &meter.reading {
                    for entry in entries {
                        amount = amount.max(entry.value.chars().count());
                    }
                }
            }
        }
        Self { label, amount }
    }
}

/// One vertical's meters: the ungrouped ones first, then each group as an
/// indented sub-block under its own name.
fn meter_lines(
    meters: &[UsageMeterView],
    columns: &LimitsColumns,
    theme: &Theme,
    text: Style,
    muted: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(meters.len());
    for meter in meters.iter().filter(|meter| meter.group.is_none()) {
        lines.push(meter_row(meter, "", columns, theme, text, muted));
    }

    let mut seen: Vec<&String> = Vec::new();
    for meter in meters {
        let Some(group) = meter.group.as_ref() else {
            continue;
        };
        if seen.contains(&group) {
            continue;
        }
        seen.push(group);
        lines.push(Line::from(Span::styled(format!("{group}:"), muted)));
        for member in meters
            .iter()
            .filter(|candidate| candidate.group.as_ref() == Some(group))
        {
            lines.push(meter_row(member, "  ", columns, theme, text, muted));
        }
    }
    lines
}

/// One meter row: label column, bar, right-aligned number, then the reset the
/// provider reported. Every value is host-supplied; nothing here is recomputed
/// from a rendered string.
fn meter_row(
    meter: &UsageMeterView,
    indent: &str,
    columns: &LimitsColumns,
    theme: &Theme,
    text: Style,
    muted: Style,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(
            "{:<width$}  ",
            format!("{indent}{}", meter.label),
            width = columns.label
        ),
        text,
    )];
    match &meter.reading {
        UsageMeterReading::Percent {
            floored_percent,
            percent_left,
            ..
        } => {
            let left = percent_left_of(*floored_percent, *percent_left);
            let filled = usize::from(left)
                .saturating_mul(LIMIT_BAR_CELLS)
                .saturating_div(100);
            let fill = bar_fill_style(left, theme);
            // An exhausted allowance draws no fill cells at all, so colouring
            // only the fill left the one state that matters looking like a
            // neutral row. The empty track carries the colour when
            // there is nothing left to fill, and the percentage always states
            // its own health.
            let track = if filled == 0 { fill } else { theme.bar_track };
            spans.push(Span::styled("[".to_owned(), muted));
            spans.push(Span::styled(
                LIMIT_BAR_CELL.to_string().repeat(filled),
                fill,
            ));
            spans.push(Span::styled(
                LIMIT_BAR_CELL
                    .to_string()
                    .repeat(LIMIT_BAR_CELLS.saturating_sub(filled)),
                track,
            ));
            spans.push(Span::styled("]".to_owned(), muted));
            spans.push(Span::styled(format!(" {left:>3}%"), fill));
            spans.push(Span::styled(" left".to_owned(), muted));
        }
        UsageMeterReading::Amounts { entries } => {
            for (index, entry) in entries.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" \u{b7} ".to_owned(), muted));
                }
                spans.push(Span::styled(
                    format!("{:>width$}", entry.value, width = columns.amount),
                    text,
                ));
                spans.push(Span::styled(format!(" {}", entry.label), muted));
            }
        }
        UsageMeterReading::Unreported => {
            spans.push(Span::styled(
                crate::tui::presentation::USAGE_NOT_REPORTED.to_owned(),
                muted,
            ));
        }
    }
    if let Some(reset) = meter.resets_at.as_ref() {
        spans.push(Span::styled(format!(" (resets {reset})"), muted));
    }
    Line::from(spans)
}

/// Colour of a limit bar's fill, chosen by the allowance that remains: healthy
/// at half or more left, caution above a fifth, and error below it.
fn bar_fill_style(percent_left: u8, theme: &Theme) -> Style {
    if percent_left >= 50 {
        theme.success
    } else if percent_left >= 20 {
        theme.warning
    } else {
        theme.error
    }
}

/// Percent left, floored. The host projects it; until it does, it is the
/// complement of the floored percent used, which is the same truncation.
#[must_use]
pub(crate) const fn percent_left_of(floored_percent: u8, percent_left: Option<u8>) -> u8 {
    match percent_left {
        Some(left) if left <= 100 => left,
        Some(_) => 100,
        None => 100u8.saturating_sub(floored_percent),
    }
}

/// Reporting coverage for the latest turn, stated exactly (spec 02 §7.2).
fn turn_coverage(turn: TurnUsage) -> String {
    let (Some(operations), Some(reporting)) =
        (turn.model_operations, turn.reporting_model_operations)
    else {
        return "unknown · live cumulative snapshot, not a turn total".to_owned();
    };
    if operations == 0 {
        return "unknown (no model operations observed)".to_owned();
    }
    if reporting == operations {
        format!("complete ({reporting}/{operations} operations)")
    } else if reporting == 0 {
        format!("missing (0/{operations} operations)")
    } else {
        format!("partial ({reporting}/{operations} operations)")
    }
}

/// The session ledger rows, as plain strings so a test can pin them.
#[must_use]
pub(crate) fn session_ledger_lines(limits: &UsageLimitsView) -> Vec<String> {
    let ledger = &limits.session;
    let turn_wall_time = ledger
        .turn_wall_time
        .clone()
        .unwrap_or_else(|| NOT_REPORTED.to_owned());
    let cost = match &ledger.cost {
        // Never `$0` for an unknown cost — the honest string is `not available`.
        UsageCostView::NotAvailable { reason } => format!("not available ({reason})"),
        UsageCostView::Reported { amount } => amount.clone(),
    };
    vec![
        format!(
            "  Input tokens:   {} ({} cached)",
            group_digits(ledger.input_tokens),
            group_digits(ledger.cached_input_tokens)
        ),
        format!(
            "  Output tokens:  {} ({} reasoning)",
            group_digits(ledger.output_tokens),
            group_digits(ledger.reasoning_tokens)
        ),
        format!(
            "  Model calls:    {} ({} reporting) · turn wall time (host-measured): {turn_wall_time}",
            ledger.model_calls, ledger.reporting_model_calls
        ),
        format!("  Cost:           {cost}"),
    ]
}

fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// One Session-info row. Non-compact rows render as two lines (muted label,
/// then value); compact rows cluster onto one line (spec 02 §5.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionRow {
    /// Row label without its colon.
    pub label: String,
    /// Formatted value, or an honest `not reported`.
    pub value: String,
    /// Whether this row belongs to the tight runtime cluster.
    pub compact: bool,
}

/// Live chrome facts the Session section pairs with the host's typed snapshot.
///
/// There is deliberately no `binding` here: the account/model/effort it
/// repeated already have their own rows (spec 02 §5.5).
pub(crate) struct SessionLive<'a> {
    /// Live context gauge `X / Y tokens (pct)`, when a projection has landed.
    /// Absent, the host's admitted envelope from [`SessionInfoView::context`]
    /// states the window instead.
    pub context: Option<String>,
    /// Queue state.
    pub queue: String,
    /// Tree epoch state, when a snapshot has been observed.
    pub tree_epoch: Option<&'a str>,
    /// This process's binary id, with the `/reload` offer when a newer
    /// build is installed.
    pub build: String,
}

/// Builds every Session row from typed data. Never re-parses a rendered
/// string (spec 02 §5.5).
pub(crate) fn session_rows(info: &SessionInfoView, live: &SessionLive<'_>) -> Vec<SessionRow> {
    let row = |label: &str, value: String, compact: bool| SessionRow {
        label: label.to_owned(),
        value,
        compact,
    };
    let stated = |value: &str| {
        if value.is_empty() {
            NOT_REPORTED.to_owned()
        } else {
            value.to_owned()
        }
    };
    let optional = |value: Option<&String>| {
        value
            .filter(|text| !text.is_empty())
            .map_or_else(|| NOT_REPORTED.to_owned(), Clone::clone)
    };
    let options = if info.admitted_options.is_empty() {
        "none".to_owned()
    } else {
        info.admitted_options.join(" · ")
    };
    let mut rows = vec![
        row("Plak version", stated(&info.host_version), false),
        row("Build", live.build.clone(), false),
        row("Session ID", stated(&info.session_id), false),
        row("Working directory", stated(&info.working_dir), false),
        row("Model", optional(info.root_model.as_ref()), true),
    ];
    // The root template only earns a row when it is not already the model
    // line: repeating one string under two labels is the duplication the
    // operator called out.
    if let Some(template) = info
        .root_template
        .as_ref()
        .filter(|name| !name.is_empty() && Some(*name) != info.root_model.as_ref())
    {
        rows.push(row("Template", template.clone(), true));
    }
    rows.extend([
        row("Effort", optional(info.root_effort.as_ref()), true),
        row("Thinking", stated(&info.thinking), true),
        row("Tool policy", optional(info.tool_policy.as_ref()), true),
        row("Options", options, true),
        row("API kernel", optional(info.api_kernel.as_ref()), true),
        row(
            "Turn",
            info.turn_number
                .map_or_else(|| NOT_REPORTED.to_owned(), |turn| turn.to_string()),
            true,
        ),
        row("Context", context_value(info, live), true),
        row(
            "Tree epoch",
            info.tree_epoch
                .as_deref()
                .or(live.tree_epoch)
                .filter(|epoch| !epoch.is_empty())
                .map_or_else(|| NOT_REPORTED.to_owned(), ToOwned::to_owned),
            true,
        ),
        row("Queue", live.queue.clone(), true),
    ]);
    rows
}

/// The live gauge when one has landed; otherwise the host's admitted envelope.
fn context_value(info: &SessionInfoView, live: &SessionLive<'_>) -> String {
    if let Some(live) = live.context.as_ref() {
        return live.clone();
    }
    let Some(context) = info.context.as_ref() else {
        return NOT_REPORTED.to_owned();
    };
    let mut parts = Vec::new();
    if let Some(admitted) = context.admitted_input {
        parts.push(format!("{} tokens admitted", fmt_tok(admitted)));
    }
    if let Some(window) = context.reported_window {
        parts.push(format!("window {}", fmt_tok(window)));
    }
    if parts.is_empty() {
        return NOT_REPORTED.to_owned();
    }
    parts.push(context.evidence.clone());
    parts.join(" · ")
}

/// `y` copies every row as `Label: value` lines (spec 02 §5.5).
#[must_use]
pub(crate) fn session_copy_text(rows: &[SessionRow]) -> String {
    rows.iter()
        .map(session_row_copy_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One row's copy payload (click-to-copy).
#[must_use]
pub(crate) fn session_row_copy_text(row: &SessionRow) -> String {
    format!("{}: {}", row.label, row.value)
}

/// Renders the Session section, returning the lines and, for each row, the
/// line index its value occupies so a click can copy exactly that row.
pub(crate) fn session_lines(
    rows: &[SessionRow],
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let muted = theme.slash_item_help;
    let text = theme.slash_item;
    let strong = theme.slash_item.add_modifier(Modifier::BOLD);

    let mut lines = vec![Line::from(vec![
        Span::styled(UsageSection::Session.label(), strong),
        Span::styled("   click a value to copy it", muted),
    ])];
    let mut hits = Vec::with_capacity(rows.len());
    let mut previous_compact = false;
    for (index, row) in rows.iter().enumerate() {
        // A blank separator precedes every group except between consecutive
        // compact rows, so the runtime block reads as one tight cluster.
        if !(row.compact && previous_compact) {
            lines.push(Line::from(""));
        }
        if row.compact {
            hits.push((index, lines.len()));
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", row.label), muted),
                Span::styled(row.value.clone(), text),
            ]));
        } else {
            lines.push(Line::from(Span::styled(format!("{}:", row.label), muted)));
            hits.push((index, lines.len()));
            lines.push(Line::from(Span::styled(row.value.clone(), text)));
        }
        previous_compact = row.compact;
    }
    (lines, hits)
}

/// Plain text of `lines`, for the per-section clipboard payload.
#[must_use]
pub(crate) fn plain_text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests;
