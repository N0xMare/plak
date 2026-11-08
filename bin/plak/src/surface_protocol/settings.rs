//! Local appearance wrapper around the product wire settings.

use super::SettingsSnapshot;

/// Product settings and the independently owned terminal preferences edited together.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SettingsView {
    pub product: SettingsSnapshot,
    /// Client-local on wire v2: it never travels, and a received view
    /// holds the defaults.
    #[serde(skip)]
    pub tui: crate::tui_preferences::TuiPreferences,
}

impl Default for SettingsView {
    /// The config-schema defaults, so a test fixture cannot drift from §5.5.
    fn default() -> Self {
        let mut snapshot = crate::shell::settings::settings_view(
            &crate::product_config::ProductConfig::default_product(),
        );
        // A default snapshot has no host-resolved composition availability.
        snapshot.product.composition_options = Box::default();
        snapshot
    }
}

pub type SettingsUpdate = SettingsView;

impl From<SettingsView> for plakd_wire_types::SettingsView {
    fn from(value: SettingsView) -> Self {
        Self {
            product: value.product,
        }
    }
}

impl From<plakd_wire_types::SettingsView> for SettingsView {
    fn from(value: plakd_wire_types::SettingsView) -> Self {
        Self {
            product: value.product,
            tui: crate::tui_preferences::TuiPreferences::default(),
        }
    }
}
