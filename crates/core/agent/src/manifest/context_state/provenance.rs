use super::*;

impl RuntimeContextTransitionKind {
    pub(super) const fn tag(self) -> u8 {
        match self {
            Self::OrdinaryFork => 1,
            Self::Rebind => 2,
            Self::SuccessfulCompaction => 5,
        }
    }

    pub(in crate::manifest) fn from_tag(tag: u8) -> Result<Self, RuntimeManifestError> {
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

impl RuntimeContextTransition {
    pub(super) fn new(
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

    pub(super) fn from_recorded(
        mut transition: Self,
        identity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        transition.validate()?;
        if runtime_context_transition_identity(&transition)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        transition.identity = identity;
        Ok(transition)
    }

    pub(super) fn validate(&self) -> Result<(), RuntimeManifestError> {
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

impl RuntimeCompactionStrategy {
    pub(in crate::manifest) const fn tag(self) -> u8 {
        match self {
            Self::SemanticSummaryPrefixV1 => 1,
            Self::NativeResponsesTriggerV1 => 2,
            Self::ToolResultPlaceholdersV1 => 3,
            Self::ReplayOmissionV1 => 4,
        }
    }
}

impl CompactionInvalidation {
    pub(super) fn new(
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

    pub(super) fn validate(&self) -> Result<(), RuntimeManifestError> {
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

impl CompactionProvenance {
    pub(super) fn from_recorded(
        mut provenance: Self,
        identity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        provenance.validate()?;
        if compaction_provenance_identity(&provenance)? != identity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        provenance.identity = identity;
        Ok(provenance)
    }

    pub(in crate::manifest) fn validate(&self) -> Result<(), RuntimeManifestError> {
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
            || self.trailing_segment_count > self.retained_segment_count
            || (self.strategy == RuntimeCompactionStrategy::NativeResponsesTriggerV1
                && self.strategy_revision != NATIVE_COMPACTION_STRATEGY_REVISION)
            || (self.strategy != RuntimeCompactionStrategy::NativeResponsesTriggerV1
                && self.trailing_segment_count != 0)
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        self.invalidation.validate()
    }

    pub(in crate::manifest) fn validate_history_prefix(
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
            RuntimeCompactionStrategy::NativeResponsesTriggerV1 => self
                .retained_segment_count
                .checked_sub(self.trailing_segment_count)
                .and_then(|index| usize::try_from(index).ok())
                .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?,
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

    /// Returns how many retained segments follow the summary segment: a native
    /// successor's protected current-round tail, otherwise zero.
    #[must_use]
    pub const fn trailing_segment_count(&self) -> u32 {
        self.trailing_segment_count
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
            .field("trailing_segment_count", &self.trailing_segment_count)
            .field("summary_segment", &self.summary_segment)
            .field("invalidation", &self.invalidation)
            .finish_non_exhaustive()
    }
}
