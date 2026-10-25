//! Context history plans and the semantic compaction cut sealed from one.

use std::slice;
use std::sync::Arc;

use serde::Serialize;
use thiserror::Error;

use crate::{Actor, ContentBlock, Digest, DigestDomain, DigestError};

use super::{
    CONTEXT_PLANNER_POLICY_REVISION, CONTEXT_PLANNER_STRATEGY_REVISION, CanonicalHistory,
    CanonicalSegmentIter, ConversationView, EntryNode, HistoryError, HistorySegment,
    HistorySegmentKind, MAX_CONTEXT_PLANNER_SEGMENTS, SEMANTIC_COMPACTION_CUT_REVISION,
    WorkingProjection,
};

/// The sole whole-group history-selection strategy admitted by the driver policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ContextHistoryStrategy {
    /// Select a contiguous prefix of complete older intent groups for summary replacement.
    SemanticSummaryPrefixV1,
}

impl ContextHistoryStrategy {
    const fn tag(self) -> u8 {
        match self {
            Self::SemanticSummaryPrefixV1 => 1,
        }
    }
}

/// Exact evidence that one complete older intent and its causal tail were selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSelectedIntentGroup {
    start_position: u32,
    end_position: u32,
    identity: Digest,
}

impl ContextSelectedIntentGroup {
    /// Returns the inclusive user-intent segment position.
    #[must_use]
    pub const fn start_position(self) -> u32 {
        self.start_position
    }

    /// Returns the exclusive next-intent boundary.
    #[must_use]
    pub const fn end_position(self) -> u32 {
        self.end_position
    }

    /// Returns the versioned identity of every ordered segment in the group.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

/// Exact evidence for one selected complete canonical segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSelectedSegment {
    position: u32,
    identity: Digest,
}

impl ContextSelectedSegment {
    /// Returns the segment's canonical zero-based position.
    #[must_use]
    pub const fn position(self) -> u32 {
        self.position
    }

    /// Returns the exact versioned canonical segment identity.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

/// Exact evidence for the latest user intent (or steering-only anchor) and tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextRetainedTail {
    start_position: u32,
    segment_count: u32,
    identity: Digest,
}

impl ContextRetainedTail {
    /// Returns the zero-based position across committed then working groups.
    #[must_use]
    pub const fn start_position(self) -> u32 {
        self.start_position
    }

    /// Returns the complete retained causal group count.
    #[must_use]
    pub const fn segment_count(self) -> u32 {
        self.segment_count
    }

    /// Returns the versioned identity of the ordered causal tail.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

/// Closed overflow classification minted only from a complete history plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ContextPlannerDispositionKind {
    /// At least one complete committed group is eligible for transformation.
    EligibleWholeGroups,
    /// The selection reaches inside the active turn: complete older groups
    /// plus sealed committed segments between the latest intent and the
    /// protected current-round tail.
    EligibleActiveTurnPrefix,
    /// No complete committed group may be removed under the closed policy.
    ProtectedOnly,
}

/// Bounded sealed planner evidence consumed by context-overflow status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextPlannerDisposition {
    plan: Digest,
    source_history: Digest,
    source_projection: Digest,
    policy_revision: u32,
    strategy: ContextHistoryStrategy,
    strategy_revision: u32,
    kind: ContextPlannerDispositionKind,
    selected_group_count: u32,
    selected_segment_count: u32,
    retained_segment_count: u32,
    identity: Digest,
}

impl ContextPlannerDisposition {
    fn for_plan(plan: &ContextHistoryPlan) -> Result<Self, HistoryPlanningError> {
        let selected_group_count = u32::try_from(plan.selected_groups.len())
            .map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let selected_segment_count =
            u32::try_from(plan.selected.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let retained_segment_count = plan.retained_segment_count;
        let kind = if selected_group_count == 0 {
            ContextPlannerDispositionKind::ProtectedOnly
        } else if plan.latest_user_intent.is_some_and(|latest| {
            plan.selected
                .iter()
                .any(|selected| selected.position > latest)
        }) {
            ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        } else {
            ContextPlannerDispositionKind::EligibleWholeGroups
        };
        let identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-planner-disposition-v1",
                &plan.identity.identity_bytes(),
                &plan.source.history().identity().identity_bytes(),
                &plan.source.identity().identity_bytes(),
                &CONTEXT_PLANNER_POLICY_REVISION.to_be_bytes(),
                &[plan.strategy.tag()],
                &CONTEXT_PLANNER_STRATEGY_REVISION.to_be_bytes(),
                &[match kind {
                    ContextPlannerDispositionKind::EligibleWholeGroups => 1,
                    ContextPlannerDispositionKind::ProtectedOnly => 2,
                    ContextPlannerDispositionKind::EligibleActiveTurnPrefix => 3,
                }],
                &selected_group_count.to_be_bytes(),
                &selected_segment_count.to_be_bytes(),
                &retained_segment_count.to_be_bytes(),
            ],
        )?;
        Ok(Self {
            plan: plan.identity,
            source_history: plan.source.history().identity(),
            source_projection: plan.source.identity(),
            policy_revision: CONTEXT_PLANNER_POLICY_REVISION,
            strategy: plan.strategy,
            strategy_revision: CONTEXT_PLANNER_STRATEGY_REVISION,
            kind,
            selected_group_count,
            selected_segment_count,
            retained_segment_count,
            identity,
        })
    }

    /// Returns whether any complete whole group is eligible.
    #[must_use]
    pub const fn kind(&self) -> ContextPlannerDispositionKind {
        self.kind
    }

    /// Returns the complete history-plan identity.
    #[must_use]
    pub const fn plan_identity(&self) -> Digest {
        self.plan
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub const fn source_history_identity(&self) -> Digest {
        self.source_history
    }

    /// Returns the complete source projection identity.
    #[must_use]
    pub const fn source_projection_identity(&self) -> Digest {
        self.source_projection
    }

    /// Returns the closed protection-policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u32 {
        self.policy_revision
    }

    /// Returns the closed selection strategy.
    #[must_use]
    pub const fn strategy(&self) -> ContextHistoryStrategy {
        self.strategy
    }

    /// Returns the closed strategy revision.
    #[must_use]
    pub const fn strategy_revision(&self) -> u32 {
        self.strategy_revision
    }

    /// Returns the eligible complete intent-group count.
    #[must_use]
    pub const fn selected_group_count(&self) -> u32 {
        self.selected_group_count
    }

    /// Returns the complete selected segment count across eligible intent groups.
    #[must_use]
    pub const fn selected_segment_count(&self) -> u32 {
        self.selected_segment_count
    }

    /// Returns the retained complete-group count.
    #[must_use]
    pub const fn retained_segment_count(&self) -> u32 {
        self.retained_segment_count
    }

    /// Returns the sealed disposition identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// A fail-closed pure context-planning error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum HistoryPlanningError {
    /// Committed plus working semantic groups exceeded the fixed planner bound.
    #[error("context history exceeds the closed planner segment bound")]
    LimitExceeded,
    /// A protected semantic group was not complete at the named position.
    #[error("history group at position {position} is incomplete")]
    IncompleteGroup {
        /// Zero-based position across committed then working groups.
        position: u32,
    },
    /// A current model-visible projection retained more than one compaction
    /// output.
    #[error("current history has a noncanonical compaction layout")]
    NoncanonicalCompaction,
    /// A versioned plan identity could not be constructed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

/// A pure, bounded whole-group plan over one immutable conversation projection.
///
/// The stable request prefix is outside history and is always protected by
/// this policy. The plan contains identities and shared handles only; it never
/// estimates tokens, predicts savings, or claims that a summary successor fits.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextHistoryPlan {
    source: ConversationView,
    strategy: ContextHistoryStrategy,
    latest_user_intent: Option<u32>,
    selected_groups: Arc<[ContextSelectedIntentGroup]>,
    selected: Arc<[ContextSelectedSegment]>,
    retained_tail: Option<ContextRetainedTail>,
    retained_segment_count: u32,
    summary_tail_identity: Option<Digest>,
    disposition: ContextPlannerDisposition,
    identity: Digest,
}

struct ContextHistoryAnalysis {
    total: usize,
    canonical_count: u32,
    intent_positions: Vec<u32>,
    latest_user_intent: Option<u32>,
    latest_steering: Option<u32>,
    compaction_positions: Vec<u32>,
}

struct ContextSummarySelection {
    groups: Vec<ContextSelectedIntentGroup>,
    segments: Vec<ContextSelectedSegment>,
}

impl ContextHistoryPlan {
    /// Applies the sole driver-owned policy to committed and working groups.
    ///
    /// Working groups are always retained. Tool exchanges, reasoning/replay,
    /// and steering are invariant-bearing atomic segments: they may move only
    /// with their complete older intent group. A committed compaction output is
    /// a retained barrier. The latest semantic user intent and every later
    /// group form the retained causal tail.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an incomplete group, excessive group count,
    /// or identity construction failure.
    pub fn new(source: &ConversationView) -> Result<Self, HistoryPlanningError> {
        let analysis = analyze_context_history(source)?;
        let strategy = ContextHistoryStrategy::SemanticSummaryPrefixV1;
        let selection = select_context_summary_prefix(source, &analysis)?;
        let retained_tail = analysis
            .latest_user_intent
            .or(analysis.latest_steering)
            .map(|_| context_retained_tail(source, &selection.segments, analysis.total))
            .transpose()?;
        let retained_segment_count =
            u32::try_from(analysis.total.saturating_sub(selection.segments.len()))
                .map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let selected_groups: Arc<[ContextSelectedIntentGroup]> = selection.groups.into();
        let selected: Arc<[ContextSelectedSegment]> = selection.segments.into();
        let identity = context_history_plan_identity(
            source,
            strategy,
            &selected_groups,
            &selected,
            retained_tail.as_ref(),
        )?;
        let summary_tail_identity = (!selected.is_empty())
            .then(|| {
                Digest::sha256(
                    DigestDomain::ContextPlan,
                    1,
                    &[
                        b"plakous-model/context-summary-retained-tail-v1",
                        &identity.identity_bytes(),
                        &retained_segment_count.to_be_bytes(),
                        b"insert-summary-then-fresh-prepare-and-measurement-required",
                    ],
                )
            })
            .transpose()?;
        let mut plan = Self {
            source: source.clone(),
            strategy,
            latest_user_intent: analysis.latest_user_intent,
            selected_groups,
            selected,
            retained_tail,
            retained_segment_count,
            summary_tail_identity,
            disposition: ContextPlannerDisposition {
                plan: identity,
                source_history: source.history().identity(),
                source_projection: source.identity(),
                policy_revision: CONTEXT_PLANNER_POLICY_REVISION,
                strategy,
                strategy_revision: CONTEXT_PLANNER_STRATEGY_REVISION,
                kind: ContextPlannerDispositionKind::ProtectedOnly,
                selected_group_count: 0,
                selected_segment_count: 0,
                retained_segment_count,
                identity,
            },
            identity,
        };
        plan.disposition = ContextPlannerDisposition::for_plan(&plan)?;
        Ok(plan)
    }

    /// Returns the closed policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u32 {
        CONTEXT_PLANNER_POLICY_REVISION
    }

    /// Returns the sole closed strategy.
    #[must_use]
    pub const fn strategy(&self) -> ContextHistoryStrategy {
        self.strategy
    }

    /// Returns the latest semantic user-intent position, if one exists.
    #[must_use]
    pub const fn latest_user_intent_position(&self) -> Option<u32> {
        self.latest_user_intent
    }

    /// Returns exact ordered evidence for complete selected intent groups.
    #[must_use]
    pub fn selected_intent_groups(&self) -> &[ContextSelectedIntentGroup] {
        &self.selected_groups
    }

    /// Returns exact ordered evidence for every selected complete group.
    #[must_use]
    pub fn selected_segments(&self) -> &[ContextSelectedSegment] {
        &self.selected
    }

    /// Returns exact latest-intent/steering causal-tail evidence, when present.
    #[must_use]
    pub const fn retained_tail(&self) -> Option<&ContextRetainedTail> {
        self.retained_tail.as_ref()
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub fn source_history_identity(&self) -> Digest {
        self.source.history().identity()
    }

    /// Returns the complete source projection identity.
    #[must_use]
    pub const fn source_projection_identity(&self) -> Digest {
        self.source.identity()
    }

    /// Returns the sealed overflow disposition.
    #[must_use]
    pub const fn disposition(&self) -> &ContextPlannerDisposition {
        &self.disposition
    }

    /// Returns the complete plan identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the shared retained tail only when semantic-summary input exists.
    ///
    /// The tail is not a naked-deletion successor: a later driver must
    /// insert and commit the semantic summary before rebuilding a request.
    #[must_use]
    pub fn summary_replacement_tail(&self) -> Option<ContextSummaryRetainedTail<'_>> {
        self.summary_tail_identity
            .map(|identity| ContextSummaryRetainedTail {
                plan: self,
                identity,
            })
    }

    /// Proves that the plan retained the exact source-history allocation.
    #[must_use]
    pub fn shares_source_history_with(&self, history: &CanonicalHistory) -> bool {
        self.source.shares_history_with(history)
    }
}

/// A fail-closed error sealing or applying one semantic compaction cut.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SemanticCompactionCutError {
    /// Compaction was requested while model-visible working state existed.
    #[error("semantic compaction requires an empty working projection")]
    WorkingProjectionNotEmpty,
    /// The supplied source no longer matched the closed plan's exact source.
    #[error("semantic compaction source and history plan have drifted")]
    SourcePlanDrift,
    /// The plan did not select a complete replaceable source prefix.
    #[error("semantic compaction has no eligible selected prefix")]
    NoEligibleSelectedPrefix,
    /// Selected handles or evidence did not form the plan's complete groups.
    #[error("semantic compaction selected groups are invalid")]
    InvalidSelectedSemantics,
    /// No complete latest-intent causal tail would survive the cut.
    #[error("semantic compaction retained semantics cannot be empty")]
    EmptyRetainedSemantics,
    /// Retained handles did not match the plan's exact causal tail.
    #[error("semantic compaction retained semantics are invalid")]
    InvalidRetainedSemantics,
    /// The supplied successor summary was not one canonical non-empty summary.
    #[error("semantic compaction successor requires exactly one canonical compaction summary")]
    InvalidSummary,
    /// Canonical history construction failed.
    #[error(transparent)]
    History(#[from] HistoryError),
    /// Stable proof identity construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

/// One owned exact cut from a quiescent committed source history.
///
/// This proof carries only whole shared segment handles. It is not a token
/// measurement, a fit claim, a provider request, or context-state authority.
/// The closed [`ContextHistoryPlan`] supplies every selected boundary; callers
/// cannot author a cut or count.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticCompactionCut {
    source: ConversationView,
    plan: ContextHistoryPlan,
    summary_input: ConversationView,
    selected: Arc<[HistorySegment]>,
    retained: Arc<[HistorySegment]>,
    prior_compaction: Option<HistorySegment>,
    retained_tail_identity: Digest,
    identity: Digest,
}

impl SemanticCompactionCut {
    /// Seals a closed plan against its exact quiescent committed source.
    ///
    /// The sole recognized prior compaction is included in [`Self::summary_input`]
    /// so the new summary must fold its semantics, but it is excluded from the
    /// retained successor handles. All other selected and retained content is
    /// shared at complete semantic-group boundaries.
    ///
    /// # Errors
    ///
    /// Returns a typed error for working state, source or plan drift, an absent
    /// selected prefix, noncanonical selected/retained semantics, or identity
    /// construction failure.
    #[allow(
        clippy::too_many_lines,
        reason = "the single closed sealing sequence keeps every source, selection, retention, and identity fence auditable in order"
    )]
    pub fn seal(
        source: &ConversationView,
        plan: &ContextHistoryPlan,
    ) -> Result<Self, SemanticCompactionCutError> {
        if !source.working().segments().is_empty() {
            return Err(SemanticCompactionCutError::WorkingProjectionNotEmpty);
        }
        if source.history().identity() != plan.source_history_identity()
            || source.identity() != plan.source_projection_identity()
            || !plan.source.working().segments().is_empty()
        {
            return Err(SemanticCompactionCutError::SourcePlanDrift);
        }
        let evidence = plan.selected_segments();
        if !matches!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::EligibleWholeGroups
                | ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        ) || evidence.is_empty()
            || plan.selected_intent_groups().is_empty()
        {
            return Err(SemanticCompactionCutError::NoEligibleSelectedPrefix);
        }

        let source_segments = source.history().segments();
        let prior_position = prior_compaction_position(source);
        let prior_compaction = prior_position
            .and_then(|position| source_segments.get(position))
            .cloned();
        if source_segments
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .count()
            != usize::from(prior_position.is_some())
        {
            return Err(SemanticCompactionCutError::InvalidSelectedSemantics);
        }
        // Re-run the closed selection over this exact source: the sealed
        // evidence must equal it — positions, identities and group runs alike
        // — so a plan cannot widen, reorder, or re-aim the cut past the closed
        // eligibility policy (older whole groups plus the sealed middle of
        // the active turn; never intent, steering, compaction, or the
        // protected current-round tail).
        let analysis = analyze_context_history(source)
            .map_err(|_| SemanticCompactionCutError::SourcePlanDrift)?;
        let recomputed = select_context_summary_prefix(source, &analysis)
            .map_err(|_| SemanticCompactionCutError::SourcePlanDrift)?;
        if recomputed.groups.as_slice() != plan.selected_intent_groups()
            || recomputed.segments.as_slice() != evidence
            || plan.disposition().selected_group_count()
                != u32::try_from(recomputed.groups.len()).unwrap_or(u32::MAX)
            || plan.disposition().selected_segment_count()
                != u32::try_from(evidence.len()).unwrap_or(u32::MAX)
        {
            return Err(SemanticCompactionCutError::InvalidSelectedSemantics);
        }
        let selected_start = evidence.first().map_or(0, |selected| selected.position());
        let selected_end = evidence
            .last()
            .and_then(|selected| selected.position().checked_add(1))
            .ok_or(SemanticCompactionCutError::InvalidSelectedSemantics)?;
        if selected_start < u32::from(prior_compaction.is_some())
            || evidence
                .windows(2)
                .any(|pair| pair[1].position() <= pair[0].position())
        {
            return Err(SemanticCompactionCutError::NoEligibleSelectedPrefix);
        }
        let selected = evidence
            .iter()
            .map(|selected| {
                source_segments
                    .get(usize::try_from(selected.position()).unwrap_or(usize::MAX))
                    .filter(|segment| {
                        segment.identity() == selected.identity()
                            && segment.model_visible_complete()
                    })
                    .cloned()
                    .ok_or(SemanticCompactionCutError::NoEligibleSelectedPrefix)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Retained is the exact complement: every committed segment the cut
        // does not fold, minus the prior compaction whose
        // semantics the new summary must carry forward.
        let mut selected_iter = evidence.iter().peekable();
        let mut retained = Vec::new();
        for (position, segment) in source_segments.iter().enumerate() {
            if Some(position) == prior_position {
                continue;
            }
            if selected_iter.peek().is_some_and(|next| {
                usize::try_from(next.position()).unwrap_or(usize::MAX) == position
            }) {
                let _ = selected_iter.next();
                continue;
            }
            retained.push(segment.clone());
        }
        if retained.is_empty() {
            return Err(SemanticCompactionCutError::EmptyRetainedSemantics);
        }
        let retained_tail = plan
            .retained_tail()
            .ok_or(SemanticCompactionCutError::InvalidRetainedSemantics)?;
        let recomputed_tail = context_retained_tail(source, evidence, analysis.total)
            .map_err(|_| SemanticCompactionCutError::InvalidRetainedSemantics)?;
        let retained_count = u32::try_from(retained.len())
            .map_err(|_| SemanticCompactionCutError::InvalidRetainedSemantics)?;
        if retained_tail != &recomputed_tail
            || plan.disposition().retained_segment_count()
                != retained_count.saturating_add(u32::from(prior_compaction.is_some()))
            || retained.iter().any(|segment| {
                !segment.model_visible_complete()
                    || segment.kind() == HistorySegmentKind::Compaction
            })
            || selected_iter.next().is_some()
        {
            return Err(SemanticCompactionCutError::InvalidRetainedSemantics);
        }

        // The summarizer sees the folded prefix in source order. When the cut
        // reaches inside the active turn it also sees the latest intent: the
        // summary must preserve the user intent it is folding work for, and a
        // verbatim copy still leads the retained tail.
        let intra_turn = plan
            .latest_user_intent_position()
            .is_some_and(|latest| evidence.iter().any(|selected| selected.position() > latest));
        let mut input_handles: Vec<HistorySegment> = prior_compaction.iter().cloned().collect();
        if intra_turn {
            let latest = plan
                .latest_user_intent_position()
                .ok_or(SemanticCompactionCutError::InvalidSelectedSemantics)?;
            let split = evidence
                .iter()
                .position(|selected| selected.position() > latest)
                .unwrap_or(evidence.len());
            input_handles.extend_from_slice(&selected[..split]);
            input_handles.push(
                source_segments
                    .get(usize::try_from(latest).unwrap_or(usize::MAX))
                    .cloned()
                    .ok_or(SemanticCompactionCutError::InvalidSelectedSemantics)?,
            );
            input_handles.extend_from_slice(&selected[split..]);
        } else {
            input_handles.extend_from_slice(&selected);
        }
        let summary_history = CanonicalHistory::new(input_handles)?;
        let summary_input =
            ConversationView::from_shared(summary_history, WorkingProjection::default());
        let retained_tail_identity = retained_tail.identity();
        let identity = Digest::sha256(
            DigestDomain::ContextPlan,
            SEMANTIC_COMPACTION_CUT_REVISION,
            &[
                b"plakous-model/semantic-compaction-cut-v1",
                &source.history().identity().identity_bytes(),
                &source.identity().identity_bytes(),
                &plan.identity().identity_bytes(),
                &plan.disposition().identity().identity_bytes(),
                &plan.policy_revision().to_be_bytes(),
                &[plan.strategy().tag()],
                &plan.disposition().strategy_revision().to_be_bytes(),
                &summary_input.history().identity().identity_bytes(),
                &summary_input.identity().identity_bytes(),
                &prior_compaction
                    .as_ref()
                    .map(HistorySegment::identity)
                    .map_or_else(Vec::new, |digest| digest.identity_bytes()),
                &selected_start.to_be_bytes(),
                &selected_end.to_be_bytes(),
                &retained_tail_identity.identity_bytes(),
                &retained_count.to_be_bytes(),
            ],
        )?;
        Ok(Self {
            source: source.clone(),
            plan: plan.clone(),
            summary_input,
            selected: selected.into(),
            retained: retained.into(),
            prior_compaction,
            retained_tail_identity,
            identity,
        })
    }

    /// Returns the exact committed source projection.
    #[must_use]
    pub const fn source(&self) -> &ConversationView {
        &self.source
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub fn source_history_identity(&self) -> Digest {
        self.source.history().identity()
    }

    /// Returns the exact source model-visible projection identity.
    #[must_use]
    pub const fn source_projection_identity(&self) -> Digest {
        self.source.identity()
    }

    /// Returns the complete closed history-plan identity.
    #[must_use]
    pub const fn plan_identity(&self) -> Digest {
        self.plan.identity()
    }

    /// Returns the closed protection-policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u32 {
        self.plan.policy_revision()
    }

    /// Returns the closed whole-group selection strategy.
    #[must_use]
    pub const fn strategy(&self) -> ContextHistoryStrategy {
        self.plan.strategy()
    }

    /// Returns the closed whole-group strategy revision.
    #[must_use]
    pub const fn strategy_revision(&self) -> u32 {
        self.plan.disposition().strategy_revision()
    }

    /// Returns the exact planner-selected intent-group evidence.
    #[must_use]
    pub fn selected_intent_groups(&self) -> &[ContextSelectedIntentGroup] {
        self.plan.selected_intent_groups()
    }

    /// Returns exact planner-selected segment evidence.
    #[must_use]
    pub fn selected_segment_evidence(&self) -> &[ContextSelectedSegment] {
        self.plan.selected_segments()
    }

    /// Returns the exact shared handles selected for replacement.
    ///
    /// A prior compaction is not in this slice; it is exposed by
    /// [`Self::prior_compaction`] and included in [`Self::summary_input`].
    #[must_use]
    pub fn selected_segments(&self) -> &[HistorySegment] {
        &self.selected
    }

    /// Returns the planner disposition identity binding the full selection.
    #[must_use]
    pub const fn selection_identity(&self) -> Digest {
        self.plan.disposition().identity()
    }

    /// Returns the exact owned summary input and no retained causal tail.
    #[must_use]
    pub const fn summary_input(&self) -> &ConversationView {
        &self.summary_input
    }

    /// Returns the prior compaction that the new summary must fold, if any.
    #[must_use]
    pub fn prior_compaction(&self) -> Option<&HistorySegment> {
        self.prior_compaction.as_ref()
    }

    /// Returns exact shared handles retained after the sole new summary.
    #[must_use]
    pub fn retained_segments(&self) -> &[HistorySegment] {
        &self.retained
    }

    /// Returns the complete retained group count.
    #[must_use]
    pub fn retained_segment_count(&self) -> usize {
        self.retained.len()
    }

    /// Returns the planner's exact retained causal-tail identity.
    #[must_use]
    pub const fn retained_tail_identity(&self) -> Digest {
        self.retained_tail_identity
    }

    /// Returns the stable semantic identity of this complete cut.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Builds the sole canonical successor from one new semantic summary.
    ///
    /// The result contains the supplied summary at position zero followed by
    /// the sealed retained handles in source order. The old compaction, if
    /// any, cannot survive as a second compaction segment. Retained segment
    /// payloads are shared exactly; no item is flattened or copied.
    ///
    /// # Errors
    ///
    /// Returns a typed error unless `summary` is exactly one canonical
    /// non-empty compaction group with fresh entry identities, or if canonical
    /// successor construction fails.
    pub fn build_successor(
        &self,
        summary: &HistorySegment,
    ) -> Result<CanonicalHistory, SemanticCompactionCutError> {
        if !is_canonical_compaction_summary(summary)
            || summary.items().iter().any(|item| {
                EntryNode::contains(self.source.history().inner.entries.as_deref(), item.id())
            })
        {
            return Err(SemanticCompactionCutError::InvalidSummary);
        }
        let mut segments = Vec::with_capacity(self.retained.len().saturating_add(1));
        segments.push(summary.clone());
        segments.extend(self.retained.iter().cloned());
        let successor = CanonicalHistory::new(segments)?;
        if successor.segments().len() != self.retained.len().saturating_add(1)
            || successor
                .segments()
                .get(0)
                .is_none_or(|candidate| !candidate.shares_storage_with(summary))
            || successor
                .segments()
                .iter()
                .skip(1)
                .zip(self.retained.iter())
                .any(|(candidate, retained)| !candidate.shares_storage_with(retained))
            || successor
                .segments()
                .iter()
                .skip(1)
                .any(|segment| segment.kind() == HistorySegmentKind::Compaction)
        {
            return Err(SemanticCompactionCutError::InvalidRetainedSemantics);
        }
        Ok(successor)
    }
}

fn is_canonical_compaction_summary(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::Compaction
        && segment.model_visible_complete()
        && segment.items().len() == 1
        && segment.items()[0].actor() == Actor::Assistant
        && matches!(segment.items()[0].blocks(), [ContentBlock::Compaction(_)])
}

// Native successors place retained instructions before their sole bound item.
// Nothing else authorizes moving a nonleading summary into a semantic cut.
fn prior_compaction_position(source: &ConversationView) -> Option<usize> {
    let segments = source.history().segments();
    let mut positions = segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| segment.kind() == HistorySegmentKind::Compaction);
    let (position, segment) = positions.next()?;
    if positions.next().is_some() || !is_canonical_compaction_summary(segment) {
        return None;
    }
    if position == 0 {
        return Some(position);
    }
    let [ContentBlock::Compaction(block)] = segment.items()[0].blocks() else {
        return None;
    };
    (block.native_item().is_some()
        && block.native_binding().is_some()
        && segments.iter().take(position).all(|segment| {
            matches!(
                segment.kind(),
                HistorySegmentKind::RecentIntent
                    | HistorySegmentKind::Steering
                    | HistorySegmentKind::Conversation
            ) && segment.model_visible_complete()
                && segment.items().iter().all(|item| {
                    matches!(item.actor(), Actor::User | Actor::Developer | Actor::System)
                })
        }))
    .then_some(position)
}

/// Zero-copy retained groups awaiting a committed semantic-summary insertion.
///
/// This value is neither a valid compaction successor nor an admission/fit
/// witness. A driver must first insert and commit the summary which replaces
/// the selected intent groups, then the descriptor context path must freshly
/// prepare and measure the complete successor request. An existing
/// compaction is retained here as required semantic input, but a canonical
/// successor folds it into the sole newly committed compaction output.
#[derive(Clone, Copy, Debug)]
pub struct ContextSummaryRetainedTail<'a> {
    plan: &'a ContextHistoryPlan,
    identity: Digest,
}

impl<'a> ContextSummaryRetainedTail<'a> {
    /// Returns the exact summary-retained-tail identity.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }

    /// Returns the retained complete-group count.
    #[must_use]
    pub const fn segment_count(self) -> u32 {
        self.plan.retained_segment_count
    }

    /// Iterates retained group handles without flattening or copying payloads.
    #[must_use]
    pub fn segments(self) -> RetainedHistorySegmentIter<'a> {
        RetainedHistorySegmentIter {
            segments: self
                .plan
                .source
                .history()
                .segments()
                .iter()
                .chain(self.plan.source.working().segments()),
            selected: self.plan.selected.iter(),
            position: 0,
            remaining: usize::try_from(self.plan.retained_segment_count).unwrap_or(usize::MAX),
        }
    }
}

/// Exact-size zero-copy iterator over one semantic-summary retained tail.
pub struct RetainedHistorySegmentIter<'a> {
    segments: std::iter::Chain<CanonicalSegmentIter<'a>, slice::Iter<'a, HistorySegment>>,
    selected: slice::Iter<'a, ContextSelectedSegment>,
    position: u32,
    remaining: usize,
}

impl<'a> Iterator for RetainedHistorySegmentIter<'a> {
    type Item = &'a HistorySegment;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let segment = self.segments.next()?;
            let position = self.position;
            self.position = self.position.saturating_add(1);
            if self
                .selected
                .as_slice()
                .first()
                .is_some_and(|selected| selected.position == position)
            {
                let _ = self.selected.next();
                continue;
            }
            self.remaining = self.remaining.saturating_sub(1);
            return Some(segment);
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for RetainedHistorySegmentIter<'_> {}

fn is_semantic_user_intent(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::RecentIntent
        || (segment.kind() == HistorySegmentKind::Conversation
            && segment
                .items()
                .iter()
                .any(|item| item.actor() == Actor::User))
}

fn analyze_context_history(
    source: &ConversationView,
) -> Result<ContextHistoryAnalysis, HistoryPlanningError> {
    let canonical_count = source.history().segments().len();
    let total = canonical_count
        .checked_add(source.working().segments().len())
        .ok_or(HistoryPlanningError::LimitExceeded)?;
    if total > MAX_CONTEXT_PLANNER_SEGMENTS {
        return Err(HistoryPlanningError::LimitExceeded);
    }
    let mut intent_positions = Vec::new();
    let mut latest_steering = None;
    let mut compaction_positions = Vec::new();
    for (position, segment) in source.segments().enumerate() {
        let position = u32::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)?;
        if !segment.model_visible_complete() {
            return Err(HistoryPlanningError::IncompleteGroup { position });
        }
        if is_semantic_user_intent(segment) {
            intent_positions.push(position);
        }
        if segment.kind() == HistorySegmentKind::Steering {
            latest_steering = Some(position);
        }
        if segment.kind() == HistorySegmentKind::Compaction {
            compaction_positions.push(position);
        }
    }
    match compaction_positions.as_slice() {
        [] | [_] => {}
        _ => return Err(HistoryPlanningError::NoncanonicalCompaction),
    }
    Ok(ContextHistoryAnalysis {
        total,
        canonical_count: u32::try_from(canonical_count)
            .map_err(|_| HistoryPlanningError::LimitExceeded)?,
        latest_user_intent: intent_positions.last().copied(),
        intent_positions,
        latest_steering,
        compaction_positions,
    })
}

fn context_summary_barrier(source: &ConversationView, analysis: &ContextHistoryAnalysis) -> u32 {
    prior_compaction_position(source)
        .and_then(|position| u32::try_from(position).ok())
        .or_else(|| {
            analysis.latest_user_intent.and_then(|latest| {
                analysis
                    .compaction_positions
                    .iter()
                    .rev()
                    .copied()
                    .find(|position| *position < latest)
            })
        })
        .map_or(0, |position| position.saturating_add(1))
}

fn select_context_summary_prefix(
    source: &ConversationView,
    analysis: &ContextHistoryAnalysis,
) -> Result<ContextSummarySelection, HistoryPlanningError> {
    let total = u32::try_from(analysis.total).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let barrier = context_summary_barrier(source, analysis);
    let mut groups = Vec::new();
    if let Some(latest) = analysis.latest_user_intent {
        for (index, start) in analysis.intent_positions.iter().copied().enumerate() {
            if start < barrier || start >= latest {
                continue;
            }
            let end = analysis
                .intent_positions
                .get(index.saturating_add(1))
                .copied()
                .unwrap_or(total);
            if end > analysis.canonical_count {
                break;
            }
            groups.push(context_selected_intent_group(source, start, end)?);
        }
    }
    let mut segments = Vec::new();
    if let (Some(first), Some(last)) = (groups.first(), groups.last()) {
        for position in first.start_position..last.end_position {
            let index =
                usize::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)?;
            let segment = source
                .history()
                .segments()
                .get(index)
                .ok_or(HistoryPlanningError::LimitExceeded)?;
            segments.push(ContextSelectedSegment {
                position,
                identity: segment.identity(),
            });
        }
    }
    // Rolling cut: the sealed middle of the active intent group is eligible
    // too. The intent and every steering segment keep their verbatim user
    // authority, and the protected tail begins at the newest complete tool
    // exchange — the batch of the current round, the same boundary the
    // placeholder rung refuses to touch.
    if let Some(latest) = analysis.latest_user_intent {
        let canonical = usize::try_from(analysis.canonical_count)
            .map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let committed = source.history().segments();
        let tail_start = committed
            .iter()
            .take(canonical)
            .enumerate()
            .skip(
                usize::try_from(latest)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1),
            )
            .filter(|(_, segment)| segment.kind() == HistorySegmentKind::ToolExchange)
            .map(|(position, _)| position)
            .last()
            .map(|position| {
                u32::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)
            })
            .transpose()?;
        if let Some(tail_start) = tail_start {
            let mut run_start = None;
            let start = prior_compaction_position(source)
                .and_then(|position| u32::try_from(position).ok())
                .map_or(latest.saturating_add(1), |prior| {
                    latest.max(prior).saturating_add(1)
                });
            for position in start..tail_start {
                let segment = committed
                    .get(usize::try_from(position).unwrap_or(usize::MAX))
                    .ok_or(HistoryPlanningError::LimitExceeded)?;
                let selectable = !matches!(
                    segment.kind(),
                    HistorySegmentKind::RecentIntent
                        | HistorySegmentKind::Steering
                        | HistorySegmentKind::Compaction
                );
                match (run_start, selectable) {
                    (None, true) => run_start = Some(position),
                    (Some(start), false) => {
                        groups.push(context_selected_intent_group(source, start, position)?);
                        run_start = None;
                    }
                    _ => {}
                }
                if selectable {
                    segments.push(ContextSelectedSegment {
                        position,
                        identity: segment.identity(),
                    });
                }
            }
            if let Some(start) = run_start {
                groups.push(context_selected_intent_group(source, start, tail_start)?);
            }
        }
    }
    Ok(ContextSummarySelection { groups, segments })
}

/// The retained set is every segment the selection does not fold, minus a
/// recognized prior compaction (its semantics fold into the new summary
/// rather than surviving as a second compaction). `start_position` is the
/// first retained position; the identity formula is unchanged, so a wholly
/// contiguous retained set produces the same digest as before.
fn context_retained_tail(
    source: &ConversationView,
    selected: &[ContextSelectedSegment],
    total: usize,
) -> Result<ContextRetainedTail, HistoryPlanningError> {
    let prior_position = prior_compaction_position(source);
    let mut selected = selected.iter().peekable();
    let mut start = None;
    let mut retained = Vec::new();
    for (position, segment) in source.segments().enumerate() {
        let position = u32::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)?;
        if Some(usize::try_from(position).unwrap_or(usize::MAX)) == prior_position {
            continue;
        }
        if selected
            .peek()
            .is_some_and(|next| next.position == position)
        {
            let _ = selected.next();
            continue;
        }
        if start.is_none() {
            start = Some(position);
        }
        retained.push(segment);
    }
    let Some(start) = start else {
        return Ok(ContextRetainedTail {
            start_position: u32::try_from(total).unwrap_or(u32::MAX),
            segment_count: 0,
            identity: Digest::sha256(
                DigestDomain::ContextPlan,
                1,
                &[
                    b"plakous-model/context-retained-tail-v1",
                    &source.identity().identity_bytes(),
                    &u32::try_from(total).unwrap_or(u32::MAX).to_be_bytes(),
                    &0_u32.to_be_bytes(),
                ],
            )?,
        });
    };
    let segment_count =
        u32::try_from(retained.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let mut identity = Digest::sha256(
        DigestDomain::ContextPlan,
        1,
        &[
            b"plakous-model/context-retained-tail-v1",
            &source.identity().identity_bytes(),
            &start.to_be_bytes(),
            &segment_count.to_be_bytes(),
        ],
    )?;
    for segment in retained {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-retained-tail-append-v1",
                &identity.identity_bytes(),
                &segment.identity().identity_bytes(),
            ],
        )?;
    }
    Ok(ContextRetainedTail {
        start_position: start,
        segment_count,
        identity,
    })
}

fn context_history_plan_identity(
    source: &ConversationView,
    strategy: ContextHistoryStrategy,
    selected_groups: &[ContextSelectedIntentGroup],
    selected: &[ContextSelectedSegment],
    retained_tail: Option<&ContextRetainedTail>,
) -> Result<Digest, HistoryPlanningError> {
    let selected_count =
        u32::try_from(selected.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let selected_group_count =
        u32::try_from(selected_groups.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let mut identity = Digest::sha256(
        DigestDomain::ContextPlan,
        1,
        &[
            b"plakous-model/context-history-plan-v1",
            &source.history().identity().identity_bytes(),
            &source.identity().identity_bytes(),
            &CONTEXT_PLANNER_POLICY_REVISION.to_be_bytes(),
            &[strategy.tag()],
            &CONTEXT_PLANNER_STRATEGY_REVISION.to_be_bytes(),
            &selected_group_count.to_be_bytes(),
            &selected_count.to_be_bytes(),
            &retained_tail
                .map(|tail| tail.identity())
                .map_or_else(Vec::new, |digest| digest.identity_bytes()),
        ],
    )?;
    for group in selected_groups {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-history-plan-intent-group-v1",
                &identity.identity_bytes(),
                &group.start_position.to_be_bytes(),
                &group.end_position.to_be_bytes(),
                &group.identity.identity_bytes(),
            ],
        )?;
    }
    for selected in selected {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-history-plan-selection-v1",
                &identity.identity_bytes(),
                &selected.position.to_be_bytes(),
                &selected.identity.identity_bytes(),
            ],
        )?;
    }
    Ok(identity)
}

fn context_selected_intent_group(
    source: &ConversationView,
    start: u32,
    end: u32,
) -> Result<ContextSelectedIntentGroup, HistoryPlanningError> {
    let start_usize = usize::try_from(start).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let end_usize = usize::try_from(end).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let mut identity = Digest::sha256(
        DigestDomain::ContextPlan,
        1,
        &[
            b"plakous-model/context-selected-intent-group-v1",
            &source.history().identity().identity_bytes(),
            &source.identity().identity_bytes(),
            &start.to_be_bytes(),
            &end.to_be_bytes(),
        ],
    )?;
    for segment in source
        .history()
        .segments()
        .iter()
        .skip(start_usize)
        .take(end_usize.saturating_sub(start_usize))
    {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-selected-intent-group-append-v1",
                &identity.identity_bytes(),
                &segment.identity().identity_bytes(),
            ],
        )?;
    }
    Ok(ContextSelectedIntentGroup {
        start_position: start,
        end_position: end,
        identity,
    })
}
