//! The single-session host service (wire v2, spec 02 §4.4): one session's
//! observations fanned out to its attachments by class, each attachment's
//! bounded outbound queue, the attach snapshot, and the slow-consumer drop.
//!
//! The service is the one consumer of the session's observations
//! ([`HostService::observe`]) and the one sender of its attachments'
//! intents ([`HostService::intent`]). Every observation takes its class
//! ([`SessionObservation::class`]):
//!
//! * a **state** is broadcast to the root's attachments and kept as the
//!   latest of its kind;
//! * an **event** is broadcast to the root's attachments; a required one is
//!   never dropped;
//! * an **addressed** observation goes to the attachment whose intent the
//!   session is answering: the one the last `IntentStarted` names, which
//!   gets back its own correlation in it. An answer to no attachment's
//!   intent goes to every root attachment.
//!
//! A transcript observation (snapshot, keyed patch, stream) goes, whatever
//! its class, to every attachment holding its scope: the root's to the
//! root's attachments, an agent's to those that opened it and to those
//! attached to that agent's node.
//!
//! An attachment to a child node (`child`) holds that node's transcript and
//! nothing else, and its intents are that node's controls.
//!
//! A root attachment starts with the attach snapshot: every state, the root
//! baseline, every approval still pending and the credential wait that
//! holds, each re-emitted as itself. Its queue holds at most its capacity:
//! a droppable event makes room by dropping the oldest droppable one, or is
//! itself dropped; a state replaces a queued state of its kind. When a state,
//! required event or addressed answer still finds no room, the attachment is
//! dropped: its queue is replaced by one resync-required and it takes
//! nothing more, so the session never blocks on a slow consumer. Transcript
//! content an attachment lost to a full queue is a gap in that scope, repaid
//! at its next take or observation: the root's from the kept baseline, any
//! other by asking the session for a fresh snapshot of that scope.
//!
//! An in-process face (the TUI, `plak agent` JSONL, an attached command) is
//! an attachment of its session's service too ([`HostService::attach_face`]):
//! its intents pass through the service as that attachment's
//! ([`FaceSender`]) and it takes its observations through [`FaceReceiver`].
//! A faces' service holds the session's mailbox without being one of its
//! senders ([`HostService::for_faces`]), so the session still ends when its
//! last face sender and attachment are gone.

mod child;
mod face;
mod tree_nodes;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use plakous_host_protocol::observation::{Delivery, ObservationClass};
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress, RequestRefusal};
use plakous_host_protocol::{
    IntentSendError, IntentSender, ObservationExtension, ProtocolExtension, SessionIntent,
    SessionObservation, WeakIntentSender,
};

pub use child::{ChildNodes, ControlDone, NodeState};
pub use face::{FaceReceiver, FaceSender, face};
pub use tree_nodes::TreeNodes;

/// The prefix marking an `IntentStarted` whose window holds only that
/// intent's answer ([`crate::runtime::Sender::answering`]).
pub(crate) const ANSWER: &str = "answer:";

/// The most root patches the service keeps after the root snapshot for an
/// attach; past it, a new attachment asks the session for a fresh one.
const MAX_BASELINE_PATCHES: usize = 64;

/// What an attachment takes from its queue.
#[allow(
    clippy::large_enum_variant,
    reason = "an event carries one observation by value"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outbound<P: ProtocolExtension> {
    /// One observation, with the attachment's sequence number (from one;
    /// a gap means a droppable event was dropped) and its class.
    Event {
        /// The attachment's sequence number.
        seq: u64,
        /// The observation's class.
        class: ObservationClass,
        /// The observation.
        observation: SessionObservation<P>,
    },
    /// The attachment fell behind on what it may not lose and is dropped;
    /// it takes nothing more and must attach again.
    ResyncRequired,
    /// The session was replaced by another (`/new`, a resume, a rebind)
    /// whose root is `node`: the attachment has ended after what it held,
    /// and its client attaches `node` fresh.
    Moved {
        /// The new session's root.
        node: NodeAddress,
    },
}

/// One attachment's state.
struct Attachment<P: ProtocolExtension> {
    /// The session's mailbox sender tagged with this attachment, or `None`
    /// for a face, which holds its own.
    sender: Option<IntentSender<P>>,
    /// The child node this attachment targets, or `None` for the root.
    node: Option<String>,
    queue: VecDeque<Outbound<P>>,
    seq: u64,
    /// The transcripts this attachment holds; a face of a service with no
    /// session mailbox holds every one, since its intents pass through no
    /// service.
    scopes: Scopes,
    /// Scopes whose transcript content this attachment lost to a full
    /// queue; repaid with a fresh baseline.
    gapped: BTreeSet<TranscriptScope>,
    /// Attach-snapshot items still at the head of the queue; they do not
    /// count against the capacity.
    snapshot_held: usize,
    dropped: bool,
}

/// The transcript scopes an attachment holds.
enum Scopes {
    Held(BTreeSet<TranscriptScope>),
    All,
}

impl Scopes {
    fn contains(&self, scope: &TranscriptScope) -> bool {
        match self {
            Self::Held(held) => held.contains(scope),
            Self::All => true,
        }
    }

    fn agents(&self) -> impl Iterator<Item = &String> {
        let held = match self {
            Self::Held(held) => Some(held),
            Self::All => None,
        };
        held.into_iter().flatten().filter_map(|scope| match scope {
            TranscriptScope::Agent(agent) => Some(agent),
            TranscriptScope::Root => None,
        })
    }
}

/// What the service keeps for a new attachment.
struct Snapshot<P: ProtocolExtension> {
    /// The latest state of each kind, in the order kinds first arrived.
    states: Vec<SessionObservation<P>>,
    /// The latest root snapshot and the root patches after it, or `None`
    /// when a new attachment must ask for a fresh one.
    baseline: Option<(SessionObservation<P>, Vec<SessionObservation<P>>)>,
    /// Approvals asked and not yet resolved, in the order asked.
    approvals: Vec<(String, SessionObservation<P>)>,
    /// The credential wait that holds, if any.
    credential_wait: Option<SessionObservation<P>>,
}

struct State<P: ProtocolExtension> {
    attachments: BTreeMap<AttachmentId, Attachment<P>>,
    next_attachment: AttachmentId,
    snapshot: Snapshot<P>,
    /// The attachment whose intent the session is answering.
    requester: Option<AttachmentId>,
    /// How many attachments hold each agent transcript: the session serves
    /// it from the first holder's open to the last holder's close.
    agent_holders: BTreeMap<String, usize>,
}

/// One session's host service; its clones are the same service.
pub struct HostService<P: ProtocolExtension> {
    /// The session's mailbox, or `None` when the service only serves a face.
    actions: Option<Mailbox<P>>,
    root: NodeAddress,
    capacity: usize,
    /// The session tree's children, or `None` when only the root attaches.
    nodes: Option<Arc<dyn ChildNodes>>,
    state: Arc<Mutex<State<P>>>,
}

impl<P: ProtocolExtension> Clone for HostService<P> {
    fn clone(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            root: self.root.clone(),
            capacity: self.capacity,
            nodes: self.nodes.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

/// How a service holds its session's mailbox.
enum Mailbox<P: ProtocolExtension> {
    /// As one of its senders: the session lives while the service does.
    Sender(IntentSender<P>),
    /// Beside its faces, as none of its senders.
    Beside(WeakIntentSender<P>),
}

impl<P: ProtocolExtension> Clone for Mailbox<P> {
    fn clone(&self) -> Self {
        match self {
            Self::Sender(sender) => Self::Sender(sender.clone()),
            Self::Beside(weak) => Self::Beside(weak.clone()),
        }
    }
}

/// Why the service did not pass on an attachment's intent, with the intent.
enum Unsent<P: ProtocolExtension> {
    /// Refused for the attachment's state or role.
    Refused(RequestRefusal, SessionIntent<P>),
    /// The session's mailbox refused it.
    Mailbox(IntentSendError<P>),
}

/// The origin tag an attachment's intents carry, so the session's
/// `IntentStarted` names the attachment its answers belong to.
fn origin(attachment: AttachmentId) -> String {
    format!("attachment:{attachment}")
}

/// The attachment a correlation names, and the client's own correlation
/// inside it.
fn attachment_of(correlation: &str) -> Option<(AttachmentId, Option<String>)> {
    let rest = correlation.strip_prefix("attachment:")?;
    let (id, client) = match rest.split_once(':') {
        Some((id, client)) => (id, Some(client.to_owned())),
        None => (rest, None),
    };
    Some((id.parse().ok()?, client))
}

/// Whether an attach re-emits `observation` from what the service keeps
/// (states, the root baseline, approvals, the credential wait), so a
/// replay must not send it again.
fn kept_for_attach<P: ProtocolExtension>(observation: &SessionObservation<P>) -> bool {
    observation.class() == ObservationClass::State
        || matches!(
            observation,
            SessionObservation::ApprovalRequired { .. }
                | SessionObservation::ApprovalResolved { .. }
                | SessionObservation::CredentialWait { .. }
                | SessionObservation::ClearView
                | SessionObservation::SessionOpening
                | SessionObservation::SessionOpened { .. }
        )
        || matches!(
            observation,
            SessionObservation::PresentationSnapshot(snapshot)
                if snapshot.scope == TranscriptScope::Root
        )
        || matches!(
            observation,
            SessionObservation::TranscriptPatch(patch) if patch.scope == TranscriptScope::Root
        )
}

/// Whether two observations are states of one kind.
fn same_kind<P: ProtocolExtension>(a: &SessionObservation<P>, b: &SessionObservation<P>) -> bool {
    match (a, b) {
        (SessionObservation::Extension(a), SessionObservation::Extension(b)) => a.same_kind(b),
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// The transcript scope a baseline observation belongs to, if it is one.
fn baseline_scope<P: ProtocolExtension>(
    observation: &SessionObservation<P>,
) -> Option<&TranscriptScope> {
    match observation {
        SessionObservation::PresentationSnapshot(snapshot) => Some(&snapshot.scope),
        SessionObservation::TranscriptPatch(patch) => Some(&patch.scope),
        SessionObservation::TranscriptStream { scope, .. } => Some(scope),
        _ => None,
    }
}

impl<P: ProtocolExtension> HostService<P> {
    /// The service of the session `actions` drives, whose root is `root`;
    /// each attachment holds at most `capacity` outbound items beyond its
    /// attach snapshot.
    #[must_use]
    pub fn new(actions: IntentSender<P>, root: NodeAddress, capacity: usize) -> Self {
        Self::serving(Some(Mailbox::Sender(actions)), root, capacity)
    }

    /// [`Self::new`] for a session its in-process faces hold
    /// ([`Self::attach_face`]): the service holds the mailbox without being
    /// one of its senders, so the session sees its last sender leave when
    /// its faces' senders and its attachments are gone.
    #[must_use]
    pub fn for_faces(actions: IntentSender<P>, root: NodeAddress, capacity: usize) -> Self {
        Self::serving(Some(Mailbox::Beside(actions.downgrade())), root, capacity)
    }

    fn serving(actions: Option<Mailbox<P>>, root: NodeAddress, capacity: usize) -> Self {
        Self {
            actions,
            root,
            capacity: capacity.max(1),
            nodes: None,
            state: Arc::new(Mutex::new(State {
                attachments: BTreeMap::new(),
                next_attachment: 1,
                snapshot: Snapshot {
                    states: Vec::new(),
                    baseline: None,
                    approvals: Vec::new(),
                    credential_wait: None,
                },
                requester: None,
                agent_holders: BTreeMap::new(),
            })),
        }
    }

    /// Moves this service to the root `root` of the session that replaced
    /// its own, with that tree's children reached through `nodes`; existing
    /// attachments are the caller's to end. The root and nodes are this
    /// handle's own: only the owner of a service no clone of which is
    /// shared (a wire connection's) reroots it.
    pub fn reroot(&mut self, root: NodeAddress, nodes: Option<Arc<dyn ChildNodes>>) {
        self.root = root;
        self.nodes = nodes;
        self.lock().requester = None;
    }

    /// The session root this handle attaches.
    #[must_use]
    pub const fn root(&self) -> &NodeAddress {
        &self.root
    }

    /// Ends every attachment with [`Outbound::Moved`] naming `node`, after
    /// what each already holds: the session was replaced by the one whose
    /// root that is. Each attachment's holder reroots its own handle.
    pub fn move_attachments(&self, node: &NodeAddress) {
        let mut state = self.lock();
        state.requester = None;
        for held in state.attachments.values_mut() {
            if !held.dropped {
                held.dropped = true;
                held.queue.push_back(Outbound::Moved { node: node.clone() });
            }
        }
    }

    /// This service, with the session tree's children attachable through
    /// `nodes`.
    #[must_use]
    pub fn with_nodes(mut self, nodes: Arc<dyn ChildNodes>) -> Self {
        self.nodes = Some(nodes);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<P>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A sender of the session's mailbox while the session has one.
    fn mailbox(&self) -> Option<IntentSender<P>> {
        match self.actions.as_ref()? {
            Mailbox::Sender(sender) => Some(sender.clone()),
            Mailbox::Beside(weak) => weak.upgrade(),
        }
    }

    /// Attaches to `node`, queueing the attach snapshot first. A child
    /// node's attachment holds that node's transcript only, and its attach
    /// snapshot is that transcript's baseline ([`ChildNodes`]).
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::UnknownNode`] for a node that is neither the
    /// session's root nor one of its tree's children;
    /// [`RequestRefusal::NodeClosed`] and [`RequestRefusal::NodeRetiring`]
    /// for a child that is closed or retiring.
    pub fn attach(&self, node: &NodeAddress) -> Result<AttachmentId, RequestRefusal> {
        if node.tree_id != self.root.tree_id {
            return Err(RequestRefusal::UnknownNode);
        }
        if node.agent_id != self.root.agent_id {
            return self.attach_child(&node.agent_id);
        }
        let Some(mailbox) = self.mailbox() else {
            return Err(RequestRefusal::Disconnected);
        };
        let (id, ask) = self.admit(Some(mailbox));
        if let Some(sender) = ask {
            // No baseline is kept: the session's fresh root snapshot reaches
            // every attachment holding the root, this one included. An
            // attach the session cannot be asked for one fails.
            if let Err(error) = sender.send(SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Root,
            }) {
                self.lock().attachments.remove(&id);
                return Err(refusal_of(&error));
            }
        }
        Ok(id)
    }

    /// Adds an attachment with the attach snapshot queued, and the sender
    /// to ask the session for a root baseline through when none is kept.
    /// With no `mailbox` it is a face's, which sends its own intents.
    fn admit(&self, mailbox: Option<IntentSender<P>>) -> (AttachmentId, Option<IntentSender<P>>) {
        let mut state = self.lock();
        let id = state.next_attachment;
        state.next_attachment += 1;
        let snapshot = &state.snapshot;
        let mut items: Vec<SessionObservation<P>> = snapshot.states.clone();
        let needs_baseline = snapshot.baseline.is_none();
        if let Some((baseline, patches)) = &snapshot.baseline {
            items.push(baseline.clone());
            items.extend(patches.iter().cloned());
        }
        items.extend(snapshot.approvals.iter().map(|(_, asked)| asked.clone()));
        items.extend(snapshot.credential_wait.iter().cloned());
        let sender = mailbox.map(|actions| actions.tagged(origin(id)));
        let scopes = match self.actions {
            Some(_) => Scopes::Held(BTreeSet::from([TranscriptScope::Root])),
            None => Scopes::All,
        };
        let ask = sender.clone().filter(|_| needs_baseline);
        let mut attachment = Attachment {
            sender,
            node: None,
            queue: VecDeque::new(),
            seq: 0,
            scopes,
            gapped: BTreeSet::new(),
            snapshot_held: items.len(),
            dropped: false,
        };
        for observation in items {
            attachment.seq += 1;
            attachment.queue.push_back(Outbound::Event {
                seq: attachment.seq,
                class: observation.class(),
                observation,
            });
        }
        state.attachments.insert(id, attachment);
        (id, ask)
    }

    /// Whether `attachment` is attached to a child node.
    #[must_use]
    pub fn is_child(&self, attachment: AttachmentId) -> bool {
        self.lock()
            .attachments
            .get(&attachment)
            .is_some_and(|held| held.node.is_some())
    }

    /// Whether `attachment` is attached and not dropped.
    #[must_use]
    pub fn attached(&self, attachment: AttachmentId) -> bool {
        self.lock()
            .attachments
            .get(&attachment)
            .is_some_and(|held| !held.dropped)
    }

    /// Detaches `attachment`; the session runs on.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::NotAttached`] when it is not attached.
    pub fn detach(&self, attachment: AttachmentId) -> Result<(), RequestRefusal> {
        // The senders this ends drop after the lock: a session's last
        // sender runs its shutdown hooks as it drops.
        let mailbox = self.mailbox();
        let removed = self.lock().remove(attachment, mailbox.as_ref());
        match removed {
            Some(held) if !held.dropped => Ok(()),
            _ => Err(RequestRefusal::NotAttached),
        }
    }

    /// Sends `intent` from `attachment` into the session's mailbox.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::NotAttached`] for an attachment not attached (or
    /// dropped), [`RequestRefusal::Disconnected`] for a face, which sends its
    /// own; the mailbox's own refusals otherwise. A refused intent did
    /// nothing.
    pub fn intent(
        &self,
        attachment: AttachmentId,
        intent: SessionIntent<P>,
    ) -> Result<(), RequestRefusal> {
        self.intent_correlated(attachment, intent, None)
    }

    /// [`Self::intent`] with the client's own correlation, which the
    /// attachment gets back in the `IntentStarted` that opens the answer.
    ///
    /// # Errors
    ///
    /// As [`Self::intent`].
    pub fn intent_correlated(
        &self,
        attachment: AttachmentId,
        intent: SessionIntent<P>,
        client: Option<&str>,
    ) -> Result<(), RequestRefusal> {
        let mut state = self.lock();
        let Some(held) = state
            .attachments
            .get(&attachment)
            .filter(|held| !held.dropped)
        else {
            return Err(RequestRefusal::NotAttached);
        };
        let Some(sender) = held.sender.clone() else {
            return Err(RequestRefusal::Disconnected);
        };
        if let Some(agent) = held.node.clone() {
            drop(state);
            return self.child_intent(attachment, &agent, &sender, intent, client);
        }
        state
            .root_intent(attachment, &sender, intent, client)
            .map_err(|unsent| match unsent {
                Unsent::Refused(refusal, _) => refusal,
                Unsent::Mailbox(error) => refusal_of(&error),
            })
    }

    /// `intent` from the face `attachment`, sent with the face's own
    /// `sender`. A face that already left sends it as is.
    pub(crate) fn face_intent(
        &self,
        attachment: AttachmentId,
        sender: &IntentSender<P>,
        intent: SessionIntent<P>,
        client: Option<String>,
    ) -> Result<(), IntentSendError<P>> {
        let mut state = self.lock();
        if !state.attachments.contains_key(&attachment) {
            drop(state);
            return sender.send_correlated(intent, client);
        }
        state
            .root_intent(attachment, sender, intent, client.as_deref())
            .map_err(|unsent| match unsent {
                Unsent::Refused(_, intent) => IntentSendError::Refused(intent),
                Unsent::Mailbox(error) => error,
            })
    }

    /// Takes everything queued for `attachment`. A dropped attachment
    /// yields its resync-required once and is then gone.
    pub fn take(&self, attachment: AttachmentId) -> Vec<Outbound<P>> {
        self.repair_gaps();
        // As in `detach`, the senders this ends drop after the lock.
        let mailbox = self.mailbox();
        let mut state = self.lock();
        let Some(held) = state.attachments.get_mut(&attachment) else {
            return Vec::new();
        };
        let taken: Vec<_> = held.queue.drain(..).collect();
        held.snapshot_held = 0;
        let removed = if held.dropped {
            state.remove(attachment, mailbox.as_ref())
        } else {
            None
        };
        drop(state);
        drop(removed);
        taken
    }

    /// Takes one observation from the session: keeps what an attach needs
    /// and queues it for the attachments its class names.
    pub fn observe(&self, observation: SessionObservation<P>) {
        {
            let mut state = self.lock();
            let class = observation.class();
            // Inside a child's answer window everything but a transcript is
            // that child's answer, and none of it is the root's to keep.
            let child_answer = state.requester.filter(|owner| {
                !matches!(observation, SessionObservation::IntentStarted(_))
                    && state
                        .attachments
                        .get(owner)
                        .is_some_and(|held| held.node.is_some())
            });
            // A face's service takes no later attachment to keep anything for.
            if self.actions.is_some() && child_answer.is_none() {
                state.remember(&observation, class);
            }
            let mut observation = observation;
            let targets: Vec<AttachmentId> = match (class, baseline_scope(&observation)) {
                (_, Some(scope)) => state
                    .attachments
                    .iter()
                    .filter(|(_, held)| held.scopes.contains(scope))
                    .map(|(id, _)| *id)
                    .collect(),
                _ if child_answer.is_some() => child_answer.into_iter().collect(),
                (ObservationClass::Addressed, None) => {
                    if let SessionObservation::IntentStarted(started) = &observation {
                        let (answer, named) = match started.as_deref() {
                            Some(started) => match started.strip_prefix(ANSWER) {
                                Some(named) => (true, Some(named.to_owned())),
                                None => (false, Some(started.to_owned())),
                            },
                            None => (false, None),
                        };
                        // A faces' mailbox may carry another sender's
                        // correlation that only looks like an attachment's:
                        // one naming no attachment the service holds is
                        // passed through as no attachment's.
                        let owner = named.as_deref().and_then(attachment_of).filter(|(id, _)| {
                            matches!(self.actions, Some(Mailbox::Sender(_)))
                                || state.attachments.contains_key(id)
                        });
                        if let Some((owner, client)) = owner {
                            // The attachment sees its own correlation.
                            observation = SessionObservation::IntentStarted(client);
                            let child = state
                                .attachments
                                .get(&owner)
                                .is_some_and(|held| held.node.is_some());
                            // A child attachment's window holds only its own
                            // control's answers, which come marked; the rest
                            // of an unmarked window is the session's.
                            state.requester = (answer || !child).then_some(owner);
                            vec![owner]
                        } else {
                            state.requester = None;
                            observation = SessionObservation::IntentStarted(named);
                            state.root_attachments()
                        }
                    } else {
                        // An answer to no attachment's intent belongs to
                        // none: every root attachment gets it.
                        match state.requester {
                            Some(owner) => vec![owner],
                            None => state.root_attachments(),
                        }
                    }
                }
                _ => state.root_attachments(),
            };
            if let Some((last, others)) = targets.split_last() {
                for id in others {
                    if let Some(held) = state.attachments.get_mut(id) {
                        let capacity = held.capacity(self.capacity);
                        held.offer(observation.clone(), class, capacity);
                    }
                }
                if let Some(held) = state.attachments.get_mut(last) {
                    let capacity = held.capacity(self.capacity);
                    held.offer(observation, class, capacity);
                }
            }
        }
        self.repair_gaps();
    }

    /// Repays each attachment's transcript gaps: the root's from the kept
    /// baseline, any scope without one by asking the session for a fresh
    /// snapshot of it, correlated to the attachment.
    fn repair_gaps(&self) {
        let mut asks = Vec::new();
        {
            let mut state = self.lock();
            let State {
                attachments,
                snapshot,
                ..
            } = &mut *state;
            for held in attachments.values_mut() {
                if held.dropped {
                    continue;
                }
                for scope in std::mem::take(&mut held.gapped) {
                    match (&scope, &snapshot.baseline) {
                        (TranscriptScope::Root, Some((baseline, patches))) => {
                            for item in std::iter::once(baseline).chain(patches) {
                                held.offer(item.clone(), item.class(), self.capacity);
                            }
                        }
                        _ => asks.extend(held.sender.clone().map(|sender| (sender, scope))),
                    }
                }
            }
        }
        for (sender, scope) in asks {
            let _ = sender.send(SessionIntent::ResyncTranscript { scope });
        }
    }
}

/// The request refusal a mailbox refusal is.
fn refusal_of<P: ProtocolExtension>(error: &IntentSendError<P>) -> RequestRefusal {
    match error {
        IntentSendError::Saturated(_) => RequestRefusal::Backpressure,
        IntentSendError::Duplicate(_) => RequestRefusal::OneInFlight,
        IntentSendError::Disconnected(_) => RequestRefusal::Disconnected,
        IntentSendError::Refused(_) => RequestRefusal::RefusedForRole {
            row: "transcript".to_owned(),
        },
    }
}

impl<P: ProtocolExtension> State<P> {
    /// `intent` from the root attachment `attachment`, sent with `sender`.
    /// The caller holds the service's lock throughout, so the session's
    /// answer finds the scopes it changes already changed.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the unsent intent back so a face can restore it"
    )]
    fn root_intent(
        &mut self,
        attachment: AttachmentId,
        sender: &IntentSender<P>,
        intent: SessionIntent<P>,
        client: Option<&str>,
    ) -> Result<(), Unsent<P>> {
        let held_agent = self
            .attachments
            .get(&attachment)
            .and_then(|held| held.scopes.agents().next().cloned());
        // The session serves one agent transcript: another is refused while
        // any other attachment holds one.
        if let SessionIntent::OpenAgentTranscript { agent_id } = &intent
            && self.holds_another(agent_id, held_agent.as_deref())
        {
            return Err(Unsent::Refused(
                RequestRefusal::RefusedForRole {
                    row: "transcript".to_owned(),
                },
                intent,
            ));
        }
        // An agent transcript is opened in the session by its first holder
        // and closed by its last; the others only hold or release it.
        let forwarded = match &intent {
            SessionIntent::OpenAgentTranscript { agent_id }
                if self.agent_holders.get(agent_id).copied().unwrap_or(0) > 0
                    && held_agent.as_deref() != Some(agent_id) =>
            {
                SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Agent(agent_id.clone()),
                }
            }
            SessionIntent::CloseAgentTranscript
                if held_agent.as_ref().is_none_or(|agent| {
                    self.agent_holders.get(agent).copied().unwrap_or(0) > 1
                }) =>
            {
                if let Some(agent) = held_agent {
                    self.release_agent(attachment, &agent);
                }
                return Ok(());
            }
            _ => intent.clone(),
        };
        if let Err(error) = sender.send_correlated(forwarded, client.map(str::to_owned)) {
            // The refusal hands back what the attachment sent.
            return Err(Unsent::Mailbox(match error {
                IntentSendError::Saturated(_) => IntentSendError::Saturated(intent),
                IntentSendError::Disconnected(_) => IntentSendError::Disconnected(intent),
                IntentSendError::Duplicate(_) => IntentSendError::Duplicate(intent),
                IntentSendError::Refused(_) => IntentSendError::Refused(intent),
            }));
        }
        // Scope changes take effect only once the session took the intent.
        match intent {
            SessionIntent::OpenAgentTranscript { agent_id } => {
                if let Some(agent) = held_agent.filter(|agent| *agent != agent_id) {
                    self.release_agent(attachment, &agent);
                }
                if let Some(held) = self.attachments.get_mut(&attachment)
                    && let Scopes::Held(scopes) = &mut held.scopes
                    && scopes.insert(TranscriptScope::Agent(agent_id.clone()))
                {
                    *self.agent_holders.entry(agent_id).or_default() += 1;
                }
            }
            SessionIntent::CloseAgentTranscript => {
                if let Some(agent) = held_agent {
                    self.release_agent(attachment, &agent);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Removes `attachment`, releasing its agent transcript: the last
    /// holder's release closes the session's agent view, asked as no
    /// attachment's intent since the attachment is gone.
    fn remove(
        &mut self,
        attachment: AttachmentId,
        actions: Option<&IntentSender<P>>,
    ) -> Option<Attachment<P>> {
        let held = self.attachments.get(&attachment)?;
        let agents: Vec<String> = held.scopes.agents().cloned().collect();
        for agent in &agents {
            self.release_agent(attachment, agent);
            if !self.agent_holders.contains_key(agent)
                && let Some(actions) = actions
            {
                let _ = actions.send(SessionIntent::CloseAgentTranscript);
            }
        }
        if self.requester == Some(attachment) {
            self.requester = None;
        }
        self.attachments.remove(&attachment)
    }

    /// Whether an attachment other than one holding `own` holds an agent
    /// transcript other than `agent`.
    fn holds_another(&self, agent: &str, own: Option<&str>) -> bool {
        self.agent_holders
            .iter()
            .any(|(held, count)| held != agent && *count > usize::from(own == Some(held.as_str())))
    }

    /// The attachments on the session's root: a child's attachment takes
    /// only its node's transcript and the answers to its own intents.
    fn root_attachments(&self) -> Vec<AttachmentId> {
        self.attachments
            .iter()
            .filter(|(_, held)| held.node.is_none())
            .map(|(id, _)| *id)
            .collect()
    }

    /// `attachment` stops holding `agent`'s transcript.
    fn release_agent(&mut self, attachment: AttachmentId, agent: &str) {
        if let Some(held) = self.attachments.get_mut(&attachment)
            && let Scopes::Held(scopes) = &mut held.scopes
            && scopes.remove(&TranscriptScope::Agent(agent.to_owned()))
            && let Some(count) = self.agent_holders.get_mut(agent)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.agent_holders.remove(agent);
            }
        }
    }

    /// Keeps what a later attach re-emits.
    fn remember(&mut self, observation: &SessionObservation<P>, class: ObservationClass) {
        let snapshot = &mut self.snapshot;
        if matches!(
            observation,
            SessionObservation::SessionOpening | SessionObservation::SessionOpened { .. }
        ) {
            // A session (re)opening starts what an attach re-emits afresh:
            // nothing the earlier session asked or held is still pending.
            snapshot.states.clear();
            snapshot.baseline = None;
            snapshot.approvals.clear();
            snapshot.credential_wait = None;
        }
        match observation {
            SessionObservation::ApprovalRequired { approval_id, .. } => {
                snapshot.approvals.retain(|(id, _)| id != approval_id);
                snapshot
                    .approvals
                    .push((approval_id.clone(), observation.clone()));
            }
            SessionObservation::ApprovalResolved { approval_id, .. } => {
                snapshot.approvals.retain(|(id, _)| id != approval_id);
            }
            SessionObservation::CredentialWait { account } => {
                snapshot.credential_wait = account.is_some().then(|| observation.clone());
            }
            SessionObservation::PresentationSnapshot(baseline)
                if baseline.scope == TranscriptScope::Root =>
            {
                snapshot.baseline = Some((observation.clone(), Vec::new()));
            }
            SessionObservation::TranscriptPatch(patch) if patch.scope == TranscriptScope::Root => {
                if let Some((_, patches)) = &mut snapshot.baseline {
                    if patches.len() < MAX_BASELINE_PATCHES {
                        patches.push(observation.clone());
                    } else {
                        snapshot.baseline = None;
                    }
                }
            }
            SessionObservation::ClearView => snapshot.baseline = None,
            _ if class == ObservationClass::State => {
                match snapshot
                    .states
                    .iter_mut()
                    .find(|held| same_kind(held, observation))
                {
                    Some(held) => *held = observation.clone(),
                    None => snapshot.states.push(observation.clone()),
                }
            }
            _ => {}
        }
    }
}

impl<P: ProtocolExtension> Attachment<P> {
    /// How many items this attachment's queue holds: a face's is unbounded,
    /// since the face reads it on its own thread and the session's mailbox
    /// is the bound it meets.
    const fn capacity(&self, service: usize) -> usize {
        if self.sender.is_some() {
            service
        } else {
            usize::MAX
        }
    }

    /// Records the transcript a dropped observation belonged to, so the
    /// attachment's view of it is repaid with a fresh baseline.
    fn note_loss(&mut self, lost: &SessionObservation<P>) {
        let scope = match lost {
            SessionObservation::TranscriptStream { scope, .. } => scope.clone(),
            SessionObservation::AssistantDelta(_)
            | SessionObservation::AssistantDone
            | SessionObservation::ReasoningDelta { .. }
            | SessionObservation::ReasoningSealed { .. }
            | SessionObservation::UserMessage { .. }
            | SessionObservation::Row(_) => TranscriptScope::Root,
            _ => return,
        };
        self.gapped.insert(scope);
    }

    /// Queues `observation`, making room by its class, or drops the
    /// attachment when what it may not lose finds none.
    fn offer(
        &mut self,
        observation: SessionObservation<P>,
        class: ObservationClass,
        capacity: usize,
    ) {
        if self.dropped {
            return;
        }
        let droppable = class == ObservationClass::Event(Delivery::Droppable);
        if class == ObservationClass::State
            && let Some(index) = self.queue.iter().position(|item| {
                matches!(item, Outbound::Event { class: ObservationClass::State, observation: held, .. }
                    if same_kind(held, &observation))
            })
        {
            // A queued state of its kind is superseded: the old one goes,
            // and the new one takes its turn after what came before it.
            self.queue.remove(index);
            if index < self.snapshot_held {
                self.snapshot_held -= 1;
            }
        } else if self.queue.len() - self.snapshot_held.min(self.queue.len()) >= capacity {
            let oldest_droppable = self.queue.iter().position(|item| {
                matches!(
                    item,
                    Outbound::Event {
                        class: ObservationClass::Event(Delivery::Droppable),
                        ..
                    }
                )
            });
            match (oldest_droppable, droppable) {
                (Some(index), _) => {
                    if let Some(Outbound::Event {
                        observation: lost, ..
                    }) = self.queue.remove(index)
                    {
                        self.note_loss(&lost);
                    }
                }
                (None, true) => {
                    // Dropped: the gap in the sequence tells the client.
                    self.note_loss(&observation);
                    self.seq += 1;
                    return;
                }
                (None, false) => {
                    self.dropped = true;
                    self.queue.clear();
                    self.queue.push_back(Outbound::ResyncRequired);
                    return;
                }
            }
        }
        self.seq += 1;
        self.queue.push_back(Outbound::Event {
            seq: self.seq,
            class,
            observation,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
