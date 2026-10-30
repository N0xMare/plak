//! Generic Responses-family **dialect host** shapes.
//!
//! Free-function request/stream/response/project hosts plus shared value types.
//! Dialects (OpenRouter, xAI) consume via type aliases and hooks, not via a
//! mandatory plugin trait.
//!
//! ## Ownership
//!
//! | Layer | This crate | Pure wire (`plakous-responses`) | Dialect consumers |
//! | --- | --- | --- | --- |
//! | Request / stream / response / project **hosts** | yes | pure wire builders | supply values, pins, hooks |
//! | SSE framing, lifecycle, projector core | compose | owns machines | hooks only |
//! | Attempt lifecycle / ledger / await | compose `plakous-attempt` | re-exports only | branded façades |
//! | Secrets, origins, catalogs, agent types | **never** | never | dialect-only |
//! | Anthropic Messages | **never** | never | own family |

#![forbid(unsafe_code)]
// Host docs name OpenRouter/xAI.
#![allow(
    clippy::doc_markdown,
    clippy::must_use_candidate,
    reason = "product names in host docs"
)]
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_value,
        reason = "scenario tests spell out whole wire exchanges and own their fixtures"
    )
)]

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
    ResponsesResponseLimits, ResponsesResponseStatus, ResponsesUsage, ResponsesUsageError,
    ResponsesUsagePolicy, output_item_status, parse_annotation, parse_content_part,
    parse_custom_tool_call, parse_function_call, parse_message, parse_output_item,
    parse_provider_error_with_policy, parse_reasoning, parse_unique_json_value,
    validate_output_identity_roles,
};
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
    ResponsesFamilyStreamEvent, ResponsesJsonPolicy, ResponsesSseDecoderFront,
    ResponsesStreamCapabilities, ResponsesStreamDialectData, ResponsesStreamEventKind,
    ResponsesStreamHostError, map_sequence_error_standard, map_sse_error_standard,
    required_output_index,
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
