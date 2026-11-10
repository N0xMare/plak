#![allow(
    clippy::too_many_lines,
    reason = "Rebind scenarios construct and reopen production API branches to verify complete staging lineage and immutable source authority."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]
use crate::runtime::test_kit::BlockOn as _;

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use super::*;
use crate::runtime::*;

use crate::binding::CompositionSelection as ProductComposition;
use crate::binding::TargetSelection;
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::runtime::startup::api_runtime_for_record;
use crate::runtime::test_support::*;

#[test]
fn live_rebind_discard_removes_only_its_request_without_a_notice() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let opened = executor
        .block_on(startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    let requests = ["plak:rebind:refused", "plak:fork:other"].map(|operation| {
        sessions::projector::WorkflowRequest {
            operation_id: operation.into(),
            source_locator: opened.locator(),
            kind: sessions::SessionWorkflowKind::Fork,
        }
    });
    for request in &requests {
        config
            .store
            .submit_workflow_request(&mut config.session, request)
            .unwrap();
    }
    executor
        .block_on(abandon_prepared_rebind(
            &mut config,
            &requests[0].operation_id,
        ))
        .expect("the executor drives the call")
        .unwrap();
    let intents = config.store.load_intents(&config.session.id).unwrap();
    assert!(
        intents
            .workflow_request(&requests[0].operation_id)
            .is_none()
    );
    assert!(
        intents
            .workflow_request(&requests[1].operation_id)
            .is_some()
    );
    assert_eq!(
        intents.notices,
        [] as [sessions::projector::OperatorNotice; 0]
    );
    assert!(
        config
            .store
            .submit_workflow_request(&mut config.session, &requests[0])
            .is_err(),
        "discarded identity must refuse identical replay"
    );
}

fn instruction_fact(id: &str, content: &str) -> plakous::agent::ResolvedContextSourceFact {
    plakous::agent::ResolvedContextSourceFact::new(id, content, "observed", "test").unwrap()
}

#[test]
fn a_recompiled_branch_names_each_agents_md_it_re_read() {
    let before = [
        instruction_fact("plak.agents.global.v1", "global"),
        instruction_fact(
            "plak.agents.workspace-root.v1",
            "root as the session started",
        ),
        instruction_fact("plak.agents.workspace-0123abcd.v1", "nested"),
    ];
    assert_eq!(reread_instructions_note(&before, &before), None);
    let after = [
        instruction_fact("plak.agents.global.v1", "global"),
        instruction_fact("plak.agents.workspace-root.v1", "root after the edit"),
        instruction_fact("plak.agents.workspace-89abcdef.v1", "new nested"),
    ];
    assert_eq!(
        reread_instructions_note(&before, &after).as_deref(),
        Some(
            "AGENTS.md re-read for this branch: workspace root changed, nested directory \
                 added, nested directory removed"
        )
    );
}

#[test]
fn a_recompiled_branch_names_a_skills_listing_it_re_read() {
    let agents = instruction_fact("plak.agents.workspace-root.v1", "root");
    let skills = |content| instruction_fact(plakous::prompts::skills::SKILLS_SOURCE_ID, content);
    assert_eq!(
        reread_instructions_note(
            std::slice::from_ref(&agents),
            &[agents.clone(), skills("one skill")]
        )
        .as_deref(),
        Some("AGENTS.md and skills re-read for this branch: skills listing added")
    );
    assert_eq!(
        reread_instructions_note(
            &[agents.clone(), skills("one skill")],
            &[agents.clone(), skills("two skills")]
        )
        .as_deref(),
        Some("AGENTS.md and skills re-read for this branch: skills listing changed")
    );
    assert_eq!(
        reread_instructions_note(&[agents.clone(), skills("one skill")], &[agents]).as_deref(),
        Some("AGENTS.md and skills re-read for this branch: skills listing removed")
    );
}

#[test]
fn a_treeless_profile_is_not_already_active_when_topology_is_admitted() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let roster = crate::orchestration::RosterInput::unpinned(&config.model_id);
    assert!(!profile_cards_active(&config, &roster).unwrap());
}

#[test]
fn unreadable_topology_refuses_even_an_injected_rebind_target() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.topology_admission = Some(Arc::new(|| Err(CliFailure::new("named config error"))));
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let result = ApplicationExecutor::new()
        .unwrap()
        .block_on(resolve_rebind_target(
            InterruptWatch::new(&actions, &state),
            &config,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            "target",
            config.session.composition,
            &TargetSelection::Native,
            "target",
        ))
        .expect("the executor drives the call");
    assert!(
        result
            .err()
            .is_some_and(|error| error.message() == "named config error")
    );
}
use crate::sessions::{self, QueueItem as SessionQueueItem, SessionRecord};
use plakous::agent::ReasoningEffort;
use plakous::application::ApplicationExecutor;
use std::cell::RefCell;

fn actual_api_topology_host(
    root: &std::path::Path,
    executor: &ApplicationExecutor,
) -> (HostConfig<TestExtension>, AttachedCodingSession) {
    actual_api_topology_host_with(root, executor, None)
}

/// [`actual_api_topology_host`] whose root was created with `frozen`.
fn actual_api_topology_host_with(
    root: &std::path::Path,
    executor: &ApplicationExecutor,
    frozen: Option<crate::frozen_tools::FrozenRootTools>,
) -> (HostConfig<TestExtension>, AttachedCodingSession) {
    api_topology_host_on(
        root,
        executor,
        frozen,
        crate::binding::PlatformProvider::OpenAi,
        "gpt-5.6-luna",
        crate::skills::SkillsSetting::default(),
    )
}

/// A real API root on `provider`/`model`, created under `skills`.
fn api_topology_host_on(
    root: &std::path::Path,
    executor: &ApplicationExecutor,
    frozen: Option<crate::frozen_tools::FrozenRootTools>,
    provider: crate::binding::PlatformProvider,
    model: &str,
    skills: crate::skills::SkillsSetting,
) -> (HostConfig<TestExtension>, AttachedCodingSession) {
    let (config, parent) = api_topology_root_parent(root, frozen, provider, model, skills, None);
    let opened = executor
        .block_on(crate::agent::build_api_root_coding_session(
            parent,
            "offline-topology-root",
            None,
        ))
        .unwrap()
        .unwrap();
    bind_api_topology_root(config, opened)
}

/// The host and root parent [`api_topology_host_on`] opens, over `trees`
/// when given.
fn api_topology_root_parent(
    root: &std::path::Path,
    frozen: Option<crate::frozen_tools::FrozenRootTools>,
    provider: crate::binding::PlatformProvider,
    model: &str,
    skills: crate::skills::SkillsSetting,
    trees: Option<Arc<dyn crate::agent::TreeStoreFactory>>,
) -> (
    HostConfig<TestExtension>,
    crate::agent::build::ApiTopologyParent,
) {
    let (mut config, _) = fresh_injected_attached_host(root);
    config.session.frozen_tools = frozen;
    config.runtime_factory = Arc::new(ProductionRuntimeFactory);
    if let Some(trees) = trees {
        config.state_roots = config.state_roots.with_tree_store_factory(trees);
    }
    config.state_roots = config
        .state_roots
        .with_test_platform_credentials(Arc::new(TopologyApiCredentials))
        .with_skills(skills);
    config.store = config.state_roots.session_records();
    config.target = HostTargetAuthority::Platform {
        provider,
        subscriptions: config.state_roots.native_subscriptions().unwrap(),
    };
    config.session.account = Some(provider.account().into());
    config.session.model = Some(model.into());
    config.session.effort = Some("low".into());
    config.session.composition = ProductComposition::Direct;
    config.session.target_selection = TargetSelection::Platform { provider };
    config.model_id = model.into();
    config.effort = Some(ReasoningEffort::Low);
    let parent = crate::agent::api_topology_parent(
        config.product_home.clone(),
        config.session.id.clone(),
        &config.session,
        config.cwd.clone(),
        config.state_roots.clone(),
        config.model_id.clone(),
        config.effort,
        crate::binding::ApiTarget::Platform {
            provider,
            account: (provider).account().parse().unwrap(),
        },
        info::plan_state_authority(&*config.store, &config.session).unwrap(),
        false,
        crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        true,
    );
    let mut parent = parent;
    parent.tool_selection = startup::root_tool_selection(&config.session);
    (config, parent)
}

fn bind_api_topology_root(
    mut config: HostConfig<TestExtension>,
    opened: AttachedCodingSession,
) -> (HostConfig<TestExtension>, AttachedCodingSession) {
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    (config, opened)
}

const TREE_HOLDER_ENV: &str = "PLAK_TEST_TREE_CUSTODY_HOLDER";

/// The tree store in a file, so that two processes reach one custody record.
struct SqliteTrees(PathBuf);

impl crate::agent::TreeStoreFactory for SqliteTrees {
    fn open(&self) -> CliResult<Arc<dyn crate::agent_tree::TreeStore>> {
        let store = plakous_recursive_subagents_store::SqliteTreeStore::open(&self.0)
            .map_err(|error| CliFailure::new(error.message()))?;
        Ok(Arc::new(store))
    }
}

/// Two processes, one tree: while another live process on this machine holds
/// the custody of a session's tree, this process's attach of that session is
/// refused with the typed refusal. It exits 7, and its terminal JSONL error
/// line carries the stable code; it brings no hub up and writes no tree.
#[test]
fn a_second_process_attach_is_refused_with_exit_7_and_its_jsonl_code() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let database = root.path().join("trees").join("subagents.sqlite3");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let (config, parent) = api_topology_root_parent(
        root.path(),
        None,
        crate::binding::PlatformProvider::OpenAi,
        "gpt-5.6-luna",
        crate::skills::SkillsSetting::default(),
        Some(Arc::new(SqliteTrees(database.clone()))),
    );
    let tree_id = config.session.id.clone();
    let ready = root.path().join("holder.ready");
    let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::rebind::tests::tree_custody_holder_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            TREE_HOLDER_ENV,
            format!("{}|{tree_id}|{}", database.display(), ready.display()),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the holder never took custody");
        thread::sleep(Duration::from_millis(5));
    }

    let failure = executor
        .block_on(crate::agent::build_api_root_coding_session(
            parent,
            "offline-topology-root",
            None,
        ))
        .unwrap()
        .err()
        .expect("the attach is refused");
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert_eq!(failure.exit_code(), 7, "{}", failure.message());
    assert_eq!(failure.code(), Some("tree_hosted_elsewhere"));
    let line: serde_json::Value =
        serde_json::from_str(&plakous_host_protocol::envelope::coded_diagnostic_line(
            None,
            failure.code(),
            failure.message(),
        ))
        .unwrap();
    assert_eq!(line["kind"], "error");
    assert_eq!(line["code"], "tree_hosted_elsewhere");
    assert!(
        failure.message().starts_with("tree_hosted_elsewhere ("),
        "{}",
        failure.message()
    );
    assert!(crate::topology_runtime::lookup(&tree_id).is_none());
    let store = plakous_recursive_subagents_store::SqliteTreeStore::open(&database).unwrap();
    assert!(
        !crate::agent_tree::TreeStore::exists(&store, &tree_id).unwrap(),
        "nothing was written"
    );
}

/// The holding process of the two-process attach: takes custody of one tree
/// through the host's liveness lock, says so, and waits to be killed.
#[test]
fn tree_custody_holder_process() {
    let Ok(spec) = std::env::var(TREE_HOLDER_ENV) else {
        return;
    };
    let mut parts = spec.split('|');
    let (path, tree_id, ready) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let store =
        plakous_recursive_subagents_store::SqliteTreeStore::open(std::path::Path::new(path))
            .unwrap();
    let hold = crate::agent_tree::acquire_custody(&store, tree_id).unwrap();
    std::fs::write(ready, hold.claim().fence.to_string()).unwrap();
    thread::sleep(Duration::from_secs(120));
    crate::agent_tree::release_custody_hold(&store, hold).unwrap();
}

fn retained_api_parent(tree_id: &str) -> crate::agent::build::ApiTopologyParent {
    let hub = crate::topology_runtime::lookup(tree_id).unwrap();
    let retained = hub.rejoin_spec().unwrap();
    let parent = retained
        .downcast_ref::<crate::agent::SpawnParent>()
        .unwrap();
    match parent {
        crate::agent::SpawnParent::Api(parent) => parent.clone(),
        crate::agent::SpawnParent::Native(_) => panic!("API tree retained a native parent"),
    }
}

fn topology_tool_names(tree_id: &str) -> Vec<String> {
    let parent = crate::agent::SpawnParent::Api(retained_api_parent(tree_id));
    crate::agent::build::topology_extensions_if_enabled(
        &parent,
        "gpt-5.6-luna",
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        true,
        &mut None,
    )
    .block()
    .unwrap()
    .names()
    .map(ToString::to_string)
    .collect()
}

/// Verifying an exact reopen of an API root whose tree is persisted, in a
/// process that does not hold it, compiles over a detached copy: the verdict
/// is `Opens`, no hub is attached, and the stored tree is unchanged.
#[test]
fn verifying_an_api_root_with_a_persisted_tree_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = actual_api_topology_host(root.path(), &executor);
    let tree_id = config.session.id.clone();
    let locator = opened.locator();
    executor.block_on(opened.release()).unwrap().unwrap();
    assert!(crate::topology_runtime::lookup(&tree_id).is_none());
    let store = config.state_roots.tree_store().unwrap();
    let facts = || {
        let tree =
            crate::agent_tree::ProductAgentTree::reopen(store.load(&tree_id).unwrap().unwrap());
        (tree.revision, tree.host_generation, tree.state)
    };
    let before = facts();
    assert_eq!(
        verify_exact_reopen(config, locator).unwrap(),
        ReopenVerdict::Opens
    );
    assert!(crate::topology_runtime::lookup(&tree_id).is_none());
    assert_eq!(facts(), before);
}

#[test]
fn production_api_rebind_projects_complete_staging_lineage() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source_id = config.session.id.clone();
    let hub = crate::topology_runtime::lookup(&source_id).unwrap();
    let operation = "plak:rebind:recover-topology";
    let mut target = SessionRecord::new(config.cwd.clone());
    target.id = sessions::projector::WorkflowRequest {
        operation_id: operation.to_owned(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Fork,
    }
    .successor_product_id();
    target.create_operation_id = format!("plak:create:{}", target.id);
    target.account = Some("openai".into());
    target.model = Some("gpt-5.6-sol".into());
    target.effort = Some("medium".into());
    target.composition = ProductComposition::Direct;
    target.target_selection = config.session.target_selection.clone();
    target.source_product_session_id = Some(source_id.clone());
    target.source_workflow_operation_id = Some(operation.into());
    target.validate_for_store().unwrap();
    let runtime = executor
        .block_on(api_runtime_for_record(
            &config,
            &target,
            false,
            crate::topology_runtime::TopologyHubAttach::LookupOnly,
            None,
            &mut None,
        ))
        .unwrap()
        .unwrap();
    let binding = runtime.target_binding();
    let selection = sessions::RebindSelection {
        provider: "openai".into(),
        account: "openai".into(),
        model: "gpt-5.6-sol".into(),
        effort: Some("medium".into()),
        composition: ProductComposition::Direct,
        target_selection: target.target_selection.clone(),
        protocol: binding.protocol().as_str().into(),
        route: binding.route().as_str().into(),
        requested_model: binding.model().as_str().into(),
        selected_configuration: serde_json::to_string(&binding.selected_provider_configuration())
            .unwrap(),
        effective_configuration: serde_json::to_string(&binding.effective_provider_configuration())
            .unwrap(),
        account_binding: serde_json::to_string(&binding.account_binding()).unwrap(),
        // Staged on the source's tree (LookupOnly), so it stays on it.
        own_tree: None,
    };
    let request = sessions::projector::WorkflowRequest {
        operation_id: operation.into(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Rebind(Box::new(selection)),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();

    let (events, _rx) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let source_locator = opened.locator();
    let target_id = target.id.clone();
    executor
        .block_on(complete_rebind(
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            operation,
            source_locator,
            ResolvedRebindTarget {
                route: RebindRoute::Api(Box::new(target)),
                effort: Some(ReasoningEffort::Medium),
                effort_explicit: true,
                binding,
            },
            runtime,
            Instant::now(),
            None,
        ))
        .expect("the executor drives the call")
        .unwrap();
    assert_eq!(config.session.id, target_id);
    assert_eq!(
        config.session.source_product_session_id.as_deref(),
        Some(source_id.as_str())
    );
    assert_eq!(
        config.session.source_workflow_operation_id.as_deref(),
        Some(operation)
    );
    config.session.validate_for_store().unwrap();
    assert_eq!(retained_api_parent(&source_id).model, "gpt-5.6-sol");
    assert_eq!(
        config.source_releases.settle(&source_id, || {}),
        source_release::Settlement::Released
    );
    let source = config.store.load(&source_id).unwrap();
    let projected = executor
        .block_on(project_session(opened.sessions(), &config.store, &source))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(projected.pending_workflows, [] as [String; 0]);
    assert_eq!(
        projected.head.branch_id(),
        source.plakous_locator.unwrap().branch_id()
    );
    assert!(Arc::ptr_eq(
        &hub,
        &crate::topology_runtime::lookup(&source_id).unwrap()
    ));
    executor.block_on(opened.release()).unwrap().unwrap();
    assert!(crate::topology_runtime::lookup(&source_id).is_none());
}

#[test]
fn reopening_with_an_unadmitted_rebind_withdraws_once_without_branching() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, injection) = fresh_injected_attached_host(root.path());
    let opened = executor
        .block_on(startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    let source_locator = opened.locator();
    config.session.bind_plakous_locator(source_locator).unwrap();
    config.store.save(&mut config.session).unwrap();
    let source_id = config.session.id.clone();
    let binding = &injection.rebind_target;
    let request = sessions::projector::WorkflowRequest {
        operation_id: "plak:rebind:unadmitted".into(),
        source_locator,
        kind: sessions::SessionWorkflowKind::Rebind(Box::new(sessions::RebindSelection {
            provider: "codex".into(),
            account: "codex".into(),
            model: "offline-loopback-v2".into(),
            effort: Some("medium".into()),
            composition: config.session.composition,
            target_selection: config.session.target_selection.clone(),
            protocol: binding.protocol().as_str().into(),
            route: binding.route().as_str().into(),
            requested_model: binding.model().as_str().into(),
            selected_configuration: serde_json::to_string(
                &binding.selected_provider_configuration(),
            )
            .unwrap(),
            effective_configuration: serde_json::to_string(
                &binding.effective_provider_configuration(),
            )
            .unwrap(),
            account_binding: serde_json::to_string(&binding.account_binding()).unwrap(),
            own_tree: Some(sessions::RootProfile::default()),
        })),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    executor.block_on(opened.release()).unwrap().unwrap();
    let store = Arc::clone(&config.store);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    loop {
        if matches!(
            events.recv_timeout(Duration::from_secs(10)).unwrap(),
            HostEvent::AgentAttached(true)
        ) {
            break;
        }
    }
    let stored = store.load(&source_id).unwrap();
    assert_eq!(
        stored.plakous_locator.unwrap().branch_id(),
        source_locator.branch_id()
    );
    let intents = store.load_intents(&source_id).unwrap();
    assert_eq!(
        intents.workflow_requests,
        [] as [sessions::projector::WorkflowRequest; 0]
    );
    let notice = TranscriptRow::info(NoticeScope::Recovery, request.withdrawal_note())
        .with_remedy(request.withdrawal_remedy());
    assert_eq!(
        intents
            .notices
            .iter()
            .filter(|row| row.row == notice)
            .count(),
        1
    );
    assert!(store.load(&request.successor_product_id()).is_err());
    let mut stored = store.load(&source_id).unwrap();
    assert_eq!(
        store.withdraw_unadmitted_workflows(&mut stored).unwrap(),
        [] as [TranscriptRow; 0]
    );

    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
}

/// A plain `/fork` lands on the instruction files as they are now and
/// keeps the source's history: unchanged files fork exactly and say
/// nothing, an `AGENTS.md` written since is re-read and named.
#[test]
fn plain_fork_rereads_changed_agents_md_and_names_it() {
    use crate::runtime::fork::handle_fork;
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let fork = |config: &mut HostConfig<TestExtension>, opened: &mut AttachedCodingSession| {
        let source_id = config.session.id.clone();
        let (events, notes_rx) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        executor
            .block_on(handle_fork(
                InterruptWatch::new(&receiver, &state),
                config,
                opened,
                &mut Vec::new(),
                &events,
                None,
            ))
            .expect("the executor drives the call")
            .unwrap();
        assert_ne!(config.session.id, source_id);
        assert_fork_has_a_tree_of_its_own(config, &source_id);
        notes_rx
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(row) => row.text().map(str::to_owned),
                _ => None,
            })
            .filter(|note| note.contains("AGENTS.md"))
            .collect::<Vec<_>>()
    };
    let instructions = |opened: &AttachedCodingSession| {
        instruction_sources(opened)
            .unwrap()
            .iter()
            .map(|source| source.content().to_owned())
            .collect::<Vec<_>>()
    };
    let before = instructions(&opened);
    assert_eq!(fork(&mut config, &mut opened), Vec::<String>::new());
    assert_eq!(instructions(&opened), before);

    std::fs::write(
        config.session.workspace_root.join("AGENTS.md"),
        "written before the fork",
    )
    .unwrap();
    assert_eq!(
        fork(&mut config, &mut opened),
        ["AGENTS.md re-read for this branch: workspace root added"]
    );
    assert!(
        instructions(&opened)
            .iter()
            .any(|content| content.contains("written before the fork")),
        "{:?}",
        instructions(&opened)
    );
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A fork is a new node in a tree of its own: its record names that node,
/// its tree is keyed by its id, seeded with the source tree's current epoch
/// bytes and naming its source node, line and branch, and the source's tree
/// is left as it was (spec 03 §1.2).
fn assert_fork_has_a_tree_of_its_own(config: &HostConfig<TestExtension>, source_id: &str) {
    use crate::agent_tree::ForkedFrom;
    let fork = config.store.load(&config.session.id).unwrap();
    let source = config.store.load(source_id).unwrap();
    assert_eq!(fork.node, sessions::SessionNode::genesis(&fork.id));
    assert_ne!(fork.node, source.node);
    let rows = config.topology_rows();
    assert_eq!(durable_tree_id(&rows, &fork), fork.id);
    let source_tree_id = durable_tree_id(&rows, &source);
    assert_eq!(source_tree_id, source.node.tree_id);
    let store = config.state_roots.tree_store().unwrap();
    let source_tree =
        crate::agent_tree::ProductAgentTree::reopen(store.load(&source_tree_id).unwrap().unwrap());
    let fork_tree =
        crate::agent_tree::ProductAgentTree::reopen(store.load(&fork.id).unwrap().unwrap());
    assert_eq!(fork_tree.root_agent_id, fork.id);
    assert_eq!(fork_tree.nodes().len(), 1);
    assert!(fork_tree.candidates().is_empty());
    assert_eq!(
        serde_json::to_string(fork_tree.current_epoch()).unwrap(),
        serde_json::to_string(source_tree.current_epoch()).unwrap()
    );
    let source_locator = source.plakous_locator.unwrap();
    let Some(ForkedFrom::Node {
        tree_id,
        agent_id,
        line_id,
        branch_id,
        epoch_no,
    }) = fork_tree.forked_from()
    else {
        panic!("the fork's tree records no source node");
    };
    assert_eq!(
        (tree_id, agent_id),
        (&source_tree_id, &source_tree.root_agent_id)
    );
    assert_eq!(branch_id, &source_locator.branch_id().to_string());
    assert_eq!(*epoch_no, source_tree.epochs().current);
    assert_ne!(line_id.as_str(), "");
}

/// A seeded fork tree is removed on a definite no-commit path, compare-and-
/// swapped on the revision the seed published: a tree something else moved
/// since is left alone.
#[test]
fn a_fork_seed_is_discarded_only_at_its_own_revision() {
    use crate::runtime::fork::{discard_seeded_tree, seed_fork_tree};
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source = executor.block_on(opened.detach()).unwrap().unwrap();
    let rows = config.topology_rows();
    let seeded = executor
        .block_on(seed_fork_tree(&config, &opened, "fork-discard", source))
        .expect("the executor drives the call")
        .unwrap();
    assert!(seeded.is_some());
    assert!(rows.tree_exists("fork-discard").unwrap());
    discard_seeded_tree(&config, "fork-discard", seeded);
    assert!(!rows.tree_exists("fork-discard").unwrap());

    let seeded = executor
        .block_on(seed_fork_tree(&config, &opened, "fork-moved", source))
        .expect("the executor drives the call")
        .unwrap();
    let mut moved = rows.tree("fork-moved").unwrap().unwrap();
    let epoch = moved.current_epoch().clone();
    moved.install_snapshot(epoch);
    rows.save_tree(&mut moved).unwrap();
    discard_seeded_tree(&config, "fork-moved", seeded);
    assert!(rows.tree_exists("fork-moved").unwrap());
}

/// Skills follow `AGENTS.md`: a fork re-reads them and names the listing, an
/// exact reopen keeps the listing the session was compiled with.
#[test]
fn a_fork_rereads_skills_and_an_exact_reopen_keeps_the_frozen_listing() {
    use crate::runtime::fork::handle_fork;
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = api_topology_host_on(
        root.path(),
        &executor,
        None,
        crate::binding::PlatformProvider::OpenAi,
        "gpt-5.6-luna",
        crate::skills::SkillsSetting::new(true, None),
    );
    let skill = config
        .session
        .workspace_root
        .join(".agents/skills/review-pr");
    std::fs::create_dir_all(&skill).unwrap();
    let write_skill = |description: &str| {
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: review-pr\ndescription: {description}\n---\nSteps.\n"),
        )
        .unwrap();
    };
    write_skill("Review a PR the first way.");
    let listing = |opened: &AttachedCodingSession| {
        instruction_sources(opened)
            .unwrap()
            .iter()
            .find(|source| source.source_id() == plakous::prompts::skills::SKILLS_SOURCE_ID)
            .map(|source| source.content().to_owned())
    };
    assert_eq!(listing(&opened), None, "no skill existed at construction");

    let (events, notes_rx) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    executor
        .block_on(handle_fork(
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
        .unwrap();
    let notes = notes_rx
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(row) => row.text().map(str::to_owned),
            _ => None,
        })
        .filter(|note| note.contains("re-read"))
        .collect::<Vec<_>>();
    assert_eq!(
        notes,
        ["AGENTS.md and skills re-read for this branch: skills listing added"]
    );
    let forked = listing(&opened).expect("the fork lists the skill");
    assert!(
        forked.contains("- review-pr: Review a PR the first way."),
        "{forked}"
    );

    write_skill("Review a PR the second way.");
    let locator = executor.block_on(opened.detach()).unwrap().unwrap();
    config.session.bind_plakous_locator(locator).unwrap();
    config.store.save(&mut config.session).unwrap();
    executor.block_on(opened.release()).unwrap().unwrap();
    let reopened = executor
        .block_on(startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(listing(&reopened), Some(forked));
    executor.block_on(reopened.release()).unwrap().unwrap();
}

fn claude_api_host(
    root: &std::path::Path,
    executor: &ApplicationExecutor,
    skills_enabled: bool,
) -> (HostConfig<TestExtension>, AttachedCodingSession) {
    api_topology_host_on(
        root,
        executor,
        None,
        crate::binding::PlatformProvider::Anthropic,
        "claude-sonnet-5",
        crate::skills::SkillsSetting::new(skills_enabled, None),
    )
}

fn froze_skill_tool(executor: &ApplicationExecutor, config: &HostConfig<TestExtension>) -> bool {
    let sessions = config.state_roots.runtime_sessions().unwrap();
    executor
        .block_on(crate::skills::frozen_skills(
            &sessions,
            config.session.plakous_locator.unwrap(),
        ))
        .unwrap()
        .unwrap()
        .on
}

fn rebind_to(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<TestExtension>,
    opened: &mut AttachedCodingSession,
    account: &str,
    model: &str,
) -> CliResult<Rebound> {
    let (events, _notes) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    executor
        .block_on(handle_rebind(
            InterruptWatch::new(&receiver, &state),
            config,
            opened,
            &mut Vec::new(),
            &events,
            account,
            model,
            Some("low"),
            None,
            Some(ProductComposition::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned(model),
            None,
        ))
        .expect("the executor drives the call")
}

fn set_skills(config: &mut HostConfig<TestExtension>, enabled: bool) {
    config.state_roots = config
        .state_roots
        .clone()
        .with_skills(crate::skills::SkillsSetting::new(enabled, None));
}

fn reopen_exactly(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<TestExtension>,
    mut opened: AttachedCodingSession,
) -> AttachedCodingSession {
    let locator = executor.block_on(opened.detach()).unwrap().unwrap();
    config.session.bind_plakous_locator(locator).unwrap();
    config.store.save(&mut config.session).unwrap();
    executor.block_on(opened.release()).unwrap().unwrap();
    executor
        .block_on(startup::build_session_from_config(config, &mut Vec::new()))
        .unwrap()
        .unwrap()
}

fn fork_now(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<TestExtension>,
    opened: &mut AttachedCodingSession,
) -> CliResult<()> {
    let (events, _notes) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    executor
        .block_on(fork::handle_fork(
            InterruptWatch::new(&receiver, &state),
            config,
            opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
}

/// A Claude session made before skills (or with them off) reopens exactly
/// once they are on, keeping its tools; one made with them reopens exactly
/// once they are off, keeping `Skill`. The reopen follows what the session
/// froze, never the live setting.
#[test]
fn an_exact_reopen_keeps_the_skill_tool_the_session_froze_in_both_directions() {
    for created_with_skills in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, opened) = claude_api_host(root.path(), &executor, created_with_skills);
        assert_eq!(froze_skill_tool(&executor, &config), created_with_skills);
        set_skills(&mut config, !created_with_skills);
        let reopened = reopen_exactly(&executor, &mut config, opened);
        assert_eq!(
            froze_skill_tool(&executor, &config),
            created_with_skills,
            "reopen created_with_skills={created_with_skills}"
        );
        executor.block_on(reopened.release()).unwrap().unwrap();
    }
}

/// A rebind or switch successor inherits its source's skills on/off whatever
/// tuple it moves to, so canonical tool authority never changes: Claude to
/// OpenAI and back with skills on keeps `Skill` registered (presented only on
/// Claude), and a toggle before the rebind changes nothing.
#[test]
fn a_cross_family_rebind_keeps_the_sources_skills_in_both_directions() {
    for setting_at_rebind in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, mut opened) = claude_api_host(root.path(), &executor, true);
        assert!(froze_skill_tool(&executor, &config));
        set_skills(&mut config, setting_at_rebind);
        let to_openai = rebind_to(
            &executor,
            &mut config,
            &mut opened,
            "openai",
            "gpt-5.6-luna",
        );
        assert_eq!(to_openai.unwrap(), Rebound::Switched);
        assert!(froze_skill_tool(&executor, &config), "openai successor");
        let to_claude = rebind_to(
            &executor,
            &mut config,
            &mut opened,
            "anthropic",
            "claude-sonnet-5",
        );
        assert_eq!(to_claude.unwrap(), Rebound::Switched);
        assert!(froze_skill_tool(&executor, &config), "claude successor");
        executor.block_on(opened.release()).unwrap().unwrap();
    }
}

/// An OpenAI session made with skills on rebinds onto Claude keeping `Skill`,
/// which the Claude successor now presents.
#[test]
fn an_openai_session_with_skills_rebinds_onto_claude() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = api_topology_host_on(
        root.path(),
        &executor,
        None,
        crate::binding::PlatformProvider::OpenAi,
        "gpt-5.6-luna",
        crate::skills::SkillsSetting::new(true, None),
    );
    assert!(froze_skill_tool(&executor, &config));
    let rebound = rebind_to(
        &executor,
        &mut config,
        &mut opened,
        "anthropic",
        "claude-sonnet-5",
    );
    assert_eq!(rebound.unwrap(), Rebound::Switched);
    assert!(froze_skill_tool(&executor, &config));
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A Claude session made with skills off (as every session a build without
/// skills made) never gains `Skill`: a model switch after they are turned on
/// keeps its tools.
#[test]
fn a_claude_model_switch_on_a_session_made_without_skills_never_gains_skill() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = claude_api_host(root.path(), &executor, false);
    set_skills(&mut config, true);
    let rebound = rebind_to(
        &executor,
        &mut config,
        &mut opened,
        "anthropic",
        "claude-opus-5",
    );
    assert_eq!(rebound.unwrap(), Rebound::Switched);
    assert!(!froze_skill_tool(&executor, &config));
    let rebound = rebind_to(
        &executor,
        &mut config,
        &mut opened,
        "openai",
        "gpt-5.6-luna",
    );
    assert_eq!(rebound.unwrap(), Rebound::Switched);
    assert!(!froze_skill_tool(&executor, &config));
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A fork is a successor of its source: on Claude and OpenAI alike it keeps
/// whether the source's skills are on after `skills.enabled` changed (made
/// without them and forked with them on, or the reverse), so it is admitted
/// with the source's tools, re-reads the roots under that (a skill written
/// since is listed only when they are on), and a fork of the fork keeps them.
#[test]
fn a_fork_after_skills_were_toggled_keeps_the_sources_skills() {
    for (provider, model) in [
        (
            crate::binding::PlatformProvider::Anthropic,
            "claude-sonnet-5",
        ),
        (crate::binding::PlatformProvider::OpenAi, "gpt-5.6-luna"),
    ] {
        for created_with_skills in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let executor = ApplicationExecutor::new().unwrap();
            let (mut config, mut opened) = api_topology_host_on(
                root.path(),
                &executor,
                None,
                provider,
                model,
                crate::skills::SkillsSetting::new(created_with_skills, None),
            );
            let case = format!("{model} created_with_skills={created_with_skills}");
            assert_eq!(
                froze_skill_tool(&executor, &config),
                created_with_skills,
                "{case}"
            );
            set_skills(&mut config, !created_with_skills);
            let skill = config
                .session
                .workspace_root
                .join(".agents/skills/review-pr");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                "---\nname: review-pr\ndescription: Review a PR.\n---\nSteps.\n",
            )
            .unwrap();
            let source_id = config.session.id.clone();
            let forked = fork_now(&executor, &mut config, &mut opened);
            assert!(forked.is_ok(), "{case}: {forked:?}");
            assert_ne!(config.session.id, source_id, "{case}: the fork attached");
            assert_eq!(
                froze_skill_tool(&executor, &config),
                created_with_skills,
                "{case}"
            );
            let listed = instruction_sources(&opened)
                .unwrap()
                .iter()
                .any(|source| source.source_id() == plakous::prompts::skills::SKILLS_SOURCE_ID);
            assert_eq!(
                listed, created_with_skills,
                "{case}: the fork re-read the roots"
            );
            let again = fork_now(&executor, &mut config, &mut opened);
            assert!(again.is_ok(), "{case}: {again:?}");
            assert_eq!(
                froze_skill_tool(&executor, &config),
                created_with_skills,
                "{case}"
            );
            executor.block_on(opened.release()).unwrap().unwrap();
        }
    }
}

fn row_texts(receiver: &crate::surface_protocol::HostEventReceiver<TestExtension>) -> Vec<String> {
    receiver
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(row) => row.text().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Reopen projects a recorded fork; an unrecorded request is withdrawn,
/// never compiled or replayed after a crash.
fn prepared_fork_across_open(
    executor: &ApplicationExecutor,
    mut config: HostConfig<TestExtension>,
    mut opened: AttachedCodingSession,
    canonical_published: bool,
    _names_reread_instructions: bool,
) {
    let source_id = config.session.id.clone();
    let source_locator = executor.block_on(opened.detach()).unwrap().unwrap();
    config.session.bind_plakous_locator(source_locator).unwrap();
    config.store.save(&mut config.session).unwrap();
    let request = sessions::projector::WorkflowRequest {
        operation_id: "plak:fork:prepared-before-open".into(),
        source_locator,
        kind: sessions::SessionWorkflowKind::Fork,
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    std::fs::write(
        config.session.workspace_root.join("AGENTS.md"),
        "written while the fork was prepared",
    )
    .unwrap();
    let published = canonical_published.then(|| {
        let target_id = request.successor_product_id();
        executor
            .block_on(fork::seed_fork_tree(
                &config,
                &opened,
                &target_id,
                source_locator,
            ))
            .expect("the executor drives the call")
            .unwrap();
        let runtime = executor
            .block_on(fork::fork_runtime_binding(
                &config,
                &target_id,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        let outcome = executor
            .block_on(opened.fork(
                SessionOperationId::new(&request.operation_id).unwrap(),
                runtime,
            ))
            .unwrap()
            .unwrap();
        let plakous::sessions::ForkSessionOutcome::Forked { target, .. } = outcome else {
            panic!("the canonical fork did not publish: {outcome:?}");
        };
        target
    });
    executor.block_on(opened.release()).unwrap().unwrap();
    let reopened = executor
        .block_on(startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        config.session.id, source_id,
        "reopen keeps its requested source"
    );
    let notices = config
        .store
        .withdraw_unadmitted_workflows(&mut config.session)
        .unwrap();
    if let Some(target) = published {
        assert_eq!(notices, [] as [TranscriptRow; 0]);
        let successor = config.store.load(&request.successor_product_id()).unwrap();
        assert_eq!(successor.plakous_locator, Some(target));
        let views = config.store.load_views(&source_id).unwrap().unwrap();
        assert!(
            views.state.workflows[&request.operation_id]
                .settled
                .is_some()
        );
    } else {
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].text(), Some(request.withdrawal_note()));
        assert_eq!(
            config
                .store
                .withdraw_unadmitted_workflows(&mut config.session)
                .unwrap(),
            [] as [TranscriptRow; 0]
        );
        assert_eq!(
            config
                .store
                .load_intents(&source_id)
                .unwrap()
                .workflow_requests,
            [] as [sessions::projector::WorkflowRequest; 0]
        );
    }

    executor.block_on(reopened.release()).unwrap().unwrap();
}

#[test]
fn prepared_fork_recovers_before_open_in_a_fresh_process() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = actual_api_topology_host(root.path(), &executor);
    prepared_fork_across_open(&executor, config, opened, false, true);
}

#[test]
fn published_fork_recovers_before_open_in_a_fresh_process() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = actual_api_topology_host(root.path(), &executor);
    prepared_fork_across_open(&executor, config, opened, true, true);
}

fn native_injected_host(
    root: &std::path::Path,
    executor: &ApplicationExecutor,
) -> (HostConfig<TestExtension>, AttachedCodingSession) {
    let (mut config, _) = fresh_injected_attached_host(root);
    let opened = executor
        .block_on(startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    (config, opened)
}

#[test]
fn prepared_native_fork_recovers_before_open_in_a_fresh_process() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = native_injected_host(root.path(), &executor);
    prepared_fork_across_open(&executor, config, opened, false, false);
}

#[test]
fn published_native_fork_recovers_before_open_in_a_fresh_process() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = native_injected_host(root.path(), &executor);
    prepared_fork_across_open(&executor, config, opened, true, false);
}

/// A fork saved without attaching names the `AGENTS.md` it re-read from
/// the plan it was stored with.
#[test]
fn a_saved_fork_names_the_agents_md_it_reread() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let before = instruction_sources(&opened);
    std::fs::write(
        config.session.workspace_root.join("AGENTS.md"),
        "written before the fork",
    )
    .unwrap();
    let source = executor.block_on(opened.detach()).unwrap().unwrap();
    let target_id = sessions::projector::WorkflowRequest {
        operation_id: "plak:fork:saved-note".into(),
        source_locator: source,
        kind: sessions::SessionWorkflowKind::Fork,
    }
    .successor_product_id();
    executor
        .block_on(fork::seed_fork_tree(&config, &opened, &target_id, source))
        .expect("the executor drives the call")
        .unwrap();
    let runtime = executor
        .block_on(fork::fork_runtime_binding(
            &config,
            &target_id,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let outcome = executor
        .block_on(
            sessions.fork(plakous::sessions::ForkSessionRequest::recompiling(
                SessionOperationId::new("plak:fork:saved-note").unwrap(),
                source,
                runtime,
            )),
        )
        .unwrap()
        .unwrap();
    let plakous::sessions::ForkSessionOutcome::Forked { target, .. } = outcome else {
        panic!("the fork did not publish: {outcome:?}");
    };
    let (events, notes_rx) = host_event_sink();
    executor
        .block_on(note_reread_instructions_at(
            &config, &events, before, target,
        ))
        .expect("the executor drives the call");
    executor.block_on(opened.release()).unwrap().unwrap();
    assert_eq!(
        row_texts(&notes_rx),
        ["AGENTS.md re-read for this branch: workspace root added"]
    );
}

#[test]
fn refused_fork_does_not_strand_the_session() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = native_injected_host(root.path(), &executor);
    prepared_fork_across_open(&executor, config, opened, false, false);
}

/// `[roster] subagents` turned off after the session started: a plain
/// `/fork` recompiles the session's own tree and still lands.
#[test]
fn plain_fork_after_subagents_were_turned_off_lands() {
    use crate::runtime::fork::handle_fork;
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    config.topology_enabled = false;
    let source_id = config.session.id.clone();
    let (events, _notes) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let forked = executor
        .block_on(handle_fork(
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call");
    let message = forked
        .as_ref()
        .err()
        .map(|error| error.message().to_owned());
    executor.block_on(opened.release()).unwrap().unwrap();
    assert!(forked.is_ok(), "/fork failed: {message:?}");
    assert_ne!(config.session.id, source_id);
}

/// Fork now onto a direct API root branches onto that account's route
/// with a tree of its own, names the `AGENTS.md` it re-read, and leaves
/// no build lease behind.
#[test]
fn fork_now_onto_an_api_root_branches_onto_its_own_tree() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source_id = config.session.id.clone();
    std::fs::write(
        config.session.workspace_root.join("AGENTS.md"),
        "written before the fork",
    )
    .unwrap();
    let (events, notes_rx) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let rebound = executor
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
            // Direct: plakous-default binds native-ptc here, whose sidecar this
            // test does not install.
            Some(ProductComposition::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("gpt-5.6-sol"),
            None,
        ))
        .expect("the executor drives the call")
        .unwrap();
    assert_eq!(rebound, Rebound::Switched);
    let target_id = config.session.id.clone();
    assert_ne!(target_id, source_id);
    // The switch took its own tree for the root, so it ran the
    // custody-attach scan.
    let hub = crate::topology_runtime::lookup(&target_id).expect("the switch took its tree");
    assert!(!hub.finish_attach_scan(), "the switch finished the scan");
    assert_eq!(
        config.session.source_product_session_id.as_deref(),
        Some(source_id.as_str())
    );
    assert_eq!(
        config.session.target_selection,
        TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::OpenAi
        }
    );
    assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(config.session.effort.as_deref(), Some("medium"));
    assert_eq!(config.session.composition, ProductComposition::Direct);
    assert!(matches!(
        config.target,
        HostTargetAuthority::Platform { .. }
    ));
    assert_eq!(config.model_id, "gpt-5.6-sol");
    assert_eq!(
        durable_tree_id(&config.topology_rows(), &config.session),
        target_id
    );
    // The stored node names the tree the branch compiled against.
    let stored = config.store.load(&target_id).unwrap();
    assert_eq!(stored.node, sessions::SessionNode::genesis(&target_id));
    // It froze the profile it was forked onto.
    assert_eq!(
        stored.root_profile,
        Some(sessions::RootProfile::from_roster(
            &crate::orchestration::RosterInput::unpinned("gpt-5.6-sol")
        ))
    );
    assert_eq!(stored.runner, None);
    let names = topology_tool_names(&target_id);
    assert!(!names.is_empty(), "{names:?}");
    let notes = notes_rx
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(row) => row.text().map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        notes
            .iter()
            .any(|note| note == "AGENTS.md re-read for this branch: workspace root added"),
        "{notes:?}"
    );
    executor.block_on(opened.release()).unwrap().unwrap();
    assert!(crate::topology_runtime::lookup(&target_id).is_none());
}

/// The root's tools belong to the session: a rebind onto another model
/// keeps the MCP records frozen at creation, so canonical tool authority
/// is unchanged, nothing is listed again, and the successor binds them.
#[test]
fn a_rebind_keeps_the_sessions_frozen_mcp_tools_without_listing_again() {
    let server = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap()
        .join("mcp-fake-server");
    assert!(
        server.is_file(),
        "run the workspace tests so the fixture is built"
    );
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let list_log = root.path().join("mcp-lists.log");
    let template = crate::binding::RootTemplateFact {
        name: "lead".to_owned(),
        account: "openai".to_owned(),
        model: "gpt-5.6-luna".to_owned(),
        tools: crate::tool_plan::TemplateToolSelection::parse(["mcp:fake"]).unwrap(),
        tool_sections: std::collections::BTreeMap::from([(
            "mcp_servers.fake".to_owned(),
            format!(
                "[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {:?}\n\
                     args = [\"--list-log\", {:?}]\nenv = {{ }}\nsecret_env = {{ }}\n\
                     startup_seconds = 20\ncall_seconds = 120\n",
                server.to_str().unwrap(),
                list_log.to_str().unwrap()
            ),
        )]),
    };
    let unlisted = crate::frozen_tools::FrozenRootTools::at_creation(
        Some(template),
        Some("openai"),
        Some("gpt-5.6-luna"),
    );
    let tools = unlisted.selection();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let records = crate::mcp_binding::bind_mcp_tools(
        &tools.selection,
        &tools.tool_sections,
        &workspace,
        crate::agent::ProductStateRoots::with_doubles(root.path().join("h"), root.path().join("s"))
            .named_secrets(),
        None,
        crate::mcp_binding::McpNodeServices::default(),
    )
    .unwrap()
    .placed_records();
    let frozen = unlisted.with_mcp_tools(records);
    let (mut config, mut opened) =
        actual_api_topology_host_with(root.path(), &executor, Some(frozen.clone()));
    assert_eq!(std::fs::read_to_string(&list_log).unwrap(), "list\n");

    let (events, _notes) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let rebound = executor
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
            &crate::orchestration::RosterInput::unpinned("gpt-5.6-sol"),
            None,
        ))
        .expect("the executor drives the call")
        .unwrap();
    assert_eq!(rebound, Rebound::Switched);
    let successor = config.store.load(&config.session.id).unwrap();
    assert_eq!(successor.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(successor.frozen_tools.as_ref(), Some(&frozen));
    let registrations = crate::agent::injected_root_tool_registrations(
        &frozen,
        &successor,
        &config.state_roots,
        crate::mcp_binding::McpBinding::Lease,
    )
    .unwrap();
    assert!(
        registrations
            .iter()
            .any(|registration| registration.name().as_str() == "mcp__fake__echo"),
        "the successor binds the frozen records"
    );
    assert_eq!(
        std::fs::read_to_string(&list_log).unwrap(),
        "list\n",
        "neither the rebind nor a later build lists again"
    );
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A rebind whose target route cannot serve one of the session's tools
/// refuses with one line naming the tool and the remedy, before any
/// durable work.
#[test]
fn a_rebind_to_a_route_without_a_frozen_tool_refuses_up_front() {
    let root = tempfile::tempdir().unwrap();
    let mut config = code_mode_source_with_a_prefix(root.path());
    config.session.frozen_tools = Some(crate::frozen_tools::FrozenRootTools::new(Some(
        crate::binding::RootTemplateFact {
            name: "lead".to_owned(),
            account: "claude".to_owned(),
            model: "m".to_owned(),
            tools: crate::tool_plan::TemplateToolSelection::parse(["web_fetch"]).unwrap(),
            tool_sections: std::collections::BTreeMap::new(),
        },
    )));
    let mut opened = memory_attached_stub();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    let (events, _rx) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_rebind(
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            Some(crate::binding::CompositionSelection::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert_eq!(
        error.message(),
        "this session keeps its tools across a model change, and web_fetch is not available \
             on codex; start a fresh conversation on that model"
    );
    assert_eq!(
        error.remedy(),
        Some(&plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {})
    );
    assert!(
        test_kit::scripted(&config)
            .rebind_builds
            .lock()
            .unwrap()
            .is_empty(),
        "no target runtime was built"
    );
    assert!(
        !matches!(
            config.store.load_intents(&config.session.id),
            Ok(intents) if !intents.workflow_requests.is_empty()
        ),
        "no workflow request was written"
    );
}

#[test]
fn production_api_open_failures_do_not_steal_or_leak_root_leases() {
    for failure in [
        "occupancy",
        "runtime-db",
        "model-admission",
        "canonical-open",
    ] {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, opened) = actual_api_topology_host(root.path(), &executor);
        let tree_id = config.session.id.clone();
        let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
        let mut parent = retained_api_parent(&tree_id);
        match failure {
            "occupancy" => {
                parent.cwd = root.path().join(".plak/worktrees/not-a-directory");
                std::fs::create_dir_all(parent.cwd.parent().unwrap()).unwrap();
                std::fs::write(&parent.cwd, b"fixture").unwrap();
            }
            "runtime-db" => {
                let product = root.path().join("bad-runtime");
                std::fs::create_dir_all(product.join("runtime.sqlite3")).unwrap();
                parent.state_roots = crate::agent::ProductStateRoots::with_doubles(
                    product,
                    root.path().join("bad-state"),
                )
                .with_test_platform_credentials(Arc::new(TopologyApiCredentials));
            }
            "model-admission" => parent.model = "unsupported-model".into(),
            _ => parent.model = "gpt-5.6-sol".into(),
        }
        let result = executor
            .block_on(crate::agent::open_api_root_coding_session(
                parent,
                opened.locator(),
                None,
            ))
            .unwrap();
        assert!(result.is_err(), "{failure}");
        assert!(
            Arc::ptr_eq(&hub, &crate::topology_runtime::lookup(&tree_id).unwrap()),
            "{failure} stole the source lease"
        );
        assert_eq!(retained_api_parent(&tree_id).model, "gpt-5.6-luna");
        executor.block_on(opened.release()).unwrap().unwrap();
        assert!(
            crate::topology_runtime::lookup(&tree_id).is_none(),
            "{failure} leaked an acquired lease"
        );
    }
}

#[test]
fn production_api_detached_source_restoration_retires_previous_root_lease() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let tree_id = config.session.id.clone();
    let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
    let names = topology_tool_names(&tree_id);
    let locator = executor.block_on(opened.detach()).unwrap().unwrap();
    assert!(Arc::ptr_eq(
        &hub,
        &crate::topology_runtime::lookup(&tree_id).unwrap()
    ));
    executor
        .block_on(restore_detached_source(
            &config,
            &mut opened,
            locator,
            &mut Vec::new(),
        ))
        .expect("the executor drives the call")
        .unwrap();
    assert!(Arc::ptr_eq(
        &hub,
        &crate::topology_runtime::lookup(&tree_id).unwrap()
    ));
    assert_eq!(topology_tool_names(&tree_id), names);
    assert!(opened.agent().is_some());
    executor.block_on(opened.release()).unwrap().unwrap();
    assert!(
        crate::topology_runtime::lookup(&tree_id).is_none(),
        "detached prior owner leaked a root lease"
    );
}

/// The hub defect on the production attach path. A model rebind that stays
/// on its tree builds the successor's runtime with `LookupOnly`
/// (`prepare_profile_runtime` with no own tree): it compiles against the
/// attached hub and takes no root attachment. A hand-off that releases the
/// predecessor right after the commit and activates the attached successor
/// without opening it again (live succession step 6) gives back the last
/// attachment, but the successor's runtime is resident in the tree, so the
/// tree stays in custody and the hub keeps it (spec 03 §14).
#[test]
fn a_lookup_only_successor_attach_keeps_the_hub_across_the_predecessors_release() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (config, opened) = actual_api_topology_host(root.path(), &executor);
    let tree_id = config.session.id.clone();
    let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
    let mut target = config.session.clone();
    target.model = Some("gpt-5.6-sol".into());
    let mut attached = None;
    let successor = executor
        .block_on(api_runtime_for_record(
            &config,
            &target,
            false,
            crate::topology_runtime::TopologyHubAttach::LookupOnly,
            Some(&tree_id),
            &mut attached,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(attached, None, "the successor's attach took a root lease");

    executor
        .block_on(opened.release_for_handoff())
        .unwrap()
        .unwrap();

    let survived = crate::topology_runtime::lookup(&tree_id)
        .is_some_and(|runtime| Arc::ptr_eq(&runtime, &hub));
    drop(successor);
    executor
        .block_on(crate::topology_runtime::force_release(&tree_id))
        .unwrap();
    assert!(
        survived,
        "the predecessor's release dropped the last root lease and shut the hub down"
    );
}

/// An effort-only change is not a model switch: a profile fork onto the
/// same account and model must not report a switch between a model and
/// itself, nor the caveat about the previous model's reasoning.
#[test]
fn an_effort_only_rebind_is_not_reported_as_a_model_switch() {
    let before = RebindBefore {
        account: Some("codex"),
        model: "gpt-5.6-luna",
        effort: Some(ReasoningEffort::Low),
    };
    let raised = RebindBefore {
        effort: Some(ReasoningEffort::High),
        ..before
    };
    let note = rebind_note(before, raised);
    assert!(
        note.starts_with("Effort changed \u{b7} codex \u{b7} gpt-5.6-luna \u{b7} high"),
        "{note}"
    );
    assert!(!note.contains("Model switched"), "{note}");
    assert!(
        !note.contains("native reasoning from the previous model"),
        "the caveat does not apply when the model has not moved: {note}"
    );

    let other_model = RebindBefore {
        model: "gpt-5.3-codex-spark",
        ..before
    };
    let switched = rebind_note(before, other_model);
    assert!(
        switched.starts_with("Model switched \u{b7} codex \u{b7} gpt-5.3-codex-spark"),
        "{switched}"
    );
    assert!(switched.contains("native reasoning from the previous model"));

    // A cross-provider move on the same model name is still a switch.
    let other_account = RebindBefore {
        account: Some("openrouter"),
        ..before
    };
    assert!(rebind_note(before, other_account).starts_with("Model switched"));
}

#[test]
fn rebind_boundary_requires_empty_queue_and_no_unresolved_work() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionRecord::new(root.path().to_path_buf());
    assert!(session_rebind_boundary_ready(&session));
    let mut queued = session.clone();
    queued.pending_queue.push(SessionQueueItem::new("later"));
    assert!(!session_rebind_boundary_ready(&queued));
}

#[test]
fn rebind_boundary_uses_submission_facts_without_delivery_receipts() {
    use crate::sessions::projector::{
        Delivery, ProductIntents, SessionViews, Submission, SubmissionKind,
    };

    let mut session = SessionRecord::new("/work".into());
    let item = SessionQueueItem::new("submitted");
    let intents = ProductIntents {
        submissions: vec![Submission {
            command_id: "rebind-boundary-command".into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: item.id,
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text,
            submitted_at_ms: item.submitted_at_ms,
        }],
        ..ProductIntents::default()
    };
    let mut views = SessionViews::empty();
    session.queue_commands = sessions::QueueCommands::from_join(&intents, Some(&views));
    assert!(!session_rebind_boundary_ready(&session));

    for delivery in [
        Delivery::Accepted,
        Delivery::Rejected {
            error: "busy".into(),
        },
    ] {
        views
            .state
            .deliveries
            .insert("rebind-boundary-command".into(), delivery);
        session.queue_commands = sessions::QueueCommands::from_join(&intents, Some(&views));
        assert!(session_rebind_boundary_ready(&session));
    }
}

#[test]
fn rebind_defaults_reasoning_to_medium_when_admitted() {
    let catalog = vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".into(),
        label: "Codex".into(),
        ready: true,
        entries: vec![
            plakous_host_protocol::views::ModelEntry {
                id: "plain-non-reasoning".into(),
                tag: String::new(),
                reasoning: false,
                effort_options: Vec::new(),
                extras: Vec::new(),
                unavailable_reason: None,
            },
            plakous_host_protocol::views::ModelEntry {
                id: "gpt-reasoning".into(),
                tag: String::new(),
                reasoning: true,
                effort_options: vec!["low".into(), "medium".into()],
                extras: Vec::new(),
                unavailable_reason: None,
            },
        ],
    }];
    assert_eq!(
        validate_rebind_effort(&catalog, "codex", "plain-non-reasoning", None).unwrap(),
        None
    );
    assert_eq!(
        validate_rebind_effort(&catalog, "codex", "gpt-reasoning", None).unwrap(),
        Some("medium".into())
    );
    assert_eq!(
        validate_rebind_effort(&catalog, "codex", "gpt-reasoning", Some("low")).unwrap(),
        Some("low".into())
    );
    assert!(validate_rebind_effort(&catalog, "codex", "gpt-reasoning", Some("invalid")).is_err());
    assert!(
        validate_rebind_effort(&catalog, "codex", "plain-non-reasoning", Some("medium")).is_err()
    );
}

#[test]
fn restored_paused_tree_epoch_blocks_rebind_for_session_tree_id() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = host_config_with_tree_epoch_rebind_block(root.path());
    assert!(tree_epoch_blocks_rebind(
        &config.topology_rows(),
        &config.session
    ));
}

#[test]
fn handle_rebind_fails_closed_when_tree_epoch_has_live_descendants() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    install_live_descendant_on_hub(
        &durable_tree_id(&config.topology_rows(), &config.session),
        &config.cwd,
    );
    let mut opened = memory_attached_stub();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();

    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let (_actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_rebind(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            None,
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert_eq!(
        error.message(),
        crate::agent_tree::ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS
    );
    assert!(no_unsettled_rebind_requests(&config));
}

/// A fork opens a tree of its own, so it refuses while the source's tree has
/// live descendants, which would stay behind with the source; nothing is
/// written.
#[test]
fn handle_fork_refuses_while_the_source_tree_has_live_descendants() {
    use crate::runtime::fork::handle_fork;
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    install_live_descendant_on_hub(
        &durable_tree_id(&config.topology_rows(), &config.session),
        &config.cwd,
    );
    let mut opened = memory_attached_stub();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let (_actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_fork(
            InterruptWatch::new(&actions_rx, &interrupt_state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert!(
        error
            .message()
            .starts_with("cannot fork while this session's sub-agents run"),
        "{}",
        error.message()
    );
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        []
    );
}

/// A Codex Code Mode (native-ptc) prefix is replayable onto every
/// presentation: the kernels re-project `exec`/`wait` cells at dispatch
/// (Responses/Chat carry the original string input, Messages wraps it as
/// `{"input": ...}` — see the kernel tests), so the host admits the rebind
/// like any other. A refusal here ("cannot replay a Codex Code Mode
/// (native-ptc) prefix onto another presentation") would make a profile
/// fork after a Code Mode turn a dead end. The memory stub cannot complete
/// the branch, so only the admission is asserted.
#[test]
fn a_code_mode_prefix_is_admitted_for_a_direct_target() {
    let root = tempfile::tempdir().unwrap();
    let mut config = code_mode_source_with_a_prefix(root.path());
    assert_eq!(config.resolved_composition_label(), "native-ptc");
    let mut opened = memory_attached_stub();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();

    let (events, rx) = host_event_sink();
    let (_actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    let result = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_rebind(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            Some(crate::binding::CompositionSelection::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
            None,
        ))
        .expect("the executor drives the call");
    let mut notes = Vec::new();
    if let Err(error) = &result {
        notes.push(error.message().to_owned());
    }
    while let Ok(event) = rx.try_recv() {
        if let HostEvent::Row(TranscriptRow::Notice(
            plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
        )) = event
        {
            notes.push(note);
        }
    }
    assert!(
        notes
            .iter()
            .all(|note| !note.contains("Code Mode") && !note.contains("native-ptc")),
        "no Code Mode prefix refusal: {notes:?}"
    );
    let error = result.unwrap_err();
    assert!(
        error
            .message()
            .contains("projector could not read the feed"),
        "unexpected refusal: {error}"
    );
    assert!(
        !test_kit::scripted(&config)
            .rebind_builds
            .lock()
            .unwrap()
            .is_empty(),
        "the target runtime must have compiled before the stub's missing feed refuses publication"
    );
}

#[test]
fn interrupt_refuses_rebind_before_any_branch_is_created() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
        plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v2".into(),
                tag: "offline".into(),
                reasoning: true,
                effort_options: vec!["medium".into()],
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        },
    ]);
    let mut opened = memory_attached_stub();
    let source = opened.locator();
    config.session.bind_plakous_locator(source).unwrap();
    let (events, _rx) = host_event_sink();
    let (actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    actions_tx.send(UserAction::Interrupt).unwrap();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);

    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_rebind(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            None,
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
            None,
        ))
        .expect("the executor drives the call")
        .expect_err("an interrupted rebind must not create a branch");

    assert!(
        error.message().contains("interrupted"),
        "rebind refusal must name the interrupt: {}",
        error.message()
    );
    // the exact source binding and model survive untouched.
    assert_eq!(config.session.plakous_locator, Some(source));
    assert_eq!(config.session.model.as_deref(), Some("offline-loopback-v1"));
    assert!(no_unsettled_rebind_requests(&config));
}

#[test]
fn handle_rebind_fails_closed_when_tree_epoch_blocks_rebind() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    test_kit::seed_reopened_tree_with_live_descendant(
        &config.topology_rows(),
        &config.session.id,
        &config.cwd,
    )
    .unwrap();
    let mut opened = memory_attached_stub();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();

    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let (_actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_rebind(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            None,
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert_eq!(
        error.message(),
        crate::agent_tree::ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS
    );
    assert!(no_unsettled_rebind_requests(&config));
}

/// A switch refused because its successor's tools would change says so in
/// `/rebind` terms; other refusals keep the canonical text.
#[test]
fn a_switch_refused_for_changed_tools_says_so() {
    assert_eq!(
        complete::rebind_refusal(
            AgentSessionsErrorKind::Revalidation,
            "rebind changed canonical tool authority outside its authority",
        ),
        Some("cannot switch: this session's tools would change; start a new session on that model")
    );
    assert_eq!(
        complete::rebind_refusal(
            AgentSessionsErrorKind::Revalidation,
            "rebind changed effect policy outside its authority",
        ),
        None
    );
}

mod remote_skills;
mod succession;
