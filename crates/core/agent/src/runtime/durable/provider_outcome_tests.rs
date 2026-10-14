//! Every round's provider attempt ends in a `provider-outcome-v2` record
//! carrying the decision the driver took at stream end (spec 01 §5.5).

use super::*;
use crate::runtime::operation_record::{
    ProviderOutcome, RoundDecision, TurnBudgets, provider_outcome,
};
use plakous_model::{ResponseStarted, UsageUpdate};
use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};

/// The outcome records of `turn_id`'s round providers, oldest first.
fn round_outcomes(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
) -> TestResult<Vec<ProviderOutcome>> {
    let mut records = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(turn),
                    ..
                } if *turn == turn_id
            )
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|record| record.operation_id());
    records
        .into_iter()
        .map(|record| -> TestResult<ProviderOutcome> {
            Ok(provider_outcome(record)?.ok_or("round provider without an outcome record")?)
        })
        .collect()
}

fn failed_attempt(usage: Usage, error: ModelError) -> TestResult<ModelScenario> {
    Ok(ModelScenario::new(
        ExpectedRequest::Any,
        ScenarioStart::Immediate,
        vec![
            ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                None,
                Digest::sha256(DigestDomain::RequestPlan, 1, &[b"rebound"])?,
            ))),
            ScenarioStep::Emit(ModelEvent::Usage(UsageUpdate::new(usage))),
            ScenarioStep::Error(error),
        ],
    ))
}

fn text_answer(entry: u128, usage: Usage) -> TestResult<ModelScenario> {
    let response = ModelResponse::new(
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(entry))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("answer", None)?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        Some(usage),
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"rebound"])?,
    );
    Ok(ModelScenario::new(
        ExpectedRequest::Any,
        ScenarioStart::Immediate,
        vec![ScenarioStep::Emit(ModelEvent::ResponseCompleted(response))],
    ))
}

fn content_filtered() -> ModelError {
    ModelError::new(
        ModelErrorKind::ContentFilteredIncomplete,
        "stream disconnected before completion: Incomplete response returned, reason: content_filter",
    )
}

fn failure_kind(decision: &RoundDecision) -> TestResult<Option<ModelErrorKind>> {
    let RoundDecision::FailTurn(shape) = decision else {
        return Err(format!("expected FailTurn, found {decision:?}").into());
    };
    assert!(!shape.prompt_withheld);
    assert_eq!(shape.remedy, None);
    Ok(shape.failure.clone().into_error()?.model_kind())
}

/// Runs one prompt on a fresh durable session over `scenarios` and returns
/// its result with the outcome records of its round providers.
async fn scripted_turn(
    create: &str,
    scenarios: Vec<ModelScenario>,
) -> TestResult<(TurnResult, Vec<ProviderOutcome>)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), scenarios)
            .with_runtime_authority(authority.clone())
            .with_context_observation(CountObservation::new(
                "agent-scripted-counter",
                "agent-scripted-exact-v1",
                64,
            )?)
            .with_content_filter_instruction("Offer a permitted alternative.".to_owned()),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = configured_test_model(
        model.clone(),
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions.create(create, builder).await?;
    let turn = agent.prompt("scripted").await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    let outcomes = round_outcomes(result.checkpoint(), turn_id)?;
    assert_eq!(model.remaining_scenarios(), 0);
    agent.shutdown().await?;
    Ok((result, outcomes))
}

#[tokio::test]
async fn retries_are_decided_and_recorded_with_budgets_and_partial_usage() -> TestResult {
    let (result, outcomes) = scripted_turn(
        "create:provider-outcome-retries",
        vec![
            failed_attempt(Usage::new(5, 1, 0, 0, 0), content_filtered())?,
            failed_attempt(
                Usage::new(7, 2, 0, 0, 0),
                ModelError::new(ModelErrorKind::OutputLimit, "truncated mid call"),
            )?,
            text_answer(0xA2_0001, Usage::new(11, 3, 0, 0, 0))?,
        ],
    )
    .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.decision.clone())
            .collect::<Vec<_>>(),
        [
            RoundDecision::RetryContentFilter,
            RoundDecision::RetryOutputLimit,
            RoundDecision::Terminal,
        ]
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.usage)
            .collect::<Vec<_>>(),
        [
            Some(Usage::new(5, 1, 0, 0, 0)),
            Some(Usage::new(7, 2, 0, 0, 0)),
            Some(Usage::new(11, 3, 0, 0, 0)),
        ]
    );
    // The filtered attempt does not count as a round; the answer resets the
    // output-limit guard and keeps the spent filter retry.
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.budgets)
            .collect::<Vec<_>>(),
        [
            TurnBudgets {
                content_filter_retry_spent: true,
                model_rounds: 0,
                ..TurnBudgets::default()
            },
            TurnBudgets {
                content_filter_retry_spent: true,
                output_limit_retry_taken: true,
                model_rounds: 1,
                ..TurnBudgets::default()
            },
            TurnBudgets {
                content_filter_retry_spent: true,
                model_rounds: 2,
                ..TurnBudgets::default()
            },
        ]
    );
    assert!(outcomes[0].failure().is_some() && outcomes[1].failure().is_some());
    assert!(outcomes[2].response().is_some());
    assert!(outcomes.iter().all(|outcome| outcome.steering.is_empty()));
    Ok(())
}

#[tokio::test]
async fn a_failure_that_ends_the_turn_records_its_shape_and_partial_usage() -> TestResult {
    let (result, outcomes) = scripted_turn(
        "create:provider-outcome-failure",
        vec![failed_attempt(
            Usage::new(9, 3, 0, 0, 0),
            ModelError::new(ModelErrorKind::Protocol, "stream broke"),
        )?],
    )
    .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        failure_kind(&outcomes[0].decision)?,
        Some(ModelErrorKind::Protocol)
    );
    assert_eq!(outcomes[0].usage, Some(Usage::new(9, 3, 0, 0, 0)));
    assert_eq!(
        outcomes[0].budgets,
        TurnBudgets {
            model_rounds: 1,
            ..TurnBudgets::default()
        }
    );
    Ok(())
}

#[tokio::test]
async fn the_budget_alone_spends_the_content_filter_retry() -> TestResult {
    let (result, outcomes) = scripted_turn(
        "create:provider-outcome-filtered-twice",
        vec![
            failed_attempt(Usage::new(1, 0, 0, 0, 0), content_filtered())?,
            failed_attempt(Usage::new(2, 0, 0, 0, 0), content_filtered())?,
        ],
    )
    .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].decision, RoundDecision::RetryContentFilter);
    assert_eq!(
        failure_kind(&outcomes[1].decision)?,
        Some(ModelErrorKind::Policy)
    );
    assert_eq!(
        outcomes[1]
            .failure()
            .ok_or("the attempt's own failure")?
            .clone()
            .into_error()?
            .model_kind(),
        Some(ModelErrorKind::ContentFilteredIncomplete)
    );
    Ok(())
}

#[tokio::test]
async fn completed_rounds_record_tool_batch_continue_and_terminal_at_stream_end() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0xA2_1000,
            false,
        )
        .with_tool_call_on(0)
        .with_assistant_continuations([1])
        .with_report_usage(),
    );
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:provider-outcome-rounds",
            session_update_builder(workspace, Arc::clone(&model), one_tool_registry()?)?,
        )
        .await?;
    let turn = agent.prompt("probe, continue, answer").await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let outcomes = round_outcomes(result.checkpoint(), turn_id)?;
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.decision.clone())
            .collect::<Vec<_>>(),
        [
            RoundDecision::ToolBatch,
            RoundDecision::Continue,
            RoundDecision::Terminal,
        ]
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.budgets.model_rounds)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    for outcome in &outcomes {
        let response = outcome.response().ok_or("completed round response")?;
        assert_eq!(outcome.usage, response.usage());
        assert!(outcome.usage.is_some());
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_restored_frame_resumes_with_the_budgets_its_decision_recorded() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            0xA2_2000,
            false,
        )
        .with_provider_pauses([0]),
    );
    let store = Arc::new(TestCreateStore::default());
    store.block_after_promoted_hosted();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:provider-outcome-frame",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let paused_turn = original.prompt("pause, then answer").await?;
    let turn_id = paused_turn.id();
    let parked = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire)
            && matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                    ..
                }
            )
        {
            break checkpoint;
        }
        tokio::task::yield_now().await;
    };
    let frame_budgets = decode_hosted_continuation(&parked)?.budgets();
    let paused = round_outcomes(&parked, turn_id)?;
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0].decision, RoundDecision::Continue);
    assert_eq!(paused[0].budgets, frame_budgets);
    assert_eq!(frame_budgets.model_rounds, 1);

    let address =
        DurableSessionAddress::new(original.session_id(), original.branch_id(), parked.id());
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = sessions
        .open(
            "open:provider-outcome-frame",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?
    else {
        return Err("server pause did not restore as working".into());
    };
    let result = turn.result().result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let outcomes = round_outcomes(result.checkpoint(), turn_id)?;
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| (outcome.decision.clone(), outcome.budgets.model_rounds))
            .collect::<Vec<_>>(),
        [(RoundDecision::Continue, 1), (RoundDecision::Terminal, 2)],
        "the restored driver counts on from the frame's budgets"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn an_overflow_refusal_records_the_fold_decision_before_the_fold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (agent, _model) = provider_overflow_agent(workspace, 0xA2_3000, &[2])?;
    for prompt in ["older replaceable intent", "second older intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let turn = agent
        .prompt("answer after the provider refuses the window")
        .await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let outcomes = round_outcomes(result.checkpoint(), turn_id)?;
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.decision.clone())
            .collect::<Vec<_>>(),
        [RoundDecision::OverflowFold, RoundDecision::Terminal]
    );
    assert_eq!(
        outcomes[0]
            .failure()
            .ok_or("the refused attempt's failure")?
            .clone()
            .into_error()?
            .model_kind(),
        Some(ModelErrorKind::ContextLimit)
    );
    // The resend reuses the refused round's count and keeps the spent fold.
    for outcome in &outcomes {
        assert_eq!(
            outcome.budgets,
            TurnBudgets {
                overflow_recovery_taken: true,
                model_rounds: 1,
                ..TurnBudgets::default()
            }
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[test]
fn terminal_outcomes_reject_pending_steering() -> TestResult {
    let outcome = ProviderOutcome {
        result: crate::runtime::operation_record::ProviderAttemptResult::Response(
            ModelResponse::new(
                Vec::new(),
                FinishDetails::new(FinishReason::Stop, None),
                None,
                None,
                Digest::sha256(DigestDomain::RequestPlan, 1, &[b"terminal steering"])?,
            ),
        ),
        usage: None,
        decision: RoundDecision::Terminal,
        budgets: TurnBudgets::default(),
        steering: vec![OperationId(1)],
    };
    let bytes = serde_json::to_vec(&("provider-outcome-v2", &outcome))?;
    let evidence = runtime_component(RuntimeComponentKind::Operation, bytes)?;
    assert!(ProviderOutcome::decode(&evidence).is_err());
    let continuing = ProviderOutcome {
        decision: RoundDecision::Continue,
        ..outcome
    };
    assert!(ProviderOutcome::decode(&continuing.component()?)?.is_some());
    Ok(())
}
