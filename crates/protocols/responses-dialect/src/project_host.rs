//! Concrete terminal projection over shared Responses output values.
//!
//! The host preserves exact output order, derives canonical finish/usage facts,
//! and prepares bounded replay envelopes for the Responses family artifact.
//! Route/account/storage/cache binding remains the artifact owner's authority.

use std::fmt;

use plakous_model::{
    Actor, CanonicalItemPrefixIdentity, CitationUnit, ContentBlock, ConversationItem,
    DataClassification, Digest, DigestDomain, FamilyArtifactProducerFacts, FinishDetails,
    FinishReason, ModelFamilyId, ModelId, ModelProviderRequestId, ModelRequest, ProtocolId,
    Provenance, ReasoningVisibility, ReplayEnvelope, ReplayKindId, ReplayScope,
    SealedFamilyArtifactCandidate, TextPhase, Timestamp, ToolCallState, ToolExecutionOwner,
    ToolInputContract, Usage, ValidatedRequestPlan,
};
use plakous_responses::{
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactError,
    ResponsesArtifactProduction, ResponsesArtifactReplayKind, ResponsesContinuation,
    ResponsesReplayContract, ResponsesReplayItem, ResponsesReplayPurpose, ResponsesStorageMode,
};
use serde_json::value::RawValue;

use crate::response_host::{
    ResponsesAnnotation, ResponsesContentPart, ResponsesOutputItem, ResponsesReasoningSummaryPart,
    ResponsesResponse, ResponsesResponseStatus,
};

/// Exact replay-envelope construction data supplied by the admitted dialect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesReplayProjection {
    protocol: ProtocolId,
    model_family: Option<ModelFamilyId>,
    model: ModelId,
    output_item_kind: ReplayKindId,
    version: u32,
    opaque_output_kind: Option<(ReplayKindId, u32)>,
    require_encrypted_reasoning: bool,
    require_completed_reasoning_status: bool,
}

impl ResponsesReplayProjection {
    /// Builds exact-model replay construction data.
    ///
    /// # Errors
    ///
    /// Returns [`ResponsesProjectError::InvalidReplayVersion`] for version zero.
    pub fn new(
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        model: ModelId,
        output_item_kind: ReplayKindId,
        version: u32,
    ) -> Result<Self, ResponsesProjectError> {
        if version == 0 {
            return Err(ResponsesProjectError::InvalidReplayVersion);
        }
        Ok(Self {
            protocol,
            model_family,
            model,
            output_item_kind,
            version,
            opaque_output_kind: None,
            require_encrypted_reasoning: false,
            require_completed_reasoning_status: false,
        })
    }

    /// Requires every reasoning output item to be completed and carry a
    /// non-empty encrypted replay payload before an artifact can be promoted.
    #[must_use]
    pub const fn requiring_encrypted_reasoning(mut self) -> Self {
        self.require_encrypted_reasoning = true;
        self.require_completed_reasoning_status = true;
        self
    }

    /// Requires encrypted reasoning while accepting the Codex reference
    /// client's status-less item shape. A present status must still be
    /// `completed`.
    #[must_use]
    pub const fn requiring_encrypted_reasoning_with_optional_status(mut self) -> Self {
        self.require_encrypted_reasoning = true;
        self
    }

    /// Uses a distinct exact-model replay kind for future opaque output items.
    ///
    /// # Errors
    ///
    /// Returns [`ResponsesProjectError::InvalidReplayVersion`] for version zero.
    pub fn with_opaque_output_kind(
        mut self,
        kind: ReplayKindId,
        version: u32,
    ) -> Result<Self, ResponsesProjectError> {
        if version == 0 {
            return Err(ResponsesProjectError::InvalidReplayVersion);
        }
        self.opaque_output_kind = Some((kind, version));
        Ok(self)
    }

    /// Returns the exact known-output replay kind.
    #[must_use]
    pub const fn output_kind(&self) -> &ReplayKindId {
        &self.output_item_kind
    }

    /// Returns the exact known-output replay version.
    #[must_use]
    pub const fn output_kind_version(&self) -> u32 {
        self.version
    }

    /// Returns the distinct future-output replay kind when configured.
    #[must_use]
    pub fn opaque_output_kind(&self) -> Option<&ReplayKindId> {
        self.opaque_output_kind.as_ref().map(|(kind, _)| kind)
    }

    /// Returns the distinct future-output replay version when configured.
    #[must_use]
    pub fn opaque_output_kind_version(&self) -> Option<u32> {
        self.opaque_output_kind
            .as_ref()
            .map(|(_, version)| *version)
    }

    /// Confirms that this replay projection belongs to one exact admitted target.
    pub(crate) fn matches_target(
        &self,
        protocol: &ProtocolId,
        model_family: Option<&ModelFamilyId>,
        model: &ModelId,
    ) -> bool {
        &self.protocol == protocol
            && self.model_family.as_ref() == model_family
            && &self.model == model
    }

    fn artifact_replay_contract(&self) -> Result<ResponsesReplayContract, ResponsesProjectError> {
        let output = ResponsesArtifactReplayKind::new(self.output_item_kind.clone(), self.version)?;
        let mut contract = ResponsesReplayContract::new(output);
        if let Some((kind, version)) = &self.opaque_output_kind {
            contract = contract
                .with_opaque_output(ResponsesArtifactReplayKind::new(kind.clone(), *version)?)?;
        }
        Ok(contract)
    }

    /// Returns the complete behavior-bearing execution identity for attempt binding.
    pub(crate) fn execution_identity(&self) -> Result<Digest, ResponsesProjectError> {
        let version = self.version.to_be_bytes();
        let (family_presence, family) = self
            .model_family
            .as_ref()
            .map_or((b"absent".as_slice(), ""), |family| {
                (b"present".as_slice(), family.as_str())
            });
        let opaque_version = self
            .opaque_output_kind
            .as_ref()
            .map_or(0_u32, |(_, version)| *version)
            .to_be_bytes();
        let opaque_kind = self
            .opaque_output_kind
            .as_ref()
            .map_or("", |(kind, _)| kind.as_str());
        let encrypted_reasoning = if self.require_encrypted_reasoning {
            b"required".as_slice()
        } else {
            b"not-required".as_slice()
        };
        let completed_status = if self.require_completed_reasoning_status {
            b"required".as_slice()
        } else {
            b"optional".as_slice()
        };
        Digest::sha256(
            DigestDomain::ModelBinding,
            3,
            &[
                b"plak-responses/replay-projection-execution-v3",
                self.protocol.as_str().as_bytes(),
                family_presence,
                family.as_bytes(),
                self.model.as_str().as_bytes(),
                self.output_item_kind.as_str().as_bytes(),
                &version,
                opaque_kind.as_bytes(),
                &opaque_version,
                encrypted_reasoning,
                completed_status,
            ],
        )
        .map_err(|_| ResponsesProjectError::ProjectionIdentity)
    }
}

/// Request metadata captured by the owner of one exact physical response head.
///
/// Construction is crate-private so canonical projection cannot accept a loose
/// provider request ID selected by an outer dialect adapter.
#[derive(Clone, Eq, PartialEq)]
pub struct ResponsesProviderRequestBinding {
    physical_request: Digest,
    terminal_response: Digest,
    provider_request_id: Option<ModelProviderRequestId>,
}

impl ResponsesProviderRequestBinding {
    /// Binds one validated response-head request ID to its physical request.
    pub(crate) fn new(
        physical_request: Digest,
        terminal_response: Digest,
        provider_request_id: Option<ModelProviderRequestId>,
    ) -> Result<Self, ResponsesProjectError> {
        if physical_request.domain() != DigestDomain::ProviderOperation
            || terminal_response.domain() != DigestDomain::ProviderOperation
            || physical_request == terminal_response
        {
            return Err(ResponsesProjectError::InvalidProviderRequestBinding);
        }
        Ok(Self {
            physical_request,
            terminal_response,
            provider_request_id,
        })
    }

    pub(crate) const fn physical_request(&self) -> Digest {
        self.physical_request
    }

    pub(crate) const fn terminal_response(&self) -> Digest {
        self.terminal_response
    }

    pub(crate) fn canonical_provider_request_id(&self) -> Option<ModelProviderRequestId> {
        self.provider_request_id.clone()
    }
}

impl fmt::Debug for ResponsesProviderRequestBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesProviderRequestBinding")
            .field("physical_request", &self.physical_request)
            .field("terminal_response", &self.terminal_response)
            .field(
                "provider_request_id",
                &self.provider_request_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Reconstruction-ready input supplied to the Responses artifact owner.
#[derive(Clone, PartialEq)]
pub struct ResponsesArtifactCandidateInput {
    storage: ResponsesStorageMode,
    output_count: u32,
    response_id: String,
    replay_contract: ResponsesReplayContract,
    replay: Vec<ResponsesReplayItem>,
    physical_request: Digest,
    terminal_response: Digest,
}

impl fmt::Debug for ResponsesArtifactCandidateInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesArtifactCandidateInput")
            .field("storage", &self.storage)
            .field("output_count", &self.output_count)
            .field("response_id", &"<redacted>")
            .field("replay_contract", &self.replay_contract)
            .field("replay", &self.replay)
            .field("physical_request", &self.physical_request)
            .field("terminal_response", &self.terminal_response)
            .finish()
    }
}

impl ResponsesArtifactCandidateInput {
    /// Exact response-retention policy used for this candidate.
    #[must_use]
    pub const fn storage(&self) -> ResponsesStorageMode {
        self.storage
    }

    /// Exact number of native output items in this one-response group.
    #[must_use]
    pub const fn output_count(&self) -> u32 {
        self.output_count
    }

    /// Raw provider response identity, not yet promoted as continuation authority.
    #[must_use]
    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    /// Exact purpose-to-replay-kind contract admitted by the dialect.
    #[must_use]
    pub const fn replay_contract(&self) -> &ResponsesReplayContract {
        &self.replay_contract
    }

    /// Ordered raw official output-item replay candidates.
    #[must_use]
    pub fn replay(&self) -> &[ResponsesReplayItem] {
        &self.replay
    }
}

/// One exact terminal aggregate plus canonical observation facts.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesTerminalProjection {
    response: ResponsesResponse,
    finish: Option<FinishDetails>,
    usage: Option<Usage>,
    provider_request: ResponsesProviderRequestBinding,
    artifact_candidate: Option<ResponsesArtifactCandidateInput>,
}

impl ResponsesTerminalProjection {
    /// Exact ordered terminal aggregate.
    #[must_use]
    pub const fn response(&self) -> &ResponsesResponse {
        &self.response
    }

    /// Canonical successful finish classification; failed terminals have none.
    #[must_use]
    pub const fn finish(&self) -> Option<&FinishDetails> {
        self.finish.as_ref()
    }

    /// Canonical provider usage/cache observation.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    pub(crate) const fn provider_request(&self) -> &ResponsesProviderRequestBinding {
        &self.provider_request
    }

    /// Borrows inert native replay material for canonical terminal projection.
    ///
    /// This candidate cannot itself promote an artifact; [`Self::pair_canonical`]
    /// must close the exact raw/canonical pairing first.
    #[must_use]
    pub const fn artifact_candidate(&self) -> Option<&ResponsesArtifactCandidateInput> {
        self.artifact_candidate.as_ref()
    }

    /// Closes raw/canonical output pairing before artifact or effect use.
    ///
    /// The returned value owns the exact canonical output and artifact input
    /// together so an adapter can promote them atomically.
    ///
    /// # Errors
    ///
    /// Returns a typed error for count/order/replay disagreement or any raw
    /// tool call that does not exactly match its canonical `ToolCallBlock`.
    pub fn pair_canonical(
        self,
        producing_request: &ModelRequest,
        canonical_output: Vec<ConversationItem>,
    ) -> Result<ResponsesPairedTerminalProjection, ResponsesProjectError> {
        let paired_artifact = match self.artifact_candidate {
            Some(artifact) => {
                if self.response.output().len() != canonical_output.len()
                    || artifact.replay().len() != canonical_output.len()
                {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "raw and canonical output counts differ",
                    ));
                }
                for (output_index, ((raw, replay), canonical)) in self
                    .response
                    .output()
                    .iter()
                    .zip(artifact.replay())
                    .zip(&canonical_output)
                    .enumerate()
                {
                    if usize::try_from(replay.output_index()).ok() != Some(output_index) {
                        return Err(ResponsesProjectError::CanonicalPair(
                            "raw replay coordinate differs from canonical output order",
                        ));
                    }
                    validate_exact_replay_pair(replay.envelope(), canonical)?;
                    validate_output_semantic_pair(
                        raw,
                        replay.envelope(),
                        canonical,
                        producing_request,
                    )?;
                }
                Some(ResponsesPairedArtifactCandidate {
                    canonical_output,
                    artifact,
                    producing_request: producing_request.identity(),
                    producing_projection_items: u32::try_from(
                        producing_request.input().items().len(),
                    )
                    .map_err(|_| ResponsesProjectError::OutputIndex)?,
                })
            }
            None if canonical_output.is_empty() => None,
            None => {
                return Err(ResponsesProjectError::CanonicalPair(
                    "failed terminal carried canonical output",
                ));
            }
        };
        Ok(ResponsesPairedTerminalProjection {
            response: self.response,
            finish: self.finish,
            usage: self.usage,
            paired_artifact,
        })
    }
}

/// Closed canonical output plus its exact native artifact candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesPairedArtifactCandidate {
    canonical_output: Vec<ConversationItem>,
    artifact: ResponsesArtifactCandidateInput,
    producing_request: Digest,
    producing_projection_items: u32,
}

impl ResponsesPairedArtifactCandidate {
    /// Returns canonical output in exact provider output-item order.
    #[must_use]
    pub fn canonical_output(&self) -> &[ConversationItem] {
        &self.canonical_output
    }

    /// Returns the exact native artifact construction input.
    #[must_use]
    pub const fn artifact(&self) -> &ResponsesArtifactCandidateInput {
        &self.artifact
    }

    /// Seals exact production facts only after the runtime supplies committed coverage.
    ///
    /// # Errors
    ///
    /// Rejects a different producing plan, coverage that does not end exactly
    /// after this provider output group, or inconsistent artifact facts.
    pub fn seal(
        self,
        plan: &ValidatedRequestPlan,
        covered_projection: CanonicalItemPrefixIdentity,
    ) -> Result<ResponsesSealedArtifactCandidate, ResponsesProjectError> {
        let request = plan.request();
        let expected_coverage = self
            .producing_projection_items
            .checked_add(self.artifact.output_count)
            .ok_or(ResponsesProjectError::OutputIndex)?;
        if request.identity() != self.producing_request
            || u32::try_from(covered_projection.item_count()).ok() != Some(expected_coverage)
        {
            return Err(ResponsesProjectError::InvalidArtifactCoverage);
        }
        let production = ResponsesArtifactProduction::from_admitted_response(
            request,
            plan,
            self.artifact.physical_request,
            self.artifact.terminal_response,
            self.artifact.output_count,
            covered_projection.identity(),
        )?;
        let continuation = if self.artifact.storage == ResponsesStorageMode::ProviderStored
            && self.artifact.output_count != 0
        {
            Some(ResponsesContinuation::new(
                self.artifact.response_id,
                expected_coverage,
                covered_projection.identity(),
                self.artifact.terminal_response,
            )?)
        } else {
            None
        };
        Ok(ResponsesSealedArtifactCandidate {
            canonical_output: self.canonical_output,
            production,
            replay_contract: self.artifact.replay_contract,
            replay: self.artifact.replay,
            continuation,
            storage: self.artifact.storage,
        })
    }
}

/// Runtime-covered canonical output plus complete Responses artifact facts.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesSealedArtifactCandidate {
    canonical_output: Vec<ConversationItem>,
    production: ResponsesArtifactProduction,
    replay_contract: ResponsesReplayContract,
    replay: Vec<ResponsesReplayItem>,
    continuation: Option<ResponsesContinuation>,
    storage: ResponsesStorageMode,
}

impl ResponsesSealedArtifactCandidate {
    /// Consumes the sealed candidate into exact artifact-constructor inputs.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Vec<ConversationItem>,
        ResponsesArtifactProduction,
        ResponsesReplayContract,
        Vec<ResponsesReplayItem>,
        Option<ResponsesContinuation>,
        ResponsesStorageMode,
    ) {
        (
            self.canonical_output,
            self.production,
            self.replay_contract,
            self.replay,
            self.continuation,
            self.storage,
        )
    }
}

/// Encodes one paired and runtime-covered terminal as a neutral Responses sidecar.
///
/// The returned candidate carries no continuation, history, or terminal
/// authority. The neutral runtime must validate it again at the atomic commit
/// boundary before promoting its opaque bytes into a manifest.
///
/// # Errors
///
/// Rejects a foreign plan or binding, covered-prefix/count disagreement,
/// invalid family-local state, or artifact encoding failure.
pub fn seal_responses_family_artifact(
    sealed: ResponsesSealedArtifactCandidate,
    plan: &ValidatedRequestPlan,
    covered_projection: CanonicalItemPrefixIdentity,
    binding: ResponsesArtifactBinding,
    now: Timestamp,
) -> Result<SealedFamilyArtifactCandidate, ResponsesProjectError> {
    let (canonical_output, production, replay_contract, replay, continuation, storage) =
        sealed.into_parts();
    let artifact = ResponsesArtifact::new(
        binding,
        production,
        replay_contract,
        replay,
        continuation,
        None,
        storage,
        None,
        now,
    )?;
    let binding = artifact.binding();
    let production = artifact.production();
    let producer = FamilyArtifactProducerFacts::new(
        production.producer_request(),
        binding.model_input(),
        production.validated_plan(),
        binding.cache_affinity(),
        binding.run(),
        binding.root(),
        binding.attempt(),
        production.producing_projection(),
        production.producing_projection_items(),
        production.output_items(),
        production.covered_projection(),
        production.covered_projection_items(),
        production.physical_request(),
        production.terminal_response(),
    )?;
    producer.validate_for_plan(plan, covered_projection)?;
    let candidate = SealedFamilyArtifactCandidate::new(
        "responses",
        RESPONSES_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_ARTIFACT_CODEC_REVISION,
        binding.identity(),
        producer,
        ResponsesArtifactCodec::encode(&artifact)?,
    )?;
    candidate.validate_for_commit(plan, covered_projection, canonical_output.len())?;
    Ok(candidate)
}

/// One terminal aggregate whose raw/canonical output pairing is closed.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesPairedTerminalProjection {
    response: ResponsesResponse,
    finish: Option<FinishDetails>,
    usage: Option<Usage>,
    paired_artifact: Option<ResponsesPairedArtifactCandidate>,
}

impl ResponsesPairedTerminalProjection {
    /// Returns the exact ordered terminal aggregate.
    #[must_use]
    pub const fn response(&self) -> &ResponsesResponse {
        &self.response
    }

    /// Returns canonical successful finish classification.
    #[must_use]
    pub const fn finish(&self) -> Option<&FinishDetails> {
        self.finish.as_ref()
    }

    /// Returns canonical usage/cache observation.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns the atomic canonical/native promotion candidate.
    #[must_use]
    pub const fn paired_artifact(&self) -> Option<&ResponsesPairedArtifactCandidate> {
        self.paired_artifact.as_ref()
    }

    /// Consumes the closed terminal aggregate and paired promotion candidate.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ResponsesResponse,
        Option<FinishDetails>,
        Option<Usage>,
        Option<ResponsesPairedArtifactCandidate>,
    ) {
        (self.response, self.finish, self.usage, self.paired_artifact)
    }
}

/// Fail-closed terminal/project replay failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResponsesProjectError {
    /// Terminal aggregate is not projectable under the requested operation.
    #[error("Responses terminal aggregate is not projectable: {0}")]
    InvalidTerminal(&'static str),
    /// Replay format version must be non-zero.
    #[error("Responses replay version must be non-zero")]
    InvalidReplayVersion,
    /// Provider request metadata was not bound to a physical request identity.
    #[error("Responses provider request metadata binding is invalid")]
    InvalidProviderRequestBinding,
    /// Runtime coverage does not end at this exact paired provider output group.
    #[error("Responses artifact coverage is not the exact committed output prefix")]
    InvalidArtifactCoverage,
    /// Hardened Responses artifact facts are inconsistent.
    #[error("Responses artifact candidate is invalid")]
    Artifact(#[from] ResponsesArtifactError),
    /// Neutral family-artifact producer facts or sidecar metadata are inconsistent.
    #[error("Responses neutral family artifact candidate is invalid")]
    FamilyArtifact(#[from] plakous_model::FamilyArtifactError),
    /// Output count overflowed the typed provider index.
    #[error("Responses output count exceeded typed index bounds")]
    OutputIndex,
    /// Exact native replay serialization failed.
    #[error("Responses replay serialization failed")]
    Json(#[from] serde_json::Error),
    /// Canonical replay-envelope validation failed.
    #[error("Responses replay envelope is invalid")]
    Replay(#[from] plakous_model::ContentError),
    /// Raw provider output and canonical semantic projection disagreed.
    #[error("Responses raw/canonical output pairing is invalid: {0}")]
    CanonicalPair(&'static str),
    /// Replay projection execution identity could not be constructed.
    #[error("Responses replay projection identity construction failed")]
    ProjectionIdentity,
}

/// Projects one exact terminal response into canonical observation facts and
/// Responses-artifact candidate input.
///
/// Failed terminals remain typed aggregates but intentionally produce no
/// continuation/artifact candidate.
///
/// # Errors
///
/// Fails closed for active/unknown status, model-binding mismatch, index
/// overflow, or invalid exact replay material.
#[cfg(test)]
pub(crate) fn project_terminal_response(
    response: ResponsesResponse,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_request: ResponsesProviderRequestBinding,
) -> Result<ResponsesTerminalProjection, ResponsesProjectError> {
    project_terminal_response_with_model_binding(
        response,
        replay,
        storage,
        provider_request,
        plakous_responses::ResponseModelBinding::ExactRequest,
    )
}

/// Projects a terminal response under an explicit provider-reported model
/// binding policy.
pub(crate) fn project_terminal_response_with_model_binding(
    response: ResponsesResponse,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_request: ResponsesProviderRequestBinding,
    model_binding: plakous_responses::ResponseModelBinding,
) -> Result<ResponsesTerminalProjection, ResponsesProjectError> {
    if model_binding == plakous_responses::ResponseModelBinding::ExactRequest
        && response.model() != replay.model.as_str()
    {
        return Err(ResponsesProjectError::InvalidTerminal(
            "response model differs from replay binding",
        ));
    }
    if replay.require_encrypted_reasoning {
        for item in response.output() {
            if let ResponsesOutputItem::Reasoning {
                status,
                encrypted_content,
                ..
            } = item
                && (encrypted_content.as_deref().is_none_or(str::is_empty)
                    || status.as_deref().is_some_and(|value| value != "completed")
                    || (replay.require_completed_reasoning_status
                        && status.as_deref() != Some("completed")))
            {
                return Err(ResponsesProjectError::InvalidTerminal(
                    "strict reasoning replay is incomplete or lacks encrypted content",
                ));
            }
        }
    }
    let usage = response.usage().map(|usage| {
        Usage::new(
            usage.input_tokens().unwrap_or(0),
            usage.output_tokens().unwrap_or(0),
            usage.reasoning_tokens().unwrap_or(0),
            usage.cached_input_tokens().unwrap_or(0),
            0,
        )
    });
    let finish = finish_details(&response)?;
    let artifact_candidate = if matches!(response.status(), ResponsesResponseStatus::Failed) {
        None
    } else {
        Some(build_artifact_candidate(
            &response,
            replay,
            storage,
            &provider_request,
        )?)
    };
    Ok(ResponsesTerminalProjection {
        response,
        finish,
        usage,
        provider_request,
        artifact_candidate,
    })
}

fn finish_details(
    response: &ResponsesResponse,
) -> Result<Option<FinishDetails>, ResponsesProjectError> {
    let reason = match response.status() {
        ResponsesResponseStatus::Completed => {
            if response.output().iter().any(|item| {
                matches!(
                    item,
                    ResponsesOutputItem::FunctionCall { .. }
                        | ResponsesOutputItem::CustomToolCall { .. }
                )
            }) {
                FinishReason::ToolCalls
            } else if response.output().iter().any(|item| {
                matches!(item, ResponsesOutputItem::Message { content, .. } if content.iter().any(|part| matches!(part, ResponsesContentPart::Refusal { .. })))
            }) {
                FinishReason::Refusal
            } else if response.end_turn() == Some(false) {
                FinishReason::AssistantContinuation
            } else {
                FinishReason::Stop
            }
        }
        ResponsesResponseStatus::Incomplete => match response
            .incomplete_details()
            .and_then(|details| details.reason())
        {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter" | "policy") => FinishReason::Policy,
            Some(_) => {
                return Err(ResponsesProjectError::InvalidTerminal(
                    "unknown incomplete reason",
                ));
            }
            None => {
                return Err(ResponsesProjectError::InvalidTerminal(
                    "incomplete response omitted its reason",
                ));
            }
        },
        ResponsesResponseStatus::Failed => return Ok(None),
        ResponsesResponseStatus::Queued
        | ResponsesResponseStatus::InProgress
        | ResponsesResponseStatus::Unknown(_) => {
            return Err(ResponsesProjectError::InvalidTerminal(
                "response status is not terminal",
            ));
        }
    };
    let detail = response
        .incomplete_details()
        .and_then(|details| details.reason())
        .map(str::to_owned);
    Ok(Some(FinishDetails::new(reason, detail)))
}

fn build_artifact_candidate(
    response: &ResponsesResponse,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_request: &ResponsesProviderRequestBinding,
) -> Result<ResponsesArtifactCandidateInput, ResponsesProjectError> {
    let covered =
        u32::try_from(response.output().len()).map_err(|_| ResponsesProjectError::OutputIndex)?;
    let mut candidates = Vec::with_capacity(response.output().len());
    for (output_index, item) in response.output().iter().enumerate() {
        let (purpose, kind, version) = match (item, replay.opaque_output_kind.as_ref()) {
            (ResponsesOutputItem::Opaque { .. }, Some((kind, version))) => {
                (ResponsesReplayPurpose::OpaqueOutputItem, kind, *version)
            }
            _ => (
                ResponsesReplayPurpose::OutputItem,
                &replay.output_item_kind,
                replay.version,
            ),
        };
        candidates.push(ResponsesReplayItem::new(
            purpose,
            u32::try_from(output_index).map_err(|_| ResponsesProjectError::OutputIndex)?,
            replay_envelope(item.native(), kind, version, replay)?,
        ));
    }
    Ok(ResponsesArtifactCandidateInput {
        storage,
        output_count: covered,
        response_id: response.id().to_owned(),
        replay_contract: replay.artifact_replay_contract()?,
        replay: candidates,
        physical_request: provider_request.physical_request(),
        terminal_response: provider_request.terminal_response(),
    })
}

fn replay_envelope(
    value: &serde_json::Value,
    kind: &ReplayKindId,
    version: u32,
    replay: &ResponsesReplayProjection,
) -> Result<ReplayEnvelope, ResponsesProjectError> {
    let raw = RawValue::from_string(serde_json::to_string(value)?)?;
    Ok(ReplayEnvelope::new(
        replay.protocol.clone(),
        replay.model_family.clone(),
        ReplayScope::ExactModel(replay.model.clone()),
        kind.clone(),
        version,
        DataClassification::Sensitive,
        raw,
    )?)
}

fn validate_exact_replay_pair(
    replay: &ReplayEnvelope,
    canonical: &ConversationItem,
) -> Result<(), ResponsesProjectError> {
    let matches = canonical
        .blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay()),
            ContentBlock::Reasoning(reasoning) => reasoning.replay(),
            _ => None,
        })
        .filter(|candidate| candidate.digest() == replay.digest())
        .count();
    if matches == 1 {
        Ok(())
    } else {
        Err(ResponsesProjectError::CanonicalPair(
            "canonical output lacks one exact native replay",
        ))
    }
}

pub(crate) fn validate_output_semantic_pair(
    raw: &ResponsesOutputItem,
    replay: &ReplayEnvelope,
    canonical: &ConversationItem,
    request: &ModelRequest,
) -> Result<(), ResponsesProjectError> {
    if canonical.actor() != Actor::Assistant || canonical.provenance() != Provenance::ModelOutput {
        return Err(ResponsesProjectError::CanonicalPair(
            "canonical output actor or provenance disagrees",
        ));
    }
    let semantic = canonical
        .blocks()
        .iter()
        .filter(|block| !is_exact_replay_only(block, replay))
        .collect::<Vec<_>>();
    match raw {
        ResponsesOutputItem::Message {
            content, native, ..
        } => {
            validate_message_semantics(content, message_phase(native)?, &semantic, canonical)?;
            Ok(())
        }
        ResponsesOutputItem::Reasoning {
            summary, content, ..
        } => {
            validate_reasoning_semantics(summary, content, &semantic)?;
            Ok(())
        }
        ResponsesOutputItem::Opaque { .. } => {
            if semantic.is_empty() {
                return Ok(());
            }
            Err(ResponsesProjectError::CanonicalPair(
                "opaque output invented canonical semantics",
            ))
        }
        ResponsesOutputItem::Compaction {
            encrypted_content, ..
        } => {
            let matches = matches!(
                semantic.as_slice(),
                [ContentBlock::Compaction(block)] if block.summary() == encrypted_content
            );
            if matches {
                Ok(())
            } else {
                Err(ResponsesProjectError::CanonicalPair(
                    "compaction output disagrees with canonical marker",
                ))
            }
        }
        ResponsesOutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            native,
            ..
        } => validate_call_semantics(
            RawToolCallKind::Function,
            call_id,
            name,
            arguments,
            native,
            &semantic,
            request,
        ),
        ResponsesOutputItem::CustomToolCall {
            call_id,
            name,
            input,
            native,
            ..
        } => validate_call_semantics(
            RawToolCallKind::Custom,
            call_id,
            name,
            input,
            native,
            &semantic,
            request,
        ),
    }
}

fn validate_call_semantics(
    expected_kind: RawToolCallKind,
    remote_id: &str,
    name: &str,
    arguments: &str,
    native: &serde_json::Value,
    semantic: &[&ContentBlock],
    request: &ModelRequest,
) -> Result<(), ResponsesProjectError> {
    let [ContentBlock::ToolCall(call)] = semantic else {
        return Err(ResponsesProjectError::CanonicalPair(
            "raw call must pair with exactly one canonical tool call",
        ));
    };
    let arguments_match = match expected_kind {
        RawToolCallKind::Function => crate::response_host::parse_unique_json_value(arguments)
            .is_ok_and(|value| value.is_object() && call.raw_arguments() == arguments),
        RawToolCallKind::Custom => {
            crate::response_host::parse_unique_json_value(call.raw_arguments())
                .is_ok_and(|value| value.as_str() == Some(arguments))
        }
    };
    if request_tool_kind(request, call.exposed_name().as_str()) != Some(expected_kind)
        || call.correlation().remote_call_id() != Some(remote_id)
        || call.exposed_name().as_str() != name
        || !arguments_match
        || !matches!(
            call.owner(),
            ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
        )
        || !raw_caller_matches(native, call.correlation().caller_lineage())
        || call.state() != ToolCallState::Requested
    {
        return Err(ResponsesProjectError::CanonicalPair(
            "raw call identity or execution semantics disagree",
        ));
    }
    Ok(())
}

fn raw_caller_matches(
    native: &serde_json::Value,
    canonical: &plakous_model::ToolCallerLineage,
) -> bool {
    match native.get("caller") {
        None | Some(serde_json::Value::Null) => {
            matches!(canonical, plakous_model::ToolCallerLineage::Direct)
        }
        Some(caller)
            if caller.get("type").and_then(serde_json::Value::as_str) == Some("direct") =>
        {
            matches!(canonical, plakous_model::ToolCallerLineage::Direct)
        }
        Some(caller)
            if caller.get("type").and_then(serde_json::Value::as_str) == Some("program") =>
        {
            caller.get("caller_id").and_then(serde_json::Value::as_str)
                == canonical.provider_programmatic_execution_id()
        }
        Some(_) => false,
    }
}

fn validate_message_semantics(
    content: &[ResponsesContentPart],
    phase: TextPhase,
    semantic: &[&ContentBlock],
    canonical: &ConversationItem,
) -> Result<(), ResponsesProjectError> {
    let mut actual = semantic.iter().copied();
    for part in content {
        match part {
            ResponsesContentPart::OutputText { text, annotations } => {
                if text.is_empty() && annotations.is_empty() {
                    continue;
                }
                let Some(ContentBlock::Text(block)) = actual.next() else {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "message text block order disagrees",
                    ));
                };
                if block.text() != text || block.phase() != Some(phase) {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "message text or phase disagrees",
                    ));
                }
                for annotation in annotations {
                    let Some(ContentBlock::Citation(citation)) = actual.next() else {
                        return Err(ResponsesProjectError::CanonicalPair(
                            "message citation order disagrees",
                        ));
                    };
                    validate_annotation_pair(annotation, citation, canonical)?;
                }
            }
            ResponsesContentPart::Refusal { refusal } => {
                if refusal.is_empty() {
                    continue;
                }
                let Some(ContentBlock::Refusal(block)) = actual.next() else {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "message refusal block order disagrees",
                    ));
                };
                if block.text() != refusal {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "message refusal text disagrees",
                    ));
                }
            }
            ResponsesContentPart::Opaque { .. } => {}
        }
    }
    if actual.next().is_some() {
        return Err(ResponsesProjectError::CanonicalPair(
            "message contains invented canonical blocks",
        ));
    }
    Ok(())
}

fn message_phase(native: &serde_json::Value) -> Result<TextPhase, ResponsesProjectError> {
    match native.get("phase").and_then(serde_json::Value::as_str) {
        None | Some("final" | "final_answer") => Ok(TextPhase::Final),
        Some("commentary") => Ok(TextPhase::Commentary),
        Some(_) => Err(ResponsesProjectError::CanonicalPair(
            "unknown message phase",
        )),
    }
}

fn validate_annotation_pair(
    annotation: &ResponsesAnnotation,
    citation: &plakous_model::CitationBlock,
    canonical: &ConversationItem,
) -> Result<(), ResponsesProjectError> {
    let ResponsesAnnotation::UrlCitation { .. } = annotation else {
        return Err(ResponsesProjectError::CanonicalPair(
            "opaque annotation gained canonical semantics",
        ));
    };
    let native = annotation.native();
    let start = native
        .get("start_index")
        .and_then(serde_json::Value::as_u64);
    let end = native.get("end_index").and_then(serde_json::Value::as_u64);
    let uri = native.get("url").and_then(serde_json::Value::as_str);
    let title = native.get("title").and_then(serde_json::Value::as_str);
    let range_matches = citation.range().is_some_and(|range| {
        Some(range.start()) == start
            && Some(range.end()) == end
            && range.unit() == CitationUnit::UnicodeScalar
    });
    if citation.target().item() != canonical.id()
        || citation.source().uri() != uri
        || citation.source().title() != title
        || !range_matches
    {
        return Err(ResponsesProjectError::CanonicalPair(
            "message citation semantics disagree",
        ));
    }
    Ok(())
}

fn validate_reasoning_semantics(
    summary: &[ResponsesReasoningSummaryPart],
    content: &[crate::ResponsesReasoningContentPart],
    semantic: &[&ContentBlock],
) -> Result<(), ResponsesProjectError> {
    let mut actual = semantic.iter().copied();
    for part in summary {
        match part {
            ResponsesReasoningSummaryPart::SummaryText(text) => {
                if text.is_empty() {
                    continue;
                }
                let Some(ContentBlock::Reasoning(block)) = actual.next() else {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "reasoning summary order disagrees",
                    ));
                };
                if block.visibility() != ReasoningVisibility::Summary || block.text() != Some(text)
                {
                    return Err(ResponsesProjectError::CanonicalPair(
                        "reasoning summary semantics disagree",
                    ));
                }
            }
            ResponsesReasoningSummaryPart::Opaque { .. } => {}
        }
    }
    for part in content {
        let crate::ResponsesReasoningContentPart::ReasoningText(text) = part;
        if text.is_empty() {
            continue;
        }
        let Some(ContentBlock::Reasoning(block)) = actual.next() else {
            return Err(ResponsesProjectError::CanonicalPair(
                "visible reasoning order disagrees",
            ));
        };
        if block.visibility() != ReasoningVisibility::Visible || block.text() != Some(text) {
            return Err(ResponsesProjectError::CanonicalPair(
                "visible reasoning semantics disagree",
            ));
        }
    }
    if actual.next().is_some() {
        return Err(ResponsesProjectError::CanonicalPair(
            "reasoning output invented canonical semantics",
        ));
    }
    Ok(())
}

fn is_exact_replay_only(block: &ContentBlock, replay: &ReplayEnvelope) -> bool {
    match block {
        ContentBlock::Opaque(opaque) => opaque.replay().digest() == replay.digest(),
        ContentBlock::Reasoning(reasoning) => {
            reasoning.visibility() == ReasoningVisibility::OpaqueReplay
                && reasoning
                    .replay()
                    .is_some_and(|candidate| candidate.digest() == replay.digest())
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RawToolCallKind {
    Function,
    Custom,
}

fn canonical_tool_kind(
    tools: &[plakous_model::ConfiguredTool],
    name: &str,
) -> Option<RawToolCallKind> {
    let mut matches = tools
        .iter()
        .filter(|tool| tool.definition().name().as_str() == name);
    let tool = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let is_custom = matches!(tool.definition().input(), ToolInputContract::Utf8(_));
    Some(if is_custom {
        RawToolCallKind::Custom
    } else {
        RawToolCallKind::Function
    })
}

fn request_tool_kind(request: &ModelRequest, name: &str) -> Option<RawToolCallKind> {
    if let Some(definition) = request.tool_projection().and_then(|projection| {
        projection
            .definitions()
            .iter()
            .find(|definition| definition.exposed_name().as_str() == name)
    }) {
        return Some(
            if matches!(
                definition.presentation(),
                plakous_model::ProjectedToolPresentationV2::CustomUtf8 { .. }
            ) {
                RawToolCallKind::Custom
            } else {
                RawToolCallKind::Function
            },
        );
    }
    canonical_tool_kind(request.tools(), name)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "unit fixtures use focused expectations and exhaustive mutation tables"
)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::response_host::{
        ResponsesHostDecodeError, ResponsesResponseLimits, ResponsesUsagePolicy, parse_output_item,
    };
    use plakous_model::{
        Actor, AttemptId, CallId, Capability, CapabilityConstraints, CapabilitySet, EntryId,
        FactProvenance, FactSource, FeatureSupport, McpServerId, ModelAdapterProfileId,
        ModelResponse, NamespaceId, OpaqueBlock, OpaqueFallback, Provenance, ReasoningBlock,
        RequestMetadata, ResolvedModelDescriptor, RouteId, RunId, Support, TextBlock, Timestamp,
        ToolConcurrency, ToolCorrelation, ToolEffect, ToolInputContract, ToolName,
        ToolOutputContract, ToolResultBlock, ToolResultKind, ToolResultPart, TurnId,
        ValidationMode,
    };
    use plakous_responses::{MAX_RESPONSES_ARTIFACT_BYTES, ResponsesSemanticRevisions};
    use serde_json::json;
    use uuid::Uuid;

    fn replay() -> ResponsesReplayProjection {
        let Ok(protocol) = ProtocolId::new("openai.responses") else {
            panic!("valid protocol test fixture");
        };
        let Ok(model) = ModelId::new("gpt-test") else {
            panic!("valid model test fixture");
        };
        let Ok(output_kind) = ReplayKindId::new("responses.output_item") else {
            panic!("valid output kind test fixture");
        };
        let Ok(replay) = ResponsesReplayProjection::new(protocol, None, model, output_kind, 1)
        else {
            panic!("valid replay test fixture");
        };
        replay
    }

    fn provider_request() -> ResponsesProviderRequestBinding {
        ResponsesProviderRequestBinding::new(
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"project-host-physical-request"],
            )
            .expect("physical request digest"),
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"project-host-terminal-response"],
            )
            .expect("terminal response digest"),
            None,
        )
        .expect("provider request binding")
    }

    #[test]
    fn strict_reasoning_projection_requires_completed_nonempty_encrypted_content() {
        let strict = replay().requiring_encrypted_reasoning();
        assert_ne!(
            replay().execution_identity().expect("ordinary identity"),
            strict.execution_identity().expect("strict identity")
        );
        for reasoning in [
            json!({"id":"r1","type":"reasoning","status":"completed","summary":[]}),
            json!({
                "id":"r1","type":"reasoning","status":"completed","summary":[],
                "encrypted_content":""
            }),
            json!({
                "id":"r1","type":"reasoning","status":"in_progress","summary":[],
                "encrypted_content":"opaque"
            }),
        ] {
            let response = ResponsesResponse::parse(
                &json!({
                    "id":"resp-strict","object":"response","status":"completed",
                    "model":"gpt-test","output":[reasoning]
                }),
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .expect("response");
            assert!(matches!(
                project_terminal_response(
                    response,
                    &strict,
                    ResponsesStorageMode::Stateless,
                    provider_request(),
                ),
                Err(ResponsesProjectError::InvalidTerminal(
                    "strict reasoning replay is incomplete or lacks encrypted content"
                ))
            ));
        }

        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp-strict","object":"response","status":"completed",
                "model":"gpt-test",
                "output":[{
                    "id":"r1","type":"reasoning","status":"completed","summary":[],
                    "encrypted_content":"opaque"
                }]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("response");
        assert!(
            project_terminal_response(
                response,
                &strict,
                ResponsesStorageMode::Stateless,
                provider_request(),
            )
            .is_ok()
        );
    }

    #[test]
    fn codex_reasoning_projection_requires_encrypted_content_with_optional_status() {
        let codex = replay().requiring_encrypted_reasoning_with_optional_status();
        for (reasoning, accepted) in [
            (
                json!({
                    "id":"r1","type":"reasoning","summary":[],
                    "encrypted_content":"opaque"
                }),
                true,
            ),
            (
                json!({
                    "id":"r1","type":"reasoning","status":"completed","summary":[],
                    "encrypted_content":"opaque"
                }),
                true,
            ),
            (json!({"id":"r1","type":"reasoning","summary":[]}), false),
            (
                json!({
                    "id":"r1","type":"reasoning","status":"in_progress","summary":[],
                    "encrypted_content":"opaque"
                }),
                false,
            ),
        ] {
            let response = ResponsesResponse::parse(
                &json!({
                    "id":"resp-codex","object":"response","status":"completed",
                    "model":"gpt-test","output":[reasoning]
                }),
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .expect("response");
            assert_eq!(
                project_terminal_response(
                    response,
                    &codex,
                    ResponsesStorageMode::Stateless,
                    provider_request(),
                )
                .is_ok(),
                accepted
            );
        }
    }

    fn request_with_tool(custom: bool) -> ModelRequest {
        let input = if custom {
            ToolInputContract::utf8(65_536).expect("UTF-8 contract")
        } else {
            ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false})).expect("schema")
        };
        crate::test_support::model_request_builder(
            plakous_model::ConversationView::default(),
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1)).expect("run"),
                TurnId::try_from(Uuid::from_u128(2)).expect("turn"),
                AttemptId::try_from(Uuid::from_u128(3)).expect("attempt"),
                Timestamp::new(1_700_000_000, 0).expect("time"),
            ),
        )
        .resolved_harness_plan_identity(
            Digest::sha256(
                DigestDomain::ResolvedHarnessPlan,
                1,
                &[b"project-host-harness-plan"],
            )
            .expect("harness plan identity"),
        )
        .tools(vec![plakous_model::ConfiguredTool::new(
            plakous_model::ToolDefinition::new(
                ToolName::new("lookup").expect("tool"),
                "1.0.0",
                "lookup",
                input,
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )
            .expect("definition"),
            plakous_model::ToolExecutionDisposition::Local,
            plakous_model::ToolCallerDisposition::DirectOnly,
        )])
        .build()
        .expect("request")
    }

    #[test]
    fn projected_function_kind_uses_exposed_request_authority() {
        let base = request_with_tool(true);
        let schema = plakous_model::PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"command":{"type":"string"}},
            "required":["command"],
            "additionalProperties":false
        }))
        .expect("schema");
        let identity = Digest::sha256(DigestDomain::ToolDefinition, 2, &[b"projected-function"])
            .expect("identity");
        let target = plakous_model::ToolProjectionTargetV2::new(
            plakous_model::IntegrationClass::Standard,
            "xai.responses.grok-subscription.2026-07",
            "xai.grok_subscription",
            "xai.grok.compat.2026-07-31.r1",
            "responses-family",
            "grok-4.5",
            "grok-4.5",
            base.effective_provider_configuration_id(),
            4,
        )
        .expect("target");
        let canonical = base.tools()[0].definition().name().clone();
        let projection = plakous_model::CompiledToolProjectionV2::new(
            target,
            identity,
            identity,
            vec![
                plakous_model::ProjectedToolDefinitionV2::new(
                    canonical.clone(),
                    vec![canonical],
                    ToolName::new("run_terminal_cmd").expect("exposed name"),
                    "projected function",
                    schema.clone(),
                    schema,
                    identity,
                )
                .expect("definition"),
            ],
        )
        .expect("projection");
        let prefix = base
            .stable_prefix()
            .with_tool_projection(projection)
            .expect("prefix");
        let request = ModelRequest::from_parts(prefix, base.input().clone(), base.metadata())
            .expect("request");
        assert!(matches!(
            request_tool_kind(&request, "run_terminal_cmd"),
            Some(RawToolCallKind::Function)
        ));
    }

    #[test]
    fn projected_custom_kind_uses_explicit_presentation_not_schema_inference() {
        let base = request_with_tool(true);
        let output = plakous_model::PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object", "properties":{}, "required":[],
            "additionalProperties":false
        }))
        .expect("schema");
        let identity = Digest::sha256(DigestDomain::ToolDefinition, 3, &[b"projected-custom"])
            .expect("identity");
        let target = plakous_model::ToolProjectionTargetV2::new(
            plakous_model::IntegrationClass::Standard,
            "openai.codex.responses",
            "openai.codex_subscription",
            "openai.codex.compat.r1",
            "responses-family",
            "gpt-codex",
            "gpt-codex",
            base.effective_provider_configuration_id(),
            5,
        )
        .expect("target");
        let canonical = base.tools()[0].definition().name().clone();
        let projection = plakous_model::CompiledToolProjectionV2::new(
            target,
            identity,
            identity,
            vec![
                plakous_model::ProjectedToolDefinitionV2::new_custom(
                    canonical.clone(),
                    vec![canonical],
                    ToolName::new("exec").expect("exposed name"),
                    "execute code",
                    plakous_model::Utf8TextContractV1::new(65_536).expect("bound"),
                    plakous_model::ProjectedCustomToolFormatV2::lark("start: /(?s).+/")
                        .expect("grammar"),
                    output,
                    identity,
                )
                .expect("definition"),
            ],
        )
        .expect("projection");
        let prefix = base
            .stable_prefix()
            .with_tool_projection(projection)
            .expect("prefix");
        let request = ModelRequest::from_parts(prefix, base.input().clone(), base.metadata())
            .expect("request");
        assert!(matches!(
            request_tool_kind(&request, "exec"),
            Some(RawToolCallKind::Custom)
        ));
    }

    fn descriptor() -> ResolvedModelDescriptor {
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0).expect("time"),
            None,
        );
        let mut capabilities = BTreeMap::new();
        for capability in [Capability::Streaming, Capability::Tools] {
            capabilities.insert(
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
        }
        ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("openai.responses").expect("protocol"),
            RouteId::new("responses-test").expect("route"),
            ModelId::new("gpt-test").expect("model"),
            None,
            None,
            ModelAdapterProfileId::new("responses-test-adapter").expect("adapter"),
            CapabilitySet::new(capabilities, BTreeMap::new()),
        )
    }

    fn admitted_plan(request: &ModelRequest) -> ValidatedRequestPlan {
        descriptor()
            .validate_request(request.clone(), ValidationMode::Strict)
            .expect("admitted plan")
    }

    fn artifact_binding(plan: &ValidatedRequestPlan) -> ResponsesArtifactBinding {
        ResponsesArtifactBinding::from_admitted_operation(
            &descriptor(),
            plan,
            plan.request(),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"project-host-account"])
                .expect("account binding"),
            ResponsesSemanticRevisions::new(1, 1, 3).expect("semantic revisions"),
            Some(
                Digest::sha256(
                    DigestDomain::CacheAffinity,
                    1,
                    &[b"project-host-cache-affinity"],
                )
                .expect("cache affinity"),
            ),
        )
        .expect("artifact binding")
    }

    fn sealed_artifact_fixture() -> (
        ResponsesSealedArtifactCandidate,
        ValidatedRequestPlan,
        CanonicalItemPrefixIdentity,
        Vec<ConversationItem>,
    ) {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp_neutral_secret","object":"response","status":"completed",
                "model":"gpt-test",
                "output":[{
                    "id":"m1","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"sealed-secret","annotations":[]}]
                }]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("response");
        let projected = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::ProviderStored,
            provider_request(),
        )
        .expect("projection");
        let output_replay = projected.artifact_candidate().expect("candidate").replay()[0]
            .envelope()
            .clone();
        let canonical = item(
            75,
            vec![
                replay_block(output_replay),
                ContentBlock::Text(
                    TextBlock::new("sealed-secret", Some(TextPhase::Final)).expect("text"),
                ),
            ],
        );
        let request = request_with_tool(false);
        let plan = admitted_plan(&request);
        let paired = projected
            .pair_canonical(&request, vec![canonical.clone()])
            .expect("paired");
        let (_, _, _, Some(candidate)) = paired.into_parts() else {
            panic!("paired artifact candidate");
        };
        let covered = plakous_model::ConversationView::from_items(vec![canonical.clone()])
            .expect("test conversation items form one valid segment")
            .canonical_item_prefix_identity(1)
            .expect("covered output");
        (
            candidate.seal(&plan, covered).expect("sealed candidate"),
            plan,
            covered,
            vec![canonical],
        )
    }

    fn item(value: u128, blocks: Vec<ContentBlock>) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(value)).expect("entry"),
            Actor::Assistant,
            blocks,
            Provenance::ModelOutput,
        )
        .expect("item")
    }

    fn raw_item(value: serde_json::Value) -> ResponsesOutputItem {
        parse_output_item::<ResponsesHostDecodeError>(&value).expect("raw item")
    }

    fn envelope(value: &serde_json::Value) -> ReplayEnvelope {
        replay_envelope(
            value,
            &ReplayKindId::new("responses.output_item").expect("kind"),
            1,
            &replay(),
        )
        .expect("envelope")
    }

    fn replay_block(replay: ReplayEnvelope) -> ContentBlock {
        ContentBlock::Opaque(
            OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay).expect("opaque replay"),
        )
    }

    fn call_block(
        owner: ToolExecutionOwner,
        state: ToolCallState,
        remote_id: &str,
        name: &str,
        arguments: &str,
    ) -> ContentBlock {
        ContentBlock::ToolCall(
            plakous_model::ToolCallBlock::new(
                CallId::try_from(Uuid::from_u128(10)).expect("call"),
                ToolName::new(name).expect("tool"),
                owner,
                state,
                ToolCorrelation::new(
                    Some(remote_id.to_owned()),
                    None,
                    plakous_model::ToolCallerLineage::Direct,
                )
                .expect("correlation"),
                arguments,
            )
            .expect("tool call"),
        )
    }

    #[test]
    fn replay_projection_matches_only_its_exact_target() {
        let projection = replay();
        let protocol = ProtocolId::new("openai.responses").expect("protocol");
        let model = ModelId::new("gpt-test").expect("model");
        assert!(projection.matches_target(&protocol, None, &model));

        let other_protocol = ProtocolId::new("xai.responses").expect("other protocol");
        let other_model = ModelId::new("grok-test").expect("other model");
        let family = ModelFamilyId::new("gpt-family").expect("family");
        assert!(!projection.matches_target(&other_protocol, None, &model));
        assert!(!projection.matches_target(&protocol, None, &other_model));
        assert!(!projection.matches_target(&protocol, Some(&family), &model));

        let ambiguous = projection
            .with_opaque_output_kind(
                ReplayKindId::new("responses.output_item").expect("same kind"),
                2,
            )
            .expect("projection shape");
        assert!(ambiguous.artifact_replay_contract().is_err());
    }

    /// The raw replay item pairs with the name the model called: a call whose
    /// canonical name differs replays under its exposed name.
    #[test]
    fn raw_call_pairing_binds_the_exposed_name_not_the_canonical_name() {
        let native = json!({
            "id":"c1","type":"function_call","status":"completed",
            "call_id":"remote-1","name":"lookup","arguments":"{}"
        });
        let raw = raw_item(native.clone());
        let replay = envelope(&native);
        let request = request_with_tool(false);
        let aliased = |exposed: &str, canonical: &str| {
            let ContentBlock::ToolCall(call) = call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-1",
                exposed,
                "{}",
            ) else {
                unreachable!("call_block builds a tool call");
            };
            ContentBlock::ToolCall(
                call.with_canonical_name(ToolName::new(canonical).expect("canonical")),
            )
        };
        let paired = item(
            11,
            vec![replay_block(replay.clone()), aliased("lookup", "search")],
        );
        assert!(validate_output_semantic_pair(&raw, &replay, &paired, &request).is_ok());
        let canonical_only = item(
            12,
            vec![replay_block(replay.clone()), aliased("search", "lookup")],
        );
        assert!(validate_output_semantic_pair(&raw, &replay, &canonical_only, &request).is_err());
    }

    #[test]
    fn raw_call_pairing_accepts_mcp_and_rejects_every_execution_mutation() {
        let native = json!({
            "id":"c1","type":"function_call","status":"completed",
            "call_id":"remote-1","name":"lookup","arguments":"{}"
        });
        let raw = raw_item(native.clone());
        let replay = envelope(&native);
        let request = request_with_tool(false);
        let mcp = ToolExecutionOwner::Mcp {
            server: McpServerId::new("test-mcp").expect("server"),
        };
        let canonical = item(
            11,
            vec![
                replay_block(replay.clone()),
                call_block(mcp, ToolCallState::Requested, "remote-1", "lookup", "{}"),
            ],
        );
        assert!(validate_output_semantic_pair(&raw, &replay, &canonical, &request).is_ok());

        let mutations = [
            call_block(
                ToolExecutionOwner::ProviderHosted {
                    provider_kind: NamespaceId::new("provider.lookup").expect("kind"),
                },
                ToolCallState::Requested,
                "remote-1",
                "lookup",
                "{}",
            ),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Pending,
                "remote-1",
                "lookup",
                "{}",
            ),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "wrong-id",
                "lookup",
                "{}",
            ),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-1",
                "other",
                "{}",
            ),
            call_block(
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                "remote-1",
                "lookup",
                "{\"wrong\":true}",
            ),
        ];
        for (index, mutation) in mutations.into_iter().enumerate() {
            let candidate = item(
                20 + u128::try_from(index).expect("index"),
                vec![replay_block(replay.clone()), mutation],
            );
            assert!(
                validate_output_semantic_pair(&raw, &replay, &candidate, &request).is_err(),
                "call mutation {index} was accepted"
            );
        }
        let extra = item(
            30,
            vec![
                replay_block(replay.clone()),
                call_block(
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    "remote-1",
                    "lookup",
                    "{}",
                ),
                ContentBlock::Text(
                    TextBlock::new("invented", Some(TextPhase::Final)).expect("text"),
                ),
            ],
        );
        assert!(validate_output_semantic_pair(&raw, &replay, &extra, &request).is_err());

        let extra_result = item(
            31,
            vec![
                replay_block(replay.clone()),
                call_block(
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    "remote-1",
                    "lookup",
                    "{}",
                ),
                ContentBlock::ToolResult(
                    ToolResultBlock::new(
                        CallId::try_from(Uuid::from_u128(10)).expect("call"),
                        ToolExecutionOwner::Local,
                        ToolResultKind::Success,
                        vec![ToolResultPart::Text(
                            TextBlock::new("result", None).expect("text"),
                        )],
                        ToolCorrelation::new(
                            Some("remote-1".to_owned()),
                            None,
                            plakous_model::ToolCallerLineage::Direct,
                        )
                        .expect("correlation"),
                    )
                    .expect("result"),
                ),
            ],
        );
        assert!(validate_output_semantic_pair(&raw, &replay, &extra_result, &request).is_err());

        let custom_native = json!({
            "id":"c2","type":"custom_tool_call","status":"completed",
            "call_id":"remote-2","name":"lookup","input":"plain text"
        });
        let custom = raw_item(custom_native.clone());
        let custom_replay = envelope(&custom_native);
        let custom_canonical = item(
            32,
            vec![
                replay_block(custom_replay.clone()),
                call_block(
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    "remote-2",
                    "lookup",
                    "\"plain text\"",
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(
                &custom,
                &custom_replay,
                &custom_canonical,
                &request_with_tool(true),
            )
            .is_ok()
        );
        assert!(
            validate_output_semantic_pair(&custom, &custom_replay, &custom_canonical, &request,)
                .is_err()
        );

        let empty_native = json!({
            "id":"c3","type":"custom_tool_call","status":"completed",
            "call_id":"remote-3","name":"lookup","input":""
        });
        let empty = raw_item(empty_native.clone());
        let empty_replay = envelope(&empty_native);
        let empty_canonical = item(
            33,
            vec![
                replay_block(empty_replay.clone()),
                call_block(
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    "remote-3",
                    "lookup",
                    "\"\"",
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(
                &empty,
                &empty_replay,
                &empty_canonical,
                &request_with_tool(true),
            )
            .is_ok()
        );

        for invalid in ["", "not json", "{\"key\":1,\"key\":2}", "[]"] {
            let invalid_native = json!({
                "id":"f-invalid","type":"function_call","status":"completed",
                "call_id":"remote-invalid","name":"lookup","arguments":invalid
            });
            let invalid_raw = raw_item(invalid_native.clone());
            let invalid_replay = envelope(&invalid_native);
            let canonical_arguments = if invalid.is_empty() { "null" } else { invalid };
            let invalid_canonical = item(
                34,
                vec![
                    replay_block(invalid_replay.clone()),
                    call_block(
                        ToolExecutionOwner::Local,
                        ToolCallState::Requested,
                        "remote-invalid",
                        "lookup",
                        canonical_arguments,
                    ),
                ],
            );
            assert!(
                validate_output_semantic_pair(
                    &invalid_raw,
                    &invalid_replay,
                    &invalid_canonical,
                    &request,
                )
                .is_err(),
                "invalid function arguments were accepted: {invalid:?}"
            );
        }
    }

    #[test]
    fn message_reasoning_and_opaque_semantics_are_exact() {
        let request = request_with_tool(false);
        let message_native = json!({
            "id":"m1","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"exact","annotations":[]}]
        });
        let message = raw_item(message_native.clone());
        let message_replay = envelope(&message_native);
        let exact_message = item(
            40,
            vec![
                replay_block(message_replay.clone()),
                ContentBlock::Text(TextBlock::new("exact", Some(TextPhase::Final)).expect("text")),
            ],
        );
        assert!(
            validate_output_semantic_pair(&message, &message_replay, &exact_message, &request)
                .is_ok()
        );
        for (index, block) in [
            ContentBlock::Text(TextBlock::new("tampered", Some(TextPhase::Final)).expect("text")),
            ContentBlock::Text(TextBlock::new("exact", Some(TextPhase::Commentary)).expect("text")),
        ]
        .into_iter()
        .enumerate()
        {
            let candidate = item(
                41 + u128::try_from(index).expect("index"),
                vec![replay_block(message_replay.clone()), block],
            );
            assert!(
                validate_output_semantic_pair(&message, &message_replay, &candidate, &request)
                    .is_err()
            );
        }

        let commentary_native = json!({
            "id":"m2","type":"message","role":"assistant","status":"completed",
            "phase":"commentary",
            "content":[{"type":"output_text","text":"working","annotations":[]}]
        });
        let commentary = raw_item(commentary_native.clone());
        let commentary_replay = envelope(&commentary_native);
        let exact_commentary = item(
            43,
            vec![
                replay_block(commentary_replay.clone()),
                ContentBlock::Text(
                    TextBlock::new("working", Some(TextPhase::Commentary)).expect("commentary"),
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(
                &commentary,
                &commentary_replay,
                &exact_commentary,
                &request,
            )
            .is_ok()
        );

        for (actor, provenance) in [
            (Actor::User, Provenance::ModelOutput),
            (Actor::Assistant, Provenance::Imported),
        ] {
            let invalid_authority = ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(44)).expect("entry"),
                actor,
                exact_commentary.blocks().to_vec(),
                provenance,
            )
            .expect("structurally valid item");
            assert!(
                validate_output_semantic_pair(
                    &commentary,
                    &commentary_replay,
                    &invalid_authority,
                    &request,
                )
                .is_err()
            );
        }

        let reasoning_native = json!({
            "id":"r1","type":"reasoning","status":"completed",
            "summary":[{"type":"summary_text","text":"summary"}],
            "encrypted_content":"opaque"
        });
        let reasoning = raw_item(reasoning_native.clone());
        let reasoning_replay = envelope(&reasoning_native);
        let exact_reasoning = item(
            50,
            vec![
                replay_block(reasoning_replay.clone()),
                ContentBlock::Reasoning(
                    ReasoningBlock::new(
                        ReasoningVisibility::Summary,
                        Some("summary".to_owned()),
                        None,
                    )
                    .expect("reasoning"),
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(
                &reasoning,
                &reasoning_replay,
                &exact_reasoning,
                &request
            )
            .is_ok()
        );
        let visible = item(
            51,
            vec![
                replay_block(reasoning_replay.clone()),
                ContentBlock::Reasoning(
                    ReasoningBlock::new(
                        ReasoningVisibility::Visible,
                        Some("summary".to_owned()),
                        None,
                    )
                    .expect("reasoning"),
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(&reasoning, &reasoning_replay, &visible, &request)
                .is_err()
        );

        let opaque_native = json!({"id":"u1","type":"future_item","status":"completed"});
        let opaque = raw_item(opaque_native.clone());
        let opaque_replay = envelope(&opaque_native);
        let invented = item(
            60,
            vec![
                replay_block(opaque_replay.clone()),
                ContentBlock::Text(
                    TextBlock::new("invented", Some(TextPhase::Final)).expect("text"),
                ),
            ],
        );
        assert!(
            validate_output_semantic_pair(&opaque, &opaque_replay, &invented, &request).is_err()
        );
    }

    #[test]
    fn pair_canonical_requires_exact_replay_before_atomic_promotion() {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp_pair","object":"response","status":"completed","model":"gpt-test",
                "output":[{"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"exact","annotations":[]}]}]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("response");
        let projected = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::Stateless,
            provider_request(),
        )
        .expect("projection");
        let replay = projected.artifact_candidate().expect("candidate").replay()[0]
            .envelope()
            .clone();
        let canonical = item(
            70,
            vec![
                replay_block(replay),
                ContentBlock::Text(TextBlock::new("exact", Some(TextPhase::Final)).expect("text")),
            ],
        );
        assert!(
            projected
                .clone()
                .pair_canonical(&request_with_tool(false), vec![canonical])
                .is_ok()
        );
        let missing_replay = item(
            71,
            vec![ContentBlock::Text(
                TextBlock::new("exact", Some(TextPhase::Final)).expect("text"),
            )],
        );
        assert!(
            projected
                .pair_canonical(&request_with_tool(false), vec![missing_replay])
                .is_err()
        );
    }

    #[test]
    fn artifact_seal_requires_runtime_derived_exact_coverage() {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp_seal","object":"response","status":"completed","model":"gpt-test",
                "output":[{"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"sealed","annotations":[]}]}]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("response");
        let projected = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::ProviderStored,
            provider_request(),
        )
        .expect("projection");
        let replay = projected.artifact_candidate().expect("candidate").replay()[0]
            .envelope()
            .clone();
        let canonical = item(
            72,
            vec![
                replay_block(replay),
                ContentBlock::Text(TextBlock::new("sealed", Some(TextPhase::Final)).expect("text")),
            ],
        );
        let request = request_with_tool(false);
        let plan = admitted_plan(&request);
        let paired = projected
            .pair_canonical(&request, vec![canonical.clone()])
            .expect("paired");
        let (_, _, _, Some(candidate)) = paired.into_parts() else {
            panic!("paired artifact candidate");
        };
        let wrong_coverage = plakous_model::ConversationView::default()
            .canonical_item_prefix_identity(0)
            .expect("empty coverage");
        assert!(matches!(
            candidate.clone().seal(&plan, wrong_coverage),
            Err(ResponsesProjectError::InvalidArtifactCoverage)
        ));

        let covered = plakous_model::ConversationView::from_items(vec![canonical])
            .expect("test conversation items form one valid segment")
            .canonical_item_prefix_identity(1)
            .expect("covered output");
        let sealed = candidate.seal(&plan, covered).expect("sealed candidate");
        let (_, production, contract, replay, continuation, storage) = sealed.into_parts();
        assert_eq!(production.output_items(), 1);
        assert_eq!(production.producing_projection_items(), 0);
        assert_eq!(production.covered_projection_items(), 1);
        assert_eq!(production.covered_projection(), covered.identity());
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].output_index(), 0);
        assert_eq!(storage, ResponsesStorageMode::ProviderStored);
        let continuation = continuation.expect("provider continuation");
        assert_eq!(continuation.response_id(), "resp_seal");
        assert_eq!(continuation.covered_projection_items(), 1);
        assert_eq!(continuation.covered_projection(), covered.identity());
        assert_eq!(
            continuation.terminal_response(),
            production.terminal_response()
        );
        assert_eq!(
            contract
                .for_purpose(ResponsesReplayPurpose::OutputItem)
                .map(|kind| (kind.kind().as_str(), kind.version())),
            Some(("responses.output_item", 1))
        );
    }

    #[test]
    fn neutral_candidate_is_exact_v3_and_validates_every_producer_fact() {
        let (sealed, plan, covered, output) = sealed_artifact_fixture();
        let binding = artifact_binding(&plan);
        let now = Timestamp::new(1_700_000_100, 0).expect("time");
        let candidate =
            seal_responses_family_artifact(sealed, &plan, covered, binding.clone(), now)
                .expect("neutral candidate");

        assert_eq!(candidate.family(), "responses");
        assert_eq!(
            candidate.schema_revision(),
            RESPONSES_ARTIFACT_SCHEMA_REVISION
        );
        assert_eq!(
            candidate.codec_revision(),
            RESPONSES_ARTIFACT_CODEC_REVISION
        );
        assert_eq!(candidate.binding(), binding.identity());
        assert!(candidate.bytes().len() <= MAX_RESPONSES_ARTIFACT_BYTES);
        candidate
            .validate_for_commit(&plan, covered, output.len())
            .expect("exact producer validation");

        let artifact =
            ResponsesArtifactCodec::decode_inert(candidate.bytes()).expect("inert family decode");
        let producer = candidate.producer();
        assert_eq!(producer.request(), artifact.production().producer_request());
        assert_eq!(producer.model_input(), artifact.binding().model_input());
        assert_eq!(producer.plan(), artifact.production().validated_plan());
        assert_eq!(
            producer.cache_affinity(),
            artifact.binding().cache_affinity()
        );
        assert_eq!(producer.run(), artifact.binding().run());
        assert_eq!(producer.turn(), artifact.binding().turn());
        assert_eq!(producer.attempt(), artifact.binding().attempt());
        assert_eq!(
            producer.source_projection(),
            artifact.production().producing_projection()
        );
        assert_eq!(
            producer.source_projection_items(),
            artifact.production().producing_projection_items()
        );
        assert_eq!(
            producer.output_items(),
            artifact.production().output_items()
        );
        assert_eq!(
            producer.covered_projection(),
            artifact.production().covered_projection()
        );
        assert_eq!(
            producer.covered_projection_items(),
            artifact.production().covered_projection_items()
        );
        assert_eq!(
            producer.physical_operation(),
            artifact.production().physical_request()
        );
        assert_eq!(
            producer.terminal_operation(),
            artifact.production().terminal_response()
        );
        assert_eq!(
            artifact.schema_revision(),
            RESPONSES_ARTIFACT_SCHEMA_REVISION
        );
        artifact
            .validate_for(&binding, artifact.production(), now)
            .expect("exact family producer validation");

        let debug = format!("{candidate:?}");
        assert!(debug.contains("<redacted;"));
        assert!(!debug.contains("resp_neutral_secret"));
        assert!(!debug.contains("sealed-secret"));
    }

    #[test]
    fn neutral_candidate_rejects_foreign_facts_and_corruption() {
        let (sealed, plan, covered, output) = sealed_artifact_fixture();
        let binding = artifact_binding(&plan);
        let now = Timestamp::new(1_700_000_100, 0).expect("time");

        let foreign_plan = admitted_plan(&request_with_tool(true));
        assert!(matches!(
            seal_responses_family_artifact(
                sealed.clone(),
                &foreign_plan,
                covered,
                binding.clone(),
                now,
            ),
            Err(ResponsesProjectError::FamilyArtifact(
                plakous_model::FamilyArtifactError::ForeignProducerFacts
            ))
        ));

        let foreign_covered = plakous_model::ConversationView::from_items(vec![item(
            76,
            vec![ContentBlock::Text(
                TextBlock::new("foreign", Some(TextPhase::Final)).expect("text"),
            )],
        )])
        .expect("test conversation items form one valid segment")
        .canonical_item_prefix_identity(1)
        .expect("foreign covered output");
        assert!(matches!(
            seal_responses_family_artifact(
                sealed.clone(),
                &plan,
                foreign_covered,
                binding.clone(),
                now,
            ),
            Err(ResponsesProjectError::FamilyArtifact(
                plakous_model::FamilyArtifactError::ForeignProducerFacts
            ))
        ));

        let candidate = seal_responses_family_artifact(sealed, &plan, covered, binding, now)
            .expect("neutral candidate");
        assert!(matches!(
            candidate.validate_for_commit(&plan, covered, output.len() + 1),
            Err(plakous_model::FamilyArtifactError::ResponseOutputMismatch)
        ));
        let mut corrupted = candidate.bytes().to_vec();
        let last = corrupted.last_mut().expect("encoded artifact byte");
        *last ^= 1;
        assert!(matches!(
            ResponsesArtifactCodec::decode_inert(&corrupted),
            Err(ResponsesArtifactError::IntegrityMismatch)
        ));
    }

    #[test]
    fn neutral_artifact_bytes_never_enter_response_replay_or_canonical_output() {
        let (sealed, plan, covered, output) = sealed_artifact_fixture();
        let candidate = seal_responses_family_artifact(
            sealed,
            &plan,
            covered,
            artifact_binding(&plan),
            Timestamp::new(1_700_000_100, 0).expect("time"),
        )
        .expect("neutral candidate");
        assert!(candidate.bytes().starts_with(b"PLAKRSP\0"));

        let response = ModelResponse::new(
            output,
            FinishDetails::new(FinishReason::Stop, None),
            None,
            Some("resp_neutral_secret".to_owned()),
            plan.digest(),
        )
        .with_family_artifact_candidate(candidate)
        .expect("sidecar attachment");
        assert!(
            !serde_json::to_value(&response)
                .expect("response JSON")
                .as_object()
                .is_some_and(|object| object.contains_key("replay"))
        );
        let canonical = serde_json::to_vec(response.output()).expect("canonical JSON");
        assert!(
            !canonical
                .windows(b"PLAKRSP".len())
                .any(|part| part == b"PLAKRSP")
        );
        assert!(
            !canonical
                .windows(b"responses.artifact".len())
                .any(|part| { part == b"responses.artifact" })
        );
        assert!(
            !canonical
                .windows(b"\"encoding\":\"base64\"".len())
                .any(|part| { part == b"\"encoding\":\"base64\"" })
        );
    }

    #[test]
    fn explicit_end_turn_false_projects_continuation_with_tool_and_refusal_precedence() {
        let text = json!({"id":"m1","type":"message","role":"assistant", "status":"completed",
            "content":[{"type":"output_text","text":"continuing","annotations":[]}]});
        for (signal, expected) in [
            (None, FinishReason::Stop),
            (Some(json!(null)), FinishReason::Stop),
            (Some(json!(true)), FinishReason::Stop),
            (Some(json!(false)), FinishReason::AssistantContinuation),
        ] {
            let mut value = json!({"id":"resp_1","object":"response","status":"completed",
                "model":"gpt-test","output":[text.clone()],
                "usage":{"input_tokens":7,"output_tokens":5}});
            if let Some(signal) = signal {
                value["end_turn"] = signal;
            }
            let response = ResponsesResponse::parse(
                &value,
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .expect("valid terminal response");
            let projected = project_terminal_response(
                response,
                &replay(),
                ResponsesStorageMode::ProviderStored,
                provider_request(),
            )
            .expect("projected response");
            assert_eq!(
                projected.finish().map(FinishDetails::reason),
                Some(expected)
            );
            assert_eq!(projected.usage(), Some(Usage::new(7, 5, 0, 0, 0)));
        }

        for (output, expected) in [
            (json!([]), FinishReason::AssistantContinuation),
            (
                json!([{"id":"c1","type":"function_call","status":"completed",
                "call_id":"call1","name":"test","arguments":"{}"}]),
                FinishReason::ToolCalls,
            ),
            (
                json!([{"id":"m1","type":"message","role":"assistant", "status":"completed",
                "content":[{"type":"refusal","refusal":"cannot help"}]}]),
                FinishReason::Refusal,
            ),
        ] {
            let response = ResponsesResponse::parse(
                &json!({"id":"resp_1","object":"response","status":"completed",
                    "model":"gpt-test","output":output,"end_turn":false}),
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .expect("valid terminal response");
            let reason = finish_details(&response)
                .expect("finish details")
                .expect("finish")
                .reason();
            assert_eq!(reason, expected);
        }
        assert!(matches!(
            ResponsesResponse::parse(
                &json!({"id":"resp_1","object":"response","status":"completed",
                    "model":"gpt-test","output":[],"end_turn":"false"}),
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            ),
            Err(ResponsesHostDecodeError::InvalidField("end_turn"))
        ));
    }

    #[test]
    fn exact_order_usage_cache_and_reasoning_candidates_are_preserved() {
        let Ok(response) = ResponsesResponse::parse(
            &json!({
                "id":"resp_1","object":"response","status":"completed","model":"gpt-test",
                "output":[
                    {"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]},
                    {"id":"r1","type":"reasoning","status":"completed","summary":[],"encrypted_content":"opaque"},
                    {"id":"c1","type":"custom_tool_call","status":"completed","call_id":"call1","name":"code","input":"print(1)"}
                ],
                "usage":{"input_tokens":7,"output_tokens":5,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        ) else {
            panic!("valid response test fixture");
        };
        let Ok(projected) = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::ProviderStored,
            provider_request(),
        ) else {
            panic!("projectable response test fixture");
        };
        assert_eq!(
            projected.finish().map(FinishDetails::reason),
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(projected.usage(), Some(Usage::new(7, 5, 2, 3, 0)));
        let Some(candidate) = projected.artifact_candidate.as_ref() else {
            panic!("completed response has an artifact candidate");
        };
        assert_eq!(candidate.response_id(), "resp_1");
        assert!(!format!("{candidate:?}").contains("resp_1"));
        assert_eq!(candidate.output_count(), 3);
        assert_eq!(candidate.replay().len(), 3);
        assert_eq!(candidate.storage(), ResponsesStorageMode::ProviderStored);
        assert_eq!(
            candidate.replay()[0].purpose(),
            ResponsesReplayPurpose::OutputItem
        );
        assert_eq!(candidate.replay()[0].output_index(), 0);
        assert_eq!(
            candidate.replay()[1].purpose(),
            ResponsesReplayPurpose::OutputItem
        );
        assert_eq!(candidate.replay()[1].output_index(), 1);
        assert!(
            candidate.replay()[1]
                .envelope()
                .payload()
                .get()
                .contains("\"encrypted_content\":\"opaque\"")
        );
        assert_eq!(
            candidate.replay()[2].purpose(),
            ResponsesReplayPurpose::OutputItem
        );
        assert_eq!(candidate.replay()[2].output_index(), 2);
        assert_eq!(
            candidate
                .replay_contract()
                .for_purpose(ResponsesReplayPurpose::OutputItem)
                .map(|kind| (kind.kind().as_str(), kind.version())),
            Some(("responses.output_item", 1))
        );
    }

    #[test]
    fn stateless_projection_emits_exact_replay_without_provider_continuation() {
        let Ok(response) = ResponsesResponse::parse(
            &json!({
                "id":"resp_stateless","object":"response","status":"completed","model":"gpt-test",
                "output":[
                    {"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}
                ]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        ) else {
            panic!("valid stateless response test fixture");
        };
        let Ok(projected) = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::Stateless,
            provider_request(),
        ) else {
            panic!("stateless response must remain replayable");
        };
        let Some(candidate) = projected.artifact_candidate.as_ref() else {
            panic!("stateless response must emit a local replay candidate");
        };
        assert_eq!(candidate.storage(), ResponsesStorageMode::Stateless);
        assert_eq!(candidate.response_id(), "resp_stateless");
        assert_eq!(candidate.output_count(), 1);
        assert_eq!(candidate.replay().len(), 1);
        assert_eq!(
            candidate.replay()[0].purpose(),
            ResponsesReplayPurpose::OutputItem
        );
    }

    #[test]
    fn grok_opaque_output_uses_its_distinct_strict_replay_kind() {
        let response = ResponsesResponse::parse(
            &json!({
                "id":"resp_grok","object":"response","status":"completed","model":"gpt-test",
                "output":[
                    {"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]},
                    {"id":"o1","type":"future_xai_output","status":"completed","payload":{"safe":"opaque"}}
                ]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("Grok response");
        let projection = ResponsesReplayProjection::new(
            ProtocolId::new("xai.responses.grok-subscription.2026-07").expect("protocol"),
            None,
            ModelId::new("gpt-test").expect("model"),
            ReplayKindId::new("xai.output_item").expect("output kind"),
            2,
        )
        .expect("projection")
        .with_opaque_output_kind(
            ReplayKindId::new("xai.opaque_output").expect("opaque kind"),
            1,
        )
        .expect("opaque projection");
        let projected = project_terminal_response(
            response,
            &projection,
            ResponsesStorageMode::Stateless,
            provider_request(),
        )
        .expect("projected");
        let replay = projected.artifact_candidate().expect("candidate").replay();
        assert_eq!(replay[0].envelope().kind().as_str(), "xai.output_item");
        assert_eq!(replay[0].envelope().version(), 2);
        assert_eq!(replay[0].purpose(), ResponsesReplayPurpose::OutputItem);
        assert_eq!(replay[0].output_index(), 0);
        assert_eq!(replay[1].envelope().kind().as_str(), "xai.opaque_output");
        assert_eq!(replay[1].envelope().version(), 1);
        assert_eq!(
            replay[1].purpose(),
            ResponsesReplayPurpose::OpaqueOutputItem
        );
        assert_eq!(replay[1].output_index(), 1);
    }

    #[test]
    fn failed_terminal_is_not_reclassified_as_a_successful_finish_or_artifact() {
        let Ok(response) = ResponsesResponse::parse(
            &json!({
                "id":"resp_failed","status":"failed","model":"gpt-test",
                "error":{"code":"server_error","message":"redacted native detail"}
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        ) else {
            panic!("valid failed response test fixture");
        };
        let Ok(projected) = project_terminal_response(
            response,
            &replay(),
            ResponsesStorageMode::ProviderStored,
            provider_request(),
        ) else {
            panic!("failed terminal remains an exact typed projection");
        };
        assert!(projected.finish().is_none());
        assert!(projected.artifact_candidate.is_none());
        assert!(projected.response().error().is_some());
    }

    #[test]
    fn incomplete_finish_reason_is_exact_and_unknowns_fail_closed() {
        for (reason, expected) in [
            ("max_output_tokens", Some(FinishReason::Length)),
            ("content_filter", Some(FinishReason::Policy)),
            ("future_limit", None),
        ] {
            let response = ResponsesResponse::parse(
                &json!({
                    "id":"resp_incomplete","object":"response","status":"incomplete",
                    "model":"gpt-test","output":[],
                    "incomplete_details":{"reason":reason}
                }),
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .expect("typed incomplete response");
            let projected = project_terminal_response(
                response,
                &replay(),
                ResponsesStorageMode::Stateless,
                provider_request(),
            );
            match expected {
                Some(expected) => assert_eq!(
                    projected
                        .expect("known reason")
                        .finish()
                        .map(FinishDetails::reason),
                    Some(expected)
                ),
                None => assert!(projected.is_err(), "unknown reason was normalized"),
            }
        }
    }

    #[test]
    fn provider_request_binding_requires_a_physical_request_identity() {
        let physical = Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"physical-request"])
            .expect("physical request digest");
        let invalid = ResponsesProviderRequestBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"not-physical"])
                .expect("other-domain digest"),
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"terminal"])
                .expect("terminal digest"),
            None,
        );
        assert!(matches!(
            invalid,
            Err(ResponsesProjectError::InvalidProviderRequestBinding)
        ));
        assert!(matches!(
            ResponsesProviderRequestBinding::new(
                physical,
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"not-terminal"])
                    .expect("other-domain digest"),
                None,
            ),
            Err(ResponsesProjectError::InvalidProviderRequestBinding)
        ));
        assert!(matches!(
            ResponsesProviderRequestBinding::new(physical, physical, None),
            Err(ResponsesProjectError::InvalidProviderRequestBinding)
        ));

        let bound = ResponsesProviderRequestBinding::new(
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"physical-with-request-id"],
            )
            .expect("physical request digest"),
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"terminal-with-request-id"],
            )
            .expect("terminal response digest"),
            Some(ModelProviderRequestId::new("req_sensitive_123").expect("request ID")),
        )
        .expect("provider request binding");
        assert!(!format!("{bound:?}").contains("req_sensitive_123"));
    }
}
