use plakous_model::{DigestDomain, ModelCallContext};
use uuid::Uuid;

use super::super::test_wire::TestWire;
use super::*;
use plakous_attempt::ProviderRefusal;
type ChatAttempt = FamilyAttempt<TestWire>;
type ChatAttemptError = FamilyAttemptError<TestWire>;
type ChatOperationBinding = FamilyOperationBinding;
type ChatPhysicalRequestBinding = FamilyPhysicalRequestBinding;

fn attempt(label: &[u8]) -> Result<ChatAttempt, Box<dyn std::error::Error>> {
    let operation = ChatOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[label])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[label])?,
        ReplaySafety::CostOnly,
    );
    let context = ModelCallContext::new(
        AttemptId::try_from(Uuid::from_u128(1))?,
        CancellationToken::new(),
        None,
    );
    Ok(ChatAttempt::with_event_channel(operation, context, NonZeroUsize::MIN).0)
}

#[test]
fn a_cost_only_operation_never_reports_a_hosted_effect() -> Result<(), Box<dyn std::error::Error>> {
    let attempt = attempt(b"no-hosted-effects")?;
    assert_eq!(
        attempt.binding().operation().replay_safety(),
        ReplaySafety::CostOnly
    );
    attempt.core.mark_service_admitted()?;
    attempt.observe_physical_request_started()?;
    attempt.observe_provider_accepted()?;
    let facts = attempt.terminal_facts();
    assert_eq!(facts.replay_safety(), ReplaySafety::CostOnly);
    assert!(!facts.hosted_effects_possible());
    Ok(())
}

#[test]
fn an_in_band_deadline_after_a_hosted_effect_never_replays()
-> Result<(), Box<dyn std::error::Error>> {
    let attempt = attempt(b"hosted-before-deadline")?;
    attempt.core.mark_service_admitted()?;
    attempt.core.declare_server_effect()?;
    attempt.observe_physical_request_started()?;
    attempt.core.observe_received_event(true)?;
    attempt.observe_physical_request_finished(PhysicalRequestOutcome::AcceptedDisconnect)?;
    let facts = attempt.terminal_facts();
    assert!(facts.hosted_effects_possible());
    for status in [408, 504] {
        assert_eq!(
            ProviderRefusal::from_http_status(status),
            Some(ProviderRefusal::Deadline)
        );
        let delay = plakous_attempt::RetryPolicy::TURN.next_delay(
            facts,
            plakous_attempt::FailurePoint::MidStream,
            None,
            std::time::Duration::ZERO,
            plakous_attempt::Jitter::SHORTEST,
        );
        assert_eq!(delay, None);
        let reason = crate::retry_decline(
            &crate::RetryDeclineInput {
                deadline_remaining: None,
                established: None,
                redispatch_available: true,
                turn_retry: true,
                cause: plakous_model::ModelRetryCause::StreamInterrupted {
                    fault: plakous_model::ModelTransportFault::Timeout,
                },
                point: plakous_attempt::FailurePoint::MidStream,
                facts,
                elapsed: std::time::Duration::ZERO,
                delay,
            },
            &mut plakous_attempt::LongStreamReplayBudget::default(),
        );
        assert_eq!(
            reason,
            Some(plakous_model::ModelRetryDeclineReason::ServerEffect)
        );
    }
    Ok(())
}

#[test]
fn a_refused_generation_is_followed_by_a_fresh_one_never_an_overlapping_one()
-> Result<(), Box<dyn std::error::Error>> {
    let attempt = attempt(b"generations")?;
    attempt.core.mark_service_admitted()?;
    let first = attempt.observe_physical_request_started()?;
    assert!(attempt.observe_physical_request_started().is_err());
    attempt.observe_physical_request_finished(PhysicalRequestOutcome::DefiniteRejection)?;
    assert_eq!(
        attempt.terminal_facts().provider_acceptance(),
        ProviderAcceptance::NotAccepted
    );
    let second = attempt.observe_physical_request_started()?;
    assert_eq!((first.generation(), second.generation()), (1, 2));
    assert_eq!(attempt.reserve_authorization()?, second);
    assert_eq!(attempt.terminal_facts().physical_attempts(), 2);
    Ok(())
}

#[test]
fn authorization_rejects_foreign_attempt_and_generation_bindings()
-> Result<(), Box<dyn std::error::Error>> {
    let owner = attempt(b"binding-owner")?;
    owner.core.mark_service_admitted()?;
    let physical = owner.observe_physical_request_started()?;
    let reserved = owner.reserve_authorization()?;
    assert_eq!(physical, reserved);

    let wrong_generation = ChatPhysicalRequestBinding::new(owner.binding().clone(), 2);
    assert!(matches!(
        owner.validate_authorization(&wrong_generation),
        Err(ChatAttemptError::Observation { .. })
    ));

    let foreign = attempt(b"binding-foreign")?;
    let foreign_binding =
        ChatPhysicalRequestBinding::new(foreign.binding().clone(), physical.generation());
    assert!(matches!(
        owner.validate_authorization(&foreign_binding),
        Err(ChatAttemptError::Binding { .. })
    ));
    Ok(())
}

#[test]
fn admitted_exact_acceptance_is_preserved_in_terminal_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let (attempt, _receiver) = ChatAttempt::with_event_channel(
        FamilyOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"accepted"])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"accepted"])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"accepted"])?,
            ReplaySafety::CostOnly,
        ),
        ModelCallContext::new(
            AttemptId::try_from(Uuid::from_u128(5))?,
            CancellationToken::new(),
            None,
        ),
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    attempt.core.mark_service_admitted()?;
    attempt.observe_physical_request_started()?;
    attempt.observe_provider_accepted()?;
    let facts = attempt.terminal_facts();
    assert_eq!(facts.physical_attempts(), 1);
    assert_eq!(facts.provider_acceptance(), ProviderAcceptance::Accepted);
    assert_ne!(
        facts.retry_class(),
        plakous_attempt::RetryClass::BeforeAcceptance
    );
    Ok(())
}
