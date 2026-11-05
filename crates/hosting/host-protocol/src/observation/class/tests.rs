use super::{Delivery, ObservationClass};
use crate::channel::latest_state_event;
use crate::envelope::{JsonlCorrelation, REQUIRED_STDOUT_KINDS};
use crate::jsonl::{host_event_kind, project_host_event};
use crate::observation::SessionObservation;

type Observation = SessionObservation<crate::intent::Core>;

fn class_name(class: ObservationClass) -> &'static str {
    match class {
        ObservationClass::State => "state",
        ObservationClass::Event(Delivery::Required) => "required",
        ObservationClass::Event(Delivery::Droppable) => "droppable",
        ObservationClass::Addressed => "addressed",
    }
}

/// The classes of the plain-valued variants, by their content-free kind.
#[test]
fn plain_observations_take_their_pinned_class() {
    let table: Vec<String> = crate::observation::class_samples()
        .iter()
        .map(|observation| {
            format!(
                "{} {}",
                host_event_kind(observation),
                class_name(observation.class())
            )
        })
        .collect();
    assert_eq!(
        table,
        [
            "job_started required",
            "job_promoted required",
            "activity state",
            "attached state",
            "busy state",
            "prompt_history state",
            "prompt_correlation_reset addressed",
            "intent_started addressed",
            "draft_accepted addressed",
            "draft_rejected addressed",
            "draft_restored addressed",
            "user_message droppable",
            "assistant droppable",
            "assistant_done droppable",
            "model_refusal required",
            "clear_view required",
            "reasoning droppable",
            "reasoning_sealed droppable",
            "approval_required required",
            "approval_resolved required",
            "steer_ack addressed",
            "reattach_ready state",
            "session_opening state",
            "model_request_started droppable",
            "model_request_queued droppable",
            "credential_wait required",
            "session_opened state",
            "tree_unavailable addressed",
            "pending_continuation state",
            "last_user_prompt_at state",
            "processes state",
        ]
    );
}

/// A state is what the mailbox already keeps as a latest value, so the
/// attach snapshot holds exactly what a saturated mailbox would.
#[test]
fn every_state_is_a_latest_value_in_the_mailbox() {
    for observation in crate::observation::class_samples() {
        if observation.class() == ObservationClass::State {
            assert!(
                latest_state_event(&observation),
                "{}",
                host_event_kind(&observation)
            );
        }
    }
}

/// What JSONL v1 must never drop stays undroppable on wire v2, except the
/// navigator's answers, which go to the attachment that asked.
#[test]
fn a_required_v1_line_is_never_droppable() {
    for observation in crate::observation::class_samples() {
        let Some(line) = project_host_event(&observation, JsonlCorrelation::default()) else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        let kind = value["kind"].as_str().unwrap();
        if !REQUIRED_STDOUT_KINDS.contains(&kind) {
            continue;
        }
        let class = observation.class();
        assert!(
            class != ObservationClass::Event(Delivery::Droppable),
            "{kind} is required in v1"
        );
        if class == ObservationClass::Addressed {
            assert!(
                matches!(
                    observation,
                    Observation::TopologyNavigatorSnapshot(_)
                        | Observation::TopologyNavigatorUnavailable { .. }
                ),
                "{kind}"
            );
        }
    }
}
