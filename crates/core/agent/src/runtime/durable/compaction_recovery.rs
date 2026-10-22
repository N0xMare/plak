//! Recover or abandon one paid compaction without redispatch.

use crate::manifest::RuntimeContextState;
use crate::runtime::checkpoint::candidate::{
    CheckpointCandidate, SuccessorFacts, successor_candidate,
};
use crate::runtime::checkpoint::io::{
    StoreCallError, StorePhase, commit_with_acknowledgement_recovery, store_call,
};
use crate::runtime::compaction::continuation::with_post_compaction_note;
use crate::runtime::compaction::fold::{build_compaction_success_fold, reconstruct_compaction_cut};
use crate::runtime::compaction::record::{
    CompactionReceiptKind, compaction_operation_component, validate_compaction_receipt,
};
use crate::runtime::compaction::request::exact_compaction_runtime;
use crate::runtime::compaction::{
    CompactionCompletionError, DurableCompactionIdentity, DurableCompactionRequestRecord,
    DurablePlaceholderRequestRecord,
};
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::driver::model_cleanup::ambiguous_compaction_error;
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::operation_run_id;
use crate::runtime::durable::turn_recovery::verdict_refusal;
use crate::runtime::durable::{
    CompactionRecoveryOwner, DecodedCompactionWorkflow, DurableCompactionOperations,
    DurableCompactionReceipt, DurableCompactionRecoveryOutcome, DurableSessionAddress,
    DurableSessionRuntime,
};
use crate::runtime::operation_record::{
    compaction_success_lifecycle, operation_result_component, working_lifecycle_for_root,
};
use crate::runtime::terminal::DurableAgentError;
use crate::runtime::{AgentBuilder, OperationId, PostCompactionNote};
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, CheckpointId, CommitCheckpoint,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, FamilyArtifact, ReconstructiveComponent, RuntimeCheckpoint,
    RuntimeComponentKind, RuntimeLifecycle, RuntimeOperationLedger, RuntimeTopology,
    RuntimeWorkingRoot, SessionStoreError, StoreAdmission,
};
use plakous_model::{
    CanonicalHistory, Clock, CompiledProviderConfiguration, EntryId, IdGenerator, LanguageModel,
    ModelCallRecoveryOutcome, ModelOperationRoot, ModelResponse, PreparedCallRecord,
    ProviderCallRecoveryLocator,
};
use std::sync::Arc;
use std::time::Duration;
impl DurableSessionRuntime {
    /// Searches deterministic finalized compaction receipts before consulting
    /// current branch freshness.
    #[doc(hidden)]
    pub async fn compaction_receipt(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
    ) -> Result<DurableCompactionReceipt, AgentError> {
        let identity = DurableCompactionIdentity::new(operation_id, source)?;
        let mut finalized = Vec::new();
        for (checkpoint_id, kind) in [
            (identity.success_checkpoint, CompactionReceiptKind::Success),
            (identity.failure_checkpoint, CompactionReceiptKind::Failure),
            (
                identity.abandonment_evidence_checkpoint,
                CompactionReceiptKind::Abandonment,
            ),
        ] {
            let recovered = match store_call(
                StorePhase::RecoverCommit,
                self.store_timeouts.operation,
                self.store.recover_commit(checkpoint_id),
            )
            .await
            {
                Ok(recovered) => recovered,
                Err(error) if error.is(SessionStoreError::NotFound) => continue,
                Err(error) => return Err(error.into_agent()),
            };
            finalized.push((checkpoint_id, kind, recovered));
        }
        if finalized.len() > 1 {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        if let Some((checkpoint_id, kind, recovered)) = finalized.pop() {
            let stored = store_call(
                StorePhase::LoadCheckpoint,
                self.store_timeouts.operation,
                self.store.load_checkpoint(checkpoint_id),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let source_checkpoint = store_call(
                StorePhase::LoadCheckpoint,
                self.store_timeouts.operation,
                self.store.load_checkpoint(source.checkpoint_id),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let previous_checkpoint_id =
                recovered.receipt().previous().checkpoint().checkpoint_id();
            let previous_checkpoint = store_call(
                StorePhase::LoadCheckpoint,
                self.store_timeouts.operation,
                self.store.load_checkpoint(previous_checkpoint_id),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            validate_compaction_receipt(
                &recovered,
                &stored,
                &source_checkpoint,
                &previous_checkpoint,
                identity,
                kind,
            )?;
            let target =
                DurableSessionAddress::new(source.session_id, source.branch_id, checkpoint_id);
            return Ok(match kind {
                CompactionReceiptKind::Success => {
                    DurableCompactionReceipt::Compacted { source, target }
                }
                CompactionReceiptKind::Failure => {
                    DurableCompactionReceipt::Failed { source, target }
                }
                CompactionReceiptKind::Abandonment => {
                    DurableCompactionReceipt::Abandoned { source, target }
                }
            });
        }
        let loaded = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(source.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let topology = loaded.checkpoint().checkpoint().manifest().topology();
        if matches!(
            loaded.checkpoint().checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::SessionWorkflow(workflow_id),
                ..
            } if *workflow_id == identity.workflow_id
        ) {
            return Ok(DurableCompactionReceipt::RecoveryRequired(head_address(
                topology,
            )));
        }
        Ok(DurableCompactionReceipt::NotFound)
    }

    /// Reconciles one exact recovery-only compaction without redispatch.
    #[doc(hidden)]
    pub async fn recover_compaction(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        builder: AgentBuilder,
    ) -> Result<DurableCompactionRecoveryOutcome, AgentError> {
        match self.compaction_receipt(operation_id, source).await? {
            DurableCompactionReceipt::Compacted { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Compacted { source, target });
            }
            DurableCompactionReceipt::Failed { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Failed { source, target });
            }
            DurableCompactionReceipt::Abandoned { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Abandoned { source, target });
            }
            DurableCompactionReceipt::NotFound => {
                let loaded = store_call(
                    StorePhase::LoadBranch,
                    self.store_timeouts.operation,
                    self.store.load_branch(source.session()),
                )
                .await
                .map_err(StoreCallError::into_agent)?;
                let manifest = loaded.checkpoint().checkpoint().manifest();
                let topology = manifest.topology();
                if topology.checkpoint_id == source.checkpoint_id
                    && matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent)
                {
                    return Ok(DurableCompactionRecoveryOutcome::NotRequired(source));
                }
                if matches!(manifest.lifecycle(), RuntimeLifecycle::Working { .. }) {
                    return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                        head_address(topology),
                    ));
                }
                return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
            }
            DurableCompactionReceipt::RecoveryRequired(_) => {}
        }
        let identity = DurableCompactionIdentity::new(operation_id, source)?;
        let owner_id = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                source.session(),
                owner_id,
                self.lease_seconds,
            )),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let recovery = Box::pin(async {
            let loaded = store_call(
                StorePhase::LoadBranch,
                self.store_timeouts.operation,
                self.store.load_branch(source.session()),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let checkpoint = loaded.checkpoint().checkpoint();
            if !matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::SessionWorkflow(workflow_id),
                    ..
                } if *workflow_id == identity.workflow_id
            ) {
                return match self.compaction_receipt(operation_id, source).await? {
                    DurableCompactionReceipt::Compacted { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Compacted { source, target })
                    }
                    DurableCompactionReceipt::Failed { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Failed { source, target })
                    }
                    DurableCompactionReceipt::Abandoned { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Abandoned { source, target })
                    }
                    DurableCompactionReceipt::NotFound
                        if checkpoint.manifest().topology().checkpoint_id
                            == source.checkpoint_id =>
                    {
                        Ok(DurableCompactionRecoveryOutcome::NotRequired(source))
                    }
                    DurableCompactionReceipt::RecoveryRequired(address) => {
                        Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(address))
                    }
                    DurableCompactionReceipt::NotFound => Err(AgentError::new(
                        AgentErrorKind::Session,
                        "compaction recovery source was superseded by an unrelated head",
                    )),
                };
            }
            // The explicit workflow reads the same verdict as an open and
            // keeps its typed integrity failure.
            if let Some(refusal) = verdict_refusal(checkpoint) {
                return Err(refusal);
            }
            let operations = decode_paid_compaction_operations(checkpoint, identity)?;
            if operations.request.source() != source {
                return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
            }
            if operations.request.version < 3 {
                reconstruct_compaction_cut(checkpoint.manifest(), identity, &operations.request)?;
            }
            // The recovered fold gets the note the live publication would
            // have appended, from the same builder function.
            let note = builder.post_compaction_note.clone();
            let ids = Arc::clone(&builder.ids);
            let runtime = exact_compaction_runtime(
                builder,
                checkpoint,
                operations.request.request.metadata().run_id(),
            )
            .await?;
            let bundle = loaded.checkpoint().bundle().clone();
            let mut owner = CompactionRecoveryOwner {
                store: Arc::clone(&self.store),
                store_timeout: self.store_timeouts.operation,
                lease: lease.clone(),
                head: loaded.head(),
                checkpoint: Arc::clone(checkpoint),
                bundle,
            };
            let DurableCompactionOperations {
                compaction,
                provider,
                request,
                prepared,
            } = operations;
            if owner.checkpoint.manifest().topology().checkpoint_id
                == identity.failure_evidence_checkpoint
            {
                let state = owner
                    .checkpoint
                    .manifest()
                    .ledger()
                    .operation(compaction.0)
                    .map(DurableOperationRecord::state)
                    .ok_or_else(|| {
                        ambiguous_compaction_error(
                            "compaction failure-evidence phase lost its terminal cause",
                        )
                    })?;
                // A compaction stopped before its provider dispatch records its
                // cause as proven-not-dispatched; any later stop as completed.
                if let DurableOperationState::Completed(evidence)
                | DurableOperationState::ProvenNotDispatched(evidence) = state
                {
                    let error = decode_compaction_failure_evidence(evidence)?;
                    let target = recovery_fail_compaction(
                        &mut owner, identity, compaction, provider, &request, None, &error,
                    )
                    .await?;
                    return Ok(DurableCompactionRecoveryOutcome::Failed { source, target });
                }
                if !matches!(state, DurableOperationState::OutcomeUnknown(_)) {
                    return Err(ambiguous_compaction_error(
                        "compaction failure-evidence phase is contradictory",
                    ));
                }
            }
            let provider_record = owner
                .checkpoint
                .manifest()
                .ledger()
                .operation(provider.0)
                .cloned()
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Session,
                        "compaction provider recovery operation disappeared",
                    )
                })?;
            let recovered_response = match provider_record.state() {
                DurableOperationState::Completed(evidence) => {
                    serde_json::from_slice::<ModelResponse>(evidence.bytes()).ok()
                }
                _ => None,
            };
            let response = if let Some(response) = recovered_response {
                Some(response)
            } else {
                let locator = provider_record
                    .latest_locator()
                    .and_then(|component| {
                        serde_json::from_slice::<ProviderCallRecoveryLocator>(component.bytes())
                            .ok()
                    })
                    .or_else(|| prepared.recovery_locator().cloned());
                let Some(locator) = locator else {
                    if matches!(provider_record.state(), DurableOperationState::Dispatched) {
                        let error = ambiguous_compaction_error(
                            "dispatched compaction has no provider recovery locator",
                        );
                        let evidence = operation_result_component::<()>(&Err(error))?;
                        let manifest = owner.checkpoint.manifest();
                        let mut ledger = manifest.ledger().clone();
                        for operation in [compaction, provider] {
                            if !matches!(
                                ledger
                                    .operation(operation.0)
                                    .map(DurableOperationRecord::state),
                                Some(DurableOperationState::Dispatched)
                            ) {
                                return Err(ambiguous_compaction_error(
                                    "no-locator compaction obligations are contradictory",
                                ));
                            }
                            ledger = ledger
                                .transition_operation(
                                    operation.0,
                                    DurableOperationTransition::new(
                                        identity.failure_evidence_checkpoint,
                                        DurableOperationState::OutcomeUnknown(evidence.clone()),
                                    ),
                                )
                                .map_err(runtime_manifest_error)?;
                        }
                        let lifecycle = working_lifecycle_for_root(
                            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
                            &ledger,
                        )?;
                        owner
                            .publish(
                                identity.failure_evidence_checkpoint,
                                lifecycle,
                                manifest.canonical().history().clone(),
                                ledger,
                                manifest.canonical().context().retained_for_exact_resume(),
                                manifest.family_artifact().cloned(),
                            )
                            .await?;
                    }
                    let topology = owner.checkpoint.manifest().topology();
                    return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                        head_address(topology),
                    ));
                };
                let recovery_request =
                    prepared
                        .recovery_request_with_locator(locator)
                        .map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Session,
                                "persisted compaction recovery locator is contradictory",
                            )
                        })?;
                let Ok(Ok(result)) = tokio::time::timeout(
                    runtime.model_operation_timeout,
                    Arc::clone(&runtime.model).recover_call(recovery_request.clone()),
                )
                .await
                else {
                    let topology = owner.checkpoint.manifest().topology();
                    return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                        head_address(topology),
                    ));
                };
                result
                    .validate_for_request(&recovery_request)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Session,
                            "provider compaction recovery result is contradictory",
                        )
                    })?;
                match result.outcome() {
                    ModelCallRecoveryOutcome::Completed(response) => Some((**response).clone()),
                    ModelCallRecoveryOutcome::ProvenNotDispatched => {
                        let Ok(recovery_evidence) = compaction_operation_component(&result) else {
                            let topology = owner.checkpoint.manifest().topology();
                            return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                                head_address(topology),
                            ));
                        };
                        let error = AgentError::new(
                            AgentErrorKind::Model,
                            "provider proved compaction dispatch did not occur",
                        );
                        let target = recovery_fail_compaction(
                            &mut owner,
                            identity,
                            compaction,
                            provider,
                            &request,
                            Some(DurableOperationState::ProvenNotDispatched(
                                recovery_evidence,
                            )),
                            &error,
                        )
                        .await?;
                        return Ok(DurableCompactionRecoveryOutcome::Failed { source, target });
                    }
                    ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
                        let Ok(recovery_evidence) = compaction_operation_component(&result) else {
                            let topology = owner.checkpoint.manifest().topology();
                            return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                                head_address(topology),
                            ));
                        };
                        let error = AgentError::from_model((**error).clone());
                        let target = recovery_fail_compaction(
                            &mut owner,
                            identity,
                            compaction,
                            provider,
                            &request,
                            Some(DurableOperationState::Completed(recovery_evidence)),
                            &error,
                        )
                        .await?;
                        return Ok(DurableCompactionRecoveryOutcome::Failed { source, target });
                    }
                    _ => {
                        let topology = owner.checkpoint.manifest().topology();
                        return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                            head_address(topology),
                        ));
                    }
                }
            };
            let response = response.ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "completed compaction recovery lost its provider response",
                )
            })?;
            let Ok(evidence) = compaction_operation_component(&response) else {
                let topology = owner.checkpoint.manifest().topology();
                return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                    head_address(topology),
                ));
            };
            recovery_complete_compaction_call(&mut owner, identity, compaction, provider, evidence)
                .await?;
            match recovery_publish_compaction_success(
                &mut owner,
                identity,
                compaction,
                provider,
                &request,
                &prepared,
                &response,
                runtime.model,
                &runtime.provider_configuration,
                runtime.clock.as_ref(),
                runtime.model_operation_timeout,
                note.as_ref(),
                ids.as_ref(),
            )
            .await
            {
                Ok(target) => Ok(DurableCompactionRecoveryOutcome::Compacted { source, target }),
                Err(CompactionCompletionError::RecoveryRequired(error)) => {
                    let topology = owner.checkpoint.manifest().topology();
                    // Open-time recovery has no telemetry context to carry
                    // the cause; the head stays recovery-only either way.
                    let _ = error;
                    Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                        head_address(topology),
                    ))
                }
                Err(CompactionCompletionError::Semantic(error)) => {
                    let target = recovery_fail_compaction(
                        &mut owner, identity, compaction, provider, &request, None, &error,
                    )
                    .await?;
                    Ok(DurableCompactionRecoveryOutcome::Failed { source, target })
                }
            }
        })
        .await;
        let cleanup =
            release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup).await;
        match (recovery, cleanup) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Deliberately terminalizes one exact ambiguous compaction without lookup
    /// or redispatch, preserving its semantic source in a quiescent descendant.
    #[doc(hidden)]
    pub async fn abandon_compaction(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
    ) -> Result<DurableCompactionRecoveryOutcome, AgentError> {
        match self.compaction_receipt(operation_id, source).await? {
            DurableCompactionReceipt::Compacted { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Compacted { source, target });
            }
            DurableCompactionReceipt::Failed { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Failed { source, target });
            }
            DurableCompactionReceipt::Abandoned { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Abandoned { source, target });
            }
            DurableCompactionReceipt::NotFound => {
                return Ok(DurableCompactionRecoveryOutcome::NotRequired(source));
            }
            DurableCompactionReceipt::RecoveryRequired(_) => {}
        }
        let identity = DurableCompactionIdentity::new(operation_id, source)?;
        let owner_id = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                source.session(),
                owner_id,
                self.lease_seconds,
            )),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let abandonment = async {
            let loaded = store_call(
                StorePhase::LoadBranch,
                self.store_timeouts.operation,
                self.store.load_branch(source.session()),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let checkpoint = loaded.checkpoint().checkpoint();
            if !matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::SessionWorkflow(workflow_id),
                    ..
                } if *workflow_id == identity.workflow_id
            ) {
                return match self.compaction_receipt(operation_id, source).await? {
                    DurableCompactionReceipt::Compacted { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Compacted { source, target })
                    }
                    DurableCompactionReceipt::Failed { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Failed { source, target })
                    }
                    DurableCompactionReceipt::Abandoned { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Abandoned { source, target })
                    }
                    DurableCompactionReceipt::NotFound
                        if checkpoint.manifest().topology().checkpoint_id
                            == source.checkpoint_id =>
                    {
                        Ok(DurableCompactionRecoveryOutcome::NotRequired(source))
                    }
                    DurableCompactionReceipt::RecoveryRequired(address) => {
                        Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(address))
                    }
                    DurableCompactionReceipt::NotFound => Err(AgentError::new(
                        AgentErrorKind::Session,
                        "compaction abandonment source was superseded by an unrelated head",
                    )),
                };
            }
            // The explicit workflow reads the same verdict as an open and
            // keeps its typed integrity failure.
            if let Some(refusal) = verdict_refusal(checkpoint) {
                return Err(refusal);
            }
            let DurableCompactionOperations {
                compaction,
                provider,
                request,
                ..
            } = decode_paid_compaction_operations(checkpoint, identity)?;
            if request.version < 3 {
                reconstruct_compaction_cut(checkpoint.manifest(), identity, &request)?;
            }
            // The preflight predicate must not hide typed validation failures.
            if !super::checkpoint_can_abandon_compaction(checkpoint, operation_id, source) {
                let topology = checkpoint.manifest().topology();
                return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                    head_address(topology),
                ));
            }
            let bundle = loaded.checkpoint().bundle().clone();
            let mut owner = CompactionRecoveryOwner {
                store: Arc::clone(&self.store),
                store_timeout: self.store_timeouts.operation,
                lease: lease.clone(),
                head: loaded.head(),
                checkpoint: Arc::clone(checkpoint),
                bundle,
            };
            let settled = [compaction, provider].map(|operation| {
                owner
                    .checkpoint
                    .manifest()
                    .ledger()
                    .operation(operation.0)
                    .is_some_and(|record| {
                        matches!(record.state(), DurableOperationState::Completed(_))
                    })
            });
            match settled {
                // Completed evidence already permits the original failure
                // publication; do not replace its receipt with abandonment.
                [true, true] => {
                    let target = recovery_fail_compaction(
                        &mut owner,
                        identity,
                        compaction,
                        provider,
                        &request,
                        None,
                        &abandonment_error(),
                    )
                    .await?;
                    Ok(DurableCompactionRecoveryOutcome::Failed { source, target })
                }
                [false, false] => {
                    let target = close_abandoned_compaction(
                        &mut owner, identity, compaction, provider, &request,
                    )
                    .await?;
                    Ok(DurableCompactionRecoveryOutcome::Abandoned { source, target })
                }
                _ => {
                    let topology = owner.checkpoint.manifest().topology();
                    Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(
                        head_address(topology),
                    ))
                }
            }
        }
        .await;
        let cleanup =
            release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup).await;
        match (abandonment, cleanup) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

impl CompactionRecoveryOwner {
    #[allow(
        clippy::too_many_arguments,
        reason = "recovery publication consumes the exact immutable checkpoint groups"
    )]
    pub(in crate::runtime) async fn publish(
        &mut self,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family: Option<FamilyArtifact>,
    ) -> Result<(), AgentError> {
        let prior = self.checkpoint.manifest();
        let topology = prior.topology();
        if self.head.checkpoint().checkpoint_id() != topology.checkpoint_id {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "compaction recovery head changed before publication",
            ));
        }
        let CheckpointCandidate {
            checkpoint,
            bundle,
            expected_ref,
            commit_identity,
        } = successor_candidate(
            &self.checkpoint,
            &self.bundle,
            SuccessorFacts {
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                jobs: prior.jobs().clone(),
                context,
                family,
            },
            self.lease.fence().get(),
            prior.interpreter_observation().cloned(),
        )?;
        let commit = CommitCheckpoint::new(self.lease.clone(), self.head, bundle.clone())
            .with_admission(StoreAdmission::Recovery)
            .with_feed_facts(crate::feed::facts::commit_facts(
                &self.checkpoint,
                &checkpoint,
            )?);
        let receipt = Box::pin(commit_with_acknowledgement_recovery(
            &self.store,
            self.store_timeout,
            checkpoint_id,
            commit,
            self.head,
            expected_ref,
            commit_identity,
            // Open-time recovery has no session telemetry context; durability
            // telemetry covers turn-time commits only.
            None,
        ))
        .await?;
        self.head = receipt.head();
        self.checkpoint = checkpoint;
        self.bundle = bundle;
        Ok(())
    }
}

/// The failure an abandoned compaction's records finalize with.
fn abandonment_error() -> AgentError {
    AgentError::new(
        AgentErrorKind::Cancelled,
        "ambiguous compaction was deliberately abandoned by the caller",
    )
}

/// Closes an interrupted paid compaction without redispatch in ONE quiescent
/// closure commit at its abandonment checkpoint: a Prepared record is proven
/// not dispatched, an uncertain one is closed with the abandonment evidence,
/// and both are finalized in the same commit, which therefore carries the
/// workflow's `WorkflowSettled { Abandoned }` fact.
pub(in crate::runtime) async fn close_abandoned_compaction(
    owner: &mut CompactionRecoveryOwner,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
) -> Result<DurableSessionAddress, AgentError> {
    let evidence = operation_result_component::<()>(&Err(abandonment_error()))?;
    let manifest = owner.checkpoint.manifest();
    if manifest.canonical().history().identity() != request.source_history {
        return Err(ambiguous_compaction_error(
            "compaction abandonment lost its semantic source",
        ));
    }
    let checkpoint_id = identity.abandonment_evidence_checkpoint;
    let mut ledger = manifest.ledger().clone();
    for operation in [compaction, provider] {
        let closed = match ledger
            .operation(operation.0)
            .map(DurableOperationRecord::state)
        {
            Some(DurableOperationState::Prepared) => {
                DurableOperationState::ProvenNotDispatched(evidence.clone())
            }
            Some(
                DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_),
            ) => DurableOperationState::Completed(evidence.clone()),
            _ => {
                return Err(ambiguous_compaction_error(
                    "compaction abandonment found a settled or missing operation",
                ));
            }
        };
        ledger = ledger
            .transition_operation_many(
                operation.0,
                [
                    DurableOperationTransition::new(checkpoint_id, closed),
                    DurableOperationTransition::new(
                        checkpoint_id,
                        DurableOperationState::Finalized(evidence.clone()),
                    ),
                ],
            )
            .map_err(runtime_manifest_error)?;
    }
    let history = manifest.canonical().history().clone();
    let context = manifest.canonical().context().retained_for_exact_resume();
    let family = manifest.family_artifact().cloned();
    owner
        .publish(
            checkpoint_id,
            RuntimeLifecycle::Quiescent,
            history,
            ledger,
            context,
            family,
        )
        .await?;
    Ok(DurableSessionAddress::new(
        request.source_session,
        request.source_branch,
        checkpoint_id,
    ))
}

pub(in crate::runtime) fn decode_compaction_operations(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> Result<DecodedCompactionWorkflow, AgentError> {
    let mut compaction = None;
    let mut provider = None;
    for record in checkpoint.manifest().ledger().operations() {
        match record.kind() {
            DurableOperationKind::Compaction {
                workflow_id: candidate,
            } if *candidate == identity.workflow_id => {
                if compaction.is_some() {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "compaction recovery ledger contains duplicate workflow operations",
                    ));
                }
                let Ok(request) = serde_json::from_slice::<DurableCompactionRequestRecord>(
                    record.request().bytes(),
                ) else {
                    // The offline placeholder rung publishes a Compaction
                    // operation whose request is a placeholder pass plan and which is
                    // finalized in the same CAS, so it has no provider
                    // attempt and no recoverable dispatch. Report that shape
                    // rather than reading it as a malformed paid request.
                    let offline: DurablePlaceholderRequestRecord =
                        serde_json::from_slice(record.request().bytes()).map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Session,
                                "compaction recovery request is not reconstructive",
                            )
                        })?;
                    let canonical = runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&offline).map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::Internal,
                                "placeholders request could not be canonicalized",
                            )
                        })?,
                    )?;
                    if &canonical != record.request()
                        || !((offline.version == 1 && offline.replaced_results > 0)
                            || (offline.version == 2
                                && offline.replaced_results == 0
                                && offline.replaced_bytes == 0))
                        || offline.workflow_id != identity.workflow_id
                    {
                        return Err(AgentError::new(
                            AgentErrorKind::Session,
                            "placeholders request encoding is not canonical",
                        ));
                    }
                    return Ok(DecodedCompactionWorkflow::OfflinePlaceholders {
                        compaction: OperationId(record.operation_id()),
                        resolved: matches!(record.state(), DurableOperationState::Finalized(_)),
                    });
                };
                let canonical = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&request).map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "compaction recovery request could not be canonicalized",
                        )
                    })?,
                )?;
                if &canonical != record.request() {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "compaction recovery request encoding is not canonical",
                    ));
                }
                compaction = Some((OperationId(record.operation_id()), request));
            }
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(candidate),
                attempt_id,
            } if *candidate == identity.workflow_id => {
                if provider.is_some() || *attempt_id != identity.attempt_id {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "compaction recovery provider scope is duplicated or contradictory",
                    ));
                }
                let prepared: PreparedCallRecord = serde_json::from_slice(record.request().bytes())
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Session,
                            "prepared compaction recovery call is not reconstructive",
                        )
                    })?;
                let canonical = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&prepared).map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "prepared compaction call could not be canonicalized",
                        )
                    })?,
                )?;
                if &canonical != record.request() {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "prepared compaction call encoding is not canonical",
                    ));
                }
                provider = Some((OperationId(record.operation_id()), prepared));
            }
            _ => {}
        }
    }
    let (compaction_id, request) = compaction.ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Session,
            "compaction recovery operation is absent",
        )
    })?;
    let (provider_id, prepared) = provider.ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Session,
            "compaction provider operation is absent",
        )
    })?;
    let expected_root = ModelOperationRoot::SessionWorkflow(identity.workflow_id);
    if (request.version != 2 && request.version != 3)
        || request.workflow_id != identity.workflow_id
        || request.source() != identity.source
        || request.request.metadata().root() != expected_root
        || request.request.metadata().attempt_id() != identity.attempt_id
        || prepared.projection().operation_root() != expected_root
        || prepared.projection().attempt_id() != identity.attempt_id
        || prepared.projection().request() != request.request.identity()
        || prepared.projection().model_input() != request.request.model_input_identity()
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "compaction recovery request, prepared call, and provider root disagree",
        ));
    }
    if let Some(prepared_locator) = prepared.recovery_locator()
        && (prepared_locator.operation_root() != expected_root
            || prepared_locator.request() != request.request.identity()
            || prepared_locator.projection() != prepared.projection().identity()
            || prepared.recovery_request().is_err())
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "compaction recovery locator disagrees with the prepared call",
        ));
    }
    Ok(DecodedCompactionWorkflow::Paid(Box::new(
        DurableCompactionOperations {
            compaction: compaction_id,
            provider: provider_id,
            request,
            prepared,
        },
    )))
}

/// Decodes the paid workflow, refusing the offline placeholders shape.
///
/// Every recovery and receipt path is about a dispatched provider operation.
/// A placeholder pass never dispatches and is finalized in its own publication, so
/// reaching one of those paths with a placeholder pass identity is not something to
/// recover but something to name.
pub(in crate::runtime) fn decode_paid_compaction_operations(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> Result<DurableCompactionOperations, AgentError> {
    match decode_compaction_operations(checkpoint, identity)? {
        DecodedCompactionWorkflow::Paid(operations) => Ok(*operations),
        DecodedCompactionWorkflow::OfflinePlaceholders {
            compaction,
            resolved,
        } => Err(AgentError::new(
            AgentErrorKind::Session,
            format!(
                "compaction operation {} is a{} offline placeholders; it has no provider outcome to recover",
                compaction.0,
                if resolved {
                    " committed"
                } else {
                    "n unfinalized"
                },
            ),
        )),
    }
}

async fn recovery_complete_compaction_call(
    owner: &mut CompactionRecoveryOwner,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    evidence: ReconstructiveComponent,
) -> Result<(), AgentError> {
    let manifest = owner.checkpoint.manifest();
    let current_checkpoint = manifest.topology().checkpoint_id;
    let current_states = [compaction, provider].map(|operation| {
        manifest
            .ledger()
            .operation(operation.0)
            .map(|record| record.state().clone())
    });
    if current_checkpoint == identity.completed_checkpoint {
        if current_states
            .iter()
            .all(|state| matches!(state, Some(DurableOperationState::Completed(value)) if value == &evidence))
        {
            return Ok(());
        }
        return Err(ambiguous_compaction_error(
            "completed compaction checkpoint carries contradictory response evidence",
        ));
    }
    if current_states.iter().any(|state| {
        matches!(
            state,
            Some(
                DurableOperationState::Completed(_)
                    | DurableOperationState::ProvenNotDispatched(_)
                    | DurableOperationState::Finalized(_),
            )
        )
    }) {
        return Err(ambiguous_compaction_error(
            "compaction response evidence appeared at a contradictory recovery phase",
        ));
    }
    let mut ledger = manifest.ledger().clone();
    for operation in [compaction, provider] {
        ledger = ledger
            .transition_operation(
                operation.0,
                DurableOperationTransition::new(
                    identity.completed_checkpoint,
                    DurableOperationState::Completed(evidence.clone()),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    let lifecycle = working_lifecycle_for_root(
        ModelOperationRoot::SessionWorkflow(identity.workflow_id),
        &ledger,
    )?;
    owner
        .publish(
            identity.completed_checkpoint,
            lifecycle,
            manifest.canonical().history().clone(),
            ledger,
            manifest.canonical().context().retained_for_exact_resume(),
            manifest.family_artifact().cloned(),
        )
        .await
}

pub(in crate::runtime) async fn recovery_fail_compaction(
    owner: &mut CompactionRecoveryOwner,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
    provider_terminal: Option<DurableOperationState>,
    error: &AgentError,
) -> Result<DurableSessionAddress, AgentError> {
    let evidence = operation_result_component::<()>(&Err(error.clone()))?;
    let manifest = owner.checkpoint.manifest();
    if manifest.canonical().history().identity() != request.source_history {
        return Err(ambiguous_compaction_error(
            "compaction recovery failure lost its semantic source",
        ));
    }
    let current_checkpoint = manifest.topology().checkpoint_id;
    let mut ledger = manifest.ledger().clone();
    if current_checkpoint == identity.failure_evidence_checkpoint {
        let compaction_state = ledger
            .operation(compaction.0)
            .map(DurableOperationRecord::state);
        let provider_state = ledger
            .operation(provider.0)
            .map(DurableOperationRecord::state);
        if !matches!(
            compaction_state,
            Some(
                DurableOperationState::Completed(value)
                    | DurableOperationState::ProvenNotDispatched(value)
            ) if value == &evidence
        ) || !matches!(
            provider_state,
            Some(
                DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_)
            )
        ) || provider_terminal
            .as_ref()
            .is_some_and(|expected| provider_state.is_none_or(|actual| actual != expected))
        {
            return Err(ambiguous_compaction_error(
                "compaction failure-evidence checkpoint is contradictory",
            ));
        }
    } else {
        let mut transitioned = false;
        for operation in [compaction, provider] {
            if !matches!(
                ledger
                    .operation(operation.0)
                    .map(DurableOperationRecord::state),
                Some(
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_)
                )
            ) {
                let terminal = if operation == provider {
                    provider_terminal
                        .clone()
                        .unwrap_or_else(|| DurableOperationState::Completed(evidence.clone()))
                } else if matches!(
                    provider_terminal,
                    Some(DurableOperationState::ProvenNotDispatched(_))
                ) {
                    // The provider's exact negative dispatch evidence also
                    // proves the enclosing paid compaction never crossed its
                    // dispatch fence. Prepared -> Completed is deliberately
                    // illegal; retain the stronger negative fact.
                    DurableOperationState::ProvenNotDispatched(evidence.clone())
                } else {
                    DurableOperationState::Completed(evidence.clone())
                };
                ledger = ledger
                    .transition_operation(
                        operation.0,
                        DurableOperationTransition::new(
                            identity.failure_evidence_checkpoint,
                            terminal,
                        ),
                    )
                    .map_err(runtime_manifest_error)?;
                transitioned = true;
            }
        }
        if transitioned {
            let lifecycle = working_lifecycle_for_root(
                ModelOperationRoot::SessionWorkflow(identity.workflow_id),
                &ledger,
            )?;
            owner
                .publish(
                    identity.failure_evidence_checkpoint,
                    lifecycle,
                    manifest.canonical().history().clone(),
                    ledger,
                    manifest.canonical().context().retained_for_exact_resume(),
                    manifest.family_artifact().cloned(),
                )
                .await?;
        }
    }
    let manifest = owner.checkpoint.manifest();
    let mut ledger = manifest.ledger().clone();
    for operation in [compaction, provider] {
        let terminal = if operation == compaction {
            evidence.clone()
        } else {
            ledger
                .operation(operation.0)
                .and_then(|record| match record.state() {
                    DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence) => {
                        Some(evidence.clone())
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    ambiguous_compaction_error(
                        "compaction recovery failure lacks provider evidence",
                    )
                })?
        };
        ledger = ledger
            .transition_operation(
                operation.0,
                DurableOperationTransition::new(
                    identity.failure_checkpoint,
                    DurableOperationState::Finalized(terminal),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    let family = manifest.family_artifact().cloned();
    let history = manifest.canonical().history().clone();
    let context = manifest.canonical().context().retained_for_exact_resume();
    owner
        .publish(
            identity.failure_checkpoint,
            RuntimeLifecycle::Quiescent,
            history,
            ledger,
            context,
            family,
        )
        .await?;
    Ok(DurableSessionAddress::new(
        request.source_session,
        request.source_branch,
        identity.failure_checkpoint,
    ))
}

pub(in crate::runtime) fn decode_compaction_failure_evidence(
    evidence: &ReconstructiveComponent,
) -> Result<AgentError, AgentError> {
    let (disposition, durable): (String, DurableAgentError) =
        serde_json::from_slice(evidence.bytes()).map_err(|_| {
            ambiguous_compaction_error("compaction failure evidence is not reconstructive")
        })?;
    if disposition != "failed" {
        return Err(ambiguous_compaction_error(
            "compaction failure evidence has a contradictory disposition",
        ));
    }
    let error = durable.into_error()?;
    if error.kind() == AgentErrorKind::OutcomeUnknown
        || operation_result_component::<()>(&Err(error.clone()))? != *evidence
    {
        return Err(ambiguous_compaction_error(
            "compaction failure evidence is ambiguous or non-canonical",
        ));
    }
    Ok(error)
}

#[allow(
    clippy::too_many_arguments,
    reason = "restart publication consumes the exact persisted compaction operation tuple"
)]
pub(in crate::runtime) async fn recovery_publish_compaction_success(
    owner: &mut CompactionRecoveryOwner,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
    prepared: &PreparedCallRecord,
    response: &ModelResponse,
    model: Arc<dyn LanguageModel>,
    current_configuration: &CompiledProviderConfiguration,
    clock: &dyn Clock,
    authority_timeout: Duration,
    note: Option<&PostCompactionNote>,
    ids: &dyn IdGenerator,
) -> Result<DurableSessionAddress, CompactionCompletionError> {
    let fold = build_compaction_success_fold(
        owner.checkpoint.manifest(),
        identity,
        compaction,
        provider,
        request,
        prepared,
        response,
        model,
        current_configuration,
        clock,
        authority_timeout,
    )
    .await?;
    let history = with_post_compaction_note(
        fold.history,
        owner.checkpoint.manifest().canonical().history(),
        note,
        || EntryId::generate(clock, ids).map_err(|error| AgentError::from_identity(&error)),
    )
    .map_err(CompactionCompletionError::semantic)?;
    owner
        .publish(
            identity.success_checkpoint,
            compaction_success_lifecycle(&fold.ledger)
                .map_err(CompactionCompletionError::semantic)?,
            history,
            fold.ledger,
            fold.context,
            None,
        )
        .await
        .map_err(CompactionCompletionError::recovery)?;
    Ok(DurableSessionAddress::new(
        request.source_session,
        request.source_branch,
        identity.success_checkpoint,
    ))
}

/// The exact address of the head this recovery inspected.
fn head_address(topology: RuntimeTopology) -> DurableSessionAddress {
    DurableSessionAddress::new(
        topology.session_id,
        topology.branch_id,
        topology.checkpoint_id,
    )
}
