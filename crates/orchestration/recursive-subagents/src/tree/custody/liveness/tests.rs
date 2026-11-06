use std::cell::RefCell;
use std::fs::File;
use std::path::Path;

use super::{DirectoryLiveness, hold_own_lock, lock_path};
use crate::tree::{CustodyHost, CustodyLiveness, HostInstance, MemoryTreeStore, TreeCustody as _};

/// What runs between `free_lock`'s open and its lock.
type BeforeLock = Box<dyn Fn(&Path)>;

thread_local! {
    /// The hook of this thread's `free_lock` calls.
    static BEFORE_LOCK: RefCell<Option<BeforeLock>> = RefCell::new(None);
}

pub(super) fn before_lock(path: &Path) {
    BEFORE_LOCK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(path);
        }
    });
}

fn host(index: u64, host_name: &str) -> CustodyHost {
    CustodyHost {
        host_id: "local".to_owned(),
        instance: HostInstance::parse(&format!("{}-{index:016x}", 4000 + index)).unwrap(),
        binary: None,
        host_name: host_name.to_owned(),
    }
}

fn gone(directory: &Path, own: &CustodyHost, holder: &CustodyHost) -> bool {
    DirectoryLiveness { directory, own }.holder_gone(holder)
}

/// A holder whose lock is held is alive, even to another descriptor of this
/// process; a free lock proves it gone, a missing one proves nothing.
/// Another machine's holder and this instance itself are never proven gone.
#[test]
fn only_a_free_lock_on_this_machine_proves_a_holder_gone() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("custody-hosts");
    let own = host(1, "machine");
    let holder = host(2, "machine");
    hold_own_lock(&directory, holder.instance, &MemoryTreeStore::new()).unwrap();
    assert!(
        !gone(&directory, &own, &holder),
        "a held lock is a live holder"
    );
    assert!(!gone(&directory, &holder, &holder), "never gone to itself");

    let free = host(3, "machine");
    File::create(lock_path(&directory, free.instance)).unwrap();
    assert!(
        gone(&directory, &own, &free),
        "a free lock is a dead holder"
    );
    let missing = host(4, "machine");
    assert!(
        !gone(&directory, &own, &missing),
        "a missing lock proves nothing"
    );
    assert!(
        !gone(&directory, &own, &host(5, "another-machine")),
        "another machine's holder is never proven gone here"
    );
    assert!(
        !gone(&root.path().join("absent"), &own, &missing),
        "no directory proves nothing"
    );
}

/// Removing a live holder's lock file, or the whole directory, never proves
/// that holder gone.
#[test]
fn removing_a_live_holders_lock_proves_nothing() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("custody-hosts");
    std::fs::create_dir(&directory).unwrap();
    let own = host(11, "machine");
    let holder = host(12, "machine");
    let path = lock_path(&directory, holder.instance);
    let held = File::create(&path).unwrap();
    held.lock().unwrap();

    std::fs::remove_file(&path).unwrap();
    assert!(!gone(&directory, &own, &holder), "its file was removed");
    std::fs::remove_dir(&directory).unwrap();
    assert!(
        !gone(&directory, &own, &holder),
        "its directory was removed"
    );
    std::fs::create_dir(&directory).unwrap();
    assert!(
        !gone(&directory, &own, &holder),
        "its directory was recreated"
    );
    drop(held);
}

/// A lock file unlinked and recreated between `free_lock`'s open and its
/// lock is not the file that was locked, whether the new one is free or
/// held: neither proves the holder gone.
#[test]
fn a_lock_replaced_between_the_open_and_the_lock_proves_nothing() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("custody-hosts");
    std::fs::create_dir(&directory).unwrap();
    let own = host(13, "machine");
    let holder = host(14, "machine");
    File::create(lock_path(&directory, holder.instance)).unwrap();
    for lock_the_new_file in [false, true] {
        let kept = std::rc::Rc::new(RefCell::new(Vec::new()));
        let replaced = std::rc::Rc::clone(&kept);
        BEFORE_LOCK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                std::fs::remove_file(path).unwrap();
                let file = File::create(path).unwrap();
                if lock_the_new_file {
                    file.lock().unwrap();
                }
                replaced.borrow_mut().push(file);
            }));
        });
        let proven = gone(&directory, &own, &holder);
        BEFORE_LOCK.with(|hook| hook.borrow_mut().take());
        assert!(!proven, "locked the new file: {lock_the_new_file}");
        // Each attempt of the probe met a replaced file.
        assert_eq!(
            kept.borrow().len(),
            usize::try_from(super::PROBE_ATTEMPTS).unwrap(),
            "the hook ran"
        );
    }
}

/// A lock file replaced by a link is never read as the holder's lock.
#[test]
fn a_linked_lock_proves_nothing() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("custody-hosts");
    std::fs::create_dir(&directory).unwrap();
    let target = root.path().join("elsewhere.lock");
    File::create(&target).unwrap();
    let holder = host(6, "machine");
    std::os::unix::fs::symlink(&target, lock_path(&directory, holder.instance)).unwrap();
    assert!(!gone(&directory, &host(7, "machine"), &holder));
}

/// Taking this instance's lock sweeps the free locks of dead instances that
/// no held record names, and leaves named ones, held ones and unrelated
/// files alone, so a crashed holder's record keeps its proof.
#[test]
fn taking_the_own_lock_sweeps_only_unnamed_free_locks() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("custody-hosts");
    std::fs::create_dir(&directory).unwrap();
    let store = MemoryTreeStore::new();
    let dead = lock_path(&directory, host(8, "machine").instance);
    File::create(&dead).unwrap();
    let crashed = host(15, "machine");
    let crashed_lock = lock_path(&directory, crashed.instance);
    File::create(&crashed_lock).unwrap();
    store.take_custody("tree-held", &crashed, None).unwrap();
    let released = host(16, "machine");
    let released_lock = lock_path(&directory, released.instance);
    File::create(&released_lock).unwrap();
    let claim = store
        .take_custody("tree-released", &released, None)
        .unwrap();
    store.release_custody(&claim).unwrap();
    let live = lock_path(&directory, host(9, "machine").instance);
    let held = File::create(&live).unwrap();
    held.lock().unwrap();
    let other = directory.join("notes.txt");
    File::create(&other).unwrap();

    let own = host(10, "machine");
    hold_own_lock(&directory, own.instance, &store).unwrap();
    assert!(lock_path(&directory, own.instance).is_file());
    assert!(!dead.exists(), "a dead instance's free lock is swept");
    assert!(
        !released_lock.exists(),
        "a released record does not keep a free lock"
    );
    assert!(
        crashed_lock.exists(),
        "a held record keeps its crashed holder's free lock"
    );
    assert!(
        gone(&directory, &own, &crashed),
        "and so its proof of the crashed holder"
    );
    assert!(live.exists() && other.exists());
    drop(held);
}

/// Probes of one dead holder's free lock from two threads of this process
/// always prove it gone: each probe is serialized, so neither sees the
/// other's momentary lock as a live holder.
#[test]
fn concurrent_probes_never_read_a_dead_holder_alive() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("custody-hosts");
    std::fs::create_dir(&directory).unwrap();
    let own = host(31, "machine");
    let dead = host(32, "machine");
    File::create(lock_path(&directory, dead.instance)).unwrap();
    let alive = std::thread::scope(|scope| {
        let probes: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    (0..2_000)
                        .filter(|_| !gone(&directory, &own, &dead))
                        .count()
                })
            })
            .collect();
        probes
            .into_iter()
            .map(|probe| probe.join().unwrap())
            .sum::<usize>()
    });
    assert_eq!(alive, 0, "a dead holder was read alive");
}
