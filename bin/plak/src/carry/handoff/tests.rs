use super::*;
use crate::home::paths::PlakPaths;
use std::os::unix::fs::PermissionsExt as _;

fn handoff(cause: &str) -> ReloadHandoff {
    ReloadHandoff {
        cause: Some(cause.to_owned()),
        ..ReloadHandoff::default()
    }
}

#[test]
fn a_written_handoff_is_read_once_and_deleted() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let written = handoff("its model adapter profile differs");
    let path = written.write(&paths, "session").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(ReloadHandoff::take(&path, paths.home()), Some(written));
    assert!(!path.exists());
    assert_eq!(ReloadHandoff::take(&path, paths.home()), None);
}

#[test]
fn a_handoff_that_is_not_private_or_not_in_the_carry_directory_is_ignored() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let readable = handoff("cause").write(&paths, "session").unwrap();
    fs::set_permissions(&readable, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(ReloadHandoff::take(&readable, paths.home()), None);
    assert!(readable.exists(), "a refused file is not deleted");

    let elsewhere = paths.home().join("handoff.json");
    fs::write(&elsewhere, serde_json::to_vec(&handoff("cause")).unwrap()).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(ReloadHandoff::take(&elsewhere, paths.home()), None);

    let linked = paths.home().join("carry").join("linked.reload.json");
    std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
    assert_eq!(ReloadHandoff::take(&linked, paths.home()), None);
}

#[test]
fn another_version_reads_as_no_handoff_and_a_cause_is_one_bounded_line() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let future = ReloadHandoff {
        version: VERSION + 1,
        ..handoff("cause")
    };
    let path = future.write(&paths, "session").unwrap();
    assert_eq!(ReloadHandoff::take(&path, paths.home()), None);

    let long = format!("{}\nsecond line", "x".repeat(MAX_CAUSE_CHARS + 10));
    let path = handoff(&long).write(&paths, "session").unwrap();
    let read = ReloadHandoff::take(&path, paths.home()).unwrap();
    assert_eq!(read.cause.unwrap().chars().count(), MAX_CAUSE_CHARS);
}

#[test]
fn a_carry_directory_others_can_reach_reads_no_handoff_and_stale_sidecars_are_pruned() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let stale = handoff("old").write(&paths, "session").unwrap();
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60),
        )
        .unwrap();
    let fresh = handoff("new").write(&paths, "session").unwrap();
    assert!(
        !stale.exists(),
        "a day-old sidecar is pruned when another is written"
    );
    let carry = paths.home().join("carry");
    fs::set_permissions(&carry, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(ReloadHandoff::take(&fresh, paths.home()), None);
    fs::set_permissions(&carry, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(ReloadHandoff::take(&fresh, paths.home()).is_some());
}
