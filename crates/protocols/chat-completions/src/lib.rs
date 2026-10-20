//! Provider-neutral Chat Completions protocol-family kernel.
//!
//! The kernel owns deterministic message lowering, the one-choice streaming
//! lifecycle, indexed local function calls, exact usage, and the bounded
//! turn retry of one logical attempt. Dialects own routes,
//! authorization, headers, and provider errors.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::too_many_arguments,
    reason = "closed protocol vocabulary and explicit attempt authority"
)]

mod attempt;
mod http;
mod request;
mod retrieval;
mod stream;

pub use attempt::{
    CHAT_FAMILY_LEDGER_POLICY, ChatAttempt, ChatAttemptBinding, ChatAttemptError,
    ChatAttemptEventReceiver, ChatAttemptEventSink, ChatAttemptProtocolError, ChatAttemptResponse,
    ChatOperationBinding, ChatPhysicalRequestBinding, MAX_CHAT_ADMITTED_CLEANUP_POLLS,
    call_chat_operation,
};
pub use http::{
    ChatHttpAttemptService, ChatHttpErrorHead, ChatHttpHooks, ChatHttpResponseHead, ChatHttpSafety,
    PreparedChatHttpOperation,
};
pub use plakous_attempt::ReplaySafety;
pub use request::{
    CHAT_REPLAY_SCHEMA_REVISION, ChatDisabledReasoningWire, ChatOutputLimitField,
    ChatReasoningDetailReplayV1, ChatReasoningReplay, ChatReplayTargetV1, ChatRequestError,
    ChatRequestErrorKind, ChatRequestPlugin, ChatRequestPolicy, ChatRouterPolicy,
    LoweredChatRequest, OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS, lower_request,
};
pub use retrieval::{
    CHAT_RETRIEVAL_MAX_TOKENS, CHAT_WEB_PLUGIN_ID, CHAT_WEB_PLUGIN_MAX_RESULTS,
    ChatRetrievalAuthorization, ChatRetrievalError, ChatRetrievalErrorKind, ChatRetrievalLimits,
    ChatRetrievalOutcome, ChatRetrievalReasoning, ChatRetrievalRequest, ChatRetrievalRouting,
    ChatRetrievalSource, ChatRetrievalTransport, ChatRetrievalUsage,
    MAX_CHAT_RETRIEVAL_PHYSICAL_REQUESTS, classify_chat_retrieval_body, run_chat_retrieval,
};
pub use stream::{
    ChatFinishReason, ChatStreamError, ChatStreamErrorKind, ChatStreamExtensions, ChatStreamLimits,
    ChatStreamProjector, ChatTerminalProjection, PreparedChatStreamBinding, ReasoningAliasPolicy,
};
