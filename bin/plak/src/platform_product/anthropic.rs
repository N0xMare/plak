use super::{Arc, ProductPlatformCredentials};
use plakous_hosted::CliResult;
impl plakous::api_key::ApiKeySource<plakous::anthropic::AnthropicApiKeyRequest>
    for ProductPlatformCredentials
{
    fn authority(&self) -> &() {
        &()
    }

    fn generation_grant(
        &self,
        _: &dyn plakous::api_key::ApiKeyRequest,
    ) -> Result<plakous::api_key::ApiKeyGenerationGrant, plakous::api_key::ApiKeyError> {
        let value = self
            .key("anthropic")
            .map_err(|_| plakous::api_key::ApiKeyError::Unavailable)?;
        Ok(plakous::api_key::ApiKey::new(value)?.generation_grant(
            plakous::api_key::CredentialProviderTag::AnthropicPlatform,
            (),
        ))
    }

    fn api_key(
        &self,
        _request: &plakous::anthropic::AnthropicApiKeyRequest,
        expected: &plakous::api_key::ApiKeyGenerationGrant,
    ) -> Result<plakous::api_key::ApiKey, plakous::api_key::ApiKeyError> {
        let value = self
            .key("anthropic")
            .map_err(|_| plakous::api_key::ApiKeyError::Unavailable)?;
        let key = plakous::api_key::ApiKey::new(value)?;
        if key.generation_grant(
            plakous::api_key::CredentialProviderTag::AnthropicPlatform,
            (),
        ) != *expected
        {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        Ok(key)
    }
}
impl plakous_hosted::platform::AnthropicCredentials for ProductPlatformCredentials {
    fn anthropic(&self) -> CliResult<Arc<plakous::anthropic::AnthropicKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "anthropic")?;
        Ok(Arc::new(self.clone()))
    }
}
