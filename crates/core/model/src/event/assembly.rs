use super::{
    Actor, AssemblerLimits, AssemblyState, BTreeMap, BlockStart, BlockStarted, CitationAdded,
    ContentBlock, ConversationItem, Digest, DigestDomain, EntryId, FinishReason, ModelError,
    ModelErrorKind, ModelEvent, ModelResponse, ModelResponseAssembler, ModelResponseMetadata,
    OpenAssembly, OutputLocation, PartialBlock, Provenance, ReasoningBlock, ReasoningReplayBound,
    ReasoningVisibility, ReasoningVisibilityBound, RefusalBlock, ResponseStarted, TextBlock,
    ToolCallBlock, ToolCallState, ToolExecutionOwner, Usage, invalid_event,
};

impl ModelResponseAssembler {
    /// Creates an assembler bound to the inspected prepared-plan digest.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when the supplied digest is not in the
    /// request-plan domain.
    pub fn new(expected_plan_digest: Digest) -> Result<Self, ModelError> {
        Self::with_limits(expected_plan_digest, AssemblerLimits::default())
    }

    /// Creates a plan-bound assembler with explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when the supplied digest is not in the
    /// request-plan domain.
    pub fn with_limits(
        expected_plan_digest: Digest,
        limits: AssemblerLimits,
    ) -> Result<Self, ModelError> {
        if expected_plan_digest.domain() != DigestDomain::RequestPlan {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "response assembler requires a request-plan digest",
            ));
        }
        Ok(Self {
            state: AssemblyState::AwaitingStart,
            expected_plan_digest,
            limits,
            events_seen: 0,
        })
    }

    /// Applies one event, returning the terminal aggregate exactly once.
    ///
    /// # Errors
    ///
    /// Returns a terminal protocol error when the event violates the assembly
    /// grammar or contradicts already assembled state.
    pub fn push(&mut self, event: ModelEvent) -> Result<Option<ModelResponse>, ModelError> {
        if matches!(
            event,
            ModelEvent::StreamActivity(_) | ModelEvent::PromptCacheDiagnosis(_)
        ) {
            return Ok(None);
        }
        self.events_seen = self.events_seen.saturating_add(1);
        if self.events_seen > self.limits.max_events() {
            return self.fail("model stream exceeded the event limit");
        }
        if let ModelEvent::ProviderEvent(remote) = &event
            && remote.payload().get().len() > self.limits.max_provider_event_bytes()
        {
            return self.fail("remote event exceeded the retained payload limit");
        }
        match event {
            ModelEvent::Diagnostic(_) | ModelEvent::ProviderEvent(_)
                if matches!(self.state, AssemblyState::AwaitingStart) =>
            {
                Ok(None)
            }
            ModelEvent::Retry(retry) => self.restart(retry.output_discarded()),
            ModelEvent::ResponseStarted(started) => self.start_response(started),
            ModelEvent::ResponseCompleted(response) => self.complete_response(response),
            other => self.apply_open_event(other),
        }
    }

    /// Validates EOF, rejecting it unless a terminal aggregate was assembled.
    ///
    /// # Errors
    ///
    /// Returns an incomplete-response error unless a valid terminal aggregate
    /// was assembled before EOF.
    pub fn finish_eof(&mut self) -> Result<(), ModelError> {
        match self.state {
            AssemblyState::Succeeded => Ok(()),
            AssemblyState::Failed => Err(invalid_event("assembler already failed")),
            AssemblyState::AwaitingStart | AssemblyState::Open(_) => {
                self.state = AssemblyState::Failed;
                Err(ModelError::incomplete())
            }
        }
    }

    /// A retry before any output leaves the assembly awaiting its start; one
    /// that discards output voids the open response so the replay starts over.
    fn restart(&mut self, output_discarded: bool) -> Result<Option<ModelResponse>, ModelError> {
        match self.state {
            AssemblyState::AwaitingStart => Ok(None),
            AssemblyState::Open(_) if output_discarded => {
                self.state = AssemblyState::AwaitingStart;
                Ok(None)
            }
            AssemblyState::Open(_) => self.fail("retry kept output of an open response"),
            AssemblyState::Succeeded | AssemblyState::Failed => {
                self.fail("retry after terminal state")
            }
        }
    }

    fn start_response(
        &mut self,
        started: ResponseStarted,
    ) -> Result<Option<ModelResponse>, ModelError> {
        if !matches!(self.state, AssemblyState::AwaitingStart) {
            return self.fail("response started more than once or after terminal state");
        }
        if started.plan_digest != self.expected_plan_digest {
            return self.fail("response start does not match the prepared request plan");
        }
        self.state = AssemblyState::Open(OpenAssembly {
            started,
            item_ids: BTreeMap::new(),
            blocks: BTreeMap::new(),
            usage: None,
        });
        Ok(None)
    }

    fn apply_open_event(&mut self, event: ModelEvent) -> Result<Option<ModelResponse>, ModelError> {
        let AssemblyState::Open(open) = &mut self.state else {
            return self.fail("event received outside an open response");
        };
        let result = match event {
            ModelEvent::BlockStarted(started) => add_started(open, started, self.limits),
            ModelEvent::TextDelta(delta) => append_delta(
                open,
                delta.location(),
                delta.bytes(),
                DeltaKind::Text,
                self.limits,
            ),
            ModelEvent::ReasoningDelta(delta) => append_delta(
                open,
                delta.location(),
                delta.bytes(),
                DeltaKind::Reasoning,
                self.limits,
            ),
            ModelEvent::ReasoningReplayBound(binding) => {
                bind_reasoning_replay(open, binding, self.limits)
            }
            ModelEvent::ReasoningVisibilityBound(binding) => {
                bind_reasoning_visibility(open, binding)
            }
            ModelEvent::ToolInputDelta(delta) => append_delta(
                open,
                delta.location(),
                delta.bytes(),
                DeltaKind::ToolInput,
                self.limits,
            ),
            ModelEvent::CitationAdded(citation) => add_citation(open, citation, self.limits),
            ModelEvent::BlockCompleted(completed) => complete_block(open, completed.location()),
            ModelEvent::Usage(update) => update_usage(open, update.usage()),
            ModelEvent::Diagnostic(_)
            | ModelEvent::ProviderEvent(_)
            | ModelEvent::StreamActivity(_)
            | ModelEvent::PromptCacheDiagnosis(_) => Ok(()),
            ModelEvent::Retry(_)
            | ModelEvent::ResponseStarted(_)
            | ModelEvent::ResponseCompleted(_) => {
                Err(invalid_event("invalid nested response event"))
            }
        };
        match result {
            Ok(()) => Ok(None),
            Err(error) => {
                self.state = AssemblyState::Failed;
                Err(error)
            }
        }
    }

    fn complete_response(
        &mut self,
        response: ModelResponse,
    ) -> Result<Option<ModelResponse>, ModelError> {
        let AssemblyState::Open(open) = &self.state else {
            return self.fail("terminal aggregate received outside an open response");
        };
        if let Err(error) = validate_completed_response(open, &response) {
            self.state = AssemblyState::Failed;
            return Err(error);
        }
        self.state = AssemblyState::Succeeded;
        Ok(Some(response))
    }

    fn fail<T>(&mut self, message: &'static str) -> Result<T, ModelError> {
        self.state = AssemblyState::Failed;
        Err(invalid_event(message))
    }
}

fn validate_completed_response(
    open: &OpenAssembly,
    response: &ModelResponse,
) -> Result<(), ModelError> {
    response.validate_for_commit()?;
    // Refusal can revoke local proposals before their provisional arguments
    // become valid canonical tool input. Other open blocks still fail closed.
    if response.finish.reason() == FinishReason::Refusal && response.output.is_empty() {
        validate_revoked_output(open)?;
    } else if assemble_items(open)? != response.output {
        return Err(invalid_event(
            "terminal aggregate output does not match streamed blocks",
        ));
    }
    if open.started.response_id != response.response_id {
        return Err(invalid_event(
            "terminal response identity does not match response start",
        ));
    }
    if open.started.plan_digest != response.plan_digest {
        return Err(invalid_event(
            "terminal plan digest does not match response start",
        ));
    }
    validate_response_metadata(&open.started.metadata, &response.metadata)?;
    if open.usage.is_some() && open.usage != response.usage {
        return Err(invalid_event(
            "terminal usage does not match the final usage update",
        ));
    }
    Ok(())
}

fn validate_revoked_output(open: &OpenAssembly) -> Result<(), ModelError> {
    let mut previous_item = None;
    let mut next_block = 0;
    for (location, block) in &open.blocks {
        if previous_item != Some(location.item_index) {
            let expected_item = previous_item.map_or(0, |index| index + 1);
            if location.item_index != expected_item {
                return Err(invalid_event("assistant item indices are not contiguous"));
            }
            previous_item = Some(location.item_index);
            next_block = 0;
        }
        if location.block_index != next_block {
            return Err(invalid_event("output block indices are not contiguous"));
        }
        next_block += 1;
        match block {
            PartialBlock::ToolCall {
                owner: ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. },
                state: ToolCallState::Requested,
                ..
            } => {}
            PartialBlock::Complete(block) => {
                if matches!(block, ContentBlock::ToolCall(call)
                    if call.state() != ToolCallState::Requested
                        || matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. }))
                    || matches!(
                        block,
                        ContentBlock::ToolResult(_) | ContentBlock::ToolResultUpdate(_)
                    )
                {
                    return Err(invalid_event(
                        "refusal cannot discard possible tool effects",
                    ));
                }
            }
            _ => {
                return Err(invalid_event(
                    "refusal arrived with an open non-client block",
                ));
            }
        }
    }
    Ok(())
}

fn validate_response_metadata(
    started: &ModelResponseMetadata,
    completed: &ModelResponseMetadata,
) -> Result<(), ModelError> {
    match (started.actual_model(), completed.actual_model()) {
        (Some(started), Some(completed)) if started == completed => {}
        (Some(_), None) => {
            return Err(invalid_event(
                "terminal response omitted the actual model reported at response start",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_event(
                "terminal actual model contradicts response start",
            ));
        }
        (None, _) => {}
    }
    match (
        started.provider_request_id(),
        completed.provider_request_id(),
    ) {
        (Some(started), Some(completed)) if started == completed => {}
        (Some(_), None) => {
            return Err(invalid_event(
                "terminal response omitted the provider request ID reported at response start",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_event(
                "terminal provider request ID contradicts response start",
            ));
        }
        (None, _) => {}
    }
    match (started.served_by(), completed.served_by()) {
        (Some(started), Some(completed)) if started == completed => {}
        (Some(_), None) => {
            return Err(invalid_event(
                "terminal response omitted the served endpoint reported at response start",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_event(
                "terminal served endpoint contradicts response start",
            ));
        }
        (None, _) => {}
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum DeltaKind {
    Text,
    Reasoning,
    ToolInput,
}

fn add_started(
    open: &mut OpenAssembly,
    started: BlockStarted,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    register_item(open, started.location.item_index, started.item_id, limits)?;
    if open.blocks.contains_key(&started.location) {
        return Err(invalid_event("output block started more than once"));
    }
    if open.blocks.len() >= limits.max_blocks() {
        return Err(invalid_event("model output exceeded the block limit"));
    }
    let partial = match started.block {
        BlockStart::Text { phase } => PartialBlock::Text {
            phase,
            bytes: Vec::new(),
        },
        BlockStart::Reasoning { visibility, replay } => PartialBlock::Reasoning {
            visibility: Some(visibility),
            replay,
            bytes: Vec::new(),
        },
        BlockStart::ReasoningPending { replay } => PartialBlock::Reasoning {
            visibility: None,
            replay,
            bytes: Vec::new(),
        },
        BlockStart::ToolCall {
            call_id,
            tool,
            owner,
            state,
            correlation,
        } => PartialBlock::ToolCall {
            call_id,
            tool,
            owner,
            state,
            correlation,
            bytes: Vec::new(),
        },
        BlockStart::Refusal => PartialBlock::Refusal(Vec::new()),
        BlockStart::Complete(block) => PartialBlock::Complete(block),
    };
    if partial.retained_bytes() > limits.max_block_bytes() {
        return Err(invalid_event("model output block exceeded the byte limit"));
    }
    open.blocks.insert(started.location, partial);
    Ok(())
}

fn add_citation(
    open: &mut OpenAssembly,
    citation: CitationAdded,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    register_item(open, citation.location.item_index, citation.item_id, limits)?;
    if open.blocks.contains_key(&citation.location) {
        return Err(invalid_event("citation location is already occupied"));
    }
    if open.blocks.len() >= limits.max_blocks() {
        return Err(invalid_event("model output exceeded the block limit"));
    }
    let block = ContentBlock::Citation(citation.citation);
    if block.retained_bytes() > limits.max_block_bytes() {
        return Err(invalid_event("citation exceeded the block byte limit"));
    }
    open.blocks
        .insert(citation.location, PartialBlock::Complete(block));
    Ok(())
}

fn register_item(
    open: &mut OpenAssembly,
    index: u32,
    id: EntryId,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    if open.item_ids.get(&index).is_some_and(|known| *known != id) {
        return Err(invalid_event("assistant item index changed identity"));
    }
    if !open.item_ids.contains_key(&index) && open.item_ids.len() >= limits.max_items() {
        return Err(invalid_event("model output exceeded the item limit"));
    }
    open.item_ids.entry(index).or_insert(id);
    Ok(())
}

fn append_delta(
    open: &mut OpenAssembly,
    location: OutputLocation,
    bytes: &[u8],
    kind: DeltaKind,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    if bytes.is_empty() {
        return Err(invalid_event("empty deltas are not canonical"));
    }
    let Some(block) = open.blocks.get_mut(&location) else {
        return Err(invalid_event("delta arrived before block start"));
    };
    if matches!(kind, DeltaKind::Reasoning) {
        match &*block {
            PartialBlock::Reasoning {
                visibility: None, ..
            } => {
                return Err(invalid_event(
                    "reasoning delta arrived before visibility was bound",
                ));
            }
            PartialBlock::Reasoning {
                visibility: Some(ReasoningVisibility::Visible | ReasoningVisibility::Summary),
                ..
            }
            | PartialBlock::Text { .. }
            | PartialBlock::ToolCall { .. }
            | PartialBlock::Refusal(_)
            | PartialBlock::Complete(_) => {}
            PartialBlock::Reasoning {
                visibility: Some(_),
                ..
            } => {
                return Err(invalid_event("reasoning delta targeted hidden reasoning"));
            }
        }
    }
    let Some(new_retained_bytes) = block.retained_bytes().checked_add(bytes.len()) else {
        return Err(invalid_event("streamed block byte length overflowed"));
    };
    if new_retained_bytes > limits.max_block_bytes() {
        return Err(invalid_event("streamed block exceeded the byte limit"));
    }
    let ((
        DeltaKind::Text,
        PartialBlock::Text { bytes: target, .. } | PartialBlock::Refusal(target),
    )
    | (DeltaKind::Reasoning, PartialBlock::Reasoning { bytes: target, .. })
    | (DeltaKind::ToolInput, PartialBlock::ToolCall { bytes: target, .. })) = (kind, block)
    else {
        return Err(invalid_event("delta kind does not match output block"));
    };
    target.extend_from_slice(bytes);
    Ok(())
}

fn bind_reasoning_replay(
    open: &mut OpenAssembly,
    binding: ReasoningReplayBound,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    let ReasoningReplayBound { location, replay } = binding;
    let Some(block) = open.blocks.get_mut(&location) else {
        return Err(invalid_event(
            "reasoning replay binding targeted an unknown block",
        ));
    };
    match block {
        PartialBlock::Reasoning {
            replay: target,
            bytes,
            ..
        } => {
            if target.is_some() {
                return Err(invalid_event("reasoning replay was bound more than once"));
            }
            let Some(retained_bytes) = bytes.len().checked_add(replay.payload().get().len()) else {
                return Err(invalid_event("reasoning block byte length overflowed"));
            };
            if retained_bytes > limits.max_block_bytes() {
                return Err(invalid_event(
                    "reasoning replay binding exceeded the block byte limit",
                ));
            }
            *target = Some(replay);
            Ok(())
        }
        PartialBlock::Complete(_) => Err(invalid_event(
            "reasoning replay binding arrived after block completion",
        )),
        PartialBlock::Text { .. } | PartialBlock::ToolCall { .. } | PartialBlock::Refusal(_) => {
            Err(invalid_event(
                "reasoning replay binding targeted a non-reasoning block",
            ))
        }
    }
}

fn bind_reasoning_visibility(
    open: &mut OpenAssembly,
    binding: ReasoningVisibilityBound,
) -> Result<(), ModelError> {
    let Some(block) = open.blocks.get_mut(&binding.location()) else {
        return Err(invalid_event(
            "reasoning visibility binding targeted an unknown block",
        ));
    };
    match block {
        PartialBlock::Reasoning {
            visibility, bytes, ..
        } => {
            if visibility.is_some() {
                return Err(invalid_event("reasoning visibility was already known"));
            }
            if !bytes.is_empty() {
                return Err(invalid_event(
                    "reasoning visibility was bound after reasoning bytes arrived",
                ));
            }
            *visibility = Some(binding.visibility());
            Ok(())
        }
        PartialBlock::Complete(_) => Err(invalid_event(
            "reasoning visibility binding arrived after block completion",
        )),
        PartialBlock::Text { .. } | PartialBlock::ToolCall { .. } | PartialBlock::Refusal(_) => {
            Err(invalid_event(
                "reasoning visibility binding targeted a non-reasoning block",
            ))
        }
    }
}

fn complete_block(open: &mut OpenAssembly, location: OutputLocation) -> Result<(), ModelError> {
    let Some(partial) = open.blocks.remove(&location) else {
        return Err(invalid_event("block completed before it started"));
    };
    if matches!(partial, PartialBlock::Complete(_)) {
        open.blocks.insert(location, partial);
        return Err(invalid_event("output block completed more than once"));
    }
    let complete = materialize(partial)?;
    open.blocks
        .insert(location, PartialBlock::Complete(complete));
    Ok(())
}

fn materialize(partial: PartialBlock) -> Result<ContentBlock, ModelError> {
    match partial {
        PartialBlock::Text { phase, bytes } => {
            let text = decode_utf8(bytes, "text block ended with invalid UTF-8")?;
            TextBlock::new(text, phase)
                .map(ContentBlock::Text)
                .map_err(|error| invalid_event(error.to_string()))
        }
        PartialBlock::Reasoning {
            visibility,
            replay,
            bytes,
        } => {
            let Some(visibility) = visibility else {
                return Err(invalid_event(
                    "reasoning block completed before visibility was bound",
                ));
            };
            let text = if bytes.is_empty() {
                None
            } else {
                Some(decode_utf8(
                    bytes,
                    "reasoning block ended with invalid UTF-8",
                )?)
            };
            ReasoningBlock::new(visibility, text, replay)
                .map(ContentBlock::Reasoning)
                .map_err(|error| invalid_event(error.to_string()))
        }
        PartialBlock::ToolCall {
            call_id,
            tool,
            owner,
            state,
            correlation,
            bytes,
        } => {
            let arguments = decode_utf8(bytes, "tool input ended with invalid UTF-8")?;
            let block = ToolCallBlock::new(call_id, tool, owner, state, correlation, arguments)
                .map_err(|error| invalid_event(error.to_string()))?;
            if block.arguments().is_none() {
                return Err(invalid_event("tool input ended with malformed JSON"));
            }
            Ok(ContentBlock::ToolCall(block))
        }
        PartialBlock::Refusal(bytes) => {
            let text = decode_utf8(bytes, "refusal ended with invalid UTF-8")?;
            RefusalBlock::new(text)
                .map(ContentBlock::Refusal)
                .map_err(|error| invalid_event(error.to_string()))
        }
        PartialBlock::Complete(block) => Ok(block),
    }
}

fn decode_utf8(bytes: Vec<u8>, message: &'static str) -> Result<String, ModelError> {
    String::from_utf8(bytes).map_err(|_| invalid_event(message))
}

fn update_usage(open: &mut OpenAssembly, usage: Usage) -> Result<(), ModelError> {
    if open
        .usage
        .is_some_and(|earlier| !usage.is_monotonic_after(earlier))
    {
        return Err(invalid_event(
            "usage updates must be cumulative and monotonic",
        ));
    }
    open.usage = Some(usage);
    Ok(())
}

fn assemble_items(open: &OpenAssembly) -> Result<Vec<ConversationItem>, ModelError> {
    if open
        .blocks
        .values()
        .any(|block| !matches!(block, PartialBlock::Complete(_)))
    {
        return Err(invalid_event(
            "terminal aggregate arrived with an open block",
        ));
    }
    let item_count = open.item_ids.len();
    let mut output = Vec::with_capacity(item_count);
    for item_index in 0..item_count {
        let item_index = u32::try_from(item_index)
            .map_err(|_| invalid_event("too many assistant output items"))?;
        let Some(item_id) = open.item_ids.get(&item_index) else {
            return Err(invalid_event("assistant item indices are not contiguous"));
        };
        let mut blocks = Vec::new();
        for (location, partial) in open
            .blocks
            .range(OutputLocation::new(item_index, 0)..=OutputLocation::new(item_index, u32::MAX))
        {
            let expected = u32::try_from(blocks.len())
                .map_err(|_| invalid_event("too many blocks in assistant output item"))?;
            if location.block_index != expected {
                return Err(invalid_event("output block indices are not contiguous"));
            }
            let PartialBlock::Complete(block) = partial else {
                return Err(invalid_event(
                    "terminal aggregate arrived with an open block",
                ));
            };
            blocks.push(block.clone());
        }
        let item =
            ConversationItem::new(*item_id, Actor::Assistant, blocks, Provenance::ModelOutput)
                .map_err(|error| invalid_event(error.to_string()))?;
        output.push(item);
    }
    Ok(output)
}
