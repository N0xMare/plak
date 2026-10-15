//! Provider-family continuation artifacts and their integrity binding.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    AttemptId, CanonicalItemPrefixIdentity, Digest, DigestAlgorithm, DigestDomain,
    ModelOperationRoot, RunId, TurnId, ValidatedRequestPlan,
};

/// Maximum opaque bytes retained by one protocol-family artifact sidecar.
pub const MAX_FAMILY_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;

const MAX_FAMILY_ID_BYTES: usize = 256;
const FAMILY_ARTIFACT_INTEGRITY_REVISION: u32 = 3;

/// Exact provider-neutral producer facts for one family-artifact candidate.
///
/// These digests identify reconstruction data owned elsewhere. They never
/// replace the request, plan, canonical projection, or durable operation
/// records and grant no continuation or terminal authority by themselves.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyArtifactProducerFacts {
    request: Digest,
    model_input: Digest,
    plan: Digest,
    cache_affinity: Option<Digest>,
    run: RunId,
    root: ModelOperationRoot,
    attempt: AttemptId,
    source_projection: Digest,
    source_projection_items: u32,
    output_items: u32,
    covered_projection: Digest,
    covered_projection_items: u32,
    physical_operation: Digest,
    terminal_operation: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FamilyArtifactProducerFactsWire {
    request: Digest,
    model_input: Digest,
    plan: Digest,
    cache_affinity: RequiredCacheAffinity,
    run: RunId,
    root: ModelOperationRoot,
    attempt: AttemptId,
    source_projection: Digest,
    source_projection_items: u32,
    output_items: u32,
    covered_projection: Digest,
    covered_projection_items: u32,
    physical_operation: Digest,
    terminal_operation: Digest,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct RequiredCacheAffinity(Option<Digest>);

impl<'de> Deserialize<'de> for FamilyArtifactProducerFacts {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FamilyArtifactProducerFactsWire::deserialize(deserializer)?;
        Self::new(
            wire.request,
            wire.model_input,
            wire.plan,
            wire.cache_affinity.0,
            wire.run,
            wire.root,
            wire.attempt,
            wire.source_projection,
            wire.source_projection_items,
            wire.output_items,
            wire.covered_projection,
            wire.covered_projection_items,
            wire.physical_operation,
            wire.terminal_operation,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl FamilyArtifactProducerFacts {
    /// Captures exact request, canonical coverage, and physical-operation facts.
    ///
    /// # Errors
    ///
    /// Rejects foreign digest domains, empty output, inconsistent counts, or
    /// identical physical-request and terminal-response identities.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the closed exact producer fact set"
    )]
    pub fn new(
        request: Digest,
        model_input: Digest,
        plan: Digest,
        cache_affinity: Option<Digest>,
        run: RunId,
        root: ModelOperationRoot,
        attempt: AttemptId,
        source_projection: Digest,
        source_projection_items: u32,
        output_items: u32,
        covered_projection: Digest,
        covered_projection_items: u32,
        physical_operation: Digest,
        terminal_operation: Digest,
    ) -> Result<Self, FamilyArtifactError> {
        validate_family_digest(request, DigestDomain::RequestPlan, "request")?;
        validate_family_digest(model_input, DigestDomain::RequestPlan, "model input")?;
        validate_family_digest(plan, DigestDomain::RequestPlan, "plan")?;
        if let Some(cache_affinity) = cache_affinity {
            validate_family_digest(
                cache_affinity,
                DigestDomain::CacheAffinity,
                "cache affinity",
            )?;
        }
        validate_family_digest(
            source_projection,
            DigestDomain::CanonicalItemPrefix,
            "source item prefix",
        )?;
        validate_family_digest(
            covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "covered item prefix",
        )?;
        validate_family_digest(
            physical_operation,
            DigestDomain::ProviderOperation,
            "physical operation",
        )?;
        validate_family_digest(
            terminal_operation,
            DigestDomain::ProviderOperation,
            "terminal operation",
        )?;
        if output_items == 0
            || source_projection_items.checked_add(output_items) != Some(covered_projection_items)
        {
            return Err(FamilyArtifactError::InvalidCounts);
        }
        if physical_operation == terminal_operation {
            return Err(FamilyArtifactError::InvalidOperationPair);
        }
        Ok(Self {
            request,
            model_input,
            plan,
            cache_affinity,
            run,
            root,
            attempt,
            source_projection,
            source_projection_items,
            output_items,
            covered_projection,
            covered_projection_items,
            physical_operation,
            terminal_operation,
        })
    }

    /// Verifies all logical producer facts against reconstruction-complete inputs.
    ///
    /// # Errors
    ///
    /// Returns a mismatch when any request, plan, identity, source projection,
    /// or covered projection fact belongs to another operation.
    pub fn validate_for_plan(
        &self,
        plan: &ValidatedRequestPlan,
        covered_projection: CanonicalItemPrefixIdentity,
    ) -> Result<(), FamilyArtifactError> {
        let request = plan.request();
        let metadata = request.metadata();
        let source_projection = request
            .input()
            .canonical_item_prefix_identity(request.input().items().len())
            .map_err(|_| FamilyArtifactError::ForeignProducerFacts)?;
        if self.request != request.identity()
            || self.model_input != request.model_input_identity()
            || self.plan != plan.digest()
            || self.run != metadata.run_id()
            || self.root != metadata.root()
            || self.attempt != metadata.attempt_id()
            || self.source_projection != source_projection.identity()
            || usize::try_from(self.source_projection_items).ok()
                != Some(request.input().items().len())
            || self.covered_projection != covered_projection.identity()
            || usize::try_from(self.covered_projection_items).ok()
                != Some(covered_projection.item_count())
        {
            return Err(FamilyArtifactError::ForeignProducerFacts);
        }
        Ok(())
    }

    /// Returns the exact complete request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }

    /// Returns the exact model-visible request identity.
    #[must_use]
    pub const fn model_input(&self) -> Digest {
        self.model_input
    }

    /// Returns the exact admitted request-plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.plan
    }

    /// Returns the runtime-owned cache-affinity identity fixed for production.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<Digest> {
        self.cache_affinity
    }

    /// Returns the producing run identity.
    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }

    /// Returns the exact producing operation root.
    #[must_use]
    pub const fn root(&self) -> ModelOperationRoot {
        self.root
    }

    /// Returns the producing turn only for a turn-rooted operation.
    #[must_use]
    pub const fn turn(&self) -> Option<TurnId> {
        self.root.turn_id()
    }

    /// Returns the producing logical-attempt identity.
    #[must_use]
    pub const fn attempt(&self) -> AttemptId {
        self.attempt
    }

    /// Returns the source canonical projection identity.
    #[must_use]
    pub const fn source_projection(&self) -> Digest {
        self.source_projection
    }

    /// Returns the source canonical projection item count.
    #[must_use]
    pub const fn source_projection_items(&self) -> u32 {
        self.source_projection_items
    }

    /// Returns the exact canonical output count.
    #[must_use]
    pub const fn output_items(&self) -> u32 {
        self.output_items
    }

    /// Returns the covered canonical projection identity.
    #[must_use]
    pub const fn covered_projection(&self) -> Digest {
        self.covered_projection
    }

    /// Returns the covered canonical projection item count.
    #[must_use]
    pub const fn covered_projection_items(&self) -> u32 {
        self.covered_projection_items
    }

    /// Returns the host-verified physical request-operation identity.
    #[must_use]
    pub const fn physical_operation(&self) -> Digest {
        self.physical_operation
    }

    /// Returns the exact terminal provider-operation identity.
    #[must_use]
    pub const fn terminal_operation(&self) -> Digest {
        self.terminal_operation
    }

    pub(crate) fn integrity(&self) -> Result<Digest, FamilyArtifactError> {
        let request = self.request.identity_bytes();
        let model_input = self.model_input.identity_bytes();
        let plan = self.plan.identity_bytes();
        let cache_affinity = self
            .cache_affinity
            .map(|cache_affinity| cache_affinity.identity_bytes());
        let cache_affinity_present = [u8::from(cache_affinity.is_some())];
        let source_projection = self.source_projection.identity_bytes();
        let covered_projection = self.covered_projection.identity_bytes();
        let physical_operation = self.physical_operation.identity_bytes();
        let terminal_operation = self.terminal_operation.identity_bytes();
        let source_items = self.source_projection_items.to_be_bytes();
        let output_items = self.output_items.to_be_bytes();
        let covered_items = self.covered_projection_items.to_be_bytes();
        let run = self.run.to_string();
        let root = serde_json::to_vec(&self.root)
            .map_err(|_| FamilyArtifactError::IntegrityConstruction)?;
        let attempt = self.attempt.to_string();
        Digest::sha256(
            DigestDomain::Blob,
            FAMILY_ARTIFACT_INTEGRITY_REVISION,
            &[
                b"plakous-model/family-artifact-producer-v3",
                &request,
                &model_input,
                &plan,
                &cache_affinity_present,
                cache_affinity.as_deref().unwrap_or_default(),
                run.as_bytes(),
                &root,
                attempt.as_bytes(),
                &source_projection,
                &source_items,
                &output_items,
                &covered_projection,
                &covered_items,
                &physical_operation,
                &terminal_operation,
            ],
        )
        .map_err(|_| FamilyArtifactError::IntegrityConstruction)
    }
}

/// Immutable, bounded native family state admitted for one model call.
///
/// The bytes remain opaque to the neutral runtime. Only the owning protocol
/// family may decode them and grant a continuation/replay path after checking
/// the complete generic binding and its family-local contract.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ActiveFamilyArtifact {
    family: String,
    schema_revision: u32,
    codec_revision: u32,
    binding: Digest,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveFamilyArtifactWire {
    family: String,
    schema_revision: u32,
    codec_revision: u32,
    binding: Digest,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

impl<'de> Deserialize<'de> for ActiveFamilyArtifact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ActiveFamilyArtifactWire::deserialize(deserializer)?;
        Self::from_recorded(
            wire.family,
            wire.schema_revision,
            wire.codec_revision,
            wire.binding,
            wire.bytes,
            wire.integrity,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ActiveFamilyArtifact {
    /// Captures exact active native family bytes and computes their integrity.
    ///
    /// # Errors
    ///
    /// Rejects invalid family/revision/binding metadata or empty/oversized bytes.
    pub fn new(
        family: impl Into<String>,
        schema_revision: u32,
        codec_revision: u32,
        binding: Digest,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, FamilyArtifactError> {
        let family = validate_family_id(family.into())?;
        validate_family_revisions(schema_revision, codec_revision)?;
        validate_family_digest(binding, DigestDomain::ModelBinding, "binding")?;
        let bytes = bytes.into();
        validate_family_bytes(&bytes)?;
        let integrity = family_artifact_integrity(
            b"plakous-model/active-family-artifact-v2",
            &family,
            schema_revision,
            codec_revision,
            binding,
            None,
            &bytes,
        )?;
        Ok(Self {
            family,
            schema_revision,
            codec_revision,
            binding,
            bytes,
            integrity,
        })
    }

    fn from_recorded(
        family: String,
        schema_revision: u32,
        codec_revision: u32,
        binding: Digest,
        bytes: Arc<[u8]>,
        integrity: Digest,
    ) -> Result<Self, FamilyArtifactError> {
        validate_family_digest(integrity, DigestDomain::Blob, "integrity")?;
        let artifact = Self::new(family, schema_revision, codec_revision, binding, bytes)?;
        if artifact.integrity != integrity {
            return Err(FamilyArtifactError::IntegrityMismatch);
        }
        Ok(artifact)
    }

    /// Returns the stable protocol-family identity.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Returns the behavior-bearing family artifact schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }

    /// Returns the exact family artifact codec revision.
    #[must_use]
    pub const fn codec_revision(&self) -> u32 {
        self.codec_revision
    }

    /// Returns the complete generic family/target/producer binding.
    #[must_use]
    pub const fn binding(&self) -> Digest {
        self.binding
    }

    /// Returns exact opaque family-owned bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the neutral envelope integrity identity.
    #[must_use]
    pub const fn integrity(&self) -> Digest {
        self.integrity
    }
}

impl fmt::Debug for ActiveFamilyArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveFamilyArtifact")
            .field("family", &self.family)
            .field("schema_revision", &self.schema_revision)
            .field("codec_revision", &self.codec_revision)
            .field("binding", &self.binding)
            .field(
                "bytes",
                &format_args!("<redacted; {} bytes>", self.bytes.len()),
            )
            .field("integrity", &self.integrity)
            .finish()
    }
}

/// Fail-closed validation error for neutral family-artifact sidecars.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum FamilyArtifactError {
    /// Schema and codec revisions reserve version zero.
    #[error("family artifact revisions must be nonzero")]
    ZeroRevision,
    /// The family identity was empty, oversized, or nonportable.
    #[error("family artifact family identity is invalid")]
    InvalidFamily,
    /// An exact digest carried the wrong algorithm, domain, or version.
    #[error("family artifact has an invalid {field} digest")]
    InvalidDigest {
        /// The digest field that failed validation.
        field: &'static str,
    },
    /// Opaque family bytes were absent.
    #[error("family artifact bytes are empty")]
    EmptyBytes,
    /// Opaque family bytes exceeded the neutral retention ceiling.
    #[error("family artifact exceeds {maximum} bytes")]
    BytesTooLarge {
        /// Maximum complete opaque artifact bytes.
        maximum: usize,
    },
    /// Source, output, and covered item counts were inconsistent.
    #[error("family artifact projection counts are invalid")]
    InvalidCounts,
    /// The physical request and terminal response used one identity.
    #[error("family artifact physical and terminal operations are inconsistent")]
    InvalidOperationPair,
    /// Durable bytes or producer facts disagreed with their sealed integrity.
    #[error("family artifact integrity mismatch")]
    IntegrityMismatch,
    /// The integrity identity could not be computed.
    #[error("family artifact integrity construction failed")]
    IntegrityConstruction,
    /// Producer facts belonged to another admitted request or projection.
    #[error("family artifact producer facts belong to another operation")]
    ForeignProducerFacts,
    /// A terminal response carried a candidate for another request plan.
    #[error("family artifact candidate belongs to another response plan")]
    ForeignResponsePlan,
    /// Candidate output coverage disagreed with canonical response output.
    #[error("family artifact candidate output count disagrees with the response")]
    ResponseOutputMismatch,
}

pub(crate) fn family_artifact_integrity(
    tag: &[u8],
    family: &str,
    schema_revision: u32,
    codec_revision: u32,
    binding: Digest,
    producer_integrity: Option<Digest>,
    bytes: &[u8],
) -> Result<Digest, FamilyArtifactError> {
    let binding = binding.identity_bytes();
    let producer_integrity = producer_integrity.map(|digest| digest.identity_bytes());
    Digest::sha256(
        DigestDomain::Blob,
        FAMILY_ARTIFACT_INTEGRITY_REVISION,
        &[
            tag,
            family.as_bytes(),
            &schema_revision.to_be_bytes(),
            &codec_revision.to_be_bytes(),
            &binding,
            producer_integrity.as_deref().unwrap_or_default(),
            bytes,
        ],
    )
    .map_err(|_| FamilyArtifactError::IntegrityConstruction)
}

pub(crate) fn validate_family_id(family: String) -> Result<String, FamilyArtifactError> {
    if family.is_empty()
        || family.len() > MAX_FAMILY_ID_BYTES
        || !family.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        Err(FamilyArtifactError::InvalidFamily)
    } else {
        Ok(family)
    }
}

pub(crate) const fn validate_family_revisions(
    schema_revision: u32,
    codec_revision: u32,
) -> Result<(), FamilyArtifactError> {
    if schema_revision == 0 || codec_revision == 0 {
        Err(FamilyArtifactError::ZeroRevision)
    } else {
        Ok(())
    }
}

pub(crate) fn validate_family_digest(
    digest: Digest,
    domain: DigestDomain,
    field: &'static str,
) -> Result<(), FamilyArtifactError> {
    if digest.algorithm() != DigestAlgorithm::Sha256
        || digest.domain() != domain
        || digest.version() == 0
    {
        Err(FamilyArtifactError::InvalidDigest { field })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_family_bytes(bytes: &[u8]) -> Result<(), FamilyArtifactError> {
    if bytes.is_empty() {
        Err(FamilyArtifactError::EmptyBytes)
    } else if bytes.len() > MAX_FAMILY_ARTIFACT_BYTES {
        Err(FamilyArtifactError::BytesTooLarge {
            maximum: MAX_FAMILY_ARTIFACT_BYTES,
        })
    } else {
        Ok(())
    }
}
