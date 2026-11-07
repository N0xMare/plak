//! Single-owner event pump and private mutable actor state.

use crate::local_composition::LocalCompositionResponse;
use crate::runtime::checkpoint::identity::deterministic_checkpoint_id;
use crate::runtime::checkpoint::owner::{CheckpointOwner, StartAuthority};
use crate::runtime::cleanup::{combine_cleanup_results, contain_cleanup_panic};
use crate::runtime::composition::CompositionSlot;
use crate::runtime::config::AgentConfig;
use crate::runtime::operation_record::TurnBudgets;
use crate::runtime::terminal::{
    actor_panic_error, cancelled_error, recovery_gate_error, remove_known_turn,
};
use crate::runtime::{
    CommandLedgerEntry, DriverRestoredStartup, OperationId, PromptCommand, QueuedSteering,
    TerminalProposal, ToolExecutionResult, ToolPresentation, TurnShared,
};
use crate::tool_runtime::{
    AuthorizedToolInvocation, EffectDispatchIntent, PendingEffectDecision, ToolRuntime,
};
use crate::{
    ActorControl, AgentError, AgentErrorKind, AgentSessionEndpointError, CheckpointId, CommandId,
    ControlError, ControlErrorKind, DurableOperationState, EffectDecisionRef,
    EffectDecisionResolution, EventContext, EventHub, EventPipeline, StoreAdmission,
    SubscriptionLimits, TelemetryContext, TelemetryEmitter, ToolAuditIdentity,
    ToolCallAttemptIdentity, ToolCallObservation, ToolCallerLineageView, TurnCommand,
};
use admitted_round::AdmittedRound;
use futures_util::FutureExt;
use plakous_model::{
    AttemptId, CancellationToken, Clock, CompiledProviderConfiguration, ConversationItem,
    ConversationView, Digest, DriverInstanceId, EntryId, IdGenerator, LanguageModel, ModelError,
    ModelErrorPhase, ModelErrorTarget, ModelOperationRoot, ModelOperationScope, ModelRequest,
    ModelResponse, RunId, StableRequestPrefix, ToolCallBlock, ToolEffect, ToolResultBlock, TurnId,
    Usage, ValidatedRequestPlan,
};
use plakous_tools::{ProgressSink, ToolInvocation, ToolProjectionReceiptV2, ToolRuntimeErrorKind};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
mod admitted_round;
mod cleanup;
mod commands;
mod compaction;
mod composition;
mod context;
mod effects;
#[cfg(test)]
pub(crate) mod exit_probe;
mod model;
pub(super) use model::completed_history_prefix;
mod notices;
pub(super) use notices::is_process_notice_batch;
mod observation;
mod operations;
mod placeholders;
mod residency;
mod spawn;
mod terminal;
mod tools;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ToolDispatchOrigin {
    TopLevel,
    NestedComposition,
}

mod credential_wait;
mod operator_input;
mod reresolution;
mod turn;
pub(in crate::runtime) mod turn_position;
#[cfg(test)]
pub(crate) use compaction::ledger_paid_fold_failed_since_last_round;
pub(crate) use compaction::paid_fold_failed_since_last_round;
pub(crate) use operator_input::last_operator_input_unix_ms;
pub use turn::OPERATION_BUDGET_WRAP_UP_NOTE;

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-turn recovery and paid-fold guards must persist separately"
)]
struct Driver {
    agent_id: DriverInstanceId,
    model: Arc<dyn LanguageModel>,
    provider_configuration: Arc<CompiledProviderConfiguration>,
    /// Present only for a model resolved against windowed evidence.
    model_rebind: Option<super::reresolution::ModelRebind>,
    model_error_target: ModelErrorTarget,
    tools: ToolRuntime,
    tool_presentation: ToolPresentation,
    config: AgentConfig,
    stable_request_prefix: StableRequestPrefix,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    run_id: RunId,
    events: EventPipeline,
    /// A reopen's output-bound change, published on the first turn.
    ///
    /// The hosted product subscribes to turn events only, so an agent-scoped
    /// publication reaches nothing. This rides the first turn instead, which is
    /// the first moment anything is listening.
    pending_output_bound_rederived: Option<crate::manifest::OutputBoundRederived>,
    event_hub: EventHub,
    event_limits: SubscriptionLimits,
    authority: CheckpointOwner,
    known_turns: Arc<Mutex<BTreeMap<TurnId, Weak<TurnShared>>>>,
    provider_dispatch_gate: Arc<AsyncMutex<()>>,
    control_sender: mpsc::Sender<ActorControl>,
    control_receiver: mpsc::Receiver<ActorControl>,
    #[cfg(test)]
    exit_probe: Arc<exit_probe::ExitProbe>,
    steering: VecDeque<QueuedSteering>,
    tool_preempt: tokio::sync::watch::Sender<plakous_tools::ToolPreempt>,
    dispatch_preempts: Arc<Mutex<Vec<tokio::sync::watch::Sender<plakous_tools::ToolPreempt>>>>,
    nested_notice_preempts: Arc<Mutex<Vec<tokio::sync::watch::Sender<plakous_tools::ToolPreempt>>>>,
    roots: VecDeque<PromptCommand>,
    active_root: Option<PromptCommand>,
    retention_due: Option<TurnId>,
    last_compaction_diagnostic: Option<String>,
    last_compaction_successor: Option<Digest>,
    active_model_root_scope: Option<Box<dyn ModelOperationScope>>,
    restored: Option<DriverRestoredStartup>,
    compaction_in_flight: bool,
    /// The active root turn's guards and round count (spec 01 §5.5). Its
    /// `paid_fold_spent` is set once a paid fold in the turn failed to
    /// commit: every later scheduled compaction in the same turn uses the
    /// free placeholder rung, so a slow or failing summarizer cannot bill
    /// once per tool group.
    turn_budgets: TurnBudgets,
    /// Pressure-forced paid folds committed in the active root turn by this
    /// driver; capped by `COMMITTED_PRESSURE_FOLD_CAP`, reset with the turn.
    pressure_folds_committed: usize,
    /// Worst-case append debt of accepted top-level calls not yet joined.
    reserved_top_level_operations: usize,
    /// The family-artifact binding a round of this driver produced, with that
    /// round's stable prefix. The manifest keeps only the binding digest, so
    /// this is how a later round names which prefix field drifted.
    artifact_producer_prefix: Option<(Digest, StableRequestPrefix)>,
    /// The last provider input count with the estimate of the request it
    /// counted; cleared by a committed compaction (spec 01, 10.3).
    context_anchor: Option<plakous_model::ContextAnchor>,
    request_anchor: Option<plakous_model::ContextRequestAnchor>,
    /// `ceil(utf8/4)` of nested local-composition results, which canonical
    /// `ToolExchange` cannot hold. Fence (b) adds it to the parts estimate;
    /// it resets when a round sets a new anchor or a compaction commits.
    nested_committed_observation_tokens: u64,
    seed_compaction_pending: bool,
    /// Derives the note a committed compaction leaves behind.
    post_compaction_note: Option<crate::runtime::PostCompactionNote>,
    /// The host's shared bound on concurrent model requests, if any.
    request_slots: Option<crate::runtime::RequestSlots>,
    request_slot: Option<crate::runtime::request_slots::RequestSlot>,
    credential_wait_elapsed: Duration,
    credential_wait: Arc<Mutex<Option<super::CredentialWait>>>,
    effect_decision_tombstones: VecDeque<EffectDecisionTombstone>,
    command_ledger: BTreeMap<CommandId, CommandLedgerEntry>,
    next_command_sequence: u64,
    next_operation_sequence: u64,
    next_checkpoint_sequence: u64,
    process_session_cleanup_uncertainty: Option<TurnId>,
    pending_job_promotions: Vec<crate::manifest::JobRow>,
    pending_named_state: Option<(OperationId, crate::manifest::NamedState)>,
    pending_job_consumptions: Vec<u32>,
    noticed_job_terminals: Vec<u32>,
    observed_job_terminals: Vec<u32>,
    job_sessions: plakous_tools::JoinedProcessSessions,
    job_host: Option<Arc<dyn plakous_tools::JobHost>>,
    job_owner: plakous_tools::JobOwner,
    job_release_mode: Arc<Mutex<super::JobReleaseMode>>,
    recovery_gated: bool,
    observation_budget: crate::runtime::observation::budget::ObservationBudget,
    accepting: Arc<AtomicBool>,
    command_nonce: Arc<AtomicU64>,
    shutdown: CancellationToken,
    telemetry: Arc<TelemetryEmitter>,
    telemetry_context: TelemetryContext,
    local_composition: CompositionSlot,
    /// The turn whose user prompt is in canonical history. A cancelled or
    /// failed turn in it gets a note after the prompt; one that ended before
    /// its prompt was appended has nothing to annotate.
    prompt_in_history: Option<TurnId>,
    /// The restored turn this driver is running, and the hosted tool-batch
    /// or server-pause baton it restored, from its restored admission until
    /// its terminal. A cancel of that frame finalizes the baton. A
    /// continuation opened live in this process is not named here, so
    /// cancellation leaves it for recovery (spec 01 §3.5, §5.5).
    restored_turn: Option<(TurnId, Option<OperationId>)>,
    /// The entry this root appended as its prompt, unknown for a resumed
    /// continuation whose prompt an earlier frame appended.
    appended_prompt: Option<EntryId>,
    /// The turn the segment limit refused (spec 01 §10.3). Its prompt is never
    /// withheld: a note in the prompt's place keeps the segment count.
    segment_refused_turn: Option<TurnId>,
    /// The next round's request as the tool exchange's capacity admission
    /// prepared it, kept so the round dispatches it instead of preparing the
    /// same request again.
    admitted_round: Option<Box<AdmittedRound>>,
    /// Per-driver salt for access digests, drawn only when telemetry is
    /// enabled. Never persisted; `None` stores no digest.
    access_salt: Option<[u8; 32]>,
    /// Instruction and tool schema bytes of the stable request prefix,
    /// measured once when telemetry is enabled.
    request_footprint: Option<(u64, u64)>,
    /// When this driver last sent a provider request, for the idle gap.
    last_provider_request_at: Option<crate::executor::Instant>,
}

impl Driver {
    fn start_authority(&self) -> StartAuthority {
        self.authority.start_authority(self.shutdown.clone())
    }

    async fn run(mut self) -> Result<(), AgentError> {
        let mut run_result = match Box::pin(AssertUnwindSafe(self.run_loop()).catch_unwind()).await
        {
            Ok(result) => result,
            Err(_) => Err(actor_panic_error()),
        };
        #[cfg(test)]
        self.exit_probe.at_exit().await;
        // Exit law: close the ingress, settle every remaining root, then
        // answer what is queued. A `Stale` cancel then names a turn whose
        // terminal is published.
        self.control_receiver.close();
        let settlement = Box::pin(contain_cleanup_panic(self.settle_roots(&mut run_result))).await;
        self.refuse_queued_controls().await;
        let cleanup = Box::pin(contain_cleanup_panic(self.cleanup_session())).await;
        combine_cleanup_results([run_result, settlement, cleanup])
    }

    /// Answers each control left in the closed ingress with a typed refusal
    /// that records nothing; a dropped reply reads as an unknown outcome.
    /// `recv` returns `None` only once no sender still holds a permit, so a
    /// send that reserved before the close is answered too.
    async fn refuse_queued_controls(&mut self) {
        while let Some(control) = self.control_receiver.recv().await {
            self.refuse_control(control);
        }
    }

    /// Whether `turn_id` is still admitted: a turn leaves `known_turns` only
    /// once its terminal is published, so one whose settlement failed stays.
    fn still_admitted(&self, turn_id: TurnId) -> bool {
        self.known_turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&turn_id)
    }

    fn refuse_control(&self, control: ActorControl) {
        let stopped = || ControlError::new(ControlErrorKind::Shutdown, "agent stopped");
        match control {
            ActorControl::Notice { reply, .. } | ActorControl::Background { reply, .. } => {
                let _ = reply.send(Err(stopped()));
            }
            ActorControl::RecordJobTerminals { reply } => {
                let _ = reply.send(Err(AgentError::new(
                    AgentErrorKind::Shutdown,
                    "agent stopped",
                )));
            }
            ActorControl::Command { command, reply } => {
                let error = match command {
                    TurnCommand::Cancel { turn_id, .. } if !self.still_admitted(turn_id) => {
                        ControlError::new(ControlErrorKind::Stale, "turn is no longer admitted")
                    }
                    _ => stopped(),
                };
                let _ = reply.send(self.unrecorded_rejection(
                    command.command_id().clone(),
                    command.target_turn_id(),
                    error,
                ));
            }
            ActorControl::ContextStatus { reply, .. } => {
                let _ = reply.send(Err(AgentSessionEndpointError::Unavailable));
            }
            ActorControl::CompactSession { reply, .. } => {
                let _ = reply.send(Err(AgentSessionEndpointError::Unavailable));
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The owner pump selects commands, turn work, and shutdown while keeping each branch under the same driver borrow"
    )]
    async fn run_loop(&mut self) -> Result<(), AgentError> {
        if let Some(restored) = self.restored.take() {
            Box::pin(self.run_restored_startup(restored)).await?;
        }
        loop {
            self.authority.check_heartbeat()?;
            let Some(command) = Box::pin(self.next_command()).await? else {
                if self.recovery_gated {
                    return self.wait_recovery_gate().await;
                }
                self.cancel_queued().await?;
                return Ok(());
            };
            self.active_root = Some(command.clone());
            #[cfg(test)]
            self.exit_probe.strike()?;
            if self.shutdown.is_cancelled() {
                self.commit_and_publish_terminal(
                    &command,
                    TerminalProposal::Cancelled(Box::new(cancelled_error())),
                )
                .await?;
                self.active_root = None;
                self.cancel_queued().await?;
                return Ok(());
            }
            match command.shared.start() {
                Ok(true) => {}
                Ok(false) => {
                    return Err(AgentError::new(
                        AgentErrorKind::Internal,
                        "admitted root entered execution without a publishable lifecycle",
                    ));
                }
                Err(error) => {
                    self.commit_and_publish_terminal(
                        &command,
                        TerminalProposal::Failed(Box::new(error.clone())),
                    )
                    .await?;
                    self.active_root = None;
                    remove_known_turn(&self.known_turns, command.turn_id);
                    return Err(error);
                }
            }
            let model_root_scope = Arc::clone(&self.model)
                .begin_operation_root(ModelOperationRoot::Turn(command.turn_id))
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation));
            match model_root_scope {
                Ok(scope) => self.active_model_root_scope = Some(scope),
                Err(error) => {
                    self.commit_and_publish_terminal(
                        &command,
                        TerminalProposal::Failed(Box::new(error.clone())),
                    )
                    .await?;
                    self.active_root = None;
                    remove_known_turn(&self.known_turns, command.turn_id);
                    return Err(error);
                }
            }
            let operation_id = self.root_operation_for_command(&command)?;
            if let Err(error) = self
                .transition_operation_as(
                    StoreAdmission::Settlement,
                    command.turn_id,
                    operation_id,
                    DurableOperationState::Dispatched,
                )
                .await
            {
                let proposal = TerminalProposal::from_result(Err(error.clone()));
                self.commit_and_publish_terminal(&command, proposal).await?;
                self.active_root = None;
                self.active_model_root_scope = None;
                remove_known_turn(&self.known_turns, command.turn_id);
                return Err(error);
            }
            self.begin_turn(&command);
            let (result, fatal_error) = self.run_admitted_turn(&command).await;
            let proposal = if self.recovery_gated {
                TerminalProposal::RecoveryRequired(Box::new(recovery_gate_error()))
            } else {
                TerminalProposal::from_result(result)
            };
            let recovery_required = proposal.recovery_required();
            self.commit_and_publish_terminal(&command, proposal).await?;
            self.active_root = None;
            self.active_model_root_scope = None;
            remove_known_turn(&self.known_turns, command.turn_id);
            if self.recovery_gated {
                return self.wait_recovery_gate().await;
            }
            if recovery_required {
                self.accepting.store(false, Ordering::Release);
                self.cancel_queued().await?;
                return Ok(());
            }
            if let Some(error) = fatal_error {
                self.cancel_queued().await?;
                return Err(error);
            }
            if self.shutdown.is_cancelled() {
                self.cancel_queued().await?;
                return Ok(());
            }
        }
    }

    fn conversation(&self) -> ConversationView {
        self.authority.conversation()
    }

    fn entry_id(&self) -> Result<EntryId, AgentError> {
        EntryId::generate(self.clock.as_ref(), self.ids.as_ref())
            .map_err(|error| AgentError::from_identity(&error))
    }

    fn attempt_id(&self) -> Result<AttemptId, AgentError> {
        AttemptId::generate(self.clock.as_ref(), self.ids.as_ref())
            .map_err(|error| AgentError::from_identity(&error))
    }

    fn checkpoint_id(&mut self) -> Result<CheckpointId, AgentError> {
        let sequence = self.next_checkpoint_sequence;
        self.next_checkpoint_sequence =
            self.next_checkpoint_sequence
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "in-memory checkpoint identity space was exhausted",
                    )
                })?;
        deterministic_checkpoint_id(self.run_id, sequence)
    }

    fn take_operation_id(&mut self) -> Result<OperationId, AgentError> {
        let operation_id = OperationId(self.next_operation_sequence);
        self.next_operation_sequence =
            self.next_operation_sequence.checked_add(1).ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "in-memory operation identity space was exhausted",
                )
            })?;
        Ok(operation_id)
    }
}

pub(in crate::runtime) mod execution;

pub(in crate::runtime) mod model_cleanup;

#[cfg(test)]
pub(in crate::runtime) use operations::TerminalPromptTail;
pub(in crate::runtime) use operations::{
    failure_preserves_artifact, failure_prompt_tail, trailing_prompt,
};

#[derive(Clone, Debug)]
pub(in crate::runtime) struct EffectDecisionTombstone {
    decision: EffectDecisionRef,
    resolution: EffectDecisionResolution,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct DurableEffectIntentEnvelope {
    pub(in crate::runtime) intent_digest: Digest,
    pub(crate) call: ToolCallBlock,
    intent: Vec<u8>,
}

pub(crate) type DurableUndispatchedToolRequest =
    (String, Digest, ToolCallBlock, EntryId, ToolExecutionResult);

pub(in crate::runtime) struct ProgressForwardingGuard(ProgressSink);

pub(in crate::runtime) struct ToolDispatchTelemetry {
    emitter: Arc<TelemetryEmitter>,
    context: TelemetryContext,
    clock: Arc<dyn Clock>,
    access_salt: Option<[u8; 32]>,
}

pub(in crate::runtime) struct ActorToolDispatchObserver {
    authority: StartAuthority,
    cancellation: CancellationToken,
    shared: Arc<TurnShared>,
    event_context: EventContext,
    audit_identity: ToolAuditIdentity,
    lineage: ToolCallerLineageView,
    observation: ToolCallObservation,
    telemetry: Arc<TelemetryEmitter>,
    telemetry_context: TelemetryContext,
    clock: Arc<dyn Clock>,
    access_salt: Option<[u8; 32]>,
    started: Mutex<Option<crate::executor::Instant>>,
    dispatch_witness: Arc<AtomicBool>,
    error: Mutex<Option<AgentError>>,
}

pub(in crate::runtime) struct AdmittedToolCall {
    operation_id: OperationId,
    index: usize,
    call: ToolCallBlock,
    result_entry_id: EntryId,
    audit_identity: ToolAuditIdentity,
    execution: AdmittedToolExecution,
}

pub(in crate::runtime) enum AdmittedToolExecution {
    ReadOnly {
        invocation: Box<ToolInvocation>,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        authorized: Box<AuthorizedToolInvocation>,
        effect: ToolEffect,
    },
    Mutating {
        invocation: Box<ToolInvocation>,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        authorized: Box<AuthorizedToolInvocation>,
        intent: Box<EffectDispatchIntent>,
    },
}

#[allow(
    clippy::large_enum_variant,
    reason = "the pending variant uniquely owns the exact non-cloneable authorization pause"
)]
pub(in crate::runtime) enum ToolAdmission {
    RejectedBeforeValidation {
        execution: ToolExecutionResult,
        attempt: ToolCallAttemptIdentity,
        reason: ToolRuntimeErrorKind,
        /// The rejection's descriptive message.
        ///
        /// Every schema refusal shares one category, so the category alone
        /// cannot say which constraint the call missed. Carried here so the
        /// observation the driver publishes can name it.
        detail: String,
    },
    RejectedAfterValidation {
        execution: ToolExecutionResult,
    },
    Admitted {
        execution: Box<AdmittedToolExecution>,
        audit_identity: ToolAuditIdentity,
    },
    DecisionRequired {
        invocation: Box<ToolInvocation>,
        projection_receipt: Option<ToolProjectionReceiptV2>,
        pending: Box<PendingEffectDecision>,
        audit_identity: ToolAuditIdentity,
    },
}

pub(in crate::runtime) struct IndexedToolExecution {
    operation_id: OperationId,
    index: usize,
    call: ToolCallBlock,
    result_entry_id: EntryId,
    audit_identity: ToolAuditIdentity,
    lineage: ToolCallerLineageView,
    dispatched: bool,
    process_session_update: Option<plakous_tools::internal::ProcessSessionUpdate>,
    result: Result<ToolExecutionResult, AgentError>,
}

pub(in crate::runtime) struct MutatingExecution {
    item: ConversationItem,
    dispatched: bool,
    value: Option<plakous_tools::ExecutionValue>,
}

pub(in crate::runtime) struct IndexedReadOnlyRequest {
    authority: StartAuthority,
    operation_id: OperationId,
    tools: ToolRuntime,
    tool_presentation: ToolPresentation,
    invocation: ToolInvocation,
    projection_receipt: Option<ToolProjectionReceiptV2>,
    authorized: AuthorizedToolInvocation,
    audit_identity: ToolAuditIdentity,
    lineage: ToolCallerLineageView,
    index: usize,
    call: ToolCallBlock,
    result_entry_id: EntryId,
    cancellation: CancellationToken,
    shutdown: CancellationToken,
    preempt: tokio::sync::watch::Receiver<plakous_tools::ToolPreempt>,
    preempt_guard: Option<commands::DispatchPreemptGuard>,
    clock: Arc<dyn Clock>,
    progress_capacity: usize,
    events: EventPipeline,
    turn_id: TurnId,
    attempt_id: AttemptId,
    telemetry: Arc<TelemetryEmitter>,
    telemetry_context: TelemetryContext,
    turn_shared: Arc<TurnShared>,
    dispatch_witness: Arc<AtomicBool>,
    process_session_update: Arc<Mutex<Option<plakous_tools::internal::ProcessSessionUpdate>>>,
    access_salt: Option<[u8; 32]>,
}

pub(in crate::runtime) type IndexedToolFuture =
    Pin<Box<dyn Future<Output = IndexedToolExecution> + Send + 'static>>;

pub(in crate::runtime) struct ToolBatchResult {
    items: Vec<ConversationItem>,
    dispatched: usize,
    operations: Vec<OperationId>,
}

pub(in crate::runtime) struct LocalCompositionOuterExecution {
    item: ConversationItem,
    dispatched: usize,
    operations: Vec<OperationId>,
}

pub(in crate::runtime) struct LocalCompositionCallResponse {
    response: LocalCompositionResponse,
    terminal_state: Option<(String, crate::manifest::NamedState)>,
    max_output_tokens: usize,
    wall_time: Duration,
}

pub(in crate::runtime) enum ModelStartAdmission {
    Pending(plakous_model::StartModelCallWithControl),
    Ready(Result<plakous_model::ModelControlStream, ModelError>),
}

/// What one model round dispatches.
enum RoundRequest {
    /// A request the round validates and prepares itself.
    Fresh(Box<ModelRequest>),
    /// The request the tool exchange's admission prepared.
    Admitted(Box<AdmittedRound>),
}

pub(in crate::runtime) struct ModelOperationResult {
    request_context_status: Option<Box<plakous_model::ContextStatus>>,
    result: Result<ModelResponse, AgentError>,
    usage: Option<Usage>,
    operation_id: Option<OperationId>,
    plan: Option<ValidatedRequestPlan>,
    /// Whole-request `bytes/4` estimate of the body that was sent.
    request_estimate_tokens: Option<u64>,
    request_wire_input: Option<plakous_model::ContextWireInput>,
    /// Prepared wire output maximum, when preparation recorded one.
    wire_output_limit: Option<std::num::NonZeroU32>,
}

pub(in crate::runtime) enum AuthorizationOutcome {
    Authorized(Box<AuthorizedToolInvocation>),
    Pending(Box<PendingEffectDecision>),
    Rejected(ToolResultBlock),
}
