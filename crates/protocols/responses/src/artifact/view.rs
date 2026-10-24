//! Replay purposes, authorized replay and the artifact reuse an artifact admits.

use std::fmt;

use plakous_model::ReplayEnvelope;

use super::{ResponsesArtifact, ResponsesCacheFact, ResponsesContinuation};

/// Why an exact native replay item is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesReplayPurpose {
    /// An exact ordered provider-native output item.
    OutputItem,
    /// An exact provider-native output item unknown to the current projector.
    OpaqueOutputItem,
    /// Exact encrypted native reasoning required for lossless replay.
    EncryptedReasoning,
}

impl ResponsesReplayPurpose {
    pub(super) const fn is_output(self) -> bool {
        matches!(self, Self::OutputItem | Self::OpaqueOutputItem)
    }
}

/// One canonical item-level replay envelope authorized by an active v4 artifact.
///
/// The envelope remains owned by canonical history. This value carries only its
/// closed purpose and absolute coordinate, so the family artifact never stores
/// a second copy of the raw provider payload.
#[derive(Clone, Copy)]
pub struct ResponsesAuthorizedReplay<'a> {
    purpose: ResponsesReplayPurpose,
    canonical_position: usize,
    envelope: &'a ReplayEnvelope,
}

impl<'a> ResponsesAuthorizedReplay<'a> {
    pub(super) const fn new(
        purpose: ResponsesReplayPurpose,
        canonical_position: usize,
        envelope: &'a ReplayEnvelope,
    ) -> Self {
        Self {
            purpose,
            canonical_position,
            envelope,
        }
    }

    /// Returns the purpose selected by the artifact's exact replay contract.
    #[must_use]
    pub const fn purpose(self) -> ResponsesReplayPurpose {
        self.purpose
    }

    /// Returns the absolute coordinate in the current canonical projection.
    #[must_use]
    pub const fn canonical_position(self) -> usize {
        self.canonical_position
    }

    /// Borrows the canonical item-level envelope without copying its payload.
    #[must_use]
    pub const fn envelope(self) -> &'a ReplayEnvelope {
        self.envelope
    }
}

impl fmt::Debug for ResponsesAuthorizedReplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesAuthorizedReplay")
            .field("purpose", &self.purpose)
            .field("canonical_position", &self.canonical_position)
            .field("envelope", &self.envelope)
            .finish()
    }
}

/// The sole active reuse path selected after complete validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesArtifactReuseKind {
    /// Use the exact provider-stored continuation and suppress local output replay.
    ProviderContinuation,
    /// Replay exact local output items and emit no provider continuation.
    LocalReplay,
    /// Only a provider cache fact remains applicable.
    CacheOnly,
}

/// Active, time-checked reuse authority borrowed from one inert artifact.
///
/// This wrapper is the narrow family-owned seam a neutral runtime may retain
/// transiently. It exposes continuation and local replay mutually exclusively.
pub struct ResponsesArtifactReuse<'a> {
    artifact: &'a ResponsesArtifact,
    kind: ResponsesArtifactReuseKind,
    replay: Vec<ResponsesAuthorizedReplay<'a>>,
}

impl<'a> ResponsesArtifactReuse<'a> {
    pub(super) const fn new(
        artifact: &'a ResponsesArtifact,
        kind: ResponsesArtifactReuseKind,
        replay: Vec<ResponsesAuthorizedReplay<'a>>,
    ) -> Self {
        Self {
            artifact,
            kind,
            replay,
        }
    }
}

impl ResponsesArtifactReuse<'_> {
    /// Returns the one admitted reuse path.
    #[must_use]
    pub const fn kind(&self) -> ResponsesArtifactReuseKind {
        self.kind
    }

    /// Returns a continuation only for the provider-stored reuse path.
    #[must_use]
    pub fn continuation(&self) -> Option<&ResponsesContinuation> {
        matches!(self.kind, ResponsesArtifactReuseKind::ProviderContinuation)
            .then(|| self.artifact.continuation.as_ref())
            .flatten()
    }

    /// Returns local replay only for the stateless reuse path.
    #[must_use]
    pub fn replay(&self) -> &[ResponsesAuthorizedReplay<'_>] {
        if matches!(self.kind, ResponsesArtifactReuseKind::LocalReplay) {
            &self.replay
        } else {
            &[]
        }
    }

    /// Looks up replay by full purpose and absolute canonical coordinate.
    ///
    /// Envelope digest is deliberately not a uniqueness key: byte-identical
    /// envelopes may occur at distinct exact coordinates or response groups.
    #[must_use]
    pub fn replay_at(
        &self,
        purpose: ResponsesReplayPurpose,
        canonical_position: usize,
    ) -> Option<ResponsesAuthorizedReplay<'_>> {
        if !matches!(self.kind, ResponsesArtifactReuseKind::LocalReplay) {
            return None;
        }
        self.replay
            .iter()
            .copied()
            .find(|item| item.purpose == purpose && item.canonical_position == canonical_position)
    }

    /// Returns the exact cache fact after its canonical coverage was checked.
    #[must_use]
    pub const fn cache(&self) -> Option<&ResponsesCacheFact> {
        self.artifact.cache.as_ref()
    }
}

impl fmt::Debug for ResponsesArtifactReuse<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesArtifactReuse")
            .field("kind", &self.kind)
            .field("binding", &self.artifact.binding.identity)
            .finish()
    }
}
