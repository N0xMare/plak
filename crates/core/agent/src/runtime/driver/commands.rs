//! Actor command admission, acknowledgement and steering order.

use crate::runtime::compaction::CompactOrigin;
use crate::runtime::continuation::runtime_component;
use crate::runtime::control::{command_binding, command_canonical_bytes, validate_command_text};
use crate::runtime::driver::{Driver, EffectDecisionTombstone};
use crate::runtime::terminal::{
    cancelled_error, insert_known_turn, queued_turn_machine, remove_known_turn,
};
use crate::runtime::{
    CommandLedgerEntry, OperationId, PreparedPromptCommand, PromptCommand, QueuedSteering,
    TerminalProposal, Turn, TurnLifecycle, TurnResultHandle, TurnSettlement, TurnShared,
    TurnTelemetry, TurnTelemetryCompartments,
};
use crate::{
    ActorCommandEffect, ActorCommandReply, ActorControl, AgentError, AgentErrorKind,
    AgentEventKind, CancellationStatus, CommandAck, CommandId, ControlError, ControlErrorKind,
    DurableOperationState, EffectDecisionRef, EffectDecisionResolution, EventContext,
    RuntimeComponentKind, SessionStoreError, SteeringStatus, StoreAdmission, TurnCommand,
    TurnControl, TurnUsage,
};
use plakous_model::{
    Actor, CancellationToken, ContentBlock, ConversationItem, Digest, HistorySegment,
    HistorySegmentKind, Provenance, TextBlock, TurnId,
};
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::watch;
fn update_dispatch_preempt(
    sender: &watch::Sender<plakous_tools::ToolPreempt>,
    preempt: plakous_tools::ToolPreempt,
) {
    sender.send_if_modified(|current| {
        if *current == plakous_tools::ToolPreempt::Background || *current == preempt {
            return false;
        }
        *current = preempt;
        true
    });
}

pub(super) struct DispatchPreemptGuard {
    senders: Arc<Mutex<Vec<watch::Sender<plakous_tools::ToolPreempt>>>>,
    sender: watch::Sender<plakous_tools::ToolPreempt>,
}

impl Drop for DispatchPreemptGuard {
    fn drop(&mut self) {
        self.senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|sender| !sender.same_channel(&self.sender));
    }
}

impl Driver {
    pub(super) fn dispatch_preempt(
        &self,
    ) -> (
        watch::Receiver<plakous_tools::ToolPreempt>,
        DispatchPreemptGuard,
    ) {
        let (sender, receiver) = watch::channel(*self.tool_preempt.borrow());
        let mut dispatches = self
            .dispatch_preempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        dispatches.retain(|sender| sender.receiver_count() != 0);
        dispatches.push(sender.clone());
        (
            receiver,
            DispatchPreemptGuard {
                senders: Arc::clone(&self.dispatch_preempts),
                sender,
            },
        )
    }

    pub(super) fn set_tool_preempt(&self, preempt: plakous_tools::ToolPreempt) {
        self.tool_preempt.send_replace(preempt);
        for sender in self
            .dispatch_preempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            update_dispatch_preempt(sender, preempt);
        }
    }

    pub(super) async fn wait_recovery_gate(&mut self) -> Result<(), AgentError> {
        loop {
            tokio::select! {
                () = self.authority.lease_lost() => {
                    return Err(self.authority.heartbeat_failure());
                }
                () = self.shutdown.cancelled() => return Ok(()),
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        return Ok(());
                    };
                    self.handle_control(None, control).await;
                }
            }
        }
    }

    pub(super) async fn next_command(&mut self) -> Result<Option<PromptCommand>, AgentError> {
        if !self.roots.is_empty() {
            self.active_root = self.roots.pop_front();
            return Ok(self.active_root.clone());
        }
        loop {
            tokio::select! {
                biased;
                () = self.authority.lease_lost() => {
                    return Err(self.authority.heartbeat_failure());
                }
                () = self.shutdown.cancelled() => return Ok(None),
                control = self.control_receiver.recv() => {
                    if let Some(control) = control {
                        Box::pin(self.handle_control(None, control)).await;
                    } else {
                        return Ok(None);
                    }
                }
                () = std::future::ready(()), if self.retention_due.is_some() => {
                    if let Some(turn_id) = self.retention_due.take() {
                        self.retain_history_when_idle(turn_id).await;
                    }
                }
            }
            if self.recovery_gated {
                return Ok(None);
            }
            if !self.roots.is_empty() {
                self.active_root = self.roots.pop_front();
                return Ok(self.active_root.clone());
            }
        }
    }

    pub(super) fn root_operation_for_command(
        &self,
        command: &PromptCommand,
    ) -> Result<OperationId, AgentError> {
        #[cfg(test)]
        if command
            .shared
            .binding_lookup_fault
            .swap(false, Ordering::AcqRel)
        {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "injected missing durable root operation binding",
            ));
        }
        let admission = self
            .command_ledger
            .get(&command.command_id)
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "admitted root command is absent from the driver admission index",
                )
            })?;
        if admission.binding != command.binding
            || admission.acknowledgement.turn_id() != Some(command.turn_id)
            || !admission.acknowledgement.is_accepted()
            || !matches!(admission.effect, ActorCommandEffect::Root)
            || admission.result.is_none()
        {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "admitted root command disagrees with its exact driver admission binding",
            ));
        }
        Ok(command.operation_id)
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the actor-owned exact-turn control transition table remains auditable in one place"
    )]
    pub(super) async fn handle_control(
        &mut self,
        active: Option<&PromptCommand>,
        control: ActorControl,
    ) {
        match control {
            ActorControl::Background { turn_id, reply } => {
                let result = if active.is_some_and(|root| root.turn_id == turn_id) {
                    // Each in-flight dispatch owns its latch. New dispatches subscribe
                    // to the turn state, which never contains this background edge.
                    for sender in self
                        .dispatch_preempts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                    {
                        update_dispatch_preempt(sender, plakous_tools::ToolPreempt::Background);
                    }
                    Ok(())
                } else {
                    Err(ControlError::new(
                        ControlErrorKind::WrongPhase,
                        "background is valid only for the active exact turn",
                    ))
                };
                let _ = reply.send(result);
            }
            ActorControl::RecordJobTerminals { reply } => {
                let _ = reply.send(self.record_job_terminals().await);
            }
            ActorControl::Command { command, reply } => {
                let response = self.handle_command(active, command).await;
                let _ = reply.send(response);
            }
            ActorControl::ContextStatus { request, reply } => {
                let response = self.inspect_context_status(active, request).await;
                let _ = reply.send(response);
            }
            ActorControl::CompactSession { request, reply } => {
                let response =
                    Box::pin(self.compact_session(active, *request, CompactOrigin::Endpoint)).await;
                let _ = reply.send(response);
            }
        }
    }

    async fn handle_command(
        &mut self,
        active: Option<&PromptCommand>,
        command: TurnCommand,
    ) -> ActorCommandReply {
        let command_id = command.command_id().clone();
        let target_turn_id = command.target_turn_id();
        let canonical_input = command_canonical_bytes(&command);
        let binding = match command_binding(&command) {
            Ok(binding) => binding,
            Err(error) => return self.unrecorded_rejection(command_id, target_turn_id, error),
        };
        if let Some(recorded) = self.command_ledger.get(&command_id).cloned() {
            return self
                .duplicate_command_reply(command_id, target_turn_id, binding, recorded)
                .await;
        }
        if self.recovery_gated {
            return self.unrecorded_rejection(
                command_id,
                target_turn_id,
                ControlError::new(
                    ControlErrorKind::Shutdown,
                    "runtime admission is gated by an unresolved checkpoint publication",
                ),
            );
        }
        if self.command_ledger.len() >= 4_096
            && let Err(error) = self.retain_settled_commands().await
        {
            return self.command_persistence_failure(command_id, target_turn_id, &error);
        }
        if self.command_ledger.len() >= 4_096 {
            return self.unrecorded_rejection(
                command_id,
                target_turn_id,
                ControlError::new(
                    ControlErrorKind::LedgerFull,
                    "bounded command ledger is full",
                ),
            );
        }

        match command {
            TurnCommand::Start { text, .. } | TurnCommand::StartInstruction { text, .. } => {
                if !self.accepting.load(Ordering::Acquire) || self.shutdown.is_cancelled() {
                    return self
                        .record_rejection(
                            command_id,
                            binding,
                            canonical_input,
                            None,
                            ControlError::new(ControlErrorKind::Shutdown, "agent is shutting down"),
                        )
                        .await;
                }
                if let Err(error) = validate_command_text(&text, "prompt") {
                    return self
                        .record_rejection(command_id, binding, canonical_input, None, error)
                        .await;
                }
                let queued = self.roots.iter().filter(|root| !root.follow_up).count();
                if queued >= self.config.prompt_queue_capacity {
                    // A transient refusal must not bind this identity to a permanent rejection.
                    return self.unrecorded_rejection(
                        command_id,
                        None,
                        ControlError::new(
                            ControlErrorKind::QueueFull,
                            "agent prompt queue is full",
                        ),
                    );
                }
                self.admit_root(command_id, binding, canonical_input, text, false)
                    .await
            }
            TurnCommand::FollowUp {
                target_turn_id,
                text,
                ..
            } => {
                self.follow_up_command(
                    active,
                    command_id,
                    binding,
                    canonical_input,
                    target_turn_id,
                    text,
                )
                .await
            }
            TurnCommand::Steer { turn_id, text, .. } => {
                self.steer_command(active, command_id, binding, canonical_input, turn_id, text)
                    .await
            }
            TurnCommand::Cancel { turn_id, .. } => {
                self.cancel_command(active, command_id, binding, canonical_input, turn_id)
                    .await
            }
            TurnCommand::ResolveEffectDecision { reference, .. } => {
                self.record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(reference.turn_id()),
                    self.late_effect_decision_error(&reference),
                )
                .await
            }
            TurnCommand::Shutdown { .. } => {
                self.shutdown_command(active, command_id, binding, canonical_input, target_turn_id)
                    .await
            }
        }
    }

    /// Replies to a command whose identity is already in the ledger.
    async fn duplicate_command_reply(
        &mut self,
        command_id: CommandId,
        target_turn_id: Option<TurnId>,
        binding: Digest,
        recorded: CommandLedgerEntry,
    ) -> ActorCommandReply {
        if recorded.binding != binding {
            return ActorCommandReply {
                acknowledgement: CommandAck::rejected(
                    command_id,
                    recorded.acknowledgement.sequence(),
                    target_turn_id,
                    ControlError::new(
                        ControlErrorKind::IdentityConflict,
                        "command identity was reused with different canonical content",
                    ),
                ),
                turn: None,
                result: None,
                effect: ActorCommandEffect::Rejected,
            };
        }
        let mut admission_reclaimed = recorded.admission_reclaimed;
        if let Some(seq) = recorded.acknowledgement.feed_seq() {
            match self.authority.admission_reclaimed(seq).await {
                Ok(reclaimed) => admission_reclaimed |= reclaimed,
                Err(error) => {
                    return self.command_persistence_failure(command_id, target_turn_id, &error);
                }
            }
        }
        if admission_reclaimed {
            return self.unrecorded_rejection(
                command_id,
                target_turn_id,
                ControlError::new(
                    ControlErrorKind::Reclaimed,
                    "the command's admission record was reclaimed by retention",
                ),
            );
        }
        let result = if let Some(result) = recorded.result.clone() {
            Some(result)
        } else if let Some(terminal) = recorded.terminal.as_ref() {
            match self.rehydrate_terminal_result(terminal).await {
                Ok(result) => {
                    let result = TurnResultHandle::resolved(result);
                    if let Some(entry) = self.command_ledger.get_mut(&command_id) {
                        entry.result = Some(result.clone());
                    }
                    Some(result)
                }
                Err(error) => {
                    let rejection = if error.store_error() == Some(SessionStoreError::Pruned) {
                        ControlError::new(
                            ControlErrorKind::Reclaimed,
                            "the duplicate's recorded result was reclaimed by history \
                                 retention",
                        )
                    } else {
                        ControlError::new(ControlErrorKind::Internal, error.message())
                    };
                    return self.unrecorded_rejection(command_id, target_turn_id, rejection);
                }
            }
        } else {
            None
        };
        ActorCommandReply {
            acknowledgement: CommandAck::duplicate(command_id, &recorded.acknowledgement),
            turn: None,
            result,
            effect: recorded.effect,
        }
    }

    /// Admits a root turn queued behind the named active turn.
    async fn follow_up_command(
        &mut self,
        active: Option<&PromptCommand>,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        target_turn_id: TurnId,
        text: String,
    ) -> ActorCommandReply {
        if let Err(error) = validate_command_text(&text, "follow-up") {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(target_turn_id),
                    error,
                )
                .await;
        }
        if self.shutdown.is_cancelled() {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(target_turn_id),
                    ControlError::new(ControlErrorKind::Shutdown, "agent is shutting down"),
                )
                .await;
        }
        if active.is_none_or(|root| root.turn_id != target_turn_id)
            || active.is_some_and(|root| root.shared.cancellation().is_cancelled())
        {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(target_turn_id),
                    ControlError::new(
                        ControlErrorKind::WrongPhase,
                        "follow-up target is not the active logical turn",
                    ),
                )
                .await;
        }
        let queued = self.roots.iter().filter(|root| root.follow_up).count();
        if queued >= self.config.follow_up_queue_capacity {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(target_turn_id),
                    ControlError::new(ControlErrorKind::QueueFull, "agent follow-up queue is full"),
                )
                .await;
        }
        self.admit_root(command_id, binding, canonical_input, text, true)
            .await
    }

    /// Queues steering text for the named active turn.
    async fn steer_command(
        &mut self,
        active: Option<&PromptCommand>,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        turn_id: TurnId,
        text: String,
    ) -> ActorCommandReply {
        if let Err(error) = validate_command_text(&text, "steering") {
            return self
                .record_rejection(command_id, binding, canonical_input, Some(turn_id), error)
                .await;
        }
        if active.is_none_or(|root| root.turn_id != turn_id) {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(turn_id),
                    ControlError::new(
                        ControlErrorKind::WrongPhase,
                        "steering is valid only for the active exact turn",
                    ),
                )
                .await;
        }
        if self.steering.len() >= self.config.steering_queue_capacity {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(turn_id),
                    ControlError::new(
                        ControlErrorKind::QueueFull,
                        "active turn steering queue is full",
                    ),
                )
                .await;
        }
        let status = SteeringStatus::Queued {
            position: self.steering.len() + 1,
        };
        let (acknowledgement, operation_id) = match self
            .record_acceptance(
                command_id.clone(),
                binding,
                canonical_input,
                Some(turn_id),
                ActorCommandEffect::Steer(status),
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                return self.command_persistence_failure(command_id, Some(turn_id), &error);
            }
        };
        if let Err(error) = self
            .transition_operation_as(
                StoreAdmission::Settlement,
                turn_id,
                operation_id,
                DurableOperationState::Dispatched,
            )
            .await
        {
            return self.command_persistence_failure(command_id, Some(turn_id), &error);
        }
        self.steering
            .push_back(QueuedSteering { text, operation_id });
        self.set_tool_preempt(plakous_tools::ToolPreempt::Steer);
        if let Some(root) = active {
            let _ = root.shared.events.publish(
                EventContext::turn(turn_id),
                AgentEventKind::SteeringQueued {
                    position: self.steering.len(),
                },
            );
        }
        ActorCommandReply {
            acknowledgement,
            turn: None,
            result: None,
            effect: ActorCommandEffect::Steer(status),
        }
    }

    /// Cancels the active or a queued turn.
    async fn cancel_command(
        &mut self,
        active: Option<&PromptCommand>,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        turn_id: TurnId,
    ) -> ActorCommandReply {
        let shared = self.known_turn(turn_id);
        let Some(shared) = shared else {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    Some(turn_id),
                    ControlError::new(ControlErrorKind::Stale, "turn is no longer admitted"),
                )
                .await;
        };
        let queued_position = active
            .is_none_or(|root| root.turn_id != turn_id)
            .then(|| {
                self.roots
                    .iter()
                    .position(|queued| queued.turn_id == turn_id)
            })
            .flatten();
        let status = if shared.is_terminal() {
            CancellationStatus::AlreadyTerminal
        } else if shared.cancellation().is_cancelled() {
            CancellationStatus::AlreadyRequested
        } else {
            CancellationStatus::Requested
        };
        if matches!(status, CancellationStatus::Requested)
            && let Some(position) = queued_position
        {
            let Some(queued) = self.roots.get(position) else {
                return self.command_persistence_failure(
                    command_id,
                    Some(turn_id),
                    &AgentError::new(
                        AgentErrorKind::Internal,
                        "queued cancellation lost its exact root command",
                    ),
                );
            };
            if queued.turn_id != turn_id {
                return self.command_persistence_failure(
                    command_id,
                    Some(turn_id),
                    &AgentError::new(
                        AgentErrorKind::Internal,
                        "queued cancellation selected a foreign root command",
                    ),
                );
            }
            let queued = queued.clone();
            let root_operation = queued.operation_id;
            let root_command_id = queued.command_id.clone();
            let usage = queued.shared.usage();
            let sequence = self.take_command_sequence();
            let publication = self
                .publish_atomic_queued_cancellation(
                    &command_id,
                    sequence,
                    turn_id,
                    binding,
                    canonical_input,
                    &root_command_id,
                    root_operation,
                    usage,
                )
                .await;
            if let Err(error) = publication {
                return self.command_persistence_failure(command_id, Some(turn_id), &error);
            }
            // Queued work has no live cell: only flip after the atomic
            // cancel publication succeeds so a rejected persist cannot
            // leave an in-memory cancel with an unchanged checkpoint.
            shared.cancellation().cancel();
            let acknowledgement = CommandAck::accepted(command_id.clone(), sequence, Some(turn_id))
                .with_feed_seq(self.authority.head_feed_seq());
            self.command_ledger.insert(
                command_id,
                CommandLedgerEntry {
                    admission_reclaimed: false,
                    binding,
                    acknowledgement: acknowledgement.clone(),
                    effect: ActorCommandEffect::Cancel(status),
                    result: None,
                    terminal: None,
                },
            );
            let completion = self
                .commit_and_publish_terminal(
                    &queued,
                    TerminalProposal::Cancelled(Box::new(cancelled_error())),
                )
                .await;
            if completion.is_ok() {
                let removed = self.roots.remove(position);
                if removed
                    .as_ref()
                    .is_none_or(|removed| removed.turn_id != turn_id)
                {
                    self.gate_recovery();
                } else {
                    remove_known_turn(&self.known_turns, turn_id);
                }
            } else {
                self.gate_recovery();
            }
            return ActorCommandReply {
                acknowledgement,
                turn: None,
                result: None,
                effect: ActorCommandEffect::Cancel(status),
            };
        }
        if matches!(status, CancellationStatus::Requested) {
            shared.cancellation().cancel();
        }
        let (acknowledgement, command_operation) = match self
            .record_acceptance(
                command_id.clone(),
                binding,
                canonical_input,
                Some(turn_id),
                ActorCommandEffect::Cancel(status),
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                return self.command_persistence_failure(command_id, Some(turn_id), &error);
            }
        };
        if let Err(error) = self
            .complete_command_operation(
                turn_id,
                command_operation,
                ActorCommandEffect::Cancel(status),
            )
            .await
        {
            return self.command_persistence_failure(command_id, Some(turn_id), &error);
        }
        ActorCommandReply {
            acknowledgement,
            turn: None,
            result: None,
            effect: ActorCommandEffect::Cancel(status),
        }
    }

    /// Stops admission and cancels the runtime.
    async fn shutdown_command(
        &mut self,
        active: Option<&PromptCommand>,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        target_turn_id: Option<TurnId>,
    ) -> ActorCommandReply {
        if !self.accepting.load(Ordering::Acquire) {
            return self
                .record_rejection(
                    command_id,
                    binding,
                    canonical_input,
                    None,
                    ControlError::new(ControlErrorKind::Shutdown, "agent is shutting down"),
                )
                .await;
        }
        let (acknowledgement, command_operation) = match self
            .record_acceptance(
                command_id.clone(),
                binding,
                canonical_input,
                active.map(|root| root.turn_id),
                ActorCommandEffect::Shutdown,
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                return self.command_persistence_failure(command_id, target_turn_id, &error);
            }
        };
        let lifecycle_turn = match active {
            Some(root) => Ok(root.turn_id),
            None => TurnId::try_from(self.run_id.as_uuid())
                .map_err(|error| AgentError::from_identity(&error)),
        };
        let lifecycle_turn = match lifecycle_turn {
            Ok(turn_id) => turn_id,
            Err(error) => {
                return self.command_persistence_failure(command_id, None, &error);
            }
        };
        if let Err(error) = self
            .complete_command_operation(
                lifecycle_turn,
                command_operation,
                ActorCommandEffect::Shutdown,
            )
            .await
        {
            return self.command_persistence_failure(command_id, target_turn_id, &error);
        }
        self.accepting.store(false, Ordering::Release);
        *self
            .job_release_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            crate::runtime::JobReleaseMode::Terminal;
        self.shutdown.cancel();
        ActorCommandReply {
            acknowledgement,
            turn: None,
            result: None,
            effect: ActorCommandEffect::Shutdown,
        }
    }

    async fn admit_root(
        &mut self,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        text: String,
        follow_up: bool,
    ) -> ActorCommandReply {
        let (mut command, turn) = match self.prepare_root(command_id.clone(), text, follow_up) {
            Ok(prepared) => prepared,
            Err(error) => {
                return self
                    .record_rejection(command_id, binding, canonical_input, None, error)
                    .await;
            }
        };
        command.instruction = canonical_input
            == command_canonical_bytes(&TurnCommand::StartInstruction {
                command_id: command_id.clone(),
                text: command.text.clone(),
            });
        let turn_id = command.turn_id;
        // A root begins new work: refuse it while any aggregate bound has no
        // ordinary headroom, so that the reserve stays with settlement and
        // recovery. Nothing is recorded; the refusal is the typed outcome.
        if let Some(capacity) = self.authority.ordinary_store_pressure().await {
            return self.unrecorded_rejection(
                command_id,
                Some(turn_id),
                storage_pressure_rejection(&SessionStoreError::Capacity(capacity)),
            );
        }
        let sequence = self.take_command_sequence();
        let operation_id = match self
            .persist_command_classification(
                &command_id,
                sequence,
                Some(turn_id),
                binding,
                canonical_input,
                ActorCommandEffect::Root,
                None,
            )
            .await
        {
            Ok(Some(operation_id)) => operation_id,
            Ok(None) => {
                return self.unrecorded_rejection(
                    command_id,
                    Some(turn_id),
                    ControlError::new(
                        ControlErrorKind::Internal,
                        "accepted root command omitted its durable operation",
                    ),
                );
            }
            Err(error) => {
                if let Some(refusal) = ordinary_capacity_refusal(&error) {
                    return self.unrecorded_rejection(
                        command_id,
                        Some(turn_id),
                        storage_pressure_rejection(&refusal),
                    );
                }
                if error.kind() == AgentErrorKind::OutcomeUnknown {
                    self.accepting.store(false, Ordering::Release);
                    return self.unrecorded_rejection(
                        command_id,
                        Some(turn_id),
                        ControlError::outcome_unknown(error),
                    );
                }
                if error.kind() == AgentErrorKind::Session
                    && matches!(
                        error.store_error(),
                        Some(
                            SessionStoreError::Corrupt(_)
                                | SessionStoreError::StorageFull
                                | SessionStoreError::Invalid(_)
                        )
                    )
                {
                    return self.unrecorded_rejection(
                        command_id,
                        Some(turn_id),
                        ControlError::admission_rejected(error),
                    );
                }
                return self.unrecorded_rejection(
                    command_id,
                    Some(turn_id),
                    ControlError::new(
                        ControlErrorKind::Internal,
                        format!(
                            "root command could not be durably admitted: {}",
                            error.message()
                        ),
                    ),
                );
            }
        };
        let command = command.admit(operation_id, binding, self.config.max_model_rounds);
        let acknowledgement = CommandAck::accepted(command_id.clone(), sequence, Some(turn_id))
            .with_feed_seq(self.authority.head_feed_seq());
        let result = turn.result.clone();
        self.command_ledger.insert(
            command_id,
            CommandLedgerEntry {
                admission_reclaimed: false,
                binding,
                acknowledgement: acknowledgement.clone(),
                effect: ActorCommandEffect::Root,
                result: Some(result.clone()),
                terminal: None,
            },
        );
        insert_known_turn(&self.known_turns, turn_id, &command.shared);
        if follow_up {
            let position = self
                .roots
                .iter()
                .position(|queued| !queued.follow_up)
                .unwrap_or(self.roots.len());
            self.roots.insert(position, command);
        } else {
            self.roots.push_back(command);
        }
        ActorCommandReply {
            acknowledgement,
            turn: Some(turn),
            result: Some(result),
            effect: ActorCommandEffect::Root,
        }
    }

    fn prepare_root(
        &self,
        command_id: CommandId,
        text: String,
        follow_up: bool,
    ) -> Result<(PreparedPromptCommand, Turn), ControlError> {
        let turn_id = TurnId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|_| {
            ControlError::new(
                ControlErrorKind::Internal,
                "turn identity could not be allocated",
            )
        })?;
        let cancellation = CancellationToken::new();
        let (result_sender, result) = watch::channel(None);
        let (terminal_failure, terminal_failure_events) = watch::channel(None);
        let initial_events = self
            .event_hub
            .turn_events(turn_id, self.event_limits)
            .ok()
            .map(|events| events.with_terminal_failure(terminal_failure_events.clone()));
        let shared = Arc::new(TurnShared {
            turn_id,
            lifecycle: TurnLifecycle::Admitted(Mutex::new(queued_turn_machine(turn_id).map_err(
                |_| {
                    ControlError::new(
                        ControlErrorKind::Internal,
                        "turn lifecycle could not be initialized",
                    )
                },
            )?)),
            settlement: watch::channel(TurnSettlement::Pending).0,
            cancellation: cancellation.clone(),
            result: result_sender,
            terminal_failure,
            events: self.events.clone(),
            usage: Mutex::new(TurnUsage::default()),
            telemetry: Some(TurnTelemetry {
                emitter: Arc::clone(&self.telemetry),
                context: self.telemetry_context.with_turn(turn_id),
                clock: Arc::clone(&self.clock),
                shutdown: self.shutdown.clone(),
                started: Mutex::new(None),
                compartments: Mutex::new(TurnTelemetryCompartments::default()),
            }),
            #[cfg(test)]
            terminal_fault: AtomicU64::new(0),
            #[cfg(test)]
            published_results: AtomicU64::new(0),
            #[cfg(test)]
            binding_lookup_fault: AtomicBool::new(false),
        });
        let command = PreparedPromptCommand {
            instruction: false,
            command_id,
            turn_id,
            text,
            follow_up,
            admitted_at: tokio::time::Instant::now(),
            cancellation,
            shared: Arc::clone(&shared),
        };
        let turn = Turn {
            turn_id,
            result: TurnResultHandle {
                turn_id,
                result,
                terminal_failure: terminal_failure_events,
            },
            events: self.event_hub.clone(),
            event_limits: self.event_limits,
            initial_events,
            control: TurnControl {
                agent_id: self.agent_id,
                turn_id,
                shared,
                controls: self.control_sender.clone(),
                command_nonce: Arc::clone(&self.command_nonce),
            },
        };
        Ok((command, turn))
    }

    pub(super) async fn record_acceptance(
        &mut self,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        turn_id: Option<TurnId>,
        effect: ActorCommandEffect,
    ) -> Result<(CommandAck, OperationId), AgentError> {
        let sequence = self.take_command_sequence();
        let operation_id = self
            .persist_command_classification(
                &command_id,
                sequence,
                turn_id,
                binding,
                canonical_input,
                effect,
                None,
            )
            .await?
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "accepted command omitted its durable operation",
                )
            })?;
        let acknowledgement = CommandAck::accepted(command_id.clone(), sequence, turn_id)
            .with_feed_seq(self.authority.head_feed_seq());
        self.command_ledger.insert(
            command_id,
            CommandLedgerEntry {
                admission_reclaimed: false,
                binding,
                acknowledgement: acknowledgement.clone(),
                effect,
                result: None,
                terminal: None,
            },
        );
        Ok((acknowledgement, operation_id))
    }

    pub(super) async fn record_rejection(
        &mut self,
        command_id: CommandId,
        binding: Digest,
        canonical_input: Vec<u8>,
        turn_id: Option<TurnId>,
        error: ControlError,
    ) -> ActorCommandReply {
        let sequence = self.take_command_sequence();
        if let Err(persistence) = self
            .persist_command_classification(
                &command_id,
                sequence,
                turn_id,
                binding,
                canonical_input,
                ActorCommandEffect::Rejected,
                Some(&error),
            )
            .await
        {
            return self.command_persistence_failure(command_id, turn_id, &persistence);
        }
        let acknowledgement = CommandAck::rejected(command_id.clone(), sequence, turn_id, error)
            .with_feed_seq(self.authority.head_feed_seq());
        self.command_ledger.insert(
            command_id,
            CommandLedgerEntry {
                admission_reclaimed: false,
                binding,
                acknowledgement: acknowledgement.clone(),
                effect: ActorCommandEffect::Rejected,
                result: None,
                terminal: None,
            },
        );
        ActorCommandReply {
            acknowledgement,
            turn: None,
            result: None,
            effect: ActorCommandEffect::Rejected,
        }
    }

    pub(super) fn command_persistence_failure(
        &mut self,
        command_id: CommandId,
        turn_id: Option<TurnId>,
        error: &AgentError,
    ) -> ActorCommandReply {
        // An ordinary refusal is definite and changed nothing; only an
        // unpublished settlement needs the recovery gate.
        if let Some(refusal) = ordinary_capacity_refusal(error) {
            return self.unrecorded_rejection(
                command_id,
                turn_id,
                storage_pressure_rejection(&refusal),
            );
        }
        self.gate_recovery();
        self.unrecorded_rejection(
            command_id,
            turn_id,
            ControlError::new(
                ControlErrorKind::Internal,
                format!(
                    "command classification could not be durably published: {}",
                    error.message()
                ),
            ),
        )
    }

    pub(super) fn gate_recovery(&mut self) {
        self.accepting.store(false, Ordering::Release);
        self.recovery_gated = true;
    }

    pub(super) fn required_terminal_publication_failure(
        &mut self,
        error: &AgentError,
    ) -> AgentError {
        self.gate_recovery();
        AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            format!(
                "required terminal checkpoint publication requires exact recovery: {}",
                error.message()
            ),
        )
    }

    pub(super) fn unrecorded_rejection(
        &self,
        command_id: CommandId,
        turn_id: Option<TurnId>,
        error: ControlError,
    ) -> ActorCommandReply {
        ActorCommandReply {
            acknowledgement: CommandAck::rejected(
                command_id,
                self.next_command_sequence,
                turn_id,
                error,
            ),
            turn: None,
            result: None,
            effect: ActorCommandEffect::Rejected,
        }
    }

    fn take_command_sequence(&mut self) -> u64 {
        let sequence = self.next_command_sequence;
        self.next_command_sequence = self.next_command_sequence.saturating_add(1);
        sequence
    }

    fn known_turn(&self, turn_id: TurnId) -> Option<Arc<TurnShared>> {
        match self.known_turns.lock() {
            Ok(turns) => turns.get(&turn_id).and_then(Weak::upgrade),
            Err(poisoned) => poisoned.into_inner().get(&turn_id).and_then(Weak::upgrade),
        }
    }

    fn late_effect_decision_error(&self, decision: &EffectDecisionRef) -> ControlError {
        let resolution = self
            .effect_decision_tombstones
            .iter()
            .rev()
            .find(|tombstone| tombstone.decision == *decision)
            .map(|tombstone| tombstone.resolution);
        match resolution {
            Some(EffectDecisionResolution::Expired) => ControlError::new(
                ControlErrorKind::Expired,
                "tool decision expired before the decision was applied",
            ),
            Some(EffectDecisionResolution::Cancelled | EffectDecisionResolution::Shutdown) => {
                ControlError::new(
                    ControlErrorKind::Cancelled,
                    "tool decision was removed by turn cancellation",
                )
            }
            Some(
                EffectDecisionResolution::Allowed
                | EffectDecisionResolution::Denied
                | EffectDecisionResolution::Invalidated,
            )
            | None => ControlError::new(
                ControlErrorKind::Stale,
                "tool decision is no longer pending",
            ),
        }
    }

    pub(super) fn remember_effect_decision(
        &mut self,
        decision: EffectDecisionRef,
        resolution: EffectDecisionResolution,
    ) {
        let capacity = self.config.control_queue_capacity.max(1);
        if self.effect_decision_tombstones.len() == capacity {
            self.effect_decision_tombstones.pop_front();
        }
        self.effect_decision_tombstones
            .push_back(EffectDecisionTombstone {
                decision,
                resolution,
            });
    }

    fn drain_steering_items(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(Vec<ConversationItem>, Vec<OperationId>), AgentError> {
        let mut items = Vec::new();
        let mut operations = Vec::new();
        while let Some(queued) = self.steering.pop_front() {
            operations.push(queued.operation_id);
            items.push(
                ConversationItem::new(
                    self.entry_id()?,
                    Actor::User,
                    vec![ContentBlock::Text(
                        TextBlock::new(queued.text, None).map_err(|_| {
                            AgentError::new(
                                AgentErrorKind::InvalidInput,
                                "steering text is invalid",
                            )
                        })?,
                    )],
                    Provenance::UserInput,
                )
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "steering conversation item is invalid",
                    )
                })?,
            );
            let _ = command.shared.events.publish(
                EventContext::turn(command.turn_id),
                AgentEventKind::SteeringApplied,
            );
        }
        Ok((items, operations))
    }

    pub(super) async fn apply_steering(
        &mut self,
        command: &PromptCommand,
    ) -> Result<bool, AgentError> {
        let (items, operations) = self.drain_steering_items(command)?;
        let applied = !items.is_empty();
        if !applied {
            if let Err(error) = self
                .append_history_with_notices(
                    command.turn_id,
                    Some(HistorySegmentKind::Steering),
                    [],
                    true,
                )
                .await
                && ordinary_capacity_refusal(&error).is_none()
            {
                return Err(error);
            }
            return Ok(false);
        }
        // Each steer's application is its history promotion: one commit
        // (spec 01 §3.5 item 4).
        let mut plan = self.commit_plan()?;
        if let Some((notice, numbers)) = self.process_notices()? {
            plan.segments.push(notice);
            plan.notices = numbers;
        }
        plan.segments.push(
            HistorySegment::new(HistorySegmentKind::Steering, items)
                .map_err(|_| super::notices::notice_history_error())?,
        );
        let applied_evidence = runtime_component(
            RuntimeComponentKind::Operation,
            b"steering-applied".to_vec(),
        )?;
        for operation in &operations {
            plan.transitions.push((
                *operation,
                DurableOperationState::Completed(applied_evidence.clone()),
            ));
            plan.finalize.insert(*operation);
        }
        let consumed = plan.notices.clone();
        self.publish_commit_plan(command.turn_id, plan).await?;
        self.observe_notice_delivery(
            command.turn_id,
            crate::JobNoticeDelivery::Boundary,
            consumed.len(),
        );
        self.set_tool_preempt(if self.steering.is_empty() {
            plakous_tools::ToolPreempt::None
        } else {
            plakous_tools::ToolPreempt::Steer
        });
        Ok(applied)
    }
}

/// The store error of a commit refused at the ordinary admission threshold.
pub(super) fn ordinary_capacity_refusal(error: &AgentError) -> Option<SessionStoreError> {
    let store_error = error.store_error()?;
    let capacity = store_error.capacity_refusal()?;
    (capacity.admission == Some(StoreAdmission::Ordinary)).then_some(store_error)
}

fn storage_pressure_rejection(refusal: &SessionStoreError) -> ControlError {
    match refusal.capacity_refusal() {
        Some(capacity) => ControlError::storage_pressure(capacity),
        None => ControlError::new(
            ControlErrorKind::StoragePressure,
            format!("new work was refused under storage pressure: {refusal}"),
        ),
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    #[test]
    fn dispatch_guard_removes_sender_even_if_receiver_survives() {
        let (sender, _receiver) = watch::channel(plakous_tools::ToolPreempt::None);
        let senders = Arc::new(Mutex::new(vec![sender.clone()]));
        drop(DispatchPreemptGuard {
            senders: Arc::clone(&senders),
            sender,
        });
        assert!(senders.lock().is_ok_and(|senders| senders.is_empty()));
    }
    #[test]
    fn background_dispatch_latch_survives_turn_resets_and_steer() {
        use plakous_tools::ToolPreempt;
        let (sender, receiver) = watch::channel(ToolPreempt::Background);
        for value in [ToolPreempt::None, ToolPreempt::Steer, ToolPreempt::None] {
            update_dispatch_preempt(&sender, value);
            assert_eq!(*receiver.borrow(), ToolPreempt::Background);
        }
    }
}
