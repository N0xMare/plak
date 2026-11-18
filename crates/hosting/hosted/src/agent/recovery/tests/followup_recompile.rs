//! A settled child whose provider configuration changed is recompiled by the
//! parent's follow-up, or the follow-up is refused, and neither path wedges it.

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
