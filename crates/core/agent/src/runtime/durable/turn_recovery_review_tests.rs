//! Review pins for the root-only and uncommitted ordinary-effect cuts.

#[path = "turn_recovery/provider_attempt_tests.rs"]
mod provider_attempt_tests;

use super::super::tests::SessionUpdateFixtureMode;
use super::super::tests::landed_round_abandonment::{
    completed_tool_round_cut, root_without_provider_cut,
};
use super::*;
use crate::runtime::terminal::decode_terminal_result_record;
use plakous_model::{AttemptId, CanonicalHistory};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub(in crate::runtime::durable) fn checkpoint_with_review_state(
    source: &RuntimeCheckpoint,
    records: Vec<DurableOperationRecord>,
    history: CanonicalHistory,
    reverse_obligations: bool,
) -> TestResult<RuntimeCheckpoint> {
    checkpoint_with_review_commands(
        source,
        source
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .cloned()
            .collect(),
        records,
        history,
        reverse_obligations,
    )
}

pub(super) fn checkpoint_with_review_commands(
    source: &RuntimeCheckpoint,
    commands: Vec<DurableCommandRecord>,
    records: Vec<DurableOperationRecord>,
    history: CanonicalHistory,
    reverse_obligations: bool,
) -> TestResult<RuntimeCheckpoint> {
    let prior = source.manifest();
    let RuntimeLifecycle::Working { root, .. } = prior.lifecycle() else {
        return Err("working root".into());
    };
    let RuntimeWorkingRoot::Turn { turn_id, .. } = root else {
        return Err("turn root".into());
    };
    let ledger = RuntimeOperationLedger::new(commands, records)?;
    let mut lifecycle = working_lifecycle(*turn_id, &ledger)?;
    if let RuntimeLifecycle::Working {
        root: working_root, ..
    } = &mut lifecycle
    {
        *working_root = *root;
    }
    if reverse_obligations {
        let RuntimeLifecycle::Working { obligations, .. } = &mut lifecycle else {
            return Err("working obligations".into());
        };
        obligations.reverse();
    }
    let canonical = RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        history,
        prior.canonical().working().clone(),
        prior.canonical().context().clone(),
    )?;
    let mut topology = prior.topology();
    topology.commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id: topology.checkpoint_id,
        parent_checkpoint_id: topology.parent_checkpoint_id.ok_or("parent")?,
        branch_revision: topology.branch_revision,
        lifecycle: &lifecycle,
        history_identity: canonical.history().identity(),
        ledger: &ledger,
        context_identity: canonical.context().identity(),
        family_artifact: prior.family_artifact(),
    })?;
    Ok(RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        topology,
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
    )?)?)
}

fn records(source: &RuntimeCheckpoint) -> Vec<DurableOperationRecord> {
    source
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect()
}

fn note_and_message(source: &RuntimeCheckpoint) -> TestResult<(String, String)> {
    let fold = abandoned_turn_recovery_fold(source)?;
    let operation = fold.root_operation;
    let claim = abandoned_turn_claim(&fold);
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_999))?;
    let closure = abandonment_closure_state(source, fold, claim, checkpoint_id)?;
    let note = closure
        .history
        .segments()
        .iter()
        .last()
        .ok_or("note")?
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text().to_owned()),
            _ => None,
        })
        .ok_or("text")?;
    let root = closure
        .ledger
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == operation.0)
        .ok_or("root")?;
    let DurableOperationState::Finalized(evidence) = root.state() else {
        return Err("finalized root".into());
    };
    let terminal = decode_terminal_result_record(evidence.bytes()).ok_or("terminal")?;
    let DurableTerminalOutcome::Cancelled(error) = terminal.outcome else {
        return Err("cancelled terminal".into());
    };
    Ok((note, error.into_error()?.message().to_owned()))
}

#[tokio::test]
async fn recovery_review_root_only_note_and_terminal_are_exact() -> TestResult {
    for prepared in [false, true] {
        let cut = root_without_provider_cut(prepared).await?;
        let (note, message) = note_and_message(&cut)?;
        assert_eq!(
            note,
            "this turn was abandoned after its process was lost, before any model request \
            or tool started. Nothing ran, no result was inferred, and the prompt is not retried."
        );
        assert_eq!(
            message,
            "the turn was abandoned after the process was lost, before any work started"
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovery_recorded_terminal_refuses_foreign_sibling_debt() -> TestResult {
    let cut = root_without_provider_cut(false).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    let id = CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_994))?;
    let closed = abandonment_closure_state(&cut, fold, AbandonedTurnClaim::NothingStarted, id)?;
    let root = closed.ledger.operations()[0].clone();
    let foreign = DurableOperationRecord::new(
        root.operation_id() + 1,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(TurnId::try_from(uuid::Uuid::from_u128(0xE8C_995))?),
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_996))?,
        },
        abandoned_operation_evidence()?,
        vec![
            DurableOperationTransition::new(cut.id(), DurableOperationState::Prepared),
            DurableOperationTransition::new(id, DurableOperationState::Dispatched),
        ],
    )?;
    let error = checkpoint_with_review_state(&cut, vec![root, foreign], closed.history, false)
        .err()
        .ok_or("foreign debt admitted")?;
    assert!(error.to_string().contains("lifecycle"));
    Ok(())
}

#[tokio::test]
async fn recovery_recorded_terminal_uncertain_evidence_rejects_crossed_commit_and_restore()
-> TestResult {
    let cut = root_without_provider_cut(false).await?;
    let root = cut.manifest().ledger().operations()[0];
    let turn_id = operation_turn_id(root).ok_or("turn")?;
    let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_996))?;
    for kind in [
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(turn_id),
            attempt_id,
        },
        DurableOperationKind::Commit {
            root: ModelOperationRoot::Turn(turn_id),
        },
    ] {
        let record = DurableOperationRecord::new(
            root.operation_id() + 1,
            kind,
            abandoned_operation_evidence()?,
            root.transitions().to_vec(),
        )?;
        for (kind, action) in [
            (
                RecoveryObligationKind::Effect,
                RecoveryAction::ProviderEvidenceRequired,
            ),
            (
                RecoveryObligationKind::Commit,
                RecoveryAction::CommitRecovery,
            ),
            (
                RecoveryObligationKind::HostedContinuation,
                RecoveryAction::RestoreToolBatch,
            ),
            (
                RecoveryObligationKind::HostedContinuation,
                RecoveryAction::RestoreServerPause,
            ),
            (
                RecoveryObligationKind::EffectDecision,
                RecoveryAction::RestoreEffectDecision,
            ),
        ] {
            let obligation = RecoveryObligation::new(
                record.operation_id(),
                crate::RecoveryHandlerSignature::new(kind, action),
            );
            assert!(terminal_siblings::uncertain_evidence(&record, &obligation)?.is_none());
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_review_prepared_root_with_same_turn_siblings_refuses() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, true).await?;
    let mut changed = records(&cut);
    let root = changed
        .iter_mut()
        .find(|record| matches!(record.kind(), DurableOperationKind::Command { .. }))
        .ok_or("root")?;
    *root = DurableOperationRecord::new(
        root.operation_id(),
        root.kind().clone(),
        root.request().clone(),
        root.transitions()[..1].to_vec(),
    )?;
    changed.retain(|record| {
        !matches!(
            record.kind(),
            DurableOperationKind::Effect { .. } | DurableOperationKind::ProcessSession { .. }
        )
    });
    let provider = changed
        .iter_mut()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    *provider = DurableOperationRecord::new(
        provider.operation_id(),
        provider.kind().clone(),
        provider.request().clone(),
        provider.transitions()[..1].to_vec(),
    )?;
    let changed = checkpoint_with_review_state(
        &cut,
        changed,
        cut.manifest().canonical().history().clone(),
        false,
    )?;
    let error = abandoned_turn_recovery_fold(&changed)
        .err()
        .ok_or("must refuse")?;
    assert_eq!(error.kind(), AgentErrorKind::OutcomeUnknown);
    Ok(())
}

#[tokio::test]
async fn recovery_review_completed_effect_requires_its_provider_attempt_and_turn() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, true).await?;
    for foreign_provider in [false, true] {
        let mut changed = records(&cut);
        let record = changed
            .iter_mut()
            .find(|record| {
                if foreign_provider {
                    matches!(record.kind(), DurableOperationKind::Provider { .. })
                } else {
                    matches!(record.kind(), DurableOperationKind::Effect { .. })
                }
            })
            .ok_or("record")?;
        let mut kind = record.kind().clone();
        match &mut kind {
            DurableOperationKind::Effect { attempt_id, .. } => {
                *attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_997))?;
            }
            DurableOperationKind::Provider { root, .. } => {
                *root =
                    ModelOperationRoot::Turn(TurnId::try_from(uuid::Uuid::from_u128(0xE8C_998))?);
            }
            _ => return Err("effect or provider".into()),
        }
        *record = DurableOperationRecord::new(
            record.operation_id(),
            kind,
            record.request().clone(),
            record.transitions().to_vec(),
        )?;
        let changed = checkpoint_with_review_state(
            &cut,
            changed,
            cut.manifest().canonical().history().clone(),
            false,
        );
        if foreign_provider {
            let error = changed
                .err()
                .ok_or("must refuse foreign-turn provider debt")?;
            let error = error
                .downcast_ref::<AgentError>()
                .ok_or("typed root refusal")?;
            assert_eq!(error.kind(), AgentErrorKind::Internal);
            assert_eq!(
                error.message(),
                "durable working set mixes logical operation roots"
            );
            continue;
        }
        let changed = changed?;
        let error = abandoned_turn_recovery_fold(&changed)
            .err()
            .ok_or("must refuse")?;
        assert_eq!(error.kind(), AgentErrorKind::OutcomeUnknown);
    }
    Ok(())
}

#[tokio::test]
async fn recovery_review_completed_effect_is_independent_of_obligation_order() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, true).await?;
    let reversed = checkpoint_with_review_state(
        &cut,
        records(&cut),
        cut.manifest().canonical().history().clone(),
        true,
    )?;
    let normal = abandoned_turn_recovery_fold(&cut)?;
    let reversed = abandoned_turn_recovery_fold(&reversed)?;
    assert_eq!(
        abandoned_turn_claim(&normal),
        abandoned_turn_claim(&reversed)
    );
    assert_eq!(normal.root_operation, reversed.root_operation);
    assert_eq!(
        normal.uncommitted_effect_operations,
        reversed.uncommitted_effect_operations
    );
    Ok(())
}

#[tokio::test]
async fn recovery_review_completed_effect_note_and_terminal_are_exact() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, true).await?;
    let (note, message) = note_and_message(&cut)?;
    assert_eq!(
        note,
        "this turn was abandoned after a tool finished, but its result was not added \
        to the conversation. It may have changed the workspace and is not retried. \
        Check the workspace before relying on what came before this point."
    );
    assert_eq!(
        message,
        "the turn was abandoned after a tool finished, but its result was not added to the conversation; it may have changed the workspace and is not retried"
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "matrix pins all recorded root states, sibling kinds and exact/uncertain evidence"
)]
async fn recovery_recorded_terminal_settles_uncertain_siblings_without_replacing_history()
-> TestResult {
    let cut = root_without_provider_cut(false).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    let id = CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_991))?;
    let closed = abandonment_closure_state(&cut, fold, AbandonedTurnClaim::NothingStarted, id)?;
    let finalized = closed.ledger.operations()[0];
    for (root_finalized, root_not_dispatched) in [(false, false), (false, true), (true, false)] {
        let terminal = match finalized.state() {
            DurableOperationState::Finalized(evidence) => {
                decode_terminal_result_record(evidence.bytes()).ok_or("terminal")?
            }
            _ => return Err("finalized terminal".into()),
        };
        let pending = runtime_component(
            RuntimeComponentKind::Operation,
            encode_pending_terminal_record(PendingTerminalResultRecord {
                remedy: None,
                version: TERMINAL_RESULT_RECORD_VERSION,
                command_id: terminal.command_id,
                turn_id: terminal.turn_id,
                operation_id: terminal.operation_id,
                outcome: terminal.outcome,
                usage: terminal.usage,
            })?,
        )?;
        let mut transitions: Vec<_> = finalized
            .transitions()
            .iter()
            .map(|transition| {
                if !root_finalized
                    && matches!(transition.state(), DurableOperationState::Finalized(_))
                {
                    DurableOperationTransition::new(
                        transition.checkpoint_id(),
                        DurableOperationState::Completed(pending.clone()),
                    )
                } else {
                    transition.clone()
                }
            })
            .collect();
        if root_not_dispatched {
            transitions.truncate(1);
            transitions.push(DurableOperationTransition::new(
                id,
                DurableOperationState::ProvenNotDispatched(pending.clone()),
            ));
        }
        let root = DurableOperationRecord::new(
            finalized.operation_id(),
            finalized.kind().clone(),
            finalized.request().clone(),
            transitions,
        )?;
        for unknown in [false, true] {
            for sibling_kind in 0..3 {
                // A process cannot stop at OutcomeUnknown: that state and its
                // finalization are committed atomically by recovery.
                if unknown && sibling_kind == 2 {
                    continue;
                }
                for control_transition_count in 0..=4 {
                    let mut transitions = vec![
                        DurableOperationTransition::new(cut.id(), DurableOperationState::Prepared),
                        DurableOperationTransition::new(id, DurableOperationState::Dispatched),
                    ];
                    let request = runtime_component(
                        RuntimeComponentKind::Operation,
                        b"sibling request".to_vec(),
                    )?;
                    // The evidence the live driver writes when an attempt's
                    // outcome is never observed.
                    let unobserved =
                        AgentError::new(AgentErrorKind::OutcomeUnknown, "outcome never observed");
                    let evidence = if sibling_kind == 0 {
                        crate::runtime::operation_record::test_support::outcome_component(Err(
                            &unobserved,
                        ))?
                    } else {
                        crate::runtime::operation_record::completed_tool_outcome_component(
                            &Err(unobserved),
                            EntryId::try_from(uuid::Uuid::from_u128(0xE8C_997))?,
                        )?
                    };
                    if unknown {
                        transitions.push(DurableOperationTransition::new(
                            CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_990))?,
                            DurableOperationState::OutcomeUnknown(evidence.clone()),
                        ));
                    }
                    let kind = match sibling_kind {
                        0 => DurableOperationKind::Provider {
                            root: ModelOperationRoot::Turn(closed.turn_id),
                            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_992))?,
                        },
                        1 => DurableOperationKind::Effect {
                            turn_id: closed.turn_id,
                            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_992))?,
                            call_id: plakous_model::CallId::try_from(uuid::Uuid::from_u128(
                                0xE8C_993,
                            ))?,
                            source_index: 0,
                        },
                        _ => DurableOperationKind::ProcessSession {
                            turn_id: closed.turn_id,
                            session_id: crate::manifest::ProcessSessionId::new([0x94; 16])?,
                            owner_generation: crate::manifest::ProcessSessionOwnerGeneration::new(
                                1,
                            )?,
                        },
                    };
                    let sibling = DurableOperationRecord::new(
                        root.operation_id() + 1,
                        kind,
                        request.clone(),
                        transitions,
                    )?;
                    let exact_record = |evidence: &ReconstructiveComponent| -> TestResult<_> {
                        Ok(DurableOperationRecord::new(
                            root.operation_id() + 2,
                            DurableOperationKind::Provider {
                                root: ModelOperationRoot::Turn(closed.turn_id),
                                attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_995))?,
                            },
                            request.clone(),
                            vec![
                                DurableOperationTransition::new(
                                    cut.id(),
                                    DurableOperationState::Prepared,
                                ),
                                DurableOperationTransition::new(
                                    id,
                                    DurableOperationState::Dispatched,
                                ),
                                DurableOperationTransition::new(
                                    CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_996))?,
                                    DurableOperationState::Completed(evidence.clone()),
                                ),
                            ],
                        )?)
                    };
                    let exact_evidence =
                        crate::runtime::operation_record::test_support::outcome_component(Err(
                            &AgentError::from_model(plakous_model::ModelError::new(
                                plakous_model::ModelErrorKind::Protocol,
                                "exact sibling failed",
                            )),
                        ))?;
                    let exact = exact_record(&exact_evidence)?;
                    let with_controls = |records: Vec<DurableOperationRecord>| {
                        let changed = checkpoint_with_review_state(
                            &cut,
                            records,
                            closed.history.clone(),
                            false,
                        )?;
                        if control_transition_count == 0 {
                            return Ok::<_, Box<dyn std::error::Error>>(changed);
                        }
                        controls::tests::with_control(
                            &changed,
                            crate::runtime::DurableActorCommandEffect::SteerQueued { position: 1 },
                            control_transition_count,
                        )
                    };
                    // Epoch 25 has no legacy provider records: an untagged
                    // record is corrupt in both folds, never "no response".
                    let legacy = with_controls(vec![
                        root.clone(),
                        sibling.clone(),
                        exact_record(&runtime_component(
                            RuntimeComponentKind::Operation,
                            b"exact completed sibling evidence".to_vec(),
                        )?)?,
                    ])?;
                    for refusal in [
                        terminal_evidence_fold(&legacy).err(),
                        abandonment_evidence_fold(&legacy).err(),
                        local_terminal_recovery_fold(&legacy).err(),
                    ] {
                        assert_eq!(
                            refusal
                                .ok_or("a legacy provider record must refuse")?
                                .kind(),
                            AgentErrorKind::Session
                        );
                    }
                    let changed =
                        with_controls(vec![root.clone(), sibling.clone(), exact.clone()])?;
                    assert_eq!(
                        terminal_evidence_fold(&changed)
                            .err()
                            .ok_or("uncertain siblings are outside exact finalization")?
                            .kind(),
                        AgentErrorKind::OutcomeUnknown
                    );
                    assert_eq!(
                        abandonment_evidence_fold(&changed)
                            .err()
                            .ok_or("a recorded terminal has nothing to abandon")?
                            .kind(),
                        AgentErrorKind::OutcomeUnknown
                    );
                    let fold = local_terminal_recovery_fold(&changed).map_err(|error| {
                        format!(
                            "finalized={root_finalized}, unknown={unknown}: {}",
                            error.message()
                        )
                    })?;
                    assert!(fold.synthesized.is_none());
                    let settled = terminal_closure_state(
                        &changed,
                        fold,
                        false,
                        CheckpointId::try_from(uuid::Uuid::from_u128(0xE8C_989))?,
                    )?;
                    assert_eq!(settled.history, closed.history);
                    assert!(matches!(
                        working_lifecycle(closed.turn_id, &settled.ledger)?,
                        RuntimeLifecycle::Quiescent
                    ));
                    let settled_root = settled
                        .ledger
                        .operations()
                        .into_iter()
                        .find(|record| record.operation_id() == root.operation_id())
                        .ok_or("root")?;
                    let DurableOperationState::Finalized(actual) = settled_root.state() else {
                        return Err("terminal".into());
                    };
                    let DurableOperationState::Finalized(expected) = finalized.state() else {
                        return Err("expected terminal".into());
                    };
                    assert_eq!(
                        decode_terminal_result_record(actual.bytes())
                            .ok_or("actual")?
                            .outcome,
                        decode_terminal_result_record(expected.bytes())
                            .ok_or("expected")?
                            .outcome
                    );
                    let settled_sibling = settled
                        .ledger
                        .operations()
                        .into_iter()
                        .find(|record| record.operation_id() == sibling.operation_id())
                        .ok_or("sibling")?;
                    assert!(
                        settled_sibling
                            .transitions()
                            .iter()
                            .any(|transition| matches!(
                                transition.state(),
                                DurableOperationState::OutcomeUnknown(_)
                            ))
                    );
                    let expected = if unknown {
                        evidence.clone()
                    } else {
                        match sibling_kind {
                            0 => abandoned_operation_evidence()?,
                            1 => abandoned_effect_evidence()?,
                            _ => abandoned_process_evidence()?,
                        }
                    };
                    assert_eq!(
                        settled_sibling.state(),
                        &DurableOperationState::Finalized(expected)
                    );
                    let settled_exact = settled
                        .ledger
                        .operations()
                        .into_iter()
                        .find(|record| record.operation_id() == exact.operation_id())
                        .ok_or("exact sibling")?;
                    assert_eq!(
                        settled_exact.state(),
                        &DurableOperationState::Finalized(exact_evidence)
                    );
                }
            }
        }
    }
    Ok(())
}
