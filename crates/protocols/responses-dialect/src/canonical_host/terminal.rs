//! Authoritative terminal projection rebuilt from the complete typed aggregate.

use super::{
    Actor, BTreeMap, BlockCompleted, BlockStart, BlockStarted, CanonicalIdSources, CitationAdded,
    CitationBlock, CitationRange, CitationSource, CitationTarget, CitationUnit, CompactionBlock,
    ContentBlock, ConversationItem, Digest, ModelEvent, ModelId, ModelResponse,
    ModelResponseMetadata, OpaqueBlock, OpaqueFallback, OutputLocation, ProjectorLimits,
    Provenance, ReasoningBlock, ReasoningDelta, ReasoningVisibility, RefusalBlock, ResponseStarted,
    ResponsesAnnotation, ResponsesCanonicalDialectData, ResponsesCanonicalPolicy,
    ResponsesCanonicalProjectError, ResponsesCanonicalTool, ResponsesCanonicalToolKind,
    ResponsesContentPart, ResponsesHostedProgramContext, ResponsesOutputItem,
    ResponsesReasoningContentPart, ResponsesReasoningSummaryPart, ResponsesResponseStatus,
    ResponsesTerminalProjection, TextBlock, TextDelta, TextPhase, ToolCallBlock, ToolCallState,
    ToolCorrelation, ToolExecutionOwner, ToolInputDelta, ToolName, UsageUpdate, Value,
};

/// Authoritative terminal canonical aggregate, independent of live delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesCanonicalTerminal {
    response: ModelResponse,
}

impl ResponsesCanonicalTerminal {
    /// Exact authoritative canonical response.
    #[must_use]
    pub const fn response(&self) -> &ModelResponse {
        &self.response
    }

    /// Exact canonical output in provider output-item order.
    #[must_use]
    pub fn canonical_output(&self) -> &[ConversationItem] {
        self.response.output()
    }

    /// Emits one complete self-consistent authoritative event sequence.
    ///
    /// The sequence opens the response, reconstructs every exact canonical
    /// block, emits optional usage, and closes with exactly one completion. It
    /// can therefore be consumed directly by `ModelResponseAssembler` even
    /// when every live observation was dropped.
    #[must_use]
    pub fn into_events(self) -> Vec<ModelEvent> {
        let mut events = Vec::new();
        events.push(ModelEvent::ResponseStarted(
            ResponseStarted::new(
                self.response.response_id().map(str::to_owned),
                self.response.plan_digest(),
            )
            .with_metadata(self.response.metadata().clone()),
        ));
        let mut item_index = 0_u32;
        for item in self.response.output() {
            let mut block_index = 0_u32;
            for block in item.blocks() {
                emit_authoritative_block(
                    &mut events,
                    item.id(),
                    OutputLocation::new(item_index, block_index),
                    block,
                );
                block_index = block_index.saturating_add(1);
            }
            item_index = item_index.saturating_add(1);
        }
        if let Some(usage) = self.response.usage() {
            events.push(ModelEvent::Usage(UsageUpdate::new(usage)));
        }
        events.push(ModelEvent::ResponseCompleted(self.response));
        events
    }
}

pub(super) fn emit_authoritative_block(
    events: &mut Vec<ModelEvent>,
    item_id: plakous_model::EntryId,
    location: OutputLocation,
    block: &ContentBlock,
) {
    match block {
        ContentBlock::Text(text) => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Text {
                    phase: text.phase(),
                },
            )));
            if !text.text().is_empty() {
                events.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    text.text().as_bytes(),
                )));
            }
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::Refusal(refusal) => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Refusal,
            )));
            if !refusal.text().is_empty() {
                events.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    refusal.text().as_bytes(),
                )));
            }
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::Reasoning(reasoning) => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Reasoning {
                    visibility: reasoning.visibility(),
                    replay: reasoning.replay().cloned(),
                },
            )));
            if let Some(text) = reasoning.text() {
                events.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                    location,
                    text.as_bytes(),
                )));
            }
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::ToolCall(call) if call.arguments().is_some() => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::ToolCall {
                    call_id: call.call_id(),
                    tool: call.exposed_name().clone(),
                    owner: call.owner().clone(),
                    state: call.state(),
                    correlation: call.correlation().clone(),
                },
            )));
            events.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location,
                call.raw_arguments().as_bytes(),
            )));
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::Citation(citation) => {
            events.push(ModelEvent::CitationAdded(CitationAdded::new(
                item_id,
                location,
                citation.clone(),
            )));
        }
        ContentBlock::Image(_)
        | ContentBlock::Document(_)
        | ContentBlock::Audio(_)
        | ContentBlock::ToolCall(_)
        | ContentBlock::ToolResult(_)
        | ContentBlock::Compaction(_)
        | ContentBlock::Opaque(_)
        | _ => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Complete(block.clone()),
            )));
        }
    }
}

/// Builds the authoritative canonical terminal from complete shared data only.
///
/// No live observation projector is accepted by this function.  Dropped,
/// saturated, delayed, or absent family observations therefore cannot affect
/// the returned IDs, output, usage, finish, or terminal success.
///
/// # Errors
///
/// Fails closed for failed/non-terminal responses, model/tool disagreement,
/// incomplete tool calls, replay count/order mismatch, or canonical bounds.
pub fn project_canonical_terminal(
    terminal: &ResponsesTerminalProjection,
    data: &ResponsesCanonicalDialectData,
    plan_digest: Digest,
    ids: &CanonicalIdSources,
) -> Result<ResponsesCanonicalTerminal, ResponsesCanonicalProjectError> {
    project_canonical_terminal_with_hosted_programs(
        terminal,
        data,
        plan_digest,
        ids,
        ResponsesHostedProgramContext::default(),
    )
}

/// Builds the authoritative terminal using exact validated prior hosted-program state.
///
/// # Errors
///
/// In addition to ordinary terminal errors, rejects caller/output references
/// to absent or already completed programs and duplicate program terminals.
#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
pub fn project_canonical_terminal_with_hosted_programs(
    terminal: &ResponsesTerminalProjection,
    data: &ResponsesCanonicalDialectData,
    plan_digest: Digest,
    ids: &CanonicalIdSources,
    mut hosted_programs: ResponsesHostedProgramContext,
) -> Result<ResponsesCanonicalTerminal, ResponsesCanonicalProjectError> {
    let response = terminal.response();
    if data.model_binding == plakous_responses::ResponseModelBinding::ExactRequest
        && response.model() != data.expected_model.as_str()
    {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "terminal model differs from admitted model",
        ));
    }
    if let Some(provider) = data.router_provider.as_deref() {
        validate_router_metadata(response.native(), data.expected_model.as_str(), provider)?;
    }
    if !matches!(
        response.status(),
        ResponsesResponseStatus::Completed | ResponsesResponseStatus::Incomplete
    ) {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "terminal status is not canonical success",
        ));
    }
    let finish =
        terminal
            .finish()
            .cloned()
            .ok_or(ResponsesCanonicalProjectError::InvalidTerminal(
                "successful terminal omitted finish facts",
            ))?;
    let artifact =
        terminal
            .artifact_candidate()
            .ok_or(ResponsesCanonicalProjectError::InvalidTerminal(
                "successful terminal omitted exact replay candidates",
            ))?;
    if response.output().len() != artifact.replay().len()
        || artifact.output_count()
            != u32::try_from(response.output().len())
                .map_err(|_| ResponsesCanonicalProjectError::ResourceLimit("output items"))?
    {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "terminal output and replay counts differ",
        ));
    }
    if response.output().len() > data.limits.max_items {
        return Err(ResponsesCanonicalProjectError::ResourceLimit(
            "output items",
        ));
    }
    validate_terminal_item_statuses(response.status(), response.output())?;
    for item in response.output() {
        hosted_programs.apply_output(item, data.policy)?;
    }

    let mut budget = TerminalBudget::new(data.limits);
    let mut output = Vec::with_capacity(response.output().len());
    for (output_index, (item, replay)) in
        response.output().iter().zip(artifact.replay()).enumerate()
    {
        let entry_id = ids.entry_id()?;
        let mut blocks = terminal_item_blocks(
            item,
            replay.envelope(),
            output_index,
            entry_id,
            data,
            ids,
            &mut budget,
        )?;
        budget.add_block(replay.envelope().payload().get().len())?;
        let blank_message = match item {
            ResponsesOutputItem::Message { content, .. } => content.iter().all(|part| match part {
                ResponsesContentPart::OutputText { text, annotations } => {
                    text.is_empty() && annotations.is_empty()
                }
                ResponsesContentPart::Refusal { refusal } => refusal.is_empty(),
                ResponsesContentPart::Opaque { .. } => false,
            }),
            _ => false,
        };
        let fallback = if matches!(item, ResponsesOutputItem::Reasoning { .. }) || blank_message {
            // Encrypted/native reasoning is valuable only to the producing
            // Responses target. Its visible summary, when present, is already
            // canonical; a foreign model must never be forced to replay or
            // understand the hidden native item. Blank known placeholders can
            // also be omitted; retained unknown parts are never blank.
            OpaqueFallback::Omit
        } else {
            OpaqueFallback::CompatibleReplay
        };
        blocks.push(ContentBlock::Opaque(OpaqueBlock::new(
            replay.envelope().clone(),
            fallback,
        )?));
        output.push(ConversationItem::new(
            entry_id,
            Actor::Assistant,
            blocks,
            Provenance::ModelOutput,
        )?);
    }
    let reported_model = ModelId::new(response.model())?;
    let metadata = ModelResponseMetadata::new(
        Some(reported_model),
        terminal.provider_request().canonical_provider_request_id(),
    );
    let response = ModelResponse::new(
        output,
        finish,
        terminal.usage(),
        Some(response.id().to_owned()),
        plan_digest,
    )
    .with_metadata(metadata);
    Ok(ResponsesCanonicalTerminal { response })
}

pub(super) fn validate_router_metadata(
    native: &Value,
    model: &str,
    provider: &str,
) -> Result<(), ResponsesCanonicalProjectError> {
    let invalid = || ResponsesCanonicalProjectError::InvalidTerminal("router metadata mismatch");
    let metadata = native
        .as_object()
        .and_then(|object| object.get("openrouter_metadata"))
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    if metadata.get("requested").and_then(Value::as_str) != Some(model)
        || metadata.get("strategy").and_then(Value::as_str) != Some("direct")
        || metadata.get("attempt").and_then(Value::as_u64) != Some(1)
        || metadata.get("is_byok").and_then(Value::as_bool) != Some(false)
    {
        return Err(invalid());
    }
    let endpoints = metadata
        .get("endpoints")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let available = endpoints
        .get("available")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let [selected] = available.as_slice() else {
        return Err(invalid());
    };
    let selected = selected.as_object().ok_or_else(invalid)?;
    if endpoints.get("total").and_then(Value::as_u64) != Some(1)
        || selected.get("provider").and_then(Value::as_str) != Some(provider)
        || selected.get("model").and_then(Value::as_str) != Some(model)
        || selected.get("selected").and_then(Value::as_bool) != Some(true)
    {
        return Err(invalid());
    }
    if let Some(attempts) = metadata.get("attempts").filter(|value| !value.is_null()) {
        let attempts = attempts.as_array().ok_or_else(invalid)?;
        let [successful] = attempts.as_slice() else {
            return Err(invalid());
        };
        let successful = successful.as_object().ok_or_else(invalid)?;
        if successful.get("provider").and_then(Value::as_str) != Some(provider)
            || successful.get("model").and_then(Value::as_str) != Some(model)
            || successful.get("status").and_then(Value::as_u64) != Some(200)
        {
            return Err(invalid());
        }
    }
    if metadata
        .get("pipeline")
        .filter(|value| !value.is_null())
        .is_some_and(|value| !value.as_array().is_some_and(Vec::is_empty))
        || metadata
            .get("params")
            .filter(|value| !value.is_null())
            .is_some_and(|value| !value.as_object().is_some_and(serde_json::Map::is_empty))
    {
        return Err(invalid());
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "terminal item projection binds exact item/replay/identity/config facts"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
pub(super) fn terminal_item_blocks(
    item: &ResponsesOutputItem,
    _replay: &plakous_model::ReplayEnvelope,
    output_index: usize,
    entry_id: plakous_model::EntryId,
    data: &ResponsesCanonicalDialectData,
    ids: &CanonicalIdSources,
    budget: &mut TerminalBudget,
) -> Result<Vec<ContentBlock>, ResponsesCanonicalProjectError> {
    let mut blocks = Vec::new();
    match item {
        ResponsesOutputItem::Message {
            id,
            role,
            content,
            native,
            ..
        } => {
            if role != "assistant" {
                return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                    "message output role is not assistant",
                ));
            }
            let phase = message_phase(native)?;
            for (content_index, part) in content.iter().enumerate() {
                match part {
                    ResponsesContentPart::OutputText { text, annotations } => {
                        if text.is_empty() && annotations.is_empty() {
                            continue;
                        }
                        budget.add_block(text.len())?;
                        let target_block = u32::try_from(blocks.len()).map_err(|_| {
                            ResponsesCanonicalProjectError::ResourceLimit("message blocks")
                        })?;
                        blocks.push(ContentBlock::Text(TextBlock::new(
                            text.clone(),
                            Some(phase),
                        )?));
                        for (annotation_index, annotation) in annotations.iter().enumerate() {
                            match annotation {
                                ResponsesAnnotation::UrlCitation {
                                    start_index,
                                    end_index,
                                    url,
                                    title,
                                    ..
                                } => {
                                    require_terminal_policy(
                                        data,
                                        ResponsesCanonicalPolicy::CITATIONS,
                                        "citations",
                                    )?;
                                    let scalar_len =
                                        u64::try_from(text.chars().count()).map_err(|_| {
                                            ResponsesCanonicalProjectError::ResourceLimit(
                                                "citation text offsets",
                                            )
                                        })?;
                                    if *end_index > scalar_len {
                                        return Err(
                                            ResponsesCanonicalProjectError::InvalidTerminal(
                                                "citation range exceeds output text",
                                            ),
                                        );
                                    }
                                    budget.add_block(url.len().saturating_add(title.len()))?;
                                    let provider_id = id.as_deref().unwrap_or("output");
                                    let annotation_id = format!(
                                        "{provider_id}:{output_index}:{content_index}:{annotation_index}"
                                    );
                                    let source = CitationSource::new(
                                        url,
                                        annotation_id,
                                        Some(url.clone()),
                                        Some(title.clone()),
                                    )?;
                                    let range = CitationRange::new(
                                        *start_index,
                                        *end_index,
                                        CitationUnit::UnicodeScalar,
                                    )?;
                                    blocks.push(ContentBlock::Citation(CitationBlock::new(
                                        CitationTarget::new(entry_id, target_block),
                                        source,
                                        Some(range),
                                    )));
                                }
                                ResponsesAnnotation::Opaque { .. } => {
                                    require_terminal_policy(
                                        data,
                                        ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                                        "opaque annotation",
                                    )?;
                                }
                            }
                        }
                    }
                    ResponsesContentPart::Refusal { refusal } => {
                        if refusal.is_empty() {
                            continue;
                        }
                        budget.add_block(refusal.len())?;
                        blocks.push(ContentBlock::Refusal(RefusalBlock::new(refusal.clone())?));
                    }
                    ResponsesContentPart::Opaque { .. } => require_terminal_policy(
                        data,
                        ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                        "opaque message content",
                    )?,
                }
            }
        }
        ResponsesOutputItem::Reasoning {
            summary, content, ..
        } => {
            for part in summary {
                match part {
                    ResponsesReasoningSummaryPart::SummaryText(text) => {
                        if text.is_empty() {
                            continue;
                        }
                        budget.add_block(text.len())?;
                        blocks.push(ContentBlock::Reasoning(ReasoningBlock::new(
                            ReasoningVisibility::Summary,
                            Some(text.clone()),
                            None,
                        )?));
                    }
                    ResponsesReasoningSummaryPart::Opaque { .. } => require_terminal_policy(
                        data,
                        ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                        "opaque reasoning summary",
                    )?,
                }
            }
            for part in content {
                let ResponsesReasoningContentPart::ReasoningText(text) = part;
                if text.is_empty() {
                    continue;
                }
                budget.add_block(text.len())?;
                blocks.push(ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some(text.clone()),
                    None,
                )?));
            }
        }
        ResponsesOutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            native,
            ..
        } => blocks.push(terminal_tool_block(
            call_id,
            name,
            arguments,
            native,
            ResponsesCanonicalToolKind::Function,
            data,
            ids,
            budget,
        )?),
        ResponsesOutputItem::CustomToolCall {
            call_id,
            name,
            input,
            native,
            ..
        } => {
            require_terminal_policy(data, ResponsesCanonicalPolicy::CUSTOM_TOOLS, "custom tools")?;
            blocks.push(terminal_tool_block(
                call_id,
                name,
                input,
                native,
                ResponsesCanonicalToolKind::Custom,
                data,
                ids,
                budget,
            )?);
        }
        ResponsesOutputItem::Opaque { kind, .. } => {
            validate_opaque_kind(kind, data.policy)?;
            require_terminal_policy(
                data,
                ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                "unknown output",
            )?;
        }
        ResponsesOutputItem::Compaction {
            encrypted_content,
            native,
            ..
        } => {
            require_terminal_policy(
                data,
                ResponsesCanonicalPolicy::NATIVE_COMPACTION,
                "Responses native compaction is not qualified",
            )?;
            budget.add_block(
                encrypted_content
                    .len()
                    .saturating_add(native.to_string().len()),
            )?;
            blocks.push(ContentBlock::Compaction(
                CompactionBlock::new(encrypted_content.clone())?
                    .with_native_item(native.to_string())
                    .with_native_binding(data.native_compaction_binding.clone().ok_or(
                        ResponsesCanonicalProjectError::InvalidConfiguration(
                            "native compaction producing binding",
                        ),
                    )?),
            ));
        }
    }
    Ok(blocks)
}

#[allow(
    clippy::too_many_arguments,
    reason = "tool projection validates every identity-bearing wire fact together"
)]
pub(super) fn terminal_tool_block(
    remote_call_id: &str,
    name: &str,
    arguments: &str,
    native: &Value,
    expected_kind: ResponsesCanonicalToolKind,
    data: &ResponsesCanonicalDialectData,
    ids: &CanonicalIdSources,
    budget: &mut TerminalBudget,
) -> Result<ContentBlock, ResponsesCanonicalProjectError> {
    let caller_lineage = tool_caller_lineage(native, data.policy)?;
    let name = ToolName::new(name.to_owned())?;
    let (owner, kind) = local_or_advertised_tool(&data.tools, &name, expected_kind)?;
    require_lite_advertised_tool(native, data.policy, &data.tools, &name)?;
    let canonical_arguments = match kind {
        ResponsesCanonicalToolKind::Function => {
            let value = crate::response_host::parse_unique_json_value(arguments).map_err(|_| {
                ResponsesCanonicalProjectError::InvalidTerminal(
                    "function call arguments are not strict JSON",
                )
            })?;
            if !value.is_object() {
                return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                    "function call arguments are not a JSON object",
                ));
            }
            arguments.to_owned()
        }
        ResponsesCanonicalToolKind::Custom => serde_json::to_string(arguments)?,
    };
    budget.add_block(canonical_arguments.len())?;
    let correlation = ToolCorrelation::new(Some(remote_call_id.to_owned()), None, caller_lineage)?;
    let canonical_name = data
        .tools
        .get(&name)
        .and_then(ResponsesCanonicalTool::canonical_name)
        .cloned();
    let call = ToolCallBlock::new(
        ids.call_id()?,
        name,
        owner,
        ToolCallState::Requested,
        correlation,
        canonical_arguments,
    )?;
    Ok(ContentBlock::ToolCall(match canonical_name {
        Some(canonical_name) => call.with_canonical_name(canonical_name),
        None => call,
    }))
}

pub(super) fn local_or_advertised_tool(
    tools: &BTreeMap<ToolName, ResponsesCanonicalTool>,
    name: &ToolName,
    expected_kind: ResponsesCanonicalToolKind,
) -> Result<(ToolExecutionOwner, ResponsesCanonicalToolKind), ResponsesCanonicalProjectError> {
    match tools.get(name) {
        Some(tool) if tool.kind != expected_kind => {
            Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "tool call kind differs from advertised schema",
            ))
        }
        Some(tool) => Ok((tool.owner.canonical(), tool.kind)),
        None => Ok((ToolExecutionOwner::Local, expected_kind)),
    }
}

pub(super) fn require_lite_advertised_tool(
    native: &Value,
    policy: ResponsesCanonicalPolicy,
    tools: &BTreeMap<ToolName, ResponsesCanonicalTool>,
    name: &ToolName,
) -> Result<(), ResponsesCanonicalProjectError> {
    if policy.has(ResponsesCanonicalPolicy::CODEX_LITE_NAMESPACE)
        && native
            .get("namespace")
            .is_some_and(|value| !value.is_null())
        && !tools.contains_key(name)
    {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "namespaced Codex tool was not advertised",
        ));
    }
    Ok(())
}

pub(super) fn validate_terminal_item_statuses(
    terminal: &ResponsesResponseStatus,
    output: &[ResponsesOutputItem],
) -> Result<(), ResponsesCanonicalProjectError> {
    for item in output {
        let status = crate::output_item_status(item);
        let completed = matches!(status, None | Some("completed" | "done"));
        let incomplete = status == Some("incomplete");
        if matches!(terminal, ResponsesResponseStatus::Completed) && !completed {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "completed response contains a non-complete item",
            ));
        }
        if matches!(
            item,
            ResponsesOutputItem::FunctionCall { .. } | ResponsesOutputItem::CustomToolCall { .. }
        ) && !completed
        {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "incomplete output cannot authorize a tool call",
            ));
        }
        if matches!(terminal, ResponsesResponseStatus::Incomplete) && !(completed || incomplete) {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "incomplete response contains an invalid item status",
            ));
        }
    }
    Ok(())
}

pub(super) fn output_identity(item: &ResponsesOutputItem) -> (Option<&str>, Option<&str>) {
    match item {
        ResponsesOutputItem::Message { id, .. }
        | ResponsesOutputItem::Reasoning { id, .. }
        | ResponsesOutputItem::Compaction { id, .. } => (id.as_deref(), None),
        ResponsesOutputItem::FunctionCall { id, call_id, .. }
        | ResponsesOutputItem::CustomToolCall { id, call_id, .. } => (id.as_deref(), Some(call_id)),
        ResponsesOutputItem::Opaque { native, .. } => {
            (native.get("id").and_then(Value::as_str), None)
        }
    }
}

pub(super) fn message_phase(native: &Value) -> Result<TextPhase, ResponsesCanonicalProjectError> {
    match native.get("phase").and_then(Value::as_str) {
        None | Some("final" | "final_answer") => Ok(TextPhase::Final),
        Some("commentary") => Ok(TextPhase::Commentary),
        Some(_) => Err(ResponsesCanonicalProjectError::Unsupported("message phase")),
    }
}

pub(super) fn tool_caller_lineage(
    native: &Value,
    policy: ResponsesCanonicalPolicy,
) -> Result<plakous_model::ToolCallerLineage, ResponsesCanonicalProjectError> {
    let namespace = native.get("namespace").filter(|value| !value.is_null());
    if native.get("caller_id").is_some()
        || native.get("container_id").is_some()
        || namespace.is_some_and(|value| {
            !policy.has(ResponsesCanonicalPolicy::CODEX_LITE_NAMESPACE)
                || value.as_str() != Some("functions")
        })
    {
        return Err(ResponsesCanonicalProjectError::Unsupported(
            "provider-hosted or namespaced tool call",
        ));
    }
    let Some(caller) = native.get("caller") else {
        return Ok(plakous_model::ToolCallerLineage::Direct);
    };
    if caller.is_null() {
        return Ok(plakous_model::ToolCallerLineage::Direct);
    }
    let Some(object) = caller.as_object() else {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "tool caller is not an object",
        ));
    };
    if object.len() == 1 && object.get("type").and_then(Value::as_str) == Some("direct") {
        Ok(plakous_model::ToolCallerLineage::Direct)
    } else if object.len() == 2 && object.get("type").and_then(Value::as_str) == Some("program") {
        if !policy.has(ResponsesCanonicalPolicy::PROGRAMMATIC_CALLERS) {
            return Err(ResponsesCanonicalProjectError::Unsupported(
                "provider-programmatic tool caller",
            ));
        }
        let caller_id = object.get("caller_id").and_then(Value::as_str).ok_or(
            ResponsesCanonicalProjectError::InvalidTerminal("program caller lacks caller_id"),
        )?;
        Ok(plakous_model::ToolCallerLineage::provider_programmatic(
            plakous_model::NamespaceId::new("openai.responses.program")?,
            caller_id,
        )?)
    } else {
        Err(ResponsesCanonicalProjectError::Unsupported(
            "provider-hosted tool caller",
        ))
    }
}

pub(super) fn validate_opaque_kind(
    kind: &str,
    policy: ResponsesCanonicalPolicy,
) -> Result<(), ResponsesCanonicalProjectError> {
    if matches!(kind, "program" | "program_output") {
        return if policy.has(ResponsesCanonicalPolicy::PROGRAMMATIC_CALLERS) {
            Ok(())
        } else {
            Err(ResponsesCanonicalProjectError::Unsupported(
                "provider-programmatic output item",
            ))
        };
    }
    if matches!(
        kind,
        "computer_call"
            | "web_search_call"
            | "x_search_call"
            | "file_search_call"
            | "approval_request"
            | "compaction"
    ) || kind.starts_with("function_call")
        || kind.starts_with("custom_tool_call")
        || kind.starts_with("message")
    {
        Err(ResponsesCanonicalProjectError::Unsupported(
            "declared output-item subvariant",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn require_terminal_policy(
    data: &ResponsesCanonicalDialectData,
    capability: u8,
    surface: &'static str,
) -> Result<(), ResponsesCanonicalProjectError> {
    if data.policy.has(capability) {
        Ok(())
    } else {
        Err(ResponsesCanonicalProjectError::Unsupported(surface))
    }
}

pub(super) struct TerminalBudget {
    limits: ProjectorLimits,
    blocks: usize,
    bytes: usize,
}

impl TerminalBudget {
    const fn new(limits: ProjectorLimits) -> Self {
        Self {
            limits,
            blocks: 0,
            bytes: 0,
        }
    }

    fn add_block(&mut self, bytes: usize) -> Result<(), ResponsesCanonicalProjectError> {
        self.blocks =
            self.blocks
                .checked_add(1)
                .ok_or(ResponsesCanonicalProjectError::ResourceLimit(
                    "canonical blocks",
                ))?;
        self.bytes =
            self.bytes
                .checked_add(bytes)
                .ok_or(ResponsesCanonicalProjectError::ResourceLimit(
                    "canonical bytes",
                ))?;
        if self.blocks > self.limits.max_blocks {
            return Err(ResponsesCanonicalProjectError::ResourceLimit(
                "canonical blocks",
            ));
        }
        if self.bytes > self.limits.max_accumulated_bytes {
            return Err(ResponsesCanonicalProjectError::ResourceLimit(
                "canonical bytes",
            ));
        }
        Ok(())
    }
}
