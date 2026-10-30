use super::*;
use plakous_model::{
    Actor, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId, FinishDetails,
    FinishReason, ModelError, ModelErrorKind, Provenance, TextBlock,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn response() -> Result<ModelResponse, Box<dyn std::error::Error>> {
    Ok(ModelResponse::new(
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(0xA2_01))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("done", None)?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        Some(Usage::new(11, 3, 0, 0, 0)),
        Some("resp-a2".into()),
        Digest::sha256(DigestDomain::Blob, 1, &[b"a2-plan"])?,
    ))
}

fn failure(kind: ModelErrorKind) -> Result<DurableAgentError, AgentError> {
    DurableAgentError::from_error(&AgentError::from_model(ModelError::new(
        kind,
        "attempt failed",
    )))
}

fn budgets() -> TurnBudgets {
    TurnBudgets {
        output_limit_retry_taken: true,
        content_filter_retry_spent: true,
        overflow_recovery_taken: false,
        paid_fold_spent: true,
        pressure_fold_spent: false,
        model_rounds: 3,
    }
}

#[test]
fn every_decision_round_trips_through_its_exact_canonical_bytes() -> TestResult {
    let fail = RoundDecision::FailTurn(FailTurnShape {
        failure: failure(ModelErrorKind::ContextLimit)?,
        prompt_withheld: true,
        remedy: Some(DecidedRemedy::ResendShorter),
    });
    for (result, decision) in [
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::Terminal,
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::Continue,
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::ToolBatch,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
            fail,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::OutputLimit)?),
            RoundDecision::RetryOutputLimit,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::ContentFilteredIncomplete)?),
            RoundDecision::RetryContentFilter,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
            RoundDecision::OverflowFold,
        ),
    ] {
        let steering = if decision == RoundDecision::Terminal {
            Vec::new()
        } else {
            vec![OperationId(4), OperationId(9)]
        };
        let outcome = ProviderOutcome {
            result,
            usage: Some(Usage::new(7, 2, 0, 0, 0)),
            decision,
            budgets: budgets(),
            steering,
        };
        let component = outcome.component()?;
        assert!(component.bytes().starts_with(b"[\"provider-outcome-v2\","));
        assert_eq!(ProviderOutcome::decode(&component)?, Some(outcome));
    }
    Ok(())
}

#[test]
fn a_non_canonical_or_malformed_record_is_corrupt_and_other_evidence_is_none() -> TestResult {
    let outcome = ProviderOutcome {
        result: ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
        usage: None,
        decision: RoundDecision::FailTurn(FailTurnShape {
            failure: failure(ModelErrorKind::Protocol)?,
            prompt_withheld: false,
            remedy: None,
        }),
        budgets: TurnBudgets::default(),
        steering: Vec::new(),
    };
    let mut padded = outcome.component()?.bytes().to_vec();
    padded.push(b' ');
    for bytes in [
        padded,
        b"[\"provider-outcome-v2\",{}]".to_vec(),
        b"[\"provider-outcome-v2\",{\"result\":1}]".to_vec(),
        b"corrupt".to_vec(),
    ] {
        let evidence = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        assert!(
            ProviderOutcome::decode(&evidence).is_err(),
            "malformed record accepted"
        );
    }
    let other = runtime_component(
        RuntimeComponentKind::Operation,
        b"[\"plakous-code-cell-abandonment-evidence-v1\",{}]".to_vec(),
    )?;
    assert_eq!(ProviderOutcome::decode(&other)?, None);
    Ok(())
}

#[test]
fn an_incoherent_decision_is_corrupt() -> TestResult {
    for (result, decision, steering) in [
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
            RoundDecision::Terminal,
            Vec::new(),
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
            RoundDecision::ToolBatch,
            Vec::new(),
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
            RoundDecision::Continue,
            Vec::new(),
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::RetryOutputLimit,
            Vec::new(),
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::RetryContentFilter,
            Vec::new(),
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::OverflowFold,
            Vec::new(),
        ),
    ] {
        let outcome = ProviderOutcome {
            result,
            usage: None,
            decision,
            budgets: TurnBudgets::default(),
            steering,
        };
        assert!(
            ProviderOutcome::decode(&outcome.component()?).is_err(),
            "{:?}",
            outcome.decision
        );
    }
    Ok(())
}

/// The frozen bytes of one record per decision. A change here is an epoch
/// bump, never a silent re-pin.
#[test]
fn one_record_per_decision_has_frozen_bytes() -> TestResult {
    const GOLDEN: [&str; 7] = [
        "b0b683b4ae1e0bd9499b9f19ce68589175ec8bbcd1edab4262743d105b9a04bd",
        "6976c4d2283d4c87f09ee9703039402d78c4b9a488e9fa4db8925e9c142c4ab1",
        "6f20caa88e11bd4ae0a937c1b42a716cbbb34fe0edc6aca35942ea2324754e86",
        "149487ced9e8d22acd070eef9458bc37403d3024387f228447fb015c3a30b0db",
        "8bb87b1ff4247f2936df201468d5a66fc76d89ce1b237a5abddb898d260d2aa7",
        "df056b6fdfcf3cf7565519103cd238a154bbd36499252dd3fae4cbb65b69a901",
        "57aadb4f022c0b70d202cce8ba3aba080e878461ec18d0e7e436ba1d97d8e50a",
    ];
    let fail = RoundDecision::FailTurn(FailTurnShape {
        failure: failure(ModelErrorKind::ContextLimit)?,
        prompt_withheld: true,
        remedy: Some(DecidedRemedy::ResendShorter),
    });
    let mut actual = Vec::new();
    for (result, decision) in [
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::Terminal,
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::Continue,
        ),
        (
            ProviderAttemptResult::Response(response()?),
            RoundDecision::ToolBatch,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
            fail,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::OutputLimit)?),
            RoundDecision::RetryOutputLimit,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::ContentFilteredIncomplete)?),
            RoundDecision::RetryContentFilter,
        ),
        (
            ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
            RoundDecision::OverflowFold,
        ),
    ] {
        let steering = if decision == RoundDecision::Terminal {
            Vec::new()
        } else {
            vec![OperationId(4)]
        };
        let bytes = ProviderOutcome {
            result,
            usage: Some(Usage::new(7, 2, 0, 0, 0)),
            decision,
            budgets: budgets(),
            steering,
        }
        .component()?
        .bytes()
        .to_vec();
        let digest = Digest::sha256(DigestDomain::Blob, 1, &[&bytes])?;
        actual.push(digest.bytes().iter().fold(String::new(), |mut text, byte| {
            let _ = std::fmt::Write::write_fmt(&mut text, format_args!("{byte:02x}"));
            text
        }));
    }
    assert_eq!(actual, GOLDEN, "provider-outcome-v2 golden bytes changed");
    Ok(())
}

#[test]
fn budgets_compare_rounds_against_the_admitted_cap() {
    let budgets = TurnBudgets {
        model_rounds: 4,
        ..TurnBudgets::default()
    };
    assert!(budgets.over_round_cap(3));
    assert!(!budgets.over_round_cap(4));
}

fn round_provider(
    states: Vec<DurableOperationState>,
) -> Result<DurableOperationRecord, Box<dyn std::error::Error>> {
    let request = runtime_component(RuntimeComponentKind::Operation, b"request".to_vec())?;
    Ok(DurableOperationRecord::new(
        7,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(plakous_model::TurnId::try_from(
                uuid::Uuid::from_u128(0xA4_E0),
            )?),
            attempt_id: plakous_model::AttemptId::try_from(uuid::Uuid::from_u128(0xA4_E1))?,
        },
        request,
        std::iter::once(DurableOperationState::Prepared)
            .chain(states)
            .enumerate()
            .map(|(index, state)| {
                Ok(crate::DurableOperationTransition::new(
                    crate::CheckpointId::try_from(uuid::Uuid::from_u128(0xA4_F0 + index as u128))?,
                    state,
                ))
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
    )?)
}

fn tagged(tag: &str) -> Result<ReconstructiveComponent, AgentError> {
    runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&(tag, "closure detail")).map_err(|_| {
            AgentError::new(AgentErrorKind::Internal, "evidence could not be encoded")
        })?,
    )
}

#[test]
fn each_enumerated_closure_tag_reads_as_no_outcome() -> TestResult {
    for tag in CLOSURE_TAGS {
        let evidence = tagged(tag)?;
        assert_eq!(ProviderOutcome::decode(&evidence)?, None, "{tag}");
        for states in [
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::OutcomeUnknown(evidence.clone()),
                DurableOperationState::Finalized(evidence.clone()),
            ],
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::OutcomeUnknown(evidence.clone()),
            ],
            vec![
                DurableOperationState::ProvenNotDispatched(evidence.clone()),
                DurableOperationState::Finalized(evidence.clone()),
            ],
        ] {
            assert_eq!(provider_outcome(&round_provider(states)?)?, None, "{tag}");
        }
    }
    Ok(())
}

#[test]
fn an_unknown_tag_is_corrupt_in_every_state() -> TestResult {
    let evidence = tagged("plakous-agent/unknown-closure-v1")?;
    assert!(ProviderOutcome::decode(&evidence).is_err());
    for states in [
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::OutcomeUnknown(evidence.clone()),
        ],
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::OutcomeUnknown(evidence.clone()),
            DurableOperationState::Finalized(evidence.clone()),
        ],
        vec![DurableOperationState::ProvenNotDispatched(evidence.clone())],
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::Completed(evidence.clone()),
        ],
    ] {
        assert!(provider_outcome(&round_provider(states)?).is_err());
    }
    Ok(())
}

#[test]
fn completed_and_its_finalization_require_the_outcome_record() -> TestResult {
    let closure = tagged("plakous-agent/abandoned-provider-attempt-v1")?;
    for states in [
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::Completed(closure.clone()),
        ],
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::Completed(closure.clone()),
            DurableOperationState::Finalized(closure.clone()),
        ],
    ] {
        assert!(
            provider_outcome(&round_provider(states)?).is_err(),
            "a closure tag stood in for provider-outcome-v2"
        );
    }
    let outcome = ProviderOutcome {
        result: ProviderAttemptResult::Response(response()?),
        usage: None,
        decision: RoundDecision::Terminal,
        budgets: TurnBudgets::default(),
        steering: Vec::new(),
    };
    let record = outcome.component()?;
    for states in [
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::Completed(record.clone()),
        ],
        vec![
            DurableOperationState::Dispatched,
            DurableOperationState::Completed(record.clone()),
            DurableOperationState::Finalized(record.clone()),
        ],
    ] {
        assert_eq!(
            provider_outcome(&round_provider(states)?)?,
            Some(outcome.clone())
        );
    }
    Ok(())
}
