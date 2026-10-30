use super::*;

enum SizeFault {
    Unreadable,
    Vanish,
}

thread_local! {
    static SIZE_FAULT: std::cell::RefCell<Option<(PathBuf, SizeFault)>> = const { std::cell::RefCell::new(None) };
    static REMOVE_FAULT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static RECREATE_DATABASE: std::cell::RefCell<Option<(PathBuf, bool)>> = const { std::cell::RefCell::new(None) };
    static CHECK_CURRENT_FENCES: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    static PROBE_VANISH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static NOTICE_SIZE_BUDGET: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

pub(super) fn notice_size_budget() -> Option<Duration> {
    NOTICE_SIZE_BUDGET.with(std::cell::Cell::get)
}

/// Removes the armed path as the probe reaches it, as a concurrent job
/// output retention sweep would.
pub(super) fn before_probe(path: &Path) -> Result<(), String> {
    PROBE_VANISH.with(|vanish| {
        let mut vanish = vanish.borrow_mut();
        if vanish.as_deref() == Some(path) {
            fs::remove_file(vanish.take().unwrap()).map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}

#[test]
fn explicit_prune_selects_only_old_telemetry_and_its_sidecars() {
    let (_home, paths, _old) = fixture();
    paths.ensure_layout().unwrap();
    let old = paths.telemetry_dir().join("telemetry-v1.sqlite3");
    let legacy = paths.telemetry_dir().join("telemetry.sqlite3");
    for path in [&old, &legacy] {
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch("CREATE TABLE fixture(value);")
            .unwrap();
    }
    let orphan = paths.telemetry_dir().join("telemetry-v2.sqlite3-wal");
    fs::write(&orphan, b"orphaned sidecar").unwrap();
    let current = paths.telemetry_database();
    let newer = paths.telemetry_dir().join("telemetry-v999.sqlite3");
    let unrelated = paths.telemetry_dir().join("notes.txt");
    for path in [&current, &newer, &unrelated] {
        fs::write(path, b"retained").unwrap();
    }
    let mut report = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report).unwrap();
    assert!(old.exists(), "confirmation must not remove telemetry");
    assert!(
        String::from_utf8(report)
            .unwrap()
            .contains("telemetry-v1.sqlite3")
    );
    prune_confirmed(&paths, &inventory).unwrap();
    for path in [&old, &legacy, &orphan] {
        assert!(!path.exists(), "{}", path.display());
    }
    for path in [&current, &newer, &unrelated] {
        assert_eq!(fs::read(path).unwrap(), b"retained");
    }
}

#[test]
fn explicit_prune_retains_noncanonical_telemetry_names() {
    let (_home, paths, _old) = fixture();
    paths.ensure_layout().unwrap();
    let malformed = [
        "telemetry-v+5.sqlite3",
        "telemetry-v05.sqlite3",
        "telemetry-vbad.sqlite3",
        "telemetry-v+5.sqlite3-wal",
    ];
    for name in malformed {
        fs::write(paths.telemetry_dir().join(name), b"retained").unwrap();
    }
    let old = paths.telemetry_dir().join("telemetry-v5.sqlite3");
    drop(rusqlite::Connection::open(&old).unwrap());
    prune_legacy(&paths).unwrap();
    assert!(!old.exists());
    for name in malformed {
        assert_eq!(
            fs::read(paths.telemetry_dir().join(name)).unwrap(),
            b"retained"
        );
    }
}

#[test]
fn old_telemetry_prune_refuses_busy_database_and_symlinks() {
    let (_home, paths, old_state) = fixture();
    paths.ensure_layout().unwrap();
    let old = paths.telemetry_dir().join("telemetry-v1.sqlite3");
    let database = rusqlite::Connection::open(&old).unwrap();
    database
        .execute_batch("CREATE TABLE fixture(value); BEGIN IMMEDIATE;")
        .unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
    assert!(old_state.exists());
    drop(database);
    fs::remove_file(&old).unwrap();
    let retained = paths.telemetry_dir().join("retained");
    fs::write(&retained, b"retained").unwrap();
    std::os::unix::fs::symlink(&retained, &old).unwrap();
    assert!(
        prune_legacy(&paths)
            .unwrap_err()
            .contains("unsafe telemetry")
    );
    assert!(old_state.exists());
    assert_eq!(fs::read(retained).unwrap(), b"retained");
}

#[test]
fn legacy_cleanup_does_not_walk_current_job_output_files() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_home, paths, old) = fixture();
    let session = paths
        .job_outputs()
        .join("00000000-0000-0000-0000-0000000000b0");
    fs::create_dir_all(&session).unwrap();
    for directory in [paths.state_dir(), paths.job_outputs(), session.clone()] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let file = session.join("b-p1-0123456789abcdef.log");
    fs::write(&file, b"output").unwrap();
    // A sweep deleting the file mid-walk must not fail the cleanup.
    PROBE_VANISH.with(|vanish| *vanish.borrow_mut() = Some(file.clone()));
    prune_legacy(&paths).unwrap();
    assert!(!old.exists());
    assert!(file.exists(), "the probe walked into the job output root");
    PROBE_VANISH.with(|vanish| *vanish.borrow_mut() = None);
}

pub(super) fn before_sidecar_cleanup() -> Result<(), String> {
    CHECK_CURRENT_FENCES.with(|paths| {
        for path in paths.borrow_mut().drain(..) {
            let connection = rusqlite::Connection::open(path).unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            assert!(connection.execute_batch("BEGIN IMMEDIATE").is_err());
        }
    });
    RECREATE_DATABASE.with(|path| {
        if let Some((path, symlink)) = path.borrow_mut().take() {
            if symlink {
                std::os::unix::fs::symlink("missing", &path).map_err(|error| error.to_string())?;
            } else {
                fs::write(&path, b"replacement").map_err(|error| error.to_string())?;
            }
            for suffix in ["wal", "shm", "journal"] {
                fs::write(
                    path.with_extension(format!("sqlite3-{suffix}")),
                    b"replacement",
                )
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    })
}

#[test]
fn legacy_sidecar_cleanup_preserves_recreated_database_sidecars() {
    for symlink in [false, true] {
        let (_home, paths, _old) = fixture();
        let database = paths.state_root().join("runtime.sqlite3");
        drop(rusqlite::Connection::open(&database).unwrap());
        RECREATE_DATABASE.with(|path| *path.borrow_mut() = Some((database.clone(), symlink)));
        prune_legacy(&paths).unwrap();
        for suffix in ["wal", "shm", "journal"] {
            assert_eq!(
                fs::read(database.with_extension(format!("sqlite3-{suffix}"))).unwrap(),
                b"replacement"
            );
        }
    }
}

#[test]
fn legacy_cleanup_holds_current_fences_through_dotdot_home() {
    let (_home, paths, old) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    rusqlite::Connection::open(paths.subagents_database())
        .unwrap()
        .execute_batch(
            "PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT, launch_checkout TEXT);",
        )
        .unwrap();
    CHECK_CURRENT_FENCES.with(|fences| {
        *fences.borrow_mut() = vec![paths.product_database(), paths.subagents_database()];
    });
    fs::create_dir(paths.home().join("alias")).unwrap();
    let paths = PlakPaths::explicit(paths.home().join("alias/.."));
    prune_legacy(&paths).unwrap();
    assert!(!old.exists());
}

pub(super) fn before_remove(path: &Path) -> Result<(), String> {
    REMOVE_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.as_deref() == Some(path) {
            fs::remove_file(fault.take().unwrap()).map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}

pub(super) fn before_size(path: &Path) -> io::Result<()> {
    SIZE_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.as_ref().is_none_or(|(target, _)| target != path) {
            return Ok(());
        }
        match fault.take().unwrap().1 {
            SizeFault::Unreadable => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            SizeFault::Vanish => fs::remove_file(path),
        }
    })
}

#[test]
fn unreadable_and_vanishing_entries_do_not_fail_stateful_admission_or_doctor() {
    // The injected fault, not the 20 ms launch budget, decides each size:
    // on a loaded host the budget alone can expire before the entry is
    // reached, which `exhausted_notice_budget_keeps_entry_count_and_prune_hint`
    // covers on its own.
    NOTICE_SIZE_BUDGET.with(|budget| budget.set(Some(Duration::from_secs(60))));
    for doctor in [false, true] {
        for unreadable in [false, true] {
            let (_root, paths, old) = fixture();
            let entry = old.join("changing");
            fs::write(&entry, b"bytes").unwrap();
            SIZE_FAULT.with(|fault| {
                *fault.borrow_mut() = Some((
                    entry,
                    if unreadable {
                        SizeFault::Unreadable
                    } else {
                        SizeFault::Vanish
                    },
                ));
            });
            let mut output = Vec::new();
            if doctor {
                let warnings = report_legacy_state_until(&paths, &mut output, None).unwrap();
                assert_eq!(warnings, usize::from(unreadable));
            } else {
                ensure_usable_state_with_notice(&paths, &mut output, &Once::new()).unwrap();
                assert!(paths.state_dir().exists());
            }
            let text = String::from_utf8(output).unwrap();
            assert!(
                text.contains(if unreadable {
                    if doctor {
                        "size unavailable:"
                    } else {
                        "size unknown"
                    }
                } else {
                    "0 bytes"
                }),
                "{text}"
            );
        }
    }
}

#[test]
fn notice_inspects_once_and_ignores_output_failures() {
    let (_root, paths, old) = fixture();
    let once = Once::new();
    let mut output = Vec::new();
    ensure_usable_state_with_notice(&paths, &mut output, &once).unwrap();
    let entry = old.join("must-not-be-inspected");
    fs::write(&entry, b"keep").unwrap();
    SIZE_FAULT.with(|fault| *fault.borrow_mut() = Some((entry.clone(), SizeFault::Vanish)));
    ensure_usable_state_with_notice(&paths, &mut output, &once).unwrap();
    assert!(entry.exists());
    SIZE_FAULT.with(|fault| *fault.borrow_mut() = None);
    let mut full = &mut [][..];
    ensure_usable_state_with_notice(&paths, &mut full, &Once::new()).unwrap();
}

#[test]
fn exhausted_startup_size_budget_returns_unknown_without_walking() {
    let (_root, _paths, old) = fixture();
    assert_eq!(
        file_bytes(&old, Some(Instant::now())).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[test]
fn exhausted_notice_budget_keeps_entry_count_and_prune_hint() {
    let (_root, paths, _old) = fixture();
    fs::write(paths.state_root().join("flat"), b"legacy").unwrap();
    let deadline = Instant::now();
    let entries = legacy_state_until(&paths, Some(deadline)).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.bytes.as_ref().unwrap_err().kind() == io::ErrorKind::TimedOut)
    );
    let mut notice = Vec::new();
    report_other_state_once_until(&paths, &mut notice, &Once::new(), Some(deadline));
    let text = String::from_utf8(notice).unwrap();
    assert!(
        text.contains("2 legacy state entries (size unknown) remain"),
        "{text}"
    );
    assert!(text.contains("`plak state prune-legacy`"), "{text}");
}

#[test]
fn prune_lists_and_removes_only_references_absent_from_the_current_store() {
    let (_root, paths, old) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    let dir = paths.home().join("references");
    fs::create_dir(&dir).unwrap();
    let current = dir.join(format!("{}.json", record.id));
    let orphan = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
    fs::write(&current, b"current reference").unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    let unrelated = dir.join("not-a-session.json");
    fs::write(&unrelated, b"unrelated").unwrap();
    let mut output = Vec::new();
    let inventory = confirm_prune_legacy_options(
        &paths,
        false,
        PruneOptions {
            forget_missing_launches: false,
            prune_orphaned_references: true,
        },
        true,
        &mut &b"yes\n"[..],
        &mut output,
    )
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(
        text.contains(&format!(
            "Orphaned reference: {} · 6 bytes",
            orphan.display()
        )),
        "{text}"
    );
    assert!(!text.contains(&current.display().to_string()));
    assert_eq!(prune_confirmed(&paths, &inventory).unwrap(), 2);
    assert!(!old.exists());
    assert!(!orphan.exists());
    assert!(current.exists());
    assert!(unrelated.exists());
}

#[test]
fn unsafe_orphaned_references_refuse_before_any_removal() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let (root, paths, old) = fixture();
    let dir = paths.home().join("references");
    fs::create_dir(&dir).unwrap();
    let orphan = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
    let outside = root.path().join("outside");
    fs::write(&outside, b"keep").unwrap();
    symlink(&outside, &orphan).unwrap();
    assert!(
        confirm_prune_legacy_options(
            &paths,
            true,
            PruneOptions {
                forget_missing_launches: false,
                prune_orphaned_references: true
            },
            false,
            &mut &b""[..],
            &mut Vec::new()
        )
        .err()
        .unwrap()
        .contains("unsafe orphaned reference")
    );
    assert!(old.exists());
    assert_eq!(fs::read(&outside).unwrap(), b"keep");
    fs::remove_file(&orphan).unwrap();
    fs::write(&orphan, b"archive").unwrap();
    fs::set_permissions(&orphan, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(
        confirm_prune_legacy_options(
            &paths,
            true,
            PruneOptions {
                forget_missing_launches: false,
                prune_orphaned_references: true
            },
            false,
            &mut &b""[..],
            &mut Vec::new()
        )
        .err()
        .unwrap()
        .contains("unsafe orphaned reference")
    );
    assert!(old.exists());
    assert!(orphan.exists());
}

#[test]
fn prune_keeps_current_sessions_source_references_even_when_explicitly_selected() {
    for selected in [false, true] {
        let (_root, paths, old) = fixture();
        let source = uuid::Uuid::new_v4().to_string();
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        record.link_carried_forward(&source, "fixture-previous-build");
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        let dir = paths.home().join("references");
        fs::create_dir(&dir).unwrap();
        let source_reference = dir.join(format!("{source}.json"));
        let current_reference = dir.join(format!("{}.json", record.id));
        let orphan = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        for path in [&source_reference, &current_reference, &orphan] {
            fs::write(path, b"fixture export").unwrap();
        }
        let mut output = Vec::new();
        let inventory = confirm_prune_legacy_options(
            &paths,
            true,
            PruneOptions {
                prune_orphaned_references: selected,
                ..PruneOptions::default()
            },
            false,
            &mut &b""[..],
            &mut output,
        )
        .unwrap();
        prune_confirmed(&paths, &inventory).unwrap();
        assert!(
            source_reference.exists(),
            "a current session's source is not an orphan"
        );
        assert!(current_reference.exists());
        assert_eq!(orphan.exists(), !selected);
        assert!(!old.exists());
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains(&source_reference.display().to_string()));
        assert!(!output.contains(&current_reference.display().to_string()));
    }
}

#[test]
fn prune_confirmation_lists_exactly_its_fenced_state_inventory() {
    let (_root, paths, _old) = fixture();
    let vanished = paths.state_root().join("vanishing");
    fs::write(&vanished, b"fixture state").unwrap();
    SIZE_FAULT.with(|fault| *fault.borrow_mut() = Some((vanished.clone(), SizeFault::Vanish)));
    let mut output = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    let rows = output
        .lines()
        .filter(|line| line.starts_with("Legacy state: "))
        .collect::<Vec<_>>();
    let expected = inventory
        .removals
        .iter()
        .map(|path| format!("Legacy state: {} · 0 bytes", path.display()))
        .collect::<Vec<_>>();
    assert_eq!(rows, expected);
    assert!(inventory.removals.contains(&vanished));
    assert!(
        prune_confirmed(&paths, &inventory)
            .unwrap_err()
            .contains("(removals)")
    );
}

#[test]
fn flat_sqlite_journal_is_a_transient_prune_sidecar() {
    let (_root, paths, _old) = fixture();
    let database = paths.state_root().join("runtime.sqlite3");
    drop(rusqlite::Connection::open(&database).unwrap());
    let journal = database.with_extension("sqlite3-journal");
    fs::write(&journal, b"").unwrap();
    let locked = build_prune_inventory(&paths, PruneOptions::default()).unwrap();
    assert!(locked.sidecars.contains(&journal));
    assert!(!locked.inventory.removals.contains(&journal));
    let LockedPruneInventory {
        inventory,
        databases,
        holders,
        ..
    } = locked;
    drop(databases);
    drop(holders);
    if journal.try_exists().unwrap() {
        fs::remove_file(&journal).unwrap();
    }
    prune_confirmed(&paths, &inventory).unwrap();
    assert!(!database.exists());
}

#[test]
fn prune_accepts_sidecar_cleanup_after_fenced_inventory() {
    for attached in [true, false] {
        let (_root, paths, old) = fixture();
        let database = paths.state_root().join("legacy.sqlite3");
        if attached {
            drop(rusqlite::Connection::open(&database).unwrap());
        }
        let sidecar = database.with_extension("sqlite3-shm");
        fs::write(&sidecar, b"").unwrap();
        REMOVE_FAULT.with(|fault| *fault.borrow_mut() = Some(sidecar.clone()));
        let result = prune_legacy(&paths);
        REMOVE_FAULT.with(|fault| *fault.borrow_mut() = None);
        assert_eq!(result.is_ok(), attached, "sidecar removal: {result:?}");
        assert!(!database.exists());
        assert!(!sidecar.exists());
        assert_eq!(old.exists(), !attached);
    }
}

fn fixture() -> (tempfile::TempDir, PlakPaths, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    paths.ensure_private_state_home().unwrap();
    let old = paths.state_root().join("r1.e1.p1.t1");
    fs::create_dir_all(&old).unwrap();
    (root, paths, old)
}

#[test]
fn prune_inventory_refusals_name_only_the_changed_component() {
    for component in ["removals", "artifacts", "carried"] {
        let (_root, paths, old) = fixture();
        let mut inventory =
            confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
        match component {
            "removals" => inventory.removals.push(paths.state_root().join("changed")),
            "artifacts" => inventory
                .artifacts
                .push(super::super::state_artifacts::Artifacts::default()),
            "carried" => {
                inventory.carried.insert(
                    "fixture source".to_owned(),
                    super::super::state_artifacts::Artifacts::default(),
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            prune_confirmed(&paths, &inventory).unwrap_err(),
            format!(
                "prune inventory changed since confirmation ({component}); nothing removed; run prune-legacy again"
            )
        );
        assert!(old.exists());
    }
}

#[test]
fn standalone_sqlite_sidecars_remain_prune_inventory_targets() {
    for suffix in ["wal", "shm", "journal"] {
        let (_root, paths, old) = fixture();
        let sidecar = paths.state_root().join(format!("absent.sqlite3-{suffix}"));
        fs::write(&sidecar, b"standalone state").unwrap();
        let inventory =
            confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
        assert!(inventory.removals.contains(&sidecar));
        fs::remove_file(&sidecar).unwrap();
        assert!(
            prune_confirmed(&paths, &inventory)
                .unwrap_err()
                .contains("(removals)")
        );
        assert!(old.exists());
    }
}

#[test]
fn unsafe_flat_database_sidecars_refuse_prune_before_sqlite_opens_them() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    for (suffix, linked) in ["wal", "shm", "journal"]
        .into_iter()
        .flat_map(|suffix| [true, false].map(|linked| (suffix, linked)))
    {
        let (root, paths, old) = fixture();
        let database = paths.state_root().join("runtime.sqlite3");
        drop(rusqlite::Connection::open(&database).unwrap());
        let sidecar = database.with_extension(format!("sqlite3-{suffix}"));
        let outside = root.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        if linked {
            symlink(&outside, &sidecar).unwrap();
        } else {
            fs::write(&sidecar, b"unsafe").unwrap();
            fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o666)).unwrap();
        }
        let error = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new())
            .err()
            .unwrap();
        assert!(error.contains("refusing unsafe legacy state"), "{error}");
        assert!(old.exists());
        assert!(database.exists());
        assert_eq!(fs::read(outside).unwrap(), b"keep");
    }
}

#[test]
fn legacy_sizes_and_pruning_remove_flat_files_and_leave_the_current_key_alone() {
    let (_root, paths, old) = fixture();
    fs::write(old.join("saved"), b"12345").unwrap();
    fs::create_dir_all(paths.state_dir()).unwrap();
    fs::write(paths.state_dir().join("current"), b"keep").unwrap();
    fs::write(paths.state_root().join("flat"), b"old flat").unwrap();
    let entries = legacy_state_until(&paths, None).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .any(|entry| entry.path == old && entry.bytes.as_ref().is_ok_and(|bytes| *bytes == 5))
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!old.exists());
    assert!(paths.state_dir().join("current").exists());
    assert!(!paths.state_root().join("flat").exists());
    let mut notice = Vec::new();
    report_other_state_once(&paths, &mut notice, &Once::new());
    assert_eq!(notice, [] as [u8; 0]);
}

#[test]
fn current_product_writer_refuses_pruning_without_removals() {
    let (_root, paths, old) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    let inventory =
        confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    let writer = rusqlite::Connection::open(paths.product_database()).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    assert!(
        prune_confirmed(&paths, &inventory)
            .unwrap_err()
            .contains("in use")
    );
    assert!(old.exists());
    assert!(paths.product_database().exists());
}

#[test]
fn current_runtime_writer_does_not_block_legacy_pruning() {
    let (_root, paths, old) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    let writer = rusqlite::Connection::open(paths.runtime_database()).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value); BEGIN IMMEDIATE; INSERT INTO fixture VALUES(1);").unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
    assert!(!old.exists());
    assert!(paths.runtime_database().exists());
}

#[test]
fn current_idle_wal_connections_do_not_block_legacy_pruning() {
    for committed_write in [false, true] {
        let (_root, paths, old) = fixture();
        let record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        crate::store::sessions::save_session(&paths, &mut record.clone()).unwrap();
        let mut connections = Vec::new();
        for path in [
            paths.runtime_database(),
            paths.product_database(),
            paths.subagents_database(),
        ] {
            let database = rusqlite::Connection::open(path).unwrap();
            database
                .execute_batch(
                    "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS idle_fixture(value);",
                )
                .unwrap();
            if committed_write {
                database
                    .execute_batch("INSERT INTO idle_fixture VALUES(1);")
                    .unwrap();
            }
            connections.push(database);
        }
        connections[2].execute_batch("PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT,launch_checkout TEXT); CREATE TABLE agent_workspaces(tree_id TEXT,path TEXT,mode TEXT); CREATE TABLE agent_candidates(tree_id TEXT,candidate_id TEXT,adopted_by TEXT);").unwrap();
        let inventory =
            confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
        assert_eq!(prune_confirmed(&paths, &inventory).unwrap(), 1);
        assert!(!old.exists());
    }
}

#[test]
fn current_holder_does_not_block_legacy_pruning() {
    let (_root, paths, old) = fixture();
    let locks = paths.state_dir().join("runtime.sqlite3-holders");
    fs::create_dir_all(&locks).unwrap();
    let holder = fs::File::create(locks.join("session.lock")).unwrap();
    rustix::fs::flock(&holder, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let mut report = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report)
        .expect("current holder is not legacy");
    assert_eq!(prune_confirmed(&paths, &inventory).unwrap(), 1);
    assert!(!old.exists());
    assert!(locks.join("session.lock").exists());
}

#[test]
fn holder_and_sqlite_locks_refuse_before_any_directory_is_removed() {
    let (_root, paths, old) = fixture();
    let unlocked = paths.state_root().join("r0.e0.p0.t0");
    fs::create_dir(&unlocked).unwrap();
    let locks = old.join("runtime.sqlite3-holders");
    fs::create_dir(&locks).unwrap();
    let file = fs::File::create(locks.join("branch")).unwrap();
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("holder lock"));
    assert!(unlocked.exists());
    drop(file);
    let connection = rusqlite::Connection::open(old.join("runtime.sqlite3")).unwrap();
    connection
        .execute_batch("CREATE TABLE fixture(value); BEGIN EXCLUSIVE;")
        .unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
    assert!(unlocked.exists());
    drop(connection);
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
}

#[test]
fn flat_files_must_pass_mode_and_database_lock_checks_before_any_removal() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_root, paths, old) = fixture();
    let flat = paths.state_root().join("pre-key.log");
    fs::write(&flat, b"old").unwrap();
    fs::set_permissions(&flat, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("unsafe"));
    assert!(old.exists());
    assert!(flat.exists());
    fs::set_permissions(&flat, fs::Permissions::from_mode(0o600)).unwrap();
    let database = paths.state_root().join("runtime.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE TABLE fixture(value); BEGIN EXCLUSIVE;")
        .unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
    assert!(old.exists());
    assert!(flat.exists());
    drop(connection);
    assert_eq!(prune_legacy(&paths).unwrap(), 3);
}

#[test]
fn an_idle_open_wal_database_is_still_in_use() {
    let (_root, paths, old) = fixture();
    let connection = rusqlite::Connection::open(old.join("runtime.sqlite3")).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value);")
        .unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
    assert!(old.exists());
    drop(connection);
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
}

#[test]
fn symlinks_and_unreadable_databases_are_doubt_not_permission_to_delete() {
    let (root, paths, old) = fixture();
    let outside = root.path().join("outside");
    fs::write(&outside, b"must survive").unwrap();
    std::os::unix::fs::symlink(&outside, old.join("link")).unwrap();
    assert!(prune_legacy(&paths).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"must survive");
    fs::remove_file(old.join("link")).unwrap();
    fs::write(old.join("product.sqlite3"), b"not a readable database").unwrap();
    assert!(prune_legacy(&paths).is_err());
    assert!(old.exists());
    fs::remove_dir_all(paths.state_root()).unwrap();
    let outside_dir = root.path().join("outside-dir");
    fs::create_dir(&outside_dir).unwrap();
    fs::write(outside_dir.join("keep"), b"outside state").unwrap();
    std::os::unix::fs::symlink(&outside_dir, paths.state_root()).unwrap();
    assert!(prune_legacy(&paths).is_err());
    assert!(outside_dir.join("keep").exists());
}

#[test]
fn confirmation_requires_a_terminal_or_yes_and_defaults_to_no() {
    let (_root, paths, _old) = fixture();
    let mut output = Vec::new();
    assert!(confirm_prune_legacy(&paths, false, false, &mut &b"yes\n"[..], &mut output).is_err());
    assert_eq!(output, [] as [u8; 0]);
    assert!(confirm_prune_legacy(&paths, false, true, &mut &b"\n"[..], &mut output).is_err());
    assert!(confirm_prune_legacy(&paths, false, true, &mut &b"yes\n"[..], &mut output).is_ok());
    let text = String::from_utf8(output.clone()).unwrap();
    assert!(text.contains(&paths.state_root().join("r1.e1.p1.t1").display().to_string()));
    assert!(text.contains("Delete 1 listed non-current state entries"));
    assert!(confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut output).is_ok());
}
