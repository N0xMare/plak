//! Observation allowances on rounds without provider usage, and across the
//! durable effect-decision and server-pause frames.
use super::fence_compaction::compaction_workflow_count;
use super::*;

/// A round whose request was admitted on an unanchored estimate and whose
/// response reports no usage still grants half of the estimated headroom, so
/// a modest tool result is kept whole and no pressure fold runs.
#[tokio::test]
async fn a_usage_less_unanchored_round_keeps_half_its_estimated_headroom() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        0x0b5e_0001,
        1_000_000,
        &[25_000],
        &[0],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    // The wire's one-token-per-byte bound overflows the window while its
    // bytes/4 estimate fits, so admission delegates to the provider.
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(1_200_000, Ordering::Release);
    model
        .usage_suppressed_until
        .store(usize::MAX, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:usage-less-round", builder).await?;
    let result = agent.prompt("read the file").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(
        result
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_none(),
        "no round reported usage"
    );
    assert!(
        committed_tool_result_texts(result.checkpoint())
            .iter()
            .any(|text| *text == "x".repeat(25_000)),
        "half the estimated headroom keeps a 25 KB result whole"
    );
    assert_eq!(compaction_workflow_count(result.checkpoint()), 0);
    assert!(model.observed_compaction_contexts().is_empty());
    agent.shutdown().await?;
    Ok(())
}

/// The capped observation budget a durable round derives survives a provider
/// server pause before the round and an effect-decision frame across a
/// reopen: the restored allowance still clips the approved tool's output.
#[tokio::test]
async fn a_capped_budget_survives_a_server_pause_and_a_reopened_decision_frame() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (original_builder, model) = placeholder_fixture_pausing(
        Arc::clone(&workspace),
        0x0b5e_0002,
        1_000_000,
        &[5_000],
        &[1],
        false,
        LocalDurabilityDisposition::DurableRequired,
        3,
        &[0],
    )?;
    let capped = |builder: AgentBuilder| -> TestResult<AgentBuilder> {
        Ok(builder
            .tool_policy(Arc::new(RestoreDecisionPolicy::new()?))
            .config(AgentConfig::default().with_observation_budget_cap_bytes(Some(2_048))))
    };
    let original_builder = capped(original_builder)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let original = sessions
        .create("create:capped-decision-budget", original_builder)
        .await?;
    let turn = original
        .start(
            CommandId::new("root:capped-decision-budget")?,
            "read the file",
        )
        .await?
        .into_turn()
        .ok_or("capped turn was not admitted")?;
    let turn_id = turn.id();
    let address = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "the paused round and the tool round"
    );
    store.expire_active_lease();
    // The reopened owner continues on the same scripted model.
    let registry = oversized_tool_registry(&[5_000])?;
    let configured = configured_test_model(
        model.clone(),
        model.descriptor.clone(),
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let reopen_builder = capped(
        AgentBuilder::new(configured, registry, Arc::clone(&workspace))
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            )
            .with_scheduled_compaction_enabled(true),
    )?;
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open("open:capped-decision-budget", address, reopen_builder)
        .await?
    else {
        return Err("the pending decision did not restore as working".into());
    };
    assert_eq!(turn.turn_id(), turn_id);
    let decision = turn
        .pending_decision()
        .ok_or("restored effect decision was absent")?
        .clone();
    assert_eq!(
        turn.control()
            .allow_effect(
                decision,
                EffectDecisionAuditId::new("test:capped-decision-budget")?,
            )
            .await?,
        EffectDecisionStatus::Allowed
    );
    let result = turn.result().result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let texts = committed_tool_result_texts(result.checkpoint());
    let clipped = texts
        .iter()
        .find(|text| text.contains("omitted"))
        .ok_or_else(|| format!("the restored cap must clip the 5 KB result: {texts:?}"))?;
    assert!(
        clipped.len() <= 2_048 + 256,
        "the restored cap bounds the kept text: {}",
        clipped.len()
    );
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(())
}
