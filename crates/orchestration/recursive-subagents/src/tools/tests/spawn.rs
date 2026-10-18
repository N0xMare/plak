//! Spawn admission, binding and minting.

use super::*;

#[tokio::test]
async fn spawn_admit_may_relock_hub_tree_without_deadlock() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let binder = Arc::new(RelockingBinder {
        tree: Arc::clone(&tree),
        binds: AtomicUsize::new(0),
    });
    let tool = spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tool.spawn_with("op-spawn-relock".into(), &arguments),
    )
    .await
    .expect("spawn deadlocked on nested tree lock")
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
}

struct NotReadyBinder {
    binds: AtomicUsize,
}

struct ChangingReadinessBinder {
    ready: AtomicBool,
    admissions: AtomicUsize,
    binds: AtomicUsize,
}

impl SpawnedSessionBinder for ChangingReadinessBinder {
    fn admit_binding<'a>(
        &'a self,
        _template: &'a TemplateName,
        _binding: &'a RosterBinding,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            if self.ready.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(TopologyError::new(
                    TopologyErrorCode::ModelNotAdmitted,
                    "account is no longer Ready",
                ))
            }
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
                turn_id: Some("turn-c1".into()),
            })
        })
    }
}

#[tokio::test]
async fn spawn_receipt_replays_after_account_readiness_changes() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let binder = Arc::new(ChangingReadinessBinder {
        ready: AtomicBool::new(true),
        admissions: AtomicUsize::new(0),
        binds: AtomicUsize::new(0),
    });
    let tool = spawn_tool(
        driver,
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    let first = tool
        .spawn_with("ready-replay".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(first.kind(), ToolOutcomeKind::Success);
    binder.ready.store(false, Ordering::SeqCst);
    let replay = tool
        .spawn_with("ready-replay".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&first), outcome_json(&replay));
    assert_eq!(binder.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);

    let mut different = spawn_request();
    different.description.push_str(" changed");
    let conflict = tool
        .spawn_with(
            "ready-replay".into(),
            &serde_json::to_value(different).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conflict.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&conflict)["code"],
        "operation_recovery_required"
    );
    assert_eq!(binder.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
}

impl SpawnedSessionBinder for NotReadyBinder {
    fn admit_binding<'a>(
        &'a self,
        _template: &'a TemplateName,
        _binding: &'a RosterBinding,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
        Box::pin(async {
            Err(TopologyError::new(
                TopologyErrorCode::ModelNotAdmitted,
                "template review binds account codex, which is not signed in and Ready",
            ))
        })
    }

    fn bind(
        &self,
        _start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
        self.binds.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(SpawnBindError::Refused("unreachable".into())) })
    }
}

/// An account that is not Ready is refused before the tree admits a
/// node: no node, attempt, or bind is spent, and the tree stays live.
#[tokio::test]
async fn a_not_ready_account_is_refused_before_any_node_is_admitted() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let binder = Arc::new(NotReadyBinder {
        binds: AtomicUsize::new(0),
    });
    let tool = spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    let outcome = tool
        .spawn_with(
            "op-spawn-not-ready".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&outcome)["code"], "model_not_admitted");
    assert_eq!(binder.binds.load(Ordering::SeqCst), 0);
    let tree = tree.lock().unwrap();
    assert!(tree.node("c1").is_err(), "no node was admitted");
    assert_eq!(tree.state, crate::protocol::TreeAdmissionState::Live);
}

/// Counts every binder entry. An off-menu spawn must not reach either.
struct MenuProbeBinder {
    admits: AtomicUsize,
}

impl SpawnedSessionBinder for MenuProbeBinder {
    fn admit_spawn(&self, _request: &SpawnAgentRequest) -> Result<(), TopologyError> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        Err(TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            "template account is not a supported tree account",
        ))
    }

    fn admit_binding<'a>(
        &'a self,
        _template: &'a TemplateName,
        _binding: &'a RosterBinding,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(TopologyError::new(
                TopologyErrorCode::ModelNotAdmitted,
                "template binds an account which is not signed in and Ready",
            ))
        })
    }

    fn bind(
        &self,
        _start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(SpawnBindError::Refused("unreachable".into())) })
    }
}

/// Puts `nested` in the epoch only because `orchestrate` names it, not on
/// the root's working set, and narrows that card so it does not name
/// `review`.
fn closure_only_nested(tree: &mut ProductAgentTree) {
    let name = |raw: &str| TemplateName::parse(raw).unwrap();
    let mut nested = RosterBinding::leaf(crate::epoch::RosterInput::TEST_ACCOUNT, "selected-tuple");
    nested.template = "nested".into();
    tree.snapshot.roster.insert(name("nested"), nested);
    tree.snapshot
        .roster
        .get_mut(&name("orchestrate"))
        .unwrap()
        .spawns = vec![name("nested")];
    tree.snapshot.digest = tree.snapshot.compute_digest();
}

/// The root naming a closure-only template is `template_not_spawnable`
/// before a not-Ready binder runs.
#[tokio::test]
async fn root_off_menu_spawn_refuses_before_the_binder_runs() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    closure_only_nested(&mut tree);
    let driver = driver_on(tree, Arc::clone(&store));
    let binder = Arc::new(MenuProbeBinder {
        admits: AtomicUsize::new(0),
    });
    let tool = spawn_tool(
        driver,
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    let request =
        SpawnAgentRequest::test_named("nested", Tier::Mutate, "nested", "do the nested work");
    let outcome = tool
        .spawn_with(
            "op-root-off-menu".into(),
            &serde_json::to_value(request).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&outcome)["code"], "template_not_spawnable");
    assert_eq!(binder.admits.load(Ordering::SeqCst), 0);
}

/// A child naming a working-set template its card omits is
/// `template_not_spawnable`, and the binder still does not run.
#[tokio::test]
async fn child_off_menu_spawn_refuses_before_the_binder_runs() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    closure_only_nested(&mut tree);
    tree.grants_mut_for_test("root").max_depth = 3;
    let supervisor = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let binder = Arc::new(MenuProbeBinder {
        admits: AtomicUsize::new(0),
    });
    let tool = spawn_tool(
        driver,
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    *tool.caller_id.lock().unwrap() = supervisor.agent_id;
    let outcome = tool
        .spawn_with(
            "op-child-off-menu".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&outcome)["code"], "template_not_spawnable");
    assert_eq!(binder.admits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn spawn_binder_binds_locator_and_skips_replay() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let binder = Arc::new(StubBinder::ok());
    let tool = spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    let first = tool
        .spawn_with("op-spawn-bind".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(first.kind(), ToolOutcomeKind::Success);
    let first_json = outcome_json(&first);
    assert!(first_json.get("session_locator").is_none());
    assert!(first_json.get("account").is_none());
    assert!(first_json.get("tuple_id").is_none());
    assert!(first_json.get("effort").is_none());
    assert_eq!(first_json["agent_id"], "c1");
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
    {
        let tree = tree.lock().unwrap();
        let child = tree.node("c1").unwrap();
        assert_eq!(
            child.session_locator.as_deref(),
            Some(
                "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
            )
        );
        assert_ne!(child.status, NodeStatus::RecoveryRequired);
    }
    let replay = tool
        .spawn_with("op-spawn-bind".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    assert!(outcome_json(&replay).get("session_locator").is_none());
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.node("c1").unwrap().session_locator.as_deref(),
        Some(
            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
        )
    );
}

#[tokio::test]
async fn binder_accepts_only_exact_previously_published_locator() {
    struct PublishedBinder {
        driver: TreeDriver,
        same: bool,
    }
    impl SpawnedSessionBinder for PublishedBinder {
        fn bind(
            &self,
            start: SpawnStart,
        ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
            Box::pin(async move {
                let mut tree = self.driver.tree.lock().unwrap();
                tree.record_child_session(&start.spawned.agent_id, "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001")
                    .unwrap();
                let session = "00000000-0000-4000-8000-0000000000c1";
                tree.project_feed(
                    &start.spawned.agent_id,
                    session,
                    &[],
                    &crate::tree::TreeFeedReads::default(),
                )
                .unwrap();
                self.driver.save(&mut tree).unwrap();
                Ok(SpawnBound {
                    durably_registered: false,
                    session_locator: if self.same {
                        "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
                    } else {
                        "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003002"
                    }
                    .into(),
                    turn_id: None,
                })
            })
        }
    }
    for same in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::new(PublishedBinder {
                driver: driver.clone(),
                same,
            })),
        );
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        let outcome = tool
            .spawn_with("published-spawn".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(
            outcome.kind(),
            if same {
                ToolOutcomeKind::Success
            } else {
                ToolOutcomeKind::Failure
            }
        );
        assert_eq!(
            driver
                .tree
                .lock()
                .unwrap()
                .node("c1")
                .unwrap()
                .session_locator
                .as_deref(),
            Some(
                "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
            )
        );
    }
}

#[tokio::test]
async fn unresolved_spawn_is_durable_unsealed_and_never_rebound_on_replay() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let binder = Arc::new(StubBinder {
        binds: AtomicUsize::new(0),
        result: Err(SpawnBindError::RecoveryRequired {
            stage: crate::hub::ChildStartupStage::Starting,
            message: "acknowledgement delayed; owner retained".into(),
        }),
    });
    let tool = spawn_tool(driver.clone(), Some(binder.clone()));
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    assert_eq!(
        tool.spawn_with("uncertain-spawn".into(), &arguments)
            .await
            .unwrap()
            .kind(),
        ToolOutcomeKind::Failure
    );
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(loaded.attempt("c1").unwrap().candidate_id.is_none());
    let _ = tool
        .spawn_with("uncertain-spawn".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spawn_replay_restores_running_after_bound_recovery() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let tool = spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    );
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    tool.spawn_with("op-spawn-bind".into(), &arguments)
        .await
        .unwrap();
    {
        let mut tree = tree.lock().unwrap();
        tree.record_host_materialization("c1")
            .and_then(|value| tree.fold_due("c1").map(|_| value))
            .unwrap();
        assert_eq!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }
    let replay = tool
        .spawn_with("op-spawn-bind".into(), &arguments)
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    let tree = tree.lock().unwrap();
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
    assert_eq!(
        tree.node("c1").unwrap().session_locator.as_deref(),
        Some(
            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
        )
    );
    let expected = tree.attempt("c1").unwrap().clone();
    let mut rebuilt = reload(&store, "tree1");
    rebuilt.clear_feed_views();
    rebuilt
        .project_feed(
            "c1",
            &rebuilt.tree_id.clone(),
            &[],
            &crate::tree::TreeFeedReads::default(),
        )
        .unwrap();
    assert_eq!(rebuilt.attempt("c1").unwrap(), &expected);
    rebuilt.record_fault("c1", "second materialization failure");
    rebuilt.record_host_materialization("c1").unwrap();
    rebuilt.fold_due("c1").unwrap();
    assert_eq!(
        rebuilt.attempt("c1").unwrap().status,
        crate::protocol::AttemptStatus::RecoveryRequired
    );
}

/// Spawn is the only carrier of the prompt (mail `delegate` is
/// refused), so the child's assignment turn is running when spawn returns.
/// Mail that follows is a follow-up on that turn, never a second Start.
#[tokio::test]
async fn spawn_starts_the_assignment_turn_and_later_mail_follows_up() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let tool = spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    );
    let outcome = tool
        .spawn_with(
            "op-spawn-start".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    {
        let mut tree = tree.lock().unwrap();
        let child = tree.node("c1").unwrap();
        assert_eq!(child.status, NodeStatus::Running);
        assert!(
            !child.active_turn,
            "a construction turn is not feed evidence"
        );
        assert!(tree.has_turn_owner("c1"));
        assert_eq!(tree.attempt("c1").unwrap().turn_id.as_deref(), None);
        let mail = tree
            .send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: "c1".into(),
                    message: "also check the tests".into(),
                    purpose: None,
                    priority: Some(MessagePriority::Deferred),
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(mail.disposition, MailDisposition::Queued);
    }
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.attempt("c1").unwrap().turn_id.as_deref(),
        None,
        "a construction turn never becomes a durable binding"
    );
}

/// A binder refusal proves no session exists: the node is an ordinary
/// failed spawn, the replay repeats the refusal, and the tree stays open.
#[tokio::test]
async fn spawn_binder_refusal_fails_the_node_without_a_recovery_fence() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let binder = Arc::new(StubBinder::fail());
    let tool = spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    let outcome = tool
        .spawn_with(
            "op-spawn-fail".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
    {
        let tree = tree.lock().unwrap();
        let child = tree.node("c1").unwrap();
        assert!(child.session_locator.is_none());
        assert_eq!(child.status, NodeStatus::Failed);
        assert_eq!(tree.state, crate::protocol::TreeAdmissionState::Live);
    }
    let loaded = reload(&store, "tree1");
    assert_eq!(loaded.node("c1").unwrap().status, NodeStatus::Failed);
    assert_eq!(
        loaded.operations()["op-spawn-fail"].terminal_code,
        Some(TopologyErrorCode::ModelNotAdmitted),
        "an exact replay repeats the refusal instead of binding again"
    );
    assert!(loaded.node("c1").unwrap().session_locator.is_none());
}

/// Through the tool: a `mutate` ask on a read-only template
/// clips, and the wire receipt says so.
#[tokio::test]
async fn spawn_tool_clips_tier_to_the_template_ceiling() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let mut request = spawn_request();
    request.tier = Tier::Mutate;
    let outcome = spawn_tool(driver.clone(), None)
        .spawn_with("op-clip".into(), &serde_json::to_value(request).unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let json = outcome_json(&outcome);
    assert_eq!(json["tier"], "read_only");
    assert_eq!(json["workspace"]["mode"], "inherit");
    assert_eq!(json["attempt_id"], "c1-a1");
    assert!(json.get("path").is_none() && json.get("description").is_none());
}

/// `workspace=fork` on `execute` mints a snapshot copy and persists the
/// base it was minted from.
#[tokio::test]
async fn spawn_tool_execute_tier_mints_a_snapshot_copy() {
    let (_root, store, tree) = git_persist_fixture();
    let driver = driver_on(tree, Arc::clone(&store));
    let mut request = mint_spawn_request();
    request.tier = Tier::Execute;
    request.workspace = SpawnWorkspace::Fork;
    let outcome = spawn_tool(driver.clone(), None)
        .spawn_with("op-exec".into(), &serde_json::to_value(request).unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let json = outcome_json(&outcome);
    assert_eq!(json["tier"], "execute");
    assert_eq!(json["workspace"]["mode"], "snapshot");
    assert_eq!(
        json["workspace"]["base"].as_str().map(str::len),
        Some(40),
        "{json}"
    );
    let loaded = reload(&store, "tree1");
    assert_eq!(loaded.nodes().len(), 2);
    let workspace_id = json["workspace"]["id"].as_str().unwrap();
    let row = loaded.workspace(workspace_id).unwrap();
    assert_eq!(row.mode, WorkspaceMode::Snapshot);
    assert_eq!(row.base_commit.as_deref().map(str::len), Some(40));
}

/// the template's `on_mint` runs once in the new copy; a
/// non-zero exit unmints it and fails the spawn with `mint_hook_failed`.
#[tokio::test]
async fn spawn_tool_runs_on_mint_and_fails_closed_on_non_zero() {
    let (_root, store, mut tree) = git_persist_fixture();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    binding.on_mint = Some("echo minted > MARK.txt".into());
    tree.pin_template("implement", binding.clone());
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let outcome = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-hook-ok".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.kind(),
        ToolOutcomeKind::Success,
        "{}",
        outcome_json(&outcome)
    );
    let first = outcome_json(&outcome)["agent_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let first_path = child_path(&tree, &first);
    assert_eq!(
        std::fs::read_to_string(first_path.join("MARK.txt")).unwrap(),
        "minted\n"
    );

    binding.on_mint = Some("echo broken >&2; exit 1".into());
    tree.lock().unwrap().pin_template("implement", binding);
    let outcome = spawn_tool(driver.clone(), None)
        .spawn_with(
            "op-hook-fail".into(),
            &serde_json::to_value(mint_spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure, "{outcome:?}");
    let json = outcome_json(&outcome);
    assert_eq!(json["code"], "mint_hook_failed");
    assert!(
        json["message"].as_str().unwrap().contains("broken"),
        "{json}"
    );
    let stored = tree.lock().unwrap();
    let failed = stored
        .nodes()
        .values()
        .find(|node| node.agent_id != first && node.parent_agent_id.is_some())
        .expect("the failed spawn keeps a row");
    assert_eq!(failed.status, NodeStatus::Failed);
    assert!(!failed.path.exists(), "the copy was unminted");
    assert_eq!(
        stored.operations()["op-hook-fail"].terminal_code,
        Some(TopologyErrorCode::MintHookFailed)
    );
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.node(&failed.agent_id).unwrap().status,
        NodeStatus::Failed,
        "the failure was persisted"
    );
}

#[tokio::test]
async fn spawn_close_emits_tree_node_spawned_and_detached() {
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.acknowledge_revision(store.save(&tree).unwrap());
    let driver = driver_on(tree, store);
    let recorder = Arc::new(RecordingObserver::default());
    let observer = Arc::clone(&recorder) as Arc<dyn TopologyObserver>;
    let spawn = TopologyTool {
        driver: driver.clone(),
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Spawn,
        vocabulary: TopologyVocabulary::default(),
        binder: Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: Some(Arc::clone(&observer)),
        cell_budget: test_budget(),
    };
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    spawn
        .spawn_with("op-spawn-telemetry".into(), &arguments)
        .await
        .unwrap();
    let close = TopologyTool {
        driver: driver.clone(),
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Close,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: Some(Arc::clone(&observer)),
        cell_budget: test_budget(),
    };
    close
        .close_with("op-close-telemetry".into(), &agent_id_request("c1"))
        .await
        .unwrap();
    let spawned = recorder.spawned.lock().unwrap().clone();
    let detached = recorder.detached.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].0, tree_id);
    assert_eq!(spawned[0].1, "c1");
    assert_eq!(spawned[0].2.as_deref(), Some("root"));
    assert_eq!(detached.len(), 1);
    assert_eq!(detached[0].0, tree_id);
    assert_eq!(detached[0].1, "c1");
    assert_eq!(detached[0].2, NodeDetach::Closed);
}
