//! Replay envelopes and the opaque and compaction blocks that carry them.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::{Digest, DigestDomain, DigestError, ModelFamilyId, ModelId, ProtocolId, ReplayKindId};

use super::{ContentError, MAX_CANONICAL_BLOCK_BYTES, ensure_maximum, ensure_nonempty};

/// The compatibility scope of opaque replay material.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReplayScope {
    /// Compatible with the same protocol dialect.
    Protocol,
    /// Compatible only with the declared model family.
    ModelFamily,
    /// Compatible only with one exact model identity.
    ExactModel(ModelId),
    /// Compatible only with one exact account and model identity.
    ExactAccountModel {
        /// The producing model identity.
        model: ModelId,
        /// The producing account's non-secret binding.
        account: Digest,
    },
}

/// The data-sensitivity classification of replay material.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum DataClassification {
    /// Data approved for ordinary display.
    Public,
    /// Data requiring log and telemetry redaction.
    Sensitive,
    /// Secret material requiring the strongest storage policy.
    Secret,
}

/// Sensitive protocol replay material with a verified, scoped digest.
#[derive(Clone, Serialize)]
pub struct ReplayEnvelope {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    scope: ReplayScope,
    kind: ReplayKindId,
    version: u32,
    classification: DataClassification,
    digest: Digest,
    payload: Box<RawValue>,
}

impl PartialEq for ReplayEnvelope {
    fn eq(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.model_family == other.model_family
            && self.scope == other.scope
            && self.kind == other.kind
            && self.version == other.version
            && self.classification == other.classification
            && self.digest == other.digest
            && self.payload.get() == other.payload.get()
    }
}

impl ReplayEnvelope {
    /// Constructs an envelope and hashes its exact raw payload and identity.
    ///
    /// # Errors
    ///
    /// Returns an error for version zero, a missing required family scope, or a
    /// digest construction failure.
    pub fn new(
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        scope: ReplayScope,
        kind: ReplayKindId,
        version: u32,
        classification: DataClassification,
        payload: Box<RawValue>,
    ) -> Result<Self, ContentError> {
        if version == 0 {
            return Err(ContentError::ZeroReplayVersion);
        }
        ensure_maximum(
            payload.get().len(),
            "replay payload",
            MAX_CANONICAL_BLOCK_BYTES,
        )?;
        if matches!(scope, ReplayScope::ModelFamily) && model_family.is_none() {
            return Err(ContentError::Empty {
                field: "model family for family-scoped replay",
            });
        }
        let digest = replay_digest(
            &protocol,
            model_family.as_ref(),
            &scope,
            &kind,
            version,
            classification,
            &payload,
        )?;
        Ok(Self {
            protocol,
            model_family,
            scope,
            kind,
            version,
            classification,
            digest,
            payload,
        })
    }

    /// Returns the protocol scope.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the optional model-family scope.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the scope rule.
    #[must_use]
    pub const fn scope(&self) -> &ReplayScope {
        &self.scope
    }

    /// Returns the replay kind.
    #[must_use]
    pub const fn kind(&self) -> &ReplayKindId {
        &self.kind
    }

    /// Returns the replay format version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Returns the sensitivity classification.
    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    /// Returns the verified replay digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Returns the exact raw JSON payload.
    #[must_use]
    pub fn payload(&self) -> &RawValue {
        &self.payload
    }

    /// Reports whether the target binding may consume this replay material.
    /// Supplying an account requires an exact account/model scope; an omitted
    /// account never authorizes account-scoped material.
    #[must_use]
    pub fn is_compatible(
        &self,
        protocol: &ProtocolId,
        model_family: Option<&ModelFamilyId>,
        model: &ModelId,
        account: Option<Digest>,
    ) -> bool {
        if &self.protocol != protocol {
            return false;
        }
        match (&self.scope, account) {
            (
                ReplayScope::ExactAccountModel {
                    model: expected,
                    account: producing,
                },
                Some(account),
            ) => {
                expected == model
                    && *producing == account
                    && self.model_family.as_ref() == model_family
            }
            (ReplayScope::ExactAccountModel { .. }, None) | (_, Some(_)) => false,
            (ReplayScope::Protocol, None) => true,
            (ReplayScope::ModelFamily, None) => self.model_family.as_ref() == model_family,
            (ReplayScope::ExactModel(expected), None) => expected == model,
        }
    }
}

impl fmt::Debug for ReplayEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayEnvelope")
            .field("protocol", &self.protocol)
            .field("model_family", &self.model_family)
            .field("scope", &self.scope)
            .field("kind", &self.kind)
            .field("version", &self.version)
            .field("classification", &self.classification)
            .field("digest", &self.digest)
            .field("payload", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct ReplayEnvelopeWire {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    scope: ReplayScope,
    kind: ReplayKindId,
    version: u32,
    classification: DataClassification,
    digest: Digest,
    payload: Box<RawValue>,
}

impl<'de> Deserialize<'de> for ReplayEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReplayEnvelopeWire::deserialize(deserializer)?;
        let envelope = Self::new(
            wire.protocol,
            wire.model_family,
            wire.scope,
            wire.kind,
            wire.version,
            wire.classification,
            wire.payload,
        )
        .map_err(serde::de::Error::custom)?;
        if envelope.digest != wire.digest {
            return Err(serde::de::Error::custom(ContentError::ReplayDigestMismatch));
        }
        Ok(envelope)
    }
}

fn replay_digest(
    protocol: &ProtocolId,
    model_family: Option<&ModelFamilyId>,
    scope: &ReplayScope,
    kind: &ReplayKindId,
    version: u32,
    classification: DataClassification,
    payload: &RawValue,
) -> Result<Digest, DigestError> {
    let family = model_family.map_or("", ModelFamilyId::as_str);
    if let ReplayScope::ExactAccountModel { model, account } = scope {
        return Digest::sha256(
            DigestDomain::ReplayEnvelope,
            3,
            &[
                protocol.as_str().as_bytes(),
                family.as_bytes(),
                b"account-model",
                model.as_str().as_bytes(),
                &account.identity_bytes(),
                kind.as_str().as_bytes(),
                &version.to_be_bytes(),
                &[match classification {
                    DataClassification::Public => 0,
                    DataClassification::Sensitive => 1,
                    DataClassification::Secret => 2,
                }],
                payload.get().as_bytes(),
            ],
        );
    }
    let (scope_tag, exact_model) = match scope {
        ReplayScope::Protocol => (&[0_u8][..], ""),
        ReplayScope::ModelFamily => (&[1_u8][..], ""),
        ReplayScope::ExactModel(model) => (&[2_u8][..], model.as_str()),
        ReplayScope::ExactAccountModel { model, .. } => (&[3_u8][..], model.as_str()),
    };
    let classification_text = match classification {
        DataClassification::Public => "public",
        DataClassification::Sensitive => "sensitive",
        DataClassification::Secret => "secret",
    };
    Digest::sha256(
        DigestDomain::ReplayEnvelope,
        2,
        &[
            protocol.as_str().as_bytes(),
            family.as_bytes(),
            scope_tag,
            exact_model.as_bytes(),
            kind.as_str().as_bytes(),
            &version.to_be_bytes(),
            classification_text.as_bytes(),
            payload.get().as_bytes(),
        ],
    )
}

/// The cross-protocol fallback policy for opaque content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OpaqueFallback {
    /// Omit the block on incompatible targets.
    Omit,
    /// Preserve only for a compatible replay target.
    CompatibleReplay,
    /// Use this documented semantic summary on incompatible targets.
    SemanticSummary(String),
}

/// A scoped opaque content block.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OpaqueBlock {
    replay: ReplayEnvelope,
    fallback: OpaqueFallback,
}

#[derive(Deserialize)]
struct OpaqueBlockWire {
    replay: ReplayEnvelope,
    fallback: OpaqueFallback,
}

impl<'de> Deserialize<'de> for OpaqueBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OpaqueBlockWire::deserialize(deserializer)?;
        Self::new(wire.replay, wire.fallback).map_err(serde::de::Error::custom)
    }
}

impl OpaqueBlock {
    /// Creates an opaque block with an explicit fallback policy.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when a semantic fallback summary is empty.
    pub fn new(replay: ReplayEnvelope, fallback: OpaqueFallback) -> Result<Self, ContentError> {
        if matches!(&fallback, OpaqueFallback::SemanticSummary(summary) if summary.is_empty()) {
            return Err(ContentError::Empty {
                field: "opaque semantic summary",
            });
        }
        if let OpaqueFallback::SemanticSummary(summary) = &fallback {
            ensure_maximum(
                summary.len(),
                "opaque semantic summary",
                MAX_CANONICAL_BLOCK_BYTES,
            )?;
        }
        Ok(Self { replay, fallback })
    }

    /// Returns scoped replay material.
    #[must_use]
    pub const fn replay(&self) -> &ReplayEnvelope {
        &self.replay
    }

    /// Returns the incompatible-target fallback.
    #[must_use]
    pub const fn fallback(&self) -> &OpaqueFallback {
        &self.fallback
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.replay.payload.get().len()
            + match &self.fallback {
                OpaqueFallback::SemanticSummary(summary) => summary.len(),
                OpaqueFallback::Omit | OpaqueFallback::CompatibleReplay => 0,
            }
    }
}

/// Exact producing target for a native compaction item, independent of aggregate continuation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompactionReplayBinding {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    model: ModelId,
    account: Digest,
}

impl CompactionReplayBinding {
    /// The producing protocol, for portable neutral encodings.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// The producing model family, when one was declared.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// The exact producing model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// The producing account binding.
    #[must_use]
    pub const fn account(&self) -> Digest {
        self.account
    }

    /// Records the admitted producing dialect, family, requested model and account.
    #[must_use]
    pub const fn new(
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        model: ModelId,
        account: Digest,
    ) -> Self {
        Self {
            protocol,
            model_family,
            model,
            account,
        }
    }

    /// Proves equality with the current admitted target; freshness is not replay identity.
    #[must_use]
    pub fn is_compatible(
        &self,
        protocol: &ProtocolId,
        model_family: Option<&ModelFamilyId>,
        model: &ModelId,
        account: Digest,
    ) -> bool {
        &self.protocol == protocol
            && self.model_family.as_ref() == model_family
            && &self.model == model
            && self.account == account
    }
}

/// Root turns a compaction removed from canonical history.
///
/// Counted like the turns still in history: `turns` covers every root turn,
/// `prompts` only those a person submitted. The counts include any earlier
/// compaction the removed history already carried, so a history's turn
/// position survives being summarized.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizedTurns {
    prompts: u64,
    turns: u64,
}

impl SummarizedTurns {
    /// Creates counts of summarized root turns; `prompts` never exceeds `turns`.
    #[must_use]
    pub const fn new(prompts: u64, turns: u64) -> Option<Self> {
        if prompts > turns {
            return None;
        }
        Some(Self { prompts, turns })
    }

    /// Returns the summarized turns a person submitted.
    #[must_use]
    pub const fn prompts(self) -> u64 {
        self.prompts
    }

    /// Returns every summarized root turn.
    #[must_use]
    pub const fn turns(self) -> u64 {
        self.turns
    }
}

/// A canonical compaction marker and semantic summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompactionBlock {
    summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_binding: Option<CompactionReplayBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summarized_turns: Option<SummarizedTurns>,
}

#[derive(Deserialize)]
struct CompactionBlockWire {
    summary: String,
    #[serde(default)]
    native_item: Option<String>,
    #[serde(default)]
    native_binding: Option<CompactionReplayBinding>,
    #[serde(default)]
    summarized_turns: Option<SummarizedTurns>,
}

impl<'de> Deserialize<'de> for CompactionBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CompactionBlockWire::deserialize(deserializer)?;
        let mut block = Self::new(wire.summary).map_err(serde::de::Error::custom)?;
        if let Some(native_item) = wire.native_item {
            block = block.with_native_item(native_item);
        }
        block.native_binding = wire.native_binding;
        if let Some(turns) = wire.summarized_turns {
            if turns.prompts > turns.turns {
                return Err(serde::de::Error::custom(
                    "summarized prompts exceed summarized turns",
                ));
            }
            block.summarized_turns = Some(turns);
        }
        Ok(block)
    }
}

impl CompactionBlock {
    /// Creates a non-empty compaction summary.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when the summary is empty.
    pub fn new(summary: impl Into<String>) -> Result<Self, ContentError> {
        let summary = summary.into();
        ensure_nonempty(&summary, "compaction summary")?;
        ensure_maximum(
            summary.len(),
            "compaction summary",
            MAX_CANONICAL_BLOCK_BYTES,
        )?;
        Ok(Self {
            summary,
            native_item: None,
            native_binding: None,
            summarized_turns: None,
        })
    }

    /// Records the root turns this compaction removed from history.
    #[must_use]
    pub const fn with_summarized_turns(mut self, turns: SummarizedTurns) -> Self {
        self.summarized_turns = Some(turns);
        self
    }

    /// Returns the root turns this compaction removed, when it recorded them.
    #[must_use]
    pub const fn summarized_turns(&self) -> Option<SummarizedTurns> {
        self.summarized_turns
    }

    /// Attaches the exact provider compaction JSON for later verbatim replay.
    #[must_use]
    pub fn with_native_item(mut self, native_item: String) -> Self {
        self.native_item = Some(native_item);
        self
    }

    /// Binds native replay to the provider-neutral producing target.
    #[must_use]
    pub fn with_native_binding(mut self, binding: CompactionReplayBinding) -> Self {
        self.native_binding = Some(binding);
        self
    }

    /// Returns the producing binding, if native replay has independently retained authority.
    #[must_use]
    pub const fn native_binding(&self) -> Option<&CompactionReplayBinding> {
        self.native_binding.as_ref()
    }

    /// Returns the semantic summary.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Returns native compaction JSON when this marker is a provider item.
    #[must_use]
    pub fn native_item(&self) -> Option<&str> {
        self.native_item.as_deref()
    }
}
