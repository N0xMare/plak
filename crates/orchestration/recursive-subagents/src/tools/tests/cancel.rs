//! Interrupt, close and fail-closed sessions.

use super::*;

#[tokio::test]
async fn interrupt_cancels_live_child_and_replays() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.set_active_turn("c1", true).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let canceller = Arc::new(StubCanceller::ok());
    let tool = interrupt_tool(
        driver.clone(),
        Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
    );
    let first = tool
        .interrupt_with("op-interrupt".into(), &agent_id_request("c1"))
        .await
        .unwrap();
    assert_eq!(first.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&first)["status"], "interrupted");
    {
        let calls = canceller.calls.lock().unwrap();
        assert_eq!(calls.as_slice(), &[vec!["c1".to_owned()]]);
    }
    {
        let tree = tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        // This stub has no terminal observer or release worker.
        assert!(tree.node("c1").unwrap().active_turn);
    }
    let replay = tool
        .interrupt_with("op-interrupt".into(), &agent_id_request("c1"))
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&replay)["status"], "interrupted");
    assert_eq!(canceller.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn interrupt_without_canceller_persists_only() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.set_active_turn("c1", true).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let outcome = interrupt_tool(driver.clone(), None)
        .interrupt_with("op-interrupt-store".into(), &agent_id_request("c1"))
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let tree = tree.lock().unwrap();
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
    // This stub has no terminal observer or release worker.
    assert!(tree.node("c1").unwrap().active_turn);
}

#[tokio::test]
async fn interrupt_cancel_failure_is_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.set_active_turn("c1", true).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let canceller = Arc::new(StubCanceller::fail());
    let outcome = interrupt_tool(
        driver.clone(),
        Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
    )
    .interrupt_with("op-interrupt-fail".into(), &agent_id_request("c1"))
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        tree.lock().unwrap().node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

#[tokio::test]
async fn close_cancels_closed_ids_in_domain_order() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let canceller = Arc::new(StubCanceller::ok());
    let outcome = close_tool(
        driver.clone(),
        Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
    )
    .close_with("op-close".into(), &agent_id_request("c1"))
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&outcome)["agents"][0]["agent_id"], "c1");
    {
        let calls = canceller.calls.lock().unwrap();
        assert_eq!(calls.as_slice(), &[vec!["c1".to_owned()]]);
    }
    assert_eq!(
        tree.lock().unwrap().node("c1").unwrap().status,
        NodeStatus::Closed
    );
    let replay = close_tool(
        driver.clone(),
        Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
    )
    .close_with("op-close".into(), &agent_id_request("c1"))
    .await
    .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    assert_eq!(canceller.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn close_cancel_failure_keeps_close_intent_and_recovery_evidence() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.set_active_turn("c1", true).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let outcome = close_tool(
        driver.clone(),
        Some(Arc::new(StubCanceller::fail()) as Arc<dyn LiveTurnCanceller>),
    )
    .close_with("op-close-fail".into(), &agent_id_request("c1"))
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        tree.lock().unwrap().node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(matches!(
        tree.lock().unwrap().recovery_need("c1").unwrap(),
        crate::tree::RecoveryNeed::Session { .. }
    ));
}

#[tokio::test]
async fn close_cancel_failure_without_ids_keeps_closed() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let outcome = close_tool(
        driver.clone(),
        Some(Arc::new(StubCanceller::fail_without_ids()) as Arc<dyn LiveTurnCanceller>),
    )
    .close_with("op-close-empty".into(), &agent_id_request("c1"))
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        tree.lock().unwrap().node("c1").unwrap().status,
        NodeStatus::Closed
    );
}

#[tokio::test]
async fn fail_closed_session_does_not_project_unsaved_materialization() {
    use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};

    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let driver = driver_on(
        ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            uuid::Uuid::new_v4().to_string(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap(),
        Arc::new(FailingStore),
    );
    let tree = Arc::clone(&driver.tree);
    let agent_id = {
        let mut tree = tree.lock().unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let spawned = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        tree.bind_session_locator(&spawned.agent_id, "sess/fail")
            .unwrap();
        spawned.agent_id
    };
    let recorder = Arc::new(RecordingObserver::default());
    let observer = Arc::clone(&recorder) as Arc<dyn TopologyObserver>;
    let tool = TopologyTool {
        driver: driver.clone(),
        caller_id: Arc::new(Mutex::new("root".into())),
        kind: TopologyKind::Spawn,
        vocabulary: TopologyVocabulary::default(),
        binder: None,
        deliverer: None,
        canceller: None,
        recoverer: None,
        observer: Some(Arc::clone(&observer)),
        cell_budget: test_budget(),
    };
    let _ = tool.fail_closed_session(&agent_id, "materialization failed".into());
    driver
        .read(|tree| {
            assert_eq!(
                tree.attempt(&agent_id).unwrap().status,
                crate::protocol::AttemptStatus::Running
            );
            assert_eq!(tree.node(&agent_id).unwrap().status, NodeStatus::Running);
            assert!(tree.attempt(&agent_id).unwrap().failure.is_none());
            assert!(tree.attempt(&agent_id).unwrap().candidate_id.is_none());
        })
        .unwrap();
    assert!(vcs.calls_of(FakeOp::Seal).is_empty());
    let detached = recorder.detached.lock().unwrap().clone();
    assert_eq!(detached.len(), 1);
    assert_eq!(detached[0].1, agent_id);
    assert_eq!(detached[0].2, NodeDetach::RecoveryRequired);
}

/// A send waiting behind another send's resume Start keeps waiting when
/// an interrupt lands while that Start is being admitted. The product
/// Start marks the turn active (which makes the node `running`) before
/// admission and binds the turn and registers its waiter only after it;
/// an interrupt in between leaves the node `interrupted` with an active
/// but unbound turn and no waiter yet. That is not a bound turn nobody
/// will release, so the waiting send does not fail the child closed.
#[tokio::test(start_paused = true)]
async fn an_interrupt_during_a_resume_start_admission_is_not_unwatched() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    let stub = StubDeliverer::ok().binding(&tree);
    for _ in 0..2 {
        stub.first
            .lock()
            .unwrap()
            .push_back(Err(MailDeliveryFailure::TurnEnded));
    }
    // The resume Start is held at admission.
    let (stub, gate) = stub.holding_from(2);
    let deliverer = Arc::new(stub);
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let send = |operation: &'static str, text: &'static str| {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.send_with(operation.into(), &send_request("c1", text, None))
                .await
                .unwrap()
        })
    };
    let first = send("op-resume", "use the v2 schema");
    until_calls(&deliverer, 1).await;
    let second = send("op-rider", "keep the old field too");
    until_calls(&deliverer, 2).await;
    release_unwound_turn(&tree);
    until_calls(&deliverer, 3).await;
    {
        // What the product Start does before admission, then an
        // interrupt before its bind.
        let mut tree = tree.lock().unwrap();
        tree.set_active_turn("c1", true).unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
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
    }
    // The interrupt notified the waiting send; let it look, past a tick
    // of virtual time.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (status, fault) = {
        let tree = tree.lock().unwrap();
        (
            tree.node("c1").unwrap().status,
            tree.fault_reason("c1").map(str::to_owned),
        )
    };
    // One send holds the resume Start; the other still waits for it.
    assert!(
        !first.is_finished() && !second.is_finished(),
        "the waiting send stopped waiting"
    );
    first.abort();
    second.abort();
    gate.add_permits(4);
    assert_ne!(
        status,
        NodeStatus::RecoveryRequired,
        "a resume Start still being admitted was read as an unwatched turn: {fault:?}"
    );
}
