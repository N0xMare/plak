//! Bounded reconstructive native state for the Responses protocol family.
//!
//! The artifact records historical producer facts. Decoding it proves only
//! structural integrity; active continuation or replay additionally requires
//! a fresh exact-target and canonical-prefix validation. It is never a route,
//! credential, tool, history, checkpoint, or terminal authority.

use std::fmt;
use std::sync::Arc;

use plakous_model::{
    Actor, AttemptId, ContentBlock, DataClassification, Digest, DigestDomain,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelOperationRoot, ModelRequest, ProtocolId,
    Provenance, ReplayEnvelope, ReplayKindId, ResolvedModelDescriptor, RouteId, RunId, Timestamp,
    TurnId, ValidatedRequestPlan,
};

mod codec;
mod error;
mod native_compaction;
mod view;

pub use codec::ResponsesArtifactCodec;
use codec::{encode_body, envelope_len, integrity};
pub use error::ResponsesArtifactError;
use error::{validate_digest, validate_opaque_id};
pub use view::{
    ResponsesArtifactReuse, ResponsesArtifactReuseKind, ResponsesAuthorizedReplay,
    ResponsesReplayPurpose,
};

/// Current provisional family-artifact binary codec revision.
pub const RESPONSES_ARTIFACT_CODEC_REVISION: u32 = 6;
/// Current Responses artifact semantic schema revision.
pub const RESPONSES_ARTIFACT_SCHEMA_REVISION: u32 = 7;
/// Hard upper bound for one complete encoded Responses artifact.
pub const MAX_RESPONSES_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum output or replay items appended by one Responses response.
pub const MAX_RESPONSES_REPLAY_ITEMS: usize = 4_096;
/// Maximum aggregate replay-envelope bytes appended by one Responses response.
pub const MAX_RESPONSES_REPLAY_BYTES: usize = 3 * 1024 * 1024;

const MAGIC: &[u8; 8] = b"PLAKRSP\0";
const INTEGRITY_DOMAIN: &[u8] = b"plakous-responses-artifact-integrity-v3\0";
const BINDING_DOMAIN: &[u8] = b"plakous-responses-artifact-binding-v5\0";
const ENVELOPE_OVERHEAD: usize = MAGIC.len() + 4 + 4 + 32;
const MAX_BODY_BYTES: usize = MAX_RESPONSES_ARTIFACT_BYTES - ENVELOPE_OVERHEAD;
const MAX_ID_BYTES: usize = 256;
const MAX_OPAQUE_ID_BYTES: usize = 1_024;
const MAX_REPLAY_ITEM_BYTES: usize = 1024 * 1024;
const UUID_TEXT_BYTES: usize = 36;

/// The sole protocol family represented by this artifact codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesArtifactFamily {
    /// Responses request/event/session semantics.
    Responses,
}

impl ResponsesArtifactFamily {
    /// Returns the stable family label used by a neutral manifest wrapper.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
        }
    }
}

/// Exact behavior-bearing revisions required to interpret an artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesSemanticRevisions {
    family_kernel: u32,
    dialect: u32,
    adapter: u32,
}

impl ResponsesSemanticRevisions {
    /// Creates a certified semantic revision tuple.
    pub fn new(
        family_kernel: u32,
        dialect: u32,
        adapter: u32,
    ) -> Result<Self, ResponsesArtifactError> {
        for (field, revision) in [
            ("family kernel", family_kernel),
            ("dialect", dialect),
            ("adapter", adapter),
        ] {
            if revision == 0 {
                return Err(ResponsesArtifactError::ZeroRevision { field });
            }
        }
        Ok(Self {
            family_kernel,
            dialect,
            adapter,
        })
    }

    /// Returns the Responses kernel semantic revision.
    #[must_use]
    pub const fn family_kernel(self) -> u32 {
        self.family_kernel
    }

    /// Returns the dialect semantic revision.
    #[must_use]
    pub const fn dialect(self) -> u32 {
        self.dialect
    }

    /// Returns the model-adapter semantic revision.
    #[must_use]
    pub const fn adapter(self) -> u32 {
        self.adapter
    }
}

/// Exact non-secret admitted producer operation bound to native state.
///
/// The request and plan digests bind reconstruction data owned elsewhere; they
/// never substitute for those values in a runtime manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesArtifactBinding {
    dialect: ProtocolId,
    route: RouteId,
    account: Digest,
    model: ModelId,
    model_family: Option<ModelFamilyId>,
    adapter: ModelAdapterProfileId,
    semantic_model: Digest,
    revisions: ResponsesSemanticRevisions,
    cache_affinity: Option<Digest>,
    stable_prefix: Digest,
    projection: Digest,
    item_prefix: Digest,
    tool_plan: Digest,
    request: Digest,
    model_input: Digest,
    validated_plan: Digest,
    run: RunId,
    root: ModelOperationRoot,
    attempt: AttemptId,
    identity: Digest,
}

impl ResponsesArtifactBinding {
    /// Captures an exact binding including the runtime-owned branch cache affinity.
    ///
    /// # Errors
    ///
    /// Rejects a foreign plan, missing request identity, invalid digest domain,
    /// or a binding whose identity cannot be constructed.
    pub fn from_admitted_operation(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        request: &ModelRequest,
        account: Digest,
        revisions: ResponsesSemanticRevisions,
        cache_affinity: Option<Digest>,
    ) -> Result<Self, ResponsesArtifactError> {
        descriptor
            .validate_plan_admission(request, plan)
            .map_err(|_| ResponsesArtifactError::AdmissionMismatch)?;
        let semantic_model = descriptor
            .semantic_binding_digest()
            .map_err(|_| ResponsesArtifactError::AdmissionMismatch)?;
        let tool_plan = request
            .resolved_harness_plan_identity()
            .ok_or(ResponsesArtifactError::MissingHarnessPlanIdentity)?;
        let metadata = request.metadata();
        let item_prefix = request
            .input()
            .canonical_item_prefix_identity(request.input().items().len())
            .map_err(|_| ResponsesArtifactError::AdmissionMismatch)?
            .identity();
        Self::from_parts(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            account,
            descriptor.requested_model().clone(),
            descriptor.model_family().cloned(),
            descriptor.profile().clone(),
            semantic_model,
            revisions,
            cache_affinity,
            request.stable_prefix().reuse_identity(),
            request.input().identity(),
            item_prefix,
            tool_plan,
            request.identity(),
            request.model_input_identity(),
            plan.digest(),
            metadata.run_id(),
            metadata.root(),
            metadata.attempt_id(),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the closed exact artifact producer binding"
    )]
    fn from_parts(
        dialect: ProtocolId,
        route: RouteId,
        account: Digest,
        model: ModelId,
        model_family: Option<ModelFamilyId>,
        adapter: ModelAdapterProfileId,
        semantic_model: Digest,
        revisions: ResponsesSemanticRevisions,
        cache_affinity: Option<Digest>,
        stable_prefix: Digest,
        projection: Digest,
        item_prefix: Digest,
        tool_plan: Digest,
        request: Digest,
        model_input: Digest,
        validated_plan: Digest,
        run: RunId,
        root: ModelOperationRoot,
        attempt: AttemptId,
    ) -> Result<Self, ResponsesArtifactError> {
        validate_digest(account, DigestDomain::ModelBinding, "account")?;
        validate_digest(semantic_model, DigestDomain::ModelBinding, "semantic model")?;
        if let Some(cache_affinity) = cache_affinity {
            validate_digest(
                cache_affinity,
                DigestDomain::CacheAffinity,
                "cache affinity",
            )?;
        }
        validate_digest(
            stable_prefix,
            DigestDomain::StableRequestPrefix,
            "stable prefix",
        )?;
        validate_digest(
            projection,
            DigestDomain::HistoryProjection,
            "history projection",
        )?;
        validate_digest(
            item_prefix,
            DigestDomain::CanonicalItemPrefix,
            "canonical item prefix",
        )?;
        validate_digest(tool_plan, DigestDomain::ResolvedHarnessPlan, "tool plan")?;
        validate_digest(request, DigestDomain::RequestPlan, "request")?;
        validate_digest(model_input, DigestDomain::RequestPlan, "model input")?;
        validate_digest(validated_plan, DigestDomain::RequestPlan, "validated plan")?;
        let identity = artifact_binding_identity(
            &dialect,
            &route,
            account,
            &model,
            model_family.as_ref(),
            &adapter,
            semantic_model,
            revisions,
            cache_affinity,
            stable_prefix,
            projection,
            item_prefix,
            tool_plan,
            request,
            model_input,
            validated_plan,
            run,
            root,
            attempt,
        )?;
        Ok(Self {
            dialect,
            route,
            account,
            model,
            model_family,
            adapter,
            semantic_model,
            revisions,
            cache_affinity,
            stable_prefix,
            projection,
            item_prefix,
            tool_plan,
            request,
            model_input,
            validated_plan,
            run,
            root,
            attempt,
            identity,
        })
    }

    /// Returns the closed family identity.
    #[must_use]
    pub const fn family(&self) -> ResponsesArtifactFamily {
        ResponsesArtifactFamily::Responses
    }

    /// Returns the exact protocol dialect identity.
    #[must_use]
    pub const fn dialect(&self) -> &ProtocolId {
        &self.dialect
    }

    /// Returns the exact configured route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the non-secret exact billed-account binding.
    #[must_use]
    pub const fn account(&self) -> Digest {
        self.account
    }

    /// Returns the exact requested model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the optional exact model-family identity.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the exact admitted model-adapter identity.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }

    /// Returns the stable behavior-bearing descriptor/capability identity.
    #[must_use]
    pub const fn semantic_model(&self) -> Digest {
        self.semantic_model
    }

    /// Returns all behavior-bearing semantic revisions.
    #[must_use]
    pub const fn revisions(&self) -> ResponsesSemanticRevisions {
        self.revisions
    }

    /// Returns the runtime-owned branch cache-affinity identity, when present.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<Digest> {
        self.cache_affinity
    }

    /// Returns the producer's stable-prefix reuse identity.
    ///
    /// This is [`plakous_model::StableRequestPrefix::reuse_identity`], so a
    /// round that changes only tool selection still names the same prefix.
    #[must_use]
    pub const fn stable_prefix(&self) -> Digest {
        self.stable_prefix
    }

    /// Returns the producing history-projection identity.
    #[must_use]
    pub const fn projection(&self) -> Digest {
        self.projection
    }

    /// Returns the producing ordered canonical-item prefix identity.
    #[must_use]
    pub const fn item_prefix(&self) -> Digest {
        self.item_prefix
    }

    /// Returns the resolved tool/harness-plan identity.
    #[must_use]
    pub const fn tool_plan(&self) -> Digest {
        self.tool_plan
    }

    /// Returns the complete producing request identity, including attempt metadata.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }

    /// Returns the exact model-visible producing request identity.
    #[must_use]
    pub const fn model_input(&self) -> Digest {
        self.model_input
    }

    /// Returns the exact admitted request-plan identity.
    #[must_use]
    pub const fn validated_plan(&self) -> Digest {
        self.validated_plan
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

    /// Returns the producing turn only for an ordinary turn-rooted operation.
    #[must_use]
    pub const fn turn(&self) -> Option<TurnId> {
        self.root.turn_id()
    }

    /// Returns the producing logical-attempt identity.
    #[must_use]
    pub const fn attempt(&self) -> AttemptId {
        self.attempt
    }

    /// Returns a complete non-secret binding identity for a neutral manifest wrapper.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Checks the request's replay prefix, including the closed native-trigger refinement.
    ///
    /// This check alone grants no replay authority: target, expiry and canonical
    /// coverage must also pass [`ResponsesArtifact::validate_for_reuse`].
    #[must_use]
    pub fn matches_replay_prefix(&self, request: &ModelRequest) -> bool {
        self.stable_prefix == request.stable_prefix().reuse_identity()
            || native_compaction::source_prefix(request) == Some(self.stable_prefix)
    }

    fn same_reuse_target(&self, current: &Self, request: &ModelRequest) -> bool {
        self.dialect == current.dialect
            && self.route == current.route
            && self.account == current.account
            && self.model == current.model
            && self.model_family == current.model_family
            && self.adapter == current.adapter
            && self.semantic_model == current.semantic_model
            && self.revisions == current.revisions
            && self.cache_affinity == current.cache_affinity
            && self.matches_replay_prefix(request)
            && self.tool_plan == current.tool_plan
    }

    fn names_request(&self, request: &ModelRequest) -> bool {
        let metadata = request.metadata();
        self.request == request.identity()
            && self.model_input == request.model_input_identity()
            && self.stable_prefix == request.stable_prefix().reuse_identity()
            && self.projection == request.input().identity()
            && request
                .input()
                .canonical_item_prefix_identity(request.input().items().len())
                .is_ok_and(|prefix| self.item_prefix == prefix.identity())
            && request.resolved_harness_plan_identity() == Some(self.tool_plan)
            && self.run == metadata.run_id()
            && self.root == metadata.root()
            && self.attempt == metadata.attempt_id()
    }
}

/// Exact physical and canonical production facts for one terminal response group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesArtifactProduction {
    producer_request: Digest,
    producing_projection: Digest,
    validated_plan: Digest,
    physical_request: Digest,
    terminal_response: Digest,
    producing_projection_items: u32,
    output_items: u32,
    covered_projection_items: u32,
    covered_projection: Digest,
}

#[derive(Clone, Copy)]
struct ResponsesArtifactProductionParts {
    producer_request: Digest,
    producing_projection: Digest,
    validated_plan: Digest,
    physical_request: Digest,
    terminal_response: Digest,
    producing_projection_items: u32,
    output_items: u32,
    covered_projection_items: u32,
    covered_projection: Digest,
}

impl ResponsesArtifactProduction {
    /// Binds a terminal response and its canonical output coverage to one admitted request.
    pub fn from_admitted_response(
        request: &ModelRequest,
        plan: &ValidatedRequestPlan,
        physical_request: Digest,
        terminal_response: Digest,
        output_items: u32,
        covered_projection: Digest,
    ) -> Result<Self, ResponsesArtifactError> {
        if plan.request().identity() != request.identity() {
            return Err(ResponsesArtifactError::AdmissionMismatch);
        }
        let producing_projection = request
            .input()
            .canonical_item_prefix_identity(request.input().items().len())
            .map_err(|_| ResponsesArtifactError::AdmissionMismatch)?
            .identity();
        let producing_projection_items = u32::try_from(request.input().items().len())
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        Self::validated(ResponsesArtifactProductionParts {
            producer_request: request.identity(),
            producing_projection,
            validated_plan: plan.digest(),
            physical_request,
            terminal_response,
            producing_projection_items,
            output_items,
            covered_projection_items: producing_projection_items
                .checked_add(output_items)
                .ok_or(ResponsesArtifactError::InvalidCoordinates)?,
            covered_projection,
        })
    }

    fn validated(parts: ResponsesArtifactProductionParts) -> Result<Self, ResponsesArtifactError> {
        let ResponsesArtifactProductionParts {
            producer_request,
            producing_projection,
            validated_plan,
            physical_request,
            terminal_response,
            producing_projection_items,
            output_items,
            covered_projection_items,
            covered_projection,
        } = parts;
        validate_digest(
            producer_request,
            DigestDomain::RequestPlan,
            "producer request",
        )?;
        validate_digest(
            producing_projection,
            DigestDomain::CanonicalItemPrefix,
            "producing item prefix",
        )?;
        validate_digest(validated_plan, DigestDomain::RequestPlan, "validated plan")?;
        validate_digest(
            physical_request,
            DigestDomain::ProviderOperation,
            "physical request",
        )?;
        validate_digest(
            terminal_response,
            DigestDomain::ProviderOperation,
            "terminal response",
        )?;
        validate_digest(
            covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "covered item prefix",
        )?;
        if physical_request == terminal_response
            || producing_projection_items.checked_add(output_items)
                != Some(covered_projection_items)
        {
            return Err(ResponsesArtifactError::InvalidCoordinates);
        }
        Ok(Self {
            producer_request,
            producing_projection,
            validated_plan,
            physical_request,
            terminal_response,
            producing_projection_items,
            output_items,
            covered_projection_items,
            covered_projection,
        })
    }

    /// Returns the exact admitted request that produced the physical operation.
    #[must_use]
    pub const fn producer_request(&self) -> Digest {
        self.producer_request
    }

    /// Returns the canonical projection identity before the response group.
    #[must_use]
    pub const fn producing_projection(&self) -> Digest {
        self.producing_projection
    }

    /// Returns the exact validated plan that produced the physical operation.
    #[must_use]
    pub const fn validated_plan(&self) -> Digest {
        self.validated_plan
    }

    /// Returns the host-verified physical request identity.
    #[must_use]
    pub const fn physical_request(&self) -> Digest {
        self.physical_request
    }

    /// Returns the exact terminal response aggregate identity.
    #[must_use]
    pub const fn terminal_response(&self) -> Digest {
        self.terminal_response
    }

    /// Returns the canonical item count before this response group.
    #[must_use]
    pub const fn producing_projection_items(&self) -> u32 {
        self.producing_projection_items
    }

    /// Returns the exact native/canonical output count in this response group.
    #[must_use]
    pub const fn output_items(&self) -> u32 {
        self.output_items
    }

    /// Returns the canonical prefix count after this response group.
    #[must_use]
    pub const fn covered_projection_items(&self) -> u32 {
        self.covered_projection_items
    }

    /// Returns the exact canonical prefix identity after this response group.
    #[must_use]
    pub const fn covered_projection(&self) -> Digest {
        self.covered_projection
    }
}

/// One exact dialect-owned replay kind and payload revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesArtifactReplayKind {
    kind: ReplayKindId,
    version: u32,
}

impl ResponsesArtifactReplayKind {
    /// Creates one nonzero exact replay-kind pin.
    pub fn new(kind: ReplayKindId, version: u32) -> Result<Self, ResponsesArtifactError> {
        if version == 0 {
            return Err(ResponsesArtifactError::ZeroRevision {
                field: "replay kind",
            });
        }
        if kind.as_str().len() > MAX_ID_BYTES {
            return Err(ResponsesArtifactError::InvalidIdentity {
                field: "replay kind",
            });
        }
        Ok(Self { kind, version })
    }

    /// Returns the exact replay kind.
    #[must_use]
    pub const fn kind(&self) -> &ReplayKindId {
        &self.kind
    }

    /// Returns the exact replay payload revision.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    fn matches(&self, envelope: &ReplayEnvelope) -> bool {
        self.kind == *envelope.kind() && self.version == envelope.version()
    }
}

/// Closed purpose-to-kind relation admitted by one exact dialect revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesReplayContract {
    output: ResponsesArtifactReplayKind,
    opaque_output: Option<ResponsesArtifactReplayKind>,
    encrypted_reasoning: Option<ResponsesArtifactReplayKind>,
}

impl ResponsesReplayContract {
    /// Creates a contract with one mandatory ordinary output-item kind.
    #[must_use]
    pub const fn new(output: ResponsesArtifactReplayKind) -> Self {
        Self {
            output,
            opaque_output: None,
            encrypted_reasoning: None,
        }
    }

    /// Adds a distinct exact opaque-output kind.
    pub fn with_opaque_output(
        mut self,
        kind: ResponsesArtifactReplayKind,
    ) -> Result<Self, ResponsesArtifactError> {
        self.ensure_distinct(&kind)?;
        self.opaque_output = Some(kind);
        Ok(self)
    }

    /// Adds a distinct exact encrypted-reasoning kind.
    pub fn with_encrypted_reasoning(
        mut self,
        kind: ResponsesArtifactReplayKind,
    ) -> Result<Self, ResponsesArtifactError> {
        self.ensure_distinct(&kind)?;
        self.encrypted_reasoning = Some(kind);
        Ok(self)
    }

    /// Returns the exact pin for a closed replay purpose, when admitted.
    #[must_use]
    pub const fn for_purpose(
        &self,
        purpose: ResponsesReplayPurpose,
    ) -> Option<&ResponsesArtifactReplayKind> {
        match purpose {
            ResponsesReplayPurpose::OutputItem => Some(&self.output),
            ResponsesReplayPurpose::OpaqueOutputItem => self.opaque_output.as_ref(),
            ResponsesReplayPurpose::EncryptedReasoning => self.encrypted_reasoning.as_ref(),
        }
    }

    fn purpose_for_kind(&self, kind: &ReplayKindId) -> Option<ResponsesReplayPurpose> {
        if self.output.kind == *kind {
            Some(ResponsesReplayPurpose::OutputItem)
        } else if self
            .opaque_output
            .as_ref()
            .is_some_and(|candidate| candidate.kind == *kind)
        {
            Some(ResponsesReplayPurpose::OpaqueOutputItem)
        } else if self
            .encrypted_reasoning
            .as_ref()
            .is_some_and(|candidate| candidate.kind == *kind)
        {
            Some(ResponsesReplayPurpose::EncryptedReasoning)
        } else {
            None
        }
    }

    fn ensure_distinct(
        &self,
        candidate: &ResponsesArtifactReplayKind,
    ) -> Result<(), ResponsesArtifactError> {
        if self.output.kind == candidate.kind
            || self
                .opaque_output
                .as_ref()
                .is_some_and(|kind| kind.kind == candidate.kind)
            || self
                .encrypted_reasoning
                .as_ref()
                .is_some_and(|kind| kind.kind == candidate.kind)
        {
            return Err(ResponsesArtifactError::AmbiguousReplayContract);
        }
        Ok(())
    }
}

/// One exact native replay item at a response-relative canonical coordinate.
#[derive(Clone, PartialEq)]
pub struct ResponsesReplayItem {
    purpose: ResponsesReplayPurpose,
    output_index: u32,
    envelope: ReplayEnvelope,
}

impl ResponsesReplayItem {
    /// Wraps a canonical replay envelope at one response-relative output coordinate.
    #[must_use]
    pub const fn new(
        purpose: ResponsesReplayPurpose,
        output_index: u32,
        envelope: ReplayEnvelope,
    ) -> Self {
        Self {
            purpose,
            output_index,
            envelope,
        }
    }

    /// Returns why this native item is retained.
    #[must_use]
    pub const fn purpose(&self) -> ResponsesReplayPurpose {
        self.purpose
    }

    /// Returns the relative output coordinate inside its physical response group.
    #[must_use]
    pub const fn output_index(&self) -> u32 {
        self.output_index
    }

    /// Returns the exact scoped replay envelope.
    #[must_use]
    pub const fn envelope(&self) -> &ReplayEnvelope {
        &self.envelope
    }
}

impl fmt::Debug for ResponsesReplayItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesReplayItem")
            .field("purpose", &self.purpose)
            .field("output_index", &self.output_index)
            .field("envelope", &self.envelope)
            .finish()
    }
}

/// Exact provider-stored continuation fact and absolute canonical coverage.
#[derive(Clone, Eq, PartialEq)]
pub struct ResponsesContinuation {
    response_id: String,
    covered_projection_items: u32,
    covered_projection: Digest,
    terminal_response: Digest,
}

impl ResponsesContinuation {
    /// Creates a bounded continuation bound to one full canonical prefix and response.
    pub fn new(
        response_id: impl Into<String>,
        covered_projection_items: u32,
        covered_projection: Digest,
        terminal_response: Digest,
    ) -> Result<Self, ResponsesArtifactError> {
        let response_id = response_id.into();
        validate_opaque_id(&response_id, "continuation response id")?;
        if covered_projection_items == 0 {
            return Err(ResponsesArtifactError::ZeroProjectionItems);
        }
        validate_digest(
            covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "continuation covered item prefix",
        )?;
        validate_digest(
            terminal_response,
            DigestDomain::ProviderOperation,
            "continuation terminal response",
        )?;
        Ok(Self {
            response_id,
            covered_projection_items,
            covered_projection,
            terminal_response,
        })
    }

    /// Returns the exact provider response identity.
    #[must_use]
    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    /// Returns the absolute canonical prefix count covered by the response.
    #[must_use]
    pub const fn covered_projection_items(&self) -> u32 {
        self.covered_projection_items
    }

    /// Returns the exact covered canonical-prefix identity.
    #[must_use]
    pub const fn covered_projection(&self) -> Digest {
        self.covered_projection
    }

    /// Returns the exact terminal response that produced this continuation.
    #[must_use]
    pub const fn terminal_response(&self) -> Digest {
        self.terminal_response
    }
}

impl fmt::Debug for ResponsesContinuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesContinuation")
            .field("response_id", &"<redacted>")
            .field("covered_projection_items", &self.covered_projection_items)
            .field("covered_projection", &self.covered_projection)
            .field("terminal_response", &self.terminal_response)
            .finish()
    }
}

/// Exact provider cache key and stable/canonical prefix it covers.
#[derive(Clone, Eq, PartialEq)]
pub struct ResponsesCacheFact {
    key: String,
    stable_prefix: Digest,
    covered_projection_items: u32,
    covered_projection: Digest,
}

impl ResponsesCacheFact {
    /// Creates a bounded cache fact with exact stable and canonical coverage.
    pub fn new(
        key: impl Into<String>,
        stable_prefix: Digest,
        covered_projection_items: u32,
        covered_projection: Digest,
    ) -> Result<Self, ResponsesArtifactError> {
        let key = key.into();
        validate_opaque_id(&key, "cache key")?;
        validate_digest(
            stable_prefix,
            DigestDomain::StableRequestPrefix,
            "cache stable prefix",
        )?;
        validate_digest(
            covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "cache covered item prefix",
        )?;
        Ok(Self {
            key,
            stable_prefix,
            covered_projection_items,
            covered_projection,
        })
    }

    /// Returns the exact cache key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the stable-prefix identity covered by this cache key.
    #[must_use]
    pub const fn stable_prefix(&self) -> Digest {
        self.stable_prefix
    }

    /// Returns the canonical prefix item count covered by this cache key.
    #[must_use]
    pub const fn covered_projection_items(&self) -> u32 {
        self.covered_projection_items
    }

    /// Returns the canonical prefix identity covered by this cache key.
    #[must_use]
    pub const fn covered_projection(&self) -> Digest {
        self.covered_projection
    }
}

impl fmt::Debug for ResponsesCacheFact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesCacheFact")
            .field("key", &"<redacted>")
            .field("stable_prefix", &self.stable_prefix)
            .field("covered_projection_items", &self.covered_projection_items)
            .field("covered_projection", &self.covered_projection)
            .finish()
    }
}

/// Provider response-retention mode captured with native state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesStorageMode {
    /// The provider retained no response; exact local replay is required.
    Stateless,
    /// The exact bound route requested provider-side response storage.
    ProviderStored,
}

/// One closed, bounded, reconstructive Responses-family native artifact.
#[derive(Clone, PartialEq)]
pub struct ResponsesArtifact {
    binding: ResponsesArtifactBinding,
    production: ResponsesArtifactProduction,
    replay_contract: ResponsesReplayContract,
    continuation: Option<ResponsesContinuation>,
    cache: Option<ResponsesCacheFact>,
    storage: ResponsesStorageMode,
    expires_at: Option<Timestamp>,
    integrity: [u8; 32],
    encoded_len: usize,
    replay_bytes: usize,
    canonical_body: Arc<[u8]>,
}

impl ResponsesArtifact {
    /// Captures complete family-native state and rejects stale or inconsistent facts.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the closed reconstructive artifact body"
    )]
    pub fn new(
        binding: ResponsesArtifactBinding,
        production: ResponsesArtifactProduction,
        replay_contract: ResponsesReplayContract,
        replay: Vec<ResponsesReplayItem>,
        continuation: Option<ResponsesContinuation>,
        cache: Option<ResponsesCacheFact>,
        storage: ResponsesStorageMode,
        expires_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<Self, ResponsesArtifactError> {
        let mut artifact = Self {
            binding,
            production,
            replay_contract,
            continuation,
            cache,
            storage,
            expires_at,
            integrity: [0; 32],
            encoded_len: 0,
            replay_bytes: 0,
            canonical_body: Arc::from([]),
        };
        artifact.validate_structure()?;
        artifact.validate_append_input(&replay)?;
        drop(replay);
        artifact.ensure_reusable(now)?;
        let encoded = encode_body(&artifact)?;
        artifact.integrity = integrity(&encoded.body)?;
        artifact.encoded_len = envelope_len(encoded.body.len())?;
        artifact.replay_bytes = encoded.replay_bytes;
        artifact.canonical_body = encoded.body.into();
        Ok(artifact)
    }

    /// Revalidates this artifact against its exact admitted producer and time.
    pub fn validate_for(
        &self,
        expected: &ResponsesArtifactBinding,
        expected_production: &ResponsesArtifactProduction,
        now: Timestamp,
    ) -> Result<(), ResponsesArtifactError> {
        if &self.binding != expected || &self.production != expected_production {
            return Err(ResponsesArtifactError::BindingMismatch);
        }
        self.validate_active(now)
    }

    /// Validates this historical artifact for one exact current admitted request.
    ///
    /// The current attempt is intentionally different from the producer. Every
    /// target axis and both producing/covered canonical prefix identities must
    /// still match before the returned wrapper exposes any active native state.
    pub fn validate_for_reuse<'a>(
        &'a self,
        current: &ResponsesArtifactBinding,
        request: &'a ModelRequest,
        now: Timestamp,
    ) -> Result<ResponsesArtifactReuse<'a>, ResponsesArtifactError> {
        self.validate_active(now)?;
        if !current.names_request(request) || !self.binding.same_reuse_target(current, request) {
            return Err(ResponsesArtifactError::BindingMismatch);
        }
        let replay = self.validate_canonical_coverage(request)?;
        let kind = if self.continuation.is_some() {
            ResponsesArtifactReuseKind::ProviderContinuation
        } else if self.production.output_items > 0 {
            ResponsesArtifactReuseKind::LocalReplay
        } else {
            ResponsesArtifactReuseKind::CacheOnly
        };
        Ok(ResponsesArtifactReuse::new(self, kind, replay))
    }

    /// Revalidates canonical structure and the stored body integrity witness.
    ///
    /// This is intentionally temporal-neutral: expired historical artifacts
    /// remain inspectable, exportable, and diagnosable but gain no reuse right.
    pub fn validate_integrity(&self) -> Result<(), ResponsesArtifactError> {
        self.validate_structure()?;
        if integrity(&self.canonical_body)? != self.integrity
            || self.encoded_len != envelope_len(self.canonical_body.len())?
        {
            return Err(ResponsesArtifactError::IntegrityMismatch);
        }
        Ok(())
    }

    /// Checks structural integrity and the temporal validity boundary.
    ///
    /// This is only one prerequisite for active reuse. It grants no target or
    /// canonical-coordinate authority; callers must still use
    /// [`Self::validate_for_reuse`] before lowering native state.
    pub fn validate_active(&self, now: Timestamp) -> Result<(), ResponsesArtifactError> {
        self.validate_integrity()?;
        self.ensure_reusable(now)
    }

    /// Returns the artifact schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        RESPONSES_ARTIFACT_SCHEMA_REVISION
    }

    /// Returns the exact family/target/producer binding.
    #[must_use]
    pub const fn binding(&self) -> &ResponsesArtifactBinding {
        &self.binding
    }

    /// Returns exact physical and canonical production facts.
    #[must_use]
    pub const fn production(&self) -> &ResponsesArtifactProduction {
        &self.production
    }

    /// Returns the exact closed replay purpose-to-kind relation.
    #[must_use]
    pub const fn replay_contract(&self) -> &ResponsesReplayContract {
        &self.replay_contract
    }

    /// Returns the inert provider-stored continuation fact when present.
    #[must_use]
    pub const fn continuation(&self) -> Option<&ResponsesContinuation> {
        self.continuation.as_ref()
    }

    /// Returns the inert exact provider cache fact when present.
    #[must_use]
    pub const fn cache(&self) -> Option<&ResponsesCacheFact> {
        self.cache.as_ref()
    }

    /// Returns the exact response-retention mode.
    #[must_use]
    pub const fn storage(&self) -> ResponsesStorageMode {
        self.storage
    }

    /// Returns the aggregate validity boundary, if one was supplied.
    #[must_use]
    pub const fn expires_at(&self) -> Option<Timestamp> {
        self.expires_at
    }

    /// Reports whether this artifact is expired at `now`.
    #[must_use]
    pub fn is_expired(&self, now: Timestamp) -> bool {
        self.expires_at.is_some_and(|expiry| expiry <= now)
    }

    /// Returns the SHA-256 integrity witness over the complete canonical body.
    #[must_use]
    pub const fn integrity(&self) -> &[u8; 32] {
        &self.integrity
    }

    /// Returns the exact byte length emitted by [`ResponsesArtifactCodec`].
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    /// Returns aggregate canonical replay-envelope bytes retained by the artifact.
    #[must_use]
    pub const fn replay_bytes(&self) -> usize {
        self.replay_bytes
    }

    fn ensure_reusable(&self, now: Timestamp) -> Result<(), ResponsesArtifactError> {
        if self.is_expired(now) {
            Err(ResponsesArtifactError::Expired)
        } else {
            Ok(())
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one linear pass enforces the closed replay, storage, and coverage relation"
    )]
    fn validate_structure(&self) -> Result<(), ResponsesArtifactError> {
        if self.production.output_items == 0 && self.continuation.is_none() && self.cache.is_none()
        {
            return Err(ResponsesArtifactError::EmptyArtifact);
        }
        if self
            .production
            .producing_projection_items
            .checked_add(self.production.output_items)
            != Some(self.production.covered_projection_items)
        {
            return Err(ResponsesArtifactError::InvalidCoordinates);
        }
        if self.production.producer_request != self.binding.request
            || self.production.producing_projection != self.binding.item_prefix
            || self.production.validated_plan != self.binding.validated_plan
        {
            return Err(ResponsesArtifactError::ProductionBindingMismatch);
        }

        let output_items = usize::try_from(self.production.output_items)
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        if output_items > MAX_RESPONSES_REPLAY_ITEMS {
            return Err(ResponsesArtifactError::TooManyReplayItems {
                maximum: MAX_RESPONSES_REPLAY_ITEMS,
            });
        }
        self.validate_active_state()
    }

    fn validate_append_input(
        &self,
        replay: &[ResponsesReplayItem],
    ) -> Result<(), ResponsesArtifactError> {
        if replay.len() > MAX_RESPONSES_REPLAY_ITEMS {
            return Err(ResponsesArtifactError::TooManyReplayItems {
                maximum: MAX_RESPONSES_REPLAY_ITEMS,
            });
        }
        let encrypted_slots_len = usize::try_from(self.production.output_items)
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        let mut encrypted_slots = vec![false; encrypted_slots_len];
        let mut next_output = 0_u32;
        let mut aggregate_payload = 0_usize;
        for (index, item) in replay.iter().enumerate() {
            let expected = self
                .replay_contract
                .for_purpose(item.purpose)
                .filter(|kind| kind.matches(&item.envelope))
                .ok_or(ResponsesArtifactError::ReplayPurposeKindMismatch { index })?;
            let _ = expected;
            if !item.envelope.is_compatible(
                &self.binding.dialect,
                self.binding.model_family.as_ref(),
                &self.binding.model,
                Some(self.binding.account),
            ) {
                return Err(ResponsesArtifactError::ReplayBindingMismatch { index });
            }
            if item.purpose == ResponsesReplayPurpose::EncryptedReasoning
                && item.envelope.classification() == DataClassification::Public
            {
                return Err(ResponsesArtifactError::PublicEncryptedReasoning { index });
            }
            if item.envelope.classification() == DataClassification::Secret {
                return Err(ResponsesArtifactError::SecretReplayMaterial { index });
            }
            let payload_bytes = item.envelope.payload().get().len();
            if payload_bytes > MAX_REPLAY_ITEM_BYTES {
                return Err(ResponsesArtifactError::ReplayItemTooLarge {
                    index,
                    maximum: MAX_REPLAY_ITEM_BYTES,
                });
            }
            aggregate_payload = aggregate_payload.checked_add(payload_bytes).ok_or(
                ResponsesArtifactError::ReplayBytesTooLarge {
                    maximum: MAX_RESPONSES_REPLAY_BYTES,
                },
            )?;
            if aggregate_payload > MAX_RESPONSES_REPLAY_BYTES {
                return Err(ResponsesArtifactError::ReplayBytesTooLarge {
                    maximum: MAX_RESPONSES_REPLAY_BYTES,
                });
            }
            if item.purpose.is_output() {
                if item.output_index != next_output {
                    return Err(ResponsesArtifactError::InvalidReplayCoordinate { index });
                }
                next_output = next_output
                    .checked_add(1)
                    .ok_or(ResponsesArtifactError::InvalidCoordinates)?;
            } else {
                let slot = usize::try_from(item.output_index)
                    .map_err(|_| ResponsesArtifactError::InvalidReplayCoordinate { index })?;
                let occupied = encrypted_slots
                    .get_mut(slot)
                    .ok_or(ResponsesArtifactError::InvalidReplayCoordinate { index })?;
                if std::mem::replace(occupied, true) {
                    return Err(ResponsesArtifactError::DuplicateReplayCoordinate { index });
                }
            }
        }
        if next_output != self.production.output_items {
            return Err(ResponsesArtifactError::IncompleteReplayCoverage);
        }
        Ok(())
    }

    fn validate_active_state(&self) -> Result<(), ResponsesArtifactError> {
        match (
            self.storage,
            self.production.output_items,
            &self.continuation,
        ) {
            (ResponsesStorageMode::Stateless, _, Some(_)) => {
                return Err(ResponsesArtifactError::ContinuationWithoutStorage);
            }
            (ResponsesStorageMode::ProviderStored, output, None) if output > 0 => {
                return Err(ResponsesArtifactError::MissingStoredContinuation);
            }
            (_, _, _) => {}
        }
        if let Some(continuation) = &self.continuation
            && (continuation.covered_projection_items != self.production.covered_projection_items
                || continuation.covered_projection != self.production.covered_projection
                || continuation.terminal_response != self.production.terminal_response)
        {
            return Err(ResponsesArtifactError::ContinuationCoverageMismatch);
        }
        if let Some(cache) = &self.cache {
            if cache.stable_prefix != self.binding.stable_prefix {
                return Err(ResponsesArtifactError::CachePrefixMismatch);
            }
            let producing = cache.covered_projection_items
                == self.production.producing_projection_items
                && cache.covered_projection == self.binding.item_prefix;
            let covered = cache.covered_projection_items
                == self.production.covered_projection_items
                && cache.covered_projection == self.production.covered_projection;
            if !producing && !covered {
                return Err(ResponsesArtifactError::CacheCoverageMismatch);
            }
        }
        Ok(())
    }

    fn validate_canonical_coverage<'a>(
        &self,
        request: &'a ModelRequest,
    ) -> Result<Vec<ResponsesAuthorizedReplay<'a>>, ResponsesArtifactError> {
        let producing = usize::try_from(self.production.producing_projection_items)
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        let covered = usize::try_from(self.production.covered_projection_items)
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        let items = request.input().items();
        if items.len() < covered
            || request
                .input()
                .canonical_item_prefix_identity(producing)
                .map_err(|_| ResponsesArtifactError::CanonicalCoordinateMismatch)?
                .identity()
                != self.binding.item_prefix
            || request
                .input()
                .canonical_item_prefix_identity(covered)
                .map_err(|_| ResponsesArtifactError::CanonicalCoordinateMismatch)?
                .identity()
                != self.production.covered_projection
        {
            return Err(ResponsesArtifactError::CanonicalCoordinateMismatch);
        }
        if let Some(cache) = &self.cache {
            let cache_items = usize::try_from(cache.covered_projection_items)
                .map_err(|_| ResponsesArtifactError::CanonicalCoordinateMismatch)?;
            if request
                .input()
                .canonical_item_prefix_identity(cache_items)
                .map_err(|_| ResponsesArtifactError::CanonicalCoordinateMismatch)?
                .identity()
                != cache.covered_projection
            {
                return Err(ResponsesArtifactError::CanonicalCoordinateMismatch);
            }
        }

        let mut authorized = Vec::new();
        // Append limits constrain one response, not the already-owned history.
        // Authorization borrows payloads and still checks every covered item.
        for (position, item) in items.iter().enumerate() {
            let mut output_at_position = 0_usize;
            for envelope in item.blocks().iter().filter_map(canonical_replay) {
                let Some(purpose) = self.replay_contract.purpose_for_kind(envelope.kind()) else {
                    continue;
                };
                let compatible = envelope.is_compatible(
                    &self.binding.dialect,
                    self.binding.model_family.as_ref(),
                    &self.binding.model,
                    Some(self.binding.account),
                );
                // Compatible historical envelopes in the bound canonical
                // prefix may still be replayed by a latest stateless artifact.
                // Incompatible historical envelopes, however, belong to an
                // earlier model/provider coordinate: this aggregate artifact
                // cannot authorize or reject them. Request projection applies
                // their explicit portable/opaque fallback semantics.
                if position < producing && !compatible {
                    continue;
                }
                if position >= covered {
                    return Err(ResponsesArtifactError::UnauthorizedReplayTail { position });
                }
                let expected = self
                    .replay_contract
                    .for_purpose(purpose)
                    .filter(|kind| kind.matches(envelope))
                    .ok_or(ResponsesArtifactError::CanonicalCoordinateMismatch)?;
                let _ = expected;
                if !compatible
                    || envelope.classification() == DataClassification::Secret
                    || (purpose == ResponsesReplayPurpose::EncryptedReasoning
                        && envelope.classification() == DataClassification::Public)
                {
                    return Err(ResponsesArtifactError::CanonicalCoordinateMismatch);
                }
                if purpose.is_output() {
                    output_at_position = output_at_position.saturating_add(1);
                }
                authorized.push(ResponsesAuthorizedReplay::new(purpose, position, envelope));
            }
            let model_output =
                item.actor() == Actor::Assistant && item.provenance() == Provenance::ModelOutput;
            if output_at_position > 1
                || (output_at_position == 1 && !model_output)
                || (position >= producing && position < covered && output_at_position != 1)
            {
                return Err(ResponsesArtifactError::CanonicalCoordinateMismatch);
            }
        }
        Ok(authorized)
    }
}

fn canonical_replay(block: &ContentBlock) -> Option<&ReplayEnvelope> {
    match block {
        ContentBlock::Opaque(opaque) => Some(opaque.replay()),
        ContentBlock::Reasoning(reasoning) => reasoning.replay(),
        _ => None,
    }
}

impl fmt::Debug for ResponsesArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesArtifact")
            .field("binding", &self.binding)
            .field("production", &self.production)
            .field("replay_contract", &self.replay_contract)
            .field("replay_bytes", &self.replay_bytes)
            .field("continuation", &self.continuation.is_some())
            .field("cache", &self.cache.is_some())
            .field("storage", &self.storage)
            .field("expires_at", &self.expires_at)
            .field("integrity", &self.integrity)
            .field("encoded_len", &self.encoded_len)
            .finish_non_exhaustive()
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the identity covers the complete closed producer binding"
)]
fn artifact_binding_identity(
    dialect: &ProtocolId,
    route: &RouteId,
    account: Digest,
    model: &ModelId,
    family: Option<&ModelFamilyId>,
    adapter: &ModelAdapterProfileId,
    semantic_model: Digest,
    revisions: ResponsesSemanticRevisions,
    cache_affinity: Option<Digest>,
    stable_prefix: Digest,
    projection: Digest,
    item_prefix: Digest,
    tool_plan: Digest,
    request: Digest,
    model_input: Digest,
    validated_plan: Digest,
    run: RunId,
    root: ModelOperationRoot,
    attempt: AttemptId,
) -> Result<Digest, ResponsesArtifactError> {
    let family_revision = revisions.family_kernel.to_be_bytes();
    let dialect_revision = revisions.dialect.to_be_bytes();
    let adapter_revision = revisions.adapter.to_be_bytes();
    let cache_affinity = cache_affinity.map(|cache_affinity| cache_affinity.identity_bytes());
    let cache_affinity_present = [u8::from(cache_affinity.is_some())];
    let run = run.to_string();
    let root =
        serde_json::to_vec(&root).map_err(|_| ResponsesArtifactError::BindingConstruction)?;
    let attempt = attempt.to_string();
    Digest::sha256(
        DigestDomain::ModelBinding,
        4,
        &[
            BINDING_DOMAIN,
            ResponsesArtifactFamily::Responses.as_str().as_bytes(),
            dialect.as_str().as_bytes(),
            route.as_str().as_bytes(),
            account.identity_bytes().as_slice(),
            model.as_str().as_bytes(),
            family.map_or("", ModelFamilyId::as_str).as_bytes(),
            adapter.as_str().as_bytes(),
            semantic_model.identity_bytes().as_slice(),
            &family_revision,
            &dialect_revision,
            &adapter_revision,
            &cache_affinity_present,
            cache_affinity.as_deref().unwrap_or_default(),
            stable_prefix.identity_bytes().as_slice(),
            projection.identity_bytes().as_slice(),
            item_prefix.identity_bytes().as_slice(),
            tool_plan.identity_bytes().as_slice(),
            request.identity_bytes().as_slice(),
            model_input.identity_bytes().as_slice(),
            validated_plan.identity_bytes().as_slice(),
            run.as_bytes(),
            &root,
            attempt.as_bytes(),
        ],
    )
    .map_err(|_| ResponsesArtifactError::BindingConstruction)
}

struct EncodedBody {
    body: Vec<u8>,
    replay_bytes: usize,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may use focused expect/unwrap assertions"
)]
mod tests;
