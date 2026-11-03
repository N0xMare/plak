//! The custody law both tree stores share (spec 03 §14): one deterministic
//! suite run against the memory and the SQLite store, and a seeded random
//! walk that drives both through the same takes, takeovers, renewals,
//! releases, proofs, fenced saves and fenced deletes and requires the same
//! answer and the same stored state after every step.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]

use std::path::Path;
use std::sync::Arc;

use plakous_recursive_subagents::protocol::WriterMode;
use plakous_recursive_subagents::tree::{
    CustodyClaim, CustodyError, CustodyHost, FakeVcs, HostInstance, MemoryTreeStore,
    ProductAgentTree, TreeCustody, TreeStore, TreeStoreError, TreeStoreErrorKind,
};
use plakous_recursive_subagents_store::SqliteTreeStore;

const TREES: [&str; 2] = ["custody-a", "custody-b"];

fn host(index: u64) -> CustodyHost {
    CustodyHost {
        host_id: format!("host-{}", index % 2),
        instance: HostInstance::parse(&format!("{}-{index:016x}", 500 + index)).unwrap(),
        binary: Some(format!("build-{index}")),
        host_name: "machine".to_owned(),
    }
}

fn tree(root: &Path, tree_id: &str) -> ProductAgentTree {
    ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        tree_id,
        "root",
        "owner",
        root.to_owned(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap()
}

/// The two stores behind one surface: the fenced delete is each store's own.
trait Store: TreeStore + TreeCustody {
    fn delete_fenced(
        &self,
        tree_id: &str,
        expected: u64,
        claim: &CustodyClaim,
    ) -> Result<(), TreeStoreError>;
}

impl Store for MemoryTreeStore {
    fn delete_fenced(
        &self,
        tree_id: &str,
        expected: u64,
        claim: &CustodyClaim,
    ) -> Result<(), TreeStoreError> {
        self.delete(tree_id, expected, claim)
    }
}

impl Store for SqliteTreeStore {
    fn delete_fenced(
        &self,
        tree_id: &str,
        expected: u64,
        claim: &CustodyClaim,
    ) -> Result<(), TreeStoreError> {
        self.delete(tree_id, expected, claim)
    }
}

fn custody_kind(result: &Result<CustodyClaim, CustodyError>) -> String {
    match result {
        Ok(claim) => format!("ok {} {}", claim.host.instance, claim.fence),
        Err(CustodyError::HostedElsewhere(refusal)) => {
            format!("elsewhere {} {}", refusal.holder.instance, refusal.fence)
        }
        Err(CustodyError::NotHeld { fence, .. }) => format!("not-held {fence}"),
        Err(CustodyError::Store(error)) => format!("store {:?}", error.kind),
    }
}

fn unit_kind(result: Result<(), CustodyError>) -> String {
    custody_kind(&result.map(|()| CustodyClaim {
        tree_id: String::new(),
        host: host(0),
        fence: 0,
    }))
}

fn write_kind<T>(result: &Result<T, TreeStoreError>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => match &error.kind {
            TreeStoreErrorKind::HostedElsewhere(refusal) => {
                format!("elsewhere {} {}", refusal.holder.instance, refusal.fence)
            }
            kind => format!("{kind:?}"),
        },
    }
}

/// The deterministic law, against one store.
fn law(store: &dyn Store, root: &Path) {
    let (one, two) = (host(1), host(2));
    let id = TREES[0];
    let mut rows = tree(root, id);

    // No claim minted yet: nothing writes.
    let unminted = CustodyClaim {
        tree_id: id.to_owned(),
        host: one.clone(),
        fence: 1,
    };
    assert_eq!(write_kind(&store.save(&rows, &unminted)), "NotInCustody");

    // A take mints fence 1; the holder's own take returns it; another host is
    // refused by the holder's name.
    let first = store.take_custody(id, &one, None).unwrap();
    assert_eq!(first.fence, 1);
    assert_eq!(store.take_custody(id, &one, None).unwrap(), first);
    assert_eq!(
        store.held_custody_instances().unwrap(),
        [one.instance].into_iter().collect()
    );
    assert_eq!(
        custody_kind(&store.take_custody(id, &two, None)),
        format!("elsewhere {} 1", one.instance)
    );
    // A claim for one tree never writes another.
    let mut other = tree(root, TREES[1]);
    assert_eq!(write_kind(&store.save(&other, &first)), "NotInCustody");

    // The holder writes and deletes under its claim.
    rows.acknowledge_revision(store.save(&rows, &first).unwrap());
    store.renew_custody(&first).unwrap();
    store.prove_custody(&first).unwrap();

    // A takeover needs exactly the gone holder at its fence.
    let stale_proof = ProductAgentTree::holder_gone_for_test(CustodyClaim {
        fence: 7,
        ..first.clone()
    });
    assert_eq!(
        custody_kind(&store.take_custody(id, &two, Some(&stale_proof))),
        format!("elsewhere {} 1", one.instance)
    );
    let second = store
        .take_custody(
            id,
            &two,
            Some(&ProductAgentTree::holder_gone_for_test(first.clone())),
        )
        .unwrap();
    assert_eq!(second.fence, 2);

    // The superseded holder writes nothing and cannot renew, release, prove
    // or delete; the new holder can.
    let mut next = rows.clone();
    next.revision += 1;
    assert_eq!(
        write_kind(&store.save(&next, &first)),
        format!("elsewhere {} 2", two.instance)
    );
    assert_eq!(
        write_kind(&store.delete_fenced(id, rows.revision, &first)),
        format!("elsewhere {} 2", two.instance)
    );
    assert!(store.renew_custody(&first).is_err());
    assert!(store.release_custody(&first).is_err());
    assert!(store.prove_custody(&first).is_err());
    next.acknowledge_revision(store.save(&next, &second).unwrap());

    // A release keeps the fence and holder; the released claim writes
    // nothing; only the last holder may repeat it; the next take mints 3.
    store.release_custody(&second).unwrap();
    store.release_custody(&second).unwrap();
    assert!(store.release_custody(&first).is_err());
    let record = store.custody(id).unwrap().unwrap();
    assert_eq!(
        (record.fence, record.released, &record.holding.host),
        (2, true, &two)
    );
    assert!(
        store.held_custody_instances().unwrap().is_empty(),
        "a released record names no holder a sweep must keep"
    );
    let mut later = next.clone();
    later.revision += 1;
    assert_eq!(write_kind(&store.save(&later, &second)), "NotInCustody");
    let third = store.take_custody(id, &one, None).unwrap();
    assert_eq!(third.fence, 3);

    // A stale revision under a held claim is still the revision refusal.
    let mut behind = rows.clone();
    behind.revision += 5;
    assert!(matches!(
        store.save(&behind, &third).unwrap_err().kind,
        TreeStoreErrorKind::StaleRevision { .. }
    ));

    // The holder's delete removes the rows and the record; a take after it
    // starts again at fence 1.
    store.delete_fenced(id, next.revision, &third).unwrap();
    assert!(!store.exists(id).unwrap());
    assert!(store.custody(id).unwrap().is_none());
    assert_eq!(store.take_custody(id, &two, None).unwrap().fence, 1);

    // Trees are independent.
    let fourth = store.take_custody(TREES[1], &one, None).unwrap();
    other.acknowledge_revision(store.save(&other, &fourth).unwrap());
    assert_eq!(fourth.fence, 1);
}

#[test]
fn the_memory_store_keeps_the_custody_law() {
    let root = tempfile::tempdir().unwrap();
    law(&MemoryTreeStore::new(), root.path());
}

#[test]
fn the_sqlite_store_keeps_the_custody_law() {
    let root = tempfile::tempdir().unwrap();
    law(
        &SqliteTreeStore::open(&root.path().join("subagents.sqlite3")).unwrap(),
        root.path(),
    );
}

/// A seeded generator, so a failing walk replays exactly.
struct Walk(u64);

impl Walk {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Everything either store holds that the law decides: each tree's custody
/// record (without its clock stamps) and its stored revision, and the
/// instances that hold a record.
fn state(store: &dyn Store) -> Vec<String> {
    let held = format!("held {:?}", store.held_custody_instances().unwrap());
    TREES
        .iter()
        .map(|id| {
            let record = store.custody(id).unwrap().map(|record| {
                format!(
                    "{} {} {}",
                    record.fence, record.holding.host.instance, record.released
                )
            });
            let revision = store
                .load(id)
                .unwrap()
                .map(|persisted| persisted.rows().revision);
            format!("{id}: {record:?} rows {revision:?}")
        })
        .chain([held])
        .collect()
}

/// 4,000 random steps over three hosts and two trees, the claims each host
/// was ever handed (stale ones included) and every integrated transition:
/// take, takeover with a proof for the recorded or a stale holder, renew,
/// release, prove, fenced save at the current or a stale revision, and fenced
/// delete. Both stores must answer every step alike and hold the same state
/// after it.
#[test]
fn the_memory_and_sqlite_stores_agree_on_a_random_custody_walk() {
    let root = tempfile::tempdir().unwrap();
    let memory = MemoryTreeStore::new();
    let sqlite = SqliteTreeStore::open(&root.path().join("subagents.sqlite3")).unwrap();
    let stores: [&dyn Store; 2] = [&memory, &sqlite];
    let hosts = [host(1), host(2), host(3)];
    let mut claims: Vec<CustodyClaim> = Vec::new();
    let mut acknowledged = [0_u64; 2];
    let mut walk = Walk(0x00c0_ffee_d00d_f00d);
    let mut kinds = std::collections::BTreeMap::<&str, usize>::new();
    for step in 0..4_000 {
        let tree_index = usize::try_from(walk.below(2)).unwrap();
        let tree_id = TREES[tree_index];
        let host = &hosts[usize::try_from(walk.below(3)).unwrap()];
        let claim = (!claims.is_empty())
            .then(|| claims[usize::try_from(walk.below(claims.len() as u64)).unwrap()].clone());
        let (name, answers): (&str, Vec<String>) = match (walk.below(8), claim) {
            (0 | 1, _) => (
                "take",
                stores
                    .iter()
                    .map(|store| custody_kind(&store.take_custody(tree_id, host, None)))
                    .collect(),
            ),
            (2, _) => {
                // A proof for the holder the record names, or for a stale one.
                let record = memory.custody(tree_id).unwrap();
                let proven = record.map(|record| CustodyClaim {
                    tree_id: tree_id.to_owned(),
                    host: record.holding.host,
                    fence: if walk.below(4) == 0 {
                        record.fence + 1
                    } else {
                        record.fence
                    },
                });
                let proof = proven.map(ProductAgentTree::holder_gone_for_test);
                (
                    "takeover",
                    stores
                        .iter()
                        .map(|store| {
                            custody_kind(&store.take_custody(tree_id, host, proof.as_ref()))
                        })
                        .collect(),
                )
            }
            (3, Some(claim)) => (
                "renew",
                stores
                    .iter()
                    .map(|store| unit_kind(store.renew_custody(&claim)))
                    .collect(),
            ),
            (4, Some(claim)) => (
                "release",
                stores
                    .iter()
                    .map(|store| unit_kind(store.release_custody(&claim)))
                    .collect(),
            ),
            (5, Some(claim)) => (
                "prove",
                stores
                    .iter()
                    .map(|store| unit_kind(store.prove_custody(&claim)))
                    .collect(),
            ),
            (6, Some(claim)) => {
                let mut rows = tree(root.path(), &claim.tree_id);
                let index = TREES.iter().position(|id| *id == claim.tree_id).unwrap();
                rows.stored_revision = if walk.below(5) == 0 {
                    acknowledged[index] + 1
                } else {
                    acknowledged[index]
                };
                rows.revision = rows.stored_revision + 1;
                let answers = stores
                    .iter()
                    .map(|store| write_kind(&store.save(&rows, &claim)))
                    .collect::<Vec<_>>();
                if answers[0] == "ok" {
                    acknowledged[index] = rows.revision;
                }
                ("save", answers)
            }
            (7, Some(claim)) => {
                let index = TREES.iter().position(|id| *id == claim.tree_id).unwrap();
                let expected = if walk.below(5) == 0 {
                    acknowledged[index] + 1
                } else {
                    acknowledged[index]
                };
                let answers = stores
                    .iter()
                    .map(|store| write_kind(&store.delete_fenced(&claim.tree_id, expected, &claim)))
                    .collect::<Vec<_>>();
                if answers[0] == "ok" {
                    acknowledged[index] = 0;
                }
                ("delete", answers)
            }
            _ => continue,
        };
        assert_eq!(
            answers[0], answers[1],
            "step {step} {name}: the stores disagree"
        );
        if let Some(fresh) = answers[0]
            .strip_prefix("ok ")
            .filter(|_| matches!(name, "take" | "takeover"))
        {
            let fence = fresh.rsplit(' ').next().unwrap().parse().unwrap();
            claims.push(CustodyClaim {
                tree_id: tree_id.to_owned(),
                host: host.clone(),
                fence,
            });
        }
        *kinds
            .entry(if answers[0].starts_with("ok") {
                name
            } else {
                "refused"
            })
            .or_default() += 1;
        assert_eq!(
            state(&memory),
            state(&sqlite),
            "step {step} {name}: the stored state diverged"
        );
    }
    // The walk reached every transition, both granted and refused.
    for kind in [
        "take", "takeover", "renew", "release", "prove", "save", "delete", "refused",
    ] {
        assert!(kinds.get(kind).copied().unwrap_or(0) > 10, "{kinds:?}");
    }
}
