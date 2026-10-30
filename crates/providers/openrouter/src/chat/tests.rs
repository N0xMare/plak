use super::*;
use plakous_attempt::{PhysicalRequestOutcome, TerminalFacts};
use plakous_model::{ModelErrorContext, ModelEvent, ModelRetryRisk, ModelRetryability};
use std::num::NonZeroU32;

use crate::discovery::{AdmissionWire, catalog_snapshot_identity};

fn admission_fixture() -> Result<OpenRouterChatModelEvidence, Box<dyn std::error::Error>> {
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(crate::static_openrouter_api_key_source(
            crate::ApiKey::new("offline-admission-key")?,
            "offline-account",
            "offline-authority",
        )?),
        crate::OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)));
    let now =
        plakous_model::FixedClock::new(Timestamp::new(OPENROUTER_CHAT_REVIEWED_AT + 60, 0)?).now();
    Ok(OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        route.identity(),
        OpenRouterChatDiscovery {
            endpoints: vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/first".to_owned(),
                provider_name: "First".to_owned(),
                supported_parameters: BTreeSet::from(
                    ["tools", "max_tokens", "tool_choice", "reasoning", "stop"].map(str::to_owned),
                ),
                context_length: Some(131_072),
                max_prompt_tokens: Some(100_000),
                max_completion_tokens: Some(65_536),
                tool_choice: OpenRouterToolChoiceModes {
                    auto: true,
                    ..OpenRouterToolChoiceModes::default()
                },
                supports_implicit_caching: false,
            }],
            model_supported_parameters: BTreeSet::from(
                ["tools", "max_tokens", "tool_choice", "reasoning", "stop"].map(str::to_owned),
            ),
            reasoning: Some(OpenRouterReasoningFacts {
                supported_efforts: Some(vec!["medium".to_owned()]),
                default_effort: Some("medium".to_owned()),
                default_enabled: true,
                supports_max_tokens: false,
                mandatory: false,
            }),
            context_length: 131_072,
            image_input: false,
            observed_at: now,
            expires_at: Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
        },
    )?)
}

#[test]
fn admission_projection_ignores_endpoint_names_order_and_irrelevant_parameters()
-> Result<(), Box<dyn std::error::Error>> {
    let base = admission_fixture()?;
    for wire in [
        AdmissionWire::Chat,
        AdmissionWire::Responses,
        AdmissionWire::Messages,
    ] {
        let identity = base.admission_identity(wire)?;
        let mut added = base.clone();
        let mut endpoint = added.endpoints()[0].clone();
        endpoint.endpoint = "provider/second".to_owned();
        endpoint.provider_name = "Second".to_owned();
        endpoint
            .supported_parameters
            .insert("future_parameter".to_owned());
        added.endpoints.push(endpoint);
        added
            .model_supported_parameters
            .insert("future_parameter".to_owned());
        assert_eq!(identity, added.admission_identity(wire)?);
        added.endpoints.reverse();
        assert_eq!(identity, added.admission_identity(wire)?);
        added.endpoints.remove(1);
        assert_eq!(identity, added.admission_identity(wire)?);
        assert_ne!(
            catalog_snapshot_identity(
                &base.route_fingerprint,
                base.model().as_str(),
                base.context_length(),
                base.supported_parameters(),
                base.reasoning(),
                base.image_input(),
                base.endpoints()
            )?,
            catalog_snapshot_identity(
                &base.route_fingerprint,
                added.model().as_str(),
                added.context_length(),
                added.supported_parameters(),
                added.reasoning(),
                added.image_input(),
                added.endpoints()
            )?
        );
    }
    Ok(())
}

struct AdmissionRow {
    label: &'static str,
    before: OpenRouterChatModelEvidence,
    after: OpenRouterChatModelEvidence,
    unchanged: Option<AdmissionWire>,
}

fn admission_fact_rows(
    base: &OpenRouterChatModelEvidence,
) -> Result<Vec<AdmissionRow>, Box<dyn std::error::Error>> {
    let mut changed = Vec::new();
    let mut value = base.clone();
    value.context_length = 131_070;
    changed.push(("model context", value));
    let mut value = base.clone();
    value.model = ModelId::new("vendor/other")?;
    changed.push(("model id", value));
    let mut value = base.clone();
    value.model_supported_parameters.remove("max_tokens");
    changed.push(("model required parameter", value));
    let mut value = base.clone();
    value.model_supported_parameters.remove("stop");
    changed.push(("model wire parameter", value));
    for fact in [
        "efforts",
        "default_effort",
        "default_enabled",
        "supports_max_tokens",
        "mandatory",
    ] {
        let mut value = base.clone();
        let reasoning = value.reasoning.as_mut().ok_or("reasoning")?;
        match fact {
            "efforts" => {
                reasoning.supported_efforts = Some(vec!["low".to_owned(), "medium".to_owned()]);
            }
            "default_effort" => reasoning.default_effort = None,
            "default_enabled" => reasoning.default_enabled = false,
            "supports_max_tokens" => reasoning.supports_max_tokens = true,
            _ => reasoning.mandatory = true,
        }
        changed.push((fact, value));
    }
    for fact in [
        "prompt",
        "completion",
        "context",
        "tools",
        "max_tokens",
        "reasoning",
        "stop",
        "caching",
        "output rung",
        "safe input",
    ] {
        let mut value = base.clone();
        let endpoint = &mut value.endpoints[0];
        match fact {
            "prompt" => endpoint.max_prompt_tokens = Some(99_000),
            "completion" => endpoint.max_completion_tokens = Some(40_000),
            "context" => endpoint.context_length = Some(120_000),
            "caching" => endpoint.supports_implicit_caching = true,
            "output rung" => endpoint.max_completion_tokens = Some(16_384),
            "safe input" => endpoint.max_prompt_tokens = Some(32_768),
            parameter => {
                endpoint.supported_parameters.remove(parameter);
            }
        }
        changed.push((fact, value));
    }
    Ok(changed
        .into_iter()
        .map(|(label, after)| AdmissionRow {
            label,
            before: base.clone(),
            after,
            unchanged: matches!(label, "stop" | "model wire parameter")
                .then_some(AdmissionWire::Responses),
        })
        .collect())
}

fn admission_tool_choice_rows(base: &OpenRouterChatModelEvidence) -> Vec<AdmissionRow> {
    let mut rows = Vec::new();
    for fact in ["auto", "none", "required", "function", "tool_choice"] {
        let mut value = base.clone();
        let endpoint = &mut value.endpoints[0];
        match fact {
            "auto" => endpoint.tool_choice.auto = false,
            "none" => endpoint.tool_choice.none = true,
            "required" => endpoint.tool_choice.required = true,
            "function" => endpoint.tool_choice.function = true,
            _ => {
                endpoint.supported_parameters.remove("tool_choice");
            }
        }
        rows.push(AdmissionRow {
            label: fact,
            before: base.clone(),
            after: value,
            unchanged: Some(AdmissionWire::Chat),
        });
    }
    rows
}

fn admission_correlated_row(base: &OpenRouterChatModelEvidence) -> AdmissionRow {
    let mut paired = base.clone();
    let mut second = paired.endpoints[0].clone();
    second.endpoint = "provider/second".to_owned();
    second.supported_parameters.remove("stop");
    second.supports_implicit_caching = true;
    paired.endpoints.push(second);
    let mut crossed = paired.clone();
    crossed.endpoints[0].supports_implicit_caching = true;
    crossed.endpoints[1].supports_implicit_caching = false;
    AdmissionRow {
        label: "per-endpoint parameter/behaviour correlation",
        before: paired,
        after: crossed,
        unchanged: Some(AdmissionWire::Responses),
    }
}

#[test]
fn admission_projection_rotates_on_each_model_and_endpoint_fact()
-> Result<(), Box<dyn std::error::Error>> {
    let base = admission_fixture()?;
    let mut rows = admission_fact_rows(&base)?;
    rows.extend(admission_tool_choice_rows(&base));
    rows.push(admission_correlated_row(&base));
    for row in rows {
        for wire in [
            AdmissionWire::Chat,
            AdmissionWire::Responses,
            AdmissionWire::Messages,
        ] {
            let equal = matches!(
                (wire, row.unchanged),
                (AdmissionWire::Chat, Some(AdmissionWire::Chat))
                    | (AdmissionWire::Responses, Some(AdmissionWire::Responses))
            );
            assert_eq!(
                row.before.admission_identity(wire)? == row.after.admission_identity(wire)?,
                equal,
                "{}: {wire:?}",
                row.label
            );
        }
    }
    Ok(())
}

#[test]
fn chat_tool_omitting_envelopes_bind_every_endpoint_fact() -> Result<(), Box<dyn std::error::Error>>
{
    let base = admission_fixture()?;
    for reasoning in [false, true] {
        let mut added = base.clone();
        let mut endpoint = base.endpoints[0].clone();
        endpoint.endpoint = "provider/no-tools".to_owned();
        endpoint.supported_parameters = BTreeSet::from(["max_tokens".to_owned()]);
        if reasoning {
            endpoint.supported_parameters.insert("reasoning".to_owned());
        }
        endpoint.max_completion_tokens = Some(4_096);
        endpoint.max_prompt_tokens = Some(32_768);
        added.endpoints.push(endpoint);
        let identity = added.admission_identity(AdmissionWire::Chat)?;
        assert_ne!(base.admission_identity(AdmissionWire::Chat)?, identity);
        for fact in [
            "prompt",
            "completion",
            "context",
            "max_tokens",
            "reasoning",
            "stop",
            "caching",
        ] {
            let mut changed = added.clone();
            let endpoint = &mut changed.endpoints[1];
            match fact {
                "prompt" => endpoint.max_prompt_tokens = Some(30_000),
                "completion" => endpoint.max_completion_tokens = Some(5_000),
                "context" => endpoint.context_length = Some(120_000),
                "max_tokens" => {
                    endpoint.supported_parameters.remove("max_tokens");
                }
                "reasoning" => {
                    if reasoning {
                        endpoint.supported_parameters.remove("reasoning");
                    } else {
                        endpoint.supported_parameters.insert("reasoning".to_owned());
                    }
                }
                "stop" => {
                    endpoint.supported_parameters.insert("stop".to_owned());
                }
                _ => endpoint.supports_implicit_caching = true,
            }
            assert_ne!(
                identity,
                changed.admission_identity(AdmissionWire::Chat)?,
                "{fact}, reasoning={reasoning}"
            );
        }
    }
    // A weaker non-reasoning endpoint hides this change in the omitted
    // envelope; only the reasoning-on-wire tool-omitting envelope sees it.
    let mut paired = base.clone();
    let mut weaker = base.endpoints[0].clone();
    weaker.supported_parameters = BTreeSet::from(["max_tokens".to_owned()]);
    weaker.max_completion_tokens = Some(4_096);
    let mut reasoning = weaker.clone();
    reasoning
        .supported_parameters
        .insert("reasoning".to_owned());
    reasoning.max_completion_tokens = Some(8_192);
    paired.endpoints.extend([weaker, reasoning]);
    let identity = paired.admission_identity(AdmissionWire::Chat)?;
    paired.endpoints[2].max_completion_tokens = Some(16_384);
    assert_ne!(identity, paired.admission_identity(AdmissionWire::Chat)?);
    Ok(())
}

#[test]
fn chat_tool_omitting_endpoints_constrain_gateway_proofs() -> Result<(), Box<dyn std::error::Error>>
{
    let mut evidence = admission_fixture()?;
    let mut endpoint = evidence.endpoints[0].clone();
    endpoint.endpoint = "provider/no-tools".to_owned();
    endpoint.supported_parameters = BTreeSet::from(["max_tokens".to_owned()]);
    endpoint.max_completion_tokens = Some(4_096);
    endpoint.max_prompt_tokens = Some(30_000);
    evidence.endpoints.push(endpoint);
    assert_eq!(
        evidence
            .gateway_output_bound(OPENROUTER_CHAT_PROTOCOL_ID, false)?
            .get(),
        4_096
    );
    assert_eq!(
        evidence
            .gateway_output_bound(OPENROUTER_CHAT_PROTOCOL_ID, true)?
            .get(),
        32_768
    );
    assert_eq!(
        evidence.gateway_maximum_projected_input(
            OPENROUTER_CHAT_PROTOCOL_ID,
            false,
            NonZeroU32::new(4_096).ok_or("output")?
        )?,
        30_000
    );
    assert!(matches!(
        evidence.gateway_maximum_projected_input(
            OPENROUTER_CHAT_PROTOCOL_ID,
            false,
            NonZeroU32::new(32_768).ok_or("output")?
        ),
        Err(OpenRouterChatBuildError::UnprovenCompletionBound)
    ));
    evidence.endpoints[1].max_prompt_tokens = None;
    evidence.endpoints[1].context_length = None;
    assert!(matches!(
        evidence.gateway_maximum_projected_input(
            OPENROUTER_CHAT_PROTOCOL_ID,
            false,
            NonZeroU32::new(4_096).ok_or("output")?
        ),
        Err(OpenRouterChatBuildError::UnprovenPromptBound)
    ));
    evidence.endpoints[1].max_prompt_tokens = Some(30_000);
    evidence.endpoints[1]
        .supported_parameters
        .insert("reasoning".to_owned());
    assert_eq!(
        evidence
            .gateway_output_bound(OPENROUTER_CHAT_PROTOCOL_ID, true)?
            .get(),
        4_096
    );
    assert_eq!(
        evidence.gateway_maximum_projected_input(
            OPENROUTER_CHAT_PROTOCOL_ID,
            true,
            NonZeroU32::new(4_096).ok_or("output")?
        )?,
        30_000
    );
    Ok(())
}

#[test]
fn admission_projection_uses_the_complete_wire_eligibility_predicate()
-> Result<(), Box<dyn std::error::Error>> {
    let base = admission_fixture()?;
    let output = NonZeroU32::new(32_768).ok_or("output")?;
    for protocol in [
        OPENROUTER_CHAT_PROTOCOL_ID,
        crate::OPENROUTER_RESPONSES_PROTOCOL_ID,
        crate::OPENROUTER_MESSAGES_PROTOCOL_ID,
    ] {
        assert_eq!(
            base.gateway_maximum_projected_input(protocol, false, output)?,
            98_304
        );
        for parameter in ["tools", "max_tokens", "reasoning", "tool_choice"] {
            let mut changed = base.clone();
            changed.endpoints[0].supported_parameters.remove(parameter);
            let result =
                changed.gateway_maximum_projected_input(protocol, parameter == "reasoning", output);
            if parameter == "tool_choice" && protocol == OPENROUTER_CHAT_PROTOCOL_ID {
                assert_eq!(result?, 98_304, "Chat uses the automatic gateway default");
            } else {
                assert!(
                    matches!(result, Err(OpenRouterChatBuildError::UnboundedAdmission)),
                    "{protocol}: {parameter}"
                );
            }
        }
        let mut changed = base.clone();
        changed.endpoints[0].tool_choice.auto = false;
        let result = changed.gateway_maximum_projected_input(protocol, false, output);
        if protocol == crate::OPENROUTER_MESSAGES_PROTOCOL_ID {
            assert!(result.is_err());
        } else {
            assert_eq!(result?, 98_304);
        }
    }
    assert!(
        base.gateway_output_bound("foreign.protocol", false)
            .is_err()
    );
    Ok(())
}

#[test]
fn admission_projection_derivation_keeps_defaults_floor_and_all_endpoint_proofs()
-> Result<(), Box<dyn std::error::Error>> {
    for protocol in [
        OPENROUTER_CHAT_PROTOCOL_ID,
        crate::OPENROUTER_RESPONSES_PROTOCOL_ID,
        crate::OPENROUTER_MESSAGES_PROTOCOL_ID,
    ] {
        let base = admission_fixture()?;
        let bound = base.gateway_output_bound(protocol, false)?;
        assert_eq!(bound.get(), 32_768);
        let mut unknown = base.clone();
        unknown.endpoints[0].max_completion_tokens = None;
        assert_eq!(unknown.gateway_output_bound(protocol, false)?.get(), 16_384);
        let mut plain = unknown.clone();
        plain.endpoints[0].supported_parameters.remove("reasoning");
        assert_eq!(plain.gateway_output_bound(protocol, false)?.get(), 4_096);
        assert_eq!(unknown.gateway_output_bound(protocol, true)?.get(), 16_384);
        assert!(
            unknown
                .gateway_maximum_projected_input(protocol, false, bound)
                .is_err()
        );
        let mut constrained = base.clone();
        constrained.context_length = 1_000_000;
        constrained.endpoints[0].max_prompt_tokens = Some(100_000);
        constrained.endpoints[0].context_length = Some(100_000);
        assert_eq!(
            constrained.gateway_maximum_projected_input(protocol, false, bound)?,
            67_232
        );
        let mut large = base.clone();
        large.context_length = 1_000_000;
        large.endpoints[0].max_prompt_tokens = None;
        large.endpoints[0].context_length = Some(1_000_000);
        assert_eq!(
            large.gateway_maximum_projected_input(protocol, false, bound)?,
            967_232
        );
        let mut small = base.clone();
        small.context_length = 16_384;
        assert_eq!(
            small.gateway_maximum_projected_input(
                protocol,
                false,
                NonZeroU32::new(8_192).ok_or("output")?
            )?,
            8_192
        );
        assert_eq!(small.gateway_output_bound(protocol, false)?.get(), 8_192);
        small.context_length = 3_000;
        assert_eq!(small.gateway_output_bound(protocol, false)?.get(), 4_096);
        assert!(
            small
                .gateway_maximum_projected_input(
                    protocol,
                    false,
                    small.gateway_output_bound(protocol, false)?
                )
                .is_err()
        );
        let mut mixed = base.clone();
        let mut endpoint = mixed.endpoints[0].clone();
        endpoint.endpoint = "provider/second".to_owned();
        endpoint.max_prompt_tokens = Some(40_000);
        mixed.endpoints.push(endpoint);
        assert_eq!(
            mixed.gateway_maximum_projected_input(protocol, false, bound)?,
            40_000
        );
        mixed.endpoints[1].max_prompt_tokens = None;
        mixed.endpoints[1].context_length = Some(60_000);
        assert_eq!(
            mixed.gateway_maximum_projected_input(protocol, false, bound)?,
            27_232
        );
        mixed.endpoints[1].context_length = None;
        assert!(
            mixed
                .gateway_maximum_projected_input(protocol, false, bound)
                .is_err()
        );
        mixed.endpoints[1].max_prompt_tokens = Some(40_000);
        mixed.endpoints[1].max_completion_tokens = Some(128);
        assert!(
            mixed
                .gateway_maximum_projected_input(protocol, false, bound)
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn admission_projection_does_not_authorize_tampered_or_foreign_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let evidence = admission_fixture()?;
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(crate::static_openrouter_api_key_source(
            crate::ApiKey::new("offline-admission-key")?,
            "offline-account",
            "offline-authority",
        )?),
        crate::OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)));
    let now =
        plakous_model::FixedClock::new(Timestamp::new(OPENROUTER_CHAT_REVIEWED_AT + 60, 0)?).now();
    assert!(evidence.validates(route.identity(), now));
    let identity = evidence.admission_identity(AdmissionWire::Chat)?;
    assert_ne!(identity, evidence.catalog_snapshot_identity);
    let mut tampered = evidence.clone();
    tampered.endpoints[0].provider_name = "Unattested".to_owned();
    assert_eq!(identity, tampered.admission_identity(AdmissionWire::Chat)?);
    assert!(!tampered.validates(route.identity(), now));
    assert!(!evidence.validates(route.identity(), evidence.expires_at()));
    let foreign = OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(crate::static_openrouter_api_key_source(
            crate::ApiKey::new("offline-admission-key")?,
            "foreign-account",
            "offline-authority",
        )?),
        crate::OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)));
    assert!(!evidence.validates(foreign.identity(), now));
    let moved = OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(crate::static_openrouter_api_key_source(
            crate::ApiKey::new("offline-admission-key")?,
            "offline-account",
            "offline-authority",
        )?),
        crate::OpenRouterOrigin::new("https://127.0.0.1:18444")?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)));
    assert!(!evidence.validates(moved.identity(), now));
    Ok(())
}

#[test]
fn admission_projection_pins_every_envelope_and_derivation_input()
-> Result<(), Box<dyn std::error::Error>> {
    let evidence = admission_fixture()?;
    let identities = [
        AdmissionWire::Chat,
        AdmissionWire::Responses,
        AdmissionWire::Messages,
    ]
    .map(|wire| {
        evidence
            .admission_identity(wire)
            .map(|identity| identity.to_string())
    });
    let identities: Vec<_> = identities.into_iter().collect::<Result<_, _>>()?;
    assert_eq!(
        identities,
        [
            "f314755328e541f9c39e5826b03b3395270d9415fc8fb6f4596a347bac5d5a98",
            "e4b2fb777f9be495f723d05ecee0fb7a76b32a29962453fcc9eab4336fe867f2",
            "de12dc9d0dc7dad4a12e61abf43ea5db8172101dc6e72b134824ce31068c61f8"
        ]
    );
    Ok(())
}

#[test]
fn an_openrouter_context_overflow_body_is_classified_as_its_own_kind() {
    use plakous_attempt::TerminalFacts;
    assert!(body_reports_context_overflow(
        br#"{"error":{"code":400,"message":"This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens (139000 of text input, 1000 in the output)."}}"#
    ));
    assert!(body_reports_context_overflow(
        br#"{"error":{"code":400,"message":"Provider returned error","metadata":{"raw":"{\"error\":{\"code\":\"context_length_exceeded\"}}"}}}"#
    ));
    assert!(!body_reports_context_overflow(
        br#"{"error":{"code":400,"message":"No endpoints found that support the parameters requested."}}"#
    ));
    assert!(!body_reports_context_overflow(b"not json"));
    let facts = TerminalFacts::preacceptance_test();
    assert_eq!(
        context_overflow_kind(true, Some(facts)),
        Some(ModelErrorKind::ContextLimit)
    );
    assert_eq!(context_overflow_kind(false, Some(facts)), None);
}

#[tokio::test]
async fn joined_error_follows_prefix_queued_by_the_same_operation_poll()
-> Result<(), Box<dyn std::error::Error>> {
    use futures_util::StreamExt as _;
    use plakous_attempt::{ObservationSnapshot, ProviderAcceptance, TerminalFacts};
    use plakous_model::{AttemptId, CancellationToken, ModelCallContext, ResponseStarted};
    use std::str::FromStr as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let binding = ChatOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target"])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"operation"])?,
        plakous_attempt::ReplaySafety::CostOnly,
    );
    let prefix = ModelEvent::ResponseStarted(ResponseStarted::new(None, binding.plan()));
    let context = ModelCallContext::new(
        AttemptId::from_str("00000000-0000-4000-8000-000000000044")?,
        CancellationToken::new(),
        None,
    );
    let (mut attempt, receiver) =
        ChatAttempt::with_event_channel(binding, context, NonZeroUsize::new(4).ok_or("capacity")?);
    let completed = Arc::new(AtomicUsize::new(0));
    let operation_completed = Arc::clone(&completed);
    let operation = Box::pin(async move {
        attempt.events_mut().send(prefix).await?;
        let observation = ChatAttemptProtocolError::new("accepted stream failed");
        attempt.events_mut().send_error(observation.clone()).await?;
        let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: ProviderAcceptance::Accepted,
            visible_output: false,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        });
        operation_completed.fetch_add(1, Ordering::SeqCst);
        Err(ChatAttemptError::protocol(observation, facts))
    });
    let mut stream =
        JoinedStream::<ChatWire, _>::new(test_descriptor(), VecDeque::new(), operation, receiver);
    assert!(matches!(
        stream.next().await,
        Some(Ok(ModelEvent::ResponseStarted(_)))
    ));
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    let Some(Err(error)) = stream.next().await else {
        return Err("missing terminal error".into());
    };
    // Accepted, no hosted effect, nothing named: a definite failure that
    // states its duplicate risk.
    assert_eq!(error.kind(), ModelErrorKind::Protocol);
    assert_eq!(
        error.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling
        })
    );
    for _ in 0..3 {
        assert!(stream.next().await.is_none());
    }
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    Ok(())
}

fn assert_language_model<T: LanguageModel>() {}

#[test]
fn bounded_literals_have_no_syntactic_catalog_privilege() {
    assert!(concrete_model_id("vendor/new-model_2026.08"));
    assert!(concrete_model_id("nvidia/model-v3:free"));
    assert!(concrete_model_id("openrouter/auto"));
    assert!(concrete_model_id("vendor/model:beta"));
    assert!(concrete_model_id("vendor/model-latest"));
    assert!(!concrete_model_id("another-vendor/deep/model-v3"));
}

#[test]
fn routing_choices_are_explicit_and_never_protocol_fallback()
-> Result<(), Box<dyn std::error::Error>> {
    assert_language_model::<OpenRouterChatLanguageModel>();
    let strict = OpenRouterChatRoutingPolicy::StrictPinned(OpenRouterRoutingPolicy::strict(
        "vendor/endpoint-v1",
        "Vendor",
    )?);
    assert!(strict.pinned().is_some());
    assert!(!strict.allows_cross_protocol_fallback());
    let managed = OpenRouterChatRoutingPolicy::GatewayManaged;
    assert!(managed.pinned().is_none());
    assert!(!managed.allows_cross_protocol_fallback());
    assert_eq!(OPENROUTER_CHAT_COMPLETIONS_PATH, "/api/v1/chat/completions");
    assert_ne!(
        OPENROUTER_CHAT_PROTOCOL_ID,
        crate::OpenRouterDialect::StatelessResponsesBeta2026_07.protocol_id()
    );
    Ok(())
}

#[test]
fn sse_content_type_accepts_parameters_but_not_other_media_types() {
    assert!(is_sse_content_type("text/event-stream"));
    assert!(is_sse_content_type("Text/Event-Stream; charset=utf-8"));
    assert!(!is_sse_content_type("application/json"));
}

#[test]
fn openrouter_generation_header_is_primary_request_correlation() {
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("legacy-request"));
    headers.insert("request-id", HeaderValue::from_static("request"));
    headers.insert("x-generation-id", HeaderValue::from_static("generation"));
    assert_eq!(
        openrouter_request_id(|name| headers.get(name)),
        Some("generation")
    );
    headers.remove("x-generation-id");
    assert_eq!(
        openrouter_request_id(|name| headers.get(name)),
        Some("request")
    );
}

#[test]
fn initial_policy_and_rate_limits_reject_while_other_statuses_fail_closed() {
    for status in [StatusCode::BAD_REQUEST, StatusCode::INTERNAL_SERVER_ERROR] {
        assert_eq!(
            post_send_status_outcome(status),
            PhysicalRequestOutcome::AmbiguousPostSend
        );
    }
    assert_eq!(
        post_send_status_outcome(StatusCode::UNAUTHORIZED),
        PhysicalRequestOutcome::DefiniteRejection
    );
    assert_eq!(
        post_send_status_outcome(StatusCode::TOO_MANY_REQUESTS),
        PhysicalRequestOutcome::DefiniteRejection
    );
    assert_eq!(
        post_send_status_outcome(StatusCode::FORBIDDEN),
        PhysicalRequestOutcome::DefiniteRejection
    );
    assert!(!gateway_retention_is_explicit(None));
    let default = OpenRouterRetentionPolicy::gateway_default();
    assert_eq!(default.data_collection(), None);
    assert_eq!(default.zdr(), None);
    let strict = OpenRouterRetentionPolicy::strict();
    assert_eq!(strict.data_collection(), Some("deny"));
    assert_eq!(strict.zdr(), Some(true));
}

/// The `OpenRouter` Chat row of the provider-error matrix.
///
/// The columns are the ways a provider can fail, and every one of them must
/// be definite unless a hosted effect may have run. This route is the
/// hardest case for that rule: it commits HTTP 200 at acceptance and then
/// carries failures inside the stream, so acceptance can never separate
/// them.
#[test]
fn the_openrouter_chat_row_of_the_provider_failure_matrix_is_definite() {
    use plakous_attempt::ProviderRefusal as Refusal;

    for (name, refusal, expected) in [
        (
            "pre-stream 5xx",
            Refusal::Overloaded,
            ModelErrorKind::Unavailable,
        ),
        (
            "pre-stream 429",
            Refusal::RateLimit,
            ModelErrorKind::RateLimited,
        ),
        (
            "in-band 5xx after partial stream",
            Refusal::Overloaded,
            ModelErrorKind::Unavailable,
        ),
        (
            "in-band 429 after partial stream",
            Refusal::RateLimit,
            ModelErrorKind::RateLimited,
        ),
        (
            "transport timeout",
            Refusal::Deadline,
            ModelErrorKind::Timeout,
        ),
        (
            "operation deadline",
            Refusal::Deadline,
            ModelErrorKind::Timeout,
        ),
    ] {
        // Facts as the adapter reports them for an accepted stream that had
        // already emitted output: the live shape.
        let kind = named_refusal_kind(Some(refusal), None);
        assert_eq!(kind, Some(expected), "{name}");
        let kind = kind.unwrap_or(ModelErrorKind::Protocol);
        assert_ne!(kind, ModelErrorKind::OutcomeUnknown, "{name}");
        // The same column with a hosted effect in flight is the one thing
        // that stays undetermined, and the facts carrying it say so.
        assert_eq!(
            named_refusal_kind(Some(refusal), Some(hosted_effect_facts())),
            None,
            "{name} with a hosted effect must not be classified definitely"
        );
    }
    // A failure the gateway did not name keeps the conservative reading.
    assert_eq!(named_refusal_kind(None, None), None);
}

/// An initial `401` is an authentication rejection only while the facts
/// prove the provider never accepted the request and no hosted effect can
/// be in flight; the same status with possible hosted effects stays
/// undetermined.
#[test]
fn an_initial_401_is_authentication_only_without_acceptance_or_hosted_effects() {
    let rejected = ChatAttemptError::protocol(
        ChatAttemptProtocolError::new("OpenRouter Chat HTTP 401 (rejected-before-acceptance)")
            .with_http_status(401),
        TerminalFacts::preacceptance_test(),
    );
    assert_eq!(
        chat_model_error(&test_descriptor(), &rejected).kind(),
        ModelErrorKind::Authentication
    );
    let hosted = ChatAttemptError::protocol(
        ChatAttemptProtocolError::new("OpenRouter Chat HTTP 401 (post-acceptance)")
            .with_http_status(401),
        hosted_effect_facts(),
    );
    assert_eq!(
        chat_model_error(&test_descriptor(), &hosted).kind(),
        ModelErrorKind::OutcomeUnknown
    );
}

/// Terminal facts for an accepted exchange with a hosted effect in flight.
fn hosted_effect_facts() -> TerminalFacts {
    TerminalFacts::from_snapshot(plakous_attempt::ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
        visible_output: true,
        replay_safety: plakous_attempt::ReplaySafety::ServerEffect,
        hosted_effects_possible: true,
        logical_success: false,
    })
}

#[test]
fn openrouter_error_detail_takes_message_and_drops_secrets() {
    assert_eq!(
        openrouter_error_detail(
            br#"{"error":{"message":"No endpoints found that support the parameters requested."}}"#
        )
        .as_deref(),
        Some("No endpoints found that support the parameters requested.")
    );
    assert_eq!(
        openrouter_error_detail(
            br#"{"error":{"message":"Provider returned error","metadata":{"raw":"Unknown parameter: 'session_id'"}}}"#
        )
        .as_deref(),
        Some("Provider returned error: Unknown parameter: 'session_id'")
    );
    assert_eq!(
        openrouter_error_detail(br#"{"error":{"message":"Authorization Bearer sk-test"}}"#),
        None
    );
    assert_eq!(openrouter_error_detail(br"not-json"), None);
}

#[test]
fn reviewed_chat_facts_carry_a_review_due_date_and_no_expiry()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        OPENROUTER_CHAT_REVIEW_DUE_AT - OPENROUTER_CHAT_REVIEWED_AT,
        90 * 86_400
    );
    let capabilities = openrouter_chat_dialect_capabilities(&admission_fixture()?)?;
    assert!(!capabilities.features().is_empty());
    for (capability, support) in capabilities.features() {
        if support.provenance().source() == FactSource::BuiltIn {
            assert_eq!(support.provenance().expires_at(), None, "{capability:?}");
        }
    }
    Ok(())
}

#[test]
fn disabled_reasoning_wire_follows_openrouter_reasoning_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let default_on = OpenRouterReasoningFacts {
        supported_efforts: Some(vec!["medium".to_owned()]),
        default_effort: None,
        default_enabled: true,
        supports_max_tokens: false,
        mandatory: false,
    };
    let evidence = OpenRouterChatModelEvidence {
        model: ModelId::new("vendor/model")?,
        catalog_snapshot_identity: Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[b"x"],
        )?,
        route_fingerprint: "route".to_owned(),
        account_binding: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        credential_authority_binding: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"auth"])?,
        endpoints: vec![],
        model_supported_parameters: BTreeSet::default(),
        reasoning: Some(default_on),
        context_length: 1,
        image_input: false,
        observed_at: Timestamp::new(1, 0)?,
        expires_at: Timestamp::new(2, 0)?,
    };
    assert_eq!(
        openrouter_disabled_reasoning_wire(&evidence, plakous_model::ReasoningPolicy::Disabled),
        ChatDisabledReasoningWire::EnabledFalse
    );

    let mandatory = OpenRouterReasoningFacts {
        mandatory: true,
        default_enabled: true,
        supported_efforts: None,
        default_effort: None,
        supports_max_tokens: false,
    };
    let evidence = OpenRouterChatModelEvidence {
        reasoning: Some(mandatory),
        ..evidence
    };
    assert_eq!(
        openrouter_disabled_reasoning_wire(&evidence, plakous_model::ReasoningPolicy::Disabled),
        ChatDisabledReasoningWire::Omit
    );

    assert_eq!(
        openrouter_disabled_reasoning_wire(
            &OpenRouterChatModelEvidence {
                reasoning: None,
                ..evidence
            },
            plakous_model::ReasoningPolicy::Disabled
        ),
        ChatDisabledReasoningWire::Omit
    );
    Ok(())
}
