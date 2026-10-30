//! Concrete terminal projection over shared Responses output values.
//!
//! The host preserves exact output order, derives canonical finish/usage facts,
//! and prepares bounded replay envelopes for the Responses family artifact.
//! Route/account/storage/cache binding remains the artifact owner's authority.

use std::fmt;

use crate::{
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactError,
    ResponsesArtifactProduction, ResponsesArtifactReplayKind, ResponsesContinuation,
    ResponsesReplayContract, ResponsesReplayItem, ResponsesReplayPurpose, ResponsesStorageMode,
};
use plakous_model::{
    Actor, CanonicalItemPrefixIdentity, CitationUnit, ContentBlock, ConversationItem,
    DataClassification, Digest, DigestDomain, FamilyArtifactProducerFacts, FinishDetails,
    FinishReason, ModelFamilyId, ModelId, ModelProviderRequestId, ModelRequest, ProtocolId,
    Provenance, ReasoningVisibility, ReplayEnvelope, ReplayKindId, ReplayScope,
    SealedFamilyArtifactCandidate, TextPhase, Timestamp, ToolCallState, ToolExecutionOwner,
    ToolInputContract, Usage, ValidatedRequestPlan,
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
    account: Digest,
    output_item_kind: ReplayKindId,
    version: u32,
    opaque_output_kind: Option<(ReplayKindId, u32)>,
    require_encrypted_reasoning: bool,
    require_completed_reasoning_status: bool,
}

impl ResponsesReplayProjection {
    /// Builds exact-account/model replay construction data.
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
        account: Digest,
    ) -> Result<Self, ResponsesProjectError> {
        if version == 0 {
            return Err(ResponsesProjectError::InvalidReplayVersion);
        }
        Ok(Self {
            protocol,
            model_family,
            model,
            account,
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
        account: Digest,
    ) -> bool {
        &self.protocol == protocol
            && self.model_family.as_ref() == model_family
            && &self.model == model
            && self.account == account
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
        let account = self.account.identity_bytes();
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
            4,
            &[
                b"plak-responses/replay-projection-execution-v4",
                &account,
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
        crate::ResponseModelBinding::ExactRequest,
    )
}

/// Projects a terminal response under an explicit provider-reported model
/// binding policy.
pub(crate) fn project_terminal_response_with_model_binding(
    response: ResponsesResponse,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_request: ResponsesProviderRequestBinding,
    model_binding: crate::ResponseModelBinding,
) -> Result<ResponsesTerminalProjection, ResponsesProjectError> {
    if model_binding == crate::ResponseModelBinding::ExactRequest
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
        ReplayScope::ExactAccountModel {
            model: replay.model.clone(),
            account: replay.account,
        },
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
mod tests;
