//! A busy turn takes an intent the moment it arrives (spec 02 §1.1), not at
//! its next 20 ms delta-coalescing tick.
//!
//! A turn stalled in its provider call leaves its select two ways to wake:
//! the intent arm and the tick. The test build records which one ended the
//! pass before each follow-up was admitted. The host runs on the facade's
//! multi-thread executor, whose clock cannot be paused, so the test runs on
//! wall time in the serial lane. A send that lands between a tick's wake and
//! the next mailbox read is taken after that tick; the window is tens of
//! microseconds in a 20 ms period, so one such send in ten is allowed.

use crate::runtime::recover_tests::{scripted_host_with, wait_for};
use crate::runtime::spawn_host_worker;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::runtime::turn_drive::wake_trace::wake_before;
use std::sync::Arc;
use std::time::{Duration, Instant};

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

#[test]
fn a_busy_turn_takes_an_intent_without_waiting_for_the_coalesce_tick() {
    let root = tempfile::tempdir().unwrap();
    let plan = serde_json::json!({
        "version": 1,
        "steps": [{"stream": [{"text": "busy prefix"}, {"stall": 30000}]}]
    });
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> =
        Arc::new(serde_json::from_value(plan).unwrap());
    let (config, _) = scripted_host_with(root.path(), plan);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("start".into()));
    wait_for(
        &events,
        &mut seen,
        "the stalled turn's prefix",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("busy prefix")),
    );
    let mut taken = Vec::new();
    for send in 0..10_u64 {
        // Spread the sends across the tick's phase.
        std::thread::sleep(Duration::from_millis(7 + 3 * send));
        let text = format!("intent-wake follow-up {send}");
        let sent = Instant::now();
        send_attached_action(&actions, UserAction::FollowUp(text.clone().into()));
        wait_for(&events, &mut seen, "the follow-up taken", |event| {
            matches!(event, HostEvent::IntentStarted(_))
        });
        taken.push(sent.elapsed());
        wait_for(
            &events,
            &mut seen,
            "the follow-up admitted",
            |event| matches!(event, HostEvent::PromptAdmitted(item) if item.text == text),
        );
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let wakes: Vec<_> = (0..10)
        .map(|send| wake_before(&format!("intent-wake follow-up {send}")))
        .collect();
    taken.sort();
    eprintln!("send to take, sorted: {taken:?}; wakes before each take: {wakes:?}");
    let on_intent = wakes.iter().filter(|wake| **wake == Some("intent")).count();
    assert!(
        on_intent >= 9,
        "a busy turn took its follow-ups after these wakes: {wakes:?}"
    );
}
