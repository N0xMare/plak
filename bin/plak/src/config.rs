//! The concrete product vocabulary the TUI reads through its explicit module
//! boundary. Hosted sessions carry these opaque extension payloads without
//! owning settings, themes, or product intents.

pub(crate) use crate::product_config::{DEFAULT_SCROLL_LINES, MAX_SCROLL_LINES, MIN_SCROLL_LINES};
pub(crate) use crate::surface_protocol::{
    AgentAcknowledgement, HostEvent, HostEventReceiver, ProfileOutcome, ProfileRole, ProfileRow,
    ProfilesView, SettingsSnapshot, SettingsTheme, SurfaceIntent, SurfaceObservation, UserAction,
    UserActionSendError, UserActionSender,
};
#[cfg(test)]
pub(crate) use crate::surface_protocol::{host_event_channel, user_action_channel};
/// Reading, sniffing and chips of the image files a draft attaches.
pub(crate) use plakous_hosted::attachments as image_attachments;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use crate::product_config::{MAX_SCROLL_LINES, MIN_SCROLL_LINES, ProductConfig};
    use crate::tui::SettingsMenu;
    use crate::tui::settings_registry::{self, SettingKind};

    /// Spec 02 §5.5: "A test must pin every registry default to the config schema
    /// default." The registry is the TUI's only source of row defaults, so this
    /// is the one place the two schemas are compared.
    #[test]
    fn settings_registry_defaults_match_the_config_schema() {
        let schema = ProductConfig::default_product();
        let snapshot = crate::shell::settings::settings_snapshot(&schema);
        let menu = SettingsMenu::open(snapshot);
        let mut checked = 0usize;
        for row in settings_registry::ROWS {
            match row.kind {
                SettingKind::Bool { default } => {
                    assert_eq!(
                        menu.bool_value(row.key),
                        Some(default),
                        "{} default drifted from the config schema",
                        row.key
                    );
                    checked += 1;
                }
                SettingKind::Enum { default, .. } => {
                    assert_eq!(
                        menu.enum_value(row.key),
                        Some(default),
                        "{} default drifted from the config schema",
                        row.key
                    );
                    checked += 1;
                }
                SettingKind::Int { default, min, max } => {
                    assert_eq!(
                        menu.int_value(row.key),
                        Some(default),
                        "{} default drifted from the config schema",
                        row.key
                    );
                    assert!((min..=max).contains(&default));
                    checked += 1;
                }
                SettingKind::Action => {}
            }
        }
        assert_eq!(
            checked,
            settings_registry::ROWS.len() - 1,
            "only the raw-config action row carries no value"
        );

        // The bounded rows publish the same bounds the schema enforces.
        assert!(
            ProductConfig::default_product()
                .clone()
                .set_key("tui.scroll_lines", &(MIN_SCROLL_LINES - 1).to_string())
                .is_err()
        );
        assert!(
            ProductConfig::default_product()
                .clone()
                .set_key("tui.scroll_lines", &(MAX_SCROLL_LINES + 1).to_string())
                .is_err()
        );
    }
}
