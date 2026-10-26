//! Bounded reconstruction-complete Messages hosted-execution artifact.

use std::sync::Arc;

use plakous_model::{
    ActiveFamilyArtifact, CanonicalItemPrefixIdentity, Digest, DigestDomain, FamilyArtifactError,
    FamilyArtifactProducerFacts, SealedFamilyArtifactCandidate, ValidatedRequestPlan,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::request::MessageBlock;

/// Behavior-bearing Messages artifact schema.
pub const MESSAGES_ARTIFACT_SCHEMA_REVISION: u32 = 1;
/// Exact Messages artifact codec.
pub const MESSAGES_ARTIFACT_CODEC_REVISION: u32 = 1;
const FAMILY: &str = "messages";
const MAX_CONTAINER_BYTES: usize = 512;
const MAX_NATIVE_BLOCKS: usize = 256;
const MAX_SERVER_TOOLS: usize = 256;

/// Validated cumulative server-tool lineage carried into the next streamed response.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct MessagesArtifactLineage(Vec<MessagesServerToolState>);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct MessagesServerToolState {
    id: String,
    result_seen: bool,
}

impl MessagesArtifactLineage {
    pub(crate) fn server_ids(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|state| state.id.as_str())
    }

    pub(crate) fn completed_result_ids(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(|state| state.result_seen)
            .map(|state| state.id.as_str())
    }

    pub(crate) fn next_unresolved_id(&self) -> Option<&str> {
        self.0
            .iter()
            .find(|state| !state.result_seen)
            .map(|state| state.id.as_str())
    }

    pub(crate) fn is_unresolved(&self, id: &str) -> bool {
        self.0
            .iter()
            .any(|state| state.id == id && !state.result_seen)
    }

    pub(crate) fn push_server(&mut self, id: &str) -> Result<(), MessagesArtifactError> {
        if id.is_empty()
            || self.0.len() >= MAX_SERVER_TOOLS
            || self.0.iter().any(|state| state.id == id)
        {
            return Err(MessagesArtifactError::InvalidBlocks);
        }
        self.0.push(MessagesServerToolState {
            id: id.to_owned(),
            result_seen: false,
        });
        Ok(())
    }

    pub(crate) fn complete_next(&mut self, id: &str) -> Result<(), MessagesArtifactError> {
        if self.next_unresolved_id() != Some(id) {
            return Err(MessagesArtifactError::InvalidBlocks);
        }
        let state = self
            .0
            .iter_mut()
            .find(|state| state.id == id)
            .ok_or(MessagesArtifactError::InvalidBlocks)?;
        state.result_seen = true;
        Ok(())
    }

    fn validate(&self) -> Result<(), MessagesArtifactError> {
        if self.0.len() > MAX_SERVER_TOOLS
            || self.0.iter().any(|state| state.id.is_empty())
            || self
                .0
                .windows(2)
                .any(|pair| !pair[0].result_seen && pair[1].result_seen)
            || self
                .0
                .iter()
                .map(|state| state.id.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.0.len()
        {
            return Err(MessagesArtifactError::InvalidBlocks);
        }
        Ok(())
    }

    pub(crate) fn advance(&self, blocks: &[MessageBlock]) -> Result<Self, MessagesArtifactError> {
        self.validate()?;
        let mut next = self.clone();
        for block in blocks {
            match block {
                MessageBlock::ServerToolUse { id, name, .. } => {
                    if name != "code_execution" {
                        return Err(MessagesArtifactError::InvalidBlocks);
                    }
                    next.push_server(id)?;
                }
                MessageBlock::ToolUse {
                    caller: Some(caller),
                    ..
                } => {
                    if caller.kind == "direct" && caller.tool_id.is_none() {
                        continue;
                    }
                    let Some(tool_id) = caller.tool_id.as_deref() else {
                        return Err(MessagesArtifactError::InvalidBlocks);
                    };
                    if caller.kind != "code_execution_20260120" || !next.is_unresolved(tool_id) {
                        return Err(MessagesArtifactError::InvalidBlocks);
                    }
                }
                MessageBlock::CodeExecutionToolResult { tool_use_id, .. } => {
                    next.complete_next(tool_use_id)?;
                }
                _ => {}
            }
        }
        Ok(next)
    }
}

/// Fail-closed Messages artifact error.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MessagesArtifactError {
    /// The neutral wrapper, codec, or integrity was incompatible.
    InvalidEnvelope,
    /// Exact target/configuration/tool binding drifted.
    BindingMismatch,
    /// Canonical covered prefix or producing plan drifted.
    ProjectionMismatch,
    /// Container was absent, empty, or oversized.
    InvalidContainer,
    /// Native assistant blocks were absent, oversized, malformed, or reordered.
    InvalidBlocks,
    /// Stop state was incompatible with the native blocks.
    InvalidStopState,
    /// Neutral candidate construction failed.
    Candidate,
}

impl std::fmt::Display for MessagesArtifactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidEnvelope => "invalid Messages family artifact envelope",
            Self::BindingMismatch => "Messages family artifact binding mismatch",
            Self::ProjectionMismatch => "Messages family artifact canonical projection mismatch",
            Self::InvalidContainer => "Messages family artifact container is invalid",
            Self::InvalidBlocks => "Messages family artifact native blocks are invalid",
            Self::InvalidStopState => "Messages family artifact stop state is invalid",
            Self::Candidate => "Messages family artifact candidate could not be sealed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for MessagesArtifactError {}

/// Exact native stop state retained for the next Messages request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessagesArtifactStop {
    pub(crate) reason: String,
    pub(crate) sequence: Option<String>,
    pub(crate) details: Option<Box<RawValue>>,
}

impl PartialEq for MessagesArtifactStop {
    fn eq(&self, other: &Self) -> bool {
        self.reason == other.reason
            && self.sequence == other.sequence
            && self.details.as_deref().map(RawValue::get)
                == other.details.as_deref().map(RawValue::get)
    }
}

/// Decoded inert family-owned artifact.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessagesFamilyArtifact {
    binding: Digest,
    producer_plan: Digest,
    tool_set: Digest,
    configuration: Digest,
    covered_projection: Digest,
    pub(crate) covered_items: u32,
    pub(crate) container: String,
    prior_lineage: MessagesArtifactLineage,
    lineage: MessagesArtifactLineage,
    assistant_blocks: Vec<MessageBlock>,
    stop: MessagesArtifactStop,
}

pub(crate) struct MessagesArtifactProduction {
    pub(crate) binding: Digest,
    pub(crate) producer_plan: Digest,
    pub(crate) tool_set: Digest,
    pub(crate) configuration: Digest,
    pub(crate) covered: CanonicalItemPrefixIdentity,
    pub(crate) container: String,
    pub(crate) prior_lineage: MessagesArtifactLineage,
    pub(crate) assistant_blocks: Vec<MessageBlock>,
    pub(crate) stop: MessagesArtifactStop,
}

impl MessagesFamilyArtifact {
    pub(crate) fn new(
        production: MessagesArtifactProduction,
    ) -> Result<Self, MessagesArtifactError> {
        let MessagesArtifactProduction {
            binding,
            producer_plan,
            tool_set,
            configuration,
            covered,
            container,
            prior_lineage,
            assistant_blocks,
            stop,
        } = production;
        let lineage = prior_lineage.advance(&assistant_blocks)?;
        let value = Self {
            binding,
            producer_plan,
            tool_set,
            configuration,
            covered_projection: covered.identity(),
            covered_items: u32::try_from(covered.item_count())
                .map_err(|_| MessagesArtifactError::ProjectionMismatch)?,
            container,
            prior_lineage,
            lineage,
            assistant_blocks,
            stop,
        };
        value.validate_shape()?;
        Ok(value)
    }

    fn validate_shape(&self) -> Result<(), MessagesArtifactError> {
        if self.binding.domain() != DigestDomain::ModelBinding
            || self.producer_plan.domain() != DigestDomain::RequestPlan
            || self.tool_set.domain() != DigestDomain::Blob
            || self.configuration.domain() != DigestDomain::Blob
            || self.covered_projection.domain() != DigestDomain::CanonicalItemPrefix
            || self.covered_items == 0
        {
            return Err(MessagesArtifactError::InvalidEnvelope);
        }
        if self.container.is_empty() || self.container.len() > MAX_CONTAINER_BYTES {
            return Err(MessagesArtifactError::InvalidContainer);
        }
        if self.assistant_blocks.is_empty() || self.assistant_blocks.len() > MAX_NATIVE_BLOCKS {
            return Err(MessagesArtifactError::InvalidBlocks);
        }
        if !matches!(
            self.stop.reason.as_str(),
            "end_turn"
                | "stop_sequence"
                | "tool_use"
                | "max_tokens"
                | "model_context_window_exceeded"
                | "pause_turn"
        ) {
            return Err(MessagesArtifactError::InvalidStopState);
        }
        self.prior_lineage.validate()?;
        self.lineage.validate()?;
        if self.prior_lineage.advance(&self.assistant_blocks)? != self.lineage
            || self.lineage.0.is_empty()
        {
            return Err(MessagesArtifactError::InvalidBlocks);
        }
        let has_customer_call = self
            .assistant_blocks
            .iter()
            .any(|block| matches!(block, MessageBlock::ToolUse { .. }));
        if (self.stop.reason == "tool_use") != has_customer_call
            || (self.stop.reason != "tool_use" && has_customer_call)
            || (self.stop.reason == "stop_sequence") != self.stop.sequence.is_some()
        {
            return Err(MessagesArtifactError::InvalidStopState);
        }
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, MessagesArtifactError> {
        let mut bytes = b"PLAKMSG\0".to_vec();
        bytes.extend(serde_json::to_vec(self).map_err(|_| MessagesArtifactError::InvalidEnvelope)?);
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, MessagesArtifactError> {
        let payload = bytes
            .strip_prefix(b"PLAKMSG\0")
            .ok_or(MessagesArtifactError::InvalidEnvelope)?;
        let value: Self =
            serde_json::from_slice(payload).map_err(|_| MessagesArtifactError::InvalidEnvelope)?;
        value.validate_shape()?;
        Ok(value)
    }

    pub(crate) fn from_active(
        active: &ActiveFamilyArtifact,
        expected_binding: Digest,
    ) -> Result<Self, MessagesArtifactError> {
        if active.family() != FAMILY
            || active.schema_revision() != MESSAGES_ARTIFACT_SCHEMA_REVISION
            || active.codec_revision() != MESSAGES_ARTIFACT_CODEC_REVISION
            || active.binding() != expected_binding
        {
            return Err(MessagesArtifactError::BindingMismatch);
        }
        let value = Self::decode(active.bytes())?;
        if value.binding != expected_binding {
            return Err(MessagesArtifactError::BindingMismatch);
        }
        Ok(value)
    }

    pub(crate) fn validate_reuse(
        &self,
        plan: &ValidatedRequestPlan,
        tool_set: Digest,
        configuration: Digest,
        native_blocks: &[MessageBlock],
        container: Option<&str>,
    ) -> Result<(), MessagesArtifactError> {
        let covered = plan
            .request()
            .input()
            .canonical_item_prefix_identity(
                usize::try_from(self.covered_items)
                    .map_err(|_| MessagesArtifactError::ProjectionMismatch)?,
            )
            .map_err(|_| MessagesArtifactError::ProjectionMismatch)?;
        if covered.identity() != self.covered_projection
            || self.tool_set != tool_set
            || self.configuration != configuration
            || !native_blocks_match_canonical(&self.assistant_blocks, native_blocks)
            || container != Some(self.container.as_str())
        {
            return Err(MessagesArtifactError::ProjectionMismatch);
        }
        Ok(())
    }

    pub(crate) fn active_lineage(&self) -> MessagesArtifactLineage {
        self.lineage.clone()
    }

    pub(crate) fn native_blocks(&self) -> &[MessageBlock] {
        &self.assistant_blocks
    }

    pub(crate) fn prior_lineage(&self) -> MessagesArtifactLineage {
        self.prior_lineage.clone()
    }

    /// Reports whether the exact stop state requires continuation inside the root turn.
    #[must_use]
    pub fn requires_continuation(&self) -> bool {
        matches!(self.stop.reason.as_str(), "tool_use" | "pause_turn")
    }

    #[allow(
        clippy::large_types_passed_by_value,
        reason = "the exact immutable producer fact set is consumed by the neutral candidate"
    )]
    pub(crate) fn seal_candidate(
        &self,
        producer: FamilyArtifactProducerFacts,
    ) -> Result<SealedFamilyArtifactCandidate, MessagesArtifactError> {
        SealedFamilyArtifactCandidate::new(
            FAMILY,
            MESSAGES_ARTIFACT_SCHEMA_REVISION,
            MESSAGES_ARTIFACT_CODEC_REVISION,
            self.binding,
            producer,
            Arc::<[u8]>::from(self.encode()?),
        )
        .map_err(|_: FamilyArtifactError| MessagesArtifactError::Candidate)
    }
}

pub(crate) fn native_blocks_match_canonical(
    native: &[MessageBlock],
    canonical: &[MessageBlock],
) -> bool {
    if native.len() != canonical.len() {
        return false;
    }
    native.iter().zip(canonical).all(|(native, canonical)| {
        let mut normalized = native.clone();
        let explicit_direct = matches!(
            &normalized,
            MessageBlock::ToolUse {
                caller: Some(caller),
                ..
            } if caller.kind == "direct" && caller.tool_id.is_none()
        );
        if explicit_direct && let MessageBlock::ToolUse { caller, .. } = &mut normalized {
            *caller = None;
        }
        &normalized == canonical
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "artifact fixtures fail fast on identity and codec construction"
)]
mod tests;
