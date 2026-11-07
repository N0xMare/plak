use super::{
    ActorCommandEffect, AgentError, AgentErrorKind, AuthorizedToolInvocation,
    ClaimedToolInvocation, CommandId, ControlError, Digest, Driver, DurableCommandDisposition,
    DurableCommandEnvelope, DurableCommandRecord, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, EffectDispatchIntent, HistorySegment,
    OperationId, Ordering, RuntimeComponentKind, RuntimeLifecycle, RuntimeWorkingRoot,
    StoreAdmission, TurnId, durable_command_effect, durable_control_error_kind, durable_rejection,
    effect_intent_bytes, effect_operation_intent_digest, ledger_needs_retention,
    operation_kind_turn_id, runtime_component, runtime_manifest_error, working_lifecycle,
};

impl Driver {
    /// Prepares one operation. The caller classifies the commit: preparing
    /// new work is ordinary, recording a started or settled fact is settlement.
    pub(in crate::runtime::driver) async fn prepare_operation(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        kind: DurableOperationKind,
        binding: Digest,
        recoverable_input: Option<Vec<u8>>,
        segment: Option<HistorySegment>,
    ) -> Result<OperationId, AgentError> {
        let operation_id = self.take_operation_id()?;
        self.prepare_operation_with_id(
            admission,
            turn_id,
            operation_id,
            kind,
            binding,
            recoverable_input,
            segment,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the admission class travels with the exact prepared record"
    )]
    pub(in crate::runtime::driver) async fn prepare_operation_with_id(
        &mut self,
        admission: StoreAdmission,
        turn_id: TurnId,
        operation_id: OperationId,
        kind: DurableOperationKind,
        binding: Digest,
        recoverable_input: Option<Vec<u8>>,
        segment: Option<HistorySegment>,
    ) -> Result<OperationId, AgentError> {
        let checkpoint_id = self.checkpoint_id()?;
        let (mut history, mut ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        if let Some(segment) = segment {
            history = history.append(segment).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "canonical history rejected an atomic semantic group",
                )
            })?;
        }
        let request = runtime_component(
            RuntimeComponentKind::Operation,
            recoverable_input.unwrap_or_else(|| binding.bytes().to_vec()),
        )?;
        let record = DurableOperationRecord::new(
            operation_id.0,
            kind,
            request,
            vec![DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Prepared,
            )],
        )
        .map_err(runtime_manifest_error)?;
        ledger = ledger
            .append_operation(record)
            .map_err(runtime_manifest_error)?;
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
        .await?;
        Ok(operation_id)
    }

    pub(in crate::runtime::driver) async fn claim_mutating_effect(
        &mut self,
        turn_id: TurnId,
        effect_operation: OperationId,
        intent: EffectDispatchIntent,
        authorized: AuthorizedToolInvocation,
    ) -> Result<ClaimedToolInvocation, AgentError> {
        self.certify_mutating_effect_recovery()?;
        let claim_evidence_digest = self
            .persist_effect_claim(turn_id, effect_operation, &intent)
            .await?;
        let tools = self.tools.clone();
        tools
            .claim_effect(authorized, intent, claim_evidence_digest)
            .map_err(|error| {
                self.accepting.store(false, Ordering::Release);
                AgentError::from_tool(&error)
            })
    }

    pub(super) async fn persist_effect_claim(
        &mut self,
        turn_id: TurnId,
        effect_operation: OperationId,
        intent: &EffectDispatchIntent,
    ) -> Result<Digest, AgentError> {
        if intent.digest()
            != effect_operation_intent_digest(&self.authority.checkpoint(), effect_operation)?
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect claim disagrees with the prepared durable intent",
            ));
        }
        let kind = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let effect = manifest
                .ledger()
                .operation(effect_operation.0)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "effect claim referenced an unknown prepared operation",
                    )
                })?;
            if !matches!(effect.kind(), DurableOperationKind::Effect { .. })
                || !matches!(effect.state(), DurableOperationState::Prepared)
            {
                return Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "effect claim is not bound to a prepared effect",
                ));
            }
            effect.kind().clone()
        };
        if operation_kind_turn_id(&kind) != Some(turn_id) {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect claim targets another turn",
            ));
        }
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            effect_intent_bytes(intent)?,
        )?;
        let claim_evidence_digest = evidence.integrity();
        self.transition_operation(
            turn_id,
            effect_operation,
            DurableOperationState::Claimed(evidence),
        )
        .await?;
        Ok(claim_evidence_digest)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Command classification folds admission and control effects into the ledger before acknowledging their disposition"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "the durable command receipt carries the complete closed classification tuple"
    )]
    pub(in crate::runtime::driver) async fn persist_command_classification(
        &mut self,
        command_id: &CommandId,
        sequence: u64,
        turn_id: Option<TurnId>,
        binding: Digest,
        canonical_input: Vec<u8>,
        effect: ActorCommandEffect,
        rejection: Option<&ControlError>,
    ) -> Result<Option<OperationId>, AgentError> {
        let checkpoint_id = self.checkpoint_id()?;
        let durable_effect = durable_command_effect(effect)?;
        let envelope = DurableCommandEnvelope {
            version: 1,
            canonical_input,
            effect: durable_effect,
            rejection: rejection.map(durable_rejection),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "command receipt could not be canonically encoded",
            )
        })?;
        let component = runtime_component(RuntimeComponentKind::Command, bytes.clone())?;
        let operation = runtime_component(RuntimeComponentKind::Operation, bytes)?;
        let accepted = rejection.is_none();
        // Root and steering receipts begin work; cancel, decision and shutdown
        // receipts settle work already admitted and must fit in the reserve.
        let admission = match effect {
            ActorCommandEffect::Root
            | ActorCommandEffect::Steer(_)
            | ActorCommandEffect::Rejected => StoreAdmission::Ordinary,
            ActorCommandEffect::Cancel(_)
            | ActorCommandEffect::EffectDecision(_)
            | ActorCommandEffect::Shutdown => StoreAdmission::Settlement,
        };
        let operation_id = accepted.then(|| self.take_operation_id()).transpose()?;
        let (history, ledger, context, family, prior_lifecycle) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let operation_turn_id = if matches!(effect, ActorCommandEffect::Root) {
                turn_id
            } else {
                match manifest.lifecycle() {
                    RuntimeLifecycle::Working {
                        root: RuntimeWorkingRoot::Turn { turn_id, .. },
                        ..
                    } => Some(*turn_id),
                    RuntimeLifecycle::Working {
                        root: RuntimeWorkingRoot::SessionWorkflow(_),
                        ..
                    } => {
                        return Err(AgentError::new(
                            AgentErrorKind::Internal,
                            "turn command cannot join an active session workflow",
                        ));
                    }
                    RuntimeLifecycle::Quiescent => turn_id,
                }
            };
            let record = DurableCommandRecord::new(
                command_id.as_str(),
                sequence,
                turn_id,
                binding,
                component,
                if accepted {
                    DurableCommandDisposition::Accepted
                } else {
                    DurableCommandDisposition::Rejected
                },
                rejection.map(|error| durable_control_error_kind(error.kind()).to_owned()),
            )
            .map_err(runtime_manifest_error)?;
            let mut ledger = manifest
                .ledger()
                .append_command(record)
                .map_err(runtime_manifest_error)?;
            let pending_operation = operation_id
                .map(|operation_id| {
                    DurableOperationRecord::new(
                        operation_id.0,
                        DurableOperationKind::Command {
                            turn_id: operation_turn_id,
                        },
                        operation,
                        vec![DurableOperationTransition::new(
                            checkpoint_id,
                            DurableOperationState::Prepared,
                        )],
                    )
                    .map_err(runtime_manifest_error)
                })
                .transpose()?;
            // The new receipt makes the previous root eligible for retention.
            // Prune before appending its operation: a recovered, finalized
            // latest turn can occupy every operation slot (spec 01 §3).
            if matches!(effect, ActorCommandEffect::Root)
                && manifest.lifecycle() == &RuntimeLifecycle::Quiescent
                && ledger_needs_retention(&ledger)
            {
                let owned_turns = self
                    .active_root
                    .iter()
                    .chain(self.roots.iter())
                    .map(|root| root.turn_id)
                    .collect();
                let anchor = pending_operation.ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "root admission retention requires its pending operation",
                    )
                })?;
                ledger = crate::runtime::operation_record::retain_quiescent_ledger_with_anchor(
                    &ledger,
                    &owned_turns,
                    crate::manifest::RetentionAnchor::NewRecord(anchor),
                )?;
            } else if let Some(record) = pending_operation {
                ledger = ledger
                    .append_operation(record)
                    .map_err(runtime_manifest_error)?;
            }
            (
                manifest.canonical().history().clone(),
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
                manifest.lifecycle().clone(),
            )
        };
        let lifecycle = if accepted {
            let lifecycle_turn = match prior_lifecycle {
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::Turn { turn_id, .. },
                    ..
                } => turn_id,
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::SessionWorkflow(_),
                    ..
                } => {
                    return Err(AgentError::new(
                        AgentErrorKind::Internal,
                        "turn command cannot join an active session workflow",
                    ));
                }
                RuntimeLifecycle::Quiescent => turn_id.unwrap_or(
                    TurnId::try_from(self.run_id.as_uuid())
                        .map_err(|error| AgentError::from_identity(&error))?,
                ),
            };
            working_lifecycle(lifecycle_turn, &ledger)?
        } else {
            prior_lifecycle
        };
        let published = self
            .publish_checkpoint(
                admission,
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                context,
                family,
            )
            .await?;
        // The caller inserts this new receipt into the actor index next.
        // Mirror admission-time retention only after its CAS has succeeded.
        if published.manifest().ledger().command_count()
            < self.command_ledger.len().saturating_add(1)
        {
            self.retain_command_index(&published);
        }
        Ok(operation_id)
    }

    pub(in crate::runtime::driver) async fn complete_command_operation(
        &mut self,
        turn_id: TurnId,
        operation_id: OperationId,
        effect: ActorCommandEffect,
    ) -> Result<(), AgentError> {
        let mut plan = self.commit_plan()?;
        if matches!(
            self.operation_state(operation_id)?,
            DurableOperationState::Prepared
        ) {
            plan.transitions
                .push((operation_id, DurableOperationState::Dispatched));
        }
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&durable_command_effect(effect)?).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "command effect evidence could not be encoded",
                )
            })?,
        )?;
        plan.transitions
            .push((operation_id, DurableOperationState::Completed(evidence)));
        plan.finalize.insert(operation_id);
        self.publish_commit_plan(turn_id, plan).await?;
        Ok(())
    }
}
