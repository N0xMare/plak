use super::*;

#[tokio::test]
async fn native_paid_dispatch_keeps_the_active_head_artifact_but_no_continuation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        915_000,
        40_000,
        &[72_000],
        &[1],
        true,
        LocalDurabilityDisposition::Ephemeral,
        16,
    )?;
    model.item_context.store(true, Ordering::Release);
    let agent = builder.build()?;
    for prompt in ["older replaceable intent", "read the big file"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
    }
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    let contexts = model.observed_compaction_contexts();
    assert!(!contexts.is_empty());
    assert!(
        contexts
            .iter()
            .all(|context| context.active_family_artifact().is_some())
    );
    assert!(
        contexts
            .iter()
            .all(|context| context.previous_response_metadata().is_none())
    );
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("native fold")?
            .strategy(),
        RuntimeCompactionStrategy::NativeResponsesTriggerV1
    );
    agent.shutdown().await?;
    Ok(())
}
