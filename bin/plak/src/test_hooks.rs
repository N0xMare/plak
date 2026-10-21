//! The offline model selector the repository gate drives the real binary with.
//!
//! This module exists only under the `test-hooks` feature. It is not in the
//! default feature set, it is not enabled by any release build, and nothing in
//! the shipped product references it. Its whole job is to let a spawned `plak`
//! process bind a deterministic in-process model instead of a provider, so the
//! gate can drive complete lifecycles of the CLI face, the agent JSONL face and
//! the terminal application without a network or a credential.
//!
//! Two environment variables select it, and both must be present:
//!
//! * `PLAK_TEST_MODEL` must be exactly [`SELECTOR`]. Naming the model rather
//!   than a bare boolean keeps the selector an explicit binding fact.
//! * `PLAK_TEST_SCRIPT` names the JSON script document
//!   (`plakous_offline_crash_harness::script`) the model replays.
//!
//! `PLAK_TEST_EVIDENCE` optionally names the fsynced provider/effect evidence
//! log; it defaults to a file beside the script. `PLAK_TEST_TUI_PILOT`
//! independently names the owner-only semantic TUI pilot directory.
//!
//! Anything wrong with either variable is a hard refusal. A test hook that
//! silently falls back to the real provider would let a gate run pass while
//! testing nothing.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::script::ScriptPlan;
use plakous_offline_crash_harness::{
    BoundaryController, scripted_artifact_runtime_binding,
    scripted_code_mode_runtime_binding_with_extensions, scripted_compaction_runtime_binding,
    scripted_runtime_binding_with_extensions, scripted_topology_runtime_binding,
};

use crate::{product_agent, product_config, product_host, product_paths, product_sessions};

/// The exact model id this hook answers to.
pub(crate) const SELECTOR: &str = "offline-loopback-v1";

pub(crate) fn assistant_done_release() -> Option<PathBuf> {
    if std::env::var("PLAK_TEST_MODEL").as_deref() != Ok(SELECTOR) {
        return None;
    }
    std::env::var_os("PLAK_TEST_ASSISTANT_DONE_RELEASE").map(PathBuf::from)
}

/// The topology registrations one scripted tree exposes as its whole registry.
///
/// The binder is shared across every runtime this selection builds. A fresh
/// binder per runtime would restart the child ordinal, and a failure or hold
/// dial would land on the wrong child.
fn tree_registrations(
    driver: &plakous_hosted::agent_tree_tools::TreeDriver,
    binder: Arc<ScriptedChildBinder>,
) -> Result<Vec<plakous::tools::ToolRegistration>, String> {
    let caller_id = driver
        .tree_handle_for_test()
        .lock()
        .map_err(|_| "offline scripted tree lock".to_owned())?
        .root_agent_id
        .clone();
    plakous_hosted::agent_tree_tools::topology_tool_registrations_on(
        driver.clone(),
        caller_id,
        plakous_hosted::agent_tree_tools::TopologyHostPorts {
            binder: Some(binder),
            ..Default::default()
        },
    )
    .map_err(|error| error.message().to_owned())
}

/// One scripted tree and the child binder every runtime of that selection shares.
struct PreparedTree {
    driver: plakous_hosted::agent_tree_tools::TreeDriver,
    binder: Arc<ScriptedChildBinder>,
}

/// Builds the one tree a `tree` script's runtimes all compile against, and
/// attaches it to the process hub.
///
/// Two separate things have to be true for a scripted tree to be usable, and
/// the first one alone is not enough:
///
/// * the topology **tools** must be the session's registry, which is what the
///   `tree` dial does, so the model can call `spawn_agent` at all;
/// * the tree must be in the **process hub** under the id the navigator
///   derives for this session, or every navigator intent answers "agent tree
///   navigator unavailable: no topology hub is attached"
///   (`topology_navigator.rs`), and the terminal's tree canvas and node
///   transcript overlay stay dark.
///
/// `tree_id` is the session's own id: a root session's topology tree id is its
/// plan-state authority, which is the session id. A child session would carry
/// the root's instead, but a scripted selection is always a root.
///
/// The hub is a process-wide registry keyed by tree id, so this is created once
/// per selection and shared: a fresh tree per runtime would give each role its
/// own root, and a child spawned through one would be invisible to the next.
///
/// `durable_tree` publishes through the product `subagents.sqlite3`. The
/// in-memory store is what a canvas test needs and nothing else: `plak session
/// show` opens the sqlite file, so a child left only in memory is a quiet
/// absence after the process exits.
fn prepare_scripted_tree(
    paths: &product_paths::PlakPaths,
    workspace: &Path,
    tree_id: &str,
    plan: &ScriptPlan,
    sessions: Arc<dyn plakous_hosted::session_store::SessionRecordStore>,
    state_roots: product_agent::ProductStateRoots,
) -> Result<PreparedTree, String> {
    use plakous_hosted::agent_tree::{FakeVcs, MemoryTreeStore, ProductAgentTree, TreeStore};
    use plakous_hosted::agent_tree_protocol::WriterMode;
    use plakous_hosted::occupancy_vcs::VcsOps;

    let vcs: Arc<dyn VcsOps> = Arc::new(FakeVcs::new());
    // `plak session show` looks the tree up by the product session id. The
    // in-memory canvas uses the hub key and can keep a private tree id; a
    // durable publish has to use the session id or show reports no tree.
    let bootstrap_id = if plan.durable_tree {
        tree_id
    } else {
        "offline-scripted-tree"
    };
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&vcs),
        bootstrap_id,
        "root",
        "owner",
        workspace.to_path_buf(),
        SELECTOR,
        WriterMode::SingleLease,
    )
    .map_err(|error| format!("offline scripted tree: {}", error.message))?;
    let inner: Arc<dyn TreeStore> = if plan.durable_tree {
        paths
            .ensure_layout()
            .map_err(|error| format!("offline scripted tree layout: {error}"))?;
        Arc::new(
            plakous_recursive_subagents_store::SqliteTreeStore::open(&paths.subagents_database())
                .map_err(|error| format!("offline scripted tree store: {}", error.message()))?,
        )
    } else {
        Arc::new(MemoryTreeStore::new())
    };
    let store: Arc<dyn TreeStore> = if plan.disk_full_at_child_locator {
        Arc::new(ChildLocatorDiskFullStore::new(inner))
    } else {
        inner
    };
    let revision = store
        .save(&tree)
        .map_err(|error| format!("offline scripted tree could not be saved: {}", error.detail))?;
    tree.acknowledge_revision(revision);
    drop(vcs);
    // Attach rather than construct: `insert_root_with` puts the tree in the
    // process hub *and* hands back the runtime that owns it, so the navigator
    // and the tools are looking at one tree rather than two copies of one.
    // Building a `TreeDriver` directly would leave the hub empty.
    let runtime =
        plakous_hosted::topology_runtime::insert_root_with(tree_id, tree, Arc::clone(&store), None)
            .map_err(|error| format!("offline scripted tree could not attach: {error}"))?;
    let driver = runtime.driver().clone();
    let binder = Arc::new(ScriptedChildBinder {
        tree: Arc::clone(driver.tree_handle_for_test()),
        store,
        sessions,
        session_id: tree_id.to_owned(),
        child_hold_ms: plan.child_hold_ms,
        child_hold_after: plan.child_hold_after,
        fail_child_after_successes: plan.fail_child_after_successes,
        durable: plan.durable_tree,
        ordinal: AtomicU32::new(0),
        child_process: plan.child_process,
        child_process_quiescent: plan.child_process_quiescent,
        state_roots,
    });
    Ok(PreparedTree { driver, binder })
}

/// Fails the first bound child locator publish and every save after it.
/// Prepares and the committed child row land independently of projector saves.
struct ChildLocatorDiskFullStore {
    inner: Arc<dyn plakous_hosted::agent_tree::TreeStore>,
    failed: AtomicBool,
}

impl ChildLocatorDiskFullStore {
    fn new(inner: Arc<dyn plakous_hosted::agent_tree::TreeStore>) -> Self {
        Self {
            inner,
            failed: AtomicBool::new(false),
        }
    }
}

impl plakous_hosted::agent_tree::TreeStore for ChildLocatorDiskFullStore {
    fn exists(&self, tree_id: &str) -> Result<bool, plakous_hosted::agent_tree::TreeStoreError> {
        self.inner.exists(tree_id)
    }

    fn ids(&self) -> Result<Vec<String>, plakous_hosted::agent_tree::TreeStoreError> {
        self.inner.ids()
    }

    fn save(
        &self,
        tree: &plakous_hosted::agent_tree::ProductAgentTree,
    ) -> Result<plakous_hosted::agent_tree::Revision, plakous_hosted::agent_tree::TreeStoreError>
    {
        if tree
            .nodes()
            .values()
            .any(|node| node.parent_agent_id.is_some() && node.session_locator.is_some())
        {
            self.failed.store(true, Ordering::Release);
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(plakous_hosted::agent_tree::TreeStoreError::unavailable(
                "could not durably commit agent tree: disk_full",
            ));
        }
        self.inner.save(tree)
    }

    fn load(
        &self,
        tree_id: &str,
    ) -> Result<
        Option<plakous_hosted::agent_tree::PersistedTree>,
        plakous_hosted::agent_tree::TreeStoreError,
    > {
        self.inner.load(tree_id)
    }
}

/// Stands in for the product's child session on a scripted tree.
///
/// The spawn tool records the bind after this future resolves, so a child's
/// turn may only be completed once the locator is on the node. Completing it
/// earlier is undone by the bind itself, which reads as a child that ran and
/// then un-ran.
struct ScriptedChildBinder {
    tree: Arc<std::sync::Mutex<plakous_hosted::agent_tree::ProductAgentTree>>,
    store: Arc<dyn plakous_hosted::agent_tree::TreeStore>,
    sessions: Arc<dyn plakous_hosted::session_store::SessionRecordStore>,
    session_id: String,
    child_hold_ms: u64,
    child_hold_after: u32,
    fail_child_after_successes: Option<u32>,
    /// Publish a child's completion, and borrow the root locator, only when
    /// the tree is the product sqlite file. Memory-store scripts keep the
    /// historical binder: no extra save, and the placeholder locator.
    durable: bool,
    ordinal: AtomicU32,
    child_process: bool,
    child_process_quiescent: bool,
    state_roots: product_agent::ProductStateRoots,
}

impl ScriptedChildBinder {
    /// The root session's own locator, when the product has bound one.
    ///
    /// A scripted child has no plakous session of its own. Pointing the node
    /// at the root locator lets `plak session show` inspect a real checkpoint.
    /// The node's status is still the tree's, which is the fact the durability
    /// scenarios assert. A locator that does not parse makes every show exit
    /// 5, including a child that completed, and that exit cannot be the oracle.
    fn root_locator(&self) -> Option<String> {
        self.sessions
            .load(&self.session_id)
            .ok()
            .and_then(|record| record.plakous_locator.map(|locator| locator.to_string()))
    }

    fn publish(
        tree: &mut plakous_hosted::agent_tree::ProductAgentTree,
        store: &dyn plakous_hosted::agent_tree::TreeStore,
    ) {
        if let Ok(revision) = store.save(tree) {
            tree.acknowledge_revision(revision);
        }
    }
}

impl plakous_hosted::agent_tree_tools::SpawnedSessionBinder for ScriptedChildBinder {
    fn bind(
        &self,
        start: plakous_hosted::agent_tree_tools::SpawnStart,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        plakous_hosted::agent_tree_tools::SpawnBound,
                        plakous_hosted::agent_tree_tools::SpawnBindError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let ordinal = self.ordinal.fetch_add(1, Ordering::Relaxed);
        if self
            .fail_child_after_successes
            .is_some_and(|after| ordinal >= after)
        {
            return Box::pin(async {
                Err(
                    plakous_hosted::agent_tree_tools::SpawnBindError::RecoveryRequired {
                        stage: plakous_hosted::topology_runtime::ChildStartupStage::Starting,
                        message: "scripted_child_failure".to_owned(),
                    },
                )
            });
        }
        let tree = Arc::clone(&self.tree);
        let store = Arc::clone(&self.store);
        let publish = self.durable;
        let agent_id = start.spawned.agent_id.clone();
        if self.child_process {
            let quiescent = self.child_process_quiescent;
            let runtime = plakous_hosted::topology_runtime::lookup(&self.session_id);
            let roots = self.state_roots.clone();
            let path = self
                .tree
                .lock()
                .ok()
                .and_then(|tree| tree.node(&agent_id).ok().map(|node| node.path.clone()));
            return Box::pin(async move {
                bind_process_child(runtime, roots, agent_id, path, quiescent)
                    .await
                    .map_err(|message| {
                        plakous_hosted::agent_tree_tools::SpawnBindError::RecoveryRequired {
                            stage: plakous_hosted::topology_runtime::ChildStartupStage::Starting,
                            message,
                        }
                    })
            });
        }
        let hold_ms = if ordinal >= self.child_hold_after {
            self.child_hold_ms
        } else {
            0
        };
        let locator = if self.durable {
            self.root_locator()
        } else {
            None
        }
        .unwrap_or_else(|| "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".to_owned());
        Box::pin(async move {
            tokio::spawn(async move {
                for _ in 0..600 {
                    let ready = {
                        let Ok(tree) = tree.lock() else {
                            return;
                        };
                        tree.node(&agent_id)
                            .is_ok_and(|node| node.session_locator.is_some())
                    };
                    if !ready {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        continue;
                    }
                    if hold_ms > 0 {
                        if let Ok(tree) = tree.lock()
                            && let Ok(node) = tree.node(&agent_id)
                        {
                            std::fs::write(node.path.join("holding.txt"), "holding\n").ok();
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                    }
                    let Ok(mut tree) = tree.lock() else {
                        return;
                    };
                    if let Ok(node) = tree.node(&agent_id) {
                        std::fs::write(node.path.join("child.txt"), "child\n").ok();
                    }
                    let turn_token = tree
                        .attempt(&agent_id)
                        .map_or(1, |attempt| attempt.turn_token);
                    let submitted = tree.submit_result(
                        &agent_id,
                        &plakous_hosted::agent_tree_protocol::SubmitResultRequest {
                            turn_token,
                            output_json: "{\"summary\":\"offline scripted child done\"}".to_owned(),
                        },
                        None,
                    );
                    if submitted.is_ok() {
                        tree.record_turn_completed(&agent_id).ok();
                    }
                    tree.settle_for_test();
                    if publish {
                        Self::publish(&mut tree, store.as_ref());
                    }
                    return;
                }
            });
            Ok(plakous_hosted::agent_tree_tools::SpawnBound {
                durably_registered: false,
                session_locator: locator,
                turn_id: None,
            })
        })
    }
}

struct ProcessChild {
    sessions: Arc<plakous::sessions::AgentSessions>,
    session: plakous::sessions::OpenedSession,
    control: plakous::agent::TurnControl,
    turn_id: plakous::model::TurnId,
}

impl plakous_hosted::topology_runtime::LiveChildHandle for ProcessChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        Some(self.session.agent().clone())
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        self.turn_id
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        self.control.clone()
    }
    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Err("offline held child does not accept follow-ups".into()) })
    }

    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        None
    }
    fn restart(
        &mut self,
        _: plakous_hosted::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> Pin<Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>>
    {
        Box::pin(async { Err("offline held child does not restart".into()) })
    }
    fn release(
        self: Box<Self>,
        _driver: plakous_hosted::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = plakous_hosted::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async move {
            match self.session.release_for_transfer().await {
                Ok(locator) => {
                    plakous_hosted::runtime::test_kit::child_release_with_feed(
                        &self.sessions,
                        locator,
                        None,
                    )
                    .await
                }
                Err(error) => plakous_hosted::topology_runtime::LiveRelease {
                    feed: None,
                    locator: error.released_locator().map(|locator| locator.to_string()),
                    failure: Some(error.to_string()),
                },
            }
        })
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: plakous_hosted::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = plakous_hosted::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async move {
            let failure = if self.control.is_settled() {
                None
            } else {
                self.control
                    .cancel()
                    .await
                    .err()
                    .map(|error| error.to_string())
            };
            let mut released =
                plakous_hosted::topology_runtime::LiveChildHandle::release(self, driver).await;
            released.failure = failure.or(released.failure);
            released
        })
    }
}

fn process_child_script(quiescent: bool) -> Result<Arc<ScriptPlan>, String> {
    let final_step = if quiescent {
        serde_json::json!({"output":{"text":"child is waiting"}})
    } else {
        serde_json::json!({"stream":[{"text":"child is waiting"},{"stall":60_000}]})
    };
    // The sleep before the TERM trap makes a stop sent before `owned.pid`
    // exists lose the `stopped.txt` marker every time, not only under load.
    let plan = Arc::new(serde_json::from_value(serde_json::json!({
        "version": 1, "process_tools": true, "steps": [
            {"output":{"tool":{"name":"process_session_start","arguments":{
                "command":"sleep 1; trap 'printf stopped > stopped.txt; exit 0' TERM; printf '%s' \"$$\" > owned.pid; while :; do sleep 0.1; done",
                "workdir":".","login":false,"tty":false,"keep_stdin_open":false,
                "yield_time_ms":0,"max_output_bytes":8192
            }}},"finish":"tool_calls"},
            final_step
        ]
    })).map_err(|error| error.to_string())?);
    Ok(plan)
}

async fn bind_process_child(
    runtime: Option<Arc<plakous_hosted::topology_runtime::ProductTopologyRuntime>>,
    roots: product_agent::ProductStateRoots,
    agent_id: String,
    path: Option<PathBuf>,
    quiescent: bool,
) -> Result<plakous_hosted::agent_tree_tools::SpawnBound, String> {
    let runtime = runtime.ok_or("offline child has no topology runtime")?;
    let path = path.ok_or("offline child has no workspace")?;
    std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        path.join("child-evidence.jsonl"),
    )
    .map_err(|error| error.to_string())?;
    let plan = process_child_script(quiescent)?;
    let binding = plakous_offline_crash_harness::scripted_runtime_binding(&path, controller, plan)
        .map_err(|error| error.to_string())?
        .with_job_host(roots.test_job_host(agent_id.clone()));
    let sessions = roots
        .runtime_sessions()
        .map_err(|error| error.message().to_owned())?;
    let created = sessions
        .create(plakous::sessions::CreateSessionRequest::new(
            plakous::sessions::SessionOperationId::new(format!("offline-child:{agent_id}"))
                .map_err(|error| error.to_string())?,
            binding,
        ))
        .await
        .map_err(|error| error.to_string())?;
    let plakous::sessions::CreateSessionOutcome::Created { session, .. } = created else {
        return Err("offline child was not created".into());
    };
    let command_id = plakous::agent::CommandId::new(format!("offline-child:{agent_id}:start"))
        .map_err(|error| error.to_string())?;
    let prompt = "start a background process";
    runtime.mutate_then_emit(false, |tree| {
        tree.record_child_session(&agent_id, &session.locator().to_string())
            .map_err(|error| error.message)?;
        tree.record_command(plakous_hosted::agent_tree::AgentCommand::intent(
            command_id.as_str(),
            &agent_id,
            tree.attempt(&agent_id)
                .map(|attempt| attempt.attempt_id.clone()),
            plakous_hosted::agent_tree::AgentCommandKind::Start,
            prompt,
            Vec::new(),
        ))
        .map_err(|error| error.message)
    })?;
    let mut turn = session
        .agent()
        .start(command_id, prompt)
        .await
        .map_err(|error| error.to_string())?
        .into_turn()
        .ok_or("offline child start produced no turn")?;
    let turn_id = turn.id();
    let control = turn.control();
    let mut events = turn.take_events().map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.map_err(|error| error.to_string())?;
            if matches!(
                event.kind(),
                plakous::agent::AgentEventKind::ModelDispatchStarted
            ) && !session.agent().jobs().is_empty()
            {
                return Ok::<_, String>(());
            }
        }
    })
    .await
    .map_err(|_| "offline child did not promote its process".to_owned())??;
    if quiescent {
        turn.result().await.map_err(|error| error.to_string())?;
    }
    let locator = session.locator().to_string();
    let feed =
        plakous_hosted::runtime::test_kit::child_feed(&sessions, session.locator().session_id())
            .await?;
    runtime.mutate_then_emit(false, |tree| {
        feed.apply(tree, &agent_id)
            .map(|_| ())
            .map_err(|error| error.message)
    })?;
    // Retain a live attempt with a durable promotion, either between turns
    // or inside a cancellable model stream.
    runtime
        .try_occupy_live(
            agent_id,
            Box::new(ProcessChild {
                sessions,
                session,
                control,
                turn_id,
            }),
        )
        .await
        .map_err(|_| "offline child slot was already occupied".to_owned())?;
    Ok(plakous_hosted::agent_tree_tools::SpawnBound {
        durably_registered: true,
        session_locator: locator,
        turn_id: Some(turn_id.to_string()),
    })
}

/// The root's optional tools as the offline runtime binds them.
///
/// The scripted runtimes compile their own registries, so the product's
/// native construction, which binds the root's tool plan, never runs. This
/// binds the same plan through the product's own binder: from the session's
/// frozen tools on reopen, or from the `[root] agent` template a creation
/// freezes. It binds once, on first use, so a session without optional tools
/// starts no server and a session's runtimes share one binding.
struct RootTools {
    frozen: plakous_hosted::frozen_tools::FrozenRootTools,
    session: product_sessions::SessionRecord,
    state_roots: product_agent::ProductStateRoots,
    bound: std::sync::OnceLock<Result<Vec<plakous::tools::ToolRegistration>, String>>,
}

impl RootTools {
    fn new(
        paths: &product_paths::PlakPaths,
        state_roots: &product_agent::ProductStateRoots,
        session: &product_sessions::SessionRecord,
    ) -> Result<Self, String> {
        let frozen = match &session.frozen_tools {
            Some(frozen) => frozen.clone(),
            None => plakous_hosted::frozen_tools::FrozenRootTools::at_creation(
                product_config::SessionPolicy::load(paths)?.root_template,
                session.account.as_deref(),
                session.model.as_deref(),
            ),
        };
        Ok(Self {
            frozen,
            session: session.clone(),
            state_roots: state_roots.clone(),
            bound: std::sync::OnceLock::new(),
        })
    }

    fn registrations(&self) -> Result<Vec<plakous::tools::ToolRegistration>, String> {
        self.bound
            .get_or_init(|| {
                plakous_hosted::agent::injected_root_tool_registrations(
                    &self.frozen,
                    &self.session,
                    &self.state_roots,
                )
            })
            .clone()
    }
}

/// One selected offline script and the evidence log its boundaries append to.
pub(crate) struct OfflineSelection {
    plan: Arc<ScriptPlan>,
    evidence: PathBuf,
}

/// The selected offline script, when the process was started with one.
///
/// `None` means no hook was requested and the ordinary provider path owns the
/// launch. `Some(Err(..))` means a hook was requested and is unusable, which
/// must fail the launch rather than fall through.
/// Whether the offline test model is requested at all, without reading or
/// checking its script.
pub(crate) fn requested() -> bool {
    selector().is_some()
}

pub(crate) fn selected() -> Option<Result<OfflineSelection, String>> {
    Some(resolve(&selector()?))
}

fn selector() -> Option<std::ffi::OsString> {
    std::env::var_os("PLAK_TEST_MODEL")
}

pub(crate) fn tui_pilot_root() -> Option<std::ffi::OsString> {
    std::env::var_os("PLAK_TEST_TUI_PILOT")
}

fn resolve(model: &std::ffi::OsStr) -> Result<OfflineSelection, String> {
    if model != std::ffi::OsStr::new(SELECTOR) {
        return Err(format!(
            "PLAK_TEST_MODEL must name `{SELECTOR}` to select the offline test model"
        ));
    }
    let script = std::env::var_os("PLAK_TEST_SCRIPT")
        .ok_or_else(|| "PLAK_TEST_SCRIPT must name an offline script document".to_owned())?;
    let script = PathBuf::from(script);
    let plan = ScriptPlan::read(&script).map_err(|error| error.message().to_owned())?;
    let evidence = std::env::var_os("PLAK_TEST_EVIDENCE")
        .map_or_else(|| script.with_extension("evidence.jsonl"), PathBuf::from);
    Ok(OfflineSelection {
        plan: Arc::new(plan),
        evidence,
    })
}

/// Which face is asking for the host configuration.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum OfflineFace {
    /// The attached multi-turn faces: the terminal application and `plak agent`.
    Attached,
    /// The single-turn `plak run` face.
    OneShot,
}

impl OfflineSelection {
    /// The complete injected host configuration for one face.
    ///
    /// Every field outside the injected target is read from the same product
    /// sources the real path reads, so what the gate exercises is the product's
    /// own composition with one substituted model.
    pub(crate) fn host_config(
        &self,
        face: OfflineFace,
        paths: &product_paths::PlakPaths,
        state_roots: &product_agent::ProductStateRoots,
        session: product_sessions::SessionRecord,
        effort: ReasoningEffort,
        effort_explicit: bool,
        cwd: &Path,
        ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
        resume_publication: Option<Arc<AtomicBool>>,
    ) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, String> {
        let controller = if let Some(release) = std::env::var_os("PLAK_TEST_PROVIDER_RELEASE") {
            BoundaryController::quiet_gated_from_evidence_path(
                CrashBoundary::ProviderDispatchOne,
                &self.evidence,
                PathBuf::from(release),
            )
        } else {
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &self.evidence)
        }
        .map_err(|error| format!("offline evidence log: {error}"))?;
        let workspace = cwd.to_path_buf();
        let plan = Arc::clone(&self.plan);
        let root_tools = Arc::new(RootTools::new(paths, state_roots, &session)?);
        // One session, one script position. The host may build more than one
        // runtime for a session, and a per-binding cursor would restart the
        // script at step one each time, which is silent and would look like the
        // model repeating itself.
        let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
        // One tree per selection, not per runtime: a child spawned through one
        // runtime has to be visible to the next one the host builds, and a
        // fresh tree would give each its own root.
        let prepared = if plan.tree {
            Some(prepare_scripted_tree(
                paths,
                cwd,
                &session.id,
                &plan,
                state_roots.session_records(),
                state_roots.clone(),
            )?)
        } else {
            None
        };
        // The rebind target is metadata read off a binding; it shares the same
        // cursor as every other runtime this selection builds, so constructing
        // it cannot advance or strand the script.
        let target = if let Some(prepared) = prepared.as_ref() {
            scripted_topology_runtime_binding(
                cwd,
                controller.clone(),
                Arc::clone(&plan),
                Arc::clone(&cursor),
                tree_registrations(&prepared.driver, Arc::clone(&prepared.binder))?,
            )
        } else if plan.code_mode {
            let sidecar = crate::sidecars::js_host::locate().map_err(|reason| {
                format!("offline Code Mode needs the JavaScript sidecar: {reason}")
            })?;
            scripted_code_mode_runtime_binding_with_extensions(
                cwd,
                controller.clone(),
                Arc::clone(&plan),
                Arc::clone(&cursor),
                &sidecar,
                None,
                root_tools.registrations()?,
            )
        } else if plan.seal_family_artifacts {
            scripted_artifact_runtime_binding(cwd, controller.clone(), &plan)
        } else {
            scripted_runtime_binding_with_extensions(
                cwd,
                controller.clone(),
                Arc::clone(&plan),
                Arc::clone(&cursor),
                root_tools.registrations()?,
            )
        }
        .map_err(|error| error.to_string())?
        .target_binding();
        let runtime_controller = controller;
        let runtime_tree = prepared;
        let telemetry = state_roots.telemetry_emitter();
        let runtime = Arc::new(move |role: product_host::TestRuntimeRole| {
            if role == product_host::TestRuntimeRole::Compact {
                let tree_tools = runtime_tree
                    .as_ref()
                    .map(|prepared| {
                        tree_registrations(&prepared.driver, Arc::clone(&prepared.binder))
                    })
                    .transpose()?;
                let sidecar = if plan.code_mode && tree_tools.is_none() {
                    Some(crate::sidecars::js_host::locate().map_err(|reason| {
                        format!("offline Code Mode needs the JavaScript sidecar: {reason}")
                    })?)
                } else {
                    None
                };
                scripted_compaction_runtime_binding(
                    &workspace,
                    runtime_controller.clone(),
                    &plan,
                    tree_tools,
                    sidecar.as_deref(),
                    if runtime_tree.is_some() || plan.seal_family_artifacts {
                        Vec::new()
                    } else {
                        root_tools.registrations()?
                    },
                )
            } else if let Some(prepared) = runtime_tree.as_ref() {
                // The same driver every time, so a child spawned on one runtime
                // is still on the tree the next one compiles against.
                scripted_topology_runtime_binding(
                    &workspace,
                    runtime_controller.clone(),
                    Arc::clone(&plan),
                    Arc::clone(&cursor),
                    tree_registrations(&prepared.driver, Arc::clone(&prepared.binder))?,
                )
            } else if plan.code_mode {
                // The product's own sidecar discovery, so the test binds what
                // the product would bind rather than a path of its own.
                match crate::sidecars::js_host::locate() {
                    Ok(sidecar) => scripted_code_mode_runtime_binding_with_extensions(
                        &workspace,
                        runtime_controller.clone(),
                        Arc::clone(&plan),
                        Arc::clone(&cursor),
                        &sidecar,
                        telemetry.clone(),
                        root_tools.registrations()?,
                    ),
                    Err(reason) => {
                        return Err(format!(
                            "offline Code Mode needs the JavaScript sidecar: {reason}"
                        ));
                    }
                }
            } else if plan.seal_family_artifacts {
                scripted_artifact_runtime_binding(&workspace, runtime_controller.clone(), &plan)
            } else {
                scripted_runtime_binding_with_extensions(
                    &workspace,
                    runtime_controller.clone(),
                    Arc::clone(&plan),
                    Arc::clone(&cursor),
                    root_tools.registrations()?,
                )
            }
            .map_err(|error| error.to_string())
        });
        let injection = Arc::new(product_host::TestHostInjection {
            runtime,
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let policy = product_config::SessionPolicy::load(paths)?;
        Ok(product_host::HostConfig {
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: state_roots.session_records(),
            state_roots: state_roots.clone(),
            session,
            target: product_host::HostTargetAuthority::Injected {
                provider: NativeProvider::Codex,
            },
            model_id: SELECTOR.to_owned(),
            effort: Some(effort),
            effort_explicit,
            compaction_enabled: policy.compaction_enabled,
            max_model_rounds: policy.max_model_rounds,
            topology_enabled: policy.topology_enabled,
            auto_continue: policy.auto_continue,
            topology_admission: Some(product_config::topology_admission_probe(paths)),
            surface: match face {
                OfflineFace::Attached => Some(crate::shell::settings::ProductSurface::boxed(paths)),
                OfflineFace::OneShot => None,
            },
            root_template: policy.root_template,
            attached_one_shot: face == OfflineFace::OneShot,
            interactive: face != OfflineFace::OneShot,
            cwd: cwd.to_path_buf(),
            ready_catalog: ready_catalog.clone(),
            resume_publication,
            test_injection: Some(injection),
            ready_path: product_host::ReadyPathTiming::default(),
            seed_history: None,
            source_releases: product_host::SourceReleases::default(),
        })
    }
}
