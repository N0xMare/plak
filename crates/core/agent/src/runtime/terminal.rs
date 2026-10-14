//! Single-terminal transitions, result records and error reconstruction.

use crate::manifest::RuntimeCheckpoint;
use crate::runtime::cleanup::error_with_cleanup_disposition;
use crate::runtime::continuation::{runtime_component, terminal_record_at_checkpoint};
use crate::runtime::control::usage_error;
use crate::runtime::{
    OperationId, PreparedTerminalLifecycle, TerminalProposal, TerminalTransitionPlan,
    TurnLifecycle, TurnResult, TurnResultOutcome, TurnSettlement, TurnShared, TurnTerminalOutcome,
};
use crate::session::{
    SessionStoreCapacity, StoreAdmission, StoreCapacityBound, StoreIntegrityReason,
    StoreInvalidReason,
};
use crate::state::{
    CancellationResolution, FailureResolution, TerminalOutcome, TurnMachine, TurnPhase,
    TurnStateError, TurnTransition,
};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, CheckpointId, CommandId, EventContext,
    ReconstructiveComponent, RuntimeComponentKind, SessionStoreError, TelemetryObservation,
    TelemetryObservationKind, TelemetryTerminalStatus, ToolAuditIdentity, ToolCallObservation,
    ToolCallerLineageView, TurnUsage,
};
use plakous_model::{CancellationToken, Digest, ModelError, ModelResponse, TurnId, Usage};
use plakous_tools::{ToolOutcome, ToolOutcomeKind, ToolRuntimeError, ToolRuntimeErrorKind};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::watch;
pub(super) fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn queued_turn_machine(turn_id: TurnId) -> Result<TurnMachine, AgentError> {
    let mut lifecycle = TurnMachine::new(turn_id);
    apply_turn_transition(&mut lifecycle, TurnTransition::Enqueue)?;
    Ok(lifecycle)
}

fn apply_turn_transition(
    lifecycle: &mut TurnMachine,
    transition: TurnTransition,
) -> Result<crate::state::TransitionRecord, AgentError> {
    let turn_id = lifecycle.turn_id();
    let previous = lifecycle.phase();
    let record = lifecycle
        .transition(transition)
        .map_err(project_turn_state_error)?;
    if record.turn_id() != turn_id
        || record.previous() != previous
        || record.current() != lifecycle.phase()
        || record.terminal()
            != lifecycle
                .terminal()
                .map(crate::state::TerminalTurn::outcome)
    {
        return Err(turn_lifecycle_error());
    }
    Ok(record)
}

fn project_turn_state_error(error: TurnStateError) -> AgentError {
    let _kind = error.kind();
    turn_lifecycle_error()
}

pub(super) fn turn_lifecycle_error() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "turn lifecycle rejected an illegal production transition",
    )
}

fn terminal_outcome_matches_result<T>(
    outcome: TerminalOutcome,
    result: &Result<T, AgentError>,
) -> bool {
    match (outcome, result) {
        (TerminalOutcome::Completed, Ok(_)) => true,
        (TerminalOutcome::Cancelled, Err(error)) => error.kind() == AgentErrorKind::Cancelled,
        (TerminalOutcome::OutcomeUnknown, Err(error)) => {
            error.kind() == AgentErrorKind::OutcomeUnknown
        }
        (TerminalOutcome::Failed, Err(error)) => !matches!(
            error.kind(),
            AgentErrorKind::Cancelled | AgentErrorKind::OutcomeUnknown
        ),
        (
            TerminalOutcome::Completed
            | TerminalOutcome::Cancelled
            | TerminalOutcome::Failed
            | TerminalOutcome::OutcomeUnknown,
            _,
        ) => false,
    }
}

pub(super) fn agent_terminal_status<T>(
    result: &Result<T, AgentError>,
    shutting_down: bool,
) -> TelemetryTerminalStatus {
    match result {
        Ok(_) => TelemetryTerminalStatus::Completed,
        Err(error) if error.kind() == AgentErrorKind::Shutdown => TelemetryTerminalStatus::Shutdown,
        Err(error) if error.kind() == AgentErrorKind::Cancelled && shutting_down => {
            TelemetryTerminalStatus::Shutdown
        }
        Err(error) if error.kind() == AgentErrorKind::Cancelled => {
            TelemetryTerminalStatus::Cancelled
        }
        Err(_) => TelemetryTerminalStatus::Failed,
    }
}

fn turn_terminal_status(
    outcome: TurnTerminalOutcome,
    shutting_down: bool,
) -> TelemetryTerminalStatus {
    match outcome {
        TurnTerminalOutcome::Completed => TelemetryTerminalStatus::Completed,
        TurnTerminalOutcome::Failed(AgentErrorKind::Shutdown) => TelemetryTerminalStatus::Shutdown,
        TurnTerminalOutcome::Failed(AgentErrorKind::Cancelled) if shutting_down => {
            TelemetryTerminalStatus::Shutdown
        }
        TurnTerminalOutcome::Failed(AgentErrorKind::Cancelled) => {
            TelemetryTerminalStatus::Cancelled
        }
        TurnTerminalOutcome::Failed(_) => TelemetryTerminalStatus::Failed,
    }
}

pub(super) fn tool_terminal_status(
    result: &Result<ToolOutcome, ToolRuntimeError>,
    shutting_down: bool,
) -> TelemetryTerminalStatus {
    match result {
        Ok(outcome) if outcome.kind() == ToolOutcomeKind::Success => {
            TelemetryTerminalStatus::Completed
        }
        Err(error) if error.kind() == ToolRuntimeErrorKind::Cancelled && shutting_down => {
            TelemetryTerminalStatus::Shutdown
        }
        Err(error) if error.kind() == ToolRuntimeErrorKind::Cancelled => {
            TelemetryTerminalStatus::Cancelled
        }
        Ok(_) | Err(_) => TelemetryTerminalStatus::Failed,
    }
}

impl TerminalProposal {
    pub(super) fn from_result(result: Result<TurnResult, AgentError>) -> Self {
        match result {
            Ok(result) => Self::Completed(Box::new(result)),
            Err(error) => match error.kind() {
                AgentErrorKind::Cancelled => Self::Cancelled(Box::new(error)),
                AgentErrorKind::OutcomeUnknown => Self::RecoveryRequired(Box::new(error)),
                _ => Self::Failed(Box::new(error)),
            },
        }
    }

    pub(super) const fn recovery_required(&self) -> bool {
        matches!(self, Self::RecoveryRequired(_))
    }

    pub(super) fn with_cleanup_failure(self, cleanup: AgentError) -> Self {
        let error = match self {
            Self::Completed(_) => cleanup,
            Self::Cancelled(primary) | Self::Failed(primary) | Self::RecoveryRequired(primary) => {
                error_with_cleanup_disposition(*primary, &[cleanup])
            }
        };
        Self::RecoveryRequired(Box::new(error))
    }
}

impl TerminalTransitionPlan {
    pub(super) const fn one(transition: TurnTransition) -> Self {
        Self {
            transitions: [transition, transition],
            len: 1,
        }
    }

    pub(super) const fn two(first: TurnTransition, second: TurnTransition) -> Self {
        Self {
            transitions: [first, second],
            len: 2,
        }
    }

    pub(super) fn transitions(&self) -> &[TurnTransition] {
        &self.transitions[..self.len]
    }
}

impl TurnShared {
    pub(crate) const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(super) fn start(&self) -> Result<bool, AgentError> {
        let TurnLifecycle::Admitted(lifecycle) = &self.lifecycle;
        let mut lifecycle = lock_mutex(lifecycle);
        if lifecycle.phase() != TurnPhase::Queued {
            return Ok(false);
        }
        apply_turn_transition(&mut lifecycle, TurnTransition::BeginPreparing)?;
        Ok(true)
    }

    pub(super) fn mark_telemetry_started(&self, started: tokio::time::Instant) {
        if let Some(telemetry) = &self.telemetry {
            *lock_mutex(&telemetry.started) = Some(started);
        }
    }

    pub(super) fn record_model_telemetry(&self, elapsed: Duration) {
        if let Some(telemetry) = &self.telemetry {
            let mut compartments = lock_mutex(&telemetry.compartments);
            compartments.model_wall = compartments.model_wall.saturating_add(elapsed);
            compartments.model_attempts = compartments.model_attempts.saturating_add(1);
        }
    }

    pub(super) fn record_tool_telemetry(&self, elapsed: Duration) {
        if let Some(telemetry) = &self.telemetry {
            let mut compartments = lock_mutex(&telemetry.compartments);
            compartments.tool_wall = compartments.tool_wall.saturating_add(elapsed);
            compartments.tool_calls = compartments.tool_calls.saturating_add(1);
        }
    }

    pub(super) fn record_compaction_telemetry(&self, elapsed: Duration) {
        if let Some(telemetry) = &self.telemetry {
            let mut compartments = lock_mutex(&telemetry.compartments);
            compartments.compaction_wall = compartments.compaction_wall.saturating_add(elapsed);
            compartments.compactions = compartments.compactions.saturating_add(1);
        }
    }

    pub(super) fn is_terminal(&self) -> bool {
        let TurnLifecycle::Admitted(lifecycle) = &self.lifecycle;
        matches!(
            lock_mutex(lifecycle).phase(),
            TurnPhase::Completed
                | TurnPhase::Cancelled
                | TurnPhase::Failed
                | TurnPhase::OutcomeUnknown
        )
    }

    pub(super) fn has_terminal_failure(&self) -> bool {
        self.terminal_failure.borrow().is_some()
    }

    pub(crate) fn is_settled(&self) -> bool {
        *self.settlement.borrow() != TurnSettlement::Pending
            || self.terminal_failure.borrow().is_some()
    }

    pub(crate) async fn wait_until_settled(&self) {
        let mut settlement = self.settlement.subscribe();
        let mut terminal_failure = self.terminal_failure.subscribe();
        loop {
            if self.is_settled() {
                return;
            }
            tokio::select! {
                changed = settlement.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = terminal_failure.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    pub(super) fn transition(&self, transition: TurnTransition) -> Result<(), AgentError> {
        let TurnLifecycle::Admitted(lifecycle) = &self.lifecycle;
        apply_turn_transition(&mut lock_mutex(lifecycle), transition).map(|_| ())
    }

    pub(super) fn begin_model_streaming(&self) -> Result<(), AgentError> {
        self.transition(TurnTransition::BeginModelStreaming)
    }

    pub(super) fn begin_tools(&self) -> Result<(), AgentError> {
        self.transition(TurnTransition::BeginTools)
    }

    pub(super) fn continue_after_model(&self) -> Result<(), AgentError> {
        self.transition(TurnTransition::ContinueAfterModel)
    }

    pub(super) fn begin_preparing(&self) -> Result<(), AgentError> {
        self.transition(TurnTransition::BeginPreparing)
    }

    pub(super) fn begin_commit(&self) -> Result<(), AgentError> {
        self.transition(TurnTransition::BeginCommit)
    }

    pub(super) fn record_model_operation(&self, usage: Option<Usage>) -> Result<(), AgentError> {
        lock_mutex(&self.usage)
            .record_model_operation(usage)
            .map_err(usage_error)
    }

    pub(super) fn begin_local_tool_dispatch(
        &self,
        context: EventContext,
        identity: ToolAuditIdentity,
        lineage: ToolCallerLineageView,
        observation: ToolCallObservation,
    ) -> Result<(), AgentError> {
        lock_mutex(&self.usage)
            .record_local_tool_attempt()
            .map_err(usage_error)?;
        let _ = self.events.publish(
            context,
            AgentEventKind::ToolStarted {
                identity,
                lineage,
                observation: Some(observation),
            },
        );
        Ok(())
    }

    pub(super) fn usage(&self) -> TurnUsage {
        *lock_mutex(&self.usage)
    }

    pub(super) fn publish_terminal(
        &self,
        result: TurnResult,
    ) -> Result<TurnTerminalOutcome, AgentError> {
        let lifecycle_result = result.error().cloned().map_or(Ok(()), Err);
        let cancellation_requested = self.cancellation.is_cancelled();
        let prepared_lifecycle =
            self.prepare_terminal_lifecycle(&lifecycle_result, cancellation_requested)?;
        #[cfg(test)]
        let terminal_fault = self
            .terminal_fault
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |fault| {
                Some(if fault == 4 { 4 } else { 0 })
            })
            .unwrap_or_default();
        #[cfg(test)]
        if terminal_fault == 1 || terminal_fault == 4 {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "injected failure after terminal lifecycle preparation",
            ));
        }
        self.install_terminal_result(result, prepared_lifecycle, {
            #[cfg(test)]
            {
                terminal_fault == 2
            }
            #[cfg(not(test))]
            {
                false
            }
        })?;
        #[cfg(test)]
        assert!(
            terminal_fault != 3,
            "injected failure after atomic terminal installation"
        );
        let installed = self.result.borrow();
        let installed = installed.as_ref().ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "terminal observation followed an empty result installation",
            )
        })?;
        let usage = installed.usage();
        let (outcome, terminal) = match installed.error() {
            None => (
                TurnTerminalOutcome::Completed,
                AgentEventKind::TurnCompleted { usage },
            ),
            Some(error) => (
                TurnTerminalOutcome::Failed(error.kind()),
                AgentEventKind::TurnFailed {
                    kind: error.kind(),
                    usage,
                },
            ),
        };
        self.emit_terminal_telemetry(outcome, usage);
        let context = EventContext::turn(self.turn_id).with_feed_seq(installed.feed_seq);
        let _ = self.events.publish(context, terminal);
        Ok(outcome)
    }

    pub(super) fn install_terminal_result(
        &self,
        result: TurnResult,
        prepared_lifecycle: PreparedTerminalLifecycle,
        inject_install_panic: bool,
    ) -> Result<(), AgentError> {
        let installed = self.result.send_if_modified(move |slot| {
            if slot.is_some() {
                return false;
            }
            assert!(
                !inject_install_panic,
                "injected terminal installation panic"
            );
            *slot = Some(Arc::new(result));
            self.commit_terminal_lifecycle(prepared_lifecycle);
            self.settlement.send_replace(TurnSettlement::Terminalized);
            #[cfg(test)]
            self.published_results.fetch_add(1, Ordering::AcqRel);
            true
        });
        if installed {
            Ok(())
        } else {
            Err(AgentError::new(
                AgentErrorKind::Internal,
                "terminal result publication was attempted more than once",
            ))
        }
    }

    pub(super) fn has_published_result(&self) -> bool {
        self.result.borrow().is_some()
    }

    pub(super) fn publish_terminal_failure(&self, error: AgentError) {
        if !self.has_published_result() {
            self.terminal_failure.send_replace(Some(error));
        }
    }

    #[cfg(test)]
    pub(super) fn inject_terminal_fault(&self, fault: u64) {
        self.terminal_fault.store(fault, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn inject_binding_lookup_fault(&self) {
        self.binding_lookup_fault.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn published_result_count(&self) -> u64 {
        self.published_results.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn assert_terminal_authority_consistent(&self) {
        assert!(self.has_published_result());
        assert!(self.is_terminal());
        assert_eq!(*self.settlement.borrow(), TurnSettlement::Terminalized);
    }

    pub(super) fn emit_terminal_telemetry(&self, outcome: TurnTerminalOutcome, usage: TurnUsage) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let started = lock_mutex(&telemetry.started).take();
        let elapsed = started.map_or(Duration::ZERO, |started| started.elapsed());
        let compartments = *lock_mutex(&telemetry.compartments);
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            telemetry.emitter.emit(TelemetryObservation::new(
                telemetry.context,
                telemetry.clock.now(),
                TelemetryObservationKind::TurnFinished {
                    elapsed,
                    status: turn_terminal_status(outcome, telemetry.shutdown.is_cancelled()),
                    usage,
                    model_wall: compartments.model_wall,
                    tool_wall: compartments.tool_wall,
                    compaction_wall: compartments.compaction_wall,
                    model_attempts: compartments.model_attempts,
                    tool_calls: compartments.tool_calls,
                    compactions: compartments.compactions,
                },
            ));
        }));
    }

    pub(super) fn prepare_terminal_lifecycle<T>(
        &self,
        result: &Result<T, AgentError>,
        cancellation_requested: bool,
    ) -> Result<PreparedTerminalLifecycle, AgentError> {
        let TurnLifecycle::Admitted(lifecycle) = &self.lifecycle;
        let mut prepared = lock_mutex(lifecycle).clone();
        terminalize_turn_machine(&mut prepared, self.turn_id, result, cancellation_requested)?;
        Ok(PreparedTerminalLifecycle {
            lifecycle: prepared,
        })
    }

    pub(super) fn commit_terminal_lifecycle(&self, prepared: PreparedTerminalLifecycle) {
        let TurnLifecycle::Admitted(lifecycle) = &self.lifecycle;
        *lock_mutex(lifecycle) = prepared.lifecycle;
    }
}

fn terminalize_turn_machine<T>(
    lifecycle: &mut TurnMachine,
    turn_id: TurnId,
    result: &Result<T, AgentError>,
    cancellation_requested: bool,
) -> Result<TerminalOutcome, AgentError> {
    let plan = terminal_transition_plan(result, cancellation_requested);
    let preview = preview_terminal_turn_with_plan(lifecycle, turn_id, result, &plan)?;
    for transition in plan.transitions() {
        apply_turn_transition(lifecycle, *transition)?;
    }
    let terminal = lifecycle.terminal().ok_or_else(turn_lifecycle_error)?;
    if terminal != preview || lifecycle.cleanup_origin().is_some() {
        return Err(turn_lifecycle_error());
    }
    let taken = lifecycle
        .take_terminal()
        .map_err(project_turn_state_error)?;
    if taken.turn_id() != turn_id || taken != terminal {
        return Err(turn_lifecycle_error());
    }
    Ok(taken.outcome())
}

fn preview_terminal_turn_with_plan<T>(
    lifecycle: &TurnMachine,
    turn_id: TurnId,
    result: &Result<T, AgentError>,
    plan: &TerminalTransitionPlan,
) -> Result<crate::state::TerminalTurn, AgentError> {
    let terminal = lifecycle
        .preview_transitions(plan.transitions())
        .map_err(project_turn_state_error)?
        .ok_or_else(turn_lifecycle_error)?;
    if terminal.turn_id() != turn_id || !terminal_outcome_matches_result(terminal.outcome(), result)
    {
        return Err(turn_lifecycle_error());
    }
    Ok(terminal)
}

fn terminal_transition_plan<T>(
    result: &Result<T, AgentError>,
    cancellation_requested: bool,
) -> TerminalTransitionPlan {
    match result {
        Ok(_) => TerminalTransitionPlan::one(TurnTransition::CommitSucceeded),
        Err(error) if error.kind() == AgentErrorKind::Cancelled => TerminalTransitionPlan::two(
            TurnTransition::BeginCancellation,
            TurnTransition::ResolveCancellation(CancellationResolution::JoinedCleanly),
        ),
        Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown && cancellation_requested => {
            TerminalTransitionPlan::two(
                TurnTransition::BeginCancellation,
                TurnTransition::ResolveCancellation(CancellationResolution::OutcomeUnknown),
            )
        }
        Err(error) => {
            let resolution = if error.kind() == AgentErrorKind::OutcomeUnknown {
                FailureResolution::OutcomeUnknown
            } else {
                FailureResolution::Failed
            };
            TerminalTransitionPlan::two(
                TurnTransition::BeginFailure,
                TurnTransition::ResolveFailure(resolution),
            )
        }
    }
}

impl DurableAgentError {
    pub(super) fn from_error(error: &AgentError) -> Result<Self, AgentError> {
        Ok(Self {
            kind: error.kind(),
            message: error.message().to_owned(),
            model_error: error.model_error().cloned(),
            tool_kind: error
                .tool_kind()
                .map(durable_tool_error_kind)
                .transpose()?
                .map(str::to_owned),
            store_error: error
                .store_error()
                .map(DurableSessionStoreError::from_error),
        })
    }

    pub(crate) fn into_error(self) -> Result<AgentError, AgentError> {
        let tool_kind = self
            .tool_kind
            .as_deref()
            .map(tool_error_kind_from_durable)
            .transpose()?;
        let store_error = self.store_error.map(DurableSessionStoreError::into_error);
        Ok(AgentError::from_canonical_parts(
            self.kind,
            self.message,
            self.model_error,
            tool_kind,
            store_error,
        ))
    }
}

impl DurableSessionStoreError {
    pub(super) fn from_error(error: SessionStoreError) -> Self {
        match error {
            SessionStoreError::Invalid(reason) => Self::Invalid(reason),
            SessionStoreError::NotFound => Self::NotFound,
            SessionStoreError::IdentityConflict => Self::IdentityConflict,
            SessionStoreError::Conflict => Self::Conflict,
            SessionStoreError::LeaseHeld => Self::LeaseHeld,
            SessionStoreError::LeaseExpired => Self::LeaseExpired,
            SessionStoreError::LeaseDomainMismatch => Self::LeaseDomainMismatch,
            SessionStoreError::FenceSuperseded => Self::FenceSuperseded,
            SessionStoreError::Archived => Self::Archived,
            SessionStoreError::Capacity(capacity) => Self::Capacity {
                bound: capacity.bound,
                current: capacity.current,
                max: capacity.max,
                admission: capacity
                    .admission
                    .map(|class| DurableCapacityAdmission::new(class, capacity.limit)),
            },
            SessionStoreError::BackendUnavailable => Self::BackendUnavailable,
            SessionStoreError::BackendBusy => Self::BackendBusy,
            SessionStoreError::Corrupt(reason) => Self::Corrupt(reason),
            SessionStoreError::StorageFull => Self::StorageFull,
            SessionStoreError::UnsupportedSchema => Self::UnsupportedSchema,
            SessionStoreError::UnsupportedCodec => Self::UnsupportedCodec,
            SessionStoreError::Shutdown => Self::Shutdown,
            SessionStoreError::Pruned => Self::Pruned,
        }
    }

    pub(super) fn into_error(self) -> SessionStoreError {
        match self {
            Self::Invalid(reason) => SessionStoreError::Invalid(reason),
            Self::NotFound => SessionStoreError::NotFound,
            Self::IdentityConflict => SessionStoreError::IdentityConflict,
            Self::Conflict => SessionStoreError::Conflict,
            Self::LeaseHeld => SessionStoreError::LeaseHeld,
            Self::LeaseExpired => SessionStoreError::LeaseExpired,
            Self::LeaseDomainMismatch => SessionStoreError::LeaseDomainMismatch,
            Self::FenceSuperseded => SessionStoreError::FenceSuperseded,
            Self::Archived => SessionStoreError::Archived,
            Self::Capacity {
                bound,
                current,
                max,
                admission: None,
            } => SessionStoreError::capacity(bound, current, max),
            Self::Capacity {
                bound,
                current,
                max,
                admission: Some(admission),
            } => SessionStoreError::Capacity(SessionStoreCapacity {
                bound,
                current,
                max,
                limit: admission.limit,
                admission: Some(admission.class.into_admission()),
            }),
            Self::BackendUnavailable => SessionStoreError::BackendUnavailable,
            Self::BackendBusy => SessionStoreError::BackendBusy,
            Self::Corrupt(reason) => SessionStoreError::Corrupt(reason),
            Self::StorageFull => SessionStoreError::StorageFull,
            Self::UnsupportedSchema => SessionStoreError::UnsupportedSchema,
            Self::UnsupportedCodec => SessionStoreError::UnsupportedCodec,
            Self::Shutdown => SessionStoreError::Shutdown,
            Self::Pruned => SessionStoreError::Pruned,
        }
    }
}

fn durable_tool_error_kind(kind: ToolRuntimeErrorKind) -> Result<&'static str, AgentError> {
    Ok(match kind {
        ToolRuntimeErrorKind::Registration => "Registration",
        ToolRuntimeErrorKind::InvalidDefinition => "InvalidDefinition",
        ToolRuntimeErrorKind::UnknownTool => "UnknownTool",
        ToolRuntimeErrorKind::InvalidArguments => "InvalidArguments",
        ToolRuntimeErrorKind::Policy => "Policy",
        ToolRuntimeErrorKind::UnsupportedCapability => "UnsupportedCapability",
        ToolRuntimeErrorKind::EffectSafety => "EffectSafety",
        ToolRuntimeErrorKind::Cancelled => "Cancelled",
        ToolRuntimeErrorKind::ExecutionTimeout => "ExecutionTimeout",
        ToolRuntimeErrorKind::CleanupTimeout => "CleanupTimeout",
        ToolRuntimeErrorKind::WorkspacePolicy => "WorkspacePolicy",
        ToolRuntimeErrorKind::Execution => "Execution",
        ToolRuntimeErrorKind::ResourceLimit => "ResourceLimit",
        ToolRuntimeErrorKind::ScriptExhausted => "ScriptExhausted",
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "tool error category has no canonical terminal-result encoding",
            ));
        }
    })
}

pub(super) fn tool_error_kind_from_durable(kind: &str) -> Result<ToolRuntimeErrorKind, AgentError> {
    match kind {
        "Registration" => Ok(ToolRuntimeErrorKind::Registration),
        "InvalidDefinition" => Ok(ToolRuntimeErrorKind::InvalidDefinition),
        "UnknownTool" => Ok(ToolRuntimeErrorKind::UnknownTool),
        "InvalidArguments" => Ok(ToolRuntimeErrorKind::InvalidArguments),
        "Policy" => Ok(ToolRuntimeErrorKind::Policy),
        "UnsupportedCapability" => Ok(ToolRuntimeErrorKind::UnsupportedCapability),
        "EffectSafety" => Ok(ToolRuntimeErrorKind::EffectSafety),
        "Cancelled" => Ok(ToolRuntimeErrorKind::Cancelled),
        "ExecutionTimeout" => Ok(ToolRuntimeErrorKind::ExecutionTimeout),
        "CleanupTimeout" => Ok(ToolRuntimeErrorKind::CleanupTimeout),
        "WorkspacePolicy" => Ok(ToolRuntimeErrorKind::WorkspacePolicy),
        "Execution" => Ok(ToolRuntimeErrorKind::Execution),
        "ResourceLimit" => Ok(ToolRuntimeErrorKind::ResourceLimit),
        "ScriptExhausted" => Ok(ToolRuntimeErrorKind::ScriptExhausted),
        _ => Err(AgentError::new(
            AgentErrorKind::Session,
            "terminal result carries an unknown tool error category",
        )),
    }
}

pub(super) fn encode_pending_terminal_record(
    record: PendingTerminalResultRecord,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&("plakous-terminal-result-pending", record))
}

pub(super) fn decode_pending_terminal_record(bytes: &[u8]) -> Option<PendingTerminalResultRecord> {
    let (tag, record): (String, PendingTerminalResultRecord) =
        serde_json::from_slice(bytes).ok()?;
    if tag != "plakous-terminal-result-pending"
        || record.version != TERMINAL_RESULT_RECORD_VERSION
        || encode_pending_terminal_record(record.clone())
            .ok()?
            .as_slice()
            != bytes
    {
        return None;
    }
    Some(record)
}

pub(super) fn encode_terminal_result_record(
    record: &DurableTerminalResultRecord,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&("plakous-terminal-result", record))
}

/// Why stored bytes were not accepted as a terminal result record.
///
/// The two cases need different words. "Absent" means there is no record here
/// to read. "`RoundTrip`" means the record is present and readable but its
/// re-encoding does not reproduce the stored bytes, which is how a field added
/// by a later build makes an older session unreadable. Collapsing both into
/// `None` is what made the 2026-09-18 resume blocker read as a missing record
/// and cost a day of looking in the wrong place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum TerminalRecordRejection {
    /// These bytes are not a terminal result record at all.
    Absent,
    /// It decoded, but re-encoding produced different bytes.
    RoundTrip(String),
}

impl TerminalRecordRejection {
    /// The operator-facing clause naming what went wrong.
    pub(in crate::runtime) fn detail(&self) -> String {
        match self {
            Self::Absent => "no canonical terminal result record".to_owned(),
            Self::RoundTrip(detail) => {
                format!("a terminal result record this build re-encodes differently ({detail})")
            }
        }
    }
}

/// Reads a terminal result record, saying which way it failed.
pub(in crate::runtime) fn read_terminal_result_record(
    bytes: &[u8],
) -> Result<DurableTerminalResultRecord, TerminalRecordRejection> {
    let (tag, record): (String, DurableTerminalResultRecord) =
        serde_json::from_slice(bytes).map_err(|_| TerminalRecordRejection::Absent)?;
    if tag != "plakous-terminal-result" || record.version != TERMINAL_RESULT_RECORD_VERSION {
        return Err(TerminalRecordRejection::Absent);
    }
    let reencoded =
        encode_terminal_result_record(&record).map_err(|_| TerminalRecordRejection::Absent)?;
    if reencoded.as_slice() != bytes {
        return Err(TerminalRecordRejection::RoundTrip(
            describe_round_trip_difference(bytes, &reencoded),
        ));
    }
    Ok(record)
}

pub(crate) fn valid_terminal_enrichment(turn_id: TurnId, before: &[u8], after: &[u8]) -> bool {
    let (Ok(original), Ok(mut enriched)) = (
        read_terminal_result_record(before),
        read_terminal_result_record(after),
    ) else {
        return false;
    };
    if original.turn_id != turn_id
        || original.abandonment.is_some()
        || enriched.abandonment.take().is_none()
        || !matches!(
            original.outcome,
            DurableTerminalOutcome::RecoveryRequired(_)
        )
    {
        return false;
    }
    encode_terminal_result_record(&enriched).is_ok_and(|bytes| bytes == before)
}

/// The `Option` form, for callers that only need "was there one".
#[cfg(test)]
pub(super) fn decode_terminal_result_record(bytes: &[u8]) -> Option<DurableTerminalResultRecord> {
    read_terminal_result_record(bytes).ok()
}

/// Where a re-encoding first departs from the stored bytes, and in which field.
///
/// A byte offset alone is unreadable, and the field name alone does not say
/// which direction the change went, so this reports both plus the two lengths.
fn describe_round_trip_difference(stored: &[u8], reencoded: &[u8]) -> String {
    let offset = stored
        .iter()
        .zip(reencoded)
        .position(|(stored, reencoded)| stored != reencoded)
        .unwrap_or_else(|| stored.len().min(reencoded.len()));
    let field = json_field_at(reencoded, offset)
        .or_else(|| json_field_at(stored, offset))
        .map_or_else(
            || "no enclosing field in range".to_owned(),
            |field| format!("in field \"{field}\""),
        );
    // The enclosing field says where; the next key on each side says what,
    // which is the difference between "something in usage changed" and
    // "the stored bytes carry peak_input_tokens and this build does not".
    let next = match (
        json_next_field(stored, offset),
        json_next_field(reencoded, offset),
    ) {
        (Some(stored), Some(reencoded)) if stored != reencoded => {
            format!(", stored next key \"{stored}\" against re-encoded \"{reencoded}\"")
        }
        (None, Some(reencoded)) => {
            format!(", re-encoded next key \"{reencoded}\" with none stored")
        }
        (Some(stored), None) => format!(", stored next key \"{stored}\" with none re-encoded"),
        _ => String::new(),
    };
    format!(
        "first differs at byte {offset}, {field}{next}; stored {} bytes, re-encoded {} bytes",
        stored.len(),
        reencoded.len()
    )
}

/// The first JSON object key whose name contains or follows `offset`.
///
/// Anchors on the key's closing `":` rather than its opening quote, because
/// the offset is usually *inside* the name — the two sides first differ on a
/// character of the key itself — and an opening-quote scan would then find the
/// closing quote and read the value as a name.
fn json_next_field(bytes: &[u8], offset: usize) -> Option<String> {
    let close = bytes
        .windows(2)
        .enumerate()
        .skip(offset.saturating_sub(1))
        .find_map(|(index, pair)| (pair == b"\":").then_some(index))?;
    let open = bytes.get(..close)?.iter().rposition(|byte| *byte == b'"')?;
    let name = bytes.get(open.checked_add(1)?..close)?;
    if name.is_empty() || name.contains(&b'\\') {
        return None;
    }
    String::from_utf8(name.to_vec()).ok()
}

/// The JSON object key whose value contains `offset`, best effort.
///
/// Scans backwards for the nearest `"name":` that starts at or before the
/// offset. This is a diagnostic, not a parser: it is allowed to be wrong on
/// pathological input, and it is never used for a decision.
fn json_field_at(bytes: &[u8], offset: usize) -> Option<String> {
    let window = bytes.get(..offset.min(bytes.len()))?;
    let mut cursor = window.len();
    while cursor > 0 {
        cursor -= 1;
        if window[cursor] != b':' {
            continue;
        }
        let close = window
            .get(..cursor)?
            .iter()
            .rposition(|byte| *byte == b'"')?;
        let open = window
            .get(..close)?
            .iter()
            .rposition(|byte| *byte == b'"')?;
        let name = window.get(open.checked_add(1)?..close)?;
        if name.is_empty() || name.contains(&b'\\') {
            return None;
        }
        return String::from_utf8(name.to_vec()).ok();
    }
    None
}

pub(super) fn finalized_operation_evidence(
    evidence: &ReconstructiveComponent,
    checkpoint_id: CheckpointId,
    conversation_identity: Digest,
) -> Result<ReconstructiveComponent, AgentError> {
    let Some(pending) = decode_pending_terminal_record(evidence.bytes()) else {
        return Ok(evidence.clone());
    };
    CommandId::new(&pending.command_id).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "terminal result command binding is invalid",
        )
    })?;
    let record = DurableTerminalResultRecord {
        version: TERMINAL_RESULT_RECORD_VERSION,
        command_id: pending.command_id,
        turn_id: pending.turn_id,
        operation_id: pending.operation_id,
        checkpoint_id,
        conversation_identity,
        outcome: pending.outcome,
        usage: pending.usage,
        abandonment: None,
        remedy: pending.remedy,
    };
    runtime_component(
        RuntimeComponentKind::Operation,
        encode_terminal_result_record(&record).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "terminal result record could not be canonicalized",
            )
        })?,
    )
}

pub(super) fn remove_known_turn(
    known_turns: &Mutex<BTreeMap<TurnId, Weak<TurnShared>>>,
    turn_id: TurnId,
) {
    match known_turns.lock() {
        Ok(mut turns) => {
            turns.remove(&turn_id);
        }
        Err(poisoned) => {
            poisoned.into_inner().remove(&turn_id);
        }
    }
}

pub(super) fn insert_known_turn(
    known_turns: &Mutex<BTreeMap<TurnId, Weak<TurnShared>>>,
    turn_id: TurnId,
    shared: &Arc<TurnShared>,
) {
    match known_turns.lock() {
        Ok(mut turns) => {
            turns.insert(turn_id, Arc::downgrade(shared));
        }
        Err(poisoned) => {
            poisoned
                .into_inner()
                .insert(turn_id, Arc::downgrade(shared));
        }
    }
}

pub(super) async fn wait_for_completion(
    mut completion: watch::Receiver<Option<Result<(), AgentError>>>,
) -> Result<(), AgentError> {
    loop {
        if let Some(result) = completion.borrow().clone() {
            return result;
        }
        if completion.changed().await.is_err() {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "agent actor completion channel closed unexpectedly",
            ));
        }
    }
}

pub(super) fn cancelled_error() -> AgentError {
    AgentError::new(AgentErrorKind::Cancelled, "turn was cancelled")
}

pub(super) fn recovery_gate_error() -> AgentError {
    AgentError::new(
        AgentErrorKind::OutcomeUnknown,
        "runtime admission is gated by an unresolved checkpoint publication",
    )
}

pub(super) fn tool_cleanup_error() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "supervised tool work did not reach a proven drained terminal boundary",
    )
}

pub(super) fn actor_panic_error() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "agent actor dependency panicked during owned work",
    )
}

pub(super) const TERMINAL_RESULT_RECORD_VERSION: u32 = 2;

/// Canonical bounded terminal authority retained by the root operation after
/// finalization. The checkpoint binding is compositional and non-recursive:
/// the record names the checkpoint and exact canonical history identity while
/// the store retains that checkpoint's content-addressed object graph.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) struct DurableTerminalResultRecord {
    pub(in crate::runtime) remedy: Option<crate::runtime::operation_record::DecidedRemedy>,
    pub(in crate::runtime) abandonment: Option<crate::runtime::AbandonedTurnReport>,
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) command_id: String,
    pub(in crate::runtime) turn_id: TurnId,
    pub(in crate::runtime) operation_id: OperationId,
    pub(in crate::runtime) checkpoint_id: CheckpointId,
    pub(in crate::runtime) conversation_identity: Digest,
    pub(in crate::runtime) outcome: DurableTerminalOutcome,
    pub(in crate::runtime) usage: TurnUsage,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "the private canonical terminal codec retains the complete typed model response without a representation-changing indirection"
)]
pub(in crate::runtime) enum DurableTerminalOutcome {
    Completed(ModelResponse),
    Cancelled(DurableAgentError),
    Failed(DurableAgentError),
    RecoveryRequired(DurableAgentError),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct DurableAgentError {
    pub(in crate::runtime) kind: AgentErrorKind,
    message: String,
    model_error: Option<ModelError>,
    tool_kind: Option<String>,
    store_error: Option<DurableSessionStoreError>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum DurableSessionStoreError {
    Invalid(StoreInvalidReason),
    NotFound,
    IdentityConflict,
    Conflict,
    LeaseHeld,
    LeaseExpired,
    LeaseDomainMismatch,
    FenceSuperseded,
    Archived,
    Capacity {
        bound: StoreCapacityBound,
        current: u64,
        max: u64,
        // Absent for a bound without an admission reserve, so those records
        // keep their exact bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admission: Option<DurableCapacityAdmission>,
    },
    BackendUnavailable,
    BackendBusy,
    Corrupt(StoreIntegrityReason),
    StorageFull,
    UnsupportedSchema,
    UnsupportedCodec,
    Shutdown,
    Pruned,
}

/// The admission class a capacity refusal was held to, and the configured
/// limit its threshold derives from. Hosts branch on the class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) struct DurableCapacityAdmission {
    class: DurableAdmissionClass,
    limit: u64,
}

impl DurableCapacityAdmission {
    const fn new(admission: StoreAdmission, limit: u64) -> Self {
        let class = match admission {
            StoreAdmission::Ordinary => DurableAdmissionClass::Ordinary,
            StoreAdmission::Settlement => DurableAdmissionClass::Settlement,
            StoreAdmission::Recovery => DurableAdmissionClass::Recovery,
        };
        Self { class, limit }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
enum DurableAdmissionClass {
    Ordinary,
    Settlement,
    Recovery,
}

impl DurableAdmissionClass {
    const fn into_admission(self) -> StoreAdmission {
        match self {
            Self::Ordinary => StoreAdmission::Ordinary,
            Self::Settlement => StoreAdmission::Settlement,
            Self::Recovery => StoreAdmission::Recovery,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) struct PendingTerminalResultRecord {
    pub(in crate::runtime) remedy: Option<crate::runtime::operation_record::DecidedRemedy>,
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) command_id: String,
    pub(in crate::runtime) turn_id: TurnId,
    pub(in crate::runtime) operation_id: OperationId,
    pub(in crate::runtime) outcome: DurableTerminalOutcome,
    pub(in crate::runtime) usage: TurnUsage,
}

/// Whether reopening this checkpoint would close an unfinished turn itself.
///
/// A caller that holds a locator behind the durable head uses this to tell the
/// two reasons apart. A head a reopen closes by abandonment is one it is
/// entitled to adopt, because the record only lagged. A head carrying a
/// definite terminal result the product record never learned about is not, and
/// keeps its refusal: advancing to it would skip that result silently.
#[must_use]
pub fn checkpoint_recovers_an_abandoned_turn(checkpoint: &RuntimeCheckpoint) -> bool {
    crate::runtime::durable::turn_recovery::reopen_abandons_working_turn(checkpoint)
}

/// Whether exact reopen can locally finalize this command's unfinished root.
///
/// # Errors
/// Returns typed integrity errors from matching local terminal evidence.
pub fn checkpoint_finalizes_root_turn(
    checkpoint: &RuntimeCheckpoint,
    command_id: &CommandId,
) -> Result<bool, AgentError> {
    let manifest = checkpoint.manifest();
    let crate::RuntimeLifecycle::Working {
        root: crate::RuntimeWorkingRoot::Turn { turn_id, .. },
        recovery_phase: crate::WorkingRecoveryPhase::Finalization,
        ..
    } = manifest.lifecycle()
    else {
        return Ok(false);
    };
    let records = manifest.ledger().commands();
    let Some(command) = records.into_iter().find(|record| {
        record.command_id() == command_id.as_str()
            && record.turn_id() == Some(*turn_id)
            && crate::runtime::operation_record::is_accepted_root(record)
    }) else {
        return Ok(false);
    };
    if manifest.ledger().operations().into_iter().any(|record| {
        record.request().bytes() == command.command_component().bytes()
            && matches!(record.state(), crate::DurableOperationState::Finalized(_))
    }) {
        return Ok(false);
    }
    match crate::runtime::durable::turn_recovery::verdict_closure(checkpoint) {
        Ok(Some(closure)) => Ok(closure
            .terminal()
            .is_some_and(|fold| fold.command_id == *command_id)),
        Ok(None) => Ok(false),
        Err(error) if error.kind() == AgentErrorKind::OutcomeUnknown => Ok(false),
        Err(error) => Err(error),
    }
}

pub(in crate::runtime) fn completed_terminal_evidence(
    command_id: &CommandId,
    turn_id: TurnId,
    operation_id: OperationId,
    response: &ModelResponse,
    usage: TurnUsage,
) -> Result<ReconstructiveComponent, AgentError> {
    let bytes = encode_pending_terminal_record(PendingTerminalResultRecord {
        remedy: None,
        version: TERMINAL_RESULT_RECORD_VERSION,
        command_id: command_id.as_str().to_owned(),
        turn_id,
        operation_id,
        outcome: DurableTerminalOutcome::Completed(response.clone()),
        usage,
    })
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "terminal turn result could not be canonicalized",
        )
    })?;
    runtime_component(RuntimeComponentKind::Operation, bytes)
}

pub(in crate::runtime) fn completed_turn_result(
    turn_id: TurnId,
    response: ModelResponse,
    usage: TurnUsage,
    checkpoint: Arc<RuntimeCheckpoint>,
) -> TurnResult {
    TurnResult {
        context_overflow_remedy: None,
        turn_id,
        outcome: TurnResultOutcome::Completed(response),
        recovery_operation: None,
        conversation: checkpoint.manifest().canonical().projection(),
        checkpoint,
        usage,
        feed_seq: None,
    }
}

/// The root terminal result of `command_id` when this checkpoint is the
/// quiescent checkpoint the runtime published it at, and `None` otherwise.
///
/// A host whose record committed that command's delivery but holds a locator
/// behind this head uses it to tell a lost terminal publication apart from any
/// other advance: when it is `Some`, the head is exactly the locator that
/// publication would have recorded, nothing was committed after it, and the
/// result is the one the host never published.
///
/// # Errors
///
/// Returns a session error when the checkpoint names that terminal but its
/// command or result record is inconsistent.
pub fn checkpoint_root_terminal_result(
    checkpoint: &Arc<RuntimeCheckpoint>,
    command_id: &CommandId,
) -> Result<Option<TurnResult>, AgentError> {
    let manifest = checkpoint.manifest();
    let published_here = matches!(manifest.lifecycle(), crate::RuntimeLifecycle::Quiescent)
        && terminal_published_here(manifest.ledger(), checkpoint.id(), command_id)?;
    if !published_here {
        return Ok(None);
    }
    let terminal = terminal_record_at_checkpoint(checkpoint, command_id)?;
    if terminal.checkpoint_id != checkpoint.id()
        || manifest.canonical().history().identity() != terminal.conversation_identity
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "terminal checkpoint carries a different result record",
        ));
    }
    terminal_turn_result(&terminal, Arc::clone(checkpoint)).map(Some)
}

fn terminal_published_here(
    ledger: &crate::RuntimeOperationLedger,
    checkpoint_id: CheckpointId,
    command_id: &CommandId,
) -> Result<bool, AgentError> {
    let commands = ledger.commands();
    let Some(command) = commands
        .iter()
        .find(|command| command.command_id() == command_id.as_str())
    else {
        return Ok(false);
    };
    let Some(turn_id) = command.turn_id() else {
        return Ok(false);
    };
    let matching_terminal = ledger.operations().into_iter().any(|operation| {
        matches!(operation.kind(), crate::DurableOperationKind::Command { turn_id: Some(turn) } if *turn == turn_id)
            && operation.request().bytes() == command.command_component().bytes()
            && matches!(operation.state(), crate::DurableOperationState::Finalized(_))
    });
    if !matching_terminal {
        return Ok(false);
    }
    let terminal = super::continuation::terminal_record_for_command(ledger, command)?;
    if terminal.command_id != command_id.as_str() || terminal.turn_id != turn_id {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "terminal result record does not match its command",
        ));
    }
    Ok(terminal.checkpoint_id == checkpoint_id)
}

/// Rebuilds the turn result a terminal record names from the checkpoint it was
/// published at.
pub(in crate::runtime) fn terminal_turn_result(
    terminal: &DurableTerminalResultRecord,
    checkpoint: Arc<RuntimeCheckpoint>,
) -> Result<TurnResult, AgentError> {
    let outcome = match terminal.outcome.clone() {
        DurableTerminalOutcome::Completed(response) => {
            return Ok(completed_turn_result(
                terminal.turn_id,
                response,
                terminal.usage,
                checkpoint,
            ));
        }
        DurableTerminalOutcome::Cancelled(error) => {
            TurnResultOutcome::Cancelled(error.into_error()?)
        }
        DurableTerminalOutcome::Failed(error) => TurnResultOutcome::Failed(error.into_error()?),
        DurableTerminalOutcome::RecoveryRequired(error) => {
            TurnResultOutcome::RecoveryRequired(error.into_error()?)
        }
    };
    Ok(TurnResult {
        context_overflow_remedy: terminal.remedy.map(Into::into),
        turn_id: terminal.turn_id,
        recovery_operation: matches!(outcome, TurnResultOutcome::RecoveryRequired(_))
            .then_some(terminal.operation_id.into()),
        outcome,
        conversation: checkpoint.manifest().canonical().projection(),
        checkpoint,
        usage: terminal.usage,
        feed_seq: None,
    })
}

/// Root terminal results a duplicate command can still rehydrate after
/// history retention; older duplicates are refused as reclaimed.
const RETAINED_TERMINAL_RESULTS: usize = 16;

/// Historical checkpoints this head still names as authority: the checkpoints
/// the most recent finalized root commands published their terminal results
/// at. A duplicate of one of those commands rehydrates its result from there,
/// so history retention keeps them.
#[must_use]
pub(crate) fn checkpoint_retained_history(
    checkpoint: &RuntimeCheckpoint,
) -> BTreeSet<CheckpointId> {
    let terminals = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter_map(|operation| match operation.state() {
            crate::DurableOperationState::Finalized(evidence) => {
                read_terminal_result_record(evidence.bytes()).ok()
            }
            _ => None,
        })
        .map(|terminal| terminal.checkpoint_id)
        .collect::<Vec<_>>();
    terminals
        .iter()
        .rev()
        .take(RETAINED_TERMINAL_RESULTS)
        .copied()
        .collect()
}

/// Whether an exact reopen can close this Working head using saved local
/// terminal evidence or the certified abandonment fold. This does not authorize
/// advancing a stale product locator past an unseen definite terminal result.
#[must_use]
pub fn checkpoint_closes_on_reopen(checkpoint: &RuntimeCheckpoint) -> bool {
    crate::runtime::durable::turn_recovery::reopen_closes_working_turn(checkpoint)
}

#[cfg(test)]
mod exact_terminal_tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "unit test assertions"
    )]
    use super::*;
    use crate::runtime::continuation::runtime_component;
    use crate::{
        DurableCommandDisposition, DurableCommandRecord, DurableOperationKind,
        DurableOperationRecord, DurableOperationState, DurableOperationTransition,
        RuntimeOperationLedger,
    };

    #[test]
    fn matching_finalized_terminal_corruption_is_not_a_nonmatch() {
        let turn = uuid::Uuid::from_u128(881).try_into().unwrap();
        let checkpoint = uuid::Uuid::from_u128(882).try_into().unwrap();
        let binding = Digest::sha256(
            plakous_model::DigestDomain::ProviderOperation,
            4,
            &[b"synthetic-root"],
        )
        .unwrap();
        let bytes =
            serde_json::to_vec(&("root:corrupt", 1_u64, turn, binding, "prompt", false)).unwrap();
        let command = DurableCommandRecord::new(
            "root:corrupt",
            1,
            Some(turn),
            binding,
            runtime_component(RuntimeComponentKind::Command, bytes.clone()).unwrap(),
            DurableCommandDisposition::Accepted,
            None,
        )
        .unwrap();
        let operation = DurableOperationRecord::new(
            1,
            DurableOperationKind::Command {
                turn_id: Some(turn),
            },
            runtime_component(RuntimeComponentKind::Operation, bytes).unwrap(),
            vec![DurableOperationTransition::new(
                checkpoint,
                DurableOperationState::Finalized(
                    runtime_component(
                        RuntimeComponentKind::Operation,
                        b"corrupt-terminal".to_vec(),
                    )
                    .unwrap(),
                ),
            )],
        )
        .unwrap();
        let ledger = RuntimeOperationLedger::new(vec![command], vec![operation]).unwrap();
        let error = terminal_published_here(
            &ledger,
            checkpoint,
            &CommandId::new("root:corrupt").unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), AgentErrorKind::Session);
        assert!(error.message().contains("terminal result record"));
        assert!(
            !terminal_published_here(&ledger, checkpoint, &CommandId::new("unrelated").unwrap())
                .unwrap()
        );
    }
}
