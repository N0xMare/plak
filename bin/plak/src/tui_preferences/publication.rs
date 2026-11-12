use super::{SettingsTheme, TuiPreferences};

impl TuiPreferences {
    /// Publishes only client-owned changes in the home's `[tui]` section.
    pub(crate) fn save(
        &self,
        paths: &crate::home::paths::PlakPaths,
        before: &Self,
    ) -> Result<(), String> {
        use crate::product_config::{ProductConfig, ProductTheme};
        ProductConfig::update_user(paths, |config| {
            if self.mouse != before.mouse {
                config.tui_mouse = self.mouse;
            }
            if self.scroll_lines != before.scroll_lines {
                config.scroll_lines = self.scroll_lines;
            }
            if self.show_thinking != before.show_thinking {
                config.show_thinking = self.show_thinking;
            }
            if self.group_tool_calls != before.group_tool_calls {
                config.group_tool_calls = self.group_tool_calls;
            }
            if self.reduced_motion != before.reduced_motion {
                config.reduced_motion = self.reduced_motion;
            }
            if self.theme != before.theme {
                config.theme = match self.theme {
                    SettingsTheme::DefaultDark => ProductTheme::DefaultDark,
                    SettingsTheme::DefaultLight => ProductTheme::DefaultLight,
                    SettingsTheme::CatppuccinLatte => ProductTheme::CatppuccinLatte,
                    SettingsTheme::CatppuccinFrappe => ProductTheme::CatppuccinFrappe,
                    SettingsTheme::CatppuccinMacchiato => ProductTheme::CatppuccinMacchiato,
                    SettingsTheme::CatppuccinMocha => ProductTheme::CatppuccinMocha,
                };
            }
            Ok(())
        })
        .map(|_| ())
        .map_err(crate::product_config::ConfigSaveError::into_message)
    }
}
