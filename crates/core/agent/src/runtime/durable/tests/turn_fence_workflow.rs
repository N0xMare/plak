use super::*;
use crate::feed::{FeedCompactReason, FeedFact, FeedWorkflowKind};
use crate::runtime::operation_record::is_accepted_root;
use crate::runtime::terminal::encode_terminal_result_record;
use crate::runtime::{AbandonedTurnClaim, AbandonmentCause};

#[tokio::test]
async fn turn_fence_workflow_after_recovery_required_closes_the_turn_not_an_endpoint() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:turn-fence-workflow",
        91_150,
    )
    .await?;
    let operation = "compact:turn-fence-workflow";
    let identity = DurableCompactionIdentity::new(operation, source)?;
    let (builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::AmbiguousFailure(ModelError::new(
            ModelErrorKind::OutcomeUnknown,
            "summary outcome is uncertain",
        )),
        None,
    )?;
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
        .map_err(|_| "compaction endpoint")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { .. }
    ));
    let working = store
        .load_checkpoint(identity.dispatched_checkpoint)
        .await?;
    let working = Arc::clone(working.checkpoint());
    let root_command = working
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .rev()
        .find(|command| is_accepted_root(command))
        .ok_or("root command")?;
    let turn_id = root_command.turn_id().ok_or("root turn")?;
    // Seed the live-written shape: an in-turn fold remains open after the
    // root's RecoveryRequired publication leaves a SessionWorkflow head.
    let mut original_terminal = None;
    let records = working.manifest().ledger().operations().into_iter().map(|record| {
        let mut request = record.request().clone();
        let mut transitions = record.transitions().to_vec();
        if matches!(record.kind(), DurableOperationKind::Compaction { workflow_id } if *workflow_id == identity.workflow_id) {
            let mut paid: crate::runtime::compaction::DurableCompactionRequestRecord = serde_json::from_slice(request.bytes())?;
            paid.turn_fence = true;
            request = runtime_component(RuntimeComponentKind::Operation, serde_json::to_vec(&paid)?)?;
        }
        if matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id) {
            let last = transitions.last_mut().ok_or("root transition")?;
            let DurableOperationState::Finalized(evidence) = last.state() else { return Err("root terminal".into()); };
            let mut terminal = read_terminal_result_record(evidence.bytes()).map_err(|error| error.detail())?;
            terminal.outcome = DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(
                &AgentError::new(AgentErrorKind::OutcomeUnknown, "turn-fence compaction remains uncertain"),
            )?);
            original_terminal = Some(terminal.clone());
            *last = DurableOperationTransition::new(last.checkpoint_id(), DurableOperationState::Finalized(
                runtime_component(RuntimeComponentKind::Operation, encode_terminal_result_record(&terminal)?)?
            ));
        }
        Ok(DurableOperationRecord::new(record.operation_id(), record.kind().clone(), request, transitions)?)
    }).collect::<TestResult<Vec<_>>>()?;
    let cut = checkpoint_with_compaction_records(&working, identity, records)?;
    assert!(matches!(
        cut.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::SessionWorkflow(_),
            ..
        }
    ));
    let _ = agent.shutdown().await;
    let bundle = RuntimeCheckpointCodec::bundle(&cut, None)?;
    let reference = CheckpointObjectRef::from_store(cut.id(), bundle.root(), cut.integrity());
    let stored = StoredCheckpoint::from_store(reference, bundle)?;
    {
        let mut state = lock_mutex(&store.state);
        let session = state.loaded.as_ref().ok_or("branch")?.session();
        let head = BranchHead::from_store(
            source.session(),
            BranchRevision::from_store(cut.manifest().topology().branch_revision),
            reference,
        );
        state.loaded = Some(crate::LoadedBranch::from_store(
            session,
            head,
            stored.clone(),
        ));
        state.stored = Some(stored.clone());
        state.stored_commits.insert(cut.id(), stored);
        state.active_lease = None;
        state.released_lease = None;
    }
    let restart = artifact_test_model(0, 91_200)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:turn-fence-workflow",
            DurableSessionAddress::new(source.session_id, source.branch_id, cut.id()),
            durable_artifact_test_builder(workspace, Arc::clone(&restart)),
        )
        .await
        .map_err(|error| format!("turn-fence open: {}", error.message()))?
    else {
        return Err("turn fence did not close".into());
    };
    let report = reopened.abandoned_turn().ok_or("abandonment report")?;
    assert_eq!(report.claim(), AbandonedTurnClaim::CompactionNotApplied);
    assert_eq!(report.cause(), AbandonmentCause::Interrupted);
    let terminal = terminal_record_at_checkpoint(
        &reopened.checkpoint(),
        &CommandId::new(root_command.command_id())?,
    )?;
    let mut enriched = terminal.clone();
    assert_eq!(enriched.abandonment.take().as_ref(), Some(report));
    assert_eq!(Some(enriched), original_terminal);
    let facts = crate::feed::facts::commit_facts(&cut, &reopened.checkpoint())?;
    assert!(
        !facts
            .iter()
            .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
    );
    assert!(facts.iter().any(|fact| matches!(
        fact,
        FeedFact::WorkflowSettled {
            kind: FeedWorkflowKind::Compact {
                reason: FeedCompactReason::TurnFence
            },
            ..
        }
    )));
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(restart.calls.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    Ok(())
}
