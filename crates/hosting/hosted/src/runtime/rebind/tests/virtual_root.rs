//! A virtual-tree successor retains the root address its record publishes.

use super::*;

struct DisabledTopology(Arc<dyn crate::agent::RosterSource>);

impl crate::agent::RosterSource for DisabledTopology {
    fn roster(
        &self,
        account: &str,
        model: &str,
        effort: Option<&str>,
        profile: Option<&sessions::RootProfile>,
    ) -> CliResult<crate::orchestration::RosterInput> {
        self.0.roster(account, model, effort, profile)
    }

    fn account_name(&self, provider: NativeProvider) -> &'static str {
        self.0.account_name(provider)
    }

    fn topology_admitted(&self) -> CliResult<bool> {
        Ok(false)
    }
}

fn production_transition(materialized: bool, switch: bool) {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut parent) = api_topology_root_parent(
        root.path(),
        None,
        crate::binding::PlatformProvider::OpenAi,
        "gpt-5.6-luna",
        crate::skills::SkillsSetting::default(),
        None,
    );
    config.topology_enabled = materialized;
    config.topology_admission = None;
    parent.topology_enabled = materialized;
    if !materialized {
        let source = Arc::clone(config.state_roots.roster_source());
        config.state_roots = config
            .state_roots
            .with_roster_source(Arc::new(DisabledTopology(source)));
        parent.state_roots = config.state_roots.clone();
    }
    let opened = executor
        .block_on(crate::agent::build_api_root_coding_session(
            parent,
            "production-virtual-root",
            None,
        ))
        .unwrap()
        .unwrap();
    let (mut config, mut opened) = bind_api_topology_root(config, opened);
    let source_root = crate::host_service::wire::session_root(&config);
    assert_tree_shape(&config, materialized);
    assert_recorded_root(&config);
    let mut roster = crate::orchestration::RosterInput::unpinned("gpt-5.6-sol");
    roster.root.account = "openai".to_owned();
    roster.root.effort = Some("medium".to_owned());
    let (events, _notes) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    executor
        .block_on(handle_rebind(
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "openai",
            "gpt-5.6-sol",
            Some("medium"),
            None,
            Some(ProductComposition::Direct),
            None,
            &roster,
            switch.then_some("successor"),
        ))
        .unwrap()
        .unwrap();
    if switch {
        assert_eq!(
            crate::host_service::wire::session_root(&config),
            source_root,
            "a switched root must keep its recorded address"
        );
    }
    assert_recorded_root(&config);
    let opened = reopen_exactly(&executor, &mut config, opened);
    assert_tree_shape(&config, materialized);
    assert_recorded_root(&config);
    executor.block_on(opened.release()).unwrap().unwrap();
}

fn assert_tree_shape(config: &HostConfig<TestExtension>, materialized: bool) {
    let tree = &config.session.node.tree_id;
    assert_eq!(
        config.topology_rows().tree_exists(tree).unwrap()
            || crate::topology_runtime::lookup(tree).is_some(),
        materialized
    );
}

fn assert_recorded_root(config: &HostConfig<TestExtension>) {
    let expected = plakous_host_protocol::wire::NodeAddress {
        tree_id: config.session.node.tree_id.clone(),
        agent_id: config.session.node.agent_id.clone(),
    };
    assert_eq!(crate::host_service::wire::session_root(config), expected);
    assert_eq!(
        crate::host_service::wire::session_root_of(&config.state_roots, &config.session.id),
        Some(expected.clone())
    );
    let (actions, _intents) = plakous_host_protocol::intent_channel::<TestExtension>();
    let service = crate::host_service::HostService::new(actions, expected.clone(), 32);
    service
        .attach(&expected, crate::host_service::wire::Face::Local)
        .unwrap();
}

#[test]
fn production_virtual_switch_keeps_the_recorded_wire_root() {
    production_transition(false, true);
}

#[test]
fn production_real_tree_switch_and_forks_keep_the_recorded_wire_root() {
    for (materialized, switch) in [(true, true), (true, false), (false, false)] {
        production_transition(materialized, switch);
    }
}
