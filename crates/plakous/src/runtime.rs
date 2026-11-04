//! Executor-neutral runtime interfaces and their native implementations.
//!
//! [`Runtime`] spawns, sleeps and reads monotonic time; [`HttpTransport`]
//! sends one HTTP exchange; wall-clock time is [`crate::model::Clock`].
//! [`TokioRuntime`] and, with an HTTP-capable feature, `http::NativeHttpTransport`
//! implement them on Tokio and the bounded Plakous transport.

pub use plakous_agent::TokioRuntime;
pub use plakous_model::{
    HttpBody, HttpRequest, HttpResponse, HttpResponseFuture, HttpTransport, HttpTransportError,
    HttpTransportErrorKind, Runtime, RuntimeBlockingTask, RuntimeSleep, RuntimeTask, SpawnedTask,
};

/// The native [`HttpTransport`], present with any HTTP-capable feature.
#[cfg(any(
    feature = "anthropic",
    feature = "claude",
    feature = "codex",
    feature = "grok",
    feature = "openai",
    feature = "openrouter",
    feature = "web-fetch",
    feature = "xai"
))]
pub mod http;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
