//! Terminal-owned preferences, independent of product binding and runtime settings.

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent terminal preferences"
)]
pub struct TuiPreferences {
    pub mouse: bool,
    pub scroll_lines: u16,
    pub show_thinking: bool,
    pub group_tool_calls: bool,
    pub reduced_motion: bool,
    pub theme: SettingsTheme,
}

impl Default for TuiPreferences {
    fn default() -> Self {
        Self {
            mouse: true,
            scroll_lines: 3,
            show_thinking: true,
            group_tool_calls: true,
            reduced_motion: false,
            theme: SettingsTheme::default(),
        }
    }
}

#[cfg(any(test, feature = "tui"))]
mod publication;

/// Closed set of product themes supported by this TUI build (spec 02 §5.5).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SettingsTheme {
    /// Plak's original high-contrast dark product theme.
    #[default]
    DefaultDark,
    /// Plak's high-contrast light product theme.
    DefaultLight,
    /// Catppuccin's light flavor.
    CatppuccinLatte,
    /// Catppuccin's muted dark flavor.
    CatppuccinFrappe,
    /// Catppuccin's medium-contrast dark flavor.
    CatppuccinMacchiato,
    /// Catppuccin's darkest color-rich flavor.
    CatppuccinMocha,
}

impl SettingsTheme {
    /// Every theme in settings order.
    pub const ALL: [Self; 6] = [
        Self::DefaultDark,
        Self::DefaultLight,
        Self::CatppuccinLatte,
        Self::CatppuccinFrappe,
        Self::CatppuccinMacchiato,
        Self::CatppuccinMocha,
    ];

    /// Stable configuration label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultDark => "default-dark",
            Self::DefaultLight => "default-light",
            Self::CatppuccinLatte => "catppuccin-latte",
            Self::CatppuccinFrappe => "catppuccin-frappe",
            Self::CatppuccinMacchiato => "catppuccin-macchiato",
            Self::CatppuccinMocha => "catppuccin-mocha",
        }
    }

    /// Next theme in the control-center cycle.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::DefaultDark => Self::DefaultLight,
            Self::DefaultLight => Self::CatppuccinLatte,
            Self::CatppuccinLatte => Self::CatppuccinFrappe,
            Self::CatppuccinFrappe => Self::CatppuccinMacchiato,
            Self::CatppuccinMacchiato => Self::CatppuccinMocha,
            Self::CatppuccinMocha => Self::DefaultDark,
        }
    }

    /// Parses a canonical label. `dark` and `light` remain accepted aliases.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::DefaultDark),
            "light" => Some(Self::DefaultLight),
            other => Self::ALL
                .into_iter()
                .find(|candidate| candidate.as_str() == other),
        }
    }
}
