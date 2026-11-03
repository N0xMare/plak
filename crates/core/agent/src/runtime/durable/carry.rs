//! The source of a same-store carry (spec 01 §5.7): the current head of a
//! line, taken over by a line-continuing genesis under the head's own lease.
//!
//! The carry is fenced like a rebind. The source's lease is held from the
//! read of its ledger to the create, the create names the exact head it read
//! and the pending line job terminals it folded, and the store refuses it
//! unless all of that still holds, so the head move and the ledger it hands
//! over are one compare-and-swap.

use crate::manifest::{JobLedger, LineJobTerminal};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, capacity_store_call, store_call};
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::branch::{operation_run_id, validate_session_operation_id};
use crate::runtime::durable::{DurableGenesisIdentities, DurableSessionRuntime};
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
    /// This operation's carry already published: its genesis is rebuilt
    /// exactly from the published one.
    Published(DurableCarryReplay),
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

/// The published genesis of an exact carry replay.
#[doc(hidden)]
#[derive(Debug)]
pub struct DurableCarryReplay {
    line: LineId,
    generation: u64,
    jobs: JobLedger,
}

/// The line a durable genesis joins, resolved before its create.
pub(in crate::runtime) enum LineCarry {
    Head(Box<DurableCarryHead>),
    Replay(DurableCarryReplay),
}

impl LineCarry {
    /// The target's line, generation and the ledger it takes over (rows not
    /// yet handed to the target).
    pub(in crate::runtime) fn line_and_jobs(
        &self,
    ) -> Result<(LineId, u64, &JobLedger), AgentError> {
        match self {
            Self::Head(head) => Ok((
                head.line,
                head.from.checked_add(1).ok_or_else(|| {
                    AgentError::new(AgentErrorKind::Session, "line generations exhausted")
                })?,
                &head.jobs,
            )),
            Self::Replay(replay) => Ok((replay.line, replay.generation, &replay.jobs)),
        }
    }

    /// The create's fence over the taken-over head, and the line and
    /// generation its folded terminals were recorded under.
    pub(in crate::runtime) fn continuation(&self) -> Option<(LineContinuation, LineId, u64)> {
        match self {
            Self::Head(head) => Some((
                LineContinuation::new(head.lease.clone(), head.head, head.consumed.clone()),
                head.line,
                head.from,
            )),
            Self::Replay(_) => None,
        }
    }

    /// The held source lease, released once the create settles.
    pub(in crate::runtime) fn held_lease(&self) -> Option<BranchLease> {
        match self {
            Self::Head(head) => Some(head.lease.clone()),
            Self::Replay(_) => None,
        }
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
        if let Some(replay) = self.published_carry(operation_id).await? {
            return Ok(DurableCarrySource::Published(replay));
        }
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

    /// The genesis this operation already created, when it did.
    async fn published_carry(
        &self,
        operation_id: &str,
    ) -> Result<Option<DurableCarryReplay>, AgentError> {
        let identities = DurableGenesisIdentities::new(operation_id)?;
        match store_call(
            StorePhase::LoadCheckpoint,
            self.store_timeouts.operation,
            self.store.load_checkpoint(identities.checkpoint_id),
        )
        .await
        {
            Ok(stored) => {
                let manifest = stored.checkpoint().manifest();
                let topology = manifest.topology();
                if topology.session_id != identities.session_id
                    || topology.branch_id != identities.branch_id
                {
                    return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
                }
                Ok(Some(DurableCarryReplay {
                    line: topology.line,
                    generation: topology.line_generation,
                    jobs: manifest.jobs().clone(),
                }))
            }
            Err(error) if error.is(SessionStoreError::NotFound) => Ok(None),
            Err(error) => Err(error.into_agent()),
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
