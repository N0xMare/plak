use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Request<'a> {
    provider: CredentialProviderTag,
    origin: &'a str,
    reference: Option<&'a str>,
}

impl ApiKeyRequest for Request<'_> {
    fn provider(&self) -> CredentialProviderTag {
        self.provider
    }

    fn origin(&self) -> &str {
        self.origin
    }

    fn credential_reference(&self) -> Option<&str> {
        self.reference
    }
}

#[test]
fn static_source_checks_provider_audience_reference_and_exact_generation() -> TestResult {
    let key = ApiKey::new("synthetic-api-key")?;
    let grant = key.generation_grant(CredentialProviderTag::OpenAiPlatform, "owner");
    let source = StaticApiKeySource::new(
        key,
        "https://api.example.invalid",
        Some("selected".into()),
        grant,
    );
    let request = Request {
        provider: CredentialProviderTag::OpenAiPlatform,
        origin: "https://api.example.invalid",
        reference: Some("selected"),
    };
    let admitted = source.generation_grant(&request)?;
    assert_eq!(admitted, grant);
    assert_eq!(
        source.api_key(&request, &grant)?.expose_secret(),
        "synthetic-api-key"
    );
    for refused in [
        Request {
            provider: CredentialProviderTag::XaiPlatform,
            ..request
        },
        Request {
            origin: "https://other.example.invalid",
            ..request
        },
        Request {
            reference: Some("other"),
            ..request
        },
    ] {
        assert_eq!(
            source.generation_grant(&refused),
            Err(ApiKeyError::BindingMismatch)
        );
        assert_eq!(
            source.api_key(&refused, &grant).err(),
            Some(ApiKeyError::BindingMismatch)
        );
    }
    let changed = ApiKeyGenerationGrant::from_generation_binding(
        CredentialProviderTag::OpenAiPlatform,
        "owner",
        b"rotated generation",
    )?;
    assert_eq!(
        source.api_key(&request, &changed).err(),
        Some(ApiKeyError::BindingMismatch)
    );
    let other_owner = ApiKeyGenerationGrant::from_generation_binding(
        CredentialProviderTag::OpenAiPlatform,
        "other owner",
        b"synthetic-api-key",
    )?;
    assert_eq!(
        source.api_key(&request, &other_owner).err(),
        Some(ApiKeyError::BindingMismatch)
    );
    Ok(())
}

#[test]
fn secrets_are_bounded_and_formatting_and_tokens_do_not_disclose_or_link_them() -> TestResult {
    for value in [
        String::new(),
        "key\nvalue".into(),
        "x".repeat(MAX_API_KEY_BYTES + 1),
    ] {
        assert_eq!(ApiKey::new(value).err(), Some(ApiKeyError::InvalidKey));
    }
    let key = ApiKey::new("synthetic-private-key")?;
    let independent = ApiKey::new("synthetic-private-key")?;
    assert_ne!(key.authority(), independent.authority());
    assert!(key.renew_with("\n").is_err());
    let retained = key.renew_with("synthetic-private-key")?;
    assert_eq!(key.authority(), retained.authority());
    let grant = key.generation_grant(CredentialProviderTag::OpenRouterPlatform, "private owner");
    let source = StaticApiKeySource::new(key, "https://api.example.invalid", None, grant);
    for debug in [
        format!("{retained:?}"),
        format!("{retained}"),
        format!("{grant:?}"),
        format!("{source:?}"),
    ] {
        assert!(!debug.contains("synthetic-private-key"));
        assert!(!debug.contains("private owner"));
        assert!(debug.contains("redacted"));
    }
    Ok(())
}

#[test]
fn an_owner_generation_nonce_is_not_replaced_with_a_hash_of_the_bearer() -> TestResult {
    let grant = ApiKeyGenerationGrant::from_generation_binding(
        CredentialProviderTag::OpenRouterPlatform,
        (),
        b"owner generation nonce",
    )?;
    let key = ApiKey::new("synthetic-bearer")?;
    assert_ne!(
        grant.fingerprint(),
        key.generation_grant(CredentialProviderTag::OpenRouterPlatform, ())
            .fingerprint()
    );
    let source = StaticApiKeySource::new(key, "https://router.example.invalid", None, grant);
    let request = Request {
        provider: CredentialProviderTag::OpenRouterPlatform,
        origin: "https://router.example.invalid",
        reference: None,
    };
    assert_eq!(source.generation_grant(&request)?, grant);
    assert_eq!(
        source.api_key(&request, &grant)?.expose_secret(),
        "synthetic-bearer"
    );
    Ok(())
}
