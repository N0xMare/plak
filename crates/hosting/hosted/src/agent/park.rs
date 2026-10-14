//! Parking a child whose turn ended with only waiting left, and waking it on
//! the work the harness tracks for it (spec 03 §5.2).
//!
//! The turn waiter asks [`turn_end`] what an unsubmitted turn end means. A
//! parked child keeps its attempt `running` and its live session (its
//! processes need it); [`park_until_wake`] watches its own processes, its
//! children and its mail, and [`start_wake_turn`] admits the wake as an
//! ordinary host instruction on the same attempt and turn token.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::{CommandId, TurnResultHandle};

use super::children::{ChildTurnIdentity, mutate_child, persist_or_fault, settle_or_fault};
use crate::agent_tree_tools::TreeDriver;
use crate::topology_runtime::LiveChildTable;

/// Prefix of every wake command; the result-context journal reads it to admit
/// mail of every purpose into the wake batch.
pub(super) const WAKE_COMMAND_PREFIX: &str = "plak:park-wake:";

/// Near-simultaneous arrivals ride one wake.
const COALESCE: Duration = Duration::from_secs(1);
/// Safety tick behind the tree and process notifications, as `wait_agent` has.
const SAFETY_TICK: Duration = Duration::from_secs(1);

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
pub(super) type Arrivals = std::collections::BTreeSet<String>;

struct ParkFacts {
    running: Vec<String>,
    arrivals: Arrivals,
    children: Vec<String>,
    /// Unread mail a live delivery still holds: it lands as an arrival (or
    /// is delivered) once that delivery returns.
    claimed: Vec<String>,
}

impl ParkFacts {
    fn live_work(&self) -> bool {
        !self.running.is_empty() || !self.children.is_empty()
    }

    /// Arrivals the last wake did not already carry. A wake consumes its
    /// batch and its notices; whatever it could not consume (past the prompt
    /// bound) must not wake the child again by itself, or a turn end would
    /// wake forever.
    fn arrived_beyond(&self, carried: &Arrivals) -> bool {
        self.arrivals.iter().any(|id| !carried.contains(id))
    }
}

/// Reads the facts after the turn result: the actor has finished that turn,
/// so a process that ends now is either still `running` here (its terminal
/// edge wakes the park) or already a pending notice (which wakes at once).
async fn read_facts(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent_id: &str,
) -> Option<ParkFacts> {
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
    Some(ParkFacts {
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
    let follow_up = live
        .lock()
        .await
        .get_mut(agent_id)
        .filter(|child| child.turn_id() == identity.turn)
        .and_then(crate::topology_runtime::LiveChildHandle::take_follow_up);
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
        if facts.claimed.is_empty() && (facts.arrived_beyond(carried) || !facts.live_work()) {
            tokio::time::sleep(COALESCE).await;
            if !still_parked(driver, live, agent_id, identity).await {
                return end_park(driver, live, agent_id, identity).await;
            }
            let arrivals = read_facts(driver, live, agent_id)
                .await
                .map(|facts| facts.arrivals)
                .unwrap_or_default();
            return match start_wake_turn(driver, live, agent_id, identity).await {
                Ok(handle) => ParkEnd::Woken(handle, arrivals),
                Err(WakeRefusal::ParkEnded) => end_park(driver, live, agent_id, identity).await,
                // The child stays parked: the turn waiter marks it ending
                // while it ends the attempt (or hands it to an interrupt).
                Err(WakeRefusal::Failed(error)) => ParkEnd::WakeFailed(error),
            };
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
    let attempt = tree
        .attempt(agent_id)
        .ok_or_else(|| "attempt is missing".to_owned())?;
    if attempt.output_json.is_some() || attempt.status.is_stopped() {
        return Err("attempt is already submitted or terminal".into());
    }
    if tree.attempt_turns_spent(agent_id) {
        return Err("attempt has spent its template max_turns; no wake turn is admitted".into());
    }
    let mut count = attempt.usage.turns;
    loop {
        let id = format!(
            "{WAKE_COMMAND_PREFIX}{agent_id}:{}:{}:{count}",
            tree.host_generation, attempt.attempt_id
        );
        // Consumption did not spend a turn, but its identity is no longer
        // available to a fresh wake on this attachment.
        if !tree.command(&id).is_some_and(|command| {
            command.fact == Some(crate::agent_tree::CommandFact::Accepted)
                && command.turn_id.is_none()
        }) {
            return Ok(id);
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| "wake command identities are exhausted".to_owned())?;
    }
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
    let context = super::result_context::prepare(driver, agent_id, &command_text, prompt)
        .map_err(WakeRefusal::Failed)?;
    let mut live_map = live.lock().await;
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
        super::result_context::acknowledge(
            driver,
            &command_text,
            super::result_context::Submission::Refused,
        )
        .map_err(WakeRefusal::Failed)?;
        return Err(WakeRefusal::ParkEnded);
    };
    let result = match child
        .restart(driver.clone(), command_id, context.prompt.clone())
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
            } else if error == super::children::CHILD_COMMAND_CONSUMED {
                // Recorded below the feed's retention: its batch is in
                // history, and nothing is derived from it.
                if let Err(settle) = super::result_context::acknowledge(
                    driver,
                    &command_text,
                    super::result_context::Submission::Consumed,
                ) {
                    return Err(WakeRefusal::Failed(format!("{error}; {settle}")));
                }
            } else if error.starts_with(super::live::RESTART_REJECTED) {
                // Nothing ran: the batch returns to unread for whatever
                // admits the child next, and blocks no later command.
                if let Err(settle) = super::result_context::acknowledge(
                    driver,
                    &command_text,
                    super::result_context::Submission::Refused,
                ) {
                    return Err(WakeRefusal::Failed(format!("{error}; {settle}")));
                }
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
        super::result_context::acknowledge(
            driver,
            &command_text,
            super::result_context::Submission::Recorded,
        )
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
    for command in super::result_context::prepared_commands(driver, agent_id)? {
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
                    super::result_context::prepared_commands(runtime.driver(), agent_id)?;
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
                        super::children::ChildCommand::Fresh(
                            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT.to_owned(),
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
            super::result_context::project_feed(runtime, agent_id, &sessions, session_id).await?;
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
    Start(Option<super::result_context::JournaledCommand>),
    /// `max_turns` is spent: the attempt ends instead.
    Spent,
}

struct ReopenedResume {
    identity: ChildTurnIdentity,
    command_id: CommandId,
    gate: crate::agent::transient::ResumeGate,
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
        tree.mark_transient_wait(
            agent_id,
            resume.gate == crate::agent::transient::ResumeGate::Network,
        );
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
            resume.gate,
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
        let prompt = crate::child_prompts::TransientResume {
            failure: error.message(),
        }
        .render();
        let started = super::children::start_child_turn_owned_with_ack(
            &owner,
            &id,
            attached,
            resume.command_id,
            prompt,
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
    let driver = runtime.driver();
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
        let context = model
            .context()
            .ok_or_else(|| "saved continuation has no typed failure context".to_owned())?;
        let gate = crate::agent::transient::classify(
            model.kind(),
            context.phase(),
            context.retryability(),
        )
        .ok_or_else(|| "saved continuation is not eligible".to_owned())?;
        let attempt = runtime
            .read(|tree| {
                tree.attempt(agent_id)
                    .map(|attempt| attempt.attempt_id.clone())
            })
            .map_err(|error| error.message)?
            .ok_or_else(|| "saved continuation has no attempt".to_owned())?;
        let command_id = CommandId::new(format!(
            "{}{agent_id}:{attempt}:{round}",
            super::result_context::TRANSIENT_RESUME_PREFIX
        ))
        .map_err(|error| error.to_string())?;
        for prior in super::result_context::prepared_commands(driver, agent_id)? {
            // The exact pending Resume keeps its original prompt and mail;
            // withdrawing it would make an accepted retry impossible to bind.
            if prior.command_id != command_id.as_str() {
                prior.settle(driver, false)?;
            }
        }
        return Ok(ReopenedWake::Resume(ReopenedResume {
            identity: ChildTurnIdentity { attempt, turn },
            command_id,
            gate,
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
    for command in super::result_context::prepared_commands(driver, agent_id)? {
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
    let driver = runtime.driver();
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
    let driver = runtime.driver();
    super::result_context::project_child_feed(runtime, agent_id, attached).await?;
    for command in super::result_context::prepared_commands(driver, agent_id)? {
        command.settle(driver, false)?;
    }
    super::result_context::accepted_command(driver, agent_id, turn)?.ok_or_else(|| {
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
    let _ = super::result_context::project_feed(runtime, agent_id, sessions, session_id).await;
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
#[allow(clippy::unwrap_used, reason = "offline park fixtures")]
mod tests {
    use std::time::Duration;

    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
    use serde_json::json;

    use super::*;
    use crate::agent::AttachedCodingSession;
    use crate::agent::live::ProductLiveChild;
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{
        AttemptStatus, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest,
        TemplateName,
    };
    use crate::topology_runtime::ProductTopologyRuntime;

    struct Fixture {
        provider_gates: Vec<plakous_offline_crash_harness::gate::GateHandle>,
        root: tempfile::TempDir,
        runtime: Arc<ProductTopologyRuntime>,
        agent: plakous::agent::Agent,
        registry: Arc<crate::jobs::JobRegistry>,
        sessions: Arc<AgentSessions>,
        binding: Box<dyn Fn() -> plakous::sessions::SessionRuntimeBinding + Send + Sync>,
        child: String,
    }

    impl Fixture {
        async fn until(&self, what: &str, done: impl Fn(&ProductAgentTree, &str) -> bool) {
            let changed = self.runtime.read(ProductAgentTree::waiters).unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let notified = changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.runtime.read(|tree| done(tree, &self.child)).unwrap() {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for {what}: {:?}",
                    self.runtime.read(|tree| (
                        tree.attempt(&self.child).cloned(),
                        tree.waiting_on(&self.child).cloned(),
                        tree.fault_reason(&self.child).map(str::to_owned),
                    ))
                )
            });
        }

        /// The exact wake prompts the result-context journal admitted.
        fn wake_prompts(&self) -> Vec<String> {
            self.runtime
                .read(|tree| {
                    tree.commands()
                        .values()
                        .filter(|command| {
                            command.recipient == self.child
                                && command.kind == crate::agent_tree::AgentCommandKind::Wake
                        })
                        .map(|command| command.text.clone())
                        .collect()
                })
                .unwrap()
        }

        fn mutate<R>(&self, work: impl FnOnce(&mut ProductAgentTree) -> R) -> R {
            let mut out = None;
            self.runtime
                .mutate_then_emit(false, |tree| {
                    out = Some(work(tree));
                    Ok(())
                })
                .unwrap();
            out.unwrap()
        }

        fn submit(&self) {
            self.mutate(|tree| {
                tree.submit_result(
                    &self.child,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.to_owned(),
                    },
                    None,
                )
                .unwrap();
            });
            for gate in &self.provider_gates {
                gate.release();
            }
        }
    }

    fn parked(tree: &ProductAgentTree, child: &str) -> bool {
        tree.waiting_on(child).is_some()
    }

    fn turns(count: u32) -> impl Fn(&ProductAgentTree, &str) -> bool {
        // Acceptance is durable; the projected live turn can already be gone
        // when a loaded test next polls, even though the wake ran successfully.
        move |tree, child| {
            tree.attempt(child)
                .is_some_and(|attempt| attempt.usage.turns >= count)
        }
    }

    fn settled(tree: &ProductAgentTree, child: &str) -> bool {
        tree.attempt(child)
            .is_some_and(|attempt| attempt.status.is_stopped())
    }

    /// One child with a real scripted session whose first turn is admitted
    /// and handed to the production turn waiter, exactly as a spawn does.
    async fn fixture(
        steps: serde_json::Value,
        setup: impl FnOnce(&mut ProductAgentTree) -> String,
    ) -> Fixture {
        fixture_with_probe(steps, setup, None).await
    }

    async fn fixture_with_probe(
        mut steps: serde_json::Value,
        setup: impl FnOnce(&mut ProductAgentTree) -> String,
        probe: Option<plakous::model::ReachabilityProbe>,
    ) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(ProductTopologyRuntime::new(
            crate::topology_runtime::test_support::bootstrap("park-wake", root.path().to_owned()),
            crate::topology_runtime::test_support::memory_store(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ));
        let mut child = None;
        runtime
            .mutate_then_emit(false, |tree| {
                child = Some(setup(tree));
                Ok(())
            })
            .unwrap();
        let child = child.unwrap();
        let mut provider_gates = Vec::new();
        for step in steps.as_array_mut().unwrap() {
            if let Some(stream) = step
                .get_mut("stream")
                .and_then(serde_json::Value::as_array_mut)
            {
                for event in stream {
                    if event.get("gate").is_some() {
                        let provider_gate = plakous_offline_crash_harness::gate::GateHandle::new();
                        event["gate"] = serde_json::to_value(provider_gate.script()).unwrap();
                        provider_gates.push(provider_gate);
                    }
                }
            }
        }
        let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
            serde_json::from_value(json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "process_tools": true,
                "steps": steps,
            }))
            .unwrap(),
        );
        let registry = Arc::new(crate::jobs::JobRegistry::default());
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("park.jsonl"),
        )
        .unwrap();
        let sessions = Arc::new(
            AgentSessions::new(SessionBackend::sqlite(root.path().join("park.sqlite3")).unwrap())
                .unwrap(),
        );
        let binding = {
            let (path, registry) = (root.path().to_owned(), registry.clone());
            let cursor = probe
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(1)));
            move || {
                let binding = if let Some(cursor) = &cursor {
                    plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
                        &path,
                        controller.clone(),
                        plan.clone(),
                        cursor.clone(),
                    )
                } else {
                    scripted_runtime_binding(&path, controller.clone(), plan.clone())
                };
                binding.unwrap().with_job_host(registry.clone())
            }
        };
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("park:create").unwrap(),
                binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("created child session required")
        };
        runtime
            .mutate_then_emit(false, |tree| {
                tree.record_child_session(&child, &session.locator().to_string())
                    .map_err(|error| error.message)?;
                let attempt = tree.attempt(&child).unwrap().attempt_id.clone();
                tree.record_command(crate::agent_tree::AgentCommand::intent(
                    "park:assignment",
                    &child,
                    Some(attempt),
                    crate::agent_tree::AgentCommandKind::Start,
                    "assignment",
                    Vec::new(),
                ))
                .map_err(|error| error.message)
            })
            .unwrap();
        let agent = session.agent().clone();
        let registration =
            super::super::children::TurnWaiterRegistration::register(runtime.driver(), &child)
                .unwrap();
        let submission = agent
            .start(CommandId::new("park:assignment").unwrap(), "assignment")
            .await
            .unwrap();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        let control = submission.turn().unwrap().control();
        let locator = session.locator();
        let initial_feed = super::super::read_tree_feed(&sessions, locator.session_id(), 0)
            .await
            .unwrap();
        runtime
            .mutate_then_emit(false, |tree| {
                initial_feed
                    .apply(tree, &child)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })
            .unwrap();
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: probe,
            sessions: sessions.clone(),
            locator,
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        };
        let feed = super::super::children::ChildFeedSource::of(&attached);
        runtime
            .try_occupy_live(
                child.clone(),
                Box::new(ProductLiveChild::new(attached, turn, control)),
            )
            .await
            .map_err(|_| "live slot refused")
            .unwrap();
        super::super::children::spawn_topology_turn_waiter(
            registration,
            Arc::clone(&runtime.live),
            None,
            Some(feed),
            result,
        );
        Fixture {
            provider_gates,
            root,
            runtime,
            agent,
            registry,
            sessions,
            binding: Box::new(binding),
            child,
        }
    }

    fn start_job(command: &str) -> serde_json::Value {
        json!({"output": {"tool": {"name": "process_session_start", "arguments": {
            "command": command, "workdir": ".", "login": false, "tty": false,
            "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192
        }}}, "finish": "tool_calls"})
    }

    fn leaf(tree: &mut ProductAgentTree) -> String {
        tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap()
            .agent_id
    }

    /// Hold the final stream until the test submits or cancels the model.
    fn held(text: &str) -> serde_json::Value {
        json!({"stream": [{"gate": {"registry": "test-held-response"}}], "output": {"text": text}})
    }

    #[tokio::test]
    async fn transient_failure_continues_same_attempt_after_exact_route_probe() {
        use plakous::model::{ReachabilityProbe, RouteId};
        let entered = Arc::new(tokio::sync::Notify::new());
        let online = Arc::new(tokio::sync::Notify::new());
        let probe = ReachabilityProbe::new(RouteId::new("offline.loopback").unwrap(), {
            let entered = entered.clone();
            let online = online.clone();
            move |_, _| {
                let online = online.clone();
                entered.notify_one();
                Box::pin(async move {
                    online.notified().await;
                    Ok(())
                })
            }
        });
        let fixture = fixture_with_probe(json!([
            {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "offline network failure"}, "output": {"text": "unused"}},
            held("continued")
        ]), leaf, Some(probe)).await;
        entered.notified().await;
        let (attempt_id, token) = fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.status, AttemptStatus::Running);
                assert!(
                    tree.is_parked(&fixture.child),
                    "mail must not start a second turn beside a pending continuation"
                );
                assert!(tree.waiting_on(&fixture.child).unwrap().network);
                let listed = tree
                    .list_agents(
                        "root",
                        &crate::agent_tree_protocol::ListAgentsRequest {
                            include_read: None,
                            include_self: None,
                            include_completed: None,
                        },
                    )
                    .unwrap();
                assert_eq!(listed.agents[0]["waiting_on"]["network"], true);
                assert_eq!(
                    tree.transient_resume_count(&fixture.child, &attempt.attempt_id)
                        .unwrap(),
                    1
                );
                assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
                (attempt.attempt_id.clone(), attempt.turn_token)
            })
            .unwrap();
        online.notify_one();
        fixture.provider_gates[0].entered().await;
        fixture.until("continuation acceptance", turns(2)).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.attempt_id, attempt_id);
                assert_eq!(attempt.turn_token, token);
                let id = format!("plak:resume-transient:{}:{attempt_id}:1", fixture.child);
                let command = tree.command(&id).unwrap();
                assert_eq!(command.kind, crate::agent_tree::AgentCommandKind::Resume);
                assert_eq!(command.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(command.text.contains("offline network failure"));
                assert!(command.text.contains("Do not repeat completed actions"));
            })
            .unwrap();
        fixture.submit();
        fixture.provider_gates[0].release();
        fixture.until("submitted continuation", settled).await;
        assert_eq!(
            fixture
                .runtime
                .read(|tree| tree.attempt(&fixture.child).unwrap().status)
                .unwrap(),
            AttemptStatus::Completed
        );
    }

    #[tokio::test]
    async fn transient_resume_exhaustion_preserves_failure_without_corrective_rounds() {
        let fault = json!({"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "original outage"}, "output": {"text": "unused"}});
        let probe = plakous::model::ReachabilityProbe::new(
            plakous::model::RouteId::new("offline.exhaustion").unwrap(),
            |_, _| Box::pin(async { Ok(()) }),
        );
        let fixture = fixture_with_probe(json!([fault.clone(), fault]), leaf, Some(probe)).await;
        fixture.until("exhausted failure", settled).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.status, AttemptStatus::Failed);
                assert!(
                    attempt
                        .failure
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("original outage")
                );
                assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
                assert_eq!(attempt.usage.turns, 2);
                assert_eq!(
                    tree.transient_resume_count(&fixture.child, &attempt.attempt_id)
                        .unwrap(),
                    1
                );
                assert_eq!(
                    tree.commands()
                        .values()
                        .filter(
                            |command| command.kind == crate::agent_tree::AgentCommandKind::Resume
                        )
                        .count(),
                    1
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn reopened_transient_resume_rederives_gate_and_exact_command_identity() {
        assert_reopened_transient_resume(false).await;
    }

    #[tokio::test]
    async fn reopened_transient_resume_retries_prepared_command_without_withdrawal() {
        assert_reopened_transient_resume(true).await;
    }

    async fn assert_reopened_transient_resume(journaled: bool) {
        let route = if journaled {
            "offline.reopen.prepared"
        } else {
            "offline.reopen.fresh"
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let probe =
            plakous::model::ReachabilityProbe::new(plakous::model::RouteId::new(route).unwrap(), {
                let entered = entered.clone();
                move |_, _| {
                    entered.notify_one();
                    Box::pin(std::future::pending())
                }
            });
        let fixture = fixture_with_probe(json!([
            {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "retained original outage"}, "output": {"text": "unused"}}, held("reopened continuation")
        ]), leaf, Some(probe)).await;
        entered.notified().await;
        let attempt = fixture
            .runtime
            .read(|tree| tree.attempt(&fixture.child).unwrap().attempt_id.clone())
            .unwrap();
        let mut attached = lose_and_reopen(&fixture).await;
        attached.reachability_probe = Some(plakous::model::ReachabilityProbe::new(
            plakous::model::RouteId::new(route).unwrap(),
            |_, _| Box::pin(async { Ok(()) }),
        ));
        if journaled {
            let (turn, _) = fixture
                .runtime
                .read(|tree| {
                    tree.pending_transient_resume(&fixture.child)
                        .unwrap()
                        .unwrap()
                })
                .unwrap();
            let locator = attached.locator();
            let plakous::sessions::feed::FeedTerminalOutcome::Terminal(terminal) = fixture
                .sessions
                .feed_terminal(plakous::sessions::feed::FeedTerminalRequest {
                    session: locator.session_id(),
                    branch: locator.branch_id(),
                    checkpoint: locator.checkpoint_id(),
                    turn_id: turn.parse().unwrap(),
                })
                .await
                .unwrap()
            else {
                panic!("canonical failure required")
            };
            let plakous::agent::TurnResultOutcome::Failed(error) = terminal.result().outcome()
            else {
                panic!("failed turn required")
            };
            let prompt = crate::child_prompts::TransientResume {
                failure: error.message(),
            }
            .render();
            super::super::result_context::prepare(
                fixture.runtime.driver(),
                &fixture.child,
                &format!("plak:resume-transient:{}:{attempt}:1", fixture.child),
                &prompt,
            )
            .unwrap();
        }
        wake_held(&fixture, attached).await;
        fixture.provider_gates[0].entered().await;
        fixture
            .until("reopened continuation acceptance", turns(2))
            .await;
        fixture
            .runtime
            .read(|tree| {
                let command = tree
                    .command(&format!(
                        "plak:resume-transient:{}:{attempt}:1",
                        fixture.child
                    ))
                    .unwrap();
                assert_eq!(command.kind, crate::agent_tree::AgentCommandKind::Resume);
                assert_eq!(command.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(command.text.contains("retained original outage"));
                assert_eq!(
                    tree.transient_resume_count(&fixture.child, &attempt)
                        .unwrap(),
                    1
                );
            })
            .unwrap();
        fixture.submit();
        fixture.provider_gates[0].release();
        fixture.until("reopened submission", settled).await;
    }

    #[tokio::test]
    async fn reopened_transient_wait_interrupt_and_close_cancel_startup_without_dispatch() {
        for close in [false, true] {
            let route = if close {
                "offline.reopen.close"
            } else {
                "offline.reopen.interrupt"
            };
            let entered = Arc::new(tokio::sync::Notify::new());
            let probe = plakous::model::ReachabilityProbe::new(
                plakous::model::RouteId::new(route).unwrap(),
                {
                    let entered = entered.clone();
                    move |_, _| {
                        entered.notify_one();
                        Box::pin(std::future::pending())
                    }
                },
            );
            let fixture = fixture_with_probe(json!([
                {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted"}, "output": {"text": "unused"}}, held("must not dispatch")
            ]), leaf, Some(probe.clone())).await;
            entered.notified().await;
            let mut attached = lose_and_reopen(&fixture).await;
            attached.reachability_probe = Some(probe);
            wake_held(&fixture, attached).await;
            entered.notified().await;
            assert!(
                fixture
                    .runtime
                    .live
                    .lock()
                    .await
                    .contains_key(&fixture.child),
                "the pending gate has a hub-owned startup reservation"
            );
            fixture.mutate(|tree| {
                let request = crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                };
                if close {
                    tree.close_agent("root", &request, None).unwrap();
                } else {
                    tree.interrupt_agent("root", &request, None).unwrap();
                }
            });
            fixture.runtime.cancel_live(&fixture.child).await.unwrap();
            fixture
                .runtime
                .read(|tree| {
                    assert_eq!(
                        tree.node(&fixture.child).unwrap().status,
                        if close {
                            NodeStatus::Closed
                        } else {
                            NodeStatus::Interrupted
                        }
                    );
                    assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
                    assert!(!tree.commands().values().any(|command| command.kind
                        == crate::agent_tree::AgentCommandKind::Resume
                        && command.accepted_seq.is_some()));
                    assert!(!tree.is_parked(&fixture.child));
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_live_job_parks_the_child_and_its_terminal_wakes_it_to_submit() {
        let fixture = fixture(
            json!([start_job("while [ ! -e release-job ]; do sleep 0.01; done"),
                {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.status, AttemptStatus::Running);
                assert_eq!(
                    attempt.submit_harness_nudge.rounds(),
                    0,
                    "a park is not a nudge"
                );
                assert_eq!(tree.waiting_on(&fixture.child).unwrap().processes, ["p1"]);
                // The parent's rows say what the child waits on.
                let listed = tree
                    .list_agents(
                        "root",
                        &crate::agent_tree_protocol::ListAgentsRequest {
                            include_read: None,
                            include_self: None,
                            include_completed: None,
                        },
                    )
                    .unwrap();
                assert_eq!(
                    listed.agents[0]["waiting_on"],
                    json!({"processes": ["p1"], "agents": []})
                );
            })
            .unwrap();
        std::fs::write(fixture.root.path().join("release-job"), b"release").unwrap();
        fixture.until("the wake turn", turns(2)).await;
        let wakes = fixture.wake_prompts();
        assert_eq!(wakes, [crate::child_prompts::PLAK_CHILD_WAKE_PROMPT]);
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(
                    tree.attempt(&fixture.child).unwrap().status,
                    AttemptStatus::Completed
                );
                assert_eq!(
                    tree.attempt(&fixture.child)
                        .unwrap()
                        .submit_harness_nudge
                        .rounds(),
                    0
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_live_child_parks_the_hub_its_result_wakes_it_and_an_empty_end_is_nudged() {
        let mut grandchild = String::new();
        let fixture = fixture(
            json!([{"output": {"text": "delegated"}}, {"output": {"text": "read it"}}, held("reminded")]),
            |tree| {
                let mut snapshot = tree.snapshot.clone();
                snapshot.max_depth = 2;
                snapshot.digest = snapshot.compute_digest();
                tree.install_snapshot(snapshot);
                let hub = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                    .unwrap()
                    .agent_id;
                grandchild = tree
                    .spawn_agent(&hub, &SpawnAgentRequest::test_review(), None)
                    .unwrap()
                    .agent_id;
                hub
            },
        )
        .await;
        fixture.until("the park", parked).await;
        let waiting = fixture
            .runtime
            .read(|tree| tree.waiting_on(&fixture.child).cloned().unwrap())
            .unwrap();
        assert_eq!(waiting.agents, [grandchild.clone()]);
        assert_eq!(waiting.processes, [] as [String; 0]);
        fixture.mutate(|tree| {
            tree.submit_result(
                &grandchild,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&grandchild).unwrap();
        });
        fixture.until("the wake turn", turns(2)).await;
        let wakes = fixture.wake_prompts();
        assert_eq!(wakes.len(), 1);
        assert!(wakes[0].starts_with(crate::child_prompts::PLAK_CHILD_WAKE_PROMPT));
        assert!(wakes[0].contains("untrusted data from sub-agents"));
        assert!(wakes[0].contains(&format!("from=\"{grandchild}\" purpose=\"result\"")));
        // The wake consumed the result; with nothing left the end is nudged.
        fixture
            .until("the reminder", |tree, child| {
                tree.attempt(child)
                    .is_some_and(|attempt| attempt.submit_harness_nudge.rounds() == 1)
            })
            .await;
        assert!(
            fixture
                .runtime
                .read(|tree| tree.wake_mail(&fixture.child).is_empty())
                .unwrap()
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    #[tokio::test]
    async fn a_completed_wake_is_observable_after_its_live_turn_ends() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}},
                {"output": {"text": "noted"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        parent_mail(&fixture, "also check the docs");
        // Deliberately observe only after the fast wake has ended and parked.
        fixture
            .until("the completed wake's park", |tree, child| {
                parked(tree, child)
                    && tree
                        .attempt(child)
                        .is_some_and(|attempt| attempt.usage.turns == 2)
                    && tree.wake_mail(child).is_empty()
            })
            .await;
        assert!(
            fixture
                .runtime
                .read(|tree| turns(2)(tree, &fixture.child))
                .unwrap()
        );
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn parent_mail_wakes_a_parked_child_and_an_interrupt_keeps_its_job() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, {"output": {"text": "noted"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let sent = fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "also check the docs".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        });
        assert_eq!(sent.disposition, crate::agent_tree::MailDisposition::Queued);
        fixture.until("the wake turn", turns(2)).await;
        assert_eq!(
            fixture.wake_prompts(),
            [format!(
                "{}\n\nMessage:\nalso check the docs",
                crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
            )]
        );
        // The job still runs, so the wake turn's end parks again.
        fixture.until("the second park", parked).await;
        let status = fixture.mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        });
        assert_eq!(status, NodeStatus::Interrupted);
        fixture.runtime.cancel_live(&fixture.child).await.unwrap();
        fixture
            .until("the park to end", |tree, child| !tree.is_parked(child))
            .await;
        assert_eq!(
            fixture.agent.jobs()[0].status,
            plakous::tools::JobStatus::Running,
            "an interrupt keeps the child's processes (spec 03 §5.2)"
        );
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }

    /// A follow-up sent as the turn ended (the tree still saw it active)
    /// reaches the ended turn: it is not a delivery failure, and its mail is
    /// left unread for the wake.
    #[tokio::test]
    async fn a_follow_up_that_reaches_an_ended_turn_is_left_for_the_wake() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let error = {
            let live = fixture.runtime.live.lock().await;
            let child = live.get(&fixture.child).unwrap();
            child
                .follow_up(
                    CommandId::new("plak:mail:late:follow-up").unwrap(),
                    "one more thing".to_owned(),
                )
                .await
                .unwrap_err()
        };
        assert_eq!(error, super::super::live::TURN_ENDED);
        assert_eq!(
            super::super::children::turn_ended_or(error),
            crate::agent_tree_tools::MailDeliveryFailure::TurnEnded
        );
        assert!(
            fixture
                .runtime
                .read(|tree| tree.is_parked(&fixture.child))
                .unwrap(),
            "nothing was admitted behind the ended turn"
        );
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wake_turns_count_toward_max_turns() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, {"output": {"text": "noted"}}]),
            |tree| {
                let name = TemplateName::parse("review").unwrap();
                let mut binding = tree.snapshot.binding(&name).unwrap().clone();
                binding.max_turns = Some(2);
                tree.pin_template("review", binding);
                leaf(tree)
            },
        )
        .await;
        fixture.until("the park", parked).await;
        fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "status?".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        });
        // The wake is the second turn; its end cannot park past the ceiling.
        fixture.until("the attempt to settle", settled).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.usage.turns, 2);
                assert_eq!(attempt.status, AttemptStatus::Blocked);
                assert!(!tree.is_parked(&fixture.child));
            })
            .unwrap();
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }

    /// The owning process goes away: the parked child's live session is
    /// released and its processes stop; only durable rows remain. Returns the
    /// session reopened as the next process would.
    async fn lose_and_reopen(fixture: &Fixture) -> AttachedCodingSession {
        crate::topology_runtime::retire_live(
            fixture.runtime.driver(),
            &fixture.runtime.live,
            &fixture.child,
            None,
            false,
        )
        .await
        .unwrap();
        fixture
            .until("the park to end", |tree, child| !tree.is_parked(child))
            .await;
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
        let locator = fixture
            .runtime
            .read(|tree| tree.node(&fixture.child).unwrap().session_locator.clone())
            .unwrap();
        let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = fixture
            .sessions
            .open(plakous::sessions::OpenSessionRequest::new(
                SessionOperationId::new("park:reopen").unwrap(),
                locator.unwrap().parse().unwrap(),
                (fixture.binding)(),
            ))
            .await
            .unwrap()
        else {
            panic!("parked child session must reopen")
        };
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: fixture.sessions.clone(),
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        }
    }

    /// The reopen wake as reattach runs it: after the resume held the child.
    async fn wake_held(fixture: &Fixture, attached: AttachedCodingSession) {
        // Rebuild the allocation seed before resume takes its hold. The wake
        // preparation still owns the first retained feed/terminal pass.
        fixture.mutate(|tree| {
            tree.project_feed(
                &fixture.child,
                &attached.locator().session_id().to_string(),
                &[],
                &crate::agent_tree::TreeFeedReads::default(),
            )
            .unwrap()
        });
        fixture.mutate(|tree| tree.resume_tree().unwrap());
        assert_eq!(
            wake_reopened(&fixture.runtime, &fixture.child, attached)
                .await
                .unwrap(),
            Reopened::Done,
            "reopen state: {:?}",
            fixture
                .runtime
                .read(|tree| (
                    tree.node(&fixture.child).unwrap().clone(),
                    tree.attempt(&fixture.child).unwrap().clone(),
                    tree.fault_reason(&fixture.child).map(str::to_owned),
                    tree.commands().clone(),
                ))
                .unwrap(),
        );
    }

    /// A turn the lost process admitted before it bound, rejoined as
    /// reattach rejoins it: the resume holds the child, the child reopen
    /// reconciles its journal with the recovered turn unbound, and the
    /// rejoin binds it. Returns what the rejoin did.
    async fn rejoin_held(
        fixture: &Fixture,
        attached: AttachedCodingSession,
        control: plakous::agent::TurnControl,
        result: TurnResultHandle,
        before_rejoin: impl FnOnce(&Fixture),
    ) -> Reopened {
        let turn = result.turn_id();
        fixture.mutate(|tree| tree.resume_tree().unwrap());
        before_rejoin(fixture);
        super::super::children::reconcile_reopened_results(
            &fixture.runtime,
            &fixture.child,
            &attached,
            Some(turn),
        )
        .await
        .unwrap();
        rejoin_turn(
            &fixture.runtime,
            &fixture.child,
            attached,
            turn,
            control,
            result,
        )
        .await
        .unwrap()
    }

    fn parent_mail(fixture: &Fixture, text: &str) {
        fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: text.into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        });
    }

    #[tokio::test]
    async fn a_reopen_wakes_a_parked_child_once_with_its_stopped_process() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let targets = fixture
            .runtime
            .read(ProductAgentTree::parked_reopen_targets)
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, fixture.child);
        assert!(
            attached
                .agent()
                .unwrap()
                .jobs()
                .iter()
                .all(|job| job.status != plakous::tools::JobStatus::Running),
            "the released process is a terminal notice for the wake"
        );
        wake_held(&fixture, attached).await;
        fixture.until("the wake turn", turns(2)).await;
        assert_eq!(
            fixture.wake_prompts(),
            [crate::child_prompts::PLAK_CHILD_WAKE_PROMPT]
        );
        assert!(
            fixture
                .runtime
                .read(|tree| tree.parked_reopen_targets().is_empty())
                .unwrap()
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    #[tokio::test]
    async fn late_finalize_reads_after_acceptance_without_replaying_old_history() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let (accepted, revision, usage, cursor) = fixture
            .runtime
            .read(|tree| {
                let command = tree.command("park:assignment").unwrap();
                (
                    command.accepted_seq.unwrap(),
                    tree.revision,
                    tree.attempt(&fixture.child).unwrap().usage,
                    tree.feed_cursor(&attached.locator().session_id().to_string()),
                )
            })
            .unwrap();
        let db = rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
        db.execute(
            "DELETE FROM session_feed WHERE seq <= ?1",
            [i64::try_from(accepted).unwrap()],
        )
        .unwrap();
        db.execute("UPDATE sessions SET feed_retained_from=?1, feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed)", [i64::try_from(accepted + 1).unwrap()]).unwrap();
        let terminal = super::super::children::read_finalize_terminal(
            fixture.runtime.driver(),
            &fixture.sessions,
            attached.locator().session_id(),
            &fixture.child,
        )
        .await
        .unwrap();
        assert!(terminal.seq > accepted);
        assert!(terminal.seq <= cursor);
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(tree.revision, revision);
                assert_eq!(tree.attempt(&fixture.child).unwrap().usage, usage);
                assert_eq!(
                    tree.feed_cursor(&attached.locator().session_id().to_string()),
                    cursor
                );
            })
            .unwrap();
        db.execute(
            "DELETE FROM session_feed WHERE seq=?1",
            [i64::try_from(terminal.seq).unwrap()],
        )
        .unwrap();
        assert!(
            super::super::children::read_finalize_terminal(
                fixture.runtime.driver(),
                &fixture.sessions,
                attached.locator().session_id(),
                &fixture.child,
            )
            .await
            .is_err()
        );
        assert_eq!(
            fixture
                .runtime
                .read(|tree| tree.attempt(&fixture.child).unwrap().status)
                .unwrap(),
            AttemptStatus::Running
        );
        attached.release_opened().await.unwrap();
    }

    /// A wake the lost process journaled but never got to the runtime is
    /// retried under its own command and exact prompt: one Start, not a
    /// second wake, and not `recovery_required`.
    #[tokio::test]
    async fn a_lost_wake_start_is_retried_exactly_after_a_reopen() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        parent_mail(&fixture, "status?");
        let lost = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap()
            .replace(":1:", ":7:");
        let prepared = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &lost,
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
        )
        .unwrap();
        wake_held(&fixture, attached).await;
        fixture.until("the wake turn", turns(2)).await;
        let wakes = fixture
            .runtime
            .read(|tree| {
                tree.commands()
                    .values()
                    .filter(|command| command.kind == crate::agent_tree::AgentCommandKind::Wake)
                    .map(|command| {
                        (
                            command.command_id.clone(),
                            if command.withdrawn {
                                "withdrawn".to_owned()
                            } else {
                                format!("{:?}", command.fact)
                            },
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(wakes.len(), 1, "no second wake beside the journaled one");
        assert!(wakes[0].0.ends_with(&lost));
        assert_eq!(wakes[0].1, "Some(Accepted)");
        assert_eq!(fixture.wake_prompts(), [prepared.prompt]);
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    #[tokio::test]
    async fn behind_retention_retry_survives_a_consumed_command_and_a_caught_up_cursor() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let first = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let stem = first.rsplit_once(':').unwrap().0;
        let ids = [
            format!("{stem}:7"),
            format!("{stem}:8"),
            format!("{stem}:9"),
        ];
        for id in &ids {
            super::super::result_context::prepare(
                fixture.runtime.driver(),
                &fixture.child,
                id,
                "exact lost wake",
            )
            .unwrap();
        }
        fixture.mutate(ProductAgentTree::clear_feed_views);
        // Retain only the release/open suffix, not the assignment's facts.
        let connection =
            rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
        connection
            .execute_batch(
                "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
             UPDATE sessions SET feed_retained_from=feed_tail,\
             feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
            )
            .unwrap();
        let expected_commands =
            std::iter::once("park:assignment".to_owned()).chain(ids.iter().cloned());
        for expected in expected_commands {
            let ReopenedWake::Start(Some(command)) =
                prepare_reopened_wake(&fixture.runtime, &fixture.child, &attached)
                    .await
                    .unwrap()
            else {
                panic!("the unresolved command must be retried")
            };
            assert_eq!(command.command_id, expected);
            command.settle(fixture.runtime.driver(), true).unwrap();
            assert!(
                fixture
                    .runtime
                    .read(|tree| ids.iter().all(|id| !tree.command(id).unwrap().withdrawn))
                    .unwrap()
            );
        }
        attached.release_opened().await.unwrap();
    }

    #[tokio::test]
    async fn reopened_wake_consumes_the_whole_retention_queue_on_one_attachment() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}},
                {"output": {"text": "lost a"}}, {"output": {"text": "lost b"}},
                {"output": {"text": "lost c"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let base = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let stem = base.rsplit_once(':').unwrap().0;
        let ids = [base.clone(), format!("{stem}:2"), format!("{stem}:3")];
        for id in &ids {
            let context = super::super::result_context::prepare(
                fixture.runtime.driver(),
                &fixture.child,
                id,
                "exact lost wake",
            )
            .unwrap();
            let submission = attached
                .agent()
                .unwrap()
                .start_instruction(CommandId::new(id.clone()).unwrap(), context.prompt)
                .await
                .unwrap();
            submission.result().unwrap().clone().result().await.unwrap();
        }
        fixture.mutate(ProductAgentTree::clear_feed_views);
        let connection =
            rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
        connection
            .execute_batch(
                "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
             UPDATE sessions SET feed_retained_from=feed_tail,\
             feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
            )
            .unwrap();
        wake_held(&fixture, attached).await;
        fixture
            .until("the fresh wake after consumed commands", turns(1))
            .await;
        fixture
            .runtime
            .read(|tree| {
                for id in &ids {
                    let intent = tree.command(id).unwrap();
                    assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                    assert!(intent.turn_id.is_none());
                    assert!(!intent.withdrawn);
                }
                assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
            })
            .unwrap();
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    #[tokio::test]
    async fn reopened_wake_consumes_a_reclaimed_command_without_binding_it() {
        let mut steps = vec![
            start_job("read line"),
            json!({"output": {"text": "started"}}),
        ];
        steps.extend((0..18).map(|_| json!({"output": {"text": "retained history"}})));
        steps.push(held("woke"));
        let fixture = fixture(serde_json::Value::Array(steps), leaf).await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let id = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &id,
            "exact lost wake",
        )
        .unwrap();
        attached
            .agent()
            .unwrap()
            .start_instruction(CommandId::new(id.clone()).unwrap(), context.prompt.clone())
            .await
            .unwrap()
            .result()
            .unwrap()
            .clone()
            .result()
            .await
            .unwrap();
        for round in 0..17 {
            attached
                .agent()
                .unwrap()
                .start_instruction(
                    CommandId::new(format!("prune:{round}")).unwrap(),
                    "untracked round",
                )
                .await
                .unwrap()
                .result()
                .unwrap()
                .clone()
                .result()
                .await
                .unwrap();
        }
        let locator = attached.release_opened().await.unwrap();
        assert!(matches!(
            fixture.sessions.prune_history(locator).await.unwrap(),
            plakous::sessions::PruneHistoryOutcome::Pruned(_)
        ));
        let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = fixture
            .sessions
            .open(plakous::sessions::OpenSessionRequest::new(
                SessionOperationId::new("park:pruned:reopen").unwrap(),
                locator,
                (fixture.binding)(),
            ))
            .await
            .unwrap()
        else {
            panic!("pruned parked session must reopen");
        };
        let replay = session
            .agent()
            .command(plakous::agent::TurnCommand::StartInstruction {
                command_id: CommandId::new(id.clone()).unwrap(),
                text: context.prompt,
            })
            .await
            .unwrap_or_else(|error| panic!("reclaimed replay: {}", error.message()));
        assert_eq!(
            replay
                .acknowledgement()
                .rejection()
                .map(plakous::agent::ControlError::kind),
            Some(plakous::agent::ControlErrorKind::Reclaimed)
        );
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: Arc::clone(&fixture.sessions),
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        };
        fixture.mutate(ProductAgentTree::clear_feed_views);
        let connection =
            rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
        connection.execute_batch("DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
            UPDATE sessions SET feed_retained_from=feed_tail, feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);").unwrap();
        wake_held(&fixture, attached).await;
        fixture
            .until("the fresh wake after the reclaimed command", turns(1))
            .await;
        fixture
            .runtime
            .read(|tree| {
                let command = tree.command(&id).unwrap();
                assert_eq!(command.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(command.turn_id.is_none());
                assert!(!command.withdrawn);
                assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
            })
            .unwrap();
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A corrective round the lost process wrote but never got to the
    /// runtime is withdrawn, not retried: its round is re-derived from the
    /// attempt's state, so nothing is spent for it and one fresh wake runs.
    #[tokio::test]
    async fn an_unrecorded_corrective_round_is_withdrawn_at_the_reopen_wake() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let (corrective, rounds) = fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                (
                    format!(
                        "plak:submit-harness:{}:{}:{}:1",
                        fixture.child, tree.host_generation, attempt.attempt_id
                    ),
                    attempt.submit_harness_nudge.rounds(),
                )
            })
            .unwrap();
        super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &corrective,
            "submit your result",
        )
        .unwrap();
        wake_held(&fixture, attached).await;
        fixture.until("the wake turn", turns(2)).await;
        let (command, spent) = fixture
            .runtime
            .read(|tree| {
                (
                    tree.command(&corrective).unwrap().clone(),
                    tree.attempt(&fixture.child)
                        .unwrap()
                        .submit_harness_nudge
                        .rounds(),
                )
            })
            .unwrap();
        assert_eq!(
            command.kind,
            crate::agent_tree::AgentCommandKind::Corrective
        );
        assert!(command.withdrawn && command.fact.is_none(), "{command:?}");
        assert_eq!(spent, rounds, "the withdrawn round spends nothing");
        assert_eq!(
            fixture.wake_prompts(),
            [crate::child_prompts::PLAK_CHILD_WAKE_PROMPT.to_owned()],
            "one fresh wake, not the corrective text"
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A wait the lost process acknowledged mail through, but which never
    /// reached the child's committed history, settles before the reopen wake
    /// dispatches: the mail is unread again and that wake carries it.
    #[tokio::test]
    async fn a_reopen_settles_lost_await_receipts_before_the_wake() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        parent_mail(&fixture, "use the v2 schema");
        fixture.mutate(|tree| {
            let mail = tree.wake_mail(&fixture.child).pop().unwrap();
            tree.remember_success(
                "lost-wait",
                &fixture.child,
                "wait_agent",
                &json!({}),
                &json!({"output": {}, "acknowledged": [mail], "newly_delivered": []}),
            )
            .unwrap();
            tree.acknowledge_mail(&mail.message_id).unwrap();
            assert_eq!(
                tree.wake_mail(&fixture.child),
                [] as [plakous_recursive_subagents::tree::MailRecord; 0]
            );
        });
        wake_held(&fixture, attached).await;
        fixture.until("the wake turn", turns(2)).await;
        assert!(
            fixture
                .runtime
                .read(|tree| tree.retained_wait_receipts(&fixture.child).is_empty())
                .unwrap()
        );
        assert_eq!(
            fixture.wake_prompts(),
            [format!(
                "{}\n\nMessage:\nuse the v2 schema",
                crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
            )]
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    #[tokio::test]
    async fn a_reopen_with_max_turns_spent_ends_the_attempt_blocked() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}]),
            |tree| {
                let name = TemplateName::parse("review").unwrap();
                let mut binding = tree.snapshot.binding(&name).unwrap().clone();
                binding.max_turns = Some(2);
                tree.pin_template("review", binding);
                leaf(tree)
            },
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        // A second turn the lost process bound and spent before it parked.
        fixture.mutate(|tree| {
            tree.bind_attempt_turn(&fixture.child, "00000000-0000-0000-0000-0000000000f2")
                .unwrap();
            tree.release_attempt_turn(&fixture.child).unwrap();
        });
        wake_held(&fixture, attached).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.status, AttemptStatus::Blocked);
                assert_eq!(attempt.usage.turns, 2);
            })
            .unwrap();
        assert_eq!(fixture.wake_prompts(), [] as [String; 0]);
    }

    /// Mail that is unread when a turn ends wakes the child although nothing
    /// it started still runs: an arrival is not a nudge.
    #[tokio::test]
    async fn an_arrival_without_live_work_wakes_instead_of_nudging() {
        let fixture = fixture(json!([held("thinking"), held("woke")]), leaf).await;
        parent_mail(&fixture, "use the v2 schema");
        fixture.provider_gates[0].release();
        fixture.until("the wake turn", turns(2)).await;
        assert_eq!(
            fixture.wake_prompts(),
            [format!(
                "{}\n\nMessage:\nuse the v2 schema",
                crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
            )]
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.status, AttemptStatus::Completed);
                assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
            })
            .unwrap();
    }

    /// A mail Start the lost process journaled is retried exactly on reopen
    /// (spec 03 §14.1) and acknowledges the mail its prompt carries, so the
    /// child's next unsubmitted end does not wake it with the same message a
    /// second time.
    #[tokio::test]
    async fn a_retried_mail_start_acknowledges_its_mail() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        // The lost process accepted parent mail as a Start, journaled that
        // Start and died before the runtime recorded it.
        let sent = fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "use the v2 schema".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        });
        assert_eq!(
            sent.disposition,
            crate::agent_tree::MailDisposition::Started
        );
        let command = format!("plak:mail:{}:start", sent.message_id);
        super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            "assignment\n\nMessage:\nuse the v2 schema",
        )
        .unwrap();
        wake_held(&fixture, attached).await;
        fixture.until("the retried Start", turns(2)).await;
        let unread = fixture
            .runtime
            .read(|tree| {
                tree.wake_mail(&fixture.child)
                    .iter()
                    .any(|mail| mail.message_id == sent.message_id)
            })
            .unwrap();
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
        assert!(
            !unread,
            "the retried mail Start delivered `use the v2 schema`, yet the mail is still \
             unread for the next wake to deliver again"
        );
    }

    /// An interrupt that lands after the turn ended but before the waiter
    /// parked takes the interrupted-turn path: the bound turn is released,
    /// so the next mail starts the resume turn (spec 03 §9) instead of
    /// queueing behind a turn that no longer exists.
    #[tokio::test]
    async fn an_interrupt_racing_the_park_decision_releases_the_bound_turn() {
        let fixture = fixture(json!([start_job("read line"), held("started")]), leaf).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            while !fixture
                .agent
                .jobs()
                .iter()
                .any(|job| job.status == plakous::tools::JobStatus::Running)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Holding the live table stops the waiter at its park decision, after
        // it classified the ended turn as an ordinary completion.
        fixture.provider_gates[0].entered().await;
        let held = fixture.runtime.live.lock().await;
        fixture.provider_gates[0].release();
        fixture
            .until("the turn end", |tree, child| {
                tree.attempt(child)
                    .is_some_and(|attempt| attempt.usage.tool_calls >= 1)
            })
            .await;
        let status = fixture.mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        });
        assert_eq!(status, NodeStatus::Interrupted);
        drop(held);
        fixture
            .until("the park to end", |tree, child| {
                !tree.has_turn_owner(child) && !tree.is_parked(child)
            })
            .await;
        let bound = fixture
            .runtime
            .read(|tree| tree.attempt(&fixture.child).unwrap().turn_id.clone())
            .unwrap();
        let sent = fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "resume".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        });
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
        assert_eq!(
            bound, None,
            "the interrupted child still binds the turn that already ended"
        );
        assert_eq!(
            sent.disposition,
            crate::agent_tree::MailDisposition::Started,
            "the resume mail must start a fresh turn on the interrupted attempt"
        );
    }

    /// An interrupt that lands after a wake began but before its Start is
    /// admitted ends the park instead of the attempt: nothing starts, the
    /// batch the wake prepared stays unread, the attempt stays open, and the
    /// next mail resumes the child (spec 03 §9).
    #[tokio::test]
    async fn an_interrupt_racing_the_wake_admission_keeps_the_attempt() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let runtime = Arc::clone(&fixture.runtime);
        let child = fixture.child.clone();
        hooks::set_before_wake_admission(
            &fixture.child,
            Arc::new(move || {
                let (runtime, child) = (Arc::clone(&runtime), child.clone());
                Box::pin(async move {
                    runtime
                        .mutate_then_emit(false, |tree| {
                            tree.interrupt_agent(
                                "root",
                                &crate::agent_tree_protocol::AttemptScopedRequest {
                                    agent_id: child.clone(),
                                    expected_attempt_id: None,
                                },
                                None,
                            )
                            .map(|_| ())
                            .map_err(|error| error.message)
                        })
                        .unwrap();
                    runtime.cancel_live(&child).await.unwrap();
                })
            }),
        );
        parent_mail(&fixture, "status?");
        fixture
            .until("the wake batch to be released", |tree, child| {
                tree.commands().values().any(|command| {
                    command.recipient == child
                        && command.kind == crate::agent_tree::AgentCommandKind::Wake
                        && command.withdrawn
                })
            })
            .await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(
                    tree.node(&fixture.child).unwrap().status,
                    NodeStatus::Interrupted
                );
                assert_eq!(attempt.status, AttemptStatus::Running);
                assert_eq!(attempt.turn_id, None);
                assert_eq!(attempt.usage.turns, 1, "no wake turn was admitted");
                assert_eq!(
                    tree.wake_mail(&fixture.child).len(),
                    1,
                    "the batch stays unread"
                );
            })
            .unwrap();
        assert!(
            super::super::result_context::prepared_commands(
                fixture.runtime.driver(),
                &fixture.child
            )
            .unwrap()
            .is_empty(),
            "the released wake batch blocks no later command"
        );
        assert_eq!(
            fixture.agent.jobs()[0].status,
            plakous::tools::JobStatus::Running,
            "an interrupt keeps the child's processes"
        );
        let resumed = fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "resume".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        });
        assert_eq!(
            resumed.disposition,
            crate::agent_tree::MailDisposition::Started
        );
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }

    /// While a wake that was not admitted ends the attempt, the child is
    /// marked ending: new mail is refused as terminal rather than started,
    /// an interrupt no longer applies, and a Start that reached the live
    /// session is not joined to a turn the tree no longer binds.
    #[tokio::test]
    async fn mail_during_a_failed_wake_end_is_refused_not_started() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        // A result recorded while parked: the next wake is refused, as for a
        // spent or terminal attempt.
        fixture.submit();
        let observed = Arc::new(std::sync::Mutex::new(None));
        let runtime = Arc::clone(&fixture.runtime);
        let child = fixture.child.clone();
        let seen = Arc::clone(&observed);
        hooks::set_before_failed_wake_end(
            &fixture.child,
            Arc::new(move || {
                let (runtime, child, seen) =
                    (Arc::clone(&runtime), child.clone(), Arc::clone(&seen));
                Box::pin(async move {
                    let mut sent = None;
                    let mut status = None;
                    runtime
                        .mutate_then_emit(false, |tree| {
                            sent = Some(
                                tree.send_agent_message(
                                    "root",
                                    &SendAgentMessageRequest {
                                        agent_id: child.clone(),
                                        message: "one more thing".into(),
                                        purpose: None,
                                        priority: None,
                                        in_reply_to: None,
                                        output_schema_json: None,
                                    },
                                    None,
                                )
                                .map_err(|error| error.code),
                            );
                            status = tree
                                .interrupt_agent(
                                    "root",
                                    &crate::agent_tree_protocol::AttemptScopedRequest {
                                        agent_id: child.clone(),
                                        expected_attempt_id: None,
                                    },
                                    None,
                                )
                                .ok();
                            Ok(())
                        })
                        .unwrap();
                    let (locator, path) = runtime
                        .read(|tree| {
                            let node = tree.node(&child).unwrap();
                            (
                                node.session_locator.clone().unwrap(),
                                node.path.display().to_string(),
                            )
                        })
                        .unwrap();
                    let deliverer = super::super::children::ProductMailDeliverer {
                        parent: crate::agent::SpawnParent::Api(
                            super::super::tests::openrouter_test_parent(),
                        ),
                        runtime: Arc::clone(&runtime),
                    };
                    let started = crate::agent_tree_tools::MailDeliverer::deliver(
                        &deliverer,
                        crate::agent_tree_tools::MailDeliveryRequest {
                            agent_id: child.clone(),
                            session_locator: locator,
                            disposition: crate::agent_tree::MailDisposition::Started,
                            message_id: "m-late".into(),
                            body: "late".into(),
                            description: String::new(),
                            prompt: String::new(),
                            output_schema_json: String::new(),
                            path,
                            turn_token: 1,
                        },
                    )
                    .await;
                    *seen.lock().unwrap() = Some((sent, status, started));
                })
            }),
        );
        parent_mail(&fixture, "status?");
        fixture.until("the attempt to settle", settled).await;
        let (sent, status, started) = observed.lock().unwrap().take().unwrap();
        assert_eq!(
            sent.unwrap().unwrap_err(),
            crate::agent_tree_protocol::TopologyErrorCode::RecipientAttemptTerminal
        );
        assert_eq!(
            status,
            Some(NodeStatus::Running),
            "an ending child is not interrupted"
        );
        assert_eq!(
            started.unwrap_err(),
            crate::agent_tree_tools::MailDeliveryFailure::TurnEnded
        );
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(
                    tree.attempt(&fixture.child).unwrap().status,
                    AttemptStatus::Completed
                );
                assert!(!tree.is_parked(&fixture.child));
                // The mail that waited for the wake is settled and named,
                // not left unread on an attempt nothing will wake.
                assert_eq!(
                    tree.wake_mail(&fixture.child),
                    [] as [plakous_recursive_subagents::tree::MailRecord; 0]
                );
                let fault = tree.fault_reason(&fixture.child).unwrap_or_default();
                assert!(fault.contains("was not delivered"), "{fault}");
            })
            .unwrap();
    }

    /// Delivers `message_id` to the fixture's child as a Started mail, as
    /// a send to a child with no turn does.
    async fn deliver_started(
        fixture: &Fixture,
        message_id: &str,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        let (locator, path) = fixture
            .runtime
            .read(|tree| {
                let node = tree.node(&fixture.child).unwrap();
                (
                    node.session_locator.clone().unwrap(),
                    node.path.display().to_string(),
                )
            })
            .unwrap();
        let deliverer = super::super::children::ProductMailDeliverer {
            parent: crate::agent::SpawnParent::Api(super::super::tests::openrouter_test_parent()),
            runtime: Arc::clone(&fixture.runtime),
        };
        crate::agent_tree_tools::MailDeliverer::deliver(
            &deliverer,
            crate::agent_tree_tools::MailDeliveryRequest {
                agent_id: fixture.child.clone(),
                session_locator: locator,
                disposition: crate::agent_tree::MailDisposition::Started,
                message_id: message_id.into(),
                body: "use the v2 schema".into(),
                description: String::new(),
                prompt: String::new(),
                output_schema_json: String::new(),
                path,
                turn_token: 1,
            },
        )
        .await
    }

    /// A Started mail never joins a turn another command admitted as a
    /// Start: it would be acknowledged, and its mail settled, without ever
    /// reaching that turn. The send hears `TurnEnded` and delivers the mail
    /// into that turn as a follow-up.
    #[tokio::test]
    async fn a_started_mail_never_joins_a_turn_another_command_admitted() {
        let fixture = fixture(json!([held("working")]), leaf).await;
        let delivered = deliver_started(&fixture, "m-late-start").await;
        assert_eq!(
            delivered.map(|ack| ack.turn_id),
            Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded),
            "a Started mail was acknowledged by a turn its command did not admit"
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A Started mail that reaches an interrupted child while another Start
    /// holds its live slot answers `SlotBusy`, so the send retries once the
    /// slot is vacant, instead of reopening the session and faulting the
    /// node when its own Start finds the slot taken.
    #[tokio::test]
    async fn a_mail_start_on_a_starting_slot_is_retried_not_reopened() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        fixture.mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        });
        fixture.runtime.cancel_live(&fixture.child).await.unwrap();
        fixture
            .until("the interrupted turn to unwind", |tree, child| {
                !tree.is_parked(child)
                    && tree
                        .attempt(child)
                        .is_some_and(|attempt| attempt.turn_id.is_none())
            })
            .await;
        let startup = fixture
            .runtime
            .reserve_startup(&fixture.child)
            .await
            .unwrap();
        let delivered = deliver_started(&fixture, "m-resume").await;
        let fault = fixture
            .runtime
            .read(|tree| tree.fault_reason(&fixture.child).map(str::to_owned))
            .unwrap();
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
        assert_eq!(
            delivered.map(|ack| ack.turn_id),
            Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy)
        );
        assert_eq!(fault, None, "the starting slot faulted the node");
        drop(startup);
    }

    /// A Start whose reservation finds the child's slot held by another
    /// Start (taken while a mail Start reopened the session) is refused as
    /// `SlotOccupied`: its attachment is released and the child is not
    /// faulted, so the mail Start answers `SlotBusy` and its send retries.
    #[tokio::test]
    async fn a_start_whose_slot_is_taken_before_its_reservation_does_not_fault_the_child() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let startup = fixture
            .runtime
            .reserve_startup(&fixture.child)
            .await
            .unwrap();
        let started = super::super::children::start_child_turn(
            &fixture.runtime,
            &fixture.child,
            attached,
            CommandId::new("plak:mail:m-taken:start").unwrap(),
            "assignment",
        )
        .await;
        let (fault, status) = fixture
            .runtime
            .read(|tree| {
                (
                    tree.fault_reason(&fixture.child).map(str::to_owned),
                    tree.node(&fixture.child).unwrap().status,
                )
            })
            .unwrap();
        assert!(
            matches!(
                started,
                Err(super::super::children::ChildStartFailure::SlotOccupied(_))
            ),
            "the taken slot was not a distinct refusal"
        );
        assert_eq!(fault, None, "the taken slot faulted the node");
        assert_ne!(status, NodeStatus::RecoveryRequired);
        drop(startup);
    }

    /// Mail whose live delivery is still in flight when the turn ends (it
    /// targeted the turn that just ended) parks the child instead of nudging
    /// it, and the wake runs once the delivery returns, carrying the mail.
    #[tokio::test]
    async fn mail_claimed_at_turn_end_parks_and_wakes_when_its_delivery_returns() {
        let fixture = fixture(json!([held("thinking"), held("woke")]), leaf).await;
        parent_mail(&fixture, "use the v2 schema");
        let message = fixture.mutate(|tree| {
            let mail = tree.wake_mail(&fixture.child).pop().unwrap();
            tree.set_delivery_claim_for_test(&mail.message_id, true);
            mail.message_id
        });
        fixture.provider_gates[0].release();
        fixture.until("the park", parked).await;
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.usage.turns, 1);
                assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
            })
            .unwrap();
        // The delivery found the turn ended and left the mail unread.
        fixture.mutate(|tree| tree.set_delivery_claim_for_test(&message, false));
        fixture.until("the wake turn", turns(2)).await;
        assert_eq!(
            fixture.wake_prompts(),
            [format!(
                "{}\n\nMessage:\nuse the v2 schema",
                crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
            )]
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
        assert_eq!(
            fixture
                .runtime
                .read(|tree| tree
                    .attempt(&fixture.child)
                    .unwrap()
                    .submit_harness_nudge
                    .rounds())
                .unwrap(),
            0
        );
    }

    /// A wake an earlier attempt journaled is never retried onto the
    /// current attempt after a reopen; it settles, and a fresh wake runs.
    #[tokio::test]
    async fn a_reopen_does_not_retry_another_attempts_wake() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let current = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let attempt = fixture
            .runtime
            .read(|tree| {
                tree.attempt(&fixture.child)
                    .unwrap()
                    .attempt_id
                    .as_str()
                    .to_owned()
            })
            .unwrap();
        assert_eq!(
            super::super::result_context::command_attempt(&current),
            Some(attempt.as_str())
        );
        let stale = current.replace(&format!(":{attempt}:"), ":c1-a0:");
        super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &stale,
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
        )
        .unwrap();
        wake_held(&fixture, attached).await;
        fixture.until("the wake turn", turns(2)).await;
        let wakes = fixture
            .runtime
            .read(|tree| {
                tree.commands()
                    .values()
                    .filter(|command| command.kind == crate::agent_tree::AgentCommandKind::Wake)
                    .map(|command| {
                        (
                            command.command_id.clone(),
                            if command.withdrawn {
                                "withdrawn".to_owned()
                            } else {
                                format!("{:?}", command.fact)
                            },
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(wakes.len(), 2);
        for (operation, response) in wakes {
            if operation.ends_with(&stale) {
                assert_eq!(response, "withdrawn");
            } else {
                assert!(operation.ends_with(&current));
                assert_eq!(response, "Some(Accepted)");
            }
        }
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A mail Start the lost process admitted before it bound its turn
    /// rejoins after the child reopen (spec 03 section 14.1): the reopen's
    /// result reconcile leaves its command prepared, so the rejoin finds it,
    /// binds the turn and acknowledges the mail it carried.
    #[tokio::test]
    async fn an_admitted_mail_start_rejoins_after_the_reopen_reconcile() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let sent = fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "use the v2 schema".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        });
        assert_eq!(
            sent.disposition,
            crate::agent_tree::MailDisposition::Started
        );
        let command = format!("plak:mail:{}:start", sent.message_id);
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            "assignment\n\nMessage:\nuse the v2 schema",
        )
        .unwrap();
        // The lost process admitted the Start and died before binding its turn.
        let submission = attached
            .agent()
            .unwrap()
            .start(
                CommandId::new(command.clone()).unwrap(),
                context.prompt.clone(),
            )
            .await
            .unwrap();
        let control = submission.turn().unwrap().control();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        let rejoined = rejoin_held(&fixture, attached, control, result, |_| {}).await;
        assert_eq!(rejoined, Reopened::Done);
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.turn_id, Some(turn.to_string()));
                assert_eq!(attempt.usage.turns, 2);
                assert!(!tree.is_parked(&fixture.child));
                assert!(
                    tree.wake_mail(&fixture.child)
                        .iter()
                        .all(|mail| mail.message_id != sent.message_id),
                    "the rejoined Start acknowledged the mail it carried"
                );
            })
            .unwrap();
        assert!(
            super::super::result_context::prepared_commands(
                fixture.runtime.driver(),
                &fixture.child
            )
            .unwrap()
            .is_empty()
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A mail Start whose reopen recovers a turn another command admitted
    /// (here another mail's Start the lost process journaled) never claims
    /// that turn: the turn binds and goes live with its own batch settled,
    /// and the delivery answers `TurnEnded`, so the send follows up into it
    /// and the mail is not acknowledged by a turn that did not carry it.
    #[tokio::test]
    async fn a_mail_start_never_claims_a_recovered_turn_another_command_admitted() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let send = |text: &str| {
            fixture.mutate(|tree| {
                tree.send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        agent_id: fixture.child.clone(),
                        message: text.into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap()
            })
        };
        let journaled = send("use the v2 schema");
        let command = format!("plak:mail:{}:start", journaled.message_id);
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            "assignment\n\nMessage:\nuse the v2 schema",
        )
        .unwrap();
        // The lost process admitted that Start and died before binding it.
        let submission = attached
            .agent()
            .unwrap()
            .start(CommandId::new(command).unwrap(), context.prompt.clone())
            .await
            .unwrap();
        let control = submission.turn().unwrap().control();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        let late = send("keep the old field too");
        assert_eq!(
            late.disposition,
            crate::agent_tree::MailDisposition::Started
        );
        super::super::children::reconcile_reopened_results(
            &fixture.runtime,
            &fixture.child,
            &attached,
            Some(turn),
        )
        .await
        .unwrap();
        let rejoined = super::super::children::rejoin_recovered_start(
            &fixture.runtime,
            &fixture.child,
            &late.message_id,
            attached,
            turn,
            control,
            result,
        )
        .await;
        assert_eq!(
            rejoined.map(|ack| ack.turn_id),
            Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded),
            "the late mail's Start was acknowledged by a turn its command did not admit"
        );
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(attempt.turn_id, Some(turn.to_string()));
                assert!(tree.node(&fixture.child).unwrap().active_turn);
                let unread: Vec<_> = tree
                    .wake_mail(&fixture.child)
                    .into_iter()
                    .map(|mail| mail.message_id)
                    .collect();
                assert_eq!(unread, std::slice::from_ref(&late.message_id));
            })
            .unwrap();
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A recovered turn binds to the attempt before its waiter can observe
    /// it: a turn that ends while the rejoin is still returning settles as
    /// the bound turn, and the rejoin never marks the ended turn active
    /// again (spec 03 section 14.1).
    #[tokio::test]
    async fn a_recovered_turn_that_ends_before_the_rejoin_returns_stays_ended() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let send = |text: &str| {
            fixture.mutate(|tree| {
                tree.send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        agent_id: fixture.child.clone(),
                        message: text.into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap()
            })
        };
        let journaled = send("use the v2 schema");
        let command = format!("plak:mail:{}:start", journaled.message_id);
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            "assignment\n\nMessage:\nuse the v2 schema",
        )
        .unwrap();
        let submission = attached
            .agent()
            .unwrap()
            .start(CommandId::new(command).unwrap(), context.prompt.clone())
            .await
            .unwrap();
        let control = submission.turn().unwrap().control();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        let late = send("keep the old field too");
        super::super::children::reconcile_reopened_results(
            &fixture.runtime,
            &fixture.child,
            &attached,
            Some(turn),
        )
        .await
        .unwrap();
        // The turn ends, and its waiter settles it, before the rejoin returns.
        let (runtime, child) = (Arc::clone(&fixture.runtime), fixture.child.clone());
        hooks::set_after_recovered_occupy(
            &fixture.child,
            Arc::new(move || {
                let (runtime, child) = (Arc::clone(&runtime), child.clone());
                Box::pin(async move {
                    runtime
                        .mutate_then_emit(false, |tree| {
                            tree.submit_result(
                                &child,
                                &SubmitResultRequest {
                                    turn_token: 1,
                                    output_json: r#"{"ok":true}"#.to_owned(),
                                },
                                None,
                            )
                            .map(|_| ())
                            .map_err(|error| error.message)
                        })
                        .unwrap();
                    tokio::time::timeout(Duration::from_secs(30), async {
                        while !runtime.read(|tree| settled(tree, &child)).unwrap() {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .expect("the recovered turn settles");
                })
            }),
        );
        let rejoined = super::super::children::rejoin_recovered_start(
            &fixture.runtime,
            &fixture.child,
            &late.message_id,
            attached,
            turn,
            control,
            result,
        )
        .await;
        assert_eq!(
            rejoined.map(|ack| ack.turn_id),
            Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded),
            "the rejoin faulted on a turn that ended before it returned"
        );
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert!(attempt.status.is_stopped());
                assert_eq!(attempt.turn_id, None);
                assert!(
                    !tree.node(&fixture.child).unwrap().active_turn,
                    "the ended turn was marked active again"
                );
            })
            .unwrap();
    }

    /// A recovered mail-Start turn on an attempt that already binds another
    /// turn is refused (`RecoveryRequired`): the rejoin never binds over that
    /// turn, and the recovered turn is cancelled and released instead of
    /// going live (spec 03 section 14.1).
    #[tokio::test]
    async fn a_recovered_turn_on_an_attempt_bound_to_another_turn_is_refused() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        let sent = fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: fixture.child.clone(),
                    message: "use the v2 schema".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        });
        let command = format!("plak:mail:{}:start", sent.message_id);
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            "assignment\n\nMessage:\nuse the v2 schema",
        )
        .unwrap();
        let submission = attached
            .agent()
            .unwrap()
            .start(CommandId::new(command).unwrap(), context.prompt.clone())
            .await
            .unwrap();
        let control = submission.turn().unwrap().control();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        super::super::children::reconcile_reopened_results(
            &fixture.runtime,
            &fixture.child,
            &attached,
            Some(turn),
        )
        .await
        .unwrap();
        let other = "00000000-0000-0000-0000-0000000000b2";
        fixture.mutate(|tree| {
            tree.bind_attempt_turn(&fixture.child, other).unwrap();
            tree.set_active_turn(&fixture.child, true).unwrap();
        });
        let rejoined = super::super::children::rejoin_recovered_start(
            &fixture.runtime,
            &fixture.child,
            &sent.message_id,
            attached,
            turn,
            control,
            result,
        )
        .await;
        assert!(
            matches!(
                rejoined,
                Err(crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(_))
            ),
            "{rejoined:?}"
        );
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(
                    tree.attempt(&fixture.child).unwrap().turn_id.as_deref(),
                    Some(other),
                    "the rejoin bound over the attempt's turn"
                );
            })
            .unwrap();
        assert!(
            !fixture
                .runtime
                .live
                .lock()
                .await
                .contains_key(&fixture.child),
            "the refused turn went live"
        );
    }

    /// The lost process's wake carrying mail, admitted before it bound,
    /// rejoins the same way.
    #[tokio::test]
    async fn an_admitted_wake_with_mail_rejoins_after_the_reopen_reconcile() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        parent_mail(&fixture, "status?");
        let command = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
        )
        .unwrap();
        assert!(context.prompt.ends_with("\n\nMessage:\nstatus?"));
        let submission = attached
            .agent()
            .unwrap()
            .command(plakous::agent::TurnCommand::StartInstruction {
                command_id: CommandId::new(command).unwrap(),
                text: context.prompt.clone(),
            })
            .await
            .unwrap();
        let control = submission.turn().unwrap().control();
        let result = submission.result().unwrap().clone();
        let rejoined = rejoin_held(&fixture, attached, control, result, |_| {}).await;
        assert_eq!(rejoined, Reopened::Done);
        fixture.until("the rejoined wake", turns(2)).await;
        assert!(
            fixture
                .runtime
                .read(|tree| tree.wake_mail(&fixture.child).is_empty())
                .unwrap(),
            "the rejoined wake acknowledged its batch"
        );
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// An interrupt that ends a held child's hold before its reopen rejoin
    /// wins: the recovered turn is cancelled instead of bound and the node
    /// stays `interrupted`. The runtime admitted that turn, so the batch it
    /// carried is in history and settles accepted: the resume does not
    /// deliver it again.
    #[tokio::test]
    async fn an_interrupt_of_a_held_child_cancels_its_rejoined_turn() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        parent_mail(&fixture, "status?");
        let command = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let context = super::super::result_context::prepare(
            fixture.runtime.driver(),
            &fixture.child,
            &command,
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
        )
        .unwrap();
        let submission = attached
            .agent()
            .unwrap()
            .command(plakous::agent::TurnCommand::StartInstruction {
                command_id: CommandId::new(command).unwrap(),
                text: context.prompt.clone(),
            })
            .await
            .unwrap();
        let control = submission.turn().unwrap().control();
        let result = submission.result().unwrap().clone();
        let rejoined = rejoin_held(&fixture, attached, control, result, |fixture| {
            let status = fixture.mutate(|tree| {
                tree.interrupt_agent(
                    "root",
                    &crate::agent_tree_protocol::AttemptScopedRequest {
                        agent_id: fixture.child.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .unwrap()
            });
            assert_eq!(status, NodeStatus::Interrupted);
        })
        .await;
        assert_eq!(rejoined, Reopened::HoldEnded);
        fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                assert_eq!(
                    tree.node(&fixture.child).unwrap().status,
                    NodeStatus::Interrupted
                );
                assert_eq!(attempt.status, AttemptStatus::Running);
                assert_eq!(attempt.turn_id, None);
                assert_eq!(
                    attempt.usage.turns, 2,
                    "an admitted turn is never refunded when registration loses the hold"
                );
                assert!(
                    tree.wake_mail(&fixture.child).is_empty(),
                    "the cancelled rejoin's batch is still unread, so the resume would \
                     deliver mail its admitted turn already carried"
                );
            })
            .unwrap();
        assert!(
            !fixture
                .runtime
                .live
                .lock()
                .await
                .contains_key(&fixture.child)
        );
        assert!(
            super::super::result_context::prepared_commands(
                fixture.runtime.driver(),
                &fixture.child
            )
            .unwrap()
            .is_empty(),
            "the released batch blocks no later command"
        );
    }

    /// A resume holds a child a reopen finds parked, and an interrupt ends
    /// that hold: the reopen wake, which walks the targets reattach read
    /// before the interrupt, starts nothing and leaves the child
    /// `interrupted`.
    #[tokio::test]
    async fn an_interrupt_of_a_held_child_stops_its_reopen_wake() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        fixture.mutate(|tree| tree.resume_tree().unwrap());
        assert!(
            fixture
                .runtime
                .read(|tree| tree.is_parked(&fixture.child))
                .unwrap(),
            "the resume holds the child for its reopen wake"
        );
        fixture.mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        });
        let interrupted = fixture
            .runtime
            .read(|tree| {
                (
                    tree.node(&fixture.child).unwrap().status,
                    tree.is_parked(&fixture.child),
                )
            })
            .unwrap();
        assert_eq!(interrupted, (NodeStatus::Interrupted, false));
        let woke = wake_reopened(&fixture.runtime, &fixture.child, attached).await;
        let (status, turn, turns) = fixture
            .runtime
            .read(|tree| {
                let attempt = tree.attempt(&fixture.child).unwrap();
                (
                    tree.node(&fixture.child).unwrap().status,
                    attempt.turn_id.clone(),
                    attempt.usage.turns,
                )
            })
            .unwrap();
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
        assert_eq!(woke, Ok(Reopened::HoldEnded));
        assert_eq!(status, NodeStatus::Interrupted);
        assert!(
            turn.is_none() && turns == 1,
            "the reopen wake started a turn on a child the operator interrupted while it \
             was held (bound turn {turn:?}, turns {turns})"
        );
    }

    /// An interrupt that lands after the reopen wake began, while its Start
    /// is being reserved, refuses the Start: the reservation re-checks the
    /// hold under the live lock, so it never writes `running` over the
    /// interrupt, and the wake's batch stays unread.
    #[tokio::test]
    async fn an_interrupt_during_a_reopen_wake_start_is_not_overwritten() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        parent_mail(&fixture, "status?");
        fixture.mutate(|tree| {
            tree.resume_tree().unwrap();
            assert!(tree.begin_reopen_wake(&fixture.child));
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
        });
        let command = fixture
            .runtime
            .read(|tree| wake_command(tree, &fixture.child))
            .unwrap()
            .unwrap();
        let started = super::super::children::start_child_turn_as(
            &fixture.runtime,
            &fixture.child,
            attached,
            CommandId::new(command).unwrap(),
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
            super::super::children::StartAdmission::ReopenWake,
        )
        .await;
        assert!(matches!(
            started,
            Err(super::super::children::ChildStartFailure::Refused(ref message))
                if message == REOPEN_HOLD_ENDED
        ));
        fixture
            .runtime
            .read(|tree| {
                let node = tree.node(&fixture.child).unwrap();
                assert_eq!(node.status, NodeStatus::Interrupted);
                assert!(!node.active_turn);
                assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
                assert_eq!(tree.wake_mail(&fixture.child).len(), 1);
                assert!(tree.fault_reason(&fixture.child).is_none());
            })
            .unwrap();
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }

    /// An interrupt that lands after the runtime admitted a reopen wake's
    /// Start, before its turn binds, ends the hold: nothing binds, but the
    /// runtime put the wake's batch in history, so its mail settles read (as
    /// a rejoin whose hold ended settles its batch) instead of staying unread
    /// for the resume to deliver a second time.
    #[tokio::test]
    async fn an_interrupt_after_a_reopen_wake_was_admitted_settles_its_batch_read() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let attached = lose_and_reopen(&fixture).await;
        parent_mail(&fixture, "status?");
        let runtime = Arc::clone(&fixture.runtime);
        let child = fixture.child.clone();
        hooks::set_after_start_admission(
            &fixture.child,
            Arc::new(move || {
                let (runtime, child) = (Arc::clone(&runtime), child.clone());
                Box::pin(async move {
                    runtime
                        .mutate_then_emit(false, |tree| {
                            tree.interrupt_agent(
                                "root",
                                &crate::agent_tree_protocol::AttemptScopedRequest {
                                    agent_id: child.clone(),
                                    expected_attempt_id: None,
                                },
                                None,
                            )
                            .map(|_| ())
                            .map_err(|error| error.message)
                        })
                        .unwrap();
                })
            }),
        );
        fixture.mutate(|tree| tree.resume_tree().unwrap());
        let woke = wake_reopened(&fixture.runtime, &fixture.child, attached).await;
        let (status, turn, unread) = fixture
            .runtime
            .read(|tree| {
                (
                    tree.node(&fixture.child).unwrap().status,
                    tree.attempt(&fixture.child).unwrap().turn_id.clone(),
                    tree.wake_mail(&fixture.child).len(),
                )
            })
            .unwrap();
        let prepared = super::super::result_context::prepared_commands(
            fixture.runtime.driver(),
            &fixture.child,
        )
        .unwrap()
        .len();
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
        assert_eq!(woke, Ok(Reopened::HoldEnded));
        assert_eq!(status, NodeStatus::Interrupted);
        assert_eq!(turn, None, "nothing bound the admitted wake");
        assert_eq!(prepared, 0);
        assert_eq!(
            unread, 0,
            "the admitted wake's mail is in history, yet it is unread for the resume to \
             deliver again"
        );
    }

    /// A fact committed after a local withdrawal still admits the wake. Its
    /// waiter remains live and its batch settles from the feed exactly once.
    #[tokio::test]
    async fn a_wake_fact_outranks_a_pre_admission_withdrawal() {
        let fixture = fixture(
            json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
            leaf,
        )
        .await;
        fixture.until("the park", parked).await;
        let runtime = Arc::clone(&fixture.runtime);
        let child = fixture.child.clone();
        hooks::set_before_wake_admission(
            &fixture.child,
            Arc::new(move || {
                let runtime = Arc::clone(&runtime);
                let child = child.clone();
                Box::pin(async move {
                    let command = runtime
                        .read(|tree| wake_command(tree, &child))
                        .unwrap()
                        .unwrap();
                    super::super::result_context::prepare(
                        runtime.driver(),
                        &child,
                        &command,
                        crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
                    )
                    .unwrap();
                    super::super::result_context::acknowledge(
                        runtime.driver(),
                        &command,
                        super::super::result_context::Submission::Refused,
                    )
                    .unwrap();
                })
            }),
        );
        parent_mail(&fixture, "status?");
        fixture.until("the admitted wake", turns(2)).await;
        fixture
            .runtime
            .read(|tree| {
                assert!(tree.has_turn_owner(&fixture.child));
                assert!(
                    tree.commands()
                        .values()
                        .any(|command| command.recipient == fixture.child
                            && command.kind == crate::agent_tree::AgentCommandKind::Wake
                            && command.withdrawn
                            && command.fact == Some(crate::agent_tree::CommandFact::Accepted))
                );
            })
            .unwrap();
        fixture.submit();
        fixture.until("the attempt to settle", settled).await;
    }

    /// A turn waiter aborted by runtime shutdown (its task dropped mid-turn,
    /// never polled to completion) still leaves its registration: the
    /// registration's `Drop` runs with the task and the count does not leak.
    #[test]
    fn an_aborted_turn_waiter_leaves_its_registration() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let fixture = rt.block_on(fixture(json!([held("working")]), leaf));
        let registered = fixture
            .runtime
            .read(|tree| tree.has_turn_waiter(&fixture.child))
            .unwrap();
        rt.shutdown_timeout(Duration::from_secs(5));
        let after = fixture
            .runtime
            .read(|tree| tree.has_turn_waiter(&fixture.child))
            .unwrap();
        // Session handles may need a reactor to drop.
        std::mem::forget(fixture);
        assert!(registered, "the waiter registered");
        assert!(!after, "the aborted waiter's registration leaked");
    }
}
