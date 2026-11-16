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
    fn key(&self, account: &plakous_hosted::account::AccountRef) -> Result<String, String> {
        crate::home::secrets::resolve_api_key(&self.paths, account.as_str())?
            .ok_or_else(|| format!("{account} API key is unavailable"))
    }

    fn require_account(
        &self,
        account: &plakous_hosted::account::AccountRef,
        vertical: plakous_hosted::account::Vertical,
    ) -> plakous_hosted::CliResult<()> {
        if account.vertical() != vertical {
            return Err(
                plakous_hosted::capabilities::AccountAdmissionError::AccountTargetMismatch {
                    account: account.to_string(),
                }
                .into(),
            );
        }
        crate::home::secrets::require_api_key(&self.paths, account.as_str())
    }
}
