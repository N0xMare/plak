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
    wire_input: Option<crate::ContextWireInput>,
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
    wire_input: Option<crate::ContextWireInput>,
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
        self.wire_input = Some(
            crate::ContextWireInput::measure_wire(body, image_maximum)
                .map_err(ContextAdmissionError::from)?,
        );
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

    /// Returns measured input items, when the dialect supplies them.
    #[must_use]
    pub const fn wire_input(&self) -> Option<&crate::ContextWireInput> {
        self.wire_input.as_ref()
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
        value.wire_input = wire.wire_input;
        value.request_anchor_identity = wire.request_anchor_identity;
        value.require_input_bound = wire.require_input_bound;
        value
            .bind_input_identity()
            .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
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

impl PreparedModelProjection {
    /// Constructs and seals the exact non-secret result of one lowering.
    ///
    /// The descriptor, admitted plan, fresh authority, fixed artifact, exact
    /// wire body, count projection, cache use, and output proof are checked as
    /// one trust-boundary result. No attempt/cancellation/deadline fact enters
    /// the lowering identity.
    ///
    /// # Errors
    ///
    /// Rejects any scope drift, missing integrity, invalid label, or oversized wire.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::similar_names,
        reason = "the complete exact prepared projection is checked as one auditable trust-boundary operation"
    )]
    pub fn new(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
        protocol_family: ProtocolId,
        lowering_revision: impl Into<String>,
        projection_revision: impl Into<String>,
        accounting_revision: impl Into<String>,
        wire_digest: Digest,
        wire_size: u64,
        token_projection: PreparedTokenProjection,
        cache_use: PreparedCacheUse,
        output_limit: Option<PreparedOutputLimit>,
    ) -> Result<Self, PreparedCallError> {
        descriptor
            .validate_plan_binding(plan)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "plan" })?;
        context
            .authority
            .validate(descriptor, context.prepared_at)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "authority" })?;
        validate_prepared_wire(wire_digest, wire_size)?;
        if token_projection.wire_digest != wire_digest || token_projection.wire_size != wire_size {
            return Err(PreparedCallError::ScopeDrift {
                field: "token projection",
            });
        }
        let lowering_revision = lowering_revision.into();
        let projection_revision = projection_revision.into();
        let accounting_revision = accounting_revision.into();
        validate_preparation_label(&lowering_revision)?;
        validate_preparation_label(&projection_revision)?;
        validate_preparation_label(&accounting_revision)?;
        let active_artifact = context
            .active_family_artifact
            .as_ref()
            .map(ActiveFamilyArtifact::integrity);
        let cache_affinity = context
            .cache_affinity
            .as_ref()
            .map(ModelCacheAffinity::identity);
        match cache_use {
            PreparedCacheUse::None => {}
            PreparedCacheUse::ActiveFamilyArtifact { artifact }
                if active_artifact == Some(artifact) => {}
            PreparedCacheUse::RuntimeAffinity { affinity } if cache_affinity == Some(affinity) => {}
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { artifact, affinity }
                if active_artifact == Some(artifact) && cache_affinity == Some(affinity) => {}
            PreparedCacheUse::ActiveFamilyArtifact { .. }
            | PreparedCacheUse::RuntimeAffinity { .. }
            | PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { .. } => {
                return Err(PreparedCallError::ScopeDrift { field: "cache use" });
            }
        }
        if let Some(limit) = &output_limit {
            match limit.source() {
                PreparedOutputLimitSource::EnforcedWireMaximum {
                    wire_digest: proof_wire,
                    lowering_revision: proof_lowering,
                    ..
                } if *proof_wire == wire_digest && *proof_lowering == lowering_revision => {}
                PreparedOutputLimitSource::ReleaseVerifiedHardCap { scope, .. }
                    if *scope
                        == prepared_output_scope_identity(
                            descriptor,
                            context,
                            &protocol_family,
                            &lowering_revision,
                            &projection_revision,
                            &accounting_revision,
                        )? => {}
                _ => {
                    return Err(PreparedCallError::ScopeDrift {
                        field: "output limit",
                    });
                }
            }
        }
        let authority = context.authority.execution_authority_binding()?;
        let authority_class = context.authority.authority_class_binding()?;
        let request = plan.request();
        let effective_model = descriptor
            .reported_model()
            .unwrap_or(descriptor.requested_model())
            .clone();
        let content = PreparedModelProjectionContent {
            plan: plan.digest(),
            selected_provider_configuration: request.selected_provider_configuration_id(),
            effective_provider_configuration: request.effective_provider_configuration_id(),
            descriptor_binding: plan.binding_digest(),
            request: request.identity(),
            model_input: request.model_input_identity(),
            operation_root: request.metadata().root(),
            attempt_id: request.metadata().attempt_id(),
            protocol_family: &protocol_family,
            dialect: descriptor.protocol(),
            route: descriptor.route(),
            authority,
            authority_class,
            requested_model: descriptor.requested_model(),
            effective_model: &effective_model,
            adapter: descriptor.profile(),
            lowering_revision: &lowering_revision,
            projection_revision: &projection_revision,
            accounting_revision: &accounting_revision,
            wire_digest,
            wire_size,
            token_projection: token_projection.identity,
            active_artifact,
            cache_affinity,
            cache_use,
            output_limit: output_limit.as_ref().map(PreparedOutputLimit::identity),
        };
        let identity = prepared_projection_identity(&content)?;
        Ok(Self {
            plan: content.plan,
            selected_provider_configuration: content.selected_provider_configuration,
            effective_provider_configuration: content.effective_provider_configuration,
            descriptor_binding: content.descriptor_binding,
            request: content.request,
            model_input: content.model_input,
            operation_root: content.operation_root,
            attempt_id: content.attempt_id,
            protocol_family,
            dialect: descriptor.protocol().clone(),
            route: descriptor.route().clone(),
            authority,
            authority_class,
            requested_model: descriptor.requested_model().clone(),
            effective_model,
            adapter: descriptor.profile().clone(),
            lowering_revision,
            projection_revision,
            accounting_revision,
            wire_digest,
            wire_size,
            token_projection,
            active_artifact,
            cache_affinity,
            cache_use,
            output_limit,
            identity,
        })
    }

    /// Validates every neutral scope axis against the exact plan and descriptor.
    ///
    /// # Errors
    ///
    /// Returns the first exact scope dimension which drifted.
    pub fn validate_for_plan(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
    ) -> Result<(), PreparedCallError> {
        descriptor
            .validate_plan_binding(plan)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "plan" })?;
        self.validate_plan_axes(descriptor, plan)
    }

    /// Checks every projection axis against `plan` and the semantic axes of
    /// `descriptor`, but not that the plan is bound to this exact observation
    /// of the descriptor.
    #[allow(
        clippy::too_many_lines,
        reason = "all prepared scope axes are rechecked together at the trust boundary"
    )]
    pub(super) fn validate_plan_axes(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
    ) -> Result<(), PreparedCallError> {
        let request = plan.request();
        let effective_model = descriptor
            .reported_model()
            .unwrap_or(descriptor.requested_model());
        let content = PreparedModelProjectionContent {
            plan: self.plan,
            selected_provider_configuration: self.selected_provider_configuration,
            effective_provider_configuration: self.effective_provider_configuration,
            descriptor_binding: self.descriptor_binding,
            request: self.request,
            model_input: self.model_input,
            operation_root: self.operation_root,
            attempt_id: self.attempt_id,
            protocol_family: &self.protocol_family,
            dialect: &self.dialect,
            route: &self.route,
            authority: self.authority,
            authority_class: self.authority_class,
            requested_model: &self.requested_model,
            effective_model: &self.effective_model,
            adapter: &self.adapter,
            lowering_revision: &self.lowering_revision,
            projection_revision: &self.projection_revision,
            accounting_revision: &self.accounting_revision,
            wire_digest: self.wire_digest,
            wire_size: self.wire_size,
            token_projection: self.token_projection.identity,
            active_artifact: self.active_artifact,
            cache_affinity: self.cache_affinity,
            cache_use: self.cache_use,
            output_limit: self
                .output_limit
                .as_ref()
                .map(PreparedOutputLimit::identity),
        };
        let output_scope = prepared_output_scope_identity_from_axes(
            self.descriptor_binding,
            &self.protocol_family,
            &self.dialect,
            &self.route,
            self.authority_class,
            &self.requested_model,
            &self.effective_model,
            &self.adapter,
            &self.lowering_revision,
            &self.projection_revision,
            &self.accounting_revision,
        )?;
        let cache_use_matches = match self.cache_use {
            PreparedCacheUse::None => true,
            PreparedCacheUse::ActiveFamilyArtifact { artifact } => {
                self.active_artifact == Some(artifact)
            }
            PreparedCacheUse::RuntimeAffinity { affinity } => self.cache_affinity == Some(affinity),
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { artifact, affinity } => {
                self.active_artifact == Some(artifact) && self.cache_affinity == Some(affinity)
            }
        };
        let checks = [
            (self.plan == plan.digest(), "plan"),
            (
                self.selected_provider_configuration
                    == request.selected_provider_configuration_id(),
                "selected provider configuration",
            ),
            (
                self.effective_provider_configuration
                    == request.effective_provider_configuration_id(),
                "effective provider configuration",
            ),
            (
                self.descriptor_binding == plan.binding_digest(),
                "descriptor binding",
            ),
            (self.request == request.identity(), "request"),
            (
                self.model_input == request.model_input_identity(),
                "model input",
            ),
            (
                self.operation_root == request.metadata().root(),
                "operation root",
            ),
            (
                self.attempt_id == request.metadata().attempt_id(),
                "attempt",
            ),
            (self.dialect == *descriptor.protocol(), "dialect"),
            (self.route == *descriptor.route(), "route"),
            (
                self.requested_model == *descriptor.requested_model(),
                "requested model",
            ),
            (self.effective_model == *effective_model, "effective model"),
            (self.adapter == *descriptor.profile(), "adapter"),
            (
                self.token_projection.wire_digest == self.wire_digest
                    && self.token_projection.wire_size == self.wire_size,
                "token projection",
            ),
            (cache_use_matches, "cache use"),
            (
                self.output_limit
                    .as_ref()
                    .is_none_or(|limit| match limit.source() {
                        PreparedOutputLimitSource::EnforcedWireMaximum {
                            wire_digest,
                            lowering_revision,
                            ..
                        } => {
                            *wire_digest == self.wire_digest
                                && *lowering_revision == self.lowering_revision
                        }
                        PreparedOutputLimitSource::ReleaseVerifiedHardCap { scope, .. } => {
                            *scope == output_scope
                        }
                    }),
                "output limit",
            ),
            (
                prepared_projection_identity(&content)? == self.identity,
                "identity",
            ),
        ];
        for (valid, field) in checks {
            if !valid {
                return Err(PreparedCallError::ScopeDrift { field });
            }
        }
        Ok(())
    }

    /// Returns the admitted plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.plan
    }
    /// Returns the caller-selected provider configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration
    }
    /// Returns the compiled provider configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }
    /// Returns the complete resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }
    /// Returns the exact canonical request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }
    /// Returns the complete model-input identity.
    #[must_use]
    pub const fn model_input(&self) -> Digest {
        self.model_input
    }
    /// Returns the exact logical operation root.
    #[must_use]
    pub const fn operation_root(&self) -> ModelOperationRoot {
        self.operation_root
    }
    /// Returns the logical attempt fixed by the request.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }
    /// Returns the protocol-family identity.
    #[must_use]
    pub const fn protocol_family(&self) -> &ProtocolId {
        &self.protocol_family
    }
    /// Returns the exact dialect identity.
    #[must_use]
    pub const fn dialect(&self) -> &ProtocolId {
        &self.dialect
    }
    /// Returns the exact route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }
    /// Returns the complete non-secret fresh-authority identity.
    #[must_use]
    pub const fn authority(&self) -> Digest {
        self.authority
    }
    /// Returns the stable account/credential authority class.
    #[must_use]
    pub const fn authority_class(&self) -> Digest {
        self.authority_class
    }
    /// Returns the requested model.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }
    /// Returns the effective model.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }
    /// Returns the adapter identity.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }
    /// Returns the lowering behavior revision.
    #[must_use]
    pub fn lowering_revision(&self) -> &str {
        &self.lowering_revision
    }
    /// Returns the family projection revision.
    #[must_use]
    pub fn projection_revision(&self) -> &str {
        &self.projection_revision
    }
    /// Returns the context accounting revision.
    #[must_use]
    pub fn accounting_revision(&self) -> &str {
        &self.accounting_revision
    }
    /// Returns the exact non-secret wire digest.
    #[must_use]
    pub const fn wire_digest(&self) -> Digest {
        self.wire_digest
    }
    /// Returns the exact non-secret wire size.
    #[must_use]
    pub const fn wire_size(&self) -> u64 {
        self.wire_size
    }
    /// Returns the complete prepared token projection.
    #[must_use]
    pub const fn token_projection(&self) -> &PreparedTokenProjection {
        &self.token_projection
    }
    /// Returns the fixed active-artifact identity.
    #[must_use]
    pub const fn active_artifact(&self) -> Option<Digest> {
        self.active_artifact
    }
    /// Returns the fixed redacted runtime cache-affinity identity.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<Digest> {
        self.cache_affinity
    }
    /// Returns the exact cache behavior.
    #[must_use]
    pub const fn cache_use(&self) -> PreparedCacheUse {
        self.cache_use
    }
    /// Returns the authoritative prepared output limit, when present.
    #[must_use]
    pub const fn output_limit(&self) -> Option<&PreparedOutputLimit> {
        self.output_limit.as_ref()
    }
    /// Returns the complete prepared-projection identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for PreparedModelProjection {
    #[allow(
        clippy::too_many_lines,
        reason = "deserialization rechecks every retained projection invariant before construction"
    )]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreparedModelProjectionWire::deserialize(deserializer)?;
        let value = Self {
            plan: wire.plan,
            selected_provider_configuration: wire.selected_provider_configuration,
            effective_provider_configuration: wire.effective_provider_configuration,
            descriptor_binding: wire.descriptor_binding,
            request: wire.request,
            model_input: wire.model_input,
            operation_root: wire.operation_root,
            attempt_id: wire.attempt_id,
            protocol_family: wire.protocol_family,
            dialect: wire.dialect,
            route: wire.route,
            authority: wire.authority,
            authority_class: wire.authority_class,
            requested_model: wire.requested_model,
            effective_model: wire.effective_model,
            adapter: wire.adapter,
            lowering_revision: wire.lowering_revision,
            projection_revision: wire.projection_revision,
            accounting_revision: wire.accounting_revision,
            wire_digest: wire.wire_digest,
            wire_size: wire.wire_size,
            token_projection: wire.token_projection,
            active_artifact: wire.active_artifact,
            cache_affinity: wire.cache_affinity,
            cache_use: wire.cache_use,
            output_limit: wire.output_limit,
            identity: wire.identity,
        };
        validate_prepared_wire(value.wire_digest, value.wire_size)
            .map_err(serde::de::Error::custom)?;
        validate_prepared_digest(value.plan, DigestDomain::RequestPlan)
            .and_then(|()| {
                validate_prepared_digest(value.descriptor_binding, DigestDomain::ModelBinding)
            })
            .and_then(|()| validate_prepared_digest(value.request, DigestDomain::RequestPlan))
            .and_then(|()| validate_prepared_digest(value.model_input, DigestDomain::RequestPlan))
            .and_then(|()| validate_prepared_digest(value.authority, DigestDomain::ModelBinding))
            .and_then(|()| {
                validate_prepared_digest(value.authority_class, DigestDomain::ModelBinding)
            })
            .and_then(|()| {
                value.active_artifact.map_or(Ok(()), |digest| {
                    validate_prepared_digest(digest, DigestDomain::Blob)
                })
            })
            .and_then(|()| {
                value.cache_affinity.map_or(Ok(()), |digest| {
                    validate_prepared_digest(digest, DigestDomain::CacheAffinity)
                })
            })
            .map_err(serde::de::Error::custom)?;
        validate_preparation_label(&value.lowering_revision)
            .and_then(|()| validate_preparation_label(&value.projection_revision))
            .and_then(|()| validate_preparation_label(&value.accounting_revision))
            .map_err(serde::de::Error::custom)?;
        let content = PreparedModelProjectionContent {
            plan: value.plan,
            selected_provider_configuration: value.selected_provider_configuration,
            effective_provider_configuration: value.effective_provider_configuration,
            descriptor_binding: value.descriptor_binding,
            request: value.request,
            model_input: value.model_input,
            operation_root: value.operation_root,
            attempt_id: value.attempt_id,
            protocol_family: &value.protocol_family,
            dialect: &value.dialect,
            route: &value.route,
            authority: value.authority,
            authority_class: value.authority_class,
            requested_model: &value.requested_model,
            effective_model: &value.effective_model,
            adapter: &value.adapter,
            lowering_revision: &value.lowering_revision,
            projection_revision: &value.projection_revision,
            accounting_revision: &value.accounting_revision,
            wire_digest: value.wire_digest,
            wire_size: value.wire_size,
            token_projection: value.token_projection.identity,
            active_artifact: value.active_artifact,
            cache_affinity: value.cache_affinity,
            cache_use: value.cache_use,
            output_limit: value
                .output_limit
                .as_ref()
                .map(PreparedOutputLimit::identity),
        };
        let cache_use_matches = match value.cache_use {
            PreparedCacheUse::None => true,
            PreparedCacheUse::ActiveFamilyArtifact { artifact } => {
                value.active_artifact == Some(artifact)
            }
            PreparedCacheUse::RuntimeAffinity { affinity } => {
                value.cache_affinity == Some(affinity)
            }
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { artifact, affinity } => {
                value.active_artifact == Some(artifact) && value.cache_affinity == Some(affinity)
            }
        };
        if !cache_use_matches {
            return Err(serde::de::Error::custom(PreparedCallError::ScopeDrift {
                field: "cache use",
            }));
        }
        let output_scope = prepared_output_scope_identity_from_axes(
            value.descriptor_binding,
            &value.protocol_family,
            &value.dialect,
            &value.route,
            value.authority_class,
            &value.requested_model,
            &value.effective_model,
            &value.adapter,
            &value.lowering_revision,
            &value.projection_revision,
            &value.accounting_revision,
        )
        .map_err(serde::de::Error::custom)?;
        let output_limit_matches =
            value
                .output_limit
                .as_ref()
                .is_none_or(|limit| match limit.source() {
                    PreparedOutputLimitSource::EnforcedWireMaximum {
                        wire_digest,
                        lowering_revision,
                        ..
                    } => {
                        *wire_digest == value.wire_digest
                            && *lowering_revision == value.lowering_revision
                    }
                    PreparedOutputLimitSource::ReleaseVerifiedHardCap { scope, .. } => {
                        *scope == output_scope
                    }
                });
        if !output_limit_matches {
            return Err(serde::de::Error::custom(PreparedCallError::ScopeDrift {
                field: "output limit",
            }));
        }
        if prepared_projection_identity(&content).map_err(serde::de::Error::custom)?
            != value.identity
        {
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

fn prepared_projection_identity(
    content: &PreparedModelProjectionContent<'_>,
) -> Result<Digest, PreparedCallError> {
    let bytes = serde_json::to_vec(content).map_err(|_| PreparedCallError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ContextBudget,
        11,
        &[b"plakous-model/prepared-model-projection-v2", &bytes],
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
