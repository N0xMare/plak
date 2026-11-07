use super::*;
use crate::runtime::continuation::runtime_component;
use crate::runtime::operation_record::{
    ProviderAttemptResult, ProviderOutcome, RoundDecision, TurnBudgets,
};
use crate::runtime::terminal::DurableAgentError;
use crate::{DurableOperationTransition, RuntimeComponentKind};
use plakous_model::{
    AttemptId, CallId, CheckpointId, Digest, DigestDomain, FinishDetails, FinishReason,
    ModelResponse, Usage,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn checkpoint(value: u64) -> TestResult<CheckpointId> {
    Ok(CheckpointId::try_from(uuid::Uuid::from_u128(
        u128::from(value) + 0x10000,
    ))?)
}

pub(in crate::runtime) fn retirement_fixture(
    leaves: u64,
    decision: RoundDecision,
) -> TestResult<(RuntimeOperationLedger, TurnId)> {
    let turn = TurnId::try_from(uuid::Uuid::from_u128(0x99))?;
    let command = super::super::tests::root_command(1, turn)?;
    let root = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn),
        },
        runtime_component(
            RuntimeComponentKind::Operation,
            command.command_component().bytes().to_vec(),
        )?,
        vec![
            DurableOperationTransition::new(checkpoint(1)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(checkpoint(2)?, DurableOperationState::Dispatched),
        ],
    )?;
    let mut records = vec![root];
    for id in 2..=leaves + 1 {
        records.push(leaf(id, turn, true)?);
    }
    records.push(retirement_round(leaves + 2, turn, decision)?);
    Ok((RuntimeOperationLedger::new(vec![command], records)?, turn))
}

fn leaf(id: u64, turn: TurnId, finalized: bool) -> TestResult<DurableOperationRecord> {
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        b"raw tool outcome".to_vec(),
    )?;
    let mut transitions = vec![
        DurableOperationTransition::new(checkpoint(1)?, DurableOperationState::Prepared),
        DurableOperationTransition::new(checkpoint(2)?, DurableOperationState::Dispatched),
        DurableOperationTransition::new(
            checkpoint(3)?,
            DurableOperationState::Completed(evidence.clone()),
        ),
    ];
    if finalized {
        transitions.push(DurableOperationTransition::new(
            checkpoint(4)?,
            DurableOperationState::Finalized(evidence.clone()),
        ));
    }
    Ok(DurableOperationRecord::new(
        id,
        DurableOperationKind::Effect {
            turn_id: turn,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0x100))?,
            call_id: CallId::try_from(uuid::Uuid::from_u128(u128::from(id) + 0x10000))?,
            source_index: 0,
        },
        evidence,
        transitions,
    )?)
}

pub(in crate::runtime) fn retirement_leaf(
    id: u64,
    turn: TurnId,
) -> TestResult<DurableOperationRecord> {
    leaf(id, turn, true)
}

pub(in crate::runtime) fn retirement_round(
    id: u64,
    turn: TurnId,
    decision: RoundDecision,
) -> TestResult<DurableOperationRecord> {
    let plan = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"retirement fixture"])?;
    let result = if matches!(
        decision,
        RoundDecision::RetryOutputLimit | RoundDecision::OverflowFold
    ) {
        ProviderAttemptResult::Failure(DurableAgentError::from_error(&AgentError::new(
            AgentErrorKind::Model,
            "refused round",
        ))?)
    } else {
        ProviderAttemptResult::Response(ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Stop, None),
            Some(Usage::new(5, 1, 0, 0, 0)),
            None,
            plan,
        ))
    };
    let outcome = ProviderOutcome {
        result,
        usage: Some(Usage::new(5, 1, 0, 0, 0)),
        decision,
        budgets: TurnBudgets::default(),
        steering: Vec::new(),
    }
    .component()?;
    Ok(DurableOperationRecord::new(
        id,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(turn),
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(u128::from(id) + 0x20000))?,
        },
        runtime_component(RuntimeComponentKind::Operation, b"fixture request".to_vec())?,
        vec![
            DurableOperationTransition::new(checkpoint(1)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(checkpoint(2)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                checkpoint(3)?,
                DurableOperationState::Completed(outcome.clone()),
            ),
            DurableOperationTransition::new(
                checkpoint(4)?,
                DurableOperationState::Finalized(outcome),
            ),
        ],
    )?)
}

#[test]
fn retirement_is_p_anchored_counts_dispatches_and_preserves_completed_raw_bytes() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let records = ledger
        .operations()
        .into_iter()
        .map(|record| {
            if record.operation_id() == 2 {
                leaf(2, turn, false)
            } else {
                Ok(record.clone())
            }
        })
        .collect::<TestResult<Vec<_>>>()?;
    let ledger =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let ledger = ledger
        .append_operation(leaf(103, turn, true)?)?
        .append_operation(leaf(104, turn, false)?)?;
    let retained = retire_at(&ledger, turn, &BTreeSet::new(), 16)?;
    assert!(retained.operation_count() <= 12);
    assert_eq!(retained.operation(1), ledger.operation(1));
    for id in [2, 102, 103, 104] {
        assert_eq!(retained.operation(id), ledger.operation(id));
    }
    assert_eq!(
        retained.retired_attempts(turn),
        u64::try_from(ledger.operation_count() - retained.operation_count())?
    );
    assert!(retained.retired_attempts(turn) > 0);
    validate_live_retirement(&ledger, &retained, turn)?;
    for count in [
        0,
        retained.retired_attempts(turn) - 1,
        retained.retired_attempts(turn) + 1,
    ] {
        let changed = retained
            .clone()
            .with_retired_attempts((count != 0).then_some((turn, count)))?;
        assert!(validate_live_retirement(&ledger, &changed, turn).is_err());
    }
    Ok(())
}

#[test]
fn retirement_needs_p_obeys_hysteresis_and_overflow_is_atomic() -> TestResult {
    let (ledger, turn) = retirement_fixture(10, RoundDecision::Continue)?;
    assert_eq!(retire_at(&ledger, turn, &BTreeSet::new(), 16)?, ledger);
    let no_provider = RuntimeOperationLedger::new(
        ledger.commands().into_iter().cloned().collect(),
        ledger
            .operations()
            .into_iter()
            .filter(|record| record.operation_id() != 12)
            .cloned()
            .collect(),
    )?;
    assert_eq!(
        retire_at(&no_provider, turn, &BTreeSet::new(), 8)?,
        no_provider
    );
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let overflow = ledger.with_retired_attempts(Some((turn, u64::MAX)))?;
    assert!(retire_at(&overflow, turn, &BTreeSet::new(), 16).is_err());
    assert_eq!(overflow.retired_attempts(turn), u64::MAX);
    assert_eq!(overflow.operation_count(), 102);
    Ok(())
}

#[test]
fn retirement_emergency_reclaims_one_record_without_waiting_for_a_full_sweep() -> TestResult {
    for (leaves, expected_removed) in [(21, 0), (22, 1)] {
        let (ledger, turn) = retirement_fixture(leaves, RoundDecision::Continue)?;
        let records = ledger
            .operations()
            .into_iter()
            .map(|record| {
                if matches!(record.kind(), DurableOperationKind::Effect { .. }) {
                    leaf(record.operation_id(), turn, record.operation_id() == 2)
                } else {
                    Ok(record.clone())
                }
            })
            .collect::<TestResult<Vec<_>>>()?;
        let source =
            RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
        let retained = retire_at(&source, turn, &BTreeSet::new(), 16)?;
        assert_eq!(
            source.operation_count() - retained.operation_count(),
            expected_removed
        );
        assert_eq!(
            retained.retired_attempts(turn),
            u64::try_from(expected_removed)?
        );
        for id in 3..=leaves + 1 {
            assert_eq!(retained.operation(id), source.operation(id));
        }
    }
    Ok(())
}

#[test]
fn staged_finalizations_contribute_once_and_proof_is_not_persisted() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let mut records = ledger.operations().into_iter().cloned().collect::<Vec<_>>();
    records[1] = leaf(2, turn, false)?;
    let prior =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let staged = prior.transition_operation(
        2,
        DurableOperationTransition::new(
            checkpoint(5)?,
            DurableOperationState::Finalized(runtime_component(
                RuntimeComponentKind::Operation,
                b"promoted outcome".to_vec(),
            )?),
        ),
    )?;
    let retained = retire_at(&staged, turn, &BTreeSet::new(), 16)?;
    assert!(retained.operation(2).is_none());
    assert!(matches!(
        retained
            .retirement_source()
            .and_then(|source| source.operation(2))
            .map(DurableOperationRecord::state),
        Some(DurableOperationState::Finalized(_))
    ));
    assert_eq!(
        retained.retired_attempts(turn),
        u64::try_from(staged.operation_count() - retained.operation_count())?
    );
    assert!(
        retained
            .without_retirement_source()
            .retirement_source()
            .is_none()
    );
    Ok(())
}

#[test]
fn retirement_bounds_1024_rounds_with_a_successful_fold_every_round() -> TestResult {
    use crate::runtime::compaction::instruction::{CompactionInstruction, retirement_fold_pair};
    let (ledger, turn) = retirement_fixture(0, RoundDecision::Continue)?;
    let mut ledger = RuntimeOperationLedger::new(
        ledger.commands().into_iter().cloned().collect(),
        vec![ledger.operation(1).ok_or("root")?.clone()],
    )?;
    let mut next = 2;
    for round in 0..1_024 {
        for _ in 0..2 {
            ledger = ledger.append_operation(leaf(next, turn, true)?)?;
            next += 1;
        }
        ledger = ledger.append_operation(retirement_round(next, turn, RoundDecision::Continue)?)?;
        next += 1;
        for record in retirement_fold_pair(next, true)? {
            ledger = ledger.append_operation(record)?;
        }
        next += 2;
        let before = CompactionInstruction::for_ledger(&ledger);
        ledger =
            retire_at(&ledger, turn, &BTreeSet::new(), HIGH_WATER)?.without_retirement_source();
        assert_eq!(
            matches!(before, CompactionInstruction::Tight),
            matches!(
                CompactionInstruction::for_ledger(&ledger),
                CompactionInstruction::Tight
            )
        );
        assert!(
            ledger.operation_count() < HIGH_WATER + 5,
            "round {round}: {}",
            ledger.operation_count()
        );
        assert_eq!(ledger.operations().iter().filter(|record| matches!(record.kind(), DurableOperationKind::Provider { root: ModelOperationRoot::Turn(root), .. } if *root == turn)).count(), round + 1);
    }
    assert!(ledger.retired_attempts(turn) > 0);
    assert_eq!(
        ledger.retired_attempts(turn)
            + u64::try_from(
                ledger
                    .operations()
                    .iter()
                    .filter(|record| matches!(record.kind(), DurableOperationKind::Effect { .. }))
                    .count()
            )?,
        2_048
    );
    // The wrap-up and two refunded requests do not spend another model round.
    for decision in [
        RoundDecision::RetryOutputLimit,
        RoundDecision::OverflowFold,
        RoundDecision::Continue,
    ] {
        ledger = ledger.append_operation(retirement_round(next, turn, decision)?)?;
        next += 1;
    }
    ledger = retire_at(&ledger, turn, &BTreeSet::new(), HIGH_WATER)?;
    assert!(ledger.operation_count() < HIGH_WATER + 8);
    // The irreducible provider/root/fold anchor envelope, excluding closed debt.
    let rounds = ledger
        .operations()
        .iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(rounds, 1_027);
    assert!(rounds + 1 + 2 <= 1_297);
    Ok(())
}

#[test]
fn retirement_keeps_n_spanning_process_sessions_even_above_the_target() -> TestResult {
    let (ledger, turn) = retirement_fixture(128, RoundDecision::Continue)?;
    let records = ledger
        .operations()
        .into_iter()
        .map(|record| {
            if !matches!(record.kind(), DurableOperationKind::Effect { .. }) {
                return Ok(record.clone());
            }
            let mut identity = [0; crate::manifest::PROCESS_SESSION_ID_BYTES];
            identity[..8].copy_from_slice(&record.operation_id().to_be_bytes());
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                DurableOperationKind::ProcessSession {
                    turn_id: turn,
                    session_id: crate::manifest::ProcessSessionId::new(identity)?,
                    owner_generation: crate::manifest::ProcessSessionOwnerGeneration::new(1)?,
                },
                record.request().clone(),
                record.transitions().to_vec(),
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let source =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let retained = retire_at(&source, turn, &BTreeSet::new(), 16)?;
    assert_eq!(retained, source);
    assert_eq!(
        retained
            .operations()
            .iter()
            .filter(|record| matches!(record.kind(), DurableOperationKind::ProcessSession { .. }))
            .count(),
        128
    );
    assert_eq!(retained.retired_attempts(turn), 0);
    Ok(())
}

#[test]
fn retirement_does_not_hide_an_obsolete_malformed_continuation() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let records = ledger
        .operations()
        .into_iter()
        .map(|record| {
            if record.operation_id() != 2 {
                return Ok(record.clone());
            }
            Ok(DurableOperationRecord::new(
                2,
                DurableOperationKind::ToolBatchContinuation {
                    turn_id: turn,
                    attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0x111))?,
                },
                record.request().clone(),
                vec![
                    DurableOperationTransition::new(
                        checkpoint(1)?,
                        DurableOperationState::ContinuationRequired(record.request().clone()),
                    ),
                    DurableOperationTransition::new(
                        checkpoint(2)?,
                        DurableOperationState::Finalized(record.request().clone()),
                    ),
                ],
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let ledger =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    assert!(retire_at(&ledger, turn, &BTreeSet::new(), 16).is_err());
    assert_eq!(ledger.operation_count(), 102);
    Ok(())
}

#[test]
fn retirement_pins_prior_response_failure_fence_and_every_live_round() -> TestResult {
    use plakous_model::{Actor, ContentBlock, ConversationItem, EntryId, Provenance, TextBlock};
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let foreign = TurnId::try_from(uuid::Uuid::from_u128(0x888))?;
    let response = ModelResponse::new(
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(0x889))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new(
                "previous response",
                None,
            )?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"previous"])?,
    );
    let evidence = ProviderOutcome {
        result: ProviderAttemptResult::Response(response),
        usage: None,
        decision: RoundDecision::Continue,
        budgets: TurnBudgets::default(),
        steering: Vec::new(),
    }
    .component()?;
    let records = ledger
        .operations()
        .into_iter()
        .map(|record| match record.operation_id() {
            2 => {
                let provider = retirement_round(2, foreign, RoundDecision::Continue)?;
                Ok(DurableOperationRecord::new(
                    2,
                    provider.kind().clone(),
                    provider.request().clone(),
                    vec![
                        DurableOperationTransition::new(
                            checkpoint(1)?,
                            DurableOperationState::Prepared,
                        ),
                        DurableOperationTransition::new(
                            checkpoint(2)?,
                            DurableOperationState::Dispatched,
                        ),
                        DurableOperationTransition::new(
                            checkpoint(3)?,
                            DurableOperationState::Completed(evidence.clone()),
                        ),
                        DurableOperationTransition::new(
                            checkpoint(4)?,
                            DurableOperationState::Finalized(evidence.clone()),
                        ),
                    ],
                )?)
            }
            3 => retirement_round(3, turn, RoundDecision::Continue),
            4 => retirement_round(4, foreign, RoundDecision::RetryOutputLimit),
            102 => Ok(DurableOperationRecord::new(
                102,
                record.kind().clone(),
                record.request().clone(),
                record.transitions()[..2].to_vec(),
            )?),
            _ => Ok(record.clone()),
        })
        .collect::<TestResult<Vec<_>>>()?;
    let source =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let retained = retire_at(&source, turn, &BTreeSet::new(), 16)?;
    assert!(retained.operation_count() < source.operation_count());
    for id in [1, 2, 3, 4, 102] {
        assert_eq!(retained.operation(id), source.operation(id), "anchor {id}");
    }
    Ok(())
}

#[test]
fn retirement_pins_three_failed_pairs_latest_success_and_distinct_harness_pair() -> TestResult {
    use crate::runtime::compaction::instruction::retirement_fold_pair;
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let mut records = ledger
        .operations()
        .into_iter()
        .filter(|record| !(2..=13).contains(&record.operation_id()))
        .cloned()
        .collect::<Vec<_>>();
    for (id, successful) in [(2, true), (6, true), (8, false), (10, false), (12, false)] {
        for record in retirement_fold_pair(id, successful)? {
            let mut request: serde_json::Value = serde_json::from_slice(record.request().bytes())?;
            if id == 6 {
                request["version"] = serde_json::json!(3);
            }
            let failure = runtime_component(
                RuntimeComponentKind::Operation,
                b"failed paid fold".to_vec(),
            )?;
            let transitions = record
                .transitions()
                .iter()
                .map(|transition| {
                    DurableOperationTransition::new(
                        transition.checkpoint_id(),
                        match transition.state() {
                            DurableOperationState::Completed(_) if !successful => {
                                DurableOperationState::Completed(failure.clone())
                            }
                            DurableOperationState::Finalized(_) if !successful => {
                                DurableOperationState::Finalized(failure.clone())
                            }
                            state => state.clone(),
                        },
                    )
                })
                .collect();
            records.push(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&request)?,
                )?,
                transitions,
            )?);
        }
    }
    records.sort_by_key(DurableOperationRecord::operation_id);
    let source =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let retained = retire_at(&source, turn, &BTreeSet::new(), 16)?;
    assert!(retained.operation_count() < source.operation_count());
    for id in [2, 3, 6, 7, 8, 9, 10, 11, 12, 13] {
        assert_eq!(
            retained.operation(id),
            source.operation(id),
            "fold anchor {id}"
        );
    }
    Ok(())
}

#[test]
fn retirement_keeps_a_yielded_locator_bound_group_and_its_below_p_children() -> TestResult {
    let (ledger, turn) = retirement_fixture(200, RoundDecision::Continue)?;
    let group = crate::manifest::composition_graph_records()?;
    let mut records = ledger
        .operations()
        .into_iter()
        .filter(|record| !(101..=103).contains(&record.operation_id()))
        .cloned()
        .collect::<Vec<_>>();
    for record in group {
        let mut kind = record.kind().clone();
        match &mut kind {
            DurableOperationKind::CompositionEvaluation { turn_id, .. }
            | DurableOperationKind::CompositionContinuation { turn_id, .. }
            | DurableOperationKind::CompositionChild { turn_id, .. } => *turn_id = turn,
            _ => return Err("composition fixture kind".into()),
        }
        let mut transitions = record.transitions().to_vec();
        if record.operation_id() == 101 {
            transitions.truncate(2);
            transitions.push(DurableOperationTransition::new(
                checkpoint(99)?,
                DurableOperationState::LocatorBound(runtime_component(
                    RuntimeComponentKind::Operation,
                    b"yielded cell locator".to_vec(),
                )?),
            ));
        }
        records.push(DurableOperationRecord::new(
            record.operation_id(),
            kind,
            record.request().clone(),
            transitions,
        )?);
    }
    records.sort_by_key(DurableOperationRecord::operation_id);
    let source =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    let retained = retire_at(&source, turn, &BTreeSet::new(), 16)?;
    assert!(retained.operation_count() < source.operation_count());
    for id in [101, 102, 103] {
        assert_eq!(
            retained.operation(id),
            source.operation(id),
            "live group {id}"
        );
    }
    validate_live_retirement(&source, &retained, turn)?;
    let stripped = retained.without_retirement_source();
    let lifecycle = crate::runtime::operation_record::working_lifecycle(turn, &stripped)?;
    let checkpoint = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        plakous_model::CanonicalHistory::default(),
        3,
        true,
        Some(stripped.clone()),
        Some(lifecycle),
    )?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    assert_eq!(
        bundle.validated_checkpoint()?.manifest().ledger(),
        &stripped
    );
    Ok(())
}

#[test]
fn retirement_keeps_receipt_controls_and_owned_roots_outside_the_window() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let owned = TurnId::try_from(uuid::Uuid::from_u128(0x900))?;
    let receipt = super::super::tests::root_command(2, owned)?;
    let mut commands = vec![ledger.commands()[0].clone(), receipt.clone()];
    for sequence in 3..=260 {
        commands.push(super::super::tests::rejected_command(sequence)?);
    }
    let latest = TurnId::try_from(uuid::Uuid::from_u128(0x905))?;
    commands[3] = super::super::tests::root_command(4, latest)?;
    commands[2] = steering_receipt(3, turn)?;
    commands[259] = steering_receipt(260, turn)?;
    let records = ledger
        .operations()
        .into_iter()
        .map(|record| {
            let (kind, command) = match record.operation_id() {
                2 => (
                    DurableOperationKind::Command {
                        turn_id: Some(owned),
                    },
                    &receipt,
                ),
                4 => (
                    DurableOperationKind::Command {
                        turn_id: Some(latest),
                    },
                    &commands[3],
                ),
                20 => (
                    DurableOperationKind::Command {
                        turn_id: Some(turn),
                    },
                    &commands[259],
                ),
                3 => (
                    DurableOperationKind::Command {
                        turn_id: Some(turn),
                    },
                    &commands[2],
                ),
                _ => return Ok(record.clone()),
            };
            let component = runtime_component(
                RuntimeComponentKind::Operation,
                command.command_component().bytes().to_vec(),
            )?;
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                kind,
                component.clone(),
                vec![DurableOperationTransition::new(
                    checkpoint(90)?,
                    DurableOperationState::Finalized(component),
                )],
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let source = RuntimeOperationLedger::new(commands, records)?;
    let retained = retire_at(&source, turn, &BTreeSet::from([turn, owned]), 16)?;
    for id in [1, 2, 4, 20] {
        assert_eq!(
            retained.operation(id),
            source.operation(id),
            "owned/receipt anchor {id}"
        );
    }
    assert!(
        retained.operation(3).is_none(),
        "expired control is independent closed debt"
    );
    assert!(
        retained
            .commands()
            .iter()
            .any(|command| command.turn_id() == Some(owned))
    );
    assert_eq!(retained.command_count(), COMMAND_RETENTION_WINDOW + 3);
    assert!(retained.retirement_preserves_owned_roots());
    let lost_owner = retained.retained(
        RetentionAnchor::ExistingHighest,
        |_| true,
        |record| record.operation_id() != 2,
    )?;
    assert!(!lost_owner.retirement_preserves_owned_roots());
    assert_owned_publication_refusal(source, lost_owner, turn)
}

fn assert_owned_publication_refusal(
    source: RuntimeOperationLedger,
    lost_owner: RuntimeOperationLedger,
    turn: TurnId,
) -> TestResult {
    let lifecycle = crate::runtime::operation_record::working_lifecycle(turn, &source)?;
    let prior = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        plakous_model::CanonicalHistory::default(),
        3,
        true,
        Some(source),
        Some(lifecycle.clone()),
    )?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&prior, None)?;
    let manifest = prior.manifest();
    let candidate = crate::runtime::checkpoint::candidate::successor_candidate(
        &prior,
        &bundle,
        crate::runtime::checkpoint::candidate::SuccessorFacts {
            checkpoint_id: checkpoint(0x901)?,
            lifecycle,
            history: manifest.canonical().history().clone(),
            ledger: lost_owner,
            jobs: manifest.jobs().clone(),
            named_state: manifest.named_state().clone(),
            context: manifest.canonical().context().retained_for_exact_resume(),
            family: manifest.family_artifact().cloned(),
        },
        1,
        None,
    );
    assert_eq!(
        candidate.err().ok_or("lost owner published")?.message(),
        "retirement source disagrees with its parent"
    );
    Ok(())
}

fn steering_receipt(sequence: u64, turn_id: TurnId) -> TestResult<crate::DurableCommandRecord> {
    use crate::runtime::control::{command_binding, command_canonical_bytes};
    let command = crate::TurnCommand::Steer {
        command_id: crate::CommandId::new(format!("steering:{sequence}"))?,
        turn_id,
        input: format!("note:{sequence}").into(),
    };
    let envelope = crate::runtime::DurableCommandEnvelope {
        version: 1,
        canonical_input: command_canonical_bytes(&command),
        effect: crate::runtime::DurableActorCommandEffect::SteerQueued { position: 1 },
        rejection: None,
    };
    Ok(crate::DurableCommandRecord::new(
        command.command_id().as_str(),
        sequence,
        Some(turn_id),
        command_binding(&command)?,
        runtime_component(
            RuntimeComponentKind::Command,
            serde_json::to_vec(&envelope)?,
        )?,
        crate::DurableCommandDisposition::Accepted,
        None,
    )?)
}

#[test]
fn retirement_keeps_steering_references_and_their_receipts_beyond_the_window() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let control = steering_receipt(2, turn)?;
    let mut commands = vec![ledger.commands()[0].clone(), control.clone()];
    for sequence in 3..=260 {
        commands.push(super::super::tests::rejected_command(sequence)?);
    }
    let records = ledger
        .operations()
        .into_iter()
        .map(|record| {
            if record.operation_id() == 2 {
                let request = runtime_component(
                    RuntimeComponentKind::Operation,
                    control.command_component().bytes().to_vec(),
                )?;
                return Ok(DurableOperationRecord::new(
                    2,
                    DurableOperationKind::Command {
                        turn_id: Some(turn),
                    },
                    request.clone(),
                    vec![DurableOperationTransition::new(
                        checkpoint(90)?,
                        DurableOperationState::Finalized(request),
                    )],
                )?);
            }
            if record.operation_id() != 102 {
                return Ok(record.clone());
            }
            let mut outcome = crate::runtime::operation_record::provider_outcome(record)?
                .ok_or("round outcome")?;
            outcome.steering.push(crate::runtime::OperationId(2));
            let evidence = outcome.component()?;
            let transitions = record
                .transitions()
                .iter()
                .map(|transition| {
                    DurableOperationTransition::new(
                        transition.checkpoint_id(),
                        match transition.state() {
                            DurableOperationState::Completed(_) => {
                                DurableOperationState::Completed(evidence.clone())
                            }
                            DurableOperationState::Finalized(_) => {
                                DurableOperationState::Finalized(evidence.clone())
                            }
                            state => state.clone(),
                        },
                    )
                })
                .collect();
            Ok(DurableOperationRecord::new(
                102,
                record.kind().clone(),
                record.request().clone(),
                transitions,
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    let source = RuntimeOperationLedger::new(commands, records)?;
    let retained = retire_at(&source, turn, &BTreeSet::from([turn]), 16)?;
    assert!(retained.retired_attempts(turn) > 0);
    assert_eq!(retained.operation(2), source.operation(2));
    assert!(retained.commands().contains(&&control));
    assert_eq!(retained.command_count(), COMMAND_RETENTION_WINDOW + 2);
    Ok(())
}

#[test]
fn a_nested_process_join_is_not_a_retirement_boundary() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let process = DurableOperationRecord::new(
        103,
        DurableOperationKind::ProcessSession {
            turn_id: turn,
            session_id: crate::manifest::ProcessSessionId::new(
                [1; crate::manifest::PROCESS_SESSION_ID_BYTES],
            )?,
            owner_generation: crate::manifest::ProcessSessionOwnerGeneration::new(1)?,
        },
        runtime_component(RuntimeComponentKind::Operation, b"process".to_vec())?,
        leaf(103, turn, false)?.transitions().to_vec(),
    )?;
    let prior = ledger.append_operation(process)?;
    let joined = prior.transition_operation(
        103,
        DurableOperationTransition::new(
            checkpoint(5)?,
            DurableOperationState::Finalized(runtime_component(
                RuntimeComponentKind::Operation,
                b"joined".to_vec(),
            )?),
        ),
    )?;
    assert!(!has_retirement_boundary(&prior, &joined));
    let next = joined.append_operation(leaf(104, turn, true)?)?;
    assert!(has_retirement_boundary(&joined, &next));
    assert!(!has_retirement_boundary(&next, &next));
    Ok(())
}
