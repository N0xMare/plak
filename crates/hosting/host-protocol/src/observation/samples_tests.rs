//! Observations constructible from plain values, shared by the class and
//! serde tests.

use crate::intent::{Core, Draft, SessionIntent};
use crate::observation::SessionObservation;

type Observation = SessionObservation<Core>;

/// Observations constructible from plain values, one or more per class.
pub(crate) fn class_samples() -> Vec<Observation> {
    vec![
        Observation::JobStarted {
            job_id: "p1".into(),
            owner: "root".into(),
            command: "make".into(),
        },
        Observation::JobPromoted {
            job_id: "p1".into(),
            owner: "root".into(),
        },
        Observation::Activity {
            label: "thinking".into(),
            active: true,
        },
        Observation::AgentAttached(true),
        Observation::SetBusy(true),
        Observation::PromptHistorySnapshot(vec!["hi".into()]),
        Observation::PromptCorrelationReset,
        Observation::IntentStarted(Some("i1".into())),
        Observation::DraftAccepted(SessionIntent::Interrupt),
        Observation::DraftRejected(SessionIntent::Prompt(Draft::default())),
        Observation::DraftRestored(Draft::default()),
        Observation::UserMessage {
            text: "hi".into(),
            sent_at_ms: None,
        },
        Observation::AssistantDelta("a".into()),
        Observation::AssistantDone,
        Observation::ModelRefusal { detail: None },
        Observation::ClearView,
        Observation::ReasoningDelta {
            location_key: "r".into(),
            text: "t".into(),
        },
        Observation::ReasoningSealed {
            location_key: "r".into(),
        },
        Observation::ApprovalRequired {
            approval_id: "a1".into(),
            call_id: None,
            tool_name: "shell".into(),
            subject: None,
            prompt: None,
            expires_at_ms: 1,
        },
        Observation::ApprovalResolved {
            approval_id: "a1".into(),
            resolution: "approved".into(),
        },
        Observation::SteerAck {
            state: "queued".into(),
            position: Some(1),
        },
        Observation::ReattachReady,
        Observation::SessionOpening,
        Observation::ModelRequestStarted,
        Observation::ModelRequestQueued {
            slots: "1/2".into(),
        },
        Observation::CredentialWait {
            account: Some("main".into()),
        },
        Observation::SessionOpened {
            session_open_ms: Some(3),
        },
        Observation::TopologyNavigatorUnavailable {
            reason: "no tree".into(),
        },
        Observation::PendingContinuation {
            results: 0,
            questions: 0,
            process_exits: 0,
            auto_continue_paused: false,
        },
        Observation::LastUserPromptAt(None),
        Observation::Processes {
            views: vec![crate::views::ProcessView {
                id: "p1".into(),
                command: "make".into(),
                owner: "root".into(),
                started_at: 1,
                elapsed_ms: 2,
                status: crate::views::ProcessStatus::Running,
                output: "ok".into(),
            }],
            running: 1,
        },
    ]
}
