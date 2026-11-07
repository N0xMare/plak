use super::*;

#[tokio::test]
async fn saturated_outcome_unknown_recycle_admits_a_new_root() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 0xF2_0000)?;
    model.unknown_outcome_from_call.store(0, Ordering::Release);
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let make = || session_update_builder(workspace.clone(), model.clone(), registry.clone());
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:saturated-retask", make()?).await?;
    seed_expiring_command_receipts(&agent).await?;
    let original_command = CommandId::new("root:original")?;
    let result = agent
        .start(original_command.clone(), "unknown request")
        .await?
        .into_turn()
        .ok_or("original turn")?
        .result()
        .await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let source = agent.checkpoint();
    let _ = agent.shutdown().await;
    let cut = saturated_turn(&source)?;
    let address = install_cut(&store, &cut)?;
    model
        .unknown_outcome_from_call
        .store(usize::MAX, Ordering::Release);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:saturated-retask", address, make()?)
        .await
        .map_err(|error| format!("recycle open: {}", error.message()))?
    else {
        return Err("recycle did not reopen".into());
    };
    assert!(reopened.abandoned_turn().is_some());
    assert_eq!(
        reopened.checkpoint().manifest().lifecycle(),
        &RuntimeLifecycle::Quiescent
    );
    assert_eq!(
        reopened.checkpoint().manifest().ledger().operations().len(),
        4_096
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "no recovery dispatch"
    );
    let original_terminal = terminal_evidence_bytes(&reopened.checkpoint(), &original_command)?;
    let old_full = reopened.checkpoint();
    assert_refused_admission_is_atomic(&reopened, &store).await?;
    let next = reopened
        .start(CommandId::new("root:retask")?, "new task")
        .await
        .map_err(|error| format!("retask admission: {}", error.message()))?;
    assert!(
        next.acknowledgement().is_accepted(),
        "{:?}",
        next.acknowledgement()
    );
    let result = next.into_turn().ok_or("new turn")?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_expired_command_identity_is_reusable(&reopened).await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 3);
    let reopened = assert_admission_crash_cuts(reopened, old_full, &sessions, &store, make).await?;
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        3,
        "cuts never dispatch"
    );
    assert_eq!(
        terminal_evidence_bytes(&reopened.checkpoint(), &original_command)?,
        original_terminal,
        "retention preserves the original terminal evidence"
    );
    reopened.shutdown().await?;
    Ok(())
}

async fn seed_expiring_command_receipts(agent: &Agent) -> TestResult {
    for sequence in 0..300 {
        let refused = agent
            .command(TurnCommand::Start {
                command_id: CommandId::new(format!("expired:{sequence}"))?,
                input: "".into(),
            })
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(
            refused
                .acknowledgement()
                .rejection()
                .ok_or("empty input refusal")?
                .kind(),
            ControlErrorKind::InvalidInput
        );
    }
    Ok(())
}

async fn assert_expired_command_identity_is_reusable(agent: &Agent) -> TestResult {
    let result = agent
        .start(
            CommandId::new("expired:0")?,
            "reuse pruned command identity",
        )
        .await
        .map_err(|error| format!("actor retained an expired command: {}", error.message()))?
        .into_turn()
        .ok_or("fresh command identity")?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    Ok(())
}

fn terminal_evidence_bytes(
    checkpoint: &RuntimeCheckpoint,
    command: &CommandId,
) -> TestResult<Vec<u8>> {
    let terminal = terminal_record_at_checkpoint(checkpoint, command)?;
    let record = checkpoint
        .manifest()
        .ledger()
        .operation(terminal.operation_id.0)
        .ok_or("terminal operation")?;
    let DurableOperationState::Finalized(evidence) = record.state() else {
        return Err("terminal evidence is not finalized".into());
    };
    Ok(evidence.bytes().to_vec())
}

fn saturated_turn(source: &RuntimeCheckpoint) -> TestResult<RuntimeCheckpoint> {
    use super::super::turn_recovery::review_tests::checkpoint_with_review_state;

    let turn_id = source
        .manifest()
        .lifecycle()
        .operation_root()
        .and_then(|root| match root {
            ModelOperationRoot::Turn(turn_id) => Some(turn_id),
            ModelOperationRoot::SessionWorkflow(_) => None,
        })
        .ok_or("working turn")?;
    let mut records = source
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let evidence = runtime_component(RuntimeComponentKind::Operation, b"settled effect".to_vec())?;
    // Earlier tool rounds can fill a turn's ledger before its last request
    // fails. Seed their already-finalized effects rather than dispatching
    // thousands of tools in this regression.
    for id in (records.last().ok_or("last operation")?.operation_id() + 1)..=4_096 {
        let checkpoint =
            |offset| CheckpointId::try_from(uuid::Uuid::from_u128(u128::from(id) * 8 + offset));
        records.push(DurableOperationRecord::new(
            id,
            DurableOperationKind::Effect {
                turn_id,
                attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xF2_1000))?,
                call_id: CallId::try_from(uuid::Uuid::from_u128(u128::from(id)))?,
                source_index: 0,
            },
            evidence.clone(),
            vec![
                DurableOperationTransition::new(checkpoint(0)?, DurableOperationState::Prepared),
                DurableOperationTransition::new(checkpoint(1)?, DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    checkpoint(2)?,
                    DurableOperationState::Completed(evidence.clone()),
                ),
                DurableOperationTransition::new(
                    checkpoint(3)?,
                    DurableOperationState::Finalized(evidence.clone()),
                ),
            ],
        )?);
    }
    checkpoint_with_review_state(
        source,
        records,
        source.manifest().canonical().history().clone(),
        false,
    )
}

fn install_cut(
    store: &TestCreateStore,
    cut: &RuntimeCheckpoint,
) -> TestResult<DurableSessionAddress> {
    let bundle = RuntimeCheckpointCodec::bundle(cut, None)?;
    let reference = CheckpointObjectRef::from_store(cut.id(), bundle.root(), cut.integrity());
    let stored = StoredCheckpoint::from_store(reference, bundle)?;
    let address = DurableSessionAddress {
        session_id: cut.manifest().topology().session_id,
        branch_id: cut.manifest().topology().branch_id,
        checkpoint_id: cut.id(),
    };
    {
        let mut state = lock_mutex(&store.state);
        let session = state.loaded.as_ref().ok_or("branch")?.session();
        let head = BranchHead::from_store(
            address.session(),
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
    Ok(address)
}

async fn assert_refused_admission_is_atomic(
    reopened: &Agent,
    store: &TestCreateStore,
) -> TestResult {
    let recovered = reopened.checkpoint();
    store.refuse_ordinary_commit_on.store(
        store.ordinary_commits.load(Ordering::Acquire) + 1,
        Ordering::Release,
    );
    let refused = reopened
        .command(TurnCommand::Start {
            command_id: CommandId::new("root:refused-retask")?,
            input: "new task".into(),
        })
        .await
        .map_err(|error| format!("refused admission: {error}"))?;
    let rejection = refused.acknowledgement().rejection().ok_or("refusal")?;
    assert_eq!(
        rejection.kind(),
        ControlErrorKind::StoragePressure,
        "{rejection}"
    );
    assert_eq!(reopened.checkpoint().id(), recovered.id());
    assert_eq!(
        reopened.checkpoint().manifest().ledger(),
        recovered.manifest().ledger()
    );
    Ok(())
}

async fn assert_admission_crash_cuts(
    reopened: Agent,
    old: Arc<RuntimeCheckpoint>,
    sessions: &DurableSessionRuntime,
    store: &TestCreateStore,
    make: impl Fn() -> TestResult<AgentBuilder>,
) -> TestResult<Agent> {
    // A cut before the admission CAS sees exactly the old full ledger and no
    // new root. Reopen that cut, not an in-memory pruned intermediate.
    reopened.abort_driver_for_test();
    tokio::task::yield_now().await;
    let address = install_cut(store, &old)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:before-admission-cut", address, make()?)
        .await?
    else {
        return Err("before-admission cut did not reopen".into());
    };
    assert_eq!(reopened.checkpoint().id(), old.id());
    assert_eq!(
        reopened.checkpoint().manifest().ledger().operations().len(),
        4096
    );

    // Accept only the next commit, then halt the store as if the process
    // died immediately after admission and before root dispatch.
    let accepted = lock_mutex(&store.accepted_commits).len();
    store
        .halt_after_accepted_commit
        .store(accepted + 1, Ordering::Release);
    let command = CommandId::new("root:admission-cut")?;
    let turn = reopened
        .start(command.clone(), "cut task")
        .await?
        .into_turn()
        .ok_or("cut turn admitted")?;
    let _ = tokio::time::timeout(Duration::from_secs(30), turn.result()).await?;
    reopened.abort_driver_for_test();
    tokio::task::yield_now().await;
    let admission = {
        let commits = lock_mutex(&store.accepted_commits);
        assert_eq!(commits.len(), accepted + 1, "one atomic admission commit");
        commits.last().ok_or("admission checkpoint")?.0.clone()
    };
    let ledger = admission.manifest().ledger();
    assert!(
        ledger.operations().len() < 4096,
        "admission includes pruning"
    );
    assert!(
        ledger
            .commands()
            .iter()
            .any(|record| record.command_id() == command.as_str())
    );
    assert!(matches!(
        ledger.operations().last().ok_or("root operation")?.state(),
        DurableOperationState::Prepared
    ));
    assert!(
        ledger
            .operations()
            .last()
            .ok_or("new sequence")?
            .operation_id()
            > old
                .manifest()
                .ledger()
                .operations()
                .last()
                .ok_or("old sequence")?
                .operation_id()
    );
    assert_eq!(
        old.manifest().ledger().operations().len(),
        4096,
        "old checkpoint stays immutable"
    );
    store.commits_halted.store(false, Ordering::Release);
    let address = install_cut(store, &admission)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:after-admission-cut", address, make()?)
        .await?
    else {
        return Err("after-admission cut did not reopen".into());
    };
    let duplicate = reopened
        .start(command, "cut task")
        .await?
        .into_result()
        .ok_or("admitted root survives cut")?
        .result()
        .await?;
    assert!(matches!(
        duplicate.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    Ok(reopened)
}
