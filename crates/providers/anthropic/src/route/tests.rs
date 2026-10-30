use super::*;

#[test]
fn credential_debug_and_display_are_redacted() -> Result<(), AnthropicError> {
    let sentinel = "sk-ant-sentinel-never-print";
    // AnthropicApiKey is intentionally non-`Clone` (secrecy hygiene):
    // independently wrapping the same text creates a new authority token.
    let key = AnthropicApiKey::new(sentinel)?;
    assert!(!format!("{key:?} {key}").contains(sentinel));
    let route = AnthropicPlatformRoute::new(key)?;
    assert!(!format!("{route:?}").contains(sentinel));

    let independently_wrapped = AnthropicPlatformRoute::new(AnthropicApiKey::new(sentinel)?)?;
    assert_ne!(route.account(), independently_wrapped.account());
    assert_ne!(
        route.auth().credential_authority(),
        independently_wrapped.auth().credential_authority()
    );
    Ok(())
}

#[test]
fn api_replay_account_tracks_key_generation_without_changing_the_selection_label()
-> Result<(), AnthropicError> {
    let route = |key| {
        AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new(key)?,
            AnthropicAccountIdentity::new("stable-account")?,
        )
    };
    let first = route("offline-one")?;
    let reconstructed = route("offline-one")?;
    let rotated = route("offline-two")?;
    assert_eq!(first.account(), rotated.account());
    assert_eq!(
        first.replay_account_binding()?,
        reconstructed.replay_account_binding()?
    );
    assert_ne!(
        first.replay_account_binding()?,
        rotated.replay_account_binding()?
    );
    Ok(())
}

#[test]
fn credential_authority_is_exact_and_independent_of_stable_account_identity()
-> Result<(), AnthropicError> {
    let account = AnthropicAccountIdentity::new("stable-account")?;
    let first = AnthropicPlatformRoute::for_account(
        AnthropicApiKey::new("sk-ant-generation-one")?,
        account.clone(),
    )?;
    let rotated = AnthropicPlatformRoute::for_account(
        AnthropicApiKey::new("sk-ant-generation-two")?,
        account,
    )?;
    assert_eq!(first.account(), rotated.account());
    assert_ne!(
        first.auth().credential_authority(),
        rotated.auth().credential_authority()
    );
    Ok(())
}

#[cfg(feature = "native-auth")]
#[test]
fn oauth_access_token_rotation_retains_prepared_credential_authority() -> Result<(), AnthropicError>
{
    let route = AnthropicPlatformRoute::claude_oauth(
        AnthropicApiKey::new("oauth-access-token-one")?,
        AnthropicAccountIdentity::new("grant-fixture-authority")?,
    )?;
    let rotated = route.with_oauth_access_token("oauth-access-token-two")?;
    assert!(rotated.is_claude_oauth());
    assert_eq!(route.id(), rotated.id());
    assert_eq!(route.account(), rotated.account());
    assert_eq!(
        route.auth().credential_authority(),
        rotated.auth().credential_authority()
    );
    assert!(Arc::ptr_eq(
        &route.auth.key.credential_authority,
        &rotated.auth.key.credential_authority
    ));
    assert_ne!(route.auth.key.expose(), rotated.auth.key.expose());
    Ok(())
}

#[cfg(feature = "native-auth")]
#[test]
fn oauth_access_token_rotation_rejects_platform_and_empty_tokens() -> Result<(), AnthropicError> {
    let platform = AnthropicPlatformRoute::new(AnthropicApiKey::new("sk-ant-platform")?)?;
    assert!(
        platform
            .with_oauth_access_token("oauth-access-token")
            .is_err()
    );
    let oauth = AnthropicPlatformRoute::claude_oauth(
        AnthropicApiKey::new("oauth-access-token-one")?,
        AnthropicAccountIdentity::new("grant-fixture-authority")?,
    )?;
    assert!(oauth.with_oauth_access_token("").is_err());
    assert!(oauth.with_oauth_access_token("oauth\naccess").is_err());
    Ok(())
}
