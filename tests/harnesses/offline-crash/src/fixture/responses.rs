use super::{
    Actor, CODE_MODE_EXEC_NAME, CODE_MODE_WAIT_NAME, CallId, ContentBlock, ConversationItem,
    Digest, EntryId, FINAL_TEXT, FinishDetails, FinishReason, LoopbackScenario, ModelError,
    ModelErrorKind, ModelEvent, ModelResponse, ModelStream, NamespaceId, Provenance,
    ResolvedModelDescriptor, ScriptErrorKind, ScriptFinish, ScriptOutput, ScriptStep,
    ScriptStreamEvent, StreamExt, TextBlock, TextPhase, ToolCallBlock, ToolCallState,
    ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultPart, Uuid, ValidatedRequestPlan,
    future, json, stream,
};

/// The provider error one scripted fault reports.
pub(super) fn scripted_error(
    fault: &crate::script::ScriptFault,
    descriptor: &ResolvedModelDescriptor,
) -> ModelError {
    let kind = match fault.kind {
        ScriptErrorKind::Unavailable => ModelErrorKind::Unavailable,
        ScriptErrorKind::Transport => ModelErrorKind::Transport,
        ScriptErrorKind::Timeout => ModelErrorKind::Timeout,
        ScriptErrorKind::RateLimited => ModelErrorKind::RateLimited,
        ScriptErrorKind::IncompleteResponse => ModelErrorKind::IncompleteResponse,
        ScriptErrorKind::OutcomeUnknown => ModelErrorKind::OutcomeUnknown,
        ScriptErrorKind::Protocol => ModelErrorKind::Protocol,
        ScriptErrorKind::ContextOverflow => ModelErrorKind::ContextLimit,
    };
    let message = fault
        .message
        .clone()
        .unwrap_or_else(|| "scripted provider fault".to_owned());
    if fault.phase.is_none() && fault.retryability.is_none() {
        return ModelError::new(kind, message);
    }
    let context = plakous::model::ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        fault
            .phase
            .unwrap_or(plakous::model::ModelErrorPhase::Stream),
        fault
            .retryability
            .unwrap_or(plakous::model::ModelRetryability::Unknown),
    );
    context
        .and_then(|context| ModelError::contextual(kind, message, context))
        .unwrap_or_else(|error| {
            ModelError::new(
                ModelErrorKind::Protocol,
                format!("invalid scripted error context: {error}"),
            )
        })
}

/// The events one scripted step streams ahead of its terminal response: its
/// `prefix`, then its `stream`.
///
/// Reasoning blocks take consecutive output indexes, restarting after a
/// discarding retry the way a replayed request does.
pub(super) fn scripted_stream(
    step: &ScriptStep,
    cancellation: plakous::model::CancellationToken,
    deaf: bool,
    cancel_settle: std::time::Duration,
) -> Result<ModelStream, ModelError> {
    enum Item {
        Event(Box<ModelEvent>),
        Stall(u64),
        Gate(crate::gate::ScriptGate),
    }
    let event = |model_event: ModelEvent| Item::Event(Box::new(model_event));
    let text = |text: &str| {
        event(ModelEvent::TextDelta(plakous::model::TextDelta::new(
            plakous::model::OutputLocation::new(0, 0),
            text.as_bytes().to_vec(),
        )))
    };
    let mut items = Vec::new();
    if let Some(prefix) = step.prefix.as_deref().filter(|prefix| !prefix.is_empty()) {
        items.push(text(prefix));
    }
    let mut next_reasoning = 1_u32;
    for streamed in &step.stream {
        match streamed {
            ScriptStreamEvent::Reasoning(reasoning) => {
                let location = plakous::model::OutputLocation::new(next_reasoning, 0);
                let item_id =
                    EntryId::try_from(Uuid::from_u128(0x5c21_0000 + u128::from(next_reasoning)))
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::Protocol,
                                "scripted reasoning identity is invalid",
                            )
                        })?;
                next_reasoning += 1;
                items.push(event(ModelEvent::BlockStarted(
                    plakous::model::BlockStarted::new(
                        item_id,
                        location,
                        plakous::model::BlockStart::Reasoning {
                            visibility: plakous::model::ReasoningVisibility::Visible,
                            replay: None,
                        },
                    ),
                )));
                items.push(event(ModelEvent::ReasoningDelta(
                    plakous::model::ReasoningDelta::new(location, reasoning.as_bytes().to_vec()),
                )));
                items.push(event(ModelEvent::BlockCompleted(
                    plakous::model::BlockCompleted::new(location),
                )));
            }
            ScriptStreamEvent::Text(delta) => items.push(text(delta)),
            ScriptStreamEvent::RetryDiscarded => {
                next_reasoning = 1;
                items.push(event(ModelEvent::Retry(plakous::model::ModelRetry::new(
                    2,
                    4,
                    std::time::Duration::ZERO,
                    plakous::model::ModelRetryCause::StreamInterrupted {
                        fault: plakous::model::ModelTransportFault::Io,
                    },
                    true,
                    "scripted stream reset",
                ))));
            }
            ScriptStreamEvent::Stall(milliseconds) => items.push(Item::Stall(*milliseconds)),
            ScriptStreamEvent::Gate(gate) => items.push(Item::Gate(gate.clone())),
        }
    }
    Ok(Box::pin(
        stream::iter(items)
            .then(move |item| {
                let cancellation = cancellation.clone();
                async move {
                    match item {
                        Item::Event(event) => Some(Ok(*event)),
                        Item::Gate(gate) => crate::gate::wait_gate(&gate, &cancellation, deaf)
                            .await
                            .err()
                            .map(Err),
                        Item::Stall(milliseconds) => {
                            if !stall(milliseconds, &cancellation, deaf).await {
                                return None;
                            }
                            tokio::time::sleep(cancel_settle).await;
                            Some(Err(ModelError::new(
                                ModelErrorKind::Cancelled,
                                "the scripted stream stall was cancelled",
                            )))
                        }
                    }
                }
            })
            .filter_map(future::ready),
    ))
}

/// Whether this scenario's stalls ignore cancellation.
pub(super) fn scripted_stall_is_deaf(scenario: &LoopbackScenario) -> bool {
    match scenario {
        LoopbackScenario::Scripted(script) | LoopbackScenario::ScriptedSummary(script, _) => {
            script.stall_ignores_cancellation
        }
        _ => false,
    }
}

/// Stalls one scripted operation for its declared duration, or until the call
/// is cancelled.
///
/// A real client deadline is a wall-clock fact the gate cannot afford to wait
/// out, so a script names a short stall and the error the deadline produces.
///
/// **The token is the contract, not the drop.** A model implementation is
/// handed a [`plakous::model::CancellationToken`] and is expected to observe
/// it; the runtime does not simply drop the in-flight future. A stall that
/// ignored the token therefore made every cancellation look broken from the
/// outside: the face reported "cancel did not settle", and the scripted answer
/// arrived afterwards as though nothing had been asked. That was this fixture
/// misreporting the product, and it is the reason this polls rather than
/// sleeping once.
///
/// Returns `true` when the stall ended because the call was cancelled.
pub(super) async fn stall(
    milliseconds: u64,
    cancellation: &plakous::model::CancellationToken,
    deaf: bool,
) -> bool {
    // The token exposes only `is_cancelled`, so observing it means polling. The
    // slice is short enough that a cancellation is answered promptly and long
    // enough that a multi-second stall is not a busy loop.
    const SLICE: std::time::Duration = std::time::Duration::from_millis(10);
    if deaf {
        if milliseconds > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
        }
        return false;
    }
    if milliseconds == 0 {
        return cancellation.is_cancelled();
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
    while tokio::time::Instant::now() < deadline {
        if cancellation.is_cancelled() {
            return true;
        }
        tokio::time::sleep(SLICE.min(deadline - tokio::time::Instant::now())).await;
    }
    cancellation.is_cancelled()
}

#[allow(
    clippy::too_many_lines,
    reason = "the deterministic fixture keeps its three closed response scenarios visibly exhaustive"
)]
pub(super) fn loopback_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    scenario: &LoopbackScenario,
    identity_namespace: u128,
    prepared_input_tokens: u64,
) -> Result<ModelResponse, ModelError> {
    if let LoopbackScenario::Scripted(script) = scenario {
        return scripted_response(
            round,
            plan,
            script.step(round),
            identity_namespace,
            prepared_input_tokens,
            script.hosted_tool_call_final,
        );
    }
    if let LoopbackScenario::ScriptedSummary(_, finish) = scenario {
        return scripted_summary_response(round, plan, identity_namespace, *finish);
    }
    let block = if let LoopbackScenario::Text { terminal: "", .. } = *scenario {
        ContentBlock::Reasoning(
            plakous::model::ReasoningBlock::new(
                plakous::model::ReasoningVisibility::Omitted,
                None,
                None,
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if let LoopbackScenario::Text { terminal, .. } = *scenario {
        ContentBlock::Text(
            TextBlock::new(terminal, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )
    } else if matches!(*scenario, LoopbackScenario::Compaction) {
        ContentBlock::Text(
            TextBlock::new("older intent groups compacted", Some(TextPhase::Final))
                .map_err(invalid_model_fixture)?,
        )
    } else if matches!(*scenario, LoopbackScenario::ExitedChildContinuation) && round <= 2 {
        let (name, arguments) = if round == 1 {
            (
                "process_session_start",
                // The child is still running when the start returns, so the
                // poll that follows is what finds it exited and joins it.
                json!({
                    "command": "head -c 10106 /dev/zero | tr '\\0' x; sleep 0.5",
                    "workdir": ".",
                    "login": false,
                    "tty": false,
                    "keep_stdin_open": true,
                    "yield_time_ms": 100,
                    "max_output_bytes": 8192
                }),
            )
        } else {
            let (session_id, cursor) = process_session_projection(plan).ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "process continuation fixture did not receive a session id and cursor",
                )
            })?;
            (
                "process_session_poll",
                json!({
                    "session_id": session_id,
                    "cursor": cursor,
                    "yield_time_ms": 5000,
                    "max_output_bytes": 8192,
                    "max_output_records": 256
                }),
            )
        };
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                CallId::try_from(loopback_identity(plan, identity_namespace, round, b"call")?)
                    .map_err(invalid_model_fixture)?,
                ToolName::new(name).map_err(invalid_model_fixture)?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                serde_json::to_string(&arguments).map_err(invalid_model_fixture)?,
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if matches!(scenario, LoopbackScenario::Topology) && round <= 3 {
        let (name, arguments) = topology_call(round, plan)?;
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                CallId::try_from(loopback_identity(plan, identity_namespace, round, b"call")?)
                    .map_err(invalid_model_fixture)?,
                ToolName::new(name).map_err(invalid_model_fixture)?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                serde_json::to_string(&arguments).map_err(invalid_model_fixture)?,
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if matches!(scenario, LoopbackScenario::ExitedChildContinuation) {
        ContentBlock::Text(
            TextBlock::new(
                "large exited child continued and joined",
                Some(TextPhase::Final),
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if round == 1
        || (round == 2 && matches!(scenario, LoopbackScenario::ContentSensitiveEffect))
    {
        // The growth scenario reads twice: the placeholder rung never touches the
        // batch the current round just received, so a single read leaves it
        // nothing to act on.
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                CallId::try_from(loopback_identity(plan, identity_namespace, round, b"call")?)
                    .map_err(invalid_model_fixture)?,
                ToolName::new("loopback_effect").map_err(invalid_model_fixture)?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                "{}",
            )
            .map_err(invalid_model_fixture)?,
        )
    } else {
        ContentBlock::Text(
            TextBlock::new(FINAL_TEXT, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )
    };
    let output = ConversationItem::new(
        EntryId::try_from(loopback_identity(
            plan,
            identity_namespace,
            round,
            b"entry",
        )?)
        .map_err(invalid_model_fixture)?,
        Actor::Assistant,
        if let LoopbackScenario::TextBlocks(texts) = scenario {
            texts
                .iter()
                .map(|text| {
                    TextBlock::new(*text, Some(TextPhase::Final))
                        .map(ContentBlock::Text)
                        .map_err(invalid_model_fixture)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![block]
        },
        Provenance::ModelOutput,
    )
    .map_err(invalid_model_fixture)?;
    Ok(ModelResponse::new(
        if matches!(
            *scenario,
            LoopbackScenario::Text {
                terminal: "",
                finish: FinishReason::Refusal,
                ..
            }
        ) {
            Vec::new()
        } else {
            vec![output]
        },
        FinishDetails::new(
            if (round == 1
                && matches!(
                    *scenario,
                    LoopbackScenario::Effect | LoopbackScenario::ContentSensitiveEffect
                ))
                || (round == 2 && matches!(*scenario, LoopbackScenario::ContentSensitiveEffect))
                || (round <= 2 && matches!(*scenario, LoopbackScenario::ExitedChildContinuation))
                || (round <= 3 && matches!(*scenario, LoopbackScenario::Topology))
            {
                FinishReason::ToolCalls
            } else if let LoopbackScenario::Text { finish, .. } = *scenario {
                finish
            } else {
                FinishReason::Stop
            },
            None,
        ),
        // Only the budget-exhausted scenario reports usage: a turn that stops
        // at `Length` having spent the whole bound on reasoning is what the
        // host has to tell apart from an ordinary empty stream, and it can
        // only do that from the counts. Every other scenario stays silent and
        // its goldens are unchanged.
        match *scenario {
            LoopbackScenario::Text {
                finish: FinishReason::Length,
                ..
            } => Some(plakous::model::Usage::new(64, 4_096, 4_095, 0, 0)),
            // The growth scenario reports its counts only on the terminal
            // response and never mid-stream. That is the shape several live
            // routes have, and the one that left the host's prepared-input
            // peak unpopulated until it was recorded here too.
            LoopbackScenario::ContentSensitiveEffect => Some(plakous::model::Usage::new(
                prepared_input_tokens,
                1,
                0,
                0,
                0,
            )),
            _ => None,
        },
        None,
        plan.digest(),
    ))
}

/// Deterministic identity scoped to one model attempt. A reopen binds a fresh
/// model whose round counter restarts at one while the session's committed
/// items can already carry identities the earlier process minted, and
/// history-derived lengths are not monotonic under compaction; the request's
/// generated attempt id is fresh for every model call, so it — not the
/// history shape — keeps a replayed round off the calls and entries the
/// canonical assembler refuses to duplicate.
fn loopback_identity(
    plan: &ValidatedRequestPlan,
    namespace: u128,
    round: u64,
    kind: &[u8],
) -> Result<Uuid, ModelError> {
    let attempt = plan.request().metadata().attempt_id().to_string();
    loopback_identity_for_attempt(&attempt, namespace, round, kind)
}

pub(super) fn loopback_identity_for_attempt(
    attempt: &str,
    namespace: u128,
    round: u64,
    kind: &[u8],
) -> Result<Uuid, ModelError> {
    let digest = plakous::model::sha256_hex(&[
        attempt.as_bytes(),
        &namespace.to_le_bytes(),
        &round.to_le_bytes(),
        kind,
    ]);
    Uuid::parse_str(&digest[..32]).map_err(invalid_model_fixture)
}

/// The terminal response one scripted round commits.
///
/// A round the script does not name commits the fixture's ordinary final text,
/// so a script only has to describe the rounds it cares about.
#[allow(
    clippy::too_many_lines,
    reason = "closed scripted output variants and response assembly"
)]
fn scripted_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    step: Option<&ScriptStep>,
    identity_namespace: u128,
    prepared_input_tokens: u64,
    hosted_tool_call_final: bool,
) -> Result<ModelResponse, ModelError> {
    let default_step = ScriptStep::default();
    let step = step.unwrap_or(&default_step);
    // Response identities must stay unique across processes, because a resumed
    // session replays the identities the previous process minted and the
    // canonical assembler refuses a duplicate tool call. The round counter
    // restarts at one in every process, so the identity is derived from the
    // request instead: its projected size, its input length and its round.
    // Sessions that share a namespace can still collide, because projected
    // size is a digest's JSON length and two digests can have the same
    // decimal width. `ScriptPlan::identity_namespace` separates those
    // sessions in the high 64 bits.
    let ordinal = u128::from(prepared_input_tokens)
        .saturating_mul(1 << 24)
        .saturating_add(
            u128::try_from(plan.request().input().items().len())
                .unwrap_or_default()
                .saturating_mul(1 << 8),
        )
        .saturating_add(u128::from(round));
    let call_identity = (identity_namespace << 64).saturating_add(ordinal.saturating_mul(2));
    let entry_identity = call_identity.saturating_add(1);
    let mut blocks = match &step.output {
        ScriptOutput::Empty => Vec::new(),
        ScriptOutput::HistoryReport => {
            let text = plan
                .request()
                .input()
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            vec![ContentBlock::Text(
                TextBlock::new(text, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
            )]
        }
        ScriptOutput::Images(count) => (0..*count)
            .map(|_| {
                plakous::model::MediaSource::bytes(vec![1; 3_000])
                    .and_then(|source| plakous::model::ImageBlock::new("image/png", source))
                    .map(ContentBlock::Image)
                    .map_err(invalid_model_fixture)
            })
            .collect::<Result<Vec<_>, _>>()?,
        ScriptOutput::ImageReport => {
            let images = plan
                .request()
                .input()
                .items()
                .iter()
                .filter(|item| item.actor() == Actor::User)
                .last()
                .map(|item| {
                    item.blocks()
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Image(image) => Some(format!(
                                "{} {} bytes",
                                image.media_type(),
                                image.source().as_bytes().map_or(0, <[u8]>::len)
                            )),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            vec![ContentBlock::Text(
                TextBlock::new(
                    format!("images seen: [{}]", images.join(", ")),
                    Some(TextPhase::Final),
                )
                .map_err(invalid_model_fixture)?,
            )]
        }
        ScriptOutput::FinalText => vec![ContentBlock::Text(
            TextBlock::new(FINAL_TEXT, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )],
        ScriptOutput::Text(text) => vec![ContentBlock::Text(
            TextBlock::new(text.clone(), Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )],
        ScriptOutput::ReasoningOnly => vec![ContentBlock::Reasoning(
            plakous::model::ReasoningBlock::new(
                plakous::model::ReasoningVisibility::Omitted,
                None,
                None,
            )
            .map_err(invalid_model_fixture)?,
        )],
        ScriptOutput::Tool(call) => vec![scripted_tool_call(
            call_identity,
            &call.name,
            serde_json::to_string(&resolved_arguments(&call.arguments, plan))
                .map_err(invalid_model_fixture)?,
            scripted_correlation(call.programmatic, call_identity)?,
        )?],
        // The first call keeps the single-call identity. The others step above
        // the 48-bit namespace (bits 64..112), so no ordinal of any round can
        // reach them.
        ScriptOutput::Tools(calls) => calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let step = u128::try_from(index)
                    .map_err(invalid_model_fixture)?
                    .saturating_mul(1 << 112);
                scripted_tool_call(
                    call_identity.saturating_add(step),
                    &call.name,
                    serde_json::to_string(&resolved_arguments(&call.arguments, plan))
                        .map_err(invalid_model_fixture)?,
                    scripted_correlation(call.programmatic, call_identity.saturating_add(step))?,
                )
            })
            .collect::<Result<_, _>>()?,
        // The exec marker's contract is `ToolInputContract::Utf8`, which is a
        // claim about the *decoded* value, not the wire bytes. The runtime
        // requires `call.arguments()` to parse to a JSON string and takes the
        // program from inside it (`runtime/composition/projection.rs`, "local
        // composition exec input is not a custom UTF-8 string"). Sending the
        // program as bare text is refused, because bare JavaScript is not
        // valid JSON.
        ScriptOutput::Cell(program) => vec![scripted_tool_call(
            call_identity,
            CODE_MODE_EXEC_NAME,
            serde_json::to_string(program).map_err(invalid_model_fixture)?,
            ToolCorrelation::default(),
        )?],
        ScriptOutput::Wait(wait) => {
            let mut arguments = serde_json::Map::new();
            arguments.insert(
                "cell_id".to_owned(),
                serde_json::Value::from(wait.cell_id.clone()),
            );
            if let Some(yield_time_ms) = wait.yield_time_ms {
                arguments.insert(
                    "yield_time_ms".to_owned(),
                    serde_json::Value::from(yield_time_ms),
                );
            }
            if wait.terminate {
                arguments.insert("terminate".to_owned(), serde_json::Value::from(true));
            }
            vec![scripted_tool_call(
                call_identity,
                CODE_MODE_WAIT_NAME,
                serde_json::to_string(&serde_json::Value::Object(arguments))
                    .map_err(invalid_model_fixture)?,
                ToolCorrelation::default(),
            )?]
        }
    };
    if hosted_tool_call_final
        && matches!(step.output, ScriptOutput::Text(_) | ScriptOutput::FinalText)
    {
        blocks.insert(0, scripted_hosted_call(call_identity)?);
        blocks.insert(1, scripted_hosted_result(call_identity)?);
    }
    let output = if blocks.is_empty() {
        Vec::new()
    } else {
        vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(entry_identity))
                    .map_err(invalid_model_fixture)?,
                Actor::Assistant,
                blocks,
                Provenance::ModelOutput,
            )
            .map_err(invalid_model_fixture)?,
        ]
    };
    Ok(ModelResponse::new(
        output,
        FinishDetails::new(
            match step.finish {
                ScriptFinish::Stop => FinishReason::Stop,
                ScriptFinish::ToolCalls => FinishReason::ToolCalls,
                ScriptFinish::AssistantContinuation => FinishReason::AssistantContinuation,
                ScriptFinish::Length => FinishReason::Length,
                ScriptFinish::Refusal => FinishReason::Refusal,
            },
            None,
        ),
        step.usage.map(|usage| {
            plakous::model::Usage::new(
                usage.input.unwrap_or(prepared_input_tokens),
                usage.output,
                usage.reasoning,
                usage.cached_input,
                usage.cache_write,
            )
        }),
        None,
        plan.digest(),
    ))
}

fn scripted_hosted_call(identity: u128) -> Result<ContentBlock, ModelError> {
    Ok(ContentBlock::ToolCall(
        ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(identity)).map_err(invalid_model_fixture)?,
            ToolName::new("hosted_interpreter").map_err(invalid_model_fixture)?,
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("hosted.interpreter")
                    .map_err(invalid_model_fixture)?,
            },
            ToolCallState::Completed,
            ToolCorrelation::default(),
            "{}",
        )
        .map_err(invalid_model_fixture)?,
    ))
}

fn scripted_hosted_result(identity: u128) -> Result<ContentBlock, ModelError> {
    Ok(ContentBlock::ToolResult(
        plakous::model::ToolResultBlock::new(
            CallId::try_from(Uuid::from_u128(identity)).map_err(invalid_model_fixture)?,
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("hosted.interpreter")
                    .map_err(invalid_model_fixture)?,
            },
            plakous::model::ToolResultKind::Success,
            vec![ToolResultPart::Text(
                TextBlock::new("hosted result", None).map_err(invalid_model_fixture)?,
            )],
            ToolCorrelation::default(),
        )
        .map_err(invalid_model_fixture)?,
    ))
}

/// Fills a scripted call's placeholders from the previous tool result.
///
/// A process session's id is minted by the run, so a script cannot write it
/// down in advance. `"$session_id"` and `"$cursor"` are replaced with what the
/// last process observation reported, which is the same substitution the
/// topology script does by hand for agent and candidate ids. Anything else is
/// passed through untouched, and a placeholder with nothing to fill it stays as
/// written so the refusal names it.
fn resolved_arguments(
    arguments: &serde_json::Value,
    plan: &ValidatedRequestPlan,
) -> serde_json::Value {
    fn fill(
        value: &serde_json::Value,
        process: Option<(u64, u64)>,
        plan: &ValidatedRequestPlan,
    ) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) if text == "$session_id" => process.map_or_else(
                || value.clone(),
                |(session_id, _)| serde_json::Value::from(session_id),
            ),
            serde_json::Value::String(text) if text == "$cursor" => process.map_or_else(
                || value.clone(),
                |(_, cursor)| serde_json::Value::from(cursor),
            ),
            // A tree script cannot restate ids the tree mints, so it names the
            // field it wants from the last tool result that carried one:
            // `$result.agent_id`, `$result.candidate`. An unresolved reference
            // stays as written, so the refusal names the placeholder rather
            // than a silently-empty string.
            serde_json::Value::String(text) => text.strip_prefix("$result.").map_or_else(
                || value.clone(),
                |field| {
                    tool_result_string(plan, field)
                        .map_or_else(|| value.clone(), serde_json::Value::from)
                },
            ),
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items.iter().map(|item| fill(item, process, plan)).collect(),
            ),
            serde_json::Value::Object(fields) => serde_json::Value::Object(
                fields
                    .iter()
                    .map(|(key, field)| (key.clone(), fill(field, process, plan)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    fill(arguments, process_session_projection(plan), plan)
}

/// The correlation of one scripted call: a provider program's call, so the
/// round is a hosted tool batch, or a direct one.
fn scripted_correlation(programmatic: bool, identity: u128) -> Result<ToolCorrelation, ModelError> {
    if !programmatic {
        return Ok(ToolCorrelation::default());
    }
    ToolCorrelation::new(
        Some(format!("offline-program-call-{identity:032x}")),
        Some("offline-program-container".to_owned()),
        plakous::model::ToolCallerLineage::provider_programmatic(
            NamespaceId::new("offline.program").map_err(invalid_model_fixture)?,
            "offline-program-execution",
        )
        .map_err(invalid_model_fixture)?,
    )
    .map_err(invalid_model_fixture)
}

/// One requested tool call with arguments already in their wire form.
fn scripted_tool_call(
    identity: u128,
    name: &str,
    arguments: String,
    correlation: ToolCorrelation,
) -> Result<ContentBlock, ModelError> {
    Ok(ContentBlock::ToolCall(
        ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(identity)).map_err(invalid_model_fixture)?,
            ToolName::new(name.to_owned()).map_err(invalid_model_fixture)?,
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            correlation,
            arguments,
        )
        .map_err(invalid_model_fixture)?,
    ))
}

/// The bounded summary one accepted scripted fold commits.
fn scripted_summary_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    identity_namespace: u128,
    finish: FinishReason,
) -> Result<ModelResponse, ModelError> {
    let output = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(
            (identity_namespace << 64)
                .saturating_add(60_000)
                .saturating_add(u128::from(round)),
        ))
        .map_err(invalid_model_fixture)?,
        Actor::Assistant,
        vec![ContentBlock::Text(
            TextBlock::new("older intent groups compacted", Some(TextPhase::Final))
                .map_err(invalid_model_fixture)?,
        )],
        Provenance::ModelOutput,
    )
    .map_err(invalid_model_fixture)?;
    Ok(ModelResponse::new(
        vec![output],
        FinishDetails::new(finish, None),
        None,
        None,
        plan.digest(),
    ))
}

pub(super) fn native_summary_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    identity_namespace: u128,
    descriptor: &ResolvedModelDescriptor,
    account: Digest,
) -> Result<ModelResponse, ModelError> {
    let native = plakous::model::CompactionBlock::new("native older context compacted")
        .map_err(invalid_model_fixture)?
        .with_native_item(
            r#"{"type":"compaction","encrypted_content":"offline-native-summary"}"#.into(),
        )
        .with_native_binding(plakous::model::CompactionReplayBinding::new(
            descriptor.protocol().clone(),
            descriptor.model_family().cloned(),
            descriptor.requested_model().clone(),
            account,
        ));
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(
            (identity_namespace << 64)
                .saturating_add(90_000)
                .saturating_add(u128::from(round)),
        ))
        .map_err(invalid_model_fixture)?,
        Actor::Assistant,
        vec![ContentBlock::Compaction(native)],
        Provenance::ModelOutput,
    )
    .map_err(invalid_model_fixture)?;
    Ok(ModelResponse::new(
        vec![item],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        plan.digest(),
    ))
}

/// The tree call for one round of [`LoopbackScenario::Topology`].
///
/// Round 1 spawns; rounds 2 and 3 read the previous round's tool result, so
/// the script names the exact agent the tree minted and the exact candidate it
/// sealed instead of assuming the tree's id shapes.
fn topology_call(
    round: u64,
    plan: &ValidatedRequestPlan,
) -> Result<(&'static str, serde_json::Value), ModelError> {
    match round {
        1 => Ok((
            "spawn_agent",
            json!({
                "template": "implement",
                "tier": "mutate",
                "workspace": "fork",
                "description": "offline parity child",
                "prompt": "make the offline parity change",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }),
        )),
        2 => {
            let agent_id = tool_result_string(plan, "agent_id").ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "topology fixture did not receive a spawned agent id",
                )
            })?;
            Ok(("wait_agent", json!({ "agent_ids": [agent_id] })))
        }
        _ => {
            let candidate = tool_result_string(plan, "candidate").ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "topology fixture did not receive a sealed candidate",
                )
            })?;
            Ok(("adopt_candidate", json!({ "candidate_id": candidate })))
        }
    }
}

/// The most recent tool result carrying `field`, anywhere in its JSON body.
fn tool_result_string(plan: &ValidatedRequestPlan, field: &str) -> Option<String> {
    plan.request()
        .input()
        .items()
        .iter()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|item| item.blocks().iter().rev())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(|result| result.parts().iter())
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => {
                serde_json::from_str::<serde_json::Value>(text.text()).ok()
            }
            _ => None,
        })
        .find_map(|value| first_string_field(&value, field))
}

/// Depth-first search for the first string at `field`, so the script does not
/// restate a tool output's envelope shape.
fn first_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(found)) = map.get(field) {
                return Some(found.clone());
            }
            map.values()
                .find_map(|nested| first_string_field(nested, field))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|nested| first_string_field(nested, field)),
        _ => None,
    }
}

fn process_session_projection(plan: &ValidatedRequestPlan) -> Option<(u64, u64)> {
    let items = plan.request().input().items().iter().collect::<Vec<_>>();
    items
        .into_iter()
        .rev()
        .flat_map(|item| item.blocks().iter().rev())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(|result| result.parts().iter())
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => {
                serde_json::from_str::<serde_json::Value>(text.text()).ok()
            }
            _ => None,
        })
        .find_map(|value| {
            Some((
                value.get("session_id")?.as_u64()?,
                value.get("cursor")?.as_u64()?,
            ))
        })
}

fn invalid_model_fixture(error: impl std::fmt::Display) -> ModelError {
    ModelError::new(
        ModelErrorKind::InvalidEvent,
        format!("invalid loopback fixture: {error}"),
    )
}
