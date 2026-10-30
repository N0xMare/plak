use super::{
    ImageSelectionKind, ImageTotals, POLICY_REVISION, ResidencyPlan, ResidencySuccessor, invalid,
};
use crate::manifest::ResidencySelectionAudit;
use crate::{
    AgentError, CheckpointId, ContextEvaluationAudit, RuntimeCheckpoint,
    SemanticCompactionPublication, SessionWorkflowId,
};
use plakous_model::{BranchId, Digest, DigestDomain, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ResidencyRequest {
    pub version: u32,
    pub workflow_id: SessionWorkflowId,
    pub operation_id: String,
    pub source_session: SessionId,
    pub source_branch: BranchId,
    pub source_checkpoint: CheckpointId,
    pub source_history: Digest,
    pub source_projection: Digest,
    pub source_harness_plan: Digest,
    pub admission: bool,
    pub plan: ResidencyPlan,
    pub before_measurement: Digest,
    pub before_status: Digest,
    pub before_input_tokens: u64,
}

impl ResidencyRequest {
    pub(in crate::runtime) fn decode(bytes: &[u8], workflow: SessionWorkflowId) -> Option<Self> {
        let request: Self = serde_json::from_slice(bytes).ok()?;
        let selected = &request.plan.selection;
        let keys: Vec<_> = selected
            .iter()
            .map(|image| (image.segment, image.item, image.block, image.part))
            .collect();
        (request.version == 3
            && request.workflow_id == workflow
            && request.plan.policy_revision == POLICY_REVISION
            && !selected.is_empty()
            && valid_plan_shape(&request.plan)
            && request.plan.incoming.unknown_bytes <= request.plan.incoming.count
            && (request.admission || request.plan.incoming == ImageTotals::ZERO)
            && keys.windows(2).all(|pair| pair[0] < pair[1])
            && serde_json::to_vec(&request).ok()?.as_slice() == bytes)
            .then_some(request)
    }

    pub(in crate::runtime) fn selection_audit(
        &self,
        source_segments: u32,
    ) -> Result<ResidencySelectionAudit, AgentError> {
        let start = self
            .plan
            .selection
            .first()
            .ok_or_else(|| invalid("empty image residency selection"))?
            .segment;
        let end = self
            .plan
            .selection
            .last()
            .ok_or_else(|| invalid("empty image residency selection"))?
            .segment
            + 1;
        Ok(ResidencySelectionAudit {
            source_history: self.source_history,
            source_projection: self.source_projection,
            source_segments,
            start,
            end,
            images: super::checked_count(self.plan.selection.len())?,
            segments: super::checked_count(
                self.plan
                    .selection
                    .iter()
                    .map(|image| image.segment)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
            )?,
            identity: self.plan.identity()?,
        })
    }
}

fn valid_plan_shape(plan: &ResidencyPlan) -> bool {
    let Ok(budget) =
        plakous_model::ResidentImageBudget::new(plan.max_images, plan.max_encoded_bytes)
    else {
        return false;
    };
    let mut after = plan.before;
    for image in &plan.selection {
        if image.segment as usize >= crate::MAX_RUNTIME_RECORDS
            || !matches!(
                (image.kind, image.part, image.call_id),
                (ImageSelectionKind::UserImage, None, None)
                    | (ImageSelectionKind::ToolResultImage, Some(_), Some(_))
            )
            || image.raw_bytes.is_some() != image.sha256.is_some()
        {
            return false;
        }
        let Some(count) = after.count.checked_sub(1) else {
            return false;
        };
        after.count = count;
        if let Some(bytes) = image.encoded_bytes {
            let Some(bytes) = after.encoded_bytes.checked_sub(bytes) else {
                return false;
            };
            after.encoded_bytes = bytes;
        } else {
            let Some(count) = after.unknown_bytes.checked_sub(1) else {
                return false;
            };
            after.unknown_bytes = count;
        }
    }
    !plan.before.fits(budget)
        && plan.after.fits(budget)
        && after == plan.after
        && plan.before.unknown_bytes <= plan.before.count
        && plan.after.unknown_bytes <= plan.after.count
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ResidencyEvidence {
    pub version: u32,
    pub workflow_id: SessionWorkflowId,
    pub source_checkpoint: CheckpointId,
    pub operation: Digest,
    pub selection: Digest,
    pub successor_history: Digest,
    pub notice_segment: Digest,
    pub replaced_images: u32,
    pub before: ImageTotals,
    pub after: ImageTotals,
    pub after_measurement: Digest,
    pub after_status: Digest,
    pub after_input_tokens: u64,
}

impl ResidencyEvidence {
    pub(in crate::runtime) fn decode(bytes: &[u8], workflow: SessionWorkflowId) -> Option<Self> {
        let evidence: Self = serde_json::from_slice(bytes).ok()?;
        (evidence.version == 3
            && evidence.workflow_id == workflow
            && evidence.replaced_images > 0
            && evidence.before.count.checked_sub(evidence.replaced_images)
                == Some(evidence.after.count)
            && evidence.after.encoded_bytes <= evidence.before.encoded_bytes
            && evidence.after.unknown_bytes <= evidence.before.unknown_bytes
            && serde_json::to_vec(&evidence).ok()?.as_slice() == bytes)
            .then_some(evidence)
    }
}

pub(in crate::runtime) fn operation_identity(
    workflow: SessionWorkflowId,
    plan: &ResidencyPlan,
    source: Digest,
    successor: &ResidencySuccessor,
) -> Result<Digest, AgentError> {
    Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/session-image-residency-publication-v1",
            workflow.as_bytes(),
            &source.identity_bytes(),
            &plan.identity()?.identity_bytes(),
            &successor.history.identity().identity_bytes(),
            &successor.notice.identity().identity_bytes(),
        ],
    )
    .map_err(|_| invalid("image residency publication identity failed"))
}

pub(in crate::runtime) fn publication(
    source: &RuntimeCheckpoint,
    request: &ResidencyRequest,
    evidence: &ResidencyEvidence,
) -> Result<SemanticCompactionPublication, AgentError> {
    let convert = crate::runtime::continuation::runtime_manifest_error;
    SemanticCompactionPublication::new(
        source.id(),
        evidence.operation,
        source
            .manifest()
            .binding()
            .route_binding_identity()
            .map_err(convert)?,
        source.manifest().binding().model_binding(),
        request.source_harness_plan,
        ContextEvaluationAudit::new(
            request.before_measurement,
            request.before_status,
            request.before_input_tokens,
        )
        .map_err(convert)?,
        ContextEvaluationAudit::new(
            evidence.after_measurement,
            evidence.after_status,
            evidence.after_input_tokens,
        )
        .map_err(convert)?,
        None,
    )
    .map_err(convert)
}

pub(in crate::runtime) fn is_publication(
    record: &crate::DurableOperationRecord,
    workflow: SessionWorkflowId,
) -> bool {
    let Some(request) = ResidencyRequest::decode(record.request().bytes(), workflow) else {
        return false;
    };
    let crate::DurableOperationState::Finalized(component) = record.state() else {
        return false;
    };
    let Some(evidence) = ResidencyEvidence::decode(component.bytes(), workflow) else {
        return false;
    };
    let source = crate::runtime::durable::DurableSessionAddress::new(
        request.source_session,
        request.source_branch,
        request.source_checkpoint,
    );
    let Ok(identity) =
        crate::runtime::compaction::DurableCompactionIdentity::new(&request.operation_id, source)
    else {
        return false;
    };
    request.plan.identity().ok() == Some(evidence.selection)
        && identity.workflow_id == workflow
        && request.source_checkpoint == evidence.source_checkpoint
        && request.plan.before == evidence.before
        && request.plan.after == evidence.after
        && request.plan.selection.len() == evidence.replaced_images as usize
        && request.before_input_tokens > evidence.after_input_tokens
        && ContextEvaluationAudit::new(
            request.before_measurement,
            request.before_status,
            request.before_input_tokens,
        )
        .is_ok()
        && ContextEvaluationAudit::new(
            evidence.after_measurement,
            evidence.after_status,
            evidence.after_input_tokens,
        )
        .is_ok()
        && record.transitions().len() == 1
        && record.transitions()[0].checkpoint_id() == identity.success_checkpoint
}
