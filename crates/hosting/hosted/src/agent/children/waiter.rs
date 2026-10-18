//! The turn waiter: what runs when a child turn ends, and resuming it.

use super::{
    Arc, AttachedCodingSession, ChildReopen, ChildReopenMode, ChildStartup, ChildTurnIdentity,
    CommandId, Duration, StartAdmission, SubmitNudgeBudget, TreeDriver, TurnId, TurnOutcome,
    TurnResult, TurnResultHandle, TurnResultOutcome, end_attempt_work, failed_worktree,
    mutate_child, persist_or_fault, reopen_spawned_child, retire_live_child, settle_or_fault,
    submit_harness_prompt,
};

/// A turn waiter's registration on the tree. It is taken before the turn
/// it watches is bound or occupies the live slot, so a bound turn some
/// waiter will release always reads as watched; the waiter task holds it for
/// its life however it ends (a panic or an aborted task included), and a turn
/// that never gets its waiter drops it. A send tells a bound turn its waiter
/// will still release from one nobody watches by it (spec 03 section 9).
///
/// Dropping it takes the tree lock: never drop one while holding that lock.
pub(in crate::agent) struct TurnWaiterRegistration {
    pub(super) driver: TreeDriver,
    pub(super) agent_id: String,
    pub(super) attempt: crate::agent_tree_protocol::AttemptId,
}

pub(super) fn register_admitted_turn(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    admission: StartAdmission,
    agent_id: &str,
    turn_id: TurnId,
    registration: &mut Option<TurnWaiterRegistration>,
) -> Result<(), String> {
    runtime.prepare_then_emit(|tree| {
        admission.check(tree, agent_id)?;
        tree.bind_turn_owner(agent_id, turn_id.to_string());
        *registration = TurnWaiterRegistration::enter(runtime.driver(), tree, agent_id);
        if admission == StartAdmission::ReopenWake {
            tree.unpark_child(agent_id);
        }
        Ok(())
    })
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
        })
    }

    /// [`Self::enter`] under its own tree lock.
    pub(in crate::agent) fn register(driver: &TreeDriver, agent_id: &str) -> Option<Self> {
        let (registration, effects) = driver
            .mutate(|tree| Self::enter(driver, tree, agent_id))
            .ok()?;
        settle_or_fault(driver, effects, agent_id);
        registration
    }
}

impl Drop for TurnWaiterRegistration {
    fn drop(&mut self) {
        let left = self
            .driver
            .mutate(|tree| tree.leave_turn_waiter(&self.agent_id));
        if let Ok(((), effects)) = left {
            settle_or_fault(&self.driver, effects, &self.agent_id);
        }
    }
}

/// The waiter owns the driver it was registered with: its saves and settles
/// go to the store the hub bound, whether or not the hub still lists this
/// tree when the turn ends (a root that quit releases the hub entry while a
/// child turn can still be unwinding).
pub(in crate::agent) fn spawn_topology_turn_waiter(
    registration: TurnWaiterRegistration,
    live: Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    observer: Option<Arc<dyn crate::agent_tree::TopologyObserver>>,
    feed: Option<ChildFeedSource>,
    result: TurnResultHandle,
) {
    let driver = registration.driver.clone();
    let agent_id = registration.agent_id.clone();
    let attempt = registration.attempt.clone();
    tokio::spawn(async move {
        let _registration = registration;
        let mut pending = Some(result);
        // What the last wake carried, so leftovers cannot wake forever.
        let mut carried = crate::agent::park::Arrivals::new();
        loop {
            let Some(handle) = pending.take() else {
                break;
            };
            let completed_turn = handle.turn_id();
            let identity = ChildTurnIdentity {
                attempt: attempt.clone(),
                turn: completed_turn,
            };
            let terminal = handle.result().await;
            // A child has no operator to wait for, so a turn its owner fenced
            // is recovered at once: release the stopped session and reopen it,
            // which abandons the turn exactly as a crash reattach would. The
            // attempt then ends as an ordinary failure the parent can re-task.
            if let Ok(result) = &terminal
                && matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_))
            {
                project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                if driver
                    .read(|tree| identity.still_owns(tree, &agent_id))
                    .unwrap_or(false)
                    && driver
                        .stop_agent_jobs(std::slice::from_ref(&agent_id))
                        .await
                        .is_err()
                {
                    retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                    break;
                }
                // The recycle may wait out the fenced owner's lease, so it
                // never runs under the reattach lock. Under that lock the
                // node is marked recycling before its live entry goes, which a
                // `recover_agent` or a recovery pass treats as a live owner;
                // the lock is retaken to record the outcome and clear the
                // mark, so recovery either settled the attempt first (and
                // this failure report then changes nothing) or starts after
                // it is recorded.
                let runtime = driver
                    .read(|tree| tree.tree_id.clone())
                    .ok()
                    .and_then(|tree_id| crate::topology_runtime::lookup(&tree_id));
                let recycling = match runtime.as_ref() {
                    Some(runtime) => {
                        let _reattach = runtime.lock_reattach().await;
                        Some(runtime.mark_recycling(&agent_id))
                    }
                    None => None,
                };
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                let recycled = recycle_fenced_child(&driver, &agent_id).await;
                let reattach = match runtime.as_ref() {
                    Some(runtime) => Some(runtime.lock_reattach().await),
                    None => None,
                };
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
                ) && end_attempt_work(&driver, &live, &agent_id, &identity)
                    .await
                    .is_err()
                {
                    break;
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
                // Cleared while the reattach lock is still held.
                drop(recycling);
                drop(reattach);
                break;
            }
            if let Ok(result) = &terminal
                && let Some(resumed) = try_transient_resume(
                    &driver,
                    &live,
                    &agent_id,
                    &identity,
                    feed.as_ref(),
                    result,
                    observer.as_ref(),
                )
                .await
            {
                if let Ok(next) = resumed {
                    pending = Some(next);
                    continue;
                }
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                break;
            }
            if let Ok(result) = &terminal
                && matches!(result.outcome(), TurnResultOutcome::Failed(_))
                && end_attempt_work(&driver, &live, &agent_id, &identity)
                    .await
                    .is_err()
            {
                break;
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
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                break;
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
                if end_attempt_work(&driver, &live, &agent_id, &identity)
                    .await
                    .is_err()
                {
                    break;
                }
                // end the attempt as an ordinary failure so the
                // parent's `wait_agent` settles, sealing and pushing the
                // `result` by the usual rules.
                if terminal.is_ok() {
                    project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                }
                let pending = driver
                    .mutate(|tree| {
                        if !identity.still_owns(tree, &agent_id) {
                            return;
                        }
                        tree.record_fault(
                            &agent_id,
                            "the turn was cancelled outside the host; nothing interrupted it",
                        );
                        // the seal is a subprocess and runs with
                        // the lock released.
                        let published =
                            tree.record_host_cancellation(&agent_id).and_then(|value| {
                                driver.apply_recorded(tree, &agent_id).map(|()| value)
                            });
                        if let Err(error) = published {
                            tree.record_fault(&agent_id, error.message);
                            return;
                        }
                        tree.try_emit_tree_node_detached(
                            observer.as_ref(),
                            &agent_id,
                            crate::agent_tree::NodeDetach::Failed,
                        );
                    })
                    .map(|((), effects)| effects)
                    .unwrap_or_default();
                settle_or_fault(&driver, pending, &agent_id);
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                break;
            }
            if ending == TurnOutcome::Interrupted {
                if terminal.is_ok() {
                    project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                }
                finish_interrupted_turn(&driver, &live, &agent_id, &identity, observer.as_ref())
                    .await;
                break;
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
                            &driver, &live, &agent_id, &identity, &carried,
                        )
                        .await
                        {
                            TurnEnd::FollowUp(next) => Some(ParkEnd::Woken(next, carried.clone())),
                            TurnEnd::Park => Some(
                                crate::agent::park::park_until_wake(
                                    &driver, &live, &agent_id, &identity, &carried,
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
                                carried = arrivals;
                                pending = Some(next);
                                continue;
                            }
                            Some(ParkEnd::Ended) => break,
                            Some(ParkEnd::Interrupted) => {
                                finish_interrupted_turn(
                                    &driver,
                                    &live,
                                    &agent_id,
                                    &identity,
                                    observer.as_ref(),
                                )
                                .await;
                                break;
                            }
                            Some(ParkEnd::WakeFailed(error)) => {
                                end_failed_wake(
                                    &driver,
                                    &live,
                                    &agent_id,
                                    &identity,
                                    ended.clone(),
                                    &error,
                                    observer.as_ref(),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    if submitted || turns_spent || harness_nudge.is_exhausted() {
                        if end_attempt_work(&driver, &live, &agent_id, &identity)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        finalize_child_turn(
                            &driver,
                            &agent_id,
                            Some(&identity),
                            ended.clone(),
                            cancelled,
                            true,
                            observer.as_ref(),
                        );
                        retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                        break;
                    }
                    // `is_exhausted` above is the only exit, so the budget
                    // always has a next round here.
                    let next_nudge = harness_nudge.next().unwrap_or(harness_nudge);
                    if let Ok(next) =
                        start_submit_harness_turn(&driver, &live, &agent_id, &identity, next_nudge)
                            .await
                    {
                        pending = Some(next);
                    } else {
                        let reconciliation_required = driver.read(|tree| {
                            tree.attempt(&agent_id).is_some_and(|attempt|
                                attempt.status == crate::agent_tree_protocol::AttemptStatus::RecoveryRequired)
                        }).unwrap_or(false);
                        if reconciliation_required {
                            retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                            break;
                        }
                        let submitted = driver
                            .read(|tree| {
                                tree.attempt(&agent_id)
                                    .is_some_and(|assignment| assignment.output_json.is_some())
                            })
                            .unwrap_or(false);
                        if end_attempt_work(&driver, &live, &agent_id, &identity)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        finalize_child_turn(
                            &driver,
                            &agent_id,
                            Some(&identity),
                            ended.clone(),
                            cancelled,
                            submitted,
                            observer.as_ref(),
                        );
                        retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                        break;
                    }
                }
                Ok(_) => {
                    let ended = project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                    if end_attempt_work(&driver, &live, &agent_id, &identity)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    finalize_child_turn(
                        &driver,
                        &agent_id,
                        Some(&identity),
                        ended.clone(),
                        cancelled,
                        true,
                        observer.as_ref(),
                    );
                    retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                    break;
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
                    retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                    break;
                }
            }
        }
    });
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
    );
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
    let unpark = || {
        mutate_child(driver, agent_id, |tree| tree.unpark_child(agent_id));
    };
    let reconciliation_required = driver
        .read(|tree| {
            tree.attempt(agent_id).is_some_and(|attempt| {
                attempt.status == crate::agent_tree_protocol::AttemptStatus::RecoveryRequired
            })
        })
        .unwrap_or(false);
    if reconciliation_required {
        unpark();
        retire_live_child(driver, live, agent_id, identity.turn).await;
        return;
    }
    let ending = mutate_child(driver, agent_id, |tree| {
        identity.still_owns(tree, agent_id) && tree.mark_park_ending(agent_id)
    })
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
        unpark();
        if interrupted {
            finish_interrupted_turn(driver, live, agent_id, identity, observer).await;
        } else {
            retire_live_child(driver, live, agent_id, identity.turn).await;
        }
        return;
    }
    mutate_child(driver, agent_id, |tree| {
        let settled = tree
            .record_failed_wake_mail(agent_id, &identity.turn.to_string(), error)
            .and_then(|()| tree.project_mail_decisions(agent_id));
        if let Err(error) = settled {
            tree.record_fault(agent_id, error.message);
        }
        persist_or_fault(driver, tree, agent_id);
    });
    #[cfg(test)]
    crate::agent::park::hooks::before_failed_wake_end(agent_id).await;
    if end_attempt_work(driver, live, agent_id, identity)
        .await
        .is_err()
    {
        unpark();
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
    );
    unpark();
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
    let Ok(ChildReopen::Attached { session, .. }) = reopen_spawned_child(
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
    crate::agent::result_context::project_feed(&runtime, agent_id, &sessions, session_id)
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
        mutate_child(driver, agent_id, |tree| {
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
        });
    }
    project_turn_end(driver, feed, agent_id).await;
    mutate_child(driver, agent_id, |tree| {
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
    });
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
    let pending = driver
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return;
            }
            let evidence = match waits {
                FailedTurnWaits::Settle(terminal) => {
                    crate::agent::wait_evidence_in(terminal, tree.retained_wait_receipts(agent_id))
                }
                FailedTurnWaits::Keep => Vec::new(),
            };
            if let Err(error) = tree
                .record_host_turn_failure(agent_id, disposition, message, evidence)
                .and_then(|()| driver.apply_recorded(tree, agent_id))
            {
                tree.record_fault(agent_id, error.message);
            }
        })
        .map(|((), effects)| effects)
        .unwrap_or_default();
    settle_or_fault(driver, pending, agent_id);
    mutate_child(driver, agent_id, |tree| {
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
    });
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
    let terminal = mutate_child(driver, agent_id, |tree| match applied {
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
    })?
    .ok()?;
    if terminal.is_some() {
        return terminal;
    }
    // A live tick may already have advanced the cursor past this terminal.
    // Reread only the exact accepted command's bounded evidence, not history.
    match read_finalize_terminal(driver, &source.sessions, source.session, agent_id).await {
        Ok(terminal) => Some(terminal),
        Err(error) => {
            mutate_child(driver, agent_id, |tree| tree.record_fault(agent_id, error));
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
pub fn finalize_topology_child_turn(
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
    );
}

pub(super) fn finalize_child_turn(
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
        .mutate(|tree| {
            if identity.is_some_and(|identity| !identity.still_owns(tree, agent_id)) {
                return;
            }
            if cancelled
                && tree
                    .node(agent_id)
                    .is_ok_and(|node| node.status != crate::agent_tree_protocol::NodeStatus::Closed)
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
                        .and_then(|value| driver.apply_recorded(tree, agent_id).map(|()| value));
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
        })
        .map(|((), effects)| effects)
        .unwrap_or_default();
    settle_or_fault(driver, pending, agent_id);
    mutate_child(driver, agent_id, |tree| {
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
    });
}

fn resume_allowed_in(
    tree: &crate::agent_tree::ProductAgentTree,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> bool {
    identity.still_owns(tree, agent_id)
        && tree
            .node(agent_id)
            .is_ok_and(|node| node.status == crate::agent_tree_protocol::NodeStatus::Running)
        && tree
            .attempt(agent_id)
            .is_some_and(|attempt| !attempt.status.is_stopped() && attempt.output_json.is_none())
        && !tree.attempt_turns_spent(agent_id)
}

fn resume_allowed(driver: &TreeDriver, agent_id: &str, identity: &ChildTurnIdentity) -> bool {
    driver
        .read(|tree| resume_allowed_in(tree, agent_id, identity))
        .unwrap_or(false)
}

pub(super) async fn wait_transient_gate(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    gate: crate::agent::transient::ResumeGate,
) -> bool {
    let probe = live
        .lock()
        .await
        .get(agent_id)
        .and_then(crate::topology_runtime::LiveChildHandle::reachability_probe);
    wait_resume_gate(driver, live.into(), None, agent_id, identity, probe, gate).await
}

pub(in crate::agent) async fn wait_resume_gate(
    driver: &TreeDriver,
    live: Option<&Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>>,
    startup: Option<&ChildStartup>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    probe: Option<plakous::model::ReachabilityProbe>,
    gate: crate::agent::transient::ResumeGate,
) -> bool {
    let cancellation = plakous::model::CancellationToken::new();
    let (probe, delay) = match gate {
        crate::agent::transient::ResumeGate::Network => (probe, Duration::from_secs(30)),
        crate::agent::transient::ResumeGate::Delay(delay) => (None, delay),
    };
    let ready =
        crate::runtime::network_watch::NetworkWatch::shared().wait(probe, delay, &cancellation);
    tokio::pin!(ready);
    loop {
        let Ok(notify) = driver.read(crate::agent_tree::ProductAgentTree::waiters) else {
            return false;
        };
        let notified = notify.notified_owned();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !resume_allowed(driver, agent_id, identity)
            || startup.is_some_and(ChildStartup::is_cancelled)
            || if let Some(live) = live {
                live.lock()
                    .await
                    .get(agent_id)
                    .is_none_or(|child| child.turn_id() != identity.turn)
            } else {
                false
            }
        {
            cancellation.cancel();
            return false;
        }
        tokio::select! {
            biased;
            () = &mut notified => {}
            value = &mut ready => return value && resume_allowed(driver, agent_id, identity),
            // A hub release may drain the live slot without a tree mutation.
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn try_transient_resume(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    feed: Option<&ChildFeedSource>,
    result: &TurnResult,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) -> Option<Result<TurnResultHandle, ()>> {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return None;
    };
    let model = error.model_error()?;
    let context = model.context()?;
    let gate =
        crate::agent::transient::classify(model.kind(), context.phase(), context.retryability())?;
    if !resume_allowed(driver, agent_id, identity) {
        return None;
    }
    let round = driver
        .read(|tree| {
            let spent = tree
                .transient_resume_count(agent_id, &identity.attempt)
                .ok()?;
            (spent < tree.snapshot.transient_resumes).then_some(spent + 1)
        })
        .ok()
        .flatten()?;
    let notice = failed_worktree::snapshot_failure(driver, identity, agent_id)
        .await
        .unwrap_or_default();
    let (saved, effects) = driver
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return Err("attempt changed".to_owned());
            }
            tree.record_child_finalization(
                agent_id,
                &identity.turn.to_string(),
                crate::agent_tree::ChildFinalizationDecision::Resume { round },
                false,
                None,
            )
            .map_err(|error| error.message)?;
            // The notice is the projector's join barrier: never release it before
            // the immutable continuation decision is durably saved.
            tree.record_failed_turn_notice(
                agent_id,
                &identity.attempt,
                &identity.turn.to_string(),
                notice,
            )
            .map_err(|error| error.message)?;
            if !persist_or_fault(driver, tree, agent_id) {
                return Err("continuation decision could not be saved".to_owned());
            }
            Ok(())
        })
        .ok()?;
    settle_or_fault(driver, effects, agent_id);
    if saved.is_err() {
        return Some(Err(()));
    }
    project_turn_end(driver, feed, agent_id).await;
    mutate_child(driver, agent_id, |tree| {
        if resume_allowed_in(tree, agent_id, identity) {
            let _ = tree.park_child(agent_id, Vec::new());
            if gate == crate::agent::transient::ResumeGate::Network {
                tree.mark_transient_wait(agent_id, true);
            }
        }
    });
    if !wait_transient_gate(driver, live, agent_id, identity, gate).await {
        mutate_child(driver, agent_id, |tree| tree.unpark_child(agent_id));
        return Some(Err(()));
    }
    let command_id = CommandId::new(format!(
        "{}{}:{}:{round}",
        crate::agent::result_context::TRANSIENT_RESUME_PREFIX,
        agent_id,
        identity.attempt
    ))
    .ok()?;
    let prompt = crate::child_prompts::TransientResume {
        failure: error.message(),
    }
    .render();
    if let Ok(next) =
        start_instruction_turn(driver, live, agent_id, identity, command_id, prompt).await
    {
        Some(Ok(next))
    } else {
        if resume_allowed(driver, agent_id, identity) {
            let _ = end_attempt_work(driver, live, agent_id, identity).await;
            finalize_child_failure(
                driver,
                identity,
                agent_id,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                error.message().to_owned(),
                FailedTurnWaits::Settle(result),
                observer,
            )
            .await;
        }
        Some(Err(()))
    }
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
        .mutate(|tree| {
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
        })
        .map_err(|error| error.message)?;
    driver.settle(effects).map_err(|error| error.message)?;
    decision?;
    start_instruction_turn(driver, live, agent_id, identity, command_id, prompt).await
}

async fn start_instruction_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    command_id: CommandId,
    prompt: String,
) -> Result<TurnResultHandle, String> {
    let command_text = command_id.as_str().to_owned();
    let result_context =
        crate::agent::result_context::prepare(driver, agent_id, &command_text, &prompt)?;
    let mut live_map = live.lock().await;
    if command_text.starts_with(crate::agent::result_context::TRANSIENT_RESUME_PREFIX)
        && !resume_allowed(driver, agent_id, identity)
    {
        crate::agent::result_context::acknowledge(
            driver,
            &command_text,
            crate::agent::result_context::Submission::Refused,
        )?;
        return Err("transient continuation was cancelled".to_owned());
    }
    let child = live_map
        .get_mut(agent_id)
        .ok_or_else(|| "child live session is missing".to_owned())?;
    if child.turn_id() != identity.turn {
        return Err("child turn has already been replaced".into());
    }
    let result = match child
        .restart(driver.clone(), command_id, result_context.prompt.clone())
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
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return Err("child attempt has already been replaced".into());
            }
            if command_text.starts_with(crate::agent::result_context::TRANSIENT_RESUME_PREFIX) {
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
        })
        .unwrap_or_else(|error| (Err(error.message), crate::agent_tree::Effects::default()));
    if let Err(error) = published {
        settle_or_fault(driver, effects, agent_id);
        if let Err(retirement_error) =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await
        {
            return Err(format!("{error}; {retirement_error}"));
        }
        return Err(error);
    }
    if let Err(error) = crate::agent::result_context::acknowledge(
        driver,
        &command_text,
        crate::agent::result_context::Submission::Recorded,
    ) {
        settle_or_fault(driver, effects, agent_id);
        if let Err(retirement_error) =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await
        {
            return Err(format!("{error}; {retirement_error}"));
        }
        return Err(error);
    }
    settle_or_fault(driver, effects, agent_id);
    Ok(result)
}
