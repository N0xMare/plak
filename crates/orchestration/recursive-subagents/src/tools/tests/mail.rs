//! Sends, followups, steering and redelivery.

use super::*;
use crate::tools::mail::TURN_UNWIND_WAIT;

#[tokio::test]
async fn send_without_deliverer_leaves_mail_accepted() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let tool = send_tool(driver.clone(), None);
    let outcome = tool
        .send_with("op-mail-1".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let payload = outcome_json(&outcome);
    assert_eq!(payload["disposition"], "started");
    let tree = tree.lock().unwrap();
    assert_eq!(tree.mail("m1").unwrap().durable, MailDurableState::Accepted);
    assert!(!tree.node("c1").unwrap().active_turn);
}

#[tokio::test]
async fn send_deliverer_starts_idle_child_and_skips_replay() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let binder = Arc::new(StubBinder::bind_only());
    let spawn = spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    );
    spawn
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let deliverer = Arc::new(StubDeliverer::ok().binding(&tree));
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let first = tool
        .send_with("op-mail-1".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
    assert_eq!(first.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&first)["disposition"], "started");
    {
        let calls = deliverer.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].disposition, MailDisposition::Started);
        assert_eq!(calls[0].body, "hello");
        assert_eq!(
            calls[0].session_locator,
            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
        );
        assert_eq!(calls[0].turn_token, 1);
    }
    {
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.mail("m1").unwrap().durable,
            MailDurableState::Acknowledged
        );
        assert!(tree.node("c1").unwrap().active_turn);
        assert_eq!(
            tree.attempt("c1").unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
    }
    let replay = tool
        .send_with("op-mail-1".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Success);
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    let loaded = reload(&store, "tree1");
    assert_eq!(
        loaded.mail("m1").unwrap().durable,
        MailDurableState::Acknowledged
    );
    assert_eq!(
        loaded.attempt("c1").unwrap().turn_id.as_deref(),
        Some("turn-1")
    );
}

#[tokio::test]
async fn send_receipt_replays_after_acknowledged_mail_history_is_pruned() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "history-spawn".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let deliverer = Arc::new(StubDeliverer::ok());
    let sender = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let request = send_request("c1", "original", None);
    let first = sender
        .send_with("original-send".into(), &request)
        .await
        .unwrap();
    {
        let mut tree = driver.tree.lock().unwrap();
        for _ in 0..70 {
            let request: SendAgentMessageRequest =
                serde_json::from_value(send_request("c1", "later", None)).unwrap();
            let next = tree.send_agent_message("root", &request, None).unwrap();
            tree.acknowledge_mail(&next.message_id).unwrap();
        }
        assert!(tree.mail("m1").is_none());
        driver.save(&mut tree).unwrap();
    }
    let replay = sender
        .send_with("original-send".into(), &request)
        .await
        .unwrap();
    assert_eq!(outcome_json(&replay), outcome_json(&first));
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn send_without_locator_is_fail_closed_when_deliverer_present() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let deliverer = Arc::new(StubDeliverer::ok());
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let outcome = tool
        .send_with("op-mail-missing".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(deliverer.calls.lock().unwrap().is_empty());
    {
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }
    assert_eq!(
        reload(&store, "tree1").node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

#[tokio::test]
async fn send_deliverer_failure_is_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let binder = Arc::new(StubBinder::ok());
    spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-bind".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let deliverer = Arc::new(StubDeliverer::fail());
    let outcome = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with("op-mail-fail".into(), &send_request("c1", "hello", None))
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    assert_eq!(
        reload(&store, "tree1").node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

/// Mail sent between a child's turn end and the host's park decision
/// reaches an ended turn: the send stands, the child is not faulted, and
/// the mail stays unread and unclaimed so the host's wake carries it.
#[tokio::test]
async fn send_to_a_turn_that_already_ended_leaves_the_mail_for_the_wake() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
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
    let deliverer = Arc::new(StubDeliverer::turn_ended());
    let outcome = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with(
        "op-mail-late".into(),
        &send_request("c1", "one more thing", None),
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    let message_id = outcome_json(&outcome)["message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tree = tree.lock().unwrap();
    assert_ne!(
        tree.node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(
        tree.wake_mail("c1")
            .iter()
            .any(|mail| mail.message_id == message_id)
    );
    drop(tree);
    assert_ne!(
        reload(&store, "tree1").node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

/// `TurnEnded` is benign only while a wake can still carry the mail
/// (spec 03 §5.2). A child whose turn submitted and then ended is never
/// woken again: the send is refused `recipient_attempt_terminal` and the
/// mail is settled instead of staying unread on the terminal attempt.
#[tokio::test]
async fn turn_ended_send_to_a_submitted_child_is_refused_and_settled() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
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
    // The child called submit_result; its turn is still unwinding, so the
    // send is admitted against the open attempt.
    tree.lock()
        .unwrap()
        .submit_result(
            "c1",
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.to_owned(),
            },
            None,
        )
        .unwrap();
    let deliverer = Arc::new(StubDeliverer::turn_ended());
    let outcome = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with(
        "op-mail-late".into(),
        &send_request("c1", "also check the docs", None),
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&outcome)["code"],
        json!(TopologyErrorCode::RecipientAttemptTerminal)
    );
    // A replay is refused the same way instead of returning the success
    // the send first recorded.
    let replay = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with(
        "op-mail-late".into(),
        &send_request("c1", "also check the docs", None),
    )
    .await
    .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&replay)["code"],
        json!(TopologyErrorCode::RecipientAttemptTerminal)
    );
    assert_eq!(
        deliverer.calls.lock().unwrap().len(),
        1,
        "never re-delivered"
    );
    // The submitted turn ends: the attempt completes and nothing wakes it,
    // and no unread mail is left on it.
    let mut tree = tree.lock().unwrap();
    tree.record_turn_completed("c1").unwrap();
    assert!(tree.attempt("c1").unwrap().status.is_stopped());
    assert_eq!(tree.wake_mail("c1"), [] as [crate::tree::MailRecord; 0]);
}

/// Spec 03 section 9: mail that reaches an interrupted child while its
/// cancelled turn is still unwinding has no park and no wake to carry
/// it. The send waits for the unwind to release the turn, then delivers
/// the same message again as the Start of the resume turn: its
/// disposition and receipt say `started` and the mail is acknowledged.
#[tokio::test]
async fn turn_ended_send_to_an_interrupted_child_starts_its_resume_turn() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
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
    {
        let mut tree = tree.lock().unwrap();
        tree.set_active_turn("c1", true).unwrap();
        if tree.attempt("c1").unwrap().turn_id.is_none() {
            tree.bind_attempt_turn("c1", "00000000-0000-0000-0000-000000000001")
                .unwrap();
        }
        // The parent interrupts the child; its turn is still unwinding.
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
    // The redirect reaches the live session after the cancelled turn ended.
    let deliverer = Arc::new(StubDeliverer::turn_ended_then_ok().binding(&tree));
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let request = send_request("c1", "use the v2 schema instead", None);
    let pending_send = tokio::spawn({
        let (tool, request) = (Arc::clone(&tool), request.clone());
        async move {
            tool.send_with("op-mail-redirect".into(), &request)
                .await
                .unwrap()
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while deliverer.calls.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!pending_send.is_finished(), "the send waits for the unwind");
    // The interrupted turn's waiter releases the turn; nothing parks it.
    {
        let mut tree = tree.lock().unwrap();
        tree.release_attempt_turn("c1").unwrap();
        tree.release_turn_owner("c1");
        tree.set_active_turn("c1", false).unwrap();
        tree.leave_turn_waiter("c1");
        assert!(!tree.is_parked("c1"));
    }
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending_send)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.kind(),
        ToolOutcomeKind::Success,
        "{}",
        outcome_json(&outcome)
    );
    let sent = outcome_json(&outcome);
    assert_eq!(sent["disposition"], json!(MailDisposition::Started));
    let calls = deliverer.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].message_id, calls[1].message_id);
    assert_eq!(calls[1].disposition, MailDisposition::Started);
    let message_id = sent["message_id"].as_str().unwrap().to_owned();
    {
        let tree = tree.lock().unwrap();
        assert!(
            tree.mail(&message_id)
                .is_none_or(|mail| mail.durable != MailDurableState::Accepted),
            "the resume turn carried the mail"
        );
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
        assert_eq!(
            tree.attempt("c1").unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
    }
    // A replay returns the updated receipt.
    let replay = tool
        .send_with("op-mail-redirect".into(), &request)
        .await
        .unwrap();
    assert_eq!(
        outcome_json(&replay)["disposition"],
        json!(MailDisposition::Started)
    );
}

/// Two sends that both found an interrupted child's turn ended restart
/// it once: the first to look reserves the resume Start under the tree
/// lock, and the other waits for that Start, then follows up into the
/// turn it admitted with its receipt as it was.
#[tokio::test]
async fn concurrent_turn_ended_sends_restart_an_interrupted_child_once() {
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
    // The resume Start is held until both sends have looked.
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
    let first = send("op-mail-a", "use the v2 schema");
    until_calls(&deliverer, 1).await;
    let second = send("op-mail-b", "keep the old field too");
    until_calls(&deliverer, 2).await;
    release_unwound_turn(&tree);
    until_calls(&deliverer, 3).await;
    // Both sends saw the release; give the other one its look.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        deliverer.calls.lock().unwrap().len(),
        3,
        "a second resume Start was delivered for the same interrupted child"
    );
    gate.add_permits(4);
    let mut outcomes = Vec::new();
    for pending in [first, second] {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        outcomes.push(outcome_json(&outcome));
    }
    let calls = deliverer.calls.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        4,
        "one resume Start, then the rider's follow-up"
    );
    assert_eq!(calls[2].disposition, MailDisposition::Started);
    assert_eq!(calls[3].disposition, MailDisposition::Queued);
    assert_ne!(calls[3].message_id, calls[2].message_id);
    let started = outcomes
        .iter()
        .find(|sent| sent["message_id"] == json!(calls[2].message_id))
        .unwrap();
    let rider = outcomes
        .iter()
        .find(|sent| sent["message_id"] != json!(calls[2].message_id))
        .unwrap();
    assert_eq!(started["disposition"], json!(MailDisposition::Started));
    assert_eq!(rider["disposition"], json!(MailDisposition::Queued));
    let tree = tree.lock().unwrap();
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
    assert_eq!(
        tree.mail(rider["message_id"].as_str().unwrap())
            .map(|mail| mail.durable),
        Some(MailDurableState::Acknowledged),
        "the other send's mail reached the resumed turn as a follow-up"
    );
}

/// A sender cancelled while its send waits for an interrupted child's
/// turn to unwind returns `Cancelled` at once, before any restart: the
/// mail stays unread under its original receipt.
#[tokio::test]
async fn a_cancelled_sender_stops_waiting_for_the_unwind_before_any_restart() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    let deliverer = Arc::new(StubDeliverer::turn_ended_then_ok());
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let cancellation = plakous::model::CancellationToken::new();
    let pending = tokio::spawn({
        let (tool, cancellation) = (Arc::clone(&tool), cancellation.clone());
        async move {
            tool.dispatch_cancellable(
                &ToolCallerLineage::Direct,
                "op-mail-cancelled".into(),
                &send_request("c1", "use the v2 schema", None),
                cancellation.cancelled(),
            )
            .await
        }
    });
    until_calls(&deliverer, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!pending.is_finished(), "the send waits for the unwind");
    cancellation.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .expect("a cancelled send stops waiting")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
    release_unwound_turn(&tree);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        deliverer.calls.lock().unwrap().len(),
        1,
        "nothing restarted"
    );
    let tree = tree.lock().unwrap();
    let mail = tree
        .messages()
        .values()
        .find(|mail| mail.to == "c1")
        .unwrap();
    assert_eq!(mail.durable, MailDurableState::Accepted);
    assert_eq!(mail.disposition, MailDisposition::Queued);
    let receipt = tree.operations()["op-mail-cancelled"]
        .terminal_response
        .clone()
        .unwrap();
    assert!(receipt.contains("queued"), "{receipt}");
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
}

/// The same holds for a send waiting behind a parked wait on its mail.
#[tokio::test]
async fn a_cancelled_sender_stops_waiting_behind_a_parked_wait() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    tree.lock()
        .unwrap()
        .parked_waits
        .insert(("c1".into(), "op-wait".into()), vec!["root".into()]);
    let deliverer = Arc::new(StubDeliverer::ok());
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let cancellation = plakous::model::CancellationToken::new();
    let pending = tokio::spawn({
        let (tool, cancellation) = (Arc::clone(&tool), cancellation.clone());
        async move {
            tool.dispatch_cancellable(
                &ToolCallerLineage::Direct,
                "op-mail-behind-wait".into(),
                &send_request("c1", "answer when you can", None),
                cancellation.cancelled(),
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!pending.is_finished(), "the send waits behind the wait");
    cancellation.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .expect("a cancelled send stops waiting")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
    assert!(deliverer.calls.lock().unwrap().is_empty());
    let tree = tree.lock().unwrap();
    assert!(
        tree.messages()
            .values()
            .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Accepted)
    );
}

/// A resume Start that fails refuses the send, and the receipt becomes
/// that refusal: it never read `started` for a Start that was not
/// admitted, so a replay is refused without delivering again.
#[tokio::test]
async fn a_failed_resume_start_never_records_a_started_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    let deliverer = Arc::new(StubDeliverer::turn_ended_then_fail());
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let request = send_request("c1", "use the v2 schema", None);
    let pending = tokio::spawn({
        let (tool, request) = (Arc::clone(&tool), request.clone());
        async move {
            tool.send_with("op-mail-resume".into(), &request)
                .await
                .unwrap()
        }
    });
    until_calls(&deliverer, 1).await;
    release_unwound_turn(&tree);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(deliverer.calls.lock().unwrap().len(), 2);
    assert_eq!(
        tree.lock().unwrap().operations()["op-mail-resume"].terminal_code,
        Some(TopologyErrorCode::OperationRecoveryRequired),
        "the receipt is the refusal, not a `started` success"
    );
    let replay = tool
        .send_with("op-mail-resume".into(), &request)
        .await
        .unwrap();
    assert_eq!(replay.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&replay)["code"],
        json!(TopologyErrorCode::OperationRecoveryRequired)
    );
    assert_eq!(
        deliverer.calls.lock().unwrap().len(),
        2,
        "a replay is not delivered again"
    );
}

/// A `followup` Start whose delivery finds the child's previous slot
/// still retiring is retried once the slot is vacant, under the send's
/// budget: it is never left unread under a `started` receipt on an
/// attempt nothing will start.
#[tokio::test]
async fn a_followup_start_on_a_retiring_slot_is_retried() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&tree);
    let stub = StubDeliverer::ok().binding(&tree);
    stub.first
        .lock()
        .unwrap()
        .push_back(Err(MailDeliveryFailure::SlotBusy));
    let deliverer = Arc::new(stub);
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let request = serde_json::to_value(SendAgentMessageRequest {
        agent_id: "c1".into(),
        message: "now do the v3 schema".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tool.send_with("op-followup".into(), &request),
    )
    .await
    .expect("the send returns")
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let sent = outcome_json(&outcome);
    assert_eq!(sent["disposition"], json!(MailDisposition::Started));
    let calls = deliverer.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "the followup Start was retried once");
    assert!(
        calls
            .iter()
            .all(|call| call.disposition == MailDisposition::Started)
    );
    let tree = tree.lock().unwrap();
    assert_eq!(
        tree.mail(sent["message_id"].as_str().unwrap())
            .map(|mail| mail.durable),
        Some(MailDurableState::Acknowledged)
    );
    assert!(tree.node("c1").unwrap().active_turn);
    assert_eq!(
        tree.attempt("c1").unwrap().turn_id.as_deref(),
        Some("turn-1")
    );
    assert!(tree.start_reservations.is_empty());
}

/// A Started mail whose Start finds a turn another command admitted is
/// delivered into that turn as a follow-up, and its mail and receipt
/// become `queued`. The second send's Start waits for the first's, which
/// it reserved.
#[tokio::test]
async fn a_started_mail_that_finds_another_commands_turn_follows_up_into_it() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::bind_only()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-bind".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let stub = StubDeliverer::ok().binding(&tree);
    // The first Start is admitted; the second finds its turn live.
    stub.first.lock().unwrap().extend([
        Ok(MailDeliveryAck {
            turn_id: Some("turn-1".into()),
        }),
        Err(MailDeliveryFailure::TurnEnded),
    ]);
    let (stub, gate) = stub.holding_from(0);
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
    let first = send("op-mail-a", "use the v2 schema");
    until_calls(&deliverer, 1).await;
    let second = send("op-mail-b", "keep the old field too");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        deliverer.calls.lock().unwrap().len(),
        1,
        "a second Start was delivered while the first was in flight"
    );
    gate.add_permits(8);
    let mut receipts = Vec::new();
    for pending in [first, second] {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        receipts.push(outcome_json(&outcome));
    }
    assert_eq!(receipts[0]["disposition"], json!(MailDisposition::Started));
    assert_eq!(receipts[1]["disposition"], json!(MailDisposition::Queued));
    let calls = deliverer.calls.lock().unwrap().clone();
    let shapes: Vec<_> = calls
        .iter()
        .map(|call| (call.message_id.clone(), call.disposition))
        .collect();
    let (a, b) = (
        receipts[0]["message_id"].as_str().unwrap().to_owned(),
        receipts[1]["message_id"].as_str().unwrap().to_owned(),
    );
    assert_eq!(
        shapes,
        [
            (a.clone(), MailDisposition::Started),
            (b.clone(), MailDisposition::Started),
            (b.clone(), MailDisposition::Queued),
        ]
    );
    let tree = tree.lock().unwrap();
    let mail = tree.mail(&b).unwrap();
    assert_eq!(mail.durable, MailDurableState::Acknowledged);
    assert_eq!(mail.disposition, MailDisposition::Queued);
    let receipt = tree.operations()["op-mail-b"]
        .terminal_response
        .clone()
        .unwrap();
    assert!(receipt.contains("queued"), "{receipt}");
}

/// One 60 s budget per send, however often it restarts: the
/// reservation holder checks it before it renews its Start, so a resume
/// Start that keeps finding the slot taken is refused definitively, its
/// mail settled read, its receipt the refusal, and its reservation
/// released.
#[tokio::test(start_paused = true)]
async fn a_restart_loop_is_bounded_by_the_send_budget() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&tree);
    let deliverer = Arc::new(StubDeliverer::turn_ended());
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        tool.send_with(
            "op-stuck".into(),
            &send_request("c1", "use the v2 schema", None),
        ),
    )
    .await;
    let calls = deliverer.calls.lock().unwrap().len();
    let outcome = outcome.unwrap_or_else(|_| {
        panic!(
            "the send was still restarting after 300 s (virtual): {calls} Starts \
             delivered, the 60 s budget never ended it"
        )
    });
    let outcome = outcome.unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    let refused = outcome_json(&outcome);
    assert_eq!(
        refused["code"],
        json!(TopologyErrorCode::RecipientAttemptTerminal)
    );
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|message| message.contains("not admitted in time")),
        "{refused}"
    );
    let tree = tree.lock().unwrap();
    assert!(
        tree.start_reservations.is_empty(),
        "the reservation is released"
    );
    assert_eq!(
        tree.operations()["op-stuck"].terminal_code,
        Some(TopologyErrorCode::RecipientAttemptTerminal)
    );
    assert!(
        tree.messages()
            .values()
            .filter(|mail| mail.to == "c1")
            .all(|mail| mail.durable == MailDurableState::Acknowledged),
        "the refused mail is settled read"
    );
}

/// SlotBusy confirms no submission; expiry withdraws the opening.
#[tokio::test(start_paused = true)]
async fn a_followup_start_that_never_finds_the_slot_vacant_withdraws_its_attempt() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&tree);
    let found = tree
        .lock()
        .unwrap()
        .attempt("c1")
        .unwrap()
        .attempt_id
        .clone();
    let mut stub = StubDeliverer::ok();
    stub.result = Err(MailDeliveryFailure::SlotBusy);
    let deliverer = Arc::new(stub);
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let request = serde_json::to_value(SendAgentMessageRequest {
        agent_id: "c1".into(),
        message: "now do the v3 schema".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        tool.send_with("op-followup".into(), &request),
    )
    .await
    .expect("the budget ends the send")
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    let refused = outcome_json(&outcome);
    assert_eq!(
        refused["code"],
        json!(TopologyErrorCode::RecipientAttemptTerminal)
    );
    assert!(
        deliverer.calls.lock().unwrap().len() > 1,
        "the Start was retried"
    );
    let tree = tree.lock().unwrap();
    assert_eq!(tree.attempt("c1").unwrap().attempt_id, found);
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
    assert!(tree.start_reservations.is_empty());
    assert_eq!(
        tree.operations()["op-followup"].terminal_code,
        Some(TopologyErrorCode::RecipientAttemptTerminal)
    );
    assert_followup_views_rebuild(&store, &tree);
}

/// A Started mail whose Start finds another command's turn follows up as
/// `queued`. When that follow-up never reaches a live turn before the
/// budget ends, the mail stays unread and the send answers the receipt it
/// recorded (`started`), not the in-memory follow-up disposition.
#[tokio::test(start_paused = true)]
async fn pf5_a_left_follow_up_answers_its_durable_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&tree);
    let stub = StubDeliverer::ok();
    stub.first
        .lock()
        .unwrap()
        .push_back(Err(MailDeliveryFailure::TurnEnded));
    let mut stub = stub;
    stub.result = Err(MailDeliveryFailure::SlotBusy);
    let (stub, gate) = stub.holding_from(0);
    let deliverer = Arc::new(stub);
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let request = serde_json::to_value(SendAgentMessageRequest {
        agent_id: "c1".into(),
        message: "now do the v3 schema".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap();
    let pending = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move { tool.send_with("op-left".into(), &request).await.unwrap() }
    });
    until_calls(&deliverer, 1).await;
    // Another command's Start now holds the child's turn.
    tree.lock().unwrap().set_active_turn("c1", true).unwrap();
    gate.add_permits(100_000);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
        .await
        .expect("the budget ends the send")
        .unwrap();
    let answered = outcome_json(&outcome);
    let tree = tree.lock().unwrap();
    let durable = tree.operations()["op-left"]
        .terminal_response
        .clone()
        .unwrap();
    let durable: Value = serde_json::from_str(&durable).unwrap();
    let message_id = answered["message_id"].as_str().unwrap();
    let mail = tree
        .mail(message_id)
        .map(|mail| (mail.durable, mail.disposition));
    assert_eq!(
        answered["disposition"], durable["disposition"],
        "the send answered {answered} but its durable receipt (a replay) reads {durable}; \
         mail {mail:?}"
    );
    assert_eq!(answered["disposition"], json!(MailDisposition::Started));
    assert_eq!(
        mail,
        Some((MailDurableState::Accepted, MailDisposition::Started)),
        "the unread mail keeps the recorded disposition"
    );
}

/// Cancellation after SlotBusy, before resubmission, withdraws the opening.
#[tokio::test(start_paused = true)]
async fn a_cancelled_followup_withdraws_its_unstarted_attempt() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&tree);
    let found = tree
        .lock()
        .unwrap()
        .attempt("c1")
        .unwrap()
        .attempt_id
        .clone();
    let mut stub = StubDeliverer::ok();
    stub.result = Err(MailDeliveryFailure::SlotBusy);
    let deliverer = Arc::new(stub);
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let request = serde_json::to_value(SendAgentMessageRequest {
        agent_id: "c1".into(),
        message: "now do the v3 schema".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap();
    let cancellation = plakous::model::CancellationToken::new();
    let pending = tokio::spawn({
        let (tool, cancellation, request) =
            (Arc::clone(&tool), cancellation.clone(), request.clone());
        async move {
            tool.dispatch_cancellable(
                &ToolCallerLineage::Direct,
                "op-followup-cancelled".into(),
                &request,
                cancellation.cancelled(),
            )
            .await
        }
    });
    until_calls(&deliverer, 1).await;
    cancellation.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
    {
        let tree = tree.lock().unwrap();
        assert_eq!(tree.attempt("c1").unwrap().attempt_id, found);
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        assert!(
            tree.messages()
                .values()
                .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Acknowledged)
        );
        assert!(tree.start_reservations.is_empty());
        assert_eq!(
            tree.operations()["op-followup-cancelled"].terminal_code,
            Some(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert_followup_views_rebuild(&store, &tree);
    }
}

fn assert_followup_views_rebuild(store: &Arc<dyn TreeStore>, live: &ProductAgentTree) {
    let mut rebuilt = reload(store, "tree1");
    rebuilt.clear_feed_views();
    rebuilt
        .project_feed(
            "root",
            &rebuilt.tree_id.clone(),
            &[],
            &crate::tree::TreeFeedReads::default(),
        )
        .unwrap();
    rebuilt
        .project_feed(
            "c1",
            &rebuilt.tree_id.clone(),
            &[],
            &crate::tree::TreeFeedReads::default(),
        )
        .unwrap();
    rebuilt.fold_due("c1").unwrap();
    assert_eq!(
        rebuilt.node("c1").unwrap().status,
        live.node("c1").unwrap().status
    );
    assert_eq!(
        rebuilt.node("c1").unwrap().attempt_id,
        live.node("c1").unwrap().attempt_id
    );
    assert_eq!(
        rebuilt.node("c1").unwrap().output_schema_json,
        live.node("c1").unwrap().output_schema_json
    );
    assert_eq!(
        rebuilt.attempt("c1").unwrap().status,
        live.attempt("c1").unwrap().status
    );
}

#[tokio::test]
async fn a_followup_cancelled_before_delivery_is_withdrawn_and_rebuilds() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&driver.tree);
    let previous = driver
        .read(|tree| tree.attempt("c1").unwrap().attempt_id.clone())
        .unwrap();
    let deliverer = Arc::new(StubDeliverer::ok());
    let tool = send_tool(driver.clone(), Some(deliverer.clone()));
    let request = json!({"agent_id": "c1", "message": "again", "purpose": "followup"});
    let answer = tool
        .dispatch_cancellable(
            &ToolCallerLineage::Direct,
            "before-delivery-followup".to_owned(),
            &request,
            std::future::ready(()),
        )
        .await;
    assert_eq!(answer.unwrap_err().kind(), ToolRuntimeErrorKind::Cancelled);
    assert!(deliverer.calls.lock().unwrap().is_empty());
    let tree = driver.tree.lock().unwrap();
    assert_eq!(tree.attempt("c1").unwrap().attempt_id, previous);
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
    assert!(
        tree.messages()
            .values()
            .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Acknowledged)
    );
    assert!(tree.start_reservations.is_empty());
    assert_followup_views_rebuild(&store, &tree);
}

#[tokio::test(start_paused = true)]
async fn followup_cancel_and_slot_expiry_with_a_future_tail_hold_recovery_and_rebuild() {
    struct FutureTail {
        inner: Arc<StubDeliverer>,
    }
    impl MailDeliverer for FutureTail {
        fn deliver(
            &self,
            request: MailDeliveryRequest,
        ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>
        {
            self.inner.deliver(request)
        }

        fn withdraw_followup(
            &self,
            driver: TreeDriver,
            agent: String,
            message: String,
            receipt: Option<(String, TopologyErrorCode)>,
        ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
            Box::pin(async move {
                let (session, tail) = driver.read(|tree| {
                    let session = tree
                        .node(&agent)
                        .unwrap()
                        .session_locator
                        .as_ref()
                        .unwrap()
                        .parse::<plakous::sessions::SessionLocator>()
                        .unwrap()
                        .session_id()
                        .to_string();
                    let tail = tree.feed_cursor(&session) + 1;
                    (session, tail)
                })?;
                driver.withdraw_unstarted_followup(&agent, &message, &session, tail, receipt)
            })
        }
    }

    for expiry in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&driver.tree);
        let previous = driver
            .read(|tree| tree.attempt("c1").unwrap().attempt_id.clone())
            .unwrap();
        let mut stub = StubDeliverer::ok();
        stub.result = Err(MailDeliveryFailure::SlotBusy);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::new(FutureTail {
                inner: Arc::clone(&deliverer),
            })),
        ));
        let cancellation = plakous::model::CancellationToken::new();
        let pending = tokio::spawn({
            let token = cancellation.clone();
            async move {
                let request = json!({"agent_id": "c1", "message": "again", "purpose": "followup"});
                tool.dispatch_cancellable(
                    &ToolCallerLineage::Direct,
                    "future-tail-followup".to_owned(),
                    &request,
                    token.cancelled(),
                )
                .await
            }
        });
        until_calls(&deliverer, 1).await;
        if !expiry {
            cancellation.cancel();
        }
        let answer = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
            .await
            .unwrap()
            .unwrap();
        if expiry {
            assert_eq!(answer.unwrap().kind(), ToolOutcomeKind::Failure);
        } else {
            assert_eq!(answer.unwrap_err().kind(), ToolRuntimeErrorKind::Cancelled);
        }
        let tree = driver.tree.lock().unwrap();
        assert_ne!(tree.attempt("c1").unwrap().attempt_id, previous);
        assert_eq!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            tree.messages()
                .values()
                .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Acknowledged)
        );
        assert!(tree.start_reservations.is_empty());
        assert_followup_views_rebuild(&store, &tree);
    }
}

/// The send budget never cuts a submission in flight: a Start admitted
/// after the budget is spent is delivered late, its attempt keeps
/// running and can still submit.
#[tokio::test(start_paused = true)]
async fn a_followup_start_admitted_after_the_send_budget_is_delivered_not_fenced() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&driver.tree);
    let (stub, gate) = StubDeliverer::ok().binding(&driver.tree).holding_from(0);
    let deliverer = Arc::new(stub);
    let tool = Arc::new(send_tool(driver.clone(), Some(deliverer.clone())));
    let request = json!({"agent_id": "c1", "message": "again", "purpose": "followup"});
    let mut pending = tokio::spawn(async move {
        tool.dispatch_cancellable(
            &ToolCallerLineage::Direct,
            "late-followup".to_owned(),
            &request,
            std::future::pending(),
        )
        .await
    });
    until_calls(&deliverer, 1).await;
    let opened = driver
        .read(|tree| tree.attempt("c1").unwrap().attempt_id.clone())
        .unwrap();
    tokio::time::advance(TURN_UNWIND_WAIT * 2).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        tokio::time::timeout(std::time::Duration::ZERO, &mut pending)
            .await
            .is_err(),
        "an expired budget must not answer a submission in flight"
    );
    driver
        .read(|tree| {
            assert_ne!(
                tree.node("c1").unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert!(tree.fault_reason("c1").is_none());
        })
        .unwrap();
    gate.add_permits(1);
    let answer = pending.await.unwrap().unwrap();
    assert_eq!(answer.kind(), ToolOutcomeKind::Success, "{answer:?}");
    let tree = driver.tree.lock().unwrap();
    let attempt = tree.attempt("c1").unwrap();
    assert_eq!(attempt.attempt_id, opened);
    assert!(!attempt.status.is_stopped(), "{:?}", attempt.status);
    assert!(attempt.turn_id.is_some());
    assert_ne!(
        tree.node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(tree.fault_reason("c1").is_none());
    assert!(tree.start_reservations.is_empty());
    assert!(tree.delivery_claims.is_empty());
}

#[tokio::test(start_paused = true)]
async fn followup_cancel_during_unknown_submission_holds_recovery_and_rebuild() {
    {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&driver.tree);
        let (stub, _gate) = StubDeliverer::ok().holding_from(0);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(driver.clone(), Some(deliverer.clone())));
        let cancellation = plakous::model::CancellationToken::new();
        let request = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".to_owned(),
            message: "again".to_owned(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let pending = tokio::spawn({
            let token = cancellation.clone();
            async move {
                tool.dispatch_cancellable(
                    &ToolCallerLineage::Direct,
                    "unknown-followup".to_owned(),
                    &request,
                    token.cancelled(),
                )
                .await
            }
        });
        until_calls(&deliverer, 1).await;
        cancellation.cancel();
        let answer = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
            .await
            .expect("unknown submission has a bounded cancellation")
            .unwrap();
        assert_eq!(answer.unwrap_err().kind(), ToolRuntimeErrorKind::Cancelled);
        let mut tree = driver.tree.lock().unwrap();
        assert_eq!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let expected = tree.attempt("c1").unwrap().clone();
        assert_eq!(
            expected.status,
            crate::protocol::AttemptStatus::RecoveryRequired
        );
        assert!(
            tree.operations()
                .contains_key(&format!("plak:followup_opened:{}", expected.attempt_id))
        );
        assert!(tree.start_reservations.is_empty());
        assert!(tree.delivery_claims.is_empty());
        assert!(
            tree.messages()
                .values()
                .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Accepted)
        );
        assert_followup_views_rebuild(&store, &tree);
        tree.clear_feed_views();
        let session = tree.tree_id.clone();
        tree.project_feed(
            "root",
            &session,
            &[],
            &crate::tree::TreeFeedReads::default(),
        )
        .unwrap();
        tree.project_feed("c1", &session, &[], &crate::tree::TreeFeedReads::default())
            .unwrap();
        assert_eq!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(tree.attempt("c1").unwrap(), &expected);
    }
}

/// A refused Start of an ordinary (coordinate) mail never rolls back the
/// attempt a `followup` it did not send opened: only the send that
/// opened an attempt may undo it.
#[tokio::test]
async fn a_refused_coordinate_start_never_rolls_back_another_sends_followup() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    release_unwound_turn(&tree);
    let opened = {
        let mut tree = tree.lock().unwrap();
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: "c1".into(),
                message: "now do the v3 schema".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
        tree.attempt("c1").unwrap().attempt_id.clone()
    };
    let deliverer = Arc::new(StubDeliverer::refuse());
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let outcome = tool
        .send_with(
            "op-coordinate".into(),
            &send_request("c1", "also keep the old field", None),
        )
        .await
        .unwrap();
    let tree = tree.lock().unwrap();
    assert_eq!(
        tree.attempt("c1").unwrap().attempt_id,
        opened,
        "a coordinate's refused Start rolled back another send's followup attempt: {}",
        outcome_json(&outcome)
    );
}

/// A follow-up into a turn another command admitted that is still not
/// live when the send's budget is spent leaves the mail unread: that
/// turn's end parks or wakes the child with it. The send stands.
#[tokio::test(start_paused = true)]
async fn a_follow_up_whose_turn_is_not_live_in_time_is_left_for_its_end() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
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
    {
        let mut tree = tree.lock().unwrap();
        tree.set_active_turn("c1", true).unwrap();
        if tree.attempt("c1").unwrap().turn_id.is_none() {
            tree.bind_attempt_turn("c1", "00000000-0000-0000-0000-000000000001")
                .unwrap();
        }
    }
    let mut stub = StubDeliverer::ok();
    stub.result = Err(MailDeliveryFailure::SlotBusy);
    let deliverer = Arc::new(stub);
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        tool.send_with(
            "op-late".into(),
            &send_request(
                "c1",
                "keep the old field too",
                Some(MessagePriority::Deferred),
            ),
        ),
    )
    .await
    .expect("the budget ends the send")
    .unwrap();
    assert_eq!(
        outcome.kind(),
        ToolOutcomeKind::Success,
        "{}",
        outcome_json(&outcome)
    );
    let sent = outcome_json(&outcome);
    assert_eq!(sent["disposition"], json!(MailDisposition::Queued));
    assert!(deliverer.calls.lock().unwrap().len() > 1);
    let tree = tree.lock().unwrap();
    assert_eq!(
        tree.mail(sent["message_id"].as_str().unwrap())
            .map(|mail| mail.durable),
        Some(MailDurableState::Accepted),
        "the mail stays unread for the turn's end"
    );
    assert_eq!(tree.operations()["op-late"].terminal_code, None);
}

/// A Start whose deliverer bound the turn it admitted, and whose turn
/// already ended when the delivery returns, is not marked active again:
/// the send binds nothing.
#[tokio::test]
async fn an_admitted_start_whose_turn_already_ended_is_not_marked_active() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::bind_only()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-bind".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let (stub, gate) = StubDeliverer::ok().holding_from(0);
    let deliverer = Arc::new(stub);
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let pending = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move {
            tool.send_with(
                "op-start".into(),
                &send_request("c1", "use the v2 schema", None),
            )
            .await
            .unwrap()
        }
    });
    until_calls(&deliverer, 1).await;
    {
        // The deliverer bound the turn it admitted, and the turn ended.
        let mut tree = tree.lock().unwrap();
        tree.bind_attempt_turn("c1", "turn-1").unwrap();
        tree.release_turn_owner("c1");
        tree.set_active_turn("c1", false).unwrap();
    }
    gate.add_permits(1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        outcome_json(&outcome)["disposition"],
        json!(MailDisposition::Started)
    );
    let tree = tree.lock().unwrap();
    assert!(
        !tree.node("c1").unwrap().active_turn,
        "the ended turn was marked active again"
    );
    assert_eq!(tree.attempt("c1").unwrap().usage.turns, 1);
}

/// Spawns `c1` with no turn and starts a send whose Start the deliverer
/// holds at admission, so a test can change what the child's attempt
/// binds before the delivery returns.
async fn start_held_at_admission() -> (
    tempfile::TempDir,
    Arc<Mutex<ProductAgentTree>>,
    Arc<tokio::sync::Semaphore>,
    tokio::task::JoinHandle<ToolOutcome>,
) {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::bind_only()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-bind".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let (stub, gate) = StubDeliverer::ok().holding_from(0);
    let deliverer = Arc::new(stub);
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let pending = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move {
            tool.send_with(
                "op-start".into(),
                &send_request("c1", "use the v2 schema", None),
            )
            .await
            .unwrap()
        }
    });
    until_calls(&deliverer, 1).await;
    (root, tree, gate, pending)
}

/// A Start whose turn was interrupted and released before the delivery
/// returns is not bound or marked active again: the send binds nothing,
/// so the child stays `interrupted` with no turn for the next mail to
/// resume (spec 03 section 9).
#[tokio::test]
async fn an_admitted_start_whose_turn_was_released_is_not_bound_again() {
    let (_root, tree, gate, pending) = start_held_at_admission().await;
    {
        // The deliverer bound the turn it admitted; an interrupt ended
        // it and its waiter released it.
        let mut tree = tree.lock().unwrap();
        tree.bind_attempt_turn("c1", "turn-1").unwrap();
        tree.set_active_turn("c1", true).unwrap();
        tree.interrupt_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: "c1".into(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        tree.release_attempt_turn("c1").unwrap();
    }
    gate.add_permits(1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let tree = tree.lock().unwrap();
    let node = tree.node("c1").unwrap();
    assert_eq!(node.status, NodeStatus::Interrupted);
    assert!(!node.active_turn, "the released turn was marked active");
    let attempt = tree.attempt("c1").unwrap();
    assert_eq!(attempt.turn_id, None, "the released turn was bound again");
    assert_eq!(attempt.usage.turns, 1);
}

/// A Start whose turn a later turn replaced before the delivery returns
/// keeps that later turn bound: the send never binds the turn its ack
/// names over another one.
#[tokio::test]
async fn an_admitted_start_never_binds_over_a_later_turn() {
    let (_root, tree, gate, pending) = start_held_at_admission().await;
    {
        let mut tree = tree.lock().unwrap();
        tree.bind_attempt_turn("c1", "turn-1").unwrap();
        tree.set_active_turn("c1", true).unwrap();
        tree.bind_attempt_turn("c1", "turn-2").unwrap();
    }
    gate.add_permits(1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let tree = tree.lock().unwrap();
    let attempt = tree.attempt("c1").unwrap();
    assert_eq!(attempt.turn_id.as_deref(), Some("turn-2"));
    assert_eq!(attempt.usage.turns, 2);
}

/// A send that waited behind another send's resume Start delivers its
/// mail into the resumed turn as a follow-up once that Start is
/// admitted. Its mail was claimed while the Start was composed, so the
/// resumed turn never carried it; left unread, a resumed turn that
/// submits would strand it under a `queued` success.
#[tokio::test]
async fn a_rider_reaches_the_resumed_turn() {
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
    let first = send("op-mail-a", "use the v2 schema");
    until_calls(&deliverer, 1).await;
    let second = send("op-mail-b", "keep the old field too");
    until_calls(&deliverer, 2).await;
    release_unwound_turn(&tree);
    until_calls(&deliverer, 3).await;
    gate.add_permits(8);
    let mut ids = Vec::new();
    for pending in [first, second] {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        ids.push(
            outcome_json(&outcome)["message_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let calls = deliverer.calls.lock().unwrap().clone();
    let rider = ids
        .iter()
        .find(|id| **id != calls[2].message_id)
        .unwrap()
        .clone();
    assert!(
        calls[3..]
            .iter()
            .any(|call| call.message_id == rider && call.disposition == MailDisposition::Queued),
        "the rider's mail was never offered to the resumed turn; it stays unread ({:?}) \
         until that turn's unsubmitted end",
        tree.lock().unwrap().mail(&rider).map(|mail| mail.durable)
    );
    assert_eq!(
        tree.lock().unwrap().mail(&rider).map(|mail| mail.durable),
        Some(MailDurableState::Acknowledged),
        "the resumed turn's follow-up carried the rider's mail"
    );
}

#[tokio::test]
async fn send_urgent_busy_child_steers() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
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
    let deliverer = Arc::new(StubDeliverer::ok());
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    tool.send_with("op-mail-1".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
    let body = "🦀".repeat(crate::protocol::MAX_MESSAGE_BYTES / 4);
    let refused = tool
        .send_with(
            "op-mail-too-big".into(),
            &send_request("c1", &(body.clone() + "a"), Some(MessagePriority::Urgent)),
        )
        .await
        .unwrap();
    assert_eq!(outcome_json(&refused)["code"], "bound_exceeded");
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    let steered = tool
        .send_with(
            "op-mail-2".into(),
            &send_request("c1", &body, Some(MessagePriority::Urgent)),
        )
        .await
        .unwrap();
    assert_eq!(steered.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&steered)["disposition"], "steered");
    let calls = deliverer.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].disposition, MailDisposition::Steered);
    assert_eq!(calls[1].body, body);
    assert_eq!(calls[1].turn_token, 1);
    let tree = tree.lock().unwrap();
    assert_eq!(tree.attempt("c1").unwrap().turn_token, 1);
}

#[tokio::test]
async fn send_deferred_busy_child_queues_follow_up() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
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
    let deliverer = Arc::new(StubDeliverer::ok());
    let tool = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    tool.send_with("op-mail-1".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
    let queued = tool
        .send_with(
            "op-mail-2".into(),
            &send_request("c1", "later", Some(MessagePriority::Deferred)),
        )
        .await
        .unwrap();
    assert_eq!(queued.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&queued)["disposition"], "queued");
    let calls = deliverer.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].disposition, MailDisposition::Queued);
    assert_eq!(calls[1].body, "later");
}

#[tokio::test]
async fn sibling_send_persists_grant_denied() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let spawn = spawn_tool(driver.clone(), None);
    let arguments = serde_json::to_value(spawn_request()).unwrap();
    spawn
        .spawn_with("op-sib-1".into(), &arguments)
        .await
        .unwrap();
    spawn
        .spawn_with("op-sib-2".into(), &arguments)
        .await
        .unwrap();
    let send = send_tool(driver.clone(), None);
    *send.caller_id.lock().unwrap() = "c1".into();
    let outcome = send
        .send_with("op-sib-mail".into(), &send_request("c2", "peer", None))
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&outcome)["code"], "grant_denied");
    let stored = tree.lock().unwrap();
    assert_eq!(
        stored
            .operations()
            .get("op-sib-mail")
            .expect("grant_denied must persist a terminal receipt")
            .terminal_code,
        Some(TopologyErrorCode::GrantDenied)
    );
}

/// Spawn → complete → `followup` opens attempt a2 on the
/// same node, delivered as a Start, and that attempt completes.
#[tokio::test]
async fn followup_after_completion_opens_attempt_two_and_completes() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let binder = Arc::new(StubBinder::ok());
    let spawned = spawn_tool(
        driver.clone(),
        Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
    {
        let mut tree = tree.lock().unwrap();
        tree.submit_result(
            "c1",
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed("c1").unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Completed);
    }
    let deliverer = Arc::new(StubDeliverer::ok().binding(&tree));
    let body = "🦀".repeat(crate::protocol::MAX_MESSAGE_BYTES / 4);
    let followup = serde_json::to_value(SendAgentMessageRequest {
        agent_id: "c1".into(),
        message: body.clone(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap();
    let mut too_big = followup.clone();
    too_big["message"] = json!(body.clone() + "a");
    let refused = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with("op-followup-too-big".into(), &too_big)
    .await
    .unwrap();
    assert_eq!(outcome_json(&refused)["code"], "bound_exceeded");
    assert!(deliverer.calls.lock().unwrap().is_empty());
    let outcome = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with("op-followup".into(), &followup)
    .await
    .unwrap();
    assert_eq!(
        outcome.kind(),
        ToolOutcomeKind::Success,
        "{}",
        outcome_json(&outcome)
    );
    assert_eq!(outcome_json(&outcome)["disposition"], "started");
    {
        let calls = deliverer.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].disposition, MailDisposition::Started);
        assert_eq!(calls[0].turn_token, 2, "the new attempt's token travels");
        assert_eq!(calls[0].body, body);
    }
    {
        let mut tree = tree.lock().unwrap();
        let attempt = tree.attempt("c1").unwrap();
        assert_eq!(attempt.attempt_id.as_str(), "c1-a2");
        assert_eq!(attempt.status, crate::protocol::AttemptStatus::Running);
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
        assert!(tree.node("c1").unwrap().active_turn);
        tree.submit_result(
            "c1",
            &SubmitResultRequest {
                turn_token: 2,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed("c1").unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Completed);
        assert_eq!(
            tree.attempt("c1").unwrap().status,
            crate::protocol::AttemptStatus::Completed
        );
    }
}

#[tokio::test]
async fn owner_fenced_followup_refusal_fails_closed_without_erasing_evidence() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let id = tree
        .spawn_agent("root", &spawn_request(), None)
        .unwrap()
        .agent_id;
    tree.submit_result(
        &id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&id).unwrap();
    tree.settle_for_test();
    let opener = tree
        .send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: id.clone(),
                message: "again".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap()
        .message_id;
    tree.record_host_retirement(&id, "owner release failed".into())
        .and_then(|value| tree.fold_due(&id).map(|_| value))
        .unwrap();
    let pending = tree.attempt(&id).unwrap().attempt_id.clone();
    let driver = driver_on(tree, store);
    let tool = send_tool(driver.clone(), None);
    let outcome = tool
        .refuse_delivery(
            None,
            &id,
            &opener,
            MailDeliveryFailure::Refused("child is retiring".into()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&outcome)["code"],
        "operation_recovery_required"
    );
    driver
        .read(|tree| {
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            let attempt = tree.attempt(&id).unwrap();
            assert_eq!(attempt.attempt_id, pending);
            assert_eq!(
                attempt.status,
                crate::protocol::AttemptStatus::RecoveryRequired
            );
            assert_eq!(attempt.failure.as_ref().unwrap().kind, "session_retirement");
            assert!(
                attempt
                    .failure
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("owner release failed")
            );
            assert!(matches!(
                tree.recovery_need(&id).unwrap(),
                crate::tree::RecoveryNeed::Session { .. }
            ));
        })
        .unwrap();
}

/// A `followup` whose Start the deliverer refuses leaves no attempt row.
/// The refused Start left `c1-a2` in place, the node fell
/// to `recovery_required`, and its partial-work seal minted `cand-c1-a2`
/// — which moved the workspace head out from under the parent's adopt of
/// `cand-c1-a1` (`target_changed`).
#[tokio::test]
async fn refused_followup_start_rolls_the_attempt_back() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let (schema_before, candidates_before) = {
        let mut tree = tree.lock().unwrap();
        tree.submit_result(
            "c1",
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed("c1").unwrap();
        let node = tree.node("c1").unwrap();
        assert_eq!(node.status, NodeStatus::Completed);
        (node.output_schema_json.clone(), tree.candidates().len())
    };
    let deliverer = Arc::new(StubDeliverer::refuse());
    let followup = serde_json::to_value(SendAgentMessageRequest {
        agent_id: "c1".into(),
        message: "re-task the finished child".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    })
    .unwrap();
    let outcome = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    )
    .send_with("op-followup-refused".into(), &followup)
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(outcome_json(&outcome)["code"], "capacity");
    assert!(
        outcome_json(&outcome)["message"]
            .as_str()
            .unwrap()
            .contains("rolled back"),
        "{outcome:?}"
    );
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    let tree = tree.lock().unwrap();
    let node = tree.node("c1").unwrap();
    assert_eq!(node.status, NodeStatus::Completed);
    assert_eq!(node.attempt_id.as_ref().unwrap().as_str(), "c1-a1");
    assert_eq!(node.output_schema_json, schema_before);
    assert!(!node.active_turn);
    assert_eq!(
        tree.attempt("c1").unwrap().status,
        crate::protocol::AttemptStatus::Completed
    );
    assert!(
        tree.attempts().keys().all(|id| id.as_str() != "c1-a2"),
        "the refused start must leave no attempt row"
    );
    assert_eq!(tree.candidates().len(), candidates_before);
    let opening = &deliverer.calls.lock().unwrap()[0].message_id;
    assert_eq!(
        tree.mail(opening).unwrap().durable,
        MailDurableState::Acknowledged,
    );
    assert_eq!(
        tree.operations()["op-followup-refused"].terminal_code,
        Some(TopologyErrorCode::Capacity),
    );
}

/// A send that heard `TurnEnded` while the interrupted turn's waiter was
/// registered, whose waiter then ends without releasing the cancelled
/// turn (a panicked or aborted waiter), fails closed at its next look
/// instead of waiting out the budget for a release nobody will make
/// (spec 03 section 9).
#[tokio::test(start_paused = true)]
async fn a_send_whose_turn_waiter_left_without_releasing_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    interrupted_unwinding_child(&driver).await;
    let deliverer = Arc::new(StubDeliverer::turn_ended());
    let tool = Arc::new(send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    ));
    let started = tokio::time::Instant::now();
    let pending = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move {
            tool.send_with(
                "op-unwatched".into(),
                &send_request("c1", "use the v2 schema", None),
            )
            .await
            .unwrap()
        }
    });
    until_calls(&deliverer, 1).await;
    // The waiter ends without releasing the turn.
    tree.lock().unwrap().leave_turn_waiter("c1");
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the send waited {:?} for a release no waiter will make",
        started.elapsed()
    );
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        outcome_json(&outcome)["code"],
        json!(TopologyErrorCode::OperationRecoveryRequired)
    );
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    let tree = tree.lock().unwrap();
    assert_eq!(
        tree.node("c1").unwrap().status,
        NodeStatus::RecoveryRequired
    );
}
