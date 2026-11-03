use super::{
    CustodyClaim, CustodyError, CustodyHost, CustodyLiveness, CustodyRecord, CustodyTake,
    HolderGone, HostInstance, TreeCustody, sealed,
};
use crate::tree::MemoryTreeStore;

fn instance(index: u64) -> HostInstance {
    HostInstance::parse(&format!("{}-{index:016x}", 100 + index)).unwrap()
}

fn host(host_id: &str, index: u64) -> CustodyHost {
    CustodyHost {
        host_id: host_id.to_owned(),
        instance: instance(index),
        binary: Some(format!("build-{host_id}")),
        host_name: "machine".to_owned(),
    }
}

fn claimed(take: CustodyTake) -> (CustodyRecord, CustodyClaim) {
    match take {
        CustodyTake::Claimed { record, claim } => (record, claim),
        CustodyTake::Held(claim) => panic!("expected a new claim, got {claim:?}"),
    }
}

/// Liveness that proves nothing gone, as a live holder's host would.
struct HolderAlive;

impl sealed::Sealed for HolderAlive {}

impl CustodyLiveness for HolderAlive {
    fn holder_gone(&self, _holder: &CustodyHost) -> bool {
        false
    }
}

/// Liveness that proves every holder gone.
struct AssumeHolderGone;

impl sealed::Sealed for AssumeHolderGone {}

impl CustodyLiveness for AssumeHolderGone {
    fn holder_gone(&self, _holder: &CustodyHost) -> bool {
        true
    }
}

#[test]
fn a_host_instance_is_typed_pid_nonce_text() {
    let generated = HostInstance::generate();
    assert_eq!(generated.pid(), std::process::id());
    assert_eq!(
        HostInstance::parse(&generated.to_string()).unwrap(),
        generated
    );
    assert_ne!(
        HostInstance::generate(),
        generated,
        "each start draws a nonce"
    );
    for refused in [
        "",
        "boot-1",
        "12-00000000000000a",
        "12-00000000000000AA",
        "012-000000000000000a",
        "12_000000000000000a",
        "-000000000000000a",
        "99999999999-000000000000000a",
    ] {
        assert!(HostInstance::parse(refused).is_err(), "{refused}");
    }
}

/// The law every store applies: a first take mints fence 1, the holder's own
/// take is idempotent, another host is refused by name and binary, and a
/// release keeps the fence and the holder so the next take supersedes every
/// older claim.
#[test]
fn custody_takes_fence_and_refuse_another_host() {
    let first = host("host-a", 1);
    let other = host("host-b", 2);
    let (record, claim) = claimed(CustodyRecord::take(None, "t", &first, None, 10).unwrap());
    assert_eq!(claim.fence, 1);
    assert_eq!(record.holding.claimed_at_ms, 10);
    assert!(!record.released);

    let CustodyTake::Held(again) =
        CustodyRecord::take(Some(&record), "t", &first, None, 11).unwrap()
    else {
        panic!("the holder's own take writes nothing");
    };
    assert_eq!(again, claim);

    let Err(CustodyError::HostedElsewhere(refusal)) =
        CustodyRecord::take(Some(&record), "t", &other, None, 12)
    else {
        panic!("another host is refused");
    };
    assert_eq!(refusal.holder, first);
    assert_eq!(refusal.fence, 1);
    assert_eq!(refusal.binary(), Some("build-host-a"));
    assert!(
        refusal.to_string().starts_with(
            "tree_hosted_elsewhere (build-host-a): agent tree t is hosted by host-a on machine"
        ),
        "{refusal}"
    );

    let released = CustodyRecord::release(Some(&record), &claim)
        .unwrap()
        .unwrap();
    assert_eq!(released.fence, 1);
    assert!(released.released);
    assert_eq!(released.holding.host, first, "the last holder is kept");
    assert!(
        CustodyRecord::release(Some(&released), &claim)
            .unwrap()
            .is_none(),
        "the last holder's repeated release succeeds and writes nothing"
    );
    assert!(matches!(
        CustodyRecord::prove(Some(&released), &claim),
        Err(CustodyError::NotHeld { .. })
    ));
    let (_, next) = claimed(CustodyRecord::take(Some(&released), "t", &other, None, 13).unwrap());
    assert_eq!(next.fence, 2);
}

/// Only the last holder's claim may repeat a release: a stale lower-fence
/// claim of the same host, and another host's claim at the same fence, are
/// not held.
#[test]
fn custody_release_of_a_released_record_needs_the_last_holder() {
    let me = host("host-a", 1);
    let (record, first) = claimed(CustodyRecord::take(None, "t", &me, None, 1).unwrap());
    let released = CustodyRecord::release(Some(&record), &first)
        .unwrap()
        .unwrap();
    let (again, second) = claimed(CustodyRecord::take(Some(&released), "t", &me, None, 2).unwrap());
    let released_again = CustodyRecord::release(Some(&again), &second)
        .unwrap()
        .unwrap();
    assert_eq!(
        CustodyRecord::release(Some(&released_again), &first).unwrap_err(),
        CustodyError::NotHeld {
            tree_id: "t".to_owned(),
            fence: 1
        },
        "a stale lower-fence release"
    );
    let mut forged = second.clone();
    forged.host = host("host-b", 2);
    assert!(matches!(
        CustodyRecord::release(Some(&released_again), &forged),
        Err(CustodyError::NotHeld { .. })
    ));
    assert!(
        CustodyRecord::release(Some(&released_again), &second)
            .unwrap()
            .is_none()
    );
}

/// A restart of the same host is another instance: it is refused until
/// liveness proves the old instance gone at exactly its fence.
#[test]
fn custody_takeover_needs_the_exact_proven_gone_holder() {
    let old = host("host-a", 1);
    let restarted = host("host-a", 2);
    let (record, claim) = claimed(CustodyRecord::take(None, "t", &old, None, 1).unwrap());
    assert!(matches!(
        CustodyRecord::take(Some(&record), "t", &restarted, None, 2),
        Err(CustodyError::HostedElsewhere(_))
    ));
    assert!(
        HolderGone::prove(claim.clone(), &HolderAlive).is_none(),
        "a live holder yields no proof"
    );
    let mut wrong_fence = claim.clone();
    wrong_fence.fence = 7;
    let stale = HolderGone::prove(wrong_fence, &AssumeHolderGone).unwrap();
    assert!(matches!(
        CustodyRecord::take(Some(&record), "t", &restarted, Some(&stale), 2),
        Err(CustodyError::HostedElsewhere(_))
    ));
    let gone = HolderGone::prove(claim.clone(), &AssumeHolderGone).unwrap();
    assert_eq!(gone.claim(), &claim);
    let (taken, new_claim) =
        claimed(CustodyRecord::take(Some(&record), "t", &restarted, Some(&gone), 3).unwrap());
    assert_eq!(new_claim.fence, 2);
    // The superseded holder can neither prove, renew nor release.
    assert!(matches!(
        CustodyRecord::prove(Some(&taken), &claim),
        Err(CustodyError::HostedElsewhere(_))
    ));
    assert!(CustodyRecord::renew(Some(&taken), &claim, 4).is_err());
    assert!(CustodyRecord::release(Some(&taken), &claim).is_err());
    // A replayed proof no longer names the holder.
    assert!(matches!(
        CustodyRecord::take(Some(&taken), "t", &host("host-c", 3), Some(&gone), 5),
        Err(CustodyError::HostedElsewhere(_))
    ));
}

#[test]
fn custody_stale_claim_of_the_same_host_is_not_held_and_renew_stamps() {
    let me = host("host-a", 1);
    let (record, first) = claimed(CustodyRecord::take(None, "t", &me, None, 1).unwrap());
    let released = CustodyRecord::release(Some(&record), &first)
        .unwrap()
        .unwrap();
    let (again, second) = claimed(CustodyRecord::take(Some(&released), "t", &me, None, 2).unwrap());
    assert_eq!(second.fence, 2);
    assert_eq!(
        CustodyRecord::prove(Some(&again), &first).unwrap_err(),
        CustodyError::NotHeld {
            tree_id: "t".to_owned(),
            fence: 1
        }
    );
    let renewed = CustodyRecord::renew(Some(&again), &second, 9).unwrap();
    assert_eq!(
        (renewed.holding.claimed_at_ms, renewed.holding.renewed_at_ms),
        (2, 9)
    );
    for empty in [
        CustodyHost {
            host_id: String::new(),
            ..me.clone()
        },
        CustodyHost {
            host_name: String::new(),
            ..me.clone()
        },
        CustodyHost {
            binary: Some(String::new()),
            ..me.clone()
        },
    ] {
        assert!(matches!(
            CustodyRecord::take(None, "t", &empty, None, 1),
            Err(CustodyError::Store(_))
        ));
    }
}

/// The in-memory store applies the same law over its own map.
#[test]
fn memory_store_custody_take_renew_release_prove() {
    let store = MemoryTreeStore::new();
    let first = host("host-a", 1);
    let other = host("host-b", 2);
    assert!(store.custody("t").unwrap().is_none());
    let claim = store.take_custody("t", &first, None).unwrap();
    store.prove_custody(&claim).unwrap();
    store.renew_custody(&claim).unwrap();
    assert!(matches!(
        store.take_custody("t", &other, None),
        Err(CustodyError::HostedElsewhere(_))
    ));
    assert_eq!(store.take_custody("t", &first, None).unwrap(), claim);
    store.release_custody(&claim).unwrap();
    store.release_custody(&claim).unwrap();
    assert!(store.prove_custody(&claim).is_err());
    let next = store.take_custody("t", &other, None).unwrap();
    assert_eq!(next.fence, 2);
    assert!(
        store.release_custody(&claim).is_err(),
        "a superseded release"
    );
    let record = store.custody("t").unwrap().unwrap();
    assert_eq!((record.fence, record.released), (2, false));
    assert!(
        store.custody("u").unwrap().is_none(),
        "trees are independent"
    );
}

/// Every holder in a process shares one claim: a release by any share but
/// the last writes nothing and leaves the record held; the last one releases
/// it.
#[test]
fn only_the_last_share_of_a_claim_releases_the_record() {
    use super::{acquire_custody, release_custody_hold};
    let store = MemoryTreeStore::new();
    let first = acquire_custody(&store, "shared").unwrap();
    let second = acquire_custody(&store, "shared").unwrap();
    assert_eq!(first.claim(), second.claim());
    release_custody_hold(&store, first).unwrap();
    assert!(
        !store.custody("shared").unwrap().unwrap().released,
        "a share is still held"
    );
    store.prove_custody(second.claim()).unwrap();
    release_custody_hold(&store, second).unwrap();
    assert!(store.custody("shared").unwrap().unwrap().released);
}

/// A forgotten share writes nothing; a last share dropped without its
/// release would keep the record held until the process exits, so debug
/// builds refuse it.
#[test]
fn a_last_share_is_released_or_forgotten_never_dropped() {
    use super::acquire_custody;
    let store = MemoryTreeStore::new();
    acquire_custody(&store, "forgotten").unwrap().forget();
    assert!(!store.custody("forgotten").unwrap().unwrap().released);
    if cfg!(debug_assertions) {
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(acquire_custody(&store, "dropped").unwrap());
        }));
        let message = dropped.unwrap_err();
        let message = message.downcast_ref::<String>().unwrap();
        assert!(
            message.contains("dropped without release_custody_hold"),
            "{message}"
        );
    }
}
