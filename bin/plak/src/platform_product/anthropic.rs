use super::{Arc, ProductPlatformCredentials};
use plakous_hosted::CliResult;
struct AnthropicSource {
    product: ProductPlatformCredentials,
    account: plakous_hosted::account::AccountRef,
}

impl plakous::api_key::ApiKeySource<plakous::anthropic::AnthropicApiKeyRequest>
    for AnthropicSource
{
    fn authority(&self) -> &() {
        &()
    }

    fn generation_grant(
        &self,
        request: &dyn plakous::api_key::ApiKeyRequest,
    ) -> Result<plakous::api_key::ApiKeyGenerationGrant, plakous::api_key::ApiKeyError> {
        if request.provider() != plakous::api_key::CredentialProviderTag::AnthropicPlatform
            || request.origin() != plakous::anthropic::ANTHROPIC_PLATFORM_ORIGIN
            || request.credential_reference().is_some()
        {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        let value = self
            .product
            .key(&self.account)
            .map_err(|_| plakous::api_key::ApiKeyError::Unavailable)?;
        Ok(plakous::api_key::ApiKey::new(value)?.generation_grant(
            plakous::api_key::CredentialProviderTag::AnthropicPlatform,
            (),
        ))
    }

    fn api_key(
        &self,
        request: &plakous::anthropic::AnthropicApiKeyRequest,
        expected: &plakous::api_key::ApiKeyGenerationGrant,
    ) -> Result<plakous::api_key::ApiKey, plakous::api_key::ApiKeyError> {
        if self.generation_grant(request)? != *expected {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        let value = self
            .product
            .key(&self.account)
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
    fn anthropic(
        &self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<Arc<plakous::anthropic::AnthropicKeySource>> {
        self.require_account(account, plakous_hosted::account::Vertical::Anthropic)?;
        Ok(Arc::new(AnthropicSource {
            product: self.clone(),
            account: account.clone(),
        }))
    }
}
