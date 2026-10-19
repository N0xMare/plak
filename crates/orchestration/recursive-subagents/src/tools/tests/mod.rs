use super::*;
use crate::cell_budget::{CEILING_MAX_AGENTS_PER_CELL, DEFAULT_MAX_AGENTS_PER_CELL};
use crate::protocol::{
    CandidateId, MessagePriority, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest,
    SpawnWorkspace, TOPOLOGY_PRESENTATION_ORDER, Tier, TopologyErrorCode, WorkspaceIdRequest,
    WorkspaceMode, WorkspaceStatus, WriterMode,
};
use crate::tree::{MailDisposition, MailDurableState, MemoryTreeStore, ProductAgentTree};
use plakous::model::{PortableToolSchemaV2, ToolName, ToolResultPart};
use plakous::tools::{ToolOutcomeKind, ToolRegistryBuilder, WorkspacePolicy};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

mod budget;
mod cancel;
mod driver;
mod jobs;
mod mail;
mod registrations;
mod spawn;
mod wait;
mod workspace;

#[test]
fn topology_envelope_limit_failure_is_not_proven_no_effect() {
    let outcome = ToolOutcome::success_text("\"".repeat(350_000)).unwrap();
    let error = topology_envelope(outcome).unwrap_err();
    assert!(!error.proven_no_effect());
}

fn spawn_request() -> SpawnAgentRequest {
    SpawnAgentRequest::test_review()
}

fn bootstrap_tree(tree_id: &str, root: &Path) -> ProductAgentTree {
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id,
        "root",
        "owner",
        root.to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.pin_bind_hints_for_test(crate::epoch::BindHints::from_names(
        &[
            "codex",
            "grok",
            "openrouter",
            crate::epoch::RosterInput::TEST_ACCOUNT,
        ],
        &["openrouter"],
    ));
    tree
}

/// Pins `review` to an explicit account/tuple/effort.
fn pin_review(tree: &mut ProductAgentTree, account: &str, tuple: &str, effort: Option<&str>) {
    let mut binding = RosterBinding::leaf(account, tuple);
    binding.effort = effort.map(str::to_owned);
    binding.max_tier = Tier::ReadOnly;
    tree.pin_template("review", binding);
}

fn child_path(tree: &Arc<Mutex<ProductAgentTree>>, agent_id: &str) -> std::path::PathBuf {
    tree.lock().unwrap().node(agent_id).unwrap().path.clone()
}

/// A tree published once, plus the store handle the test's driver
/// publishes through.
///
/// The store is the port, not a file: what these tests assert is that the
/// tree published its rows and that they load back. Which implementation
/// backs the port is the conformance suite's business, and it runs the
/// same contract over the memory store and over SQLite.
fn persist_fixture(root: &tempfile::TempDir) -> (Arc<dyn TreeStore>, ProductAgentTree) {
    let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
    let mut tree = bootstrap_tree("tree1", root.path());
    tree.acknowledge_revision(store.save(&tree).unwrap());
    (store, tree)
}

/// The tree as the store holds it, with the reopen law applied.
fn reload(store: &Arc<dyn TreeStore>, tree_id: &str) -> ProductAgentTree {
    ProductAgentTree::reopen(store.load(tree_id).unwrap().unwrap())
}

/// The driver a tool runs on: the tree, `store`, and the tree's own port.
fn driver_on(tree: ProductAgentTree, store: Arc<dyn TreeStore>) -> TreeDriver {
    let vcs = tree.vcs_port();
    TreeDriver::new(Arc::new(Mutex::new(tree)), store, vcs, None)
}

struct StubBinder {
    binds: AtomicUsize,
    result: Result<SpawnBound, SpawnBindError>,
}

impl StubBinder {
    fn ok() -> Self {
        Self {
            binds: AtomicUsize::new(0),
            result: Ok(SpawnBound {
                durably_registered: false,
                session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                turn_id: Some("turn-c1".into()),
            }),
        }
    }

    /// Binds a session without a started turn: the shape mail `Started`
    /// still serves when a bound child has no turn (recovered reopen).
    fn bind_only() -> Self {
        Self {
            binds: AtomicUsize::new(0),
            result: Ok(SpawnBound {
                durably_registered: false,
                session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                turn_id: None,
            }),
        }
    }

    fn fail() -> Self {
        Self {
            binds: AtomicUsize::new(0),
            result: Err("child session could not be created".into()),
        }
    }
}

impl SpawnedSessionBinder for StubBinder {
    fn bind(
        &self,
        start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
        self.binds.fetch_add(1, Ordering::SeqCst);
        assert!(
            !start.prompt.is_empty(),
            "spawn start carries the prompt brief"
        );
        assert_eq!(start.turn_token, 1);
        let result = self.result.clone();
        Box::pin(async move { result })
    }
}

struct RelockingBinder {
    tree: Arc<Mutex<ProductAgentTree>>,
    binds: AtomicUsize,
}

impl SpawnedSessionBinder for RelockingBinder {
    fn admit_spawn(&self, request: &SpawnAgentRequest) -> Result<(), TopologyError> {
        let tree = self.tree.lock().map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "topology tree lock is poisoned",
            )
        })?;
        tree.snapshot
            .binding(&request.template)
            .map(|_| ())
            .ok_or_else(|| {
                TopologyError::new(
                    TopologyErrorCode::TemplateUnknown,
                    "template is not in this tree epoch's roster",
                )
            })
    }

    fn bind(
        &self,
        _start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
        self.binds.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(SpawnBound {
                durably_registered: false,
                session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                turn_id: None,
            })
        })
    }
}

/// A budget nothing in these fixtures shares, so a test that does not
/// exercise the bound cannot be perturbed by one that does.
fn test_budget() -> Arc<CellAgentBudget> {
    Arc::new(CellAgentBudget::new(DEFAULT_MAX_AGENTS_PER_CELL))
}

fn spawn_tool(driver: TreeDriver, binder: Option<Arc<dyn SpawnedSessionBinder>>) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Spawn,
        vocabulary: TopologyVocabulary::default(),
        binder,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn wait_tool(driver: TreeDriver) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Wait,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn send_tool(driver: TreeDriver, deliverer: Option<Arc<dyn MailDeliverer>>) -> TopologyTool {
    struct ProjectedDeliverer {
        inner: Arc<dyn MailDeliverer>,
        driver: TreeDriver,
    }
    impl MailDeliverer for ProjectedDeliverer {
        fn withdraw_followup(
            &self,
            driver: TreeDriver,
            agent_id: String,
            message_id: String,
            receipt: Option<(String, TopologyErrorCode)>,
        ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
            self.inner
                .withdraw_followup(driver, agent_id, message_id, receipt)
        }
        fn deliver(
            &self,
            request: MailDeliveryRequest,
        ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>
        {
            Box::pin(async move {
                use plakous::sessions::feed::{FeedFact, FeedRecord};
                let ack = self.inner.deliver(request.clone()).await?;
                let command = format!("fixture:mail:{}", request.message_id);
                self.driver.mutate_and_settle(|tree| {
                    let session = tree.node(&request.agent_id)?.session_locator.as_deref()
                        .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
                        .map_or_else(|| "00000000-0000-4000-8000-000000000001".to_owned(), |locator| locator.session_id().to_string());
                    let seq = tree.feed_cursor(&session) + 1;
                    let header = serde_json::from_value(json!({
                        "session_id": session, "seq": seq,
                        "branch_id": "00000000-0000-4000-8000-000000000002", "committed_at_ms": seq,
                        "change": {"created": {"genesis": "00000000-0000-4000-8000-000000000003", "revision": seq}}
                    })).unwrap();
                    tree.record_command(crate::tree::AgentCommand::intent(&command, &request.agent_id, None,
                        crate::tree::AgentCommandKind::MailStart, request.body.clone(), vec![request.message_id.clone()]))?;
                    let record = FeedRecord::from_store(header, vec![FeedFact::CommandAccepted {
                        command_id: plakous::agent::CommandId::new(&command).unwrap(),
                        kind: plakous::sessions::feed::FeedCommandKind::Root { origin: plakous::model::TurnOrigin::UserPrompt },
                        turn_id: None, sequence: seq, operation_id: None,
                    }]);
                    tree.project_feed(&request.agent_id, &session, &[record], &crate::tree::TreeFeedReads::default()).map_err(TopologyError::from)?;
                    Ok::<_, TopologyError>(())
                }).map_err(|error| MailDeliveryFailure::RecoveryRequired(error.message))?.0
                    .map_err(|error| MailDeliveryFailure::RecoveryRequired(error.message))?;
                Ok(ack)
            })
        }
    }
    let deliverer = deliverer.map(|inner| {
        Arc::new(ProjectedDeliverer {
            inner,
            driver: driver.clone(),
        }) as Arc<dyn MailDeliverer>
    });
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Send,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer,
        canceller: None,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn interrupt_tool(
    driver: TreeDriver,
    canceller: Option<Arc<dyn LiveTurnCanceller>>,
) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Interrupt,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn close_tool(driver: TreeDriver, canceller: Option<Arc<dyn LiveTurnCanceller>>) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Close,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn adopt_tool(driver: TreeDriver) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Adopt,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

/// The registry a caller's granted registrations compile into. What the
/// host wraps them in — an extension set, an agreement pack — is the
/// host's; the tree's output is the registrations.
fn registry_for(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
) -> plakous::tools::FrozenToolRegistry {
    let mut builder = ToolRegistryBuilder::default();
    for registration in topology_tool_registrations(tree, caller_id).unwrap() {
        builder.register(registration).unwrap();
    }
    builder.freeze()
}

/// The names a caller is granted, in registration order.
fn granted_names(tree: Arc<Mutex<ProductAgentTree>>, caller_id: impl Into<String>) -> Vec<String> {
    topology_tool_registrations(tree, caller_id)
        .unwrap()
        .iter()
        .map(|registration| registration.name().as_str().to_owned())
        .collect()
}

/// A [`TopologyObserver`] that records what the tree told it. The product's
/// mapping onto its own telemetry vocabulary is asserted where that
/// mapping lives (`plak-host`'s `topology_telemetry`); what the tree owes
/// is that it reports the spawn and the detach at all, exactly once.
/// One reported spawn: tree, node, parent, template.
type SpawnReport = (String, String, Option<String>, String);

/// One reported detach: tree, node, status.
type DetachReport = (String, String, NodeDetach);

#[derive(Default)]
struct RecordingObserver {
    spawned: Mutex<Vec<SpawnReport>>,
    detached: Mutex<Vec<DetachReport>>,
}

impl TopologyObserver for RecordingObserver {
    fn turn_usage(&self, _terminal: crate::tree::ProjectedChildTurnUsage) {}
    fn node_spawned(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        template: &str,
    ) {
        self.spawned.lock().unwrap().push((
            tree_id.to_owned(),
            node_id.to_owned(),
            parent_node_id.map(str::to_owned),
            template.to_owned(),
        ));
    }

    fn node_detached(
        &self,
        tree_id: &str,
        node_id: &str,
        _parent_node_id: Option<&str>,
        status: NodeDetach,
    ) {
        self.detached
            .lock()
            .unwrap()
            .push((tree_id.to_owned(), node_id.to_owned(), status));
    }

    fn epoch_advanced(&self, _tree_id: &str) {}
}

fn list_tool(driver: TreeDriver) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::List,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn discard_tool(driver: TreeDriver) -> TopologyTool {
    TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Discard,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: None,
        cell_budget: test_budget(),
    }
}

fn workspace_request(workspace_id: &str) -> Value {
    serde_json::to_value(WorkspaceIdRequest {
        workspace_id: workspace_id.into(),
    })
    .unwrap()
}

fn mint_spawn_request() -> SpawnAgentRequest {
    let mut request =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "implementer", "edit the parser");
    request.workspace = SpawnWorkspace::Fork;
    request
}

fn git_repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "plak@example.test"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "plak"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "SEED.txt"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    root
}

fn git_persist_fixture() -> (tempfile::TempDir, Arc<dyn TreeStore>, ProductAgentTree) {
    let root = git_repo();
    let (store, tree) = persist_fixture(&root);
    (root, store, tree)
}

fn jj_repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
        Command::new("jj")
            .args(["--no-pager", "git", "init"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local"
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
    assert!(
        Command::new("jj")
            .args(["--no-pager", "commit", "-m", "seed"])
            .args([
                "--config",
                "user.name=plak",
                "--config",
                "user.email=plak@local"
            ])
            .env("JJ_CONFIG", "/dev/null")
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    root
}

fn jj_persist_fixture() -> (tempfile::TempDir, Arc<dyn TreeStore>, ProductAgentTree) {
    let root = jj_repo();
    let (store, tree) = persist_fixture(&root);
    (root, store, tree)
}

fn send_request(agent_id: &str, message: &str, priority: Option<MessagePriority>) -> Value {
    serde_json::to_value(SendAgentMessageRequest {
        agent_id: agent_id.into(),
        message: message.into(),
        purpose: None,
        priority,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap()
}

fn outcome_json(outcome: &ToolOutcome) -> Value {
    let ToolResultPart::Text(block) = &outcome.parts()[0] else {
        panic!("expected text tool outcome");
    };
    serde_json::from_str(block.text()).unwrap()
}

struct StubDeliverer {
    calls: Mutex<Vec<MailDeliveryRequest>>,
    result: Result<MailDeliveryAck, MailDeliveryFailure>,
    /// Answers for the first calls, in order, before `result` repeats.
    first: Mutex<std::collections::VecDeque<Result<MailDeliveryAck, MailDeliveryFailure>>>,
    /// Calls from this index on answer only once the test adds a permit.
    hold: Option<(usize, Arc<tokio::sync::Semaphore>)>,
    /// The tree an admitted Start binds its turn on at admission, as the
    /// product deliverer does (`MailDeliverer`).
    tree: Option<Arc<Mutex<ProductAgentTree>>>,
}

impl StubDeliverer {
    fn ok() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            first: Mutex::default(),
            hold: None,
            tree: None,
            result: Ok(MailDeliveryAck {
                turn_id: Some("turn-1".into()),
            }),
        }
    }

    fn fail() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            first: Mutex::default(),
            hold: None,
            tree: None,
            result: Err("child turn could not be admitted".into()),
        }
    }

    fn turn_ended() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            first: Mutex::default(),
            hold: None,
            tree: None,
            result: Err(MailDeliveryFailure::TurnEnded),
        }
    }

    /// The turn ends under the first delivery; later ones start a turn.
    fn turn_ended_then_ok() -> Self {
        let stub = Self::ok();
        stub.first
            .lock()
            .unwrap()
            .push_back(Err(MailDeliveryFailure::TurnEnded));
        stub
    }

    /// The turn ends under the first delivery; the restart after it fails.
    fn turn_ended_then_fail() -> Self {
        let stub = Self::fail();
        stub.first
            .lock()
            .unwrap()
            .push_back(Err(MailDeliveryFailure::TurnEnded));
        stub
    }

    /// Holds every call from `index` on until the returned gate gets a
    /// permit per call.
    fn holding_from(mut self, index: usize) -> (Self, Arc<tokio::sync::Semaphore>) {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.hold = Some((index, Arc::clone(&gate)));
        (self, gate)
    }

    /// Admitted Starts bind their turn on `tree` and mark it active
    /// as the call returns, as the product deliverer does.
    fn binding(mut self, tree: &Arc<Mutex<ProductAgentTree>>) -> Self {
        self.tree = Some(Arc::clone(tree));
        self
    }

    fn refuse() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            first: Mutex::default(),
            hold: None,
            tree: None,
            result: Err(MailDeliveryFailure::Refused(
                "child turn was refused before admission".into(),
            )),
        }
    }
}

impl MailDeliverer for StubDeliverer {
    fn withdraw_followup(
        &self,
        driver: TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
        Box::pin(async move {
            let (session, tail) = driver.read(|tree| {
                let session = tree
                    .node(&agent_id)
                    .unwrap()
                    .session_locator
                    .as_ref()
                    .unwrap()
                    .parse::<plakous::sessions::SessionLocator>()
                    .unwrap()
                    .session_id()
                    .to_string();
                let tail = tree.feed_cursor(&session);
                (session, tail)
            })?;
            driver.withdraw_unstarted_followup(&agent_id, &message_id, &session, tail, receipt)
        })
    }
    fn deliver(
        &self,
        request: MailDeliveryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>
    {
        let started =
            (request.disposition == MailDisposition::Started).then(|| request.agent_id.clone());
        let index = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(request);
            calls.len() - 1
        };
        let result = self
            .first
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.result.clone());
        let gate = self
            .hold
            .as_ref()
            .filter(|(from, _)| index >= *from)
            .map(|(_, gate)| Arc::clone(gate));
        let tree = self.tree.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            // The Start is admitted as the call returns.
            if let (
                Some(tree),
                Some(agent),
                Ok(MailDeliveryAck {
                    turn_id: Some(turn),
                }),
            ) = (tree, started, &result)
            {
                let mut tree = tree.lock().unwrap();
                tree.set_active_turn(&agent, true).unwrap();
                tree.bind_attempt_turn(&agent, turn.clone()).unwrap();
            }
            result
        })
    }
}

struct StubCanceller {
    calls: Mutex<Vec<Vec<String>>>,
    fail_requested: bool,
    fail_empty_ids: bool,
}

impl StubCanceller {
    fn ok() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_requested: false,
            fail_empty_ids: false,
        }
    }

    fn fail() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_requested: true,
            fail_empty_ids: false,
        }
    }

    fn fail_without_ids() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_requested: false,
            fail_empty_ids: true,
        }
    }
}

impl LiveTurnCanceller for StubCanceller {
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
    {
        self.calls.lock().unwrap().push(agent_ids.clone());
        let result = if self.fail_empty_ids {
            Err(LiveCancelFailure {
                message: "child turn could not be cancelled".into(),
                failed_ids: Vec::new(),
                joiner_expired_ids: Vec::new(),
                owner_failures: Vec::new(),
                cancelled_ids: Vec::new(),
                not_live_ids: agent_ids,
            })
        } else if self.fail_requested {
            Err(LiveCancelFailure {
                message: "child turn could not be cancelled".into(),
                owner_failures: agent_ids
                    .iter()
                    .map(|id| crate::hub::LiveRetirementFailure {
                        agent_id: id.clone(),
                        expected_attempt_id: Some(crate::protocol::AttemptId::new(id, 1)),
                        message: "child turn could not be cancelled".into(),
                    })
                    .collect(),
                failed_ids: agent_ids,
                joiner_expired_ids: Vec::new(),
                cancelled_ids: Vec::new(),
                not_live_ids: Vec::new(),
            })
        } else {
            Ok(LiveCancelOutcome {
                cancelled_ids: agent_ids,
                not_live_ids: Vec::new(),
            })
        };
        Box::pin(async move { result })
    }
}

fn agent_id_request(agent_id: &str) -> Value {
    serde_json::to_value(AttemptScopedRequest {
        agent_id: agent_id.into(),
        expected_attempt_id: None,
    })
    .unwrap()
}

/// A child `c1` whose turn the parent interrupted and which is still
/// unwinding: bound, active, and `interrupted`.
async fn interrupted_unwinding_child(driver: &TreeDriver) {
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-bind".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let mut tree = driver.tree.lock().unwrap();
    tree.set_active_turn("c1", true).unwrap();
    if tree.attempt("c1").unwrap().turn_id.is_none() {
        tree.bind_attempt_turn("c1", "00000000-0000-0000-0000-000000000001")
            .unwrap();
    }
    tree.interrupt_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: "c1".into(),
            expected_attempt_id: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
    // The cancelled turn's waiter is still running.
    tree.enter_turn_waiter("c1");
}

/// The interrupted turn's waiter releases the cancelled turn and ends.
fn release_unwound_turn(tree: &Mutex<ProductAgentTree>) {
    let mut tree = tree.lock().unwrap();
    tree.release_attempt_turn("c1").unwrap();
    tree.release_turn_owner("c1");
    tree.set_active_turn("c1", false).unwrap();
    tree.leave_turn_waiter("c1");
}

async fn until_calls(deliverer: &StubDeliverer, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while deliverer.calls.lock().unwrap().len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for delivery {count}"));
}

#[derive(Default)]
struct FixtureJobHost(Mutex<Option<plakous::tools::JoinedProcessSessions>>);

impl plakous::tools::JobHost for FixtureJobHost {
    fn service(
        &self,
        _: plakous::model::SessionId,
        _: plakous::model::BranchId,
    ) -> Option<plakous::tools::JoinedProcessSessions> {
        self.0.lock().unwrap().clone()
    }
    fn attach(
        &self,
        _: plakous::tools::JobOwner,
        fresh: plakous::tools::JoinedProcessSessions,
    ) -> Result<plakous::tools::JoinedProcessSessions, plakous::tools::ExecutionBackendError> {
        *self.0.lock().unwrap() = Some(fresh.clone());
        Ok(fresh)
    }
}

async fn install_running_job(
    driver: &TreeDriver,
    caller: &str,
    root: &Path,
) -> (
    plakous::tools::JoinedProcessSessions,
    plakous::sessions::OpenedSession,
) {
    use plakous::sessions::*;
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.join("job-evidence.jsonl"),
    )
    .unwrap();
    let plan = Arc::new(serde_json::from_value(json!({"version": plakous_offline_crash_harness::script::SCRIPT_VERSION, "process_tools": true, "steps": [
        {"output": {"tool": {"name": "process_session_start", "arguments": {"command": "read line", "workdir": ".", "login": false, "tty": false, "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192}}}, "finish": "tool_calls"},
        {"output": {"text": "ready"}}
    ]})).unwrap());
    let host = Arc::new(FixtureJobHost::default());
    let runtime = scripted_runtime_binding(root, controller, plan)
        .unwrap()
        .with_job_host(host.clone());
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(root.join("job-runtime.sqlite3")).unwrap())
            .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("fixture-job:create").unwrap(),
            runtime,
        ))
        .await
        .unwrap()
    else {
        panic!("created job session required")
    };
    session
        .agent()
        .prompt("start a job")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    let jobs = host.0.lock().unwrap().clone().unwrap();
    assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
    driver
        .tree
        .lock()
        .unwrap()
        .bind_session_locator(caller, session.locator().to_string())
        .unwrap();
    driver.bind_job_host(Some(host));
    (jobs, session)
}

struct FailingStore;

impl TreeStore for FailingStore {
    fn exists(&self, _tree_id: &str) -> Result<bool, crate::tree::TreeStoreError> {
        Ok(false)
    }

    fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
        Ok(Vec::new())
    }

    fn save(
        &self,
        _tree: &ProductAgentTree,
    ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
        Err(crate::tree::TreeStoreError::unavailable(
            "the test store refuses every save",
        ))
    }

    fn load(
        &self,
        _tree_id: &str,
    ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
        Ok(None)
    }
}

/// The lineage L0 stamps on a nested call from a Code Mode cell.
///
/// `execute_local_composition_nested` builds exactly this
/// (`crates/plakous/agent/src/runtime/body.rs:21157`) for `native-ptc`,
/// `js-ptc` and `py-ptc` alike; only `cell_id` differs between them.
fn cell_lineage(cell_id: &str, sequence: u32) -> ToolCallerLineage {
    ToolCallerLineage::local_programmatic(
        41,
        7,
        cell_id,
        sequence,
        <plakous::model::CallId as std::str::FromStr>::from_str(
            &uuid::Uuid::from_u128(u128::from(sequence) + 900).to_string(),
        )
        .unwrap(),
        3,
    )
    .unwrap()
}

fn task_tool(driver: TreeDriver, kind: TopologyKind) -> TopologyTool {
    let mut tool = wait_tool(driver);
    tool.kind = kind;
    tool.vocabulary = TopologyVocabulary::TaskTools;
    tool
}
