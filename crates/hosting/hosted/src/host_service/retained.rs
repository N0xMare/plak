//! The attach cache is a complete, bounded replay from one root baseline.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Arc;

use plakous_host_protocol::views::{PresentationSnapshot, transcript::TranscriptPatch};
use plakous_host_protocol::wire::{AttachmentId, NodeAddress};
use uuid::Uuid;

use super::queue::MAX_QUEUE_BYTES;
use super::{
    Face, ObservationClass, ProtocolExtension, SessionObservation, Snapshot, TranscriptScope,
    attachment::keyed_append_scope, same_kind,
};

const MAX_PATCHES: usize = 64;
// Keep small incremental replays, but spend at most half a wire line on the
// whole chain rather than sending many large deltas instead of a fresh view.
pub(super) const MAX_PATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_VIEWPORTS: usize = 16;

/// A point the client holds in a process-local viewport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewportRevision {
    /// The serving process's generation, compared for equality only.
    pub host_generation: u64,
    /// An exact baseline or patch boundary.
    pub revision: u64,
    /// Opaque identity of the viewport opened by the first page.
    pub token: Uuid,
}

/// Publication fence sampled before reading history, without retaining a lock.
pub struct ViewportRead {
    pub(super) node: NodeAddress,
    pub(super) generation: u64,
    epoch: Uuid,
}

/// An admitted viewport and its immutable ordered prefix. Patches are numbered
/// from 1, before anything the new attachment can take from its live queue.
pub struct ViewportStart {
    /// The newly admitted attachment.
    pub attachment: AttachmentId,
    /// The starting revision of the prefix, not its endpoint.
    pub revision: ViewportRevision,
    /// The gap-free chain from that revision to the frozen live endpoint.
    /// Shares up to 8 MiB of encoded patch payload with the journal; a writer
    /// that materializes owned frames may copy that much per attach.
    pub patches: Vec<Arc<TranscriptPatch>>,
}

#[derive(Clone, Copy)]
pub(super) enum ViewportBase<'a> {
    Replay(&'a ViewportRevision),
    FirstPage(u64),
}

struct Viewport {
    first: ViewportRevision,
    // Opaque line-head and prune fence from the product's read snapshot.
    line: Arc<str>,
}

#[derive(Default)]
pub(super) struct Viewports {
    epoch: Option<Uuid>,
    tip: Option<u64>,
    points: VecDeque<Viewport>,
    patches: VecDeque<(Arc<TranscriptPatch>, usize)>,
    bytes: usize,
}

impl Viewports {
    pub(super) fn read(
        &mut self,
        node: NodeAddress,
        generation: u64,
        revision: Option<u64>,
    ) -> ViewportRead {
        let epoch = *self.epoch.get_or_insert_with(|| {
            self.tip = revision;
            Uuid::new_v4()
        });
        ViewportRead {
            node,
            generation,
            epoch,
        }
    }

    pub(super) fn reset(&mut self, tip: Option<u64>) {
        if self.epoch.is_some() {
            *self = Self {
                epoch: Some(Uuid::new_v4()),
                tip,
                ..Self::default()
            };
        }
    }

    fn observe<P: ProtocolExtension>(&mut self, payload: &Payload<P>, bytes: usize) {
        if self.epoch.is_none() {
            return;
        }
        match payload {
            Payload::Snapshot(snapshot)
                if snapshot.scope == TranscriptScope::Root
                    && self.tip != Some(snapshot.revision) =>
            {
                self.reset(Some(snapshot.revision));
            }
            Payload::Patch(patch) if patch.scope == TranscriptScope::Root => {
                if self.tip != Some(patch.from_revision)
                    || patch.to_revision <= patch.from_revision
                    || bytes > MAX_PATCH_BYTES
                {
                    self.reset(Some(patch.to_revision));
                    return;
                }
                self.tip = Some(patch.to_revision);
                self.bytes += bytes;
                self.patches.push_back((Arc::clone(patch), bytes));
                while self.patches.len() > MAX_PATCHES || self.bytes > MAX_PATCH_BYTES {
                    if let Some((_, bytes)) = self.patches.pop_front() {
                        self.bytes -= bytes;
                    }
                }
            }
            Payload::Other(
                SessionObservation::SessionOpening
                | SessionObservation::SessionOpened { .. }
                | SessionObservation::ClearView,
            ) => self.reset(None),
            _ => {}
        }
    }

    pub(super) fn contains_read(&self, read: &ViewportRead) -> bool {
        self.epoch == Some(read.epoch)
    }

    pub(super) fn plan(
        &self,
        read: &ViewportRead,
        line: &str,
        base: ViewportBase<'_>,
    ) -> Option<(ViewportRevision, Vec<Arc<TranscriptPatch>>)> {
        if !self.contains_read(read) {
            return None;
        }
        let tip = self.tip?;
        let start = match base {
            ViewportBase::Replay(start) => {
                if start.host_generation != read.generation
                    || !self.points.iter().any(|point| {
                        point.first.token == start.token
                            && point.first.host_generation == start.host_generation
                            && point.first.revision <= start.revision
                            && *point.line == *line
                    })
                {
                    return None;
                }
                start.clone()
            }
            ViewportBase::FirstPage(revision) => ViewportRevision {
                host_generation: read.generation,
                revision,
                token: Uuid::new_v4(),
            },
        };
        let mut at = start.revision;
        let mut patches = Vec::new();
        if at != tip {
            let first = self
                .patches
                .iter()
                .position(|(patch, _)| patch.from_revision == at)?;
            for (patch, _) in self.patches.iter().skip(first) {
                if patch.from_revision != at {
                    return None;
                }
                at = patch.to_revision;
                patches.push(Arc::clone(patch));
            }
        }
        (at == tip).then_some((start, patches))
    }

    pub(super) fn remember(&mut self, first: ViewportRevision, line: Arc<str>) {
        if self.points.len() == MAX_VIEWPORTS {
            self.points.pop_front();
        }
        self.points.push_back(Viewport { first, line });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "ordinary events keep their original owned payload"
)]
pub(super) enum Payload<P: ProtocolExtension> {
    Snapshot(Arc<PresentationSnapshot>),
    Patch(Arc<TranscriptPatch>),
    Other(SessionObservation<P>),
}

impl<P: ProtocolExtension> From<SessionObservation<P>> for Payload<P> {
    fn from(observation: SessionObservation<P>) -> Self {
        match observation {
            SessionObservation::PresentationSnapshot(snapshot) => {
                Self::Snapshot(Arc::new(snapshot))
            }
            SessionObservation::TranscriptPatch(patch) => Self::Patch(Arc::new(patch)),
            other => Self::Other(other),
        }
    }
}

impl<P: ProtocolExtension> Payload<P> {
    pub(super) fn into_observation(self) -> SessionObservation<P> {
        match self {
            Self::Snapshot(snapshot) => {
                SessionObservation::PresentationSnapshot(Arc::unwrap_or_clone(snapshot))
            }
            Self::Patch(patch) => SessionObservation::TranscriptPatch(Arc::unwrap_or_clone(patch)),
            Self::Other(other) => other,
        }
    }

    pub(super) fn class(&self) -> ObservationClass {
        match self {
            Self::Snapshot(_) | Self::Patch(_) => ObservationClass::Addressed,
            Self::Other(other) => other.class(),
        }
    }

    pub(super) fn is_root_preview(&self) -> bool {
        match self {
            Self::Other(
                SessionObservation::AssistantDelta(_) | SessionObservation::ReasoningDelta { .. },
            ) => true,
            Self::Other(event) => keyed_append_scope(event) == Some(&TranscriptScope::Root),
            _ => false,
        }
    }

    pub(super) fn received_by(&self, face: Face) -> bool {
        match self {
            Self::Snapshot(_) | Self::Patch(_) => true,
            Self::Other(other) => face.receives(other),
        }
    }

    pub(super) fn measure(observation: &SessionObservation<P>) -> usize {
        match observation {
            SessionObservation::TranscriptPatch(patch) => encoded_size(patch),
            SessionObservation::PresentationSnapshot(snapshot) => encoded_size(snapshot),
            other => encoded_size(other),
        }
    }
}

pub(super) struct RetainedBaseline {
    snapshot: (Arc<PresentationSnapshot>, usize),
    patches: Vec<(Arc<TranscriptPatch>, usize)>,
    remaining: usize,
}

impl RetainedBaseline {
    pub(super) fn observations<P: ProtocolExtension>(
        &self,
    ) -> impl Iterator<Item = (Payload<P>, usize)> {
        std::iter::once((
            Payload::Snapshot(Arc::clone(&self.snapshot.0)),
            self.snapshot.1,
        ))
        .chain(
            self.patches
                .iter()
                .cloned()
                .map(|(patch, size)| (Payload::Patch(patch), size)),
        )
    }

    pub(super) fn patch_count(&self) -> usize {
        self.patches.len()
    }

    fn keep_patch(&mut self, patch: &Arc<TranscriptPatch>, size: usize) -> bool {
        let revision = self
            .patches
            .last()
            .map_or(self.snapshot.0.revision, |(last, _)| last.to_revision);
        if self.patches.len() >= MAX_PATCHES
            || size > self.remaining
            || patch.from_revision != revision
        {
            return false;
        }
        self.remaining -= size;
        self.patches.push((Arc::clone(patch), size));
        true
    }
}

fn encoded_size(value: &impl serde::Serialize) -> usize {
    let mut budget = Budget(MAX_QUEUE_BYTES);
    if serde_json::to_writer(&mut budget, value).is_ok() {
        MAX_QUEUE_BYTES - budget.0
    } else {
        MAX_QUEUE_BYTES + 1
    }
}

// Charge the JSON without allocating an encoded frame.
struct Budget(usize);

impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("attachment budget exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<P: ProtocolExtension> Snapshot<P> {
    pub(super) fn remember(&mut self, payload: &Payload<P>, class: ObservationClass, size: usize) {
        self.viewports.observe(payload, size);
        self.root_preview_pending |= payload.is_root_preview();
        let observation = match payload {
            Payload::Snapshot(baseline) => {
                if baseline.scope == TranscriptScope::Root {
                    self.root_preview_pending = false;
                    self.baseline = Some(RetainedBaseline {
                        snapshot: (Arc::clone(baseline), size),
                        patches: Vec::new(),
                        remaining: MAX_PATCH_BYTES,
                    });
                }
                return;
            }
            Payload::Patch(patch) => {
                if patch.scope == TranscriptScope::Root {
                    self.root_preview_pending = false;
                    if let Some(baseline) = &mut self.baseline
                        && !baseline.keep_patch(patch, size)
                    {
                        self.baseline = None;
                    }
                }
                return;
            }
            Payload::Other(observation) => observation,
        };
        if matches!(
            observation,
            SessionObservation::SessionOpening | SessionObservation::SessionOpened { .. }
        ) {
            self.states.clear();
            self.baseline = None;
            self.root_preview_pending = false;
            self.approvals.clear();
            self.credential_wait = None;
        }
        match observation {
            SessionObservation::ApprovalRequired { approval_id, .. } => {
                self.approvals.retain(|(id, ..)| id != approval_id);
                self.approvals
                    .push((approval_id.clone(), observation.clone(), size));
            }
            SessionObservation::ApprovalResolved { approval_id, .. } => {
                self.approvals.retain(|(id, ..)| id != approval_id);
            }
            SessionObservation::CredentialWait { account } => {
                self.credential_wait = account.is_some().then(|| (observation.clone(), size));
            }
            SessionObservation::ClearView => {
                self.baseline = None;
                self.root_preview_pending = false;
            }
            _ if class == ObservationClass::State => {
                match self
                    .states
                    .iter_mut()
                    .find(|(held, _)| same_kind(held, observation))
                {
                    Some(held) => *held = (observation.clone(), size),
                    None => self.states.push((observation.clone(), size)),
                }
            }
            _ => {}
        }
    }
}
