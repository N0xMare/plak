//! Prepare a local carry with explicit rebind fallback policy.

use super::import_export::{PreparedImport, import_prepared_with_fallback};
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliResult};

pub(crate) fn rebind_document(
    services: &crate::shell::services::Services,
    document: crate::carry::CarryDocument,
    cause: &str,
    mode: crate::carry::rebind::ReboundMode,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let estimated_tokens = document.estimated_tokens();
    let messages = document.message_count();
    let config = product_config::ProductConfig::load(services.paths()?).map_err(CliFailure::new)?;
    let carried = document.into_rebound_import(cause)?;
    import_prepared_with_fallback(
        services,
        PreparedImport {
            carried,
            estimated_tokens,
            messages,
        },
        (mode != crate::carry::rebind::ReboundMode::Orchestration).then_some(&config),
    )
}
