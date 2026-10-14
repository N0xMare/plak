//! Eligibility for a new continuation turn, never a replay of a failed request.

use plakous::model::{ModelErrorKind, ModelErrorPhase, ModelRetryability};
use std::time::Duration;

/// The gate a transient continuation must pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeGate {
    /// Wait for content-free reachability on the original route.
    Network,
    /// Honor bounded retry advice, or the fixed fallback for scripted models.
    Delay(Duration),
}

/// Classifies only committed model-error facts. Budget and cancellation are
/// checked by the owner before it writes a continuation decision.
pub fn classify(
    kind: ModelErrorKind,
    phase: ModelErrorPhase,
    retry: ModelRetryability,
) -> Option<ResumeGate> {
    use plakous::model::ModelRetryRisk;
    if !matches!(
        retry,
        ModelRetryability::Eligible { .. }
            | ModelRetryability::Exhausted
            | ModelRetryability::Unsafe {
                risk: ModelRetryRisk::DuplicateInferenceOrBilling
            }
    ) {
        return None;
    }
    match kind {
        ModelErrorKind::Transport
        | ModelErrorKind::Timeout
        | ModelErrorKind::Unavailable
        | ModelErrorKind::IncompleteResponse => {}
        ModelErrorKind::RateLimited
            if matches!(
                crate::agent_tree::turn_outcome::classify_provider_refusal(kind, Some(retry), 0),
                Some(crate::agent_tree::turn_outcome::TurnRefusal::Failure { .. })
            ) => {}
        _ => return None,
    }
    if phase == ModelErrorPhase::Connect
        || (phase == ModelErrorPhase::Authentication && kind == ModelErrorKind::Transport)
    {
        return Some(ResumeGate::Network);
    }
    let delay = match retry {
        ModelRetryability::Eligible {
            retry_after: Some(delay),
        } => Duration::from_millis(delay.as_millis()),
        _ => Duration::from_secs(30),
    };
    Some(ResumeGate::Delay(
        delay.clamp(Duration::from_secs(1), Duration::from_secs(60)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::model::{ModelRetryDelay, ModelRetryRisk};

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
}
