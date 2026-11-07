use super::{
    AgentError, AgentErrorKind, Arc, BranchHead, BranchLease, CheckpointId, CheckpointObjectBundle,
    DurabilityCommitTelemetry, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, Duration, RecoveryObligation, RootClosure, RootClosureState,
    RuntimeCheckpoint, RuntimeComponentKind, RuntimeLifecycle, RuntimeOperationLedger,
    SessionStore, SessionStoreError, abandonment_closure_state, runtime_component,
    runtime_manifest_error, terminal_closure_state, terminal_record_for_command, working_lifecycle,
};
use crate::runtime::checkpoint::candidate::{SuccessorFacts, successor_candidate};
use crate::runtime::durable::RecoveryPublicationOwner;

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the recovery publisher consumes the exact immutable source/store tuple"
)]
pub(in crate::runtime) async fn recover_root_turn_publication(
    store: &Arc<dyn SessionStore>,
    lease: &mut BranchLease,
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
        let counter = ledger.retired_counter();
        ledger = RuntimeOperationLedger::new(commands, records)
            .and_then(|ledger| ledger.with_retired_attempts(counter))
            .map_err(runtime_manifest_error)?;
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
    let command = ledger
        .commands()
        .into_iter()
        .find(|command| command.command_id() == command_id.as_str())
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "terminal checkpoint omitted its admitted root command",
            )
        })?;
    terminal_record_for_command(&ledger, command)?;
    let candidate = successor_candidate(
        source,
        source_bundle,
        SuccessorFacts {
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            jobs: source_manifest.jobs().clone(),
            named_state: source_manifest.named_state().clone(),
            context,
            family: family_artifact,
        },
        lease.fence().get(),
        source_manifest.interpreter_observation().cloned(),
    )?;
    let mut owner = RecoveryPublicationOwner {
        store: Arc::clone(store),
        store_timeout: timeout,
        lease,
        head,
        checkpoint: Arc::clone(source),
        bundle: source_bundle.clone(),
    };
    owner
        .publish_candidate(candidate, durability_telemetry)
        .await?;
    Ok((owner.checkpoint, owner.bundle, owner.head))
}
