use super::{Arc, ProductPlatformCredentials};
use plakous_hosted::CliResult;
impl plakous::xai::XaiPlatformApiKeySource for ProductPlatformCredentials {
    fn grant(
        &self,
    ) -> Result<plakous::xai::XaiPlatformCredentialGrant, plakous::xai::XaiPlatformBuildError> {
        let key = self
            .key("xai-api")
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
            .key("xai-api")
            .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?;
        let key = plakous::xai::XaiPlatformApiKey::new(key)?;
        if &key.grant(authority)? != expected {
            return Err(plakous::xai::XaiPlatformBuildError::Authentication);
        }
        Ok(key)
    }
}

impl plakous_hosted::platform::XaiCredentials for ProductPlatformCredentials {
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
        crate::home::secrets::require_api_key(&self.paths, "xai-api")?;
        Ok(Arc::new(self.clone()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit test fixtures")]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    #[test]
    fn xai_api_key_file_reaches_grant_and_bearer() {
        use plakous::xai::XaiPlatformApiKeySource;
        let directory = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(directory.path().to_owned());
        paths.ensure_layout().unwrap();
        // An exported XAI_API_KEY takes precedence; either source must resolve.
        std::fs::write(paths.api_key_file("xai"), "xai-test-key\n").unwrap();
        let source = ProductPlatformCredentials::shared(&paths);
        let grant = XaiPlatformApiKeySource::grant(&*source).unwrap();
        XaiPlatformApiKeySource::api_key(&*source, &grant).unwrap();
    }
}
