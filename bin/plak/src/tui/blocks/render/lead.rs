//! Lead rows: the header row of a tool, item or group block, its summary
//! row and the cell footer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use super::detail::{ItemFacts, push_item_detail};
use super::item_row::{MIN_SUBJECT_CELLS, command_spans, item_row, truncate_spans};
use super::{assistant_markdown_theme, cells_u16};
use crate::tui::blocks::{
    CellTail, RowPlan, ScrollBlock, compose_program_language, detail_rail, format_group_elapsed,
    milestone_has_detail, milestone_parts, row_indent, thinking_has_detail, thinking_summary,
    tool_is_running,
};
use crate::tui::markdown::RowCopy;
use crate::tui::presentation::ActivityKind;
use crate::tui::theme::Theme;

/// One clickable one-line row at the top of a block: a group summary, a
/// merged row, an item, a thinking or a milestone row.
pub(crate) struct LeadRow {
    /// Rail and indent before the glyph.
    pub(crate) rail: &'static str,
    glyph: (&'static str, Style),
    label: Vec<Span<'static>>,
    /// A failure count the cut never hides (`(1 failed)`).
    problems: Option<String>,
    meta: Vec<String>,
    /// Cells the label keeps before a meta part drops.
    label_floor: usize,
    /// Leading meta parts that never drop; the label is cut instead.
    kept_meta: usize,
    caret: &'static str,
    /// The whole label a copy carries: uncut, with the subject's own
    /// newlines rather than `↵`, failure count included.
    copy: String,
    thinking: bool,
}

impl LeadRow {
    pub(super) fn line(&self, theme: &Theme, width: usize) -> Line<'static> {
        let meta: Vec<&str> = self.meta.iter().map(String::as_str).collect();
        item_row(
            vec![
                Span::styled(self.rail, theme.tool_label),
                Span::styled(self.glyph.0, self.glyph.1),
                Span::styled(" ", theme.tool),
            ],
            self.label.clone(),
            self.problems
                .as_ref()
                .map(|problems| vec![Span::styled(format!(" {problems}"), theme.tool)])
                .unwrap_or_default(),
            &meta,
            (self.label_floor, self.kept_meta),
            self.caret,
            theme,
            width,
        )
    }

    /// The row's untruncated text, which a copy of the row carries.
    pub(crate) fn full_text(&self) -> String {
        if self.thinking {
            return format!("◆ {}", self.copy);
        }
        let mut text = format!("{}{} {}", self.rail, self.glyph.0, self.copy);
        // The tip slot is room for paint, not text.
        for meta in self.meta.iter().filter(|meta| !meta.trim().is_empty()) {
            text.push_str(" · ");
            text.push_str(meta);
        }
        text
    }
}

/// Cells a running item keeps free at its right edge for the tip's
/// elapsed (`59m59s` and its gap); dropped like any meta under the
/// 12-cell subject rule.
const TIP_META_SLOT: &str = "     ";

/// A label and the failure count at its end, which a cut must not hide.
fn split_problems(label: String) -> (String, Option<String>) {
    if (label.ends_with(" failed)") || label.ends_with(" interrupted)"))
        && let Some(at) = label.rfind(" (")
    {
        let problems = label[at + 1..].to_owned();
        let mut body = label;
        body.truncate(at);
        return (body, Some(problems));
    }
    (label, None)
}

fn split_meta(meta: Option<&str>) -> Vec<String> {
    meta.map(|meta| meta.split(" · ").map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The one-line rows a block starts with, in order (spec 02 §5.4).
#[allow(
    clippy::too_many_lines,
    reason = "Lead-row rendering exhaustively maps block variants to the matching measured RowPlan and themed heading."
)]
pub(crate) fn lead_rows(block: &ScrollBlock, plan: &RowPlan, theme: &Theme) -> Vec<LeadRow> {
    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::Mail {
            text,
            open,
            group_open,
        } => {
            let row = |label: String, copy: String, open: bool| LeadRow {
                rail: "  ",
                glyph: (if open { "▾" } else { "▸" }, theme.system),
                label: vec![Span::styled(label, theme.system)],
                problems: None,
                meta: Vec::new(),
                label_floor: MIN_SUBJECT_CELLS,
                kept_meta: 0,
                caret: "",
                copy,
                thinking: false,
            };
            let mut rows = Vec::new();
            if let Some(summary) = &plan.mail_summary {
                rows.push(row(summary.clone(), summary.clone(), *group_open));
            }
            if !plan.group_folded {
                rows.push(row(
                    text.lines().next().unwrap_or_default().to_owned(),
                    text.clone(),
                    *open,
                ));
            }
            rows
        }

        ScrollBlock::Tool {
            kind,
            name,
            state,
            subject,
            mode,
            wait,
            ..
        } if *kind != ActivityKind::Compact => {
            let mut rows = Vec::new();
            rows.extend(summary_row(plan, theme));
            if plan.group_folded {
                return rows;
            }
            let merged = plan.activity_action.is_some() && *kind != ActivityKind::Compose;
            let item = |plan: &RowPlan, caret| {
                let glyph = crate::tui::activity::item_glyph(state, *kind, plan);
                let glyph_style = match glyph {
                    "⠼" => theme.busy,
                    "✗" => theme.error_label,
                    _ => theme.success,
                };
                let copy = crate::tui::activity::item_label_raw(
                    name,
                    state,
                    *kind,
                    subject.as_deref(),
                    wait.as_ref(),
                    plan,
                );
                let label = crate::tui::activity::one_line(&copy);
                let (label, problems) = if let Some(command) = label.strip_prefix("$ ") {
                    (command_spans(command, theme), None)
                } else {
                    let (label, problems) = split_problems(label);
                    (vec![Span::styled(label, theme.tool)], problems)
                };
                let mut meta = split_meta(
                    crate::tui::activity::item_meta(name, state, *kind, subject.as_deref(), plan)
                        .as_deref(),
                );
                if tool_is_running(state) || plan.activity_running {
                    meta.push(TIP_META_SLOT.to_owned());
                }
                LeadRow {
                    rail: row_indent(plan),
                    glyph: (glyph, glyph_style),
                    label,
                    problems,
                    meta,
                    label_floor: MIN_SUBJECT_CELLS,
                    kept_meta: 0,
                    caret,
                    copy,
                    thinking: false,
                }
            };
            if merged {
                let caret = if plan.members_open { "▾" } else { "▸" };
                rows.push(item(plan, caret));
                if plan.members_open {
                    let own = RowPlan {
                        activity_action: None,
                        activity_running: false,
                        activity_failed: false,
                        activity_meta: None,
                        compose_child: Some(false),
                        ..plan.clone()
                    };
                    rows.push(item(&own, crate::tui::activity::item_caret(*mode)));
                }
            } else {
                rows.push(item(plan, crate::tui::activity::item_caret(*mode)));
            }
            rows
        }
        ScrollBlock::Milestone { milestone, open } => {
            let (glyph, label) = milestone_parts(milestone);
            let glyph_style = match glyph {
                "✓" => theme.success,
                "✗" => theme.error_label,
                "⚠" | "?" => theme.warning,
                _ => theme.command_program,
            };
            let caret = if !milestone_has_detail(milestone) {
                ""
            } else if *open {
                "▾"
            } else {
                "▸"
            };
            vec![LeadRow {
                rail: "  ",
                glyph: (glyph, glyph_style),
                label: vec![Span::styled(
                    crate::tui::activity::one_line(&label),
                    theme.tool,
                )],
                problems: None,
                meta: Vec::new(),
                label_floor: MIN_SUBJECT_CELLS,
                kept_meta: 0,
                caret,
                copy: label,
                thinking: false,
            }]
        }
        ScrollBlock::Thinking {
            text,
            sealed,
            elapsed_ms,
            mode,
            ..
        } => {
            let mut rows: Vec<_> = summary_row(plan, theme).into_iter().collect();
            if !plan.group_folded && (!*sealed || !text.trim().is_empty()) {
                let label = if *sealed {
                    thinking_summary(text)
                } else {
                    "Thinking…".to_owned()
                };
                rows.push(LeadRow {
                    rail: row_indent(plan),
                    glyph: (
                        if *sealed { "◆" } else { "⠼" },
                        if *sealed {
                            theme.tool_label
                        } else {
                            theme.busy
                        },
                    ),
                    label: vec![Span::styled(label.clone(), theme.user_label)],
                    problems: None,
                    meta: if *sealed {
                        elapsed_ms.map(format_group_elapsed).into_iter().collect()
                    } else {
                        Vec::new()
                    },
                    label_floor: MIN_SUBJECT_CELLS,
                    kept_meta: 0,
                    caret: if !*sealed || !thinking_has_detail(text) {
                        ""
                    } else if mode.shows_detail() {
                        "▾"
                    } else {
                        "▸"
                    },
                    copy: label,
                    thinking: true,
                });
            }
            rows
        }
        ScrollBlock::Item { text, meta } => vec![LeadRow {
            rail: row_indent(plan),
            glyph: ("·", theme.system),
            label: vec![Span::styled(
                crate::tui::activity::one_line(text),
                theme.system,
            )],
            problems: None,
            meta: meta.clone(),
            // Only a process row has meta: its job id and outcome
            // outlast the command and the duration.
            label_floor: text.split(' ').next().map_or(0, |id| id.width() + 1),
            kept_meta: 1,
            caret: "",
            copy: text.clone(),
            thinking: false,
        }],
        _ => Vec::new(),
    }
}

fn summary_row(plan: &RowPlan, theme: &Theme) -> Option<LeadRow> {
    let copy = plan.activity_summary.as_ref()?.clone();
    let (label, problems) = split_problems(crate::tui::activity::one_line(&copy));
    Some(LeadRow {
        rail: "  ",
        glyph: if plan.activity_failed {
            ("✗", theme.error_label)
        } else {
            ("✓", theme.success)
        },
        label: vec![Span::styled(label, theme.tool)],
        problems,
        meta: split_meta(plan.summary_meta.as_deref()),
        label_floor: MIN_SUBJECT_CELLS,
        kept_meta: 0,
        caret: crate::tui::activity::header_caret(plan),
        copy,
        thinking: false,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "Cell footers render completion, failure, and progress metadata under the same row budget and folding rules."
)]
pub(super) fn cell_footer_rows(
    tail: &CellTail,
    blocks: &[ScrollBlock],
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<RowCopy>) {
    let Some(ScrollBlock::Tool {
        name,
        kind,
        state,
        subject,
        detail,
        mode,
        view,
        ..
    }) = blocks.get(tail.owner)
    else {
        return (Vec::new(), Vec::new());
    };
    let rail = detail_rail(tail.rail);
    let mut rows = Vec::new();
    let mut copy = Vec::new();
    if tail.hidden_calls > 0 {
        let label = format!("… {} more", tail.hidden_calls);
        let more_rail = row_indent(&RowPlan {
            group_child: match tail.rail {
                "  ├─  " => Some(false),
                "  └─  " => Some(true),
                _ => None,
            },
            compose_child: Some(true),
            ..RowPlan::default()
        });
        let mut line = item_row(
            vec![Span::styled(more_rail, theme.tool_label)],
            vec![Span::styled(label, theme.tool_label)],
            Vec::new(),
            &[],
            (0, 0),
            "",
            theme,
            width,
        );
        line.alignment = Some(ratatui::layout::Alignment::Center);
        rows.push(line);
        copy.push(RowCopy::default());
    }
    if !mode.shows_detail() {
        return (rows, copy);
    }
    if let Some(source) = detail.as_deref().filter(|source| !source.trim().is_empty()) {
        let language = compose_program_language(name).unwrap_or("javascript");
        let fence = "`".repeat(
            source
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1,
        );
        let fenced = format!("{fence}{language}\n{source}\n{fence}");
        let (source_rows, source_copy) = crate::tui::markdown::render_markdown_rows(
            &fenced,
            u16::try_from(width.saturating_sub(rail.width()).max(1)).unwrap_or(u16::MAX),
            &assistant_markdown_theme(theme),
        );
        for (mut row, mut metadata) in source_rows.into_iter().zip(source_copy) {
            row.spans
                .insert(0, Span::styled(rail.clone(), theme.tool_label));
            metadata.decoration = metadata.decoration.saturating_add(cells_u16(rail.width()));
            rows.push(row);
            copy.push(metadata);
        }
    }
    let mut output = Line::from(truncate_spans(
        vec![Span::styled(
            format!("{rail}output {}", if view.output_open { "▾" } else { "▸" }),
            theme.tool_label,
        )],
        width,
    ));
    output.alignment = Some(ratatui::layout::Alignment::Right);
    rows.push(output);
    copy.push(RowCopy {
        decoration: cells_u16(rail.width()),
        ..RowCopy::default()
    });
    if view.output_open {
        let first = rows.len();
        push_item_detail(
            &mut rows,
            &ItemFacts {
                name,
                kind: *kind,
                state,
                subject: subject.as_deref(),
                detail: detail.as_deref(),
                view,
            },
            tail.rail,
            theme,
            width,
        );
        copy.extend(std::iter::repeat_n(RowCopy::default(), rows.len() - first));
    }
    (rows, copy)
}
