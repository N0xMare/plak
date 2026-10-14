use super::*;
use crate::feed::{
    FeedCommandKind, FeedFact, FeedOperationKind, FeedOperationState, FeedTurnOutcome,
};

#[derive(Default)]
struct UnknownSteeredTool {
    started: AtomicBool,
    release: AtomicBool,
}

impl plakous_tools::ToolExecutor for UnknownSteeredTool {
    fn execute(
        self: Arc<Self>,
        _: ToolInvocation,
        _: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            wait_for_scheduler_fact(&self.release).await;
            ToolOutcome::outcome_unknown_text("the mutation's outcome is unknown")
        })
    }
}

#[tokio::test]
async fn live_recovery_required_settles_queued_steer_in_exactly_one_terminal_fact() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = Arc::new(
        ArtifactModel::new(
            durable_test_descriptor("test.route")?,
            artifact_authority(&durable_test_descriptor("test.route")?, 1)?,
            0,
            46_500,
            false,
        )
        .with_tool_first(),
    );
    let tool = Arc::new(UnknownSteeredTool::default());
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"] )?,
        ToolDefinition::new(
            ToolName::new("probe")?, "1.0.0", "Unknown mutation probe.",
            ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{}, "required":[], "additionalProperties":false}))?,
            ToolOutputContract::model_content(128, 1_048_576)?,
            ToolConcurrency::Serial, ToolEffect::Mutating,
        )?, tool.clone(),
    ))?;
    let registry = registry.freeze();
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:rr-steer",
            session_update_builder(workspace.clone(), model.clone(), registry.clone())?,
        )
        .await?;
    let turn = agent.prompt("mutate").await?;
    wait_for_scheduler_fact(&tool.started).await;
    turn.control().steer("queued instruction").await?;
    tool.release.store(true, Ordering::Release);
    let result = turn.result().await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let settlements = || {
        lock_mutex(&store.feed_facts)
            .iter()
            .flatten()
            .filter_map(|fact| match fact {
                FeedFact::TurnTerminal {
                    turn_id,
                    steering_settled_unapplied,
                    ..
                } if *turn_id == result.turn_id() => Some(*steering_settled_unapplied),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(settlements(), vec![true]);
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let _ = agent.shutdown().await;
    store.expire_active_lease();
    let calls = model.calls.load(Ordering::Acquire);
    let DurableRuntimeOpen::Opened(reopened) = runtime
        .open(
            "open:rr-steer",
            address,
            session_update_builder(workspace, model.clone(), registry)?,
        )
        .await?
    else {
        return Err("RR steer head did not reopen".into());
    };
    assert_eq!(settlements(), vec![true]);
    assert_eq!(model.calls.load(Ordering::Acquire), calls);
    assert!(
        crate::runtime::feed_views::checkpoint_turn_terminal(
            &reopened.checkpoint(),
            result.turn_id(),
        )?
        .is_some()
    );
    reopened.prompt("later turn").await?.result().await?;
    assert!(
        crate::runtime::feed_views::checkpoint_turn_terminal(
            &reopened.checkpoint(),
            result.turn_id(),
        )?
        .is_none()
    );
    reopened.shutdown().await?;
    Ok(())
}

/// A text turn's commits state its admission, history, provider attempt and
/// terminal, each once, and the terminal commit carries the terminal.
#[tokio::test]
async fn a_completed_turn_states_its_admission_attempt_history_and_terminal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 46_000)?;
    let agent = runtime
        .create(
            "create:feed-facts",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    let result = agent.prompt("state the facts").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let _ = agent.shutdown().await;
    let records = lock_mutex(&store.feed_facts).clone();
    assert_eq!(
        records.first().map(Vec::as_slice),
        Some(
            &[FeedFact::SessionCreated {
                operation_id: "create:feed-facts".to_owned(),
                seeded: false,
            }][..]
        )
    );
    let facts = records.iter().flatten().collect::<Vec<_>>();
    let accepted = facts
        .iter()
        .filter(|fact| {
            matches!(
                fact,
                FeedFact::CommandAccepted {
                    kind: FeedCommandKind::Root {
                        origin: plakous_model::TurnOrigin::UserPrompt
                    },
                    turn_id: Some(turn),
                    ..
                } if *turn == result.turn_id()
            )
        })
        .count();
    assert_eq!(accepted, 1, "{facts:?}");
    let provider_states = facts
        .iter()
        .filter_map(|fact| match fact {
            FeedFact::OperationAdvanced {
                kind: FeedOperationKind::ProviderAttempt { .. },
                turn_id: Some(turn),
                state,
                ..
            } if *turn == result.turn_id() => Some(*state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(provider_states.contains(&FeedOperationState::Dispatched));
    assert!(provider_states.contains(&FeedOperationState::Completed));
    assert!(provider_states.contains(&FeedOperationState::Finalized));
    assert!(
        facts
            .iter()
            .any(|fact| matches!(fact, FeedFact::HistoryAppended { turn_id: Some(turn), .. } if *turn == result.turn_id())),
        "{facts:?}"
    );
    let terminals = records
        .iter()
        .enumerate()
        .flat_map(|(index, facts)| facts.iter().map(move |fact| (index, fact)))
        .filter_map(|(index, fact)| match fact {
            FeedFact::TurnTerminal {
                turn_id,
                outcome,
                closed_exchange,
                steering_settled_unapplied,
                usage,
                ..
            } => Some((
                index,
                *turn_id,
                *outcome,
                *closed_exchange,
                *steering_settled_unapplied,
                *usage,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "one terminal per root turn: {terminals:?}"
    );
    let (index, turn_id, outcome, closed_exchange, steering, usage) = terminals[0];
    assert_eq!(turn_id, result.turn_id());
    assert!(
        matches!(
            outcome,
            FeedTurnOutcome::Completed {
                finish: FinishReason::Stop
            }
        ),
        "{outcome:?}"
    );
    assert!(!closed_exchange && !steering);
    assert_eq!(usage, result.usage());
    // The terminal commit also finalizes the attempt.
    assert!(records[index].iter().any(|fact| matches!(
        fact,
        FeedFact::OperationAdvanced {
            kind: FeedOperationKind::ProviderAttempt { .. },
            state: FeedOperationState::Finalized,
            ..
        }
    )));
    Ok(())
}

/// A real Code Cell nested call states its local lineage: the owning
/// evaluation, the outer marker call, the cell, its acceptance sequence and
/// depth, matching the ledger's own child edge.
#[tokio::test]
async fn a_code_cell_nested_call_states_its_local_lineage() -> TestResult {
    let (_, store) =
        run_lineage_fixture(LineagePtcClass::Native, LineageScenario::Read, false, None).await?;
    let head = lock_mutex(&store.state)
        .stored
        .clone()
        .ok_or("the lineage session has no durable head")?;
    let ledger = head.checkpoint().manifest().ledger().clone();
    let records = lock_mutex(&store.feed_facts).clone();
    let facts = records.iter().flatten().collect::<Vec<_>>();
    let cells = facts
        .iter()
        .filter_map(|fact| match fact {
            FeedFact::OperationAdvanced {
                operation_id,
                kind: FeedOperationKind::CodeCell { call_id, .. },
                ..
            } => Some((*operation_id, *call_id)),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(cells.len(), 1, "{facts:?}");
    let (evaluation, marker) = cells.into_iter().next().ok_or("one Code Cell")?;
    let mut nested = 0;
    for fact in &facts {
        let FeedFact::OperationAdvanced {
            kind: FeedOperationKind::Effect {
                call_id, lineage, ..
            },
            ..
        } = fact
        else {
            continue;
        };
        let crate::feed::FeedCallerLineage::LocalProgrammatic {
            evaluation_operation_id,
            parent_call_id,
            cell_id,
            request_sequence,
            depth,
        } = lineage
        else {
            return Err(format!("a nested call was reported as {lineage:?}").into());
        };
        assert_eq!(*evaluation_operation_id, evaluation);
        assert_eq!(*parent_call_id, Some(marker));
        assert_eq!(*depth, 1);
        assert!(
            ledger.operations().into_iter().any(|record| matches!(
                record.kind(),
                DurableOperationKind::CompositionChild {
                    call_id: edge_call,
                    evaluation_operation_id: edge_evaluation,
                    cell_id: bound_cell,
                    request_sequence: edge_sequence,
                    ..
                } if edge_call == call_id
                    && *edge_evaluation == evaluation
                    && bound_cell == cell_id
                    && edge_sequence == request_sequence
            )),
            "nested lineage has no exact ledger edge: {lineage:?}"
        );
        nested += 1;
    }
    assert!(nested > 0, "{facts:?}");
    Ok(())
}

fn compaction_settlements(
    store: &TestCreateStore,
    operation_id: &str,
) -> Vec<crate::feed::FeedWorkflowOutcome> {
    lock_mutex(&store.feed_facts)
        .iter()
        .flatten()
        .filter_map(|fact| match fact {
            FeedFact::WorkflowSettled {
                operation_id: Some(settled),
                kind: crate::feed::FeedWorkflowKind::Compact { .. },
                outcome,
                ..
            } if settled == operation_id => Some(*outcome),
            _ => None,
        })
        .collect()
}

async fn endpoint_compaction_settlement(
    label: &str,
    entry_base: u128,
    outcome: CompactionFixtureOutcome,
) -> TestResult<crate::feed::FeedWorkflowOutcome> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        &format!("create:{label}"),
        entry_base,
    )
    .await?;
    let operation_id = format!("compact:{label}");
    let (builder, _model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        outcome,
        None,
    )?;
    agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.clone(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected its exact source")?;
    agent.shutdown().await?;
    let settled = compaction_settlements(&store, &operation_id);
    match settled.as_slice() {
        [outcome] => Ok(*outcome),
        _ => Err(format!("one settlement per compaction: {settled:?}").into()),
    }
}

/// A committed summary fold states its pass, selected segments and both
/// measurements.
#[tokio::test]
async fn a_committed_compaction_states_its_pass_segments_and_tokens() -> TestResult {
    let outcome =
        endpoint_compaction_settlement("feed-compacted", 92_100, CompactionFixtureOutcome::Summary)
            .await?;
    let crate::feed::FeedWorkflowOutcome::Compacted {
        source,
        target,
        pass,
        selected_segments,
        tokens_before,
        tokens_after,
    } = outcome
    else {
        return Err(format!("a committed fold settled {outcome:?}").into());
    };
    assert_eq!(pass, crate::feed::FeedCompactPass::Summary);
    assert_ne!(source.checkpoint_id, target.checkpoint_id);
    assert!(selected_segments > 0);
    assert!(tokens_before > 0 && tokens_after > 0);
    Ok(())
}

/// A failure proven before dispatch is a live failure whose summary was
/// never sent, not an abandonment.
#[tokio::test]
async fn a_compaction_refused_before_dispatch_fails_unsent() -> TestResult {
    let rejection = ModelError::new(
        ModelErrorKind::ContextLimit,
        "local admission refused the compaction request",
    )
    .with_dispatch_rejection(plakous_model::ModelDispatchRejection::new(
        plakous_model::ModelDispatchRejectionClass::Admission,
        plakous_model::ModelDispatchRejectionPhase::Revalidation,
    ))?;
    let outcome = endpoint_compaction_settlement(
        "feed-unsent",
        92_200,
        CompactionFixtureOutcome::DispatchRejected(rejection),
    )
    .await?;
    assert!(
        matches!(
            outcome,
            crate::feed::FeedWorkflowOutcome::Failed {
                summary_sent: false,
                model_kind: Some(ModelErrorKind::ContextLimit),
                ..
            }
        ),
        "{outcome:?}"
    );
    Ok(())
}

/// A definite failure after dispatch is a live failure whose summary was
/// sent.
#[tokio::test]
async fn a_compaction_failing_after_dispatch_fails_sent() -> TestResult {
    let outcome = endpoint_compaction_settlement(
        "feed-sent",
        92_300,
        CompactionFixtureOutcome::DefiniteFailure(ModelError::new(
            ModelErrorKind::Protocol,
            "summary failed definitively",
        )),
    )
    .await?;
    assert!(
        matches!(
            outcome,
            crate::feed::FeedWorkflowOutcome::Failed {
                summary_sent: true,
                ..
            }
        ),
        "{outcome:?}"
    );
    Ok(())
}

/// A compaction lost before its summary was sent and then abandoned settles
/// in the closure commit as `Abandoned`, with its summary proven unsent.
#[tokio::test]
async fn an_abandoned_compaction_closure_states_its_summary_unsent() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:feed-abandoned",
        92_400,
    )
    .await?;
    let operation_id = "compact:feed-abandoned";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let (builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    store.halt_after_checkpoint(identity.prepared_checkpoint);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "compaction endpoint rejected its exact source")?;
    assert!(
        matches!(
            outcome,
            AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
                if checkpoint == identity.prepared_checkpoint
        ),
        "{outcome:?}"
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    let _ = agent.shutdown().await;
    store.resume_commits();
    store.expire_active_lease();
    assert!(compaction_settlements(&store, operation_id).is_empty());
    assert!(matches!(
        sessions.abandon_compaction(operation_id, source).await?,
        DurableCompactionRecoveryOutcome::Abandoned { .. }
    ));
    assert_eq!(
        compaction_settlements(&store, operation_id),
        [crate::feed::FeedWorkflowOutcome::Abandoned {
            summary_unsent: true
        }]
    );
    Ok(())
}
