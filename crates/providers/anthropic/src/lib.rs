//! Anthropic Platform Messages integration.
//!
//! This crate supplies exact Anthropic route, API-key authorization, catalog,
//! and error hooks to the provider-neutral `plakous-messages` kernel. The API-key
//! model executes stable Messages with adaptive thinking for every model the
//! reviewed family rules admit (`api_models`), at the model's published effort
//! levels, with an explicit caller-owned context envelope. Controlled TLS tests exercise
//! the production executor; they do not establish live provider acceptance.

#![forbid(unsafe_code)]
// Messages dialect docs; attempt/stream shape; shared retry-never match arms.
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
    reason = "Anthropic product names; Messages attempt/stream shape; shared retry-never arms"
)]

mod api_models;
mod cch;
mod config;
mod dialect;
mod error;
mod http;
mod model;
#[cfg(feature = "native-auth")]
mod native_auth;
mod oauth;
mod oauth_identity;
#[cfg(feature = "native-auth")]
mod oauth_model;
mod request;
mod route;
mod stream;
mod subscription_usage;

pub use oauth_identity::{ClaudeAccountUuid, ClaudeClientDeviceId, ClaudeOAuthRequestIdentity};

pub use api_models::{
    AnthropicApiModel, AnthropicApiModelStatus, AnthropicListedModel,
    accept_anthropic_listed_models, anthropic_api_bundled_listing, anthropic_api_models,
    list_anthropic_api_models,
};
pub use cch::{
    CLAUDE_OAUTH_BILLING_HEADER, CLAUDE_OAUTH_BILLING_HEADER_SUBAGENT, CLAUDE_OAUTH_CC_VERSION,
    CLAUDE_OAUTH_CCH_PLACEHOLDER, attest_claude_oauth_cch, claude_oauth_billing_header,
    claude_oauth_cch_matches_body, first_user_text,
};
pub use config::{
    ANTHROPIC_API_ADAPTIVE_PROFILE_ID, ANTHROPIC_API_KEY_AUTH_PROFILE_ID,
    ANTHROPIC_API_KEY_AUTH_REVISION, ANTHROPIC_API_KEY_AUTHORITY_CLASS,
    ANTHROPIC_API_KEY_CREDENTIAL_SELECTION, ANTHROPIC_PLATFORM_ADMISSION_EXPIRES_AT_SECONDS,
    ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS, ANTHROPIC_PLATFORM_ADMISSION_REVISION,
    ANTHROPIC_SONNET_5_INTEGRATION_REVISION, AnthropicPlatformCapabilityProfile,
    AnthropicPlatformModelConfig, AnthropicPlatformModelResolver, CLAUDE_OPUS_5_5_MODEL_ID,
    CLAUDE_OPUS_5_MODEL_ID, CLAUDE_SONNET_4_6_MODEL_ID, CLAUDE_SONNET_4_CLIENT_TOOLS_PROFILE_ID,
    CLAUDE_SONNET_4_CLIENT_TOOLS_THINKING_PROFILE_ID, CLAUDE_SONNET_4_FAMILY_ID,
    CLAUDE_SONNET_5_CLIENT_TOOLS_PROFILE_ID, CLAUDE_SONNET_5_FAMILY_ID,
    CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID, CLAUDE_SONNET_5_MODEL_ID,
    compile_anthropic_sonnet_5_configuration,
};
pub use dialect::{
    ANTHROPIC_API_VERSION, ANTHROPIC_CODE_EXECUTION_CALLER,
    ANTHROPIC_CODE_EXECUTION_CALLER_20260521, ANTHROPIC_CODE_EXECUTION_TOOL, AnthropicBetaFeature,
    AnthropicMessagesDialect, FeatureMatrixEntry, FeatureStatus, MESSAGES_FEATURE_MATRIX_V1,
};
pub use error::{
    AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, MAX_PROVIDER_ERROR_BODY_BYTES,
    MAX_PROVIDER_ERROR_CODE_BYTES, MAX_PROVIDER_ERROR_MESSAGE_BYTES, MAX_PROVIDER_REQUEST_ID_BYTES,
    RetryAdvice, RetrySafety, classify_provider_error, classify_provider_error_with_safety,
};
pub use http::{
    AnthropicHttpDeadline, AnthropicLocalHttpService, AnthropicLocalHttpServiceFactory,
    AnthropicRetryGrant, AnthropicRetryRevalidation, ClaudeCredentialRenewal,
    STANDARD_ANTHROPIC_EVENT_CAPACITY, call_local_anthropic_operation,
    start_anthropic_messages_model_call,
};
pub use model::AnthropicLanguageModel;
#[cfg(feature = "native-auth")]
pub use native_auth::{
    ClaudeCompatibilityAcceptance, ClaudeEnrolledModel, ClaudeNativeAuthError,
    ClaudeNativeAuthManager, ClaudeOAuthLogin, ClaudeOfflineSelectedSlot,
    ClaudeSubscriptionIntegrationProfile, ClaudeSubscriptionStatus,
};
pub use oauth::{
    CLAUDE_ENROLLMENTS, CLAUDE_OAUTH_ADAPTER_REVISION, CLAUDE_OAUTH_AUTHORIZE_ORIGIN,
    CLAUDE_OAUTH_AUTHORIZE_PATH, CLAUDE_OAUTH_AUTHORIZE_SCOPES, CLAUDE_OAUTH_BASELINE_BETAS,
    CLAUDE_OAUTH_CLIENT_ID, CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID, CLAUDE_OAUTH_DEFAULT_MODEL_ID,
    CLAUDE_OAUTH_EXPIRES_AT_UNIX, CLAUDE_OAUTH_IDENTITY, CLAUDE_OAUTH_INFERENCE_SCOPE,
    CLAUDE_OAUTH_KILL_SWITCH_REVISION, CLAUDE_OAUTH_LEGACY_REDIRECT_URI,
    CLAUDE_OAUTH_MESSAGES_ORIGIN, CLAUDE_OAUTH_MESSAGES_PATH, CLAUDE_OAUTH_MESSAGES_USER_AGENT,
    CLAUDE_OAUTH_NOTICE_REVISION, CLAUDE_OAUTH_PROTOCOL_ID, CLAUDE_OAUTH_REDIRECT_URI,
    CLAUDE_OAUTH_REFRESH_SCOPES, CLAUDE_OAUTH_REVIEWED_AT_UNIX, CLAUDE_OAUTH_ROUTE_ID,
    CLAUDE_OAUTH_SOURCE_REVISION, CLAUDE_OAUTH_STAINLESS_LANG,
    CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION, CLAUDE_OAUTH_STAINLESS_RETRY_COUNT,
    CLAUDE_OAUTH_STAINLESS_RUNTIME, CLAUDE_OAUTH_STAINLESS_RUNTIME_VERSION,
    CLAUDE_OAUTH_STAINLESS_TIMEOUT, CLAUDE_OAUTH_TOKEN_ORIGIN, CLAUDE_OAUTH_TOKEN_PATH,
    CLAUDE_OAUTH_USER_AGENT, ClaudeEnrollment, ClaudeOAuthTokenGrant, claude_authorize_url,
    claude_enrollment, claude_max_output_tokens, claude_oauth_beta_header,
    claude_oauth_enrolled_model, claude_oauth_enrolled_models,
    claude_oauth_reasoning_effort_options, claude_oauth_request_max_tokens,
    claude_oauth_scope_admits_inference, claude_oauth_stainless_arch, claude_oauth_stainless_os,
    claude_token_code_body, claude_token_refresh_body, parse_claude_oauth_authorization_input,
    parse_claude_oauth_token_grant,
};
pub use plakous_messages::{
    MessagesProviderConfigurationError, compile_messages_oauth_configuration,
};
pub use request::{
    AnthropicMessagesOperationBinding, MessagesRequestTranslator, PreparedMessagesRequest,
    REDACTED_REPLAY_KIND, SIGNATURE_REPLAY_KIND, ToolOwnership,
};
pub(crate) use route::AnthropicResolutionTrust;
pub use route::{
    ANTHROPIC_MESSAGES_PATH, ANTHROPIC_PLATFORM_ORIGIN, AnthropicAccountIdentity, AnthropicApiKey,
    AnthropicApiKeyAuth, AnthropicApiKeySource, AnthropicAttemptAuthorization,
    AnthropicCredentialAuthority, AnthropicMessagesTargetBinding, AnthropicMessagesTransport,
    AnthropicModel, AnthropicPlatformRoute, ClaudeProfile, ResolvedAnthropicMessages,
    ScopedFeatureEvidence, ThinkingSupport,
};
pub use stream::{
    AnthropicMessagesStreamDecoder, MessagesStopDetails, MessagesStopDetailsKind,
    MessagesStopOutcome, MessagesStopReason, MessagesStreamLimits, SseDecoder, SseLimits,
    SseMessage,
};
pub use subscription_usage::{
    CLAUDE_SUBSCRIPTION_PROFILE_PATH, CLAUDE_SUBSCRIPTION_USAGE_DOCUMENTATION,
    CLAUDE_SUBSCRIPTION_USAGE_PATH, ClaudeOauthProfile, MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES,
    decode_claude_oauth_profile, decode_claude_subscription_usage,
};
