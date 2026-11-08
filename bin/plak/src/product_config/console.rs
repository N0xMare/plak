//! Read-only home projections from bytes already admitted by the console budget.

use super::{ConfigField, ConfigValueOrigin, ProductConfig, parse_template};
use std::collections::BTreeMap;

impl ProductConfig {
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
