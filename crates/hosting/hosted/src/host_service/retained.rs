//! The attach cache is a complete, bounded replay from one root baseline.

use std::io::{self, Write};
use std::sync::Arc;

use plakous_host_protocol::views::{PresentationSnapshot, transcript::TranscriptPatch};

use super::{
    Face, ObservationClass, ProtocolExtension, SessionObservation, Snapshot, TranscriptScope,
    same_kind,
};

const MAX_PATCHES: usize = 64;
// Keep small incremental replays, but spend at most half a wire line on the
// whole chain rather than sending many large deltas instead of a fresh view.
const MAX_PATCH_BYTES: usize = 8 * 1024 * 1024;

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

    pub(super) fn received_by(&self, face: Face) -> bool {
        match self {
            Self::Snapshot(_) | Self::Patch(_) => true,
            Self::Other(other) => face.receives(other),
        }
    }
}

pub(super) struct RetainedBaseline {
    snapshot: Arc<PresentationSnapshot>,
    patches: Vec<Arc<TranscriptPatch>>,
    remaining: usize,
}

impl RetainedBaseline {
    pub(super) fn observations<P: ProtocolExtension>(&self) -> impl Iterator<Item = Payload<P>> {
        std::iter::once(Payload::Snapshot(Arc::clone(&self.snapshot)))
            .chain(self.patches.iter().cloned().map(Payload::Patch))
    }

    pub(super) fn patch_count(&self) -> usize {
        self.patches.len()
    }

    fn keep_patch(&mut self, patch: &Arc<TranscriptPatch>) -> bool {
        let mut budget = Budget(self.remaining);
        if self.patches.len() >= MAX_PATCHES
            || serde_json::to_writer(&mut budget, patch.as_ref()).is_err()
        {
            return false;
        }
        self.remaining = budget.0;
        self.patches.push(Arc::clone(patch));
        true
    }
}

// Charge the encoded patch (including deletes and metadata), without
// allocating an encoding or scanning bytes past the available budget.
struct Budget(usize);

impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("retained patch budget exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<P: ProtocolExtension> Snapshot<P> {
    pub(super) fn remember(&mut self, payload: &Payload<P>, class: ObservationClass) {
        let observation = match payload {
            Payload::Snapshot(baseline) => {
                if baseline.scope == TranscriptScope::Root {
                    self.baseline = Some(RetainedBaseline {
                        snapshot: Arc::clone(baseline),
                        patches: Vec::new(),
                        remaining: MAX_PATCH_BYTES,
                    });
                }
                return;
            }
            Payload::Patch(patch) => {
                if patch.scope == TranscriptScope::Root
                    && let Some(baseline) = &mut self.baseline
                    && !baseline.keep_patch(patch)
                {
                    self.baseline = None;
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
            self.approvals.clear();
            self.credential_wait = None;
        }
        match observation {
            SessionObservation::ApprovalRequired { approval_id, .. } => {
                self.approvals.retain(|(id, _)| id != approval_id);
                self.approvals
                    .push((approval_id.clone(), observation.clone()));
            }
            SessionObservation::ApprovalResolved { approval_id, .. } => {
                self.approvals.retain(|(id, _)| id != approval_id);
            }
            SessionObservation::CredentialWait { account } => {
                self.credential_wait = account.is_some().then(|| observation.clone());
            }
            SessionObservation::ClearView => self.baseline = None,
            _ if class == ObservationClass::State => {
                match self
                    .states
                    .iter_mut()
                    .find(|held| same_kind(held, observation))
                {
                    Some(held) => *held = observation.clone(),
                    None => self.states.push(observation.clone()),
                }
            }
            _ => {}
        }
    }
}
