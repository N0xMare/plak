//! Executor-neutral runtime interfaces and their native implementations.
//!
//! [`Runtime`] spawns, sleeps and reads monotonic time; [`HttpTransport`]
//! sends one HTTP exchange; wall-clock time is [`crate::model::Clock`].
//! [`TokioRuntime`] and, with an HTTP-capable feature, `http::NativeHttpTransport`
//! implement them on Tokio and the bounded Plakous transport.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

pub use plakous_model::{
    HttpBody, HttpRequest, HttpResponse, HttpResponseFuture, HttpTransport, HttpTransportError,
    HttpTransportErrorKind, Runtime, RuntimeSleep, RuntimeTask, SpawnedTask,
};

/// [`Runtime`] on the Tokio runtime the caller is running on. Its clock is
/// Tokio's, so a paused test clock pauses [`Runtime::now`] too.
///
/// [`Runtime::spawn`] panics when called outside a Tokio runtime, as
/// `tokio::spawn` does.
#[derive(Clone, Copy, Debug)]
pub struct TokioRuntime {
    origin: Instant,
}

impl TokioRuntime {
    /// A runtime whose [`Runtime::now`] counts from this call.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// The process's shared native runtime.
    #[must_use]
    pub fn shared() -> Arc<dyn Runtime> {
        static SHARED: std::sync::OnceLock<Arc<dyn Runtime>> = std::sync::OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Self::new())))
    }
}

impl Default for TokioRuntime {
    fn default() -> Self {
        Self::new()
    }
}

struct TokioTask(tokio::task::AbortHandle);

impl SpawnedTask for TokioTask {
    fn abort(&self) {
        self.0.abort();
    }

    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Runtime for TokioRuntime {
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask> {
        Box::new(TokioTask(tokio::spawn(task).abort_handle()))
    }

    fn sleep(&self, duration: Duration) -> RuntimeSleep {
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

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
