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
    assert_eq!(
        tree.interrupt_running_descendants_for(InterruptCause::Stopped)
            .unwrap(),
        [] as [String; 0]
    );
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Running);
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

    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(
        !reopened.is_parked(&parked),
        "the parked map is process-local"
    );
    // The rows prove the parked child idle between turns: the reopen law
    // interrupts it (owner lost) and owes it nothing.
    assert_eq!(
        reopened.node(&parked).unwrap().status,
        NodeStatus::Interrupted
    );
    assert!(!reopened.recovery_owed(&parked));
    assert_eq!(
        reopened.parked_reopen_targets(),
        [] as [(String, String, String); 0]
    );
    // A bound turn is owed a recover_node and joined, not marked.
    assert!(reopened.recovery_owed(&running));
    assert_eq!(reopened.node(&running).unwrap().status, NodeStatus::Running);
    assert!(
        reopened
            .inflight_join_targets()
            .iter()
            .any(|(agent_id, ..)| agent_id == &running)
    );
    // Neither an unstarted spawn nor a fresh followup spent a turn.
    for untouched in [&unstarted, &retasked] {
        assert_eq!(
            reopened.node(untouched).unwrap().status,
            NodeStatus::Running
        );
        assert!(!reopened.recovery_owed(untouched));
    }
}

/// A child interrupted while its turn was still bound (an owner-lost mark,
/// or an operator interrupt, written before the turn's terminal was
/// projected) is owed its recover_node like a running one: only that run
/// projects the feed that ends the turn and releases its binding.
#[test]
fn a_bound_child_interrupted_before_the_loss_is_still_owed() {
    let (_root, mut tree) = tree();
    let marked = after_one_turn(&mut tree);
    tree.mark_owner_lost(&marked);
    let interrupted = after_one_turn(&mut tree);
    tree.interrupt_agent("root", &close_req(&interrupted), None)
        .unwrap();
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    for agent in [&marked, &interrupted] {
        assert_eq!(
            reopened.node(agent).unwrap().status,
            NodeStatus::Interrupted
        );
        assert!(reopened.recovery_owed(agent), "{agent} keeps its binding");
        assert!(
            reopened
                .inflight_join_targets()
                .iter()
                .any(|(agent_id, ..)| agent_id == agent)
        );
    }
}

/// A child interrupted with a command no fact names and the tree has not
/// withdrawn (an owner-lost mark that projected its feed but was lost before
/// the withdrawal) is owed the rest of that mark.
#[test]
fn an_interrupted_child_with_an_unwithdrawn_command_is_owed() {
    let (_root, mut tree) = tree();
    let child = after_one_turn(&mut tree);
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    let attempt = tree.node(&child).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        "wake-lost",
        &child,
        attempt,
        AgentCommandKind::Wake,
        "text wake-lost".to_owned(),
        Vec::new(),
    ))
    .unwrap();
    tree.mark_owner_lost(&child);
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Interrupted);
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(
        reopened.node(&child).unwrap().status,
        NodeStatus::Interrupted
    );
    assert!(reopened.recovery_owed(&child));
}

/// A parked child holding a prepared command that is not its own start (a
/// Followup, a transient Resume, or a Wake of another attempt) is marked
/// owner-lost and owed in the same reopen, and a second reopen of those
/// rows classifies it the same way.
#[test]
fn a_parked_child_with_another_unresolved_command_is_marked_and_owed_once() {
    let other = AttemptId::from_persisted("other-attempt".to_owned());
    for (kind, label) in [
        (AgentCommandKind::Followup, "followup"),
        (AgentCommandKind::Resume, "resume"),
        (AgentCommandKind::Wake, "other-wake"),
    ] {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        tree.release_attempt_turn(&child).unwrap();
        tree.park_child(&child, Vec::new()).unwrap();
        let attempt = if kind == AgentCommandKind::Wake {
            Some(other.clone())
        } else {
            tree.node(&child).unwrap().attempt_id.clone()
        };
        tree.record_command(AgentCommand::intent(
            label,
            &child,
            attempt,
            kind,
            format!("text {label}"),
            Vec::new(),
        ))
        .unwrap();
        let first = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(
            first.node(&child).unwrap().status,
            NodeStatus::Interrupted,
            "{label}"
        );
        assert!(
            first.recovery_owed(&child),
            "{label}: owed on the first reopen"
        );
        let second = ProductAgentTree::reopen(first.to_persisted());
        assert_eq!(
            second.node(&child).unwrap().status,
            NodeStatus::Interrupted,
            "{label}"
        );
        assert!(second.recovery_owed(&child), "{label}: owed again");
    }
}

/// A parked child with a start the lost process prepared and no fact names
/// is marked like every parked child: no start is rejoined after a take.
/// The unfacted start leaves it owed, so its recover_node cancels a turn the
/// runtime admitted for it and withdraws one it never admitted.
#[test]
fn an_owed_parked_start_is_marked_owner_lost_and_owed() {
    let (_root, mut tree) = tree();
    let woken = after_one_turn(&mut tree);
    tree.release_attempt_turn(&woken).unwrap();
    tree.park_child(&woken, Vec::new()).unwrap();
    let interrupted = after_one_turn(&mut tree);
    tree.release_attempt_turn(&interrupted).unwrap();
    tree.park_child(&interrupted, Vec::new()).unwrap();
    for (agent, command) in [(&woken, "wake-w"), (&interrupted, "wake-i")] {
        let attempt = tree.node(agent).unwrap().attempt_id.clone();
        tree.record_command(AgentCommand::intent(
            command,
            agent,
            attempt,
            AgentCommandKind::Wake,
            format!("text {command}"),
            Vec::new(),
        ))
        .unwrap();
    }
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    for agent in [&woken, &interrupted] {
        assert_eq!(
            reopened.node(agent).unwrap().status,
            NodeStatus::Interrupted
        );
        assert!(reopened.recovery_owed(agent), "marked, and owed");
        assert!(!reopened.is_parked(agent));
    }
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
