//! Expanded item detail: arguments, output, diffs and command listings.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use super::item_row::{command_styles, prompted_command};
use super::wrap_styled_line_railed;
use crate::tui::blocks::{
    FULL_OUTPUT_PREFIX, ItemView, TOOL_DETAIL_HEAD_LINES, TOOL_DETAIL_TAIL_LINES, ToolVerb,
    detail_rail, hidden_lines_label, line_diff, mark_hidden_lines, subject_tool_verb,
    tool_is_running,
};
use crate::tui::markdown::RowCopy;
use crate::tui::presentation::ActivityKind;
use crate::tui::theme::Theme;

/// Copy metadata for one painted detail or diff row.
///
/// The first span is the visual rail (`detail_rail`) or that rail plus the
/// two-space hanging indent of a wrapped continuation. That span is
/// decoration. Everything after it, including a diff marker, is content.
pub(super) fn detail_row_copy(line: &Line<'_>, rail: &str, hanging: &str) -> RowCopy {
    let prefix = line
        .spans
        .first()
        .map(|span| span.content.as_ref())
        .unwrap_or("");
    let width = if prefix == hanging {
        hanging.width()
    } else if prefix == rail {
        rail.width()
    } else {
        0
    };
    RowCopy {
        decoration: super::cells_u16(width),
        ..RowCopy::default()
    }
}

/// One tool item's fields the detail reads.
pub(super) struct ItemFacts<'a> {
    pub(super) name: &'a str,
    pub(super) kind: ActivityKind,
    pub(super) state: &'a str,
    pub(super) subject: Option<&'a str>,
    pub(super) detail: Option<&'a str>,
    pub(super) view: &'a ItemView,
}

/// What the detail shows: the host's canonical answer, or the retained
/// preview (labelled) when the answer is not in yet or the call is no
/// longer in canonical history.
pub(super) struct DetailText<'a> {
    pub(super) input: &'a str,
    pub(super) output: &'a str,
    pub(super) input_truncated: bool,
    pub(super) output_truncated: bool,
    pub(super) preview: bool,
    /// Host-parsed sources of a web search or fetch result.
    pub(super) sources: &'a [plakous_host_protocol::views::SourceLink],
}

pub(super) fn detail_text<'a>(item: &'a ItemFacts<'a>) -> DetailText<'a> {
    use plakous_host_protocol::views::{CallDetailOutcome, CallDetailSource};
    if let Some(fetched) = item.view.fetched.as_deref()
        && let CallDetailOutcome::Detail {
            input,
            output,
            input_truncated,
            output_truncated,
            source,
            sources,
        } = fetched
    {
        let preview = *source == CallDetailSource::Preview;
        return DetailText {
            input,
            output,
            input_truncated: *input_truncated && !preview,
            output_truncated: *output_truncated && !preview,
            preview,
            sources,
        };
    }
    // The host's own preview rule: a running row's detail is its
    // arguments; a finished row's subject stands for the input and its
    // detail is the result preview.
    let running = tool_is_running(item.state);
    // A cell's retained detail is its source, drawn above; it has no
    // output preview.
    let (input, output) = if item.kind == ActivityKind::Compose {
        ("", "")
    } else if running {
        (item.detail.or(item.subject).unwrap_or_default(), "")
    } else {
        (
            item.subject.unwrap_or_default(),
            item.detail.unwrap_or_default(),
        )
    };
    // Only a settled answer, or the lack of anything to ask, names the
    // preview; a pending fetch shows the preview without the label.
    let preview = item.view.fetched.is_some();
    DetailText {
        input,
        output,
        input_truncated: false,
        output_truncated: false,
        preview,
        sources: &[],
    }
}

/// Lines past which a write's detail stops showing the content.
const WRITE_HEAD_LINES: usize = 20;

/// Input first, then the output window, every row under the item's rail
/// with a hanging indent (spec 02 §5.4).
pub(super) fn push_item_detail(
    logical: &mut Vec<Line<'static>>,
    item: &ItemFacts<'_>,
    item_rail: &str,
    theme: &Theme,
    width: usize,
) {
    let rail = detail_rail(item_rail);
    let hanging = format!("{rail}  ");
    let push = |logical: &mut Vec<Line<'static>>, line: Line<'static>| {
        logical.extend(wrap_styled_line_railed(
            &rail,
            &hanging,
            theme.tool_label,
            line,
            width.max(1),
        ));
    };
    let text = detail_text(item);
    if text.preview {
        push(
            logical,
            Line::from(Span::styled("preview", theme.chrome_dim)),
        );
    }
    let verb = item
        .view
        .operation
        .map(crate::tui::blocks::operation_verb)
        .unwrap_or_else(|| subject_tool_verb(item.name, item.kind, item.subject));
    // A code cell's source is already drawn above its detail.
    if item.kind != ActivityKind::Compose {
        for line in input_lines(verb, text.input, theme) {
            push(logical, line);
        }
        if text.input_truncated {
            push(
                logical,
                Line::from(Span::styled("… input truncated", theme.chrome_dim)),
            );
        }
    }
    if matches!(
        verb,
        ToolVerb::WebSearch | ToolVerb::XSearch | ToolVerb::Fetch
    ) && !text.sources.is_empty()
    {
        push(
            logical,
            Line::from(Span::styled("sources", theme.chrome_dim)),
        );
        for source in text.sources {
            let mut spans = Vec::new();
            if !source.title.is_empty() {
                spans.push(Span::styled(format!("{}  ", source.title), theme.tool));
            }
            spans.push(Span::styled(source.url.clone(), theme.chrome_dim));
            push(logical, Line::from(spans));
        }
    }
    let output = operator_output(item.name, verb, text.output);
    let output = output.trim_end_matches('\n');
    if output.trim().is_empty() {
        return;
    }
    push(logical, Line::from(Span::styled("┄", theme.tool_label)));
    let lines: Vec<&str> = output.split('\n').collect();
    let kept = TOOL_DETAIL_HEAD_LINES + TOOL_DETAIL_TAIL_LINES;
    let style = theme.detail_output;
    if item.view.full_output || lines.len() <= kept + 1 {
        for line in &lines {
            push(logical, Line::from(Span::styled((*line).to_owned(), style)));
        }
    } else {
        for line in &lines[..TOOL_DETAIL_HEAD_LINES] {
            push(logical, Line::from(Span::styled((*line).to_owned(), style)));
        }
        let tail_start = lines.len() - TOOL_DETAIL_TAIL_LINES;
        // A spill line names where the whole result went; folding the
        // window must never hide it.
        let spills: Vec<&str> = lines[TOOL_DETAIL_HEAD_LINES..tail_start]
            .iter()
            .copied()
            .filter(|line| line.trim_start().starts_with(FULL_OUTPUT_PREFIX))
            .collect();
        let hidden = lines.len() - kept - spills.len();
        if hidden > 0 {
            push(
                logical,
                Line::from(Span::styled(hidden_lines_label(hidden), theme.tool_label)),
            );
            if let Some(row) = logical.last_mut() {
                mark_hidden_lines(row);
            }
        }
        for line in spills {
            push(logical, Line::from(Span::styled(line.to_owned(), style)));
        }
        for line in &lines[tail_start..] {
            push(logical, Line::from(Span::styled((*line).to_owned(), style)));
        }
    }
    if text.output_truncated {
        push(
            logical,
            Line::from(Span::styled("… output truncated", theme.chrome_dim)),
        );
    }
}

fn operator_output<'a>(name: &str, verb: ToolVerb, output: &'a str) -> std::borrow::Cow<'a, str> {
    let output = crate::refusal_text::tool_error_text(
        name,
        output,
        crate::notice_text::RemedyContext {
            frontend: crate::notice_text::Frontend::Tui,
            session_id: None,
        },
    )
    .map_or_else(
        || std::borrow::Cow::Borrowed(output),
        std::borrow::Cow::Owned,
    );
    if verb == ToolVerb::Plan {
        output
            .lines()
            .map(crate::tui::blocks::plan::plan_line)
            .collect::<Vec<_>>()
            .join("\n")
            .into()
    } else {
        output
    }
}

/// The input part of an item's detail: the full command, a diff, a
/// write's first lines, or `key: value` arguments, never raw JSON.
fn input_lines(verb: ToolVerb, input: &str, theme: &Theme) -> Vec<Line<'static>> {
    let parsed = serde_json::from_str::<serde_json::Value>(input).ok();
    let Some(serde_json::Value::Object(mut args)) = parsed else {
        return match verb {
            ToolVerb::Run => command_lines(input, theme),
            _ => input
                .lines()
                .map(|line| {
                    Line::from(Span::styled(
                        if verb == ToolVerb::Plan {
                            crate::tui::blocks::plan::plan_line(line)
                        } else {
                            line.to_owned()
                        },
                        theme.tool,
                    ))
                })
                .collect(),
        };
    };
    let take_str = |args: &mut serde_json::Map<String, serde_json::Value>, keys: &[&str]| {
        keys.iter().find_map(|key| match args.get(*key) {
            Some(serde_json::Value::String(text)) => {
                let text = text.clone();
                args.remove(*key);
                Some(text)
            }
            _ => None,
        })
    };
    let mut lines = Vec::new();
    match verb {
        ToolVerb::Run => {
            let command = match args.get("cmd").or_else(|| args.get("command")) {
                Some(serde_json::Value::Array(parts)) => Some(
                    parts
                        .iter()
                        .map(|part| {
                            part.as_str()
                                .map_or_else(|| part.to_string(), str::to_owned)
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            };
            let command = command.or_else(|| take_str(&mut args, &["cmd", "command"]));
            args.remove("cmd");
            args.remove("command");
            if let Some(command) = command {
                lines.extend(command_lines(&command, theme));
            }
        }
        ToolVerb::Edit => {
            let path = take_str(&mut args, &["file_path", "path"]);
            if let Some(patch) = take_str(&mut args, &["patch", "input"]) {
                lines.extend(diff_lines(patch.lines().map(str::to_owned), theme));
            } else if let (Some(old), Some(new)) = (
                take_str(&mut args, &["old_string", "old_text"]),
                take_str(&mut args, &["new_string", "new_text"]),
            ) {
                let mut diff = Vec::new();
                if let Some(path) = &path {
                    diff.push(format!("--- {path}"));
                    diff.push(format!("+++ {path}"));
                }
                diff.extend(line_diff(&old, &new));
                lines.extend(diff_lines(diff.into_iter(), theme));
            } else if let Some(path) = path {
                lines.push(Line::from(Span::styled(path, theme.tool)));
            }
        }
        ToolVerb::Write => {
            if let Some(path) = take_str(&mut args, &["file_path", "path"]) {
                lines.push(Line::from(Span::styled(path, theme.tool)));
            }
            if let Some(content) = take_str(&mut args, &["content", "contents", "text"]) {
                let all: Vec<&str> = content.lines().collect();
                for line in all.iter().take(WRITE_HEAD_LINES) {
                    lines.push(Line::from(Span::styled(
                        format!("+{line}"),
                        theme.diff_added,
                    )));
                }
                if all.len() > WRITE_HEAD_LINES {
                    lines.push(Line::from(Span::styled(
                        format!("… {} more lines", all.len() - WRITE_HEAD_LINES),
                        theme.chrome_dim,
                    )));
                }
            }
        }
        _ => {}
    }
    push_key_values(&mut lines, &args, 0, theme);
    lines
}

/// `key: value` rows for JSON arguments; nested values indent by two.
fn push_key_values(
    lines: &mut Vec<Line<'static>>,
    args: &serde_json::Map<String, serde_json::Value>,
    depth: usize,
    theme: &Theme,
) {
    let indent = "  ".repeat(depth);
    for (key, value) in args {
        match value {
            serde_json::Value::Object(map) if !map.is_empty() => {
                lines.push(Line::from(Span::styled(
                    format!("{indent}{key}:"),
                    theme.tool_label,
                )));
                push_key_values(lines, map, depth + 1, theme);
            }
            serde_json::Value::Array(items)
                if items.iter().any(|item| item.is_object() || item.is_array()) =>
            {
                lines.push(Line::from(Span::styled(
                    format!("{indent}{key}:"),
                    theme.tool_label,
                )));
                for item in items {
                    match item {
                        serde_json::Value::Object(map) => {
                            lines.push(Line::from(Span::styled(
                                format!("{indent}  -"),
                                theme.tool_label,
                            )));
                            push_key_values(lines, map, depth + 2, theme);
                        }
                        other => lines.push(Line::from(vec![
                            Span::styled(format!("{indent}  - "), theme.tool_label),
                            Span::styled(scalar_text(other), theme.tool),
                        ])),
                    }
                }
            }
            serde_json::Value::String(text) if text.contains('\n') => {
                lines.push(Line::from(Span::styled(
                    format!("{indent}{key}:"),
                    theme.tool_label,
                )));
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("{indent}  {line}"),
                        theme.tool,
                    )));
                }
            }
            other => lines.push(Line::from(vec![
                Span::styled(format!("{indent}{key}: "), theme.tool_label),
                Span::styled(scalar_text(other), theme.tool),
            ])),
        }
    }
}

fn scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => {
            items.iter().map(scalar_text).collect::<Vec<_>>().join(", ")
        }
        other => other.to_string(),
    }
}

/// A full command, multi-line, coloured like its item row.
/// A command's detail rows: highlighted as one unit, so quoting and
/// heredocs carry across lines, with `$` and the program accent on the
/// first row only.
fn command_lines(command: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows =
        crate::tui::highlight::lines(command, Some("bash"), &command_styles(theme)).into_iter();
    let first = rows.next().unwrap_or_default();
    let first_text = command.lines().next().unwrap_or_default();
    std::iter::once(Line::from(prompted_command(first, first_text, theme)))
        .chain(rows.map(Line::from))
        .collect()
}

/// Unified-diff rows in the diff colours.
fn diff_lines(rows: impl Iterator<Item = String>, theme: &Theme) -> Vec<Line<'static>> {
    rows.map(|row| {
        let style = if row.starts_with("+++")
            || row.starts_with("---")
            || row.starts_with("@@")
            || row.starts_with("***")
        {
            theme.diff_hunk
        } else if row.starts_with('+') {
            theme.diff_added
        } else if row.starts_with('-') {
            theme.diff_removed
        } else {
            theme.tool
        };
        Line::from(Span::styled(row, style))
    })
    .collect()
}

#[cfg(test)]
mod tests;
