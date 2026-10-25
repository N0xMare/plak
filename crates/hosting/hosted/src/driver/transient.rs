//! Plan and gate a new continuation turn, never replay a failed request.

use crate::agent_tree_protocol::AttemptId;
use plakous::agent::{CommandId, TurnResult, TurnResultOutcome};
use plakous::model::{
    CancellationToken, ModelErrorKind, ModelErrorPhase, ModelRetryability, ReachabilityProbe,
    TurnId,
};
use std::future::Future;
use std::time::Duration;

pub(crate) const COMMAND_PREFIX: &str = "plak:resume-transient:";
pub(crate) const ROOT_COMMAND_PREFIX: &str = "plak:resume-transient:root:";

pub(crate) enum ResumeOwner<'a> {
    Root {
        failed_turn: TurnId,
    },
    Child {
        agent: &'a str,
        attempt: &'a AttemptId,
    },
}

pub(crate) struct ResumePlan {
    pub(crate) gate: ResumeGate,
    pub(crate) round: u8,
    pub(crate) command_id: CommandId,
    pub(crate) text: String,
}

/// The root supplies its rendered diagnostic; a child supplies its canonical
/// failure message. Neither is interchangeable with the other's prompt text.
pub(crate) fn plan(
    gate: ResumeGate,
    owner: ResumeOwner<'_>,
    failure: &str,
    spent: u8,
    budget: u8,
) -> Option<ResumePlan> {
    if spent >= budget {
        return None;
    }
    ResumePlan::for_round(gate, owner, failure, spent + 1)
}

impl ResumePlan {
    pub(crate) fn for_round(
        gate: ResumeGate,
        owner: ResumeOwner<'_>,
        failure: &str,
        round: u8,
    ) -> Option<Self> {
        debug_assert!(round > 0, "continuation rounds are one-based");
        if round == 0 {
            return None;
        }
        let (command, text) = match owner {
            ResumeOwner::Root { failed_turn } => (
                format!("{ROOT_COMMAND_PREFIX}{failed_turn}"),
                format!(
                    "The previous turn failed transiently: {failure}. Continue the current task from the last completed step; do not repeat completed actions."
                ),
            ),
            ResumeOwner::Child { agent, attempt } => (
                crate::agent_tree::ProductAgentTree::transient_resume_command_id(
                    agent, attempt, round,
                ),
                crate::child_prompts::TransientResume { failure }.render(),
            ),
        };
        let command_id = CommandId::new(command);
        debug_assert!(
            command_id.is_ok(),
            "host-derived continuation identity is valid"
        );
        Some(Self {
            gate,
            round,
            command_id: command_id.ok()?,
            text,
        })
    }
}

pub(crate) fn failure_gate(result: &TurnResult) -> Option<ResumeGate> {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return None;
    };
    let model = error.model_error()?;
    let context = model.context()?;
    classify(model.kind(), context.phase(), context.retryability())
}

/// Arm owner notifications before checking ownership, so a concurrent close
/// cannot be lost between the check and the wait. Dropping the wait releases
/// only this subscriber to the shared exact-route probe.
pub(crate) async fn wait_gate<E: Future<Output = ()>, A: Future<Output = bool>>(
    gate: ResumeGate,
    probe: Option<ReachabilityProbe>,
    cancellation: &CancellationToken,
    safety_tick: Option<Duration>,
    mut edges: impl FnMut() -> Option<E>,
    mut allowed: impl FnMut() -> A,
) -> bool {
    let (probe, delay) = match gate {
        ResumeGate::Network => (probe, Duration::from_secs(30)),
        ResumeGate::Delay(delay) => (None, delay),
    };
    let ready =
        crate::runtime::network_watch::NetworkWatch::shared().wait(probe, delay, cancellation);
    tokio::pin!(ready);
    loop {
        let Some(edge) = edges() else {
            return false;
        };
        tokio::pin!(edge);
        if !allowed().await {
            cancellation.cancel();
            return false;
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return false,
            () = &mut edge => {},
            reached = &mut ready => return reached && allowed().await,
            // A hub release can drain the live slot without a tree mutation.
            () = async {
                match safety_tick {
                    Some(tick) => tokio::time::sleep(tick).await,
                    None => std::future::pending().await,
                }
            } => {},
        }
    }
}

/// The gate a transient continuation must pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeGate {
    /// Wait for content-free reachability on the original route.
    Network,
    /// Honor bounded retry advice, or the fixed fallback for scripted models.
    Delay(Duration),
}

/// Classifies only committed model-error facts. Budget and cancellation are
/// checked by the owner before it writes a continuation decision.
fn classify(
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
