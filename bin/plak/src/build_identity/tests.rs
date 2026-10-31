use super::*;
use std::ffi::OsString;

#[test]
fn manifest_requires_installer_layout_and_safe_file_and_directory() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir().unwrap();
    let snapshot = root
        .path()
        .join("target/.local/lib/plak/release")
        .join("b".repeat(64));
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = snapshot.join("plak");
    std::fs::write(&executable, "image").unwrap();
    let mut image = std::fs::File::open(&executable).unwrap();
    let manifest = snapshot.join("binary-id");
    let contents = format!("abcdef012345 {}", image_fields(&image.metadata().unwrap()));
    std::fs::write(&manifest, &contents).unwrap();
    assert_eq!(
        read_manifest_id(&image, &executable).as_deref(),
        Some("abcdef012345")
    );
    for mode in [0o664, 0o646] {
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(read_manifest_id(&image, &executable), None);
    }
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(read_manifest_id(&image, &executable), None);
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o755)).unwrap();
    let moved = snapshot.join("other");
    std::fs::rename(&manifest, &moved).unwrap();
    symlink(&moved, &manifest).unwrap();
    assert_eq!(read_manifest_id(&image, &executable), None);
    std::fs::remove_file(&manifest).unwrap();
    // rustix has no mkfifo on Apple targets; mkfifo(1) exists on both.
    let made = std::process::Command::new("mkfifo")
        .args(["-m", "600"])
        .arg(&manifest)
        .status()
        .unwrap();
    assert!(made.success());
    assert_eq!(read_manifest_id(&image, &executable), None);
    assert_eq!(
        identify_image(&mut image, None, None, hash_image).as_deref(),
        Some("6105d6cc76af")
    );
    for path in [
        "/tmp/plak",
        "/home/me/target/debug/plak",
        "/home/me/.local/lib/plak/other/aaaaaaaa/plak",
    ] {
        assert_eq!(installed_snapshot_directory(Path::new(path)), None);
    }
}

#[test]
fn manifest_match_skips_hash_and_all_other_cases_hash() {
    let directory = tempfile::tempdir().unwrap();
    let snapshot = directory
        .path()
        .join(".local/lib/plak/maintainer")
        .join("a".repeat(64));
    std::fs::create_dir_all(&snapshot).unwrap();
    let executable = snapshot.join("plak");
    std::fs::write(&executable, "image").unwrap();
    let metadata = std::fs::metadata(&executable).unwrap();
    let fields = image_fields(&metadata);
    let valid = format!("abcdef012345 {fields}\n");
    let mut cases = vec![
        (None, true),
        (Some("malformed".to_owned()), true),
        (Some(format!("zzzzzzzzzzzz {fields}")), true),
        (Some(format!("{valid}extra")), true),
        (Some(format!("{valid}{}", " ".repeat(257))), true),
        (Some(valid.clone()), false),
    ];
    for index in 1..5 {
        let mut words: Vec<_> = valid.split_whitespace().map(str::to_owned).collect();
        words[index] = "0".to_owned();
        cases.push((Some(words.join(" ")), true));
    }
    for (manifest, should_hash) in cases {
        let path = snapshot.join("binary-id");
        if let Some(contents) = manifest {
            std::fs::write(&path, contents).unwrap();
        } else {
            let _ = std::fs::remove_file(&path);
        }
        let calls = std::cell::Cell::new(0);
        let mut image = std::fs::File::open(&executable).unwrap();
        let id = identify_image(&mut image, Some(&executable), None, |file| {
            calls.set(calls.get() + 1);
            hash_image(file)
        })
        .unwrap();
        assert_eq!(calls.get(), usize::from(should_hash));
        assert_eq!(
            id,
            if should_hash {
                "6105d6cc76af"
            } else {
                "abcdef012345"
            }
        );
    }
    let target = directory.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("binary-id"), valid).unwrap();
    let mut image = std::fs::File::open(&executable).unwrap();
    let calls = std::cell::Cell::new(0);
    let _ = identify_image(&mut image, Some(&target.join("plak")), None, |file| {
        calls.set(calls.get() + 1);
        hash_image(file)
    });
    assert_eq!(calls.get(), 1);
}

fn private_home() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    home
}

fn counted_identify(image: &Path, home: &Path) -> (String, usize) {
    identify_with(image, home, true)
}

fn identify_with(image: &Path, home: &Path, writable: bool) -> (String, usize) {
    let calls = std::cell::Cell::new(0);
    let mut file = std::fs::File::open(image).unwrap();
    let cache = Some(IdentityCache { home, writable });
    let id = identify_image(&mut file, Some(image), cache, |file| {
        calls.set(calls.get() + 1);
        hash_image(file)
    })
    .unwrap();
    (id, calls.get())
}

#[test]
fn the_home_cache_skips_the_hash_for_the_same_image_fields() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = private_home();
    let builds = tempfile::tempdir().unwrap();
    let image = builds.path().join("plak");
    std::fs::write(&image, "image").unwrap();
    assert_eq!(
        counted_identify(&image, home.path()),
        ("6105d6cc76af".to_owned(), 1)
    );
    let cache = home.path().join(IDENTITY_CACHE_FILE);
    assert_eq!(
        std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        counted_identify(&image, home.path()),
        ("6105d6cc76af".to_owned(), 0)
    );
    // A rebuild (new size and mtime) is a new key; the old entry stays.
    std::fs::write(&image, "image, rebuilt").unwrap();
    let rebuilt = counted_identify(&image, home.path());
    assert_eq!(rebuilt.1, 1);
    assert_ne!(rebuilt.0, "6105d6cc76af");
    assert_eq!(counted_identify(&image, home.path()), (rebuilt.0, 0));
    assert_eq!(std::fs::read_to_string(&cache).unwrap().lines().count(), 2);
}

#[test]
fn the_home_cache_matches_every_field_and_stays_bounded() {
    let home = private_home();
    let builds = tempfile::tempdir().unwrap();
    let image = builds.path().join("plak");
    std::fs::write(&image, "image").unwrap();
    let fields = cache_fields(&std::fs::metadata(&image).unwrap());
    let cache = home.path().join(IDENTITY_CACHE_FILE);
    for index in 1..6 {
        let mut words: Vec<_> = format!("abcdef012345 {fields}")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        words[index] = "0".to_owned();
        std::fs::write(&cache, words.join(" ")).unwrap();
        assert_eq!(counted_identify(&image, home.path()).1, 1, "field {index}");
    }
    let foreign: String = (0..40)
        .map(|index| format!("{index:012x} 1 {index} 2 3 4\n"))
        .collect();
    std::fs::write(&cache, format!("{foreign}abcdef012345 {fields}\n")).unwrap();
    assert_eq!(
        counted_identify(&image, home.path()),
        ("abcdef012345".to_owned(), 0)
    );
    std::fs::write(&cache, &foreign).unwrap();
    assert_eq!(counted_identify(&image, home.path()).1, 1);
    let kept = std::fs::read_to_string(&cache).unwrap();
    assert_eq!(kept.lines().count(), IDENTITY_CACHE_ENTRIES);
    assert!(kept.starts_with(&format!("6105d6cc76af {fields}\n")));
}

#[test]
fn a_changed_inode_change_time_misses_the_cache() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = private_home();
    let builds = tempfile::tempdir().unwrap();
    let image = builds.path().join("plak");
    std::fs::write(&image, "image").unwrap();
    assert_eq!(counted_identify(&image, home.path()).1, 1);
    let before = std::fs::metadata(&image).unwrap();
    // chmod moves ctime and nothing in the manifest's fields.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o640)).unwrap();
    let after = std::fs::metadata(&image).unwrap();
    assert_eq!(image_fields(&before), image_fields(&after));
    assert_ne!(cache_fields(&before), cache_fields(&after));
    assert_eq!(counted_identify(&image, home.path()).1, 1);
    assert_eq!(counted_identify(&image, home.path()).1, 0);
}

#[test]
fn a_read_only_command_reads_the_cache_and_never_writes_it() {
    let home = private_home();
    let builds = tempfile::tempdir().unwrap();
    let image = builds.path().join("plak");
    std::fs::write(&image, "image").unwrap();
    let cache = home.path().join(IDENTITY_CACHE_FILE);
    assert_eq!(identify_with(&image, home.path(), false).1, 1);
    assert!(
        !cache.exists(),
        "an inspection command created product state"
    );
    assert_eq!(identify_with(&image, home.path(), true).1, 1);
    assert_eq!(
        identify_with(&image, home.path(), false),
        ("6105d6cc76af".to_owned(), 0)
    );
    let leftovers: Vec<_> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, [OsString::from(IDENTITY_CACHE_FILE)]);
}

#[test]
fn an_unsafe_home_or_cache_file_falls_back_to_hashing() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let builds = tempfile::tempdir().unwrap();
    let image = builds.path().join("plak");
    std::fs::write(&image, "image").unwrap();
    let fields = cache_fields(&std::fs::metadata(&image).unwrap());
    let entry = format!("abcdef012345 {fields}\n");

    let shared = private_home();
    std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(shared.path().join(IDENTITY_CACHE_FILE), &entry).unwrap();
    assert_eq!(counted_identify(&image, shared.path()).1, 1);
    assert_eq!(
        std::fs::read_to_string(shared.path().join(IDENTITY_CACHE_FILE)).unwrap(),
        entry,
        "a home that is not private is neither read nor written"
    );

    let missing = private_home();
    let absent = missing.path().join("absent");
    assert_eq!(counted_identify(&image, &absent).1, 1);
    assert!(!absent.exists(), "the cache never creates a home");

    let home = private_home();
    let cache = home.path().join(IDENTITY_CACHE_FILE);
    std::fs::write(&cache, &entry).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(counted_identify(&image, home.path()).0, "6105d6cc76af");
    std::fs::remove_file(&cache).unwrap();
    let elsewhere = builds.path().join("forged");
    std::fs::write(&elsewhere, &entry).unwrap();
    symlink(&elsewhere, &cache).unwrap();
    assert_eq!(counted_identify(&image, home.path()).0, "6105d6cc76af");
    // The fresh hash replaced the link with this home's own private file.
    assert!(std::fs::symlink_metadata(&cache).unwrap().is_file());
    assert_eq!(
        counted_identify(&image, home.path()),
        ("6105d6cc76af".to_owned(), 0)
    );
}

#[test]
fn a_matching_manifest_is_used_before_the_home_cache() {
    let home = private_home();
    let directory = tempfile::tempdir().unwrap();
    let snapshot = directory
        .path()
        .join(".local/lib/plak/release")
        .join("c".repeat(64));
    std::fs::create_dir_all(&snapshot).unwrap();
    let executable = snapshot.join("plak");
    std::fs::write(&executable, "image").unwrap();
    let fields = image_fields(&std::fs::metadata(&executable).unwrap());
    std::fs::write(
        snapshot.join("binary-id"),
        format!("abcdef012345 {fields}\n"),
    )
    .unwrap();
    assert_eq!(
        counted_identify(&executable, home.path()),
        ("abcdef012345".to_owned(), 0)
    );
    assert!(!home.path().join(IDENTITY_CACHE_FILE).exists());
}

#[test]
fn run_and_agent_startup_leave_the_identity_cache_cold_without_waiting_for_the_worker() {
    let gate = BACKGROUND_IDENTITY_TEST_GATE.lock().unwrap();
    assert_eq!(ready_binary_id(), None);
    for arguments in [
        vec!["run", "--account", "grok", "--model", "m", "--", "hello"],
        vec![
            "run",
            "--session",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "--",
            "hello",
        ],
        vec!["agent"],
    ] {
        let command = crate::parse(arguments.into_iter().map(OsString::from)).unwrap();
        crate::log_command_start(&command);
        assert_eq!(
            ready_binary_id(),
            None,
            "startup must not hash on the calling thread"
        );
    }
    drop(gate);
}

#[test]
fn an_unrecorded_launch_is_its_own_installed_build() {
    assert!(!newer_build_installed());
    assert_eq!(
        installed_executable(),
        std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok())
    );
}

#[test]
fn a_replaced_file_is_a_different_build() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plak");
    std::fs::write(&path, b"old").unwrap();
    let old = FileIdentity::of(&path).unwrap();
    assert!(FileIdentity::of(&path) == Some(old.clone()));
    let staged = dir.path().join("plak.new");
    std::fs::write(&staged, b"new").unwrap();
    std::fs::rename(&staged, &path).unwrap();
    assert!(FileIdentity::of(&path) != Some(old));
}

#[test]
fn a_replaced_install_is_detected_as_a_newer_build() {
    use super::interactive::installed_build_is_newer;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plak");
    std::fs::write(&path, b"old").unwrap();
    let running = FileIdentity::of(&path).unwrap();
    assert!(!installed_build_is_newer(Some(&running), Some(&running)));
    assert!(!installed_build_is_newer(None, Some(&running)));
    assert!(!installed_build_is_newer(Some(&running), None));
    let staged = dir.path().join("plak.new");
    std::fs::write(&staged, b"new").unwrap();
    std::fs::rename(&staged, &path).unwrap();
    let installed = FileIdentity::of(&path).unwrap();
    assert!(installed_build_is_newer(Some(&running), Some(&installed)));
}

/// A `PATH` name follows the install symlink. Replacing the target is a newer
/// build; the name itself does not change.
#[test]
fn a_path_name_follows_its_symlink_and_sees_a_replaced_target() {
    use super::interactive::{installed_build_is_newer, resolve_installed_name};
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempfile::tempdir().unwrap();
    let bin_dir = root.path().join("bin");
    let real_dir = root.path().join("real");
    std::fs::create_dir(&bin_dir).unwrap();
    std::fs::create_dir(&real_dir).unwrap();
    let target = real_dir.join("plak");
    std::fs::write(&target, b"old").unwrap();
    let mut permissions = std::fs::metadata(&target).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&target, permissions).unwrap();
    symlink(&target, bin_dir.join("plak")).unwrap();
    let path = std::env::join_paths([bin_dir]).unwrap();

    let resolved = resolve_installed_name("plak".as_ref(), Some(path.as_os_str())).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&target).unwrap());
    let running = FileIdentity::of(&resolved).unwrap();

    let staged = real_dir.join("plak.new");
    std::fs::write(&staged, b"new-bytes").unwrap();
    let mut permissions = std::fs::metadata(&staged).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&staged, permissions).unwrap();
    std::fs::rename(&staged, &target).unwrap();

    let again = resolve_installed_name("plak".as_ref(), Some(path.as_os_str())).unwrap();
    assert_eq!(again, std::fs::canonicalize(&target).unwrap());
    let installed = FileIdentity::of(&again).unwrap();
    assert!(installed_build_is_newer(Some(&running), Some(&installed)));
}
