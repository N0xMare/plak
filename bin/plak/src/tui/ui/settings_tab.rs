//! Settings tab state and keys.

use super::{
    App, Instant, KeyCode, KeyEvent, KeyModifiers, SettingKind, SettingsMenu, SettingsPane,
    UserAction, is_accepted_single_line_char, picker, settings_registry, theme_for,
};
use crate::tui::SurfaceIntent;

impl App {
    pub(super) fn move_settings_selection(&mut self, delta: i8) {
        let Some(menu) = self.settings_menu.as_mut() else {
            return;
        };
        menu.move_selection(delta);
    }

    /// Fires the action a settings row names, or `None` when the row mutated
    /// the draft in place.
    pub(super) fn activate_settings_row(&mut self, index: usize) -> Option<UserAction> {
        let action = self.settings_menu.as_mut()?.activate(index);
        // Every activation changes what is painted: a Bool flips in place, an
        // Enum or Int row opens its sub-pane over the list. The frame must
        // land on this key, not on the next one the operator sends into a
        // modal they cannot see.
        self.mark_dirty();
        let Some(action) = action else {
            // A Bool row flipped in place, so it persists immediately;
            // the host is the only authority that can refuse it and the
            // refusal lands inline as `SettingsSaveFailed`. An Enum or Int row
            // only *opened* its sub-pane and has changed nothing yet — its own
            // commit is what applies.
            return matches!(
                SettingsMenu::meta(index).map(|meta| meta.kind),
                Some(SettingKind::Bool { .. })
            )
            .then(|| self.persist_settings())
            .flatten();
        };
        match action {
            settings_registry::EDIT_RAW_CONFIG => {
                self.close_control_center();
                Some(UserAction::Extension(SurfaceIntent::OpenConfigFile))
            }
            _ => None,
        }
    }

    /// `d` on a value row opens the shared confirmation pop-up, focused on
    /// Reset. The pop-up binds the row's key. The draft is unchanged until
    /// Enter or a click confirms, and the reset is computed from the live
    /// draft then.
    pub(super) fn confirm_settings_reset(&mut self, row: usize) {
        let Some(meta) = SettingsMenu::meta(row) else {
            return;
        };
        if matches!(meta.kind, SettingKind::Action) {
            return;
        }
        let label = meta.label;
        let default = SettingsMenu::default_label(row);
        self.open_setting_reset(
            meta.key,
            format!("Reset {label}?"),
            vec![format!("{label} returns to {default}.")],
        );
    }

    /// Persists the current draft. Every row applies on change (spec 02 §5.5), so
    /// this runs on the mutation itself, never on a Save row.
    ///
    /// The TUI never validates the value: the draft always goes to the host,
    /// which answers with `SettingsApplied` or a refusal the tab paints inline.
    pub(super) fn persist_settings(&mut self) -> Option<UserAction> {
        let update = self.settings_menu.as_ref()?.draft.clone();
        self.mark_dirty();
        Some(UserAction::Extension(SurfaceIntent::SaveSettings(update)))
    }

    /// A click on a chooser's choice focuses and previews it; a double-click
    /// commits it exactly as Enter does.
    pub(super) fn click_settings_choice(&mut self, index: usize) -> Option<UserAction> {
        let double = self.list_clicks.click(
            picker::ClickList::SettingsChoice,
            &index.to_string(),
            Instant::now(),
        );
        if let Some(menu) = self.settings_menu.as_mut()
            && let SettingsPane::Choosing(chooser) = &mut menu.pane
        {
            chooser.focus = index;
        }
        self.mark_dirty();
        self.preview_settings_choice();
        if double {
            return self.handle_settings_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        None
    }

    /// Applies the chooser's focused value to the live theme while it previews.
    pub(super) fn preview_settings_choice(&mut self) {
        let Some(menu) = self.settings_menu.as_ref() else {
            return;
        };
        let SettingsPane::Choosing(chooser) = menu.pane else {
            return;
        };
        let Some(meta) = SettingsMenu::meta(chooser.row) else {
            return;
        };
        if !meta.previews() {
            return;
        }
        let Some(choice) = meta.choices().get(chooser.focus).map(|choice| choice.value) else {
            return;
        };
        if let Some(menu) = self.settings_menu.as_mut() {
            menu.set_enum(meta.key, choice);
        }
        if meta.key == settings_registry::THEME
            && let Some(theme) = self.settings_menu.as_ref().map(|menu| menu.draft.theme)
        {
            self.theme = theme_for(theme);
        }
        self.mark_dirty();
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one owner routes browse, search, chooser, stepper and reset in the Settings tab"
    )]
    pub(super) fn handle_settings_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        let action = self.handle_settings_key_inner(key);
        self.sync_render_prefs();
        action
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one owner routes browse, search, chooser, stepper and reset in the Settings tab"
    )]
    pub(super) fn handle_settings_key_inner(&mut self, key: KeyEvent) -> Option<UserAction> {
        let code = key.code;
        let pane = self.settings_menu.as_ref()?.pane;
        match pane {
            SettingsPane::Choosing(chooser) => {
                let meta = SettingsMenu::meta(chooser.row)?;
                let count = meta.choices().len().max(1);
                match code {
                    KeyCode::Up | KeyCode::Down => {
                        let delta = if code == KeyCode::Up { count - 1 } else { 1 };
                        if let Some(menu) = self.settings_menu.as_mut()
                            && let SettingsPane::Choosing(state) = &mut menu.pane
                        {
                            state.focus = (state.focus + delta) % count;
                        }
                        // A row that does not live-preview still moves its
                        // focus band, so the frame is owed either way.
                        self.mark_dirty();
                        self.preview_settings_choice();
                        None
                    }
                    KeyCode::Enter => {
                        let value = meta.choices().get(chooser.focus).map(|c| c.value);
                        if let Some(menu) = self.settings_menu.as_mut() {
                            if let Some(value) = value {
                                menu.set_enum(meta.key, value);
                            }
                            menu.pane = SettingsPane::Browse;
                        }
                        if meta.key == settings_registry::THEME
                            && let Some(theme) =
                                self.settings_menu.as_ref().map(|menu| menu.draft.theme)
                        {
                            self.theme = theme_for(theme);
                        }
                        self.mark_dirty();
                        self.persist_settings()
                    }
                    KeyCode::Esc => {
                        // Esc reverts the live preview to the value that was
                        // committed when the chooser opened (spec 02 §5.5).
                        let value = meta.choices().get(chooser.committed).map(|c| c.value);
                        if let Some(menu) = self.settings_menu.as_mut() {
                            if let Some(value) = value {
                                menu.set_enum(meta.key, value);
                            }
                            menu.pane = SettingsPane::Browse;
                        }
                        if meta.key == settings_registry::THEME
                            && let Some(theme) =
                                self.settings_menu.as_ref().map(|menu| menu.draft.theme)
                        {
                            self.theme = theme_for(theme);
                        }
                        self.mark_dirty();
                        None
                    }
                    _ => None,
                }
            }
            SettingsPane::Stepping(stepper) => {
                let meta = SettingsMenu::meta(stepper.row)?;
                let SettingKind::Int { min, max, .. } = meta.kind else {
                    return None;
                };
                let large = (max - min) / 10;
                let step = |value: u16, delta: i32| -> u16 {
                    let next = i32::from(value) + delta;
                    u16::try_from(next.clamp(i32::from(min), i32::from(max))).unwrap_or(min)
                };
                match code {
                    KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                        let delta = match code {
                            KeyCode::Up => 1,
                            KeyCode::Down => -1,
                            KeyCode::Right => i32::from(large.max(1)),
                            _ => -i32::from(large.max(1)),
                        };
                        if let Some(menu) = self.settings_menu.as_mut()
                            && let SettingsPane::Stepping(state) = &mut menu.pane
                        {
                            state.value = step(state.value, delta);
                        }
                        self.mark_dirty();
                        None
                    }
                    KeyCode::Enter => {
                        if let Some(menu) = self.settings_menu.as_mut() {
                            menu.set_int(meta.key, stepper.value);
                            menu.pane = SettingsPane::Browse;
                        }
                        self.mark_dirty();
                        self.persist_settings()
                    }
                    KeyCode::Esc => {
                        if let Some(menu) = self.settings_menu.as_mut() {
                            menu.set_int(meta.key, stepper.committed);
                            menu.pane = SettingsPane::Browse;
                        }
                        self.mark_dirty();
                        None
                    }
                    _ => None,
                }
            }
            SettingsPane::Browse => self.handle_settings_browse_key(key),
        }
    }

    pub(super) fn handle_settings_browse_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        let searching = self.settings_menu.as_ref()?.search_active;
        match key.code {
            KeyCode::Esc if searching => {
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.search_active = false;
                    menu.filter.clear();
                }
                self.mark_dirty();
                None
            }
            KeyCode::Esc => {
                self.close_control_center();
                None
            }
            KeyCode::Up => {
                self.move_settings_selection(-1);
                self.mark_dirty();
                None
            }
            KeyCode::Down => {
                self.move_settings_selection(1);
                self.mark_dirty();
                None
            }
            KeyCode::Backspace => {
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.pop_filter_char();
                    self.mark_dirty();
                }
                None
            }
            KeyCode::Enter => {
                let selected = self.settings_menu.as_ref()?.selected;
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.search_active = false;
                }
                self.activate_settings_row(selected)
            }
            KeyCode::Char(' ') if !searching => {
                let selected = self.settings_menu.as_ref()?.selected;
                if matches!(
                    SettingsMenu::meta(selected).map(|meta| meta.kind),
                    Some(SettingKind::Bool { .. })
                ) {
                    return self.activate_settings_row(selected);
                }
                None
            }
            KeyCode::Char('/') if !searching => {
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.search_active = true;
                }
                self.mark_dirty();
                None
            }
            KeyCode::Char('d') if !searching => {
                let selected = self.settings_menu.as_ref()?.selected;
                self.confirm_settings_reset(selected);
                None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::SUPER)
                    && is_accepted_single_line_char(c) =>
            {
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.search_active = true;
                    menu.push_filter_char(c);
                    self.mark_dirty();
                }
                None
            }
            _ => None,
        }
    }
}
