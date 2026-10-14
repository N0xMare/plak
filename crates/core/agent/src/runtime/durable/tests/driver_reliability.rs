use super::*;

#[tokio::test]
async fn failed_paid_fold_without_placeholders_refuses_a_nonfitting_successor() -> TestResult {
    nonfitting_successor_after_fold(false).await
}

#[tokio::test]
async fn unpreparable_fold_without_placeholders_refuses_a_nonfitting_successor() -> TestResult {
    nonfitting_successor_after_fold(true).await
}

async fn nonfitting_successor_after_fold(unpreparable: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        345_000,
        40_000,
        &[180_000],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model
        .fail_compaction_prepare
        .store(unpreparable, Ordering::Release);
    model
        .fail_compaction_call
        .store(!unpreparable, Ordering::Release);
    model
        .capacity_loss_after_fold
        .store(true, Ordering::Release);
    model
        .tool_successor_capacity_loss
        .store(40_000, Ordering::Release);
    let agent = builder.build()?;
    assert!(matches!(
        agent
            .prompt("older intent")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let result = agent.prompt("read the big file").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.model_kind() == Some(ModelErrorKind::ContextLimit)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2 + usize::from(!unpreparable),
        "no overflowing continuation dispatch"
    );
    assert_eq!(result.model_rounds(), 1);
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(
        placeholder_bodies(agent.checkpoint().manifest().canonical().history()),
        [] as [String; 0]
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn near_ceiling_wire_refuses_a_genuinely_nonfitting_tool_successor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        346_000,
        1_000_000,
        &[100_000],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(900_000, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(980_000, Ordering::Release);
    model.usage_suppressed_until.store(1, Ordering::Release);
    model.fail_compaction_call();
    model
        .tool_successor_capacity_loss
        .store(1_000_000, Ordering::Release);
    let agent = builder.build()?;
    assert!(matches!(
        agent
            .prompt("older intent")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let result = agent.prompt("read the big file").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.model_kind() == Some(ModelErrorKind::ContextLimit)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(result.model_rounds(), 1);
    assert_eq!(result.tool_calls(), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn exhausted_pressure_ladder_with_zero_headroom_names_its_refusal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = placeholder_test_descriptor(1_128, false)?;
    let fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        4,
        342_000,
        false,
    )
    .with_tool_call_on(2)
    .with_context_tokens_from_prepare(3, 1_000);
    fixture
        .fail_compaction_prepare
        .store(true, Ordering::Release);
    let model = Arc::new(fixture);
    let agent = scheduled_tool_group_fence_builder_with_model(
        workspace,
        true,
        model.clone(),
        LocalDurabilityDisposition::Ephemeral,
    )?
    .build()?;
    for prompt in ["older intent", "latest intent"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let result = agent.prompt("use the probe").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        panic!(
            "zero-headroom irreducible head continued: {:?}",
            result.outcome()
        );
    };
    assert_eq!(error.model_kind(), Some(ModelErrorKind::ContextLimit));
    for field in ["rung=", "outcome=", "refusal=", "headroom is 0"] {
        assert!(error.message().contains(field), "{}", error.message());
    }
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_pressure_fold_with_positive_headroom_continues() -> TestResult {
    for refuse_preparation in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = placeholder_test_descriptor(5_200, false)?;
        let mut fixture = ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            341_000,
            false,
        )
        .with_tool_call_on(2)
        .with_context_tokens_from_prepare(3, 1_000);
        fixture.response_provenance = true;
        fixture
            .fail_compaction_prepare
            .store(refuse_preparation, Ordering::Release);
        fixture
            .fail_compaction_call
            .store(!refuse_preparation, Ordering::Release);
        let model = Arc::new(fixture);
        let agent = scheduled_tool_group_fence_builder_with_model(
            workspace,
            true,
            model.clone(),
            LocalDurabilityDisposition::Ephemeral,
        )?
        .build()?;
        for prompt in ["older replaceable intent", "latest protected intent"] {
            assert!(matches!(
                agent.prompt(prompt).await?.result().await?.outcome(),
                TurnResultOutcome::Completed(_)
            ));
        }
        let result = agent
            .prompt("use the probe with a narrow envelope")
            .await?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(result.model_rounds(), 2);
        assert_eq!(result.tool_calls(), 1);
        assert_eq!(
            model.paid_compaction_calls.load(Ordering::Acquire),
            usize::from(!refuse_preparation)
        );
        agent.shutdown().await?;
    }
    Ok(())
}
