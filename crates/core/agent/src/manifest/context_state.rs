//! Typed runtime context state: transitions, compaction provenance, and their
//! canonical identities and encodings.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the manifest module that shares its vocabulary"
)]

mod codec;
mod provenance;
mod state;
pub(super) use codec::compaction_provenance_identity;
use codec::decode_runtime_context_state;
use codec::encode_runtime_context_state;
#[cfg(test)]
pub(super) use codec::read_compaction_provenance;
use codec::runtime_context_state_identity;
use codec::runtime_context_transition_identity;
#[cfg(test)]
pub(super) use codec::write_compaction_provenance_fields;

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

/// Native successors that may keep a protected current-round tail after
/// the compaction item.
const NATIVE_COMPACTION_STRATEGY_REVISION: u32 = 2;

/// Exact invalidation and epoch-rotation proof published with compaction.
#[derive(Clone, Eq, PartialEq)]
pub struct CompactionInvalidation {
    pub(super) source_affinity: Digest,
    pub(super) source_epoch: u64,
    pub(super) target_affinity: Digest,
    pub(super) target_epoch: u64,
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
    /// Retained segments after the summary: a native successor's protected
    /// current-round tail. Zero for every other strategy.
    pub(super) trailing_segment_count: u32,
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

/// One compact typed, bounded, versioned runtime context authority.
#[derive(Clone, Eq, PartialEq)]
pub struct RuntimeContextState {
    affinity: ModelCacheAffinity,
    /// The driver's context-measure anchor as of this checkpoint's
    /// publication (spec 01, 10.3). Every transition and compaction successor
    /// starts without one.
    occupancy_anchor: Option<ContextAnchor>,
    request_anchor: Option<plakous_model::ContextRequestAnchor>,
    transition: Option<RuntimeContextTransition>,
    compaction: Option<CompactionProvenance>,
    pub(super) identity: Digest,
}
