//! Product-owned API-key sources selected by the route features.
#[cfg(any(feature = "openai", feature = "anthropic", feature = "xai-api"))]
mod available;
#[cfg(not(any(feature = "openai", feature = "anthropic", feature = "xai-api")))]
#[path = "platform_product/unavailable.rs"]
mod available;
pub(crate) use available::ProductPlatformCredentials;
