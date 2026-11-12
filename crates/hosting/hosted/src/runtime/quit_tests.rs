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
    super::turn_drive::quit_trace::expire_next_join(&id);
    send_attached_action(&actions, UserAction::Prompt("start".into()));
    wait_for(
        &events,
        &mut seen,
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("ready to cancel")),
    );
    // Expire only after the fixture's result receiver is genuinely ready;
    // control settlement can precede notification of the result future.
    send_attached_action(&actions, UserAction::Quit);
    let outcome = worker.join().unwrap().unwrap();
    seen.extend(events.try_iter());
    assert_eq!(
        outcome,
        super::HostQuitOutcome::Clean,
        "steps={:?}; events={seen:#?}",
        super::turn_drive::quit_trace::steps(&id)
    );
    assert!(
        seen.iter().any(|event| matches!(event,
            HostEvent::TurnLifecycle(lifecycle)
                if lifecycle.phase == plakous_host_protocol::TurnPhase::Cancelled
        )),
        "{seen:#?}"
    );
}
