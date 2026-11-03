//! Reload applies changed epoch settings by carrying history into fresh genesis.

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use plakous_hosted::orchestration::{OrchestrationSnapshot, RosterInput};
use plakous_recursive_subagents_store::TreeInspection;

pub(super) const ORCHESTRATION_CARRY_CAUSE: &str =
    "orchestration settings changed; carried into a new session to apply them";

/// Runs only in the installed process after a same-state reload. A cross-state
/// import already creates a fresh genesis, so it must not pass through here.
pub(crate) fn apply_on_reload(
    services: &crate::shell::services::Services,
    record: &mut SessionRecord,
) -> Option<String> {
    let apply = || -> crate::CliResult<Option<(SessionRecord, String)>> {
        let paths = PlakPaths::for_workspace(
            services.paths()?.home().to_path_buf(),
            &record.workspace_root,
        );
        if !orchestration_changed(&paths, record).map_err(crate::CliFailure::new)? {
            return Ok(None);
        }
        super::rebind::ensure_carry_quiescence(&paths, record)?;
        let document = super::export_session(&paths, record)
            .map_err(|error| crate::CliFailure::new(error.to_string()))?;
        crate::commands::session::rebind::rebind_document(
            services,
            document,
            ORCHESTRATION_CARRY_CAUSE,
            super::rebind::ReboundMode::Orchestration,
            crate::profiles::session_selection(&paths, record),
            None,
        )
        .map(Some)
    };
    match apply() {
        Ok(Some((next, note))) => {
            *record = next;
            Some(note)
        }
        Ok(None) => None,
        Err(error) => Some(format!(
            "orchestration settings were not applied: {}; settings apply to new sessions; continuing normal reload",
            error.message()
        )),
    }
}

pub(super) fn orchestration_changed(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<bool, String> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)
        .map_err(|error| error.message().to_owned())?;
    let TreeInspection::Tree(tree) = observed else {
        return Ok(false);
    };
    let frozen = tree.rows().current_epoch();
    // The session's own profile, re-resolved against the template files as
    // they are now: an edit to one of its files applies to this session, a
    // change of the home default never does. A profile that no longer loads
    // carries onto the home default, and the carry says so.
    let home = crate::product_config::ProductConfig::load(paths)?;
    let config = crate::profiles::selection_config(
        &home,
        &crate::profiles::SessionSelection::from_snapshot(frozen),
    )
    .unwrap_or(home);
    let desired = crate::roster_input::roster_input_from_config(
        &config,
        &frozen.root_account,
        &frozen.root_tuple,
        frozen.root_effort.clone(),
    );
    effective_settings_changed(frozen, &desired)
}

fn effective_settings_changed(
    frozen: &OrchestrationSnapshot,
    desired: &RosterInput,
) -> Result<bool, String> {
    let mut desired = OrchestrationSnapshot::freeze(desired.clone());
    let mut frozen = frozen.clone();
    let tool_sections_changed =
        tool_section_values(&frozen.tool_sections)? != tool_section_values(&desired.tool_sections)?;
    // Compare every effective frozen field, including built-in/default values.
    // Only these build-owned digests/hints and TOML formatting are excluded.
    for snapshot in [&mut frozen, &mut desired] {
        snapshot.digest.clear();
        snapshot.templates_digest.clear();
        snapshot.bind_hints = plakous_hosted::orchestration::BindHints::default();
        snapshot.tool_sections.clear();
    }
    Ok(tool_sections_changed || frozen != desired)
}

fn tool_section_values(
    sections: &std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<&str, toml::Value>, String> {
    sections
        .iter()
        .map(|(name, source)| {
            toml::from_str(source)
                .map(|value| (name.as_str(), value))
                .map_err(|error| format!("cannot inspect frozen tool table {name}: {error}"))
        })
        .collect()
}

#[cfg(test)]
mod tests;
