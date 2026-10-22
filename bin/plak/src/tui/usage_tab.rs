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
        muted,
        text,
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
        usage, breakdown, chrome, used, total, fence, width, theme, muted,
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
    muted: Style,
    text: Style,
) -> Vec<Line<'static>> {
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
    used: u64,
    total: u64,
    fence: Option<u64>,
    width: u16,
    theme: &Theme,
    muted: Style,
) -> Vec<Line<'static>> {
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
                        && refresh
                            .accounts
                            .get(refresh.focus)
                            .is_some_and(|id| id == account)
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
mod tests {
    #[test]
    fn wrapped_limit_reset_rows_keep_the_source_indent() {
        use ratatui::text::{Line, Span};
        for indent in [2, 4] {
            let source = format!("{}(resets Tue 23:44 local time)", " ".repeat(indent));
            let rows = super::fit_limits_lines(vec![Line::from(Span::raw(source))], 18);
            assert!(rows.len() > 1);
            for row in rows {
                assert!(row.width() <= 18);
                let text = row
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                assert!(text.starts_with(&" ".repeat(indent)), "{text:?}");
            }
        }
    }
    use super::{
        CONTEXT_GRID_CELLS, GRID_OCCUPIED, GRID_RESERVED, LIMIT_BAR_CELL, LIMIT_BAR_CELLS,
        LimitsChrome, SessionLive, SessionRow, cells_for, context_lines, fmt_tok,
        is_context_grid_line, limits_lines, occupying_categories, percent_left_of,
        percent_of_window, reserved_and_free, session_copy_text, session_ledger_lines,
        session_lines, session_rows,
    };
    use crate::tui::presentation::{
        AdmittedContextCeiling, ContextBreakdownView, ContextCategoryView, ContextInfoRowView,
        ContextUsage, PreparedInputUsage, SessionInfoView, SessionLedgerView, UsageAmountView,
        UsageCostView, UsageLimitsView, UsageMeterReading, UsageMeterView, UsageVertical,
        VerticalUsageState, VerticalUsageView,
    };
    use crate::tui::theme::Theme;
    use ratatui::style::Style;

    fn chrome(fence: Option<u8>) -> super::ContextChrome<'static> {
        super::ContextChrome {
            model_label: Some("grok-4.6"),
            auto_compact_enabled: fence.is_some(),
        }
    }

    fn usage(used: u64, total: u64, fence: Option<u8>) -> ContextUsage {
        ContextUsage {
            breakdown: Some(ContextBreakdownView {
                used_tokens: used,
                total_tokens: total,
                categories: vec![
                    ContextCategoryView {
                        label: "System".to_owned(),
                        tokens: used / 10,
                    },
                    ContextCategoryView {
                        label: "Messages".to_owned(),
                        tokens: used - used / 10,
                    },
                ],
                informational: vec![ContextInfoRowView {
                    label: "Tool definitions".to_owned(),
                    tokens: 5_600,
                    detail: Some("12 tools".to_owned()),
                }],
                auto_compact_trigger_tokens: fence
                    .map(|pct| total.saturating_mul(u64::from(pct)) / 100),
                occupancy_anchored: true,
                turns: Some(5),
                tool_calls: Some(12),
                compactions: Some(0),
            }),
            prepared_input: Some(PreparedInputUsage::Exact(used)),
            total_window: Some(total),
            admitted_ceiling: Some(AdmittedContextCeiling {
                tokens: total,
                basis: "exact".to_owned(),
            }),
            gauge_percent: None,
            display_percent: None,
            ceiling_excess: None,
            evidence: "release-verified".to_owned(),
            non_guarantee: None,
            status: "available".to_owned(),
        }
    }

    fn text_of(lines: &[ratatui::text::Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn grid_only(lines: &[String], width: u16) -> Vec<String> {
        let per_row: usize = if width <= super::NARROW_BREAKPOINT {
            10
        } else {
            20
        };
        let n = per_row.saturating_mul(2).saturating_sub(1);
        lines
            .iter()
            .filter(|line| is_context_grid_line(line, width))
            .map(|line| line.chars().take(n).collect())
            .collect()
    }

    #[test]
    fn token_formatting_avoids_the_width_discontinuity() {
        assert_eq!(fmt_tok(0), "0");
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(1_000), "1.0k");
        assert_eq!(fmt_tok(36_700), "36.7k");
        assert_eq!(fmt_tok(99_499), "99.5k");
        assert_eq!(fmt_tok(99_500), "100k");
        assert_eq!(fmt_tok(963_000), "963k");
        assert_eq!(fmt_tok(1_000_000), "1.0m");
    }

    #[test]
    fn tiny_nonzero_shares_floor_to_a_tenth_of_a_percent() {
        assert_eq!(percent_of_window(0, 1_000), "0.0%");
        assert_eq!(percent_of_window(1, 1_000_000), "0.1%");
        assert_eq!(percent_of_window(30_000, 1_000_000), "3.0%");
        assert_eq!(percent_of_window(960_000, 1_000_000), "96%");
        assert_eq!(percent_of_window(5, 0), "—");
    }

    #[test]
    fn the_grid_is_always_exactly_one_hundred_cells_at_both_breakpoints() {
        let theme = Theme::dark();
        for (width, rows, per_row) in [(120_u16, 5_usize, 20_usize), (40, 10, 10)] {
            let lines = text_of(&context_lines(
                &usage(36_700, 1_000_000, Some(85)),
                &chrome(Some(85)),
                width,
                &theme,
            ));
            let grid = grid_only(&lines, width);
            assert_eq!(grid.len(), rows, "width {width} grid row count");
            let cells: usize = grid
                .iter()
                .map(|line| line.chars().filter(|ch| *ch != ' ').count())
                .sum();
            assert_eq!(cells, CONTEXT_GRID_CELLS, "width {width} grid cell count");
            for line in &grid {
                assert_eq!(line.chars().filter(|ch| *ch != ' ').count(), per_row);
            }
        }
    }

    #[test]
    fn cells_round_from_the_share() {
        assert_eq!(cells_for(0, 1_000), 0);
        assert_eq!(cells_for(4, 1_000), 0);
        assert_eq!(cells_for(5, 1_000), 1);
        assert_eq!(cells_for(1_000, 1_000), 100);
    }

    #[test]
    fn the_reserved_tail_is_the_share_past_the_fence() {
        assert_eq!(
            reserved_and_free(36_700, 1_000_000, Some(850_000)),
            (813_300, 150_000)
        );
        assert_eq!(
            reserved_and_free(900_000, 1_000_000, Some(850_000)),
            (0, 100_000)
        );
        assert_eq!(reserved_and_free(36_700, 1_000_000, None), (963_300, 0));
    }

    #[test]
    fn the_autocompact_buffer_occupies_the_unused_tail() {
        let theme = Theme::dark();
        let lines = text_of(&context_lines(
            &usage(36_700, 1_000_000, Some(85)),
            &chrome(Some(85)),
            40,
            &theme,
        ));
        let cells: String = grid_only(&lines, 40)
            .iter()
            .flat_map(|line| line.chars().filter(|ch| *ch != ' '))
            .collect();
        assert_eq!(cells.chars().count(), CONTEXT_GRID_CELLS);
        let reserved: String = cells
            .chars()
            .rev()
            .take_while(|ch| *ch == GRID_RESERVED)
            .collect();
        assert_eq!(reserved.chars().count(), 15);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Autocompact buffer: 150k tokens"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Estimated usage by category"))
        );
    }

    #[test]
    fn the_board_glyphs_are_one_cell_wide() {
        use unicode_width::UnicodeWidthChar;
        for glyph in [
            GRID_OCCUPIED,
            super::GRID_FREE,
            GRID_RESERVED,
            LIMIT_BAR_CELL,
        ] {
            assert_eq!(glyph.width(), Some(1), "U+{:04X}", u32::from(glyph));
        }
    }

    #[test]
    fn a_breakdown_free_projection_still_draws_one_hundred_honest_cells() {
        let theme = Theme::dark();
        let mut bare = usage(250_000, 1_000_000, Some(85));
        bare.breakdown = None;
        let lines = text_of(&context_lines(&bare, &chrome(Some(85)), 120, &theme));
        let cells: usize = grid_only(&lines, 120)
            .iter()
            .map(|line| line.chars().filter(|ch| *ch != ' ').count())
            .sum();
        assert_eq!(cells, CONTEXT_GRID_CELLS);
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!("{GRID_OCCUPIED} Used:")))
        );
        assert!(!lines.iter().any(|line| line.contains('◈')));
        // A breakdown-free projection counts nothing, and says so.
        assert!(lines.iter().any(|line| {
            line == "Turns: not reported · Tool calls: not reported · Compactions: not reported"
        }));
    }

    /// The counter line is three facts, not one that gets clipped.
    ///
    /// At 80x24 the Menu body is 66 columns and the joined line is
    /// 74, so `Compactions: not` was the last thing the operator saw — cut
    /// mid-word, with no ellipsis, because the tab's paragraph does not wrap.
    #[test]
    fn the_context_counters_stack_rather_than_clip_on_a_narrow_tab() {
        // The Menu body at 80x24.
        const NARROW: u16 = 66;

        let theme = Theme::dark();
        let mut bare = usage(250_000, 1_000_000, Some(85));
        bare.breakdown = None;

        let wide = text_of(&context_lines(&bare, &chrome(Some(85)), 120, &theme));
        assert!(wide.iter().any(|line| {
            line == "Turns: not reported · Tool calls: not reported · Compactions: not reported"
        }));

        let narrow = text_of(&context_lines(&bare, &chrome(Some(85)), NARROW, &theme));
        assert!(
            narrow
                .iter()
                .all(|line| line.chars().count() <= usize::from(NARROW)),
            "no Context line exceeds the body it is drawn into: {narrow:#?}"
        );
        for counter in [
            "Turns: not reported",
            "Tool calls: not reported",
            "Compactions: not reported",
        ] {
            assert!(
                narrow.iter().any(|line| line == counter),
                "`{counter}` survives whole: {narrow:#?}"
            );
        }
    }

    #[test]
    fn the_tip_band_renders_only_between_eighty_and_the_fence() {
        let theme = Theme::dark();
        let tip = "Tip: run /compact to free up context space.";
        let below = text_of(&context_lines(
            &usage(700_000, 1_000_000, Some(85)),
            &chrome(Some(85)),
            120,
            &theme,
        ));
        assert!(!below.iter().any(|line| line == tip));
        let inside = text_of(&context_lines(
            &usage(820_000, 1_000_000, Some(85)),
            &chrome(Some(85)),
            120,
            &theme,
        ));
        assert!(inside.iter().any(|line| line == tip));
        assert!(
            inside
                .iter()
                .any(|line| line.starts_with("Auto-compact at 85% (850k)"))
        );
        let past = text_of(&context_lines(
            &usage(900_000, 1_000_000, Some(85)),
            &chrome(Some(85)),
            120,
            &theme,
        ));
        assert!(!past.iter().any(|line| line == tip), "tip never stacks");
        assert!(
            past.iter()
                .any(|line| line == "Auto-compact triggers next turn (at 85%)")
        );
    }

    /// Over the fence on an unanchored measure the idle fence will not fire,
    /// so the footer must not promise a compaction.
    #[test]
    fn an_unanchored_measure_over_the_fence_does_not_promise_auto_compact() {
        let theme = Theme::dark();
        let mut over = usage(900_000, 1_000_000, Some(85));
        over.breakdown.as_mut().unwrap().occupancy_anchored = false;
        let lines = text_of(&context_lines(&over, &chrome(Some(85)), 120, &theme));
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("Auto-compact triggers"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "Over 85%: auto-compact waits for a provider token count")
        );
    }

    /// The auto-compact line names the fence the *host envelope* carries, in
    /// tokens as well as percent: a 1M window fences at 850k.
    #[test]
    fn the_auto_compact_line_names_the_host_envelope_fence_in_tokens() {
        let theme = Theme::dark();
        let envelope = usage(36_700, 1_000_000, Some(85));
        // Chrome enables compaction but does not supply the threshold.
        let lines = text_of(&context_lines(&envelope, &chrome(Some(50)), 120, &theme));
        assert!(
            lines
                .iter()
                .any(|line| line == "Auto-compact at 85% (850k) · ~813k tokens remaining"),
            "{lines:?}"
        );
    }

    #[test]
    fn enabled_compaction_without_a_reported_trigger_is_not_off() {
        for (breakdown_present, prepared_present) in [(true, true), (false, true), (false, false)] {
            let mut view = usage(36_700, 1_000_000, None);
            if !breakdown_present {
                view.breakdown = None;
            }
            if !prepared_present {
                view.prepared_input = None;
            }
            let lines = text_of(&context_lines(
                &view,
                &chrome(Some(90)),
                120,
                &Theme::dark(),
            ));
            assert!(lines.iter().any(|line| {
                line == "Auto compaction is on; trigger not reported for this route."
            }));
            assert!(!lines.iter().any(|line| line == "Auto compaction is off."));
        }
        let lines = text_of(&context_lines(
            &usage(36_700, 1_000_000, Some(90)),
            &chrome(None),
            120,
            &Theme::dark(),
        ));
        assert!(lines.iter().any(|line| line == "Auto compaction is off."));
    }

    #[test]
    fn non_round_triggers_keep_exact_tokens_and_do_not_fire_at_the_rounded_percent() {
        let mut view = usage(151_000, 200_000, None);
        view.breakdown.as_mut().unwrap().auto_compact_trigger_tokens = Some(151_201);
        let lines = text_of(&context_lines(
            &view,
            &chrome(Some(90)),
            120,
            &Theme::dark(),
        ));
        assert!(
            lines
                .iter()
                .any(|line| { line == "Auto-compact at 76% (151201) · ~201 tokens remaining" }),
            "{lines:?}"
        );
        assert_eq!(
            reserved_and_free(151_000, 200_000, Some(151_201)),
            (201, 48_799)
        );

        view.breakdown.as_mut().unwrap().used_tokens = 151_201;
        let lines = text_of(&context_lines(
            &view,
            &chrome(Some(90)),
            120,
            &Theme::dark(),
        ));
        assert!(
            lines
                .iter()
                .any(|line| line == "Auto-compact triggers next turn (at 76%)")
        );
    }

    #[test]
    fn the_header_percent_is_recomputed_not_read_from_a_rounded_field() {
        let theme = Theme::dark();
        let lines = text_of(&context_lines(
            &usage(36_700, 1_000_000, Some(85)),
            &chrome(Some(85)),
            120,
            &theme,
        ));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("36.7k / 1.0m tokens (3.67%)"))
        );
        assert!(lines.iter().any(|line| line.contains("grok-4.6")));
    }

    #[test]
    fn occupancy_header_keeps_the_admission_bound_as_its_own_fact() {
        let theme = Theme::dark();
        let mut view = usage(4_000, 200_000, Some(85));
        view.prepared_input = Some(PreparedInputUsage::UpperBound(20_000));
        if let Some(breakdown) = view.breakdown.as_mut() {
            breakdown.used_tokens = 4_000;
            breakdown.informational = vec![ContextInfoRowView {
                label: "Admission padding (est.)".to_owned(),
                tokens: 16_000,
                detail: None,
            }];
        }
        let lines = text_of(&context_lines(&view, &chrome(Some(85)), 120, &theme));
        assert!(
            lines.iter().any(|line| line.contains("4.0k / 200k tokens")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("≤4.0k")),
            "occupancy must not wear the bound qualifier: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Prepared input ≤20.0k tokens")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Admission padding (est.)")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Auto-compact at 85% (170k) · ~166k tokens remaining")),
            "compact remaining uses occupancy, same as the header: {lines:?}"
        );
    }

    #[test]
    fn admission_padding_does_not_enter_the_board() {
        let theme = Theme::dark();
        let mut view = usage(4_000, 200_000, None);
        view.prepared_input = Some(PreparedInputUsage::UpperBound(20_000));
        if let Some(breakdown) = view.breakdown.as_mut() {
            breakdown.used_tokens = 4_000;
            breakdown.categories = vec![
                ContextCategoryView {
                    label: "System prompt (est.)".to_owned(),
                    tokens: 1_000,
                },
                ContextCategoryView {
                    label: "Tool definitions (est. · 7 tools)".to_owned(),
                    tokens: 3_000,
                },
            ];
            breakdown.informational = vec![ContextInfoRowView {
                label: "Admission padding (est.)".to_owned(),
                tokens: 16_000,
                detail: None,
            }];
        }
        let lines = text_of(&context_lines(&view, &chrome(None), 120, &theme));
        let occupied: usize = grid_only(&lines, 120)
            .iter()
            .flat_map(|line| line.chars().filter(|ch| *ch == GRID_OCCUPIED))
            .count();
        assert!(
            occupied < 10,
            "padding must not fill the board: occupied={occupied} {lines:?}"
        );
    }

    #[test]
    fn occupying_categories_keep_envelope_and_provider_floor_distinct() {
        let breakdown = ContextBreakdownView {
            used_tokens: 5_000,
            total_tokens: 200_000,
            categories: vec![
                ContextCategoryView {
                    label: "System prompt (est.)".to_owned(),
                    tokens: 10,
                },
                ContextCategoryView {
                    label: "Used (unattributed)".to_owned(),
                    tokens: 20,
                },
                ContextCategoryView {
                    label: "Provider-observed (est.)".to_owned(),
                    tokens: 4_970,
                },
            ],
            ..ContextBreakdownView::default()
        };
        let categories = occupying_categories(&breakdown, 5_000);
        assert_eq!(
            categories
                .iter()
                .find(|(label, _)| label == "Used (unattributed)")
                .map(|(_, tokens)| *tokens),
            Some(20)
        );
        assert_eq!(
            categories
                .iter()
                .find(|(label, _)| label == "Provider-observed (est.)")
                .map(|(_, tokens)| *tokens),
            Some(4_970)
        );
        assert_eq!(categories.len(), 3, "{categories:?}");
    }

    #[test]
    fn an_unavailable_projection_states_so_and_draws_no_grid() {
        let theme = Theme::dark();
        let usage = ContextUsage {
            breakdown: None,
            prepared_input: None,
            total_window: None,
            admitted_ceiling: None,
            gauge_percent: None,
            display_percent: None,
            ceiling_excess: None,
            evidence: "unavailable".to_owned(),
            status: "unavailable".to_owned(),
            non_guarantee: None,
        };
        let lines = text_of(&context_lines(&usage, &chrome(Some(85)), 120, &theme));
        assert!(lines.iter().any(|line| line.contains("unavailable")));
        assert!(!lines.iter().any(|line| line.contains(GRID_OCCUPIED)));
    }

    #[test]
    fn a_live_in_turn_projection_is_current_not_stale() {
        let theme = Theme::dark();
        let live = usage(36_700, 1_000_000, Some(85));
        let lines = context_lines(&live, &chrome(Some(85)), 120, &theme);
        let text = text_of(&lines);
        assert!(!text.iter().any(|line| line.contains("Stale")), "{text:?}");
        assert!(!text.iter().any(|line| line.contains("unavailable")));
        assert!(
            !lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style == theme.error),
            "{text:?}"
        );
    }

    fn percent_meter(
        label: &str,
        used: u8,
        left: u8,
        resets: &str,
        group: Option<&str>,
    ) -> UsageMeterView {
        UsageMeterView {
            label: label.to_owned(),
            group: group.map(ToOwned::to_owned),
            reading: UsageMeterReading::Percent {
                floored_percent: used,
                exact_percent: format!("{used}.4"),
                percent_left: Some(left),
            },
            resets_at: Some(resets.to_owned()),
        }
    }

    fn limits_fixture() -> UsageLimitsView {
        UsageLimitsView {
            verticals: vec![
                VerticalUsageView {
                    vertical: UsageVertical::new("codex", "Codex"),
                    account_email: Some("operator@example.com".to_owned()),
                    plan_label: Some("Pro".to_owned()),
                    observed_at: None,
                    state: VerticalUsageState::Ready {
                        meters: vec![
                            percent_meter("Weekly limit", 1, 99, "23:44 on 6 Sep", None),
                            percent_meter("5h limit", 78, 22, "16:00 on 3 Sep", None),
                            percent_meter(
                                "Weekly limit",
                                20,
                                80,
                                "23:44 on 6 Sep",
                                Some("GPT-5.3-Codex-Spark limit"),
                            ),
                        ],
                    },
                    balances: Vec::new(),
                },
                VerticalUsageView {
                    vertical: UsageVertical::new("grok", "SuperGrok"),
                    account_email: None,
                    plan_label: None,
                    observed_at: None,
                    state: VerticalUsageState::NotConnected,
                    balances: Vec::new(),
                },
                VerticalUsageView {
                    vertical: UsageVertical::new("claude", "Claude"),
                    account_email: None,
                    plan_label: None,
                    observed_at: None,
                    state: VerticalUsageState::NotConnected,
                    balances: Vec::new(),
                },
                VerticalUsageView {
                    vertical: UsageVertical::new("openrouter", "OpenRouter"),
                    account_email: None,
                    plan_label: Some("API key · free tier".to_owned()),
                    observed_at: None,
                    state: VerticalUsageState::Ready {
                        meters: vec![UsageMeterView {
                            label: "Key limit".to_owned(),
                            group: None,
                            reading: UsageMeterReading::Amounts {
                                entries: vec![
                                    UsageAmountView {
                                        label: "cap".to_owned(),
                                        value: "$10.00".to_owned(),
                                    },
                                    UsageAmountView {
                                        label: "remaining".to_owned(),
                                        value: "$6.42".to_owned(),
                                    },
                                ],
                            },
                            resets_at: None,
                        }],
                    },
                    balances: Vec::new(),
                },
            ],
            session: SessionLedgerView {
                input_tokens: 1_234_567,
                cached_input_tokens: 1_000_000,
                output_tokens: 45_678,
                reasoning_tokens: 12_000,
                model_calls: 42,
                reporting_model_calls: 40,
                turn_wall_time: Some("3m12s".to_owned()),
                cost: UsageCostView::default(),
            },
            stale_age_seconds: None,
        }
    }

    /// A provider that named its period without a usage figure gets a row in
    /// words with the reset it reported, and never a bar or a percentage.
    #[test]
    fn an_unreported_reading_renders_in_words_with_its_reset_and_no_bar() {
        let theme = Theme::dark();
        let mut limits = limits_fixture();
        limits.verticals[1].state = VerticalUsageState::Ready {
            meters: vec![UsageMeterView {
                label: "Weekly limit".to_owned(),
                group: None,
                reading: UsageMeterReading::Unreported,
                resets_at: Some("13:22 on 10 Sep".to_owned()),
            }],
        };
        limits.verticals[1].balances = vec![crate::tui::presentation::UsageBalanceView {
            label: "Pay as you go".to_owned(),
            value: "not enabled".to_owned(),
        }];
        let lines = text_of(&limits_lines(
            &limits,
            &LimitsChrome {
                refresh: None,
                refresh_failure_notes: &[],
                read_note: None,
                latest_turn: None,
            },
            120,
            &theme,
        ));
        let row = lines
            .iter()
            .find(|line| line.contains(crate::tui::presentation::USAGE_NOT_REPORTED))
            .expect("the unmetered period is a row");
        assert!(row.starts_with("Weekly limit"), "{row}");
        assert!(row.ends_with("(resets 13:22 on 10 Sep)"), "{row}");
        assert!(!row.contains(LIMIT_BAR_CELL) && !row.contains('%'), "{row}");
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("no allowance meters")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("Pay as you go") && line.ends_with("not enabled")),
            "{lines:?}"
        );
    }

    #[test]
    fn all_four_verticals_are_listed_and_non_ready_ones_draw_no_bar() {
        let theme = Theme::dark();
        let lines = text_of(&limits_lines(
            &limits_fixture(),
            &LimitsChrome {
                refresh: None,
                refresh_failure_notes: &[],
                read_note: None,
                latest_turn: None,
            },
            120,
            &theme,
        ));
        for label in ["Codex", "SuperGrok", "Claude", "OpenRouter"] {
            assert!(
                lines.iter().any(|line| line.contains(label)),
                "{label} must be listed"
            );
        }
        // A non-Ready vertical is one plain line that names the CLI remedy.
        assert!(
            lines
                .iter()
                .any(|line| line == "not connected · run `plak login grok`"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "not connected · run `plak login claude`")
        );
        let claude = lines
            .iter()
            .position(|line| line == "Claude")
            .expect("the Claude block is listed");
        assert!(
            !lines[claude + 1].contains(LIMIT_BAR_CELL) && !lines[claude + 1].contains("0%"),
            "Claude is never drawn as a bar and never shown as 0%"
        );
    }

    /// Spec 02 §7.2: the vertical header is the vertical, the account line is its
    /// own line, and a group renders as an indented sub-block.
    #[test]
    fn a_vertical_renders_its_account_line_bars_and_group_sub_block() {
        let theme = Theme::dark();
        let lines = text_of(&limits_lines(
            &limits_fixture(),
            &LimitsChrome {
                refresh: None,
                refresh_failure_notes: &[],
                read_note: None,
                latest_turn: None,
            },
            120,
            &theme,
        ));
        assert!(
            lines
                .iter()
                .any(|line| line == "Account: operator@example.com (Pro)"),
            "{lines:?}"
        );
        let bar_at = |line: &str| line.find('[');
        let weekly = lines
            .iter()
            .find(|line| line.contains("Weekly limit") && line.contains("99% left"))
            .expect("weekly");
        let five_h = lines
            .iter()
            .find(|line| line.contains("5h limit") && line.contains("22% left"))
            .expect("5h");
        let spark_weekly = lines
            .iter()
            .find(|line| line.contains("Weekly limit") && line.contains("80% left"))
            .expect("spark weekly");
        assert_eq!(bar_at(weekly), bar_at(five_h), "{weekly} vs {five_h}");
        assert_eq!(
            bar_at(weekly),
            bar_at(spark_weekly),
            "{weekly} vs {spark_weekly}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "GPT-5.3-Codex-Spark limit:")
        );
        // OpenRouter carries only a plan; the account line still states it.
        assert!(
            lines
                .iter()
                .any(|line| line == "Account: API key · free tier")
        );
    }

    /// One meter row, as plain text plus the style of each filled bar cell.
    fn meter_row_line(percent_left: u8, theme: &Theme) -> ratatui::text::Line<'static> {
        let meter = UsageMeterView {
            label: "Weekly limit".to_owned(),
            group: None,
            reading: UsageMeterReading::Percent {
                floored_percent: 100 - percent_left,
                exact_percent: "0".to_owned(),
                percent_left: Some(percent_left),
            },
            resets_at: None,
        };
        let columns = super::LimitsColumns {
            label: meter.label.chars().count(),
            amount: 0,
        };
        super::meter_row(
            &meter,
            "",
            &columns,
            theme,
            theme.slash_item,
            theme.slash_item_help,
        )
    }

    fn meter_row_text(percent_left: u8, theme: &Theme) -> (String, Option<Style>) {
        let line = meter_row_line(percent_left, theme);
        // The fill span is always present, even at zero cells, so its style is
        // asserted at every percentage.
        let fill = line.spans.get(2).map(|span| span.style);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        (text, fill)
    }

    /// The bar fill *is* the percent left: full bar = nothing used, empty bar =
    /// limit reached (spec 02 §7.2).
    #[test]
    fn the_bar_fill_is_the_percent_left_over_twenty_cells() {
        let theme = Theme::dark();
        for percent in 0_u8..=100 {
            let (text, _) = meter_row_text(percent, &theme);
            let cells = text.chars().filter(|ch| *ch == LIMIT_BAR_CELL).count();
            assert_eq!(cells, LIMIT_BAR_CELLS, "percent {percent}");
        }
        let filled_cells = |percent: u8| -> usize {
            usize::from(percent)
                .saturating_mul(LIMIT_BAR_CELLS)
                .saturating_div(100)
        };
        for percent in [0_u8, 1, 99, 100] {
            let (text, _) = meter_row_text(percent, &theme);
            let inside = text
                .split_once('[')
                .and_then(|(_, rest)| rest.split_once(']'))
                .map(|(bar, _)| bar)
                .expect("the bar is bracketed");
            let filled = filled_cells(percent);
            assert_eq!(inside.chars().count(), LIMIT_BAR_CELLS);
            assert!(inside.chars().all(|ch| ch == LIMIT_BAR_CELL));
            let line = meter_row_line(percent, &theme);
            assert_eq!(line.spans[2].content.chars().count(), filled);
            assert!(line.spans[2].content.chars().all(|ch| ch == LIMIT_BAR_CELL));
            assert_eq!(
                line.spans[3].content.chars().count(),
                LIMIT_BAR_CELLS - filled
            );
            assert!(line.spans[3].content.chars().all(|ch| ch == LIMIT_BAR_CELL));
        }
        let bar = |percent| meter_row_text(percent, &theme).0;
        // The number is right-aligned in its own column, so a two- and a
        // three-digit percentage keep the same column.
        assert!(bar(99).contains("]  99% left"), "{}", bar(99));
        assert!(bar(100).contains("] 100% left"), "{}", bar(100));
    }

    /// Colour states the allowance that remains, from theme tokens only.
    #[test]
    fn the_bar_fill_colour_tracks_the_allowance_that_remains() {
        let theme = Theme::dark();
        for (percent, expected) in [
            (100_u8, theme.success),
            (50, theme.success),
            (49, theme.warning),
            (20, theme.warning),
            (19, theme.error),
            (1, theme.error),
        ] {
            assert_eq!(
                meter_row_text(percent, &theme).1,
                Some(expected),
                "percent {percent}"
            );
        }
    }

    /// An exhausted allowance is the one state that must stand out.
    ///
    /// At 0% left there are no fill cells, so colouring only the fill left the
    /// row in ordinary body colour — visually identical to a healthy neutral
    /// row. The empty track carries the colour instead, and the percentage
    /// always states its own health.
    #[test]
    fn an_exhausted_limit_is_coloured_rather_than_left_neutral() {
        let theme = Theme::dark();
        let styles = |percent: u8| {
            let meter = UsageMeterView {
                label: "Weekly limit".to_owned(),
                group: None,
                reading: UsageMeterReading::Percent {
                    floored_percent: 100 - percent,
                    exact_percent: "0".to_owned(),
                    percent_left: Some(percent),
                },
                resets_at: None,
            };
            let columns = super::LimitsColumns {
                label: meter.label.chars().count(),
                amount: 0,
            };
            let line = super::meter_row(
                &meter,
                "",
                &columns,
                &theme,
                theme.slash_item,
                theme.slash_item_help,
            );
            // spans: label, "[", fill, track, "]", " NNN%", " left"
            (
                line.spans[3].style,
                line.spans[5].style,
                line.spans[3].content.to_string(),
            )
        };

        let (track, number, cells) = styles(0);
        assert_eq!(
            cells.chars().count(),
            LIMIT_BAR_CELLS,
            "the bar is all track"
        );
        assert_eq!(track, theme.error, "an empty bar is coloured, not muted");
        assert_eq!(number, theme.error, "and so is `0%`");

        // A healthy bar keeps its darker stippled track; only the fill and
        // the number carry the state.
        let (track, number, _) = styles(99);
        assert_eq!(track, theme.bar_track);
        assert_eq!(number, theme.success);

        let (_, number, _) = styles(30);
        assert_eq!(number, theme.warning);
    }

    #[test]
    fn percent_left_falls_back_to_the_complement_of_the_used_percent() {
        assert_eq!(percent_left_of(0, None), 100);
        assert_eq!(percent_left_of(22, None), 78);
        assert_eq!(percent_left_of(100, None), 0);
        assert_eq!(percent_left_of(22, Some(77)), 77);
        assert_eq!(percent_left_of(0, Some(200)), 100);
    }

    #[test]
    fn an_unknown_cost_is_never_printed_as_zero_dollars() {
        let lines = session_ledger_lines(&limits_fixture());
        let cost = lines
            .iter()
            .find(|line| line.contains("Cost:"))
            .expect("the ledger reports a cost row");
        assert!(cost.contains("not available"));
        assert!(!cost.contains("$0"));
        assert!(lines[0].contains("1,234,567 (1,000,000 cached)"));
    }

    #[test]
    fn a_stale_limits_projection_states_its_age() {
        let theme = Theme::dark();
        let mut stale = limits_fixture();
        stale.stale_age_seconds = Some(95);
        let lines = text_of(&limits_lines(
            &stale,
            &LimitsChrome {
                refresh: None,
                refresh_failure_notes: &[],
                read_note: None,
                latest_turn: None,
            },
            120,
            &theme,
        ));
        assert!(
            lines
                .iter()
                .any(|line| line == "Stale · this projection is 1m35s old")
        );
    }

    fn session_fixture() -> SessionInfoView {
        SessionInfoView {
            host_version: "0.1.0".to_owned(),
            session_id: "sid-123abc".to_owned(),
            working_dir: "/home/user/project".to_owned(),
            root_template: None,
            root_model: Some("gpt-5.6-luna".to_owned()),
            root_effort: Some("medium".to_owned()),
            thinking: "reasoning summaries (auto) requested at medium".to_owned(),
            tool_policy: Some("Code Mode (native-ptc)".to_owned()),
            admitted_options: vec!["extra = priority".to_owned()],
            api_kernel: Some("Responses".to_owned()),
            turn_number: Some(5),
            tree_epoch: None,
            context: Some(crate::tui::presentation::SessionContextView {
                reported_window: Some(1_000_000),
                admitted_input: Some(1_000_000),
                evidence: "reference-client-envelope".to_owned(),
            }),
        }
    }

    fn live() -> SessionLive<'static> {
        SessionLive {
            context: Some("36.7k / 1.0m tokens (3.7%)".to_owned()),
            queue: "0 queued · live auto-drain".to_owned(),
            tree_epoch: Some("live"),
            build: "plak-test-build".to_owned(),
        }
    }

    #[test]
    fn the_session_tab_names_the_running_build() {
        let rows = session_rows(&session_fixture(), &live());
        assert_eq!(rows[1].label, "Build");
        assert_eq!(rows[1].value, "plak-test-build");
    }

    #[test]
    fn session_rows_state_absent_fields_and_copy_as_label_value_lines() {
        let rows = session_rows(&session_fixture(), &live());
        let copy = session_copy_text(&rows);
        assert!(copy.contains("Session ID: sid-123abc"));
        assert!(copy.contains("Working directory: /home/user/project"));
        assert!(copy.contains("Tool policy: Code Mode (native-ptc)"));
        assert!(copy.contains("API kernel: Responses"));
        assert!(copy.contains("Context: 36.7k / 1.0m tokens (3.7%)"));
        assert!(copy.contains("Tree epoch: live"));
        assert!(
            !copy.contains("Binding:"),
            "root model/effort are the root-agent items; no duplicate Binding row\n{copy}"
        );
        assert_eq!(copy.lines().count(), rows.len());
    }

    /// Before the live gauge lands, the host's admitted envelope states the
    /// window; the host's tree epoch wins over the chrome's snapshot label.
    #[test]
    fn session_rows_fall_back_to_the_host_envelope_and_epoch() {
        let mut info = session_fixture();
        info.tree_epoch = Some("tree_terminal".to_owned());
        let mut live = live();
        live.context = None;
        let copy = session_copy_text(&session_rows(&info, &live));
        assert!(
            copy.contains(
                "Context: 1.0m tokens admitted · window 1.0m · reference-client-envelope"
            ),
            "{copy}"
        );
        assert!(copy.contains("Tree epoch: tree_terminal"), "{copy}");
        info.context = None;
        let copy = session_copy_text(&session_rows(&info, &live));
        assert!(copy.contains("Context: not reported"), "{copy}");
    }

    /// The operator's complaint: `Binding` repeated the model and effort rows,
    /// and `Root template` repeated the model. Neither may come back.
    #[test]
    fn the_session_cluster_carries_no_duplicate_rows() {
        let mut info = session_fixture();
        // A template equal to the model is the model line; it earns no row.
        info.root_template.clone_from(&info.root_model);
        let rows = session_rows(&info, &live());
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert!(!labels.contains(&"Binding"), "{labels:?}");
        assert!(!labels.contains(&"Root template"), "{labels:?}");
        assert!(!labels.contains(&"Template"), "{labels:?}");
        for (index, label) in labels.iter().enumerate() {
            assert!(
                !labels[..index].contains(label),
                "{label} is listed twice: {labels:?}"
            );
        }
        let duplicates = rows
            .iter()
            .filter(|row| row.value == "gpt-5.6-luna")
            .count();
        assert_eq!(duplicates, 1, "the model is stated exactly once: {rows:?}");

        // A template that differs from the model earns exactly one row.
        info.root_template = Some("reviewer".to_owned());
        let rows = session_rows(&info, &live());
        let templates: Vec<&SessionRow> =
            rows.iter().filter(|row| row.label == "Template").collect();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].value, "reviewer");
    }

    #[test]
    fn compact_rows_cluster_and_every_row_has_a_click_target() {
        let theme = Theme::dark();
        let rows = session_rows(&session_fixture(), &live());
        let (lines, hits) = session_lines(&rows, &theme);
        assert_eq!(hits.len(), rows.len(), "every row is clickable");
        let rendered = text_of(&lines);
        for (index, line) in &hits {
            assert!(rendered[*line].contains(&rows[*index].value));
        }
        // The runtime cluster has no blank rows inside it.
        let model = hits
            .iter()
            .find(|(index, _)| rows[*index].label == "Model")
            .expect("Model row")
            .1;
        let effort = hits
            .iter()
            .find(|(index, _)| rows[*index].label == "Effort")
            .expect("Effort row")
            .1;
        assert_eq!(effort, model + 1, "consecutive compact rows are adjacent");
    }
}
