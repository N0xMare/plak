//! High-level durable agent-session workflows.
//!
//! Consumers operate on exact [`SessionLocator`](crate::sessions::SessionLocator) values and typed reports. The
//! semantic store, leases, checkpoint object graphs, and backend representation
//! remain private to this service.
//!
//! Low-level persistence is deliberately absent from this module:
//!
//! ```compile_fail
//! use plakous::sessions::SessionStore;
//! ```
//!
//! The private driver locator also remains outside the facade:
//!
//! ```compile_fail
//! use plakous::AgentSessionEndpoint;
//! ```
//!
//! Fork and rebind publication is read from the change feed, not a separate
//! cross-store receipt reconciliation API:
//!
//! ```compile_fail
//! use plakous::sessions::ForkReceiptReconciliation;
//! ```
//!
//! ```compile_fail
//! use plakous::sessions::ForkReceiptReconciliationRequest;
//! ```
//!
//! ```compile_fail
//! use plakous::sessions::RebindReceiptReconciliation;
//! ```
//!
//! ```compile_fail
//! use plakous::sessions::RebindReceiptReconciliationRequest;
//! ```
//!
//! ```compile_fail
//! let _ = plakous::sessions::AgentSessions::reconcile_fork_receipt;
//! ```
//!
//! ```compile_fail
//! let _ = plakous::sessions::AgentSessions::reconcile_rebind_receipt;
//! ```
//!
//! ```compile_fail
//! let _ = plakous::sessions::OpenedSession::parent_locator;
//! ```

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::Arc;

#[cfg(test)]
use plakous_agent::CommandId;
use plakous_agent::internal::facade_support::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason,
    AgentSessionEndpointError, CompactionSummary, ModelRouteBinding, RuntimeCheckpoint,
    SessionStoreOrigin, agent_builder_target_authority, agent_checkpoint, agent_store_origin,
    checkpoint_instruction_sources, checkpoint_job_config, checkpoint_registers_tool,
};
use plakous_agent::internal::{
    ArchiveSession, DeleteSession, DurableCodeCellAbandonmentOutcome,
    DurableCodeCellAbandonmentRequest, DurableCompactionReceipt, DurableCompactionRecoveryOutcome,
    DurableEffectEvidenceRecoveryOutcome, DurableEffectEvidenceRecoveryRequest,
    DurableRuntimeBranchOutcome, DurableRuntimeOpen, DurableSessionAddress, DurableSessionRuntime,
    LoadedBranch, RecoveryAction, RecoveryObligation, RuntimeLifecycle, RuntimeWorkingRoot,
    SessionRef, SessionStore, WorkingRecoveryPhase,
};
use plakous_agent::{
    Agent, AgentBuilder, AgentError, AgentErrorKind, RecoveredTurnHandle, UserInput,
};
use plakous_model::{
    Actor, BranchId, CallId, CheckpointId, ContextStatus, Digest, EffectiveProviderConfigurationId,
    EntryId, ModelAdapterProfileId, ModelFamilyId, ModelId, OutputBoundDisposition, ProtocolId,
    Provenance, RouteId, SelectedProviderConfigurationId, SessionId, Timestamp, ToolName, TurnId,
};
use plakous_tools::{ToolExecutorId, ToolOutcome};

/// Schema version of the SQLite runtime store this build reads and writes.
/// A product keys its state directory by it and by [`RUNTIME_FORMAT_EPOCH`],
/// so a build never opens a store another format wrote.
#[cfg(feature = "sessions-sqlite")]
pub const RUNTIME_STORE_SCHEMA_VERSION: i64 = plakous_store::SQLITE_SCHEMA_VERSION;
/// Format epoch of the persisted runtime checkpoint graph. Any change to a
/// persisted runtime encoding bumps it; nothing older decodes.
pub const RUNTIME_FORMAT_EPOCH: u32 = plakous_agent::internal::RUNTIME_FORMAT_EPOCH;

mod attached;

/// Who holds a session's runtime branch, read from the SQLite store's holder
/// records.
#[cfg(feature = "sessions-sqlite")]
pub mod holder;

/// Repository-only read-only reload diagnostics; not supported public API.
#[cfg(feature = "sessions-sqlite")]
#[doc(hidden)]
pub mod rehearsal;

#[cfg(feature = "sessions-sqlite")]
fn require_export_job_terminals(may_be_running: bool) -> Result<(), AgentSessionsError> {
    if may_be_running {
        return Err(AgentSessionsError::new(
            AgentSessionsErrorKind::UnsettledProcesses,
            "background process outcomes are not settled; resume the session and send a prompt to deliver their notices before exporting",
        ));
    }
    Ok(())
}
/// Abandons supplied runtime handles and releases their leases in one transaction
/// per backend, without driver joins, shutdown receipts, or WAL checkpoints.
///
/// # Errors
/// Returns a runtime/store failure; the caller's process-exit deadline still applies.
pub async fn abandon_and_release(
    agents: &[Agent],
    job_budget: std::time::Duration,
) -> Result<(), AgentSessionsError> {
    plakous_agent::internal::abandon_and_release_many(agents, job_budget)
        .await
        .map_err(|error| runtime_error(&error))
}
mod backend;
pub mod feed;
pub use backend::{SessionBackend, SessionRetention, SessionStoreBound, SessionStoreBoundUsage};
mod call_detail;
#[cfg(feature = "sessions-sqlite")]
pub use backend::{SessionPersistenceShape, SessionStoreUsage};
pub use call_detail::{
    MAX_CALL_DETAIL_SIDE_BYTES, SessionCallDetail, SessionCallDetailOutcome, SessionCallDetailText,
};
mod error;
mod portable;
pub use portable::PortableSessionHistory;
mod read;
mod transcript;
pub use transcript::{
    SessionTranscriptItem, SessionTranscriptPart, SessionTranscriptSource, SessionTranscriptTail,
};
mod line;
pub use line::{BranchHold, LeaseOnlyHead, LineHead, carried_from_provenance};
mod presentation;
mod registry;
#[doc(hidden)]
pub use presentation::FrozenSessionPresentation;
mod types;
pub use attached::OpenedSession;
pub use error::{AgentSessionsError, SessionStoreAdmission, SessionStorePressure};
use error::{runtime_error, store_error};
use read::{committed_compaction_facts, history_page_preflight, project_history_page};
use registry::{DriverRegistry, SessionBranchKey};
pub use types::{
    AbandonCodeCellOutcome, AbandonCodeCellRequest, AbandonCompactionOutcome,
    AbandonCompactionRequest, AgentSessionsErrorKind, ArchiveSessionReport, ArchivedSessionReport,
    ArtifactPortability, CompactReason, CompactSessionOutcome, CompactSessionRequest, ContextQuery,
    ContextStatusRequest, CreateSessionOutcome, CreateSessionRequest, DeleteSessionReport,
    ForkSessionOutcome, ForkSessionRequest, OpenSessionOutcome, OpenSessionRequest,
    PruneHistoryOutcome, PruneHistoryReport, RebindRefused, RebindSessionOutcome,
    RebindSessionRequest, RebindToolTransition, ReclaimReport, RecoverEffectEvidenceOutcome,
    RecoverEffectEvidenceRequest, RecoverSessionOutcome, RecoverSessionRequest,
    RecoveryObligationReport, RecoveryRequiredReport, SESSION_HISTORY_VISIBILITY_REVISION,
    SessionBindingReport, SessionContextStatus, SessionContextStatusKind, SessionEffectEvidence,
    SessionHistoryCompaction, SessionHistoryCompactionPass, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryItem, SessionHistoryPageOutcome,
    SessionHistoryPageRequest, SessionHistoryResnapshot, SessionHistoryToolBlock,
    SessionHistoryVisibility, SessionLocator, SessionOperationId, SessionRecoveryAction,
    SessionRecoveryPhase, SessionTargetBinding, SessionWorkflowUnavailable,
};

const fn runtime_can_restore_phase(phase: SessionRecoveryPhase) -> bool {
    matches!(
        phase,
        SessionRecoveryPhase::EffectDecision | SessionRecoveryPhase::HostedContinuation
    )
}

const MAX_OPERATION_ID_BYTES: usize = 128;
const SESSION_LEASE_SECONDS: NonZeroU64 = match NonZeroU64::new(30) {
    Some(seconds) => seconds,
    None => NonZeroU64::MIN,
};

/// The facade lease. A `test-hooks` build may shorten it with
/// `PLAK_TEST_SESSION_LEASE_SECONDS` (a whole second from 1 through 30).
/// Other builds ignore the variable. The stored duration is whole seconds.
fn session_lease_seconds() -> NonZeroU64 {
    if let Some(seconds) = lease_seconds_override(
        crate::test_session_lease::session_lease_seconds_override().as_deref(),
    ) {
        return seconds;
    }
    SESSION_LEASE_SECONDS
}

/// Accepts `1..=30`. Anything else, including absence, keeps the production lease.
fn lease_seconds_override(value: Option<&str>) -> Option<NonZeroU64> {
    let seconds = value?.trim().parse::<u64>().ok()?;
    if (1..=SESSION_LEASE_SECONDS.get()).contains(&seconds) {
        NonZeroU64::new(seconds)
    } else {
        None
    }
}

/// Exact current runtime construction request.
///
/// The builder is never allowed to replace persisted history, plan, model, or
/// tool defaults. It is not external-authority evidence; the private runtime
/// must compare it with the historical manifest and a separate Plakous-owned
/// fresh admission before ordinary execution can start.
pub struct SessionRuntimeBinding {
    builder: AgentBuilder,
}

/// Advisory destination capacity, never request admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInputWindow {
    /// Exact destination route.
    pub route: String,
    /// Reported total window, if known.
    pub total_window: Option<u64>,
    /// Input ceiling after the configured output reservation, if known.
    pub admitted_input: Option<u64>,
}

impl SessionRuntimeBinding {
    /// Shares the host's request slots across opens and runtime refreshes.
    #[doc(hidden)]
    #[must_use]
    pub fn with_request_slots(mut self, slots: crate::agent::RequestSlots) -> Self {
        self.builder = self.builder.request_slots(slots);
        self
    }

    /// Installs this node's explicit runner backend and executor. No other
    /// session's execution authority changes.
    #[doc(hidden)]
    #[must_use]
    pub fn with_runner_services(
        mut self,
        services: plakous_tools::internal::runner::RunnerServices,
    ) -> Self {
        self.builder = plakous_agent::internal::select_runner_services(self.builder, services);
        self
    }
    /// Selects the host's home-local process transport, not a binding fact.
    #[doc(hidden)]
    #[must_use]
    pub fn with_local_daemon(
        mut self,
        backend: plakous_tools::internal::runner::LocalDaemonBackend,
    ) -> Self {
        self.builder = plakous_agent::internal::select_local_daemon(self.builder, backend);
        self
    }
    /// The configured route's total window (when reported) and input ceiling,
    /// after this builder's shared-window output reservation. This advisory
    /// snapshot is not request admission or a token measurement.
    #[must_use]
    pub fn input_window(&self) -> SessionInputWindow {
        let capacity =
            plakous_agent::internal::facade_support::agent_builder_input_window(&self.builder);
        SessionInputWindow {
            route: self.target_binding().route().as_str().to_owned(),
            total_window: capacity.and_then(|c| c.0),
            admitted_input: capacity.map(|c| c.1),
        }
    }

    /// Shares a host process registry across runtime opens and same-branch refreshes.
    #[must_use]
    pub fn with_job_host(mut self, host: Arc<dyn plakous_tools::JobHost>) -> Self {
        self.builder = self.builder.job_host(host);
        self
    }

    /// Binds an exact builder for sealed Plakous-owned revalidation.
    ///
    /// A caller-supplied revalidation witness is intentionally not accepted:
    /// copying facts from a manifest cannot prove fresh auth, workspace, tool,
    /// environment, or policy authority.
    ///
    /// Runner authority is supplied explicitly through node services; this
    /// constructor never consults a process-global selector.
    #[must_use]
    pub fn new(builder: AgentBuilder) -> Self {
        Self { builder }
    }

    /// Copies the source counter into a cleared session without pending notices.
    ///
    /// # Errors
    /// Reports invalid runtime construction inputs.
    pub fn with_job_counter(self, source: &Agent) -> Result<Self, AgentError> {
        let next_id = agent_checkpoint(source).manifest().jobs().next_id();
        Ok(Self::new(self.builder.job_counter_seed(next_id)?))
    }

    /// Derives the complete explicit rebind target represented by this runtime.
    #[must_use]
    pub fn target_binding(&self) -> SessionTargetBinding {
        let (protocol, route, model, selected, effective, account) =
            agent_builder_target_authority(&self.builder);
        SessionTargetBinding::new(protocol, route, model, selected, effective, account)
    }

    /// Narrows generation limits to the harness compaction bound derived from
    /// this runtime's own output bound and reasoning, which stay unchanged.
    ///
    /// # Errors
    ///
    /// Returns invalid input when the compact output bound is invalid.
    pub fn with_compaction_output_limit(self) -> Result<Self, plakous_model::RequestError> {
        Ok(Self::new(self.builder.with_compaction_generation_limits()?))
    }
}

/// Gives this agent its own tool scheduler instead of the process-wide one.
/// Use when one process runs several concurrent sessions.
///
/// # Errors
///
/// Returns invalid input when the scheduler or runtime budget is invalid.
pub fn isolated_tool_scheduler(builder: AgentBuilder) -> Result<AgentBuilder, AgentSessionsError> {
    use plakous_agent::internal::{ToolRuntimeConfig, ToolScheduler};
    let scheduler = ToolScheduler::new(64).map_err(|error| {
        AgentSessionsError::new(
            AgentSessionsErrorKind::InvalidInput,
            error.message().as_str(),
        )
    })?;
    Ok(builder.tool_scheduler(scheduler, ToolRuntimeConfig::default()))
}

impl fmt::Debug for SessionRuntimeBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionRuntimeBinding")
            .field("builder", &self.builder)
            .finish()
    }
}

/// One high-level application service for durable agent sessions.
///
/// The backend is intentionally erased and private. Consumers never acquire a
/// lease, issue a compare-and-swap commit, inspect object rows, or manage SQL.
/// [`SessionBackend`] is the only construction seam and keeps those mechanisms
/// crate-owned.
pub struct AgentSessions {
    store: Arc<dyn SessionStore>,
    /// The backend's store before this handle's feed wrapper.
    backend: Arc<dyn SessionStore>,
    runtime: DurableSessionRuntime,
    drivers: Arc<DriverRegistry>,
    feed: Arc<feed::FeedNotifier>,
    interrupt: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    subordinate: bool,
}

mod branch;
mod compaction;
mod maintenance;
mod open;
pub use open::OpenVerification;
mod reads;
mod recovery;

impl AgentSessions {
    /// Composes the high-level workflows over one opaque semantic backend.
    ///
    /// # Errors
    ///
    /// Returns a typed failure if the private runtime cannot be composed.
    pub fn new(backend: SessionBackend) -> Result<Self, AgentSessionsError> {
        let (backend, durability, drivers) = backend.into_parts();
        let feed = Arc::new(feed::FeedNotifier::default());
        let store =
            feed::NotifyingStore::wrap(Arc::clone(&backend), Arc::clone(&feed), None, false);
        let runtime =
            DurableSessionRuntime::current(Arc::clone(&store), durability, session_lease_seconds())
                .map_err(|error| runtime_error(&error))?;
        Ok(Self {
            store,
            backend,
            runtime,
            drivers,
            feed,
            interrupt: None,
            subordinate: false,
        })
    }

    /// This handle with `interrupt` and `subordinate` over the same backend,
    /// drivers and change feed.
    fn view(
        &self,
        interrupt: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
        subordinate: bool,
    ) -> Self {
        let store = feed::NotifyingStore::wrap(
            Arc::clone(&self.backend),
            Arc::clone(&self.feed),
            interrupt.clone(),
            subordinate,
        );
        Self {
            runtime: self.runtime.with_store(Arc::clone(&store)),
            store,
            backend: Arc::clone(&self.backend),
            drivers: Arc::clone(&self.drivers),
            feed: Arc::clone(&self.feed),
            interrupt,
            subordinate,
        }
    }

    /// This service as one host handle uses it: a store wait for capacity
    /// that a workflow or agent started through the returned handle stops
    /// once `interrupt` reports true. The probe is read-only and never
    /// authorizes a write. Every handle shares the backend, its drivers and
    /// its change feed; each keeps its own interrupt. An agent's own
    /// interrupt is bound on the agent.
    #[must_use]
    pub fn with_storage_interrupt(&self, interrupt: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.view(Some(interrupt), self.subordinate)
    }

    /// Reads the backend's aggregate retention usage, one reading per bound
    /// (checkpoints, objects, object bytes), against the limits it was opened
    /// with and the store's own class thresholds, whatever this handle's
    /// class. A SQLite backend answers from the snapshot its last write
    /// published, so another process's writes show after this process's next
    /// store operation.
    ///
    /// # Errors
    /// Returns a typed backend failure.
    pub async fn store_usage(&self) -> Result<[SessionStoreBoundUsage; 3], AgentSessionsError> {
        self.backend
            .usage()
            .await
            .map(SessionStoreBoundUsage::readings)
            .map_err(store_error)
    }

    /// This service for sessions that must stop before the others: an
    /// ordinary write (one that begins new work: a create, fork, root or
    /// prompt receipt, a prepare or first dispatch fence) through the returned
    /// handle is refused, definitely and with nothing written, once any
    /// aggregate bound of limit `L` reaches one reserve below the ordinary
    /// threshold, `L - 2 * ceil(L / 8)`. The refusal is the
    /// ordinary-class [`SessionStorePressure`] a full store gives, held to that
    /// lower mark, so a turn running there ends exactly as at the ordinary
    /// threshold. Settlement and recovery writes are unchanged. Handles
    /// without the class keep the last reserve before the ordinary threshold
    /// for their own new work. Every handle shares the backend, its drivers
    /// and its change feed, and this one keeps this handle's interrupt.
    #[must_use]
    pub fn with_subordinate_admission(&self) -> Self {
        self.view(self.interrupt.clone(), true)
    }

    async fn attach(
        &self,
        operation_id: SessionOperationId,
        agent: Agent,
    ) -> Result<OpenedSession, AgentSessionsError> {
        let binding = SessionBindingReport::from_checkpoint(&agent_checkpoint(&agent));
        let registration = match self.drivers.register(operation_id, &agent) {
            Ok(registration) => registration,
            Err(error) => {
                agent
                    .shutdown()
                    .await
                    .map_err(|shutdown| runtime_error(&shutdown))?;
                return Err(error);
            }
        };
        Ok(OpenedSession::new(agent, binding, registration))
    }

    async fn load_exact(
        &self,
        requested: SessionLocator,
    ) -> Result<ExactSource, AgentSessionsError> {
        let loaded = self
            .store
            .load_branch(requested.session_ref())
            .await
            .map_err(|error| store_error(error).for_session(requested.session_id))?;
        ExactSource::validate(requested, &loaded)
    }

    async fn load_recovery_required(
        &self,
        locator: SessionLocator,
    ) -> Result<RecoveryRequiredReport, AgentSessionsError> {
        self.load_exact(locator)
            .await?
            .recovery_report()
            .ok_or_else(|| {
                AgentSessionsError::new(
                    AgentSessionsErrorKind::Integrity,
                    "runtime recovery receipt did not name a working checkpoint",
                )
            })
    }

    async fn map_branch_runtime_error(
        &self,
        source: SessionLocator,
        error: &AgentError,
    ) -> AgentSessionsError {
        if ((error.kind() == AgentErrorKind::Session && agent_store_origin(error).is_none())
            || matches!(
                agent_store_origin(error),
                Some(SessionStoreOrigin::NotFound | SessionStoreOrigin::Pruned)
            ))
            && let Err(stale) = self.load_exact(source).await
        {
            return stale;
        }
        runtime_error(error).for_session(source.session_id)
    }
}

fn compaction_pending_user_input(
    reason: CompactReason,
    qualification: ContextQuery,
) -> Result<Option<UserInput>, AgentSessionsError> {
    match qualification {
        ContextQuery::Current => Ok(None),
        ContextQuery::WithPendingUserInput(input) if reason == CompactReason::Scheduled => {
            Ok(Some(input))
        }
        ContextQuery::WithPendingUserInput(_) => Err(AgentSessionsError::new(
            AgentSessionsErrorKind::InvalidInput,
            "pending-input compaction qualification requires the scheduled reason",
        )),
    }
}

fn validate_compaction_operation_id(
    operation_id: &SessionOperationId,
) -> Result<(), AgentSessionsError> {
    if operation_id.as_str().ends_with(":placeholders") {
        return Err(AgentSessionsError::new(
            AgentSessionsErrorKind::InvalidInput,
            "compaction operation ID suffix :placeholders is reserved",
        ));
    }
    Ok(())
}

fn validate_compaction_source(
    expected: SessionLocator,
    source: DurableSessionAddress,
) -> Result<SessionLocator, AgentSessionsError> {
    let source = SessionLocator::from_durable(source);
    if source != expected {
        return Err(AgentSessionsError::new(
            AgentSessionsErrorKind::Integrity,
            "runtime compaction receipt disagrees with its exact source",
        ));
    }
    Ok(source)
}

fn validate_compaction_publication(
    expected: SessionLocator,
    source: DurableSessionAddress,
    target: DurableSessionAddress,
) -> Result<(SessionLocator, SessionLocator), AgentSessionsError> {
    let source = validate_compaction_source(expected, source)?;
    let target = SessionLocator::from_durable(target);
    if (target.session_id, target.branch_id) != (expected.session_id, expected.branch_id) {
        return Err(AgentSessionsError::new(
            AgentSessionsErrorKind::Integrity,
            "runtime compaction receipt crossed its bound session branch",
        ));
    }
    Ok((source, target))
}

impl fmt::Debug for AgentSessions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSessions")
            .field("store", &"<semantic session store>")
            .finish_non_exhaustive()
    }
}

struct ExactSource {
    locator: SessionLocator,
    session_revision: plakous_agent::internal::SessionRevision,
    checkpoint: Arc<RuntimeCheckpoint>,
}

impl ExactSource {
    fn validate(
        requested: SessionLocator,
        loaded: &LoadedBranch,
    ) -> Result<Self, AgentSessionsError> {
        let session = loaded.session();
        let head = loaded.head();
        let reference = loaded.checkpoint().reference();
        let checkpoint = Arc::clone(loaded.checkpoint().checkpoint());
        let current = SessionLocator::new(
            head.session().session_id(),
            head.session().branch_id(),
            reference.checkpoint_id(),
        );
        if current != requested {
            return Err(AgentSessionsError::stale(current));
        }
        let manifest = checkpoint.manifest();
        let topology = manifest.topology();
        let bundle = loaded.checkpoint().bundle();
        if session.session_id() != requested.session_id
            || session.is_archived()
            || session.durability() != manifest.durability()
            || reference != head.checkpoint()
            || checkpoint.integrity() != reference.integrity()
            || bundle.root() != reference.root()
            || bundle.checkpoint_id() != reference.checkpoint_id()
            || bundle.checkpoint_integrity() != reference.integrity()
            || topology.session_id != requested.session_id
            || topology.branch_id != requested.branch_id
            || topology.checkpoint_id != requested.checkpoint_id
            || topology.branch_revision != head.revision().get()
        {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::Integrity,
                "loaded branch disagrees with its runtime manifest authority",
            ));
        }
        Ok(Self {
            locator: requested,
            session_revision: session.revision(),
            checkpoint,
        })
    }

    fn recovery_report(&self) -> Option<RecoveryRequiredReport> {
        RecoveryRequiredReport::from_checkpoint(self.locator, &self.checkpoint)
    }
}

#[cfg(test)]
mod lease_override {
    use std::num::NonZeroU64;

    use super::{SESSION_LEASE_SECONDS, lease_seconds_override};

    #[test]
    fn the_production_lease_is_thirty_seconds() {
        assert_eq!(SESSION_LEASE_SECONDS.get(), 30);
    }

    #[test]
    fn only_a_shorter_whole_second_override_is_accepted() {
        assert!(lease_seconds_override(None).is_none());
        assert!(lease_seconds_override(Some("0")).is_none());
        assert!(lease_seconds_override(Some("31")).is_none());
        assert!(lease_seconds_override(Some("nope")).is_none());
        assert_eq!(
            lease_seconds_override(Some("1")).map(NonZeroU64::get),
            Some(1)
        );
        assert_eq!(
            lease_seconds_override(Some(" 30 ")).map(NonZeroU64::get),
            Some(30)
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "session workflow integration tests keep each complete fault/publication scenario together"
)]
mod tests;

/// Unsupported implementation bridge for process-local hosting capabilities.
#[doc(hidden)]
pub mod internal {
    /// Captures only the selected model's content-free, exact-route probe.
    #[must_use]
    pub fn reachability_probe(
        runtime: &super::SessionRuntimeBinding,
    ) -> Option<plakous_model::ReachabilityProbe> {
        plakous_agent::internal::facade_support::agent_builder_reachability_probe(&runtime.builder)
    }
}
