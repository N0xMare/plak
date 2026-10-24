//! Power loss after unsynced lease renewal and revival.
//!
//! Renewal and revival commit under `synchronous=NORMAL`; every other commit
//! is `FULL`. These tests model the worst case WAL allows: the bytes left by a
//! power loss are the database and its WAL as they stood when the last `FULL`
//! commit returned, without the shared-memory index. WAL recovery keeps a
//! prefix of the commit sequence and a `FULL` commit syncs every frame before
//! it, so any power loss before the next fsync leaves that image or a later
//! one.
//!
//! What each test pins, at the API level and never as a device-level fsync
//! proof (the bytes are copied from the page cache):
//! - the reboot tests: whether or not a renewal or revival survived, a new boot
//!   retires the grant;
//! - the same-boot test: losing a renewal never hands a live holder's branch
//!   away, and a dead holder is taken over through holder proof;
//! - the torn-frame test: the prefix property itself, that a renewal whose WAL
//!   frames are cut short or fail their checksum recovers as exactly the state
//!   before it;
//! - the next-commit test: a renewal is inside the image of the `FULL` commit
//!   after it.

use std::error::Error;
use std::fs;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::num::NonZeroU64;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use plakous_agent::internal::{
    AcquireBranch, BranchLease, CheckpointId, CommitCheckpoint, LeaseDomain, LeaseInstant,
    RuntimeDurability, SessionStore, SessionStoreError,
};
use plakous_model::RunId;
use plakous_store::SqliteSessionStore;

use super::conformance::{self, DomainLeaseClock};
use super::sqlite::{Database, seed, successor};

type TestResult = Result<(), Box<dyn Error>>;

const IMAGE_CHILD_PATH: &str = "PLAK_STORE_POWER_LOSS_IMAGE_PATH";
const IMAGE_TAKEN: &str = "durable-image-taken";

fn boot() -> LeaseDomain {
    LeaseDomain::new([1; 16], [2; 32])
}

fn next_boot() -> LeaseDomain {
    LeaseDomain::new([3; 16], [2; 32])
}

fn at(seconds: u64) -> Result<LeaseInstant, SessionStoreError> {
    LeaseInstant::new(boot(), seconds, 0)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

/// The database and WAL bytes as they stood when the last `FULL` commit
/// returned. Taken between store calls, so no transaction is in flight.
struct DurableImage {
    database: Vec<u8>,
    wal: Vec<u8>,
}

impl DurableImage {
    fn take(path: &Path) -> io::Result<Self> {
        Ok(Self {
            database: fs::read(path)?,
            wal: fs::read(sibling(path, "-wal"))?,
        })
    }

    fn save(&self, prefix: &Path) -> io::Result<()> {
        write_private(&sibling(prefix, ".image-db"), &self.database)?;
        write_private(&sibling(prefix, ".image-wal"), &self.wal)
    }

    fn load(prefix: &Path) -> io::Result<Self> {
        Ok(Self {
            database: fs::read(sibling(prefix, ".image-db"))?,
            wal: fs::read(sibling(prefix, ".image-wal"))?,
        })
    }

    /// Leaves exactly this image at `path`, as the next boot would find it.
    fn restore(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(sibling(path, "-shm")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        write_private(path, &self.database)?;
        write_private(&sibling(path, "-wal"), &self.wal)
    }
}

/// Kills and reaps the holder child however the test ends, so an early
/// return cannot leave it holding the branch lock.
struct KillOnDrop(std::process::Child);

impl KillOnDrop {
    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()?;
        self.0.wait().map(drop)
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn acquire(
    selector: plakous_agent::internal::SessionRef,
    owner: u128,
    seconds: u64,
) -> Result<AcquireBranch, Box<dyn Error>> {
    Ok(AcquireBranch::new(
        selector,
        conformance::id::<RunId>(owner)?,
        NonZeroU64::new(seconds).ok_or("lease duration")?,
    ))
}

/// Asserts that an old-boot grant can do nothing on the rebooted store.
async fn assert_retired(
    store: &SqliteSessionStore,
    lease: &BranchLease,
    commit: CommitCheckpoint,
) -> TestResult {
    let superseded = Err(SessionStoreError::FenceSuperseded);
    assert_eq!(store.renew(lease.clone()).await.map(|_| ()), superseded);
    assert_eq!(store.revive(lease.clone()).await.map(|_| ()), superseded);
    assert_eq!(store.commit(commit).await.map(|_| ()), superseded);
    assert_eq!(store.release(lease.clone()).await, superseded);
    Ok(())
}

/// A power loss right after unsynced renewals loses at most the renewals, and
/// the reboot retires the grant whether they survived or not: another owner
/// acquires the next fence, and nothing the old grant names can act again.
#[tokio::test]
async fn power_loss_after_an_unsynced_renewal_retires_the_grant() -> TestResult {
    let db = Database::new();
    let clock = Arc::new(DomainLeaseClock::new(boot(), 100)?);
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let initial = seed(47_000, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request.clone()).await?;
    let next = conformance::id::<CheckpointId>(47_010)?;
    let bundle = successor(&initial, next, initial.checkpoint_id, 1, &initial.bundle)?;
    let committed = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            bundle.clone(),
        ))
        .await?;
    let image = DurableImage::take(&db.path)?;
    clock.set(at(101));
    let first = store.renew(created.lease().clone()).await?;
    clock.set(at(102));
    let second = store.renew(first.clone()).await?;
    assert!(second.expires_at().seconds() > created.lease().expires_at().seconds());

    let rebooted = Database::new();
    image.restore(&rebooted.path)?;
    let current = SqliteSessionStore::open_with_clock(
        &rebooted.path,
        Arc::new(DomainLeaseClock::new(next_boot(), 1)?),
    )?;
    assert_eq!(
        current.load_branch(initial.selector).await?.head(),
        committed.head(),
        "the head is the last FULL commit"
    );
    assert_eq!(current.recover_commit(next).await?.receipt(), committed);
    let later = conformance::id::<CheckpointId>(47_011)?;
    let later_bundle = successor(&initial, later, next, 2, &bundle)?;
    for lease in [created.lease(), &first, &second] {
        assert_retired(
            &current,
            lease,
            CommitCheckpoint::new(lease.clone(), committed.head(), later_bundle.clone()),
        )
        .await?;
    }
    let taken = current
        .acquire(acquire(initial.selector, 47_020, 30)?)
        .await?;
    assert_eq!(taken.fence().get(), created.lease().fence().get() + 1);
    assert_eq!(taken.expires_at().domain(), next_boot());
    for lease in [created.lease(), &first, &second] {
        assert_retired(
            &current,
            lease,
            CommitCheckpoint::new(lease.clone(), committed.head(), later_bundle.clone()),
        )
        .await?;
    }
    assert_eq!(current.recover_commit(next).await?.receipt(), committed);
    Ok(())
}

/// A revival lost to power loss and one a later `FULL` commit made durable
/// leave the same authority after a reboot: neither grant revives or renews,
/// and another owner acquires.
#[tokio::test]
async fn revival_after_reboot_is_refused_whether_or_not_it_was_synced() -> TestResult {
    let db = Database::new();
    let clock = Arc::new(DomainLeaseClock::new(boot(), 100)?);
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let initial = seed(47_100, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request.clone()).await?;
    let first = conformance::id::<CheckpointId>(47_110)?;
    let first_bundle = successor(&initial, first, initial.checkpoint_id, 1, &initial.bundle)?;
    let committed = store
        .commit(CommitCheckpoint::new(
            created.lease().clone(),
            created.branch(),
            first_bundle.clone(),
        ))
        .await?;
    let unsynced = DurableImage::take(&db.path)?;

    clock.set(Ok(created.lease().expires_at().checked_add_seconds(600)?));
    assert_eq!(
        store.renew(created.lease().clone()).await,
        Err(SessionStoreError::LeaseExpired)
    );
    let revived = store.revive(created.lease().clone()).await?;
    assert_eq!(revived.fence(), created.lease().fence());
    let second = conformance::id::<CheckpointId>(47_111)?;
    store
        .commit(CommitCheckpoint::new(
            revived.clone(),
            committed.head(),
            successor(&initial, second, first, 2, &first_bundle)?,
        ))
        .await?;
    let synced = DurableImage::take(&db.path)?;

    for (name, image) in [("revival lost", unsynced), ("revival synced", synced)] {
        let rebooted = Database::new();
        image.restore(&rebooted.path)?;
        let current = SqliteSessionStore::open_with_clock(
            &rebooted.path,
            Arc::new(DomainLeaseClock::new(next_boot(), 1)?),
        )?;
        for lease in [created.lease(), &revived] {
            assert_eq!(
                current.revive(lease.clone()).await,
                Err(SessionStoreError::FenceSuperseded),
                "{name}"
            );
            assert_eq!(
                current.renew(lease.clone()).await,
                Err(SessionStoreError::FenceSuperseded),
                "{name}"
            );
        }
        let taken = current
            .acquire(acquire(initial.selector, 47_120, 30)?)
            .await?;
        assert_eq!(
            taken.fence().get(),
            created.lease().fence().get() + 1,
            "{name}"
        );
    }
    Ok(())
}

/// Within one boot (a test clock whose epoch survives), losing a renewal only
/// leaves the earlier expiry. That never hands the branch away from a live
/// holder: while the holder runs its lock refuses another owner, and once it
/// is gone the holder proof takes the branch at once, renewal or not.
#[tokio::test]
async fn same_boot_image_without_a_renewal_has_no_live_holder() -> TestResult {
    let initial = seed(47_200, RuntimeDurability::Required, 1)?;
    if let Some(path) = std::env::var_os(IMAGE_CHILD_PATH).map(PathBuf::from) {
        let clock = Arc::new(DomainLeaseClock::new(boot(), 100)?);
        let store = SqliteSessionStore::open_with_clock(&path, clock.clone())?;
        let created = store.create(initial.request).await?;
        DurableImage::take(&path)?.save(&path)?;
        clock.set(at(101));
        let renewed = store.renew(created.lease().clone()).await?;
        clock.set(at(102));
        store.renew(renewed).await?;
        println!("{IMAGE_TAKEN}");
        std::thread::sleep(Duration::from_secs(600));
        return Ok(());
    }

    let db = Database::new();
    let mut child = KillOnDrop(
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "sqlite_power_loss::same_boot_image_without_a_renewal_has_no_live_holder",
                "--nocapture",
            ])
            .env(IMAGE_CHILD_PATH, &db.path)
            .stdout(std::process::Stdio::piped())
            .spawn()?,
    );
    let out = child.0.stdout.take().ok_or("child stdout")?;
    let (taken_tx, taken_rx) = mpsc::channel();
    // The reader ends at the child's EOF, which its kill below guarantees.
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            if line.is_ok_and(|line| line.contains(IMAGE_TAKEN)) {
                let _ = taken_tx.send(());
            }
        }
    });
    taken_rx
        .recv_timeout(Duration::from_secs(20))
        .map_err(|_| "holder subprocess never took its image")?;

    let observer = SqliteSessionStore::open_with_clock(
        &db.path,
        Arc::new(DomainLeaseClock::new(boot(), 103)?),
    )?;
    assert_eq!(
        observer
            .acquire(acquire(initial.selector, 47_210, 30)?)
            .await,
        Err(SessionStoreError::LeaseHeld),
        "a live holder keeps its branch"
    );
    drop(observer);
    child.kill()?;

    DurableImage::load(&db.path)?.restore(&db.path)?;
    let current = SqliteSessionStore::open_with_clock(
        &db.path,
        Arc::new(DomainLeaseClock::new(boot(), 200)?),
    )?;
    let created = current.recover_create(initial.request.clone()).await?;
    assert_eq!(
        current.load_branch(initial.selector).await?.head(),
        created.branch()
    );
    // Both renewals are gone; the grant is the creation lease, still live.
    let original = created.lease();
    assert_eq!(
        current
            .acquire(AcquireBranch::new(
                initial.selector,
                original.owner(),
                original.lease_seconds(),
            ))
            .await?,
        original.clone()
    );
    assert!(original.expires_at().seconds() > 200);
    let taken = current
        .acquire(acquire(initial.selector, 47_220, 30)?)
        .await?;
    assert_eq!(taken.fence().get(), original.fence().get() + 1);
    current.release(taken).await?;
    current.shutdown().await?;
    Ok(())
}

/// WAL keeps commit order, so an unsynced renewal is inside the image of the
/// next `FULL` commit: the restored grant is the renewed one, and the commit
/// that bound it recovers with it.
#[tokio::test]
async fn an_unsynced_renewal_is_inside_the_next_full_commits_image() -> TestResult {
    let db = Database::new();
    let clock = Arc::new(DomainLeaseClock::new(boot(), 100)?);
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let initial = seed(47_300, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request.clone()).await?;
    clock.set(at(101));
    let renewed = store.renew(created.lease().clone()).await?;
    assert!(renewed.expires_at().seconds() > created.lease().expires_at().seconds());
    let next = conformance::id::<CheckpointId>(47_310)?;
    let committed = store
        .commit(CommitCheckpoint::new(
            renewed.clone(),
            created.branch(),
            successor(&initial, next, initial.checkpoint_id, 1, &initial.bundle)?,
        ))
        .await?;
    let image = DurableImage::take(&db.path)?;

    let restored = Database::new();
    image.restore(&restored.path)?;
    let current = SqliteSessionStore::open_with_clock(
        &restored.path,
        Arc::new(DomainLeaseClock::new(boot(), 102)?),
    )?;
    assert_eq!(
        current
            .acquire(AcquireBranch::new(
                initial.selector,
                renewed.owner(),
                renewed.lease_seconds(),
            ))
            .await?,
        renewed
    );
    let recovered = current.recover_commit(next).await?;
    assert_eq!(recovered.receipt(), committed);
    assert_eq!(recovered.operation().lease(), &renewed);
    Ok(())
}

/// A renewal's WAL frames that are cut short or fail their checksum are
/// dropped as a whole at recovery: the store reopens as exactly the state
/// before the renewal. The intact frames reopen as the renewal, so the cut and
/// corrupted images remove exactly that transaction and nothing else.
#[tokio::test]
async fn a_torn_or_corrupt_renewal_recovers_as_exactly_the_state_before_it() -> TestResult {
    let db = Database::new();
    let clock = Arc::new(DomainLeaseClock::new(boot(), 100)?);
    let store = SqliteSessionStore::open_with_clock(&db.path, clock.clone())?;
    let initial = seed(47_500, RuntimeDurability::Required, 1)?;
    let created = store.create(initial.request.clone()).await?;
    let before_stats = store.stats().await?;
    let before = DurableImage::take(&db.path)?;
    clock.set(at(101));
    let renewed = store.renew(created.lease().clone()).await?;
    let after = DurableImage::take(&db.path)?;
    // The renewal only appended frames: no checkpoint or WAL restart between.
    assert_eq!(after.database, before.database);
    assert!(after.wal.len() > before.wal.len());
    assert_eq!(&after.wal[..before.wal.len()], &before.wal[..]);
    let page_size = usize::try_from(u32::from_be_bytes(
        after.wal[8..12].try_into().map_err(|_| "WAL header")?,
    ))?;
    let first_frame = before.wal.len();
    assert_eq!((after.wal.len() - first_frame) % (24 + page_size), 0);

    let torn = after.wal[..first_frame + 24 + page_size / 2].to_vec();
    let mut corrupt = after.wal.clone();
    corrupt[first_frame + 24 + page_size / 2] ^= 0xff;
    let intact = after.wal.clone();
    for (name, wal, expected) in [
        ("torn", torn, created.lease()),
        ("corrupt", corrupt, created.lease()),
        ("intact", intact, &renewed),
    ] {
        let restored = Database::new();
        DurableImage {
            database: after.database.clone(),
            wal,
        }
        .restore(&restored.path)?;
        let current = SqliteSessionStore::open_with_clock(
            &restored.path,
            Arc::new(DomainLeaseClock::new(boot(), 102)?),
        )?;
        assert_eq!(
            current
                .acquire(AcquireBranch::new(
                    initial.selector,
                    created.lease().owner(),
                    created.lease().lease_seconds(),
                ))
                .await?,
            expected.clone(),
            "{name}"
        );
        assert_eq!(
            current.load_branch(initial.selector).await?.head(),
            created.branch(),
            "{name}"
        );
        if name != "intact" {
            assert_eq!(current.stats().await?, before_stats, "{name}");
        }
    }
    Ok(())
}
