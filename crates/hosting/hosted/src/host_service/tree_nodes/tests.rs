use std::sync::{Arc, Mutex};

use super::{Answer, ChildCounts, count_children};

#[test]
fn directory_counts_exclude_root_and_separate_parked_held_and_running_children() {
    use crate::agent_tree::{AgentCommand, AgentCommandKind, ProductAgentTree};
    use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
    let dir = tempfile::tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "tree",
        "root",
        "owner",
        dir.path().to_path_buf(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let counts = |work, running, held| ChildCounts {
        work,
        running,
        held,
    };
    assert_eq!(count_children(&tree, false), counts(0, 0, 0));
    let live = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    assert_eq!(count_children(&tree, false), counts(1, 1, 0));
    let held = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&held, "locator").unwrap();
    tree.set_active_turn(&held, true).unwrap();
    tree.bind_attempt_turn(&held, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    let attempt = tree.node(&held).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        "start",
        &held,
        attempt,
        AgentCommandKind::Start,
        "assignment",
        Vec::new(),
    ))
    .unwrap();
    tree.release_attempt_turn(&held).unwrap();
    assert_eq!(count_children(&tree, false), counts(2, 2, 0));
    tree.park_child(&held, Vec::new()).unwrap();
    assert_eq!(count_children(&tree, false), counts(2, 1, 0));
    let mut handed_off = tree.clone();
    assert_eq!(
        handed_off.record_handoff().unwrap().as_slice(),
        std::slice::from_ref(&held)
    );
    assert_eq!(count_children(&handed_off, false), counts(2, 2, 0));
    tree.mark_owner_lost(&held);
    assert_eq!(count_children(&tree, false), counts(1, 1, 1));
    assert_eq!(count_children(&tree, true), counts(2, 2, 0));
    tree.submit_result(
        &live,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&live).unwrap();
    assert_eq!(count_children(&tree, false), counts(0, 0, 1));
    tree.set_active_turn(&held, true).unwrap();
    assert_eq!(count_children(&tree, false), counts(1, 1, 0));
}

#[test]
fn directory_counts_keep_a_seal_live_until_it_settles() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, Tier, WriterMode};
    use plakous_workspace::occupancy_vcs::fake::FakeVcs;

    let dir = tempfile::tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        "tree",
        "root",
        "owner",
        dir.path().to_path_buf(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let request =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "work", "assignment").with_fork();
    let child = tree.spawn_agent("root", &request, None).unwrap().agent_id;
    tree.record_turn_completed(&child).unwrap();
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Sealing);
    assert_eq!(count_children(&tree, false).running, 1);
    tree.settle_for_test();
    assert_eq!(count_children(&tree, false).running, 0);
}

/// A control spawned on an executor that already stopped is still
/// answered: the dropped answer reports that the executor stopped.
#[test]
fn a_control_on_a_stopped_executor_is_answered() {
    let answered: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let handle = executor.handle().clone();
    drop(executor);
    let done = Answer(Some(Box::new({
        let answered = Arc::clone(&answered);
        move |result| answered.lock().unwrap().push(result)
    })));
    drop(handle.spawn(async move {
        done.send(Ok(()));
    }));
    assert_eq!(
        *answered.lock().unwrap(),
        [Err("the agent tree's executor stopped".to_owned())]
    );
}

/// An answered control is answered once.
#[test]
fn an_answer_is_sent_once() {
    let answered: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let done = Answer(Some(Box::new({
        let answered = Arc::clone(&answered);
        move |result| answered.lock().unwrap().push(result)
    })));
    done.send(Ok(()));
    assert_eq!(*answered.lock().unwrap(), [Ok(())]);
}
