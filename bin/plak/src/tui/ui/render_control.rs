//! Menu overlay rendering.

use super::{
    App, Color, ControlTab, Frame, Hint, Line, ModalChrome, ModalSizing, Paragraph,
    QueueDrainState, Rect, SettingKind, SettingsMenu, SettingsPane, Span, Style,
    SubscriptionUsageView, Wrap, keymap, modal_chrome, picker, settings_registry, usage_tab,
};
use crate::tui::providers::ProviderRowPresentation as _;
use unicode_width::UnicodeWidthStr;

/// Narrow Menu column before the list/detail breakpoint.
const MENU_BODY_MAX_COLUMNS: u16 = 120;

impl App {
    /// Footer hints for the active tab, phrased as effects (spec 02 §5.5).
    pub(super) fn control_hints(&self) -> Vec<Hint> {
        let mut hints = Vec::new();
        if let Some(menu) = self.settings_menu.as_ref()
            && self.control_tab == Some(ControlTab::Settings)
        {
            match menu.pane {
                SettingsPane::Choosing(chooser) => {
                    let previews = SettingsMenu::meta(chooser.row)
                        .is_some_and(settings_registry::SettingMeta::previews);
                    hints.push(Hint::new("↑/↓", if previews { "try" } else { "choose" }));
                    hints.push(Hint::new("Enter", "commit"));
                    hints.push(Hint::new("Esc", if previews { "revert" } else { "cancel" }));
                    return hints;
                }
                SettingsPane::Stepping(_) => {
                    hints.push(Hint::new("↑/↓", "adjust"));
                    hints.push(Hint::new("Enter", "commit"));
                    hints.push(Hint::new("Esc", "revert"));
                    return hints;
                }
                SettingsPane::Browse => {
                    // The switch hint names what pressing Space would do to the
                    // focused row, never the key's generic meaning.
                    match SettingsMenu::meta(menu.selected).map(|meta| meta.kind) {
                        Some(SettingKind::Bool { .. }) => {
                            hints.push(Hint::new(
                                "Space",
                                if menu.bool_value(
                                    SettingsMenu::meta(menu.selected).map_or("", |meta| meta.key),
                                ) == Some(true)
                                {
                                    "disable"
                                } else {
                                    "enable"
                                },
                            ));
                        }
                        Some(SettingKind::Enum { .. }) => hints.push(Hint::new("Enter", "choose")),
                        Some(SettingKind::Int { .. }) => hints.push(Hint::new("Enter", "adjust")),
                        Some(SettingKind::Action) | None => {
                            hints.push(Hint::new("Enter", "run"));
                        }
                    }
                    hints.push(Hint::new("/", "search"));
                    hints.push(Hint::new("d", "restore default"));
                }
            }
        }
        if self.control_tab == Some(ControlTab::Models) {
            hints.push(Hint::new("Enter", "choose when it applies"));
        }
        if self.control_tab == Some(ControlTab::Session) {
            hints.push(Hint::new("click", "copy this row"));
        }
        if self.control_tab == Some(ControlTab::Keybinds) {
            hints.push(Hint::new(
                if self.keybinds_full { "Esc" } else { "Enter" },
                if self.keybinds_full {
                    "while typing"
                } else {
                    "all keybinds"
                },
            ));
        }
        if self.control_tab == Some(ControlTab::Providers) {
            hints.push(Hint::new("↑/↓", "read another vertical"));
        }
        if matches!(
            self.control_tab,
            Some(
                ControlTab::Providers
                    | ControlTab::Usage
                    | ControlTab::Context
                    | ControlTab::Session
            )
        ) {
            hints.push(Hint::new("y", "copy this tab"));
        }
        hints.push(Hint::new("←/→", "switch tab"));
        hints.push(Hint::new("PgUp/PgDn", "scroll"));
        hints.push(Hint::new("Esc", "close"));
        hints
    }

    /// Below this the value column is not worth keeping and the row wraps as
    /// prose instead.
    pub(super) const MIN_SHEET_VALUE_WIDTH: usize = 12;

    pub(super) fn draw_control_overlay(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(active) = self.control_tab else {
            return;
        };
        let labels: Vec<&str> = ControlTab::ALL.iter().map(|tab| tab.label()).collect();
        let hints = self.control_hints();
        let theme = self.theme.clone();
        let chrome = ModalChrome {
            title: "Menu",
            tabs: &labels,
            active: active.index(),
            hints: &hints,
            close_hover: self.control_close_hover,
            sizing: ModalSizing::full_page(),
            content_max: (area.width.saturating_sub(4) < modal_chrome::WIDE_MENU_COLUMNS)
                .then_some(MENU_BODY_MAX_COLUMNS),
        };
        let Some(layout) = modal_chrome::render(frame, area, &theme, &chrome) else {
            // Below the refuse-to-draw floor: the shared too-small line, and
            // every hit rect cleared so a stale click cannot fire (spec 02 §5.5).
            self.clear_control_hits();
            modal_chrome::draw_too_small(frame, area, &theme, "Menu");
            return;
        };
        self.control_popup = Some(layout.popup);
        self.control_close_hit = Some(layout.close);
        self.control_tab_hits = layout
            .tabs
            .iter()
            .filter_map(|(index, rect)| ControlTab::ALL.get(*index).map(|tab| (*tab, *rect)))
            .collect();
        self.settings_hits.clear();
        self.session_row_hits.clear();
        let body = layout.body;
        if body.height == 0 || body.width == 0 {
            return;
        }
        let mut panel = self.control_description(active);
        if body.width >= modal_chrome::WIDE_MENU_COLUMNS
            && active == ControlTab::Settings
            && let Some(menu) = &self.settings_menu
            && let Some(meta) = SettingsMenu::meta(menu.selected)
            && let Some(panel) = &mut panel
        {
            let choices = meta.choices();
            if !choices.is_empty() {
                panel.body.push_str(" Options: ");
                panel.body.push_str(
                    &choices
                        .iter()
                        .map(|choice| choice.label)
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                panel.body.push('.');
            }
        }
        let (body, floating) = if panel.is_some() {
            modal_chrome::split_wide_detail(body).map_or_else(
                || modal_chrome::split_description(body, panel.as_ref()),
                |(list, detail)| (list, Some(detail)),
            )
        } else {
            (body, None)
        };
        match active {
            ControlTab::Settings => self.draw_control_settings(frame, body),
            ControlTab::Models => self.draw_control_models(frame, body),
            ControlTab::Providers => self.draw_control_providers(frame, body),
            ControlTab::Usage => self.draw_control_usage(frame, body),
            ControlTab::Context => self.draw_control_context(frame, body),
            ControlTab::Session => self.draw_control_session(frame, body),
            ControlTab::Keybinds => self.draw_control_keybinds(frame, body),
        }
        if let (Some(rect), Some(panel)) = (floating, panel) {
            modal_chrome::render_description(frame, rect, &self.theme, &panel);
        }
    }

    /// The floating description for the focused row of `tab`, when it has one.
    ///
    /// Tabs whose rows carry no description — Models, Usage, Context, Session
    /// and Keybinds — return `None` and keep their whole body.
    pub(super) fn control_description(
        &self,
        tab: ControlTab,
    ) -> Option<modal_chrome::DescriptionPanel> {
        match tab {
            ControlTab::Settings => self.settings_description(),
            ControlTab::Providers => self.provider_description(),
            ControlTab::Models
            | ControlTab::Usage
            | ControlTab::Context
            | ControlTab::Session
            | ControlTab::Keybinds => None,
        }
    }

    pub(super) fn settings_description(&self) -> Option<modal_chrome::DescriptionPanel> {
        let menu = self.settings_menu.as_ref()?;
        if menu.pane != SettingsPane::Browse {
            return None;
        }
        let index = menu.selected;
        let meta = SettingsMenu::meta(index)?;
        let row = Self::settings_row(menu, index, meta);
        if row.description.is_empty() {
            return None;
        }
        // An Action row holds no value, so it gets no `now/default` footnote
        // rather than an empty one.
        let value = (!menu.value_label(index).is_empty()).then(|| {
            let default = SettingsMenu::default_label(index);
            format!("now {} · default {default}", menu.value_label(index))
        });
        Some(modal_chrome::DescriptionPanel {
            title: meta.label.to_owned(),
            body: row.description,
            value,
        })
    }

    pub(super) fn clear_control_hits(&mut self) {
        self.control_popup = None;
        self.control_close_hit = None;
        self.control_close_hover = false;
        self.control_tab_hits.clear();
        self.settings_hits.clear();
        self.session_row_hits.clear();
        self.provider_row_hits.clear();
    }

    /// Renders `lines` inside the shared scroll cursor for `tab`.
    /// Wraps a plain sheet to `width`, hanging continuation rows two columns
    /// past their row's own indent so a wrapped description still reads as one
    /// entry (§5.5, §5.2).
    pub(super) fn wrap_sheet_lines_impl(text: &str, width: u16) -> Vec<String> {
        let width = usize::from(width).max(1);
        let mut wrapped = Vec::new();
        for line in text.lines() {
            if line.width() <= width {
                wrapped.push(line.to_owned());
                continue;
            }
            // A keybinds row is a padded key column and a description. Wrapping
            // it as one paragraph collapsed the pad and started the tail at
            // column 2, so a long row lost the aligned column its neighbours
            // kept. Keep the key where it is and wrap only the
            // description, under itself.
            if let Some(column) = Self::sheet_value_column(line)
                && column + Self::MIN_SHEET_VALUE_WIDTH <= width
            {
                let (key, description) = line.split_at(column);
                let pad = " ".repeat(column);
                for (index, piece) in picker::wrap(description.trim_start(), width - column)
                    .into_iter()
                    .enumerate()
                {
                    if index == 0 {
                        wrapped.push(format!("{key}{piece}"));
                    } else {
                        wrapped.push(format!("{pad}{piece}"));
                    }
                }
                continue;
            }
            let indent: String = line
                .chars()
                .take_while(|ch| ch.is_whitespace())
                .collect::<String>();
            let budget = width
                .saturating_sub(indent.chars().count())
                .saturating_sub(2)
                .max(8);
            for (index, piece) in picker::wrap(line.trim_start(), budget)
                .into_iter()
                .enumerate()
            {
                if index == 0 {
                    wrapped.push(format!("{indent}{piece}"));
                } else {
                    wrapped.push(format!("{indent}  {piece}"));
                }
            }
        }
        wrapped
    }

    /// Where a sheet row's description begins, for a row that has one.
    ///
    /// [`crate::tui::keymap::CHORD_WIDTH`] is the column the sheet is generated at,
    /// so it is read rather than re-derived: a description that itself contains
    /// a double space would otherwise be mistaken for the key column's pad.
    /// A row that is not a chord row — a blank, a heading, an indented note —
    /// has no description column and wraps as ordinary prose.
    pub(super) fn sheet_value_column(line: &str) -> Option<usize> {
        const COLUMN: usize = keymap::CHORD_WIDTH;
        if line.starts_with(char::is_whitespace) || line.chars().count() <= COLUMN {
            return None;
        }
        let boundary = line.char_indices().nth(COLUMN)?.0;
        let key = &line[..boundary];
        // Exactly one padded key column, and something after it.
        // ASCII keys only, so the byte index this returns is also the display
        // column the continuation rows are padded to.
        (key.is_ascii() && key.ends_with(' ') && !line[boundary..].starts_with(' '))
            .then_some(boundary)
    }

    pub(super) fn draw_scrolled_text(
        &mut self,
        frame: &mut Frame<'_>,
        body: Rect,
        tab: ControlTab,
        text: &str,
    ) {
        let item = self.theme.slash_item;
        let bg = self.theme.menu_bg;
        let scroll = &mut self.control_scroll[tab.index()];
        Self::render_scrolled_text(frame, body, scroll, text, item, bg);
    }

    /// Paint `text` into `body` through `scroll`, wrapping each row under its
    /// own indent (§5.5: truncating a description mid-word is a defect).
    ///
    /// The Keybinds tab and the `/help` overlay share this one renderer, so a
    /// body taller than its frame scrolls on both instead of clipping on one.
    pub(super) fn render_scrolled_text(
        frame: &mut Frame<'_>,
        body: Rect,
        scroll: &mut modal_chrome::ScrollState,
        text: &str,
        item: Style,
        bg: Color,
    ) {
        let lines = Self::wrap_sheet_lines_impl(text, body.width);
        scroll.measure(lines.len(), usize::from(body.height));
        let offset = scroll.offset();
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(offset)
            .take(usize::from(body.height))
            .map(|line| {
                let heading = matches!(line.as_str(), "While typing" | "All keybinds")
                    || keymap::When::ALL.iter().any(|when| line == when.heading());
                let style = if heading {
                    item.add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    item
                };
                Line::from(Span::styled(line, style))
            })
            .collect();
        frame.render_widget(Paragraph::new(visible).style(Style::default().bg(bg)), body);
    }

    pub(super) fn draw_scrolled_lines(
        &mut self,
        frame: &mut Frame<'_>,
        body: Rect,
        tab: ControlTab,
        lines: Vec<Line<'static>>,
    ) -> usize {
        let scroll = &mut self.control_scroll[tab.index()];
        scroll.measure(lines.len(), usize::from(body.height));
        let offset = scroll.offset();
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(offset)
            .take(usize::from(body.height))
            .collect();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().bg(self.theme.menu_bg)),
            body,
        );
        offset
    }

    /// The Usage tab body: provider allowances (spec 02 §7.2).
    pub(super) fn draw_control_usage(&mut self, frame: &mut Frame<'_>, body: Rect) {
        let lines = self.usage_tab_lines(body.width);
        self.remember_tab_copy_text(ControlTab::Usage, &lines);
        let _ = self.draw_scrolled_lines(frame, body, ControlTab::Usage, lines);
    }

    /// Lines of the Usage tab body.
    pub(super) fn usage_tab_lines(&self, width: u16) -> Vec<Line<'static>> {
        let note = match &self.subscription_usage {
            SubscriptionUsageView::Loading => {
                Some("Loading provider allowance snapshots…".to_owned())
            }
            SubscriptionUsageView::Unavailable { reason } => {
                Some(format!("Allowance read unavailable · {reason}"))
            }
            SubscriptionUsageView::Read => None,
        };
        let chrome = usage_tab::LimitsChrome {
            refresh_failure_notes: &self.refresh_failure_notes,
            read_note: note.as_deref(),
            latest_turn: self
                .last_turn_usage
                .map(|counters| crate::tui::presentation::TurnUsage {
                    input_tokens: counters.0,
                    output_tokens: counters.1,
                    reasoning_tokens: counters.2,
                    cached_input_tokens: counters.3,
                    cache_write_tokens: counters.4,
                    model_operations: counters.5,
                    reporting_model_operations: counters.6,
                }),
        };
        usage_tab::limits_lines(&self.usage_limits, &chrome, width, &self.theme)
    }

    /// The Context tab body: the canonical context projection (spec 02 §5.6).
    pub(super) fn draw_control_context(&mut self, frame: &mut Frame<'_>, body: Rect) {
        let lines = self.context_tab_lines(body.width);
        self.remember_tab_copy_text(ControlTab::Context, &lines);
        let _ = self.draw_scrolled_lines(frame, body, ControlTab::Context, lines);
    }

    /// Lines of the Context tab body at `width`.
    pub(super) fn context_tab_lines(&self, width: u16) -> Vec<Line<'static>> {
        let model = self.model.clone();
        let chrome = usage_tab::ContextChrome {
            model_label: model.as_deref(),
            auto_compact_enabled: self
                .settings_snapshot
                .as_ref()
                .is_none_or(|settings| settings.compaction),
        };
        usage_tab::context_lines(&self.context_usage, &chrome, width, &self.theme)
    }

    /// The Session tab body: typed session facts, click-to-copy (spec 02 §5.5).
    pub(super) fn draw_control_session(&mut self, frame: &mut Frame<'_>, body: Rect) {
        self.session_row_hits.clear();
        if body.height == 0 {
            return;
        }
        let rows = self.session_info_rows();
        let (lines, hits) = usage_tab::session_lines(&rows, &self.theme);
        self.session_rows = rows;
        self.remember_tab_copy_text(ControlTab::Session, &lines);
        let offset = self.draw_scrolled_lines(frame, body, ControlTab::Session, lines);
        for (row, line) in hits {
            let Some(relative) = line.checked_sub(offset) else {
                continue;
            };
            let Ok(relative) = u16::try_from(relative) else {
                continue;
            };
            if relative >= body.height {
                continue;
            }
            self.session_row_hits.push((
                row,
                Rect {
                    x: body.x,
                    y: body.y.saturating_add(relative),
                    width: body.width,
                    height: 1,
                },
            ));
        }
    }

    /// Keeps the plain text of a read-only tab's body so `y` copies exactly
    /// what the last draw painted.
    pub(super) fn remember_tab_copy_text(&mut self, tab: ControlTab, lines: &[Line<'static>]) {
        let text = usage_tab::plain_text(lines);
        match self
            .tab_copy_text
            .iter_mut()
            .find(|(candidate, _)| *candidate == tab)
        {
            Some(entry) => entry.1 = text,
            None => self.tab_copy_text.push((tab, text)),
        }
    }

    /// Whether `name` is a template the host projected.
    ///
    /// Presentation only: the TUI never reads `config.toml` and never resolves
    /// a pin. It only declines to print a string under a label
    /// that promises a template name.
    pub(super) fn is_projected_template(&self, name: &str) -> bool {
        self.templates.iter().any(|row| row.name == name)
    }

    /// Every Session row, built from typed host data plus the live chrome the
    /// TUI itself holds authoritatively (spec 02 §5.5).
    pub(super) fn session_info_rows(&self) -> Vec<usage_tab::SessionRow> {
        let occupied = self
            .context_usage
            .breakdown
            .as_ref()
            .map(|breakdown| breakdown.used_tokens)
            .or_else(|| {
                self.context_usage
                    .prepared_input
                    .map(crate::tui::presentation::PreparedInputUsage::tokens)
            });
        let context = match (occupied, self.context_usage.total_window) {
            (Some(occupied), Some(window)) => Some(format!(
                "{} / {} tokens ({})",
                usage_tab::fmt_tok(occupied),
                usage_tab::fmt_tok(window),
                usage_tab::percent_of_window(occupied, window)
            )),
            _ => None,
        };
        let queue = format!(
            "{} queued · {}",
            self.queue_len,
            match self.queue_drain {
                QueueDrainState::LiveAutoDrain => "live auto-drain",
                QueueDrainState::RestoredPaused => "restored, paused",
            }
        );
        let live = usage_tab::SessionLive {
            context,
            queue,
            tree_epoch: self.tree_epoch_state.as_deref(),
            build: {
                // Never hash on the UI thread: the launch prewarm fills it.
                let id = crate::build_identity::ready_binary_id().unwrap_or("not reported");
                if self.newer_build_installed {
                    format!("{id} · new build installed · /reload")
                } else {
                    id.to_owned()
                }
            },
        };
        // Identity the launch already carries typed stands in until the host
        // publishes its own projection, so the rows are never blank on first
        // paint. Nothing here is re-parsed from a rendered string.
        let mut info = self.session_info.clone();
        // §5.5 "Root template" names an `[agent.<name>]` template. Anything
        // else under that label is an invented value, so a string the host did
        // not project as a template name is not rendered at all.
        if !info
            .root_template
            .as_ref()
            .is_some_and(|name| self.is_projected_template(name))
        {
            info.root_template = None;
        }
        if info.session_id.is_empty() {
            info.session_id = self.session_id.clone().unwrap_or_default();
        }
        if info.working_dir.is_empty() {
            info.working_dir.clone_from(&self.workspace);
        }
        if info.root_model.is_none() {
            info.root_model.clone_from(&self.model);
        }
        if info.root_effort.is_none() {
            info.root_effort.clone_from(&self.effort);
        }
        usage_tab::session_rows(&info, &live)
    }

    pub(super) fn draw_control_keybinds(&mut self, frame: &mut Frame<'_>, body: Rect) {
        let text = if self.keybinds_full {
            format!("All keybinds\n\n{}", keymap::sheet_text())
        } else {
            keymap::typing_text()
        };
        self.draw_scrolled_text(frame, body, ControlTab::Keybinds, &text);
    }

    /// The Agents tab (spec 02 §5.5): the profile list and nothing else.
    ///
    /// There is no readiness header and no Templates section; credential
    /// status lives on its own tab (§5.5).
    pub(super) fn draw_control_models(&mut self, frame: &mut Frame<'_>, body: Rect) {
        if body.height == 0 {
            return;
        }
        self.draw_agent_profiles(frame, body);
    }

    /// Rows one vertical occupies on the Providers tab, separator included.
    pub(super) const PROVIDER_BLOCK_ROWS: usize = 5;

    /// The Providers tab (spec 02 §5.5): a read-only status view, one block per
    /// vertical.
    ///
    /// Nothing here is an action. Each block names the account the host already
    /// knows, the credential status, and the CLI command that connects or
    /// repairs it; `Enter` only focuses, because login, logout and key entry
    /// are CLI-only (§5.5).
    pub(super) fn draw_control_providers(&mut self, frame: &mut Frame<'_>, body: Rect) {
        self.provider_row_hits.clear();
        if body.height == 0 || body.width == 0 {
            return;
        }
        if self.providers.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " No provider status has been projected yet.",
                    self.theme.slash_item_help,
                )))
                .style(Style::default().bg(self.theme.menu_bg)),
                body,
            );
            return;
        }
        let focus = self.providers_focus.min(self.providers.len() - 1);
        self.providers_focus = focus;
        let lines = self.provider_lines(focus);
        self.remember_tab_copy_text(ControlTab::Providers, &lines);
        // The focused block is revealed the way every cursor-owning tab reveals
        // its row, so paging the cursor carries the scroll with it.
        let first = focus.saturating_mul(Self::PROVIDER_BLOCK_ROWS);
        let scroll = &mut self.control_scroll[ControlTab::Providers.index()];
        scroll.reveal(first.saturating_add(Self::PROVIDER_BLOCK_ROWS - 2));
        scroll.reveal(first);
        let offset = self.draw_scrolled_lines(frame, body, ControlTab::Providers, lines);
        for (index, _) in self.providers.iter().enumerate() {
            let first = index.saturating_mul(Self::PROVIDER_BLOCK_ROWS);
            let Some(relative) = first.checked_sub(offset) else {
                continue;
            };
            let Ok(relative) = u16::try_from(relative) else {
                continue;
            };
            if relative >= body.height {
                continue;
            }
            let height = u16::try_from(Self::PROVIDER_BLOCK_ROWS - 1)
                .unwrap_or(1)
                .min(body.height.saturating_sub(relative));
            self.provider_row_hits.push((
                index,
                Rect {
                    x: body.x,
                    y: body.y.saturating_add(relative),
                    width: body.width,
                    height,
                },
            ));
        }
    }

    /// The Providers tab body: `label` / `account` / `status` / `cli` per
    /// vertical, in host order (spec 02 §5.5).
    pub(super) fn provider_lines(&self, focus: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.providers.len() * Self::PROVIDER_BLOCK_ROWS);
        for (index, row) in self.providers.iter().enumerate() {
            let tone = match row.status_tone() {
                crate::tui::StatusTone::Ready => self.theme.success,
                crate::tui::StatusTone::LoginRequired => self.theme.warning,
                crate::tui::StatusTone::Blocked => self.theme.error,
            };
            let label_style = if index == focus {
                self.theme.menu_title
            } else {
                self.theme.slash_item
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", row.state.glyph()), tone),
                Span::styled(row.label.clone(), label_style),
            ]));
            for (field, value, style) in [
                (
                    "account",
                    self.provider_account_label(row),
                    self.theme.slash_item,
                ),
                ("status", row.status_label(), tone),
                ("cli", row.cli_direction(), self.theme.slash_item_help),
            ] {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("     {field}:{:width$}", "", width = 8 - field.len()),
                        self.theme.slash_item_help,
                    ),
                    Span::styled(value, style),
                ]));
            }
            lines.push(Line::from(""));
        }
        lines
    }

    /// The account identity the host already knows for `row`, or
    /// `not reported`.
    ///
    /// Identity is never invented: an OAuth vertical only has one once an
    /// authenticated endpoint reported it into the Usage projection, and an
    /// API-key account only has the redaction-safe label the key store
    /// supplies (its fingerprint, or the environment variable that won).
    pub(super) fn provider_account_label(&self, row: &crate::tui::ProviderDef) -> String {
        if let Some(label) = row.account_label.as_ref().filter(|label| !label.is_empty()) {
            return label.clone();
        }
        let reported = self
            .usage_limits
            .verticals
            .iter()
            .find(|vertical| vertical.vertical.account() == row.id);
        let email = reported
            .and_then(|vertical| vertical.account_email.as_ref())
            .filter(|email| !email.is_empty());
        match (
            email,
            reported.and_then(|vertical| vertical.plan_label.as_ref()),
        ) {
            (Some(email), Some(plan)) => format!("{email} ({plan})"),
            (Some(email), None) => email.clone(),
            (None, _) => "not reported".to_owned(),
        }
    }

    /// The focused vertical's explanation, floated at the foot of the body
    /// (spec 02 §5.5). This is where `refresh outcome unknown` is spelled out.
    pub(super) fn provider_description(&self) -> Option<modal_chrome::DescriptionPanel> {
        let row = self.providers.get(self.providers_focus)?;
        Some(modal_chrome::DescriptionPanel {
            title: row.label.clone(),
            body: row.status_description(),
            value: None,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one renderer keeps the registry rows, sub-panes and hit map synchronized"
    )]
    pub(super) fn draw_control_settings(&mut self, frame: &mut Frame<'_>, body: Rect) {
        let Some(menu) = self.settings_menu.clone() else {
            let message = self
                .settings_error
                .as_deref()
                .unwrap_or("Loading settings…");
            frame.render_widget(
                Paragraph::new(message)
                    .style(if self.settings_error.is_some() {
                        self.theme.error
                    } else {
                        self.theme.slash_item
                    })
                    .wrap(Wrap { trim: false }),
                body,
            );
            return;
        };
        match menu.pane {
            SettingsPane::Choosing(chooser) => {
                self.draw_settings_chooser(frame, body, &menu, chooser);
                return;
            }
            SettingsPane::Stepping(stepper) => {
                self.draw_settings_stepper(frame, body, &menu, stepper);
                return;
            }
            SettingsPane::Browse => {}
        }

        let search = if menu.filter.is_empty() {
            " / search settings".to_owned()
        } else {
            format!(" / {}█", menu.filter)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                search,
                if menu.search_active {
                    self.theme.slash_selected
                } else {
                    self.theme.slash_item_help
                },
            ))),
            Rect { height: 1, ..body },
        );
        let list = Rect {
            x: body.x,
            y: body.y.saturating_add(1),
            width: body.width,
            height: body.height.saturating_sub(1),
        };
        if list.height == 0 {
            return;
        }

        let visible = menu.visible_rows();
        // Every row is one line — the description floats at the foot of the body
        // instead of pushing the list around — so the scroll math is the row
        // index and cannot drift from the hit map.
        let total: usize = visible.len();
        let selected_line: usize = visible
            .iter()
            .position(|index| *index == menu.selected)
            .unwrap_or(0);
        {
            let scroll = &mut self.control_scroll[ControlTab::Settings.index()];
            scroll.measure(total, usize::from(list.height));
            scroll.reveal(selected_line);
        }
        let offset = self.control_scroll[ControlTab::Settings.index()].offset();

        for (position, index) in visible.into_iter().enumerate() {
            let Some(top) = position
                .checked_sub(offset)
                .and_then(|top| u16::try_from(top).ok())
            else {
                continue;
            };
            if top >= list.height {
                break;
            }
            let Some(meta) = SettingsMenu::meta(index) else {
                continue;
            };
            let row = Self::settings_row(&menu, index, meta);
            let hit = picker::render_row(
                frame,
                Rect {
                    x: list.x,
                    y: list.y.saturating_add(top),
                    width: list.width,
                    height: 1,
                },
                &self.theme,
                &row,
                index,
                index == menu.selected,
                index == menu.selected || self.settings_hover == Some(index),
            );
            self.settings_hits.push(hit);
        }
    }

    pub(super) fn settings_row(
        menu: &SettingsMenu,
        index: usize,
        meta: &'static settings_registry::SettingMeta,
    ) -> picker::PickerRow {
        let mut label = meta.label.to_owned();
        if meta.new_sessions {
            label.push_str("  (new sessions)");
        }
        let description = meta.description.to_owned();
        picker::PickerRow {
            glyph: if index == menu.selected { "▾" } else { "▸" },
            label,
            value: menu.value_label(index),
            description,
            pill: meta.restart_required.then_some("· restart"),
            recessive: menu.is_recessive(index),
        }
    }

    pub(super) fn draw_settings_chooser(
        &mut self,
        frame: &mut Frame<'_>,
        body: Rect,
        menu: &SettingsMenu,
        chooser: crate::tui::EnumChooser,
    ) {
        let Some(meta) = SettingsMenu::meta(chooser.row) else {
            return;
        };
        let mut lines = vec![
            Line::from(Span::styled(
                format!(" Settings › {}", meta.label),
                self.theme.menu_title,
            )),
            Line::from(Span::styled(
                format!(" {}", meta.description),
                self.theme.slash_item_help,
            )),
            Line::from(Span::raw("")),
        ];
        let committed = meta
            .choices()
            .get(chooser.committed)
            .map(|choice| choice.value);
        for (index, choice) in meta.choices().iter().enumerate() {
            let y = u16::try_from(lines.len())
                .ok()
                .filter(|top| *top < body.height)
                .map(|top| body.y + top);
            if let Some(y) = y {
                let rect = Rect::new(body.x, y, body.width, 1);
                self.settings_hits.push(picker::RowHit {
                    index,
                    rect,
                    value: Rect { width: 0, ..rect },
                });
            }
            let mark = if Some(choice.value) == committed {
                "●"
            } else {
                "○"
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {mark} {:<22}", choice.label),
                    if index == chooser.focus {
                        self.theme.slash_selected
                    } else {
                        self.theme.slash_item
                    },
                ),
                Span::styled(
                    format!(" · {}", choice.description),
                    self.theme.slash_item_help,
                ),
            ]));
        }
        let _ = menu;
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(self.theme.menu_bg)),
            body,
        );
    }

    pub(super) fn draw_settings_stepper(
        &self,
        frame: &mut Frame<'_>,
        body: Rect,
        menu: &SettingsMenu,
        stepper: crate::tui::IntStepper,
    ) {
        let Some(meta) = SettingsMenu::meta(stepper.row) else {
            return;
        };
        let SettingKind::Int { min, max, .. } = meta.kind else {
            return;
        };
        let _ = menu;
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!(" Settings › {}", meta.label),
                    self.theme.menu_title,
                )),
                Line::from(Span::styled(
                    format!(" {}", meta.description),
                    self.theme.slash_item_help,
                )),
                Line::from(Span::raw("")),
                Line::from(Span::styled(
                    format!("   ‹  {}  ›", stepper.value),
                    self.theme.slash_selected,
                )),
                Line::from(Span::styled(
                    format!("   range {min}–{max}"),
                    self.theme.slash_item_help,
                )),
            ])
            .style(Style::default().bg(self.theme.menu_bg)),
            body,
        );
    }
}
