use super::{bundle_apply, bundle_write, sweep};
use crate::vcs_fixtures::git_repo;

fn uploads(repo: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(repo.join(".git/plak-transfer/uploads"))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

/// A failed apply leaves no upload behind: nothing reruns from it, and the
/// carry's rerun writes it again.
#[test]
fn a_failed_apply_removes_its_upload() {
    let repo = git_repo();
    bundle_write(repo.path(), "u1", 0, b"not a bundle").unwrap();
    assert_eq!(uploads(repo.path()).len(), 1);
    let refused = bundle_apply(repo.path(), "u1", &"0".repeat(64), "ns", &[], &[]);
    assert!(refused.is_err());
    assert_eq!(uploads(repo.path()), Vec::<std::path::PathBuf>::new());
}

/// The sweep removes transfer files older than its age, and keeps younger
/// ones.
#[test]
fn the_sweep_removes_only_stale_transfer_files() {
    let repo = git_repo();
    bundle_write(repo.path(), "u1", 0, b"partial").unwrap();
    assert_eq!(
        sweep(repo.path(), std::time::Duration::from_secs(3600)).unwrap(),
        0
    );
    assert_eq!(uploads(repo.path()).len(), 1);
    assert_eq!(sweep(repo.path(), std::time::Duration::ZERO).unwrap(), 1);
    assert_eq!(uploads(repo.path()), Vec::<std::path::PathBuf>::new());
    let elsewhere = tempfile::tempdir().unwrap();
    assert_eq!(
        sweep(elsewhere.path(), std::time::Duration::ZERO).unwrap(),
        0
    );
}
