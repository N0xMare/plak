use plakous_hosted::agent_tree::{
    CustodyClaim, CustodyHost, HostInstance, TreeCustody as _, TreeStore as _, custody_host,
};

use super::release;
use crate::home::paths::PlakPaths;

fn holder(host_name: &str) -> CustodyHost {
    CustodyHost {
        host_id: "local".to_owned(),
        instance: HostInstance::parse("31-0000000000000031").unwrap(),
        binary: Some("0123456789ab".to_owned()),
        host_name: host_name.to_owned(),
    }
}

fn run(
    paths: &PlakPaths,
    tree_id: &str,
) -> (Result<(), plakous_hosted::CliFailure>, String, String) {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let result = release(paths, tree_id, &mut stdout, &mut stderr);
    (
        result,
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

/// A holder on another machine cannot be proven gone: the break-glass
/// releases its record with a warning, keeping its fence and holder, and its
/// claim writes nothing afterwards; this machine's next take supersedes it.
#[test]
fn the_break_glass_releases_another_machines_holder_with_a_warning() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let store = crate::store::tree::open_tree_store(&paths).unwrap();
    let elsewhere = holder("another-machine");
    let claim = store.take_custody("tree-b", &elsewhere, None).unwrap();
    // A held lock of the same instance here says nothing about another
    // machine's process.
    let directory = store.liveness_dir().unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    let same_instance =
        std::fs::File::create(directory.join(format!("{}.lock", elsewhere.instance))).unwrap();
    same_instance.lock().unwrap();

    let (result, stdout, stderr) = run(&paths, "tree-b");
    result.unwrap();
    assert!(
        stderr.starts_with(
            "plak: warning: forcing the release of agent tree tree-b from local on another-machine"
        ),
        "{stderr}"
    );
    assert!(
        stdout.contains("released custody of agent tree tree-b (fence 1"),
        "{stdout}"
    );
    let record = store.custody("tree-b").unwrap().unwrap();
    assert!(record.released);
    assert_eq!((record.fence, &record.holding.host), (1, &elsewhere));
    assert!(
        store.prove_custody(&claim).is_err(),
        "the broken claim no longer holds"
    );

    let taken = store.take_custody("tree-b", custody_host(), None).unwrap();
    assert_eq!(taken.fence, 2);
    let refused = store.save_in_custody(
        &plakous_hosted::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            "tree-b",
            "root",
            "owner",
            root.path().to_path_buf(),
            "tuple",
            plakous_hosted::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap(),
    );
    assert!(
        refused.is_ok(),
        "this process now holds the tree: {refused:?}"
    );
    let stale = CustodyClaim { fence: 1, ..claim };
    assert!(store.prove_custody(&stale).is_err());

    // Nothing is held any more once released twice.
    store.release_custody(&taken).unwrap();
    let (result, stdout, _) = run(&paths, "tree-b");
    result.unwrap();
    assert!(stdout.contains("already released"), "{stdout}");
    let (result, stdout, _) = run(&paths, "never-taken");
    result.unwrap();
    assert!(stdout.contains("no custody record"), "{stdout}");
}

/// A holder proven to be running on this machine is never forced: quitting
/// it is the remedy, and its record is left as it was.
#[test]
fn the_break_glass_refuses_a_holder_running_here() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let store = crate::store::tree::open_tree_store(&paths).unwrap();
    let hold = plakous_hosted::agent_tree::acquire_custody(&store, "tree-h").unwrap();

    let (result, _, stderr) = run(&paths, "tree-h");
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 7);
    assert!(
        failure.message().contains("still running on this machine"),
        "{}",
        failure.message()
    );
    assert!(stderr.is_empty(), "no warning without a release");
    store.prove_custody(hold.claim()).unwrap();
    plakous_hosted::agent_tree::release_custody_hold(&store, hold).unwrap();
}

/// A holder on this machine is proven running only while its liveness lock
/// is held: then the break-glass refuses it. Once its lock file is missing it
/// is unknown, not proven running, and is released with the warning.
#[test]
fn the_break_glass_trusts_only_a_held_lock_on_this_machine() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("p"));
    let store = crate::store::tree::open_tree_store(&paths).unwrap();
    let here = holder(&custody_host().host_name);
    store.take_custody("tree-m", &here, None).unwrap();
    let directory = store.liveness_dir().unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{}.lock", here.instance));
    let lock = std::fs::File::create(&path).unwrap();
    lock.lock().unwrap();

    let (result, _, stderr) = run(&paths, "tree-m");
    assert_eq!(result.unwrap_err().exit_code(), 7);
    assert!(stderr.is_empty(), "{stderr}");
    assert!(!store.custody("tree-m").unwrap().unwrap().released);

    std::fs::remove_file(&path).unwrap();
    let (result, stdout, stderr) = run(&paths, "tree-m");
    result.unwrap();
    assert!(
        stderr.starts_with("plak: warning: forcing the release of agent tree tree-m"),
        "{stderr}"
    );
    assert!(stdout.contains("released custody"), "{stdout}");
    assert!(store.custody("tree-m").unwrap().unwrap().released);
    drop(lock);
}
