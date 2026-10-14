use super::*;
use plakous_model::{
    Actor, AttemptId, CallId, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId,
    FamilyArtifactProducerFacts, FinishDetails, McpServerId, ModelErrorPhase, ModelOperationRoot,
    NamespaceId, Provenance, RunId, SealedFamilyArtifactCandidate, TextBlock, ToolCallState,
    ToolCorrelation, ToolExecutionOwner, ToolName, TurnId,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn output_limit_retry_eligibility_is_exact() {
    for kind in [
        None,
        Some(ModelErrorKind::OutputLimit),
        Some(ModelErrorKind::IncompleteResponse),
    ] {
        for retry_taken in [false, true] {
            for over_budget in [false, true] {
                assert_eq!(
                    output_limit_retry_eligible(kind, retry_taken, over_budget),
                    kind == Some(ModelErrorKind::OutputLimit) && !retry_taken && !over_budget
                );
            }
        }
    }
}

fn digest(domain: DigestDomain) -> TestResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[b"round-classifier-fixture"])?)
}

fn call(id: u128, owner: ToolExecutionOwner, state: ToolCallState) -> TestResult<ToolCallBlock> {
    Ok(ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(id))?,
        ToolName::new("probe")?,
        owner,
        state,
        ToolCorrelation::default(),
        "{}",
    )?)
}

fn hosted() -> TestResult<ToolExecutionOwner> {
    Ok(ToolExecutionOwner::ProviderHosted {
        provider_kind: NamespaceId::new("hosted.probe")?,
    })
}

fn item(calls: Vec<ToolCallBlock>) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(100))?,
        Actor::Assistant,
        calls.into_iter().map(ContentBlock::ToolCall).collect(),
        Provenance::ModelOutput,
    )?)
}

fn response(
    finish: FinishReason,
    calls: Vec<ToolCallBlock>,
    artifact: bool,
) -> TestResult<ModelResponse> {
    let output = if calls.is_empty() {
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(100))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("answer", None)?)],
            Provenance::ModelOutput,
        )?]
    } else {
        vec![item(calls)?]
    };
    let output_count = u32::try_from(output.len())?;
    let plan = digest(DigestDomain::RequestPlan)?;
    let response = ModelResponse::new(output, FinishDetails::new(finish, None), None, None, plan);
    if !artifact {
        return Ok(response);
    }
    let producer = FamilyArtifactProducerFacts::new(
        plan,
        plan,
        plan,
        None,
        RunId::try_from(uuid::Uuid::from_u128(101))?,
        ModelOperationRoot::Turn(TurnId::try_from(uuid::Uuid::from_u128(102))?),
        AttemptId::try_from(uuid::Uuid::from_u128(103))?,
        digest(DigestDomain::CanonicalItemPrefix)?,
        0,
        output_count,
        digest(DigestDomain::CanonicalItemPrefix)?,
        output_count,
        digest(DigestDomain::ProviderOperation)?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"terminal"])?,
    )?;
    Ok(
        response.with_family_artifact_candidate(SealedFamilyArtifactCandidate::new(
            "classifier-fixture",
            1,
            1,
            digest(DigestDomain::ModelBinding)?,
            producer,
            vec![1],
        )?)?,
    )
}

fn classify(
    response: &ModelResponse,
    over_budget: bool,
) -> Result<HostedRoundDisposition, RoundClassificationError> {
    classify_round(RoundClassificationInput {
        response,
        history: &ConversationView::default(),
        over_budget,
        wire_output_limit: None,
    })
}

#[test]
fn round_classifier_terminal_finishes_stop_length_refusal_policy() -> TestResult {
    for finish in [
        FinishReason::Stop,
        FinishReason::Length,
        FinishReason::Refusal,
        FinishReason::Policy,
    ] {
        assert!(matches!(
            classify(&response(finish, vec![], false)?, false),
            Ok(HostedRoundDisposition::Terminal)
        ));
    }
    Ok(())
}

#[test]
fn round_classifier_local_calls_preserve_order_and_owners() -> TestResult {
    let local = call(1, ToolExecutionOwner::PlakLocal, ToolCallState::Requested)?;
    let mcp = call(
        2,
        ToolExecutionOwner::PlakMcp {
            server: McpServerId::new("probe")?,
        },
        ToolCallState::Pending,
    )?;
    let hosted = call(3, hosted()?, ToolCallState::Completed)?;
    let response = response(
        FinishReason::ToolCalls,
        vec![local.clone(), hosted, mcp.clone()],
        false,
    )?;
    let Ok(HostedRoundDisposition::CustomerToolsRequested(calls)) = classify(&response, false)
    else {
        return Err("local calls not classified".into());
    };
    assert_eq!(calls, vec![local, mcp]);
    Ok(())
}

#[test]
fn round_classifier_provider_hosted_terminal() -> TestResult {
    let response = response(
        FinishReason::Stop,
        vec![call(1, hosted()?, ToolCallState::Completed)?],
        false,
    )?;
    assert!(matches!(
        classify(&response, false),
        Ok(HostedRoundDisposition::Terminal)
    ));
    Ok(())
}

#[test]
fn round_classifier_assistant_continuation() -> TestResult {
    assert!(matches!(
        classify(
            &response(FinishReason::AssistantContinuation, vec![], false)?,
            false
        ),
        Ok(HostedRoundDisposition::AssistantContinuation)
    ));
    Ok(())
}

#[test]
fn round_classifier_provider_continuation_requires_artifact() -> TestResult {
    let missing = response(FinishReason::ProviderContinuation, vec![], false)?;
    let Err(RoundClassificationError::InvalidDisposition(error)) = classify(&missing, false) else {
        return Err("missing artifact accepted".into());
    };
    assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    let sealed = response(FinishReason::ProviderContinuation, vec![], true)?;
    assert!(matches!(
        classify(&sealed, false),
        Ok(HostedRoundDisposition::ServerPaused)
    ));
    Ok(())
}

#[test]
fn round_classifier_output_limit_uses_prepared_wire_bound() -> TestResult {
    let response = response(
        FinishReason::Length,
        vec![call(
            1,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
        )?],
        false,
    )?;
    for wire_output_limit in [None, NonZeroU32::new(4096)] {
        let Err(RoundClassificationError::IncompleteLocalCall(error)) =
            classify_round(RoundClassificationInput {
                response: &response,
                history: &ConversationView::default(),
                over_budget: true,
                wire_output_limit,
            })
        else {
            return Err("truncated local call accepted".into());
        };
        assert_eq!(error.kind(), ModelErrorKind::OutputLimit);
        assert_eq!(error.wire_output_limit(), wire_output_limit);
    }
    Ok(())
}

#[test]
fn round_classifier_incomplete_local_call_precedes_budget_and_pause() -> TestResult {
    for finish in [
        FinishReason::Stop,
        FinishReason::AssistantContinuation,
        FinishReason::ProviderContinuation,
        FinishReason::Refusal,
        FinishReason::Policy,
    ] {
        let response = response(
            finish,
            vec![call(
                1,
                ToolExecutionOwner::PlakLocal,
                ToolCallState::Requested,
            )?],
            false,
        )?;
        let Err(RoundClassificationError::IncompleteLocalCall(error)) = classify(&response, true)
        else {
            return Err("wrong incomplete-call priority".into());
        };
        assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    }
    Ok(())
}

#[test]
fn round_classifier_over_budget_rejects_tools_and_assistant() -> TestResult {
    let tools = response(
        FinishReason::ToolCalls,
        vec![call(
            1,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
        )?],
        false,
    )?;
    let assistant = response(FinishReason::AssistantContinuation, vec![], false)?;
    for response in [tools, assistant] {
        assert!(matches!(
            classify(&response, true),
            Err(RoundClassificationError::RoundLimit)
        ));
    }
    Ok(())
}

#[test]
fn round_classifier_wrap_up_allows_terminal_and_server_pause() -> TestResult {
    assert!(matches!(
        classify(&response(FinishReason::Stop, vec![], false)?, true),
        Ok(HostedRoundDisposition::Terminal)
    ));
    assert!(matches!(
        classify(
            &response(FinishReason::ProviderContinuation, vec![], true)?,
            true
        ),
        Ok(HostedRoundDisposition::ServerPaused)
    ));
    Ok(())
}

#[test]
fn round_classifier_tool_call_finish_requires_local_calls() -> TestResult {
    for calls in [vec![], vec![call(1, hosted()?, ToolCallState::Completed)?]] {
        let response = response(FinishReason::ToolCalls, calls, false)?;
        let Err(RoundClassificationError::InvalidDisposition(error)) = classify(&response, false)
        else {
            return Err("empty local-tool finish accepted".into());
        };
        assert_eq!(error.kind(), ModelErrorKind::InvalidEvent);
    }
    Ok(())
}

#[test]
fn round_classifier_duplicate_call_identity_response_and_history() -> TestResult {
    let local = call(1, ToolExecutionOwner::PlakLocal, ToolCallState::Requested)?;
    let duplicate = response(
        FinishReason::Length,
        vec![local.clone(), local.clone()],
        false,
    )?;
    let Err(RoundClassificationError::InvalidCalls(error)) = classify(&duplicate, true) else {
        return Err("duplicate response call accepted".into());
    };
    assert_eq!(error.kind(), ModelErrorKind::InvalidEvent);
    let history = ConversationView::new(vec![item(vec![local.clone()])?]);
    let reused = response(FinishReason::ToolCalls, vec![local], false)?;
    assert!(matches!(
        classify_round(RoundClassificationInput {
            response: &reused,
            history: &history,
            over_budget: false,
            wire_output_limit: None
        }),
        Err(RoundClassificationError::InvalidCalls(_))
    ));
    Ok(())
}

#[test]
fn round_classifier_terminal_local_states_refuse_before_other_checks() -> TestResult {
    for state in [ToolCallState::Completed, ToolCallState::Failed] {
        let response = response(
            FinishReason::Length,
            vec![call(1, ToolExecutionOwner::PlakLocal, state)?],
            false,
        )?;
        let Err(RoundClassificationError::InvalidCalls(error)) = classify(&response, true) else {
            return Err("terminal local state accepted".into());
        };
        assert_eq!(error.kind(), ModelErrorKind::InvalidEvent);
    }
    Ok(())
}

#[test]
fn round_classifier_steering_continuing() {
    assert!(!HostedRoundDisposition::Terminal.continuing(false));
    assert!(HostedRoundDisposition::Terminal.continuing(true));
    assert!(HostedRoundDisposition::AssistantContinuation.continuing(false));
    assert!(HostedRoundDisposition::AssistantContinuation.continuing(true));
}

#[test]
fn round_classifier_driver_adapter_rows() {
    let model_error = || ModelError::new(ModelErrorKind::InvalidEvent, "adapter fixture");
    for (error, retry_output_limit) in [
        (RoundClassificationError::InvalidCalls(model_error()), false),
        (
            RoundClassificationError::IncompleteLocalCall(model_error()),
            true,
        ),
        (
            RoundClassificationError::InvalidDisposition(model_error()),
            false,
        ),
        (RoundClassificationError::RoundLimit, false),
    ] {
        let action = round_failure_action(&error);
        assert_eq!(action.phase, ModelErrorPhase::Terminal);
        assert_eq!(action.retry_output_limit, retry_output_limit);
    }
}

#[derive(Debug, PartialEq)]
enum Decision {
    Terminal(bool),
    AssistantContinuation(bool),
    LocalCalls(Vec<ToolCallBlock>),
    ServerPaused,
    Error(
        ModelErrorKind,
        String,
        Option<NonZeroU32>,
        ModelErrorPhase,
        bool,
    ),
    RoundLimit,
}

fn model_failure(error: &ModelError, retry: bool) -> Decision {
    Decision::Error(
        error.kind(),
        error.to_string(),
        error.wire_output_limit(),
        ModelErrorPhase::Terminal,
        retry,
    )
}

fn decision(
    result: Result<HostedRoundDisposition, RoundClassificationError>,
    steering: bool,
) -> Decision {
    match result {
        Ok(disposition) => {
            let continuing = disposition.continuing(steering);
            match disposition {
                HostedRoundDisposition::Terminal => Decision::Terminal(continuing),
                HostedRoundDisposition::AssistantContinuation => {
                    Decision::AssistantContinuation(continuing)
                }
                HostedRoundDisposition::CustomerToolsRequested(calls) => {
                    Decision::LocalCalls(calls)
                }
                HostedRoundDisposition::ServerPaused => Decision::ServerPaused,
            }
        }
        Err(error) => {
            let action = round_failure_action(&error);
            match error {
                RoundClassificationError::InvalidCalls(error)
                | RoundClassificationError::InvalidDisposition(error)
                | RoundClassificationError::IncompleteLocalCall(error) => Decision::Error(
                    error.kind(),
                    error.to_string(),
                    error.wire_output_limit(),
                    action.phase,
                    action.retry_output_limit,
                ),
                RoundClassificationError::RoundLimit => Decision::RoundLimit,
            }
        }
    }
}

// Frozen pre-extraction live decisions, only for the preservation proof below.
fn legacy_live_decision(input: RoundClassificationInput<'_>, steering: bool) -> Decision {
    let response = input.response;
    let calls = match collect_local_calls(response, input.history) {
        Ok(calls) => calls.into_iter().cloned().collect::<Vec<_>>(),
        Err(error) => return model_failure(&error, false),
    };
    if !calls.is_empty() && response.finish().reason() != FinishReason::ToolCalls {
        let error = if response.finish().reason() == FinishReason::Length {
            let mut error = ModelError::new(
                ModelErrorKind::OutputLimit,
                "response reached its output bound before its tool call was complete",
            );
            if let Some(tokens) = input.wire_output_limit {
                error.fill_missing_wire_output_limit(tokens);
            }
            error
        } else {
            ModelError::new(
                ModelErrorKind::IncompleteResponse,
                "model proposed a local tool call without a complete tool-call finish",
            )
        };
        return model_failure(&error, true);
    }
    let disposition = if calls.is_empty() {
        if response.finish().reason() == FinishReason::ProviderContinuation {
            HostedRoundDisposition::ServerPaused
        } else if response.finish().reason() == FinishReason::AssistantContinuation {
            HostedRoundDisposition::AssistantContinuation
        } else {
            HostedRoundDisposition::Terminal
        }
    } else {
        HostedRoundDisposition::CustomerToolsRequested(calls)
    };
    if input.over_budget
        && matches!(
            &disposition,
            HostedRoundDisposition::CustomerToolsRequested(_)
                | HostedRoundDisposition::AssistantContinuation
        )
    {
        return Decision::RoundLimit;
    }
    if !matches!(
        &disposition,
        HostedRoundDisposition::CustomerToolsRequested(_)
    ) && response.finish().reason() == FinishReason::ToolCalls
    {
        return model_failure(
            &ModelError::new(
                ModelErrorKind::InvalidEvent,
                "model reported tool-call completion without a dispatchable local call",
            ),
            false,
        );
    }
    if matches!(&disposition, HostedRoundDisposition::ServerPaused)
        && response.family_artifact_candidate().is_none()
    {
        return model_failure(
            &ModelError::new(
                ModelErrorKind::IncompleteResponse,
                "provider server pause omitted its exact family artifact",
            ),
            false,
        );
    }
    let continuing =
        steering || matches!(&disposition, HostedRoundDisposition::AssistantContinuation);
    match disposition {
        HostedRoundDisposition::Terminal => Decision::Terminal(continuing),
        HostedRoundDisposition::AssistantContinuation => {
            Decision::AssistantContinuation(continuing)
        }
        HostedRoundDisposition::CustomerToolsRequested(calls) => Decision::LocalCalls(calls),
        HostedRoundDisposition::ServerPaused => Decision::ServerPaused,
    }
}

#[test]
fn round_classifier_behaviour_preservation_legacy_live_decision_matrix() -> TestResult {
    let local = call(1, ToolExecutionOwner::PlakLocal, ToolCallState::Requested)?;
    let variants = [
        vec![],
        vec![local.clone()],
        vec![call(
            2,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Pending,
        )?],
        vec![call(
            2,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Completed,
        )?],
        vec![call(
            2,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Failed,
        )?],
        vec![call(
            2,
            ToolExecutionOwner::PlakMcp {
                server: McpServerId::new("probe")?,
            },
            ToolCallState::Requested,
        )?],
        vec![call(2, hosted()?, ToolCallState::Completed)?],
        vec![local.clone(), call(2, hosted()?, ToolCallState::Completed)?],
        vec![local.clone(), local.clone()],
    ];
    let histories = [
        ConversationView::default(),
        ConversationView::new(vec![item(vec![local])?]),
    ];
    let mut cases = 0;
    for finish in [
        FinishReason::Stop,
        FinishReason::ToolCalls,
        FinishReason::AssistantContinuation,
        FinishReason::ProviderContinuation,
        FinishReason::Length,
        FinishReason::Refusal,
        FinishReason::Policy,
    ] {
        for calls in &variants {
            for artifact in [false, true] {
                // Recovery supplies the stored response, not a live-only object.
                let response: ModelResponse = serde_json::from_slice(&serde_json::to_vec(
                    &response(finish, calls.clone(), artifact)?,
                )?)?;
                for history in &histories {
                    for over_budget in [false, true] {
                        for steering in [false, true] {
                            for wire_output_limit in [None, NonZeroU32::new(4096)] {
                                let input = || RoundClassificationInput {
                                    response: &response,
                                    history,
                                    over_budget,
                                    wire_output_limit,
                                };
                                assert_eq!(
                                    decision(classify_round(input()), steering),
                                    legacy_live_decision(input(), steering),
                                    "case {cases}: {finish:?}, artifact={artifact}, budget={over_budget}, steering={steering}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 2016);
    Ok(())
}
