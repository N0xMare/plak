//! Agent lines at the facade (spec 01 §5.1, §5.3): the head read, the
//! lease-only open of a head, and the line a new session joins.

use super::types::CreateLine;
use super::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, OpenedSession, SessionLocator,
    SessionOperationId, runtime_error,
};
use plakous_agent::AgentBuilder;
use plakous_agent::internal::{DurableLineHead, LineOrigin};
use plakous_model::{BranchId, LineId, SessionId};
use plakous_tools::JobView;

/// One agent line's current head: the only branch of the line that commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineHead {
    line: LineId,
    session_id: SessionId,
    branch_id: BranchId,
    generation: u64,
}

impl LineHead {
    /// The line.
    #[must_use]
    pub const fn line(&self) -> LineId {
        self.line
    }

    /// The head branch's runtime session.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The head branch.
    #[must_use]
    pub const fn branch_id(&self) -> BranchId {
        self.branch_id
    }

    /// The head's generation: 1 for the branch that started the line, one
    /// more for every rebind or carry since.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// The lease of one line's head, held without a driver, route, catalog or
/// credential.
///
/// It keeps every other owner off the head while a full open is not possible,
/// and it is the only writer of the line's job terminal table: terminals it
/// records are folded into the head's job ledger, and removed from the
/// table, by the head's next commit once it opens. The holder renews the
/// lease (30 s) and releases it before the full open.
#[derive(Debug)]
pub struct LeaseOnlyHead {
    inner: DurableLineHead,
}

impl LeaseOnlyHead {
    /// The line.
    #[must_use]
    pub const fn line(&self) -> LineId {
        self.inner.line()
    }

    /// The held head's generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.inner.generation()
    }

    /// The held head's exact locator.
    #[must_use]
    pub fn locator(&self) -> SessionLocator {
        SessionLocator::new(
            self.inner.session_id(),
            self.inner.branch_id(),
            self.inner.checkpoint_id(),
        )
    }

    /// The held lease's fence.
    #[must_use]
    pub const fn fence(&self) -> u64 {
        self.inner.fence()
    }

    /// Records the terminal snapshots among `jobs` (exited, stopped or
    /// released) that match one of the head's job rows still lacking terminal
    /// evidence. Returns the job numbers newly recorded; a job already
    /// recorded keeps its first evidence.
    ///
    /// # Errors
    /// [`AgentSessionsErrorKind::NotLineHead`] once the line's head moved,
    /// and other typed lease or backend refusals; nothing is recorded on
    /// error.
    pub async fn record_job_terminals(
        &self,
        jobs: &[JobView],
    ) -> Result<Vec<u32>, AgentSessionsError> {
        self.inner
            .record_job_terminals(jobs)
            .await
            .map_err(|error| runtime_error(&error))
    }

    /// Renews the held lease.
    ///
    /// # Errors
    /// Returns the typed refusal; a refused renewal ends the hold.
    pub async fn renew(&mut self) -> Result<(), AgentSessionsError> {
        self.inner
            .renew()
            .await
            .map_err(|error| runtime_error(&error))
    }

    /// Releases the held lease, so the head can open fully.
    ///
    /// # Errors
    /// Returns the typed store refusal.
    pub async fn release(self) -> Result<(), AgentSessionsError> {
        self.inner
            .release()
            .await
            .map_err(|error| runtime_error(&error))
    }
}

/// One branch's lease, held without a driver, route or credential, whether
/// or not the branch is its line's head. While it is held no other owner
/// opens or commits on the branch, and no rebind or carry moves its line,
/// since both take the source branch's lease.
#[derive(Debug)]
pub struct BranchHold {
    inner: plakous_agent::internal::DurableBranchHold,
}

impl BranchHold {
    /// Renews the held branch lease without opening a driver.
    ///
    /// # Errors
    /// Returns the typed refusal; a refused renewal ends the hold.
    pub async fn renew(&mut self) -> Result<(), AgentSessionsError> {
        self.inner
            .renew()
            .await
            .map_err(|error| runtime_error(&error))
    }

    /// Releases the held lease.
    ///
    /// # Errors
    /// Returns the typed store refusal.
    pub async fn release(self) -> Result<(), AgentSessionsError> {
        self.inner
            .release()
            .await
            .map_err(|error| runtime_error(&error))
    }
}

impl AgentSessions {
    /// Takes `locator`'s branch lease without building a driver, for work
    /// that must not race the branch's owner, such as settling an interrupted
    /// carry from it.
    ///
    /// # Errors
    /// [`AgentSessionsErrorKind::Ownership`] while another owner holds the
    /// branch (it is released within [`Self::lease_period_seconds`] of its
    /// owner's exit), and other typed lookup or backend failures.
    pub async fn hold_branch(
        &self,
        operation_id: &SessionOperationId,
        locator: SessionLocator,
    ) -> Result<BranchHold, AgentSessionsError> {
        self.runtime
            .hold_branch(operation_id.as_str(), locator.session_ref())
            .await
            .map(|inner| BranchHold { inner })
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// The lease duration this store's owners hold and renew: the longest a
    /// lease of an exited owner can outlive it.
    #[must_use]
    pub fn lease_period_seconds(&self) -> u64 {
        self.runtime.lease_seconds().get()
    }

    /// Reads one agent line's current head without a lease.
    ///
    /// # Errors
    /// An unknown line, or one whose head session was deleted, is
    /// [`AgentSessionsErrorKind::NotFound`].
    pub async fn line_head(&self, line: LineId) -> Result<LineHead, AgentSessionsError> {
        self.runtime
            .line(line)
            .await
            .map(|record| LineHead {
                line,
                session_id: record.head().session_id(),
                branch_id: record.head().branch_id(),
                generation: record.generation(),
            })
            .map_err(|error| runtime_error(&error))
    }

    /// Reads the current head of the agent line `locator`'s branch belongs
    /// to: the branch itself while it is the head, otherwise where a rebind
    /// or carry moved the line.
    ///
    /// # Errors
    /// [`AgentSessionsErrorKind::LineEnded`] once the session holding the
    /// line's head was deleted, and typed lookup or backend failures.
    pub async fn line_head_of(
        &self,
        locator: SessionLocator,
    ) -> Result<LineHead, AgentSessionsError> {
        let line = self
            .store
            .branch_line(locator.session_ref())
            .await
            .map_err(|error| super::store_error(error).for_session(locator.session_id))?;
        match self.line_head(line).await {
            Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => {
                Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::LineEnded,
                    "the branch's agent line ended when the session holding its head was deleted",
                ))
            }
            head => head,
        }
    }

    /// The provenance text a line was started with when it was imported or
    /// carried from a branch that was already superseded; `None` for a line
    /// a new session or a fork started.
    ///
    /// # Errors
    /// An unknown or ended line is [`AgentSessionsErrorKind::NotFound`].
    pub async fn line_provenance(
        &self,
        line: LineId,
    ) -> Result<Option<String>, AgentSessionsError> {
        self.runtime
            .line(line)
            .await
            .map(|record| match record.origin() {
                LineOrigin::Imported { provenance } => Some(provenance.clone()),
                _ => None,
            })
            .map_err(|error| runtime_error(&error))
    }

    /// Takes the lease and a fresh fence of `line`'s head without building a
    /// driver, so no route, catalog or credential is needed.
    ///
    /// # Errors
    /// [`AgentSessionsErrorKind::Ownership`] while another owner holds the
    /// head, [`AgentSessionsErrorKind::NotLineHead`] when the head moved
    /// while it was taken, and other typed lookup or backend failures.
    pub async fn open_lease_only(
        &self,
        operation_id: &SessionOperationId,
        line: LineId,
    ) -> Result<LeaseOnlyHead, AgentSessionsError> {
        self.runtime
            .open_line_head(operation_id.as_str(), line)
            .await
            .map(|inner| LeaseOnlyHead { inner })
            .map_err(|error| runtime_error(&error))
    }

    /// Reads a `NotLineHead` refusal of a branch on `line` as
    /// [`AgentSessionsErrorKind::LineEnded`] when the line has no head any
    /// more: the session holding it was deleted (spec 01 §5.9).
    pub(super) async fn line_refusal(
        &self,
        line: LineId,
        error: AgentSessionsError,
    ) -> AgentSessionsError {
        if error.kind() != AgentSessionsErrorKind::NotLineHead || !self.line_ended(line).await {
            return error;
        }
        AgentSessionsError::new(
            AgentSessionsErrorKind::LineEnded,
            "the branch's agent line ended when the session holding its head was deleted; it is read-only history: view, export or fork it",
        )
    }

    /// Whether `line` ended: its head's session was deleted.
    pub(super) async fn line_ended(&self, line: LineId) -> bool {
        use plakous_agent::internal::facade_support::{SessionStoreOrigin, agent_store_origin};
        self.runtime
            .line(line)
            .await
            .is_err_and(|error| agent_store_origin(&error) == Some(SessionStoreOrigin::NotFound))
    }

    /// Applies a create request's line choice to its builder.
    pub(super) async fn line_builder(
        &self,
        operation_id: &SessionOperationId,
        builder: AgentBuilder,
        line: Option<CreateLine>,
    ) -> Result<AgentBuilder, AgentSessionsError> {
        use plakous_agent::internal::facade_support::agent_builder_with_line_origin;
        match line {
            None => Ok(builder),
            Some(CreateLine::Imported(provenance)) => {
                let origin = LineOrigin::imported(provenance).ok_or_else(|| {
                    AgentSessionsError::new(
                        AgentSessionsErrorKind::InvalidInput,
                        "line provenance must be 1 to 256 bytes on one line",
                    )
                })?;
                Ok(agent_builder_with_line_origin(builder, origin))
            }
            Some(CreateLine::Continue(source)) => {
                self.carry_builder(operation_id, builder, source).await
            }
        }
    }

    /// Resolves a same-store carry from `source`: the next head of its line
    /// under the head's held lease, or a new line when `source` is read-only
    /// history. An exact retry of a published carry is answered from its
    /// create receipt.
    async fn carry_builder(
        &self,
        operation_id: &SessionOperationId,
        builder: AgentBuilder,
        source: SessionLocator,
    ) -> Result<AgentBuilder, AgentSessionsError> {
        use plakous_agent::internal::DurableCarrySource;
        use plakous_agent::internal::facade_support::{
            agent_builder_carrying_head, agent_builder_has_job_host, agent_builder_with_line_origin,
        };
        let carried = Box::pin(
            self.runtime
                .carry_source(operation_id.as_str(), source.session_ref()),
        )
        .await
        .map_err(|error| runtime_error(&error).for_session(source.session_id))?;
        match carried {
            DurableCarrySource::Head(head) => {
                // The store refuses a head that is not quiescent, in the
                // create's own transaction.
                if head.running_jobs() != 0 && !agent_builder_has_job_host(&builder) {
                    let _unreleased = head.release().await;
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::UnsettledProcesses,
                        "the carry source has running jobs and the new binding installs no JobHost to take them over; install one (for example plakous::tools::LocalJobHost) or stop the jobs first",
                    ));
                }
                Ok(agent_builder_carrying_head(builder, head))
            }
            DurableCarrySource::Superseded => {
                // A superseded source has no line to continue: its carry
                // starts a new line that names where it came from.
                let origin =
                    LineOrigin::imported(carried_from_provenance(source)).ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Internal,
                            "carry provenance is not single-line text",
                        )
                    })?;
                Ok(agent_builder_with_line_origin(builder, origin))
            }
        }
    }
}

/// The provenance a carry from an already superseded `source` records on the
/// new line it starts.
#[must_use]
pub fn carried_from_provenance(source: SessionLocator) -> String {
    format!("carried from {}:{}", source.session_id, source.branch_id)
}

impl OpenedSession {
    /// The agent line this session's branch belongs to.
    #[must_use]
    pub fn line(&self) -> LineId {
        plakous_agent::internal::facade_support::agent_checkpoint(self.agent())
            .manifest()
            .topology()
            .line
    }
}
