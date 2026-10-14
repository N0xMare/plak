//! Read-only rescue snapshots, separate from adoptable candidate authority.

use crate::agent_tree_protocol::{WorkspaceMode, WorkspaceStatus};
use plakous_workspace::occupancy_vcs::{candidate_ref, digest_text};

use super::{ChildTurnIdentity, TreeDriver};

pub(super) async fn snapshot_failure(
    driver: &TreeDriver,
    identity: &ChildTurnIdentity,
    agent_id: &str,
) -> Option<String> {
    let ticket = driver
        .read(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return None;
            }
            let node = tree.node(agent_id).ok()?;
            let workspace = tree.workspace(&node.workspace_id)?;
            if workspace.mode == WorkspaceMode::Inherit
                || workspace.status != WorkspaceStatus::Active
            {
                return None;
            }
            Some((
                tree.vcs_port(),
                node.path.clone(),
                digest_text(&tree.tree_id),
                format!("failed-{}-{}", identity.attempt, identity.turn),
            ))
        })
        .ok()
        .flatten()?;
    let (vcs, path, namespace, id) = ticket;
    let reference = candidate_ref(&namespace, &id);
    // The seal primitive leaves HEAD, the branch and the real index untouched,
    // and excludes secrets. This pin is not registered as an adoptable candidate.
    let result = tokio::task::spawn_blocking(move || vcs.seal(&path, &namespace, &id)).await;
    Some(match result {
        Ok(Ok(snapshot)) => format!(
            "; failed worktree snapshot: {reference} (commit {})",
            snapshot.commit
        ),
        Ok(Err(error)) => format!(
            "; failed worktree snapshot unavailable: {} (ref {reference})",
            error.detail
        ),
        Err(_) => "; failed worktree snapshot unavailable: snapshot worker failed".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier};
    use crate::occupancy_vcs::fake::{FakeOp, FakeVcs};
    use crate::topology_runtime::ProductTopologyRuntime;
    use plakous::model::TurnId;
    use std::{str::FromStr, sync::Arc};

    fn turn(n: u8) -> TurnId {
        TurnId::from_str(&format!("00000000-0000-0000-0000-{n:012}")).unwrap()
    }

    fn setup(
        path: &std::path::Path,
    ) -> (
        ProductTopologyRuntime,
        Arc<FakeVcs>,
        String,
        ChildTurnIdentity,
    ) {
        let vcs = Arc::new(FakeVcs::new());
        let runtime = ProductTopologyRuntime::new(
            crate::topology_runtime::test_support::bootstrap("failed-pins", path.to_path_buf()),
            crate::topology_runtime::test_support::memory_store(),
            vcs.clone(),
            None,
        );
        let (child, identity) = runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent(
                        "root",
                        &SpawnAgentRequest::test_named(
                            "implement",
                            Tier::Mutate,
                            "implement",
                            "work",
                        )
                        .with_fork(),
                        None,
                    )
                    .unwrap()
                    .agent_id;
                tree.bind_attempt_turn(&child, turn(1).to_string()).unwrap();
                let identity = ChildTurnIdentity {
                    attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                    turn: turn(1),
                };
                Ok((child, identity))
            })
            .unwrap();
        (runtime, vcs, child, identity)
    }

    #[tokio::test]
    async fn failed_worktree_pins_each_turn_of_the_same_attempt_separately() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, vcs, child, mut identity) = setup(root.path());
        let first = snapshot_failure(runtime.driver(), &identity, &child)
            .await
            .unwrap();
        runtime
            .mutate_then_emit(false, |tree| {
                tree.bind_attempt_turn(&child, turn(2).to_string()).unwrap();
                Ok(())
            })
            .unwrap();
        identity.turn = turn(2);
        let second = snapshot_failure(runtime.driver(), &identity, &child)
            .await
            .unwrap();
        assert_ne!(
            first, second,
            "a later failure must not overwrite the first turn's rescue pin"
        );
        assert!(first.contains(&turn(1).to_string()));
        assert!(second.contains(&turn(2).to_string()));
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 2);
    }

    #[tokio::test]
    async fn failed_worktree_seal_refusal_reports_the_rescue_ref() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, vcs, child, identity) = setup(root.path());
        vcs.seal_next_with(Err(
            plakous_workspace::occupancy_vcs::VcsError::seal_failed("pin refused"),
        ));
        let message = snapshot_failure(runtime.driver(), &identity, &child)
            .await
            .unwrap();
        let reference = candidate_ref(
            &digest_text("failed-pins"),
            &format!("failed-{}-{}", identity.attempt, identity.turn),
        );
        assert!(message.contains(&reference), "{message}");
        assert!(message.contains("pin refused"));
    }

    #[tokio::test]
    async fn failed_worktree_does_not_seal_an_inactive_workspace() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, vcs, child, identity) = setup(root.path());
        runtime
            .mutate_then_emit(false, |tree| {
                let workspace = tree.node(&child).unwrap().workspace_id.clone();
                tree.set_workspace_adopting_for_test(&workspace, "root");
                Ok(())
            })
            .unwrap();
        assert!(
            snapshot_failure(runtime.driver(), &identity, &child)
                .await
                .is_none()
        );
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
    }

    #[tokio::test]
    async fn failed_worktree_does_not_seal_a_stale_turn_or_attempt() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, vcs, child, mut identity) = setup(root.path());
        identity.turn = turn(2);
        assert!(
            snapshot_failure(runtime.driver(), &identity, &child)
                .await
                .is_none()
        );
        identity.turn = turn(1);
        identity.attempt = crate::agent_tree_protocol::AttemptId::new("foreign-attempt", 1);
        assert!(
            snapshot_failure(runtime.driver(), &identity, &child)
                .await
                .is_none()
        );
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
    }
}
