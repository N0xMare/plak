use super::*;

#[tokio::test]
async fn recovered_child_with_a_full_operation_ledger_accepts_followup() {
    let fixture = Fixture::new("saturated-child-retask");
    pin_native_template(&fixture).await;
    let id = fixture.spawn();
    let (session, handle, _) = fixture.start_failing_turn(&fixture.sessions(), &id).await;
    Fixture::await_unknown_outcome(&handle).await;
    let locator = session.release().await.unwrap();
    let locator = plakous_offline_crash_harness::seed_saturated_operation_ledger(
        &fixture.database(),
        locator,
        handle.turn_id(),
    )
    .await
    .unwrap();
    fixture.bind_turn(&id, &locator, handle.turn_id());
    fixture
        .runtime
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| {
                tree.record_turn_failure(
                    &id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    "unknown child outcome".into(),
                )
                .map_err(|error| error.message)
            }
        })
        .await
        .unwrap();
    let dispatched = fixture.dispatches();
    let (ok, receipt) = fixture.recover_agent(&id).await;
    assert!(ok, "{receipt}");
    assert_eq!(receipt["recovered"], true);
    assert_eq!(
        fixture.dispatches(),
        dispatched,
        "recovery never dispatches"
    );
    fixture
        .runtime
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| {
                tree.record_turn_completed(&id)
                    .map_err(|error| error.message)
            }
        })
        .await
        .unwrap();
    let workspace = fixture.root.path().to_path_buf();
    let evidence = fixture.evidence.clone();
    let _binding = fixture.binding.install(Arc::new(move |_, _, _| {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                .map_err(|error| error.to_string())?;
        let script = serde_json::from_value(json!({
            "version": 1, "steps": [{"output": {"text": "retask succeeded"}}],
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
                "agent_id": id, "message": "new task", "purpose": "followup",
            }),
        )
        .await;
    assert!(ok, "{receipt}");
    assert_eq!(receipt["disposition"], "started");
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.attempt_status(&id) == AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retasked child completes");
    assert_eq!(
        fixture.attempt_status(&id),
        AttemptStatus::CompletedWithoutSubmission
    );
    // The scripted child supplies text, not submit_result: its task and the
    // two bounded submission nudges each dispatch once, never the old turn.
    assert_eq!(fixture.dispatches(), dispatched + 3);
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

async fn pin_native_template(fixture: &Fixture) {
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.pin_bind_hints_for_test(crate::orchestration::BindHints::from_names(
                &["claude"],
                &[],
            ));
            tree.pin_template(
                "review",
                crate::orchestration::RosterBinding::leaf("claude", "test-model"),
            );
            Ok(())
        })
        .await
        .unwrap();
}
