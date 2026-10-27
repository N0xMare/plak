//! Admission reads the installed state, never tree-mutex contention.

#![allow(clippy::unwrap_used, reason = "test-owned runtime and rendezvous")]

use super::*;
use crate::tree::TreeAdmissionState;
use std::sync::mpsc;

fn with_tree_locked(runtime: &Arc<ProductTopologyRuntime>, check: impl FnOnce()) {
    std::thread::scope(|scope| {
        let (locked, ready) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let runtime = runtime.clone();
        let holder = scope.spawn(move || {
            runtime
                .read(|_| {
                    locked.send(()).unwrap();
                    let _ = released.recv();
                })
                .unwrap();
        });
        ready.recv_timeout(Duration::from_secs(30)).unwrap();
        check();
        drop(release);
        holder.join().unwrap();
    });
}

#[tokio::test]
async fn live_admission_readers_do_not_mistake_tree_contention_for_pause() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let runtime = insert_root(
        id.clone(),
        test_support::bootstrap(&id, root.path().to_owned()),
        None,
    )
    .unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
        assert!(!is_restored_paused(&id));
    });
    force_release(&id).await;
}

#[tokio::test]
async fn paused_admission_is_visible_under_contention_and_tracks_resume() {
    let root = tempfile::tempdir().unwrap();
    let id = test_support::unique_tree_id();
    let runtime = insert_root(
        id.clone(),
        test_support::bootstrap(&id, root.path().to_owned()),
        None,
    )
    .unwrap();
    // Startup legitimately lifts a singleton's restored pause. Install the
    // paused state after startup so this test exercises the attached reader.
    runtime.driver().lock().unwrap().state = TreeAdmissionState::RestoredPaused;
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
        assert!(is_restored_paused(&id));
    });
    // Internal guarded changes and detached public mutations must both
    // publish the final state, including a transition back to a real pause.
    runtime.driver().lock().unwrap().state = TreeAdmissionState::Live;
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
    });
    let ((), effects) = runtime
        .driver()
        .mutate(|tree| tree.state = TreeAdmissionState::RestoredPaused)
        .unwrap();
    assert!(effects.is_empty());
    runtime.driver().settle(effects).unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(true));
    });
    let ((), effects) = runtime
        .driver()
        .mutate(|tree| tree.state = TreeAdmissionState::Live)
        .unwrap();
    assert!(effects.is_empty());
    runtime.driver().settle(effects).unwrap();
    with_tree_locked(&runtime, || {
        assert_eq!(hub_tree_is_restored_paused(&id), Some(false));
    });
    force_release(&id).await;
}
