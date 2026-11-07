//! Source-derived offline receipts, including exact history and context replay.

use super::placeholder::{
    collect_placeholder_candidates, placeholder_successor, portable_placeholder_successor,
    replay_omission_successor,
};
use super::request::portable_compaction_history;
use super::{
    DurableCompactionIdentity, DurablePlaceholderEvidence, DurablePlaceholderRequestRecord,
};
use crate::{
    AgentError, ContextEvaluationAudit, DurableOperationKind, DurableOperationState,
    RuntimeCheckpoint, SemanticCompactionPublication, SessionStoreError,
};
use plakous_model::{Digest, DigestDomain};

pub(super) fn validate(
    checkpoint: &RuntimeCheckpoint,
    source: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> Result<(), AgentError> {
    let refuse = || AgentError::from_store(SessionStoreError::corrupt());
    let manifest = checkpoint.manifest();
    let source_manifest = source.manifest();
    let operations = manifest.ledger().operations();
    let record = operations.iter().find(|record| matches!(record.kind(),DurableOperationKind::Compaction { workflow_id } if *workflow_id == identity.workflow_id)).ok_or_else(refuse)?;
    let request: DurablePlaceholderRequestRecord =
        serde_json::from_slice(record.request().bytes()).map_err(|_| refuse())?;
    let DurableOperationState::Finalized(component) = record.state() else {
        return Err(refuse());
    };
    let evidence: DurablePlaceholderEvidence =
        serde_json::from_slice(component.bytes()).map_err(|_| refuse())?;
    let expected = source_manifest
        .ledger()
        .clone()
        .append_operation((*record).clone())
        .map_err(|_| refuse())?;
    let canonical = source_manifest.canonical();
    if request.workflow_id != identity.workflow_id
        || request.source_session != identity.source.session_id
        || request.source_branch != identity.source.branch_id
        || request.source_checkpoint != source.id()
        || request.source_history != canonical.history().identity()
        || request.source_projection != canonical.projection().identity()
        || canonical.stable_prefix().resolved_harness_plan_identity()
            != Some(request.source_harness_plan)
        || DurableCompactionIdentity::new(&request.operation_id, identity.source)?.workflow_id
            != identity.workflow_id
        || request.target_affinity_entropy != [0; 32]
        || request.version != if request.replaced_results == 0 { 2 } else { 1 }
        || evidence.version != request.version
        || evidence.workflow_id != identity.workflow_id
        || evidence.source_checkpoint != source.id()
        || serde_json::to_vec(&request).map_err(|_| refuse())? != record.request().bytes()
        || serde_json::to_vec(&evidence).map_err(|_| refuse())? != component.bytes()
        || !crate::runtime::operation_record::ledger_matches_retirement(
            &expected,
            manifest.ledger(),
            manifest.lifecycle(),
        )
        || manifest.lifecycle() != source_manifest.lifecycle()
        || manifest.harness_plan_component() != source_manifest.harness_plan_component()
        || manifest.binding() != source_manifest.binding()
        || manifest.tools() != source_manifest.tools()
        || manifest.durability() != source_manifest.durability()
        || manifest.canonical().stable_prefix().identity() != canonical.stable_prefix().identity()
        || manifest.family_artifact().is_some()
    {
        return Err(refuse());
    }
    validate_successor(checkpoint, source, identity, &request, &evidence)
}

fn validate_successor(
    checkpoint: &RuntimeCheckpoint,
    source: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
    request: &DurablePlaceholderRequestRecord,
    evidence: &DurablePlaceholderEvidence,
) -> Result<(), AgentError> {
    let refuse = || AgentError::from_store(SessionStoreError::corrupt());
    let manifest = checkpoint.manifest();
    let source_manifest = source.manifest();
    let canonical = source_manifest.canonical();
    let history = canonical.history();
    let portable = portable_compaction_history(history)?;
    for input in [history, &portable] {
        let count = usize::try_from(request.replaced_results).map_err(|_| refuse())?;
        let candidates = collect_placeholder_candidates(input);
        let successor = if count == 0 {
            replay_omission_successor(history, input, identity.workflow_id)
        } else if input.identity() != history.identity() {
            portable_placeholder_successor(history, input, &candidates, count, identity.workflow_id)
        } else {
            placeholder_successor(input, &candidates, count, identity.workflow_id)
        };
        let Ok(successor) = successor else {
            continue;
        };
        if successor.selected_start != request.selected_start
            || successor.selected_end != request.selected_end
            || successor.replaced_results != evidence.replaced_results
            || successor.replaced_bytes != request.replaced_bytes
            || successor.replaced_bytes != evidence.replaced_bytes
            || successor.history.identity() != evidence.successor_history
            || successor.history.identity() != manifest.canonical().history().identity()
            || successor.notice.identity() != evidence.notice_segment
        {
            continue;
        }
        let operation = Digest::sha256(
            DigestDomain::ProviderOperation,
            3,
            &[
                b"plakous-agent/session-tool-result-placeholders-publication-v1",
                identity.workflow_id.as_bytes(),
                &input.identity().identity_bytes(),
                &successor.history.identity().identity_bytes(),
                &successor.notice.identity().identity_bytes(),
            ],
        )
        .map_err(|_| refuse())?;
        if evidence.operation != operation {
            continue;
        }
        let before = ContextEvaluationAudit::new(
            request.before_measurement,
            request.before_status,
            request.before_input_tokens,
        )
        .map_err(|_| refuse())?;
        let after = ContextEvaluationAudit::new(
            evidence.after_measurement,
            evidence.after_status,
            evidence.after_input_tokens,
        )
        .map_err(|_| refuse())?;
        let publication = SemanticCompactionPublication::new(
            source.id(),
            operation,
            source_manifest
                .binding()
                .route_binding_identity()
                .map_err(|_| refuse())?,
            source_manifest.binding().model_binding(),
            request.source_harness_plan,
            before,
            after,
            None,
        )
        .map_err(|_| refuse())?;
        let context = canonical
            .context()
            .retained_for_exact_resume()
            .successful_tool_result_placeholders(
                &publication,
                &successor.notice,
                &successor.history,
                history.identity(),
                request.source_projection,
                successor.selected_start,
                successor.selected_end,
                successor.replaced_results,
                u32::try_from(history.segments().len()).map_err(|_| refuse())?,
            )
            .map_err(|_| refuse())?;
        if context.identity() == manifest.canonical().context().identity() {
            return Ok(());
        }
    }
    Err(refuse())
}
