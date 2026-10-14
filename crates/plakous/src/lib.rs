//! Plakous's dependency-thin public facade and optional composition services.
//!
//! The facade re-exports selected components while preserving their independent
//! usability. Optional services compose the same public model, agent, tool, and
//! provider contracts; the facade does not define competing canonical types or
//! own a second agent runtime.
//!
//! ## Public surface tiers
//!
//! | Tier | Modules | Stability |
//! | --- | --- | --- |
//! | **Embedder workflow** | [`agent`], [`application`], [`construction`], [`sessions`] (including `feed`), [`model`], [`tools`], [`profiles`], [`prompts`], [`VERSION`] | Exact-set supported inventory in `public_facade.rs` and consumer fixtures |
//! | **Credentials/subscriptions** | `auth`, `subscriptions` | Feature-gated library workflows; supported inventory imports, not product UX |
//! | **Observations** | `exchange_observation` | Passive feature-gated hooks; no dispatch authority |
//! | **Provider integrations** | `openai`, `xai`, `openrouter`, `anthropic` | Feature-gated; only inventory-named facade workflows are supported; remaining exports are nameable internals |
//!
//! Maintainer spending policy is intentionally absent from this facade:
//!
//! ```compile_fail
//! use plakous::platforms::PaidPlatformAuthorityPaths;
//! ```

//!
//! ```compile_fail
//! use plakous::live_platform::PaidAuthorityStore;
//! ```

#![allow(
    clippy::doc_markdown,
    reason = "facade docs name provider brands and exact feature identifiers"
)]

/// Optional observational exchange hooks. These carry no dispatch authority.
#[cfg(any(
    feature = "native-subscriptions",
    feature = "openrouter",
    feature = "openai",
    feature = "anthropic",
    feature = "xai"
))]
pub mod exchange_observation {
    pub use plakous_http::{
        ConnectionUse, ExchangeObservation, ExchangeObserver, ExchangeTerminal,
    };
}

#[cfg(feature = "composition")]
mod local_composition;
#[cfg(feature = "composition-python")]
pub(crate) use plakous_py_eval as monty_python;
#[cfg(feature = "composition")]
mod js_code_mode;

/// The Plakous library version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Curated agent facade.
///
/// Prefer this module over depending on `plakous-agent` directly from application
/// code. Use [`sessions`] for durable creation, exact opening
/// and recovery; low-level store construction remains component-internal.
///
/// Includes content-free [`TelemetryEmitter`](crate::agent::TelemetryEmitter) / [`TelemetrySink`](crate::agent::TelemetrySink) types for TUI
/// status strips. Telemetry is off unless `AgentBuilder::telemetry` is set.
/// Exact-set inventory: `crates/plakous/tests/public_facade.rs`.
/// Start with `cancellation_shutdown` or `durable_session` in the examples directory.
///
/// Full durable bootstrapping is sealed behind the high-level session workflow:
///
/// ```compile_fail
/// use plakous::agent::AgentBuilder;
///
/// async fn cannot_bootstrap(builder: AgentBuilder) {
///     let _ = builder.build_durable(todo!()).await;
/// }
/// ```
///
/// The private-driver session endpoint is not an agent capability:
///
/// ```compile_fail
/// use plakous::agent::Agent;
///
/// fn cannot_get_endpoint(agent: &Agent) {
///     let _ = agent.session_endpoint();
/// }
/// ```
///
/// Full checkpoint manifests are not available from an agent:
///
/// ```compile_fail
/// use plakous::agent::Agent;
///
/// fn cannot_get_checkpoint(agent: &Agent) {
///     let _ = agent.checkpoint();
/// }
/// ```
///
/// Store-origin internals are not available from facade errors:
///
/// ```compile_fail
/// use plakous::agent::AgentError;
///
/// fn cannot_get_store_error(error: &AgentError) {
///     let _ = error.store_error();
/// }
/// ```
///
/// Terminal results expose the authoritative ID, not the full checkpoint:
///
/// ```compile_fail
/// use plakous::agent::TurnResult;
///
/// fn cannot_get_checkpoint(result: &TurnResult) {
///     let _ = result.checkpoint();
/// }
/// ```
///
/// Full checkpoint aggregates are not curated facade types:
///
/// ```compile_fail
/// use plakous::agent::RuntimeCheckpoint;
/// ```
///
/// Store implementation failures are not curated facade types:
///
/// ```compile_fail
/// use plakous::agent::SessionStoreError;
/// ```
///
/// Private-driver endpoints are not curated facade types:
///
/// ```compile_fail
/// use plakous::agent::AgentSessionEndpoint;
/// ```
///
/// Reconstruction-complete route bindings are not curated facade types:
///
/// ```compile_fail
/// use plakous::agent::ModelRouteBinding;
/// ```
///
/// Runtime manifests are not curated facade types:
///
/// ```compile_fail
/// use plakous::agent::RuntimeManifest;
/// ```
///
/// Store authority is not a curated facade type:
///
/// ```compile_fail
/// use plakous::agent::SessionStore;
/// ```
///
/// The durable runtime is not a curated facade type:
///
/// ```compile_fail
/// use plakous::agent::DurableSessionRuntime;
/// ```
///
/// Object codecs and object graphs are not curated facade types:
///
/// ```compile_fail
/// use plakous::agent::RuntimeCheckpointCodec;
/// ```
///
/// ```compile_fail
/// use plakous::agent::RuntimeObject;
/// ```
///
/// Implementation bridge modules are not exposed through the facade:
///
/// ```compile_fail
/// use plakous::agent::facade_support;
/// ```
pub mod agent {
    #[cfg(feature = "composition")]
    pub use crate::local_composition::{
        LocalCompositionAttachment, LocalCompositionAttachmentBuilder, LocalCompositionContainment,
        LocalCompositionEngineArtifact, LocalCompositionLanguage, LocalCompositionProfile,
        ProductExtensionEvidence, ProductExtensionLimits, attach_local_composition,
        codex_minimal_local_code_mode_sidecar,
    };
    pub use plakous_agent::{
        AbandonedTurnClaim,
        AbandonedTurnReport,
        AbandonmentCause,
        // Actor lifecycle
        Agent,
        AgentBuilder,
        AgentConfig,
        AgentError,
        AgentErrorKind,
        // Events / presentation (serde on AgentEvent + EventCursor for transcripts)
        AgentEvent,
        AgentEventKind,
        AgentRequestPolicy,
        AgentViewSnapshot,
        // Profiles / harness
        CachePreferenceStrategy,
        CancellationStatus,
        // Model policy types re-exported for harness construction
        CapabilityRequirement,
        CheckpointId,
        ChildProcessExit,
        ChildProcessKind,
        CommandAck,
        CommandAckDisposition,
        CommandId,
        CompactionEventOutcome,
        CompactionEventReason,
        CompactionRefusalReason,
        CompactionRung,
        CompactionSummary,
        ContextOverflowRemedy,
        ContextSourceCatalog,
        ContextSourceResolver,
        ContextSourceSelector,
        ContinuationPreferenceStrategy,
        ControlError,
        ControlErrorKind,
        CredentialProviderLabel,
        CredentialRefreshFailureClass,
        CredentialRefreshPhase,
        Digest,
        DigestDomain,
        DurabilityAttemptKind,
        DurabilityAttemptOutcome,
        // Turn control / nameable return types (facade consumers must name these)
        EffectDecisionChoice,
        // Turns
        EffectDecisionRef,
        EffectDecisionResolution,
        EffectDecisionStatus,
        EventCursor,
        EventScope,
        EventSequence,
        EventSubscription,
        EventVisibility,
        GenerationLimits,
        HarnessProfileId,
        HarnessProfileProvenance,
        HarnessProfileSpec,
        HarnessProfileSpecBuilder,
        HttpStatusClass,
        InstructionCompositionStrategy,
        JobHintSource,
        JobNoticeDelivery,
        JobPreemption,
        JobTool,
        ModelReresolutionError,
        ModelReresolutionOperation,
        ModelReresolver,
        NormalizedModelEventKind,
        ProfileError,
        ProfileSemanticVersion,
        RUNTIME_EXECUTOR_STACK_BYTES,
        ReasoningEffort,
        ReasoningPolicy,
        ReasoningPreferenceStrategy,
        RecoveredTurnHandle,
        RegisteredContextSource,
        ReresolvedModel,
        ResolvedContextSourceFact,
        ResolvedHarnessPlan,
        ResolvedInstructionPlan,
        ResumeOutcome,
        SteeringStatus,
        TelemetryContext,
        TelemetryEmitter,
        TelemetryObservation,
        TelemetryObservationKind,
        TelemetrySink,
        TelemetrySinkError,
        TelemetryStatus,
        TelemetryTerminalStatus,
        // Tool lifecycle observation lineage
        ToolCallObservation,
        ToolCallerClass,
        ToolCallerLineageView,
        ToolExposureStrategy,
        Turn,
        TurnCommand,
        TurnControl,
        TurnEventError,
        TurnEvents,
        TurnRecoveryOperationId,
        TurnResult,
        TurnResultHandle,
        TurnResultOutcome,
        TurnSubmission,
        TurnUsage,
        VerifiedContextSourceRef,
        ViewAttachment,
        ViewError,
        ViewRequest,
        WeakAgent,
        compile_explicit_profile,
        compile_explicit_runtime_profile,
    };
    pub use plakous_model::ConversationView;
}
/// Recommended data-only harness profiles shipped with the public facade.
///
/// Provider protocol crates do not own model-facing harness policy. Consumers
/// select these specifications and compile them through
/// [`agent::compile_explicit_profile`].
///
/// Re-exported from `plakous-prompts`, which owns the profiles and the base
/// instructions they name.
pub mod profiles {
    pub use plakous_prompts::profiles::{claude, codex, grok, neutral_coding};
}

/// Model-visible instruction text and the first-turn assembler.
///
/// Tool descriptions are not here: a tool's text travels with its pack
/// in the harness layer. This module owns single-agent instruction assembly
/// and bounded `AGENTS.md` discovery (spec 01 §12).
pub mod prompts {
    pub use plakous_prompts::{
        bases, content_filter, identity, instructions, pins, plan, ptc, vocabulary,
    };
}

/// Construction helpers that compute library-owned configuration facts.
///
/// These reduce ordinary Custom-model construction to the facts a caller
/// actually owns. They materialize complete configuration and never discover a
/// credential, select a provider default, or fall back across routes.
pub mod construction;

/// High-level exact session workflows and opaque backend composition.
///
/// This surface exposes semantic locators and reports only. Store traits,
/// leases, compare-and-swap operations, object graphs, and SQL remain private.
pub mod sessions;

#[cfg(test)]
mod test_model;

pub mod application;
#[cfg(feature = "native-subscriptions")]
pub mod subscriptions;

/// Curated Anthropic integration configuration.
///
/// Includes the API-key `AnthropicLanguageModel` and its explicit credential
/// inputs. Route qualification and admission remain separate from nameability.
#[cfg(feature = "anthropic")]
pub mod anthropic {
    pub use plakous_anthropic::{
        ANTHROPIC_API_VERSION, ANTHROPIC_MESSAGES_PATH, ANTHROPIC_PLATFORM_ORIGIN, AnthropicApiKey,
        AnthropicApiKeySource, AnthropicApiModel, AnthropicApiModelStatus, AnthropicBetaFeature,
        AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, AnthropicLanguageModel,
        AnthropicListedModel, AnthropicMessagesDialect, AnthropicPlatformCapabilityProfile,
        AnthropicPlatformModelConfig, AnthropicPlatformModelResolver, CLAUDE_ENROLLMENTS,
        CLAUDE_SONNET_4_6_MODEL_ID, CLAUDE_SONNET_5_MODEL_ID, ClaudeEnrollment, FeatureStatus,
        RetryAdvice, RetrySafety, accept_anthropic_listed_models, anthropic_api_bundled_listing,
        anthropic_api_models, claude_enrollment, claude_max_output_tokens,
        list_anthropic_api_models,
    };
}
/// Provider-neutral auth foundations (secrets, private store, OAuth helpers).
///
/// Externally owned login-store readers and brokers were removed from the
/// product path. Plak-owned OAuth (`native-subscriptions` / provider
/// `native-auth`) is the only supported login surface. This module re-exports
/// neutral credential, OAuth and account-binding foundations for embedders.
///
/// ```compile_fail
/// use plakous::auth::ExistingLoginAccess;
/// ```
#[cfg(feature = "auth")]
pub mod auth {
    pub use plakous_auth::{
        AccountBinding, AuthSingleFlight, CallbackBinding, CredentialFence, CredentialGeneration,
        CredentialRecord, CredentialSlotId, CredentialStatus, ExposeSecret,
        OfflineCredentialLifecycle, PkceS256, PrivateCredentialStore, SecretBytes, SecretString,
        SecretStringExt, SubscriptionIntegrationProfileId, generate_oauth_correlation_token,
        oauth_pins, oauth_workflow,
    };
}
/// Curated provider-neutral model contracts for facade consumers.
///
/// The implementation crate is intentionally not aliased wholesale. In
/// particular, release-owned Standard-integration authority and protocol-family
/// artifact sealing stay behind their owning provider/family boundaries.
/// Deterministic repository fixtures are absent as well:
///
/// ```compile_fail
/// use plakous::model::ScriptedModel;
/// ```
///
/// Release-owned Standard integration authority is not a facade capability, so
/// descriptor, evidence, identity, registration, and configured-model minting
/// cannot be invoked by facade-only consumers:
///
/// ```compile_fail
/// use plakous::model::StandardIntegrationAuthority;
/// ```
///
/// Protocol-family artifact production and sealing likewise stay private to
/// their owning kernels:
///
/// ```compile_fail
/// use plakous::model::{FamilyArtifactProducerFacts, SealedFamilyArtifactCandidate};
/// ```
pub mod model {
    pub use plakous_model::{
        Actor, AgentId, AssemblerLimits, AttemptId, AudioBlock, BlockCompleted, BlockStart,
        BlockStarted, BranchId, CANONICAL_ITEM_PREFIX_DIGEST_REVISION,
        CONTEXT_PLANNER_POLICY_REVISION, CONTEXT_PLANNER_STRATEGY_REVISION, CachePolicy, CallId,
        CancellationFuture, CancellationToken, CanonicalHistory, CanonicalItemPrefixIdentity,
        CanonicalSegmentIter, CanonicalSegments, Capability, CapabilityConstraints,
        CapabilityDecision, CapabilityOutcome, CapabilityRequirement, CapabilitySet,
        CapacityCertificate, CapacityRule, CheckpointId, CitationAdded, CitationBlock,
        CitationRange, CitationSource, CitationTarget, CitationUnit, Clock, CompactionBlock,
        CompiledProviderConfiguration, CompiledToolProjectionV2, ConfiguredLanguageModel,
        ConfiguredTool, ContentBlock, ContentError, ContextAdmission, ContextAdmissionError,
        ContextAdmissionRequirement, ContextAnchor, ContextCapacity, ContextCapacityFailure,
        ContextContract, ContextContractError, ContextEvidenceClass, ContextEvidenceSource,
        ContextHeadroom, ContextHistoryPlan, ContextHistoryStrategy, ContextInputParts,
        ContextMeasurement, ContextNonGuaranteeDisposition, ContextOverflow,
        ContextPlannerDisposition, ContextPlannerDispositionKind, ContextRequestAnchor,
        ContextRetainedTail, ContextScope, ContextSelectedIntentGroup, ContextSelectedSegment,
        ContextStatus, ContextStatusKind, ContextSummaryRetainedTail, ContextUnavailable,
        ContextWireInput, ConversationItem, ConversationItemIter, ConversationItems,
        ConversationPrefixIdentity, ConversationView, CountBound, CountCertificateKind,
        CountObservation, CredentialSelectionId, DataClassification, Digest, DigestAlgorithm,
        DigestDomain, DigestError, DocumentBlock,
        EFFECTIVE_PROVIDER_CONFIGURATION_COMPILER_REVISION, EffectiveProviderConfiguration,
        EffectiveProviderConfigurationId, EntryId, EventId, EvidenceFreshness, FactProvenance,
        FactSource, FamilyArtifactError, FamilyPolicyEnvelope, FeatureSupport, FinishDetails,
        FinishReason, FixedClock, GenerationLimits, HistoryError, HistoryPlanningError,
        HistorySegment, HistorySegmentKind, HostedTool, IdGenerator, IdentityError, ImageBlock,
        InstructionBlock, InstructionSource, IntegrationClass, LanguageModel, LimitOverflow,
        LocalDurabilityDisposition, MAX_CONTEXT_PLANNER_SEGMENTS, MAX_FAMILY_ARTIFACT_BYTES,
        MAX_FAMILY_POLICY_BYTES, MAX_MODEL_PAID_RECOVERY_BYTES, MAX_MODEL_PROVIDER_CODE_BYTES,
        MAX_MODEL_PROVIDER_REQUEST_ID_BYTES, MAX_PREPARED_CALL_RECORD_BYTES,
        MAX_PREPARED_WIRE_BYTES, MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES,
        MAX_PROVIDER_SEMANTIC_EVIDENCE_BYTES, MAX_RUNTIME_ADMISSION_WITNESS_BYTES,
        MAX_SAFE_DIAGNOSTIC_BYTES, MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION,
        MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION,
        MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION, McpServerId, MeasurementCertificate,
        MediaSource, ModelAdapterProfileId, ModelCacheAffinity, ModelCacheAffinityError,
        ModelCallContext, ModelCallRecoveryError, ModelCallRecoveryEvidence,
        ModelCallRecoveryOperation, ModelCallRecoveryOutcome, ModelCallRecoveryRequest,
        ModelCallRecoveryResult, ModelContentContractV1, ModelControlStream, ModelDiagnostic,
        ModelDispatchRejection, ModelDispatchRejectionClass, ModelDispatchRejectionPhase,
        ModelError, ModelErrorContext, ModelErrorContextError, ModelErrorKind, ModelErrorPhase,
        ModelErrorTarget, ModelEvent, ModelFamilyId, ModelHttpStatus, ModelId,
        ModelImplementationIdentity, ModelNetworkFailure, ModelNetworkTransmission, ModelOperation,
        ModelOperationRoot, ModelPaidProviderAcceptance, ModelPaidRecovery, ModelPaidRecoveryBytes,
        ModelPaidRecoveryRequirement, ModelPaidTerminalFacts, ModelPreparationContext,
        ModelPreparationError, ModelPreparationErrorKind, ModelProviderCode,
        ModelProviderRequestId, ModelRequest, ModelRequestBuilder, ModelResponse,
        ModelResponseAssembler, ModelResponseMetadata, ModelRetry, ModelRetryCause,
        ModelRetryDeclineReason, ModelRetryDelay, ModelRetryLoss, ModelRetryPhase, ModelRetryRisk,
        ModelRetryability, ModelStream, ModelStreamActivity, ModelStreamItem, ModelTransportFault,
        NamespaceId, NativeOptions, OpaqueBlock, OpaqueFallback, OutputBoundDisposition,
        OutputFormat, OutputLocation, OutputPolicy, OutputReservation,
        PORTABLE_TOOL_SCHEMA_REVISION, PORTABLE_TOOL_SCHEMA_V2_REVISION,
        PREPARED_CALL_RECORD_SEMANTIC_REVISION, PROVIDER_OPERATION_SEMANTIC_REVISION,
        PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION, ParallelToolDisposition, PlanDiagnostic,
        PortableToolSchemaV1, PortableToolSchemaV2, PreparedCacheUse, PreparedCallError,
        PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedOutputLimit,
        PreparedOutputLimitSource, PreparedTokenProjection, ProcessingTierDisposition,
        PromptCacheDiagnosis, PromptCacheMissReason, ProtocolId, Provenance,
        ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderCallRecoveryLocator,
        ProviderCallRecoveryLocatorStage, ProviderConfigurationError, ProviderEvent,
        ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
        ProviderServiceTierId, ProviderStorageDisposition, QualifiedTokens, ReachabilityProbe,
        ReachabilityProbeOperation, ReasoningBlock, ReasoningDelta, ReasoningEffort,
        ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
        ReasoningReplayBound, ReasoningSummaryDisposition, ReasoningVisibility,
        ReasoningVisibilityBound, RedactedModelErrorSource, RefusalBlock, RegisteredLanguageModel,
        ReplayDisposition, ReplayEnvelope, ReplayKindId, ReplayScope, RequestError,
        RequestMetadata, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
        ResponseStarted, ResponseStateDisposition, RetainedHistorySegmentIter,
        RetentionDisposition, RouteId, RunId, RuntimeAdmissionWitness, RuntimeModelAuthority,
        RuntimeModelAuthorityError, RuntimeModelAuthorityOperation,
        SELECTED_PROVIDER_CONFIGURATION_REVISION, SEMANTIC_COMPACTION_CUT_REVISION,
        SEMANTIC_COMPACTION_INSTRUCTION, SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS,
        SEMANTIC_COMPACTION_SUMMARY_TOKENS, SEMANTIC_JSON_REVISION, SESSION_WORKFLOW_ROOT_ID_BYTES,
        SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO, SUBSCRIPTION_BALANCE_PREPAID_CREDITS, SafeDiagnostic,
        SafeLimit, SelectedProviderConfiguration, SelectedProviderConfigurationId,
        SemanticCompactionCut, SemanticCompactionCutError, SemanticJsonError, SemanticJsonV1,
        SequenceIdGenerator, ServedBy, SessionId, SessionLineage, SessionWorkflowRootId,
        StableRequestPrefix, StableRequestPrefixDrift, StableRequestPrefixField, StartModelCall,
        StartModelCallWithControl, SubscriptionUsageBalance, SubscriptionUsageCredentialState,
        SubscriptionUsageMeter, SubscriptionUsagePercent, SubscriptionUsagePeriod,
        SubscriptionUsageRead, SubscriptionUsageSnapshot, SubscriptionUsageUnavailable,
        SubscriptionUsageValueError, SubscriptionUsageWindow, Support, SystemClock,
        SystemIdGenerator, TextBlock, TextDelta, TextPhase, Timestamp, ToolCallBlock,
        ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolChoice, ToolConcurrency,
        ToolCorrelation, ToolDefinition, ToolEffect, ToolExecutionDisposition, ToolExecutionOwner,
        ToolInputContract, ToolInputDelta, ToolName, ToolOutputContract, ToolPresentationMode,
        ToolResourceKey, ToolResultBlock, ToolResultKind, ToolResultPart, TurnId, TurnOrigin,
        Usage, UsageUpdate, Utf8TextContractV1, ValidatedRequestPlan, ValidationError,
        ValidationMode, WorkingProjection, WorkingProjectionLimits, context_occupancy_tokens,
        hex_lower, now_unix_millis, now_unix_secs, sha256_bytes, sha256_hex,
    };
}
/// Exact-origin OpenAI API-key execution and the separate Codex subscription integration.
#[cfg(feature = "openai")]
pub mod openai {
    pub use plakous_openai::{
        AccessMode as OpenAiAccessMode, AuthError as OpenAiAuthError, CodexSubscriptionRouteConfig,
        ConfigError as OpenAiConfigError, CredentialAuthorityId, CredentialReference,
        EndpointOrigin, ErrorPhase as OpenAiErrorPhase, OPENAI_CHATGPT_ORIGIN,
        OPENAI_CODEX_RESPONSES_PATH, OpenAiApiKeyGenerationGrant, OpenAiApiModel,
        OpenAiApiModelStatus, OpenAiError, OpenAiListedModel, OpenAiPlatformModel,
        OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts, OpenAiRoute, OpenAiTarget,
        PlatformCredentialSource, PlatformProjectPolicy, PlatformStoragePolicy,
        RetryClass as OpenAiRetryClass, SecretString as OpenAiApiKey,
        StaticPlatformCredentialSource, accept_openai_listed_models, list_openai_api_models,
        openai_api_bundled_listing, openai_api_key_fingerprint, openai_api_models,
    };

    #[cfg(feature = "native-subscriptions")]
    pub use plakous_openai::OpenAiLanguageModel;
}
/// Curated OpenRouter Chat connection and Custom model construction.
///
/// The facade deliberately has no wire-family `auto`, static model catalog, or
/// endpoint-pinned constructor. Discovery authenticates one exact literal and
/// every configured model remains [`crate::model::IntegrationClass::Custom`].
///
/// Raw Chat-kernel and HTTP transport types are not part of this surface:
///
/// ```compile_fail
/// use plakous::openrouter::ChatRouterPolicy;
/// ```
///
/// ```compile_fail
/// use plakous::openrouter::Client;
/// ```
///
/// Strict endpoint pinning remains unavailable until it can be enforced and
/// terminally proved:
///
/// ```compile_fail
/// use plakous::openrouter::StrictPinned;
/// ```
///
/// OpenRouter has no release-owned static model catalog:
///
/// ```compile_fail
/// use plakous::openrouter::OPENROUTER_MODELS;
/// ```
#[cfg(feature = "openrouter")]
pub mod openrouter {
    use std::fmt;
    use std::sync::Arc;

    use plakous_http::{Client, Limits};
    use plakous_model::{
        CapacityCertificate, CapacityRule, ConfiguredLanguageModel, ContextContract,
        ContextEvidenceSource, CountCertificateKind, Digest, DigestDomain, EvidenceFreshness,
        IntegrationClass, MeasurementCertificate, ProtocolId, ProviderBehaviorConfiguration,
        RuntimeModelAuthority, SelectedProviderConfiguration, Timestamp,
    };

    pub use plakous_openrouter::{
        OPENROUTER_CHAT_PROTOCOL_ID, OPENROUTER_MESSAGES_PROTOCOL_ID,
        OPENROUTER_RESPONSES_PROTOCOL_ID, OpenRouterApiKey, OpenRouterApiKeyGenerationGrant,
        OpenRouterApiKeySource, OpenRouterAuthError, OpenRouterCatalogCredentialRequest,
        OpenRouterChatBuildError, OpenRouterChatCredentialRequest, OpenRouterChatDiscoveryConfig,
        OpenRouterChatDiscoveryError, OpenRouterChatEndpointCandidate, OpenRouterChatModelEvidence,
        OpenRouterChatProfile, OpenRouterChatReasoningAlias, OpenRouterChatReasoningReplay,
        OpenRouterCredentialBinding, OpenRouterCredentialRequest, OpenRouterKeyUsage,
        OpenRouterKeyUsageError, OpenRouterMessagesBuildError, OpenRouterMessagesCredentialRequest,
        OpenRouterReasoningFacts, OpenRouterResponsesBuildError, OpenRouterRetentionPolicy,
        OpenRouterRouteError, OpenRouterToolChoiceModes, StaticOpenRouterApiKeySource,
        decode_openrouter_key_usage,
    };

    /// A bounded OpenRouter Chat connection owning one exact route and HTTP client.
    ///
    /// `Debug` never exposes the credential source or API key.
    pub struct OpenRouterChatClient {
        route: plakous_openrouter::OpenRouterChatRoute,
        client: Client,
    }

    impl OpenRouterChatClient {
        /// Creates the exact production Chat route and a bounded WebPKI client.
        ///
        /// # Errors
        ///
        /// Returns a safe connection error when the credential binding or reviewed
        /// transport configuration is invalid.
        pub fn connect(
            key_source: Arc<dyn OpenRouterApiKeySource>,
        ) -> Result<Self, OpenRouterChatConnectionError> {
            Self::connect_inner(key_source, None)
        }

        /// Creates the exact production route with explicit retention restrictions.
        ///
        /// # Errors
        ///
        /// Returns a safe connection error when the route or transport is invalid.
        pub fn connect_with_retention(
            key_source: Arc<dyn OpenRouterApiKeySource>,
            retention: OpenRouterRetentionPolicy,
        ) -> Result<Self, OpenRouterChatConnectionError> {
            Self::connect_inner(key_source, Some(retention))
        }

        fn connect_inner(
            key_source: Arc<dyn OpenRouterApiKeySource>,
            retention: Option<OpenRouterRetentionPolicy>,
        ) -> Result<Self, OpenRouterChatConnectionError> {
            let route = match retention {
                Some(policy) => {
                    plakous_openrouter::OpenRouterChatRoute::new_with_retention(key_source, policy)?
                }
                None => plakous_openrouter::OpenRouterChatRoute::new(key_source)?,
            };
            // OpenRouter's public gateway is qualified over one explicit
            // HTTP/1.1 transport profile. The request is never replayed as a
            // fallback across protocol versions after possible transmission.
            let client = Client::new_http1(Limits::for_streaming_inference())
                .map_err(|_| OpenRouterChatConnectionError::HttpClientConfiguration)?;
            Ok(Self { route, client })
        }

        /// Attaches a passive, explicitly supplied exchange observer.
        #[must_use]
        pub fn with_exchange_observer(
            mut self,
            observer: Option<Arc<dyn crate::exchange_observation::ExchangeObserver>>,
        ) -> Self {
            self.client = self.client.with_observer(observer);
            self
        }

        /// Enables Plak's fixed truthful attribution headers.
        ///
        /// Plak-product-only: another embedder must not attribute its traffic to Plak.
        ///
        /// # Errors
        ///
        /// Returns a route error if the rebuilt exact identity is invalid.
        pub fn with_plak_attribution(mut self) -> Result<Self, OpenRouterChatConnectionError> {
            self.route = self.route.with_plak_attribution()?;
            Ok(self)
        }

        /// Lists authenticated catalog identifiers without inferring capabilities or admission.
        ///
        /// # Errors
        ///
        /// Returns a typed error for authentication, transport, bounds, or malformed catalog data.
        pub async fn list_models(&self) -> Result<Vec<String>, OpenRouterChatDiscoveryError> {
            self.route
                .list_models(&self.client, OpenRouterChatDiscoveryConfig::default())
                .await
        }

        /// Authenticates and discovers exactly one literal model using bounded defaults.
        ///
        /// # Errors
        ///
        /// Returns a typed discovery error for invalid input, authentication,
        /// transport, resource-limit, or catalog-evidence failures.
        pub async fn discover_exact(
            &self,
            literal: &str,
        ) -> Result<OpenRouterChatModelEvidence, OpenRouterChatDiscoveryError> {
            self.discover_exact_with_config(literal, OpenRouterChatDiscoveryConfig::default())
                .await
        }

        /// Authenticates and discovers one literal model with explicit operation bounds.
        ///
        /// # Errors
        ///
        /// Returns a typed discovery error for invalid policy, input,
        /// authentication, transport, resource-limit, or evidence failures.
        pub async fn discover_exact_with_config(
            &self,
            literal: &str,
            config: OpenRouterChatDiscoveryConfig,
        ) -> Result<OpenRouterChatModelEvidence, OpenRouterChatDiscoveryError> {
            self.route
                .discover_chat_model(&self.client, literal, config)
                .await
        }

        /// Reads this inference key's own allowance with one authenticated
        /// `GET /api/v1/key`.
        ///
        /// This is an on-demand control-plane read for the Usage surface. It is
        /// never issued on the turn path, never dispatches inference, and never
        /// requests the management-key Credits endpoint.
        ///
        /// # Errors
        ///
        /// Returns a typed key-usage error for authentication, transport,
        /// status, media-type, or bounded-body failures.
        pub async fn key_usage(&self) -> Result<OpenRouterKeyUsage, OpenRouterKeyUsageError> {
            self.route.key_usage(&self.client).await
        }

        /// Binds a caller-owned conservative context envelope to this exact route.
        ///
        /// `reported_context_window` is the discovered model `context_length`:
        /// the total window the product meters against. `maximum_projected_input`
        /// is the Plak/product operating bound admitted under it, not a provider
        /// context guarantee. Authenticated discovery separately constrains both
        /// when the configured model is constructed.
        ///
        /// # Errors
        ///
        /// Returns a typed construction error when the bound or hidden route
        /// authority cannot form an exact Custom context contract.
        pub fn custom_context_contract(
            &self,
            reported_context_window: u64,
            maximum_projected_input: u64,
        ) -> Result<ContextContract, OpenRouterChatBuildError> {
            self.custom_context_contract_for(
                reported_context_window,
                maximum_projected_input,
                "chat-completions-family",
                "openrouter-chat-custom-accounting-v1",
                "openrouter-chat-canonical-v1",
                "openrouter-chat-output-reservation-v1",
                "plakous-facade-openrouter-chat-wire-bound",
                "complete-lowered-chat-body-utf8-byte-upper-bound",
            )
        }

        /// Binds a caller-owned Responses-family context envelope to this route.
        ///
        /// # Errors
        ///
        /// Returns a typed construction error when the bound or hidden route
        /// authority cannot form an exact Custom context contract.
        pub fn custom_responses_context_contract(
            &self,
            reported_context_window: u64,
            maximum_projected_input: u64,
        ) -> Result<ContextContract, OpenRouterChatBuildError> {
            self.custom_context_contract_for(
                reported_context_window,
                maximum_projected_input,
                "responses-family",
                "responses-wire-bytes-openrouter-v1",
                "responses-canonical-openrouter-v1",
                "openrouter-responses-output-reservation-v1",
                "plakous-facade-openrouter-responses-wire-bound",
                "complete-lowered-responses-body-utf8-byte-upper-bound",
            )
        }

        /// Binds a caller-owned Messages-family context envelope to this route.
        ///
        /// # Errors
        ///
        /// Returns an error when the caller's bound is zero or the exact
        /// Custom context authority cannot be constructed.
        pub fn custom_messages_context_contract(
            &self,
            reported_context_window: u64,
            maximum_projected_input: u64,
        ) -> Result<ContextContract, OpenRouterChatBuildError> {
            self.custom_context_contract_for(
                reported_context_window,
                maximum_projected_input,
                "messages-family",
                "openrouter-messages-wire-v1",
                "openrouter-messages-canonical-v2",
                "openrouter-messages-output-reservation-v1",
                "plakous-facade-openrouter-messages-wire-bound",
                "complete-lowered-messages-body-utf8-byte-upper-bound",
            )
        }

        #[allow(
            clippy::too_many_arguments,
            reason = "closed family contract axes stay explicit at the facade boundary"
        )]
        fn custom_context_contract_for(
            &self,
            reported_context_window: u64,
            maximum_projected_input: u64,
            protocol_family: &'static str,
            accounting_revision: &'static str,
            projection_revision: &'static str,
            reservation_revision: &'static str,
            certificate_id: &'static str,
            measurement: &'static str,
        ) -> Result<ContextContract, OpenRouterChatBuildError> {
            if maximum_projected_input == 0 || maximum_projected_input > reported_context_window {
                return Err(OpenRouterChatBuildError::ContextExceedsDiscovery);
            }
            let identity = self.route.identity();
            let authority = RuntimeModelAuthority::authority_class_for(
                identity.account_binding(),
                identity.credential_authority_binding(),
            )?;
            let declaration = Digest::sha256(
                DigestDomain::ProviderSemanticEvidence,
                1,
                &[
                    b"plakous-facade/openrouter-custom-context-v2",
                    identity.fingerprint().as_bytes(),
                    protocol_family.as_bytes(),
                    &reported_context_window.to_be_bytes(),
                    &maximum_projected_input.to_be_bytes(),
                ],
            )?;
            let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
            let revision = "plakous-facade-openrouter-custom-context-v2";
            let capacity = CapacityCertificate::custom_envelope(
                "plakous-facade-openrouter-custom-capacity",
                revision,
                CapacityRule::ProviderReservedInput {
                    reported_context_window: Some(reported_context_window),
                    maximum_projected_input,
                    auto_compact_input: None,
                },
                authority,
                freshness,
                declaration,
                revision,
            )?;
            let measurement = MeasurementCertificate::new(
                certificate_id,
                revision,
                measurement,
                revision,
                ContextEvidenceSource::CallerDeclaration,
                CountCertificateKind::CustomEnvelopeUpperBound { declaration },
                Some(authority),
                freshness,
            )?;
            Ok(ContextContract::new(
                ProtocolId::new(protocol_family)?,
                accounting_revision,
                projection_revision,
                reservation_revision,
                capacity,
                vec![measurement],
            )?)
        }

        /// Constructs a GatewayManaged Custom model from exact authenticated evidence.
        ///
        /// # Errors
        ///
        /// Returns a build error when evidence is stale or mismatched, or when the
        /// selected configuration cannot be sealed exactly.
        pub fn configured_gateway_managed(
            &self,
            evidence: OpenRouterChatModelEvidence,
            context_contract: ContextContract,
            selected: SelectedProviderConfiguration,
        ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
            plakous_openrouter::OpenRouterChatLanguageModel::configured_gateway_managed(
                self.route.clone(),
                evidence,
                context_contract,
                selected,
                self.client.pooled(),
            )
        }

        /// Constructs a GatewayManaged Custom model from complete behavior.
        ///
        /// The facade delegates exact route selection to the provider that owns
        /// the hidden route identity.
        ///
        /// # Errors
        ///
        /// Returns a build error when discovery evidence or the complete
        /// behavior cannot be sealed against the exact hidden route.
        pub fn configured_gateway_managed_behavior(
            &self,
            evidence: OpenRouterChatModelEvidence,
            context_contract: ContextContract,
            behavior: ProviderBehaviorConfiguration,
        ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
            plakous_openrouter::OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
                self.route.clone(),
                evidence,
                context_contract,
                behavior,
                self.client.pooled(),
            )
        }

        /// Constructs a GatewayManaged Custom Responses model from complete behavior.
        ///
        /// # Errors
        ///
        /// Returns a typed error when discovery, the Responses-family context
        /// contract, or complete provider behavior cannot be sealed exactly.
        pub fn configured_responses_gateway_managed_behavior(
            &self,
            evidence: OpenRouterChatModelEvidence,
            context_contract: ContextContract,
            behavior: ProviderBehaviorConfiguration,
        ) -> Result<ConfiguredLanguageModel, OpenRouterResponsesBuildError> {
            plakous_openrouter::OpenRouterResponsesLanguageModel::configured_gateway_managed_behavior(
                self.route.clone(),
                evidence,
                context_contract,
                behavior,
                self.client.pooled(),
            )
        }

        /// Constructs a GatewayManaged Custom Messages model from complete behavior.
        ///
        /// # Errors
        ///
        /// Returns an error when evidence, context, behavior, route, or
        /// credential authority cannot form one exact Messages target.
        pub fn configured_messages_gateway_managed_behavior(
            &self,
            evidence: OpenRouterChatModelEvidence,
            context_contract: ContextContract,
            behavior: ProviderBehaviorConfiguration,
        ) -> Result<ConfiguredLanguageModel, OpenRouterMessagesBuildError> {
            plakous_openrouter::OpenRouterMessagesLanguageModel::configured_gateway_managed_behavior(
                self.route.clone(),
                evidence,
                context_contract,
                behavior,
                self.client.pooled(),
            )
        }

        /// Constructs a GatewayManaged Custom model with explicit Chat behavior.
        ///
        /// # Errors
        ///
        /// Returns a build error when evidence, profile, or selected configuration
        /// cannot be validated and sealed exactly.
        pub fn configured_gateway_managed_with_profile(
            &self,
            evidence: OpenRouterChatModelEvidence,
            context_contract: ContextContract,
            profile: OpenRouterChatProfile,
            selected: SelectedProviderConfiguration,
        ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
            plakous_openrouter::OpenRouterChatLanguageModel::configured_gateway_managed_with_profile(
                self.route.clone(),
                evidence,
                context_contract,
                profile,
                selected,
                self.client.pooled(),
            )
        }

        /// Returns the immutable integration class of every model built by this lane.
        #[must_use]
        pub const fn integration_class(&self) -> IntegrationClass {
            IntegrationClass::Custom
        }
    }

    impl fmt::Debug for OpenRouterChatClient {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("OpenRouterChatClient")
                .field("route", &self.route)
                .field("client", &"<bounded client>")
                .finish()
        }
    }

    /// Safe construction failure for the facade-owned connection.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum OpenRouterChatConnectionError {
        /// The exact route or credential binding was invalid.
        Route(OpenRouterRouteError),
        /// The facade's reviewed HTTP bounds could not be constructed.
        HttpClientConfiguration,
    }

    impl fmt::Display for OpenRouterChatConnectionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Route(error) => error.fmt(formatter),
                Self::HttpClientConfiguration => {
                    formatter.write_str("OpenRouter HTTP client configuration is invalid")
                }
            }
        }
    }

    impl std::error::Error for OpenRouterChatConnectionError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::Route(error) => Some(error),
                Self::HttpClientConfiguration => None,
            }
        }
    }

    impl From<OpenRouterRouteError> for OpenRouterChatConnectionError {
        fn from(error: OpenRouterRouteError) -> Self {
            Self::Route(error)
        }
    }
}
/// Curated tool extension and configuration surface.
///
/// Dispatch capabilities and runtime authority stay private to the agent
/// driver. Consumers may describe, register, and implement tools, but cannot
/// mint an admitted invocation or participate in durable effect arbitration.
///
/// ```compile_fail
/// # use plakous::tools::{ExecutionEnvironment, ExecutionOperationControl, ProcessSessionTools};
/// # let environment: &ExecutionEnvironment = todo!();
/// # let control: ExecutionOperationControl = todo!();
/// ProcessSessionTools::posix_sh().unwrap().exec_command(
///     environment,
///     r#"{"cmd":"true"}"#,
///     control,
/// );
/// ```
///
/// ```compile_fail
/// # use plakous::tools::{ExecutionEnvironment, ProcessSessionTools};
/// # let environment: &ExecutionEnvironment = todo!();
/// ProcessSessionTools::posix_sh().unwrap().write_stdin(
///     environment,
///     r#"{"session_id":1}"#,
/// );
/// ```
///
/// ```compile_fail
/// # use plakous::tools::{ExecutionEnvironment, ExecutionOperationControl, ProcessSessionTools};
/// # let environment: &ExecutionEnvironment = todo!();
/// # let control: ExecutionOperationControl = todo!();
/// ProcessSessionTools::posix_sh().unwrap().run_terminal_cmd(
///     environment,
///     r#"{"command":"true"}"#,
///     control,
/// );
/// ```
pub mod tools {
    pub use plakous_compose::{
        CodexMinimalLocalCodeModeDescriptorV1, CodexMinimalLocalCodeModeV1,
        CodexNativeLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeV1, CodingPresentationExt,
        ProductJavaScriptCompositionDescriptorV1, ProductJavaScriptCompositionV1,
        PythonPtcDescriptorV1, PythonPtcLimitsV1, PythonPtcPresentationV1, PythonPtcV1,
        SidecarLocator, process_presentation_candidate_v2,
        process_presentation_candidate_with_extensions_v2, standard_coding_menus,
        standard_coding_packs, standard_coding_packs_in_memory,
        validate_process_presentation_candidate_v2,
    };
    pub use plakous_tool_files::FilesToolPack;
    pub use plakous_tool_plan::{
        InMemoryPlanState, PlanItem, PlanItemStatus, PlanReplacement, PlanSnapshot, PlanState,
        PlanStateFuture, PlanToolPack, plan_restoration_note,
    };
    pub use plakous_tool_process::{
        PROCESS_PROJECTION_FAMILY, ProcessSessionTools, ProcessToolPack, ProcessToolProjectionKind,
    };
    pub use plakous_tool_search::SearchToolPack;
    /// Unsupported preview-minimization seam for official observation and hosting.
    #[doc(hidden)]
    pub use plakous_tools::internal::preview as preview_redaction;
    pub use plakous_tools::{
        AlwaysAllowPolicy, ArgumentLimits, CodingPresentation, CompiledSurface, CompositionRuntime,
        DelegationSourceRevision, Earned, EffectAuthorizationBinding, EffectDecision,
        EffectDecisionAuditId, EffectDecisionDisposition, EffectDecisionPrompt,
        EnvironmentDelegation, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackend,
        ExecutionBackendAuthorityId, ExecutionBackendAuthorityRevision, ExecutionBackendError,
        ExecutionBackendErrorKind, ExecutionBackendFuture, ExecutionBackendIdentity,
        ExecutionBackendKind, ExecutionBackendSemanticRevision, ExecutionContainmentDescriptor,
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentDescriptor,
        ExecutionEnvironmentError, ExecutionEnvironmentErrorKind, ExecutionEnvironmentLimits,
        ExecutionEnvironmentScope, ExecutionOperationControl, ExecutionWorkspace,
        FilesystemContainment, FrozenToolExposureDescriptor, FrozenToolRegistry,
        FrozenToolRegistryDescriptor, HostExecutionBackend, HostPathFormat, HostProcessOptions,
        IsolationLevel, JobConfig, JobHost, JobHostCapacity, JobOutputRoot, JobOwner, JobStatus,
        JobStopReason, JobView, JoinedProcessCommand, JoinedProcessInputDisposition,
        JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessShellProfile,
        JoinedProcessStartRequest, JoinedProcessStatus, JoinedProcessTerminalMode,
        LiteralProcessIntent, LiteralProcessRequest, LiteralProcessResult, MenuBinding, MenuEntry,
        MenuOperation, MenuOrdinal, MenuOutput, MenuShape, NamespaceClash, NamespaceClashReason,
        NamespacePresentation, NetworkContainment, PROCESS_OBSERVATION_BYTES,
        PROCESS_SESSION_CAPABILITY_NAME, PROCESS_TOOL_PROJECTION_REVISION,
        PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PRODUCT_JAVASCRIPT_RUN_NAME_V1,
        PRODUCT_JAVASCRIPT_WAIT_NAME_V1, PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1,
        PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_WAIT_NAME_V1, PassthroughPack, PlanStateIdentity,
        PreparedLiteralProcess, PresentationProvider, PresentationRoute, PresentationRouteTuple,
        PresentationRoutes, ProcessContainment, ProjectionCandidateAdmission, PromptContribution,
        PromptDigest, PromptPin, ProviderModelProjectionIdentityV2, ReadOnlyToolPolicy,
        RecordedLiteralProcess, RegisteredToolId, RegisteredToolIdentity, RepositoryMatchCase,
        RepositoryQuery, RepositoryQueryLimits, RepositoryQueryMatch, RepositoryQueryResult,
        SafeErrorMessage, SchemaLimits, ToolBindingError, ToolContext, ToolDescriptorError,
        ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorBindings,
        ToolExecutorBindingsBuilder, ToolExecutorId, ToolExposure, ToolExposureBinding,
        ToolExposureMap, ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome,
        ToolOutcomeKind, ToolOutcomeLimits, ToolPack, ToolPolicy, ToolPolicyDecision,
        ToolPolicyDenial, ToolPolicyFuture, ToolPolicyRequest, ToolPolicyRevision, ToolPreempt,
        ToolProgress, ToolProjectionLoweringV2, ToolProjectionMapV2, ToolRegistration,
        ToolRegistryBuilder, ToolRegistryLimits, ToolRuntimeError, ToolRuntimeErrorKind,
        ValidatedArguments, ValidatedToolSchema, WorkspaceContentIdentity,
        WorkspaceExpectedFileState, WorkspaceMutation, WorkspaceMutationDisposition,
        WorkspaceMutationResultItem, WorkspaceMutationTransaction,
        WorkspaceMutationTransactionResult, WorkspacePolicy, WorkspaceReadCursor,
        WorkspaceReadSlice, WorkspaceReadSliceRequest, WorkspaceReplacement, compile, compile_over,
        drifted_pins, menu_names, register_packs,
    };
}
/// Exact-origin xAI API-key execution and the separate SuperGrok subscription integration.
#[cfg(feature = "xai")]
pub mod xai {
    pub use plakous_xai::{
        GrokCodingProfile, SUPERGROK_RESPONSES_ORIGIN, SUPERGROK_RESPONSES_PATH,
        SuperGrokResponsesDialect, XAI_PLATFORM_GROK_46_CONTEXT_TOKENS, XAI_PLATFORM_GROK_46_MODEL,
        XAI_PLATFORM_MAX_OUTPUT_TOKENS, XaiApiModel, XaiApiModelError, XaiApiModelStatus,
        XaiAuthError, XaiGrokSubscriptionRoute, XaiListedModel, XaiModel, XaiModelError,
        XaiPlatformApiKey, XaiPlatformApiKeyAuthority, XaiPlatformApiKeySource,
        XaiPlatformBuildError, XaiPlatformCredentialGrant, XaiPlatformModel, XaiResolvedModel,
        XaiRouteError, accept_xai_listed_models, grok_max_output_tokens, list_xai_api_models,
        xai_api_bundled_listing, xai_api_key_fingerprint, xai_api_models, xai_platform_selection,
        xai_platform_selection_for,
    };

    #[cfg(feature = "native-subscriptions")]
    pub use plakous_xai::{
        StandardXaiHttpConfig, XaiLanguageModel, XaiLanguageModelConfigError,
        compile_supergrok_subscription_configuration,
    };
}
