//! Ready-terminal precedence at a quit deadline, without a wall-clock measurement.

use crate::runtime::recover_tests::{script, scripted_host_with, wait_for};
use crate::runtime::spawn_host_worker;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

#[test]
fn quit_observes_a_ready_terminal_before_an_expired_join_deadline() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {"stream": [{"text": "ready to cancel"}, {"stall": 30000}]}
        ])),
    );
    let id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("start".into()));
    wait_for(
        &events,
        &mut seen,
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("ready to cancel")),
    );
    // Cancellation installs the result before reporting settlement. Simulate
    // a host descheduled past its join deadline before polling that result.
    super::turn_drive::quit_trace::expire_next_join(&id);
    send_attached_action(&actions, UserAction::Quit);
    assert_eq!(
        worker.join().unwrap().unwrap(),
        super::HostQuitOutcome::Clean
    );
    seen.extend(events.try_iter());
    assert!(
        seen.iter().any(|event| matches!(event,
            HostEvent::TurnLifecycle(lifecycle)
                if lifecycle.phase == plakous_host_protocol::TurnPhase::Cancelled
        )),
        "{seen:#?}"
    );
}
