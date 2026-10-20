//! Driver terminal publication and queued-root settlement.

use crate::runtime::cleanup::{combine_cleanup_results, contain_cleanup_panic};
use crate::runtime::continuation::{terminal_record_at_checkpoint, terminal_record_for_command};
use crate::runtime::driver::Driver;
use crate::runtime::driver::operations::TurnConsequence;
use crate::runtime::terminal::{
    DurableTerminalResultRecord, actor_panic_error, cancelled_error, recovery_gate_error,
    remove_known_turn, terminal_turn_result,
};
use crate::runtime::{PromptCommand, TerminalProposal, TurnResult, TurnResultOutcome};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, DurableOperationState, EventContext, PruneReceipt,
    TelemetryObservationKind, TelemetryTerminalStatus,
};

/// Superseded checkpoints a branch accumulates before a terminal publication
/// prunes them. Each prune marks the whole store's reachable graph once, so a
/// batch amortizes that walk over many turns while keeping per-session history
/// bounded.
pub(in crate::runtime) const AUTOMATIC_PRUNE_MINIMUM_CHECKPOINTS: u64 = 64;
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
impl Driver {
    pub(super) async fn settle_roots(
        &mut self,
        run_result: &mut Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        if let Some(command) = self.active_root.clone() {
            let owner_error = run_result.clone().err().unwrap_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "runtime owner retained an active root after its loop ended",
                )
            });
            let settlement = Box::pin(contain_cleanup_panic(async {
                let proposal = self.owner_loss_proposal(&command, &owner_error);
                self.commit_and_publish_terminal(&command, proposal).await
            }))
            .await;
            match settlement {
                Ok(()) => {
                    self.active_root = None;
                    self.active_model_root_scope = None;
                    remove_known_turn(&self.known_turns, command.turn_id);
                }
                Err(error) => {
                    command.shared.publish_terminal_failure(error.clone());
                    *run_result = combine_cleanup_results([run_result.clone(), Err(error)]);
                }
            }
        }
        // PromptCommand no longer owns a Drop-time result escape hatch. The
        // driver must therefore terminalize every still-admitted queued root
        // before releasing session ownership, including recovery-gated exits.
        // In a recovery gate this publishes RecoveryRequired from the last
        // authoritative checkpoint without attempting another store write.
        if !self.roots.is_empty() {
            let drain = AssertUnwindSafe(self.cancel_queued()).catch_unwind().await;
            match drain {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    *run_result = combine_cleanup_results([run_result.clone(), Err(error.clone())]);
                    let recovery = self.recover_queued_after_drain_failure(&error).await;
                    *run_result = combine_cleanup_results([run_result.clone(), recovery]);
                }
                Err(_) => {
                    let error = actor_panic_error();
                    *run_result = combine_cleanup_results([run_result.clone(), Err(error.clone())]);
                    let recovery = self.recover_queued_after_drain_failure(&error).await;
                    *run_result = combine_cleanup_results([run_result.clone(), recovery]);
                }
            }
        }
        Ok(())
    }

    pub(super) fn begin_turn(&mut self, command: &PromptCommand) {
        let _ = command.shared.events.publish(
            EventContext::turn(command.turn_id),
            AgentEventKind::TurnStarted,
        );
        // Once, on the first turn of a reopened session whose output budget
        // moved. It rides a turn because that is the only subscription the
        // product has; agent-scoped, it would reach nothing.
        if let Some(rederived) = self.pending_output_bound_rederived.take() {
            let _ = command.shared.events.publish(
                EventContext::turn(command.turn_id),
                AgentEventKind::OutputBoundRederived {
                    previous: rederived.previous(),
                    current: rederived.current(),
                },
            );
        }
        let started = tokio::time::Instant::now();
        command.shared.mark_telemetry_started(started);
        self.emit_telemetry(
            self.telemetry_context.with_turn(command.turn_id),
            TelemetryObservationKind::TurnStarted {
                origin: if command.instruction {
                    plakous_model::TurnOrigin::HostWake
                } else {
                    plakous_model::TurnOrigin::UserPrompt
                },
                queue_time: started.saturating_duration_since(command.admitted_at),
            },
        );
        self.steering.clear();
        self.set_tool_preempt(plakous_tools::ToolPreempt::None);
        self.turn_budgets = crate::runtime::operation_record::TurnBudgets::default();
        self.pressure_folds_committed = 0;
        self.helper_spend.reset();
        self.last_compaction_diagnostic = None;
    }

    pub(super) async fn cancel_queued(&mut self) -> Result<(), AgentError> {
        while let Some(command) = self.roots.front().cloned() {
            let proposal = if self.recovery_gated {
                TerminalProposal::RecoveryRequired(Box::new(recovery_gate_error()))
            } else {
                TerminalProposal::Cancelled(Box::new(cancelled_error()))
            };
            self.commit_and_publish_terminal(&command, proposal).await?;
            let removed = self.roots.pop_front().ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "published queued root disappeared before driver release",
                )
            })?;
            if removed.turn_id != command.turn_id {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "published queued root release selected a foreign command",
                ));
            }
            remove_known_turn(&self.known_turns, command.turn_id);
        }
        Ok(())
    }

    async fn recover_queued_after_drain_failure(
        &mut self,
        cause: &AgentError,
    ) -> Result<(), AgentError> {
        let mut results = Vec::new();
        while let Some(command) = self.roots.pop_front() {
            let result =
                contain_cleanup_panic(self.publish_terminal_fallback(&command, cause)).await;
            if let Err(error) = &result {
                command.shared.publish_terminal_failure(error.clone());
            }
            results.push(result);
            remove_known_turn(&self.known_turns, command.turn_id);
        }
        combine_cleanup_results(results)
    }

    pub(super) async fn commit_and_publish_terminal(
        &mut self,
        command: &PromptCommand,
        mut proposal: TerminalProposal,
    ) -> Result<(), AgentError> {
        if !matches!(proposal, TerminalProposal::Completed(_))
            && let Err(error) = self.cleanup_process_sessions(command.turn_id).await
        {
            proposal = proposal.with_cleanup_failure(error);
        }
        let attempted = AssertUnwindSafe(async {
            let mut result = self.commit_terminal_proposal(command, proposal).await?;
            result.feed_seq = self.authority.feed_seq_at(result.checkpoint.id());
            command.shared.publish_terminal(result).map(|_| ())
        })
        .catch_unwind()
        .await;
        match attempted {
            Ok(Ok(())) => {
                self.retention_due = Some(command.turn_id);
                return Ok(());
            }
            Ok(Err(_)) | Err(_) if command.shared.has_published_result() => return Ok(()),
            Ok(Err(error)) => {
                return self.publish_terminal_fallback(command, &error).await;
            }
            Err(_) => {}
        }
        self.publish_terminal_fallback(command, &actor_panic_error())
            .await
    }

    /// Idle-only maintenance; failures leave history for the next terminal's retry.
    pub(super) async fn retain_history_when_idle(&self, turn_id: plakous_model::TurnId) {
        let started = tokio::time::Instant::now();
        let pruned = AssertUnwindSafe(
            self.authority
                .prune_superseded(AUTOMATIC_PRUNE_MINIMUM_CHECKPOINTS),
        )
        .catch_unwind()
        .await;
        let (status, receipt) = match pruned {
            Ok(Ok(None)) => return,
            Ok(Ok(Some(receipt))) if receipt.pruned_checkpoints() == 0 => return,
            Ok(Ok(Some(receipt))) => (TelemetryTerminalStatus::Completed, Some(receipt)),
            Ok(Err(_)) | Err(_) => (TelemetryTerminalStatus::Failed, None),
        };
        self.emit_telemetry(
            self.telemetry_context.with_turn(turn_id),
            TelemetryObservationKind::RetentionFinished {
                elapsed: started.elapsed(),
                status,
                pruned_checkpoints: receipt.map_or(0, PruneReceipt::pruned_checkpoints),
            },
        );
    }

    async fn publish_terminal_fallback(
        &mut self,
        command: &PromptCommand,
        cause: &AgentError,
    ) -> Result<(), AgentError> {
        let checkpoint = self.authority.checkpoint();
        let recovered = match terminal_record_at_checkpoint(&checkpoint, &command.command_id) {
            Ok(terminal) => self.rehydrate_terminal_result(&terminal).await.ok(),
            Err(_) => None,
        };
        let result = if let Some(result) = recovered {
            result
        } else {
            self.gate_recovery();
            let error = AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                format!(
                    "authoritative terminal publication requires exact recovery: {}",
                    cause.message()
                ),
            );
            TurnResult {
                context_overflow_remedy: None,
                turn_id: command.turn_id,
                outcome: TurnResultOutcome::RecoveryRequired(error),
                recovery_operation: Some(command.operation_id.into()),
                conversation: checkpoint.manifest().canonical().projection(),
                checkpoint,
                usage: command.shared.usage(),
                feed_seq: None,
            }
        };
        match std::panic::catch_unwind(AssertUnwindSafe(|| command.shared.publish_terminal(result)))
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) | Err(_) if command.shared.has_published_result() => Ok(()),
            Ok(Err(error)) => {
                command.shared.publish_terminal_failure(error.clone());
                Err(error)
            }
            Err(_) => {
                let error = actor_panic_error();
                command.shared.publish_terminal_failure(error.clone());
                Err(error)
            }
        }
    }

    fn owner_loss_proposal(&self, command: &PromptCommand, error: &AgentError) -> TerminalProposal {
        match self
            .root_operation_for_command(command)
            .and_then(|operation| {
                self.operation_state(operation)
                    .map(|state| matches!(state, DurableOperationState::Prepared))
            }) {
            Ok(true) => TerminalProposal::Failed(Box::new(error.clone())),
            Ok(false) | Err(_) => TerminalProposal::RecoveryRequired(Box::new(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                format!(
                    "admitted turn owner ended before terminal checkpoint publication: {}",
                    error.message()
                ),
            ))),
        }
    }

    async fn commit_terminal_proposal(
        &mut self,
        command: &PromptCommand,
        proposal: TerminalProposal,
    ) -> Result<TurnResult, AgentError> {
        if let TerminalProposal::Completed(result) = proposal {
            let terminal = terminal_record_at_checkpoint(result.checkpoint(), &command.command_id)?;
            return self.rehydrate_terminal_result(&terminal).await;
        }
        let committed = {
            let authority = self.authority.checkpoint();
            terminal_record_at_checkpoint(&authority, &command.command_id).ok()
        };
        if let Some(terminal) = committed {
            return self.rehydrate_terminal_result(&terminal).await;
        }
        let operation_id = match self.root_operation_for_command(command) {
            Ok(operation_id) => operation_id,
            Err(error) => {
                self.gate_recovery();
                return Err(error);
            }
        };
        if self.recovery_gated {
            return Err(recovery_gate_error());
        }
        let persistence = match proposal {
            TerminalProposal::Cancelled(error) | TerminalProposal::Failed(error) => {
                self.commit_turn_terminal(
                    &command.command_id,
                    command.turn_id,
                    operation_id,
                    TurnConsequence::Ended(error.as_ref()),
                    command.shared.usage(),
                )
                .await
            }
            TerminalProposal::RecoveryRequired(error) => {
                self.persist_recovery_required(
                    &command.command_id,
                    command.turn_id,
                    operation_id,
                    error.as_ref(),
                    command.shared.usage(),
                )
                .await
            }
            TerminalProposal::Completed(_) => unreachable!("completed proposal handled above"),
        };
        match persistence {
            Ok(checkpoint) => {
                let terminal = terminal_record_at_checkpoint(&checkpoint, &command.command_id)?;
                self.rehydrate_terminal_result(&terminal).await
            }
            Err(error) => Err(self.required_terminal_publication_failure(&error)),
        }
    }

    pub(super) async fn rehydrate_terminal_result(
        &self,
        terminal: &DurableTerminalResultRecord,
    ) -> Result<TurnResult, AgentError> {
        let current = self.authority.checkpoint();
        let current_topology = current.manifest().topology();
        let checkpoint = if current.id() == terminal.checkpoint_id {
            Arc::clone(&current)
        } else {
            self.authority
                .load_checkpoint(terminal.checkpoint_id)
                .await?
        };
        let manifest = checkpoint.manifest();
        let topology = manifest.topology();
        if topology.checkpoint_id != terminal.checkpoint_id
            || topology.session_id != current_topology.session_id
            || manifest.canonical().history().identity() != terminal.conversation_identity
        {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "historical terminal checkpoint facts disagree with the result record",
            ));
        }
        let command = manifest
            .ledger()
            .commands()
            .into_iter()
            .find(|command| command.command_id() == terminal.command_id)
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "historical terminal checkpoint omitted its root command",
                )
            })?;
        if terminal_record_for_command(manifest.ledger(), command)? != *terminal {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "historical terminal checkpoint carries a different result record",
            ));
        }
        terminal_turn_result(terminal, checkpoint)
    }
}
