//! Owner-driven reclamation of superseded checkpoint history.

use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::{operation_run_id, validate_session_operation_id};
use crate::runtime::durable::{DurableSessionAddress, DurableSessionRuntime};
use crate::runtime::terminal::checkpoint_retained_history;
use crate::{
    AcquireBranch, AgentError, PruneBranch, PruneReceipt, RuntimeCheckpointKind, SessionStoreError,
};
use std::sync::Arc;

impl DurableSessionRuntime {
    /// Reclaims the superseded history of one released session at its exact
    /// head. Returns `None` when the head is a Working turn: its recovery may
    /// still read older checkpoints, so it is left for recovery first.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership failure when another owner holds the branch,
    /// an identity conflict when the head moved past `address`, or the store
    /// failure. The acquired lease is always released.
    #[doc(hidden)]
    pub async fn prune_history(
        &self,
        operation_id: &str,
        address: DurableSessionAddress,
    ) -> Result<Option<PruneReceipt>, AgentError> {
        validate_session_operation_id(operation_id)?;
        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                address.session(),
                owner,
                self.lease_seconds,
            )),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let pruned = async {
            let loaded = store_call(
                StorePhase::LoadBranch,
                self.store_timeouts.operation,
                self.store.load_branch(address.session()),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let checkpoint = loaded.checkpoint().checkpoint();
            if checkpoint.id() != address.checkpoint_id {
                return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
            }
            if checkpoint.kind() != RuntimeCheckpointKind::Quiescent {
                return Ok(None);
            }
            let request = PruneBranch::new(
                lease.clone(),
                loaded.head(),
                checkpoint_retained_history(checkpoint),
                1,
            );
            store_call(
                StorePhase::Prune,
                self.store_timeouts.operation,
                self.store.prune(request),
            )
            .await
            .map(Some)
            .map_err(StoreCallError::into_agent)
        }
        .await;
        let cleanup =
            release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup).await;
        match (pruned, cleanup) {
            (Ok(receipt), Ok(())) => Ok(receipt),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}
