use super::RECOVERY_ABANDONED;
use crate::protocol::{
    AttemptScopedRequest, AttemptStatus, MessagePurpose, NodeStatus, SendAgentMessageRequest, Tier,
    TopologyErrorCode, WorkspaceStatus,
};
use crate::tree::test_support::*;
use crate::tree::turn_outcome::FailedTurnDisposition;
use crate::tree::*;

fn stuck_child(tree: &mut ProductAgentTree) -> String {
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&id, "locator-1").unwrap();
    tree.set_active_turn(&id, true).unwrap();
    tree.record_turn_failure(
        &id,
        FailedTurnDisposition::RecoveryRequired,
        "child turn outcome is unknown and its session could not be reopened".into(),
    )
    .unwrap();
    tree.settle_for_test();
    id
}

/// A spawn recovery settles never-created ran nothing in its copy: the
/// copy is unminted like a spawn-time refusal's, not sealed into a
/// candidate.
#[test]
fn a_never_created_spawn_unmints_its_copy_instead_of_sealing_it() {
    let (_root, vcs, mut tree) = fake_tree();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let id = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    tree.settle_for_test();
    let workspace_id = tree.node(&id).unwrap().workspace_id.clone();
    tree.record_host_retirement(
        &id,
        "startup Creating: child session could not be created (Revalidation)".into(),
    )
    .and_then(|value| tree.fold_due(&id).map(|_| value))
    .unwrap();
    tree.settle_for_test();
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
    let attempt_id = tree.node(&id).unwrap().attempt_id.clone().unwrap();

    tree.recover_host_decision(
        &id,
        &attempt_id,
        RecoveryResolution::NeverCreated {
            message: "construction refused".into(),
        },
    )
    .unwrap();
    assert_eq!(
        tree.workspaces.get(&workspace_id).unwrap().status,
        WorkspaceStatus::Discarding
    );
    tree.settle_for_test();
    assert!(vcs.calls_of(FakeOp::Seal).is_empty(), "the copy was sealed");
    assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    assert!(!tree.seal_pending(&id));
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
    assert_eq!(
        tree.attempt(&id).unwrap().failure.as_ref().unwrap().kind,
        super::SPAWN_REFUSED
    );
}

#[test]
fn interrupted_tool_note_distinguishes_conversation_from_workspace_changes() {
    let note = AbandonedChildTurn {
        operations: vec![1],
        claim: plakous::agent::AbandonedTurnClaim::RunningTool,
    }
    .note("c1");
    assert!(note.contains("conversation"), "{note}");
    assert!(note.contains("review"), "{note}");
    assert!(!note.contains("nothing from it was kept"), "{note}");
}

#[test]
fn closed_submission_stays_closed_after_late_retirement_recovery() {
    let (_tmp, mut tree) = tree();
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.set_active_turn(&id, true).unwrap();
    let token = tree.attempt(&id).unwrap().turn_token;
    tree.submit_result(
        &id,
        &SubmitResultRequest {
            turn_token: token,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.close_agent("root", &close_req(&id), None).unwrap();
    tree.current_attempt_mut(&id).unwrap().ended_at = Some(1);
    let ended_at = tree.attempt(&id).unwrap().ended_at;
    tree.record_host_retirement(&id, "release deadline".into())
        .and_then(|value| tree.fold_due(&id).map(|_| value))
        .unwrap();
    let mut tree = ProductAgentTree::reopen(tree.to_persisted());
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
    assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Closed);
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_completed: Some(false),
                include_self: None,
                include_read: None,
            },
        )
        .unwrap();
    assert!(listed.agents.iter().any(|agent| agent["agent_id"] == id));
    assert_eq!(tree.pending_result_mail("root"), [] as [MailRecord; 0]);
    let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
    tree.recover_host_decision(
        &id,
        &attempt,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "quiescent".into(),
        },
    )
    .unwrap();
    tree.settle_for_test();
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Closed);
    assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Closed);
    assert_eq!(tree.attempt(&id).unwrap().ended_at, ended_at);
    assert_eq!(
        tree.attempt(&id).unwrap().output_json.as_deref(),
        Some(r#"{"ok":true}"#)
    );
    assert_eq!(tree.pending_result_mail("root"), [] as [MailRecord; 0]);
}

#[test]
fn delete_abandonment_survives_reopen_and_cannot_be_recovered_or_overwritten() {
    let (_tmp, mut tree) = tree();
    let child = stuck_child(&mut tree);
    let attempt_id = tree.attempt(&child).unwrap().attempt_id.clone();
    assert!(tree.take_effects().is_empty());
    assert!(tree.abandon_recovery_for_delete(&child).unwrap());
    assert!(!tree.abandon_recovery_for_delete(&child).unwrap());
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
    assert_eq!(
        tree.attempt(&child).unwrap().failure.as_ref().unwrap().kind,
        RECOVERY_ABANDONED
    );
    assert!(
        tree.take_effects().is_empty(),
        "abandon never runs a seal or VCS operation"
    );
    tree.record_turn_failure(
        &child,
        FailedTurnDisposition::RecoveryRequired,
        "late waiter".into(),
    )
    .unwrap();
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
    let store = MemoryTreeStore::default();
    tree.acknowledge_revision(store.save_in_custody(&tree).unwrap());
    let mut reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    assert_eq!(
        reopened.recovery_need(&child).unwrap(),
        RecoveryNeed::NotRequired
    );
    assert_eq!(reopened.session_recovery_targets(), [] as [String; 0]);
    let recovered = reopened
        .recover_host_decision(
            &child,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "must not replace abandonment".into(),
            },
        )
        .unwrap();
    assert!(!recovered.recovered);
    assert_eq!(recovered.failure_kind.as_deref(), Some(RECOVERY_ABANDONED));
    assert!(reopened.take_effects().is_empty());
    assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
}

#[test]
fn submitted_result_survives_quiescent_retirement_recovery() {
    let (_tmp, mut tree) = tree();
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let token = tree.attempt(&id).unwrap().turn_token;
    tree.submit_result(
        &id,
        &SubmitResultRequest {
            turn_token: token,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_host_retirement(&id, "release deadline".into())
        .and_then(|value| tree.fold_due(&id).map(|_| value))
        .unwrap();
    let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
    tree.recover_host_decision(
        &id,
        &attempt,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "quiescent after late release".into(),
        },
    )
    .unwrap();
    tree.settle_for_test();
    assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Completed);
    assert_eq!(
        tree.attempt(&id).unwrap().output_json.as_deref(),
        Some(r#"{"ok":true}"#)
    );
    assert!(tree.attempt(&id).unwrap().failure.is_none());
}

#[test]
fn scoped_retirement_fault_skips_settled_and_replaced_attempts() {
    let (_tmp, mut tree) = tree();
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let expected = tree.attempt(&id).unwrap().attempt_id.clone();
    tree.submit_result(
        &id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    assert!(
        tree.record_host_retirement_for_attempt(&id, Some(&expected), "owner deadline".into())
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .unwrap()
    );
    tree.recover_host_decision(
        &id,
        &expected,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "quiescent".into(),
        },
    )
    .unwrap();
    tree.settle_for_test();
    let settled = (
        tree.revision,
        tree.node(&id).unwrap().clone(),
        tree.attempt(&id).unwrap().clone(),
    );
    assert!(
        !tree
            .record_host_retirement_for_attempt(&id, Some(&expected), "late owner failure".into())
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .unwrap()
    );
    assert_eq!(
        (
            tree.revision,
            tree.node(&id).unwrap().clone(),
            tree.attempt(&id).unwrap().clone()
        ),
        settled
    );
    assert!(tree.take_effects().is_empty());
    let node = tree.node(&id).unwrap();
    let schema = node.output_schema_json.clone();
    let digest = node.schema_digest.clone();
    tree.open_followup_attempt(&id, "m-followup", schema, digest)
        .unwrap();
    let successor = tree.attempt(&id).unwrap().clone();
    let revision = tree.revision;
    assert!(
        !tree
            .record_host_retirement_for_attempt(&id, Some(&expected), "old owner failure".into())
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .unwrap()
    );
    assert_eq!(tree.attempt(&id).unwrap(), &successor);
    assert_eq!(tree.revision, revision);
}

#[test]
fn closed_turn_failures_keep_recovery_visible_without_result_mail() {
    let (_tmp, mut tree) = tree();
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.set_active_turn(&id, true).unwrap();
    tree.close_agent("root", &close_req(&id), None).unwrap();
    let expected = tree.attempt(&id).unwrap().attempt_id.clone();
    let ended_at = tree.attempt(&id).unwrap().ended_at;
    for message in ["unknown terminal", "late terminal waiter"] {
        tree.record_turn_failure(&id, FailedTurnDisposition::RecoveryRequired, message.into())
            .unwrap();
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Closed);
        assert_eq!(tree.pending_result_mail("root"), [] as [MailRecord; 0]);
    }
    tree.recover_host_decision(
        &id,
        &expected,
        RecoveryResolution::TurnAbandoned {
            message: "recovered".into(),
        },
    )
    .unwrap();
    tree.settle_for_test();
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Closed);
    assert_eq!(tree.attempt(&id).unwrap().ended_at, ended_at);
    assert_eq!(tree.pending_result_mail("root"), [] as [MailRecord; 0]);
}

#[test]
fn session_bound_after_failure_clears_only_its_terminal_timestamp() {
    let (_tmp, mut tree) = tree();
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.record_host_materialization(&id).unwrap();
    tree.fold_due(&id).unwrap();
    assert!(tree.attempt(&id).unwrap().ended_at.is_some());
    tree.bind_session_locator(&id, "bound-child-session")
        .unwrap();
    tree.record_session_bound_after_failure(&id).unwrap();
    tree.fold_due(&id).unwrap();
    assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Running);
    assert_eq!(tree.attempt(&id).unwrap().ended_at, None);
    tree.record_turn_failure(
        &id,
        FailedTurnDisposition::Failed { kind: "terminal" },
        "new terminal".into(),
    )
    .unwrap();
    assert!(tree.attempt(&id).unwrap().ended_at.unwrap() > 1);
}

#[test]
fn submitted_failed_terminal_is_not_completed_by_retirement_recovery() {
    for terminal_first in [true, false] {
        let (_tmp, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        let token = tree.attempt(&id).unwrap().turn_token;
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: token,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        if !terminal_first {
            tree.record_host_retirement(&id, "release deadline".into())
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .unwrap();
        }
        tree.record_turn_failure(
            &id,
            FailedTurnDisposition::Failed {
                kind: "committed_failure",
            },
            "authoritative terminal failure".into(),
        )
        .unwrap();
        if terminal_first {
            tree.record_host_retirement(&id, "release deadline".into())
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .unwrap();
        }
        let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
        tree.recover_host_decision(
            &id,
            &attempt,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiescent".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Failed);
    }
}

#[test]
fn resolving_recovery_does_not_repush_a_pruned_result() {
    let (_tmp, mut tree) = tree();
    let child = stuck_child(&mut tree);
    let attempt_id = tree.attempt(&child).unwrap().attempt_id.clone();
    let result = tree.pending_result_mail("root")[0].message_id.clone();
    tree.acknowledge_mail(&result).unwrap();
    let sender = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    for _ in 0..70 {
        let sent = tree
            .send_agent_message(
                &sender.agent_id,
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: "root".into(),
                    message: "later".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        tree.acknowledge_mail(&sent.message_id).unwrap();
    }
    assert_eq!(tree.inbox_for("root", true).len(), 64);
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    reopened
        .recover_host_decision(
            &child,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiet".into(),
            },
        )
        .unwrap();
    reopened.settle_for_test();
    assert_eq!(reopened.pending_result_mail("root"), [] as [MailRecord; 0]);
}

fn scoped(agent_id: &str) -> AttemptScopedRequest {
    AttemptScopedRequest {
        agent_id: agent_id.to_owned(),
        expected_attempt_id: None,
    }
}

fn followup(agent_id: &str) -> SendAgentMessageRequest {
    SendAgentMessageRequest {
        refresh: None,
        agent_id: agent_id.to_owned(),
        message: "again".into(),
        purpose: Some(MessagePurpose::Followup),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    }
}

/// A turn ended recovery-required and the one reopen that could have
/// abandoned it failed. Close and followup refuse it until a recovery settles it; then
/// both work, the attempt is refunded, and closing frees the node slot.
#[test]
fn a_settled_recovery_unsticks_close_and_followup() {
    let (_tmp, mut tree) = tree();
    let id = stuck_child(&mut tree);
    let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
    assert!(matches!(
        tree.recovery_need(&id).unwrap(),
        RecoveryNeed::Session {
            locator: Some(_),
            ..
        }
    ));
    assert_eq!(tree.session_recovery_targets(), vec![id.clone()]);
    let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
    assert_eq!(closed, vec![(id.clone(), NodeStatus::RecoveryRequired)]);
    let refused = tree
        .send_agent_message("root", &followup(&id), None)
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::FollowupRecoveryRequired);

    let receipt = tree
        .recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::TurnAbandoned {
                message: "abandoned".into(),
            },
        )
        .unwrap();
    tree.settle_for_test();
    assert!(receipt.recovered);
    assert_eq!(receipt.status, NodeStatus::Failed);
    assert_eq!(receipt.failure_kind.as_deref(), Some(TURN_ABANDONED));
    assert!(tree.attempt(&id).unwrap().turn_was_abandoned());
    assert_eq!(tree.recovery_need(&id).unwrap(), RecoveryNeed::NotRequired);
    assert_eq!(tree.session_recovery_targets(), [] as [String; 0]);

    let again = tree
        .recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "second".into(),
            },
        )
        .unwrap();
    assert!(!again.recovered, "a repeat changes nothing");
    assert_eq!(again.failure_kind.as_deref(), Some(TURN_ABANDONED));

    tree.send_agent_message("root", &followup(&id), None)
        .unwrap();
    assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Running);
    tree.record_turn_completed(&id).unwrap();
    tree.settle_for_test();
    let before = tree
        .nodes()
        .values()
        .filter(|node| node.status != NodeStatus::Closed)
        .count();
    let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
    assert_eq!(closed, vec![(id.clone(), NodeStatus::Closed)]);
    let after = tree
        .nodes()
        .values()
        .filter(|node| node.status != NodeStatus::Closed)
        .count();
    assert_eq!(after + 1, before);
}

/// A quiescent head settles as its own kind, never as completion, and a
/// resolution for an attempt that is no longer current is refused.
#[test]
fn a_quiescent_recovery_is_its_own_kind_and_is_fenced_to_the_attempt() {
    let (_tmp, mut tree) = tree();
    let id = stuck_child(&mut tree);
    let stale = AttemptId::new(&id, 9);
    let error = tree
        .recover_host_decision(
            &id,
            &stale,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiet".into(),
            },
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AttemptMismatch);
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
    let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
    let receipt = tree
        .recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiet".into(),
            },
        )
        .unwrap();
    assert_eq!(
        receipt.failure_kind.as_deref(),
        Some(RECOVERED_WITHOUT_TURN)
    );
    assert_eq!(receipt.attempt, Some(AttemptStatus::Failed));
    assert_ne!(receipt.attempt, Some(AttemptStatus::Completed));
    tree.send_agent_message("root", &followup(&id), None)
        .unwrap();
    let error = tree
        .recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "late".into(),
            },
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AttemptMismatch);
    let error = tree
        .recover_receipt(&id, Some(&attempt_id), true)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AttemptMismatch);
}

/// An interrupted adopt behind a session failure: the session part is
/// settled first without touching the copy or its adopt, the node stays
/// `recovery_required` for the VCS reason alone, and `close_agent` ends it
/// and frees its node slot.
#[test]
fn an_interrupted_adopt_settles_its_session_then_closes_to_a_free_slot() {
    let (_tmp, mut tree) = tree();
    let id = stuck_child(&mut tree);
    tree.grants.get_mut("root").unwrap().max_nodes = 2;
    let workspace_id = tree.node(&id).unwrap().workspace_id.clone();
    tree.workspaces.get_mut(&workspace_id).unwrap().status = WorkspaceStatus::Adopting;
    let mut tree = ProductAgentTree::reopen(tree.to_persisted());
    assert!(matches!(
        tree.recovery_need(&id).unwrap(),
        RecoveryNeed::Session { .. }
    ));
    assert_eq!(tree.session_recovery_targets(), vec![id.clone()]);
    let full = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(
        full.code,
        TopologyErrorCode::SpawnBudget,
        "it holds its slot"
    );

    let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
    let receipt = tree
        .recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::TurnAbandoned {
                message: "abandoned".into(),
            },
        )
        .unwrap();
    assert!(receipt.recovered);
    assert_eq!(receipt.status, NodeStatus::RecoveryRequired);
    assert_eq!(receipt.failure_kind.as_deref(), Some(TURN_ABANDONED));
    assert!(
        receipt
            .workspace_recovery
            .as_deref()
            .is_some_and(|reason| reason.contains("close_agent"))
    );
    assert_eq!(
        tree.workspaces.get(&workspace_id).unwrap().status,
        WorkspaceStatus::Adopting,
        "settling the session leaves the adopt alone"
    );
    assert!(matches!(
        tree.recovery_need(&id).unwrap(),
        RecoveryNeed::Workspace { .. }
    ));
    assert_eq!(tree.session_recovery_targets(), [] as [String; 0]);
    let again = tree
        .recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiet".into(),
            },
        )
        .unwrap_err();
    assert_eq!(again.code, TopologyErrorCode::RecoveryVcsAmbiguous);

    let live = |tree: &ProductAgentTree| {
        tree.nodes()
            .values()
            .filter(|node| node.status != NodeStatus::Closed)
            .count()
    };
    let before = live(&tree);
    let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
    assert_eq!(closed, vec![(id.clone(), NodeStatus::Closed)]);
    tree.settle_for_test();
    assert_eq!(live(&tree) + 1, before, "close freed the node slot");
}

/// A settled abandonment is final for its attempt: a failure report that
/// arrives after it (a turn waiter finishing late) changes nothing.
#[test]
fn the_first_settlement_of_an_attempt_wins() {
    let (_tmp, mut tree) = tree();
    let id = stuck_child(&mut tree);
    let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
    tree.recover_host_decision(
        &id,
        &attempt_id,
        RecoveryResolution::TurnAbandoned {
            message: "abandoned".into(),
        },
    )
    .unwrap();
    tree.settle_for_test();
    tree.record_turn_failure(
        &id,
        FailedTurnDisposition::RecoveryRequired,
        "late waiter".into(),
    )
    .unwrap();
    let attempt = tree.attempt(&id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(
        attempt
            .failure
            .as_ref()
            .map(|failure| failure.kind.as_str()),
        Some(TURN_ABANDONED)
    );
    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
}

/// The operator may retry any child's recovery; the root is not a child.
#[test]
fn operator_recover_authority_covers_every_child_but_the_root() {
    let (_tmp, mut tree) = tree();
    let id = stuck_child(&mut tree);
    tree.authorize_operator_recover(&scoped(&id)).unwrap();
    let error = tree
        .authorize_operator_recover(&scoped("root"))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::GrantDenied);
}

/// A stuck node keeps its tree-wide node slot until it is resolved and
/// closed.
#[test]
fn a_recovery_required_node_holds_its_tree_node_slot_until_closed() {
    let (_tmp, mut tree) = tree();
    let stuck = stuck_child(&mut tree);
    tree.grants.get_mut("root").unwrap().max_nodes = 2;
    let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
    let attempt_id = tree.attempt(&stuck).unwrap().attempt_id.clone();
    tree.recover_host_decision(
        &stuck,
        &attempt_id,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "quiet".into(),
        },
    )
    .unwrap();
    tree.settle_for_test();
    let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
    tree.close_agent("root", &scoped(&stuck), None).unwrap();
    tree.settle_for_test();
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
}

/// A failed copy removal is a VCS question, not a session one: no
/// session reopen is attempted for it, and `close_agent` is its remedy.
#[test]
fn a_failed_copy_removal_is_not_a_session_recovery() {
    let (_root, mut tree) = tree();
    let id = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&id, "locator-1").unwrap();
    if let Some(node) = tree.nodes.get_mut(&id) {
        node.status = NodeStatus::RecoveryRequired;
    }
    let attempt = tree.current_attempt_mut(&id).unwrap();
    attempt.status = AttemptStatus::RecoveryRequired;
    attempt.failure = Some(AttemptFailure {
        kind: WORKSPACE_CLEANUP_KIND.into(),
        message: "the copy could not be removed".into(),
    });
    assert!(matches!(
        tree.recovery_need(&id).unwrap(),
        RecoveryNeed::Workspace { .. }
    ));
    assert_eq!(tree.session_recovery_targets(), [] as [String; 0]);
}

/// A terminal recovery refusal recorded for the current attempt lets
/// `close_agent` settle and end the node; one for a stale attempt is
/// ignored.
#[test]
fn close_agent_settles_a_terminal_recovery_refusal() {
    let (_root, mut tree) = tree();
    let id = stuck_child(&mut tree);
    let stale = AttemptId::new(&id, 99);
    tree.record_terminal_recovery_refusal(&id, &stale, "recovery_session_unavailable", "x");
    assert!(tree.terminal_recovery_refusal(&id).is_none());
    let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
    tree.record_terminal_recovery_refusal(
        &id,
        &attempt_id,
        "recovery_session_unavailable",
        "the child session was created by an incompatible build",
    );
    assert_eq!(tree.session_recovery_targets(), [] as [String; 0]);
    let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
    assert_eq!(closed, vec![(id.clone(), NodeStatus::Closed)]);
    let attempt = tree.attempt(&id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(
        attempt.failure.as_ref().unwrap().kind,
        "recovery_session_unavailable"
    );
}
