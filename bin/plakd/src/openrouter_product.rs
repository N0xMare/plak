//! Product OpenRouter vocabulary and feature-selected credential/catalog work.
#[cfg(feature = "openrouter")]
mod available;
#[cfg(not(feature = "openrouter"))]
#[path = "openrouter_product/unavailable.rs"]
mod available;
pub use available::*;
#[cfg(feature = "openrouter")]
pub(crate) mod usage;
#[cfg(not(feature = "openrouter"))]
#[path = "openrouter_product/usage_unavailable.rs"]
pub(crate) mod usage;
pub(crate) use usage::legacy as legacy_key_usage;
