use super::*;
use plakous_hosted::orchestration::RosterBinding;
use plakous_hosted::topology_runtime::test_support::bootstrap;

#[test]
fn build_rendering_and_hint_changes_keep_exact_resume_across_two_hops() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::for_workspace(root.path().join("home"), root.path());
    paths.ensure_layout().unwrap();
    let config = crate::product_config::ProductConfig::default_product();
    config.save(&paths).unwrap();
    let mut record = SessionRecord::new(root.path().to_path_buf());
    let id = record.id.clone();
    let mut source =
        crate::roster_input::roster_input_from_config(&config, "grok", "grok-4.20", None);
    source.bind_hints = plakous_hosted::orchestration::BindHints::default();
    source.templates_digest = "old-build-render-recipe".into();
    let snapshot = OrchestrationSnapshot::freeze(source);
    let mut tree = bootstrap(&id, root.path().to_path_buf());
    tree.install_snapshot(snapshot.clone());
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let services = crate::shell::services::Services::for_home(paths);
    for recipe in ["next-build-render-recipe", "third-build-render-recipe"] {
        let mut next =
            crate::roster_input::roster_input_from_config(&config, "grok", "grok-4.20", None);
        next.templates_digest = recipe.into();
        next.bind_hints = plakous_hosted::orchestration::BindHints::new(
            std::collections::BTreeSet::from([recipe.to_owned()]),
            std::collections::BTreeSet::new(),
        );
        let (_, TreeInspection::Tree(current)) =
            crate::topology_product::inspect_session_tree(services.paths().unwrap(), &record)
                .unwrap()
        else {
            panic!("the preceding hop must retain its source tree");
        };
        assert_ne!(
            snapshot.digest,
            OrchestrationSnapshot::freeze(next.clone()).digest
        );
        assert!(!effective_settings_changed(&current.rows().snapshot, &next).unwrap());
        assert!(apply_on_reload(&services, &mut record).is_none());
        assert_eq!(record.id, id);
    }
}

#[test]
fn changed_default_knobs_and_template_selections_apply_without_a_config_edit() {
    let config = crate::product_config::ProductConfig::default_product();
    let mut input =
        crate::roster_input::roster_input_from_config(&config, "grok", "grok-4.20", None);
    let name = plakous_hosted::agent_tree_protocol::TemplateName::parse("review").unwrap();
    input.working_set.insert(name.clone());
    input
        .templates
        .insert(name.clone(), RosterBinding::leaf("grok", "grok-4.20"));
    let frozen = OrchestrationSnapshot::freeze(input.clone());
    let mut new_default = input.clone();
    new_default.bounds.max_children -= 1;
    assert!(effective_settings_changed(&frozen, &new_default).unwrap());
    let mut new_default = input;
    new_default.templates.get_mut(&name).unwrap().effort = Some("high".into());
    assert!(effective_settings_changed(&frozen, &new_default).unwrap());
}

#[test]
fn template_values_carry_but_tool_table_formatting_alone_does_not() {
    let config = crate::product_config::ProductConfig::default_product();
    let mut source =
        crate::roster_input::roster_input_from_config(&config, "grok", "grok-4.20", None);
    let name = plakous_hosted::agent_tree_protocol::TemplateName::parse("review").unwrap();
    source.working_set.insert(name.clone());
    source
        .templates
        .insert(name.clone(), RosterBinding::leaf("grok", "grok-4.20"));
    source.tool_sections.insert(
        "tools.fetch_url".into(),
        "[tools.fetch_url]\nmax_bytes = 64\n".into(),
    );
    let frozen = OrchestrationSnapshot::freeze(source.clone());
    source.tool_sections.insert(
        "tools.fetch_url".into(),
        "[tools.fetch_url]\n# new rendering\nmax_bytes=64\n".into(),
    );
    assert!(!effective_settings_changed(&frozen, &source).unwrap());
    let mut description_edit = source.clone();
    description_edit
        .templates
        .get_mut(&name)
        .unwrap()
        .description = "changed card text".into();
    assert!(effective_settings_changed(&frozen, &description_edit).unwrap());
    let mut name_edit = source.clone();
    name_edit.templates.get_mut(&name).unwrap().template = "renamed-card".into();
    assert!(effective_settings_changed(&frozen, &name_edit).unwrap());
    let mut cost_edit = source.clone();
    cost_edit.templates.get_mut(&name).unwrap().cost_tier = Some(2);
    assert!(effective_settings_changed(&frozen, &cost_edit).unwrap());
    let mut visibility_edit = source.clone();
    visibility_edit.templates.get_mut(&name).unwrap().show_model = true;
    assert!(effective_settings_changed(&frozen, &visibility_edit).unwrap());
    let mut root_edit = source.clone();
    root_edit.root.template = Some("lead".into());
    assert!(effective_settings_changed(&frozen, &root_edit).unwrap());
    source.tool_sections.insert(
        "tools.fetch_url".into(),
        "[tools.fetch_url]\nmax_bytes=128\n".into(),
    );
    assert!(effective_settings_changed(&frozen, &source).unwrap());
}

#[test]
fn bounds_are_compared_after_the_same_clamping_as_genesis() {
    let mut source = RosterInput::unpinned("model");
    source.bounds.max_children = u16::MAX;
    let frozen = OrchestrationSnapshot::freeze(source.clone());
    assert!(!effective_settings_changed(&frozen, &source).unwrap());
}

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
        super::super::rebind::ensure_carry_quiescence(&paths, &record)
            .unwrap_err()
            .message()
            .contains("not quiescent")
    );
    let services = crate::shell::services::Services::for_home(paths);
    assert!(
        crate::carry::rebind::rebind_session(&services, &record, "test rebind")
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
