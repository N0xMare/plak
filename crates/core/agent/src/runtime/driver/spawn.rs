//! Consume prepared resources once and publish the owning actor.

use crate::input::UserInput;
use crate::runtime::composition::{CompositionOwner, CompositionSlot};
use crate::runtime::driver::Driver;
use crate::runtime::terminal::{actor_panic_error, insert_known_turn};
use crate::runtime::{
    Agent, AgentInner, DriverRestoredStartup, PreparedAgent, PromptCommand, RecoveredTurnHandle,
    TurnLifecycle, TurnResultHandle, TurnSettlement, TurnShared, TurnTelemetry,
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
impl PreparedAgent {
    pub(in crate::runtime) fn spawn(self) -> Agent {
        let event_pipeline = EventPipeline::new(self.events.clone());
        let ingress_capacity = self
            .config
            .prompt_queue_capacity
            .saturating_add(self.config.control_queue_capacity);
        let (controls, control_receiver) = mpsc::channel(ingress_capacity);
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
        let checkpoint_owner = self.authority.start();
        let authority = checkpoint_owner.reader();
        let provider_dispatch_gate = Arc::new(AsyncMutex::new(()));
        let command_nonce = Arc::new(AtomicU64::new(1));
        let (completion_sender, completion) = watch::channel(None);
        let mut command_ledger = self.initial_command_ledger;
        let (restored, recovered_turn) = if let Some(restored) = self.restored {
            let command_id = restored.command_id;
            let cancellation = CancellationToken::new();
            let (result_sender, result_receiver) = watch::channel(None);
            let (terminal_failure, terminal_failure_receiver) = watch::channel(None);
            let shared = Arc::new(TurnShared {
                turn_id: restored.frame.root().turn_id,
                lifecycle: TurnLifecycle::Admitted(Mutex::new(restored.lifecycle)),
                settlement: watch::channel(TurnSettlement::Pending).0,
                cancellation: cancellation.clone(),
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
                admitted_at: tokio::time::Instant::now(),
                cancellation,
                shared: Arc::clone(&shared),
            };
            insert_known_turn(&known_turns, command.turn_id, &shared);
            if let Some(entry) = command_ledger.get_mut(&command_id) {
                entry.result = Some(result.clone());
            }
            let handle = RecoveredTurnHandle {
                turn_id: command.turn_id,
                decision: restored.frame.pending_decision(),
                result,
                control,
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
        let job_release_mode = Arc::new(Mutex::new(crate::runtime::JobReleaseMode::default()));
        let driver = Driver {
            job_sessions: self.job_sessions,
            job_host: self.job_host,
            job_owner: plakous_tools::JobOwner {
                session_id: self.session_id,
                branch_id: self.branch_id,
                agent_id: self.agent_id,
            },
            job_release_mode: Arc::clone(&job_release_mode),
            pending_job_promotions: Vec::new(),
            pending_job_consumptions: Vec::new(),
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
            control_receiver,
            steering: VecDeque::new(),
            tool_preempt: watch::channel(plakous_tools::ToolPreempt::None).0,
            dispatch_preempts: Arc::new(Mutex::new(Vec::new())),
            roots: VecDeque::new(),
            active_root: None,
            retention_due: None,
            last_compaction_diagnostic: None,
            active_model_root_scope: None,
            restored,
            compaction_in_flight: false,
            turn_budgets: crate::runtime::operation_record::TurnBudgets::default(),
            pressure_folds_committed: 0,
            artifact_producer_prefix: None,
            context_anchor,
            request_anchor,
            nested_committed_observation_tokens: 0,
            post_compaction_note: self.post_compaction_note,
            prompt_in_history: None,
            appended_prompt: None,
            effect_decision_tombstones: VecDeque::new(),
            command_ledger,
            next_command_sequence: self.next_command_sequence,
            next_operation_sequence: self.next_operation_sequence,
            next_checkpoint_sequence,
            process_session_cleanup_uncertainty: None,
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
        };
        let panic_events = events.clone();
        let driver_task = tokio::spawn(async move {
            let result =
                if let Ok(result) = Box::pin(AssertUnwindSafe(driver.run()).catch_unwind()).await {
                    result
                } else {
                    panic_events.force_close();
                    Err(actor_panic_error())
                };
            let _ = completion_sender.send(Some(result));
        });
        #[cfg(test)]
        let driver_abort = driver_task.abort_handle();
        drop(driver_task);

        Agent {
            inner: Arc::new(AgentInner {
                job_stop: Mutex::new(crate::runtime::JobStopState::Idle),
                job_runtime: tokio::runtime::Handle::current(),
                job_sessions,
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
            }),
        }
    }
}
