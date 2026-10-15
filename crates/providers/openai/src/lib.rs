//! Thin OpenAI route, catalog, and authorization dialect for the shared Responses host.
//!
//! Codex subscription execution uses the shared Responses request, attempt,
//! stream, and projection path. OpenAI Platform uses the same shared host with
//! exact API-key authority and explicit caller context envelopes.

#![forbid(unsafe_code)]
// Protocol docs name OpenAI/Codex; attempt/stream skeletons exceed 100 lines;
// match arms intentionally share retry-never bodies; Result helpers are pure wire.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::redundant_closure_for_method_calls,
    clippy::collapsible_match,
    clippy::collapsible_if,
    clippy::empty_line_after_outer_attr,
    clippy::empty_line_after_doc_comments,
    clippy::double_must_use,
    clippy::ignored_unit_patterns,
    clippy::unnested_or_patterns,
    clippy::unnecessary_wraps,
    clippy::unnecessary_lazy_evaluations,
    clippy::manual_let_else,
    clippy::items_after_statements,
    reason = "OpenAI product names; dialect stream/attempt shape; shared retry-never arms"
)]

mod api_models;
mod config;
mod error;
mod manifest;
mod model;
#[cfg(feature = "native-auth")]
mod native_auth;
#[cfg(feature = "native-auth")]
mod oauth_pins;
mod platform_local;
mod subscription_usage;

pub use api_models::{
    OpenAiApiModel, OpenAiApiModelStatus, OpenAiListedModel, accept_openai_listed_models,
    list_openai_api_models, openai_api_bundled_listing, openai_api_key_fingerprint,
    openai_api_models,
};
pub use config::{
    AccessMode, AuthError, CodexSubscriptionRouteConfig, ConfigError, ConformanceStatus,
    CredentialAuthorityId, CredentialReference, EndpointOrigin, OPENAI_CHATGPT_ORIGIN,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE,
    OPENAI_CODEX_REFERENCE_CLIENT_VERSION, OPENAI_CODEX_RESPONSES_PATH,
    OPENAI_CODEX_RESPONSES_PROTOCOL_ID, OPENAI_CODEX_SOURCE_REVISION,
    OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID, OPENAI_PLATFORM_AUTH_PROFILE_ID,
    OPENAI_PLATFORM_AUTH_REVISION, OPENAI_PLATFORM_AUTHORITY_CLASS,
    OPENAI_PLATFORM_CREDENTIAL_SELECTION, OPENAI_PLATFORM_ORIGIN, OPENAI_PLATFORM_ROUTE_ID,
    OPENAI_RESPONSES_ADAPTER_REVISION, OPENAI_RESPONSES_PROTOCOL_ID, OpenAiApiKeyGenerationGrant,
    OpenAiPlatformRouteConfig, OpenAiProjectId, OpenAiRoute, OpenAiRouteConformance, OpenAiTarget,
    PlatformCredentialSource, PlatformProjectPolicy, PlatformStoragePolicy, ResponsesDialect,
    ResponsesFeature, Routability, SecretString, StaticPlatformCredentialSource, StorageMode,
};
pub use error::{ErrorPhase, MAX_PROVIDER_REQUEST_ID_BYTES, OpenAiError, RetryClass};
pub use manifest::OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS;
#[cfg(feature = "native-auth")]
pub use model::{OpenAiLanguageModel, compile_codex_subscription_configuration};
pub use model::{
    OpenAiPlatformConfigurationAuthority, OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts,
    OpenAiPlatformResolveError, StandardOpenAiHttpBuildError, StandardOpenAiHttpConfig,
    compile_openai_platform_configuration, openai_platform_request_policy,
};
#[cfg(feature = "native-auth")]
pub use native_auth::{
    CodexBrowserLogin, CodexCatalogApiSupport, CodexCatalogCapacityFacts, CodexCatalogEntry,
    CodexCatalogServiceTier, CodexCatalogToolMode, CodexCatalogVisibility,
    CodexCompatibilityAcceptance, CodexCredentialStatus, CodexDeviceAuthorization,
    CodexDeviceLogin, CodexModelCatalog, CodexModelResolutionEvidence, CodexModelResolver,
    CodexSubscriptionCredentialManager, CodexSubscriptionIntegrationProfile,
    CodexSubscriptionLocalAdmin, NativeCodexAuthError, RELEASE_ENROLLED_CODEX_MODELS,
    SubscriptionIntegrationProvenance, release_enrolled_codex_model,
    release_enrolled_codex_tool_mode,
};
pub use plakous_responses_dialect::ResponsesProviderConfigurationError;
pub use platform_local::{OpenAiPlatformBuildError, OpenAiPlatformLocalModel, OpenAiPlatformModel};
pub use subscription_usage::decode_codex_subscription_usage;

// Crate-internal paths resolve through the crate root; the public surface is
// the explicit lists above.
#[cfg(feature = "native-auth")]
pub(crate) use config::{
    NativeCodexCatalogWitness, NativeCodexCredentialAuthority, NativeCodexForcedRefresh,
    NativeCodexPreflight, NativeCodexPreflightRequest, NativeCodexRuntimeAuthority,
};

#[cfg(feature = "native-auth")]
pub(crate) use native_auth::NativeCodexAuthorization;
