//! Composition tests for the product coding session.
//!
//! The nine tests here read Plak's own authorities back after driving the
//! agent: the roster file a genesis freezes, the telemetry log an unavailable
//! sink writes, the tree store a child's seal survives in, the session facade's
//! durable round trip. The rest of the agent suite runs on the port doubles and
//! stayed in the hosted crate; these pin the composition, so they live with the
//! crate that composes it.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::str::FromStr as _;

use plakous::sessions::{SessionLocator, SessionOperationId};
use plakous::subscriptions::NativeProvider;

use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

use plakous::sessions::ForkSessionOutcome;
use plakous_hosted::agent::*;
use plakous_hosted::agent_tree::MemoryTreeStore;
use plakous_hosted::agent_tree_tools::TreeDriver;
use plakous_hosted::child_prompts::ChildAssignmentStartKind;

use crate::home::paths::PlakPaths;
use crate::telemetry_sink::TelemetryWriter;
use plakous_hosted::agent::build::{invoke_topology_tool_on_hub, spawn_child_composition};
use plakous_hosted::agent::children::{
    ChildBoundBind, finalize_topology_child_turn, retire_live_child, spawn_child_start_composition,
    spawned_child_spec,
};
use plakous_hosted::binding::{CompositionSelection, ResolvedProtocol};

/// A driver over an in-memory store for the turn-finalisation tests that
/// hold the bare tree `Arc`.
fn driver_over(tree: &Arc<Mutex<plakous_hosted::agent_tree::ProductAgentTree>>) -> TreeDriver {
    let vcs = tree.lock().expect("lock").vcs_port();
    TreeDriver::new(
        Arc::clone(tree),
        Arc::new(MemoryTreeStore::new()),
        vcs,
        None,
    )
}

fn openrouter_test_parent() -> ApiTopologyParent {
    ApiTopologyParent {
        created_output_bound: None,
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        product_home: None,
        tool_selection: plakous_hosted::tool_plan::NodeToolSelection::default(),
        workspace_root: PathBuf::from("/tmp/or-root"),
        cwd: PathBuf::from("/tmp/or-root"),
        plan_state_authority: "tree-or".into(),
        composition: CompositionSelection::Direct,
        topology_tree_id: Some("tree-or".into()),
        topology_caller_id: None,
        state_roots: test_state_roots(PathBuf::from("/tmp/plak"), PathBuf::from("/tmp/state")),
        model: "vendor/parent".into(),
        effort: None,
        target: plakous_hosted::binding::ApiTarget::OpenRouter {
            wire: ResolvedProtocol::Chat,
        },
        scheduled_compaction: true,
        max_model_rounds: NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: true,
    }
}

/// State roots over Plak's own implementations, for the tests that read its
/// authorities back.
fn test_state_roots(product: PathBuf, _state_home: PathBuf) -> ProductStateRoots {
    crate::shell::host::product_state_roots(&PlakPaths::from_home(product))
}

/// C8: an unreadable `config.toml` must not become a frozen empty roster.
#[test]
fn genesis_refuses_an_unreadable_config_and_defaults_only_when_absent() {
    use plakous_hosted::agent::RosterSource as _;
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");

    // No file: the schema defaults are the operator's configuration.
    assert_eq!(
        crate::roster_input::genesis_orchestration_config(Some(&paths))
            .expect("absent config is not a failure"),
        crate::product_config::ProductConfig::default_product()
    );

    std::fs::write(paths.config_file(), "[agent.broken\naccount = \"claude\"\n").expect("write");
    let error = crate::roster_input::genesis_orchestration_config(Some(&paths))
        .expect_err("an unparseable config.toml must not freeze an epoch");
    assert!(
        error.message().contains("refusing to freeze a tree epoch"),
        "{}",
        error.message()
    );

    let source = crate::roster_input::ProductRosterSource::new(Some(paths.clone()));
    assert!(
        source
            .topology_admitted()
            .unwrap_err()
            .message()
            .contains("config.toml")
    );

    // No product home at all keeps the defaults: there is no file to fail on.
    assert!(crate::roster_input::genesis_orchestration_config(None).is_ok());
}

/// `native-ptc` on an OpenRouter card is a wire-vocabulary
/// refusal the tree makes, so the binder-less JSONL `invoke_tool` on the hub
/// — no rejoin spec, `(None, None, None)` — refuses the same card a product
/// binder would.
#[tokio::test]
async fn invoke_tool_spawn_on_the_hub_refuses_openrouter_native_ptc_without_a_binder() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    use plakous_hosted::orchestration::RosterBinding;

    let root = tempfile::tempdir().expect("tempdir");
    let tree_id = format!("hub-{}", uuid::Uuid::new_v4().simple());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &tree_id,
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    tree.pin_bind_hints_for_test(crate::roster_input::product_bind_hints());
    let mut router = RosterBinding::leaf("openrouter", "vendor/model");
    router.composition = Some(CompositionSelection::NativePtc.into());
    tree.pin_template("review", router);
    let runtime =
        plakous_hosted::topology_runtime::insert_root(tree_id.clone(), tree, None).expect("hub");
    assert!(runtime.rejoin_spec().is_none(), "no binder on this hub");
    let receipt = invoke_topology_tool_on_hub(
        &tree_id,
        None,
        "spawn_agent",
        &serde_json::to_value(SpawnAgentRequest::test_review()).expect("request"),
        "op-native-ptc",
    )
    .await
    .expect("receipt");
    assert!(!receipt.succeeded, "{}", receipt.detail);
    assert!(
        receipt.detail.contains("schema_mismatch")
            && receipt
                .detail
                .contains("template composition must be direct|js-ptc|py-ptc for this account"),
        "{}",
        receipt.detail
    );
    plakous_hosted::topology_runtime::force_release(&tree_id).await;
}

#[test]
fn telemetry_unavailable_is_logged_once_per_process() {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");
    std::fs::write(paths.telemetry_database(), b"not sqlite").expect("bad telemetry db");
    let state_roots = crate::shell::host::product_state_roots(&paths);
    let session_id = "11111111-2222-3333-4444-555555555555";
    assert!(
        state_roots.telemetry_emitter().is_some(),
        "retry logging remains available"
    );
    assert!(matches!(
        state_roots.telemetry_health(),
        TelemetryHealth::Unavailable(_)
    ));
    state_roots.log_telemetry_unavailable_once(session_id);
    state_roots.log_telemetry_unavailable_once("22222222-3333-4444-5555-666666666666");
    let events = plakous_hosted::session_log::read_tail(&paths.session_home(), session_id, 4)
        .expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|line| line.contains("telemetry.unavailable"))
            .count(),
        1
    );
}

/// Retiring a finished child releases its session and advances the
/// node's locator to the released head, so the next `followup` reopens the
/// branch head instead of the spawn-time locator (`StaleLocator`).
#[tokio::test]
async fn retiring_a_live_child_advances_its_session_locator() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

    let temporary = PlakPaths::isolated_tempdir();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_roots = test_state_roots(
        temporary.path().to_path_buf(),
        temporary.path().join("state"),
    );
    let sessions = state_roots.runtime_sessions().expect("runtime sessions");
    let controller = BoundaryController::from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        temporary.path().join("physical.jsonl"),
    )
    .expect("offline facade controller");
    let created = build_or_open_bound_session(
        Arc::clone(&sessions),
        None,
        None,
        SessionOperationId::new("plak:test:retire-create").expect("create operation"),
        runtime_binding(&workspace, controller).expect("create runtime"),
    )
    .await
    .unwrap_or_else(|error| panic!("durable product create: {}", error.failure.message()));
    let spawn_locator = created.locator();
    let tree = Arc::new(Mutex::new(
        ProductAgentTree::bootstrap_root_for_test(
            uuid::Uuid::new_v4().to_string(),
            "root",
            "owner",
            workspace.clone(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .expect("tree"),
    ));
    {
        let mut tree = tree.lock().expect("lock");
        tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .expect("spawn");
        tree.bind_session_locator("c1", spawn_locator.to_string())
            .expect("bind");
    }
    let turn = created
        .agent()
        .expect("created product session remains attached")
        .prompt("one child turn")
        .await
        .expect("turn admission");
    let turn_id = turn.id();
    let control = turn.control();
    turn.result().await.expect("terminal turn result");
    let live = Arc::new(tokio::sync::Mutex::new(
        plakous_hosted::topology_runtime::LiveChildTable::default(),
    ));
    live.lock().await.insert_for_test(
        "c1".to_owned(),
        Box::new(live::ProductLiveChild::new(created, turn_id, control))
            as Box<dyn plakous_hosted::topology_runtime::LiveChildHandle>,
    );
    retire_live_child(&driver_over(&tree), &live, "c1", turn_id).await;
    assert!(live.lock().await.is_empty());
    let bound = tree
        .lock()
        .expect("lock")
        .node("c1")
        .expect("node")
        .session_locator
        .clone()
        .expect("locator stays bound");
    assert_ne!(
        bound,
        spawn_locator.to_string(),
        "the turn moved the branch head; the node must follow it"
    );
    // The advanced locator is the head: an exact reopen of it succeeds.
    let reopened = build_or_open_bound_session(
        Arc::clone(&sessions),
        Some(SessionLocator::from_str(&bound).expect("locator parses")),
        None,
        SessionOperationId::new("plak:test:retire-reopen").expect("open operation"),
        runtime_binding(
            &workspace,
            BoundaryController::from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                temporary.path().join("physical-2.jsonl"),
            )
            .expect("controller"),
        )
        .expect("reopen runtime"),
    )
    .await
    .unwrap_or_else(|error| panic!("reopen at the advanced head: {}", error.failure.message()));
    reopened.release().await.expect("release");
}

#[tokio::test]
async fn product_session_facade_create_turn_shutdown_and_reopen() {
    let temporary = PlakPaths::isolated_tempdir();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_roots = test_state_roots(
        temporary.path().to_path_buf(),
        temporary.path().join("state"),
    );
    let sessions = state_roots.runtime_sessions().expect("runtime sessions");
    let controller = BoundaryController::from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        temporary.path().join("physical.jsonl"),
    )
    .expect("offline facade controller");
    let created = build_or_open_bound_session(
        Arc::clone(&sessions),
        None,
        None,
        SessionOperationId::new("plak:test:durable-create").expect("create operation"),
        runtime_binding(&workspace, controller.clone()).expect("create runtime"),
    )
    .await
    .unwrap_or_else(|error| panic!("durable product create: {}", error.failure.message()));
    created
        .agent()
        .expect("created product session remains attached")
        .prompt("exercise durable product resume")
        .await
        .expect("turn admission")
        .result()
        .await
        .expect("terminal turn result");
    let locator = created.release().await.expect("durable shutdown");

    let mut reopened = build_or_open_bound_session(
        Arc::clone(&sessions),
        Some(locator),
        None,
        SessionOperationId::new("plak:test:durable-open").expect("open operation"),
        runtime_binding(&workspace, controller.clone()).expect("reopen runtime"),
    )
    .await
    .unwrap_or_else(|error| panic!("exact durable product reopen: {}", error.failure.message()));
    assert_eq!(reopened.locator(), locator);
    let detached = reopened.detach().await.expect("reopened detach");
    assert_eq!(detached, locator);
    let forked = reopened
        .fork_from(
            SessionOperationId::new("plak:test:detached-fork").expect("fork operation"),
            detached,
        )
        .await
        .expect("detached canonical fork");
    assert!(matches!(
        forked,
        ForkSessionOutcome::Forked { source, .. } if source == detached
    ));
    drop(reopened);

    let restored = build_or_open_bound_session(
        Arc::clone(&sessions),
        Some(detached),
        None,
        SessionOperationId::new("plak:test:restore-source").expect("restore operation"),
        runtime_binding(&workspace, controller).expect("restore runtime"),
    )
    .await
    .unwrap_or_else(|error| panic!("exact detached source restore: {}", error.failure.message()));
    restored.release().await.expect("restored shutdown");
}

/// The seeded config shape — a `native-ptc` root template (`luna`)
/// with a Claude template in the working set that omits `composition`. The
/// review child must spawn on its own presentation (plakous-default → Direct
/// on Claude), never inherit the root's Code Mode prefix.
///
#[test]
fn native_ptc_root_spawns_a_claude_working_set_review_child() {
    let mut cfg = crate::product_config::ProductConfig::default_product();
    cfg.templates.insert(
        "luna".to_owned(),
        crate::product_config::AgentTemplate {
            account: "codex".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            effort: Some("low".to_owned()),
            composition: Some(CompositionSelection::NativePtc),
            ..crate::product_config::AgentTemplate::default()
        },
    );
    cfg.templates.insert(
        "claude".to_owned(),
        crate::product_config::AgentTemplate {
            account: "claude".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            effort: Some("low".to_owned()),
            description: "Claude reviewer".to_owned(),
            ..crate::product_config::AgentTemplate::default()
        },
    );
    cfg.root_agent = Some("luna".to_owned());
    cfg.working_set =
        plakous_hosted::agent_tree_protocol::WorkingSet::parse(["claude"]).expect("set");
    cfg.validate().expect("the seeded shape validates");
    let root = cfg.root_template().expect("[root] agent = luna");
    assert_eq!(
        root.composition_or_default(),
        CompositionSelection::NativePtc
    );
    let review = cfg
        .working_set_template("claude")
        .expect("working_set names claude");
    assert_eq!(review.composition, None);

    // The class pin resolves the template's own default, so the spawn sees
    // `Some(FollowCatalog)` rather than the "unpinned" `None`.
    let pinned = Some(review.composition_or_default());
    assert_eq!(pinned, Some(CompositionSelection::FollowCatalog));
    assert_eq!(
        spawn_child_composition(
            CompositionSelection::NativePtc,
            NativeProvider::Claude,
            pinned,
        )
        .expect("a Claude review child of a native-ptc root spawns"),
        CompositionSelection::FollowCatalog
    );
}

#[test]
fn cancelled_running_child_skips_detach_from_finalize() {
    use crate::telemetry_store::TelemetryItem;
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::SpawnAgentRequest;
    use plakous_hosted::agent_tree_protocol::WriterMode;
    use plakous_hosted::product_observation::ProductObservation;
    use std::sync::mpsc::TryRecvError;

    let root = tempfile::tempdir().expect("tempdir");
    let tree_id = uuid::Uuid::new_v4().to_string();
    let tree = Arc::new(Mutex::new(
        ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .expect("tree"),
    ));
    let child = {
        let mut tree = tree.lock().expect("lock");
        let spawned = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .expect("spawn");
        tree.bind_session_locator(&spawned.agent_id, "sess/child")
            .expect("bind");
        spawned.agent_id
    };
    let (writer, receiver) = TelemetryWriter::detached(4);
    let observer =
        crate::telemetry::topology::ProductTopologyObserver::port(Some(Arc::clone(&writer)))
            .expect("a writer yields the product observer");
    finalize_topology_child_turn(&driver_over(&tree), &child, true, false, Some(&observer));
    let mut detached = 0_u32;
    loop {
        match receiver.try_recv() {
            Ok(TelemetryItem::Product(ProductObservation::TreeNodeDetached { .. })) => {
                detached += 1;
            }
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    assert_eq!(detached, 0);
}

/// The turn waiter persists through the driver it captured at spawn: a root
/// that quit has released the hub entry, and the child's failure and seal
/// still reach the store.
#[tokio::test]
async fn child_seal_persists_after_the_root_released_the_hub() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{
        AttemptStatus, NodeStatus, SpawnAgentRequest, Tier, WriterMode,
    };
    use plakous_hosted::occupancy_vcs::VcsOps;
    use plakous_hosted::occupancy_vcs::fake::FakeVcs;
    use plakous_hosted::orchestration::RosterBinding;

    let (_home, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");
    let root = tempfile::tempdir().expect("tempdir");
    let tree_id = format!("hub-{}", uuid::Uuid::new_v4().simple());
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&vcs) as Arc<dyn VcsOps>,
        &tree_id,
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let store = crate::store::tree::tree_store(&paths).expect("store");
    tree.acknowledge_revision(store.save(&tree).expect("genesis"));
    let runtime = plakous_hosted::topology_runtime::insert_root_with(
        tree_id.clone(),
        tree,
        Arc::clone(&store),
        None,
    )
    .expect("hub");
    let driver = runtime.driver().clone();
    let child = {
        let mut tree = runtime.tree_handle_for_test().lock().expect("lock");
        let spawned = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "impl", "edit")
                    .with_fork(),
                None,
            )
            .expect("spawn");
        tree.bind_session_locator(&spawned.agent_id, "sess/child")
            .expect("bind");
        tree.set_active_turn(&spawned.agent_id, true)
            .expect("active");
        driver.save(&mut tree).expect("publish");
        spawned.agent_id
    };
    drop(runtime);
    // The root quits: the hub entry is gone while the child turn unwinds.
    plakous_hosted::topology_runtime::force_release(&tree_id).await;
    assert!(plakous_hosted::topology_runtime::lookup(&tree_id).is_none());
    // The waiter's `CancelledOutside` arm, on the driver it captured.
    let pending = {
        let mut tree = driver.tree_handle_for_test().lock().expect("lock");
        tree.record_fault(
            &child,
            "the turn was cancelled outside the host; nothing interrupted it",
        );
        tree.record_host_cancellation(&child)
            .and_then(|value| tree.fold_due(&child).map(|_| value))
            .expect("fail");
        assert!(children::persist_or_fault(&driver, &mut tree, &child));
        tree.take_effects()
    };
    assert!(
        !pending.is_empty(),
        "a failed mutate copy seals its partial work"
    );
    children::settle_or_fault(&driver, pending, &child);
    let persisted = store.load(&tree_id).expect("load").expect("rows");
    let rows = persisted.rows();
    assert_eq!(rows.node(&child).expect("node").status, NodeStatus::Failed);
    let attempt = rows.attempt(&child).expect("attempt");
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert!(attempt.candidate_id.is_some(), "the seal reached the store");
    assert!(plakous_hosted::topology_runtime::lookup(&tree_id).is_none());
}

/// The mail-delivered assignment Start reads the composition off the node's
/// own bound row. It once read a full `spawned_child_spec`
/// instead, and every OpenRouter child's followup was refused with "spawn
/// child account is not a supported native OAuth account".
#[tokio::test]
async fn mail_start_composition_of_an_openrouter_child_never_takes_the_oauth_spec() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    use plakous_hosted::orchestration::RosterBinding;

    let root = tempfile::tempdir().expect("tempdir");
    let tree_id = format!("hub-{}", uuid::Uuid::new_v4().simple());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &tree_id,
        "root",
        "owner",
        root.path().to_path_buf(),
        "vendor/parent",
        WriterMode::SingleLease,
    )
    .expect("tree");
    tree.pin_bind_hints_for_test(crate::roster_input::product_bind_hints());
    let mut binding = RosterBinding::leaf("openrouter", "vendor/child");
    binding.composition = Some(CompositionSelection::Direct.into());
    tree.pin_template("review", binding);
    let spawned = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("spawn");
    let agent_id = spawned.agent_id.clone();
    let bind = {
        let node = tree.node(&agent_id).expect("node");
        assert_eq!(node.account, "openrouter");
        ChildBoundBind::of(node)
    };
    plakous_hosted::topology_runtime::insert_root(tree_id.clone(), tree, None).expect("hub");

    let mut openrouter = openrouter_test_parent();
    openrouter.topology_tree_id = Some(tree_id.clone());
    openrouter.plan_state_authority = tree_id.clone();
    let parent = SpawnParent::Api(openrouter);

    // This is the OAuth gate the live refusal hit.
    let oauth_spec = spawned_child_spec(
        &parent,
        &agent_id,
        root.path().to_str().expect("path"),
        bind.account.as_deref(),
        bind.tuple_id.as_deref(),
        bind.effort.as_deref(),
    )
    .await
    .err()
    .expect("the OAuth spec builder cannot bind an OpenRouter account");
    assert!(
        oauth_spec.contains("not a supported native OAuth account"),
        "{oauth_spec}"
    );

    // The exact call the mail Start used to make: `(None, None, None)` reads
    // the account back off the node and lands on the same gate.
    let live_shape = spawned_child_spec(
        &parent,
        &agent_id,
        root.path().to_str().expect("path"),
        None,
        None,
        None,
    )
    .await
    .err()
    .expect("the OAuth spec builder cannot bind an OpenRouter account");
    assert!(
        live_shape.contains("not a supported native OAuth account"),
        "{live_shape}"
    );

    let composition = spawn_child_start_composition(&parent, &agent_id, bind.account.as_deref())
        .expect("the bound row composes the Start prompt");
    assert_eq!(composition, CompositionSelection::Direct);
    assert_eq!(
        ChildAssignmentStartKind::from_composition(
            plakous_hosted::prompt_inputs::prompt_composition(composition)
        ),
        ChildAssignmentStartKind::Direct
    );
    plakous_hosted::topology_runtime::force_release(&tree_id).await;
}
