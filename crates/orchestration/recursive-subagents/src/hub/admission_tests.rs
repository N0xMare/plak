//! A restored tree is admitted live.

#![allow(clippy::unwrap_used, reason = "test-owned runtime")]

use super::*;
use crate::tree::TreeAdmissionState;

#[tokio::test]
async fn restored_nonquiescent_tree_is_live_immediately_after_insert() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let mut tree = test_support::bootstrap(&id, root.path().to_owned());
    tree.spawn_agent(
        "root",
        &crate::protocol::SpawnAgentRequest::test_review(),
        None,
    )
    .unwrap();
    tree.apply_reopen_law();
    assert!(tree.quiescence_refusal().is_some());
    let runtime = insert_root(id.clone(), tree, None).await.unwrap();
    assert_eq!(
        runtime.read(|tree| tree.state).unwrap(),
        TreeAdmissionState::Live
    );
    force_release(&id).await;
}
