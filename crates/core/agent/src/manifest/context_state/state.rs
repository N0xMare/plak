use super::*;

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
    /// user/developer/system prefix, then exactly one native compaction
    /// segment, then optionally the source's protected current-round tail
    /// verbatim. Cache affinity still rotates.
    #[allow(
        clippy::too_many_lines,
        reason = "Native compaction constructs the retained-tail provenance and replacement context from one validated source checkpoint"
    )]
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
        let mut compactions = successor_segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| segment.kind() == HistorySegmentKind::Compaction);
        let (summary_index, summary) = compactions
            .next()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if compactions.next().is_some()
            || summary_segment.kind() != HistorySegmentKind::Compaction
            || summary.identity() != summary_segment.identity()
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let trailing = successor_segments
            .iter()
            .skip(summary_index + 1)
            .collect::<Vec<_>>();
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
        let retained_segment_count = u32::try_from(successor_segments.len() - 1)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let trailing_segment_count = u32::try_from(trailing.len())
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        let target_segment_count = u32::try_from(successor_segments.len())
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
                .segment_prefix_identity(summary_index)
                .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
            let trailing_identities = trailing
                .iter()
                .flat_map(|segment| segment.identity().bytes().to_vec())
                .collect::<Vec<_>>();
            Digest::sha256(
                DigestDomain::ContextPlan,
                3,
                &[
                    b"plakous-agent/native-compaction-retained-v2",
                    prefix.bytes(),
                    &trailing_segment_count.to_be_bytes(),
                    &trailing_identities,
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
            strategy_revision: NATIVE_COMPACTION_STRATEGY_REVISION,
            source_projection,
            selected_start: 0,
            selected_end,
            selected_group_count: selected_end,
            selected_segment_count: selected_end,
            selected_identity: selection_plan,
            retained_segment_count,
            trailing_segment_count,
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
        clippy::too_many_lines,
        reason = "Placeholder compaction binds selected tool results, retained history, and invalidation evidence in one context transition"
    )]
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
            trailing_segment_count: 0,
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

    pub(crate) fn successful_image_residency(
        &self,
        publication: &SemanticCompactionPublication,
        notice: &HistorySegment,
        successor: &CanonicalHistory,
        selection: ResidencySelectionAudit,
    ) -> Result<Self, RuntimeManifestError> {
        if selection.images == 0 {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        validate_digest(selection.identity, DigestDomain::ContextPlan)?;
        let mut context = self.successful_tool_result_placeholders(
            publication,
            notice,
            successor,
            selection.source_history,
            selection.source_projection,
            selection.start,
            selection.end,
            selection.images,
            selection.source_segments,
        )?;
        let provenance = context
            .compaction
            .as_mut()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        provenance.strategy = RuntimeCompactionStrategy::ResidencyPlaceholdersV1;
        provenance.selection_plan = selection.identity;
        provenance.selected_identity = selection.identity;
        provenance.selected_segment_count = selection.segments;
        provenance.identity = compaction_provenance_identity(provenance)?;
        provenance.validate()?;
        Self::from_parts(
            context.affinity,
            None,
            context.transition,
            context.compaction,
        )
    }

    /// Atomically describes a successful semantic-summary publication.
    ///
    /// The target history must contain exactly one leading compaction segment
    /// followed by the closed planner's retained source segments. Summary
    /// content stays only in that canonical segment.
    #[allow(
        clippy::too_many_lines,
        reason = "Semantic compaction checks the published summary against its source and target context identities before rotation"
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
            trailing_segment_count: 0,
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

    pub(super) fn branch_transition(
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

    pub(super) fn from_parts(
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

    pub(super) fn from_recorded(
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

    pub(super) fn validate_shape(&self) -> Result<(), RuntimeManifestError> {
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

    pub(in crate::manifest) fn validate_history(
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

    /// Returns the context-measure anchor recorded at this checkpoint.
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
