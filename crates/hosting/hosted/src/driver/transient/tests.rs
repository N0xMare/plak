use super::*;
use plakous::model::{ModelRetryDelay, ModelRetryRisk};

#[test]
fn a_zero_saved_round_is_refused_without_panicking() {
    assert!(
        ResumePlan::for_round(
            ResumeGate::Network,
            ResumeOwner::Root {
                failed_turn: TurnId::try_from(uuid::Uuid::from_u128(1)).unwrap(),
            },
            "temporary failure",
            0,
        )
        .is_none()
    );
}

#[test]
fn transient_classifier_uses_only_persisted_facts() {
    for kind in [
        ModelErrorKind::Transport,
        ModelErrorKind::Timeout,
        ModelErrorKind::Unavailable,
        ModelErrorKind::IncompleteResponse,
    ] {
        for retry in [
            ModelRetryability::Eligible { retry_after: None },
            ModelRetryability::Exhausted,
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::DuplicateInferenceOrBilling,
            },
        ] {
            assert_eq!(
                classify(kind, ModelErrorPhase::Connect, retry),
                Some(ResumeGate::Network)
            );
            assert_eq!(
                classify(kind, ModelErrorPhase::Stream, retry),
                Some(ResumeGate::Delay(Duration::from_secs(30)))
            );
        }
        for retry in [
            ModelRetryability::Unknown,
            ModelRetryability::Permanent,
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::ProviderHostedEffect,
            },
        ] {
            assert_eq!(classify(kind, ModelErrorPhase::Connect, retry), None);
        }
    }
    for kind in [
        ModelErrorKind::OutcomeUnknown,
        ModelErrorKind::ContextLimit,
        ModelErrorKind::Policy,
        ModelErrorKind::QuotaExceeded,
        ModelErrorKind::Authentication,
        ModelErrorKind::ReauthenticationRequired,
        ModelErrorKind::Authorization,
        ModelErrorKind::OutputLimit,
        ModelErrorKind::Cancelled,
    ] {
        assert_eq!(
            classify(
                kind,
                ModelErrorPhase::Authentication,
                ModelRetryability::Eligible { retry_after: None }
            ),
            None
        );
    }
    assert_eq!(
        classify(
            ModelErrorKind::Transport,
            ModelErrorPhase::Authentication,
            ModelRetryability::Exhausted
        ),
        Some(ResumeGate::Network)
    );
}

#[test]
fn transient_classifier_clamps_advice_and_rejects_blocked_429() {
    for (millis, seconds) in [(0, 1), (1_500, 0), (120_000, 60)] {
        let retry = ModelRetryability::Eligible {
            retry_after: Some(ModelRetryDelay::from_millis(millis).unwrap()),
        };
        let expected = if seconds == 0 {
            Duration::from_millis(millis)
        } else {
            Duration::from_secs(seconds)
        };
        assert_eq!(
            classify(
                ModelErrorKind::RateLimited,
                ModelErrorPhase::FirstEvent,
                retry
            ),
            Some(ResumeGate::Delay(expected))
        );
    }
    for retry in [
        ModelRetryability::Exhausted,
        ModelRetryability::Eligible {
            retry_after: Some(ModelRetryDelay::from_millis(300_000).unwrap()),
        },
    ] {
        assert_eq!(
            classify(
                ModelErrorKind::RateLimited,
                ModelErrorPhase::FirstEvent,
                retry
            ),
            None
        );
    }
}
