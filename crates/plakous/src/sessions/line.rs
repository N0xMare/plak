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

impl AgentSessions {
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

    /// Applies a create request's line choice to its builder.
    pub(super) async fn line_builder(
        &self,
        builder: AgentBuilder,
        line: Option<CreateLine>,
    ) -> Result<AgentBuilder, AgentSessionsError> {
        use plakous_agent::internal::facade_support::{
            agent_builder_continuing_line, agent_builder_with_line_origin,
        };
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
                // Any checkpoint of the source branch names its line: the
                // membership is fixed for the branch's life.
                let loaded = self
                    .store
                    .load_branch(source.session_ref())
                    .await
                    .map_err(|error| super::store_error(error).for_session(source.session_id))?;
                let topology = loaded.checkpoint().checkpoint().manifest().topology();
                let head = self.line_head(topology.line).await?;
                if head.session_id != source.session_id
                    || head.branch_id != source.branch_id
                    || head.generation != topology.line_generation
                {
                    // A superseded source has no line to continue: its carry
                    // starts a new line that names where it came from.
                    let origin = LineOrigin::imported(format!(
                        "carried from {}:{}",
                        source.session_id, source.branch_id
                    ))
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Internal,
                            "carry provenance is not single-line text",
                        )
                    })?;
                    return Ok(agent_builder_with_line_origin(builder, origin));
                }
                let generation = topology.line_generation.checked_add(1).ok_or_else(|| {
                    AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "line generations exhausted",
                    )
                })?;
                Ok(agent_builder_continuing_line(
                    builder,
                    topology.line,
                    generation,
                    loaded.checkpoint().checkpoint().manifest().jobs().clone(),
                ))
            }
        }
    }
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
