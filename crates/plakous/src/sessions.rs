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
    checkpoint_instruction_sources, checkpoint_job_config,
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
#[cfg(feature = "sessions-sqlite")]
use plakous_store::SqliteSessionStore;
use plakous_tools::{ToolExecutorId, ToolOutcome};
#[cfg(feature = "sessions-sqlite")]
use std::path::Path;

/// Schema version of the SQLite runtime store this build reads and writes.
/// A product keys its state directory by it and by [`RUNTIME_FORMAT_EPOCH`],
/// so a build never opens a store another format wrote.
#[cfg(feature = "sessions-sqlite")]
pub const RUNTIME_STORE_SCHEMA_VERSION: i64 = plakous_store::SQLITE_SCHEMA_VERSION;
/// Format epoch of the persisted runtime checkpoint graph. Any change to a
/// persisted runtime encoding bumps it; nothing older decodes.
pub const RUNTIME_FORMAT_EPOCH: u32 = plakous_agent::internal::RUNTIME_FORMAT_EPOCH;

mod attached;

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
pub use backend::{SessionBackend, SessionStoreBound};
mod call_detail;
#[cfg(feature = "sessions-sqlite")]
pub use backend::{SessionPersistenceShape, SessionStoreBoundUsage, SessionStoreUsage};
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
mod registry;
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
    PruneHistoryOutcome, PruneHistoryReport, RebindSessionOutcome, RebindSessionRequest,
    RebindToolTransition, ReclaimReport, RecoverEffectEvidenceOutcome,
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

/// Exact current runtime construction request.
///
/// The builder is never allowed to replace persisted history, plan, model, or
/// tool defaults. It is not external-authority evidence; the private runtime
/// must compare it with the historical manifest and a separate Plakous-owned
/// fresh admission before ordinary execution can start.
pub struct SessionRuntimeBinding {
    builder: AgentBuilder,
}

impl SessionRuntimeBinding {
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
    runtime: DurableSessionRuntime,
    drivers: Arc<DriverRegistry>,
    feed: Arc<feed::FeedNotifier>,
}

impl AgentSessions {
    /// Drains deferred object reclaim in bounded, crash-safe transactions.
    /// Does not prune checkpoints or delete session authority.
    ///
    /// # Errors
    /// Returns a typed backend or integrity failure.
    pub async fn reclaim(&self) -> Result<ReclaimReport, AgentSessionsError> {
        self.store
            .reclaim(plakous_agent::internal::StoreReclaimMode::Drain)
            .await
            .map(ReclaimReport::from_store)
            .map_err(store_error)
    }

    /// Runs passive backend checkpoint maintenance after a consumer is promptable.
    ///
    /// # Errors
    /// Returns a typed backend failure, without changing runtime authority.
    pub async fn passive_checkpoint(&self) -> Result<(), AgentSessionsError> {
        self.store.flush().await.map(|_| ()).map_err(store_error)
    }

    /// Inspects one exact SQLite branch head without opening a mutable runtime,
    /// acquiring a lease, changing a boot domain, or calling a provider or tool.
    /// A stale locator returns the current head in the typed error; callers may
    /// inspect that head separately but must not adopt it as a session command.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, schema, or storage failure.
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_recovery_sqlite(
        path: impl AsRef<Path>,
        locator: SessionLocator,
    ) -> Result<Option<RecoveryRequiredReport>, AgentSessionsError> {
        let loaded =
            SqliteSessionStore::inspect_branch(path, locator.session_ref()).map_err(store_error)?;
        let exact = ExactSource::validate(locator, &loaded)?;
        let report = exact.recovery_report();
        if matches!(
            exact.checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working { .. }
        ) && report.is_none()
        {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::Integrity,
                "working checkpoint lost its exact recovery obligations",
            ));
        }
        Ok(report)
    }

    /// Pages committed canonical history at one exact SQLite branch head,
    /// including Working heads. This reads no uncommitted stream items, opens
    /// no mutable runtime, takes no lease, and performs no recovery or writes.
    /// Cursor, visibility and page bounds are the same as [`Self::history_page`].
    /// A committed compaction segment projects as kind `compaction` with a short
    /// display phrase; its summary and provider replay envelope are omitted.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, schema, invalid-input,
    /// or storage failure. Cursor/revision mismatch requests a resnapshot.
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_history_page_sqlite(
        path: impl AsRef<Path>,
        request: &SessionHistoryPageRequest,
    ) -> Result<SessionHistoryPageOutcome, AgentSessionsError> {
        if let Some(outcome) = history_page_preflight(request)? {
            return Ok(outcome);
        }
        let loaded = SqliteSessionStore::inspect_branch(path, request.locator.session_ref())
            .map_err(store_error)?;
        let exact = ExactSource::validate(request.locator, &loaded)?;
        Ok(project_history_page(
            exact.checkpoint.manifest().canonical().history(),
            request,
            committed_compaction_facts(&exact.checkpoint),
        ))
    }

    /// Reads the committed conversation of one exact quiescent SQLite branch
    /// head so it can seed a new session elsewhere, without opening a mutable
    /// runtime, acquiring a lease, or calling a provider or tool.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, schema, or storage
    /// failure, and an invalid-input refusal when the head is an unfinished
    /// turn.
    #[cfg(feature = "sessions-sqlite")]
    pub fn export_history_sqlite(
        path: impl AsRef<Path>,
        locator: SessionLocator,
    ) -> Result<PortableSessionHistory, AgentSessionsError> {
        let loaded =
            SqliteSessionStore::inspect_branch(path, locator.session_ref()).map_err(store_error)?;
        let exact = ExactSource::validate(locator, &loaded)?;
        let manifest = exact.checkpoint.manifest();
        if !matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent) {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "session head is an unfinished turn; recover it before carrying its history",
            ));
        }
        require_export_job_terminals(
            manifest
                .jobs()
                .rows()
                .iter()
                .any(|row| row.terminal.is_none()),
        )?;
        Ok(PortableSessionHistory::new(
            manifest.canonical().history().segments().to_vec(),
            manifest.binding().account().subject().to_owned(),
            manifest.binding().requested_model().clone(),
        ))
    }

    /// Composes the high-level workflows over one opaque semantic backend.
    ///
    /// # Errors
    ///
    /// Returns a typed failure if the private runtime cannot be composed.
    pub fn new(backend: SessionBackend) -> Result<Self, AgentSessionsError> {
        let (store, durability, drivers) = backend.into_parts();
        let feed = Arc::new(feed::FeedNotifier::default());
        let store = feed::NotifyingStore::wrap(store, Arc::clone(&feed));
        let runtime =
            DurableSessionRuntime::current(Arc::clone(&store), durability, SESSION_LEASE_SECONDS)
                .map_err(|error| runtime_error(&error))?;
        Ok(Self {
            store,
            runtime,
            drivers,
            feed,
        })
    }

    /// Archives one exact released session revision.
    ///
    /// The consumer owns discovery, retention policy, and confirmation. Plakous
    /// owns the exact compare-and-swap transition and returns the revision that
    /// can authorize a later semantic deletion. Archived sessions are listed by
    /// [`Self::archived_sessions`]; a consumer that also archives for reasons
    /// other than deletion must not finish every listed session as a delete.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership, stale-locator, integrity, or backend failure.
    pub async fn archive(
        &self,
        locator: SessionLocator,
    ) -> Result<ArchiveSessionReport, AgentSessionsError> {
        let _reservation = self.drivers.reserve_session(locator.session_id)?;
        let source = self.load_exact(locator).await?;
        let receipt = self
            .store
            .archive(ArchiveSession::new(
                locator.session_id,
                source.session_revision,
            ))
            .await
            .map_err(store_error)?;
        Ok(ArchiveSessionReport::from_receipt(receipt))
    }

    /// Deletes one exact already-archived session revision.
    ///
    /// Listing, archival, user confirmation, and retention policy remain the
    /// consumer's workflow. An attached private driver always refuses deletion.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership, revision, capacity, integrity, or backend
    /// failure without partially deleting authority.
    pub async fn delete(
        &self,
        session_id: SessionId,
        expected_archived_revision: u64,
    ) -> Result<DeleteSessionReport, AgentSessionsError> {
        let _reservation = self.drivers.reserve_session(session_id)?;
        let receipt = self
            .store
            .delete(DeleteSession::new(
                session_id,
                plakous_agent::internal::SessionRevision::from_store(expected_archived_revision),
            ))
            .await
            .map_err(store_error)?;
        Ok(DeleteSessionReport::from_receipt(receipt))
    }

    /// Deletes the session `session_id` if it is retained only as an archived
    /// session: an archive that was accepted but whose deletion did not
    /// complete. Returns `None` when the store retains no session under this
    /// identity, live or archived, which is the case once it was deleted.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when the session is retained but not archived,
    /// or a typed ownership, integrity, or backend failure.
    pub async fn delete_archived(
        &self,
        session_id: SessionId,
    ) -> Result<Option<DeleteSessionReport>, AgentSessionsError> {
        let page_size = std::num::NonZeroUsize::MIN.saturating_add(15);
        let mut query =
            plakous_agent::internal::SessionQuery::new(page_size).include_archived(true);
        let head = loop {
            let page = self.store.list(query).await.map_err(store_error)?;
            if let Some(summary) = page
                .items()
                .iter()
                .find(|summary| summary.head().session_id() == session_id)
            {
                break summary.head();
            }
            match page.next_after() {
                Some(after) => query = query.after(after),
                None => return Ok(None),
            }
        };
        if !head.is_archived() {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "session is retained but not archived",
            ));
        }
        self.delete(session_id, head.revision().get())
            .await
            .map(Some)
    }

    /// Reclaims the superseded checkpoint history of one released session at
    /// its exact head. The head, its parent, create and fork checkpoints,
    /// checkpoints other sessions depend on, and terminal results the head
    /// still names are kept; the locator stays valid. Repeating it reclaims
    /// nothing more.
    ///
    /// # Errors
    ///
    /// Returns a typed ownership failure while the session is attached or
    /// leased, a stale-locator failure naming the current head, or an
    /// integrity or backend failure. Never refused for store capacity.
    pub async fn prune_history(
        &self,
        locator: SessionLocator,
    ) -> Result<PruneHistoryOutcome, AgentSessionsError> {
        const OPERATION: &str = "session-history-prune";
        let _reservation = self.drivers.reserve_session(locator.session_id)?;
        self.load_exact(locator).await?;
        let address = DurableSessionAddress::new(
            locator.session_id,
            locator.branch_id,
            locator.checkpoint_id,
        );
        match self.runtime.prune_history(OPERATION, address).await {
            Ok(Some(receipt)) => Ok(PruneHistoryOutcome::Pruned(
                PruneHistoryReport::from_receipt(locator, receipt),
            )),
            Ok(None) => Ok(PruneHistoryOutcome::RecoveryRequired),
            Err(error) => Err(self.map_branch_runtime_error(locator, &error).await),
        }
    }

    /// Lists the sessions this host's store holds archived but not deleted,
    /// each with the archived revision [`Self::delete`] needs. A consumer that
    /// archives only as the first half of a deletion can finish every listed
    /// session as an interrupted delete; one that archives for other reasons
    /// must tell them apart itself.
    ///
    /// # Errors
    ///
    /// Returns a typed integrity or backend failure.
    pub async fn archived_sessions(
        &self,
    ) -> Result<Vec<ArchivedSessionReport>, AgentSessionsError> {
        const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(128).unwrap();
        let mut archived = Vec::new();
        let mut query = plakous_agent::internal::SessionQuery::new(PAGE).include_archived(true);
        loop {
            let page = self.store.list(query).await.map_err(store_error)?;
            archived.extend(
                page.items()
                    .iter()
                    .map(|summary| summary.head())
                    .filter(|head| head.is_archived())
                    .map(|head| {
                        ArchivedSessionReport::new(head.session_id(), head.revision().get())
                    }),
            );
            let Some(after) = page.next_after() else {
                return Ok(archived);
            };
            query = query.after(after);
        }
    }

    /// Recovers the exact accepted deletion after acknowledgement loss.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found, revision-conflict, integrity, or backend
    /// failure.
    pub async fn recover_delete(
        &self,
        session_id: SessionId,
        expected_archived_revision: u64,
    ) -> Result<DeleteSessionReport, AgentSessionsError> {
        let receipt = self
            .store
            .recover_delete(DeleteSession::new(
                session_id,
                plakous_agent::internal::SessionRevision::from_store(expected_archived_revision),
            ))
            .await
            .map_err(store_error)?;
        Ok(DeleteSessionReport::from_receipt(receipt))
    }

    /// Inspects the exact accepted create receipt without compiling a runtime.
    /// Hosts use the returned genesis locator to read construction facts after
    /// losing locator publication. This never adopts a later head or mutates state.
    ///
    /// # Errors
    /// Returns typed store, integrity or identity failures. Only an absent
    /// deterministic session returns `None`; a retained but stale genesis is
    /// still returned and exact construction reads refuse it.
    pub async fn accepted_create(
        &self,
        operation_id: &SessionOperationId,
    ) -> Result<Option<SessionLocator>, AgentSessionsError> {
        self.runtime
            .accepted_create(operation_id.as_str())
            .await
            .map(|address| address.map(SessionLocator::from_durable))
            .map_err(|error| runtime_error(&error))
    }

    /// Derives the exact genesis locator a create operation will use.
    /// No store, clock or lease is read; this is not evidence of admission.
    ///
    /// # Errors
    /// Returns a typed identity derivation failure.
    pub fn genesis_locator_for(
        operation_id: &SessionOperationId,
    ) -> Result<SessionLocator, AgentSessionsError> {
        DurableSessionRuntime::genesis_address_for(operation_id.as_str())
            .map(SessionLocator::from_durable)
            .map_err(|error| runtime_error(&error))
    }

    /// Atomically creates and attaches one reconstruction-complete session.
    /// Exact retries after release recover the runtime-owned create receipt
    /// before returning the same genesis locator. A still-attached operation
    /// is refused so it cannot create a second private driver.
    ///
    /// # Errors
    ///
    /// Returns typed validation, revalidation, identity, ownership, or backend
    /// failure. Any failed attachment releases its acquired lease.
    pub async fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionOutcome, AgentSessionsError> {
        let CreateSessionRequest {
            operation_id,
            runtime,
            history,
        } = request;
        let history = match history {
            Some(history) => {
                let retain_replay = history.retains_replay_for(&runtime);
                history.into_canonical(retain_replay)?
            }
            None => plakous_model::CanonicalHistory::default(),
        };
        let _reservation = self.drivers.reserve(&operation_id)?;
        match Box::pin(
            self.runtime
                .create_seeded(operation_id.as_str(), runtime.builder, history),
        )
        .await
        {
            Ok(agent) => {
                let session = self.attach(operation_id.clone(), agent).await?;
                Ok(CreateSessionOutcome::Created {
                    operation_id,
                    session,
                })
            }
            Err(error) => Err(runtime_error(&error)),
        }
    }

    /// Whether the head at this locator is an unfinished turn awaiting recovery.
    ///
    /// A caller that has just been refused for a stale locator uses this to say
    /// something true about why. A head that is simply further along is one
    /// thing; a head that is an unfinished turn this release cannot close is
    /// another, and it will still be one after any amount of waiting. Saying so
    /// is the difference between a caller that stops and a caller that spends
    /// its whole patience on a refusal that cannot change.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, integrity or backend failure an open
    /// would.
    pub async fn head_requires_recovery(
        &self,
        head: SessionLocator,
    ) -> Result<bool, AgentSessionsError> {
        Ok(self.load_exact(head).await?.recovery_report().is_some())
    }

    /// Whether the head at this locator is quiescent: no turn or session
    /// workflow is unsettled on it.
    ///
    /// A host deleting the sessions a finished agent tree left behind uses this
    /// to confirm a child's runtime session is at rest rather than trusting the
    /// tree's record of it.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, stale-locator, integrity or backend
    /// failure an open would.
    pub async fn head_is_quiescent(
        &self,
        head: SessionLocator,
    ) -> Result<bool, AgentSessionsError> {
        Ok(matches!(
            self.load_exact(head)
                .await?
                .checkpoint
                .manifest()
                .lifecycle(),
            RuntimeLifecycle::Quiescent
        ))
    }

    /// Reads the running ordinary turn of an exact head without acquiring a
    /// lease. Quiescent heads and session workflow roots have no ordinary turn.
    ///
    /// # Errors
    /// Returns typed exact-head lookup, stale-locator, integrity or backend failure.
    pub async fn head_running_turn(
        &self,
        head: SessionLocator,
    ) -> Result<Option<TurnId>, AgentSessionsError> {
        Ok(
            match self
                .load_exact(head)
                .await?
                .checkpoint
                .manifest()
                .lifecycle()
            {
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::Turn { turn_id, .. },
                    ..
                } => Some(*turn_id),
                _ => None,
            },
        )
    }

    /// The process budgets bound to the session's stored runtime environment.
    ///
    /// Read these before compiling tool descriptions or composition attachments
    /// for an exact reopen. Current product defaults are not resume authority.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity, compatibility or backend
    /// failures, without substituting defaults for invalid stored values.
    pub async fn created_job_config(
        &self,
        locator: SessionLocator,
    ) -> Result<plakous_tools::JobConfig, AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        checkpoint_job_config(&exact.checkpoint)
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// Verifies and loads the exact recorded evaluator before runtime compilation.
    /// No current sidecar locator participates, and this read never changes a head.
    ///
    /// # Errors
    /// Returns typed exact-lookup failures or incompatibility when the recorded
    /// executable, canonical path, hash or engine contract no longer agrees.
    #[cfg(feature = "composition")]
    pub async fn created_composition_artifact(
        &self,
        locator: SessionLocator,
    ) -> Result<Option<crate::local_composition::LocalCompositionEngineArtifact>, AgentSessionsError>
    {
        let exact = self.load_exact(locator).await?;
        plakous_agent::internal::facade_support::checkpoint_composition_artifact(&exact.checkpoint)
            .and_then(|stored| {
                stored
                    .as_ref()
                    .map(crate::local_composition::restore_stored_artifact)
                    .transpose()
            })
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// The instruction sources (`AGENTS.md` and the like) the session at this
    /// exact locator was compiled from, without opening it.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity, compatibility or backend
    /// failures.
    pub async fn created_instruction_sources(
        &self,
        locator: SessionLocator,
    ) -> Result<Vec<plakous_agent::ResolvedContextSourceFact>, AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        checkpoint_instruction_sources(&exact.checkpoint)
            .map_err(|error| runtime_error(&error).for_session(locator.session_id))
    }

    /// The per-request output bound the session at this locator was created with.
    ///
    /// A route that derives its output bound from its own attestation moves the
    /// figure when the provider's catalog moves, and that figure is
    /// identity-bearing: it reaches the configuration identity, and from there
    /// the persisted harness plan, tool projection target, request prefix and
    /// admission evidence. Those artefacts cannot be re-signed on reopen, and
    /// the request prefix has to satisfy the persisted projection and the live
    /// adapter at once, so a reopened session has to be compiled with the bound
    /// it was created under rather than with today's. The caller reads it here,
    /// before it compiles anything.
    ///
    /// The disposition is returned whole, not just the figure: a session
    /// created before this route derived anything spelled the same figure
    /// `Enforced`, and the two spellings digest differently.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, stale-locator, integrity or backend
    /// failure an open would.
    pub async fn created_request_output_bound(
        &self,
        locator: SessionLocator,
    ) -> Result<OutputBoundDisposition, AgentSessionsError> {
        Ok(self
            .load_exact(locator)
            .await?
            .checkpoint
            .manifest()
            .provider_configuration()
            .effective()
            .behavior()
            .output_bound)
    }

    /// The provider semantic evidence the session at this locator was created
    /// with: the canonical bytes its configuration identity binds.
    ///
    /// A route that binds admitted model facts into that evidence reads them
    /// here before it compiles a reopen, so a moved fact is refused with its
    /// own reason instead of an identity mismatch.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, stale-locator, integrity or backend
    /// failure an open would.
    pub async fn created_provider_semantic_evidence(
        &self,
        locator: SessionLocator,
    ) -> Result<Vec<u8>, AgentSessionsError> {
        Ok(self
            .load_exact(locator)
            .await?
            .checkpoint
            .manifest()
            .provider_configuration()
            .effective()
            .authority()
            .semantic_evidence()
            .canonical_bytes()
            .to_vec())
    }

    /// Loads and validates one exact historical head without substituting
    /// current defaults. Working state returns recovery-only authority.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, compatibility, or
    /// backend failure without returning an ordinary runtime.
    pub async fn open(
        &self,
        request: OpenSessionRequest,
    ) -> Result<OpenSessionOutcome, AgentSessionsError> {
        let OpenSessionRequest {
            operation_id,
            locator,
            runtime,
        } = request;
        let _reservation = self.drivers.reserve(&operation_id)?;
        let exact = self.load_exact(locator).await?;
        // Use the runtime's exact closure scope: saved terminal evidence and
        // certified abandonment both recover here without redispatch.
        if let Some(report) = exact.recovery_report()
            && !runtime_can_restore_phase(report.phase())
            && !plakous_agent::internal::checkpoint_closes_on_reopen(&exact.checkpoint)
        {
            return Ok(OpenSessionOutcome::RecoveryRequired {
                operation_id,
                recovery: report,
                retry_after_seconds: None,
            });
        }
        let address = DurableSessionAddress::new(
            locator.session_id,
            locator.branch_id,
            locator.checkpoint_id,
        );
        match Box::pin(
            self.runtime
                .open(operation_id.as_str(), address, runtime.builder),
        )
        .await
        {
            Ok(DurableRuntimeOpen::Opened(agent)) => {
                let session = self.attach(operation_id.clone(), agent).await?;
                Ok(OpenSessionOutcome::Opened {
                    operation_id,
                    session,
                })
            }
            Ok(DurableRuntimeOpen::RestoredWorking { agent, turn }) => {
                let session = self.attach(operation_id.clone(), agent).await?;
                Ok(OpenSessionOutcome::RestoredWorking {
                    operation_id,
                    session,
                    turn,
                })
            }
            Ok(DurableRuntimeOpen::RecoveryRequired {
                retry_after_seconds,
                ..
            }) => {
                let recovery = self
                    .load_exact(locator)
                    .await?
                    .recovery_report()
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "runtime recovery classification changed during exact open",
                        )
                    })?;
                Ok(OpenSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery,
                    retry_after_seconds,
                })
            }
            Err(error) => {
                if error.kind() == AgentErrorKind::Session && agent_store_origin(&error).is_none() {
                    self.load_exact(locator).await?;
                }
                Err(runtime_error(&error).for_session(locator.session_id))
            }
        }
    }

    /// Reconciles only the exact compaction operation bound to this identity.
    /// A finalized receipt wins before branch freshness, and recovery never
    /// issues a second provider call.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn recover(
        &self,
        request: RecoverSessionRequest,
    ) -> Result<RecoverSessionOutcome, AgentSessionsError> {
        let RecoverSessionRequest {
            operation_id,
            locator,
            runtime,
        } = request;
        let recovery_head = match self
            .runtime
            .compaction_receipt(operation_id.as_str(), locator.durable())
            .await
        {
            Ok(DurableCompactionReceipt::Compacted { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(RecoverSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::Failed { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(RecoverSessionOutcome::Failed {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::Abandoned { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(RecoverSessionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::NotFound) => None,
            Ok(DurableCompactionReceipt::RecoveryRequired(address)) => Some(address),
            Err(error) => {
                return Err(self.map_branch_runtime_error(locator, &error).await);
            }
        };

        if let Some(address) = recovery_head {
            if address.session_id() != locator.session_id
                || address.branch_id() != locator.branch_id
            {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::Integrity,
                    "compaction recovery receipt crossed its source branch",
                ));
            }
            self.load_recovery_required(SessionLocator::from_durable(address))
                .await?;
        } else {
            let exact = self.load_exact(locator).await?;
            if matches!(
                exact.checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::Turn { .. },
                    ..
                }
            ) {
                let recovery = exact.recovery_report().ok_or_else(|| {
                    AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "ordinary working recovery lost its exact obligations",
                    )
                })?;
                return Ok(RecoverSessionOutcome::Unavailable {
                    operation_id,
                    recovery,
                    reason: SessionWorkflowUnavailable::WorkingRecovery,
                });
            }
        }

        let recovered = match Box::pin(self.runtime.recover_compaction(
            operation_id.as_str(),
            locator.durable(),
            runtime.builder,
        ))
        .await
        {
            Ok(recovered) => recovered,
            Err(error) => {
                let error_locator = recovery_head.map_or(locator, SessionLocator::from_durable);
                return Err(self.map_branch_runtime_error(error_locator, &error).await);
            }
        };
        self.present_compaction_recovery(operation_id, locator, recovered)
            .await
    }

    async fn present_compaction_recovery(
        &self,
        operation_id: SessionOperationId,
        locator: SessionLocator,
        recovered: DurableCompactionRecoveryOutcome,
    ) -> Result<RecoverSessionOutcome, AgentSessionsError> {
        Ok(match recovered {
            DurableCompactionRecoveryOutcome::NotRequired(source) => {
                RecoverSessionOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableCompactionRecoveryOutcome::Compacted { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Failed { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverSessionOutcome::Failed {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Abandoned { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverSessionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::RecoveryRequired(address) => {
                RecoverSessionOutcome::Unavailable {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                    reason: SessionWorkflowUnavailable::WorkingRecovery,
                }
            }
        })
    }

    /// Deliberately terminalizes one exact ambiguous compaction without
    /// provider lookup or redispatch. The returned target preserves the source
    /// semantics and may admit a later, differently identified operation.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, identity, ownership, integrity, or backend failure.
    pub async fn abandon_compaction(
        &self,
        request: AbandonCompactionRequest,
    ) -> Result<AbandonCompactionOutcome, AgentSessionsError> {
        let AbandonCompactionRequest {
            operation_id,
            locator,
        } = request;
        let outcome = self
            .runtime
            .abandon_compaction(operation_id.as_str(), locator.durable())
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(match outcome {
            DurableCompactionRecoveryOutcome::NotRequired(source) => {
                AbandonCompactionOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableCompactionRecoveryOutcome::Compacted { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCompactionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Abandoned { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCompactionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Failed { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCompactionOutcome::Failed {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::RecoveryRequired(address) => {
                AbandonCompactionOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                }
            }
        })
    }

    /// Finalizes one exact recovery-only Code Cell without evaluating source,
    /// redispatching a nested child, or emitting another root turn result.
    /// Exact repetition returns the first committed target.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, identity, ownership, integrity, or backend failure.
    pub async fn abandon_code_cell(
        &self,
        request: AbandonCodeCellRequest,
    ) -> Result<AbandonCodeCellOutcome, AgentSessionsError> {
        let AbandonCodeCellRequest {
            operation_id,
            locator,
        } = request;
        let request =
            DurableCodeCellAbandonmentRequest::new(operation_id.as_str(), locator.durable())
                .map_err(|error| runtime_error(&error))?;
        let outcome = self
            .runtime
            .abandon_code_cell(request)
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(match outcome {
            DurableCodeCellAbandonmentOutcome::NotRequired(source) => {
                AbandonCodeCellOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableCodeCellAbandonmentOutcome::Finalized { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCodeCellOutcome::Finalized {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCodeCellAbandonmentOutcome::RecoveryRequired(address) => {
                AbandonCodeCellOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                }
            }
        })
    }

    /// Finalizes one exact Working effect from executor-owned terminal evidence.
    ///
    /// The private driver validates the original claim/dispatch evidence and
    /// executor identity, then completes that same effect without executing the
    /// tool. Identical evidence returns the first committed target. Conflicting
    /// evidence leaves the caller turn [`RecoverEffectEvidenceOutcome::RecoveryRequired`]
    /// and never dispatches under a new identity.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, identity, ownership, integrity, or backend failure.
    pub async fn recover_effect_evidence(
        &self,
        request: RecoverEffectEvidenceRequest,
    ) -> Result<RecoverEffectEvidenceOutcome, AgentSessionsError> {
        let RecoverEffectEvidenceRequest {
            operation_id,
            locator,
            recovery_operation,
            evidence,
            executor,
            outcome,
        } = request;
        let request = DurableEffectEvidenceRecoveryRequest::new(
            operation_id.as_str(),
            locator.durable(),
            recovery_operation,
            evidence.claim_digest,
            evidence.was_dispatched(),
            executor,
            outcome,
        )
        .map_err(|error| runtime_error(&error))?;
        let outcome = self
            .runtime
            .recover_effect_evidence(request)
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(match outcome {
            DurableEffectEvidenceRecoveryOutcome::NotRequired(source) => {
                RecoverEffectEvidenceOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableEffectEvidenceRecoveryOutcome::Finalized { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverEffectEvidenceOutcome::Finalized {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableEffectEvidenceRecoveryOutcome::RecoveryRequired(address) => {
                RecoverEffectEvidenceOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                }
            }
        })
    }

    /// Pages presentation items from exact canonical history without opening a
    /// mutable driver or store write. A committed compaction segment projects
    /// as kind `compaction`; its summary and provider replay envelope are omitted.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, invalid-input, or backend failure.
    pub async fn history_page(
        &self,
        request: SessionHistoryPageRequest,
    ) -> Result<SessionHistoryPageOutcome, AgentSessionsError> {
        if let Some(outcome) = history_page_preflight(&request)? {
            return Ok(outcome);
        }
        let exact = self.load_exact(request.locator).await?;
        Ok(project_history_page(
            exact.checkpoint.manifest().canonical().history(),
            &request,
            committed_compaction_facts(&exact.checkpoint),
        ))
    }

    /// Reads one tool call's input and result from the exact checkpoint's
    /// committed canonical history by call id, without opening a mutable
    /// driver or store write. Each side is redacted with the shared preview
    /// redactor and bounded at [`MAX_CALL_DETAIL_SIDE_BYTES`]. Parsed arguments
    /// are pretty-printed only up to the redaction window; the unread tail is
    /// not copied. Only this session's history is searched: a call that is
    /// absent (compacted, pruned, not carried, not committed yet, or another
    /// session's) is [`SessionCallDetailOutcome::NotInCanonicalHistory`].
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn call_detail(
        &self,
        locator: SessionLocator,
        call_id: CallId,
    ) -> Result<SessionCallDetailOutcome, AgentSessionsError> {
        let exact = self.load_exact(locator).await?;
        Ok(call_detail::project_call_detail(
            exact.checkpoint.manifest().canonical().history(),
            call_id,
        ))
    }

    /// Tests whether each id has both a call and a result in this exact head's
    /// committed canonical history, preserving request order and duplicates.
    /// Authenticates the head in one load, including an empty batch, without
    /// opening a driver or rendering call previews.
    ///
    /// # Errors
    /// Returns typed lookup, stale-locator, integrity or backend failure.
    pub async fn committed_calls(
        &self,
        locator: SessionLocator,
        ids: Vec<CallId>,
    ) -> Result<Vec<(CallId, bool)>, AgentSessionsError> {
        use plakous_model::{ContentBlock, ConversationItem, HistorySegment};
        use std::collections::BTreeMap;
        let exact = self.load_exact(locator).await?;
        let mut calls: BTreeMap<_, _> = ids.iter().map(|id| (*id, (false, false))).collect();
        for block in exact
            .checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
        {
            match block {
                ContentBlock::ToolCall(call) => {
                    if let Some(seen) = calls.get_mut(&call.call_id()) {
                        seen.0 = true;
                    }
                }
                ContentBlock::ToolResult(result) => {
                    if let Some(seen) = calls.get_mut(&result.call_id()) {
                        seen.1 = true;
                    }
                }
                _ => {}
            }
        }
        Ok(ids
            .into_iter()
            .map(|id| {
                let (call, result) = calls[&id];
                (id, call && result)
            })
            .collect())
    }

    /// Reads the committed canonical history of `locator`'s session and
    /// branch at its current head, in one load and without opening a mutable
    /// driver or store write. A head that moved on the same session and
    /// branch is read where it is now; one on another session or branch is
    /// refused as stale.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn transcript_source(
        &self,
        locator: SessionLocator,
    ) -> Result<SessionTranscriptSource, AgentSessionsError> {
        let loaded = self
            .store
            .load_branch(locator.session_ref())
            .await
            .map_err(|error| store_error(error).for_session(locator.session_id))?;
        let (current, exact) = match ExactSource::validate(locator, &loaded) {
            Ok(exact) => (locator, exact),
            Err(error) => match error.current_locator() {
                Some(current)
                    if error.kind() == AgentSessionsErrorKind::StaleLocator
                        && current.session_id == locator.session_id
                        && current.branch_id == locator.branch_id =>
                {
                    (current, ExactSource::validate(current, &loaded)?)
                }
                _ => return Err(error),
            },
        };
        Ok(SessionTranscriptSource::new(
            current,
            exact.checkpoint.manifest().canonical().history().clone(),
            exact.checkpoint.manifest().ledger().clone(),
        ))
    }

    /// Returns only descriptor-bound status or a typed refusal; it never
    /// estimates tokens from characters or guesses a model limit.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn context_status(
        &self,
        request: ContextStatusRequest,
    ) -> Result<SessionContextStatus, AgentSessionsError> {
        let exact = self.load_exact(request.locator).await?;
        let recovery = exact.recovery_report();
        let pending_user_input = match request.query {
            ContextQuery::Current => None,
            ContextQuery::WithPendingUserInput(input) => Some(input),
        };
        let branch = SessionBranchKey::new(request.locator.session_id, request.locator.branch_id);
        let Some(endpoint) = self.drivers.endpoint(branch) else {
            return Ok(SessionContextStatus {
                operation_id: request.operation_id,
                locator: request.locator,
                kind: recovery.map_or(
                    SessionContextStatusKind::Unavailable(
                        SessionWorkflowUnavailable::ContextStatus,
                    ),
                    SessionContextStatusKind::RecoveryRequired,
                ),
            });
        };
        let kind = match endpoint
            .context_status(
                request.locator.session_id,
                request.locator.branch_id,
                request.locator.checkpoint_id,
                pending_user_input,
            )
            .await
        {
            Ok(status) => SessionContextStatusKind::Available(Box::new(status)),
            Err(AgentSessionEndpointError::Stale {
                session_id,
                branch_id,
                checkpoint_id,
            }) => {
                return Err(AgentSessionsError::stale(SessionLocator::new(
                    session_id,
                    branch_id,
                    checkpoint_id,
                )));
            }
            Err(AgentSessionEndpointError::InvalidInput) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::InvalidInput,
                    "pending user input is not valid canonical text",
                ));
            }
            Err(
                AgentSessionEndpointError::Backpressure
                | AgentSessionEndpointError::Busy
                | AgentSessionEndpointError::Unavailable
                | AgentSessionEndpointError::OutcomeUnknown
                | AgentSessionEndpointError::Rejected { .. },
            ) => SessionContextStatusKind::Unavailable(SessionWorkflowUnavailable::ContextStatus),
        };
        Ok(SessionContextStatus {
            operation_id: request.operation_id,
            locator: request.locator,
            kind,
        })
    }

    /// Requests driver-owned active-model no-tools compaction, returning an
    /// exact finalized receipt before consulting current branch freshness.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn compact(
        &self,
        request: CompactSessionRequest,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        let CompactSessionRequest {
            operation_id,
            locator,
            reason,
            qualification,
            runtime,
        } = request;
        let pending_user_input = compaction_pending_user_input(reason, qualification)?;
        match self
            .runtime
            .compaction_receipt(operation_id.as_str(), locator.durable())
            .await
        {
            Ok(DurableCompactionReceipt::Compacted { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(CompactSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                    // A recovered receipt names the checkpoints and nothing
                    // else; the figures live on the successor and this path
                    // does not open it.
                    summary: None,
                    feed_seq: None,
                });
            }
            Ok(DurableCompactionReceipt::Failed { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(CompactSessionOutcome::Failed {
                    operation_id,
                    source,
                    target,
                    diagnostic: None,
                });
            }
            Ok(DurableCompactionReceipt::Abandoned { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(CompactSessionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::RecoveryRequired(address)) => {
                return Ok(CompactSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                    diagnostic: None,
                });
            }
            Ok(DurableCompactionReceipt::NotFound) => {}
            Err(error) => {
                return Err(self.map_branch_runtime_error(locator, &error).await);
            }
        }

        let exact = self.load_exact(locator).await?;
        if let Some(recovery) = exact.recovery_report() {
            return Ok(CompactSessionOutcome::RecoveryRequired {
                operation_id,
                recovery,
                diagnostic: None,
            });
        }
        let branch = SessionBranchKey::new(locator.session_id, locator.branch_id);
        let Some(endpoint) = self.drivers.endpoint(branch) else {
            return Ok(CompactSessionOutcome::Unavailable {
                operation_id,
                source: locator,
                reason: SessionWorkflowUnavailable::Compaction,
            });
        };
        let reason = match reason {
            CompactReason::Manual => AgentCompactionReason::Manual,
            CompactReason::Scheduled => AgentCompactionReason::Scheduled,
        };
        let outcome = endpoint
            .compact_session(AgentCompactSessionRequest::with_pending_user_input(
                operation_id.as_str().to_owned(),
                locator.session_id,
                locator.branch_id,
                locator.checkpoint_id,
                reason,
                pending_user_input,
                runtime.builder,
            ))
            .await;
        self.map_compact_outcome(operation_id, locator, outcome)
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one closed mapping preserves every compaction outcome and endpoint refusal"
    )]
    async fn map_compact_outcome(
        &self,
        operation_id: SessionOperationId,
        locator: SessionLocator,
        outcome: Result<AgentCompactSessionOutcome, AgentSessionEndpointError>,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        Ok(match outcome {
            Ok(AgentCompactSessionOutcome::Compacted {
                source,
                target,
                summary,
                feed_seq,
                ..
            }) => {
                let (source, target) = validate_compaction_publication(
                    locator,
                    DurableSessionAddress::new(locator.session_id, locator.branch_id, source),
                    DurableSessionAddress::new(locator.session_id, locator.branch_id, target),
                )?;
                CompactSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                    summary,
                    feed_seq,
                }
            }
            Ok(AgentCompactSessionOutcome::NotNeeded) => CompactSessionOutcome::NotNeeded {
                operation_id,
                source: locator,
            },
            Ok(AgentCompactSessionOutcome::NoEligibleRange) => {
                CompactSessionOutcome::NoEligibleRange {
                    operation_id,
                    source: locator,
                }
            }
            Ok(AgentCompactSessionOutcome::Failed {
                checkpoint,
                diagnostic,
                ..
            }) => CompactSessionOutcome::Failed {
                operation_id,
                source: locator,
                target: SessionLocator::new(locator.session_id, locator.branch_id, checkpoint),
                diagnostic,
            },
            Ok(AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint,
                diagnostic,
            }) => CompactSessionOutcome::RecoveryRequired {
                operation_id,
                recovery: self
                    .load_recovery_required(SessionLocator::new(
                        locator.session_id,
                        locator.branch_id,
                        checkpoint,
                    ))
                    .await?,
                diagnostic,
            },
            Ok(AgentCompactSessionOutcome::Unavailable)
            | Err(
                AgentSessionEndpointError::Backpressure | AgentSessionEndpointError::Unavailable,
            ) => CompactSessionOutcome::Unavailable {
                operation_id,
                source: locator,
                reason: SessionWorkflowUnavailable::Compaction,
            },
            Err(AgentSessionEndpointError::Busy) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::Busy,
                    "compaction cannot start while another compaction is already in progress",
                ));
            }
            Err(AgentSessionEndpointError::OutcomeUnknown) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::OutcomeUnknown,
                    "compaction reply was lost or its admitted outcome is unknown; consult the change feed",
                ));
            }
            Err(AgentSessionEndpointError::Rejected { reason }) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::InvalidInput,
                    reason,
                ));
            }
            Err(AgentSessionEndpointError::Stale {
                session_id,
                branch_id,
                checkpoint_id,
            }) => {
                return Err(AgentSessionsError::stale(SessionLocator::new(
                    session_id,
                    branch_id,
                    checkpoint_id,
                )));
            }
            Err(AgentSessionEndpointError::InvalidInput) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::InvalidInput,
                    "compaction runtime or request was rejected for this exact source",
                ));
            }
        })
    }

    /// Creates or recovers one exact quiescent copy-on-write fork.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn fork(
        &self,
        request: ForkSessionRequest,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        let ForkSessionRequest {
            operation_id,
            locator,
            runtime,
        } = request;
        let branched = match runtime {
            None => {
                self.runtime
                    .ordinary_fork(operation_id.as_str(), locator.durable())
                    .await
            }
            Some(runtime) => {
                Box::pin(self.runtime.recompiling_fork(
                    operation_id.as_str(),
                    locator.durable(),
                    runtime.builder,
                ))
                .await
            }
        };
        match branched {
            Ok(DurableRuntimeBranchOutcome::Published(publication)) => {
                let source = SessionLocator::from_durable(publication.source());
                if source != locator {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "runtime fork receipt disagrees with its exact source",
                    ));
                }
                Ok(ForkSessionOutcome::Forked {
                    operation_id,
                    source,
                    target: SessionLocator::from_durable(publication.target()),
                    portability: ArtifactPortability::ClearedUnproved,
                    feed_seq: publication.feed_seq(),
                })
            }
            Ok(DurableRuntimeBranchOutcome::RecoveryRequired { .. }) => {
                let recovery = self
                    .load_exact(locator)
                    .await?
                    .recovery_report()
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "fork recovery classification changed during exact workflow",
                        )
                    })?;
                Ok(ForkSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery,
                })
            }
            Err(error) => Err(self.map_branch_runtime_error(locator, &error).await),
        }
    }

    /// Creates or recovers one explicitly selected, freshly admitted rebind branch.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn rebind(
        &self,
        request: RebindSessionRequest,
    ) -> Result<RebindSessionOutcome, AgentSessionsError> {
        let RebindSessionRequest {
            operation_id,
            locator,
            target,
            runtime,
            tool_transition,
        } = request;
        match Box::pin(self.runtime.rebind(
            operation_id.as_str(),
            locator.durable(),
            target.protocol,
            target.route,
            target.model,
            target.selected_provider_configuration,
            target.effective_provider_configuration,
            target.account_binding,
            tool_transition == RebindToolTransition::RecompileForTarget,
            runtime.builder,
        ))
        .await
        {
            Ok(DurableRuntimeBranchOutcome::Published(publication)) => {
                let source = SessionLocator::from_durable(publication.source());
                if source != locator {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "runtime rebind receipt disagrees with its exact source",
                    ));
                }
                let target = SessionLocator::from_durable(publication.target());
                let exact_target = self.load_exact(target).await?;
                let binding = SessionBindingReport::from_checkpoint(&exact_target.checkpoint);
                if binding.identity() != publication.binding().model_binding() {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "runtime rebind receipt disagrees with its target manifest",
                    ));
                }
                Ok(RebindSessionOutcome::Rebound {
                    operation_id,
                    source,
                    target,
                    portability: ArtifactPortability::ClearedUnproved,
                    binding: Box::new(binding),
                    feed_seq: publication.feed_seq(),
                })
            }
            Ok(DurableRuntimeBranchOutcome::RecoveryRequired { .. }) => {
                let recovery = self
                    .load_exact(locator)
                    .await?
                    .recovery_report()
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "rebind recovery classification changed during exact workflow",
                        )
                    })?;
                Ok(RebindSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery,
                })
            }
            Err(error) => Err(self.map_branch_runtime_error(locator, &error).await),
        }
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
