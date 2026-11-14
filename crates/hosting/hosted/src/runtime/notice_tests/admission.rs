//! A newly admitted user turn restores the process-notice guarantee.

use super::*;

#[test]
fn a_user_prompt_after_an_idle_interrupt_can_receive_notice_in_turn() {
    let root = tempfile::tempdir().unwrap();
    let config = process_host(
        root.path(),
        serde_json::json!([
            {"output":{"text":"idle before the operator interrupt"}},
            {"output":{"tool": start_call("read line", 600_000, true)},"finish":"tool_calls"},
            {"output":{"text":"the new user turn received its notice"}}
        ]),
    );
    let id = config.session.id.clone();
    let records = config.store.clone();
    install_live_descendant_on_hub(&id, &config.cwd);
    let hub = crate::topology_runtime::lookup(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first turn".into()));
    wait_for(
        &events,
        &mut seen,
        "the first idle boundary",
        turn_completed,
    );
    send_attached_action(&actions, UserAction::Interrupt);
    wait_for(
        &events,
        &mut seen,
        "the idle interrupt's durable suppression",
        |event| {
            matches!(
                event,
                HostEvent::PendingContinuation {
                    auto_continue_paused: true,
                    ..
                }
            )
        },
    );
    assert!(
        records
            .load_views(&id)
            .unwrap()
            .unwrap()
            .wake_suppressed(&records.load_intents(&id).unwrap())
    );
    send_attached_action(&actions, UserAction::Prompt("observe again".into()));
    wait_for(
        &events,
        &mut seen,
        "the new turn's quiet process",
        |event| {
            matches!(event, HostEvent::ToolLine { name, state, .. }
            if name == "process_session_start" && state == "running")
        },
    );
    send_question(&hub);
    wait_for(
        &events,
        &mut seen,
        "the new turn yielding on Notice",
        turn_completed,
    );
    assert!(
        format!("{seen:?}").contains("New messages are waiting; end this turn to receive them.")
    );
    assert_eq!(
        records
            .load_views(&id)
            .unwrap()
            .unwrap()
            .state
            .counters
            .root_turns,
        2
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}
