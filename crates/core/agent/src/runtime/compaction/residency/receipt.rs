use super::record::{ResidencyEvidence, ResidencyRequest, operation_identity, publication};
use super::residency_successor;
use crate::runtime::compaction::DurableCompactionIdentity;
use crate::{
    AgentError, DurableOperationKind, DurableOperationState, RuntimeCheckpoint, SessionStoreError,
};

pub(in crate::runtime) fn validate(
    checkpoint: &RuntimeCheckpoint,
    source: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> Result<(), AgentError> {
    let refuse = || AgentError::from_store(SessionStoreError::corrupt());
    let manifest = checkpoint.manifest();
    let prior = source.manifest();
    let operations = manifest.ledger().operations();
    let record = operations.iter().find(|record| matches!(record.kind(), DurableOperationKind::Compaction { workflow_id } if *workflow_id == identity.workflow_id)).ok_or_else(refuse)?;
    let request = ResidencyRequest::decode(record.request().bytes(), identity.workflow_id)
        .ok_or_else(refuse)?;
    let DurableOperationState::Finalized(component) = record.state() else {
        return Err(refuse());
    };
    let evidence =
        ResidencyEvidence::decode(component.bytes(), identity.workflow_id).ok_or_else(refuse)?;
    let expected_ledger = prior
        .ledger()
        .clone()
        .append_operation((*record).clone())
        .map_err(|_| refuse())?;
    if request.source_session != identity.source.session_id
        || request.source_branch != identity.source.branch_id
        || request.source_checkpoint != source.id()
        || request.source_history != prior.canonical().history().identity()
        || request.source_projection != prior.canonical().projection().identity()
        || prior
            .canonical()
            .stable_prefix()
            .resolved_harness_plan_identity()
            != Some(request.source_harness_plan)
        || DurableCompactionIdentity::new(&request.operation_id, identity.source)?.workflow_id
            != identity.workflow_id
        || evidence.source_checkpoint != source.id()
        || evidence.selection != request.plan.identity()?
        || evidence.replaced_images as usize != request.plan.selection.len()
        || evidence.before != request.plan.before
        || evidence.after != request.plan.after
        || evidence.after_input_tokens >= request.before_input_tokens
        || !crate::runtime::operation_record::ledger_matches_retirement(
            &expected_ledger,
            manifest.ledger(),
            manifest.lifecycle(),
        )
        || manifest.lifecycle() != prior.lifecycle()
        || manifest.harness_plan_component() != prior.harness_plan_component()
        || manifest.binding() != prior.binding()
        || manifest.tools() != prior.tools()
        || manifest.durability() != prior.durability()
        || manifest.canonical().stable_prefix().identity()
            != prior.canonical().stable_prefix().identity()
        || manifest.family_artifact().is_some()
    {
        return Err(refuse());
    }
    // This recomputes oldest-first selection, protections, newest exemption,
    // both batching targets and the maximum-legal-batch full-budget bound.
    let successor = residency_successor(
        prior.canonical().history(),
        &request.plan,
        identity.workflow_id,
    )
    .map_err(|_| refuse())?;
    if evidence.operation
        != operation_identity(
            identity.workflow_id,
            &request.plan,
            request.source_history,
            &successor,
        )?
        || evidence.successor_history != successor.history.identity()
        || evidence.successor_history != manifest.canonical().history().identity()
        || evidence.notice_segment != successor.notice.identity()
    {
        return Err(refuse());
    }
    let context = prior
        .canonical()
        .context()
        .retained_for_exact_resume()
        .successful_image_residency(
            &publication(source, &request, &evidence)?,
            &successor.notice,
            &successor.history,
            request.selection_audit(super::checked_count(
                prior.canonical().history().segments().len(),
            )?)?,
        )
        .map_err(|_| refuse())?;
    if context.identity() != manifest.canonical().context().identity() {
        return Err(refuse());
    }
    Ok(())
}
