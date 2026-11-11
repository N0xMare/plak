//! Parking a child whose turn ended with only waiting left, and waking it on
//! the work the harness tracks for it (spec 03 §5.2).
//!
//! The turn waiter asks [`turn_end`] what an unsubmitted turn end means. A
//! parked child keeps its attempt `running` and its live session (its
//! processes need it); [`park_until_wake`] watches its own processes, its
//! children and its mail, and [`start_wake_turn`] admits the wake as an
//! ordinary host instruction on the same attempt and turn token.
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger, LedgerError};
use crate::driver::wake::{WakeCoalescer, WakeDecision, WakeIdentity, WakePolicy, WakeSources};

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::{CommandId, TurnResultHandle};

use super::children::{ChildTurnIdentity, mutate_child, persist_or_fault, settle_or_fault};
use crate::agent_tree_tools::TreeDriver;
use crate::topology_runtime::LiveChildTable;

/// Safety tick behind the tree and process notifications, as `wait_agent` has.
const SAFETY_TICK: Duration = Duration::from_secs(1);

/// A role classifies its transport's response; only this mapping changes the
/// journal. Uncertainty, including reconciliation, never withdraws an intent.
enum WakeSubmission {
    Recorded,
    Consumed,
    Refused,
    Uncertain,
}

async fn settle_wake(
    ledger: &mut (dyn CommandLedger + Send),
    command_id: &str,
    outcome: WakeSubmission,
) -> Result<(), LedgerError> {
    use crate::driver::ledger::Settlement;
    let settlement = match outcome {
        WakeSubmission::Recorded => Settlement::Recorded,
        WakeSubmission::Consumed => Settlement::Consumed,
        WakeSubmission::Refused => Settlement::Refused { pause: None },
        WakeSubmission::Uncertain => return Ok(()),
    };
    ledger.settle(command_id, settlement).await
}

/// What an unsubmitted child turn end means.
pub(super) enum TurnEnd {
    /// A follow-up admitted behind the ended turn is now running: observe it.
    FollowUp(TurnResultHandle),
    /// Live work remains or tracked work already arrived: park. Arrivals wake
    /// the park at once.
    Park,
    /// Nothing is tracked: the submit harness decides, as before.
    Settle,
    /// The template's `max_turns` is spent: no wake and no corrective turn
    /// may run, so the attempt ends now.
    Spent,
    /// An interrupt landed after the turn ended, before this decision: the
    /// turn ends as an interrupted one.
    Interrupted,
}

/// How a park ended.
pub(super) enum ParkEnd {
    /// The wake turn was admitted and bound; the arrivals it woke on.
    Woken(TurnResultHandle, Arrivals),
    /// The wake turn could not be admitted.
    WakeFailed(String),
    /// An interrupt ended the park, possibly racing the wake's admission:
    /// the turn ends as an interrupted one and the attempt stays open.
    Interrupted,
    /// Close, attempt end or hub shutdown ended the park; the turn waiter
    /// has nothing left to do.
    Ended,
}

/// Unread mail ids and unconsumed process notices (`p<N>`) a wake carried.
pub(super) use crate::driver::wake::Arrivals;

/// Reads the facts after the turn result: the actor has finished that turn,
/// so a process that ends now is either still `running` here (its terminal
/// edge wakes the park) or already a pending notice (which wakes at once).
async fn read_facts(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
) -> Option<WakeSources> {
    let agent = live.lock().await.get(agent_id)?.agent()?;
    let jobs = agent.jobs();
    let running = jobs
        .iter()
        .filter(|job| job.status == plakous::tools::JobStatus::Running)
        .map(|job| format!("p{}", job.number))
        .collect();
    let notices = jobs
        .iter()
        .filter(|job| job.status != plakous::tools::JobStatus::Running)
        .map(|job| format!("p{}", job.number));
    let (children, mail, claimed) = driver
        .read(|tree| {
            (
                tree.live_child_work(agent_id),
                tree.wake_mail(agent_id)
                    .into_iter()
                    .map(|mail| mail.message_id)
                    .collect::<Vec<_>>(),
                tree.claimed_mail(agent_id),
            )
        })
        .ok()?;
    Some(WakeSources {
        running,
        arrivals: notices.chain(mail).collect(),
        children,
        claimed,
    })
}

/// Whether an interrupt moved the node after this turn's terminal: the
/// attempt is still this one, and the node reads `interrupted`.
fn interrupted(driver: &TreeDriver, agent_id: &str, identity: &ChildTurnIdentity) -> bool {
    driver
        .read(|tree| {
            identity.still_owns(tree, agent_id)
                && tree.node(agent_id).is_ok_and(|node| {
                    node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                })
        })
        .unwrap_or(false)
}

/// Decides an unsubmitted turn end. `carried` is what the last wake carried.
pub(super) async fn turn_end(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    carried: &Arrivals,
) -> TurnEnd {
    let follow_up = match super::live::lock_for_transition(live, agent_id, identity.turn).await {
        Ok((mut children, _delivery)) => children
            .get_mut(agent_id)
            .and_then(crate::topology_runtime::LiveChildHandle::take_follow_up),
        Err(_) => None,
    };
    if let Some(handle) = follow_up {
        let (id, attempt, turn) = (
            agent_id.to_owned(),
            identity.attempt.clone(),
            handle.turn_id().to_string(),
        );
        let bound = mutate_child(driver, agent_id, move |tree| {
            let owns = tree
                .attempt(&id)
                .is_some_and(|current| current.attempt_id == attempt);
            if owns {
                tree.bind_turn_owner(&id, turn);
            }
            owns
        })
        .await;
        if bound == Some(true) {
            return TurnEnd::FollowUp(handle);
        }
    }
    // The waiter classified the terminal before an interrupt that landed
    // since; the node no longer runs, so nothing may park it.
    if interrupted(driver, agent_id, identity) {
        return TurnEnd::Interrupted;
    }
    if driver
        .read(|tree| tree.attempt_turns_spent(agent_id))
        .unwrap_or(false)
    {
        return TurnEnd::Spent;
    }
    let Some(facts) = read_facts(driver, live, agent_id).await else {
        return TurnEnd::Settle;
    };
    // Work that arrived as the turn ended wakes without a nudge, and so does
    // mail whose delivery is still in flight; only an end with nothing
    // running and nothing new takes the corrective rounds.
    if facts.live_work() || !facts.claimed.is_empty() || facts.arrived_beyond(carried) {
        TurnEnd::Park
    } else {
        TurnEnd::Settle
    }
}

/// Parks the child, waits for a wake source, and admits the wake turn.
pub(super) async fn park_until_wake(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    carried: &Arrivals,
) -> ParkEnd {
    let processes = read_facts(driver, live, agent_id)
        .await
        .map(|facts| facts.running)
        .unwrap_or_default();
    let parked = mutate_child(driver, agent_id, {
        let (identity, agent_id, driver) = (identity.clone(), agent_id.to_owned(), driver.clone());
        move |tree| {
            let (identity, agent_id, driver) = (&identity, agent_id.as_str(), &driver);
            if !identity.still_owns(tree, agent_id) {
                return false;
            }
            if tree
                .record_child_finalization(
                    agent_id,
                    &identity.turn.to_string(),
                    crate::agent_tree::ChildFinalizationDecision::Park,
                    true,
                    None,
                )
                .is_err()
                || !persist_or_fault(driver, tree, agent_id)
            {
                return false;
            }
            let parked = tree.park_child(agent_id, processes).is_ok();
            persist_or_fault(driver, tree, agent_id);
            parked
        }
    })
    .await;
    if parked != Some(true) {
        return end_park(driver, live, agent_id, identity).await;
    }
    let mut coalescer = WakeCoalescer::new(WakePolicy::Child);
    coalescer.carried(carried.clone());
    loop {
        // Subscribe before reading, so an edge during the read still wakes
        // the next wait instead of waiting out the tick.
        let Ok(notify) = driver.read(crate::agent_tree::ProductAgentTree::waiters) else {
            return end_park(driver, live, agent_id, identity).await;
        };
        let mut notified = Box::pin(notify.notified_owned());
        notified.as_mut().enable();
        let mut job_changes = driver.job_terminal_changes(agent_id);
        if !still_parked(driver, live, agent_id, identity).await {
            return end_park(driver, live, agent_id, identity).await;
        }
        let Some(facts) = read_facts(driver, live, agent_id).await else {
            return end_park(driver, live, agent_id, identity).await;
        };
        // Work that vanished without a report (a child closed by someone
        // else) also wakes: nothing else would. Mail whose delivery is still
        // in flight holds the wake until that delivery returns, so the wake
        // carries it if the delivery found the turn ended.
        match coalescer.observe(&facts, std::time::Instant::now()) {
            WakeDecision::Wait { until } => {
                tokio::time::sleep_until(until.into()).await;
                continue;
            }
            WakeDecision::Fire(arrivals) => {
                if !crate::agent::placement::ready(driver, agent_id)
                    && crate::agent::placement::wait_ready(driver, live, agent_id, identity).await
                        != crate::agent::placement::Admission::Ready
                {
                    return end_park(driver, live, agent_id, identity).await;
                }
                return match start_wake_turn(driver, live, agent_id, identity).await {
                    Ok(handle) => ParkEnd::Woken(handle, arrivals),
                    Err(WakeRefusal::ParkEnded) => end_park(driver, live, agent_id, identity).await,
                    Err(WakeRefusal::Placement) => continue,
                    // The child stays parked: the turn waiter marks it ending
                    // while it ends the attempt (or hands it to an interrupt).
                    Err(WakeRefusal::Failed(error)) => ParkEnd::WakeFailed(error),
                };
            }
            WakeDecision::Idle => {}
        }
        mutate_child(driver, agent_id, {
            let agent_id = agent_id.to_owned();
            move |tree| {
                let agent_id = agent_id.as_str();
                tree.refresh_parked(agent_id, facts.running);
            }
        })
        .await;
        let mut process_exit = Box::pin(async move {
            if let Some(changes) = job_changes.as_mut()
                && changes.changed().await.is_ok()
            {
                return;
            }
            std::future::pending::<()>().await;
        });
        let changed = std::future::poll_fn(|context| {
            if notified.as_mut().poll(context).is_ready()
                || process_exit.as_mut().poll(context).is_ready()
            {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        });
        let _ = tokio::time::timeout(SAFETY_TICK, changed).await;
    }
}

async fn still_parked(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> bool {
    let live_entry = live
        .lock()
        .await
        .get(agent_id)
        .is_some_and(|child| child.turn_id() == identity.turn);
    live_entry
        && driver
            .read(|tree| {
                identity.still_owns(tree, agent_id)
                    && tree.is_parked(agent_id)
                    && tree.node(agent_id).is_ok_and(|node| {
                        node.status == crate::agent_tree_protocol::NodeStatus::Running
                    })
            })
            .unwrap_or(false)
}

/// An interrupt ends the turn as an interrupted one (spec 03 §9), whether it
/// moved a parked child or reached the turn waiter before the park. Close or
/// attempt end already did their own cleanup (close stops the processes and
/// seals). A hub shutdown leaves the durable parked shape for the next open
/// to wake.
async fn end_park(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> ParkEnd {
    mutate_child(driver, agent_id, {
        let agent_id = agent_id.to_owned();
        move |tree| {
            let agent_id = agent_id.as_str();
            tree.unpark_child(agent_id);
        }
    })
    .await;
    if interrupted(driver, agent_id, identity) {
        return ParkEnd::Interrupted;
    }
    let finished = driver
        .read(|tree| {
            tree.node(agent_id)
                .is_ok_and(|node| node.status != crate::agent_tree_protocol::NodeStatus::Running)
                || tree
                    .attempt(agent_id)
                    .is_none_or(|attempt| attempt.status.is_stopped())
        })
        .unwrap_or(true);
    if finished {
        super::children::retire_live_child(driver, live, agent_id, identity.turn).await;
    }
    ParkEnd::Ended
}

/// The command a wake admits: unique per attempt and bound turn count, so an
/// exact retry keeps its identity and every later wake gets its own.
pub(super) fn wake_command(
    tree: &crate::agent_tree::ProductAgentTree,
    agent_id: &str,
) -> Result<String, String> {
    WakeIdentity::next_child(tree, agent_id).map_err(|error| error.to_string())
}

/// Why a wake did not start.
enum WakeRefusal {
    Placement,
    /// The park ended before the Start (an interrupt or close won the race,
    /// or the live session went away): nothing was admitted, and any batch
    /// prepared for it was released unread.
    ParkEnded,
    /// The wake could not be admitted.
    Failed(String),
}

/// Admits the wake on the live session: the fixed wake instruction plus the
/// coalesced mail batch, with `Instruction` provenance. The driver inserts the
/// process notices at admission. An interrupt or close that lands before the
/// Start is admitted ends the park instead; one that lands after binds the
/// wake turn first and then cancels it, like any running turn.
#[allow(
    clippy::too_many_lines,
    reason = "A parked-child wake binds its durable wake receipt before starting the instruction turn and reconciles every refusal window."
)]
async fn start_wake_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> Result<TurnResultHandle, WakeRefusal> {
    if !crate::agent::placement::ready(driver, agent_id) {
        return Err(WakeRefusal::Placement);
    }
    let (begun, effects) = driver
        .mutate({
            let agent_id = agent_id.to_owned();
            move |tree| {
                let agent_id = agent_id.as_str();
                let command = wake_command(tree, agent_id)?;
                Ok::<_, String>(tree.begin_wake(agent_id).then_some(command))
            }
        })
        .await
        .map_err(|error| WakeRefusal::Failed(error.message))?;
    settle_or_fault(driver, effects, agent_id).await;
    let Some(command_text) = begun.map_err(WakeRefusal::Failed)? else {
        if !crate::agent::placement::ready(driver, agent_id) {
            return Err(WakeRefusal::Placement);
        }
        return Err(WakeRefusal::ParkEnded);
    };
    #[cfg(test)]
    hooks::before_wake_admission(agent_id).await;
    if !crate::agent::placement::ready(driver, agent_id)
        && crate::agent::placement::wait_ready(driver, live, agent_id, identity).await
            != crate::agent::placement::Admission::Ready
    {
        return Err(WakeRefusal::ParkEnded);
    }
    let prompt = crate::child_prompts::PLAK_CHILD_WAKE_PROMPT;
    let command_id = CommandId::new(command_text.clone())
        .map_err(|error| WakeRefusal::Failed(error.to_string()))?;
    let context = crate::agent::ledger::ChildLedger::new(driver, agent_id)
        .prepare(CommandIntent {
            command_id: &command_text,
            kind: CommandKind::Wake,
            base_text: prompt,
            batch: BatchRule::WakeAll,
        })
        .await
        .map_err(String::from)
        .map_err(WakeRefusal::Failed)?;
    #[cfg(test)]
    hooks::before_wake_binding(agent_id).await;
    let Ok((mut live_map, delivery)) =
        super::live::lock_for_transition(live, agent_id, identity.turn).await
    else {
        settle_wake(
            &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
            &command_text,
            WakeSubmission::Refused,
        )
        .await
        .map_err(String::from)
        .map_err(WakeRefusal::Failed)?;
        return Err(WakeRefusal::ParkEnded);
    };
    // Re-checked under the live lock: an interrupt's cancel needs this lock,
    // so after this point it can only cancel the bound wake turn.
    let admissible = driver
        .read(|tree| identity.still_owns(tree, agent_id) && tree.wake_admissible(agent_id))
        .unwrap_or(false);
    if !crate::agent::placement::ready(driver, agent_id) {
        drop(live_map);
        settle_wake(
            &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
            &command_text,
            WakeSubmission::Refused,
        )
        .await
        .map_err(String::from)
        .map_err(WakeRefusal::Failed)?;
        mutate_child(driver, agent_id, {
            let id = agent_id.to_owned();
            move |tree| tree.defer_wake(&id)
        })
        .await;
        return Err(WakeRefusal::Placement);
    }
    let child = live_map
        .get_mut(agent_id)
        .filter(|child| admissible && child.turn_id() == identity.turn);
    let Some(child) = child else {
        drop(live_map);
        settle_wake(
            &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
            &command_text,
            WakeSubmission::Refused,
        )
        .await
        .map_err(String::from)
        .map_err(WakeRefusal::Failed)?;
        return Err(WakeRefusal::ParkEnded);
    };
    let result = match child
        .restart(driver.clone(), command_id, context.prompt.clone(), delivery)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            drop(live_map);
            if error == super::live::RESTART_PLACEMENT_UNAVAILABLE {
                settle_wake(
                    &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
                    &command_text,
                    WakeSubmission::Refused,
                )
                .await
                .map_err(String::from)
                .map_err(WakeRefusal::Failed)?;
                mutate_child(driver, agent_id, {
                    let id = agent_id.to_owned();
                    move |tree| tree.defer_wake(&id)
                })
                .await;
                return Err(WakeRefusal::Placement);
            }
            if error == super::live::RESTART_RECONCILIATION_REQUIRED {
                super::children::finalize_child_failure(
                    driver,
                    identity,
                    agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    error.clone(),
                    super::children::FailedTurnWaits::Keep,
                    None,
                )
                .await;
            } else {
                // Consumption records a below-retention batch; definite
                // refusal withdraws it with the mail unread. Other failures
                // retain the intent for reconciliation.
                let settlement = if error == super::children::CHILD_COMMAND_CONSUMED {
                    WakeSubmission::Consumed
                } else if error.starts_with(super::live::RESTART_REJECTED) {
                    WakeSubmission::Refused
                } else {
                    WakeSubmission::Uncertain
                };
                settle_wake(
                    &mut crate::agent::ledger::ChildLedger::new(driver, agent_id),
                    &command_text,
                    settlement,
                )
                .await
                .map_err(|settle| WakeRefusal::Failed(format!("{error}; {settle}")))?;
            }
            return Err(WakeRefusal::Failed(error));
        }
    };
    let turn_id = child.turn_id();
    let cursor = super::live::live_feed_cursor(driver, agent_id).map_err(WakeRefusal::Failed)?;
    let read = child.feed(cursor);
    drop(live_map);
    let feed = read.await;
    let (published, effects) = driver
        .mutate({
            let (identity, agent_id, driver) =
                (identity.clone(), agent_id.to_owned(), driver.clone());
            move |tree| {
                let (identity, agent_id, driver) = (&identity, agent_id.as_str(), &driver);
                if !identity.still_owns(tree, agent_id) {
                    return Err("child attempt has already been replaced".into());
                }
                tree.bind_turn_owner(agent_id, turn_id.to_string());
                // Through the wake turn's acceptance only: the turn runs
                // on, and its turn end folds the rest.
                if let Some(feed) = feed? {
                    crate::agent::feed_projection::through_acceptance(Arc::new(feed), turn_id)
                        .apply(tree, agent_id)
                        .map_err(|error| error.message)?;
                }
                tree.unpark_child(agent_id);
                persist_or_fault(driver, tree, agent_id);
                Ok::<_, String>(())
            }
        })
        .await
        .unwrap_or_else(|error| (Err(error.message), crate::agent_tree::Effects::default()));
    settle_or_fault(driver, effects, agent_id).await;
    let acknowledged = match published {
        Ok(()) => settle_wake(
            &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
            &command_text,
            WakeSubmission::Recorded,
        )
        .await
        .map_err(String::from),
        Err(error) => Err(error),
    };
    if let Err(error) = acknowledged {
        // The wake was admitted (and possibly bound) but its batch could not
        // be settled: like a restart that needs reconciliation, the wake
        // turn is cancelled and the attempt falls to recovery under the wake
        // turn's identity, its context left for the reopen reconcile. The
        // parent's wait settles on the `recovery_required` node.
        let retired =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await;
        let message = match retired {
            Ok(_) => format!("parked child wake could not be settled: {error}"),
            Err(retirement) => {
                format!("parked child wake could not be settled: {error}; {retirement}")
            }
        };
        super::children::finalize_child_failure(
            driver,
            &ChildTurnIdentity {
                attempt: identity.attempt.clone(),
                turn: turn_id,
            },
            agent_id,
            crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
            message.clone(),
            super::children::FailedTurnWaits::Keep,
            None,
        )
        .await;
        return Err(WakeRefusal::Failed(message));
    }
    Ok(result)
}

/// Test seams: work a test runs inside the windows where an interrupt or new
/// mail races a wake, so the race is forced instead of hoped for.
#[cfg(test)]
pub(super) mod hooks;

#[cfg(test)]
pub(in crate::agent) mod tests;
