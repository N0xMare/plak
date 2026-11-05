//! Topology bindings and root attachment.

use crate::{CliFailure, CliResult};

use crate::agent::{
    ProductChildSessionBinder, ProductLiveCanceller, ProductMailDeliverer, ProductStateRoots,
};
use crate::binding::CompositionSelection;
use crate::tools::ProductToolExtensions;
use plakous::agent::ReasoningEffort;
use plakous::model::OutputBoundDisposition;
use plakous::subscriptions::{NativeProvider, ResolvedProvider};
use plakous::tools::CodingPresentation;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Inputs for building the product coding agent (run + TUI host).
#[derive(Clone)]
pub struct ProductAgentSpec {
    /// Attached host completion channel used by the prompt compiler.
    pub prompt_host_facts: plakous_host_protocol::PromptHostFacts,
    pub provider: NativeProvider,
    pub resolved: Arc<ResolvedProvider>,
    pub effort: Option<ReasoningEffort>,
    pub effort_explicit: bool,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub plan_state_authority: String,
    pub composition: CompositionSelection,
    /// Durable `ProductAgentTree` id. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions keep the root tree id.
    pub topology_tree_id: Option<String>,
    /// Grant-compiled caller on that tree. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions use the spawned agent id.
    pub topology_caller_id: Option<String>,
    pub state_roots: ProductStateRoots,
    /// Product `compaction.enabled`; gates tool-group scheduled compact.
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    /// Product `subagents`: whether a fresh root may open a topology hub. A
    /// tree that already exists is joined regardless.
    pub topology_enabled: bool,
    /// The attached root's `resume_after_loss = "continue"`: a child held
    /// after a loss continues by itself (spec 03 section 14.1).
    pub continue_after_loss: bool,
    /// The product home this session belongs to, when it has one.
    ///
    /// `None` is a real state — a session constructed with no product home
    /// behind it — and it is stated here rather than discovered: nothing under
    /// `agent/**` reads the environment for it. The topology, the product instruction tail and the
    /// OpenRouter binding are the three things that ask.
    pub product_home: Option<PathBuf>,
    /// The optional tools this node's template selects. Binding resolves them
    /// on this session's route or refuses session creation.
    pub tool_selection: crate::tool_plan::NodeToolSelection,
}

/// Product topology parent for an API-key hub root.
#[derive(Clone)]
pub struct ApiTopologyParent {
    /// The per-request output bound a session being reopened was created with.
    ///
    /// `None` for a new session. See `ProductOpenRouterSpec::created_output_bound`.
    /// Attached host completion channel used by the prompt compiler.
    pub prompt_host_facts: plakous_host_protocol::PromptHostFacts,
    pub created_output_bound: Option<OutputBoundDisposition>,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub plan_state_authority: String,
    pub composition: CompositionSelection,
    /// Durable `ProductAgentTree` id. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions keep the root tree id.
    pub topology_tree_id: Option<String>,
    /// Grant-compiled caller on that tree. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions use the spawned agent id.
    pub topology_caller_id: Option<String>,
    pub state_roots: ProductStateRoots,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub target: crate::binding::ApiTarget,
    /// Product `compaction.enabled`; gates tool-group scheduled compact.
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    /// Product `subagents`: whether a fresh root may open a topology hub.
    pub topology_enabled: bool,
    /// The attached root's `resume_after_loss = "continue"`: a child held
    /// after a loss continues by itself (spec 03 section 14.1).
    pub continue_after_loss: bool,
    /// The product home this hub root belongs to, when it has one; see
    /// [`ProductAgentSpec::product_home`].
    pub product_home: Option<PathBuf>,
    /// See [`ProductAgentSpec::tool_selection`].
    pub tool_selection: crate::tool_plan::NodeToolSelection,
}

/// Spawn/mail/rejoin parent: OAuth native spec or API-key hub root.
#[derive(Clone)]
pub enum SpawnParent {
    Native(ProductAgentSpec),
    Api(ApiTopologyParent),
}

impl SpawnParent {
    pub(crate) fn from_native(spec: ProductAgentSpec) -> Self {
        Self::Native(spec)
    }

    pub(crate) fn state_roots(&self) -> &ProductStateRoots {
        match self {
            Self::Native(spec) => &spec.state_roots,
            Self::Api(parent) => &parent.state_roots,
        }
    }

    /// The product's `subagents` fact for this parent.
    /// The product home behind this parent, when it has one.
    pub(in crate::agent) fn product_home(&self) -> Option<&Path> {
        match self {
            Self::Native(spec) => spec.product_home.as_deref(),
            Self::Api(parent) => parent.product_home.as_deref(),
        }
    }

    pub(in crate::agent) const fn topology_enabled(&self) -> bool {
        match self {
            Self::Native(spec) => spec.topology_enabled,
            Self::Api(parent) => parent.topology_enabled,
        }
    }

    /// Whether a child held after a loss continues by itself under this
    /// parent's root.
    pub(in crate::agent) const fn continues_after_loss(&self) -> bool {
        match self {
            Self::Native(spec) => spec.continue_after_loss,
            Self::Api(parent) => parent.continue_after_loss,
        }
    }

    /// Everything a child's bind and reopen read from the parent that
    /// spawned it. Only `composition` depends on the parent's profile; the
    /// rest belongs to the session and its tree, so a profile switch keeps
    /// it, and a node's frozen `parent_composition` is the one input a
    /// switch has to carry.
    pub(in crate::agent) fn child_inputs(&self) -> SpawnParentInputs<'_> {
        let (workspace_root, product_home, state_roots, composition) = match self {
            Self::Native(spec) => (
                spec.workspace_root.as_path(),
                spec.product_home.as_deref(),
                &spec.state_roots,
                spec.composition,
            ),
            Self::Api(parent) => (
                parent.workspace_root.as_path(),
                parent.product_home.as_deref(),
                &parent.state_roots,
                parent.composition,
            ),
        };
        let (scheduled_compaction, max_model_rounds) = match self {
            Self::Native(spec) => (spec.scheduled_compaction, spec.max_model_rounds),
            Self::Api(parent) => (parent.scheduled_compaction, parent.max_model_rounds),
        };
        SpawnParentInputs {
            tree_id: spawn_parent_tree_id(self),
            workspace_root,
            product_home,
            state_roots,
            scheduled_compaction,
            max_model_rounds,
            topology_enabled: self.topology_enabled(),
            composition,
        }
    }
}

/// The spawn-parent inputs of [`SpawnParent::child_inputs`].
pub(in crate::agent) struct SpawnParentInputs<'a> {
    /// The tree the child belongs to.
    pub(in crate::agent) tree_id: &'a str,
    /// The session workspace the child's copy path is resolved under.
    pub(in crate::agent) workspace_root: &'a Path,
    /// The product home.
    pub(in crate::agent) product_home: Option<&'a Path>,
    /// The process's state roots.
    pub(in crate::agent) state_roots: &'a ProductStateRoots,
    /// Whether compaction is scheduled for sessions under this parent.
    pub(in crate::agent) scheduled_compaction: bool,
    /// The model-round bound per turn.
    pub(in crate::agent) max_model_rounds: usize,
    /// Whether the product admits topology for this parent.
    pub(in crate::agent) topology_enabled: bool,
    /// The parent's tool composition: the one profile-dependent input.
    pub(in crate::agent) composition: CompositionSelection,
}

pub(in crate::agent) fn topology_tree_id(spec: &ProductAgentSpec) -> &str {
    spec.topology_tree_id
        .as_deref()
        .unwrap_or(spec.plan_state_authority.as_str())
}

/// Grant-compiled caller. Root sessions (`topology_caller_id` absent) keep the
/// hub lease, but grants are keyed by genesis `root_agent_id` / durable tree
/// id — never a successor session UUID after rebind.
pub(in crate::agent) fn topology_grant_caller_id<'a>(
    caller_id: Option<&'a str>,
    tree_id: Option<&'a str>,
    plan_state_authority: &'a str,
) -> &'a str {
    caller_id
        .filter(|id| !id.is_empty())
        .or_else(|| tree_id.filter(|id| !id.is_empty()))
        .unwrap_or(plan_state_authority)
}

pub(in crate::agent) fn topology_caller_id(spec: &ProductAgentSpec) -> &str {
    topology_grant_caller_id(
        spec.topology_caller_id.as_deref(),
        spec.topology_tree_id.as_deref(),
        spec.plan_state_authority.as_str(),
    )
}

pub(in crate::agent) fn spawn_parent_tree_id(parent: &SpawnParent) -> &str {
    match parent {
        SpawnParent::Native(spec) => topology_tree_id(spec),
        SpawnParent::Api(parent) => parent
            .topology_tree_id
            .as_deref()
            .unwrap_or(parent.plan_state_authority.as_str()),
    }
}

pub(in crate::agent) fn spawn_parent_caller_id(parent: &SpawnParent) -> &str {
    match parent {
        SpawnParent::Native(spec) => topology_caller_id(spec),
        SpawnParent::Api(parent) => topology_grant_caller_id(
            parent.topology_caller_id.as_deref(),
            parent.topology_tree_id.as_deref(),
            parent.plan_state_authority.as_str(),
        ),
    }
}

pub(in crate::agent) fn spawn_parent_is_root(parent: &SpawnParent) -> bool {
    match parent {
        SpawnParent::Native(spec) => spec.topology_caller_id.is_none(),
        SpawnParent::Api(parent) => parent.topology_caller_id.is_none(),
    }
}

/// Whether this parent states a product home at all.
///
/// Only the *presence* of the home is the question here — the tree store itself
/// is reached through the roots' port — so this is the fact without the
/// product type: `false` is a session with no product home, which a durable
/// open refuses and a non-durable one treats as "no topology".
pub(in crate::agent) fn topology_home_stated(
    parent: &SpawnParent,
    opening_durable_session: bool,
) -> CliResult<bool> {
    match parent.product_home() {
        Some(_) => Ok(true),
        None if opening_durable_session => Err(CliFailure::new(
            "plak product home is unavailable for a durable topology open",
        )),
        None => Ok(false),
    }
}

pub(crate) async fn topology_extensions_if_enabled(
    parent: &SpawnParent,
    selected_tuple: &str,
    attach: crate::topology_runtime::TopologyHubAttach,
    opening_durable_session: bool,
    acquired_owner: &mut Option<String>,
) -> CliResult<ProductToolExtensions> {
    topology_extensions_if_enabled_with_owner(
        parent,
        selected_tuple,
        attach,
        opening_durable_session,
        acquired_owner,
    )
    .await
}

pub(super) async fn topology_extensions_if_enabled_with_owner(
    parent: &SpawnParent,
    selected_tuple: &str,
    attach: crate::topology_runtime::TopologyHubAttach,
    opening_durable_session: bool,
    acquired_owner: &mut Option<String>,
) -> CliResult<ProductToolExtensions> {
    if let Some(epoch) = parent.state_roots().pending_epoch()
        && spawn_parent_is_root(parent)
    {
        return pending_switch_topology_extensions(parent, epoch);
    }
    if attach == crate::topology_runtime::TopologyHubAttach::Inspect {
        return inspected_topology_extensions(parent, opening_durable_session);
    }
    if attach == crate::topology_runtime::TopologyHubAttach::LookupOnly {
        if crate::topology_runtime::lookup(spawn_parent_tree_id(parent)).is_some() {
            return topology_extensions_from_hub_with_owner(
                parent,
                selected_tuple,
                attach,
                true,
                acquired_owner,
            )
            .await;
        }
        // Single-agent compact recompiles the coding runtime after attach.
        // `subagents=false` never inserts a hub; LookupOnly must not fail closed.
        if spawn_parent_is_root(parent) {
            return Ok(ProductToolExtensions::empty());
        }
        return topology_extensions_from_hub_with_owner(
            parent,
            selected_tuple,
            attach,
            true,
            acquired_owner,
        )
        .await;
    }
    if crate::topology_runtime::lookup(spawn_parent_tree_id(parent)).is_some() {
        return topology_extensions_from_hub_with_owner(
            parent,
            selected_tuple,
            attach,
            true,
            acquired_owner,
        )
        .await;
    }
    if !topology_home_stated(parent, opening_durable_session)? {
        return Ok(ProductToolExtensions::empty());
    }
    // Through the roots' store port: whether a tree is persisted is a question
    // for whoever owns the rows, not for a path the agent builder resolves.
    let existing = match parent
        .state_roots()
        .tree_store()
        .map_err(|failure| failure.message().to_owned())
        .and_then(|store| {
            store
                .exists(spawn_parent_tree_id(parent))
                .map_err(|error| error.detail.clone())
        }) {
        Ok(existing) => existing,
        Err(error) if opening_durable_session => {
            return Err(CliFailure::recovery_required(format!(
                "known topology tree could not be opened ({error}); RecoveryRequired"
            )));
        }
        Err(_) => false,
    };
    if existing {
        return topology_extensions_from_hub_with_owner(
            parent,
            selected_tuple,
            attach,
            true,
            acquired_owner,
        )
        .await;
    }
    // The gate is read live, from the file, at the moment a fresh root would
    // open a tree — not from the attach-time snapshot on the spec. A
    // `plak config set orchestration.subagents …` from another shell is in force for
    // the next `/new`, rebind or plan recompile, and an unreadable roster
    // refuses creation.
    if opening_durable_session || !parent.state_roots().roster_source().topology_admitted()? {
        return Ok(ProductToolExtensions::empty());
    }
    topology_extensions_from_hub_with_owner(parent, selected_tuple, attach, false, acquired_owner)
        .await
}

/// The topology catalog an exact-reopen verification compiles, always over a
/// detached copy of the tree with the reopen law applied in memory: a copy of
/// this process's attached tree when it holds one, otherwise of the persisted
/// rows. The copy has an empty in-memory store and is never inserted, leased,
/// folded, settled or saved, and the tools it registers bind to the copy's
/// driver, never the live one (so a live driver's job host is untouched). The
/// tree's live owner, here or in another process, still finds the revision
/// and host generation it wrote.
fn inspected_topology_extensions(
    parent: &SpawnParent,
    opening_durable_session: bool,
) -> CliResult<ProductToolExtensions> {
    let tree_id = spawn_parent_tree_id(parent);
    let unopened = |error: &str| {
        CliFailure::recovery_required(format!(
            "known topology tree could not be opened ({error}); RecoveryRequired"
        ))
    };
    let persisted = if let Some(runtime) = crate::topology_runtime::lookup(tree_id) {
        runtime
            .read(crate::agent_tree::ProductAgentTree::to_persisted)
            .map_err(|error| unopened(&error.message))?
    } else {
        if !topology_home_stated(parent, opening_durable_session)? {
            return Ok(ProductToolExtensions::empty());
        }
        let store = parent
            .state_roots()
            .tree_store()
            .map_err(|failure| unopened(failure.message()))?;
        let Some(persisted) = store
            .load(tree_id)
            .map_err(|error| unopened(&error.detail))?
        else {
            // An open never bootstraps a tree for a session it reopens.
            return Ok(ProductToolExtensions::empty());
        };
        persisted
    };
    let mut tree = crate::agent_tree::ProductAgentTree::reopen(persisted);
    if !tree.epoch_digests_match() {
        return Err(CliFailure::recovery_required(
            "orchestration snapshot digest does not match persisted roster; RecoveryRequired",
        ));
    }
    stamp_root_account(&mut tree, spawn_parent_root_account(parent));
    let vcs = tree.vcs_port();
    let detached = Arc::new(crate::topology_runtime::ProductTopologyRuntime::new(
        tree,
        Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        vcs,
        None,
    ));
    topology_extensions_from_runtime(detached, parent, spawn_parent_caller_id(parent), None)
}

/// The topology catalog a switch successor compiles before its publication:
/// over a detached copy of the tree (this process's attached one, or the
/// persisted rows) with the switch's successor `epoch` appended in memory, so
/// the root's spawn menu, cards and briefing are the successor epoch's. The
/// copy is never inserted, leased, folded or saved, exactly as for an
/// inspected reopen; the published successor later opens against the live
/// tree once the same bytes are appended there.
fn pending_switch_topology_extensions(
    parent: &SpawnParent,
    epoch: &crate::orchestration::OrchestrationSnapshot,
) -> CliResult<ProductToolExtensions> {
    let tree_id = spawn_parent_tree_id(parent);
    let unopened = |error: &str| {
        CliFailure::new(format!(
            "the switch's tree could not be read ({error}); the session is unchanged"
        ))
    };
    let persisted = if let Some(runtime) = crate::topology_runtime::lookup(tree_id) {
        runtime
            .read(crate::agent_tree::ProductAgentTree::to_persisted)
            .map_err(|error| unopened(&error.message))?
    } else {
        parent
            .state_roots()
            .tree_store()
            .map_err(|failure| unopened(failure.message()))?
            .load(tree_id)
            .map_err(|error| unopened(&error.detail))?
            .ok_or_else(|| unopened("no tree is persisted"))?
    };
    let mut tree = crate::agent_tree::ProductAgentTree::reopen(persisted);
    tree.append_epoch(epoch.clone())
        .map_err(|error| unopened(&error.message))?;
    stamp_root_account(&mut tree, spawn_parent_root_account(parent));
    let vcs = tree.vcs_port();
    let detached = Arc::new(crate::topology_runtime::ProductTopologyRuntime::new(
        tree,
        Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        vcs,
        None,
    ));
    topology_extensions_from_runtime(detached, parent, spawn_parent_caller_id(parent), None)
}

async fn topology_extensions_from_hub_with_owner(
    parent: &SpawnParent,
    selected_tuple: &str,
    attach: crate::topology_runtime::TopologyHubAttach,
    must_exist: bool,
    acquired_owner: &mut Option<String>,
) -> CliResult<ProductToolExtensions> {
    let tree_id = spawn_parent_tree_id(parent).to_owned();
    let caller_id = spawn_parent_caller_id(parent);
    let is_root = spawn_parent_is_root(parent);
    let workspace_root = match parent {
        SpawnParent::Native(spec) => spec.workspace_root.clone(),
        SpawnParent::Api(parent) => parent.workspace_root.clone(),
    };
    let selected_tuple = selected_tuple.to_owned();
    let root_account = spawn_parent_root_account(parent);
    let root_effort = spawn_parent_root_effort(parent);
    let roots = parent.state_roots();
    let roster_source = Arc::clone(roots.roster_source());
    let runtime = crate::topology_runtime::resolve_topology_runtime(
        &tree_id.clone(),
        is_root,
        attach,
        || {
            roots
                .tree_store()
                .map_err(|error| crate::agent_tree::TreeRuntimeError::new(error.message()))
        },
        roots.topology_observer(),
        move |store| async move {
            load_or_bootstrap_topology_tree(
                store.as_ref(),
                &tree_id,
                &workspace_root,
                &selected_tuple,
                root_account,
                root_effort.as_deref(),
                must_exist,
                roster_source.as_ref(),
            )
            .await
            .map_err(|error| crate::agent_tree::TreeRuntimeError::new(error.message()))
        },
    )
    .await?;
    // The resolver has acquired this lease; hub existence alone is not ownership.
    // Publish the marker before compiling extensions, which may still fail.
    if attach == crate::topology_runtime::TopologyHubAttach::AttachRoot {
        *acquired_owner = Some(spawn_parent_tree_id(parent).to_owned());
        if let Some(host) = roots.host_shutdown() {
            runtime.driver().bind_host_shutdown(host);
        }
    }
    // A root built without a session attachment (a rebind's successor) keeps
    // the tree resident while its tools live (spec 03 §14).
    let residency = (is_root && attach == crate::topology_runtime::TopologyHubAttach::LookupOnly)
        .then(|| runtime.reside());
    topology_extensions_from_runtime(runtime, parent, caller_id, residency)
}

/// The account spelling the roster freezes for this parent's root.
///
/// The product owns its account names — `grok` for xAI, `claude` for
/// Anthropic — so the spelling comes from the roster source that owns the rest
/// of the roster vocabulary, not from a table in the constructor.
pub(in crate::agent) fn spawn_parent_root_account(parent: &SpawnParent) -> &'static str {
    match parent {
        SpawnParent::Native(spec) => spec.state_roots.roster_source().account_name(spec.provider),
        SpawnParent::Api(parent) => parent.target.account(),
    }
}

pub(in crate::agent) fn spawn_parent_root_effort(parent: &SpawnParent) -> Option<String> {
    match parent {
        SpawnParent::Native(spec) => spec.effort.map(crate::runtime::effort_label),
        SpawnParent::Api(parent) => parent.effort.map(crate::runtime::effort_label),
    }
}

/// The root's tree from `store` with the reopen law applied, or a fresh
/// genesis tree published through it. `store` is the roots' one handle and the
/// roster is the product's answer; nothing here reads a configuration file.
#[allow(
    clippy::too_many_arguments,
    reason = "one genesis binds every root fact the epoch freezes"
)]
pub(in crate::agent) async fn load_or_bootstrap_topology_tree(
    store: &dyn crate::agent_tree::TreeStore,
    tree_id: &str,
    workspace_root: &Path,
    selected_tuple: &str,
    root_account: &str,
    root_effort: Option<&str>,
    must_exist: bool,
    roster_source: &dyn crate::agent::RosterSource,
) -> CliResult<crate::agent_tree::ProductAgentTree> {
    let roster = roster_source.roster(root_account, selected_tuple, root_effort)?;
    let persisted = store.load(tree_id).map_err(|error| {
        if must_exist {
            CliFailure::recovery_required(format!(
                "known topology tree could not be opened ({error}); RecoveryRequired"
            ))
        } else {
            CliFailure::new(error.detail)
        }
    })?;
    if let Some(persisted) = persisted {
        let mut tree = crate::agent_tree::ProductAgentTree::reopen(persisted);
        if !tree.epoch_digests_match() {
            return Err(CliFailure::recovery_required(
                "orchestration snapshot digest does not match persisted roster; RecoveryRequired",
            ));
        }
        if stamp_root_account(&mut tree, root_account) {
            let acknowledged = store
                .save_in_custody(&tree)
                .map_err(|error| CliFailure::new(error.detail))?;
            tree.acknowledge_revision(acknowledged);
        }
        return Ok(tree);
    }
    if must_exist {
        return Err(CliFailure::recovery_required(
            "known topology tree is missing from the product store; RecoveryRequired",
        ));
    }
    let mut tree = genesis_tree(
        tree_id,
        workspace_root,
        selected_tuple,
        root_account,
        roster,
    )
    .await?;
    let acknowledged = store
        .save_in_custody(&tree)
        .map_err(|error| CliFailure::new(error.detail))?;
    tree.acknowledge_revision(acknowledged);
    Ok(tree)
}

async fn genesis_tree(
    tree_id: &str,
    workspace_root: &Path,
    selected_tuple: &str,
    root_account: &str,
    roster: crate::orchestration::RosterInput,
) -> CliResult<crate::agent_tree::ProductAgentTree> {
    let writer_mode = roster.writer_mode;
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root(
        tree_id,
        tree_id,
        "root",
        workspace_root.to_path_buf(),
        selected_tuple,
        writer_mode,
    )
    .await
    .map_err(|error| CliFailure::new(error.message))?;
    tree.install_snapshot(crate::orchestration::OrchestrationSnapshot::freeze(roster));
    stamp_root_account(&mut tree, root_account);
    Ok(tree)
}

/// Persists a genesis tree for a session that has not opened yet.
///
/// A profile fork gives its new session a tree of its own, frozen on the
/// profile's roster, so the source keeps its tree, epoch and cards and still
/// reopens with exactly the tools it sealed. The new session finds this tree
/// under its own id when its tools compile.
///
/// # Errors
/// The tree store is unavailable, a tree already exists under `tree_id`, or
/// the save fails.
pub(crate) async fn seed_genesis_tree(
    roots: &ProductStateRoots,
    tree_id: &str,
    workspace_root: &Path,
    selected_tuple: &str,
    root_account: &str,
    roster: crate::orchestration::RosterInput,
) -> CliResult<()> {
    let store = roots.tree_store()?;
    if store
        .exists(tree_id)
        .map_err(|error| CliFailure::new(error.detail))?
    {
        return Err(CliFailure::new(
            "a topology tree already exists for the new session",
        ));
    }
    let tree = genesis_tree(
        tree_id,
        workspace_root,
        selected_tuple,
        root_account,
        roster,
    )
    .await?;
    // This handle is dropped; the session's own open reloads the tree.
    let _revision = store
        .save_in_custody(&tree)
        .map_err(|error| CliFailure::new(error.detail))?;
    Ok(())
}

pub(in crate::agent) fn stamp_root_account(
    tree: &mut crate::agent_tree::ProductAgentTree,
    root_account: &str,
) -> bool {
    let root_id = tree.root_agent_id.clone();
    let empty = tree
        .node(&root_id)
        .map(|node| node.account.is_empty())
        .unwrap_or(false);
    if empty {
        tree.set_node_account_if_empty(&root_id, root_account);
    }
    empty
}

/// The rejoin spec this host parked on the hub. The hub keeps it as `Any`
/// precisely so the tree cannot read it; this is the one place that knows it
/// is a [`SpawnParent`].
pub(in crate::agent) fn rejoin_parent(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
) -> Option<SpawnParent> {
    runtime
        .rejoin_spec()?
        .downcast_ref::<SpawnParent>()
        .cloned()
}

pub(crate) fn topology_extensions_from_runtime(
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    parent: &SpawnParent,
    caller_id: &str,
    residency: Option<crate::topology_runtime::HubResidency>,
) -> CliResult<ProductToolExtensions> {
    let caller_id = if spawn_parent_is_root(parent) {
        runtime
            .read(|tree| tree.root_agent_id.clone())
            .ok()
            .unwrap_or_else(|| caller_id.to_owned())
    } else {
        caller_id.to_owned()
    };
    let registrations = crate::agent_tree_tools::topology_tool_registrations_on(
        runtime.driver().clone(),
        caller_id,
        topology_host_ports(&runtime, Some(parent.clone()), residency)?,
    )?;
    ProductToolExtensions::from_nested_registrations(registrations).map(|extensions| {
        extensions.with_host_facts(match parent {
            SpawnParent::Native(spec) => spec.prompt_host_facts,
            SpawnParent::Api(parent) => parent.prompt_host_facts,
        })
    })
}

/// The spelling this node's pack gives the await and interrupt slots.
///
/// It is the presentation [`CodingPresentation::admit`] would compile for
/// this parent — Grok's output and kill tools on the Grok pack, pi's `wait`
/// and `stop` on the pi pack (OpenRouter), the Plak names on every other pack
/// (spec 03 §3) — not a second provider match.
fn topology_vocabulary(
    parent: &SpawnParent,
) -> CliResult<crate::agent_tree_tools::TopologyVocabulary> {
    parent_coding_presentation(parent).map(vocabulary_for_presentation)
}

/// Await and interrupt spelling for one compiled coding presentation.
pub(crate) const fn vocabulary_for_presentation(
    presentation: CodingPresentation,
) -> crate::agent_tree_tools::TopologyVocabulary {
    match presentation {
        CodingPresentation::Grok => crate::agent_tree_tools::TopologyVocabulary::TaskTools,
        CodingPresentation::Pi => crate::agent_tree_tools::TopologyVocabulary::WaitStop,
        _ => crate::agent_tree_tools::TopologyVocabulary::Plak,
    }
}

/// The coding presentation a tree node's bound account and model literal
/// compile, read from the same route catalog a session compiles against. An
/// unattached or unenrolled pair has none.
pub(crate) fn node_coding_presentation(account: &str, model: &str) -> Option<CodingPresentation> {
    if let Some(provider) = crate::binding::PlatformProvider::for_account(account) {
        return crate::coding_routes::platform_route_presentation(provider, model).ok();
    }
    if let Some(provider) = crate::binding::oauth_provider_for_account(account) {
        return crate::coding_routes::selected_route_presentation(provider, model).ok();
    }
    (account == "openrouter").then_some(CodingPresentation::Pi)
}

/// A parent whose route the catalog cannot place is an error: its await
/// spelling is never guessed.
fn parent_coding_presentation(parent: &SpawnParent) -> CliResult<CodingPresentation> {
    match parent {
        SpawnParent::Native(spec) => CodingPresentation::admit(
            spec.resolved.configuration(),
            crate::coding_routes::product_presentation_routes()?,
        )
        .map_err(|_| CliFailure::new("native route has no enrolled coding vocabulary")),
        SpawnParent::Api(parent) => match parent.target {
            crate::binding::ApiTarget::Platform { provider } => {
                crate::coding_routes::platform_route_presentation(provider, &parent.model)
            }
            crate::binding::ApiTarget::OpenRouter { .. } => Ok(CodingPresentation::Pi),
        },
    }
}

/// The host's topology ports for `runtime`. Without a parent the tree's own
/// tools still run, but nothing that opens a child session can.
fn topology_host_ports(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    parent: Option<SpawnParent>,
    residency: Option<crate::topology_runtime::HubResidency>,
) -> CliResult<crate::agent_tree_tools::TopologyHostPorts> {
    let observer = parent
        .as_ref()
        .and_then(|parent| parent.state_roots().topology_observer());
    let Some(parent) = parent else {
        return Ok(crate::agent_tree_tools::TopologyHostPorts::default());
    };
    Ok(crate::agent_tree_tools::TopologyHostPorts {
        vocabulary: topology_vocabulary(&parent)?,
        job_host: Some(parent.state_roots().job_registry().clone()),
        controller: Some(Arc::new(crate::agent::children::ProductChildController {
            binder: ProductChildSessionBinder {
                parent: parent.clone(),
            },
            deliverer: ProductMailDeliverer {
                parent,
                runtime: Arc::clone(runtime),
            },
            canceller: ProductLiveCanceller {
                runtime: Arc::clone(runtime),
            },
            _residency: residency,
        })),
        observer,
    })
}

pub struct TopologyInvokeReceipt {
    pub call_id: String,
    pub name: String,
    pub succeeded: bool,
    pub detail: String,
}

/// The operator retry's refusal as its receipt detail.
#[derive(serde::Serialize)]
struct OperatorRecoverRefusal<'a> {
    code: plakous_host_protocol::topology::TopologyErrorCode,
    message: &'a str,
}

/// Idle JSONL `invoke_tool`: TopologyTool on the attached hub, not a model turn.
pub async fn invoke_topology_tool_on_hub(
    tree_id: &str,
    caller_id: Option<&str>,
    name: &str,
    arguments: &serde_json::Value,
    operation_id: &str,
) -> Result<TopologyInvokeReceipt, String> {
    let runtime = crate::topology_runtime::lookup(tree_id)
        .ok_or_else(|| "invoke_tool requires an attached topology hub".to_owned())?;
    // The operator's retry is no model tool: the host runs it on the Agents
    // list's port, whichever node the call names as its caller.
    if name == plakous_host_protocol::topology::OPERATOR_RECOVER_TOOL {
        let (succeeded, detail) =
            match crate::agent::recovery::operator_recover_tool(&runtime, arguments).await {
                Ok(receipt) => (true, serde_json::to_string(&receipt)),
                Err(error) => (
                    false,
                    serde_json::to_string(&OperatorRecoverRefusal {
                        code: error.code,
                        message: &error.message,
                    }),
                ),
            };
        let detail = detail.map_err(|error| error.to_string())?;
        return Ok(TopologyInvokeReceipt {
            call_id: operation_id.to_owned(),
            name: name.to_owned(),
            succeeded,
            detail,
        });
    }
    let caller = runtime
        .read(|tree| {
            Ok::<_, String>(match caller_id {
                Some(id) if !id.is_empty() => {
                    tree.node(id).map_err(|error| error.message)?;
                    id.to_owned()
                }
                _ => tree.root_agent_id.clone(),
            })
        })
        .map_err(|error| error.message)??;
    let ports = topology_host_ports(&runtime, rejoin_parent(&runtime), None)
        .map_err(|error| error.message().to_owned())?;
    let outcome = crate::agent_tree_tools::invoke_granted_topology_tool_on(
        runtime.driver().clone(),
        caller,
        name,
        arguments,
        operation_id.to_owned(),
        ports,
    )
    .await
    .map_err(|error| error.message().to_owned())?;
    Ok(TopologyInvokeReceipt {
        call_id: operation_id.to_owned(),
        name: name.to_owned(),
        succeeded: outcome.is_success(),
        detail: outcome.joined_text(),
    })
}

pub(in crate::agent) fn claim_root_topology_owner(tree_id: &str, is_root: bool) -> Option<String> {
    if is_root && crate::topology_runtime::lookup(tree_id).is_some() {
        Some(tree_id.to_owned())
    } else {
        None
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "An API topology parent freezes route evidence, tool selection, instruction authority, limits, and its owning tree identity."
)]
pub fn api_topology_parent(
    product_home: PathBuf,
    topology_tree_id: String,
    record: &crate::sessions::SessionRecord,
    cwd: PathBuf,
    state_roots: ProductStateRoots,
    model: String,
    effort: Option<ReasoningEffort>,
    target: crate::binding::ApiTarget,
    plan_state_authority: String,
    scheduled_compaction: bool,
    max_model_rounds: usize,
    topology_enabled: bool,
) -> ApiTopologyParent {
    ApiTopologyParent {
        // The caller already hands the home in; the parent states it rather
        // than letting the binder rediscover it.
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        product_home: Some(product_home),
        tool_selection: crate::tool_plan::NodeToolSelection::default(),
        workspace_root: record.workspace_root.clone(),
        cwd,
        plan_state_authority,
        composition: record.composition,
        topology_tree_id: Some(topology_tree_id),
        topology_caller_id: None,
        state_roots,
        model,
        effort,
        target,
        scheduled_compaction,
        max_model_rounds,
        topology_enabled,
        continue_after_loss: false,
        created_output_bound: None,
    }
}

/// Parks `parent` on its attached hub as the root's rejoin spec, with the
/// root's loss policy and the retirement recovery hook: what a root attach
/// leaves for the hub's later child recoveries and reopens.
pub(crate) async fn park_rejoin_parent(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    tree_id: &str,
    parent: &SpawnParent,
) {
    // The custody-take scan's recovery runs read the root's loss policy.
    runtime.set_continue_after_loss(parent.continues_after_loss());
    runtime.remember_rejoin_spec(Arc::new(parent.clone()));
    crate::agent::recovery::install_retirement_recovery(runtime, tree_id).await;
}
