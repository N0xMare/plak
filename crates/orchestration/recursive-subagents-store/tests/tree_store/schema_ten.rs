//! Tree schema 10 in `subagents.sqlite3`: every new row round-trips through
//! both save paths, incoherent epoch rows are refused on load, and custody
//! is one fenced claim per tree, also between processes.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use plakous_recursive_subagents::epoch::OrchestrationSnapshot;
use plakous_recursive_subagents::protocol::{
    AttemptId, CandidateId, SpawnAgentRequest, WriterMode,
};
use plakous_recursive_subagents::tree::{
    AttemptFailure, ContinuationAnchor, ContinuationCause, ContinuationDecision,
    ContinuationIntent, CustodyClaim, CustodyError, CustodyHost, EpochPending, FakeVcs, ForkedFrom,
    HostInstance, NodeLineId, NodePlacement, NodeWorkspace, OperationRecord, PersistedTree,
    ProductAgentTree, TerminalRefusalIntent, TreeCustody, TreeEpochs, TreeStore,
    TreeStoreErrorKind, VcsExpectedOutcome, VcsStageClass, VcsStageIntent, VcsStageKind,
    acquire_custody, custody_host, release_custody_hold,
};
use plakous_recursive_subagents_store::SqliteTreeStore;

const TREE: &str = "schema-ten";
const LINE: &str = "0190c3a1-5b7e-7c2d-9f00-0123456789ab";
const CONTENDER_ENV: &str = "PLAK_TREE_CUSTODY_CONTENDER";
const HOLDER_ENV: &str = "PLAK_TREE_CUSTODY_HOLDER";

fn spawned(root: &Path) -> (ProductAgentTree, String) {
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        TREE,
        "root",
        "owner",
        root.to_owned(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    (tree, child)
}

/// One row of every intent kind schema 10 defines, for `child` and the root.
fn schema_ten_intents(child: &str, successor: &OrchestrationSnapshot) -> Vec<OperationRecord> {
    let attempt = AttemptId::from_persisted(format!("{child}-a1"));
    vec![
        ContinuationIntent {
            attempt_id: Some(attempt.clone()),
            anchor: ContinuationAnchor::Turn {
                turn_id: "turn-1".to_owned(),
            },
            host_generation: 2,
            cause: ContinuationCause::OwnerLost,
            round: 1,
            decided_at_ms: 11,
            decision: ContinuationDecision::NewTurn {
                command_id: "continue-1".to_owned(),
                withdrawn: vec!["start-2".to_owned(), "start-3".to_owned()],
            },
        }
        .to_operation(child, None)
        .unwrap(),
        ContinuationIntent {
            attempt_id: None,
            anchor: ContinuationAnchor::Command {
                command_id: "root-cmd".to_owned(),
            },
            host_generation: 1,
            cause: ContinuationCause::Transient,
            round: 1,
            decided_at_ms: 12,
            decision: ContinuationDecision::Resubmit {
                command_id: "root-cmd".to_owned(),
            },
        }
        .to_operation("root", None)
        .unwrap(),
        TerminalRefusalIntent {
            attempt_id: attempt.clone(),
            failure: AttemptFailure {
                kind: "session_incompatible".to_owned(),
                message: "created by an incompatible build".to_owned(),
            },
            observed_at_ms: 13,
        }
        .to_operation(child, None)
        .unwrap(),
        VcsStageIntent {
            attempt_id: Some(attempt),
            stage: VcsStageKind::BundleApply,
            class: VcsStageClass::Verifiable,
            agent_id: child.to_owned(),
            candidate_id: Some(CandidateId::from_persisted(format!("{child}-a1-cand"))),
            source_runner: Some("runner-1".to_owned()),
            target_runner: Some("runner-0".to_owned()),
            base_commit: None,
            tip_commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            bundle_digest: Some("ab".repeat(32)),
            expected: Some(VcsExpectedOutcome {
                reference: "refs/plak/candidates/c1-a1".to_owned(),
                commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            }),
        }
        .to_operation("stage-1", child, None)
        .unwrap(),
        EpochPending::of(1, "switch-1", successor)
            .unwrap()
            .to_operation("epoch-1", "root", None)
            .unwrap(),
    ]
}

/// `tree` with every schema-10 row and column filled, as a handle that has
/// published `stored_revision`.
fn with_schema_ten_rows(
    tree: &ProductAgentTree,
    child: &str,
    stored_revision: u64,
) -> ProductAgentTree {
    let rows = tree.to_persisted();
    let rows = rows.rows();
    let mut nodes = rows.nodes().clone();
    let node = nodes.get_mut(child).unwrap();
    node.line_id = Some(NodeLineId::parse(LINE).unwrap());
    node.epoch_no = 1;
    node.placement = NodePlacement {
        runner_id: Some("runner-1".to_owned()),
        workspace: Some(NodeWorkspace {
            root: PathBuf::from("/srv/copies/t/c1"),
            cwd: PathBuf::from("/srv/copies/t/c1/src"),
        }),
    };
    node.parent_composition =
        Some(serde_json::from_value(serde_json::json!("native-ptc")).unwrap());
    let mut operations = rows.operations().clone();
    let mut successor = rows.current_epoch().clone();
    successor.transient_resumes = 2;
    successor.digest = successor.compute_digest();
    for operation in schema_ten_intents(child, &successor) {
        operations.insert(operation.operation_id.clone(), operation);
    }
    let mut tree = PersistedTree::from_rows(
        rows.tree_id.clone(),
        rows.root_agent_id.clone(),
        rows.revision,
        rows.host_generation,
        rows.state,
        rows.writer_mode,
        rows.selected_tuple.clone(),
        rows.launch_checkout.clone(),
        rows.current_epoch().clone(),
        nodes,
        rows.grants().clone(),
        rows.attempts().clone(),
        rows.candidates().clone(),
        rows.workspaces().clone(),
        rows.messages().clone(),
        operations,
    )
    .with_feed_rows(rows.commands().clone(), rows.feed_cursors().clone())
    .with_lineage_rows(
        TreeEpochs {
            current: 1,
            successors: BTreeMap::from([(1, successor)]),
        },
        Some(ForkedFrom::Node {
            tree_id: "source-tree".to_owned(),
            agent_id: "root".to_owned(),
            line_id: NodeLineId::parse(LINE).unwrap(),
            branch_id: LINE.to_owned(),
            epoch_no: 0,
        }),
        BTreeMap::from([(
            "runner-1".to_owned(),
            BTreeSet::from(["abc".to_owned(), "def".to_owned()]),
        )]),
    )
    .rows()
    .clone();
    tree.stored_revision = stored_revision;
    tree
}

fn assert_same_rows(loaded: &ProductAgentTree, expected: &ProductAgentTree) {
    assert_eq!(loaded.nodes(), expected.nodes());
    assert_eq!(loaded.operations(), expected.operations());
    assert_eq!(loaded.epochs(), expected.epochs());
    assert_eq!(loaded.forked_from(), expected.forked_from());
    assert_eq!(loaded.runner_ancestors(), expected.runner_ancestors());
}

/// Every schema-10 row survives the whole-tree save, then a row-level save
/// that changes and removes them, then a reopened connection.
#[test]
fn schema_ten_rows_round_trip_through_both_save_paths() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let (tree, child) = spawned(root.path());

    let first = with_schema_ten_rows(&tree, &child, 0);
    let acknowledged = store.save_in_custody(&first).unwrap();
    let loaded = store.load(TREE).unwrap().unwrap();
    assert_same_rows(loaded.rows(), &first);
    for operation in loaded.rows().operations().values() {
        let _ = ContinuationIntent::from_operation(operation).unwrap();
        let _ = TerminalRefusalIntent::from_operation(operation).unwrap();
        let _ = VcsStageIntent::from_operation(operation).unwrap();
        let _ = EpochPending::from_operation(operation).unwrap();
    }

    // The row-level path: change a node's placement, drop the ancestors and
    // the provenance, add a second epoch and move the current one.
    let mut second = first.to_persisted().rows().clone();
    second.stored_revision = acknowledged.value();
    second.revision = acknowledged.value() + 1;
    let mut epochs = second.epochs().clone();
    let mut later = epochs.successors[&1].clone();
    later.transient_resumes = 0;
    later.digest = later.compute_digest();
    epochs.successors.insert(2, later);
    epochs.current = 2;
    let rows = second.to_persisted();
    let mut nodes = rows.rows().nodes().clone();
    nodes.get_mut(&child).unwrap().placement = NodePlacement::default();
    let second = rebuild(&second, nodes, epochs, None, BTreeMap::new());
    let _ = store.save_in_custody(&second).unwrap();
    assert_same_rows(store.load(TREE).unwrap().unwrap().rows(), &second);
    drop(store);
    let reopened = SqliteTreeStore::open(&path).unwrap();
    assert_same_rows(reopened.load(TREE).unwrap().unwrap().rows(), &second);
}

fn rebuild(
    tree: &ProductAgentTree,
    nodes: BTreeMap<String, plakous_recursive_subagents::tree::NodeRecord>,
    epochs: TreeEpochs,
    forked_from: Option<ForkedFrom>,
    ancestors: BTreeMap<String, BTreeSet<String>>,
) -> ProductAgentTree {
    let mut rebuilt = PersistedTree::from_rows(
        tree.tree_id.clone(),
        tree.root_agent_id.clone(),
        tree.revision,
        tree.host_generation,
        tree.state,
        tree.writer_mode,
        tree.selected_tuple.clone(),
        tree.launch_checkout.clone(),
        tree.current_epoch().clone(),
        nodes,
        tree.grants().clone(),
        tree.attempts().clone(),
        tree.candidates().clone(),
        tree.workspaces().clone(),
        tree.messages().clone(),
        tree.operations().clone(),
    )
    .with_feed_rows(tree.commands().clone(), tree.feed_cursors().clone())
    .with_lineage_rows(epochs, forked_from, ancestors)
    .rows()
    .clone();
    rebuilt.stored_revision = tree.stored_revision;
    rebuilt
}

/// Today's tree writes only the defaults, and they read back as such.
#[test]
fn todays_tree_writes_the_schema_ten_defaults() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let (tree, _) = spawned(root.path());
    let _ = store.save_in_custody(&tree).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let header: (i64, Option<String>) = connection
        .query_row(
            "SELECT current_epoch, forked_from FROM agent_trees WHERE tree_id=?1",
            [TREE],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(header, (0, None));
    let defaults: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_nodes WHERE tree_id=?1 AND line_id IS NULL AND epoch_no=0 AND runner_id IS NULL AND workspace_root IS NULL AND workspace_cwd IS NULL AND parent_composition IS NULL",
            [TREE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(defaults, 2);
    // The save outside a driver took custody for itself and released it.
    let custody: (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(released), 0) FROM agent_tree_custody",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(custody, (1, 1), "agent_tree_custody");
    for table in ["agent_tree_epochs", "agent_tree_runner_ancestors"] {
        let rows: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table}");
    }
    let loaded = store.load(TREE).unwrap().unwrap();
    assert_same_rows(loaded.rows(), &tree);
}

/// A node or header naming an epoch the tree does not hold, and a line id
/// two nodes share, are refused rather than read back.
#[test]
fn incoherent_lineage_rows_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let (tree, child) = spawned(root.path());
    let full = with_schema_ten_rows(&tree, &child, 0);
    let mut nodes = full.nodes().clone();
    nodes.get_mut(&child).unwrap().epoch_no = 5;
    let dangling = rebuild(&full, nodes, full.epochs().clone(), None, BTreeMap::new());
    let _ = store.save_in_custody(&dangling).unwrap();
    assert_eq!(
        store.load(TREE).unwrap_err().kind,
        TreeStoreErrorKind::Corrupt
    );

    let mut nodes = full.nodes().clone();
    nodes.get_mut("root").unwrap().line_id = Some(NodeLineId::parse(LINE).unwrap());
    let mut shared = rebuild(&full, nodes, full.epochs().clone(), None, BTreeMap::new());
    shared.stored_revision = dangling.revision;
    shared.revision = dangling.revision + 1;
    assert!(
        store.save_in_custody(&shared).is_err(),
        "one line maps to one node"
    );
}

const ONE: &str = "1-0000000000000001";
const TWO: &str = "2-0000000000000002";

fn host(instance: &str) -> CustodyHost {
    CustodyHost {
        host_id: "host-a".to_owned(),
        instance: HostInstance::parse(instance).unwrap(),
        binary: Some("build-1".to_owned()),
        host_name: "machine".to_owned(),
    }
}

/// Take, renew, release and prove over the file; a second connection sees
/// the same record and the same law.
#[test]
fn sqlite_custody_take_renew_release_prove() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let peer = SqliteTreeStore::open(&path).unwrap();
    assert!(store.custody(TREE).unwrap().is_none());
    let claim = store.take_custody(TREE, &host(ONE), None).unwrap();
    assert_eq!(claim.fence, 1);
    peer.prove_custody(&claim).unwrap();
    store.renew_custody(&claim).unwrap();
    let Err(CustodyError::HostedElsewhere(refusal)) = peer.take_custody(TREE, &host(TWO), None)
    else {
        panic!("another instance is refused");
    };
    assert_eq!(refusal.holder_claim(), claim);
    assert_eq!(refusal.binary(), Some("build-1"));
    assert_eq!(store.take_custody(TREE, &host(ONE), None).unwrap(), claim);

    // Takeover against exactly the gone holder.
    let taken = peer
        .take_custody(
            TREE,
            &host(TWO),
            Some(&ProductAgentTree::holder_gone_for_test(claim.clone())),
        )
        .unwrap();
    assert_eq!(taken.fence, 2);
    assert!(matches!(
        store.prove_custody(&claim),
        Err(CustodyError::HostedElsewhere(_))
    ));
    assert!(store.renew_custody(&claim).is_err());
    assert!(store.release_custody(&claim).is_err());

    peer.release_custody(&taken).unwrap();
    peer.release_custody(&taken).unwrap();
    let record = store.custody(TREE).unwrap().unwrap();
    assert_eq!(
        (record.fence, record.released, &record.holding.host),
        (2, true, &host(TWO))
    );
    assert!(
        store.release_custody(&claim).is_err(),
        "a stale lower-fence release is not held"
    );
    let next = store.take_custody(TREE, &host(ONE), None).unwrap();
    assert_eq!(next.fence, 3);
    assert!(peer.prove_custody(&taken).is_err());
}

/// Custody is not tree content: a whole-tree replace leaves it.
#[test]
fn custody_survives_tree_saves() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let claim = store.take_custody(TREE, &host(ONE), None).unwrap();
    let (tree, _) = spawned(root.path());
    let _ = store.save(&tree, &claim).unwrap();
    // Another connection's commit forces this store's next save to replace
    // the tree's rows wholesale.
    let peer = SqliteTreeStore::open(&path).unwrap();
    peer.renew_custody(&claim).unwrap();
    let mut next = tree.clone();
    next.stored_revision = tree.revision;
    next.revision = tree.revision + 1;
    let _ = store.save(&next, &claim).unwrap();
    store.prove_custody(&claim).unwrap();
}

/// Every tree write proves its claim in its own transaction: a writer whose
/// claim a takeover superseded, or that released it, writes nothing, on
/// either path of the save and on a delete; the delete of the holder takes
/// the custody record with the rows.
#[test]
fn fenced_writes_refuse_a_superseded_writer() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let peer = SqliteTreeStore::open(&path).unwrap();
    let (tree, _) = spawned(root.path());
    assert!(
        matches!(
            store
                .save(
                    &tree,
                    &CustodyClaim {
                        tree_id: TREE.to_owned(),
                        host: host(ONE),
                        fence: 1
                    }
                )
                .unwrap_err()
                .kind,
            TreeStoreErrorKind::NotInCustody
        ),
        "a claim no take minted writes nothing"
    );
    let first = store.take_custody(TREE, &host(ONE), None).unwrap();
    let acknowledged = store.save(&tree, &first).unwrap();
    let mut next = tree.clone();
    next.acknowledge_revision(acknowledged);
    next.revision += 1;

    // Another instance takes over from the first, which is proven gone.
    let second = peer
        .take_custody(
            TREE,
            &host(TWO),
            Some(&ProductAgentTree::holder_gone_for_test(first.clone())),
        )
        .unwrap();
    // The superseded writer's image path and wholesale path both refuse.
    for writer in [&store, &SqliteTreeStore::open(&path).unwrap()] {
        let refused = writer.save(&next, &first).unwrap_err();
        assert!(
            matches!(refused.kind, TreeStoreErrorKind::HostedElsewhere(_)),
            "{refused}"
        );
        assert!(
            refused.detail.starts_with("tree_hosted_elsewhere"),
            "{refused}"
        );
    }
    let refused = store
        .delete(TREE, next.stored_revision, &first)
        .unwrap_err();
    assert!(matches!(
        refused.kind,
        TreeStoreErrorKind::HostedElsewhere(_)
    ));
    assert!(
        store.exists(TREE).unwrap(),
        "the refused delete left the rows"
    );
    assert_eq!(
        store.load(TREE).unwrap().unwrap().rows().revision,
        tree.revision,
        "nothing the superseded writer offered was written"
    );

    // The holder writes; once it releases, its own claim writes nothing.
    let _ = peer.save(&next, &second).unwrap();
    peer.release_custody(&second).unwrap();
    let mut later = next.clone();
    later.stored_revision = next.revision;
    later.revision += 1;
    assert!(matches!(
        peer.save(&later, &second).unwrap_err().kind,
        TreeStoreErrorKind::NotInCustody
    ));

    // The holder's delete takes the record with the rows.
    let third = peer.take_custody(TREE, &host(TWO), None).unwrap();
    assert_eq!(third.fence, 3);
    peer.delete(TREE, next.revision, &third).unwrap();
    assert!(!peer.exists(TREE).unwrap());
    assert!(peer.custody(TREE).unwrap().is_none());
}

fn contenders(path: &Path, round: u32, count: u32) -> Vec<String> {
    let go = path.with_extension(format!("{round}.go"));
    let children = (0..count)
        .map(|index| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "schema_ten::custody_contender",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(
                    CONTENDER_ENV,
                    format!(
                        "{}|{}|{}-{index:016x}",
                        path.display(),
                        go.display(),
                        round * 10 + index + 1
                    ),
                )
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    std::fs::write(&go, b"go").unwrap();
    children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    line.split_once("CUSTODY ")
                        .map(|(_, outcome)| outcome.to_owned())
                })
                .unwrap_or_else(|| panic!("no custody outcome: {output:?}"))
        })
        .collect()
}

/// Several processes race one take; exactly one holds the tree and every
/// other one is refused by that holder's name. After its release the next
/// race mints the next fence.
#[test]
fn custody_contention_across_processes_has_one_winner_per_fence() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    for (round, fence) in [(1, 1_u64), (2, 2)] {
        let outcomes = contenders(&path, round, 4);
        let winners = outcomes
            .iter()
            .filter_map(|outcome| outcome.strip_prefix("taken "))
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1, "{outcomes:?}");
        let (instance, won) = winners[0].split_once(' ').unwrap();
        assert_eq!(won, fence.to_string());
        for outcome in &outcomes {
            assert!(
                outcome.starts_with("taken ")
                    || *outcome == format!("elsewhere {instance} {fence}"),
                "{outcomes:?}"
            );
        }
        let claim = CustodyClaim {
            tree_id: TREE.to_owned(),
            host: host(instance),
            fence,
        };
        store.prove_custody(&claim).unwrap();
        store.release_custody(&claim).unwrap();
    }
}

/// One contending process: waits for the start signal, takes once, reports.
#[test]
fn custody_contender() {
    let Ok(spec) = std::env::var(CONTENDER_ENV) else {
        return;
    };
    let mut parts = spec.split('|');
    let (path, go, instance) = (
        PathBuf::from(parts.next().unwrap()),
        PathBuf::from(parts.next().unwrap()),
        parts.next().unwrap().to_owned(),
    );
    let store = SqliteTreeStore::open(&path).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !go.exists() {
        assert!(std::time::Instant::now() < deadline, "no start signal");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    match store.take_custody(TREE, &host(&instance), None) {
        Ok(claim) => println!("CUSTODY taken {instance} {}", claim.fence),
        Err(CustodyError::HostedElsewhere(refusal)) => println!(
            "CUSTODY elsewhere {} {}",
            refusal.holder.instance, refusal.fence
        ),
        Err(error) => panic!("{error}"),
    }
}

/// Starts a process that takes custody of [`TREE`] in the store at `path`
/// through the host's liveness lock, and returns once it holds it.
fn spawn_holder(root: &Path, path: &Path) -> std::process::Child {
    let ready = root.join("holder.ready");
    let holder = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "schema_ten::custody_holder",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            HOLDER_ENV,
            format!("{}|{}", path.display(), ready.display()),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the holder never took custody"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    holder
}

/// A process that took custody through the host's liveness lock keeps the
/// tree from every other process while it lives; once it is gone, the next
/// process proves it gone and takes the tree over under the next fence. No
/// clock is involved.
#[test]
fn a_dead_holder_process_is_taken_over_and_a_live_one_is_not() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let mut holder = spawn_holder(root.path(), &path);
    let held = store.custody(TREE).unwrap().unwrap();
    assert_eq!(held.fence, 1);
    let Err(CustodyError::HostedElsewhere(refusal)) = acquire_custody(&store, TREE) else {
        panic!("a live holder keeps the tree");
    };
    assert_eq!(refusal.holder, held.holding.host);

    holder.kill().unwrap();
    holder.wait().unwrap();
    let taken = acquire_custody(&store, TREE).unwrap();
    assert_eq!(taken.claim().fence, 2, "the takeover mints the next fence");
    assert_eq!(&taken.claim().host, custody_host());
    let Err(CustodyError::HostedElsewhere(_)) = store.prove_custody(&CustodyClaim {
        tree_id: TREE.to_owned(),
        host: held.holding.host,
        fence: 1,
    }) else {
        panic!("the dead holder's claim is superseded");
    };
    release_custody_hold(&store, taken).unwrap();
}

/// Removing a live holder's liveness lock file, or the whole liveness
/// directory, proves nothing: the holder keeps the tree while it lives and
/// after it is gone, until the operator's break-glass releases the record.
#[test]
fn a_removed_liveness_lock_never_proves_a_holder_gone() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let store = SqliteTreeStore::open(&path).unwrap();
    let mut holder = spawn_holder(root.path(), &path);
    let held = store.custody(TREE).unwrap().unwrap();
    let directory = store.liveness_dir().unwrap();
    let refused = |why: &str| {
        let Err(CustodyError::HostedElsewhere(refusal)) = acquire_custody(&store, TREE) else {
            panic!("{why}: the holder keeps the tree");
        };
        assert_eq!(refusal.holder, held.holding.host, "{why}");
        assert_eq!(refusal.fence, 1, "{why}");
    };
    std::fs::remove_file(directory.join(format!("{}.lock", held.holding.host.instance))).unwrap();
    refused("its lock file was removed");
    std::fs::remove_dir_all(&directory).unwrap();
    refused("the liveness directory was removed");
    holder.kill().unwrap();
    holder.wait().unwrap();
    refused("it is gone, but nothing proves it");
}

/// The holding process of the takeover law: takes custody, says so, waits to
/// be killed.
#[test]
fn custody_holder() {
    let Ok(spec) = std::env::var(HOLDER_ENV) else {
        return;
    };
    let (path, ready) = spec.split_once('|').unwrap();
    let store = SqliteTreeStore::open(Path::new(path)).unwrap();
    let hold = acquire_custody(&store, TREE).unwrap();
    std::fs::write(ready, hold.claim().fence.to_string()).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(120));
}
