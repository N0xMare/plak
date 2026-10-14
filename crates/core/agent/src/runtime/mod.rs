//! Shared private construction and turn storage for the single-owner runtime.

use crate::local_composition::LocalCompositionConstruction;
use crate::manifest::InterpreterObservation;
use crate::runtime::checkpoint::owner::{CheckpointReader, DurableBranchFacts, PreparedCheckpoint};
use crate::runtime::cleanup::settle_local_composition_deliveries;
use crate::runtime::continuation::RestoredContinuationFrame;
use crate::runtime::terminal::DurableTerminalResultRecord;
use crate::state::{TurnMachine, TurnTransition};
use crate::tool_runtime::{PendingEffectDecision, ToolRuntime, ToolRuntimeConfig, ToolScheduler};
use crate::{
    ActorCommandEffect, ActorControl, AgentError, AgentErrorKind, CheckpointId,
    CheckpointObjectBundle, CommandAck, CommandId, EffectDecisionRef, EventHub, EventPipeline,
    ResolvedHarnessPlan, RuntimeCheckpoint, SubscriptionLimits, TelemetryContext, TelemetryEmitter,
    TurnControl, TurnEvents, TurnUsage,
};
pub use config::AgentRequestPolicy;
pub use config::RUNTIME_EXECUTOR_STACK_BYTES;
pub use driver::OPERATION_BUDGET_WRAP_UP_NOTE;
pub(crate) use driver::last_operator_input_unix_ms;
pub(crate) use observation::{
    history_tool_call_observation, history_tool_result_observation_with_subject,
    normalized_tool_failure,
};
use plakous_model::{
    AgentId, BranchId, CancellationToken, CanonicalHistory, Clock, CompiledProviderConfiguration,
    ContextCapacityFailure, ConversationView, Digest, IdGenerator, InstructionBlock, LanguageModel,
    ModelErrorTarget, ModelResponse, RunId, RuntimeModelAuthority, SealedFamilyArtifactCandidate,
    SessionId, StableRequestPrefix, ToolResultBlock, TurnId, ValidatedRequestPlan,
};
use plakous_tools::{
    FrozenToolRegistry, ProjectionCandidateAdmission, ToolExecutorBindings, ToolExposureMap,
    ToolInvocation, ToolPolicy, ToolProjectionMapV2, ToolProjectionReceiptV2, WorkspacePolicy,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

pub use durable::DurableSessionAddress;

pub use durable::DurableCodeCellAbandonmentOutcome;
pub use durable::DurableCodeCellAbandonmentRequest;
pub use durable::DurableCompactionReceipt;
pub use durable::DurableCompactionRecoveryOutcome;
pub use durable::DurableEffectEvidenceRecoveryOutcome;
pub use durable::DurableEffectEvidenceRecoveryRequest;
pub use durable::DurableRuntimeBranchOutcome;
pub use durable::DurableRuntimeOpen;

pub use config::AgentConfig;

mod checkpoint;
mod durable;
mod family_artifact;
pub(crate) mod feed_views;

pub use durable::DurableSessionRuntime;
pub(crate) use durable::turn_recovery::recovery_verdict_refuses;
pub use durable::turn_recovery::{CheckpointRecoveryVerdict, checkpoint_recovery_verdict};

mod build;
pub use build::compile_explicit_runtime_profile;
pub(crate) use build::{stored_composition_artifact, stored_job_config};

mod admission;

mod continuation;

mod observation;

mod config;

mod control;

mod context;

mod terminal;
#[cfg(test)]
use durable::checkpoint_can_abandon_compaction;
pub use terminal::checkpoint_closes_on_reopen;
pub(crate) use terminal::checkpoint_retained_history;
pub(crate) use terminal::valid_terminal_enrichment;
#[cfg(test)]
pub(crate) use terminal::{
    checkpoint_finalizes_root_turn, checkpoint_recovers_an_abandoned_turn,
    checkpoint_root_terminal_result,
};

mod agent;
pub use agent::{abandon_and_release_many, close_admission, lease_released, shutdown_for_transfer};
mod operation_record;

mod cleanup;

mod compaction;

mod composition;

mod driver;
pub(crate) use driver::{DurableEffectIntentEnvelope, DurableUndispatchedToolRequest};
pub(crate) use terminal::DurableAgentError;
mod prompts;
mod reresolution;
mod round_classification;
pub use reresolution::{
    ModelReresolutionError, ModelReresolutionOperation, ModelReresolver, ReresolvedModel,
};
/// Default deadline for one session-store call made by the durable runtime, including the
/// shutdown receipt a session release commits. Hosts size their release timeouts from it.
pub const DEFAULT_STORE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STORE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoreTimeouts {
    operation: Duration,
    cleanup: Duration,
}

struct RuntimeInstanceNonce([u8; 32]);

/// Validated low-level durable branch handoff consumed by the private driver.
///
/// High-level create/open/recovery workflows own store lookup and lease
/// acquisition. This value admits an exact certified quiescent branch, after
/// selectively folding checkpoint-contained terminal evidence when possible.
pub(crate) struct DurableRuntimeBootstrap {
    checkpoint: Arc<RuntimeCheckpoint>,
    bundle: CheckpointObjectBundle,
    branch: DurableBranchFacts,
    admission: crate::RevalidationFact,
}

/// Builder for one independently owned agent actor.
pub struct AgentBuilder {
    model: Arc<dyn LanguageModel>,
    provider_configuration: Arc<CompiledProviderConfiguration>,
    initial_model_authority: RuntimeModelAuthority,
    tools: FrozenToolRegistry,
    tool_projection_v2: Option<ToolProjectionMapV2>,
    projection_admission: Option<Arc<dyn ProjectionCandidateAdmission>>,
    tool_bindings: ToolExecutorBindings,
    workspace: Arc<WorkspacePolicy>,
    host_process_options: plakous_tools::HostProcessOptions,
    job_config: plakous_tools::JobConfig,
    job_counter_seed: u32,
    job_host: Option<Arc<dyn plakous_tools::JobHost>>,
    config: AgentConfig,
    request_policy: AgentRequestPolicy,
    instructions: Vec<InstructionBlock>,
    harness_plan: Option<ResolvedHarnessPlan>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    event_ids: Arc<dyn IdGenerator>,
    policy: Arc<dyn ToolPolicy>,
    telemetry: Arc<TelemetryEmitter>,
    /// Optional process-local scheduler override (multi-session isolation).
    /// When `None`, [`ToolRuntime::new`] uses the process-wide default scheduler.
    tool_scheduler: Option<(ToolScheduler, ToolRuntimeConfig)>,
    local_composition: Option<LocalCompositionConstruction>,
    post_compaction_note: Option<PostCompactionNote>,
    model_reresolver: Option<(Arc<dyn ModelReresolver>, plakous_model::Timestamp)>,
}

/// Derives a note to append after a committed compaction from its source and
/// its successor. `None` means the successor needs no note.
pub(crate) type PostCompactionNote =
    Arc<dyn Fn(&CanonicalHistory, &CanonicalHistory) -> Option<String> + Send + Sync>;

#[derive(Clone)]
enum ToolPresentation {
    V1(ToolExposureMap),
    V2(Arc<ToolProjectionMapV2>),
}

struct PreparedToolInvocation {
    invocation: ToolInvocation,
    projection_receipt: Option<ToolProjectionReceiptV2>,
}

struct PreparedAgent {
    job_sessions: plakous_tools::JoinedProcessSessions,
    job_host: Option<Arc<dyn plakous_tools::JobHost>>,
    model: Arc<dyn LanguageModel>,
    provider_configuration: Arc<CompiledProviderConfiguration>,
    model_error_target: ModelErrorTarget,
    tools: ToolRuntime,
    tool_presentation: ToolPresentation,
    config: AgentConfig,
    stable_request_prefix: StableRequestPrefix,
    harness_plan: ResolvedHarnessPlan,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    agent_id: AgentId,
    session_id: SessionId,
    branch_id: BranchId,
    run_id: RunId,
    authority: PreparedCheckpoint,
    initial_command_ledger: BTreeMap<CommandId, CommandLedgerEntry>,
    next_command_sequence: u64,
    next_operation_sequence: u64,
    events: EventHub,
    event_limits: SubscriptionLimits,
    /// A reopen that derived a different output bound than the session carried.
    output_bound_rederived: Option<crate::manifest::OutputBoundRederived>,
    /// A reopen that closed a turn a lost process left with an unfinished request.
    abandoned_turn: Option<AbandonedTurnReport>,
    telemetry: Arc<TelemetryEmitter>,
    telemetry_context: TelemetryContext,
    restored: Option<RestoredStartup>,
    local_composition: Option<LocalCompositionConstruction>,
    interpreter_observation: Option<InterpreterObservation>,
    post_compaction_note: Option<PostCompactionNote>,
    model_rebind: Option<reresolution::ModelRebind>,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum JobReleaseMode {
    #[default]
    Pending,
    Transfer,
    Terminal,
    Abandon,
}

enum JobStopState {
    Idle,
    Stopping(watch::Receiver<Option<Result<(), AgentError>>>),
}

struct AgentInner {
    job_stop: Mutex<JobStopState>,
    job_runtime: tokio::runtime::Handle,
    job_sessions: plakous_tools::JoinedProcessSessions,
    job_release_mode: Arc<Mutex<JobReleaseMode>>,
    agent_id: AgentId,
    session_id: SessionId,
    branch_id: BranchId,
    harness_plan: ResolvedHarnessPlan,
    stable_request_prefix: StableRequestPrefix,
    controls: mpsc::Sender<ActorControl>,
    shutdown: CancellationToken,
    completion: watch::Receiver<Option<Result<(), AgentError>>>,
    accepting: Arc<AtomicBool>,
    events: EventHub,
    authority: CheckpointReader,
    known_turns: Arc<Mutex<BTreeMap<TurnId, Weak<TurnShared>>>>,
    command_nonce: Arc<AtomicU64>,
    telemetry: Arc<TelemetryEmitter>,
    recovered_turn: Option<RecoveredTurnHandle>,
    abandoned_turn: Option<AbandonedTurnReport>,
    #[cfg(test)]
    driver_abort: tokio::task::AbortHandle,
}

/// Cheap cloneable capability for one private agent actor.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

/// Non-owning agent capability.
#[derive(Clone, Debug)]
pub struct WeakAgent {
    inner: Weak<AgentInner>,
}

/// One admitted logical turn with independent result, event, and control views.
#[allow(
    clippy::struct_field_names,
    reason = "the explicit turn_id field distinguishes identity from the turn capability"
)]
/// Acknowledged start-command submission; duplicates never create a turn.
pub struct TurnSubmission {
    acknowledgement: CommandAck,
    turn: Option<Turn>,
    result: Option<TurnResultHandle>,
}

/// One admitted logical turn.
#[allow(
    clippy::struct_field_names,
    reason = "turn_id explicitly distinguishes identity from the owned turn capability"
)]
pub struct Turn {
    turn_id: TurnId,
    result: TurnResultHandle,
    events: EventHub,
    event_limits: SubscriptionLimits,
    initial_events: Option<TurnEvents>,
    control: TurnControl,
}

/// Opaque authority for one original turn restored at an exact decision barrier.
#[derive(Clone)]
pub struct RecoveredTurnHandle {
    turn_id: TurnId,
    decision: Option<EffectDecisionRef>,
    result: TurnResultHandle,
    control: TurnControl,
}

/// Cloneable bounded observer for one authoritative terminal turn result.
#[derive(Clone)]
pub struct TurnResultHandle {
    turn_id: TurnId,
    result: watch::Receiver<Option<Arc<TurnResult>>>,
    terminal_failure: watch::Receiver<Option<AgentError>>,
}

/// Complete committed result of one logical turn.
#[derive(Clone)]
pub struct TurnResult {
    context_overflow_remedy: Option<ContextOverflowRemedy>,
    turn_id: TurnId,
    outcome: TurnResultOutcome,
    recovery_operation: Option<TurnRecoveryOperationId>,
    checkpoint: Arc<RuntimeCheckpoint>,
    conversation: ConversationView,
    usage: TurnUsage,
    feed_seq: Option<crate::feed::FeedSeq>,
}

/// Exact terminal source and typed disposition for an in-turn capacity failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnContextCapacity {
    source_checkpoint: CheckpointId,
    failure: ContextCapacityFailure,
}

/// What a host can offer after a turn failed because its input did not fit the
/// model's context window or protected replay was incompatible. Product wording stays with the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ContextOverflowRemedy {
    /// The turn's own prompt did not fit and was withheld: canonical history
    /// holds a note in its place, the session admits the next prompt
    /// unchanged, and the message can be sent again smaller.
    ResendShorter,
    /// An explicit compaction may make room before the next prompt.
    Compact,
    /// Capacity or incompatible protected history requires clearing context or
    /// forking from a usable checkpoint; compaction is not an admitted remedy.
    ClearOrFork,
}

/// What a newly opened owner closed when it found an unfinished turn.
///
/// Carries the operations it abandoned so the face can name them. Its presence
/// is the fact worth reporting: the session is back, the turn was interrupted,
/// and any uncertain operations are closed without replay. Completed tool
/// results retain their evidence.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbandonedTurnReport {
    model_round_completed: bool,
    tool_round_completed: bool,
    operations: Vec<u64>,
    effects: Vec<u64>,
    uncommitted_tool_calls: Vec<(plakous_model::CallId, plakous_model::ToolName)>,
    claim: AbandonedTurnClaim,
    cause: AbandonmentCause,
}

/// The one thing an abandonment tells whoever reads the session next.
///
/// Exactly one is true of a cut, and the strongest applicable one is reported:
/// a running tool outranks an unobserved model request, which outranks an
/// unapplied compaction. A completed tool round interrupted before the next
/// request is distinct from a turn whose work never started. Saying
/// the wrong one is worse than saying nothing: a workspace warning over a
/// command that provably never started teaches an operator to ignore the
/// warning, and a billing warning over a turn that never reached a provider is
/// simply false.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum AbandonedTurnClaim {
    /// A tool, process or Code Cell was running. It may have changed the
    /// workspace, and its result was never observed.
    RunningTool,
    /// A model request whose outcome nobody observed. It may have been billed.
    ModelRequest,
    /// A compaction was in flight. It was not applied; it may have been billed.
    CompactionNotApplied,
    /// The harness was asked for a tool that exact evidence proves never started.
    ///
    /// Earlier rounds may already be in the history. The note covers only the
    /// last round.
    NothingStarted,
    /// The tool round completed, but the host was lost before preparing the
    /// next model request. Completed tool results remain in history.
    ToolRoundCompleted,
}

impl AbandonedTurnClaim {
    /// The stable lower-case name faces and logs print.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunningTool => "running_tool",
            Self::ModelRequest => "model_request",
            Self::CompactionNotApplied => "compaction_not_applied",
            Self::NothingStarted => "nothing_started",
            Self::ToolRoundCompleted => "tool_round_completed",
        }
    }
}

/// How the turn came to be unfinished.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum AbandonmentCause {
    /// The process that owned the turn was lost before it finished: a crash,
    /// a signal, a machine that stopped. No terminal result was delivered.
    ProcessLost,
    /// A live owner ended the turn as recovery-required because one outcome
    /// became uncertain. That result was already delivered; abandoning the
    /// turn delivers no second one.
    Interrupted,
}

impl AbandonmentCause {
    /// The stable lower-case name faces and logs print.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessLost => "process_lost",
            Self::Interrupted => "interrupted",
        }
    }
}

impl AbandonedTurnReport {
    pub(crate) const fn new(
        operations: Vec<u64>,
        effects: Vec<u64>,
        uncommitted_tool_calls: Vec<(plakous_model::CallId, plakous_model::ToolName)>,
        claim: AbandonedTurnClaim,
        cause: AbandonmentCause,
    ) -> Self {
        Self {
            model_round_completed: false,
            tool_round_completed: false,
            operations,
            effects,
            uncommitted_tool_calls,
            claim,
            cause,
        }
    }

    pub(crate) const fn with_completed_tool_round(mut self, completed: bool) -> Self {
        self.tool_round_completed = completed;
        self
    }

    pub(crate) const fn with_completed_model_round(mut self, completed: bool) -> Self {
        self.model_round_completed = completed;
        self
    }

    /// The closure selected a finalized text-only model boundary, not a tool round.
    #[must_use]
    pub const fn model_round_completed(&self) -> bool {
        self.model_round_completed
    }

    /// The last tool round was fully finalized, and no next model request was
    /// prepared. A process it started may still have an unknown outcome.
    #[must_use]
    pub const fn tool_round_completed(&self) -> bool {
        self.tool_round_completed
    }

    /// The operations this abandonment closed.
    #[must_use]
    pub fn operations(&self) -> &[u64] {
        &self.operations
    }

    /// Of those, the tools, processes and Code Cells that may have run.
    ///
    /// These are the ones that may have *done* something. A lost model request
    /// may have cost money; a lost tool may have written a file or started a
    /// process, and nothing afterwards can find out which.
    #[must_use]
    pub fn effects(&self) -> &[u64] {
        &self.effects
    }

    /// Call identities whose semantic exchange was left uncommitted by abandonment.
    /// Includes completed invocations and nested local-composition calls, but
    /// no arguments or payloads, so hosts can reconcile their own durable effects.
    #[must_use]
    pub fn uncommitted_tool_calls(&self) -> &[(plakous_model::CallId, plakous_model::ToolName)] {
        &self.uncommitted_tool_calls
    }

    /// The one claim the abandonment note makes.
    #[must_use]
    pub const fn claim(&self) -> AbandonedTurnClaim {
        self.claim
    }

    /// Whether the turn was cut by a lost process or fenced inside a live one.
    #[must_use]
    pub const fn cause(&self) -> AbandonmentCause {
        self.cause
    }

    /// Whether a tool was running when the turn stopped.
    #[must_use]
    pub const fn abandoned_a_tool(&self) -> bool {
        matches!(self.claim, AbandonedTurnClaim::RunningTool)
    }

    /// Whether exact evidence proves the harness-requested tool never started.
    #[must_use]
    pub const fn nothing_started(&self) -> bool {
        matches!(self.claim, AbandonedTurnClaim::NothingStarted)
    }
}

/// Closed terminal authority for one admitted root turn.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "the authoritative completed aggregate remains inline and immutable"
)]
pub enum TurnResultOutcome {
    /// The turn committed a complete provider response.
    Completed(ModelResponse),
    /// The turn stopped through joined cancellation.
    Cancelled(AgentError),
    /// The turn failed without an unresolved external outcome.
    Failed(AgentError),
    /// The turn has an exact unresolved external outcome which forbids replay.
    RecoveryRequired(AgentError),
}

struct PreparedPromptCommand {
    instruction: bool,
    command_id: CommandId,
    turn_id: TurnId,
    text: String,
    follow_up: bool,
    admitted_at: tokio::time::Instant,
    cancellation: CancellationToken,
    shared: Arc<TurnShared>,
}

#[derive(Clone)]
pub(crate) struct PromptCommand {
    instruction: bool,
    // Fixed at admission (or restored from the continuation root): a config
    // change applies to the next turn, never to one already running.
    max_model_rounds: usize,
    command_id: CommandId,
    turn_id: TurnId,
    text: String,
    follow_up: bool,
    operation_id: OperationId,
    binding: Digest,
    admitted_at: tokio::time::Instant,
    cancellation: CancellationToken,
    shared: Arc<TurnShared>,
}

#[derive(Clone, Copy)]
enum TurnTerminalOutcome {
    Completed,
    Failed(AgentErrorKind),
}

enum TurnLifecycle {
    Admitted(Mutex<TurnMachine>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnSettlement {
    Pending,
    Terminalized,
}

enum TerminalProposal {
    Completed(Box<TurnResult>),
    Cancelled(Box<AgentError>),
    Failed(Box<AgentError>),
    RecoveryRequired(Box<AgentError>),
}

#[derive(Clone, Copy)]
struct TerminalTransitionPlan {
    transitions: [TurnTransition; 2],
    len: usize,
}

struct PreparedTerminalLifecycle {
    lifecycle: TurnMachine,
}

pub(crate) struct TurnShared {
    turn_id: TurnId,
    lifecycle: TurnLifecycle,
    settlement: watch::Sender<TurnSettlement>,
    cancellation: CancellationToken,
    result: watch::Sender<Option<Arc<TurnResult>>>,
    terminal_failure: watch::Sender<Option<AgentError>>,
    events: EventPipeline,
    usage: Mutex<TurnUsage>,
    telemetry: Option<TurnTelemetry>,
    #[cfg(test)]
    terminal_fault: AtomicU64,
    #[cfg(test)]
    published_results: AtomicU64,
    #[cfg(test)]
    binding_lookup_fault: AtomicBool,
}

struct TurnTelemetry {
    emitter: Arc<TelemetryEmitter>,
    context: TelemetryContext,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    started: Mutex<Option<tokio::time::Instant>>,
    compartments: Mutex<TurnTelemetryCompartments>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TurnTelemetryCompartments {
    model_wall: Duration,
    tool_wall: Duration,
    compaction_wall: Duration,
    model_attempts: u32,
    tool_calls: u32,
    compactions: u32,
}

/// Stable typed identity of an unresolved root operation requiring recovery.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TurnRecoveryOperationId(u64);

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
struct OperationId(u64);

enum ArtifactTransition {
    Preserve,
    Replace {
        candidate: SealedFamilyArtifactCandidate,
        plan: Box<ValidatedRequestPlan>,
        response_output_items: usize,
    },
    Clear,
}

#[derive(Clone)]
struct CommandLedgerEntry {
    binding: Digest,
    acknowledgement: CommandAck,
    admission_reclaimed: bool,
    effect: ActorCommandEffect,
    result: Option<TurnResultHandle>,
    terminal: Option<DurableTerminalResultRecord>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
enum DurableActorCommandEffect {
    Root,
    CancelRequested,
    CancelAlreadyRequested,
    CancelAlreadyTerminal,
    EffectDecisionAllowed,
    EffectDecisionDenied,
    SteerQueued { position: u32 },
    Shutdown,
    Rejected,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct DurableControlRejection {
    kind: String,
    message: String,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct DurableCommandEnvelope {
    version: u32,
    canonical_input: Vec<u8>,
    effect: DurableActorCommandEffect,
    rejection: Option<DurableControlRejection>,
}

#[derive(Clone)]
struct QueuedSteering {
    text: String,
    operation_id: OperationId,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct ToolExecutionResult {
    pub(crate) result: ToolResultBlock,
    dispatched: bool,
    /// Host-only presentation subject. Not part of [`Self::result`].
    /// Omitted from the operation-evidence encoding when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) presentation_subject: Option<String>,
}

impl ToolExecutionResult {
    fn new(result: ToolResultBlock, dispatched: bool) -> Self {
        Self {
            result,
            dispatched,
            presentation_subject: None,
        }
    }

    fn with_presentation_subject(mut self, subject: Option<String>) -> Self {
        self.presentation_subject = subject.filter(|subject| !subject.is_empty());
        self
    }
}

struct RestoredStartup {
    frame: RestoredContinuationFrame,
    effect: Option<RestoredEffectDecision>,
    command_id: CommandId,
    lifecycle: TurnMachine,
}
struct DriverRestoredStartup {
    frame: RestoredContinuationFrame,
    effect: Option<RestoredEffectDecision>,
    command: PromptCommand,
}
struct RestoredEffectDecision {
    invocation: ToolInvocation,
    projection_receipt: Option<ToolProjectionReceiptV2>,
    pending: PendingEffectDecision,
}
