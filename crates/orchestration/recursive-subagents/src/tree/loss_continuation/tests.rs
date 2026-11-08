use crate::tree::test_support::*;
use crate::tree::*;
use plakous_host_protocol::views::{InterruptCauseView, LossContinuationView};

/// A child that ran one turn and parked between turns, then lost its host:
/// the reopen marks it owner-lost.
fn reopened_after_loss() -> (tempfile::TempDir, ProductAgentTree, String) {
    let (root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    (root, reopened, child)
}

fn view(tree: &ProductAgentTree, agent_id: &str) -> plakous_host_protocol::views::NodeView {
    view::project_tree_view(tree)
        .nodes
        .into_iter()
        .find(|node| node.agent_id == agent_id)
        .unwrap()
}

/// The owner-lost mark leaves the child's continuation held: no decision
/// names the loss and no turn ran since. The rows carry the cause and the
/// held state, and a second reopen derives the same.
#[test]
fn an_owner_lost_child_holds_its_continuation() {
    let (_root, tree, child) = reopened_after_loss();
    let continuation = tree.loss_continuation(&child).unwrap();
    assert_eq!(continuation.state, LossContinuationState::Held);
    assert_eq!(continuation.cause, ContinuationCause::OwnerLost);
    assert_eq!(continuation.command_id, None);
    let row = view(&tree, &child);
    assert_eq!(row.interrupt_cause, Some(InterruptCauseView::OwnerLost));
    assert_eq!(row.continuation, Some(LossContinuationView::Held));
    let again = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(again.loss_continuation(&child), Some(continuation));
    assert_eq!(again.loss_continuations().len(), 1);
}

/// A parent's `wait_agent` row says why its child stopped and where the
/// continuation stands, in the same words the operator's rows use; a child
/// that was never interrupted carries neither.
#[test]
fn a_wait_row_carries_the_cause_and_the_continuation() {
    let (_root, mut tree, child) = reopened_after_loss();
    let row = serde_json::to_value(tree.wait_row(&child).unwrap()).unwrap();
    assert_eq!(row["cause"], "owner_lost");
    assert_eq!(row["continuation"], "held");
    tree.record_loss_continuation(&child, 7).unwrap();
    let row = serde_json::to_value(tree.wait_row(&child).unwrap()).unwrap();
    assert_eq!(row["continuation"], "pending");
    // Resumed and running again: its latest interrupt is stale, so the row
    // no longer says why it stopped.
    tree.nodes.get_mut(&child).unwrap().status = NodeStatus::Running;
    assert_eq!(tree.interrupt_cause_of(&child), None);
    let row = serde_json::to_value(tree.wait_row(&child).unwrap()).unwrap();
    assert!(row.get("cause").is_none(), "{row}");
    let (_other, mut other) = test_support::tree();
    let fresh = other
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let row = serde_json::to_value(other.wait_row(&fresh).unwrap()).unwrap();
    assert!(row.get("cause").is_none() && row.get("continuation").is_none());
}

/// Consent is one durable decision: recording it again returns the same
/// command, and reopens of its rows read the same pending continuation,
/// whose command a later owner-lost mark keeps instead of withdrawing.
#[test]
fn a_recorded_consent_is_one_pending_decision_across_reopens() {
    let (_root, mut tree, child) = reopened_after_loss();
    let command = tree.record_loss_continuation(&child, 7).unwrap();
    assert!(command.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX));
    assert!(command.len() <= 128);
    let decisions = |tree: &ProductAgentTree| {
        tree.operations()
            .values()
            .filter(|row| row.tool_name == ContinuationIntent::KIND)
            .count()
    };
    assert_eq!(decisions(&tree), 1);
    assert_eq!(tree.record_loss_continuation(&child, 8).unwrap(), command);
    assert_eq!(
        decisions(&tree),
        1,
        "a pending decision is not recorded again"
    );
    let pending = tree.loss_continuation(&child).unwrap();
    assert_eq!(pending.state, LossContinuationState::Pending);
    assert_eq!(pending.command_id.as_deref(), Some(command.as_str()));
    assert!(tree.loss_continuation_admissible(&child, &command));
    assert!(!tree.loss_continuation_admissible(&child, "another"));
    // The continuation's command was prepared and the process was lost
    // before any fact named it.
    let attempt = tree.node(&child).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        command.clone(),
        &child,
        attempt,
        AgentCommandKind::Resume,
        "continue",
        Vec::new(),
    ))
    .unwrap();
    assert!(tree.is_loss_continuation_command(&child, &command));
    for _ in 0..2 {
        tree = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(tree.loss_continuation(&child), Some(pending.clone()));
        assert!(
            !tree.recovery_owed(&child),
            "a recorded continuation's command is resubmitted, not withdrawn"
        );
        assert_eq!(decisions(&tree), 1);
    }
}

/// A held continuation ends through actions that already exist, with no
/// record of its own: another interrupt, a close, or a later turn.
#[test]
fn a_held_continuation_ends_by_interrupt_close_or_a_later_turn() {
    let (_root, mut interrupted, child) = reopened_after_loss();
    interrupted
        .interrupt_agent("root", &close_req(&child), None)
        .unwrap();
    assert_eq!(interrupted.loss_continuation(&child), None);
    assert_eq!(
        view(&interrupted, &child).interrupt_cause,
        Some(InterruptCauseView::Parent),
        "a parent's interrupt records its cause"
    );

    let (_root, mut closed, child) = reopened_after_loss();
    closed
        .close_agent("root", &close_req(&child), None)
        .unwrap();
    assert_eq!(closed.loss_continuation(&child), None);

    let (_root, mut resumed, child) = reopened_after_loss();
    resumed.set_active_turn(&child, true).unwrap();
    assert_eq!(resumed.loss_continuation(&child), None);
    assert!(
        resumed.record_loss_continuation(&child, 1).is_err(),
        "nothing is held once another turn runs"
    );
}

/// A continuation whose hold ended keeps nothing: its prepared command is
/// no longer kept from withdrawal, so the reopen law owes the child, and
/// two reopens of the rows read the same.
#[test]
fn an_ended_continuation_keeps_no_prepared_command() {
    let (_root, mut tree, child) = reopened_after_loss();
    let command = tree.record_loss_continuation(&child, 7).unwrap();
    let attempt = tree.node(&child).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        command.clone(),
        &child,
        attempt,
        AgentCommandKind::Resume,
        "continue",
        Vec::new(),
    ))
    .unwrap();
    tree.interrupt_agent("root", &close_req(&child), None)
        .unwrap();
    assert_eq!(tree.loss_continuation(&child), None);
    assert!(!tree.is_loss_continuation_command(&child, &command));
    for _ in 0..2 {
        tree = ProductAgentTree::reopen(tree.to_persisted());
        assert!(
            tree.recovery_owed(&child),
            "the stranded command is withdrawn by recover_node"
        );
        assert!(!tree.is_loss_continuation_command(&child, &command));
    }
}

/// A spent attempt is not held: the continuation would be a turn past
/// `max_turns`, so nothing is held and consent is refused.
#[test]
fn a_spent_attempt_holds_no_continuation() {
    let (_root, mut tree) = tree();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::ReadOnly;
    binding.max_turns = Some(1);
    tree.pin_template("review", binding);
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    assert!(tree.attempt_turns_spent(&child));
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    reopened.mark_owner_lost(&child);
    assert_eq!(reopened.loss_continuation(&child), None);
    assert!(reopened.record_loss_continuation(&child, 1).is_err());
    // It stays interrupted, as an interrupted spent attempt does, until its
    // parent closes it.
    assert_eq!(
        reopened.node(&child).unwrap().status,
        NodeStatus::Interrupted
    );
    assert_eq!(
        reopened.attempt(&child).unwrap().status,
        AttemptStatus::Running
    );
    reopened
        .close_agent("root", &close_req(&child), None)
        .unwrap();
    assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
}

/// A root stop ends a held child's continuation, though the child is not
/// running: stops stay stopped.
#[test]
fn a_root_stop_ends_a_held_continuation() {
    let (_root, mut tree, child) = reopened_after_loss();
    assert_eq!(
        tree.interrupt_running_descendants().unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(tree.loss_continuation(&child), None);
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(reopened.loss_continuation(&child), None);
}

/// A decided continuation whose command a fact settled is neither pending
/// nor resubmitted: the runtime answered it, so nothing is owed again.
#[test]
fn a_continuation_whose_command_has_a_fact_is_not_pending() {
    let (_root, mut tree, child) = reopened_after_loss();
    let command = tree.record_loss_continuation(&child, 1).unwrap();
    let attempt = tree.node(&child).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        command.clone(),
        &child,
        attempt,
        AgentCommandKind::Resume,
        "continue",
        Vec::new(),
    ))
    .unwrap();
    tree.commands.get_mut(&command).unwrap().fact = Some(CommandFact::Rejected);
    assert_eq!(tree.loss_continuation(&child), None);
    assert!(!tree.loss_continuation_admissible(&child, &command));
    assert!(tree.record_loss_continuation(&child, 2).is_err());
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(reopened.loss_continuation(&child), None);
}

/// A tree with a child whose turn is running, one parked between turns,
/// and one a lost host left held.
fn working_children() -> (tempfile::TempDir, ProductAgentTree, [String; 3]) {
    let (root, mut tree) = tree();
    let mut spawn = || {
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id
    };
    let [running, parked, held] = [spawn(), spawn(), spawn()];
    for (child, turn) in [
        (&running, "00000000-0000-0000-0000-000000000001"),
        (&parked, "00000000-0000-0000-0000-000000000002"),
        (&held, "00000000-0000-0000-0000-000000000003"),
    ] {
        tree.bind_session_locator(child, "locator".to_owned())
            .unwrap();
        tree.set_active_turn(child, true).unwrap();
        tree.bind_attempt_turn(child, turn).unwrap();
        accept_start(&mut tree, child, turn);
    }
    for child in [&parked, &held] {
        tree.release_attempt_turn(child).unwrap();
        tree.park_child(child, Vec::new()).unwrap();
    }
    tree.mark_owner_lost(&held);
    (root, tree, [running, parked, held])
}

/// Records `child`'s Start command as accepted as `turn`.
fn accept_start(tree: &mut ProductAgentTree, child: &str, turn: &str) {
    let command = format!("start:{child}");
    let attempt = tree.node(child).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        command.clone(),
        child,
        attempt,
        AgentCommandKind::Start,
        "assignment",
        Vec::new(),
    ))
    .unwrap();
    let accepted = tree.commands.get_mut(&command).unwrap();
    accepted.fact = Some(CommandFact::Accepted);
    // This fixture has no feed: the acceptance sits at the fold position a
    // later intent records, as a host's projection leaves it.
    accepted.accepted_seq = Some(0);
    accepted.turn_id = Some(turn.to_owned());
}

/// A child reserved for startup whose Start was never accepted is not
/// handed off: a continuation would reach it without its assignment. Its
/// startup is left to recovery.
#[test]
fn a_handoff_skips_a_child_whose_assignment_never_started() {
    let (_root, mut tree, [running, parked, _]) = working_children();
    let starting = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.set_active_turn(&starting, true).unwrap();
    let mut handed_off = tree.record_handoff().unwrap();
    handed_off.sort();
    let mut expected = vec![running, parked];
    expected.sort();
    assert_eq!(handed_off, expected);
    assert_eq!(tree.node(&starting).unwrap().status, NodeStatus::Running);
    assert_eq!(tree.loss_continuation(&starting), None);
}

/// A planned handoff interrupts each running or parked child with cause
/// `handoff`, keeps its attempt, and leaves a child held after a loss held
/// as it was. The next host reads the same rows: nothing is marked again,
/// and each handed-off child is held as a handoff, which continues by
/// itself.
#[test]
fn a_handoff_interrupts_running_and_parked_children_and_keeps_a_hold() {
    let (_root, mut tree, [running, parked, held]) = working_children();
    let mut handed_off = tree.record_handoff().unwrap();
    handed_off.sort();
    let mut expected = vec![running.clone(), parked.clone()];
    expected.sort();
    assert_eq!(handed_off, expected);
    for child in [&running, &parked] {
        assert_eq!(tree.node(child).unwrap().status, NodeStatus::Interrupted);
        assert_eq!(tree.attempt(child).unwrap().status, AttemptStatus::Running);
        let continuation = tree.loss_continuation(child).unwrap();
        assert_eq!(continuation.cause, ContinuationCause::Handoff);
        assert_eq!(continuation.state, LossContinuationState::Held);
        assert_eq!(
            view(&tree, child).interrupt_cause,
            Some(InterruptCauseView::Handoff)
        );
    }
    assert_eq!(
        tree.loss_continuation(&held).unwrap().cause,
        ContinuationCause::OwnerLost
    );
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    reopened.apply_reopen_law();
    for child in [&running, &parked] {
        assert_eq!(
            reopened.loss_continuation(child).map(|held| held.cause),
            Some(ContinuationCause::Handoff),
            "the reopen law marks a handed-off child nothing new"
        );
    }
    assert_eq!(reopened.record_handoff().unwrap(), Vec::<String>::new());
}

/// A stop is final: a later handoff does not replace it, so the next host
/// does not continue the stopped children.
#[test]
fn a_handoff_does_not_continue_a_stopped_child() {
    let (_root, mut tree, [running, parked, held]) = working_children();
    tree.interrupt_running_descendants_for(InterruptCause::Stopped)
        .unwrap();
    assert_eq!(tree.record_handoff().unwrap(), Vec::<String>::new());
    for child in [&running, &parked] {
        assert_eq!(
            view(&tree, child).interrupt_cause,
            Some(InterruptCauseView::Stopped)
        );
        assert_eq!(tree.loss_continuation(child), None);
    }
    assert_eq!(tree.loss_continuation(&held), None);
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    for child in [&running, &parked, &held] {
        assert_eq!(reopened.loss_continuation(child), None);
    }
}

#[test]
fn a_session_stop_interrupts_an_unadmitted_spawn_and_reopen_never_hands_it_off() {
    let (_root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    assert!(!tree.node(&child).unwrap().active_turn);
    assert!(tree.node(&child).unwrap().session_locator.is_none());
    assert_eq!(tree.record_handoff().unwrap(), Vec::<String>::new());
    let mut tree = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Running);
    assert_eq!(
        tree.interrupt_running_descendants_for(InterruptCause::Stopped)
            .unwrap(),
        vec![child.clone()]
    );
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Interrupted);
    assert_eq!(
        view(&tree, &child).interrupt_cause,
        Some(InterruptCauseView::Stopped)
    );
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    reopened.apply_reopen_law();
    assert_eq!(
        reopened.node(&child).unwrap().status,
        NodeStatus::Interrupted
    );
    assert_eq!(reopened.loss_continuation(&child), None);
    assert_eq!(reopened.recovery_owed_targets(), [] as [String; 0]);
    assert_eq!(
        reopened.parked_reopen_targets(),
        [] as [(String, String, String); 0]
    );
    assert_eq!(
        reopened.recovery_need(&child).unwrap(),
        RecoveryNeed::NotRequired
    );
    assert_eq!(reopened.record_handoff().unwrap(), Vec::<String>::new());
}

/// One running child, and nothing else a handoff could select.
fn one_running() -> (tempfile::TempDir, ProductAgentTree, String) {
    let (root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    accept_start(&mut tree, &child, "00000000-0000-0000-0000-000000000001");
    (root, tree, child)
}

/// A stopped child that is running again, still on the turn the stop
/// interrupted, is not cascaded or handed off: the stop stays the cause.
#[test]
fn a_stopped_child_is_not_interrupted_again_without_a_later_turn() {
    let (_root, mut tree, running) = one_running();
    tree.interrupt_running_descendants_for(InterruptCause::Stopped)
        .unwrap();
    tree.set_active_turn(&running, true).unwrap();
    assert_eq!(tree.node(&running).unwrap().status, NodeStatus::Running);
    assert_eq!(tree.record_handoff().unwrap(), Vec::<String>::new());
    assert_eq!(
        tree.interrupt_running_descendants_for(InterruptCause::Cascade)
            .unwrap(),
        Vec::<String>::new()
    );
    // Running again, the node does not project the cause. The record is
    // still the stop: nothing replaced it.
    assert!(tree.operations().values().any(|operation| {
        interrupt_cause(operation).ok().flatten().flatten() == Some(InterruptCause::Stopped)
    }));
    assert!(!tree.operations().values().any(|operation| {
        matches!(
            interrupt_cause(operation).ok().flatten().flatten(),
            Some(InterruptCause::Handoff | InterruptCause::Cascade)
        )
    }));
}

/// A child mail resumed onto a new turn of the same attempt after a stop
/// is interrupted again: a cascade and a handoff both reach it.
#[test]
fn a_child_resumed_after_a_stop_is_interrupted_again() {
    let resume = |tree: &mut ProductAgentTree, child: &str| {
        tree.interrupt_running_descendants_for(InterruptCause::Stopped)
            .unwrap();
        tree.release_attempt_turn(child).unwrap();
        tree.set_active_turn(child, true).unwrap();
        tree.bind_attempt_turn(child, "00000000-0000-0000-0000-000000000099")
            .unwrap();
    };
    let (_root, mut handed, running) = one_running();
    resume(&mut handed, &running);
    assert_eq!(handed.record_handoff().unwrap(), vec![running.clone()]);
    assert_eq!(
        view(&handed, &running).interrupt_cause,
        Some(InterruptCauseView::Handoff)
    );
    let (_root, mut cascaded, running) = one_running();
    resume(&mut cascaded, &running);
    assert_eq!(
        cascaded
            .interrupt_running_descendants_for(InterruptCause::Cascade)
            .unwrap(),
        vec![running.clone()]
    );
    assert_eq!(
        view(&cascaded, &running).interrupt_cause,
        Some(InterruptCauseView::Cascade)
    );
}

/// A command accepted after the stop, on the turn the stop interrupted,
/// is a later turn: a cascade reaches the child.
#[test]
fn a_later_admission_lifts_a_stopped_child() {
    let (_root, mut tree, running) = one_running();
    tree.interrupt_running_descendants_for(InterruptCause::Stopped)
        .unwrap();
    tree.set_active_turn(&running, true).unwrap();
    let attempt = tree.node(&running).unwrap().attempt_id.clone();
    let turn = tree.attempt(&running).unwrap().turn_id.clone().unwrap();
    let command = format!("again:{running}");
    tree.record_command(AgentCommand::intent(
        command.clone(),
        &running,
        attempt,
        AgentCommandKind::Start,
        "assignment",
        Vec::new(),
    ))
    .unwrap();
    let accepted = tree.commands.get_mut(&command).unwrap();
    accepted.fact = Some(CommandFact::Accepted);
    accepted.accepted_seq = Some(1);
    accepted.turn_id = Some(turn);
    assert_eq!(
        tree.interrupt_running_descendants_for(InterruptCause::Cascade)
            .unwrap(),
        vec![running.clone()]
    );
}

/// A turn admitted after the handoff and lost again is an owner loss: the
/// child is held for consent, so a handoff continues by itself once.
#[test]
fn a_turn_lost_after_its_handoff_continuation_is_held_as_an_owner_loss() {
    let (_root, mut tree, [_, parked, _]) = working_children();
    tree.record_handoff().unwrap();
    let command = tree.record_loss_continuation(&parked, 1).unwrap();
    let attempt = tree.node(&parked).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        command.clone(),
        &parked,
        attempt,
        AgentCommandKind::Resume,
        "continue",
        Vec::new(),
    ))
    .unwrap();
    let accepted = tree.commands.get_mut(&command).unwrap();
    accepted.fact = Some(CommandFact::Accepted);
    accepted.accepted_seq = Some(1_000);
    accepted.turn_id = Some("00000000-0000-0000-0000-000000000009".to_owned());
    tree.lose_owner_in_fold(&parked);
    let continuation = tree.loss_continuation(&parked).unwrap();
    assert_eq!(continuation.cause, ContinuationCause::OwnerLost);
    assert_eq!(continuation.state, LossContinuationState::Held);
}

/// Each interrupt records why: a parent's `interrupt_agent` records
/// `parent`, the root's Ctrl-C `cascade`, and a run's exit `stopped`.
#[test]
fn interrupts_record_their_cause() {
    for (cause, expected) in [
        (None, InterruptCauseView::Parent),
        (Some(InterruptCause::Cascade), InterruptCauseView::Cascade),
        (Some(InterruptCause::Stopped), InterruptCauseView::Stopped),
    ] {
        let (_root, mut tree, [running, ..]) = working_children();
        match cause {
            None => {
                tree.interrupt_agent("root", &close_req(&running), None)
                    .unwrap();
            }
            Some(InterruptCause::Cascade) => {
                tree.interrupt_running_descendants().unwrap();
            }
            Some(cause) => {
                tree.interrupt_running_descendants_for(cause).unwrap();
            }
        }
        assert_eq!(view(&tree, &running).interrupt_cause, Some(expected));
        assert_eq!(tree.loss_continuation(&running), None);
    }
}

/// A restored copy of the turn the attempt binds is the turn its latest
/// interrupt already covers: it is not marked again, so a handed-off child
/// keeps `handoff` and continues by itself once.
#[test]
fn a_restored_bound_turn_keeps_its_handoff() {
    let (_root, mut tree, [running, ..]) = working_children();
    tree.record_handoff().unwrap();
    tree.mark_restored_turn_lost(&running, "00000000-0000-0000-0000-000000000001");
    assert_eq!(
        view(&tree, &running).interrupt_cause,
        Some(InterruptCauseView::Handoff)
    );
    let continuation = tree.loss_continuation(&running).unwrap();
    assert_eq!(continuation.cause, ContinuationCause::Handoff);
    assert_eq!(continuation.state, LossContinuationState::Held);
}

/// A turn the lost owner admitted on an interrupted child before binding it
/// (a wake, mail Start or continuation), restored after a take, is lost
/// again: re-marked `owner_lost`, so the child's continuation is decided on
/// that turn under the policy, never by the handoff it no longer covers.
#[test]
fn a_restored_unbound_turn_is_marked_owner_lost_again() {
    let (_root, mut tree, [_, parked, _]) = working_children();
    tree.record_handoff().unwrap();
    assert_eq!(
        tree.loss_continuation(&parked).unwrap().cause,
        ContinuationCause::Handoff
    );
    tree.mark_restored_turn_lost(&parked, "00000000-0000-0000-0000-0000000000aa");
    assert_eq!(
        view(&tree, &parked).interrupt_cause,
        Some(InterruptCauseView::OwnerLost)
    );
    assert_eq!(
        tree.loss_continuation(&parked).unwrap().cause,
        ContinuationCause::OwnerLost
    );
}

/// A reopen closing a turn a planned handoff let go of tells the runtime
/// so: the turn is handed off until a turn is admitted after the handoff,
/// which a lost process then loses.
#[test]
fn a_turn_is_handed_off_until_a_later_one_is_admitted() {
    let (_root, mut tree, [running, parked, held]) = working_children();
    assert!(!tree.turn_handed_off(&running));
    tree.record_handoff().unwrap();
    assert!(tree.turn_handed_off(&running));
    assert!(tree.turn_handed_off(&parked));
    assert!(
        !tree.turn_handed_off(&held),
        "a held loss is not handed off"
    );
    let command = tree.record_loss_continuation(&parked, 1).unwrap();
    let attempt = tree.node(&parked).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        command.clone(),
        &parked,
        attempt,
        AgentCommandKind::Resume,
        "continue",
        Vec::new(),
    ))
    .unwrap();
    let accepted = tree.commands.get_mut(&command).unwrap();
    accepted.fact = Some(CommandFact::Accepted);
    accepted.accepted_seq = Some(1_000);
    accepted.turn_id = Some("00000000-0000-0000-0000-000000000009".to_owned());
    assert!(!tree.turn_handed_off(&parked));
}
