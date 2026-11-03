//! Plak-owned rendering of typed host lifecycle facts.

use plakous_host_protocol::status::{
    AttachFailure, CancelUnsettled, DetachedReason, HostFailure, HostFailureOrigin, HostPause,
    HostStatusDetail, HostTransitionStage, SteerSource,
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
            format!("resolve_failed: {}", legacy_failure(failure))
        }
        DetachedReason::CredentialUnusable(failure) => {
            format!("credential_unusable: {}", legacy_failure(failure))
        }
        DetachedReason::RecoveryRequired(failure) => {
            format!("recovery_required: {}", legacy_failure(failure))
        }
        DetachedReason::Other(diagnostic) => diagnostic.as_str().into(),
    }
}

fn legacy_failure(failure: &HostFailure) -> String {
    use AttachFailure as F;
    match failure.origin.as_deref() {
        Some(HostFailureOrigin::NativeBinding { label, account }) => {
            let account = crate::notice_text::account_label(*account);
            match failure.cause {
                F::AccountChanged => format!(
                    "{label} provider account changed; log in to the original account with `plak login {account}` to reopen exactly, or use /rebind to carry the history into a new session under the newly logged-in account"
                ),
                F::CredentialRejected => format!(
                    "{label} credential was rejected at refresh; run `plak login {account}`"
                ),
                F::CredentialUnavailable => format!(
                    "{label} credential is unavailable (signed out, quarantined, or a login made by an earlier build); run `plak login {account}`"
                ),
                F::LoginSelectionChanged => format!(
                    "exact reopen refused: {label} login selection changed; use /reload or /rebind to carry the history under the current login; Fork now is available only while attached"
                ),
                F::BindingChanged => format!(
                    "exact reopen refused: {label} is bound to a different account or model; restore the original account and configuration with `plak login {account}`, or use /rebind to carry the history into a new session"
                ),
                _ => failure.diagnostic.as_str().into(),
            }
        }
        Some(HostFailureOrigin::ModelFacts { account, model }) => format!(
            "{account} model `{model}` changed since this session was created (its provider-published facts or this build's reviewed table moved); rebind with Fork now in the Agents tab, or start a new session"
        ),
        None => failure.diagnostic.as_str().into(),
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
            "starting a new session on the current exact binding".into()
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
        D::ClearRefused { model_id } => format!("new session refused · {model_id} remains active"),
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
        D::Failure { failure } if frontend == crate::notice_text::Frontend::Jsonl => {
            legacy_failure(failure)
        }
        D::Failure { failure } => crate::refusal_text::host_failure_text(
            failure,
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
