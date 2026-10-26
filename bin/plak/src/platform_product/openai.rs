use super::{Arc, ProductPlatformCredentials};
use plakous::openai::{
    CredentialAuthorityId, CredentialReference, EndpointOrigin, OpenAiApiKey,
    OpenAiApiKeyGenerationGrant, OpenAiAuthError, PlatformCredentialSource,
    StaticPlatformCredentialSource,
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
    ) -> Result<StaticPlatformCredentialSource, OpenAiAuthError> {
        if reference != &self.openai_reference || origin.as_str() != "https://api.openai.com/" {
            return Err(OpenAiAuthError::BindingMismatch);
        }
        let key = self
            .key("openai")
            .map_err(|_| OpenAiAuthError::SourceFailure)?;
        let key = OpenAiApiKey::new(key).map_err(|_| OpenAiAuthError::SourceFailure)?;
        Ok(StaticPlatformCredentialSource::new(
            key,
            self.openai_reference.clone(),
            self.openai_authority.clone(),
        ))
    }
}
impl PlatformCredentialSource for OpenAiSource {
    fn credential_authority(&self) -> &CredentialAuthorityId {
        &self.openai_authority
    }

    fn api_key(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
    ) -> Result<OpenAiApiKey, OpenAiAuthError> {
        self.openai_source(reference, origin)?
            .api_key(reference, origin)
    }

    fn generation_grant(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
    ) -> Result<OpenAiApiKeyGenerationGrant, OpenAiAuthError> {
        self.openai_source(reference, origin)?
            .generation_grant(reference, origin)
    }

    fn api_key_for_generation(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<OpenAiApiKey, OpenAiAuthError> {
        self.openai_source(reference, origin)?
            .api_key_for_generation(reference, origin, expected)
    }
}

impl plakous_hosted::platform::OpenAiCredentials for ProductPlatformCredentials {
    fn openai(&self) -> CliResult<Arc<dyn PlatformCredentialSource>> {
        crate::home::secrets::require_api_key(&self.paths, "openai")?;
        Ok(Arc::new(OpenAiSource::new(self.clone())?))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit test fixtures")]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    use plakous::openai::{
        CredentialReference, EndpointOrigin, OpenAiAuthError, PlatformCredentialSource,
    };

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
                source.generation_grant(&source.openai_reference, &origin),
                Ok(_) | Err(OpenAiAuthError::SourceFailure)
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
                source.generation_grant(&source.openai_reference, &origin),
                Err(OpenAiAuthError::BindingMismatch)
            ));
        }
        let origin = EndpointOrigin::parse("https://api.openai.com/").unwrap();
        let reference = CredentialReference::application("plak/other").unwrap();
        assert!(matches!(
            source.generation_grant(&reference, &origin),
            Err(OpenAiAuthError::BindingMismatch)
        ));
    }
}
