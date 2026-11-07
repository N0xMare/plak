use super::{CliResult, NoOpenRouterCredential};

impl crate::openrouter::OpenRouterCredentials for NoOpenRouterCredential {
    fn key_label(&self, _account: &crate::account::AccountRef) -> CliResult<String> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }

    fn preflight(&self, _account: &crate::account::AccountRef) -> CliResult<()> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }
}
