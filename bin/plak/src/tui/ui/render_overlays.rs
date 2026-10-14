//! Resume and topology navigator overlays.

use super::{
    App, Frame, Hint, Line, ModalChrome, ModalSizing, Paragraph, Rect, ResumeMenu, Span, Style,
    modal_chrome, picker, resume_controls_suffix,
};
use crate::tui::resume_pane::ResumeRow as _;

impl App {
    /// Footer hints for the resume picker.
    ///
    /// There is no delete intent in the product today, so no `d delete` hint is
    /// advertised: a footer names effects the key actually has (§5.5).
    /// The picker's footer (spec 02 §5.7).
    pub(super) fn resume_hints() -> Vec<Hint> {
        vec![
            Hint::new("↑↓", "select"),
            Hint::new("Enter", "resume"),
            Hint::new("Esc", "close"),
        ]
    }

    /// The floating panel under the list: the full opening prompt the row had
    /// to clip, and when this session started against what it resumes under.
    pub(super) fn resume_description(menu: &ResumeMenu) -> Option<modal_chrome::DescriptionPanel> {
        let entry = menu.selected_entry()?;
        let controls = resume_controls_suffix([entry.display_effort(), entry.display_extra()]);
        Some(modal_chrome::DescriptionPanel {
            title: entry.short_id(),
            body: entry.display_summary(),
            value: Some(format!(
                "started {} · {}{controls}",
                menu.started_date(entry),
                entry.display_binding(),
            )),
        })
    }

    pub(super) fn draw_resume_overlay(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.resume_menu.is_none() {
            self.resume_popup = None;
            self.resume_close_hit = None;
            self.resume_close_hover = false;
            self.resume_hits.clear();
            return;
        }
        let theme = self.theme.clone();
        let hints = Self::resume_hints();
        let chrome = ModalChrome {
            title: "Resume session",
            tabs: &[],
            active: 0,
            hints: &hints,
            close_hover: self.resume_close_hover,
            sizing: ModalSizing::resume_picker(),
            content_max: None,
        };
        let Some(layout) = modal_chrome::render(frame, area, &theme, &chrome) else {
            // Below the refuse-to-draw floor nothing paints and every hit rect
            // is cleared, so a stale click cannot fire (§5.5).
            self.resume_popup = None;
            self.resume_close_hit = None;
            self.resume_close_hover = false;
            self.resume_hits.clear();
            return;
        };
        self.resume_popup = Some(layout.popup);
        self.resume_close_hit = Some(layout.close);
        self.resume_hits.clear();
        let body = layout.body;
        if body.width == 0 || body.height == 0 {
            return;
        }
        let Some(menu) = self.resume_menu.as_ref() else {
            return;
        };
        if menu.entries.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " No sessions in this workspace yet.",
                    theme.slash_item_help,
                )))
                .style(Style::default().bg(theme.menu_bg)),
                body,
            );
            return;
        }

        let panel = Self::resume_description(menu);
        let (rows, floating) = modal_chrome::split_description(body, panel.as_ref());
        let selected = menu.selected.min(menu.entries.len() - 1);
        let visible = usize::from(rows.height).max(1);
        // Keep the focused row in view without moving it: the window slides,
        // the selection does not.
        let start = selected
            .saturating_sub(visible / 2)
            .min(menu.entries.len().saturating_sub(visible));
        let mut row_hits = Vec::with_capacity(visible);
        for (offset, index) in (start..menu.entries.len()).take(visible).enumerate() {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            let (Some(label), Some(entry)) = (menu.row_label(index), menu.entries.get(index))
            else {
                break;
            };
            let row = picker::PickerRow {
                glyph: "◆",
                label,
                // Half the row is the value column's ceiling, so a long model
                // id can never swallow the summary beside it (§5.5).
                value: picker::clip(
                    &entry.display_binding_for_width(rows.width),
                    usize::from(rows.width / 2).max(8),
                ),
                description: String::new(),
                pill: None,
                // The binding is what the row resumes under, not a choice being
                // made here, so it stays recessive even under the focus band.
                recessive: true,
            };
            row_hits.push(picker::render_row(
                frame,
                Rect {
                    x: rows.x,
                    y: rows.y.saturating_add(offset),
                    width: rows.width,
                    height: 1,
                },
                &theme,
                &row,
                index,
                index == selected,
                index == selected || self.resume_hover == Some(index),
            ));
        }
        self.resume_hits = row_hits;
        if let (Some(rect), Some(panel)) = (floating, panel) {
            modal_chrome::render_description(frame, rect, &theme, &panel);
        }
    }
    #[allow(
        clippy::too_many_lines,
        reason = "tree + transcript + confirm panes share one overlay"
    )]
    pub(super) fn draw_tree_navigator_overlay(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.tree_hits.clear();
        let Some(nav) = &self.tree_navigator else {
            let text = self.agents_unavailable.as_ref().map_or_else(
                || "opening…".to_owned(),
                |reason| format!("Sub-agents are unavailable: {reason}"),
            );
            let rows = picker::wrap(&text, usize::from(area.width.max(1)));
            frame.render_widget(
                Paragraph::new(rows.join("\n")).style(self.theme.slash_item_help),
                area,
            );
            return;
        };
        let inner = if nav.snapshot.is_none() {
            Rect {
                y: area.y.saturating_add(1),
                height: area.height.saturating_sub(1),
                ..area
            }
        } else {
            area
        };
        let mut lines = Vec::new();
        if nav.snapshot.is_none() {
            lines.push(Line::from(Span::styled(
                " requesting tree snapshot… ",
                self.theme.slash_item_help,
            )));
        } else if nav
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.nodes.is_empty())
        {
            lines.push(Line::from(Span::styled(
                " No agents in tree.",
                self.theme.slash_item_help,
            )));
        } else {
            let spinner = (!self.render_prefs.reduced_motion).then(|| self.counter_spinner(0));
            let owned = self.running_processes_by_owner();
            let now = self.agents_now();
            let Some(list) = self.tree_navigator.as_mut().map(|nav| {
                nav.set_now(now);
                nav.frame(
                    inner.width,
                    inner.height,
                    spinner,
                    &owned,
                    self.composer_chrome.profile.as_ref(),
                )
            }) else {
                return;
            };
            lines.push(Line::from(Span::styled(
                list.header.clone(),
                self.theme.slash_item_help,
            )));
            let mut hits = Vec::new();
            for (offset, row) in list.rows.iter().enumerate() {
                lines.push(agent_row_line(
                    &self.theme,
                    &row.cells,
                    row.selected,
                    row.dim,
                ));
                let y = inner
                    .y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
                hits.push((row.id.clone(), Rect::new(inner.x, y, inner.width, 1)));
            }
            let used = 1 + list.rows.len() + list.inspector.len();
            let footer_rows = crate::tui::tree_navigator::wrap_footer_hint(
                &list.footer,
                inner.width,
                crate::tui::tree_navigator::footer_max_rows(inner.height),
            );
            // Rows sit under the header, the inspector and footer at the bottom.
            let pad = usize::from(inner.height)
                .saturating_sub(used)
                .saturating_sub(footer_rows.len());
            lines.extend(std::iter::repeat_n(Line::default(), pad));
            for line in &list.inspector {
                lines.push(Line::from(Span::styled(
                    format!(" {line}"),
                    self.theme.slash_item_help,
                )));
            }
            for row in footer_rows {
                let y = inner
                    .y
                    .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
                self.key_label_hits
                    .extend(super::destructive_confirm::key_label_hits(
                        &row,
                        inner.x,
                        y,
                        if list.footer.contains("interrupt") {
                            &[
                                ("r resumes", super::KeyCode::Char('r')),
                                ("i interrupt", super::KeyCode::Char('i')),
                                ("c close", super::KeyCode::Char('c')),
                                ("s stop all", super::KeyCode::Char('s')),
                            ]
                        } else {
                            &[
                                ("r resume", super::KeyCode::Char('r')),
                                ("i", super::KeyCode::Char('i')),
                                ("c", super::KeyCode::Char('c')),
                                ("s", super::KeyCode::Char('s')),
                            ]
                        },
                    ));
                lines.push(Line::from(Span::styled(row, self.theme.footer)));
            }
            self.tree_hits = hits;
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(self.theme.menu_bg)),
            inner,
        );
    }
}

/// One Agents row as painted: rails, body and attention mark. The Activity
/// list and the agent view's bar share it.
pub(super) fn agent_row_line(
    theme: &crate::tui::theme::Theme,
    cells: &crate::tui::agent_list::RowCells,
    selected: bool,
    dim: bool,
) -> Line<'static> {
    let body = if selected {
        theme.slash_selected
    } else if dim {
        theme.slash_item_help
    } else {
        theme.slash_item
    };
    let mark = if selected { body } else { theme.warning };
    let mut spans = vec![
        Span::styled(cells.rails.clone(), theme.slash_item_help),
        Span::styled(cells.body.clone(), body),
    ];
    if !cells.mark.is_empty() {
        spans.push(Span::styled(cells.mark.clone(), mark));
    }
    Line::from(spans)
}
