//! One admission-owned child reducer, shared by every host face.

use super::{HostEvent, TurnProjection};
use plakous::agent::{
    Agent, AgentEvent, CommandAck, EventDelivery, EventSubscription, SubscriptionEnd, Turn,
    TurnControl, TurnEventError, TurnEvents, TurnResultHandle, TurnSubmission, TurnViewState,
};
use plakous::model::TurnId;
use plakous_host_protocol::{Core, views::ChildProgress};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;
use tokio::task::JoinHandle;

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
    task: JoinHandle<()>,
}

impl Drop for ChildTurnProjection {
    fn drop(&mut self) {
        self.observation.close();
        self.task.abort();
    }
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

    #[allow(
        clippy::too_many_lines,
        reason = "Child projection coordinates the bounded face stream, committed history catch-up, and cancellation lifetime in one task."
    )]
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
            state: Mutex::new(State {
                projection: Some(projection),
                progress,
                terminal: false,
                cursor: 0,
                updates: VecDeque::new(),
                closed: false,
            }),
        });
        let observed = Arc::clone(&observation);
        let task = tokio::spawn(async move {
            if let Some(mut source) = source.await {
                let mut gap = false;
                loop {
                    let event = match source.recv().await {
                        Received::Event(event) => event,
                        Received::Lost => {
                            gap = true;
                            continue;
                        }
                        Received::Recover => {
                            gap = true;
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            let Some(next) = recover(agent.as_ref(), turn).await else {
                                break;
                            };
                            source = next;
                            if let Some(projection) = observed.lock().projection.as_mut() {
                                projection.gap_observed = true;
                            }
                            continue;
                        }
                        Received::Closed => break,
                    };
                    if matches!(event.kind(), plakous::agent::AgentEventKind::AgentClosed) {
                        break;
                    }
                    if event.turn_id() != Some(turn) {
                        continue;
                    }
                    let terminal = {
                        let mut state = observed.lock();
                        let Some(projection) = state.projection.as_mut() else {
                            break;
                        };
                        projection.gap_observed |= gap;
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
                                        let oldest_settled =
                                            projection.child_tools.iter().position(|op| {
                                                matches!(op,
                                            HostEvent::ToolLine { state, .. } if state != "running")
                                            });
                                        projection.child_tools.remove(oldest_settled.unwrap_or(0));
                                    }
                                }
                            }
                        }
                        if gap {
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
                        observed.changed.send_replace(cursor);
                        terminal
                    };
                    if terminal {
                        break;
                    }
                    gap = false;
                }
            }
            observed.close();
        });
        Self { observation, task }
    }
}

pub(crate) struct ChildObservation {
    turn: TurnId,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

struct State {
    projection: Option<TurnProjection<Core>>,
    progress: ChildProgress,
    terminal: bool,
    cursor: u64,
    updates: VecDeque<Update>,
    closed: bool,
}

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

impl ChildObservation {
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

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        // Faces may retain the closed read model, but never its live reducer.
        drop(state.projection.take());
        self.changed.send_replace(state.cursor);
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
