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
