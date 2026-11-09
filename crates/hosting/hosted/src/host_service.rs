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
pub(crate) mod drafts;
mod face;
mod tree_nodes;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use plakous_host_protocol::observation::{Delivery, ObservationClass};
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress, RequestRefusal};
use plakous_host_protocol::{
    IntentSendError, IntentSender, ObservationExtension, ProtocolExtension, SessionIntent,
    SessionObservation, WeakIntentSender,
};

use self::wire::Face;

pub use child::{ChildNodes, ControlDone, NodeState};
pub use face::{FacePublisher, FaceReceiver, FaceSender, face};
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
        /// Process-local wire origin, never part of the shared observation.
        client_id: Option<plakous_host_protocol::wire::ClientDraftId>,
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
        /// The session request that caused the switch: the attachment
        /// that asked and its client's id for it.
        request: Option<(AttachmentId, String)>,
    },
}

/// One attachment's state.
struct Attachment<P: ProtocolExtension> {
    /// Its connection's face: a remote one takes no local-only observation.
    face: Face,
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
    /// Admitted while the service held what follows a session opening it
    /// could not resolve: it takes nothing until the opening's binding ends
    /// it with `moved` or `resync_required`.
    waiting: bool,
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
    pending_drafts: BTreeMap<plakous_host_protocol::wire::ClientDraftId, drafts::PendingDraft>,
    draft_window: Option<plakous_host_protocol::wire::ClientDraftId>,
    draft_answer: bool,
    attachments: BTreeMap<AttachmentId, Attachment<P>>,
    next_attachment: AttachmentId,
    snapshot: Snapshot<P>,
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
    /// How many host requests ask the session to release as soon as
    /// nothing keeps it, without waiting out the idle release
    /// (`release_session`).
    release_now: std::sync::atomic::AtomicU32,
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
        marked
    }

    /// Asks the session to release as soon as nothing keeps it resident,
    /// without waiting out the idle release; an attach or a hold still
    /// keeps it.
    pub fn request_release_now(&self) {
        self.0.release_now.fetch_add(1, Ordering::AcqRel);
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
        }
    }

    /// Whether a host asked the session to release now.
    pub(crate) fn release_now(&self) -> bool {
        self.0.release_now.load(Ordering::Acquire) > 0
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
        self.0
            .bits
            .try_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (word & RELEASING == 0).then_some(word.wrapping_add(TAKEN))
            })
            .is_ok()
    }

    /// Counts one more, releasing or not: an in-process face's attachment,
    /// whose session is never released for being idle.
    fn add(&self) {
        self.0.bits.fetch_add(TAKEN, Ordering::AcqRel);
    }

    fn remove(&self) {
        self.0.bits.fetch_sub(1, Ordering::AcqRel);
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

/// What a session's host service kept that its host's directory shows
/// ([`HostService::kept_activity`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeptActivity {
    /// A turn runs, or a child's work is live.
    pub busy: bool,
    /// The root's own turn runs, whatever its children do.
    pub turn_busy: bool,
    /// An approval, credential wait or context-capacity decision needs the operator.
    pub awaiting_operator: bool,
    /// Background processes running or stopping.
    pub processes: usize,
    /// Children whose work is live, including parked and sealing children.
    pub children: usize,
    /// Children awaiting consent to continue after host loss.
    pub held_children: usize,
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
    ///
    /// `request` names the session request that caused the switch, if one
    /// did: the attachment that asked and its client's id for it. Returns
    /// the attachments it ended.
    pub fn move_attachments(
        &self,
        node: &NodeAddress,
        request: Option<(AttachmentId, &str)>,
    ) -> Vec<AttachmentId> {
        let mut state = self.lock();
        state.requester = None;
        let mut moved = Vec::new();
        for (id, held) in &mut state.attachments {
            if !held.dropped {
                held.dropped = true;
                held.queue.push_back(Outbound::Moved {
                    node: node.clone(),
                    request: request.map(|(requester, id)| (requester, id.to_owned())),
                });
                moved.push(*id);
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
        }
        for held in state.attachments.values_mut() {
            let gapped = match &held.scopes {
                Scopes::Held(scopes) => scopes.iter().any(&mut lost),
                Scopes::All => false,
            };
            if gapped && !held.dropped {
                held.dropped = true;
                held.queue.push_back(Outbound::ResyncRequired);
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
                held.queue.push_back(Outbound::ResyncRequired);
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
        observations: Vec<SessionObservation<P>>,
    ) {
        {
            let mut state = self.lock();
            let Some(held) = state.attachments.get_mut(&attachment) else {
                return;
            };
            let capacity = held.capacity(self.capacity);
            let mut window: Option<AttachmentId> = None;
            for observation in observations {
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
                            Some((owner, _)) if owner != moved => continue,
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
                            || window.is_none_or(|owner| owner == moved)
                    }
                };
                if shown {
                    held.offer(observation, class, capacity);
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
    /// asks the session for one only when `ask`: a caller that knows the
    /// session's next root baseline reaches the attachment need not.
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
            let (id, attachment, asked) = self.admission(&mut state, Some(mailbox), face);
            state.insert_counted(id, attachment)?;
            (id, asked)
        };
        if let Some(sender) = asked.filter(|_| ask) {
            // No baseline is kept: the session's fresh root snapshot reaches
            // every attachment holding the root, this one included. An
            // attach the session cannot be asked for one fails.
            if let Err(error) = sender.send(SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Root,
            }) {
                self.lock().remove_counted(id);
                return Err(refusal_of(&error));
            }
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
        let mut items: Vec<SessionObservation<P>> = snapshot.states.clone();
        let needs_baseline = snapshot.baseline.is_none() && !waiting;
        if let Some((baseline, patches)) = &snapshot.baseline {
            items.push(baseline.clone());
            items.extend(patches.iter().cloned());
        }
        items.extend(snapshot.approvals.iter().map(|(_, asked)| asked.clone()));
        items.extend(snapshot.credential_wait.iter().cloned());
        items.retain(|item| face.receives(item) && !waiting);
        let sender = mailbox.map(|actions| actions.tagged(origin(id)));
        let scopes = match self.actions {
            Some(_) => Scopes::Held(BTreeSet::from([TranscriptScope::Root])),
            None => Scopes::All,
        };
        let ask = sender.clone().filter(|_| needs_baseline);
        let mut attachment = Attachment {
            face,
            sender,
            node: None,
            queue: VecDeque::new(),
            seq: 0,
            scopes,
            gapped: BTreeSet::new(),
            snapshot_held: items.len(),
            dropped: false,
            waiting,
        };
        for observation in items {
            attachment.seq += 1;
            attachment.queue.push_back(Outbound::Event {
                client_id: None,
                seq: attachment.seq,
                class: observation.class(),
                observation,
            });
        }
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

    /// What this service kept of the session that a directory shows: whether
    /// a turn runs, whether an approval or a credential wait needs the
    /// operator, and how many background processes run.
    #[must_use]
    pub fn kept_activity(&self) -> KeptActivity {
        let state = self.lock();
        let snapshot = &state.snapshot;
        let mut kept = KeptActivity {
            awaiting_operator: !snapshot.approvals.is_empty() || snapshot.credential_wait.is_some(),
            ..KeptActivity::default()
        };
        for observation in &snapshot.states {
            match observation {
                SessionObservation::SetBusy(busy) => {
                    kept.busy = *busy;
                    kept.turn_busy = *busy;
                }
                SessionObservation::Processes { running, .. } => kept.processes = *running,
                SessionObservation::ContextCapacityRecovery(Some(_)) => {
                    kept.awaiting_operator = true;
                }
                _ => {}
            }
        }
        drop(state);
        kept.awaiting_operator |= crate::runtime::tree_needs_operator(&self.root().tree_id);
        (kept.children, kept.held_children) = tree_nodes::child_counts(&self.root().tree_id);
        kept.busy |= kept.children > 0;
        kept
    }

    pub(super) fn lifecycle_busy(&self) -> plakous_host_protocol::wire::LifecycleReason {
        use plakous_host_protocol::views::ProcessStatus;
        use plakous_host_protocol::wire::{LifecycleReason, MAX_LIFECYCLE_IDS};
        let state = self.lock();
        let mut turn = false;
        let mut processes = Vec::new();
        for observation in &state.snapshot.states {
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

    /// Takes one observation from the session: keeps what an attach needs
    /// and queues it for the attachments its class names.
    pub fn observe(&self, observation: SessionObservation<P>) {
        {
            let mut state = self.lock();
            let client_id = state.observe_draft(&observation);
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
                        held.correlate_draft(client_id);
                    }
                }
                if let Some(held) = state.attachments.get_mut(last) {
                    let capacity = held.capacity(self.capacity);
                    held.offer(observation, class, capacity);
                    held.correlate_draft(client_id);
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
        if self.dropped || self.waiting || !self.face.receives(&observation) {
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
            client_id: None,
            seq: self.seq,
            class,
            observation,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
