//! Reload applies changed epoch settings by carrying history into fresh genesis.

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use plakous_recursive_subagents_store::TreeInspection;

pub(super) const ORCHESTRATION_CARRY_CAUSE: &str =
    "orchestration settings changed; carried into a new session to apply them";

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
        let source = document.source.session_id.clone();
        let mut record = SessionRecord::new(root.path().to_path_buf());
        record.id.clone_from(&source);
        let mut tree = bootstrap(&source, root.path().to_path_buf());
        tree.install_snapshot(crate::roster_input::snapshot_from_config(
            &config,
            "claude",
            "claude-opus-5",
            Some("high".into()),
        ));
        crate::store::tree::save_tree(&paths, &mut tree).map_err(|error| error.message)?;
        assert!(!orchestration_changed(&paths, &record)?);
        config.max_children = 10;
        config.save(&paths)?;
        assert!(orchestration_changed(&paths, &record)?);
        let carried = document.into_rebound_import(&config, ORCHESTRATION_CARRY_CAUSE)?;
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
}
