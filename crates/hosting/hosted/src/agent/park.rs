//! Parking a child whose turn ended with only waiting left, and waking it on
//! the work the harness tracks for it (spec 03 §5.2).
//!
//! The turn waiter asks [`turn_end`] what an unsubmitted turn end means. A
//! parked child keeps its attempt `running` and its live session (its
//! processes need it); [`park_until_wake`] watches its own processes, its
//! children and its mail, and [`start_wake_turn`] admits the wake as an
//! ordinary host instruction on the same attempt and turn token.
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger, LedgerError};
use crate::driver::transient::{self, ResumeGate, ResumeOwner, ResumePlan};
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

fn settle_wake(
    ledger: &mut dyn CommandLedger,
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
    ledger.settle(command_id, settlement)
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
        let bound = mutate_child(driver, agent_id, |tree| {
            let owns = tree
                .attempt(agent_id)
                .is_some_and(|attempt| attempt.attempt_id == identity.attempt);
            if owns {
                tree.bind_turn_owner(agent_id, handle.turn_id().to_string());
            }
            owns
        });
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
    let parked = mutate_child(driver, agent_id, |tree| {
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
    });
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
                return match start_wake_turn(driver, live, agent_id, identity).await {
                    Ok(handle) => ParkEnd::Woken(handle, arrivals),
                    Err(WakeRefusal::ParkEnded) => end_park(driver, live, agent_id, identity).await,
                    // The child stays parked: the turn waiter marks it ending
                    // while it ends the attempt (or hands it to an interrupt).
                    Err(WakeRefusal::Failed(error)) => ParkEnd::WakeFailed(error),
                };
            }
            WakeDecision::Idle => {}
        }
        mutate_child(driver, agent_id, |tree| {
            tree.refresh_parked(agent_id, facts.running);
        });
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
    mutate_child(driver, agent_id, |tree| tree.unpark_child(agent_id));
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
async fn start_wake_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> Result<TurnResultHandle, WakeRefusal> {
    let (begun, effects) = driver
        .mutate(|tree| {
            let command = wake_command(tree, agent_id)?;
            Ok::<_, String>(tree.begin_wake(agent_id).then_some(command))
        })
        .map_err(|error| WakeRefusal::Failed(error.message))?;
    settle_or_fault(driver, effects, agent_id);
    let Some(command_text) = begun.map_err(WakeRefusal::Failed)? else {
        return Err(WakeRefusal::ParkEnded);
    };
    #[cfg(test)]
    hooks::before_wake_admission(agent_id).await;
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
        .map_err(String::from)
        .map_err(WakeRefusal::Failed)?;
    let Ok((mut live_map, delivery)) =
        super::live::lock_for_transition(live, agent_id, identity.turn).await
    else {
        settle_wake(
            &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
            &command_text,
            WakeSubmission::Refused,
        )
        .map_err(String::from)
        .map_err(WakeRefusal::Failed)?;
        return Err(WakeRefusal::ParkEnded);
    };
    // Re-checked under the live lock: an interrupt's cancel needs this lock,
    // so after this point it can only cancel the bound wake turn.
    let admissible = driver
        .read(|tree| identity.still_owns(tree, agent_id) && tree.wake_admissible(agent_id))
        .unwrap_or(false);
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
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return Err("child attempt has already been replaced".into());
            }
            tree.bind_turn_owner(agent_id, turn_id.to_string());
            if let Some(feed) = feed? {
                feed.apply(tree, agent_id).map_err(|error| error.message)?;
            }
            tree.unpark_child(agent_id);
            persist_or_fault(driver, tree, agent_id);
            Ok::<_, String>(())
        })
        .unwrap_or_else(|error| (Err(error.message), crate::agent_tree::Effects::default()));
    settle_or_fault(driver, effects, agent_id);
    let acknowledged = published.and_then(|()| {
        settle_wake(
            &mut crate::agent::ledger::ChildLedger::settlement_only(driver),
            &command_text,
            WakeSubmission::Recorded,
        )
        .map_err(String::from)
    });
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

/// How a reopen wake or rejoin of a held child ended, when it did not fail.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Reopened {
    /// The wake started, the turn rejoined, or the spent attempt ended.
    Done,
    /// An interrupt or close ended the hold first: nothing started or bound,
    /// the session was released, and any batch prepared for the turn is
    /// unread again. The attempt stays as the interrupt left it.
    HoldEnded,
}

/// The refusal a reopen wake's admission checks return once the hold ended.
pub(super) const REOPEN_HOLD_ENDED: &str =
    "the reopen hold ended before the wake; an interrupt or close won the race";

/// Moves a held child to its reopen wake (spec 03 section 14.1).
fn begin_reopen_wake(driver: &TreeDriver, agent_id: &str) -> bool {
    mutate_child(driver, agent_id, |tree| tree.begin_reopen_wake(agent_id)).unwrap_or(false)
}

/// Whether an interrupt or close ended a reopen wake's hold: the node no
/// longer runs, or its attempt ended.
fn reopen_hold_ended(driver: &TreeDriver, agent_id: &str) -> bool {
    driver
        .read(|tree| {
            tree.node(agent_id)
                .is_ok_and(|node| node.status != crate::agent_tree_protocol::NodeStatus::Running)
                || tree
                    .attempt(agent_id)
                    .is_none_or(|attempt| attempt.status.is_stopped())
        })
        .unwrap_or(false)
}

/// Settles `command_id`'s prepared batch, if it is still prepared: read when
/// the runtime admitted the command, unread otherwise.
fn settle_prepared(
    driver: &TreeDriver,
    agent_id: &str,
    command_id: &str,
    admitted: bool,
) -> Result<(), String> {
    for command in crate::agent::ledger::prepared_commands(driver, agent_id)? {
        if command.command_id == command_id {
            command.settle(driver, admitted)?;
        }
    }
    Ok(())
}

/// Wakes a child a reopen found parked, once, on its reopened session.
///
/// The resume's hold moves to the reopen wake first; an interrupt or close
/// that ended the hold leaves nothing to wake. Then, in order: the child's
/// retained await receipts settle against its committed history; a spent
/// `max_turns` ends the attempt `blocked` as the live path does; a command
/// the journal prepared and the runtime never recorded (a lost wake, or a
/// mail Start) is retried exactly under its own command identity and prompt
/// instead of a second Start; recorded ones settle. Only then does a fresh
/// wake run. Its reservation and its bind re-check the hold under the live
/// lock, so an interrupt that lands meanwhile never sees the node written
/// back to `running`.
pub(super) async fn wake_reopened(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: super::AttachedCodingSession,
) -> Result<Reopened, String> {
    if !begin_reopen_wake(runtime.driver(), agent_id) {
        let _ = attached.release_opened().await;
        return Ok(Reopened::HoldEnded);
    }
    match prepare_reopened_wake(runtime, agent_id, &attached).await {
        Ok(ReopenedWake::Resume(resume)) => {
            start_reopened_resume(runtime, agent_id, attached, resume).await
        }
        Ok(ReopenedWake::Start(command)) => {
            let (command_id, prompt) = if let Some(journaled) = command {
                let remaining =
                    crate::agent::ledger::prepared_commands(runtime.driver(), agent_id)?;
                let remaining = runtime
                    .read(|tree| {
                        remaining
                            .into_iter()
                            .filter(|command| {
                                command.command_id != journaled.command_id
                                    && tree.command_behind_retention(&command.command_id)
                            })
                            .collect()
                    })
                    .map_err(|error| error.message)?;
                (
                    journaled.command_id.clone(),
                    super::children::ChildCommand::Reopened {
                        first: journaled,
                        remaining,
                    },
                )
            } else {
                let command = runtime
                    .read(|tree| wake_command(tree, agent_id))
                    .map_err(|error| error.message)
                    .and_then(std::convert::identity);
                match command {
                    Ok(command) => (
                        command,
                        super::children::ChildCommand::fresh(
                            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT.to_owned(),
                            CommandKind::Wake,
                        ),
                    ),
                    Err(error) => {
                        let _ = attached.release_opened().await;
                        return Err(error);
                    }
                }
            };
            let command = CommandId::new(command_id.clone()).map_err(|error| error.to_string())?;
            let started = super::children::start_child_turn_as(
                runtime,
                agent_id,
                attached,
                command,
                prompt,
                super::children::StartAdmission::ReopenWake,
            )
            .await;
            match started {
                Ok(_) => Ok(Reopened::Done),
                // The hold ended after the runtime admitted the wake: its
                // batch is in history, so it settles read, as a rejoin whose
                // hold ended does, or the resume would deliver it again.
                Err(super::children::ChildStartFailure::HoldEndedAfterAdmission(_)) => {
                    settle_prepared(runtime.driver(), agent_id, &command_id, true)?;
                    Ok(Reopened::HoldEnded)
                }
                // The refusal names the hold; nothing else is at fault.
                Err(_) if reopen_hold_ended(runtime.driver(), agent_id) => {
                    settle_prepared(runtime.driver(), agent_id, &command_id, false)?;
                    Ok(Reopened::HoldEnded)
                }
                Err(failure) => Err(failure.into()),
            }
        }
        Ok(ReopenedWake::Spent) => {
            // Releasing stops the processes, as every attempt end does.
            let sessions = Arc::clone(&attached.sessions);
            let session_id = attached.locator().session_id();
            attached
                .release_opened()
                .await
                .map_err(|error| error.to_string())?;
            crate::agent::ledger::project_feed(runtime, agent_id, &sessions, session_id).await?;
            let terminal = super::children::read_finalize_terminal(
                runtime.driver(),
                &sessions,
                session_id,
                agent_id,
            )
            .await?;
            super::children::end_spent_attempt(runtime, agent_id, terminal).await?;
            Ok(Reopened::Done)
        }
        Err(error) => {
            let _ = attached.release_opened().await;
            Err(error)
        }
    }
}

enum ReopenedWake {
    /// A saved continuation decision, joined to its exact canonical failure.
    Resume(ReopenedResume),
    /// Start the wake, or retry the journaled command exactly.
    Start(Option<crate::agent::ledger::JournaledCommand>),
    /// `max_turns` is spent: the attempt ends instead.
    Spent,
}

struct ReopenedResume {
    identity: ChildTurnIdentity,
    plan: ResumePlan,
    terminal: Box<plakous::sessions::feed::FeedTerminal>,
}

async fn start_reopened_resume(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: super::AttachedCodingSession,
    resume: ReopenedResume,
) -> Result<Reopened, String> {
    let startup = match runtime
        .reserve_startup_admitted(agent_id, |tree| {
            if tree.reopen_wake_admissible(agent_id) {
                Ok(())
            } else {
                Err(REOPEN_HOLD_ENDED.to_owned())
            }
        })
        .await
    {
        Ok(startup) => startup,
        Err(error) => {
            let _ = attached.release_opened().await;
            return Err(error);
        }
    };
    mutate_child(runtime.driver(), agent_id, |tree| {
        tree.mark_transient_wait(agent_id, resume.plan.gate == ResumeGate::Network);
    });
    let owner = runtime.clone();
    let id = agent_id.to_owned();
    // The startup reservation belongs to this task, not the global reattach
    // lock. Interrupt and hub shutdown can cancel and join its session release.
    tokio::spawn(async move {
        let ready = super::children::wait_resume_gate(
            owner.driver(),
            None,
            Some(&startup),
            &id,
            &resume.identity,
            attached.reachability_probe.clone(),
            resume.plan.gate,
        )
        .await;
        if !ready {
            mutate_child(owner.driver(), &id, |tree| tree.unpark_child(&id));
            super::children::release_startup(&owner, &id, &startup, attached).await;
            return;
        }
        let plakous::agent::TurnResultOutcome::Failed(error) = resume.terminal.result().outcome()
        else {
            super::children::release_startup(&owner, &id, &startup, attached).await;
            return;
        };
        let started = super::children::start_child_turn_owned_with_ack(
            &owner,
            &id,
            attached,
            resume.plan.command_id,
            super::children::ChildCommand::fresh(resume.plan.text, CommandKind::Resume),
            &startup,
            false,
            super::children::StartAdmission::ReopenWake,
            std::future::ready(()),
        )
        .await;
        if started.is_err() && !reopen_hold_ended(owner.driver(), &id) {
            super::children::end_attempt_work(owner.driver(), &owner.live, &id, &resume.identity)
                .await
                .ok();
            super::children::finalize_child_failure(
                owner.driver(),
                &resume.identity,
                &id,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                error.message().to_owned(),
                super::children::FailedTurnWaits::Settle(resume.terminal.result()),
                None,
            )
            .await;
        }
        if started.is_err() {
            mutate_child(owner.driver(), &id, |tree| tree.unpark_child(&id));
        }
    });
    Ok(Reopened::Done)
}

async fn prepare_reopened_wake(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: &super::AttachedCodingSession,
) -> Result<ReopenedWake, String> {
    let driver = &runtime.driver().for_agent(agent_id);
    let retained = runtime
        .read(|tree| tree.retained_wait_receipts(agent_id))
        .map_err(|error| error.message)?;
    let evidence =
        crate::agent::committed_wait_evidence(attached.sessions(), attached.locator(), retained)
            .await?;
    // Canonical reads join the committed head in the projector, including
    // receipts left behind by a prior process's abandonment publication.
    let cursor = runtime
        .read(|tree| tree.feed_cursor(&attached.locator().session_id().to_string()))
        .map_err(|error| error.message)?;
    let mut feed =
        crate::agent::read_tree_feed(attached.sessions(), attached.locator().session_id(), cursor)
            .await?;
    feed.reads.insert_waits(attached.locator(), evidence);
    super::feed_projection::retry_tree_feed(attached.sessions(), feed, |feed| {
        super::feed_projection::apply_on_runtime(runtime, feed, agent_id)
    })
    .await
    .map_err(|error| error.message)?;
    if runtime
        .read(|tree| tree.attempt_turns_spent(agent_id))
        .map_err(|error| error.message)?
    {
        return Ok(ReopenedWake::Spent);
    }
    if let Some((turn, round)) = runtime
        .read(|tree| tree.pending_transient_resume(agent_id))
        .map_err(|error| error.message)?
        .map_err(|error| error.message)?
    {
        let locator = attached.locator();
        let turn = turn
            .parse()
            .map_err(|_| "saved continuation has an invalid turn identity".to_owned())?;
        let terminal = attached
            .sessions()
            .feed_terminal(plakous::sessions::feed::FeedTerminalRequest {
                session: locator.session_id(),
                branch: locator.branch_id(),
                checkpoint: locator.checkpoint_id(),
                turn_id: turn,
            })
            .await
            .map_err(|error| error.to_string())?;
        let plakous::sessions::feed::FeedTerminalOutcome::Terminal(terminal) = terminal else {
            return Err("saved continuation's canonical failure is not retained".to_owned());
        };
        let plakous::agent::TurnResultOutcome::Failed(error) = terminal.result().outcome() else {
            return Err("saved continuation does not name a canonical failed turn".to_owned());
        };
        let model = error
            .model_error()
            .ok_or_else(|| "saved continuation has no model failure".to_owned())?;
        model
            .context()
            .ok_or_else(|| "saved continuation has no typed failure context".to_owned())?;
        let gate = transient::failure_gate(terminal.result())
            .ok_or_else(|| "saved continuation is not eligible".to_owned())?;
        let attempt = runtime
            .read(|tree| {
                tree.attempt(agent_id)
                    .map(|attempt| attempt.attempt_id.clone())
            })
            .map_err(|error| error.message)?
            .ok_or_else(|| "saved continuation has no attempt".to_owned())?;
        // The immutable round is already spent; rebuild it, not a fresh round
        // under the current roster's budget.
        let plan = ResumePlan::for_round(
            gate,
            ResumeOwner::Child {
                agent: agent_id,
                attempt: &attempt,
            },
            error.message(),
            round,
        )
        .ok_or_else(|| "saved continuation has an invalid round".to_owned())?;
        for prior in crate::agent::ledger::prepared_commands(driver, agent_id)? {
            // The exact pending Resume keeps its original prompt and mail;
            // withdrawing it would make an accepted retry impossible to bind.
            if prior.command_id != plan.command_id.as_str() {
                prior.settle(driver, false)?;
            }
        }
        return Ok(ReopenedWake::Resume(ReopenedResume {
            identity: ChildTurnIdentity { attempt, turn },
            plan,
            terminal,
        }));
    }
    let attempt = runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .map(|attempt| attempt.attempt_id.clone())
        })
        .map_err(|error| error.message)?;
    let mut unrecorded = Vec::new();
    for command in crate::agent::ledger::prepared_commands(driver, agent_id)? {
        // An earlier attempt's command is never submitted onto this one, and
        // an unrecorded corrective round is re-derived from the attempt's
        // state: both are withdrawn, their mail unread.
        if command.is_corrective() || command.for_other_attempt(attempt.as_ref()) {
            command.settle(driver, false)?;
            continue;
        }
        unrecorded.push(command);
    }
    if unrecorded.len() == 1 {
        return Ok(ReopenedWake::Start(unrecorded.pop()));
    }
    // This knowledge is durable: a previous startup may have caught the
    // cursor up and consumed only the first command before it was lost.
    let marked = driver
        .read(|tree| {
            unrecorded
                .iter()
                .position(|command| tree.command_behind_retention(&command.command_id))
        })
        .map_err(|error| error.message)?;
    if let Some(index) = marked {
        return Ok(ReopenedWake::Start(Some(unrecorded.remove(index))));
    }
    // More than one never-admitted command has no single exact retry: none
    // ran, so their mail returns to unread and one fresh wake carries it.
    for command in unrecorded {
        command.settle(driver, false)?;
    }
    Ok(ReopenedWake::Start(None))
}

/// Rejoins a turn a reopen found in flight on a held child with the parked
/// shape: a wake or mail Start the lost process journaled, which the runtime
/// admitted before the tree bound its turn. The child reopen leaves that
/// command prepared (its generic reconcile skips an unbound recovered turn),
/// so the command is found here by its record. The turn binds and occupies
/// the live slot under the live lock only while the hold still stands; its
/// journaled batch then settles accepted (acknowledging its mail) and the
/// host observes it like any rejoined turn. An interrupt or close that ended
/// the hold first cancels the turn; its batch, already in history, settles
/// accepted. A turn no journaled command admitted leaves the child to
/// recovery.
pub(super) async fn rejoin_reopened(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: super::AttachedCodingSession,
    recovered: plakous::agent::RecoveredTurnHandle,
) -> Result<Reopened, String> {
    let (turn, control, result) = (recovered.turn_id(), recovered.control(), recovered.result());
    rejoin_turn(runtime, agent_id, attached, turn, control, result).await
}

async fn rejoin_turn(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: super::AttachedCodingSession,
    turn: plakous::model::TurnId,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
) -> Result<Reopened, String> {
    let driver = &runtime.driver().for_agent(agent_id);
    let begun = begin_reopen_wake(driver, agent_id);
    // The feed pass binds the turn to the command that admitted it and
    // acknowledges that command's batch: the batch is in the session's
    // history even if the turn is cancelled below, so the resume never
    // delivers that mail a second time.
    let admitted = admitted_command(runtime, agent_id, &attached, turn).await;
    let sessions = Arc::clone(&attached.sessions);
    let session_id = attached.locator().session_id();
    let feed = super::children::ChildFeedSource::of(&attached);
    let child: Box<dyn crate::topology_runtime::LiveChildHandle> =
        Box::new(super::live::ProductLiveChild::new(attached, turn, control));
    match (begun, admitted) {
        (true, Ok(_)) => {}
        (false, _) => {
            let _ = child.cancel_and_release(driver.clone()).await;
            project_cancelled(runtime, agent_id, &sessions, session_id).await;
            return Ok(Reopened::HoldEnded);
        }
        (true, Err(error)) => {
            let _ = child.release(driver.clone()).await;
            return Err(error);
        }
    }
    // The turn's waiter registers with the bind, before the slot is visible;
    // a rejoin that never spawns the waiter drops it.
    let mut registration = None;
    let occupied = runtime
        .try_occupy_live_admitted(agent_id.to_owned(), child, || {
            mutate_child(driver, agent_id, |tree| {
                if !tree.reopen_wake_admissible(agent_id) {
                    return Err(REOPEN_HOLD_ENDED.to_owned());
                }
                registration =
                    super::children::TurnWaiterRegistration::enter(driver, tree, agent_id);
                tree.unpark_child(agent_id);
                persist_or_fault(driver, tree, agent_id);
                Ok(())
            })
            .unwrap_or_else(|| Err("topology tree is unavailable".to_owned()))
        })
        .await;
    match occupied {
        Ok(_) => {}
        Err((child, Some(refusal))) if refusal == REOPEN_HOLD_ENDED => {
            let _ = child.cancel_and_release(driver.clone()).await;
            project_cancelled(runtime, agent_id, &sessions, session_id).await;
            return Ok(Reopened::HoldEnded);
        }
        Err((child, refusal)) => {
            let _ = child.cancel_and_release(driver.clone()).await;
            return Err(refusal.unwrap_or_else(|| {
                "the child's live slot is taken or the topology hub is closed".to_owned()
            }));
        }
    }
    if let Some(registration) = registration {
        super::children::spawn_topology_turn_waiter(
            registration,
            Arc::clone(&runtime.live),
            runtime.observer(),
            Some(feed),
            result,
        );
    }
    Ok(Reopened::Done)
}

/// The command whose Start the runtime admitted as `turn`, from the child's
/// feed: applying it to its tail settles every command it names. A command
/// still unresolved afterwards was never recorded and did not run, so it is
/// withdrawn and its mail returns to unread.
async fn admitted_command(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: &super::AttachedCodingSession,
    turn: plakous::model::TurnId,
) -> Result<String, String> {
    let driver = &runtime.driver().for_agent(agent_id);
    crate::agent::ledger::project_child_feed(runtime, agent_id, attached).await?;
    for command in crate::agent::ledger::prepared_commands(driver, agent_id)? {
        command.settle(driver, false)?;
    }
    crate::agent::ledger::accepted_command(driver, agent_id, turn)?.ok_or_else(|| {
        "a reopened parked child runs a turn no journaled command admitted; recovery required"
            .to_owned()
    })
}

/// Applies a cancelled rejoin's terminal to the tree: the turn stays bound
/// to the attempt that admitted it and is no longer active. A refused pass
/// is projection debt: the cursor did not move, and the next open's pass
/// applies the same records.
async fn project_cancelled(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    sessions: &plakous::sessions::AgentSessions,
    session_id: plakous::model::SessionId,
) {
    let _ = crate::agent::ledger::project_feed(runtime, agent_id, sessions, session_id).await;
}

/// Settles the journaled command that admitted a turn a mail-delivered
/// reopen recovered with no bound turn, and names it: the child reopen
/// leaves it prepared for exactly this (spec 03 section 14.1).
pub(super) async fn settle_recovered_start(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: &super::AttachedCodingSession,
    turn: plakous::model::TurnId,
) -> Result<String, String> {
    admitted_command(runtime, agent_id, attached, turn).await
}

/// Test seams: work a test runs inside the windows where an interrupt or new
/// mail races a wake, so the race is forced instead of hoped for.
#[cfg(test)]
pub(super) mod hooks {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    pub(in crate::agent) type Hook =
        Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

    static BEFORE_WAKE_ADMISSION: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
    static BEFORE_FAILED_WAKE_END: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
    static AFTER_FOLLOW_UP_ADMISSION: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
    static AFTER_START_ADMISSION: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
    static AFTER_RECOVERED_OCCUPY: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
    static AROUND_REOPEN_SPAWNED_CHILD: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);

    fn set(slot: &Mutex<Option<HashMap<String, Hook>>>, agent_id: &str, hook: Hook) {
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(agent_id.to_owned(), hook);
    }

    async fn run(slot: &Mutex<Option<HashMap<String, Hook>>>, agent_id: &str) {
        let hook = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|hooks| hooks.get(agent_id).cloned());
        if let Some(hook) = hook {
            hook().await;
        }
    }

    /// Runs `hook` after a wake began (the child is marked waking) and
    /// before its Start is admitted.
    pub(in crate::agent) fn set_before_wake_admission(agent_id: &str, hook: Hook) {
        set(&BEFORE_WAKE_ADMISSION, agent_id, hook);
    }

    pub(in crate::agent) async fn before_wake_admission(agent_id: &str) {
        run(&BEFORE_WAKE_ADMISSION, agent_id).await;
    }

    /// Runs `hook` after a wake that was not admitted marked the child
    /// ending, before the attempt ends.
    pub(in crate::agent) fn set_before_failed_wake_end(agent_id: &str, hook: Hook) {
        set(&BEFORE_FAILED_WAKE_END, agent_id, hook);
    }

    pub(in crate::agent) async fn before_failed_wake_end(agent_id: &str) {
        run(&BEFORE_FAILED_WAKE_END, agent_id).await;
    }

    /// Runs `hook` after the runtime admitted a child Start and before its
    /// turn binds.
    pub(in crate::agent) fn set_after_start_admission(agent_id: &str, hook: Hook) {
        set(&AFTER_START_ADMISSION, agent_id, hook);
    }

    pub(in crate::agent) async fn after_start_admission(agent_id: &str) {
        run(&AFTER_START_ADMISSION, agent_id).await;
    }

    pub(in crate::agent) fn set_after_follow_up_admission(agent_id: &str, hook: Hook) {
        set(&AFTER_FOLLOW_UP_ADMISSION, agent_id, hook);
    }

    pub(in crate::agent) async fn after_follow_up_admission(agent_id: &str) {
        run(&AFTER_FOLLOW_UP_ADMISSION, agent_id).await;
    }

    /// Runs `hook` after a mail Start's rejoin handed its recovered turn to
    /// a turn waiter, before the rejoin returns.
    pub(in crate::agent) fn set_after_recovered_occupy(agent_id: &str, hook: Hook) {
        set(&AFTER_RECOVERED_OCCUPY, agent_id, hook);
    }

    pub(in crate::agent) async fn after_recovered_occupy(agent_id: &str) {
        run(&AFTER_RECOVERED_OCCUPY, agent_id).await;
    }

    /// Runs `hook` after a mail Start has reopened the child session and
    /// before it reserves the live slot. That is the window where another
    /// Start can take the slot.
    pub(in crate::agent) fn set_around_reopen_spawned_child(agent_id: &str, hook: Hook) {
        set(&AROUND_REOPEN_SPAWNED_CHILD, agent_id, hook);
    }

    pub(in crate::agent) fn clear_around_reopen_spawned_child(agent_id: &str) {
        if let Some(hooks) = AROUND_REOPEN_SPAWNED_CHILD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            hooks.remove(agent_id);
        }
    }

    pub(in crate::agent) async fn around_reopen_spawned_child(agent_id: &str) {
        run(&AROUND_REOPEN_SPAWNED_CHILD, agent_id).await;
    }
}

#[cfg(test)]
mod tests;
