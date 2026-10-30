//! Conversation-item lowering into Responses input items.

use super::{
    Actor, BTreeMap, BTreeSet, CallId, CompactionBlock, ContentBlock, ConversationItem,
    ImageDetailHint, MAX_RESPONSE_ID_BYTES, MediaSource, ModelRequest, OpaqueFallback, Provenance,
    ReplayAuthority, ResponsesProtectedToolError, ResponsesReplayError, ResponsesReplayPolicy,
    ResponsesRequestConfig, ResponsesRequestContentError, ResponsesRequestError, ToolCallState,
    ToolCallerLineage, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind,
    ToolResultPart, ToolResultUpdateBlock, Value, exact_output_replay, function_call_item,
    function_call_output_item, input_text_part, json, message_item, output_text_part,
    replay_authority, replay_output_for_policy, replay_output_item,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolCallKind {
    Function,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct CanonicalPosition {
    item_index: usize,
    block_index: usize,
}

impl CanonicalPosition {
    const fn new(item_index: usize, block_index: usize) -> Self {
        Self {
            item_index,
            block_index,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ToolCallSource {
    position: CanonicalPosition,
    kind: ToolCallKind,
    call_id: CallId,
    owner: ToolExecutionOwner,
    name: ToolName,
    caller_lineage: ToolCallerLineage,
}

pub(super) struct ToolResultMatches<'a> {
    tool_calls: &'a BTreeMap<String, ToolCallSource>,
    matched: BTreeSet<String>,
    /// Yield texts of a UTF-8 cell that this target replays as a plain
    /// `function_call` (the tool is not one of its custom tools). A function
    /// call owns exactly one `function_call_output`, so its updates are folded
    /// into that output in source order instead of being sent as
    /// `custom_tool_call_output` items the wire would reject.
    folded_updates: BTreeMap<String, Vec<String>>,
}

impl<'a> ToolResultMatches<'a> {
    pub(super) fn new(tool_calls: &'a BTreeMap<String, ToolCallSource>) -> Self {
        Self {
            tool_calls,
            matched: BTreeSet::new(),
            folded_updates: BTreeMap::new(),
        }
    }

    /// Takes the yields folded for `remote_id`, if the call was replayed as a
    /// function call.
    fn take_folded_updates(&mut self, remote_id: &str) -> Option<Vec<String>> {
        self.folded_updates.remove(remote_id)
    }

    pub(super) fn match_prefix<'b>(
        &mut self,
        items: impl IntoIterator<Item = &'b ConversationItem>,
        config: ResponsesRequestConfig<'_>,
    ) -> Result<(), ResponsesRequestError> {
        for (item_index, item) in items.into_iter().enumerate() {
            if exact_output_replay(item_index, item, config)?.is_some() {
                continue;
            }
            for (block_index, block) in item.blocks().iter().enumerate() {
                match block {
                    ContentBlock::ToolResult(result) => {
                        let remote_id = result
                            .correlation()
                            .remote_call_id()
                            .map_or_else(|| result.call_id().to_string(), ToOwned::to_owned);
                        self.match_result(
                            &remote_id,
                            CanonicalPosition::new(item_index, block_index),
                            result,
                        )?;
                    }
                    ContentBlock::ToolResultUpdate(update) => {
                        let remote_id = update
                            .correlation()
                            .remote_call_id()
                            .map_or_else(|| update.call_id().to_string(), ToOwned::to_owned);
                        self.match_update(
                            &remote_id,
                            CanonicalPosition::new(item_index, block_index),
                            update,
                        )?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn match_result(
        &mut self,
        remote_id: &str,
        result_position: CanonicalPosition,
        result: &ToolResultBlock,
    ) -> Result<ToolCallKind, ResponsesRequestError> {
        let source = self
            .tool_calls
            .get(remote_id)
            .ok_or(ResponsesProtectedToolError::ResultWithoutCall)?;
        if source.position >= result_position {
            return Err(ResponsesProtectedToolError::ResultBeforeCall.into());
        }
        if source.call_id != result.call_id() {
            return Err(ResponsesProtectedToolError::CallIdentityMismatch.into());
        }
        if &source.owner != result.owner()
            || &source.caller_lineage != result.correlation().caller_lineage()
            || !matches!(
                result.owner(),
                ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
            )
        {
            return Err(ResponsesProtectedToolError::OwnerMismatch.into());
        }
        if !matches!(
            result.kind(),
            ToolResultKind::Success | ToolResultKind::Failure
        ) {
            return Err(ResponsesProtectedToolError::CallIdentityMismatch.into());
        }
        if !self.matched.insert(remote_id.to_owned()) {
            return Err(ResponsesProtectedToolError::DuplicateResult.into());
        }
        Ok(source.kind)
    }

    /// Matches a yield to its call. Returns the custom tool name when the wire
    /// carries the yield as its own `custom_tool_call_output`; returns `None`
    /// after folding it when the call is replayed as a function call.
    fn match_update(
        &mut self,
        remote_id: &str,
        update_position: CanonicalPosition,
        update: &ToolResultUpdateBlock,
    ) -> Result<Option<ToolName>, ResponsesRequestError> {
        let source = self
            .tool_calls
            .get(remote_id)
            .ok_or(ResponsesProtectedToolError::ResultWithoutCall)?;
        if source.position >= update_position {
            return Err(ResponsesProtectedToolError::ResultBeforeCall.into());
        }
        if source.call_id != update.call_id() {
            return Err(ResponsesProtectedToolError::CallIdentityMismatch.into());
        }
        if &source.owner != update.owner() {
            return Err(ResponsesProtectedToolError::OwnerMismatch.into());
        }
        match source.kind {
            ToolCallKind::Custom => Ok(Some(source.name.clone())),
            ToolCallKind::Function => {
                self.folded_updates
                    .entry(remote_id.to_owned())
                    .or_default()
                    .push(update.text().text().to_owned());
                Ok(None)
            }
        }
    }

    pub(super) fn finish(&self) -> Result<(), ResponsesRequestError> {
        if self.matched.len() != self.tool_calls.len() {
            return Err(ResponsesProtectedToolError::MissingResult.into());
        }
        Ok(())
    }
}

pub(super) fn derive_tool_calls(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
    custom_tools: &BTreeSet<ToolName>,
    first_replay_position: usize,
) -> Result<BTreeMap<String, ToolCallSource>, ResponsesRequestError> {
    let mut calls = BTreeMap::new();
    for (item_index, item) in request.input().items().iter().enumerate() {
        if item_index >= first_replay_position
            && let Some((block_index, replay)) = exact_output_replay(item_index, item, config)?
        {
            if let Some(replayed) = replay_output_for_policy(config, item_index, replay)? {
                remember_replayed_call(
                    &replayed,
                    CanonicalPosition::new(item_index, block_index),
                    item,
                    &mut calls,
                )?;
            }
            continue;
        }
        for (block_index, block) in item.blocks().iter().enumerate() {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            if item.actor() != Actor::Assistant {
                return Err(ResponsesProtectedToolError::InvalidCallActor.into());
            }
            if call.state() != ToolCallState::Requested {
                return Err(ResponsesProtectedToolError::InvalidCallState.into());
            }
            if !matches!(
                call.owner(),
                ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
            ) {
                return Err(ResponsesProtectedToolError::OwnerMismatch.into());
            }
            let remote_id = call
                .correlation()
                .remote_call_id()
                .map_or_else(|| call.call_id().to_string(), ToOwned::to_owned);
            let kind = if custom_tools.contains(call.exposed_name()) {
                ToolCallKind::Custom
            } else {
                ToolCallKind::Function
            };
            remember_tool_call(
                &mut calls,
                remote_id,
                ToolCallSource {
                    position: CanonicalPosition::new(item_index, block_index),
                    kind,
                    call_id: call.call_id(),
                    owner: call.owner().clone(),
                    name: call.exposed_name().clone(),
                    caller_lineage: call.correlation().caller_lineage().clone(),
                },
            )?;
        }
    }
    Ok(calls)
}

pub(super) fn remember_replayed_call(
    replayed: &Value,
    position: CanonicalPosition,
    item: &ConversationItem,
    calls: &mut BTreeMap<String, ToolCallSource>,
) -> Result<(), ResponsesRequestError> {
    let kind = match replayed.get("type").and_then(Value::as_str) {
        Some("function_call") => ToolCallKind::Function,
        Some("custom_tool_call") => ToolCallKind::Custom,
        _ => return Ok(()),
    };
    let call_id = replayed
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_RESPONSE_ID_BYTES)
        .ok_or(ResponsesReplayError::MalformedEnvelope)?;
    let mut canonical_calls = item.blocks().iter().filter_map(|block| match block {
        ContentBlock::ToolCall(call) => Some(call),
        _ => None,
    });
    let call = canonical_calls
        .next()
        .ok_or(ResponsesProtectedToolError::ResultWithoutCall)?;
    if canonical_calls.next().is_some()
        || item.actor() != Actor::Assistant
        || call.state() != ToolCallState::Requested
    {
        return Err(ResponsesProtectedToolError::InvalidCallActor.into());
    }
    remember_tool_call(
        calls,
        call_id.to_owned(),
        ToolCallSource {
            position,
            kind,
            call_id: call.call_id(),
            owner: call.owner().clone(),
            name: call.exposed_name().clone(),
            caller_lineage: call.correlation().caller_lineage().clone(),
        },
    )
}

pub(super) fn remember_tool_call(
    calls: &mut BTreeMap<String, ToolCallSource>,
    call_id: String,
    source: ToolCallSource,
) -> Result<(), ResponsesRequestError> {
    if calls.insert(call_id, source).is_some() {
        return Err(ResponsesProtectedToolError::DuplicateCall.into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive canonical-block match preserves source order"
)]
pub(super) fn lower_item(
    config: ResponsesRequestConfig<'_>,
    item_index: usize,
    item: &ConversationItem,
    custom_tools: &BTreeSet<ToolName>,
    tool_results: &mut ToolResultMatches<'_>,
    output: &mut Vec<Value>,
) -> Result<(), ResponsesRequestError> {
    if let Some((_, replay)) = exact_output_replay(item_index, item, config)? {
        if let Some(replayed) = replay_output_for_policy(config, item_index, replay)? {
            output.push(replayed);
        }
        return Ok(());
    }
    let role = match item.actor() {
        Actor::Developer => "developer",
        Actor::System => "system",
        Actor::User | Actor::Tool | Actor::Harness => "user",
        Actor::Assistant => "assistant",
        _ => return Err(ResponsesRequestContentError::UnknownActor.into()),
    };
    let mut message_parts = Vec::new();
    for (block_index, block) in item.blocks().iter().enumerate() {
        match block {
            ContentBlock::Text(text) => {
                if item.actor() == Actor::Assistant {
                    message_parts.push(output_text_part(text.text()));
                } else {
                    message_parts.push(input_text_part(text.text()));
                }
            }
            ContentBlock::Image(image) => {
                if !config.policy.image_input {
                    return Err(ResponsesRequestContentError::ImageInputUnsupported.into());
                }
                message_parts.push(json!({
                    "type": "input_image",
                    "image_url": image_url(image.media_type(), image.source())?,
                }));
            }
            ContentBlock::Refusal(_) if item.actor() == Actor::Assistant => {
                return Err(ResponsesRequestContentError::AssistantRefusalWithoutReplay.into());
            }
            ContentBlock::ToolCall(call) => {
                flush_message(role, &mut message_parts, output);
                if item.actor() != Actor::Assistant {
                    return Err(ResponsesProtectedToolError::InvalidCallActor.into());
                }
                if call.state() != ToolCallState::Requested {
                    return Err(ResponsesProtectedToolError::InvalidCallState.into());
                }
                if !matches!(
                    call.owner(),
                    ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
                ) {
                    return Err(
                        ResponsesRequestContentError::ProviderHostedCallWithoutReplay.into(),
                    );
                }
                let remote_id = call
                    .correlation()
                    .remote_call_id()
                    .map_or_else(|| call.call_id().to_string(), ToOwned::to_owned);
                if custom_tools.contains(call.exposed_name()) {
                    let custom_input = call
                        .arguments()
                        .and_then(Value::as_str)
                        .ok_or(ResponsesRequestContentError::InvalidCustomToolArguments)?;
                    let mut wire = json!({
                        "type": "custom_tool_call",
                        "call_id": remote_id,
                        "name": call.exposed_name().as_str(),
                        "input": custom_input,
                    });
                    lower_program_caller(&mut wire, call.correlation().caller_lineage())?;
                    output.push(wire);
                } else {
                    // A UTF-8 cell (Codex Code Mode `exec`) replayed onto a target
                    // that does not expose the tool as custom: the function call
                    // carries the cell's original string input verbatim. The
                    // canonical block keeps that input as a JSON string literal
                    // so `arguments()` is `Some(String)` exactly for this shape;
                    // a structured call is always a JSON object.
                    let arguments = match call.arguments() {
                        Some(Value::String(input)) => input.as_str(),
                        _ => call.raw_arguments(),
                    };
                    let mut wire =
                        function_call_item(remote_id, call.exposed_name().as_str(), arguments);
                    lower_program_caller(&mut wire, call.correlation().caller_lineage())?;
                    output.push(wire);
                }
            }
            ContentBlock::ToolResult(result) => {
                flush_message(role, &mut message_parts, output);
                if item.actor() != Actor::Tool {
                    return Err(ResponsesProtectedToolError::InvalidResultActor.into());
                }
                let remote_id = result
                    .correlation()
                    .remote_call_id()
                    .map_or_else(|| result.call_id().to_string(), ToOwned::to_owned);
                let parts = lower_tool_result(result.parts())?;
                match tool_results.match_result(
                    &remote_id,
                    CanonicalPosition::new(item_index, block_index),
                    result,
                )? {
                    ToolCallKind::Custom => {
                        let mut wire = json!({
                        "type": "custom_tool_call_output",
                        "call_id": remote_id,
                        "output": parts,
                        });
                        lower_program_caller(&mut wire, result.correlation().caller_lineage())?;
                        output.push(wire);
                    }
                    ToolCallKind::Function => {
                        let parts = match tool_results.take_folded_updates(&remote_id) {
                            Some(updates) => fold_updates_into_output(updates, parts),
                            None => parts,
                        };
                        let mut wire = function_call_output_item(remote_id, parts);
                        lower_program_caller(&mut wire, result.correlation().caller_lineage())?;
                        output.push(wire);
                    }
                }
            }
            ContentBlock::ToolResultUpdate(update) => {
                flush_message(role, &mut message_parts, output);
                if item.actor() != Actor::Tool {
                    return Err(ResponsesProtectedToolError::InvalidResultActor.into());
                }
                let remote_id = update
                    .correlation()
                    .remote_call_id()
                    .map_or_else(|| update.call_id().to_string(), ToOwned::to_owned);
                if let Some(name) = tool_results.match_update(
                    &remote_id,
                    CanonicalPosition::new(item_index, block_index),
                    update,
                )? {
                    output.push(json!({
                        "type": "custom_tool_call_output",
                        "call_id": remote_id,
                        "name": name.as_str(),
                        "output": update.text().text(),
                    }));
                }
            }
            ContentBlock::Reasoning(reasoning) => {
                flush_message(role, &mut message_parts, output);
                if config.policy.replay == ResponsesReplayPolicy::DropUnreplayableReasoning {
                    continue;
                }
                let Some(replay) = reasoning.replay() else {
                    continue;
                };
                if replay_authority(config, item_index, replay)? == ReplayAuthority::Active {
                    let replayed = replay_output_for_policy(config, item_index, replay)?
                        .ok_or(ResponsesReplayError::StrictEncryptedReasoning)?;
                    output.push(replayed);
                }
            }
            ContentBlock::Opaque(opaque) => {
                if replay_authority(config, item_index, opaque.replay())? == ReplayAuthority::Active
                {
                    flush_message(role, &mut message_parts, output);
                    output.push(replay_output_item(config, item_index, opaque.replay())?);
                } else {
                    match opaque.fallback() {
                        OpaqueFallback::Omit => {}
                        OpaqueFallback::SemanticSummary(summary) => {
                            if item.actor() == Actor::Assistant {
                                message_parts.push(output_text_part(summary));
                            } else {
                                message_parts.push(input_text_part(summary));
                            }
                        }
                        OpaqueFallback::CompatibleReplay => {
                            if !has_complete_portable_item_semantics(item) {
                                return Err(
                                    ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                                        .into(),
                                );
                            }
                        }
                        _ => {
                            return Err(ResponsesRequestContentError::UnknownCanonicalBlock.into());
                        }
                    }
                }
            }
            ContentBlock::Document(_) | ContentBlock::Audio(_) => {
                return Err(ResponsesRequestContentError::DocumentOrAudio.into());
            }
            ContentBlock::Citation(_) => {
                return Err(ResponsesRequestContentError::StandaloneCitation.into());
            }
            ContentBlock::Compaction(compaction) => {
                flush_message(role, &mut message_parts, output);
                if let Some(wire) = bound_native_compaction_item(config, compaction)? {
                    if item.actor() != Actor::Assistant {
                        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
                    }
                    output.push(wire);
                    continue;
                }
                if compaction.native_item().is_some() {
                    return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
                }
                if item.actor() == Actor::Assistant {
                    message_parts.push(output_text_part(compaction.summary()));
                } else {
                    return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
                }
            }
            _ => return Err(ResponsesRequestContentError::UnknownCanonicalBlock.into()),
        }
    }
    flush_message(role, &mut message_parts, output);
    Ok(())
}

/// Binding-checked next-turn replay of a native compact item.
///
/// Reconstructs `{type:compaction,encrypted_content}` from the stored
/// ciphertext after exact producing-target, `type` and ciphertext checks. Extra native keys
/// (for example `id`) are not forwarded. Other Responses policies stay
/// fail-closed on `native_item`.
pub(super) fn bound_native_compaction_item(
    config: ResponsesRequestConfig<'_>,
    compaction: &CompactionBlock,
) -> Result<Option<Value>, ResponsesRequestError> {
    let Some(native_item) = compaction.native_item() else {
        return Ok(None);
    };
    if !config.policy.authorizes_native_compaction_item_replay()
        || !config
            .account
            .copied()
            .zip(compaction.native_binding())
            .is_some_and(|(account, binding)| {
                binding.is_compatible(config.protocol, config.model_family, config.model, account)
            })
    {
        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
    }
    let parsed: Value = serde_json::from_str(native_item)
        .map_err(|_| ResponsesRequestContentError::CompactionWithoutReplay)?;
    let object = parsed
        .as_object()
        .ok_or(ResponsesRequestContentError::CompactionWithoutReplay)?;
    if object.get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
    }
    let encrypted = object
        .get("encrypted_content")
        .and_then(Value::as_str)
        .ok_or(ResponsesRequestContentError::CompactionWithoutReplay)?;
    if encrypted.is_empty() || encrypted != compaction.summary() {
        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
    }
    Ok(Some(json!({
        "type": "compaction",
        "encrypted_content": compaction.summary(),
    })))
}

pub(super) fn lower_program_caller(
    wire: &mut Value,
    lineage: &ToolCallerLineage,
) -> Result<(), ResponsesRequestError> {
    let (kind, execution_id) = match lineage {
        ToolCallerLineage::Direct => return Ok(()),
        ToolCallerLineage::LocalProgrammatic { .. } => {
            return Err(ResponsesRequestContentError::ProviderHostedCallWithoutReplay.into());
        }
        ToolCallerLineage::ProviderProgrammatic { kind, execution_id } => (kind, execution_id),
    };
    if kind.as_str() != "openai.responses.program" {
        return Err(ResponsesRequestContentError::ProviderHostedCallWithoutReplay.into());
    }
    wire.as_object_mut()
        .ok_or(ResponsesRequestError::Serialization)?
        .insert(
            "caller".to_owned(),
            json!({"type":"program", "caller_id":execution_id}),
        );
    Ok(())
}

/// An unbound or foreign native output item may be omitted only when the same
/// canonical item independently carries all semantics this target is allowed
/// to project. Exact target-bound replay returns before this predicate is used.
pub(super) fn has_complete_portable_item_semantics(item: &ConversationItem) -> bool {
    if item.actor() != Actor::Assistant || item.provenance() != Provenance::ModelOutput {
        return false;
    }
    let mut semantic_blocks = 0_usize;
    for block in item.blocks() {
        match block {
            ContentBlock::Opaque(_) => {}
            ContentBlock::Text(text)
                if text.phase().is_none()
                    || text.phase() == Some(plakous_model::TextPhase::Final)
                    || text.phase() == Some(plakous_model::TextPhase::Commentary) =>
            {
                semantic_blocks = semantic_blocks.saturating_add(1);
            }
            ContentBlock::ToolCall(call)
                if call.state() == ToolCallState::Requested
                    && matches!(
                        call.owner(),
                        ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
                    ) =>
            {
                semantic_blocks = semantic_blocks.saturating_add(1);
            }
            ContentBlock::Reasoning(_) | ContentBlock::Compaction(_) => {
                semantic_blocks = semantic_blocks.saturating_add(1);
            }
            _ => return false,
        }
    }
    semantic_blocks != 0
}

pub(super) fn flush_message(role: &str, parts: &mut Vec<Value>, output: &mut Vec<Value>) {
    if !parts.is_empty() {
        output.push(message_item(role, std::mem::take(parts)));
    }
}

/// Prepends folded cell yields to a function call's output, in source order.
pub(super) fn fold_updates_into_output(updates: Vec<String>, output: Value) -> Value {
    let mut parts: Vec<Value> = updates
        .into_iter()
        .map(|text| input_text_part(&text))
        .collect();
    match output {
        Value::Array(rest) => parts.extend(rest),
        Value::String(text) => parts.push(input_text_part(&text)),
        other => parts.push(other),
    }
    Value::Array(parts)
}

pub(super) fn lower_tool_result(parts: &[ToolResultPart]) -> Result<Value, ResponsesRequestError> {
    if let [ToolResultPart::Text(text)] = parts {
        return Ok(Value::String(text.text().to_owned()));
    }
    let mut output = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ToolResultPart::Text(text) => output.push(input_text_part(text.text())),
            ToolResultPart::Image(image) => {
                let image_url = image_url(image.media_type(), image.source())?;
                let detail = image.detail().map(image_detail_wire).transpose()?;
                let mut item = json!({"type": "input_image", "image_url": image_url});
                if let Some(detail) = detail {
                    item["detail"] = Value::String(detail.to_owned());
                }
                output.push(item);
            }
            ToolResultPart::Audio(audio) => output.push(json!({
                "type": "input_audio",
                "audio_url": exact_media_url(audio.source())?,
            })),
            ToolResultPart::Document(_) | ToolResultPart::Opaque(_) => {
                return Err(ResponsesRequestContentError::NonTextToolResult.into());
            }
            _ => return Err(ResponsesRequestContentError::NonTextToolResult.into()),
        }
    }
    Ok(Value::Array(output))
}

/// The `image_url` of an `input_image`: inline bytes become a base64 `data:` URL.
fn image_url(media_type: &str, source: &MediaSource) -> Result<String, ResponsesRequestError> {
    use base64::Engine as _;
    if let Some(bytes) = source.as_bytes() {
        return Ok(format!(
            "data:{media_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ));
    }
    exact_media_url(source).map(ToOwned::to_owned)
}

pub(super) fn exact_media_url(source: &MediaSource) -> Result<&str, ResponsesRequestError> {
    source
        .as_data_url()
        .or_else(|| source.as_uri())
        .ok_or_else(|| ResponsesRequestContentError::NonTextToolResult.into())
}

pub(super) fn image_detail_wire(
    detail: ImageDetailHint,
) -> Result<&'static str, ResponsesRequestError> {
    match detail {
        ImageDetailHint::Auto => Ok("auto"),
        ImageDetailHint::Low => Ok("low"),
        ImageDetailHint::High => Ok("high"),
        ImageDetailHint::Original => Ok("original"),
        _ => Err(ResponsesRequestContentError::ToolResultImage.into()),
    }
}
