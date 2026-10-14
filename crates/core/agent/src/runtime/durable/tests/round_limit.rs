use super::*;

async fn round_limit_after_server_pause(durable: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 3, 9_800, false)
            .with_provider_pauses([0, 1])
            .with_assistant_continuations([2]),
    );
    let config = AgentConfig::default().with_max_model_rounds(2);
    let agent = if durable {
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        sessions
            .create(
                "create:round-limit-after-pause",
                durable_artifact_test_builder(workspace, Arc::clone(&model)).config(config),
            )
            .await?
    } else {
        artifact_test_builder(workspace, Arc::clone(&model))
            .config(config)
            .build()?
    };
    let result = agent.prompt("bounded server pause").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!("expected RoundLimit, got {:?}", result.outcome()).into());
    };
    assert_eq!(error.kind(), AgentErrorKind::RoundLimit, "{error:?}");
    assert_eq!(error.store_error(), None);
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    let checkpoint = result.checkpoint();
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let operations = checkpoint.manifest().ledger().operations();
    let pauses = operations
        .iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::ServerPauseContinuation { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(pauses.len(), 2);
    for pause in pauses {
        let transitions = pause.transitions();
        assert_eq!(transitions.len(), 2);
        assert!(matches!(
            transitions[0].state(),
            DurableOperationState::ContinuationRequired(_)
        ));
        assert!(matches!(
            transitions[1].state(),
            DurableOperationState::Finalized(_)
        ));
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn round_classifier_driver_round_limit_after_server_pause_memory() -> TestResult {
    round_limit_after_server_pause(false).await
}

#[tokio::test]
async fn round_classifier_driver_round_limit_after_server_pause_durable() -> TestResult {
    round_limit_after_server_pause(true).await
}
