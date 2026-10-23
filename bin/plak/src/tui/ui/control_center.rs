//! Menu tab activation, keys, and mouse.

use super::terminal::copy_text_osc52;
use super::{
    App, ControlTab, Instant, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
    SettingsMenu, SettingsPane, UserAction, event, modal_chrome, picker, theme_for, usage_tab,
};
use crate::tui::SurfaceIntent;

impl App {
    /// `PageUp`/`PageDown`/`Home`/`End` on the active tab (spec 02 §5.5).
    ///
    /// Settings and Models reveal their focused row on every draw, so a bare
    /// offset move there snaps straight back: both move the focus by a viewport
    /// instead, and the reveal carries the scroll with it.
    pub(super) fn scroll_active_tab(&mut self, code: KeyCode) -> bool {
        let Some(tab) = self.control_tab else {
            return false;
        };
        let page = self.control_scroll[tab.index()].viewport();
        if tab == ControlTab::Settings
            && let Some(menu) = self.settings_menu.as_mut()
            && menu.pane == SettingsPane::Browse
        {
            if menu.page_selection(code, page) {
                self.mark_dirty();
            }
            return true;
        }
        // Providers owns a block cursor, so a page move moves the cursor and
        // the reveal carries the scroll with it (spec 02 §5.5).
        if tab == ControlTab::Providers && !self.providers.is_empty() {
            let last = self.providers.len() - 1;
            let stride = (page / Self::PROVIDER_BLOCK_ROWS).max(1);
            let target = match code {
                KeyCode::PageUp => self.providers_focus.saturating_sub(stride),
                KeyCode::PageDown => self.providers_focus.saturating_add(stride).min(last),
                KeyCode::Home => 0,
                KeyCode::End => last,
                _ => return false,
            };
            if target != self.providers_focus {
                self.providers_focus = target;
                self.mark_dirty();
            }
            return true;
        }
        let scroll = &mut self.control_scroll[tab.index()];
        let moved = match code {
            KeyCode::PageUp => scroll.page_up(),
            KeyCode::PageDown => scroll.page_down(),
            KeyCode::Home => scroll.home(),
            KeyCode::End => scroll.end(),
            _ => return false,
        };
        if moved {
            self.mark_dirty();
        }
        true
    }

    pub(super) fn handle_control_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        let code = key.code;
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let settings_sub_pane = self.control_tab == Some(ControlTab::Settings)
            && self
                .settings_menu
                .as_ref()
                .is_some_and(|menu| menu.pane != SettingsPane::Browse);
        let agents_list = self.control_tab == Some(ControlTab::Models);
        if matches!(
            code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
        ) && agents_list
        {
            return self.handle_agents_key(key);
        }
        if matches!(
            code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
        ) && self.scroll_active_tab(code)
        {
            return None;
        }
        match code {
            KeyCode::Enter
                if self.control_tab == Some(ControlTab::Keybinds) && !self.keybinds_full =>
            {
                self.keybinds_full = true;
                self.control_scroll[ControlTab::Keybinds.index()].home();
                self.mark_dirty();
                None
            }
            KeyCode::Esc
                if self.control_tab == Some(ControlTab::Keybinds) && self.keybinds_full =>
            {
                self.keybinds_full = false;
                self.control_scroll[ControlTab::Keybinds.index()].home();
                self.mark_dirty();
                None
            }
            // A sub-pane owns Esc and the arrows before the chrome does.
            _ if settings_sub_pane => self.handle_settings_key(key),
            KeyCode::Esc if agents_list && !self.profiles.filter.is_empty() => {
                self.clear_profile_filter();
                None
            }
            KeyCode::Esc => {
                self.close_control_center();
                None
            }
            // `y` copies the whole body of a read-only tab: the Session rows
            // as `Label: value` lines, Usage and Context as their own text.
            KeyCode::Char('y')
                if matches!(
                    self.control_tab,
                    Some(
                        ControlTab::Providers
                            | ControlTab::Usage
                            | ControlTab::Context
                            | ControlTab::Session
                    )
                ) =>
            {
                self.copy_active_tab();
                None
            }
            KeyCode::Left | KeyCode::BackTab => {
                let tab = self.control_tab.map(|tab| tab.shifted(-1))?;
                self.activate_control_tab(tab)
            }
            KeyCode::Right | KeyCode::Tab => {
                let tab = self
                    .control_tab
                    .map(|tab| tab.shifted(if shift { -1 } else { 1 }))?;
                self.activate_control_tab(tab)
            }
            _ if self.control_tab == Some(ControlTab::Settings) => self.handle_settings_key(key),
            _ if self.control_tab == Some(ControlTab::Models) => self.handle_agents_key(key),
            // The Providers tab has no actions: Enter focuses the block under
            // the cursor and emits nothing (spec 02 §5.5).
            _ if self.control_tab == Some(ControlTab::Providers) && !self.providers.is_empty() => {
                let last = self.providers.len() - 1;
                match code {
                    KeyCode::Up => self.providers_focus = self.providers_focus.saturating_sub(1),
                    KeyCode::Down => {
                        self.providers_focus = self.providers_focus.saturating_add(1).min(last);
                    }
                    KeyCode::Enter => {}
                    _ => return None,
                }
                self.mark_dirty();
                None
            }
            _ if self.control_tab == Some(ControlTab::Usage)
                && !self.usage_refresh.accounts.is_empty() =>
            {
                match code {
                    // The first Up/Down only shows the focus on the current
                    // action; nothing is highlighted when Usage opens.
                    KeyCode::Up | KeyCode::Down if !self.usage_refresh.focus_shown => {
                        self.usage_refresh.focus_shown = true;
                    }
                    KeyCode::Up => {
                        self.usage_refresh.focus = self.usage_refresh.focus.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        self.usage_refresh.focus = self
                            .usage_refresh
                            .focus
                            .saturating_add(1)
                            .min(self.usage_refresh.accounts.len() - 1);
                    }
                    // Enter activates only a focus the operator can see.
                    KeyCode::Enter if !self.usage_refresh.focus_shown => return None,
                    KeyCode::Enter => {
                        let account = self
                            .usage_refresh
                            .accounts
                            .get(self.usage_refresh.focus)?
                            .clone();
                        return self.start_usage_refresh(account);
                    }
                    _ => return None,
                }
                self.usage_refresh.reveal = true;
                self.mark_dirty();
                None
            }
            KeyCode::Up | KeyCode::Down => {
                let delta = if code == KeyCode::Up { -1 } else { 1 };
                let scroll = &mut self.control_scroll[self.control_tab?.index()];
                if scroll.scroll_by(delta) {
                    self.mark_dirty();
                }
                None
            }
            _ => None,
        }
    }

    /// Opens `tab` without emitting an action that could re-route the tab.
    /// Scrolls the one Usage page to the next (`delta >= 0`) or previous
    /// Copies the body of the active read-only tab (spec 02 §5.5).
    pub(super) fn start_usage_refresh(&mut self, account: String) -> Option<UserAction> {
        if self.usage_refresh.active.is_some() || !self.usage_refresh.accounts.contains(&account) {
            return None;
        }
        if let Some(index) = self
            .usage_refresh
            .accounts
            .iter()
            .position(|id| id == &account)
        {
            self.usage_refresh.focus = index;
        }
        self.usage_refresh.active = Some(account.clone());
        self.usage_refresh.error = None;
        self.mark_dirty();
        Some(UserAction::Extension(
            SurfaceIntent::RefreshUsageCredential { account },
        ))
    }

    pub(super) fn maybe_reread_usage(&mut self, actions: Option<&crate::tui::UserActionSender>) {
        if self.usage_refresh.pending_read
            && let Some(actions) = actions
            && actions
                .send(UserAction::Extension(SurfaceIntent::RereadUsage))
                .is_ok()
        {
            self.usage_refresh.pending_read = false;
        }
    }

    pub(super) fn copy_active_tab(&mut self) {
        let Some(tab) = self.control_tab else {
            return;
        };
        if tab == ControlTab::Session {
            copy_text_osc52(&usage_tab::session_copy_text(&self.session_rows));
            return;
        }
        if let Some((_, text)) = self
            .tab_copy_text
            .iter()
            .find(|(candidate, _)| *candidate == tab)
        {
            copy_text_osc52(text);
        }
    }

    pub(super) fn activate_control_tab(&mut self, tab: ControlTab) -> Option<UserAction> {
        self.control_tab = Some(tab);
        self.last_control_tab = tab;
        if tab == ControlTab::Keybinds {
            self.keybinds_full = false;
            self.control_scroll[tab.index()].home();
        }
        self.mark_dirty();
        match tab {
            ControlTab::Models => Some(self.request_profiles()),
            ControlTab::Settings => {
                if self.settings_menu.is_none() {
                    self.settings_menu = self.settings_snapshot.clone().map(SettingsMenu::open);
                }
                // The host refreshes the snapshot; the tab is already set, and
                // `OpenSettings` no longer moves it (spec 02 §5.5).
                Some(UserAction::Extension(SurfaceIntent::OpenSettings))
            }
            // The Providers tab is the product's honesty about credentials, so
            // opening it is itself a credential read (spec 02 §5.5). The account
            // line reads whatever identity the host already published — the
            // Usage projection or the key store — and never fetches one of its
            // own, because this tab performs no provider call.
            ControlTab::Providers => {
                self.providers_focus = self
                    .providers_focus
                    .min(self.providers.len().saturating_sub(1));
                Some(UserAction::RefreshReadiness)
            }
            ControlTab::Usage | ControlTab::Context => {
                self.usage_refresh.focus = 0;
                self.usage_refresh.focus_shown = false;
                self.usage_refresh.hover = None;
                self.usage_refresh.reveal = true;
                self.pending_open_usage = true;
                Some(UserAction::Extension(SurfaceIntent::OpenUsage))
            }
            // The Session tab's `Root template` row only prints a name the host
            // projected as a template, so opening it re-reads that projection.
            ControlTab::Session => Some(UserAction::RefreshTemplates),
            ControlTab::Keybinds => None,
        }
    }

    /// Moves the info overlay's scroll, repainting only when it actually moved.
    pub(super) fn scroll_info_overlay(
        &mut self,
        step: impl FnOnce(&mut modal_chrome::ScrollState) -> bool,
    ) {
        if step(&mut self.info_overlay_scroll) {
            self.mark_dirty();
        }
    }

    /// Opens the Menu on `tab`, the one path every §2.1 entry uses.
    /// The composer draft is deliberately left in place: a modal owns the key
    /// chain while it is open, so the parked draft cannot be edited and is
    /// still there when the modal closes. Slash entries reach here with an
    /// already-consumed draft — `submit_line` clears it before dispatch.
    pub(super) fn open_control_tab(&mut self, tab: ControlTab) -> Option<UserAction> {
        self.slash_open = false;
        self.info_overlay = None;
        self.activate_control_tab(tab)
    }

    /// Whether a surface other than the composer currently owns the key chain.
    /// A late `/resume` reply must not stack a picker over one of these, since
    /// the chain would still route to the earlier owner.
    pub(super) fn a_modal_owns_input(&self) -> bool {
        self.activity_tab.is_some()
            || self.destructive_confirm.is_some()
            || self.info_overlay.is_some()
            || self.resume_menu.is_some()
            || self.control_tab.is_some()
    }

    /// Dismisses the picker, and rescues a launch that has nothing attached.
    ///
    /// `plak resume` and `plak --resume` open the picker over a shell host that
    /// was never asked to attach, so before this an `Esc` left the product in
    /// `not ready · pick a session to resume` forever and silently queued every
    /// prompt typed after it; the only escape was an `/attach` nothing on screen
    /// mentioned. Declining now attaches the workspace's default root
    /// binding — what a bare `plak` does — and says so in one line. When no
    /// default is attachable the launcher quits cleanly instead of parking in a
    /// not-ready shell.
    pub(super) fn decline_resume_menu(&mut self) -> Option<UserAction> {
        let stranded = self.launch_resume_picker && !self.agent_enabled;
        self.launch_resume_picker = false;
        self.close_resume_menu();
        self.arm_launch_intro();
        if !stranded {
            return None;
        }
        self.declined_launch_resume_picker = true;
        // The note itself is carried by the launcher into the round it starts
        // (`TuiLaunch::opening_note`): a block pushed here would be discarded
        // with this App when the reattach tears the presentation down.
        self.reattach_note = Some(crate::tui::DECLINED_RESUME_STATUS.to_owned());
        Some(UserAction::RetryAttach)
    }

    /// Closes the resume picker and retires any outstanding `/resume` request,
    /// so a reply still in flight cannot reopen it.
    pub(super) fn close_resume_menu(&mut self) {
        self.resume_menu = None;
        self.pending_open_resume = false;
        self.resume_popup = None;
        self.resume_close_hit = None;
        self.resume_close_hover = false;
        self.resume_hits.clear();
        self.resume_hover = None;
        self.mark_dirty();
    }

    pub(super) fn close_control_center(&mut self) {
        self.theme = theme_for(self.persisted_theme);
        self.control_tab = None;
        self.scrollbar_drag = None;
        self.pending_open_settings = false;
        self.pending_open_usage = false;
        self.profiles.filter.clear();
        self.profiles.hits.clear();
        self.settings_menu = None;
        self.settings_error = None;
        // Reverting the previewed theme also reverts the previewed render
        // prefs: an Esc must leave nothing of the draft behind.
        self.sync_render_prefs();
        self.control_scroll = [modal_chrome::ScrollState::default(); ControlTab::ALL.len()];
        self.clear_control_hits();
        self.mark_dirty();
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Menu mouse dispatch keeps tab, close, settings, and models hits in one owner"
    )]
    pub(super) fn handle_control_mouse(&mut self, mouse: MouseEvent) -> Option<UserAction> {
        let point = (mouse.column, mouse.row).into();
        // The close glyph brightens under the pointer (Grok Build §1.2). It is
        // the only hover the border has, so it is tracked before anything else
        // claims the move.
        let close_hover = self
            .control_close_hit
            .is_some_and(|rect| rect.contains(point));
        if self.control_close_hover != close_hover {
            self.control_close_hover = close_hover;
            self.mark_dirty();
        }
        if matches!(mouse.kind, MouseEventKind::Moved)
            && self.control_tab == Some(ControlTab::Models)
        {
            if let Some((index, _)) = self
                .profiles
                .hits
                .iter()
                .find(|(_, rect)| rect.contains(point))
                .copied()
            {
                self.hover_profile(index);
            }
            return None;
        }
        // Hover highlights a Usage `[refresh]` without moving focus.
        if matches!(mouse.kind, MouseEventKind::Moved)
            && self.control_tab == Some(ControlTab::Usage)
        {
            let hover = self
                .usage_refresh_hits
                .iter()
                .find(|(_, rect)| rect.contains(point))
                .map(|(account, _)| account.clone());
            if self.usage_refresh.hover != hover {
                self.usage_refresh.hover = hover;
                self.mark_dirty();
            }
            return None;
        }
        // Hover reveals a row's description without focusing or activating it
        // (spec 02 §5.5).
        if matches!(mouse.kind, MouseEventKind::Moved)
            && self.control_tab == Some(ControlTab::Settings)
        {
            // Only the browse list has row hover; a chooser's hits index its
            // choices, not registry rows.
            let browsing = self
                .settings_menu
                .as_ref()
                .is_some_and(|menu| matches!(menu.pane, SettingsPane::Browse));
            let hover = picker::hit_at(&self.settings_hits, mouse.column, mouse.row)
                .filter(|_| browsing)
                .map(|hit| hit.index);
            if self.settings_hover != hover {
                self.settings_hover = hover;
                self.mark_dirty();
            }
            return None;
        }
        if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left)) {
            if self
                .control_close_hit
                .is_some_and(|rect| rect.contains(point))
            {
                self.close_control_center();
                return None;
            }
            if let Some((tab, _)) = self
                .control_tab_hits
                .iter()
                .find(|(_, rect)| rect.contains(point))
                .copied()
            {
                return self.activate_control_tab(tab);
            }
            if self
                .control_popup
                .is_some_and(|popup| !popup.contains(point))
            {
                self.close_control_center();
                return None;
            }
            if self.control_tab == Some(ControlTab::Usage)
                && let Some((account, _)) = self
                    .usage_refresh_hits
                    .iter()
                    .find(|(_, rect)| rect.contains(point))
            {
                return self.start_usage_refresh(account.clone());
            }
            if self.control_tab == Some(ControlTab::Session)
                && let Some((row, _)) = self
                    .session_row_hits
                    .iter()
                    .find(|(_, rect)| rect.contains(point))
                    .copied()
                && let Some(row) = self.session_rows.get(row)
            {
                copy_text_osc52(&usage_tab::session_row_copy_text(row));
                return None;
            }
            // A click on a Providers block only focuses it: the tab has no
            // actions, so there is no second stage (spec 02 §5.5).
            if self.control_tab == Some(ControlTab::Providers)
                && let Some((index, _)) = self
                    .provider_row_hits
                    .iter()
                    .find(|(_, rect)| rect.contains(point))
                    .copied()
            {
                if self.providers_focus != index {
                    self.providers_focus = index;
                    self.mark_dirty();
                }
                return None;
            }
            if self.control_tab == Some(ControlTab::Settings)
                && let Some(hit) = picker::hit_at(&self.settings_hits, mouse.column, mouse.row)
            {
                if let Some(SettingsPane::Choosing(_)) =
                    self.settings_menu.as_ref().map(|menu| menu.pane)
                {
                    return self.click_settings_choice(hit.index);
                }
                let double = self.list_clicks.click(
                    picker::ClickList::Settings,
                    &hit.index.to_string(),
                    Instant::now(),
                );
                let outcome = picker::classify_click(double, &hit, mouse.column);
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.selected = hit.index;
                }
                self.mark_dirty();
                if let picker::ClickOutcome::Activate(index) = outcome {
                    let action = self.activate_settings_row(index);
                    self.sync_render_prefs();
                    return action;
                }
                return None;
            }
            if self.control_tab == Some(ControlTab::Models)
                && let Some((index, _)) = self
                    .profiles
                    .hits
                    .iter()
                    .find(|(_, rect)| rect.contains(point))
                    .copied()
            {
                let double = self.list_clicks.click(
                    picker::ClickList::Models,
                    &index.to_string(),
                    Instant::now(),
                );
                self.click_profile(index, double);
                return None;
            }
            return None;
        }
        // The wheel scrolls every tab; on Settings it also moves the focus so
        // the description under the pointer stays the described row.
        let tab = self.control_tab?;
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return None,
        };
        if tab == ControlTab::Settings
            && self
                .settings_menu
                .as_ref()
                .is_some_and(|menu| menu.pane == SettingsPane::Browse)
        {
            self.move_settings_selection(delta);
            self.mark_dirty();
            return None;
        }
        if tab == ControlTab::Models {
            self.move_profile_cursor(isize::from(delta));
            return None;
        }
        // The wheel moves the labels out from under the pointer.
        if tab == ControlTab::Usage {
            self.usage_refresh.hover = None;
        }
        if self.control_scroll[tab.index()].scroll_by(isize::from(delta)) {
            self.mark_dirty();
        }
        None
    }

    pub(super) fn handle_resume_mouse(&mut self, mouse: MouseEvent) -> Option<UserAction> {
        let point = (mouse.column, mouse.row).into();
        match mouse.kind {
            MouseEventKind::ScrollUp
                if self.resume_popup.is_some_and(|popup| popup.contains(point)) =>
            {
                self.resume_menu.as_mut()?.move_selection(-1);
                self.mark_dirty();
            }
            MouseEventKind::ScrollDown
                if self.resume_popup.is_some_and(|popup| popup.contains(point)) =>
            {
                self.resume_menu.as_mut()?.move_selection(1);
                self.mark_dirty();
            }
            MouseEventKind::Moved => {
                let close_hover = self
                    .resume_close_hit
                    .is_some_and(|rect| rect.contains(point));
                if self.resume_close_hover != close_hover {
                    self.resume_close_hover = close_hover;
                    self.mark_dirty();
                }
                // §5.5: hover paints the band and nothing else; focus moves
                // only on a click.
                let hovered =
                    picker::hit_at(&self.resume_hits, mouse.column, mouse.row).map(|hit| hit.index);
                if hovered != self.resume_hover {
                    self.resume_hover = hovered;
                    self.mark_dirty();
                }
            }
            MouseEventKind::Down(event::MouseButton::Left) => {
                if self
                    .resume_close_hit
                    .is_some_and(|rect| rect.contains(point))
                    || self
                        .resume_popup
                        .is_some_and(|popup| !popup.contains(point))
                {
                    return self.decline_resume_menu();
                }
                let hit = picker::hit_at(&self.resume_hits, mouse.column, mouse.row)?;
                let double = self.list_clicks.click(
                    picker::ClickList::Resume,
                    &hit.index.to_string(),
                    Instant::now(),
                );
                match picker::classify_click(double, &hit, mouse.column) {
                    picker::ClickOutcome::Focus(index) => {
                        self.resume_menu.as_mut()?.selected = index;
                        self.mark_dirty();
                    }
                    picker::ClickOutcome::Activate(index) => {
                        self.resume_menu.as_mut()?.selected = index;
                        return self.accept_resume_selection();
                    }
                }
            }
            _ => {}
        }
        None
    }
}
