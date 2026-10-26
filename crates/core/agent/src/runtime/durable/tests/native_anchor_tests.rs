use super::*;

#[tokio::test]
async fn native_foreign_artifact_refusal_uses_one_artifact_free_harness_dispatch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let fixture = |candidate_calls| {
        placeholder_fixture_producing_artifacts(
            workspace.clone(),
            916_000,
            40_000,
            &[],
            &[],
            true,
            LocalDurabilityDisposition::Ephemeral,
            candidate_calls,
        )
    };
    let (builder, source_model) = fixture(16)?;
    let agent = builder.with_scheduled_compaction_enabled(false).build()?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let source = agent.checkpoint();
    let artifact = source
        .manifest()
        .family_artifact()
        .ok_or("active artifact")?;
    let foreign = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"foreign-native-replay-binding"],
    )?;
    assert_ne!(artifact.binding(), foreign);
    let (builder, model) = fixture(0)?;
    *lock_mutex(&model.native_artifact_binding) = Some(foreign);
    let topology = source.manifest().topology();
    let result = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:foreign-native-artifact".into(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("foreign-artifact compaction: {error:?}"))?;
    assert!(
        matches!(result, AgentCompactSessionOutcome::Compacted { .. }),
        "{result:?}"
    );
    assert_eq!(model.compaction_starts.load(Ordering::Acquire), 1);
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(source_model.calls.load(Ordering::Acquire), 2);
    let contexts = model.observed_compaction_contexts();
    assert!(contexts[0].active_family_artifact().is_some());
    assert!(
        contexts[1..]
            .iter()
            .all(|context| context.active_family_artifact().is_none())
    );
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("summary provenance")?
            .strategy(),
        RuntimeCompactionStrategy::SemanticSummaryPrefixV1
    );
    agent.shutdown().await?;
    Ok(())
}

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
