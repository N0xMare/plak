//! Deterministic checkpoint and commit identities.

use crate::manifest::RuntimeLifecycleCodec;
use crate::runtime::continuation::runtime_manifest_error;
use crate::{
    AgentError, AgentErrorKind, CheckpointId, FamilyArtifact, RuntimeLifecycle,
    RuntimeOperationLedger,
};
use plakous_model::{
    ContentBlock, ConversationItem, Digest, DigestDomain, HistorySegmentKind, RunId,
};
pub(in crate::runtime) fn history_segment_kind(items: &[ConversationItem]) -> HistorySegmentKind {
    let mut reasoning_replay = false;
    let mut compaction = false;
    for block in items.iter().flat_map(ConversationItem::blocks) {
        match block {
            ContentBlock::ToolCall(_) | ContentBlock::ToolResult(_) => {
                return HistorySegmentKind::ToolExchange;
            }
            ContentBlock::Reasoning(_) | ContentBlock::Opaque(_) => reasoning_replay = true,
            ContentBlock::Compaction(_) => compaction = true,
            _ => {}
        }
    }
    if compaction {
        HistorySegmentKind::Compaction
    } else if reasoning_replay {
        HistorySegmentKind::ReasoningReplay
    } else {
        HistorySegmentKind::Conversation
    }
}

pub(in crate::runtime) fn deterministic_checkpoint_id(
    run_id: RunId,
    sequence: u64,
) -> Result<CheckpointId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/in-memory-checkpoint-id",
            run_id.as_uuid().as_bytes(),
            &sequence.to_be_bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "in-memory checkpoint identity could not be derived",
        )
    })?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    // RFC 9562 UUIDv8: deterministic application-defined bytes with the
    // standard version and variant bits set explicitly.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CheckpointId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

pub(in crate::runtime) fn checkpoint_commit_identity(
    facts: CheckpointCommitFacts<'_>,
) -> Result<Digest, AgentError> {
    let lifecycle_bytes =
        RuntimeLifecycleCodec::encode(facts.lifecycle).map_err(runtime_manifest_error)?;
    let ledger_identity = facts
        .ledger
        .authority_identity()
        .map_err(runtime_manifest_error)?;
    let family_integrity = facts.family_artifact.map(FamilyArtifact::integrity);
    Digest::sha256(
        DigestDomain::ProviderOperation,
        CHECKPOINT_COMMIT_IDENTITY_REVISION,
        &[
            b"plakous-agent/checkpoint-commit-v4",
            facts.checkpoint_id.as_uuid().as_bytes(),
            facts.parent_checkpoint_id.as_uuid().as_bytes(),
            &facts.branch_revision.to_be_bytes(),
            &lifecycle_bytes,
            facts.history_identity.bytes(),
            ledger_identity.bytes(),
            facts.context_identity.bytes(),
            family_integrity
                .as_ref()
                .map_or(&[][..], |digest| digest.bytes().as_slice()),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "checkpoint commit identity could not be constructed",
        )
    })
}

pub(in crate::runtime) const CHECKPOINT_COMMIT_IDENTITY_REVISION: u32 = 6;

#[derive(Clone, Copy)]
pub(in crate::runtime) struct CheckpointCommitFacts<'a> {
    pub(in crate::runtime) checkpoint_id: CheckpointId,
    pub(in crate::runtime) parent_checkpoint_id: CheckpointId,
    pub(in crate::runtime) branch_revision: u64,
    pub(in crate::runtime) lifecycle: &'a RuntimeLifecycle,
    pub(in crate::runtime) history_identity: Digest,
    pub(in crate::runtime) ledger: &'a RuntimeOperationLedger,
    pub(in crate::runtime) context_identity: Digest,
    pub(in crate::runtime) family_artifact: Option<&'a FamilyArtifact>,
}
