use super::{host_event_kind, project_host_event};
use crate::envelope::JsonlCorrelation;
use crate::intent::{Core, ObservationExtension, ProtocolExtension};
use crate::observation::{SessionObservation, TurnLifecycle, TurnPhase};
use crate::views::transcript::{NoticeScope, TranscriptRow};

#[test]
fn last_user_prompt_time_is_in_process_and_omitted_from_jsonl_v1() {
    for event in [
        SessionObservation::<Core>::LastUserPromptAt(Some(1_790_000_000_000)),
        SessionObservation::<Core>::LastUserPromptAt(None),
    ] {
        assert_eq!(host_event_kind(&event), "last_user_prompt_at");
        assert_eq!(
            project_host_event(&event, JsonlCorrelation::default()),
            None
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Overlay;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenOverlay;

impl ObservationExtension for OpenOverlay {
    fn latest_state(&self) -> bool {
        true
    }

    fn same_kind(&self, _other: &Self) -> bool {
        true
    }

    fn kind_label(&self) -> &'static str {
        "overlay"
    }
}

impl ProtocolExtension for Overlay {
    type Intent = OpenOverlay;
    type Observation = OpenOverlay;
    type HostRequest = crate::Never;
    type HostResponse = crate::Never;
    type WelcomeExtension = Option<crate::Never>;
}

#[test]
fn extension_traffic_never_reaches_the_wire() {
    let observation: SessionObservation<Overlay> = SessionObservation::Extension(OpenOverlay);
    assert_eq!(
        project_host_event(&observation, JsonlCorrelation::default()),
        None
    );
    assert_eq!(host_event_kind(&observation), "overlay");
}

#[test]
fn model_refusal_projects_supplied_detail_without_inventing_one() {
    for detail in [
        None,
        Some("category=fixture; explanation=declined".to_owned()),
    ] {
        let event: SessionObservation<Core> = SessionObservation::ModelRefusal {
            detail: detail.clone(),
        };
        let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["kind"], "model_refusal");
        assert_eq!(value["message"], "model refused the request");
        assert_eq!(
            value.get("detail").and_then(serde_json::Value::as_str),
            detail.as_deref()
        );
        assert_eq!(host_event_kind(&event), "model_refusal");
        assert!(crate::envelope::REQUIRED_STDOUT_KINDS.contains(&"model_refusal"));
    }
}

#[test]
fn a_model_retry_projects_one_required_retry_frame() {
    let event: SessionObservation<Core> =
        SessionObservation::ModelRetry(crate::observation::ModelRetryNotice {
            next_attempt: 3,
            max_attempts: 4,
            delay_ms: 2_000,
            cause_kind: "http_status".to_owned(),
            cause: "provider busy (HTTP 503)".to_owned(),
            output_discarded: false,
        });
    let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["kind"], "retry");
    assert_eq!(value["attempt"], 3);
    assert_eq!(value["max_attempts"], 4);
    assert_eq!(value["delay_ms"], 2_000);
    assert_eq!(value["cause_kind"], "http_status");
    assert_eq!(value["cause"], "provider busy (HTTP 503)");
    assert_eq!(value["output_discarded"], false);
    assert_eq!(host_event_kind(&event), "retry");
    assert!(crate::envelope::required_stdout_kind(&line));
}

#[test]
fn turn_lifecycle_projects_phase_and_error_context() {
    let event: SessionObservation<Core> = SessionObservation::TurnLifecycle(TurnLifecycle {
        turn_id: "00000000-0000-4000-8000-0000000000aa".to_owned(),
        phase: TurnPhase::Failed,
        model_error_kind: Some("RateLimited".to_owned()),
        http_status: Some(429),
    });
    let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["kind"], "turn_lifecycle");
    assert_eq!(value["turn_id"], "00000000-0000-4000-8000-0000000000aa");
    assert_eq!(value["phase"], "failed");
    assert_eq!(value["model_error_kind"], "RateLimited");
    assert_eq!(value["http_status"], 429);
    assert_eq!(host_event_kind(&event), "turn_lifecycle");
    assert!(
        crate::envelope::REQUIRED_STDOUT_KINDS.contains(&"turn_lifecycle"),
        "lifecycle evidence is a required stdout kind"
    );
}

#[test]
fn turn_lifecycle_omits_absent_error_context() {
    let event: SessionObservation<Core> = SessionObservation::TurnLifecycle(TurnLifecycle {
        turn_id: "00000000-0000-4000-8000-0000000000bb".to_owned(),
        phase: TurnPhase::Unresolved,
        model_error_kind: None,
        http_status: None,
    });
    let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["phase"], "unresolved");
    assert!(value.get("model_error_kind").is_none());
    assert!(value.get("http_status").is_none());
}

#[test]
fn turn_phase_labels_match_the_serde_form() {
    for (phase, label) in [
        (TurnPhase::Admitted, "admitted"),
        (TurnPhase::Completed, "completed"),
        (TurnPhase::Refused, "refused"),
        (TurnPhase::Cancelled, "cancelled"),
        (TurnPhase::Failed, "failed"),
        (TurnPhase::RecoveryRequired, "recovery_required"),
        (TurnPhase::Unresolved, "unresolved"),
    ] {
        assert_eq!(phase.as_str(), label);
        assert_eq!(serde_json::to_value(phase).unwrap(), label);
    }
}

#[test]
fn required_kinds_project_for_the_core_protocol() {
    let error: SessionObservation<Core> =
        SessionObservation::Row(TranscriptRow::error(NoticeScope::Host, "boom"));
    assert!(
        project_host_event(&error, JsonlCorrelation::default()).is_none(),
        "product-formatted notices do not bypass Plak"
    );
    assert_eq!(host_event_kind(&error), "row");
}
