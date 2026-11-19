use std::sync::Barrier;

use super::*;

pub(crate) fn publish_fixture(home: &Path, binary: &[u8]) -> (PathBuf, String) {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("plakd"), binary).unwrap();
    for name in NAMES.into_iter().skip(1) {
        fs::write(source.path().join(name), name).unwrap();
    }
    let files = shipping::collect(source.path()).unwrap();
    let id = shipping::snapshot_id(&files);
    let path = Layout::from_home(home, Flavor::Release)
        .unwrap()
        .publish(source.path(), &files, &id)
        .unwrap();
    (path, id)
}

#[test]
fn concurrent_same_bytes_publish_one_snapshot_and_each_sidecar_once() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    let start = Barrier::new(2);
    let (first, second) = std::thread::scope(|threads| {
        let publish = || {
            start.wait();
            publish_fixture(&home, b"candidate")
        };
        let first = threads.spawn(publish);
        let second = threads.spawn(publish);
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first, second);
    assert_eq!(fs::read_dir(first.0.parent().unwrap()).unwrap().count(), 1);
    let layout = Layout::from_home(&home, Flavor::Release).unwrap();
    for name in NAMES.into_iter().skip(1) {
        assert_eq!(
            fs::read_dir(layout.root.join("sidecars").join(name))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn physical_install_roots_allow_linked_home_local_and_lib() {
    let base = tempfile::tempdir().unwrap();
    let base = base.path().canonicalize().unwrap();
    let home = base.join("home");
    let shared = base.join("local");
    let library = base.join("library");
    for dir in [&home, &shared, &library] {
        fs::create_dir(dir).unwrap();
    }
    let linked_home = base.join("login");
    symlink(&home, &linked_home).unwrap();
    symlink(&shared, home.join(".local")).unwrap();
    symlink(&library, shared.join("lib")).unwrap();
    let (snapshot, id) = publish_fixture(&linked_home, b"candidate");
    assert_eq!(snapshot, library.join("plak/release").join(id));
    let layout = Layout::from_home(&linked_home, Flavor::Release).unwrap();
    layout.link(&snapshot, None, false).unwrap();
    assert_eq!(
        fs::read_link(shared.join("bin/plakd")).unwrap(),
        snapshot.join("plakd")
    );
    assert_eq!(
        fs::read_link(snapshot.join("plakous-js-host"))
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap(),
        library.join("plak/sidecars/plakous-js-host")
    );
}

#[test]
fn symlinked_digest_directories_are_not_followed() {
    let home = tempfile::tempdir().unwrap();
    let (snapshot, _) = publish_fixture(home.path(), b"candidate");
    let worker = fs::read_link(snapshot.join("plakous-js-host")).unwrap();
    let digest = worker.parent().unwrap();
    let moved = digest.with_extension("elsewhere");
    fs::rename(digest, &moved).unwrap();
    symlink(moved, digest).unwrap();
    let source = tempfile::tempdir().unwrap();
    for name in NAMES {
        fs::write(
            source.path().join(name),
            if name == "plakd" { "candidate" } else { name },
        )
        .unwrap();
    }
    let files = shipping::collect(source.path()).unwrap();
    let layout = Layout::from_home(home.path(), Flavor::Release).unwrap();
    let id = shipping::snapshot_id(&files);
    let error = layout.publish(source.path(), &files, &id).unwrap_err();
    assert!(error.message().contains("symlinked installation directory"));
}

#[test]
fn source_links_are_repaired_without_creating_cargo_entries_or_touching_foreign_ones() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    let (snapshot, _) = publish_fixture(&home, b"candidate");
    let mut layout = Layout::from_home(&home, Flavor::Release).unwrap();
    layout.cargo_bin = home.join("cargo/bin");
    ensure(&layout.cargo_bin).unwrap();
    ensure(&layout.bin).unwrap();
    let target = home.join("build");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("plakd"), b"source target").unwrap();
    symlink(target.join("plakd"), layout.bin.join("plakd")).unwrap();
    symlink(target.join("plakd"), layout.cargo_bin.join("plakd")).unwrap();
    let foreign = layout.cargo_bin.join("plakous-py-host");
    fs::write(&foreign, b"not ours").unwrap();
    let installed = || fs::read_link(layout.bin.join("plakd")).unwrap();
    let error = layout.link(&snapshot, Some(&target), true).unwrap_err();
    assert!(error.message().contains("refusing to replace non-Plak"));
    assert_eq!(installed(), target.join("plakd"));
    assert_eq!(fs::read(&foreign).unwrap(), b"not ours");
    fs::remove_file(&foreign).unwrap();
    let obsolete = layout.bin.join("monty");
    symlink(layout.root.join("maintainer/old/monty"), &obsolete).unwrap();
    layout.link(&snapshot, Some(&target), true).unwrap();
    assert_eq!(installed(), snapshot.join("plakd"));
    assert_eq!(
        fs::read_link(layout.cargo_bin.join("plakd")).unwrap(),
        snapshot.join("plakd")
    );
    assert_eq!(fs::read_dir(&layout.cargo_bin).unwrap().count(), 1);
    assert!(fs::symlink_metadata(&obsolete).is_err());
    symlink(home.join("unrelated"), &obsolete).unwrap();
    layout.link(&snapshot, Some(&target), true).unwrap();
    assert_eq!(fs::read_link(&obsolete).unwrap(), home.join("unrelated"));
}
