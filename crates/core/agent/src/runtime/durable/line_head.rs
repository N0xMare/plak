//! Lease-only open of an agent line's head (spec 01 §5.3).
//!
//! A lease-only holder owns the head branch's lease and fence without a
//! driver, route, catalog or credential. It keeps other owners off the head
//! while a full open is not possible, and it is the only writer of the line's
//! job terminal table.

use crate::manifest::LineJobTerminal;
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, capacity_store_call, store_call};
use crate::runtime::durable::DurableSessionRuntime;
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::operation_run_id;
use crate::session::{LineRecord, RecordLineJobTerminals};
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, BranchLease, SessionStore, SessionStoreError,
};
use plakous_model::{CheckpointId, LineId, SessionId};
use plakous_tools::JobView;
use std::fmt;
use std::sync::Arc;

/// The lease of one line's head, held without a driver.
pub struct DurableLineHead {
    store: Arc<dyn SessionStore>,
    line: LineId,
    generation: u64,
    head: CheckpointId,
    lease: BranchLease,
    timeouts: crate::runtime::StoreTimeouts,
}

impl fmt::Debug for DurableLineHead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableLineHead")
            .field("line", &self.line)
            .field("generation", &self.generation)
            .field("head", &self.lease.session())
            .field("checkpoint", &self.head)
            .finish_non_exhaustive()
    }
}

impl DurableSessionRuntime {
    /// Reads one line's current head without a lease.
    ///
    /// # Errors
    /// Returns the typed store refusal; an unknown line is `NotFound`.
    #[doc(hidden)]
    pub async fn line(&self, line: LineId) -> Result<LineRecord, AgentError> {
        store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_line(line),
        )
        .await
        .map_err(StoreCallError::into_agent)
    }

    /// Takes the lease of `line`'s head under a fresh fence, building no
    /// driver. A lease another owner holds is refused as `LeaseHeld`.
    ///
    /// # Errors
    /// Returns typed lookup, ownership or backend failure; a head that moved
    /// between the read and the acquisition is `NotLineHead`.
    #[doc(hidden)]
    pub async fn open_line_head(
        &self,
        operation_id: &str,
        line: LineId,
    ) -> Result<DurableLineHead, AgentError> {
        crate::runtime::durable::branch::validate_session_operation_id(operation_id)?;
        let record = self.line(line).await?;
        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = capacity_store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            || {
                self.store
                    .acquire(AcquireBranch::new(record.head(), owner, self.lease_seconds))
            },
            || self.store.storage_wait_cancelled(),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        match confirm_head(&self.store, self.store_timeouts, &record, &lease).await {
            Ok(head) => Ok(DurableLineHead {
                store: Arc::clone(&self.store),
                line,
                generation: record.generation(),
                head,
                lease,
                timeouts: self.store_timeouts,
            }),
            Err(error) => {
                let _unreleased = release_store_lease(
                    Arc::clone(&self.store),
                    lease,
                    self.store_timeouts.cleanup,
                )
                .await;
                Err(error)
            }
        }
    }
}

/// Re-reads the line and the head branch under the newly held lease: a head
/// that moved between the read and the acquisition is not this line's head.
async fn confirm_head(
    store: &Arc<dyn SessionStore>,
    timeouts: crate::runtime::StoreTimeouts,
    expected: &LineRecord,
    lease: &BranchLease,
) -> Result<CheckpointId, AgentError> {
    let record = store_call(
        StorePhase::LoadBranch,
        timeouts.operation,
        store.load_line(expected.line()),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    if record.head() != lease.session() || record.generation() != expected.generation() {
        return Err(AgentError::from_store(SessionStoreError::NotLineHead));
    }
    let loaded = store_call(
        StorePhase::LoadBranch,
        timeouts.operation,
        store.load_branch(lease.session()),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    Ok(loaded.head().checkpoint().checkpoint_id())
}

impl DurableLineHead {
    /// The line.
    #[must_use]
    pub const fn line(&self) -> LineId {
        self.line
    }

    /// The head's generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The head branch's session.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.lease.session().session_id()
    }

    /// The head branch.
    #[must_use]
    pub const fn branch_id(&self) -> plakous_model::BranchId {
        self.lease.session().branch_id()
    }

    /// The head checkpoint.
    #[must_use]
    pub const fn checkpoint_id(&self) -> CheckpointId {
        self.head
    }

    /// The held lease's fence.
    #[must_use]
    pub const fn fence(&self) -> u64 {
        self.lease.fence().get()
    }

    /// Records the terminal snapshots among `jobs` that match a job row of
    /// the head still lacking terminal evidence, under the held fence.
    /// Returns the numbers newly recorded; a job already recorded keeps its
    /// first evidence, and running, unknown or unmatched snapshots record
    /// nothing.
    ///
    /// # Errors
    /// Returns `NotLineHead` once the head moved, and other typed lease or
    /// backend refusals; nothing is recorded on error.
    pub async fn record_job_terminals(&self, jobs: &[JobView]) -> Result<Vec<u32>, AgentError> {
        let stored = store_call(
            StorePhase::LoadCheckpoint,
            self.timeouts.operation,
            self.store.load_checkpoint(self.head),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let rows = stored.checkpoint().manifest().jobs().rows();
        let mut terminals = Vec::new();
        for view in jobs {
            let Some(terminal) = LineJobTerminal::from_view(view)
                .map_err(crate::runtime::continuation::runtime_manifest_error)?
            else {
                continue;
            };
            if rows.iter().any(|row| {
                row.terminal.is_none()
                    && row.number == terminal.number
                    && row.session_id == terminal.session_id
                    && row.owner_generation == terminal.owner_generation
            }) && !terminals
                .iter()
                .any(|known: &LineJobTerminal| known.number == terminal.number)
            {
                terminals.push(terminal);
            }
        }
        if terminals.is_empty() {
            return Ok(Vec::new());
        }
        let receipt = capacity_store_call(
            StorePhase::Commit,
            self.timeouts.operation,
            || {
                self.store
                    .record_line_job_terminals(RecordLineJobTerminals::new(
                        self.lease.clone(),
                        self.line,
                        self.generation,
                        self.head,
                        terminals.clone(),
                    ))
            },
            || self.store.storage_wait_cancelled(),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        Ok(receipt.recorded().to_vec())
    }

    /// Renews the held lease.
    ///
    /// # Errors
    /// Returns the typed store refusal; a refused renewal ends the hold.
    pub async fn renew(&mut self) -> Result<(), AgentError> {
        self.lease = store_call(
            StorePhase::Renew,
            self.timeouts.operation,
            self.store.renew(self.lease.clone()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        Ok(())
    }

    /// Releases the held lease.
    ///
    /// # Errors
    /// Returns the typed store refusal.
    pub async fn release(self) -> Result<(), AgentError> {
        release_store_lease(self.store, self.lease, self.timeouts.cleanup)
            .await
            .map_err(|error| {
                AgentError::new(
                    AgentErrorKind::Session,
                    format!("line head lease release failed: {error}"),
                )
            })
    }
}
