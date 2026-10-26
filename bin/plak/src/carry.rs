//! Session carry-forward across builds with different state keys.
//!
//! A build never reads another build's stores as session authority. The one
//! thing that crosses is a portable session reference: `plak session export` writes it
//! from this build's state, `plak session import` creates a new linked session
//! from the full neutral conversation and named state. Version 8 imports with
//! empty named state; version 7 is no longer importable.

mod document;
#[cfg(any(test, feature = "tui"))]
mod epoch;
mod export;
mod history;
mod import;
#[cfg(any(test, feature = "tui"))]
mod interactive;
mod reference;
mod reload;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReboundMode {
    Rebind,
    Orchestration,
}

pub(crate) use document::{
    CarryDocument, IMPORTABLE_VERSIONS, VERSION, read_document, write_document,
};

pub(crate) use export::{export_session, new_carry_file};
#[cfg(feature = "tui")]
pub(crate) use {
    epoch::apply_on_reload,
    interactive::{ReloadPlan, ReloadTarget, carry_foreign_session, carry_out, reload_plan,
    reload_target_for_session},
};
pub(crate) use reload::{CarryCapacityProbe, ReloadProbe, reload_probe};

pub(crate) use import::{CarriedImport, already_carried, carried_forward_label, import_and_resume};

pub(crate) use reference::remove_retained_archive;

pub(crate) fn rebind_session(
    services: &crate::shell::services::Services,
    record: &crate::store::sessions::SessionRecord,
    cause: &str,
) -> crate::CliResult<(crate::store::sessions::SessionRecord, String)> {
    ensure_carry_quiescence(services.paths()?, record)?;
    let document = export_session(services.paths()?, record)
        .map_err(|error| crate::CliFailure::new(format!("cannot carry this session: {error}")))?;
    crate::commands::session::rebind_document(services, document, cause, ReboundMode::Rebind)
}

fn ensure_carry_quiescence(
    paths: &crate::home::paths::PlakPaths,
    record: &crate::store::sessions::SessionRecord,
) -> crate::CliResult<()> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    if let plakous_recursive_subagents_store::TreeInspection::Tree(tree) = observed
        && let Some(reason) = tree.rows().quiescence_refusal()
    {
        return Err(crate::CliFailure::new(format!(
            "cannot carry this session: its persisted tree is not quiescent ({reason:?})"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
