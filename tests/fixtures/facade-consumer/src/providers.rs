pub fn surface_is_nameable() {
    let review_expiries: [u64; 4] = [
        plakous::openai::OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS,
        plakous::xai::XAI_PLATFORM_REVIEW_EXPIRES_AT_UNIX_SECONDS,
        plakous::openrouter::OPENROUTER_CHAT_REVIEW_EXPIRES_AT,
        plakous::anthropic::ANTHROPIC_PLATFORM_ADMISSION_EXPIRES_AT_UNIX_SECONDS,
    ];
    assert!(review_expiries.into_iter().all(|expiry| expiry > 0));
    use plakous::anthropic::{AnthropicMessagesDialect, AnthropicPlatformModelConfig};
    use plakous::openai::{CodexSubscriptionRouteConfig, OpenAiLanguageModel};
    use plakous::openrouter::{OpenRouterChatClient, OpenRouterRetentionPolicy};
    use plakous::xai::{XaiGrokSubscriptionRoute, XaiLanguageModel};

    fn name<T: ?Sized>() {
        let _ = std::any::type_name::<T>();
    }
    name::<AnthropicMessagesDialect>();
    name::<AnthropicPlatformModelConfig>();
    let _: u32 = plakous::anthropic::claude_max_output_tokens("claude-sonnet-5");
    name::<CodexSubscriptionRouteConfig>();
    name::<OpenAiLanguageModel>();
    name::<OpenRouterChatClient>();
    let _ = OpenRouterChatClient::list_models;
    name::<OpenRouterRetentionPolicy>();
    name::<XaiGrokSubscriptionRoute>();
    name::<XaiLanguageModel>();
}

// Caller-owned labels and keys are supported; execution bindings remain provider-owned.
#[allow(dead_code)]
pub fn xai_platform_credential_source_surface() -> Result<(), Box<dyn std::error::Error>> {
    use plakous::xai::{XaiPlatformApiKey, XaiPlatformApiKeyAuthority};

    let authority = XaiPlatformApiKeyAuthority::new("fixture-account", "fixture-source", 1)?;
    let key = XaiPlatformApiKey::new("synthetic-compile-contract-key")?;
    let grant = key.grant(authority.clone())?;
    assert_eq!(grant.authority(), &authority);
    Ok(())
}
