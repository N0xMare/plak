//! Activity's process list and live output.

use super::{App, Frame, KeyCode, Line, Paragraph, Rect, Span, UserAction, modal_chrome};
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

const ACTIVITY_TITLE: &str = "Activity";
/// The process list's footer keys at full width.
const LIST_KEYS: &str = "Enter output · s stop · S stop all";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivityTab {
    Agents,
    Processes,
}

impl App {
    pub(super) fn draw_activity_overlay(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(tab) = self.activity_tab else {
            return;
        };
        self.activity_agents_hit = None;
        self.activity_processes_hit = None;
        self.activity_close_hit = None;
        // The whole screen: no composer, shelf or hint line beneath it.
        let popup = area;
        let agent_open = tab == ActivityTab::Agents && self.viewed_agent_id().is_some();
        let body_owns_escape = match tab {
            ActivityTab::Agents => {
                agent_open
                    || self
                        .tree_navigator
                        .as_ref()
                        .is_some_and(|nav| nav.snapshot.is_some())
            }
            ActivityTab::Processes => self.process_output,
        };
        // The agent transcript owns the footer. Otherwise the root's wait
        // leads, then the tab switch.
        let hints = if agent_open {
            self.agent_activity_hints()
        } else {
            let mut hints = Vec::new();
            if let Some(need) = self.root_needs_operator() {
                hints.push(super::Hint::alert("Esc", need));
            }
            hints.push(super::Hint::new("Tab", "switch"));
            if !body_owns_escape {
                hints.push(super::Hint::new("Esc", "close"));
            }
            hints
        };
        let chrome = super::ModalChrome {
            title: ACTIVITY_TITLE,
            tabs: &["Agents", "Processes"],
            active: usize::from(tab == ActivityTab::Processes),
            hints: &hints,
            close_hover: self.activity_close_hover,
            sizing: super::ModalSizing::full_page(),
            content_max: None,
        };
        let Some(layout) = modal_chrome::render(frame, popup, &self.theme, &chrome) else {
            // Same contract as Menu: the too-small line clears every hit, so a
            // stale tab, process row or transcript track cannot fire.
            self.clear_activity_hits();
            self.clear_transcript_hit_rects();
            modal_chrome::draw_too_small(frame, popup, &self.theme, ACTIVITY_TITLE);
            return;
        };
        self.tree_popup = Some(layout.popup);
        self.activity_close_hit = Some(layout.close);
        self.draw_activity_turn_state(frame, layout.popup);
        self.activity_agents_hit = layout
            .tabs
            .iter()
            .find(|(index, _)| *index == 0)
            .map(|(_, rect)| *rect);
        self.activity_processes_hit = layout
            .tabs
            .iter()
            .find(|(index, _)| *index == 1)
            .map(|(_, rect)| *rect);
        match tab {
            ActivityTab::Agents if agent_open => {
                self.tree_hits.clear();
                self.draw_agent_activity_body(frame, layout.body);
                self.record_agent_footer_hits(layout.footer, &hints);
            }
            ActivityTab::Agents => self.draw_tree_navigator_overlay(frame, layout.body),
            ActivityTab::Processes => self.draw_processes(frame, layout.body),
        }
    }

    /// Leaves Activity, including an agent transcript open in it, for the root.
    pub(super) fn close_activity(&mut self) {
        self.activity_tab = None;
        self.tree_navigator = None;
        self.process_output = false;
        self.clear_activity_hits();
        self.close_agent_view();
        self.mark_dirty();
    }

    /// Drops Activity's hit rectangles so a frame that does not draw it
    /// cannot take a click.
    pub(super) fn clear_activity_hits(&mut self) {
        self.activity_close_hit = None;
        self.activity_agents_hit = None;
        self.activity_processes_hit = None;
        self.activity_close_hover = false;
        self.tree_popup = None;
        self.tree_hits.clear();
        self.process_hits.clear();
    }

    /// The root turn's timer, or its idle clock, on Activity's top border
    /// left of the centred title, when it fits.
    fn draw_activity_turn_state(&mut self, frame: &mut Frame<'_>, popup: Rect) {
        let now = self.now();
        let idle = self.idle_prompt_clock(now);
        self.idle_clock_painted.clone_from(&idle);
        let candidates: Vec<String> = match idle {
            Some(age) => vec![
                format!(" root · last user prompt · {age} "),
                format!(" root · {age} "),
            ],
            None => self
                .turn_started
                .filter(|_| self.busy)
                .map(|start| {
                    format!(
                        " root · {} ",
                        super::format_status_elapsed(now.saturating_duration_since(start))
                    )
                })
                .into_iter()
                .collect(),
        };
        // From the corner and one rule cell to a cell short of the title.
        let title_width = u16::try_from(ACTIVITY_TITLE.width() + 2).unwrap_or(u16::MAX);
        let title_x = 1 + popup.width.saturating_sub(2).saturating_sub(title_width) / 2;
        let room = usize::from(title_x.saturating_sub(3));
        let Some(text) = candidates.into_iter().find(|text| text.width() <= room) else {
            return;
        };
        let width = u16::try_from(text.width()).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, self.theme.chrome_dim))),
            Rect::new(popup.x + 2, popup.y, width, 1),
        );
    }

    pub(super) fn apply_process_snapshot(&mut self, rows: Arc<Vec<ProcessView>>) {
        if *self.processes == *rows {
            return;
        }
        let changed = self.processes.len() != rows.len()
            || self.processes.iter().zip(rows.iter()).any(|(old, new)| {
                old.id != new.id
                    || old.command != new.command
                    || old.owner != new.owner
                    || old.started_at != new.started_at
                    || old.status != new.status
                    || old.output != new.output
                    || old.elapsed_ms / 1000 != new.elapsed_ms / 1000
            });
        self.background_processes = rows.iter().filter(|row| row.status.is_live()).count();
        if self
            .process_selected
            .as_ref()
            .is_none_or(|id| !rows.iter().any(|row| &row.id == id))
        {
            self.process_selected = rows.first().map(|row| row.id.clone());
            self.process_scroll = 0;
        }
        self.processes = rows;
        if changed {
            self.mark_dirty();
        }
        self.retire_stale_confirm();
    }

    pub(super) fn open_processes(&mut self) {
        self.close_control_center();
        self.info_overlay = None;
        self.close_resume_menu();
        self.slash_open = false;
        self.activity_tab = Some(ActivityTab::Processes);
        self.activity_last_tab = ActivityTab::Processes;
        self.process_output = false;
        self.process_scroll = 0;
        if self.process_selected.is_none() {
            self.process_selected = self.processes.first().map(|row| row.id.clone());
        }
        self.mark_dirty();
    }

    /// `Ctrl+Q`, `/quit` and the `Ctrl+C` ladder's quit: immediate when
    /// nothing runs, otherwise the quit pop-up names what stops. A turn whose
    /// stop is already in flight has nothing left to lose, so the ladder's
    /// way out of a stuck cancel stays two presses.
    pub(super) fn request_quit(&mut self) -> Option<UserAction> {
        self.quit_arm = None;
        if self.turn_in_flight() || self.tree_live_descendants > 0 || self.background_processes > 0
        {
            self.confirm_destructive(UserAction::Quit)
        } else {
            self.begin_quit();
            Some(UserAction::Quit)
        }
    }

    /// A root turn runs and no stop of it is in flight.
    pub(super) const fn turn_in_flight(&self) -> bool {
        self.busy && !self.cancel_pending
    }

    /// Keys on the Processes tab. `s` and `S` open the destructive
    /// confirmation pop-up; nothing here stops a process directly.
    pub(super) fn handle_process_key(&mut self, code: KeyCode) -> Option<UserAction> {
        match code {
            KeyCode::Esc if self.process_output => self.process_output = false,
            KeyCode::Esc => self.close_activity(),
            KeyCode::Tab | KeyCode::BackTab => {
                // Coming back to Agents shows the list, not a transcript that
                // was open when this tab was left.
                self.close_agent_transcript_onto_list();
                return self.open_tree_navigator();
            }
            KeyCode::Enter if self.process_selected.is_some() => {
                self.process_output = true;
                self.process_scroll = 0;
            }
            KeyCode::Char('s') if !self.process_output => {
                if let Some(id) = self
                    .selected_process()
                    .filter(|row| row.status == ProcessStatus::Running)
                    .map(|row| row.id.clone())
                {
                    return self.confirm_destructive(UserAction::StopProcesses {
                        id: Some(id),
                        interrupt: false,
                    });
                }
            }
            KeyCode::Char('S') if !self.process_output && self.background_processes > 0 => {
                return self.confirm_destructive(UserAction::StopProcesses {
                    id: None,
                    interrupt: false,
                });
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_process(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_process(1),
            KeyCode::PageUp => self.move_process(-10),
            KeyCode::PageDown => self.move_process(10),
            KeyCode::Home => {
                self.process_scroll = 0;
                if !self.process_output {
                    self.process_selected = self.processes.first().map(|row| row.id.clone());
                }
            }
            KeyCode::End => {
                self.process_scroll = usize::MAX;
                if !self.process_output {
                    self.process_selected = self.processes.last().map(|row| row.id.clone());
                }
            }
            _ => {}
        }
        self.mark_dirty();
        None
    }

    fn selected_process(&self) -> Option<&ProcessView> {
        self.processes
            .iter()
            .find(|row| Some(&row.id) == self.process_selected.as_ref())
    }

    fn move_process(&mut self, delta: isize) {
        if self.process_output {
            self.process_scroll = self.process_scroll.saturating_add_signed(delta);
        } else if !self.processes.is_empty() {
            let selected = self
                .processes
                .iter()
                .position(|row| Some(&row.id) == self.process_selected.as_ref())
                .unwrap_or(0);
            let next = selected
                .saturating_add_signed(delta)
                .min(self.processes.len() - 1);
            self.process_selected = Some(self.processes[next].id.clone());
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one overlay projection owns rows, output and matching hit geometry"
    )]
    pub(super) fn draw_processes(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.height < 2 {
            return;
        }
        let heading = u16::from(self.process_output && area.height > 3);
        if heading > 0 {
            let command = self
                .selected_process()
                .map_or("process unavailable".to_owned(), |row| {
                    format!("{} · {}", row.owner, row.command)
                });
            frame.render_widget(
                Paragraph::new(crate::cli_output::safe_terminal_text(&command, 4096))
                    .style(self.theme.slash_item),
                Rect::new(area.x, area.y, area.width, 1),
            );
        }
        let body = Rect::new(
            area.x,
            area.y + heading,
            area.width,
            area.height - 1 - heading,
        );
        let visible = usize::from(body.height);
        self.process_hits.clear();
        let lines = if self.process_output {
            let output = self
                .selected_process()
                .map_or("process is no longer available", |row| row.output.as_str());
            let lines = output
                .lines()
                .flat_map(|line| {
                    super::blocks::wrap_plain_rows(
                        &crate::cli_output::safe_terminal_text(line, 4096),
                        usize::from(body.width.max(1)),
                    )
                })
                .map(|row| row.text)
                .collect::<Vec<_>>();
            let total = lines.len();
            self.process_scroll = self.process_scroll.min(total.saturating_sub(visible));
            lines
                .into_iter()
                .skip(self.process_scroll)
                .take(visible)
                .map(|line| Line::from(Span::styled(line, self.theme.slash_item)))
                .collect::<Vec<_>>()
        } else {
            let selected = self
                .processes
                .iter()
                .position(|row| Some(&row.id) == self.process_selected.as_ref())
                .unwrap_or(0);
            self.process_scroll = self
                .process_scroll
                .min(selected)
                .max(selected.saturating_add(1).saturating_sub(visible));
            let spinner = self.status_spinner();
            let status_of = |status: &ProcessStatus| match status {
                ProcessStatus::Running => spinner.to_owned(),
                ProcessStatus::Stopping => "stopping".into(),
                ProcessStatus::Exited(Some(0)) => "✓".into(),
                ProcessStatus::Exited(Some(code)) => format!("✗ {code}"),
                ProcessStatus::Exited(None) => "exited".into(),
                ProcessStatus::Stopped => "stopped".into(),
                ProcessStatus::OutcomeUnknown => "outcome unknown".into(),
            };
            let elapsed_of =
                |ms: u64| super::format_status_elapsed(std::time::Duration::from_millis(ms));
            // Columns are measured over every row, so they hold while scrolling.
            let width = |text: &str| UnicodeWidthStr::width(text);
            let column = |cell: &dyn Fn(&ProcessView) -> String| {
                self.processes
                    .iter()
                    .map(|row| width(&cell(row)))
                    .max()
                    .unwrap_or(0)
            };
            let status_width = column(&|row| status_of(&row.status));
            let owner_width = column(&|row| row.owner.clone());
            let elapsed_width = column(&|row| elapsed_of(row.elapsed_ms));
            let mut lines = Vec::new();
            for (index, row) in self
                .processes
                .iter()
                .enumerate()
                .skip(self.process_scroll)
                .take(visible)
            {
                let pad = |text: String, to: usize| {
                    let fill = " ".repeat(to.saturating_sub(width(&text)));
                    (text, fill)
                };
                let (status, status_fill) = pad(status_of(&row.status), status_width);
                let (owner, owner_fill) = pad(row.owner.clone(), owner_width);
                let (elapsed, elapsed_fill) = pad(elapsed_of(row.elapsed_ms), elapsed_width);
                let lead =
                    format!("{status}{status_fill} {owner}{owner_fill} {elapsed_fill}{elapsed} ");
                let command = crate::tui::picker::clip(
                    &crate::cli_output::safe_terminal_text(&row.command, 4096),
                    usize::from(body.width).saturating_sub(width(&lead)),
                );
                let style = if index == selected {
                    self.theme.slash_selected
                } else {
                    self.theme.slash_item
                };
                lines.push(Line::from(Span::styled(format!("{lead}{command}"), style)));
                self.process_hits.push((
                    row.id.clone(),
                    Rect::new(
                        body.x,
                        body.y + u16::try_from(lines.len() - 1).unwrap_or(0),
                        body.width,
                        1,
                    ),
                ));
            }
            if self.processes.is_empty() {
                lines.push(Line::from(" No background processes."));
            }
            lines
        };
        frame.render_widget(Paragraph::new(lines), body);
        let footer = if self.process_output {
            "↑/↓ PgUp/PgDn Home/End scroll · Esc back"
        } else if LIST_KEYS.width() <= usize::from(area.width) {
            LIST_KEYS
        } else {
            // A narrow footer keeps every key, short and still clickable.
            "Enter · s · S"
        };
        let footer_style = self.theme.slash_item_help;
        let footer_row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        if !self.process_output {
            let labels: &[(&str, KeyCode)] = if footer == LIST_KEYS {
                &[
                    ("s stop", KeyCode::Char('s')),
                    ("S stop all", KeyCode::Char('S')),
                ]
            } else {
                &[("s", KeyCode::Char('s')), ("S", KeyCode::Char('S'))]
            };
            self.key_label_hits
                .extend(super::destructive_confirm::key_label_hits(
                    footer,
                    footer_row.x,
                    footer_row.y,
                    labels,
                ));
        }
        frame.render_widget(
            Paragraph::new(Span::styled(footer, footer_style)),
            footer_row,
        );
    }
}
