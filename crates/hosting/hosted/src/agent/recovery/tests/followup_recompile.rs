//! A settled child whose provider configuration changed is recompiled by the
//! parent's follow-up, or the follow-up is refused, and neither path wedges it.
#![allow(
    clippy::too_many_lines,
    reason = "each follow-up admission path stays in one test"
)]

use std::sync::Arc;

use plakous::sessions::{CreateSessionOutcome, CreateSessionRequest};
use plakous_offline_crash_harness::BoundaryController;
use plakous_offline_crash_harness::protocol::CrashBoundary;

use super::*;

async fn settled_scripted_child(fixture: &Fixture) -> (String, String) {
    saturated_retask::pin_native_template(fixture).await;
    let id = fixture.spawn();
    let sessions = fixture
        .parent
        .state_roots()
        .child_runtime_sessions()
        .unwrap();
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &fixture.evidence)
            .unwrap();
    let script = serde_json::from_value(json!({
        "version": 1,
        "steps": [{"output": {"text": "first task done"}}],
    }))
    .unwrap();
    let binding = plakous_offline_crash_harness::projected_scripted_runtime_binding(
        fixture.root.path(),
        controller,
        Arc::new(script),
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("created session required");
    };
    let result = session
        .agent()
        .prompt("do the first task")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let locator = session.release().await.unwrap().to_string();
    fixture
        .runtime
        .mutate_then_emit(false, {
            let (id, locator) = (id.clone(), locator.clone());
            move |tree| {
                tree.bind_session_locator(&id, locator)
                    .map_err(|error| error.message)?;
                tree.record_turn_completed(&id)
                    .map_err(|error| error.message)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    // A turn that finishes without submit_result is CompletedWithoutSubmission.
    // The node face for that is Failed, and it is still a settled child.
    assert_eq!(
        fixture.attempt_status(&id),
        AttemptStatus::CompletedWithoutSubmission
    );
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    (id, locator)
}

fn install_script(fixture: &Fixture, account: &'static [u8], maximum_projected_input: u64) {
    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    let guard = fixture.binding.install(Arc::new(move |_, _, _| {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        let script = serde_json::from_value(json!({
            "version": 1,
            "steps": [{"output": {"text": "follow-up done"}}],
        }))
        .map_err(|error| error.to_string())?;
        plakous_offline_crash_harness::projected_scripted_runtime_binding_with(
            &workspace,
            controller,
            Arc::new(script),
            None,
            account,
            maximum_projected_input,
        )
        .map_err(|error| error.to_string())
    }));
    // The guard must outlive the follow-up. Leak it for the process-local test.
    std::mem::forget(guard);
}

#[tokio::test]
async fn followup_recompiles_changed_provider_configuration_and_continues() {
    let fixture = Fixture::new("followup-recompile-config");
    let (id, before) = settled_scripted_child(&fixture).await;
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let (ok, receipt) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": id,
                "message": "do the next task",
                "purpose": "followup",
                "refresh": false,
            }),
        )
        .await;
    assert!(ok, "{receipt}");
    let notice = receipt["configuration_notice"].as_str().unwrap_or("");
    assert!(
        notice.contains("recompiled provider configuration") && notice.contains("not failover"),
        "{receipt}"
    );
    assert_eq!(receipt["disposition"], "started");
    let after = fixture.locator(&id).unwrap();
    assert_ne!(after, before, "the node records the rebound head");
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.attempt_status(&id) == AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recompiled follow-up settles");
    assert_ne!(fixture.status(&id), NodeStatus::RecoveryRequired);
    assert_ne!(fixture.attempt_status(&id), AttemptStatus::RecoveryRequired);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn followup_refuses_a_changed_account_and_leaves_the_child_settled() {
    let fixture = Fixture::new("followup-binding-changed");
    let (id, locator) = settled_scripted_child(&fixture).await;
    install_script(&fixture, b"offline-loopback-account-other", 10_000_000);
    let (ok, receipt) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": id,
                "message": "do the next task",
                "purpose": "followup",
                "refresh": false,
            }),
        )
        .await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "followup_binding_changed", "{receipt}");
    let message = receipt["message"].as_str().unwrap_or("");
    assert!(
        message.contains("spawn a new child from the template"),
        "{message}"
    );
    assert_eq!(fixture.status(&id), NodeStatus::Failed);
    assert_eq!(
        fixture.attempt_status(&id),
        AttemptStatus::CompletedWithoutSubmission
    );
    assert_eq!(fixture.locator(&id).as_deref(), Some(locator.as_str()));
    let (closed, close) = fixture
        .invoke("close_agent", json!({ "agent_id": id }))
        .await;
    assert!(closed, "{close}");
    assert_eq!(fixture.status(&id), NodeStatus::Closed);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn list_says_the_host_is_recovering_only_while_recovery_holds_the_child() {
    let fixture = Fixture::new("list-recovery-wording");
    let id = fixture.stuck_child().await;
    let row = listed_row(&fixture, &id).await;
    let recovery = row["recovery"].as_str().unwrap_or("");
    assert!(recovery.contains("recovery has not started"), "{row}");
    assert!(!recovery.contains("the host is recovering"), "{row}");
    assert_eq!(row["remedy"], "close_agent");
    let claim = fixture
        .runtime
        .claim_recovery(&id)
        .expect("the child is free to recover");
    let row = listed_row(&fixture, &id).await;
    assert_eq!(
        row["recovery"], "the host is recovering its session; no call is needed",
        "{row}"
    );
    assert!(row.get("remedy").is_none(), "{row}");
    drop(claim);
    let row = listed_row(&fixture, &id).await;
    assert!(
        row["recovery"]
            .as_str()
            .unwrap_or("")
            .contains("recovery has not started"),
        "{row}"
    );
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

async fn listed_row(fixture: &Fixture, id: &str) -> serde_json::Value {
    let (ok, listed) = fixture
        .invoke("list_agents", json!({ "include_completed": true }))
        .await;
    assert!(ok, "{listed}");
    listed["agents"]
        .as_array()
        .and_then(|agents| {
            agents
                .iter()
                .find(|row| row["agent_id"].as_str() == Some(id))
                .cloned()
        })
        .unwrap_or_else(|| panic!("missing {id} in {listed}"))
}

fn rebind_checkpoints(fixture: &Fixture) -> i64 {
    rusqlite::Connection::open(fixture.database())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM fork_receipts WHERE transition = 2",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

async fn send(
    fixture: &Fixture,
    caller: Option<&str>,
    operation: &str,
    arguments: serde_json::Value,
) -> (bool, serde_json::Value) {
    let receipt = crate::agent::invoke_topology_tool_on_hub(
        &fixture.tree_id,
        caller,
        "send_agent_message",
        &arguments,
        operation,
    )
    .await
    .unwrap();
    (
        receipt.succeeded,
        serde_json::from_str(&receipt.detail).unwrap_or(serde_json::Value::String(receipt.detail)),
    )
}

fn followup_body(id: &str) -> serde_json::Value {
    json!({
        "agent_id": id,
        "message": "do the next task",
        "purpose": "followup",
        "refresh": false,
    })
}

fn install_named(fixture: &Fixture, model: &str, adapter: &str) {
    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    let model = model.to_owned();
    let adapter = adapter.to_owned();
    let guard = fixture.binding.install(Arc::new(move |_, _, _| {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        let script = serde_json::from_value(json!({
            "version": 1,
            "steps": [{"output": {"text": "follow-up done"}}],
        }))
        .map_err(|error| error.to_string())?;
        plakous_offline_crash_harness::projected_scripted_runtime_binding_named(
            &workspace,
            controller,
            Arc::new(script),
            b"offline-loopback-account",
            10_000_000,
            &model,
            &adapter,
        )
        .map_err(|error| error.to_string())
    }));
    std::mem::forget(guard);
}

#[tokio::test]
async fn a_non_parent_followup_does_not_rebind() {
    let fixture = Fixture::new("followup-non-parent");
    let (id, locator) = settled_scripted_child(&fixture).await;
    let sibling = fixture.spawn();
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let (ok, receipt) = send(
        &fixture,
        Some(&sibling),
        "followup-non-parent",
        followup_body(&id),
    )
    .await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "grant_denied", "{receipt}");
    assert_eq!(rebind_checkpoints(&fixture), 0);
    assert_eq!(fixture.locator(&id).as_deref(), Some(locator.as_str()));
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn a_capacity_refusal_does_not_rebind() {
    let fixture = Fixture::new("followup-capacity");
    let (id, locator) = settled_scripted_child(&fixture).await;
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    fixture
        .store
        .capacity_waiting
        .store(true, Ordering::Release);
    fixture.store.capacity_full.store(true, Ordering::Release);
    let (ok, receipt) = send(&fixture, None, "followup-capacity", followup_body(&id)).await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "capacity", "{receipt}");
    assert_eq!(rebind_checkpoints(&fixture), 0);
    assert_eq!(fixture.locator(&id).as_deref(), Some(locator.as_str()));
    fixture.store.capacity_full.store(false, Ordering::Release);
    fixture
        .store
        .capacity_waiting
        .store(false, Ordering::Release);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn replaying_a_followup_returns_the_notice_and_does_not_rebind_again() {
    let fixture = Fixture::new("followup-replay");
    let (id, before) = settled_scripted_child(&fixture).await;
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let (ok, first) = send(&fixture, None, "followup-replay", followup_body(&id)).await;
    assert!(ok, "{first}");
    let notice = first["configuration_notice"].as_str().unwrap_or("");
    assert!(
        notice.contains("recompiled provider configuration"),
        "{first}"
    );
    assert_eq!(rebind_checkpoints(&fixture), 1);
    let (ok, second) = send(&fixture, None, "followup-replay", followup_body(&id)).await;
    assert!(ok, "{second}");
    assert_eq!(
        second["configuration_notice"],
        first["configuration_notice"]
    );
    assert_eq!(rebind_checkpoints(&fixture), 1);
    assert_ne!(fixture.locator(&id).as_deref(), Some(before.as_str()));
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn followup_refuses_a_changed_model() {
    let fixture = Fixture::new("followup-model-changed");
    let (id, locator) = settled_scripted_child(&fixture).await;
    install_named(
        &fixture,
        "offline-loopback-v2",
        "offline-loopback-adapter-v1",
    );
    let (ok, receipt) = send(&fixture, None, "followup-model", followup_body(&id)).await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "followup_binding_changed", "{receipt}");
    assert!(
        receipt["message"].as_str().unwrap_or("").contains("model"),
        "{receipt}"
    );
    assert_eq!(rebind_checkpoints(&fixture), 0);
    assert_eq!(fixture.locator(&id).as_deref(), Some(locator.as_str()));
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn followup_refuses_a_changed_adapter_profile() {
    let fixture = Fixture::new("followup-adapter-changed");
    let (id, locator) = settled_scripted_child(&fixture).await;
    install_named(
        &fixture,
        "offline-loopback-v1",
        "offline-loopback-adapter-v2",
    );
    let (ok, receipt) = send(&fixture, None, "followup-adapter", followup_body(&id)).await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "followup_binding_changed", "{receipt}");
    assert!(
        receipt["message"]
            .as_str()
            .unwrap_or("")
            .contains("adapter"),
        "{receipt}"
    );
    assert_eq!(rebind_checkpoints(&fixture), 0);
    assert_eq!(fixture.locator(&id).as_deref(), Some(locator.as_str()));
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn followup_refuses_a_head_that_is_not_quiescent() {
    let fixture = Fixture::new("followup-not-quiescent");
    saturated_retask::pin_native_template(&fixture).await;
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let evidence = fixture.evidence.clone();
    let release = fixture.root.path().join("release-dispatch");
    let workspace = fixture.root.path().to_path_buf();
    let controller = BoundaryController::quiet_gated_from_evidence_path(
        CrashBoundary::ProviderDispatchOne,
        &evidence,
        &release,
    )
    .unwrap();
    let script = serde_json::from_value(json!({
        "version": 1,
        "steps": [{"output": {"text": "still running"}}],
    }))
    .unwrap();
    let binding = plakous_offline_crash_harness::projected_scripted_runtime_binding(
        &workspace,
        controller,
        Arc::new(script),
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("created session required");
    };
    let turn = session
        .agent()
        .start(
            CommandId::new("followup-not-quiescent:start").unwrap(),
            "do the first task",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !plakous_offline_crash_harness::evidence::read_evidence(&evidence).is_ok_and(|rows| {
            rows.iter()
                .any(|row| row.boundary == CrashBoundary::ProviderDispatchOne)
        }) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch is held");
    let mut locator = session.locator();
    tokio::time::timeout(Duration::from_secs(10), async {
        while sessions.head_is_quiescent(locator).await.unwrap() {
            tokio::time::sleep(Duration::from_millis(20)).await;
            locator = session.locator();
        }
    })
    .await
    .expect("the held dispatch leaves a non-quiescent head");
    let recorded = locator.to_string();
    fixture
        .runtime
        .mutate_then_emit(false, {
            let (id, recorded) = (id.clone(), recorded.clone());
            move |tree| {
                tree.bind_session_locator(&id, recorded)
                    .map_err(|error| error.message)?;
                tree.record_turn_completed(&id)
                    .map_err(|error| error.message)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let (ok, receipt) = send(&fixture, None, "followup-busy", followup_body(&id)).await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "followup_binding_changed", "{receipt}");
    assert!(
        receipt["message"]
            .as_str()
            .unwrap_or("")
            .contains("quiescent"),
        "{receipt}"
    );
    assert_eq!(rebind_checkpoints(&fixture), 0);
    assert_eq!(fixture.locator(&id).as_deref(), Some(recorded.as_str()));
    std::fs::write(&release, "release").unwrap();
    drop(turn);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn followup_refuses_running_jobs_without_a_job_host() {
    let fixture = Fixture::new("followup-running-jobs");
    saturated_retask::pin_native_template(&fixture).await;
    let id = fixture.spawn();
    let sessions = fixture.sessions();
    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let plan = Arc::new(
        serde_json::from_value(json!({
            "version": 1,
            "process_tools": true,
            "steps": [
                {"output": {"tool": {"name": "process_session_start", "arguments": {
                    "command": "read line", "workdir": ".", "login": false,
                    "tty": false, "keep_stdin_open": true, "yield_time_ms": 0,
                    "max_output_bytes": 8192
                }}}, "finish": "tool_calls"},
                {"output": {"text": "job left running"}}
            ]
        }))
        .unwrap(),
    );
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let binding = plakous_offline_crash_harness::scripted_runtime_binding(
        &workspace,
        controller,
        Arc::clone(&plan),
    )
    .unwrap()
    .with_job_host(registry.clone());
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, &id).unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("created session required");
    };
    let agent = session.agent().clone();
    let turn = agent.prompt("start a job").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while agent.jobs().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the script starts a job");
    let result = turn.result().await.unwrap();
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(agent.jobs()[0].status, plakous::tools::JobStatus::Running);
    let locator = session.release_for_transfer().await.unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, {
            let (id, locator) = (id.clone(), locator.to_string());
            move |tree| {
                tree.bind_session_locator(&id, locator)
                    .map_err(|error| error.message)?;
                tree.record_turn_completed(&id)
                    .map_err(|error| error.message)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let (ok, receipt) = send(&fixture, None, "followup-jobs", followup_body(&id)).await;
    assert!(!ok, "{receipt}");
    assert_eq!(code(&receipt), "followup_binding_changed", "{receipt}");
    assert!(
        receipt["message"]
            .as_str()
            .unwrap_or("")
            .contains("running jobs"),
        "{receipt}"
    );
    assert_eq!(rebind_checkpoints(&fixture), 0);
    let head = locator.to_string();
    assert_eq!(fixture.locator(&id).as_deref(), Some(head.as_str()));
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn an_acknowledged_interrupted_followup_recompiles_after_recovery_settles() {
    let fixture = Fixture::new("followup-interrupted-recompile");
    saturated_retask::pin_native_template(&fixture).await;
    let id = fixture.stuck_child().await;
    let attempt = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
        .unwrap();
    // Recovery reopens the recorded projection. A changed catalog is the
    // follow-up's recompile, after that reopen has settled the attempt.
    install_script(&fixture, b"offline-loopback-account", 10_000_000);
    let (ok, receipt) = send(
        &fixture,
        None,
        "followup-interrupted",
        json!({
            "agent_id": id,
            "message": "do the next task",
            "purpose": "followup",
            "refresh": false,
            "acknowledge_recovery": attempt,
        }),
    )
    .await;
    assert!(ok, "{receipt}");
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.attempt_status(&id) == AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("acknowledged recovery settles");
    assert_ne!(fixture.status(&id), NodeStatus::RecoveryRequired);
    assert_eq!(rebind_checkpoints(&fixture), 0);
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let (ok, receipt) = send(
        &fixture,
        None,
        "followup-interrupted-recompile",
        followup_body(&id),
    )
    .await;
    assert!(ok, "{receipt}");
    let notice = receipt["configuration_notice"].as_str().unwrap_or("");
    assert!(
        notice.contains("recompiled provider configuration"),
        "{receipt}"
    );
    assert_eq!(rebind_checkpoints(&fixture), 1);
    assert_ne!(fixture.status(&id), NodeStatus::RecoveryRequired);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn list_says_recovery_is_pending_until_the_claim_is_held() {
    let fixture = Fixture::new("list-recovery-pending");
    let id = fixture.stuck_child().await;
    assert!(
        fixture.runtime.track_recovery(
            fixture
                .runtime
                .driver()
                .tasks()
                .spawn(std::future::pending())
        )
    );
    let row = listed_row(&fixture, &id).await;
    assert_eq!(row["recovery"], "recovery is pending", "{row}");
    assert!(row.get("remedy").is_none(), "{row}");
    let claim = fixture
        .runtime
        .claim_recovery(&id)
        .expect("the scheduled run has not claimed the child");
    let row = listed_row(&fixture, &id).await;
    assert_eq!(
        row["recovery"], "the host is recovering its session; no call is needed",
        "{row}"
    );
    drop(claim);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn a_crash_after_rebind_and_before_the_tree_save_converges() {
    let mut fixture = Fixture::new("followup-rebind-cut");
    let (id, before) = settled_scripted_child(&fixture).await;
    install_script(&fixture, b"offline-loopback-account", 8_000_000);
    let dispatched = fixture.dispatches();
    struct ClearCut;
    impl Drop for ClearCut {
        fn drop(&mut self) {
            crate::agent::children::CUT_AFTER_REBIND.store(false, Ordering::Release);
        }
    }
    let _clear = ClearCut;
    crate::agent::children::CUT_AFTER_REBIND.store(true, Ordering::Release);
    let tree_id = fixture.tree_id.clone();
    let child = id.clone();
    let task = tokio::spawn(async move {
        let _ = crate::agent::invoke_topology_tool_on_hub(
            &tree_id,
            None,
            "send_agent_message",
            &json!({
                "agent_id": child,
                "message": "do the next task",
                "purpose": "followup",
                "refresh": false,
            }),
            "followup-cut",
        )
        .await;
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while rebind_checkpoints(&fixture) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rebind committed");
    assert_eq!(fixture.locator(&id).as_deref(), Some(before.as_str()));
    task.abort();
    let _ = task.await;
    crate::agent::children::CUT_AFTER_REBIND.store(false, Ordering::Release);
    let persisted = fixture.store.load(&fixture.tree_id).unwrap().unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
    let runtime = crate::topology_runtime::insert_root_with(
        fixture.tree_id.clone(),
        ProductAgentTree::reopen(persisted),
        Arc::clone(&fixture.store) as Arc<dyn TreeStore>,
        None,
    )
    .await
    .unwrap();
    runtime.remember_rejoin_spec(Arc::new(fixture.parent.clone()));
    fixture.runtime = runtime;
    let row = listed_row(&fixture, &id).await;
    assert_ne!(row["status"], "recovery_required", "{row}");
    let (ok, receipt) = send(&fixture, None, "followup-after-cut", followup_body(&id)).await;
    assert!(ok, "{receipt}");
    assert_eq!(rebind_checkpoints(&fixture), 1, "{receipt}");
    assert_ne!(fixture.locator(&id).as_deref(), Some(before.as_str()));
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.attempt_status(&id) == AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the follow-up settles");
    let followup_dispatches = fixture.dispatches() - dispatched;
    assert!(
        (1..=4).contains(&followup_dispatches),
        "the follow-up turn dispatched {followup_dispatches} times"
    );
    let locator: SessionLocator = fixture.locator(&id).unwrap().parse().unwrap();
    let sessions = fixture.sessions();
    let archived = sessions.archive(locator).await.expect("archive");
    sessions
        .delete(archived.session_id(), archived.archived_revision())
        .await
        .expect("delete");
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}
