//! Single-sourced interpreter evaluation and continuation records.

use crate::runtime::durable::{
    DurableCodeCellAbandonmentIdentity, DurableCodeCellAbandonmentRequest,
};
use crate::{AgentError, AgentErrorKind, CheckpointId, ReconstructiveComponent};
use plakous_model::{Digest, DigestDomain, EntryId, ToolCallBlock};
impl DurableCodeCellAbandonmentIdentity {
    pub(in crate::runtime) fn new(
        request: &DurableCodeCellAbandonmentRequest,
    ) -> Result<Self, AgentError> {
        let source = request.source();
        let workflow = Digest::sha256(
            DigestDomain::ProviderOperation,
            3,
            &[
                b"plakous-agent/code-cell-abandonment-workflow-v1",
                request.operation_id().as_bytes(),
                source.session_id.as_uuid().as_bytes(),
                source.branch_id.as_uuid().as_bytes(),
                source.checkpoint_id.as_uuid().as_bytes(),
            ],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "Code Cell abandonment identity failed",
            )
        })?;
        Ok(Self {
            source,
            workflow,
            evidence_checkpoint: code_cell_abandonment_checkpoint_id(
                workflow,
                b"abandonment-evidence",
            )?,
            final_checkpoint: code_cell_abandonment_checkpoint_id(workflow, b"finalized")?,
        })
    }
}

fn code_cell_abandonment_checkpoint_id(
    workflow: Digest,
    role: &[u8],
) -> Result<CheckpointId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/code-cell-abandonment-derived-id-v1",
            workflow.bytes(),
            role,
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "Code Cell abandonment identity failed",
        )
    })?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CheckpointId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

pub(in crate::runtime) fn encode_local_composition_evaluation_request(
    request: &DurableLocalCompositionEvaluationRequestV2,
) -> Result<Vec<u8>, AgentError> {
    serde_json::to_vec(&("plakous-local-composition-evaluation-v2", request)).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "local composition request could not be captured",
        )
    })
}

pub(in crate::runtime) fn decode_local_composition_evaluation_request(
    component: &ReconstructiveComponent,
) -> Option<DurableLocalCompositionEvaluationRequestV2> {
    let (tag, request): (String, DurableLocalCompositionEvaluationRequestV2) =
        serde_json::from_slice(component.bytes()).ok()?;
    if tag != "plakous-local-composition-evaluation-v2"
        || request.version != 2
        || encode_local_composition_evaluation_request(&request)
            .ok()?
            .as_slice()
            != component.bytes()
    {
        return None;
    }
    Some(request)
}

pub(in crate::runtime) fn encode_local_composition_continuation_request(
    request: &DurableLocalCompositionContinuationRequestV1,
) -> Result<Vec<u8>, AgentError> {
    serde_json::to_vec(&("plakous-local-composition-continuation-v1", request)).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "local composition continuation could not be captured",
        )
    })
}

#[cfg(test)]
pub(in crate::runtime) fn decode_local_composition_continuation_request(
    component: &ReconstructiveComponent,
) -> Option<DurableLocalCompositionContinuationRequestV1> {
    let (tag, request): (String, DurableLocalCompositionContinuationRequestV1) =
        serde_json::from_slice(component.bytes()).ok()?;
    if tag != "plakous-local-composition-continuation-v1"
        || request.version != 1
        || encode_local_composition_continuation_request(&request)
            .ok()?
            .as_slice()
            != component.bytes()
    {
        return None;
    }
    Some(request)
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct DurableLocalCompositionEvaluationRequestV2 {
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) descriptor_identity: Digest,
    pub(in crate::runtime) host_identity: Vec<u8>,
    pub(in crate::runtime) call: ToolCallBlock,
    pub(in crate::runtime) arguments_digest: Digest,
    pub(in crate::runtime) source_digest: Option<Digest>,
    pub(in crate::runtime) result_entry_id: EntryId,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct DurableLocalCompositionContinuationRequestV1 {
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) descriptor_identity: Digest,
    pub(in crate::runtime) host_identity: Vec<u8>,
    pub(in crate::runtime) evaluation_operation_id: u64,
    pub(in crate::runtime) call: ToolCallBlock,
    pub(in crate::runtime) arguments_digest: Digest,
    pub(in crate::runtime) result_entry_id: EntryId,
}
