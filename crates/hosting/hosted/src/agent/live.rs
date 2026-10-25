//! The product's live child turn: `plakous-hosted`'s implementation of the tree's
//! [`crate::topology_runtime::LiveChildHandle`].
//!
//! One attached child session and the turn it is running. Everything the hub
//! asks of a live child — follow up, steer, restart, release, cancel — is a
//! session operation, and this is the only place that says so.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous::agent::{CommandId, TurnControl, TurnResultHandle};
use plakous::model::TurnId;

use crate::agent::AttachedCodingSession;
use crate::topology_runtime::{LiveChildHandle, LiveRelease};

/// A follow-up or steer reached the child after the turn it targeted ended.
pub(super) const TURN_ENDED: &str = "the child's turn ended before this mail reached it";

pub(super) const RESTART_RECONCILIATION_REQUIRED: &str =
    "duplicate child restart has no live turn; result reconciliation required";

/// Prefix of a restart the runtime definitely rejected: nothing was admitted.
pub(super) const RESTART_REJECTED: &str = "child restart was rejected";

type LiveTransition<'a> = (
    tokio::sync::MutexGuard<'a, crate::topology_runtime::LiveChildTable>,
    Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
);

pub(super) async fn lock_for_transition<'a>(
    live: &'a Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    turn: TurnId,
) -> Result<LiveTransition<'a>, String> {
    loop {
        let children = live.lock().await;
        let child = children
            .get(agent_id)
            .ok_or("child live session is missing")?;
        if child.turn_id() != turn {
            return Err("child turn has already been replaced".into());
        }
        let delivery = match child.follow_up_barrier() {
            Some(barrier) => {
                if let Ok(guard) = Arc::clone(&barrier).try_write_owned() {
                    Some(guard)
                } else {
                    drop(children);
                    // Never invert map/barrier order. Racing deliveries target
                    // the ended turn and fail fast, so this drain is bounded.
                    drop(barrier.write_owned().await);
                    continue;
                }
            }
            None => None,
        };
        return Ok((children, delivery));
    }
}

#[derive(Debug)]
enum ChildRestartError {
    ReconciliationRequired,
    /// The command was recorded below the feed's retention (spec 03 §14.1).
    Consumed,
    Runtime(String),
}

impl From<String> for ChildRestartError {
    fn from(message: String) -> Self {
        Self::Runtime(message)
    }
}

impl std::fmt::Display for ChildRestartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReconciliationRequired => formatter.write_str(RESTART_RECONCILIATION_REQUIRED),
            Self::Consumed => formatter.write_str(super::children::CHILD_COMMAND_CONSUMED),
            Self::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ChildRestartError {}

/// One live child turn: the session it is attached to, the turn it is
/// running, and that turn's control.
pub struct ProductLiveChild {
    pub attached: AttachedCodingSession,
    pub turn_id: TurnId,
    pub control: TurnControl,
    /// Follow-up turns admitted behind the running turn, in admission order.
    pub follow_ups:
        Arc<std::sync::Mutex<std::collections::VecDeque<(TurnControl, TurnResultHandle)>>>,
    delivery: Arc<tokio::sync::RwLock<()>>,
    delivery_serial: Arc<tokio::sync::Mutex<()>>,
}

impl ProductLiveChild {
    /// One live child on its running turn, with no follow-up queued.
    #[must_use]
    pub fn new(attached: AttachedCodingSession, turn_id: TurnId, control: TurnControl) -> Self {
        Self {
            attached,
            turn_id,
            control,
            follow_ups: Arc::default(),
            delivery: Arc::default(),
            delivery_serial: Arc::default(),
        }
    }

    fn follow_up_with_ack(
        &self,
        command_id: CommandId,
        body: String,
        acknowledged: impl Future<Output = ()> + Send + 'static,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        // Reserve synchronously under the map guard: terminal handoff must
        // wait even if this owned operation has not been polled yet.
        let delivery = Arc::clone(&self.delivery).try_read_owned();
        let agent = self.attached.agent().cloned();
        let turn_id = self.turn_id;
        let follow_ups = Arc::clone(&self.follow_ups);
        let serial = Arc::clone(&self.delivery_serial);
        Box::pin(async move {
            let _delivery = delivery.map_err(|_| TURN_ENDED.to_owned())?;
            let _serial = serial.lock().await;
            let agent = agent.ok_or_else(|| "opened child has no agent".to_owned())?;
            let submission = agent
                .follow_up_with_id(command_id, turn_id, body)
                .await
                .map_err(|error| {
                    if error.kind() == plakous::agent::ControlErrorKind::WrongPhase {
                        TURN_ENDED.to_owned()
                    } else {
                        error.to_string()
                    }
                })?;
            #[cfg(test)]
            super::park::hooks::after_follow_up_admission(&agent.id().to_string()).await;
            acknowledged.await;
            // A newly admitted follow-up is the child's next turn; an exact
            // duplicate was queued by the first delivery.
            if let (Some(turn), Some(result)) = (submission.turn(), submission.result()) {
                follow_ups
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back((turn.control(), result.clone()));
            }
            Ok(())
        })
    }

    // The offline fixture delays host acknowledgement while the admitted turn
    // completes, exercising registration independently of provider scheduling.
    async fn restart_with_ack(
        &mut self,
        driver: &crate::agent_tree_tools::TreeDriver,
        command_id: CommandId,
        prompt: String,
        acknowledged: impl Future<Output = ()> + Send,
    ) -> Result<TurnResultHandle, ChildRestartError> {
        let agent = self
            .attached
            .agent()
            .ok_or_else(|| "opened child has no agent".to_owned())?;
        let command_text = command_id.as_str().to_owned();
        let submission = agent
            .command(plakous::agent::TurnCommand::StartInstruction {
                command_id,
                text: prompt,
            })
            .await
            .map_err(|error| error.to_string())?;
        acknowledged.await;
        if crate::agent::ledger::consumed_below_retention(
            driver,
            &self.attached.sessions,
            self.attached.locator().session_id(),
            &command_text,
            submission.acknowledgement().disposition(),
            submission
                .acknowledgement()
                .rejection()
                .is_some_and(|rejection| {
                    rejection.kind() == plakous::agent::ControlErrorKind::Reclaimed
                }),
        )
        .await?
        {
            return Err(ChildRestartError::Consumed);
        }
        if let Some(rejection) = submission.acknowledgement().rejection() {
            return Err(ChildRestartError::Runtime(format!(
                "{RESTART_REJECTED}: {rejection}"
            )));
        }
        let turn_id = submission
            .acknowledgement()
            .turn_id()
            .ok_or_else(|| "submit harness start produced no turn identity".to_owned())?;
        let disposition = submission.acknowledgement().disposition();
        let control = match disposition {
            plakous::agent::CommandAckDisposition::Accepted => {
                submission.turn().map(plakous::agent::Turn::control)
            }
            plakous::agent::CommandAckDisposition::Duplicate => agent.turn_control(turn_id),
            _ => None,
        }
        .ok_or_else(|| {
            if disposition == plakous::agent::CommandAckDisposition::Duplicate {
                ChildRestartError::ReconciliationRequired
            } else {
                ChildRestartError::Runtime("child turn control is unavailable".to_owned())
            }
        })?;
        let result = submission
            .result()
            .cloned()
            .ok_or_else(|| "submit harness start produced no result handle".to_owned())?;
        // The table must name the turn that is actually running, or a
        // later follow-up steers a turn that has already ended.
        self.turn_id = turn_id;
        self.control = control;
        Ok(result)
    }
}

pub(super) fn release_observation(
    driver: &crate::agent_tree_tools::TreeDriver,
    locator: plakous::sessions::SessionLocator,
) -> (u64, bool) {
    driver
        .read(|tree| {
            (
                tree.feed_read_cursor(&locator.session_id().to_string()),
                tree.nodes().values().any(|node| {
                    node.session_locator.as_deref() == Some(locator.to_string().as_str())
                }),
            )
        })
        .unwrap_or((0, false))
}

pub(super) fn live_feed_cursor(
    driver: &crate::agent_tree_tools::TreeDriver,
    agent_id: &str,
) -> Result<u64, String> {
    driver
        .read(|tree| {
            let locator = tree
                .node(agent_id)?
                .session_locator
                .as_deref()
                .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok());
            Ok::<_, crate::agent_tree::TopologyError>(locator.map_or(0, |locator| {
                tree.feed_read_cursor(&locator.session_id().to_string())
            }))
        })
        .map_err(|error| error.message)?
        .map_err(|error| error.message)
}

/// Loads the released session's feed before returning its construction locator.
pub(crate) async fn released_with_feed(
    sessions: &plakous::sessions::AgentSessions,
    locator: plakous::sessions::SessionLocator,
    failure: Option<String>,
    cursor: u64,
    recorded: bool,
) -> LiveRelease {
    match super::read_tree_feed(sessions, locator.session_id(), cursor)
        .await
        .map_err(String::from)
    {
        Ok(feed) => {
            let reached = recorded || (feed.records.is_empty() && !feed.went_behind_retention()) || feed.heads.contains(&locator) || feed.records.iter().any(|record| {
                let header = record.header();
                header.session_id == locator.session_id()
                    && header.branch_id == locator.branch_id()
                    && matches!(header.change, plakous::sessions::feed::FeedChange::LeaseReleased { head, .. } if head == locator.checkpoint_id())
            });
            let failure = if reached {
                failure
            } else {
                let debt =
                    "child release feed did not reach the released head; projection required";
                Some(
                    failure.map_or_else(|| debt.to_owned(), |failure| format!("{failure}; {debt}")),
                )
            };
            LiveRelease {
                feed: Some(feed),
                locator: Some(locator.to_string()),
                failure,
            }
        }
        Err(error) => LiveRelease {
            feed: None,
            locator: Some(locator.to_string()),
            failure: Some(failure.map_or(error.clone(), |failure| format!("{failure}; {error}"))),
        },
    }
}

impl LiveChildHandle for ProductLiveChild {
    fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        self.attached.reachability_probe.clone()
    }

    fn agent(&self) -> Option<plakous::agent::Agent> {
        self.attached.agent().cloned()
    }
    fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    fn turn_control(&self) -> TurnControl {
        self.control.clone()
    }

    fn feed(
        &self,
        cursor: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<crate::agent_tree::TreeFeed>, String>> + Send>>
    {
        let sessions = Arc::clone(&self.attached.sessions);
        let session = self.attached.locator().session_id();
        Box::pin(async move {
            super::read_tree_feed(&sessions, session, cursor)
                .await
                .map(Some)
                .map_err(String::from)
        })
    }

    fn follow_up(
        &self,
        command_id: CommandId,
        body: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        self.follow_up_with_ack(command_id, body, std::future::ready(()))
    }

    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        Some(Arc::clone(&self.delivery))
    }

    fn take_follow_up(&mut self) -> Option<TurnResultHandle> {
        let (control, result) = self
            .follow_ups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()?;
        self.turn_id = result.turn_id();
        self.control = control;
        Some(result)
    }

    fn restart(
        &mut self,
        driver: crate::agent_tree_tools::TreeDriver,
        command_id: CommandId,
        prompt: String,
        delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        Box::pin(async move {
            let _delivery = delivery
                .ok_or_else(|| "child restart requires a delivery reservation".to_owned())?;
            self.restart_with_ack(&driver, command_id, prompt, std::future::ready(()))
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::pin(async move {
            let _delivery = Arc::clone(&self.delivery).write_owned().await;
            let sessions = Arc::clone(&self.attached.sessions);
            match self.attached.release_opened_for_transfer().await {
                Ok(locator) => {
                    let (cursor, recorded) = release_observation(&driver, locator);
                    released_with_feed(&sessions, locator, None, cursor, recorded).await
                }
                Err(error) => LiveRelease {
                    feed: None,
                    locator: None,
                    failure: Some(format!(
                        "child session could not be released ({:?}): {error}",
                        error.kind()
                    )),
                },
            }
        })
    }

    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::pin(async move {
            let _delivery = Arc::clone(&self.delivery).write_owned().await;
            let cancel = match crate::interrupt::settle_cancel(
                self.control.cancel(),
                plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
            )
            .await
            {
                crate::interrupt::CancelSettlement::Settled => None,
                crate::interrupt::CancelSettlement::Unsettled(detail) => Some(detail),
            };
            let sessions = Arc::clone(&self.attached.sessions);
            match self.attached.release_opened_for_transfer().await {
                Ok(locator) => {
                    let (cursor, recorded) = release_observation(&driver, locator);
                    released_with_feed(&sessions, locator, cancel, cursor, recorded).await
                }
                Err(error) => LiveRelease {
                    feed: None,
                    locator: None,
                    failure: cancel.or_else(|| {
                        Some(format!(
                            "child session could not be released after cancel ({:?}): {error}",
                            error.kind()
                        ))
                    }),
                },
            }
        })
    }
}

#[cfg(test)]
mod job_release_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "live/follow_up_tests.rs"]
mod follow_up_tests;
