use super::{
    ActiveFamilyArtifact, AdmittedRound, AgentError, AgentErrorKind, AgentEventKind, Arc,
    ArtifactTransition, AttemptId, ConversationItem, ConversationView, Driver, EventContext,
    FamilyArtifact, HistorySegment, HistorySegmentKind, ModelCacheAffinity, ModelError,
    ModelErrorKind, ModelErrorPhase, ModelRequest, OperationId, PromptCommand, RequestMetadata,
    RuntimeCheckpoint, TurnId, history_segment_kind, retained_tool_exchange_failure,
    runtime_manifest_error, shrink_tool_exchange_observations, tool_exchange_shrink_budget,
};

impl Driver {
    pub(in crate::runtime::driver) async fn promote_tool_exchange_within_next_round_capacity(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        mut batch: Vec<ConversationItem>,
        operations: Vec<OperationId>,
        artifact_transition: ArtifactTransition,
    ) -> Result<Option<AgentError>, AgentError> {
        match self
            .admit_pending_tool_exchange(command, &batch, &artifact_transition)
            .await
        {
            Ok(()) => {
                self.promote_completed_operations_with_history(
                    command.turn_id,
                    Some(HistorySegmentKind::ToolExchange),
                    batch,
                    operations,
                    artifact_transition,
                )
                .await?;
                Ok(None)
            }
            Err(overflow)
                if overflow
                    .model_error()
                    .and_then(ModelError::context_capacity)
                    .is_some() =>
            {
                // Never fail the turn with the unshrunk overflow after the
                // retained projection has been truncated. Inspect and compact
                // see the promoted checkpoint; the operator error must match.
                // A fair split of the excess is tried first; if the estimate
                // misses, every result is exhausted as before.
                let budget = overflow
                    .model_error()
                    .and_then(ModelError::context_capacity)
                    .and_then(|failure| tool_exchange_shrink_budget(&batch, failure.overflow()));
                let (mut shrunk, mut replacements) =
                    shrink_tool_exchange_observations(batch.clone(), budget)?;
                let mut shrunk_admit = self
                    .admit_pending_tool_exchange(command, &shrunk, &artifact_transition)
                    .await;
                if budget.is_some()
                    && shrunk_admit.as_ref().is_err_and(|error| {
                        error
                            .model_error()
                            .and_then(ModelError::context_capacity)
                            .is_some()
                    })
                {
                    (shrunk, replacements) = shrink_tool_exchange_observations(batch, None)?;
                    shrunk_admit = self
                        .admit_pending_tool_exchange(command, &shrunk, &artifact_transition)
                        .await;
                }
                batch = shrunk;
                self.observation_budget.mark_capacity_exhausted();
                self.promote_completed_operations_with_history(
                    command.turn_id,
                    Some(HistorySegmentKind::ToolExchange),
                    batch,
                    operations,
                    artifact_transition,
                )
                .await?;
                for replacement in replacements {
                    let _ = command.shared.events.publish(
                        EventContext::call(command.turn_id, attempt_id, replacement.call_id),
                        AgentEventKind::ObservationReplaced {
                            bytes_before: replacement.bytes_before,
                            bytes_after: replacement.bytes_after,
                        },
                    );
                }
                retained_tool_exchange_failure(shrunk_admit)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn admit_pending_tool_exchange(
        &mut self,
        command: &PromptCommand,
        batch: &[ConversationItem],
        artifact_transition: &ArtifactTransition,
    ) -> Result<(), AgentError> {
        if batch.is_empty() {
            return Ok(());
        }
        let segment = HistorySegment::new(HistorySegmentKind::ToolExchange, batch.to_vec())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "pending tool exchange could not be grouped for admission",
                )
            })?;
        let current = self.conversation();
        let history = current.history().clone().append(segment).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "pending tool exchange could not be appended for admission",
            )
        })?;
        let view = ConversationView::from_shared(history, current.working().clone());
        let attempt_id = self.attempt_id()?;
        let prepared_at = self.clock.now();
        let request = ModelRequest::from_parts(
            self.stable_request_prefix.clone(),
            view,
            RequestMetadata::for_turn(self.run_id, command.turn_id, attempt_id, prepared_at),
        )
        .map_err(|error| {
            self.project_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!("canonical model request is invalid: {error}"),
                ),
                ModelErrorPhase::Preparation,
            )
        })?;
        let descriptor = self.provider_configuration.effective().descriptor().clone();
        let plan = self.validate_round_request(&descriptor, request)?;
        let (active_family_artifact, cache_affinity) =
            self.pending_model_preparation_state(artifact_transition)?;
        let (prepared, prepared_at) = self
            .prepare_round_call(
                &descriptor,
                &plan,
                prepared_at,
                active_family_artifact.clone(),
                cache_affinity.clone(),
            )
            .await?;
        // The next round reads the same history, artifact and ledger once the
        // exchange is promoted; it dispatches this request unless one changed.
        self.admitted_round = Some(Box::new(AdmittedRound {
            turn_id: command.turn_id,
            attempt_id,
            plan,
            prepared: std::sync::Mutex::new(prepared),
            prepared_at,
            provider_configuration: Arc::clone(&self.provider_configuration),
            operation_count: self
                .authority
                .checkpoint()
                .manifest()
                .ledger()
                .operation_count(),
            request_anchor: self.request_anchor.clone(),
            family_artifact: active_family_artifact,
            cache_affinity,
        }));
        Ok(())
    }

    /// Artifact the next model round will see after this tool-exchange promote.
    ///
    /// Peek must not use the still-current checkpoint artifact: that envelope
    /// covers the previous round and treats this round's encrypted output as an
    /// unauthorized replay tail.
    pub(super) fn pending_model_preparation_state(
        &self,
        transition: &ArtifactTransition,
    ) -> Result<(Option<ActiveFamilyArtifact>, ModelCacheAffinity), AgentError> {
        let (current, cache_affinity) = self.active_model_preparation_state()?;
        let artifact = match transition {
            ArtifactTransition::Preserve => current,
            ArtifactTransition::Clear => None,
            ArtifactTransition::Replace { candidate, .. } => Some(
                FamilyArtifact::from_candidate(candidate)
                    .and_then(|artifact| artifact.to_active())
                    .map_err(runtime_manifest_error)?,
            ),
        };
        Ok((artifact, cache_affinity))
    }

    pub(in crate::runtime::driver) async fn promote_completed_operations_with_history<I, O>(
        &mut self,
        turn_id: TurnId,
        kind: Option<HistorySegmentKind>,
        items: I,
        required: O,
        artifact_transition: ArtifactTransition,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError>
    where
        I: IntoIterator<Item = ConversationItem>,
        O: IntoIterator<Item = OperationId>,
    {
        let items = items.into_iter().collect::<Vec<_>>();
        let segments = if items.is_empty() {
            Vec::new()
        } else {
            vec![
                HistorySegment::new(kind.unwrap_or_else(|| history_segment_kind(&items)), items)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "canonical history rejected an atomic semantic group",
                        )
                    })?,
            ]
        };
        let mut plan = self.commit_plan()?;
        plan.segments = segments;
        plan.finalize.extend(required);
        plan.artifact = artifact_transition;
        self.publish_commit_plan(turn_id, plan).await
    }
}
