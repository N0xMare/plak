//! Model call preparation: cache affinity, token projection, output limits and
//! prepared projections.

use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use super::authority::{RuntimeModelAuthority, RuntimeModelAuthorityError};
use super::family_artifact::ActiveFamilyArtifact;
use super::prepared_call::PreparedCallError;
use crate::{
    AttemptId, BranchId, ContextAdmissionError, ContextStatus, CountObservation, Digest,
    DigestAlgorithm, DigestDomain, EffectiveProviderConfigurationId, ModelAdapterProfileId,
    ModelId, ModelOperationRoot, ProtocolId, ResolvedModelDescriptor, RouteId, SafeDiagnostic,
    SelectedProviderConfigurationId, SessionId, Timestamp, ValidatedRequestPlan, ValidationError,
};

mod projection;

/// Inspectable non-secret projection of one exact provider preparation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreparedModelProjection {
    pub(super) plan: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    pub(super) descriptor_binding: Digest,
    pub(super) request: Digest,
    pub(super) model_input: Digest,
    pub(super) operation_root: ModelOperationRoot,
    pub(super) attempt_id: AttemptId,
    protocol_family: ProtocolId,
    pub(super) dialect: ProtocolId,
    pub(super) route: RouteId,
    pub(super) authority: Digest,
    pub(super) authority_class: Digest,
    requested_model: ModelId,
    pub(super) effective_model: ModelId,
    adapter: ModelAdapterProfileId,
    pub(super) lowering_revision: String,
    projection_revision: String,
    accounting_revision: String,
    wire_digest: Digest,
    wire_size: u64,
    pub(super) token_projection: PreparedTokenProjection,
    active_artifact: Option<Digest>,
    cache_affinity: Option<Digest>,
    cache_use: PreparedCacheUse,
    output_limit: Option<PreparedOutputLimit>,
    pub(super) identity: Digest,
}

#[derive(Serialize)]
struct PreparedModelProjectionContent<'a> {
    plan: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    descriptor_binding: Digest,
    request: Digest,
    model_input: Digest,
    operation_root: ModelOperationRoot,
    attempt_id: AttemptId,
    protocol_family: &'a ProtocolId,
    dialect: &'a ProtocolId,
    route: &'a RouteId,
    authority: Digest,
    authority_class: Digest,
    requested_model: &'a ModelId,
    effective_model: &'a ModelId,
    adapter: &'a ModelAdapterProfileId,
    lowering_revision: &'a str,
    projection_revision: &'a str,
    accounting_revision: &'a str,
    wire_digest: Digest,
    wire_size: u64,
    token_projection: Digest,
    active_artifact: Option<Digest>,
    cache_affinity: Option<Digest>,
    cache_use: PreparedCacheUse,
    output_limit: Option<Digest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedModelProjectionWire {
    plan: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    descriptor_binding: Digest,
    request: Digest,
    model_input: Digest,
    operation_root: ModelOperationRoot,
    attempt_id: AttemptId,
    protocol_family: ProtocolId,
    dialect: ProtocolId,
    route: RouteId,
    authority: Digest,
    authority_class: Digest,
    requested_model: ModelId,
    effective_model: ModelId,
    adapter: ModelAdapterProfileId,
    lowering_revision: String,
    projection_revision: String,
    accounting_revision: String,
    wire_digest: Digest,
    wire_size: u64,
    token_projection: PreparedTokenProjection,
    active_artifact: Option<Digest>,
    cache_affinity: Option<Digest>,
    cache_use: PreparedCacheUse,
    output_limit: Option<PreparedOutputLimit>,
    identity: Digest,
}

/// Maximum exact credential-free wire body represented by one preparation.
pub const MAX_PREPARED_WIRE_BYTES: u64 = 32 * 1024 * 1024;

const MAX_PREPARATION_LABEL_BYTES: usize = 128;

/// Runtime-owned, redaction-protected identity for one branch cache epoch.
///
/// The bounded opaque value is retained and serialized so decode can recompute
/// the session/branch/epoch binding; `Debug` and ordinary accessors never expose
/// it. The derived identity is deliberately separate from account and credential
/// authority: it grants neither authentication nor route access.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelCacheAffinity {
    session: SessionId,
    branch: BranchId,
    epoch: u64,
    opaque: [u8; 32],
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCacheAffinityWire {
    session: SessionId,
    branch: BranchId,
    epoch: u64,
    opaque: [u8; 32],
    identity: Digest,
}

impl ModelCacheAffinity {
    /// Derives one affinity from runtime entropy and its exact branch epoch.
    ///
    /// # Errors
    ///
    /// Rejects an all-zero entropy value or digest construction failure.
    pub fn from_runtime_entropy(
        session: SessionId,
        branch: BranchId,
        epoch: u64,
        entropy: [u8; 32],
    ) -> Result<Self, ModelCacheAffinityError> {
        if entropy.iter().all(|byte| *byte == 0) {
            return Err(ModelCacheAffinityError::InvalidEntropy);
        }
        let identity = cache_affinity_identity(session, branch, epoch, &entropy)?;
        Ok(Self {
            session,
            branch,
            epoch,
            opaque: entropy,
            identity,
        })
    }

    /// Returns the bound session identity.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the bound branch identity.
    #[must_use]
    pub const fn branch(&self) -> BranchId {
        self.branch
    }

    /// Returns the cache-affinity epoch.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the redacted non-secret affinity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ModelCacheAffinity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelCacheAffinityWire::deserialize(deserializer)?;
        if wire.opaque.iter().all(|byte| *byte == 0) {
            return Err(serde::de::Error::custom(
                ModelCacheAffinityError::InvalidEntropy,
            ));
        }
        let expected = cache_affinity_identity(wire.session, wire.branch, wire.epoch, &wire.opaque)
            .map_err(serde::de::Error::custom)?;
        if expected != wire.identity {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
        Ok(Self {
            session: wire.session,
            branch: wire.branch,
            epoch: wire.epoch,
            opaque: wire.opaque,
            identity: wire.identity,
        })
    }
}

impl fmt::Debug for ModelCacheAffinity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCacheAffinity")
            .field("epoch", &self.epoch)
            .field("identity", &"<redacted cache affinity>")
            .finish_non_exhaustive()
    }
}

/// Invalid runtime cache-affinity construction.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ModelCacheAffinityError {
    /// Runtime entropy must not be the all-zero sentinel.
    #[error("cache-affinity entropy is invalid")]
    InvalidEntropy,
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] crate::DigestError),
}

fn cache_affinity_identity(
    session: SessionId,
    branch: BranchId,
    epoch: u64,
    opaque: &[u8; 32],
) -> Result<Digest, crate::DigestError> {
    let session_text = session.to_string();
    let branch_text = branch.to_string();
    Digest::sha256(
        DigestDomain::CacheAffinity,
        1,
        &[
            b"plakous-model/cache-affinity-v1",
            session_text.as_bytes(),
            branch_text.as_bytes(),
            &epoch.to_be_bytes(),
            opaque,
        ],
    )
}

/// The 1-based position of a root turn within its logical session.
///
/// `turn_index` counts every root turn, `prompt_index` only those a person
/// submitted, through the current turn. Both derive from canonical history,
/// including the turns a compaction summarized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnPosition {
    prompt_index: u64,
    turn_index: u64,
}

impl TurnPosition {
    /// Creates a position; `turn_index` is at least 1 and `prompt_index` at most it.
    #[must_use]
    pub const fn new(prompt_index: u64, turn_index: u64) -> Option<Self> {
        if turn_index == 0 || prompt_index > turn_index {
            return None;
        }
        Some(Self {
            prompt_index,
            turn_index,
        })
    }

    /// Returns the number of person-submitted root turns through this one.
    #[must_use]
    pub const fn prompt_index(self) -> u64 {
        self.prompt_index
    }

    /// Returns the number of root turns through this one.
    #[must_use]
    pub const fn turn_index(self) -> u64 {
        self.turn_index
    }
}

/// A provider response id; debug output never prints it.
#[derive(Clone)]
struct ProviderResponseId(String);

impl fmt::Debug for ProviderResponseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderResponseId(..)")
    }
}

/// Fresh non-secret inputs fixed before provider-specific lowering begins.
#[derive(Clone, Debug)]
pub struct ModelPreparationContext {
    pub(super) authority: RuntimeModelAuthority,
    pub(super) prepared_at: Timestamp,
    active_family_artifact: Option<ActiveFamilyArtifact>,
    cache_affinity: Option<ModelCacheAffinity>,
    previous_response_metadata: Option<crate::ModelResponseMetadata>,
    previous_response_id: Option<ProviderResponseId>,
    turn_position: Option<TurnPosition>,
    completed_history_prefix: Option<crate::ConversationPrefixIdentity>,
    request_anchor: Option<crate::ContextRequestAnchor>,
    require_input_bound: bool,
}

impl ModelPreparationContext {
    /// Creates one exact preparation context from freshly revalidated authority.
    #[must_use]
    pub const fn new(authority: RuntimeModelAuthority, prepared_at: Timestamp) -> Self {
        Self {
            authority,
            prepared_at,
            active_family_artifact: None,
            cache_affinity: None,
            previous_response_metadata: None,
            previous_response_id: None,
            turn_position: None,
            completed_history_prefix: None,
            request_anchor: None,
            require_input_bound: false,
        }
    }

    /// Supplies a completed-response anchor for the exact current binding.
    #[must_use]
    pub fn with_request_anchor(mut self, anchor: Option<crate::ContextRequestAnchor>) -> Self {
        self.request_anchor = anchor;
        self
    }

    /// Requires a conservative fit for a paid fold's own input.
    #[must_use]
    pub const fn with_required_input_bound(mut self) -> Self {
        self.require_input_bound = true;
        self
    }

    /// Supplies the exact request prefix proven to belong to closed root turns.
    ///
    /// The runtime must derive this boundary from lifecycle authority, never
    /// message roles: steering and assistant continuations do not close a turn.
    /// Providers must match the identity against the request before using it.
    #[must_use]
    pub const fn with_completed_history_prefix(
        mut self,
        prefix: crate::ConversationPrefixIdentity,
    ) -> Self {
        self.completed_history_prefix = Some(prefix);
        self
    }

    /// Returns the runtime-proven closed-turn prefix, absent by default.
    #[must_use]
    pub const fn completed_history_prefix(&self) -> Option<crate::ConversationPrefixIdentity> {
        self.completed_history_prefix
    }

    /// Supplies metadata from the response that produced the latest retained assistant entry.
    ///
    /// The runtime must prove the exact entry identity and matching descriptor,
    /// provider configuration, account, and credential authority from durable dispatch
    /// evidence. Missing or ambiguous provenance must remain absent.
    #[must_use]
    pub fn with_previous_response_metadata(
        mut self,
        metadata: crate::ModelResponseMetadata,
    ) -> Self {
        self.previous_response_metadata = Some(metadata);
        self
    }

    /// Supplies the provider response id of that same runtime-proven response.
    #[must_use]
    pub fn with_previous_response_id(mut self, response_id: Option<String>) -> Self {
        self.previous_response_id = response_id.map(ProviderResponseId);
        self
    }

    /// Returns the provider response id of the runtime-proven previous response.
    #[must_use]
    pub fn previous_response_id(&self) -> Option<&str> {
        self.previous_response_id
            .as_ref()
            .map(|ProviderResponseId(id)| id.as_str())
    }

    /// Supplies the current root turn's position in its logical session.
    #[must_use]
    pub const fn with_turn_position(mut self, position: Option<TurnPosition>) -> Self {
        self.turn_position = position;
        self
    }

    /// Returns the current root turn's position, when canonical history proves it.
    #[must_use]
    pub const fn turn_position(&self) -> Option<TurnPosition> {
        self.turn_position
    }

    /// Returns runtime-proven response provenance for this preparation.
    #[must_use]
    pub const fn previous_response_metadata(&self) -> Option<&crate::ModelResponseMetadata> {
        self.previous_response_metadata.as_ref()
    }

    /// Fixes the sole active family artifact before lowering.
    #[must_use]
    pub fn with_active_family_artifact(mut self, artifact: ActiveFamilyArtifact) -> Self {
        self.active_family_artifact = Some(artifact);
        self
    }

    /// Fixes the runtime-owned branch cache affinity before lowering.
    #[must_use]
    pub fn with_cache_affinity(mut self, affinity: ModelCacheAffinity) -> Self {
        self.cache_affinity = Some(affinity);
        self
    }

    /// Returns the fresh non-secret runtime authority.
    #[must_use]
    pub const fn authority(&self) -> &RuntimeModelAuthority {
        &self.authority
    }

    /// Returns the injected preparation time used for freshness checks.
    #[must_use]
    pub const fn prepared_at(&self) -> Timestamp {
        self.prepared_at
    }

    /// Returns the sole active artifact fixed for this lowering.
    #[must_use]
    pub const fn active_family_artifact(&self) -> Option<&ActiveFamilyArtifact> {
        self.active_family_artifact.as_ref()
    }

    /// Returns the fixed runtime-owned cache affinity, when enabled.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<&ModelCacheAffinity> {
        self.cache_affinity.as_ref()
    }
}

/// Exact cache behavior selected by one prepared lowering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum PreparedCacheUse {
    /// No provider-visible cache facility is used.
    None,
    /// Reuse is authorized solely by the fixed active family artifact.
    ActiveFamilyArtifact {
        /// Integrity identity of the exact active artifact.
        artifact: Digest,
    },
    /// The exact runtime-owned branch affinity was emitted on the wire.
    RuntimeAffinity {
        /// Redacted identity of the exact branch affinity.
        affinity: Digest,
    },
    /// Both the active family artifact and runtime affinity were used.
    ActiveFamilyArtifactAndRuntimeAffinity {
        /// Integrity identity of the exact active artifact.
        artifact: Digest,
        /// Redacted identity of the exact branch affinity.
        affinity: Digest,
    },
}

/// Complete prepared token projection plus an optional release-verified count fact.
///
/// The projection identity is derived from the exact credential-free wire body,
/// not from a caller estimate or a generic canonical request.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedTokenProjection {
    wire_digest: Digest,
    wire_size: u64,
    observation: Option<CountObservation>,
    identity: Digest,
    /// Byte composition of the same wire body, when the preparing provider
    /// measured it. Presentation-only and outside the identity: the digest
    /// and size already bind the exact body the parts describe.
    input_parts: Option<crate::ContextInputParts>,
    /// Item measurements of the same body, held only by the live preparation.
    /// A round's record references them by `wire_input_identity`, so a
    /// persisted projection costs O(1) in the request's item count.
    #[serde(skip)]
    wire_input: Option<crate::ContextWireInput>,
    wire_input_identity: Option<Digest>,
    #[serde(skip)]
    request_anchor: Option<crate::ContextRequestAnchor>,
    request_anchor_identity: Option<Digest>,
    require_input_bound: bool,
}

impl PartialEq for PreparedTokenProjection {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity && self.input_parts == other.input_parts
    }
}
impl Eq for PreparedTokenProjection {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedTokenProjectionWire {
    wire_digest: Digest,
    wire_size: u64,
    observation: Option<CountObservation>,
    identity: Digest,
    #[serde(default)]
    input_parts: Option<crate::ContextInputParts>,
    wire_input_identity: Option<Digest>,
    request_anchor_identity: Option<Digest>,
    require_input_bound: bool,
}

impl PreparedTokenProjection {
    /// Binds count evidence, when present, to one exact complete lowered body.
    ///
    /// # Errors
    ///
    /// Rejects an invalid body digest/size or integrity construction failure.
    pub fn new(
        wire_digest: Digest,
        wire_size: u64,
        observation: Option<CountObservation>,
    ) -> Result<Self, PreparedCallError> {
        validate_prepared_wire(wire_digest, wire_size)?;
        let observation_bytes =
            serde_json::to_vec(&observation).map_err(|_| PreparedCallError::Serialization)?;
        let identity = Digest::sha256(
            DigestDomain::TokenMeasurement,
            4,
            &[
                b"plakous-model/prepared-token-projection-v1",
                &wire_digest.identity_bytes(),
                &wire_size.to_be_bytes(),
                &observation_bytes,
            ],
        )?;
        Ok(Self {
            wire_digest,
            wire_size,
            observation,
            identity,
            input_parts: None,
            wire_input: None,
            wire_input_identity: None,
            request_anchor: None,
            request_anchor_identity: None,
            require_input_bound: false,
        })
    }

    /// Attaches the byte composition of the exact body this projection binds.
    #[must_use]
    pub const fn with_input_parts(mut self, input_parts: Option<crate::ContextInputParts>) -> Self {
        self.input_parts = input_parts;
        self
    }

    /// Binds item measurements and admission authority to this wire projection.
    ///
    /// # Errors
    /// Rejects unknown image bounds and failed integrity construction.
    pub fn with_wire_input(
        mut self,
        body: &[u8],
        image_maximum: Option<u64>,
        context: &ModelPreparationContext,
    ) -> Result<Self, ModelPreparationError> {
        let input = crate::ContextWireInput::measure_wire(body, image_maximum)
            .map_err(ContextAdmissionError::from)?;
        self.wire_input_identity = Some(input.identity().map_err(ContextAdmissionError::from)?);
        self.wire_input = Some(input);
        self.input_parts = self.input_parts.map(|parts| {
            parts.with_image_tokens(
                self.wire_input
                    .as_ref()
                    .map_or(0, crate::ContextWireInput::image_tokens),
            )
        });
        self.request_anchor.clone_from(&context.request_anchor);
        if let (Some(anchor), Some(input)) = (&mut self.request_anchor, &self.wire_input) {
            anchor.observe_response_items(input);
        }
        self.request_anchor_identity = self
            .request_anchor
            .as_ref()
            .map(crate::ContextRequestAnchor::identity)
            .transpose()
            .map_err(|_| PreparedCallError::Serialization)?;
        self.require_input_bound = context.require_input_bound;
        self.bind_input_identity()?;
        Ok(self)
    }

    fn bind_input_identity(&mut self) -> Result<(), PreparedCallError> {
        if self.wire_input.is_some() {
            let bytes = serde_json::to_vec(&(
                &self.wire_input,
                &self.request_anchor_identity,
                self.require_input_bound,
            ))
            .map_err(|_| PreparedCallError::Serialization)?;
            self.identity = Digest::sha256(
                DigestDomain::TokenMeasurement,
                5,
                &[&self.identity.identity_bytes(), &bytes],
            )?;
        }
        Ok(())
    }

    /// Returns measured input items, when the dialect supplies them and this
    /// is the live preparation; a decoded projection only references them.
    #[must_use]
    pub const fn wire_input(&self) -> Option<&crate::ContextWireInput> {
        self.wire_input.as_ref()
    }

    /// Whether this projection was measured from items it no longer holds: a
    /// decoded record references its item list (and any request anchor, which
    /// decode admits only beside that reference) by identity. Such a
    /// projection cannot be measured again.
    pub(crate) const fn is_recorded(&self) -> bool {
        self.wire_input_identity.is_some() && self.wire_input.is_none()
    }

    /// Returns the persisted reference to the measured item list.
    #[cfg(test)]
    pub(crate) const fn wire_input_identity(&self) -> Option<Digest> {
        self.wire_input_identity
    }

    /// Length of `record_bytes`, a record serialized with this projection, if
    /// it embedded the measured item list (or its absence) in place of the
    /// reference. A decoded measured projection holds no list to count.
    pub(super) fn embedded_record_bytes(
        &self,
        record_bytes: usize,
    ) -> Result<usize, PreparedCallError> {
        if self.is_recorded() {
            return Ok(record_bytes);
        }
        let list = serde_json::to_vec(&self.wire_input)
            .map_err(|_| PreparedCallError::Serialization)?
            .len()
            .saturating_add(br#""wire_input":"#.len());
        let reference = serde_json::to_vec(&self.wire_input_identity)
            .map_err(|_| PreparedCallError::Serialization)?
            .len()
            .saturating_add(br#""wire_input_identity":"#.len());
        Ok(record_bytes.saturating_add(list).saturating_sub(reference))
    }

    /// Returns the completed-response anchor bound into this projection.
    #[must_use]
    pub const fn request_anchor(&self) -> Option<&crate::ContextRequestAnchor> {
        self.request_anchor.as_ref()
    }
    pub(crate) const fn require_input_bound(&self) -> bool {
        self.require_input_bound
    }

    /// Returns the byte composition of the bound body, when measured.
    #[must_use]
    pub const fn input_parts(&self) -> Option<crate::ContextInputParts> {
        self.input_parts
    }

    /// Returns the exact credential-free wire digest.
    #[must_use]
    pub const fn wire_digest(&self) -> Digest {
        self.wire_digest
    }

    /// Returns the exact credential-free wire size.
    #[must_use]
    pub const fn wire_size(&self) -> u64 {
        self.wire_size
    }

    /// Returns the count candidate to be checked against descriptor evidence.
    #[must_use]
    pub const fn observation(&self) -> Option<&CountObservation> {
        self.observation.as_ref()
    }

    /// Returns the complete prepared token-projection identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for PreparedTokenProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreparedTokenProjectionWire::deserialize(deserializer)?;
        let mut value = Self::new(wire.wire_digest, wire.wire_size, wire.observation)
            .map_err(serde::de::Error::custom)?;
        let measured = |digest: Option<Digest>| {
            digest.is_none_or(|digest| {
                validate_prepared_digest(digest, DigestDomain::TokenMeasurement).is_ok()
                    && digest.version() == 5
            })
        };
        let consistent = match wire.wire_input_identity {
            // An unmeasured projection binds no item, anchor or bound
            // authority, so its identity is recomputed from the body facts.
            None => {
                value.identity == wire.identity
                    && wire.request_anchor_identity.is_none()
                    && !wire.require_input_bound
            }
            // A measured identity binds the item list this record references
            // but does not carry, so it is not re-derived here: the enclosing
            // record identity integrity-binds it with every retained byte. The
            // three references name distinct objects and never coincide.
            Some(reference) => {
                measured(Some(reference))
                    && measured(Some(wire.identity))
                    && measured(wire.request_anchor_identity)
                    && wire.identity != reference
                    && wire.request_anchor_identity != Some(reference)
                    && wire.request_anchor_identity != Some(wire.identity)
            }
        };
        if !consistent {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
        value.wire_input_identity = wire.wire_input_identity;
        value.request_anchor_identity = wire.request_anchor_identity;
        value.require_input_bound = wire.require_input_bound;
        value.identity = wire.identity;
        Ok(value.with_input_parts(wire.input_parts))
    }
}

/// Provider-owned source of one authoritative prepared output limit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PreparedOutputLimitSource {
    /// The exact serialized request emits and enforces this maximum.
    EnforcedWireMaximum {
        /// Inspected provider wire field.
        wire_field: String,
        /// Exact credential-free body carrying the field.
        wire_digest: Digest,
        /// Lowering implementation revision which enforces the field.
        lowering_revision: String,
    },
    /// Exact descriptor-owned evidence certifies a provider hard cap.
    ReleaseVerifiedHardCap {
        /// Identity of the reviewed exact-scope certificate.
        certificate: Digest,
        /// Complete descriptor/route/authority/model/revision scope identity.
        scope: Digest,
    },
}

/// Proof of one authoritative nonzero output maximum for a prepared call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreparedOutputLimit {
    tokens: NonZeroU32,
    source: PreparedOutputLimitSource,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedOutputLimitWire {
    tokens: NonZeroU32,
    source: PreparedOutputLimitSource,
    identity: Digest,
}

impl PreparedOutputLimit {
    /// Records an inspected, emitted, and enforced wire maximum.
    ///
    /// # Errors
    ///
    /// Rejects invalid labels, a foreign wire digest, or identity failure.
    pub fn enforced_wire_maximum(
        tokens: NonZeroU32,
        wire_field: impl Into<String>,
        wire_digest: Digest,
        lowering_revision: impl Into<String>,
    ) -> Result<Self, PreparedCallError> {
        let wire_field = wire_field.into();
        let lowering_revision = lowering_revision.into();
        validate_preparation_label(&wire_field)?;
        validate_preparation_label(&lowering_revision)?;
        validate_prepared_digest(wire_digest, DigestDomain::Blob)?;
        Self::seal(
            tokens,
            PreparedOutputLimitSource::EnforcedWireMaximum {
                wire_field,
                wire_digest,
                lowering_revision,
            },
        )
    }

    /// Records an exact-scope provider hard-cap certificate.
    ///
    /// The descriptor contract remains the authority for whether the named
    /// certificate actually certifies this token cap. Context admission checks
    /// that fact again before using it as a reservation.
    ///
    /// # Errors
    ///
    /// Rejects stale/foreign authority, invalid revisions, or certificate
    /// identities outside the context-budget domain.
    #[allow(
        clippy::too_many_arguments,
        reason = "hard-cap evidence binds the complete prepared model scope"
    )]
    pub fn release_verified_hard_cap(
        tokens: NonZeroU32,
        certificate: Digest,
        descriptor: &ResolvedModelDescriptor,
        context: &ModelPreparationContext,
        protocol_family: &ProtocolId,
        lowering_revision: &str,
        projection_revision: &str,
        accounting_revision: &str,
    ) -> Result<Self, PreparedCallError> {
        validate_prepared_digest(certificate, DigestDomain::ContextBudget)?;
        let scope = prepared_output_scope_identity(
            descriptor,
            context,
            protocol_family,
            lowering_revision,
            projection_revision,
            accounting_revision,
        )?;
        Self::seal(
            tokens,
            PreparedOutputLimitSource::ReleaseVerifiedHardCap { certificate, scope },
        )
    }

    fn seal(
        tokens: NonZeroU32,
        source: PreparedOutputLimitSource,
    ) -> Result<Self, PreparedCallError> {
        let source_bytes =
            serde_json::to_vec(&source).map_err(|_| PreparedCallError::Serialization)?;
        let identity = Digest::sha256(
            DigestDomain::ContextBudget,
            8,
            &[
                b"plakous-model/prepared-output-limit-v2",
                &tokens.get().to_be_bytes(),
                &source_bytes,
            ],
        )?;
        Ok(Self {
            tokens,
            source,
            identity,
        })
    }

    /// Returns the exact authoritative output maximum.
    #[must_use]
    pub const fn tokens(&self) -> NonZeroU32 {
        self.tokens
    }

    /// Returns the provider-owned proof source.
    #[must_use]
    pub const fn source(&self) -> &PreparedOutputLimitSource {
        &self.source
    }

    /// Returns the proof identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for PreparedOutputLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreparedOutputLimitWire::deserialize(deserializer)?;
        match &wire.source {
            PreparedOutputLimitSource::EnforcedWireMaximum {
                wire_field,
                wire_digest,
                lowering_revision,
            } => {
                validate_preparation_label(wire_field)
                    .and_then(|()| validate_preparation_label(lowering_revision))
                    .and_then(|()| validate_prepared_digest(*wire_digest, DigestDomain::Blob))
                    .map_err(serde::de::Error::custom)?;
            }
            PreparedOutputLimitSource::ReleaseVerifiedHardCap { certificate, scope } => {
                validate_prepared_digest(*certificate, DigestDomain::ContextBudget)
                    .and_then(|()| validate_prepared_digest(*scope, DigestDomain::ContextBudget))
                    .map_err(serde::de::Error::custom)?;
            }
        }
        let value = Self::seal(wire.tokens, wire.source).map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

/// Stable category for a provider-owned lowering rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ModelPreparationErrorKind {
    /// The admitted plan cannot be lowered for this exact provider binding.
    InvalidRequest,
    /// The attached family artifact failed reuse validation; no call was prepared.
    ArtifactReuse,
    /// Required route/model behavior has no certified lowering.
    UnsupportedCapability,
    /// Exact family projection or serialization failed.
    Projection,
    /// Protected native history cannot be replayed under the current authority.
    ProtectedHistory,
    /// Bounded local resources cannot retain the exact preparation.
    ResourceLimit,
    /// The selected adapter is internally misconfigured.
    InvalidConfiguration,
}

/// Typed failure before a one-shot provider dispatch capability exists.
#[derive(Clone, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ModelPreparationError {
    /// Provider-neutral validation or plan binding failed.
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// Fresh selected-model authority was absent, stale, or foreign.
    #[error(transparent)]
    RuntimeAuthority(#[from] RuntimeModelAuthorityError),
    /// Prepared evidence or record integrity failed.
    #[error(transparent)]
    PreparedCall(#[from] PreparedCallError),
    /// Descriptor-owned complete-projection context admission failed.
    #[error(transparent)]
    ContextAdmission(#[from] ContextAdmissionError),
    /// Descriptor-owned admission denied this exact prepared projection.
    ///
    /// The complete status is retained only for typed inspection; ordinary
    /// error formatting does not expose its private scope metadata.
    #[error("context admission denied: {error}")]
    ContextAdmissionDenied {
        /// Complete exact-scope status computed from the prepared projection.
        status: Box<ContextStatus>,
        /// Unchanged typed admission denial derived from `status`.
        #[source]
        error: ContextAdmissionError,
    },
    /// Provider-owned exact lowering rejected the plan.
    #[error("model preparation failed: {message}")]
    Rejected {
        /// Stable rejection category.
        kind: ModelPreparationErrorKind,
        /// Bounded redaction-safe detail.
        message: SafeDiagnostic,
    },
}

impl ModelPreparationError {
    pub(super) fn from_context_admission(
        status: ContextStatus,
        error: ContextAdmissionError,
    ) -> Self {
        match error {
            error @ (ContextAdmissionError::Unavailable(_)
            | ContextAdmissionError::WouldOverflow(_)
            | ContextAdmissionError::ProtectedOverflow(_)) => Self::ContextAdmissionDenied {
                status: Box::new(status),
                error,
            },
            error @ (ContextAdmissionError::Contract(_) | ContextAdmissionError::Digest(_)) => {
                Self::ContextAdmission(error)
            }
        }
    }

    /// Creates a provider-owned redaction-safe lowering rejection.
    #[must_use]
    pub fn rejected(kind: ModelPreparationErrorKind, message: impl AsRef<str>) -> Self {
        Self::Rejected {
            kind,
            message: SafeDiagnostic::new(message),
        }
    }

    /// Returns the stable provider-owned category, when this is a rejection.
    #[must_use]
    pub const fn rejection_kind(&self) -> Option<ModelPreparationErrorKind> {
        match self {
            Self::Rejected { kind, .. } => Some(*kind),
            Self::Validation(_)
            | Self::RuntimeAuthority(_)
            | Self::PreparedCall(_)
            | Self::ContextAdmission(_)
            | Self::ContextAdmissionDenied { .. } => None,
        }
    }

    /// Returns the complete exact status retained for an admission denial.
    #[must_use]
    pub fn context_status(&self) -> Option<&ContextStatus> {
        match self {
            Self::ContextAdmissionDenied { status, .. } => Some(status),
            Self::Validation(_)
            | Self::RuntimeAuthority(_)
            | Self::PreparedCall(_)
            | Self::ContextAdmission(_)
            | Self::Rejected { .. } => None,
        }
    }

    /// Returns the unchanged typed admission denial paired with that status.
    #[must_use]
    pub const fn context_admission_error(&self) -> Option<&ContextAdmissionError> {
        match self {
            Self::ContextAdmissionDenied { error, .. } => Some(error),
            Self::Validation(_)
            | Self::RuntimeAuthority(_)
            | Self::PreparedCall(_)
            | Self::ContextAdmission(_)
            | Self::Rejected { .. } => None,
        }
    }
}

impl fmt::Debug for ModelPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => formatter.debug_tuple("Validation").field(error).finish(),
            Self::RuntimeAuthority(error) => formatter
                .debug_tuple("RuntimeAuthority")
                .field(error)
                .finish(),
            Self::PreparedCall(error) => {
                formatter.debug_tuple("PreparedCall").field(error).finish()
            }
            Self::ContextAdmission(error) => formatter
                .debug_tuple("ContextAdmission")
                .field(error)
                .finish(),
            Self::ContextAdmissionDenied { error, .. } => formatter
                .debug_struct("ContextAdmissionDenied")
                .field("status", &"<complete context status retained>")
                .field("error", error)
                .finish(),
            Self::Rejected { kind, message } => formatter
                .debug_struct("Rejected")
                .field("kind", kind)
                .field("message", message)
                .finish(),
        }
    }
}

#[derive(Serialize)]
struct PreparedOutputScopeContent<'a> {
    descriptor_binding: Digest,
    protocol_family: &'a ProtocolId,
    dialect: &'a ProtocolId,
    route: &'a RouteId,
    authority_class: Digest,
    requested_model: &'a ModelId,
    effective_model: &'a ModelId,
    adapter: &'a ModelAdapterProfileId,
    lowering_revision: &'a str,
    projection_revision: &'a str,
    accounting_revision: &'a str,
}

fn prepared_output_scope_identity(
    descriptor: &ResolvedModelDescriptor,
    context: &ModelPreparationContext,
    protocol_family: &ProtocolId,
    lowering_revision: &str,
    projection_revision: &str,
    accounting_revision: &str,
) -> Result<Digest, PreparedCallError> {
    context
        .authority
        .validate(descriptor, context.prepared_at)
        .map_err(PreparedCallError::RuntimeAuthority)?;
    let descriptor_binding =
        descriptor
            .binding_digest()
            .map_err(|_| PreparedCallError::ScopeDrift {
                field: "descriptor binding",
            })?;
    prepared_output_scope_identity_from_axes(
        descriptor_binding,
        protocol_family,
        descriptor.protocol(),
        descriptor.route(),
        context.authority.authority_class_binding()?,
        descriptor.requested_model(),
        descriptor
            .reported_model()
            .unwrap_or(descriptor.requested_model()),
        descriptor.profile(),
        lowering_revision,
        projection_revision,
        accounting_revision,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "output certificate scope is the complete exact model scope"
)]
fn prepared_output_scope_identity_from_axes(
    descriptor_binding: Digest,
    protocol_family: &ProtocolId,
    dialect: &ProtocolId,
    route: &RouteId,
    authority_class: Digest,
    requested_model: &ModelId,
    effective_model: &ModelId,
    adapter: &ModelAdapterProfileId,
    lowering_revision: &str,
    projection_revision: &str,
    accounting_revision: &str,
) -> Result<Digest, PreparedCallError> {
    validate_preparation_label(lowering_revision)?;
    validate_preparation_label(projection_revision)?;
    validate_preparation_label(accounting_revision)?;
    let content = PreparedOutputScopeContent {
        descriptor_binding,
        protocol_family,
        dialect,
        route,
        authority_class,
        requested_model,
        effective_model,
        adapter,
        lowering_revision,
        projection_revision,
        accounting_revision,
    };
    let bytes = serde_json::to_vec(&content).map_err(|_| PreparedCallError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ContextBudget,
        10,
        &[b"plakous-model/prepared-output-scope-v1", &bytes],
    )?)
}

pub(super) fn validate_prepared_digest(
    digest: Digest,
    domain: DigestDomain,
) -> Result<(), PreparedCallError> {
    if digest.algorithm() == DigestAlgorithm::Sha256
        && digest.domain() == domain
        && digest.version() != 0
    {
        Ok(())
    } else {
        Err(PreparedCallError::InvalidDigest)
    }
}

fn validate_prepared_wire(digest: Digest, size: u64) -> Result<(), PreparedCallError> {
    validate_prepared_digest(digest, DigestDomain::Blob)?;
    if size == 0 || size > MAX_PREPARED_WIRE_BYTES {
        Err(PreparedCallError::InvalidWireSize)
    } else {
        Ok(())
    }
}

fn validate_preparation_label(value: &str) -> Result<(), PreparedCallError> {
    if value.is_empty()
        || value.len() > MAX_PREPARATION_LABEL_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        Err(PreparedCallError::InvalidLabel)
    } else {
        Ok(())
    }
}
