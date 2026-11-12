//! Agents tab: one searchable list of named profiles, plus the apply dialog.
//!
//! The product owns the list: the tab asks for it with `ListProfiles`, paints
//! the [`ProfilesView`] it gets back, and sends the chosen row's id. What
//! landed comes back as `ProfileApplied` or `ProfileRefused`; the `default`
//! mark (the profile brand-new sessions bind) is only ever the product's
//! answer.

use super::super::text_input::is_accepted_single_line_char;
use super::{App, KeyCode, KeyEvent, KeyModifiers, Rect, ScrollBlock, UserAction};
use crate::tui::{
    ProfileApply, ProfileOutcome, ProfileRole, ProfileRow, ProfilesView, SurfaceIntent,
};
use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

/// Everything the Agents tab and its dialog hold.
#[derive(Default)]
pub(super) struct ProfilesPane {
    /// The product's last list; `None` until the first one arrives.
    pub view: Option<ProfilesView>,
    /// Index into `view.rows` of the highlighted row.
    pub cursor: usize,
    pub filter: String,
    pub scroll: usize,
    pub hits: Vec<(usize, Rect)>,
    /// `/agents <name>` waiting on the list to resolve `name`.
    pub pending_name: Option<String>,
}

impl ProfilesPane {
    fn rows(&self) -> &[ProfileRow] {
        self.view.as_ref().map_or(&[], |view| view.rows.as_slice())
    }

    pub(super) fn visible(&self) -> Vec<usize> {
        let query = self.filter.trim().to_ascii_lowercase();
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| query.is_empty() || row.id.to_ascii_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    /// The highlighted row, only while the filter still shows it.
    fn selected(&self) -> Option<&ProfileRow> {
        self.visible()
            .contains(&self.cursor)
            .then(|| self.rows().get(self.cursor))
            .flatten()
    }

    /// Exact id first, then an alias the product listed for a row.
    fn resolve(&self, name: &str) -> Option<usize> {
        let rows = self.rows();
        rows.iter().position(|row| row.id == name).or_else(|| {
            rows.iter()
                .position(|row| row.aliases.iter().any(|alias| alias == name))
        })
    }
}

impl App {
    /// The tab opened: ask the product for its list.
    pub(super) fn request_profiles(&mut self) -> UserAction {
        self.mark_dirty();
        UserAction::Extension(SurfaceIntent::ListProfiles)
    }

    /// `/agents <name>`: resolve `name` against a fresh list, then ask.
    pub(super) fn open_profile_confirm(&mut self, name: &str) -> Option<UserAction> {
        self.profiles.pending_name = Some(name.trim().to_owned());
        self.profiles.filter.clear();
        if self.control_tab == Some(super::ControlTab::Models) {
            return Some(self.request_profiles());
        }
        self.open_control_tab(super::ControlTab::Models)
    }

    pub(super) fn apply_profiles_view(&mut self, view: ProfilesView) {
        let highlighted = self
            .profiles
            .rows()
            .get(self.profiles.cursor)
            .map(|row| row.id.clone());
        let cursor = highlighted
            .and_then(|id| view.rows.iter().position(|row| row.id == id))
            .or_else(|| {
                view.current
                    .as_ref()
                    .and_then(|current| view.rows.iter().position(|row| &row.id == current))
            })
            .unwrap_or(0);
        self.profiles.view = Some(view);
        self.profiles.cursor = cursor;
        if let Some(name) = self.profiles.pending_name.take() {
            match self.profiles.resolve(&name) {
                Some(index) => {
                    self.profiles.cursor = index;
                    self.arm_profile_confirm();
                }
                None => self.push_block(ScrollBlock::System(format!(
                    "unknown profile `{name}`; /agents lists them"
                ))),
            }
        }
        self.mark_dirty();
    }

    pub(super) fn apply_profile_applied(&mut self, name: &str, outcome: ProfileOutcome) {
        let note = match outcome {
            ProfileOutcome::Saved => {
                format!(
                    "profile {name} is the default for new sessions; existing sessions keep their own"
                )
            }
            ProfileOutcome::Forked => {
                format!(
                    "forked this session onto profile {name}; the previous session stays resumable"
                )
            }
            // The host's own notice names what kept running.
            ProfileOutcome::Switched => format!("this session continues as profile {name}"),
            ProfileOutcome::Bound => format!("profile {name} is this session's root"),
            ProfileOutcome::AlreadyActive => {
                format!("this session already runs profile {name}'s root and cards")
            }
        };
        // Only the default action moves the default; a fork or a bind changes
        // that one session (a first bind's default arrives with the list).
        if outcome == ProfileOutcome::Saved
            && let Some(view) = self.profiles.view.as_mut()
        {
            view.current = Some(name.to_owned());
        }
        self.push_block(ScrollBlock::System(note));
    }

    pub(super) fn apply_profile_refused(&mut self, name: &str, reason: &str) {
        self.push_block(ScrollBlock::Error(format!("profile {name}: {reason}")));
    }

    /// Asks how to apply the highlighted profile, in the destructive
    /// confirmation pop-up (spec 02 §5.5): Cancel, then Switch now when the
    /// root is idle with an empty queue, then Fork now when the root is idle
    /// with no running child, then Set as default. A session with no model
    /// offers Use now in place of both.
    pub(super) fn arm_profile_confirm(&mut self) {
        let Some(row) = self.profiles.selected() else {
            return;
        };
        let id = row.id.clone();
        let apply = |apply| {
            UserAction::Extension(SurfaceIntent::ApplyProfile {
                name: id.clone(),
                apply,
            })
        };
        let mut lines = Vec::new();
        let mut actions = Vec::new();
        if self.model.is_none() {
            if self.busy {
                lines.push("Use now waits until the root is idle.".to_owned());
            } else {
                lines.push("Use now makes it this session's root.".to_owned());
                actions.push(("[ Use now ]", apply(ProfileApply::Fork)));
            }
        } else {
            if self.busy || self.queue_len != 0 {
                lines.push(
                    "Switch now waits until the root is idle and nothing is queued.".to_owned(),
                );
            } else {
                lines.push(
                    "Switch now continues this session as the profile; its agents and processes keep running.".to_owned(),
                );
                actions.push(("[ Switch now ]", apply(ProfileApply::Switch)));
            }
            if self.busy || self.tree_live_descendants != 0 {
                lines.push(
                    "Fork now waits until the root is idle and no child is running.".to_owned(),
                );
            } else {
                lines.push(
                    "Fork now branches this session onto it; this one stays resumable.".to_owned(),
                );
                actions.push(("[ Fork now ]", apply(ProfileApply::Fork)));
            }
        }
        if self.host_mode {
            lines.push("Set as default is unavailable on a host attachment.".into());
            lines.push(format!("On the host: `plak config set root.profile {id}`."));
        } else {
            lines.push(
                "Set as default makes it the profile new sessions start with; existing sessions keep their own."
                    .to_owned(),
            );
            actions.push(("[ Set as default ]", apply(ProfileApply::Default)));
        }
        self.open_confirm(format!("Apply profile {id}?"), lines, actions);
    }

    pub(super) fn handle_agents_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return None;
        }
        match key.code {
            KeyCode::Up => self.move_profile_cursor(-1),
            KeyCode::Down => self.move_profile_cursor(1),
            KeyCode::PageUp => self.move_profile_cursor(-8),
            KeyCode::PageDown => self.move_profile_cursor(8),
            KeyCode::Home => self.move_profile_cursor(-isize::MAX / 4),
            KeyCode::End => self.move_profile_cursor(isize::MAX / 4),
            KeyCode::Backspace => {
                self.profiles.filter.pop();
                self.clamp_profile_cursor();
                self.mark_dirty();
            }
            KeyCode::Char(ch) if is_accepted_single_line_char(ch) => {
                self.profiles.filter.push(ch);
                self.clamp_profile_cursor();
                self.mark_dirty();
            }
            KeyCode::Enter => self.arm_profile_confirm(),
            _ => return None,
        }
        None
    }

    pub(super) fn move_profile_cursor(&mut self, delta: isize) {
        let visible = self.profiles.visible();
        if visible.is_empty() {
            return;
        }
        let pos = visible
            .iter()
            .position(|index| *index == self.profiles.cursor)
            .unwrap_or(0);
        let next = pos.saturating_add_signed(delta).min(visible.len() - 1);
        self.profiles.cursor = visible[next];
        self.mark_dirty();
    }

    pub(super) fn clamp_profile_cursor(&mut self) {
        let visible = self.profiles.visible();
        if let Some(first) = visible.first()
            && !visible.contains(&self.profiles.cursor)
        {
            self.profiles.cursor = *first;
        }
    }

    pub(super) fn clear_profile_filter(&mut self) {
        self.profiles.filter.clear();
        self.clamp_profile_cursor();
        self.mark_dirty();
    }

    pub(super) fn hover_profile(&mut self, index: usize) {
        if self.profiles.cursor != index {
            self.profiles.cursor = index;
            self.mark_dirty();
        }
    }

    /// A click moves the cursor; a double-click opens the apply dialog
    /// exactly as Enter does.
    pub(super) fn click_profile(&mut self, index: usize, double: bool) {
        self.profiles.cursor = index;
        self.mark_dirty();
        if double {
            self.arm_profile_confirm();
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Profile drawing keeps list selection, detail rows, and hit rectangles synchronized across narrow and wide layouts."
    )]
    pub(super) fn draw_agent_profiles(&mut self, frame: &mut Frame<'_>, body: Rect) {
        self.profiles.hits.clear();
        let visible = self.profiles.visible();
        let row = self.profiles.selected().cloned();
        let diagnostics = self
            .profiles
            .view
            .as_ref()
            .map(|view| view.diagnostics.clone())
            .unwrap_or_default();
        let (body, wide_detail) = super::modal_chrome::split_wide_detail(body)
            .map_or((body, None), |(list, detail)| (list, Some(detail)));
        let panel_rows = row.as_ref().map_or(0, detail_line_count);
        let panel_height = u16::try_from(panel_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(2);
        let reserve = if wide_detail.is_none()
            && row.is_some()
            && body.height > panel_height.saturating_add(4)
        {
            panel_height
        } else {
            0
        };
        let list_height = body.height.saturating_sub(reserve);
        // `/` is not a search key here; every printable key filters.
        let search = if self.profiles.filter.is_empty() {
            "type to filter".to_owned()
        } else {
            format!("filter: {}█", self.profiles.filter)
        };
        let mut lines = vec![
            Line::from(Span::styled(" Profiles", self.theme.menu_title)),
            Line::from(Span::styled(
                format!(" {search}"),
                self.theme.slash_item_help,
            )),
        ];
        let list_room = usize::from(list_height)
            .saturating_sub(2)
            .saturating_sub(diagnostics.len());
        let pos = visible
            .iter()
            .position(|index| *index == self.profiles.cursor)
            .unwrap_or(0);
        if pos < self.profiles.scroll {
            self.profiles.scroll = pos;
        }
        if list_room > 0 && pos >= self.profiles.scroll + list_room {
            self.profiles.scroll = pos + 1 - list_room;
        }
        let scroll = &mut self.control_scroll[super::ControlTab::Models.index()];
        scroll.measure(visible.len(), list_room);
        scroll.set_offset(self.profiles.scroll);
        let window = visible
            .iter()
            .skip(self.profiles.scroll)
            .take(list_room)
            .copied()
            .collect::<Vec<_>>();
        if self.profiles.view.is_none() {
            lines.push(Line::from(Span::styled(
                " Loading profiles…",
                self.theme.slash_item_help,
            )));
        } else if window.is_empty()
            && !(self.profiles.filter.is_empty()
                && self.profiles.rows().is_empty()
                && !diagnostics.is_empty())
        {
            lines.push(Line::from(Span::styled(
                " No matching profiles",
                self.theme.slash_item_help,
            )));
        }
        let current = self
            .profiles
            .view
            .as_ref()
            .and_then(|view| view.current.clone());
        let width = usize::from(body.width);
        for (offset, index) in window.into_iter().enumerate() {
            let Some(profile) = self.profiles.rows().get(index) else {
                continue;
            };
            let selected = index == self.profiles.cursor;
            let mark = if selected { "❯" } else { " " };
            let suffix = if current.as_deref() == Some(profile.id.as_str()) {
                "  default"
            } else {
                ""
            };
            // One screen row per profile: the hit rows below depend on it.
            let label = clip_line(&format!(" {mark} {}{suffix}", profile.id), width);
            lines.push(Line::from(Span::styled(
                label,
                if selected {
                    self.theme.slash_selected
                } else {
                    self.theme.slash_item
                },
            )));
            let y = body.y.saturating_add(2).saturating_add(offset as u16);
            self.profiles.hits.push((
                index,
                Rect {
                    x: body.x,
                    y,
                    width: body.width,
                    height: 1,
                },
            ));
        }
        for diagnostic in &diagnostics {
            lines.push(Line::from(Span::styled(
                format!(" ! {diagnostic}"),
                self.theme.slash_item_help,
            )));
        }
        let list = Rect {
            height: list_height,
            ..body
        };
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(self.theme.menu_bg))
                .wrap(Wrap { trim: false }),
            list,
        );
        if let (Some(detail), Some(row)) = (wide_detail, row.as_ref()) {
            self.draw_profile_detail(frame, detail, row, true);
            return;
        }
        let Some(row) = row.filter(|_| reserve > 0) else {
            return;
        };
        let panel = Rect {
            x: body.x,
            y: body.y.saturating_add(list_height),
            width: body.width,
            height: reserve,
        };
        self.draw_profile_detail(frame, panel, &row, false);
    }

    fn draw_profile_detail(&self, frame: &mut Frame<'_>, area: Rect, row: &ProfileRow, wide: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(" {} ", row.id))
            .title_style(self.theme.menu_title)
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = if wide {
            usize::MAX
        } else {
            usize::from(inner.width)
        };
        let mut lines = vec![Line::from(Span::styled(
            clip_line(&role_text("Root agent", &row.root), width),
            self.theme.slash_item,
        ))];
        lines.push(Line::from(Span::styled(
            "Sub-agents",
            self.theme.menu_title,
        )));
        for card in &row.cards {
            lines.push(Line::from(Span::styled(
                clip_line(
                    &format!(
                        "{}{}",
                        "  ".repeat(card.depth + 1),
                        role_text(&card.label, card)
                    ),
                    width,
                ),
                self.theme.slash_item,
            )));
        }
        lines.push(Line::from(Span::styled(
            clip_line("Running children stay as they are.", width),
            self.theme.slash_item_help,
        )));
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(self.theme.menu_bg))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn detail_line_count(row: &ProfileRow) -> usize {
    3 + row.cards.len()
}

fn role_text(label: &str, role: &ProfileRole) -> String {
    let fast = if role.fast { " · fast" } else { "" };
    format!(
        "{label}   {} · {} · {}{fast}",
        role.account, role.model, role.effort
    )
}

fn clip_line(text: &str, width: usize) -> String {
    if width == 0 || text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests;
