//! Carry a released session under its current root selection.

use super::{ReboundMode, ensure_carry_quiescence};
use crate::carry::export_session;
use crate::{CliFailure, CliResult};

pub(crate) fn rebind_session(
    services: &crate::shell::services::Services,
    record: &crate::store::sessions::SessionRecord,
    cause: &str,
) -> CliResult<(crate::store::sessions::SessionRecord, String)> {
    ensure_carry_quiescence(services.paths()?, record)?;
    let document = export_session(services.paths()?, record)
        .map_err(|error| CliFailure::new(format!("cannot carry this session: {error}")))?;
    let paths = services.paths()?.for_workspace(&record.workspace_root);
    crate::commands::session::rebind::rebind_document(
        services,
        document,
        cause,
        ReboundMode::Rebind,
        crate::profiles::session_selection(&paths, record),
        None,
    )
}
