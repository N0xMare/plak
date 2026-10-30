use super::*;
use plakous_host_protocol::status::{ControlFailure, DiagnosticText, HostPhase};

#[test]
fn native_failure_status_spelling_is_restored_from_binding_facts_not_diagnostic() {
    use plakous_host_protocol::notice_remedy::AccountTarget;
    let mut failure: HostFailure = "opaque neutral diagnosis".into();
    failure.origin = Some(Box::new(HostFailureOrigin::NativeBinding {
        label: "Native".into(),
        account: AccountTarget::Grok,
    }));
    for (cause, expected) in [
        (
            AttachFailure::CredentialRejected,
            "Native credential was rejected at refresh; run `plak login grok`",
        ),
        (
            AttachFailure::CredentialUnavailable,
            "Native credential is unavailable (signed out, quarantined, or a login made by an earlier build); run `plak login grok`",
        ),
        (
            AttachFailure::LoginSelectionChanged,
            "exact reopen refused: Native login selection changed; use /reload or /rebind to carry the history under the current login; Fork now is available only while attached",
        ),
        (
            AttachFailure::AccountChanged,
            "Native provider account changed; log in to the original account with `plak login grok` to reopen exactly, or use /rebind to carry the history into a new session under the newly logged-in account",
        ),
        (
            AttachFailure::BindingChanged,
            "exact reopen refused: Native is bound to a different account or model; restore the original account and configuration with `plak login grok`, or use /rebind to carry the history into a new session",
        ),
    ] {
        failure.cause = cause;
        assert_eq!(
            status_detail(
                &HostStatusDetail::Failure {
                    failure: failure.clone()
                },
                Frontend::Jsonl
            ),
            expected
        );
        assert_eq!(
            detached_reason(&DetachedReason::ResolveFailed(failure.clone())),
            format!("resolve_failed: {expected}")
        );
    }
}
use crate::agent_jsonl::{JsonlCorrelation, project_host_event};
use crate::notice_text::Frontend;
use crate::surface_protocol::HostEvent;

fn wire(detail: HostStatusDetail, phase: HostPhase) -> serde_json::Value {
    let event = HostEvent::Status {
        phase,
        queue_len: 7,
        detail: Some(detail),
    };
    assert!(
        plakous_host_protocol::jsonl::project_host_event(&event, JsonlCorrelation::default())
            .is_none()
    );
    serde_json::from_str(
        &project_host_event(&event, JsonlCorrelation::default(), None).expect("Plak status"),
    )
    .expect("JSONL")
}

#[test]
fn all_host_phase_spellings_and_missing_detail_stay_stable() {
    for (phase, label) in [
        (HostPhase::Idle, "idle"),
        (HostPhase::Running, "running"),
        (HostPhase::Preparing, "preparing"),
        (HostPhase::Interrupting, "interrupting"),
        (HostPhase::Interrupted, "interrupted"),
        (HostPhase::Error, "error"),
        (HostPhase::NotReady, "not_ready"),
        (HostPhase::Compacting, "compacting"),
        (HostPhase::Clear, "clear"),
        (HostPhase::Resume, "resume"),
        (HostPhase::Fork, "fork"),
        (HostPhase::Rebind, "rebind"),
    ] {
        let event = HostEvent::Status {
            phase,
            queue_len: 3,
            detail: None,
        };
        let value: serde_json::Value = serde_json::from_str(
            &project_host_event(&event, JsonlCorrelation::default(), None).expect("status"),
        )
        .expect("JSONL");
        assert_eq!(value["phase"], label);
        assert_eq!(value["queue_len"], 3);
        assert!(value.get("detail").is_none());
    }
}

#[test]
fn every_unit_status_detail_keeps_its_jsonl_spelling() {
    for (detail, expected) in [
        (
            HostStatusDetail::ChooseProfile,
            "choose a profile with /agents",
        ),
        (
            HostStatusDetail::CreatingFreshConversation,
            "creating a fresh conversation on the current exact binding",
        ),
        (HostStatusDetail::ForkingSession, "forking session"),
        (HostStatusDetail::InterruptRequested, "interrupt requested"),
        (HostStatusDetail::NoActiveTurn, "no active turn"),
        (
            HostStatusDetail::NoAttachedTurnToInterrupt,
            "nothing to interrupt; no agent turn is attached",
        ),
        (HostStatusDetail::PromptNotAdmitted, "prompt not admitted"),
        (
            HostStatusDetail::PromptNotAdmittedQueuePaused,
            "prompt not admitted; queue paused",
        ),
        (
            HostStatusDetail::PromptPreparationStopped,
            "prompt preparation stopped; prompt remains queued",
        ),
        (
            HostStatusDetail::QueueDeliveryNeedsAttachment,
            "queue delivery requires an attached canonical agent",
        ),
        (
            HostStatusDetail::QueueProjectionCatchingUp,
            "queue projection is catching up; prompt remains queued",
        ),
        (HostStatusDetail::QueuedFollowUp, "queued follow-up"),
        (HostStatusDetail::ReattachPending, "reattach pending"),
        (
            HostStatusDetail::RecoveringInterruptedTurn,
            "recovering the interrupted turn",
        ),
        (
            HostStatusDetail::RecoveryIncomplete,
            "recovery did not complete; the next prompt retries it",
        ),
        (
            HostStatusDetail::RecoveryPending,
            "needs recovery · next prompt recovers",
        ),
        (
            HostStatusDetail::ReleasingPreviousAttachment,
            "releasing the previous session attachment",
        ),
        (HostStatusDetail::TurnComplete, "turn complete"),
    ] {
        assert_eq!(status_detail(&detail, Frontend::Tui), expected);
        assert_eq!(wire(detail, HostPhase::Idle)["detail"], expected);
    }
}

#[test]
fn structured_status_facts_preserve_pause_and_steer_projection() {
    for (pause, suffix) in [
        (
            HostPause {
                tree: false,
                queue: false,
            },
            "",
        ),
        (
            HostPause {
                tree: true,
                queue: false,
            },
            " · agent tree paused · /tree then r",
        ),
        (
            HostPause {
                tree: false,
                queue: true,
            },
            " · queue paused, not draining · Tab then r",
        ),
        (
            HostPause {
                tree: true,
                queue: true,
            },
            " · agent tree paused · queue not draining · /tree then r",
        ),
    ] {
        assert_eq!(
            wire(
                HostStatusDetail::Ready {
                    model_id: "exact".into(),
                    pause
                },
                HostPhase::Idle
            )["detail"],
            format!("ready · exact{suffix}")
        );
        assert_eq!(
            wire(
                HostStatusDetail::RestoringAdmittedTurn { pause },
                HostPhase::Running
            )["detail"],
            format!("restoring the original admitted turn{suffix}")
        );
    }
    for (source, expected) in [
        (SteerSource::Direct, "steering…"),
        (SteerSource::QueuedDelivery, "steering..."),
    ] {
        assert_eq!(
            wire(HostStatusDetail::Steering { source }, HostPhase::Running)["detail"],
            expected
        );
    }
    assert_eq!(
        wire(
            HostStatusDetail::CancelUnsettled {
                cause: CancelUnsettled::Deadline { timeout_ms: 5000 }
            },
            HostPhase::Interrupting
        )["detail"],
        "cancel did not settle within 5000ms; the turn is still running"
    );
    assert_eq!(
        wire(
            HostStatusDetail::CancelUnsettled {
                cause: CancelUnsettled::RuntimeRefused {
                    cause: ControlFailure {
                        diagnostic: DiagnosticText::new("refused")
                    }
                }
            },
            HostPhase::Interrupting
        )["detail"],
        "cancel was refused by the runtime (refused); the turn is still running"
    );
}

#[test]
fn every_transition_stage_keeps_its_cumulative_elapsed_label() {
    for (stage, expected) in [
        (
            HostTransitionStage::ClearResolveAuthority,
            "fresh authority resolved",
        ),
        (
            HostTransitionStage::ClearBuildCandidate,
            "candidate runtime built",
        ),
        (
            HostTransitionStage::ClearPublishCandidate,
            "candidate product record published",
        ),
        (HostTransitionStage::ClearActivate, "candidate activated"),
        (
            HostTransitionStage::RebindResolveTarget,
            "exact target resolved",
        ),
        (
            HostTransitionStage::RebindPrepareRuntime,
            "immutable runtime prepared",
        ),
        (
            HostTransitionStage::RebindPublishIntent,
            "durable transition prepared",
        ),
        (
            HostTransitionStage::RebindCanonicalTransition,
            "canonical transition completed",
        ),
        (
            HostTransitionStage::RebindOpenTarget,
            "target runtime opened",
        ),
        (HostTransitionStage::RebindActivate, "target activated"),
    ] {
        assert_eq!(
            wire(
                HostStatusDetail::TransitionStage {
                    stage,
                    cumulative_elapsed_ms: 123
                },
                HostPhase::Rebind
            )["detail"],
            format!("{expected} · cumulative elapsed 123ms")
        );
    }
}
