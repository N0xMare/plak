use super::*;
use std::os::unix::fs::PermissionsExt as _;

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn inside(path: &Path, rule: StorageRule) -> StorageViolation {
    StorageViolation {
        path: path.to_path_buf(),
        rule,
        place: Place::Store,
    }
}

fn private_layout() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(home.path()).unwrap().join("state");
    let subscriptions = root.join("plak/subscriptions");
    for name in ["codex", "catalog", "compatibility"] {
        fs::create_dir_all(subscriptions.join(name)).unwrap();
    }
    for directory in [
        root.clone(),
        root.join("plak"),
        subscriptions.clone(),
        subscriptions.join("codex"),
        subscriptions.join("catalog"),
        subscriptions.join("compatibility"),
    ] {
        set_mode(&directory, 0o700);
    }
    let record = subscriptions.join("codex/credential.json");
    fs::write(&record, b"{}").unwrap();
    set_mode(&record, 0o600);
    (home, root)
}

#[test]
fn a_private_layout_breaks_no_rule() {
    let (_home, root) = private_layout();
    assert_eq!(find_violation(&root, None), None);
    assert_eq!(find_violation(&root.join("absent/state"), None), None);
}

#[test]
fn each_broken_rule_names_its_path_and_the_repair() {
    let (_home, root) = private_layout();
    let codex = root.join("plak/subscriptions/codex");
    set_mode(&codex, 0o755);
    let found = find_violation(&root, None).unwrap();
    assert_eq!(
        found,
        inside(&codex, StorageRule::DirectoryMode { mode: 0o755 })
    );
    assert_eq!(
        found.to_string(),
        format!(
            "{0} has mode 0755 but private directories must be 0700; run `chmod 700 '{0}'`",
            codex.display()
        )
    );
    set_mode(&codex, 0o700);

    let record = codex.join("credential.json");
    set_mode(&record, 0o644);
    assert_eq!(
        find_violation(&root, None),
        Some(inside(&record, StorageRule::FileMode { mode: 0o644 }))
    );
    set_mode(&record, 0o600);

    let alias = root.join("alias");
    fs::hard_link(&record, &alias).unwrap();
    let found = find_violation(&root, None).unwrap();
    assert_eq!(found, inside(&record, StorageRule::HardLinks { links: 2 }));
    assert!(found.to_string().contains("exactly one"));
    fs::remove_file(&alias).unwrap();

    let link = root.join("plak/subscriptions/catalog/codex.json");
    std::os::unix::fs::symlink(&record, &link).unwrap();
    let found = find_violation(&root, None).unwrap();
    assert_eq!(found, inside(&link, StorageRule::SymbolicLink));
    assert!(found.to_string().contains("move it aside"));
    fs::remove_file(&link).unwrap();

    set_mode(&root, 0o777);
    assert_eq!(
        find_violation(&root, None),
        Some(outside(&root, StorageRule::RootWritable { mode: 0o777 }))
    );
}

#[test]
fn the_state_root_follows_the_stores_own_rules() {
    let (_home, root) = private_layout();
    // The store admits a state root other tools share: group and other
    // may read and search it.
    set_mode(&root, 0o755);
    assert_eq!(find_violation(&root, None), None);

    set_mode(&root, 0o500);
    let found = find_violation(&root, None).unwrap();
    assert_eq!(
        found,
        outside(&root, StorageRule::OwnerAccess { mode: 0o500 })
    );
    assert!(
        found
            .to_string()
            .ends_with(&format!("run `chmod u+rwx '{}'`", root.display()))
    );

    set_mode(&root, 0o1700);
    let found = find_violation(&root, None).unwrap();
    assert_eq!(
        found,
        outside(&root, StorageRule::SpecialBits { mode: 0o1700 })
    );
    assert!(found.to_string().contains("sticky"));
    assert!(
        found
            .to_string()
            .ends_with(&format!("run `chmod a-st '{}'`", root.display()))
    );
    set_mode(&root, 0o700);
}

#[test]
fn only_the_names_the_store_opens_are_inspected() {
    let (_home, root) = private_layout();
    let subscriptions = root.join("plak/subscriptions");
    let stray = subscriptions.join(".DS_Store");
    fs::write(&stray, b"finder").unwrap();
    set_mode(&stray, 0o644);
    assert_eq!(find_violation(&root, None), None);

    let lock = subscriptions.join("compatibility/.codex.lock");
    fs::write(&lock, b"").unwrap();
    set_mode(&lock, 0o644);
    assert_eq!(
        find_violation(&root, None),
        Some(inside(&lock, StorageRule::FileMode { mode: 0o644 }))
    );
}

#[test]
fn another_accounts_files_are_never_blamed() {
    let (_home, root) = private_layout();
    let compatibility = root.join("plak/subscriptions/compatibility");
    let codex = compatibility.join("codex.json");
    let claude = compatibility.join("claude.json");
    for receipt in [&codex, &claude] {
        fs::write(receipt, b"{}").unwrap();
        set_mode(receipt, 0o640);
    }
    assert_eq!(
        find_violation(&root, None).map(|found| found.path),
        Some(codex.clone())
    );
    assert_eq!(
        find_violation(&root, Some(NativeProvider::Claude)).map(|found| found.path),
        Some(claude)
    );
    assert_eq!(find_violation(&root, Some(NativeProvider::Xai)), None);
}

#[test]
fn the_credential_locks_are_inspected_and_a_credential_needs_a_login() {
    let (_home, root) = private_layout();
    let codex = root.join("plak/subscriptions/codex");
    for name in [".credential.json.lock", ".credential.json.refresh.lock"] {
        let lock = codex.join(name);
        fs::write(&lock, b"").unwrap();
        set_mode(&lock, 0o644);
        assert_eq!(
            find_violation(&root, Some(NativeProvider::Codex)),
            Some(inside(&lock, StorageRule::FileMode { mode: 0o644 }))
        );
        set_mode(&lock, 0o600);
    }

    let record = codex.join("credential.json");
    fs::remove_file(&record).unwrap();
    std::os::unix::fs::symlink(root.join("elsewhere"), &record).unwrap();
    let found = find_violation(&root, Some(NativeProvider::Codex)).unwrap();
    assert_eq!(found, inside(&record, StorageRule::SymbolicLink));
    assert!(
        found
            .to_string()
            .ends_with("move it aside, then run `plak login codex`"),
        "{found}"
    );
}

#[test]
fn an_ancestor_the_user_cannot_read_gets_a_relocation_remedy() {
    let (home, _root) = private_layout();
    let closed = fs::canonicalize(home.path()).unwrap().join("closed");
    fs::create_dir_all(closed.join("state")).unwrap();
    set_mode(&closed.join("state"), 0o700);
    set_mode(&closed, 0o100);
    let found = find_violation(&closed.join("state"), None);
    set_mode(&closed, 0o700);
    // Root bypasses permission bits.
    if rustix::process::geteuid().is_root() {
        return;
    }
    let found = found.unwrap();
    assert_eq!(found, outside(&closed, StorageRule::NoAccess));
    assert!(found.to_string().ends_with(RELOCATE), "{found}");
}

#[test]
fn paths_plak_did_not_create_get_only_a_relocation_remedy() {
    let (home, root) = private_layout();
    let linked = fs::canonicalize(home.path()).unwrap().join("linked");
    std::os::unix::fs::symlink(&root, &linked).unwrap();
    let found = find_violation(&linked.join("state"), None).unwrap();
    assert_eq!(found, outside(&linked, StorageRule::SymbolicLink));
    assert!(found.to_string().ends_with(RELOCATE), "{found}");
    assert!(!found.to_string().contains("replace"), "{found}");

    let file = fs::canonicalize(home.path()).unwrap().join("file");
    fs::write(&file, b"").unwrap();
    let found = find_violation(&file.join("state"), None).unwrap();
    assert_eq!(found, outside(&file, StorageRule::NotDirectory));
    assert!(found.to_string().ends_with(RELOCATE), "{found}");

    let foreign = outside(
        Path::new("/var/lib/x"),
        StorageRule::Owner {
            found: 0,
            expected: 501,
        },
    );
    assert_eq!(
        foreign.to_string(),
        format!("/var/lib/x is owned by uid 0, not the current user (uid 501); {RELOCATE}")
    );
    assert_eq!(
        inside(
            Path::new("/state/plak/subscriptions/codex"),
            StorageRule::Owner {
                found: 0,
                expected: 501,
            },
        )
        .to_string(),
        "/state/plak/subscriptions/codex is owned by uid 0, not the current user (uid 501); `chown` it back to the current user"
    );
}

#[test]
fn a_missing_state_root_needs_a_writable_parent() {
    let (home, _root) = private_layout();
    let parent = fs::canonicalize(home.path()).unwrap().join("sealed");
    fs::create_dir(&parent).unwrap();
    set_mode(&parent, 0o500);
    let found = find_violation(&parent.join("state"), None);
    set_mode(&parent, 0o700);
    // Root bypasses permission bits, so the refusal only exists for an
    // ordinary user.
    if rustix::process::geteuid().is_root() {
        return;
    }
    let found = found.unwrap();
    assert_eq!(found, outside(&parent, StorageRule::NotWritable));
    assert!(found.to_string().ends_with(RELOCATE), "{found}");
}

#[test]
fn a_state_root_with_dot_dot_is_named_before_the_filesystem() {
    let (_home, root) = private_layout();
    let detour = root.join("plak/../state");
    let found = find_violation(&detour, None).unwrap();
    assert_eq!(found, outside(&detour, StorageRule::NotNormal));
    assert!(found.to_string().contains("without them"), "{found}");
}

#[test]
fn remedies_quote_paths_for_the_shell() {
    let found = inside(
        Path::new("/state/it's/plak/subscriptions"),
        StorageRule::DirectoryMode { mode: 0o2700 },
    );
    assert_eq!(
        found.to_string(),
        r"/state/it's/plak/subscriptions has mode 2700 but private directories must be 0700; run `chmod 00700 '/state/it'\''s/plak/subscriptions'`"
    );
}
