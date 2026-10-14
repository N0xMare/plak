//! Paid workflow cuts after a published root answer never restart the fold.

use super::*;

#[tokio::test]
async fn admitted_compaction_workflow_abandons_every_phase_without_dispatch() -> TestResult {
    for (phase, interrupted) in (0..3).flat_map(|phase| [false, true].map(|cut| (phase, cut))) {
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
            workspace.clone(),
            "create:workflow-abandonment",
            90_500,
        )
        .await?;
        let source_checkpoint = store.load_checkpoint(source.checkpoint_id).await?;
        let operation = "compact:workflow-abandonment";
        let identity = DurableCompactionIdentity::new(operation, source)?;
        let cut_id = [
            identity.prepared_checkpoint,
            identity.dispatched_checkpoint,
            identity.completed_checkpoint,
        ][phase];
        store.halt_after_checkpoint(cut_id);
        let (builder, model) = recovery_compaction_fixture(
            workspace,
            plakous_test_support::ScriptedRecoveryState::new(),
            CompactionFixtureOutcome::Summary,
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
            .map_err(|_| "exact source rejected")?;
        assert!(matches!(
            outcome,
            AgentCompactSessionOutcome::RecoveryRequired { .. }
        ));
        let cut = store.load_checkpoint(cut_id).await?;
        assert!(crate::runtime::checkpoint_can_abandon_compaction(
            cut.checkpoint(),
            operation,
            source
        ));
        assert!(!crate::runtime::checkpoint_can_abandon_compaction(
            cut.checkpoint(),
            "compact:foreign",
            source
        ));
        let operations = decode_paid_compaction_operations(cut.checkpoint(), identity)?;
        agent.shutdown().await?;
        store.resume_commits();
        store.expire_active_lease();
        let starts = model.starts.load(Ordering::Acquire);
        if interrupted {
            // Each closure is one commit; a halt after it only stops later
            // commits, and the repeat below answers from its receipt.
            store.halt_after_checkpoint(if phase == 2 {
                identity.failure_checkpoint
            } else {
                identity.abandonment_evidence_checkpoint
            });
            let first = sessions.abandon_compaction(operation, source).await?;
            assert!(matches!(
                (phase, first),
                (2, DurableCompactionRecoveryOutcome::Failed { .. })
                    | (0 | 1, DurableCompactionRecoveryOutcome::Abandoned { .. })
            ));
            store.resume_commits();
            store.expire_active_lease();
        }
        let abandoned = sessions.abandon_compaction(operation, source).await?;
        let target = match abandoned {
            DurableCompactionRecoveryOutcome::Abandoned { target, .. } if phase < 2 => target,
            DurableCompactionRecoveryOutcome::Failed { target, .. } if phase == 2 => target,
            other => return Err(format!("phase {phase} was not abandoned: {other:?}").into()),
        };
        let closed = store.load_checkpoint(target.checkpoint_id).await?;
        let provider = closed
            .checkpoint()
            .manifest()
            .ledger()
            .operation(operations.provider.0)
            .ok_or("closed compaction provider is missing")?;
        assert_eq!(
            provider.transitions().iter().any(|transition| {
                matches!(transition.state(), DurableOperationState::Dispatched)
            }),
            phase != 0
        );
        assert_eq!(
            closed.checkpoint().manifest().canonical().history(),
            source_checkpoint
                .checkpoint()
                .manifest()
                .canonical()
                .history(),
            "the finalized answer and original history must remain exact"
        );
        assert!(matches!(
            closed.checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ));
        for operation in [operations.compaction, operations.provider] {
            let record = closed
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == operation.0)
                .ok_or("closed operation")?;
            assert!(matches!(
                record.state(),
                DurableOperationState::Finalized(_)
            ));
            if phase == 0 {
                assert!(record.transitions().iter().any(|transition| matches!(
                    transition.state(),
                    DurableOperationState::ProvenNotDispatched(_)
                )));
                assert!(!record.transitions().iter().any(|transition| matches!(
                    transition.state(),
                    DurableOperationState::Dispatched
                )));
            }
        }
        let commits = store.commit_calls.load(Ordering::Acquire);
        assert_eq!(
            sessions.abandon_compaction(operation, source).await?,
            abandoned
        );
        assert_eq!(store.commit_calls.load(Ordering::Acquire), commits);
        assert_eq!(model.starts.load(Ordering::Acquire), starts);
        assert_eq!(model.recover_calls.load(Ordering::Acquire), 0);
    }
    Ok(())
}

#[tokio::test]
async fn compaction_abandonment_refuses_foreign_debt_and_misplaced_not_dispatched() -> TestResult {
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
        workspace.clone(),
        "create:inspection-debt",
        90_600,
    )
    .await?;
    let operation = "compact:inspection-debt";
    let identity = DurableCompactionIdentity::new(operation, source)?;
    store.halt_after_checkpoint(identity.prepared_checkpoint);
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let result = agent
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
        .map_err(|_| "source")?;
    assert!(matches!(
        result,
        AgentCompactSessionOutcome::RecoveryRequired { .. }
    ));
    agent.shutdown().await?;
    store.resume_commits();
    let valid = store.load_checkpoint(identity.prepared_checkpoint).await?;
    let decoded = decode_paid_compaction_operations(valid.checkpoint(), identity)?;
    for foreign in [true, false] {
        let mut records = valid
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let provider = records
            .iter()
            .find(|r| r.operation_id() == decoded.provider.0)
            .ok_or("provider")?
            .clone();
        if foreign {
            records.push(DurableOperationRecord::new(
                records
                    .iter()
                    .map(DurableOperationRecord::operation_id)
                    .max()
                    .ok_or("ledger")?
                    + 1,
                DurableOperationKind::Command { turn_id: None },
                provider.request().clone(),
                provider.transitions().to_vec(),
            )?);
        } else {
            for record in records.iter_mut().filter(|record| {
                record.operation_id() == decoded.provider.0
                    || record.operation_id() == decoded.compaction.0
            }) {
                let mut transitions = record.transitions().to_vec();
                transitions.push(DurableOperationTransition::new(
                    identity.dispatched_checkpoint,
                    DurableOperationState::ProvenNotDispatched(record.request().clone()),
                ));
                *record = DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )?;
            }
        }
        let poisoned = checkpoint_with_compaction_records(valid.checkpoint(), identity, records)?;
        assert!(
            !crate::runtime::checkpoint_can_abandon_compaction(&poisoned, operation, source),
            "foreign={foreign}"
        );
        let bundle = RuntimeCheckpointCodec::bundle(&poisoned, None)?;
        let reference =
            CheckpointObjectRef::from_store(poisoned.id(), bundle.root(), poisoned.integrity());
        let stored = StoredCheckpoint::from_store(reference, bundle)?;
        {
            let mut state = lock_mutex(&store.state);
            let loaded = state.loaded.as_ref().ok_or("branch")?;
            let head = BranchHead::from_store(
                source.session(),
                BranchRevision::from_store(poisoned.manifest().topology().branch_revision),
                reference,
            );
            state.loaded = Some(crate::LoadedBranch::from_store(
                loaded.session(),
                head,
                stored.clone(),
            ));
            state.stored = Some(stored.clone());
            state.stored_commits.insert(poisoned.id(), stored);
            state.active_lease = None;
            state.released_lease = None;
        }
        let commits = store.commit_calls.load(Ordering::Acquire);
        // Foreign debt and a misplaced not-dispatched summary are workflow
        // shapes no closure row admits: typed integrity.
        let outcome = sessions.abandon_compaction(operation, source).await;
        assert_eq!(
            outcome
                .expect_err("no closure row admits this workflow debt")
                .kind(),
            AgentErrorKind::Session,
            "foreign={foreign}"
        );
        assert_eq!(store.commit_calls.load(Ordering::Acquire), commits);
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
    }
    Ok(())
}
