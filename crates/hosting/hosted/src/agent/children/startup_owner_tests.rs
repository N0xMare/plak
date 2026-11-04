//! Startup owners belong to the tree's retained release owners: neither the
//! tree's last-lease shutdown nor a closed tree task set cuts their release.
use crate::runtime::test_kit::BlockOn as _;

use super::bind::StartupWorker;
use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
use std::sync::Arc;
use std::time::Duration;

fn tree_with_child(
    workspace: &std::path::Path,
) -> (
    String,
    Arc<crate::topology_runtime::ProductTopologyRuntime>,
    String,
) {
    let tree_id = uuid::Uuid::new_v4().to_string();
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &tree_id,
        "root",
        "owner",
        workspace.to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
        .unwrap()
        .agent_id;
    let runtime = crate::topology_runtime::insert_root(tree_id.clone(), tree, None)
        .block()
        .unwrap();
    (tree_id, runtime, child)
}

fn holds_turn_owner(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    child: &str,
) -> bool {
    runtime.read(|tree| tree.has_turn_owner(child)).unwrap()
}

/// A startup owner dropped after the tree's task set closed still runs its
/// cleanup, which releases the child's startup reservation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_startup_owner_dropped_after_the_tree_closes_still_frees_its_reservation() {
    let workspace = tempfile::tempdir().unwrap();
    let (tree_id, runtime, child) = tree_with_child(workspace.path());
    let startup = runtime.reserve_startup(&child).await.unwrap();
    assert!(holds_turn_owner(&runtime, &child));
    runtime.driver().tasks().abort_all();

    drop(StartupWorker::new(
        Arc::clone(&runtime),
        child.clone(),
        startup,
    ));

    tokio::time::timeout(Duration::from_secs(10), async {
        while holds_turn_owner(&runtime, &child) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the drop cleanup released the reservation");
    assert!(runtime.driver().tasks().is_closed());
    crate::topology_runtime::force_release(&tree_id).await;
}

/// A startup in flight when the tree's last lease shuts it down keeps
/// running and finishes its own release after the shutdown returns.
#[tokio::test(start_paused = true)]
async fn a_startup_in_flight_at_the_last_lease_shutdown_finishes_its_release() {
    let workspace = tempfile::tempdir().unwrap();
    let (tree_id, runtime, child) = tree_with_child(workspace.path());
    let startup = runtime.reserve_startup(&child).await.unwrap();
    let mut worker = StartupWorker::new(Arc::clone(&runtime), child.clone(), startup);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let owner = runtime.driver().spawn_release_owner(async move {
        let _ = released.await;
        worker.finish().await;
    });

    crate::topology_runtime::force_release(&tree_id).await;

    assert!(runtime.driver().tasks().is_closed());
    assert!(
        !owner.is_finished(),
        "shutdown left the startup owner running"
    );
    assert!(holds_turn_owner(&runtime, &child));
    release.send(()).unwrap();
    owner.await.expect("the startup owner finished its release");
    assert!(!holds_turn_owner(&runtime, &child));
}

/// Every spawn in the files that own child sessions is a release owner, at
/// exactly these sites, so moving one onto the abortable tree task set (or a
/// bare `tokio::spawn`) fails here. The other spawns are a local JoinSet
/// its caller joins, the thread a dropped turn waiter registration leaves
/// on, and the child's actor, which every production caller starts on the
/// driver's release owners.
#[test]
fn child_owner_spawns_stay_release_owners() {
    let files: [(&str, &str, usize); 6] = [
        ("bind.rs", include_str!("bind.rs"), 3),
        ("turns.rs", include_str!("turns.rs"), 2),
        ("waiter.rs", include_str!("waiter.rs"), 2),
        ("live.rs", include_str!("../live.rs"), 0),
        ("deliver.rs", include_str!("deliver.rs"), 1),
        ("park.rs", include_str!("../park.rs"), 0),
    ];
    let allowed = [
        ("turns.rs", "retirements.spawn"),
        ("waiter.rs", "leaver.spawn"),
        // The child's actor, on the release owners its constructor is given.
        ("live.rs", "owners.spawn"),
    ];
    for (name, source, owners) in files {
        let code = source.split_whitespace().collect::<String>();
        assert!(
            !code.contains("tokio::spawn(") && !code.contains("task::spawn("),
            "{name}: a bare spawn escapes every task set"
        );
        let mut release_owners = 0;
        for (at, _) in code.match_indices(".spawn") {
            let call = &code[at + 1..];
            let end = call
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(call.len());
            let method = &call[..end];
            if !call[end..].starts_with('(') {
                continue;
            }
            if method == "spawn_release_owner" {
                release_owners += 1;
                continue;
            }
            let receiver_start = code[..at]
                .rfind(|c: char| !(c.is_alphanumeric() || "_().".contains(c)))
                .map_or(0, |index| index + 1);
            let site = format!("{}.{method}", &code[receiver_start..at]);
            assert!(
                allowed
                    .iter()
                    .any(|(file, allowed)| *file == name && site.ends_with(allowed)),
                "{name}: `{site}` is not a release owner"
            );
        }
        assert_eq!(release_owners, owners, "{name}: release-owner sites");
    }
}
