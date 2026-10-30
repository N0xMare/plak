use super::{
    AgentError, AgentErrorKind, ArtifactTransition, AttemptId, ContinuationToolSlot, Driver,
    DurableAgentError, DurableOperationKind, DurableOperationState, EntryId, HistorySegmentKind,
    ModelResponse, OperationId, PromptCommand, SessionStoreError, ToolCallBlock,
    ToolExecutionResult, canonicalize_observed_tool_execution, tool_operation_terminal_state,
};

impl Driver {
    /// Closes the exchange of a batch that stopped with `error` and returns the
    /// error that ends the turn. An outcome-unknown stop keeps its working
    /// obligations for recovery instead, and an internal stop (a contained
    /// panic or broken driver invariant) does not rebuild history from state it
    /// can no longer vouch for. A close that itself fails on an
    /// unresolved effect ends the turn with that stronger error; any other close
    /// failure leaves the original failure in charge of the terminal.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact interrupted batch and its provider round"
    )]
    pub(in crate::runtime::driver) async fn close_interrupted_tool_batch(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        calls: &[ToolCallBlock],
        result_entry_ids: &[EntryId],
        provider_operation: OperationId,
        provider_response: &ModelResponse,
        restored_slots: Option<&[ContinuationToolSlot]>,
        error: AgentError,
    ) -> AgentError {
        if matches!(
            error.kind(),
            AgentErrorKind::OutcomeUnknown | AgentErrorKind::Internal
        ) {
            return error;
        }
        match self
            .promote_interrupted_tool_batch(
                command,
                attempt_id,
                calls,
                result_entry_ids,
                provider_operation,
                provider_response,
                restored_slots,
                &error,
            )
            .await
        {
            Err(close)
                if error.kind() == AgentErrorKind::Cancelled
                    || close.kind() == AgentErrorKind::OutcomeUnknown =>
            {
                close
            }
            _ => error,
        }
    }

    /// A batch interrupted by cancellation or an operational failure may have
    /// completed effects before it stopped. Rebuild from durable effect
    /// outcomes, not from published observations, and close every proposed call
    /// in one valid tool exchange before the root's terminal is published, so
    /// the next turn's model sees what already ran. A call that never completed
    /// gets a typed failure result naming `interruption`.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one interruption close reconstructs every source slot and its durable operations"
    )]
    pub(in crate::runtime::driver) async fn promote_interrupted_tool_batch(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        calls: &[ToolCallBlock],
        result_entry_ids: &[EntryId],
        provider_operation: OperationId,
        provider_response: &ModelResponse,
        restored_slots: Option<&[ContinuationToolSlot]>,
        interruption: &AgentError,
    ) -> Result<(), AgentError> {
        // Provider-programmatic calls carry a server continuation whose next
        // request has stricter adjacency rules than an ordinary local batch.
        if calls.iter().any(|call| {
            call.correlation()
                .caller_lineage()
                .provider_programmatic_kind()
                .is_some()
        }) {
            return Ok(());
        }
        let authority = self.authority.checkpoint();
        let mut effects = std::collections::BTreeMap::new();
        let mut markers = std::collections::BTreeMap::new();
        for record in authority.manifest().ledger().operations() {
            let (turn_id, recorded_attempt, call_id, source_index, marker) = match record.kind() {
                DurableOperationKind::Effect {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index,
                } => (turn_id, attempt_id, call_id, source_index, false),
                DurableOperationKind::CompositionEvaluation {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index,
                }
                | DurableOperationKind::CompositionContinuation {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index,
                    ..
                } => (turn_id, attempt_id, call_id, source_index, true),
                _ => continue,
            };
            if *turn_id == command.turn_id
                && *recorded_attempt == attempt_id
                && calls
                    .get(*source_index as usize)
                    .is_some_and(|call| call.call_id() == *call_id)
            {
                let target = if marker { &mut markers } else { &mut effects };
                if target
                    .insert(*source_index as usize, (*record).clone())
                    .is_some()
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
        }
        let mut items = provider_response.output().to_vec();
        let mut operations = vec![provider_operation];
        for (index, (call, result_entry_id)) in calls.iter().zip(result_entry_ids).enumerate() {
            if let Some(record) = markers.remove(&index) {
                // A cell that yielded earlier in this batch is still live and
                // owned here: its marker already holds the yielded result, and
                // the root boundary terminates the cell and settles the marker.
                let live_yield = matches!(record.state(), DurableOperationState::LocatorBound(_))
                    && self.local_composition.as_ref().and_then(
                        crate::runtime::composition::CompositionOwner::evaluation_operation,
                    ) == Some(OperationId(record.operation_id()));
                let (item, nested_operations) =
                    self.cancelled_composition_result(call, *result_entry_id, &record, live_yield)?;
                if matches!(
                    record.state(),
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_)
                ) {
                    operations.push(OperationId(record.operation_id()));
                }
                operations.extend(nested_operations);
                items.push(item);
                continue;
            }
            let outcome = effects.remove(&index);
            let item = if let Some(record) = outcome {
                if matches!(record.state(), DurableOperationState::Prepared) {
                    let operation_id = OperationId(record.operation_id());
                    let terminal = tool_operation_terminal_state(
                        &Err(interruption.clone()),
                        false,
                        *result_entry_id,
                    )?;
                    self.transition_operation(command.turn_id, operation_id, terminal)
                        .await?;
                    operations.push(operation_id);
                    self.undispatched_tool_result(call, *result_entry_id, interruption)?
                } else {
                    let (DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence)) = record.state()
                    else {
                        return Err(AgentError::new(
                            AgentErrorKind::OutcomeUnknown,
                            "interrupted tool batch contains an unresolved effect",
                        ));
                    };
                    operations.push(OperationId(record.operation_id()));
                    let (tag, evidence_entry, value): (String, EntryId, serde_json::Value) =
                        serde_json::from_slice(evidence.bytes())
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    if evidence_entry != *result_entry_id {
                        return Err(AgentError::from_store(SessionStoreError::corrupt()));
                    }
                    if let Some(ContinuationToolSlot::Completed {
                        operation_id,
                        result_entry_id: restored_entry_id,
                        item,
                        ..
                    }) = restored_slots.and_then(|slots| slots.get(index))
                        && *operation_id == OperationId(record.operation_id())
                        && *restored_entry_id == *result_entry_id
                    {
                        item.clone()
                    } else if tag == "completed-tool-outcome-v1" {
                        let execution: ToolExecutionResult = serde_json::from_value(value)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        canonicalize_observed_tool_execution(
                            *result_entry_id,
                            execution,
                            &mut self.observation_budget,
                        )?
                        .0
                    } else if tag == "failed-tool-outcome-v1" {
                        let failure: DurableAgentError = serde_json::from_value(value)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        let failure = failure.into_error()?;
                        self.failed_tool_result(call, *result_entry_id, &failure)?
                    } else {
                        return Err(AgentError::from_store(SessionStoreError::corrupt()));
                    }
                }
            } else {
                self.undispatched_tool_result(call, *result_entry_id, interruption)?
            };
            items.push(item);
        }
        if !effects.is_empty() || !markers.is_empty() || calls.len() != result_entry_ids.len() {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        self.promote_completed_operations_with_history(
            command.turn_id,
            Some(HistorySegmentKind::ToolExchange),
            items,
            operations,
            ArtifactTransition::Clear,
        )
        .await?;
        Ok(())
    }
}
