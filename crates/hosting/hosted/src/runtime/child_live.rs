//! Non-authoritative child observations, independent of admission and turn control.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::TaskHandle;
use plakous::agent::{
    Agent, AgentEvent, AgentEventKind, AgentViewSnapshot, TurnViewState, ViewRequest,
};
use plakous::model::{ModelEvent, TurnId, Usage};
use plakous::sessions::{AgentSessions, SessionLocator, SessionTranscriptSource};
use plakous_host_protocol::Core;
use plakous_host_protocol::views::transcript::{TranscriptItem, TranscriptScope};
use plakous_host_protocol::views::{ChildProgress, PresentationSnapshot, TreeView};

use super::child_projection::ChildObservation;
#[cfg(test)]
use super::events::progress::Progress;
use super::{HostEvent, Sender, TurnProjection, UserAction};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::topology_runtime::ProductTopologyRuntime;

const MAX_CHILDREN: usize = 8;
pub(super) const GAP: &str = "earlier activity not shown";

type ObservedChild = (String, TurnId, Agent, Arc<ChildObservation>);

struct Observer {
    turn_id: TurnId,
    observation: Arc<ChildObservation>,
}

/// Restarts an open agent view's serving task, optionally on a live turn,
/// under a send generation.
type ServeAgent =
    Box<dyn Fn(Option<(TurnId, Agent, Arc<ChildObservation>)>, u64, bool) -> TaskHandle<()> + Send>;

/// The send generation of a face's agent view. A serve sends only while the
/// generation it was started under is current; close, replace, a new turn
/// and a session switch bump it under the same lock the check holds, so no
/// send of an earlier serve lands after them.
#[derive(Default)]
struct ViewGate {
    generation: Mutex<u64>,
}

impl ViewGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        self.generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self) -> u64 {
        let mut generation = self.lock();
        *generation = generation.wrapping_add(1);
        *generation
    }

    fn send_if_current(&self, generation: u64, send: impl FnOnce() -> bool) -> bool {
        let current = self.lock();
        *current == generation && send()
    }
}

/// One history projection at a time per face, off the async workers.
struct ReadPermit(Arc<tokio::sync::Semaphore>);

impl Default for ReadPermit {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(1)))
    }
}

/// The one agent transcript a host face has open (spec 03 §16). Its identity
/// is the agent id: tree revisions never reset it.
struct AgentView {
    agent_id: String,
    /// The product session it was opened in; another session drops it.
    session_id: String,
    /// The live turn the serving task attached to, when there was one.
    turn_id: Option<TurnId>,
    serve: ServeAgent,
    task: TaskHandle<()>,
}

impl Drop for AgentView {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Owned by one host face; dropping the face drops every child subscription.
#[derive(Default)]
pub(super) struct ChildObservers {
    tree_id: Option<String>,
    held: std::collections::BTreeSet<String>,
    children: BTreeMap<String, Observer>,
    agent_view: Option<AgentView>,
    gate: Arc<ViewGate>,
    reads: ReadPermit,
    /// The host's job registry, for an agent view's process subjects.
    jobs: Option<Arc<crate::jobs::JobRegistry>>,
    published: BTreeMap<String, ChildProgress>,
}

fn running_children(runtime: &ProductTopologyRuntime) -> Option<Vec<ObservedChild>> {
    let live = runtime.live.try_lock().ok()?;
    Some(
        live.keys()
            .filter_map(|id| {
                let child = live.get(id)?;
                Some((
                    id.clone(),
                    child.turn_id(),
                    child.agent()?,
                    super::child_projection::of(child)?,
                ))
            })
            .collect(),
    )
}

impl ChildObservers {
    pub(super) fn sync(&mut self, runtime: &ProductTopologyRuntime) {
        let Some(children) = running_children(runtime) else {
            return;
        };
        let _ = runtime.try_read(|tree| {
            for (id, _, _, observation) in &children {
                if let Ok(node) = tree.node(id) {
                    observation.set_login_account(&node.account);
                }
            }
        });
        self.sync_children(children);
    }

    fn sync_children(&mut self, mut children: Vec<ObservedChild>) {
        self.follow_agent_view_turn(&children);
        children.sort_by(|(left, turn, ..), (right, right_turn, ..)| {
            let existing = |id: &String, turn: &TurnId| {
                self.children
                    .get(id)
                    .is_some_and(|observer| observer.turn_id == *turn)
            };
            (!existing(left, turn), left).cmp(&(!existing(right, right_turn), right))
        });
        children.truncate(MAX_CHILDREN);
        self.children = children
            .into_iter()
            .map(|(id, turn_id, _, observation)| {
                (
                    id,
                    Observer {
                        turn_id,
                        observation,
                    },
                )
            })
            .collect();
    }

    fn material_progress(&self) -> BTreeMap<String, ChildProgress> {
        self.children
            .iter()
            .filter_map(|(id, observer)| {
                let state = observer.observation.snapshot();
                if state.terminal {
                    return None;
                }
                let mut view = state.progress;
                view.last_activity_unix_ms = None;
                Some((id.clone(), view))
            })
            .collect()
    }

    pub(super) fn progress_changed(&self) -> bool {
        self.published != self.material_progress()
            || self.held_now().is_some_and(|held| held != self.held)
    }

    fn held_now(&self) -> Option<std::collections::BTreeSet<String>> {
        let runtime = crate::topology_runtime::lookup(self.tree_id.as_deref()?)?;
        runtime.try_read(|tree| {
            tree.nodes()
                .values()
                .filter(|node| {
                    node.agent_id != tree.root_agent_id
                        && matches!(
                            node.status,
                            plakous_host_protocol::topology::NodeStatus::Running
                                | plakous_host_protocol::topology::NodeStatus::Sealing
                        )
                        && tree.placement_held(&node.agent_id)
                })
                .map(|node| node.agent_id.clone())
                .collect()
        })
    }

    /// Progress can change without a durable tree revision. Publish a complete
    /// snapshot for either kind of change; never manufacture a tree revision.
    pub(super) fn project(&mut self, view: &mut TreeView) -> bool {
        self.tree_id = Some(view.tree_id.clone());
        let held = self.held_now().unwrap_or_else(|| self.held.clone());
        let held_changed = held != self.held;
        self.held = held;
        let mut progress = BTreeMap::new();
        for node in &mut view.nodes {
            node.live_progress = self.children.get(&node.agent_id).and_then(|observer| {
                let state = observer.observation.snapshot();
                (!state.terminal).then_some(state.progress)
            });
            if let Some(value) = &node.live_progress {
                let mut value = value.clone();
                value.last_activity_unix_ms = None;
                progress.insert(node.agent_id.clone(), value);
            }
            if self.held.contains(&node.agent_id)
                && let Some(progress) = &mut node.live_progress
            {
                progress.activity = Some(format!("tools waiting · {}", node.runner.name));
            }
        }
        let changed = held_changed || self.published != progress;
        self.published = progress;
        changed
    }

    /// A new turn of the open agent restarts its view: fresh committed rows,
    /// then that turn's ops. The same turn, or none, changes nothing.
    fn follow_agent_view_turn(&mut self, children: &[ObservedChild]) {
        let Some(view) = self.agent_view.as_mut() else {
            return;
        };
        let Some((_, turn_id, agent, observation)) =
            children.iter().find(|(id, ..)| *id == view.agent_id)
        else {
            return;
        };
        if view.turn_id == Some(*turn_id) {
            return;
        }
        view.task.abort();
        view.turn_id = Some(*turn_id);
        let generation = self.gate.bump();
        view.task = (view.serve)(
            Some((*turn_id, agent.clone(), observation.clone())),
            generation,
            false,
        );
    }

    /// Closes the open agent view; nothing it started sends afterwards.
    pub(super) fn close_agent_view(&mut self) {
        self.agent_view = None;
        self.gate.bump();
    }

    pub(super) fn close_agent_transcript<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        events: &Sender<P>,
    ) {
        self.close_agent_view();
        events.retire_agent_transcript();
    }

    /// The job registry an agent view reads process subjects against.
    pub(super) fn set_jobs(&mut self, jobs: Arc<crate::jobs::JobRegistry>) {
        self.jobs = Some(jobs);
    }

    /// Drops the open agent view when the face has moved to another session.
    pub(super) fn retain_session(&mut self, session_id: &str) {
        if self
            .agent_view
            .as_ref()
            .is_some_and(|view| view.session_id != session_id)
        {
            self.close_agent_view();
        }
    }

    /// Opens `agent_id`'s transcript, replacing any open one. Reopening the
    /// open agent on the same live turn changes nothing. A node outside the
    /// attached tree, the root, a node with no bound session and a host with
    /// no session store answer at once with a note and open nothing.
    pub(super) async fn open_agent_view<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        tasks: &crate::TaskSet,
        runtime: &ProductTopologyRuntime,
        sessions: Option<Arc<AgentSessions>>,
        session_id: &str,
        agent_id: &str,
        events: &Sender<P>,
    ) {
        let locator = runtime
            .read(|tree| crate::call_detail::child_locator(tree, agent_id))
            .unwrap_or_else(|_| Err("the agent tree is unavailable".to_owned()));
        let (locator, sessions) = match (locator, sessions) {
            (Ok(locator), Some(sessions)) => (locator, sessions),
            (Err(note), _) => {
                self.close_agent_view();
                send_agent_transcript(events, agent_id, Err(note));
                return;
            }
            (Ok(_), None) => {
                self.close_agent_view();
                send_agent_transcript(
                    events,
                    agent_id,
                    Err("transcript unavailable: no session store is open".to_owned()),
                );
                return;
            }
        };
        let live = {
            let live = runtime.live.lock().await;
            live.get(agent_id).and_then(|child| {
                Some((
                    child.turn_id(),
                    child.agent()?,
                    super::child_projection::of(child)?,
                ))
            })
        };
        if live
            .as_ref()
            .is_some_and(|(turn_id, ..)| self.serves(session_id, agent_id, *turn_id))
        {
            return;
        }
        let task_tools = runtime
            .read(|tree| {
                if let Some((_, _, observation)) = &live
                    && let Ok(node) = tree.node(agent_id)
                {
                    observation.set_login_account(&node.account);
                }
                node_presents_task_tools(tree, agent_id)
            })
            .unwrap_or(false);
        self.start_agent_view(
            tasks, task_tools, session_id, agent_id, locator, sessions, live, events,
        );
    }

    /// Whether the open view already serves `agent_id`'s live `turn_id`.
    fn serves(&self, session_id: &str, agent_id: &str, turn_id: TurnId) -> bool {
        self.agent_view.as_ref().is_some_and(|view| {
            view.agent_id == agent_id
                && view.session_id == session_id
                && view.turn_id == Some(turn_id)
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "An agent-view task owns its executor and cancellation while borrowing the selected session, tree, and event transport."
    )]
    fn start_agent_view<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        tasks: &crate::TaskSet,
        task_tools: bool,
        session_id: &str,
        agent_id: &str,
        locator: SessionLocator,
        sessions: Arc<AgentSessions>,
        live: Option<(TurnId, Agent, Arc<ChildObservation>)>,
        events: &Sender<P>,
    ) {
        self.close_agent_view();
        let context = ServeContext {
            agent_id: agent_id.to_owned(),
            locator,
            sessions,
            jobs: self.jobs.clone(),
            events: events.clone(),
            gate: Arc::clone(&self.gate),
            generation: 0,
            reads: Arc::clone(&self.reads.0),
            last_good: Arc::new(Mutex::new(None)),
            task_tools,
        };
        let tasks = tasks.clone();
        let serve: ServeAgent = Box::new(move |live, generation, replace| {
            tasks.spawn(serve_agent(
                ServeContext {
                    generation,
                    ..context.clone()
                },
                live,
                replace,
            ))
        });
        let turn_id = live.as_ref().map(|(turn_id, ..)| *turn_id);
        let task = serve(live, self.gate.bump(), true);
        self.agent_view = Some(AgentView {
            agent_id: agent_id.to_owned(),
            session_id: session_id.to_owned(),
            turn_id,
            serve,
            task,
        });
    }

    pub(super) async fn action<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        tasks: &crate::TaskSet,
        runtime: &ProductTopologyRuntime,
        sessions: Option<Arc<AgentSessions>>,
        session_id: &str,
        action: &UserAction<P>,
        events: &Sender<P>,
    ) {
        match action {
            UserAction::OpenAgentTranscript { agent_id } => {
                self.open_agent_view(tasks, runtime, sessions, session_id, agent_id, events)
                    .await;
            }
            UserAction::CloseAgentTranscript => {
                self.close_agent_transcript(events);
            }
            _ => {}
        }
    }
}

/// A restored Grok output or kill call, or pi wait or stop call, reads as the
/// await or the interrupt only on a node that was granted those slots and
/// whose bound account and model compile a pack that claims them (the
/// vocabulary [`crate::agent::vocabulary_for_presentation`] gives the
/// presentation). A leaf keeps the process-tool name.
fn node_presents_task_tools(tree: &crate::agent_tree::ProductAgentTree, agent_id: &str) -> bool {
    let Ok(node) = tree.node(agent_id) else {
        return false;
    };
    let Some(grant) = tree.grants().get(agent_id) else {
        return false;
    };
    let slots = grant.compiled_tool_names(node.parent_agent_id.is_none(), true);
    slots.contains(&"wait_agent")
        && crate::agent::node_coding_presentation(&node.account, &node.tuple_id)
            .map(crate::agent::vocabulary_for_presentation)
            .is_some_and(|vocabulary| {
                vocabulary != crate::agent_tree_tools::TopologyVocabulary::Plak
            })
}

pub(super) fn send_agent_transcript<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    agent_id: &str,
    rows: Result<Vec<TranscriptItem>, String>,
) {
    let items = match rows {
        Ok(items) => items,
        Err(note) => vec![TranscriptItem {
            key: crate::sessions::provisional_key(),
            row: TranscriptRow::info(NoticeScope::Tree, note),
        }],
    };
    let _ = events.replace_transcript(PresentationSnapshot {
        scope: TranscriptScope::Agent(agent_id.to_owned()),
        items,
        ..PresentationSnapshot::default()
    });
}

/// What one serve of an agent view reads from and sends through.
struct ServeContext<P: plakous_host_protocol::ProtocolExtension> {
    agent_id: String,
    locator: SessionLocator,
    sessions: Arc<AgentSessions>,
    jobs: Option<Arc<crate::jobs::JobRegistry>>,
    events: Sender<P>,
    gate: Arc<ViewGate>,
    generation: u64,
    reads: Arc<tokio::sync::Semaphore>,
    /// The rows of the view's last good read, re-sent beside a failure note.
    last_good: Arc<Mutex<Option<Vec<TranscriptItem>>>>,
    /// This node presents the await and interrupt as Grok's task tools, so a
    /// restored output or kill call naming a child reads as that slot. A leaf
    /// does not, and a refused process call keeps its own name.
    task_tools: bool,
}

impl<P: plakous_host_protocol::ProtocolExtension> Clone for ServeContext<P> {
    fn clone(&self) -> Self {
        Self {
            agent_id: self.agent_id.clone(),
            locator: self.locator,
            sessions: Arc::clone(&self.sessions),
            jobs: self.jobs.clone(),
            events: self.events.clone(),
            gate: Arc::clone(&self.gate),
            generation: self.generation,
            reads: Arc::clone(&self.reads),
            last_good: Arc::clone(&self.last_good),
            task_tools: self.task_tools,
        }
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> ServeContext<P> {
    /// Projects `source` to rows on the blocking pool, one projection per
    /// face at a time. The permit moves into the projection, so a serve
    /// aborted mid-read still holds it until the work ends.
    async fn project(
        &self,
        source: SessionTranscriptSource,
    ) -> Result<Vec<TranscriptItem>, String> {
        let permit = Arc::clone(&self.reads)
            .acquire_owned()
            .await
            .map_err(|_| "transcript unavailable: the host is closing".to_owned())?;
        let processes = self
            .jobs
            .as_ref()
            .map(|jobs| jobs.snapshot())
            .unwrap_or_default();
        let task_tools = self.task_tools;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            super::agent_transcript::items_from_source(&source, &processes, task_tools)
        })
        .await
        .map_err(|_| "transcript unavailable: the read did not finish".to_owned())
    }

    /// The node's committed rows from its bound session in the store.
    async fn stored_rows(&self) -> Result<Vec<TranscriptItem>, String> {
        let source = super::agent_transcript::read_source(&self.sessions, self.locator).await?;
        self.project(source).await
    }

    /// A live agent's committed rows from an atomic attach, or from the
    /// store when it cannot be attached.
    async fn committed_rows(&self, agent: &Agent) -> Result<Vec<TranscriptItem>, String> {
        match SessionTranscriptSource::attach(agent, ViewRequest::content()) {
            Ok((_, source)) => self.project(source).await,
            Err(_) => self.stored_rows().await,
        }
    }

    /// Sends the view's rows, or its last good rows beside the failure note.
    fn send_transcript(&self, rows: Result<Vec<TranscriptItem>, String>, replace: bool) -> bool {
        // The last good rows change only under the gate, so a serve whose
        // generation is retired never replaces what a current one sent.
        self.gate.send_if_current(self.generation, || {
            let mut last_good = self
                .last_good
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut items = match rows {
                Ok(rows) => {
                    *last_good = Some(rows.clone());
                    rows
                }
                Err(note) => {
                    let mut items = last_good.clone().unwrap_or_default();
                    items.push(TranscriptItem {
                        key: crate::sessions::provisional_key(),
                        row: TranscriptRow::info(NoticeScope::Tree, note),
                    });
                    items
                }
            };
            drop(last_good);
            items.sort_by_key(|item| item.key);
            let snapshot = PresentationSnapshot {
                scope: TranscriptScope::Agent(self.agent_id.clone()),
                items,
                ..PresentationSnapshot::default()
            };
            if replace {
                self.events.replace_transcript(snapshot)
            } else {
                self.events.publish_transcript(snapshot)
            }
        })
    }

    fn emitter(&self) -> impl Fn(HostEvent<Core>) -> bool + Send + 'static {
        let emit = child_live_emitter(&self.agent_id, &self.events);
        let gate = Arc::clone(&self.gate);
        let generation = self.generation;
        move |op| gate.send_if_current(generation, || emit(op))
    }
}

/// Serves one open agent view: its committed rows, then, when it has a live
/// turn that has not ended, that turn's ops, then fresh committed rows once
/// the turn ends so the view converges on committed history. A live turn's
/// tail is registered first at the hosted observation cursor, then history is
/// captured at the core attachment cursor. Reduced updates at or before that
/// history cursor are skipped; later updates remain in the registered tail.
/// This two-cursor rule cannot lose an intervening live update or repeat a
/// committed item. Missing uncommitted prefixes are marked with a gap notice.
/// A node with no live turn is read from its bound session in the store.
async fn serve_agent<P: plakous_host_protocol::ProtocolExtension>(
    context: ServeContext<P>,
    live: Option<(TurnId, Agent, Arc<ChildObservation>)>,
    replace: bool,
) {
    let Some((turn_id, agent, observation)) = live else {
        let rows = context.stored_rows().await;
        context.send_transcript(rows, replace);
        return;
    };
    let (live, tail) = observation.attach();
    #[cfg(test)]
    hooks::before_attach(&context.agent_id).await;
    let Ok((attachment, source)) = SessionTranscriptSource::attach(&agent, ViewRequest::content())
    else {
        let rows = context.stored_rows().await;
        context.send_transcript(rows, replace);
        return;
    };
    let (snapshot, unused_tail) = attachment.into_parts();
    drop(unused_tail);
    let rows = context.project(source).await;
    if turn_ended(&snapshot, turn_id) {
        context.send_transcript(rows, replace);
        return;
    }
    if live.terminal {
        let rows = context.committed_rows(&agent).await;
        context.send_transcript(rows, replace);
        return;
    }
    let covered_calls = rows.as_deref().map(covered_tool_calls).unwrap_or_default();
    if !context.send_transcript(rows, replace) {
        return;
    }
    let emit = context.emitter();
    if live.cursor > 1 {
        emit(HostEvent::Row(TranscriptRow::info(NoticeScope::Tree, GAP)));
    }
    emit_retained_tools(live.tools, &covered_calls, &emit);
    #[cfg(test)]
    hooks::before_follow(&context.agent_id).await;
    follow_turn(
        observation,
        tail,
        snapshot.cursor().sequence().get(),
        covered_calls,
        live.cursor > 1,
        emit,
    )
    .await;
    let rows = context.committed_rows(&agent).await;
    context.send_transcript(rows, false);
}

pub(super) fn covered_tool_calls(rows: &[TranscriptItem]) -> std::collections::HashSet<String> {
    rows.iter()
        .filter_map(|item| item.row.as_tool())
        .filter(|tool| tool.state != "interrupted")
        .map(|tool| tool.call_id.clone())
        .collect()
}

pub(super) fn emit_retained_tools(
    tools: Vec<HostEvent<Core>>,
    covered: &std::collections::HashSet<String>,
    emit: &impl Fn(HostEvent<Core>) -> bool,
) {
    for tool in tools {
        if matches!(&tool, HostEvent::ToolLine {call_id: Some(id), ..} if !covered.contains(id)) {
            emit(tool);
        }
    }
}

/// Reduces one agent observation through the root's keyed row projector.
fn child_live_emitter<P: plakous_host_protocol::ProtocolExtension>(
    agent_id: &str,
    events: &Sender<P>,
) -> impl Fn(HostEvent<Core>) -> bool + Send + 'static {
    let id = agent_id.to_owned();
    let sender = events.clone();
    move |op| sender.observe_agent(&id, &op)
}

/// Follows already-reduced child observations; a lagging face resynchronizes
/// its cursor without ever creating another reducer.
pub(super) async fn follow_turn(
    observation: Arc<ChildObservation>,
    mut tail: super::child_projection::Tail,
    committed_through: u64,
    covered_calls: std::collections::HashSet<String>,
    mut gap_emitted: bool,
    emit: impl Fn(HostEvent<Core>) -> bool,
) {
    let mut retry_after = Instant::now() + Duration::from_secs(1);
    loop {
        match tail.recv().await {
            Ok(Some(update)) => {
                if update.source_sequence <= committed_through {
                    for op in update.ops.iter() {
                        if matches!(op, HostEvent::ToolLine { call_id: Some(id), .. } if !covered_calls.contains(id))
                        {
                            emit(op.clone());
                        }
                    }
                    if !gap_emitted && !update.ops.is_empty() {
                        emit(HostEvent::Row(TranscriptRow::info(NoticeScope::Tree, GAP)));
                        gap_emitted = true;
                    }
                    continue;
                }
                for op in update.ops.iter() {
                    if !emit(op.clone()) {
                        emit(HostEvent::Row(TranscriptRow::info(NoticeScope::Tree, GAP)));
                    }
                }
            }
            Ok(None) => break,
            Err(()) => {
                tokio::time::sleep_until(retry_after.into()).await;
                retry_after = Instant::now() + Duration::from_secs(1);
                emit(HostEvent::Row(TranscriptRow::info(NoticeScope::Tree, GAP)));
                let (snapshot, next) = observation.attach();
                if snapshot.terminal {
                    break;
                }
                emit_retained_tools(snapshot.tools, &covered_calls, &emit);
                tail = next;
            }
        }
    }
}

fn turn_ended(snapshot: &AgentViewSnapshot, turn_id: TurnId) -> bool {
    snapshot
        .turns()
        .iter()
        .any(|turn| turn.turn_id() == turn_id && turn.state() != TurnViewState::Running)
}

fn usage_op(usage: Usage, terminal: bool) -> HostEvent<Core> {
    HostEvent::TurnUsage {
        input_tokens: usage.input_tokens(),
        output_tokens: usage.output_tokens(),
        reasoning_tokens: usage.reasoning_tokens(),
        cached_input_tokens: usage.cached_input_tokens(),
        cache_write_tokens: usage.cache_write_tokens(),
        model_operations: None,
        reporting_model_operations: None,
        peak_prepared_input_tokens: None,
        terminal,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Transcript diffing preserves row identity while reconciling append, replacement, truncation, and gap-recovery operations."
)]
pub(super) fn transcript_ops(
    event: &AgentEvent,
    projection: &mut TurnProjection<Core>,
) -> Vec<HostEvent<Core>> {
    let mut ops = Vec::new();
    projection
        .progress
        .observe(event.kind(), event.attempt_id().zip(event.call_id()));
    match event.kind() {
        AgentEventKind::ModelCredentialWait { waiting: true } => {
            ops.push(HostEvent::Row(
                TranscriptRow::info(NoticeScope::Settings, projection.progress.login_notice())
                    .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::Login {
                        account: projection
                            .progress
                            .account
                            .as_deref()
                            .map(plakous_host_protocol::notice_remedy::AccountTarget::from_account)
                            .unwrap_or(
                                plakous_host_protocol::notice_remedy::AccountTarget::Unspecified,
                            ),
                    }),
            ));
        }
        AgentEventKind::Model {
            event: Some(model), ..
        } => match model.as_ref() {
            ModelEvent::TextDelta(delta) => {
                if let Ok(text) = std::str::from_utf8(delta.bytes()) {
                    ops.push(HostEvent::AssistantDelta(text.to_owned()));
                }
            }
            ModelEvent::BlockStarted(started) => projection.observe_block_start(started),
            ModelEvent::ReasoningDelta(delta) => {
                if let Some(text) = projection.observe_reasoning_delta(delta) {
                    ops.push(HostEvent::ReasoningDelta {
                        location_key: super::events::reasoning_location_key(delta.location()),
                        text,
                    });
                }
            }
            ModelEvent::ReasoningVisibilityBound(bound) => {
                if let Some(text) = projection.bind_reasoning_visibility(*bound) {
                    ops.push(HostEvent::ReasoningDelta {
                        location_key: super::events::reasoning_location_key(bound.location()),
                        text,
                    });
                }
            }
            ModelEvent::BlockCompleted(completed) => {
                if let Some(sealed) = projection.seal_reasoning(completed.location()) {
                    ops.push(HostEvent::ReasoningSealed {
                        location_key: sealed.location_key,
                    });
                }
            }
            ModelEvent::Usage(_) => {
                let view = &projection.progress.view;
                ops.push(usage_op(
                    Usage::new(
                        view.input_tokens,
                        view.output_tokens,
                        view.reasoning_tokens,
                        view.cached_input_tokens,
                        view.cache_write_tokens,
                    ),
                    false,
                ));
            }
            ModelEvent::ResponseCompleted(_) => ops.push(HostEvent::AssistantDone),
            _ => {}
        },
        AgentEventKind::ToolStarted {
            identity,
            lineage,
            observation,
        } => {
            let arguments = observation.as_ref().and_then(|item| {
                serde_json::from_str::<serde_json::Value>(item.arguments_preview()).ok()
            });
            let call_id = event.call_id().map(|id| id.to_string());
            let name = super::events::presented_start_name(
                projection,
                call_id.as_deref(),
                identity
                    .exposed_name()
                    .unwrap_or_else(|| identity.canonical_name())
                    .as_str(),
                arguments.as_ref(),
                plakous_host_protocol::topology::is_topology_registration(
                    identity.registered_tool_id().as_str(),
                ),
            );
            let nested_parent = super::tree::nested_parent_call_id(lineage, projection);
            let parent_call_id = nested_parent
                .clone()
                .or_else(|| super::tree::continuation_parent_call_id(&name, lineage, projection));
            let kind = crate::activity::tool_activity_kind(&name);
            if kind == plakous_host_protocol::views::presentation::ActivityKind::Compose
                && lineage.is_composition_marker()
                && let Some(call_id) = call_id.as_deref()
            {
                projection.open_compose(call_id, &name, lineage.cell_id());
            }
            if let Some(parent) = nested_parent.as_deref() {
                projection.learn_cell_owner(parent, lineage.cell_id());
            }
            let label = super::tools::process_observation_label(
                &name,
                None,
                arguments.as_ref().unwrap_or(&serde_json::Value::Null),
            );
            let text = super::events::started_tool_text(
                &name,
                None,
                observation.as_ref(),
                None,
                super::events::ProcessContext {
                    label,
                    args: arguments.as_ref(),
                    locator: None,
                    processes: &[],
                    commands: &std::collections::HashMap::new(),
                },
            );
            if let Some(id) = call_id.as_ref() {
                projection.progress.subjects.retain(|(call, _)| call != id);
                if projection.progress.subjects.len() >= 512 {
                    projection.progress.subjects.pop_front();
                }
                projection
                    .progress
                    .subjects
                    .push_back((id.clone(), text.subject.clone()));
            }
            ops.push(HostEvent::ToolLine {
                call_id: event.call_id().map(|id| id.to_string()),
                parent_call_id,
                cell_id: lineage.cell_id().map(str::to_owned),
                kind,
                name,
                state: "running".into(),
                subject: text.subject,
                detail: text.detail,
                duration_ms: None,
                started_at_ms: Some(super::events::event_timestamp_ms(event)),
                finished_at_ms: None,
                wait: None,
            });
        }
        AgentEventKind::ToolCompleted {
            identity,
            lineage,
            observation,
        } => {
            let call_id = event.call_id().map(|id| id.to_string());
            let name = super::events::presented_later_name(
                projection,
                call_id.as_deref(),
                identity
                    .exposed_name()
                    .unwrap_or_else(|| identity.canonical_name())
                    .as_str()
                    .to_owned(),
                true,
            );
            let result = observation
                .as_ref()
                .filter(|value| value.succeeded())
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value.preview()).ok());
            let name = plakous_host_protocol::topology::presented_settled_name(
                &name,
                None,
                result.as_ref(),
                plakous_host_protocol::topology::is_topology_registration(
                    identity.registered_tool_id().as_str(),
                ),
            )
            .to_owned();
            if lineage.is_composition_marker()
                && let Some(call_id) = call_id.as_deref()
            {
                projection.learn_marker_cell(call_id, lineage.cell_id());
            }
            let parent_call_id = super::tree::nested_parent_call_id(lineage, projection)
                .or_else(|| super::tree::continuation_parent_call_id(&name, lineage, projection));
            let outcome = observation
                .as_ref()
                .map(|value| (value.succeeded(), value.preview()));
            let state = super::events::completed_tool_state(&name, outcome);
            let state = if state == "succeeded" {
                "done".to_owned()
            } else {
                state
            };
            let subject = call_id
                .as_ref()
                .and_then(|id| {
                    projection
                        .progress
                        .subjects
                        .iter()
                        .position(|(call, _)| call == id)
                })
                .and_then(|index| projection.progress.subjects.remove(index))
                .and_then(|(_, subject)| subject);
            let subject = super::events::completed_tool_summary(
                &name,
                subject.as_deref(),
                outcome.map(|(_, preview)| preview),
            );
            ops.push(HostEvent::ToolLine {
                call_id: event.call_id().map(|id| id.to_string()),
                parent_call_id,
                cell_id: lineage.cell_id().map(str::to_owned),
                kind: crate::activity::tool_activity_kind(&name),
                name,
                state,
                subject,
                detail: observation.as_ref().and_then(|value| {
                    crate::activity::tool_detail_text(None, Some(value.preview()))
                }),
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: Some(super::events::event_timestamp_ms(event)),
                wait: None,
            });
        }
        AgentEventKind::TurnCompleted { usage } | AgentEventKind::TurnFailed { usage, .. } => {
            ops.push(usage_op(usage.model(), true));
            ops.push(HostEvent::AssistantDone);
            if matches!(event.kind(), AgentEventKind::TurnFailed { .. }) {
                ops.push(HostEvent::Row(TranscriptRow::error(
                    NoticeScope::Tree,
                    "child turn failed",
                )));
            }
            ops.push(HostEvent::SetBusy(false));
        }
        // The child's own decision handle, offered like the root's; the
        // child's actor is its single decider.
        AgentEventKind::EffectDecisionRequired {
            identity,
            decision,
            prompt,
            expires_at,
        } => {
            let approval_id = format!("approval-{}", uuid::Uuid::new_v4());
            projection.approvals.insert(
                approval_id.clone(),
                super::PendingApproval {
                    decision: decision.clone(),
                },
            );
            projection
                .approval_by_decision
                .insert(decision.clone(), approval_id.clone());
            ops.push(HostEvent::ApprovalRequired {
                approval_id,
                call_id: event.call_id().map(|id| id.as_uuid().to_string()),
                tool_name: identity.canonical_name().as_str().to_owned(),
                subject: None,
                prompt: prompt.as_ref().map(|prompt| {
                    crate::session_log::redact_text_preview(prompt.description().as_str())
                }),
                expires_at_ms: super::events::timestamp_ms(*expires_at),
            });
        }
        AgentEventKind::EffectDecisionResolved {
            decision,
            resolution,
            ..
        } => {
            if let Some(approval_id) = projection.approval_by_decision.remove(decision) {
                projection.approvals.remove(&approval_id);
                ops.push(HostEvent::ApprovalResolved {
                    approval_id,
                    resolution: super::events::resolution_label(*resolution).to_owned(),
                });
            }
        }
        _ => {}
    }
    ops
}

/// A test seam: work a test runs while an agent view opens, right before
/// the view attaches to the live turn.
#[cfg(test)]
pub(super) mod hooks {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    type Hook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

    static BEFORE_ATTACH: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
    static BEFORE_FOLLOW: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);

    pub(in crate::runtime) fn set_before_follow(agent_id: &str, hook: Hook) {
        BEFORE_FOLLOW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(agent_id.to_owned(), hook);
    }

    pub(super) async fn before_follow(agent_id: &str) {
        let hook = BEFORE_FOLLOW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|hooks| hooks.get(agent_id).cloned());
        if let Some(hook) = hook {
            hook().await;
        }
    }

    pub(in crate::runtime) fn set_before_attach(agent_id: &str, hook: Hook) {
        BEFORE_ATTACH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(agent_id.to_owned(), hook);
    }

    pub(super) async fn before_attach(agent_id: &str) {
        let hook = BEFORE_ATTACH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|hooks| hooks.get(agent_id).cloned());
        if let Some(hook) = hook {
            hook().await;
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
