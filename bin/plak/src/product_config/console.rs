//! Read-only home projections from bytes already admitted by the console budget.

use super::{ConfigField, ConfigValueOrigin, ProductConfig, parse_template};
use std::collections::BTreeMap;

impl ProductConfig {
    pub(crate) fn console_switches(
        paths: &crate::home::paths::PlakPaths,
    ) -> Result<(bool, bool), String> {
        // Startup policy must not enumerate or admit unrelated templates.
        let config = Self::load_config_file(&paths.config_file())?;
        Ok((config.web_accounts_read, config.web_config_read))
    }

    pub(crate) fn from_home_snapshot(
        config: Option<&str>,
        templates: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut product = config.map_or_else(|| Ok(Self::default_product()), Self::parse)?;
        for field in product.present.clone() {
            product.origins.insert(field, ConfigValueOrigin::User);
        }
        for (name, text) in templates {
            let template = parse_template(text, name)?;
            product.templates.insert(name.clone(), template);
            product
                .template_origins
                .insert(name.clone(), ConfigValueOrigin::User);
        }
        product.validate()?;
        Ok(product)
    }

    pub(crate) fn console_field(&self, field: ConfigField) -> Option<toml_edit::Value> {
        self.field_value(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_read_switches_are_home_only_false_by_default_and_explicit_on_reload()
    -> Result<(), Box<dyn std::error::Error>> {
        let defaults = ProductConfig::default_product();
        assert!(!defaults.web_accounts_read && !defaults.web_config_read);
        assert!(!defaults.render().contains("[web]"));
        let reference = defaults.render_annotated();
        assert!(reference.contains("# [web]\n"));
        assert!(reference.contains("# accounts_read = false\n"));
        assert!(reference.contains("# config_read = false\n"));
        let config = ProductConfig::parse("[web]\naccounts_read = true\nconfig_read = false\n")?;
        assert!(config.web_accounts_read && !config.web_config_read);
        for name in ["web.accounts_read", "web.config_read"] {
            let key = super::super::config_key(name).ok_or("registered switch")?;
            assert!(key.field.is_home_only());
            assert!(!key.field.is_home_orchestration());
        }
        let home = tempfile::tempdir()?;
        let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
        std::fs::write(
            paths.config_file(),
            "[web]\naccounts_read = true\nconfig_read = true\n",
        )?;
        let captured = ProductConfig::console_switches(&paths)?;
        std::fs::write(
            paths.config_file(),
            "[web]\naccounts_read = false\nconfig_read = false\n",
        )?;
        assert_eq!(captured, (true, true));
        assert_eq!(ProductConfig::console_switches(&paths)?, (false, false));
        Ok(())
    }
}
