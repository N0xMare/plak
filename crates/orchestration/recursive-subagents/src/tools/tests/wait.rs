//! Waits, their receipts and what wakes them.

use super::*;

#[tokio::test]
async fn cancelling_pending_topology_wait_preserves_running_child() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "spawn-cancel-wait".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let waiter = wait_tool(driver.clone());
    let arguments = json!({"agent_ids": ["c1"], "timeout_ms": 30_000});
    let cancellation = plakous::model::CancellationToken::new();
    let mut pending = Box::pin(waiter.dispatch_cancellable(
        &ToolCallerLineage::Direct,
        "cancel-wait".into(),
        &arguments,
        cancellation.cancelled(),
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), pending).await;
    assert_eq!(
        result
            .expect("wait must cooperate before its 30s deadline")
            .unwrap_err()
            .kind(),
        ToolRuntimeErrorKind::Cancelled
    );
    assert_eq!(
        driver.tree.lock().unwrap().node("c1").unwrap().status,
        NodeStatus::Running
    );
    // Cancellation must not settle the child; its later completion still
    // returns normally through the ordinary wait path.
    driver
        .tree
        .lock()
        .unwrap()
        .record_turn_completed("c1")
        .unwrap();
    assert_eq!(
        waiter.wait_with(&arguments).await.unwrap().kind(),
        ToolOutcomeKind::Success
    );
}

#[tokio::test]
async fn cancelled_topology_wait_does_not_acknowledge_ready_mail() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "spawn-ready-mail".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let (messages, revision) = {
        let mut tree = driver.tree.lock().unwrap();
        tree.send_agent_message(
            "c1",
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "synthetic finding".into(),
                purpose: Some(MessagePurpose::Question),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
        (tree.messages().clone(), tree.revision)
    };
    let waiter = wait_tool(driver.clone());
    let arguments = json!({"agent_ids": ["c1"], "timeout_ms": 30_000});
    let cancellation = plakous::model::CancellationToken::new();
    cancellation.cancel();
    let error = waiter
        .dispatch_cancellable(
            &ToolCallerLineage::Direct,
            "cancel-ready-mail".into(),
            &arguments,
            cancellation.cancelled(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
    {
        let tree = driver.tree.lock().unwrap();
        assert_eq!(tree.messages(), &messages);
        assert_eq!(tree.revision, revision);
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
    }
    assert_eq!(
        waiter.wait_with(&arguments).await.unwrap().kind(),
        ToolOutcomeKind::Success
    );
    assert_ne!(driver.tree.lock().unwrap().messages(), &messages);
}

/// A child completing settles a pending `wait_agent` within one
/// poll interval, not at the timeout.
#[tokio::test]
async fn child_completion_settles_a_pending_wait_within_a_poll_interval() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-wait".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    let waiter = wait_tool(driver.clone());
    let arguments = json!({ "agent_ids": ["c1"], "timeout_ms": 30_000 });
    let started = tokio::time::Instant::now();
    let pending = tokio::spawn(async move { waiter.wait_with(&arguments).await });
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert!(
        !pending.is_finished(),
        "a running child does not settle the wait"
    );
    tree.lock().unwrap().record_turn_completed("c1").unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), pending)
        .await
        .expect("the wait returns well before its 30 s timeout")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    let output = outcome_json(&outcome);
    assert_eq!(output["timed_out"], false);
    assert_eq!(output["agents"][0]["agent_id"], "c1");
    assert_eq!(output["agents"][0]["status"], "failed");
    assert_eq!(
        output["agents"][0]["attempt"],
        "completed_without_submission"
    );
    assert!(
        output["agents"][0].get("output_json").is_none(),
        "no submission, no output on the wire"
    );
}

/// The spawner reads the delegated result off the wait row (and the full
/// list row); nothing else carries `output_json` back up the tree.
#[tokio::test]
async fn a_submitted_result_rides_the_wait_row_and_the_full_list_row() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-out".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
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
    }
    let outcome = wait_tool(driver.clone())
        .wait_with(&json!({ "agent_ids": ["c1"], "timeout_ms": 1000 }))
        .await
        .unwrap();
    let output = outcome_json(&outcome);
    assert_eq!(output["timed_out"], false);
    assert_eq!(output["agents"][0]["status"], "completed");
    assert_eq!(output["agents"][0]["attempt"], "completed");
    assert_eq!(output["agents"][0]["output_json"], r#"{"ok":true}"#);
    let listed = tree
        .lock()
        .unwrap()
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: Some(true),
            },
        )
        .unwrap();
    assert_eq!(listed.agents[0]["agent_id"], "c1");
    assert_eq!(listed.agents[0]["output_json"], r#"{"ok":true}"#);
}

/// A bare `wait_agent {}` hands back each submitted result once; after
/// the last one it refuses instead of returning the same rows again.
#[tokio::test]
async fn bare_waits_return_each_result_once_then_refuse() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    for key in ["a", "b"] {
        let mut request = spawn_request();
        request.request_key = Some(format!("wave-{key}"));
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            format!("op-spawn-{key}"),
            &serde_json::to_value(request).unwrap(),
        )
        .await
        .unwrap();
    }
    let mut seen = Vec::new();
    for child in ["c2", "c1"] {
        {
            let mut tree = tree.lock().unwrap();
            tree.submit_result(
                child,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(child).unwrap();
        }
        let outcome = wait_tool(driver.clone())
            .wait_with(&json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        let output = outcome_json(&outcome);
        let results: Vec<String> = output["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["output_json"].is_string())
            .map(|row| row["agent_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(results, [child]);
        seen.push(child);
    }
    assert_eq!(seen, ["c2", "c1"]);
    let refused = wait_tool(driver.clone())
        .wait_with(&json!({ "timeout_ms": 1000 }))
        .await
        .unwrap();
    assert_eq!(outcome_json(&refused)["code"], "no_pending_children");
}

/// A child that cannot start is terminal with a reason at once, so
/// a `wait_agent` on it returns instead of parking behind a `running` node
/// nothing is driving.
#[tokio::test]
async fn a_child_that_cannot_start_is_faulted_with_a_reason_and_settles_waits() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let outcome = spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::fail()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-nostart".into(),
        &serde_json::to_value(spawn_request()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    let output = outcome_json(&outcome);
    assert_eq!(output["code"], "model_not_admitted");
    let message = output["message"].as_str().unwrap().to_owned();
    assert!(message.contains("child c1 could not start"), "{message}");
    assert!(
        message.contains("child session could not be created"),
        "{message}"
    );
    assert!(message.contains("no session was created"), "{message}");
    {
        let tree = tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Failed);
        assert!(!tree.node("c1").unwrap().active_turn);
        assert_eq!(tree.fault_reason("c1"), Some(message.as_str()));
        let attempt = tree.attempt("c1").unwrap();
        assert_eq!(attempt.status, crate::protocol::AttemptStatus::Failed);
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .map(|failure| failure.kind.as_str()),
            Some("spawn_refused")
        );
    }
    let started = tokio::time::Instant::now();
    let outcome = wait_tool(driver.clone())
        .wait_with(&json!({ "agent_ids": ["c1"], "timeout_ms": 30_000 }))
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let output = outcome_json(&outcome);
    assert_eq!(output["timed_out"], false);
    assert_eq!(output["agents"][0]["status"], "failed");
}

#[tokio::test]
async fn abandoned_uncommitted_wait_receipt_restores_mail_even_after_history_pruning() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "receipt-spawn".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let mail = driver
        .tree
        .lock()
        .unwrap()
        .send_agent_message(
            "c1",
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "uncommitted question".into(),
                purpose: Some(MessagePurpose::Question),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    wait_tool(driver.clone())
        .wait_operation_with("abandoned-wait", &json!({}))
        .await
        .unwrap();
    {
        let mut tree = driver.tree.lock().unwrap();
        for _ in 0..70 {
            let next = tree
                .send_agent_message(
                    "c1",
                    &SendAgentMessageRequest {
                        agent_id: "root".into(),
                        message: "later acknowledged mail".into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            tree.acknowledge_mail(&next.message_id).unwrap();
        }
        driver.save(&mut tree).unwrap();
    }
    // Reopen the durable cut after receipt persistence but before the
    // tool result reached canonical history. History pruning is not loss.
    let mut reopened = reload(&store, "tree1");
    assert!(reopened.mail(&mail.message_id).is_none());
    assert_eq!(
        reopened
            .settle_wait_receipts("root", &[("abandoned-wait".into(), false)])
            .unwrap()
            .restored_mail,
        1
    );
    assert_eq!(
        reopened.mail(&mail.message_id).unwrap().durable,
        MailDurableState::Accepted
    );
    assert_eq!(
        reopened.pending_mail("root")[0].body,
        "uncommitted question"
    );
    assert_eq!(
        reopened
            .settle_wait_receipts("root", &[("abandoned-wait".into(), false)])
            .unwrap()
            .restored_mail,
        0
    );
}

/// Settlement follows canonical commit evidence per receipt: a committed
/// wait is retired with its acknowledgements standing, an uncommitted
/// one is rolled back, and settling again changes nothing.
#[tokio::test]
async fn committed_wait_evidence_retires_and_uncommitted_evidence_restores() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.record_turn_completed("c1").unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let waiter = wait_tool(driver.clone());
    waiter
        .wait_operation_with("committed-wait", &json!({}))
        .await
        .unwrap();
    {
        let mut tree = driver.tree.lock().unwrap();
        tree.send_agent_message(
            "c1",
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "uncommitted note".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    }
    waiter
        .wait_operation_with("uncommitted-wait", &json!({"agent_ids": ["c1"]}))
        .await
        .unwrap();
    let mut tree = driver.tree.lock().unwrap();
    assert_eq!(
        tree.pending_mail("root"),
        [] as [crate::tree::MailRecord; 0]
    );
    let mut retained = tree.retained_wait_receipts("root");
    retained.sort();
    assert_eq!(retained, ["committed-wait", "uncommitted-wait"]);
    let evidence = [
        ("committed-wait".to_owned(), true),
        ("uncommitted-wait".to_owned(), false),
    ];
    let settled = tree.settle_wait_receipts("root", &evidence).unwrap();
    assert_eq!(
        settled,
        crate::tree::WaitReceiptSettlement {
            retired: 1,
            rolled_back: 1,
            restored_mail: 1,
        }
    );
    assert_eq!(tree.retained_wait_receipts("root"), [] as [String; 0]);
    assert!(tree.delivered_to("root", "c1"));
    let pending = tree.pending_mail("root");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].body, "uncommitted note");
    assert_eq!(
        tree.settle_wait_receipts("root", &evidence).unwrap(),
        crate::tree::WaitReceiptSettlement::default()
    );
}

#[tokio::test]
async fn wait_recovery_only_undoes_deliveries_consumed_by_that_operation() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.record_turn_completed("c1").unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let waiter = wait_tool(driver.clone());
    waiter
        .wait_operation_with("uncommitted-settlement", &json!({}))
        .await
        .unwrap();
    {
        let mut tree = driver.tree.lock().unwrap();
        assert!(tree.delivered_to("root", "c1"));
        assert_eq!(
            tree.settle_wait_receipts("root", &[("uncommitted-settlement".into(), false)])
                .unwrap()
                .restored_mail,
            1
        );
        assert!(!tree.delivered_to("root", "c1"));
    }
    waiter
        .wait_operation_with("committed-settlement", &json!({}))
        .await
        .unwrap();
    waiter
        .wait_operation_with("uncommitted-reread", &json!({"agent_ids": ["c1"]}))
        .await
        .unwrap();
    let mut tree = driver.tree.lock().unwrap();
    assert_eq!(
        tree.settle_wait_receipts("root", &[("uncommitted-reread".into(), false)])
            .unwrap()
            .restored_mail,
        0
    );
    assert!(tree.delivered_to("root", "c1"));
    assert_eq!(
        tree.pending_mail("root"),
        [] as [crate::tree::MailRecord; 0]
    );
}

#[tokio::test]
async fn five_hundred_waits_across_committed_turns_keep_receipts_bounded() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let waiter = wait_tool(driver.clone());
    for turn in 0..500 {
        {
            let mut tree = driver.tree.lock().unwrap();
            // The previous turn committed and its terminal retired it.
            tree.retire_wait_receipts("root");
            tree.send_agent_message(
                "c1",
                &SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: format!("question {turn}"),
                    purpose: Some(MessagePurpose::Question),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        }
        let outcome = waiter
            .wait_operation_with(&format!("wait-{turn}"), &json!({}))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let tree = driver.tree.lock().unwrap();
        assert_eq!(
            tree.operations()
                .values()
                .filter(|op| op.tool_name == "wait_agent")
                .count(),
            1
        );
        assert!(tree.messages().len() <= 64);
    }
    let mut reopened = reload(&store, "tree1");
    assert_eq!(
        reopened
            .settle_wait_receipts("root", &[("wait-499".into(), false)])
            .unwrap()
            .restored_mail,
        1
    );
    assert_eq!(reopened.retire_wait_receipts("root"), 0);
}

#[tokio::test]
async fn committed_child_completion_retires_only_its_own_wait_receipts() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.grants_mut_for_test("root").max_depth = 3;
    tree.spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    tree.spawn_agent("c1", &spawn_request(), None).unwrap();
    tree.bind_attempt_turn("c1", "child-turn").unwrap();
    for (sender, recipient) in [("c1", "root"), ("c2", "c1")] {
        tree.send_agent_message(
            sender,
            &SendAgentMessageRequest {
                agent_id: recipient.into(),
                message: "question".into(),
                purpose: Some(MessagePurpose::Question),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    }
    let driver = driver_on(tree, Arc::clone(&store));
    wait_tool(driver.clone())
        .wait_operation_with("root-wait", &json!({}))
        .await
        .unwrap();
    let mut child_waiter = wait_tool(driver.clone());
    child_waiter.caller_id = Arc::new(Mutex::new("c1".into()));
    child_waiter
        .wait_operation_with("child-wait", &json!({}))
        .await
        .unwrap();
    let mut tree = driver.tree.lock().unwrap();
    assert!(tree.operations().contains_key("child-wait"));
    tree.set_active_turn("c1", true).unwrap();
    assert!(
        tree.operations().contains_key("child-wait"),
        "same bound-turn rejoin keeps rollback evidence"
    );
    tree.record_turn_completed("c1").unwrap();
    assert!(!tree.operations().contains_key("child-wait"));
    assert!(tree.operations().contains_key("root-wait"));
    driver.save(&mut tree).unwrap();
    let reopened = reload(&store, "tree1");
    assert!(!reopened.operations().contains_key("child-wait"));
    assert!(reopened.operations().contains_key("root-wait"));
}

/// Spec 03 section 4.4: a child terminal settles its own receipts. A
/// cancelled turn closed its exchange, so releasing it retires them; a
/// failure leaves them to the host's settlement, and neither the
/// followup that opens the next attempt nor its first turn retires them
/// blindly, so uncommitted evidence still restores the mail.
#[tokio::test]
async fn child_receipts_retire_by_terminal_kind_and_never_at_followup() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.grants_mut_for_test("root").max_depth = 3;
    tree.spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    tree.spawn_agent("c1", &spawn_request(), None).unwrap();
    tree.bind_attempt_turn("c1", "child-turn").unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let mut child_waiter = wait_tool(driver.clone());
    child_waiter.caller_id = Arc::new(Mutex::new("c1".into()));
    for wait in ["interrupted-wait", "failed-wait"] {
        driver
            .tree
            .lock()
            .unwrap()
            .send_agent_message(
                "c2",
                &SendAgentMessageRequest {
                    agent_id: "c1".into(),
                    message: wait.into(),
                    purpose: Some(MessagePurpose::Question),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        child_waiter
            .wait_operation_with(wait, &json!({ "agent_ids": ["c2"] }))
            .await
            .unwrap();
        let mut tree = driver.tree.lock().unwrap();
        assert_eq!(tree.pending_mail("c1"), [] as [crate::tree::MailRecord; 0]);
        if wait == "interrupted-wait" {
            tree.release_attempt_turn("c1").unwrap();
            assert_eq!(tree.retained_wait_receipts("c1"), [] as [String; 0]);
            tree.bind_attempt_turn("c1", "child-turn-2").unwrap();
        }
    }
    let mut tree = driver.tree.lock().unwrap();
    tree.record_turn_failure(
        "c1",
        crate::tree::turn_outcome::FailedTurnDisposition::Failed {
            kind: "turn_failed",
        },
        "contained panic".into(),
    )
    .unwrap();
    tree.settle_for_test();
    tree.send_agent_message(
        "root",
        &SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "again".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        },
        None,
    )
    .unwrap();
    tree.set_active_turn("c1", true).unwrap();
    assert_eq!(tree.retained_wait_receipts("c1"), ["failed-wait"]);
    let settled = tree
        .settle_wait_receipts("c1", &[("failed-wait".into(), false)])
        .unwrap();
    assert_eq!(settled.restored_mail, 1);
    let unread: Vec<String> = tree
        .pending_mail("c1")
        .into_iter()
        .filter(|mail| mail.from == "c2")
        .map(|mail| mail.body)
        .collect();
    assert_eq!(unread, ["failed-wait"]);
}

#[tokio::test]
async fn timeout_without_mail_does_not_publish_a_wait_receipt() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("timeout-no-write", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let outcome = wait_tool(driver.clone())
        .wait_operation_with("empty-wait", &json!({"timeout_ms": 1}))
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome_json(&outcome)["woke_by"], "timeout");
    assert!(driver.tree.lock().unwrap().parked_waits.is_empty());
    assert!(
        !driver
            .tree
            .lock()
            .unwrap()
            .operations()
            .contains_key("empty-wait")
    );
}

#[tokio::test]
async fn owned_process_wakes_only_waits_admitted_before_its_terminal_transition() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("process-no-write", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
    let waiter = wait_tool(driver.clone());
    let arguments = json!({"timeout_ms": 300_000});
    let mut pending = Box::pin(waiter.wait_operation_with("process-wait", &arguments));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome_json(&outcome)["woke_by"], "process");
    assert_eq!(outcome_json(&outcome)["processes"], json!(["p1"]));
    assert!(driver.tree.lock().unwrap().operations().is_empty());
    let next = waiter
        .wait_operation_with("next-process-wait", &json!({"timeout_ms": 1}))
        .await
        .unwrap();
    assert_eq!(outcome_json(&next)["woke_by"], "timeout");
}

#[tokio::test]
async fn wait_resolves_jobs_that_attach_after_admission() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("late-job-service", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let waiter = wait_tool(driver.clone());
    let arguments = json!({"timeout_ms": 300_000});
    let mut pending = Box::pin(waiter.wait_operation_with("late-job-wait", &arguments));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
    driver.read(|tree| tree.waiters().notify_waiters()).unwrap();
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome_json(&outcome)["woke_by"], "process");
}

/// A fan-out whose results together outgrow the tool outcome limit
/// used to fail every wait. The aggregate budget returns them in
/// batches: each return stays within it, a withheld result stays
/// pending with its result envelope unread, and every result arrives
/// exactly once.
#[tokio::test]
async fn a_fan_out_wait_past_the_outcome_limit_returns_in_budgeted_batches() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let children = ["c1", "c2", "c3", "c4", "c5", "c6"];
    for (index, child) in children.iter().enumerate() {
        let mut request = spawn_request();
        request.request_key = Some(format!("budget-{index}"));
        request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"blob":{"type":"string"}},"required":["blob"],"additionalProperties":false}"#.into();
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            format!("op-spawn-budget-{index}"),
            &serde_json::to_value(request).unwrap(),
        )
        .await
        .unwrap();
        let mut tree = tree.lock().unwrap();
        tree.submit_result(
            child,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: format!(r#"{{"blob":"{}"}}"#, child.repeat(100 * 1024)),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(child).unwrap();
    }
    let mut seen = Vec::new();
    for wave in 0..children.len() {
        let outcome = wait_tool(driver.clone())
            .wait_operation_with(
                &format!("budget-wait-{wave}"),
                &json!({ "timeout_ms": 1000 }),
            )
            .await
            .unwrap();
        let ToolResultPart::Text(block) = &outcome.parts()[0] else {
            panic!("expected text tool outcome");
        };
        assert!(block.text().len() <= crate::tree::WAIT_OUTPUT_BUDGET_BYTES);
        let output = outcome_json(&outcome);
        let delivered: Vec<String> = output["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["output_json"].is_string())
            .map(|row| row["agent_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(delivered.len(), 1, "wave {wave}: {}", output["omitted"]);
        let withheld = children.len() - wave - 1;
        if withheld == 0 {
            assert!(output.get("omitted").is_none());
        } else {
            assert_eq!(
                output["omitted"]["outputs"].as_array().unwrap().len(),
                withheld
            );
            assert_eq!(output["omitted"]["mail"], withheld);
            let tree = tree.lock().unwrap();
            for id in output["omitted"]["outputs"].as_array().unwrap() {
                assert!(
                    tree.unread_mail("root")
                        .any(|mail| mail.from == id.as_str().unwrap()),
                    "a withheld result keeps its envelope unread"
                );
            }
        }
        seen.extend(delivered);
    }
    seen.sort();
    assert_eq!(seen, children);
    let refused = wait_tool(driver.clone())
        .wait_with(&json!({ "timeout_ms": 1000 }))
        .await
        .unwrap();
    assert_eq!(outcome_json(&refused)["code"], "no_pending_children");
}

/// Mail the budget withheld from a child whose row the same return
/// delivered stays watched: the next bare wait returns it at once instead
/// of refusing `no_pending_children`.
#[tokio::test]
async fn withheld_mail_from_a_delivered_child_returns_on_the_next_wait() {
    withheld_mail_from_a_delivered_child_returns(true).await;
}

/// The same for a settled row without `output_json`: its Result envelope
/// is plain budgeted mail, and when the budget withholds it the next bare
/// wait still returns it.
#[tokio::test]
async fn a_withheld_result_envelope_of_a_delivered_row_returns_on_the_next_wait() {
    withheld_mail_from_a_delivered_child_returns(false).await;
}

async fn withheld_mail_from_a_delivered_child_returns(submit: bool) {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    let tree = Arc::clone(&driver.tree);
    let mut request = spawn_request();
    request.request_key = Some("probe-mail".to_owned());
    request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"blob":{"type":"string"}},"required":["blob"],"additionalProperties":false}"#.into();
    spawn_tool(
        driver.clone(),
        Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
    )
    .spawn_with(
        "op-spawn-probe-mail".into(),
        &serde_json::to_value(request).unwrap(),
    )
    .await
    .unwrap();
    {
        let mut tree = tree.lock().unwrap();
        // Without a 200 KiB output, enough mail alone overruns the budget.
        let count = if submit { 40 } else { 140 };
        for index in 0..count {
            tree.send_agent_message(
                "c1",
                &SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: format!("{index:03}{}", "m".repeat(2000)),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        }
        if submit {
            tree.submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: format!(r#"{{"blob":"{}"}}"#, "c".repeat(200 * 1024)),
                },
                None,
            )
            .unwrap();
        }
        tree.record_turn_completed("c1").unwrap();
    }
    let first = wait_tool(driver.clone())
        .wait_operation_with("probe-mail-1", &json!({ "timeout_ms": 1000 }))
        .await
        .unwrap();
    let first = outcome_json(&first);
    assert_eq!(first["agents"][0]["output_json"].is_string(), submit);
    let withheld = first["omitted"]["mail"].as_u64().unwrap();
    assert!(withheld > 0, "{}", first["omitted"]);
    let second = wait_tool(driver.clone())
        .wait_operation_with("probe-mail-2", &json!({ "timeout_ms": 1000 }))
        .await
        .unwrap();
    let second = outcome_json(&second);
    assert_eq!(
        second["mail"].as_array().map(Vec::len),
        Some(usize::try_from(withheld).unwrap()),
        "the next wait returns the withheld mail: {second}"
    );
    if !submit {
        assert_eq!(
            second["mail"].as_array().unwrap().last().unwrap()["purpose"],
            "result"
        );
    }
}

/// Spec 03 section 4.4: a bare wait delivers each settled row once. It
/// keeps watching a delivered child whose mail the budget withheld, and
/// the next bare wait returns that mail without the delivered
/// `output_json`, neither re-sent whole nor listed under
/// `omitted.rereads`: re-reads exist only for named waits.
#[tokio::test]
async fn a_bare_wait_over_withheld_mail_does_not_resend_a_delivered_output() {
    // Forty withheld envelopes leave room to re-send the output whole;
    // ninety leave none, so it would be listed as a re-read instead.
    for count in [40, 90] {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let mut request = spawn_request();
        request.request_key = Some("probe-reread".to_owned());
        request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"blob":{"type":"string"}},"required":["blob"],"additionalProperties":false}"#.into();
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-probe-reread".into(),
            &serde_json::to_value(request).unwrap(),
        )
        .await
        .unwrap();
        {
            let mut tree = tree.lock().unwrap();
            for index in 0..count {
                tree.send_agent_message(
                    "c1",
                    &SendAgentMessageRequest {
                        agent_id: "root".into(),
                        message: format!("{index:03}{}", "m".repeat(2000)),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            }
            tree.submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: format!(r#"{{"blob":"{}"}}"#, "c".repeat(200 * 1024)),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed("c1").unwrap();
        }
        let first = wait_tool(driver.clone())
            .wait_operation_with("probe-reread-1", &json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        let first = outcome_json(&first);
        assert!(first["agents"][0]["output_json"].is_string());
        assert!(first["omitted"]["mail"].as_u64().unwrap() > 0);
        let second = wait_tool(driver.clone())
            .wait_operation_with("probe-reread-2", &json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        let second = outcome_json(&second);
        assert!(
            !second["agents"][0]["output_json"].is_string(),
            "{count} envelopes: a bare wait re-sent the delivered output ({} bytes)",
            second["agents"][0]["output_json"]
                .as_str()
                .map_or(0, str::len)
        );
        assert!(
            second["omitted"]["rereads"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{count} envelopes: a bare wait listed a delivered output as a re-read: {}",
            second["omitted"]
        );
    }
}

#[tokio::test]
async fn steer_wakes_wait_without_mail_or_a_durable_receipt() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("steer-no-write", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let waiter = wait_tool(driver.clone());
    let (sender, receiver) = tokio::sync::watch::channel(plakous::tools::ToolPreempt::None);
    let arguments = json!({"timeout_ms": 300_000});
    let mut pending =
        Box::pin(waiter.wait_operation_preempt("steer-wait", &arguments, Some(receiver)));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    sender.send(plakous::tools::ToolPreempt::Steer).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    let output = outcome_json(&result);
    assert_eq!(output["woke_by"], "steer");
    assert_eq!(output["mail"], json!([]));
    assert!(driver.tree.lock().unwrap().operations().is_empty());
}

#[tokio::test]
async fn parked_hub_wait_is_the_only_consumer_of_a_child_question() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.grants_mut_for_test("root").max_depth = 3;
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "hub-spawn".into(),
            &serde_json::to_value(SpawnAgentRequest::test_orchestrate()).unwrap(),
        )
        .await
        .unwrap();
    {
        let mut tree = driver.tree.lock().unwrap();
        tree.spawn_agent("c1", &spawn_request(), None).unwrap();
    }
    let mut waiter = wait_tool(driver.clone());
    waiter.caller_id = Arc::new(Mutex::new("c1".into()));
    let arguments = json!({"agent_ids": ["c2"], "timeout_ms": 300_000});
    let mut pending = Box::pin(waiter.wait_operation_with("hub-wait", &arguments));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let deliverer = Arc::new(StubDeliverer::ok());
    let mut send = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    send.caller_id = Arc::new(Mutex::new("c2".into()));
    let arguments = json!({"agent_id": "c1", "message": "which file?", "purpose": "question", "priority": "urgent"});
    let (receipt, outcome) =
        tokio::join!(send.send_with("hub-question".into(), &arguments), pending);
    let receipt = receipt.unwrap();
    assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
    let output = outcome_json(&outcome.unwrap());
    assert_eq!(output["woke_by"], "mail");
    assert_eq!(output["mail"][0]["body"], "which file?");
    assert!(deliverer.calls.lock().unwrap().is_empty());
    assert_eq!(
        driver.tree.lock().unwrap().pending_mail("c1"),
        [] as [crate::tree::MailRecord; 0]
    );
}

#[tokio::test]
async fn watched_urgent_mail_beats_a_simultaneous_steer_flag() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.grants_mut_for_test("root").max_depth = 3;
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "hub-spawn".into(),
            &serde_json::to_value(SpawnAgentRequest::test_orchestrate()).unwrap(),
        )
        .await
        .unwrap();
    {
        let mut tree = driver.tree.lock().unwrap();
        tree.spawn_agent("c1", &spawn_request(), None).unwrap();
    }
    let mut waiter = wait_tool(driver.clone());
    waiter.caller_id = Arc::new(Mutex::new("c1".into()));
    let arguments = json!({"agent_ids": ["c2"], "timeout_ms": 300_000});
    let (sender, receiver) = tokio::sync::watch::channel(plakous::tools::ToolPreempt::None);
    let mut pending =
        Box::pin(waiter.wait_operation_preempt("hub-wait", &arguments, Some(receiver)));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let deliverer = Arc::new(StubDeliverer::ok());
    let mut send = send_tool(
        driver.clone(),
        Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
    );
    send.caller_id = Arc::new(Mutex::new("c2".into()));
    let arguments = json!({"agent_id": "c1", "message": "which file?", "purpose": "question", "priority": "urgent"});
    sender.send(plakous::tools::ToolPreempt::Steer).unwrap();
    let (receipt, outcome) =
        tokio::join!(send.send_with("hub-question".into(), &arguments), pending);
    let receipt = receipt.unwrap();
    assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
    let output = outcome_json(&outcome.unwrap());
    assert_eq!(output["woke_by"], "mail");
    assert_eq!(output["mail"][0]["body"], "which file?");
    assert!(deliverer.calls.lock().unwrap().is_empty());
    assert_eq!(
        driver.tree.lock().unwrap().pending_mail("c1"),
        [] as [crate::tree::MailRecord; 0]
    );
}

#[tokio::test]
async fn cancelling_a_parked_hub_wait_releases_mail_to_the_deliverer() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.grants_mut_for_test("root").max_depth = 3;
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "hub-spawn".into(),
            &serde_json::to_value(SpawnAgentRequest::test_orchestrate()).unwrap(),
        )
        .await
        .unwrap();
    driver
        .tree
        .lock()
        .unwrap()
        .spawn_agent("c1", &spawn_request(), None)
        .unwrap();
    let mut waiter = wait_tool(driver.clone());
    waiter.caller_id = Arc::new(Mutex::new("c1".into()));
    let wait_arguments = json!({"agent_ids": ["c2"]});
    let mut pending_wait =
        Box::pin(waiter.wait_operation_with("cancelled-hub-wait", &wait_arguments));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending_wait.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let deliverer = Arc::new(StubDeliverer::ok());
    let mut send = send_tool(driver.clone(), Some(deliverer.clone()));
    send.caller_id = Arc::new(Mutex::new("c2".into()));
    let send_arguments = json!({"agent_id": "c1", "message": "which file?", "purpose": "question", "priority": "urgent"});
    let mut pending_send = Box::pin(send.send_with("held-question".into(), &send_arguments));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending_send.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(deliverer.calls.lock().unwrap().is_empty());
    drop(pending_wait);
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(1), pending_send)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
    assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    assert_eq!(
        driver.tree.lock().unwrap().pending_mail("c1"),
        [] as [crate::tree::MailRecord; 0]
    );
}

#[tokio::test]
async fn a_failed_wait_receipt_publish_leaves_mail_unread() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("refused-wait-receipt", root.path());
    let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let request = SendAgentMessageRequest {
        agent_id: "root".into(),
        message: "question".into(),
        purpose: Some(MessagePurpose::Question),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    };
    let receipt = tree
        .send_agent_message(&child.agent_id, &request, None)
        .unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let output = wait_tool(driver.clone())
        .wait_with(&json!({}))
        .await
        .unwrap();
    assert_eq!(output.kind(), ToolOutcomeKind::Failure);
    driver
        .read(|tree| {
            assert_eq!(
                tree.mail(&receipt.message_id).unwrap().durable,
                MailDurableState::Accepted
            );
            assert!(
                tree.poll_wait("root", &AwaitAgents::Pending)
                    .unwrap()
                    .is_some()
            );
        })
        .unwrap();
}

#[tokio::test]
async fn wait_returns_full_mail_and_blocks_again_until_new_mail() {
    let root = tempfile::tempdir().unwrap();
    let (store, tree) = persist_fixture(&root);
    let driver = driver_on(tree, Arc::clone(&store));
    spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
        .spawn_with(
            "mail-wait-spawn".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
    let body = "€".repeat(682) + "ab";
    let send = |message: &str| SendAgentMessageRequest {
        agent_id: "root".into(),
        message: message.into(),
        purpose: Some(MessagePurpose::Question),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    };
    let first = driver
        .tree
        .lock()
        .unwrap()
        .send_agent_message("c1", &send(&body), None)
        .unwrap();
    let waiter = wait_tool(driver.clone());
    let arguments = json!({"timeout_ms": 1_800_000});
    let output = outcome_json(&waiter.wait_with(&arguments).await.unwrap());
    assert_eq!(output["woke_by"], "mail");
    assert_eq!(output["mail"][0]["message_id"], first.message_id);
    assert_eq!(output["mail"][0]["body"], body);
    assert_eq!(output["mail"][0]["purpose"], "question");
    assert!(output["agents"][0].get("usage").is_none());
    assert_eq!(
        reload(&store, "tree1")
            .mail(&first.message_id)
            .unwrap()
            .durable,
        MailDurableState::Acknowledged
    );
    for agent_ids in [Value::Null, json!(["c1"])] {
        let arguments = json!({"agent_ids": agent_ids, "timeout_ms": 1_800_000});
        let mut pending = Box::pin(waiter.wait_with(&arguments));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let next = driver
            .tree
            .lock()
            .unwrap()
            .send_agent_message("c1", &send("new question"), None)
            .unwrap();
        let output = outcome_json(&pending.await.unwrap());
        assert_eq!(output["mail"].as_array().unwrap().len(), 1);
        assert_eq!(output["mail"][0]["message_id"], next.message_id);
        assert_eq!(output["timed_out"], false);
    }
}
