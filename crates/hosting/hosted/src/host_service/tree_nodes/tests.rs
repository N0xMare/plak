use std::sync::{Arc, Mutex};

use super::Answer;

#[test]
fn directory_counts_exclude_root_and_separate_live_held_and_completed_children() {
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
    assert_eq!(super::count_children(&tree), (0, 0));
    let live = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    assert_eq!(super::count_children(&tree), (1, 0));
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
    tree.park_child(&held, Vec::new()).unwrap();
    assert_eq!(super::count_children(&tree), (2, 0));
    tree.mark_owner_lost(&held);
    assert_eq!(super::count_children(&tree), (1, 1));
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
    assert_eq!(super::count_children(&tree), (0, 1));
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
