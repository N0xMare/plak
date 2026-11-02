use super::network_failure;
use std::collections::BTreeMap;

use plakous_messages::{
    MessagesAttemptError, MessagesAttemptProtocolError, ProviderAcceptance, TerminalFacts,
};
use plakous_model::{
    CapabilitySet, ModelAdapterProfileId, ModelErrorKind, ModelFamilyId, ModelHttpStatus, ModelId,
    ModelRetryDelay, ModelRetryability, ProtocolId, ResolvedModelDescriptor, RouteId,
};

use plakous_http::{
    ErrorKind as HttpErrorKind, Failure as HttpFailure, Phase as HttpPhase, TransmissionFact,
};

use super::{anthropic_model_error, classify_messages_attempt_failure, transport_error};

#[test]
fn proved_contract_drift_is_permanent_configuration_drift_not_a_login_or_request_retry()
-> Result<(), Box<dyn std::error::Error>> {
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.claude_oauth")?,
        ModelId::new("claude-sonnet-5")?,
        None,
        None,
        ModelAdapterProfileId::new("anthropic.claude.oauth.test")?,
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    );
    let message = "this build's Claude contract is out of date; install a newer build";
    let attempt = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new(message)
            .with_compatibility_drift()
            .with_http_status(ModelHttpStatus::new(400)?),
        TerminalFacts::preacceptance_test(),
    );
    let failure = anthropic_model_error(&descriptor, Some(&attempt), message);
    assert_eq!(failure.kind(), ModelErrorKind::InvalidConfiguration);
    assert_eq!(
        failure.context().ok_or("context absent")?.retryability(),
        ModelRetryability::Permanent
    );
    assert!(failure.to_string().contains("install a newer build"));
    Ok(())
}

#[test]
fn user_cancel_after_accept_is_cancelled_not_outcome_unknown() {
    for (acceptance, message) in [
        (
            Some(ProviderAcceptance::Accepted),
            "Messages attempt cancelled",
        ),
        (
            Some(ProviderAcceptance::Accepted),
            "anthropic/Cancelled status=None request_id=None: cancelled",
        ),
        (
            Some(ProviderAcceptance::PossiblyAccepted),
            "Messages attempt cancelled",
        ),
        (
            Some(ProviderAcceptance::NotAccepted),
            "Messages attempt cancelled",
        ),
        (None, "Messages attempt cancelled"),
    ] {
        let (kind, retryability) = classify_messages_attempt_failure(
            acceptance,
            false,
            None,
            None,
            None,
            plakous_messages::ExchangeBreak::Protocol,
            message,
        );
        assert_eq!(kind, ModelErrorKind::Cancelled, "{acceptance:?} {message}");
        assert_eq!(retryability, ModelRetryability::Permanent);
    }
}

/// A break nobody named, after acceptance and with no hosted effect
/// possible, is a definite failure of its own kind. The inference may have
/// run, so nothing may replay it, but nothing is left to reconcile either.
#[test]
fn an_accepted_break_without_a_hosted_effect_is_definite() {
    use plakous_messages::ExchangeBreak;
    for acceptance in [
        ProviderAcceptance::Accepted,
        ProviderAcceptance::PossiblyAccepted,
    ] {
        for (exchange_break, expected) in [
            (ExchangeBreak::Transport, ModelErrorKind::Transport),
            (ExchangeBreak::Protocol, ModelErrorKind::Protocol),
        ] {
            let (kind, retryability) = classify_messages_attempt_failure(
                Some(acceptance),
                false,
                None,
                None,
                None,
                exchange_break,
                "Messages stream omitted terminal response",
            );
            assert_eq!(kind, expected, "{acceptance:?} {exchange_break:?}");
            assert_eq!(
                retryability,
                ModelRetryability::Unsafe {
                    risk: plakous_model::ModelRetryRisk::DuplicateInferenceOrBilling,
                }
            );
        }
    }
}

#[test]
fn typed_auth_rejection_requires_definite_non_acceptance() {
    for (status, expected) in [
        (401, ModelErrorKind::Authentication),
        (403, ModelErrorKind::Authorization),
    ] {
        for acceptance in [
            ProviderAcceptance::NotAccepted,
            ProviderAcceptance::PossiblyAccepted,
            ProviderAcceptance::Accepted,
        ] {
            let (kind, retryability) = classify_messages_attempt_failure(
                Some(acceptance),
                false,
                None,
                Some(ModelHttpStatus::new(status).expect("status")),
                None,
                plakous_messages::ExchangeBreak::Protocol,
                "opaque provider rejection",
            );
            if acceptance == ProviderAcceptance::NotAccepted {
                assert_eq!(kind, expected);
                assert_eq!(retryability, ModelRetryability::Permanent);
            } else {
                assert_eq!(kind, ModelErrorKind::Protocol);
                assert!(matches!(retryability, ModelRetryability::Unsafe { .. }));
            }
        }
    }
}

#[test]
fn possible_hosted_effect_requires_explicit_recovery() {
    let (kind, retryability) = classify_messages_attempt_failure(
        Some(ProviderAcceptance::Accepted),
        true,
        None,
        None,
        None,
        plakous_messages::ExchangeBreak::Protocol,
        "Anthropic HTTP transport failed",
    );
    assert_eq!(kind, ModelErrorKind::OutcomeUnknown);
    assert_eq!(
        retryability,
        ModelRetryability::Unsafe {
            risk: plakous_model::ModelRetryRisk::ProviderHostedEffect,
        }
    );
}

#[test]
fn transport_failure_message_names_the_http_phase() {
    let phases = [
        (HttpPhase::Resolution, "resolution"),
        (HttpPhase::Connect, "connect"),
        (HttpPhase::Tls, "tls"),
        (HttpPhase::RequestWrite, "request_write"),
        (
            HttpPhase::RequestWriteOrResponseHead,
            "request_write_or_response_head",
        ),
        (HttpPhase::ResponseHead, "response_head"),
        (HttpPhase::ResponseBody, "response_body"),
        (HttpPhase::Cleanup, "cleanup"),
    ];
    for (phase, name) in phases {
        let deadline = transport_error(
            HttpFailure::for_classifier_test(
                phase,
                TransmissionFact::MayHaveTransmitted,
                HttpErrorKind::DeadlineExceeded,
            ),
            None,
        );
        let message = deadline.message();
        assert_eq!(
            message,
            network_failure(HttpFailure::for_classifier_test(
                phase,
                TransmissionFact::MayHaveTransmitted,
                HttpErrorKind::DeadlineExceeded
            ))
            .expect("known phase")
            .to_string(),
            "{name}"
        );

        let transport = transport_error(
            HttpFailure::for_classifier_test(
                phase,
                TransmissionFact::NotStarted,
                HttpErrorKind::Io,
            ),
            None,
        );
        let message = transport.message();
        assert_eq!(
            message,
            network_failure(HttpFailure::for_classifier_test(
                phase,
                TransmissionFact::NotStarted,
                HttpErrorKind::Io
            ))
            .expect("known phase")
            .to_string(),
            "{name}"
        );
        assert!(
            !message.contains("deadline"),
            "a transport failure must not claim a deadline: {message}"
        );
    }
    let cancelled = transport_error(
        HttpFailure::for_classifier_test(
            HttpPhase::Connect,
            TransmissionFact::NotStarted,
            HttpErrorKind::Cancelled,
        ),
        None,
    );
    assert!(cancelled.message().contains("was cancelled during connect"));
    let invalid = transport_error(
        HttpFailure::for_classifier_test(
            HttpPhase::ResponseHead,
            TransmissionFact::MayHaveTransmitted,
            HttpErrorKind::InvalidResponse,
        ),
        None,
    );
    assert!(
        invalid
            .message()
            .contains("response was invalid during response_head")
    );
    let limited = transport_error(
        HttpFailure::for_classifier_test(
            HttpPhase::RequestWrite,
            TransmissionFact::RequestIncomplete,
            HttpErrorKind::LimitExceeded(plakous_http::LimitKind::RequestBodyBytes),
        ),
        None,
    );
    assert!(
        limited
            .message()
            .contains("exceeded a resource bound during request_write")
    );
    assert!(!limited.message().contains("deadline"));
}

/// A mid-stream `rate_limit_error` on an accepted stream escalated to
/// `recovery_required` on both surfaces and blocked continuation. The
/// stream was accepted, so the old classification stopped at acceptance;
/// the provider had in fact named exactly why it produced nothing.
#[test]
fn a_named_refusal_that_produced_nothing_is_definite_however_it_was_accepted() {
    use plakous_messages::MessagesProviderRefusal;
    let retry_after = ModelRetryDelay::from_millis(1_000).expect("retry delay");
    for acceptance in [
        ProviderAcceptance::NotAccepted,
        ProviderAcceptance::PossiblyAccepted,
        ProviderAcceptance::Accepted,
    ] {
        for (refusal, expected) in [
            (
                MessagesProviderRefusal::RateLimit,
                ModelErrorKind::RateLimited,
            ),
            (
                MessagesProviderRefusal::Overloaded,
                ModelErrorKind::Unavailable,
            ),
            (MessagesProviderRefusal::Deadline, ModelErrorKind::Timeout),
        ] {
            let (kind, retryability) = classify_messages_attempt_failure(
                Some(acceptance),
                false,
                Some(refusal),
                None,
                Some(retry_after),
                plakous_messages::ExchangeBreak::Protocol,
                "Anthropic provider error",
            );
            assert_eq!(
                kind, expected,
                "{refusal:?} under {acceptance:?} names its own outcome"
            );
            assert_ne!(
                kind,
                ModelErrorKind::OutcomeUnknown,
                "a refusal that produced nothing leaves nothing to recover"
            );
            assert_eq!(
                retryability,
                if acceptance == ProviderAcceptance::NotAccepted {
                    ModelRetryability::Eligible {
                        retry_after: Some(retry_after),
                    }
                } else {
                    ModelRetryability::Unsafe {
                        risk: plakous_model::ModelRetryRisk::DuplicateInferenceOrBilling,
                    }
                }
            );
        }
    }
}

/// One column of the provider-error matrix.
///
/// The rows are routes and the columns are the ways a provider can fail;
/// this is the column, shared by every row so a new one is one line in the
/// table below. Each names the condition and the terminal facts the adapter
/// reports for it. Visible output is deliberately not among them: no
/// classifier reads it any more, so a column carrying it would suggest a
/// lever that does not exist.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderFailureColumn {
    pub(crate) name: &'static str,
    pub(crate) acceptance: ProviderAcceptance,
    pub(crate) hosted_effects: bool,
    pub(crate) refusal: Option<plakous_messages::MessagesProviderRefusal>,
    pub(crate) http_status: Option<u16>,
    /// `None` means the column must be definite without pinning which kind.
    pub(crate) expect: Option<ModelErrorKind>,
}

/// Every way a provider fails that this matrix covers.
///
/// Adding a column is one line. Each row of the matrix runs all of them and
/// asserts the same property: a provider failure is a definite outcome
/// unless a hosted effect may have run.
pub(crate) fn provider_failure_columns() -> Vec<ProviderFailureColumn> {
    use plakous_messages::MessagesProviderRefusal as Refusal;
    let column = |name, acceptance, refusal, http_status, expect| ProviderFailureColumn {
        name,
        acceptance,
        hosted_effects: false,
        refusal,
        http_status,
        expect,
    };
    vec![
        column(
            "pre-stream 5xx",
            ProviderAcceptance::NotAccepted,
            Some(Refusal::Overloaded),
            Some(503),
            Some(ModelErrorKind::Unavailable),
        ),
        column(
            "pre-stream 429",
            ProviderAcceptance::NotAccepted,
            Some(Refusal::RateLimit),
            Some(429),
            Some(ModelErrorKind::RateLimited),
        ),
        column(
            "in-band error after partial stream",
            ProviderAcceptance::Accepted,
            Some(Refusal::Overloaded),
            None,
            Some(ModelErrorKind::Unavailable),
        ),
        column(
            "in-band rate limit after partial stream",
            ProviderAcceptance::Accepted,
            Some(Refusal::RateLimit),
            None,
            Some(ModelErrorKind::RateLimited),
        ),
        column(
            "transport timeout",
            ProviderAcceptance::NotAccepted,
            Some(Refusal::Deadline),
            None,
            Some(ModelErrorKind::Timeout),
        ),
        column(
            "operation deadline on an accepted stream",
            ProviderAcceptance::Accepted,
            Some(Refusal::Deadline),
            None,
            Some(ModelErrorKind::Timeout),
        ),
        column(
            "overloaded refusal on an accepted stream",
            ProviderAcceptance::PossiblyAccepted,
            Some(Refusal::Overloaded),
            Some(529),
            Some(ModelErrorKind::Unavailable),
        ),
    ]
}

/// The Anthropic row of the provider-error matrix.
///
/// Anthropic API and Claude OAuth are one row: they differ in credential
/// and route identity, and share `classify_messages_attempt_failure`
/// exactly. Nothing in the classification reads the credential.
#[test]
fn the_anthropic_row_of_the_provider_failure_matrix_is_definite() {
    for column in provider_failure_columns() {
        let (kind, retryability) = classify_messages_attempt_failure(
            Some(column.acceptance),
            column.hosted_effects,
            column.refusal,
            column
                .http_status
                .and_then(|status| ModelHttpStatus::new(status).ok()),
            None,
            plakous_messages::ExchangeBreak::Protocol,
            "Anthropic provider error",
        );
        assert_ne!(
            kind,
            ModelErrorKind::OutcomeUnknown,
            "{}: a named provider failure with no hosted effect fences the session",
            column.name
        );
        assert_eq!(
            matches!(retryability, ModelRetryability::Unsafe { .. }),
            column.acceptance != ProviderAcceptance::NotAccepted,
            "{}: accepted inference cannot be replayed even when its terminal outcome is definite",
            column.name
        );
        if let Some(expected) = column.expect {
            assert_eq!(kind, expected, "{}", column.name);
        }
    }
}

/// The same columns with a hosted effect in flight stay undetermined, on
/// every row. That is the one thing that may have happened outside what
/// this client can see.
#[test]
fn every_column_with_a_hosted_effect_still_requires_recovery() {
    for column in provider_failure_columns() {
        let (kind, _) = classify_messages_attempt_failure(
            Some(column.acceptance),
            true,
            column.refusal,
            column
                .http_status
                .and_then(|status| ModelHttpStatus::new(status).ok()),
            None,
            plakous_messages::ExchangeBreak::Protocol,
            "Anthropic provider error",
        );
        assert_eq!(kind, ModelErrorKind::OutcomeUnknown, "{}", column.name);
    }
}

/// Only a hosted effect keeps a named refusal undetermined.
///
/// Visible output used to keep it too, and that was wrong in a way the
/// field proved: the live `OpenRouter` `520` arrived after a chunk had
/// already streamed, so a guard on visible output would not have fired on
/// the very case it was written for. Streamed text that is abandoned is
/// discarded with the turn and leaves no state anything could fail to
/// reconcile. A hosted effect is the one thing that would, and it is now
/// the only fact this classifier reads for the question — visible output is
/// no longer a parameter at all.
#[test]
fn only_a_hosted_effect_keeps_a_named_refusal_undetermined() {
    use plakous_messages::MessagesProviderRefusal;

    let (kind, _) = classify_messages_attempt_failure(
        Some(ProviderAcceptance::Accepted),
        false,
        Some(MessagesProviderRefusal::RateLimit),
        None,
        None,
        plakous_messages::ExchangeBreak::Protocol,
        "Anthropic provider error",
    );
    assert_eq!(
        kind,
        ModelErrorKind::RateLimited,
        "abandoned output does not make a named refusal unknown"
    );

    let (kind, _) = classify_messages_attempt_failure(
        Some(ProviderAcceptance::Accepted),
        true,
        Some(MessagesProviderRefusal::RateLimit),
        None,
        None,
        plakous_messages::ExchangeBreak::Protocol,
        "Anthropic provider error",
    );
    assert_eq!(
        kind,
        ModelErrorKind::OutcomeUnknown,
        "a hosted effect may have run outside what this client can see"
    );
}

#[test]
fn typed_429_retains_retry_after_without_message_parsing() {
    let retry_after = ModelRetryDelay::from_millis(2_000).expect("retry delay");
    let (kind, retryability) = classify_messages_attempt_failure(
        Some(ProviderAcceptance::NotAccepted),
        false,
        None,
        Some(ModelHttpStatus::new(429).expect("status")),
        Some(retry_after),
        plakous_messages::ExchangeBreak::Protocol,
        "opaque provider rejection",
    );
    assert_eq!(kind, ModelErrorKind::RateLimited);
    assert_eq!(
        retryability,
        ModelRetryability::Eligible {
            retry_after: Some(retry_after),
        }
    );
}

#[test]
fn retry_after_uses_the_shared_parser_and_saturates_long_advice() {
    use super::retry_after_advice_millis;
    use crate::{AnthropicError, AnthropicErrorKind, AnthropicErrorPhase};

    assert_eq!(retry_after_advice_millis(" 7 "), Some(7_000));
    assert_eq!(
        retry_after_advice_millis("Wed, 21 Oct 2015 07:28:00 GMT"),
        Some(0)
    );
    assert_eq!(
        retry_after_advice_millis("172800"),
        Some(ModelRetryDelay::MAX_MILLIS)
    );
    assert_eq!(retry_after_advice_millis("soon"), None);
    let error = AnthropicError::new(
        AnthropicErrorKind::RateLimited,
        AnthropicErrorPhase::Connect,
        "rate limited",
    )
    .with_retry_after_millis(Some(172_800_000));
    assert_eq!(
        error.retry_after_millis(),
        Some(ModelRetryDelay::MAX_MILLIS)
    );
}

#[test]
fn context_overflow_is_a_typed_model_error() {
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages").expect("protocol"),
        RouteId::new("anthropic.claude_oauth").expect("route"),
        ModelId::new("claude-sonnet-5").expect("model"),
        None,
        None,
        ModelAdapterProfileId::new("anthropic.claude.oauth.test").expect("profile"),
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    );
    let facts = |hosted: bool| {
        TerminalFacts::from_snapshot(plakous_attempt::ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: ProviderAcceptance::Accepted,
            visible_output: true,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: hosted,
            logical_success: false,
        })
    };
    let overflow = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new("Anthropic model_context_window_exceeded")
            .with_context_overflow(),
        facts(false),
    );
    assert_eq!(
        anthropic_model_error(&descriptor, Some(&overflow), overflow.to_string().as_str()).kind(),
        ModelErrorKind::ContextLimit
    );
    let hosted = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new("Anthropic model_context_window_exceeded")
            .with_context_overflow(),
        facts(true),
    );
    assert_ne!(
        anthropic_model_error(&descriptor, Some(&hosted), "hosted").kind(),
        ModelErrorKind::ContextLimit
    );
}

/// A real `model_context_window_exceeded` stream, not a hand-built flag, has to
/// survive the provider hook. Dropping the mark in `annotate_messages_stream_terminal`
/// leaves a hand-built protocol error green and fails this test.
#[test]
fn a_context_window_stream_reaches_context_limit_through_the_hook() {
    use plakous_attempt::ObservationSnapshot;

    let stream = context_window_stream_error();
    assert!(stream.context_overflow(), "{stream}");
    let annotated = super::annotate_messages_stream_terminal(
        super::protocol_source(&crate::stream::map_stream_error(stream.clone())),
        &stream,
    );
    assert!(annotated.context_overflow(), "{annotated}");
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::Accepted,
        visible_output: false,
        replay_safety: plakous_attempt::ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    assert_eq!(facts.physical_attempts(), 1);
    let attempt = MessagesAttemptError::protocol(annotated, facts);
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages").expect("protocol"),
        RouteId::new("anthropic.claude_oauth").expect("route"),
        ModelId::new("claude-sonnet-5").expect("model"),
        None,
        None,
        ModelAdapterProfileId::new("anthropic.claude.oauth.test").expect("profile"),
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    );
    assert_eq!(
        anthropic_model_error(&descriptor, Some(&attempt), attempt.to_string().as_str()).kind(),
        ModelErrorKind::ContextLimit
    );
}

fn context_window_stream_error() -> plakous_messages::MessagesStreamError {
    use plakous_messages::{
        MessagesRequestPolicy, MessagesRequestPolicyInput, MessagesStreamDialectHooks,
        MessagesStreamProjector, PreparedMessagesStreamBinding, ToolOwnership,
    };
    use plakous_model::{
        Digest, DigestDomain, EffectiveProviderConfigurationId, ModelId, ProtocolId,
        SelectedProviderConfigurationId, SystemClock, SystemIdGenerator,
    };

    let digest =
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"context-window-hook"]).expect("digest");
    let selected: SelectedProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"selected"],
            )
            .expect("selected digest"),
        )
        .expect("selected json"),
    )
    .expect("selected");
    let effective: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"effective"],
            )
            .expect("effective digest"),
        )
        .expect("effective json"),
    )
    .expect("effective");
    let mut projector = MessagesStreamProjector::new(
        PreparedMessagesStreamBinding::new(
            MessagesRequestPolicy::new(MessagesRequestPolicyInput {
                protocol: ProtocolId::new("anthropic.messages").expect("protocol"),
                requested_model: ModelId::new("claude-sonnet-5").expect("model"),
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
        std::sync::Arc::new(SystemClock),
        std::sync::Arc::new(SystemIdGenerator),
    )
    .expect("projector");
    projector
        .push(CONTEXT_WINDOW_SSE.as_bytes())
        .expect_err("context window fails the stream")
}

const CONTEXT_WINDOW_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ctx\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"model_context_window_exceeded\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[test]
fn typed_429_and_retry_after_reach_model_error_context() {
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages").expect("protocol"),
        RouteId::new("anthropic.claude_oauth").expect("route"),
        ModelId::new("claude-sonnet-5").expect("model"),
        None,
        Some(ModelFamilyId::new("claude-5-sonnet").expect("family")),
        ModelAdapterProfileId::new("anthropic.claude.oauth.test").expect("profile"),
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    );
    let status = ModelHttpStatus::new(429).expect("status");
    let retry_after = ModelRetryDelay::from_millis(2_000).expect("retry delay");
    let attempt = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new("opaque rejection")
            .with_http_status(status)
            .with_retry_after(retry_after),
        TerminalFacts::preacceptance_test(),
    );
    let error = anthropic_model_error(&descriptor, Some(&attempt), "opaque rejection");
    assert_eq!(error.kind(), ModelErrorKind::RateLimited);
    let context = error.context().expect("model error context");
    assert_eq!(context.http_status(), Some(status));
    assert_eq!(
        context.retryability(),
        ModelRetryability::Eligible {
            retry_after: Some(retry_after),
        }
    );
}
#[tokio::test]
async fn replay_strip_diagnostic_precedes_physical_operation_poll()
-> Result<(), Box<dyn std::error::Error>> {
    use super::*;
    use futures_util::{FutureExt, StreamExt};
    use plakous_model::{
        AttemptId, CancellationToken, Digest, DigestDomain, ModelCallContext, ModelDiagnostic,
        ModelEvent,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let binding = MessagesOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target-diagnostic"])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan-diagnostic"])?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"operation-diagnostic"],
        )?,
        plakous_messages::ReplaySafety::CostOnly,
    );
    let context = ModelCallContext::new(
        AttemptId::try_from(uuid::Uuid::from_u128(45))?,
        CancellationToken::new(),
        None,
    );
    let (_attempt, events) = MessagesAttempt::with_event_channel(
        binding,
        context,
        NonZeroUsize::new(1).ok_or("capacity")?,
    );
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.claude_oauth")?,
        ModelId::new("claude-sonnet-5")?,
        None,
        None,
        ModelAdapterProfileId::new("anthropic.claude.oauth.test")?,
        CapabilitySet::default(),
    );
    let polled = Arc::new(AtomicBool::new(false));
    let operation_polled = Arc::clone(&polled);
    let operation: AnthropicMessagesOperation = Box::pin(std::future::poll_fn(move |_| {
        operation_polled.store(true, Ordering::SeqCst);
        Poll::Pending
    }));
    let transition = Digest::sha256(DigestDomain::HistoryProjection, 1, &[b"transition"])?;
    let mut stream = AnthropicMessagesModelStream {
        pre_dispatch_diagnostics: VecDeque::from([
            ModelDiagnostic::new("incompatible Messages thinking replay was stripped"),
            ModelDiagnostic::historical_projection(transition, "configuration transition"),
        ]),
        events,
        operation: Some(operation),
        descriptor,
        pending_protocol: None,
        pending_error: None,
        done: false,
    };
    let Some(Ok(ModelEvent::Diagnostic(diagnostic))) = stream.next().await else {
        return Err("missing replay-strip diagnostic".into());
    };
    assert_eq!(
        diagnostic.message().as_str(),
        "incompatible Messages thinking replay was stripped"
    );
    assert_eq!(diagnostic.history_projection_identity(), None);
    assert!(!polled.load(Ordering::SeqCst));
    let Some(Ok(ModelEvent::Diagnostic(diagnostic))) = stream.next().await else {
        return Err("missing historical projection diagnostic".into());
    };
    assert_eq!(diagnostic.history_projection_identity(), Some(transition));
    assert!(!polled.load(Ordering::SeqCst));
    assert!(stream.next().now_or_never().is_none());
    assert!(polled.load(Ordering::SeqCst));
    Ok(())
}

/// The operation can enqueue observations and fail in the same poll. The
/// stream delivers every queued observation in order, then the operation
/// error exactly once, then ends.
#[tokio::test]
async fn queued_events_drain_in_order_before_the_operation_error()
-> Result<(), Box<dyn std::error::Error>> {
    use super::*;
    use futures_util::{FutureExt, StreamExt};
    use plakous_model::{
        AttemptId, CancellationToken, Digest, DigestDomain, ModelCallContext, ModelEvent,
        ModelRetry, ModelRetryCause,
    };

    let binding = MessagesOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target-drain"])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan-drain"])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"operation-drain"])?,
        plakous_messages::ReplaySafety::CostOnly,
    );
    let context = ModelCallContext::new(
        AttemptId::try_from(uuid::Uuid::from_u128(46))?,
        CancellationToken::new(),
        None,
    );
    let (mut attempt, events) = MessagesAttempt::with_event_channel(
        binding,
        context,
        NonZeroUsize::new(4).ok_or("capacity")?,
    );
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.claude_oauth")?,
        ModelId::new("claude-sonnet-5")?,
        None,
        None,
        ModelAdapterProfileId::new("anthropic.claude.oauth.test")?,
        CapabilitySet::default(),
    );
    let retry = |next| {
        ModelRetry::new(
            next,
            4,
            Duration::ZERO,
            ModelRetryCause::RecordRejectedInTransit,
            false,
            "queued",
        )
    };
    let operation: AnthropicMessagesOperation = Box::pin(async move {
        for next in [2, 3] {
            attempt.events_mut().send_retry(retry(next)).await?;
        }
        Err(MessagesAttemptError::Unavailable)
    });
    let mut stream = AnthropicMessagesModelStream {
        pre_dispatch_diagnostics: VecDeque::new(),
        events,
        operation: Some(operation),
        descriptor,
        pending_protocol: None,
        pending_error: None,
        done: false,
    };
    for expected in [2, 3] {
        let Some(Some(Ok(ModelEvent::Retry(retry)))) = stream.next().now_or_never() else {
            return Err(format!("queued retry {expected} was not delivered").into());
        };
        assert_eq!(retry.next_physical(), expected);
    }
    let Some(Some(Err(error))) = stream.next().now_or_never() else {
        return Err("operation error was not delivered".into());
    };
    assert!(
        error.to_string().contains("Messages service unavailable"),
        "{error}"
    );
    assert!(matches!(stream.next().now_or_never(), Some(None)));
    Ok(())
}
