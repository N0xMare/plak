use super::{CliResult, NoOpenRouterCredential};

impl crate::openrouter::OpenRouterCredentials for NoOpenRouterCredential {
    fn key_label(&self) -> CliResult<String> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }

    fn preflight(&self) -> CliResult<()> {
        Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
    }
}
