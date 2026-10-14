use super::super::turn_recovery::{abandoned_turn_recovery_fold, local_terminal_recovery_fold};
use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
enum MixedRoot {
    Finalized,
    DebtOnly,
    PendingTerminal,
    Dispatched,
    Prepared,
    QueuedSteer,
    CompletedSteer,
}

async fn seed_mixed_debt(
    store: &Arc<TestCreateStore>,
    source: Arc<RuntimeCheckpoint>,
    turn_id: TurnId,
    not_dispatched: bool,
    root_state: MixedRoot,
) -> TestResult<DurableSessionAddress> {
    let source = seed_code_cell_recovery_checkpoint(store, source, turn_id).await?;
    let loaded = store.load_checkpoint(source.checkpoint_id).await?;
    let prior = loaded.checkpoint().manifest();
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_1002))?;
    let settled = runtime_component(RuntimeComponentKind::Operation, b"settled sibling".to_vec())?;
    let mut records = Vec::new();
    for record in prior.ledger().operations() {
        let replacement = match record.kind() {
            DurableOperationKind::Command {
                turn_id: Some(bound),
            } if *bound == turn_id => {
                if matches!(
                    root_state,
                    MixedRoot::Dispatched
                        | MixedRoot::Prepared
                        | MixedRoot::QueuedSteer
                        | MixedRoot::CompletedSteer
                ) {
                    let end = record
                        .transitions()
                        .iter()
                        .position(|transition| {
                            matches!(transition.state(), DurableOperationState::Dispatched)
                        })
                        .ok_or("root dispatch missing")?;
                    let end = if root_state == MixedRoot::Prepared {
                        0
                    } else {
                        end
                    };
                    records.push(DurableOperationRecord::new(
                        record.operation_id(),
                        record.kind().clone(),
                        record.request().clone(),
                        record.transitions()[..=end].to_vec(),
                    )?);
                    continue;
                }
                if root_state == MixedRoot::PendingTerminal {
                    records.push(record.clone());
                    continue;
                }
                let DurableOperationState::Completed(evidence) = record.state() else {
                    return Err("root evidence missing".into());
                };
                record.append_transition(DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(finalized_operation_evidence(
                        evidence,
                        checkpoint_id,
                        prior.canonical().history().identity(),
                    )?),
                ))?
            }
            DurableOperationKind::Provider { .. } => {
                if root_state == MixedRoot::DebtOnly && not_dispatched {
                    // A provider record carries provider-outcome-v2 evidence.
                    let unsent = crate::runtime::operation_record::test_support::outcome_component(
                        Err(&AgentError::from_model(ModelError::new(
                            ModelErrorKind::Protocol,
                            "start refused before dispatch",
                        ))),
                    )?;
                    records.push(DurableOperationRecord::new(
                        record.operation_id(),
                        record.kind().clone(),
                        record.request().clone(),
                        vec![
                            record.transitions()[0].clone(),
                            DurableOperationTransition::new(
                                checkpoint_id,
                                DurableOperationState::ProvenNotDispatched(unsent),
                            ),
                        ],
                    )?);
                    continue;
                }
                let end = record
                    .transitions()
                    .iter()
                    .position(|transition| {
                        matches!(transition.state(), DurableOperationState::Completed(_))
                    })
                    .ok_or("provider completion missing")?;
                DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    record.transitions()[..=end].to_vec(),
                )?
            }
            DurableOperationKind::CompositionContinuation { .. } => {
                if root_state == MixedRoot::DebtOnly {
                    record.append_transition(DurableOperationTransition::new(
                        checkpoint_id,
                        if not_dispatched {
                            DurableOperationState::ProvenNotDispatched(settled.clone())
                        } else {
                            DurableOperationState::Completed(settled.clone())
                        },
                    ))?
                } else if not_dispatched {
                    DurableOperationRecord::new(
                        record.operation_id(),
                        record.kind().clone(),
                        record.request().clone(),
                        record.transitions()[..1].to_vec(),
                    )?
                } else {
                    record.append_transition(DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::OutcomeUnknown(settled.clone()),
                    ))?
                }
            }
            DurableOperationKind::CompositionEvaluation { .. }
                if root_state == MixedRoot::Prepared =>
            {
                record.clone()
            }
            _ => record.append_transition(DurableOperationTransition::new(
                checkpoint_id,
                if root_state == MixedRoot::DebtOnly && not_dispatched {
                    DurableOperationState::ProvenNotDispatched(settled.clone())
                } else {
                    DurableOperationState::Completed(settled.clone())
                },
            ))?,
        };
        records.push(replacement);
    }
    let next = records
        .iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .ok_or("records")?
        + 1;
    records.push(DurableOperationRecord::new(
        next,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xc0de_3000))?,
            call_id: CallId::try_from(uuid::Uuid::from_u128(0xc0de_3001))?,
            source_index: 2,
        },
        runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&(
                settled.integrity(),
                ToolCallBlock::new(
                    CallId::try_from(uuid::Uuid::from_u128(0xc0de_3001))?,
                    ToolName::new("probe")?,
                    ToolExecutionOwner::PlakLocal,
                    ToolCallState::Requested,
                    ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
                    "{}",
                )?,
            ))?,
        )?,
        vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_3002))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_3003))?,
                DurableOperationState::Claimed(settled.clone()),
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_3004))?,
                DurableOperationState::Dispatched,
            ),
            DurableOperationTransition::new(
                checkpoint_id,
                if root_state == MixedRoot::DebtOnly && not_dispatched {
                    DurableOperationState::ProvenNotDispatched(settled.clone())
                } else {
                    DurableOperationState::Completed(settled.clone())
                },
            ),
        ],
    )?);
    if root_state == MixedRoot::DebtOnly {
        records.push(DurableOperationRecord::new(
            next + 1,
            DurableOperationKind::ProcessSession {
                turn_id,
                session_id: crate::manifest::ProcessSessionId::new([0xf2; 16])?,
                owner_generation: crate::manifest::ProcessSessionOwnerGeneration::new(1)?,
            },
            settled.clone(),
            if not_dispatched {
                vec![
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xf232))?,
                        DurableOperationState::Prepared,
                    ),
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::ProvenNotDispatched(settled),
                    ),
                ]
            } else {
                vec![
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xf232))?,
                        DurableOperationState::Prepared,
                    ),
                    DurableOperationTransition::new(
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xf233))?,
                        DurableOperationState::Dispatched,
                    ),
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Completed(settled),
                    ),
                ]
            },
        )?);
    }
    let mut commands = prior
        .ledger()
        .commands()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    for index in 0..if root_state == MixedRoot::DebtOnly {
        0
    } else {
        8_u64
    } {
        let queued_turn = TurnId::try_from(uuid::Uuid::from_u128(0xc0de_2000 + u128::from(index)))?;
        let command = TurnCommand::FollowUp {
            command_id: CommandId::new(if index == 7 {
                "embedder:queued-follow-up".to_owned()
            } else {
                format!("plak:mail:queued-{index}:follow-up")
            })?,
            target_turn_id: turn_id,
            text: "queued mail".into(),
        };
        let component = runtime_component(
            RuntimeComponentKind::Command,
            serde_json::to_vec(&DurableCommandEnvelope {
                version: 1,
                canonical_input: crate::runtime::control::command_canonical_bytes(&command),
                effect: DurableActorCommandEffect::Root,
                rejection: None,
            })?,
        )?;
        commands.push(DurableCommandRecord::new(
            command.command_id().as_str(),
            commands.len() as u64 + 1,
            Some(queued_turn),
            crate::runtime::control::command_binding(&command)?,
            component.clone(),
            DurableCommandDisposition::Accepted,
            None,
        )?);
        records.push(DurableOperationRecord::new(
            next + 1 + index,
            DurableOperationKind::Command {
                turn_id: Some(queued_turn),
            },
            runtime_component(RuntimeComponentKind::Operation, component.bytes().to_vec())?,
            vec![DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Prepared,
            )],
        )?);
    }
    if matches!(
        root_state,
        MixedRoot::QueuedSteer | MixedRoot::CompletedSteer
    ) {
        let command = TurnCommand::Steer {
            command_id: CommandId::new("fixture:queued-steer")?,
            turn_id,
            text: "pending steering during live cell".into(),
        };
        let component = runtime_component(
            RuntimeComponentKind::Command,
            serde_json::to_vec(&DurableCommandEnvelope {
                version: 1,
                canonical_input: crate::runtime::control::command_canonical_bytes(&command),
                effect: DurableActorCommandEffect::SteerQueued { position: 1 },
                rejection: None,
            })?,
        )?;
        commands.push(DurableCommandRecord::new(
            command.command_id().as_str(),
            commands.len() as u64 + 1,
            Some(turn_id),
            crate::runtime::control::command_binding(&command)?,
            component.clone(),
            DurableCommandDisposition::Accepted,
            None,
        )?);
        let mut transitions = vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_4001))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_4002))?,
                DurableOperationState::Dispatched,
            ),
        ];
        if root_state == MixedRoot::CompletedSteer {
            transitions.push(DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Completed(runtime_component(
                    RuntimeComponentKind::Operation,
                    b"steering-applied".to_vec(),
                )?),
            ));
        }
        records.push(DurableOperationRecord::new(
            next + 9,
            DurableOperationKind::Command {
                turn_id: Some(turn_id),
            },
            runtime_component(RuntimeComponentKind::Operation, component.bytes().to_vec())?,
            transitions,
        )?);
    }
    let ledger = RuntimeOperationLedger::new(commands, records)?;
    let lifecycle = working_lifecycle(turn_id, &ledger)?;
    let topology = prior.topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(uuid::Uuid::from_u128(0xc0de_1003))?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let head = store.load_branch(session).await?.head();
    let branch_revision = topology.branch_revision + 1;
    let context = prior.canonical().context().retained_for_exact_resume();
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: &lifecycle,
        history_identity: prior.canonical().history().identity(),
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: prior.family_artifact(),
    })?;
    let canonical = RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        prior.canonical().history().clone(),
        WorkingProjection::default(),
        context,
    )?;
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision,
            fork: topology.fork,
            lease_fence: lease.fence().get(),
            commit_identity,
        },
        lifecycle,
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        canonical,
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), head, bundle))
        .await?;
    store.release(lease).await?;
    Ok(DurableSessionAddress::new(
        topology.session_id,
        topology.branch_id,
        checkpoint_id,
    ))
}

#[tokio::test]
async fn mixed_code_cell_debt_reopens_without_inventing_a_cell_result() -> TestResult {
    assert_mixed_debt(false, MixedRoot::Finalized).await
}

#[tokio::test]
async fn finalized_root_with_only_local_debt_reopens_quiescent_without_second_terminal()
-> TestResult {
    for not_dispatched in [false, true] {
        assert_finalized_local_debt(not_dispatched).await?;
    }
    Ok(())
}

async fn assert_finalized_local_debt(not_dispatched: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:debt-only",
            durable_artifact_test_builder(workspace.clone(), artifact_test_model(0, 89_700)?),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("local finalization debt").await?;
    let turn_id = turn.id();
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let checkpoint = agent.checkpoint();
    let _ = agent.shutdown().await;
    let source = seed_mixed_debt(
        &store,
        checkpoint,
        turn_id,
        not_dispatched,
        MixedRoot::DebtOnly,
    )
    .await?;
    let seeded = store.load_checkpoint(source.checkpoint_id).await?;
    assert_crossed_finalization_obligations_are_corrupt(seeded.checkpoint())?;
    assert!(matches!(
        super::super::turn_recovery::root_turn_closure_for_test(seeded.checkpoint())?,
        Some(super::super::turn_recovery::RootClosure::FinalizeRecorded {
            code_cell: None,
            ..
        })
    ));
    let before = seeded.checkpoint().manifest();
    let history = before.canonical().history().identity();
    let command = before
        .ledger()
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id))
        .ok_or("root command")?;
    let terminal = terminal_record_for_command(before.ledger(), command)?;
    for foreign_turn in [false, true] {
        let records = before
            .ledger()
            .operations()
            .into_iter()
            .map(|record| {
                let DurableOperationKind::Effect {
                    attempt_id,
                    call_id,
                    source_index,
                    ..
                } = record.kind()
                else {
                    return Ok(record.clone());
                };
                let (kind, transitions) = if foreign_turn {
                    (
                        DurableOperationKind::Effect {
                            turn_id: TurnId::try_from(uuid::Uuid::from_u128(0xf23f))?,
                            attempt_id: *attempt_id,
                            call_id: *call_id,
                            source_index: *source_index,
                        },
                        record.transitions().to_vec(),
                    )
                } else {
                    let end = record
                        .transitions()
                        .iter()
                        .position(|transition| {
                            matches!(transition.state(), DurableOperationState::Dispatched)
                        })
                        .ok_or("effect dispatch")?;
                    (record.kind().clone(), record.transitions()[..=end].to_vec())
                };
                Ok(DurableOperationRecord::new(
                    record.operation_id(),
                    kind,
                    record.request().clone(),
                    transitions,
                )?)
            })
            .collect::<TestResult<Vec<_>>>()?;
        let changed = checkpoint_with_code_cell_records(seeded.checkpoint(), turn_id, records);
        if foreign_turn {
            assert!(
                changed.is_err(),
                "foreign-turn debt is invalid lifecycle authority"
            );
        } else {
            assert!(matches!(
                super::super::turn_recovery::root_turn_closure_for_test(&changed?)?,
                Some(super::super::turn_recovery::RootClosure::FinalizeRecorded {
                    code_cell: None,
                    ..
                })
            ));
        }
    }
    let missing_terminal = before
        .ledger()
        .operations()
        .into_iter()
        .map(|record| {
            if !matches!(record.kind(), DurableOperationKind::Command { .. }) {
                return Ok(record.clone());
            }
            let mut transitions = record.transitions().to_vec();
            let last = transitions.last_mut().ok_or("root terminal")?;
            *last = DurableOperationTransition::new(
                seeded.checkpoint().id(),
                DurableOperationState::Finalized(runtime_component(
                    RuntimeComponentKind::Operation,
                    b"not a durable terminal".to_vec(),
                )?),
            );
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                transitions,
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let refused =
        checkpoint_with_code_cell_records(seeded.checkpoint(), turn_id, missing_terminal)?;
    assert!(
        local_terminal_recovery_fold(&refused).is_err(),
        "finalized state alone is not terminal evidence"
    );
    assert!(
        local_terminal_recovery_fold(seeded.checkpoint()).is_ok(),
        "exact debt-only head must close locally"
    );
    crate::recovery_scope::require_recoverable(seeded.checkpoint())?;
    let model = artifact_test_model(0, 89_800)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:debt-only",
            source,
            durable_artifact_test_builder(workspace, model.clone()),
        )
        .await?
    else {
        return Err("debt-only head refused reopen".into());
    };
    assert!(reopened.abandoned_turn().is_none());
    let head = reopened.checkpoint();
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(head.manifest().canonical().history().identity(), history);
    assert_eq!(
        terminal_record_for_command(head.manifest().ledger(), command)?,
        terminal
    );
    for old in before.ledger().operations() {
        let new = head
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| record.operation_id() == old.operation_id())
            .ok_or("operation")?;
        if matches!(old.state(), DurableOperationState::Finalized(_)) {
            assert_eq!(new, old);
        } else {
            let (DurableOperationState::Completed(evidence)
            | DurableOperationState::ProvenNotDispatched(evidence)) = old.state()
            else {
                return Err("unexpected debt state".into());
            };
            assert_eq!(
                new.state(),
                &DurableOperationState::Finalized(finalized_operation_evidence(
                    evidence,
                    head.id(),
                    history
                )?)
            );
            assert_eq!(
                &new.transitions()[..old.transitions().len()],
                old.transitions()
            );
        }
    }
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn prepared_code_cell_debt_reopens_as_nothing_started() -> TestResult {
    assert_mixed_debt(true, MixedRoot::Finalized).await
}

#[tokio::test]
async fn mixed_code_cell_debt_with_pending_root_finalizes_without_a_cell_result() -> TestResult {
    assert_mixed_debt(false, MixedRoot::PendingTerminal).await
}

#[tokio::test]
async fn queued_steer_during_live_cell_reopens_without_corruption() -> TestResult {
    assert_mixed_debt(false, MixedRoot::QueuedSteer).await
}

#[tokio::test]
async fn completed_steer_during_live_cell_reopens_without_corruption() -> TestResult {
    assert_mixed_debt(false, MixedRoot::CompletedSteer).await
}

#[tokio::test]
async fn prepared_root_completed_provider_and_live_cell_reopen() -> TestResult {
    assert_mixed_debt(false, MixedRoot::Prepared).await
}

async fn assert_mixed_debt(not_dispatched: bool, root_state: MixedRoot) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let model = artifact_test_model(0, 89_700)?;
    let agent = sessions
        .create(
            "create:mixed-code-cell",
            durable_artifact_test_builder(Arc::clone(&workspace), model),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("mixed code cell debt").await?;
    let turn_id = turn.id();
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let checkpoint = agent.checkpoint();
    let _ = agent.shutdown().await;
    let source = seed_mixed_debt(&store, checkpoint, turn_id, not_dispatched, root_state).await?;
    let seeded = store.load_checkpoint(source.checkpoint_id).await?;
    assert_crossed_finalization_obligations_are_corrupt(seeded.checkpoint())?;
    code_cell_abandonment_fold(seeded.checkpoint())
        .map_err(|error| format!("fold: {}", error.message()))?;
    crate::recovery_scope::require_recoverable(seeded.checkpoint())?;
    let records = seeded.checkpoint().manifest().ledger().operations();
    let uncertain = records
        .iter()
        .filter(|record| matches!(record.state(), DurableOperationState::OutcomeUnknown(_)))
        .count();
    assert_eq!(uncertain, usize::from(!not_dispatched));
    assert_eq!(
        records
            .iter()
            .filter(
                |record| matches!(record.kind(), DurableOperationKind::Command { .. })
                    && matches!(record.state(), DurableOperationState::Prepared)
            )
            .count(),
        8 + usize::from(root_state == MixedRoot::Prepared)
    );
    let refused_records = records
        .into_iter()
        .map(|record| {
            if !matches!(record.kind(), DurableOperationKind::Effect { .. }) {
                return Ok(record.clone());
            }
            let transitions = record.transitions();
            let end = transitions
                .iter()
                .position(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
                .ok_or("effect dispatch missing")?;
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                transitions[..=end].to_vec(),
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let uncertain_effects = refused_records
        .iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Effect { .. }))
        .map(|record| OperationId(record.operation_id()))
        .collect::<Vec<_>>();
    assert_ne!(uncertain_effects, [] as [OperationId; 0]);
    let refused = checkpoint_with_code_cell_records(seeded.checkpoint(), turn_id, refused_records)?;
    // An uncertain nested effect is not Code Cell finalization debt and never
    // takes Code Cell evidence; it makes the closure a turn abandonment, which
    // settles it by the ordinary effect rule.
    let cell = code_cell_abandonment_fold(&refused)?;
    assert!(cell.turn_abandonment);
    assert!(
        cell.operations
            .iter()
            .all(|(operation, _)| !uncertain_effects.contains(operation))
    );
    let turn = abandoned_turn_recovery_fold(&refused)?;
    assert!(
        uncertain_effects
            .iter()
            .all(|operation| turn.effect_operations.contains(operation))
    );
    let before = seeded
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .len();
    let reopening_model = artifact_test_model(0, 89_750)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:mixed-code-cell",
            source,
            durable_artifact_test_builder(workspace, Arc::clone(&reopening_model)),
        )
        .await
        .map_err(|error| format!("open: {}", error.message()))?
    else {
        return Err("mixed Code Cell debt did not reopen".into());
    };
    let report = reopened.abandoned_turn().ok_or("turn_abandoned missing")?;
    assert_eq!(
        report.claim(),
        if not_dispatched {
            crate::AbandonedTurnClaim::NothingStarted
        } else {
            crate::AbandonedTurnClaim::RunningTool
        }
    );
    assert_eq!(
        report.cause(),
        if matches!(
            root_state,
            MixedRoot::Dispatched
                | MixedRoot::Prepared
                | MixedRoot::QueuedSteer
                | MixedRoot::CompletedSteer
        ) {
            crate::AbandonmentCause::ProcessLost
        } else {
            crate::AbandonmentCause::Interrupted
        }
    );
    let head = reopened.checkpoint();
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let history = head.manifest().canonical().history();
    assert_eq!(history.segments().len(), before + 1);
    assert!(
        history
            .segments()
            .into_iter()
            .last()
            .ok_or("note missing")?
            .items()
            .iter()
            .all(|item| item
                .blocks()
                .iter()
                .all(|block| matches!(block, ContentBlock::Text(_))))
    );
    for prior in seeded.checkpoint().manifest().ledger().operations() {
        if let DurableOperationState::Completed(evidence) = prior.state()
            && !matches!(prior.kind(), DurableOperationKind::Command { .. })
        {
            let current = head
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == prior.operation_id())
                .ok_or("sibling missing")?;
            assert_eq!(
                current.state(),
                &DurableOperationState::Finalized(evidence.clone())
            );
        }
    }
    let root = head
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id))
        .ok_or("root command missing")?;
    let terminal = terminal_record_for_command(head.manifest().ledger(), root)?;
    assert_eq!(head.manifest().ledger().operations().into_iter().filter(|op|
        matches!(op.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
        && op.request().bytes() == root.command_component().bytes()
        && matches!(op.state(), DurableOperationState::Finalized(_))).count(), 1);
    if matches!(
        root_state,
        MixedRoot::QueuedSteer | MixedRoot::CompletedSteer
    ) {
        let [ContentBlock::Text(note)] =
            history.segments().iter().last().ok_or("note")?.items()[0].blocks()
        else {
            return Err("note text".into());
        };
        assert_eq!(
            note.text()
                .matches("Queued steering was not applied and is not retried.")
                .count(),
            1
        );
        assert!(
            super::super::turn_recovery::controls::has_unapplied_steering(
                head.manifest().ledger(),
                turn_id,
            )
        );
        let steer = head
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .find(|command| command.command_id() == "fixture:queued-steer")
            .ok_or("steer receipt lost")?;
        assert!(
            head.manifest()
                .ledger()
                .operations()
                .into_iter()
                .any(
                    |op| op.request().bytes() == steer.command_component().bytes()
                        && matches!(op.state(), DurableOperationState::Finalized(evidence)
                            if evidence.bytes() == b"steering-not-applied-turn-ended")
                )
        );
    }

    assert!(if matches!(
        root_state,
        MixedRoot::Dispatched
            | MixedRoot::Prepared
            | MixedRoot::QueuedSteer
            | MixedRoot::CompletedSteer
    ) {
        matches!(terminal.outcome, DurableTerminalOutcome::Cancelled(_))
    } else {
        matches!(
            terminal.outcome,
            DurableTerminalOutcome::RecoveryRequired(_)
        )
    });
    for command in head.manifest().ledger().commands() {
        if command.turn_id() != Some(turn_id) {
            assert!(matches!(
                terminal_record_for_command(head.manifest().ledger(), command)?.outcome,
                DurableTerminalOutcome::Cancelled(_)
            ));
            let operation = head
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|operation| {
                    operation.request().bytes() == command.command_component().bytes()
                })
                .ok_or("queued operation missing")?;
            assert!(
                !operation.transitions().iter().any(|transition| matches!(
                    transition.state(),
                    DurableOperationState::Dispatched
                ))
            );
        }
    }
    assert_eq!(reopening_model.calls.load(Ordering::Acquire), 0);
    let recovered_source =
        DurableSessionAddress::new(source.session_id, source.branch_id, head.id());
    let recovered_history = head.manifest().canonical().history().identity();
    reopened.shutdown().await?;
    let model = artifact_test_model(0, 89_800)?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let DurableRuntimeOpen::Opened(repeated) = sessions
        .open(
            "open:mixed-code-cell-repeat",
            recovered_source,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?
    else {
        return Err("settled Code Cell did not reopen".into());
    };
    assert!(repeated.abandoned_turn().is_none());
    assert_eq!(
        repeated
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .identity(),
        recovered_history
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    repeated.shutdown().await?;
    Ok(())
}

fn assert_crossed_finalization_obligations_are_corrupt(
    checkpoint: &RuntimeCheckpoint,
) -> TestResult {
    let prior = checkpoint.manifest();
    for crossed_kind in [
        RecoveryObligationKind::ProviderAttempt,
        RecoveryObligationKind::Effect,
    ] {
        let mut lifecycle = prior.lifecycle().clone();
        let RuntimeLifecycle::Working { obligations, .. } = &mut lifecycle else {
            return Err("working lifecycle missing".into());
        };
        let obligation = obligations
            .iter_mut()
            .find(|obligation| {
                obligation.action() == RecoveryAction::FinalizationRequired
                    && obligation.kind() != crossed_kind
                    && matches!(
                        obligation.kind(),
                        RecoveryObligationKind::ProviderAttempt | RecoveryObligationKind::Effect
                    )
            })
            .ok_or("completed sibling obligation missing")?;
        *obligation = RecoveryObligation::new(
            obligation.operation_id(),
            RecoveryHandlerSignature::new(crossed_kind, RecoveryAction::FinalizationRequired),
        );
        let corrupt = RuntimeManifest::new(
            prior.provenance().clone(),
            prior.topology(),
            lifecycle,
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
        );
        assert!(matches!(
            corrupt,
            Err(crate::RuntimeManifestError::InvalidLifecycle)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn live_code_cell_with_dispatched_root_reopens_abandoned_without_dispatch() -> TestResult {
    assert_mixed_debt(false, MixedRoot::Dispatched).await
}

/// Explicit Code Cell reconciliation refuses a head whose nested effect is
/// still uncertain. Exact reopen owns that closure; this workflow writes nothing.
#[tokio::test]
async fn explicit_code_cell_abandonment_refuses_an_uncertain_nested_effect() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:uncertain-nested",
            durable_artifact_test_builder(workspace, artifact_test_model(0, 89_760)?),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("uncertain nested effect").await?;
    let turn_id = turn.id();
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let checkpoint = agent.checkpoint();
    let _ = agent.shutdown().await;
    let seeded_address =
        seed_mixed_debt(&store, checkpoint, turn_id, false, MixedRoot::Dispatched).await?;
    let seeded = store
        .load_checkpoint(seeded_address.checkpoint_id)
        .await?
        .checkpoint()
        .clone();
    let records = seeded
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .map(|record| {
            if !matches!(record.kind(), DurableOperationKind::Effect { .. }) {
                return Ok(record.clone());
            }
            let transitions = record.transitions();
            let end = transitions
                .iter()
                .position(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
                .ok_or("effect dispatch missing")?;
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                transitions[..=end].to_vec(),
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert!(records.iter().any(|record| {
        matches!(record.kind(), DurableOperationKind::Effect { .. })
            && matches!(record.state(), DurableOperationState::Dispatched)
    }));
    let prior = seeded.manifest();
    let ledger = RuntimeOperationLedger::new(
        prior.ledger().commands().into_iter().cloned().collect(),
        records,
    )?;
    let lifecycle = working_lifecycle(turn_id, &ledger)?;
    let topology = prior.topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(uuid::Uuid::from_u128(0xc0de_1601))?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let head = store.load_branch(session).await?.head();
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_1602))?;
    let branch_revision = topology
        .branch_revision
        .checked_add(1)
        .ok_or("revision exhausted")?;
    let context = prior.canonical().context().retained_for_exact_resume();
    let history_identity = prior.canonical().history().identity();
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: &lifecycle,
        history_identity,
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: prior.family_artifact(),
    })?;
    let canonical = RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        prior.canonical().history().clone(),
        WorkingProjection::default(),
        context,
    )?;
    let uncertain = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision,
            fork: topology.fork,
            lease_fence: lease.fence().get(),
            commit_identity,
        },
        lifecycle,
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        canonical,
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let cell = code_cell_abandonment_fold(&uncertain)?;
    assert!(cell.turn_abandonment);
    let bundle = RuntimeCheckpointCodec::bundle(&uncertain, None)?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), head, bundle))
        .await?;
    store.release(lease).await?;
    let address =
        DurableSessionAddress::new(topology.session_id, topology.branch_id, checkpoint_id);
    let before = store.load_branch(session).await?.head();
    let commits_before = lock_mutex(&store.state).commits.len();
    let error = sessions
        .abandon_code_cell(DurableCodeCellAbandonmentRequest::new(
            "recover:uncertain-nested",
            address,
        )?)
        .await
        .expect_err("uncertain nested effect must not be explicitly abandoned");
    assert_eq!(error.kind(), AgentErrorKind::OutcomeUnknown);
    assert!(
        error
            .message()
            .contains("mixed finalization debt is abandoned by exact reopen"),
        "{}",
        error.message()
    );
    assert_eq!(store.load_branch(session).await?.head(), before);
    assert_eq!(lock_mutex(&store.state).commits.len(), commits_before);
    assert_eq!(
        store
            .load_checkpoint(checkpoint_id)
            .await?
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .identity(),
        history_identity
    );
    Ok(())
}
