use super::*;
use crate::runtime::compaction::instruction::CompactionInstruction;

async fn fold_once(
    agent: &Agent,
    workspace: Arc<WorkspacePolicy>,
    operation: &str,
    fixture: CompactionFixtureOutcome,
    instruction: CompactionInstruction,
) -> TestResult<AgentCompactSessionOutcome> {
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let identity = DurableCompactionIdentity::new(operation, source)?;
    let (mut builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        fixture,
        None,
    )?;
    if !agent
        .checkpoint()
        .manifest()
        .canonical()
        .stable_prefix()
        .tools()
        .is_empty()
    {
        let registry = oversized_tool_registry(&[])?;
        let configured = configured_test_model(
            model.clone(),
            model.scripted.descriptor().clone(),
            builder.initial_model_authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        builder.provider_configuration = Arc::new(configured.configuration().clone());
        builder.tool_bindings = ToolExecutorBindings::capture(&registry);
        builder.tools = registry;
    }
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("compact rejected: {error:?}"))?;
    assert_eq!(
        model.starts.load(Ordering::Acquire),
        1,
        "one dispatch per fold"
    );
    let checkpoint = agent.checkpoint();
    let ledger = checkpoint.manifest().ledger();
    let operations = decode_paid_compaction_operations(&checkpoint, identity)?;
    let last = operations
        .request
        .request
        .input()
        .history()
        .segments()
        .iter()
        .last()
        .ok_or("instruction missing")?;
    let ContentBlock::Text(text) = &last.items()[0].blocks()[0] else {
        return Err("instruction is not text".into());
    };
    assert_eq!(text.text(), instruction.text());
    let provider = ledger
        .operation(operations.provider.0)
        .ok_or("provider missing")?;
    assert_eq!(
        provider
            .transitions()
            .iter()
            .filter(|transition| {
                matches!(transition.state(), DurableOperationState::Dispatched)
            })
            .count(),
        1
    );
    Ok(outcome)
}

#[tokio::test]
async fn response_free_tight_failure_keeps_length_selection_across_reopen_and_retention()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, _) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:overrun-tight",
        191_000,
    )
    .await?;
    assert!(matches!(
        fold_once(
            &agent,
            Arc::clone(&workspace),
            "compact:overrun-standard",
            CompactionFixtureOutcome::LengthSummary,
            CompactionInstruction::Standard,
        )
        .await?,
        AgentCompactSessionOutcome::Failed { .. }
    ));
    assert!(matches!(
        fold_once(
            &agent,
            Arc::clone(&workspace),
            "compact:tight-rate-limited",
            CompactionFixtureOutcome::ProviderError(ModelError::new(
                ModelErrorKind::RateLimited,
                "fixture dispatched 429",
            )),
            CompactionInstruction::Tight,
        )
        .await?,
        AgentCompactSessionOutcome::Failed { .. }
    ));
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let reopened = sessions
        .open(
            "open:overrun-tight",
            source,
            durable_artifact_test_builder(Arc::clone(&workspace), artifact_test_model(0, 192_000)?),
        )
        .await?;
    let DurableRuntimeOpen::Opened(agent) = reopened else {
        return Err("finalized overrun must reopen without an obligation".into());
    };
    assert!(matches!(
        agent
            .prompt("new work after overrun")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let retained = crate::runtime::operation_record::retain_quiescent_ledger(
        agent.checkpoint().manifest().ledger(),
        &BTreeSet::new(),
    )?;
    assert_eq!(
        CompactionInstruction::for_ledger(&retained).text(),
        CompactionInstruction::Tight.text()
    );
    assert!(matches!(
        fold_once(
            &agent,
            Arc::clone(&workspace),
            "compact:overrun-tight",
            CompactionFixtureOutcome::EmptySummary,
            CompactionInstruction::Tight,
        )
        .await?,
        AgentCompactSessionOutcome::Failed { .. }
    ));
    assert!(matches!(
        agent
            .prompt("work after completed non-Length failure")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert!(matches!(
        fold_once(
            &agent,
            workspace,
            "compact:standard-again",
            CompactionFixtureOutcome::Summary,
            CompactionInstruction::Standard,
        )
        .await?,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn endpoint_overrun_falls_to_placeholders_without_a_second_paid_dispatch() -> TestResult {
    endpoint_failed_fold_falls_to_placeholders(
        CompactionFixtureOutcome::LengthSummary,
        CompactionInstruction::Tight,
    )
    .await
}

#[tokio::test]
async fn endpoint_provider_error_commits_placeholders_and_keeps_paid_failure_flag() -> TestResult {
    endpoint_failed_fold_falls_to_placeholders(
        CompactionFixtureOutcome::ProviderError(ModelError::new(
            ModelErrorKind::RateLimited,
            "fixture dispatched 429",
        )),
        CompactionInstruction::Standard,
    )
    .await
}

async fn endpoint_failed_fold_falls_to_placeholders(
    fixture: CompactionFixtureOutcome,
    next_instruction: CompactionInstruction,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (builder, _) = placeholder_fixture_with_descriptor(
        Arc::clone(&workspace),
        193_000,
        durable_test_descriptor("test.route")?,
        &[8_000],
        &[1],
        LocalDurabilityDisposition::DurableRequired,
        0,
        &[],
    )?;
    let agent = sessions
        .create(
            "create:endpoint-overrun",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    for prompt in ["older replaceable intent", "read current result"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let source = agent.checkpoint();
    assert!(matches!(
        fold_once(
            &agent,
            Arc::clone(&workspace),
            "compact:endpoint-overrun",
            fixture,
            CompactionInstruction::Standard,
        )
        .await?,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    assert!(
        crate::runtime::driver::paid_fold_failed_since_last_round(&agent.checkpoint()),
        "the free commit must not authorize a paid follow-on at the same fence"
    );
    assert_eq!(
        placeholder_harness_notes(agent.checkpoint().manifest().canonical().history()).len(),
        1
    );
    let loaded = store.load_checkpoint(source.id()).await?;
    assert_eq!(
        loaded.checkpoint().manifest().canonical().history(),
        source.manifest().canonical().history()
    );
    assert!(matches!(
        fold_once(
            &agent,
            workspace,
            "compact:endpoint-after-overrun",
            CompactionFixtureOutcome::Summary,
            next_instruction,
        )
        .await?,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    agent.shutdown().await?;
    Ok(())
}
