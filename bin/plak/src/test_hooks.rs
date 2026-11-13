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
    scripted_runtime_binding_with_extensions, scripted_runtime_binding_with_telemetry,
    scripted_topology_runtime_binding,
};

use crate::store::sessions as product_sessions;
use crate::{product_agent, product_config, product_host, product_paths};

/// The exact model id this hook answers to.
pub(crate) const SELECTOR: &str = "offline-loopback-v1";

pub(crate) fn upgrade_state_key() -> Option<String> {
    std::env::var("PLAK_TEST_STATE_KEY")
        .ok()
        .filter(|key| crate::carry::upgrade::state_key_valid(key))
}

pub(crate) fn upgrade_input_limit() -> Option<String> {
    std::env::var("PLAK_TEST_HOST_UPGRADE_INPUT_LIMIT").ok()
}

pub(crate) fn upgrade_runner_revision() -> Option<String> {
    std::env::var("PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION").ok()
}

pub(crate) fn upgrade_import_refused() -> bool {
    std::env::var("PLAK_TEST_HOST_UPGRADE_IMPORT_REFUSAL").as_deref() == Ok("1")
}

/// Model a candidate's incompatible executable revision, never a stored verdict.
pub(crate) fn binding_executor_cut() -> bool {
    std::env::var_os("PLAK_TEST_BINDING_CUT_IMAGE").is_some_and(|image| {
        std::env::current_exe().is_ok_and(|current| current == Path::new(&image))
    })
}

pub(crate) fn force_host_upgrade() -> bool {
    std::env::var("PLAK_TEST_FORCE_HOST_UPGRADE").as_deref() == Ok("1")
}

pub(crate) fn upgrade_crash_cut(name: &str) {
    if std::env::var("PLAK_TEST_HOST_UPGRADE_CUT").is_ok_and(|cut| cut == name) {
        std::process::exit(86);
    }
}

pub(crate) fn upgrade_preparation_refused(name: &str) -> bool {
    if name == "step-drain-gate" {
        return std::env::var_os("PLAK_TEST_HOST_UPGRADE_DRAIN_GATE")
            .is_some_and(|path| !Path::new(&path).exists());
    }
    std::env::var("PLAK_TEST_HOST_UPGRADE_PREPARE_FAILURE").is_ok_and(|cut| cut == name)
}

pub(crate) fn upgrade_key_environment(key: &str) -> impl Iterator<Item = (&'static str, String)> {
    std::iter::once(("PLAK_TEST_STATE_KEY", key.to_owned()))
}

/// Extend the real catalog with only the credential-free scripted route.
fn profile_ready_catalog(
    catalog: &plakous_host_protocol::views::ReadyCatalog,
) -> plakous_host_protocol::views::ReadyCatalog {
    let mut catalog = catalog.clone();
    for mut offline in crate::profile_test_catalog::ready_catalog(&[SELECTOR]).accounts {
        if let Some(section) = catalog
            .accounts
            .iter_mut()
            .find(|section| section.account == offline.account)
        {
            // Unauthenticated catalog rows must not become selectable along
            // with the credential-free model. Authenticated rows stay real.
            if !section.ready {
                section.entries.clear();
            }
            section.ready = true;
            section.entries.retain(|entry| entry.id != SELECTOR);
            section.entries.append(&mut offline.entries);
        } else {
            catalog.accounts.push(offline);
        }
    }
    catalog
}

pub(crate) fn refresh_profile_ready_catalog(
    sections: Vec<plakous_host_protocol::views::ProviderSection>,
) -> Vec<plakous_host_protocol::views::ProviderSection> {
    if selector().as_deref() != Some(std::ffi::OsStr::new(SELECTOR)) {
        return sections;
    }
    profile_ready_catalog(&sections.into()).accounts
}

/// An injected account section grants readiness only to its exact model.
pub(crate) fn profile_route_ready(
    catalog: &[plakous_host_protocol::views::ProviderSection],
    account: &str,
    model: &str,
) -> Option<bool> {
    (selector().as_deref() == Some(std::ffi::OsStr::new(SELECTOR)) && account == "codex").then(
        || {
            catalog.iter().any(|section| {
                section.ready
                    && section.account == account
                    && section.entries.iter().any(|entry| entry.id == model)
            })
        },
    )
}

pub(crate) mod client_fixtures;
mod host;
mod runner;
pub(crate) use host::OfflineFace;
#[cfg(test)]
mod tests;
pub(crate) use runner::{
    host_start_environment, runner_before_ready, runner_lease_clock, runner_link,
    runner_ready_result, runner_windows,
};

#[allow(dead_code, reason = "headless builds do not start hosts")]
pub(crate) fn runner_detached() -> bool {
    crate::environment::test_runner_detached()
}

/// The auto-started host's idle-exit window override, in whole seconds.
#[allow(dead_code, reason = "headless builds do not start hosts")]
pub(crate) fn host_idle_exit_seconds() -> Option<std::ffi::OsString> {
    hook_environment("PLAK_TEST_HOST_IDLE_EXIT_SECONDS")
}

pub(crate) fn storage_capacity_override() -> Option<plakous_hosted::session_store::StorageAdmission>
{
    use plakous_hosted::session_store::StorageAdmission;
    match std::env::var_os("PLAK_TEST_STORAGE_CAPACITY").as_deref() {
        Some(value) if value == "available" => Some(StorageAdmission::Available),
        Some(value) if value == "warning" => {
            Some(StorageAdmission::Warning("injected low capacity".into()))
        }
        Some(_) => Some(StorageAdmission::Refused(
            "injected capacity refusal".into(),
        )),
        None => (selector().as_deref() == Some(std::ffi::OsStr::new(SELECTOR)))
            .then_some(StorageAdmission::Available),
    }
}

/// Holds every hash of this build's image for the offline script's
/// `identity_hash_delay_ms`, so a test can make a manifest-less build's
/// identity slow without loading the machine. A script that cannot be read
/// sets nothing here; the face that uses it reports that.
fn install_identity_hash_delay() {
    if let Some(Ok(selection)) = selected()
        && let Some(delay) = selection.plan.identity_hash_delay_ms
    {
        let _ = crate::build_identity::HASH_DELAY.set(std::time::Duration::from_millis(delay));
    }
}

/// Installs the process-wide test hooks: the identity hash delay, and the
/// loopback or child runner when the offline script asks for one (spec 01
/// §8.5). Nothing else selects a runner.
pub(crate) fn install() -> Result<(), String> {
    client_fixtures::install_pending_gate()?;
    install_identity_hash_delay();
    if let Some(millis) = hook_environment("PLAK_TEST_RELOAD_PROBE_LAUNCH_DELAY_MS")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok())
    {
        let _ = crate::reload_probe_latency::DELAY.set(std::time::Duration::from_millis(millis));
    }
    if let Some(Ok(selection)) = selected() {
        if selection.plan.loopback_runner {
            plakous_offline_crash_harness::loopback::select(
                plakous_hosted::runner_reader::test_runner_extension(),
            )?;
        } else if let Some(child) = &selection.plan.child_runner {
            child_runner::select(child)?;
        }
    }
    Ok(())
}

/// The offline script's child runner: a real runner daemon, reached as a
/// host's ssh reaches one, through `plak runner connect --stdio` run from this
/// executable over the runner's own product home.
mod child_runner;

pub(crate) fn runner_services() -> Option<plakous::runner::RunnerServices> {
    child_runner::services().or_else(plakous_offline_crash_harness::loopback::services)
}

/// A `quit-as-handoff` file beside the evidence log makes the agent face's
/// `quit` a planned handoff (`request_handoff_and_release`), as a reload's
/// release is: the crash sweep's `handoff_resume` shape cuts its record and
/// release.
pub(crate) fn quit_as_handoff() -> bool {
    matches!(selected(), Some(Ok(selection))
        if selection.evidence.with_file_name("quit-as-handoff").exists())
}

pub(crate) fn assistant_done_release() -> Option<PathBuf> {
    if std::env::var("PLAK_TEST_MODEL").as_deref() != Ok(SELECTOR) {
        return None;
    }
    std::env::var_os("PLAK_TEST_ASSISTANT_DONE_RELEASE").map(PathBuf::from)
}

/// A `carry-cut` file beside the evidence log naming a boundary (`prepared`
/// or `runtime-returned`) kills the process at that boundary of a seeded
/// create: before its runtime create, or after it returned and before the
/// product record that binds it is saved. The carry crash law relaunches over
/// what the kill left.
fn seeded_create_cut(
    cut: PathBuf,
) -> impl Fn(product_host::HostWorkflow, product_host::HostBoundary) + Send + Sync {
    move |workflow, boundary| {
        let requested = match std::fs::read_to_string(&cut).as_deref().map(str::trim) {
            Ok("prepared") => product_host::HostBoundary::Prepared,
            Ok("runtime-returned") => product_host::HostBoundary::RuntimeReturned,
            _ => return,
        };
        if workflow == product_host::HostWorkflow::SeededCreate && boundary == requested {
            std::process::abort();
        }
    }
}

/// The topology registrations one scripted tree exposes as its whole registry.
///
/// The binder is shared across every runtime this selection builds. A fresh
/// binder per runtime would restart the child ordinal, and a failure or hold
/// dial would land on the wrong child.
fn tree_registrations(
    prepared: &PreparedTree,
) -> Result<Vec<plakous::tools::ToolRegistration>, String> {
    if let Some(tools) = &prepared.product_tools {
        return Ok(tools.clone());
    }
    let (driver, binder) = (&prepared.driver, Arc::clone(&prepared.binder));
    let caller_id = driver
        .read(|tree| tree.root_agent_id.clone())
        .map_err(|_| "offline scripted tree lock".to_owned())?;
    plakous_hosted::agent_tree_tools::topology_tool_registrations_on(
        driver.clone(),
        caller_id,
        plakous_hosted::agent_tree_tools::TopologyHostPorts {
            controller: Some(binder),
            ..Default::default()
        },
    )
    .map_err(|error| error.message().to_owned())
}

/// One scripted tree and the child binder every runtime of that selection
/// shares, or the product's own child route when the script names a
/// product child script.
struct PreparedTree {
    driver: plakous_hosted::agent_tree_tools::TreeDriver,
    binder: Arc<ScriptedChildBinder>,
    runtime: Arc<plakous_hosted::topology_runtime::ProductTopologyRuntime>,
    product_tools: Option<Vec<plakous::tools::ToolRegistration>>,
}

/// Pins the product child roster when the script names one.
///
/// A grandchild script beside the child script lets the `review` card spawn
/// `note` and raises the depth ceiling to 2, so that child's grant can spawn.
/// Without the file the card stays a leaf and existing child shapes keep the
/// spawn menu they had.
fn pin_product_child_roster(
    tree: &mut plakous_hosted::agent_tree::ProductAgentTree,
    plan: &ScriptPlan,
) -> Result<(), String> {
    let Some(script) = plan.product_child_script.as_ref() else {
        return Ok(());
    };
    tree.pin_bind_hints_for_test(crate::roster_input::product_bind_hints());
    let mut review =
        plakous_hosted::orchestration::RosterBinding::leaf("openrouter", "offline/child");
    if script.with_file_name("grandchild-script.json").is_file() {
        let note = plakous_recursive_subagents::protocol::TemplateName::parse("note")
            .map_err(|error| format!("offline script grandchild template: {error}"))?;
        review.spawns = vec![note];
        tree.pin_template(
            "note",
            plakous_hosted::orchestration::RosterBinding::leaf("openrouter", "offline/child"),
        );
        // Depth 1 admits the child and then withholds spawn_agent.
        tree.pin_max_depth_for_test(2);
    }
    tree.pin_template("review", review);
    Ok(())
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
    detached: bool,
) -> Result<PreparedTree, crate::CliFailure> {
    use plakous_hosted::agent_tree::{MemoryTreeStore, ProductAgentTree, TreeStore};
    use plakous_hosted::agent_tree_protocol::WriterMode;
    use plakous_hosted::occupancy_vcs::VcsOps;

    let vcs: Arc<dyn VcsOps> = scripted_tree_vcs(plan);
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
    pin_product_child_roster(&mut tree, plan)?;
    let inner: Arc<dyn TreeStore> = if plan.durable_tree {
        paths
            .ensure_layout()
            .map_err(|error| format!("offline scripted tree layout: {error}"))?;
        crate::store::tree::bind_custody_host(paths)
            .map_err(|error| format!("offline scripted tree custody host: {error}"))?;
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
    if let Some(persisted) = store.load(bootstrap_id).map_err(|error| {
        format!(
            "offline scripted tree could not be loaded: {}",
            error.detail
        )
    })? {
        tree = ProductAgentTree::reopen(persisted);
        tree.set_vcs(Arc::clone(&vcs));
    }
    let runtime = if detached {
        Arc::new(
            plakous_hosted::topology_runtime::ProductTopologyRuntime::new(
                tree,
                Arc::new(MemoryTreeStore::new()),
                vcs,
                None,
            ),
        )
    } else {
        drop(vcs);
        attach_scripted_tree(tree_id, tree, store)?
    };
    let driver = runtime.driver().clone();
    let binder = Arc::new(ScriptedChildBinder {
        services: plakous_hosted::agent_tree_tools::ChildServices {
            mail: plan.tree_refresh,
            ..plakous_hosted::agent_tree_tools::ChildServices::BIND
        },
        driver: driver.clone(),
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
    Ok(PreparedTree {
        driver,
        binder,
        runtime,
        product_tools: None,
    })
}

/// Puts a `product_child_script` tree's children on the product's own
/// child route: the root's tools come from its parked parent.
fn attach_product_children(
    prepared: &mut PreparedTree,
    plan: &ScriptPlan,
    tree_id: &str,
    workspace: &Path,
    state_roots: &product_agent::ProductStateRoots,
    evidence: &Path,
    continue_after_loss: bool,
) -> Result<(), crate::CliFailure> {
    let Some(script) = &plan.product_child_script else {
        return Ok(());
    };
    prepared.product_tools = Some(run_to_end(
        plakous_hosted::runtime::test_kit::scripted_root_topology(
            Arc::clone(&prepared.runtime),
            tree_id,
            workspace,
            state_roots.clone(),
            product_child_binding(
                script.clone(),
                evidence.with_file_name("child-evidence.jsonl"),
                Arc::clone(&prepared.runtime),
            ),
            continue_after_loss,
        ),
    )??);
    Ok(())
}

/// A product child's binding: a real scripted session over `script`, read
/// whenever the child binds (a spawn, or a reopen on a later build), whose
/// provider requests append to `evidence`.
///
/// The direct child is `c1`. Any later id reads `grandchild-script.json`
/// beside `script` when that file exists. A child that can spawn (that file
/// is present) compiles topology tools for its own caller. Every product
/// child takes the root's job host so a parked process stays on the tree
/// instead of the binding.
fn product_child_binding(
    script: PathBuf,
    evidence: PathBuf,
    runtime: Arc<plakous_hosted::topology_runtime::ProductTopologyRuntime>,
) -> Arc<plakous_hosted::runtime::ScriptedChildBinding> {
    Arc::new(move |agent_id, path, roots| {
        let workspace = Path::new(path);
        std::fs::create_dir_all(workspace).map_err(|error| error.to_string())?;
        let grandchild = script.with_file_name("grandchild-script.json");
        let nested = grandchild.is_file() && agent_id != "c1";
        let script = if nested { grandchild } else { script.clone() };
        let evidence = if nested {
            evidence.with_file_name("grandchild-evidence.jsonl")
        } else {
            evidence.clone()
        };
        let plan = ScriptPlan::read(&script).map_err(|error| error.message().to_owned())?;
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        let plan = Arc::new(plan);
        // The product reopens a child only over a coding projection; the
        // pi pack is the one an `OpenRouter` parent's child compiles.
        let mut binding = if !nested && script.with_file_name("grandchild-script.json").is_file() {
            let tools = plakous_hosted::runtime::test_kit::scripted_caller_topology_tools(
                Arc::clone(&runtime),
                agent_id,
                workspace,
                roots.clone(),
            )
            .map_err(|error| error.message().to_owned())?;
            plakous_offline_crash_harness::scripted_pi_runtime_binding_with_extensions(
                workspace, controller, &plan, tools,
            )
        } else {
            plakous_offline_crash_harness::scripted_pi_runtime_binding(workspace, controller, &plan)
        }
        .map_err(|error| error.to_string())?;
        binding = binding.with_job_host(roots.test_job_host(agent_id.to_owned()));
        Ok(roots.bind_process_transport(binding))
    })
}

/// Attaches a scripted tree as a production root attach does, through the
/// process hub: the hub holds the tree, the session takes one root
/// attachment, and its release gives that back, so the last one ends the tree
/// and its custody. The tree is published under the attach's custody claim
/// when the hub does not hold it yet; an earlier runtime this process built
/// for the session shares the attached tree instead, since publishing a
/// reloaded copy would leave the hub's handle a revision behind the store.
/// Another host's custody stays the typed refusal (exit 7).
fn attach_scripted_tree(
    tree_id: &str,
    mut tree: plakous_hosted::agent_tree::ProductAgentTree,
    store: Arc<dyn plakous_hosted::agent_tree::TreeStore>,
) -> Result<Arc<plakous_hosted::topology_runtime::ProductTopologyRuntime>, crate::CliFailure> {
    run_to_end(plakous_hosted::topology_runtime::resolve_topology_runtime(
        tree_id,
        true,
        plakous_hosted::topology_runtime::TopologyHubAttach::AttachRoot,
        || Ok(store),
        None,
        |store| async move {
            let revision = store.save_in_custody(&tree)?;
            tree.acknowledge_revision(revision);
            Ok(tree)
        },
    ))?
    .map_err(|error| {
        if error.hosted_elsewhere_refusal().is_some() {
            crate::CliFailure::from(error)
        } else {
            crate::CliFailure::new(format!("offline scripted tree could not attach: {error}"))
        }
    })
}

/// Runs `future` to its end on its own runtime and thread: the fixture's
/// configuration path is synchronous, and may itself run on a runtime thread.
fn run_to_end<F>(future: F) -> Result<F::Output, String>
where
    F: Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|runtime| runtime.block_on(future))
                    .map_err(|error| format!("offline fixture runtime: {error}"))
            })
            .join()
            .map_err(|_| "offline fixture runtime panicked".to_owned())?
    })
}

fn scripted_tree_vcs(plan: &ScriptPlan) -> Arc<dyn plakous_hosted::occupancy_vcs::VcsOps> {
    use plakous_hosted::occupancy_vcs::{
        AdoptFailure, ConflictCandidate, ConflictPath, VcsSnapshot, fake::FakeVcs,
    };
    let fake = Arc::new(FakeVcs::new());
    if plan.tree_refresh {
        fake.script_refresh();
    }
    if plan.tree_conflict {
        fake.adopt_next_with(Err(AdoptFailure::Conflict {
            paths: vec![ConflictPath {
                path: "conflicted.txt".to_owned(),
                kind: "contents".to_owned(),
            }],
            candidate: Some(Box::new(ConflictCandidate {
                snapshot: VcsSnapshot {
                    commit: "conflict-commit".to_owned(),
                    content: "conflicted-tree".to_owned(),
                    ..VcsSnapshot::default()
                },
                base: VcsSnapshot {
                    commit: "target-commit".to_owned(),
                    content: "target-tree".to_owned(),
                    ..VcsSnapshot::default()
                },
            })),
        }));
    }
    fake
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
        claim: &plakous_hosted::agent_tree::CustodyClaim,
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
        self.inner.save(tree, claim)
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

    fn custody_port(&self) -> &dyn plakous_hosted::agent_tree::TreeCustody {
        self.inner.custody_port()
    }
}

/// Stands in for the product's child session on a scripted tree.
///
/// The spawn tool records the bind after this future resolves, so a child's
/// turn may only be completed once the locator is on the node. Completing it
/// earlier is undone by the bind itself, which reads as a child that ran and
/// then un-ran.
struct ScriptedChildBinder {
    services: plakous_hosted::agent_tree_tools::ChildServices,
    driver: plakous_hosted::agent_tree_tools::TreeDriver,
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
}

impl plakous_hosted::agent_tree_tools::ChildController for ScriptedChildBinder {
    fn services(&self) -> plakous_hosted::agent_tree_tools::ChildServices {
        self.services
    }

    fn deliver(
        &self,
        request: plakous_hosted::agent_tree_tools::MailDeliveryRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        plakous_hosted::agent_tree_tools::MailDeliveryAck,
                        plakous_hosted::agent_tree_tools::MailDeliveryFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        use plakous_hosted::agent_tree_tools::{MailDeliveryAck, MailDeliveryFailure};
        Box::pin(async move {
            let failure = |error: plakous_hosted::agent_tree_protocol::TopologyError| {
                MailDeliveryFailure::RecoveryRequired(error.message.into())
            };
            // Finish the scripted attempt before returning the root's send.
            // No child commit can then race the root's next provider step.
            let writer = self.driver.clone();
            let (result, effects) = self
                .driver
                .mutate(move |tree| {
                    writer.fold_and_commit(tree, |tree| {
                        tree.acknowledge_mail(&request.message_id)?;
                        tree.submit_result(
                            &request.agent_id,
                            &plakous_hosted::agent_tree_protocol::SubmitResultRequest {
                                turn_token: request.turn_token,
                                output_json: "{\"summary\":\"offline scripted followup done\"}"
                                    .into(),
                            },
                            None,
                        )?;
                        tree.record_turn_completed(&request.agent_id)
                    })
                })
                .await
                .map_err(failure)?;
            result.map_err(failure)?;
            let settled = self.driver.settle(effects).await.map_err(failure)?;
            if let Some(error) = settled.cleanup_failures.into_iter().next() {
                return Err(failure(error));
            }
            Ok(MailDeliveryAck { turn_id: None })
        })
    }

    fn withdraw_followup(
        &self,
        _driver: plakous_hosted::agent_tree_tools::TreeDriver,
        _agent_id: String,
        _message_id: String,
        _receipt: Option<(
            String,
            plakous_hosted::agent_tree_protocol::TopologyErrorCode,
        )>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), plakous_hosted::agent_tree_protocol::TopologyError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Err(plakous_hosted::agent_tree_protocol::TopologyError::new(
                plakous_hosted::agent_tree_protocol::TopologyErrorCode::OperationRecoveryRequired,
                "scripted followup completion cannot be withdrawn",
            ))
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Offline binding selects each scripted crash shape and its recovery evidence before constructing the test runtime."
    )]
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
        let publish = self.durable;
        let agent_id = start.spawned.agent_id.clone();
        let driver = self.driver.for_agent(&agent_id);
        if self.child_process {
            let quiescent = self.child_process_quiescent;
            let runtime = plakous_hosted::topology_runtime::lookup(&self.session_id);
            let roots = self.state_roots.clone();
            let path = driver
                .read(|tree| tree.node(&agent_id).ok().map(|node| node.path.clone()))
                .ok()
                .flatten();
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
            if publish && hold_ms == 0 {
                let failure = |error: plakous_hosted::agent_tree_protocol::TopologyError| {
                    plakous_hosted::agent_tree_tools::SpawnBindError::RecoveryRequired {
                        stage: plakous_hosted::topology_runtime::ChildStartupStage::Registering,
                        message: error.message,
                    }
                };
                let session: plakous::sessions::SessionLocator = locator.parse().map_err(|_| {
                    plakous_hosted::agent_tree_tools::SpawnBindError::Refused(
                        "scripted child locator is invalid".into(),
                    )
                })?;
                let (completed, effects) = driver
                    .mutate({
                        let (driver, agent_id, locator) =
                            (driver.clone(), agent_id.clone(), locator.clone());
                        move |tree| {
                            tree.record_child_session(&agent_id, &locator)?;
                            driver.save(tree)?;
                            tree.project_feed(
                                &agent_id,
                                &session.session_id().to_string(),
                                &[],
                                &plakous_hosted::agent_tree::TreeFeedReads::default(),
                            )
                            .map_err(plakous_hosted::agent_tree_protocol::TopologyError::from)?;
                            driver.save(tree)?;
                            if let Ok(node) = tree.node(&agent_id) {
                                std::fs::write(node.path.join("child.txt"), "child\n").ok();
                            }
                            tree.submit_result(
                                &agent_id,
                                &plakous_hosted::agent_tree_protocol::SubmitResultRequest {
                                    turn_token: start.turn_token,
                                    output_json: "{\"summary\":\"offline scripted child done\"}"
                                        .into(),
                                },
                                None,
                            )?;
                            tree.record_turn_completed(&agent_id)?;
                            tree.settle_for_test();
                            driver.save(tree)
                        }
                    })
                    .await
                    .map_err(failure)?;
                completed.map_err(failure)?;
                driver.settle(effects).await.map_err(failure)?;
                return Ok(plakous_hosted::agent_tree_tools::SpawnBound {
                    durably_registered: true,
                    session_locator: locator,
                    turn_id: None,
                });
            }
            tokio::spawn(async move {
                for _ in 0..600 {
                    let ready = {
                        driver
                            .read(|tree| {
                                tree.node(&agent_id)
                                    .is_ok_and(|node| node.session_locator.is_some())
                            })
                            .unwrap_or(false)
                    };
                    if !ready {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        continue;
                    }
                    if hold_ms > 0 {
                        let _ = driver.read(|tree| {
                            if let Ok(node) = tree.node(&agent_id) {
                                std::fs::write(node.path.join("holding.txt"), "holding\n").ok();
                            }
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                    }
                    let _ = driver
                        .mutate({
                            let (agent_id, driver, publish) =
                                (agent_id.clone(), driver.clone(), publish);
                            move |tree| {
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
                                        output_json:
                                            "{\"summary\":\"offline scripted child done\"}"
                                                .to_owned(),
                                    },
                                    None,
                                );
                                if submitted.is_ok() {
                                    tree.record_turn_completed(&agent_id).ok();
                                }
                                tree.settle_for_test();
                                if publish {
                                    driver.save(tree).ok();
                                }
                            }
                        })
                        .await;
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
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Pin<Box<dyn Future<Output = plakous_hosted::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async move {
            let cancellation = if self.control.is_settled() {
                Ok(())
            } else {
                self.control.request_cancel().await.map(|_| ())
            };
            if let Some(dispatched) = dispatched {
                let _ = dispatched.send(());
            }
            let failure = match cancellation {
                Ok(()) => {
                    self.control.wait_until_settled().await;
                    None
                }
                Err(error) => Some(error.to_string()),
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
    // Once ready, wait in a builtin over a separate FIFO: a foreground sleep
    // can defer the trap past the stop grace, and stdin closes during stop.
    let plan = Arc::new(serde_json::from_value(serde_json::json!({
        "version": 1, "process_tools": true, "steps": [
            {"output":{"tool":{"name":"process_session_start","arguments":{
                "command":"sleep 1; trap 'printf stopped > stopped.txt; exit 0' TERM; mkfifo stop-wait; exec 3<>stop-wait; printf '%s' \"$$\" > owned.pid; read line <&3",
                "workdir":".","login":false,"tty":false,"keep_stdin_open":false,
                "yield_time_ms":0,"max_output_bytes":8192
            }}},"finish":"tool_calls"},
            final_step
        ]
    })).map_err(|error| error.to_string())?);
    Ok(plan)
}

/// Records the process child's session and its Start intent.
async fn record_process_child_start(
    runtime: &plakous_hosted::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    session: &plakous::sessions::OpenedSession,
    command: &str,
    prompt: &'static str,
) -> Result<(), String> {
    let (child, locator, command) = (
        agent_id.to_owned(),
        session.locator().to_string(),
        command.to_owned(),
    );
    runtime
        .mutate_then_emit(false, move |tree| {
            tree.record_child_session(&child, &locator)
                .map_err(|error| error.message)?;
            tree.record_command(plakous_hosted::agent_tree::AgentCommand::intent(
                command,
                &child,
                tree.attempt(&child)
                    .map(|attempt| attempt.attempt_id.clone()),
                plakous_hosted::agent_tree::AgentCommandKind::Start,
                prompt,
                Vec::new(),
            ))
            .map_err(|error| error.message)
        })
        .await
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
    record_process_child_start(&runtime, &agent_id, &session, command_id.as_str(), prompt).await?;
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
    let child = agent_id.clone();
    runtime
        .mutate_then_emit(false, move |tree| {
            feed.apply(tree, &child)
                .map(|_| ())
                .map_err(|error| error.message)
        })
        .await?;
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
    /// Leased, or for the reopen probe rebuilt from frozen records.
    mcp_binding: plakous_hosted::mcp_binding::McpBinding,
    session: product_sessions::SessionRecord,
    state_roots: product_agent::ProductStateRoots,
    bound: std::sync::OnceLock<Result<Vec<plakous::tools::ToolRegistration>, String>>,
}

impl RootTools {
    fn new(
        paths: &product_paths::PlakPaths,
        state_roots: &product_agent::ProductStateRoots,
        session: &product_sessions::SessionRecord,
        face: OfflineFace,
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
            mcp_binding: if face == OfflineFace::Verify {
                plakous_hosted::mcp_binding::McpBinding::Recorded
            } else {
                plakous_hosted::mcp_binding::McpBinding::Lease
            },
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
                    self.mcp_binding,
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
    hook_environment("PLAK_TEST_MODEL")
}

fn hook_environment(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// The terminal application's hooks.
#[cfg(feature = "tui")]
pub(crate) mod tui {
    /// The pilot sidecar's root.
    pub(crate) fn pilot_root() -> Option<std::ffi::OsString> {
        std::env::var_os("PLAK_TEST_TUI_PILOT")
    }
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

fn provider_release() -> Option<std::ffi::OsString> {
    std::env::var_os("PLAK_TEST_PROVIDER_RELEASE")
}
