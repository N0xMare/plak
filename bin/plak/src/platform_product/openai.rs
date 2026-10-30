use super::{Arc, ProductPlatformCredentials};
use plakous::api_key::ApiKeyError;
use plakous::openai::{
    ApiKey, CredentialAuthorityId, CredentialReference, EndpointOrigin, OpenAiKeyGeneration,
    OpenAiKeySource, StaticOpenAiKeySource,
};
use plakous_hosted::error::{CliFailure, CliResult};
#[derive(Clone)]
struct OpenAiSource {
    product: ProductPlatformCredentials,
    openai_reference: CredentialReference,
    openai_authority: CredentialAuthorityId,
}
impl OpenAiSource {
    fn new(product: ProductPlatformCredentials) -> CliResult<Self> {
        Ok(Self {
            product,
            openai_reference: CredentialReference::application("plak/openai")
                .map_err(|_| CliFailure::new("OpenAI credential reference is invalid"))?,
            openai_authority: CredentialAuthorityId::new("plak/openai/environment-or-file")
                .map_err(|_| CliFailure::new("OpenAI credential authority is invalid"))?,
        })
    }
    fn key(&self, account: &str) -> Result<String, String> {
        self.product.key(account)
    }
    fn openai_source(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
    ) -> Result<StaticOpenAiKeySource, ApiKeyError> {
        if reference != &self.openai_reference || origin.as_str() != "https://api.openai.com/" {
            return Err(ApiKeyError::BindingMismatch);
        }
        let key = self.key("openai").map_err(|_| ApiKeyError::Unavailable)?;
        let key = ApiKey::new(key).map_err(|_| ApiKeyError::Unavailable)?;
        Ok(plakous::openai::static_openai_api_key_source(
            key,
            self.openai_reference.clone(),
            self.openai_authority.clone(),
        ))
    }
}
impl plakous::api_key::ApiKeySource<plakous::openai::OpenAiApiKeyRequest<'_>, CredentialAuthorityId>
    for OpenAiSource
{
    fn authority(&self) -> &CredentialAuthorityId {
        &self.openai_authority
    }
    fn generation_grant(
        &self,
        request: &dyn plakous::api_key::ApiKeyRequest,
    ) -> Result<OpenAiKeyGeneration, ApiKeyError> {
        if request.provider() != plakous::api_key::CredentialProviderTag::OpenAiPlatform
            || request.origin() != "https://api.openai.com/"
            || request.credential_reference() != Some(self.openai_reference.as_str())
        {
            return Err(ApiKeyError::BindingMismatch);
        }
        let value = self.key("openai").map_err(|_| ApiKeyError::Unavailable)?;
        Ok(ApiKey::new(value)?.generation_grant(
            plakous::api_key::CredentialProviderTag::OpenAiPlatform,
            self.openai_authority.clone(),
        ))
    }
    fn api_key(
        &self,
        request: &plakous::openai::OpenAiApiKeyRequest<'_>,
        expected: &OpenAiKeyGeneration,
    ) -> Result<ApiKey, ApiKeyError> {
        self.openai_source(request.reference(), request.endpoint())?
            .api_key(request, expected)
    }
}

impl plakous_hosted::platform::OpenAiCredentials for ProductPlatformCredentials {
    fn openai(&self) -> CliResult<Arc<OpenAiKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "openai")?;
        Ok(Arc::new(OpenAiSource::new(self.clone())?))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit test fixtures")]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    use plakous::api_key::ApiKeySource as _;

    struct Selection<'a> {
        reference: &'a CredentialReference,
        origin: &'a EndpointOrigin,
    }

    impl<'a> Selection<'a> {
        fn new(reference: &'a CredentialReference, origin: &'a EndpointOrigin) -> Self {
            Self { reference, origin }
        }
    }

    impl plakous::api_key::ApiKeyRequest for Selection<'_> {
        fn provider(&self) -> plakous::api_key::CredentialProviderTag {
            plakous::api_key::CredentialProviderTag::OpenAiPlatform
        }
        fn origin(&self) -> &str {
            self.origin.as_str()
        }
        fn credential_reference(&self) -> Option<&str> {
            Some(self.reference.as_str())
        }
    }
    use plakous::openai::{CredentialReference, EndpointOrigin};

    fn source(paths: &PlakPaths) -> CliResult<Arc<OpenAiSource>> {
        Ok(Arc::new(OpenAiSource::new(ProductPlatformCredentials {
            paths: paths.clone(),
        })?))
    }

    #[test]
    fn official_openai_origin_reaches_credential_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let source = source(&PlakPaths::explicit(directory.path().to_owned())).unwrap();
        for spelling in [
            "https://api.openai.com",
            "https://api.openai.com/",
            "https://api.openai.com:443/",
        ] {
            let origin = EndpointOrigin::parse(spelling).unwrap();
            // No key is needed to prove admission: an absent or invalid environment
            // credential may fail resolution, but the official binding must pass.
            assert!(matches!(
                source.generation_grant(&Selection::new(&source.openai_reference, &origin)),
                Ok(_) | Err(ApiKeyError::Unavailable)
            ));
        }
    }

    #[test]
    fn openai_credentials_refuse_foreign_origin_and_reference() {
        let directory = tempfile::tempdir().unwrap();
        let source = source(&PlakPaths::explicit(directory.path().to_owned())).unwrap();
        for spelling in ["https://example.com/", "https://api.openai.com:8443/"] {
            let origin = EndpointOrigin::parse(spelling).unwrap();
            assert!(matches!(
                source.generation_grant(&Selection::new(&source.openai_reference, &origin)),
                Err(ApiKeyError::BindingMismatch)
            ));
        }
        let origin = EndpointOrigin::parse("https://api.openai.com/").unwrap();
        let reference = CredentialReference::application("plak/other").unwrap();
        assert!(matches!(
            source.generation_grant(&Selection::new(&reference, &origin)),
            Err(ApiKeyError::BindingMismatch)
        ));
    }
}
