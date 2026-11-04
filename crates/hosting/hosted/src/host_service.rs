//! The single-session host service (wire v2, spec 02 §4.4): one session's
//! observations fanned out to its attachments by class, each attachment's
//! bounded outbound queue, the attach snapshot, and the slow-consumer drop.
//!
//! The service is the one consumer of the session's observations
//! ([`HostService::observe`]) and the one sender of its attachments'
//! intents ([`HostService::intent`]). Every observation takes its class
//! ([`SessionObservation::class`]):
//!
//! * a **state** is broadcast and kept as the latest of its kind;
//! * an **event** is broadcast; a required one is never dropped;
//! * an **addressed** observation goes to the attachment whose intent the
//!   session is answering: the one the last `IntentStarted` names, which
//!   gets back its own correlation in it. An answer to no attachment's
//!   intent goes to every attachment.
//!
//! A transcript observation (snapshot, keyed patch, stream) goes, whatever
//! its class, to every attachment holding its scope: the root's to all, an
//! agent's to those that opened it.
//!
//! An attachment starts with the attach snapshot: every state, the root
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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use plakous_host_protocol::observation::{Delivery, ObservationClass};
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress, RequestRefusal};
use plakous_host_protocol::{
    IntentSendError, IntentSender, ObservationExtension, ProtocolExtension, SessionIntent,
    SessionObservation,
};

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
}

/// One attachment's state.
struct Attachment<P: ProtocolExtension> {
    queue: VecDeque<Outbound<P>>,
    seq: u64,
    scopes: BTreeSet<TranscriptScope>,
    /// Scopes whose transcript content this attachment lost to a full
    /// queue; repaid with a fresh baseline.
    gapped: BTreeSet<TranscriptScope>,
    /// Attach-snapshot items still at the head of the queue; they do not
    /// count against the capacity.
    snapshot_held: usize,
    dropped: bool,
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

/// One session's host service.
pub struct HostService<P: ProtocolExtension> {
    actions: IntentSender<P>,
    root: NodeAddress,
    capacity: usize,
    state: Mutex<State<P>>,
}

/// The correlation an attachment's intents carry, so the session's
/// `IntentStarted` names the attachment its answers belong to.
fn correlation(attachment: AttachmentId, client: Option<&str>) -> String {
    match client {
        Some(client) => format!("attachment:{attachment}:{client}"),
        None => format!("attachment:{attachment}"),
    }
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
        Self {
            actions,
            root,
            capacity: capacity.max(1),
            state: Mutex::new(State {
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
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<P>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Attaches to `node`, queueing the attach snapshot first.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::UnknownNode`] for any node but the session's root.
    pub fn attach(&self, node: &NodeAddress) -> Result<AttachmentId, RequestRefusal> {
        if *node != self.root {
            return Err(RequestRefusal::UnknownNode);
        }
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
        let mut attachment = Attachment {
            queue: VecDeque::new(),
            seq: 0,
            scopes: BTreeSet::from([TranscriptScope::Root]),
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
        drop(state);
        if needs_baseline {
            // No baseline is kept: the session's fresh root snapshot reaches
            // every attachment holding the root, this one included. An
            // attach the session cannot be asked for one fails.
            if let Err(error) = self.actions.send_correlated(
                SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Root,
                },
                Some(correlation(id, None)),
            ) {
                self.lock().attachments.remove(&id);
                return Err(refusal_of(&error));
            }
        }
        Ok(id)
    }

    /// Detaches `attachment`; the session runs on.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::NotAttached`] when it is not attached.
    pub fn detach(&self, attachment: AttachmentId) -> Result<(), RequestRefusal> {
        let mut state = self.lock();
        let agents: Vec<String> = state
            .attachments
            .get(&attachment)
            .into_iter()
            .flat_map(|held| held.scopes.iter())
            .filter_map(|scope| match scope {
                TranscriptScope::Agent(agent) => Some(agent.clone()),
                TranscriptScope::Root => None,
            })
            .collect();
        for agent in &agents {
            state.release_agent(attachment, agent);
        }
        match state.attachments.remove(&attachment) {
            Some(held) if !held.dropped => Ok(()),
            _ => Err(RequestRefusal::NotAttached),
        }
    }

    /// Sends `intent` from `attachment` into the session's mailbox.
    ///
    /// # Errors
    ///
    /// [`RequestRefusal::NotAttached`] for an attachment not attached (or
    /// dropped); the mailbox's own refusals otherwise. A refused intent did
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
        let held_agent = held.scopes.iter().find_map(|scope| match scope {
            TranscriptScope::Agent(agent) => Some(agent.clone()),
            TranscriptScope::Root => None,
        });
        // An agent transcript is opened in the session by its first holder
        // and closed by its last; the others only hold or release it.
        let forwarded = match &intent {
            SessionIntent::OpenAgentTranscript { agent_id }
                if state.agent_holders.get(agent_id).copied().unwrap_or(0) > 0
                    && held_agent.as_deref() != Some(agent_id) =>
            {
                SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Agent(agent_id.clone()),
                }
            }
            SessionIntent::CloseAgentTranscript
                if held_agent.as_ref().is_none_or(|agent| {
                    state.agent_holders.get(agent).copied().unwrap_or(0) > 1
                }) =>
            {
                if let Some(agent) = held_agent {
                    state.release_agent(attachment, &agent);
                }
                return Ok(());
            }
            _ => intent.clone(),
        };
        self.actions
            .send_correlated(forwarded, Some(correlation(attachment, client)))
            .map_err(|error| refusal_of(&error))?;
        // Scope changes take effect only once the session took the intent.
        match intent {
            SessionIntent::OpenAgentTranscript { agent_id } => {
                if let Some(agent) = held_agent.filter(|agent| *agent != agent_id) {
                    state.release_agent(attachment, &agent);
                }
                if let Some(held) = state.attachments.get_mut(&attachment)
                    && held.scopes.insert(TranscriptScope::Agent(agent_id.clone()))
                {
                    *state.agent_holders.entry(agent_id).or_default() += 1;
                }
            }
            SessionIntent::CloseAgentTranscript => {
                if let Some(agent) = held_agent {
                    state.release_agent(attachment, &agent);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Takes everything queued for `attachment`. A dropped attachment
    /// yields its resync-required once and is then gone.
    pub fn take(&self, attachment: AttachmentId) -> Vec<Outbound<P>> {
        self.repair_gaps();
        let mut state = self.lock();
        let Some(held) = state.attachments.get_mut(&attachment) else {
            return Vec::new();
        };
        let taken: Vec<_> = held.queue.drain(..).collect();
        held.snapshot_held = 0;
        if held.dropped {
            state.attachments.remove(&attachment);
        }
        taken
    }

    /// Takes one observation from the session: keeps what an attach needs
    /// and queues it for the attachments its class names.
    pub fn observe(&self, observation: SessionObservation<P>) {
        {
            let mut state = self.lock();
            let class = observation.class();
            state.remember(&observation, class);
            let mut observation = observation;
            let targets: Vec<AttachmentId> = match (class, baseline_scope(&observation)) {
                (_, Some(scope)) => state
                    .attachments
                    .iter()
                    .filter(|(_, held)| held.scopes.contains(scope))
                    .map(|(id, _)| *id)
                    .collect(),
                (ObservationClass::Addressed, None) => {
                    if let SessionObservation::IntentStarted(started) = &observation {
                        let owner = started.as_deref().and_then(attachment_of);
                        state.requester = owner.as_ref().map(|(id, _)| *id);
                        if let Some((_, client)) = owner {
                            // The attachment sees its own correlation.
                            observation = SessionObservation::IntentStarted(client);
                        }
                    }
                    // An answer to no attachment's intent belongs to none:
                    // every attachment gets it.
                    match state.requester {
                        Some(owner) => vec![owner],
                        None => state.attachments.keys().copied().collect(),
                    }
                }
                _ => state.attachments.keys().copied().collect(),
            };
            for id in targets {
                if let Some(held) = state.attachments.get_mut(&id) {
                    held.offer(observation.clone(), class, self.capacity);
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
            let baseline: Option<Vec<SessionObservation<P>>> =
                state.snapshot.baseline.as_ref().map(|(snapshot, patches)| {
                    std::iter::once(snapshot.clone())
                        .chain(patches.iter().cloned())
                        .collect()
                });
            for (id, held) in &mut state.attachments {
                if held.dropped {
                    continue;
                }
                for scope in std::mem::take(&mut held.gapped) {
                    match (&scope, &baseline) {
                        (TranscriptScope::Root, Some(baseline)) => {
                            for item in baseline {
                                held.offer(item.clone(), item.class(), self.capacity);
                            }
                        }
                        _ => asks.push((*id, scope)),
                    }
                }
            }
        }
        for (id, scope) in asks {
            let _ = self.actions.send_correlated(
                SessionIntent::ResyncTranscript { scope },
                Some(correlation(id, None)),
            );
        }
    }
}

/// The request refusal a mailbox refusal is.
fn refusal_of<P: ProtocolExtension>(error: &IntentSendError<P>) -> RequestRefusal {
    match error {
        IntentSendError::Saturated(_) => RequestRefusal::Backpressure,
        IntentSendError::Duplicate(_) => RequestRefusal::OneInFlight,
        IntentSendError::Disconnected(_) => RequestRefusal::Disconnected,
    }
}

impl<P: ProtocolExtension> State<P> {
    /// `attachment` stops holding `agent`'s transcript.
    fn release_agent(&mut self, attachment: AttachmentId, agent: &str) {
        if let Some(held) = self.attachments.get_mut(&attachment)
            && held
                .scopes
                .remove(&TranscriptScope::Agent(agent.to_owned()))
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
