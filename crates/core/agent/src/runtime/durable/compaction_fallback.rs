//! Exact receipt joining and driver-owned unpaid continuation of endpoint folds.

use super::compaction_recovery::{
    decode_compaction_failure_evidence, decode_paid_compaction_operations,
};
use super::{
    DurableCompactionReceipt, DurableCompactionRecoveryOutcome, DurableRuntimeOpen,
    DurableSessionAddress, DurableSessionRuntime,
};
use crate::runtime::AgentBuilder;
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::compaction::request::validate_compaction_builder;
use crate::runtime::compaction::{
    DurableCompactionIdentity, DurableCompactionRequestRecord, DurablePlaceholderRequestRecord,
};
use crate::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason, AgentError,
    AgentErrorKind, AgentSessionEndpoint, DurableOperationKind, DurableOperationState,
    ReconstructiveComponent, RuntimeCheckpoint, SessionStoreError, StoredCheckpoint,
};

pub(in crate::runtime) fn failed_endpoint_request(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> Result<Option<DurableCompactionRequestRecord>, AgentError> {
    if checkpoint.id() != identity.failure_checkpoint {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }
    let operations = decode_paid_compaction_operations(checkpoint, identity)?;
    let record = checkpoint
        .manifest()
        .ledger()
        .operation(operations.compaction.0)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
    let DurableOperationState::Finalized(evidence) = record.state() else {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    };
    let error = decode_compaction_failure_evidence(evidence)?;
    if operations.request.turn_fence || error.kind() == AgentErrorKind::Cancelled {
        return Ok(None);
    }
    Ok(Some(operations.request))
}

impl DurableSessionRuntime {
    /// Validates the source rewrite of a live endpoint's unpaid successor.
    #[doc(hidden)]
    pub async fn joined_compaction_source(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        fallback_source: DurableSessionAddress,
        target: DurableSessionAddress,
    ) -> Result<DurableSessionAddress, AgentError> {
        let direct = self.direct_compaction_receipt(operation_id, source).await?;
        if !matches!(direct, DurableCompactionReceipt::Failed { target, .. } if target == fallback_source)
            || !matches!(self.joined_fallback_receipt(operation_id, direct).await?,
                DurableCompactionReceipt::Compacted { target: joined, .. } if joined == target)
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        Ok(source)
    }

    async fn fallback_checkpoint(
        &self,
        address: DurableSessionAddress,
    ) -> Result<StoredCheckpoint, AgentError> {
        store_call(
            StorePhase::LoadCheckpoint,
            self.store_timeouts.operation,
            self.store.load_checkpoint(address.checkpoint_id),
        )
        .await
        .map_err(StoreCallError::into_agent)
    }

    pub(super) async fn joined_fallback_receipt(
        &self,
        operation_id: &str,
        receipt: DurableCompactionReceipt,
    ) -> Result<DurableCompactionReceipt, AgentError> {
        let DurableCompactionReceipt::Failed {
            source,
            target: failed,
        } = receipt
        else {
            return Ok(receipt);
        };
        let identity = DurableCompactionIdentity::new(operation_id, source)?;
        let failure = self.fallback_checkpoint(failed).await?;
        let Some(original) = failed_endpoint_request(failure.checkpoint(), identity)? else {
            return Ok(receipt);
        };
        let child_operation = format!("{operation_id}:placeholders");
        let child_receipt = self
            .direct_compaction_receipt(&child_operation, failed)
            .await?;
        let DurableCompactionReceipt::Compacted {
            source: child_source,
            target,
        } = child_receipt
        else {
            if !matches!(child_receipt, DurableCompactionReceipt::NotFound) {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            return Ok(receipt);
        };
        let child = DurableCompactionIdentity::new(&child_operation, failed)?;
        let successor = self.fallback_checkpoint(target).await?;
        let operations = successor.checkpoint().manifest().ledger().operations();
        let record = operations.iter().find(|record|
            matches!(record.kind(), DurableOperationKind::Compaction { workflow_id } if *workflow_id == child.workflow_id))
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        validate_join(
            &original,
            failure.checkpoint(),
            record.request(),
            child,
            &child_operation,
            child_source,
        )?;
        Ok(DurableCompactionReceipt::Compacted { source, target })
    }

    /// Finishes only the unpaid rung of an already finalized endpoint failure.
    /// No provider dispatch is reachable from the continuation command.
    #[doc(hidden)]
    pub async fn continue_failed_compaction(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        mut builder: AgentBuilder,
        endpoint: Option<AgentSessionEndpoint>,
    ) -> Result<DurableCompactionRecoveryOutcome, AgentError> {
        let receipt = self.compaction_receipt(operation_id, source).await?;
        let failed = match receipt {
            DurableCompactionReceipt::Compacted { source, target } => {
                return Ok(DurableCompactionRecoveryOutcome::Compacted { source, target });
            }
            DurableCompactionReceipt::Failed { target, .. } => target,
            _ => return Err(AgentError::from_store(SessionStoreError::IdentityConflict)),
        };
        let failure = self.fallback_checkpoint(failed).await?;
        let identity = DurableCompactionIdentity::new(operation_id, source)?;
        let unchanged = DurableCompactionRecoveryOutcome::Failed {
            source,
            target: failed,
        };
        if failed_endpoint_request(failure.checkpoint(), identity)?.is_none() {
            return Ok(unchanged);
        }
        let loaded = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(source.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        if loaded.checkpoint().checkpoint().id() != failed.checkpoint_id {
            return Ok(unchanged);
        }
        let child_operation = format!("{operation_id}:placeholders");
        DurableCompactionIdentity::new(&child_operation, failed)?;
        let mut owner = None;
        let (endpoint, runtime) = if let Some(endpoint) = endpoint {
            (endpoint, Some(builder))
        } else {
            validate_compaction_builder(&builder, failure.checkpoint())?;
            // The temporary owner is the ordinary source runtime. Only its
            // historical turn limit, not the caller's summary limit, resumes.
            builder.request_policy = builder.request_policy.with_generation_limits(
                failure
                    .checkpoint()
                    .manifest()
                    .canonical()
                    .stable_prefix()
                    .limits()
                    .clone(),
            );
            let opened = Box::pin(self.open(&child_operation, failed, builder)).await?;
            let DurableRuntimeOpen::Opened(agent) = opened else {
                return Ok(DurableCompactionRecoveryOutcome::RecoveryRequired(failed));
            };
            let endpoint = agent.session_endpoint();
            owner = Some(agent);
            (endpoint, None)
        };
        let result = endpoint
            .compact_session(AgentCompactSessionRequest {
                operation_id: child_operation,
                session_id: failed.session_id,
                branch_id: failed.branch_id,
                checkpoint_id: failed.checkpoint_id,
                reason: AgentCompactionReason::Manual,
                pending_user_input: None,
                runtime,
                fallback_of: Some((operation_id.to_owned(), source)),
            })
            .await;
        if let Some(owner) = owner {
            owner.shutdown().await?;
        }
        match result {
            Ok(AgentCompactSessionOutcome::Compacted { .. }) => {
                match self.compaction_receipt(operation_id, source).await? {
                    DurableCompactionReceipt::Compacted { source, target } => {
                        Ok(DurableCompactionRecoveryOutcome::Compacted { source, target })
                    }
                    _ => Err(AgentError::from_store(SessionStoreError::corrupt())),
                }
            }
            Ok(AgentCompactSessionOutcome::NoEligibleRange) => Ok(unchanged),
            _ => Err(AgentError::new(
                AgentErrorKind::Session,
                "unpaid compaction continuation did not return an exact receipt",
            )),
        }
    }
}

pub(super) fn validate_join(
    original: &DurableCompactionRequestRecord,
    failure: &RuntimeCheckpoint,
    component: &ReconstructiveComponent,
    child: DurableCompactionIdentity,
    child_operation: &str,
    child_source: DurableSessionAddress,
) -> Result<(), AgentError> {
    let refuse = || AgentError::from_store(SessionStoreError::corrupt());
    let request: DurablePlaceholderRequestRecord =
        serde_json::from_slice(component.bytes()).map_err(|_| refuse())?;
    if child_source != child.source
        || request.operation_id != child_operation
        || child_operation != format!("{}:placeholders", original.operation_id)
        || request.turn_fence
        || request.workflow_id != child.workflow_id
        || request.source_session != child.source.session_id
        || request.source_branch != child.source.branch_id
        || request.source_checkpoint != failure.id()
        || request.source_history != original.source_history
        || request.source_projection != failure.manifest().canonical().projection().identity()
        || request.source_harness_plan != original.source_harness_plan
    {
        return Err(refuse());
    }
    Ok(())
}
