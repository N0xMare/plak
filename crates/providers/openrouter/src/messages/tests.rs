use crate::wire::{JoinedStream, retry_delay};
use plakous_messages::MessagesOperationBinding;

#[test]
fn long_retry_after_advice_saturates_instead_of_dropping() {
    use std::time::Duration;

    use plakous_model::ModelRetryDelay;

    assert_eq!(
        retry_delay(Duration::from_secs(172_800)).map(ModelRetryDelay::as_millis),
        Some(ModelRetryDelay::MAX_MILLIS)
    );
    assert_eq!(
        retry_delay(Duration::from_secs(7)).map(ModelRetryDelay::as_millis),
        Some(7_000)
    );
}

/// The `OpenRouter` Messages row of the provider-error matrix.
///
/// The same columns and the same rule as the Chat row, through this route's
/// own classifier. Both routes commit HTTP 200 at acceptance, so both had
/// the same defect and both needed the same fix.
#[test]
fn the_openrouter_messages_row_of_the_provider_failure_matrix_is_definite() {
    use plakous_attempt::ObservationSnapshot;
    use plakous_attempt::ProviderRefusal as Refusal;
    use plakous_messages::{MessagesAttemptProtocolError, TerminalFacts};

    let facts = |hosted: bool| {
        TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
            visible_output: true,
            replay_safety: if hosted {
                plakous_attempt::ReplaySafety::ServerEffect
            } else {
                plakous_attempt::ReplaySafety::CostOnly
            },
            hosted_effects_possible: hosted,
            logical_success: false,
        })
    };
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
        let error = MessagesAttemptError::protocol(
            MessagesAttemptProtocolError::new("OpenRouter Messages HTTP failure")
                .with_refusal(refusal),
            facts(false),
        );
        let projected = messages_model_error(&crate::chat::test_descriptor(), &error);
        assert_eq!(projected.kind(), expected, "{name}");
        assert_ne!(projected.kind(), ModelErrorKind::OutcomeUnknown, "{name}");

        // The one column that stays undetermined on every row.
        let hosted = MessagesAttemptError::protocol(
            MessagesAttemptProtocolError::new("OpenRouter Messages HTTP failure")
                .with_refusal(refusal),
            facts(true),
        );
        assert_eq!(
            messages_model_error(&crate::chat::test_descriptor(), &hosted).kind(),
            ModelErrorKind::OutcomeUnknown,
            "{name} with a hosted effect"
        );
    }

    // A break the gateway did not name is definite by its own kind.
    let unnamed = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new("OpenRouter Messages stream failed")
            .with_exchange_break(plakous_messages::ExchangeBreak::Transport),
        facts(false),
    );
    assert_eq!(
        messages_model_error(&crate::chat::test_descriptor(), &unnamed).kind(),
        ModelErrorKind::Transport
    );
}

#[test]
fn messages_context_overflow_is_typed() {
    use plakous_attempt::ObservationSnapshot;
    use plakous_messages::{MessagesAttemptProtocolError, TerminalFacts};

    let facts = |hosted: bool| {
        TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
            visible_output: false,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: hosted,
            logical_success: false,
        })
    };
    let overflow = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new("OpenRouter Messages model_context_window_exceeded")
            .with_context_overflow(),
        facts(false),
    );
    assert_eq!(
        messages_model_error(&crate::chat::test_descriptor(), &overflow).kind(),
        ModelErrorKind::ContextLimit
    );
}

/// A real `model_context_window_exceeded` stream has to pass through
/// `classify_stream_error`. A hand-built `with_context_overflow()` stays green
/// if that hook drops the mark.
#[test]
fn a_context_window_stream_reaches_context_limit_through_the_hook()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous_messages::MessagesHttpHooks;

    let stream = context_window_stream_error()?;
    assert!(stream.context_overflow(), "{stream}");
    let route = route()?;
    let target = OpenRouterMessagesTarget::gateway_managed(
        route.clone(),
        evidence(&route, 2, 3_600)?,
        context(&route, 4_096)?,
    )?;
    let hooks = OpenRouterMessagesHooks {
        target,
        clock: Arc::new(plakous_model::SystemClock),
        generation: route.current_credential_generation_grant()?,
    };
    let classified = MessagesHttpHooks::classify_stream_error(&hooks, stream);
    assert!(classified.context_overflow(), "{classified}");
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    assert_eq!(facts.physical_attempts(), 1);
    let attempt = MessagesAttemptError::protocol(classified, facts);
    assert_eq!(
        messages_model_error(hooks.target.descriptor(), &attempt).kind(),
        ModelErrorKind::ContextLimit
    );
    Ok(())
}
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::Poll;

use futures_util::{FutureExt, StreamExt};
use plakous_attempt::{ObservationSnapshot, TerminalFacts};
use plakous_model::{
    AttemptId, CancellationToken, CapacityCertificate, CapacityRule, ContextEvidenceSource,
    CountCertificateKind, EvidenceFreshness, IntegrationClass, MeasurementCertificate,
    ModelCallContext, ModelEvent, RuntimeModelAuthority,
};

use super::*;
use crate::wire::WireOperation;
use crate::{ApiKey, OpenRouterChatEndpointCandidate, OpenRouterOrigin, OpenRouterToolChoiceModes};

fn context_window_stream_error() -> Result<MessagesStreamError, Box<dyn std::error::Error>> {
    use plakous_messages::{
        MessagesRequestPolicy, MessagesRequestPolicyInput, MessagesStreamDialectHooks,
        MessagesStreamProjector, PreparedMessagesStreamBinding, ToolOwnership,
    };
    use plakous_model::{
        Digest, DigestDomain, EffectiveProviderConfigurationId, ModelId, ProtocolId,
        SelectedProviderConfigurationId, SystemClock, SystemIdGenerator,
    };

    let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"context-window-hook"])?;
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"effective"],
        )?)?)?;
    let mut projector = MessagesStreamProjector::new(
        PreparedMessagesStreamBinding::new(
            MessagesRequestPolicy::new(MessagesRequestPolicyInput {
                protocol: ProtocolId::new("anthropic.messages")?,
                requested_model: ModelId::new("claude-sonnet-5")?,
                expected_reported_model: None,
                descriptor_binding: digest,
                semantic_descriptor_binding: digest,
                account_binding: digest,
                selected_provider_configuration: selected,
                effective_provider_configuration: effective,
                maximum_output_tokens: 128,
                minimum_thinking_budget: None,
                api_version: "2023-06-01".into(),
                beta_header: "oauth-2025-04-20".into(),
            }),
            digest,
            ToolOwnership::default(),
            MessagesStreamDialectHooks::client_tools_only(),
        ),
        Arc::new(SystemClock),
        Arc::new(SystemIdGenerator),
    )?;
    match projector.push(CONTEXT_WINDOW_SSE.as_bytes()) {
        Err(error) => Ok(error),
        Ok(_) => Err("context window fails the stream".into()),
    }
}

const CONTEXT_WINDOW_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ctx\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"model_context_window_exceeded\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn route() -> Result<OpenRouterChatRoute, Box<dyn std::error::Error>> {
    let source = Arc::new(crate::static_openrouter_api_key_source(
        ApiKey::new("messages-test-key")?,
        "messages-test-account",
        "messages-test-authority",
    )?);
    Ok(OpenRouterChatRoute::new_local_fixture_with_retention(
        source,
        OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?))))
}

fn context(
    route: &OpenRouterChatRoute,
    limit: u64,
) -> Result<ContextContract, Box<dyn std::error::Error>> {
    let authority = RuntimeModelAuthority::authority_class_for(
        route.identity().account_binding(),
        route.identity().credential_authority_binding(),
    )?;
    let declaration = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[
            b"test-caller/openrouter-messages-context-v1",
            route.identity().fingerprint().as_bytes(),
            &limit.to_be_bytes(),
        ],
    )?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let revision = "test-openrouter-messages-context-v1";
    let capacity = CapacityCertificate::custom_envelope(
        "test-openrouter-messages-capacity",
        revision,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(limit),
            maximum_projected_input: limit,
            auto_compact_input: None,
        },
        authority,
        freshness,
        declaration,
        revision,
    )?;
    let measurement = MeasurementCertificate::new(
        "test-openrouter-messages-wire-bound",
        revision,
        "complete-lowered-messages-body-utf8-byte-upper-bound",
        revision,
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(authority),
        freshness,
    )?;
    Ok(ContextContract::new(
        ProtocolId::new(MESSAGES_FAMILY_ID)?,
        ACCOUNTING_REVISION,
        PROJECTION_REVISION,
        "openrouter-messages-output-reservation-v1",
        capacity,
        vec![measurement],
    )?)
}

fn evidence(
    route: &OpenRouterChatRoute,
    observed_offset: u64,
    expiry_offset: u64,
) -> Result<OpenRouterChatModelEvidence, Box<dyn std::error::Error>> {
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    OpenRouterChatModelEvidence::from_authenticated_discovery(
        plakous_model::ModelId::new("vendor/dynamic-model")?,
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/dynamic-model".to_owned(),
                provider_name: "Provider".to_owned(),
                supported_parameters: BTreeSet::from([
                    "tools".to_owned(),
                    "tool_choice".to_owned(),
                ]),
                context_length: Some(8_192),
                max_prompt_tokens: Some(4_096),
                max_completion_tokens: Some(4_096),
                tool_choice: OpenRouterToolChoiceModes::default(),
                supports_implicit_caching: false,
            }],
            model_supported_parameters: BTreeSet::from([
                "tools".to_owned(),
                "tool_choice".to_owned(),
            ]),
            reasoning: None,
            context_length: 8_192,
            image_input: false,
            observed_at: Timestamp::new(
                now.seconds().saturating_sub(observed_offset),
                now.nanoseconds(),
            )?,
            expires_at: Timestamp::new(now.seconds() + expiry_offset, now.nanoseconds())?,
        },
    )
    .map_err(Into::into)
}

#[test]
fn target_is_messages_family_custom_and_endpoint_neutral() -> Result<(), Box<dyn std::error::Error>>
{
    fn assert_language_model<T: LanguageModel>() {}
    assert_language_model::<OpenRouterMessagesLanguageModel>();
    let route = route()?;
    let target = OpenRouterMessagesTarget::gateway_managed(
        route.clone(),
        evidence(&route, 2, 3_600)?,
        context(&route, 4_096)?,
    )?;
    assert_eq!(
        target.descriptor().integration_class(),
        IntegrationClass::Custom
    );
    assert_eq!(
        target.descriptor().protocol().as_str(),
        OPENROUTER_MESSAGES_PROTOCOL_ID
    );
    assert_eq!(
        target
            .descriptor()
            .context_contract()
            .ok_or("context")?
            .protocol_family()
            .as_str(),
        MESSAGES_FAMILY_ID
    );
    assert_eq!(
        target
            .descriptor()
            .context_contract()
            .ok_or("context")?
            .projection_revision(),
        "openrouter-messages-canonical-v2"
    );
    assert_eq!(target.model().as_str(), "vendor/dynamic-model");
    assert_eq!(target.routing().endpoint_claim(), None);
    assert!(!target.routing().allows_cross_protocol_fallback());
    assert_eq!(OPENROUTER_MESSAGES_PATH, "/api/v1/messages");
    Ok(())
}

#[test]
fn target_rejects_route_without_retention_contract() -> Result<(), Box<dyn std::error::Error>> {
    let source = Arc::new(crate::static_openrouter_api_key_source(
        ApiKey::new("messages-no-retention-key")?,
        "messages-no-retention-account",
        "messages-no-retention-authority",
    )?);
    let route = OpenRouterChatRoute::new_local_fixture(
        source,
        OpenRouterOrigin::new("https://127.0.0.1:18443")?,
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)));
    let Err(error) = OpenRouterMessagesTarget::gateway_managed(
        route.clone(),
        evidence(&route, 2, 3_600)?,
        context(&route, 4_096)?,
    ) else {
        panic!("retention-less route must not admit Messages dispatch");
    };
    assert!(matches!(
        error,
        OpenRouterMessagesBuildError::MissingRetentionPolicy
    ));
    Ok(())
}

#[test]
fn freshness_only_refresh_preserves_messages_target_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let route = route()?;
    let contract = context(&route, 4_096)?;
    let first = OpenRouterMessagesTarget::gateway_managed(
        route.clone(),
        evidence(&route, 20, 3_600)?,
        contract.clone(),
    )?;
    let second = OpenRouterMessagesTarget::gateway_managed(
        route.clone(),
        evidence(&route, 10, 7_200)?,
        contract,
    )?;
    assert_eq!(first.target_identity(), second.target_identity());
    assert_eq!(first.descriptor(), second.descriptor());
    Ok(())
}

#[tokio::test]
async fn joined_stream_prefers_fact_bearing_post_acceptance_error()
-> Result<(), Box<dyn std::error::Error>> {
    let binding = MessagesOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target"])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"operation"])?,
        plakous_attempt::ReplaySafety::CostOnly,
    );
    let context = ModelCallContext::new(
        AttemptId::from_str("00000000-0000-4000-8000-000000000044")?,
        CancellationToken::new(),
        None,
    );
    let (mut attempt, receiver) = MessagesAttempt::with_event_channel(
        binding,
        context,
        NonZeroUsize::new(1).ok_or("capacity")?,
    );
    let observation = MessagesAttemptProtocolError::new("malformed accepted stream");
    attempt.events_mut().send_error(observation.clone()).await?;
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
        visible_output: true,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    let operation: WireOperation<MessagesWire> = Box::pin(futures_util::future::ready(Err(
        MessagesAttemptError::protocol(observation, facts),
    )));
    let mut stream = JoinedStream::<MessagesWire, _>::new(
        crate::chat::test_descriptor(),
        VecDeque::new(),
        operation,
        receiver,
    );

    let Some(Err(error)) = stream.next().await else {
        return Err("missing terminal error".into());
    };
    assert_eq!(error.kind(), ModelErrorKind::Protocol);
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn joined_error_follows_prefix_queued_by_the_same_operation_poll()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous_model::ResponseStarted;
    use std::sync::atomic::AtomicUsize;

    let binding = MessagesOperationBinding::new(
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
    let (mut attempt, receiver) = MessagesAttempt::with_event_channel(
        binding,
        context,
        NonZeroUsize::new(4).ok_or("capacity")?,
    );
    let completed = Arc::new(AtomicUsize::new(0));
    let operation_completed = Arc::clone(&completed);
    let operation = Box::pin(async move {
        attempt.events_mut().send(prefix).await?;
        let observation = MessagesAttemptProtocolError::new("accepted stream failed");
        attempt.events_mut().send_error(observation.clone()).await?;
        let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
            visible_output: false,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        });
        operation_completed.fetch_add(1, Ordering::SeqCst);
        Err(MessagesAttemptError::protocol(observation, facts))
    });
    let mut stream = JoinedStream::<MessagesWire, _>::new(
        crate::chat::test_descriptor(),
        VecDeque::new(),
        operation,
        receiver,
    );
    assert!(matches!(
        stream.next().await,
        Some(Ok(ModelEvent::ResponseStarted(_)))
    ));
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    let Some(Err(error)) = stream.next().await else {
        return Err("missing terminal error".into());
    };
    assert_eq!(error.kind(), ModelErrorKind::Protocol);
    for _ in 0..3 {
        assert!(stream.next().await.is_none());
    }
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn replay_strip_diagnostic_precedes_physical_operation_poll()
-> Result<(), Box<dyn std::error::Error>> {
    let binding = MessagesOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target-diagnostic"])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan-diagnostic"])?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"operation-diagnostic"],
        )?,
        plakous_attempt::ReplaySafety::CostOnly,
    );
    let context = ModelCallContext::new(
        AttemptId::from_str("00000000-0000-4000-8000-000000000045")?,
        CancellationToken::new(),
        None,
    );
    let (_attempt, receiver) = MessagesAttempt::with_event_channel(
        binding,
        context,
        NonZeroUsize::new(1).ok_or("capacity")?,
    );
    let polled = Arc::new(AtomicBool::new(false));
    let operation_polled = Arc::clone(&polled);
    let operation: WireOperation<MessagesWire> = Box::pin(std::future::poll_fn(move |_| {
        operation_polled.store(true, Ordering::SeqCst);
        Poll::Pending
    }));
    let mut stream = JoinedStream::<MessagesWire, _>::new(
        crate::chat::test_descriptor(),
        VecDeque::from([ModelDiagnostic::new(
            "incompatible Messages thinking replay was stripped",
        )]),
        operation,
        receiver,
    );

    let Some(Ok(ModelEvent::Diagnostic(diagnostic))) = stream.next().await else {
        return Err("missing replay-strip diagnostic".into());
    };
    assert_eq!(
        diagnostic.message().as_str(),
        "incompatible Messages thinking replay was stripped"
    );
    assert!(!polled.load(Ordering::SeqCst));
    assert!(stream.next().now_or_never().is_none());
    assert!(polled.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn chat_family_context_is_rejected_for_messages() -> Result<(), Box<dyn std::error::Error>> {
    let route = route()?;
    let mut contract = context(&route, 4_096)?;
    contract = ContextContract::new(
        ProtocolId::new("chat-completions-family")?,
        ACCOUNTING_REVISION,
        PROJECTION_REVISION,
        "openrouter-messages-output-reservation-v1",
        contract.capacity().clone(),
        contract.measurements().to_vec(),
    )?;
    assert!(
        OpenRouterMessagesTarget::gateway_managed(
            route.clone(),
            evidence(&route, 1, 3_600)?,
            contract,
        )
        .is_err()
    );
    Ok(())
}
