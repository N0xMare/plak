use super::{
    ApiKey, ApiKeyGenerationGrant, ApiKeyRequest, ApiKeySource, AuthError, CredentialAuthorityId,
    CredentialReference, EndpointOrigin, OpenAiApiKeyRequest, OpenAiKeyGeneration,
    StaticOpenAiKeySource, platform_generation_grant, platform_key_for_generation,
    static_openai_api_key_source,
};
use plakous_auth::api_key::ApiKeyError;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ForeignAuthority {
    owner: CredentialAuthorityId,
    foreign: StaticOpenAiKeySource,
    bearer_reads: AtomicUsize,
}

impl ApiKeySource<OpenAiApiKeyRequest<'_>, CredentialAuthorityId> for ForeignAuthority {
    fn authority(&self) -> &CredentialAuthorityId {
        &self.owner
    }
    fn generation_grant(
        &self,
        request: &dyn ApiKeyRequest,
    ) -> Result<OpenAiKeyGeneration, ApiKeyError> {
        self.foreign.generation_grant(request)
    }
    fn api_key(
        &self,
        request: &OpenAiApiKeyRequest<'_>,
        expected: &ApiKeyGenerationGrant<CredentialAuthorityId>,
    ) -> Result<ApiKey, ApiKeyError> {
        self.bearer_reads.fetch_add(1, Ordering::SeqCst);
        self.foreign.api_key(request, expected)
    }
}

#[test]
fn another_sources_authority_is_refused_before_openai_bearer_resolution()
-> Result<(), Box<dyn std::error::Error>> {
    let reference = CredentialReference::application("authority-test")?;
    let origin = EndpointOrigin::parse(crate::OPENAI_PLATFORM_ORIGIN)?;
    let source = ForeignAuthority {
        owner: CredentialAuthorityId::new("source-owner")?,
        foreign: static_openai_api_key_source(
            ApiKey::new("synthetic-key")?,
            reference.clone(),
            CredentialAuthorityId::new("foreign-owner")?,
        ),
        bearer_reads: AtomicUsize::new(0),
    };
    assert!(matches!(
        platform_generation_grant(&source, &reference, &origin),
        Err(AuthError::BindingMismatch)
    ));
    let grant = source.generation_grant(&OpenAiApiKeyRequest::new(&reference, &origin))?;
    assert!(matches!(
        platform_key_for_generation(&source, &reference, &origin, &grant),
        Err(AuthError::BindingMismatch)
    ));
    assert_eq!(source.bearer_reads.load(Ordering::SeqCst), 0);
    Ok(())
}
