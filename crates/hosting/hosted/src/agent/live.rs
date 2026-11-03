//! The product's live child turn: `plakous-hosted`'s implementation of the tree's
//! [`crate::topology_runtime::LiveChildHandle`].
//!
//! One attached child session and the turn it is running, owned by the
//! child's actor. Everything the hub asks of a live child that touches the
//! session (follow up, steer, restart, release, cancel) is a message to that
//! actor, and this is the only place that says so.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous::agent::{CommandId, TurnControl, TurnResultHandle};
use plakous::model::TurnId;

use crate::agent::AttachedCodingSession;
use crate::node::NodeActor;
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
pub(super) enum ChildRestartError {
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

/// The turn a live child is running, as the hub reads it between messages.
struct LiveTurn {
    turn_id: TurnId,
    control: TurnControl,
    observation: Option<crate::runtime::child_projection::ChildTurnProjection>,
}

type FollowUpQueue = std::sync::Mutex<
    std::collections::VecDeque<(
        TurnControl,
        TurnResultHandle,
        crate::runtime::child_projection::ChildTurnProjection,
    )>,
>;

/// What the child's handle and its actor share: the facts the hub reads
/// synchronously (agent, probe, feed source, the turn it runs) and the
/// delivery barrier the hub orders terminal handoff with.
struct ChildShared {
    agent: Option<plakous::agent::Agent>,
    probe: Option<plakous::model::ReachabilityProbe>,
    sessions: Arc<plakous::sessions::AgentSessions>,
    locator: plakous::sessions::SessionLocator,
    turn: std::sync::Mutex<LiveTurn>,
    /// Follow-up turns admitted behind the running turn, in admission order.
    follow_ups: Arc<FollowUpQueue>,
    /// The tree's task set, which this child's projections run in.
    tasks: crate::TaskSet,
    delivery: Arc<tokio::sync::RwLock<()>>,
    /// Orders each delivery reservation with its message, so the actor never
    /// waits for a reservation whose message is queued behind it.
    reserve: std::sync::Mutex<()>,
}

impl ChildShared {
    fn turn(&self) -> std::sync::MutexGuard<'_, LiveTurn> {
        self.turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

type Acknowledged = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The turn waiter's loop over the turns of one bound attempt
/// (`children::turn_watch`), which the actor drives beside its mailbox.
pub(super) type TurnWatch = Pin<Box<dyn Future<Output = ()> + Send>>;

/// One request to a live child's actor, with the channel its answer goes on.
enum ChildMessage {
    FollowUp {
        command_id: CommandId,
        body: String,
        acknowledged: Acknowledged,
        delivery: tokio::sync::OwnedRwLockReadGuard<()>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Restart {
        driver: crate::agent_tree_tools::TreeDriver,
        command_id: CommandId,
        prompt: String,
        acknowledged: Acknowledged,
        reply: tokio::sync::oneshot::Sender<Result<TurnResultHandle, ChildRestartError>>,
    },
    Release {
        driver: crate::agent_tree_tools::TreeDriver,
        reply: tokio::sync::oneshot::Sender<LiveRelease>,
    },
    CancelAndRelease {
        driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
        reply: tokio::sync::oneshot::Sender<LiveRelease>,
    },
    /// The turn the hub occupied the slot with is now watched.
    Watch { watch: TurnWatch },
    /// Hands the attached session back.
    #[cfg(test)]
    Detach {
        reply: tokio::sync::oneshot::Sender<AttachedCodingSession>,
    },
}

/// A live child's port on its node actor (design §3.1): the actor is the
/// sole owner of the child's attached session and of the waiter loop over
/// its turns. Everything the hub asks of a live child that touches the
/// session (follow up, restart, release, cancel) is a message it answers in
/// arrival order; an answer nobody waits for any more stops the work, as
/// dropping the request's future did.
///
/// The watch runs beside every message, never behind one: it reaches the
/// actor's own session only through the hub (a restart, a retirement's
/// release), so the actor keeps serving while the watch waits on those.
/// The actor ends once its session is gone and its watch has finished.
struct ChildRole {
    shared: Arc<ChildShared>,
    watch: Option<TurnWatch>,
    /// The actor still holds its session; a release, a detach or the
    /// handle's drop hands it on.
    held: bool,
}

/// What a live child's main loop borrows for the actor's life.
struct ChildMailbox {
    inbox: tokio::sync::mpsc::UnboundedReceiver<ChildMessage>,
    open: bool,
}

impl crate::node::Role<ChildMailbox> for ChildRole {
    type Input = ChildMessage;
    type Output = ();

    async fn next(
        actor: &mut NodeActor<Self>,
        cx: &mut ChildMailbox,
    ) -> crate::node::Next<ChildMessage, ()> {
        if !cx.open && actor.role.watch.is_none() {
            return crate::node::Next::End(());
        }
        let message = tokio::select! {
            biased;
            message = cx.inbox.recv(), if cx.open => message,
            () = watched(&mut actor.role.watch) => return crate::node::Next::Again,
        };
        let Some(message) = message else {
            // Dropped without a release: the session goes with the handle.
            cx.open = false;
            drop(actor.take_session());
            return crate::node::Next::Again;
        };
        crate::node::Next::Input(message)
    }

    async fn serve(
        actor: &mut NodeActor<Self>,
        _cx: &mut ChildMailbox,
        message: ChildMessage,
    ) -> crate::node::Served<()> {
        Box::pin(actor.serve_message(message)).await;
        crate::node::Served::Continue
    }
}

impl NodeActor<ChildRole> {
    /// Hands the session on; the actor holds it no more.
    fn take_session(&mut self) -> Option<AttachedCodingSession> {
        std::mem::take(&mut self.role.held).then(|| self.opened.take_for_release())
    }

    async fn serve_message(&mut self, message: ChildMessage) {
        match message {
            ChildMessage::FollowUp {
                command_id,
                body,
                acknowledged,
                delivery,
                reply,
            } => {
                self.serve_follow_up(command_id, body, acknowledged, delivery, reply)
                    .await;
            }
            ChildMessage::Restart {
                driver,
                command_id,
                prompt,
                acknowledged,
                reply,
            } => {
                let Some(attached) = self.role.held.then_some(&self.opened) else {
                    let gone = ChildRestartError::Runtime(ACTOR_GONE.to_owned());
                    let _ = reply.send(Err(gone));
                    return;
                };
                let work = restart(
                    attached,
                    &self.role.shared,
                    &driver,
                    command_id,
                    prompt,
                    acknowledged,
                );
                alongside(answer(reply, work), &mut self.role.watch).await;
            }
            ChildMessage::Release { driver, reply } => {
                let delivery = self.reserve_release().await;
                let Some(attached) = self.take_session() else {
                    let _ = reply.send(gone_release());
                    return;
                };
                let work = release(attached, delivery, driver);
                Box::pin(alongside(answer(reply, work), &mut self.role.watch)).await;
            }
            ChildMessage::CancelAndRelease {
                driver,
                dispatched,
                reply,
            } => {
                let delivery = self.reserve_release().await;
                let Some(attached) = self.take_session() else {
                    let _ = reply.send(gone_release());
                    return;
                };
                let control = self.role.shared.turn().control.clone();
                let work = cancel_and_release(attached, control, delivery, driver, dispatched);
                Box::pin(alongside(answer(reply, work), &mut self.role.watch)).await;
            }
            ChildMessage::Watch { watch } => self.role.watch = Some(watch),
            #[cfg(test)]
            ChildMessage::Detach { reply } => {
                if let Some(attached) = self.take_session() {
                    let _ = reply.send(attached);
                }
            }
        }
    }

    async fn serve_follow_up(
        &mut self,
        command_id: CommandId,
        body: String,
        acknowledged: Acknowledged,
        delivery: tokio::sync::OwnedRwLockReadGuard<()>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) {
        let Some(attached) = self.role.held.then_some(&self.opened) else {
            let _ = reply.send(Err(ACTOR_GONE.to_owned()));
            return;
        };
        let work = follow_up(
            attached,
            &self.role.shared,
            command_id,
            body,
            acknowledged,
            delivery,
        );
        alongside(answer(reply, work), &mut self.role.watch).await;
    }

    /// The delivery barrier's write side, for a release. Every follow-up
    /// reserved before the release is ahead of it in the mailbox (a
    /// reservation and its message are sent under the one `reserve` lock),
    /// so the actor has served it and its read side is gone; nothing
    /// reserved after the release can reach this actor, whose handle the
    /// release consumed. Only the watch can still hold the barrier, across
    /// a transition it is in, so it runs meanwhile.
    async fn reserve_release(&mut self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        let write = Arc::clone(&self.role.shared.delivery).write_owned();
        alongside(write, &mut self.role.watch).await
    }
}

/// Runs the watch to its end, then never again.
async fn watched(watch: &mut Option<TurnWatch>) {
    let Some(turn) = watch.as_mut() else {
        return std::future::pending().await;
    };
    turn.await;
    *watch = None;
}

/// `work`, with the watch running beside it.
async fn alongside<T>(work: impl Future<Output = T>, watch: &mut Option<TurnWatch>) -> T {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            value = &mut work => return value,
            () = watched(watch) => {}
        }
    }
}

/// Sends what `work` answers, or stops it once nobody waits for the answer.
async fn answer<T>(mut reply: tokio::sync::oneshot::Sender<T>, work: impl Future<Output = T>) {
    tokio::select! {
        biased;
        () = reply.closed() => {}
        value = work => {
            let _ = reply.send(value);
        }
    }
}

fn gone_release() -> LiveRelease {
    LiveRelease {
        feed: None,
        locator: None,
        failure: Some(ACTOR_GONE.to_owned()),
    }
}

async fn follow_up(
    attached: &AttachedCodingSession,
    shared: &ChildShared,
    command_id: CommandId,
    body: String,
    acknowledged: Acknowledged,
    _delivery: tokio::sync::OwnedRwLockReadGuard<()>,
) -> Result<(), String> {
    let agent = attached
        .agent()
        .cloned()
        .ok_or_else(|| "opened child has no agent".to_owned())?;
    let turn_id = shared.turn().turn_id;
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
    let mut submission =
        crate::runtime::child_projection::ChildAdmission::new(&shared.tasks, submission, &agent);
    #[cfg(test)]
    super::park::hooks::after_follow_up_admission(&agent.id().to_string()).await;
    acknowledged.await;
    // A newly admitted follow-up is the child's next turn; an exact
    // duplicate was queued by the first delivery.
    if let (Some(control), Some(result), Some(projection)) = (
        submission.control.take(),
        submission.result.take(),
        submission.projection.take(),
    ) {
        shared
            .follow_ups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back((control, result, projection));
    }
    Ok(())
}

// The offline fixture delays host acknowledgement while the admitted turn
// completes, exercising registration independently of provider scheduling.
async fn restart(
    attached: &AttachedCodingSession,
    shared: &ChildShared,
    driver: &crate::agent_tree_tools::TreeDriver,
    command_id: CommandId,
    prompt: String,
    acknowledged: Acknowledged,
) -> Result<TurnResultHandle, ChildRestartError> {
    let agent = attached
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
    let mut submission =
        crate::runtime::child_projection::ChildAdmission::new(&shared.tasks, submission, agent);
    acknowledged.await;
    if crate::agent::ledger::consumed_below_retention(
        driver,
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
    .await?
    {
        return Err(ChildRestartError::Consumed);
    }
    if let Some(rejection) = submission.acknowledgement.rejection() {
        return Err(ChildRestartError::Runtime(format!(
            "{RESTART_REJECTED}: {rejection}"
        )));
    }
    let turn_id = submission
        .acknowledgement
        .turn_id()
        .ok_or_else(|| "submit harness start produced no turn identity".to_owned())?;
    let disposition = submission.acknowledgement.disposition();
    let control = match disposition {
        plakous::agent::CommandAckDisposition::Accepted => submission.control.clone(),
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
        .result
        .clone()
        .ok_or_else(|| "submit harness start produced no result handle".to_owned())?;
    // The table must name the turn that is actually running, or a
    // later follow-up steers a turn that has already ended.
    let mut turn = shared.turn();
    if let Some(projection) = submission.projection.take() {
        turn.observation = Some(projection);
    } else if turn.turn_id != turn_id {
        turn.observation = Some(
            crate::runtime::child_projection::ChildTurnProjection::recovered(
                &shared.tasks,
                Some(agent.clone()),
                turn_id,
            ),
        );
    }
    turn.turn_id = turn_id;
    turn.control = control;
    Ok(result)
}

async fn release(
    attached: AttachedCodingSession,
    _delivery: tokio::sync::OwnedRwLockWriteGuard<()>,
    driver: crate::agent_tree_tools::TreeDriver,
) -> LiveRelease {
    let sessions = Arc::clone(&attached.sessions);
    match attached.release_opened_for_transfer().await {
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
}

async fn cancel_and_release(
    attached: AttachedCodingSession,
    control: TurnControl,
    _delivery: tokio::sync::OwnedRwLockWriteGuard<()>,
    driver: crate::agent_tree_tools::TreeDriver,
    dispatched: Option<tokio::sync::oneshot::Sender<()>>,
) -> LiveRelease {
    let cancel = match crate::interrupt::settle_cancel(
        async {
            let cancellation = control.request_cancel().await;
            if let Some(dispatched) = dispatched {
                let _ = dispatched.send(());
            }
            let status = cancellation?;
            control.wait_until_settled().await;
            Ok::<_, plakous::agent::ControlError>(status)
        },
        plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
    )
    .await
    {
        crate::interrupt::CancelSettlement::Settled => None,
        crate::interrupt::CancelSettlement::Unsettled(detail) => Some(detail.to_string()),
    };
    let sessions = Arc::clone(&attached.sessions);
    match attached.release_opened_for_transfer().await {
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
}

/// The hub's handle on one live child turn: the child's actor, which owns
/// the attached session, and the facts the hub reads without waiting for it.
pub struct ChildActorHandle {
    mailbox: tokio::sync::mpsc::UnboundedSender<ChildMessage>,
    shared: Arc<ChildShared>,
}

/// The answer of an actor that stopped before it replied: the child's
/// session went with it.
const ACTOR_GONE: &str = "the child's actor stopped before it answered";

impl ChildActorHandle {
    /// One live child on its running turn, with no follow-up queued. The
    /// actor runs on `owners`; the turn's projections run on `tasks`.
    #[must_use]
    pub fn new(
        tasks: &crate::TaskSet,
        owners: &crate::TaskSet,
        attached: AttachedCodingSession,
        turn_id: TurnId,
        control: TurnControl,
    ) -> Self {
        let observation = crate::runtime::child_projection::ChildTurnProjection::recovered(
            tasks,
            attached.agent().cloned(),
            turn_id,
        );
        Self::admitted(tasks, owners, attached, turn_id, control, observation)
    }

    pub(crate) fn admitted(
        tasks: &crate::TaskSet,
        owners: &crate::TaskSet,
        attached: AttachedCodingSession,
        turn_id: TurnId,
        control: TurnControl,
        observation: crate::runtime::child_projection::ChildTurnProjection,
    ) -> Self {
        Self::with_observation(tasks, owners, attached, turn_id, control, Some(observation))
    }

    /// A throwaway cancellation/release owner, without presentation.
    pub(crate) fn without_observation(
        tasks: &crate::TaskSet,
        owners: &crate::TaskSet,
        attached: AttachedCodingSession,
        turn_id: TurnId,
        control: TurnControl,
    ) -> Self {
        Self::with_observation(tasks, owners, attached, turn_id, control, None)
    }

    fn with_observation(
        tasks: &crate::TaskSet,
        owners: &crate::TaskSet,
        attached: AttachedCodingSession,
        turn_id: TurnId,
        control: TurnControl,
        observation: Option<crate::runtime::child_projection::ChildTurnProjection>,
    ) -> Self {
        let shared = Arc::new(ChildShared {
            agent: attached.agent().cloned(),
            probe: attached.reachability_probe.clone(),
            sessions: Arc::clone(&attached.sessions),
            locator: attached.locator(),
            turn: std::sync::Mutex::new(LiveTurn {
                turn_id,
                control,
                observation,
            }),
            follow_ups: Arc::default(),
            tasks: tasks.clone(),
            delivery: Arc::default(),
            reserve: std::sync::Mutex::new(()),
        });
        let (mailbox, inbox) = tokio::sync::mpsc::unbounded_channel();
        let mut actor = NodeActor::new(
            crate::node::RolePolicy::CHILD,
            attached,
            ChildRole {
                shared: Arc::clone(&shared),
                watch: None,
                held: true,
            },
        );
        drop(owners.spawn(async move {
            actor
                .main_loop(&mut ChildMailbox { inbox, open: true })
                .await;
        }));
        Self { mailbox, shared }
    }

    /// What hands the actor the turn the hub occupies its slot with.
    #[must_use]
    pub(in crate::agent) fn watcher(&self) -> ChildWatcher {
        ChildWatcher {
            mailbox: self.mailbox.clone(),
        }
    }

    /// The live child's session, for the feed reads around it.
    #[cfg(test)]
    pub(crate) fn locator(&self) -> plakous::sessions::SessionLocator {
        self.shared.locator
    }

    /// The live child's session store, for the feed reads around it.
    #[cfg(test)]
    pub(crate) fn sessions(&self) -> Arc<plakous::sessions::AgentSessions> {
        Arc::clone(&self.shared.sessions)
    }

    /// The follow-up turns admitted behind the running turn.
    #[cfg(test)]
    pub(super) fn follow_ups(&self) -> Arc<FollowUpQueue> {
        Arc::clone(&self.shared.follow_ups)
    }

    /// Stops the actor and hands its attached session back.
    #[cfg(test)]
    pub(crate) async fn into_attached(self) -> AttachedCodingSession {
        let (reply, answer) = tokio::sync::oneshot::channel();
        assert!(self.mailbox.send(ChildMessage::Detach { reply }).is_ok());
        answer.await.expect("the actor hands its session back")
    }

    pub(super) fn follow_up_with_ack(
        &self,
        command_id: CommandId,
        body: String,
        acknowledged: impl Future<Output = ()> + Send + 'static,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        // Reserve synchronously under the map guard: terminal handoff must
        // wait even if this owned operation has not been polled yet.
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = {
            let _order = self
                .shared
                .reserve
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(&self.shared.delivery)
                .try_read_owned()
                .map_err(|_| TURN_ENDED.to_owned())
                .and_then(|delivery| {
                    self.mailbox
                        .send(ChildMessage::FollowUp {
                            command_id,
                            body,
                            acknowledged: Box::pin(acknowledged),
                            delivery,
                            reply,
                        })
                        .map_err(|_| ACTOR_GONE.to_owned())
                })
        };
        Box::pin(async move {
            sent?;
            answer.await.unwrap_or_else(|_| Err(ACTOR_GONE.to_owned()))
        })
    }

    pub(super) async fn restart_with_ack(
        &mut self,
        driver: &crate::agent_tree_tools::TreeDriver,
        command_id: CommandId,
        prompt: String,
        acknowledged: impl Future<Output = ()> + Send + 'static,
    ) -> Result<TurnResultHandle, ChildRestartError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(ChildMessage::Restart {
                driver: driver.clone(),
                command_id,
                prompt,
                acknowledged: Box::pin(acknowledged),
                reply,
            })
            .map_err(|_| ChildRestartError::Runtime(ACTOR_GONE.to_owned()))?;
        answer
            .await
            .unwrap_or_else(|_| Err(ChildRestartError::Runtime(ACTOR_GONE.to_owned())))
    }

    fn release_with(
        self,
        message: impl FnOnce(tokio::sync::oneshot::Sender<LiveRelease>) -> ChildMessage,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = {
            let _order = self
                .shared
                .reserve
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.mailbox.send(message(reply)).is_ok()
        };
        Box::pin(async move {
            let gone = || LiveRelease {
                feed: None,
                locator: None,
                failure: Some(ACTOR_GONE.to_owned()),
            };
            if !sent {
                return gone();
            }
            answer.await.unwrap_or_else(|_| gone())
        })
    }
}

/// Hands a live child's actor the turn its slot holds, once the hub has
/// occupied the slot. Taken from the handle before the hub owns it.
pub(in crate::agent) struct ChildWatcher {
    mailbox: tokio::sync::mpsc::UnboundedSender<ChildMessage>,
}

impl ChildWatcher {
    /// The actor watches `result` and the turns after it for the attempt
    /// `registration` holds. Only an aborted actor refuses it, and the
    /// registration then leaves as it drops.
    pub(in crate::agent) fn watch(
        self,
        registration: super::children::TurnWaiterRegistration,
        live: Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
        observer: Option<Arc<dyn crate::agent_tree::TopologyObserver>>,
        feed: Option<super::children::ChildFeedSource>,
        result: TurnResultHandle,
    ) {
        let watch = super::children::turn_watch(registration, live, observer, feed, result);
        let _ = self.mailbox.send(ChildMessage::Watch { watch });
    }

    /// The actor drives `watch` as its turn's watch.
    #[cfg(test)]
    pub(in crate::agent) fn watch_with(self, watch: TurnWatch) {
        let _ = self.mailbox.send(ChildMessage::Watch { watch });
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

impl LiveChildHandle for ChildActorHandle {
    fn host_observation(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        Some(self.shared.turn().observation.as_ref()?.observation.clone())
    }

    fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        self.shared.probe.clone()
    }

    fn agent(&self) -> Option<plakous::agent::Agent> {
        self.shared.agent.clone()
    }
    fn turn_id(&self) -> TurnId {
        self.shared.turn().turn_id
    }

    fn turn_control(&self) -> TurnControl {
        self.shared.turn().control.clone()
    }

    fn feed(
        &self,
        cursor: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<crate::agent_tree::TreeFeed>, String>> + Send>>
    {
        let sessions = Arc::clone(&self.shared.sessions);
        let session = self.shared.locator.session_id();
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
        Some(Arc::clone(&self.shared.delivery))
    }

    fn take_follow_up(&mut self) -> Option<TurnResultHandle> {
        let (control, result, observation) = self
            .shared
            .follow_ups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()?;
        let mut turn = self.shared.turn();
        turn.observation = Some(observation);
        turn.turn_id = result.turn_id();
        turn.control = control;
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
        (*self).release_with(|reply| ChildMessage::Release { driver, reply })
    }

    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        (*self).release_with(|reply| ChildMessage::CancelAndRelease {
            driver,
            dispatched,
            reply,
        })
    }
}

#[cfg(test)]
mod job_release_tests;

#[cfg(test)]
mod line_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "live/follow_up_tests.rs"]
mod follow_up_tests;
