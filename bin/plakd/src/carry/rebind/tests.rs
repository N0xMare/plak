use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use plakous_hosted::topology_runtime::test_support::bootstrap;
use plakous_recursive_subagents_store::TreeInspection;

#[test]
fn explicit_rebind_freezes_current_bounds_without_changing_the_source()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home")).for_workspace(root.path());
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
    crate::store::tree::save_tree(&paths, &mut tree).map_err(|error| error.message())?;
    config.max_children = 10;
    config.save(&paths)?;
    let carried = document.into_rebound_import("explicit rebind")?;
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
    assert!(carried.note.contains("explicit rebind"));
    assert_eq!(
        carried.record.carried_forward_from().map(|(id, _)| id),
        Some(source.as_str())
    );
    crate::store::tree::save_tree(&paths, &mut next).map_err(|error| error.message())?;
    Ok(())
}

#[test]
fn persisted_children_refuse_carry_even_without_a_root_attachment() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home")).for_workspace(root.path());
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
        super::ensure_carry_quiescence(&paths, &record)
            .unwrap_err()
            .message()
            .contains("not quiescent")
    );
    let services = crate::shell::services::Services::for_home(paths);
    assert!(
        crate::carry::rebind::session::rebind_session(&services, &record, "test rebind")
            .unwrap_err()
            .message()
            .contains("not quiescent")
    );
}

/// Profile choice is per session: a changed home default never moves an
/// existing session. Profile edits are available to a subsequent explicit
/// carry; the existing session keeps its frozen epoch.
#[test]
fn a_session_keeps_its_frozen_profile_until_an_explicit_carry() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home")).for_workspace(root.path());
    paths.ensure_layout().unwrap();
    let template = |name: &str, description: &str| {
        std::fs::write(
            paths.user_agents_dir().join(format!("{name}.toml")),
            format!("account = \"grok\"\nmodel = \"grok-4.20\"\ndescription = \"{description}\"\n"),
        )
        .unwrap();
    };
    for name in ["mine-root", "mine-worker", "other-root", "other-worker"] {
        template(name, name);
    }
    let point = |root_template: &str, worker: &str| {
        let mut config = crate::product_config::ProductConfig::load(&paths).unwrap();
        config.set_key("root.agent", root_template).unwrap();
        config.set_key("roster.working_set", worker).unwrap();
        config.save(&paths).unwrap();
        config
    };
    let config = point("mine-root", "mine-worker");
    let record = SessionRecord::new(root.path().to_path_buf());
    let snapshot = crate::roster_input::snapshot_from_config(&config, "grok", "grok-4.20", None);
    assert_eq!(snapshot.root_template.as_deref(), Some("mine-root"));
    let mut tree = bootstrap(&record.id, root.path().to_path_buf());
    tree.install_snapshot(snapshot.clone());
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    point("other-root", "other-worker");
    let selected = crate::profiles::session_selection(&paths, &record).unwrap();
    assert_eq!(selected.root.as_deref(), Some("mine-root"));
    let home = crate::product_config::ProductConfig::load(&paths).unwrap();
    let own = crate::profiles::selection_config(&home, &selected).unwrap();
    assert_eq!(own.root_agent.as_deref(), Some("mine-root"));
    assert_eq!(own.templates["mine-worker"].description, "mine-worker");

    template("mine-worker", "an edited worker card");
    let home = crate::product_config::ProductConfig::load(&paths).unwrap();
    let own = crate::profiles::selection_config(&home, &selected).unwrap();
    assert_eq!(
        own.templates["mine-worker"].description,
        "an edited worker card"
    );
    let (_, TreeInspection::Tree(frozen)) =
        crate::topology_product::inspect_session_tree(&paths, &record).unwrap()
    else {
        panic!("the source tree remains present");
    };
    assert_eq!(frozen.rows().current_epoch(), &snapshot);
}
