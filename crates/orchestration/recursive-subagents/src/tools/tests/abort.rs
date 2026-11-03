//! A caller aborted mid-mutation or mid-settle never strands an effect: the
//! seal it started still records its candidate (spec 03 section 16).

use super::*;
use crate::hub::ProductTopologyRuntime;
use plakous_workspace::occupancy_vcs::fake::FakeVcs;
use std::sync::mpsc;
use std::time::Duration;

/// A published tree on a `FakeVcs` with one idle minted child.
fn fake_child_tree(tree_id: &str) -> (tempfile::TempDir, Arc<FakeVcs>, ProductAgentTree, String) {
    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        tree_id,
        "root",
        "owner",
        root.path().to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let child = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap()
        .agent_id;
    tree.settle_for_test();
    (root, vcs, tree, child)
}

/// From now on every VCS call reports itself on the returned receiver and
/// blocks until the returned sender drops.
fn gate_vcs(vcs: &FakeVcs) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entering, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let gate = Mutex::new((entering, released));
    vcs.watch_lock(move || {
        let gate = gate.lock().unwrap();
        let _ = gate.0.send(());
        let _ = gate.1.recv();
        false
    });
    (entered, release)
}

async fn until_entered(entered: mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the gated call started");
}

/// Aborts `caller` and waits until it is gone.
async fn abort<T>(caller: tokio::task::JoinHandle<T>) {
    caller.abort();
    match caller.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the caller finished before its abort"),
    }
}

/// The child's seal completed: it ends `status` with a candidate.
async fn assert_sealed(driver: &TreeDriver, child: &str, expected: NodeStatus) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, candidate) = driver
            .read(|tree| {
                (
                    tree.node(child).unwrap().status,
                    tree.attempt(child).unwrap().candidate_id.clone(),
                )
            })
            .unwrap();
        if status == expected && candidate.is_some() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the seal was stranded: {status:?}, candidate {candidate:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn close(
    child: &str,
) -> impl FnOnce(&mut ProductAgentTree) -> Result<(), TopologyError> + Send + use<> {
    let child = child.to_owned();
    move |tree| {
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: child,
                expected_attempt_id: None,
            },
            None,
        )
        .map(|_| ())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settle_aborted_during_its_seal_records_the_candidate() {
    let (_root, vcs, tree, child) = fake_child_tree("abort-settle");
    let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
    let saving = driver.clone();
    let closing = close(&child);
    let (closed, effects) = driver
        .mutate(move |tree| closing(tree).and_then(|()| saving.save(tree)))
        .await
        .unwrap();
    closed.unwrap();
    let (entered, release) = gate_vcs(&vcs);
    let settling = driver.clone();
    let caller = tokio::spawn(async move { settling.settle(effects).await });
    until_entered(entered).await;
    abort(caller).await;
    drop(release);
    assert_sealed(&driver, &child, NodeStatus::Closed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mutate_and_settle_aborted_during_its_seal_records_the_candidate() {
    let (_root, vcs, tree, child) = fake_child_tree("abort-mutate-and-settle");
    let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
    let (entered, release) = gate_vcs(&vcs);
    let settling = driver.clone();
    let closing = close(&child);
    let caller = tokio::spawn(async move { settling.mutate_and_settle(closing).await });
    until_entered(entered).await;
    abort(caller).await;
    drop(release);
    assert_sealed(&driver, &child, NodeStatus::Closed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mutation_aborted_inside_its_closure_settles_its_effects() {
    let (_root, _vcs, tree, child) = fake_child_tree("abort-mutate");
    let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
    let (entering, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let mutating = driver.clone();
    let saving = driver.clone();
    let closing = close(&child);
    let caller = tokio::spawn(async move {
        mutating
            .mutate(move |tree| {
                let _ = entering.send(());
                let _ = released.recv();
                closing(tree).and_then(|()| saving.save(tree))
            })
            .await
    });
    until_entered(entered).await;
    abort(caller).await;
    drop(release);
    assert_sealed(&driver, &child, NodeStatus::Closed).await;
}

/// A hub runtime over a [`fake_child_tree`], its child holding a turn owner
/// when `owned`.
fn hub_runtime(
    tree_id: &str,
    owned: bool,
) -> (
    tempfile::TempDir,
    Arc<FakeVcs>,
    Arc<ProductTopologyRuntime>,
    String,
) {
    let (root, vcs, mut tree, child) = fake_child_tree(tree_id);
    if owned {
        tree.reserve_turn_owner(&child);
    }
    let runtime =
        ProductTopologyRuntime::new(tree, Arc::new(MemoryTreeStore::new()), vcs.clone(), None);
    (root, vcs, Arc::new(runtime), child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_mutate_then_emit_aborted_during_its_seal_records_the_candidate() {
    let (_root, vcs, runtime, child) = hub_runtime("abort-mutate-then-emit", false);
    let (entered, release) = gate_vcs(&vcs);
    let emitting = Arc::clone(&runtime);
    let closing = close(&child);
    let caller = tokio::spawn(async move {
        emitting
            .mutate_then_emit(false, move |tree| {
                closing(tree).map_err(|error| error.message)
            })
            .await
    });
    until_entered(entered).await;
    abort(caller).await;
    drop(release);
    assert_sealed(runtime.driver(), &child, NodeStatus::Closed).await;
}

/// A host cancellation seals the copy and ends the child `failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_record_then_apply_aborted_during_its_seal_records_the_candidate() {
    let (_root, vcs, runtime, child) = hub_runtime("abort-record-then-apply", true);
    let (entered, release) = gate_vcs(&vcs);
    let recording = Arc::clone(&runtime);
    let cancelled = child.clone();
    let caller = tokio::spawn(async move {
        recording
            .record_then_apply(&cancelled.clone(), move |tree| {
                tree.record_host_cancellation(&cancelled)
            })
            .await
    });
    until_entered(entered).await;
    abort(caller).await;
    drop(release);
    assert_sealed(runtime.driver(), &child, NodeStatus::Failed).await;
}
