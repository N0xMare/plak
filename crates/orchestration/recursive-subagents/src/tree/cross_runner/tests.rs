use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{ClientWindows, Loopback, RunnerIdentity, RunnerOptions, RunnerService};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use plakous_workspace::occupancy_vcs::candidate_refs;

use super::*;
use crate::protocol::WriterMode;
use crate::remote_vcs::{RemoteVcs, vcs_runner_extension};
use crate::routed_vcs::RoutedVcs;
use crate::tree::test_support::*;
use crate::tree::vcs_fixtures::*;
use crate::tree::{SpawnAdmission, topology_error_from_vcs};

/// A loopback runner `id` over `root`, taking copies under `copies`.
fn runner(
    runtime: &tokio::runtime::Runtime,
    id: &str,
    root: &Path,
    copies: Option<&Path>,
) -> (Loopback, Arc<dyn VcsOps>) {
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000083".parse().unwrap());
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(scope, root, ".", ExecutionEnvironmentConfig::default())
            .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new(id).unwrap(),
            version: VersionTriple::current("cross-runner", "test"),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        RunnerOptions {
            extension: Some(vcs_runner_extension(
                Arc::new(|_| true),
                copies.map(Path::to_owned),
            )),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let loopback = runtime
        .block_on(Loopback::join(
            service,
            Bind {
                host_id: "host-1".into(),
                host_instance: "instance-1".into(),
                version: VersionTriple::current("cross-runner", "test"),
                inspect: false,
            },
            ClientWindows::default(),
        ))
        .unwrap();
    let port: Arc<dyn VcsOps> = Arc::new(RemoteVcs::new(
        loopback.backend().link(),
        id.into(),
        "tree1".into(),
    ));
    (loopback, port)
}

/// Where a ledger cuts the run, as a crash would: once the `n`th stage is
/// published (`before` its run), or after its run before its answer is
/// recorded.
#[derive(Clone, Copy, Debug)]
struct Cut {
    n: usize,
    before: bool,
}

/// The tree's stage record for a test: each stage recorded in the tree,
/// in order, until the cut.
struct Ledger {
    tree: Arc<Mutex<ProductAgentTree>>,
    cut: Option<Cut>,
    seen: Mutex<Vec<VcsStageIntent>>,
    crashed: std::sync::atomic::AtomicBool,
}

impl Ledger {
    fn new(tree: &Arc<Mutex<ProductAgentTree>>, cut: Option<Cut>) -> Arc<Self> {
        Arc::new(Self {
            tree: Arc::clone(tree),
            cut,
            seen: Mutex::new(Vec::new()),
            crashed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn kinds(&self) -> Vec<(VcsStageKind, VcsStageClass)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|intent| (intent.stage, intent.class))
            .collect()
    }

    fn crashed(&self) -> bool {
        self.crashed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn crash() -> VcsError {
    VcsError::mint_unavailable("the process ended")
}

impl StageLedger for Ledger {
    fn record(&self, intent: VcsStageIntent) -> VcsFuture<'_, Result<Arc<VcsStage>, VcsError>> {
        Box::pin(async move {
            if self.crashed() {
                return Err(crash());
            }
            let stage = self.tree.lock().unwrap().record_vcs_stage(&intent);
            let n = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(intent);
                seen.len()
            };
            if let Some(cut) = self.cut
                && cut.n == n
                && cut.before
            {
                self.crashed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Err(crash());
            }
            stage.map_err(|error| VcsError::mint_unavailable(error.message))
        })
    }

    fn settle(&self, stage: Arc<VcsStage>) -> VcsFuture<'_, ()> {
        Box::pin(async move {
            let n = self.seen.lock().unwrap().len();
            if self.cut.is_some_and(|cut| cut.n == n && !cut.before) {
                self.crashed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if !self.crashed() {
                self.tree.lock().unwrap().settle_vcs_stage(&stage);
            }
        })
    }

    fn conflicting(
        &self,
        agent: &str,
        attempt: Option<AttemptId>,
        candidate: &str,
    ) -> VcsFuture<'_, ()> {
        let (agent, candidate) = (agent.to_owned(), candidate.to_owned());
        Box::pin(async move {
            self.tree
                .lock()
                .unwrap()
                .fault_carried_candidate(&agent, &candidate, attempt);
        })
    }

    fn carried(&self, runner: &str, commit: &str) -> VcsFuture<'_, ()> {
        let (runner, commit) = (runner.to_owned(), commit.to_owned());
        Box::pin(async move {
            if !self.crashed() {
                self.tree
                    .lock()
                    .unwrap()
                    .record_runner_ancestor(&runner, &commit);
            }
        })
    }
}

/// Two loopback runners: the tree launches on `runner-a` over a git
/// checkout with uncommitted work, and `runner-b` takes copies.
struct World {
    runtime: tokio::runtime::Runtime,
    _runners: (Loopback, Loopback, Loopback),
    repo: tempfile::TempDir,
    _b_root: tempfile::TempDir,
    copies: tempfile::TempDir,
    vcs: Arc<dyn VcsOps>,
    tree: Arc<Mutex<ProductAgentTree>>,
}

fn world() -> World {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let repo = git_repo();
    std::fs::write(repo.path().join("A.txt"), "parent work\n").unwrap();
    let b_root = tempdir().unwrap();
    let copies = tempdir().unwrap();
    let (a, home) = runner(&runtime, "runner-a", repo.path(), None);
    let (b, other) = runner(&runtime, "runner-b", b_root.path(), Some(copies.path()));
    // A third runner keeps its copies under the same root, as like
    // machines would.
    let (c, third) = runner(&runtime, "runner-c", b_root.path(), Some(copies.path()));
    let vcs: Arc<dyn VcsOps> =
        Arc::new(runtime.block_on(RoutedVcs::new(home, "tree1", vec![other, third])));
    let tree = {
        let _entered = runtime.enter();
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            Arc::clone(&vcs),
            "tree1",
            "root",
            "owner",
            repo.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.pin_bind_hints_for_test(test_bind_hints());
        tree
    };
    World {
        runtime,
        _runners: (a, b, c),
        repo,
        _b_root: b_root,
        copies,
        vcs,
        tree: Arc::new(Mutex::new(tree)),
    }
}

impl World {
    /// Spawns a fork child of the root on `runner-b` under `ledger`, as the
    /// spawn tool does: reserve, record the mint stage, mint, commit.
    fn spawn_across(&self, ledger: &Arc<Ledger>) -> Result<String, VcsError> {
        self.spawn_on(ledger, "runner-b")
    }

    /// [`Self::spawn_across`] onto `target`.
    fn spawn_on(&self, ledger: &Arc<Ledger>, target: &str) -> Result<String, VcsError> {
        let admission = self
            .tree
            .lock()
            .unwrap()
            .spawn_reserve_on(
                "root",
                &mint_req(),
                None,
                &Ok(BTreeSet::new()),
                Some(target),
            )
            .unwrap();
        let SpawnAdmission::Reserved(reservation, Some(ticket)) = admission else {
            panic!("a fork child owes a mint");
        };
        let stage = self
            .tree
            .lock()
            .unwrap()
            .record_mint_stage(&ticket, None, "runner-a")
            .unwrap();
        let recorder: Arc<dyn StageLedger> = Arc::clone(ledger) as _;
        let minted = self.runtime.block_on(with_ledger(
            recorder,
            stage::run(Some(Arc::clone(&stage)), ticket.mint(self.vcs.as_ref())),
        ));
        // A process that ended after its last stage ran never records the
        // mint's answer either.
        let minted = minted.and_then(|minted| {
            if ledger.crashed() {
                Err(crash())
            } else {
                Ok(minted)
            }
        });
        let mut tree = self.tree.lock().unwrap();
        match minted {
            Ok(minted) => {
                tree.settle_vcs_stage(&stage);
                Ok(tree
                    .spawn_commit(*reservation, Some(minted))
                    .unwrap()
                    .agent_id)
            }
            Err(error) => {
                // The process ended: its reservation is gone with it, and
                // the reopened tree settles what it left recorded.
                drop(tree.spawn_abort(*reservation, topology_error_from_vcs(error.clone())));
                tree.settle_unfinished_vcs_stages();
                Err(error)
            }
        }
    }

    /// Seals `child`'s copy and carries the candidate back under
    /// `ledger`. A cut ends the process: the reopened tree settles what it
    /// left recorded, and the same seal runs again.
    fn seal(&self, ticket: &SealTicket, ledger: &Arc<Ledger>) -> Result<VcsSnapshot, VcsError> {
        let recorder: Arc<dyn StageLedger> = Arc::clone(ledger) as _;
        // The seal on the child's runner runs as the settle's own stage,
        // which this test leaves out.
        let sealed = self.runtime.block_on(with_ledger(
            recorder,
            stage::unstaged("the test's seal", ticket.seal(self.vcs.as_ref())),
        ));
        let sealed = sealed.and_then(|sealed| {
            if ledger.crashed() {
                Err(crash())
            } else {
                Ok(sealed)
            }
        });
        if sealed.is_err() {
            self.tree.lock().unwrap().settle_unfinished_vcs_stages();
        }
        sealed.map(|sealed| sealed.expect("the child changed its copy"))
    }

    fn faults(&self) -> usize {
        self.tree
            .lock()
            .unwrap()
            .operations()
            .values()
            .filter(|row| row.tool_name == "vcs_fault")
            .count()
    }

    fn launch_ref(&self, reference: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", reference])
            .current_dir(self.repo.path())
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

fn seal_ticket(world: &World, child: &str) -> SealTicket {
    let mut tree = world.tree.lock().unwrap();
    std::fs::write(tree.node(child).unwrap().path.join("B.txt"), "child work\n").unwrap();
    tree.record_turn_completed(child).unwrap();
    let mut effects = tree.take_effects();
    assert_eq!(effects.seals.len(), 1);
    effects.seals.remove(0)
}

const MINT_STAGES: [(VcsStageKind, VcsStageClass); 4] = [
    (VcsStageKind::BaseSnapshot, VcsStageClass::Reproducible),
    (VcsStageKind::BundleCreate, VcsStageClass::Reproducible),
    (VcsStageKind::Relay, VcsStageClass::Reproducible),
    (VcsStageKind::BundleApply, VcsStageClass::Reproducible),
];

const SEAL_BACK_STAGES: [(VcsStageKind, VcsStageClass); 3] = [
    (VcsStageKind::BundleCreate, VcsStageClass::Reproducible),
    (VcsStageKind::Relay, VcsStageClass::Reproducible),
    (VcsStageKind::BundleApply, VcsStageClass::Verifiable),
];

/// A fork child placed on another runner is minted there from a bundle of
/// its parent's live state, its candidate comes back into the parent's
/// checkout as a verifiable apply, and the parent adopts it as any other.
#[test]
fn a_fork_child_crosses_runners_and_its_candidate_comes_back() {
    let world = world();
    let ledger = Ledger::new(&world.tree, None);
    let child = world.spawn_across(&ledger).unwrap();
    assert_eq!(ledger.kinds(), MINT_STAGES);
    {
        let tree = world.tree.lock().unwrap();
        let node = tree.node(&child).unwrap();
        assert_eq!(node.placement.runner_id.as_deref(), Some("runner-b"));
        assert!(
            node.path
                .starts_with(world.copies.path().join("runner-b").join("tree1")),
            "the copy is in the tree's mirror on runner-b"
        );
        assert_eq!(
            std::fs::read_to_string(node.path.join("A.txt")).unwrap(),
            "parent work\n",
            "the parent's uncommitted work travelled"
        );
        let base = tree
            .workspace(&node.workspace_id)
            .and_then(|row| row.base_commit.clone())
            .unwrap();
        assert_eq!(tree.tree_runner_ancestors("runner-b"), vec![base]);
        assert_eq!(tree.tree_runner_ancestors("runner-a"), Vec::<String>::new());
    }

    let ticket = seal_ticket(&world, &child);
    let back = Ledger::new(&world.tree, None);
    let sealed = world.seal(&ticket, &back).unwrap();
    assert_eq!(back.kinds(), SEAL_BACK_STAGES);
    let refs = candidate_refs(&ticket.namespace, ticket.candidate_id.as_str());
    assert_eq!(world.launch_ref(&refs[0]), Some(sealed.commit.clone()));
    assert_eq!(world.launch_ref(&refs[1]), Some(ticket.base_commit.clone()));
    // Done already: carried again, it checks and runs nothing.
    let again = Ledger::new(&world.tree, None);
    world.seal(&ticket, &again).unwrap();
    assert_eq!(again.kinds(), Vec::new());

    let _entered = world.runtime.enter();
    let mut tree = world.tree.lock().unwrap();
    assert!(tree.finish_seal_with(ticket, Ok(Some(sealed))).is_some());
    tree.settle_for_test();
    let candidate = candidate_of(&tree, &child);
    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate), Some("op-adopt"))
        .unwrap();
    assert!(outcome.files_changed >= 1);
    assert_eq!(
        std::fs::read_to_string(world.repo.path().join("B.txt")).unwrap(),
        "child work\n"
    );
    drop(tree);
    assert_eq!(world.faults(), 0);
}

/// The crash windows of a cross-runner fork (strict shape
/// `cross_runner_fork_stages`): the process ends once each mint and
/// seal-back stage is published, and again once each has run before its
/// answer is recorded. The reopened tree settles what was left without a
/// fault, and the spawn or seal run again converges: the copy on the other
/// runner holds the parent's work, and the parent's checkout names the
/// sealed candidate.
#[test]
fn cross_runner_fork_stages() {
    for n in 1..=MINT_STAGES.len() {
        for before in [true, false] {
            let world = world();
            let cut = Ledger::new(&world.tree, Some(Cut { n, before }));
            assert!(world.spawn_across(&cut).is_err(), "cut {n} {before}");
            assert_eq!(cut.kinds(), MINT_STAGES[..n], "cut {n} {before}");
            let child = world.spawn_across(&Ledger::new(&world.tree, None)).unwrap();
            let tree = world.tree.lock().unwrap();
            assert_eq!(
                std::fs::read_to_string(tree.node(&child).unwrap().path.join("A.txt")).unwrap(),
                "parent work\n"
            );
            assert_eq!(
                tree.operations()
                    .values()
                    .filter(|row| row.tool_name == VcsStageIntent::KIND)
                    .count(),
                0,
                "cut {n} {before}: nothing of the lost spawn stays recorded"
            );
            drop(tree);
            assert_eq!(world.faults(), 0);
        }
    }
    for n in 1..=SEAL_BACK_STAGES.len() {
        for before in [true, false] {
            let world = world();
            let child = world.spawn_across(&Ledger::new(&world.tree, None)).unwrap();
            let ticket = seal_ticket(&world, &child);
            let cut = Ledger::new(&world.tree, Some(Cut { n, before }));
            assert!(world.seal(&ticket, &cut).is_err(), "cut {n} {before}");
            assert_eq!(cut.kinds(), SEAL_BACK_STAGES[..n], "cut {n} {before}");
            let sealed = world
                .seal(&ticket, &Ledger::new(&world.tree, None))
                .unwrap();
            let refs = candidate_refs(&ticket.namespace, ticket.candidate_id.as_str());
            assert_eq!(world.launch_ref(&refs[0]), Some(sealed.commit.clone()));
            let mut tree = world.tree.lock().unwrap();
            // The revision recording the candidate settles its carry: what
            // a lost run left may be re-run, never checked or faulted.
            assert!(tree.finish_seal_with(ticket, Ok(Some(sealed))).is_some());
            for row in tree
                .operations()
                .values()
                .filter(|row| row.tool_name == VcsStageIntent::KIND)
            {
                let left = VcsStageIntent::from_operation(row).unwrap().unwrap();
                assert_eq!(left.agent_id, child, "cut {n} {before}");
                assert_eq!(left.class, VcsStageClass::Reproducible, "cut {n} {before}");
            }
            drop(tree);
            assert_eq!(world.faults(), 0, "cut {n} {before}");
        }
    }
}

/// Runners that share a copies root keep apart mirrors, each keyed by its
/// runner, and every later call about a copy goes to the runner that holds
/// it.
#[test]
fn runners_sharing_a_copies_root_keep_apart_mirrors() {
    let world = world();
    let on_b = world
        .spawn_on(&Ledger::new(&world.tree, None), "runner-b")
        .unwrap();
    let on_c = world
        .spawn_on(&Ledger::new(&world.tree, None), "runner-c")
        .unwrap();
    let tree = world.tree.lock().unwrap();
    for (child, runner) in [(&on_b, "runner-b"), (&on_c, "runner-c")] {
        let node = tree.node(child).unwrap();
        assert_eq!(node.placement.runner_id.as_deref(), Some(runner));
        assert!(
            node.path
                .starts_with(world.copies.path().join(runner).join("tree1")),
            "{runner}"
        );
        let other = if runner == "runner-b" {
            "runner-c"
        } else {
            "runner-b"
        };
        // Routed to the runner that holds the copy, which answers for it.
        let routed = world
            .runtime
            .block_on(world.vcs.ref_commit(&node.path, "HEAD"))
            .unwrap();
        assert!(routed.is_some(), "{runner}");
        assert!(world.vcs.transfer_port(other).is_some());
    }
}

/// A seal-back never moves a candidate reference the parent's checkout
/// already holds at another commit: the node records a `vcs_fault`, and the
/// reference stays.
#[test]
fn a_seal_back_over_another_commit_faults_and_moves_nothing() {
    let world = world();
    let child = world.spawn_across(&Ledger::new(&world.tree, None)).unwrap();
    let ticket = seal_ticket(&world, &child);
    let refs = candidate_refs(&ticket.namespace, ticket.candidate_id.as_str());
    let seed = world.launch_ref("HEAD").unwrap();
    assert!(
        Command::new("git")
            .args(["update-ref", &refs[0], &seed])
            .current_dir(world.repo.path())
            .status()
            .unwrap()
            .success()
    );
    let refused = world
        .seal(&ticket, &Ledger::new(&world.tree, None))
        .unwrap_err();
    assert_eq!(refused.kind, VcsErrorKind::RecoveryRequired);
    assert_eq!(world.launch_ref(&refs[0]), Some(seed));
    assert_eq!(world.faults(), 1);
}
