//! Optional HTTP observation stays within the features selecting its transport.

#[cfg(any(
    feature = "codex",
    feature = "grok",
    feature = "claude",
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter",
    feature = "web-fetch"
))]
mod available;
#[cfg(not(any(
    feature = "codex",
    feature = "grok",
    feature = "claude",
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter",
    feature = "web-fetch"
)))]
#[path = "exchange_observation/unavailable.rs"]
mod available;

pub(crate) use available::{Observer, observe};
