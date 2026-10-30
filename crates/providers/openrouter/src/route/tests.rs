use super::{
    ApiKey, OpenRouterApiKeySelection, OpenRouterAuthError, OpenRouterCredentialBinding,
    OpenRouterCredentialRequest, OpenRouterKeyGeneration, StaticOpenRouterKeySource,
    credential_generation, resolve_key, static_openrouter_api_key_source,
};
use plakous_auth::api_key::{ApiKeyError, ApiKeyRequest, ApiKeySource};
use plakous_model::{Digest, DigestDomain};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ForeignAuthority {
    owner: OpenRouterCredentialBinding,
    foreign: StaticOpenRouterKeySource,
    bearer_reads: AtomicUsize,
}

impl ApiKeySource<OpenRouterCredentialRequest<'_>, OpenRouterCredentialBinding>
    for ForeignAuthority
{
    fn authority(&self) -> &OpenRouterCredentialBinding {
        &self.owner
    }
    fn generation_grant(
        &self,
        request: &dyn ApiKeyRequest,
    ) -> Result<OpenRouterKeyGeneration, ApiKeyError> {
        self.foreign.generation_grant(request)
    }
    fn api_key(
        &self,
        request: &OpenRouterCredentialRequest<'_>,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, ApiKeyError> {
        self.bearer_reads.fetch_add(1, Ordering::SeqCst);
        self.foreign.api_key(request, expected)
    }
}

#[test]
fn another_sources_authority_is_refused_before_openrouter_bearer_resolution()
-> Result<(), Box<dyn std::error::Error>> {
    let source = ForeignAuthority {
        owner: OpenRouterCredentialBinding::platform("account", "source-owner")?,
        foreign: static_openrouter_api_key_source(
            ApiKey::new("synthetic-key")?,
            "account",
            "foreign-owner",
        )?,
        bearer_reads: AtomicUsize::new(0),
    };
    assert!(matches!(
        credential_generation(&source, &OpenRouterApiKeySelection::platform()),
        Err(OpenRouterAuthError::BindingMismatch)
    ));
    let operation = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[b"catalog-operation"],
    )?;
    let request = OpenRouterCredentialRequest::catalog(&operation);
    let grant = source.generation_grant(&request)?;
    assert!(matches!(
        resolve_key(&source, &request, &grant),
        Err(OpenRouterAuthError::BindingMismatch)
    ));
    assert_eq!(source.bearer_reads.load(Ordering::SeqCst), 0);
    Ok(())
}
