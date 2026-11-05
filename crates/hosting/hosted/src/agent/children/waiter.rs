//! The turn waiter: what runs when a child turn ends, and resuming it.

use crate::driver::ledger::CommandLedger as _;

use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind};
use crate::driver::transient::{self, ResumeGate, ResumeOwner};
use crate::node::AfterTerminal;

use super::{
    Arc, AttachedCodingSession, ChildReopen, ChildReopenMode, ChildTurnIdentity, CommandId,
    StartAdmission, SubmitNudgeBudget, TreeDriver, TurnId, TurnOutcome, TurnResult,
    TurnResultHandle, TurnResultOutcome, end_attempt_work, failed_worktree, mutate_child,
    persist_or_fault, reopen_spawned_child, retire_live_child, settle_or_fault,
    submit_harness_prompt,
};

#[cfg(test)]
mod diagnostics;
#[cfg(test)]
mod leave_tests;
mod resume;

#[cfg(test)]
pub(in crate::agent) use resume::transient_resume_bound;
#[cfg(test)]
pub(super) use resume::wait_transient_gate;
use resume::{resume_allowed, try_transient_resume};

fn warn_recovery_required(
    agent_id: &str,
    attempt: &crate::agent_tree_protocol::AttemptId,
    turn_id: TurnId,
    error_kind: plakous::agent::AgentErrorKind,
) {
    tracing::warn!(target: "plak.topology", agent_id, attempt_id = %attempt, %turn_id,
        ?error_kind, "child turn requires recovery; recycling child");
}

/// A turn waiter's registration on the tree. It is taken before the turn
/// it watches is bound or occupies the live slot, so a bound turn some
/// waiter will release always reads as watched; the child actor's watch
/// holds it for its life however it ends (a panic or an aborted actor
/// included), and a turn that never gets its watch drops it. A send tells a bound turn its waiter
/// will still release from one nobody watches by it (spec 03 section 9).
///
/// Leave it with [`Self::release`]; a registration dropped instead (a
/// panic, an aborted task) leaves through an owed tree writer run by a
/// release owner, or on a thread of its own when no runtime will run one.
pub(in crate::agent) struct TurnWaiterRegistration {
    pub(super) driver: TreeDriver,
    pub(super) agent_id: String,
    pub(super) attempt: crate::agent_tree_protocol::AttemptId,
    entered: bool,
}

pub(super) async fn register_admitted_turn(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    admission: StartAdmission,
    agent_id: &str,
    turn_id: TurnId,
    registration: &mut Option<TurnWaiterRegistration>,
) -> Result<(), String> {
    let driver = runtime.driver().clone();
    let id = agent_id.to_owned();
    *registration = runtime
        .prepare_then_emit(move |tree| {
            admission.check(tree, &id)?;
            tree.bind_turn_owner(&id, turn_id.to_string());
            Ok(TurnWaiterRegistration::enter(&driver, tree, &id))
        })
        .await?;
    Ok(())
}

impl TurnWaiterRegistration {
    /// Registers a waiter for `agent_id`'s current attempt under the
    /// caller's tree lock; `None` when the child has no attempt.
    pub(in crate::agent) fn enter(
        driver: &TreeDriver,
        tree: &mut crate::agent_tree::ProductAgentTree,
        agent_id: &str,
    ) -> Option<Self> {
        let attempt = tree.attempt(agent_id)?.attempt_id.clone();
        tree.enter_turn_waiter(agent_id);
        Some(Self {
            driver: driver.clone(),
            agent_id: agent_id.to_owned(),
            attempt,
            entered: true,
        })
    }

    /// [`Self::enter`] under its own tree lock.
    #[cfg(test)]
    pub(in crate::agent) async fn register(driver: &TreeDriver, agent_id: &str) -> Option<Self> {
        let writer = driver.clone();
        let id = agent_id.to_owned();
        let (registration, effects) = driver
            .mutate(move |tree| Self::enter(&writer, tree, &id))
            .await
            .ok()?;
        settle_or_fault(driver, effects, agent_id).await;
        registration
    }

    /// Ends the registration before its holder goes on.
    pub(in crate::agent) async fn release(mut self) {
        self.entered = false;
        // A release owner: a holder that leaves mid-way still leaves it.
        let leave = Self::leave(self.driver.clone(), self.agent_id.clone());
        let _ = self.driver.spawn_release_owner(leave).await;
    }

    /// Releases an optional registration.
    pub(in crate::agent) async fn release_taken(registration: &mut Option<Self>) {
        if let Some(registration) = registration.take() {
            registration.release().await;
        }
    }

    async fn leave(driver: TreeDriver, agent_id: String) {
        let id = agent_id.clone();
        // Owed: the registration is left however long an outage holds the tree.
        let left = driver
            .mutate_owed(move |tree| tree.leave_turn_waiter(&id))
            .await;
        if let Ok(((), effects)) = left {
            settle_or_fault(&driver, effects, &agent_id).await;
        }
    }
}

impl Drop for TurnWaiterRegistration {
    fn drop(&mut self) {
        if !std::mem::take(&mut self.entered) {
            return;
        }
        let mut pending = PendingLeave(Some((self.driver.clone(), self.agent_id.clone())));
        if tokio::runtime::Handle::try_current().is_err() {
            // Leaves on a thread of its own as `pending` drops.
            return;
        }
        // A release owner, which no root's release aborts. A task its
        // shutting-down runtime never starts drops `pending` unrun, which
        // then leaves on a thread of its own.
        drop(self.driver.spawn_release_owner(async move {
            if let Some((driver, agent_id)) = pending.0.take() {
                Self::leave(driver, agent_id).await;
            }
        }));
    }
}

/// A registration's leave no task has started: dropped so, it leaves on a
/// thread with a runtime of its own.
struct PendingLeave(Option<(TreeDriver, String)>);

impl Drop for PendingLeave {
    fn drop(&mut self) {
        let Some((driver, agent_id)) = self.0.take() else {
            return;
        };
        let leave = TurnWaiterRegistration::leave(driver, agent_id.clone());
        let leaver = std::thread::Builder::new().name("plak-waiter-leave".into());
        let spawned = leaver.spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(leave);
            }
        });
        if spawned.is_err() {
            tracing::error!(target: "plak.topology", agent_id = %agent_id,
                "dropped turn waiter registration could not be left");
        }
    }
}

/// The waiter loop over one bound attempt's turns, which the child's actor
/// drives (`ChildWatcher::watch`). It owns the driver it was registered
/// with: its saves and settles go to the store the hub bound, whether or not
/// the hub still lists this tree when the turn ends (a root that quit
/// releases the hub entry while a child turn can still be unwinding). The
/// actor runs as a release owner, which the tree's shutdown does not abort.
pub(in crate::agent) fn turn_watch(
    registration: TurnWaiterRegistration,
    live: Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    observer: Option<Arc<dyn crate::agent_tree::TopologyObserver>>,
    feed: Option<ChildFeedSource>,
    result: TurnResultHandle,
) -> crate::agent::live::TurnWatch {
    Box::pin(async move {
        let registration = registration;
        let mut turns = ChildTurns {
            driver: registration.driver.clone(),
            agent_id: registration.agent_id.clone(),
            attempt: registration.attempt.clone(),
            live,
            observer,
            feed,
            // What the last wake carried, so leftovers cannot wake forever.
            carried: crate::agent::park::Arrivals::new(),
        };
        Box::pin(crate::node::drive_turns(&mut turns, &mut (), result)).await;
        registration.release().await;
    })
}

/// One bound attempt's turns, as its waiter loop drives them.
struct ChildTurns {
    driver: TreeDriver,
    agent_id: String,
    attempt: crate::agent_tree_protocol::AttemptId,
    live: Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    observer: Option<Arc<dyn crate::agent_tree::TopologyObserver>>,
    feed: Option<ChildFeedSource>,
    carried: crate::agent::park::Arrivals,
}

/// A child turn at its terminal.
struct ChildTerminal {
    completed_turn: TurnId,
    identity: ChildTurnIdentity,
    terminal: Result<Arc<TurnResult>, plakous::agent::AgentError>,
}

impl crate::node::TurnRole<()> for ChildTurns {
    type Turn = TurnResultHandle;
    type Terminal = ChildTerminal;
    type Settled = ();

    async fn drive(&mut self, (): &mut (), handle: TurnResultHandle) -> ChildTerminal {
        let completed_turn = handle.turn_id();
        let identity = ChildTurnIdentity {
            attempt: self.attempt.clone(),
            turn: completed_turn,
        };
        // The turn's one drive: its events into the read model every face
        // reads, beside its result.
        let observation = self
            .live
            .lock()
            .await
            .get(&self.agent_id)
            .and_then(crate::runtime::child_projection::of)
            .filter(|observation| observation.turn() == completed_turn);
        let terminal = match observation {
            Some(observation) => observation.drive(handle.result()).await,
            None => handle.result().await,
        };
        ChildTerminal {
            completed_turn,
            identity,
            terminal,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The child waiter owns observation, transient recovery, result publication, and terminal release for exactly one bound attempt."
    )]
    async fn after_terminal(
        &mut self,
        (): &mut (),
        terminal: ChildTerminal,
    ) -> AfterTerminal<TurnResultHandle, ()> {
        let ChildTerminal {
            completed_turn,
            identity,
            terminal,
        } = terminal;
        let (driver, agent_id) = (self.driver.clone(), self.agent_id.clone());
        let (attempt, live, observer, feed) =
            (&self.attempt, &self.live, &self.observer, &self.feed);
        let carried = &mut self.carried;
        // A child has no operator to wait for, so a turn its owner fenced
        // is recovered at once: release the stopped session and reopen it,
        // which abandons the turn exactly as a crash reattach would. The
        // attempt then ends as an ordinary failure the parent can re-task.
        if let Ok(result) = &terminal
            && let TurnResultOutcome::RecoveryRequired(error) = result.outcome()
        {
            warn_recovery_required(&agent_id, attempt, completed_turn, error.kind());
            project_turn_end(&driver, feed.as_ref(), &agent_id).await;
            if driver
                .read(|tree| identity.still_owns(tree, &agent_id))
                .unwrap_or(false)
                && driver
                    .stop_agent_jobs(std::slice::from_ref(&agent_id))
                    .await
                    .is_err()
            {
                retire_live_child(&driver, live, &agent_id, completed_turn).await;
                return AfterTerminal::Settled(());
            }
            // The node is marked recycling before its live entry goes, and
            // the mark is cleared only once the outcome is recorded, so a
            // recovery of it (which reads the live entry, then the mark)
            // starts only after this failure report.
            let recycling = driver
                .read(|tree| tree.tree_id.clone())
                .ok()
                .and_then(|tree_id| crate::topology_runtime::lookup(&tree_id))
                .map(|runtime| runtime.mark_recycling(&agent_id));
            retire_live_child(&driver, live, &agent_id, completed_turn).await;
            let recycled = recycle_fenced_child(&driver, &agent_id).await;
            let (disposition, message) = match recycled {
                Some(FencedChildRecycle::Abandoned(report)) => (
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                        kind: crate::agent_tree::TURN_ABANDONED,
                    },
                    crate::runtime::events::abandoned_turn_operator_line(&report).to_owned(),
                ),
                Some(FencedChildRecycle::Quiescent) => (
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                        kind: crate::agent_tree::TURN_ABANDONED,
                    },
                    "child session reopened quiescent after local finalization; \
                     the attempt left no result"
                        .to_owned(),
                ),
                Some(FencedChildRecycle::QuiescenceUnproved) => (
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    "child session reopened but quiescence could not be established".to_owned(),
                ),
                None => (
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    "child turn outcome is unknown and its session could not be reopened \
                     to recover it"
                        .to_owned(),
                ),
            };
            if !matches!(
                disposition,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired
            ) && end_attempt_work(&driver, live, &agent_id, &identity)
                .await
                .is_err()
            {
                return AfterTerminal::Settled(());
            }
            // The reopen abandoned what the fenced turn left uncommitted.
            finalize_child_failure(
                &driver,
                &identity,
                &agent_id,
                disposition,
                message,
                FailedTurnWaits::Settle(result),
                observer.as_ref(),
            )
            .await;
            drop(recycling);
            return AfterTerminal::Settled(());
        }
        if let Ok(result) = &terminal
            && let Some(resumed) = try_transient_resume(
                &driver,
                live,
                &agent_id,
                &identity,
                feed.as_ref(),
                result,
                observer.as_ref(),
            )
            .await
        {
            if let Ok(next) = resumed {
                return AfterTerminal::Next(next);
            }
            retire_live_child(&driver, live, &agent_id, completed_turn).await;
            return AfterTerminal::Settled(());
        }
        if let Ok(result) = &terminal
            && matches!(result.outcome(), TurnResultOutcome::Failed(_))
            && end_attempt_work(&driver, live, &agent_id, &identity)
                .await
                .is_err()
        {
            return AfterTerminal::Settled(());
        }
        if let Ok(result) = &terminal
            && finalize_failed_child_result(
                &driver,
                &identity,
                &agent_id,
                feed.as_ref(),
                result,
                observer.as_ref(),
            )
            .await
        {
            retire_live_child(&driver, live, &agent_id, completed_turn).await;
            return AfterTerminal::Settled(());
        }
        let node_status = driver
            .read(|tree| tree.node(&agent_id).ok().map(|node| node.status))
            .ok()
            .flatten();
        let ending = TurnOutcome::classify(node_status, child_turn_was_cancelled(&terminal));
        // Below this point `cancelled` means only "the host fenced the
        // node with `close_agent`"; the other two cancel shapes have their
        // own arms and break out of the loop.
        let cancelled = ending == TurnOutcome::Closed;
        if ending == TurnOutcome::CancelledOutside {
            if end_attempt_work(&driver, live, &agent_id, &identity)
                .await
                .is_err()
            {
                return AfterTerminal::Settled(());
            }
            // end the attempt as an ordinary failure so the
            // parent's `wait_agent` settles, sealing and pushing the
            // `result` by the usual rules.
            if terminal.is_ok() {
                project_turn_end(&driver, feed.as_ref(), &agent_id).await;
            }
            let pending = driver
                .mutate({
                    let (identity, agent_id, driver, observer) = (
                        identity.clone(),
                        agent_id.clone(),
                        driver.clone(),
                        observer.clone(),
                    );
                    move |tree| {
                        let (identity, agent_id, driver, observer) =
                            (&identity, &agent_id, &driver, &observer);
                        if !identity.still_owns(tree, agent_id) {
                            return;
                        }
                        tree.record_fault(
                            agent_id,
                            "the turn was cancelled outside the host; nothing interrupted it",
                        );
                        // the seal is a subprocess and runs with
                        // the lock released.
                        let published = tree.record_host_cancellation(agent_id).and_then(|value| {
                            driver.apply_recorded(tree, agent_id).map(|()| value)
                        });
                        if let Err(error) = published {
                            tree.record_fault(agent_id, error.message);
                            return;
                        }
                        tree.try_emit_tree_node_detached(
                            observer.as_ref(),
                            agent_id,
                            crate::agent_tree::NodeDetach::Failed,
                        );
                    }
                })
                .await
                .map(|((), effects)| effects)
                .unwrap_or_default();
            settle_or_fault(&driver, pending, &agent_id).await;
            retire_live_child(&driver, live, &agent_id, completed_turn).await;
            return AfterTerminal::Settled(());
        }
        if ending == TurnOutcome::Interrupted {
            if terminal.is_ok() {
                project_turn_end(&driver, feed.as_ref(), &agent_id).await;
            }
            finish_interrupted_turn(&driver, live, &agent_id, &identity, observer.as_ref()).await;
            return AfterTerminal::Settled(());
        }
        match terminal {
            Ok(_) if !cancelled => {
                let ended = project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                let (submitted, harness_nudge) = driver
                    .read(|tree| {
                        tree.attempt(&agent_id).map_or(
                            (false, SubmitNudgeBudget::NONE),
                            |assignment| {
                                (
                                    assignment.output_json.is_some(),
                                    assignment.submit_harness_nudge,
                                )
                            },
                        )
                    })
                    .unwrap_or((false, SubmitNudgeBudget::NONE));
                // Spec 03 §5.2: a turn that ends with live work of its own
                // parks instead of being nudged; its wake is a new turn.
                let mut turns_spent = false;
                if !submitted {
                    use crate::agent::park::{ParkEnd, TurnEnd};
                    let end = match crate::agent::park::turn_end(
                        &driver, live, &agent_id, &identity, carried,
                    )
                    .await
                    {
                        TurnEnd::FollowUp(next) => Some(ParkEnd::Woken(next, carried.clone())),
                        TurnEnd::Park => Some(
                            crate::agent::park::park_until_wake(
                                &driver, live, &agent_id, &identity, carried,
                            )
                            .await,
                        ),
                        TurnEnd::Interrupted => Some(ParkEnd::Interrupted),
                        TurnEnd::Settle => None,
                        TurnEnd::Spent => {
                            turns_spent = true;
                            None
                        }
                    };
                    match end {
                        None => {}
                        Some(ParkEnd::Woken(next, arrivals)) => {
                            *carried = arrivals;
                            return AfterTerminal::Next(next);
                        }
                        Some(ParkEnd::Ended) => return AfterTerminal::Settled(()),
                        Some(ParkEnd::Interrupted) => {
                            finish_interrupted_turn(
                                &driver,
                                live,
                                &agent_id,
                                &identity,
                                observer.as_ref(),
                            )
                            .await;
                            return AfterTerminal::Settled(());
                        }
                        Some(ParkEnd::WakeFailed(error)) => {
                            end_failed_wake(
                                &driver,
                                live,
                                &agent_id,
                                &identity,
                                ended.clone(),
                                &error,
                                observer.as_ref(),
                            )
                            .await;
                            return AfterTerminal::Settled(());
                        }
                    }
                }
                if submitted || turns_spent || harness_nudge.is_exhausted() {
                    if end_attempt_work(&driver, live, &agent_id, &identity)
                        .await
                        .is_err()
                    {
                        return AfterTerminal::Settled(());
                    }
                    finalize_child_turn(
                        &driver,
                        &agent_id,
                        Some(&identity),
                        ended.clone(),
                        cancelled,
                        true,
                        observer.as_ref(),
                    )
                    .await;
                    retire_live_child(&driver, live, &agent_id, completed_turn).await;
                    return AfterTerminal::Settled(());
                }
                // `is_exhausted` above is the only exit, so the budget
                // always has a next round here.
                let next_nudge = harness_nudge.next().unwrap_or(harness_nudge);
                if let Ok(next) =
                    start_submit_harness_turn(&driver, live, &agent_id, &identity, next_nudge).await
                {
                    return AfterTerminal::Next(next);
                }
                let reconciliation_required = driver
                    .read(|tree| {
                        tree.attempt(&agent_id).is_some_and(|attempt| {
                            attempt.status
                                == crate::agent_tree_protocol::AttemptStatus::RecoveryRequired
                        })
                    })
                    .unwrap_or(false);
                if reconciliation_required {
                    retire_live_child(&driver, live, &agent_id, completed_turn).await;
                    return AfterTerminal::Settled(());
                }
                let submitted = driver
                    .read(|tree| {
                        tree.attempt(&agent_id)
                            .is_some_and(|assignment| assignment.output_json.is_some())
                    })
                    .unwrap_or(false);
                if end_attempt_work(&driver, live, &agent_id, &identity)
                    .await
                    .is_err()
                {
                    return AfterTerminal::Settled(());
                }
                finalize_child_turn(
                    &driver,
                    &agent_id,
                    Some(&identity),
                    ended.clone(),
                    cancelled,
                    submitted,
                    observer.as_ref(),
                )
                .await;
                retire_live_child(&driver, live, &agent_id, completed_turn).await;
                return AfterTerminal::Settled(());
            }
            Ok(_) => {
                let ended = project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                if end_attempt_work(&driver, live, &agent_id, &identity)
                    .await
                    .is_err()
                {
                    return AfterTerminal::Settled(());
                }
                finalize_child_turn(
                    &driver,
                    &agent_id,
                    Some(&identity),
                    ended.clone(),
                    cancelled,
                    true,
                    observer.as_ref(),
                )
                .await;
                retire_live_child(&driver, live, &agent_id, completed_turn).await;
                return AfterTerminal::Settled(());
            }
            Err(error) => {
                // No authoritative terminal was published. Submitted work
                // cannot turn this channel failure into completion.
                finalize_child_failure(
                    &driver,
                    &identity,
                    &agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    format!(
                        "child terminal publication failed ({:?}): {}",
                        error.kind(),
                        error.message()
                    ),
                    FailedTurnWaits::Keep,
                    observer.as_ref(),
                )
                .await;
                retire_live_child(&driver, live, &agent_id, completed_turn).await;
                return AfterTerminal::Settled(());
            }
        }
    }
}

/// Spec 03 §9: an interrupt cancels this turn and nothing more. The node
/// stays `Interrupted`, the attempt stays `running`, nothing seals and no
/// `result` is pushed. Releasing the bound turn is what makes the node
/// reusable: the next mail (a steer or a coordinate) starts a fresh turn on
/// the same attempt instead of trying to rejoin a turn that has already
/// ended (the cancelled `Ok(result)` fell through to the
/// submit harness, which found no live session and marked the child
/// `recovery_required` with `session_materialization`). An interrupt that
/// lands after the turn ended, at the park decision or during a wake's
/// admission, ends here too.
async fn finish_interrupted_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    finalize_child_turn(
        driver,
        agent_id,
        Some(identity),
        None,
        true,
        false,
        observer,
    )
    .await;
    retire_live_child(driver, live, agent_id, identity.turn).await;
}

/// A parked child's wake was not admitted. The attempt ends as the submit
/// harness would end it, unless an interrupt won the race (the attempt stays
/// open) or the start needs reconciliation. While the attempt ends the child
/// stays marked ending, so mail sent meanwhile is refused as terminal rather
/// than starting a turn on a session whose turn the tree no longer binds.
async fn end_failed_wake(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    terminal: Option<crate::agent_tree::ChildTerminal>,
    error: &str,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    tracing::error!(target: "plak.topology", agent_id, error = %error,
        "parked child wake was not admitted; ending the attempt");
    let unpark = async || {
        let id = agent_id.to_owned();
        mutate_child(driver, agent_id, move |tree| tree.unpark_child(&id)).await;
    };
    let reconciliation_required = driver
        .read(|tree| {
            tree.attempt(agent_id).is_some_and(|attempt| {
                attempt.status == crate::agent_tree_protocol::AttemptStatus::RecoveryRequired
            })
        })
        .unwrap_or(false);
    if reconciliation_required {
        unpark().await;
        retire_live_child(driver, live, agent_id, identity.turn).await;
        return;
    }
    let ending = mutate_child(driver, agent_id, {
        let (identity, agent_id) = (identity.clone(), agent_id.to_owned());
        move |tree| {
            let (identity, agent_id) = (&identity, agent_id.as_str());
            identity.still_owns(tree, agent_id) && tree.mark_park_ending(agent_id)
        }
    })
    .await
    .unwrap_or(false);
    if !ending {
        let interrupted = driver
            .read(|tree| {
                identity.still_owns(tree, agent_id)
                    && tree.node(agent_id).is_ok_and(|node| {
                        node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                    })
            })
            .unwrap_or(false);
        unpark().await;
        if interrupted {
            finish_interrupted_turn(driver, live, agent_id, identity, observer).await;
        } else {
            retire_live_child(driver, live, agent_id, identity.turn).await;
        }
        return;
    }
    mutate_child(driver, agent_id, {
        let (identity, agent_id, driver, error) = (
            identity.clone(),
            agent_id.to_owned(),
            driver.clone(),
            error.to_owned(),
        );
        move |tree| {
            let (identity, agent_id, driver, error) =
                (&identity, agent_id.as_str(), &driver, error.as_str());
            let settled = tree
                .record_failed_wake_mail(agent_id, &identity.turn.to_string(), error)
                .and_then(|()| tree.project_mail_decisions(agent_id));
            if let Err(error) = settled {
                tree.record_fault(agent_id, error.message);
            }
            persist_or_fault(driver, tree, agent_id);
        }
    })
    .await;
    #[cfg(test)]
    crate::agent::park::hooks::before_failed_wake_end(agent_id).await;
    if end_attempt_work(driver, live, agent_id, identity)
        .await
        .is_err()
    {
        unpark().await;
        return;
    }
    finalize_child_turn(
        driver,
        agent_id,
        Some(identity),
        terminal,
        false,
        true,
        observer,
    )
    .await;
    unpark().await;
    retire_live_child(driver, live, agent_id, identity.turn).await;
}

enum FencedChildRecycle {
    Abandoned(plakous::agent::AbandonedTurnReport),
    Quiescent,
    QuiescenceUnproved,
}

/// Reopens a child session whose live owner fenced its turn, so the reopen
/// abandons that turn or closes local finalization debt. `None` is a refusal.
async fn recycle_fenced_child(driver: &TreeDriver, agent_id: &str) -> Option<FencedChildRecycle> {
    let tree_id = driver.read(|tree| tree.tree_id.clone()).ok()?;
    let runtime = crate::topology_runtime::lookup(&tree_id)?;
    let parent = crate::agent::build::rejoin_parent(&runtime)?;
    let (locator, path) = driver
        .read(|tree| {
            tree.node(agent_id).ok().and_then(|node| {
                Some((
                    node.session_locator.clone()?,
                    node.path.display().to_string(),
                ))
            })
        })
        .ok()??;
    let Ok(ChildReopen::Attached { session, recovered }) = reopen_spawned_child(
        &parent,
        agent_id,
        &path,
        &locator,
        "recover",
        ChildReopenMode::OWNER,
    )
    .await
    else {
        return None;
    };
    // A turn the open restored (a hosted continuation) is never resumed by a
    // recycle: it is cancelled within the host's bound before the release.
    if let Some(recovered) = recovered {
        let _ = crate::interrupt::settle_cancel(
            recovered.control().cancel(),
            plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
        )
        .await;
    }
    let report = session
        .agent()
        .and_then(plakous::agent::Agent::abandoned_turn)
        .cloned();
    let recycled = if let Some(report) = report {
        FencedChildRecycle::Abandoned(report)
    } else {
        match session.sessions.head_is_quiescent(session.locator()).await {
            Ok(true) => FencedChildRecycle::Quiescent,
            Ok(false) => FencedChildRecycle::QuiescenceUnproved,
            Err(error) => {
                tracing::error!(target: "plak.topology", agent_id, error = %error,
                    "reopened child quiescence probe failed");
                FencedChildRecycle::QuiescenceUnproved
            }
        }
    };
    let sessions = Arc::clone(&session.sessions);
    let session_id = session.locator().session_id();
    let _ = session.release_opened().await;
    crate::agent::ledger::project_feed(&runtime, agent_id, &sessions, session_id)
        .await
        .ok()?;
    Some(recycled)
}

/// The result channel's success means a terminal was delivered, not that the
/// turn succeeded. Handle failures before submission or host intent can mask them.
///
/// The tree projector records a failed terminal's outcome (spec 03 §14.3);
/// this path keeps only the external effects around it. It stops the
/// attempt's jobs, records the failed-worktree snapshot line as the turn's
/// failed-turn notice before the pass joins it into the failure message,
/// then runs the pass, whose seal and result push settle here.
async fn finalize_failed_child_result(
    driver: &TreeDriver,
    identity: &ChildTurnIdentity,
    agent_id: &str,
    feed: Option<&ChildFeedSource>,
    result: &TurnResult,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) -> bool {
    if !matches!(result.outcome(), TurnResultOutcome::Failed(_)) {
        return false;
    }
    let owned = driver
        .read(|tree| identity.still_owns(tree, agent_id))
        .unwrap_or(false);
    // A refused job stop is recorded as the attempt's own outcome, which the
    // pass below then leaves standing.
    if owned {
        let notice = if driver.stop_agent_jobs(&[agent_id.to_owned()]).await.is_ok() {
            failed_worktree::snapshot_failure(driver, identity, agent_id)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        mutate_child(driver, agent_id, {
            let (identity, agent_id, driver) =
                (identity.clone(), agent_id.to_owned(), driver.clone());
            move |tree| {
                let (identity, agent_id, driver) = (&identity, agent_id.as_str(), &driver);
                if identity.still_owns(tree, agent_id)
                    && tree
                        .record_failed_turn_notice(
                            agent_id,
                            &identity.attempt,
                            &identity.turn.to_string(),
                            notice,
                        )
                        .is_ok()
                {
                    persist_or_fault(driver, tree, agent_id);
                }
            }
        })
        .await;
    }
    project_turn_end(driver, feed, agent_id).await;
    mutate_child(driver, agent_id, {
        let (identity, agent_id, observer) =
            (identity.clone(), agent_id.to_owned(), observer.cloned());
        move |tree| {
            let (identity, agent_id, observer) = (&identity, agent_id.as_str(), observer.as_ref());
            if !identity.still_owns(tree, agent_id) {
                return;
            }
            if let Some(detach) = tree
                .node(agent_id)
                .ok()
                .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
            {
                tree.try_emit_tree_node_detached(observer, agent_id, detach);
            }
        }
    })
    .await;
    true
}

/// How a failed child terminal settles the child's await receipts before
/// any followup can open the next attempt (spec 03 section 4.4).
#[derive(Clone, Copy)]
pub(in crate::agent) enum FailedTurnWaits<'a> {
    /// It did not: each receipt settles by whether this terminal's committed
    /// history holds its call.
    Settle(&'a TurnResult),
    /// No authoritative terminal: child recovery settles them.
    Keep,
}

pub(in crate::agent) async fn finalize_child_failure(
    driver: &TreeDriver,
    identity: &ChildTurnIdentity,
    agent_id: &str,
    disposition: crate::agent_tree::turn_outcome::FailedTurnDisposition,
    mut message: String,
    waits: FailedTurnWaits<'_>,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    if !driver
        .read(|tree| identity.still_owns(tree, agent_id))
        .unwrap_or(false)
    {
        return;
    }
    // Recovery-required turns park their service, but must not park running jobs.
    // Keep cleanup failures authoritative rather than overwriting them below.
    if driver
        .stop_agent_jobs(&[agent_id.to_owned()])
        .await
        .is_err()
    {
        return;
    }
    if let Some(snapshot) = failed_worktree::snapshot_failure(driver, identity, agent_id).await {
        message.push_str(&snapshot);
    }
    let waits = match waits {
        FailedTurnWaits::Settle(terminal) => Some(terminal.clone()),
        FailedTurnWaits::Keep => None,
    };
    let pending = driver
        .mutate({
            let (identity, agent_id, driver) =
                (identity.clone(), agent_id.to_owned(), driver.clone());
            move |tree| {
                let (identity, agent_id, driver) = (&identity, agent_id.as_str(), &driver);
                if !identity.still_owns(tree, agent_id) {
                    return;
                }
                let evidence = match &waits {
                    Some(terminal) => crate::agent::wait_evidence_in(
                        terminal,
                        tree.retained_wait_receipts(agent_id),
                    ),
                    None => Vec::new(),
                };
                if let Err(error) = tree
                    .record_host_turn_failure(agent_id, disposition, message, evidence)
                    .and_then(|()| driver.apply_recorded(tree, agent_id))
                {
                    tree.record_fault(agent_id, error.message);
                }
            }
        })
        .await
        .map(|((), effects)| effects)
        .unwrap_or_default();
    settle_or_fault(driver, pending, agent_id).await;
    mutate_child(driver, agent_id, {
        let (identity, agent_id, observer) =
            (identity.clone(), agent_id.to_owned(), observer.cloned());
        move |tree| {
            let (identity, agent_id, observer) = (&identity, agent_id.as_str(), observer.as_ref());
            if !identity.still_owns(tree, agent_id) {
                return;
            }
            if let Some(detach) = tree
                .node(agent_id)
                .ok()
                .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
            {
                tree.try_emit_tree_node_detached(observer, agent_id, detach);
            }
        }
    })
    .await;
}

/// The child session whose feed a turn waiter projects when a turn ends.
pub(in crate::agent) struct ChildFeedSource {
    pub(super) sessions: Arc<plakous::sessions::AgentSessions>,
    pub(super) session: plakous::model::SessionId,
}

impl ChildFeedSource {
    pub(in crate::agent) fn of(attached: &AttachedCodingSession) -> Self {
        Self {
            sessions: Arc::clone(&attached.sessions),
            session: attached.locator().session_id(),
        }
    }
}

/// Applies the child's feed to its tail when a turn ends (spec 03 §14.3):
/// the terminal's usage folds onto the attempt its command served, as every
/// later pass would derive it. Per-attempt only; cost, quota balances and
/// subtree rollups never reach a model-visible row. A refused read or page
/// is the node's fault; the next pass applies the same records.
async fn project_turn_end(
    driver: &TreeDriver,
    feed: Option<&ChildFeedSource>,
    agent_id: &str,
) -> Option<crate::agent_tree::ChildTerminal> {
    let source = feed?;
    let key = source.session.to_string();
    let cursor = driver.read(|tree| tree.feed_read_cursor(&key)).ok()?;
    let applied = match crate::agent::read_tree_feed(&source.sessions, source.session, cursor).await
    {
        Ok(feed) => {
            crate::agent::feed_projection::retry_tree_feed(&source.sessions, feed, |feed| {
                crate::agent::feed_projection::apply_on_driver(driver, feed, agent_id)
            })
            .await
            .map(|(projection, _)| projection)
            .map_err(|error| error.message)
        }
        Err(error) => Err(String::from(error)),
    };
    let terminal = mutate_child(driver, agent_id, {
        let agent_id = agent_id.to_owned();
        move |tree| {
            let agent_id = agent_id.as_str();
            match applied {
                Ok(projection) => Ok(projection
                    .ended_turns
                    .into_iter()
                    .filter(|(_, terminal)| {
                        tree.attempt(agent_id)
                            .is_some_and(|attempt| attempt.attempt_id == terminal.attempt_id)
                    })
                    .map(|(_, terminal)| terminal)
                    .next_back()),
                Err(error) => {
                    tree.record_fault(
                        agent_id,
                        format!("the child's turn end was not projected: {error}"),
                    );
                    Err(())
                }
            }
        }
    })
    .await?
    .ok()?;
    if terminal.is_some() {
        return terminal;
    }
    // A live tick may already have advanced the cursor past this terminal.
    // Reread only the exact accepted command's bounded evidence, not history.
    match read_finalize_terminal(driver, &source.sessions, source.session, agent_id).await {
        Ok(terminal) => Some(terminal),
        Err(error) => {
            mutate_child(driver, agent_id, {
                let agent_id = agent_id.to_owned();
                move |tree| {
                    let agent_id = agent_id.as_str();
                    tree.record_fault(agent_id, error);
                }
            })
            .await;
            None
        }
    }
}

/// Reads only the latest accepted command of this attempt for a late Finalize.
/// The returned terminal belongs to the caller; it is never cached in the tree.
pub(in crate::agent) async fn read_finalize_terminal(
    driver: &TreeDriver,
    sessions: &plakous::sessions::AgentSessions,
    session_id: plakous::model::SessionId,
    agent_id: &str,
) -> Result<crate::agent_tree::ChildTerminal, String> {
    let (attempt_id, turn, accepted_seq) = driver
        .read(|tree| {
            let attempt = tree.attempt(agent_id)?;
            let command = tree
                .commands()
                .values()
                .filter(|command| {
                    command.recipient == agent_id
                        && command.attempt_id.as_ref() == Some(&attempt.attempt_id)
                        && command.turn_id.is_some()
                        && command.accepted_seq.is_some()
                })
                .max_by_key(|command| command.accepted_seq)?;
            Some((
                attempt.attempt_id.clone(),
                command.turn_id.clone()?,
                command.accepted_seq?,
            ))
        })
        .map_err(|error| error.message)?
        .ok_or_else(|| "child finalization has no accepted command evidence".to_owned())?;
    let feed = crate::agent::read_tree_feed(sessions, session_id, accepted_seq).await?;
    driver
        .read(|tree| {
            feed.records
                .iter()
                .flat_map(|record| tree.child_terminal_evidence(agent_id, record))
                .find(|terminal| terminal.turn_id == turn && terminal.attempt_id == attempt_id)
        })
        .map_err(|error| error.message)?
        .ok_or_else(|| "child finalization's exact committed terminal is unavailable".to_owned())
}

/// Whether a child turn ended by cancellation. The runtime reports a joined
/// cancel as `Ok(TurnResult)` with a `Cancelled` outcome — a normal terminal,
/// not an error. Failure to publish terminal authority requires recovery.
pub(in crate::agent) fn child_turn_was_cancelled(
    terminal: &Result<Arc<TurnResult>, plakous::agent::AgentError>,
) -> bool {
    match terminal {
        Ok(result) => matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
        Err(_) => false,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub async fn finalize_topology_child_turn(
    driver: &TreeDriver,
    agent_id: &str,
    cancelled: bool,
    terminal_ok: bool,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    finalize_child_turn(
        driver,
        agent_id,
        None,
        None,
        cancelled,
        terminal_ok,
        observer,
    )
    .await;
}

#[allow(
    clippy::too_many_lines,
    reason = "Child finalization reconciles canonical outcome and tree attempt state before publishing the result and releasing its runtime."
)]
pub(super) async fn finalize_child_turn(
    driver: &TreeDriver,
    agent_id: &str,
    identity: Option<&ChildTurnIdentity>,
    terminal: Option<crate::agent_tree::ChildTerminal>,
    cancelled: bool,
    terminal_ok: bool,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    // the seal's snapshot is a VCS subprocess and MUST NOT run
    // under the tree mutex. Phase one marks the node `Sealing` and queues the
    // ticket; phase two runs the snapshot with the lock dropped; phase three
    // re-locks to record the candidate.
    let pending = driver
        .mutate({
            let (identity, agent_id, driver, terminal) = (
                identity.cloned(),
                agent_id.to_owned(),
                driver.clone(),
                terminal.clone(),
            );
            move |tree| {
                let (identity, agent_id, driver, terminal) =
                    (identity.as_ref(), agent_id.as_str(), &driver, &terminal);
                if identity.is_some_and(|identity| !identity.still_owns(tree, agent_id)) {
                    return;
                }
                if cancelled
                    && tree.node(agent_id).is_ok_and(|node| {
                        node.status != crate::agent_tree_protocol::NodeStatus::Closed
                    })
                {
                    tree.release_turn_owner(agent_id);
                    persist_or_fault(driver, tree, agent_id);
                    return;
                }
                if !terminal_ok && !cancelled {
                    let recorded = tree.record_host_turn_failure(
                        agent_id,
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                        "child session materialization did not complete".to_owned(),
                        Vec::new(),
                    );
                    if recorded.is_ok() && persist_or_fault(driver, tree, agent_id) {
                        tree.release_turn_owner(agent_id);
                        if let Err(error) =
                            driver.fold_and_commit(tree, |tree| tree.fold_due(agent_id).map(|_| ()))
                        {
                            tree.record_fault(agent_id, error.message);
                        }
                    }
                    return;
                }
                let turn = identity
                    .map(|identity| identity.turn.to_string())
                    .or_else(|| terminal.as_ref().map(|terminal| terminal.turn_id.clone()));
                if let Some(turn) = turn {
                    if tree
                        .record_child_finalization(
                            agent_id,
                            &turn,
                            crate::agent_tree::ChildFinalizationDecision::Finalize,
                            true,
                            terminal.clone(),
                        )
                        .is_err()
                        || !persist_or_fault(driver, tree, agent_id)
                    {
                        return;
                    }
                    tree.release_turn_owner(agent_id);
                    if let Err(error) =
                        driver.fold_and_commit(tree, |tree| tree.fold_due(agent_id).map(|_| ()))
                    {
                        tree.record_fault(agent_id, error.message);
                    }
                    return;
                }
                // Topology-only fixtures have no runtime feed. Production never
                // substitutes a live observation for committed terminal evidence.
                #[cfg(any(test, feature = "test-support"))]
                if identity.is_none() {
                    tree.release_turn_owner(agent_id);
                    if terminal_ok {
                        let _ = tree.record_turn_completed(agent_id);
                    } else if !cancelled {
                        let _ = tree
                            .record_host_materialization(agent_id)
                            .and_then(|value| {
                                driver.apply_recorded(tree, agent_id).map(|()| value)
                            });
                    }
                    if cancelled
                        && tree.node(agent_id).is_ok_and(|node| {
                            node.status == crate::agent_tree_protocol::NodeStatus::Closed
                        })
                    {
                        tree.begin_seal(agent_id);
                    }
                }
                persist_or_fault(driver, tree, agent_id);
            }
        })
        .await
        .map(|((), effects)| effects)
        .unwrap_or_default();
    settle_or_fault(driver, pending, agent_id).await;
    mutate_child(driver, agent_id, {
        let (identity, agent_id, observer) =
            (identity.cloned(), agent_id.to_owned(), observer.cloned());
        move |tree| {
            let (identity, agent_id, observer) =
                (identity.as_ref(), agent_id.as_str(), observer.as_ref());
            if identity.is_some_and(|identity| !identity.still_owns(tree, agent_id)) {
                return;
            }
            if cancelled {
                let skip_emit = tree
                    .node(agent_id)
                    .is_ok_and(|node| !node.status.is_terminal());
                if skip_emit {
                    return;
                }
            }
            let status = tree
                .node(agent_id)
                .ok()
                .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
                .unwrap_or(if terminal_ok {
                    crate::agent_tree::NodeDetach::Completed
                } else {
                    crate::agent_tree::NodeDetach::Failed
                });
            tree.try_emit_tree_node_detached(observer, agent_id, status);
        }
    })
    .await;
}

pub(super) async fn start_submit_harness_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    nudge: SubmitNudgeBudget,
) -> Result<TurnResultHandle, String> {
    let (prompt, command_id) = driver
        .read(|tree| {
            let node = tree.node(agent_id).map_err(|error| error.message)?;
            let attempt = tree
                .attempt(agent_id)
                .ok_or_else(|| "attempt is missing".to_owned())?;
            if attempt.output_json.is_some() || attempt.status.is_stopped() {
                return Err("attempt is already submitted or terminal".into());
            }
            let prompt =
                submit_harness_prompt(nudge.rounds(), attempt.turn_token, &node.output_schema_json);
            let command_id = CommandId::new(format!(
                "plak:submit-harness:{agent_id}:{}:{}:{}",
                tree.host_generation,
                attempt.attempt_id,
                nudge.rounds()
            ))
            .map_err(|error| error.to_string())?;
            Ok::<_, String>((prompt, command_id))
        })
        .map_err(|error| error.message)??;
    let (decision, effects) = driver
        .mutate({
            let (driver, agent_id, identity) =
                (driver.clone(), agent_id.to_owned(), identity.clone());
            move |tree| {
                let (driver, agent_id, identity) = (&driver, agent_id.as_str(), &identity);
                tree.record_child_finalization(
                    agent_id,
                    &identity.turn.to_string(),
                    crate::agent_tree::ChildFinalizationDecision::Nudge,
                    false,
                    None,
                )
                .map_err(|error| error.message)?;
                if !persist_or_fault(driver, tree, agent_id) {
                    return Err("child nudge decision could not be saved".to_owned());
                }
                Ok(())
            }
        })
        .await
        .map_err(|error| error.message)?;
    driver
        .settle(effects)
        .await
        .map_err(|error| error.message)?;
    decision?;
    start_instruction_turn(
        driver,
        live,
        agent_id,
        identity,
        command_id,
        prompt,
        CommandKind::Corrective,
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "Instruction-turn start preserves pending delivery identity, admission acknowledgement, and runtime release when the attempt cannot bind."
)]
pub(in crate::agent) async fn start_instruction_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    command_id: CommandId,
    prompt: String,
    kind: CommandKind,
) -> Result<TurnResultHandle, String> {
    let command_text = command_id.as_str().to_owned();
    let transient = kind == CommandKind::Resume;
    let result_context = crate::agent::ledger::ChildLedger::new(driver, agent_id)
        .prepare(CommandIntent {
            command_id: &command_text,
            kind,
            base_text: &prompt,
            batch: if transient {
                BatchRule::WakeAll
            } else {
                BatchRule::Results
            },
        })
        .await
        .map_err(String::from)?;
    let (mut live_map, delivery) =
        crate::agent::live::lock_for_transition(live, agent_id, identity.turn).await?;
    let admissible = if transient {
        resume_allowed(driver, agent_id, identity)
    } else {
        driver
            .read(|tree| {
                identity.still_owns(tree, agent_id)
                    && tree.node(agent_id).is_ok_and(|node| {
                        node.status == crate::agent_tree_protocol::NodeStatus::Running
                    })
                    && tree.attempt(agent_id).is_some_and(|attempt| {
                        !attempt.status.is_stopped() && attempt.output_json.is_none()
                    })
            })
            .unwrap_or(false)
    };
    if !admissible {
        crate::agent::ledger::ChildLedger::settlement_only(driver)
            .settle(
                &command_text,
                crate::driver::ledger::Settlement::Refused { pause: None },
            )
            .await
            .map_err(String::from)?;
        return Err(if transient {
            "transient continuation was cancelled"
        } else {
            "child instruction was cancelled"
        }
        .to_owned());
    }
    let child = live_map
        .get_mut(agent_id)
        .ok_or_else(|| "child live session is missing".to_owned())?;
    if child.turn_id() != identity.turn {
        return Err("child turn has already been replaced".into());
    }
    let result = match child
        .restart(
            driver.clone(),
            command_id,
            result_context.prompt.clone(),
            delivery,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            drop(live_map);
            if error == crate::agent::live::RESTART_RECONCILIATION_REQUIRED {
                finalize_child_failure(
                    driver,
                    identity,
                    agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    error.clone(),
                    FailedTurnWaits::Keep,
                    None,
                )
                .await;
            }
            return Err(error);
        }
    };
    let turn_id = child.turn_id();
    let cursor = crate::agent::live::live_feed_cursor(driver, agent_id)?;
    let read = child.feed(cursor);
    drop(live_map);
    let feed = read.await;
    let (published, effects) = driver
        .mutate({
            let (driver, agent_id, identity) =
                (driver.clone(), agent_id.to_owned(), identity.clone());
            move |tree| {
                let (driver, agent_id, identity) = (&driver, agent_id.as_str(), &identity);
                if !identity.still_owns(tree, agent_id) {
                    return Err("child attempt has already been replaced".into());
                }
                if transient {
                    tree.unpark_child(agent_id);
                }
                tree.bind_turn_owner(agent_id, turn_id.to_string());
                if let Some(feed) = feed? {
                    feed.apply(tree, agent_id).map_err(|error| error.message)?;
                }
                // Once Start was admitted the result must retain a waiter even
                // when persistence refuses this publication. The fault remains
                // visible and the acknowledged revision is not advanced.
                persist_or_fault(driver, tree, agent_id);
                Ok::<_, String>(())
            }
        })
        .await
        .unwrap_or_else(|error| (Err(error.message), crate::agent_tree::Effects::default()));
    if let Err(error) = published {
        settle_or_fault(driver, effects, agent_id).await;
        if let Err(retirement_error) =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await
        {
            return Err(format!("{error}; {retirement_error}"));
        }
        return Err(error);
    }
    if let Err(error) = crate::agent::ledger::ChildLedger::settlement_only(driver)
        .settle(&command_text, crate::driver::ledger::Settlement::Recorded)
        .await
        .map_err(String::from)
    {
        settle_or_fault(driver, effects, agent_id).await;
        if let Err(retirement_error) =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await
        {
            return Err(format!("{error}; {retirement_error}"));
        }
        return Err(error);
    }
    settle_or_fault(driver, effects, agent_id).await;
    Ok(result)
}
