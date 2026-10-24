//! Fold plans, the plain line layout and its anchors, and row geometry.

use crate::tui::blocks::{
    DisplayMode, PROMPT_PREFIX, RenderPrefs, ScrollBlock, mail_parts, milestone_has_detail,
    thinking_has_detail, tool_is_running, wrap_plain,
};
use crate::tui::markdown::RowCopy;
use crate::tui::presentation::ActivityKind;

/// Layout metadata: for each visual line, which block index produced it (for sticky).
#[derive(Clone, Debug, Default)]
pub struct LineLayout {
    /// Visual lines as plain strings (selection-friendly).
    pub plain: Vec<String>,
    /// Block index for each visual line (`None` for blank separators).
    pub block_index: Vec<Option<usize>>,
    /// Blocks whose row count is a cheap estimate rather than a measured wrap.
    ///
    /// Their rows are blank placeholders holding geometry, never painted: the
    /// viewport promotes everything it reads to an exact measurement first.
    /// Empty for a fully measured projection.
    pub estimated: std::collections::BTreeSet<usize>,
    /// Copy metadata parallel to [`Self::plain`]: leading decoration width,
    /// unclipped code source, and language-label rows.
    pub copy: Vec<RowCopy>,
}

/// Build a line layout from blocks at `content_width` (inner width without gutters).
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn layout_blocks(blocks: &[ScrollBlock], content_width: usize) -> LineLayout {
    layout_blocks_with(blocks, content_width, RenderPrefs::default())
}

/// Plain projection under explicit §5.5 render preferences.
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn layout_blocks_with(
    blocks: &[ScrollBlock],
    content_width: usize,
    prefs: RenderPrefs,
) -> LineLayout {
    let plans = plan_rows(blocks, prefs);
    let mut plain = Vec::new();
    let mut block_index = Vec::new();
    let mut copy = Vec::new();
    let mut previous_drawn: Option<&ScrollBlock> = None;
    for bi in row_order(&plans) {
        let block = &blocks[bi];
        let lines = block_plain_lines(block, &plans[bi], content_width, prefs, blocks);
        if lines.is_empty() {
            continue;
        }
        if wants_separator(previous_drawn, block) {
            plain.push(String::new());
            block_index.push(None);
            copy.push(RowCopy::default());
        }
        for line in lines {
            plain.push(line);
            block_index.push(Some(bi));
            copy.push(RowCopy::default());
        }
        previous_drawn = Some(block);
    }
    LineLayout {
        plain,
        block_index,
        estimated: std::collections::BTreeSet::new(),
        copy,
    }
}

/// Whether a tool block closes with its own blank row (§5.8).
///
/// A Compose and an open verb-group are one rail, so the spacer follows the
/// last child rather than the head or a middle child. Every projection — the
/// plain export, the styled paint and the cheap estimate — reads this one
/// rule, so a Compose is the same height wherever it is measured.
#[must_use]
pub const fn tool_block_closes_with_blank(plan: &RowPlan) -> bool {
    !plan.activity_continues
}

/// Whether a blank row separates `block` from the block drawn before it.
///
/// Grok Build spacing: a blank row between turns, never before the first
/// drawn block. A tool or Compose row carries its own trailing blank, so it
/// takes a leading one only after text — an assistant paragraph, a prompt
/// band or an error — so a short thought or text line never glues to the tool
/// row beneath it. System notes stay compact.
#[must_use]
pub fn wants_separator(previous_drawn: Option<&ScrollBlock>, block: &ScrollBlock) -> bool {
    let Some(previous) = previous_drawn else {
        return false;
    };
    match block {
        ScrollBlock::System(_) | ScrollBlock::Header(_) | ScrollBlock::Mail { .. } => false,
        ScrollBlock::Tool { .. } | ScrollBlock::Item { .. } | ScrollBlock::Milestone { .. } => {
            matches!(
                previous,
                ScrollBlock::Assistant(_)
                    | ScrollBlock::User { .. }
                    | ScrollBlock::Steer { .. }
                    | ScrollBlock::Error(_)
            )
        }
        // Tool, thinking, milestone and approval blocks close on their own
        // blank row; a second one under them would double the gap.
        _ => !matches!(
            previous,
            ScrollBlock::Tool { .. }
                | ScrollBlock::Thinking { .. }
                | ScrollBlock::Approval { .. }
                | ScrollBlock::Milestone { .. }
        ),
    }
}

/// How one block participates in the folded transcript (§5.5, §5.8).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per independent fold/caret axis"
)]
pub struct RowPlan {
    pub(crate) mail_summary: Option<String>,
    pub(crate) activity_summary: Option<String>,
    pub(crate) activity_action: Option<String>,
    /// Right-aligned meta of a group summary line (exit codes, duration).
    pub(crate) summary_meta: Option<String>,
    /// Right-aligned meta of a cell's own line (its nested exit codes).
    pub(crate) activity_meta: Option<String>,
    pub(crate) activity_failed: bool,
    pub(crate) cell_failed: bool,
    pub(crate) activity_running: bool,
    /// The first call owns the fold even when preceding thoughts open a group.
    pub(crate) group_owner: Option<usize>,
    pub(crate) cell_tails: Vec<CellTail>,
    pub(crate) activity_continues: bool,
    /// The block contributes no rows: folded into a group or outside the
    /// cell's bounded outline.
    pub hidden: bool,
    /// Rows in this one's §5.5 run; `0` when the block is not a group head.
    pub group_count: usize,
    /// The group head is showing the run's name instead of its own subject.
    pub group_folded: bool,
    /// `Some(is_last)` when this row is drawn as a Compose child (§5.8 rails).
    pub compose_child: Option<bool>,
    pub(crate) compose_owner: Option<usize>,
    /// The row is an open §5.5 group head: the header stays drawn and the
    /// spacer belongs after the last member, not after the head.
    pub group_head_open: bool,
    /// `Some(is_last)` when this row is a member of an open §5.5 verb group.
    pub group_child: Option<bool>,
    /// A merged row listing its calls: the head's own call is drawn under it.
    pub members_open: bool,
}

/// Presentation-only footer attached to the final visible outline block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CellTail {
    pub(crate) owner: usize,
    pub(crate) hidden_calls: usize,
    pub(crate) rail: &'static str,
}

/// Fold plan for every block, in block order.
///
/// Nesting comes only from host-supplied `parent_call_id`; a lineage the TUI
/// was not given produces a flat row (§5.8).
#[must_use]
pub fn plan_rows(blocks: &[ScrollBlock], prefs: RenderPrefs) -> Vec<RowPlan> {
    crate::tui::activity::plan(blocks, prefs)
}

/// Cell outlines stay under their header even when parallel cells interleave.
pub(in crate::tui) fn row_order(plans: &[RowPlan]) -> Vec<usize> {
    row_order_range(plans, 0, plans.len())
}

pub(in crate::tui) fn row_order_range(plans: &[RowPlan], start: usize, end: usize) -> Vec<usize> {
    let mut children = std::collections::HashMap::<usize, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate().take(end).skip(start) {
        if let Some(owner) = plan.compose_owner {
            children.entry(owner).or_default().push(index);
        }
    }
    let mut order = Vec::with_capacity(end - start);
    let mut visited = std::collections::HashSet::new();
    let roots = (start..end).filter(|index| plans[*index].compose_owner.is_none());
    for root in roots.chain(start..end) {
        let mut stack = vec![root];
        while let Some(index) = stack.pop() {
            if !visited.insert(index) {
                continue;
            }
            order.push(index);
            if let Some(owned) = children.get(&index) {
                stack.extend(owned.iter().rev().copied());
            }
        }
    }
    order
}

/// What a click on one of a block's leading rows toggles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowTarget {
    /// A group summary: the group's items.
    Group,
    /// A merged row: the calls it stands for.
    Merge,
    /// An item, thinking or milestone row: its own detail.
    Item,
    /// The `… N lines hidden` row of an open item: the rest of its output.
    HiddenLines,
    CellMore(usize),
    CellOutput(usize),
    CellHiddenLines(usize),
}

/// What each of a block's leading one-line rows toggles, from its plan alone
/// (no styling), so hit-testing a frame stays cheap.
#[must_use]
pub fn lead_targets(block: &ScrollBlock, plan: &RowPlan) -> Vec<RowTarget> {
    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::Mail { .. } => {
            let mut targets = Vec::new();
            if plan.mail_summary.is_some() {
                targets.push(RowTarget::Group);
            }
            if !plan.group_folded {
                targets.push(RowTarget::Item);
            }
            targets
        }
        ScrollBlock::Tool { kind, .. } if *kind != ActivityKind::Compact => {
            let mut targets = Vec::new();
            if plan.activity_summary.is_some() {
                targets.push(RowTarget::Group);
            }
            if !plan.group_folded {
                if plan.activity_action.is_some() && *kind != ActivityKind::Compose {
                    targets.push(RowTarget::Merge);
                    if plan.members_open {
                        targets.push(RowTarget::Item);
                    }
                } else {
                    targets.push(RowTarget::Item);
                }
            }
            targets
        }
        ScrollBlock::Thinking { text, sealed, .. } => {
            let mut targets = Vec::new();
            if plan.activity_summary.is_some() {
                targets.push(RowTarget::Group);
            }
            if !plan.group_folded && *sealed && thinking_has_detail(text) {
                targets.push(RowTarget::Item);
            }
            targets
        }
        ScrollBlock::Milestone { milestone, .. } if milestone_has_detail(milestone) => {
            vec![RowTarget::Item]
        }
        _ => Vec::new(),
    }
}

/// The indent of a thought's detail rows: under its bullet, with an open
/// group's rail continued.
#[must_use]
pub fn thinking_detail_rail(plan: &RowPlan) -> String {
    format!(
        "{}  ",
        row_indent(plan).replace("├─", "│ ").replace("└─", "  ")
    )
}

/// The rail an open item's detail rows carry: the item's own rail continued,
/// then `│` under the item's label.
#[must_use]
pub fn detail_rail(item_rail: &str) -> String {
    format!("{}  │ ", item_rail.replace("├─", "│ ").replace("└─", "  "))
}

/// The row that stands for an output window's hidden middle.
#[must_use]
pub fn hidden_lines_label(hidden: usize) -> String {
    format!("… {hidden} lines hidden ▸")
}

/// An open item's hidden-lines row is marked on its styled line, where the
/// output it stands among cannot reach: cell footer actions use the other alignment markers,
/// and an explicit left alignment paints exactly like none.
pub(super) fn mark_hidden_lines(line: &mut ratatui::text::Line<'static>) {
    line.alignment = Some(ratatui::layout::Alignment::Left);
}

/// Whether a styled transcript row is an output window's hidden-lines row.
#[must_use]
pub fn is_hidden_lines_row(line: &ratatui::text::Line<'static>) -> bool {
    line.alignment == Some(ratatui::layout::Alignment::Left)
}

/// Footer actions are marked on painted lines, not inferred from output text.
#[must_use]
pub fn cell_footer_target(
    lines: &[ratatui::text::Line<'static>],
    row: usize,
    plan: &RowPlan,
    blocks: &[ScrollBlock],
) -> Option<RowTarget> {
    use ratatui::layout::Alignment;
    let expanded: Vec<_> = plan
        .cell_tails
        .iter()
        .filter(|tail| {
            matches!(
                blocks.get(tail.owner),
                Some(ScrollBlock::Tool {
                    mode: DisplayMode::Expanded,
                    ..
                })
            )
        })
        .collect();
    match lines.get(row)?.alignment? {
        Alignment::Center => {
            let more = lines[..=row]
                .iter()
                .filter(|line| line.alignment == Some(Alignment::Center))
                .count();
            let tail = plan
                .cell_tails
                .iter()
                .filter(|tail| tail.hidden_calls > 0)
                .nth(more.checked_sub(1)?)?;
            Some(RowTarget::CellMore(tail.owner))
        }
        Alignment::Right | Alignment::Left => {
            let outputs = lines[..=row]
                .iter()
                .filter(|line| line.alignment == Some(Alignment::Right))
                .count();
            let tail = expanded.get(outputs.checked_sub(1)?)?;
            if lines[row].alignment == Some(Alignment::Right) {
                Some(RowTarget::CellOutput(tail.owner))
            } else {
                Some(RowTarget::CellHiddenLines(tail.owner))
            }
        }
    }
}

/// Where the live spinner sits in a block's leading rows: `(row offset in
/// the block, glyph column)` for each row whose status glyph is the spinner
/// (spec 02 §5.4), from the block's state and plan, never from row text.
#[must_use]
pub fn spinner_cells(
    block: &ScrollBlock,
    plan: &RowPlan,
    prefs: RenderPrefs,
) -> Vec<(usize, usize)> {
    use unicode_width::UnicodeWidthStr as _;
    if plan.hidden {
        return Vec::new();
    }
    let item = |plan: &RowPlan, state: &str| {
        (tool_is_running(state) || plan.activity_running).then(|| row_indent(plan).width())
    };
    match block {
        ScrollBlock::Thinking { sealed: false, .. } if prefs.show_thinking => vec![(0, 2)],
        ScrollBlock::Tool {
            kind: ActivityKind::Compact,
            state,
            ..
        } => {
            if tool_is_running(state) {
                vec![(0, 2)]
            } else {
                Vec::new()
            }
        }
        ScrollBlock::Tool { kind, state, .. } => {
            let mut row = usize::from(plan.activity_summary.is_some());
            if plan.group_folded {
                return Vec::new();
            }
            let mut cells = Vec::new();
            cells.extend(item(plan, state).map(|column| (row, column)));
            if plan.activity_action.is_some() && *kind != ActivityKind::Compose && plan.members_open
            {
                row += 1;
                let own = RowPlan {
                    activity_action: None,
                    activity_running: false,
                    activity_failed: false,
                    activity_meta: None,
                    compose_child: Some(false),
                    ..plan.clone()
                };
                cells.extend(item(&own, state).map(|column| (row, column)));
            }
            cells
        }
        _ => Vec::new(),
    }
}

/// Head lines kept when an expanded tool body is truncated.
pub const TOOL_DETAIL_HEAD_LINES: usize = 20;

/// Tail lines kept when an expanded tool body is truncated.
pub const TOOL_DETAIL_TAIL_LINES: usize = 10;

/// Left rail for a cell's nested call or an open group member, or the plain
/// indent. A nested call inside an open group sits one rail level deeper.
#[must_use]
pub const fn row_indent(plan: &RowPlan) -> &'static str {
    match (plan.group_child, plan.compose_child) {
        (Some(false), Some(false)) => "  │   ├─  ",
        (Some(false), Some(true)) => "  │   └─  ",
        (Some(true), Some(false)) => "      ├─  ",
        (Some(true), Some(true)) => "      └─  ",
        (Some(true), None) | (None, Some(true)) => "  └─  ",
        (Some(false), None) | (None, Some(false)) => "  ├─  ",
        (None, None) => "  ",
    }
}

/// Sticky user preview: last user message fully above the viewport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickyUserPreview {
    /// Bounded rendered prompt lines.
    pub lines: Vec<String>,
    /// First transcript row of the source prompt for click-to-jump.
    pub source_start: usize,
}

///
/// Returns up to `max_lines` plain lines of that message body (with gutter).
#[must_use]
pub fn sticky_user_lines(
    blocks: &[ScrollBlock],
    layout: &LineLayout,
    visible_start: usize,
    max_lines: usize,
    content_width: usize,
) -> Option<StickyUserPreview> {
    if visible_start == 0 || max_lines == 0 {
        return None;
    }
    // Reverse-scan from the viewport edge: O(distance to last complete user block).
    let mut i = visible_start.min(layout.block_index.len());
    let (bi, source_start) = loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        let Some(bi) = layout.block_index[i] else {
            continue;
        };
        let mut start = i;
        while start > 0 && layout.block_index[start - 1] == Some(bi) {
            start -= 1;
        }
        let mut end = i + 1;
        while end < layout.block_index.len() && layout.block_index[end] == Some(bi) {
            end += 1;
        }
        if blocks.get(bi).is_some_and(ScrollBlock::is_user)
            && start < visible_start
            && end <= visible_start
        {
            break (bi, start);
        }
        i = start;
    };
    let text = blocks.get(bi)?.body();
    let w = content_width.saturating_sub(2).max(1);
    let wrapped = wrap_plain(text, w);
    let mut lines: Vec<String> = wrapped
        .iter()
        .take(max_lines)
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                format!("{PROMPT_PREFIX}{l}")
            } else {
                format!("  {l}")
            }
        })
        .collect();
    if wrapped.len() > max_lines
        && let Some(last) = lines.last_mut()
    {
        if last.chars().count() > 3 {
            *last = format!(
                "{}…",
                last.chars()
                    .take(last.chars().count().saturating_sub(1))
                    .collect::<String>()
            );
        } else {
            last.push('…');
        }
    }
    Some(StickyUserPreview {
        lines,
        source_start,
    })
}

/// The `(block, sub-row)` a transcript row belongs to.
///
/// A row number is only meaningful inside one projection: a resize re-wraps
/// every block and measuring an estimated block corrects the heights below it.
/// Converting to block space before either happens, and back afterwards, is
/// what keeps the operator's place.
///
/// A blank separator row resolves to the block above it, or to the block below
/// when the transcript opens on separators.
#[must_use]
pub fn layout_anchor(layout: &LineLayout, row: usize) -> Option<(usize, usize)> {
    let rows = layout.block_index.len();
    if rows == 0 {
        return None;
    }
    let row = row.min(rows - 1);
    // The separator belongs to the block it introduces, so look forward first;
    // only a row past the last block has to look back.
    let block = layout.block_index[row..]
        .iter()
        .flatten()
        .next()
        .or_else(|| layout.block_index[..row].iter().rev().flatten().next())
        .copied()?;
    let (start, end) = block_span(layout, block)?;
    let last = end.saturating_sub(start).saturating_sub(1);
    Some((block, row.saturating_sub(start).min(last)))
}

/// Rows one block occupies, counting the blank separator drawn above it.
///
/// The separator is charged to the block below so that turning a row into a
/// `(block, sub-row)` anchor and back again lands on the row it started from —
/// which is the whole contract [`layout_anchor`] exists to keep.
/// Test hook for [`block_span`]: whether a block draws any rows at all.
#[cfg(test)]
pub(crate) fn block_span_for_test(layout: &LineLayout, block: usize) -> Option<(usize, usize)> {
    block_span(layout, block)
}

pub(crate) fn resolve_layout_anchor(
    layout: &LineLayout,
    (block, sub_row): (usize, usize),
) -> usize {
    block_span(layout, block).map_or(0, |(start, end)| {
        start.saturating_add(sub_row).min(end.saturating_sub(1))
    })
}

pub(super) fn block_span(layout: &LineLayout, block: usize) -> Option<(usize, usize)> {
    let first = layout
        .block_index
        .iter()
        .position(|owner| *owner == Some(block))?;
    let mut start = first;
    while start > 0 && layout.block_index[start - 1].is_none() {
        start -= 1;
    }
    let end = first
        + layout.block_index[first..]
            .iter()
            .take_while(|owner| **owner == Some(block))
            .count();
    Some((start, end))
}

pub(super) fn mail_lines(block: &ScrollBlock, plan: &RowPlan, width: usize) -> Vec<String> {
    let ScrollBlock::Mail {
        text,
        open,
        group_open,
    } = block
    else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let available = width.saturating_sub(4).max(1);
    if let Some(summary) = &plan.mail_summary {
        rows.push(format!(
            "  {} {}",
            if *group_open { "▾" } else { "▸" },
            crate::tui::picker::clip(summary, available)
        ));
    }
    if plan.group_folded {
        return rows;
    }
    let first = text.lines().next().unwrap_or_default();
    rows.push(format!(
        "  {} {}",
        if *open { "▾" } else { "▸" },
        crate::tui::picker::clip(first, available)
    ));
    if *open {
        let body = mail_parts(text).map_or(text.as_str(), |(_, _, body)| body);
        rows.extend(
            wrap_plain(body, available)
                .into_iter()
                .map(|line| format!("    {line}")),
        );
    }
    rows
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn block_plain_lines(
    block: &ScrollBlock,
    plan: &RowPlan,
    content_width: usize,
    prefs: RenderPrefs,
    blocks: &[ScrollBlock],
) -> Vec<String> {
    use crate::tui::blocks::{discarded_label, format_approval_block};

    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::User { text, .. } | ScrollBlock::Steer { text, .. } => {
            // Gutter wrap; sticky uses `›` — body layout uses gutter for plain export
            let w = content_width.saturating_sub(2).max(1);
            wrap_plain(text, w)
                .into_iter()
                .map(|l| format!("┃ {l}"))
                .collect()
        }
        ScrollBlock::Assistant(text) => {
            let w = content_width.saturating_sub(2).max(1);
            wrap_plain(text, w)
                .into_iter()
                .map(|line| format!("  {line}"))
                .collect()
        }
        ScrollBlock::System(text) | ScrollBlock::Error(text) | ScrollBlock::Header(text) => {
            wrap_plain(text, content_width.saturating_sub(2).max(1))
                .into_iter()
                .map(|line| format!("  {line}"))
                .collect()
        }
        ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => Vec::new(),
        ScrollBlock::Discarded { text, thinking } => {
            let w = content_width.saturating_sub(2).max(1);
            std::iter::once(format!("  {}", discarded_label(*thinking)))
                .chain(
                    wrap_plain(text, w)
                        .into_iter()
                        .map(|line| format!("  {line}")),
                )
                .collect()
        }
        ScrollBlock::Approval {
            tool_name,
            subject,
            prompt,
            resolution,
            ..
        } => format_approval_block(
            tool_name,
            subject.as_deref(),
            prompt.as_deref(),
            resolution.as_deref(),
        )
        .into_iter()
        .flat_map(|line| wrap_plain(&line, content_width))
        .collect(),
        ScrollBlock::Tool { .. }
        | ScrollBlock::Item { .. }
        | ScrollBlock::Thinking { .. }
        | ScrollBlock::Milestone { .. }
        | ScrollBlock::Mail { .. } => {
            let mut rows = super::render::block_lines_with(
                block,
                plan,
                &crate::tui::theme::Theme::dark(),
                content_width,
                prefs,
                blocks,
            );
            if rows.last().is_some_and(|row| row.to_string().is_empty()) {
                rows.pop();
            }
            rows.into_iter()
                .map(|row| row.to_string().trim_end().to_owned())
                .collect()
        }
    }
}
