//! Structured scrollback blocks for the product TUI.
//!
//! Rendering: cyan gutter for user turns (no `you:` prefix), plain assistant
//! body, quiet tool rows.
//! Composer / sticky prompt glyph matches Grok Build (`❯` U+276F).

use std::borrow::Cow;
use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui::presentation::{ActivityKind, ActivityState, McpCall};
use plakous_host_protocol::views::CallDetailOutcome;
use plakous_host_protocol::views::transcript::{
    AgentMilestone, MilestoneEvent, NoticePlacement, NoticeScope, NoticeTone, ProcessNotice,
    ProcessOutcome, ToolItem, TranscriptRow, WaitFacts,
};

mod layout;
mod model;
mod tool_verb;
mod wrap;

pub use layout::*;
pub use model::*;
pub use tool_verb::*;
pub use wrap::*;

fn offset_at(zone: &jiff::tz::TimeZone, unix_ms: u64) -> i32 {
    let Ok(seconds) = i64::try_from(unix_ms / 1_000) else {
        return 0;
    };
    let Ok(timestamp) = jiff::Timestamp::from_second(seconds) else {
        return 0;
    };
    zone.to_offset(timestamp).seconds()
}

fn local_moment(unix_ms: u64, offset_seconds: i32) -> Option<time::OffsetDateTime> {
    let offset =
        time::UtcOffset::from_whole_seconds(offset_seconds).unwrap_or(time::UtcOffset::UTC);
    let seconds = i64::try_from(unix_ms / 1_000).ok()?;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .map(|moment| moment.to_offset(offset))
}

/// A submitted result as indented lines when it is one line of compact
/// JSON, the host's cut line (`… cut at 16 KiB of 40 KiB`) kept under it.
/// A cut result is not valid JSON, so the indent follows the tokens rather
/// than parsing, and keeps the child's key order.
fn indent_result(text: &str) -> Cow<'_, str> {
    let (body, cut) = match text.rsplit_once('\n') {
        Some((body, cut)) if cut.starts_with("… cut at ") => (body, Some(cut)),
        _ => (text, None),
    };
    let body = body.trim();
    if !(body.starts_with('{') || body.starts_with('[')) || body.contains('\n') {
        return Cow::Borrowed(text);
    }
    let mut indented = indent_json(body);
    if let Some(cut) = cut {
        indented.push('\n');
        indented.push_str(cut);
    }
    Cow::Owned(indented)
}

/// Compact JSON re-indented by two spaces per level, `serde_json`'s pretty
/// layout for valid input; strings are copied as they are.
fn indent_json(json: &str) -> String {
    fn break_line(out: &mut String, depth: usize) {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    }
    let mut out = String::with_capacity(json.len().saturating_mul(2));
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = json.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|next| next.is_whitespace()).is_some() {}
                let close = if c == '{' { '}' } else { ']' };
                if let Some(close) = chars.next_if_eq(&close) {
                    out.push(close);
                } else {
                    depth += 1;
                    break_line(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                break_line(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                break_line(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

pub(crate) use crate::tui::markdown::RowCopy;

#[cfg(any(test, feature = "test-support"))]
fn block_plain_lines(
    block: &ScrollBlock,
    plan: &RowPlan,
    content_width: usize,
    prefs: RenderPrefs,
    blocks: &[ScrollBlock],
) -> Vec<String> {
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
            let mut rows = render::block_lines_with(
                block,
                plan,
                &super::theme::Theme::dark(),
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

mod render;

#[cfg(test)]
pub(super) use render::TRANSCRIPT_RENDER_CACHE_BYTES;
pub(super) use render::{LeadRow, TranscriptChange, TranscriptRenderCache, lead_rows};

#[cfg(test)]
mod tests;
