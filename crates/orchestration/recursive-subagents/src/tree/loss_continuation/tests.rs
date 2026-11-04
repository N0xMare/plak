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
        None,
        "an operator interrupt records no cause today"
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
