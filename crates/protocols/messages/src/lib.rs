//! Messages-family protocol kernel primitives.
//!
//! This crate owns provider-neutral Messages request grammar, canonical
//! ordering, opaque thinking replay, bounded stream grammar, projection, and
//! wire accounting. Commercial integrations supply closed route, model,
//! dialect, and error policy; they do not own a peer protocol lifecycle.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::too_many_arguments,
    reason = "protocol vocabulary and closed family kernels"
)]

mod artifact;
mod attempt;
mod configuration;
mod http;
mod request;
mod retrieval;
mod stream;
mod stream_support;

pub use artifact::{
    MESSAGES_ARTIFACT_CODEC_REVISION, MESSAGES_ARTIFACT_SCHEMA_REVISION, MessagesArtifactError,
    MessagesFamilyArtifact,
};
pub use attempt::{
    MAX_MESSAGES_ADMITTED_CLEANUP_POLLS, MessagesAttempt, MessagesAttemptBinding,
    MessagesAttemptError, MessagesAttemptEventReceiver, MessagesAttemptEventSink,
    MessagesAttemptProtocolError, MessagesAttemptResponse, MessagesOperationBinding,
    MessagesPhysicalRequestBinding, MessagesProviderRefusal, call_messages_operation,
};
pub use configuration::{
    MESSAGES_FAMILY_POLICY_COMPILER_REVISION, MESSAGES_FAMILY_POLICY_SCHEMA_REVISION,
    MESSAGES_REQUEST_POLICY_REVISION, MessagesProviderConfigurationError,
    compile_messages_api_configuration, compile_messages_configuration,
    compile_messages_oauth_configuration, validate_messages_api_behavior,
};
pub use http::{
    MessagesCredentialRejectedFn, MessagesHttpAttemptService, MessagesHttpErrorHead,
    MessagesHttpHooks, MessagesHttpResponseHead, MessagesHttpSafety, MessagesRedispatchFn,
    PreparedMessagesHttpOperation,
};
pub use plakous_attempt::{
    ExchangeBreak, FailureRisk, PhysicalRequestOutcome, ProviderAcceptance, ReplaySafety,
    RetryClass, TerminalFacts,
};
pub use request::{
    LoweredMessagesRequest, MESSAGES_REPLAY_SCHEMA_REVISION, MessagesRequestError,
    MessagesRequestErrorKind, MessagesRequestPlugin, MessagesRequestPolicy, MessagesRouterPolicy,
    MessagesThinkingDisplay, OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS, REDACTED_REPLAY_KIND,
    SIGNATURE_REPLAY_KIND, ToolOwnership, bind_replay_payload, lower_request,
    lower_request_with_active_artifact,
};
pub use retrieval::{
    MAX_MESSAGES_RETRIEVAL_CONTINUATIONS, MESSAGES_RETRIEVAL_MAX_TOKENS,
    MESSAGES_WEB_FETCH_MAX_CONTENT_TOKENS, MESSAGES_WEB_FETCH_TOOL_TYPE,
    MESSAGES_WEB_SEARCH_TOOL_TYPE, MessagesRetrievalAuthorization, MessagesRetrievalError,
    MessagesRetrievalErrorKind, MessagesRetrievalLimits, MessagesRetrievalOutcome,
    MessagesRetrievalQuery, MessagesRetrievalRequest, MessagesRetrievalSource,
    MessagesRetrievalTransport, MessagesRetrievalUsage, classify_messages_retrieval_body,
    run_messages_retrieval,
};
pub use stream::{
    MessagesStopDetails, MessagesStopDetailsKind, MessagesStopOutcome, MessagesStopReason,
    MessagesStreamDialectHooks, MessagesStreamError, MessagesStreamErrorKind,
    MessagesStreamErrorPhase, MessagesStreamExtensions, MessagesStreamProjector,
    MessagesTerminalProjection, PreparedMessagesStreamBinding, SseDecoder, SseLimits, SseMessage,
};
pub use stream_support::{
    MessagesEventKind, MessagesProviderUsage, MessagesStreamLimits, MessagesUsageUpdate,
    validate_named_event,
};
