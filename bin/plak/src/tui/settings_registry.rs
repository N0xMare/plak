//! Settings metadata registry (spec 02 §5.5).
//!
//! Settings is generated from this table, never from per-row match arms in the
//! renderer. Each entry is pure metadata — no closures, no function pointers —
//! so the row list, the search index, the footer wording and the schema-default
//! parity test all read the same source. Plak owns the config schema;
//! `settings_registry_defaults_match_the_config_schema` in `crate::config`
//! pins every default here to it. Integer bounds come from that schema.

/// One choice inside an `Enum` row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnumChoice {
    /// Persisted configuration value.
    pub value: &'static str,
    /// Human-readable label.
    pub label: &'static str,
    /// One-sentence explanation shown beside the radio mark.
    pub description: &'static str,
}

/// Closed set of row kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingKind {
    /// `Space` toggles it in place.
    Bool {
        /// Config-schema default.
        default: bool,
    },
    /// `Enter` opens a chooser.
    Enum {
        /// Config-schema default value.
        default: &'static str,
        /// Ordered choices.
        choices: &'static [EnumChoice],
        /// Whether moving the cursor previews the value live (spec 02 §5.5).
        preview: bool,
    },
    /// `Enter` opens a stepper.
    Int {
        /// Config-schema default.
        default: u16,
        /// Inclusive lower bound.
        min: u16,
        /// Inclusive upper bound.
        max: u16,
    },
    /// `Enter` fires it; it holds no value.
    Action,
}

/// Who owns the value behind a row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingOwner {
    /// Persisted in `config.toml` through the typed save intent.
    ProductConfig,
    /// Rendered but not editable here.
    ViewOnly,
}

/// One settings row's complete metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingMeta {
    /// Stable key, also the config path where one exists.
    pub key: &'static str,
    /// Row label.
    pub label: &'static str,
    /// Row kind and schema default.
    pub kind: SettingKind,
    /// Value owner.
    pub owner: SettingOwner,
    /// Whether the change only takes effect on the next TUI launch.
    pub restart_required: bool,
    /// Identity-bearing rows are labelled "new sessions" and never toggle a
    /// live binding (spec 02 §5.5).
    pub new_sessions: bool,
    /// Extra search terms beyond label and description.
    pub keywords: &'static [&'static str],
    /// One-sentence description.
    pub description: &'static str,
}

impl SettingMeta {
    /// Whether `query` matches this row's label, description or keywords.
    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return true;
        }
        self.label.to_ascii_lowercase().contains(&query)
            || self.description.to_ascii_lowercase().contains(&query)
            || self.key.to_ascii_lowercase().contains(&query)
            || self
                .keywords
                .iter()
                .any(|keyword| keyword.to_ascii_lowercase().contains(&query))
    }

    /// Choices when this row is an `Enum`.
    #[must_use]
    pub const fn choices(&self) -> &'static [EnumChoice] {
        match self.kind {
            SettingKind::Enum { choices, .. } => choices,
            _ => &[],
        }
    }

    /// Whether this row previews live while the chooser moves.
    #[must_use]
    pub const fn previews(&self) -> bool {
        matches!(self.kind, SettingKind::Enum { preview: true, .. })
    }
}

/// The six product themes (spec 02 §5.5). Order matches `ThemeId::ALL`.
pub const THEME_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        value: "default-dark",
        label: "Default Dark",
        description: "Plak's high-contrast dark palette.",
    },
    EnumChoice {
        value: "default-light",
        label: "Default Light",
        description: "Plak's high-contrast light palette.",
    },
    EnumChoice {
        value: "catppuccin-latte",
        label: "Catppuccin Latte",
        description: "Catppuccin's light flavor.",
    },
    EnumChoice {
        value: "catppuccin-frappe",
        label: "Catppuccin Frappe",
        description: "Catppuccin's softly muted dark flavor.",
    },
    EnumChoice {
        value: "catppuccin-macchiato",
        label: "Catppuccin Macchiato",
        description: "Catppuccin's medium dark flavor.",
    },
    EnumChoice {
        value: "catppuccin-mocha",
        label: "Catppuccin Mocha",
        description: "Catppuccin's deepest dark flavor.",
    },
];

/// Key of the theme row.
pub const THEME: &str = "tui.theme";
/// Key of the mouse-capture row.
pub const MOUSE: &str = "tui.mouse";
/// Key of the scroll-lines row.
pub const SCROLL_LINES: &str = "tui.scroll_lines";
/// Key of the show-thinking row.
pub const SHOW_THINKING: &str = "tui.show_thinking";
/// Key of the group-tool-calls row.
pub const GROUP_TOOL_CALLS: &str = "tui.group_tool_calls";
/// Key of the auto-compaction row.
pub const AUTO_COMPACTION: &str = "compaction.enabled";
/// Key of the reduced-motion row.
pub const REDUCED_MOTION: &str = "tui.reduced_motion";
/// Key of the raw-config action row.
pub const EDIT_RAW_CONFIG: &str = "action.edit-raw-config";

/// Every settings row, in the order spec 02 §5.5 fixes.
pub const ROWS: &[SettingMeta] = &[
    SettingMeta {
        key: THEME,
        label: "Theme",
        kind: SettingKind::Enum {
            default: "default-dark",
            choices: THEME_CHOICES,
            preview: true,
        },
        owner: SettingOwner::ProductConfig,
        restart_required: false,
        new_sessions: false,
        keywords: &[
            "color",
            "palette",
            "dark",
            "light",
            "catppuccin",
            "contrast",
        ],
        description: "Color palette for the whole product shell.",
    },
    SettingMeta {
        key: MOUSE,
        label: "Mouse capture",
        kind: SettingKind::Bool { default: true },
        owner: SettingOwner::ProductConfig,
        restart_required: true,
        new_sessions: false,
        keywords: &["pointer", "click", "select", "copy", "terminal"],
        description: "Disabling hands text selection back to the terminal.",
    },
    SettingMeta {
        key: SCROLL_LINES,
        label: "Scroll lines",
        kind: SettingKind::Int {
            default: crate::config::DEFAULT_SCROLL_LINES,
            min: crate::config::MIN_SCROLL_LINES,
            max: crate::config::MAX_SCROLL_LINES,
        },
        owner: SettingOwner::ProductConfig,
        restart_required: false,
        new_sessions: false,
        keywords: &["wheel", "speed", "scroll", "notch"],
        description: "Transcript lines moved per wheel tick.",
    },
    SettingMeta {
        key: SHOW_THINKING,
        label: "Show thinking blocks",
        kind: SettingKind::Bool { default: true },
        owner: SettingOwner::ProductConfig,
        restart_required: false,
        new_sessions: false,
        keywords: &["reasoning", "thinking", "chain of thought"],
        description: "Render reasoning blocks, subject to the provider's reasoning policy.",
    },
    SettingMeta {
        key: GROUP_TOOL_CALLS,
        label: "Group tool calls",
        kind: SettingKind::Bool { default: true },
        owner: SettingOwner::ProductConfig,
        restart_required: false,
        new_sessions: false,
        keywords: &["verb", "compose", "collapse", "tools"],
        description: "Collapse completed activity groups; live actions stay visible.",
    },
    SettingMeta {
        key: AUTO_COMPACTION,
        label: "Auto compaction",
        kind: SettingKind::Bool { default: true },
        owner: SettingOwner::ProductConfig,
        restart_required: false,
        new_sessions: false,
        keywords: &["compact", "context", "trigger"],
        description: "Compact at the route's context trigger; it is never an allowance gate.",
    },
    SettingMeta {
        key: REDUCED_MOTION,
        label: "Reduced motion",
        kind: SettingKind::Bool { default: false },
        owner: SettingOwner::ProductConfig,
        restart_required: false,
        new_sessions: false,
        keywords: &["accessibility", "spinner", "pulse", "animation", "camera"],
        description: "Suppress the spinner, the pulse and camera easing.",
    },
    SettingMeta {
        key: EDIT_RAW_CONFIG,
        label: "Edit raw config",
        kind: SettingKind::Action,
        owner: SettingOwner::ViewOnly,
        restart_required: false,
        new_sessions: false,
        keywords: &["config.toml", "editor", "raw", "templates", "roster"],
        description: "Open config.toml in $EDITOR, the same effect as /config.",
    },
];

/// Index of `key` in [`ROWS`].
#[must_use]
#[cfg(test)]
pub fn index_of(key: &str) -> Option<usize> {
    ROWS.iter().position(|meta| meta.key == key)
}

/// Metadata for `key`.
#[must_use]
pub fn meta(key: &str) -> Option<&'static SettingMeta> {
    ROWS.iter().find(|row| row.key == key)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests {
    use super::{ROWS, SettingKind, THEME_CHOICES, index_of, meta};
    use crate::tui::theme::ThemeId;

    #[test]
    fn rows_are_unique_and_in_the_specified_order() {
        let labels: Vec<&str> = ROWS.iter().map(|row| row.label).collect();
        assert_eq!(
            labels,
            vec![
                "Theme",
                "Mouse capture",
                "Scroll lines",
                "Show thinking blocks",
                "Group tool calls",
                "Auto compaction",
                "Reduced motion",
                "Edit raw config",
            ]
        );
        for (index, row) in ROWS.iter().enumerate() {
            assert_eq!(index_of(row.key), Some(index), "duplicate key {}", row.key);
        }
    }

    #[test]
    fn no_row_only_navigates_to_another_tab() {
        for row in ROWS {
            let label = row.label.to_ascii_lowercase();
            assert!(
                !label.starts_with("open "),
                "spec 02 §5.5 deletes navigation-only rows: {}",
                row.label
            );
        }
    }

    #[test]
    fn theme_choices_match_the_shipped_theme_set() {
        let shipped: Vec<&str> = ThemeId::ALL.iter().map(|id| id.as_str()).collect();
        let registered: Vec<&str> = THEME_CHOICES.iter().map(|choice| choice.value).collect();
        assert_eq!(registered, shipped);
        assert_eq!(shipped.len(), 6, "spec 02 §5.5 fixes the theme set at six");
        assert!(shipped.contains(&"default-light"));
    }

    /// Identity-bearing bindings are not editable in Settings. Composition
    /// and the `OpenRouter` wire are config-file only; the Codex processing tier
    /// is a Models-tab extra on the exact model that admits it.
    #[test]
    fn no_row_binds_provider_identity() {
        let identity: Vec<&str> = ROWS
            .iter()
            .filter(|row| row.new_sessions)
            .map(|row| row.label)
            .collect();
        assert!(identity.is_empty(), "{identity:?}");
    }

    /// Settings apply on change, so there is no Save row and no
    /// draft to commit.
    #[test]
    fn the_only_action_row_is_the_raw_config_editor() {
        let actions: Vec<&str> = ROWS
            .iter()
            .filter(|row| matches!(row.kind, SettingKind::Action))
            .map(|row| row.label)
            .collect();
        assert_eq!(actions, vec!["Edit raw config"]);
    }

    #[test]
    fn only_the_theme_row_previews_live() {
        let previewing: Vec<&str> = ROWS
            .iter()
            .filter(|row| row.previews())
            .map(|row| row.label)
            .collect();
        assert_eq!(previewing, vec!["Theme"]);
    }

    #[test]
    fn scroll_lines_is_bounded_by_the_spec_range() {
        let SettingKind::Int { min, max, default } = meta(super::SCROLL_LINES).unwrap().kind else {
            panic!("scroll lines must be an Int row");
        };
        assert_eq!((min, max), (1, 10));
        assert!((min..=max).contains(&default));
    }

    /// `tui.reading_width` was deleted: the row previewed,
    /// persisted and wrapped nothing. No row may name it again.
    #[test]
    fn no_row_names_the_deleted_reading_width_key() {
        assert!(ROWS.iter().all(|row| row.key != "tui.reading_width"));
    }

    #[test]
    fn every_row_carries_a_description_and_search_terms() {
        for row in ROWS {
            assert!(
                !row.description.is_empty(),
                "{} has no description",
                row.key
            );
            assert!(
                row.description.ends_with('.'),
                "{} is not a sentence",
                row.key
            );
            assert!(!row.keywords.is_empty(), "{} has no keywords", row.key);
            assert!(
                row.matches(row.label),
                "{} does not match its own label",
                row.key
            );
        }
    }
}
