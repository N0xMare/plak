use super::{
    AgentError, AgentErrorKind, Arc, ArtifactTransition, BTreeSet, CanonicalHistory, CommitPlan,
    ConversationView, Driver, DurableOperationKind, DurableOperationState,
    DurableOperationTransition, OperationId, PreparedCallRecord, RuntimeCheckpoint,
    SessionStoreError, TurnId, finalized_operation_evidence, operation_turn_id,
    runtime_manifest_error, settled_turn_operations, working_lifecycle,
};

impl Driver {
    /// A plan for the next atomic successor of the current authority.
    pub(in crate::runtime::driver) fn commit_plan(&mut self) -> Result<CommitPlan, AgentError> {
        Ok(CommitPlan {
            checkpoint_id: self.checkpoint_id()?,
            transitions: Vec::new(),
            appended: Vec::new(),
            finalize: BTreeSet::new(),
            finalize_turn: false,
            withhold_prompt: false,
            segments: Vec::new(),
            artifact: ArtifactTransition::Preserve,
            notices: Vec::new(),
        })
    }

    /// Publishes `plan` as one checkpoint CAS: its ledger edits, its history,
    /// the artifact transition and the finalization of every operation it
    /// names, bound to the committed history (spec 01 §3.5).
    #[allow(
        clippy::too_many_lines,
        reason = "one atomic successor is checked and assembled in one place"
    )]
    pub(in crate::runtime::driver) async fn publish_commit_plan(
        &mut self,
        turn_id: TurnId,
        plan: CommitPlan,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let CommitPlan {
            checkpoint_id,
            transitions,
            appended,
            finalize,
            finalize_turn,
            withhold_prompt,
            segments,
            artifact: artifact_transition,
            notices,
        } = plan;
        let mut required = finalize;
        let edits_ledger = !transitions.is_empty() || !appended.is_empty();
        let (history, mut ledger, context, prior_family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let rejected = || {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "canonical history rejected an atomic semantic group",
                )
            };
            let mut history = if withhold_prompt {
                let mut retained = manifest.canonical().history().segments().to_vec();
                retained.pop();
                CanonicalHistory::new(retained).map_err(|_| rejected())?
            } else {
                manifest.canonical().history().clone()
            };
            for segment in segments {
                history = history.append(segment).map_err(|_| rejected())?;
            }
            let mut ledger = manifest.ledger().clone();
            for (operation_id, state) in transitions {
                ledger = ledger
                    .transition_operation(
                        operation_id.0,
                        DurableOperationTransition::new(checkpoint_id, state),
                    )
                    .map_err(runtime_manifest_error)?;
            }
            for record in appended {
                ledger = ledger
                    .append_operation(record)
                    .map_err(runtime_manifest_error)?;
            }
            (
                history,
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        if finalize_turn {
            required.extend(settled_turn_operations(&ledger, turn_id)?);
        }
        if (required.is_empty() && !edits_ledger)
            || required.iter().any(|operation_id| {
                !ledger.operations().into_iter().any(|record| {
                    record.operation_id() == operation_id.0
                        && operation_turn_id(record) == Some(turn_id)
                        && (matches!(
                            record.state(),
                            DurableOperationState::Completed(_)
                                | DurableOperationState::ProvenNotDispatched(_)
                        ) || (matches!(
                            record.kind(),
                            DurableOperationKind::ToolBatchContinuation { .. }
                                | DurableOperationKind::ServerPauseContinuation { .. }
                        ) && matches!(
                            record.state(),
                            DurableOperationState::ContinuationRequired(_)
                        )))
                })
            })
        {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "canonical promotion lacks a proven completed operation",
            ));
        }
        let family = match artifact_transition {
            ArtifactTransition::Preserve => prior_family,
            ArtifactTransition::Clear => None,
            ArtifactTransition::Replace {
                candidate,
                plan,
                response_output_items,
            } => {
                let covered_items = plan
                    .request()
                    .input()
                    .items()
                    .len()
                    .checked_add(response_output_items)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                let covered = ConversationView::from_shared(
                    history.clone(),
                    plakous_model::WorkingProjection::default(),
                )
                .canonical_item_prefix_identity(covered_items)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                candidate
                    .validate_for_commit(&plan, covered, response_output_items)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Model,
                            "family artifact does not match the committed provider response",
                        )
                    })?;
                let prepared = ledger
                    .operations()
                    .into_iter()
                    .filter(|record| {
                        required.contains(&OperationId(record.operation_id()))
                            && matches!(record.kind(), DurableOperationKind::Provider { .. })
                    })
                    .map(|record| {
                        serde_json::from_slice::<PreparedCallRecord>(record.request().bytes())
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|prepared| prepared.projection().plan() == plan.digest())
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                Some(crate::runtime::family_artifact::promote_family_artifact(
                    &candidate,
                    &prepared,
                    &history,
                    &context,
                    plan.request().input().items().len(),
                    response_output_items,
                )?)
            }
        };
        for operation_id in required {
            let evidence = ledger
                .operation(operation_id.0)
                .and_then(|record| match record.state() {
                    DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence)
                    | DurableOperationState::ContinuationRequired(evidence)
                        if matches!(
                            record.kind(),
                            DurableOperationKind::ToolBatchContinuation { .. }
                                | DurableOperationKind::ServerPauseContinuation { .. }
                        ) || !matches!(
                            record.state(),
                            DurableOperationState::ContinuationRequired(_)
                        ) =>
                    {
                        Some(evidence)
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "finalized operation lost its proven terminal evidence",
                    )
                })?;
            let evidence =
                finalized_operation_evidence(evidence, checkpoint_id, history.identity())?;
            ledger = ledger
                .transition_operation(
                    operation_id.0,
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Finalized(evidence),
                    ),
                )
                .map_err(runtime_manifest_error)?;
        }
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_turn_terminal(
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
            &notices,
        )
        .await
    }
}
