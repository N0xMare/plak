//! Shared Responses-family wire protocol, lifecycle, projection and dialect hosts.
//!
//! Commercial integrations select closed policy data and supply exact route,
//! credential and model authority. This crate owns framing, parsing and the
//! shared request, response and attempt hosts; `plakous-attempt` owns the
//! family-independent dispatch ledger. Messages remains a separate wire family.

#![forbid(unsafe_code)]
// Protocol docs name OpenAI/OpenRouter/SuperGrok; Result-heavy pure wire helpers;
// lifecycle policy bool tables.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::struct_excessive_bools,
    clippy::must_use_candidate,
    reason = "product names in ownership docs; pure Result helpers; policy bool tables"
)]
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_value,
        reason = "scenario tests spell out whole wire exchanges and own their fixtures"
    )
)]

mod artifact;
mod dialect;
mod json_fields;
mod project;
mod request;
mod stream;

pub use artifact::{
    MAX_RESPONSES_ARTIFACT_BYTES, MAX_RESPONSES_REPLAY_BYTES, MAX_RESPONSES_REPLAY_ITEMS,
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactError,
    ResponsesArtifactFamily, ResponsesArtifactProduction, ResponsesArtifactReplayKind,
    ResponsesArtifactReuse, ResponsesArtifactReuseKind, ResponsesAuthorizedReplay,
    ResponsesCacheFact, ResponsesContinuation, ResponsesReplayContract, ResponsesReplayItem,
    ResponsesReplayPurpose, ResponsesSemanticRevisions, ResponsesStorageMode,
};
pub(crate) use dialect::{
    BomPolicy, FinishPolicy, SequencePolicy, SequenceValidationError, TerminalPolicy,
};
pub use dialect::{OPENAI_PLATFORM_SEQUENCE_FIELDS, ResponsesDialectProfile};
pub(crate) use json_fields::{
    ResponsesFieldError, optional_object, optional_provider_label, required_object,
    required_provider_label, required_string, required_u32, required_u64, validate_provider_label,
};
pub use json_fields::{optional_string, optional_u64};
pub use project::FamilyProjectionError;
pub use project::{CanonicalIdSources, ProjectorLimits};
pub(crate) use project::{
    ProjectedBlock, ProjectedItemKind, ProjectorPolicy, ReasoningEmitMode, ResponseProjectorCore,
    ToolArgsFinishPolicy,
};
pub use request::{
    FunctionToolChoiceWire, FunctionToolWire, REASONING_SUMMARY_AUTO, ReasoningConfigWire,
    ReasoningEffortWire, function_call_item, function_call_output_item, input_text_part,
    insert_stream_store, join_instruction_texts, message_item, output_text_part, reasoning_object,
};
pub use stream::{
    LifecycleDialectPolicy, ResponseModelBinding, ResponsesDecodeLimitError, ResponsesDecodeLimits,
    ResponsesStreamLimitError, ResponsesStreamLimits,
};
pub(crate) use stream::{
    ReasoningLifecycleMode, ResponsesLifecycle, ResponsesLifecycleError, ResponsesStreamKernel,
    TerminalResponseInput,
};

mod attempt_host;
mod canonical_host;
mod credential;
mod images;
mod model_host;
mod project_host;
mod request_host;
mod response_host;
mod retrieval;
mod stream_host;
mod wire_accounting;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "closed test fixtures use literal values"
)]
mod test_support;

pub use attempt_host::{
    PreparedResponsesAttempt, ResponsesAccountSlot, ResponsesAttempt, ResponsesAttemptBinding,
    ResponsesAttemptBuildError, ResponsesAttemptDialectData, ResponsesAttemptError,
    ResponsesAttemptFuture, ResponsesAttemptResponse, ResponsesAttemptService,
    ResponsesAttemptTarget, ResponsesAttemptTerminal, ResponsesAuthSlot, ResponsesAuthSlots,
    ResponsesAuthorizationAdapter, ResponsesCredentialAuthority, ResponsesDispatchRejection,
    ResponsesHeaderBinding, ResponsesHttpStatusClass, ResponsesObservation,
    ResponsesOperationBinding, ResponsesPhysicalDispatchBinding, ResponsesPhysicalRequestBinding,
    ResponsesPreflightFn, ResponsesPreflightFuture, ResponsesPreflightRequest,
    ResponsesPreparedPhysicalRequest, ResponsesRevalidateFn, ResponsesRevalidationRequest,
    prepare_responses_attempt,
};
pub use canonical_host::{
    ResponsesCanonicalDialectData, ResponsesCanonicalObservationProjector,
    ResponsesCanonicalPolicy, ResponsesCanonicalProjectError, ResponsesCanonicalTerminal,
    ResponsesCanonicalTool, ResponsesCanonicalToolKind, ResponsesCanonicalToolOwner,
    ResponsesHostedProgramContext, project_canonical_terminal,
    project_canonical_terminal_with_hosted_programs,
};
pub use credential::{
    CredentialGenerationFingerprint, CredentialGenerationFingerprintError, CredentialProviderTag,
};
pub use images::{
    RESPONSES_IMAGE_EXCHANGE_TIMEOUT, RESPONSES_IMAGE_MAX_IMAGE_BYTES,
    RESPONSES_IMAGE_MAX_RESPONSE_BYTES, ResponsesImageAuthorization, ResponsesImageEndpoint,
    ResponsesImageFlavor, ResponsesImageRequest, ResponsesImageSource, ResponsesImageTransport,
    decode_responses_image, run_responses_image,
};
pub use model_host::{
    PreparedResponsesModelExecution, ResponsesModelBuildError, ResponsesModelErrorHook,
    bind_reachability_probe, start_prepared_responses_model_call,
};
pub use project_host::{
    ResponsesArtifactCandidateInput, ResponsesPairedArtifactCandidate,
    ResponsesPairedTerminalProjection, ResponsesProjectError, ResponsesProviderRequestBinding,
    ResponsesReplayProjection, ResponsesSealedArtifactCandidate, ResponsesTerminalProjection,
    seal_responses_family_artifact,
};
pub use request_host::{
    CustomToolPolicy, FunctionStrictPolicy, InstructionPlacementPolicy, LoweredResponsesRequest,
    MaxOutputTokensPolicy, ParallelToolCallsFieldPolicy, PromptCachePolicy,
    RESPONSES_FAMILY_POLICY_COMPILER_REVISION, RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
    RESPONSES_REQUEST_POLICY_EXECUTION_REVISION, ReasoningEffortPolicy, ReasoningIncludePolicy,
    ReasoningSummaryPolicy, ResponsesArtifactReplayBridge, ResponsesOutputReplayCoordinate,
    ResponsesPrefixBinding, ResponsesProtectedToolError, ResponsesProviderConfigurationError,
    ResponsesReplayContext, ResponsesReplayError, ResponsesReplayKind, ResponsesReplayMode,
    ResponsesReplayPolicy, ResponsesRequestAuthority, ResponsesRequestConfig,
    ResponsesRequestContentError, ResponsesRequestError, ResponsesRequestOptionError,
    ResponsesRequestPolicy, ResponsesRouterPolicy, ResponsesStoragePolicy, StoreFieldPolicy,
    StructuredOutputPolicy, ToolChoiceFieldPolicy, canonical_local_tools,
    compile_provider_configuration, compile_provider_configuration_with_implementation,
    compile_provider_configuration_with_implementation_and_router_policy,
    is_native_compaction_request, is_string_custom_tool_definition,
    is_unprojected_string_custom_tool_definition, lower_responses_request,
    validate_supported_family_policy,
};
pub use response_host::{
    MAX_URL_CITATION_TITLE_BYTES, ResponsesAnnotation, ResponsesContentPart,
    ResponsesHostDecodeError, ResponsesIncompleteDetails, ResponsesOutputItem,
    ResponsesProviderError, ResponsesProviderErrorClass, ResponsesProviderErrorPolicy,
    ResponsesReasoningContentPart, ResponsesReasoningSummaryPart, ResponsesResponse,
    ResponsesResponseLimits, ResponsesResponseStatus, ResponsesUsage, ResponsesUsagePolicy,
    parse_provider_error_with_policy,
};
pub(crate) use response_host::{output_item_status, parse_unique_json_value};
pub use retrieval::{
    RESPONSES_RETRIEVAL_MAX_OUTPUT_TOKENS, RESPONSES_WEB_SEARCH_SOURCES_INCLUDE,
    RESPONSES_WEB_SEARCH_TOOL_TYPE, RESPONSES_X_SEARCH_TOOL_TYPE, ResponsesCredentialRefresh,
    ResponsesRetrievalAuthorization, ResponsesRetrievalDispatch, ResponsesRetrievalError,
    ResponsesRetrievalErrorKind, ResponsesRetrievalFlavor, ResponsesRetrievalGuard,
    ResponsesRetrievalLimits, ResponsesRetrievalOutcome, ResponsesRetrievalQuery,
    ResponsesRetrievalRequest, ResponsesRetrievalSource, ResponsesRetrievalTransport,
    ResponsesRetrievalUsage, classify_responses_retrieval_body, lowest_responses_effort,
    run_responses_json_side_request, run_responses_retrieval,
};
pub use stream_host::{
    ResponsesEventPolicy, ResponsesFamilySseDecoder, ResponsesFamilyStreamError,
    ResponsesFamilyStreamEvent, ResponsesJsonPolicy, ResponsesStreamCapabilities,
    ResponsesStreamDialectData, ResponsesStreamEventKind,
};
pub use wire_accounting::{ResponsesWireOccurrence, ResponsesWireOccurrenceCounts};

#[cfg(test)]
mod host_pins {
    use super::ResponsesResponseStatus;

    #[test]
    fn completed_status_is_terminal() {
        assert!(ResponsesResponseStatus::Completed.is_terminal());
        assert_eq!(
            ResponsesResponseStatus::parse("completed"),
            ResponsesResponseStatus::Completed
        );
    }
}
