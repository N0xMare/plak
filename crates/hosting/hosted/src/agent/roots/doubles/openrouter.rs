use super::{Arc, CliResult, NoOpenRouterCredential};

impl crate::openrouter::OpenRouterCredentials for NoOpenRouterCredential {
    fn key_label(&self, _account: &crate::account::AccountRef) -> CliResult<String> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }

    fn preflight(&self, _account: &crate::account::AccountRef) -> CliResult<()> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }

    fn source(
        &self,
        _account: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::openrouter::OpenRouterKeySource>> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }
}
