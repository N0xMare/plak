//! Product-owned API-key sources. Every attempt resolves the selected key anew.
use crate::home::paths::PlakPaths;
use std::sync::Arc;
#[cfg(feature = "anthropic")]
#[path = "anthropic.rs"]
mod anthropic;
#[cfg(feature = "openai")]
#[path = "openai.rs"]
mod openai;
#[cfg(feature = "xai-api")]
#[path = "xai.rs"]
mod xai;

#[derive(Clone)]
pub(crate) struct ProductPlatformCredentials {
    paths: PlakPaths,
}
impl ProductPlatformCredentials {
    pub(crate) fn shared(paths: &PlakPaths) -> Arc<Self> {
        Arc::new(Self {
            paths: paths.clone(),
        })
    }
    fn key(&self, account: &str) -> Result<String, String> {
        crate::home::secrets::resolve_api_key(&self.paths, account)?
            .ok_or_else(|| format!("{account} API key is unavailable"))
    }
}
