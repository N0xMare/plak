//! Named color themes and semantic presentation tokens for the Plak TUI.

use ratatui::style::{Color, Modifier, Style};

/// Stable identity for every theme shipped by this TUI build.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ThemeId {
    /// Plak's original high-contrast dark palette.
    #[default]
    DefaultDark,
    /// Plak's high-contrast light palette (spec 8.1).
    DefaultLight,
    /// Catppuccin's light flavor.
    CatppuccinLatte,
    /// Catppuccin's softly muted dark flavor.
    CatppuccinFrappe,
    /// Catppuccin's medium dark flavor.
    CatppuccinMacchiato,
    /// Catppuccin's deepest dark flavor.
    CatppuccinMocha,
}

impl ThemeId {
    /// All themes in settings-menu order.
    #[cfg(test)]
    pub const ALL: [Self; 6] = [
        Self::DefaultDark,
        Self::DefaultLight,
        Self::CatppuccinLatte,
        Self::CatppuccinFrappe,
        Self::CatppuccinMacchiato,
        Self::CatppuccinMocha,
    ];

    /// Canonical persisted configuration label.
    #[must_use]
    #[cfg(test)]
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

    /// Human-readable settings label.
    #[must_use]
    #[cfg(test)]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::DefaultDark => "Default Dark",
            Self::DefaultLight => "Default Light",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::CatppuccinFrappe => "Catppuccin Frappe",
            Self::CatppuccinMacchiato => "Catppuccin Macchiato",
            Self::CatppuccinMocha => "Catppuccin Mocha",
        }
    }

    /// Light-surface themes: the session banner uses black ink.
    #[must_use]
    pub const fn is_light(self) -> bool {
        matches!(self, Self::DefaultLight | Self::CatppuccinLatte)
    }

    /// Next theme in settings-menu order, wrapping at the end.
    #[must_use]
    #[cfg(test)]
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

    /// Parses canonical labels plus the two legacy labels.
    ///
    /// Spec 8.1: `dark` and `light` remain accepted aliases of `default-dark`
    /// and `default-light`. Callers serialize with [`Self::as_str`] so a
    /// subsequent save publishes the canonical identity.
    #[must_use]
    #[cfg(test)]
    pub fn parse_compatible(value: &str) -> Option<Self> {
        match value {
            "default-dark" | "dark" => Some(Self::DefaultDark),
            "default-light" | "light" => Some(Self::DefaultLight),
            "catppuccin-latte" => Some(Self::CatppuccinLatte),
            "catppuccin-frappe" => Some(Self::CatppuccinFrappe),
            "catppuccin-macchiato" => Some(Self::CatppuccinMacchiato),
            "catppuccin-mocha" => Some(Self::CatppuccinMocha),
            _ => None,
        }
    }

    /// Semantic palette used to construct all concrete terminal styles.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "one exhaustive palette table keeps all six theme identities auditable"
    )]
    pub const fn colors(self) -> SemanticColors {
        match self {
            Self::DefaultDark => SemanticColors {
                background: Rgb::new(20, 22, 27),
                scrollbar_track: Rgb::new(30, 32, 37),
                scrollbar_thumb: Rgb::new(128, 128, 128),
                surface: Rgb::new(20, 22, 27),
                panel_surface: Rgb::new(27, 29, 35),
                user_surface: Rgb::new(48, 49, 53),
                raised_surface: Rgb::new(59, 60, 65),
                text: Rgb::new(240, 242, 248),
                strong_text: Rgb::new(248, 248, 246),
                sticky_text: Rgb::new(252, 252, 250),
                chrome_text: Rgb::new(190, 194, 204),
                muted_text: Rgb::new(150, 156, 168),
                faint_text: Rgb::new(150, 156, 168),
                tool_text: Rgb::new(240, 242, 248),
                system_text: Rgb::new(170, 176, 190),
                menu_text: Rgb::new(230, 232, 238),
                accent: Rgb::new(112, 180, 255),
                on_accent: Rgb::new(20, 24, 30),
                success: Rgb::new(104, 211, 145),
                warning: Rgb::new(238, 199, 110),
                error: Rgb::new(255, 112, 112),
                selection: Rgb::new(48, 91, 156),
                on_selection: Rgb::new(255, 255, 255),
                selection_muted: Rgb::new(200, 220, 245),
                border: Rgb::new(90, 98, 112),
            },
            // Contrast-checked against `background`: every text role clears the
            // WCAG AA 4.5:1 bar and `border` clears the 3:1 non-text bar.
            Self::DefaultLight => SemanticColors {
                background: Rgb::new(252, 252, 253),
                scrollbar_track: Rgb::new(240, 240, 241),
                scrollbar_thumb: Rgb::new(128, 128, 128),
                surface: Rgb::new(244, 245, 247),
                panel_surface: Rgb::new(244, 245, 247),
                user_surface: Rgb::new(232, 234, 238),
                raised_surface: Rgb::new(219, 223, 229),
                text: Rgb::new(24, 26, 31),
                strong_text: Rgb::new(10, 11, 14),
                sticky_text: Rgb::new(10, 11, 14),
                chrome_text: Rgb::new(58, 62, 70),
                muted_text: Rgb::new(88, 94, 104),
                faint_text: Rgb::new(102, 108, 120),
                tool_text: Rgb::new(24, 26, 31),
                system_text: Rgb::new(72, 78, 88),
                menu_text: Rgb::new(24, 26, 31),
                accent: Rgb::new(0, 86, 179),
                on_accent: Rgb::new(255, 255, 255),
                success: Rgb::new(13, 110, 64),
                warning: Rgb::new(140, 94, 0),
                error: Rgb::new(176, 0, 32),
                selection: Rgb::new(0, 86, 179),
                on_selection: Rgb::new(255, 255, 255),
                selection_muted: Rgb::new(219, 232, 247),
                border: Rgb::new(138, 145, 156),
            },
            Self::CatppuccinLatte => SemanticColors {
                background: Rgb::new(239, 241, 245), // base
                scrollbar_track: Rgb::new(227, 229, 233),
                scrollbar_thumb: Rgb::new(128, 128, 128),
                surface: Rgb::new(230, 233, 239), // mantle
                panel_surface: Rgb::new(230, 233, 239),
                user_surface: Rgb::new(220, 224, 232), // crust
                raised_surface: Rgb::new(204, 208, 218), // surface0
                text: Rgb::new(76, 79, 105),
                strong_text: Rgb::new(40, 43, 62),
                sticky_text: Rgb::new(76, 79, 105),
                chrome_text: Rgb::new(76, 79, 105),
                muted_text: Rgb::new(108, 111, 133), // subtext0
                faint_text: Rgb::new(108, 111, 133),
                tool_text: Rgb::new(76, 79, 105),
                system_text: Rgb::new(76, 79, 105),
                menu_text: Rgb::new(76, 79, 105),
                accent: Rgb::new(30, 102, 245),
                on_accent: Rgb::new(255, 255, 255),
                success: Rgb::new(38, 105, 31),
                warning: Rgb::new(150, 95, 20),
                error: Rgb::new(210, 15, 57),
                selection: Rgb::new(30, 102, 245),
                on_selection: Rgb::new(255, 255, 255),
                selection_muted: Rgb::new(239, 241, 245),
                border: Rgb::new(140, 143, 161), // overlay1
            },
            Self::CatppuccinFrappe => SemanticColors {
                background: Rgb::new(48, 52, 70),
                scrollbar_track: Rgb::new(58, 62, 80),
                scrollbar_thumb: Rgb::new(128, 128, 128),
                surface: Rgb::new(41, 44, 60),
                panel_surface: Rgb::new(41, 44, 60),
                user_surface: Rgb::new(65, 69, 89),
                raised_surface: Rgb::new(81, 87, 109),
                text: Rgb::new(198, 208, 245),
                strong_text: Rgb::new(198, 208, 245),
                sticky_text: Rgb::new(198, 208, 245),
                chrome_text: Rgb::new(165, 173, 206),
                muted_text: Rgb::new(165, 173, 206),
                faint_text: Rgb::new(165, 173, 206),
                tool_text: Rgb::new(198, 208, 245),
                system_text: Rgb::new(165, 173, 206),
                menu_text: Rgb::new(198, 208, 245),
                accent: Rgb::new(140, 170, 238),
                on_accent: Rgb::new(48, 52, 70),
                success: Rgb::new(166, 209, 137),
                warning: Rgb::new(229, 200, 144),
                error: Rgb::new(231, 130, 132),
                selection: Rgb::new(98, 104, 128),
                on_selection: Rgb::new(245, 245, 255),
                selection_muted: Rgb::new(198, 208, 245),
                border: Rgb::new(131, 139, 167),
            },
            Self::CatppuccinMacchiato => SemanticColors {
                background: Rgb::new(36, 39, 58),
                scrollbar_track: Rgb::new(46, 48, 68),
                scrollbar_thumb: Rgb::new(128, 128, 128),
                surface: Rgb::new(30, 32, 48),
                panel_surface: Rgb::new(30, 32, 48),
                user_surface: Rgb::new(54, 58, 79),
                raised_surface: Rgb::new(73, 77, 100),
                text: Rgb::new(202, 211, 245),
                strong_text: Rgb::new(202, 211, 245),
                sticky_text: Rgb::new(202, 211, 245),
                chrome_text: Rgb::new(165, 173, 203),
                muted_text: Rgb::new(165, 173, 203),
                faint_text: Rgb::new(128, 135, 162),
                tool_text: Rgb::new(202, 211, 245),
                system_text: Rgb::new(165, 173, 203),
                menu_text: Rgb::new(202, 211, 245),
                accent: Rgb::new(138, 173, 244),
                on_accent: Rgb::new(36, 39, 58),
                success: Rgb::new(166, 218, 149),
                warning: Rgb::new(238, 212, 159),
                error: Rgb::new(237, 135, 150),
                selection: Rgb::new(91, 96, 120),
                on_selection: Rgb::new(245, 245, 255),
                selection_muted: Rgb::new(202, 211, 245),
                border: Rgb::new(128, 135, 162),
            },
            Self::CatppuccinMocha => SemanticColors {
                background: Rgb::new(30, 30, 46),
                scrollbar_track: Rgb::new(40, 40, 56),
                scrollbar_thumb: Rgb::new(128, 128, 128),
                surface: Rgb::new(24, 24, 37),
                panel_surface: Rgb::new(24, 24, 37),
                user_surface: Rgb::new(49, 50, 68),
                raised_surface: Rgb::new(69, 71, 90),
                text: Rgb::new(205, 214, 244),
                strong_text: Rgb::new(205, 214, 244),
                sticky_text: Rgb::new(205, 214, 244),
                chrome_text: Rgb::new(166, 173, 200),
                muted_text: Rgb::new(166, 173, 200),
                faint_text: Rgb::new(127, 132, 156),
                tool_text: Rgb::new(205, 214, 244),
                system_text: Rgb::new(166, 173, 200),
                menu_text: Rgb::new(205, 214, 244),
                accent: Rgb::new(137, 180, 250),
                on_accent: Rgb::new(30, 30, 46),
                success: Rgb::new(166, 227, 161),
                warning: Rgb::new(249, 226, 175),
                error: Rgb::new(243, 139, 168),
                selection: Rgb::new(88, 91, 112),
                on_selection: Rgb::new(245, 245, 255),
                selection_muted: Rgb::new(205, 214, 244),
                border: Rgb::new(127, 132, 156),
            },
        }
    }
}

/// An exact terminal RGB value independent of the optional Ratatui feature.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rgb {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Rgb {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub(crate) const fn terminal(self) -> Color {
        Color::Rgb(self.red, self.green, self.blue)
    }

    /// Scales each channel toward black. Used for the Usage bar's empty track,
    /// which should read darker than muted chrome without becoming a new
    /// semantic text role.
    fn scale(self, percent: u8) -> Self {
        Self::new(
            scale_channel(self.red, percent),
            scale_channel(self.green, percent),
            scale_channel(self.blue, percent),
        )
    }
}

fn scale_channel(channel: u8, percent: u8) -> u8 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "channel * percent / 100 is at most 255"
    )]
    {
        ((u16::from(channel) * u16::from(percent)) / 100) as u8
    }
}

/// Semantic color roles shared by every concrete theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticColors {
    /// Main transcript background.
    pub background: Rgb,
    /// Constant scrollbar track, one shade away from the background.
    pub scrollbar_track: Rgb,
    /// Neutral scrollbar thumb.
    pub scrollbar_thumb: Rgb,
    /// Menus and secondary panels.
    pub surface: Rgb,
    /// Expanded tool bodies (Grok Build `bg_dark` panel).
    pub panel_surface: Rgb,
    /// User prompt band.
    pub user_surface: Rgb,
    /// Sticky prompt and raised content.
    pub raised_surface: Rgb,
    /// Primary body text.
    pub text: Rgb,
    /// High-emphasis text.
    pub strong_text: Rgb,
    /// Sticky prompt text.
    pub sticky_text: Rgb,
    /// Primary chrome text.
    pub chrome_text: Rgb,
    /// Secondary text.
    pub muted_text: Rgb,
    /// Low-emphasis chrome.
    pub faint_text: Rgb,
    /// Tool activity body text.
    pub tool_text: Rgb,
    /// System-note text.
    pub system_text: Rgb,
    /// Normal menu command and title text.
    pub menu_text: Rgb,
    /// Interactive and product accent.
    pub accent: Rgb,
    /// Foreground text placed on the accent color.
    pub on_accent: Rgb,
    /// Successful state.
    pub success: Rgb,
    /// Caution state: a value that is neither healthy nor exhausted.
    pub warning: Rgb,
    /// Error state.
    pub error: Rgb,
    /// Selected-row background.
    pub selection: Rgb,
    /// Primary text over selection.
    pub on_selection: Rgb,
    /// Secondary text over selection.
    pub selection_muted: Rgb,
    /// Panel and chrome border.
    pub border: Rgb,
}

/// Visual theme tokens for chrome + scrollback + floating menus.
#[allow(
    missing_docs,
    reason = "concrete fields retain the established internal style-token API"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Stable identity of this palette.
    pub id: ThemeId,
    /// Semantic source colors used to produce the concrete styles.
    pub colors: SemanticColors,
    pub app_bg: Color,
    pub title: Style,
    pub chrome: Style,
    pub chrome_dim: Style,
    pub plan_badge: Style,
    pub busy: Style,
    pub user: Style,
    pub user_bg: Color,
    pub user_label: Style,
    pub sticky_user: Style,
    pub sticky_user_bg: Color,
    pub assistant: Style,
    pub assistant_label: Style,
    pub markdown_link: Style,
    pub tool: Style,
    pub tool_label: Style,
    /// Compose header label: bright, high-contrast, bold (§5.8).
    pub compose_label: Style,
    /// Background painted under expanded tool and program bodies.
    pub panel_bg: Color,
    /// The `$` before a command item (§5.4 command colouring).
    pub command_prompt: Style,
    /// A command's program name.
    pub command_program: Style,
    /// A command's arguments.
    pub command_arg: Style,
    /// Quoted strings in a command.
    pub command_string: Style,
    /// Tool output inside an open item's detail.
    pub detail_output: Style,
    /// Added lines of a diff.
    pub diff_added: Style,
    /// Removed lines of a diff.
    pub diff_removed: Style,
    /// Diff file and hunk headers.
    pub diff_hunk: Style,
    pub success: Style,
    pub warning: Style,
    pub system: Style,
    pub error: Style,
    pub error_label: Style,
    /// A queued prompt's preview, on the transcript background.
    pub queue_item: Style,
    /// Queue chips and help, on the transcript background.
    pub queue_help: Style,
    pub composer_prompt: Style,
    pub transcript_selection: Style,
    pub slash_selected: Style,
    pub slash_selected_help: Style,
    pub slash_item: Style,
    pub slash_item_help: Style,
    /// Unused cells of a Usage limit bar: a darker stippled wash than muted
    /// chrome, so the track recedes behind the fill.
    pub bar_track: Style,
    /// Translucent row highlight for menus (Grok Build-style hover).
    pub row_hover: Style,
    /// The focused transcript row: a background step quieter than hover.
    pub row_focus: Style,
    pub menu_bg: Color,
    pub menu_border: Style,
    pub menu_title: Style,
    pub footer: Style,
    pub border: Style,
}

impl Theme {
    /// Constructs the exact styles for a stable theme identity.
    #[must_use]
    pub fn for_id(id: ThemeId) -> Self {
        let colors = id.colors();
        let background = colors.background.terminal();
        let surface = colors.surface.terminal();
        let text = colors.text.terminal();
        let strong = colors.strong_text.terminal();
        let sticky = colors.sticky_text.terminal();
        let chrome = colors.chrome_text.terminal();
        let muted = colors.muted_text.terminal();
        let faint = colors.faint_text.terminal();
        let tool = colors.tool_text.terminal();
        let system = colors.system_text.terminal();
        let menu_text = colors.menu_text.terminal();
        let accent = colors.accent.terminal();
        let success = colors.success.terminal();
        let warning = colors.warning.terminal();
        let error = colors.error.terminal();
        let selection = colors.selection.terminal();
        let on_selection = colors.on_selection.terminal();
        let selection_muted = colors.selection_muted.terminal();
        let border = colors.border.terminal();
        Self {
            id,
            colors,
            app_bg: background,
            title: Style::default().fg(accent).add_modifier(Modifier::BOLD),
            chrome: Style::default().fg(chrome),
            chrome_dim: Style::default().fg(faint),
            plan_badge: Style::default().fg(accent).add_modifier(Modifier::BOLD),
            busy: Style::default().fg(accent).add_modifier(Modifier::BOLD),
            user: Style::default().fg(strong),
            user_bg: colors.user_surface.terminal(),
            user_label: Style::default().fg(strong).add_modifier(Modifier::BOLD),
            sticky_user: Style::default().fg(sticky),
            sticky_user_bg: colors.raised_surface.terminal(),
            assistant: Style::default().fg(text),
            assistant_label: Style::default().fg(warning).add_modifier(Modifier::BOLD),
            markdown_link: Style::default()
                .fg(
                    if matches!(id, ThemeId::DefaultLight | ThemeId::CatppuccinLatte) {
                        accent
                    } else {
                        warning
                    },
                )
                .add_modifier(Modifier::UNDERLINED),
            tool: Style::default().fg(tool),
            tool_label: Style::default().fg(muted),
            compose_label: Style::default().fg(strong).add_modifier(Modifier::BOLD),
            panel_bg: colors.panel_surface.terminal(),
            command_prompt: Style::default().fg(muted),
            command_program: Style::default().fg(accent),
            command_arg: Style::default().fg(tool),
            command_string: Style::default().fg(warning),
            detail_output: Style::default().fg(muted),
            diff_added: Style::default().fg(success),
            diff_removed: Style::default().fg(error),
            diff_hunk: Style::default().fg(accent),
            success: Style::default().fg(success).add_modifier(Modifier::BOLD),
            warning: Style::default().fg(warning),
            system: Style::default().fg(system),
            error: Style::default().fg(error),
            error_label: Style::default().fg(error).add_modifier(Modifier::BOLD),
            queue_item: Style::default().fg(text).bg(background),
            queue_help: Style::default().fg(muted).bg(background),
            composer_prompt: Style::default().fg(strong).add_modifier(Modifier::BOLD),
            transcript_selection: Style::default().fg(on_selection).bg(selection),
            slash_selected: Style::default()
                .fg(on_selection)
                .bg(selection)
                .add_modifier(Modifier::BOLD),
            slash_selected_help: Style::default().fg(selection_muted).bg(selection),
            slash_item: Style::default().fg(menu_text).bg(surface),
            slash_item_help: Style::default().fg(muted).bg(surface),
            bar_track: Style::default()
                .fg(colors.muted_text.scale(75).terminal())
                .bg(surface),
            row_hover: Style::default()
                .fg(strong)
                .bg(colors.raised_surface.terminal()),
            row_focus: Style::default().bg(colors.panel_surface.terminal()),
            menu_bg: surface,
            menu_border: Style::default().fg(border),
            menu_title: Style::default().fg(menu_text).add_modifier(Modifier::BOLD),
            footer: Style::default().fg(faint),
            border: Style::default().fg(border),
        }
    }

    /// Backward-compatible constructor for the default dark theme.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn dark() -> Self {
        Self::for_id(ThemeId::DefaultDark)
    }

    /// Constructs the default light theme.
    #[must_use]
    #[cfg(test)]
    pub fn light() -> Self {
        Self::for_id(ThemeId::DefaultLight)
    }

    /// Constructs the Catppuccin Latte theme.
    #[must_use]
    #[cfg(test)]
    pub fn catppuccin_latte() -> Self {
        Self::for_id(ThemeId::CatppuccinLatte)
    }

    /// Constructs the Catppuccin Frappe theme.
    #[must_use]
    #[cfg(test)]
    pub fn catppuccin_frappe() -> Self {
        Self::for_id(ThemeId::CatppuccinFrappe)
    }

    /// Constructs the Catppuccin Macchiato theme.
    #[must_use]
    #[cfg(test)]
    pub fn catppuccin_macchiato() -> Self {
        Self::for_id(ThemeId::CatppuccinMacchiato)
    }

    /// Constructs the Catppuccin Mocha theme.
    #[must_use]
    #[cfg(test)]
    pub fn catppuccin_mocha() -> Self {
        Self::for_id(ThemeId::CatppuccinMocha)
    }

    /// Syntax roles for Compose programs and markdown fences, mapped onto
    /// this theme's existing tokens rather than a second palette.
    #[must_use]
    pub fn syntax_styles_for_code(&self, default: Style) -> crate::tui::highlight::HighlightStyles {
        crate::tui::highlight::HighlightStyles {
            default,
            comment: self.chrome_dim.add_modifier(Modifier::ITALIC),
            string: self.warning,
            constant: self.warning,
            keyword: self.tool_label.add_modifier(Modifier::BOLD),
            function: self.busy,
            type_name: self.warning,
            parameter: default.add_modifier(Modifier::ITALIC),
            invalid: self.error.add_modifier(Modifier::UNDERLINED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Rgb, Theme, ThemeId};

    fn contrast_ratio(foreground: Rgb, background: Rgb) -> f64 {
        fn luminance(color: Rgb) -> f64 {
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.040_45 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(color.red)
                + 0.7152 * channel(color.green)
                + 0.0722 * channel(color.blue)
        }

        let foreground = luminance(foreground);
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn identities_and_compatibility_labels_are_stable() {
        let expected = [
            (ThemeId::DefaultDark, "default-dark"),
            (ThemeId::DefaultLight, "default-light"),
            (ThemeId::CatppuccinLatte, "catppuccin-latte"),
            (ThemeId::CatppuccinFrappe, "catppuccin-frappe"),
            (ThemeId::CatppuccinMacchiato, "catppuccin-macchiato"),
            (ThemeId::CatppuccinMocha, "catppuccin-mocha"),
        ];
        assert_eq!(ThemeId::ALL, expected.map(|(id, _)| id));
        for (id, label) in expected {
            assert_eq!(id.as_str(), label);
            assert_eq!(ThemeId::parse_compatible(label), Some(id));
        }
        assert_eq!(
            ThemeId::parse_compatible("dark"),
            Some(ThemeId::DefaultDark)
        );
        assert_eq!(
            ThemeId::parse_compatible("light"),
            Some(ThemeId::DefaultLight)
        );
        assert_eq!(ThemeId::parse_compatible("sepia"), None);
        assert_eq!(ThemeId::DefaultDark.display_name(), "Default Dark");
        assert_eq!(ThemeId::DefaultLight.display_name(), "Default Light");
        assert_eq!(ThemeId::CatppuccinLatte.display_name(), "Catppuccin Latte");
        assert_eq!(
            ThemeId::CatppuccinFrappe.display_name(),
            "Catppuccin Frappe"
        );
        assert_eq!(
            ThemeId::CatppuccinMacchiato.display_name(),
            "Catppuccin Macchiato"
        );
        assert_eq!(ThemeId::CatppuccinMocha.display_name(), "Catppuccin Mocha");
        assert!(ThemeId::DefaultLight.is_light());
        assert!(ThemeId::CatppuccinLatte.is_light());
        assert!(!ThemeId::DefaultDark.is_light());
        assert!(!ThemeId::CatppuccinMocha.is_light());
        for (index, id) in ThemeId::ALL.into_iter().enumerate() {
            assert_eq!(id.next(), ThemeId::ALL[(index + 1) % ThemeId::ALL.len()]);
        }
    }

    #[test]
    fn catppuccin_flavors_keep_their_exact_base_colors() {
        assert_eq!(
            ThemeId::CatppuccinLatte.colors().background,
            Rgb::new(239, 241, 245)
        );
        assert_eq!(
            ThemeId::CatppuccinFrappe.colors().background,
            Rgb::new(48, 52, 70)
        );
        assert_eq!(
            ThemeId::CatppuccinMacchiato.colors().background,
            Rgb::new(36, 39, 58)
        );
        assert_eq!(
            ThemeId::CatppuccinMocha.colors().background,
            Rgb::new(30, 30, 46)
        );
    }

    #[test]
    fn constructors_preserve_identity_and_light_is_the_default_light_theme() {
        assert_eq!(Theme::dark().id, ThemeId::DefaultDark);
        assert_eq!(Theme::light().id, ThemeId::DefaultLight);
        assert_eq!(Theme::catppuccin_latte().id, ThemeId::CatppuccinLatte);
        assert_eq!(Theme::catppuccin_frappe().id, ThemeId::CatppuccinFrappe);
        assert_eq!(
            Theme::catppuccin_macchiato().id,
            ThemeId::CatppuccinMacchiato
        );
        assert_eq!(Theme::catppuccin_mocha().id, ThemeId::CatppuccinMocha);
    }

    #[test]
    fn every_theme_defines_distinct_readable_command_tokens() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let colors = id.colors();
            let tokens = [
                (theme.command_prompt, colors.muted_text),
                (theme.command_program, colors.accent),
                (theme.command_arg, colors.tool_text),
                (theme.command_string, colors.warning),
            ];
            for (index, (style, rgb)) in tokens.iter().enumerate() {
                assert_eq!(style.fg, Some(rgb.terminal()), "{id:?} token {index}");
                assert!(
                    contrast_ratio(*rgb, colors.background) >= 3.0,
                    "{id:?} command token {index} is unreadable on the background"
                );
                for (other, (_, other_rgb)) in tokens.iter().enumerate().skip(index + 1) {
                    assert_ne!(rgb, other_rgb, "{id:?} tokens {index} and {other} collide");
                }
            }
        }
    }

    #[test]
    fn queue_tokens_sit_on_the_transcript_background_in_every_theme() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let background = id.colors().background.terminal();
            assert_eq!(theme.queue_item.bg, Some(background), "{id:?}");
            assert_eq!(theme.queue_help.bg, Some(background), "{id:?}");
            assert_eq!(
                theme.queue_item.fg,
                Some(id.colors().text.terminal()),
                "{id:?}"
            );
        }
    }

    #[test]
    fn primary_text_and_selection_pairs_meet_normal_text_contrast() {
        for id in ThemeId::ALL {
            let colors = id.colors();
            for (foreground, background) in [
                (colors.text, colors.background),
                (colors.text, colors.surface),
                (colors.strong_text, colors.user_surface),
                (colors.strong_text, colors.raised_surface),
                (colors.chrome_text, colors.surface),
                (colors.tool_text, colors.surface),
                (colors.system_text, colors.surface),
                (colors.menu_text, colors.surface),
                (colors.on_accent, colors.accent),
                (colors.on_selection, colors.selection),
            ] {
                assert!(
                    contrast_ratio(foreground, background) >= 4.5,
                    "{id:?} has insufficient contrast for {foreground:?} on {background:?}"
                );
            }
            for foreground in [colors.muted_text, colors.faint_text] {
                let minimum = if id == ThemeId::CatppuccinLatte {
                    3.0
                } else {
                    4.5
                };
                assert!(
                    contrast_ratio(foreground, colors.surface) >= minimum,
                    "{id:?} has insufficient secondary contrast for {foreground:?}"
                );
            }
        }
    }

    #[test]
    fn default_dark_preserves_established_concrete_colors() {
        use ratatui::style::Color;

        let theme = Theme::dark();
        assert_eq!(theme.app_bg, Color::Rgb(20, 22, 27));
        assert_eq!(theme.chrome.fg, Some(Color::Rgb(190, 194, 204)));
        assert_eq!(theme.sticky_user.fg, Some(Color::Rgb(252, 252, 250)));
        assert_eq!(theme.tool.fg, Some(Color::Rgb(240, 242, 248)));
        assert_eq!(theme.system.fg, Some(Color::Rgb(170, 176, 190)));
        assert_eq!(theme.slash_item.fg, Some(Color::Rgb(230, 232, 238)));
        assert_eq!(theme.queue_item.fg, Some(Color::Rgb(240, 242, 248)));
    }

    #[test]
    fn every_theme_maps_semantic_tokens_to_concrete_styles() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let colors = id.colors();
            assert_eq!(theme.id, id);
            assert_eq!(theme.app_bg, colors.background.terminal());
            assert_eq!(theme.menu_bg, colors.surface.terminal());
            assert_eq!(theme.assistant.fg, Some(colors.text.terminal()));
            assert_eq!(theme.error.fg, Some(colors.error.terminal()));
            assert_eq!(
                theme.transcript_selection,
                ratatui::style::Style::default()
                    .fg(colors.on_selection.terminal())
                    .bg(colors.selection.terminal())
            );
            assert_eq!(theme.slash_selected.bg, Some(colors.selection.terminal()));
            assert_ne!(theme.user_bg, theme.sticky_user_bg);
        }
    }

    #[test]
    fn prose_emphasis_and_tool_output_use_readable_non_success_tokens() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            assert_ne!(theme.assistant_label.fg, theme.success.fg, "{id:?}");
            assert_eq!(theme.tool.fg, theme.assistant.fg, "{id:?}");
        }
        let latte = ThemeId::CatppuccinLatte.colors();
        assert_ne!(latte.strong_text, latte.text);
        assert_ne!(latte.muted_text, latte.text);
        assert_eq!(latte.muted_text, Rgb::new(108, 111, 133));
        assert_eq!(latte.faint_text, Rgb::new(108, 111, 133));
    }
}
