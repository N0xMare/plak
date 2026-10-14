//! The tree projector's laws (spec 05) and the derivations it owns (spec 03
//! §4.4, §8, §14.1): wait settlement, the withheld-mail return, a discarded
//! host wake's batch, and child command binding.

use plakous::agent::{
    AbandonedTurnClaim, AbandonmentCause, AgentErrorKind, CommandId, ControlErrorKind, TurnUsage,
};
use plakous::model::{CallId, EntryId, FinishReason, HistorySegmentKind, TurnId, TurnOrigin};
use plakous::sessions::feed::{
    FeedCancelCause, FeedCommandKind, FeedFact, FeedHeader, FeedRecord, FeedTurnOutcome,
};
use proptest::prelude::*;

use super::{CHILD_SESSION_KIND, ChildTerminal};
use crate::tree::test_support::*;
use crate::tree::turn_outcome::FailedTurnDisposition;
use crate::tree::*;

const ROOT_SESSION: &str = "00000000-0000-4000-8000-0000000000a1";
const CHILD_SESSION: &str = "00000000-0000-4000-8000-0000000000c1";

#[test]
fn compaction_projection_replays_per_attempt_without_persisted_fields() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    for outcome in [
        serde_json::json!({"compacted": {
            "source": {"session_id": CHILD_SESSION, "branch_id": uuid(0xb), "checkpoint_id": uuid(0x3001)},
            "target": {"session_id": CHILD_SESSION, "branch_id": uuid(0xb), "checkpoint_id": uuid(0x3002)},
            "pass": "summary", "selected_segments": 1, "tokens_before": 900, "tokens_after": 300
        }}),
        serde_json::json!({"failed": {"summary_sent": true, "kind": "model", "model_kind": "transport"}}),
        serde_json::json!("no_eligible_range"),
    ] {
        feed.push(vec![
            serde_json::from_value(serde_json::json!({"workflow_settled": {
                "workflow_id": "07".repeat(32), "operation_id": null,
                "kind": {"compact": {"reason": "turn_fence"}}, "outcome": outcome
            }}))
            .unwrap(),
        ]);
    }
    project(&mut tree, &child, &feed);
    let stats = tree.attempt_compaction(&attempt).unwrap();
    assert_eq!(
        (stats.committed, stats.failed, stats.no_eligible),
        (1, 1, 1)
    );
    assert_eq!(
        (stats.tokens_before, stats.tokens_after),
        (Some(900), Some(300))
    );
    assert!(stats.complete);
    project(&mut tree, &child, &feed);
    assert_eq!(tree.attempt_compaction(&attempt), Some(stats));
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(reopened.attempt_compaction(&attempt).is_none());
    assert_eq!(reopened.feed_read_cursor(CHILD_SESSION), 0);
    reopened.seed_feed_cursor(ROOT_SESSION, 99);
    assert_eq!(
        reopened.feed_read_cursor(ROOT_SESSION),
        99,
        "root feeds need no compaction-stat replay"
    );
    project(&mut reopened, &child, &feed);
    assert_eq!(reopened.attempt_compaction(&attempt), Some(stats));
    assert_eq!(
        view::project_tree_view(&reopened)
            .nodes
            .iter()
            .find(|node| node.agent_id == child)
            .unwrap()
            .usage
            .unwrap()
            .compaction,
        Some(stats)
    );
    let row = reopened
        .full_list_json(
            reopened.node(&child).unwrap(),
            projection::NodeOccupancy::default(),
        )
        .unwrap();
    assert_eq!(row["compaction"]["committed"], 1);
    let second = tree
        .open_followup_attempt(
            &child,
            "followup",
            tree.node(&child).unwrap().output_schema_json.clone(),
            tree.node(&child).unwrap().schema_digest.clone(),
        )
        .unwrap();
    child_command(&mut tree, &child, &second, "start-2");
    feed.accepted("start-2", turn(2));
    feed.push(vec![
        serde_json::from_value(serde_json::json!({"workflow_settled": {
            "workflow_id": "08".repeat(32), "operation_id": null,
            "kind": {"compact": {"reason": "turn_fence"}}, "outcome": "no_eligible_range"
        }}))
        .unwrap(),
    ]);
    project(&mut tree, &child, &feed);
    assert_eq!(tree.attempt_compaction(&attempt), Some(stats));
    let next = tree.attempt_compaction(&second).unwrap();
    assert_eq!((next.committed, next.failed, next.no_eligible), (0, 0, 1));
    assert_eq!((next.tokens_before, next.tokens_after), (None, None));
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    project(&mut reopened, &child, &feed);
    assert_eq!(reopened.attempt_compaction(&attempt), Some(stats));
    assert_eq!(reopened.attempt_compaction(&second), Some(next));
    reopened.compaction_views.clear();
    reopened.compaction_cursors.clear();
    reopened
        .project_feed(
            &child,
            CHILD_SESSION,
            &feed.records[1..],
            &TreeFeedReads::default(),
        )
        .unwrap();
    assert!(
        reopened.attempt_compaction(&attempt).is_none(),
        "a retained workflow without its attempt binding is not misattributed"
    );
    assert!(!reopened.attempt_compaction(&second).unwrap().complete);
    assert_eq!(
        reopened.feed_read_cursor(CHILD_SESSION),
        reopened.feed_cursor(CHILD_SESSION)
    );
}

#[cfg(test)]
mod r4b;
#[cfg(test)]
mod r5a;

#[test]
fn retained_head_binds_the_running_followup_and_preserves_an_older_unwinding_close() {
    for case in 0..4 {
        let (_tmp, mut live) = tree();
        let (child, first) = running_child(&mut live);
        let mut feed = Feed::new(CHILD_SESSION);
        child_command(&mut live, &child, &first, "first-start");
        feed.accepted("first-start", turn(1));
        feed.terminal("first-start", turn(1), End::Completed);
        project(&mut live, &child, &feed);
        generated_finalization(
            &mut live,
            &child,
            turn(1),
            ChildFinalizationDecision::Finalize,
            &feed,
        );
        let second = live
            .open_followup_attempt(
                &child,
                "followup",
                live.node(&child).unwrap().output_schema_json.clone(),
                live.node(&child).unwrap().schema_digest.clone(),
            )
            .unwrap();
        child_command(&mut live, &child, &second, "second-start");
        feed.accepted("second-start", turn(2));
        project(&mut live, &child, &feed);
        if case == 1 {
            live.close_agent("root", &close_req(&child), None).unwrap();
        }
        if case == 3 {
            live.interrupt_agent("root", &close_req(&child), None)
                .unwrap();
        }
        feed.history(turn(2), Vec::new());
        project(&mut live, &child, &feed);
        if case == 2 {
            live.close_agent("root", &close_req(&child), None).unwrap();
        }
        let expected_node = live.node(&child).unwrap().clone();
        let mut expected_attempt = live.attempt(&child).unwrap().clone();
        expected_attempt.usage.turns = 0;
        let head = expected_node
            .session_locator
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        let retained = TreeFeed {
            session_id: CHILD_SESSION.to_owned(),
            retained_from: 5,
            below_retained: Some(4),
            heads: vec![head],
            head_turns: [(head, Some(turn(2)))].into(),
            records: Vec::new(),
            reads: TreeFeedReads::default(),
        };
        let mut rebuilt = live.clone();
        rebuilt.clear_feed_views();
        retained.apply(&mut rebuilt, &child).unwrap();
        assert_eq!(rebuilt.node(&child).unwrap(), &expected_node, "case={case}");
        assert_eq!(
            rebuilt.attempt(&child).unwrap(),
            &expected_attempt,
            "case={case}"
        );
        assert_ne!(
            rebuilt.attempts[&first].turn_id.as_deref(),
            Some(turn(2).to_string().as_str())
        );
        assert!(rebuilt.take_effects().is_empty());
        retained.apply(&mut rebuilt, &child).unwrap();
        assert_eq!(rebuilt.node(&child).unwrap(), &expected_node);
    }
}

#[test]
fn clearing_one_agent_preserves_other_folds_and_uses_the_shared_reset() {
    let (_tmp, mut live) = tree();
    let (child, attempt) = running_child(&mut live);
    let other = live.spawn_agent("root", &spawn_req(), None).unwrap();
    child_command(&mut live, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut live, &child, &feed);
    let expected = rows(&live);
    let other_node = live.node(&other.agent_id).unwrap().clone();
    let other_attempt = live.attempt(&other.agent_id).unwrap().clone();
    live.clear_agent_feed_views(&child, CHILD_SESSION);
    assert!(live.node(&child).unwrap().fold_order.is_none());
    assert_eq!(live.feed_cursor(CHILD_SESSION), 0);
    assert_eq!(live.node(&other.agent_id).unwrap(), &other_node);
    assert_eq!(live.attempt(&other.agent_id).unwrap(), &other_attempt);
    assert_eq!(live.node("root").unwrap().fold_order, Some(0));
    project(&mut live, &child, &feed);
    assert_eq!(rows(&live), expected);
    assert!(live.take_effects().is_empty());
}

#[test]
fn deleted_attempt_view_restarts_at_the_retry_chain_root_and_opens_in_order() {
    let (_tmp, mut tree) = tree();
    let (child, first) = running_child(&mut tree);
    let mut feed = Feed::new(CHILD_SESSION);
    child_command(&mut tree, &child, &first, "first-start");
    feed.accepted("first-start", turn(1));
    project(&mut tree, &child, &feed);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let first_prefix = tree.node(&child).unwrap().clone();
    feed.terminal("first-start", turn(1), End::Completed);
    project(&mut tree, &child, &feed);
    let second = tree
        .open_followup_attempt(
            &child,
            "second",
            r#"{"type":"object","required":["second"]}"#.to_owned(),
            digest_text(r#"{"type":"object","required":["second"]}"#),
        )
        .unwrap();
    let second_prefix = tree.node(&child).unwrap().clone();
    child_command(&mut tree, &child, &second, "second-start");
    feed.accepted("second-start", turn(2));
    project(&mut tree, &child, &feed);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let second_stopped_prefix = tree.node(&child).unwrap().clone();
    feed.terminal("second-start", turn(2), End::Completed);
    project(&mut tree, &child, &feed);
    let third = tree
        .open_followup_attempt(
            &child,
            "third",
            r#"{"type":"object","required":["third"]}"#.to_owned(),
            digest_text(r#"{"type":"object","required":["third"]}"#),
        )
        .unwrap();
    let third_prefix = tree.node(&child).unwrap().clone();
    let expected = rows(&tree);
    tree.clear_feed_views();
    assert_eq!(tree.node(&child).unwrap().attempt_id.as_ref(), Some(&first));
    assert_eq!(
        tree.node(&child).unwrap().output_schema_json,
        tree.attempts[&first].output_schema_json
    );
    let prefixes = [
        first_prefix,
        second_prefix,
        second_stopped_prefix,
        third_prefix,
    ];
    for end in 1..=feed.records.len() {
        tree.project_feed(
            &child,
            CHILD_SESSION,
            &feed.records[..end],
            &TreeFeedReads::default(),
        )
        .unwrap();
        let expected_attempt = match end {
            1 => &first,
            2 | 3 => &second,
            _ => &third,
        };
        assert_eq!(
            tree.node(&child).unwrap().attempt_id.as_ref(),
            Some(expected_attempt)
        );
        assert_eq!(tree.node(&child).unwrap(), &prefixes[end - 1]);
    }
    tree.project_feed("root", ROOT_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rows(&tree), expected);
    assert!(tree.take_effects().is_empty());
}

#[test]
fn followup_attempt_view_is_agent_local_and_independent_of_parent_projection_order() {
    const PARENT_SESSION: &str = "00000000-0000-4000-8000-0000000000d1";
    let (_tmp, mut live) = tree();
    live.grants.get_mut("root").unwrap().max_depth = 2;
    pin(
        &mut live,
        "orchestrate",
        "test-account",
        "selected-tuple",
        None,
    );
    let parent = live
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::ReadOnly, "parent", "delegate"),
            None,
        )
        .unwrap();
    live.allocate_session_for_test(
        &parent.agent_id,
        format!("{PARENT_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let child = live
        .spawn_agent(&parent.agent_id, &spawn_req(), None)
        .unwrap();
    live.allocate_session_for_test(
        &child.agent_id,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let mut parent_feed = Feed::new(PARENT_SESSION);
    let mut child_feed = Feed::new(CHILD_SESSION);
    child_command(
        &mut live,
        &parent.agent_id,
        &parent.attempt_id,
        "parent-start",
    );
    child_command(&mut live, &child.agent_id, &child.attempt_id, "child-start");
    parent_feed.accepted("parent-start", turn(1));
    child_feed.accepted("child-start", turn(2));
    project(&mut live, &parent.agent_id, &parent_feed);
    project(&mut live, &child.agent_id, &child_feed);
    child_feed.terminal("child-start", turn(2), End::Completed);
    project(&mut live, &child.agent_id, &child_feed);
    generated_finalization(
        &mut live,
        &child.agent_id,
        turn(2),
        ChildFinalizationDecision::Finalize,
        &child_feed,
    );
    let other_nodes = live.nodes.clone();
    let other_attempt = live.attempts[&parent.attempt_id].clone();
    let second = live
        .open_followup_attempt(&child.agent_id, "followup", String::new(), String::new())
        .unwrap();
    for (id, node) in other_nodes {
        if id != child.agent_id {
            assert_eq!(live.nodes[&id], node);
        }
    }
    assert_eq!(live.attempts[&parent.attempt_id], other_attempt);
    child_command(&mut live, &child.agent_id, &second, "followup-start");
    child_feed.accepted("followup-start", turn(3));
    project(&mut live, &child.agent_id, &child_feed);
    live.interrupt_agent("root", &close_req(&parent.agent_id), None)
        .unwrap();
    let expected = rows(&live);
    for parent_first in [false, true] {
        let mut rebuilt = live.clone();
        rebuilt.clear_feed_views();
        if parent_first {
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
            project(&mut rebuilt, &child.agent_id, &child_feed);
        } else {
            project(&mut rebuilt, &child.agent_id, &child_feed);
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
        }
        assert_eq!(rows(&rebuilt), expected);
        assert!(rebuilt.take_effects().is_empty());
    }
}

#[test]
fn fresh_unfed_descendants_have_started_folds_and_close_live() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(tree.node(&child.agent_id).unwrap().fold_order, Some(0));
    assert_eq!(tree.node("root").unwrap().fold_order, Some(0));
    assert_eq!(
        tree.close_live_descendants("root").unwrap(),
        vec![child.agent_id.clone()]
    );
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
}

#[test]
fn first_cascade_after_deletion_refuses_parent_first_then_matches_postorder() {
    const PARENT_SESSION: &str = "00000000-0000-4000-8000-0000000000c2";
    for terminal_kind in 0..3 {
        let (_tmp, mut base) = tree();
        base.grants.get_mut("root").unwrap().max_depth = 2;
        pin(
            &mut base,
            "orchestrate",
            "test-account",
            "selected-tuple",
            None,
        );
        let parent = base
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("orchestrate", Tier::ReadOnly, "parent", "delegate"),
                None,
            )
            .unwrap();
        let child = base
            .spawn_agent(&parent.agent_id, &spawn_req(), None)
            .unwrap();
        for (id, session) in [
            (&parent.agent_id, PARENT_SESSION),
            (&child.agent_id, CHILD_SESSION),
        ] {
            base.allocate_session_for_test(id, format!("{session}/{}/{}", uuid(0xb), uuid(0x3001)))
                .unwrap();
        }
        child_command(
            &mut base,
            &parent.agent_id,
            &parent.attempt_id,
            "parent-start",
        );
        child_command(&mut base, &child.agent_id, &child.attempt_id, "child-start");
        let mut parent_feed = Feed::new(PARENT_SESSION);
        let mut child_feed = Feed::new(CHILD_SESSION);
        parent_feed.accepted("parent-start", turn(1));
        child_feed.accepted("child-start", turn(2));
        project(&mut base, &parent.agent_id, &parent_feed);
        project(&mut base, &child.agent_id, &child_feed);
        child_feed.terminal("child-start", turn(2), End::Completed);
        project(&mut base, &child.agent_id, &child_feed);
        base.record_child_finalization(
            &child.agent_id,
            &turn(2).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            terminal_evidence(&base, &child.agent_id, &child_feed, &turn(2).to_string()),
        )
        .unwrap();
        base.fold_due(&child.agent_id).unwrap();
        match terminal_kind {
            0 => parent_feed.terminal("parent-start", turn(1), End::Internal),
            1 => {
                base.record_host_cancellation(&parent.agent_id).unwrap();
            }
            _ => {
                parent_feed.terminal("parent-start", turn(1), End::Completed);
                base.record_child_finalization(
                    &parent.agent_id,
                    &turn(1).to_string(),
                    ChildFinalizationDecision::Finalize,
                    true,
                    terminal_evidence(&base, &parent.agent_id, &parent_feed, &turn(1).to_string()),
                )
                .unwrap();
            }
        }
        assert!(
            !base
                .operations
                .values()
                .any(|op| op.tool_name == "descendant_close_effect"
                    && op.caller_agent_id == parent.agent_id)
        );
        let mut live = ProductAgentTree::reopen(base.to_persisted());
        project(&mut live, &parent.agent_id, &parent_feed);
        let expected = rows(&live);
        for parent_first in [false, true] {
            let mut rebuilt = base.clone();
            rebuilt.clear_feed_views();
            if parent_first {
                let before = rows(&rebuilt);
                let error = rebuilt
                    .project_feed(
                        &parent.agent_id,
                        PARENT_SESSION,
                        &parent_feed.records,
                        &TreeFeedReads::default(),
                    )
                    .unwrap_err();
                assert!(matches!(
                    error,
                    TreeProjectorError::Tree(TopologyError {
                        code: TopologyErrorCode::OperationRecoveryRequired,
                        ..
                    })
                ));
                assert_eq!(
                    rows(&rebuilt),
                    before,
                    "parent must not advance on retryable refusal"
                );
                assert!(rebuilt.take_effects().is_empty());
            }
            project(&mut rebuilt, &child.agent_id, &child_feed);
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
            rebuilt.fold_due("root").unwrap();
            assert_eq!(
                rows(&rebuilt),
                expected,
                "terminal_kind={terminal_kind}, parent_first={parent_first}"
            );
            assert!(rebuilt.take_effects().is_empty());
        }
    }
}

#[test]
fn followup_opening_rebuilds_supersession_at_its_captured_stamp() {
    let (_tmp, mut tree) = tree();
    let (child, first) = running_child(&mut tree);
    tree.record_operator_intent(
        &child,
        Some(first.clone()),
        operator_intents::Decision::Interrupt { effective: true },
    )
    .unwrap();
    tree.open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    let expected = tree.attempts[&first].clone();
    assert_eq!(expected.status, AttemptStatus::Cancelled);
    tree.clear_feed_views();
    tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempts[&first], expected);
}

#[test]
fn followup_withdrawal_refuses_another_intent_without_partial_write() {
    let (_tmp, mut tree) = tree();
    let (child, first) = running_child(&mut tree);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let opened = tree
        .open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    tree.record_operator_intent(
        &child,
        Some(opened),
        operator_intents::Decision::Interrupt { effective: false },
    )
    .unwrap();
    let before = rows(&tree);
    let operations = tree.operations.clone();
    assert!(
        tree.withdraw_unstarted_followup(&child, "followup", CHILD_SESSION, 0)
            .is_err()
    );
    assert_eq!(rows(&tree), before);
    assert_eq!(tree.operations, operations);
    assert!(tree.attempts.contains_key(&first));
}

#[test]
fn followup_open_save_withdrawal_cut_reopens_and_rebuilds_remaining_inputs() {
    let (_tmp, mut tree) = tree();
    let (child, _first) = running_child(&mut tree);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let expected = tree.attempt(&child).unwrap().clone();
    let opened = tree
        .open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    tree.record_command(AgentCommand::intent(
        "followup-start",
        &child,
        Some(opened.clone()),
        AgentCommandKind::Followup,
        "again",
        vec!["followup".to_owned()],
    ))
    .unwrap();
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(
        reopened
            .withdraw_unstarted_followup(&child, "followup", CHILD_SESSION, 0)
            .is_ok()
    );
    assert_eq!(reopened.attempt(&child).unwrap(), &expected);
    assert!(!reopened.attempts.contains_key(&opened));
    assert!(
        !reopened
            .commands
            .values()
            .any(|command| command.attempt_id.as_ref() == Some(&opened))
    );
    assert!(
        !reopened
            .operations
            .values()
            .any(|operation| operation.canonical_request.contains(opened.as_str()))
    );
    reopened.clear_feed_views();
    reopened
        .project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(reopened.attempt(&child).unwrap(), &expected);
}

#[test]
fn followup_withdrawal_refuses_a_future_tail_and_a_recorded_start_rejection() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let opened = tree
        .open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    tree.record_command(AgentCommand::intent(
        "followup-start",
        &child,
        Some(opened.clone()),
        AgentCommandKind::Followup,
        "again",
        vec!["followup".to_owned()],
    ))
    .unwrap();
    let before = rows(&tree);
    assert!(
        tree.withdraw_unstarted_followup(&child, "followup", CHILD_SESSION, 1)
            .is_err()
    );
    assert_eq!(rows(&tree), before);
    let mut feed = Feed::new(CHILD_SESSION);
    feed.rejected("followup-start");
    project(&mut tree, &child, &feed);
    let before = rows(&tree);
    assert!(
        tree.withdraw_unstarted_followup(&child, "followup", CHILD_SESSION, 1)
            .is_err()
    );
    assert_eq!(rows(&tree), before);
    tree.record_host_turn_failure(
        &child,
        FailedTurnDisposition::Failed {
            kind: "turn_failed",
        },
        "Start was definitively rejected".to_owned(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.attempt(&child).unwrap().attempt_id, opened);
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Failed);
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn followup_withdrawal_refuses_a_reserved_descendant_close_effect() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    tree.open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    tree.record_close_live_descendants(&child, 1000).unwrap();
    let before = rows(&tree);
    assert!(
        tree.withdraw_unstarted_followup(&child, "followup", CHILD_SESSION, 0)
            .is_err()
    );
    assert_eq!(rows(&tree), before);
}

#[test]
fn deferred_close_observes_a_failed_terminal_seal_only_at_that_record() {
    let (_tmp, _vcs, mut live) = fake_tree();
    let spawned = live.spawn_agent("root", &mint_req(), None).unwrap();
    let child = &spawned.agent_id;
    live.allocate_session_for_test(
        child,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    child_command(&mut live, child, &spawned.attempt_id, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut live, child, &feed);
    live.close_agent("root", &close_req(child), None).unwrap();
    assert_eq!(live.node(child).unwrap().status, NodeStatus::Closed);
    assert!(live.take_effects().is_empty());
    let before_node = live.node(child).unwrap().clone();
    let mut before_attempt = live.attempt(child).unwrap().clone();
    feed.terminal("start", turn(1), End::Internal);
    project(&mut live, child, &feed);
    assert_eq!(live.take_effects().seals.len(), 1);
    assert_eq!(live.node(child).unwrap().status, NodeStatus::Sealing);
    before_attempt.usage.elapsed_ms = live.attempt(child).unwrap().usage.elapsed_ms;
    let expected = rows(&live);
    let mut rebuilt = live.clone();
    rebuilt.clear_feed_views();
    rebuilt
        .project_feed(
            child,
            CHILD_SESSION,
            &feed.records[..1],
            &TreeFeedReads::default(),
        )
        .unwrap();
    assert_eq!(rebuilt.node(child).unwrap(), &before_node);
    assert_eq!(rebuilt.attempt(child).unwrap(), &before_attempt);
    project(&mut rebuilt, child, &feed);
    assert_eq!(rows(&rebuilt), expected);
    assert!(rebuilt.take_effects().is_empty());
}

#[test]
fn submitted_fold_is_agent_local_idempotent_and_rebuilds_output() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.fold_due(&other.agent_id).unwrap();
    child_command(&mut tree, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut tree, &child, &feed);
    let before_nodes = tree.nodes.clone();
    let before_attempts = tree.attempts.clone();
    let request = SubmitResultRequest {
        turn_token: 1,
        output_json: r#"{"ok":true}"#.into(),
    };
    tree.submit_result(&child, &request, None).unwrap();
    for (id, node) in &before_nodes {
        if id != &child {
            assert_eq!(&tree.nodes[id], node);
        }
    }
    assert_eq!(
        tree.attempts[&other.attempt_id],
        before_attempts[&other.attempt_id]
    );
    let revision = tree.revision;
    tree.submit_result(&child, &request, None).unwrap();
    assert_eq!(tree.revision, revision);
    assert!(tree.take_effects().is_empty());
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn close_fold_is_agent_local_and_rebuilds_without_an_owner() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.fold_due(&other.agent_id).unwrap();
    child_command(&mut tree, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut tree, &child, &feed);
    let before_nodes = tree.nodes.clone();
    let before_attempts = tree.attempts.clone();
    tree.close_agent("root", &close_req(&child), None).unwrap();
    for (id, node) in &before_nodes {
        if id != &child {
            assert_eq!(&tree.nodes[id], node);
        }
    }
    assert_eq!(
        tree.attempts[&other.attempt_id],
        before_attempts[&other.attempt_id]
    );
    let _ = tree.take_effects();
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn close_cascade_reserves_agent_local_intents_under_the_parent_fence() {
    const PARENT_SESSION: &str = "00000000-0000-4000-8000-0000000000c2";
    let (_tmp, mut live) = tree();
    live.grants.get_mut("root").unwrap().max_depth = 2;
    pin(
        &mut live,
        "orchestrate",
        "test-account",
        "selected-tuple",
        None,
    );
    let parent = live
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::ReadOnly, "parent", "delegate"),
            None,
        )
        .unwrap();
    live.allocate_session_for_test(
        &parent.agent_id,
        format!("{PARENT_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let child = live
        .spawn_agent(&parent.agent_id, &spawn_req(), None)
        .unwrap();
    live.allocate_session_for_test(
        &child.agent_id,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let mut parent_feed = Feed::new(PARENT_SESSION);
    let mut child_feed = Feed::new(CHILD_SESSION);
    child_command(
        &mut live,
        &parent.agent_id,
        &parent.attempt_id,
        "parent-start",
    );
    child_command(&mut live, &child.agent_id, &child.attempt_id, "child-start");
    parent_feed.accepted("parent-start", turn(1));
    child_feed.accepted("child-start", turn(2));
    project(&mut live, &parent.agent_id, &parent_feed);
    project(&mut live, &child.agent_id, &child_feed);
    live.record_host_cancellation(&parent.agent_id).unwrap();
    let parent_intent = live
        .operations
        .values()
        .find(|op| op.caller_agent_id == parent.agent_id && op.fold.is_some())
        .unwrap()
        .operation_id
        .clone();
    let before_child = live.node(&child.agent_id).unwrap().clone();
    let before_attempt = live.attempt(&child.agent_id).unwrap().clone();
    let mut applied = live.clone();
    applied
        .join_host_decision(&parent.agent_id, &parent_intent)
        .unwrap();
    assert_eq!(applied.node(&child.agent_id).unwrap(), &before_child);
    assert_eq!(applied.attempt(&child.agent_id).unwrap(), &before_attempt);
    assert!(
        applied
            .operations
            .values()
            .any(|op| op.tool_name == operator_intents::CLOSE_KIND
                && op.caller_agent_id == child.agent_id)
    );
    live.fold_due(&parent.agent_id).unwrap();
    assert_eq!(live.node(&child.agent_id).unwrap(), &before_child);
    live.fold_reserved_closes().unwrap();
    assert_eq!(
        live.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    let _ = live.take_effects();
    let expected = rows(&live);
    let operations = live.operations.clone();
    for parent_first in [false, true] {
        let mut rebuilt = live.clone();
        rebuilt.clear_feed_views();
        if parent_first {
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
            project(&mut rebuilt, &child.agent_id, &child_feed);
        } else {
            project(&mut rebuilt, &child.agent_id, &child_feed);
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
        }
        assert_eq!(rows(&rebuilt), expected);
        assert_eq!(rebuilt.operations, operations);
        assert!(rebuilt.take_effects().is_empty());
    }
}

#[test]
fn feed_failure_close_cascade_rebuilds_in_either_session_order() {
    const PARENT_SESSION: &str = "00000000-0000-4000-8000-0000000000c2";
    let (_tmp, mut live) = tree();
    live.grants.get_mut("root").unwrap().max_depth = 2;
    pin(
        &mut live,
        "orchestrate",
        "test-account",
        "selected-tuple",
        None,
    );
    let parent = live
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::ReadOnly, "parent", "delegate"),
            None,
        )
        .unwrap();
    live.allocate_session_for_test(
        &parent.agent_id,
        format!("{PARENT_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let child = live
        .spawn_agent(&parent.agent_id, &spawn_req(), None)
        .unwrap();
    live.allocate_session_for_test(
        &child.agent_id,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let mut parent_feed = Feed::new(PARENT_SESSION);
    let mut child_feed = Feed::new(CHILD_SESSION);
    child_command(
        &mut live,
        &parent.agent_id,
        &parent.attempt_id,
        "parent-start",
    );
    child_command(&mut live, &child.agent_id, &child.attempt_id, "child-start");
    parent_feed.accepted("parent-start", turn(1));
    child_feed.accepted("child-start", turn(2));
    project(&mut live, &parent.agent_id, &parent_feed);
    project(&mut live, &child.agent_id, &child_feed);
    parent_feed.terminal("parent-start", turn(1), End::Internal);
    project(&mut live, &parent.agent_id, &parent_feed);
    assert_eq!(
        live.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    let _ = live.take_effects();
    let expected = rows(&live);
    let operations = live.operations.clone();
    for parent_first in [false, true] {
        let mut rebuilt = live.clone();
        rebuilt.clear_feed_views();
        if parent_first {
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
            project(&mut rebuilt, &child.agent_id, &child_feed);
        } else {
            project(&mut rebuilt, &child.agent_id, &child_feed);
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
        }
        assert_eq!(rows(&rebuilt), expected);
        assert_eq!(rebuilt.operations, operations);
        assert!(rebuilt.take_effects().is_empty());
    }
}

#[test]
fn interrupt_fold_is_agent_local_and_rebuilds_without_an_owner() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.fold_due(&other.agent_id).unwrap();
    child_command(&mut tree, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut tree, &child, &feed);
    let before_nodes = tree.nodes.clone();
    let before_attempts = tree.attempts.clone();
    tree.interrupt_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.clone(),
            expected_attempt_id: Some(attempt),
        },
        Some("interrupt"),
    )
    .unwrap();
    for (id, node) in &before_nodes {
        if id != &child {
            assert_eq!(&tree.nodes[id], node);
        }
    }
    assert_eq!(tree.attempts, before_attempts);
    assert_eq!(
        tree.node(&other.agent_id).unwrap(),
        &before_nodes[&other.agent_id]
    );
    let expected = rows(&tree);
    tree.release_turn_owner(&child);
    tree.clear_feed_views();
    restore_unallocated_children(&mut tree);
    project(&mut tree, &child, &feed);
    assert_eq!(rows(&tree), expected);
    assert!(tree.take_effects().is_empty());
    let before = tree.revision;
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.revision, before);
}

#[test]
fn recovery_abandonment_fold_touches_only_its_agent_and_rebuilds() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.fold_due(&other.agent_id).unwrap();
    tree.record_host_turn_failure(
        &child,
        FailedTurnDisposition::RecoveryRequired,
        "owner uncertain".into(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(&child).unwrap();
    let before_nodes = tree.nodes.clone();
    let before_attempts = tree.attempts.clone();
    tree.abandon_recovery_for_delete(&child).unwrap();
    for (id, node) in &before_nodes {
        if id != &child {
            assert_eq!(&tree.nodes[id], node);
        }
    }
    assert_eq!(
        tree.attempts[&other.attempt_id],
        before_attempts[&other.attempt_id]
    );
    assert_child_rebuild_laws(&tree, &child, &Feed::new(CHILD_SESSION)).unwrap();
}

#[test]
fn descendant_interrupt_and_abandonment_rebuild_in_either_session_order() {
    const PARENT_SESSION: &str = "00000000-0000-4000-8000-0000000000c2";
    let (_tmp, mut live) = tree();
    live.grants.get_mut("root").unwrap().max_depth = 2;
    pin(
        &mut live,
        "orchestrate",
        "test-account",
        "selected-tuple",
        None,
    );
    let parent = live
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::ReadOnly, "parent", "delegate"),
            None,
        )
        .unwrap();
    live.allocate_session_for_test(
        &parent.agent_id,
        format!("{PARENT_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let child = live
        .spawn_agent(&parent.agent_id, &spawn_req(), None)
        .unwrap();
    live.allocate_session_for_test(
        &child.agent_id,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let mut parent_feed = Feed::new(PARENT_SESSION);
    let mut child_feed = Feed::new(CHILD_SESSION);
    child_command(
        &mut live,
        &parent.agent_id,
        &parent.attempt_id,
        "parent-start",
    );
    child_command(&mut live, &child.agent_id, &child.attempt_id, "child-start");
    parent_feed.accepted("parent-start", turn(1));
    child_feed.accepted("child-start", turn(2));
    project(&mut live, &parent.agent_id, &parent_feed);
    project(&mut live, &child.agent_id, &child_feed);
    live.interrupt_agent("root", &close_req(&parent.agent_id), None)
        .unwrap();
    live.record_host_turn_failure(
        &child.agent_id,
        FailedTurnDisposition::RecoveryRequired,
        "child owner uncertain".into(),
        Vec::new(),
    )
    .unwrap();
    live.fold_due(&child.agent_id).unwrap();
    live.abandon_recovery_for_delete(&child.agent_id).unwrap();
    let expected = rows(&live);
    for parent_first in [false, true] {
        let mut rebuilt = live.clone();
        rebuilt.clear_feed_views();
        if parent_first {
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
            project(&mut rebuilt, &child.agent_id, &child_feed);
        } else {
            project(&mut rebuilt, &child.agent_id, &child_feed);
            project(&mut rebuilt, &parent.agent_id, &parent_feed);
        }
        assert_eq!(rows(&rebuilt), expected);
        assert!(rebuilt.take_effects().is_empty());
    }
}

fn tree() -> (tempfile::TempDir, ProductAgentTree) {
    let (tmp, mut tree) = test_support::tree();
    tree.tree_id = ROOT_SESSION.to_owned();
    (tmp, tree)
}

#[test]
fn root_host_intent_anchors_use_the_root_session_cursor() {
    let tmp = tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        ROOT_SESSION,
        "root",
        "owner",
        tmp.path().to_owned(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut feed = Feed::new(ROOT_SESSION);
    feed.push(Vec::new());
    project(&mut tree, "root", &feed);
    let order = tree.revision;
    tree.record_host_retirement("root", "root retirement uncertain".into())
        .unwrap();
    let op = tree
        .operations()
        .values()
        .find(|op| op.tool_name == "host_decision")
        .unwrap();
    assert_eq!(op.fold, Some(FoldAnchor { seq: 1, order }));
}

#[test]
fn host_intent_anchors_are_recorded_once_at_the_current_cursor() {
    let (_tmp, _vcs, mut tree) = fake_tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let child = &spawned.agent_id;
    let order = tree.revision;
    tree.record_host_materialization(child).unwrap();
    let materialization = tree
        .operations()
        .values()
        .find(|op| op.tool_name == "host_decision")
        .unwrap()
        .clone();
    assert_eq!(materialization.fold, Some(FoldAnchor { seq: 0, order }));
    let unchanged = tree.revision;
    tree.record_host_materialization(child).unwrap();
    assert_eq!(tree.revision, unchanged);
    assert_eq!(
        tree.operations()[&materialization.operation_id],
        materialization
    );
    tree.allocate_session_for_test(
        child,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    tree.record_session_bound_after_failure(child).unwrap();
    tree.fold_due(child).unwrap();
    child_command(&mut tree, child, &spawned.attempt_id, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    feed.history(turn(1), Vec::new());
    project(&mut tree, child, &feed);
    // Cold recording has no process-local turn-anchor cache and must not
    // fall back to the acceptance sequence (1) instead of the cursor (2).
    tree = ProductAgentTree::reopen(tree.to_persisted());
    let order = tree.revision;
    tree.record_host_turn_failure(
        child,
        FailedTurnDisposition::RecoveryRequired,
        "owner uncertain".into(),
        Vec::new(),
    )
    .unwrap();
    let failure = tree
        .operations()
        .values()
        .find(|op| op.tool_name == "host_turn_failure")
        .unwrap();
    assert_eq!(failure.fold, Some(FoldAnchor { seq: 2, order }));
    tree.fold_due(child).unwrap();
    tree.record_host_recovery(
        child,
        &spawned.attempt_id,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "released".into(),
        },
    )
    .unwrap();
    tree.fold_due(child).unwrap();
    feed.terminal("start", turn(1), End::Completed);
    project(&mut tree, child, &feed);
    tree.record_child_finalization(
        child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        true,
        terminal_evidence(&tree, child, &feed, &turn(1).to_string()),
    )
    .unwrap();
    let mut orders = BTreeSet::new();
    for kind in [
        "host_decision",
        "session_bound_after_failure",
        "host_turn_failure",
        "host_recovery",
        "child_finalization",
    ] {
        let op = tree
            .operations()
            .values()
            .find(|op| op.tool_name == kind)
            .unwrap();
        let fold = op.fold.unwrap();
        assert!(orders.insert(fold.order), "{kind}");
        let request: serde_json::Value = serde_json::from_str(&op.canonical_request).unwrap();
        assert!(request.get("order").is_none());
        assert_eq!(
            fold.seq,
            match kind {
                "host_decision" | "session_bound_after_failure" => 0,
                "child_finalization" => 3,
                _ => 2,
            }
        );
    }
    assert!(
        tree.operations()
            .values()
            .filter(|op| !matches!(
                op.tool_name.as_str(),
                "host_decision"
                    | "session_bound_after_failure"
                    | "host_turn_failure"
                    | "host_recovery"
                    | "child_finalization"
            ))
            .all(|op| op.fold.is_none())
    );
}

#[test]
fn consumed_command_mail_rebuilds_without_a_retained_acceptance() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "consumed", &ids);
    consume(&mut tree, "consumed");
    project(&mut tree, "root", &Feed::new(ROOT_SESSION));
    let expected = rows(&tree);
    tree.clear_feed_views();
    restore_unallocated_children(&mut tree);
    project(&mut tree, "root", &Feed::new(ROOT_SESSION));
    assert_eq!(rows(&tree), expected);
}

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

fn turn(n: u64) -> TurnId {
    uuid(0x1000 + n).parse().unwrap()
}

fn call(n: u64) -> CallId {
    uuid(0x2000 + n).parse().unwrap()
}

/// A feed under construction: consecutive records of one session.
struct Feed {
    session: &'static str,
    offset: u64,
    records: Vec<FeedRecord>,
}

impl Feed {
    const fn new(session: &'static str) -> Self {
        Self {
            session,
            offset: 0,
            records: Vec::new(),
        }
    }

    fn push(&mut self, facts: Vec<FeedFact>) {
        let seq = self.offset + self.records.len() as u64 + 1;
        let change = if seq == 1 {
            serde_json::json!({"created": {"genesis": uuid(0x3000 + seq), "revision": 1}})
        } else {
            serde_json::json!({"advanced": {
                "parent": uuid(0x3000 + seq - 1),
                "checkpoint": uuid(0x3000 + seq),
                "revision": seq,
                "head": "quiescent",
            }})
        };
        let header: FeedHeader = serde_json::from_value(serde_json::json!({
            "session_id": self.session,
            "seq": seq,
            "branch_id": uuid(0xb),
            "committed_at_ms": 1_000 * seq,
            "change": change,
        }))
        .unwrap();
        self.records.push(FeedRecord::from_store(header, facts));
    }

    fn accepted(&mut self, command: &str, turn_id: TurnId) {
        self.push(vec![FeedFact::CommandAccepted {
            command_id: CommandId::new(command).unwrap(),
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::UserPrompt,
            },
            turn_id: Some(turn_id),
            sequence: self.records.len() as u64 + 1,
            operation_id: None,
        }]);
    }

    fn rejected(&mut self, command: &str) {
        self.push(vec![FeedFact::CommandRejected {
            command_id: CommandId::new(command).unwrap(),
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::UserPrompt,
            },
            error: ControlErrorKind::Backpressure,
        }]);
    }

    fn history(&mut self, turn_id: TurnId, calls: Vec<CallId>) {
        let entry: EntryId = uuid(0x4000 + self.records.len() as u64).parse().unwrap();
        self.push(vec![FeedFact::HistoryAppended {
            turn_id: Some(turn_id),
            segment: HistorySegmentKind::ToolExchange,
            first_ordinal: 0,
            count: 1,
            first_entry: entry,
            last_entry: entry,
            tool_results: calls,
        }]);
    }

    /// A terminal whose usage is derived from its sequence: `seq` tool
    /// calls, `100 * seq` input and `10 * seq` output tokens.
    fn terminal(&mut self, command: &str, turn_id: TurnId, end: End) {
        let seq = self.records.len() as u64 + 1;
        self.terminal_with(command, turn_id, end, turn_usage(seq, 100 * seq, 10 * seq));
    }

    fn terminal_with(&mut self, command: &str, turn_id: TurnId, end: End, usage: TurnUsage) {
        let (outcome, closed_exchange) = end.outcome();
        self.push(vec![FeedFact::TurnTerminal {
            turn_id,
            command_id: CommandId::new(command).unwrap(),
            outcome,
            closed_exchange,
            steering_settled_unapplied: false,
            usage,
        }]);
    }
}

fn turn_usage(tool_calls: u64, input_tokens: u64, output_tokens: u64) -> TurnUsage {
    TurnUsage::new(
        plakous::model::Usage::new(input_tokens, output_tokens, 0, 0, 0),
        1,
        1,
        tool_calls,
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug)]
enum End {
    Completed,
    Internal,
    Model(plakous::model::ModelErrorKind),
    Withheld,
    Abandoned,
    RecoveryRequired,
}

impl End {
    fn outcome(self) -> (FeedTurnOutcome, bool) {
        match self {
            Self::Completed => (
                FeedTurnOutcome::Completed {
                    finish: FinishReason::Stop,
                },
                true,
            ),
            Self::Internal => (
                FeedTurnOutcome::Failed {
                    kind: AgentErrorKind::Internal,
                    model_kind: None,
                    context: None,
                },
                false,
            ),
            Self::Model(model_kind) => (
                FeedTurnOutcome::Failed {
                    kind: AgentErrorKind::Model,
                    model_kind: Some(model_kind),
                    context: None,
                },
                true,
            ),
            Self::Withheld => (
                FeedTurnOutcome::Failed {
                    kind: AgentErrorKind::Model,
                    model_kind: None,
                    context: Some(
                        serde_json::from_value(
                            serde_json::json!({"withheld": true, "remedy": "resend_shorter"}),
                        )
                        .unwrap(),
                    ),
                },
                true,
            ),
            Self::Abandoned => (
                FeedTurnOutcome::Cancelled {
                    cause: FeedCancelCause::Abandoned {
                        claim: AbandonedTurnClaim::RunningTool,
                        cause: AbandonmentCause::ProcessLost,
                        model_round_completed: true,
                        tool_round_completed: false,
                    },
                },
                false,
            ),
            Self::RecoveryRequired => (FeedTurnOutcome::RecoveryRequired, false),
        }
    }
}

/// A root with `results` settled children, each with one unread result
/// envelope, in mailbox order.
fn root_with_results(results: usize) -> (tempfile::TempDir, ProductAgentTree, Vec<String>) {
    let (tmp, mut tree) = tree();
    for _ in 0..results {
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.complete_from_intent_for_test(&child.agent_id).unwrap();
        tree.settle_for_test();
    }
    tree.fold_due("root").unwrap();
    let ids = tree
        .pending_result_mail("root")
        .into_iter()
        .map(|mail| mail.message_id)
        .collect();
    (tmp, tree, ids)
}

fn durable(tree: &ProductAgentTree, id: &str) -> MailDurableState {
    tree.mail(id).unwrap().durable
}

fn batch(tree: &mut ProductAgentTree, command: &str, ids: &[String]) {
    tree.record_command(AgentCommand::intent(
        command,
        "root",
        None,
        AgentCommandKind::RootBatch,
        format!("batch {command}"),
        ids.to_vec(),
    ))
    .unwrap();
}

#[test]
fn behind_retention_command_markers_survive_store_reopen_and_view_rebuild() {
    let (_tmp, mut tree, _) = root_with_results(0);
    batch(&mut tree, "lost-a", &[]);
    batch(&mut tree, "lost-b", &[]);
    tree.mark_commands_behind_retention("root").unwrap();
    consume(&mut tree, "lost-a");
    assert!(!tree.command_behind_retention("lost-a"));
    tree.seed_feed_cursor(ROOT_SESSION, 20);
    let store = MemoryTreeStore::default();
    tree.acknowledge_revision(store.save(&tree).unwrap());
    let mut restored = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
    assert!(restored.command("lost-b").unwrap().unresolved());
    assert!(restored.command_behind_retention("lost-b"));
    restored.clear_feed_views();
    assert!(restored.command_behind_retention("lost-b"));
    batch(&mut restored, "new-after-catch-up", &[]);
    assert!(!restored.command_behind_retention("new-after-catch-up"));
    let before = restored.operations().clone();
    restored.mark_commands_behind_retention("nobody").unwrap();
    assert_eq!(&before, restored.operations());
}

#[test]
fn command_settlement_removes_only_its_retention_marker() {
    let (_tmp, mut tree, _) = root_with_results(0);
    for id in ["consumed", "withdrawn", "accepted", "rejected", "pending"] {
        batch(&mut tree, id, &[]);
    }
    tree.mark_commands_behind_retention("root").unwrap();
    consume(&mut tree, "consumed");
    tree.withdraw_command("withdrawn");
    let mut feed = Feed::new(ROOT_SESSION);
    feed.accepted("accepted", turn(1));
    feed.rejected("rejected");
    project(&mut tree, "root", &feed);
    for id in ["consumed", "withdrawn", "accepted", "rejected"] {
        assert!(!tree.command_behind_retention(id), "{id}");
    }
    assert!(tree.command_behind_retention("pending"));
}

fn project(tree: &mut ProductAgentTree, agent: &str, feed: &Feed) -> TreeProjection {
    restore_unallocated_children(tree);
    tree.project_feed(
        agent,
        feed.session,
        &feed.records,
        &TreeFeedReads::default(),
    )
    .unwrap()
}

fn terminal_evidence(
    tree: &ProductAgentTree,
    agent: &str,
    feed: &Feed,
    turn_id: &str,
) -> Option<ChildTerminal> {
    project(&mut tree.clone(), agent, feed)
        .ended_turns
        .into_iter()
        .map(|(_, terminal)| terminal)
        .find(|terminal| terminal.turn_id == turn_id)
}

/// A wait receipt for `call_id` that acknowledged `ids`.
fn wait_receipt(tree: &mut ProductAgentTree, caller: &str, call_id: CallId, ids: &[String]) {
    let mut acknowledged = Vec::new();
    for id in ids {
        acknowledged.push(tree.mail(id).unwrap().clone());
        tree.acknowledge_mail(id).unwrap();
    }
    tree.remember_success(
        &call_id.to_string(),
        caller,
        AWAIT_RECEIPT_KIND,
        &serde_json::json!({}),
        &WaitReceipt {
            output: serde_json::json!({}),
            acknowledged,
            newly_delivered: Vec::new(),
        },
    )
    .unwrap();
}

#[test]
fn a_committed_wait_call_retires_its_receipt_and_keeps_the_mail_read() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    wait_receipt(&mut tree, "root", call(1), &ids);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.history(turn(1), vec![call(9), call(1)]);
    project(&mut tree, "root", &feed);
    assert!(tree.retained_wait_receipts("root").is_empty());
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Acknowledged);
    assert_eq!(tree.feed_cursor(ROOT_SESSION), 1);
}

#[test]
fn an_unclosed_terminal_restores_the_turns_waits_and_a_closed_one_retires_them() {
    let (_tmp, mut tree, ids) = root_with_results(2);
    wait_receipt(&mut tree, "root", call(1), &ids[..1]);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.terminal("cmd-1", turn(1), End::RecoveryRequired);
    project(&mut tree, "root", &feed);
    assert_eq!(
        tree.retained_wait_receipts("root"),
        vec![call(1).to_string()],
        "a recovery-required terminal leaves the receipts for the record that closes the turn"
    );
    feed.terminal("cmd-1", turn(1), End::Abandoned);
    project(&mut tree, "root", &feed);
    assert!(tree.retained_wait_receipts("root").is_empty());
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);

    wait_receipt(&mut tree, "root", call(2), &ids[1..]);
    feed.terminal("cmd-2", turn(2), End::Completed);
    project(&mut tree, "root", &feed);
    assert!(tree.retained_wait_receipts("root").is_empty());
    assert_eq!(durable(&tree, &ids[1]), MailDurableState::Acknowledged);
}

#[test]
fn a_withheld_admission_returns_its_batch_once_from_the_terminal_fact() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-1", &ids);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.accepted("cmd-1", turn(1));
    project(&mut tree, "root", &feed);
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Acknowledged);
    assert_eq!(
        tree.command("cmd-1").unwrap().fact,
        Some(CommandFact::Accepted)
    );

    feed.terminal("cmd-1", turn(1), End::Withheld);
    let applied = project(&mut tree, "root", &feed);
    assert_eq!(applied.applied, 1);
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);
    assert!(tree.admitted_mail_returned("cmd-1"));
    assert_eq!(
        tree.command("cmd-1").unwrap().fact,
        Some(CommandFact::Returned)
    );

    // A later admission acknowledges the returned mail; replaying the page
    // cannot return it a second time.
    batch(&mut tree, "cmd-2", &ids);
    feed.accepted("cmd-2", turn(2));
    project(&mut tree, "root", &feed);
    let again = project(&mut tree, "root", &feed);
    assert_eq!(again.applied, 0);
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Acknowledged);
    assert!(!tree.admitted_mail_returned("cmd-1"));
}

#[test]
fn a_rejected_command_consumes_nothing() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-1", &ids);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.rejected("cmd-1");
    project(&mut tree, "root", &feed);
    assert_eq!(
        tree.command("cmd-1").unwrap().fact,
        Some(CommandFact::Rejected)
    );
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);
}

#[test]
fn a_discarded_host_wake_withdraws_its_batch_at_the_open_join() {
    let (_tmp, mut tree, ids) = root_with_results(3);
    batch(&mut tree, "plak:wake:1", &ids[..1]);
    batch(&mut tree, "user:2", &ids[1..2]);
    batch(&mut tree, "user:3", &ids[2..]);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.accepted("user:3", turn(3));
    project(&mut tree, "root", &feed);

    // The product names only its pending user submission; the wake it
    // discarded is not named, and user:3 is recorded.
    let withdrawn = tree.join_pending_root_commands(&["user:2".to_owned()]);
    assert_eq!(withdrawn, vec!["plak:wake:1".to_owned()]);
    assert!(tree.command("plak:wake:1").unwrap().withdrawn);
    assert!(tree.command("user:2").unwrap().unresolved());
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);
    assert!(
        tree.pending_result_mail("root")
            .iter()
            .any(|mail| mail.message_id == ids[0]),
        "the withdrawn batch's mail is unread for the next admission"
    );
    // The join is idempotent.
    assert!(
        tree.join_pending_root_commands(&["user:2".to_owned()])
            .is_empty()
    );
}

#[test]
fn a_command_intent_is_immutable_and_capped() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-1", &ids);
    batch(&mut tree, "cmd-1", &ids);
    let conflict = tree.record_command(AgentCommand::intent(
        "cmd-1",
        "root",
        None,
        AgentCommandKind::RootBatch,
        "other bytes",
        ids.clone(),
    ));
    assert_eq!(
        conflict.unwrap_err().code,
        TopologyErrorCode::OperationRecoveryRequired
    );
    for n in 1..MAX_TREE_COMMANDS {
        batch(&mut tree, &format!("fill-{n}"), &[]);
    }
    let full = tree.record_command(AgentCommand::intent(
        "one-more",
        "root",
        None,
        AgentCommandKind::RootBatch,
        "",
        Vec::new(),
    ));
    assert_eq!(full.unwrap_err().code, TopologyErrorCode::Capacity);
}

impl ProductAgentTree {
    fn allocate_session_for_test(
        &mut self,
        agent: &str,
        locator: impl Into<String>,
    ) -> Result<(), TopologyError> {
        let locator = locator.into();
        let locator = if locator.parse::<plakous::sessions::SessionLocator>().is_ok() {
            locator
        } else {
            format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001))
        };
        self.record_child_session(agent, &locator)?;
        self.bind_session_locator(agent, locator)
    }

    fn complete_from_intent_for_test(&mut self, agent: &str) -> Result<(), TopologyError> {
        let attempt = self.attempt(agent).unwrap().attempt_id.clone();
        let turn = turn(0).to_string();
        let terminal = ChildTerminal {
            attempt_id: attempt,
            turn_id: turn.clone(),
            committed_at_ms: now_unix_millis(),
            outcome: FeedTurnOutcome::Completed {
                finish: FinishReason::Stop,
            },
            seq: self.fold_anchor_now(agent)?.seq,
        };
        self.record_child_finalization(
            agent,
            &turn,
            ChildFinalizationDecision::Finalize,
            true,
            Some(terminal),
        )?;
        self.fold_due(agent)?;
        Ok(())
    }
}

fn restore_unallocated_children(tree: &mut ProductAgentTree) {
    let agents: Vec<_> = tree
        .preorder_ids()
        .into_iter()
        .rev()
        .filter(|id| {
            !tree
                .operations
                .contains_key(&format!("{CHILD_SESSION_KIND}:{id}"))
        })
        .collect();
    for agent in agents {
        tree.ensure_fold_started(&agent).unwrap();
        tree.fold_due(&agent).unwrap();
    }
    tree.lift_restored_pause_if_quiescent();
}

/// A running child with one open attempt and a session.
fn running_child(tree: &mut ProductAgentTree) -> (String, AttemptId) {
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.allocate_session_for_test(
        &child.agent_id,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    (child.agent_id, child.attempt_id)
}

#[test]
fn submitted_host_decisions_refuse_recovery_at_record_time() {
    for materialization in [false, true] {
        let (_tmp, mut tree) = tree();
        let (child, _) = running_child(&mut tree);
        tree.submit_result(
            &child,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.nodes.get_mut(&child).unwrap().status = NodeStatus::RecoveryRequired;
        let before = rows(&tree);
        let result = if materialization {
            tree.record_host_materialization(&child)
        } else {
            tree.record_host_cancellation(&child)
        };
        assert_eq!(
            result.unwrap_err().code,
            TopologyErrorCode::RecipientRecoveryRequired
        );
        assert_eq!(rows(&tree), before);
        tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
    }
}

#[test]
fn a_refused_host_join_does_not_poison_the_feed_or_later_intents() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut tree, &child, &feed);
    tree.submit_result(
        &child,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_host_cancellation(&child).unwrap();
    tree.nodes.get_mut(&child).unwrap().status = NodeStatus::RecoveryRequired;
    let before = tree.attempt(&child).unwrap().clone();
    let refused = tree.fold_due(&child).unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(
        refused[0].error.code,
        TopologyErrorCode::RecipientRecoveryRequired
    );
    assert_eq!(tree.attempt(&child).unwrap(), &before);
    assert_eq!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.node(&child).unwrap().fold_order,
        tree.operations[&refused[0].operation_id]
            .fold
            .map(|fold| fold.order)
    );
    assert!(tree.take_effects().is_empty());
    tree.record_host_retirement(&child, "owner still uncertain".into())
        .unwrap();
    feed.history(turn(1), Vec::new());
    let projection = tree
        .project_feed(
            &child,
            CHILD_SESSION,
            &feed.records,
            &TreeFeedReads::default(),
        )
        .expect("semantic refusal must not abort the page");
    assert_eq!(projection.cursor, 2);
    assert!(
        projection.refused_intents.is_empty(),
        "the refused position was already folded"
    );
    assert_eq!(
        tree.attempt(&child).unwrap().failure.as_ref().unwrap().kind,
        "session_retirement"
    );
    assert!(tree.take_effects().is_empty());
}

#[test]
fn an_ignored_host_join_does_not_lift_restored_paused() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_host_cancellation(&child).unwrap();
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .unwrap();
    tree.state = TreeAdmissionState::RestoredPaused;
    tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.state, TreeAdmissionState::RestoredPaused);
    assert_eq!(
        tree.node("root").unwrap().presentation_status(tree.state),
        NodeStatus::Paused
    );
}

#[test]
fn explicit_finalize_refused_by_recovery_stays_inert_after_recovery_settles() {
    let cancelled = HostInterleaving {
        decision: GeneratedHostDecision::Cancelled,
        terminal_first: false,
        failed_terminal: false,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: false,
        interrupt: false,
        withdraw_before_start: true,
    };
    let steps = [
        cancelled,
        HostInterleaving {
            decision: GeneratedHostDecision::TurnRecovery,
            late_finalize: true,
            ..cancelled
        },
    ];
    assert_generated_host_interleavings(&steps).unwrap();
}

#[test]
fn host_decision_below_retention_rebuilds_its_live_outcome() {
    assert_intent_outcome_below_retention(RetainedIntent::Cancelled);
}

#[test]
fn an_anchor_zero_retirement_precedes_later_command_acceptance() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    tree.record_host_retirement(&child, "release uncertain".into())
        .unwrap();
    tree.fold_due(&child).unwrap();
    assert_eq!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(tree.feed_cursor(CHILD_SESSION), 0);
    let mut feed = Feed::new(CHILD_SESSION);
    feed.push(Vec::new());
    feed.accepted("start-1", turn(1));
    project(&mut tree, &child, &feed);
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id,
        Some(turn(1).to_string())
    );
    assert_eq!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(!tree.node(&child).unwrap().active_turn);
}

#[test]
fn host_turn_failure_below_retention_rebuilds_its_live_outcome() {
    assert_intent_outcome_below_retention(RetainedIntent::Failed);
}

#[test]
fn finalize_below_retention_rebuilds_its_live_outcome() {
    assert_intent_outcome_below_retention(RetainedIntent::Finalize);
}

#[test]
fn blocked_finalize_below_retention_rebuilds_its_live_outcome_without_turns() {
    assert_intent_outcome_below_retention(RetainedIntent::MaxTurnsFinalize);
}

#[test]
fn retained_window_host_decision_and_finalize_rebuild_from_normal_records() {
    for finalize in [false, true] {
        let (_tmp, mut live) = tree();
        let child = live.spawn_agent("root", &spawn_req(), None).unwrap();
        let locator = format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001));
        live.record_child_session(&child.agent_id, &locator)
            .unwrap();
        let mut feed = Feed::new(CHILD_SESSION);
        for n in 1..=3 {
            let command = format!("start-{n}");
            child_command(&mut live, &child.agent_id, &child.attempt_id, &command);
            feed.accepted(&command, turn(n));
            if n != 3 || finalize {
                feed.terminal(&command, turn(n), End::Completed);
            }
            project(&mut live, &child.agent_id, &feed);
        }
        if finalize {
            live.record_child_finalization(
                &child.agent_id,
                &turn(3).to_string(),
                ChildFinalizationDecision::Finalize,
                true,
                terminal_evidence(&live, &child.agent_id, &feed, &turn(3).to_string()),
            )
            .unwrap();
        } else {
            live.record_host_cancellation(&child.agent_id).unwrap();
        }
        live.fold_due(&child.agent_id).unwrap();
        let mut expected = live.attempt(&child.agent_id).unwrap().clone();
        expected.usage.turns = 2;
        for fact in feed.records[..2].iter().flat_map(FeedRecord::facts) {
            if let FeedFact::TurnTerminal { usage, .. } = fact {
                expected.usage.tool_calls -= u32::try_from(usage.local_tool_attempts()).unwrap();
                expected.usage.input_tokens -= usage.model().input_tokens();
                expected.usage.output_tokens -= usage.model().output_tokens();
            }
        }
        let expected_node = live.node(&child.agent_id).unwrap().clone();
        let head = expected_node
            .session_locator
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        let retained = TreeFeed {
            session_id: CHILD_SESSION.to_owned(),
            retained_from: 3,
            below_retained: Some(2),
            heads: vec![head],
            head_turns: BTreeMap::from([(head, (!finalize).then(|| turn(3)))]),
            records: feed.records[2..].to_vec(),
            reads: TreeFeedReads::default(),
        };
        let mut rebuilt = live.clone();
        rebuilt.clear_feed_views();
        rebuilt = ProductAgentTree::reopen(rebuilt.to_persisted());
        let projection = retained.apply(&mut rebuilt, &child.agent_id).unwrap();
        assert!(
            projection.conflicts.is_empty(),
            "a seeded head must not conflict with retained admissions"
        );
        assert_eq!(rebuilt.attempt(&child.agent_id).unwrap(), &expected);
        assert_eq!(rebuilt.node(&child.agent_id).unwrap(), &expected_node);
        assert!(
            !rebuilt
                .operations
                .values()
                .any(|operation| operation.tool_name == "consumed_below_retention")
        );
        assert!(rebuilt.take_effects().is_empty());
    }
}

enum RetainedIntent {
    Cancelled,
    Failed,
    Finalize,
    MaxTurnsFinalize,
}

fn assert_intent_outcome_below_retention(intent: RetainedIntent) {
    let (_tmp, mut live) = tree();
    let finalize = matches!(
        intent,
        RetainedIntent::Finalize | RetainedIntent::MaxTurnsFinalize
    );
    if matches!(intent, RetainedIntent::MaxTurnsFinalize) {
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::ReadOnly;
        binding.max_turns = Some(1);
        live.pin_template("review", binding);
    }
    let child = live.spawn_agent("root", &spawn_req(), None).unwrap();
    let locator = format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001));
    live.record_child_session(&child.agent_id, &locator)
        .unwrap();
    child_command(&mut live, &child.agent_id, &child.attempt_id, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    if finalize {
        feed.terminal("start-1", turn(1), End::Completed);
    }
    project(&mut live, &child.agent_id, &feed);
    if finalize {
        live.record_child_finalization(
            &child.agent_id,
            &turn(1).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            terminal_evidence(&live, &child.agent_id, &feed, &turn(1).to_string()),
        )
        .unwrap();
    } else if matches!(intent, RetainedIntent::Cancelled) {
        live.record_host_cancellation(&child.agent_id).unwrap();
    } else {
        live.record_host_turn_failure(
            &child.agent_id,
            FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
            "host failed below retention".into(),
            Vec::new(),
        )
        .unwrap();
    }
    live.fold_due(&child.agent_id).unwrap();
    live.seed_head_turn(&child.agent_id, None).unwrap();
    let mut expected = live.attempt(&child.agent_id).unwrap().clone();
    if matches!(intent, RetainedIntent::MaxTurnsFinalize) {
        assert_eq!(expected.status, AttemptStatus::Blocked);
    }
    // Retention restarts feed-derived counters, not the saved outcome.
    expected.usage.turns = 0;
    expected.usage.tool_calls = 0;
    expected.usage.input_tokens = 0;
    expected.usage.output_tokens = 0;
    let mut expected_node = live.node(&child.agent_id).unwrap().clone();
    expected_node.fold_order = Some(0);
    let head = expected_node
        .session_locator
        .as_ref()
        .unwrap()
        .parse()
        .unwrap();
    let retained = TreeFeed {
        session_id: CHILD_SESSION.to_owned(),
        retained_from: 30,
        below_retained: Some(40),
        heads: vec![head],
        head_turns: BTreeMap::from([(head, None)]),
        records: Vec::new(),
        reads: TreeFeedReads::default(),
    };
    let mut rebuilt = live.clone();
    rebuilt.clear_feed_views();
    rebuilt = ProductAgentTree::reopen(rebuilt.to_persisted());
    retained.apply(&mut rebuilt, &child.agent_id).unwrap();
    assert_eq!(rebuilt.attempt(&child.agent_id).unwrap(), &expected);
    assert_eq!(rebuilt.node(&child.agent_id).unwrap(), &expected_node);
    assert!(rebuilt.take_effects().is_empty());
    let consumed = rebuilt
        .operations
        .values()
        .filter(|operation| operation.tool_name == "consumed_below_retention")
        .map(|operation| operation.canonical_request.clone())
        .collect::<Vec<_>>();
    assert!(consumed.is_empty());
    if finalize {
        rebuilt
            .record_child_finalization(
                &child.agent_id,
                &turn(1).to_string(),
                ChildFinalizationDecision::Finalize,
                true,
                terminal_evidence(&rebuilt, &child.agent_id, &feed, &turn(1).to_string()),
            )
            .unwrap();
    }
    retained.apply(&mut rebuilt, &child.agent_id).unwrap();
    assert_eq!(rebuilt.attempt(&child.agent_id).unwrap(), &expected);
    assert!(rebuilt.take_effects().is_empty());
    assert_eq!(
        rebuilt
            .operations
            .values()
            .filter(|operation| operation.tool_name == "consumed_below_retention")
            .map(|operation| operation.canonical_request.clone())
            .collect::<Vec<_>>(),
        consumed
    );
}

#[test]
fn materialization_restore_is_ordered_and_rebuilds_as_running() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_host_materialization(&child).unwrap();
    tree.fold_due(&child).unwrap();
    tree.record_session_bound_after_failure(&child).unwrap();
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Running);
    let expected = tree.attempt(&child).unwrap().clone();
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.attempt(&child).unwrap(), &expected);
    tree.clear_feed_views();
    tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(&child).unwrap(), &expected);
    assert!(tree.take_effects().seals.is_empty());
}

#[test]
fn a_second_materialization_failure_after_restore_has_a_new_identity() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_fault(&child, "first failure");
    tree.record_host_materialization(&child).unwrap();
    tree.fold_due(&child).unwrap();
    tree.record_session_bound_after_failure(&child).unwrap();
    tree.fold_due(&child).unwrap();
    tree.record_fault(&child, "second failure");
    tree.record_host_materialization(&child).unwrap();
    tree.fold_due(&child).unwrap();
    assert_eq!(
        tree.attempt(&child)
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .message,
        "second failure"
    );
    assert_eq!(
        tree.attempt(&child).unwrap().status,
        AttemptStatus::RecoveryRequired
    );
    let expected = tree.attempt(&child).unwrap().clone();
    tree.clear_feed_views();
    tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(&child).unwrap(), &expected);
}

#[test]
fn stopped_command_acceptance_rebuilds_the_binding_without_activating_the_node() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    tree.close_agent("root", &close_req(&child), None).unwrap();
    project(&mut tree, &child, &feed);
    tree.clear_feed_views();
    project(&mut tree, &child, &feed);
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id,
        Some(turn(1).to_string())
    );
    assert!(!tree.node(&child).unwrap().active_turn);
}

#[test]
fn a_resolved_vcs_obligation_does_not_stick_in_a_host_intent() {
    let (_tmp, _, mut tree) = fake_tree();
    let child = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    tree.allocate_session_for_test(
        &child,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let effects = tree.take_effects();
    assert_eq!(effects.seals.len(), 1);
    // A workspace obligation can arrive after the host outcome. Clearing its
    // view must not copy that temporary observation into the intent fence.
    tree.nodes.get_mut(&child).unwrap().status = NodeStatus::RecoveryRequired;
    tree.clear_feed_views();
    project(&mut tree, &child, &Feed::new(CHILD_SESSION));
    let _ = tree.complete(effects.run(tree.vcs()));
    tree.clear_feed_views();
    project(&mut tree, &child, &Feed::new(CHILD_SESSION));
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Failed);
    assert!(
        tree.operations
            .values()
            .filter(|op| op.tool_name == "host_decision")
            .all(|op| op.terminal_response.as_deref() == Some("effects_reserved"))
    );
}

#[test]
fn adoption_preserves_a_reflected_host_recovery_when_intents_rejoin() {
    let (_tmp, vcs, mut tree) = fake_tree();
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child = &spawned.agent_id;
    tree.allocate_session_for_test(
        child,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    tree.record_host_retirement(child, "release uncertain".into())
        .unwrap();
    tree.fold_due(child).unwrap();
    tree.record_host_recovery(
        child,
        &spawned.attempt_id,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "released".into(),
        },
    )
    .unwrap();
    tree.fold_due(child).unwrap();
    let effects = tree.take_effects();
    let _ = tree.complete(effects.run(&*vcs));
    let candidate = candidate_of(&tree, child);
    tree.adopt_candidate("root", &adopt_req(&candidate), None)
        .unwrap();
    let expected_attempt = tree.attempt(child).unwrap().clone();
    let expected_node = tree.node(child).unwrap().clone();
    tree.fold_due(child).unwrap();
    assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(tree.node(child).unwrap(), &expected_node);
    tree.clear_feed_views();
    tree.project_feed(child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(tree.node(child).unwrap(), &expected_node);
    assert!(tree.take_effects().is_empty());
}

#[test]
fn host_decision_rebuild_keeps_a_superseded_attempt_and_its_followup_separate() {
    for forked in [false, true] {
        for bound in [false, true] {
            for submitted in [false, true] {
                let (_tmp, mut tree) = if forked {
                    let (tmp, _, tree) = fake_tree();
                    (tmp, tree)
                } else {
                    tree()
                };
                let spawned = tree
                    .spawn_agent("root", &if forked { mint_req() } else { spawn_req() }, None)
                    .unwrap();
                let (child, first) = (spawned.agent_id, spawned.attempt_id);
                tree.allocate_session_for_test(
                    &child,
                    format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
                )
                .unwrap();
                let mut feed = Feed::new(CHILD_SESSION);
                if bound {
                    child_command(&mut tree, &child, &first, "first-start");
                    feed.accepted("first-start", turn(1));
                    project(&mut tree, &child, &feed);
                }
                if submitted {
                    tree.record_operator_intent(
                        &child,
                        Some(first.clone()),
                        operator_intents::Decision::Submitted {
                            output_json: r#"{"done":true}"#.to_owned(),
                        },
                    )
                    .unwrap();
                    tree.record_host_materialization(&child).unwrap();
                } else {
                    tree.record_host_cancellation(&child).unwrap();
                }
                tree.fold_due(&child).unwrap();
                let effects = tree.take_effects();
                assert_eq!(effects.seals.len(), usize::from(forked));
                let _ = tree.complete(effects.run(tree.vcs()));
                let expected_first = tree.attempts[&first].clone();
                let second = tree
                    .open_followup_attempt(&child, "followup", String::new(), String::new())
                    .unwrap();
                child_command(&mut tree, &child, &second, "second-start");
                feed.accepted("second-start", turn(2));
                project(&mut tree, &child, &feed);
                let expected_second = tree.attempts[&second].clone();
                let expected_node = tree.node(&child).unwrap().clone();
                let _ = tree.take_effects();
                tree.clear_feed_views();
                project(&mut tree, &child, &feed);
                assert_eq!(
                    tree.attempts[&first], expected_first,
                    "bound={bound}, submitted={submitted}"
                );
                assert_eq!(tree.attempts[&second], expected_second);
                assert_eq!(tree.node(&child).unwrap(), &expected_node);
                assert!(tree.take_effects().seals.is_empty());
            }
        }
    }
}

#[test]
fn a_preserved_closed_followup_does_not_suppress_historical_intents() {
    let (_tmp, mut tree) = tree();
    let (child, first) = running_child(&mut tree);
    tree.record_host_cancellation(&child).unwrap();
    tree.fold_due(&child).unwrap();
    let expected_first = tree.attempts[&first].clone();
    tree.open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    tree.close_agent("root", &close_req(&child), None).unwrap();
    let _ = tree.take_effects();
    let expected_current = tree.attempt(&child).unwrap().clone();
    tree.clear_feed_views();
    project(&mut tree, &child, &Feed::new(CHILD_SESSION));
    assert_eq!(tree.attempts[&first], expected_first);
    assert_eq!(tree.attempt(&child).unwrap(), &expected_current);
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
    assert!(tree.take_effects().is_empty());
}

#[test]
fn recovered_and_finalized_attempts_rebuild_after_a_followup() {
    for decision in 0..3 {
        let (_tmp, mut tree) = tree();
        let (child, first) = running_child(&mut tree);
        child_command(&mut tree, &child, &first, "first-start");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("first-start", turn(1));
        project(&mut tree, &child, &feed);
        if decision == 2 {
            feed.terminal("first-start", turn(1), End::Completed);
            project(&mut tree, &child, &feed);
            tree.record_child_finalization(
                &child,
                &turn(1).to_string(),
                ChildFinalizationDecision::Finalize,
                true,
                terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
            )
            .unwrap();
            tree.fold_due(&child).unwrap();
        } else {
            if decision == 0 {
                tree.record_host_materialization(&child).unwrap();
            } else {
                tree.record_host_turn_failure(
                    &child,
                    FailedTurnDisposition::RecoveryRequired,
                    "host could not reopen the turn".to_owned(),
                    Vec::new(),
                )
                .unwrap();
            }
            tree.fold_due(&child).unwrap();
            tree.record_host_recovery(
                &child,
                &first,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "reopened quiescent".to_owned(),
                },
            )
            .unwrap();
            tree.fold_due(&child).unwrap();
        }
        let expected_first = tree.attempts[&first].clone();
        let second = tree
            .open_followup_attempt(&child, "followup", String::new(), String::new())
            .unwrap();
        child_command(&mut tree, &child, &second, "second-start");
        feed.accepted("second-start", turn(2));
        project(&mut tree, &child, &feed);
        let expected_second = tree.attempts[&second].clone();
        let expected_node = tree.node(&child).unwrap().clone();
        let _ = tree.take_effects();
        tree.clear_feed_views();
        project(&mut tree, &child, &feed);
        assert_eq!(tree.attempts[&first], expected_first, "decision={decision}");
        assert_eq!(tree.attempts[&second], expected_second);
        assert_eq!(tree.node(&child).unwrap(), &expected_node);
        assert!(tree.take_effects().seals.is_empty());
    }
}

#[test]
fn a_host_failure_rebuilds_its_deleted_historical_outcome_without_touching_the_followup() {
    let (_tmp, mut tree) = tree();
    let (child, first) = running_child(&mut tree);
    tree.record_host_turn_failure(
        &child,
        FailedTurnDisposition::Failed {
            kind: TURN_ABANDONED,
        },
        "the old turn was abandoned".to_owned(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(&child).unwrap();
    let expected_first = tree.attempts[&first].clone();
    let second = tree
        .open_followup_attempt(&child, "followup", String::new(), String::new())
        .unwrap();
    let expected_second = tree.attempts[&second].clone();
    let expected_node = tree.node(&child).unwrap().clone();
    let _ = tree.take_effects();
    tree.clear_feed_views();
    let deleted = tree.attempts.get_mut(&first).unwrap();
    deleted.status = AttemptStatus::Running;
    deleted.failure = None;
    deleted.ended_at = None;
    project(&mut tree, &child, &Feed::new(CHILD_SESSION));
    assert_eq!(tree.attempts[&first], expected_first);
    assert_eq!(tree.attempts[&second], expected_second);
    assert_eq!(tree.node(&child).unwrap(), &expected_node);
    assert!(tree.take_effects().seals.is_empty());
}

#[test]
fn host_intents_share_record_order_across_kinds() {
    for failure_first in [false, true] {
        let (_tmp, mut tree) = tree();
        let (child, _) = running_child(&mut tree);
        if failure_first {
            tree.record_host_turn_failure(
                &child,
                FailedTurnDisposition::Failed {
                    kind: TURN_ABANDONED,
                },
                "first failure".into(),
                Vec::new(),
            )
            .unwrap();
            tree.record_host_materialization(&child).unwrap();
        } else {
            tree.record_host_materialization(&child).unwrap();
            tree.record_host_turn_failure(
                &child,
                FailedTurnDisposition::Failed {
                    kind: TURN_ABANDONED,
                },
                "later failure".into(),
                Vec::new(),
            )
            .unwrap();
        }
        tree.fold_due(&child).unwrap();
        let expected = tree.attempt(&child).unwrap().clone();
        assert_eq!(expected.failure.as_ref().unwrap().kind, TURN_ABANDONED);
        tree.clear_feed_views();
        tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.attempt(&child).unwrap(), &expected);
    }
}

#[test]
fn host_decisions_preserve_another_stopped_outcome_and_its_clear() {
    for status in [AttemptStatus::Closed, AttemptStatus::Failed] {
        let (_tmp, mut tree) = tree();
        let (child, _) = running_child(&mut tree);
        if status == AttemptStatus::Failed {
            tree.record_host_turn_failure(
                &child,
                FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                "another outcome".into(),
                Vec::new(),
            )
            .unwrap();
            tree.fold_due(&child).unwrap();
        } else {
            tree.close_agent("root", &close_req(&child), None).unwrap();
        }
        let expected = tree.current_attempt(&child).unwrap().clone();
        tree.record_host_materialization(&child).unwrap();
        tree.fold_due(&child).unwrap();
        assert_eq!(tree.attempt(&child).unwrap(), &expected);
        tree.clear_feed_views();
        tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.attempt(&child).unwrap(), &expected);
    }
}

#[test]
fn an_anchored_adoption_preserves_its_stopped_outcome_across_host_replay() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    tree.complete_from_intent_for_test(&child).unwrap();
    tree.record_host_materialization(&child).unwrap();
    tree.record_effect_outcome(
        &child,
        Some(attempt),
        effect_outcomes::Outcome::AttemptAdopted,
    )
    .unwrap();
    let expected = tree.attempt(&child).unwrap().clone();
    assert_eq!(expected.status, AttemptStatus::Adopted);
    tree.clear_feed_views();
    tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(&child).unwrap(), &expected);
    assert!(tree.take_effects().is_empty());
}

#[test]
fn finalized_child_is_not_overwritten_by_a_later_host_decision() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal("start-1", turn(1), End::Completed);
    project(&mut tree, &child, &feed);
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        true,
        terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
    )
    .unwrap();
    tree.record_host_materialization(&child).unwrap();
    tree.fold_due(&child).unwrap();
    assert_eq!(
        tree.attempt(&child).unwrap().status,
        AttemptStatus::CompletedWithoutSubmission
    );
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn retirement_annotations_rebuild_closed_and_completed_attempts() {
    for closed in [false, true] {
        let (_tmp, mut tree) = tree();
        let (child, _) = running_child(&mut tree);
        tree.submit_result(
            &child,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        if closed {
            tree.close_agent("root", &close_req(&child), None).unwrap();
        } else {
            tree.complete_from_intent_for_test(&child).unwrap();
        }
        let status = tree.attempt(&child).unwrap().status;
        tree.record_host_retirement(&child, "release refused".into())
            .unwrap();
        tree.fold_due(&child).unwrap();
        assert_eq!(tree.attempt(&child).unwrap().status, status);
        assert_eq!(
            tree.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let expected = tree.attempt(&child).unwrap().clone();
        tree.clear_feed_views();
        tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.attempt(&child).unwrap(), &expected);
        assert_eq!(
            tree.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }
}

#[test]
fn retirement_annotations_rebuild_after_seal_success_or_failure() {
    for seal_fails in [false, true] {
        let (_tmp, vcs, mut tree) = fake_tree();
        let child = tree
            .spawn_agent("root", &mint_req(), None)
            .unwrap()
            .agent_id;
        tree.complete_from_intent_for_test(&child).unwrap();
        let effects = tree.take_effects();
        assert_eq!(effects.seals.len(), 1);
        tree.record_host_retirement(&child, "release refused".into())
            .unwrap();
        tree.fold_due(&child).unwrap();
        tree.close_agent("root", &close_req(&child), None).unwrap();
        tree.stop_all("root", None).unwrap();
        if seal_fails {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
        }
        tree.complete(effects.run(vcs.as_ref()));
        let expected = tree.attempt(&child).unwrap().clone();
        let node = tree.node(&child).unwrap().status;
        tree.clear_feed_views();
        tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.attempt(&child).unwrap(), &expected);
        assert_eq!(tree.node(&child).unwrap().status, node);
        assert!(tree.take_effects().seals.is_empty());
        let mut restored = tree.to_persisted().rows().clone();
        restored.clear_feed_views();
        restored
            .project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(restored.attempt(&child).unwrap(), &expected);
        assert_eq!(restored.node(&child).unwrap().status, node);
        assert!(restored.take_effects().seals.is_empty());
    }
}

#[test]
fn pending_seal_record_faults_reopen_even_without_its_node_view() {
    let (_tmp, vcs, mut tree) = fake_tree();
    let child = tree
        .spawn_agent("root", &mint_req(), None)
        .unwrap()
        .agent_id;
    tree.complete_from_intent_for_test(&child).unwrap();
    assert_eq!(tree.take_effects().seals.len(), 1);
    let mut rows = tree.to_persisted().rows().clone();
    rows.nodes.get_mut(&child).unwrap().status = NodeStatus::Running;
    let mut reopened = ProductAgentTree::reopen(rows.to_persisted());
    assert_eq!(
        reopened.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert!(
        !reopened.begin_seal(&child),
        "an uncertain seal cannot be retried"
    );
    assert!(reopened.take_effects().seals.is_empty());
    assert!(vcs.calls_of(FakeOp::Seal).is_empty());
}

fn child_command(tree: &mut ProductAgentTree, agent: &str, attempt: &AttemptId, command: &str) {
    tree.record_command(AgentCommand::intent(
        command,
        agent,
        Some(attempt.clone()),
        AgentCommandKind::Start,
        "assignment",
        Vec::new(),
    ))
    .unwrap();
}

#[test]
fn an_accepted_child_command_binds_its_attempt_and_the_terminal_ends_the_turn() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    project(&mut tree, &child, &feed);
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id.as_deref(),
        Some(turn(1).to_string().as_str())
    );
    assert!(tree.node(&child).unwrap().active_turn);
    assert_eq!(
        tree.command("start-1").unwrap().turn_id,
        Some(turn(1).to_string())
    );

    feed.terminal("start-1", turn(1), End::Completed);
    let ended = project(&mut tree, &child, &feed);
    assert_eq!(
        ended.ended_turns,
        vec![(
            child.clone(),
            terminal_evidence(&tree, &child, &feed, &turn(1).to_string()).unwrap()
        )]
    );
    assert!(!tree.node(&child).unwrap().active_turn);
}

#[test]
fn a_queued_followup_binding_does_not_replace_the_live_waiter_owner() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-owned");
    record_child_command(
        &mut tree,
        &child,
        &attempt,
        "followup-owned",
        AgentCommandKind::Followup,
    );
    tree.bind_turn_owner(&child, turn(1).to_string());
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-owned", turn(1));
    feed.accepted("followup-owned", turn(2));
    let pass = project(&mut tree, &child, &feed);
    assert!(pass.conflicts.is_empty());
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id,
        Some(turn(2).to_string())
    );
    assert_eq!(
        tree.owned_turn_id(&child),
        Some(turn(1).to_string().as_str())
    );
    feed.terminal("start-owned", turn(1), End::Completed);
    project(&mut tree, &child, &feed);
    assert!(tree.node(&child).unwrap().active_turn);
    tree.bind_turn_owner(&child, turn(2).to_string());
    feed.terminal("followup-owned", turn(2), End::Completed);
    project(&mut tree, &child, &feed);
    assert_eq!(tree.attempt(&child).unwrap().usage.turns, 2);
    tree.release_turn_owner(&child);
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn terminal_projection_releases_the_view_but_not_the_waiter_owner() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-owned");
    tree.bind_turn_owner(&child, turn(1).to_string());
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-owned", turn(1));
    project(&mut tree, &child, &feed);
    feed.terminal("start-owned", turn(1), End::Completed);
    project(&mut tree, &child, &feed);
    assert!(!tree.node(&child).unwrap().active_turn);
    assert_eq!(tree.attempt(&child).unwrap().turn_id, None);
    assert!(tree.has_turn_owner(&child));
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    assert!(!reopened.has_turn_owner(&child));
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .unwrap();
    assert!(
        !tree.seal_pending(&child),
        "the waiter still owns the mutable copy"
    );
    tree.release_turn_owner(&child);
    assert!(!tree.has_turn_owner(&child));
}

#[test]
fn a_child_command_of_an_earlier_attempt_never_binds_the_current_one() {
    let (_tmp, mut tree) = tree();
    let (child, _attempt) = running_child(&mut tree);
    let stale = AttemptId::from_persisted("a-earlier".to_owned());
    child_command(&mut tree, &child, &stale, "wake-old");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("wake-old", turn(1));
    project(&mut tree, &child, &feed);
    assert_eq!(tree.attempt(&child).unwrap().turn_id, None);
    assert!(!tree.node(&child).unwrap().active_turn);
}

#[test]
fn only_a_feed_acceptance_acknowledges_a_live_batch() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-live", &ids);
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.accepted("cmd-live", turn(1));
    project(&mut tree, "root", &feed);
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Acknowledged);
    let settled = tree.clone();
    project(&mut tree, "root", &feed);
    assert_eq!(rows(&tree), rows(&settled));
    assert!(!tree.withdraw_command("cmd-live"));
}

#[test]
fn a_live_refusal_withdraws_without_fabricating_a_rejection() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-refused", &ids);
    assert!(tree.withdraw_command("cmd-refused"));
    assert!(!tree.withdraw_command("cmd-refused"));
    let command = tree.command("cmd-refused").unwrap();
    assert!(command.withdrawn && command.fact.is_none() && !command.unresolved());
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);
    assert!(tree.record_consumed_command("cmd-refused").is_err());
}

fn consume(tree: &mut ProductAgentTree, command: &str) {
    let recipient = tree.command(command).unwrap().recipient.clone();
    tree.record_consumed_command(command).unwrap();
    tree.project_mail_decisions(&recipient).unwrap();
}

#[test]
fn an_accepted_corrective_round_binds_its_turn_and_spends_its_round() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let corrective = format!("plak:submit-harness:{child}:1:{attempt}:1");
    tree.record_command(AgentCommand::intent(
        corrective.as_str(),
        child.as_str(),
        Some(attempt.clone()),
        AgentCommandKind::Corrective,
        "submit your result",
        Vec::new(),
    ))
    .unwrap();
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal("start-1", turn(1), End::Completed);
    feed.accepted(&corrective, turn(2));
    project(&mut tree, &child, &feed);
    let bound = tree.attempt(&child).unwrap();
    assert_eq!(bound.turn_id.as_deref(), Some(turn(2).to_string().as_str()));
    assert_eq!(bound.submit_harness_nudge.rounds(), 1);
    assert!(tree.node(&child).unwrap().active_turn);
}

#[test]
fn an_accepted_command_never_resumes_an_interrupted_child() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "wake-1");
    // The interrupt intent, as a settled interrupt leaves it.
    tree.nodes.get_mut(&child).unwrap().status = NodeStatus::Interrupted;
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("wake-1", turn(1));
    project(&mut tree, &child, &feed);
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Interrupted);
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id,
        Some(turn(1).to_string())
    );
    assert!(!tree.node(&child).unwrap().active_turn);
    assert_eq!(
        tree.command("wake-1").unwrap().fact,
        Some(CommandFact::Accepted),
        "its batch is still acknowledged: it reached history"
    );
}

#[test]
fn a_second_running_turn_for_one_attempt_fails_closed() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    child_command(&mut tree, &child, &attempt, "wake-2");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.accepted("wake-2", turn(2));
    let projection = project(&mut tree, &child, &feed);
    assert_eq!(
        projection.conflicts,
        vec![BindConflict {
            agent_id: child.clone(),
            command_id: "wake-2".to_owned(),
            bound_turn_id: turn(1).to_string(),
            recovered_turn_id: turn(2).to_string(),
        }]
    );
    assert_eq!(
        tree.attempt(&child).unwrap().turn_id.as_deref(),
        Some(turn(1).to_string().as_str()),
        "the binding is untouched"
    );
    assert_eq!(
        tree.node(&child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

#[test]
fn an_abandoned_child_turn_settles_turn_abandoned_with_the_read_operator_line() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal("start-1", turn(1), End::Abandoned);
    assert_eq!(
        terminal_reads_needed(&feed.records),
        vec![(1, turn(1))],
        "only the abandonment needs its terminal read"
    );
    let mut reads = TreeFeedReads::default();
    reads.insert_terminal(
        turn(1).to_string(),
        TerminalRead {
            message: "operator line".to_owned(),
            abandoned: Some(AbandonedChildTurn {
                operations: vec![7],
                claim: AbandonedTurnClaim::RunningTool,
            }),
        },
    );
    tree.project_feed(&child, CHILD_SESSION, &feed.records, &reads)
        .unwrap();
    let attempt = tree.attempt(&child).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    let failure = attempt.failure.clone().unwrap();
    assert_eq!(failure.kind, TURN_ABANDONED);
    assert!(failure.message.contains("operator line"), "{failure:?}");
    assert_eq!(
        tree.node(&child)
            .unwrap()
            .abandoned_turn
            .as_ref()
            .unwrap()
            .operations,
        vec![7],
        "the caller's face reports the abandonment once"
    );
    // A later process applying the same record settles nothing again.
    let mut later = ProductAgentTree::reopen(tree.to_persisted());
    let again = later
        .project_feed(&child, CHILD_SESSION, &feed.records, &reads)
        .unwrap();
    assert_eq!(again.applied, 0);
}

#[test]
fn a_page_with_a_gap_or_a_foreign_record_changes_nothing() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-1", &ids);
    let mut feed = Feed::new(ROOT_SESSION);
    feed.accepted("cmd-1", turn(1));
    feed.push(Vec::new());
    feed.push(Vec::new());
    let gapped = vec![feed.records[0].clone(), feed.records[2].clone()];
    let refused = tree.project_feed("root", ROOT_SESSION, &gapped, &TreeFeedReads::default());
    assert!(matches!(refused, Err(TreeProjectorError::Gap { .. })));
    assert_eq!(tree.feed_cursor(ROOT_SESSION), 0);
    assert_eq!(durable(&tree, &ids[0]), MailDurableState::Accepted);
    let foreign = tree.project_feed(
        "root",
        CHILD_SESSION,
        &feed.records,
        &TreeFeedReads::default(),
    );
    assert!(matches!(
        foreign,
        Err(TreeProjectorError::ForeignSession { .. })
    ));
}

/// The persisted rows the laws compare: everything but the revision.
#[derive(Clone, Copy, Debug)]
enum GeneratedHostDecision {
    Close,
    Cancelled,
    Materialization,
    Retirement,
    TurnFailure,
    TurnRecovery,
    Finalize,
}

#[test]
fn a_finalize_refused_by_recovery_stays_inert_after_recovery_settles() {
    let cancelled = HostInterleaving {
        decision: GeneratedHostDecision::Cancelled,
        terminal_first: false,
        failed_terminal: false,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: false,
        interrupt: false,
        withdraw_before_start: true,
    };
    let steps = [
        cancelled,
        HostInterleaving {
            decision: GeneratedHostDecision::TurnRecovery,
            late_finalize: true,
            ..cancelled
        },
    ];
    assert_generated_host_interleavings(&steps).unwrap();
}

/// Minimized MAIN-law failure: an old unbound recovery delays a followup's
/// cancellation until its terminal has released the binding. Rebuild loses
/// that cancellation and also changes the historical recovery's end stamp.
#[test]
fn deferred_unbound_recovery_preserves_the_followup_cancellation_on_rebuild() {
    let recovery = HostInterleaving {
        decision: GeneratedHostDecision::TurnRecovery,
        terminal_first: true,
        failed_terminal: true,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: false,
        interrupt: false,
        withdraw_before_start: true,
    };
    let steps = [
        recovery,
        HostInterleaving {
            decision: GeneratedHostDecision::Cancelled,
            terminal_first: false,
            failed_terminal: false,
            ..recovery
        },
    ];
    assert_generated_host_interleavings(&steps).unwrap();
}

#[test]
fn an_unbound_retirement_does_not_delay_a_bound_cancel_to_page_end() {
    let retirement = HostInterleaving {
        decision: GeneratedHostDecision::Retirement,
        terminal_first: true,
        failed_terminal: false,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: false,
        interrupt: false,
        withdraw_before_start: true,
    };
    assert_generated_host_interleavings(&[
        retirement,
        HostInterleaving {
            decision: GeneratedHostDecision::Cancelled,
            terminal_first: false,
            ..retirement
        },
    ])
    .unwrap();
}

#[test]
fn an_adopted_followup_workspace_is_retained_on_rebuild() {
    let adopted = HostInterleaving {
        decision: GeneratedHostDecision::Finalize,
        terminal_first: true,
        failed_terminal: false,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: true,
        interrupt: false,
        withdraw_before_start: true,
    };
    assert_generated_host_interleavings(&[adopted, adopted]).unwrap();
}

#[test]
fn generated_seal_success_failure_and_adoption_rebuild_like_live() {
    let success = HostInterleaving {
        decision: GeneratedHostDecision::Finalize,
        terminal_first: true,
        failed_terminal: false,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: true,
        interrupt: false,
        withdraw_before_start: true,
    };
    assert_generated_host_interleavings(&[
        success,
        success,
        HostInterleaving {
            failed_seal: true,
            adopt: false,
            interrupt: false,
            withdraw_before_start: false,
            ..success
        },
    ])
    .unwrap();
}

#[test]
fn a_failed_runtime_seal_keeps_its_external_fence_during_rebuild() {
    let cancelled = HostInterleaving {
        decision: GeneratedHostDecision::Cancelled,
        terminal_first: false,
        failed_terminal: false,
        nudge: false,
        finalize_first: false,
        late_finalize: false,
        submitted: false,
        failed_seal: false,
        adopt: false,
        interrupt: false,
        withdraw_before_start: true,
    };
    assert_generated_host_interleavings(&[
        cancelled,
        HostInterleaving {
            terminal_first: true,
            failed_terminal: true,
            failed_seal: true,
            ..cancelled
        },
    ])
    .unwrap();
}

#[test]
fn turn_failure_then_retirement_is_folded_once() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut tree, &child, &feed);
    tree.record_host_turn_failure(
        &child,
        FailedTurnDisposition::RecoveryRequired,
        "M1".into(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(&child).unwrap();
    tree.record_host_retirement(&child, "MR".into()).unwrap();
    tree.fold_due(&child).unwrap();
    let _ = tree.take_effects();
    let expected = rows(&tree);
    tree.fold_due(&child).unwrap();
    assert_eq!(rows(&tree), expected);
    assert!(tree.take_effects().is_empty());
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn anchored_intents_replay_across_more_than_sixty_four_pages() {
    let (_tmp, mut live) = tree();
    let (child, attempt) = running_child(&mut live);
    child_command(&mut live, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    for _ in 0..130 {
        feed.history(turn(1), Vec::new());
    }
    project(&mut live, &child, &feed);
    live.record_host_cancellation(&child).unwrap();
    live.fold_due(&child).unwrap();
    feed.terminal("start", turn(1), End::Completed);
    project(&mut live, &child, &feed);
    let _ = live.take_effects();
    let expected = rows(&live);
    let mut rebuilt = live.clone();
    rebuilt.clear_feed_views();
    restore_unallocated_children(&mut rebuilt);
    for page in feed.records.chunks(1) {
        rebuilt
            .project_feed(&child, CHILD_SESSION, page, &TreeFeedReads::default())
            .unwrap();
    }
    assert_eq!(rows(&rebuilt), expected);
    assert!(rebuilt.take_effects().is_empty());
    assert_child_rebuild_laws(&live, &child, &feed).unwrap();
}

#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent generated scheduling and outcome choices"
)]
struct HostInterleaving {
    decision: GeneratedHostDecision,
    terminal_first: bool,
    failed_terminal: bool,
    nudge: bool,
    finalize_first: bool,
    late_finalize: bool,
    submitted: bool,
    failed_seal: bool,
    adopt: bool,
    interrupt: bool,
    withdraw_before_start: bool,
}

fn host_interleaving() -> impl Strategy<Value = HostInterleaving> {
    (
        prop_oneof![
            Just(GeneratedHostDecision::Close),
            Just(GeneratedHostDecision::Cancelled),
            Just(GeneratedHostDecision::Materialization),
            Just(GeneratedHostDecision::Retirement),
            Just(GeneratedHostDecision::TurnFailure),
            Just(GeneratedHostDecision::TurnRecovery),
            Just(GeneratedHostDecision::Finalize),
        ],
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(
                decision,
                terminal_first,
                failed_terminal,
                nudge,
                finalize_first,
                late_finalize,
                submitted,
                failed_seal,
                adopt,
                interrupt,
                withdraw_before_start,
            )| {
                HostInterleaving {
                    decision,
                    terminal_first,
                    failed_terminal,
                    nudge,
                    finalize_first,
                    late_finalize,
                    submitted,
                    failed_seal,
                    adopt,
                    interrupt,
                    withdraw_before_start,
                }
            },
        )
}

fn settle_generated_effects(tree: &mut ProductAgentTree) -> usize {
    let mut seals = 0;
    loop {
        let effects = tree.take_effects();
        if effects.is_empty() {
            return seals;
        }
        seals += effects.seals.len();
        let _ = tree.complete(effects.run(tree.vcs()));
    }
}

fn generated_finalization(
    tree: &mut ProductAgentTree,
    child: &str,
    turn_id: TurnId,
    decision: ChildFinalizationDecision,
    feed: &Feed,
) -> usize {
    tree.record_child_finalization(
        child,
        &turn_id.to_string(),
        decision,
        decision != ChildFinalizationDecision::Nudge,
        (decision == ChildFinalizationDecision::Finalize)
            .then(|| terminal_evidence(tree, child, feed, &turn_id.to_string()))
            .flatten(),
    )
    .unwrap();
    tree.fold_due(child).unwrap();
    settle_generated_effects(tree)
}

#[allow(
    clippy::too_many_lines,
    reason = "generated live interleavings and their rebuild oracle"
)]
fn assert_generated_host_interleavings(steps: &[HostInterleaving]) -> Result<(), TestCaseError> {
    let (_tmp, vcs, mut live) = fake_tree();
    let spawned = live.spawn_agent("root", &mint_req(), None).unwrap();
    let child = spawned.agent_id;
    live.allocate_session_for_test(
        &child,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    let mut feed = Feed::new(CHILD_SESSION);
    let mut seals = 0;
    for (n, step) in steps.iter().enumerate() {
        // A failed seal needs operator inspection before another attempt opens.
        if step.failed_seal && n + 1 == steps.len() {
            vcs.seal_next_with(Err(VcsError::seal_failed("generated seal failure")));
        }
        if n > 0 && step.withdraw_before_start {
            let before = rows(&live);
            live.open_followup_attempt(
                &child,
                "withdraw-before-start",
                live.node(&child).unwrap().output_schema_json.clone(),
                live.node(&child).unwrap().schema_digest.clone(),
            )
            .unwrap();
            live.withdraw_unstarted_followup(
                &child,
                "withdraw-before-start",
                CHILD_SESSION,
                live.feed_cursor(CHILD_SESSION),
            )
            .unwrap();
            prop_assert_eq!(rows(&live), before);
        }
        let attempt = if n == 0 {
            spawned.attempt_id.clone()
        } else {
            live.open_followup_attempt(
                &child,
                &format!("followup-{n}"),
                live.node(&child).unwrap().output_schema_json.clone(),
                live.node(&child).unwrap().schema_digest.clone(),
            )
            .unwrap()
        };
        let command = format!("start-{n}");
        let turn_id = turn(n as u64 + 1);
        child_command(&mut live, &child, &attempt, &command);
        feed.accepted(&command, turn_id);
        project(&mut live, &child, &feed);
        if step.submitted {
            live.submit_result(
                &child,
                &SubmitResultRequest {
                    turn_token: live.attempt(&child).unwrap().turn_token,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
        }
        let finalizing = matches!(step.decision, GeneratedHostDecision::Finalize);
        if step.interrupt && !finalizing {
            live.interrupt_agent("root", &close_req(&child), None)
                .unwrap();
        }
        let terminal_first = step.terminal_first || finalizing;
        let parked = if step.nudge {
            ChildFinalizationDecision::Nudge
        } else {
            ChildFinalizationDecision::Park
        };
        if terminal_first {
            feed.terminal(
                &command,
                turn_id,
                if step.failed_terminal {
                    End::Internal
                } else {
                    End::Completed
                },
            );
            project(&mut live, &child, &feed);
            seals += settle_generated_effects(&mut live);
            if finalizing && step.finalize_first {
                seals += generated_finalization(
                    &mut live,
                    &child,
                    turn_id,
                    ChildFinalizationDecision::Finalize,
                    &feed,
                );
            }
            seals += generated_finalization(&mut live, &child, turn_id, parked, &feed);
        }
        let recorded = match step.decision {
            GeneratedHostDecision::Close => live
                .close_agent("root", &close_req(&child), None)
                .map(|_| ()),
            GeneratedHostDecision::Cancelled => live.record_host_cancellation(&child),
            GeneratedHostDecision::Materialization => live.record_host_materialization(&child),
            GeneratedHostDecision::Retirement => {
                live.record_host_retirement(&child, "generated retirement".into())
            }
            GeneratedHostDecision::TurnFailure | GeneratedHostDecision::TurnRecovery => {
                let disposition = if matches!(step.decision, GeneratedHostDecision::TurnRecovery) {
                    FailedTurnDisposition::RecoveryRequired
                } else {
                    FailedTurnDisposition::Failed {
                        kind: "turn_failed",
                    }
                };
                live.record_host_turn_failure(
                    &child,
                    disposition,
                    "generated host failure".into(),
                    Vec::new(),
                )
            }
            GeneratedHostDecision::Finalize => {
                seals += generated_finalization(
                    &mut live,
                    &child,
                    turn_id,
                    ChildFinalizationDecision::Finalize,
                    &feed,
                );
                Ok(())
            }
        };
        if let Err(error) = recorded {
            prop_assert_eq!(error.code, TopologyErrorCode::RecipientRecoveryRequired);
            prop_assert!(step.submitted);
            prop_assert_eq!(
                live.node(&child).unwrap().status,
                NodeStatus::RecoveryRequired
            );
        }
        live.fold_due(&child).unwrap();
        seals += settle_generated_effects(&mut live);
        if !terminal_first {
            feed.terminal(
                &command,
                turn_id,
                if step.failed_terminal {
                    End::Internal
                } else {
                    End::Completed
                },
            );
            project(&mut live, &child, &feed);
            seals += settle_generated_effects(&mut live);
            seals += generated_finalization(&mut live, &child, turn_id, parked, &feed);
        }
        if step.late_finalize || matches!(step.decision, GeneratedHostDecision::Close) {
            seals += generated_finalization(
                &mut live,
                &child,
                turn_id,
                ChildFinalizationDecision::Finalize,
                &feed,
            );
        }
        if live.node(&child).unwrap().status == NodeStatus::RecoveryRequired
            && live.attempt(&child).unwrap().session_outcome_unresolved()
        {
            live.record_host_recovery(
                &child,
                &attempt,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "generated exact-open release".into(),
                },
            )
            .unwrap();
            live.fold_due(&child).unwrap();
            seals += settle_generated_effects(&mut live);
        }
        if step.adopt
            && let Some(candidate_id) = live.attempt(&child).unwrap().candidate_id.clone()
        {
            live.begin_adopt(
                "root",
                &AdoptCandidateRequest {
                    candidate_id,
                    expected_target: None,
                    evidence: None,
                },
            )
            .unwrap();
            seals += settle_generated_effects(&mut live);
        }
        prop_assert!(
            live.attempt(&child).unwrap().status.is_stopped(),
            "{step:?}"
        );
    }
    prop_assert!(
        seals > 0,
        "the fork workspace must exercise actual seal reservations"
    );
    prop_assert_eq!(live.attempts.len(), steps.len());
    assert_child_rebuild_laws(&live, &child, &feed)?;
    let mut rebuilt = ProductAgentTree::reopen(live.to_persisted());
    rebuilt.clear_feed_views();
    project(&mut rebuilt, &child, &feed);
    prop_assert!(
        rebuilt.take_effects().is_empty(),
        "rebuild must not redispatch any effect"
    );
    Ok(())
}

fn rows(tree: &ProductAgentTree) -> String {
    // The driver lifts a quiescent restored overlay after the fold, not inside it.
    let mut tree = tree.clone();
    tree.lift_restored_pause_if_quiescent();
    let persisted = tree.to_persisted();
    let rows = persisted.rows();
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
        rows.nodes, rows.attempts, rows.messages, rows.operations, rows.commands, rows.feed_cursors
    )
}

/// One root admission in a generated scenario.
#[derive(Clone, Copy, Debug)]
enum Admission {
    /// Accepted; its turn ends with this outcome.
    Accepted(End),
    /// Rejected.
    Rejected,
    /// Never recorded: the product discards it at open.
    Discarded,
}

fn admission() -> impl Strategy<Value = Admission> {
    prop_oneof![
        Just(Admission::Accepted(End::Completed)),
        Just(Admission::Accepted(End::Withheld)),
        Just(Admission::Accepted(End::Internal)),
        Just(Admission::Rejected),
        Just(Admission::Discarded),
    ]
}

/// Runs a scenario the way a live root does: each admission selects the
/// unread result mail, records its batch, and its facts reach the tree one
/// record at a time. Returns the tree, the feed and the incremental rows.
fn live_run(admissions: &[Admission], behind: bool) -> (tempfile::TempDir, ProductAgentTree, Feed) {
    let (tmp, mut tree, _) = root_with_results(3);
    let mut feed = Feed::new(ROOT_SESSION);
    if behind {
        feed.offset = 8;
        let mail = tree.pending_result_mail("root")[0].message_id.clone();
        batch(&mut tree, "pruned-consumed", &[mail]);
        tree.mark_commands_behind_retention("root").unwrap();
        tree.fold_onto_seed("root", feed.offset).unwrap();
        consume(&mut tree, "pruned-consumed");
    }
    for (n, admission) in admissions.iter().enumerate() {
        let command = format!("cmd-{n}");
        let unread: Vec<String> = tree
            .pending_result_mail("root")
            .into_iter()
            .map(|mail| mail.message_id)
            .collect();
        batch(&mut tree, &command, &unread);
        let turn_id = turn(n as u64);
        match admission {
            Admission::Accepted(end) => {
                feed.accepted(&command, turn_id);
                project(&mut tree, "root", &feed);
                feed.terminal(&command, turn_id, *end);
            }
            Admission::Rejected => feed.rejected(&command),
            Admission::Discarded => {
                tree.join_pending_root_commands(&[]);
            }
        }
        project(&mut tree, "root", &feed);
    }
    (tmp, tree, feed)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Idempotence and rebuild == incremental: the whole feed as one page,
    /// the same page again, and a delete-and-rebuild of every view from the
    /// feed all land on the rows the live, record-at-a-time run left.
    #[test]
    fn the_tree_projector_is_idempotent_and_rebuilds_its_views(
        admissions in proptest::collection::vec(admission(), 1..8),
        behind in any::<bool>(),
        host_steps in proptest::collection::vec(host_interleaving(), 2..5),
    ) {
        let (_tmp, live, feed) = live_run(&admissions, behind);
        let expected = rows(&live);

        let mut again = live.clone();
        let replay = project(&mut again, "root", &feed);
        prop_assert_eq!(replay.applied, 0);
        prop_assert_eq!(rows(&again), expected.clone());

        let mut rebuilt = ProductAgentTree::reopen(live.to_persisted());
        rebuilt.clear_feed_views(); restore_unallocated_children(&mut rebuilt);
        if behind {
            rebuilt.mark_commands_behind_retention("root").unwrap();
            rebuilt.fold_onto_seed("root", feed.offset).unwrap();
            rebuilt.project_mail_decisions("root").unwrap();
        }
        let pass = project(&mut rebuilt, "root", &feed);
        prop_assert_eq!(pass.applied, feed.records.len());
        prop_assert_eq!(rows(&rebuilt), expected.clone());

        // Delete-and-reopen: the cleared rows survive a store round trip and
        // the reopened tree rebuilds the same views in pages of one.
        let mut cleared = live.clone();
        cleared.clear_feed_views(); restore_unallocated_children(&mut cleared);
        let mut reopened = ProductAgentTree::reopen(cleared.to_persisted());
        if behind {
            reopened.mark_commands_behind_retention("root").unwrap();
            reopened.fold_onto_seed("root", feed.offset).unwrap();
            reopened.project_mail_decisions("root").unwrap();
        }
        for end in 0..=feed.records.len() {
            reopened
                .project_feed("root", ROOT_SESSION, &feed.records[..end], &TreeFeedReads::default())
                .unwrap();
        }
        prop_assert_eq!(rows(&reopened), expected);
        assert_generated_host_interleavings(&host_steps)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn host_decisions_are_inert_and_rebuild_without_repeating_effects(
        decision in 0_u8..3,
        submitted in any::<bool>(),
    ) {
        let (_tmp, mut tree) = tree();
        let (child, attempt) = running_child(&mut tree);
        child_command(&mut tree, &child, &attempt, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        project(&mut tree, &child, &feed);
        if submitted {
            let attempt = tree.node(&child).unwrap().attempt_id.clone();
            tree.record_operator_intent(&child, attempt,
                operator_intents::Decision::Submitted { output_json: r#"{"done":true}"#.to_owned() }).unwrap();
        }
        let before = tree.attempt(&child).unwrap().clone();
        tree.record_fault(&child, "host decision diagnostic");
        match decision {
            0 => tree.record_host_cancellation(&child).unwrap(),
            1 => tree.record_host_retirement(&child, "release uncertain".into()).unwrap(),
            _ => tree.record_host_materialization(&child).unwrap(),
        }
        prop_assert_eq!(tree.attempt(&child).unwrap(), &before);
        tree.fold_due(&child).unwrap();
        let expected = tree.attempt(&child).unwrap().clone();
        let _ = tree.take_effects();
        tree.fold_due(&child).unwrap();
        prop_assert_eq!(tree.attempt(&child).unwrap(), &expected);
        prop_assert!(tree.take_effects().seals.is_empty());
        assert_child_rebuild_laws(&tree, &child, &feed)?;
        let mut rebuilt = ProductAgentTree::reopen(tree.to_persisted());
        rebuilt.clear_feed_views(); restore_unallocated_children(&mut rebuilt);
        project(&mut rebuilt, &child, &feed);
        prop_assert!(rebuilt.take_effects().seals.is_empty());
        prop_assert_eq!(rebuilt.attempt(&child).unwrap().ended_at, expected.ended_at);
        prop_assert_eq!(&rebuilt.attempt(&child).unwrap().failure, &expected.failure);
    }

    #[test]
    fn quiescent_wait_reads_join_retained_terminals_idempotently(
        committed in any::<bool>(),
        late in any::<bool>(),
        foreign in any::<bool>(),
    ) {
        let (_tmp, mut base, ids) = root_with_results(1);
        let mut feed = Feed::new(ROOT_SESSION);
        feed.accepted("start", turn(1));
        if late { project(&mut base, "root", &feed); }
        wait_receipt(&mut base, "root", call(1), &ids);
        // The call's HistoryAppended record is not retained. Its terminal is.
        feed.terminal("start", turn(1), End::Internal);
        let header = feed.records.last().unwrap().header();
        let mut head = super::record_locator(header).unwrap();
        if foreign {
            head = plakous::sessions::SessionLocator::new(CHILD_SESSION.parse().unwrap(), head.branch_id(), head.checkpoint_id());
        }
        let mut reads = TreeFeedReads::default();
        reads.insert_waits(head, vec![(call(1).to_string(), committed)]);
        let mut live = base.clone();
        live.project_feed("root", ROOT_SESSION, &feed.records, &reads).unwrap();
        prop_assert!(live.retained_wait_receipts("root").is_empty());
        prop_assert_eq!(durable(&live, &ids[0]), if committed && !foreign { MailDurableState::Acknowledged } else { MailDurableState::Accepted });
        let expected = rows(&live);
        live.project_feed("root", ROOT_SESSION, &feed.records, &reads).unwrap();
        prop_assert_eq!(rows(&live), expected.clone());
        live.clear_feed_views(); restore_unallocated_children(&mut live);
        let mut restored = ProductAgentTree::reopen(live.to_persisted());
        restored.project_feed("root", ROOT_SESSION, &feed.records, &reads).unwrap();
        prop_assert_eq!(rows(&restored), expected);
        base.clear_feed_views(); restore_unallocated_children(&mut base);
        base.project_feed("root", ROOT_SESSION, &feed.records, &reads).unwrap();
        prop_assert_eq!(durable(&base, &ids[0]), durable(&restored, &ids[0]));
    }

    #[test]
    fn root_wait_reads_join_exact_retention_heads(committed in any::<bool>()) {
        let (_tmp, mut tree, ids) = root_with_results(1);
        wait_receipt(&mut tree, "root", call(1), &ids);
        let head = plakous::sessions::SessionLocator::new(ROOT_SESSION.parse().unwrap(), uuid(0xb).parse().unwrap(), uuid(0x3001).parse().unwrap());
        let mut reads = TreeFeedReads::default();
        reads.insert_waits(head, vec![(call(1).to_string(), committed)]);
        let feed = TreeFeed { session_id: ROOT_SESSION.to_owned(), retained_from: 11, below_retained: Some(10), heads: vec![head],
            head_turns: BTreeMap::new(), records: Vec::new(), reads };
        feed.apply(&mut tree, "root").unwrap();
        prop_assert_eq!(durable(&tree, &ids[0]), if committed { MailDurableState::Acknowledged } else { MailDurableState::Accepted });
        let expected = rows(&tree);
        feed.apply(&mut tree, "root").unwrap();
        prop_assert_eq!(rows(&tree), expected.clone());
        tree.clear_feed_views(); restore_unallocated_children(&mut tree);
        let mut restored = ProductAgentTree::reopen(tree.to_persisted());
        feed.apply(&mut restored, "root").unwrap();
        prop_assert_eq!(rows(&restored), expected);
    }

    #[test]
    fn host_recovery_decisions_are_inert_and_rebuild_without_new_effects(never_created in any::<bool>(), retirement in any::<bool>()) {
        let (_tmp, mut tree) = tree();
        let (child, attempt) = running_child(&mut tree);
        child_command(&mut tree, &child, &attempt, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        project(&mut tree, &child, &feed);
        if retirement {
            tree.record_host_retirement(&child, "ownership uncertainty".into()).unwrap();
            tree.fold_due(&child).unwrap();
        } else {
            tree.record_host_turn_failure(&child, FailedTurnDisposition::RecoveryRequired, "ownership uncertainty".into(), Vec::new()).unwrap();
            tree.fold_due(&child).unwrap();
        }
        let before = tree.attempt(&child).unwrap().clone();
        let resolution = if never_created { RecoveryResolution::NeverCreated { message: "create refused".into() } }
            else { RecoveryResolution::RecoveredWithoutTurn { message: "released quiescent".into() } };
        prop_assert!(tree.record_host_recovery(&child, &attempt, resolution).unwrap());
        prop_assert_eq!(tree.attempt(&child).unwrap(), &before);
        tree.fold_due(&child).unwrap();
        let expected = rows(&tree);
        tree.fold_due(&child).unwrap();
        prop_assert_eq!(rows(&tree), expected.clone());
        let repeated = tree.record_host_recovery(&child, &attempt, RecoveryResolution::TurnAbandoned { message: "later observation".into() }).unwrap();
        prop_assert!(!repeated);
        assert_child_rebuild_laws(&tree, &child, &feed)?;
        let mut rebuilt = ProductAgentTree::reopen(tree.to_persisted());
        rebuilt.clear_feed_views(); restore_unallocated_children(&mut rebuilt);
        project(&mut rebuilt, &child, &feed);
        prop_assert!(rebuilt.take_effects().seals.is_empty());
        prop_assert!(rebuilt.take_effects().unmints.is_empty());
        prop_assert_eq!(rows(&rebuilt), expected);
    }
}

/// How one child turn in a generated scenario ends.
#[derive(Clone, Copy, Debug)]
enum ChildEnd {
    /// Its `TurnTerminal` completes it.
    Completed,
    /// It completes and the child parks, releasing the binding.
    Parked,
    /// No terminal yet; later commands steer into it.
    Open,
}

/// One child command in a generated scenario.
#[derive(Clone, Copy, Debug)]
enum ChildStep {
    /// A command that starts a turn: a corrective round when `corrective`
    /// and rounds remain, a wake otherwise.
    Turn {
        corrective: bool,
        end: ChildEnd,
    },
    /// A command the runtime admits into the open turn, or a wake when none
    /// is open.
    Steer,
    /// A header for another branch of the same runtime session.
    OtherBranch,
    /// A release header carried back across the neutral live-child port.
    Release,
    Ownership,
}

fn child_step() -> impl Strategy<Value = ChildStep> {
    prop_oneof![
        Just(ChildStep::Turn {
            corrective: false,
            end: ChildEnd::Completed
        }),
        Just(ChildStep::Turn {
            corrective: false,
            end: ChildEnd::Parked
        }),
        Just(ChildStep::Turn {
            corrective: false,
            end: ChildEnd::Open
        }),
        Just(ChildStep::Turn {
            corrective: true,
            end: ChildEnd::Completed
        }),
        Just(ChildStep::Turn {
            corrective: true,
            end: ChildEnd::Open
        }),
        Just(ChildStep::Steer),
        Just(ChildStep::OtherBranch),
        Just(ChildStep::Release),
        Just(ChildStep::Ownership),
    ]
}

fn record_child_command(
    tree: &mut ProductAgentTree,
    agent: &str,
    attempt: &AttemptId,
    command: &str,
    kind: AgentCommandKind,
) {
    tree.record_command(AgentCommand::intent(
        command,
        agent,
        Some(attempt.clone()),
        kind,
        format!("text {command}"),
        Vec::new(),
    ))
    .unwrap();
}

/// Runs a child scenario the way a live host does: the first command is
/// the attempt's Start, each command is written before its fact reaches the
/// tree, and every record is applied as it commits. Parking only releases
/// process-local ownership. Returns the child and its feed.
fn live_child_run(steps: &[ChildStep]) -> (tempfile::TempDir, ProductAgentTree, String, Feed) {
    let (tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    tree.nodes.get_mut(&child).unwrap().session_locator = None;
    let locator = plakous::sessions::SessionLocator::new(
        CHILD_SESSION.parse().unwrap(),
        uuid(0xb).parse().unwrap(),
        uuid(0x3001).parse().unwrap(),
    );
    tree.record_child_session(&child, &locator.to_string())
        .unwrap();
    let mut feed = Feed::new(CHILD_SESSION);
    project(&mut tree, &child, &feed);
    let mut open: Option<(String, TurnId)> = None;
    let mut rounds = 0u8;
    let mut next_turn = 0u64;
    for (n, step) in steps.iter().enumerate() {
        let (corrective, end) = match *step {
            ChildStep::Ownership => {
                let before = rows(&tree);
                tree.reserve_turn_owner(&child);
                assert_eq!(rows(&tree), before);
                tree.release_turn_owner(&child);
                continue;
            }
            ChildStep::Release => {
                feed.push(Vec::new());
                let last = feed.records.last_mut().unwrap();
                let mut header = *last.header();
                header.change = plakous::sessions::feed::FeedChange::LeaseReleased {
                    head: uuid(0x3000 + header.seq.get()).parse().unwrap(),
                    fence: header.seq.get(),
                };
                *last = FeedRecord::from_store(header, Vec::new());
                TreeFeed {
                    session_id: feed.session.to_owned(),
                    retained_from: 1,
                    below_retained: None,
                    heads: Vec::new(),
                    head_turns: BTreeMap::default(),
                    records: feed.records.clone(),
                    reads: TreeFeedReads::default(),
                }
                .apply(&mut tree, &child)
                .unwrap();
                continue;
            }
            ChildStep::OtherBranch => {
                feed.push(Vec::new());
                let last = feed.records.last_mut().unwrap();
                let mut header = *last.header();
                header.branch_id = uuid(0xc).parse().unwrap();
                *last = FeedRecord::from_store(header, Vec::new());
                project(&mut tree, &child, &feed);
                continue;
            }
            ChildStep::Steer => {
                if let Some((_, turn_id)) = open {
                    let command = format!("steer-{n}");
                    record_child_command(
                        &mut tree,
                        &child,
                        &attempt,
                        &command,
                        AgentCommandKind::MailStart,
                    );
                    feed.accepted(&command, turn_id);
                    project(&mut tree, &child, &feed);
                    continue;
                }
                (false, ChildEnd::Completed)
            }
            ChildStep::Turn { corrective, end } => (corrective, end),
        };
        if let Some((command, turn_id)) = open.take() {
            feed.terminal(&command, turn_id, End::Completed);
            project(&mut tree, &child, &feed);
        }
        let (command, kind) = if n == 0 {
            (format!("start-{n}"), AgentCommandKind::Start)
        } else if corrective && rounds < 2 {
            rounds += 1;
            (
                format!("plak:submit-harness:{child}:1:{attempt}:{rounds}"),
                AgentCommandKind::Corrective,
            )
        } else {
            (format!("wake-{n}"), AgentCommandKind::Wake)
        };
        record_child_command(&mut tree, &child, &attempt, &command, kind);
        next_turn += 1;
        let turn_id = turn(next_turn);
        feed.accepted(&command, turn_id);
        project(&mut tree, &child, &feed);
        match end {
            ChildEnd::Open => open = Some((command, turn_id)),
            ChildEnd::Completed | ChildEnd::Parked => {
                feed.terminal(&command, turn_id, End::Completed);
                project(&mut tree, &child, &feed);
                if matches!(end, ChildEnd::Parked) {
                    tree.park_child(&child, Vec::new()).unwrap();
                }
            }
        }
    }
    (tmp, tree, child, feed)
}

/// The rebuild laws for one child's feed (spec 05).
fn assert_child_rebuild_laws(
    live: &ProductAgentTree,
    child: &str,
    feed: &Feed,
) -> Result<(), TestCaseError> {
    let expected = rows(live);

    let mut again = live.clone();
    let replay = project(&mut again, child, feed);
    prop_assert_eq!(replay.applied, 0);
    prop_assert_eq!(rows(&again), expected.clone());

    let mut rebuilt = live.clone();
    rebuilt.clear_feed_views();
    restore_unallocated_children(&mut rebuilt);
    let pass = project(&mut rebuilt, child, feed);
    prop_assert_eq!(pass.applied, feed.records.len());
    prop_assert!(pass.conflicts.is_empty(), "{:?}", pass.conflicts);
    prop_assert_eq!(rows(&rebuilt), expected.clone());
    prop_assert_eq!(&rebuilt.workspaces, &live.workspaces);

    // Re-applying the rebuilt feed is idempotent too.
    let replay = project(&mut rebuilt, child, feed);
    prop_assert_eq!(replay.applied, 0);
    prop_assert_eq!(rows(&rebuilt), expected.clone());

    // A reopen pauses the tree and clears its active-turn flags whether or
    // not the views were deleted first.
    let expected = rows(&ProductAgentTree::reopen(live.to_persisted()));
    let mut cleared = live.clone();
    cleared.clear_feed_views();
    restore_unallocated_children(&mut cleared);
    let mut reopened = ProductAgentTree::reopen(cleared.to_persisted());
    for end in 0..=feed.records.len() {
        reopened
            .project_feed(
                child,
                CHILD_SESSION,
                &feed.records[..end],
                &TreeFeedReads::default(),
            )
            .unwrap();
    }
    prop_assert_eq!(rows(&reopened), expected);
    Ok(())
}

#[test]
fn a_rebuild_never_rebinds_an_attempt_to_an_older_turn() {
    // Two parked wakes and a corrective round still running: a replay that
    // re-bound each command in feed order would count three extra turns,
    // and the first command would find the open turn bound and fail closed.
    let steps = [
        ChildStep::Turn {
            corrective: false,
            end: ChildEnd::Parked,
        },
        ChildStep::Turn {
            corrective: false,
            end: ChildEnd::Parked,
        },
        ChildStep::Turn {
            corrective: true,
            end: ChildEnd::Open,
        },
        ChildStep::Steer,
    ];
    let (_tmp, live, child, feed) = live_child_run(&steps);
    let attempt = live.attempt(&child).unwrap();
    assert_eq!(
        attempt.turn_id.as_deref(),
        Some(turn(3).to_string().as_str())
    );
    assert_eq!(attempt.usage.turns, 3);
    assert_eq!(attempt.submit_harness_nudge.rounds(), 1);
    assert!(live.node(&child).unwrap().active_turn);
    assert_child_rebuild_laws(&live, &child, &feed).unwrap();
}

#[test]
fn child_locators_follow_only_their_session_and_branch_in_feed_order() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let initial = plakous::sessions::SessionLocator::new(
        CHILD_SESSION.parse().unwrap(),
        uuid(0xb).parse().unwrap(),
        uuid(0x3001).parse().unwrap(),
    );
    tree.record_child_session(&child.agent_id, &initial.to_string())
        .unwrap();
    assert!(
        tree.node(&child.agent_id)
            .unwrap()
            .session_locator
            .is_none()
    );
    let mut feed = Feed::new(CHILD_SESSION);
    feed.push(Vec::new());
    feed.push(Vec::new());
    project(&mut tree, &child.agent_id, &feed);
    let expected = plakous::sessions::SessionLocator::new(
        initial.session_id(),
        initial.branch_id(),
        uuid(0x3002).parse().unwrap(),
    );
    assert_eq!(
        tree.node(&child.agent_id)
            .unwrap()
            .session_locator
            .as_deref(),
        Some(expected.to_string().as_str())
    );
    let before = rows(&tree);
    project(&mut tree, &child.agent_id, &feed);
    assert_eq!(rows(&tree), before);
    tree.clear_feed_views();
    restore_unallocated_children(&mut tree);
    assert!(
        tree.node(&child.agent_id)
            .unwrap()
            .session_locator
            .is_none()
    );
    project(&mut tree, &child.agent_id, &feed);
    assert_eq!(rows(&tree), before);

    let mut other = Feed::new(CHILD_SESSION);
    other.offset = 2;
    other.push(Vec::new());
    let mut header = *other.records[0].header();
    header.branch_id = uuid(0xc).parse().unwrap();
    other.records[0] = FeedRecord::from_store(header, Vec::new());
    project(&mut tree, &child.agent_id, &other);
    assert_eq!(
        tree.node(&child.agent_id)
            .unwrap()
            .session_locator
            .as_deref(),
        Some(expected.to_string().as_str())
    );
}

#[test]
fn child_session_allocation_is_immutable_and_survives_tree_reopen() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let initial = plakous::sessions::SessionLocator::new(
        CHILD_SESSION.parse().unwrap(),
        uuid(0xb).parse().unwrap(),
        uuid(0x3001).parse().unwrap(),
    );
    tree.record_child_session(&child.agent_id, &initial.to_string())
        .unwrap();
    let changed_head = plakous::sessions::SessionLocator::new(
        initial.session_id(),
        initial.branch_id(),
        uuid(0x3002).parse().unwrap(),
    );
    tree.record_child_session(&child.agent_id, &changed_head.to_string())
        .unwrap();
    let other_branch = plakous::sessions::SessionLocator::new(
        initial.session_id(),
        uuid(0xc).parse().unwrap(),
        initial.checkpoint_id(),
    );
    let before = rows(&tree);
    assert_eq!(
        tree.record_child_session(&child.agent_id, &other_branch.to_string())
            .unwrap_err()
            .code,
        TopologyErrorCode::OperationRecoveryRequired
    );
    assert_eq!(rows(&tree), before);
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    reopened
        .project_feed(
            &child.agent_id,
            CHILD_SESSION,
            &[],
            &TreeFeedReads::default(),
        )
        .unwrap();
    assert_eq!(
        reopened
            .node(&child.agent_id)
            .unwrap()
            .session_locator
            .as_deref(),
        Some(initial.to_string().as_str())
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The same laws over a child's commands: several per attempt, steers
    /// into an open turn, parked wakes and corrective rounds.
    #[test]
    fn the_tree_projector_rebuilds_child_bindings_in_feed_order(
        steps in proptest::collection::vec(child_step(), 1..10),
        host_steps in proptest::collection::vec(host_interleaving(), 2..5),
    ) {
        let (_tmp, live, child, feed) = live_child_run(&steps);
        assert_child_rebuild_laws(&live, &child, &feed)?;
        assert_generated_host_interleavings(&host_steps)?;
    }
}

#[test]
fn a_child_terminal_folds_its_usage_onto_the_attempt_its_command_served() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    record_child_command(
        &mut tree,
        &child,
        &attempt,
        "wake-2",
        AgentCommandKind::Wake,
    );
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal_with("start-1", turn(1), End::Completed, turn_usage(2, 900, 120));
    feed.accepted("wake-2", turn(2));
    feed.terminal_with("wake-2", turn(2), End::Internal, turn_usage(3, 100, 30));
    for end in 1..=feed.records.len() {
        tree.project_feed(
            &child,
            CHILD_SESSION,
            &feed.records[..end],
            &TreeFeedReads::default(),
        )
        .unwrap();
    }
    // What the live turn waiter folded from each `TurnResult` before the
    // projector owned it: the turn's local tool attempts and its model
    // tokens, summed; each bound turn counted once.
    let usage = tree.attempt(&child).unwrap().usage;
    assert_eq!(usage.turns, 2);
    assert_eq!(usage.tool_calls, 5);
    assert_eq!(usage.input_tokens, 1_000);
    assert_eq!(usage.output_tokens, 150);

    let mut rebuilt = tree.clone();
    rebuilt.clear_feed_views();
    restore_unallocated_children(&mut rebuilt);
    assert_eq!(rebuilt.attempt(&child).unwrap().usage.tool_calls, 0);
    project(&mut rebuilt, &child, &feed);
    assert_eq!(rows(&rebuilt), rows(&tree));
    project(&mut rebuilt, &child, &feed);
    assert_eq!(rows(&rebuilt), rows(&tree), "a second pass folds nothing");
}

#[test]
fn a_turn_of_an_earlier_attempt_folds_onto_that_attempt_and_the_root_folds_nothing() {
    let (_tmp, mut tree, ids) = root_with_results(1);
    batch(&mut tree, "cmd-root", &ids);
    let mut root_feed = Feed::new(ROOT_SESSION);
    root_feed.accepted("cmd-root", turn(9));
    root_feed.terminal("cmd-root", turn(9), End::Completed);
    let before = tree.attempts.clone();
    project(&mut tree, "root", &root_feed);
    assert_eq!(tree.attempts, before, "root turns carry no attempt usage");

    let (child, attempt) = running_child(&mut tree);
    let earlier = AttemptId::from_persisted("a-earlier".to_owned());
    tree.attempts.insert(earlier.clone(), {
        let mut record = tree.attempt(&child).unwrap().clone();
        record.attempt_id = earlier.clone();
        record
    });
    record_child_command(
        &mut tree,
        &child,
        &earlier,
        "wake-old",
        AgentCommandKind::Wake,
    );
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("wake-old", turn(1));
    feed.terminal_with("wake-old", turn(1), End::Completed, turn_usage(4, 40, 4));
    project(&mut tree, &child, &feed);
    assert_eq!(tree.attempts[&earlier].usage.tool_calls, 4);
    assert_eq!(tree.attempt(&child).unwrap().usage.tool_calls, 0);
}

/// A failed child turn whose terminal reads `retryability`, after a host
/// notice for it: the live run applies the feed page by page.
fn failed_child_run(
    model_kind: plakous::model::ModelErrorKind,
    retryability: Option<plakous::model::ModelRetryability>,
) -> (
    tempfile::TempDir,
    ProductAgentTree,
    ProductAgentTree,
    String,
    Feed,
    TreeFeedReads,
) {
    let (tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    tree.record_failed_turn_notice(
        &child,
        &attempt,
        &turn(1).to_string(),
        "; failed worktree snapshot: refs/plak/x (commit c0ffee)".to_owned(),
    )
    .unwrap();
    let before = tree.clone();
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal("start-1", turn(1), End::Model(model_kind));
    assert_eq!(terminal_reads_needed(&feed.records), vec![(1, turn(1))]);
    let mut reads = TreeFeedReads::default();
    reads.insert_failure(
        turn(1).to_string(),
        FailureRead {
            kind: AgentErrorKind::Model,
            retryability,
            phase: Some(plakous::model::ModelErrorPhase::Connect),
            http_status: Some(plakous::model::ModelHttpStatus::new(503).unwrap()),
            provider_code: Some("overloaded_error".to_owned()),
            message: "provider said no".to_owned(),
        },
    );
    for end in 1..=feed.records.len() {
        tree.project_feed(&child, CHILD_SESSION, &feed.records[..end], &reads)
            .unwrap();
    }
    (tmp, tree, before, child, feed, reads)
}

/// The rebuild law for failure rows: the same intents projected in one pass,
/// a delete-and-rebuild, and a second pass all derive the live rows.
fn assert_failure_rebuild_laws(
    tree: &ProductAgentTree,
    before: &ProductAgentTree,
    child: &str,
    feed: &Feed,
    reads: &TreeFeedReads,
) {
    let mut fresh = before.clone();
    fresh
        .project_feed(child, CHILD_SESSION, &feed.records, reads)
        .unwrap();
    assert_eq!(rows(&fresh), rows(tree), "one pass derives the live rows");
    let mut rebuilt = tree.clone();
    rebuilt.clear_feed_views();
    restore_unallocated_children(&mut rebuilt);
    rebuilt
        .project_feed(child, CHILD_SESSION, &feed.records, reads)
        .unwrap();
    assert_eq!(
        rows(&rebuilt),
        rows(tree),
        "a rebuild derives the live rows"
    );
    rebuilt
        .project_feed(child, CHILD_SESSION, &feed.records, reads)
        .unwrap();
    assert_eq!(rows(&rebuilt), rows(tree), "a second pass changes nothing");
}

#[test]
fn a_failed_child_terminal_ends_the_attempt_with_its_read_and_the_host_notice() {
    let (_tmp, tree, before, child, feed, reads) =
        failed_child_run(plakous::model::ModelErrorKind::ContextLimit, None);
    let attempt = tree.attempt(&child).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(attempt.ended_at, Some(2_000), "the terminal's commit");
    let failure = attempt.failure.clone().unwrap();
    assert_eq!(failure.kind, "context_exhausted");
    assert_eq!(
        failure.message,
        "child turn failed: Model at Connect (HTTP 503 overloaded_error): provider said no; respawn with a handoff of completed \
         work and the remaining task, not a follow-up into the same session; failed worktree \
         snapshot: refs/plak/x (commit c0ffee)"
    );
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Failed);
    assert!(!tree.node(&child).unwrap().active_turn);
    assert_failure_rebuild_laws(&tree, &before, &child, &feed, &reads);
}

#[test]
fn a_live_waiter_holds_a_failed_terminal_until_its_notice_is_durable() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal(
        "start-1",
        turn(1),
        End::Model(plakous::model::ModelErrorKind::Protocol),
    );
    tree.enter_turn_waiter(&child);
    let first = tree
        .project_feed(
            &child,
            CHILD_SESSION,
            &feed.records,
            &TreeFeedReads::default(),
        )
        .unwrap();
    assert_eq!(first.applied, 1);
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Running);
    let held = rows(&tree);
    tree.project_feed(
        &child,
        CHILD_SESSION,
        &feed.records,
        &TreeFeedReads::default(),
    )
    .unwrap();
    assert_eq!(rows(&tree), held);
    tree.record_failed_turn_notice(
        &child,
        &attempt,
        &turn(1).to_string(),
        "; captured snapshot".into(),
    )
    .unwrap();
    let ready = tree.clone();
    let projected = tree
        .project_feed(
            &child,
            CHILD_SESSION,
            &feed.records,
            &TreeFeedReads::default(),
        )
        .unwrap();
    assert_eq!(projected.applied, 1);
    assert!(
        tree.attempt(&child)
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .message
            .ends_with("; captured snapshot")
    );
    assert_failure_rebuild_laws(&tree, &ready, &child, &feed, &TreeFeedReads::default());
}

#[test]
fn host_failure_intents_are_inert_until_projected_and_replay_without_new_effects() {
    for disposition in [
        FailedTurnDisposition::Failed {
            kind: TURN_ABANDONED,
        },
        FailedTurnDisposition::RecoveryRequired,
    ] {
        let (_tmp, mut tree) = tree();
        let (child, attempt) = running_child(&mut tree);
        child_command(&mut tree, &child, &attempt, "start");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start", turn(1));
        project(&mut tree, &child, &feed);
        let before = format!("{:?}{:?}", tree.node(&child), tree.attempt(&child));
        tree.record_host_turn_failure(&child, disposition, "host observation".into(), Vec::new())
            .unwrap();
        assert_eq!(
            format!("{:?}{:?}", tree.node(&child), tree.attempt(&child)),
            before,
            "an intent does not write views"
        );
        let mut rebuilt = tree.clone();
        tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        let projected = rows(&tree);
        let ended = tree.attempt(&child).unwrap().ended_at;
        tree.fold_due(&child).unwrap();
        assert_eq!(rows(&tree), projected);
        assert_eq!(tree.attempt(&child).unwrap().ended_at, ended);
        rebuilt.clear_feed_views();
        restore_unallocated_children(&mut rebuilt);
        project(&mut rebuilt, &child, &feed);
        assert_eq!(
            rows(&rebuilt),
            projected,
            "a rebuild uses the intent's stamp"
        );
    }
}

#[test]
fn a_long_rate_limit_blocks_with_resets_at_from_the_records_commit() {
    let retry = plakous::model::ModelRetryability::Eligible {
        retry_after: Some(plakous::model::ModelRetryDelay::from_millis(600_500).unwrap()),
    };
    let (_tmp, tree, before, child, feed, reads) =
        failed_child_run(plakous::model::ModelErrorKind::RateLimited, Some(retry));
    let attempt = tree.attempt(&child).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Blocked);
    let blocked = attempt.blocked.clone().unwrap();
    assert_eq!(blocked.quota, "rate_limited");
    // Committed at 2_000 ms, plus 600.5 s, rounded up to a Unix second.
    assert_eq!(blocked.resets_at, Some(603));
    assert_failure_rebuild_laws(&tree, &before, &child, &feed, &reads);

    // A short rate limit is an ordinary, retryable failure.
    let short = plakous::model::ModelRetryability::Eligible {
        retry_after: Some(plakous::model::ModelRetryDelay::from_millis(30_000).unwrap()),
    };
    let (_tmp, tree, _, child, ..) =
        failed_child_run(plakous::model::ModelErrorKind::RateLimited, Some(short));
    let attempt = tree.attempt(&child).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(attempt.failure.clone().unwrap().kind, "rate_limited");
}

#[test]
fn a_failed_terminal_without_its_read_ends_the_attempt_on_the_facts_kinds() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal(
        "start-1",
        turn(1),
        End::Model(plakous::model::ModelErrorKind::Policy),
    );
    project(&mut tree, &child, &feed);
    let failure = tree.attempt(&child).unwrap().failure.clone().unwrap();
    assert_eq!(failure.kind, "refused");
    assert!(
        failure.message.starts_with("child turn failed: Model"),
        "{failure:?}"
    );
}

#[test]
fn child_finalization_requires_an_exact_committed_terminal() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    project(&mut tree, &child, &feed);
    let refused = tree
        .record_child_finalization(
            &child,
            &turn(2).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            terminal_evidence(&tree, &child, &feed, &turn(2).to_string()),
        )
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::OperationRecoveryRequired);
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Running);
    assert!(tree.pending_result_mail("root").is_empty());
    feed.terminal("start-1", turn(1), End::Completed);
    project(&mut tree, &child, &feed);
    assert!(!tree.node(&child).unwrap().active_turn);
    assert_eq!(
        tree.attempt(&child).unwrap().status,
        AttemptStatus::Running,
        "ended, decision pending"
    );
    assert!(tree.pending_result_mail("root").is_empty());
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        true,
        terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
    )
    .unwrap();
    let usage = tree.attempt(&child).unwrap().usage;
    tree.fold_due(&child).unwrap();
    assert_eq!(
        tree.attempt(&child).unwrap().status,
        AttemptStatus::CompletedWithoutSubmission
    );
    assert_eq!(tree.attempt(&child).unwrap().usage, usage);
    assert_eq!(tree.attempt(&child).unwrap().ended_at, Some(2_000));
    assert_eq!(tree.pending_result_mail("root").len(), 1);
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn caller_terminal_survives_a_cold_cursor_without_a_tree_cache() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal("start-1", turn(1), End::Completed);
    let projection = project(&mut tree, &child, &feed);
    let terminal = projection.ended_turns[0].1.clone();
    assert_eq!(terminal.attempt_id, attempt);
    assert_eq!(terminal.turn_id, turn(1).to_string());
    assert_eq!(terminal.seq, 2);
    let mut cold = ProductAgentTree::reopen(tree.to_persisted());
    let before = rows(&cold);
    let mut foreign = terminal.clone();
    foreign.turn_id = turn(2).to_string();
    assert!(
        cold.record_child_finalization(
            &child,
            &turn(1).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            Some(foreign)
        )
        .is_err()
    );
    assert_eq!(rows(&cold), before);
    let mut foreign_attempt = terminal.clone();
    foreign_attempt.attempt_id = AttemptId::new(&child, 99);
    assert!(
        cold.record_child_finalization(
            &child,
            &turn(1).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            Some(foreign_attempt),
        )
        .is_err()
    );
    assert_eq!(rows(&cold), before);
    cold.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        true,
        Some(terminal.clone()),
    )
    .unwrap();
    cold.fold_due(&child).unwrap();
    cold.lift_restored_pause_if_quiescent();
    assert_eq!(
        cold.attempt(&child).unwrap().status,
        AttemptStatus::CompletedWithoutSubmission
    );
    let finalized = rows(&cold);
    cold.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        true,
        Some(terminal.clone()),
    )
    .unwrap();
    assert_eq!(rows(&cold), finalized);
    let mut changed = terminal;
    changed.committed_at_ms += 1;
    assert!(
        cold.record_child_finalization(
            &child,
            &turn(1).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            Some(changed)
        )
        .is_err()
    );
    assert_eq!(rows(&cold), finalized);
    assert_child_rebuild_laws(&cold, &child, &feed).unwrap();
}

#[test]
fn a_child_turn_cannot_both_park_and_nudge() {
    for (first, second) in [
        (
            ChildFinalizationDecision::Park,
            ChildFinalizationDecision::Nudge,
        ),
        (
            ChildFinalizationDecision::Nudge,
            ChildFinalizationDecision::Park,
        ),
    ] {
        let (_tmp, mut tree) = tree();
        let (child, attempt) = running_child(&mut tree);
        child_command(&mut tree, &child, &attempt, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        feed.terminal("start-1", turn(1), End::Completed);
        project(&mut tree, &child, &feed);
        tree.record_child_finalization(&child, &turn(1).to_string(), first, false, None)
            .unwrap();
        let before = rows(&tree);
        assert_eq!(
            tree.record_child_finalization(&child, &turn(1).to_string(), second, false, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::OperationRecoveryRequired
        );
        assert_eq!(rows(&tree), before);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn child_finalization_rebuilds_with_finalize_precedence_and_late_joins(
        nudge in any::<bool>(),
        final_first in any::<bool>(),
        late in any::<bool>(),
        failed_wake in any::<bool>(),
    ) {
        let (_tmp, mut base) = tree();
        let (child, attempt) = running_child(&mut base);
        child_command(&mut base, &child, &attempt, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        project(&mut base, &child, &feed);
        feed.terminal("start-1", turn(1), End::Completed);
        let undelivered = base.send_agent_message("root", &SendAgentMessageRequest {
            agent_id: child.clone(), message: "waiting for a wake".to_owned(), purpose: None,
            priority: None, in_reply_to: None, output_schema_json: None,
        }, None).unwrap().message_id;
        let decision = if nudge { ChildFinalizationDecision::Nudge } else { ChildFinalizationDecision::Park };
        let mut expected = None;
        for reversed in [final_first, !final_first] {
            let mut live = base.clone();
            if failed_wake {
                live.record_failed_wake_mail(&child, &turn(1).to_string(), "wake refused").unwrap();
                prop_assert_eq!(durable(&live, &undelivered), MailDurableState::Accepted);
            }
            let later = live.send_agent_message("root", &SendAgentMessageRequest {
                agent_id: child.clone(), message: "later mail".to_owned(), purpose: None,
                priority: None, in_reply_to: None, output_schema_json: None,
            }, None).unwrap().message_id;
            if failed_wake {
                live.record_failed_wake_mail(&child, &turn(1).to_string(), "a retry cannot enlarge the decision").unwrap();
            }
            project(&mut live, &child, &feed);
            if late { project(&mut live, &child, &feed); }
            let decisions = if reversed { [ChildFinalizationDecision::Finalize, decision] }
                else { [decision, ChildFinalizationDecision::Finalize] };
            for decision in decisions {
                live.record_child_finalization(&child, &turn(1).to_string(), decision, decision != ChildFinalizationDecision::Nudge, (decision == ChildFinalizationDecision::Finalize).then(|| terminal_evidence(&live, &child, &feed, &turn(1).to_string())).flatten()).unwrap();
                project(&mut live, &child, &feed);
            }
            prop_assert_eq!(live.attempt(&child).unwrap().status, AttemptStatus::CompletedWithoutSubmission);
            prop_assert_eq!(live.pending_result_mail("root").len(), 1);
            prop_assert_eq!(durable(&live, &undelivered), if failed_wake { MailDurableState::Acknowledged } else { MailDurableState::Accepted });
            prop_assert_eq!(durable(&live, &later), MailDurableState::Accepted);
            assert_child_rebuild_laws(&live, &child, &feed)?;
            // A new process has no cache: a retained record below the cursor
            // authenticates the late join without charging its usage again.
            let mut restored = live.to_persisted().rows().clone();
            let usage = restored.attempt(&child).unwrap().usage;
            project(&mut restored, &child, &feed);
            prop_assert_eq!(&restored.attempt(&child).unwrap().usage, &usage);
            // Different authoring orders have different intent metadata, but
            // Finalize precedence must derive identical views.
            let mut nodes = live.nodes.clone();
            for node in nodes.values_mut() { node.fold_order = None; }
            let views = format!("{:?}{:?}{:?}{:?}{:?}", nodes, live.attempts, live.messages, live.commands, live.feed_cursors);
            if let Some(expected) = &expected { prop_assert_eq!(&views, expected); }
            else { expected = Some(views); }
        }
    }
}

#[test]
fn vcs_fault_recording_and_settlement_are_anchored_and_idempotent() {
    let (_tmp, _vcs, mut tree) = fake_tree();
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child = spawned.agent_id;
    tree.allocate_session_for_test(
        &child,
        format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
    )
    .unwrap();
    child_command(&mut tree, &child, &spawned.attempt_id, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    project(&mut tree, &child, &feed);
    let workspace = tree.node(&child).unwrap().workspace_id.clone();
    tree.workspaces.get_mut(&workspace).unwrap().status = WorkspaceStatus::Discarding;
    let before = tree.nodes.clone();
    tree.fault_in_flight_vcs();
    assert_eq!(tree.nodes, before, "recording must not write views");
    assert!(!tree.has_faulted_vcs_obligation(&child));
    tree.fold_due(&child).unwrap();
    assert!(tree.has_faulted_vcs_obligation(&child));
    tree.workspaces.get_mut(&workspace).unwrap().status = WorkspaceStatus::Discarded;
    tree.settle_workspace_faults(&workspace);
    assert!(!tree.has_faulted_vcs_obligation(&child));
    let revision = tree.revision;
    tree.settle_workspace_faults(&workspace);
    assert_eq!(tree.revision, revision);
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn orphaned_seals_fault_after_finalize_and_submitted_cancel_on_rebuild() {
    for cancelled in [false, true] {
        let (_tmp, _vcs, mut tree) = fake_tree();
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child = spawned.agent_id;
        tree.allocate_session_for_test(
            &child,
            format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
        )
        .unwrap();
        child_command(&mut tree, &child, &spawned.attempt_id, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        project(&mut tree, &child, &feed);
        tree.submit_result(
            &child,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        if cancelled {
            tree.record_host_cancellation(&child).unwrap();
        } else {
            feed.terminal("start-1", turn(1), End::Completed);
            project(&mut tree, &child, &feed);
            tree.record_child_finalization(
                &child,
                &turn(1).to_string(),
                ChildFinalizationDecision::Finalize,
                true,
                terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
            )
            .unwrap();
        }
        tree.fold_due(&child).unwrap();
        assert_eq!(tree.take_effects().seals.len(), 1);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        project(&mut reopened, &child, &feed);
        assert_eq!(
            reopened.attempt(&child).unwrap().status,
            AttemptStatus::Completed
        );
        assert_eq!(
            reopened.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(reopened.take_effects().is_empty());
        assert_child_rebuild_laws(&reopened, &child, &feed).unwrap();
    }
}

#[test]
fn seal_outcomes_rebuild_at_their_anchor_without_repeating_effects() {
    for failed in [false, true] {
        let (_tmp, vcs, mut tree) = fake_tree();
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child = spawned.agent_id;
        tree.allocate_session_for_test(
            &child,
            format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
        )
        .unwrap();
        child_command(&mut tree, &child, &spawned.attempt_id, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        feed.terminal("start-1", turn(1), End::Completed);
        project(&mut tree, &child, &feed);
        tree.record_child_finalization(
            &child,
            &turn(1).to_string(),
            ChildFinalizationDecision::Finalize,
            true,
            terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
        )
        .unwrap();
        tree.fold_due(&child).unwrap();
        let effects = tree.take_effects();
        assert_eq!(effects.seals.len(), 1);
        if failed {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
        }
        tree.complete(effects.run(vcs.as_ref()));
        let _ = tree.take_effects();
        let outcome = tree
            .operations()
            .values()
            .find(|operation| operation.tool_name == "seal_outcome")
            .unwrap();
        assert_eq!(outcome.fold.unwrap().seq, feed.records.len() as u64);
        let ended_at = tree.attempt(&child).unwrap().ended_at;
        assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
        assert_eq!(tree.attempt(&child).unwrap().ended_at, ended_at);
        let before = rows(&tree);
        project(&mut tree, &child, &feed);
        assert_eq!(rows(&tree), before);
        assert!(tree.take_effects().is_empty());
    }
}

#[test]
fn child_finalization_fences_seal_effects_across_view_rebuild() {
    let (_tmp, vcs, mut tree) = fake_tree();
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child = spawned.agent_id;
    child_command(&mut tree, &child, &spawned.attempt_id, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    feed.terminal("start-1", turn(1), End::Completed);
    project(&mut tree, &child, &feed);
    assert!(
        tree.take_effects().seals.is_empty(),
        "decision pending cannot seal"
    );
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Park,
        true,
        None,
    )
    .unwrap();
    project(&mut tree, &child, &feed);
    assert!(tree.take_effects().seals.is_empty(), "Park cannot seal");
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        true,
        terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
    )
    .unwrap();
    assert!(
        tree.take_effects().seals.is_empty(),
        "intent write alone cannot seal"
    );
    // A restored tree has no terminal cache. Reload below its saved cursor.
    let mut tree = tree.to_persisted().rows().clone();
    project(&mut tree, &child, &feed);
    let effects = tree.take_effects();
    assert_eq!(effects.seals.len(), 1);
    // Even before the external effect completes, rebuilding must not open a
    // second seal or erase the in-flight external-effect status.
    let before = rows(&tree);
    tree.clear_feed_views();
    restore_unallocated_children(&mut tree);
    project(&mut tree, &child, &feed);
    assert!(tree.take_effects().seals.is_empty());
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Sealing);
    assert_eq!(rows(&tree), before);
    let conflict = tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Finalize,
        false,
        terminal_evidence(&tree, &child, &feed, &turn(1).to_string()),
    );
    assert_eq!(
        conflict.unwrap_err().code,
        TopologyErrorCode::OperationRecoveryRequired
    );
    // The original ticket still owns the seal after rebuilding.
    tree.set_vcs(vcs);
    let _ = tree.complete(effects.run(tree.vcs()));
    tree.settle_for_test();
    assert_eq!(tree.pending_result_mail("root").len(), 1);
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn host_recovery_fences_a_real_seal_across_view_rebuild() {
    let (_tmp, vcs, mut tree) = fake_tree();
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child = spawned.agent_id;
    child_command(&mut tree, &child, &spawned.attempt_id, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    project(&mut tree, &child, &feed);
    tree.record_host_turn_failure(
        &child,
        FailedTurnDisposition::RecoveryRequired,
        "ownership uncertainty".into(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(&child).unwrap();
    tree.record_host_recovery(
        &child,
        &spawned.attempt_id,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "released quiescent".into(),
        },
    )
    .unwrap();
    assert!(
        tree.take_effects().seals.is_empty(),
        "writing the decision cannot seal"
    );
    tree.fold_due(&child).unwrap();
    let effects = tree.take_effects();
    assert_eq!(effects.seals.len(), 1);
    let expected = rows(&tree);
    tree.clear_feed_views();
    restore_unallocated_children(&mut tree);
    project(&mut tree, &child, &feed);
    assert!(
        tree.take_effects().seals.is_empty(),
        "the saved decision already reserved its seal"
    );
    assert_eq!(rows(&tree), expected);
    tree.set_vcs(vcs);
    let _ = tree.complete(effects.run(tree.vcs()));
    let sealed = rows(&tree);
    tree.clear_feed_views();
    restore_unallocated_children(&mut tree);
    project(&mut tree, &child, &feed);
    assert!(tree.take_effects().seals.is_empty());
    assert_eq!(rows(&tree), sealed);
}

#[test]
fn host_decisions_fence_real_seals_across_view_rebuild() {
    for shape in 0_u8..8 {
        let materialization = shape & 1 != 0;
        let submitted = shape & 2 != 0;
        let failed_seal = shape & 4 != 0;
        let (_tmp, vcs, mut tree) = fake_tree();
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child = spawned.agent_id;
        tree.allocate_session_for_test(
            &child,
            format!("{CHILD_SESSION}/{}/{}", uuid(0xb), uuid(0x3001)),
        )
        .unwrap();
        child_command(&mut tree, &child, &spawned.attempt_id, "start-1");
        let mut feed = Feed::new(CHILD_SESSION);
        feed.accepted("start-1", turn(1));
        project(&mut tree, &child, &feed);
        if submitted {
            let attempt = tree.node(&child).unwrap().attempt_id.clone();
            tree.record_operator_intent(
                &child,
                attempt,
                operator_intents::Decision::Submitted {
                    output_json: r#"{"done":true}"#.to_owned(),
                },
            )
            .unwrap();
        }
        tree.record_fault(&child, "saved host diagnostic");
        if materialization {
            tree.record_host_materialization(&child).unwrap();
        } else {
            tree.record_host_cancellation(&child).unwrap();
        }
        assert!(tree.take_effects().seals.is_empty());
        tree.fold_due(&child).unwrap();
        let effects = tree.take_effects();
        assert_eq!(effects.seals.len(), 1);
        let expected = rows(&tree);
        tree.clear_feed_views();
        restore_unallocated_children(&mut tree);
        project(&mut tree, &child, &feed);
        assert!(tree.take_effects().seals.is_empty());
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Sealing);
        assert_eq!(rows(&tree), expected);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(
            reopened.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let ambiguous = rows(&reopened);
        reopened.clear_feed_views();
        restore_unallocated_children(&mut reopened);
        reopened.clear_feed_views();
        restore_unallocated_children(&mut reopened);
        project(&mut reopened, &child, &feed);
        assert!(reopened.take_effects().seals.is_empty());
        assert_eq!(rows(&reopened), ambiguous);
        if failed_seal {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
        }
        tree.set_vcs(vcs);
        let _ = tree.complete(effects.run(tree.vcs()));
        let sealed = rows(&tree);
        tree.clear_feed_views();
        restore_unallocated_children(&mut tree);
        project(&mut tree, &child, &feed);
        assert!(tree.take_effects().seals.is_empty());
        assert_eq!(rows(&tree), sealed);
        assert_eq!(tree.pending_result_mail("root").len(), 1);
    }
}

#[test]
fn retirement_intents_keep_the_first_observation_and_fence_delayed_joiners() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start-1", turn(1));
    project(&mut tree, &child, &feed);
    let wrong = AttemptId::new(&child, 99);
    assert!(
        !tree
            .record_host_retirement_for_attempt(&child, Some(&wrong), "late caller".into())
            .unwrap()
    );
    assert!(
        tree.record_host_retirement_for_attempt(
            &child,
            Some(&attempt),
            "first release uncertainty".into()
        )
        .unwrap()
    );
    tree.record_host_retirement(&child, "later diagnostic".into())
        .unwrap();
    tree.fold_due(&child).unwrap();
    assert_eq!(
        tree.attempt(&child)
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .message,
        "first release uncertainty"
    );
    tree.recover_host_decision(
        &child,
        &attempt,
        RecoveryResolution::RecoveredWithoutTurn {
            message: "released".into(),
        },
    )
    .unwrap();
    let settled = rows(&tree);
    assert!(
        !tree
            .record_host_retirement_for_attempt(&child, Some(&attempt), "late caller".into())
            .unwrap()
    );
    tree.fold_due(&child).unwrap();
    assert_eq!(rows(&tree), settled);
    assert_child_rebuild_laws(&tree, &child, &feed).unwrap();
}

#[test]
fn terminal_read_selection_skips_five_hundred_tool_rounds() {
    use plakous::sessions::feed::{FeedOperationKind, FeedOperationState};
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    for round in 0..=500 {
        feed.history(turn(1), Vec::new());
        let prior = feed.records.pop().unwrap();
        let mut header = *prior.header();
        if round < 500 {
            let mut encoded = serde_json::to_value(header).unwrap();
            encoded["change"]["advanced"]["head"] = "working".into();
            header = serde_json::from_value(encoded).unwrap();
        }
        let mut facts = prior.facts().to_vec();
        facts.push(FeedFact::OperationAdvanced {
            operation_id: round + 1,
            turn_id: Some(turn(1)),
            kind: FeedOperationKind::ProcessSession { job_number: None },
            state: FeedOperationState::Finalized,
        });
        feed.records.push(FeedRecord::from_store(header, facts));
    }
    feed.terminal(
        "failed",
        turn(2),
        End::Model(plakous::model::ModelErrorKind::Transport),
    );
    assert_eq!(
        terminal_reads_needed(&feed.records),
        [(501, turn(1)), (502, turn(2))]
    );
    feed.history(turn(1), Vec::new());
    assert_eq!(
        terminal_reads_needed(&feed.records),
        [(502, turn(2))],
        "a later record naming the turn rules out a closure candidate"
    );
}

#[test]
fn transient_resume_intent_keeps_failed_attempt_running_across_rebuilds() {
    let (_tmp, _failed, mut tree, child, feed, reads) = failed_child_run(
        plakous::model::ModelErrorKind::Transport,
        Some(plakous::model::ModelRetryability::Exhausted),
    );
    let attempt_id = tree.attempt(&child).unwrap().attempt_id.clone();
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Resume { round: 1 },
        false,
        None,
    )
    .unwrap();
    let before = tree.clone();
    assert!(
        tree.parked_reopen_targets()
            .iter()
            .any(|(agent, ..)| agent == &child),
        "a crash after Resume commit but before failure projection must re-enter its gate"
    );
    tree.project_feed(&child, CHILD_SESSION, &feed.records, &reads)
        .unwrap();
    let attempt = tree.attempt(&child).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Running);
    assert_eq!(attempt.turn_id, None);
    assert_eq!(attempt.failure, None);
    assert_eq!(attempt.submit_harness_nudge, SubmitNudgeBudget::NONE);
    assert_eq!(tree.transient_resume_count(&child, &attempt_id).unwrap(), 1);
    assert_eq!(
        tree.pending_transient_resume(&child).unwrap(),
        Some((turn(1).to_string(), 1))
    );
    assert_failure_rebuild_laws(&tree, &before, &child, &feed, &reads);
    record_child_command(
        &mut tree,
        &child,
        &attempt_id,
        &format!("plak:resume-transient:{child}:{attempt_id}:1"),
        AgentCommandKind::Resume,
    );
    let mut resumed = feed;
    resumed.accepted(
        &format!("plak:resume-transient:{child}:{attempt_id}:1"),
        turn(2),
    );
    resumed.push(vec![
        serde_json::from_value(serde_json::json!({"workflow_settled": {
            "workflow_id": "07".repeat(32), "operation_id": null,
            "kind": {"compact": {"reason": "turn_fence"}},
            "outcome": "no_eligible_range"
        }}))
        .unwrap(),
    ]);
    project(&mut tree, &child, &resumed);
    assert_eq!(tree.pending_transient_resume(&child).unwrap(), None);
    assert_eq!(
        tree.attempt(&child).unwrap().submit_harness_nudge,
        SubmitNudgeBudget::NONE
    );
    let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
    project(&mut reopened, &child, &resumed);
    for projected in [&tree, &reopened] {
        let row = projected
            .full_list_json(
                projected.node(&child).unwrap(),
                projection::NodeOccupancy::default(),
            )
            .unwrap();
        assert_eq!(row["transient_resumes"], 1);
        assert_eq!(row["compaction"]["no_eligible"], 1);
    }
}

#[test]
fn transient_resume_intent_refuses_conflicting_decisions_and_rounds() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start-1");
    for round in [0, 2, 3] {
        assert!(
            tree.record_child_finalization(
                &child,
                &turn(1).to_string(),
                ChildFinalizationDecision::Resume { round },
                false,
                None
            )
            .is_err()
        );
    }
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Resume { round: 1 },
        false,
        None,
    )
    .unwrap();
    tree.record_child_finalization(
        &child,
        &turn(1).to_string(),
        ChildFinalizationDecision::Resume { round: 1 },
        false,
        None,
    )
    .unwrap();
    for decision in [
        ChildFinalizationDecision::Park,
        ChildFinalizationDecision::Nudge,
        ChildFinalizationDecision::Resume { round: 2 },
    ] {
        assert!(
            tree.record_child_finalization(&child, &turn(1).to_string(), decision, false, None)
                .is_err()
        );
    }
}
