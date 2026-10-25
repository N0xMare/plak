//! Credential-store platform regressions.

use crate::private_store::PrivateCredentialStore;
use crate::private_store::PrivateStoreError;
use crate::private_store::platform;
use crate::private_store::{
    Fixture, LOCK_HELPER_PATH, LOCK_HELPER_READY, LOCK_HELPER_RELEASE, LOCK_HELPER_SHARED_LEASE,
    RotatedDirectory, create_private_file, run_after_blocked_directory_rotation, wait_for_file,
};
use crate::{
    CredentialGeneration, CredentialSlotId, CredentialStatus, SubscriptionIntegrationProfileId,
};
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

#[test]
fn rejects_unsafe_permissions_symlinks_and_hardlinks() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fs::set_permissions(fixture.directory.path(), fs::Permissions::from_mode(0o755))?;
    assert!(matches!(
        fixture.store(),
        Err(PrivateStoreError::UnsafePermissions)
    ));
    fs::set_permissions(fixture.directory.path(), fs::Permissions::from_mode(0o700))?;

    let outside = fixture.directory.path().join("outside");
    fs::write(&outside, b"{}")?;
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
    symlink(&outside, &fixture.path)?;
    assert!(matches!(
        fixture.store()?.load(),
        Err(PrivateStoreError::UnsafeRecord)
    ));
    fs::remove_file(&fixture.path)?;

    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"payload")?)?;
    let alias = fixture.directory.path().join("hardlink");
    fs::hard_link(&fixture.path, &alias)?;
    assert!(matches!(store.load(), Err(PrivateStoreError::UnsafeRecord)));
    fs::remove_file(alias)?;

    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o644))?;
    assert!(matches!(
        store.load(),
        Err(PrivateStoreError::UnsafePermissions)
    ));
    Ok(())
}

#[test]
fn rejects_unsafe_lock_file() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let outside = fixture.directory.path().join("outside-lock");
    fs::write(&outside, b"")?;
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
    symlink(&outside, store.lock_path())?;
    assert!(matches!(store.load(), Err(PrivateStoreError::UnsafeLock)));
    fs::remove_file(store.lock_path())?;

    fs::hard_link(&outside, store.lock_path())?;
    assert!(matches!(store.load(), Err(PrivateStoreError::UnsafeLock)));
    Ok(())
}

#[test]
fn rejects_symlink_in_private_directory_path() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
    let actual = root.path().join("actual");
    fs::create_dir(&actual)?;
    fs::set_permissions(&actual, fs::Permissions::from_mode(0o700))?;
    let alias = root.path().join("alias");
    symlink(&actual, &alias)?;
    let result = PrivateCredentialStore::open_slot(
        alias.join("credentials.json"),
        SubscriptionIntegrationProfileId::new("profile")?,
        CredentialSlotId::new("primary")?,
    );
    assert!(matches!(result, Err(PrivateStoreError::UnsafeDirectory)));
    Ok(())
}

#[test]
fn existing_handles_reject_split_directory_authority_for_cas_and_lease()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let ready = store.load()?.ok_or("missing ready fence")?;
    let replacement = fixture.record(2, CredentialStatus::Ready, b"new")?;

    let rotation = RotatedDirectory::replace(fixture.directory.path())?;
    assert_eq!(
        store.replace(CredentialGeneration::new(1)?, &replacement),
        Err(PrivateStoreError::UnsafeDirectory)
    );
    assert!(matches!(
        store.acquire_ready(&ready),
        Err(PrivateStoreError::UnsafeDirectory)
    ));
    drop(rotation);

    assert_eq!(
        store.load()?.ok_or("original authority was not restored")?,
        ready
    );
    Ok(())
}

#[test]
fn blocked_load_rejects_directory_rotation_after_flock() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture
        .store()?
        .initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let before = fs::read(&fixture.path)?;
    let (result, rotation) =
        run_after_blocked_directory_rotation(&fixture, |store| store.load().map(drop))?;
    assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
    assert_eq!(
        fs::read(rotation.displaced.join("credentials.json"))?,
        before
    );
    Ok(())
}

#[test]
fn blocked_initialize_cannot_write_detached_directory() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let candidate = fixture.record(1, CredentialStatus::Ready, b"new")?;
    let (result, rotation) =
        run_after_blocked_directory_rotation(&fixture, move |store| store.initialize(&candidate))?;
    assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
    assert!(!rotation.displaced.join("credentials.json").exists());
    assert!(!fixture.path.exists());
    Ok(())
}

#[test]
fn blocked_cas_cannot_write_detached_directory() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture
        .store()?
        .initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let before = fs::read(&fixture.path)?;
    let replacement = fixture.record(2, CredentialStatus::Ready, b"new")?;
    let (result, rotation) = run_after_blocked_directory_rotation(&fixture, move |store| {
        store.replace(
            CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
            &replacement,
        )
    })?;
    assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
    assert_eq!(
        fs::read(rotation.displaced.join("credentials.json"))?,
        before
    );
    Ok(())
}

#[test]
fn blocked_ready_lease_rejects_detached_directory() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let fence = store.load()?.ok_or("missing ready fence")?;
    let (result, _rotation) = run_after_blocked_directory_rotation(&fixture, move |store| {
        store.acquire_ready(&fence).map(drop)
    })?;
    assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
    Ok(())
}

#[test]
fn lock_retries_when_open_inode_is_replaced_before_flock() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    assert!(store.load()?.is_none());
    let lock_path = store.lock_path();
    let displaced = fixture.directory.path().join("displaced-lock");
    let mut observations = 0_usize;
    let _guard = platform::lock_with_test_observer(&store, |attempt| {
        observations += 1;
        if attempt == 0 {
            fs::rename(&lock_path, &displaced).map_err(|_| PrivateStoreError::Io)?;
            create_private_file(&lock_path)?;
        }
        Ok(())
    })?;
    assert_eq!(observations, 2);
    assert_ne!(
        fs::metadata(&lock_path)?.ino(),
        fs::metadata(displaced)?.ino()
    );
    Ok(())
}

#[test]
fn lock_fails_closed_under_continuous_inode_replacement() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    assert!(store.load()?.is_none());
    let lock_path = store.lock_path();
    let result = platform::lock_with_test_observer(&store, |attempt| {
        let displaced = fixture.directory.path().join(format!("lock-{attempt}"));
        fs::rename(&lock_path, displaced).map_err(|_| PrivateStoreError::Io)?;
        create_private_file(&lock_path)
    });
    assert!(matches!(result, Err(PrivateStoreError::UnsafeLock)));
    Ok(())
}

#[test]
fn lock_is_exclusive_across_processes() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture
        .store()?
        .initialize(&fixture.record(1, CredentialStatus::Ready, b"payload")?)?;
    let fence = fixture.store()?.load()?.ok_or("missing fence")?;
    let ready = fixture.directory.path().join("helper-ready");
    let release = fixture.directory.path().join("helper-release");
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .args([
            "--exact",
            "private_store::tests::cross_process_lock_helper",
            "--nocapture",
        ])
        .env(LOCK_HELPER_PATH, &fixture.path)
        .env(LOCK_HELPER_READY, &ready)
        .env(LOCK_HELPER_RELEASE, &release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_for_file(&ready)?;
    assert!(fixture.store()?.try_acquire_ready(&fence)?.is_none());

    let contender = fixture.store()?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = contender.load().map(|record| record.is_some());
        let _ = sender.send(result);
    });
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    fs::write(&release, b"release")?;
    assert!(receiver.recv_timeout(Duration::from_secs(2))??);
    worker.join().map_err(|_| "lock contender panicked")?;
    assert!(child.wait()?.success());
    Ok(())
}

#[test]
fn shared_lease_is_cross_process_and_blocks_exclusive_logout()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture
        .store()?
        .initialize(&fixture.record(1, CredentialStatus::Ready, b"payload")?)?;
    let fence = fixture.store()?.load()?.ok_or("missing fence")?;
    let ready = fixture.directory.path().join("shared-helper-ready");
    let release = fixture.directory.path().join("shared-helper-release");
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "private_store::tests::cross_process_lock_helper",
            "--nocapture",
        ])
        .env(LOCK_HELPER_PATH, &fixture.path)
        .env(LOCK_HELPER_READY, &ready)
        .env(LOCK_HELPER_RELEASE, &release)
        .env(LOCK_HELPER_SHARED_LEASE, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_for_file(&ready)?;
    let local_lease = fixture
        .store()?
        .try_acquire_ready(&fence)?
        .ok_or("shared lock was not shared")?;

    let mutation_store = fixture.store()?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = mutation_store
            .logout(CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?);
        let _ = sender.send(result);
        Ok::<(), PrivateStoreError>(())
    });
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    drop(local_lease);
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    fs::write(&release, b"release")?;
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5))??,
        CredentialGeneration::new(2)?
    );
    worker
        .join()
        .map_err(|_| "cross-process logout panicked")??;
    assert!(child.wait()?.success());
    Ok(())
}

#[test]
fn replacement_is_always_a_complete_document() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"first")?)?;
    let before_inode = fs::metadata(&fixture.path)?.ino();
    store.replace(
        CredentialGeneration::new(1)?,
        &fixture.record(2, CredentialStatus::Ready, b"second")?,
    )?;
    let after_inode = fs::metadata(&fixture.path)?.ino();
    assert_ne!(before_inode, after_inode);
    let loaded = store.load()?.ok_or("missing replacement")?;
    assert_eq!(
        store.acquire_ready(&loaded)?.payload().expose_secret(),
        b"second"
    );
    let leftovers = fs::read_dir(fixture.directory.path())?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count();
    assert_eq!(leftovers, 0);
    Ok(())
}
