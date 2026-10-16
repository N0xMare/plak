//! Answers `RequestCallDetail`. Without an agent id the attached session
//! answers: its committed canonical history first, then its own retained
//! transcript tool row. With one, a child node of the attached agent tree
//! answers from its own session's canonical history only. A call in none of
//! these is refused; no other history is ever consulted.
//!
//! The target is resolved on the host loop, then the read runs as a spawned
//! task that posts the observation, so neither the idle loop nor a running
//! turn waits on a checkpoint load. Reads run one at a time. At most 32 wait
//! behind that one; a further request is answered `busy` at once. A second
//! request for the same session, call and agent, while one is in flight or
//! waiting, is not started and gets no answer. A waiting read whose session
//! is no longer attached is dropped without an answer. The surface asks again.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};

use plakous::model::CallId;
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, SessionCallDetailOutcome, SessionLocator,
};
use plakous_host_protocol::views::{
    CallDetail, CallDetailOutcome, CallDetailSource, parse_source_links,
};
use tokio::sync::Semaphore;

use crate::runtime::{HostEvent, Sender, TopologyRows};
use crate::sessions::{SessionRecord, ToolItem};

const NOT_IN_SESSION: &str = "this call is not part of the attached session";
const NOT_IN_CHILD: &str = "this call is not in that agent's session history";

/// Most bytes of a preview side the host returns. A retained row holds a
/// subject and an argument and result preview of about 2 KiB each.
const MAX_PREVIEW_SIDE_BYTES: usize = 8 * 1024;

/// How many reads may wait behind the one that holds the checkpoint load.
const MAX_WAITING_READS: usize = 32;

const BUSY: &str = "busy";

/// One request, resolved against the host's attached session and tree so the
/// read owns everything it needs.
pub(crate) struct CallDetailRequest {
    call_id: String,
    agent_id: Option<String>,
    /// The attached product session this read was resolved against.
    session_id: String,
    target: Target,
}

/// `None` when `requested_session` is not the session attached now: nothing
/// is read and no answer is sent. The surface asks again.
pub(crate) fn prepare(
    rows: &dyn TopologyRows,
    session: &SessionRecord,
    call_id: String,
    agent_id: Option<String>,
    requested_session: &str,
) -> Option<CallDetailRequest> {
    if requested_session != session.id {
        return None;
    }
    Some(CallDetailRequest::resolve(rows, session, call_id, agent_id))
}

enum Target {
    /// The attached session's canonical head, then its own retained row.
    Attached {
        locator: Option<SessionLocator>,
        preview: Option<Box<ToolItem>>,
    },
    /// One child node's own session, canonical history only.
    Child(SessionLocator),
    Refused(String),
}

impl CallDetailRequest {
    /// Resolves the history `agent_id` addresses: `None` is the attached
    /// session, otherwise a non-root node of the attached session's tree with
    /// a bound session.
    pub(crate) fn resolve(
        rows: &dyn TopologyRows,
        session: &SessionRecord,
        call_id: String,
        agent_id: Option<String>,
    ) -> Self {
        let target = match &agent_id {
            None => Target::Attached {
                locator: session.plakous_locator,
                preview: session.transcript.tool_activity(&call_id).map(Box::new),
            },
            Some(agent_id) => child_target(rows, session, agent_id),
        };
        Self {
            call_id,
            agent_id,
            session_id: session.id.clone(),
            target,
        }
    }

    /// Reads the detail. `sessions` is the process's runtime session handle,
    /// `None` when the host has none to read from.
    pub(crate) async fn read(self, sessions: Option<&AgentSessions>) -> CallDetail {
        let id = parse_call_id(&self.call_id);
        let outcome = match self.target {
            Target::Refused(reason) => CallDetailOutcome::Unavailable { reason },
            Target::Attached { locator, preview } => {
                let canonical = match (id, sessions, locator) {
                    (None, ..) => CanonicalRead::Absent,
                    (Some(id), Some(sessions), Some(locator)) => {
                        canonical_read(sessions, locator, id).await
                    }
                    (Some(_), ..) => {
                        CanonicalRead::Failed("canonical history is unavailable".to_owned())
                    }
                };
                resolve(canonical, preview.map(|row| *row))
            }
            Target::Child(locator) => {
                let canonical = match (id, sessions) {
                    (None, _) => CanonicalRead::Absent,
                    (Some(id), Some(sessions)) => canonical_read(sessions, locator, id).await,
                    (Some(_), None) => {
                        CanonicalRead::Failed("canonical history is unavailable".to_owned())
                    }
                };
                match canonical {
                    CanonicalRead::Absent => CallDetailOutcome::Unavailable {
                        reason: NOT_IN_CHILD.to_owned(),
                    },
                    other => resolve(other, None),
                }
            }
        };
        CallDetail {
            call_id: self.call_id,
            agent_id: self.agent_id,
            session_id: self.session_id,
            outcome,
        }
    }

    /// Runs the read as a task on the current runtime and publishes its one
    /// answer; returns at once. A duplicate or a full wait queue does not
    /// start a task (the full queue answers `busy` here, on the caller).
    pub(crate) fn spawn<P: plakous_host_protocol::ProtocolExtension>(
        self,
        sessions: Option<Arc<AgentSessions>>,
        events: Sender<P>,
    ) {
        self.spawn_on(sessions, events, process_gate());
    }

    fn spawn_on<P: plakous_host_protocol::ProtocolExtension>(
        self,
        sessions: Option<Arc<AgentSessions>>,
        events: Sender<P>,
        gate: Arc<ReadGate>,
    ) {
        let key = ReadKey {
            session_id: self.session_id.clone(),
            call_id: self.call_id.clone(),
            agent_id: self.agent_id.clone(),
        };
        match gate.admit(&key) {
            Admit::Duplicate => {}
            Admit::Busy => {
                let _ = events.send(HostEvent::CallDetail(CallDetail {
                    call_id: self.call_id,
                    agent_id: self.agent_id,
                    session_id: self.session_id,
                    outcome: CallDetailOutcome::Unavailable {
                        reason: BUSY.to_owned(),
                    },
                }));
            }
            Admit::Ready(generation) => {
                tokio::spawn(async move {
                    let _guard = Inflight {
                        gate: Arc::clone(&gate),
                        key: key.clone(),
                    };
                    // The semaphore is never closed, so a permit always comes.
                    let _permit = gate.reads.acquire().await.ok();
                    if !gate.still(generation, &key.session_id) {
                        return;
                    }
                    let answer = self.read(sessions.as_deref()).await;
                    let _ = events.send(HostEvent::CallDetail(answer));
                });
            }
        }
    }
}

/// The refusal a host without a readable session sends.
pub(crate) fn unavailable(
    call_id: String,
    agent_id: Option<String>,
    session_id: String,
    reason: String,
) -> CallDetail {
    CallDetail {
        call_id,
        agent_id,
        session_id,
        outcome: CallDetailOutcome::Unavailable { reason },
    }
}

fn child_target(rows: &dyn TopologyRows, session: &SessionRecord, agent_id: &str) -> Target {
    let tree_id = crate::runtime::durable_topology_tree_id(rows, session);
    let Some(runtime) = crate::topology_runtime::lookup(&tree_id) else {
        return Target::Refused("no agent tree is attached to this session".to_owned());
    };
    runtime
        .read(|tree| child_locator(tree, agent_id))
        .unwrap_or_else(|_| Err("the agent tree is unavailable".to_owned()))
        .map_or_else(Target::Refused, Target::Child)
}

/// The exact session a non-root node of `tree` is bound to. The root is the
/// attached session (or a predecessor in its lineage), so it is never
/// addressed by agent id.
pub(crate) fn child_locator(
    tree: &crate::agent_tree::ProductAgentTree,
    agent_id: &str,
) -> Result<SessionLocator, String> {
    let node = tree
        .node(agent_id)
        .map_err(|_| "no such agent in the attached tree".to_owned())?;
    if node.parent_agent_id.is_none() {
        return Err("the tree root is the attached session; omit the agent id".to_owned());
    }
    let locator = node
        .session_locator
        .as_deref()
        .ok_or_else(|| "this agent has no bound session".to_owned())?;
    SessionLocator::from_str(locator)
        .map_err(|_| "this agent's session locator is not three valid identities".to_owned())
}

/// What the addressed session's canonical history held for one call.
#[derive(PartialEq, Eq)]
enum CanonicalRead {
    Found {
        input: String,
        output: String,
        input_truncated: bool,
        output_truncated: bool,
    },
    Absent,
    Failed(String),
}

impl std::fmt::Debug for CanonicalRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found {
                input,
                output,
                input_truncated,
                output_truncated,
            } => formatter
                .debug_struct("Found")
                .field("input_bytes", &input.len())
                .field("output_bytes", &output.len())
                .field("input_truncated", input_truncated)
                .field("output_truncated", output_truncated)
                .finish(),
            Self::Absent => formatter.write_str("Absent"),
            Self::Failed(reason) => formatter.debug_tuple("Failed").field(reason).finish(),
        }
    }
}

fn parse_call_id(call_id: &str) -> Option<CallId> {
    uuid::Uuid::parse_str(call_id)
        .ok()
        .and_then(|id| CallId::try_from(id).ok())
}

/// `locator` names the head the host last saw; a head that moved since (a
/// turn committed meanwhile) is followed once on the same session and branch,
/// never to another session or branch.
async fn canonical_read(
    sessions: &AgentSessions,
    mut locator: SessionLocator,
    call_id: CallId,
) -> CanonicalRead {
    for _ in 0..2 {
        match sessions.call_detail(locator, call_id).await {
            Ok(SessionCallDetailOutcome::Found(detail)) => {
                return CanonicalRead::Found {
                    input: detail.input().text().to_owned(),
                    output: detail.output().text().to_owned(),
                    input_truncated: detail.input().truncated(),
                    output_truncated: detail.output().truncated(),
                };
            }
            Ok(SessionCallDetailOutcome::NotInCanonicalHistory) => return CanonicalRead::Absent,
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                match error.current_locator() {
                    Some(current)
                        if current.session_id() == locator.session_id()
                            && current.branch_id() == locator.branch_id() =>
                    {
                        locator = current;
                    }
                    _ => return CanonicalRead::Failed(format!("call detail unavailable: {error}")),
                }
            }
            Err(error) => {
                return CanonicalRead::Failed(format!("call detail unavailable: {error}"));
            }
        }
    }
    CanonicalRead::Failed("call detail unavailable: the session head kept moving".to_owned())
}

fn resolve(canonical: CanonicalRead, preview: Option<ToolItem>) -> CallDetailOutcome {
    match (canonical, preview) {
        (
            CanonicalRead::Found {
                input,
                output,
                input_truncated,
                output_truncated,
            },
            _,
        ) => CallDetailOutcome::Detail {
            sources: parse_source_links(&output),
            input,
            output,
            input_truncated,
            output_truncated,
            source: CallDetailSource::Canonical,
        },
        (_, Some(preview)) => preview_detail(preview),
        (CanonicalRead::Absent, None) => CallDetailOutcome::Unavailable {
            reason: NOT_IN_SESSION.to_owned(),
        },
        (CanonicalRead::Failed(reason), None) => CallDetailOutcome::Unavailable { reason },
    }
}

/// A running row's retained detail is its argument preview; a finished row's
/// is its result preview, with the subject standing in for the input. A
/// restored await row's subject is the observation's own field, not a fresh
/// read of the result preview. The row was redacted when it was written; it
/// is redacted and bounded again here because a carried row arrived from an
/// export document.
fn preview_detail(preview: ToolItem) -> CallDetailOutcome {
    let running = matches!(
        preview.activity_state(),
        plakous_host_protocol::views::presentation::ActivityState::Running
    );
    let subject = preview.subject.unwrap_or(preview.name);
    let detail = preview.detail.unwrap_or_default();
    let (input, output) = if running {
        (
            if detail.is_empty() { subject } else { detail },
            String::new(),
        )
    } else {
        (subject, detail)
    };
    let output = bounded_preview(&output);
    CallDetailOutcome::Detail {
        input: bounded_preview(&input),
        sources: parse_source_links(&output),
        output,
        input_truncated: true,
        output_truncated: true,
        source: CallDetailSource::Preview,
    }
}

fn bounded_preview(text: &str) -> String {
    let mut redacted = plakous::tools::preview_redaction::text(text);
    if redacted.len() > MAX_PREVIEW_SIDE_BYTES {
        let mut end = MAX_PREVIEW_SIDE_BYTES;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        redacted.truncate(end);
    }
    redacted
}

#[allow(
    clippy::struct_field_names,
    reason = "session, call and agent are three different ids, and that is the whole key"
)]
#[derive(Clone, Eq, PartialEq, Hash)]
struct ReadKey {
    session_id: String,
    call_id: String,
    agent_id: Option<String>,
}

enum Admit {
    Ready(u64),
    Duplicate,
    Busy,
}

struct GateInner {
    /// The session the host last noted as attached. Empty until the first note.
    current: String,
    /// Bumped when a session stops being the attached one, so a read admitted
    /// under the old generation is dropped when it reaches the semaphore.
    generations: HashMap<String, u64>,
    inflight: HashSet<ReadKey>,
    /// Reads admitted behind the one allowed to hold the checkpoint load.
    waiting: usize,
    running: bool,
}

struct ReadGate {
    inner: Mutex<GateInner>,
    reads: Semaphore,
}

fn process_gate() -> Arc<ReadGate> {
    static GATE: OnceLock<Arc<ReadGate>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(ReadGate::new))
}

/// Records the session the host loop has attached, so a read queued for a
/// session that has since been left is dropped when it reaches the loader.
pub(crate) fn note_attached(session_id: &str) {
    process_gate().note(session_id);
}

impl ReadGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GateInner {
                current: String::new(),
                generations: HashMap::new(),
                inflight: HashSet::new(),
                waiting: 0,
                running: false,
            }),
            reads: Semaphore::new(1),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn note(&self, session_id: &str) {
        let mut inner = self.lock();
        if inner.current == session_id {
            return;
        }
        if !inner.current.is_empty() {
            let previous = inner.current.clone();
            *inner.generations.entry(previous).or_insert(0) += 1;
        }
        inner.current = session_id.to_owned();
    }

    fn admit(&self, key: &ReadKey) -> Admit {
        let mut inner = self.lock();
        if inner.inflight.contains(key) {
            return Admit::Duplicate;
        }
        if inner.running && inner.waiting >= MAX_WAITING_READS {
            return Admit::Busy;
        }
        inner.inflight.insert(key.clone());
        if inner.running {
            inner.waiting += 1;
        } else {
            inner.running = true;
        }
        Admit::Ready(inner.generations.get(&key.session_id).copied().unwrap_or(0))
    }

    fn still(&self, generation: u64, session_id: &str) -> bool {
        self.lock()
            .generations
            .get(session_id)
            .copied()
            .unwrap_or(0)
            == generation
    }

    fn finish(&self, key: &ReadKey) {
        let mut inner = self.lock();
        inner.inflight.remove(key);
        if inner.waiting > 0 {
            inner.waiting -= 1;
        } else {
            inner.running = false;
        }
    }

    #[cfg(test)]
    fn inflight_len(&self) -> usize {
        self.lock().inflight.len()
    }
}

/// Removes the key when the task ends, including the drop path that sends nothing.
struct Inflight {
    gate: Arc<ReadGate>,
    key: ReadKey,
}

impl Drop for Inflight {
    fn drop(&mut self) {
        self.gate.finish(&self.key);
    }
}

#[cfg(test)]
mod tests;
