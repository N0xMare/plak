//! One child turn's read model, shared by every host face, and the one
//! drive that reduces the turn's events into it beside the turn's result
//! (design §3.1): the child's actor runs it from its turn waiter, so a
//! turn's events and its result have one owner, and a follow-up admitted
//! mid-turn waits for that drive rather than running a reducer of its own.

use super::{HostEvent, TurnProjection};
#[cfg(test)]
use crate::TaskHandle;
use crate::node::Flow;
use plakous::agent::{
    Agent, AgentEvent, CommandAck, EventDelivery, EventSubscription, SubscriptionEnd, Turn,
    TurnControl, TurnEventError, TurnEvents, TurnResultHandle, TurnSubmission, TurnViewState,
};
use plakous::model::TurnId;
use plakous_host_protocol::{Core, views::ChildProgress};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

const RETAINED_UPDATES: usize = 128;

pub(crate) struct ChildAdmission {
    pub(crate) acknowledgement: CommandAck,
    pub(crate) result: Option<TurnResultHandle>,
    pub(crate) control: Option<TurnControl>,
    pub(crate) projection: Option<ChildTurnProjection>,
}

impl ChildAdmission {
    pub(crate) fn new(submission: TurnSubmission, agent: &Agent) -> Self {
        let acknowledgement = submission.acknowledgement().clone();
        let result = submission.result().cloned();
        let mut turn = submission.into_turn();
        let control = turn.as_ref().map(Turn::control);
        let projection = turn
            .as_mut()
            .map(|turn| ChildTurnProjection::admitted(turn, agent));
        Self {
            acknowledgement,
            result,
            control,
            projection,
        }
    }
}

pub(crate) struct ChildTurnProjection {
    pub(crate) observation: Arc<ChildObservation>,
    /// A test's own drive of the turn, which no waiter takes.
    #[cfg(test)]
    task: Option<TaskHandle<()>>,
}

impl Drop for ChildTurnProjection {
    fn drop(&mut self) {
        self.observation.close();
        #[cfg(test)]
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Where a turn's events will come from, until its drive takes them.
type PendingSource = std::pin::Pin<Box<dyn Future<Output = Option<Source>> + Send>>;

/// A turn's events not yet driven: where they come from, the agent that
/// recovers them, and whether an earlier drive ended before them.
struct Pending {
    source: PendingSource,
    agent: Option<Agent>,
    gap: bool,
}

enum Source {
    Admission(TurnEvents),
    Recovered(EventSubscription),
    #[cfg(test)]
    Fixture(tokio::sync::mpsc::Receiver<Received>),
}

impl Source {
    async fn recv(&mut self) -> Received {
        match self {
            Self::Admission(events) => match events.recv_delivery().await {
                Ok(delivery) => Received::delivery(delivery),
                Err(TurnEventError::Closed) => Received::Closed,
                Err(_) => Received::Recover,
            },
            Self::Recovered(events) => match events.recv_delivery().await {
                Ok(delivery) => Received::delivery(delivery),
                Err(SubscriptionEnd::AgentClosed { .. }) => Received::Closed,
                Err(_) => Received::Recover,
            },
            #[cfg(test)]
            Self::Fixture(events) => events.recv().await.unwrap_or(Received::Closed),
        }
    }
}

enum Received {
    Event(Arc<AgentEvent>),
    Lost,
    Recover,
    Closed,
}

impl Received {
    fn delivery(delivery: EventDelivery) -> Self {
        match delivery {
            EventDelivery::Event(event) => Self::Event(event),
            EventDelivery::Lost(_) => Self::Lost,
            _ => Self::Recover,
        }
    }
}

async fn recover(agent: Option<&Agent>, turn: TurnId) -> Option<Source> {
    let agent = agent?;
    loop {
        if let Ok(attachment) = agent.attach(plakous::agent::ViewRequest::content()).await {
            let (snapshot, events) = attachment.into_parts();
            if snapshot.state() == plakous::agent::AgentViewState::Closed
                || snapshot
                    .turns()
                    .iter()
                    .any(|view| view.turn_id() == turn && view.state() != TurnViewState::Running)
            {
                return None;
            }
            return Some(Source::Recovered(events));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod perf;
#[cfg(test)]
mod tests;

impl ChildTurnProjection {
    pub(crate) fn admitted(turn: &mut Turn, agent: &Agent) -> Self {
        match turn.take_events() {
            Ok(events) => Self::start(
                turn.id(),
                Some(agent.clone()),
                std::future::ready(Some(Source::Admission(events))),
            ),
            Err(_) => Self::recovered(Some(agent.clone()), turn.id()),
        }
    }

    pub(crate) fn recovered(agent: Option<Agent>, turn: TurnId) -> Self {
        let initial_agent = agent.clone();
        Self::start(turn, agent, async move {
            recover(initial_agent.as_ref(), turn).await
        })
    }

    fn start(
        turn: TurnId,
        agent: Option<Agent>,
        source: impl Future<Output = Option<Source>> + Send + 'static,
    ) -> Self {
        let (changed, _) = watch::channel(0);
        let mut projection = TurnProjection::default();
        projection.progress.view.turn_id = turn.to_string();
        let progress = projection.progress.view.clone();
        let observation = Arc::new(ChildObservation {
            turn,
            changed,
            source: Mutex::new(Some(Pending {
                source: Box::pin(source),
                agent,
                gap: false,
            })),
            state: Mutex::new(State {
                projection: Some(projection),
                progress,
                terminal: false,
                cursor: 0,
                updates: VecDeque::new(),
                closed: false,
            }),
        });
        Self {
            observation,
            #[cfg(test)]
            task: None,
        }
    }

    /// Runs this turn's drive on `tasks` with no result to take, as a test
    /// does whose turn no waiter drives.
    #[cfg(test)]
    pub(crate) fn drive_in(mut self, tasks: &crate::TaskSet) -> Self {
        let observation = Arc::clone(&self.observation);
        self.task = Some(tasks.spawn(async move {
            observation.drive(std::future::pending::<()>()).await;
        }));
        self
    }
}

/// A child turn's events, recovered in place when their stream breaks.
struct ChildEvents {
    source: Source,
    agent: Option<Agent>,
    turn: TurnId,
}

/// What a child turn's event stream yields.
enum ChildEvent {
    Event(Arc<AgentEvent>),
    Lost,
    /// The stream broke and was resubscribed.
    Recovered,
    Closed,
}

impl crate::node::EventSource for ChildEvents {
    type Event = ChildEvent;

    async fn recv(&mut self) -> ChildEvent {
        match self.source.recv().await {
            Received::Event(event) => ChildEvent::Event(event),
            Received::Lost => ChildEvent::Lost,
            Received::Recover => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Some(next) = recover(self.agent.as_ref(), self.turn).await else {
                    return ChildEvent::Closed;
                };
                self.source = next;
                ChildEvent::Recovered
            }
            Received::Closed => ChildEvent::Closed,
        }
    }
}

/// The child's port on its turn's drive: the turn's result, and the
/// reducer that folds the turn's events into the observation every face
/// reads.
struct ChildPort<T> {
    observed: Arc<ChildObservation>,
    turn: TurnId,
    /// Events were lost since the last one reduced.
    gap: bool,
    /// The turn's result, once it came.
    result: Option<T>,
    /// The reducer is done: the turn's terminal or the stream's end.
    reduced: bool,
}

/// The drive's own wake: the result came and no event is ready.
struct Drained;

impl<T> crate::node::TurnPort for ChildPort<T> {
    type Result = T;
    type Event = ChildEvent;
    type Wake = Drained;

    fn pass(&mut self) -> impl Future<Output = crate::node::Pass> {
        std::future::ready(crate::node::Pass::Wait)
    }

    fn awaits_result(&self) -> bool {
        self.result.is_none()
    }

    fn awaits_events(&self) -> bool {
        !self.reduced
    }

    fn urgent_armed(&self) -> bool {
        false
    }

    fn urgent(&self) -> impl Future<Output = Self::Wake> {
        std::future::pending()
    }

    /// Once the result came, the events already delivered are reduced and
    /// then the drive ends: events outrank this wake, so it comes only when
    /// none is ready.
    fn ready(&self) -> impl Future<Output = Self::Wake> {
        let drained = self.result.is_some();
        async move {
            if !drained {
                std::future::pending::<()>().await;
            }
            Drained
        }
    }

    fn on_wake(
        &mut self,
        wake: crate::node::TurnWake<T, ChildEvent, Self::Wake>,
    ) -> impl Future<Output = Flow> {
        let flow = match wake {
            crate::node::TurnWake::Result(result) => {
                self.result = Some(result);
                if self.reduced {
                    Flow::Done
                } else {
                    Flow::Continue
                }
            }
            crate::node::TurnWake::Event(event) => {
                if self.reduce(event) == Flow::Done {
                    // The read model closes with its reducer, as faces
                    // expect, whether or not the result came yet.
                    self.reduced = true;
                    self.observed.close();
                }
                if self.reduced && self.result.is_some() {
                    Flow::Done
                } else {
                    Flow::Continue
                }
            }
            crate::node::TurnWake::Urgent(Drained) | crate::node::TurnWake::Ready(Drained) => {
                Flow::Done
            }
        };
        std::future::ready(flow)
    }
}

impl<T> ChildPort<T> {
    /// Folds one event into the observation; `Done` once the reducer is.
    #[allow(
        clippy::too_many_lines,
        reason = "Child projection coordinates the bounded face stream, committed history catch-up, and cancellation lifetime in one reducer."
    )]
    fn reduce(&mut self, event: ChildEvent) -> Flow {
        let event = match event {
            ChildEvent::Event(event) => event,
            ChildEvent::Lost => {
                self.gap = true;
                return Flow::Continue;
            }
            ChildEvent::Recovered => {
                self.gap = true;
                if let Some(projection) = self.observed.lock().projection.as_mut() {
                    projection.gap_observed = true;
                }
                return Flow::Continue;
            }
            ChildEvent::Closed => return Flow::Done,
        };
        if matches!(event.kind(), plakous::agent::AgentEventKind::AgentClosed) {
            return Flow::Done;
        }
        if event.turn_id() != Some(self.turn) {
            return Flow::Continue;
        }
        let terminal = {
            let mut state = self.observed.lock();
            let Some(projection) = state.projection.as_mut() else {
                return Flow::Done;
            };
            projection.gap_observed |= self.gap;
            let mut ops = super::child_live::transcript_ops(&event, projection);
            for op in &ops {
                if let HostEvent::ToolLine {
                    call_id: Some(id), ..
                } = op
                {
                    if let Some(kept) = projection.child_tools.iter_mut().find(|kept| {
                        matches!(kept,
                            HostEvent::ToolLine { call_id: Some(kept), .. } if kept == id)
                    }) {
                        *kept = op.clone();
                    } else {
                        projection.child_tools.push_back(op.clone());
                        if projection.child_tools.len() > 512 {
                            let oldest_settled = projection.child_tools.iter().position(|op| {
                                matches!(op,
                                    HostEvent::ToolLine { state, .. } if state != "running")
                            });
                            projection.child_tools.remove(oldest_settled.unwrap_or(0));
                        }
                    }
                }
            }
            if self.gap {
                ops.insert(
                    0,
                    HostEvent::Row(crate::sessions::TranscriptRow::info(
                        crate::sessions::NoticeScope::Tree,
                        super::child_live::GAP,
                    )),
                );
            }
            let terminal = projection.progress.terminal;
            state.progress = projection.progress.view.clone();
            state.terminal = terminal;
            state.cursor = state.cursor.saturating_add(1);
            let cursor = state.cursor;
            state.updates.push_back(Update {
                cursor,
                source_sequence: event.cursor().sequence().get(),
                ops: Arc::from(ops),
            });
            while state.updates.len() > RETAINED_UPDATES {
                state.updates.pop_front();
            }
            self.observed.changed.send_replace(cursor);
            terminal
        };
        if terminal {
            return Flow::Done;
        }
        self.gap = false;
        Flow::Continue
    }
}

pub(crate) struct ChildObservation {
    turn: TurnId,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    /// The turn's events and the agent that recovers them, until the
    /// turn's one drive takes them.
    source: Mutex<Option<Pending>>,
}

struct State {
    projection: Option<TurnProjection<Core>>,
    progress: ChildProgress,
    terminal: bool,
    cursor: u64,
    updates: VecDeque<Update>,
    closed: bool,
}

/// No decision with that handle is pending on this turn. An answered or
/// expired handle reads the same as one never offered (spec 02 section
/// 4.3.1).
pub(crate) const DECISION_UNKNOWN: &str = super::APPROVAL_REFUSED_NOTE;

#[derive(Clone)]
pub(super) struct Update {
    pub(super) cursor: u64,
    pub(super) source_sequence: u64,
    pub(super) ops: Arc<[HostEvent<Core>]>,
}

pub(super) struct Snapshot {
    pub(super) cursor: u64,
    pub(super) progress: ChildProgress,
    pub(super) terminal: bool,
    pub(super) tools: Vec<HostEvent<Core>>,
}

/// The only downcast of the hub's opaque host-owned observation capability.
pub(crate) fn of(
    child: &dyn crate::topology_runtime::LiveChildHandle,
) -> Option<Arc<ChildObservation>> {
    let observation: Arc<ChildObservation> = child.host_observation()?.downcast().ok()?;
    (observation.turn == child.turn_id()).then_some(observation)
}

pub(crate) fn tree_needs_operator(tree_id: &str) -> bool {
    struct Attention {
        runtime: std::sync::Weak<crate::topology_runtime::ProductTopologyRuntime>,
        value: bool,
    }
    static LAST: std::sync::LazyLock<Mutex<std::collections::BTreeMap<String, Attention>>> =
        std::sync::LazyLock::new(Mutex::default);
    let Some(runtime) = crate::topology_runtime::lookup(tree_id) else {
        return false;
    };
    let placement_attention = runtime.try_read(|tree| {
        tree.nodes().values().any(|node| {
            node.agent_id != tree.root_agent_id
                && matches!(
                    node.status,
                    crate::agent_tree_protocol::NodeStatus::Running
                        | crate::agent_tree_protocol::NodeStatus::Pending
                        | crate::agent_tree_protocol::NodeStatus::Sealing
                        | crate::agent_tree_protocol::NodeStatus::RecoveryRequired
                )
                && !tree.placement_ready(&node.agent_id)
        })
    });
    let Ok(live) = runtime.live.try_lock() else {
        return placement_attention.unwrap_or(false)
            || LAST
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(tree_id)
                .is_some_and(|last| last.runtime.ptr_eq(&Arc::downgrade(&runtime)) && last.value);
    };
    let value = placement_attention.unwrap_or_else(|| {
        LAST.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tree_id)
            .is_some_and(|last| last.runtime.ptr_eq(&Arc::downgrade(&runtime)) && last.value)
    }) || live.keys().any(|id| {
        live.get(id)
            .and_then(of)
            .is_some_and(|observation| observation.needs_operator())
    });
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    last.retain(|_, last| last.runtime.strong_count() > 0);
    last.insert(
        tree_id.to_owned(),
        Attention {
            runtime: Arc::downgrade(&runtime),
            value,
        },
    );
    value
}

impl ChildObservation {
    fn needs_operator(&self) -> bool {
        let state = self.lock();
        !state.closed
            && !state.terminal
            && state.projection.as_ref().is_some_and(|projection| {
                projection.progress.waiting_for_login || !projection.approvals.is_empty()
            })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Fills the product label without adding another reducer or subscription.
    pub(super) fn set_login_account(&self, account: &str) {
        let mut state = self.lock();
        let Some(projection) = state.projection.as_mut() else {
            return;
        };
        if projection.progress.account.as_deref() == Some(account) {
            return;
        }
        projection.progress.account = Some(account.to_owned());
        if projection.progress.waiting_for_login {
            projection.progress.view.credential_wait = Some(account.to_owned());
        }
        let label = projection.progress.login_notice();
        if projection.progress.waiting_for_login {
            projection.progress.view.activity = Some(label);
        }
        state.progress = projection.progress.view.clone();
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        let state = self.lock();
        Snapshot {
            cursor: state.cursor,
            progress: state.progress.clone(),
            terminal: state.closed || state.terminal,
            tools: Vec::new(),
        }
    }

    pub(super) fn attach(self: &Arc<Self>) -> (Snapshot, Tail) {
        let state = self.lock();
        let snapshot = Snapshot {
            cursor: state.cursor,
            progress: state.progress.clone(),
            terminal: state.closed || state.terminal,
            tools: state
                .projection
                .as_ref()
                .map(|projection| projection.child_tools.iter().cloned().collect())
                .unwrap_or_default(),
        };
        let tail = Tail {
            observation: Arc::clone(self),
            next: state.cursor.saturating_add(1),
            changed: self.changed.subscribe(),
        };
        (snapshot, tail)
    }

    /// The decisions this turn waits on.
    #[cfg(test)]
    pub(crate) fn pending_decisions(&self) -> Vec<String> {
        self.lock()
            .projection
            .as_ref()
            .map(|projection| projection.approvals.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether this turn waits on the decision `approval_id` names.
    pub(crate) fn holds_decision(&self, approval_id: &str) -> bool {
        self.lock()
            .projection
            .as_ref()
            .is_some_and(|projection| projection.approvals.contains_key(approval_id))
    }

    /// Answers the decision `approval_id` names for this turn. The first
    /// answer takes it; a later one is refused. A definite runtime refusal
    /// that leaves the decision pending keeps it offered.
    pub(crate) async fn decide(
        &self,
        control: &TurnControl,
        approval_id: &str,
        choice: plakous_host_protocol::ApprovalChoice,
    ) -> Result<(), String> {
        let taken = self
            .lock()
            .projection
            .as_mut()
            .and_then(|projection| projection.approvals.remove(approval_id));
        let Some(pending) = taken else {
            return Err(DECISION_UNKNOWN.to_owned());
        };
        let decision = pending.decision.clone();
        if let Err(refusal) =
            super::events::submit_decision(control, &decision, approval_id, choice).await
        {
            if refusal.retained
                && let Some(projection) = self.lock().projection.as_mut()
            {
                projection.approvals.insert(approval_id.to_owned(), pending);
            }
            return Err(refusal.message);
        }
        Ok(())
    }

    /// The turn this read model is of.
    pub(crate) const fn turn(&self) -> TurnId {
        self.turn
    }

    /// The turn's one drive: reduces the turn's events into this read model
    /// while it waits for `result`, then reduces what was already delivered
    /// and closes the read model. A turn is driven once; a later drive of
    /// it only waits for its result. A drive dropped before it closed the
    /// read model leaves the next drive a recovery of the turn's events,
    /// with a gap marked (or, with no agent to recover from, the close).
    pub(crate) async fn drive<T>(self: Arc<Self>, result: impl Future<Output = T>) -> T {
        let taken = self
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(Pending { source, agent, gap }) = taken else {
            return result.await;
        };
        let _undriven = Undriven {
            observed: Arc::clone(&self),
            agent: agent.clone(),
        };
        tokio::pin!(result);
        // A source at hand is taken even when the result already came, so a
        // turn that ended before its drive still has its delivered events
        // reduced; a recovery that cannot attach yields to the result.
        let source = tokio::select! {
            biased;
            source = source => source,
            result = &mut result => {
                self.close();
                return result;
            }
        };
        let Some(source) = source else {
            self.close();
            return result.await;
        };
        let mut events = ChildEvents {
            source,
            agent,
            turn: self.turn,
        };
        let mut drive = crate::node::TurnDrive {
            port: ChildPort {
                observed: Arc::clone(&self),
                turn: self.turn,
                gap,
                result: None,
                reduced: false,
            },
        };
        drive.run(&mut events, &mut result).await;
        self.close();
        match drive.port.result.take() {
            Some(result) => result,
            // The reducer ended the drive before the result came.
            None => result.await,
        }
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        // Faces may retain the closed read model, but never its live reducer.
        drop(state.projection.take());
        self.changed.send_replace(state.cursor);
    }
}

/// Held by a drive until it ends: a drive dropped before the read model
/// closed hands the turn's events back as a recovery.
struct Undriven {
    observed: Arc<ChildObservation>,
    agent: Option<Agent>,
}

impl Drop for Undriven {
    fn drop(&mut self) {
        if self.observed.lock().closed {
            return;
        }
        let (agent, turn) = (self.agent.take(), self.observed.turn);
        let recovering = agent.clone();
        *self
            .observed
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Pending {
            source: Box::pin(async move { recover(recovering.as_ref(), turn).await }),
            agent,
            gap: true,
        });
    }
}

pub(super) struct Tail {
    observation: Arc<ChildObservation>,
    next: u64,
    changed: watch::Receiver<u64>,
}

impl Tail {
    pub(super) async fn recv(&mut self) -> Result<Option<Update>, ()> {
        loop {
            {
                let state = self.observation.lock();
                if state
                    .updates
                    .front()
                    .is_some_and(|update| self.next < update.cursor)
                {
                    return Err(());
                }
                if let Some(update) = state
                    .updates
                    .front()
                    .and_then(|first| self.next.checked_sub(first.cursor))
                    .and_then(|offset| usize::try_from(offset).ok())
                    .and_then(|offset| state.updates.get(offset))
                {
                    self.next = self.next.saturating_add(1);
                    return Ok(Some(update.clone()));
                }
                if state.closed {
                    return Ok(None);
                }
            }
            if self.changed.changed().await.is_err() {
                return Ok(None);
            }
        }
    }
}
