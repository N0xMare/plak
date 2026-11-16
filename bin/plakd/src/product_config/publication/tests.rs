#![allow(
    clippy::unwrap_used,
    reason = "deterministic config publication regressions"
)]

use std::sync::mpsc;
use std::time::Duration;

use super::*;

#[test]
fn concurrent_operator_and_host_writers_keep_both_changes() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    let before = ProductConfig::default_product();
    before.save(&paths).unwrap();
    let mut requested = before.clone();
    requested.compaction_enabled = !before.compaction_enabled;
    let (held, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let operator_paths = paths.clone();
    let operator = std::thread::spawn(move || {
        ProductConfig::update_user(&operator_paths, |config| {
            config.jobs_foreground_seconds = 9;
            config.skills_enabled = false;
            held.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        })
        .unwrap();
    });
    entered.recv().unwrap();
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.home().join("config.lock"))
        .unwrap();
    assert!(
        matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "the operator must hold the publication lock while reading and editing"
    );
    let host_paths = paths.clone();
    let (done, finished) = mpsc::channel();
    let host = std::thread::spawn(move || {
        let saved = requested
            .save_settings_delta(&host_paths, &before, ConfigWriteScope::User)
            .unwrap();
        done.send(saved.compaction_enabled).unwrap();
    });
    assert!(
        finished.recv_timeout(Duration::from_millis(100)).is_err(),
        "the host must wait for the operator's publication"
    );
    release.send(()).unwrap();
    operator.join().unwrap();
    let compaction = finished.recv_timeout(Duration::from_secs(5)).unwrap();
    host.join().unwrap();
    let saved = ProductConfig::load(&paths).unwrap();
    assert_eq!(saved.jobs_foreground_seconds, 9);
    assert!(!saved.skills_enabled);
    assert_eq!(saved.compaction_enabled, compaction);
    assert!(!compaction);
}

#[test]
fn a_stale_host_delta_preserves_other_product_fields_and_unrelated_settings() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    let before = ProductConfig::default_product();
    before.save(&paths).unwrap();
    ProductConfig::update_user(&paths, |config| {
        config.jobs_foreground_seconds = 9;
        config.auto_continue = !before.auto_continue;
        Ok(())
    })
    .unwrap();
    let mut requested = before.clone();
    requested.compaction_enabled = !before.compaction_enabled;
    let saved = requested
        .save_settings_delta(&paths, &before, ConfigWriteScope::User)
        .unwrap();
    assert_eq!(saved.jobs_foreground_seconds, 9);
    assert_ne!(saved.auto_continue, before.auto_continue);
    assert_ne!(saved.compaction_enabled, before.compaction_enabled);
}

#[test]
fn a_waiting_writer_reads_the_baseline_after_taking_the_lock() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    ProductConfig::default_product().save(&paths).unwrap();
    let lock = ConfigLock::take(&paths).unwrap();
    let writer_paths = paths.clone();
    let (started, starting) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        started.send(()).unwrap();
        ProductConfig::update_user(&writer_paths, |config| {
            config.skills_enabled = false;
            Ok(())
        })
        .unwrap();
        done.send(()).unwrap();
    });
    starting.recv().unwrap();
    assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
    let mut latest = ProductConfig::load_user(&paths).unwrap();
    latest.jobs_foreground_seconds = 9;
    latest.save_user_unlocked(&paths).unwrap();
    drop(lock);
    writer.join().unwrap();
    let saved = ProductConfig::load_user(&paths).unwrap();
    assert_eq!(saved.jobs_foreground_seconds, 9);
    assert!(!saved.skills_enabled);
}

#[test]
fn a_symlinked_publication_lock_refuses_without_touching_configuration() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let target = paths.home().join("not-a-lock");
    std::fs::write(&target, "unchanged").unwrap();
    std::os::unix::fs::symlink(&target, paths.home().join("config.lock")).unwrap();
    assert!(ProductConfig::default_product().save(&paths).is_err());
    assert!(!paths.config_file().exists());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
}
