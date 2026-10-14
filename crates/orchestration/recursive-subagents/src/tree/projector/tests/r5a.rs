#![cfg(test)]

use super::*;

#[test]
fn deleted_fold_refuses_recorders_and_due_without_mutation() {
    for recorder in 0..9 {
        let (_tmp, mut tree) = tree();
        let (child, attempt) = running_child(&mut tree);
        tree.clear_agent_feed_views(&child, CHILD_SESSION);
        let before = format!("{:?}", tree.to_persisted());
        let result = match recorder {
            0 => tree.record_host_cancellation(&child),
            1 => tree.record_host_retirement(&child, "release failed".into()),
            2 => tree.record_host_materialization(&child),
            3 => tree.record_host_turn_failure(
                &child,
                FailedTurnDisposition::RecoveryRequired,
                "host failure".into(),
                Vec::new(),
            ),
            4 => tree
                .record_host_recovery(
                    &child,
                    &attempt,
                    RecoveryResolution::RecoveredWithoutTurn {
                        message: "reopened".into(),
                    },
                )
                .map(|_| ()),
            5 => tree.record_close_target(&child, 1000),
            6 => tree.fold_due(&child).map(|_| ()),
            7 => tree
                .close_agent("root", &close_req(&child), None)
                .map(|_| ()),
            _ => tree
                .open_followup_attempt(&child, "followup", "{}".into(), digest_text("{}"))
                .map(|_| ()),
        };
        assert_eq!(
            result.unwrap_err().code,
            TopologyErrorCode::OperationRecoveryRequired,
            "recorder={recorder}"
        );
        assert_eq!(
            format!("{:?}", tree.to_persisted()),
            before,
            "recorder={recorder}"
        );
        assert!(tree.node(&child).unwrap().fold_order.is_none());
    }
}

#[test]
fn close_drain_never_restarts_a_deleted_fold() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_close_target(&child, 1000).unwrap();
    tree.clear_agent_feed_views(&child, CHILD_SESSION);
    tree.pending_close_folds.insert(child.clone());
    let before = format!("{:?}", tree.to_persisted());
    assert!(tree.fold_reserved_closes().unwrap().is_empty());
    assert_eq!(format!("{:?}", tree.to_persisted()), before);
    assert!(tree.pending_close_folds.contains(&child));
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("untracked-start", turn(1));
    project(&mut tree, &child, &feed);
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
    assert!(tree.pending_close_folds.is_empty());
}

#[test]
fn startup_defers_ancestor_cascade_until_deleted_descendant_rebuilds() {
    use crate::hub::TopologyHub;
    let (_tmp, mut tree) = tree();
    tree.grants_mut_for_test("root").max_depth = 2;
    let parent = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap()
        .agent_id;
    let child = tree
        .spawn_agent(&parent, &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.record_host_cancellation(&parent).unwrap();
    tree.clear_agent_feed_views(&child, CHILD_SESSION);
    let store = Arc::new(MemoryTreeStore::new());
    tree.acknowledge_revision(store.save(&tree).unwrap());
    let id = tree.tree_id.clone();
    let loaded = ProductAgentTree::reopen(store.load(&id).unwrap().unwrap());
    let runtime = TopologyHub::new()
        .insert_root(&id, loaded, store, Arc::new(FakeVcs::default()), None)
        .unwrap();
    runtime
        .read(|tree| {
            assert!(tree.node(&child).unwrap().fold_order.is_none());
            assert_eq!(tree.node(&parent).unwrap().status, NodeStatus::Running);
        })
        .unwrap();
    let ((), _effects) = runtime
        .driver()
        .mutate(|tree| {
            runtime
                .driver()
                .fold_and_commit(tree, |tree| {
                    tree.project_feed(&child, CHILD_SESSION, &[], &TreeFeedReads::default())
                        .map_err(TopologyError::from)?;
                    Ok(())
                })
                .unwrap();
        })
        .unwrap();
    runtime
        .read(|tree| {
            assert_eq!(tree.attempt(&parent).unwrap().status, AttemptStatus::Failed);
            assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
        })
        .unwrap();
}

#[test]
fn stale_page_after_cursor_regression_refuses_then_full_reread_converges() {
    let (_tmp, mut tree) = tree();
    let (child, attempt) = running_child(&mut tree);
    child_command(&mut tree, &child, &attempt, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    project(&mut tree, &child, &feed);
    feed.terminal("start", turn(1), End::Completed);
    let page = feed.records[1..].to_vec();
    tree.clear_agent_feed_views(&child, CHILD_SESSION);
    let before = format!("{:?}", tree.to_persisted());
    assert!(matches!(
        tree.project_feed(&child, CHILD_SESSION, &page, &TreeFeedReads::default()),
        Err(TreeProjectorError::Gap {
            expected: 1,
            found: 2
        })
    ));
    assert_eq!(format!("{:?}", tree.to_persisted()), before);
    project(&mut tree, &child, &feed);
    assert_eq!(tree.feed_cursor(CHILD_SESSION), 2);
    assert_eq!(tree.attempt(&child).unwrap().usage.turns, 1);
    assert!(tree.attempt(&child).unwrap().turn_id.is_none());
}

#[test]
fn applied_old_attempt_read_is_inert_after_followup_retirement() {
    let (_tmp, mut tree) = tree();
    let (child, first) = running_child(&mut tree);
    child_command(&mut tree, &child, &first, "start");
    let mut feed = Feed::new(CHILD_SESSION);
    feed.accepted("start", turn(1));
    feed.terminal("start", turn(1), End::Abandoned);
    let mut reads = TreeFeedReads::default();
    reads.insert_terminal(
        turn(1).to_string(),
        TerminalRead {
            message: "abandoned first attempt".into(),
            abandoned: Some(AbandonedChildTurn {
                operations: vec![7],
                claim: AbandonedTurnClaim::RunningTool,
            }),
        },
    );
    tree.project_feed(&child, CHILD_SESSION, &feed.records, &reads)
        .unwrap();
    let second = tree
        .open_followup_attempt(&child, "followup", "{}".into(), digest_text("{}"))
        .unwrap();
    tree.record_host_turn_failure(
        &child,
        FailedTurnDisposition::RecoveryRequired,
        "newer recovery obligation".into(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(&child).unwrap();
    tree.record_host_retirement(&child, "release failed".into())
        .unwrap();
    tree.fold_due(&child).unwrap();
    let before = rows(&tree);
    let revision = tree.revision;
    tree.project_feed(&child, CHILD_SESSION, &feed.records, &reads)
        .unwrap();
    assert_eq!(
        tree.revision, revision,
        "the old attempt's applied read never refolds the new attempt"
    );
    assert_eq!(rows(&tree), before);
    assert_eq!(
        tree.node(&child).unwrap().attempt_id.as_ref(),
        Some(&second)
    );
    assert_eq!(
        tree.attempt(&child).unwrap().failure.as_ref().unwrap().kind,
        "turn_recovery_required"
    );
    let mut rebuilt = tree.clone();
    rebuilt.clear_feed_views();
    restore_unallocated_children(&mut rebuilt);
    rebuilt
        .project_feed(&child, CHILD_SESSION, &feed.records, &reads)
        .unwrap();
    assert_eq!(rows(&rebuilt), before);
}

#[test]
fn refold_usage_filter_is_sequence_exact_for_repeated_turn_ids() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    let mut feed = Feed::new(CHILD_SESSION);
    let old = turn_usage(1, 11, 7);
    let new = turn_usage(2, 29, 13);
    feed.terminal_with("start", turn(1), End::RecoveryRequired, old);
    feed.terminal_with("start", turn(1), End::Completed, new);
    let mut projection = tree
        .project_feed(
            &child,
            CHILD_SESSION,
            &feed.records,
            &TreeFeedReads::default(),
        )
        .unwrap();
    projection.retain_new_usage(&feed.records, 1);
    assert_eq!(projection.applied, 1);
    assert_eq!(projection.terminal_usages.len(), 1);
    assert_eq!(projection.terminal_usages[0].usage, new);
    let again = tree
        .project_feed(
            &child,
            CHILD_SESSION,
            &feed.records,
            &TreeFeedReads::default(),
        )
        .unwrap();
    assert_eq!(again.applied, 0);
    assert!(again.terminal_usages.is_empty());
}
