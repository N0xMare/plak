use super::{
    API_KEY_REQUEST, ApiKey, CredentialProviderTag, XaiPlatformApiKeyAuthority,
    XaiPlatformApiKeyRequest, XaiPlatformBuildError, XaiPlatformKeyGeneration,
    xai_api_key_generation,
};
use plakous_auth::api_key::{ApiKeyError, ApiKeyRequest, ApiKeySource, StaticApiKeySource};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ForeignAuthority {
    owner: XaiPlatformApiKeyAuthority,
    foreign: StaticApiKeySource<XaiPlatformApiKeyAuthority>,
    bearer_reads: AtomicUsize,
}

impl ApiKeySource<XaiPlatformApiKeyRequest, XaiPlatformApiKeyAuthority> for ForeignAuthority {
    fn authority(&self) -> &XaiPlatformApiKeyAuthority {
        &self.owner
    }
    fn generation_grant(
        &self,
        request: &dyn ApiKeyRequest,
    ) -> Result<XaiPlatformKeyGeneration, ApiKeyError> {
        self.foreign.generation_grant(request)
    }
    fn api_key(
        &self,
        request: &XaiPlatformApiKeyRequest,
        expected: &XaiPlatformKeyGeneration,
    ) -> Result<ApiKey, ApiKeyError> {
        self.bearer_reads.fetch_add(1, Ordering::SeqCst);
        self.foreign.api_key(request, expected)
    }
}

#[test]
fn another_sources_authority_is_refused_before_xai_bearer_resolution()
-> Result<(), Box<dyn std::error::Error>> {
    let source = ForeignAuthority {
        owner: XaiPlatformApiKeyAuthority::new("account", "source-owner", 1)?,
        foreign: StaticApiKeySource::for_key(
            ApiKey::new("synthetic-key")?,
            CredentialProviderTag::XaiPlatform,
            crate::XAI_PLATFORM_API_KEY_ORIGIN,
            None,
            XaiPlatformApiKeyAuthority::new("account", "foreign-owner", 1)?,
        ),
        bearer_reads: AtomicUsize::new(0),
    };
    let foreign = source.generation_grant(&API_KEY_REQUEST)?;
    assert_eq!(foreign.provider(), CredentialProviderTag::XaiPlatform);
    assert!(matches!(
        xai_api_key_generation(&source),
        Err(XaiPlatformBuildError::Authentication)
    ));
    assert_eq!(source.bearer_reads.load(Ordering::SeqCst), 0);
    Ok(())
}
