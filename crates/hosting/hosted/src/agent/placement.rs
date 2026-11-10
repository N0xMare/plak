use std::sync::Arc;

use super::children::ChildTurnIdentity;
use crate::agent_tree_tools::TreeDriver;
use crate::topology_runtime::LiveChildTable;

pub(super) fn watch_debt(owner: &Arc<crate::topology_runtime::ProductTopologyRuntime>) {
    let weak = Arc::downgrade(owner);
    owner.driver().tasks().spawn(async move {
        let mut previous = std::collections::BTreeSet::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(runtime) = weak.upgrade().filter(|runtime| !runtime.is_closed()) else {
                break;
            };
            let Some((ready, seals)) = runtime.try_read(|tree| {
                let mut owed = tree.recovery_owed_targets();
                owed.extend(tree.session_recovery_targets());
                owed.extend(
                    tree.loss_continuations()
                        .into_iter()
                        .filter(|(_, continuation)| {
                            use crate::agent_tree::LossContinuationState;
                            continuation.state == LossContinuationState::Pending
                                || (continuation.state == LossContinuationState::Held
                                    && (runtime.continues_after_loss()
                                        || continuation.cause
                                            == crate::agent_tree::ContinuationCause::Handoff))
                        })
                        .map(|(id, _)| id),
                );
                let ready = owed
                    .into_iter()
                    .filter(|id| tree.placement_ready(id))
                    .collect::<std::collections::BTreeSet<_>>();
                let seals = tree.nodes().values().any(|node| {
                    node.status == crate::agent_tree_protocol::NodeStatus::Sealing
                        && tree.placement_ready(&node.agent_id)
                });
                (ready, seals)
            }) else {
                continue;
            };
            let released = ready.difference(&previous).next().is_some();
            previous = ready;
            if seals {
                // Only queued tickets drain; a staged/uncertain seal has none.
                let _ = runtime.mutate_and_settle(|_| ()).await;
            }
            if released {
                runtime.driver().request_retirement_recovery();
            }
        }
    });
}

pub(super) fn ready(driver: &TreeDriver, agent: &str) -> bool {
    driver
        .read(|tree| tree.placement_ready(agent))
        .unwrap_or(false)
}

// A between-turn wait owns no tree/live lock and claims no mail. The safety
// tick also observes placement edges, which do not advance the tree revision.
pub(super) async fn wait_ready(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<LiveChildTable>>,
    agent: &str,
    identity: &ChildTurnIdentity,
) -> bool {
    loop {
        let Ok(notify) = driver.read(crate::agent_tree::ProductAgentTree::waiters) else {
            return false;
        };
        let mut notified = Box::pin(notify.notified_owned());
        notified.as_mut().enable();
        if !super::children::resume_allowed(driver, agent, identity)
            || live
                .lock()
                .await
                .get(agent)
                .is_none_or(|child| child.turn_id() != identity.turn)
            || driver
                .read(|tree| tree.tree_id.clone())
                .ok()
                .and_then(|id| crate::topology_runtime::lookup(&id))
                .is_some_and(|runtime| runtime.is_closed())
        {
            return false;
        }
        if ready(driver, agent) {
            return true;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), notified).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::recovery;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Uncalled;
    type UncalledFuture<'a, T> =
        std::pin::Pin<Box<dyn Future<Output = Result<T, recovery::PortRefusal>> + Send + 'a>>;
    impl recovery::ChildSessionPort for Uncalled {
        fn reopen<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
        ) -> UncalledFuture<'a, recovery::ChildReopened> {
            Box::pin(async { panic!("unavailable recovery called its session port") })
        }
        fn replay_create<'a>(
            &'a self,
            _: &'a crate::topology_runtime::ProductTopologyRuntime,
            _: &'a str,
        ) -> UncalledFuture<'a, recovery::ReplayedCreate> {
            Box::pin(async { panic!("unavailable recovery replayed create") })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn placement_hold_recovery_watch_isolates_ready_debt_and_rechecks_release_once() {
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(crate::occupancy_vcs::fake::FakeVcs::new());
        let mut tree = crate::topology_runtime::test_support::bootstrap(
            "placement-recovery",
            root.path().to_owned(),
        );
        tree.set_vcs(vcs.clone());
        let mut request = crate::agent_tree_protocol::SpawnAgentRequest::test_review();
        request.workspace = crate::agent_tree_protocol::SpawnWorkspace::Fork;
        let held = tree.spawn_agent("root", &request, None).unwrap().agent_id;
        let ready = tree.spawn_agent("root", &request, None).unwrap().agent_id;
        let held_path = tree.node(&held).unwrap().path.clone();
        vcs.set_placement_ready(held_path.clone(), false);
        for id in [&held, &ready] {
            tree.record_host_retirement(id, "owner lost".into())
                .unwrap();
            tree.fold_due(id).unwrap();
        }
        let runtime = Arc::new(crate::topology_runtime::ProductTopologyRuntime::new(
            tree,
            crate::topology_runtime::test_support::memory_store(),
            vcs.clone(),
            None,
        ));
        assert!(
            recovery::recover_child(&runtime, &Uncalled, &held, None)
                .await
                .is_err()
        );
        let triggered = Arc::new(AtomicUsize::new(0));
        let count = triggered.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }))
            .await;
        watch_debt(&runtime);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            triggered.load(Ordering::SeqCst),
            1,
            "held debt blocked unrelated ready recovery"
        );
        vcs.set_placement_ready(held_path, true);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(triggered.load(Ordering::SeqCst), 2);
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            triggered.load(Ordering::SeqCst),
            2,
            "unchanged debt must not spin recovery"
        );
    }
}
