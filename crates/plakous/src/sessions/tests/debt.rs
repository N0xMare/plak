use super::*;
use plakous_model::ModelOperationRoot;

#[tokio::test]
async fn facade_open_closes_finalization_debt_after_root_already_finalized() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:finalized-debt")?,
            completing_runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let result = session
        .agent()
        .prompt("finish root")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let turn_id = result.turn_id();
    let source = session.release().await?;
    let loaded = store.load_branch(source.session_ref()).await?;
    let prior = loaded.checkpoint().checkpoint().manifest();
    let lease = store
        .acquire(AcquireBranch::new(
            source.session_ref(),
            RunId::try_from(Uuid::from_u128(0xf230))?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let checkpoint_id = CheckpointId::try_from(Uuid::from_u128(0xf231))?;
    let provider = prior
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    let DurableOperationState::Finalized(evidence) = provider.state() else {
        return Err("provider terminal missing".into());
    };
    let operation_id = prior
        .ledger()
        .operations()
        .into_iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .ok_or("operations")?
        + 1;
    let ledger = prior
        .ledger()
        .append_operation(DurableOperationRecord::new(
            operation_id,
            DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(turn_id),
                attempt_id: AttemptId::try_from(Uuid::from_u128(0xf235))?,
            },
            ReconstructiveComponent::new(
                RuntimeComponentKind::Operation,
                1,
                b"completed provider fixture".as_slice(),
            )?,
            vec![
                DurableOperationTransition::new(
                    source.checkpoint_id(),
                    DurableOperationState::Prepared,
                ),
                DurableOperationTransition::new(
                    CheckpointId::try_from(Uuid::from_u128(0xf234))?,
                    DurableOperationState::Dispatched,
                ),
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Completed(evidence.clone()),
                ),
            ],
        )?)?;
    let obligations = vec![RecoveryObligation::new(
        operation_id,
        RecoveryHandlerSignature::new(
            RecoveryObligationKind::ProviderAttempt,
            RecoveryAction::FinalizationRequired,
        ),
    )];
    let topology = prior.topology();
    let working = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision: topology.branch_revision + 1,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"finalized-debt"],
            )?,
            ..topology
        },
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id,
                next_round: 1,
            },
            recovery_phase: WorkingRecoveryPhase::Finalization,
            obligations,
        },
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::advance(&working, loaded.checkpoint().bundle())?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    let source = SessionLocator::new(source.session_id(), source.branch_id(), checkpoint_id);
    #[cfg(feature = "sessions-sqlite")]
    {
        let before = store.stats()?;
        let report = rehearsal::inspect_exact(&sessions.load_exact(source).await?);
        assert_eq!(
            report.outcome,
            rehearsal::OfflineReopenOutcome::RecoversSavedTerminal
        );
        assert_eq!(store.stats()?, before);
    }
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:finalized-debt")?,
            source,
            completing_runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?,
        ))
        .await?
    else {
        return Err("facade refused exact local debt".into());
    };
    let head = agent_checkpoint(session.agent());
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert!(session.agent().abandoned_turn().is_none());
    assert_eq!(
        head.manifest().canonical().history().identity(),
        prior.canonical().history().identity()
    );
    for old in prior
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Command { .. }))
    {
        assert!(head.manifest().ledger().operations().contains(&old));
    }
    let closed = session.release().await?;
    assert_ne!(closed.checkpoint_id(), checkpoint_id);
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:closed-finalized-debt")?,
            closed,
            completing_runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?
    else {
        return Err("facade refused the closed head".into());
    };
    assert_eq!(session.locator(), closed);
    assert!(session.agent().abandoned_turn().is_none());
    assert_eq!(
        agent_checkpoint(session.agent())
            .manifest()
            .topology()
            .checkpoint_id,
        closed.checkpoint_id()
    );
    assert_eq!(session.release().await?, closed);
    Ok(())
}
