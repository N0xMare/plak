use std::collections::BTreeSet;
use std::path::Path;

use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{ClientWindows, Loopback, RunnerIdentity, RunnerOptions, RunnerService};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

use super::*;
use crate::protocol::{SpawnWorkspace, TopologyErrorCode, WriterMode};
use crate::tree::block_on_for_test;
use crate::tree::test_support::*;
use crate::tree::vcs_fixtures::*;

/// A loopback runner over this machine's filesystem, with the tree's
/// version control registered, and the tree's port to it.
struct Runner {
    runtime: tokio::runtime::Runtime,
    _loopback: Loopback,
    vcs: Arc<dyn VcsOps>,
}

fn loopback_runner(root: &Path) -> Runner {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000081".parse().unwrap());
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(scope, root, ".", ExecutionEnvironmentConfig::default())
            .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: VersionTriple::current("stages", "test"),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        RunnerOptions {
            extension: Some(crate::remote_vcs::vcs_runner_extension(
                Arc::new(|_| true),
                None,
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
                version: VersionTriple::current("stages", "test"),
                inspect: false,
            },
            ClientWindows::default(),
        ))
        .unwrap();
    let vcs: Arc<dyn VcsOps> = Arc::new(crate::remote_vcs::RemoteVcs::new(
        loopback.backend().link(),
        "loopback".into(),
        "tree1".into(),
    ));
    Runner {
        runtime,
        _loopback: loopback,
        vcs,
    }
}

fn stage_rows(tree: &ProductAgentTree) -> usize {
    tree.operations()
        .values()
        .filter(|row| row.tool_name == VcsStageIntent::KIND)
        .count()
}

/// The tree shape on a runner: the root and its fork child are placed on
/// it, the child's copy is minted, sealed and adopted through the link, and
/// each queued effect runs as a recorded stage that its answer settles.
#[test]
fn a_fork_child_on_a_runner_mints_seals_and_adopts_through_the_link() {
    let repo = git_repo();
    let runner = loopback_runner(repo.path());
    let _entered = runner.runtime.enter();
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&runner.vcs),
        "tree1",
        "root",
        "owner",
        repo.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.pin_bind_hints_for_test(test_bind_hints());
    assert_eq!(
        tree.node("root").unwrap().placement.runner_id.as_deref(),
        Some("loopback")
    );

    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let node = tree.node(&child.agent_id).unwrap();
    assert_eq!(node.placement.runner_id.as_deref(), Some("loopback"));
    let child_path = node.path.clone();
    assert!(
        child_path.join(".git").exists(),
        "the runner minted the copy"
    );
    std::fs::write(child_path.join("H.txt"), "child work\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();

    let effects = tree.take_effects();
    assert_eq!(effects.seals.len(), 1);
    let plan = tree.record_effect_stages(&effects, "loopback").unwrap();
    assert_eq!(stage_rows(&tree), 1, "the seal is recorded before it runs");
    let stage = tree
        .operations()
        .values()
        .find_map(|row| VcsStageIntent::from_operation(row).ok().flatten())
        .unwrap();
    assert_eq!(stage.stage, VcsStageKind::Seal);
    assert_eq!(stage.class, VcsStageClass::Reproducible);
    assert_eq!(stage.target_runner.as_deref(), Some("loopback"));
    let completed = block_on_for_test(effects.run_staged(Arc::clone(&runner.vcs).as_ref(), plan));
    tree.complete(completed);
    assert_eq!(stage_rows(&tree), 0, "the answer settles the stage");
    tree.settle_for_test();

    let candidate_id = candidate_of(&tree, &child.agent_id);
    let outcome = tree
        .adopt_candidate("root", &adopt_req(&candidate_id), Some("op-adopt"))
        .unwrap();
    assert_eq!(outcome.files_changed, 1);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
        "child work\n"
    );
}

/// A stage whose step lost its outcome settles by its class: a re-runnable
/// one stays recorded and its rerun takes its operation id again, an
/// ambiguous one faults its node, and a crash's leftover stages settle the
/// same way on reopen.
#[test]
fn an_unknown_stage_settles_by_its_class() {
    let (_root, _vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let faults = |tree: &ProductAgentTree| {
        tree.operations()
            .values()
            .filter(|row| row.tool_name == "vcs_fault" && row.caller_agent_id == child.agent_id)
            .count()
    };

    let unmint = local_stage(
        VcsStageKind::Unmint,
        VcsStageClass::Reproducible,
        &child.agent_id,
        "loopback",
    );
    let lost = tree.record_vcs_stage(&unmint).unwrap();
    lost.mark_unknown();
    tree.settle_vcs_stage(&lost);
    assert_eq!((stage_rows(&tree), faults(&tree)), (1, 0));
    let rerun = tree.record_vcs_stage(&unmint).unwrap();
    assert_eq!(rerun.operation_id(), lost.operation_id());
    assert_eq!(rerun.next_step(), format!("{}/0", lost.operation_id()));

    let landing = tree
        .record_vcs_stage(&local_stage(
            VcsStageKind::Mint,
            VcsStageClass::Ambiguous,
            &child.agent_id,
            "loopback",
        ))
        .unwrap();
    landing.mark_unknown();
    tree.settle_vcs_stage(&landing);
    assert_eq!((stage_rows(&tree), faults(&tree)), (1, 1));

    // A stage a crashed process left recorded settles on reopen: the
    // re-runnable one stays for its rerun, the ambiguous one faults.
    tree.record_vcs_stage(&local_stage(
        VcsStageKind::Mint,
        VcsStageClass::Ambiguous,
        &child.agent_id,
        "loopback",
    ))
    .unwrap();
    assert_eq!(stage_rows(&tree), 2);
    tree.settle_unfinished_vcs_stages();
    assert_eq!((stage_rows(&tree), faults(&tree)), (1, 2));
    // The settled answer removes it.
    tree.settle_vcs_stage(&rerun);
    assert_eq!(stage_rows(&tree), 0);
}

/// A root stage whose outcome is unknown faults the root, naming its
/// checkout, since nothing recovers the root itself.
#[test]
fn an_ambiguous_root_stage_faults_the_root() {
    let (_root, _vcs, mut tree) = fake_tree();
    let landing = tree
        .record_vcs_stage(&local_stage(
            VcsStageKind::Mint,
            VcsStageClass::Ambiguous,
            "root",
            "loopback",
        ))
        .unwrap();
    landing.mark_unknown();
    tree.settle_vcs_stage(&landing);
    let fault = tree
        .operations()
        .values()
        .find(|row| row.tool_name == "vcs_fault" && row.caller_agent_id == "root");
    assert!(fault.is_some(), "the root records its fault");
}

/// A host that lost the link mid-step reruns the reproducible stage under
/// its first key: the runner answers the rerun when the lost step ends
/// instead of running it a second time beside it.
#[test]
fn a_rerun_stage_waits_for_the_lost_step_instead_of_overlapping_it() {
    let repo = git_repo();
    let runner = loopback_runner(repo.path());
    let _entered = runner.runtime.enter();
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&runner.vcs),
        "tree1",
        "root",
        "owner",
        repo.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let intent = local_stage(
        VcsStageKind::Unmint,
        VcsStageClass::Reproducible,
        "root",
        "loopback",
    );
    let step = |stage: Arc<VcsStage>| {
        let (vcs, cwd) = (Arc::clone(&runner.vcs), repo.path().to_owned());
        plakous_workspace::occupancy_vcs::stage::run(Some(stage), async move {
            vcs.run_mint_hook(
                &cwd,
                "echo start >> steps; sleep 1; echo end >> steps",
                &std::collections::BTreeMap::new(),
            )
            .await
        })
    };
    let lost = runner
        .runtime
        .spawn(step(tree.record_vcs_stage(&intent).unwrap()));
    let log = repo.path().join("steps");
    while !std::fs::read_to_string(&log).is_ok_and(|steps| steps.contains("start")) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The host is gone: its reopen keeps the stage for its rerun.
    lost.abort();
    tree.settle_unfinished_vcs_stages();
    assert_eq!(stage_rows(&tree), 1);
    let rerun = tree.record_vcs_stage(&intent).unwrap();
    runner.runtime.block_on(step(Arc::clone(&rerun))).unwrap();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "start\nend\n");
    tree.settle_vcs_stage(&rerun);
    assert_eq!(stage_rows(&tree), 0);
}

/// An `inherit` child runs on its parent's runner; a copy on another runner
/// is refused until copies cross runners. Neither reserves anything.
#[test]
fn a_child_on_another_runner_is_refused_typed() {
    let (_root, _vcs, mut tree) = fake_tree();
    let before = tree.revision;
    let mut inherit = mint_req();
    inherit.workspace = SpawnWorkspace::Inherit;
    let refused = tree
        .spawn_reserve_on(
            "root",
            &inherit,
            None,
            &Ok(BTreeSet::new()),
            Some("elsewhere"),
        )
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::RunnerMismatch);
    let refused = tree
        .spawn_reserve_on(
            "root",
            &mint_req(),
            None,
            &Ok(BTreeSet::new()),
            Some("elsewhere"),
        )
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::CrossRunnerCopyUnavailable);
    assert_eq!(tree.revision, before);
    // The parent's own runner (here the local one) is no refusal.
    assert!(
        tree.spawn_reserve_on("root", &mint_req(), None, &Ok(BTreeSet::new()), None)
            .is_ok()
    );
}
