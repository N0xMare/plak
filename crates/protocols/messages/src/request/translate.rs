use std::collections::{BTreeMap, BTreeSet};

use plakous_model::{
    Actor, CallId, CapabilityRequirement, ContentBlock, DataClassification, Digest, DigestDomain,
    EffectiveProviderConfigurationId, ModelDiagnostic, ModelId, OpaqueFallback, ReasoningEffort,
    ReasoningPolicy, ReasoningVisibility, ReplayEnvelope, ReplayScope,
    SelectedProviderConfigurationId, SemanticJsonV1, ToolCallBlock, ToolCallerDisposition,
    ToolCallerLineage, ToolChoice, ToolExecutionOwner, ToolResultKind, ToolResultPart,
    ToolResultUpdateBlock,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use super::{
    CODE_EXECUTION_RESULT_REPLAY_KIND, CacheControlParam, MESSAGES_REPLAY_SCHEMA_REVISION,
    MessageBlock, MessageParam, MessageRole, MessagesRequestError, MessagesRequestPolicy,
    MessagesThinkingDisplay, OutputConfigParam, PendingTool, REDACTED_REPLAY_KIND,
    SIGNATURE_REPLAY_KIND, ThinkingParam, ToolCallerParam, ToolChoiceParam, ToolOwnership,
    ToolResultContent, image_source, invalid, native_compaction_refusal, unsupported,
};

type TranslatedHistory = (Vec<MessageParam>, Option<String>, Vec<ModelDiagnostic>);

pub(super) fn translate_history(
    input: &plakous_model::ConversationView,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    include_native_thinking: bool,
    replay_diagnostics: &mut Vec<String>,
) -> Result<TranslatedHistory, MessagesRequestError> {
    let mut messages: Vec<MessageParam> = Vec::new();
    let mut historical_projection_diagnostics = Vec::new();
    let items = input.items();
    let completed_items = match policy.completed_history_prefix {
        Some(prefix)
            if input.semantic_prefix_identity(prefix.item_count()).ok() == Some(prefix) =>
        {
            prefix.item_count()
        }
        Some(_) => {
            return Err(invalid(
                "completed history prefix does not match this request",
            ));
        }
        None => 0,
    };
    let mut index = 0;
    let mut pending = Vec::<PendingTool>::new();
    let mut container = None;
    let mut server_lineage = crate::artifact::MessagesArtifactLineage::default();
    while index < items.len() {
        let item = &items[index];
        match item.actor() {
            Actor::Assistant => {
                let extending = !pending.is_empty();
                let (blocks, calls) = translate_assistant_with_history(
                    item.blocks(),
                    policy,
                    ownership,
                    AssistantProjection {
                        programmatic,
                        include_native_thinking,
                        completed_turn: index < completed_items,
                    },
                    AssistantReplayState {
                        container: &mut container,
                        server_lineage: &mut server_lineage,
                        diagnostics: replay_diagnostics,
                    },
                    &mut historical_projection_diagnostics,
                )?;
                if extending {
                    // A cancelled batch records each tool call as its own
                    // assistant item, with the results after all of them.
                    // Only further `tool_use` blocks fold into the open
                    // assistant message: thinking must lead that message, and
                    // text or server-tool output in front of unfinished
                    // results is not a batch.
                    let only_tool_use = blocks
                        .iter()
                        .all(|block| matches!(block, MessageBlock::ToolUse { .. }));
                    if !only_tool_use || calls.is_empty() {
                        return Err(invalid(
                            "assistant output intervened before all tool results",
                        ));
                    }
                    match messages.last_mut() {
                        Some(message) if message.role == MessageRole::Assistant => {
                            message.content.extend(blocks);
                        }
                        _ => {
                            return Err(invalid(
                                "assistant output intervened before all tool results",
                            ));
                        }
                    }
                    pending.extend(calls);
                } else {
                    if !blocks.is_empty() {
                        push_message(&mut messages, MessageRole::Assistant, blocks);
                    }
                    pending = calls;
                }
                index += 1;
            }
            Actor::Tool => {
                let result_blocks = translate_tool_messages(items, &mut index, &mut pending)?;
                push_message(&mut messages, MessageRole::User, result_blocks);
            }
            Actor::User => {
                if !pending.is_empty() {
                    return Err(invalid("user content intervened before tool results"));
                }
                push_message(
                    &mut messages,
                    MessageRole::User,
                    translate_user(item.blocks())?,
                );
                index += 1;
            }
            Actor::System | Actor::Developer | Actor::Harness => {
                return Err(unsupported(
                    "historical instruction actors require an admitted native capability",
                ));
            }
            _ => {
                return Err(unsupported(
                    "unknown canonical actor cannot be translated safely",
                ));
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid("history ends before required tool results"));
    }
    Ok((messages, container, historical_projection_diagnostics))
}

fn translate_tool_messages(
    items: plakous_model::ConversationItems<'_>,
    index: &mut usize,
    pending: &mut Vec<PendingTool>,
) -> Result<Vec<MessageBlock>, MessagesRequestError> {
    if pending.is_empty() {
        return Err(invalid("tool result has no immediately preceding tool use"));
    }
    let mut result_blocks = Vec::new();
    let mut updates = BTreeMap::<CallId, Vec<&ToolResultUpdateBlock>>::new();
    let mut terminated = BTreeSet::<CallId>::new();
    while *index < items.len() && matches!(items[*index].actor(), Actor::Tool) {
        for block in items[*index].blocks() {
            match block {
                ContentBlock::ToolResultUpdate(update) => {
                    if !pending.iter().any(|tool| tool.call_id == update.call_id()) {
                        return Err(invalid("tool-result update has no pending tool use"));
                    }
                    if terminated.contains(&update.call_id()) {
                        return Err(invalid("tool-result update followed its terminal result"));
                    }
                    updates.entry(update.call_id()).or_default().push(update);
                }
                ContentBlock::ToolResult(result) => {
                    let expected = pending
                        .get(result_blocks.len())
                        .ok_or_else(|| invalid("too many tool results"))?;
                    if result.call_id() != expected.call_id {
                        return Err(invalid("parallel tool results are not in source order"));
                    }
                    let result_updates = updates.remove(&result.call_id()).unwrap_or_default();
                    result_blocks.push(translate_tool_result(result, expected, &result_updates)?);
                    terminated.insert(result.call_id());
                }
                _ => {
                    return Err(invalid(
                        "Tool actor may contain only tool results or updates",
                    ));
                }
            }
        }
        *index += 1;
    }
    if !updates.is_empty() {
        return Err(invalid("tool-result update has no terminal result"));
    }
    if result_blocks.len() != pending.len() {
        return Err(invalid(
            "all parallel tool results must be returned together",
        ));
    }
    if *index < items.len() && matches!(items[*index].actor(), Actor::User) {
        if pending
            .iter()
            .any(|tool| !matches!(tool.correlation.caller_lineage(), ToolCallerLineage::Direct))
        {
            return Err(invalid(
                "programmatic continuation user message must contain only tool results",
            ));
        }
        result_blocks.extend(translate_user(items[*index].blocks())?);
        *index += 1;
    }
    pending.clear();
    Ok(result_blocks)
}

pub(super) fn text_block(text: impl Into<String>) -> MessageBlock {
    MessageBlock::Text {
        text: text.into(),
        cache_control: None,
    }
}

fn tool_result_text(text: impl Into<String>) -> ToolResultContent {
    ToolResultContent::Text {
        text: text.into(),
        cache_control: None,
    }
}

pub(super) fn mark_last_message_cache_breakpoint(
    messages: &mut [MessageParam],
    cache: CacheControlParam,
) {
    for message in messages.iter_mut().rev() {
        for block in message.content.iter_mut().rev() {
            match block {
                MessageBlock::Text { cache_control, .. }
                | MessageBlock::Image { cache_control, .. } => {
                    *cache_control = Some(cache);
                    return;
                }
                MessageBlock::ToolResult { content, .. } => {
                    if let Some(
                        ToolResultContent::Text { cache_control, .. }
                        | ToolResultContent::Image { cache_control, .. },
                    ) = content.last_mut()
                    {
                        *cache_control = Some(cache);
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

pub(super) fn push_message(
    messages: &mut Vec<MessageParam>,
    role: MessageRole,
    mut content: Vec<MessageBlock>,
) {
    if let Some(last) = messages.last_mut().filter(|message| message.role == role) {
        last.content.append(&mut content);
    } else {
        messages.push(MessageParam { role, content });
    }
}

fn portable_reasoning_text(reasoning: &plakous_model::ReasoningBlock) -> Option<MessageBlock> {
    reasoning
        .text()
        .filter(|text| !text.is_empty())
        .map(text_block)
}

/// Reasoning that cannot be replayed natively, under a policy that forbids
/// sending it as text.
///
/// Anthropic replays a thinking block exactly with its signature and a redacted
/// block opaque. Reasoning has no third form on this wire: handing the model's
/// own chain of thought back as assistant text is what a resumed Claude Opus 5
/// turn read as `reasoning_extraction` and refused. When the signature is not
/// there to send, the block is dropped, not translated.
fn unreplayable_thinking(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    reason: &str,
    diagnose_when_lenient: bool,
    replay_diagnostics: &mut Vec<String>,
) -> Option<MessageBlock> {
    if policy.strict_thinking_replay {
        replay_diagnostics.push(format!(
            "{reason}; the block was dropped, never sent as text"
        ));
        return None;
    }
    if diagnose_when_lenient {
        replay_diagnostics.push(reason.to_owned());
    }
    portable_reasoning_text(reasoning)
}

fn native_or_portable_thinking(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    kind: &str,
    visible: bool,
    replay_diagnostics: &mut Vec<String>,
) -> Option<MessageBlock> {
    let Some(replay) = reasoning.replay() else {
        return unreplayable_thinking(
            reasoning,
            policy,
            "Messages thinking carried no replay signature",
            false,
            replay_diagnostics,
        );
    };
    if replay.protocol() != &policy.protocol {
        return unreplayable_thinking(
            reasoning,
            policy,
            "incompatible Messages thinking replay was stripped",
            true,
            replay_diagnostics,
        );
    }
    let Ok(payload) = replay_payload(replay, policy, kind) else {
        return unreplayable_thinking(
            reasoning,
            policy,
            "incompatible Messages thinking replay was stripped",
            true,
            replay_diagnostics,
        );
    };
    if matches!(
        serde_json::from_str::<String>(payload.get()).as_deref(),
        Ok("")
    ) {
        return unreplayable_thinking(
            reasoning,
            policy,
            "empty Messages thinking replay was stripped",
            true,
            replay_diagnostics,
        );
    }
    Some(if visible {
        MessageBlock::Thinking {
            thinking: reasoning.text().unwrap_or_default().to_owned(),
            signature: payload,
        }
    } else {
        MessageBlock::RedactedThinking { data: payload }
    })
}

fn translate_reasoning_block(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    replay_diagnostics: &mut Vec<String>,
) -> Option<MessageBlock> {
    match reasoning.visibility() {
        ReasoningVisibility::Visible | ReasoningVisibility::Omitted => native_or_portable_thinking(
            reasoning,
            policy,
            SIGNATURE_REPLAY_KIND,
            true,
            replay_diagnostics,
        ),
        ReasoningVisibility::Redacted => native_or_portable_thinking(
            reasoning,
            policy,
            REDACTED_REPLAY_KIND,
            false,
            replay_diagnostics,
        ),
        ReasoningVisibility::OpaqueReplay => None,
        // A summary, or any visibility this wire has no native form for, is
        // still reasoning. It is dropped rather than flattened into text.
        _ => unreplayable_thinking(
            reasoning,
            policy,
            "Messages has no native form for this reasoning visibility",
            false,
            replay_diagnostics,
        ),
    }
}

pub(crate) struct AssistantReplayState<'a> {
    pub(crate) container: &'a mut Option<String>,
    pub(crate) server_lineage: &'a mut crate::artifact::MessagesArtifactLineage,
    pub(crate) diagnostics: &'a mut Vec<String>,
}

pub(crate) fn translate_assistant_bound(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    include_native_thinking: bool,
    replay: AssistantReplayState<'_>,
) -> Result<(Vec<MessageBlock>, Vec<PendingTool>), MessagesRequestError> {
    translate_assistant_with_history(
        blocks,
        policy,
        ownership,
        AssistantProjection {
            programmatic,
            include_native_thinking,
            completed_turn: false,
        },
        replay,
        &mut Vec::new(),
    )
}

#[derive(Clone, Copy)]
struct AssistantProjection {
    programmatic: bool,
    include_native_thinking: bool,
    completed_turn: bool,
}

fn translate_assistant_with_history(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    projection: AssistantProjection,
    replay: AssistantReplayState<'_>,
    historical_projection_diagnostics: &mut Vec<ModelDiagnostic>,
) -> Result<(Vec<MessageBlock>, Vec<PendingTool>), MessagesRequestError> {
    let AssistantProjection {
        programmatic,
        include_native_thinking,
        completed_turn,
    } = projection;
    let AssistantReplayState {
        container,
        server_lineage,
        diagnostics: replay_diagnostics,
    } = replay;
    let mut output = Vec::with_capacity(blocks.len());
    let mut pending = Vec::new();
    let mut remote_ids = BTreeSet::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => output.push(text_block(text.text().to_owned())),
            ContentBlock::Reasoning(reasoning) => {
                if project_completed_thinking(
                    reasoning,
                    policy,
                    completed_turn,
                    include_native_thinking,
                    historical_projection_diagnostics,
                )? {
                    continue;
                }
                if include_native_thinking
                    && let Some(block) =
                        translate_reasoning_block(reasoning, policy, replay_diagnostics)
                {
                    output.push(block);
                }
            }
            ContentBlock::ToolCall(call) => {
                let (block, tool) = translate_assistant_tool_call(
                    call,
                    policy,
                    ownership,
                    programmatic,
                    container,
                    server_lineage,
                    &mut remote_ids,
                )?;
                output.push(block);
                pending.extend(tool);
            }
            ContentBlock::Opaque(opaque)
                if opaque.replay().protocol() == &policy.protocol
                    && opaque.replay().kind().as_str() == CODE_EXECUTION_RESULT_REPLAY_KIND =>
            {
                output.push(translate_hosted_result(
                    opaque,
                    policy,
                    programmatic,
                    server_lineage,
                )?);
            }
            ContentBlock::Opaque(opaque) => match opaque.fallback() {
                OpaqueFallback::SemanticSummary(summary) => {
                    output.push(text_block(summary.clone()));
                }
                OpaqueFallback::Omit | OpaqueFallback::CompatibleReplay => {}
                _ => {
                    return Err(unsupported(
                        "assistant content block is outside the Messages profile",
                    ));
                }
            },
            ContentBlock::Citation(_) => {}
            ContentBlock::Refusal(refusal) => output.push(text_block(refusal.text().to_owned())),
            ContentBlock::Compaction(compaction) if compaction.native_item().is_some() => {
                return Err(native_compaction_refusal());
            }
            ContentBlock::Compaction(compaction) => {
                output.push(text_block(compaction.summary().to_owned()));
            }
            _ => {
                return Err(unsupported(
                    "assistant content block is outside the Messages profile",
                ));
            }
        }
    }
    Ok((output, pending))
}

fn translate_hosted_result(
    opaque: &plakous_model::OpaqueBlock,
    policy: &MessagesRequestPolicy,
    programmatic: bool,
    server_lineage: &mut crate::artifact::MessagesArtifactLineage,
) -> Result<MessageBlock, MessagesRequestError> {
    if !programmatic {
        return Err(invalid("hosted result replay is not admitted"));
    }
    let raw = replay_payload(opaque.replay(), policy, CODE_EXECUTION_RESULT_REPLAY_KIND)?;
    let value: Value =
        serde_json::from_str(raw.get()).map_err(|_| invalid("invalid hosted result replay"))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("hosted result replay must be an object"))?;
    let tool_use_id = object
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| server_lineage.is_unresolved(id))
        .ok_or_else(|| invalid("hosted result does not match a server tool"))?;
    let content = object
        .get("content")
        .cloned()
        .ok_or_else(|| invalid("hosted result requires content"))?;
    let block = MessageBlock::CodeExecutionToolResult {
        tool_use_id: tool_use_id.into(),
        content,
    };
    server_lineage
        .complete_next(tool_use_id)
        .map_err(|_| invalid("hosted results did not follow exact server-tool order"))?;
    Ok(block)
}

fn project_completed_thinking(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    completed_turn: bool,
    include_native_thinking: bool,
    diagnostics: &mut Vec<ModelDiagnostic>,
) -> Result<bool, MessagesRequestError> {
    if !policy.strict_thinking_replay {
        return Ok(false);
    }
    let Some(replay) = reasoning.replay() else {
        if !include_native_thinking {
            return Ok(false);
        }
        if completed_turn {
            record_historical_projection(
                unsigned_historical_projection_identity(policy)?,
                "Completed historical thinking without replay was omitted; conversation content was retained. No private thinking or signatures were sent as text.",
                diagnostics,
            );
        }
        return Ok(completed_turn);
    };
    let kind = if reasoning.visibility() == ReasoningVisibility::Redacted {
        REDACTED_REPLAY_KIND
    } else {
        SIGNATURE_REPLAY_KIND
    };
    if completed_turn && projectable_historical_thinking(replay, policy, kind) {
        if include_native_thinking {
            record_historical_projection(
                historical_projection_identity(replay, policy)?,
                "Configuration transition: completed historical thinking was omitted; conversation content was retained. No private thinking or signatures were sent as text.",
                diagnostics,
            );
        }
        return Ok(true);
    }
    replay_payload(replay, policy, kind)?;
    Ok(false)
}

fn translate_assistant_tool_call(
    call: &ToolCallBlock,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    container: &mut Option<String>,
    server_lineage: &mut crate::artifact::MessagesArtifactLineage,
    remote_ids: &mut BTreeSet<String>,
) -> Result<(MessageBlock, Option<PendingTool>), MessagesRequestError> {
    // Messages `tool_use.input` is always an object. A structured call is one
    // already. A UTF-8 cell (Codex Code Mode `exec`) is canonical as a JSON
    // string literal, and a call whose text never parsed as JSON has no
    // parsed value at all; both replay truthfully as `{"input": <text>}` so a
    // rebind onto Messages re-projects the prefix instead of
    // refusing it.
    let input = match call.arguments() {
        Some(Value::Object(_)) => call
            .arguments()
            .cloned()
            .unwrap_or_else(|| unreachable!("matched a parsed object")),
        Some(other) => serde_json::json!({ "input": other }),
        None => serde_json::json!({ "input": call.raw_arguments() }),
    };
    let remote_id = call
        .correlation()
        .remote_call_id()
        .ok_or_else(|| invalid("tool call requires exact remote correlation"))?
        .to_owned();
    if !remote_ids.insert(remote_id.clone()) {
        return Err(invalid("duplicate provider tool-use ID"));
    }
    if matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. }) {
        if !programmatic
            || call.exposed_name().as_str() != "code_execution"
            || !matches!(
                call.correlation().caller_lineage(),
                ToolCallerLineage::Direct
            )
            || call.correlation().container_id().is_some()
        {
            return Err(invalid("incompatible provider-hosted Messages tool call"));
        }
        server_lineage
            .push_server(&remote_id)
            .map_err(|_| invalid("duplicate provider server-tool ID"))?;
        return Ok((
            MessageBlock::ServerToolUse {
                id: remote_id,
                name: "code_execution".into(),
                input,
            },
            None,
        ));
    }
    let caller = translate_tool_caller(
        call,
        policy,
        ownership,
        programmatic,
        container,
        server_lineage,
    )?;
    Ok((
        MessageBlock::ToolUse {
            id: remote_id.clone(),
            name: call.exposed_name().as_str().to_owned(),
            input,
            caller,
        },
        Some(PendingTool {
            call_id: call.call_id(),
            remote_id,
            owner: call.owner().clone(),
            correlation: call.correlation().clone(),
        }),
    ))
}

fn translate_tool_caller(
    call: &ToolCallBlock,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    container: &mut Option<String>,
    server_lineage: &crate::artifact::MessagesArtifactLineage,
) -> Result<Option<ToolCallerParam>, MessagesRequestError> {
    Ok(match call.correlation().caller_lineage() {
        ToolCallerLineage::Direct => {
            if call.correlation().container_id().is_some() {
                return Err(invalid("direct tool replay cannot invent a container"));
            }
            None
        }
        ToolCallerLineage::ProviderProgrammatic { kind, execution_id } => {
            let Some(caller_policy) = policy.programmatic_tool_caller.as_ref() else {
                return Err(invalid(
                    "programmatic caller is not admitted by this policy",
                ));
            };
            if !programmatic
                || kind != &caller_policy.namespace
                || !server_lineage.is_unresolved(execution_id)
            {
                return Err(invalid(
                    "programmatic caller does not match a preceding server tool",
                ));
            }
            if !matches!(
                ownership.caller(call.exposed_name()),
                Some(
                    ToolCallerDisposition::ProviderProgrammaticOnly
                        | ToolCallerDisposition::DirectAndProviderProgrammatic
                )
            ) {
                return Err(invalid("tool is not eligible for programmatic callers"));
            }
            let call_container = call
                .correlation()
                .container_id()
                .ok_or_else(|| invalid("programmatic tool call requires a container"))?;
            match container {
                Some(existing) if existing != call_container => {
                    return Err(invalid(
                        "programmatic tool calls used mismatched containers",
                    ));
                }
                Some(_) => {}
                None => *container = Some(call_container.to_owned()),
            }
            Some(ToolCallerParam {
                kind: caller_policy.response_caller.clone(),
                tool_id: Some(execution_id.clone()),
            })
        }
        ToolCallerLineage::LocalProgrammatic { .. } => {
            return Err(invalid(
                "local programmatic caller cannot be translated to the Messages provider wire format",
            ));
        }
    })
}

#[cfg(test)]
fn translate_assistant(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
) -> Result<(Vec<MessageBlock>, Vec<PendingTool>), MessagesRequestError> {
    translate_assistant_bound(
        blocks,
        policy,
        &ToolOwnership::default(),
        false,
        true,
        AssistantReplayState {
            container: &mut None,
            server_lineage: &mut crate::artifact::MessagesArtifactLineage::default(),
            diagnostics: &mut Vec::new(),
        },
    )
}

fn translate_user(blocks: &[ContentBlock]) -> Result<Vec<MessageBlock>, MessagesRequestError> {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => Ok(text_block(text.text().to_owned())),
            ContentBlock::Image(image) => Ok(MessageBlock::Image {
                source: image_source(image)?,
                cache_control: None,
            }),
            _ => Err(unsupported(
                "user content block is outside the Messages profile",
            )),
        })
        .collect()
}

fn translate_tool_result(
    result: &plakous_model::ToolResultBlock,
    expected: &PendingTool,
    updates: &[&ToolResultUpdateBlock],
) -> Result<MessageBlock, MessagesRequestError> {
    if matches!(result.owner(), ToolExecutionOwner::ProviderHosted { .. }) {
        return Err(unsupported(
            "provider-hosted results cannot be sent as client tool results",
        ));
    }
    if result.owner() != &expected.owner {
        return Err(invalid(
            "tool-result execution owner does not match the assistant tool call",
        ));
    }
    if result.correlation().remote_call_id() != Some(expected.remote_id.as_str()) {
        return Err(invalid(
            "tool-result remote correlation does not match the assistant tool call",
        ));
    }
    if result.correlation() != &expected.correlation {
        return Err(invalid(
            "tool-result caller/container correlation does not match the tool call",
        ));
    }
    let mut content = updates
        .iter()
        .map(|update| {
            if update.owner() != result.owner() || update.correlation() != result.correlation() {
                return Err(invalid(
                    "tool-result update correlation does not match terminal result",
                ));
            }
            Ok(tool_result_text(update.text().text().to_owned()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    content.extend(
        result
            .parts()
            .iter()
            .map(|part| match part {
                ToolResultPart::Text(text) => Ok(tool_result_text(text.text().to_owned())),
                ToolResultPart::Image(image)
                    if matches!(
                        expected.correlation.caller_lineage(),
                        ToolCallerLineage::Direct
                    ) =>
                {
                    Ok(ToolResultContent::Image {
                        source: image_source(image)?,
                        cache_control: None,
                    })
                }
                ToolResultPart::Audio(audio)
                    if matches!(
                        expected.correlation.caller_lineage(),
                        ToolCallerLineage::Direct
                    ) =>
                {
                    Ok(tool_result_text(format!(
                        "[audio result: {}]",
                        audio.media_type()
                    )))
                }
                _ => Err(unsupported("tool result is outside the Messages profile")),
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    Ok(MessageBlock::ToolResult {
        tool_use_id: expected.remote_id.clone(),
        content,
        is_error: matches!(result.kind(), ToolResultKind::Failure),
    })
}

/// A completed assistant turn may lose its native thinking on a rebind. The
/// source envelope still has to be internally well formed for this Messages
/// route; none of its opaque bytes are sent when the target binding changes.
fn projectable_historical_thinking(
    replay: &ReplayEnvelope,
    policy: &MessagesRequestPolicy,
    expected_kind: &str,
) -> bool {
    let ReplayScope::ExactModel(source_model) = replay.scope() else {
        return false;
    };
    if replay.protocol() != &policy.protocol
        || replay.model_family().is_some()
        || replay.kind().as_str() != expected_kind
        || replay.classification() != DataClassification::Sensitive
        || replay.version() != MESSAGES_REPLAY_SCHEMA_REVISION
    {
        return false;
    }
    let Ok(bound) = serde_json::from_str::<BoundReplayPayload>(replay.payload().get()) else {
        return false;
    };
    bound.actual_model == *source_model
        && bound.replay_schema_revision == MESSAGES_REPLAY_SCHEMA_REVISION
        && serde_json::from_str::<String>(bound.value.get()).is_ok()
        && (bound.requested_model != policy.requested_model
            || bound.semantic_descriptor_binding != policy.semantic_descriptor_binding
            || bound.account_binding != policy.account_binding
            || bound.selected_provider_configuration != policy.selected_provider_configuration
            || bound.effective_provider_configuration != policy.effective_provider_configuration
            || bound.api_version != policy.api_version
            || bound.beta_header != policy.beta_header
            || policy
                .expected_reported_model
                .as_ref()
                .is_some_and(|expected| expected != source_model))
}

fn record_historical_projection(
    identity: Digest,
    message: &str,
    diagnostics: &mut Vec<ModelDiagnostic>,
) {
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.history_projection_identity() == Some(identity))
    {
        return;
    }
    diagnostics.push(ModelDiagnostic::historical_projection(identity, message));
}

// Only compatibility facts identify the transition. In particular, signatures,
// reasoning, freshness bindings and the growing request projection do not.
fn unsigned_historical_projection_identity(
    policy: &MessagesRequestPolicy,
) -> Result<Digest, MessagesRequestError> {
    let facts = serde_json::to_value((
        &policy.protocol,
        &policy.requested_model,
        &policy.expected_reported_model,
        policy.semantic_descriptor_binding,
        policy.account_binding,
        policy.selected_provider_configuration,
        policy.effective_provider_configuration,
        MESSAGES_REPLAY_SCHEMA_REVISION,
        &policy.api_version,
        &policy.beta_header,
    ))
    .map_err(|_| invalid("failed to encode historical projection transition"))?;
    let canonical = SemanticJsonV1::from_value(facts)
        .map_err(|_| invalid("failed to canonicalize historical projection transition"))?;
    Digest::sha256(
        DigestDomain::HistoryProjection,
        1,
        &[
            b"messages-historical-thinking-without-replay",
            canonical.canonical_bytes(),
        ],
    )
    .map_err(|_| invalid("failed to digest historical projection transition"))
}

fn historical_projection_identity(
    replay: &ReplayEnvelope,
    policy: &MessagesRequestPolicy,
) -> Result<Digest, MessagesRequestError> {
    let bound: BoundReplayPayload = serde_json::from_str(replay.payload().get())
        .map_err(|_| invalid("opaque replay binding was malformed"))?;
    let facts = serde_json::to_value((
        (
            replay.protocol(),
            &bound.requested_model,
            &bound.actual_model,
            bound.semantic_descriptor_binding,
            bound.account_binding,
            bound.selected_provider_configuration,
            bound.effective_provider_configuration,
            bound.replay_schema_revision,
            &bound.api_version,
            &bound.beta_header,
        ),
        (
            &policy.protocol,
            &policy.requested_model,
            &policy.expected_reported_model,
            policy.semantic_descriptor_binding,
            policy.account_binding,
            policy.selected_provider_configuration,
            policy.effective_provider_configuration,
            MESSAGES_REPLAY_SCHEMA_REVISION,
            &policy.api_version,
            &policy.beta_header,
        ),
    ))
    .map_err(|_| invalid("failed to encode historical projection transition"))?;
    let canonical = SemanticJsonV1::from_value(facts)
        .map_err(|_| invalid("failed to canonicalize historical projection transition"))?;
    Digest::sha256(
        DigestDomain::HistoryProjection,
        1,
        &[
            b"messages-historical-thinking-transition",
            canonical.canonical_bytes(),
        ],
    )
    .map_err(|_| invalid("failed to digest historical projection transition"))
}

fn replay_payload(
    replay: &ReplayEnvelope,
    policy: &MessagesRequestPolicy,
    expected_kind: &str,
) -> Result<Box<RawValue>, MessagesRequestError> {
    let ReplayScope::ExactModel(source_model) = replay.scope() else {
        return Err(invalid("opaque thinking replay must use exact-model scope"));
    };
    if replay.protocol() != &policy.protocol {
        return Err(invalid(
            "opaque thinking replay protocol does not match Messages",
        ));
    }
    if replay.model_family().is_some()
        || replay.kind().as_str() != expected_kind
        || replay.classification() != DataClassification::Sensitive
    {
        return Err(invalid(
            "opaque thinking replay is incompatible with the exact model",
        ));
    }
    let payload = match replay.version() {
        MESSAGES_REPLAY_SCHEMA_REVISION => {
            let bound: BoundReplayPayload = serde_json::from_str(replay.payload().get())
                .map_err(|_| invalid("opaque replay binding was malformed"))?;
            if let Some(category) = replay_binding_mismatch(&bound, source_model, policy) {
                return Err(invalid(format!(
                    "opaque replay target binding mismatch: {category}"
                )));
            }
            bound.value
        }
        _ => {
            return Err(invalid(
                "opaque replay used an unsupported envelope version",
            ));
        }
    };
    if matches!(expected_kind, SIGNATURE_REPLAY_KIND | REDACTED_REPLAY_KIND) {
        serde_json::from_str::<String>(payload.get())
            .map_err(|_| invalid("opaque thinking replay payload must be an exact JSON string"))?;
    }
    Ok(payload)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BoundReplayPayload {
    requested_model: ModelId,
    actual_model: ModelId,
    semantic_descriptor_binding: Digest,
    account_binding: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    replay_schema_revision: u32,
    api_version: String,
    beta_header: String,
    value: Box<RawValue>,
}

fn replay_binding_mismatch(
    bound: &BoundReplayPayload,
    source_model: &ModelId,
    policy: &MessagesRequestPolicy,
) -> Option<&'static str> {
    // Categories only: never include the rejected value or envelope.
    [
        (
            bound.requested_model != policy.requested_model,
            "requested model",
        ),
        (
            &bound.actual_model != source_model
                || policy
                    .expected_reported_model
                    .as_ref()
                    .is_some_and(|expected| expected != source_model),
            "reported model",
        ),
        (
            bound.semantic_descriptor_binding != policy.semantic_descriptor_binding,
            "semantic descriptor",
        ),
        (bound.account_binding != policy.account_binding, "account"),
        (
            bound.selected_provider_configuration != policy.selected_provider_configuration,
            "selected configuration",
        ),
        (
            bound.effective_provider_configuration != policy.effective_provider_configuration,
            "effective configuration",
        ),
        (
            bound.replay_schema_revision != MESSAGES_REPLAY_SCHEMA_REVISION,
            "replay schema",
        ),
        (bound.api_version != policy.api_version, "API version"),
        (bound.beta_header != policy.beta_header, "beta set"),
    ]
    .into_iter()
    .find_map(|(mismatch, category)| mismatch.then_some(category))
}

/// Binds one decoded opaque replay value to the exact Messages target.
pub fn bind_replay_payload(
    policy: &MessagesRequestPolicy,
    actual_model: ModelId,
    kind: &str,
    payload: Box<RawValue>,
) -> Result<ReplayEnvelope, MessagesRequestError> {
    if !matches!(
        kind,
        SIGNATURE_REPLAY_KIND | REDACTED_REPLAY_KIND | CODE_EXECUTION_RESULT_REPLAY_KIND
    ) || policy
        .expected_reported_model
        .as_ref()
        .is_some_and(|expected| expected != &actual_model)
    {
        return Err(invalid(
            "opaque replay does not match the exact target binding",
        ));
    }
    if matches!(kind, SIGNATURE_REPLAY_KIND | REDACTED_REPLAY_KIND) {
        serde_json::from_str::<String>(payload.get())
            .map_err(|_| invalid("opaque replay payload was not an exact JSON string"))?;
    } else if !serde_json::from_str::<Value>(payload.get()).is_ok_and(|value| value.is_object()) {
        return Err(invalid(
            "hosted replay payload was not an exact JSON object",
        ));
    }
    // HistorySegment freeze JCS-canonicalizes nested replay JSON. Hash the
    // envelope over those same bytes or checkpoint decode reports
    // InvalidModelComponent (struct-field order ≠ JCS key order).
    let bound = serde_json::to_value(&BoundReplayPayload {
        requested_model: policy.requested_model.clone(),
        actual_model: actual_model.clone(),
        semantic_descriptor_binding: policy.semantic_descriptor_binding,
        account_binding: policy.account_binding,
        selected_provider_configuration: policy.selected_provider_configuration,
        effective_provider_configuration: policy.effective_provider_configuration,
        replay_schema_revision: MESSAGES_REPLAY_SCHEMA_REVISION,
        api_version: policy.api_version.clone(),
        beta_header: policy.beta_header.clone(),
        value: payload,
    })
    .map_err(|e| invalid(format!("failed to bind replay: {e}")))?;
    let canonical = SemanticJsonV1::from_value(bound)
        .map_err(|e| invalid(format!("failed to canonicalize replay binding: {e}")))?;
    let payload = RawValue::from_string(
        std::str::from_utf8(canonical.canonical_bytes())
            .map_err(|e| invalid(format!("failed to bind replay: {e}")))?
            .to_owned(),
    )
    .map_err(|e| invalid(format!("failed to bind replay: {e}")))?;
    ReplayEnvelope::new(
        policy.protocol.clone(),
        None,
        ReplayScope::ExactModel(actual_model),
        plakous_model::ReplayKindId::new(kind).map_err(|e| invalid(e.to_string()))?,
        MESSAGES_REPLAY_SCHEMA_REVISION,
        DataClassification::Sensitive,
        payload,
    )
    .map_err(|e| invalid(e.to_string()))
}

pub(super) fn translate_reasoning(
    policy: ReasoningPolicy,
    minimum: Option<u32>,
    max_tokens: u32,
    omit_disabled: bool,
    display: Option<MessagesThinkingDisplay>,
) -> Result<(Option<ThinkingParam>, Option<OutputConfigParam>), MessagesRequestError> {
    match policy {
        ReasoningPolicy::Disabled if omit_disabled => Ok((None, None)),
        ReasoningPolicy::Disabled => Ok((Some(ThinkingParam::Disabled), None)),
        ReasoningPolicy::Enabled {
            effort,
            requirement,
        } => {
            if let Some(ReasoningEffort::TokenBudget(budget)) = effort {
                let Some(minimum) = minimum else {
                    return Err(match requirement {
                        CapabilityRequirement::Required => {
                            unsupported("exact model does not support manual thinking")
                        }
                        _ => unsupported("thinking cannot be silently downgraded during lowering"),
                    });
                };
                let budget = budget.get();
                if budget < minimum || budget >= max_tokens {
                    return Err(invalid(
                        "thinking budget must meet the model minimum and be below max_tokens",
                    ));
                }
                return Ok((
                    Some(ThinkingParam::Enabled {
                        budget_tokens: budget,
                        display,
                    }),
                    None,
                ));
            }
            let effort_name = match effort {
                Some(ReasoningEffort::Low) => Some("low"),
                Some(ReasoningEffort::Medium) => Some("medium"),
                Some(ReasoningEffort::High) => Some("high"),
                Some(ReasoningEffort::XHigh) => Some("xhigh"),
                Some(ReasoningEffort::Max) => Some("max"),
                None => None,
                Some(ReasoningEffort::TokenBudget(_)) => unreachable!(),
                Some(_) => {
                    return Err(unsupported(
                        "Messages does not admit this output_config.effort",
                    ));
                }
            };
            Ok((
                Some(ThinkingParam::Adaptive { display }),
                effort_name.map(|effort| OutputConfigParam { effort }),
            ))
        }
        _ => Err(unsupported("unknown reasoning policy cannot be translated")),
    }
}

pub(super) fn translate_tool_choice(
    choice: &ToolChoice,
    has_tools: bool,
) -> Result<Option<ToolChoiceParam>, MessagesRequestError> {
    Ok(match choice {
        ToolChoice::Auto if has_tools => Some(ToolChoiceParam::Auto),
        ToolChoice::None if has_tools => Some(ToolChoiceParam::None),
        ToolChoice::Auto | ToolChoice::None => None,
        ToolChoice::Required => Some(ToolChoiceParam::Any),
        ToolChoice::Specific(name) => Some(ToolChoiceParam::Tool {
            name: name.as_str().to_owned(),
        }),
        _ => {
            return Err(unsupported(
                "unknown tool-choice policy cannot be translated",
            ));
        }
    })
}

#[cfg(test)]
mod tests;
