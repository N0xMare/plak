//! Shared fixtures for the `runtime` unit tests.

use crate::runtime::test_kit::TestExtension;
type HostConfig = crate::runtime::HostConfig<TestExtension>;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use plakous::agent::ReasoningEffort;
use plakous::sessions::AgentSessions;
use plakous::subscriptions::NativeProvider;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, overflow_runtime_binding, runtime_binding,
};

use crate::agent::{AttachedCodingSession, ProductStateRoots};
use crate::binding::CompositionSelection as ProductComposition;
use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, TargetSelection, WireBinding};
use crate::sessions::{self, SessionRecord};

use super::*;
use crate::runtime::rebind::*;
use crate::runtime::startup::*;

pub(super) fn started_observation(
    arguments: &serde_json::Value,
) -> plakous::agent::ToolCallObservation {
    serde_json::from_value(serde_json::json!({
        "subject": null,
        "arguments_preview": serde_json::to_string_pretty(arguments).unwrap(),
    }))
    .expect("observation wire shape")
}

pub(super) fn nested_lineage(
    cell: &str,
    ordinal: u32,
    parent: plakous::model::CallId,
    child: plakous::model::CallId,
) -> plakous::agent::ToolCallerLineageView {
    let canonical =
        plakous::model::ToolCallerLineage::local_programmatic(7, 1, cell, ordinal, child, 1)
            .unwrap();
    plakous::agent::ToolCallerLineageView::from_lineage(&canonical, Some(parent))
}

pub(super) fn reasoning_block(
    location: plakous::model::OutputLocation,
    visibility: plakous::model::ReasoningVisibility,
) -> plakous::model::BlockStarted {
    plakous::model::BlockStarted::new(
        plakous::model::EntryId::try_from(uuid::Uuid::from_u128(0x0e01)).unwrap(),
        location,
        plakous::model::BlockStart::Reasoning {
            visibility,
            replay: None,
        },
    )
}

pub(super) fn pending_reasoning_block(
    location: plakous::model::OutputLocation,
) -> plakous::model::BlockStarted {
    plakous::model::BlockStarted::new(
        plakous::model::EntryId::try_from(uuid::Uuid::from_u128(0x0e02)).unwrap(),
        location,
        plakous::model::BlockStart::ReasoningPending { replay: None },
    )
}

pub(super) fn durable_authority_count(database: &std::path::Path, table: &str) -> i64 {
    if !database.exists() {
        return 0;
    }
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

pub(super) fn fresh_injected_attached_host(
    root: &std::path::Path,
) -> (HostConfig, Arc<ScriptedRuntimeFactory>) {
    fresh_injected_attached_host_for::<TestExtension>(root)
}

pub(super) fn fresh_injected_attached_host_for<P: plakous_host_protocol::ProtocolExtension>(
    root: &std::path::Path,
) -> (super::HostConfig<P>, Arc<ScriptedRuntimeFactory>) {
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.join("immediate-quit-evidence.jsonl"),
    )
    .unwrap();
    let target = runtime_binding(&workspace, controller.clone())
        .unwrap()
        .target_binding();
    let runtime_workspace = workspace.clone();
    let injection = Arc::new(ScriptedRuntimeFactory {
        runtime: Arc::new(move |_| {
            runtime_binding(&runtime_workspace, controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let mut session = SessionRecord::new(workspace.clone());
    session.account = Some("codex".into());
    session.model = Some("offline-loopback-v1".into());
    session.effort = Some("medium".into());
    session.composition = crate::binding::CompositionSelection::JsPtc;
    (
        super::HostConfig {
            surface: test_kit::recording_surface(),
            auto_continue: true,
            topology_admission: Some(Arc::new(|| Ok(true))),
            state_roots: ProductStateRoots::with_doubles(home.clone(), home.join("state")),
            home: SessionHome::new(home.join("sessions")),
            store: Arc::new(crate::session_store::MemoryStore::new()),
            topology_enabled: true,
            product_home: home.clone(),
            session,
            target: test_kit::registered_authority(NativeProvider::Codex),
            model_id: "offline-loopback-v1".into(),
            effort: Some(ReasoningEffort::Medium),
            effort_explicit: true,
            compaction_enabled: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: workspace,
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
            resume_publication: None,
            runtime_factory: injection.clone(),
            ready_path: ReadyPathTiming::default(),
            seed_history: None,
            source_releases: SourceReleases::default(),
        },
        injection,
    )
}

pub(super) fn no_effort_openrouter_injected_host(root: &std::path::Path) -> HostConfig {
    let (mut config, _) = fresh_injected_attached_host(root);
    config.session.account = Some("openrouter".into());
    config.session.model = Some("vendor/model-v1".into());
    config.session.effort = None;
    config.session.composition = ProductComposition::Direct;
    config.session.target_selection = TargetSelection::OpenRouter {
        wire: WireBinding::resolve(ProtocolSelection::Messages),
        routing: OpenRouterRoutingSelection::GatewayManaged,
    };
    config.model_id = "vendor/model-v1".into();
    config.effort_explicit = false;
    config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
        plakous_host_protocol::views::ProviderSection {
            account: "openrouter".into(),
            label: "OpenRouter".into(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: "vendor/model-v1".into(),
                tag: "direct".into(),
                reasoning: false,
                effort_options: Vec::new(),
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        },
    ]);
    config
}

pub(super) fn overflow_injected_attached_host(
    root: &std::path::Path,
    provider: NativeProvider,
) -> (HostConfig, Arc<ScriptedRuntimeFactory>) {
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.join("overflow-inspect-evidence.jsonl"),
    )
    .unwrap();
    let target = overflow_runtime_binding(&workspace, controller.clone())
        .unwrap()
        .target_binding();
    let runtime_workspace = workspace.clone();
    let injection = Arc::new(ScriptedRuntimeFactory {
        // Every role gets the ordinary overflow binding. The harness has no
        // compaction binding that is exact for a tools-carrying source at
        // this ceiling, so a `/compact` driven through this fixture is
        // refused at runtime admission rather than reaching the ladder. Tests
        // here assert that the compact is attempted; the exhausted-ladder
        // wording is pinned by the presentation unit assertions and by the
        // agent-level refusal test.
        runtime: Arc::new(move |_| {
            overflow_runtime_binding(&runtime_workspace, controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let mut session = SessionRecord::new(workspace.clone());
    session.account = Some(
        match provider {
            NativeProvider::Xai => "grok",
            NativeProvider::Claude => "claude",
            _ => "codex",
        }
        .into(),
    );
    session.model = Some("offline-loopback-v1".into());
    session.effort = Some("medium".into());
    (
        HostConfig {
            surface: test_kit::recording_surface(),
            auto_continue: true,
            topology_admission: Some(Arc::new(|| Ok(true))),
            state_roots: ProductStateRoots::with_doubles(home.clone(), home.join("state")),
            home: SessionHome::new(home.join("sessions")),
            store: Arc::new(crate::session_store::MemoryStore::new()),
            topology_enabled: true,
            product_home: home.clone(),
            session,
            target: test_kit::registered_authority(provider),
            model_id: "offline-loopback-v1".into(),
            effort: Some(ReasoningEffort::Medium),
            effort_explicit: true,
            compaction_enabled: true,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: workspace,
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
            resume_publication: None,
            runtime_factory: injection.clone(),
            ready_path: ReadyPathTiming::default(),
            seed_history: None,
            source_releases: SourceReleases::default(),
        },
        injection,
    )
}

pub(super) fn send_attached_action(
    actions: &crate::surface_protocol::UserActionSender<TestExtension>,
    action: UserAction,
) {
    actions
        .send(action)
        .unwrap_or_else(|error| panic!("injected host action refused: {error:?}"));
}

pub(super) fn run_attached_actions(config: HostConfig, extra: Vec<UserAction>) -> Vec<HostEvent> {
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    for action in extra {
        send_attached_action(&actions, action);
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    events.try_iter().collect()
}

pub(super) fn usage_statuses(events: &[HostEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Usage(usage) => Some(usage.status.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn pending_tool(subject: &str, detail: &str) -> PendingToolInfo {
    PendingToolInfo {
        subject: subject.to_owned(),
        detail: detail.to_owned(),
    }
}

pub(super) fn host_config_with_tree_epoch_rebind_block(
    root: &std::path::Path,
) -> (HostConfig, String) {
    let (config, _) = fresh_injected_attached_host(root);
    let session_id = config.session.id.clone();
    test_kit::seed_restored_paused_tree(&config.topology_rows(), &session_id, &config.cwd).unwrap();
    assert!(tree_epoch_blocks_rebind(
        &config.topology_rows(),
        &config.session
    ));
    (config, session_id)
}

pub(super) fn memory_attached_stub() -> AttachedCodingSession {
    use plakous::sessions::SessionBackend;

    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::memory().expect("memory backend lease clock"))
            .expect("in-memory runtime sessions should initialize"),
    );
    AttachedCodingSession::stub_for_host_tests(
        sessions,
        "00000000-0000-4000-8000-000000000101/00000000-0000-4000-8000-000000000102/00000000-0000-4000-8000-000000000103"
            .parse()
            .expect("stub locator"),
    )
}

pub(super) fn no_unsettled_rebind_requests(config: &HostConfig) -> bool {
    let intents = config.store.load_intents(&config.session.id).unwrap();
    let views = config
        .store
        .load_views(&config.session.id)
        .unwrap()
        .unwrap_or_else(sessions::projector::SessionViews::empty);
    !intents.workflow_requests.iter().any(|request| {
        matches!(request.kind, sessions::SessionWorkflowKind::Rebind(_))
            && views
                .state
                .workflows
                .get(&request.operation_id)
                .is_none_or(|workflow| workflow.settled.is_none())
    })
}

pub(super) fn install_live_descendant_on_hub(tree_id: &str, workspace: &std::path::Path) {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

    if let Some(runtime) = crate::topology_runtime::lookup(tree_id) {
        let mut tree = runtime
            .tree_handle_for_test()
            .lock()
            .expect("hub tree lock");
        let _ = tree.resume_tree();
        let root_id = tree.root_agent_id.clone();
        tree.spawn_agent(&root_id, &SpawnAgentRequest::test_review(), None)
            .expect("hub spawn for live-descendant rebind test");
        return;
    }
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id,
        "root",
        "owner",
        workspace.to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    crate::topology_runtime::insert_root(tree_id, tree, None).unwrap();
}

pub(super) fn code_mode_source_with_a_prefix(root: &std::path::Path) -> HostConfig {
    let (mut config, _) = fresh_injected_attached_host(root);
    config.session.composition = crate::binding::CompositionSelection::NativePtc;
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
    config.session.record_admitted_root_turn();
    config
}

pub(super) fn host_event_sink() -> (
    Sender,
    crate::surface_protocol::HostEventReceiver<TestExtension>,
) {
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    (Sender::over(tx), rx)
}

pub(super) fn status_phases(events: &[HostEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Status { phase, .. } => Some(phase.as_str()),
            _ => None,
        })
        .collect()
}

/// Toggleable publication refusal; every other operation delegates.
pub(super) struct SaveFailure {
    pub fail: AtomicBool,
}
impl fault_store::StoreFaults for SaveFailure {
    fn before(
        &self,
        call: fault_store::StoreCall,
        _session: Option<&SessionRecord>,
    ) -> Result<(), String> {
        use fault_store::StoreCall;
        if matches!(
            call,
            StoreCall::Save | StoreCall::SaveSubmission | StoreCall::DiscardSubmission
        ) && self.fail.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err("injected publication failure".into());
        }
        Ok(())
    }
}

/// Only projector commits pay the delay or refusal.
pub(super) struct SlowProjection {
    pub delay: Duration,
    pub refuse: bool,
}
impl fault_store::StoreFaults for SlowProjection {
    fn before(
        &self,
        call: fault_store::StoreCall,
        _session: Option<&SessionRecord>,
    ) -> Result<(), String> {
        if call == fault_store::StoreCall::CommitProjectionViews {
            thread::sleep(self.delay);
            if self.refuse {
                return Err("injected projector commit refusal".into());
            }
        }
        Ok(())
    }
}
