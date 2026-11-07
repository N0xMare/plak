use super::{
    AgentError, AgentErrorKind, Arc, CommitPlan, ConversationItem, Digest, Driver,
    DurableOperationState, DurableOperationTransition, EffectDecisionResolution, HistorySegment,
    HistorySegmentKind, ModelResponse, OperationId, ProviderOutcome, RuntimeCheckpoint,
    RuntimeComponentKind, SessionStoreError, StoreAdmission, TurnId, expected_provider_dispatch,
    finalized_operation_evidence, history_segment_kind, provider_terminal_state, runtime_component,
    runtime_manifest_error, transition_admission, working_lifecycle,
};

impl Driver {
    /// Transitions one operation under the class `transition_admission`
    /// derives from its current and target states.
    pub(in crate::runtime::driver) async fn transition_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        state: DurableOperationState,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let admission = transition_admission(&self.operation_state(operation_id)?, &state);
        self.transition_operation_as(admission, turn_id, operation_id, state)
            .await
    }

    pub(in crate::runtime::driver) async fn transition_operation_as(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        operation_id: OperationId,
        state: DurableOperationState,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let checkpoint_id = self.checkpoint_id()?;
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            // A queued root can have been admitted while its predecessor was
            // live. Its dispatch, not that earlier admission, starts accounting.
            let ledger = if matches!(state, DurableOperationState::Dispatched)
                && self.active_root.as_ref().is_some_and(|root| {
                    root.turn_id == turn_id && root.operation_id == operation_id
                })
                && manifest
                    .ledger()
                    .retired_counter()
                    .is_some_and(|(root, _)| root != turn_id)
            {
                manifest
                    .ledger()
                    .clone()
                    .with_retired_attempts(None)
                    .map_err(runtime_manifest_error)?
            } else {
                manifest.ledger().clone()
            };
            (
                manifest.canonical().history().clone(),
                ledger
                    .transition_operation(
                        operation_id.0,
                        DurableOperationTransition::new(checkpoint_id, state),
                    )
                    .map_err(runtime_manifest_error)?,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await
    }

    /// Transitions several operations atomically; the commit is ordinary
    /// only when every transition is a first fence out of `Prepared`.
    pub(in crate::runtime::driver) async fn transition_operations(
        &mut self,
        turn_id: TurnId,
        transitions: Vec<(OperationId, DurableOperationState)>,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        let mut admission = StoreAdmission::Ordinary;
        for (operation_id, state) in &transitions {
            if transition_admission(&self.operation_state(*operation_id)?, state)
                != StoreAdmission::Ordinary
            {
                admission = StoreAdmission::Settlement;
            }
        }
        self.transition_operations_as(admission, turn_id, transitions)
            .await
    }

    pub(in crate::runtime::driver) async fn transition_operations_as(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        transitions: Vec<(OperationId, DurableOperationState)>,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        if transitions.is_empty() {
            return Ok(self.authority.checkpoint());
        }
        let checkpoint_id = self.checkpoint_id()?;
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let mut ledger = manifest.ledger().clone();
            for (operation_id, state) in transitions {
                ledger = ledger
                    .transition_operation(
                        operation_id.0,
                        DurableOperationTransition::new(checkpoint_id, state),
                    )
                    .map_err(runtime_manifest_error)?;
            }
            (
                manifest.canonical().history().clone(),
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await
    }

    pub(in crate::runtime::driver) async fn finalize_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        match self.operation_state(operation_id)? {
            DurableOperationState::Completed(evidence)
            | DurableOperationState::ProvenNotDispatched(evidence) => {
                let checkpoint_id = self.checkpoint_id()?;
                let (history, ledger, context, family) = {
                    let authority = self.authority.checkpoint();
                    let manifest = authority.manifest();
                    let history = manifest.canonical().history().clone();
                    let evidence =
                        finalized_operation_evidence(&evidence, checkpoint_id, history.identity())?;
                    let ledger = manifest
                        .ledger()
                        .transition_operation(
                            operation_id.0,
                            DurableOperationTransition::new(
                                checkpoint_id,
                                DurableOperationState::Finalized(evidence),
                            ),
                        )
                        .map_err(runtime_manifest_error)?;
                    (
                        history,
                        ledger,
                        manifest.canonical().context().retained_for_exact_resume(),
                        manifest.family_artifact().cloned(),
                    )
                };
                let lifecycle = working_lifecycle(turn_id, &ledger)?;
                self.publish_turn_terminal(
                    checkpoint_id,
                    lifecycle,
                    history,
                    ledger,
                    context,
                    family,
                    &[],
                )
                .await
            }
            DurableOperationState::Claimed(_)
            | DurableOperationState::DecisionRequired(_)
            | DurableOperationState::ContinuationRequired(_)
            | DurableOperationState::LocatorBound(_)
            | DurableOperationState::OutcomeUnknown(_) => Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "operation outcome requires exact certified recovery",
            )),
            DurableOperationState::Prepared
            | DurableOperationState::Dispatched
            | DurableOperationState::Finalized(_) => Err(AgentError::new(
                AgentErrorKind::Internal,
                "operation cannot be finalized from its current state",
            )),
        }
    }

    pub(in crate::runtime::driver) async fn append_history<I>(
        &mut self,
        turn_id: TurnId,
        kind: Option<HistorySegmentKind>,
        items: I,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError>
    where
        I: IntoIterator<Item = ConversationItem>,
    {
        Box::pin(self.append_history_with_notices(turn_id, kind, items, false)).await
    }

    pub(in crate::runtime::driver) async fn append_history_with_notices<I>(
        &mut self,
        turn_id: TurnId,
        kind: Option<HistorySegmentKind>,
        items: I,
        deliver_notices: bool,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError>
    where
        I: IntoIterator<Item = ConversationItem>,
    {
        let items = items.into_iter().collect::<Vec<_>>();
        let notices = if deliver_notices {
            self.process_notices()?
        } else {
            None
        };
        if items.is_empty() && notices.is_none() {
            return Ok(self.authority.checkpoint());
        }
        let segment = if items.is_empty() {
            None
        } else {
            Some(
                HistorySegment::new(kind.unwrap_or_else(|| history_segment_kind(&items)), items)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "canonical history rejected an atomic semantic group",
                        )
                    })?,
            )
        };
        let checkpoint_id = self.checkpoint_id()?;
        let mut consumed = Vec::new();
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let mut history = manifest.canonical().history().clone();
            if let Some((notice, numbers)) = notices {
                history = history
                    .append(notice)
                    .map_err(|_| super::notices::notice_history_error())?;
                consumed = numbers;
            }
            if let Some(segment) = segment {
                history = history
                    .append(segment)
                    .map_err(|_| super::notices::notice_history_error())?;
            }
            (
                history,
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        let checkpoint = self
            .publish_checkpoint_consuming_jobs(
                StoreAdmission::Ordinary,
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                context,
                family,
                &consumed,
            )
            .await?;
        let point = if kind == Some(HistorySegmentKind::RecentIntent) {
            if self
                .active_root
                .as_ref()
                .is_some_and(|root| root.instruction)
            {
                crate::JobNoticeDelivery::IdleWake
            } else {
                crate::JobNoticeDelivery::Admission
            }
        } else {
            crate::JobNoticeDelivery::Boundary
        };
        self.observe_notice_delivery(turn_id, point, consumed.len());
        Ok(checkpoint)
    }

    pub(in crate::runtime::driver) fn operation_state(
        &self,
        operation_id: OperationId,
    ) -> Result<DurableOperationState, AgentError> {
        self.authority
            .checkpoint()
            .manifest()
            .ledger()
            .operation(operation_id.0)
            .map(|record| record.state().clone())
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "operation state referenced an unknown identity",
                )
            })
    }

    pub(in crate::runtime::driver) fn certify_mutating_effect_recovery(
        &self,
    ) -> Result<(), AgentError> {
        self.authority.require_durability()
    }

    /// The decision commit of a round's provider attempt: the operation's
    /// terminal state carrying its outcome record, plus the finalization of
    /// the continuation baton the round ran under.
    pub(in crate::runtime::driver) async fn finish_provider_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        attempt: Result<&ModelResponse, &AgentError>,
        outcome: &ProviderOutcome,
        continuation: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let mut plan = self.commit_plan()?;
        self.stage_provider_outcome(&mut plan, operation_id, attempt, outcome, continuation)?;
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }

    /// Adds a round's decision to `plan`: the provider operation's terminal
    /// state carrying `outcome`, and the finalization of the continuation
    /// baton the round ran under. The provider itself is finalized only when
    /// the caller also commits the decision's consequence in `plan`.
    pub(in crate::runtime::driver) fn stage_provider_outcome(
        &self,
        plan: &mut CommitPlan,
        operation_id: OperationId,
        attempt: Result<&ModelResponse, &AgentError>,
        outcome: &ProviderOutcome,
        continuation: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let authority = self.authority.checkpoint();
        let record = authority
            .manifest()
            .ledger()
            .operation(operation_id.0)
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "operation finalization referenced an unknown identity",
                )
            })?;
        let terminal = provider_terminal_state(
            attempt,
            outcome,
            matches!(
                record.state(),
                DurableOperationState::Dispatched | DurableOperationState::LocatorBound(_)
            ),
            expected_provider_dispatch(record),
        )?;
        plan.transitions.push((operation_id, terminal));
        if let Some(continuation) = continuation {
            plan.finalize.insert(continuation);
        }
        Ok(())
    }

    pub(in crate::runtime::driver) async fn finalize_hosted_continuation(
        &mut self,
        turn_id: TurnId,
        continuation: Option<OperationId>,
    ) -> Result<(), AgentError> {
        let Some(continuation) = continuation else {
            return Ok(());
        };
        let checkpoint_id = self.checkpoint_id()?;
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let evidence = manifest
                .ledger()
                .operation(continuation.0)
                .and_then(|record| match record.state() {
                    DurableOperationState::ContinuationRequired(evidence) => Some(evidence),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            let finalized = finalized_operation_evidence(
                evidence,
                checkpoint_id,
                manifest.canonical().history().identity(),
            )?;
            (
                manifest.canonical().history().clone(),
                manifest
                    .ledger()
                    .transition_operation(
                        continuation.0,
                        DurableOperationTransition::new(
                            checkpoint_id,
                            DurableOperationState::Finalized(finalized),
                        ),
                    )
                    .map_err(runtime_manifest_error)?,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle(turn_id, &ledger)?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(())
    }

    /// A definite refusal ends the turn, so an open server-pause or
    /// provider-programmatic continuation is finalized before it fails and
    /// the head settles Quiescent with one terminal. Cancellation,
    /// outcome-unknown, storage and binding failures keep the baton for their
    /// own recovery.
    pub(in crate::runtime::driver) async fn finalize_continuation_on_refusal<T>(
        &mut self,
        turn_id: TurnId,
        continuation: Option<OperationId>,
        result: Result<T, AgentError>,
    ) -> Result<T, AgentError> {
        if let Err(refusal) = &result
            && definite_refusal(refusal)
        {
            self.finalize_hosted_continuation(turn_id, continuation)
                .await?;
        }
        result
    }

    pub(in crate::runtime::driver) async fn finish_effect_decision_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        binding: Digest,
        resolution: EffectDecisionResolution,
        decision_recorded: bool,
    ) -> Result<(), AgentError> {
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&("plakous-agent/decision-outcome-v2", binding, resolution))
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "decision outcome evidence could not be canonicalized",
                    )
                })?,
        )?;
        let mut plan = self.commit_plan()?;
        plan.transitions.push((
            operation_id,
            if decision_recorded {
                DurableOperationState::Completed(evidence)
            } else {
                DurableOperationState::ProvenNotDispatched(evidence)
            },
        ));
        plan.finalize.insert(operation_id);
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }

    pub(in crate::runtime::driver) fn observe_notice_delivery(
        &self,
        turn_id: TurnId,
        point: crate::JobNoticeDelivery,
        count: usize,
    ) {
        if count != 0 {
            self.emit_telemetry(
                self.telemetry_context.with_turn(turn_id),
                crate::TelemetryObservationKind::JobNoticesDelivered {
                    point,
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                },
            );
        }
    }
}

/// A refusal decided before dispatch, with no ambiguous effect, storage
/// state or unavailable binding left for recovery to reconcile; a restored
/// frame retries an unavailable binding instead.
fn definite_refusal(error: &AgentError) -> bool {
    error.store_error().is_none()
        && matches!(
            error.kind(),
            AgentErrorKind::RequestBudget
                | AgentErrorKind::Model
                | AgentErrorKind::RoundLimit
                | AgentErrorKind::InvalidInput
        )
}
