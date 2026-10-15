use super::*;

pub(super) async fn historical_code_cell_continuation_recovery(
    root_finalized: bool,
    finalized_continuation: bool,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 89_500)?;
    let agent = sessions
        .create(
            "create:historical-code-cell",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("fixture only").await?;
    let turn_id = turn.id();
    let _ = turn.result().await?;
    let checkpoint = agent.checkpoint();
    let _ = agent.shutdown().await;
    let address = seed_code_cell_recovery_checkpoint(&store, checkpoint, turn_id).await?;
    let seeded = store.load_checkpoint(address.checkpoint_id).await?;
    let source = seeded.checkpoint();
    let cut = source.id();
    let settled = runtime_component(
        RuntimeComponentKind::Operation,
        b"settled evidence".to_vec(),
    )?;
    let mut records = Vec::new();
    let evaluation = source
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
            )
        })
        .ok_or("evaluation missing")?;
    let DurableOperationKind::CompositionEvaluation {
        attempt_id: evaluation_attempt,
        ..
    } = evaluation.kind()
    else {
        return Err("evaluation kind missing".into());
    };
    let mut continuation_id = 0;
    let mut open_child_id = 0;
    for record in source.manifest().ledger().operations() {
        match record.kind() {
            DurableOperationKind::Command { .. } => {
                let DurableOperationState::Completed(evidence) = record.state() else {
                    return Err("pending root missing".into());
                };
                records.push(if root_finalized {
                    record.append_transition(DurableOperationTransition::new(
                        cut,
                        DurableOperationState::Finalized(finalized_operation_evidence(
                            evidence,
                            cut,
                            source.manifest().canonical().history().identity(),
                        )?),
                    ))?
                } else {
                    record.clone()
                });
            }
            DurableOperationKind::Provider { .. } => {
                let end = record
                    .transitions()
                    .iter()
                    .position(|transition| {
                        matches!(transition.state(), DurableOperationState::Completed(_))
                    })
                    .ok_or("provider completion missing")?;
                records.push(DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    record.transitions()[..=end].to_vec(),
                )?);
            }
            DurableOperationKind::CompositionEvaluation { .. } => {
                let completed = record.append_transition(DurableOperationTransition::new(
                    cut,
                    DurableOperationState::Completed(settled.clone()),
                ))?;
                records.push(completed.append_transition(DurableOperationTransition::new(
                    cut,
                    DurableOperationState::Finalized(settled.clone()),
                ))?);
            }
            DurableOperationKind::CompositionContinuation { .. } => {
                continuation_id = record.operation_id();
                records.push(if finalized_continuation {
                    record
                        .append_transition(DurableOperationTransition::new(
                            cut,
                            DurableOperationState::Completed(settled.clone()),
                        ))?
                        .append_transition(DurableOperationTransition::new(
                            cut,
                            DurableOperationState::Finalized(settled.clone()),
                        ))?
                } else {
                    record.append_transition(DurableOperationTransition::new(
                        cut,
                        DurableOperationState::OutcomeUnknown(settled.clone()),
                    ))?
                });
            }
            DurableOperationKind::CompositionChild { .. } => {
                // Only sequences 6–11 remain obligations; earlier finalized
                // children and the evaluation have already left that set.
                // Children can belong to the evaluation or the later wait.
                for sequence in 1..=11 {
                    let mut kind = record.kind().clone();
                    let DurableOperationKind::CompositionChild {
                        attempt_id,
                        request_sequence,
                        ..
                    } = &mut kind
                    else {
                        return Err("child kind missing".into());
                    };
                    *request_sequence = sequence;
                    if sequence < 9 {
                        *attempt_id = *evaluation_attempt;
                    }
                    let (tag, parent, generation, cell, _, digest, request): (
                        String,
                        u64,
                        u64,
                        String,
                        u32,
                        Digest,
                        Vec<u8>,
                    ) = serde_json::from_slice(record.request().bytes())?;
                    let edge = serde_json::to_vec(&(
                        tag, parent, generation, cell, sequence, digest, request,
                    ))?;
                    let binding = Digest::sha256(
                        DigestDomain::ProviderOperation,
                        4,
                        &[b"plakous-agent/local-composition-child-edge-v2", &edge],
                    )?;
                    let DurableOperationKind::CompositionChild { call_id, .. } = &mut kind else {
                        return Err("child kind missing".into());
                    };
                    *call_id = local_composition_child_call_id(binding)?;
                    let dispatched = DurableOperationRecord::new(
                        record.operation_id() + u64::from(sequence) - 1,
                        kind,
                        runtime_component(RuntimeComponentKind::Operation, edge)?,
                        record.transitions().to_vec(),
                    )?;
                    if finalized_continuation && sequence == 11 {
                        open_child_id = dispatched.operation_id();
                        records.push(dispatched);
                        continue;
                    }
                    let completed =
                        dispatched.append_transition(DurableOperationTransition::new(
                            cut,
                            DurableOperationState::Completed(settled.clone()),
                        ))?;
                    records.push(if sequence < 6 {
                        completed.append_transition(DurableOperationTransition::new(
                            cut,
                            DurableOperationState::Finalized(settled.clone()),
                        ))?
                    } else {
                        completed
                    });
                }
            }
            _ => return Err("unexpected fixture operation".into()),
        }
    }
    let next = records
        .iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .ok_or("records missing")?
        + 1;
    let effect_call = ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(0xc0de_4001))?,
        ToolName::new("probe")?,
        ToolExecutionOwner::PlakLocal,
        ToolCallState::Requested,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
        "{}",
    )?;
    records.push(DurableOperationRecord::new(
        next,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id: *evaluation_attempt,
            call_id: effect_call.call_id(),
            source_index: 2,
        },
        runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&(settled.integrity(), effect_call))?,
        )?,
        vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_4005))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_4006))?,
                DurableOperationState::Claimed(settled.clone()),
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_4007))?,
                DurableOperationState::Dispatched,
            ),
            DurableOperationTransition::new(cut, DurableOperationState::Completed(settled.clone())),
        ],
    )?);
    let historical = checkpoint_with_code_cell_records(source, turn_id, records.clone())
        .map_err(|error| format!("historical fixture rejected: {error}"))?;
    assert!(matches!(
        historical.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
            ..
        }
    ));
    assert!(
        !historical
            .manifest()
            .lifecycle()
            .obligations()
            .iter()
            .any(|obligation| obligation.operation_id() == evaluation.operation_id())
    );
    let fold = code_cell_abandonment_fold(&historical)?;
    assert_eq!(fold.root_finalized, root_finalized);
    assert!(fold.turn_abandonment);
    assert_eq!(
        fold.operations,
        vec![(
            OperationId(if finalized_continuation {
                open_child_id
            } else {
                continuation_id
            }),
            crate::runtime::durable::DurableCodeCellAbandonmentDisposition::PossiblyStarted
        )]
    );
    assert_eq!(
        fold.evaluation_request,
        decode_local_composition_evaluation_request(evaluation.request())
            .ok_or("request missing")?
    );
    assert_eq!(
        historical
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>(),
        records
    );

    for fault in [
        "parent", "turn", "sequence", "attempt", "call", "request", "definite",
    ] {
        // A finalized continuation's request is settled history; only its
        // kind's parent and attempt bind its children.
        if finalized_continuation && fault == "request" {
            continue;
        }
        let mut bad = records.clone();
        let target = bad
            .iter_mut()
            .find(|record| match fault {
                "parent" | "request" => record.operation_id() == continuation_id,
                "sequence" | "attempt" => matches!(
                    record.kind(),
                    DurableOperationKind::CompositionChild {
                        request_sequence: 11,
                        ..
                    }
                ),
                "definite" => matches!(record.kind(), DurableOperationKind::Command { .. }),
                _ => record.operation_id() == evaluation.operation_id(),
            })
            .ok_or("fault target missing")?;
        let mut kind = target.kind().clone();
        let mut request = target.request().clone();
        let mut transitions = target.transitions().to_vec();
        match fault {
            "parent" => {
                if let DurableOperationKind::CompositionContinuation {
                    evaluation_operation_id,
                    ..
                } = &mut kind
                {
                    *evaluation_operation_id = next;
                }
            }
            "turn" => {
                if let DurableOperationKind::CompositionEvaluation { turn_id, .. } = &mut kind {
                    *turn_id = TurnId::try_from(uuid::Uuid::from_u128(0xc0de_4002))?;
                }
            }
            "sequence" => {
                if let DurableOperationKind::CompositionChild {
                    request_sequence, ..
                } = &mut kind
                {
                    *request_sequence = 6;
                }
            }
            "attempt" => {
                if let DurableOperationKind::CompositionChild { attempt_id, .. } = &mut kind {
                    *attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(0xc0de_4003))?;
                }
            }
            "call" => {
                if let DurableOperationKind::CompositionEvaluation { call_id, .. } = &mut kind {
                    *call_id = CallId::try_from(uuid::Uuid::from_u128(0xc0de_4004))?;
                }
            }
            "request" => {
                let mut continued = decode_local_composition_continuation_request(&request)
                    .ok_or("continuation request missing")?;
                continued.evaluation_operation_id = next;
                request = runtime_component(
                    RuntimeComponentKind::Operation,
                    encode_local_composition_continuation_request(&continued)?,
                )?;
            }
            "definite" => {
                let DurableOperationState::Completed(evidence) = transitions
                    .iter()
                    .find(|transition| {
                        matches!(transition.state(), DurableOperationState::Completed(_))
                    })
                    .ok_or("root completion missing")?
                    .state()
                else {
                    return Err("root completion missing".into());
                };
                let mut pending = decode_pending_terminal_record(evidence.bytes())
                    .ok_or("pending terminal missing")?;
                pending.outcome = DurableTerminalOutcome::Failed(DurableAgentError::from_error(
                    &AgentError::new(AgentErrorKind::Internal, "definite failure"),
                )?);
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    encode_pending_terminal_record(pending)?,
                )?;
                if root_finalized {
                    transitions.pop();
                }
                transitions.pop();
                transitions.push(DurableOperationTransition::new(
                    cut,
                    DurableOperationState::Completed(evidence.clone()),
                ));
                if root_finalized {
                    transitions.push(DurableOperationTransition::new(
                        cut,
                        DurableOperationState::Finalized(finalized_operation_evidence(
                            &evidence,
                            cut,
                            source.manifest().canonical().history().identity(),
                        )?),
                    ));
                }
            }
            _ => unreachable!(),
        }
        *target = DurableOperationRecord::new(target.operation_id(), kind, request, transitions)?;
        // The manifest also fences crossed composition identities and child
        // sequences. A malformed history must never reach an accepted fold.
        match checkpoint_with_code_cell_records(source, turn_id, bad) {
            Ok(rejected) => assert!(
                code_cell_abandonment_fold(&rejected).is_err(),
                "accepted {fault}"
            ),
            Err(error) => assert!(
                error.downcast_ref::<crate::RuntimeManifestError>()
                    == Some(&crate::RuntimeManifestError::InvalidLedger),
                "unexpected {fault} rejection: {error}"
            ),
        }
    }
    // Publish the historical cut as an actual store head. Reopen must use the
    // closure publisher, not merely accept this shape in the pure fold.
    let lease = store
        .acquire(AcquireBranch::new(
            address.session(),
            RunId::try_from(uuid::Uuid::from_u128(0xc0de_4010))?,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        ))
        .await?;
    let loaded = store.load_branch(address.session()).await?;
    let prior = historical.manifest();
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_4011))?;
    let topology = prior.topology();
    let branch_revision = topology.branch_revision + 1;
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: prior.lifecycle(),
        history_identity: prior.canonical().history().identity(),
        ledger: prior.ledger(),
        context_identity: prior.canonical().context().identity(),
        family_artifact: prior.family_artifact(),
    })?;
    let published_checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision,
            lease_fence: lease.fence().get(),
            commit_identity,
            ..topology
        },
        prior.lifecycle().clone(),
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        prior.ledger().clone(),
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    // Full encoding deliberately seeds a crash cut; the live incremental
    // publisher correctly rejects rewinding the fixture's settled provider.
    let bundle = RuntimeCheckpointCodec::bundle(&published_checkpoint, None)?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    let published =
        DurableSessionAddress::new(address.session_id, address.branch_id, checkpoint_id);
    let reopening_model = artifact_test_model(0, 89_550)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:historical-code-cell",
            published,
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&reopening_model)),
        )
        .await
        .map_err(|error| {
            format!(
                "historical reopen (root_finalized={root_finalized}): {}",
                error.message()
            )
        })?
    else {
        return Err("historical Code Cell did not close on reopen".into());
    };
    let head = reopened.checkpoint();
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(
        store
            .load_branch(address.session())
            .await?
            .checkpoint()
            .checkpoint()
            .id(),
        head.id()
    );
    let history = head.manifest().canonical().history();
    let prefix = historical.manifest().canonical().history();
    assert_eq!(history.segments().len(), prefix.segments().len() + 1);
    assert_eq!(
        history.segment_prefix_identity(prefix.segments().len())?,
        prefix.identity()
    );
    // The sole addition is an abandonment note, not another outer exchange.
    assert!(
        history
            .segments()
            .into_iter()
            .last()
            .is_some_and(|segment| segment.kind() != HistorySegmentKind::ToolExchange)
    );
    assert_eq!(head.manifest().ledger().operations().len(), records.len());
    for before in &records {
        let after = head
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| record.operation_id() == before.operation_id())
            .ok_or("recovered operation missing")?;
        assert_eq!(after.request(), before.request());
        assert_eq!(after.kind(), before.kind());
        assert!(after.transitions().starts_with(before.transitions()));
        match before.state() {
            DurableOperationState::Finalized(evidence)
                if matches!(before.kind(), DurableOperationKind::Command { .. }) =>
            {
                assert_eq!(after.transitions().len(), before.transitions().len() + 1);
                let original = read_terminal_result_record(evidence.bytes())
                    .map_err(|error| error.detail())?;
                let DurableOperationState::Finalized(enriched) = after.state() else {
                    return Err("enriched root is not finalized".into());
                };
                let mut terminal = read_terminal_result_record(enriched.bytes())
                    .map_err(|error| error.detail())?;
                assert_eq!(
                    terminal.abandonment.take().as_ref(),
                    reopened.abandoned_turn()
                );
                assert_eq!(
                    terminal, original,
                    "enrichment preserves the original terminal"
                );
            }
            DurableOperationState::Finalized(_) => assert_eq!(after, before),
            DurableOperationState::Completed(evidence)
                if !matches!(before.kind(), DurableOperationKind::Command { .. }) =>
            {
                assert_eq!(after.transitions().len(), before.transitions().len() + 1);
                assert_eq!(
                    after.state(),
                    &DurableOperationState::Finalized(evidence.clone())
                );
            }
            DurableOperationState::Completed(_) => {
                assert_eq!(after.transitions().len(), before.transitions().len() + 1);
            }
            DurableOperationState::OutcomeUnknown(_) => {
                assert_eq!(before.operation_id(), continuation_id);
                assert_eq!(after.transitions().len(), before.transitions().len() + 1);
                assert!(matches!(after.state(), DurableOperationState::Finalized(_)));
            }
            DurableOperationState::Dispatched => {
                assert_eq!(before.operation_id(), open_child_id);
                assert!(matches!(after.state(), DurableOperationState::Finalized(_)));
            }
            _ => return Err("unexpected recovery source state".into()),
        }
    }
    let facts = crate::feed::facts::commit_facts(&published_checkpoint, &head)?;
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
            .count(),
        usize::from(!root_finalized),
        "enrichment emits no second terminal"
    );
    let root = head
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id))
        .ok_or("root command missing")?;
    let command_id = CommandId::new(root.command_id())?;
    let terminal = terminal_record_at_checkpoint(&head, &command_id)?;
    assert!(matches!(
        terminal.outcome,
        DurableTerminalOutcome::RecoveryRequired(_)
    ));
    let pending_root = records
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Command { .. }))
        .ok_or("root missing")?;
    let pending = pending_root
        .transitions()
        .iter()
        .find_map(|transition| match transition.state() {
            DurableOperationState::Completed(evidence) => {
                decode_pending_terminal_record(evidence.bytes())
            }
            _ => None,
        })
        .ok_or("pending root missing")?;
    assert_eq!(terminal.outcome, pending.outcome);
    let target = DurableSessionAddress::new(address.session_id, address.branch_id, head.id());
    reopened.shutdown().await?;
    let commits = store.commit_calls.load(Ordering::Acquire);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:historical-code-cell-again",
            target,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model)),
        )
        .await?
    else {
        return Err("quiescent historical Code Cell did not reopen".into());
    };
    assert_eq!(reopened.checkpoint().id(), head.id());
    assert_eq!(
        reopened.checkpoint().manifest().canonical().history(),
        history
    );
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits);
    assert_eq!(
        terminal_record_at_checkpoint(&reopened.checkpoint(), &command_id)?,
        terminal
    );
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    Ok(())
}

#[tokio::test]
async fn code_cell_abandonment_recovers_lost_final_ack_reopens_and_is_idempotent() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 89_500)?;
    let agent = sessions
        .create(
            "create:code-cell-abandon-lost-ack",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("recovery-only code cell").await?;
    let turn_id = turn.id();
    let terminal = turn.result().await?;
    assert!(matches!(
        terminal.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(terminal.recovery_operation().is_some());
    let checkpoint = agent.checkpoint();
    assert_eq!(terminal.checkpoint_id(), checkpoint.id());
    let _ = agent.shutdown().await;
    let source = seed_code_cell_recovery_checkpoint(&store, checkpoint, turn_id)
        .await
        .map_err(|error| format!("seed failed: {error:?}"))?;
    let request = DurableCodeCellAbandonmentRequest::new("abandon:code-cell-lost-ack", source)?;
    let identity = DurableCodeCellAbandonmentIdentity::new(&request)?;
    let seeded = store.load_checkpoint(source.checkpoint_id).await?;
    let source_history = seeded.checkpoint().manifest().canonical().history();
    let source_segment_count = source_history.segments().len();
    let composition = seeded
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
                    | DurableOperationKind::CompositionContinuation { .. }
                    | DurableOperationKind::CompositionChild { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(composition.len(), 3);
    let evaluation_attempt = composition
        .iter()
        .find_map(|record| match record.kind() {
            DurableOperationKind::CompositionEvaluation { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .ok_or("seeded evaluation attempt missing")?;
    let continuation_attempt = composition
        .iter()
        .find_map(|record| match record.kind() {
            DurableOperationKind::CompositionContinuation { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .ok_or("seeded continuation attempt missing")?;
    let child_attempt = composition
        .iter()
        .find_map(|record| match record.kind() {
            DurableOperationKind::CompositionChild { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .ok_or("seeded child attempt missing")?;
    assert_ne!(evaluation_attempt, continuation_attempt);
    assert_eq!(child_attempt, continuation_attempt);
    assert!(composition.iter().all(|record| {
        matches!(record.state(), DurableOperationState::Dispatched)
            && record
                .transitions()
                .iter()
                .filter(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
                .count()
                == 1
    }));
    let source_root = seeded
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Command { turn_id: Some(bound) }
                    if *bound == turn_id
            )
        })
        .ok_or("source root missing")?;
    let DurableOperationState::Completed(source_root_evidence) = source_root.state() else {
        return Err("source root was not pending finalization".into());
    };
    let source_terminal = decode_pending_terminal_record(source_root_evidence.bytes())
        .ok_or("source terminal missing")?;
    code_cell_abandonment_fold(seeded.checkpoint()).map_err(|error| {
        format!(
            "seeded Code Cell fold failed: {:?} {}",
            error.kind(),
            error.message()
        )
    })?;
    // A command for the same turn but different accepted bytes cannot stand
    // in for the root at the dispatched crash cut.
    let mut crossed_records = seeded
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let root = crossed_records
        .iter_mut()
        .find(|record| record.operation_id() == source_root.operation_id())
        .ok_or("crossed root missing")?;
    let transitions = root
        .transitions()
        .iter()
        .take_while(|transition| !matches!(transition.state(), DurableOperationState::Completed(_)))
        .cloned()
        .collect::<Vec<_>>();
    *root = DurableOperationRecord::new(
        root.operation_id(),
        root.kind().clone(),
        runtime_component(RuntimeComponentKind::Operation, b"crossed command".to_vec())?,
        transitions,
    )?;
    let crossed = checkpoint_with_code_cell_records(seeded.checkpoint(), turn_id, crossed_records)?;
    let Err(corrupt) = code_cell_abandonment_fold(&crossed) else {
        return Err("crossed dispatched root command was admitted".into());
    };
    assert_eq!(corrupt.store_error(), Some(SessionStoreError::corrupt()));
    let mut ambiguous_records = seeded
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let evaluation = ambiguous_records
        .iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
            )
        })
        .ok_or("evaluation missing")?;
    let duplicate_operation_id = ambiguous_records
        .iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("operation identity exhausted")?;
    ambiguous_records.push(DurableOperationRecord::new(
        duplicate_operation_id,
        evaluation.kind().clone(),
        evaluation.request().clone(),
        evaluation.transitions().to_vec(),
    )?);
    let ambiguous =
        checkpoint_with_code_cell_records(seeded.checkpoint(), turn_id, ambiguous_records)?;
    let Err(ambiguity) = code_cell_abandonment_fold(&ambiguous) else {
        return Err("duplicate evaluation was admitted".into());
    };
    assert_eq!(ambiguity.store_error(), Some(SessionStoreError::corrupt()));
    crate::recovery_scope::require_recoverable(seeded.checkpoint())
        .map_err(|error| format!("seeded Code Cell recovery scope failed: {error:?}"))?;
    let commits_before = store.commit_calls.load(Ordering::Acquire);
    store.lose_commit_ack_on(commits_before.saturating_add(2));
    let routed = sessions
            .abandon_code_cell(request.clone())
            .await
            .map_err(|error| {
                let head = futures::executor::block_on(store.load_branch(source.session()))
                    .ok()
                    .map(|loaded| loaded.checkpoint().checkpoint().id());
                format!(
                    "Code Cell abandonment failed: {:?} {} {:?}, head {head:?}, source {:?}, evidence {:?}, final {:?}",
                    error.kind(),
                    error.message(),
                    error.store_error(),
                    source.checkpoint_id,
                    identity.evidence_checkpoint,
                    identity.final_checkpoint
                )
            })?;
    let DurableCodeCellAbandonmentOutcome::Finalized {
        source: exact_source,
        target,
    } = routed
    else {
        return Err(format!("Code Cell reconciliation did not finalize: {routed:?}").into());
    };
    let first = DurableCodeCellAbandonmentOutcome::Finalized {
        source: exact_source,
        target,
    };
    assert_eq!(exact_source, source);
    assert_eq!(target.checkpoint_id, identity.final_checkpoint);
    assert_eq!(store.lost_commit_acks.load(Ordering::Acquire), 1);
    assert!(store.recover_commit_calls.load(Ordering::Acquire) >= 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let commits_after = store.commit_calls.load(Ordering::Acquire);
    assert_eq!(sessions.abandon_code_cell(request.clone()).await?, first);
    let reopened = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    assert_eq!(reopened.abandon_code_cell(request).await?, first);
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits_after);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let stale = reopened
        .abandon_code_cell(DurableCodeCellAbandonmentRequest::new(
            "abandon:code-cell-stale",
            source,
        )?)
        .await;
    assert!(
        stale.is_err(),
        "stale source unexpectedly reconciled: {stale:?}"
    );
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits_after);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let target = store.load_checkpoint(target.checkpoint_id).await?;
    assert!(matches!(
        target.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let target_manifest = target.checkpoint().manifest();
    let target_history = target_manifest.canonical().history();
    assert_eq!(target_history.segments().len(), source_segment_count + 1);
    let expected_call_id = CallId::try_from(uuid::Uuid::from_u128(0xc0de_0002))?;
    let tool_results = target_history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) if result.call_id() == expected_call_id => {
                Some(result)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].kind(), ToolResultKind::Failure);
    assert_eq!(
        tool_results[0].correlation().remote_call_id(),
        Some("remote-code-cell-call")
    );
    let final_roots = target_manifest
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| record.operation_id() == source_root.operation_id())
        .collect::<Vec<_>>();
    assert_eq!(
        final_roots.len(),
        1,
        "root TurnResult authority was duplicated"
    );
    let final_root = final_roots[0];
    let DurableOperationState::Finalized(final_root_evidence) = final_root.state() else {
        return Err("final root was not finalized".into());
    };
    let final_terminal = decode_terminal_result_record(final_root_evidence.bytes())
        .ok_or("final terminal missing")?;
    assert_eq!(final_terminal.command_id, source_terminal.command_id);
    assert_eq!(final_terminal.turn_id, source_terminal.turn_id);
    assert_eq!(final_terminal.operation_id, source_terminal.operation_id);
    assert_eq!(final_terminal.outcome, source_terminal.outcome);
    assert_eq!(final_terminal.usage, source_terminal.usage);
    let final_composition = target_manifest
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
                    | DurableOperationKind::CompositionContinuation { .. }
                    | DurableOperationKind::CompositionChild { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(final_composition.len(), composition.len());
    assert!(final_composition.iter().all(|record| {
        record
            .transitions()
            .iter()
            .filter(|transition| matches!(transition.state(), DurableOperationState::Dispatched))
            .count()
            == 1
    }));
    Ok(())
}

#[tokio::test]
async fn code_cell_abandonment_retries_from_evidence_crash_cut_without_replay() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 89_700)?;
    let agent = sessions
        .create(
            "create:code-cell-abandon-crash-cut",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("crash-cut code cell").await?;
    let turn_id = turn.id();
    let terminal = turn.result().await?;
    assert!(matches!(
        terminal.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    assert!(terminal.recovery_operation().is_some());
    let checkpoint = agent.checkpoint();
    assert_eq!(terminal.checkpoint_id(), checkpoint.id());
    let _ = agent.shutdown().await;
    let source = seed_code_cell_recovery_checkpoint(&store, checkpoint, turn_id)
        .await
        .map_err(|error| format!("seed failed: {error:?}"))?;
    let request = DurableCodeCellAbandonmentRequest::new("abandon:code-cell-crash-cut", source)?;
    let identity = DurableCodeCellAbandonmentIdentity::new(&request)?;
    store.halt_after_checkpoint(identity.evidence_checkpoint);
    let first = sessions.abandon_code_cell(request.clone()).await;
    assert!(first.is_err(), "unexpected crash-cut outcome: {first:?}");
    assert_eq!(
        store
            .load_branch(source.session())
            .await?
            .checkpoint()
            .checkpoint()
            .id(),
        identity.evidence_checkpoint,
        "crash-cut failure was {first:?}"
    );
    store.resume_commits();
    let recovered = sessions.abandon_code_cell(request.clone()).await?;
    assert!(matches!(
        recovered,
        DurableCodeCellAbandonmentOutcome::Finalized { source: exact, target }
            if exact == source && target.checkpoint_id == identity.final_checkpoint
    ));
    let commits_after = store.commit_calls.load(Ordering::Acquire);
    assert_eq!(sessions.abandon_code_cell(request).await?, recovered);
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits_after);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    Ok(())
}
