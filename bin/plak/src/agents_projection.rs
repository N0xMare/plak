//! Host-computed template readiness (spec 02 §5.5).
//!
//! The TUI exposes no roster editor: `[roster]` and `[orchestration]`
//! are config-file only, applied by `plak config set` through
//! `topology_product::apply_roster`, and templates are one file each under
//! `~/.plak/agents/`. What is left here is the one read-only
//! projection the product still renders — each template with the readiness
//! the host computed for it.

use plakous::model::ProcessingTierDisposition;
use plakous_host_protocol::views::TemplateRow;

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;

/// Host-computed template rows for the Session tab.
/// A load failure projects an empty list rather than a stale one.
pub(crate) fn template_rows(
    paths: &PlakPaths,
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
) -> Vec<TemplateRow> {
    let Ok(config) = ProductConfig::load(paths) else {
        return Vec::new();
    };
    template_rows_of(&config, ready_catalog)
}

fn template_rows_of(
    config: &ProductConfig,
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
) -> Vec<TemplateRow> {
    config
        .readiness_rows(ready_catalog)
        .into_iter()
        .map(|row| TemplateRow {
            name: row.name,
            account: row.template.account.clone(),
            model: row.template.model.clone(),
            effort: row.template.effort.clone(),
            composition: row.template.composition_or_default().as_str().to_owned(),
            tier: match row.template.processing_tier() {
                ProcessingTierDisposition::Standard => "standard".to_owned(),
                ProcessingTierDisposition::ProviderNamed(tier) => tier.as_str().to_owned(),
            },
            unready_reason: row.readiness.reason().map(str::to_owned),
        })
        .collect()
}
