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

/// Discovery-only catalog allowlist probe. No inference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenRouterCatalogProbe {
    /// No OpenRouter key in the product secret store.
    NotConfigured,
    /// Key is present but doctor has no OpenRouter default model to discover.
    KeyPresentNoModel,
    /// Authenticated models+endpoints documents validated.
    Ok,
    /// Named catalog/schema failure or transport/auth error (no secret material).
    Failed(String),
}
