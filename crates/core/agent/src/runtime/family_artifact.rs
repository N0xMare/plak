//! Exact artifact promotion shared by the driver and local terminal recovery.

use crate::manifest::RuntimeContextState;
use crate::runtime::continuation::runtime_manifest_error;
use crate::{AgentError, AgentErrorKind, FamilyArtifact};
use plakous_model::{
    CanonicalHistory, ConversationView, PreparedCallRecord, SealedFamilyArtifactCandidate,
    WorkingProjection,
};

pub(super) fn promote_family_artifact(
    candidate: &SealedFamilyArtifactCandidate,
    prepared: &PreparedCallRecord,
    history: &CanonicalHistory,
    context: &RuntimeContextState,
    source_items: usize,
    output_items: usize,
) -> Result<FamilyArtifact, AgentError> {
    let refusal = || {
        AgentError::new(
            AgentErrorKind::Model,
            "family artifact does not match the committed provider response",
        )
    };
    let projection = prepared.projection();
    let producer = candidate.producer();
    let covered_items = source_items.checked_add(output_items).ok_or_else(refusal)?;
    let view = ConversationView::from_shared(history.clone(), WorkingProjection::default());
    let source = view
        .canonical_item_prefix_identity(source_items)
        .map_err(|_| refusal())?;
    let covered = view
        .canonical_item_prefix_identity(covered_items)
        .map_err(|_| refusal())?;
    // The durable request identity binds metadata not separately repeated in
    // the prepared projection; no reopened request or provider plan is guessed.
    if producer.request() != projection.request()
        || producer.model_input() != projection.model_input()
        || producer.plan() != projection.plan()
        || producer.root() != projection.operation_root()
        || producer.attempt() != projection.attempt_id()
        || producer.cache_affinity() != projection.cache_affinity()
        || producer.cache_affinity() != Some(context.affinity().identity())
        || producer.source_projection() != source.identity()
        || usize::try_from(producer.source_projection_items()).ok() != Some(source_items)
        || usize::try_from(producer.output_items()).ok() != Some(output_items)
        || producer.covered_projection() != covered.identity()
        || usize::try_from(producer.covered_projection_items()).ok() != Some(covered_items)
    {
        return Err(refusal());
    }
    FamilyArtifact::from_candidate(candidate).map_err(runtime_manifest_error)
}
