//! Starting a child turn, and the attempt bookkeeping around it.

use crate::driver::ledger::CommandLedger as _;

use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind};

use super::{
    Arc, AttachedCodingSession, ChildFeedSource, ChildStartup, ChildStartupStage, CommandId,
    Future, LiveRelease, SPAWN_MATERIALIZE_TIMEOUT, SessionLocator, StartupCaller, TreeDriver,
    TurnId, register_admitted_turn,
};

/// Starts `prompt` on an opened child session and installs it as the live
/// child, with the turn waiter that terminalizes the node when the turn ends.
/// Losing the live slot to a concurrent starter releases this session and
/// reports the occupant's turn instead; a closed hub is an error.
pub(in crate::agent) async fn release_startup(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    startup: &ChildStartup,
    attached: AttachedCodingSession,
) -> Option<SessionLocator> {
    startup.set_stage(ChildStartupStage::Releasing);
    let sessions = Arc::clone(&attached.sessions);
    let (released, release) = match attached.release_opened().await {
        Ok(locator) => (Some(locator), {
            let (cursor, recorded) =
                crate::agent::live::release_observation(runtime.driver(), locator);
            crate::agent::live::released_with_feed(&sessions, locator, None, cursor, recorded).await
        }),
        Err(error) => (
            None,
            LiveRelease {
                feed: None,
                locator: None,
                failure: Some(format!("child startup release failed: {error}")),
            },
        ),
    };
    runtime.finish_startup(agent_id, startup, release).await;
    released
}

/// Why a created child's assignment did not start.
pub(in crate::agent) enum ChildStartFailure {
    /// Storage pressure refused the start before anything ran, and the child
    /// session was released and deleted: a definite spawn refusal.
    Refused(String),
    /// Anything else; the child's startup outcome needs recovery.
    Failed(String),
    /// A reopen wake's hold ended after the runtime admitted its Start, so
    /// the turn was not bound and its session was released. Its batch is
    /// already in the session's history.
    HoldEndedAfterAdmission(String),
    /// Another turn or startup held the child's live slot when this Start
    /// reserved it. Nothing was reserved, the opened attachment was
    /// released, and nothing is at fault.
    SlotOccupied(String),
}

impl From<String> for ChildStartFailure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for ChildStartFailure {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

impl From<ChildStartFailure> for String {
    fn from(failure: ChildStartFailure) -> Self {
        match failure {
            ChildStartFailure::Refused(message)
            | ChildStartFailure::Failed(message)
            | ChildStartFailure::HoldEndedAfterAdmission(message)
            | ChildStartFailure::SlotOccupied(message) => message,
        }
    }
}

/// What a child Start must find before it reserves the child and binds its
/// turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::agent) enum StartAdmission {
    /// Any Start the tree already admitted (a spawn, a mail Start).
    Ordinary,
    /// A held child's reopen wake: its reservation and its bind each
    /// re-check the hold, so an interrupt or close that ended it refuses the
    /// Start instead of the reservation writing `running` over it.
    ReopenWake,
    /// A continuation after a loss under its recorded command id: its
    /// reservation and its bind each re-check that the continuation is still
    /// pending, so a close, an interrupt or another turn that ended it
    /// refuses the Start.
    LossContinuation(String),
}

/// The refusal a continuation's admission checks return once something
/// ended it.
pub(in crate::agent) const LOSS_CONTINUATION_ENDED: &str = "the continuation after the loss ended before it started; a close, an interrupt or another turn won the race";

/// Whether `error` is an admission check's refusal for a hold that ended:
/// nothing started or bound, and nothing is at fault.
pub(in crate::agent) fn hold_ended(error: &str) -> bool {
    error == crate::agent::park::REOPEN_HOLD_ENDED || error == LOSS_CONTINUATION_ENDED
}

impl StartAdmission {
    pub(super) fn check(
        &self,
        tree: &crate::agent_tree::ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), String> {
        match self {
            Self::Ordinary => Ok(()),
            Self::ReopenWake if tree.reopen_wake_admissible(agent_id) => Ok(()),
            Self::ReopenWake => Err(crate::agent::park::REOPEN_HOLD_ENDED.to_owned()),
            Self::LossContinuation(command_id)
                if tree.loss_continuation_admissible(agent_id, command_id) =>
            {
                Ok(())
            }
            Self::LossContinuation(_) => Err(LOSS_CONTINUATION_ENDED.to_owned()),
        }
    }
}

pub(in crate::agent) async fn start_child_turn(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: ChildCommand,
) -> Result<TurnId, ChildStartFailure> {
    start_child_turn_as(
        runtime,
        agent_id,
        attached,
        command_id,
        prompt,
        StartAdmission::Ordinary,
    )
    .await
}

/// A resubmitted child command whose original the runtime recorded below
/// the feed's retention: it settled consumed and started no turn.
pub(in crate::agent) const CHILD_COMMAND_CONSUMED: &str = "the child command was recorded below the feed's retention; it settled consumed and started \
     no turn";

pub(in crate::agent) async fn start_child_turn_as(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: ChildCommand,
    admission: StartAdmission,
) -> Result<TurnId, ChildStartFailure> {
    let expected_attempt = runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .map(|attempt| attempt.attempt_id.clone())
        })
        .ok()
        .flatten();
    let deadline = tokio::time::Instant::now() + SPAWN_MATERIALIZE_TIMEOUT;
    let reserved = tokio::time::timeout_at(
        deadline,
        runtime.reserve_startup_admitted(agent_id, {
            let (id, admission) = (agent_id.to_owned(), admission.clone());
            move |tree| admission.check(tree, &id)
        }),
    )
    .await
    .unwrap_or_else(|_| Err("child startup admission deadline exceeded".into()));
    let startup = match reserved {
        Ok(startup) => startup,
        // The hold ended: nothing was reserved and nothing is at fault.
        Err(error) if hold_ended(&error) => {
            let _ = attached.release_opened().await;
            return Err(ChildStartFailure::Refused(error));
        }
        // Another turn or startup won the slot: it owns the child, so this
        // Start only gives back its own attachment.
        Err(error) if error == crate::topology_runtime::STARTUP_SLOT_OCCUPIED => {
            let _ = attached.release_opened().await;
            return Err(ChildStartFailure::SlotOccupied(error));
        }
        Err(error) => {
            // Release the rejected attachment independently of its caller, as a
            // release owner tree shutdown does not abort.
            let owner = runtime.clone();
            let id = agent_id.to_owned();
            let released_attempt = expected_attempt.clone();
            runtime.driver().spawn_release_owner(async move {
                if attached.release_opened().await.is_err() {
                    let child = id.clone();
                    let _ = owner
                        .record_then_apply(&id, move |tree| {
                            tree.record_host_retirement_for_attempt(
                                &child,
                                released_attempt.as_ref(),
                                "rejected startup attachment release failed".into(),
                            )
                            .map(|_| ())
                        })
                        .await;
                }
            });
            record_start_failure(runtime, agent_id, expected_attempt.as_ref(), &error).await?;
            return Err(error.into());
        }
    };
    let caller = StartupCaller::new(runtime.clone(), agent_id.to_owned(), startup.clone());
    let owner = runtime.clone();
    let id = agent_id.to_owned();
    let task = runtime.driver().spawn_release_owner(async move {
        #[cfg(test)]
        let acknowledged = {
            let (id, continuation) = (
                id.clone(),
                matches!(admission, StartAdmission::LossContinuation(_)),
            );
            async move {
                if continuation {
                    crate::agent::park::hooks::after_loss_continuation_admission(&id).await;
                }
            }
        };
        #[cfg(not(test))]
        let acknowledged = std::future::ready(());
        start_child_turn_owned_with_ack(
            &owner,
            &id,
            attached,
            command_id,
            prompt,
            &startup,
            false,
            admission,
            acknowledged,
        )
        .await
    });
    await_child_start(caller, task, deadline).await
}

pub(super) async fn record_start_failure(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    expected: Option<&crate::agent_tree_protocol::AttemptId>,
    error: &str,
) -> Result<(), String> {
    let (id, expected, error) = (agent_id.to_owned(), expected.cloned(), error.to_owned());
    runtime
        .record_then_apply(agent_id, move |tree| {
            tree.record_host_retirement_for_attempt(&id, expected.as_ref(), error)
                .map(|_| ())
        })
        .await
}

pub(super) async fn await_child_start(
    mut caller: StartupCaller,
    task: crate::TaskHandle<Result<TurnId, ChildStartFailure>>,
    deadline: tokio::time::Instant,
) -> Result<TurnId, ChildStartFailure> {
    let result = match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(Ok(turn))) => { caller.disarm(); Ok(turn) }
        Ok(Ok(Err(error))) => { caller.disarm(); Err(error) },
        Ok(Err(_)) => Err("child startup owner failed; recovery required".into()),
        Err(_) => Err("child startup acknowledgement deadline exceeded; owner retained for cancellation and release".into()),
    };
    if let Err(ChildStartFailure::Failed(error)) = &result {
        let startup = caller.startup.clone();
        let failure = error.clone();
        caller
            .runtime
            .record_then_apply(&caller.agent_id, move |tree| {
                startup.record_retirement_failure(tree, failure).map(|_| ())
            })
            .await
            .map_err(ChildStartFailure::Failed)?;
    }
    result
}

pub(super) async fn start_child_turn_owned(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: ChildCommand,
    startup: &ChildStartup,
) -> Result<TurnId, ChildStartFailure> {
    start_child_turn_owned_with_ack(
        runtime,
        agent_id,
        attached,
        command_id,
        prompt,
        startup,
        true,
        StartAdmission::Ordinary,
        std::future::ready(()),
    )
    .await
}

/// What a child Start admits: a prompt the result journal prepares.
pub(in crate::agent) struct ChildCommand {
    prompt: String,
    kind: CommandKind,
    mail_id: Option<String>,
}

impl ChildCommand {
    pub(in crate::agent) fn fresh(prompt: String, kind: CommandKind) -> Self {
        Self {
            prompt,
            kind,
            mail_id: None,
        }
    }

    pub(super) fn mail_start(prompt: String, mail_id: String) -> Self {
        Self {
            prompt,
            kind: CommandKind::MailStart,
            mail_id: Some(mail_id),
        }
    }
}

// Keeping acknowledgement delivery separate from admission lets the offline
// fixture lose/delay the response while the real runtime owns an admitted turn.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Owned child admission keeps the exact start request, pending-delivery receipt, cancellation token, and acknowledgement channel distinct."
)]
pub(in crate::agent) async fn start_child_turn_owned_with_ack(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: ChildCommand,
    startup: &ChildStartup,
    delete_on_definite_refusal: bool,
    admission: StartAdmission,
    acknowledged: impl Future<Output = ()> + Send,
) -> Result<TurnId, ChildStartFailure> {
    let locator = attached.locator().to_string();
    let child = agent_id.to_owned();
    let publication = runtime
        .mutate_then_emit(false, move |tree| {
            tree.record_child_session(&child, &locator)
                .map_err(|error| error.message)?;
            Ok(())
        })
        .await;
    if let Err(error) = publication {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err(error.into());
    }
    if let Err(error) = crate::agent::ledger::project_child_feed(runtime, agent_id, &attached).await
    {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err(error.into());
    }
    if startup.is_cancelled() || runtime.is_closed() {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("child startup cancelled before assignment admission".into());
    }
    let command_text = command_id.as_str().to_owned();
    let ChildCommand {
        prompt,
        kind,
        mail_id,
    } = prompt;
    let batch = match kind {
        CommandKind::Wake | CommandKind::Resume => BatchRule::WakeAll,
        CommandKind::MailStart => BatchRule::MailStart(
            mail_id
                .as_deref()
                .ok_or("mail Start has no message identity")?,
        ),
        _ => BatchRule::Results,
    };
    let result_context = match crate::agent::ledger::ChildLedger::new(runtime.driver(), agent_id)
        .prepare(CommandIntent {
            command_id: &command_text,
            kind,
            base_text: &prompt,
            batch,
        })
        .await
        .map_err(String::from)
    {
        Ok(context) => context,
        Err(error) => {
            release_startup(runtime, agent_id, startup, attached).await;
            return Err(error.into());
        }
    };
    startup.set_stage(ChildStartupStage::Starting);
    let Some(agent) = attached.agent() else {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("opened child has no agent".into());
    };
    let command = if result_context.instruction {
        plakous::agent::TurnCommand::StartInstruction {
            command_id,
            text: result_context.prompt.clone(),
        }
    } else {
        plakous::agent::TurnCommand::Start {
            command_id,
            input: result_context.prompt.clone().into(),
        }
    };
    let mut submission = match agent.command(command).await {
        Ok(submission) => crate::runtime::child_projection::ChildAdmission::new(submission, agent),
        Err(error) => {
            let message = format!("child turn start was refused: {error}");
            release_startup(runtime, agent_id, startup, attached).await;
            return Err(message.into());
        }
    };
    acknowledged.await;
    // A resubmission whose original was recorded below the feed's retention
    // started nothing now: it settles consumed (spec 03 section 14.1).
    match crate::agent::ledger::consumed_below_retention(
        runtime.driver(),
        &attached.sessions,
        attached.locator().session_id(),
        &command_text,
        submission.acknowledgement.disposition(),
        submission
            .acknowledgement
            .rejection()
            .is_some_and(|rejection| {
                rejection.kind() == plakous::agent::ControlErrorKind::Reclaimed
            }),
    )
    .await
    {
        Ok(false) => {}
        consumed => {
            let settled = match consumed {
                Ok(_) => crate::agent::ledger::ChildLedger::settlement_only(runtime.driver())
                    .settle(&command_text, crate::driver::ledger::Settlement::Consumed)
                    .await
                    .map_err(String::from),
                Err(error) => Err(error),
            };
            release_startup(runtime, agent_id, startup, attached).await;
            return Err(settled
                .err()
                .unwrap_or_else(|| CHILD_COMMAND_CONSUMED.to_owned())
                .into());
        }
    }
    if let Some(rejection) = submission.acknowledgement.rejection() {
        let storage_pressure =
            rejection.kind() == plakous::agent::ControlErrorKind::StoragePressure;
        let rejection = rejection.to_string();
        let settled = crate::agent::ledger::ChildLedger::settlement_only(runtime.driver())
            .settle(
                &command_text,
                crate::driver::ledger::Settlement::Refused { pause: None },
            )
            .await
            .map_err(String::from);
        // Claim removal authority before the first cleanup await. A caller
        // cancellation that wins first keeps the uncertain fresh genesis for
        // later GC; a cleanup claim that wins fences all later cancellation
        // requests through release, archive and delete.
        let cleanup_owned = storage_pressure
            && settled.is_ok()
            && delete_on_definite_refusal
            && startup.claim_definite_refusal_cleanup();
        let sessions = Arc::clone(&attached.sessions);
        let released = release_startup(runtime, agent_id, startup, attached).await;
        // Storage pressure refused new work before the turn was admitted, so
        // nothing ran: the spawn is a definite refusal. The just-created
        // session is archived and deleted (never refused for capacity), so
        // the refusal leaves no child session behind.
        if storage_pressure && settled.is_ok() {
            if !delete_on_definite_refusal && released.is_some() {
                return Err(ChildStartFailure::Refused(rejection));
            }
            let deleted = match (cleanup_owned, released) {
                (false, _) => false,
                (true, Some(locator)) => match sessions.archive(locator).await {
                    Ok(report) => sessions
                        .delete(report.session_id(), report.archived_revision())
                        .await
                        .is_ok(),
                    Err(_) => false,
                },
                (true, None) => false,
            };
            if delete_on_definite_refusal && deleted {
                return Err(ChildStartFailure::Refused(rejection));
            }
        }
        return Err(settled
            .err()
            .unwrap_or_else(|| {
                format!("child start was rejected; result mail remains pending: {rejection}")
            })
            .into());
    }
    let Some(turn_id) = submission.acknowledgement.turn_id() else {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("start produced no turn identity".into());
    };
    startup.set_stage(ChildStartupStage::Registering);
    // The turn's waiter registers with the bind, so the bound turn never
    // reads as one nobody will release. Kept outside the lock: it is dropped
    // (unregistered) on every path that never spawns the waiter.
    let mut registration = None;
    if let Err(error) =
        register_admitted_turn(runtime, admission, agent_id, turn_id, &mut registration).await
    {
        // Projection precedes release: the construction attachment is not
        // itself evidence that the command was admitted.
        let projection =
            crate::agent::ledger::project_child_feed(runtime, agent_id, &attached).await;
        release_startup(runtime, agent_id, startup, attached).await;
        if hold_ended(&error) {
            projection.map_err(ChildStartFailure::from)?;
            crate::agent::ledger::ChildLedger::settlement_only(runtime.driver())
                .settle(&command_text, crate::driver::ledger::Settlement::Recorded)
                .await
                .map_err(String::from)
                .map_err(ChildStartFailure::from)?;
            return Err(ChildStartFailure::HoldEndedAfterAdmission(error));
        }
        return Err(error.into());
    }
    if let Err(error) = crate::agent::ledger::project_child_feed(runtime, agent_id, &attached).await
    {
        super::TurnWaiterRegistration::release_taken(&mut registration).await;
        release_startup(runtime, agent_id, startup, attached).await;
        return Err(error.into());
    }
    // A fresh turn can retire before registration; its admission capability
    // survives retirement. Duplicates have no new turn capability to retain.
    let control = match submission.acknowledgement.disposition() {
        plakous::agent::CommandAckDisposition::Accepted => submission.control.clone(),
        plakous::agent::CommandAckDisposition::Duplicate => agent.turn_control(turn_id),
        _ => None,
    };
    let Some(control) = control else {
        super::TurnWaiterRegistration::release_taken(&mut registration).await;
        release_startup(runtime, agent_id, startup, attached).await;
        if submission.acknowledgement.disposition()
            == plakous::agent::CommandAckDisposition::Duplicate
        {
            return Err(
                "duplicate child start has no live turn; result reconciliation required".into(),
            );
        }
        return Err("child turn control is unavailable".into());
    };
    let Some(result) = submission.result.clone() else {
        super::TurnWaiterRegistration::release_taken(&mut registration).await;
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("start produced no result handle".into());
    };
    let feed = ChildFeedSource::of(&attached);
    let projection = submission.projection.take().unwrap_or_else(|| {
        crate::runtime::child_projection::ChildTurnProjection::recovered(
            Some(agent.clone()),
            turn_id,
        )
    });
    let actor = crate::agent::live::ChildActorHandle::admitted(
        &runtime.driver().release_owners(),
        attached,
        turn_id,
        control,
        projection,
    );
    let watcher = actor.watcher();
    let child: Box<dyn crate::topology_runtime::LiveChildHandle> = Box::new(actor);
    startup.set_stage(ChildStartupStage::Registering);
    let accepted = matches!(
        submission.acknowledgement.disposition(),
        plakous::agent::CommandAckDisposition::Accepted
            | plakous::agent::CommandAckDisposition::Duplicate
    );
    let bound = if accepted {
        crate::agent::ledger::ChildLedger::settlement_only(runtime.driver())
            .settle(&command_text, crate::driver::ledger::Settlement::Recorded)
            .await
            .map_err(String::from)
    } else {
        Err("child command was not admitted; result context retained".to_owned())
    };
    if let Err(error) = bound {
        super::TurnWaiterRegistration::release_taken(&mut registration).await;
        startup.set_stage(ChildStartupStage::Releasing);
        runtime
            .finish_startup(
                agent_id,
                startup,
                child
                    .cancel_and_release(runtime.driver().clone(), None)
                    .await,
            )
            .await;
        return Err(error.into());
    }
    match runtime
        .occupy_started_child(agent_id.to_owned(), startup, child)
        .await
    {
        Ok(turn_id) => {
            if let Some(registration) = registration {
                watcher.watch(
                    registration,
                    Arc::clone(&runtime.live),
                    runtime.observer(),
                    Some(feed),
                    result,
                );
            }
            Ok(turn_id)
        }
        Err(loser) => {
            // No watch will start: release must not wait for its notice.
            super::TurnWaiterRegistration::release_taken(&mut registration).await;
            startup.set_stage(ChildStartupStage::Releasing);
            runtime
                .finish_startup(
                    agent_id,
                    startup,
                    loser
                        .cancel_and_release(runtime.driver().clone(), None)
                        .await,
                )
                .await;
            Err("child startup cancelled or hub closed; late acknowledgement reconciled through cancellation and release".into())
        }
    }
}

/// Removes a finished child from the live map, releases its session, and
/// advances the node's `session_locator` to the released head.
///
/// The locator bound at spawn is the *created* head; every turn moves the
/// branch, so a later `followup` that reopened the spawn-time locator failed
/// `StaleLocator` and poisoned the node. Releasing here is also what the lease
/// needs: the old path dropped the `OpenedSession` without `release`.
pub async fn retire_live_child(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    expected_turn: TurnId,
) {
    if let Err(error) =
        crate::topology_runtime::retire_live(driver, live, agent_id, Some(expected_turn), false)
            .await
    {
        tracing::error!(target: "plak.topology", agent_id, error = %error, "finished child retirement failed");
    }
}

/// Applies one child mutation and settles its effects after releasing the lock.
/// Callers explicitly publish their durable mutation within `work`.
pub(in crate::agent) async fn mutate_child<R: Send + 'static>(
    driver: &TreeDriver,
    agent_id: &str,
    work: impl FnOnce(&mut crate::agent_tree::ProductAgentTree) -> R + Send + 'static,
) -> Option<R> {
    let (result, effects) = driver.mutate(work).await.ok()?;
    settle_or_fault(driver, effects, agent_id).await;
    Some(result)
}

/// Publishes the tree after a turn-waiter mutation. A refused publish — the
/// store's compare-and-swap found a revision another handle wrote, or the
/// store is gone — is never silent: it is logged and recorded as the node's
/// fault, so the canvas shows that this process's copy of the tree has
/// diverged from the store. The in-memory rows are kept as they are; nothing
/// here retries or reloads, because which copy is right is an operator call.
/// Returns whether the publish succeeded.
pub fn persist_or_fault(
    driver: &TreeDriver,
    tree: &mut crate::agent_tree::ProductAgentTree,
    agent_id: &str,
) -> bool {
    let Err(error) = driver.for_agent(agent_id).save(tree) else {
        return true;
    };
    if error.code == crate::agent_tree_protocol::TopologyErrorCode::RecoveryStorageUnavailable {
        return false;
    }
    tracing::error!(
        target: "plak.topology",
        tree_id = %tree.tree_id,
        agent_id,
        revision = tree.revision,
        stored_revision = tree.stored_revision,
        error = %error.message,
        "tree publish refused; this process's tree has diverged from the store"
    );
    let reason = match tree.fault_reason(agent_id) {
        Some(prior) => format!("{prior}; tree publish refused: {}", error.message),
        None => format!("tree publish refused: {}", error.message),
    };
    tree.record_fault(agent_id, reason);
    false
}

/// Runs a turn-waiter effects batch through the driver. The
/// drain records every effect before a refused publish is reported, so a
/// seal that ran is in the tree even when the store would not take it; the
/// refusal is then surfaced like [`persist_or_fault`].
pub async fn settle_or_fault(
    driver: &TreeDriver,
    effects: crate::agent_tree::Effects,
    agent_id: &str,
) {
    if effects.is_empty() {
        return;
    }
    let Err(error) = driver.for_agent(agent_id).settle(effects).await else {
        return;
    };
    if error.code == crate::agent_tree_protocol::TopologyErrorCode::RecoveryStorageUnavailable {
        return;
    }
    tracing::error!(
        target: "plak.topology",
        agent_id,
        error = %error.message,
        "tree effects were recorded but their publish was refused"
    );
    let id = agent_id.to_owned();
    Box::pin(mutate_child(driver, agent_id, move |tree| {
        let reason = match tree.fault_reason(&id) {
            Some(prior) => format!("{prior}; tree publish refused: {}", error.message),
            None => format!("tree publish refused: {}", error.message),
        };
        tree.record_fault(&id, reason);
    }))
    .await;
}

#[derive(Clone)]
pub(in crate::agent) struct ChildTurnIdentity {
    pub(in crate::agent) attempt: crate::agent_tree_protocol::AttemptId,
    pub(in crate::agent) turn: TurnId,
}

impl ChildTurnIdentity {
    pub(in crate::agent) fn still_owns(
        &self,
        tree: &crate::agent_tree::ProductAgentTree,
        agent_id: &str,
    ) -> bool {
        tree.attempt(agent_id).is_some_and(|attempt| {
            attempt.attempt_id == self.attempt
                && tree
                    .owned_turn_id(agent_id)
                    .or(attempt.turn_id.as_deref())
                    .is_none_or(|turn| turn == self.turn.to_string())
        })
    }
}

pub(in crate::agent) async fn end_attempt_work(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> Result<(), String> {
    let driver = &driver.for_agent(agent_id);
    let result = async {
        let writer = driver.clone();
        let owner = identity.clone();
        let id = agent_id.to_owned();
        let (result, effects) = driver
            .mutate(move |tree| {
                if tree.revision != tree.stored_revision {
                    writer.save(tree)?;
                }
                let agent_id = id.as_str();
                writer.fold_and_commit(tree, |tree| {
                    if !owner.still_owns(tree, agent_id)
                        || tree.node(agent_id).is_ok_and(|node| {
                            matches!(
                                node.status,
                                crate::agent_tree_protocol::NodeStatus::Interrupted
                                    | crate::agent_tree_protocol::NodeStatus::RecoveryRequired
                            )
                        })
                    {
                        return Ok((Vec::new(), None, false));
                    }
                    let mut closed = Vec::new();
                    let mut failure = None;
                    for child in tree.live_child_ids(agent_id) {
                        match tree.close_agent(
                            agent_id,
                            &crate::agent_tree_protocol::AttemptScopedRequest {
                                agent_id: child,
                                expected_attempt_id: None,
                            },
                            None,
                        ) {
                            Ok(nodes) => closed.extend(nodes.into_iter().map(|(id, _)| id)),
                            Err(error) => {
                                failure.get_or_insert(error.message);
                            }
                        }
                    }
                    Ok((closed, failure, true))
                })
            })
            .await
            .map_err(|error| error.message)?;
        // Held across the retirements and the job stop, so an early return
        // or an abort leaves the closed copies' seals to a release owner.
        let effects = driver.hold_effects(effects);
        let (closed, mut failure, ended) = result.map_err(|error| error.message)?;
        if !ended {
            return effects
                .settle()
                .await
                .map(|_| ())
                .map_err(|error| error.message);
        }
        let mut owners = closed.clone();
        owners.push(agent_id.to_owned());
        let mut retirements = tokio::task::JoinSet::new();
        for child in closed {
            let driver = driver.clone();
            let live = Arc::clone(live);
            retirements.spawn(async move {
                crate::topology_runtime::retire_live(&driver, &live, &child, None, true).await
            });
        }
        while let Some(result) = retirements.join_next().await {
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    failure.get_or_insert(error.to_string());
                }
                Err(error) => {
                    failure.get_or_insert(error.to_string());
                }
            }
        }
        if let Err(error) = driver.stop_agent_jobs(&owners).await {
            failure.get_or_insert(error.message);
        }
        if let Err(error) = effects.settle().await {
            failure.get_or_insert(error.message);
        }
        failure.map_or(Ok::<_, String>(()), Err)
    }
    .await;
    if let Err(error) = &result {
        retire_live_child(driver, live, agent_id, identity.turn).await;
        record_attempt_end_failure(driver, agent_id, identity, error).await;
    }
    result
}

pub(super) async fn record_attempt_end_failure(
    driver: &TreeDriver,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    error: &str,
) {
    let writer = driver.clone();
    let (id, attempt, error) = (
        agent_id.to_owned(),
        identity.attempt.clone(),
        error.to_owned(),
    );
    mutate_child(driver, agent_id, move |tree| {
        let _ = tree
            .record_host_retirement_for_attempt(&id, Some(&attempt), error)
            .and_then(|value| writer.apply_recorded(tree, &id).map(|()| value));
        persist_or_fault(&writer, tree, &id);
    })
    .await;
}
