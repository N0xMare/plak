//! Reload applies changed epoch settings by carrying history into fresh genesis.

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
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
        super::ensure_carry_quiescence(&paths, record)?;
        let document = super::export_session(&paths, record)
            .map_err(|error| crate::CliFailure::new(error.to_string()))?;
        crate::commands::session::rebind_document(
            services,
            document,
            ORCHESTRATION_CARRY_CAUSE,
            super::ReboundMode::Orchestration,
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
    let frozen = &tree.rows().snapshot;
    let config = crate::product_config::ProductConfig::load(paths)?;
    let desired = crate::roster_input::snapshot_from_config(
        &config,
        &frozen.root_account,
        &frozen.root_tuple,
        frozen.root_effort.clone(),
    );
    Ok(frozen.digest != desired.digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_hosted::topology_runtime::test_support::bootstrap;

    #[test]
    fn changed_spawn_cap_carries_to_fresh_genesis_but_unchanged_settings_do_not()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = PlakPaths::for_workspace(root.path().join("home"), root.path());
        paths.ensure_layout()?;
        let mut config = crate::product_config::ProductConfig::default_product();
        config.bind_root_tuple("claude", "claude-opus-5", Some("high"));
        config.max_children = 2;
        config.save(&paths)?;
        let mut document = crate::carry::document::sample_document();
        document.workspace_root = root.path().to_path_buf();
        document.binding.account = Some("grok".into());
        document.binding.model = Some("grok-4.20".into());
        document.binding.effort = Some("medium".into());
        let source = document.source.session_id.clone();
        let mut record = SessionRecord::new(root.path().to_path_buf());
        record.id.clone_from(&source);
        let mut tree = bootstrap(&source, root.path().to_path_buf());
        tree.install_snapshot(crate::roster_input::snapshot_from_config(
            &config,
            "grok",
            "grok-4.20",
            Some("medium".into()),
        ));
        crate::store::tree::save_tree(&paths, &mut tree).map_err(|error| error.message)?;
        assert!(!orchestration_changed(&paths, &record)?);
        config.max_children = 10;
        config.save(&paths)?;
        assert!(orchestration_changed(&paths, &record)?);
        let carried = document.into_rebound_import(ORCHESTRATION_CARRY_CAUSE)?;
        assert_eq!(carried.record.account.as_deref(), Some("grok"));
        assert_eq!(carried.record.model.as_deref(), Some("grok-4.20"));
        assert_eq!(carried.record.effort.as_deref(), Some("medium"));
        let mut next = bootstrap(&carried.record.id, root.path().to_path_buf());
        next.install_snapshot(crate::roster_input::snapshot_from_config(
            &config,
            carried.record.account.clone().ok_or("missing account")?,
            carried.record.model.clone().ok_or("missing model")?,
            carried.record.effort.clone(),
        ));
        assert_eq!(next.grants_mut_for_test("root").max_children, 10);
        assert_eq!(tree.grants_mut_for_test("root").max_children, 2);
        assert_ne!(carried.record.id, source);
        assert!(carried.note.contains(ORCHESTRATION_CARRY_CAUSE));
        assert_eq!(
            carried.record.carried_forward_from().map(|(id, _)| id),
            Some(source.as_str())
        );
        crate::store::tree::save_tree(&paths, &mut next).map_err(|error| error.message)?;
        assert!(!orchestration_changed(&paths, &carried.record)?);
        Ok(())
    }

    #[test]
    fn persisted_children_refuse_carry_even_without_a_root_attachment() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::for_workspace(root.path().join("home"), root.path());
        paths.ensure_layout().unwrap();
        let record = SessionRecord::new(root.path().to_path_buf());
        let mut tree = bootstrap(&record.id, root.path().to_path_buf());
        tree.spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert!(
            super::super::ensure_carry_quiescence(&paths, &record)
                .unwrap_err()
                .message()
                .contains("not quiescent")
        );
        let services = crate::shell::services::Services::for_home(paths);
        assert!(
            crate::carry::rebind_session(&services, &record, "test rebind")
                .unwrap_err()
                .message()
                .contains("not quiescent")
        );
    }

    #[test]
    fn epoch_inspection_failures_skip_apply_and_keep_normal_reload() {
        for corrupt_tree in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let paths = PlakPaths::for_workspace(root.path().join("home"), root.path());
            paths.ensure_layout().unwrap();
            let mut record = SessionRecord::new(root.path().to_path_buf());
            let id = record.id.clone();
            if corrupt_tree {
                std::fs::write(paths.subagents_database(), b"not a database").unwrap();
            } else {
                let mut tree = bootstrap(&record.id, root.path().to_path_buf());
                crate::store::tree::save_tree(&paths, &mut tree).unwrap();
                std::fs::write(paths.config_file(), b"invalid = [").unwrap();
            }
            let services = crate::shell::services::Services::for_home(paths);
            let note = apply_on_reload(&services, &mut record).unwrap();
            assert!(note.contains("settings apply to new sessions"), "{note}");
            assert!(note.contains("continuing normal reload"), "{note}");
            assert_eq!(record.id, id);
        }
    }
}
