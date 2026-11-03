//! Attached session lifecycle and build/open operations.

use crate::driver::reopen::classify_opened;
use crate::{CliFailure, CliResult};

use super::binding::{api_session_runtime_binding_with_owner, coding_session_runtime_binding_for};
use super::policy::SessionToolFence;
use super::topology::{
    ApiTopologyParent, ProductAgentSpec, SpawnParent, claim_root_topology_owner, topology_tree_id,
};
use crate::agent::{
    ProductStateRoots, format_session_reopen_error, incompatible_session_message,
    reattach_inflight_children,
};
use plakous::agent::{Agent, RecoveredTurnHandle};
use plakous::sessions::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, CompactReason,
    CompactSessionOutcome, CompactSessionRequest, ContextQuery, ContextStatusRequest,
    CreateSessionOutcome, CreateSessionRequest, ForkSessionOutcome, ForkSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, OpenedSession, PortableSessionHistory,
    RebindSessionOutcome, RebindSessionRequest, SessionContextStatus, SessionLocator,
    SessionOperationId, SessionRuntimeBinding, SessionTargetBinding,
};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

pub struct CodingSessionOpenFailure {
    pub failure: Box<CliFailure>,
    pub current_locator: Option<SessionLocator>,
    /// The frozen binding input this build reconstructs differently, when the
    /// open (or its verification) refused for that reason.
    pub binding_change: Option<Box<plakous::agent::BindingChange>>,
}

impl From<CliFailure> for CodingSessionOpenFailure {
    fn from(failure: CliFailure) -> Self {
        Self {
            failure: Box::new(failure),
            current_locator: None,
            binding_change: None,
        }
    }
}

/// What one build/open call does with the runtime its source compiles.
#[derive(Clone, Copy, Debug)]
pub(in crate::agent) enum RuntimeTarget {
    /// Create a new session.
    Create,
    /// Open this exact head and attach it.
    Open(SessionLocator),
    /// Answer whether opening this exact head would attach, without owning
    /// it: nothing is attached, recovered or reattached.
    Verify(SessionLocator),
}

impl RuntimeTarget {
    const fn locator(self) -> Option<SessionLocator> {
        match self {
            Self::Create => None,
            Self::Open(locator) | Self::Verify(locator) => Some(locator),
        }
    }

    const fn verifies(self) -> bool {
        matches!(self, Self::Verify(_))
    }

    /// How a root compile under this target reaches its agent tree: an
    /// attaching open owns the hub, and a verification compiles over a
    /// detached copy that it never inserts, leases or saves.
    pub(in crate::agent) const fn root_topology_attach(
        self,
    ) -> crate::topology_runtime::TopologyHubAttach {
        if self.verifies() {
            crate::topology_runtime::TopologyHubAttach::Inspect
        } else {
            crate::topology_runtime::TopologyHubAttach::AttachRoot
        }
    }
}

impl From<Option<SessionLocator>> for RuntimeTarget {
    fn from(locator: Option<SessionLocator>) -> Self {
        locator.map_or(Self::Create, Self::Open)
    }
}

impl From<AgentSessionsError> for CodingSessionOpenFailure {
    fn from(error: AgentSessionsError) -> Self {
        let current_locator = error.current_locator();
        let binding_change = error.binding_change().cloned().map(Box::new);
        let failure = if error.kind() == AgentSessionsErrorKind::IncompatibleBuild {
            CliFailure::new(incompatible_session_message(&error))
                .with_optional_advice(crate::error::incompatible_session_advice(&error))
        } else {
            CliFailure::from(error)
        };
        Self {
            failure: Box::new(failure),
            current_locator,
            binding_change,
        }
    }
}

/// An attachment release can fail in its children, its canonical parent, or both.
/// A successfully released parent locator remains available for publication.
#[derive(Debug)]
pub struct AttachedReleaseError {
    released_locator: Option<SessionLocator>,
    topology: Option<crate::topology_runtime::TopologyShutdownError>,
    canonical: Option<AgentSessionsError>,
}

impl AttachedReleaseError {
    /// The parent's released head, even when owned child shutdown failed.
    pub fn released_locator(&self) -> Option<SessionLocator> {
        self.released_locator
    }
}

impl std::fmt::Display for AttachedReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = &self.topology {
            write!(f, "{error}")?;
        }
        if let Some(error) = &self.canonical {
            if self.topology.is_some() {
                write!(f, "; ")?;
            }
            write!(f, "canonical session release failed: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AttachedReleaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.canonical
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
            .or_else(|| {
                self.topology
                    .as_ref()
                    .map(|error| error as &(dyn std::error::Error + 'static))
            })
    }
}

/// The one line a reopen refusal gives an operator.
///
/// A holder this host can prove is gone has its branch reclaimed at once, so
/// reaching here means the opposite: the holder is alive, or it is somewhere
/// this process cannot speak for, and the lease's own clock is the only
/// remaining answer. Without saying so the refusal reads as a broken session
/// and the user stops trying, when the remedy is to wait. The lease is the
/// store's, so the wait is an upper bound rather than a countdown: the holder
/// renews for at most that long.
pub(in crate::agent) fn reopen_refusal(
    reason: &str,
    retry_after_seconds: Option<u64>,
) -> CliFailure {
    retry_after_seconds.map_or_else(
        || CliFailure::recovery_required(reason.to_owned()),
        |seconds| {
            CliFailure::recovery_required(format!(
                "{reason}: this session's branch is held by another plak process that \
                 could not be confirmed gone; retry in about {seconds} seconds"
            ))
        },
    )
}

/// One live coding session together with the exact public workflow service
/// that created or reopened it.
///
/// Retaining `AgentSessions` is required for driver-owned compact/fork/rebind
/// workflows. This wrapper composes that public authority; it does not own a
/// second agent loop or persistence implementation.
pub struct AttachedCodingSession {
    pub(in crate::agent) reachability_probe: Option<plakous::model::ReachabilityProbe>,
    /// Set when this reopen kept a budget the route has since moved. Reported
    /// once at session open, never during a turn.
    pub(crate) output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
    pub(in crate::agent) sessions: Arc<AgentSessions>,
    pub(in crate::agent) opened: Option<OpenedSession>,
    pub(in crate::agent) recovered_turn: Option<RecoveredTurnHandle>,
    pub(in crate::agent) locator: SessionLocator,
    /// Tree id when this session holds a root hub lease. Child/mail sessions
    /// never own the hub; their `release` must not drop the working copy.
    pub(in crate::agent) topology_owner: Option<String>,
}

impl AttachedCodingSession {
    pub(crate) fn replay_validation_error(&self) -> Option<&plakous::agent::AgentError> {
        self.opened
            .as_ref()
            .and_then(OpenedSession::replay_validation_error)
    }

    pub(crate) fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        self.reachability_probe.clone()
    }

    #[cfg(test)]
    pub(crate) fn opened_for_host_tests(
        sessions: Arc<AgentSessions>,
        session: OpenedSession,
    ) -> Self {
        Self {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn claim_registered_topology_owner(&mut self, tree_id: &str) {
        self.topology_owner = claim_root_topology_owner(tree_id, true);
    }

    pub fn agent(&self) -> Option<&Agent> {
        self.opened.as_ref().map(OpenedSession::agent)
    }

    /// Unix milliseconds of the newest operator prompt or steer in the
    /// attached session's committed history; `None` when detached.
    pub(crate) fn last_operator_input_unix_ms(&self) -> Option<u64> {
        self.opened
            .as_ref()
            .and_then(OpenedSession::last_operator_input_unix_ms)
    }

    /// Whether the attached session's newest finalized provider call is a
    /// fold that failed after dispatch; false when detached.
    pub(crate) fn paid_fold_failed_since_last_round(&self) -> bool {
        self.opened
            .as_ref()
            .is_some_and(OpenedSession::paid_fold_failed_since_last_round)
    }

    /// Whether the attached session's committed history holds at least the
    /// segment count at which the next round is refused; false when detached.
    pub(crate) fn segment_limit_reached(&self) -> bool {
        self.opened
            .as_ref()
            .is_some_and(OpenedSession::segment_limit_reached)
    }

    pub fn locator(&self) -> SessionLocator {
        self.opened
            .as_ref()
            .map_or(self.locator, OpenedSession::locator)
    }

    #[must_use]
    pub fn sessions(&self) -> &AgentSessions {
        self.sessions.as_ref()
    }

    pub(crate) fn close_admission(&self) {
        if let Some(opened) = &self.opened {
            opened.close_admission();
        }
    }

    pub(crate) fn shared_sessions(&self) -> Arc<AgentSessions> {
        Arc::clone(&self.sessions)
    }

    pub(crate) fn schedule_passive_checkpoint(&self, tasks: &crate::TaskSet) {
        let sessions = Arc::clone(&self.sessions);
        tasks.spawn(async move {
            let _ = sessions.passive_checkpoint().await;
        });
    }

    /// Takes the sole result/control authority for a root turn restored by
    /// exact open. A caller must settle this turn before admitting another.
    pub(crate) fn take_recovered_turn(&mut self) -> Option<RecoveredTurnHandle> {
        self.recovered_turn.take()
    }

    pub(crate) const fn has_recovered_turn(&self) -> bool {
        self.recovered_turn.is_some()
    }

    pub(crate) async fn create_with_runtime(
        &self,
        operation_id: SessionOperationId,
        runtime: SessionRuntimeBinding,
    ) -> Result<Self, CodingSessionOpenFailure> {
        match self
            .sessions
            .create(CreateSessionRequest::new(operation_id, runtime))
            .await
            .map_err(|error| {
                let raw = format!(
                    "agent could not be initialized ({:?}): {error}",
                    error.kind()
                );
                CliFailure::new(
                    crate::storage_pressure::sessions_storage_pressure(&error, &raw).unwrap_or(raw),
                )
                .with_optional_remedy(crate::storage_pressure::sessions_remedy(&error).as_ref())
            })? {
            CreateSessionOutcome::Created { session, .. } => {
                let locator = session.locator();
                Ok(Self {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&self.sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                })
            }
            _ => Err(CliFailure::new("agent session creation was unavailable").into()),
        }
    }

    pub(crate) fn clear_job_counter(
        &self,
        runtime: SessionRuntimeBinding,
    ) -> CliResult<SessionRuntimeBinding> {
        let source = self
            .agent()
            .ok_or_else(|| CliFailure::new("clear source agent is unavailable"))?;
        runtime.with_job_counter(source).map_err(|error| {
            CliFailure::new(format!("clear job counter could not be seeded: {error}"))
        })
    }

    /// Forks onto `runtime`'s compilation of this binding (current
    /// instruction files, the source's history).
    pub async fn fork(
        &self,
        operation_id: SessionOperationId,
        runtime: SessionRuntimeBinding,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        Box::pin(self.sessions.fork(ForkSessionRequest::recompiling(
            operation_id,
            self.locator(),
            runtime,
        )))
        .await
    }

    /// An exact ordinary fork of `source`; product forks recompile
    /// ([`Self::fork`]).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn fork_from(
        &self,
        operation_id: SessionOperationId,
        source: SessionLocator,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        self.sessions
            .fork(ForkSessionRequest::new(operation_id, source))
            .await
    }

    pub async fn compact_from(
        &self,
        operation_id: SessionOperationId,
        source: SessionLocator,
        reason: CompactReason,
        pending_user_input: Option<plakous::agent::UserInput>,
        runtime: SessionRuntimeBinding,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        self.sessions
            .compact({
                let request = CompactSessionRequest::new(operation_id, source, reason, runtime);
                match pending_user_input {
                    Some(input) => request.with_pending_user_input(input),
                    None => request,
                }
            })
            .await
    }

    pub async fn rebind_from(
        &self,
        operation_id: SessionOperationId,
        source: SessionLocator,
        target: SessionTargetBinding,
        runtime: SessionRuntimeBinding,
    ) -> Result<RebindSessionOutcome, AgentSessionsError> {
        Box::pin(
            self.sessions
                .rebind(RebindSessionRequest::recompiling_target(
                    operation_id,
                    source,
                    target,
                    runtime,
                )),
        )
        .await
    }

    pub async fn context_status(
        &self,
        operation_id: SessionOperationId,
        query: ContextQuery,
    ) -> Result<SessionContextStatus, AgentSessionsError> {
        self.sessions
            .context_status(ContextStatusRequest::new(
                operation_id,
                self.locator(),
                query,
            ))
            .await
    }

    #[allow(
        clippy::result_large_err,
        reason = "the error carries the released locator and both lease failures; it is returned once at session teardown"
    )]
    pub async fn release(mut self) -> Result<SessionLocator, AttachedReleaseError> {
        let topology = match self.topology_owner.take() {
            Some(tree_id) => crate::topology_runtime::release_root_lease(&tree_id)
                .await
                .err(),
            None => None,
        };
        // A child's failure cannot retain the parent's canonical lease.
        match self.release_opened().await {
            Ok(locator) if topology.is_none() => Ok(locator),
            Ok(locator) => Err(AttachedReleaseError {
                released_locator: Some(locator),
                topology,
                canonical: None,
            }),
            Err(canonical) => Err(AttachedReleaseError {
                released_locator: canonical.released_locator(),
                topology,
                canonical: Some(canonical),
            }),
        }
    }

    /// Process-exit release: no hub retirement, product saves or driver join.
    #[allow(
        clippy::result_large_err,
        reason = "the error carries the released locator and both lease failures; it is returned once at process exit"
    )]
    pub async fn abandon_and_release(
        mut self,
        job_budget: Duration,
    ) -> Result<SessionLocator, AttachedReleaseError> {
        let peers = match self.topology_owner.take() {
            Some(tree_id) => crate::topology_runtime::abandon_live_agents(&tree_id).await,
            None => Vec::new(),
        };
        match self.opened.take() {
            Some(opened) => opened
                .abandon_and_release(&peers, job_budget)
                .await
                .map_err(|canonical| AttachedReleaseError {
                    released_locator: canonical.released_locator(),
                    topology: None,
                    canonical: Some(canonical),
                }),
            None => Ok(self.locator),
        }
    }

    /// Takes over a released predecessor's agent-tree lease when this attach
    /// did not take one of its own, so replacing one attachment of a session
    /// with another never drops the tree between them.
    pub(crate) fn inherit_topology_owner(&mut self, previous: &mut Self) {
        if self.topology_owner.is_none() {
            self.topology_owner = previous.topology_owner.take();
        }
    }

    pub(crate) async fn release_opened(mut self) -> Result<SessionLocator, AgentSessionsError> {
        match self.opened.take() {
            Some(opened) => opened.release().await,
            None => Ok(self.locator),
        }
    }

    pub(crate) async fn release_opened_for_transfer(
        mut self,
    ) -> Result<SessionLocator, AgentSessionsError> {
        match self.opened.take() {
            Some(opened) => opened.release_for_transfer().await,
            None => Ok(self.locator),
        }
    }

    /// Moves this attachment out for its release, leaving an inert copy in
    /// its place that owns no driver, lease or recovered turn.
    pub(crate) fn take_for_release(&mut self) -> Self {
        let inert = Self {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: Arc::clone(&self.sessions),
            opened: None,
            recovered_turn: None,
            locator: self.locator,
            topology_owner: None,
        };
        std::mem::replace(self, inert)
    }

    pub(crate) async fn detach_for_transfer(
        &mut self,
    ) -> Result<SessionLocator, AgentSessionsError> {
        let Some(opened) = self.opened.take() else {
            return Ok(self.locator);
        };
        self.locator = opened.locator();
        let locator = opened.release_for_transfer().await?;
        self.locator = locator;
        Ok(locator)
    }

    /// Quiesces the live driver and releases its source lease while retaining
    /// the session workflow service needed to publish a canonical branch.
    pub async fn detach(&mut self) -> Result<SessionLocator, AgentSessionsError> {
        let Some(opened) = self.opened.take() else {
            return Ok(self.locator);
        };
        self.locator = opened.locator();
        let locator = opened.release().await?;
        self.locator = locator;
        Ok(locator)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn stub_for_host_tests(sessions: Arc<AgentSessions>, locator: SessionLocator) -> Self {
        Self {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            opened: None,
            recovered_turn: None,
            locator,
            topology_owner: None,
        }
    }
}

/// A carried history and its once-only input-window preflight, passed through the runtime factory.
pub struct SeededHistory {
    pub(crate) history: PortableSessionHistory,
    preflight:
        Option<Box<dyn FnOnce(plakous::sessions::SessionInputWindow) -> CliResult<()> + Send>>,
}

impl SeededHistory {
    pub(crate) fn new(history: PortableSessionHistory) -> Self {
        Self {
            history,
            preflight: None,
        }
    }
    pub(crate) fn with_preflight(
        history: PortableSessionHistory,
        preflight: impl FnOnce(plakous::sessions::SessionInputWindow) -> CliResult<()> + Send + 'static,
    ) -> Self {
        Self {
            history,
            preflight: Some(Box::new(preflight)),
        }
    }
    pub(crate) fn inspect(&mut self, runtime: &SessionRuntimeBinding) -> CliResult<()> {
        if let Some(preflight) = self.preflight.take() {
            preflight(runtime.input_window())?;
        }
        Ok(())
    }
}

/// Where [`build_or_open`] gets the runtime it creates or opens a session
/// over.
pub(super) enum SessionSource<'a> {
    /// A native product spec: its runtime is compiled here and the root
    /// topology attached.
    Native {
        spec: Box<ProductAgentSpec>,
        stderr: &'a mut (dyn Write + Send),
    },
    /// An API topology parent: its runtime is compiled here and its topology
    /// attached.
    Api {
        parent: Box<ApiTopologyParent>,
        topology_attach: crate::topology_runtime::TopologyHubAttach,
        cached_evidence: Option<Box<crate::openrouter::OpenRouterChatModelEvidence>>,
        tool_fence: SessionToolFence,
    },
    /// A runtime binding the caller already sealed.
    Runtime {
        sessions: Arc<AgentSessions>,
        runtime: Box<SessionRuntimeBinding>,
        topology_owner: Option<String>,
        output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
        wording: &'a RuntimeWording,
    },
}

/// What a create or open over a sealed runtime checks and how it words its
/// refusals: the product's, or an injected test runtime's.
pub(super) struct RuntimeWording {
    /// Whether a seed is inspected against the runtime and the session's tool
    /// projection is required. Only an injected test runtime is unchecked;
    /// its seeds never carry a preflight.
    pub(super) checked: bool,
    pub(super) reopen_failed: fn(&AgentSessionsError) -> String,
    pub(super) reopen_needs_recovery: &'static str,
    pub(super) reopen_unavailable: &'static str,
    pub(super) create_failed: &'static str,
    pub(super) create_unavailable: &'static str,
}

const PRODUCT_WORDING: RuntimeWording = RuntimeWording {
    checked: true,
    reopen_failed: product_reopen_failed,
    reopen_needs_recovery: "exact session reopen requires explicit recovery; no new turn was admitted",
    reopen_unavailable: "exact session reopen was unavailable",
    create_failed: "agent could not be initialized",
    create_unavailable: "agent session creation was unavailable",
};

fn product_reopen_failed(error: &AgentSessionsError) -> String {
    format_session_reopen_error("exact session reopen failed", error)
}

/// Creates a session (no locator) or opens the one `locator` names, over
/// the runtime `source` yields. The one build/open entry point.
pub(super) async fn build_or_open(
    source: SessionSource<'_>,
    target: RuntimeTarget,
    seed: Option<SeededHistory>,
    operation_id: SessionOperationId,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    match source {
        SessionSource::Native { spec, stderr } => {
            Box::pin(attach_native_source(
                *spec,
                target,
                seed,
                operation_id,
                stderr,
            ))
            .await
        }
        SessionSource::Api {
            parent,
            topology_attach,
            cached_evidence,
            tool_fence,
        } => {
            Box::pin(attach_api_source(
                *parent,
                target,
                seed,
                operation_id,
                topology_attach,
                cached_evidence.map(|evidence| *evidence),
                tool_fence,
            ))
            .await
        }
        SessionSource::Runtime {
            sessions,
            runtime,
            topology_owner,
            output_bound_rederived,
            wording,
        } => {
            Box::pin(attach_runtime_source(
                sessions,
                *runtime,
                target,
                seed,
                operation_id,
                topology_owner,
                output_bound_rederived,
                wording,
            ))
            .await
        }
    }
}

/// Builds and returns the attached public Plakous session authority.
///
/// Product code that persists or later reopens a conversation must retain this
/// handle while live and persist its current locator.
pub async fn build_coding_session(
    spec: ProductAgentSpec,
    create_operation_id: &str,
    seed: Option<PortableSessionHistory>,
    stderr: &mut (dyn Write + Send),
) -> CliResult<AttachedCodingSession> {
    build_coding_session_seed(
        spec,
        create_operation_id,
        seed.map(SeededHistory::new),
        stderr,
    )
    .await
}

pub(crate) async fn build_coding_session_seed(
    spec: ProductAgentSpec,
    create_operation_id: &str,
    seed: Option<SeededHistory>,
    stderr: &mut (dyn Write + Send),
) -> CliResult<AttachedCodingSession> {
    let operation_id = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("plak create operation identity is invalid"))?;
    Box::pin(build_or_open(
        SessionSource::Native {
            spec: Box::new(spec),
            stderr,
        },
        RuntimeTarget::Create,
        seed,
        operation_id,
    ))
    .await
    .map_err(|error| *error.failure)
}

/// Reopens one exact product-bound Plakous session without substituting current
/// defaults for its persisted runtime authority.
pub async fn open_coding_session(
    spec: ProductAgentSpec,
    locator: SessionLocator,
    stderr: &mut (dyn Write + Send),
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let operation_id = SessionOperationId::new(format!("plak:open:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("plak open operation identity is invalid"))?;
    Box::pin(build_or_open(
        SessionSource::Native {
            spec: Box::new(spec),
            stderr,
        },
        RuntimeTarget::Open(locator),
        None,
        operation_id,
    ))
    .await
}

/// Creates a durable product session from an already sealed public Plakous
/// runtime binding. This is the provider-neutral lane used by API-key dialects.
pub async fn build_runtime_session(
    state_roots: &ProductStateRoots,
    runtime: SessionRuntimeBinding,
    create_operation_id: &str,
) -> CliResult<AttachedCodingSession> {
    let operation_id = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("plak create operation identity is invalid"))?;
    Box::pin(build_or_open(
        SessionSource::Runtime {
            sessions: state_roots.runtime_sessions()?,
            runtime: Box::new(runtime),
            topology_owner: None,
            output_bound_rederived: None,
            wording: &PRODUCT_WORDING,
        },
        RuntimeTarget::Create,
        None,
        operation_id,
    ))
    .await
    .map_err(|error| *error.failure)
}

/// Answers whether [`open_coding_session`] of `locator` would attach,
/// compiling the same runtime without owning, attaching or recovering it.
pub(crate) async fn verify_coding_session(
    spec: ProductAgentSpec,
    locator: SessionLocator,
    stderr: &mut (dyn Write + Send),
) -> Result<(), CodingSessionOpenFailure> {
    let operation_id = SessionOperationId::new(format!("plak:verify:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("plak verify operation identity is invalid"))?;
    Box::pin(build_or_open(
        SessionSource::Native {
            spec: Box::new(spec),
            stderr,
        },
        RuntimeTarget::Verify(locator),
        None,
        operation_id,
    ))
    .await
    .map(drop)
}

/// The API-route twin of [`verify_coding_session`].
pub(crate) async fn verify_api_coding_session(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
) -> Result<(), CodingSessionOpenFailure> {
    let operation_id = SessionOperationId::new(format!("plak:verify:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("plak verify operation identity is invalid"))?;
    let target = RuntimeTarget::Verify(locator);
    Box::pin(build_or_open(
        SessionSource::Api {
            parent: Box::new(parent),
            topology_attach: target.root_topology_attach(),
            cached_evidence: cached_evidence.map(Box::new),
            tool_fence: SessionToolFence::Full,
        },
        target,
        None,
        operation_id,
    ))
    .await
    .map(drop)
}

async fn attach_native_source(
    mut spec: ProductAgentSpec,
    target: RuntimeTarget,
    seed: Option<SeededHistory>,
    operation_id: SessionOperationId,
    stderr: &mut (dyn Write + Send),
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let sessions = spec
        .state_roots
        .runtime_sessions()
        .map_err(CodingSessionOpenFailure::from)?;
    // Attachment catalogs are identity-bearing, so restore bounds before compilation.
    if let Some(locator) = target.locator() {
        spec.state_roots = spec
            .state_roots
            .for_exact_reopen(&sessions, locator)
            .await?;
    } else {
        spec.state_roots = spec.state_roots.without_reopen_artifact();
    }
    let tree_id = topology_tree_id(&spec).to_owned();
    let is_root = spec.topology_caller_id.is_none();
    let reattach_spec = spec.clone();
    let reattach_parent = SpawnParent::from_native(reattach_spec.clone());
    if target.verifies() {
        let runtime = coding_session_runtime_binding_for(
            spec,
            stderr,
            true,
            SessionToolFence::Full,
            target.root_topology_attach(),
            &mut None,
        )
        .await?;
        return attach_runtime_source(
            sessions,
            runtime,
            target,
            seed,
            operation_id,
            None,
            None,
            &PRODUCT_WORDING,
        )
        .await;
    }
    let runtime = match coding_session_runtime_binding_for(
        spec,
        stderr,
        target.locator().is_some(),
        SessionToolFence::Full,
        target.root_topology_attach(),
        &mut None,
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Err(cleanup) = crate::topology_runtime::release_root_lease(&tree_id).await {
                return Err(CodingSessionOpenFailure::from(
                    error.with_cleanup_failure(cleanup),
                ));
            }
            return Err(CodingSessionOpenFailure::from(error));
        }
    };
    let topology_owner = claim_root_topology_owner(&tree_id, is_root);
    if let Some(topology) = crate::topology_runtime::lookup(&tree_id) {
        topology.remember_rejoin_spec(Arc::new(reattach_parent.clone()));
        crate::agent::recovery::install_retirement_recovery(&topology, &tree_id).await;
    }
    // A reattach recompiles nothing, so it has no moved budget to report.
    let result = attach_runtime_source(
        sessions,
        runtime,
        target,
        seed,
        operation_id,
        topology_owner,
        None,
        &PRODUCT_WORDING,
    )
    .await;
    if result.is_err() {
        if let Err(cleanup) = crate::topology_runtime::release_root_lease(&tree_id).await {
            return result.map_err(|error| CodingSessionOpenFailure {
                failure: Box::new(error.failure.with_cleanup_failure(cleanup)),
                current_locator: error.current_locator,
                binding_change: error.binding_change,
            });
        }
        return result;
    }
    if is_root && let Some(topology) = crate::topology_runtime::lookup(&tree_id) {
        reattach_inflight_children(&reattach_parent, &topology).await;
    }
    result
}

pub async fn build_api_root_coding_session(
    parent: ApiTopologyParent,
    create_operation_id: &str,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<AttachedCodingSession> {
    Box::pin(build_api_coding_session(
        parent,
        create_operation_id,
        None,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        cached_evidence,
    ))
    .await
}

pub(crate) async fn build_api_coding_session(
    parent: ApiTopologyParent,
    create_operation_id: &str,
    seed: Option<PortableSessionHistory>,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<AttachedCodingSession> {
    build_api_coding_session_seed(
        parent,
        create_operation_id,
        seed.map(SeededHistory::new),
        topology_attach,
        cached_evidence,
    )
    .await
}

pub(crate) async fn build_api_coding_session_seed(
    parent: ApiTopologyParent,
    create_operation_id: &str,
    seed: Option<SeededHistory>,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<AttachedCodingSession> {
    let operation_id = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("plak create operation identity is invalid"))?;
    Box::pin(build_or_open(
        SessionSource::Api {
            parent: Box::new(parent),
            topology_attach,
            cached_evidence: cached_evidence.map(Box::new),
            tool_fence: SessionToolFence::Full,
        },
        RuntimeTarget::Create,
        seed,
        operation_id,
    ))
    .await
    .map_err(|error| *error.failure)
}

pub async fn open_api_root_coding_session(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    Box::pin(open_api_coding_session(
        parent,
        locator,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        cached_evidence,
    ))
    .await
}

pub(crate) async fn open_api_coding_session(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    Box::pin(open_api_coding_session_with_fence(
        parent,
        locator,
        topology_attach,
        cached_evidence,
        SessionToolFence::Full,
    ))
    .await
}

pub(crate) async fn open_api_coding_session_with_fence(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
    tool_fence: SessionToolFence,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let operation_id = SessionOperationId::new(format!("plak:open:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("plak open operation identity is invalid"))?;
    Box::pin(build_or_open(
        SessionSource::Api {
            parent: Box::new(parent),
            topology_attach,
            cached_evidence: cached_evidence.map(Box::new),
            tool_fence,
        },
        RuntimeTarget::Open(locator),
        None,
        operation_id,
    ))
    .await
}

async fn attach_api_source(
    parent: ApiTopologyParent,
    target: RuntimeTarget,
    seed: Option<SeededHistory>,
    operation_id: SessionOperationId,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<crate::openrouter::OpenRouterChatModelEvidence>,
    tool_fence: SessionToolFence,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let tree_id = parent
        .topology_tree_id
        .as_deref()
        .unwrap_or(parent.plan_state_authority.as_str())
        .to_owned();
    let is_root = parent.topology_caller_id.is_none();
    if parent.product_home.is_none() {
        return Err(CliFailure::new("plak product home is unavailable").into());
    }
    let sessions = parent
        .state_roots
        .runtime_sessions()
        .map_err(CodingSessionOpenFailure::from)?;
    // Read before anything is compiled: the configuration a reopen builds has
    // to carry the process budgets and output bound the session was created
    // under, not today's product policy or route attestation.
    //
    // This read opens the very branch the open below opens, so it is the first
    // place a locator that lags the durable head is noticed. The typed head has
    // to survive: the caller's stale-head adoption retries with it, and a bare
    // failure here would strand every API session whose record lagged, which is
    // the one case adoption exists for.
    let mut parent = parent;
    if let Some(locator) = target.locator() {
        parent.state_roots = parent
            .state_roots
            .for_exact_reopen(&sessions, locator)
            .await?;
        parent.created_output_bound = Some(
            sessions
                .created_request_output_bound(locator)
                .await
                .map_err(CodingSessionOpenFailure::from)?,
        );
    } else {
        parent.state_roots = parent.state_roots.without_reopen_artifact();
    }
    let reattach_parent = SpawnParent::Api(parent.clone());
    let mut topology_owner = None;
    // Set only on the success arm, which is the only arm that compiled anything.
    let output_bound_rederived;
    let runtime = match Box::pin(api_session_runtime_binding_with_owner(
        &parent,
        target.locator().is_some(),
        tool_fence,
        topology_attach,
        cached_evidence,
        &mut topology_owner,
    ))
    .await
    {
        Ok(prepared) => {
            output_bound_rederived = prepared.output_bound_rederived;
            prepared.runtime
        }
        Err(error) => {
            if let Some(acquired) = topology_owner.take() {
                if let Err(cleanup) = crate::topology_runtime::release_root_lease(&acquired).await {
                    return Err(CodingSessionOpenFailure::from(
                        error.with_cleanup_failure(cleanup),
                    ));
                }
            }
            return Err(CodingSessionOpenFailure::from(error));
        }
    };
    let acquired_owner = topology_owner.clone();
    let result = attach_runtime_source(
        sessions,
        runtime,
        target,
        seed,
        operation_id,
        topology_owner,
        output_bound_rederived,
        &PRODUCT_WORDING,
    )
    .await;
    if result.is_err() || target.verifies() {
        if let Some(acquired) = acquired_owner {
            if let Err(cleanup) = crate::topology_runtime::release_root_lease(&acquired).await {
                return result.map_err(|error| CodingSessionOpenFailure {
                    failure: Box::new(error.failure.with_cleanup_failure(cleanup)),
                    current_locator: error.current_locator,
                    binding_change: error.binding_change,
                });
            }
        }
        return result;
    }
    if topology_attach == crate::topology_runtime::TopologyHubAttach::AttachRoot
        && let Some(topology) = crate::topology_runtime::lookup(&tree_id)
    {
        topology.remember_rejoin_spec(Arc::new(reattach_parent.clone()));
        crate::agent::recovery::install_retirement_recovery(&topology, &tree_id).await;
    }
    if is_root
        && topology_attach == crate::topology_runtime::TopologyHubAttach::AttachRoot
        && let Some(topology) = crate::topology_runtime::lookup(&tree_id)
    {
        reattach_inflight_children(&reattach_parent, &topology).await;
    }
    result
}

#[allow(
    clippy::too_many_arguments,
    reason = "Takes the session store, runtime binding, optional locator and seed history, operation id, topology owner, rederived output bound, and runtime wording that one source attachment needs."
)]
async fn attach_runtime_source(
    sessions: Arc<AgentSessions>,
    runtime: SessionRuntimeBinding,
    target: RuntimeTarget,
    mut seed: Option<SeededHistory>,
    operation_id: SessionOperationId,
    topology_owner: Option<String>,
    // Set only by a reopen whose route has moved its output budget; carried to
    // the attached session so it is reported once, at open.
    output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
    wording: &RuntimeWording,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    if wording.checked
        && let Some(seed) = seed.as_mut()
    {
        seed.inspect(&runtime)?;
    }
    let reachability_probe = plakous::sessions::internal::reachability_probe(&runtime);
    let reopen_failure = |error: AgentSessionsError| CodingSessionOpenFailure {
        failure: Box::new(
            CliFailure::new((wording.reopen_failed)(&error))
                .with_attach_failure(crate::error::sessions_attach_failure(&error)),
        ),
        current_locator: error.current_locator(),
        binding_change: error.binding_change().cloned().map(Box::new),
    };
    if let RuntimeTarget::Verify(locator) = target {
        if seed.is_some() {
            return Err(CliFailure::new("a carried history seeds only a new session").into());
        }
        return match Box::pin(sessions.verify_open(locator, runtime))
            .await
            .map_err(reopen_failure)?
        {
            plakous::sessions::OpenVerification::Opens => Ok(AttachedCodingSession {
                output_bound_rederived,
                reachability_probe,
                sessions: Arc::clone(&sessions),
                opened: None,
                recovered_turn: None,
                locator,
                topology_owner,
            }),
            _ => Err(reopen_refusal(wording.reopen_needs_recovery, None).into()),
        };
    }
    if let Some(locator) = target.locator() {
        if seed.is_some() {
            return Err(CliFailure::new("a carried history seeds only a new session").into());
        }
        let opened =
            Box::pin(sessions.open(OpenSessionRequest::new(operation_id, locator, runtime)))
                .await
                .map_err(reopen_failure)?;
        let recovered = classify_opened(&opened);
        return match opened {
            OpenSessionOutcome::Opened { session, .. }
            | OpenSessionOutcome::RestoredWorking { session, .. } => {
                if wording.checked {
                    require_reconstructed_coding_projection(&session)?;
                }
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: recovered,
                    locator,
                    topology_owner,
                })
            }
            OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds,
                ..
            } => Err(reopen_refusal(wording.reopen_needs_recovery, retry_after_seconds).into()),
            _ => Err(CliFailure::new(wording.reopen_unavailable).into()),
        };
    }
    match Box::pin(sessions.create(create_request(operation_id, runtime, seed)))
        .await
        .map_err(|error| {
            CliFailure::new(format!(
                "{} ({:?}): {error}",
                wording.create_failed,
                error.kind()
            ))
        })? {
        CreateSessionOutcome::Created { session, .. } => {
            if wording.checked {
                require_reconstructed_coding_projection(&session)?;
            }
            let locator = session.locator();
            Ok(AttachedCodingSession {
                output_bound_rederived,
                reachability_probe: reachability_probe.clone(),
                sessions: Arc::clone(&sessions),
                opened: Some(session),
                recovered_turn: None,
                locator,
                topology_owner,
            })
        }
        _ => Err(CliFailure::new(wording.create_unavailable).into()),
    }
}

fn create_request(
    operation_id: SessionOperationId,
    runtime: SessionRuntimeBinding,
    seed: Option<SeededHistory>,
) -> CreateSessionRequest {
    let request = CreateSessionRequest::new(operation_id, runtime);
    match seed {
        Some(history) => request.with_history(history.history),
        None => request,
    }
}

pub(in crate::agent) fn require_reconstructed_coding_projection(
    session: &OpenedSession,
) -> Result<(), CodingSessionOpenFailure> {
    if session
        .agent()
        .stable_request_prefix()
        .tool_projection()
        .is_none()
    {
        return Err(CliFailure::new(format!(
            "session {} was created by an incompatible plak build; start a new session",
            session.locator().session_id()
        ))
        .into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::RuntimeTarget;
    use crate::topology_runtime::TopologyHubAttach;

    /// Both the native and the API root compile under a verification reach
    /// their tree as an inspection; only an open or create attaches it.
    #[test]
    fn only_a_verification_compiles_over_a_detached_tree() {
        fn id<T: std::str::FromStr>() -> T
        where
            T::Err: std::fmt::Debug,
        {
            uuid::Uuid::new_v4().to_string().parse().unwrap()
        }
        let locator = plakous::sessions::SessionLocator::new(id(), id(), id());
        assert_eq!(
            RuntimeTarget::Verify(locator).root_topology_attach(),
            TopologyHubAttach::Inspect
        );
        for target in [RuntimeTarget::Open(locator), RuntimeTarget::Create] {
            assert_eq!(target.root_topology_attach(), TopologyHubAttach::AttachRoot);
        }
    }
}
