//! Doubles of the product ports, for the hosted crate's own tests.
//!
//! A hosted test should need a product home only when what it is testing *is*
//! the composition -- the loop over Plak's real files. Everything else runs on
//! these: a surface that records what it was asked for, a root binding held in
//! memory, and the session record store the contract suite already runs over.
//!
//! What is deliberately absent is as important: no `PlakPaths`, no
//! `ProductConfig`, no SQLite. A test that cannot be written against this kit
//! is a composition test and belongs to the binary.

use std::future::Future;
use std::sync::{Arc, Mutex};

use plakous_host_protocol::views::{ProviderDef, ProviderSection};

use crate::binding::CompositionSelection;

use super::surface::{SurfaceEffect, SurfaceFacts};
use super::{
    RootBindingDocument, RootBindingDraft, RootBindingEdit, RootBindingSaveError, Sender,
    SessionHome, ShellFallbackFacts, ShellHostConfig, ShellHostRunner, Surface, SurfaceHandler,
};

/// A surface that answers every product intent by recording it.
#[derive(Debug, Default)]
pub struct RecordingSurface {
    calls: Mutex<Vec<&'static str>>,
}

impl RecordingSurface {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The port methods reached, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, call: &'static str) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> SurfaceHandler<P> for Arc<RecordingSurface> {
    fn needs_usage(&self, _intent: &P::Intent) -> bool {
        true
    }
    fn dispatch(
        &self,
        _intent: P::Intent,
        _facts: SurfaceFacts<'_>,
        _events: &Sender<P>,
    ) -> SurfaceEffect {
        self.record("extension");
        SurfaceEffect::default()
    }
    fn publish_template_readiness(&self, _catalog: &[ProviderSection], _events: &Sender<P>) {
        self.record("publish_template_readiness");
    }
    fn refresh_readiness(&self) -> Result<(Vec<ProviderDef>, Vec<ProviderSection>), String> {
        self.record("refresh_readiness");
        Ok((Vec::new(), Vec::new()))
    }
}

/// A surface for a test that does not inspect the driver's extension payload.
#[must_use]
pub fn recording_surface<P: plakous_host_protocol::ProtocolExtension>() -> Surface<P> {
    Some(Box::new(RecordingSurface::new()))
}

/// Independent extension used by hosted fixtures; deliberately neither Copy nor Default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestExtension(pub String);
/// Read-only request from the independent test driver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeIntent {
    InspectUsage,
}
/// Observation owned by the independent test driver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeObservation {
    UsageObserved,
}
impl plakous_host_protocol::ProtocolExtension for TestExtension {
    type Intent = ProbeIntent;
    type Observation = ProbeObservation;
}
impl plakous_host_protocol::ObservationExtension for ProbeObservation {
    fn latest_state(&self) -> bool {
        true
    }
    fn same_kind(&self, _other: &Self) -> bool {
        true
    }
    fn kind_label(&self) -> &'static str {
        "probe_usage"
    }
}

/// A root binding held in memory, with the file's publication semantics.
#[derive(Debug, Default)]
pub struct MemoryBindingDocument {
    current: Mutex<Option<RootBindingDraft>>,
}

impl MemoryBindingDocument {
    /// A document with no root binding configured.
    #[must_use]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A document that already carries `draft`.
    #[must_use]
    pub fn holding(draft: RootBindingDraft) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(Some(draft)),
        })
    }

    /// The binding as it stands now.
    #[must_use]
    pub fn read(&self) -> Option<RootBindingDraft> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl RootBindingDocument for Arc<MemoryBindingDocument> {
    fn open(&self) -> Result<Box<dyn RootBindingEdit>, String> {
        Ok(Box::new(MemoryBindingEdit {
            document: Arc::clone(self),
            current: self.read(),
        }))
    }
}

struct MemoryBindingEdit {
    document: Arc<MemoryBindingDocument>,
    current: Option<RootBindingDraft>,
}

impl RootBindingEdit for MemoryBindingEdit {
    fn current(&self) -> Option<RootBindingDraft> {
        self.current.clone()
    }

    /// The same shape as the product's `[composition]` lookup: the root
    /// binding's composition applies to the tuple it names, and every other
    /// tuple follows the catalog.
    fn composition_for(&self, account: Option<&str>, model: Option<&str>) -> CompositionSelection {
        self.current
            .as_ref()
            .filter(|draft| {
                account == Some(draft.account.as_str()) && model == Some(draft.model.as_str())
            })
            .and_then(|draft| draft.composition)
            .unwrap_or_default()
    }

    fn commit(self: Box<Self>, draft: RootBindingDraft) -> Result<(), RootBindingSaveError> {
        let cleared = draft.account.is_empty() || draft.model.is_empty();
        *self
            .document
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (!cleared).then_some(draft);
        Ok(())
    }
}

/// A binding document that refuses every save before publication.
#[derive(Debug)]
pub struct RefusingBindingDocument {
    reason: String,
}

impl RefusingBindingDocument {
    /// A document whose commits fail with `reason`, before publication.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            reason: reason.into(),
        })
    }
}

impl RootBindingDocument for Arc<RefusingBindingDocument> {
    fn open(&self) -> Result<Box<dyn RootBindingEdit>, String> {
        Ok(Box::new(RefusingBindingEdit {
            reason: self.reason.clone(),
        }))
    }
}

struct RefusingBindingEdit {
    reason: String,
}

impl RootBindingEdit for RefusingBindingEdit {
    fn current(&self) -> Option<RootBindingDraft> {
        None
    }

    fn composition_for(
        &self,
        _account: Option<&str>,
        _model: Option<&str>,
    ) -> CompositionSelection {
        CompositionSelection::default()
    }

    fn commit(self: Box<Self>, _draft: RootBindingDraft) -> Result<(), RootBindingSaveError> {
        Err(RootBindingSaveError::BeforePublication(self.reason))
    }
}

/// A document that cannot be opened at all.
#[derive(Debug)]
pub struct UnreadableBindingDocument {
    reason: String,
}

impl UnreadableBindingDocument {
    /// A document whose `open` fails with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            reason: reason.into(),
        })
    }
}

impl RootBindingDocument for Arc<UnreadableBindingDocument> {
    fn open(&self) -> Result<Box<dyn RootBindingEdit>, String> {
        Err(self.reason.clone())
    }
}

/// Durable topology rows held in memory: the tree store and the session
/// records a topology question reads, with no product home behind them.
#[derive(Default)]
pub struct MemoryRows {
    trees: crate::agent_tree::MemoryTreeStore,
    records: crate::session_store::MemoryStore,
}

impl MemoryRows {
    /// Empty rows.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session records behind these rows, for a test that seeds one.
    #[must_use]
    pub const fn records(&self) -> &crate::session_store::MemoryStore {
        &self.records
    }

    /// The trees behind these rows, for a test that seeds one.
    #[must_use]
    pub const fn trees(&self) -> &crate::agent_tree::MemoryTreeStore {
        &self.trees
    }
}

impl super::TreeRows for MemoryRows {
    fn tree(&self, tree_id: &str) -> Result<Option<crate::agent_tree::ProductAgentTree>, String> {
        use crate::agent_tree::TreeStore as _;
        Ok(self
            .trees
            .load(tree_id)
            .map_err(|error| error.detail.clone())?
            .map(crate::agent_tree::ProductAgentTree::reopen))
    }

    fn tree_exists(&self, tree_id: &str) -> Result<bool, String> {
        use crate::agent_tree::TreeStore as _;
        self.trees
            .exists(tree_id)
            .map_err(|error| error.detail.clone())
    }

    fn save_tree(&self, tree: &mut crate::agent_tree::ProductAgentTree) -> Result<(), String> {
        use crate::agent_tree::TreeStore as _;
        let acknowledged = self
            .trees
            .save(tree)
            .map_err(|error| error.detail.clone())?;
        tree.acknowledge_revision(acknowledged);
        Ok(())
    }
}

impl super::TopologyRows for MemoryRows {
    fn session(&self, session_id: &str) -> Result<crate::sessions::SessionRecord, String> {
        use crate::session_store::SessionRecordStore as _;
        self.records.load(session_id)
    }
}

/// Seed a tree that reopens `RestoredPaused`: a root with a descendant in
/// flight, saved through whichever rows the caller is testing over.
///
/// # Errors
///
/// Returns the tree crate's own message when the tree cannot be built or
/// saved.
pub fn seed_restored_paused_tree(
    rows: &dyn super::TopologyRows,
    tree_id: &str,
    workspace: &std::path::Path,
) -> Result<(), String> {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id,
        "root",
        "owner",
        workspace.to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .map_err(|error| error.message)?;
    // Only a tree with a descendant in flight reopens paused.
    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .map_err(|error| error.message)?;
    tree.restore_paused();
    rows.save_tree(&mut tree)
}

/// A detached shell host over doubles, with the two handles a test asserts on.
pub struct ShellHostFixture<
    P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core,
> {
    /// The configuration to drive.
    pub config: ShellHostConfig<P>,
    /// The root binding the handlers edit.
    pub binding: Arc<MemoryBindingDocument>,
    /// The session records the handlers commit through.
    pub store: Arc<crate::session_store::MemoryStore>,
}

/// Build a detached shell host on doubles: a recording surface, a memory
/// binding document seeded with `binding`, and a memory record store holding
/// `session`.
#[must_use]
pub fn shell_host_on_doubles<P: plakous_host_protocol::ProtocolExtension>(
    root: &std::path::Path,
    session: crate::sessions::SessionRecord,
    binding: Option<RootBindingDraft>,
) -> ShellHostFixture<P> {
    use crate::session_store::SessionRecordStore as _;

    let document =
        binding.map_or_else(MemoryBindingDocument::empty, MemoryBindingDocument::holding);
    let store = Arc::new(crate::session_store::MemoryStore::new());
    let mut session = session;
    let _ = store.save(&mut session);
    let config = ShellHostConfig {
        surface: recording_surface(),
        binding: Arc::new(Arc::clone(&document)) as Arc<dyn RootBindingDocument>,
        subscriptions: None,
        openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
        product_home: root.join("home"),
        home: SessionHome::new(root.join("home/sessions")),
        store: Arc::clone(&store) as Arc<dyn crate::session_store::SessionRecordStore>,
        session,
        cwd: root.join("workspace"),
        reason: "test".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
    };
    ShellHostFixture {
        config,
        binding: document,
        store,
    }
}

/// Spawn the detached shell loop on its own thread, the way the product's
/// entry point does, and hand back the two channel ends and the join handle.
///
/// The plumbing is the host's: a `UserAction` channel in, a `HostEvent`
/// channel out, one thread running `run_shell_host_loop`. Only the
/// configuration behind it is the product's, and on this path it is doubles.
#[must_use]
pub fn spawn_shell_host_on_doubles<P: plakous_host_protocol::ProtocolExtension>(
    config: ShellHostConfig<P>,
) -> (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    std::thread::JoinHandle<()>,
) {
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    let events = Sender::over(event_tx);
    let handle = std::thread::spawn(move || {
        super::shell::run_shell_host_loop(config, action_rx, events, None);
    });
    (action_tx, event_rx, handle)
}

/// The product's half of a shell host, on doubles.
pub struct DoublesShellHost;

impl<P: plakous_host_protocol::ProtocolExtension> ShellHostRunner<P> for DoublesShellHost {
    fn fallback_facts(&self) -> ShellFallbackFacts<P> {
        ShellFallbackFacts {
            surface: recording_surface(),
            binding: Arc::new(MemoryBindingDocument::empty()),
            subscriptions: None,
            openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
        }
    }

    fn run(
        self: Box<Self>,
        config: ShellHostConfig<P>,
        actions: crate::surface_protocol::UserActionReceiver<P>,
        events: Sender<P>,
        clear_publication: Option<Arc<Mutex<Option<String>>>>,
    ) -> crate::CliResult<()> {
        crate::runtime::shell::run_shell_host_loop(config, actions, events, clear_publication);
        Ok(())
    }
}

/// A boxed shell host over doubles, for the deferred-startup tests.
#[must_use]
pub fn doubles_shell_host<P: plakous_host_protocol::ProtocolExtension>()
-> Box<dyn ShellHostRunner<P>> {
    Box::new(DoublesShellHost)
}

/// Installs an idle owned topology hub without dispatching any provider.
#[allow(clippy::unwrap_used, reason = "deterministic topology fixture")]
pub fn install_idle_topology_hub(
    tree_id: &str,
    workspace: &std::path::Path,
) -> Arc<crate::topology_runtime::ProductTopologyRuntime> {
    crate::topology_runtime::resolve_topology_runtime(
        tree_id,
        true,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        || Ok(crate::topology_runtime::test_support::memory_store()),
        None,
        |_| {
            Ok(crate::topology_runtime::test_support::bootstrap(
                tree_id,
                workspace.to_path_buf(),
            ))
        },
    )
    .unwrap()
}

/// Installs one owned hub whose child refuses release. Injected root hosts claim
/// this lease just as the production attach does; no provider is dispatched.
#[allow(
    clippy::unwrap_used,
    reason = "deterministic test fixture construction"
)]
pub async fn install_failing_child_shutdown(
    tree_id: &str,
    workspace: &std::path::Path,
) -> Arc<crate::topology_runtime::ProductTopologyRuntime> {
    let runtime = install_idle_topology_hub(tree_id, workspace);
    let child = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    assert!(
        runtime
            .try_occupy_live(child, Box::new(FailedShutdownChild))
            .await
            .is_ok()
    );
    runtime
}

struct FailedShutdownChild;
#[allow(clippy::unwrap_used, reason = "literal fixture turn identity")]
impl crate::topology_runtime::LiveChildHandle for FailedShutdownChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        "00000000-0000-0000-0000-000000000001".parse().unwrap()
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("fixture")
    }
    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("fixture".into()) })
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("fixture".into()) })
    }
    fn release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async {
            crate::topology_runtime::LiveRelease {
                feed: None,
                locator: None,
                failure: Some("injected child release refusal".into()),
            }
        })
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.release(driver)
    }
}

/// Installs one owned hub whose child never completes release. Quit must
/// detach rather than wait out HTTP or model cleanup.
#[allow(
    clippy::unwrap_used,
    reason = "deterministic test fixture construction"
)]
pub async fn install_hanging_child_shutdown(
    tree_id: &str,
    workspace: &std::path::Path,
) -> Arc<crate::topology_runtime::ProductTopologyRuntime> {
    let runtime = install_idle_topology_hub(tree_id, workspace);
    let child = runtime
        .mutate_then_emit(false, |tree| {
            Ok(tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    None,
                )
                .unwrap()
                .agent_id)
        })
        .unwrap();
    assert!(
        runtime
            .try_occupy_live(child, Box::new(HangingShutdownChild))
            .await
            .is_ok()
    );
    runtime
}

struct HangingShutdownChild;
#[allow(clippy::unwrap_used, reason = "literal fixture turn identity")]
impl crate::topology_runtime::LiveChildHandle for HangingShutdownChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        "00000000-0000-0000-0000-000000000001".parse().unwrap()
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("fixture")
    }
    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("fixture".into()) })
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("fixture".into()) })
    }
    fn release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async {
            std::future::pending::<()>().await;
            unreachable!()
        })
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.release(driver)
    }
}

/// Returns a fixture child's released construction locator with its real feed.
pub async fn child_release_with_feed(
    sessions: &plakous::sessions::AgentSessions,
    locator: plakous::sessions::SessionLocator,
    failure: Option<String>,
) -> crate::topology_runtime::LiveRelease {
    crate::agent::live::released_with_feed(sessions, locator, failure, 0, false).await
}

/// Loads a fixture child's live feed through the production tree reader.
pub async fn child_feed(
    sessions: &plakous::sessions::AgentSessions,
    session_id: plakous::model::SessionId,
) -> Result<crate::agent_tree::TreeFeed, String> {
    crate::agent::read_tree_feed(sessions, session_id, 0)
        .await
        .map_err(String::from)
}
