//! Unsupported host-only move authority; not a supported embedding API or wire codec.
use super::{AgentSessions, AgentSessionsError, BranchHold, store_error};
pub use plakous_agent::internal::SessionWorkflowId as MoveOperation;
use plakous_agent::internal::host_move::PrepareMove;
pub use plakous_agent::internal::host_move::{
    MoveBranch, MoveDisposition, MoveInstall, MoveLine, MoveSession,
};
use plakous_model::Digest;

impl AgentSessions {
    /// Recover a frozen batch after acknowledgement loss or restart, without a new grant.
    ///
    /// # Errors
    /// Refuses unknown, disposed or mismatching batches.
    #[doc(hidden)]
    pub async fn recover_move(
        &self,
        operation: MoveOperation,
    ) -> Result<MoveInstall, AgentSessionsError> {
        self.backend
            .recover_move(operation)
            .await
            .map_err(store_error)
    }
    /// Freeze exact held branches and return semantic state without creating a driver.
    ///
    /// # Errors
    /// Refuses incomplete holds, unresolved runtime boundaries or incompatible identities.
    #[doc(hidden)]
    pub async fn prepare_move(
        &self,
        operation: MoveOperation,
        holds: &[&BranchHold],
    ) -> Result<MoveInstall, AgentSessionsError> {
        self.backend
            .prepare_move(PrepareMove {
                operation,
                leases: holds.iter().map(|h| h.inner.move_lease()).collect(),
            })
            .await
            .map_err(store_error)
    }

    /// Stage a reconstructed batch invisibly, with imported floors and no lease.
    ///
    /// # Errors
    /// Refuses conflicting identities, invalid semantics or exhausted destination capacity.
    #[doc(hidden)]
    pub async fn stage_move(
        &self,
        operation: MoveOperation,
        install: MoveInstall,
    ) -> Result<(), AgentSessionsError> {
        self.backend
            .stage_move(operation, install)
            .await
            .map_err(store_error)
    }

    /// Resolve only the matching operation/digest. Committed retirement cannot abort.
    ///
    /// # Errors
    /// Refuses mismatching operations/digests and invalid or irreversible transitions.
    #[doc(hidden)]
    pub async fn finish_move(
        &self,
        operation: MoveOperation,
        digest: Digest,
        disposition: MoveDisposition,
    ) -> Result<(), AgentSessionsError> {
        self.backend
            .finish_move(operation, digest, disposition)
            .await
            .map_err(store_error)
    }
}
