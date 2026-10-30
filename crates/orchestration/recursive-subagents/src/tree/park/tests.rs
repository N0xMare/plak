use crate::protocol::{ListAgentsRequest, MessagePriority, MessagePurpose, TopologyErrorCode};
use crate::tree::test_support::*;
use crate::tree::*;

/// A spawned child that already ran one turn, as the turn waiter leaves
/// it when that turn ended without `submit_result`.
fn after_one_turn(tree: &mut ProductAgentTree) -> String {
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    child
}

#[test]
fn a_parked_child_runs_waits_on_its_work_and_settles_no_wait() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    tree.release_attempt_turn(&child).unwrap();
    let waiting = tree.park_child(&child, vec!["p3".to_owned()]).unwrap();
    assert_eq!(waiting.processes, ["p3"]);
    let attempt = tree.attempt(&child).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Running);
    assert!(attempt.turn_id.is_none());
    assert_eq!(attempt.submit_harness_nudge, SubmitNudgeBudget::NONE);
    assert!(!tree.node(&child).unwrap().active_turn);
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);

    // The parent's wait does not settle on a park, and its rows say why.
    assert!(tree.poll_wait("root", &wait_for(&child)).unwrap().is_none());
    let row = tree.wait_row(&child).unwrap();
    assert_eq!(row.waiting_on.as_ref().unwrap().processes, ["p3"]);
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: None,
            },
        )
        .unwrap();
    assert_eq!(
        listed.agents[0]["waiting_on"],
        serde_json::json!({"processes": ["p3"], "agents": []})
    );

    // A wake in flight hides the row; its bound turn ends the park.
    tree.begin_wake(&child);
    assert!(tree.wait_row(&child).unwrap().waiting_on.is_none());
    assert!(tree.is_parked(&child));
    tree.unpark_child(&child);
    assert!(!tree.is_parked(&child));
}

#[test]
fn mail_to_a_parked_child_waits_for_its_wake_instead_of_starting_it() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    let sent = tree
        .send_agent_message("root", &mail(&child, "also check docs"), None)
        .unwrap();
    assert_eq!(sent.disposition, MailDisposition::Queued);
    let mut urgent = mail(&child, "stop and report");
    urgent.priority = Some(MessagePriority::Urgent);
    let steered = tree.send_agent_message("root", &urgent, None).unwrap();
    assert_eq!(steered.disposition, MailDisposition::Steered);
    assert_eq!(tree.wake_mail(&child).len(), 2);
    // Unparked with no bound turn, the same mail would start a turn.
    tree.unpark_child(&child);
    let started = tree
        .send_agent_message("root", &mail(&child, "start"), None)
        .unwrap();
    assert_eq!(started.disposition, MailDisposition::Started);
}

#[test]
fn interrupt_and_close_end_a_park() {
    let (_root, mut tree) = tree();
    let interrupted = after_one_turn(&mut tree);
    tree.release_attempt_turn(&interrupted).unwrap();
    tree.park_child(&interrupted, Vec::new()).unwrap();
    let status = tree
        .interrupt_agent("root", &close_req(&interrupted), None)
        .unwrap();
    assert_eq!(status, NodeStatus::Interrupted);
    assert!(!tree.is_parked(&interrupted));

    let by_root = after_one_turn(&mut tree);
    tree.release_attempt_turn(&by_root).unwrap();
    tree.park_child(&by_root, Vec::new()).unwrap();
    assert_eq!(
        tree.interrupt_running_descendants().unwrap(),
        std::slice::from_ref(&by_root)
    );
    assert_eq!(tree.node(&by_root).unwrap().status, NodeStatus::Interrupted);
    assert!(!tree.is_parked(&by_root));

    let closed = after_one_turn(&mut tree);
    tree.release_attempt_turn(&closed).unwrap();
    tree.park_child(&closed, Vec::new()).unwrap();
    tree.close_agent("root", &close_req(&closed), None).unwrap();
    assert!(!tree.is_parked(&closed));
    assert!(tree.waiting_on(&closed).is_none());
}

/// An interrupt that wins the race against a wake's admission ends the
/// park: the wake may not begin, or may no longer start once begun.
#[test]
fn an_interrupt_refuses_a_wake_begun_or_not() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    assert!(tree.begin_wake(&child));
    assert!(tree.wake_admissible(&child));
    assert!(!tree.begin_wake(&child), "one wake at a time");
    tree.interrupt_agent("root", &close_req(&child), None)
        .unwrap();
    assert!(!tree.wake_admissible(&child));
    assert!(!tree.is_parked(&child));

    let parked = after_one_turn(&mut tree);
    tree.release_attempt_turn(&parked).unwrap();
    tree.park_child(&parked, Vec::new()).unwrap();
    tree.interrupt_agent("root", &close_req(&parked), None)
        .unwrap();
    assert!(!tree.begin_wake(&parked));
}

/// A child whose wake was not admitted is ending its attempt: mail is
/// refused as terminal instead of starting a turn, an interrupt no
/// longer applies, and rows carry no parked marker.
#[test]
fn an_ending_park_refuses_mail_and_ignores_an_interrupt() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    tree.begin_wake(&child);
    assert!(tree.mark_park_ending(&child));
    assert!(tree.waiting_on(&child).is_none());
    let refused = tree
        .send_agent_message("root", &mail(&child, "status?"), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::RecipientAttemptTerminal);
    assert_eq!(
        tree.interrupt_agent("root", &close_req(&child), None)
            .unwrap(),
        NodeStatus::Running
    );
    assert_eq!(
        tree.interrupt_running_descendants().unwrap(),
        [] as [String; 0]
    );
    assert!(tree.is_park_ending(&child));
    tree.unpark_child(&child);
    assert!(!tree.mark_park_ending(&child), "only a parked child ends");
}

#[test]
fn claimed_mail_is_not_a_wake_arrival_until_its_delivery_returns() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    let sent = tree
        .send_agent_message("root", &mail(&child, "also check docs"), None)
        .unwrap();
    tree.set_delivery_claim_for_test(&sent.message_id, true);
    assert_eq!(
        tree.claimed_mail(&child),
        std::slice::from_ref(&sent.message_id)
    );
    assert_eq!(tree.wake_mail(&child), [] as [MailRecord; 0]);
    tree.set_delivery_claim_for_test(&sent.message_id, false);
    assert_eq!(tree.claimed_mail(&child), [] as [String; 0]);
    assert_eq!(tree.wake_mail(&child).len(), 1);
}

#[test]
fn live_child_work_counts_children_that_will_still_report() {
    let (_root, mut tree) = tree();
    let mut snapshot = tree.snapshot.clone();
    snapshot.max_depth = 2;
    snapshot.digest = snapshot.compute_digest();
    tree.install_snapshot(snapshot);
    let hub = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap()
        .agent_id;
    let running = tree.spawn_agent(&hub, &spawn_req(), None).unwrap().agent_id;
    let interrupted = tree.spawn_agent(&hub, &spawn_req(), None).unwrap().agent_id;
    tree.set_active_turn(&interrupted, true).unwrap();
    tree.interrupt_agent(&hub, &close_req(&interrupted), None)
        .unwrap();
    assert_eq!(tree.live_child_work(&hub), std::slice::from_ref(&running));
    tree.submit_result(
        &running,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.to_owned(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&running).unwrap();
    assert_eq!(tree.live_child_work(&hub), [] as [String; 0]);
}

#[test]
fn a_reopen_derives_the_parked_shape_from_durable_rows() {
    let (_root, mut tree) = tree();
    let parked = after_one_turn(&mut tree);
    tree.release_attempt_turn(&parked).unwrap();
    tree.park_child(&parked, Vec::new()).unwrap();
    // Spawned and bound but never started: no turn spent, not parked.
    let unstarted = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&unstarted, "unstarted".to_owned())
        .unwrap();
    // Mid-turn: a bound turn is an in-flight join target instead.
    let running = after_one_turn(&mut tree);
    let interrupted = after_one_turn(&mut tree);
    tree.interrupt_agent("root", &close_req(&interrupted), None)
        .unwrap();
    tree.release_attempt_turn(&interrupted).unwrap();
    // An admitted followup is a fresh attempt with no turn spent: its own
    // Start runs it, never a reopen wake beside it.
    let retasked = after_one_turn(&mut tree);
    tree.interrupt_agent("root", &close_req(&retasked), None)
        .unwrap();
    tree.release_attempt_turn(&retasked).unwrap();
    let mut followup = mail(&retasked, "now the docs");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &followup, None).unwrap();
    assert_eq!(tree.node(&retasked).unwrap().status, NodeStatus::Running);

    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(
        !reopened.is_parked(&parked),
        "the parked map is process-local"
    );
    // A restored tree wakes nothing until the operator resumes it.
    assert_eq!(
        reopened.parked_reopen_targets(),
        [] as [(String, String, String); 0]
    );
    reopened.resume_tree().unwrap();
    let targets = reopened
        .parked_reopen_targets()
        .into_iter()
        .map(|(agent_id, locator, _)| (agent_id, locator))
        .collect::<Vec<_>>();
    assert_eq!(targets, [(parked, "locator".to_owned())]);
    assert!(
        reopened
            .inflight_join_targets()
            .iter()
            .any(|(agent_id, ..)| agent_id == &running)
    );
}

/// Between `/tree resume` and the host's reopen wake, mail to a derived
/// parked child waits for that wake instead of starting a second turn,
/// and an interrupt still ends the park.
#[test]
fn a_resume_holds_derived_parked_children_for_the_reopen_wake() {
    let (_root, mut tree) = tree();
    let held = after_one_turn(&mut tree);
    tree.release_attempt_turn(&held).unwrap();
    tree.park_child(&held, Vec::new()).unwrap();
    let interrupted = after_one_turn(&mut tree);
    tree.release_attempt_turn(&interrupted).unwrap();
    tree.park_child(&interrupted, Vec::new()).unwrap();
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(!reopened.is_parked(&held));
    reopened.resume_tree().unwrap();
    assert!(reopened.is_parked(&held));
    assert_eq!(
        reopened.waiting_on(&held),
        Some(&WaitingOn::default()),
        "a held child reads as parked before its wake names what it waits on"
    );
    let sent = reopened
        .send_agent_message("root", &mail(&held, "status?"), None)
        .unwrap();
    assert_eq!(sent.disposition, MailDisposition::Queued);
    assert_eq!(reopened.wake_mail(&held).len(), 1);
    assert_eq!(
        reopened
            .interrupt_agent("root", &close_req(&interrupted), None)
            .unwrap(),
        NodeStatus::Interrupted
    );

    // The wake ran and its turn waiter parked the child again: releasing
    // the hold leaves that park alone.
    reopened
        .bind_attempt_turn(&held, "00000000-0000-0000-0000-000000000002")
        .unwrap();
    reopened.set_active_turn(&held, true).unwrap();
    reopened.release_attempt_turn(&held).unwrap();
    reopened.park_child(&held, vec!["p1".to_owned()]).unwrap();
    reopened.release_reopen_hold(&held);
    assert_eq!(reopened.waiting_on(&held).unwrap().processes, ["p1"]);
    reopened.unpark_child(&held);

    // A wake that never ran releases the hold outright.
    let mut again = ProductAgentTree::reopen(reopened.to_persisted());
    again.resume_tree().unwrap();
    assert!(again.is_parked(&held));
    again.release_reopen_hold(&held);
    assert!(!again.is_parked(&held));
}

/// The reopen wake of a held child starts only from the hold: an
/// interrupt before it began, or while it is being admitted, ends it,
/// and a released hold is not woken.
#[test]
fn a_reopen_wake_begins_only_from_a_hold_an_interrupt_has_not_ended() {
    let (_root, mut tree) = tree();
    let woken = after_one_turn(&mut tree);
    tree.release_attempt_turn(&woken).unwrap();
    tree.park_child(&woken, Vec::new()).unwrap();
    let interrupted_early = after_one_turn(&mut tree);
    tree.release_attempt_turn(&interrupted_early).unwrap();
    tree.park_child(&interrupted_early, Vec::new()).unwrap();
    let interrupted_late = after_one_turn(&mut tree);
    tree.release_attempt_turn(&interrupted_late).unwrap();
    tree.park_child(&interrupted_late, Vec::new()).unwrap();
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(!reopened.begin_reopen_wake(&woken), "nothing is held yet");
    reopened.resume_tree().unwrap();

    assert!(reopened.begin_reopen_wake(&woken));
    assert!(reopened.reopen_wake_admissible(&woken));
    assert!(
        !reopened.begin_reopen_wake(&woken),
        "one reopen wake at a time"
    );
    assert!(reopened.waiting_on(&woken).is_none());
    let sent = reopened
        .send_agent_message("root", &mail(&woken, "status?"), None)
        .unwrap();
    assert_eq!(sent.disposition, MailDisposition::Queued);
    reopened.release_reopen_hold(&woken);
    assert!(!reopened.is_parked(&woken));

    reopened
        .interrupt_agent("root", &close_req(&interrupted_early), None)
        .unwrap();
    assert!(!reopened.begin_reopen_wake(&interrupted_early));

    assert!(reopened.begin_reopen_wake(&interrupted_late));
    assert_eq!(
        reopened
            .interrupt_agent("root", &close_req(&interrupted_late), None)
            .unwrap(),
        NodeStatus::Interrupted
    );
    assert!(!reopened.reopen_wake_admissible(&interrupted_late));
    assert!(!reopened.is_parked(&interrupted_late));
}

#[test]
fn await_receipts_stay_bounded_across_many_wakes() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    for wake in 0..20_u32 {
        let operation = format!("wait-{wake}");
        tree.remember_success(
            &operation,
            &child,
            AWAIT_RECEIPT_KIND,
            &serde_json::json!({"wake": wake}),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(tree.retained_wait_receipts(&child), [operation]);
        tree.release_attempt_turn(&child).unwrap();
        tree.park_child(&child, Vec::new()).unwrap();
        assert_eq!(tree.retained_wait_receipts(&child), [] as [String; 0]);
        tree.begin_wake(&child);
        tree.bind_attempt_turn(&child, format!("00000000-0000-0000-0000-{:012}", wake + 2))
            .unwrap();
        tree.set_active_turn(&child, true).unwrap();
        tree.unpark_child(&child);
    }
    assert_eq!(tree.attempt(&child).unwrap().usage.turns, 21);
}
