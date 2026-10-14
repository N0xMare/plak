//! Typed product-settings pane state, generated from the metadata registry.
//!
//! The pane holds a draft, never parses `config.toml`, and emits one typed save
//! intent (spec 02 §5.5). Row identity, ordering, defaults, search terms and
//! descriptions all come from [`crate::tui::settings_registry`].

use crate::tui::settings_registry::{self, SettingKind, SettingMeta};
use crate::tui::text_input::is_accepted_single_line_char;
use crate::tui::{SettingsSnapshot, SettingsTheme};

/// Live-preview enum chooser opened over a settings row (spec 02 §5.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnumChooser {
    /// Registry index of the row being chosen.
    pub row: usize,
    /// Choice under the cursor; previewed live when the row previews.
    pub focus: usize,
    /// Choice that was committed when the chooser opened, restored on `Esc`.
    pub committed: usize,
}

/// Bounded integer stepper opened over a settings row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntStepper {
    /// Registry index of the row being stepped.
    pub row: usize,
    /// Pending value.
    pub value: u16,
    /// Value that was committed when the stepper opened, restored on `Esc`.
    pub committed: u16,
}

/// Which sub-pane, if any, is open over the settings list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SettingsPane {
    /// The row list itself.
    #[default]
    Browse,
    /// A live-preview enum chooser.
    Choosing(EnumChooser),
    /// A bounded integer stepper.
    Stepping(IntStepper),
}

/// Interactive settings pane state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsMenu {
    /// Editable draft.
    pub draft: SettingsSnapshot,
    /// Selected registry row.
    pub selected: usize,
    /// Search query over label, description and keywords.
    pub filter: String,
    /// Whether the search box has focus.
    pub search_active: bool,
    /// Open sub-pane.
    pub pane: SettingsPane,
}

impl SettingsMenu {
    /// Registry row count.
    #[cfg(test)]
    pub const ROW_COUNT: usize = settings_registry::ROWS.len();

    /// Opens the pane over an authoritative snapshot.
    #[must_use]
    pub const fn open(snapshot: SettingsSnapshot) -> Self {
        Self {
            draft: snapshot,
            selected: 0,
            filter: String::new(),
            search_active: false,
            pane: SettingsPane::Browse,
        }
    }

    /// Metadata for a registry row.
    #[must_use]
    pub fn meta(index: usize) -> Option<&'static SettingMeta> {
        settings_registry::ROWS.get(index)
    }

    /// Current value of a `Bool` row.
    #[must_use]
    pub fn bool_value(&self, key: &str) -> Option<bool> {
        Some(match key {
            settings_registry::MOUSE => self.draft.mouse,
            settings_registry::SHOW_THINKING => self.draft.show_thinking,
            settings_registry::GROUP_TOOL_CALLS => self.draft.group_tool_calls,
            settings_registry::AUTO_COMPACTION => self.draft.compaction,
            settings_registry::REDUCED_MOTION => self.draft.reduced_motion,
            _ => return None,
        })
    }

    /// Writes a `Bool` row.
    pub fn set_bool(&mut self, key: &str, value: bool) {
        match key {
            settings_registry::MOUSE => self.draft.mouse = value,
            settings_registry::SHOW_THINKING => self.draft.show_thinking = value,
            settings_registry::GROUP_TOOL_CALLS => self.draft.group_tool_calls = value,
            settings_registry::AUTO_COMPACTION => self.draft.compaction = value,
            settings_registry::REDUCED_MOTION => self.draft.reduced_motion = value,
            _ => {}
        }
    }

    /// Current value of an `Enum` row.
    #[must_use]
    pub fn enum_value(&self, key: &str) -> Option<&'static str> {
        Some(match key {
            settings_registry::THEME => self.draft.theme.as_str(),
            _ => return None,
        })
    }

    /// Writes an `Enum` row from a canonical label.
    pub fn set_enum(&mut self, key: &str, value: &str) {
        if key == settings_registry::THEME
            && let Some(theme) = SettingsTheme::parse(value)
        {
            self.draft.theme = theme;
        }
    }

    /// Current value of an `Int` row.
    #[must_use]
    pub fn int_value(&self, key: &str) -> Option<u16> {
        match key {
            settings_registry::SCROLL_LINES => Some(self.draft.scroll_lines),
            _ => None,
        }
    }

    /// Writes an `Int` row, clamped to the registry bounds.
    pub fn set_int(&mut self, key: &str, value: u16) {
        let Some(meta) = settings_registry::meta(key) else {
            return;
        };
        let SettingKind::Int { min, max, .. } = meta.kind else {
            return;
        };
        let value = value.clamp(min, max);
        if key == settings_registry::SCROLL_LINES {
            self.draft.scroll_lines = value;
        }
    }

    /// Right-column text for a row.
    #[must_use]
    pub fn value_label(&self, index: usize) -> String {
        let Some(meta) = Self::meta(index) else {
            return String::new();
        };
        match meta.kind {
            SettingKind::Bool { .. } => self
                .bool_value(meta.key)
                .map(|value| if value { "on" } else { "off" }.to_owned())
                .unwrap_or_default(),
            SettingKind::Enum { .. } => self.enum_value(meta.key).unwrap_or_default().to_owned(),
            SettingKind::Int { .. } => self
                .int_value(meta.key)
                .map(|value| value.to_string())
                .unwrap_or_default(),
            SettingKind::Action => String::new(),
        }
    }

    /// Whether a row's current value is recessive (an off switch).
    #[must_use]
    pub fn is_recessive(&self, index: usize) -> bool {
        Self::meta(index).is_some_and(|meta| {
            matches!(meta.kind, SettingKind::Bool { .. })
                && self.bool_value(meta.key) == Some(false)
        })
    }

    /// Restores a row to its config-schema default.
    pub fn reset_to_default(&mut self, index: usize) {
        let Some(meta) = Self::meta(index) else {
            return;
        };
        match meta.kind {
            SettingKind::Bool { default } => self.set_bool(meta.key, default),
            SettingKind::Enum { default, .. } => self.set_enum(meta.key, default),
            SettingKind::Int { default, .. } => self.set_int(meta.key, default),
            SettingKind::Action => {}
        }
    }

    /// Label naming the default a reset would install, for the confirmation.
    #[must_use]
    pub fn default_label(index: usize) -> String {
        match Self::meta(index).map(|meta| meta.kind) {
            Some(SettingKind::Bool { default }) => if default { "on" } else { "off" }.to_owned(),
            Some(SettingKind::Enum { default, .. }) => default.to_owned(),
            Some(SettingKind::Int { default, .. }) => default.to_string(),
            Some(SettingKind::Action) | None => String::new(),
        }
    }

    /// Row indices matching the current filter, in display order.
    #[must_use]
    pub fn visible_rows(&self) -> Vec<usize> {
        settings_registry::ROWS
            .iter()
            .enumerate()
            .filter(|(_, meta)| meta.matches(&self.filter))
            .map(|(index, _)| index)
            .collect()
    }

    fn snap_selected_to_visible(&mut self) {
        let visible = self.visible_rows();
        if visible.is_empty() {
            return;
        }
        if !visible.contains(&self.selected) {
            self.selected = visible[0];
        }
    }

    /// Append a type-to-search character.
    pub fn push_filter_char(&mut self, c: char) {
        if !is_accepted_single_line_char(c) {
            return;
        }
        self.filter.push(c);
        self.snap_selected_to_visible();
    }

    /// Edit the type-to-search string.
    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.snap_selected_to_visible();
    }

    /// Moves selection with wraparound among currently visible rows.
    pub fn move_selection(&mut self, delta: i8) {
        let visible = self.visible_rows();
        if visible.is_empty() {
            return;
        }
        let pos = visible
            .iter()
            .position(|&index| index == self.selected)
            .unwrap_or(0);
        let next = if delta < 0 {
            if pos == 0 { visible.len() - 1 } else { pos - 1 }
        } else {
            (pos + 1) % visible.len()
        };
        self.selected = visible[next];
    }

    /// `PageUp`/`PageDown`/`Home`/`End` over the visible rows (spec 02 §5.5).
    ///
    /// Moves the *focus*: the draw path reveals the focused row every frame,
    /// so moving only the scroll offset snaps straight back and the four keys
    /// read as no-ops.
    pub fn page_selection(&mut self, code: crossterm::event::KeyCode, page: usize) -> bool {
        use crossterm::event::KeyCode;
        let visible = self.visible_rows();
        if visible.is_empty() {
            return false;
        }
        let pos = visible
            .iter()
            .position(|&index| index == self.selected)
            .unwrap_or(0);
        let page = page.max(1).min(visible.len());
        let next = match code {
            KeyCode::PageUp => pos.saturating_sub(page),
            KeyCode::PageDown => (pos + page).min(visible.len() - 1),
            KeyCode::Home => 0,
            KeyCode::End => visible.len() - 1,
            _ => return false,
        };
        let moved = visible[next] != self.selected;
        self.selected = visible[next];
        moved
    }

    /// Opens the sub-pane a row's kind calls for, or reports the action to fire.
    #[must_use]
    pub fn activate(&mut self, index: usize) -> Option<&'static str> {
        let meta = Self::meta(index)?;
        match meta.kind {
            SettingKind::Bool { .. } => {
                let value = self.bool_value(meta.key).unwrap_or_default();
                self.set_bool(meta.key, !value);
                None
            }
            SettingKind::Enum { choices, .. } => {
                let current = self.enum_value(meta.key).unwrap_or_default();
                let committed = choices
                    .iter()
                    .position(|choice| choice.value == current)
                    .unwrap_or(0);
                self.pane = SettingsPane::Choosing(EnumChooser {
                    row: index,
                    focus: committed,
                    committed,
                });
                None
            }
            SettingKind::Int { .. } => {
                let value = self.int_value(meta.key).unwrap_or_default();
                self.pane = SettingsPane::Stepping(IntStepper {
                    row: index,
                    value,
                    committed: value,
                });
                None
            }
            SettingKind::Action => Some(meta.key),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests {
    use super::{SettingsMenu, SettingsPane, SettingsSnapshot, SettingsTheme};
    use crate::tui::CompositionSelection;
    use crate::tui::settings_registry::{self, SettingKind};

    fn menu() -> SettingsMenu {
        SettingsMenu::open(SettingsSnapshot::default())
    }

    #[test]
    fn theme_labels_and_cycle_cover_all_six() {
        assert_eq!(
            SettingsTheme::ALL.map(SettingsTheme::as_str),
            [
                "default-dark",
                "default-light",
                "catppuccin-latte",
                "catppuccin-frappe",
                "catppuccin-macchiato",
                "catppuccin-mocha",
            ]
        );
        for (index, theme) in SettingsTheme::ALL.into_iter().enumerate() {
            assert_eq!(theme.next(), SettingsTheme::ALL[(index + 1) % 6]);
            assert_eq!(SettingsTheme::parse(theme.as_str()), Some(theme));
        }
        assert_eq!(
            SettingsTheme::parse("dark"),
            Some(SettingsTheme::DefaultDark)
        );
        assert_eq!(
            SettingsTheme::parse("light"),
            Some(SettingsTheme::DefaultLight)
        );
    }

    #[test]
    fn composition_labels_and_cycle_are_closed_and_stable() {
        let compositions = [
            CompositionSelection::Direct,
            CompositionSelection::FollowCatalog,
            CompositionSelection::NativePtc,
            CompositionSelection::JsPtc,
            CompositionSelection::PyPtc,
        ];
        assert_eq!(
            compositions.map(CompositionSelection::as_str),
            [
                "direct",
                "plakous-default",
                "native-ptc",
                "js-ptc",
                "py-ptc"
            ]
        );
        assert_eq!(CompositionSelection::ALL, compositions);
        for composition in compositions {
            assert_eq!(
                CompositionSelection::parse(composition.as_str()),
                Some(composition)
            );
        }
    }

    #[test]
    fn the_draft_opens_on_the_registry_defaults() {
        let menu = menu();
        for (index, meta) in settings_registry::ROWS.iter().enumerate() {
            match meta.kind {
                SettingKind::Bool { default } => {
                    assert_eq!(menu.bool_value(meta.key), Some(default), "{}", meta.key);
                }
                SettingKind::Enum { default, .. } => {
                    assert_eq!(menu.enum_value(meta.key), Some(default), "{}", meta.key);
                }
                SettingKind::Int { default, .. } => {
                    assert_eq!(menu.int_value(meta.key), Some(default), "{}", meta.key);
                }
                SettingKind::Action => {
                    assert_eq!(menu.value_label(index), "");
                }
            }
        }
    }

    #[test]
    fn search_matches_label_description_and_keywords() {
        let mut menu = menu();
        assert_eq!(menu.visible_rows().len(), SettingsMenu::ROW_COUNT);
        for c in "completed activity groups".chars() {
            menu.push_filter_char(c);
        }
        assert_eq!(
            menu.visible_rows()
                .into_iter()
                .map(|index| SettingsMenu::meta(index).unwrap().label)
                .collect::<Vec<_>>(),
            vec!["Group tool calls"],
            "description text is searchable"
        );
        menu.filter.clear();
        for c in "accessibility".chars() {
            menu.push_filter_char(c);
        }
        assert_eq!(
            menu.visible_rows()
                .into_iter()
                .map(|index| SettingsMenu::meta(index).unwrap().label)
                .collect::<Vec<_>>(),
            vec!["Reduced motion"],
            "keywords are searchable"
        );
    }

    #[test]
    fn activating_each_kind_opens_the_right_sub_pane() {
        let mut menu = menu();
        let mouse = settings_registry::index_of(settings_registry::MOUSE).unwrap();
        assert!(menu.activate(mouse).is_none());
        assert_eq!(menu.bool_value(settings_registry::MOUSE), Some(false));
        assert_eq!(menu.pane, SettingsPane::Browse);

        let theme = settings_registry::index_of(settings_registry::THEME).unwrap();
        assert!(menu.activate(theme).is_none());
        assert!(matches!(menu.pane, SettingsPane::Choosing(_)));

        menu.pane = SettingsPane::Browse;
        let lines = settings_registry::index_of(settings_registry::SCROLL_LINES).unwrap();
        assert!(menu.activate(lines).is_none());
        assert!(matches!(menu.pane, SettingsPane::Stepping(_)));

        menu.pane = SettingsPane::Browse;
        let raw = settings_registry::index_of(settings_registry::EDIT_RAW_CONFIG).unwrap();
        assert_eq!(menu.activate(raw), Some(settings_registry::EDIT_RAW_CONFIG));
    }

    #[test]
    fn int_rows_clamp_to_the_registry_bounds() {
        let mut menu = menu();
        menu.set_int(settings_registry::SCROLL_LINES, 0);
        assert_eq!(menu.int_value(settings_registry::SCROLL_LINES), Some(1));
        menu.set_int(settings_registry::SCROLL_LINES, 99);
        assert_eq!(menu.int_value(settings_registry::SCROLL_LINES), Some(10));
    }

    #[test]
    fn reset_restores_the_schema_default() {
        let mut menu = menu();
        let row = settings_registry::index_of(settings_registry::THEME).unwrap();
        menu.draft.theme = SettingsTheme::CatppuccinMocha;
        assert_eq!(SettingsMenu::default_label(row), "default-dark");
        menu.reset_to_default(row);
        assert_eq!(menu.draft.theme, SettingsTheme::DefaultDark);
    }
}
