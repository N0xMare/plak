//! The production factory must clear presentation data without clearing succession authority.
use super::*;

#[test]
fn production_successor_factory_preserves_skills_and_pending_epoch_for_native_and_api() {
    for native in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, opened) = actual_api_topology_host(root.path(), &executor);
        let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
        let mut roster = crate::orchestration::RosterInput::unpinned("gpt-5.6-luna");
        roster.root.account = "openai".into();
        roster.root.effort = Some("low".into());
        roster.transient_resumes = 2;
        roster.bounds.max_children = 1;
        let review = crate::agent_tree_protocol::TemplateName::parse("review").unwrap();
        roster.working_set.insert(review.clone());
        roster.templates.insert(
            review,
            crate::orchestration::RosterBinding::leaf("openai", "gpt-5.6-luna"),
        );
        let epoch = Arc::new(crate::orchestration::OrchestrationSnapshot::freeze(roster));
        config.state_roots.set_pending_epoch(Some(epoch.clone()));
        config.state_roots.set_inherited_skills(Some(true));
        config.state_roots.set_frozen_skills(None);
        let build = |config: &HostConfig<TestExtension>| {
            let spec = native.then(|| {
                crate::agent::native_fixture_spec(
                    &config.session.workspace_root,
                    config.state_roots.clone(),
                    &tree_id,
                )
                .unwrap()
            });
            executor
                .block_on(ProductionRuntimeFactory.rebind_runtime(
                    config,
                    spec.as_ref(),
                    crate::topology_runtime::TopologyHubAttach::Inspect,
                    native,
                    &mut Vec::new(),
                ))
                .unwrap()
                .0
                .unwrap()
                .inspection_facts()
                .unwrap()
        };
        let pending = build(&config);
        assert!(
            pending["canonical_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "Skill")
        );
        config.state_roots.set_pending_epoch(None);
        let without_epoch = build(&config);
        assert_ne!(
            pending["registry_digest"], without_epoch["registry_digest"],
            "native={native}"
        );
        assert_eq!(epoch.max_children, 1);
        executor.block_on(opened.release()).unwrap().unwrap();
    }
}
