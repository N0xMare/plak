//! Thin dynamic OpenRouter route, discovery, authorization, and protocol dialect data.
//!
//! The crate owns route, catalog, authorization, capability tables, and narrow
//! OpenRouter dialect facts. Family kernels own request lowering, attempts,
//! stream decoding, terminal projection, and artifact execution.
//!
//! Provider-native request bodies, authorization values, and response objects
//! remain behind this crate's transport and canonical projection boundary.
//!
//! ```compile_fail
//! use plakous_openrouter::OpenRouterPreparedRequest;
//!
//! fn leak_request(prepared: &OpenRouterPreparedRequest) {
//!     let _ = prepared.request();
//! }
//! ```
//!
//! ```compile_fail
//! use plakous_openrouter::OpenRouterResponse;
//!
//! fn leak_response(response: &OpenRouterResponse) {
//!     let _ = response.native();
//! }
//! ```
//!
#![forbid(unsafe_code)]
// Protocol docs name OpenRouter; attempt/stream skeletons exceed 100 lines;
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
    reason = "OpenRouter product name; dialect stream/attempt shape; shared retry-never arms"
)]

mod chat;
mod dialect;
mod discovery;
mod execution;
mod hosted_support;
mod key_usage;
mod messages;
mod model;
mod retrieval;
mod route;

pub use chat::{
    CHAT_COMPLETIONS_FAMILY_ID, OPENROUTER_CHAT_COMPLETIONS_PATH, OPENROUTER_CHAT_PROTOCOL_ID,
    OpenRouterChatBuildError, OpenRouterChatEndpointCandidate, OpenRouterChatHttpConfig,
    OpenRouterChatLanguageModel, OpenRouterChatModelEvidence, OpenRouterChatProfile,
    OpenRouterChatReasoningAlias, OpenRouterChatReasoningReplay, OpenRouterChatRoutingPolicy,
    OpenRouterChatTarget, OpenRouterReasoningFacts, OpenRouterToolChoiceModes,
};
pub use dialect::{
    OPENROUTER_RESPONSES_PATH, OPENROUTER_RESPONSES_PROTOCOL_ID, OpenRouterAccessMode,
    OpenRouterDialect, OpenRouterRouteKind,
};
pub use discovery::{OpenRouterChatDiscoveryConfig, OpenRouterChatDiscoveryError};
pub use execution::{
    OpenRouterResponsesHttpConfig, OpenRouterResponsesLanguageModel,
    OpenRouterResponsesLanguageModelConfigError,
};
pub use hosted_support::OPENROUTER_HOSTED_RETRIEVAL;
pub use key_usage::{OpenRouterKeyUsage, OpenRouterKeyUsageError, decode_openrouter_key_usage};
pub use messages::{
    MESSAGES_FAMILY_ID, OPENROUTER_MESSAGES_PATH, OPENROUTER_MESSAGES_PROTOCOL_ID,
    OpenRouterMessagesBuildError, OpenRouterMessagesHttpConfig, OpenRouterMessagesLanguageModel,
    OpenRouterMessagesRoutingPolicy, OpenRouterMessagesTarget,
};
pub use model::{
    OPENROUTER_RESPONSES_PROFILE, OpenRouterResponsesBuildError, OpenRouterResponsesRoutingPolicy,
    OpenRouterResponsesTarget, compile_openrouter_responses_configuration,
};
pub use plakous_chat_completions::{
    ChatRetrievalError, ChatRetrievalErrorKind, ChatRetrievalLimits, ChatRetrievalOutcome,
    ChatRetrievalRequest, ChatRetrievalSource, ChatRetrievalUsage,
};
pub use retrieval::OpenRouterRetrieval;
pub use route::{
    OpenRouterApiKey, OpenRouterApiKeyGenerationGrant, OpenRouterApiKeySource,
    OpenRouterAttribution, OpenRouterAuthError, OpenRouterCatalogCredentialRequest,
    OpenRouterChatCredentialRequest, OpenRouterChatRoute, OpenRouterChatRouteIdentity,
    OpenRouterCredentialBinding, OpenRouterCredentialRequest, OpenRouterMessagesCredentialRequest,
    OpenRouterOrigin, OpenRouterRetentionPolicy, OpenRouterRetrievalCredentialRequest,
    OpenRouterRouteError, OpenRouterRoutingPolicy, StaticOpenRouterApiKeySource,
};
