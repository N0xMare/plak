//! Agents tab: one searchable list of named profiles, plus the apply dialog.
//!
//! The product owns the list: the tab asks for it with `ListProfiles`, paints
//! the [`ProfilesView`] it gets back, and sends the chosen row's id. What
//! landed comes back as `ProfileApplied` or `ProfileRefused`; the `current`
//! mark is only ever the product's answer.

use super::super::text_input::is_accepted_single_line_char;
use super::{App, KeyCode, KeyEvent, KeyModifiers, Rect, ScrollBlock, UserAction};
use crate::tui::{ProfileOutcome, ProfileRole, ProfileRow, ProfilesView, SurfaceIntent};
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
                format!("profile {name} saved; the next new plak session uses it")
            }
            ProfileOutcome::Forked => {
                format!(
                    "forked this session onto profile {name}; the previous session stays resumable"
                )
            }
            ProfileOutcome::Bound => format!("profile {name} is this session's root"),
            ProfileOutcome::AlreadyActive => {
                format!("this session already runs profile {name}'s root and cards")
            }
        };
        if let Some(view) = self.profiles.view.as_mut() {
            view.current = Some(name.to_owned());
        }
        self.push_block(ScrollBlock::System(note));
    }

    pub(super) fn apply_profile_refused(&mut self, name: &str, reason: &str) {
        self.push_block(ScrollBlock::Error(format!("profile {name}: {reason}")));
    }

    /// Asks how to apply the highlighted profile, in the destructive
    /// confirmation pop-up (spec 02 §5.5): Cancel, then Fork now (Use
    /// now on a session with no model) when the root is idle with no running
    /// child, then Next session.
    pub(super) fn arm_profile_confirm(&mut self) {
        let Some(row) = self.profiles.selected() else {
            return;
        };
        let id = row.id.clone();
        let fork_available = !self.busy && self.tree_live_descendants == 0;
        let (now_label, now_button) = if self.model.is_none() {
            ("Use now", "[ Use now ]")
        } else {
            ("Fork now", "[ Fork now ]")
        };
        let apply = |fork_now| {
            UserAction::Extension(SurfaceIntent::ApplyProfile {
                name: id.clone(),
                fork_now,
            })
        };
        let mut lines = vec![if !fork_available {
            format!("{now_label} waits until the root is idle and no child is running.")
        } else if self.model.is_none() {
            "Use now makes it this session's root.".to_owned()
        } else {
            "Fork now branches this session onto it; this one stays resumable.".to_owned()
        }];
        lines.push("Next session saves it for the next time you start plak.".to_owned());
        let mut actions = Vec::new();
        if fork_available {
            actions.push((now_button, apply(true)));
        }
        actions.push(("[ Next session ]", apply(false)));
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
        let panel_rows = row.as_ref().map_or(0, detail_line_count);
        let panel_height = u16::try_from(panel_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(2);
        let reserve = if row.is_some() && body.height > panel_height.saturating_add(4) {
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
        } else if window.is_empty() {
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
                "  current"
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
        let Some(row) = row.filter(|_| reserve > 0) else {
            return;
        };
        let panel = Rect {
            x: body.x,
            y: body.y.saturating_add(list_height),
            width: body.width,
            height: reserve,
        };
        self.draw_profile_detail(frame, panel, &row);
    }

    fn draw_profile_detail(&self, frame: &mut Frame<'_>, area: Rect, row: &ProfileRow) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(" {} ", row.id))
            .title_style(self.theme.menu_title)
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = usize::from(inner.width);
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
            Paragraph::new(lines).style(Style::default().bg(self.theme.menu_bg)),
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
mod tests {
    use super::super::test_support::{confirm_action, key, launch_basic};
    use super::super::{App, ControlTab};
    use crate::tui::{
        HostEvent, ProfileOutcome, ProfileRole, ProfileRow, ProfilesView, SurfaceIntent,
        SurfaceObservation, UserAction,
    };
    use crossterm::event::KeyCode;

    fn role(label: &str, account: &str, model: &str) -> ProfileRole {
        ProfileRole {
            label: label.to_owned(),
            account: account.to_owned(),
            model: model.to_owned(),
            effort: "medium".to_owned(),
            fast: false,
            depth: 0,
        }
    }

    fn view() -> ProfilesView {
        let row = |id: &str, aliases: &[&str], account: &str, model: &str| ProfileRow {
            id: id.to_owned(),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            root: role("root", account, model),
            cards: vec![role("worker", account, model)],
        };
        ProfilesView {
            rows: vec![
                row("codex-default", &["chatgpt"], "codex", "gpt-6-astra"),
                row("claude-default", &["claude"], "claude", "claude-sonnet-5"),
                row("grok-default", &["grok"], "grok", "grok-4.7"),
                // A user profile named like a built-in alias owns that name.
                row("codex", &[], "codex", "gpt-5.6-luna"),
            ],
            current: Some("grok-default".to_owned()),
            diagnostics: vec!["profiles/bad.toml is ignored: it needs `root`".to_owned()],
        }
    }

    /// An app on the Agents tab with the product's list delivered.
    fn agents_app() -> App {
        let mut app = App::new(launch_basic(), true);
        let action = app.activate_control_tab(ControlTab::Models);
        assert!(matches!(
            action,
            Some(UserAction::Extension(SurfaceIntent::ListProfiles))
        ));
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
        app
    }

    fn apply(action: Option<UserAction>) -> Option<(String, bool)> {
        match action {
            Some(UserAction::Extension(SurfaceIntent::ApplyProfile { name, fork_now })) => {
                Some((name, fork_now))
            }
            _ => None,
        }
    }

    /// The profile the open pop-up applies, and whether it offers Fork now.
    fn asking(app: &App) -> Option<(String, bool)> {
        let confirm = app.destructive_confirm.as_ref()?;
        assert_eq!(confirm.kind(), "profile");
        apply(confirm.first_action().cloned())
    }

    /// `→` `n` times from the primary action, then `Enter`.
    fn press_button(app: &mut App, n: usize) -> Option<UserAction> {
        for _ in 0..n {
            app.handle_key(key(KeyCode::Right));
        }
        app.handle_key(key(KeyCode::Enter))
    }

    #[test]
    fn the_list_opens_on_the_current_profile_and_enter_asks_before_applying() {
        let mut app = agents_app();
        assert_eq!(
            app.profiles.selected().map(|row| row.id.as_str()),
            Some("grok-default")
        );
        assert!(app.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(asking(&app), Some(("grok-default".to_owned(), true)));
        // Esc cancels; Enter on the primary action (Fork now) applies at once.
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert!(app.destructive_confirm.is_none());
        assert_eq!(app.control_tab, Some(ControlTab::Models));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            apply(confirm_action(&mut app)),
            Some(("grok-default".to_owned(), true))
        );
        assert!(app.control_tab.is_none());
        assert!(app.destructive_confirm.is_none());
    }

    /// Spec 02 §5.5 mouse parity: a click moves the cursor; a double-click
    /// opens the apply dialog exactly as Enter does.
    #[test]
    fn a_profile_row_selects_on_click_and_asks_on_double_click() {
        let mut app = agents_app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let (index, rect) = *app.profiles.hits.last().expect("rows are clickable");
        let click = super::super::test_support::left_click(rect.x + 2, rect.y);
        assert_eq!(app.handle_mouse(click), None);
        assert_eq!(app.profiles.cursor, index);
        assert!(app.destructive_confirm.is_none(), "a click only selects");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.handle_mouse(click), None);
        assert!(asking(&app).is_some(), "a double-click asks like Enter");
    }

    #[test]
    fn next_session_keeps_the_agents_tab_open() {
        let mut app = agents_app();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            apply(press_button(&mut app, 1)),
            Some(("grok-default".to_owned(), false))
        );
        assert_eq!(app.control_tab, Some(ControlTab::Models));
    }

    #[test]
    fn typing_filters_and_enter_on_no_match_applies_nothing() {
        let mut app = agents_app();
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(
            app.profiles.selected().map(|row| row.id.as_str()),
            Some("claude-default")
        );
        for ch in ['z', 'z', 'z'] {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        assert!(app.profiles.selected().is_none());
        assert!(app.handle_key(key(KeyCode::Enter)).is_none());
        assert!(
            app.destructive_confirm.is_none(),
            "a hidden row must not be offered"
        );
        let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
        assert!(frame.contains("No matching profiles"), "{frame}");
        assert!(
            !frame.contains("Root agent"),
            "no detail for a hidden row: {frame}"
        );
    }

    #[test]
    fn slash_is_an_ordinary_filter_character_and_the_hint_does_not_offer_it() {
        let mut app = agents_app();
        let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
        assert!(frame.contains("type to filter"), "{frame}");
        assert!(!frame.contains("/ search"), "{frame}");
    }

    #[test]
    fn product_diagnostics_are_listed_under_the_profiles() {
        let mut app = agents_app();
        let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
        assert!(frame.contains("profiles/bad.toml is ignored"), "{frame}");
        assert!(frame.contains("codex-default"), "{frame}");
    }

    #[test]
    fn agents_name_resolves_like_the_list() {
        let mut app = App::new(launch_basic(), true);
        let action = app.open_profile_confirm("codex");
        assert!(matches!(
            action,
            Some(UserAction::Extension(SurfaceIntent::ListProfiles))
        ));
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
        assert_eq!(asking(&app), Some(("codex".to_owned(), true)));
        app.destructive_confirm = None;
        app.open_profile_confirm("claude");
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
        assert_eq!(asking(&app), Some(("claude-default".to_owned(), true)));
        app.destructive_confirm = None;
        app.open_profile_confirm("nope");
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
        assert!(app.destructive_confirm.is_none());
    }

    #[test]
    fn the_current_mark_follows_only_the_products_outcome() {
        let mut app = agents_app();
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            apply(press_button(&mut app, 1)),
            Some(("claude-default".to_owned(), false))
        );
        // Sent, not yet answered: the mark has not moved.
        assert_eq!(
            app.profiles
                .view
                .as_ref()
                .and_then(|view| view.current.as_deref()),
            Some("grok-default")
        );
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::ProfileRefused {
            name: "claude-default".to_owned(),
            reason: "claude is not ready".to_owned(),
        }));
        assert_eq!(
            app.profiles
                .view
                .as_ref()
                .and_then(|view| view.current.as_deref()),
            Some("grok-default")
        );
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::ProfileApplied {
            name: "claude-default".to_owned(),
            outcome: ProfileOutcome::Saved,
        }));
        assert_eq!(
            app.profiles
                .view
                .as_ref()
                .and_then(|view| view.current.as_deref()),
            Some("claude-default")
        );
    }

    #[test]
    fn an_unbound_session_offers_use_now() {
        let mut launch = launch_basic();
        launch.model = None;
        let mut app = App::new(launch, true);
        app.activate_control_tab(ControlTab::Models);
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
        app.handle_key(key(KeyCode::Enter));
        let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
        assert!(frame.contains("[ Use now ]"), "{frame}");
        assert_eq!(
            apply(confirm_action(&mut app)),
            Some(("grok-default".to_owned(), true))
        );
    }
}
