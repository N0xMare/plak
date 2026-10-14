//! Product-owned API-key sources. Every attempt resolves the selected key anew.

use std::sync::Arc;

use plakous::openai::{
    CredentialAuthorityId, CredentialReference, EndpointOrigin, OpenAiApiKey,
    OpenAiApiKeyGenerationGrant, OpenAiAuthError, PlatformCredentialSource,
    StaticPlatformCredentialSource,
};
use plakous_hosted::error::{CliFailure, CliResult};

use crate::home::paths::PlakPaths;

#[derive(Clone)]
pub(crate) struct ProductPlatformCredentials {
    paths: PlakPaths,
    openai_reference: CredentialReference,
    openai_authority: CredentialAuthorityId,
}

impl plakous_hosted::platform::PlatformCredentials for ProductPlatformCredentials {
    fn openai(&self) -> CliResult<Arc<dyn PlatformCredentialSource>> {
        crate::home::secrets::require_api_key(&self.paths, "openai")?;
        Ok(Arc::new(self.clone()))
    }

    fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "anthropic")?;
        Ok(Arc::new(self.clone()))
    }

    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "xai-api")?;
        Ok(Arc::new(self.clone()))
    }
}

impl ProductPlatformCredentials {
    pub(crate) fn shared(paths: &PlakPaths) -> CliResult<Arc<Self>> {
        Ok(Arc::new(Self {
            paths: paths.clone(),
            openai_reference: CredentialReference::application("plak/openai")
                .map_err(|_| CliFailure::new("OpenAI credential reference is invalid"))?,
            openai_authority: CredentialAuthorityId::new("plak/openai/environment-or-file")
                .map_err(|_| CliFailure::new("OpenAI credential authority is invalid"))?,
        }))
    }

    fn key(&self, account: &str) -> Result<String, String> {
        crate::home::secrets::resolve_api_key(&self.paths, account)?
            .ok_or_else(|| format!("{account} API key is unavailable"))
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

impl PlatformCredentialSource for ProductPlatformCredentials {
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

impl plakous::xai::XaiPlatformApiKeySource for ProductPlatformCredentials {
    fn grant(
        &self,
    ) -> Result<plakous::xai::XaiPlatformCredentialGrant, plakous::xai::XaiPlatformBuildError> {
        let key = self
            .key("xai")
            .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?;
        let authority = plakous::xai::XaiPlatformApiKeyAuthority::new(
            "plak/xai-api",
            "plak/xai/environment-or-file",
            1,
        )
        .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?;
        plakous::xai::XaiPlatformApiKey::new(key)?.grant(authority)
    }

    fn api_key(
        &self,
        expected: &plakous::xai::XaiPlatformCredentialGrant,
    ) -> Result<plakous::xai::XaiPlatformApiKey, plakous::xai::XaiPlatformBuildError> {
        let authority = plakous::xai::XaiPlatformApiKeyAuthority::new(
            "plak/xai-api",
            "plak/xai/environment-or-file",
            1,
        )
        .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?;
        if expected.authority() != &authority {
            return Err(plakous::xai::XaiPlatformBuildError::Authentication);
        }
        let key = self
            .key("xai")
            .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?;
        let key = plakous::xai::XaiPlatformApiKey::new(key)?;
        if &key.grant(authority)? != expected {
            return Err(plakous::xai::XaiPlatformBuildError::Authentication);
        }
        Ok(key)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit test fixtures")]
mod tests {
    use super::{PlakPaths, ProductPlatformCredentials};
    use plakous::openai::{
        CredentialReference, EndpointOrigin, OpenAiAuthError, PlatformCredentialSource,
    };

    #[test]
    fn official_openai_origin_reaches_credential_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let source =
            ProductPlatformCredentials::shared(&PlakPaths::explicit(directory.path().to_owned()))
                .unwrap();
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
        let source =
            ProductPlatformCredentials::shared(&PlakPaths::explicit(directory.path().to_owned()))
                .unwrap();
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
