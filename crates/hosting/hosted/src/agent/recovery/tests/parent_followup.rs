//! Explicit parent consent recovers an uncertain child before a fresh attempt.

use super::*;

#[tokio::test]
async fn settled_recovery_remains_visible_when_followup_admission_refuses() {
    let fixture = Fixture::new("parent-recovery-followup-refused");
    saturated_retask::pin_native_template(&fixture).await;
    let id = fixture.stuck_child().await;
    let before = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().clone())
        .unwrap();
    let dispatched = fixture.dispatches();
    let (ok, refused) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": id, "purpose": "followup", "message": "fresh task",
                "acknowledge_recovery": before.attempt_id, "output_schema_json": "[]",
            }),
        )
        .await;
    assert!(!ok, "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(
        message.contains("a tool may have changed this workspace"),
        "{message}"
    );
    assert!(message.contains("Recovery settled"), "{message}");
    assert_eq!(fixture.status(&id), NodeStatus::Interrupted);
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
            .unwrap(),
        before.attempt_id
    );
    assert_eq!(fixture.dispatches(), dispatched);
    let (ok, steer) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": id, "message": "continue", "priority": "urgent",
            }),
        )
        .await;
    assert!(!ok, "{steer}");
    let hint = steer["message"].as_str().unwrap();
    assert!(hint.contains("acknowledge_recovery"), "{hint}");
    assert!(hint.contains(before.attempt_id.as_str()), "{hint}");
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn parent_consent_does_not_bypass_a_refused_recovery_or_start_a_replacement() {
    let fixture = Fixture::new("parent-child-recovery-refused");
    saturated_retask::pin_native_template(&fixture).await;
    let id = fixture.stuck_child().await;
    let attempt = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
        .unwrap();
    let dispatched = fixture.dispatches();
    refuse_checkpoints(&fixture.database());
    let (ok, refused) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": id, "purpose": "followup", "message": "fresh task",
                "acknowledge_recovery": attempt,
            }),
        )
        .await;
    assert!(!ok, "{refused}");
    assert_eq!(code(&refused), "recovery_storage_unavailable");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("a tool may have changed this workspace")
    );
    assert_eq!(fixture.status(&id), NodeStatus::RecoveryRequired);
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
            .unwrap(),
        attempt
    );
    assert_eq!(
        fixture.dispatches(),
        dispatched,
        "a refusal admits no paid work"
    );
    admit_checkpoints(&fixture.database());
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

#[tokio::test]
async fn a_parent_acknowledges_unknown_child_recovery_and_starts_a_fresh_attempt() {
    let fixture = Fixture::new("parent-acknowledges-child-recovery");
    saturated_retask::pin_native_template(&fixture).await;
    let id = fixture.stuck_child().await;
    let before = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().clone())
        .unwrap();
    let dispatched = fixture.dispatches();
    let (ok, refused) = fixture
        .invoke(
            "send_agent_message",
            json!({ "agent_id": id, "message": "new task", "purpose": "followup" }),
        )
        .await;
    assert!(
        !ok,
        "ordinary followup must not consent to recovery: {refused}"
    );

    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    let _binding = fixture.binding.install(Arc::new(move |_, _, _| {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        let script = serde_json::from_value(json!({
            "version": 1, "steps": [{"output": {"text": "fresh task completed"}}],
        }))
        .map_err(|error| error.to_string())?;
        plakous_offline_crash_harness::projected_scripted_runtime_binding(
            &workspace,
            controller,
            Arc::new(script),
        )
        .map_err(|error| error.to_string())
    }));
    let (ok, receipt) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": id,
                "message": "inspect the workspace, then do this fresh task",
                "purpose": "followup",
                "refresh": false,
                "acknowledge_recovery": before.attempt_id,
            }),
        )
        .await;
    let state = fixture
        .runtime
        .read(|tree| {
            (
                tree.node(&id).unwrap().clone(),
                tree.attempt(&id).unwrap().clone(),
                tree.loss_continuation(&id),
                tree.recovery_need(&id),
                tree.has_current_turn_owner(&id),
                tree.recovery_owed(&id),
            )
        })
        .unwrap();
    assert!(ok, "{receipt}; {state:#?}");
    assert_eq!(receipt["disposition"], "started");
    assert!(
        receipt["recovery_warning"]
            .as_str()
            .unwrap()
            .contains("a tool may have changed this workspace")
    );
    let next = fixture
        .runtime
        .read(|tree| tree.attempt(&id).unwrap().clone())
        .unwrap();
    assert_ne!(next.attempt_id, before.attempt_id);
    assert!(next.turn_token > before.turn_token);
    assert_ne!(next.turn_id, before.turn_id);
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.attempt_status(&id) == AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the parent's fresh attempt finishes");
    assert_eq!(
        fixture.attempt_status(&id),
        AttemptStatus::CompletedWithoutSubmission
    );
    assert_eq!(
        fixture.dispatches(),
        dispatched + 3,
        "only the fresh task and its two submission nudges dispatch; the uncertain turn is never replayed"
    );
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}
