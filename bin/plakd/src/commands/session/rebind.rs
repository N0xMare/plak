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
    profile: Option<crate::profiles::SessionSelection>,
    resume: Option<product_sessions::SessionRecord>,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let estimated_tokens = document.estimated_tokens();
    let messages = document.message_count();
    let home = product_config::ProductConfig::load(services.paths()?).map_err(CliFailure::new)?;
    // An unresolvable source binding falls back to its own profile's root,
    // and to the home default only when that profile no longer loads.
    let config = profile
        .as_ref()
        .and_then(|profile| crate::profiles::selection_config(&home, profile).ok())
        .unwrap_or(home);
    let carried = document.into_rebound_import(cause)?;
    import_prepared_with_fallback(
        services,
        PreparedImport {
            carried,
            estimated_tokens,
            messages,
            profile,
            carry: Some(super::import_export::LineCarry {
                cause: cause.to_owned(),
                orchestration: mode == crate::carry::rebind::ReboundMode::Orchestration,
                resume,
            }),
        },
        (mode != crate::carry::rebind::ReboundMode::Orchestration).then_some(&config),
    )
}
