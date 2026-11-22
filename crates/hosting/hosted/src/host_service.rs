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
//! An in-process face (the client, `plakd agent` JSONL, an attached command) is
//! an attachment of its session's service too ([`HostService::attach_face`]):
//! its intents pass through the service as that attachment's
//! ([`FaceSender`]) and it takes its observations through [`FaceReceiver`].
//! A faces' service holds the session's mailbox without being one of its
//! senders ([`HostService::for_faces`]), so the session still ends when its
//! last face sender and attachment are gone.

mod attachment;
mod attention;
mod child;
pub(crate) mod drafts;
mod face;
mod fanout;
mod gap_repair;
mod handle;
mod queue;
mod retained;
#[cfg(feature = "test-support")]
pub mod test_draft_gate;
#[cfg(feature = "test-support")]
pub mod test_open_observations;
#[cfg(not(feature = "test-support"))]
mod test_open_observations {
    pub(super) fn before_pump<P: super::ProtocolExtension>(
        _session: &str,
        _service: &super::HostService<P>,
    ) -> impl FnOnce() -> bool + Send + 'static {
        || true
    }
}
#[cfg(not(feature = "test-support"))]
mod test_draft_gate {
    use super::{
        BTreeMap, HostService, IntentSender, ProtocolExtension, RequestRefusal, SessionIntent,
        drafts,
    };
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the disabled draft gate keeps the enabled gate's fallible deferral signature"
    )]
    pub(super) fn defer<P: ProtocolExtension>(
        _service: &HostService<P>,
        _sender: &IntentSender<P>,
        _intent: &SessionIntent<P>,
        _client: Option<&str>,
        _pending: &BTreeMap<plakous_host_protocol::wire::ClientDraftId, drafts::PendingDraft>,
    ) -> Result<bool, RequestRefusal> {
        Ok(false)
    }
}
mod tree_nodes;
pub mod wire;

use self::wire::Face;
use plakous_host_protocol::observation::ObservationClass;
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress, RequestRefusal};
use plakous_host_protocol::{
    IntentSendError, IntentSender, ObservationExtension, ProtocolExtension, SessionIntent,
    SessionObservation, WeakIntentSender,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use attachment::{attachment_of, kept_for_attach, origin};
pub use attention::KeptActivity;
pub use child::{ChildNodes, ControlDone, NodeState};
pub use face::{FacePublisher, FaceReceiver, FaceSender, face};
use queue::Queue;
use retained::{Payload, RetainedBaseline};
pub use tree_nodes::TreeNodes;

/// The prefix marking an `IntentStarted` whose window holds only that
/// intent's answer ([`crate::runtime::Sender::answering`]).
pub(crate) const ANSWER: &str = "answer:";
/// Transport-only provenance, before any untrusted client correlation.
pub(crate) const REMOTE_ATTACHMENT: &str = "remote-attachment:";
pub(crate) const LOCAL_ATTACHMENT: &str = "local-attachment:";

/// What an attachment takes from its queue.
pub type Outbound<P> = QueueItem<SessionObservation<P>>;
type Pending<P> = QueueItem<Payload<P>>;

/// One sequenced queue entry or an attachment boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueItem<T> {
    /// One observation, with the attachment's sequence number (from one;
    /// a gap means a droppable event was dropped) and its class.
    Event {
        /// Process-local wire origin, never part of the shared observation.
        client_id: Option<plakous_host_protocol::wire::ClientDraftId>,
        /// The attachment's sequence number.
        seq: u64,
        /// The observation's class.
        class: ObservationClass,
        /// The observation.
        observation: T,
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
        /// The session request that caused the switch: the attachment
        /// that asked and its client's id for it.
        request: Option<(AttachmentId, String)>,
    },
}

impl<P: ProtocolExtension> Pending<P> {
    fn paced_alone(&self) -> bool {
        matches!(
            self,
            Self::Event {
                observation: Payload::Snapshot(_) | Payload::Patch(_),
                ..
            }
        )
    }

    fn into_outbound(self) -> Outbound<P> {
        match self {
            Self::Event {
                client_id,
                seq,
                class,
                observation,
            } => Outbound::Event {
                client_id,
                seq,
                class,
                observation: observation.into_observation(),
            },
            Self::Moved { node, request } => Outbound::Moved { node, request },
            Self::ResyncRequired => Outbound::ResyncRequired,
        }
    }
}

/// One attachment's state.
struct Attachment<P: ProtocolExtension> {
    /// Its connection's face: a remote one takes no local-only observation.
    face: Face,
    /// Stable only across this connection's admitted `moved` handoffs.
    reply_origin: AttachmentId,
    /// The session's mailbox sender tagged with this attachment, or `None`
    /// for a face, which holds its own.
    sender: Option<IntentSender<P>>,
    /// The child node this attachment targets, or `None` for the root.
    node: Option<String>,
    queue: Queue<P>,
    seq: u64,
    /// The transcripts this attachment holds; a face of a service with no
    /// session mailbox holds every one, since its intents pass through no
    /// service.
    scopes: Scopes,
    /// Scopes whose transcript content this attachment lost to a full
    /// queue; repaid with a fresh baseline.
    gapped: BTreeSet<TranscriptScope>,
    fresh: BTreeSet<TranscriptScope>,
    repairing: BTreeMap<TranscriptScope, std::time::Instant>,
    retained_after: Option<std::time::Instant>,
    keyed: BTreeSet<TranscriptScope>,
    dropped: bool,
    /// Admitted while the service held what follows a session opening it
    /// could not resolve: it takes nothing until the opening's binding ends
    /// it with `moved` or `resync_required`.
    waiting: bool,
}

#[derive(Clone, Copy)]
struct MovedRequester {
    origin: Option<AttachmentId>,
    face: Face,
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
    states: Vec<(SessionObservation<P>, usize)>,
    /// The latest root snapshot and the root patches after it, or `None`
    /// when a new attachment must ask for a fresh one.
    baseline: Option<RetainedBaseline>,
    /// Approvals asked and not yet resolved, in the order asked.
    approvals: Vec<(String, SessionObservation<P>, usize)>,
    /// The credential wait that holds, if any.
    credential_wait: Option<(SessionObservation<P>, usize)>,
}

struct State<P: ProtocolExtension> {
    pending_drafts: BTreeMap<plakous_host_protocol::wire::ClientDraftId, drafts::PendingDraft>,
    draft_inputs: BTreeMap<
        plakous_host_protocol::wire::ClientDraftId,
        (AttachmentId, fanout::Publication<P>),
    >,
    draft_delivery: Option<drafts::DraftDelivery>,
    draft_outputs: BTreeMap<(AttachmentId, u64), drafts::DraftDelivery>,
    draft_window: Option<plakous_host_protocol::wire::ClientDraftId>,
    draft_answer: bool,
    attachments: BTreeMap<AttachmentId, Attachment<P>>,
    next_attachment: AttachmentId,
    snapshot: Snapshot<P>,
    root_revision: Option<u64>,
    resync_wait: BTreeMap<TranscriptScope, std::time::Instant>,
    directory_children: Option<(String, (usize, usize))>,
    /// The attachment whose intent the session is answering.
    requester: Option<AttachmentId>,
    /// How many attachments hold each agent transcript: the session serves
    /// it from the first holder's open to the last holder's close.
    agent_holders: BTreeMap<String, usize>,
    /// Whether a new attachment waits ([`Attachment::waiting`]).
    holding: bool,
    /// The session's residency count, which counts the map's attachments.
    count: AttachmentCount,
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

/// What keeps a session resident from its host's side, readable by the
/// session itself (its residency, spec 02 §4.4): its host service's
/// attachments and the holds of connections its host handed it to, and
/// whether the session has begun to release. Every count taken also bumps a
/// generation, and so does withdrawing a release-now request, so the word
/// the session's actor read before it decided changes with any attach, hold
/// or withdrawal since, even one already gone again. The release begins
/// only from exactly that word, with nothing counted, in one
/// atomic step, and nothing is counted after it: an attach or a hold either
/// comes first and keeps the session, or is refused and finds it released.
#[derive(Clone, Debug, Default)]
pub struct AttachmentCount(Arc<CountWord>);

/// The count's word: the attachments and holds in the low 32 bits, the
/// releasing bit above them, and the generation in the bits above that,
/// whose carry leaves the word.
#[derive(Debug, Default)]
struct CountWord {
    bits: AtomicU64,
    changed: tokio::sync::Notify,
    /// How many host requests ask the session to release as soon as
    /// nothing keeps it, without waiting out the idle release
    /// (`release_session`).
    release_now: std::sync::atomic::AtomicU32,
    attention: Mutex<Option<crate::runtime::operator_attention::SessionAttention>>,
    directory: std::sync::OnceLock<tokio::sync::watch::Sender<u64>>,
    #[cfg(test)]
    seam: Mutex<MarkSeam>,
}

const COUNTED: u64 = (1 << 32) - 1;
/// The bit of the word that says the session is releasing.
const RELEASING: u64 = 1 << 32;
/// One generation of the word, and no count. Its carry leaves the word.
const GENERATION: u64 = 1 << 33;
/// One count taken: one more counted, and the next generation.
const TAKEN: u64 = GENERATION + 1;

/// The count's word as the session's actor read it before deciding to
/// release ([`AttachmentCount::seen`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CountSeen(u64);

impl CountSeen {
    /// The attachments and holds counted when it was read.
    pub(crate) const fn count(self) -> u64 {
        self.0 & COUNTED
    }
}

impl AttachmentCount {
    pub(crate) fn publish_attention(
        &self,
        attention: Option<crate::runtime::operator_attention::SessionAttention>,
    ) {
        let mut current = self
            .0
            .attention
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *current != attention {
            *current = attention;
            drop(current);
            self.directory_changed();
        }
    }

    pub(crate) fn watch_directory(&self, notify: tokio::sync::watch::Sender<u64>) {
        let _ = self.0.directory.set(notify);
    }

    pub(crate) fn directory_changed(&self) {
        if let Some(notify) = self.0.directory.get() {
            notify.send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    fn attention(&self) -> Option<crate::runtime::operator_attention::SessionAttention> {
        *self
            .0
            .attention
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    /// The attachments and holds counted now.
    #[must_use]
    pub fn get(&self) -> usize {
        usize::try_from(self.load() & COUNTED).unwrap_or(usize::MAX)
    }

    /// Whether the session has begun to release.
    #[must_use]
    pub fn releasing(&self) -> bool {
        self.load() & RELEASING != 0
    }

    /// The word now, for a later [`Self::begin_release`].
    pub(crate) fn seen(&self) -> CountSeen {
        CountSeen(self.load())
    }

    /// Marks the session releasing if the word is still exactly `seen` and
    /// `seen` counted nothing; `false` when an attachment or a hold was
    /// counted then, or has been since.
    pub(crate) fn begin_release(&self, seen: CountSeen) -> bool {
        #[cfg(test)]
        let (before, after) = {
            let mut seam = self.seam();
            (seam.before.take(), seam.after.take())
        };
        #[cfg(test)]
        if let Some(before) = before {
            before();
        }
        let marked = seen.count() == 0
            && seen.0 & RELEASING == 0
            && self
                .0
                .bits
                .compare_exchange(
                    seen.0,
                    seen.0 | RELEASING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
        #[cfg(test)]
        if let Some(after) = after {
            after(marked);
        }
        if marked {
            self.directory_changed();
        }
        marked
    }

    /// Asks the session to release as soon as nothing keeps it resident,
    /// without waiting out the idle release; an attach or a hold still
    /// keeps it.
    pub fn request_release_now(&self) {
        self.0.release_now.fetch_add(1, Ordering::AcqRel);
        self.0.changed.notify_one();
    }

    /// Withdraws one [`Self::request_release_now`]; another request still
    /// in flight keeps asking. A withdrawal also advances the word's
    /// generation, so a release decision that already read the word fails
    /// its compare-exchange and does not begin.
    pub fn cancel_release_now(&self) {
        let withdrew = self
            .0
            .release_now
            .try_update(Ordering::AcqRel, Ordering::Acquire, |asked| {
                asked.checked_sub(1)
            })
            .is_ok();
        if withdrew {
            self.0.bits.fetch_add(GENERATION, Ordering::AcqRel);
            self.0.changed.notify_one();
        }
    }

    /// Whether a host asked the session to release now.
    pub(crate) fn release_now(&self) -> bool {
        self.0.release_now.load(Ordering::Acquire) > 0
    }

    pub(crate) fn changed(&self) -> &tokio::sync::Notify {
        &self.0.changed
    }

    /// Holds the session resident until the hold is dropped; `None` once it
    /// is releasing.
    #[must_use]
    pub fn hold(&self) -> Option<ResidencyHold> {
        self.try_add().then(|| ResidencyHold(self.clone()))
    }

    fn load(&self) -> u64 {
        self.0.bits.load(Ordering::Acquire)
    }

    /// Counts one more unless the session is releasing.
    fn try_add(&self) -> bool {
        let added = self
            .0
            .bits
            .try_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (word & RELEASING == 0).then_some(word.wrapping_add(TAKEN))
            })
            .is_ok();
        if added {
            self.0.changed.notify_one();
        }
        added
    }

    /// Counts one more, releasing or not: an in-process face's attachment,
    /// whose session is never released for being idle.
    fn add(&self) {
        self.0.bits.fetch_add(TAKEN, Ordering::AcqRel);
        self.0.changed.notify_one();
    }

    fn remove(&self) {
        self.0.bits.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_one();
    }
}

/// What a test runs inside [`AttachmentCount::begin_release`]: `before`
/// between the read and the swap, `after` once it marked or not.
#[cfg(test)]
#[derive(Default)]
struct MarkSeam {
    before: Option<Box<dyn FnOnce() + Send>>,
    after: Option<Box<dyn FnOnce(bool) + Send>>,
}

#[cfg(test)]
impl std::fmt::Debug for MarkSeam {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MarkSeam")
    }
}

#[cfg(test)]
impl AttachmentCount {
    /// Runs `before` in the next release decision, after the actor read the
    /// word and before it tries the mark, and `after` with whether it marked.
    pub(crate) fn around_next_mark(
        &self,
        before: impl FnOnce() + Send + 'static,
        after: impl FnOnce(bool) + Send + 'static,
    ) {
        let mut seam = self.seam();
        seam.before = Some(Box::new(before));
        seam.after = Some(Box::new(after));
    }

    fn seam(&self) -> std::sync::MutexGuard<'_, MarkSeam> {
        self.0.seam.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection's hold on its session's residency
/// ([`AttachmentCount::hold`]), from the moment its host hands it the
/// session until it ends.
#[derive(Debug)]
pub struct ResidencyHold(AttachmentCount);

impl Drop for ResidencyHold {
    fn drop(&mut self) {
        self.0.remove();
    }
}

impl<P: ProtocolExtension> HostService<P> {
    /// This service, counting its attachments into `count`. Bound before
    /// anything attaches.
    #[must_use]
    pub fn with_attachment_count(self, count: &AttachmentCount) -> Self {
        self.lock().count = count.clone();
        self
    }

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
                pending_drafts: BTreeMap::new(),
                draft_inputs: BTreeMap::new(),
                draft_delivery: None,
                draft_outputs: BTreeMap::new(),
                draft_window: None,
                draft_answer: false,
                attachments: BTreeMap::new(),
                next_attachment: 1,
                snapshot: Snapshot {
                    states: Vec::new(),
                    baseline: None,
                    approvals: Vec::new(),
                    credential_wait: None,
                },
                root_revision: None,
                resync_wait: BTreeMap::new(),
                directory_children: None,
                requester: None,
                agent_holders: BTreeMap::new(),
                holding: false,
                count: AttachmentCount::default(),
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
        let mut state = self.lock();
        state.requester = None;
        state.root_revision = None;
        state.resync_wait.clear();
    }

    /// The session root this handle attaches.
    #[must_use]
    pub const fn root(&self) -> &NodeAddress {
        &self.root
    }

    fn reply_recipient(&self, requester: AttachmentId) -> Option<AttachmentId> {
        self.lock().reply_recipient(requester)
    }

    /// Only a connection following its own `moved` may continue its requester.
    fn continue_requester(&self, attachment: AttachmentId, requester: MovedRequester) -> bool {
        let Some(origin_id) = requester.origin else {
            return true;
        };
        let mut state = self.lock();
        if state
            .reply_recipient(origin_id)
            .is_some_and(|id| id != attachment)
        {
            return false;
        }
        let Some(held) = state
            .attachments
            .get_mut(&attachment)
            .filter(|held| !held.dropped && held.node.is_none() && held.face == requester.face)
        else {
            return false;
        };
        held.reply_origin = origin_id;
        held.sender = held
            .sender
            .take()
            .map(|sender| sender.tagged(origin(origin_id, requester.face)));
        if state.requester == Some(origin_id) {
            state.requester = Some(attachment);
        }
        true
    }

    /// Ends every attachment with [`Outbound::Moved`] naming `node`, after
    /// what each already holds: the session was replaced by the one whose
    /// root that is. Each attachment's holder reroots its own handle.
    ///
    /// `request` names the session request that caused the switch, if one
    /// did: the attachment that asked and its client's id for it. Returns
    /// the attachments it ended.
    #[allow(
        clippy::must_use_candidate,
        reason = "moving attachments is the effect; callers may ignore the ended-attachment summary"
    )]
    pub fn move_attachments(
        &self,
        node: &NodeAddress,
        request: Option<(AttachmentId, &str)>,
    ) -> Vec<AttachmentId> {
        self.move_attachment_requesters(node, request)
            .into_iter()
            .map(|(attachment, _)| attachment)
            .collect()
    }

    fn move_attachment_requesters(
        &self,
        node: &NodeAddress,
        request: Option<(AttachmentId, &str)>,
    ) -> Vec<(AttachmentId, MovedRequester)> {
        let mut state = self.lock();
        state.requester = None;
        let mut moved = Vec::new();
        for (id, held) in &mut state.attachments {
            if !held.dropped {
                held.dropped = true;
                held.queue.push_boundary(Pending::Moved {
                    node: node.clone(),
                    request: request.map(|(requester, id)| (requester, id.to_owned())),
                });
                moved.push((
                    *id,
                    MovedRequester {
                        origin: held.node.is_none().then_some(held.reply_origin),
                        face: held.face,
                    },
                ));
            }
        }
        moved
    }

    /// While `holding`, a new attachment takes nothing, not even its attach
    /// snapshot, until it is ended ([`Attachment::waiting`]); the kept root
    /// baseline is dropped as the hold ends, since what the hold kept from
    /// every attachment reached it, and the next attach asks the session
    /// for a fresh one.
    fn hold_attachments(&self, holding: bool) {
        let mut state = self.lock();
        if state.holding && !holding {
            state.snapshot.baseline = None;
        }
        state.holding = holding;
    }

    /// Ends every attachment holding the transcript of each scope in
    /// `lost` with [`Outbound::ResyncRequired`], after what it already
    /// holds: the session's mailbox dropped content of that transcript, so
    /// what the attachment shows has a gap only a fresh baseline repays. A
    /// root baseline kept for attaches has the same gap and is dropped, so
    /// the next attach asks the session for a fresh one. `lost` is asked
    /// of every scope an attachment holds.
    fn resync_lost_transcripts(&self, mut lost: impl FnMut(&TranscriptScope) -> bool) {
        let mut state = self.lock();
        if lost(&TranscriptScope::Root) {
            state.snapshot.baseline = None;
            state.root_revision = None;
        }
        for held in state.attachments.values_mut() {
            let gapped = match &held.scopes {
                Scopes::Held(scopes) => scopes.iter().any(&mut lost),
                Scopes::All => false,
            };
            if gapped && !held.dropped {
                held.dropped = true;
                held.queue.push_boundary(Pending::ResyncRequired);
            }
        }
    }

    /// Ends every attachment with [`Outbound::ResyncRequired`], after what
    /// each already holds: what follows belongs to no session yet that it
    /// may be shown.
    fn resync_attachments(&self) {
        let mut state = self.lock();
        state.requester = None;
        for held in state.attachments.values_mut() {
            if !held.dropped {
                held.dropped = true;
                held.queue.push_boundary(Pending::ResyncRequired);
            }
        }
    }

    /// Offers `observations`, a switch's replay, to `attachment` alone: the
    /// root attachment that replaced `moved`, which the switch ended. An
    /// answer window a moved attachment's intent opened is that
    /// attachment's: `moved`'s reaches `attachment` with its own
    /// correlation, another's reaches it not at all; what answers no
    /// attachment's intent reaches it as it does every root attachment.
    fn replay(
        &self,
        attachment: AttachmentId,
        moved: AttachmentId,
        requester: AttachmentId,
        observations: Vec<fanout::Publication<P>>,
    ) {
        {
            let mut state = self.lock();
            let Some(held) = state.attachments.get_mut(&attachment) else {
                return;
            };
            let capacity = held.capacity(self.capacity);
            let mut window: Option<AttachmentId> = None;
            for fanout::Publication { observation, size } in observations {
                let class = observation.class();
                let observation = match observation {
                    SessionObservation::IntentStarted(started) => {
                        let named = started.map(|started| {
                            started
                                .strip_prefix(ANSWER)
                                .unwrap_or(started.as_str())
                                .to_owned()
                        });
                        let answered = named.as_deref().and_then(attachment_of);
                        window = answered.as_ref().map(|(owner, _)| *owner);
                        match answered {
                            Some((owner, _)) if owner != moved && owner != requester => continue,
                            Some((_, client)) => SessionObservation::IntentStarted(client),
                            None => SessionObservation::IntentStarted(named),
                        }
                    }
                    observation => observation,
                };
                let shown = match baseline_scope(&observation) {
                    Some(scope) => held.scopes.contains(scope),
                    None => {
                        class != ObservationClass::Addressed
                            || window.is_none_or(|owner| owner == moved || owner == requester)
                    }
                };
                if shown {
                    held.offer(observation.into(), class, capacity, size);
                }
            }
        }
        self.repair_gaps();
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

    /// Attaches to `node` for a connection of `face`, queueing the attach
    /// snapshot first. A child node's attachment holds that node's
    /// transcript only, and its attach snapshot is that transcript's
    /// baseline ([`ChildNodes`]). A remote face's attachment never takes a
    /// local-only observation, in its snapshot or after.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::UnknownNode`] for a node that is neither the
    /// session's root nor one of its tree's children;
    /// [`RequestRefusal::NodeClosed`] and [`RequestRefusal::NodeRetiring`]
    /// for a child that is closed or retiring;
    /// [`RequestRefusal::NodeRetiring`] too once the session is releasing
    /// ([`AttachmentCount`]).
    pub fn attach(&self, node: &NodeAddress, face: Face) -> Result<AttachmentId, RequestRefusal> {
        self.attach_asking(node, face, true)
    }

    /// As [`Self::attach`], except that a root attach with no baseline kept
    /// asks the session immediately only when `ask`: a caller expecting
    /// an opening snapshot can let the normal repair deadline cover it.
    ///
    /// # Errors
    ///
    /// As [`Self::attach`].
    pub fn attach_asking(
        &self,
        node: &NodeAddress,
        face: Face,
        ask: bool,
    ) -> Result<AttachmentId, RequestRefusal> {
        if node.tree_id != self.root.tree_id {
            return Err(RequestRefusal::UnknownNode);
        }
        if node.agent_id != self.root.agent_id {
            return self.attach_child(&node.agent_id, face);
        }
        let Some(mailbox) = self.mailbox() else {
            return Err(RequestRefusal::Disconnected);
        };
        let (id, asked) = {
            let mut state = self.lock();
            let now = std::time::Instant::now();
            let (id, mut attachment, asked) = self.admission(&mut state, Some(mailbox), face);
            if asked.is_some() {
                attachment.await_baseline(&TranscriptScope::Root, now);
            }
            if attachment.dropped {
                return Err(RequestRefusal::Backpressure);
            }
            state.insert_counted(id, attachment)?;
            let asked = asked.filter(|_| ask).and_then(|sender| {
                state
                    .reserve_repair(&TranscriptScope::Root, now)
                    .map(|deadline| (sender, deadline))
            });
            (id, asked)
        };
        if let Some((sender, deadline)) = asked
            && let Err(error) = sender.request_root_attachment_repair()
        {
            let mut state = self.lock();
            state.cancel_repair(&TranscriptScope::Root, deadline);
            state.remove_counted(id);
            return Err(refusal_of(&error));
        }
        Ok(id)
    }

    /// A new attachment with the attach snapshot queued, its id, and the
    /// sender to ask the session for a root baseline through when none is
    /// kept. With no `mailbox` it is a face's, which sends its own intents.
    fn admission(
        &self,
        state: &mut State<P>,
        mailbox: Option<IntentSender<P>>,
        face: Face,
    ) -> (AttachmentId, Attachment<P>, Option<IntentSender<P>>) {
        let id = state.next_attachment;
        state.next_attachment += 1;
        let waiting = state.holding;
        let snapshot = &state.snapshot;
        let mut items: Vec<(Payload<P>, usize)> = snapshot
            .states
            .iter()
            .cloned()
            .map(|(event, size)| (Payload::Other(event), size))
            .collect();
        let needs_baseline = snapshot.baseline.is_none() && !waiting;
        if let Some(baseline) = &snapshot.baseline {
            items.extend(baseline.observations());
        }
        items.extend(
            snapshot
                .approvals
                .iter()
                .map(|(_, asked, size)| (Payload::Other(asked.clone()), *size)),
        );
        items.extend(
            snapshot
                .credential_wait
                .iter()
                .cloned()
                .map(|(event, size)| (Payload::Other(event), size)),
        );
        if let Some(attention) = state.count.attention() {
            let event = SessionObservation::Row(attention.notice());
            // A fixed reason, remedy and count, with no user data.
            items.push((Payload::Other(event), 512));
        }
        let sender = mailbox.map(|actions| actions.tagged(origin(id, face)));
        let scopes = match self.actions {
            Some(_) => Scopes::Held(BTreeSet::from([TranscriptScope::Root])),
            None => Scopes::All,
        };
        let mut attachment = Attachment {
            face,
            reply_origin: id,
            sender,
            node: None,
            queue: Queue::default(),
            seq: 0,
            scopes,
            gapped: BTreeSet::new(),
            fresh: BTreeSet::new(),
            repairing: BTreeMap::new(),
            retained_after: None,
            keyed: BTreeSet::new(),
            dropped: false,
            waiting,
        };
        for (observation, size) in items {
            let class = observation.class();
            attachment.offer(observation, class, usize::MAX, size);
        }
        attachment.queue.mark_initial();
        let ask = attachment
            .sender
            .clone()
            .filter(|_| needs_baseline || attachment.fresh.contains(&TranscriptScope::Root));
        (id, attachment, ask)
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
        if test_draft_gate::defer(self, &sender, &intent, client, &state.pending_drafts)? {
            return Ok(());
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
    #[allow(
        clippy::must_use_candidate,
        reason = "draining is the effect even when a caller discards the queued observations"
    )]
    pub fn take(&self, attachment: AttachmentId) -> Vec<Outbound<P>> {
        self.take_shared(attachment)
            .into_iter()
            .map(Pending::into_outbound)
            .collect()
    }

    fn take_shared(&self, attachment: AttachmentId) -> Vec<Pending<P>> {
        let mut taken = Vec::new();
        // The first drain makes room; the second includes any kept baseline
        // that repays its gaps. Never spin waiting for a mailbox snapshot.
        for _ in 0..2 {
            self.repair_gaps();
            taken.extend(self.take_queued(attachment, usize::MAX, |_| true));
        }
        taken
    }

    fn take_for_wire(
        &self,
        attachment: AttachmentId,
        bulk: bool,
        small_only: bool,
    ) -> Vec<Pending<P>> {
        self.take_queued(attachment, 1, |head| match head {
            Some(item @ Pending::Event { .. }) => bulk && !(small_only && item.paced_alone()),
            _ => true,
        })
    }

    // Fence incoming intents when residency ends, but let the transport
    // deliver everything already owed before it emits Detached.
    fn finish_attachment(&self, attachment: AttachmentId) {
        if let Some(held) = self.lock().attachments.get_mut(&attachment) {
            held.dropped = true;
        }
    }

    fn has_queued(&self, attachment: AttachmentId) -> bool {
        self.lock()
            .attachments
            .get(&attachment)
            .is_some_and(|held| !held.queue.is_empty())
    }

    fn lose_wire_patch(&self, attachment: AttachmentId, scope: &TranscriptScope) {
        if let Some(held) = self.lock().attachments.get_mut(&attachment) {
            held.miss_patch(scope);
        }
        self.repair_gaps();
    }

    fn take_queued(
        &self,
        attachment: AttachmentId,
        limit: usize,
        allow: impl FnOnce(Option<&Pending<P>>) -> bool,
    ) -> Vec<Pending<P>> {
        // As in `detach`, the senders this ends drop after the lock.
        let mailbox = self.mailbox();
        let mut state = self.lock();
        let Some(held) = state.attachments.get_mut(&attachment) else {
            return Vec::new();
        };
        if !allow(held.queue.front()) {
            return Vec::new();
        }
        let taken = held.queue.take(limit);
        let removed = if held.dropped && held.queue.is_empty() {
            state.remove(attachment, mailbox.as_ref())
        } else {
            None
        };
        drop(state);
        drop(removed);
        taken
    }

    pub(super) fn lifecycle_busy(&self) -> plakous_host_protocol::wire::LifecycleReason {
        use plakous_host_protocol::views::ProcessStatus;
        use plakous_host_protocol::wire::{LifecycleReason, MAX_LIFECYCLE_IDS};
        let state = self.lock();
        let mut turn = false;
        let mut processes = Vec::new();
        for (observation, _) in &state.snapshot.states {
            match observation {
                SessionObservation::SetBusy(busy) => turn = *busy,
                SessionObservation::Processes { views, .. } => {
                    processes = views
                        .iter()
                        .filter(|view| {
                            matches!(
                                view.status,
                                ProcessStatus::Running | ProcessStatus::Stopping
                            )
                        })
                        .map(|view| view.id.clone())
                        .collect();
                }
                _ => {}
            }
        }
        drop(state);
        let children = tree_nodes::lifecycle_children(&self.root().tree_id);
        let state_unknown = children.is_err();
        let mut children = children.unwrap_or_default();
        let children_truncated = children.len() > MAX_LIFECYCLE_IDS;
        let processes_truncated = processes.len() > MAX_LIFECYCLE_IDS;
        children.truncate(MAX_LIFECYCLE_IDS);
        processes.truncate(MAX_LIFECYCLE_IDS);
        LifecycleReason::Busy {
            turn,
            state_unknown: state_unknown || (!turn && children.is_empty() && processes.is_empty()),
            children,
            children_truncated,
            processes,
            processes_truncated,
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
        if client.is_none()
            && matches!(
                &intent,
                SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Root
                }
            )
            && self
                .attachments
                .get(&attachment)
                .is_some_and(|held| held.sender.is_some())
        {
            return self
                .request_attached_root_repair(attachment, sender)
                .map_err(Unsent::Mailbox);
        }
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
        self.remove_counted(attachment)
    }

    /// Adds `attachment` under `id`, counted.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::NodeRetiring`] once the session is releasing: it
    /// takes no attachment.
    fn insert_counted(
        &mut self,
        id: AttachmentId,
        attachment: Attachment<P>,
    ) -> Result<(), RequestRefusal> {
        if !self.count.try_add() {
            return Err(RequestRefusal::NodeRetiring);
        }
        if self.attachments.insert(id, attachment).is_some() {
            self.count.remove();
        }
        Ok(())
    }

    /// Adds an in-process face's `attachment` under `id`, counted.
    fn insert_face(&mut self, id: AttachmentId, attachment: Attachment<P>) {
        if self.attachments.insert(id, attachment).is_none() {
            self.count.add();
        }
    }

    /// Takes the attachment `id` out, counted.
    fn remove_counted(&mut self, id: AttachmentId) -> Option<Attachment<P>> {
        self.draft_outputs
            .retain(|(attachment, _), _| *attachment != id);
        let removed = self.attachments.remove(&id);
        if removed.is_some() {
            self.count.remove();
        }
        removed
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

    fn reply_recipient(&self, requester: AttachmentId) -> Option<AttachmentId> {
        if self
            .attachments
            .get(&requester)
            .is_some_and(|held| !held.dropped)
        {
            return Some(requester);
        }
        self.attachments
            .iter()
            .find_map(|(id, held)| (!held.dropped && held.reply_origin == requester).then_some(*id))
    }

    /// `attachment` stops holding `agent`'s transcript.
    fn release_agent(&mut self, attachment: AttachmentId, agent: &str) {
        if let Some(held) = self.attachments.get_mut(&attachment)
            && let Scopes::Held(scopes) = &mut held.scopes
            && scopes.remove(&TranscriptScope::Agent(agent.to_owned()))
            && let Some(count) = self.agent_holders.get_mut(agent)
        {
            held.gapped
                .remove(&TranscriptScope::Agent(agent.to_owned()));
            held.fresh.remove(&TranscriptScope::Agent(agent.to_owned()));
            held.repairing
                .remove(&TranscriptScope::Agent(agent.to_owned()));
            held.keyed.remove(&TranscriptScope::Agent(agent.to_owned()));
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.agent_holders.remove(agent);
                self.resync_wait
                    .remove(&TranscriptScope::Agent(agent.to_owned()));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;

pub(crate) mod wake_stats;
