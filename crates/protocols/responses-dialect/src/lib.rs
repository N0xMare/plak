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
        clippy::too_many_lines,
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
mod test_support {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use plakous_model::{
        CachePolicy, Capability, CapabilityConstraints, CapabilitySet, ContextAdmissionRequirement,
        ConversationView, CredentialSelectionId, Digest, DigestDomain, FactProvenance, FactSource,
        FeatureSupport, LocalDurabilityDisposition, ModelAdapterProfileId, ModelFamilyId, ModelId,
        ModelRequest, ModelRequestBuilder, OutputBoundDisposition, OutputPolicy,
        ParallelToolDisposition, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
        ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
        ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
        ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResolvedModelDescriptor,
        ResolvedProviderAuthorityBinding, ResponseStateDisposition, RetentionDisposition, RouteId,
        RuntimeModelAuthority, SelectedProviderConfiguration, Support, Timestamp, ToolChoice,
    };

    use crate::{ResponsesRequestPolicy, compile_provider_configuration};

    pub(crate) fn model_request_builder(
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> ModelRequestBuilder {
        let integration_profile = ModelAdapterProfileId::new("responses-test-adapter")
            .expect("integration profile fixture");
        let integration_revision =
            ProviderProfileRevisionId::new("responses-test-v1").expect("integration revision");
        let behavior = ProviderBehaviorConfiguration {
            processing_tier: plakous_model::ProcessingTierDisposition::Standard,
            tool_mode: plakous_model::ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(NonZeroU32::MIN),
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: integration_profile.clone(),
                revision: integration_revision.clone(),
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        };
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                plakous_model::ProtocolId::new("openai.responses").expect("protocol fixture"),
                RouteId::new("responses-test-route").expect("route fixture"),
                integration_profile.clone(),
                integration_revision,
                ModelAdapterProfileId::new("responses-test-auth").expect("auth profile fixture"),
                ProviderProfileRevisionId::new("responses-test-auth-v1")
                    .expect("auth revision fixture"),
                ProviderAuthorityClassId::new("test-subscription")
                    .expect("authority class fixture"),
                CredentialSelectionId::new("test-account").expect("credential fixture"),
                ModelId::new("gpt-test").expect("model fixture"),
            ),
            behavior,
        )
        .expect("selected provider configuration fixture");
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0).expect("timestamp fixture"),
            None,
        );
        let capabilities = [
            Capability::Streaming,
            Capability::Tools,
            Capability::Continuation,
            Capability::Caching,
        ]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            plakous_model::ProtocolId::new("openai.responses").expect("protocol fixture"),
            RouteId::new("responses-test-route").expect("route fixture"),
            ModelId::new("gpt-test").expect("model fixture"),
            None,
            Some(ModelFamilyId::new("gpt-family").expect("family fixture")),
            integration_profile,
            CapabilitySet::new(capabilities, BTreeMap::new()),
        );
        let runtime = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-account"])
                .expect("account binding fixture"),
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"responses-test-credentials"],
            )
            .expect("credential binding fixture"),
            1,
            Timestamp::new(1_700_000_000, 0).expect("observed timestamp fixture"),
            Some(Timestamp::new(1_700_000_100, 0).expect("expiry timestamp fixture")),
        )
        .expect("runtime authority fixture");
        let authority = ResolvedProviderAuthorityBinding::new(
            &descriptor,
            &runtime,
            ProviderSemanticEvidence::new(1, b"responses-test-semantic-v1".to_vec())
                .expect("semantic evidence fixture"),
        )
        .expect("provider authority fixture");
        let compiled = compile_provider_configuration(
            &selected,
            descriptor,
            authority,
            ResponsesRequestPolicy::supergrok_stateless(),
        )
        .expect("compiled provider configuration fixture");
        ModelRequest::builder(
            compiled.selected_identity(),
            compiled.effective_identity(),
            input,
            metadata,
        )
    }
}

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
    ResponsesImageFlavor, ResponsesImageOutcome, ResponsesImageRequest, ResponsesImageSource,
    ResponsesImageTransport, decode_responses_image, run_responses_image,
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
    MAX_RESPONSES_SIDE_REQUEST_PHYSICAL_REQUESTS, RESPONSES_RETRIEVAL_MAX_OUTPUT_TOKENS,
    RESPONSES_WEB_SEARCH_SOURCES_INCLUDE, RESPONSES_WEB_SEARCH_TOOL_TYPE,
    RESPONSES_X_SEARCH_TOOL_TYPE, ResponsesCredentialRefresh, ResponsesRetrievalAuthorization,
    ResponsesRetrievalDispatch, ResponsesRetrievalError, ResponsesRetrievalErrorKind,
    ResponsesRetrievalFlavor, ResponsesRetrievalGuard, ResponsesRetrievalLimits,
    ResponsesRetrievalOutcome, ResponsesRetrievalQuery, ResponsesRetrievalRequest,
    ResponsesRetrievalSource, ResponsesRetrievalTransport, ResponsesRetrievalUsage,
    ResponsesSideDocument, classify_responses_retrieval_body, lowest_responses_effort,
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
