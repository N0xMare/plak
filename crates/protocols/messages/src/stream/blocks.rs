//! Content block start, delta and stop transitions of the stream projector.

use plakous_model::{
    BlockCompleted, BlockStart, BlockStarted, CallId, ContentBlock, ModelEvent, OpaqueBlock,
    OpaqueFallback, OutputLocation, ReasoningBlock, ReasoningDelta, ReasoningReplayBound,
    ReasoningVisibility, ReasoningVisibilityBound, RefusalBlock, TextBlock, TextDelta,
    ToolCallBlock, ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolCorrelation,
    ToolExecutionOwner, ToolInputDelta, ToolName,
};
use serde_json::value::RawValue;

use crate::request::{
    CODE_EXECUTION_RESULT_REPLAY_KIND, REDACTED_REPLAY_KIND, SIGNATURE_REPLAY_KIND,
};

use super::{
    ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent, EventEnvelope,
    InputJsonDeltaWire, MessagesStreamError, MessagesStreamExtensions, MessagesStreamProjector,
    OpenBlock, RedactedStart, RefusalDeltaWire, RefusalStart, SignatureDeltaWire, TextDeltaWire,
    TextStart, ThinkingStart, ToolCaller, ToolUseInput, ToolUseStart, invalid_response,
    nonempty_opaque_signature, parse_event, parse_thinking_delta, parse_tool_caller,
    raw_block_start_has_hosted_effect, validate_opaque_string,
};

impl MessagesStreamProjector {
    #[allow(
        clippy::too_many_lines,
        reason = "keeping every content-block start transition together makes the wire grammar auditable"
    )]
    pub(super) fn block_start(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if self.prepared.hosted_effect_capable() && raw_block_start_has_hosted_effect(data) {
            self.hosted_effects_possible = true;
        }
        self.require_open()?;
        if self.stop_reason.is_some() || self.stop_sequence.is_some() || self.stop_details.is_some()
        {
            return Err(self.fail("content block started after terminal message metadata"));
        }
        if self.prepared.dialect.extensions() != MessagesStreamExtensions::OpenRouterV1
            && !self.open_blocks.is_empty()
        {
            return Err(self.fail("content block started before the previous block stopped"));
        }
        if usize::try_from(self.next_block_index)
            .map_or(true, |count| count >= self.limits.max_blocks())
        {
            return Err(self.fail("Messages response exceeded the content-block limit"));
        }
        let event: ContentBlockStartEvent = parse_event(data)?;
        if event.index != self.next_block_index {
            return Err(self.fail("Messages content block indices are not contiguous"));
        }
        let kind: EventEnvelope = serde_json::from_str(event.content_block.get())
            .map_err(|error| invalid_response(format!("invalid content block: {error}")))?;
        if crate::request::excluded_content_block(&kind.kind) {
            return Err(MessagesStreamError::new(
                super::MessagesStreamErrorKind::UnsupportedSemantic,
                super::MessagesStreamErrorPhase::Stream,
                "Messages content block is deliberately excluded by the feature matrix",
            ));
        }
        if !matches!(
            kind.kind.as_str(),
            "text"
                | "thinking"
                | "redacted_thinking"
                | "server_tool_use"
                | "code_execution_tool_result"
                | "tool_use"
                | "refusal"
        ) {
            return self.open_unknown_block(event.index, event.content_block);
        }
        let index = self.reserve_output_index()?;
        self.output_indices.insert(event.index, index);
        let location = OutputLocation::new(0, index);
        let item_id = self.item_id()?;
        let block = match kind.kind.as_str() {
            "text" => {
                let value: TextStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let citations = super::citations::parse_citations(value.citations.as_deref())?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Text { phase: None },
                )));
                if !value.text.is_empty() {
                    output.push(ModelEvent::TextDelta(TextDelta::new(
                        location,
                        value.text.as_bytes(),
                    )));
                }
                OpenBlock::Text {
                    text: value.text,
                    citations,
                    native: event.content_block,
                }
            }
            "thinking" => {
                let value: ThinkingStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::ReasoningPending { replay: None },
                )));
                let visibility = if value.thinking.is_empty() {
                    None
                } else {
                    output.push(ModelEvent::ReasoningVisibilityBound(
                        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
                    ));
                    Some(ReasoningVisibility::Visible)
                };
                if !value.thinking.is_empty() {
                    output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                        location,
                        value.thinking.as_bytes(),
                    )));
                }
                // `signature:""` on content_block_start is a placeholder, not a
                // bound envelope. Treating it as bound makes signature_delta a
                // no-op and later Messages replay of `signature:""` is 400.
                let signature = nonempty_opaque_signature(value.signature, "thinking signature")?;
                if let Some(signature) = &signature {
                    let reported_model = self
                        .reported_model
                        .clone()
                        .ok_or_else(|| self.fail("provider-reported model was not initialized"))?;
                    let replay = self.prepared.bind_replay(
                        reported_model,
                        SIGNATURE_REPLAY_KIND,
                        signature.clone(),
                    )?;
                    output.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                        location, replay,
                    )));
                }
                OpenBlock::Thinking {
                    text: value.thinking,
                    signature,
                    visibility,
                }
            }
            "redacted_thinking" => {
                let value: RedactedStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                validate_opaque_string(&value.data, "redacted thinking data")?;
                OpenBlock::Redacted { data: value.data }
            }
            "server_tool_use" => {
                if !self.prepared.hosted_effect_capable() {
                    return Err(self.fail(
                        "Messages code execution server tool is not admitted by the prepared request",
                    ));
                }
                let value: ToolUseStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                if value.name != "code_execution"
                    || value.caller.is_some()
                    || value.container_id.is_some()
                    || !self.server_tool_ids.insert(value.id.clone())
                {
                    return Err(self.fail("invalid or duplicate Messages server_tool_use"));
                }
                self.server_lineage
                    .push_server(&value.id)
                    .map_err(|error| self.fail(error.to_string()))?;
                self.hosted_effects_possible = true;
                let provider_kind = self
                    .prepared
                    .dialect
                    .programmatic_tool_namespace()
                    .ok_or_else(|| {
                        self.fail("Messages stream policy omitted hosted-tool identity")
                    })?;
                let name = ToolName::new(value.name)
                    .map_err(|error| invalid_response(error.to_string()))?;
                let call_id = CallId::generate(self.clock.as_ref(), self.ids.as_ref())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let correlation =
                    ToolCorrelation::new(Some(value.id.clone()), None, ToolCallerLineage::Direct)
                        .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::ToolCall {
                        call_id,
                        tool: name.clone(),
                        owner: ToolExecutionOwner::ProviderHosted {
                            provider_kind: provider_kind.clone(),
                        },
                        state: ToolCallState::Pending,
                        correlation: correlation.clone(),
                    },
                )));
                let mut json = String::new();
                if value.input.get() != "{}" {
                    json.push_str(value.input.get());
                    output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                        location,
                        value.input.get().as_bytes(),
                    )));
                }
                OpenBlock::ToolUse(ToolUseInput {
                    call_id,
                    remote_id: value.id,
                    name,
                    owner: ToolExecutionOwner::ProviderHosted { provider_kind },
                    state: ToolCallState::Pending,
                    correlation,
                    json,
                })
            }
            "code_execution_tool_result" => {
                if !self.prepared.hosted_effect_capable() {
                    return Err(self.fail(
                        "Messages code execution result is not admitted by the prepared request",
                    ));
                }
                let value: serde_json::Value = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let object = value
                    .as_object()
                    .ok_or_else(|| self.fail("code execution result must be an object"))?;
                let tool_use_id = object
                    .get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| self.server_tool_ids.contains(*id))
                    .ok_or_else(|| {
                        self.fail("code execution result did not match a preceding server tool")
                    })?;
                if !self
                    .code_execution_result_ids
                    .insert(tool_use_id.to_owned())
                {
                    return Err(self.fail(
                        "code execution result repeated a terminal result for one server tool",
                    ));
                }
                self.server_lineage
                    .complete_next(tool_use_id)
                    .map_err(|_| {
                        self.fail("code execution results did not follow exact server-tool order")
                    })?;
                if object.get("content").is_none()
                    || object.get("type").and_then(serde_json::Value::as_str)
                        != Some("code_execution_tool_result")
                {
                    return Err(self.fail("code execution result had an incompatible shape"));
                }
                self.hosted_effects_possible = true;
                let raw = RawValue::from_string(value.to_string())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let reported_model = self
                    .reported_model
                    .clone()
                    .ok_or_else(|| self.fail("provider-reported model was not initialized"))?;
                let replay = self.prepared.bind_replay(
                    reported_model,
                    CODE_EXECUTION_RESULT_REPLAY_KIND,
                    raw,
                )?;
                let block = ContentBlock::Opaque(
                    OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)
                        .map_err(|error| invalid_response(error.to_string()))?,
                );
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Complete(block.clone()),
                )));
                let _ = tool_use_id;
                OpenBlock::Complete(block)
            }
            "tool_use" => {
                let value: ToolUseStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                if !self.remote_tool_ids.insert(value.id.clone()) {
                    return Err(self.fail("Messages repeated a tool_use id in one response"));
                }
                let name = ToolName::new(value.name).map_err(|error| {
                    invalid_response(format!("invalid Messages tool name: {error}"))
                })?;
                let caller = parse_tool_caller(value.caller.as_deref(), &self.prepared.dialect)?;
                let owner = self.prepared.tool_ownership().owner(&name);
                let call_id =
                    CallId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|error| {
                        invalid_response(format!("failed to generate tool call ID: {error}"))
                    })?;
                if let ToolCaller::Programmatic { tool_id } = &caller {
                    if !self.prepared.hosted_effect_capable() {
                        return Err(self.fail(
                            "Messages programmatic caller is not admitted by the prepared request",
                        ));
                    }
                    // A non-direct caller is a hosted effect from this block, even
                    // while its input stays deferred until the stop reason.
                    self.hosted_effects_possible = true;
                    if !self.server_lineage.is_unresolved(tool_id)
                        || !matches!(
                            self.prepared.tool_ownership().caller(&name),
                            Some(
                                ToolCallerDisposition::ProviderProgrammaticOnly
                                    | ToolCallerDisposition::DirectAndProviderProgrammatic
                            )
                        )
                    {
                        return Err(self.fail("programmatic caller did not match an eligible tool and preceding server tool"));
                    }
                    let mut json = String::new();
                    if value.input.get() != "{}" {
                        json.push_str(value.input.get());
                    }
                    return self.open_deferred_programmatic_block(
                        event.index,
                        OpenBlock::DeferredProgrammaticToolUse {
                            call_id,
                            remote_id: value.id,
                            name,
                            owner,
                            execution_id: tool_id.clone(),
                            json,
                        },
                    );
                }
                if matches!(caller, ToolCaller::ExplicitDirect) {
                    self.explicit_direct_tool_ids.insert(value.id.clone());
                }
                // Unadvertised direct names stay Local Requested so admit_tool can
                // return a typed failure listing the presented catalog.
                let correlation =
                    ToolCorrelation::new(Some(value.id.clone()), None, ToolCallerLineage::Direct)
                        .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::ToolCall {
                        call_id,
                        tool: name.clone(),
                        owner: owner.clone(),
                        state: ToolCallState::Requested,
                        correlation: correlation.clone(),
                    },
                )));
                let mut json = String::new();
                if value.input.get() != "{}" {
                    json.push_str(value.input.get());
                    output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                        location,
                        value.input.get().as_bytes(),
                    )));
                }
                OpenBlock::ToolUse(ToolUseInput {
                    call_id,
                    remote_id: value.id,
                    name,
                    owner,
                    state: ToolCallState::Requested,
                    correlation,
                    json,
                })
            }
            "refusal" => {
                let value: RefusalStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Refusal,
                )));
                if !value.refusal.is_empty() {
                    output.push(ModelEvent::TextDelta(TextDelta::new(
                        location,
                        value.refusal.as_bytes(),
                    )));
                }
                OpenBlock::Refusal {
                    text: value.refusal,
                }
            }
            _ => return Err(self.fail("Messages known block dispatch was inconsistent")),
        };
        if block.retained_bytes() > self.limits.max_block_bytes() {
            return Err(self.fail("Messages content block exceeded the byte limit"));
        }
        self.visible_output = true;
        if self.open_blocks.insert(event.index, block).is_some() {
            return Err(self.fail("Messages repeated an open content block index"));
        }
        self.next_block_index = self.next_block_index.saturating_add(1);
        Ok(())
    }

    fn open_deferred_programmatic_block(
        &mut self,
        index: u32,
        block: OpenBlock,
    ) -> Result<(), MessagesStreamError> {
        if block.retained_bytes() > self.limits.max_block_bytes() {
            return Err(self.fail("Messages content block exceeded the byte limit"));
        }
        self.visible_output = true;
        if self.open_blocks.insert(index, block).is_some() {
            return Err(self.fail("Messages repeated an open content block index"));
        }
        self.next_block_index = self.next_block_index.saturating_add(1);
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "all typed delta transitions stay together so ownership and replay mutations remain auditable"
    )]
    pub(super) fn block_delta(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        let event: ContentBlockDeltaEvent = parse_event(data)?;
        let reported_model = self
            .reported_model
            .clone()
            .ok_or_else(|| invalid_response("provider-reported model was not initialized"))?;
        let index = self.output_indices.get(&event.index).copied();
        let Some(block) = self.open_blocks.get_mut(&event.index) else {
            return Err(self.fail("content delta arrived without an open block"));
        };
        let kind: EventEnvelope = serde_json::from_str(event.delta.get())
            .map_err(|error| invalid_response(error.to_string()))?;
        if let OpenBlock::Skipped { bytes } = block {
            *bytes = bytes.saturating_add(event.delta.get().len());
            if *bytes
                > self
                    .limits
                    .max_unknown_event_bytes()
                    .min(self.limits.max_block_bytes())
            {
                return Err(self.fail("unknown Messages block exceeded the byte limit"));
            }
            super::citations::validate_skipped_delta(event.delta.get())?;
            return Ok(());
        }
        let location = OutputLocation::new(
            0,
            index.ok_or_else(|| invalid_response("Messages block has no output location"))?,
        );
        match (kind.kind.as_str(), block) {
            ("text_delta", OpenBlock::Text { text, .. }) => {
                let delta: TextDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                if delta.text.is_empty() {
                    return Err(self.fail("empty text delta is not canonical"));
                }
                text.push_str(&delta.text);
                output.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    delta.text.into_bytes(),
                )));
            }
            (
                "thinking_delta",
                OpenBlock::Thinking {
                    text,
                    signature,
                    visibility,
                },
            ) => {
                let delta =
                    parse_thinking_delta(event.delta.get(), self.prepared.dialect.extensions())?;
                // Claude Code 2.1.x emits empty thinking blocks (`thinking: ""`
                // plus a signature). Matching empty `thinking_delta` events are
                // no-ops; they must not fail closed or mark the block visible.
                if delta.thinking.is_empty() {
                    return Ok(());
                }
                if signature.is_some() {
                    return Err(self.fail("thinking mutated after its signature was bound"));
                }
                if visibility.is_none() {
                    output.push(ModelEvent::ReasoningVisibilityBound(
                        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
                    ));
                    *visibility = Some(ReasoningVisibility::Visible);
                }
                text.push_str(&delta.thinking);
                output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                    location,
                    delta.thinking.into_bytes(),
                )));
            }
            ("signature_delta", OpenBlock::Thinking { signature, .. }) => {
                let delta: SignatureDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let Some(next) =
                    nonempty_opaque_signature(Some(delta.signature), "thinking signature")?
                else {
                    return Ok(());
                };
                // A provider may repeat the signature at block end. Compare
                // decoded JSON values so harmless encoding differences remain
                // valid while contradictory signed evidence fails closed.
                if let Some(current) = signature {
                    let current: String = serde_json::from_str(current.get())
                        .map_err(|error| invalid_response(error.to_string()))?;
                    let repeated: String = serde_json::from_str(next.get())
                        .map_err(|error| invalid_response(error.to_string()))?;
                    if current != repeated {
                        return Err(invalid_response(
                            "conflicting thinking signatures for one content block",
                        ));
                    }
                    return Ok(());
                }
                *signature = Some(next.clone());
                let replay =
                    self.prepared
                        .bind_replay(reported_model, SIGNATURE_REPLAY_KIND, next)?;
                output.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                    location, replay,
                )));
            }
            ("input_json_delta", OpenBlock::ToolUse(ToolUseInput { json, .. })) => {
                let delta: InputJsonDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                // Messages documents an empty first input delta; it is a bounded no-op.
                if !delta.partial_json.is_empty() {
                    json.push_str(&delta.partial_json);
                    output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                        location,
                        delta.partial_json.into_bytes(),
                    )));
                }
            }
            ("input_json_delta", OpenBlock::DeferredProgrammaticToolUse { json, .. }) => {
                let delta: InputJsonDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                json.push_str(&delta.partial_json);
            }
            ("refusal_delta", OpenBlock::Refusal { text }) => {
                let delta: RefusalDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                text.push_str(&delta.refusal);
                output.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    delta.refusal.into_bytes(),
                )));
            }
            (_, OpenBlock::Complete(_)) => {
                return Err(self.fail("complete hosted result received a delta"));
            }
            ("citations_delta", OpenBlock::Text { citations, .. }) => {
                #[derive(serde::Deserialize)]
                struct Delta {
                    citation: Box<RawValue>,
                }
                let delta: Delta = serde_json::from_str(event.delta.get())
                    .map_err(|_| invalid_response("invalid Messages citation delta"))?;
                if citations.len() >= self.limits.max_blocks() {
                    return Err(self.fail("Messages citations exceeded the block limit"));
                }
                citations.push(delta.citation);
            }
            _ => return Err(self.fail("content delta type did not match its open block")),
        }
        if self
            .open_blocks
            .get(&event.index)
            .is_some_and(|block| block.retained_bytes() > self.limits.max_block_bytes())
        {
            return Err(self.fail("Messages content block exceeded the byte limit"));
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeping every content-block finalization transition together makes replay rules auditable"
    )]
    pub(super) fn block_stop(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        let reported_model = self
            .reported_model
            .clone()
            .ok_or_else(|| self.fail("provider-reported model was not initialized"))?;
        let event: ContentBlockStopEvent = parse_event(data)?;
        let Some(block) = self.open_blocks.remove(&event.index) else {
            return Err(self.fail("content_block_stop arrived without an open block"));
        };
        if matches!(block, OpenBlock::Skipped { .. }) {
            return Ok(());
        }
        let index = self.output_index(event.index)?;
        let location = OutputLocation::new(0, index);
        let item_id = self.item_id()?;
        let canonical = match block {
            OpenBlock::Text {
                text,
                citations,
                native,
            } => {
                if !citations.is_empty() {
                    let native = super::citations::final_text_block(native, &text, &citations)?;
                    self.cited_text.push(super::CitedText {
                        target: index,
                        native,
                        citations,
                    });
                }
                ContentBlock::Text(
                    TextBlock::new(text, None)
                        .map_err(|error| invalid_response(error.to_string()))?,
                )
            }
            OpenBlock::Native { block } => {
                let block = self.native_content(None, &block)?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Complete(block.clone()),
                )));
                block
            }
            OpenBlock::Skipped { .. } => return Ok(()),
            OpenBlock::Thinking {
                text,
                signature,
                visibility,
            } => {
                if signature.is_none()
                    && self.prepared.dialect.extensions() != MessagesStreamExtensions::OpenRouterV1
                {
                    return Err(self.fail("thinking block ended without a signature"));
                }
                let replay = signature
                    .map(|signature| {
                        self.prepared.bind_replay(
                            reported_model.clone(),
                            SIGNATURE_REPLAY_KIND,
                            signature,
                        )
                    })
                    .transpose()?;
                let (visibility, text) = if text.is_empty() {
                    if visibility.is_some() {
                        return Err(
                            self.fail("empty Messages thinking had already resolved visible")
                        );
                    }
                    output.push(ModelEvent::ReasoningVisibilityBound(
                        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
                    ));
                    (ReasoningVisibility::Omitted, None)
                } else {
                    if visibility != Some(ReasoningVisibility::Visible) {
                        return Err(self.fail(
                            "nonempty Messages thinking did not resolve visible before its first delta",
                        ));
                    }
                    (ReasoningVisibility::Visible, Some(text))
                };
                let reasoning = ReasoningBlock::new(visibility, text, replay)
                    .map_err(|error| invalid_response(error.to_string()))?;
                ContentBlock::Reasoning(reasoning)
            }
            OpenBlock::Redacted { data } => {
                let replay =
                    self.prepared
                        .bind_replay(reported_model, REDACTED_REPLAY_KIND, data)?;
                let reasoning =
                    ReasoningBlock::new(ReasoningVisibility::Redacted, None, Some(replay))
                        .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Complete(ContentBlock::Reasoning(reasoning.clone())),
                )));
                ContentBlock::Reasoning(reasoning)
            }
            OpenBlock::ToolUse(input) => {
                if matches!(
                    input.owner,
                    ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
                ) {
                    // A closed wire block may still contain truncated JSON when a
                    // classifier revokes it. Only the terminal can authorize parsing.
                    self.closed_client_blocks.push((index, input));
                    return Ok(());
                }
                self.complete_tool_input(input, location, output)?
            }
            OpenBlock::DeferredProgrammaticToolUse {
                call_id,
                remote_id,
                name,
                owner,
                execution_id,
                json,
            } => {
                // The terminal reason is not known yet. A cut input is judged
                // at message_delta, not rejected as malformed JSON here.
                self.deferred_programmatic_blocks.push((
                    index,
                    OpenBlock::DeferredProgrammaticToolUse {
                        call_id,
                        remote_id,
                        name,
                        owner,
                        execution_id,
                        json,
                    },
                ));
                return Ok(());
            }
            OpenBlock::Refusal { text } => ContentBlock::Refusal(
                RefusalBlock::new(text).map_err(|error| invalid_response(error.to_string()))?,
            ),
            OpenBlock::Complete(block) => block,
        };
        let needs_completion_event = !matches!(
            &canonical,
            ContentBlock::Reasoning(reasoning)
                if reasoning.visibility() == ReasoningVisibility::Redacted
        ) && !matches!(&canonical, ContentBlock::Opaque(_));
        if self.blocks.insert(index, canonical).is_some() {
            return Err(self.fail("Messages completed one content block more than once"));
        }
        if needs_completion_event {
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }

    pub(super) fn complete_tool_input(
        &mut self,
        input: ToolUseInput,
        location: OutputLocation,
        output: &mut Vec<ModelEvent>,
    ) -> Result<ContentBlock, MessagesStreamError> {
        let ToolUseInput {
            call_id,
            remote_id,
            name,
            owner,
            state,
            correlation,
            mut json,
        } = input;
        if json.is_empty() {
            json.push_str("{}");
            output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location, b"{}",
            )));
        }
        let parsed = serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|_| self.fail("terminal Messages tool input was not complete valid JSON"))?;
        if !parsed.is_object() {
            return Err(self.fail("terminal Messages tool input must be a JSON object"));
        }
        if correlation.remote_call_id() != Some(remote_id.as_str()) {
            return Err(self.fail("Messages tool correlation changed during assembly"));
        }
        Ok(ContentBlock::ToolCall(
            ToolCallBlock::new(call_id, name, owner, state, correlation, json)
                .map_err(|error| invalid_response(error.to_string()))?,
        ))
    }
}
