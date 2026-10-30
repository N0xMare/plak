use plakous_model::ProviderConfigurationError;
use thiserror::Error;

/// Failure to compile a selected provider configuration through the Responses policy.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesProviderConfigurationError {
    /// One selected behavior dimension is not the exact behavior admitted by the policy.
    #[error("selected Responses behavior does not match policy dimension `{0}`")]
    PolicyMismatch(&'static str),
    /// Canonical family-policy serialization or identity construction failed.
    #[error("Responses family policy identity construction failed")]
    Identity,
    /// Neutral selected-to-effective compilation rejected the resolved facts.
    #[error(transparent)]
    ProviderConfiguration(#[from] ProviderConfigurationError),
}

/// Exact unsupported canonical-content reasons.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesRequestContentError {
    /// The actor is outside the portable Responses role table.
    #[error("unknown actor")]
    UnknownActor,
    /// Assistant phase text requires exact provider output replay.
    #[error("assistant phase without exact output-item replay")]
    AssistantPhaseWithoutReplay,
    /// The admitted route/model does not support image input.
    #[error("image input excluded by admitted route/model facts")]
    ImageInputUnsupported,
    /// Assistant refusal requires exact provider output replay.
    #[error("assistant refusal without exact output-item replay")]
    AssistantRefusalWithoutReplay,
    /// Provider-hosted calls require exact replay material.
    #[error("provider-hosted call without replay envelope")]
    ProviderHostedCallWithoutReplay,
    /// Compatible-replay content was the item's only semantic representation.
    #[error("opaque-only canonical item has no compatible replay target")]
    OpaqueOnlyWithoutCompatibleReplay,
    /// Canonical custom-tool arguments must be one exact JSON string.
    #[error("custom-tool input is not encoded as a canonical JSON string")]
    InvalidCustomToolArguments,
    /// Tool-result images are outside this host revision.
    #[error("tool-result image excluded by admitted route/model facts")]
    ToolResultImage,
    /// Only text tool-result parts are currently representable.
    #[error("non-text tool result")]
    NonTextToolResult,
    /// Reasoning without exact output replay cannot be reconstructed.
    #[error("reasoning without an exact output-item replay envelope")]
    ReasoningWithoutReplay,
    /// Document/audio input is not representable.
    #[error("document or audio")]
    DocumentOrAudio,
    /// Standalone citations are output replay data, not input messages.
    #[error("standalone citation replay")]
    StandaloneCitation,
    /// A compaction marker needs family-native replay material.
    #[error("compaction without replay")]
    CompactionWithoutReplay,
    /// A future canonical block has no admitted lowering.
    #[error("unknown canonical block")]
    UnknownCanonicalBlock,
}

/// Exact unsupported request-option reasons.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesRequestOptionError {
    /// Provider-native option namespaces are not silently forwarded.
    #[error("native options")]
    NativeOptions,
    /// Enabled cache intent has no exact lowering in the selected policy.
    #[error("prompt cache policy")]
    PromptCache,
    /// Affinity-key or gateway-session lowering requires branch cache affinity.
    #[error("prompt cache affinity")]
    PromptCacheAffinity,
    /// A future cache policy has no admitted lowering.
    #[error("unknown prompt cache policy")]
    UnknownPromptCache,
    /// Responses has no shared portable stop-sequence field in this revision.
    #[error("stop sequences")]
    StopSequences,
    /// Custom-tool schema was requested under a policy that refuses it.
    #[error("custom tools")]
    CustomTools,
    /// The selected tool is not advertised.
    #[error("specific tool choice is not advertised")]
    SpecificToolNotAdvertised,
    /// Duplicate matching definitions made selection ambiguous.
    #[error("specific tool choice is ambiguous")]
    SpecificToolAmbiguous,
    /// A future tool-selection policy has no admitted lowering.
    #[error("unknown tool choice")]
    UnknownToolChoice,
    /// A standard process capability has no exact enrolled model projection.
    #[error("process tool projection is unavailable for the exact target")]
    ProcessProjectionUnavailable,
    /// Portable reasoning token budgets have no Responses effort mapping.
    #[error("reasoning token budget")]
    ReasoningTokenBudget,
    /// A future reasoning effort has no admitted mapping.
    #[error("unknown reasoning effort")]
    UnknownReasoningEffort,
    /// A future reasoning policy has no admitted mapping.
    #[error("unknown reasoning policy")]
    UnknownReasoningPolicy,
    /// Structured output is not admitted by the selected policy.
    #[error("structured output")]
    StructuredOutput,
    /// A future output format has no admitted lowering.
    #[error("unknown output format")]
    UnknownOutputFormat,
}

/// Exact protected tool-call/result correlation failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesProtectedToolError {
    /// A canonical tool call was not emitted by the assistant.
    #[error("tool call must be owned by an assistant item")]
    InvalidCallActor,
    /// A canonical tool result was not emitted by the tool actor.
    #[error("tool result must be owned by a tool item")]
    InvalidResultActor,
    /// A canonical call was not in the pre-execution requested state.
    #[error("tool call must be in requested state")]
    InvalidCallState,
    /// Call/result execution owners disagreed or were not consumer-owned.
    #[error("tool call/result execution owner mismatch")]
    OwnerMismatch,
    /// Call/result canonical identities disagreed despite a remote-ID collision.
    #[error("tool call/result canonical identity mismatch")]
    CallIdentityMismatch,
    /// Two canonical or replayed calls reused one remote call identity.
    #[error("tool call identity was reused")]
    DuplicateCall,
    /// A result had no authoritative prior call.
    #[error("tool result has no authoritative prior call")]
    ResultWithoutCall,
    /// A result preceded its call.
    #[error("tool result precedes its authoritative call")]
    ResultBeforeCall,
    /// One call received more than one result.
    #[error("tool call has more than one result")]
    DuplicateResult,
    /// One protected call had no result in the request projection.
    #[error("protected tool call has no result")]
    MissingResult,
}

/// Exact replay/continuation validation failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesReplayError {
    /// A configured replay-kind name was empty or oversized.
    #[error("replay kind is empty or exceeds 256 bytes")]
    InvalidKind,
    /// Replay revision zero is reserved.
    #[error("replay kind version must be nonzero")]
    InvalidVersion,
    /// Prefix bindings were duplicated or unordered.
    #[error("replay prefix bindings must be strictly ordered")]
    PrefixBindingsNotStrictlyOrdered,
    /// Exact admitted operation facts were absent or did not name this request.
    #[error("replay authority is incomplete for the current operation")]
    IncompleteAuthority,
    /// The replay authority did not match the selected target or artifact.
    #[error("replay authority does not match the current operation")]
    AuthorityMismatch,
    /// One exact reusable provider-target axis differed from the current request.
    #[error("Responses artifact {axis} binding differs from the current request")]
    ArtifactTargetMismatch {
        /// Closed, non-secret target axis that differed.
        axis: &'static str,
    },
    /// The artifact was produced under a different stable request prefix.
    ///
    /// Every hard target axis matched; only the stable-prefix binding
    /// differs. The current request is not that artifact's continuation, so it
    /// is prepared fresh. The binding carries only the prefix digest, so the
    /// differing field is named by the runtime that holds both prefixes.
    #[error("Responses artifact stable-prefix binding differs from the current request")]
    ArtifactStablePrefixMismatch,
    /// A versioned authority digest could not be constructed.
    #[error("replay authority digest construction failed")]
    AuthorityConstruction,
    /// A validated artifact failed integrity or structural revalidation.
    #[error("Responses artifact is invalid for replay")]
    InvalidArtifact,
    /// The artifact's reusable target axes differed from the current request.
    #[error("Responses artifact reuse binding differs from the current request")]
    ArtifactBindingMismatch,
    /// The artifact's covered canonical prefix differed from the current request.
    #[error("Responses artifact canonical coverage differs from the current request")]
    ArtifactCanonicalMismatch,
    /// The artifact aggregate expired before replay lowering consumed it.
    #[error("Responses artifact expired before replay lowering")]
    ArtifactExpired,
    /// A raw official replay envelope lacked its exact artifact coordinate.
    #[error("raw output replay lacks an exact artifact coordinate")]
    MissingOutputCoordinate,
    /// A replay payload referenced an unbound canonical prefix.
    #[error("replay payload references an unbound canonical prefix")]
    MissingPrefixBinding,
    /// More than one exact output replay appeared in one canonical item.
    #[error("canonical item contains multiple exact output replays")]
    MultipleOutputReplay,
    /// Raw provider output and canonical effect semantics disagreed.
    #[error("raw output replay disagrees with canonical effect semantics")]
    SemanticPairMismatch,
    /// The envelope kind/scope/model did not match this target.
    #[error("replay envelope is incompatible with the selected target")]
    IncompatibleEnvelope,
    /// The payload was malformed or contradicted its binding.
    #[error("replay envelope payload is malformed or binding-invalid")]
    MalformedEnvelope,
    /// Strict replay requires completed reasoning with non-empty encrypted content.
    #[error("strict reasoning replay is incomplete or lacks encrypted content")]
    StrictEncryptedReasoning,
    /// A provider response identity was empty or oversized.
    #[error("replay response identity is empty or exceeds 1024 bytes")]
    InvalidResponseId,
    /// Output item coordinates were invalid.
    #[error("replayed output item coordinates are invalid")]
    InvalidOutputCoordinates,
    /// Output items from one response were not contiguous and complete.
    #[error("replayed response output group is non-contiguous or incomplete")]
    IncompleteOutputGroup,
    /// A completed response group reappeared later in history.
    #[error("replayed response output group was repeated")]
    RepeatedOutputGroup,
}

/// Request lowering failure before any physical attempt.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesRequestError {
    /// Canonical content cannot be represented.
    #[error("unsupported canonical Responses request content: {0}")]
    Content(#[from] ResponsesRequestContentError),
    /// A request option has no admitted exact wire representation.
    #[error("unsupported Responses request option: {0}")]
    Option(#[from] ResponsesRequestOptionError),
    /// A protected tool exchange was incomplete or ambiguous.
    #[error("invalid protected Responses tool exchange: {0}")]
    ProtectedTool(#[from] ResponsesProtectedToolError),
    /// Replay or continuation material failed closed validation.
    #[error("invalid Responses replay: {0}")]
    Replay(#[from] ResponsesReplayError),
    /// JSON body construction unexpectedly failed.
    #[error("Responses request JSON serialization failed")]
    Serialization,
}

impl ResponsesRequestError {
    /// Reports whether this error only means the active artifact does not
    /// belong to *this* request, so the request should be prepared fresh.
    ///
    /// An active artifact is the continuation of one exact canonical prefix. A
    /// request that covers a different prefix — a compaction fold over a sealed
    /// cut, a qualification probe, a turn whose oldest results were replaced — is
    /// simply not that continuation, and there is nothing wrong with it. Before
    /// this distinction existed the route refused the whole preparation, so on
    /// the Responses wire every compaction attempted while an artifact was
    /// active failed with "canonical coverage differs from the current
    /// request" or "reuse binding differs from the current request", and the
    /// next model round (and the fold rung) died for the life of the session.
    ///
    /// A different stable prefix is the same kind of miss: the operation-budget
    /// wrap-up round withdraws tool selection, so it and the round after it
    /// legitimately differ from the artifact's producer in `tool_choice`.
    /// Before this was included, the wrap-up round of every long Responses
    /// turn failed the whole turn instead of preparing fresh.
    ///
    /// `ArtifactTargetMismatch` is **not** included: that says the artifact
    /// belongs to another account, route, model, adapter, revision, tool plan
    /// or storage disposition, which is an authority problem and still fails
    /// closed.
    #[must_use]
    pub const fn artifact_is_not_for_this_request(&self) -> bool {
        matches!(
            self,
            Self::Replay(
                ResponsesReplayError::ArtifactCanonicalMismatch
                    | ResponsesReplayError::ArtifactStablePrefixMismatch
                    | ResponsesReplayError::ArtifactBindingMismatch
                    | ResponsesReplayError::ArtifactExpired
                    | ResponsesReplayError::InvalidOutputCoordinates
            )
        )
    }
}
