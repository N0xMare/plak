use plakous_model::{FixedClock, ModelAdapterProfileId, ModelId};

use super::*;
use crate::ApiKey;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn config() -> TestResult<AnthropicPlatformModelConfig> {
    Ok(AnthropicPlatformModelConfig::new(
        ModelId::new(CLAUDE_SONNET_4_6_MODEL_ID)?,
        ModelFamilyId::new(CLAUDE_SONNET_4_FAMILY_ID)?,
        4_096,
        ModelAdapterProfileId::new(CLAUDE_SONNET_4_CLIENT_TOOLS_THINKING_PROFILE_ID)?,
        AnthropicPlatformCapabilityProfile::TextClientToolsAndThinking,
    )?)
}

#[test]
fn resolver_owns_route_provenance_freshness_and_conservative_capabilities() -> TestResult {
    let now = Timestamp::new(ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS + 60, 500)?;
    let resolver = AnthropicPlatformModelResolver::with_runtime(
        AnthropicPlatformRoute::new(ApiKey::new("sk-ant-resolver-sentinel")?)?,
        Arc::new(FixedClock::new(now)),
    );
    let target = resolver.resolve(config()?)?;
    let features = target.descriptor().capabilities().features();
    assert_eq!(features.len(), 5);
    assert!(features.contains_key(&Capability::ImageInput));
    assert_eq!(features[&Capability::Caching].support(), Support::Native);
    for support in features.values() {
        assert_eq!(support.support(), Support::Native);
        assert_eq!(support.provenance().source(), FactSource::BuiltIn);
        assert_eq!(
            support.provenance().observed_at(),
            Timestamp::new(ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS, 0)?
        );
        assert_eq!(support.provenance().expires_at(), None);
    }
    for capability in [
        Capability::ParallelTools,
        Capability::StructuredOutput,
        Capability::Continuation,
        Capability::NativeOptions,
    ] {
        assert!(features.get(&capability).is_none());
    }
    assert!(target.ensure_current_admission(now).is_ok());
    assert!(
        target
            .ensure_current_admission(Timestamp::new(
                ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_SECONDS,
                0
            )?)
            .is_ok()
    );
    assert!(!format!("{resolver:?} {target:?}").contains("sk-ant-resolver-sentinel"));
    Ok(())
}

#[test]
fn minimal_profile_advertises_reviewed_streaming_client_tools_images_and_caching() -> TestResult {
    let resolver = AnthropicPlatformModelResolver::with_runtime(
        AnthropicPlatformRoute::new(ApiKey::new("sk-ant-minimal-profile")?)?,
        Arc::new(FixedClock::new(Timestamp::new(
            ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS + 60,
            0,
        )?)),
    );
    let target = resolver.resolve(AnthropicPlatformModelConfig::new(
        ModelId::new(CLAUDE_SONNET_4_6_MODEL_ID)?,
        ModelFamilyId::new(CLAUDE_SONNET_4_FAMILY_ID)?,
        4_096,
        ModelAdapterProfileId::new(CLAUDE_SONNET_4_CLIENT_TOOLS_PROFILE_ID)?,
        AnthropicPlatformCapabilityProfile::TextAndClientTools,
    )?)?;
    let features = target.descriptor().capabilities().features();
    assert_eq!(features.len(), 4);
    assert!(features.contains_key(&Capability::Streaming));
    assert!(features.contains_key(&Capability::Tools));
    assert!(features.contains_key(&Capability::ImageInput));
    assert_eq!(features[&Capability::Caching].support(), Support::Native);
    for capability in [
        Capability::Reasoning,
        Capability::ParallelTools,
        Capability::StructuredOutput,
        Capability::Continuation,
        Capability::NativeOptions,
    ] {
        assert!(!features.contains_key(&capability));
    }
    Ok(())
}

#[test]
fn sonnet_5_base_profile_is_exact_and_non_beta() -> TestResult {
    let resolver = AnthropicPlatformModelResolver::with_runtime(
        AnthropicPlatformRoute::new(ApiKey::new("sk-ant-sonnet-5-offline")?)?,
        Arc::new(FixedClock::new(Timestamp::new(
            ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS + 60,
            0,
        )?)),
    );
    let config = AnthropicPlatformModelConfig::new(
        ModelId::new(CLAUDE_SONNET_5_MODEL_ID)?,
        ModelFamilyId::new(CLAUDE_SONNET_5_FAMILY_ID)?,
        CLAUDE_5_MAXIMUM_OUTPUT_TOKENS,
        ModelAdapterProfileId::new(CLAUDE_SONNET_5_CLIENT_TOOLS_PROFILE_ID)?,
        AnthropicPlatformCapabilityProfile::TextAndClientTools,
    )?;
    let target = resolver.resolve(config.clone())?;
    assert_eq!(target.model().maximum_output_tokens(), 128_000);
    assert_eq!(target.model().thinking(), ThinkingSupport::Disabled);
    assert_eq!(
        target.descriptor().reported_model(),
        Some(target.model().remote_id())
    );
    assert_eq!(target.dialect(), &AnthropicMessagesDialect::stable());

    let beta = config.with_dialect(AnthropicMessagesDialect::with_betas([
        crate::AnthropicBetaFeature::FineGrainedToolStreaming,
    ]));
    let error = resolver
        .resolve(beta)
        .err()
        .ok_or("Sonnet 5 beta dialect unexpectedly admitted")?;
    assert_eq!(error.kind(), AnthropicErrorKind::InvalidConfiguration);
    Ok(())
}

#[test]
fn unsupported_model_profile_and_overstated_ceiling_fail_closed() -> TestResult {
    let resolver = AnthropicPlatformModelResolver::with_runtime(
        AnthropicPlatformRoute::new(ApiKey::new("sk-ant-admission-negative")?)?,
        Arc::new(FixedClock::new(Timestamp::new(
            ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS + 1,
            0,
        )?)),
    );
    for unsupported in [
        AnthropicPlatformModelConfig::new(
            ModelId::new("claude-unreviewed")?,
            ModelFamilyId::new(CLAUDE_SONNET_4_FAMILY_ID)?,
            4_096,
            ModelAdapterProfileId::new(CLAUDE_SONNET_4_CLIENT_TOOLS_PROFILE_ID)?,
            AnthropicPlatformCapabilityProfile::TextAndClientTools,
        )?,
        AnthropicPlatformModelConfig::new(
            ModelId::new(CLAUDE_SONNET_4_6_MODEL_ID)?,
            ModelFamilyId::new(CLAUDE_SONNET_4_FAMILY_ID)?,
            4_096,
            ModelAdapterProfileId::new("anthropic.messages.unreviewed-profile.v1")?,
            AnthropicPlatformCapabilityProfile::TextAndClientTools,
        )?,
        AnthropicPlatformModelConfig::new(
            ModelId::new(CLAUDE_SONNET_4_6_MODEL_ID)?,
            ModelFamilyId::new(CLAUDE_SONNET_4_FAMILY_ID)?,
            4_096,
            ModelAdapterProfileId::new(CLAUDE_SONNET_4_CLIENT_TOOLS_THINKING_PROFILE_ID)?,
            AnthropicPlatformCapabilityProfile::TextAndClientTools,
        )?,
        AnthropicPlatformModelConfig::new(
            ModelId::new(CLAUDE_SONNET_4_6_MODEL_ID)?,
            ModelFamilyId::new(CLAUDE_SONNET_4_FAMILY_ID)?,
            REVIEWED_MAXIMUM_OUTPUT_TOKENS + 1,
            ModelAdapterProfileId::new(CLAUDE_SONNET_4_CLIENT_TOOLS_PROFILE_ID)?,
            AnthropicPlatformCapabilityProfile::TextAndClientTools,
        )?,
    ] {
        let error = resolver
            .resolve(unsupported)
            .err()
            .ok_or("unsupported Anthropic admission unexpectedly resolved")?;
        assert_eq!(error.kind(), AnthropicErrorKind::InvalidConfiguration);
    }
    Ok(())
}

#[test]
fn api_admission_never_refuses_on_the_calendar() -> TestResult {
    assert_eq!(
        ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_SECONDS
            - ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS,
        90 * 86_400
    );
    for seconds in [
        ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS - 86_400,
        ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_SECONDS,
        ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_SECONDS + 10 * 365 * 86_400,
    ] {
        let now = Timestamp::new(seconds, 0)?;
        let resolver = AnthropicPlatformModelResolver::with_runtime(
            AnthropicPlatformRoute::new(ApiKey::new("sk-ant-past-review")?)?,
            Arc::new(FixedClock::new(now)),
        );
        let target = resolver.resolve(config()?)?;
        assert!(target.ensure_current_admission(now).is_ok());
    }
    Ok(())
}
