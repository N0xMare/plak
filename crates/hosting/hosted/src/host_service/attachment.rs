//! Attachment correlation tags and attach/replay classification.

use plakous_host_protocol::observation::Delivery;
use plakous_host_protocol::views::transcript::TranscriptStreamDelta;
use plakous_host_protocol::wire::{NodeAddress, RequestRefusal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::queue::MAX_QUEUE_BYTES;
use super::retained::{ViewportBase, ViewportRead, ViewportStart};
use super::{
    Attachment, AttachmentId, Face, HostService, LOCAL_ATTACHMENT, ObservationClass, Payload,
    Pending, ProtocolExtension, Queue, REMOTE_ATTACHMENT, SessionObservation, TranscriptScope,
    same_kind,
};

const STATUS_INTERVAL: Duration = Duration::from_secs(10);

/// Counters and current producer-queue gauges for one attachment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttachmentStatus {
    /// Owed text deltas rejected by queue pressure or removed from the queue
    /// before delivery. Excludes unnumbered previews withheld until a baseline
    /// or patch makes their scope usable, and teardown of the entire queue.
    pub deltas_evicted: u64,
    /// Patches not delivered because the attachment needed a new base.
    pub patches_skipped: u64,
    /// Complete logical baselines sent by the writer.
    pub baselines_sent: u64,
    /// Discontinuities observed by the writer in delivered sequences.
    pub seq_gaps: u64,
    /// Bytes charged to the attachment's producer queue.
    pub bytes_queued: u64,
    /// Entries in the attachment's producer queue.
    pub queue_len: u64,
}

pub(super) struct AttachmentMeter {
    values: AttachmentStatus,
    last_periodic: AttachmentStatus,
    next_periodic: Instant,
    seq: u64,
}

impl AttachmentMeter {
    fn new(now: Instant, seq: u64) -> Self {
        Self {
            values: AttachmentStatus::default(),
            last_periodic: AttachmentStatus::default(),
            next_periodic: now + STATUS_INTERVAL,
            seq,
        }
    }

    fn discarded<P: ProtocolExtension>(&mut self, item: &Pending<P>) {
        if let Pending::Event { observation, .. } = item {
            match observation {
                Payload::Patch(_) => self.skip_patch(),
                Payload::Other(event) if is_delta(event) => self.evict_delta(),
                _ => {}
            }
        }
    }

    const fn evict_delta(&mut self) {
        self.values.deltas_evicted = self.values.deltas_evicted.saturating_add(1);
    }

    const fn skip_patch(&mut self) {
        self.values.patches_skipped = self.values.patches_skipped.saturating_add(1);
    }
}

fn is_delta<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(
        event,
        SessionObservation::TranscriptStream {
            delta: TranscriptStreamDelta::Assistant(_)
                | TranscriptStreamDelta::Reasoning { .. }
                | TranscriptStreamDelta::Keyed { .. },
            ..
        } | SessionObservation::AssistantDelta(_)
            | SessionObservation::ReasoningDelta { .. }
    )
}

#[derive(Clone, Copy)]
pub(super) enum SentTranscript {
    Sequence(u64),
    Baseline(Option<u64>),
}

impl SentTranscript {
    pub(super) const fn sequence(self) -> Option<u64> {
        match self {
            Self::Sequence(seq) | Self::Baseline(Some(seq)) => Some(seq),
            Self::Baseline(None) => None,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct SentBatch {
    sequences: Option<(u64, u64)>,
    gaps: u64,
    baselines: u64,
}

impl SentBatch {
    pub(super) fn record(&mut self, sent: SentTranscript) {
        if matches!(sent, SentTranscript::Baseline(_)) {
            self.baselines = self.baselines.saturating_add(1);
        }
        if let Some(seq) = sent.sequence() {
            let first = self.sequences.map_or(seq, |(first, last)| {
                if last.checked_add(1) != Some(seq) {
                    self.gaps = self.gaps.saturating_add(1);
                }
                first
            });
            self.sequences = Some((first, seq));
        }
    }
}

impl<P: ProtocolExtension> HostService<P> {
    pub(super) fn viewport_read(
        &self,
        node: &NodeAddress,
        generation: u64,
    ) -> Option<ViewportRead> {
        if node != self.root() {
            return None;
        }
        let mut state = self.lock();
        let tip = state.root_revision;
        Some(state.snapshot.viewports.read(node.clone(), generation, tip))
    }

    pub(super) fn install_viewport(
        &self,
        read: &ViewportRead,
        line: Arc<str>,
        base: ViewportBase<'_>,
        face: Face,
    ) -> Result<Option<ViewportStart>, RequestRefusal> {
        if read.node != *self.root() {
            return Ok(None);
        }
        let mailbox = self.mailbox().ok_or(RequestRefusal::Disconnected)?;
        let mut state = self.lock();
        if state.holding {
            return Ok(None);
        }
        let first_page = matches!(base, ViewportBase::FirstPage(_));
        // Appends are not revisions. A later keyed offset exposes any bytes
        // missed while reading the page or reconnecting; the client can read
        // that suffix by row without waiting for a patch to end the live run.
        let Some((revision, patches)) = state.snapshot.viewports.plan(read, &line, base) else {
            return Ok(None);
        };
        let (attachment, held, _) =
            self.admission(&mut state, Some(mailbox), face, true, Some(patches.len()));
        if held.dropped {
            return Err(RequestRefusal::Backpressure);
        }
        state.insert_counted(attachment, held)?;
        if first_page {
            state.snapshot.viewports.remember(revision.clone(), line);
        }
        Ok(Some(ViewportStart {
            attachment,
            revision,
            patches,
        }))
    }

    pub(super) fn resync_viewport(&self, id: AttachmentId) {
        if let Some(held) = self.lock().attachments.get_mut(&id) {
            held.dropped = true;
            held.queue.clear();
            held.queue.push_boundary(Pending::ResyncRequired);
        }
    }

    pub(super) fn enable_attachment_status(&self, id: AttachmentId, now: Instant, seq: u64) {
        if let Some(held) = self.lock().attachments.get_mut(&id) {
            held.meter
                .get_or_insert_with(|| AttachmentMeter::new(now, seq));
        }
    }

    pub(super) fn status_deadline(&self, id: AttachmentId) -> Option<Instant> {
        let state = self.lock();
        let held = state.attachments.get(&id)?;
        let meter = held.meter.as_ref()?;
        (held.status_values(meter) != meter.last_periodic).then_some(meter.next_periodic)
    }

    pub(super) fn attachment_status(
        &self,
        id: AttachmentId,
        now: Instant,
        requested: bool,
    ) -> Option<AttachmentStatus> {
        let mut state = self.lock();
        let held = state.attachments.get_mut(&id)?;
        let values = held.status_values(held.meter.as_ref()?);
        let meter = held.meter.as_mut()?;
        if requested {
            return Some(values);
        }
        if now < meter.next_periodic || values == meter.last_periodic {
            return None;
        }
        meter.last_periodic = values;
        meter.next_periodic = now + STATUS_INTERVAL;
        Some(values)
    }

    pub(super) fn transcript_sent(&self, id: AttachmentId, sent: SentBatch) {
        if let Some(meter) = self
            .lock()
            .attachments
            .get_mut(&id)
            .and_then(|held| held.meter.as_mut())
        {
            meter.values.baselines_sent =
                meter.values.baselines_sent.saturating_add(sent.baselines);
            if let Some((first, last)) = sent.sequences {
                let leading_gap = u64::from(meter.seq.checked_add(1) != Some(first));
                meter.values.seq_gaps = meter
                    .values
                    .seq_gaps
                    .saturating_add(leading_gap)
                    .saturating_add(sent.gaps);
                meter.seq = last;
            }
        }
    }
}

/// The origin tag an attachment's intents carry, so the session's
/// `IntentStarted` names the attachment its answers belong to.
pub(super) fn origin(attachment: AttachmentId, face: Face) -> String {
    let prefix = if face == Face::Remote {
        REMOTE_ATTACHMENT
    } else {
        LOCAL_ATTACHMENT
    };
    format!("{prefix}{attachment}")
}

/// The attachment a correlation names, and the client's own correlation
/// inside it.
pub(super) fn attachment_of(correlation: &str) -> Option<(AttachmentId, Option<String>)> {
    let rest = correlation
        .strip_prefix(REMOTE_ATTACHMENT)
        .or_else(|| correlation.strip_prefix(LOCAL_ATTACHMENT))?;
    let (id, client) = match rest.split_once(':') {
        Some((id, client)) => (id, Some(client.to_owned())),
        None => (rest, None),
    };
    Some((id.parse().ok()?, client))
}

/// Whether an attach re-emits `observation` from what the service keeps
/// (states, the root baseline, approvals, the credential wait), so a
/// replay must not send it again.
pub(super) fn kept_for_attach<P: ProtocolExtension>(observation: &SessionObservation<P>) -> bool {
    observation.class() == ObservationClass::State
        || keyed_append_scope(observation).is_some()
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

pub(super) const fn keyed_append_scope<P: ProtocolExtension>(
    observation: &SessionObservation<P>,
) -> Option<&TranscriptScope> {
    match observation {
        SessionObservation::TranscriptStream {
            scope,
            delta: TranscriptStreamDelta::Keyed { .. },
        } => Some(scope),
        _ => None,
    }
}

impl<P: ProtocolExtension> Attachment<P> {
    fn status_values(&self, meter: &AttachmentMeter) -> AttachmentStatus {
        AttachmentStatus {
            bytes_queued: self.queue.bytes() as u64,
            queue_len: self.queue.len() as u64,
            ..meter.values
        }
    }

    pub(super) fn note_patch_skipped(&mut self) {
        if let Some(meter) = &mut self.meter {
            meter.skip_patch();
        }
    }

    pub(super) fn miss_patch(&mut self, scope: &TranscriptScope) {
        self.gapped.insert(scope.clone());
        if self.fresh.insert(scope.clone()) {
            self.discard_transcript(scope);
        }
    }

    pub(super) fn discard_transcript(&mut self, scope: &TranscriptScope) {
        self.queue.retain(|item| {
            let keep = queued_scope(item) != Some(scope);
            if !keep && let Some(meter) = &mut self.meter {
                meter.discarded(item);
            }
            keep
        });
    }

    pub(super) fn queued_baseline(&self, scope: &TranscriptScope) -> bool {
        self.queue.iter().any(|item| {
            matches!(item, Pending::Event {
            observation: Payload::Snapshot(snapshot), ..
        } if snapshot.scope == *scope)
        })
    }

    /// How many items this attachment's queue holds: a face's is unbounded,
    /// since the face reads it on its own thread and the session's mailbox
    /// is the bound it meets.
    pub(super) const fn capacity(&self, service: usize) -> usize {
        if self.sender.is_some() {
            service
        } else {
            usize::MAX
        }
    }

    pub(super) const fn byte_limit(&self) -> usize {
        if self.sender.is_some() {
            MAX_QUEUE_BYTES
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
        if !self.keyed.contains(&scope) {
            self.gapped.insert(scope);
        }
    }

    /// Queues `observation`, making room by its class, or drops the
    /// attachment when what it may not lose finds none.
    pub(super) fn offer(
        &mut self,
        observation: Payload<P>,
        class: ObservationClass,
        capacity: usize,
        encoded: usize,
    ) {
        if self.dropped || self.waiting || !observation.received_by(self.face) {
            return;
        }
        if self.fence_root_preview && observation.is_root_preview() {
            // An attaching viewer cannot use a suffix before the first baseline
            // that will cover it. Do not number observations it was never owed.
            return;
        }
        if let Payload::Other(event) = &observation
            && let Some(scope) = keyed_append_scope(event)
            && (!self.keyed.contains(scope) || self.fresh.contains(scope))
        {
            // A delta cannot open a row or bridge a missing revision.
            return;
        }
        let scope = payload_scope(&observation);
        if matches!(&observation, Payload::Patch(patch) if self.fresh.contains(&patch.scope)) {
            self.note_patch_skipped();
            self.seq += 1;
            return;
        }
        if let Payload::Snapshot(snapshot) = &observation {
            // The snapshot covers everything queued for its scope. Its
            // replacement takes its own turn after surviving observations.
            self.discard_transcript(&snapshot.scope);
        }
        let droppable = class == ObservationClass::Event(Delivery::Droppable);
        let replacement = if class == ObservationClass::State
            && let Payload::Other(new) = &observation
        {
            self.queue.iter().position(|item| {
                matches!(item, Pending::Event { class: ObservationClass::State, observation: Payload::Other(held), .. }
                    if same_kind(held, new))
            })
        } else {
            None
        };
        if let Some(index) = replacement {
            // A queued state of its kind is superseded: the old one goes,
            // and the new one takes its turn after what came before it.
            self.queue.remove(index);
        }
        self.seq += 1;
        let cost = Queue::<P>::charge(encoded);
        if !self.make_room(cost, capacity, !droppable && scope.is_none()) {
            if let Some(scope) = scope {
                if matches!(observation, Payload::Patch(_)) {
                    self.note_patch_skipped();
                }
                self.miss_patch(scope);
            } else if droppable {
                if let Payload::Other(lost) = &observation {
                    if is_delta(lost)
                        && let Some(meter) = &mut self.meter
                    {
                        meter.evict_delta();
                    }
                    self.note_loss(lost);
                }
            } else {
                self.dropped = true;
                self.queue.clear();
                self.queue.push_boundary(Pending::ResyncRequired);
            }
            return;
        }
        if let Payload::Snapshot(snapshot) = &observation {
            // The baseline includes any transcript evicted to make room for it.
            self.gapped.remove(&snapshot.scope);
            self.fresh.remove(&snapshot.scope);
            self.repairing.remove(&snapshot.scope);
            self.keyed.insert(snapshot.scope.clone());
            if snapshot.scope == TranscriptScope::Root {
                self.fence_root_preview = false;
            }
        }
        if matches!(
            observation,
            Payload::Other(SessionObservation::ClearView | SessionObservation::SessionOpening)
        ) {
            self.keyed.clear();
        }
        self.queue.push(
            Pending::Event {
                client_id: None,
                seq: self.seq,
                class,
                observation,
            },
            cost,
        );
    }

    fn make_room(&mut self, cost: usize, capacity: usize, may_discard_transcript: bool) -> bool {
        let limit = self.byte_limit();
        if cost > limit {
            return false;
        }
        while !self.queue.fits(cost, capacity, limit) {
            let oldest = self.queue.iter().position(|item| {
                matches!(
                    item,
                    Pending::Event {
                        class: ObservationClass::Event(Delivery::Droppable),
                        ..
                    }
                )
            });
            if let Some(index) = oldest {
                if let Some(Pending::Event {
                    observation: Payload::Other(lost),
                    ..
                }) = self.queue.remove(index)
                {
                    self.note_loss(&lost);
                    if is_delta(&lost)
                        && let Some(meter) = &mut self.meter
                    {
                        meter.evict_delta();
                    }
                }
            } else if let Some(scope) = may_discard_transcript
                .then(|| self.queue.iter().find_map(queued_scope).cloned())
                .flatten()
            {
                // Keep counted answers and ordered boundaries even if
                // their admission requires dropping a transcript replay.
                self.discard_transcript(&scope);
                self.miss_patch(&scope);
            } else {
                return false;
            }
        }
        true
    }
}

fn payload_scope<P: ProtocolExtension>(payload: &Payload<P>) -> Option<&TranscriptScope> {
    match payload {
        Payload::Snapshot(snapshot) => Some(&snapshot.scope),
        Payload::Patch(patch) => Some(&patch.scope),
        Payload::Other(_) => None,
    }
}

fn queued_scope<P: ProtocolExtension>(item: &Pending<P>) -> Option<&TranscriptScope> {
    match item {
        Pending::Event {
            observation: Payload::Other(event),
            ..
        } => keyed_append_scope(event),
        Pending::Event { observation, .. } => payload_scope(observation),
        _ => None,
    }
}
