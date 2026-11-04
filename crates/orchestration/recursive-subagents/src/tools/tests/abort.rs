//! A caller aborted mid-mutation or mid-settle never strands an effect: the
//! seal it started still records its candidate (spec 03 section 16).

use super::*;
use crate::hub::ProductTopologyRuntime;
use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
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

/// From now on the `nth` VCS call (counting from one) reports itself on the
/// returned receiver and blocks until the returned sender drops; every other
/// call passes.
fn gate_vcs_call(vcs: &FakeVcs, nth: usize) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entering, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let gate = Mutex::new((0_usize, entering, released));
    vcs.watch_lock(move || {
        let mut gate = gate.lock().unwrap();
        gate.0 += 1;
        if gate.0 == nth {
            let _ = gate.1.send(());
            let _ = gate.2.recv();
        }
        false
    });
    (entered, release)
}

/// The settle's port calls are awaited one after another: a caller aborted
/// after the first seal answered, while the second is still running, leaves
/// the rest of the batch to run and both candidates recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settle_aborted_between_two_seals_records_both_candidates() {
    let (_root, vcs, mut tree, first) = fake_child_tree("abort-between-seals");
    let second = tree
        .spawn_agent("root", &mint_spawn_request(), None)
        .unwrap()
        .agent_id;
    tree.settle_for_test();
    let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
    let saving = driver.clone();
    let (close_first, close_second) = (close(&first), close(&second));
    let (closed, effects) = driver
        .mutate(move |tree| {
            close_first(tree)
                .and_then(|()| close_second(tree))
                .and_then(|()| saving.save(tree))
        })
        .await
        .unwrap();
    closed.unwrap();
    assert_eq!(effects.seals.len(), 2);
    let (entered, release) = gate_vcs_call(&vcs, 2);
    let settling = driver.clone();
    let caller = tokio::spawn(async move { settling.settle(effects).await });
    until_entered(entered).await;
    // The first seal answered; the second is in flight.
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    abort(caller).await;
    drop(release);
    assert_sealed(&driver, &first, NodeStatus::Closed).await;
    assert_sealed(&driver, &second, NodeStatus::Closed).await;
}

/// A port whose mint yields: it reports its start, then stays `Pending` until
/// a permit is released, and answers as the [`FakeVcs`] it wraps. Every other
/// question is the fake's own answer.
struct YieldVcs {
    inner: FakeVcs,
    entered: tokio::sync::Notify,
    gate: tokio::sync::Semaphore,
}

impl YieldVcs {
    fn new() -> Self {
        Self {
            inner: FakeVcs::new(),
            entered: tokio::sync::Notify::new(),
            gate: tokio::sync::Semaphore::new(0),
        }
    }
}

mod yield_vcs {
    use super::YieldVcs;
    use plakous_workspace::occupancy_vcs::{
        AdoptFailure, AdoptLanding, AdoptRequest, MintRequest, MintedCopy, ParentBehindCache,
        RefreshApplied, RefreshPlan, RefreshPreparation, RefreshRequest, VcsError, VcsFuture,
        VcsOps, VcsSnapshot,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    impl VcsOps for YieldVcs {
        fn prepare_refresh<'a>(
            &'a self,
            request: &'a RefreshRequest,
        ) -> VcsFuture<'a, Result<RefreshPreparation, VcsError>> {
            self.inner.prepare_refresh(request)
        }
        fn apply_refresh<'a>(
            &'a self,
            plan: &'a RefreshPlan,
            recover: bool,
        ) -> VcsFuture<'a, Result<RefreshApplied, VcsError>> {
            self.inner.apply_refresh(plan, recover)
        }
        fn check_reclaimable<'a>(
            &'a self,
            path: &'a Path,
            sealed: Option<&'a VcsSnapshot>,
        ) -> VcsFuture<'a, Result<(), VcsError>> {
            self.inner.check_reclaimable(path, sealed)
        }
        fn trim_disposable_target<'a>(
            &'a self,
            path: &'a Path,
        ) -> VcsFuture<'a, Result<bool, VcsError>> {
            self.inner.trim_disposable_target(path)
        }
        fn path_exists<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Result<bool, VcsError>> {
            self.inner.path_exists(path)
        }
        fn jj_copy_dirs<'a>(
            &'a self,
            launch: &'a Path,
        ) -> VcsFuture<'a, Result<BTreeSet<String>, VcsError>> {
            self.inner.jj_copy_dirs(launch)
        }
        fn mint_from<'a>(
            &'a self,
            request: &'a MintRequest<'_>,
        ) -> VcsFuture<'a, Result<MintedCopy, VcsError>> {
            Box::pin(async move {
                self.entered.notify_one();
                let _permit = self.gate.acquire().await;
                self.inner.mint_from(request).await
            })
        }
        fn history_left_base<'a>(
            &'a self,
            path: &'a Path,
            base: &'a str,
            sealed: &'a str,
        ) -> VcsFuture<'a, Result<bool, VcsError>> {
            self.inner.history_left_base(path, base, sealed)
        }
        fn seal<'a>(
            &'a self,
            child_path: &'a Path,
            namespace: &'a str,
            candidate_id: &'a str,
            base_commit: &'a str,
        ) -> VcsFuture<'a, Result<VcsSnapshot, VcsError>> {
            self.inner
                .seal(child_path, namespace, candidate_id, base_commit)
        }
        fn unpin<'a>(
            &'a self,
            child_path: &'a Path,
            namespace: &'a str,
            candidate_id: &'a str,
        ) -> VcsFuture<'a, Result<(), VcsError>> {
            self.inner.unpin(child_path, namespace, candidate_id)
        }
        fn restore_copy_base<'a>(
            &'a self,
            path: &'a Path,
            namespace: &'a str,
            agent: &'a str,
            base: &'a str,
        ) -> VcsFuture<'a, Result<(), VcsError>> {
            self.inner.restore_copy_base(path, namespace, agent, base)
        }
        fn adopt<'a>(
            &'a self,
            request: &'a AdoptRequest<'_>,
        ) -> VcsFuture<'a, Result<AdoptLanding, AdoptFailure>> {
            self.inner.adopt(request)
        }
        fn run_mint_hook<'a>(
            &'a self,
            cwd: &'a Path,
            command: &'a str,
            env: &'a BTreeMap<String, String>,
        ) -> VcsFuture<'a, Result<(), VcsError>> {
            self.inner.run_mint_hook(cwd, command, env)
        }
        fn unmint<'a>(
            &'a self,
            launch: &'a Path,
            path: &'a Path,
            namespace: &'a str,
        ) -> VcsFuture<'a, Result<(), VcsError>> {
            self.inner.unmint(launch, path, namespace)
        }
        fn dirty<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, bool> {
            self.inner.dirty(path)
        }
        fn ahead_behind<'a>(
            &'a self,
            child: &'a Path,
            parent: &'a Path,
            base: Option<&'a str>,
        ) -> VcsFuture<'a, Option<(u64, u64)>> {
            self.inner.ahead_behind(child, parent, base)
        }
        fn ahead_behind_cached<'a>(
            &'a self,
            child: &'a Path,
            parent: &'a Path,
            base: Option<&'a str>,
            cache: &'a mut ParentBehindCache,
        ) -> VcsFuture<'a, Option<(u64, u64)>> {
            self.inner.ahead_behind_cached(child, parent, base, cache)
        }
        fn label<'a>(&'a self, path: &'a Path) -> VcsFuture<'a, Option<String>> {
            self.inner.label(path)
        }
        fn diverged_from_base<'a>(
            &'a self,
            path: &'a Path,
            base_change_id: Option<&'a str>,
            base_commit: &'a str,
        ) -> VcsFuture<'a, Result<bool, VcsError>> {
            self.inner
                .diverged_from_base(path, base_change_id, base_commit)
        }
        fn copy_is_over_candidate<'a>(
            &'a self,
            launch: &'a Path,
            copy_id: &'a str,
            candidate: &'a VcsSnapshot,
        ) -> VcsFuture<'a, Result<bool, VcsError>> {
            self.inner
                .copy_is_over_candidate(launch, copy_id, candidate)
        }
    }
}

/// A spawn whose caller is dropped while its mint is still pending neither
/// strands its reservation nor loses the copy: the mint and the writer that
/// commits it run as one release owner, so the child is recorded with its
/// workspace and its receipt answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawn_dropped_mid_mint_still_commits_its_child() {
    let root = tempfile::tempdir().unwrap();
    let vcs = Arc::new(YieldVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        "abort-spawn",
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
    let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
    let spawning = driver.clone();
    let caller = tokio::spawn(async move {
        spawn_tool(spawning, None)
            .spawn_with(
                "op-abort-spawn".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
    });
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(Duration::from_secs(30), vcs.entered.notified())
        .await
        .expect("the mint started");
    abort(caller).await;
    vcs.gate.add_permits(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (nodes, answered) = driver
            .read(|tree| {
                (
                    tree.nodes().len(),
                    tree.operations().values().any(|operation| {
                        operation.tool_name == "spawn_agent"
                            && operation.terminal_response.is_some()
                    }),
                )
            })
            .unwrap();
        if nodes == 2 && answered {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reservation was stranded: {nodes} nodes, receipt answered {answered}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child = driver
        .read(|tree| tree.nodes().keys().find(|id| *id != "root").cloned())
        .unwrap()
        .unwrap();
    assert!(
        driver
            .read(|tree| {
                let workspace = &tree.node(&child).unwrap().workspace_id;
                tree.workspace(workspace).is_some()
            })
            .unwrap(),
        "the minted copy is the child's workspace"
    );
}
