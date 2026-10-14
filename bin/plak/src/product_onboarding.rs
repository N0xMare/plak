//! TUI empty-state setup detection.

use crate::product_config::ProductConfig;

/// True when product defaults are insufficient to attach a hosted OAuth agent.
pub(crate) fn needs_setup(cfg: &ProductConfig) -> bool {
    cfg.default_account().unwrap_or("").trim().is_empty()
        || cfg.default_model().unwrap_or("").trim().is_empty()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn needs_setup_when_missing_defaults() {
        let mut cfg = ProductConfig::default_product();
        assert!(needs_setup(&cfg));
        cfg.root_template_mut().account = "grok".into();
        assert!(needs_setup(&cfg));
        cfg.root_template_mut().model = "m".into();
        assert!(!needs_setup(&cfg));
    }
}
