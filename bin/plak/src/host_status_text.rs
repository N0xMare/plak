//! Plak-owned rendering of typed host lifecycle facts.

use plakous_host_protocol::status::{
    CancelUnsettled, DetachedReason, HostPause, HostStatusDetail, HostTransitionStage, SteerSource,
};

#[cfg(test)]
mod tests;

pub(crate) fn pause_suffix(pause: HostPause) -> Option<&'static str> {
    match (pause.tree, pause.queue) {
        (false, false) => None,
        (true, true) => Some("agent tree paused · queue not draining · /tree then r"),
        (true, false) => Some("agent tree paused · /tree then r"),
        (false, true) => Some("queue paused, not draining · Tab then r"),
    }
}

fn with_pause(detail: String, pause: HostPause) -> String {
    pause_suffix(pause).map_or_else(|| detail.clone(), |suffix| format!("{detail} · {suffix}"))
}

pub(crate) fn detached_reason(reason: &DetachedReason) -> String {
    match reason {
        DetachedReason::MissingDefaults => "missing_defaults".into(),
        DetachedReason::EffortRequired => "effort_required".into(),
        DetachedReason::ResolveFailed(failure) => {
            format!("resolve_failed: {}", failure.diagnostic.as_str())
        }
        DetachedReason::CredentialUnusable(failure) => {
            format!("credential_unusable: {}", failure.diagnostic.as_str())
        }
        DetachedReason::RecoveryRequired(failure) => {
            format!("recovery_required: {}", failure.diagnostic.as_str())
        }
        DetachedReason::Other(diagnostic) => diagnostic.as_str().into(),
    }
}

pub(crate) fn transition_label(stage: HostTransitionStage) -> &'static str {
    match stage {
        HostTransitionStage::ClearResolveAuthority => "fresh authority resolved",
        HostTransitionStage::ClearBuildCandidate => "candidate runtime built",
        HostTransitionStage::ClearPublishCandidate => "candidate product record published",
        HostTransitionStage::ClearActivate => "candidate activated",
        HostTransitionStage::RebindResolveTarget => "exact target resolved",
        HostTransitionStage::RebindPrepareRuntime => "immutable runtime prepared",
        HostTransitionStage::RebindPublishIntent => "durable transition prepared",
        HostTransitionStage::RebindCanonicalTransition => "canonical transition completed",
        HostTransitionStage::RebindOpenTarget => "target runtime opened",
        HostTransitionStage::RebindActivate => "target activated",
    }
}

pub(crate) fn status_detail(
    detail: &HostStatusDetail,
    frontend: crate::notice_text::Frontend,
) -> String {
    use HostStatusDetail as D;
    match detail {
        D::ConfigurationPath { path } => format!("config: {path}"),
        D::ChooseProfile => "choose a profile with /agents".into(),
        D::CreatingFreshConversation => {
            "creating a fresh conversation on the current exact binding".into()
        }
        D::ForkingSession => "forking session".into(),
        D::InterruptRequested => "interrupt requested".into(),
        D::NoActiveTurn => "no active turn".into(),
        D::NoAttachedTurnToInterrupt => "nothing to interrupt; no agent turn is attached".into(),
        D::PromptNotAdmitted => "prompt not admitted".into(),
        D::PromptNotAdmittedQueuePaused => "prompt not admitted; queue paused".into(),
        D::PromptPreparationStopped => "prompt preparation stopped; prompt remains queued".into(),
        D::QueueDeliveryNeedsAttachment => {
            "queue delivery requires an attached canonical agent".into()
        }
        D::QueueProjectionCatchingUp => {
            "queue projection is catching up; prompt remains queued".into()
        }
        D::QueuedFollowUp => "queued follow-up".into(),
        D::ReattachPending => "reattach pending".into(),
        D::RecoveringInterruptedTurn => "recovering the interrupted turn".into(),
        D::RecoveryIncomplete => "recovery did not complete; the next prompt retries it".into(),
        D::RecoveryPending => "needs recovery · next prompt recovers".into(),
        D::ReleasingPreviousAttachment => "releasing the previous session attachment".into(),
        D::TurnComplete => "turn complete".into(),
        D::Ready { model_id, pause } => with_pause(format!("ready · {model_id}"), *pause),
        D::ClearRefused { model_id } => format!("clear refused · {model_id} remains active"),
        D::ForkingProfileRoot { model } => format!("forking onto profile root `{model}`"),
        D::RestoringAdmittedTurn { pause } => {
            with_pause("restoring the original admitted turn".into(), *pause)
        }
        D::PlanDiagnostic { diagnostic } => diagnostic.as_str().into(),
        D::Detached { reason } => format!("not ready · {}", detached_reason(reason)),
        D::DetachedPromptRefused { reason } => {
            format!("prompt refused · {}", detached_reason(reason))
        }
        D::TransitionStage {
            stage,
            cumulative_elapsed_ms,
        } => format!(
            "{} · cumulative elapsed {cumulative_elapsed_ms}ms",
            transition_label(*stage)
        ),
        D::Failure { failure } => crate::notice_text::with_remedy(
            failure.diagnostic.as_str(),
            failure.remedy.as_ref(),
            crate::notice_text::RemedyContext {
                frontend,
                session_id: None,
            },
        ),
        D::CancelUnsettled { cause } => cancel_unsettled(cause),
        D::Steering {
            source: SteerSource::Direct,
        } => "steering…".into(),
        D::Steering {
            source: SteerSource::QueuedDelivery,
        } => "steering...".into(),
        D::SteerRejected { cause } => format!("steer rejected: {}", cause.diagnostic.as_str()),
    }
}

pub(crate) fn cancel_unsettled(cause: &CancelUnsettled) -> String {
    match cause {
        CancelUnsettled::RuntimeRefused { cause } => format!(
            "cancel was refused by the runtime ({}); the turn is still running",
            cause.diagnostic.as_str()
        ),
        CancelUnsettled::Deadline { timeout_ms } => {
            format!("cancel did not settle within {timeout_ms}ms; the turn is still running")
        }
    }
}
