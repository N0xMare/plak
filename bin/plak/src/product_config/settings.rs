//! Settings metadata registry (spec 02 §5.5).
//!
//! Settings is generated from this table, never from per-row match arms in the
//! renderer. Each entry is pure metadata — no closures, no function pointers —
//! so the row list, the search index, the footer wording and the schema-default
//! parity test all read the same source. Plak owns the config schema;
//! `settings_registry_defaults_match_the_config_schema` in `crate::config`
//! pins every default here to it. Integer bounds come from that schema.

/// A presentation offer must agree with the product's compiled authority.
pub(crate) fn build_feature_available(feature: &str) -> bool {
    crate::build_features::require_feature(feature).is_ok()
}

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

/// Placeholder for the OAuth account argument, as help text spells it.
pub const OAUTH_ACCOUNT_PLACEHOLDER: &str = "<codex|grok|claude>";

/// Placeholder for the API-key account argument, as help text spells it.
pub const API_KEY_ACCOUNT_PLACEHOLDER: &str = "<openai|xai-api|anthropic|openrouter>";

/// The canonical command that connects one OAuth subscription account.
///
/// `plak auth login` still parses as a legacy alias, but no product surface
/// names it: an operator reading a remedy gets the command the help pages and
/// the docs use.
#[must_use]
pub fn oauth_login_remedy(account: &str) -> String {
    format!("plak login {account}")
}

/// The canonical command that stores one provider API key.
///
/// API-key accounts never take `plak login`: the two auth routes do not cross,
/// and pointing an `openrouter` operator at an OAuth verb sends them to a
/// refusal.
#[must_use]
pub fn api_key_remedy(account: &str) -> String {
    format!("plak key set {account}")
}

/// The remedy for `account`, chosen by which auth route the account belongs to.
///
/// An account the product does not know is treated as an API key: the four
/// OAuth verticals are a closed set, and inventing an OAuth login for an
/// unknown name is the exact defect this dispatch exists to prevent.
#[must_use]
pub fn account_remedy(account: &str) -> String {
    if crate::capabilities::oauth_accounts().any(|known| known == account) {
        oauth_login_remedy(account)
    } else {
        api_key_remedy(account)
    }
}

/// Every concrete remedy command any product surface can print.
///
/// The CLI-facing regression test parses each of these with the real argument
/// parser, so a remedy that names a command `plak` does not have fails the
/// build rather than reaching an operator.
#[must_use]
#[cfg(test)]
pub fn remedy_catalog() -> Vec<String> {
    let mut catalog = Vec::new();
    for account in crate::capabilities::OAUTH_ACCOUNTS {
        catalog.push(oauth_login_remedy(account));
        catalog.push(format!("plak logout {account}"));
    }
    for account in crate::capabilities::API_KEY_ACCOUNTS {
        catalog.push(api_key_remedy(account));
        catalog.push(format!("plak key clear {account}"));
    }
    catalog
}

#[cfg(all(test, feature = "tui"))]
/// Index of `key` in [`ROWS`].
#[must_use]
pub fn index_of(key: &str) -> Option<usize> {
    ROWS.iter().position(|meta| meta.key == key)
}

/// Metadata for `key`.
#[must_use]
pub fn meta(key: &str) -> Option<&'static SettingMeta> {
    ROWS.iter().find(|row| row.key == key)
}
