use super::{Arc, ProductPlatformCredentials};
use plakous_hosted::CliResult;
impl plakous::anthropic::AnthropicApiKeySource for ProductPlatformCredentials {
    fn current_key(
        &self,
    ) -> Result<plakous::anthropic::AnthropicApiKey, plakous::anthropic::AnthropicError> {
        let key = self.key("anthropic").map_err(|_| {
            plakous::anthropic::AnthropicError::new(
                plakous::anthropic::AnthropicErrorKind::InvalidConfiguration,
                plakous::anthropic::AnthropicErrorPhase::Prepare,
                "Anthropic API key is unavailable",
            )
        })?;
        plakous::anthropic::AnthropicApiKey::new(key)
    }
}
impl plakous_hosted::platform::AnthropicCredentials for ProductPlatformCredentials {
    fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "anthropic")?;
        Ok(Arc::new(self.clone()))
    }
}
