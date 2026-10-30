//! Fenced recovery leases and checkpoint publication.

use std::{sync::Arc, time::Duration};

use plakous_model::CanonicalHistory;

use super::bootstrap::release_store_lease;
use super::branch::operation_run_id;
use super::{DurableSessionAddress, DurableSessionRuntime};
use crate::manifest::RuntimeContextState;
use crate::runtime::checkpoint::candidate::{
    CheckpointCandidate, SuccessorFacts, successor_candidate,
};
use crate::runtime::checkpoint::io::DurabilityCommitTelemetry;
use crate::runtime::checkpoint::io::{
    StoreCallError, StorePhase, capacity_store_call, commit_with_acknowledgement_recovery,
    store_call,
};
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, BranchHead, BranchLease, CheckpointId,
    CheckpointObjectBundle, CommitCheckpoint, FamilyArtifact, RuntimeCheckpoint, RuntimeLifecycle,
    RuntimeOperationLedger, SessionStore, StoreAdmission,
};

pub(super) struct RecoveryPublicationOwner<'a> {
    pub(super) store: Arc<dyn SessionStore>,
    pub(super) store_timeout: Duration,
    pub(super) lease: &'a mut BranchLease,
    pub(super) head: BranchHead,
    pub(super) checkpoint: Arc<RuntimeCheckpoint>,
    pub(super) bundle: CheckpointObjectBundle,
}

impl DurableSessionRuntime {
    pub(super) async fn acquire_recovery_lease(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
    ) -> Result<BranchLease, AgentError> {
        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        capacity_store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            || {
                self.store.acquire(AcquireBranch::new(
                    source.session(),
                    owner,
                    self.lease_seconds,
                ))
            },
            || self.store.storage_wait_cancelled(),
        )
        .await
        .map_err(StoreCallError::into_agent)
    }

    pub(super) async fn load_recovery_owner<'a>(
        &self,
        lease: &'a mut BranchLease,
        source: DurableSessionAddress,
    ) -> Result<RecoveryPublicationOwner<'a>, AgentError> {
        let loaded = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(source.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        Ok(RecoveryPublicationOwner {
            store: Arc::clone(&self.store),
            store_timeout: self.store_timeouts.operation,
            lease,
            head: loaded.head(),
            checkpoint: Arc::clone(loaded.checkpoint().checkpoint()),
            bundle: loaded.checkpoint().bundle().clone(),
        })
    }

    pub(super) async fn release_recovery_lease<T>(
        &self,
        lease: BranchLease,
        result: Result<T, AgentError>,
    ) -> Result<T, AgentError> {
        let cleanup =
            release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup).await;
        match (result, cleanup) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

impl RecoveryPublicationOwner<'_> {
    pub(super) async fn publish(
        &mut self,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family: Option<FamilyArtifact>,
    ) -> Result<(), AgentError> {
        let prior = self.checkpoint.manifest();
        if self.head.checkpoint().checkpoint_id() != prior.topology().checkpoint_id {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "recovery head changed before publication",
            ));
        }
        let candidate = successor_candidate(
            &self.checkpoint,
            &self.bundle,
            SuccessorFacts {
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                jobs: prior.jobs().clone(),
                named_state: prior.named_state().clone(),
                context,
                family,
            },
            self.lease.fence().get(),
            prior.interpreter_observation().cloned(),
        )?;
        self.publish_candidate(candidate, None).await
    }

    pub(super) async fn publish_candidate(
        &mut self,
        candidate: CheckpointCandidate,
        telemetry: Option<&DurabilityCommitTelemetry>,
    ) -> Result<(), AgentError> {
        let CheckpointCandidate {
            checkpoint,
            bundle,
            expected_ref,
            commit_identity,
        } = candidate;
        let checkpoint_id = checkpoint.id();
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
            telemetry,
            self.lease,
        ))
        .await?;
        self.head = receipt.head();
        self.checkpoint = checkpoint;
        self.bundle = bundle;
        Ok(())
    }
}
