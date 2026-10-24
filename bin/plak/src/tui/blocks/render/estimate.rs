//! Row estimates for blocks the cache has not measured.

use unicode_width::UnicodeWidthStr as _;

use super::detail::{ItemFacts, detail_text};
use crate::tui::blocks::{
    CellTail, DisplayMode, RenderPrefs, RowPlan, ScrollBlock, TOOL_DETAIL_HEAD_LINES,
    TOOL_DETAIL_TAIL_LINES, detail_rail, mail_lines, milestone_detail, row_indent,
    thinking_detail_rail, tool_block_closes_with_blank,
};
use crate::tui::presentation::ActivityKind;

/// System rows §5.1 hides outright, so they contribute no visual rows.
///
/// Stated once because both the exact projection and the row-count
/// estimate have to agree on whether a block is drawn at all: a block the
/// estimate gives rows and the measurement gives none would shift every
/// row below it the first time the viewport reached it.
pub(super) fn system_row_is_hidden(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.starts_with("status: session=")
        || trimmed.starts_with("keybinds:")
}

/// Rows `text` plausibly wraps to at `width`, from byte length alone.
///
/// No wrapping and no allocation: this is the whole point of the estimate.
/// Byte length overcounts multi-byte text, which errs toward a taller
/// transcript — the harmless direction, because the operator can still
/// reach every row and the measurement that follows only shortens it.
fn estimate_wrapped_rows(text: &str, width: usize) -> usize {
    let width = width.max(1);
    text.split('\n')
        .map(|line| line.len().div_ceil(width).max(1))
        .fold(0usize, usize::saturating_add)
        .max(1)
}

/// Test hook for the estimator's only load-bearing property.
///
/// The estimate must agree with [`block_lines`] on whether a block draws
/// any rows at all; everything else about it is deliberately approximate.
#[cfg(test)]
pub(crate) fn estimate_rows_for_test(
    block: &ScrollBlock,
    content_width: usize,
    prefs: RenderPrefs,
) -> usize {
    estimate_block_rows(
        block,
        &RowPlan::default(),
        content_width,
        prefs,
        std::slice::from_ref(block),
    )
}

/// Cheap row-count estimate for one block's styled projection.
///
/// Agrees with [`block_lines`] on the only thing geometry correctness
/// depends on — whether the block draws any rows at all — and is
/// deliberately approximate about how many.
pub(in crate::tui::blocks) fn estimate_block_rows(
    block: &ScrollBlock,
    plan: &RowPlan,
    content_width: usize,
    prefs: RenderPrefs,
    blocks: &[ScrollBlock],
) -> usize {
    if plan.hidden {
        return 0;
    }
    let body_width = content_width.saturating_sub(2);
    match block {
        // The markdown projection pops trailing empty lines, so an empty
        // or whitespace assistant block — an empty first delta produces one
        // — draws nothing. Estimating a row for it broke the one property
        // this estimate has to hold: a block the anchor lands on vanished
        // when it was measured, and the viewport fell to the tail.
        ScrollBlock::Assistant(text) if text.trim().is_empty() => 0,
        ScrollBlock::User { text, .. }
        | ScrollBlock::Steer { text, .. }
        | ScrollBlock::Assistant(text)
        | ScrollBlock::Error(text)
        | ScrollBlock::Header(text) => estimate_wrapped_rows(text, body_width),
        ScrollBlock::Milestone { milestone, open } => {
            let detail = if *open {
                milestone_detail(milestone)
                    .iter()
                    .map(|row| estimate_wrapped_rows(row, content_width.saturating_sub(6)))
                    .fold(0usize, usize::saturating_add)
            } else {
                0
            };
            1usize
                .saturating_add(detail)
                .saturating_add(usize::from(tool_block_closes_with_blank(plan)))
        }
        ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => 0,
        ScrollBlock::Discarded { text, .. } => {
            estimate_wrapped_rows(text, body_width).saturating_add(1)
        }
        ScrollBlock::Mail { .. } => mail_lines(block, plan, content_width).len(),
        ScrollBlock::System(text) => {
            if system_row_is_hidden(text) {
                0
            } else {
                estimate_wrapped_rows(text, body_width)
            }
        }
        ScrollBlock::Item { .. } => 1 + usize::from(tool_block_closes_with_blank(plan)),
        // The reasoning, approval, and tool projections all close on one
        // blank row, which the estimate has to carry too.
        ScrollBlock::Thinking {
            text, sealed, mode, ..
        } => {
            if !prefs.show_thinking
                || (*sealed && text.trim().is_empty() && plan.activity_summary.is_none())
            {
                return 0;
            }
            let detail = if mode.shows_detail() && !plan.group_folded {
                let rail = thinking_detail_rail(plan).width();
                estimate_wrapped_rows(text, content_width.saturating_sub(rail))
            } else {
                0
            };
            detail
                .saturating_add(
                    1 + usize::from(plan.activity_summary.is_some() && !plan.group_folded),
                )
                .saturating_add(usize::from(tool_block_closes_with_blank(plan)))
        }
        ScrollBlock::Approval {
            subject,
            prompt,
            resolution,
            ..
        } => [subject, prompt, resolution]
            .into_iter()
            .flatten()
            .map(|text| estimate_wrapped_rows(text, content_width))
            .fold(2usize, usize::saturating_add),
        ScrollBlock::Tool {
            kind,
            name,
            state,
            subject,
            detail,
            mode,
            ..
        } => {
            let source = if plan.group_folded {
                0
            } else {
                plan.cell_tails
                    .iter()
                    .map(|tail| estimate_cell_footer(tail, blocks, content_width))
                    .sum::<usize>()
            };
            let header = usize::from(plan.activity_summary.is_some());
            let item_detail = if plan.group_folded
                || matches!(kind, ActivityKind::Compact | ActivityKind::Compose)
            {
                0
            } else {
                usize::from(plan.members_open).saturating_add(match mode {
                    DisplayMode::Expanded => detail
                        .as_deref()
                        .map_or(0, |text| text.lines().count().min(32))
                        .saturating_add(2),
                    DisplayMode::Collapsed => 0,
                })
            };
            let action = if plan.group_folded {
                0
            } else if *kind == ActivityKind::Compact {
                let action =
                    crate::tui::activity::action(name, state, *kind, subject.as_deref(), plan);
                estimate_wrapped_rows(
                    action.trim_start_matches(row_indent(plan)),
                    content_width.saturating_sub(4),
                )
            } else {
                1
            };
            header
                .saturating_add(action)
                .saturating_add(item_detail)
                .saturating_add(source)
                .saturating_add(usize::from(tool_block_closes_with_blank(plan)))
        }
    }
}

fn estimate_cell_footer(tail: &CellTail, blocks: &[ScrollBlock], width: usize) -> usize {
    let more = usize::from(tail.hidden_calls > 0);
    let Some(ScrollBlock::Tool {
        name,
        kind,
        state,
        subject,
        detail,
        mode: DisplayMode::Expanded,
        view,
        ..
    }) = blocks.get(tail.owner)
    else {
        return more;
    };
    let source = detail
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map_or(0, |s| s.lines().count() + 2);
    let output = if view.output_open {
        let facts = ItemFacts {
            name,
            kind: *kind,
            state,
            subject: subject.as_deref(),
            detail: detail.as_deref(),
            view,
        };
        let text = detail_text(&facts);
        let output = text.output.trim_end_matches('\n');
        if output.trim().is_empty() {
            usize::from(text.preview)
        } else {
            let count = output.split('\n').count();
            let head = TOOL_DETAIL_HEAD_LINES;
            let tail_lines = TOOL_DETAIL_TAIL_LINES;
            let windowed = !view.full_output && count > head + tail_lines + 1;
            let body_width = width.saturating_sub(detail_rail(tail.rail).width() + 2);
            let rows = output
                .split('\n')
                .enumerate()
                .filter(|(index, _)| !windowed || *index < head || *index >= count - tail_lines)
                .map(|(_, line)| estimate_wrapped_rows(line, body_width))
                .sum::<usize>();
            usize::from(text.preview)
                + 1
                + rows
                + usize::from(windowed)
                + usize::from(text.output_truncated)
        }
    } else {
        0
    };
    more + source + 1 + output
}
