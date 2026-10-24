use super::{
    AgentError, AgentErrorKind, Arc, BranchHead, BranchLease, CheckpointCommitFacts, CheckpointId,
    CheckpointObjectBundle, CheckpointObjectRef, CommitCheckpoint, DurabilityCommitTelemetry,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, Duration, RecoveryObligation, RootClosure, RootClosureState,
    RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointCodec, RuntimeComponentKind,
    RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger, RuntimeTopology, SessionStore,
    SessionStoreError, StoreAdmission, WorkingProjection, abandonment_closure_state,
    checkpoint_bundle_error, checkpoint_commit_identity, commit_with_acknowledgement_recovery,
    runtime_component, runtime_manifest_error, terminal_closure_state,
    terminal_record_at_checkpoint, working_lifecycle,
};

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the recovery publisher consumes the exact immutable source/store tuple"
)]
pub(in crate::runtime) async fn recover_root_turn_publication(
    store: &Arc<dyn SessionStore>,
    lease: &BranchLease,
    head: BranchHead,
    source: &Arc<RuntimeCheckpoint>,
    source_bundle: &CheckpointObjectBundle,
    closure: RootClosure,
    permitted: &[RecoveryObligation],
    timeout: Duration,
    checkpoint_id: CheckpointId,
    durability_telemetry: Option<&DurabilityCommitTelemetry>,
) -> Result<(Arc<RuntimeCheckpoint>, CheckpointObjectBundle, BranchHead), AgentError> {
    let source_manifest = source.manifest();
    if permitted != source_manifest.lifecycle().obligations() {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "recovery permit disagrees with the exact checkpoint obligations",
        ));
    }
    let report = crate::runtime::checkpoint::owner::root_closure_report(source, &closure)?;
    let RootClosureState {
        history,
        mut ledger,
        turn_id,
        command_id,
        family_artifact,
    } = match closure {
        RootClosure::Abandon { fold, claim } => {
            abandonment_closure_state(source, fold, claim, checkpoint_id)?
        }
        RootClosure::FinalizeRecorded {
            terminal,
            code_cell,
        } => terminal_closure_state(source, terminal, code_cell.is_some(), checkpoint_id)?,
        RootClosure::Complete(terminal) => {
            terminal_closure_state(source, terminal, false, checkpoint_id)?
        }
    };
    if let Some(report) = report {
        let commands = ledger.commands().into_iter().cloned().collect();
        let mut records = Vec::new();
        for record in ledger.operations() {
            let mut transitions = record.transitions().to_vec();
            if matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
                && let Some(last) = transitions.last()
                && let DurableOperationState::Finalized(evidence) = last.state()
                && let Ok(mut terminal) =
                    crate::runtime::terminal::read_terminal_result_record(evidence.bytes())
                && terminal.command_id == command_id.as_str()
                && terminal.turn_id == turn_id
            {
                terminal.abandonment = Some(report.clone());
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    crate::runtime::terminal::encode_terminal_result_record(&terminal)
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
                )?;
                let transition = DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(evidence),
                );
                if last.checkpoint_id() == checkpoint_id {
                    let last = transitions
                        .last_mut()
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                    *last = transition;
                } else {
                    transitions.push(transition);
                }
            }
            records.push(
                DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )
                .map_err(runtime_manifest_error)?,
            );
        }
        ledger = RuntimeOperationLedger::new(commands, records).map_err(runtime_manifest_error)?;
    }
    if !matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Quiescent
    ) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let branch_revision = source_manifest
        .topology()
        .branch_revision
        .checked_add(1)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    if branch_revision != head.revision().get().saturating_add(1) {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let context = source_manifest
        .canonical()
        .context()
        .retained_for_exact_resume();
    let lifecycle = RuntimeLifecycle::Quiescent;
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: source.id(),
        branch_revision,
        lifecycle: &lifecycle,
        history_identity: history.identity(),
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: family_artifact.as_ref(),
    })?;
    let canonical = RuntimeCanonicalState::new(
        source_manifest.canonical().stable_prefix().clone(),
        history,
        WorkingProjection::default(),
        context,
    )
    .map_err(runtime_manifest_error)?;
    let checkpoint = Arc::new(
        RuntimeCheckpoint::new(
            RuntimeManifest::new(
                source_manifest.provenance().clone(),
                RuntimeTopology {
                    session_id: source_manifest.topology().session_id,
                    branch_id: source_manifest.topology().branch_id,
                    checkpoint_id,
                    parent_checkpoint_id: Some(source.id()),
                    branch_revision,
                    fork: source_manifest.topology().fork,
                    lease_fence: lease.fence().get(),
                    commit_identity,
                },
                lifecycle,
                source_manifest.harness_plan_component().clone(),
                source_manifest.provider_configuration().clone(),
                canonical,
                source_manifest.binding().clone(),
                source_manifest.tools().clone(),
                ledger,
                family_artifact,
                source_manifest.interpreter_observation().cloned(),
                source_manifest.jobs().clone(),
                source_manifest.durability(),
            )
            .map_err(runtime_manifest_error)?,
        )
        .map_err(runtime_manifest_error)?,
    );
    terminal_record_at_checkpoint(&checkpoint, &command_id)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, source_bundle)
        .map_err(checkpoint_bundle_error)?;
    let request = CommitCheckpoint::new(lease.clone(), head, bundle.clone())
        .with_admission(StoreAdmission::Recovery)
        .with_feed_facts(crate::feed::facts::commit_facts(source, &checkpoint)?);
    let expected =
        CheckpointObjectRef::from_store(checkpoint_id, bundle.root(), checkpoint.integrity());
    let receipt = Box::pin(commit_with_acknowledgement_recovery(
        store,
        timeout,
        checkpoint_id,
        request,
        head,
        expected,
        checkpoint.manifest().topology().commit_identity,
        durability_telemetry,
    ))
    .await?;
    Ok((checkpoint, bundle, receipt.head()))
}
