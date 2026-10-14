//! Application-owned, opt-in, content-incapable telemetry observations.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous_model::{
    AgentId, AttemptId, BranchId, CallId, RunId, SessionId, Timestamp, TurnId, Usage,
};
use thiserror::Error;

use crate::{CompactionEventOutcome, CompactionEventReason, TurnUsage};

/// Content-free classification of an exact compaction endpoint refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionRefusalReason {
    /// Actor ingress was full.
    Backpressure,
    /// The source had unsealed working content.
    Busy,
    /// The source checkpoint was no longer current.
    Stale,
    /// Preparation produced no inspectable status.
    Unavailable,
    /// The request failed canonical validation.
    InvalidInput,
    /// Preparation or the successor ladder rejected the source.
    Rejected,
}

impl CompactionRefusalReason {
    /// Stable content-free storage and telemetry label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backpressure => "backpressure",
            Self::Busy => "busy",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::InvalidInput => "invalid_input",
            Self::Rejected => "rejected",
        }
    }
}

/// Nonblocking application telemetry admission failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum TelemetrySinkError {
    /// The application's bounded telemetry queue has no capacity.
    #[error("telemetry sink is full")]
    Full,
    /// The application telemetry sink is permanently closed.
    #[error("telemetry sink is closed")]
    Closed,
}

/// Object-safe, application-owned nonblocking telemetry port.
pub trait TelemetrySink: Send + Sync + 'static {
    /// Attempts to accept one content-free observation without blocking the agent.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetrySinkError::Full`] for transient bounded pressure or
    /// [`TelemetrySinkError::Closed`] when no later observation can be accepted.
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError>;

    /// Observes a transient model retry for application logging, independently
    /// of the bounded analytics queue. Must not block or retain request content.
    /// The default sink does not log retries.
    fn observe_model_retry(&self, _context: TelemetryContext, _retry: &plakous_model::ModelRetry) {}
}

/// Typed correlation context for a telemetry observation.
#[allow(
    clippy::struct_field_names,
    reason = "the ID suffix distinguishes every explicit causal identity"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryContext {
    agent_id: AgentId,
    session_id: SessionId,
    branch_id: BranchId,
    run_id: RunId,
    turn_id: Option<TurnId>,
    attempt_id: Option<AttemptId>,
    call_id: Option<CallId>,
}

impl TelemetryContext {
    /// Creates an agent-scoped context.
    #[must_use]
    pub const fn new(
        agent_id: AgentId,
        session_id: SessionId,
        branch_id: BranchId,
        run_id: RunId,
    ) -> Self {
        Self {
            agent_id,
            session_id,
            branch_id,
            run_id,
            turn_id: None,
            attempt_id: None,
            call_id: None,
        }
    }

    /// Binds an exact logical turn.
    #[must_use]
    pub const fn with_turn(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    /// Binds an exact model attempt.
    #[must_use]
    pub const fn with_attempt(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = Some(attempt_id);
        self
    }

    /// Binds an exact tool call.
    #[must_use]
    pub const fn with_call(mut self, call_id: CallId) -> Self {
        self.call_id = Some(call_id);
        self
    }

    /// Returns the agent identity.
    #[must_use]
    pub const fn agent_id(self) -> AgentId {
        self.agent_id
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the branch identity.
    #[must_use]
    pub const fn branch_id(self) -> BranchId {
        self.branch_id
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(self) -> RunId {
        self.run_id
    }

    /// Returns the turn identity when applicable.
    #[must_use]
    pub const fn turn_id(self) -> Option<TurnId> {
        self.turn_id
    }

    /// Returns the model-attempt identity when applicable.
    #[must_use]
    pub const fn attempt_id(self) -> Option<AttemptId> {
        self.attempt_id
    }

    /// Returns the tool-call identity when applicable.
    #[must_use]
    pub const fn call_id(self) -> Option<CallId> {
        self.call_id
    }

    /// Context for out-of-turn credential-lifecycle observations.
    ///
    /// Refresh is not session-scoped. These identities keep the observation
    /// well-typed for the sink and store writer; they must not be exported as
    /// product session/run identities (the export bridge uses a dedicated
    /// parentless credential scope instead).
    #[must_use]
    pub fn credential_lifecycle() -> Self {
        fn non_nil<T: TryFrom<uuid::Uuid>>(value: u128) -> T
        where
            T::Error: fmt::Debug,
        {
            T::try_from(uuid::Uuid::from_u128(value))
                .unwrap_or_else(|_| unreachable!("credential-lifecycle UUID is non-nil"))
        }
        Self::new(
            non_nil(0xC0ED_0000_0000_0000_0000_0000_0000_0001),
            non_nil(0xC0ED_0000_0000_0000_0000_0000_0000_0002),
            non_nil(0xC0ED_0000_0000_0000_0000_0000_0000_0003),
            non_nil(0xC0ED_0000_0000_0000_0000_0000_0000_0004),
        )
    }
}

/// Content-free terminal status shared by telemetry lifecycle observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TelemetryTerminalStatus {
    /// The operation completed successfully.
    Completed,
    /// The operation failed.
    Failed,
    /// The operation was cancelled.
    Cancelled,
    /// The owning runtime shut down.
    Shutdown,
}

/// Exact store operation represented by one durability-attempt observation.
///
/// The bounded commit protocol performs one commit submission and, when that
/// submission fails without a usable acknowledgement, at most one exact
/// acknowledgement-recovery read. There is no retried commit path. Cleanup
/// observations name each joined driver phase separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DurabilityAttemptKind {
    /// The first commit submission for one logical mutation.
    InitialCommit,
    /// Exact acknowledgement recovery after the first submission.
    InitialAcknowledgementRecovery,
    /// Joining pending tool tasks at session shutdown.
    CleanupToolDrain,
    /// Stopping jobs and publishing their terminal records at session shutdown.
    CleanupJobTerminals,
    /// Joining the lease heartbeat before reading release authority.
    CleanupHeartbeatJoin,
    /// Releasing the exact branch lease.
    CleanupLeaseRelease,
    /// Flushing the selected session backend.
    CleanupFlush,
}

/// Typed content-free outcome of one durability attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DurabilityAttemptOutcome {
    /// The store directly acknowledged the exact commit.
    Committed,
    /// The attempt did not establish the requested durable result.
    Failed,
    /// An exact committed mutation recovered a lost acknowledgement.
    RecoveredAcknowledgement,
    /// A cleanup phase joined successfully; not a checkpoint commit.
    Joined,
}

/// Closed, content-free jobs classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobTool {
    /// First process observation.
    Start,
    /// Nonblocking observation.
    Poll,
    /// Write followed by an observation window.
    WriteWait,
    /// Blocking await supplied by a tool adapter.
    Await,
}
impl JobTool {
    /// Stable provider-neutral analytics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Poll => "poll",
            Self::WriteWait => "write_wait",
            Self::Await => "await",
        }
    }
}

/// Source supplied by the tool adapter at lowering, independent of its vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobHintSource {
    /// Construction-bound budget.
    Default,
    /// Explicit model observation hint.
    ModelHint,
    /// Background flag or an exact-dispatch human background edge.
    Background,
}
impl JobHintSource {
    /// Stable provider-neutral analytics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ModelHint => "model_hint",
            Self::Background => "background",
        }
    }
}

/// Closed, content-free jobs classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobPreemption {
    /// `promoted`.
    Promoted,
    /// `superseded`.
    Superseded,
    /// `wait_yielded`.
    WaitYielded,
}
impl JobPreemption {
    /// Stable analytics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Promoted => "preempt_promoted",
            Self::Superseded => "preempt_superseded",
            Self::WaitYielded => "preempt_wait_yielded",
        }
    }
}

/// Closed, content-free jobs classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobNoticeDelivery {
    /// `boundary`.
    Boundary,
    /// `admission`.
    Admission,
    /// `idle_wake`.
    IdleWake,
}
impl JobNoticeDelivery {
    /// Stable analytics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Boundary => "boundary",
            Self::Admission => "admission",
            Self::IdleWake => "idle_wake",
        }
    }
}

/// Typed content-free telemetry data.
///
/// Lifecycle-only observations: identifiers, durations, counters, usage values,
/// and closed status enums. No prompt text, tool content, replay payloads,
/// credentials, headers, environment values, or arbitrary error sources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TelemetryObservationKind {
    /// A process first left its foreground window.
    JobPromoted {
        /// Neutral operation supplied by the tool adapter.
        tool: JobTool,
        /// Whether the dispatch was inside a composition cell or provider program.
        nested: bool,
        /// Source of the foreground observation window.
        hint_source: JobHintSource,
        /// Selected foreground budget in milliseconds before any early preemption;
        /// this is not the elapsed observation duration.
        hint_ms: u64,
    },
    /// A post-promotion process or agent wait.
    JobWaited {
        /// Presented wait tool.
        tool: JobTool,
    },
    /// Steering closed or yielded work.
    JobPreempted {
        /// Closed preemption classification.
        kind: JobPreemption,
    },
    /// Notices were delivered into committed history.
    JobNoticesDelivered {
        /// Actual delivery point, not a preview.
        point: JobNoticeDelivery,
        /// Number of job notices consumed by the commit.
        count: u32,
    },
    /// A turn entered the actor's active state.
    TurnStarted {
        /// The admitted root's origin, unchanged by later steering.
        origin: plakous_model::TurnOrigin,
        /// Time spent in the admitted queue.
        queue_time: Duration,
    },
    /// A turn reached a terminal state.
    TurnFinished {
        /// End-to-end active duration.
        elapsed: Duration,
        /// Terminal status.
        status: TelemetryTerminalStatus,
        /// Checked usage accumulated by the turn.
        usage: TurnUsage,
        /// Sum of finished model-operation durations within the turn.
        model_wall: Duration,
        /// Sum of finished tool-call durations within the turn.
        ///
        /// Concurrent tool calls may sum to more than [`Self::TurnFinished::elapsed`].
        tool_wall: Duration,
        /// Sum of finished compaction durations attributed to the turn.
        compaction_wall: Duration,
        /// Count of finished model operations within the turn.
        model_attempts: u32,
        /// Count of finished tool calls within the turn.
        tool_calls: u32,
        /// Count of finished compactions attributed to the turn.
        compactions: u32,
    },
    /// A model past its admitted evidence window was re-resolved between
    /// model operations. `accepted` is false when the fresh resolution was
    /// refused or failed and the turn ended without a dispatch.
    ModelReresolved {
        /// Whether the fresh resolution was swapped in.
        accepted: bool,
    },
    /// A model operation began.
    ModelStarted,
    /// A model operation's stable request prefix differs from the one that
    /// produced the active family artifact. A Responses route still replays
    /// the artifact when only tool selection differs and prepares fresh
    /// otherwise. Emitted only when this driver observed the producing round.
    ModelArtifactPrefixDrift {
        /// The stable-prefix fields that differ, by name only.
        fields: plakous_model::StableRequestPrefixDrift,
    },
    /// The first output item or delta arrived, including content-free activity
    /// for tool input suppressed by canonical projection. Envelope events alone
    /// do not stamp first output.
    ModelFirstEvent {
        /// Duration from operation start to the first content-bearing event (TTFT).
        elapsed: Duration,
    },
    /// Content-free stream progress, including keepalive-masked silence.
    ModelStreamActivity {
        /// Latest physical generation's activity facts.
        activity: plakous_model::ModelStreamActivity,
    },
    /// A Messages `message_start` cache-miss diagnosis.
    ///
    /// Class and missed input tokens only. No provider message id and no raw
    /// unknown spelling. It does not change usage, history, replay, or the turn.
    PromptCacheDiagnosis {
        /// Closed miss class.
        reason: plakous_model::PromptCacheMissReason,
        /// Non-negative `cache_missed_input_tokens`, or none when omitted.
        missed_input_tokens: Option<u64>,
    },
    /// A model operation reached a terminal state.
    ModelFinished {
        /// Complete operation duration (TTLB).
        elapsed: Duration,
        /// Terminal status.
        status: TelemetryTerminalStatus,
        /// Final operation usage when reported.
        usage: Option<Usage>,
        /// Served upstream provider display name when reported.
        served_by: Option<plakous_model::ServedBy>,
    },
    /// A local tool dispatch began.
    ToolStarted,
    /// A local tool dispatch reached a terminal state.
    ToolFinished {
        /// Complete tool duration.
        elapsed: Duration,
        /// Terminal status.
        status: TelemetryTerminalStatus,
        /// Byte length of the tool result handed back to the model (never content).
        output_bytes: u64,
    },
    /// A compaction workflow began.
    CompactionStarted {
        /// Product-visible compaction reason.
        reason: CompactionEventReason,
    },
    /// A compaction workflow or optional scheduling probe reached a terminal state.
    /// Refused optional probes have zero duration and no token counts or usage.
    CompactionFinished {
        /// Exact endpoint refusal class; diagnostic content stays in events.
        refusal: Option<CompactionRefusalReason>,
        /// Complete compaction duration.
        elapsed: Duration,
        /// Terminal compaction outcome.
        outcome: CompactionEventOutcome,
        /// Product-visible compaction reason.
        reason: CompactionEventReason,
        /// Pre-compaction input token count when measured.
        tokens_before: Option<u64>,
        /// Post-compaction input token count when measured.
        tokens_after: Option<u64>,
        /// Paid compaction call usage when the dialect reported it.
        usage: Option<Usage>,
    },
    /// A required-durability operation reached a terminal state.
    DurabilityFinished {
        /// Complete logical durability-operation duration.
        elapsed: Duration,
        /// Terminal status.
        status: TelemetryTerminalStatus,
    },
    /// One exact commit or acknowledgement-recovery attempt finished.
    DurabilityAttemptFinished {
        /// Complete duration of this individual store operation.
        elapsed: Duration,
        /// The exact position in the bounded commit protocol.
        attempt: DurabilityAttemptKind,
        /// The typed result established by the operation.
        outcome: DurabilityAttemptOutcome,
    },
    /// A best-effort history prune after a terminal publication finished.
    ///
    /// A failed prune changes nothing durable and never changes the turn.
    RetentionFinished {
        /// Complete prune duration.
        elapsed: Duration,
        /// Terminal status.
        status: TelemetryTerminalStatus,
        /// Removed checkpoints.
        pruned_checkpoints: u64,
    },
    /// A plakous-owned local-composition child process exited.
    ChildProcessExited {
        /// Which local-composition worker process exited.
        child: ChildProcessKind,
        /// User CPU time attributed to the exit sample.
        user_cpu: Duration,
        /// System CPU time attributed to the exit sample.
        system_cpu: Duration,
        /// Peak resident set size when available.
        peak_rss_bytes: Option<u64>,
    },
    /// One claimed OAuth refresh (or revoke) ladder step.
    ///
    /// Content-free: no tokens, subjects, URLs, or account identifiers.
    /// `provider` is a short ASCII label supplied by the product facade.
    CredentialRefresh {
        /// Neutral provider label chosen by the product facade.
        provider: CredentialProviderLabel,
        /// Ladder phase reached by this observation.
        phase: CredentialRefreshPhase,
        /// Closed failure class when the phase is not success.
        failure_class: Option<CredentialRefreshFailureClass>,
        /// Physical transport phase of a network failure, when known.
        transport_phase: Option<plakous_model::ModelRetryPhase>,
        /// Raw HTTP status when a response was received.
        http_status: Option<u16>,
        /// Elapsed time from `begin_refresh` (claim take) through this phase.
        elapsed: Option<Duration>,
        /// Durable credential generation at the claim.
        generation: u64,
    },
}

/// Neutral local-composition child process kind for exit telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChildProcessKind {
    /// JavaScript evaluator sidecar process owned by local composition.
    JsEvalSidecar,
    /// Python worker process owned by local composition.
    PythonWorker,
}

const MAX_CREDENTIAL_PROVIDER_LABEL_BYTES: usize = 16;

/// Bounded ASCII OAuth-provider label for credential-lifecycle telemetry.
///
/// Neutral string identity only, chosen by the product facade. Not a provider
/// enum and not a display name like [`plakous_model::ServedBy`].
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CredentialProviderLabel {
    len: u8,
    bytes: [u8; MAX_CREDENTIAL_PROVIDER_LABEL_BYTES],
}

impl CredentialProviderLabel {
    /// Validates a short printable-ASCII provider label.
    ///
    /// # Errors
    ///
    /// Empty, oversized, or non-printable values are rejected.
    pub fn new(value: impl AsRef<str>) -> Result<Self, plakous_model::IdentityError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(plakous_model::IdentityError::Empty);
        }
        if value.len() > MAX_CREDENTIAL_PROVIDER_LABEL_BYTES {
            return Err(plakous_model::IdentityError::TooLong {
                maximum: MAX_CREDENTIAL_PROVIDER_LABEL_BYTES,
            });
        }
        let mut bytes = [0_u8; MAX_CREDENTIAL_PROVIDER_LABEL_BYTES];
        for (index, byte) in value.bytes().enumerate() {
            if !(0x20..=0x7e).contains(&byte) {
                return Err(plakous_model::IdentityError::InvalidCharacter { index });
            }
            bytes[index] = byte;
        }
        Ok(Self {
            len: u8::try_from(value.len()).map_err(|_| plakous_model::IdentityError::TooLong {
                maximum: MAX_CREDENTIAL_PROVIDER_LABEL_BYTES,
            })?,
            bytes,
        })
    }

    /// Returns the validated label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        let slice = &self.bytes[..usize::from(self.len)];
        std::str::from_utf8(slice).unwrap_or("")
    }
}

impl fmt::Debug for CredentialProviderLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialProviderLabel")
            .field("value", &self.as_str())
            .finish()
    }
}

/// Credential-refresh ladder phase (content-free).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CredentialRefreshPhase {
    /// Durable Unknown claim was taken before provider I/O.
    Began,
    /// Replacement Ready record was committed.
    Completed,
    /// Post-send failure left the durable generation Unknown.
    OutcomeUnknown,
    /// The claim was left quarantined without a restore.
    Quarantined,
    /// Pre-send failure restored the pre-claim record.
    Restored,
}

/// Closed failure class for a credential-refresh observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CredentialRefreshFailureClass {
    /// Transport or protocol failure after the claim started.
    Transport,
    /// Absolute or phase deadline elapsed.
    Timeout,
    /// Provider HTTP status class (never a reason body).
    HttpStatus(HttpStatusClass),
    /// Local token/response validation rejected a received body.
    Validation,
    /// Private-store transition failed.
    Store,
    /// Caller cancellation was observed.
    Cancelled,
    /// Pre-send request gate or local readiness denied dispatch.
    NotReady,
}

/// HTTP status class carried on credential-refresh telemetry (never a URL).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpStatusClass {
    /// `4xx` client error.
    ClientError,
    /// `5xx` server error.
    ServerError,
    /// Any other status (1xx/2xx/3xx) when a class is still recorded.
    Other,
}

impl HttpStatusClass {
    /// Classifies a raw HTTP status code.
    #[must_use]
    pub const fn from_status(status: u16) -> Self {
        match status {
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            _ => Self::Other,
        }
    }
}

/// One content-free telemetry observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryObservation {
    context: TelemetryContext,
    timestamp: Timestamp,
    kind: TelemetryObservationKind,
}

impl TelemetryObservation {
    /// Creates one typed content-free observation.
    #[must_use]
    pub const fn new(
        context: TelemetryContext,
        timestamp: Timestamp,
        kind: TelemetryObservationKind,
    ) -> Self {
        Self {
            context,
            timestamp,
            kind,
        }
    }

    /// Returns its typed correlation context.
    #[must_use]
    pub const fn context(self) -> TelemetryContext {
        self.context
    }

    /// Returns its observation time.
    #[must_use]
    pub const fn timestamp(self) -> Timestamp {
        self.timestamp
    }

    /// Returns its typed content-free data.
    #[must_use]
    pub const fn kind(self) -> TelemetryObservationKind {
        self.kind
    }
}

/// Snapshot of bounded telemetry delivery health.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TelemetryStatus {
    emitted: u64,
    dropped: u64,
    panics: u64,
    enabled: bool,
}

impl TelemetryStatus {
    /// Returns successfully admitted observations.
    #[must_use]
    pub const fn emitted(self) -> u64 {
        self.emitted
    }

    /// Returns observations rejected or lost to a contained sink panic.
    #[must_use]
    pub const fn dropped(self) -> u64 {
        self.dropped
    }

    /// Returns contained application-sink panics.
    #[must_use]
    pub const fn panics(self) -> u64 {
        self.panics
    }

    /// Reports whether a telemetry sink remains enabled.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

/// Panic-contained optional telemetry emitter suitable for later actor wiring.
pub struct TelemetryEmitter {
    sink: Mutex<Option<Arc<dyn TelemetrySink>>>,
    retry_sink: Mutex<Option<Arc<dyn TelemetrySink>>>,
    emitted: AtomicU64,
    dropped: AtomicU64,
    panics: AtomicU64,
}

impl TelemetryEmitter {
    /// Creates an emitter which performs no telemetry work.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            sink: Mutex::new(None),
            retry_sink: Mutex::new(None),
            emitted: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            panics: AtomicU64::new(0),
        }
    }

    /// Creates an opt-in emitter over an application-owned sink.
    #[must_use]
    pub fn enabled(sink: Arc<dyn TelemetrySink>) -> Self {
        Self {
            retry_sink: Mutex::new(Some(Arc::clone(&sink))),
            sink: Mutex::new(Some(sink)),
            emitted: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            panics: AtomicU64::new(0),
        }
    }

    /// Attempts one observation. Sink pressure, closure, and panics never escape
    /// into agent semantics.
    pub fn emit(&self, observation: TelemetryObservation) {
        let sink = lock(&self.sink).clone();
        let Some(sink) = sink else {
            return;
        };
        match std::panic::catch_unwind(AssertUnwindSafe(|| sink.try_emit(observation))) {
            Ok(Ok(())) => increment(&self.emitted),
            Ok(Err(TelemetrySinkError::Full)) => increment(&self.dropped),
            Ok(Err(TelemetrySinkError::Closed)) => {
                increment(&self.dropped);
                self.disable(&sink);
            }
            Err(_) => {
                increment(&self.dropped);
                increment(&self.panics);
                self.disable(&sink);
            }
        }
    }

    /// Delivers a transient retry to the application's logging hook. Analytics
    /// admission failures do not suppress logs; a panicking hook is retired.
    pub fn observe_model_retry(
        &self,
        context: TelemetryContext,
        retry: &plakous_model::ModelRetry,
    ) {
        let sink = lock(&self.retry_sink).clone();
        if let Some(sink) = sink
            && std::panic::catch_unwind(AssertUnwindSafe(|| {
                sink.observe_model_retry(context, retry);
            }))
            .is_err()
        {
            increment(&self.panics);
            *lock(&self.retry_sink) = None;
        }
    }

    /// Returns a current health snapshot.
    #[must_use]
    pub fn status(&self) -> TelemetryStatus {
        TelemetryStatus {
            emitted: self.emitted.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Acquire),
            panics: self.panics.load(Ordering::Acquire),
            enabled: lock(&self.sink).is_some(),
        }
    }

    fn disable(&self, failed: &Arc<dyn TelemetrySink>) {
        let mut sink = lock(&self.sink);
        if sink
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            *sink = None;
        }
    }
}

impl Default for TelemetryEmitter {
    fn default() -> Self {
        Self::disabled()
    }
}

impl fmt::Debug for TelemetryEmitter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelemetryEmitter")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::unnecessary_wraps,
    reason = "unit tests may assert with expect/unwrap and use TestResult for uniformity"
)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};

    use uuid::Uuid;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    struct ScriptedSink {
        mode: AtomicU8,
        observations: Mutex<Vec<TelemetryObservation>>,
    }

    impl TelemetrySink for ScriptedSink {
        fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
            match self.mode.load(AtomicOrdering::Acquire) {
                0 => {
                    lock(&self.observations).push(observation);
                    Ok(())
                }
                1 => Err(TelemetrySinkError::Full),
                2 => Err(TelemetrySinkError::Closed),
                _ => panic!("contained telemetry panic"),
            }
        }
    }

    fn observation() -> TestResult<TelemetryObservation> {
        Ok(TelemetryObservation::new(
            TelemetryContext::new(
                AgentId::try_from(Uuid::from_u128(1))?,
                SessionId::try_from(Uuid::from_u128(2))?,
                BranchId::try_from(Uuid::from_u128(3))?,
                RunId::try_from(Uuid::from_u128(4))?,
            )
            .with_turn(TurnId::try_from(Uuid::from_u128(5))?)
            .with_attempt(AttemptId::try_from(Uuid::from_u128(6))?)
            .with_call(CallId::try_from(Uuid::from_u128(7))?),
            Timestamp::new(8, 9)?,
            TelemetryObservationKind::ModelFinished {
                elapsed: Duration::from_millis(10),
                status: TelemetryTerminalStatus::Completed,
                usage: Some(Usage::new(11, 12, 13, 14, 15)),
                served_by: None,
            },
        ))
    }

    #[test]
    fn retry_logging_panics_are_contained_and_retire_only_the_hook() -> TestResult {
        struct PanickingHook(AtomicU8);
        impl TelemetrySink for PanickingHook {
            fn try_emit(&self, _: TelemetryObservation) -> Result<(), TelemetrySinkError> {
                Ok(())
            }
            fn observe_model_retry(&self, _: TelemetryContext, _: &plakous_model::ModelRetry) {
                self.0.fetch_add(1, AtomicOrdering::SeqCst);
                panic!("contained retry logging panic");
            }
        }
        let sink = Arc::new(PanickingHook(AtomicU8::new(0)));
        let emitter = TelemetryEmitter::enabled(sink.clone());
        let retry = plakous_model::ModelRetry::new(
            2,
            4,
            Duration::ZERO,
            plakous_model::ModelRetryCause::RecordRejectedInTransit,
            false,
            "peer_rejected_record",
        );
        let observation = observation()?;
        emitter.observe_model_retry(observation.context(), &retry);
        emitter.observe_model_retry(observation.context(), &retry);
        emitter.emit(observation);
        assert_eq!(sink.0.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(emitter.status().panics(), 1);
        assert_eq!(emitter.status().emitted(), 1);
        assert!(emitter.status().enabled());
        Ok(())
    }

    #[test]
    fn disabled_emitter_is_a_true_opt_out() -> TestResult {
        let emitter = TelemetryEmitter::disabled();
        emitter.emit(observation()?);
        assert_eq!(emitter.status(), TelemetryStatus::default());
        Ok(())
    }

    #[test]
    fn full_is_dropped_while_closed_disables_the_sink() -> TestResult {
        let sink = Arc::new(ScriptedSink {
            mode: AtomicU8::new(1),
            observations: Mutex::new(Vec::new()),
        });
        let emitter = TelemetryEmitter::enabled(sink.clone());
        emitter.emit(observation()?);
        assert_eq!(emitter.status().dropped(), 1);
        assert!(emitter.status().enabled());

        sink.mode.store(2, AtomicOrdering::Release);
        emitter.emit(observation()?);
        emitter.emit(observation()?);
        assert_eq!(emitter.status().dropped(), 2);
        assert!(!emitter.status().enabled());
        Ok(())
    }

    #[test]
    fn sink_panic_is_contained_counted_and_disabled() -> TestResult {
        let sink = Arc::new(ScriptedSink {
            mode: AtomicU8::new(3),
            observations: Mutex::new(Vec::new()),
        });
        let emitter = TelemetryEmitter::enabled(sink);
        emitter.emit(observation()?);
        emitter.emit(observation()?);
        assert_eq!(emitter.status().dropped(), 1);
        assert_eq!(emitter.status().panics(), 1);
        assert!(!emitter.status().enabled());
        Ok(())
    }

    #[test]
    fn turn_finished_compartments_are_copy_and_typed() -> TestResult {
        let kind = TelemetryObservationKind::TurnFinished {
            elapsed: Duration::from_millis(100),
            status: TelemetryTerminalStatus::Completed,
            usage: TurnUsage::default(),
            model_wall: Duration::from_millis(40),
            tool_wall: Duration::from_millis(30),
            compaction_wall: Duration::from_millis(10),
            model_attempts: 1,
            tool_calls: 2,
            compactions: 1,
        };
        let copied = kind;
        let TelemetryObservationKind::TurnFinished {
            model_attempts,
            tool_calls,
            ..
        } = copied
        else {
            panic!("expected TurnFinished");
        };
        assert_eq!(model_attempts, 1);
        assert_eq!(tool_calls, 2);
        Ok(())
    }

    #[test]
    fn tool_finished_carries_output_bytes() -> TestResult {
        let kind = TelemetryObservationKind::ToolFinished {
            elapsed: Duration::from_millis(5),
            status: TelemetryTerminalStatus::Completed,
            output_bytes: 42,
        };
        let TelemetryObservationKind::ToolFinished { output_bytes, .. } = kind else {
            panic!("expected ToolFinished");
        };
        assert_eq!(output_bytes, 42);
        Ok(())
    }

    #[test]
    fn compaction_finished_carries_tokens_and_usage() -> TestResult {
        let kind = TelemetryObservationKind::CompactionFinished {
            refusal: None,
            elapsed: Duration::from_millis(50),
            outcome: CompactionEventOutcome::Committed,
            reason: CompactionEventReason::Manual,
            tokens_before: Some(100),
            tokens_after: Some(40),
            usage: Some(Usage::new(1, 2, 3, 4, 5)),
        };
        let TelemetryObservationKind::CompactionFinished {
            tokens_before,
            tokens_after,
            usage,
            ..
        } = kind
        else {
            panic!("expected CompactionFinished");
        };
        assert_eq!(tokens_before, Some(100));
        assert_eq!(tokens_after, Some(40));
        assert_eq!(usage, Some(Usage::new(1, 2, 3, 4, 5)));
        Ok(())
    }

    #[test]
    fn durability_attempt_finished_is_copy() -> TestResult {
        let kind = TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed: Duration::from_millis(3),
            attempt: DurabilityAttemptKind::InitialCommit,
            outcome: DurabilityAttemptOutcome::Committed,
        };
        let copied = kind;
        let TelemetryObservationKind::DurabilityAttemptFinished {
            attempt, outcome, ..
        } = copied
        else {
            panic!("expected DurabilityAttemptFinished");
        };
        assert_eq!(attempt, DurabilityAttemptKind::InitialCommit);
        assert_eq!(outcome, DurabilityAttemptOutcome::Committed);
        Ok(())
    }

    #[test]
    fn observation_surface_contains_only_typed_content_free_values() -> TestResult {
        let observation = observation()?;
        let TelemetryObservationKind::ModelFinished {
            elapsed,
            status,
            usage,
            served_by,
        } = observation.kind()
        else {
            panic!("fixture kind changed");
        };
        assert_eq!(elapsed, Duration::from_millis(10));
        assert_eq!(status, TelemetryTerminalStatus::Completed);
        assert_eq!(usage, Some(Usage::new(11, 12, 13, 14, 15)));
        assert_eq!(served_by, None);
        assert_eq!(
            observation.context().call_id(),
            Some(CallId::try_from(Uuid::from_u128(7))?)
        );
        Ok(())
    }

    #[test]
    fn credential_refresh_is_copy_and_content_free() -> TestResult {
        let kind = TelemetryObservationKind::CredentialRefresh {
            provider: CredentialProviderLabel::new("acme")?,
            phase: CredentialRefreshPhase::OutcomeUnknown,
            failure_class: Some(CredentialRefreshFailureClass::Timeout),
            transport_phase: Some(plakous_model::ModelRetryPhase::Resolution),
            http_status: Some(503),
            elapsed: Some(Duration::from_millis(30)),
            generation: 40,
        };
        let copied = kind;
        let TelemetryObservationKind::CredentialRefresh {
            provider,
            phase,
            failure_class,
            http_status,
            generation,
            ..
        } = copied
        else {
            panic!("expected CredentialRefresh");
        };
        assert_eq!(provider.as_str(), "acme");
        assert_eq!(phase, CredentialRefreshPhase::OutcomeUnknown);
        assert_eq!(failure_class, Some(CredentialRefreshFailureClass::Timeout));
        assert_eq!(http_status, Some(503));
        assert_eq!(generation, 40);
        assert_eq!(
            HttpStatusClass::from_status(503),
            HttpStatusClass::ServerError
        );
        assert!(CredentialProviderLabel::new("").is_err());
        Ok(())
    }
}
