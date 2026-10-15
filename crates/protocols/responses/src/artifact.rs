//! Bounded reconstructive native state for the Responses protocol family.
//!
//! The artifact records historical producer facts. Decoding it proves only
//! structural integrity; active continuation or replay additionally requires
//! a fresh exact-target and canonical-prefix validation. It is never a route,
//! credential, tool, history, checkpoint, or terminal authority.

use std::fmt;
use std::sync::Arc;

use plakous_model::{
    Actor, AttemptId, ContentBlock, DataClassification, Digest, DigestAlgorithm, DigestDomain,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelOperationRoot, ModelRequest, ProtocolId,
    Provenance, ReplayEnvelope, ReplayKindId, ResolvedModelDescriptor, RouteId, RunId,
    SessionWorkflowRootId, Timestamp, TurnId, ValidatedRequestPlan,
};
use thiserror::Error;

/// Current provisional family-artifact binary codec revision.
pub const RESPONSES_ARTIFACT_CODEC_REVISION: u32 = 6;
/// Current Responses artifact semantic schema revision.
pub const RESPONSES_ARTIFACT_SCHEMA_REVISION: u32 = 7;
/// Hard upper bound for one complete encoded Responses artifact.
pub const MAX_RESPONSES_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum canonical items inspected for active Responses replay authorization.
pub const MAX_RESPONSES_REPLAY_ITEMS: usize = 4_096;
/// Maximum aggregate canonical replay-envelope bytes inspected per authorization.
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

    fn same_reuse_target(&self, current: &Self) -> bool {
        self.dialect == current.dialect
            && self.route == current.route
            && self.account == current.account
            && self.model == current.model
            && self.model_family == current.model_family
            && self.adapter == current.adapter
            && self.semantic_model == current.semantic_model
            && self.revisions == current.revisions
            && self.cache_affinity == current.cache_affinity
            && self.stable_prefix == current.stable_prefix
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
        Self::from_parts(
            request.identity(),
            producing_projection,
            plan.digest(),
            physical_request,
            terminal_response,
            producing_projection_items,
            output_items,
            producing_projection_items
                .checked_add(output_items)
                .ok_or(ResponsesArtifactError::InvalidCoordinates)?,
            covered_projection,
        )
    }

    fn from_parts(
        producer_request: Digest,
        producing_projection: Digest,
        validated_plan: Digest,
        physical_request: Digest,
        terminal_response: Digest,
        producing_projection_items: u32,
        output_items: u32,
        covered_projection_items: u32,
        covered_projection: Digest,
    ) -> Result<Self, ResponsesArtifactError> {
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

/// Why an exact native replay item is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesReplayPurpose {
    /// An exact ordered provider-native output item.
    OutputItem,
    /// An exact provider-native output item unknown to the current projector.
    OpaqueOutputItem,
    /// Exact encrypted native reasoning required for lossless replay.
    EncryptedReasoning,
}

impl ResponsesReplayPurpose {
    const fn is_output(self) -> bool {
        matches!(self, Self::OutputItem | Self::OpaqueOutputItem)
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

/// One canonical item-level replay envelope authorized by an active v4 artifact.
///
/// The envelope remains owned by canonical history. This value carries only its
/// closed purpose and absolute coordinate, so the family artifact never stores
/// a second copy of the raw provider payload.
#[derive(Clone, Copy)]
pub struct ResponsesAuthorizedReplay<'a> {
    purpose: ResponsesReplayPurpose,
    canonical_position: usize,
    envelope: &'a ReplayEnvelope,
}

impl<'a> ResponsesAuthorizedReplay<'a> {
    /// Returns the purpose selected by the artifact's exact replay contract.
    #[must_use]
    pub const fn purpose(self) -> ResponsesReplayPurpose {
        self.purpose
    }

    /// Returns the absolute coordinate in the current canonical projection.
    #[must_use]
    pub const fn canonical_position(self) -> usize {
        self.canonical_position
    }

    /// Borrows the canonical item-level envelope without copying its payload.
    #[must_use]
    pub const fn envelope(self) -> &'a ReplayEnvelope {
        self.envelope
    }
}

impl fmt::Debug for ResponsesAuthorizedReplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesAuthorizedReplay")
            .field("purpose", &self.purpose)
            .field("canonical_position", &self.canonical_position)
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

/// The sole active reuse path selected after complete validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesArtifactReuseKind {
    /// Use the exact provider-stored continuation and suppress local output replay.
    ProviderContinuation,
    /// Replay exact local output items and emit no provider continuation.
    LocalReplay,
    /// Only a provider cache fact remains applicable.
    CacheOnly,
}

/// Active, time-checked reuse authority borrowed from one inert artifact.
///
/// This wrapper is the narrow family-owned seam a neutral runtime may retain
/// transiently. It exposes continuation and local replay mutually exclusively.
pub struct ResponsesArtifactReuse<'a> {
    artifact: &'a ResponsesArtifact,
    kind: ResponsesArtifactReuseKind,
    replay: Vec<ResponsesAuthorizedReplay<'a>>,
}

impl ResponsesArtifactReuse<'_> {
    /// Returns the one admitted reuse path.
    #[must_use]
    pub const fn kind(&self) -> ResponsesArtifactReuseKind {
        self.kind
    }

    /// Returns a continuation only for the provider-stored reuse path.
    #[must_use]
    pub fn continuation(&self) -> Option<&ResponsesContinuation> {
        matches!(self.kind, ResponsesArtifactReuseKind::ProviderContinuation)
            .then(|| self.artifact.continuation.as_ref())
            .flatten()
    }

    /// Returns local replay only for the stateless reuse path.
    #[must_use]
    pub fn replay(&self) -> &[ResponsesAuthorizedReplay<'_>] {
        if matches!(self.kind, ResponsesArtifactReuseKind::LocalReplay) {
            &self.replay
        } else {
            &[]
        }
    }

    /// Looks up replay by full purpose and absolute canonical coordinate.
    ///
    /// Envelope digest is deliberately not a uniqueness key: byte-identical
    /// envelopes may occur at distinct exact coordinates or response groups.
    #[must_use]
    pub fn replay_at(
        &self,
        purpose: ResponsesReplayPurpose,
        canonical_position: usize,
    ) -> Option<ResponsesAuthorizedReplay<'_>> {
        if !matches!(self.kind, ResponsesArtifactReuseKind::LocalReplay) {
            return None;
        }
        self.replay
            .iter()
            .copied()
            .find(|item| item.purpose == purpose && item.canonical_position == canonical_position)
    }

    /// Returns the exact cache fact after its canonical coverage was checked.
    #[must_use]
    pub const fn cache(&self) -> Option<&ResponsesCacheFact> {
        self.artifact.cache.as_ref()
    }
}

impl fmt::Debug for ResponsesArtifactReuse<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesArtifactReuse")
            .field("kind", &self.kind)
            .field("binding", &self.artifact.binding.identity)
            .finish()
    }
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
        if !current.names_request(request) || !self.binding.same_reuse_target(current) {
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
        Ok(ResponsesArtifactReuse {
            artifact: self,
            kind,
            replay,
        })
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

    #[allow(
        clippy::too_many_lines,
        reason = "one pass keeps coordinate, replay-byte, compatibility, and output-cardinality validation causal"
    )]
    fn validate_canonical_coverage<'a>(
        &self,
        request: &'a ModelRequest,
    ) -> Result<Vec<ResponsesAuthorizedReplay<'a>>, ResponsesArtifactError> {
        let producing = usize::try_from(self.production.producing_projection_items)
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        let covered = usize::try_from(self.production.covered_projection_items)
            .map_err(|_| ResponsesArtifactError::InvalidCoordinates)?;
        let items = request.input().items();
        if items.len() > MAX_RESPONSES_REPLAY_ITEMS {
            return Err(ResponsesArtifactError::TooManyReplayItems {
                maximum: MAX_RESPONSES_REPLAY_ITEMS,
            });
        }
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
        let mut replay_bytes = 0_usize;
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
                replay_bytes = replay_bytes
                    .checked_add(envelope.payload().get().len())
                    .ok_or(ResponsesArtifactError::ReplayBytesTooLarge {
                        maximum: MAX_RESPONSES_REPLAY_BYTES,
                    })?;
                if replay_bytes > MAX_RESPONSES_REPLAY_BYTES {
                    return Err(ResponsesArtifactError::ReplayBytesTooLarge {
                        maximum: MAX_RESPONSES_REPLAY_BYTES,
                    });
                }
                if purpose.is_output() {
                    output_at_position = output_at_position.saturating_add(1);
                }
                authorized.push(ResponsesAuthorizedReplay {
                    purpose,
                    canonical_position: position,
                    envelope,
                });
                if authorized.len() > MAX_RESPONSES_REPLAY_ITEMS {
                    return Err(ResponsesArtifactError::TooManyReplayItems {
                        maximum: MAX_RESPONSES_REPLAY_ITEMS,
                    });
                }
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

/// Closed family-local codec for canonical Responses artifact bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResponsesArtifactCodec;

impl ResponsesArtifactCodec {
    /// Encodes one artifact without reserializing or rehashing its immutable body.
    pub fn encode(artifact: &ResponsesArtifact) -> Result<Vec<u8>, ResponsesArtifactError> {
        if artifact.encoded_len != envelope_len(artifact.canonical_body.len())? {
            return Err(ResponsesArtifactError::IntegrityMismatch);
        }
        let body_len = u32::try_from(artifact.canonical_body.len()).map_err(|_| {
            ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES,
            }
        })?;
        let mut bytes = Vec::with_capacity(artifact.encoded_len);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&RESPONSES_ARTIFACT_CODEC_REVISION.to_be_bytes());
        bytes.extend_from_slice(&body_len.to_be_bytes());
        bytes.extend_from_slice(&artifact.canonical_body);
        bytes.extend_from_slice(&artifact.integrity);
        Ok(bytes)
    }

    /// Decodes structurally and exactly revalidates the original producer binding.
    pub fn decode(
        bytes: &[u8],
        expected: &ResponsesArtifactBinding,
        expected_production: &ResponsesArtifactProduction,
        now: Timestamp,
    ) -> Result<ResponsesArtifact, ResponsesArtifactError> {
        let artifact = Self::decode_inert(bytes)?;
        artifact.validate_for(expected, expected_production, now)?;
        Ok(artifact)
    }

    /// Decodes and integrity-checks inert reconstructive artifact bytes.
    ///
    /// Expiry is deliberately not consulted. The returned historical value
    /// grants no replay right until [`ResponsesArtifact::validate_for_reuse`].
    pub fn decode_inert(bytes: &[u8]) -> Result<ResponsesArtifact, ResponsesArtifactError> {
        if bytes.len() > MAX_RESPONSES_ARTIFACT_BYTES {
            return Err(ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES,
            });
        }
        let mut reader = Reader::new(bytes);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(ResponsesArtifactError::InvalidMagic);
        }
        let codec_revision = reader.u32()?;
        if codec_revision != RESPONSES_ARTIFACT_CODEC_REVISION {
            return Err(ResponsesArtifactError::UnsupportedCodecRevision {
                found: codec_revision,
            });
        }
        let body_len = reader.length(MAX_BODY_BYTES)?;
        let body = reader.take(body_len)?;
        let recorded_integrity = reader.array_32()?;
        reader.finish()?;
        if integrity(body)? != recorded_integrity {
            return Err(ResponsesArtifactError::IntegrityMismatch);
        }
        decode_body(body, recorded_integrity)
    }
}

fn decode_body(
    body: &[u8],
    recorded_integrity: [u8; 32],
) -> Result<ResponsesArtifact, ResponsesArtifactError> {
    let mut reader = Reader::new(body);
    let schema_revision = reader.u32()?;
    if schema_revision != RESPONSES_ARTIFACT_SCHEMA_REVISION {
        return Err(ResponsesArtifactError::UnsupportedSchemaRevision {
            found: schema_revision,
        });
    }
    let family = reader.u8()?;
    if family != 1 {
        return Err(ResponsesArtifactError::UnknownTag {
            field: "family",
            tag: family,
        });
    }
    let binding = decode_binding(&mut reader)?;
    let production = decode_production(&mut reader)?;
    let replay_contract = decode_replay_contract(&mut reader)?;
    let continuation = decode_continuation(&mut reader)?;
    let cache = decode_cache(&mut reader)?;
    let storage = decode_storage(&mut reader)?;
    let expires_at = decode_expiry(&mut reader)?;
    reader.finish()?;

    let artifact = ResponsesArtifact {
        binding,
        production,
        replay_contract,
        continuation,
        cache,
        storage,
        expires_at,
        integrity: recorded_integrity,
        encoded_len: envelope_len(body.len())?,
        replay_bytes: 0,
        canonical_body: Arc::from(body),
    };
    artifact.validate_structure()?;
    Ok(artifact)
}

fn decode_binding(
    reader: &mut Reader<'_>,
) -> Result<ResponsesArtifactBinding, ResponsesArtifactError> {
    let dialect = ProtocolId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "dialect" })?;
    let route = RouteId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "route" })?;
    let account = reader.digest(DigestDomain::ModelBinding)?;
    let model = ModelId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "model" })?;
    let model_family = reader
        .optional_string(MAX_ID_BYTES, "model family")?
        .map(ModelFamilyId::new)
        .transpose()
        .map_err(|_| ResponsesArtifactError::InvalidIdentity {
            field: "model family",
        })?;
    let adapter = ModelAdapterProfileId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "adapter" })?;
    let semantic_model = reader.digest(DigestDomain::ModelBinding)?;
    let revisions = ResponsesSemanticRevisions::new(reader.u32()?, reader.u32()?, reader.u32()?)?;
    let cache_affinity = reader
        .optional("cache affinity")?
        .then(|| reader.digest(DigestDomain::CacheAffinity))
        .transpose()?;
    let stable_prefix = reader.digest(DigestDomain::StableRequestPrefix)?;
    let projection = reader.digest(DigestDomain::HistoryProjection)?;
    let item_prefix = reader.digest(DigestDomain::CanonicalItemPrefix)?;
    let tool_plan = reader.digest(DigestDomain::ResolvedHarnessPlan)?;
    let request = reader.digest(DigestDomain::RequestPlan)?;
    let model_input = reader.digest(DigestDomain::RequestPlan)?;
    let validated_plan = reader.digest(DigestDomain::RequestPlan)?;
    let run = reader.uuid("run")?;
    let root = decode_operation_root(reader)?;
    let attempt = reader.uuid("attempt")?;
    ResponsesArtifactBinding::from_parts(
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
    )
}

fn encode_operation_root(
    writer: &mut Writer,
    root: ModelOperationRoot,
) -> Result<(), ResponsesArtifactError> {
    match root {
        ModelOperationRoot::Turn(turn_id) => {
            writer.u8(1)?;
            writer.string(&turn_id.to_string(), UUID_TEXT_BYTES)
        }
        ModelOperationRoot::SessionWorkflow(workflow_id) => {
            writer.u8(2)?;
            writer.extend(workflow_id.as_bytes())
        }
    }
}

fn decode_operation_root(
    reader: &mut Reader<'_>,
) -> Result<ModelOperationRoot, ResponsesArtifactError> {
    match reader.u8()? {
        1 => reader.uuid("turn root").map(ModelOperationRoot::Turn),
        2 => SessionWorkflowRootId::new(reader.array_32()?)
            .map(ModelOperationRoot::SessionWorkflow)
            .map_err(|_| ResponsesArtifactError::InvalidIdentity {
                field: "session workflow root",
            }),
        tag => Err(ResponsesArtifactError::UnknownTag {
            field: "operation root",
            tag,
        }),
    }
}

fn decode_production(
    reader: &mut Reader<'_>,
) -> Result<ResponsesArtifactProduction, ResponsesArtifactError> {
    ResponsesArtifactProduction::from_parts(
        reader.digest(DigestDomain::RequestPlan)?,
        reader.digest(DigestDomain::CanonicalItemPrefix)?,
        reader.digest(DigestDomain::RequestPlan)?,
        reader.digest(DigestDomain::ProviderOperation)?,
        reader.digest(DigestDomain::ProviderOperation)?,
        reader.u32()?,
        reader.u32()?,
        reader.u32()?,
        reader.digest(DigestDomain::CanonicalItemPrefix)?,
    )
}

fn decode_replay_kind(
    reader: &mut Reader<'_>,
) -> Result<ResponsesArtifactReplayKind, ResponsesArtifactError> {
    let kind = ReplayKindId::new(reader.string(MAX_ID_BYTES)?).map_err(|_| {
        ResponsesArtifactError::InvalidIdentity {
            field: "replay kind",
        }
    })?;
    ResponsesArtifactReplayKind::new(kind, reader.u32()?)
}

fn decode_replay_contract(
    reader: &mut Reader<'_>,
) -> Result<ResponsesReplayContract, ResponsesArtifactError> {
    let mut contract = ResponsesReplayContract::new(decode_replay_kind(reader)?);
    if reader.optional("opaque output replay kind")? {
        contract = contract.with_opaque_output(decode_replay_kind(reader)?)?;
    }
    if reader.optional("encrypted reasoning replay kind")? {
        contract = contract.with_encrypted_reasoning(decode_replay_kind(reader)?)?;
    }
    Ok(contract)
}

fn decode_continuation(
    reader: &mut Reader<'_>,
) -> Result<Option<ResponsesContinuation>, ResponsesArtifactError> {
    if reader.optional("continuation")? {
        ResponsesContinuation::new(
            reader.string(MAX_OPAQUE_ID_BYTES)?,
            reader.u32()?,
            reader.digest(DigestDomain::CanonicalItemPrefix)?,
            reader.digest(DigestDomain::ProviderOperation)?,
        )
        .map(Some)
    } else {
        Ok(None)
    }
}

fn decode_cache(
    reader: &mut Reader<'_>,
) -> Result<Option<ResponsesCacheFact>, ResponsesArtifactError> {
    if reader.optional("cache")? {
        ResponsesCacheFact::new(
            reader.string(MAX_OPAQUE_ID_BYTES)?,
            reader.digest(DigestDomain::StableRequestPrefix)?,
            reader.u32()?,
            reader.digest(DigestDomain::CanonicalItemPrefix)?,
        )
        .map(Some)
    } else {
        Ok(None)
    }
}

fn decode_storage(reader: &mut Reader<'_>) -> Result<ResponsesStorageMode, ResponsesArtifactError> {
    match reader.u8()? {
        1 => Ok(ResponsesStorageMode::Stateless),
        2 => Ok(ResponsesStorageMode::ProviderStored),
        tag => Err(ResponsesArtifactError::UnknownTag {
            field: "storage mode",
            tag,
        }),
    }
}

fn decode_expiry(reader: &mut Reader<'_>) -> Result<Option<Timestamp>, ResponsesArtifactError> {
    if reader.optional("expiry")? {
        Timestamp::new(reader.u64()?, reader.u32()?)
            .map(Some)
            .map_err(|_| ResponsesArtifactError::InvalidTimestamp)
    } else {
        Ok(None)
    }
}

/// Fail-closed artifact construction, encoding, or validation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesArtifactError {
    /// A behavior-bearing semantic revision used reserved version zero.
    #[error("Responses artifact {field} revision must be nonzero")]
    ZeroRevision {
        /// Revision field that was zero.
        field: &'static str,
    },
    /// The supplied model/request plan was not one exact admitted operation.
    #[error("Responses artifact operation was not independently admitted")]
    AdmissionMismatch,
    /// The admitted request lacked its core-resolved harness-plan identity.
    #[error("Responses artifact request lacks a resolved harness-plan identity")]
    MissingHarnessPlanIdentity,
    /// A digest carried the wrong algorithm, domain, or version.
    #[error("Responses artifact has an invalid {field} digest")]
    InvalidDigest {
        /// Digest field that failed validation.
        field: &'static str,
    },
    /// A reconstructed target identity was invalid.
    #[error("Responses artifact has an invalid {field} identity")]
    InvalidIdentity {
        /// Identity field that failed validation.
        field: &'static str,
    },
    /// The complete binding identity could not be derived.
    #[error("Responses artifact binding identity construction failed")]
    BindingConstruction,
    /// An opaque provider identity was empty or contained control characters.
    #[error("Responses artifact {field} is invalid")]
    InvalidOpaqueId {
        /// Opaque identity field that failed validation.
        field: &'static str,
    },
    /// A bounded opaque provider identity was too large.
    #[error("Responses artifact {field} exceeds {maximum} bytes")]
    OpaqueIdTooLong {
        /// Opaque identity field that exceeded its bound.
        field: &'static str,
        /// Maximum accepted byte count.
        maximum: usize,
    },
    /// A continuation claimed to cover no canonical projection items.
    #[error("Responses continuation must cover at least one projection item")]
    ZeroProjectionItems,
    /// Physical or canonical coordinates were arithmetically inconsistent.
    #[error("Responses artifact production coordinates are invalid")]
    InvalidCoordinates,
    /// Physical production facts were transplanted from another admitted request.
    #[error("Responses artifact production does not match its admitted binding")]
    ProductionBindingMismatch,
    /// No native fact justified retaining an artifact.
    #[error("Responses artifact contains no replay, continuation, or cache fact")]
    EmptyArtifact,
    /// The ordered replay list exceeded its item bound.
    #[error("Responses artifact exceeds {maximum} replay items")]
    TooManyReplayItems {
        /// Maximum retained replay items.
        maximum: usize,
    },
    /// Aggregate replay bytes exceeded the hard artifact replay budget.
    #[error("Responses artifact exceeds {maximum} aggregate replay bytes")]
    ReplayBytesTooLarge {
        /// Maximum aggregate canonical replay bytes.
        maximum: usize,
    },
    /// One replay item exceeded its byte bound.
    #[error("Responses artifact replay item {index} exceeds {maximum} bytes")]
    ReplayItemTooLarge {
        /// Ordered replay index.
        index: usize,
        /// Maximum canonical replay item bytes.
        maximum: usize,
    },
    /// Replay contract assigned one kind to multiple purposes.
    #[error("Responses replay contract assigns one kind to multiple purposes")]
    AmbiguousReplayContract,
    /// Replay purpose and exact dialect kind/version disagreed.
    #[error("Responses artifact replay item {index} has a mislabeled purpose or kind")]
    ReplayPurposeKindMismatch {
        /// Ordered replay index.
        index: usize,
    },
    /// A replay envelope was scoped to another dialect/model binding.
    #[error("Responses artifact replay item {index} has a foreign binding")]
    ReplayBindingMismatch {
        /// Ordered replay index.
        index: usize,
    },
    /// One replay item named an invalid response-relative coordinate.
    #[error("Responses artifact replay item {index} has an invalid coordinate")]
    InvalidReplayCoordinate {
        /// Ordered replay index.
        index: usize,
    },
    /// Two same-purpose replay facts claimed one exact response coordinate.
    #[error("Responses artifact replay item {index} duplicates an exact coordinate")]
    DuplicateReplayCoordinate {
        /// Ordered replay index.
        index: usize,
    },
    /// Local replay did not cover every native output item exactly once.
    #[error("Responses artifact replay does not completely cover native output")]
    IncompleteReplayCoverage,
    /// Encrypted native reasoning was incorrectly marked display-public.
    #[error("Responses encrypted reasoning item {index} must be sensitive or secret")]
    PublicEncryptedReasoning {
        /// Ordered replay index.
        index: usize,
    },
    /// Raw secret-classified replay material cannot enter the family artifact.
    #[error("Responses artifact replay item {index} contains raw secret material")]
    SecretReplayMaterial {
        /// Ordered replay index.
        index: usize,
    },
    /// A continuation existed without provider-side response storage.
    #[error("Responses continuation requires provider-stored response mode")]
    ContinuationWithoutStorage,
    /// Provider-stored output lacked its sole continuation coverage fact.
    #[error("Responses provider-stored output lacks a continuation")]
    MissingStoredContinuation,
    /// Continuation did not cover the exact terminal response and canonical prefix.
    #[error("Responses continuation coverage does not match artifact production")]
    ContinuationCoverageMismatch,
    /// A cache fact did not match the exact stable-prefix binding.
    #[error("Responses cache fact does not match the stable-prefix binding")]
    CachePrefixMismatch,
    /// A cache fact named neither an exact producing nor covered canonical prefix.
    #[error("Responses cache fact has invalid canonical coverage")]
    CacheCoverageMismatch,
    /// The artifact crossed its aggregate expiry boundary.
    #[error("Responses artifact is expired")]
    Expired,
    /// The caller expected a different exact target, request, or producer binding.
    #[error("Responses artifact binding mismatch")]
    BindingMismatch,
    /// Replayed native material was absent from its exact canonical coordinate.
    #[error("Responses artifact canonical replay coordinate mismatch")]
    CanonicalCoordinateMismatch,
    /// Native Responses replay appeared after the artifact's exact covered prefix.
    #[error("Responses artifact found unauthorized native replay at canonical position {position}")]
    UnauthorizedReplayTail {
        /// Absolute canonical position outside artifact coverage.
        position: usize,
    },
    /// The complete encoded artifact exceeded its hard bound.
    #[error("Responses artifact exceeds {maximum} bytes")]
    ArtifactTooLarge {
        /// Maximum accepted encoded artifact bytes.
        maximum: usize,
    },
    /// The family codec magic was absent.
    #[error("invalid Responses artifact magic")]
    InvalidMagic,
    /// The binary codec revision is unavailable.
    #[error("unsupported Responses artifact codec revision {found}")]
    UnsupportedCodecRevision {
        /// Encoded codec revision.
        found: u32,
    },
    /// The artifact semantic schema is unavailable.
    #[error("unsupported Responses artifact schema revision {found}")]
    UnsupportedSchemaRevision {
        /// Encoded semantic schema revision.
        found: u32,
    },
    /// A closed artifact discriminant was unavailable.
    #[error("unknown Responses artifact {field} tag {tag}")]
    UnknownTag {
        /// Closed field whose tag was unknown.
        field: &'static str,
        /// Encoded tag.
        tag: u8,
    },
    /// Bytes were truncated or carried an impossible length.
    #[error("malformed Responses artifact encoding")]
    Malformed,
    /// Bytes remained after the sole canonical value.
    #[error("Responses artifact encoding has trailing bytes")]
    TrailingBytes,
    /// The outer integrity witness disagreed with the complete body.
    #[error("Responses artifact integrity mismatch")]
    IntegrityMismatch,
    /// A versioned integrity witness could not be constructed.
    #[error("Responses artifact integrity construction failed")]
    IntegrityConstruction,
    /// A replay envelope could not be decoded and integrity-validated.
    #[error("invalid Responses replay encoding")]
    InvalidReplayEncoding,
    /// Replay bytes had a second structural representation.
    #[error("non-canonical Responses replay encoding")]
    NonCanonicalReplay,
    /// An encoded timestamp was structurally invalid.
    #[error("invalid Responses artifact timestamp")]
    InvalidTimestamp,
}

fn validate_digest(
    digest: Digest,
    domain: DigestDomain,
    field: &'static str,
) -> Result<(), ResponsesArtifactError> {
    if digest.algorithm() != DigestAlgorithm::Sha256
        || digest.domain() != domain
        || digest.version() == 0
    {
        return Err(ResponsesArtifactError::InvalidDigest { field });
    }
    Ok(())
}

fn validate_opaque_id(value: &str, field: &'static str) -> Result<(), ResponsesArtifactError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ResponsesArtifactError::InvalidOpaqueId { field });
    }
    if value.len() > MAX_OPAQUE_ID_BYTES {
        return Err(ResponsesArtifactError::OpaqueIdTooLong {
            field,
            maximum: MAX_OPAQUE_ID_BYTES,
        });
    }
    Ok(())
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

#[allow(
    clippy::too_many_lines,
    reason = "the canonical codec intentionally writes one closed schema in field order"
)]
fn encode_body(artifact: &ResponsesArtifact) -> Result<EncodedBody, ResponsesArtifactError> {
    let mut out = Writer::new();
    out.u32(RESPONSES_ARTIFACT_SCHEMA_REVISION)?;
    out.u8(1)?;
    out.string(artifact.binding.dialect.as_str(), MAX_ID_BYTES)?;
    out.string(artifact.binding.route.as_str(), MAX_ID_BYTES)?;
    out.digest(
        artifact.binding.account,
        DigestDomain::ModelBinding,
        "account",
    )?;
    out.string(artifact.binding.model.as_str(), MAX_ID_BYTES)?;
    out.optional_string(
        artifact
            .binding
            .model_family
            .as_ref()
            .map(ModelFamilyId::as_str),
        MAX_ID_BYTES,
    )?;
    out.string(artifact.binding.adapter.as_str(), MAX_ID_BYTES)?;
    out.digest(
        artifact.binding.semantic_model,
        DigestDomain::ModelBinding,
        "resolved model",
    )?;
    for revision in [
        artifact.binding.revisions.family_kernel,
        artifact.binding.revisions.dialect,
        artifact.binding.revisions.adapter,
    ] {
        out.u32(revision)?;
    }
    out.optional(artifact.binding.cache_affinity.is_some())?;
    if let Some(cache_affinity) = artifact.binding.cache_affinity {
        out.digest(
            cache_affinity,
            DigestDomain::CacheAffinity,
            "cache affinity",
        )?;
    }
    out.digest(
        artifact.binding.stable_prefix,
        DigestDomain::StableRequestPrefix,
        "stable prefix",
    )?;
    out.digest(
        artifact.binding.projection,
        DigestDomain::HistoryProjection,
        "projection",
    )?;
    out.digest(
        artifact.binding.item_prefix,
        DigestDomain::CanonicalItemPrefix,
        "canonical item prefix",
    )?;
    out.digest(
        artifact.binding.tool_plan,
        DigestDomain::ResolvedHarnessPlan,
        "tool plan",
    )?;
    out.digest(
        artifact.binding.request,
        DigestDomain::RequestPlan,
        "request",
    )?;
    out.digest(
        artifact.binding.model_input,
        DigestDomain::RequestPlan,
        "model input",
    )?;
    out.digest(
        artifact.binding.validated_plan,
        DigestDomain::RequestPlan,
        "validated plan",
    )?;
    out.string(&artifact.binding.run.to_string(), UUID_TEXT_BYTES)?;
    encode_operation_root(&mut out, artifact.binding.root)?;
    out.string(&artifact.binding.attempt.to_string(), UUID_TEXT_BYTES)?;

    out.digest(
        artifact.production.producer_request,
        DigestDomain::RequestPlan,
        "producer request",
    )?;
    out.digest(
        artifact.production.producing_projection,
        DigestDomain::CanonicalItemPrefix,
        "producing item prefix",
    )?;
    out.digest(
        artifact.production.validated_plan,
        DigestDomain::RequestPlan,
        "validated plan",
    )?;
    out.digest(
        artifact.production.physical_request,
        DigestDomain::ProviderOperation,
        "physical request",
    )?;
    out.digest(
        artifact.production.terminal_response,
        DigestDomain::ProviderOperation,
        "terminal response",
    )?;
    out.u32(artifact.production.producing_projection_items)?;
    out.u32(artifact.production.output_items)?;
    out.u32(artifact.production.covered_projection_items)?;
    out.digest(
        artifact.production.covered_projection,
        DigestDomain::CanonicalItemPrefix,
        "covered item prefix",
    )?;

    encode_replay_kind(&mut out, &artifact.replay_contract.output)?;
    out.optional(artifact.replay_contract.opaque_output.is_some())?;
    if let Some(kind) = &artifact.replay_contract.opaque_output {
        encode_replay_kind(&mut out, kind)?;
    }
    out.optional(artifact.replay_contract.encrypted_reasoning.is_some())?;
    if let Some(kind) = &artifact.replay_contract.encrypted_reasoning {
        encode_replay_kind(&mut out, kind)?;
    }

    out.optional(artifact.continuation.is_some())?;
    if let Some(continuation) = &artifact.continuation {
        out.string(&continuation.response_id, MAX_OPAQUE_ID_BYTES)?;
        out.u32(continuation.covered_projection_items)?;
        out.digest(
            continuation.covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "continuation covered item prefix",
        )?;
        out.digest(
            continuation.terminal_response,
            DigestDomain::ProviderOperation,
            "continuation terminal response",
        )?;
    }
    out.optional(artifact.cache.is_some())?;
    if let Some(cache) = &artifact.cache {
        out.string(&cache.key, MAX_OPAQUE_ID_BYTES)?;
        out.digest(
            cache.stable_prefix,
            DigestDomain::StableRequestPrefix,
            "cache stable prefix",
        )?;
        out.u32(cache.covered_projection_items)?;
        out.digest(
            cache.covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "cache covered item prefix",
        )?;
    }
    out.u8(match artifact.storage {
        ResponsesStorageMode::Stateless => 1,
        ResponsesStorageMode::ProviderStored => 2,
    })?;
    out.optional(artifact.expires_at.is_some())?;
    if let Some(expiry) = artifact.expires_at {
        out.u64(expiry.seconds())?;
        out.u32(expiry.nanoseconds())?;
    }
    Ok(EncodedBody {
        body: out.finish(),
        replay_bytes: 0,
    })
}

fn encode_replay_kind(
    out: &mut Writer,
    kind: &ResponsesArtifactReplayKind,
) -> Result<(), ResponsesArtifactError> {
    out.string(kind.kind.as_str(), MAX_ID_BYTES)?;
    out.u32(kind.version)
}

fn integrity(body: &[u8]) -> Result<[u8; 32], ResponsesArtifactError> {
    let codec = RESPONSES_ARTIFACT_CODEC_REVISION.to_be_bytes();
    let schema = RESPONSES_ARTIFACT_SCHEMA_REVISION.to_be_bytes();
    let body_len = u64::try_from(body.len())
        .map_err(|_| ResponsesArtifactError::IntegrityConstruction)?
        .to_be_bytes();
    Digest::sha256(
        DigestDomain::Blob,
        RESPONSES_ARTIFACT_CODEC_REVISION,
        &[INTEGRITY_DOMAIN, &codec, &schema, &body_len, body],
    )
    .map(|digest| *digest.bytes())
    .map_err(|_| ResponsesArtifactError::IntegrityConstruction)
}

fn envelope_len(body_len: usize) -> Result<usize, ResponsesArtifactError> {
    let length = ENVELOPE_OVERHEAD.checked_add(body_len).ok_or(
        ResponsesArtifactError::ArtifactTooLarge {
            maximum: MAX_RESPONSES_ARTIFACT_BYTES,
        },
    )?;
    if length > MAX_RESPONSES_ARTIFACT_BYTES {
        return Err(ResponsesArtifactError::ArtifactTooLarge {
            maximum: MAX_RESPONSES_ARTIFACT_BYTES,
        });
    }
    Ok(length)
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(2_048),
        }
    }

    fn reserve(&self, additional: usize) -> Result<(), ResponsesArtifactError> {
        if self
            .bytes
            .len()
            .checked_add(additional)
            .is_none_or(|length| length > MAX_BODY_BYTES)
        {
            return Err(ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES,
            });
        }
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), ResponsesArtifactError> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), ResponsesArtifactError> {
        self.reserve(1)?;
        self.bytes.push(value);
        Ok(())
    }

    fn u32(&mut self, value: u32) -> Result<(), ResponsesArtifactError> {
        self.extend(&value.to_be_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), ResponsesArtifactError> {
        self.extend(&value.to_be_bytes())
    }

    fn length(&mut self, length: usize) -> Result<(), ResponsesArtifactError> {
        let length = u32::try_from(length).map_err(|_| ResponsesArtifactError::Malformed)?;
        self.u32(length)
    }

    fn bytes(&mut self, value: &[u8], maximum: usize) -> Result<(), ResponsesArtifactError> {
        if value.len() > maximum {
            return Err(ResponsesArtifactError::Malformed);
        }
        self.length(value.len())?;
        self.extend(value)
    }

    fn string(&mut self, value: &str, maximum: usize) -> Result<(), ResponsesArtifactError> {
        self.bytes(value.as_bytes(), maximum)
    }

    fn optional(&mut self, present: bool) -> Result<(), ResponsesArtifactError> {
        self.u8(u8::from(present))
    }

    fn optional_string(
        &mut self,
        value: Option<&str>,
        maximum: usize,
    ) -> Result<(), ResponsesArtifactError> {
        self.optional(value.is_some())?;
        if let Some(value) = value {
            self.string(value, maximum)?;
        }
        Ok(())
    }

    fn digest(
        &mut self,
        digest: Digest,
        domain: DigestDomain,
        field: &'static str,
    ) -> Result<(), ResponsesArtifactError> {
        validate_digest(digest, domain, field)?;
        self.u32(digest.version())?;
        self.extend(digest.bytes())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ResponsesArtifactError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(ResponsesArtifactError::Malformed)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(ResponsesArtifactError::Malformed)?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ResponsesArtifactError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(ResponsesArtifactError::Malformed)
    }

    fn u32(&mut self) -> Result<u32, ResponsesArtifactError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ResponsesArtifactError::Malformed)?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, ResponsesArtifactError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ResponsesArtifactError::Malformed)?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn array_32(&mut self) -> Result<[u8; 32], ResponsesArtifactError> {
        self.take(32)?
            .try_into()
            .map_err(|_| ResponsesArtifactError::Malformed)
    }

    fn length(&mut self, maximum: usize) -> Result<usize, ResponsesArtifactError> {
        let length = usize::try_from(self.u32()?).map_err(|_| ResponsesArtifactError::Malformed)?;
        if length > maximum {
            return Err(ResponsesArtifactError::Malformed);
        }
        Ok(length)
    }

    fn bytes(&mut self, maximum: usize) -> Result<&'a [u8], ResponsesArtifactError> {
        let length = self.length(maximum)?;
        self.take(length)
    }

    fn string(&mut self, maximum: usize) -> Result<String, ResponsesArtifactError> {
        let bytes = self.bytes(maximum)?;
        let value = std::str::from_utf8(bytes).map_err(|_| ResponsesArtifactError::Malformed)?;
        Ok(value.to_owned())
    }

    fn optional(&mut self, field: &'static str) -> Result<bool, ResponsesArtifactError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(ResponsesArtifactError::UnknownTag { field, tag }),
        }
    }

    fn optional_string(
        &mut self,
        maximum: usize,
        field: &'static str,
    ) -> Result<Option<String>, ResponsesArtifactError> {
        if self.optional(field)? {
            self.string(maximum).map(Some)
        } else {
            Ok(None)
        }
    }

    fn digest(&mut self, domain: DigestDomain) -> Result<Digest, ResponsesArtifactError> {
        let version = self.u32()?;
        let bytes = self.array_32()?;
        Digest::from_sha256_bytes(domain, version, bytes)
            .map_err(|_| ResponsesArtifactError::InvalidDigest { field: "encoded" })
    }

    fn uuid<T>(&mut self, field: &'static str) -> Result<T, ResponsesArtifactError>
    where
        T: std::str::FromStr + fmt::Display,
    {
        let encoded = self.string(UUID_TEXT_BYTES)?;
        let value = encoded
            .parse::<T>()
            .map_err(|_| ResponsesArtifactError::InvalidIdentity { field })?;
        if value.to_string() != encoded {
            return Err(ResponsesArtifactError::InvalidIdentity { field });
        }
        Ok(value)
    }

    fn finish(self) -> Result<(), ResponsesArtifactError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(ResponsesArtifactError::TrailingBytes)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may use focused expect/unwrap assertions"
)]
mod tests {
    use std::collections::BTreeMap;

    use plakous_model::{
        Actor, Capability, CapabilityConstraints, CapabilitySet, ConversationItem,
        ConversationView, EffectiveProviderConfigurationId, EntryId, FactProvenance, FactSource,
        FeatureSupport, OpaqueBlock, OpaqueFallback, Provenance, RequestMetadata,
        SelectedProviderConfigurationId, Support, TextBlock, ValidationMode,
    };
    use serde_json::value::RawValue;

    use super::*;

    const SECRET: &str = "encrypted-native-secret-sentinel";

    struct Fixture {
        descriptor: ResolvedModelDescriptor,
        producer_request: ModelRequest,
        producer_plan: ValidatedRequestPlan,
        binding: ResponsesArtifactBinding,
        production: ResponsesArtifactProduction,
        contract: ResponsesReplayContract,
        envelope: ReplayEnvelope,
        artifact: ResponsesArtifact,
        next_request: ModelRequest,
        current_binding: ResponsesArtifactBinding,
    }

    fn digest(domain: DigestDomain, label: &[u8]) -> Digest {
        Digest::sha256(domain, 1, &[label]).expect("valid digest version")
    }

    fn timestamp(seconds: u64) -> Timestamp {
        Timestamp::new(seconds, 0).expect("timestamp")
    }

    fn metadata(turn: u128, attempt: u128) -> RequestMetadata {
        RequestMetadata::for_turn(
            "00000000-0000-0000-0000-000000000001"
                .parse::<RunId>()
                .expect("run"),
            format!("00000000-0000-0000-0000-{turn:012x}")
                .parse::<TurnId>()
                .expect("turn"),
            format!("00000000-0000-0000-0000-{attempt:012x}")
                .parse::<AttemptId>()
                .expect("attempt"),
            timestamp(100),
        )
    }

    fn entry(value: u128) -> EntryId {
        format!("00000000-0000-0000-0000-{value:012x}")
            .parse()
            .expect("entry")
    }

    fn descriptor(route: &str) -> ResolvedModelDescriptor {
        descriptor_at(route, timestamp(100))
    }

    fn descriptor_at(route: &str, observed_at: Timestamp) -> ResolvedModelDescriptor {
        let provenance = FactProvenance::new(FactSource::TestFixture, observed_at, None);
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
        ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("openai.codex.responses").expect("dialect"),
            RouteId::new(route).expect("route"),
            ModelId::new("gpt-5.6-codex").expect("model"),
            None,
            Some(ModelFamilyId::new("gpt-5.6").expect("family")),
            ModelAdapterProfileId::new("codex.2026-08-r1").expect("adapter"),
            CapabilitySet::new(capabilities, BTreeMap::new()),
        )
    }

    fn request(input: ConversationView, turn: u128, attempt: u128) -> ModelRequest {
        let selected: SelectedProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::SelectedProviderConfiguration,
                    5,
                    &[b"responses-artifact-test-selected-configuration"],
                )
                .expect("selected configuration digest"),
            )
            .expect("selected configuration digest serialization"),
        )
        .expect("selected configuration identity");
        let effective: EffectiveProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"responses-artifact-test-effective-configuration"],
                )
                .expect("effective configuration digest"),
            )
            .expect("effective configuration digest serialization"),
        )
        .expect("effective configuration identity");
        ModelRequest::builder(selected, effective, input, metadata(turn, attempt))
            .resolved_harness_plan_identity(digest(DigestDomain::ResolvedHarnessPlan, b"tool-plan"))
            .build()
            .expect("request")
    }

    fn admitted(
        descriptor: &ResolvedModelDescriptor,
        request: &ModelRequest,
        account_label: &[u8],
    ) -> (ValidatedRequestPlan, ResponsesArtifactBinding) {
        let plan = descriptor
            .validate_request(request.clone(), ValidationMode::Strict)
            .expect("admitted request");
        let binding = ResponsesArtifactBinding::from_admitted_operation(
            descriptor,
            &plan,
            request,
            digest(DigestDomain::ModelBinding, account_label),
            ResponsesSemanticRevisions::new(1, 7, 3).expect("revisions"),
            Some(digest(DigestDomain::CacheAffinity, b"branch-affinity-a")),
        )
        .expect("binding");
        (plan, binding)
    }

    fn replay_for_model(model: &str, payload: String) -> ReplayEnvelope {
        ReplayEnvelope::new(
            ProtocolId::new("openai.codex.responses").expect("dialect"),
            Some(ModelFamilyId::new("gpt-5.6").expect("family")),
            plakous_model::ReplayScope::ExactModel(ModelId::new(model).expect("model")),
            ReplayKindId::new("responses.output_item").expect("kind"),
            1,
            DataClassification::Sensitive,
            RawValue::from_string(payload).expect("raw replay json"),
        )
        .expect("replay")
    }

    fn replay(payload: String) -> ReplayEnvelope {
        replay_for_model("gpt-5.6-codex", payload)
    }

    fn contract() -> ResponsesReplayContract {
        ResponsesReplayContract::new(
            ResponsesArtifactReplayKind::new(
                ReplayKindId::new("responses.output_item").expect("kind"),
                1,
            )
            .expect("kind contract"),
        )
    }

    fn output_item(id: u128, envelope: ReplayEnvelope) -> ConversationItem {
        ConversationItem::new(
            entry(id),
            Actor::Assistant,
            vec![ContentBlock::Opaque(
                OpaqueBlock::new(envelope, OpaqueFallback::CompatibleReplay).expect("opaque"),
            )],
            Provenance::ModelOutput,
        )
        .expect("output item")
    }

    fn user_item(id: u128, text: &str) -> ConversationItem {
        ConversationItem::new(
            entry(id),
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new(text, None).expect("text"),
            )],
            Provenance::UserInput,
        )
        .expect("user item")
    }

    fn fixture(storage: ResponsesStorageMode, expires_at: Option<Timestamp>) -> Fixture {
        let descriptor = descriptor("codex.subscription");
        let producer_request = request(ConversationView::default(), 2, 3);
        let (producer_plan, binding) = admitted(&descriptor, &producer_request, b"account-a");
        let envelope = replay(r#"{"type":"message","id":"msg_1"}"#.to_owned());
        let output = output_item(10, envelope.clone());
        let next_view = ConversationView::from_items(vec![output, user_item(11, "turn two")])
            .expect("test conversation items form one valid segment");
        let covered = next_view
            .canonical_item_prefix_identity(1)
            .expect("covered prefix")
            .identity();
        let production = ResponsesArtifactProduction::from_admitted_response(
            &producer_request,
            &producer_plan,
            digest(DigestDomain::ProviderOperation, b"physical-request-1"),
            digest(DigestDomain::ProviderOperation, b"terminal-response-1"),
            1,
            covered,
        )
        .expect("production");
        let continuation = matches!(storage, ResponsesStorageMode::ProviderStored).then(|| {
            ResponsesContinuation::new("resp_exact_1", 1, covered, production.terminal_response())
                .expect("continuation")
        });
        let contract = contract();
        let artifact = ResponsesArtifact::new(
            binding.clone(),
            production.clone(),
            contract.clone(),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                envelope.clone(),
            )],
            continuation,
            None,
            storage,
            expires_at,
            timestamp(100),
        )
        .expect("artifact");
        let next_request = ModelRequest::from_parts(
            producer_request.stable_prefix().clone(),
            next_view,
            metadata(4, 5),
        )
        .expect("next request");
        let (_, current_binding) = admitted(&descriptor, &next_request, b"account-a");
        Fixture {
            descriptor,
            producer_request,
            producer_plan,
            binding,
            production,
            contract,
            envelope,
            artifact,
            next_request,
            current_binding,
        }
    }

    #[test]
    fn turn_one_artifact_codec_turn_two_bridge_selects_one_reuse_path() {
        let stored = fixture(ResponsesStorageMode::ProviderStored, Some(timestamp(200)));
        let bytes = ResponsesArtifactCodec::encode(&stored.artifact).expect("encode");
        let inert = ResponsesArtifactCodec::decode_inert(&bytes).expect("inert decode");
        let active = inert
            .validate_for_reuse(
                &stored.current_binding,
                &stored.next_request,
                timestamp(150),
            )
            .expect("active reuse");
        assert_eq!(
            active.kind(),
            ResponsesArtifactReuseKind::ProviderContinuation
        );
        assert_eq!(
            active.continuation().expect("continuation").response_id(),
            "resp_exact_1"
        );
        assert!(active.replay().is_empty());
        assert!(
            active
                .replay_at(ResponsesReplayPurpose::OutputItem, 0)
                .is_none()
        );

        let local = fixture(ResponsesStorageMode::Stateless, None);
        let active = local
            .artifact
            .validate_for_reuse(&local.current_binding, &local.next_request, timestamp(150))
            .expect("local replay");
        assert_eq!(active.kind(), ResponsesArtifactReuseKind::LocalReplay);
        assert!(active.continuation().is_none());
        assert_eq!(active.replay().len(), 1);
        assert_eq!(
            active
                .replay_at(ResponsesReplayPurpose::OutputItem, 0)
                .expect("coordinate")
                .envelope()
                .digest(),
            local.envelope.digest()
        );
    }

    #[test]
    fn active_artifact_ignores_historical_foreign_replay_but_refuses_replay_tail() {
        let descriptor = descriptor("codex.subscription");
        let historical = output_item(
            40,
            replay_for_model(
                "gpt-5.3-codex-spark",
                r#"{"type":"reasoning","id":"old"}"#.to_owned(),
            ),
        );
        let producer_request = request(
            ConversationView::from_items(vec![historical.clone()])
                .expect("test conversation items form one valid segment"),
            20,
            21,
        );
        let (producer_plan, binding) = admitted(&descriptor, &producer_request, b"account-a");
        let target_envelope = replay(r#"{"type":"function_call","id":"new"}"#.to_owned());
        let target_output = output_item(41, target_envelope.clone());
        let covered_view =
            ConversationView::from_items(vec![historical.clone(), target_output.clone()])
                .expect("test conversation items form one valid segment");
        let covered = covered_view
            .canonical_item_prefix_identity(2)
            .expect("covered prefix")
            .identity();
        let production = ResponsesArtifactProduction::from_admitted_response(
            &producer_request,
            &producer_plan,
            digest(DigestDomain::ProviderOperation, b"mixed-history-request"),
            digest(DigestDomain::ProviderOperation, b"mixed-history-terminal"),
            1,
            covered,
        )
        .expect("production");
        let artifact = ResponsesArtifact::new(
            binding,
            production,
            contract(),
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                target_envelope.clone(),
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100),
        )
        .expect("artifact");
        let next_request = ModelRequest::from_parts(
            producer_request.stable_prefix().clone(),
            ConversationView::from_items(vec![
                historical.clone(),
                target_output.clone(),
                user_item(42, "tool result"),
            ])
            .expect("test conversation items form one valid segment"),
            metadata(22, 23),
        )
        .expect("next request");
        let (_, current) = admitted(&descriptor, &next_request, b"account-a");
        let reuse = artifact
            .validate_for_reuse(&current, &next_request, timestamp(150))
            .expect("historical foreign replay is outside this artifact's authority");
        assert_eq!(reuse.replay().len(), 1);
        let authorized = reuse.replay()[0];
        assert_eq!(authorized.canonical_position(), 1);
        assert_eq!(authorized.envelope().digest(), target_envelope.digest());

        let tail_request = ModelRequest::from_parts(
            producer_request.stable_prefix().clone(),
            ConversationView::from_items(vec![
                historical,
                target_output,
                output_item(
                    43,
                    replay(r#"{"type":"message","id":"unauthorized"}"#.to_owned()),
                ),
            ])
            .expect("test conversation items form one valid segment"),
            metadata(24, 25),
        )
        .expect("tail request");
        let (_, tail_binding) = admitted(&descriptor, &tail_request, b"account-a");
        assert!(matches!(
            artifact.validate_for_reuse(&tail_binding, &tail_request, timestamp(150)),
            Err(ResponsesArtifactError::UnauthorizedReplayTail { position: 2 })
        ));
    }

    #[test]
    fn codec_round_trip_and_golden_are_canonical() {
        let fixture = fixture(ResponsesStorageMode::ProviderStored, Some(timestamp(200)));
        let encoded = ResponsesArtifactCodec::encode(&fixture.artifact).expect("encode");
        let decoded = ResponsesArtifactCodec::decode(
            &encoded,
            &fixture.binding,
            &fixture.production,
            timestamp(150),
        )
        .expect("exact decode");
        assert_eq!(decoded, fixture.artifact);
        assert_eq!(
            decoded.binding().cache_affinity(),
            Some(digest(DigestDomain::CacheAffinity, b"branch-affinity-a"))
        );
        assert_eq!(encoded.len(), fixture.artifact.encoded_len());
        assert_eq!(encoded.len(), 1011);
        assert_eq!(
            *decoded.integrity(),
            [
                0x51, 0x17, 0x03, 0xed, 0x98, 0xa3, 0xee, 0x03, 0x91, 0xcd, 0x76, 0xe4, 0xa8, 0xe9,
                0x1a, 0x76, 0xe3, 0x17, 0x31, 0x02, 0xee, 0xc3, 0xe5, 0x4e, 0x3d, 0x5d, 0xc3, 0x45,
                0x45, 0x36, 0x5c, 0xba,
            ]
        );
    }

    #[test]
    fn binding_identity_covers_cache_affinity_and_rejects_foreign_domain() {
        let descriptor = descriptor("codex.subscription");
        let request = request(ConversationView::default(), 2, 3);
        let plan = descriptor
            .validate_request(request.clone(), ValidationMode::Strict)
            .expect("admitted request");
        let revisions = ResponsesSemanticRevisions::new(1, 7, 3).expect("revisions");
        let account = digest(DigestDomain::ModelBinding, b"account-a");
        let bind = |affinity| {
            ResponsesArtifactBinding::from_admitted_operation(
                &descriptor,
                &plan,
                &request,
                account,
                revisions,
                affinity,
            )
        };
        let first_affinity = digest(DigestDomain::CacheAffinity, b"branch-affinity-a");
        let rotated_affinity = digest(DigestDomain::CacheAffinity, b"branch-affinity-b");
        let first = bind(Some(first_affinity)).expect("first affinity");
        let rotated = bind(Some(rotated_affinity)).expect("rotated affinity");
        let absent = bind(None).expect("absent affinity");

        assert_eq!(first.cache_affinity(), Some(first_affinity));
        assert_ne!(first.identity(), rotated.identity());
        assert_ne!(first.identity(), absent.identity());
        assert!(matches!(
            bind(Some(digest(
                DigestDomain::ModelBinding,
                b"foreign-affinity"
            ))),
            Err(ResponsesArtifactError::InvalidDigest {
                field: "cache affinity"
            })
        ));

        let stored = fixture(ResponsesStorageMode::Stateless, None);
        let current_plan = stored
            .descriptor
            .validate_request(stored.next_request.clone(), ValidationMode::Strict)
            .expect("current admitted request");
        let rotated_current = ResponsesArtifactBinding::from_admitted_operation(
            &stored.descriptor,
            &current_plan,
            &stored.next_request,
            account,
            revisions,
            Some(rotated_affinity),
        )
        .expect("rotated current binding");
        assert_eq!(
            stored
                .artifact
                .validate_for_reuse(&rotated_current, &stored.next_request, timestamp(150))
                .map(|_| ()),
            Err(ResponsesArtifactError::BindingMismatch)
        );
    }

    #[test]
    fn reusable_target_ignores_descriptor_observation_freshness() {
        let original = descriptor_at("codex.subscription", timestamp(100));
        let refreshed = descriptor_at("codex.subscription", timestamp(200));
        assert_ne!(
            original.binding_digest().expect("original exact binding"),
            refreshed.binding_digest().expect("refreshed exact binding")
        );
        assert_eq!(
            original
                .semantic_binding_digest()
                .expect("original semantic binding"),
            refreshed
                .semantic_binding_digest()
                .expect("refreshed semantic binding")
        );

        let request = request(ConversationView::default(), 2, 3);
        let (_, original_binding) = admitted(&original, &request, b"account-a");
        let (_, refreshed_binding) = admitted(&refreshed, &request, b"account-a");
        assert!(original_binding.same_reuse_target(&refreshed_binding));
    }

    #[test]
    fn expired_history_decodes_inert_but_cannot_be_reused() {
        let fixture = fixture(ResponsesStorageMode::ProviderStored, Some(timestamp(200)));
        let encoded = ResponsesArtifactCodec::encode(&fixture.artifact).expect("encode");
        let decoded = ResponsesArtifactCodec::decode_inert(&encoded).expect("historical decode");
        decoded.validate_integrity().expect("structural integrity");
        assert!(decoded.is_expired(timestamp(200)));
        assert_eq!(
            decoded.validate_for(&fixture.binding, &fixture.production, timestamp(200)),
            Err(ResponsesArtifactError::Expired)
        );
        assert_eq!(
            decoded
                .validate_for_reuse(
                    &fixture.current_binding,
                    &fixture.next_request,
                    timestamp(200)
                )
                .map(|_| ()),
            Err(ResponsesArtifactError::Expired)
        );
    }

    #[test]
    fn cross_route_account_attempt_and_physical_producer_fail_closed() {
        let fixture = fixture(ResponsesStorageMode::Stateless, None);
        let other_route = descriptor("other.route");
        let (_, route_binding) = admitted(&other_route, &fixture.next_request, b"account-a");
        assert_eq!(
            fixture
                .artifact
                .validate_for_reuse(&route_binding, &fixture.next_request, timestamp(150))
                .map(|_| ()),
            Err(ResponsesArtifactError::BindingMismatch)
        );
        let (_, account_binding) =
            admitted(&fixture.descriptor, &fixture.next_request, b"account-b");
        assert_eq!(
            fixture
                .artifact
                .validate_for_reuse(&account_binding, &fixture.next_request, timestamp(150))
                .map(|_| ()),
            Err(ResponsesArtifactError::BindingMismatch)
        );

        let different_attempt = fixture
            .producer_request
            .for_round(metadata(2, 99))
            .expect("other attempt");
        let (different_plan, attempt_binding) =
            admitted(&fixture.descriptor, &different_attempt, b"account-a");
        assert_eq!(
            fixture
                .artifact
                .validate_for(&attempt_binding, &fixture.production, timestamp(150)),
            Err(ResponsesArtifactError::BindingMismatch)
        );

        let mut physical = fixture.production.clone();
        physical.physical_request = digest(DigestDomain::ProviderOperation, b"other-physical");
        assert_eq!(
            fixture
                .artifact
                .validate_for(&fixture.binding, &physical, timestamp(150)),
            Err(ResponsesArtifactError::BindingMismatch)
        );

        let transplanted = ResponsesArtifactProduction::from_admitted_response(
            &different_attempt,
            &different_plan,
            digest(DigestDomain::ProviderOperation, b"other-physical"),
            digest(DigestDomain::ProviderOperation, b"other-terminal"),
            1,
            fixture.production.covered_projection(),
        )
        .expect("independently valid foreign production");
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding.clone(),
                transplanted,
                fixture.contract.clone(),
                vec![ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    0,
                    fixture.envelope.clone(),
                )],
                None,
                None,
                ResponsesStorageMode::Stateless,
                None,
                timestamp(150),
            ),
            Err(ResponsesArtifactError::ProductionBindingMismatch)
        );
    }

    #[test]
    fn coordinate_transplant_and_request_projection_drift_fail_closed() {
        let fixture = fixture(ResponsesStorageMode::Stateless, None);
        let drifted_view = ConversationView::from_items(vec![
            user_item(20, "foreign prefix"),
            output_item(21, fixture.envelope.clone()),
        ])
        .expect("test conversation items form one valid segment");
        let drifted_request = ModelRequest::from_parts(
            fixture.producer_request.stable_prefix().clone(),
            drifted_view,
            metadata(4, 6),
        )
        .expect("drifted request");
        let (_, drifted_binding) = admitted(&fixture.descriptor, &drifted_request, b"account-a");
        assert_eq!(
            fixture
                .artifact
                .validate_for_reuse(&drifted_binding, &drifted_request, timestamp(150))
                .map(|_| ()),
            Err(ResponsesArtifactError::CanonicalCoordinateMismatch)
        );
    }

    #[test]
    fn purpose_kind_mislabeling_and_ambiguous_contract_are_rejected() {
        let fixture = fixture(ResponsesStorageMode::Stateless, None);
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding.clone(),
                fixture.production.clone(),
                fixture.contract.clone(),
                vec![ResponsesReplayItem::new(
                    ResponsesReplayPurpose::EncryptedReasoning,
                    0,
                    fixture.envelope.clone()
                )],
                None,
                None,
                ResponsesStorageMode::Stateless,
                None,
                timestamp(100)
            ),
            Err(ResponsesArtifactError::ReplayPurposeKindMismatch { index: 0 })
        );
        let same = ResponsesArtifactReplayKind::new(
            ReplayKindId::new("responses.output_item").expect("kind"),
            2,
        )
        .expect("kind pin");
        assert_eq!(
            contract().with_encrypted_reasoning(same),
            Err(ResponsesArtifactError::AmbiguousReplayContract)
        );

        let secret = ReplayEnvelope::new(
            fixture.binding.dialect().clone(),
            fixture.binding.model_family().cloned(),
            plakous_model::ReplayScope::ExactModel(fixture.binding.model().clone()),
            ReplayKindId::new("responses.output_item").expect("kind"),
            1,
            DataClassification::Secret,
            RawValue::from_string(r#"{"credential":"forbidden"}"#.to_owned()).expect("raw json"),
        )
        .expect("secret envelope");
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding,
                fixture.production,
                fixture.contract,
                vec![ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    0,
                    secret
                )],
                None,
                None,
                ResponsesStorageMode::Stateless,
                None,
                timestamp(100)
            ),
            Err(ResponsesArtifactError::SecretReplayMaterial { index: 0 })
        );
    }

    #[test]
    fn continuation_and_cache_coverage_must_be_exact() {
        let fixture = fixture(ResponsesStorageMode::ProviderStored, None);
        let foreign = digest(DigestDomain::CanonicalItemPrefix, b"foreign-prefix");
        let continuation = ResponsesContinuation::new(
            "resp_1",
            fixture.production.covered_projection_items(),
            foreign,
            fixture.production.terminal_response(),
        )
        .expect("structural continuation");
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding.clone(),
                fixture.production.clone(),
                fixture.contract.clone(),
                vec![ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    0,
                    fixture.envelope.clone(),
                )],
                Some(continuation),
                None,
                ResponsesStorageMode::ProviderStored,
                None,
                timestamp(100)
            ),
            Err(ResponsesArtifactError::ContinuationCoverageMismatch)
        );
        let cache = ResponsesCacheFact::new(
            "cache_1",
            fixture.binding.stable_prefix(),
            fixture.production.covered_projection_items(),
            foreign,
        )
        .expect("structural cache");
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding,
                fixture.production,
                fixture.contract,
                vec![ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    0,
                    fixture.envelope.clone(),
                )],
                fixture.artifact.continuation,
                Some(cache),
                ResponsesStorageMode::ProviderStored,
                None,
                timestamp(100)
            ),
            Err(ResponsesArtifactError::CacheCoverageMismatch)
        );
    }

    #[test]
    fn count_and_aggregate_replay_bounds_fail_before_body_amplification() {
        let fixture = fixture(ResponsesStorageMode::Stateless, None);
        let item = ResponsesReplayItem::new(
            ResponsesReplayPurpose::OutputItem,
            0,
            fixture.envelope.clone(),
        );
        let count = u32::try_from(MAX_RESPONSES_REPLAY_ITEMS + 1).expect("count");
        let production = ResponsesArtifactProduction::from_parts(
            fixture.production.producer_request(),
            fixture.production.producing_projection(),
            fixture.production.validated_plan(),
            fixture.production.physical_request(),
            fixture.production.terminal_response(),
            0,
            count,
            count,
            digest(DigestDomain::CanonicalItemPrefix, b"large-covered"),
        )
        .expect("production");
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding.clone(),
                production,
                fixture.contract.clone(),
                vec![item; MAX_RESPONSES_REPLAY_ITEMS + 1],
                None,
                None,
                ResponsesStorageMode::Stateless,
                None,
                timestamp(100)
            ),
            Err(ResponsesArtifactError::TooManyReplayItems {
                maximum: MAX_RESPONSES_REPLAY_ITEMS
            })
        );

        let output_count = 4_u32;
        let production = ResponsesArtifactProduction::from_parts(
            fixture.production.producer_request(),
            fixture.production.producing_projection(),
            fixture.production.validated_plan(),
            fixture.production.physical_request(),
            fixture.production.terminal_response(),
            0,
            output_count,
            output_count,
            digest(DigestDomain::CanonicalItemPrefix, b"aggregate-covered"),
        )
        .expect("production");
        let large = format!(r#"{{"opaque":"{}"}}"#, "x".repeat(800_000));
        let replay = (0..output_count)
            .map(|index| {
                ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    index,
                    replay(large.clone()),
                )
            })
            .collect();
        assert_eq!(
            ResponsesArtifact::new(
                fixture.binding,
                production,
                fixture.contract,
                replay,
                None,
                None,
                ResponsesStorageMode::Stateless,
                None,
                timestamp(100)
            ),
            Err(ResponsesArtifactError::ReplayBytesTooLarge {
                maximum: MAX_RESPONSES_REPLAY_BYTES
            })
        );
    }

    #[test]
    fn identical_envelopes_remain_valid_at_distinct_exact_coordinates() {
        let fixture = fixture(ResponsesStorageMode::Stateless, None);
        let view = ConversationView::from_items(vec![
            output_item(30, fixture.envelope.clone()),
            output_item(31, fixture.envelope.clone()),
        ])
        .expect("test conversation items form one valid segment");
        let covered = view
            .canonical_item_prefix_identity(2)
            .expect("covered")
            .identity();
        let production = ResponsesArtifactProduction::from_admitted_response(
            &fixture.producer_request,
            &fixture.producer_plan,
            fixture.production.physical_request(),
            fixture.production.terminal_response(),
            2,
            covered,
        )
        .expect("production");
        let artifact = ResponsesArtifact::new(
            fixture.binding,
            production,
            fixture.contract,
            vec![
                ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    0,
                    fixture.envelope.clone(),
                ),
                ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    1,
                    fixture.envelope.clone(),
                ),
            ],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            timestamp(100),
        )
        .expect("artifact");
        let next_request = ModelRequest::from_parts(
            fixture.producer_request.stable_prefix().clone(),
            view,
            metadata(4, 7),
        )
        .expect("request");
        let (_, current) = admitted(&fixture.descriptor, &next_request, b"account-a");
        let active = artifact
            .validate_for_reuse(&current, &next_request, timestamp(150))
            .expect("active replay");
        let first = active
            .replay_at(ResponsesReplayPurpose::OutputItem, 0)
            .expect("first");
        let second = active
            .replay_at(ResponsesReplayPurpose::OutputItem, 1)
            .expect("second");
        assert_eq!(first.envelope().digest(), second.envelope().digest());
        assert_ne!(first.canonical_position(), second.canonical_position());
    }

    #[test]
    fn corruption_unknown_revision_and_outer_bound_are_rejected() {
        let fixture = fixture(ResponsesStorageMode::Stateless, None);
        let encoded = ResponsesArtifactCodec::encode(&fixture.artifact).expect("encode");
        let mut corrupt = encoded.clone();
        corrupt[ENVELOPE_OVERHEAD] ^= 1;
        assert_eq!(
            ResponsesArtifactCodec::decode_inert(&corrupt),
            Err(ResponsesArtifactError::IntegrityMismatch)
        );
        let mut unknown = encoded;
        unknown[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&99_u32.to_be_bytes());
        assert_eq!(
            ResponsesArtifactCodec::decode_inert(&unknown),
            Err(ResponsesArtifactError::UnsupportedCodecRevision { found: 99 })
        );
        assert_eq!(
            ResponsesArtifactCodec::decode_inert(&vec![0; MAX_RESPONSES_ARTIFACT_BYTES + 1]),
            Err(ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES
            })
        );
    }

    #[test]
    fn sensitive_native_values_are_redacted_from_debug() {
        let fixture = fixture(ResponsesStorageMode::ProviderStored, None);
        let sensitive = replay(format!(
            r#"{{"type":"reasoning","encrypted_content":"{SECRET}"}}"#
        ));
        let artifact = ResponsesArtifact::new(
            fixture.binding,
            fixture.production,
            fixture.contract,
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                sensitive,
            )],
            fixture.artifact.continuation,
            None,
            ResponsesStorageMode::ProviderStored,
            None,
            timestamp(100),
        )
        .expect("payload-free artifact");
        let debug = format!("{artifact:?}");
        assert!(!debug.contains(SECRET));
        assert_eq!(artifact.replay_bytes(), 0);
        assert!(
            !String::from_utf8_lossy(&ResponsesArtifactCodec::encode(&artifact).expect("encode"))
                .contains(SECRET)
        );
        assert!(!format!("{:?}", artifact.continuation()).contains("resp_exact_1"));
    }
}
