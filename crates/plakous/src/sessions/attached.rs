use super::error::runtime_error;
use super::registry::DriverRegistration;
use super::{AgentSessionsError, SessionBindingReport, SessionLocator};
use plakous_agent::Agent;
use std::fmt;

/// An attached exact session runtime.
pub struct OpenedSession {
    pub(super) replay_stripped: bool,
    agent: Agent,
    binding: SessionBindingReport,
    _registration: DriverRegistration,
}

impl OpenedSession {
    /// Whether this creation fell back to stripped history after replay validation.
    /// False on an ordinary create or reopen; the host can persist a carry notice.
    #[must_use]
    pub const fn replay_stripped(&self) -> bool {
        self.replay_stripped
    }

    pub(super) fn new(
        agent: Agent,
        binding: SessionBindingReport,
        registration: DriverRegistration,
    ) -> Self {
        Self {
            replay_stripped: false,
            agent,
            binding,
            _registration: registration,
        }
    }

    /// Returns the attached agent capability.
    #[must_use]
    pub const fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Returns the agent's current authoritative checkpoint locator.
    ///
    /// This is derived on every call because a successful durable turn advances
    /// the checkpoint. It is never a cached copy of the requested source head.
    #[must_use]
    pub fn locator(&self) -> SessionLocator {
        SessionLocator::new(
            self.agent.session_id(),
            self.agent.branch_id(),
            self.agent.checkpoint_id(),
        )
    }

    /// Unix milliseconds of the newest operator prompt or steer in the
    /// committed canonical history, read from the agent's current checkpoint
    /// without a store load. The time is the one stamped into the entry's
    /// `UUIDv7` identity; host wakes, mail and harness notes are skipped, and
    /// an entry with another identity version carries no time. `None` when no
    /// entry qualifies.
    #[must_use]
    pub fn last_operator_input_unix_ms(&self) -> Option<u64> {
        plakous_agent::internal::facade_support::last_operator_input_unix_ms(&self.agent)
    }

    /// Returns the exact historical binding view.
    #[must_use]
    pub const fn binding(&self) -> &SessionBindingReport {
        &self.binding
    }

    /// Unsupported host handoff seam: closes admission without stopping work.
    /// The owning host must follow with release or abandonment.
    #[doc(hidden)]
    pub fn close_admission(&self) {
        plakous_agent::internal::close_admission(&self.agent);
    }

    /// Joins the private driver, releases its branch lease, and flushes the
    /// selected backend. The returned locator names the exact shutdown-receipt
    /// checkpoint, which may be newer than the last ordinary turn checkpoint.
    ///
    /// Consuming this handle prevents it from being reused as live authority.
    ///
    /// # Errors
    ///
    /// Returns a typed redaction-safe runtime or backend release failure.
    pub async fn release(self) -> Result<SessionLocator, AgentSessionsError> {
        let result = self
            .agent
            .shutdown()
            .await
            .map_err(|error| runtime_error(&error));
        let locator = self.locator();
        result.map(|()| locator).map_err(|error| {
            if plakous_agent::internal::lease_released(&self.agent) {
                error.with_released_locator(locator)
            } else {
                error
            }
        })
    }

    /// Joins and releases a predecessor while preserving jobs for a same-branch open.
    /// All other owner shutdowns must use terminal `release`.
    ///
    /// # Errors
    /// Returns a typed runtime cleanup failure.
    pub async fn release_for_transfer(self) -> Result<SessionLocator, AgentSessionsError> {
        let result = plakous_agent::internal::shutdown_for_transfer(&self.agent)
            .await
            .map_err(|error| runtime_error(&error));
        let locator = self.locator();
        result.map(|()| locator).map_err(|error| {
            if plakous_agent::internal::lease_released(&self.agent) {
                error.with_released_locator(locator)
            } else {
                error
            }
        })
    }

    /// Lease-only process-exit release, batched with related runtime handles.
    /// Already dispatched work is not joined; the caller must enforce its exit bound.
    ///
    /// # Errors
    /// Returns a typed store/runtime release failure.
    pub async fn abandon_and_release(
        self,
        peers: &[Agent],
        job_budget: std::time::Duration,
    ) -> Result<SessionLocator, AgentSessionsError> {
        let mut agents = vec![self.agent.clone()];
        agents.extend_from_slice(peers);
        let result = super::abandon_and_release(&agents, job_budget).await;
        let locator = self.locator();
        result.map(|()| locator).map_err(|error| {
            if plakous_agent::internal::lease_released(&self.agent) {
                error.with_released_locator(locator)
            } else {
                error
            }
        })
    }
}

impl fmt::Debug for OpenedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenedSession")
            .field("agent", &self.agent)
            .field("locator", &self.locator())
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}
