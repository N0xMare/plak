//! The source of a same-store carry (spec 01 §5.7): the current head of a
//! line, taken over by a line-continuing genesis under the head's own lease.
//!
//! The carry is fenced like a rebind. The source's lease is held from the
//! read of its ledger to the create, the create names the exact head it read
//! and the pending line job terminals it folded, and the store refuses it
//! unless all of that still holds, so the head move and the ledger it hands
//! over are one compare-and-swap. An exact retry of a published carry is
//! answered from its create receipt, by checkpoint id, like any create.

use crate::manifest::{JobLedger, LineJobTerminal};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, capacity_store_call, store_call};
use crate::runtime::durable::DurableSessionRuntime;
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::{operation_run_id, validate_session_operation_id};
use crate::session::LineContinuation;
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, BranchHead, BranchLease, SessionRef, SessionStore,
    SessionStoreError,
};
use plakous_model::LineId;
use std::fmt;
use std::sync::Arc;

/// What a carry from one source branch continues.
#[doc(hidden)]
#[derive(Debug)]
pub enum DurableCarrySource {
    /// The source is its line's head, held under its lease.
    Head(Box<DurableCarryHead>),
    /// The source is not its line's head (or its line ended): it is
    /// read-only history, so the carry starts a new line.
    Superseded,
}

/// A carry source's line head, held under its lease until the create.
#[doc(hidden)]
pub struct DurableCarryHead {
    store: Arc<dyn SessionStore>,
    timeouts: crate::runtime::StoreTimeouts,
    lease: BranchLease,
    head: BranchHead,
    line: LineId,
    from: u64,
    jobs: JobLedger,
    consumed: Vec<u32>,
}

impl fmt::Debug for DurableCarryHead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableCarryHead")
            .field("head", &self.head)
            .field("line", &self.line)
            .field("generation", &self.from)
            .field("consumed", &self.consumed)
            .finish_non_exhaustive()
    }
}

impl DurableCarryHead {
    /// Job rows of the head that have no terminal evidence: processes the
    /// carry hands to the new head, which only a `JobHost` can take over.
    #[must_use]
    pub fn running_jobs(&self) -> usize {
        self.jobs
            .rows()
            .iter()
            .filter(|row| row.terminal.is_none())
            .count()
    }

    /// Releases the held lease without carrying.
    ///
    /// # Errors
    /// Returns the typed store refusal.
    pub async fn release(self) -> Result<(), AgentError> {
        release_store_lease(self.store, self.lease, self.timeouts.cleanup).await
    }
}

impl DurableCarryHead {
    /// The target's line, generation and the ledger it takes over (rows not
    /// yet handed to the target).
    pub(in crate::runtime) fn line_and_jobs(
        &self,
    ) -> Result<(LineId, u64, &JobLedger), AgentError> {
        Ok((
            self.line,
            self.from.checked_add(1).ok_or_else(|| {
                AgentError::new(AgentErrorKind::Session, "line generations exhausted")
            })?,
            &self.jobs,
        ))
    }

    /// The create's fence over the taken-over head, and the line and
    /// generation its folded terminals were recorded under.
    pub(in crate::runtime) fn continuation(&self) -> (LineContinuation, LineId, u64) {
        (
            LineContinuation::new(self.lease.clone(), self.head, self.consumed.clone()),
            self.line,
            self.from,
        )
    }

    /// The held source lease, released once the create settles.
    pub(in crate::runtime) fn held_lease(&self) -> BranchLease {
        self.lease.clone()
    }
}

impl DurableSessionRuntime {
    /// Resolves what a carry from `source` under `operation_id` continues,
    /// taking the source head's lease when it is its line's head.
    ///
    /// # Errors
    /// Returns typed lookup, lease (`LeaseHeld` while another owner,
    /// including a lease-only holder, holds the head) or backend failure.
    #[doc(hidden)]
    pub async fn carry_source(
        &self,
        operation_id: &str,
        source: SessionRef,
    ) -> Result<DurableCarrySource, AgentError> {
        validate_session_operation_id(operation_id)?;
        let loaded = self.load(source).await?;
        let topology = loaded.checkpoint().checkpoint().manifest().topology();
        let (line, from) = (topology.line, topology.line_generation);
        if !self.is_head(source, line, from).await? {
            return Ok(DurableCarrySource::Superseded);
        }
        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = capacity_store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            || {
                self.store
                    .acquire(AcquireBranch::new(source, owner, self.lease_seconds))
            },
            || self.store.storage_wait_cancelled(),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        match self.held_head(source, line, from, &lease).await {
            Ok(Some((head, jobs, consumed))) => {
                Ok(DurableCarrySource::Head(Box::new(DurableCarryHead {
                    store: Arc::clone(&self.store),
                    timeouts: self.store_timeouts,
                    lease,
                    head,
                    line,
                    from,
                    jobs,
                    consumed,
                })))
            }
            result => {
                let _unreleased = release_store_lease(
                    Arc::clone(&self.store),
                    lease,
                    self.store_timeouts.cleanup,
                )
                .await;
                result.map(|_| DurableCarrySource::Superseded)
            }
        }
    }

    async fn load(&self, source: SessionRef) -> Result<crate::LoadedBranch, AgentError> {
        store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(source),
        )
        .await
        .map_err(StoreCallError::into_agent)
    }

    async fn is_head(
        &self,
        source: SessionRef,
        line: LineId,
        from: u64,
    ) -> Result<bool, AgentError> {
        match self.line(line).await {
            Ok(record) => Ok(record.head() == source && record.generation() == from),
            Err(error) if error.store_error() == Some(SessionStoreError::NotFound) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Re-reads the head under the held lease and folds the line's pending
    /// terminals into its ledger; `None` when the head moved meanwhile.
    async fn held_head(
        &self,
        source: SessionRef,
        line: LineId,
        from: u64,
        lease: &BranchLease,
    ) -> Result<Option<(BranchHead, JobLedger, Vec<u32>)>, AgentError> {
        let loaded = self.load(source).await?;
        if lease.session() != source || !self.is_head(source, line, from).await? {
            return Ok(None);
        }
        let terminals: Vec<LineJobTerminal> = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.line_job_terminals(line),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let checkpoint = loaded.checkpoint().checkpoint();
        let jobs = checkpoint
            .manifest()
            .jobs()
            .with_line_terminals(&terminals)
            .map_err(crate::runtime::continuation::runtime_manifest_error)?;
        // Quiescence is the store's to refuse, in the create transaction.
        Ok(Some((
            loaded.head(),
            jobs,
            terminals.iter().map(|terminal| terminal.number).collect(),
        )))
    }
}
