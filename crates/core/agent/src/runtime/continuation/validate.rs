use super::{
    Actor, AgentError, AgentErrorKind, AttemptId, AwaitToolBatchV2, BTreeMap, BTreeSet,
    CompiledProviderConfiguration, ContinuationBindingReference, ContinuationRoot,
    ContinuationToolSlot, Digest, DurableCommandDisposition, DurableOperationKind,
    DurableOperationState, EntryId, FamilyArtifact, ModelOperationRoot, ModelRequest,
    ModelResponse, OperationId, PreparedCallRecord, Provenance, ProviderOutcome,
    ReconstructiveComponent, RestoredObservationClip, RoundDecision, RuntimeCheckpoint,
    RuntimeLifecycle, RuntimeManifest, RuntimeModelAuthority, RuntimeOperationLedger,
    RuntimeWorkingRoot, SessionStoreError, ToolCallBlock, ToolExecutionResult,
    ValidatedRequestPlan, ValidationMode, WorkingProjection, canonicalize_observed_tool_execution,
    collect_local_calls, recorded_provider_response, runtime_manifest_error,
};

/// Rebuilds the plan a continuation frame's provider response was validated
/// under, from the prepared call that dispatched it.
///
/// A plan digest binds the freshness-sensitive descriptor binding, and a
/// re-resolution or a fresh reopen observes the descriptor afresh: neither the
/// checkpoint's configuration (kept from creation) nor a running driver's can
/// recompute it. The frame is bound instead to the provider operation's
/// recorded prepared call: its plan digest and request must be the recorded
/// ones, and the record's authority evidence must carry the plan's descriptor
/// binding. `configuration` (the checkpoint's, or the driver's) is checked only
/// through what does not age: the selected and effective identities, which
/// carry the semantic descriptor binding, the account and credential
/// authority, and the descriptor's semantic axes and capability decisions.
pub(in crate::runtime) fn restore_frame_plan(
    configuration: &CompiledProviderConfiguration,
    manifest: &RuntimeManifest,
    provider_operation: OperationId,
    provider_request: &ModelRequest,
    provider_plan_digest: Digest,
) -> Result<ValidatedRequestPlan, AgentError> {
    let corrupt = || AgentError::from_store(SessionStoreError::corrupt());
    let record = manifest
        .ledger()
        .operation(provider_operation.0)
        .ok_or_else(corrupt)?;
    if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
        return Err(corrupt());
    }
    let prepared: PreparedCallRecord =
        serde_json::from_slice(record.request().bytes()).map_err(|_| corrupt())?;
    let projection = prepared.projection();
    let evidence = prepared.authority_evidence();
    let stable = configuration.effective().authority();
    if projection.plan() != provider_plan_digest
        || projection.selected_provider_configuration_id() != configuration.selected_identity()
        || projection.effective_provider_configuration_id() != configuration.effective_identity()
        || evidence.account_binding() != stable.account_binding()
        || evidence.credential_authority_binding() != stable.credential_authority_binding()
    {
        return Err(corrupt());
    }
    let plan = prepared
        .restore_plan(
            configuration.effective().descriptor(),
            provider_request.clone(),
            ValidationMode::Strict,
        )
        .map_err(|_| corrupt())?;
    if plan.digest() != provider_plan_digest || plan.request() != provider_request {
        return Err(corrupt());
    }
    Ok(plan)
}

#[allow(
    clippy::too_many_lines,
    reason = "Hosted frame validation cross-checks the root, prepared plan, response, budgets, and binding against durable authority"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "the hosted frame binding is one exact reconstruction tuple"
)]
pub(super) fn validate_hosted_frame(
    checkpoint: &RuntimeCheckpoint,
    root: &ContinuationRoot,
    provider_request: &ModelRequest,
    provider_plan_digest: Digest,
    provider_response: &ModelResponse,
    attempt_id: AttemptId,
    provider_operation: OperationId,
    decision: &RoundDecision,
    working: &WorkingProjection,
    model_admission_reference: &ContinuationBindingReference,
    policy_reference: &ContinuationBindingReference,
) -> Result<bool, AgentError> {
    let manifest = checkpoint.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        ..
    } = manifest.lifecycle()
    else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    if root.turn_id != *turn_id
        || provider_response.plan_digest() != provider_plan_digest
        || working != &WorkingProjection::default()
        || *model_admission_reference
            != ContinuationBindingReference::from(manifest.binding().admission().reference())
        || *policy_reference != ContinuationBindingReference::from(manifest.tools().policy())
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let command = manifest
        .ledger()
        .commands()
        .into_iter()
        .find(|record| record.command_id() == root.command_id)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let root_operation = manifest
        .ledger()
        .operation(root.operation_id.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if command.turn_id() != Some(*turn_id)
        || command.binding() != root.binding
        || command.disposition() != DurableCommandDisposition::Accepted
        || !matches!(root_operation.kind(), DurableOperationKind::Command { turn_id: Some(id) } if id == turn_id)
        || !matches!(root_operation.state(), DurableOperationState::Dispatched)
        || root_operation.request().bytes() != command.command_component().bytes()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let plan = restore_frame_plan(
        manifest.provider_configuration(),
        manifest,
        provider_operation,
        provider_request,
        provider_plan_digest,
    )?;
    if provider_response.validate_for_commit().is_err() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let provider = manifest
        .ledger()
        .operation(provider_operation.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if !matches!(provider.kind(), DurableOperationKind::Provider { root: ModelOperationRoot::Turn(id), attempt_id: candidate } if id == turn_id && *candidate == attempt_id)
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    // A record that does not decode is typed integrity, never "no frame".
    let records_frame = |evidence: &ReconstructiveComponent| -> Result<bool, AgentError> {
        Ok(ProviderOutcome::decode(evidence)?.is_some_and(|outcome| {
            outcome.response() == Some(provider_response) && outcome.decision == *decision
        }))
    };
    let mut completed_frame = false;
    for transition in provider.transitions() {
        if let DurableOperationState::Completed(evidence) = transition.state() {
            completed_frame |= records_frame(evidence)?;
        }
    }
    let promoted = match provider.state() {
        DurableOperationState::Completed(evidence) if records_frame(evidence)? => false,
        DurableOperationState::Finalized(_) if completed_frame => true,
        _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
    };
    if promoted {
        let projection = manifest.canonical().projection();
        let committed = projection.items();
        let start = plan.request().input().items().len();
        let end = start
            .checked_add(provider_response.output().len())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        if end > committed.len()
            || committed
                .iter()
                .skip(start)
                .take(provider_response.output().len())
                .ne(provider_response.output())
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        match provider_response.family_artifact_candidate() {
            Some(candidate) => {
                let covered = projection
                    .canonical_item_prefix_identity(end)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                candidate
                    .validate_for_commit(&plan, covered, provider_response.output().len())
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                let expected =
                    FamilyArtifact::from_candidate(candidate).map_err(runtime_manifest_error)?;
                if manifest.family_artifact() != Some(&expected) {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            None if manifest.family_artifact().is_none() => {}
            None => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        }
    }
    Ok(promoted)
}

#[allow(
    clippy::too_many_lines,
    reason = "Slot reconciliation maps every restored call to its recorded outcome or pending decision while preserving source order"
)]
pub(super) fn reconcile_hosted_tool_slots(
    checkpoint: &RuntimeCheckpoint,
    mut frame: AwaitToolBatchV2,
    promoted: bool,
) -> Result<
    (
        AwaitToolBatchV2,
        crate::runtime::observation::budget::ObservationBudget,
        usize,
        Vec<RestoredObservationClip>,
    ),
    AgentError,
> {
    let manifest = checkpoint.manifest();
    let canonical = manifest.canonical().projection();
    let prior = if promoted {
        frame.provider_request.input()
    } else {
        &canonical
    };
    let calls = collect_local_calls(&frame.provider_response, prior)
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if calls.len() != frame.slots.len()
        || calls.iter().zip(&frame.slots).any(|(call, slot)| {
            let stored = match slot {
                ContinuationToolSlot::Completed { call, .. }
                | ContinuationToolSlot::PendingDecision { call, .. }
                | ContinuationToolSlot::NotAdmitted { call, .. } => call,
            };
            call != stored
        })
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    if !calls.iter().any(|call| {
        call.correlation()
            .caller_lineage()
            .provider_programmatic_kind()
            .is_some()
    }) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let mut effects = BTreeMap::new();
    let mut resolved_decisions = BTreeSet::new();
    for record in manifest.ledger().operations() {
        match record.kind() {
            DurableOperationKind::Effect {
                turn_id,
                attempt_id,
                call_id,
                source_index,
            } if *turn_id == frame.root.turn_id && *attempt_id == frame.attempt_id => {
                let index = usize::try_from(*source_index)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                if calls.get(index).map(ToolCallBlock::call_id) != Some(*call_id)
                    || effects.insert(index, record).is_some()
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            DurableOperationKind::EffectDecision {
                turn_id,
                attempt_id,
                call_id,
            } if *turn_id == frame.root.turn_id && *attempt_id == frame.attempt_id => {
                let index = calls
                    .iter()
                    .position(|call| call.call_id() == *call_id)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                if !matches!(record.state(), DurableOperationState::DecisionRequired(_))
                    && !resolved_decisions.insert(index)
                {
                    return Err(AgentError::from_store(SessionStoreError::corrupt()));
                }
            }
            _ => {}
        }
    }
    let mut remaining_budget = frame.observation_budget;
    let mut clips = Vec::new();
    let mut dispatched = 0usize;
    let mut saw_gap = false;
    for (index, slot) in frame.slots.iter_mut().enumerate() {
        let (call, result_entry_id) = match slot {
            ContinuationToolSlot::NotAdmitted {
                call,
                result_entry_id,
            } => (call.clone(), *result_entry_id),
            _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        };
        let effect = effects.remove(&index);
        let Some(effect) = effect else {
            if resolved_decisions.contains(&index) {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "resolved hosted effect decision lacks an exact effect operation",
                ));
            }
            saw_gap = true;
            continue;
        };
        if saw_gap {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let (evidence, was_dispatched) = match effect.state() {
            DurableOperationState::Completed(evidence) => (evidence, true),
            DurableOperationState::ProvenNotDispatched(evidence) => (evidence, false),
            DurableOperationState::Finalized(_) => effect
                .transitions()
                .iter()
                .rev()
                .find_map(|transition| match transition.state() {
                    DurableOperationState::Completed(evidence) => Some((evidence, true)),
                    DurableOperationState::ProvenNotDispatched(evidence) => Some((evidence, false)),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
            _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
        };
        dispatched = dispatched.saturating_add(usize::from(was_dispatched));
        let (tag, evidence_entry, execution): (String, EntryId, ToolExecutionResult) =
            serde_json::from_slice(evidence.bytes())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        if tag != "completed-tool-outcome-v1" || evidence_entry != result_entry_id {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let (item, clip) =
            canonicalize_observed_tool_execution(result_entry_id, execution, &mut remaining_budget)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        if let Some(clip) = clip {
            clips.push(RestoredObservationClip {
                name: call.exposed_name().clone(),
                call_id: call.call_id(),
                clip,
            });
        }
        *slot = ContinuationToolSlot::Completed {
            call,
            result_entry_id,
            item,
            operation_id: OperationId(effect.operation_id()),
        };
    }
    if !effects.is_empty() {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    Ok((frame, remaining_budget, dispatched, clips))
}

#[cfg(test)]
pub(in crate::runtime) fn previous_response_metadata(
    ledger: &RuntimeOperationLedger,
    plan: &ValidatedRequestPlan,
    authority: &RuntimeModelAuthority,
) -> Option<plakous_model::ModelResponseMetadata> {
    previous_response(ledger, plan, authority)
        .ok()
        .flatten()
        .map(|(metadata, _)| metadata)
}

/// The runtime-proven provider response whose output is the newest assistant
/// item of `plan`: its metadata and its provider response id.
///
/// # Errors
///
/// A provider record whose outcome does not decode is typed integrity.
pub(in crate::runtime) fn previous_response(
    ledger: &RuntimeOperationLedger,
    plan: &ValidatedRequestPlan,
    authority: &RuntimeModelAuthority,
) -> Result<Option<(plakous_model::ModelResponseMetadata, Option<String>)>, AgentError> {
    for record in ledger.operations() {
        if matches!(
            record.state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ) {
            recorded_provider_response(record)?;
        }
    }
    Ok(proven_previous_response(ledger, plan, authority))
}

/// [`previous_response`] over a ledger whose provider records all decode.
fn proven_previous_response(
    ledger: &RuntimeOperationLedger,
    plan: &ValidatedRequestPlan,
    authority: &RuntimeModelAuthority,
) -> Option<(plakous_model::ModelResponseMetadata, Option<String>)> {
    if plan.binding_digest() != authority.descriptor_binding() {
        return None;
    }
    let latest = plan
        .request()
        .input()
        .items()
        .into_iter()
        .filter(|item| item.actor() == Actor::Assistant)
        .last()?;
    if latest.provenance() != Provenance::ModelOutput {
        return None;
    }
    let mut matched = None;
    for record in ledger.operations() {
        if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
            continue;
        }
        if !matches!(
            record.state(),
            DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
        ) {
            continue;
        }
        let Some(response) = recorded_provider_response(record).ok().flatten() else {
            continue;
        };
        if !response
            .output()
            .iter()
            .any(|item| item.id() == latest.id())
        {
            continue;
        }
        // Two operations claiming the same canonical identity are not provenance.
        if matched.is_some() {
            return None;
        }
        let prepared: PreparedCallRecord = serde_json::from_slice(record.request().bytes()).ok()?;
        let source = prepared.authority_evidence();
        // The sealed effective configuration binds the stable descriptor semantics.
        // Comparing historical freshness timestamps would lose attribution on reopen;
        // the current plan still requires its exact fresh descriptor above.
        if response.plan_digest() != prepared.projection().plan()
            || source.account_binding() != authority.account_binding()
            || source.credential_authority_binding() != authority.credential_authority_binding()
            || prepared.projection().selected_provider_configuration_id()
                != plan.request().selected_provider_configuration_id()
            || prepared.projection().effective_provider_configuration_id()
                != plan.request().effective_provider_configuration_id()
        {
            return None;
        }
        matched = Some((
            response.metadata().clone(),
            response.response_id().map(str::to_owned),
        ));
    }
    matched
}
