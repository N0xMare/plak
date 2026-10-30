use super::{Arc, ProductPlatformCredentials};
use plakous_hosted::CliResult;
struct XaiSource {
    product: ProductPlatformCredentials,
    authority: plakous::xai::XaiPlatformApiKeyAuthority,
}

impl XaiSource {
    fn new(
        product: ProductPlatformCredentials,
    ) -> Result<Self, plakous::xai::XaiPlatformBuildError> {
        Ok(Self {
            product,
            authority: plakous::xai::XaiPlatformApiKeyAuthority::new(
                "plak/xai-api",
                "plak/xai/environment-or-file",
                1,
            )
            .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?,
        })
    }
    fn key(&self) -> Result<plakous::api_key::ApiKey, plakous::api_key::ApiKeyError> {
        let key = self
            .product
            .key("xai-api")
            .map_err(|_| plakous::api_key::ApiKeyError::Unavailable)?;
        validate_key(key)
    }
}

fn validate_key(key: String) -> Result<plakous::api_key::ApiKey, plakous::api_key::ApiKeyError> {
    let valid = key.len() <= 16_384 && key.bytes().all(|byte| byte.is_ascii_graphic());
    let key = plakous::api_key::ApiKey::new(key)?;
    if !valid {
        return Err(plakous::api_key::ApiKeyError::InvalidKey);
    }
    Ok(key)
}

impl
    plakous::api_key::ApiKeySource<
        plakous::xai::XaiPlatformApiKeyRequest,
        plakous::xai::XaiPlatformApiKeyAuthority,
    > for XaiSource
{
    fn authority(&self) -> &plakous::xai::XaiPlatformApiKeyAuthority {
        &self.authority
    }
    fn generation_grant(
        &self,
        _: &dyn plakous::api_key::ApiKeyRequest,
    ) -> Result<plakous::xai::XaiPlatformKeyGeneration, plakous::api_key::ApiKeyError> {
        Ok(self.key()?.generation_grant(
            plakous::api_key::CredentialProviderTag::XaiPlatform,
            self.authority.clone(),
        ))
    }
    fn api_key(
        &self,
        _: &plakous::xai::XaiPlatformApiKeyRequest,
        expected: &plakous::xai::XaiPlatformKeyGeneration,
    ) -> Result<plakous::api_key::ApiKey, plakous::api_key::ApiKeyError> {
        if expected.authority() != &self.authority {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        let key = self.key()?;
        if key.generation_grant(
            plakous::api_key::CredentialProviderTag::XaiPlatform,
            self.authority.clone(),
        ) != *expected
        {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        Ok(key)
    }
}

impl plakous_hosted::platform::XaiCredentials for ProductPlatformCredentials {
    fn xai(&self) -> CliResult<Arc<plakous::xai::XaiPlatformKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "xai-api")?;
        Ok(Arc::new(XaiSource::new(self.clone()).map_err(|error| {
            plakous_hosted::CliFailure::new(error.to_string())
        })?))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit test fixtures")]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    #[test]
    fn xai_api_key_file_reaches_generation_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(directory.path().to_owned());
        paths.ensure_layout().unwrap();
        // An exported XAI_API_KEY takes precedence; either source must resolve.
        std::fs::write(paths.api_key_file("xai"), "xai-test-key\n").unwrap();
        let product = ProductPlatformCredentials::shared(&paths);
        let source = plakous_hosted::platform::XaiCredentials::xai(&*product).unwrap();
        plakous::xai::xai_api_key_generation(source.as_ref()).unwrap();
    }

    #[test]
    fn xai_product_key_is_rejected_before_a_generation_can_be_minted() {
        for text in [
            "has space".into(),
            "non-ascii-é".into(),
            "x".repeat(16_385),
            "x".repeat(65_536),
        ] {
            assert_eq!(
                validate_key(text).err(),
                Some(plakous::api_key::ApiKeyError::InvalidKey)
            );
        }
        assert!(validate_key("x".repeat(16_384)).is_ok());
    }
}
