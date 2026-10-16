//! xAI Responses provider facts and SuperGrok subscription authentication.
//!
//! Inference uses the shared Responses-family host. This crate intentionally
//! keeps Platform API-key and subscription authority separate. Platform execution
//! requires explicit caller-declared context bounds.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    clippy::ignored_unit_patterns,
    clippy::match_same_arms,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    reason = "provider product names, closed error mappings, and bounded auth/test flows"
)]

mod api_models;
#[cfg(feature = "native-auth")]
mod context;
mod dialect;
mod hosted_support;
mod model;
#[cfg(feature = "native-auth")]
mod native_auth;
#[cfg(feature = "native-auth")]
mod oauth_pins;
mod platform;
mod platform_binding;
mod platform_model;
mod responses_dialect;
mod route;
mod subscription_usage;

pub use api_models::{
    XaiApiModel, XaiApiModelError, XaiApiModelStatus, XaiListedModel, accept_xai_listed_models,
    list_xai_api_models, xai_api_bundled_listing, xai_api_key_fingerprint, xai_api_models,
};
pub(crate) use api_models::{xai_platform_capacity_admitted, xai_platform_effort_admitted};
pub use dialect::{XaiAccessMode, XaiDialect, XaiRouteKind, XaiTerminalConvention};
pub use hosted_support::{SUPERGROK_HOSTED_RETRIEVAL, XAI_API_HOSTED_RETRIEVAL};
pub use model::{
    GrokCodingProfile, XaiModel, XaiModelError, XaiResolvedModel, dialect_matches_route,
};
#[cfg(feature = "native-auth")]
pub use model::{StandardXaiHttpConfig, XaiLanguageModel, XaiLanguageModelConfigError};
#[cfg(feature = "native-auth")]
pub use native_auth::{
    XAI_GROK_46_CODING_MODEL, XAI_GROK_BUILD_MODEL, XAI_GROK_BUILD_SOURCE_REVISION,
    XAI_GROK_COMPATIBILITY_PROFILE, XAI_GROK_PRIMARY_CODING_MODEL, XaiBrowserLogin,
    XaiCatalogDiagnosticFacts, XaiCompatibilityAcceptance, XaiCompatibilityProvenance,
    XaiDeviceLogin, XaiModelResolutionEvidence, XaiNativeAuthError, XaiNativeAuthManager,
    XaiNativeCatalogEntry, XaiNativeCatalogReceipt, XaiOfflineCredentialStatus,
    XaiOfflineSelectedSlot, XaiRemoteBrowserLogin, XaiSubscriptionIntegrationProfile,
    XaiSubscriptionStatus, grok_max_output_tokens, is_subscription_coding_model_id,
    subscription_coding_model_ids, subscription_model_reasoning_effort_options,
    subscription_model_supports_reasoning,
};
pub use platform::{
    XAI_PLATFORM_API_KEY_AUTH_PROFILE, XAI_PLATFORM_API_KEY_AUTH_REVISION,
    XAI_PLATFORM_API_KEY_AUTHORITY_CLASS, XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION,
    XAI_PLATFORM_GROK_46_CONTEXT_TOKENS, XAI_PLATFORM_GROK_46_HOSTED_CODE_PROFILE,
    XAI_PLATFORM_GROK_46_HOSTED_CODE_REVISION, XAI_PLATFORM_GROK_46_MODEL,
    XAI_PLATFORM_MAX_OUTPUT_TOKENS, XaiPlatformApiKeyAuthority, XaiPlatformConfigurationError,
    XaiPlatformDocumentationWitness, XaiPlatformHostedCodeWitness, XaiPlatformHttpErrorClass,
    XaiPlatformRequestLoweringFacts, XaiPlatformStreamEventPolicy,
    compile_xai_platform_grok_46_hosted_code_configuration, xai_platform_http_error_policy,
    xai_platform_request_lowering_facts, xai_platform_stream_event_policy,
};
pub use platform_binding::{
    XAI_PLATFORM_EXECUTION_REVISION, XAI_PLATFORM_GROK_46_PROFILE, XaiPlatformApiKey,
    XaiPlatformApiKeySource, XaiPlatformBuildError, XaiPlatformCredentialGrant,
    xai_platform_selection, xai_platform_selection_for,
};
pub use platform_model::XaiPlatformModel;
#[cfg(feature = "native-auth")]
pub use responses_dialect::compile_supergrok_subscription_configuration;
pub use responses_dialect::{
    SUPERGROK_RESPONSES_DIALECT, SUPERGROK_RESPONSES_ORIGIN, SUPERGROK_RESPONSES_PATH,
    SuperGrokEventDelta, SuperGrokResponsesDialect, XAI_RESPONSES_ADAPTER_REVISION,
};
pub use route::{
    XaiAuthError, XaiCredentialBinding, XaiGrokSubscriptionRoute, XaiOrigin, XaiRouteError,
    XaiRouteIdentity, XaiSubscriptionBlockReason,
};
pub use subscription_usage::decode_grok_subscription_usage;

// Crate-internal paths resolve through the crate root; the public surface is
// the explicit lists above.
pub(crate) use platform::{
    XAI_PLATFORM_API_KEY_ORIGIN, XAI_PLATFORM_ENCRYPTED_REASONING_INCLUDE,
    XAI_PLATFORM_RESPONSES_PATH,
};

#[cfg(feature = "native-auth")]
pub(crate) use route::{
    NativeXaiCatalogAdmission, XaiRenewalFloor, XaiRenewedAuthority,
    XaiRevalidatedSubscriptionAuthority, XaiSubscriptionCredentialAuthority,
    XaiSubscriptionCredentialGrant,
};
