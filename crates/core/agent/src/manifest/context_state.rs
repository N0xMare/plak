//! Typed runtime context state: transitions, compaction provenance, and their
//! canonical identities and encodings.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the manifest module that shares its vocabulary"
)]

use super::*;

/// Closed runtime-owned context transition represented by the typed context state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeContextTransitionKind {
    /// A safe ordinary branch fork with unchanged binding semantics.
    OrdinaryFork,
    /// A safe explicit rebind onto a newly validated target binding.
    Rebind,
    /// One successfully published semantic compaction on the same branch.
    SuccessfulCompaction,
}

impl RuntimeContextTransitionKind {
    const fn tag(self) -> u8 {
        match self {
            Self::OrdinaryFork => 1,
            Self::Rebind => 2,
            Self::SuccessfulCompaction => 5,
        }
    }

    pub(super) fn from_tag(tag: u8) -> Result<Self, RuntimeManifestError> {
        match tag {
            1 => Ok(Self::OrdinaryFork),
            2 => Ok(Self::Rebind),
            5 => Ok(Self::SuccessfulCompaction),
            _ => Err(RuntimeManifestError::UnknownTag {
                field: "runtime context transition",
                tag,
            }),
        }
    }
}

/// Exact source and target proof for the last context-rotation fence.
#[derive(Clone, Eq, PartialEq)]
pub struct RuntimeContextTransition {
    kind: RuntimeContextTransitionKind,
    source_session: SessionId,
    source_branch: BranchId,
    source_checkpoint: CheckpointId,
    source_context: Digest,
    source_affinity: Digest,
    source_epoch: u64,
    target_session: SessionId,
    target_branch: BranchId,
    target_affinity: Digest,
    target_epoch: u64,
    identity: Digest,
}

impl RuntimeContextTransition {
    fn new(
        kind: RuntimeContextTransitionKind,
        source: &RuntimeContextState,
        source_checkpoint: CheckpointId,
        target: &ModelCacheAffinity,
    ) -> Result<Self, RuntimeManifestError> {
        let mut transition = Self {
            kind,
            source_session: source.affinity.session(),
            source_branch: source.affinity.branch(),
            source_checkpoint,
            source_context: source.identity,
            source_affinity: source.affinity.identity(),
            source_epoch: source.affinity.epoch(),
            target_session: target.session(),
            target_branch: target.branch(),
            target_affinity: target.identity(),
            target_epoch: target.epoch(),
            identity: source.identity,
        };
        transition.identity = runtime_context_transition_identity(&transition)?;
        transition.validate()?;
        Ok(transition)
    }

    fn from_recorded(mut transition: Self, identity: Digest) -> Result<Self, RuntimeManifestError> {
        transition.validate()?;
        if runtime_context_transition_identity(&transition)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        transition.identity = identity;
        Ok(transition)
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        validate_digest(self.source_context, DigestDomain::ContextBudget)?;
        validate_digest(self.source_affinity, DigestDomain::CacheAffinity)?;
        validate_digest(self.target_affinity, DigestDomain::CacheAffinity)?;
        match self.kind {
            RuntimeContextTransitionKind::OrdinaryFork | RuntimeContextTransitionKind::Rebind => {
                if self.source_affinity == self.target_affinity
                    || self.target_epoch != 0
                    || (self.source_session == self.target_session
                        && self.source_branch == self.target_branch)
                {
                    return Err(RuntimeManifestError::InvalidContextTransition);
                }
            }
            RuntimeContextTransitionKind::SuccessfulCompaction => {
                if self.source_session != self.target_session
                    || self.source_branch != self.target_branch
                    || if self.source_affinity == self.target_affinity {
                        self.target_epoch != self.source_epoch
                    } else {
                        self.source_epoch.checked_add(1) != Some(self.target_epoch)
                    }
                {
                    return Err(RuntimeManifestError::InvalidContextTransition);
                }
            }
        }
        Ok(())
    }

    /// Returns the closed transition kind.
    #[must_use]
    pub const fn kind(&self) -> RuntimeContextTransitionKind {
        self.kind
    }

    /// Returns the exact source session.
    #[must_use]
    pub const fn source_session(&self) -> SessionId {
        self.source_session
    }

    /// Returns the exact source branch.
    #[must_use]
    pub const fn source_branch(&self) -> BranchId {
        self.source_branch
    }

    /// Returns the exact source checkpoint.
    #[must_use]
    pub const fn source_checkpoint(&self) -> CheckpointId {
        self.source_checkpoint
    }

    /// Returns the exact source context-state identity.
    #[must_use]
    pub const fn source_context_identity(&self) -> Digest {
        self.source_context
    }

    /// Returns the source affinity identity without exposing opaque entropy.
    #[must_use]
    pub const fn source_affinity_identity(&self) -> Digest {
        self.source_affinity
    }

    /// Returns the source branch cache epoch.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.source_epoch
    }

    /// Returns the target session.
    #[must_use]
    pub const fn target_session(&self) -> SessionId {
        self.target_session
    }

    /// Returns the target branch.
    #[must_use]
    pub const fn target_branch(&self) -> BranchId {
        self.target_branch
    }

    /// Returns the target affinity identity without exposing opaque entropy.
    #[must_use]
    pub const fn target_affinity_identity(&self) -> Digest {
        self.target_affinity
    }

    /// Returns the target branch cache epoch.
    #[must_use]
    pub const fn target_epoch(&self) -> u64 {
        self.target_epoch
    }

    /// Returns the recomputed transition identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl fmt::Debug for RuntimeContextTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeContextTransition")
            .field("kind", &self.kind)
            .field("source_session", &self.source_session)
            .field("source_branch", &self.source_branch)
            .field("source_checkpoint", &self.source_checkpoint)
            .field("source_epoch", &self.source_epoch)
            .field("target_session", &self.target_session)
            .field("target_branch", &self.target_branch)
            .field("target_epoch", &self.target_epoch)
            .field("affinity", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Non-authoritative audit facts for one fully evaluated projection.
///
/// These identities never reconstruct or authorize a [`plakous_model::ContextStatus`].
/// They are retained only inside compaction provenance together with the exact
/// observed semantic input-token count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextEvaluationAudit {
    measurement: Digest,
    status: Digest,
    input_tokens: u64,
}

impl ContextEvaluationAudit {
    /// Captures bounded audit facts from one complete prepared projection.
    pub fn new(
        measurement: Digest,
        status: Digest,
        input_tokens: u64,
    ) -> Result<Self, RuntimeManifestError> {
        validate_digest(measurement, DigestDomain::TokenMeasurement)?;
        validate_digest(status, DigestDomain::ContextAdmission)?;
        Ok(Self {
            measurement,
            status,
            input_tokens,
        })
    }

    /// Returns the measurement identity as audit provenance only.
    #[must_use]
    pub const fn measurement_identity(self) -> Digest {
        self.measurement
    }

    /// Returns the checked-status identity as audit provenance only.
    #[must_use]
    pub const fn status_identity(self) -> Digest {
        self.status
    }

    /// Returns the complete semantic input-token count observed at that fence.
    #[must_use]
    pub const fn input_tokens(self) -> u64 {
        self.input_tokens
    }
}

/// Closed compaction strategies constructible by this release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCompactionStrategy {
    /// Driver-owned semantic summary of a closed whole-group prefix.
    SemanticSummaryPrefixV1,
    /// Codex OAuth remote compact: `compaction_trigger` on `/responses`.
    NativeResponsesTriggerV1,
    /// Offline oldest-first placeholders of tool-result bodies in the protected tail.
    ///
    /// Every call-and-result group stays whole and paired; only the result
    /// body is replaced by a bounded placeholder naming the tool, the call id
    /// and the original size. The successor appends exactly one compaction
    /// segment recording the placeholder pass, so the transformation is committed
    /// history, never silent deletion.
    ToolResultPlaceholdersV1,
    /// Offline omission of unbound replay, retaining all portable tool results.
    ReplayOmissionV1,
}

impl RuntimeCompactionStrategy {
    pub(super) const fn tag(self) -> u8 {
        match self {
            Self::SemanticSummaryPrefixV1 => 1,
            Self::NativeResponsesTriggerV1 => 2,
            Self::ToolResultPlaceholdersV1 => 3,
            Self::ReplayOmissionV1 => 4,
        }
    }
}

/// Exact invalidation and epoch-rotation proof published with compaction.
#[derive(Clone, Eq, PartialEq)]
pub struct CompactionInvalidation {
    pub(super) source_affinity: Digest,
    pub(super) source_epoch: u64,
    pub(super) target_affinity: Digest,
    pub(super) target_epoch: u64,
}

impl CompactionInvalidation {
    fn new(
        source: &ModelCacheAffinity,
        target: &ModelCacheAffinity,
    ) -> Result<Self, RuntimeManifestError> {
        let invalidation = Self {
            source_affinity: source.identity(),
            source_epoch: source.epoch(),
            target_affinity: target.identity(),
            target_epoch: target.epoch(),
        };
        invalidation.validate()?;
        Ok(invalidation)
    }

    fn validate(&self) -> Result<(), RuntimeManifestError> {
        validate_digest(self.source_affinity, DigestDomain::CacheAffinity)?;
        validate_digest(self.target_affinity, DigestDomain::CacheAffinity)?;
        let preserved =
            self.source_affinity == self.target_affinity && self.source_epoch == self.target_epoch;
        let rotated = self.source_affinity != self.target_affinity
            && self.source_epoch.checked_add(1) == Some(self.target_epoch);
        if !preserved && !rotated {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    /// Returns the exact retired epoch.
    #[must_use]
    pub const fn source_epoch(&self) -> u64 {
        self.source_epoch
    }

    /// Returns the exact published epoch.
    #[must_use]
    pub const fn target_epoch(&self) -> u64 {
        self.target_epoch
    }

    /// Reports mandatory capacity-observation invalidation.
    #[must_use]
    pub const fn clears_capacity_observations(&self) -> bool {
        true
    }

    /// Reports mandatory continuation-anchor invalidation.
    #[must_use]
    pub const fn clears_continuation_anchor(&self) -> bool {
        true
    }
}

impl fmt::Debug for CompactionInvalidation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionInvalidation")
            .field("source_affinity", &"<redacted>")
            .field("source_epoch", &self.source_epoch)
            .field("target_affinity", &"<redacted>")
            .field("target_epoch", &self.target_epoch)
            .finish_non_exhaustive()
    }
}

/// Validated driver inputs not already derivable from canonical histories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCompactionPublication {
    source_checkpoint: CheckpointId,
    operation: Digest,
    route_binding: Digest,
    model_binding: Digest,
    harness_plan: Digest,
    before: ContextEvaluationAudit,
    after: ContextEvaluationAudit,
    usage: Option<Usage>,
}

impl SemanticCompactionPublication {
    /// Captures exact non-content publication facts for semantic compaction.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact compact publication authority tuple"
    )]
    pub fn new(
        source_checkpoint: CheckpointId,
        operation: Digest,
        route_binding: Digest,
        model_binding: Digest,
        harness_plan: Digest,
        before: ContextEvaluationAudit,
        after: ContextEvaluationAudit,
        usage: Option<Usage>,
    ) -> Result<Self, RuntimeManifestError> {
        validate_digest(operation, DigestDomain::ProviderOperation)?;
        validate_digest(route_binding, DigestDomain::ModelBinding)?;
        validate_digest(model_binding, DigestDomain::ModelBinding)?;
        validate_digest(harness_plan, DigestDomain::ResolvedHarnessPlan)?;
        Ok(Self {
            source_checkpoint,
            operation,
            route_binding,
            model_binding,
            harness_plan,
            before,
            after,
            usage,
        })
    }
}

/// Bounded reconstructive audit provenance for one committed compaction.
///
/// Summary content remains solely in the referenced canonical compaction
/// segment. Measurement/status identities are explicitly audit provenance and
/// never become reusable admission anchors.
#[derive(Clone, Eq, PartialEq)]
pub struct CompactionProvenance {
    source_checkpoint: CheckpointId,
    source_history: Digest,
    operation: Digest,
    route_binding: Digest,
    model_binding: Digest,
    harness_plan: Digest,
    selection_plan: Digest,
    pub(super) strategy: RuntimeCompactionStrategy,
    strategy_revision: u32,
    source_projection: Digest,
    selected_start: u32,
    selected_end: u32,
    pub(super) selected_group_count: u32,
    selected_segment_count: u32,
    selected_identity: Digest,
    retained_segment_count: u32,
    retained_tail_identity: Digest,
    target_history: Digest,
    target_segment_count: u32,
    target_projection: Digest,
    before: ContextEvaluationAudit,
    after: ContextEvaluationAudit,
    summary_segment: Digest,
    usage: Option<Usage>,
    pub(super) invalidation: CompactionInvalidation,
    pub(super) identity: Digest,
}

impl CompactionProvenance {
    fn from_recorded(mut provenance: Self, identity: Digest) -> Result<Self, RuntimeManifestError> {
        provenance.validate()?;
        if compaction_provenance_identity(&provenance)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        provenance.identity = identity;
        Ok(provenance)
    }

    pub(super) fn validate(&self) -> Result<(), RuntimeManifestError> {
        for (digest, domain) in [
            (self.source_history, DigestDomain::CanonicalHistory),
            (self.operation, DigestDomain::ProviderOperation),
            (self.route_binding, DigestDomain::ModelBinding),
            (self.model_binding, DigestDomain::ModelBinding),
            (self.harness_plan, DigestDomain::ResolvedHarnessPlan),
            (self.selection_plan, DigestDomain::ContextPlan),
            (self.source_projection, DigestDomain::HistoryProjection),
            (self.selected_identity, DigestDomain::ContextPlan),
            (self.retained_tail_identity, DigestDomain::ContextPlan),
            (self.target_history, DigestDomain::CanonicalHistory),
            (self.target_projection, DigestDomain::HistoryProjection),
            (self.summary_segment, DigestDomain::CanonicalHistorySegment),
        ] {
            validate_digest(digest, domain)?;
        }
        if self.strategy_revision == 0
            || (!matches!(
                self.strategy,
                RuntimeCompactionStrategy::ToolResultPlaceholdersV1
                    | RuntimeCompactionStrategy::ReplayOmissionV1
            ) && self.invalidation.source_affinity == self.invalidation.target_affinity)
            || self.selected_group_count == 0
            || (self.strategy == RuntimeCompactionStrategy::ReplayOmissionV1
                && self.selected_group_count != 1)
            || self.selected_segment_count == 0
            || self.selected_start >= self.selected_end
            || self.selected_end.saturating_sub(self.selected_start) < self.selected_segment_count
            || self.target_segment_count != self.retained_segment_count.saturating_add(1)
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        self.invalidation.validate()
    }

    fn validate_history_prefix(
        &self,
        history: &CanonicalHistory,
    ) -> Result<(), RuntimeManifestError> {
        let segments = history.segments();
        let target_len = usize::try_from(self.target_segment_count)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        if segments.len() < target_len {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let summary_index = match self.strategy {
            RuntimeCompactionStrategy::NativeResponsesTriggerV1 => {
                let index = usize::try_from(self.retained_segment_count)
                    .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
                if index.saturating_add(1) != target_len {
                    return Err(RuntimeManifestError::InvalidCompactionProvenance);
                }
                index
            }
            RuntimeCompactionStrategy::SemanticSummaryPrefixV1 => 0,
            // A placeholder pass merges into whichever compaction segment the source
            // already had, so its position is whatever the published prefix
            // says — provided there is exactly one.
            RuntimeCompactionStrategy::ToolResultPlaceholdersV1
            | RuntimeCompactionStrategy::ReplayOmissionV1 => {
                let mut found = None;
                for (index, segment) in segments.iter().take(target_len).enumerate() {
                    if segment.kind() != HistorySegmentKind::Compaction {
                        continue;
                    }
                    if found.is_some() {
                        return Err(RuntimeManifestError::InvalidCompactionProvenance);
                    }
                    found = Some(index);
                }
                found.ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            }
        };
        let summary = segments
            .get(summary_index)
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if summary.kind() != HistorySegmentKind::Compaction
            || summary.identity() != self.summary_segment
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let prefix = history
            .segment_prefix_identity(target_len)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        if prefix != self.target_history {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    /// Returns the exact source checkpoint.
    #[must_use]
    pub const fn source_checkpoint(&self) -> CheckpointId {
        self.source_checkpoint
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub const fn source_history_identity(&self) -> Digest {
        self.source_history
    }

    /// Returns the durable operation identity.
    #[must_use]
    pub const fn operation_identity(&self) -> Digest {
        self.operation
    }

    /// Returns which closed strategy published this compaction.
    ///
    /// A consumer that has to tell a fold from a placeholder pass reads this rather
    /// than inferring it from the successor's shape: both put their summary
    /// segment at position zero, so the shape alone does not discriminate.
    #[must_use]
    pub const fn strategy(&self) -> RuntimeCompactionStrategy {
        self.strategy
    }

    /// Returns the exact selected source range and complete-segment count.
    #[must_use]
    pub const fn selected_range(&self) -> (u32, u32, u32) {
        (
            self.selected_start,
            self.selected_end,
            self.selected_segment_count,
        )
    }

    /// Returns the selection root binding every selected complete group.
    #[must_use]
    pub const fn selected_identity(&self) -> Digest {
        self.selected_identity
    }

    /// Returns the retained-tail root and complete-segment count.
    #[must_use]
    pub const fn retained_tail(&self) -> (Digest, u32) {
        (self.retained_tail_identity, self.retained_segment_count)
    }

    /// Returns the canonical summary segment identity without duplicating content.
    #[must_use]
    pub const fn summary_segment_identity(&self) -> Digest {
        self.summary_segment
    }

    /// Returns provider usage when it was available.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns the exact independently measured pre-compaction audit facts.
    #[must_use]
    pub const fn before_audit(&self) -> ContextEvaluationAudit {
        self.before
    }

    /// Returns the exact independently measured post-compaction audit facts.
    #[must_use]
    pub const fn after_audit(&self) -> ContextEvaluationAudit {
        self.after
    }

    /// Returns exact cache/anchor invalidation evidence.
    #[must_use]
    pub const fn invalidation(&self) -> &CompactionInvalidation {
        &self.invalidation
    }

    /// Returns the recomputed provenance identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl fmt::Debug for CompactionProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionProvenance")
            .field("source_checkpoint", &self.source_checkpoint)
            .field("source_history", &self.source_history)
            .field("operation", &self.operation)
            .field("strategy", &self.strategy)
            .field("selected_segment_count", &self.selected_segment_count)
            .field("retained_segment_count", &self.retained_segment_count)
            .field("summary_segment", &self.summary_segment)
            .field("invalidation", &self.invalidation)
            .finish_non_exhaustive()
    }
}

/// One compact typed, bounded, versioned runtime context authority.
#[derive(Clone, Eq, PartialEq)]
pub struct RuntimeContextState {
    affinity: ModelCacheAffinity,
    /// The driver's context-measure anchor as of the last turn terminal
    /// (spec 01, 10.3). Every transition and compaction successor starts
    /// without one.
    occupancy_anchor: Option<ContextAnchor>,
    request_anchor: Option<plakous_model::ContextRequestAnchor>,
    transition: Option<RuntimeContextTransition>,
    compaction: Option<CompactionProvenance>,
    pub(super) identity: Digest,
}

impl RuntimeContextState {
    /// Constructs branch genesis from runtime-supplied 256-bit entropy.
    pub fn genesis(
        session: SessionId,
        branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        let affinity = ModelCacheAffinity::from_runtime_entropy(session, branch, 0, entropy)
            .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        Self::from_parts(affinity, None, None, None)
    }

    /// Retains the exact state for retry or verified quiescent resume.
    #[must_use]
    pub fn retained_for_exact_resume(&self) -> Self {
        self.clone()
    }

    /// Records the driver's context-measure anchor, replacing any earlier one.
    pub fn with_occupancy_anchor(
        &self,
        occupancy_anchor: Option<ContextAnchor>,
    ) -> Result<Self, RuntimeManifestError> {
        Self::from_parts(
            self.affinity.clone(),
            occupancy_anchor,
            self.transition.clone(),
            self.compaction.clone(),
        )?
        .with_request_anchor(self.request_anchor.clone())
    }

    /// Records item-delta admission authority from a completed response.
    pub fn with_request_anchor(
        &self,
        request_anchor: Option<plakous_model::ContextRequestAnchor>,
    ) -> Result<Self, RuntimeManifestError> {
        let mut state = self.clone();
        state.request_anchor = request_anchor;
        state.identity = runtime_context_state_identity(&state)?;
        Ok(state)
    }

    /// Returns completed-response authority for exact-binding admission.
    #[must_use]
    pub const fn request_anchor(&self) -> Option<&plakous_model::ContextRequestAnchor> {
        self.request_anchor.as_ref()
    }

    /// Creates a fresh target-branch affinity and exact ordinary-fork proof.
    pub fn ordinary_fork(
        &self,
        source_checkpoint: CheckpointId,
        target_session: SessionId,
        target_branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        self.branch_transition(
            RuntimeContextTransitionKind::OrdinaryFork,
            source_checkpoint,
            target_session,
            target_branch,
            entropy,
        )
    }

    /// Creates a fresh target-branch affinity and exact rebind proof.
    ///
    /// No capacity observation, measurement anchor, or continuation anchor is
    /// carried: this revision deliberately has no reusable observation fields.
    pub fn rebind(
        &self,
        source_checkpoint: CheckpointId,
        target_session: SessionId,
        target_branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        self.branch_transition(
            RuntimeContextTransitionKind::Rebind,
            source_checkpoint,
            target_session,
            target_branch,
            entropy,
        )
    }

    /// Atomically describes a Codex remote-compaction publication.
    ///
    /// The successor keeps a newest-first 64k-token retained
    /// user/developer/system tail and appends exactly one native
    /// compaction segment last. Cache affinity still rotates.
    #[allow(
        clippy::too_many_arguments,
        reason = "native publication binds source, summary, successor, and affinity entropy together"
    )]
    pub fn successful_native_compaction(
        &self,
        publication: &SemanticCompactionPublication,
        summary_segment: &HistorySegment,
        successor_history: &CanonicalHistory,
        source_history: Digest,
        source_projection: Digest,
        entropy: [u8; 32],
        source_segment_count: usize,
    ) -> Result<Self, RuntimeManifestError> {
        let successor_segments = successor_history.segments();
        let retained_segment_count = successor_segments.len().saturating_sub(1);
        let summary = if successor_segments.is_empty() {
            None
        } else {
            successor_segments.get(successor_segments.len() - 1)
        }
        .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if summary_segment.kind() != HistorySegmentKind::Compaction
            || summary.kind() != HistorySegmentKind::Compaction
            || summary.identity() != summary_segment.identity()
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let selected_end = u32::try_from(source_segment_count.max(1))
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let next_epoch = self
            .affinity
            .epoch()
            .checked_add(1)
            .ok_or(RuntimeManifestError::InvalidContextTransition)?;
        let affinity = ModelCacheAffinity::from_runtime_entropy(
            self.affinity.session(),
            self.affinity.branch(),
            next_epoch,
            entropy,
        )
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        let transition = RuntimeContextTransition::new(
            RuntimeContextTransitionKind::SuccessfulCompaction,
            self,
            publication.source_checkpoint,
            &affinity,
        )?;
        let invalidation = CompactionInvalidation::new(&self.affinity, &affinity)?;
        let target_projection =
            ConversationView::from_shared(successor_history.clone(), WorkingProjection::default());
        let retained_segment_count = u32::try_from(retained_segment_count)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let target_segment_count = u32::try_from(successor_history.segments().len())
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let retained_tail_identity = if retained_segment_count == 0 {
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[b"plakous-agent/native-compaction-empty-tail-v1"],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        } else {
            let prefix = successor_history
                .segment_prefix_identity(retained_segment_count as usize)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[
                    b"plakous-agent/native-compaction-retained-prefix-v1",
                    prefix.bytes(),
                ],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        };
        let selection_plan = Digest::sha256(
            DigestDomain::ContextPlan,
            2,
            &[
                b"plakous-agent/native-compaction-selection-v1",
                source_history.bytes(),
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)?;
        let mut provenance = CompactionProvenance {
            source_checkpoint: publication.source_checkpoint,
            source_history,
            operation: publication.operation,
            route_binding: publication.route_binding,
            model_binding: publication.model_binding,
            harness_plan: publication.harness_plan,
            selection_plan,
            strategy: RuntimeCompactionStrategy::NativeResponsesTriggerV1,
            strategy_revision: 1,
            source_projection,
            selected_start: 0,
            selected_end,
            selected_group_count: selected_end,
            selected_segment_count: selected_end,
            selected_identity: selection_plan,
            retained_segment_count,
            retained_tail_identity,
            target_history: successor_history.identity(),
            target_segment_count,
            target_projection: target_projection.identity(),
            before: publication.before,
            after: publication.after,
            summary_segment: summary_segment.identity(),
            usage: publication.usage,
            invalidation,
            identity: self.identity,
        };
        provenance.identity = compaction_provenance_identity(&provenance)?;
        provenance.validate()?;
        Self::from_parts(affinity, None, Some(transition), Some(provenance))
    }

    /// Atomically describes a successful offline tool-result placeholders.
    ///
    /// The successor keeps every source segment in order with replaced
    /// tool-result bodies replaced by bounded placeholders, and appends
    /// exactly one compaction segment recording the placeholder pass. No provider
    /// operation is involved, so `publication` carries no usage; the before
    /// and after audits are still independently measured. Cache affinity and
    /// its epoch are preserved; stale anchors are cleared.
    #[allow(
        clippy::too_many_arguments,
        reason = "placeholders publication binds source, notice, successor and selection together"
    )]
    pub fn successful_tool_result_placeholders(
        &self,
        publication: &SemanticCompactionPublication,
        notice_segment: &HistorySegment,
        successor_history: &CanonicalHistory,
        source_history: Digest,
        source_projection: Digest,
        selected_start: u32,
        selected_end: u32,
        replaced_result_count: u32,
        source_segment_count: u32,
    ) -> Result<Self, RuntimeManifestError> {
        let successor_segments = successor_history.segments();
        let retained_segment_count = successor_segments.len().saturating_sub(1);
        // The notice is the history's sole compaction segment: merged into an
        // existing summary in place, or created at position zero. A second one
        // would make the whole history unplannable.
        let mut notice_position = None;
        for (index, segment) in successor_segments.iter().enumerate() {
            if segment.kind() != HistorySegmentKind::Compaction {
                continue;
            }
            if notice_position.is_some() {
                return Err(RuntimeManifestError::InvalidCompactionProvenance);
            }
            notice_position = Some(index);
        }
        let notice = notice_position
            .and_then(|index| successor_segments.get(index))
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if notice_segment.kind() != HistorySegmentKind::Compaction
            || notice.identity() != notice_segment.identity()
            || selected_start >= selected_end
            || selected_end > source_segment_count
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let retained_segment_count = u32::try_from(retained_segment_count)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let affinity = self.affinity.clone();
        let transition = RuntimeContextTransition::new(
            RuntimeContextTransitionKind::SuccessfulCompaction,
            self,
            publication.source_checkpoint,
            &affinity,
        )?;
        let invalidation = CompactionInvalidation::new(&self.affinity, &affinity)?;
        let target_projection =
            ConversationView::from_shared(successor_history.clone(), WorkingProjection::default());
        let target_segment_count = u32::try_from(successor_segments.len())
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let retained_tail_identity = if retained_segment_count == 0 {
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[b"plakous-agent/tool-result-placeholders-empty-tail-v1"],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        } else {
            let prefix = successor_history
                .segment_prefix_identity(retained_segment_count as usize)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
            Digest::sha256(
                DigestDomain::ContextPlan,
                2,
                &[
                    b"plakous-agent/tool-result-placeholders-retained-prefix-v1",
                    prefix.bytes(),
                ],
            )
            .map_err(|_| RuntimeManifestError::InvalidDigest)?
        };
        let selection_plan = Digest::sha256(
            DigestDomain::ContextPlan,
            2,
            &[
                b"plakous-agent/tool-result-placeholders-selection-v1",
                source_history.bytes(),
                &selected_start.to_be_bytes(),
                &selected_end.to_be_bytes(),
                &replaced_result_count.to_be_bytes(),
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)?;
        let mut provenance = CompactionProvenance {
            source_checkpoint: publication.source_checkpoint,
            source_history,
            operation: publication.operation,
            route_binding: publication.route_binding,
            model_binding: publication.model_binding,
            harness_plan: publication.harness_plan,
            selection_plan,
            strategy: if replaced_result_count == 0 {
                RuntimeCompactionStrategy::ReplayOmissionV1
            } else {
                RuntimeCompactionStrategy::ToolResultPlaceholdersV1
            },
            strategy_revision: 1,
            source_projection,
            selected_start,
            selected_end,
            selected_group_count: replaced_result_count.max(1),
            selected_segment_count: selected_end.saturating_sub(selected_start),
            selected_identity: selection_plan,
            retained_segment_count,
            retained_tail_identity,
            target_history: successor_history.identity(),
            target_segment_count,
            target_projection: target_projection.identity(),
            before: publication.before,
            after: publication.after,
            summary_segment: notice_segment.identity(),
            usage: publication.usage,
            invalidation,
            identity: self.identity,
        };
        provenance.identity = compaction_provenance_identity(&provenance)?;
        provenance.validate()?;
        Self::from_parts(affinity, None, Some(transition), Some(provenance))
    }

    /// Atomically describes a successful semantic-summary publication.
    ///
    /// The target history must contain exactly one leading compaction segment
    /// followed by the closed planner's retained source segments. Summary
    /// content stays only in that canonical segment.
    #[allow(
        clippy::too_many_arguments,
        reason = "the histories and summary are independently reconstructive publication inputs"
    )]
    pub fn successful_semantic_compaction(
        &self,
        publication: &SemanticCompactionPublication,
        cut: &SemanticCompactionCut,
        summary_segment: &HistorySegment,
        successor_history: &CanonicalHistory,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        if summary_segment.kind() != HistorySegmentKind::Compaction
            || successor_history
                .segments()
                .get(0)
                .map(HistorySegment::identity)
                != Some(summary_segment.identity())
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let sealed_successor = cut
            .build_successor(summary_segment)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        if sealed_successor.identity() != successor_history.identity()
            || sealed_successor
                .segments()
                .iter()
                .map(HistorySegment::identity)
                .ne(successor_history
                    .segments()
                    .iter()
                    .map(HistorySegment::identity))
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let selected = cut.selected_segment_evidence();
        let selected_start = selected
            .first()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            .position();
        let selected_end = selected
            .last()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            .position()
            .checked_add(1)
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        let retained_segment_count = cut.retained_segment_count();
        if successor_history.segments().len() != retained_segment_count.saturating_add(1)
            || successor_history
                .segments()
                .iter()
                .skip(1)
                .map(HistorySegment::identity)
                .ne(cut.retained_segments().iter().map(HistorySegment::identity))
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let next_epoch = self
            .affinity
            .epoch()
            .checked_add(1)
            .ok_or(RuntimeManifestError::InvalidContextTransition)?;
        let affinity = ModelCacheAffinity::from_runtime_entropy(
            self.affinity.session(),
            self.affinity.branch(),
            next_epoch,
            entropy,
        )
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        let transition = RuntimeContextTransition::new(
            RuntimeContextTransitionKind::SuccessfulCompaction,
            self,
            publication.source_checkpoint,
            &affinity,
        )?;
        let invalidation = CompactionInvalidation::new(&self.affinity, &affinity)?;
        let target_projection =
            ConversationView::from_shared(successor_history.clone(), WorkingProjection::default());
        let mut provenance = CompactionProvenance {
            source_checkpoint: publication.source_checkpoint,
            source_history: cut.source_history_identity(),
            operation: publication.operation,
            route_binding: publication.route_binding,
            model_binding: publication.model_binding,
            harness_plan: publication.harness_plan,
            selection_plan: cut.plan_identity(),
            strategy: RuntimeCompactionStrategy::SemanticSummaryPrefixV1,
            strategy_revision: 1,
            source_projection: cut.source_projection_identity(),
            selected_start,
            selected_end,
            selected_group_count: u32::try_from(cut.selected_intent_groups().len())
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            selected_segment_count: u32::try_from(selected.len())
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            selected_identity: cut.selection_identity(),
            retained_segment_count: u32::try_from(retained_segment_count)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            retained_tail_identity: cut.retained_tail_identity(),
            target_history: successor_history.identity(),
            target_segment_count: u32::try_from(successor_history.segments().len())
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?,
            target_projection: target_projection.identity(),
            before: publication.before,
            after: publication.after,
            summary_segment: summary_segment.identity(),
            usage: publication.usage,
            invalidation,
            identity: self.identity,
        };
        provenance.identity = compaction_provenance_identity(&provenance)?;
        provenance.validate()?;
        Self::from_parts(affinity, None, Some(transition), Some(provenance))
    }

    fn branch_transition(
        &self,
        kind: RuntimeContextTransitionKind,
        source_checkpoint: CheckpointId,
        target_session: SessionId,
        target_branch: BranchId,
        entropy: [u8; 32],
    ) -> Result<Self, RuntimeManifestError> {
        if !matches!(
            kind,
            RuntimeContextTransitionKind::OrdinaryFork | RuntimeContextTransitionKind::Rebind
        ) {
            return Err(RuntimeManifestError::InvalidContextTransition);
        }
        let affinity =
            ModelCacheAffinity::from_runtime_entropy(target_session, target_branch, 0, entropy)
                .map_err(|_| RuntimeManifestError::InvalidContextState)?;
        let transition = RuntimeContextTransition::new(kind, self, source_checkpoint, &affinity)?;
        Self::from_parts(affinity, None, Some(transition), self.compaction.clone())
    }

    fn from_parts(
        affinity: ModelCacheAffinity,
        occupancy_anchor: Option<ContextAnchor>,
        transition: Option<RuntimeContextTransition>,
        compaction: Option<CompactionProvenance>,
    ) -> Result<Self, RuntimeManifestError> {
        let mut state = Self {
            affinity,
            occupancy_anchor,
            request_anchor: None,
            transition,
            compaction,
            identity: Digest::sha256(DigestDomain::ContextBudget, 1, &[b"initial"])
                .map_err(|_| RuntimeManifestError::InvalidDigest)?,
        };
        state.validate_shape()?;
        state.identity = runtime_context_state_identity(&state)?;
        Ok(state)
    }

    fn from_recorded(
        affinity: ModelCacheAffinity,
        occupancy_anchor: Option<ContextAnchor>,
        request_anchor: Option<plakous_model::ContextRequestAnchor>,
        transition: Option<RuntimeContextTransition>,
        compaction: Option<CompactionProvenance>,
        identity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        let state = Self {
            affinity,
            occupancy_anchor,
            request_anchor,
            transition,
            compaction,
            identity,
        };
        state.validate_shape()?;
        if runtime_context_state_identity(&state)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        Ok(state)
    }

    fn validate_shape(&self) -> Result<(), RuntimeManifestError> {
        validate_digest(self.affinity.identity(), DigestDomain::CacheAffinity)?;
        if let Some(transition) = &self.transition {
            transition.validate()?;
            if transition.target_session != self.affinity.session()
                || transition.target_branch != self.affinity.branch()
                || transition.target_epoch != self.affinity.epoch()
                || transition.target_affinity != self.affinity.identity()
            {
                return Err(RuntimeManifestError::InvalidContextTransition);
            }
        }
        if let Some(provenance) = &self.compaction {
            provenance.validate()?;
        }
        if self.transition.as_ref().is_some_and(|transition| {
            transition.kind == RuntimeContextTransitionKind::SuccessfulCompaction
                && self.compaction.as_ref().is_none_or(|provenance| {
                    provenance.source_checkpoint != transition.source_checkpoint
                        || provenance.invalidation.source_affinity != transition.source_affinity
                        || provenance.invalidation.target_affinity != transition.target_affinity
                        || provenance.invalidation.source_epoch != transition.source_epoch
                        || provenance.invalidation.target_epoch != transition.target_epoch
                })
        }) {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    pub(super) fn validate_history(
        &self,
        history: &CanonicalHistory,
    ) -> Result<(), RuntimeManifestError> {
        if let Some(provenance) = &self.compaction {
            provenance.validate_history_prefix(history)?;
        }
        Ok(())
    }

    /// Verifies exact binding to one manifest topology.
    pub fn validate_binding(
        &self,
        session: SessionId,
        branch: BranchId,
    ) -> Result<(), RuntimeManifestError> {
        self.validate_shape()?;
        if self.affinity.session() != session || self.affinity.branch() != branch {
            return Err(RuntimeManifestError::InvalidContextState);
        }
        Ok(())
    }

    /// Verifies a store-owned safe branch transition against the exact source.
    pub fn validate_branch_transition_from(
        &self,
        source: &Self,
        source_checkpoint: CheckpointId,
        expected: RuntimeContextTransitionKind,
    ) -> Result<(), RuntimeManifestError> {
        if !matches!(
            expected,
            RuntimeContextTransitionKind::OrdinaryFork | RuntimeContextTransitionKind::Rebind
        ) || self.compaction != source.compaction
            || self.occupancy_anchor.is_some()
            || self.request_anchor.is_some()
        {
            return Err(RuntimeManifestError::InvalidContextTransition);
        }
        let proof = self
            .transition
            .as_ref()
            .ok_or(RuntimeManifestError::InvalidContextTransition)?;
        if proof.kind != expected
            || proof.source_session != source.affinity.session()
            || proof.source_branch != source.affinity.branch()
            || proof.source_checkpoint != source_checkpoint
            || proof.source_context != source.identity
            || proof.source_affinity != source.affinity.identity()
            || proof.source_epoch != source.affinity.epoch()
            || proof.target_session != self.affinity.session()
            || proof.target_branch != self.affinity.branch()
            || proof.target_affinity != self.affinity.identity()
            || proof.target_epoch != self.affinity.epoch()
            || proof.source_affinity == proof.target_affinity
        {
            return Err(RuntimeManifestError::InvalidContextTransition);
        }
        Ok(())
    }

    /// Returns the sole bound cache affinity.
    #[must_use]
    pub const fn affinity(&self) -> &ModelCacheAffinity {
        &self.affinity
    }

    /// Returns the context-measure anchor recorded at the last turn terminal.
    #[must_use]
    pub const fn occupancy_anchor(&self) -> Option<ContextAnchor> {
        self.occupancy_anchor
    }

    /// Returns the last closed rotation proof, when a rotation has occurred.
    #[must_use]
    pub const fn transition(&self) -> Option<&RuntimeContextTransition> {
        self.transition.as_ref()
    }

    /// Returns the latest committed compaction provenance, when present.
    #[must_use]
    pub const fn compaction_provenance(&self) -> Option<&CompactionProvenance> {
        self.compaction.as_ref()
    }

    /// Returns the recomputed complete context-state identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Encodes this typed state with its bounded canonical codec.
    pub fn encode(&self) -> Result<Vec<u8>, RuntimeManifestError> {
        encode_runtime_context_state(self)
    }

    /// Decodes and fully revalidates this typed state.
    pub fn decode(bytes: &[u8]) -> Result<Self, RuntimeManifestError> {
        decode_runtime_context_state(bytes)
    }
}

impl fmt::Debug for RuntimeContextState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeContextState")
            .field("session", &self.affinity.session())
            .field("branch", &self.affinity.branch())
            .field("epoch", &self.affinity.epoch())
            .field("affinity", &"<redacted>")
            .field("occupancy_anchor", &self.occupancy_anchor)
            .field("transition", &self.transition)
            .field("compaction", &self.compaction)
            .finish_non_exhaustive()
    }
}

fn runtime_context_transition_identity(
    transition: &RuntimeContextTransition,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    write_runtime_context_transition_fields(&mut writer, transition)?;
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-agent/runtime-context-transition-v2",
            &writer.bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

pub(super) fn compaction_provenance_identity(
    provenance: &CompactionProvenance,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    write_compaction_provenance_fields(&mut writer, provenance)?;
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-agent/runtime-compaction-provenance-v2",
            &writer.bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn runtime_context_state_identity(
    state: &RuntimeContextState,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.id(state.affinity.session().as_uuid());
    writer.id(state.affinity.branch().as_uuid());
    writer.u64(state.affinity.epoch());
    writer.digest(state.affinity.identity())?;
    write_occupancy_anchor(&mut writer, state.occupancy_anchor);
    write_request_anchor(&mut writer, state.request_anchor.as_ref())?;
    writer.optional(state.transition.is_some());
    if let Some(transition) = &state.transition {
        writer.digest(transition.identity)?;
    }
    writer.optional(state.compaction.is_some());
    if let Some(compaction) = &state.compaction {
        writer.digest(compaction.identity)?;
    }
    Digest::sha256(
        DigestDomain::ContextBudget,
        3,
        &[b"plakous-agent/runtime-context-state-v3", &writer.bytes],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn write_request_anchor(
    writer: &mut Writer,
    anchor: Option<&plakous_model::ContextRequestAnchor>,
) -> Result<(), RuntimeManifestError> {
    writer.optional(anchor.is_some());
    if let Some(anchor) = anchor {
        let bytes =
            serde_json::to_vec(anchor).map_err(|_| RuntimeManifestError::InvalidContextState)?;
        writer.raw(&bytes, MAX_RUNTIME_CONTEXT_STATE_BYTES)?;
    }
    Ok(())
}

fn read_request_anchor(
    reader: &mut Reader<'_>,
) -> Result<Option<plakous_model::ContextRequestAnchor>, RuntimeManifestError> {
    if !reader.optional()? {
        return Ok(None);
    }
    let bytes = reader.raw(MAX_RUNTIME_CONTEXT_STATE_BYTES)?;
    let anchor: plakous_model::ContextRequestAnchor =
        serde_json::from_slice(&bytes).map_err(|_| RuntimeManifestError::InvalidContextState)?;
    if serde_json::to_vec(&anchor).map_err(|_| RuntimeManifestError::InvalidContextState)?
        != bytes.as_ref()
    {
        return Err(RuntimeManifestError::InvalidContextState);
    }
    Ok(Some(anchor))
}

fn write_occupancy_anchor(writer: &mut Writer, anchor: Option<ContextAnchor>) {
    writer.optional(anchor.is_some());
    if let Some(anchor) = anchor {
        writer.u64(anchor.provider_input_tokens());
        writer.u64(anchor.request_estimate_tokens());
    }
}

fn read_occupancy_anchor(
    reader: &mut Reader<'_>,
) -> Result<Option<ContextAnchor>, RuntimeManifestError> {
    if !reader.optional()? {
        return Ok(None);
    }
    let provider_input_tokens = reader.u64()?;
    let request_estimate_tokens = reader.u64()?;
    ContextAnchor::new(provider_input_tokens, request_estimate_tokens)
        .map(Some)
        .ok_or(RuntimeManifestError::InvalidContextState)
}

fn write_runtime_context_transition_fields(
    writer: &mut Writer,
    transition: &RuntimeContextTransition,
) -> Result<(), RuntimeManifestError> {
    writer.u8(transition.kind.tag());
    writer.id(transition.source_session.as_uuid());
    writer.id(transition.source_branch.as_uuid());
    writer.id(transition.source_checkpoint.as_uuid());
    writer.digest(transition.source_context)?;
    writer.digest(transition.source_affinity)?;
    writer.u64(transition.source_epoch);
    writer.id(transition.target_session.as_uuid());
    writer.id(transition.target_branch.as_uuid());
    writer.digest(transition.target_affinity)?;
    writer.u64(transition.target_epoch);
    Ok(())
}

fn write_context_evaluation_audit(
    writer: &mut Writer,
    audit: ContextEvaluationAudit,
) -> Result<(), RuntimeManifestError> {
    writer.digest(audit.measurement)?;
    writer.digest(audit.status)?;
    writer.u64(audit.input_tokens);
    Ok(())
}

pub(super) fn write_compaction_provenance_fields(
    writer: &mut Writer,
    provenance: &CompactionProvenance,
) -> Result<(), RuntimeManifestError> {
    writer.id(provenance.source_checkpoint.as_uuid());
    writer.digest(provenance.source_history)?;
    writer.digest(provenance.operation)?;
    writer.digest(provenance.route_binding)?;
    writer.digest(provenance.model_binding)?;
    writer.digest(provenance.harness_plan)?;
    writer.digest(provenance.selection_plan)?;
    writer.u8(provenance.strategy.tag());
    writer.u32(provenance.strategy_revision);
    writer.digest(provenance.source_projection)?;
    writer.u32(provenance.selected_start);
    writer.u32(provenance.selected_end);
    writer.u32(provenance.selected_group_count);
    writer.u32(provenance.selected_segment_count);
    writer.digest(provenance.selected_identity)?;
    writer.u32(provenance.retained_segment_count);
    writer.digest(provenance.retained_tail_identity)?;
    writer.digest(provenance.target_history)?;
    writer.u32(provenance.target_segment_count);
    writer.digest(provenance.target_projection)?;
    write_context_evaluation_audit(writer, provenance.before)?;
    write_context_evaluation_audit(writer, provenance.after)?;
    writer.digest(provenance.summary_segment)?;
    writer.optional(provenance.usage.is_some());
    if let Some(usage) = provenance.usage {
        writer.u64(usage.input_tokens());
        writer.u64(usage.output_tokens());
        writer.u64(usage.reasoning_tokens());
        writer.u64(usage.cached_input_tokens());
        writer.u64(usage.cache_write_tokens());
    }
    writer.digest(provenance.invalidation.source_affinity)?;
    writer.u64(provenance.invalidation.source_epoch);
    writer.digest(provenance.invalidation.target_affinity)?;
    writer.u64(provenance.invalidation.target_epoch);
    Ok(())
}

fn encode_runtime_context_state(
    state: &RuntimeContextState,
) -> Result<Vec<u8>, RuntimeManifestError> {
    state.validate_shape()?;
    let mut writer = Writer::default();
    let affinity = serde_json::to_vec(&state.affinity)
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
    writer.raw(&affinity, 4 * 1024)?;
    write_occupancy_anchor(&mut writer, state.occupancy_anchor);
    write_request_anchor(&mut writer, state.request_anchor.as_ref())?;
    writer.optional(state.transition.is_some());
    if let Some(transition) = &state.transition {
        write_runtime_context_transition_fields(&mut writer, transition)?;
        writer.digest(transition.identity)?;
    }
    writer.optional(state.compaction.is_some());
    if let Some(provenance) = &state.compaction {
        write_compaction_provenance_fields(&mut writer, provenance)?;
        writer.digest(provenance.identity)?;
    }
    writer.digest(state.identity)?;
    let integrity = root_integrity(b"runtime-context-state", &writer.bytes)?;
    encode_envelope(
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        &writer.bytes,
        integrity,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
    )
}

fn decode_runtime_context_state(bytes: &[u8]) -> Result<RuntimeContextState, RuntimeManifestError> {
    let body = decode_envelope(
        bytes,
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
        b"runtime-context-state",
    )?;
    let mut reader = Reader::new(body);
    let affinity_bytes = reader.raw(4 * 1024)?;
    let affinity: ModelCacheAffinity = serde_json::from_slice(&affinity_bytes)
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
    if serde_json::to_vec(&affinity).map_err(|_| RuntimeManifestError::InvalidContextState)?
        != affinity_bytes.as_ref()
    {
        return Err(RuntimeManifestError::InvalidContextState);
    }
    let occupancy_anchor = read_occupancy_anchor(&mut reader)?;
    let request_anchor = read_request_anchor(&mut reader)?;
    let transition = reader
        .optional()?
        .then(|| read_runtime_context_transition(&mut reader))
        .transpose()?;
    let compaction = reader
        .optional()?
        .then(|| read_compaction_provenance(&mut reader))
        .transpose()?;
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    reader.finish()?;
    RuntimeContextState::from_recorded(
        affinity,
        occupancy_anchor,
        request_anchor,
        transition,
        compaction,
        identity,
    )
}

fn read_runtime_context_transition(
    reader: &mut Reader<'_>,
) -> Result<RuntimeContextTransition, RuntimeManifestError> {
    let kind = RuntimeContextTransitionKind::from_tag(reader.u8()?)?;
    let source_session = reader.session_id()?;
    let source_branch = reader.branch_id()?;
    let source_checkpoint = reader.checkpoint_id()?;
    let source_context = reader.digest(Some(DigestDomain::ContextBudget))?;
    let source_affinity = reader.digest(Some(DigestDomain::CacheAffinity))?;
    let source_epoch = reader.u64()?;
    let target_session = reader.session_id()?;
    let target_branch = reader.branch_id()?;
    let target_affinity = reader.digest(Some(DigestDomain::CacheAffinity))?;
    let target_epoch = reader.u64()?;
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    RuntimeContextTransition::from_recorded(
        RuntimeContextTransition {
            kind,
            source_session,
            source_branch,
            source_checkpoint,
            source_context,
            source_affinity,
            source_epoch,
            target_session,
            target_branch,
            target_affinity,
            target_epoch,
            identity,
        },
        identity,
    )
}

fn read_context_evaluation_audit(
    reader: &mut Reader<'_>,
) -> Result<ContextEvaluationAudit, RuntimeManifestError> {
    ContextEvaluationAudit::new(
        reader.digest(Some(DigestDomain::TokenMeasurement))?,
        reader.digest(Some(DigestDomain::ContextAdmission))?,
        reader.u64()?,
    )
}

pub(super) fn read_compaction_provenance(
    reader: &mut Reader<'_>,
) -> Result<CompactionProvenance, RuntimeManifestError> {
    let source_checkpoint = reader.checkpoint_id()?;
    let source_history = reader.digest(Some(DigestDomain::CanonicalHistory))?;
    let operation = reader.digest(Some(DigestDomain::ProviderOperation))?;
    let route_binding = reader.digest(Some(DigestDomain::ModelBinding))?;
    let model_binding = reader.digest(Some(DigestDomain::ModelBinding))?;
    let harness_plan = reader.digest(Some(DigestDomain::ResolvedHarnessPlan))?;
    let selection_plan = reader.digest(Some(DigestDomain::ContextPlan))?;
    let strategy = match reader.u8()? {
        1 => RuntimeCompactionStrategy::SemanticSummaryPrefixV1,
        2 => RuntimeCompactionStrategy::NativeResponsesTriggerV1,
        3 => RuntimeCompactionStrategy::ToolResultPlaceholdersV1,
        4 => RuntimeCompactionStrategy::ReplayOmissionV1,
        tag => {
            return Err(RuntimeManifestError::UnknownTag {
                field: "runtime compaction strategy",
                tag,
            });
        }
    };
    let strategy_revision = reader.u32()?;
    let source_projection = reader.digest(Some(DigestDomain::HistoryProjection))?;
    let selected_start = reader.u32()?;
    let selected_end = reader.u32()?;
    let selected_group_count = reader.u32()?;
    let selected_segment_count = reader.u32()?;
    let selected_identity = reader.digest(Some(DigestDomain::ContextPlan))?;
    let retained_segment_count = reader.u32()?;
    let retained_tail_identity = reader.digest(Some(DigestDomain::ContextPlan))?;
    let target_history = reader.digest(Some(DigestDomain::CanonicalHistory))?;
    let target_segment_count = reader.u32()?;
    let target_projection = reader.digest(Some(DigestDomain::HistoryProjection))?;
    let before = read_context_evaluation_audit(reader)?;
    let after = read_context_evaluation_audit(reader)?;
    let summary_segment = reader.digest(Some(DigestDomain::CanonicalHistorySegment))?;
    let usage = reader
        .optional()?
        .then(|| {
            Ok::<_, RuntimeManifestError>(Usage::new(
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
            ))
        })
        .transpose()?;
    let invalidation = CompactionInvalidation {
        source_affinity: reader.digest(Some(DigestDomain::CacheAffinity))?,
        source_epoch: reader.u64()?,
        target_affinity: reader.digest(Some(DigestDomain::CacheAffinity))?,
        target_epoch: reader.u64()?,
    };
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    CompactionProvenance::from_recorded(
        CompactionProvenance {
            source_checkpoint,
            source_history,
            operation,
            route_binding,
            model_binding,
            harness_plan,
            selection_plan,
            strategy,
            strategy_revision,
            source_projection,
            selected_start,
            selected_end,
            selected_group_count,
            selected_segment_count,
            selected_identity,
            retained_segment_count,
            retained_tail_identity,
            target_history,
            target_segment_count,
            target_projection,
            before,
            after,
            summary_segment,
            usage,
            invalidation,
            identity,
        },
        identity,
    )
}
