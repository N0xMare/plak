//! The recovery hook is never entered by a child acknowledging its parent.

use super::*;

struct RecoveryCounter(AtomicUsize);

impl ChildController for RecoveryCounter {
    fn services(&self) -> ChildServices {
        ChildServices::MAIL
    }

    fn recover_followup(
        &self,
        _: String,
        _: crate::protocol::AttemptId,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_grandchild_with_message_permission_cannot_run_parent_recovery() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.grants_mut_for_test("root").max_depth = 3;
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.spawns = vec![TemplateName::parse("review").unwrap()];
    tree.pin_template("orchestrate", binding);
    let parent = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let child = tree
        .spawn_agent(&parent.agent_id, &spawn_request(), None)
        .unwrap();
    assert!(
        tree.grant(&child.agent_id)
            .unwrap()
            .message_peers
            .contains(&parent.agent_id)
    );
    let driver = driver_on(tree, store);
    let mut tool = send_tool(driver.clone(), None);
    *tool.caller_id.lock().unwrap() = child.agent_id;
    let counter = Arc::new(RecoveryCounter(AtomicUsize::new(0)));
    tool.controller = Some(counter.clone());
    let outcome = tool
        .send_with(
            "child-recovery-of-parent".into(),
            &json!({
                "agent_id": parent.agent_id,
                "purpose": "followup",
                "message": "fresh task",
                "acknowledge_recovery": parent.attempt_id,
            }),
        )
        .await
        .unwrap();
    assert_eq!(outcome_json(&outcome)["code"], "grant_denied");
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        driver
            .read(|tree| tree.node(&parent.agent_id).unwrap().attempt_id.clone())
            .unwrap(),
        Some(parent.attempt_id)
    );
}
