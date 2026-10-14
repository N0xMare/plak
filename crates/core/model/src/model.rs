//! Object-safe model calls, cancellation context, facets, and typed errors.

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_core::Stream;
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    AttemptId, BranchId, CanonicalItemPrefixIdentity, ContextAdmission, ContextAdmissionError,
    ContextStatus, CountObservation, Digest, DigestAlgorithm, DigestDomain,
    EffectiveProviderConfigurationId, ModelAdapterProfileId, ModelEvent, ModelId,
    ModelOperationRoot, ModelRequest, ModelResponse, ProtocolId, ResolvedModelDescriptor, RouteId,
    RunId, SafeDiagnostic, SelectedProviderConfigurationId, SessionId, Timestamp, TurnId, Usage,
    ValidatedRequestPlan, ValidationError, ValidationMode,
};

mod error;

pub use error::{
    ContextCapacityFailure, MAX_MODEL_PAID_RECOVERY_BYTES, MAX_MODEL_PROVIDER_CODE_BYTES,
    MAX_MODEL_PROVIDER_REQUEST_ID_BYTES, MAX_SERVED_BY_BYTES, ModelDispatchRejection,
    ModelDispatchRejectionClass, ModelDispatchRejectionPhase, ModelError, ModelErrorContext,
    ModelErrorContextError, ModelErrorKind, ModelErrorPhase, ModelErrorTarget, ModelHttpStatus,
    ModelPaidProviderAcceptance, ModelPaidRecovery, ModelPaidRecoveryBytes,
    ModelPaidRecoveryRequirement, ModelPaidTerminalFacts, ModelProviderCode,
    ModelProviderRequestId, ModelRetryDelay, ModelRetryRisk, ModelRetryability,
    RedactedModelErrorSource, ServedBy, provider_rejection_is_context_overflow,
};

/// Maximum credential-free provider admission evidence retained with one
/// runtime authority observation.
///
/// The witness is deliberately small: it may identify an exact provider-owned
/// catalog/capability receipt, but must never contain credentials or arbitrary
/// provider responses.
pub const MAX_RUNTIME_ADMISSION_WITNESS_BYTES: usize = 8 * 1024;

const MAX_RUNTIME_ADMISSION_WITNESS_KIND_BYTES: usize = 128;

/// Bounded opaque provider-owned admission evidence.
///
/// Plakous treats the payload semantically as an exact byte string. The owning
/// adapter defines its canonical codec and must reproduce the same witness on
/// every fresh revalidation. This preserves provider neutrality without
/// flattening provider-specific catalog semantics into the runtime.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct RuntimeAdmissionWitness {
    kind: String,
    revision: u32,
    payload: Vec<u8>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAdmissionWitnessWire {
    kind: String,
    revision: u32,
    payload: Vec<u8>,
    identity: Digest,
}

impl RuntimeAdmissionWitness {
    /// Seals canonical credential-free provider evidence.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized labels or payloads, a zero revision, or an
    /// identity construction failure.
    pub fn new(
        kind: impl Into<String>,
        revision: u32,
        payload: Vec<u8>,
    ) -> Result<Self, RuntimeModelAuthorityError> {
        let kind = kind.into();
        if revision == 0
            || kind.is_empty()
            || kind.len() > MAX_RUNTIME_ADMISSION_WITNESS_KIND_BYTES
            || !kind.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
            || payload.is_empty()
            || payload.len() > MAX_RUNTIME_ADMISSION_WITNESS_BYTES
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        let revision_bytes = revision.to_be_bytes();
        let identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-model/runtime-admission-witness-v1",
                kind.as_bytes(),
                &revision_bytes,
                &payload,
            ],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        Ok(Self {
            kind,
            revision,
            payload,
            identity,
        })
    }

    /// Returns the provider-owned witness kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the exact canonical witness codec revision.
    #[must_use]
    pub const fn revision(&self) -> u32 {
        self.revision
    }

    /// Returns the exact credential-free canonical payload.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Returns the domain-separated identity of kind, revision, and payload.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for RuntimeAdmissionWitness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RuntimeAdmissionWitnessWire::deserialize(deserializer)?;
        let witness =
            Self::new(wire.kind, wire.revision, wire.payload).map_err(serde::de::Error::custom)?;
        if witness.identity != wire.identity {
            return Err(serde::de::Error::custom(
                RuntimeModelAuthorityError::Invalid,
            ));
        }
        Ok(witness)
    }
}

impl fmt::Debug for RuntimeAdmissionWitness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeAdmissionWitness")
            .field("kind", &self.kind)
            .field("revision", &self.revision)
            .field("payload", &"<credential-free canonical evidence>")
            .field("identity", &self.identity)
            .finish()
    }
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

/// A cloneable cooperative cancellation signal.
#[derive(Clone, Default)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    /// Creates an uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation and wakes registered operations.
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            let wakers = {
                let mut registered = match self.state.wakers.lock() {
                    Ok(registered) => registered,
                    Err(poisoned) => poisoned.into_inner(),
                };
                std::mem::take(&mut *registered)
            };
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Reports whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Returns a future that completes when cancellation is requested.
    #[must_use]
    pub fn cancelled(&self) -> CancellationFuture {
        CancellationFuture {
            token: self.clone(),
        }
    }

    pub(crate) fn register(&self, waker: &Waker) {
        if self.is_cancelled() {
            waker.wake_by_ref();
            return;
        }
        let mut registered = match self.state.wakers.lock() {
            Ok(registered) => registered,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !registered.iter().any(|known| known.will_wake(waker)) {
            registered.push(waker.clone());
        }
        if self.is_cancelled() {
            waker.wake_by_ref();
        }
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// A future resolved by a [`CancellationToken`].
#[derive(Debug)]
pub struct CancellationFuture {
    token: CancellationToken,
}

impl Future for CancellationFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            Poll::Ready(())
        } else {
            self.token.register(context.waker());
            Poll::Pending
        }
    }
}

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

/// Owned context shared by preparation-independent model work.
#[derive(Clone, Debug)]
pub struct ModelCallContext {
    attempt_id: AttemptId,
    cancellation: CancellationToken,
    deadline: Option<Timestamp>,
    dispatch_timeout: Option<std::time::Duration>,
}

impl ModelCallContext {
    /// Creates a model call context.
    #[must_use]
    pub fn new(
        attempt_id: AttemptId,
        cancellation: CancellationToken,
        deadline: Option<Timestamp>,
    ) -> Self {
        Self {
            attempt_id,
            cancellation,
            deadline,
            dispatch_timeout: None,
        }
    }

    /// Returns the attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Sets the budget of each physical dispatch, up to its response head.
    /// This is not a deadline for the logical call or a streaming body.
    #[must_use]
    pub const fn with_dispatch_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.dispatch_timeout = Some(timeout);
        self
    }

    /// Returns the caller's per-generation dispatch budget, when supplied.
    #[must_use]
    pub const fn dispatch_timeout(&self) -> Option<std::time::Duration> {
        self.dispatch_timeout
    }

    /// Returns an optional hard deadline for the entire logical call.
    #[must_use]
    pub const fn deadline(&self) -> Option<Timestamp> {
        self.deadline
    }
}

/// An owned structural model-event stream.
///
/// Nonterminal events are bounded best-effort observations. They may be
/// dropped or become structurally gapped without changing the operation's
/// semantic outcome. A successful stream's unique
/// [`ModelEvent::ResponseCompleted`] is the self-contained authoritative
/// aggregate; consumers validate it against the prepared plan rather than
/// reconstructing canonical state from deltas.
///
/// Dropping the sole stream destroys its only in-band result channel, so a
/// provider cannot emit a later [`ModelError`] to that caller from `Drop` or
/// unwind. Drop is never terminal paid-dispatch proof. Durable paid recovery
/// therefore belongs to the dispatch recovery guard and persistent authority
/// registry; a fresh owner resolves or enumerates that registry after stream
/// loss or process restart. This alias intentionally does not add an
/// independent completion handle.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static>>;

/// Closed semantic item emitted by a locator-aware model operation stream.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelStreamItem {
    /// One ordinary structural model event.
    Event(ModelEvent),
    /// A provider recovery locator became durably relevant to this call.
    ///
    /// This is semantic control input for the runtime owner. It must never be
    /// projected as an observational agent event.
    RecoveryLocatorBound(ProviderCallRecoveryLocator),
}

impl From<ModelEvent> for ModelStreamItem {
    fn from(event: ModelEvent) -> Self {
        Self::Event(event)
    }
}

/// Locator-aware semantic model stream used by the runtime recovery path.
pub type ModelControlStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamItem, ModelError>> + Send + 'static>>;

/// An owned future that starts a model stream.
///
/// Consuming [`PreparedModelCall::start`] and the returned future's first poll
/// form one provider-dispatch admission boundary. Implementations must perform
/// no provider-visible work before `start` is called, and must treat a first poll
/// which returns `Pending` as admitted: later polls may finish that same
/// operation, but must not create a second operation. Cancellation after this
/// boundary is cooperative and the future/stream remains joinable.
pub type StartModelCall =
    Pin<Box<dyn Future<Output = Result<ModelStream, ModelError>> + Send + 'static>>;

/// One-shot start future retaining semantic locator-control items.
pub type StartModelCallWithControl =
    Pin<Box<dyn Future<Output = Result<ModelControlStream, ModelError>> + Send + 'static>>;

/// An owned future for a narrow optional model operation.
pub type ModelOperation<T> = Pin<Box<dyn Future<Output = Result<T, ModelError>> + Send + 'static>>;

/// An owned read-only provider-call recovery operation.
///
/// This future may inspect provider-owned evidence for the supplied exact
/// locator. It must never start, recreate, resume, or otherwise mutate a model
/// call, including when the result is ambiguous or unavailable.
pub type ModelCallRecoveryOperation = Pin<
    Box<
        dyn Future<Output = Result<ModelCallRecoveryResult, ModelCallRecoveryError>>
            + Send
            + 'static,
    >,
>;

/// An owned fresh model/account-authority revalidation operation.
pub type RuntimeModelAuthorityOperation = Pin<
    Box<
        dyn Future<Output = Result<RuntimeModelAuthority, RuntimeModelAuthorityError>>
            + Send
            + 'static,
    >,
>;

/// Fresh credential-free authority facts owned by the selected model adapter.
///
/// The account and credential-authority values are one-way digests. Raw
/// account identifiers, credential references, tokens, and payloads cannot be
/// represented by this value. The agent combines these facts with the actual
/// descriptor, profile, tools, workspace, environment, and effect policy; this
/// value alone grants no dispatch authority.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct RuntimeModelAuthority {
    descriptor_binding: Digest,
    account_binding: Digest,
    credential_authority_binding: Digest,
    credential_generation: u64,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    admission_witness: Option<RuntimeAdmissionWitness>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeModelAuthorityWire {
    descriptor_binding: Digest,
    account_binding: Digest,
    credential_authority_binding: Digest,
    credential_generation: u64,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    admission_witness: Option<RuntimeAdmissionWitness>,
}

impl<'de> Deserialize<'de> for RuntimeModelAuthority {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RuntimeModelAuthorityWire::deserialize(deserializer)?;
        if wire.credential_generation == 0
            || wire.descriptor_binding.domain() != DigestDomain::ModelBinding
            || wire.account_binding.domain() != DigestDomain::ModelBinding
            || wire.credential_authority_binding.domain() != DigestDomain::ModelBinding
            || wire
                .valid_until
                .is_some_and(|until| until <= wire.observed_at)
        {
            return Err(serde::de::Error::custom(
                RuntimeModelAuthorityError::Invalid,
            ));
        }
        Ok(Self {
            descriptor_binding: wire.descriptor_binding,
            account_binding: wire.account_binding,
            credential_authority_binding: wire.credential_authority_binding,
            credential_generation: wire.credential_generation,
            observed_at: wire.observed_at,
            valid_until: wire.valid_until,
            admission_witness: wire.admission_witness,
        })
    }
}

impl RuntimeModelAuthority {
    /// Creates adapter-owned, credential-free fresh authority evidence.
    ///
    /// # Errors
    ///
    /// Returns a typed error for a zero generation, non-model-binding digest,
    /// invalid freshness window, or descriptor whose binding cannot be
    /// computed.
    pub fn new(
        descriptor: &ResolvedModelDescriptor,
        account_binding: Digest,
        credential_authority_binding: Digest,
        credential_generation: u64,
        observed_at: Timestamp,
        valid_until: Option<Timestamp>,
    ) -> Result<Self, RuntimeModelAuthorityError> {
        if credential_generation == 0
            || account_binding.domain() != DigestDomain::ModelBinding
            || credential_authority_binding.domain() != DigestDomain::ModelBinding
            || valid_until.is_some_and(|until| until <= observed_at)
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        let descriptor_binding = descriptor
            .binding_digest()
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        Ok(Self {
            descriptor_binding,
            account_binding,
            credential_authority_binding,
            credential_generation,
            observed_at,
            valid_until,
            admission_witness: None,
        })
    }

    /// Attaches exact bounded provider-owned admission evidence.
    #[must_use]
    pub fn with_admission_witness(mut self, witness: RuntimeAdmissionWitness) -> Self {
        self.admission_witness = Some(witness);
        self
    }

    /// Validates this evidence against the actual selected descriptor and time.
    ///
    /// # Errors
    ///
    /// Returns a typed mismatch or stale-evidence error without exposing
    /// account or credential metadata.
    pub fn validate(
        &self,
        descriptor: &ResolvedModelDescriptor,
        checked_at: Timestamp,
    ) -> Result<(), RuntimeModelAuthorityError> {
        let binding = descriptor
            .binding_digest()
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        if binding != self.descriptor_binding {
            return Err(RuntimeModelAuthorityError::ForeignDescriptor);
        }
        if self.observed_at > checked_at
            || self
                .valid_until
                .is_some_and(|valid_until| valid_until <= checked_at)
        {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        Ok(())
    }

    /// Returns the exact selected-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the one-way selected-account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }

    /// Returns the one-way credential-source/authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the stable account/credential-authority class used by
    /// descriptor-owned capacity and measurement certificates.
    ///
    /// This deliberately excludes observation time, freshness, and credential
    /// generation. The complete execution authority identity retains those
    /// axes independently and still binds every prepared call.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated identity cannot be built.
    pub fn authority_class_binding(&self) -> Result<Digest, RuntimeModelAuthorityError> {
        Self::authority_class_for(self.account_binding, self.credential_authority_binding)
    }

    /// Derives the stable capacity/measurement certificate scope from exact
    /// non-secret account and credential-authority bindings.
    ///
    /// # Errors
    ///
    /// Returns an error if either input has the wrong digest domain or the
    /// domain-separated class identity cannot be constructed.
    pub fn authority_class_for(
        account_binding: Digest,
        credential_authority_binding: Digest,
    ) -> Result<Digest, RuntimeModelAuthorityError> {
        if account_binding.domain() != DigestDomain::ModelBinding
            || credential_authority_binding.domain() != DigestDomain::ModelBinding
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        Digest::sha256(
            DigestDomain::ModelBinding,
            3,
            &[
                b"plakous-model/runtime-authority-class-v1",
                &account_binding.identity_bytes(),
                &credential_authority_binding.identity_bytes(),
            ],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }

    /// Returns the exact nonzero credential generation.
    #[must_use]
    pub const fn credential_generation(&self) -> u64 {
        self.credential_generation
    }

    /// Returns when the owning adapter observed these facts.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the exclusive freshness deadline, when bounded.
    #[must_use]
    pub const fn valid_until(&self) -> Option<Timestamp> {
        self.valid_until
    }

    /// Returns exact provider-owned admission evidence, when the selected
    /// route requires an additional catalog/capability fence.
    #[must_use]
    pub const fn admission_witness(&self) -> Option<&RuntimeAdmissionWitness> {
        self.admission_witness.as_ref()
    }

    /// Returns the stable exact selected authority used by prepared execution.
    ///
    /// Observation and expiry time are freshness evidence, not lowering inputs,
    /// and are intentionally excluded. Account, credential authority, and exact
    /// credential generation remain bound.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated identity cannot be built.
    pub fn execution_authority_binding(&self) -> Result<Digest, RuntimeModelAuthorityError> {
        let generation = self.credential_generation.to_be_bytes();
        let witness = self
            .admission_witness
            .as_ref()
            .map_or_else(Vec::new, |witness| witness.identity().identity_bytes());
        Digest::sha256(
            DigestDomain::ModelBinding,
            5,
            &[
                b"plakous-model/stable-runtime-model-authority-v2",
                &self.descriptor_binding.identity_bytes(),
                &self.account_binding.identity_bytes(),
                &self.credential_authority_binding.identity_bytes(),
                &generation,
                &witness,
            ],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }

    /// Alias for the clock-independent exact execution-authority binding.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated identity cannot be built.
    pub fn stable_identity(&self) -> Result<Digest, RuntimeModelAuthorityError> {
        self.execution_authority_binding()
    }

    /// Reports whether two fresh observations name the same exact selected
    /// descriptor, account, credential authority, and generation.
    #[must_use]
    pub fn same_selected_authority(&self, other: &Self) -> bool {
        self.descriptor_binding == other.descriptor_binding
            && self.account_binding == other.account_binding
            && self.credential_authority_binding == other.credential_authority_binding
            && self.credential_generation == other.credential_generation
            && self.admission_witness == other.admission_witness
    }
}

impl fmt::Debug for RuntimeModelAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeModelAuthority")
            .field("descriptor_binding", &self.descriptor_binding)
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("credential_generation", &self.credential_generation)
            .field("observed_at", &self.observed_at)
            .field("valid_until", &self.valid_until)
            .field("admission_witness", &self.admission_witness)
            .finish()
    }
}

/// Typed redaction-safe failure to produce fresh model/account authority.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RuntimeModelAuthorityError {
    /// The adapter has no honest authority producer for this route.
    #[error("fresh model/account authority is unavailable")]
    Unavailable,
    /// The adapter returned malformed or contradictory authority facts.
    #[error("fresh model/account authority is invalid")]
    Invalid,
    /// The authority facts belong to another selected descriptor.
    #[error("fresh model/account authority belongs to another descriptor")]
    ForeignDescriptor,
    /// Fresh facts name another account or credential generation for this descriptor.
    #[error("fresh model/account authority belongs to another authority generation")]
    ForeignAuthority,
    /// The evidence is future-dated or no longer current.
    #[error("fresh model/account authority is stale")]
    Stale,
}

/// Maximum serialized non-secret preparation record retained for recovery.
pub const MAX_PREPARED_CALL_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// Current semantic/codec revision of [`PreparedCallRecord`].
pub const PREPARED_CALL_RECORD_SEMANTIC_REVISION: u32 = 5;

/// Current provider-operation digest revision for prepared and recovery records.
pub const PROVIDER_OPERATION_SEMANTIC_REVISION: u32 = 9;

/// Current neutral envelope revision for provider-owned recovery locators.
pub const PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION: u32 = 2;

/// Current neutral provider-call recovery request revision.
pub const MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION: u32 = 2;
/// Maximum opaque provider-owned bytes in one recovery locator.
pub const MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES: usize = 4_096;

/// Current neutral provider-call recovery result revision.
pub const MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION: u32 = 2;

/// Current neutral provider-call recovery evidence revision.
pub const MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION: u32 = 1;

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
    authority: RuntimeModelAuthority,
    prepared_at: Timestamp,
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
    fn validate_plan_axes(
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

/// The point at which a provider recovery locator became available.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ProviderCallRecoveryLocatorStage {
    /// The locator was fixed before the provider-visible dispatch boundary.
    Prepared,
    /// The locator was learned from provider evidence after dispatch.
    LearnedAfterDispatch,
}

/// A bounded, versioned, credential-free provider recovery locator.
///
/// The provider owns the lookup semantics behind `provider_revision`. The
/// neutral envelope retains exact nonempty opaque bytes and binds them to the
/// stable operation root, descriptor, request, plan, and prepared projection.
/// The bytes are correlation material, not provider request/response content
/// or credential material. `Debug` never reveals them.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ProviderCallRecoveryLocator {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    availability_stage: ProviderCallRecoveryLocatorStage,
    operation_root: ModelOperationRoot,
    descriptor_binding: Digest,
    dialect: ProtocolId,
    route: RouteId,
    effective_model: ModelId,
    request: Digest,
    plan: Digest,
    projection: Digest,
    provider_material: Arc<[u8]>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderCallRecoveryLocatorWire {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    availability_stage: ProviderCallRecoveryLocatorStage,
    operation_root: ModelOperationRoot,
    descriptor_binding: Digest,
    dialect: ProtocolId,
    route: RouteId,
    effective_model: ModelId,
    request: Digest,
    plan: Digest,
    projection: Digest,
    provider_material: BoundedRecoveryLocatorBytes,
    identity: Digest,
}

struct BoundedRecoveryLocatorBytes(Arc<[u8]>);

impl<'de> Deserialize<'de> for BoundedRecoveryLocatorBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BoundedRecoveryLocatorBytes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "1..={MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES} opaque bytes"
                )
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_provider_locator_material(value).map_err(E::custom)?;
                Ok(BoundedRecoveryLocatorBytes(Arc::from(value)))
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_provider_locator_material(&value).map_err(E::custom)?;
                Ok(BoundedRecoveryLocatorBytes(Arc::from(value)))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                if sequence
                    .size_hint()
                    .is_some_and(|length| length > MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES)
                {
                    return Err(serde::de::Error::custom(
                        ModelCallRecoveryError::InvalidProviderMaterial,
                    ));
                }
                let mut bytes = Vec::with_capacity(
                    sequence
                        .size_hint()
                        .unwrap_or(0)
                        .min(MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES),
                );
                while let Some(byte) = sequence.next_element::<u8>()? {
                    if bytes.len() == MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES {
                        return Err(serde::de::Error::custom(
                            ModelCallRecoveryError::InvalidProviderMaterial,
                        ));
                    }
                    bytes.push(byte);
                }
                validate_provider_locator_material(&bytes).map_err(serde::de::Error::custom)?;
                Ok(BoundedRecoveryLocatorBytes(Arc::from(bytes)))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl ProviderCallRecoveryLocator {
    /// Seals provider-generated recovery material available before dispatch.
    ///
    /// Providers must place the same material in their provider operation or
    /// otherwise prove it is directly resolvable after restart before calling
    /// this constructor. The bytes must be opaque correlation data,
    /// never credentials or raw request/response content.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized bytes or invalid prepared scope.
    pub fn seal(
        projection: &PreparedModelProjection,
        provider_revision: NonZeroU32,
        provider_material: impl AsRef<[u8]>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::from_material(
            projection,
            provider_revision,
            ProviderCallRecoveryLocatorStage::Prepared,
            provider_material.as_ref(),
        )
    }

    /// Seals provider-assigned material learned only after dispatch.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized bytes or invalid prepared scope.
    pub fn learned_after_dispatch(
        projection: &PreparedModelProjection,
        provider_revision: NonZeroU32,
        provider_material: impl AsRef<[u8]>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::from_material(
            projection,
            provider_revision,
            ProviderCallRecoveryLocatorStage::LearnedAfterDispatch,
            provider_material.as_ref(),
        )
    }

    fn from_material(
        projection: &PreparedModelProjection,
        provider_revision: NonZeroU32,
        availability_stage: ProviderCallRecoveryLocatorStage,
        provider_material: &[u8],
    ) -> Result<Self, ModelCallRecoveryError> {
        validate_provider_locator_material(provider_material)?;
        let mut value = Self {
            semantic_revision: PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION,
            provider_revision,
            availability_stage,
            operation_root: projection.operation_root,
            descriptor_binding: projection.descriptor_binding,
            dialect: projection.dialect.clone(),
            route: projection.route.clone(),
            effective_model: projection.effective_model.clone(),
            request: projection.request,
            plan: projection.plan,
            projection: projection.identity,
            provider_material: Arc::from(provider_material),
            identity: projection.identity,
        };
        value.identity = provider_recovery_locator_identity(&value)?;
        Ok(value)
    }

    fn validate_for_projection(
        &self,
        projection: &PreparedModelProjection,
    ) -> Result<(), ModelCallRecoveryError> {
        if self.semantic_revision != PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION
            || self.operation_root != projection.operation_root
            || self.descriptor_binding != projection.descriptor_binding
            || self.dialect != projection.dialect
            || self.route != projection.route
            || self.effective_model != projection.effective_model
            || self.request != projection.request
            || self.plan != projection.plan
            || self.projection != projection.identity
        {
            return Err(ModelCallRecoveryError::ForeignScope {
                field: "prepared projection",
            });
        }
        if provider_recovery_locator_identity(self)? != self.identity {
            return Err(ModelCallRecoveryError::IntegrityMismatch);
        }
        Ok(())
    }

    /// Returns the provider-owned locator format revision.
    #[must_use]
    pub const fn provider_revision(&self) -> NonZeroU32 {
        self.provider_revision
    }

    /// Returns when this locator became available.
    #[must_use]
    pub const fn availability_stage(&self) -> ProviderCallRecoveryLocatorStage {
        self.availability_stage
    }

    /// Returns the exact stable logical operation root.
    #[must_use]
    pub const fn operation_root(&self) -> ModelOperationRoot {
        self.operation_root
    }

    /// Returns the exact resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the exact provider dialect identity.
    #[must_use]
    pub const fn dialect(&self) -> &ProtocolId {
        &self.dialect
    }

    /// Returns the exact provider route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the exact effective model identity.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }

    /// Returns the exact canonical request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }

    /// Returns the exact admitted plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.plan
    }

    /// Returns the exact prepared projection identity.
    #[must_use]
    pub const fn projection(&self) -> Digest {
        self.projection
    }

    /// Returns the opaque provider lookup material.
    #[must_use]
    pub fn provider_material(&self) -> &[u8] {
        &self.provider_material
    }

    /// Returns the locator integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ProviderCallRecoveryLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProviderCallRecoveryLocatorWire::deserialize(deserializer)?;
        if wire.semantic_revision != PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let projection = PreparedRecoveryScope {
            operation_root: wire.operation_root,
            descriptor_binding: wire.descriptor_binding,
            dialect: wire.dialect,
            route: wire.route,
            effective_model: wire.effective_model,
            request: wire.request,
            plan: wire.plan,
            projection: wire.projection,
        };
        let value = provider_recovery_locator_from_scope(
            projection,
            wire.provider_revision,
            wire.availability_stage,
            wire.provider_material.0,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

impl fmt::Debug for ProviderCallRecoveryLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCallRecoveryLocator")
            .field("semantic_revision", &self.semantic_revision)
            .field("provider_revision", &self.provider_revision)
            .field("availability_stage", &self.availability_stage)
            .field("operation_root", &self.operation_root)
            .field("descriptor_binding", &self.descriptor_binding)
            .field("dialect", &self.dialect)
            .field("route", &self.route)
            .field("effective_model", &self.effective_model)
            .field("request", &self.request)
            .field("plan", &self.plan)
            .field("projection", &self.projection)
            .field(
                "provider_material",
                &format_args!("<{} redacted bytes>", self.provider_material.len()),
            )
            .field("identity", &self.identity)
            .finish()
    }
}

#[derive(Clone)]
struct PreparedRecoveryScope {
    operation_root: ModelOperationRoot,
    descriptor_binding: Digest,
    dialect: ProtocolId,
    route: RouteId,
    effective_model: ModelId,
    request: Digest,
    plan: Digest,
    projection: Digest,
}

fn provider_recovery_locator_from_scope(
    scope: PreparedRecoveryScope,
    provider_revision: NonZeroU32,
    availability_stage: ProviderCallRecoveryLocatorStage,
    provider_material: Arc<[u8]>,
) -> Result<ProviderCallRecoveryLocator, ModelCallRecoveryError> {
    validate_provider_locator_material(&provider_material)?;
    let mut value = ProviderCallRecoveryLocator {
        semantic_revision: PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION,
        provider_revision,
        availability_stage,
        operation_root: scope.operation_root,
        descriptor_binding: scope.descriptor_binding,
        dialect: scope.dialect,
        route: scope.route,
        effective_model: scope.effective_model,
        request: scope.request,
        plan: scope.plan,
        projection: scope.projection,
        provider_material,
        identity: scope.projection,
    };
    value.identity = provider_recovery_locator_identity(&value)?;
    Ok(value)
}

fn provider_recovery_locator_identity(
    locator: &ProviderCallRecoveryLocator,
) -> Result<Digest, ModelCallRecoveryError> {
    for (digest, domain) in [
        (locator.descriptor_binding, DigestDomain::ModelBinding),
        (locator.request, DigestDomain::RequestPlan),
        (locator.plan, DigestDomain::RequestPlan),
        (locator.projection, DigestDomain::ContextBudget),
    ] {
        validate_prepared_digest(digest, domain)
            .map_err(|_| ModelCallRecoveryError::InvalidDigest)?;
    }
    let provider_revision = locator.provider_revision.get().to_be_bytes();
    let stage = match locator.availability_stage {
        ProviderCallRecoveryLocatorStage::Prepared => b"prepared".as_slice(),
        ProviderCallRecoveryLocatorStage::LearnedAfterDispatch => {
            b"learned-after-dispatch".as_slice()
        }
    };
    let root = serde_json::to_vec(&locator.operation_root)
        .map_err(|_| ModelCallRecoveryError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[
            b"plakous-model/provider-call-recovery-locator-v2",
            &locator.semantic_revision.to_be_bytes(),
            &provider_revision,
            stage,
            &root,
            &locator.descriptor_binding.identity_bytes(),
            locator.dialect.as_str().as_bytes(),
            locator.route.as_str().as_bytes(),
            locator.effective_model.as_str().as_bytes(),
            &locator.request.identity_bytes(),
            &locator.plan.identity_bytes(),
            &locator.projection.identity_bytes(),
            &locator.provider_material,
        ],
    )?)
}

fn validate_provider_locator_material(bytes: &[u8]) -> Result<(), ModelCallRecoveryError> {
    if bytes.is_empty() || bytes.len() > MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES {
        Err(ModelCallRecoveryError::InvalidProviderMaterial)
    } else {
        Ok(())
    }
}

/// Bounded serializable recovery record for one exact prepared call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreparedCallRecord {
    semantic_revision: u32,
    projection: PreparedModelProjection,
    authority_evidence: RuntimeModelAuthority,
    context_admission: Digest,
    context_status: Digest,
    recovery_locator: Option<ProviderCallRecoveryLocator>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedCallRecordWire {
    semantic_revision: u32,
    projection: PreparedModelProjection,
    authority_evidence: RuntimeModelAuthority,
    context_admission: Digest,
    context_status: Digest,
    recovery_locator: Option<ProviderCallRecoveryLocator>,
    identity: Digest,
}

impl PreparedCallRecord {
    /// Seals a bounded inert record around an inspected prepared projection.
    ///
    /// # Errors
    ///
    /// Rejects a record whose canonical JSON exceeds the recovery bound.
    pub fn for_prepared(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        projection: PreparedModelProjection,
        context: &ModelPreparationContext,
    ) -> Result<Self, ModelPreparationError> {
        Self::for_prepared_with_recovery_locator(descriptor, plan, projection, context, None)
    }

    /// Seals a prepared record with an optional certified provider locator.
    ///
    /// A present locator is part of the record codec and identity before the
    /// one-shot start capability can perform provider-visible work. A provider
    /// may later supply a separately persisted `LearnedAfterDispatch` locator
    /// bound to this same root and projection.
    ///
    /// # Errors
    ///
    /// Rejects descriptor, projection, authority, admission, or locator drift.
    pub fn for_prepared_with_recovery_locator(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        projection: PreparedModelProjection,
        context: &ModelPreparationContext,
        recovery_locator: Option<ProviderCallRecoveryLocator>,
    ) -> Result<Self, ModelPreparationError> {
        projection.validate_for_plan(descriptor, plan)?;
        context
            .authority
            .validate(descriptor, context.prepared_at)?;
        if context.authority.execution_authority_binding()? != projection.authority
            || context.authority.authority_class_binding()? != projection.authority_class
        {
            return Err(PreparedCallError::ScopeDrift {
                field: "runtime authority",
            }
            .into());
        }
        let status = descriptor
            .context_status(plan, &projection, context.prepared_at)
            .map_err(ContextAdmissionError::from)?;
        let admission = match status.admit_with_requirement(
            plan.request(),
            plan.request().context_admission_requirement(),
        ) {
            Ok(admission) => admission,
            Err(error) => {
                let parts = projection.token_projection().input_parts();
                return Err(ModelPreparationError::from_context_admission(
                    status.with_input_parts(parts),
                    error,
                ));
            }
        };
        Self::with_admission(
            projection,
            context.authority.clone(),
            &admission,
            recovery_locator,
        )
        .map_err(Into::into)
    }

    fn with_admission(
        projection: PreparedModelProjection,
        authority_evidence: RuntimeModelAuthority,
        admission: &ContextAdmission,
        recovery_locator: Option<ProviderCallRecoveryLocator>,
    ) -> Result<Self, PreparedCallError> {
        if admission.request().identity() != projection.request
            || admission.status().model_input_identity() != projection.model_input
        {
            return Err(PreparedCallError::ScopeDrift {
                field: "context admission",
            });
        }
        if admission.status().scope().is_none_or(|scope| {
            scope.prepared_token_projection() != projection.token_projection.identity()
                || scope.lowering_revision() != projection.lowering_revision
        }) {
            return Err(PreparedCallError::ScopeDrift {
                field: "context admission projection",
            });
        }
        Self::seal(
            projection,
            authority_evidence,
            admission.identity(),
            admission.status().identity(),
            recovery_locator,
        )
    }

    fn seal(
        projection: PreparedModelProjection,
        authority_evidence: RuntimeModelAuthority,
        context_admission: Digest,
        context_status: Digest,
        recovery_locator: Option<ProviderCallRecoveryLocator>,
    ) -> Result<Self, PreparedCallError> {
        for digest in [context_admission, context_status] {
            validate_prepared_digest(digest, DigestDomain::ContextAdmission)?;
        }
        if authority_evidence.execution_authority_binding()? != projection.authority
            || authority_evidence.authority_class_binding()? != projection.authority_class
        {
            return Err(PreparedCallError::ScopeDrift {
                field: "runtime authority",
            });
        }
        if let Some(locator) = &recovery_locator {
            locator.validate_for_projection(&projection)?;
        }
        let retained = (
            PREPARED_CALL_RECORD_SEMANTIC_REVISION,
            &projection,
            &authority_evidence,
            context_admission,
            context_status,
            &recovery_locator,
        );
        let retained_bytes =
            serde_json::to_vec(&retained).map_err(|_| PreparedCallError::Serialization)?;
        if retained_bytes.len() > MAX_PREPARED_CALL_RECORD_BYTES {
            return Err(PreparedCallError::RecordTooLarge);
        }
        let identity = Digest::sha256(
            DigestDomain::ProviderOperation,
            PROVIDER_OPERATION_SEMANTIC_REVISION,
            &[
                b"plakous-model/prepared-call-record-v5",
                &projection.identity.identity_bytes(),
                &retained_bytes,
            ],
        )?;
        Ok(Self {
            semantic_revision: PREPARED_CALL_RECORD_SEMANTIC_REVISION,
            projection,
            authority_evidence,
            context_admission,
            context_status,
            recovery_locator,
            identity,
        })
    }

    /// Returns the exact prepared-call semantic revision.
    #[must_use]
    pub const fn semantic_revision(&self) -> u32 {
        self.semantic_revision
    }

    /// Returns the complete non-secret prepared projection.
    #[must_use]
    pub const fn projection(&self) -> &PreparedModelProjection {
        &self.projection
    }

    /// Returns the exact runtime authority sealed at preparation.
    #[must_use]
    pub const fn authority_evidence(&self) -> &RuntimeModelAuthority {
        &self.authority_evidence
    }

    /// Returns the admitted whole-context identity.
    #[must_use]
    pub const fn context_admission_identity(&self) -> Digest {
        self.context_admission
    }

    /// Returns the admitted status identity.
    ///
    /// The digest proves which complete status was admitted; it cannot
    /// reconstruct that status.
    #[must_use]
    pub const fn context_status_identity(&self) -> Digest {
        self.context_status
    }

    /// Returns the sealed provider locator, when this call earned recovery.
    #[must_use]
    pub const fn recovery_locator(&self) -> Option<&ProviderCallRecoveryLocator> {
        self.recovery_locator.as_ref()
    }

    /// Derives the only valid read-only recovery request for this record.
    ///
    /// # Errors
    ///
    /// Returns [`ModelCallRecoveryError::MissingLocator`] when preparation did
    /// not certify provider recovery for this exact operation.
    pub fn recovery_request(&self) -> Result<ModelCallRecoveryRequest, ModelCallRecoveryError> {
        ModelCallRecoveryRequest::for_prepared(self)
    }

    /// Derives recovery using the latest separately persisted locator.
    ///
    /// This is the path for a provider locator learned after dispatch. The
    /// supplied exact locator supersedes the optional prepared locator only
    /// after it has independently crossed the runtime's checkpoint fence.
    ///
    /// # Errors
    ///
    /// Rejects root, projection, or locator-integrity drift.
    pub fn recovery_request_with_locator(
        &self,
        locator: ProviderCallRecoveryLocator,
    ) -> Result<ModelCallRecoveryRequest, ModelCallRecoveryError> {
        ModelCallRecoveryRequest::for_locator(self, locator)
    }

    /// Rechecks all neutral prepared and admission evidence at the final
    /// pre-dispatch fence without obtaining a provider call capability.
    ///
    /// # Errors
    ///
    /// Fails on descriptor or plan drift, stale stored authority evidence, or a
    /// recomputed context admission/status identity different from the record.
    pub fn revalidate(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        now: Timestamp,
    ) -> Result<(), ModelPreparationError> {
        self.projection.validate_for_plan(descriptor, plan)?;
        self.revalidate_selected_authority(&self.authority_evidence, descriptor, now)?;
        let status = descriptor
            .context_status(plan, &self.projection, now)
            .map_err(ContextAdmissionError::from)?;
        let admission = match status.admit_with_requirement(
            plan.request(),
            plan.request().context_admission_requirement(),
        ) {
            Ok(admission) => admission,
            Err(error) => {
                let parts = self.projection.token_projection().input_parts();
                return Err(ModelPreparationError::from_context_admission(
                    status.with_input_parts(parts),
                    error,
                ));
            }
        };
        let current = (admission.identity(), admission.status().identity());
        if current != (self.context_admission, self.context_status) {
            return Err(PreparedCallError::ScopeDrift {
                field: "context admission revalidation",
            }
            .into());
        }
        Ok(())
    }

    /// Checks a newly observed provider authority against this exact prepared
    /// execution binding while treating observation time only as freshness.
    ///
    /// # Errors
    ///
    /// Fails when evidence is stale/foreign or account, credential authority,
    /// or generation differs from the prepared call.
    pub fn revalidate_selected_authority(
        &self,
        authority: &RuntimeModelAuthority,
        descriptor: &ResolvedModelDescriptor,
        now: Timestamp,
    ) -> Result<(), ModelPreparationError> {
        authority.validate(descriptor, now)?;
        if authority.execution_authority_binding()? != self.projection.authority
            || authority.authority_class_binding()? != self.projection.authority_class
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        Ok(())
    }

    /// Rebuilds the exact plan this record prepared for `request` from another
    /// observation of the same semantic descriptor.
    ///
    /// A plan digest binds the freshness-sensitive descriptor binding, so a
    /// descriptor observed afresh (a reopen or a re-resolution) cannot
    /// recompute it. Capability decisions are recomputed from `descriptor`,
    /// whose dialect, route, requested and effective model and adapter must be
    /// the recorded ones; the plan keeps the descriptor binding this record and
    /// its authority evidence were prepared under, and must reproduce the
    /// recorded plan, request, model input, root and attempt. The caller proves
    /// the semantic descriptor binding through the effective provider
    /// configuration identity. The restored plan only commits or continues this
    /// record's response: [`ResolvedModelDescriptor::validate_plan_binding`]
    /// refuses it for any later dispatch under another observation.
    ///
    /// # Errors
    ///
    /// Fails when the authority evidence and projection disagree on the
    /// descriptor binding, the request does not validate under `mode`, or any
    /// projection axis differs.
    pub fn restore_plan(
        &self,
        descriptor: &ResolvedModelDescriptor,
        request: ModelRequest,
        mode: ValidationMode,
    ) -> Result<ValidatedRequestPlan, PreparedCallError> {
        let binding = self.projection.descriptor_binding;
        if self.authority_evidence.descriptor_binding() != binding {
            return Err(PreparedCallError::ScopeDrift {
                field: "runtime authority",
            });
        }
        let plan = descriptor
            .validate_request_for_binding(request, mode, binding)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "plan" })?;
        self.projection.validate_plan_axes(descriptor, &plan)?;
        Ok(plan)
    }

    /// Returns the record integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for PreparedCallRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreparedCallRecordWire::deserialize(deserializer)?;
        if wire.semantic_revision != PREPARED_CALL_RECORD_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let value = Self::seal(
            wire.projection,
            wire.authority_evidence,
            wire.context_admission,
            wire.context_status,
            wire.recovery_locator,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

/// Exact read-only request to reconcile one previously prepared provider call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallRecoveryRequest {
    semantic_revision: u32,
    prepared_call: Digest,
    locator: ProviderCallRecoveryLocator,
    identity: Digest,
}

impl ModelCallRecoveryRequest {
    fn for_prepared(record: &PreparedCallRecord) -> Result<Self, ModelCallRecoveryError> {
        let locator = record
            .recovery_locator
            .clone()
            .ok_or(ModelCallRecoveryError::MissingLocator)?;
        Self::for_locator(record, locator)
    }

    fn for_locator(
        record: &PreparedCallRecord,
        locator: ProviderCallRecoveryLocator,
    ) -> Result<Self, ModelCallRecoveryError> {
        locator.validate_for_projection(&record.projection)?;
        let mut value = Self {
            semantic_revision: MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION,
            prepared_call: record.identity,
            locator,
            identity: record.identity,
        };
        value.identity = model_call_recovery_request_identity(&value)?;
        Ok(value)
    }

    /// Rechecks this request against the exact retained prepared record.
    ///
    /// # Errors
    ///
    /// Rejects any prepared-call, locator, or scope drift.
    pub fn validate_for_record(
        &self,
        record: &PreparedCallRecord,
    ) -> Result<(), ModelCallRecoveryError> {
        let expected = Self::for_prepared(record)?;
        if *self == expected {
            Ok(())
        } else {
            Err(ModelCallRecoveryError::ForeignScope {
                field: "prepared call",
            })
        }
    }

    /// Rechecks the exact resolved model descriptor without acquiring authority.
    ///
    /// # Errors
    ///
    /// Rejects a request for another descriptor. This operation is read-only
    /// and is not dispatch revalidation.
    pub fn validate_for_descriptor(
        &self,
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<(), ModelCallRecoveryError> {
        let binding =
            descriptor
                .binding_digest()
                .map_err(|_| ModelCallRecoveryError::ForeignScope {
                    field: "descriptor",
                })?;
        if binding == self.locator.descriptor_binding {
            Ok(())
        } else {
            Err(ModelCallRecoveryError::ForeignScope {
                field: "descriptor",
            })
        }
    }

    /// Returns the exact prepared-call/provider-operation identity.
    #[must_use]
    pub const fn prepared_call(&self) -> Digest {
        self.prepared_call
    }

    /// Returns the sealed provider-owned locator.
    #[must_use]
    pub const fn locator(&self) -> &ProviderCallRecoveryLocator {
        &self.locator
    }

    /// Returns the exact descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.locator.descriptor_binding
    }

    /// Returns the exact canonical request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.locator.request
    }

    /// Returns the exact admitted plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.locator.plan
    }

    /// Returns the exact prepared projection identity.
    #[must_use]
    pub const fn projection(&self) -> Digest {
        self.locator.projection
    }

    /// Returns this complete recovery-request identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

fn model_call_recovery_request_identity(
    request: &ModelCallRecoveryRequest,
) -> Result<Digest, ModelCallRecoveryError> {
    if request.semantic_revision != MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION
        || request.prepared_call.domain() != DigestDomain::ProviderOperation
        || request.prepared_call.version() != PROVIDER_OPERATION_SEMANTIC_REVISION
    {
        return Err(ModelCallRecoveryError::InvalidDigest);
    }
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[
            b"plakous-model/model-call-recovery-request-v2",
            &request.semantic_revision.to_be_bytes(),
            &request.prepared_call.identity_bytes(),
            &request.locator.identity.identity_bytes(),
        ],
    )?)
}

/// Bounded provider-owned evidence for one read-only recovery classification.
///
/// This receipt is supplemental only and grants no classification by itself;
/// the enclosing sealed [`ModelCallRecoveryResult`] binds it to the exact
/// outcome, response or error, usage, and recovery request.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelCallRecoveryEvidence {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    recovery_request: Digest,
    receipt: [u8; 32],
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCallRecoveryEvidenceWire {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    recovery_request: Digest,
    receipt: [u8; 32],
    identity: Digest,
}

impl ModelCallRecoveryEvidence {
    /// Seals one-way provider evidence to an exact recovery request.
    ///
    /// # Errors
    ///
    /// Rejects the all-zero provider-material sentinel.
    pub fn seal(
        request: &ModelCallRecoveryRequest,
        provider_material: [u8; 32],
    ) -> Result<Self, ModelCallRecoveryError> {
        if provider_material.iter().all(|byte| *byte == 0) {
            return Err(ModelCallRecoveryError::InvalidProviderMaterial);
        }
        let provider_revision = request.locator.provider_revision;
        let provider_revision_bytes = provider_revision.get().to_be_bytes();
        let receipt = *Digest::sha256(
            DigestDomain::ProviderOperation,
            PROVIDER_OPERATION_SEMANTIC_REVISION,
            &[
                b"plakous-model/model-call-recovery-evidence-receipt-v1",
                &request.identity.identity_bytes(),
                &provider_revision_bytes,
                &provider_material,
            ],
        )?
        .bytes();
        Self::from_receipt(request.identity, provider_revision, receipt)
    }

    fn from_receipt(
        recovery_request: Digest,
        provider_revision: NonZeroU32,
        receipt: [u8; 32],
    ) -> Result<Self, ModelCallRecoveryError> {
        if receipt.iter().all(|byte| *byte == 0) {
            return Err(ModelCallRecoveryError::InvalidProviderMaterial);
        }
        let provider_revision_bytes = provider_revision.get().to_be_bytes();
        let identity = Digest::sha256(
            DigestDomain::ProviderOperation,
            PROVIDER_OPERATION_SEMANTIC_REVISION,
            &[
                b"plakous-model/model-call-recovery-evidence-v1",
                &MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION.to_be_bytes(),
                &recovery_request.identity_bytes(),
                &provider_revision_bytes,
                &receipt,
            ],
        )?;
        Ok(Self {
            semantic_revision: MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION,
            provider_revision,
            recovery_request,
            receipt,
            identity,
        })
    }

    /// Returns the provider evidence format revision.
    #[must_use]
    pub const fn provider_revision(&self) -> NonZeroU32 {
        self.provider_revision
    }

    /// Returns the evidence integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ModelCallRecoveryEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelCallRecoveryEvidenceWire::deserialize(deserializer)?;
        if wire.semantic_revision != MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let value = Self::from_receipt(wire.recovery_request, wire.provider_revision, wire.receipt)
            .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

impl fmt::Debug for ModelCallRecoveryEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallRecoveryEvidence")
            .field("semantic_revision", &self.semantic_revision)
            .field("provider_revision", &self.provider_revision)
            .field("recovery_request", &self.recovery_request)
            .field("receipt", &"<redacted provider recovery evidence>")
            .field("identity", &self.identity)
            .finish()
    }
}

/// Closed provider-neutral meaning of one read-only recovery classification.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelCallRecoveryOutcome {
    /// Provider evidence proves physical dispatch never occurred.
    ProvenNotDispatched,
    /// The exact provider operation completed with this authoritative response.
    Completed(Box<ModelResponse>),
    /// The exact provider operation terminally failed with a sanitized error.
    DefinitelyFailed(Box<ModelError>),
    /// Available evidence cannot prove non-dispatch or a terminal outcome.
    Ambiguous,
    /// The model/provider implementation has no recovery facet.
    Unsupported,
    /// The facet exists but cannot currently resolve this exact locator.
    Unavailable,
}

/// Sealed result of one read-only provider recovery request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelCallRecoveryResult {
    semantic_revision: u32,
    recovery_request: Digest,
    plan: Digest,
    outcome: ModelCallRecoveryOutcome,
    usage: Option<Usage>,
    evidence: Option<ModelCallRecoveryEvidence>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCallRecoveryResultWire {
    semantic_revision: u32,
    recovery_request: Digest,
    plan: Digest,
    outcome: ModelCallRecoveryOutcome,
    usage: Option<Usage>,
    evidence: Option<ModelCallRecoveryEvidence>,
    identity: Digest,
}

impl ModelCallRecoveryResult {
    /// Classifies an exact operation as proven not dispatched.
    ///
    /// # Errors
    ///
    /// Rejects foreign evidence or identity construction failure.
    pub fn proven_not_dispatched(
        request: &ModelCallRecoveryRequest,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::ProvenNotDispatched,
            None,
            evidence,
        )
    }

    /// Returns the exact recovered terminal response and any supplemental usage.
    ///
    /// # Errors
    ///
    /// Rejects an invalid/foreign response, contradictory usage, foreign
    /// evidence, or identity construction failure.
    pub fn completed(
        request: &ModelCallRecoveryRequest,
        response: ModelResponse,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::Completed(Box::new(response)),
            usage,
            evidence,
        )
    }

    /// Returns a sanitized definite terminal failure for the exact operation.
    ///
    /// # Errors
    ///
    /// Rejects an ambiguous failure, foreign evidence, or identity
    /// construction failure.
    pub fn definitely_failed(
        request: &ModelCallRecoveryRequest,
        error: ModelError,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::DefinitelyFailed(Box::new(error)),
            usage,
            evidence,
        )
    }

    /// Classifies the exact operation as unresolved without authorizing replay.
    ///
    /// # Errors
    ///
    /// Rejects foreign evidence or identity construction failure.
    pub fn ambiguous(
        request: &ModelCallRecoveryRequest,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::Ambiguous,
            usage,
            evidence,
        )
    }

    /// Returns the default fail-closed unsupported result.
    ///
    /// # Errors
    ///
    /// Returns an error only if sealed result identity construction fails.
    pub fn unsupported(request: &ModelCallRecoveryRequest) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(request, ModelCallRecoveryOutcome::Unsupported, None, None)
    }

    /// Reports that an implemented facet cannot resolve this exact locator.
    ///
    /// # Errors
    ///
    /// Returns an error only if sealed result identity construction fails.
    pub fn unavailable(request: &ModelCallRecoveryRequest) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(request, ModelCallRecoveryOutcome::Unavailable, None, None)
    }

    fn seal(
        request: &ModelCallRecoveryRequest,
        outcome: ModelCallRecoveryOutcome,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        if evidence.as_ref().is_some_and(|evidence| {
            evidence.recovery_request != request.identity
                || evidence.provider_revision != request.locator.provider_revision
        }) {
            return Err(ModelCallRecoveryError::ForeignScope { field: "evidence" });
        }
        match &outcome {
            ModelCallRecoveryOutcome::Completed(response) => {
                response
                    .validate_for_commit()
                    .map_err(|_| ModelCallRecoveryError::InvalidCompletedResponse)?;
                if response.plan_digest() != request.plan() {
                    return Err(ModelCallRecoveryError::ForeignScope {
                        field: "completed response",
                    });
                }
                if response.usage().is_some() && usage.is_some() && response.usage() != usage {
                    return Err(ModelCallRecoveryError::ConflictingUsage);
                }
            }
            ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
                if error.kind() == ModelErrorKind::OutcomeUnknown
                    || error.paid_recovery().is_some()
                    || error.context().is_some_and(|context| {
                        context.retryability() != ModelRetryability::Permanent
                    })
                {
                    return Err(ModelCallRecoveryError::AmbiguousDefiniteFailure);
                }
                if error.context().is_some_and(|context| {
                    context.protocol() != request.locator.dialect()
                        || context.route() != request.locator.route()
                        || context.model() != request.locator.effective_model()
                }) {
                    return Err(ModelCallRecoveryError::ForeignScope {
                        field: "definite failure target",
                    });
                }
            }
            ModelCallRecoveryOutcome::ProvenNotDispatched if usage.is_some() => {
                return Err(ModelCallRecoveryError::ConflictingUsage);
            }
            ModelCallRecoveryOutcome::Unsupported | ModelCallRecoveryOutcome::Unavailable
                if usage.is_some() || evidence.is_some() =>
            {
                return Err(ModelCallRecoveryError::ConflictingEvidence);
            }
            ModelCallRecoveryOutcome::ProvenNotDispatched
            | ModelCallRecoveryOutcome::Ambiguous
            | ModelCallRecoveryOutcome::Unsupported
            | ModelCallRecoveryOutcome::Unavailable => {}
        }
        let mut value = Self {
            semantic_revision: MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION,
            recovery_request: request.identity,
            plan: request.plan(),
            outcome,
            usage,
            evidence,
            identity: request.identity,
        };
        value.identity = model_call_recovery_result_identity(&value)?;
        Ok(value)
    }

    /// Rechecks result scope and integrity against the originating request.
    ///
    /// # Errors
    ///
    /// Rejects any request, plan, response, evidence, usage, or identity drift.
    pub fn validate_for_request(
        &self,
        request: &ModelCallRecoveryRequest,
    ) -> Result<(), ModelCallRecoveryError> {
        let expected = Self::seal(
            request,
            self.outcome.clone(),
            self.usage,
            self.evidence.clone(),
        )?;
        if *self == expected {
            Ok(())
        } else {
            Err(ModelCallRecoveryError::IntegrityMismatch)
        }
    }

    /// Returns the closed neutral recovery meaning.
    #[must_use]
    pub const fn outcome(&self) -> &ModelCallRecoveryOutcome {
        &self.outcome
    }

    /// Returns supplemental cumulative usage when provider evidence has it.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns bounded provider-owned evidence when available.
    #[must_use]
    pub const fn evidence(&self) -> Option<&ModelCallRecoveryEvidence> {
        self.evidence.as_ref()
    }

    /// Returns the recovery-result integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ModelCallRecoveryResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelCallRecoveryResultWire::deserialize(deserializer)?;
        if wire.semantic_revision != MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let value = Self {
            semantic_revision: wire.semantic_revision,
            recovery_request: wire.recovery_request,
            plan: wire.plan,
            outcome: wire.outcome,
            usage: wire.usage,
            evidence: wire.evidence,
            identity: wire.identity,
        };
        validate_recovery_result_without_request(&value).map_err(serde::de::Error::custom)?;
        if model_call_recovery_result_identity(&value).map_err(serde::de::Error::custom)?
            != value.identity
        {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

fn validate_recovery_result_without_request(
    result: &ModelCallRecoveryResult,
) -> Result<(), ModelCallRecoveryError> {
    if result.recovery_request.domain() != DigestDomain::ProviderOperation
        || result.recovery_request.version() != PROVIDER_OPERATION_SEMANTIC_REVISION
        || result.plan.domain() != DigestDomain::RequestPlan
        || result
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.recovery_request != result.recovery_request)
    {
        return Err(ModelCallRecoveryError::InvalidDigest);
    }
    match &result.outcome {
        ModelCallRecoveryOutcome::Completed(response) => {
            response
                .validate_for_commit()
                .map_err(|_| ModelCallRecoveryError::InvalidCompletedResponse)?;
            if response.plan_digest() != result.plan
                || (response.usage().is_some()
                    && result.usage.is_some()
                    && response.usage() != result.usage)
            {
                return Err(ModelCallRecoveryError::ConflictingUsage);
            }
        }
        ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
            if error.kind() == ModelErrorKind::OutcomeUnknown
                || error.paid_recovery().is_some()
                || error
                    .context()
                    .is_some_and(|context| context.retryability() != ModelRetryability::Permanent)
            {
                return Err(ModelCallRecoveryError::AmbiguousDefiniteFailure);
            }
        }
        ModelCallRecoveryOutcome::ProvenNotDispatched if result.usage.is_some() => {
            return Err(ModelCallRecoveryError::ConflictingUsage);
        }
        ModelCallRecoveryOutcome::Unsupported | ModelCallRecoveryOutcome::Unavailable
            if result.usage.is_some() || result.evidence.is_some() =>
        {
            return Err(ModelCallRecoveryError::ConflictingEvidence);
        }
        ModelCallRecoveryOutcome::ProvenNotDispatched
        | ModelCallRecoveryOutcome::Ambiguous
        | ModelCallRecoveryOutcome::Unsupported
        | ModelCallRecoveryOutcome::Unavailable => {}
    }
    Ok(())
}

fn model_call_recovery_result_identity(
    result: &ModelCallRecoveryResult,
) -> Result<Digest, ModelCallRecoveryError> {
    let retained = (
        result.semantic_revision,
        result.recovery_request,
        result.plan,
        &result.outcome,
        result.usage,
        &result.evidence,
    );
    let bytes = serde_json::to_vec(&retained).map_err(|_| ModelCallRecoveryError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[b"plakous-model/model-call-recovery-result-v1", &bytes],
    )?)
}

/// Invalid exact provider-call recovery data or classification.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ModelCallRecoveryError {
    /// Preparation did not seal a provider recovery locator.
    #[error("prepared call has no certified provider recovery locator")]
    MissingLocator,
    /// Provider material was empty or exceeded its exact byte bound.
    #[error("provider recovery material is empty or exceeds its byte bound")]
    InvalidProviderMaterial,
    /// A retained recovery value uses an unsupported semantic revision.
    #[error("provider call recovery revision is unsupported")]
    UnsupportedRevision,
    /// A recovery value belongs to another exact operation scope.
    #[error("provider call recovery scope drifted at {field}")]
    ForeignScope {
        /// Stable non-secret mismatched scope axis.
        field: &'static str,
    },
    /// A digest used the wrong domain or semantic revision.
    #[error("provider call recovery contains an invalid digest")]
    InvalidDigest,
    /// A recovered completed response is not commit-valid.
    #[error("provider call recovery returned an invalid completed response")]
    InvalidCompletedResponse,
    /// Supplemental and response usage disagree.
    #[error("provider call recovery usage evidence conflicts")]
    ConflictingUsage,
    /// An unsupported/unavailable result carried contradictory evidence.
    #[error("provider call recovery evidence conflicts with its classification")]
    ConflictingEvidence,
    /// A definite failure carried an outcome-unknown error.
    #[error("provider call recovery definite failure is ambiguous")]
    AmbiguousDefiniteFailure,
    /// Canonical identity serialization failed.
    #[error("provider call recovery identity serialization failed")]
    Serialization,
    /// Serialized fields disagreed with their sealed integrity identity.
    #[error("provider call recovery integrity mismatch")]
    IntegrityMismatch,
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] crate::DigestError),
}

type StartPreparedModelCall =
    Box<dyn FnOnce(ModelCallContext) -> StartModelCallWithControl + Send + 'static>;

/// One non-cloneable, non-serializable, one-shot prepared model call.
///
/// Dropping this value drops the only start capability and performs no provider
/// work. Starting consumes the exact captured lowering; attempt controls may
/// cancel or bound it but cannot replace target, body, cache, or token facts.
pub struct PreparedModelCall {
    record: PreparedCallRecord,
    start: Option<StartPreparedModelCall>,
}

impl PreparedModelCall {
    /// Couples one inert record to its sole private start capability.
    #[must_use]
    pub fn new<F>(record: PreparedCallRecord, start: F) -> Self
    where
        F: FnOnce(ModelCallContext) -> StartModelCall + Send + 'static,
    {
        Self::new_with_control(record, move |context| {
            semantic_start_from_events(start(context))
        })
    }

    /// Couples an inert record to a locator-aware one-shot start capability.
    #[must_use]
    pub fn new_with_control<F>(record: PreparedCallRecord, start: F) -> Self
    where
        F: FnOnce(ModelCallContext) -> StartModelCallWithControl + Send + 'static,
    {
        Self {
            record,
            start: Some(Box::new(start)),
        }
    }

    /// Returns the bounded serializable preparation record.
    #[must_use]
    pub const fn record(&self) -> &PreparedCallRecord {
        &self.record
    }

    /// Wraps the opaque start future without exposing or rebuilding the call.
    #[must_use]
    pub fn map_start<F>(self, map: F) -> Self
    where
        F: FnOnce(StartModelCall) -> StartModelCall + Send + 'static,
    {
        let Self { record, mut start } = self;
        match start.take() {
            Some(start) => Self::new(record, move |context| {
                map(events_start_from_semantic(start(context)))
            }),
            None => Self::new(record, move |_| missing_prepared_start()),
        }
    }

    /// Wraps the locator-aware start future without projecting control items.
    #[must_use]
    pub fn map_start_with_control<F>(self, map: F) -> Self
    where
        F: FnOnce(StartModelCallWithControl) -> StartModelCallWithControl + Send + 'static,
    {
        let Self { record, mut start } = self;
        match start.take() {
            Some(start) => Self::new_with_control(record, move |context| map(start(context))),
            None => Self::new_with_control(record, move |_| missing_prepared_control_start()),
        }
    }

    /// Runs prepared-call construction, start, and stream polls inside one
    /// immutable caller-supplied scope.
    ///
    /// The guard is never retained across a yield; it is recreated for each
    /// poll. Responses dialects that await credential refresh inside preflight
    /// can complete `start` on the first poll and acquire inference on a later
    /// stream poll — those stream polls must re-enter the same scope.
    #[must_use]
    pub fn scope_start<F, G>(self, enter: F) -> Self
    where
        F: Fn() -> G + Send + Sync + 'static,
        G: 'static,
    {
        let Self { record, mut start } = self;
        match start.take() {
            Some(start) => {
                let enter = Arc::new(enter);
                Self::new_with_control(record, move |context| {
                    let enter = Arc::clone(&enter);
                    let mut call = {
                        let _guard = enter();
                        start(context)
                    };
                    Box::pin(async move {
                        let stream = std::future::poll_fn(|cx| {
                            let _guard = enter();
                            call.as_mut().poll(cx)
                        })
                        .await?;
                        Ok(Box::pin(ScopedControlStream {
                            inner: stream,
                            enter,
                            _guard: PhantomData,
                        }) as ModelControlStream)
                    })
                })
            }
            None => Self::new_with_control(record, move |_| missing_prepared_control_start()),
        }
    }

    /// Consumes the sole start capability under attempt-only controls.
    pub fn start(self, context: ModelCallContext) -> StartModelCall {
        events_start_from_semantic(self.start_with_control(context))
    }

    /// Consumes the sole start capability while preserving locator controls.
    pub fn start_with_control(mut self, context: ModelCallContext) -> StartModelCallWithControl {
        if context.attempt_id() != self.record.projection.attempt_id {
            return Box::pin(async {
                Err(ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "model call context belongs to another prepared attempt",
                ))
            });
        }
        let Some(start) = self.start.take() else {
            return missing_prepared_control_start();
        };
        let prepared_call = self.record.identity();
        let attempt_id = context.attempt_id();
        let call = start(context);
        Box::pin(async move {
            let stream = call.await.map_err(|mut error| {
                error.bind_dispatch_rejection(prepared_call, attempt_id);
                error
            })?;
            Ok(Box::pin(stream.map(move |item| {
                item.map_err(|mut error| {
                    error.bind_dispatch_rejection(prepared_call, attempt_id);
                    error
                })
            })) as ModelControlStream)
        })
    }
}

fn semantic_start_from_events(start: StartModelCall) -> StartModelCallWithControl {
    Box::pin(async move {
        let stream = start.await?;
        Ok(Box::pin(stream.map(|item| item.map(ModelStreamItem::Event))) as ModelControlStream)
    })
}

fn events_start_from_semantic(start: StartModelCallWithControl) -> StartModelCall {
    Box::pin(async move {
        let stream = start.await?;
        Ok(Box::pin(stream.map(|item| match item {
            Ok(ModelStreamItem::Event(event)) => Ok(event),
            Ok(ModelStreamItem::RecoveryLocatorBound(_)) => Err(ModelError::new(
                ModelErrorKind::InvalidEvent,
                "locator-aware stream requires PreparedModelCall::start_with_control",
            )),
            Err(error) => Err(error),
        })) as ModelStream)
    })
}

fn missing_prepared_start() -> StartModelCall {
    Box::pin(async {
        Err(ModelError::new(
            ModelErrorKind::Protocol,
            "prepared call lost its sole start capability",
        ))
    })
}

fn missing_prepared_control_start() -> StartModelCallWithControl {
    Box::pin(async {
        Err(ModelError::new(
            ModelErrorKind::Protocol,
            "prepared call lost its sole start capability",
        ))
    })
}

struct ScopedControlStream<F, G> {
    inner: ModelControlStream,
    enter: Arc<F>,
    _guard: PhantomData<fn() -> G>,
}

impl<F, G> Stream for ScopedControlStream<F, G>
where
    F: Fn() -> G + Send + Sync + 'static,
    G: 'static,
{
    type Item = Result<ModelStreamItem, ModelError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let _guard = (this.enter)();
        this.inner.as_mut().poll_next(cx)
    }
}

impl fmt::Debug for PreparedModelCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedModelCall")
            .field("record", &self.record)
            .field("start", &"<one-shot opaque capability>")
            .finish()
    }
}

/// Typed construction failure for preparation evidence and inert records.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum PreparedCallError {
    /// One exact descriptor or plan/projection dimension drifted.
    #[error("prepared call scope drifted at {field}")]
    ScopeDrift {
        /// Stable non-secret name of the mismatched scope axis.
        field: &'static str,
    },
    /// A digest did not use the required domain, algorithm, or revision.
    #[error("prepared call contains an invalid digest")]
    InvalidDigest,
    /// A behavior-bearing label was empty, oversized, or non-portable.
    #[error("prepared call contains an invalid revision or field label")]
    InvalidLabel,
    /// The exact credential-free body was absent or exceeded its bound.
    #[error("prepared call wire size is invalid")]
    InvalidWireSize,
    /// The serializable recovery record exceeded its strict bound.
    #[error("prepared call record exceeds its retention bound")]
    RecordTooLarge,
    /// Serialization needed for an integrity identity failed.
    #[error("prepared call identity serialization failed")]
    Serialization,
    /// Serialized facts disagreed with their integrity identity.
    #[error("prepared call integrity mismatch")]
    IntegrityMismatch,
    /// A provider recovery locator was absent, malformed, or foreign.
    #[error(transparent)]
    CallRecovery(#[from] ModelCallRecoveryError),
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] crate::DigestError),
    /// Fresh model authority was invalid or stale.
    #[error(transparent)]
    RuntimeAuthority(#[from] RuntimeModelAuthorityError),
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
    fn from_context_admission(status: ContextStatus, error: ContextAdmissionError) -> Self {
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

/// One bounded, credential-free reachability operation.
pub type ReachabilityProbeOperation =
    Pin<Box<dyn Future<Output = Result<(), crate::ModelNetworkFailure>> + Send + 'static>>;

/// A cloneable probe bound to the same immutable route as an executable model.
#[derive(Clone)]
pub struct ReachabilityProbe {
    route: RouteId,
    run: Arc<
        dyn Fn(CancellationToken, std::time::Instant) -> ReachabilityProbeOperation + Send + Sync,
    >,
}

impl fmt::Debug for ReachabilityProbe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReachabilityProbe")
            .field("route", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ReachabilityProbe {
    /// Binds a content-free operation to an exact route identity.
    #[must_use]
    pub fn new(
        route: RouteId,
        run: impl Fn(CancellationToken, std::time::Instant) -> ReachabilityProbeOperation
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            route,
            run: Arc::new(run),
        }
    }

    /// Returns the immutable route key for sharing network readiness observations.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Starts one probe bounded by caller cancellation and an absolute deadline.
    pub fn run(
        &self,
        cancellation: CancellationToken,
        deadline: std::time::Instant,
    ) -> ReachabilityProbeOperation {
        (self.run)(cancellation, deadline)
    }
}

/// Opaque non-cloneable lifetime capability for one admitted model root.
///
/// The runtime retains this value from root admission through terminal
/// publication. Implementations may use `Drop` to close root-scoped authority.
pub trait ModelOperationScope: Send + Sync + 'static {}

struct UnscopedModelOperation;

impl ModelOperationScope for UnscopedModelOperation {}

/// The stable object-safe generation seam used by the agent runtime.
pub trait LanguageModel: Send + Sync + 'static {
    /// Returns a credential-free probe of this model's exact route, if available.
    /// Scripted and custom models without network reachability use `None`.
    fn reachability_probe(&self) -> Option<ReachabilityProbe> {
        None
    }
    /// Host-authored developer instruction for one content-filtered retry.
    ///
    /// Returning `None` disables this retry. The host records the returned
    /// fragment in canonical history before admitting another sampling call.
    fn content_filter_retry_instruction(&self) -> Option<String> {
        None
    }

    /// Claims model-owned authority for one complete admitted logical root.
    ///
    /// The default is inert for models without root-scoped resources. A model
    /// that meters subscription inference must return a nonrenewable scope and
    /// reject duplicate or retired root identities.
    ///
    /// # Errors
    ///
    /// Returns a typed preparation failure when exact root authority cannot be
    /// claimed without renewal or scope drift.
    fn begin_operation_root(
        self: Arc<Self>,
        _root: ModelOperationRoot,
    ) -> Result<Box<dyn ModelOperationScope>, ModelPreparationError> {
        Ok(Box::new(UnscopedModelOperation))
    }

    /// Revalidates the actual selected provider, account, credential authority,
    /// generation, model, and profile without returning secret material.
    ///
    /// The default is deliberately unavailable. A provider adapter must bind
    /// this operation to the same concrete model object and auth owner used for
    /// dispatch; descriptor bytes or caller-copied historical facts are not
    /// fresh authority.
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        _checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async { Err(RuntimeModelAuthorityError::Unavailable) })
    }

    /// Reads provider-owned evidence for one exact previously prepared call.
    ///
    /// The default is deliberately unsupported. Recovery is classification
    /// only: it must not start a new physical call, and `Ambiguous`,
    /// `Unsupported`, or `Unavailable` never authorize redispatch.
    fn recover_call(
        self: Arc<Self>,
        request: ModelCallRecoveryRequest,
    ) -> ModelCallRecoveryOperation {
        Box::pin(async move { ModelCallRecoveryResult::unsupported(&request) })
    }

    /// Lowers one neutral validated plan exactly once and returns its one-shot call.
    ///
    /// # Errors
    ///
    /// Returns typed preparation/evidence unavailability before the start
    /// capability can reach provider-visible work.
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError>;
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

fn validate_prepared_digest(digest: Digest, domain: DigestDomain) -> Result<(), PreparedCallError> {
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn authority_descriptor() -> Result<ResolvedModelDescriptor, crate::IdentityError> {
        Ok(ResolvedModelDescriptor::new(
            &crate::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test-protocol")?,
            RouteId::new("test-route")?,
            ModelId::new("test-model")?,
            None,
            None,
            ModelAdapterProfileId::new("test-adapter")?,
            crate::CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
        ))
    }

    fn authority_binding(label: &'static [u8]) -> Result<Digest, crate::DigestError> {
        Digest::sha256(DigestDomain::ModelBinding, 1, &[label])
    }

    fn assert_object_safe(_: Option<&dyn LanguageModel>) {}

    #[test]
    fn language_model_is_object_safe() {
        assert_object_safe(None);
    }

    #[test]
    fn cancellation_future_is_woken() {
        let token = CancellationToken::new();
        let second = token.clone();
        token.cancel();
        futures::executor::block_on(second.cancelled());
        assert!(second.is_cancelled());
    }

    #[test]
    fn authority_identity_is_clock_independent_but_generation_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let descriptor = authority_descriptor()?;
        let account = authority_binding(b"account-a")?;
        let credential = authority_binding(b"credential-a")?;
        let first = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            7,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(10, 0)?),
        )?;
        let later = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            7,
            Timestamp::new(2, 0)?,
            Some(Timestamp::new(20, 0)?),
        )?;
        assert_eq!(first.stable_identity()?, later.stable_identity()?);
        assert_eq!(
            first.authority_class_binding()?,
            later.authority_class_binding()?
        );
        assert!(first.same_selected_authority(&later));
        later.validate(&descriptor, Timestamp::new(3, 0)?)?;

        let rotated = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            8,
            Timestamp::new(2, 0)?,
            Some(Timestamp::new(20, 0)?),
        )?;
        assert_ne!(first.stable_identity()?, rotated.stable_identity()?);
        assert_eq!(
            first.authority_class_binding()?,
            rotated.authority_class_binding()?
        );
        assert!(!first.same_selected_authority(&rotated));

        let foreign_account = RuntimeModelAuthority::new(
            &descriptor,
            authority_binding(b"account-b")?,
            credential,
            7,
            Timestamp::new(2, 0)?,
            Some(Timestamp::new(20, 0)?),
        )?;
        assert_ne!(
            first.authority_class_binding()?,
            foreign_account.authority_class_binding()?
        );
        assert_eq!(
            first.validate(&descriptor, Timestamp::new(10, 0)?),
            Err(RuntimeModelAuthorityError::Stale)
        );
        Ok(())
    }

    #[test]
    fn admission_witness_is_bounded_integrity_checked_and_execution_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let descriptor = authority_descriptor()?;
        let account = authority_binding(b"account-a")?;
        let credential = authority_binding(b"credential-a")?;
        let witness = RuntimeAdmissionWitness::new(
            "test.selected-catalog-row",
            1,
            br#"{"model":"test-model","context_length":4096}"#.to_vec(),
        )?;
        assert_eq!(witness.kind(), "test.selected-catalog-row");
        assert_eq!(witness.revision(), 1);
        assert!(!format!("{witness:?}").contains("context_length"));

        let base = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            7,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(10, 0)?),
        )?;
        let admitted = base.clone().with_admission_witness(witness.clone());
        let refreshed = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            7,
            Timestamp::new(2, 0)?,
            Some(Timestamp::new(20, 0)?),
        )?
        .with_admission_witness(witness.clone());
        assert!(admitted.same_selected_authority(&refreshed));
        assert_eq!(admitted.stable_identity()?, refreshed.stable_identity()?);
        assert_eq!(
            admitted.authority_class_binding()?,
            base.authority_class_binding()?,
            "provider receipt evidence must not create an authority-class cycle"
        );
        assert_ne!(admitted.stable_identity()?, base.stable_identity()?);

        let changed = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            7,
            Timestamp::new(2, 0)?,
            Some(Timestamp::new(20, 0)?),
        )?
        .with_admission_witness(RuntimeAdmissionWitness::new(
            "test.selected-catalog-row",
            1,
            br#"{"model":"test-model","context_length":4095}"#.to_vec(),
        )?);
        assert!(!admitted.same_selected_authority(&changed));
        assert_ne!(admitted.stable_identity()?, changed.stable_identity()?);

        let encoded = serde_json::to_value(&witness)?;
        assert_eq!(
            serde_json::from_value::<RuntimeAdmissionWitness>(encoded.clone())?,
            witness
        );
        let mut tampered = encoded;
        tampered["payload"][0] = serde_json::json!(0);
        assert!(serde_json::from_value::<RuntimeAdmissionWitness>(tampered).is_err());
        assert_eq!(
            RuntimeAdmissionWitness::new("", 1, vec![1]),
            Err(RuntimeModelAuthorityError::Invalid)
        );
        assert_eq!(
            RuntimeAdmissionWitness::new("test", 0, vec![1]),
            Err(RuntimeModelAuthorityError::Invalid)
        );
        assert_eq!(
            RuntimeAdmissionWitness::new(
                "test",
                1,
                vec![0; MAX_RUNTIME_ADMISSION_WITNESS_BYTES + 1],
            ),
            Err(RuntimeModelAuthorityError::Invalid)
        );
        Ok(())
    }

    #[test]
    fn cache_affinity_roundtrip_recomputes_binding_and_redacts_debug()
    -> Result<(), Box<dyn std::error::Error>> {
        let affinity = ModelCacheAffinity::from_runtime_entropy(
            SessionId::try_from(uuid::Uuid::from_u128(10))?,
            BranchId::try_from(uuid::Uuid::from_u128(11))?,
            3,
            [7_u8; 32],
        )?;
        assert_eq!(affinity.identity().domain(), DigestDomain::CacheAffinity);
        assert!(!format!("{affinity:?}").contains("070707"));
        let encoded = serde_json::to_value(&affinity)?;
        assert_eq!(
            serde_json::from_value::<ModelCacheAffinity>(encoded.clone())?,
            affinity
        );
        let mut tampered = encoded;
        tampered["epoch"] = serde_json::json!(4);
        assert!(serde_json::from_value::<ModelCacheAffinity>(tampered).is_err());
        Ok(())
    }

    #[test]
    fn release_verified_output_hard_cap_binds_stable_exact_scope()
    -> Result<(), Box<dyn std::error::Error>> {
        let descriptor = authority_descriptor()?;
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            authority_binding(b"account-a")?,
            authority_binding(b"credential-a")?,
            1,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(10, 0)?),
        )?;
        let context = ModelPreparationContext::new(authority, Timestamp::new(2, 0)?);
        let certificate = Digest::sha256(DigestDomain::ContextBudget, 1, &[b"hard-cap"])?;
        let limit = PreparedOutputLimit::release_verified_hard_cap(
            NonZeroU32::new(64).ok_or("nonzero output")?,
            certificate,
            &descriptor,
            &context,
            descriptor.protocol(),
            "lowering-1",
            "projection-1",
            "accounting-1",
        )?;
        assert!(matches!(
            limit.source(),
            PreparedOutputLimitSource::ReleaseVerifiedHardCap {
                certificate: found,
                ..
            } if *found == certificate
        ));
        assert_eq!(
            serde_json::from_slice::<PreparedOutputLimit>(&serde_json::to_vec(&limit)?)?,
            limit
        );
        Ok(())
    }

    #[test]
    fn active_family_artifact_roundtrips_tamper_checks_and_stays_redacted()
    -> Result<(), Box<dyn std::error::Error>> {
        const SECRET_BYTES: &[u8] = b"OPAQUE_FAMILY_SECRET_SENTINEL";
        let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"])?;
        let artifact = ActiveFamilyArtifact::new("responses", 3, 3, binding, SECRET_BYTES)?;
        assert_eq!(artifact.family(), "responses");
        assert_eq!(artifact.schema_revision(), 3);
        assert_eq!(artifact.codec_revision(), 3);
        assert_eq!(artifact.binding(), binding);
        assert_eq!(artifact.bytes(), SECRET_BYTES);
        assert_eq!(artifact.integrity().domain(), DigestDomain::Blob);

        let encoded = serde_json::to_value(&artifact)?;
        assert_eq!(
            serde_json::from_value::<ActiveFamilyArtifact>(encoded.clone())?,
            artifact
        );
        let debug = format!("{artifact:?}");
        assert!(debug.contains("<redacted;"));
        assert!(!debug.contains("OPAQUE_FAMILY_SECRET_SENTINEL"));

        let mut tampered = encoded;
        tampered["bytes"][0] = serde_json::json!(0);
        assert!(serde_json::from_value::<ActiveFamilyArtifact>(tampered).is_err());
        Ok(())
    }

    #[test]
    fn active_family_artifact_enforces_revision_domain_and_size_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"])?;
        let wrong = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"binding"])?;
        assert_eq!(
            ActiveFamilyArtifact::new("responses", 0, 1, binding, [1_u8]),
            Err(FamilyArtifactError::ZeroRevision)
        );
        assert_eq!(
            ActiveFamilyArtifact::new("responses", 1, 1, wrong, [1_u8]),
            Err(FamilyArtifactError::InvalidDigest { field: "binding" })
        );
        assert_eq!(
            ActiveFamilyArtifact::new("responses", 1, 1, binding, Vec::<u8>::new()),
            Err(FamilyArtifactError::EmptyBytes)
        );
        assert_eq!(
            ActiveFamilyArtifact::new(
                "responses",
                1,
                1,
                binding,
                vec![0_u8; MAX_FAMILY_ARTIFACT_BYTES + 1],
            ),
            Err(FamilyArtifactError::BytesTooLarge {
                maximum: MAX_FAMILY_ARTIFACT_BYTES,
            })
        );
        assert!(ActiveFamilyArtifact::new("provider name", 1, 1, binding, [1_u8]).is_err());
        Ok(())
    }
}
