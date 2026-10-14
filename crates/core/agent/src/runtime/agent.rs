//! Public agent and turn handles over the private actor channels.

use crate::runtime::control::control_agent_error;
use crate::runtime::terminal::wait_for_completion;
use crate::runtime::{
    AbandonedTurnReport, Agent, AgentInner, ContextOverflowRemedy, RecoveredTurnHandle, Turn,
    TurnContextCapacity, TurnRecoveryOperationId, TurnResult, TurnResultHandle, TurnResultOutcome,
    TurnSubmission, WeakAgent,
};
use crate::{
    AgentError, AgentErrorKind, AgentSessionEndpoint, CheckpointId, CommandAck, CommandId,
    ControlError, ControlErrorKind, EffectDecisionRef, ResolvedHarnessPlan, ResumeOutcome,
    RuntimeCheckpoint, RuntimeDurability, TelemetryStatus, TurnCommand, TurnControl, TurnEvents,
    TurnUsage, ViewAttachment, ViewError, ViewRequest,
};
use plakous_model::{
    AgentId, BranchId, ContextCapacityFailure, ConversationView, ModelError, ModelErrorKind,
    ModelId, ModelResponse, SessionId, StableRequestPrefix, TurnId,
};
use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use tokio::sync::watch;
impl Drop for AgentInner {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        let mut mode = self
            .job_release_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            *mode,
            super::JobReleaseMode::Pending | super::JobReleaseMode::Terminal
        ) {
            *mode = super::JobReleaseMode::Terminal;
            plakous_tools::internal::close_job_admission(
                &self.job_sessions,
                &self.agent_id.to_string(),
            );
            if !self
                .job_sessions
                .jobs()
                .iter()
                .any(|job| job.status == plakous_tools::JobStatus::Running)
            {
                self.shutdown.cancel();
                return;
            }
            let sessions = self.job_sessions.clone();
            let owner = self.agent_id.to_string();
            // A self-exited driver can no longer perform terminal cleanup.
            let _ = std::thread::Builder::new()
                .name("job-release".to_owned())
                .spawn(move || {
                    let _ = plakous_tools::internal::stop_jobs_for_owner(
                        &sessions,
                        &owner,
                        plakous_tools::JobStopReason::SessionExit,
                        std::time::Duration::from_millis(200),
                    );
                });
        }
        self.shutdown.cancel();
    }
}

/// Unsupported facade plumbing for a host's pre-shutdown handoff.
pub fn close_admission(agent: &Agent) {
    agent.inner.accepting.store(false, Ordering::Release);
    plakous_tools::internal::close_job_admission(
        &agent.inner.job_sessions,
        &agent.id().to_string(),
    );
}

/// Unsupported facade plumbing for an explicit same-branch owner transfer.
///
/// # Errors
/// Returns the joined driver cleanup failure.
pub async fn shutdown_for_transfer(agent: &Agent) -> Result<(), AgentError> {
    {
        let mut mode = agent
            .inner
            .job_release_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *mode == super::JobReleaseMode::Pending {
            *mode = super::JobReleaseMode::Transfer;
        }
        agent.inner.accepting.store(false, Ordering::Release);
        agent.inner.shutdown.cancel();
    }
    wait_for_completion(agent.inner.completion.clone()).await
}

type LeaseBatch = (
    Arc<dyn crate::SessionStore>,
    Vec<crate::BranchLease>,
    Vec<Agent>,
);

/// Batch plumbing for the facade; public agent authority remains single-runtime.
///
/// # Errors
/// Returns a backend failure while releasing the exact captured leases.
pub async fn abandon_and_release_many(
    agents: &[Agent],
    job_budget: std::time::Duration,
) -> Result<(), AgentError> {
    for agent in agents {
        agent.begin_abandon();
    }
    let settlement = async {
        let mut stops = tokio::task::JoinSet::new();
        for agent in agents {
            let receiver = match &*agent
                .inner
                .job_stop
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                super::JobStopState::Idle => None,
                super::JobStopState::Stopping(receiver) => Some(receiver.clone()),
            };
            if let Some(receiver) = receiver {
                stops.spawn(wait_for_completion(receiver));
            }
        }
        let mut errors = Vec::new();
        while let Some(result) = stops.join_next().await {
            if let Err(error) = result
                .map_err(|_| AgentError::new(AgentErrorKind::Tool, "job stop task failed"))
                .and_then(std::convert::identity)
            {
                errors.push(error);
            }
        }
        let records = futures_util::future::join_all(agents.iter().map(|agent| async {
            crate::runtime::cleanup::contain_cleanup_panic(
                agent
                    .inner
                    .authority
                    .record_abandoned_job_terminals(&agent.inner.job_sessions.jobs()),
            )
            .await
        }))
        .await;
        errors.extend(records.into_iter().filter_map(Result::err));
        errors
    };
    let mut errors = match tokio::time::timeout(job_budget, settlement).await {
        Ok(errors) => errors,
        Err(_) => vec![AgentError::new(
            AgentErrorKind::Tool,
            "job stop and terminal recording deadline exceeded",
        )],
    };
    let mut stores = Vec::<LeaseBatch>::new();
    let mut seen = Vec::<&Agent>::new();
    let mut guards = Vec::new();
    for agent in agents {
        if seen
            .iter()
            .any(|prior| Arc::ptr_eq(&prior.inner, &agent.inner))
        {
            continue;
        }
        seen.push(agent);
        if let Some((store, lease, guard)) = agent.inner.authority.abandoned_lease().await {
            if let Some((_, leases, owners)) = stores
                .iter_mut()
                .find(|(held, _, _)| Arc::ptr_eq(held, &store))
            {
                if !leases.contains(&lease) {
                    leases.push(lease);
                }
                owners.push((*agent).clone());
            } else {
                stores.push((store, vec![lease], vec![(*agent).clone()]));
            }
            guards.push(guard);
        }
    }
    for (store, leases, owners) in stores {
        match store.release_many(leases).await {
            Ok(()) => {
                for agent in owners {
                    agent.inner.authority.mark_lease_released();
                }
            }
            Err(error) => errors.push(AgentError::from_store(error)),
        }
    }
    crate::runtime::cleanup::combine_cleanup_results(errors.into_iter().map(Err))
}

/// Whether the exact owner's lease release committed, even if cleanup failed.
#[must_use]
pub fn lease_released(agent: &Agent) -> bool {
    agent.inner.authority.lease_released()
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("agent_id", &self.inner.agent_id)
            .field("session_id", &self.inner.session_id)
            .field("branch_id", &self.inner.branch_id)
            .field("checkpoint_id", &self.checkpoint_id())
            .field(
                "stable_request_prefix",
                &self.inner.stable_request_prefix.identity(),
            )
            .field("accepting", &self.inner.accepting.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Records newly finished jobs in a manifest-only quiescent successor.
    /// Returns false when nothing changed or the head is Working.
    ///
    /// # Errors
    /// Reports bounded actor ingress or checkpoint publication failures.
    pub async fn record_job_terminals(&self) -> Result<bool, AgentError> {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        self.inner
            .controls
            .try_send(crate::ActorControl::RecordJobTerminals { reply })
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => AgentError::new(
                    AgentErrorKind::Backpressure,
                    "agent control ingress is full",
                ),
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    AgentError::new(AgentErrorKind::Shutdown, "agent control ingress is closed")
                }
            })?;
        receiver.await.map_err(|_| {
            AgentError::new(
                AgentErrorKind::Shutdown,
                "job terminal publication reply was lost",
            )
        })?
    }

    /// Returns live and terminal-but-unconsumed session-owned processes.
    #[must_use]
    pub fn jobs(&self) -> Vec<plakous_tools::JobView> {
        let checkpoint = self.inner.authority.checkpoint();
        let rows = checkpoint.manifest().jobs().rows();
        let mut jobs = self.inner.job_sessions.jobs();
        jobs.retain(|job| {
            job.status == plakous_tools::JobStatus::Running
                || rows.iter().any(|row| {
                    row.number == job.number
                        && row.session_id.as_bytes() == job.session_id
                        && row.owner_generation.get() == job.owner_generation
                })
        });
        for row in rows {
            if !jobs.iter().any(|job| {
                job.number == row.number
                    && job.session_id == row.session_id.as_bytes()
                    && job.owner_generation == row.owner_generation.get()
            }) {
                jobs.push(row.view());
            }
        }
        jobs.sort_by_key(|job| job.number);
        jobs
    }

    #[cfg(test)]
    pub(super) fn abort_driver_for_test(&self) {
        self.inner.driver_abort.abort();
    }

    pub(crate) fn recovered_turn(&self) -> Option<RecoveredTurnHandle> {
        self.inner.recovered_turn.clone()
    }

    /// What this reopen abandoned to get the session back, if anything.
    ///
    /// Present only when the process that held this session died mid-turn and
    /// this reopen closed that turn. The face reports it once; nothing else in
    /// the session's life produces one.
    #[must_use]
    pub fn abandoned_turn(&self) -> Option<&AbandonedTurnReport> {
        self.inner.abandoned_turn.as_ref()
    }
    /// Returns the stable agent identity.
    #[must_use]
    pub fn id(&self) -> AgentId {
        self.inner.agent_id
    }

    /// Returns the model-session identity.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.inner.session_id
    }

    /// Returns the current conversation-branch identity.
    #[must_use]
    pub fn branch_id(&self) -> BranchId {
        self.inner.branch_id
    }

    /// Returns the immutable resolved harness plan bound at construction.
    #[must_use]
    pub fn harness_plan(&self) -> &ResolvedHarnessPlan {
        &self.inner.harness_plan
    }

    /// Returns the immutable request prefix shared by every model round.
    #[must_use]
    pub fn stable_request_prefix(&self) -> &StableRequestPrefix {
        &self.inner.stable_request_prefix
    }

    /// Returns bounded delivery health for the configured content-free telemetry sink.
    #[must_use]
    pub fn telemetry_status(&self) -> TelemetryStatus {
        self.inner.telemetry.status()
    }

    /// Returns a non-owning capability which does not keep the actor alive.
    #[must_use]
    pub fn downgrade(&self) -> WeakAgent {
        WeakAgent {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub(crate) fn session_endpoint(&self) -> AgentSessionEndpoint {
        AgentSessionEndpoint::new(self.inner.controls.clone())
    }

    /// Recovers an exact typed control capability for a currently known turn.
    ///
    /// This lets a newly attached presentation client act on decision and turn
    /// identities from [`crate::AgentViewSnapshot`] without store access.
    #[must_use]
    pub fn turn_control(&self, turn_id: TurnId) -> Option<TurnControl> {
        let shared = match self.inner.known_turns.lock() {
            Ok(turns) => turns.get(&turn_id).and_then(Weak::upgrade),
            Err(poisoned) => poisoned.into_inner().get(&turn_id).and_then(Weak::upgrade),
        }?;
        Some(TurnControl {
            agent_id: self.inner.agent_id,
            turn_id,
            shared,
            controls: self.inner.controls.clone(),
            command_nonce: Arc::clone(&self.inner.command_nonce),
        })
    }

    /// Admits a prompt under a runtime-generated transient command identity.
    ///
    /// Use [`Self::start`] when retries must receive a stable duplicate
    /// acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns a typed validation, admission, identity, or shutdown failure.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<Turn, AgentError> {
        let command_id = self
            .transient_command_id("start")
            .map_err(control_agent_error)?;
        self.start(command_id, text)
            .await?
            .into_turn()
            .ok_or_else(|| AgentError::new(AgentErrorKind::Internal, "transient start duplicated"))
    }

    /// Admits an exact caller-identified start command.
    ///
    /// A matching retry returns the original acknowledgement and no second
    /// turn. Reusing the identity with different canonical text fails closed.
    ///
    /// # Errors
    ///
    /// Returns a typed validation, admission, identity, or shutdown failure.
    pub async fn start(
        &self,
        command_id: CommandId,
        text: impl Into<String>,
    ) -> Result<TurnSubmission, AgentError> {
        let submission = self
            .command(TurnCommand::Start {
                command_id,
                text: text.into(),
            })
            .await
            .map_err(control_agent_error)?;
        submission.accepted().map_err(control_agent_error)?;
        Ok(submission)
    }

    /// Admits a caller-identified host instruction with `Instruction` provenance.
    ///
    /// # Errors
    /// Returns the same validation, admission, identity, and shutdown failures as `start`.
    pub async fn start_instruction(
        &self,
        command_id: CommandId,
        text: impl Into<String>,
    ) -> Result<TurnSubmission, AgentError> {
        let submission = self
            .command(TurnCommand::StartInstruction {
                command_id,
                text: text.into(),
            })
            .await
            .map_err(control_agent_error)?;
        submission.accepted().map_err(control_agent_error)?;
        Ok(submission)
    }

    /// Submits one caller-identified command through the private driver's
    /// closed classification and idempotency path.
    ///
    /// # Errors
    ///
    /// Returns bounded ingress backpressure or actor shutdown before classification.
    pub async fn command(&self, command: TurnCommand) -> Result<TurnSubmission, ControlError> {
        let reply = crate::submit_actor_command(&self.inner.controls, command).await?;
        Ok(TurnSubmission {
            acknowledgement: reply.acknowledgement,
            turn: reply.turn,
            result: reply.result,
        })
    }

    /// Admits a new logical turn behind one exact active turn.
    ///
    /// Follow-ups use a distinct bounded actor-owned queue. They never alter
    /// the active turn and run in accepted order immediately after it reaches
    /// a terminal boundary, before ordinary prompts already waiting in the
    /// normal admission queue.
    ///
    /// # Errors
    ///
    /// Returns typed invalid-input, backpressure, stale-target, wrong-phase,
    /// queue-full, or shutdown errors. Rejected admission creates no turn
    /// terminal and retains no known-turn entry.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "follow-up preparation keeps every pre-ack resource and rollback boundary visible in one admission path"
    )]
    pub async fn follow_up(
        &self,
        active_turn_id: TurnId,
        text: impl Into<String>,
    ) -> Result<Turn, ControlError> {
        let command_id = self.transient_command_id("follow-up")?;
        self.follow_up_with_id(command_id, active_turn_id, text)
            .await?
            .into_turn()
            .ok_or_else(|| {
                ControlError::new(
                    ControlErrorKind::Internal,
                    "transient follow-up command duplicated",
                )
            })
    }

    /// Admits a follow-up under an exact caller idempotency identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress or actor classification failure.
    pub async fn follow_up_with_id(
        &self,
        command_id: CommandId,
        active_turn_id: TurnId,
        text: impl Into<String>,
    ) -> Result<TurnSubmission, ControlError> {
        let submission = self
            .command(TurnCommand::FollowUp {
                command_id,
                target_turn_id: active_turn_id,
                text: text.into(),
            })
            .await?;
        submission.accepted()?;
        Ok(submission)
    }

    pub(super) fn transient_command_id(&self, operation: &str) -> Result<CommandId, ControlError> {
        CommandId::transient(self.inner.agent_id, &self.inner.command_nonce, operation)
    }

    /// Returns a consistent clone of the currently committed in-memory history.
    #[must_use]
    pub fn conversation(&self) -> ConversationView {
        self.inner.authority.conversation()
    }

    /// Returns the current authoritative immutable-checkpoint identity.
    #[must_use]
    pub fn checkpoint_id(&self) -> CheckpointId {
        self.inner
            .authority
            .checkpoint()
            .manifest()
            .topology()
            .checkpoint_id
    }

    pub(crate) fn checkpoint(&self) -> Arc<RuntimeCheckpoint> {
        self.inner.authority.checkpoint()
    }

    /// Atomically obtains a complete presentation snapshot and its gapless live tail.
    ///
    /// # Errors
    ///
    /// Returns bounded subscriber-admission or view-scope errors.
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "attachment is an async public capability so actor-command serialization can replace the current in-process hub without an API break"
    )]
    pub async fn attach(&self, request: ViewRequest) -> Result<ViewAttachment, ViewError> {
        self.inner.events.attach(request)
    }

    /// [`Self::attach`] together with the committed history at the
    /// attachment's cursor, topology and ledger of the checkpoint that history
    /// belongs to. No checkpoint is published between them, and an
    /// item's streamed events precede its commit, so every item in the
    /// history was presented at or before the cursor and every later item's
    /// commit lies after it.
    pub(crate) fn attach_with_committed_history(
        &self,
        request: ViewRequest,
    ) -> Result<
        (
            ViewAttachment,
            plakous_model::CanonicalHistory,
            crate::manifest::RuntimeTopology,
            crate::manifest::RuntimeOperationLedger,
        ),
        ViewError,
    > {
        let (history, topology, ledger, attachment) = self
            .inner
            .authority
            .committed_history_with(|| self.inner.events.attach(request));
        Ok((attachment?, history, topology, ledger))
    }

    /// Reconnects after a compatible cursor or returns a fresh atomic attachment.
    ///
    /// # Errors
    ///
    /// Returns a typed scope, future-cursor, or subscriber-admission error.
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "resume is an async public capability so durable replay can replace the current retained-memory path without an API break"
    )]
    pub async fn resume(
        &self,
        cursor: crate::EventCursor,
        request: ViewRequest,
    ) -> Result<ResumeOutcome, ViewError> {
        self.inner.events.resume(cursor, request)
    }

    /// Stops admission, cancels active work, terminalizes queued turns, and
    /// waits for the private actor to exit. Calling this from clones is idempotent.
    ///
    /// # Errors
    ///
    /// Returns the shared actor cleanup failure, if any.
    pub async fn shutdown(&self) -> Result<(), AgentError> {
        self.inner.accepting.store(false, Ordering::Release);
        let terminal = {
            let mut mode = self
                .inner
                .job_release_mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *mode == super::JobReleaseMode::Pending {
                *mode = super::JobReleaseMode::Terminal;
            }
            let terminal = *mode == super::JobReleaseMode::Terminal;
            if terminal {
                plakous_tools::internal::close_job_admission(
                    &self.inner.job_sessions,
                    &self.id().to_string(),
                );
            }
            self.inner.shutdown.cancel();
            terminal
        };
        let completion = wait_for_completion(self.inner.completion.clone()).await;
        let stop = if terminal {
            self.begin_job_stop();
            let receiver = match &*self
                .inner
                .job_stop
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                super::JobStopState::Stopping(receiver) => Some(receiver.clone()),
                super::JobStopState::Idle => None,
            };
            if let Some(receiver) = receiver {
                wait_for_completion(receiver).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        crate::runtime::cleanup::combine_cleanup_results([completion, stop])
    }

    /// Closes process admission and starts one terminal stop task for this owner.
    /// The host may start this concurrently with bounded turn cancellation.
    pub fn begin_job_stop(&self) {
        plakous_tools::internal::close_job_admission(
            &self.inner.job_sessions,
            &self.id().to_string(),
        );
        let mut stop = self
            .inner
            .job_stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*stop, super::JobStopState::Idle) {
            return;
        }
        let (sender, receiver) = watch::channel(None);
        *stop = super::JobStopState::Stopping(receiver);
        let sessions = self.inner.job_sessions.clone();
        let owner = self.id().to_string();
        let runtime = self.inner.job_runtime.clone();
        self.inner.job_runtime.spawn(async move {
            let result = runtime
                .spawn_blocking(move || {
                    plakous_tools::internal::stop_jobs_for_owner(
                        &sessions,
                        &owner,
                        plakous_tools::JobStopReason::SessionExit,
                        std::time::Duration::from_millis(200),
                    )
                })
                .await
                .map_err(|_| AgentError::new(AgentErrorKind::Tool, "job stop task failed"))
                .and_then(|result| {
                    result.map_err(|error| {
                        AgentError::from_tool(&plakous_tools::backend_runtime_error(error))
                    })
                });
            let _ = sender.send(Some(result));
        });
    }

    /// Starts process-exit abandonment while retaining the lease, so the host
    /// can save its recovery handoff before batched release. The host must
    /// follow with `abandon_and_release` and enforce its process-exit bound.
    pub fn begin_abandon(&self) {
        *self
            .inner
            .job_release_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = super::JobReleaseMode::Abandon;
        self.begin_job_stop();
        self.inner.accepting.store(false, Ordering::Release);
        plakous_tools::internal::close_job_admission(
            &self.inner.job_sessions,
            &self.inner.agent_id.to_string(),
        );
        self.inner.authority.abandon();
        self.inner.shutdown.cancel();
    }

    /// Stops admission, fires cancellation, and releases the lease without joining
    /// the driver or committing a shutdown receipt. Already dispatched work may
    /// overlap the next owner until the exiting process's one-second bound.
    ///
    /// # Errors
    /// Returns a store failure when the exact lease could not be released.
    pub async fn abandon_and_release(&self) -> Result<(), AgentError> {
        abandon_and_release_many(
            std::slice::from_ref(self),
            std::time::Duration::from_millis(400),
        )
        .await
    }

    /// Submits joined shutdown under an exact caller idempotency identity.
    ///
    /// # Errors
    ///
    /// Returns a typed command classification or joined cleanup failure.
    pub async fn shutdown_with_id(&self, command_id: CommandId) -> Result<CommandAck, AgentError> {
        let submission = self
            .command(TurnCommand::Shutdown { command_id })
            .await
            .map_err(control_agent_error)?;
        let acknowledgement = submission.acknowledgement;
        if acknowledgement.is_accepted() {
            wait_for_completion(self.inner.completion.clone()).await?;
        }
        Ok(acknowledgement)
    }
}

impl WeakAgent {
    /// Upgrades the capability while the owning actor handle remains alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<Agent> {
        self.inner.upgrade().map(|inner| Agent { inner })
    }
}

impl TurnSubmission {
    /// Returns the stable command acknowledgement.
    #[must_use]
    pub const fn acknowledgement(&self) -> &CommandAck {
        &self.acknowledgement
    }

    /// Returns the newly admitted turn, or `None` for an exact duplicate.
    #[must_use]
    pub const fn turn(&self) -> Option<&Turn> {
        self.turn.as_ref()
    }

    /// Consumes the submission and returns the newly admitted turn.
    #[must_use]
    pub fn into_turn(self) -> Option<Turn> {
        self.turn
    }

    /// Returns a cloneable observer for the authoritative terminal result.
    ///
    /// Every accepted root command, including an exact duplicate, carries the
    /// same logical result handle. Rejected and conflicting commands carry no
    /// handle.
    #[must_use]
    pub const fn result(&self) -> Option<&TurnResultHandle> {
        self.result.as_ref()
    }

    /// Consumes the submission and returns its authoritative result observer.
    #[must_use]
    pub fn into_result(self) -> Option<TurnResultHandle> {
        self.result
    }

    pub(super) fn accepted(&self) -> Result<(), ControlError> {
        self.acknowledgement
            .rejection()
            .cloned()
            .map_or(Ok(()), Err)
    }
}

impl fmt::Debug for TurnSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnSubmission")
            .field("acknowledgement", &self.acknowledgement)
            .field("has_turn", &self.turn.is_some())
            .field("has_result", &self.result.is_some())
            .finish()
    }
}

impl RecoveredTurnHandle {
    /// Returns the persisted turn identity; reopen never allocates another one.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the sole exact pending decision address.
    #[must_use]
    pub const fn pending_decision(&self) -> Option<&EffectDecisionRef> {
        self.decision.as_ref()
    }

    /// Returns exact-turn decision/cancellation control.
    #[must_use]
    pub fn control(&self) -> TurnControl {
        self.control.clone()
    }

    /// Returns an independent observer of the original terminal result slot.
    #[must_use]
    pub fn result(&self) -> TurnResultHandle {
        self.result.clone()
    }
}

impl fmt::Debug for RecoveredTurnHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveredTurnHandle")
            .field("turn_id", &self.turn_id)
            .field("decision", &self.decision)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Turn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Turn")
            .field("turn_id", &self.turn_id)
            .field("cancelled", &self.control.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Turn {
    /// Returns the exact turn identity.
    #[must_use]
    pub const fn id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns an independent cancellation capability.
    #[must_use]
    pub fn control(&self) -> TurnControl {
        self.control.clone()
    }

    /// Takes the receiver created atomically with admission.
    ///
    /// Later subscriptions may miss already-emitted presentation events.
    ///
    /// # Errors
    ///
    /// Returns a subscriber-limit error after the admission-created receiver was taken.
    pub fn take_events(&mut self) -> Result<TurnEvents, ViewError> {
        if let Some(events) = self.initial_events.take() {
            Ok(events)
        } else {
            self.subscribe_events()
        }
    }

    /// Creates a best-effort live subscription from the current point.
    ///
    /// # Errors
    ///
    /// Returns a subscriber-limit error when the bounded registry is full.
    pub fn subscribe(&self) -> Result<TurnEvents, ViewError> {
        self.subscribe_events()
    }

    pub(super) fn subscribe_events(&self) -> Result<TurnEvents, ViewError> {
        let events = self
            .events
            .subscribe_turn_tail(self.turn_id, self.event_limits)?
            .with_terminal_failure(self.control.shared.terminal_failure.subscribe());
        Ok(
            if self.control.shared.is_terminal() && !self.control.shared.has_terminal_failure() {
                events.closed()
            } else {
                events
            },
        )
    }

    /// Waits for the authoritative terminal result. Event consumption is independent.
    ///
    /// # Errors
    ///
    /// Returns the turn failure or an internal error if the actor disappeared.
    pub async fn result(self) -> Result<TurnResult, AgentError> {
        self.result.result().await.map(|result| (*result).clone())
    }
}

impl fmt::Debug for TurnResultHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnResultHandle")
            .field("turn_id", &self.turn_id)
            .field("ready", &self.result.borrow().is_some())
            .field("failed", &self.terminal_failure.borrow().is_some())
            .finish()
    }
}

impl TurnResultHandle {
    /// Returns the exact root turn observed by this handle.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Resolves to shared immutable terminal authority independently of events.
    ///
    /// # Errors
    ///
    /// Returns an error when terminal authority could not be durably published,
    /// or an internal error if the runtime disappears without reporting why.
    pub async fn result(mut self) -> Result<Arc<TurnResult>, AgentError> {
        loop {
            if let Some(result) = self.result.borrow().clone() {
                return Ok(result);
            }
            if let Some(error) = self.terminal_failure.borrow().clone() {
                return Err(error);
            }
            // The actor publishes one channel and then drops both senders, so
            // a closed channel is final only after both values are re-read.
            let closed = tokio::select! {
                changed = self.result.changed() => changed.is_err(),
                changed = self.terminal_failure.changed() => changed.is_err(),
            };
            if closed {
                if let Some(result) = self.result.borrow().clone() {
                    return Ok(result);
                }
                if let Some(error) = self.terminal_failure.borrow().clone() {
                    return Err(error);
                }
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "agent actor ended without an authoritative terminal turn result",
                ));
            }
        }
    }

    pub(super) fn resolved(result: TurnResult) -> Self {
        let turn_id = result.turn_id;
        let (sender, receiver) = watch::channel(Some(Arc::new(result)));
        let (terminal_failure, terminal_failure_receiver) = watch::channel(None);
        drop(sender);
        drop(terminal_failure);
        Self {
            turn_id,
            result: receiver,
            terminal_failure: terminal_failure_receiver,
        }
    }
}

impl TurnContextCapacity {
    /// Returns the immutable quiescent checkpoint containing all committed rounds.
    #[must_use]
    pub const fn source_checkpoint(self) -> CheckpointId {
        self.source_checkpoint
    }

    /// Returns the checked compactable-versus-protected classification.
    #[must_use]
    pub const fn failure(self) -> ContextCapacityFailure {
        self.failure
    }
}

impl fmt::Debug for TurnResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome = match self.outcome {
            TurnResultOutcome::Completed(_) => "Completed",
            TurnResultOutcome::Cancelled(_) => "Cancelled",
            TurnResultOutcome::Failed(_) => "Failed",
            TurnResultOutcome::RecoveryRequired(_) => "RecoveryRequired",
        };
        formatter
            .debug_struct("TurnResult")
            .field("turn_id", &self.turn_id)
            .field("context_overflow_remedy", &self.context_overflow_remedy)
            .field("outcome", &outcome)
            .field("recovery_operation", &self.recovery_operation)
            .field("checkpoint_id", &self.checkpoint_id())
            .field("checkpoint", &"<redacted>")
            .field("turn_usage", &self.usage)
            .field("feed_seq", &self.feed_seq)
            .field("conversation_items", &self.conversation.items().len())
            .field("content", &"<redacted>")
            .finish()
    }
}

impl TurnResult {
    /// Returns the sequence of the change-feed record that made this terminal
    /// durable, so a live consumer knows how far to read before it reports
    /// the outcome. `None` without a durable store, for a terminal this owner
    /// rebuilt rather than committed (a duplicate after reopen, a recovered
    /// closure) and for an unrecorded recovery-required fallback.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<crate::feed::FeedSeq> {
        self.feed_seq
    }

    /// Whether accepted same-turn steering was closed without application.
    /// This is derived from exact finalized command evidence, not a transcript.
    #[must_use]
    pub fn has_unapplied_steering(&self) -> bool {
        super::durable::turn_recovery::controls::has_unapplied_steering(
            self.checkpoint.manifest().ledger(),
            self.turn_id,
        )
    }

    /// Returns the exact turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the closed terminal outcome.
    #[must_use]
    pub const fn outcome(&self) -> &TurnResultOutcome {
        &self.outcome
    }

    /// Returns the exact unresolved root operation for `RecoveryRequired`.
    ///
    /// Together with [`Self::checkpoint_id`], this is the typed reconciliation
    /// authority. Every other terminal outcome returns `None`.
    #[must_use]
    pub const fn recovery_operation(&self) -> Option<TurnRecoveryOperationId> {
        self.recovery_operation
    }

    /// Returns the local durability disposition bound by the terminal checkpoint.
    #[must_use]
    pub fn durability(&self) -> RuntimeDurability {
        self.checkpoint.manifest().durability()
    }

    /// Returns the authoritative immutable-checkpoint identity at terminalization.
    #[must_use]
    pub fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint().id()
    }

    pub(super) fn checkpoint(&self) -> &RuntimeCheckpoint {
        &self.checkpoint
    }

    /// Returns the final canonical model aggregate for a completed turn.
    #[must_use]
    pub const fn response(&self) -> Option<&ModelResponse> {
        match &self.outcome {
            TurnResultOutcome::Completed(response) => Some(response),
            TurnResultOutcome::Cancelled(_)
            | TurnResultOutcome::Failed(_)
            | TurnResultOutcome::RecoveryRequired(_) => None,
        }
    }

    /// Returns the terminal error for a non-completed turn.
    #[must_use]
    pub const fn error(&self) -> Option<&AgentError> {
        match &self.outcome {
            TurnResultOutcome::Completed(_) => None,
            TurnResultOutcome::Cancelled(error)
            | TurnResultOutcome::Failed(error)
            | TurnResultOutcome::RecoveryRequired(error) => Some(error),
        }
    }

    /// Returns the exact provider-reported model from the final response.
    #[must_use]
    pub const fn actual_model(&self) -> Option<&ModelId> {
        match self.response() {
            Some(response) => response.actual_model(),
            None => None,
        }
    }

    /// Returns the validated provider request identifier from the final response.
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.response().and_then(ModelResponse::provider_request_id)
    }

    /// Returns the committed conversation immediately after this turn.
    #[must_use]
    pub const fn conversation(&self) -> &ConversationView {
        &self.conversation
    }

    /// Returns the number of model operations used by the logical turn.
    #[must_use]
    pub fn model_rounds(&self) -> usize {
        match usize::try_from(self.usage.model_operations()) {
            Ok(rounds) => rounds,
            Err(_) => usize::MAX,
        }
    }

    /// Returns the number of Plakous-local tool calls dispatched.
    #[must_use]
    pub fn tool_calls(&self) -> usize {
        match usize::try_from(self.usage.local_tool_attempts()) {
            Ok(calls) => calls,
            Err(_) => usize::MAX,
        }
    }

    /// Returns checked usage accumulated across every model round and local tool attempt.
    #[must_use]
    pub const fn usage(&self) -> TurnUsage {
        self.usage
    }

    /// Returns a typed explicit-compaction boundary for a capacity-limited turn.
    ///
    /// The source checkpoint contains every model/tool round committed before
    /// admission failed. A compactable classification permits a separate
    /// caller-authorized compact command; it never authorizes automatic paid
    /// dispatch, retry, history discard, or continuation of this terminal turn.
    #[must_use]
    pub fn context_capacity(&self) -> Option<TurnContextCapacity> {
        self.error()
            .and_then(AgentError::model_error)
            .and_then(ModelError::context_capacity)
            .map(|failure| TurnContextCapacity {
                source_checkpoint: self.checkpoint_id(),
                failure,
            })
    }

    /// Returns the recovery a host can present for a failed turn whose input
    /// did not fit the model's context window or whose protected native history
    /// could not be replayed under the admitted binding.
    #[must_use]
    pub fn context_overflow_remedy(&self) -> Option<ContextOverflowRemedy> {
        self.context_overflow_remedy
    }
}

/// The remedy a turn failing with `error` offers, given whether its prompt
/// was withheld.
pub(in crate::runtime) fn context_overflow_remedy(
    error: &AgentError,
    prompt_withheld: bool,
) -> Option<ContextOverflowRemedy> {
    if error.model_kind() == Some(ModelErrorKind::ProtectedHistory) {
        return Some(ContextOverflowRemedy::ClearOrFork);
    }
    if error.model_kind() != Some(ModelErrorKind::ContextLimit) {
        return None;
    }
    if prompt_withheld {
        return Some(ContextOverflowRemedy::ResendShorter);
    }
    Some(
        match error.model_error().and_then(ModelError::context_capacity) {
            Some(ContextCapacityFailure::Protected { .. }) => ContextOverflowRemedy::ClearOrFork,
            _ => ContextOverflowRemedy::Compact,
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod result_handle_tests {
    use super::*;

    /// The actor publishes one channel and then drops both senders; a waiter
    /// that sees the other channel close first still gets what was
    /// published, never a spurious internal error.
    #[tokio::test]
    async fn a_published_terminal_failure_survives_both_senders_closing() {
        for _ in 0..200 {
            let (result_sender, result) = watch::channel(None);
            let (failure_sender, terminal_failure) = watch::channel(None);
            let handle = TurnResultHandle {
                turn_id: TurnId::try_from(uuid::Uuid::from_u128(7)).unwrap(),
                result,
                terminal_failure,
            };
            let waiting = tokio::spawn(handle.result());
            tokio::task::yield_now().await;
            failure_sender.send_replace(Some(AgentError::new(
                AgentErrorKind::Session,
                "terminal authority could not be published",
            )));
            drop(failure_sender);
            drop(result_sender);
            let error = waiting.await.unwrap().unwrap_err();
            assert_eq!(error.kind(), AgentErrorKind::Session);
        }
    }
}
