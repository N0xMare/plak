//! Consume prepared resources once and publish the owning actor.

use crate::input::UserInput;
use crate::runtime::composition::{CompositionOwner, CompositionSlot};
use crate::runtime::driver::Driver;
use crate::runtime::terminal::{actor_panic_error, insert_known_turn};
use crate::runtime::{
    Agent, AgentInner, DriverRestoredStartup, PreparedAgent, PromptCommand, RecoveredTurnHandle,
    Turn, TurnLifecycle, TurnResultHandle, TurnSettlement, TurnShared, TurnTelemetry,
    TurnTelemetryCompartments,
};
use crate::{EventPipeline, TurnControl};
use futures_util::FutureExt;
use plakous_model::CancellationToken;
use std::collections::{BTreeMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};

/// A fresh digest salt, or none when the system has no randomness to give.
fn fresh_access_salt() -> Option<[u8; 32]> {
    let mut salt = [0; 32];
    getrandom::fill(&mut salt).ok()?;
    Some(salt)
}

impl PreparedAgent {
    #[allow(
        clippy::too_many_lines,
        reason = "Driver spawning wires the bounded control channels, durable startup, and terminal supervision before exposing handles"
    )]
    pub(in crate::runtime) fn spawn(self) -> Agent {
        let event_pipeline = EventPipeline::new(self.events.clone());
        let ingress_capacity = self
            .config
            .prompt_queue_capacity
            .saturating_add(self.config.control_queue_capacity);
        let (controls, control_receiver) = mpsc::channel(ingress_capacity);
        let boundary = Arc::new(crate::control::boundary::BoundaryState::default());
        let shutdown = CancellationToken::new();
        let pending_output_bound_rederived = self.output_bound_rederived;
        let events = self.events;
        let accepting = Arc::new(AtomicBool::new(true));
        let event_limits = self.event_limits;
        let known_turns = Arc::new(Mutex::new(BTreeMap::new()));
        let next_checkpoint_sequence = self
            .authority
            .checkpoint()
            .manifest()
            .topology()
            .branch_revision;
        let checkpoint_owner = self.authority.start_on(&self.executor);
        let authority = checkpoint_owner.reader();
        let provider_dispatch_gate = Arc::new(AsyncMutex::new(()));
        #[cfg(test)]
        let exit_probe = Arc::new(super::exit_probe::ExitProbe::default());
        let command_nonce = Arc::new(AtomicU64::new(1));
        let (completion_sender, completion) = watch::channel(None);
        let mut command_ledger = self.initial_command_ledger;
        let (restored, recovered_turn) = if let Some(restored) = self.restored {
            let command_id = restored.command_id;
            let cancellation = CancellationToken::new();
            let (result_sender, result_receiver) = watch::channel(None);
            let (terminal_failure, terminal_failure_receiver) = watch::channel(None);
            let shared = Arc::new(TurnShared {
                storage_pause: crate::runtime::checkpoint::storage_pause::StoragePause::new(
                    shutdown.clone(),
                ),
                turn_id: restored.frame.root().turn_id,
                lifecycle: TurnLifecycle::Admitted(Mutex::new(restored.lifecycle)),
                settlement: watch::channel(TurnSettlement::Pending).0,
                cancellation: cancellation.clone(),
                abandonment: Mutex::default(),
                result: result_sender,
                terminal_failure,
                events: event_pipeline.clone(),
                usage: Mutex::new(restored.frame.usage()),
                telemetry: Some(TurnTelemetry {
                    emitter: Arc::clone(&self.telemetry),
                    context: self
                        .telemetry_context
                        .with_turn(restored.frame.root().turn_id),
                    clock: Arc::clone(&self.clock),
                    shutdown: shutdown.clone(),
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
            let result = TurnResultHandle {
                turn_id: restored.frame.root().turn_id,
                result: result_receiver,
                terminal_failure: terminal_failure_receiver,
            };
            let control = TurnControl {
                agent_id: self.agent_id,
                turn_id: restored.frame.root().turn_id,
                shared: Arc::clone(&shared),
                controls: controls.clone(),
                command_nonce: Arc::clone(&command_nonce),
            };
            let command = PromptCommand {
                // Restored continuations already contain the admitted input in history.
                instruction: false,
                max_model_rounds: restored.frame.root().max_model_rounds,
                command_id: command_id.clone(),
                turn_id: restored.frame.root().turn_id,
                input: UserInput::default(),
                follow_up: restored.frame.root().follow_up,
                operation_id: restored.frame.root().operation_id,
                binding: restored.frame.root().binding,
                admitted_at: self.executor.now(),
                cancellation,
                shared: Arc::clone(&shared),
            };
            insert_known_turn(&known_turns, command.turn_id, &shared);
            if let Some(entry) = command_ledger.get_mut(&command_id) {
                entry.result = Some(result.clone());
            }
            let turn = Turn {
                turn_id: command.turn_id,
                result: result.clone(),
                events: events.clone(),
                event_limits,
                initial_events: events.turn_events(command.turn_id, event_limits).ok().map(
                    |events| events.with_terminal_failure(shared.terminal_failure.subscribe()),
                ),
                control: control.clone(),
            };
            let handle = RecoveredTurnHandle {
                turn_id: command.turn_id,
                decision: restored.frame.pending_decision(),
                result,
                control,
                turn: Arc::new(Mutex::new(Some(turn))),
            };
            (
                Some(DriverRestoredStartup {
                    frame: restored.frame,
                    effect: restored.effect,
                    command,
                }),
                Some(handle),
            )
        } else {
            (None, None)
        };
        // A plain resume keeps the anchor the head checkpoint recorded; a
        // parked continuation frame replaces it with its own.
        let mut context_anchor = checkpoint_owner
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .occupancy_anchor();
        let descriptor = self.provider_configuration.effective().descriptor();
        let request_anchor = checkpoint_owner
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .filter(|anchor| anchor.matches_descriptor(descriptor))
            .cloned();
        if checkpoint_owner
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_some()
            && request_anchor.is_none()
        {
            context_anchor = None;
        }
        let job_sessions = self.job_sessions.clone();
        let has_job_host = self.job_host.is_some();
        let job_release_mode = Arc::new(Mutex::new(if self.daemon_processes {
            crate::runtime::JobReleaseMode::Transfer
        } else {
            crate::runtime::JobReleaseMode::default()
        }));
        // Only a nonempty root genesis is a seed; fork/rebind successors have
        // a parent checkpoint and must not gain seed-only occupancy scheduling.
        let seed_compaction_pending = checkpoint_owner
            .checkpoint()
            .manifest()
            .topology()
            .parent_checkpoint_id
            .is_none()
            && !checkpoint_owner
                .checkpoint()
                .manifest()
                .canonical()
                .history()
                .is_empty();
        let credential_wait = Arc::new(Mutex::new(None));
        let telemetry_enabled = self.telemetry.is_enabled();
        let access_salt = telemetry_enabled.then(fresh_access_salt).flatten();
        let request_footprint = telemetry_enabled
            .then(|| super::observation::request_footprint(&self.stable_request_prefix));
        #[cfg(test)]
        let measurement_state = (access_salt.is_some(), request_footprint.is_some());
        #[cfg(test)]
        let line_owner = self.tools.environment().line_owner();
        let driver = Driver {
            credential_wait: Arc::clone(&credential_wait),
            job_sessions: self.job_sessions,
            job_host: self.job_host,
            job_owner: self.job_owner,
            job_release_mode: Arc::clone(&job_release_mode),
            pending_job_promotions: Vec::new(),
            pending_job_consumptions: Vec::new(),
            noticed_job_terminals: Vec::new(),
            observed_job_terminals: Vec::new(),
            pending_output_bound_rederived,
            agent_id: self.agent_id,
            model: self.model,
            provider_configuration: self.provider_configuration,
            model_rebind: self.model_rebind,
            model_error_target: self.model_error_target,
            tools: self.tools,
            tool_presentation: self.tool_presentation,
            config: self.config,
            stable_request_prefix: self.stable_request_prefix.clone(),
            clock: Arc::clone(&self.clock),
            ids: Arc::clone(&self.ids),
            run_id: self.run_id,
            events: event_pipeline.clone(),
            event_hub: events.clone(),
            event_limits,
            authority: checkpoint_owner,
            known_turns: Arc::clone(&known_turns),
            provider_dispatch_gate: Arc::clone(&provider_dispatch_gate),
            control_sender: controls.clone(),
            boundary: crate::control::boundary::BoundaryOwner(Arc::clone(&boundary)),
            control_receiver,
            #[cfg(test)]
            exit_probe: Arc::clone(&exit_probe),
            steering: VecDeque::new(),
            tool_preempt: watch::channel(plakous_tools::ToolPreempt::None).0,
            dispatch_preempts: Arc::new(Mutex::new(Vec::new())),
            nested_notice_preempts: Arc::new(Mutex::new(Vec::new())),
            roots: VecDeque::new(),
            active_root: None,
            retention_due: None,
            last_compaction_diagnostic: None,
            last_compaction_successor: None,
            active_model_root_scope: None,
            restored,
            compaction_in_flight: false,
            turn_budgets: crate::runtime::operation_record::TurnBudgets::default(),
            pressure_folds_committed: 0,
            reserved_top_level_operations: 0,
            artifact_producer_prefix: None,
            context_anchor,
            request_anchor,
            nested_committed_observation_tokens: 0,
            seed_compaction_pending,
            post_compaction_note: self.post_compaction_note,
            request_slots: self.request_slots,
            request_slot: None,
            credential_wait_elapsed: std::time::Duration::ZERO,
            placement_wait_elapsed: std::time::Duration::ZERO,
            prompt_in_history: None,
            restored_turn: None,
            appended_prompt: None,
            segment_refused_turn: None,
            admitted_round: None,
            effect_decision_tombstones: VecDeque::new(),
            command_ledger,
            next_command_sequence: self.next_command_sequence,
            next_operation_sequence: self.next_operation_sequence,
            next_checkpoint_sequence,
            process_session_cleanup_uncertainty: None,
            pending_named_state: None,
            recovery_gated: false,
            observation_budget: crate::runtime::observation::budget::ObservationBudget::default(),
            accepting: Arc::clone(&accepting),
            command_nonce: Arc::clone(&command_nonce),
            shutdown: shutdown.clone(),
            telemetry: Arc::clone(&self.telemetry),
            telemetry_context: self.telemetry_context,
            local_composition: CompositionSlot::from(self.local_composition.map(|construction| {
                CompositionOwner::new(construction, self.interpreter_observation.clone())
            })),
            access_salt,
            request_footprint,
            last_provider_request_at: None,
        };
        let panic_events = events.clone();
        let storage_pause =
            crate::runtime::checkpoint::storage_pause::StoragePause::new(shutdown.clone());
        let driver_task = self.executor.spawn(async move {
            let result = if let Ok(result) =
                Box::pin(AssertUnwindSafe(storage_pause.scope(driver.run())).catch_unwind()).await
            {
                result
            } else {
                panic_events.force_close();
                Err(actor_panic_error())
            };
            let _ = completion_sender.send(Some(result));
        });
        #[cfg(test)]
        let driver_abort = driver_task;
        #[cfg(not(test))]
        drop(driver_task);

        Agent {
            inner: Arc::new(AgentInner {
                boundary,
                #[cfg(test)]
                provider_dispatch_gate,
                #[cfg(test)]
                exit_probe,
                credential_wait,
                job_stop: Mutex::new(crate::runtime::JobStopState::Idle),
                executor: self.executor.clone(),
                job_sessions,
                has_job_host,
                job_release_mode,
                agent_id: self.agent_id,
                session_id: self.session_id,
                branch_id: self.branch_id,
                harness_plan: self.harness_plan,
                stable_request_prefix: self.stable_request_prefix,
                controls,
                shutdown,
                completion,
                accepting,
                events,
                authority,
                known_turns,
                command_nonce,
                telemetry: self.telemetry,
                recovered_turn,
                abandoned_turn: self.abandoned_turn,
                #[cfg(test)]
                driver_abort,
                #[cfg(test)]
                measurement_state,
                #[cfg(test)]
                line_owner,
            }),
        }
    }
}
