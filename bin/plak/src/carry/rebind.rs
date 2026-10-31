//! Local history carry for explicit rebind and orchestration apply.

use super::{CarriedImport, CarryDocument, export_session};
use crate::{CliFailure, CliResult};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReboundMode {
    Rebind,
    Orchestration,
}

pub(crate) fn rebind_session(
    services: &crate::shell::services::Services,
    record: &crate::store::sessions::SessionRecord,
    cause: &str,
) -> CliResult<(crate::store::sessions::SessionRecord, String)> {
    ensure_carry_quiescence(services.paths()?, record)?;
    let document = export_session(services.paths()?, record)
        .map_err(|error| CliFailure::new(format!("cannot carry this session: {error}")))?;
    crate::commands::session::rebind::rebind_document(
        services,
        document,
        cause,
        ReboundMode::Rebind,
    )
}

pub(super) fn ensure_carry_quiescence(
    paths: &crate::home::paths::PlakPaths,
    record: &crate::store::sessions::SessionRecord,
) -> CliResult<()> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    if let plakous_recursive_subagents_store::TreeInspection::Tree(tree) = observed
        && let Some(reason) = tree.rows().quiescence_refusal()
    {
        return Err(CliFailure::new(format!(
            "cannot carry this session: its persisted tree is not quiescent ({reason:?})"
        )));
    }
    Ok(())
}

impl CarryDocument {
    pub(crate) fn into_rebound_import(self, cause: &str) -> CliResult<CarriedImport> {
        self.into_noticed_import(cause, true)
    }
}
