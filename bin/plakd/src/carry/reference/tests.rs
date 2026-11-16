use super::*;

#[test]
fn reference_removal_depends_only_on_the_owned_regular_target() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    fs::create_dir(paths.home()).unwrap();
    let dir = paths.home().join("references");
    let id = uuid::Uuid::new_v4().to_string();
    remove_retained_archive(&paths, &id).unwrap();
    fs::write(&dir, b"unrelated file").unwrap();
    remove_retained_archive(&paths, &id).unwrap();
    assert_eq!(fs::read(&dir).unwrap(), b"unrelated file");
    fs::remove_file(&dir).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let target = dir.join(format!("{id}.json"));
    fs::write(&target, b"archive").unwrap();
    remove_retained_archive(&paths, &id).unwrap();
    assert!(!target.exists());
    let outside = root.path().join("outside");
    fs::write(&outside, b"keep").unwrap();
    symlink(&outside, &target).unwrap();
    remove_retained_archive(&paths, &id).unwrap();
    assert!(fs::symlink_metadata(&target).unwrap().is_symlink());
    assert_eq!(fs::read(&outside).unwrap(), b"keep");
}

#[test]
fn reference_removal_never_blocks_deletion_on_permission_or_path_errors() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    fs::create_dir(paths.home()).unwrap();
    let dir = paths.home().join("references");
    fs::create_dir(&dir).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let target = dir.join(format!("{id}.json"));
    for mode in [0o500, 0o000] {
        fs::write(&target, b"archive").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        assert!(remove_retained_archive(&paths, &id).is_ok());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        if target.exists() {
            fs::remove_file(&target).unwrap();
        }
    }
    fs::remove_dir(&dir).unwrap();
    symlink("references", &dir).unwrap();
    assert!(remove_retained_archive(&paths, &id).is_ok());
    assert!(fs::symlink_metadata(&dir).unwrap().is_symlink());
}
