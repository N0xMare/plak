//! Message start, delta and stop transitions of the stream projector.

use plakous_model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, ContentBlock, ConversationItem, Digest,
    DigestDomain, EntryId, FamilyArtifactProducerFacts, ModelEvent, ModelId,
    ModelProviderRequestId, ModelResponse, ModelResponseMetadata, OutputLocation, Provenance,
    ResponseStarted, ToolCallBlock, ToolCallState, ToolCallerLineage, ToolCorrelation,
    ToolInputDelta, UsageUpdate,
};

use crate::request::{
    messages_artifact_binding, messages_artifact_configuration_identity, messages_tool_set_identity,
};

use super::{
    MessageContainer, MessageStopEvent, MessagesStopReason, MessagesStreamError,
    MessagesStreamErrorKind, MessagesStreamErrorPhase, MessagesStreamExtensions,
    MessagesStreamProjector, OpenBlock, StreamState, invalid_response, map_finish, parse_event,
    parse_message_delta, parse_message_start, parse_stop_details, validate_stop_blocks,
};

impl MessagesStreamProjector {
    pub(super) fn message_start(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if !matches!(self.state, StreamState::AwaitingStart) {
            return Err(self.fail("message_start was duplicated or out of order"));
        }
        let (event, cache_diagnosis) =
            parse_message_start(data, self.prepared.dialect.extensions())?;
        if event.message.kind != "message" || event.message.role != "assistant" {
            return Err(self.fail("message_start did not contain an assistant message"));
        }
        if !event.message.content.is_empty() {
            return Err(self.fail("streaming message_start unexpectedly contained content"));
        }
        if event.message.id.is_empty() {
            return Err(self.fail("message_start contained an empty message ID"));
        }
        if event.message.stop_reason.is_some()
            || event.message.stop_sequence.is_some()
            || event.message.stop_details.is_some()
        {
            return Err(self.fail("streaming message_start unexpectedly contained terminal state"));
        }
        if let Some(container) = event.message.container {
            self.bind_container(container, "message_start")?;
        }
        let model = ModelId::new(event.message.model)
            .map_err(|error| invalid_response(format!("invalid reported model ID: {error}")))?;
        if self
            .prepared
            .expected_reported_model()
            .is_some_and(|expected| expected != &model)
        {
            return Err(invalid_response(
                "provider-reported Messages model did not match the prepared descriptor",
            ));
        }
        let item_id =
            EntryId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|error| {
                invalid_response(format!("failed to generate assistant item ID: {error}"))
            })?;
        let provider_usage = event.message.usage.into_provider();
        let usage = provider_usage
            .into_canonical()
            .map_err(|message| self.invalid_accounting(message))?;
        if self.provider_request_id.is_none()
            && self.prepared.dialect.extensions() == MessagesStreamExtensions::OpenRouterV1
        {
            self.provider_request_id = Some(
                ModelProviderRequestId::new(event.message.id.clone()).map_err(|_| {
                    invalid_response(
                        "OpenRouter message_start ID was unsafe for request attribution",
                    )
                })?,
            );
        }
        let metadata =
            ModelResponseMetadata::new(Some(model.clone()), self.provider_request_id.clone());
        self.message_id = Some(event.message.id);
        self.reported_model = Some(model);
        self.item_id = Some(item_id);
        self.usage = Some(usage);
        self.provider_usage = Some(provider_usage);
        self.state = StreamState::Open;
        if let Some(diagnosis) = cache_diagnosis {
            output.push(ModelEvent::PromptCacheDiagnosis(diagnosis));
        }
        output.push(ModelEvent::ResponseStarted(
            ResponseStarted::new(self.message_id.clone(), self.prepared.plan_digest())
                .with_metadata(metadata),
        ));
        output.push(ModelEvent::Usage(UsageUpdate::new(usage)));
        Ok(())
    }
}

impl MessagesStreamProjector {
    pub(super) fn message_delta(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        if !self.open_blocks.is_empty() {
            return Err(self.fail("message_delta arrived before content block stop"));
        }
        if self.stop_reason.is_some() || self.stop_sequence.is_some() || self.stop_details.is_some()
        {
            return Err(self.fail("message_delta arrived after terminal message metadata"));
        }
        let event = parse_message_delta(data, self.prepared.dialect.extensions())?;
        if let Some(container) = event.delta.container {
            self.bind_container(container, "message_delta")?;
        }
        self.flush_deferred_programmatic_blocks(output)?;
        if let Some(stop_reason) = event.delta.stop_reason {
            if self.stop_reason.is_some() {
                return Err(self.fail("message stop reason was supplied more than once"));
            }
            self.stop_reason = Some(MessagesStopReason::parse(&stop_reason)?);
        }
        if self
            .stop_reason
            .is_some_and(|reason| !reason.revokes_client_input())
        {
            for (index, input) in std::mem::take(&mut self.closed_client_blocks) {
                let location = OutputLocation::new(0, index);
                let block = self.complete_tool_input(input, location, output)?;
                self.blocks.insert(index, block);
                output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
            }
        }
        match (self.stop_sequence.is_some(), event.delta.stop_sequence) {
            (true, Some(_)) => {
                return Err(self.fail("message stop sequence was supplied more than once"));
            }
            (false, Some(stop_sequence)) => self.stop_sequence = Some(stop_sequence),
            (_, None) => {}
        }
        if let Some(stop_details) = event.delta.stop_details {
            if self.stop_details.is_some() {
                return Err(self.fail("message stop details were supplied more than once"));
            }
            self.stop_details = Some(parse_stop_details(stop_details)?);
        }
        let previous = self
            .provider_usage
            .ok_or_else(|| self.fail("message usage was not initialized"))?;
        let provider_usage = previous
            .merge(event.usage.into_family())
            .map_err(|message| self.invalid_accounting(message))?;
        let usage = provider_usage
            .into_canonical()
            .map_err(|message| self.invalid_accounting(message))?;
        self.provider_usage = Some(provider_usage);
        self.usage = Some(usage);
        output.push(ModelEvent::Usage(UsageUpdate::new(usage)));
        Ok(())
    }

    fn flush_deferred_programmatic_blocks(
        &mut self,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if self.deferred_programmatic_blocks.is_empty() {
            return Ok(());
        }
        let container = self
            .container_id
            .clone()
            .ok_or_else(|| self.fail("programmatic tool use requires a message_delta container"))?;
        let item_id = self.item_id()?;
        let deferred = std::mem::take(&mut self.deferred_programmatic_blocks);
        let provider_kind = self
            .prepared
            .dialect
            .programmatic_tool_namespace()
            .ok_or_else(|| self.fail("Messages stream policy omitted hosted-tool identity"))?;
        for (index, block) in deferred {
            let OpenBlock::DeferredProgrammaticToolUse {
                call_id,
                remote_id,
                name,
                owner,
                execution_id,
                json,
            } = block
            else {
                return Err(self.fail("invalid deferred Messages block"));
            };
            let location = OutputLocation::new(0, index);
            let lineage =
                ToolCallerLineage::provider_programmatic(provider_kind.clone(), execution_id)
                    .map_err(|error| invalid_response(error.to_string()))?;
            let correlation =
                ToolCorrelation::new(Some(remote_id.clone()), Some(container.clone()), lineage)
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
            output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location,
                json.as_bytes(),
            )));
            let canonical = ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id,
                    name,
                    owner,
                    ToolCallState::Requested,
                    correlation,
                    json,
                )
                .map_err(|error| invalid_response(error.to_string()))?,
            );
            if self.blocks.insert(index, canonical).is_some() {
                return Err(self.fail("deferred Messages block completed more than once"));
            }
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }

    fn ordered_blocks(&mut self) -> Result<Vec<ContentBlock>, MessagesStreamError> {
        if self
            .blocks
            .keys()
            .any(|index| *index >= self.next_output_index)
        {
            return Err(self.fail("Messages completed a content block outside the started range"));
        }
        Ok(self.blocks.values().cloned().collect())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "terminal validation and exact artifact sealing remain one ordered boundary"
    )]
    pub(super) fn message_stop(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        parse_event::<MessageStopEvent>(data)?;
        if !self.open_blocks.is_empty() {
            return Err(self.fail("message_stop arrived with an open content block"));
        }
        let stop_reason = self
            .stop_reason
            .ok_or_else(|| self.fail("message_stop arrived without a terminal stop_reason"))?;
        // Refusal revokes the entire provisional response. An output limit
        // revokes it only when it closed client tool input, which may be cut
        // anywhere and so never becomes a call; the driver classifies the
        // empty Length as an output-limit failure.
        let revoked = stop_reason == MessagesStopReason::Refusal
            || (stop_reason.is_output_limit() && !self.closed_client_blocks.is_empty());
        if !revoked {
            self.complete_citations(output)?;
            self.retain_empty_skip_marker(output)?;
        }
        self.skipped_diagnostic(output);
        let blocks = self.ordered_blocks()?;
        if revoked && self.hosted_effects_possible {
            return Err(MessagesStreamError::new(
                MessagesStreamErrorKind::UnsupportedSemantic,
                MessagesStreamErrorPhase::Complete,
                if stop_reason == MessagesStopReason::Refusal {
                    "Messages refusal after possible hosted effects requires reconciliation"
                } else {
                    "Messages output limit after possible hosted effects requires reconciliation"
                },
            ));
        }
        validate_stop_blocks(
            stop_reason,
            &blocks,
            self.prepared.hosted_effect_capable(),
            self.container_id.as_deref(),
        )?;
        let (finish, stop_outcome) = map_finish(
            stop_reason,
            self.stop_sequence.clone(),
            self.stop_details.clone(),
        )?;
        let item_id = self.item_id()?;
        // A revoked response drops every provisional block, including tool
        // proposals and signed thinking. Nothing from it is eligible for replay.
        let items = if revoked || blocks.is_empty() {
            Vec::new()
        } else {
            vec![
                ConversationItem::new(
                    item_id,
                    Actor::Assistant,
                    blocks.clone(),
                    Provenance::ModelOutput,
                )
                .map_err(|error| invalid_response(error.to_string()))?,
            ]
        };
        let metadata = ModelResponseMetadata::new(
            self.reported_model.clone(),
            self.provider_request_id.clone(),
        );
        let mut response = ModelResponse::new(
            items,
            finish,
            self.usage,
            self.message_id.clone(),
            self.prepared.plan_digest(),
        )
        .with_metadata(metadata);
        if self.hosted_effects_possible && self.prepared.artifact_plan.is_some() {
            let plan = self.prepared.artifact_plan.clone().ok_or_else(|| {
                self.fail("hosted Messages response omitted artifact production authority")
            })?;
            let operation = self
                .prepared
                .operation_digest
                .ok_or_else(|| self.fail("hosted Messages response omitted operation identity"))?;
            let container = self
                .container_id
                .clone()
                .ok_or_else(|| self.fail("hosted Messages response omitted its exact container"))?;
            let mut native_container = None;
            let mut artifact_lineage = self.prepared.active_artifact_lineage.clone();
            let artifact_policy = self.prepared.request_policy.clone();
            let artifact_ownership = self.prepared.tool_ownership.clone();
            let (mut native_blocks, _) = crate::request::translate_assistant_bound(
                &blocks,
                &artifact_policy,
                &artifact_ownership,
                true,
                true,
                crate::request::AssistantReplayState {
                    container: &mut native_container,
                    server_lineage: &mut artifact_lineage,
                    diagnostics: &mut Vec::new(),
                },
            )
            .map_err(|error| self.fail(error.to_string()))?;
            for block in &mut native_blocks {
                if let crate::request::MessageBlock::ToolUse { id, caller, .. } = block
                    && self.explicit_direct_tool_ids.contains(id)
                {
                    *caller = Some(crate::request::ToolCallerParam {
                        kind: "direct".to_owned(),
                        tool_id: None,
                    });
                }
            }
            if native_container
                .as_deref()
                .is_some_and(|value| value != container)
            {
                return Err(self.fail("hosted Messages native blocks changed container"));
            }
            let tool_set =
                messages_tool_set_identity(plan.request(), &artifact_policy, &artifact_ownership)
                    .map_err(|error| self.fail(error.to_string()))?;
            let configuration =
                messages_artifact_configuration_identity(&artifact_policy, plan.digest())
                    .map_err(|error| self.fail(error.to_string()))?;
            let binding = messages_artifact_binding(&artifact_policy, tool_set, configuration)
                .map_err(|error| self.fail(error.to_string()))?;
            let covered = plan
                .request()
                .input()
                .staged_canonical_item_prefix_identity(response.output())
                .map_err(|error| self.fail(error.to_string()))?;
            let artifact = crate::artifact::MessagesFamilyArtifact::new(
                crate::artifact::MessagesArtifactProduction {
                    binding,
                    producer_plan: plan.digest(),
                    tool_set,
                    configuration,
                    covered,
                    container,
                    prior_lineage: self.prepared.active_artifact_lineage.clone(),
                    assistant_blocks: native_blocks,
                    stop: crate::artifact::MessagesArtifactStop {
                        reason: stop_reason.wire_name().to_owned(),
                        sequence: self.stop_sequence.clone(),
                        details: self
                            .stop_details
                            .as_ref()
                            .map(|details| details.raw.clone()),
                    },
                },
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let request = plan.request();
            let source = request
                .input()
                .canonical_item_prefix_identity(request.input().items().len())
                .map_err(|error| self.fail(error.to_string()))?;
            let physical = Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[
                    b"plakous-messages/physical-operation-v1",
                    &operation.identity_bytes(),
                ],
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let terminal = Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[
                    b"plakous-messages/terminal-operation-v1",
                    &operation.identity_bytes(),
                    self.message_id.as_deref().unwrap_or_default().as_bytes(),
                ],
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let producer = FamilyArtifactProducerFacts::new(
                request.identity(),
                request.model_input_identity(),
                plan.digest(),
                None,
                request.metadata().run_id(),
                request.metadata().root(),
                request.metadata().attempt_id(),
                source.identity(),
                u32::try_from(source.item_count())
                    .map_err(|_| self.fail("Messages source item count overflow"))?,
                1,
                covered.identity(),
                u32::try_from(covered.item_count())
                    .map_err(|_| self.fail("Messages covered item count overflow"))?,
                physical,
                terminal,
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let candidate = artifact
                .seal_candidate(producer)
                .map_err(|error| self.fail(error.to_string()))?;
            response = response
                .with_family_artifact_candidate(candidate)
                .map_err(|error| self.fail(error.to_string()))?;
        }
        self.stop_outcome = Some(stop_outcome);
        self.terminal_response = Some(response.clone());
        output.push(ModelEvent::ResponseCompleted(response));
        self.state = match self.prepared.dialect.extensions() {
            MessagesStreamExtensions::Strict | MessagesStreamExtensions::ClaudeOauthV1 => {
                StreamState::Stopped
            }
            MessagesStreamExtensions::OpenRouterV1 => StreamState::AwaitingGatewayDone,
        };
        Ok(())
    }

    /// Admits and binds the code-execution container reported by a
    /// message-family event, rejecting an unadmitted, malformed, or
    /// lineage-changing identity.
    fn bind_container(
        &mut self,
        container: MessageContainer,
        event: &str,
    ) -> Result<(), MessagesStreamError> {
        if !self.prepared.hosted_effect_capable()
            || container.id.is_empty()
            || container.id.len() > 512
        {
            return Err(self.fail(format!(
                "Messages {event} container is not admitted or valid"
            )));
        }
        if self
            .prepared
            .expected_container
            .as_deref()
            .is_some_and(|expected| expected != container.id)
        {
            return Err(self.fail(format!(
                "Messages {event} container changed active artifact lineage"
            )));
        }
        match self.container_id.as_deref() {
            Some(bound) if bound != container.id => {
                Err(self.fail(format!("Messages {event} repeated a conflicting container")))
            }
            Some(_) => Ok(()),
            None => {
                self.container_id = Some(container.id);
                Ok(())
            }
        }
    }
}
