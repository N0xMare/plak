//! The xAI API catalog against the platform route: every catalog-usable
//! effort constructs, and live facts below the reviewed row narrow it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixture construction"
)]
use plakous_model::*;
use plakous_xai::*;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

struct Source;
fn authority() -> XaiPlatformApiKeyAuthority {
    XaiPlatformApiKeyAuthority::new("fixture-account", "fixture-xai-key", 1).unwrap()
}
impl XaiPlatformApiKeySource for Source {
    fn grant(&self) -> Result<XaiPlatformCredentialGrant, XaiPlatformBuildError> {
        XaiPlatformApiKey::new("synthetic-xai-platform-key".to_owned())?.grant(authority())
    }
    fn api_key(
        &self,
        _expected: &XaiPlatformCredentialGrant,
    ) -> Result<XaiPlatformApiKey, XaiPlatformBuildError> {
        XaiPlatformApiKey::new("synthetic-xai-platform-key".to_owned())
    }
}

fn behavior(effort: ReasoningEffort) -> ProviderBehaviorConfiguration {
    ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Enabled(effort),
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
        replay: ReplayDisposition::StrictEncrypted,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(128).unwrap()),
        storage: ProviderStorageDisposition::DisabledOnWire,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new(XAI_PLATFORM_GROK_46_PROFILE).unwrap(),
            revision: ProviderProfileRevisionId::new(XAI_PLATFORM_EXECUTION_REVISION).unwrap(),
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::DurableRequired,
    }
}

/// Every (model, effort) the catalog marks usable constructs, `xhigh`
/// (grok-4.5/4.6/4.7) and `none` (grok-4.3) included.
#[test]
fn every_catalog_effort_constructs_on_the_xai_platform_route() {
    let listed = xai_api_bundled_listing().unwrap();
    let mut failures = Vec::new();
    let mut pairs = 0;
    for model in xai_api_models(&listed).unwrap() {
        if model.status() != &XaiApiModelStatus::Usable {
            continue;
        }
        for effort in model.efforts() {
            pairs += 1;
            let built =
                xai_platform_selection_for(model.id(), behavior(*effort)).and_then(|selection| {
                    XaiPlatformModel::new(
                        authority(),
                        Arc::new(Source),
                        NonZeroU64::new(200_000).unwrap(),
                        selection,
                        Arc::new(FixedClock::new(Timestamp::new(1_789_344_060, 0).unwrap())),
                    )
                });
            if let Err(error) = built {
                failures.push(format!("{} {effort:?}: {error:?}", model.id()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "catalog-usable efforts refused: {failures:#?}"
    );
    assert_eq!(pairs, 17, "4.5/4.6/4.7 publish four efforts, 4.3 five");
}

/// The live list publishes `context_length` and `reasoning_effort`. A live
/// value below the reviewed row narrows it, never is ignored.
#[test]
fn live_xai_facts_below_the_reviewed_row_narrow() {
    let listed = vec![XaiListedModel {
        id: "grok-4.7".to_owned(),
        created: 1_788_307_200,
        context_length: Some(256_000),
        long_context_threshold: Some(200_000),
        efforts: vec!["low".to_owned(), "medium".to_owned()],
    }];
    let model = xai_api_models(&listed).unwrap().remove(0);
    assert_eq!(model.status(), &XaiApiModelStatus::Usable);
    assert_eq!(model.context_length(), 256_000, "live window ignored");
    assert_eq!(
        model.efforts(),
        &[ReasoningEffort::Low, ReasoningEffort::Medium],
        "live effort ladder ignored"
    );
    assert!(model.max_input_tokens() + u64::from(model.max_tokens()) <= 256_000);
}

/// The route refuses an input window and output bound that together exceed
/// the model's reviewed context, before any credential is read.
#[test]
fn an_envelope_over_the_shared_context_is_refused() {
    let mut over = behavior(ReasoningEffort::Low);
    over.output_bound = OutputBoundDisposition::Enforced(NonZeroU32::new(128_000).unwrap());
    for (input, accepted) in [(372_000, true), (372_001, false), (500_000, false)] {
        let built = xai_platform_selection_for("grok-4.7", over.clone()).and_then(|selection| {
            XaiPlatformModel::new(
                authority(),
                Arc::new(Source),
                NonZeroU64::new(input).unwrap(),
                selection,
                Arc::new(FixedClock::new(Timestamp::new(1_789_344_060, 0).unwrap())),
            )
        });
        assert_eq!(
            built.is_ok(),
            accepted,
            "{input}: {:?}",
            built.as_ref().err()
        );
    }
    over.output_bound = OutputBoundDisposition::Enforced(NonZeroU32::new(128_001).unwrap());
    let built = xai_platform_selection_for("grok-4.7", over).and_then(|selection| {
        XaiPlatformModel::new(
            authority(),
            Arc::new(Source),
            NonZeroU64::new(1).unwrap(),
            selection,
            Arc::new(FixedClock::new(Timestamp::new(1_789_344_060, 0).unwrap())),
        )
    });
    assert!(matches!(built, Err(XaiPlatformBuildError::Configuration)));
}
