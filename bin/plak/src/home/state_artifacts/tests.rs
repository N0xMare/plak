use super::*;
use crate::home::state::{confirm_prune_legacy, prune_legacy};
use plakous_recursive_subagents::tree::vcs_fixtures::git_repo;

thread_local! {
    static INVENTORY_ADDITION: std::cell::RefCell<Option<(PathBuf, Artifacts)>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn before_inventory() {
    INVENTORY_ADDITION.with(|addition| {
        if let Some((path, artifact)) = addition.borrow_mut().take() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_vec(&artifact).unwrap()).unwrap();
        }
    });
}

#[test]
fn partial_prune_refuses_uncovered_tombstone_before_any_removal() {
    let (_home, paths, repo, old, copy) = fixture();
    let unrelated = paths.state_root().join("r9.e9.p9.t9");
    fs::create_dir(&unrelated).unwrap();
    let existing_dir = old.join("deleted-artifacts");
    fs::create_dir(&existing_dir).unwrap();
    let existing = existing_dir.join(format!("{}.json", uuid::Uuid::new_v4()));
    let artifact = Artifacts {
        launch: repo.path().to_path_buf(),
        worktrees: BTreeSet::from([copy.clone()]),
        ..Artifacts::default()
    };
    let bytes = serde_json::to_vec(&artifact).unwrap();
    fs::write(&existing, &bytes).unwrap();
    let tombstone = paths
        .state_root()
        .join("r8.e8.p8.t8/deleted-artifacts")
        .join(format!("{}.json", uuid::Uuid::new_v4()));
    INVENTORY_ADDITION.with(|addition| {
        *addition.borrow_mut() = Some((tombstone.clone(), artifact));
    });
    let error = prune_legacy(&paths).unwrap_err();
    assert!(error.contains("ownership is not covered"), "{error}");
    assert!(error.contains("nothing removed"), "{error}");
    assert!(old.join("subagents.sqlite3").exists());
    assert!(unrelated.exists());
    assert_eq!(fs::read(existing).unwrap(), bytes);
    assert!(tombstone.exists());
    assert!(copy.exists());
    let pin = maintenance::candidate_ref(&maintenance::digest_text("old"), "cand-old-a1");
    for reference in ["refs/heads/plak/old-c1", pin.as_str()] {
        assert!(maintenance::reference_exists(repo.path(), reference).unwrap());
    }
}

fn fixture() -> (
    tempfile::TempDir,
    PlakPaths,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
) {
    let home = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(home.path().join("home"));
    paths.ensure_private_state_home().unwrap();
    let old = paths.state_root().join("r1.e1.p1.t1");
    fs::create_dir_all(&old).unwrap();
    let repo = git_repo();
    // This is a schema-3 artifact fixture, not a current-schema mint:
    // it must not gain today's base and provenance ref families.
    let copy = repo.path().join(".plak/worktrees/old-c1");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::write(
        repo.path().join(".git/info/exclude"),
        ".plak/worktrees/\n.plak/bin/\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "plak/old-c1",
                copy.to_str().unwrap(),
                "HEAD"
            ])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    plakous_recursive_subagents::tree::vcs_fixtures::install_git_copy_fence(&copy);
    let database = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    database.execute_batch("PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT,launch_checkout TEXT); CREATE TABLE agent_workspaces(tree_id TEXT,path TEXT,mode TEXT); CREATE TABLE agent_candidates(tree_id TEXT,candidate_id TEXT,adopted_by TEXT);").unwrap();
    database
        .execute(
            "INSERT INTO agent_trees VALUES('old',?1)",
            [repo.path().display().to_string()],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO agent_workspaces VALUES('old',?1,'fork')",
            [copy.display().to_string()],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO agent_candidates VALUES('old','cand-old-a1','root')",
            [],
        )
        .unwrap();
    // Model landed legacy work: both branch and pin are reachable from
    // launch HEAD, so maintenance can prove collection is safe.
    let pin = maintenance::candidate_ref(&maintenance::digest_text("old"), "cand-old-a1");
    for reference in ["refs/heads/plak/old-c1", pin.as_str()] {
        assert!(
            std::process::Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["update-ref", reference, "HEAD"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    }
    (home, paths, repo, old, copy)
}

#[test]
fn real_home_flat_wal_inventory_prunes_after_confirmation() {
    let (_home, paths, repo, old, copy) = fixture();
    let flat = paths.state_root().join("subagents.sqlite3");
    fs::rename(old.join("subagents.sqlite3"), &flat).unwrap();
    let database = rusqlite::Connection::open(&flat).unwrap();
    database.execute_batch("PRAGMA user_version=2;").unwrap();
    drop(database);
    assert!(
        std::process::Command::new("git")
            .args(["update-ref", "refs/plak/candidates/cand-old-a1", "HEAD"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    for name in ["product", "runtime", "subagents"] {
        let path = paths.state_root().join(format!("{name}.sqlite3"));
        let database = rusqlite::Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE wal_fixture(value); INSERT INTO wal_fixture VALUES(1);").unwrap();
        // Model a stopped process's flat WAL files without a live holder.
        let files = [
            path.clone(),
            path.with_extension("sqlite3-wal"),
            path.with_extension("sqlite3-shm"),
        ];
        let snapshots = files
            .iter()
            .map(|file| fs::read(file).unwrap())
            .collect::<Vec<_>>();
        drop(database);
        for (file, bytes) in files.iter().zip(snapshots) {
            fs::write(file, bytes).unwrap();
        }
        let holders = paths.state_root().join(format!("{name}.sqlite3-holders"));
        fs::create_dir(&holders).unwrap();
        fs::write(holders.join("unused.lock"), b"").unwrap();
    }
    for name in [
        "r2.e2.p2.t2",
        "r3.e3.p3.t2",
        "r4.e4.p4.t2",
        "r21.e19.p27.t2",
        "sessions",
        "retired",
    ] {
        fs::create_dir(paths.state_root().join(name)).unwrap();
    }
    let references = paths.home().join("references");
    fs::create_dir(&references).unwrap();
    for _ in 0..2 {
        fs::write(
            references.join(format!("{}.json", uuid::Uuid::new_v4())),
            b"fixture export",
        )
        .unwrap();
    }
    let sidecars = ["product", "runtime", "subagents"]
        .into_iter()
        .flat_map(|name| {
            ["wal", "shm"].map(|suffix| paths.state_root().join(format!("{name}.sqlite3-{suffix}")))
        })
        .collect::<Vec<_>>();
    assert!(sidecars.iter().all(|path| path.exists()));
    let inventory =
        confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    assert!(sidecars.iter().all(|path| path.exists()));
    let count =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut Vec::new())
            .unwrap();
    assert_eq!(count.artifact_groups, 1);
    assert!(!old.exists());
    assert!(!flat.exists());
    assert!(!copy.exists());
    assert_eq!(fs::read_dir(references).unwrap().count(), 2);
    for name in ["product", "runtime", "subagents"] {
        for suffix in ["wal", "shm", "journal"] {
            assert!(
                !paths
                    .state_root()
                    .join(format!("{name}.sqlite3-{suffix}"))
                    .exists()
            );
        }
    }
}

#[test]
fn partial_prune_keeps_only_the_owning_key_and_explains_each_kept_entry() {
    for unadopted in [false, true] {
        let (_home, paths, _repo, old, copy) = fixture();
        if unadopted {
            let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
            db.execute("UPDATE agent_candidates SET adopted_by=NULL", [])
                .unwrap();
        } else {
            fs::write(copy.join("dirty"), b"keep").unwrap();
        }
        let unrelated = paths.state_root().join("r9.e9.p9.t9");
        fs::create_dir(&unrelated).unwrap();
        let flat = paths.state_root().join("runtime.sqlite3");
        rusqlite::Connection::open(&flat).unwrap();
        let references = paths.home().join("references");
        fs::create_dir(&references).unwrap();
        let reference = references.join(format!("{}.json", uuid::Uuid::new_v4()));
        fs::write(&reference, b"fixture export").unwrap();
        let inventory =
            confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
        let mut report = Vec::new();
        let counts =
            super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut report)
                .unwrap();
        assert_eq!(counts.state_entries, 2);
        assert!(old.exists());
        assert!(copy.exists());
        assert!(reference.exists());
        assert!(!unrelated.exists());
        assert!(!flat.exists());
        let report = String::from_utf8(report).unwrap();
        assert!(report.contains("kept:"), "{report}");
        assert!(report.contains("owns tree"), "{report}");
        if unadopted {
            assert!(report.contains("1 unadopted refs"), "{report}");
        } else {
            assert_eq!(
                report
                    .lines()
                    .find(|line| line.starts_with("kept:"))
                    .unwrap(),
                format!(
                    "kept: {} · owns tree cba06b57 with 0 unadopted worktrees / 0 unadopted refs / 1 dirty worktrees; retained artifacts",
                    old.display()
                )
            );
        }
    }
}

#[test]
fn partial_prune_retains_owners_through_dotdot_home() {
    let (_home, original, _repo, old, copy) = fixture();
    fs::create_dir(original.home().join("alias")).unwrap();
    let paths = PlakPaths::explicit(original.home().join("alias/.."));
    assert_aliased_owner_retained(&paths, &old, &copy);
}

#[test]
fn partial_prune_fails_closed_for_any_uncovered_owner() {
    let current = PathBuf::from("home/state/current");
    let kept = PathBuf::from("home/state/kept");
    let mut artifact = Artifacts {
        owners: BTreeMap::from([
            (current.join("subagents.sqlite3"), BTreeSet::new()),
            (kept.join("subagents.sqlite3"), BTreeSet::new()),
        ]),
        ..Artifacts::default()
    };
    let entries = vec![kept];
    assert!(super::super::state::ensure_owners_covered(&[&artifact], &entries, &current).is_ok());
    artifact
        .owners
        .insert(PathBuf::from("outside/subagents.sqlite3"), BTreeSet::new());
    assert_eq!(
        super::super::state::ensure_owners_covered(&[&artifact], &entries, &current).unwrap_err(),
        "retained artifact ownership is not covered by kept state; nothing removed"
    );
    assert!(
        super::super::state::ensure_owners_covered(&[&Artifacts::default()], &entries, &current)
            .is_err()
    );
}

#[test]
fn partial_prune_retains_owners_through_symlinked_ancestor() {
    let (base, original, _repo, _old, copy) = fixture();
    fs::create_dir(base.path().join("real")).unwrap();
    fs::rename(original.home(), base.path().join("real/home")).unwrap();
    std::os::unix::fs::symlink("real", base.path().join("link")).unwrap();
    let paths = PlakPaths::explicit(base.path().join("link/home"));
    fs::write(copy.join("dirty"), b"keep").unwrap();
    let owning_entry = paths.state_root().join("r1.e1.p1.t1");
    let database_path = owning_entry.join("subagents.sqlite3");
    let database = rusqlite::Connection::open(&database_path).unwrap();
    let (artifacts, _) = inventory(&paths, &[(database_path, database)], &BTreeSet::new()).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].check_clean().is_err());
    assert!(artifacts[0].owns_entry(&owning_entry));
    assert!(artifacts[0].kept_reason().contains("1 dirty worktrees"));
    // Arbitrary symlink ancestors remain refused at the admission boundary;
    // maintenance ownership still uses its enumeration identity.
    assert!(prune_legacy(&paths).unwrap_err().contains("ancestor"));
    assert!(owning_entry.join("subagents.sqlite3").exists());
    assert!(copy.exists());
}

fn assert_aliased_owner_retained(paths: &PlakPaths, old: &Path, copy: &Path) {
    fs::write(copy.join("dirty"), b"keep").unwrap();
    let unrelated = paths.state_root().join("r9.e9.p9.t9");
    fs::create_dir(&unrelated).unwrap();
    let inventory =
        confirm_prune_legacy(paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    let mut report = Vec::new();
    let counts =
        super::super::state::prune_confirmed_with_report(paths, &inventory, &mut report).unwrap();
    assert_eq!(counts.kept_entries, 1);
    assert_eq!(counts.kept_groups, 1);
    assert!(old.join("subagents.sqlite3").exists());
    assert!(copy.exists());
    assert!(!unrelated.exists());
    let report = String::from_utf8(report).unwrap();
    assert!(
        report.contains(&format!(
            "kept: {}",
            paths.state_root().join("r1.e1.p1.t1").display()
        )),
        "{report}"
    );
}

#[test]
fn doctor_unowned_sizes_share_an_expired_deadline() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let legacy = paths.state_root().join("r1.e1.p1.t1");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("work"), b"keep").unwrap();
    let unowned = paths.home().join("worktrees/unowned");
    fs::create_dir_all(&unowned).unwrap();
    fs::write(unowned.join("work"), b"keep").unwrap();
    let mut report = Vec::new();
    let warnings = super::super::state::report_legacy_state_until(
        &paths,
        &mut report,
        Some(std::time::Instant::now()),
    )
    .unwrap();
    let report = String::from_utf8(report).unwrap();
    assert_eq!(warnings, 0, "{report}");
    assert_eq!(
        report
            .matches("size unavailable: size inspection budget exhausted")
            .count(),
        2,
        "{report}"
    );
    assert!(!report.contains("WARNING"), "{report}");
    assert!(
        report
            .lines()
            .any(|line| line.contains("unowned worktree:") && line.contains("size unavailable")),
        "{report}"
    );
    assert!(unowned.exists());
}

#[test]
fn doctor_warns_once_when_unowned_inventory_is_unreadable() {
    for (database, tombstone) in [(true, false), (false, true), (true, true)] {
        let (_home, paths, _repo, old, _copy) = fixture();
        if database {
            fs::write(old.join("subagents.sqlite3"), b"unreadable").unwrap();
            fs::write(paths.state_root().join("subagents.sqlite3"), b"unreadable").unwrap();
        }
        if tombstone {
            let dir = old.join("deleted-artifacts");
            fs::create_dir(&dir).unwrap();
            fs::write(
                dir.join(format!("{}.json", uuid::Uuid::new_v4())),
                b"not json",
            )
            .unwrap();
        }
        let mut report = Vec::new();
        let warnings =
            super::super::state::report_legacy_state_until(&paths, &mut report, None).unwrap();
        let report = String::from_utf8(report).unwrap();
        assert_eq!(
            report
                .matches("unowned list may be incomplete or overstated")
                .count(),
            1,
            "{report}"
        );
        assert_eq!(warnings, 1);
    }
}

#[test]
fn partial_prune_retains_only_the_flat_tree_database_and_its_holders() {
    let (_home, paths, _repo, old, copy) = fixture();
    fs::rename(
        old.join("subagents.sqlite3"),
        paths.state_root().join("subagents.sqlite3"),
    )
    .unwrap();
    for name in ["product", "runtime", "subagents"] {
        let path = paths.state_root().join(format!("{name}.sqlite3"));
        let database = rusqlite::Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE wal_fixture(value); INSERT INTO wal_fixture VALUES(1);").unwrap();
        let files = [
            path.clone(),
            path.with_extension("sqlite3-wal"),
            path.with_extension("sqlite3-shm"),
        ];
        let snapshots = files
            .iter()
            .map(|file| fs::read(file).unwrap())
            .collect::<Vec<_>>();
        drop(database);
        for (file, bytes) in files.iter().zip(snapshots) {
            fs::write(file, bytes).unwrap();
        }
    }
    for name in ["product", "runtime", "subagents"] {
        let holders = paths.state_root().join(format!("{name}.sqlite3-holders"));
        fs::create_dir(&holders).unwrap();
        fs::write(holders.join("unused.lock"), b"").unwrap();
    }
    fs::write(copy.join("dirty"), b"keep").unwrap();
    let inventory =
        confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    let mut report = Vec::new();
    let counts =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut report).unwrap();
    assert_eq!(
        counts.state_entries,
        9,
        "{}",
        String::from_utf8_lossy(&report)
    );
    assert_eq!(counts.kept_entries, 4);
    assert!(!old.exists());
    for name in ["product", "runtime", "subagents"] {
        for suffix in ["wal", "shm"] {
            assert_eq!(
                paths
                    .state_root()
                    .join(format!("{name}.sqlite3-{suffix}"))
                    .exists(),
                name == "subagents"
            );
        }
        assert_eq!(
            paths.state_root().join(format!("{name}.sqlite3")).exists(),
            name == "subagents"
        );
        assert_eq!(
            paths
                .state_root()
                .join(format!("{name}.sqlite3-holders"))
                .exists(),
            name == "subagents"
        );
    }
    assert_eq!(
        String::from_utf8(report).unwrap().matches("kept:").count(),
        4
    );
}

#[test]
fn partial_prune_retains_every_key_that_owns_a_shared_group() {
    let (_home, paths, _repo, old, copy) = fixture();
    let shared = paths.state_root().join("r2.e2.p2.t2");
    fs::create_dir(&shared).unwrap();
    fs::copy(
        old.join("subagents.sqlite3"),
        shared.join("subagents.sqlite3"),
    )
    .unwrap();
    fs::write(copy.join("dirty"), b"keep").unwrap();
    let unrelated = paths.state_root().join("unrelated");
    fs::create_dir(&unrelated).unwrap();
    let mut report = Vec::new();
    let inventory =
        confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    let counts =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut report).unwrap();
    assert_eq!(counts.state_entries, 1);
    assert!(old.exists());
    assert!(shared.exists());
    assert!(!unrelated.exists());
    assert_eq!(
        String::from_utf8(report).unwrap().matches("kept:").count(),
        2
    );
}

#[test]
fn doctor_reports_unowned_project_worktrees_even_without_state() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = tempfile::tempdir().unwrap();
    let paths = paths.with_project_root(repo.path().to_path_buf());
    let unowned = repo.path().join(".plak/worktrees/unowned");
    fs::create_dir_all(&unowned).unwrap();
    fs::write(unowned.join("work"), b"keep").unwrap();
    let mut report = Vec::new();
    super::super::state::report_legacy_state_until(&paths, &mut report, None).unwrap();
    let report = String::from_utf8(report).unwrap();
    assert!(report.contains("unowned worktree"), "{report}");
    assert!(report.contains("4 bytes"), "{report}");
    assert!(unowned.exists());
}

#[test]
fn partial_prune_cleans_independent_tombstones_but_keeps_retained_ownership() {
    let (_home, paths, repo, old, copy) = fixture();
    let (_other_home, _other_paths, dirty_repo, _other_old, dirty_copy) = fixture();
    fs::write(dirty_copy.join("dirty"), b"keep").unwrap();
    let kept_id = uuid::Uuid::new_v4().to_string();
    remember_artifacts(
        &paths,
        &kept_id,
        Artifacts {
            launch: dirty_repo.path().to_path_buf(),
            worktrees: BTreeSet::from([dirty_copy.clone()]),
            ..Artifacts::default()
        },
    )
    .unwrap();
    let clean_id = uuid::Uuid::new_v4().to_string();
    remember_artifacts(
        &paths,
        &clean_id,
        Artifacts {
            launch: repo.path().to_path_buf(),
            worktrees: BTreeSet::from([copy.clone()]),
            ..Artifacts::default()
        },
    )
    .unwrap();
    let mut report = Vec::new();
    super::super::state::report_legacy_state_until(&paths, &mut report, None).unwrap();
    assert!(
        !String::from_utf8(report)
            .unwrap()
            .contains("unowned worktree")
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!old.exists());
    assert!(!copy.exists());
    assert!(dirty_copy.exists());
    let dir = paths.state_dir().join("deleted-artifacts");
    assert!(dir.join(format!("{kept_id}.json")).exists());
    assert!(!dir.join(format!("{clean_id}.json")).exists());
}

#[test]
fn doctor_reports_unowned_worktrees_with_size_without_deleting_them() {
    let (_home, paths, repo, _old, copy) = fixture();
    let unowned = repo.path().join(".plak/worktrees/unowned");
    fs::create_dir_all(&unowned).unwrap();
    fs::write(unowned.join("work"), b"keep").unwrap();
    let mut report = Vec::new();
    assert_eq!(
        super::super::state::report_legacy_state_until(&paths, &mut report, None).unwrap(),
        0
    );
    let report = String::from_utf8(report).unwrap();
    assert!(report.contains("unowned worktree"), "{report}");
    assert!(report.contains("4 bytes"), "{report}");
    assert!(report.contains(&unowned.display().to_string()), "{report}");
    assert!(!report.lines().any(
        |line| line.contains("unowned worktree") && line.contains(&copy.display().to_string())
    ));
    prune_legacy(&paths).unwrap();
    assert!(unowned.exists());
}

#[test]
fn unmatched_existing_product_database_fails_closed_without_tree_database() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    paths.ensure_private_state_home().unwrap();
    assert!(sessions(&[], &paths).unwrap().is_empty());
    fs::create_dir_all(paths.state_dir()).unwrap();
    fs::write(paths.product_database(), b"exists").unwrap();
    assert!(sessions(&[], &paths).is_err());
    fs::remove_file(paths.product_database()).unwrap();
    std::os::unix::fs::symlink("absent", paths.product_database()).unwrap();
    assert!(sessions(&[], &paths).is_err());
}

#[test]
fn missing_launch_parent_is_retained_even_with_forget_flag() {
    let (_home, paths, repo, old, copy) = fixture();
    let launch = repo.path().join("unmounted/repository");
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    db.execute(
        "UPDATE agent_trees SET launch_checkout=?1",
        [launch.to_str().unwrap()],
    )
    .unwrap();
    drop(db);
    let mut report = Vec::new();
    let inventory = super::super::state::confirm_prune_legacy_options(
        &paths,
        true,
        super::super::state::PruneOptions {
            forget_missing_launches: true,
            prune_orphaned_references: false,
        },
        false,
        &mut &b""[..],
        &mut report,
    )
    .unwrap();
    assert!(
        String::from_utf8(report)
            .unwrap()
            .contains("launch ancestor missing (unmounted volume?)")
    );
    let count =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut Vec::new())
            .unwrap();
    assert_eq!(count.forgotten_groups, 0);
    assert!(old.exists());
    assert!(copy.exists());
}

#[test]
fn launch_probe_errors_never_allow_forgetting() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
    let cases = [
        (blocked.join("launch"), 13),
        (
            root.path().join("loop/launch"),
            rustix::io::Errno::LOOP.raw_os_error(),
        ),
        (root.path().join("file/launch"), 20),
    ];
    symlink("loop", root.path().join("loop")).unwrap();
    fs::write(root.path().join("file"), b"not a directory").unwrap();
    for (launch, errno) in cases {
        // The permission assertion requires an unprivileged test process.
        if errno == 13 && rustix::process::geteuid().is_root() {
            continue;
        }
        assert_eq!(
            fs::metadata(&launch).unwrap_err().raw_os_error(),
            Some(errno)
        );
        let artifact = Artifacts {
            launch: launch.clone(),
            ..Artifacts::default()
        };
        assert!(!artifact.launch_missing());
        assert!(artifact.check_clean().is_err());
        let (_home, paths, _repo, old, copy) = fixture();
        let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
        db.execute(
            "UPDATE agent_trees SET launch_checkout=?1",
            [launch.to_str().unwrap()],
        )
        .unwrap();
        drop(db);
        let mut report = Vec::new();
        let inventory = super::super::state::confirm_prune_legacy_options(
            &paths,
            true,
            super::super::state::PruneOptions {
                forget_missing_launches: true,
                prune_orphaned_references: false,
            },
            false,
            &mut &b""[..],
            &mut report,
        )
        .unwrap();
        assert!(
            String::from_utf8(report)
                .unwrap()
                .contains("launch ancestor missing (unmounted volume?)")
        );
        let counts =
            super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut Vec::new())
                .unwrap();
        assert_eq!(counts.forgotten_groups, 0);
        assert_eq!(counts.artifact_groups, 0);
        assert_eq!(counts.state_entries, 0);
        assert!(old.exists());
        assert!(copy.exists());
    }
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn forgetting_is_not_reported_or_counted_when_another_group_retains_inventory() {
    let (_home, paths, repo, old, copy) = fixture();
    fs::create_dir(paths.home().join("alias")).unwrap();
    let paths = PlakPaths::explicit(paths.home().join("alias/.."));
    fs::write(copy.join("dirty"), b"keep").unwrap();
    let absent = repo.path().join("absent-launch");
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO agent_trees VALUES('missing',?1)",
        [absent.to_str().unwrap()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO agent_workspaces VALUES('missing',?1,'fork')",
        [absent.join(".plak/worktrees/missing").to_str().unwrap()],
    )
    .unwrap();
    drop(db);
    let inventory = super::super::state::confirm_prune_legacy_options(
        &paths,
        true,
        super::super::state::PruneOptions {
            forget_missing_launches: true,
            prune_orphaned_references: false,
        },
        false,
        &mut &b""[..],
        &mut Vec::new(),
    )
    .unwrap();
    let mut report = Vec::new();
    let counts =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut report).unwrap();
    assert_eq!(counts.forgotten_groups, 0);
    assert!(
        !String::from_utf8(report)
            .unwrap()
            .contains("Forgotten ownership")
    );
    assert!(old.exists());
    assert!(copy.exists());
}

#[test]
fn uncertain_protected_launch_only_taints_possible_aliases_and_names_owner() {
    let (_home, paths, repo, old, copy) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(repo.path().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    let protected = repo.path().join("absent");
    remember_artifacts(
        &paths,
        &record.id,
        Artifacts {
            launch: protected.clone(),
            worktrees: BTreeSet::from([protected.join(".plak/worktrees/owned")]),
            ..Artifacts::default()
        },
    )
    .unwrap();
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO agent_trees VALUES('alias',?1)",
        [protected.to_str().unwrap()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO agent_workspaces VALUES('alias',?1,'fork')",
        [protected.join(".plak/worktrees/alias").to_str().unwrap()],
    )
    .unwrap();
    drop(db);
    let mut report = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report).unwrap();
    let report = String::from_utf8(report).unwrap();
    assert!(report.contains(&format!("protected launch {}", protected.display())));
    let counts =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut Vec::new())
            .unwrap();
    assert_eq!(counts.artifact_groups, 1);
    assert!(!copy.exists());
    assert!(old.exists());
}

#[test]
fn deleted_worktrees_directory_is_absent_not_unresolvable() {
    let (_home, paths, repo, old, copy) = fixture();
    fs::remove_dir_all(copy.parent().unwrap()).unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!old.exists());
    assert!(!maintenance::reference_exists(repo.path(), "refs/heads/plak/old-c1").unwrap());
}

#[test]
fn dangling_ancestor_is_not_treated_as_a_missing_suffix() {
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("absent", root.path().join("alias")).unwrap();
    assert!(canonicalize_missing(&root.path().join("alias/copy")).is_err());
}

#[test]
fn ignored_env_file_is_reported_and_retained() {
    let (_home, paths, repo, old, copy) = fixture();
    let exclude = repo.path().join(".git/info/exclude");
    let mut rules = fs::read_to_string(&exclude).unwrap();
    rules.push_str(".env\n");
    fs::write(exclude, rules).unwrap();
    assert!(copy.join(".plak/bin/git").exists());
    fs::write(copy.join(".env"), "OFFLINE_FIXTURE=keep\n").unwrap();
    let mut report = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report).unwrap();
    let report = String::from_utf8(report).unwrap();
    assert!(report.contains(copy.to_str().unwrap()));
    assert!(report.contains(".env"));
    assert!(report.to_ascii_lowercase().contains("ignored files"));
    let count =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut Vec::new())
            .unwrap();
    assert_eq!(count.artifact_groups, 0);
    assert!(copy.join(".env").exists());
    assert!(old.exists());
}

#[test]
fn default_minted_copy_with_only_its_fence_is_prunable() {
    let (_home, paths, _repo, old, copy) = fixture();
    assert!(copy.join(".plak/bin/git").exists());
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!copy.exists());
    assert!(!old.exists());
}

#[test]
fn tampered_ignored_fence_is_retained() {
    let (_home, paths, _repo, old, copy) = fixture();
    let fence = copy.join(".plak/bin/git");
    let mut body = fs::read(&fence).unwrap();
    body[0] = b'X';
    fs::write(&fence, body).unwrap();
    let mut report = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report).unwrap();
    assert!(String::from_utf8(report).unwrap().contains("ignored files"));
    let count =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut Vec::new())
            .unwrap();
    assert_eq!(count.artifact_groups, 0);
    assert!(fence.exists());
    assert!(old.exists());
}

#[test]
fn unexpected_ignored_content_under_plak_is_retained() {
    use std::os::unix::fs::symlink;
    for linked in [false, true] {
        let (_home, paths, _repo, old, copy) = fixture();
        let file = copy.join(".plak/bin/user-file");
        if linked {
            symlink("git", &file).unwrap();
        } else {
            fs::write(&file, b"user content").unwrap();
        }
        assert_eq!(prune_legacy(&paths).unwrap(), 0);
        assert!(fs::symlink_metadata(&file).is_ok());
        assert!(old.exists());
    }
}

#[test]
fn linked_ignored_fence_is_retained_even_with_matching_content() {
    let (_home, paths, _repo, old, copy) = fixture();
    let fence = copy.join(".plak/bin/git");
    let outside = paths.home().join("fence-content");
    fs::copy(&fence, &outside).unwrap();
    fs::remove_file(&fence).unwrap();
    std::os::unix::fs::symlink(&outside, &fence).unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(
        fs::symlink_metadata(&fence)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(old.exists());
}

#[test]
fn tombstone_writer_requires_canonical_lowercase_hyphenated_uuid() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let id = uuid::Uuid::parse_str("5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11").unwrap();
    for invalid in [
        id.simple().to_string(),
        id.to_string().to_uppercase(),
        format!("{{{id}}}"),
    ] {
        assert!(remember_artifacts(&paths, &invalid, Artifacts::default()).is_err());
    }
    assert!(!paths.state_dir().join("deleted-artifacts").exists());
    remember_artifacts(&paths, &id.to_string(), Artifacts::default()).unwrap();
}

#[test]
fn missing_launch_forgetting_is_explicit_and_counts_ownership_separately() {
    let (_home, paths, repo, old, _copy) = fixture();
    fs::remove_dir_all(repo.path()).unwrap();
    let mut report = Vec::new();
    let inventory = super::super::state::confirm_prune_legacy_options(
        &paths,
        true,
        super::super::state::PruneOptions {
            forget_missing_launches: true,
            prune_orphaned_references: false,
        },
        false,
        &mut &b""[..],
        &mut report,
    )
    .unwrap();
    let report = String::from_utf8(report).unwrap();
    assert!(report.contains("launch missing"));
    assert!(report.contains("ownership will be forgotten"));
    assert!(!report.contains("use --forget-missing-launches"));
    let mut report = Vec::new();
    let counts =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut report).unwrap();
    assert_eq!(counts.state_entries, 1);
    assert_eq!(counts.artifact_groups, 0);
    assert_eq!(counts.forgotten_groups, 1);
    assert!(!old.exists());
    assert!(
        String::from_utf8(report)
            .unwrap()
            .contains("Forgotten ownership")
    );
}

#[test]
fn removal_reports_each_retention_reason_without_mixing_counts() {
    let (_home, paths, _repo, old, copy) = fixture();
    fs::write(copy.join("dirty"), "uncommitted").unwrap();
    let inventory =
        confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    let mut report = Vec::new();
    let count =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut report).unwrap();
    let report = String::from_utf8(report).unwrap();
    assert!(report.contains("Retained artifact group:"));
    assert!(report.contains("uncommitted changes"));
    assert_eq!(count.state_entries, 0);
    assert_eq!(count.artifact_groups, 0);
    assert!(old.exists());
}

#[test]
fn initialized_submodule_refusal_retains_worktree_and_refs() {
    let (_home, paths, repo, old, copy) = fixture();
    let submodule = git_repo();
    let git = |cwd: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(
        &copy,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            submodule.path().to_str().unwrap(),
            "vendor",
        ],
    );
    git(&copy, &["commit", "-am", "submodule"]);
    git(repo.path(), &["merge", "--ff-only", "plak/old-c1"]);
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(copy.exists());
    assert!(old.exists());
    assert!(maintenance::reference_exists(repo.path(), "refs/heads/plak/old-c1").unwrap());
}

#[test]
fn confirmation_releases_database_transactions_before_reading_answer() {
    struct Prompt {
        holder: PathBuf,
        observed: bool,
    }
    impl std::io::Write for Prompt {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes
                .windows(b"Continue?".len())
                .any(|part| part == b"Continue?")
            {
                let holder = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.holder)
                    .unwrap();
                rustix::fs::flock(
                    &holder,
                    rustix::fs::FlockOperation::NonBlockingLockExclusive,
                )
                .unwrap();
                self.observed = true;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Answer {
        database: PathBuf,
        holder: PathBuf,
        input: &'static [u8],
    }
    impl std::io::Read for Answer {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let bytes = std::io::BufRead::fill_buf(self)?;
            let count = buffer.len().min(bytes.len());
            buffer[..count].copy_from_slice(&bytes[..count]);
            std::io::BufRead::consume(self, count);
            Ok(count)
        }
    }
    impl std::io::BufRead for Answer {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let database = rusqlite::Connection::open(&self.database).unwrap();
            database.busy_timeout(std::time::Duration::ZERO).unwrap();
            database
                .execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
                .unwrap();
            let holder = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.holder)
                .unwrap();
            rustix::fs::flock(
                &holder,
                rustix::fs::FlockOperation::NonBlockingLockExclusive,
            )
            .unwrap();
            Ok(self.input)
        }
        fn consume(&mut self, count: usize) {
            self.input = &self.input[count..];
        }
    }
    let (_home, paths, _repo, old, _copy) = fixture();
    let holder = old.join("runtime.sqlite3-holders/branch");
    fs::create_dir_all(holder.parent().unwrap()).unwrap();
    fs::write(&holder, b"").unwrap();
    let mut answer = Answer {
        holder: holder.clone(),
        database: old.join("subagents.sqlite3"),
        input: b"yes\n",
    };
    let mut prompt = Prompt {
        holder,
        observed: false,
    };
    confirm_prune_legacy(&paths, false, true, &mut answer, &mut prompt).unwrap();
    assert!(
        prompt.observed,
        "holder lock must be released before writing the prompt"
    );
}

#[test]
fn locked_worktree_removal_refusal_retains_inventory_and_refs() {
    let (_home, paths, repo, old, copy) = fixture();
    assert!(
        std::process::Command::new("git")
            .args(["worktree", "lock"])
            .arg(&copy)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(copy.exists());
    assert!(old.exists());
    assert!(maintenance::reference_exists(repo.path(), "refs/heads/plak/old-c1").unwrap());
}

#[test]
fn exact_session_membership_does_not_protect_a_substring_tree_id() {
    let (_home, paths, _repo, old, copy) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    for table in ["agent_trees", "agent_workspaces", "agent_candidates"] {
        db.execute(&format!("UPDATE {table} SET tree_id=?1"), [&record.id[..8]])
            .unwrap();
    }
    drop(db);
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!copy.exists());
}

#[test]
fn canonical_launch_aliases_protect_overlapping_artifacts() {
    let (_home, paths, repo, old, copy) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    fs::create_dir(repo.path().join("nested")).unwrap();
    let db = rusqlite::Connection::open(paths.subagents_database()).unwrap();
    db.execute_batch("PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT,launch_checkout TEXT); CREATE TABLE agent_workspaces(tree_id TEXT,path TEXT,mode TEXT); CREATE TABLE agent_candidates(tree_id TEXT,candidate_id TEXT,adopted_by TEXT);").unwrap();
    db.execute(
        "INSERT INTO agent_trees VALUES(?1,?2)",
        rusqlite::params![
            record.id,
            repo.path().join("nested/..").display().to_string()
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO agent_workspaces VALUES(?1,?2,'fork')",
        rusqlite::params![record.id, copy.join(".").display().to_string()],
    )
    .unwrap();
    drop(db);
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(copy.exists());
    assert!(!old.exists());
}

#[test]
fn maintenance_names_legacy_and_short_branches_by_schema() {
    for version in [8, 9] {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE agent_trees(tree_id TEXT, launch_checkout TEXT);
                 CREATE TABLE agent_workspaces(tree_id TEXT, workspace_id TEXT, path TEXT, mode TEXT, status TEXT, input_candidate_id TEXT, resolves TEXT);
                 CREATE TABLE agent_nodes(tree_id TEXT, agent_id TEXT, workspace_id TEXT, branch TEXT);
                 CREATE TABLE agent_candidates(tree_id TEXT, candidate_id TEXT, adopted_by TEXT, adopted_via TEXT, kind TEXT);
                 CREATE TABLE agent_operations(tree_id TEXT, tool_name TEXT, canonical_request TEXT);
                 INSERT INTO agent_trees VALUES('tree','/checkout');
                 INSERT INTO agent_workspaces VALUES('tree','ws-c1','/checkout/.plak/worktrees/1234abcd-c1','fork','active',NULL,NULL);
                 INSERT INTO agent_nodes VALUES('tree','c1','ws-c1','plak/1234abcd/c1');"
        ).unwrap();
        db.execute(
            "INSERT INTO agent_operations VALUES('tree','rescue_pin',?1)",
            ["[\"failed-c1-a1-turn\"]"],
        )
        .unwrap();
        db.pragma_update(None, "user_version", version).unwrap();
        if version == 9 {
            db.execute_batch(
                "INSERT INTO agent_candidates VALUES('tree','superseded',NULL,'accepted','conflict');
                                  INSERT INTO agent_candidates VALUES('tree','pending',NULL,NULL,'work');",
            )
            .unwrap();
        }
        let inventory = tree_artifacts(&db).unwrap();
        if version == 9 {
            assert_eq!(
                inventory[0].1.unadopted,
                BTreeSet::from([maintenance::candidate_ref(
                    &maintenance::digest_text("tree"),
                    "pending"
                )])
            );
        }
        if version == 9 {
            db.execute_batch("UPDATE agent_workspaces SET input_candidate_id='superseded';")
                .unwrap();
            let held = tree_artifacts(&db).unwrap();
            assert!(held[0].1.unadopted.contains(&maintenance::candidate_ref(
                &maintenance::digest_text("tree"),
                "superseded"
            )));
            db.execute_batch("UPDATE agent_workspaces SET status='clean_removed';")
                .unwrap();
            let removed = tree_artifacts(&db).unwrap();
            assert!(
                !removed[0].1.unadopted.contains(&maintenance::candidate_ref(
                    &maintenance::digest_text("tree"),
                    "superseded"
                ))
            );
        }
        let refs = &inventory[0].1.references;
        for reference in
            maintenance::candidate_refs(&maintenance::digest_text("tree"), "failed-c1-a1-turn")
        {
            assert_eq!(refs.contains(&reference), version == 9);
        }
        assert_eq!(refs.contains("refs/heads/plak/1234abcd-c1"), version == 8);
        assert_eq!(refs.contains("refs/heads/plak/1234abcd/c1"), version == 9);
        assert_eq!(
            refs.contains(&maintenance::copy_base_ref(
                &maintenance::digest_text("tree"),
                "c1"
            )),
            version == 9
        );
    }
}

#[test]
fn current_schema_tree_allows_prune_and_doctor_inventory() {
    let (_home, paths, repo, old, copy) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(repo.path().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    drop(
        plakous_recursive_subagents_store::SqliteTreeStore::open(&paths.subagents_database())
            .unwrap(),
    );
    let db = rusqlite::Connection::open(paths.subagents_database()).unwrap();
    db.execute(
        "INSERT INTO agent_trees VALUES(?1,'root',1,1,'live','single_lease','tuple',?2,'{}')",
        rusqlite::params![record.id, repo.path().display().to_string()],
    )
    .unwrap();
    db.execute("INSERT INTO agent_workspaces(tree_id,workspace_id,owner_agent_id,path,mode,status,generation,conflict_paths) VALUES(?1,'workspace','c1',?2,'fork','active',0,'[]')",
        rusqlite::params![record.id, copy.display().to_string()]).unwrap();
    // Current inventory owns this shared legacy artifact by its recorded
    // branch, not by applying legacy naming to the copy directory.
    db.execute("INSERT INTO agent_nodes(tree_id,agent_id,parent_agent_id,depth,description,prompt,output_schema_json,schema_digest,tuple_id,account,status,workspace_id,tier,path,branch,active_turn) VALUES(?1,'c1','root',1,'worker','task','{}','digest','tuple','account','running','workspace','mutate',?2,'plak/old-c1',0)",
        rusqlite::params![record.id, copy.display().to_string()]).unwrap();
    drop(db);
    let unowned = repo.path().join(".plak/worktrees/unowned");
    fs::create_dir_all(&unowned).unwrap();
    let (worktrees, incomplete) = unowned_worktrees(&paths);
    assert!(!incomplete, "current tree inventory must be readable");
    assert!(worktrees.contains(&unowned));
    assert!(!worktrees.contains(&copy));
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!old.exists());
    assert!(copy.exists(), "current ownership protects the worktree");
    assert!(
        unowned.exists(),
        "doctor inventory does not remove worktrees"
    );
}

#[test]
fn current_tree_without_a_product_row_is_never_selected() {
    let (_home, paths, _repo, old, copy) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    fs::rename(old.join("subagents.sqlite3"), paths.subagents_database()).unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
    assert!(copy.exists());
    assert!(!old.exists());
}

#[test]
fn current_tree_without_product_database_fails_closed() {
    let (_home, paths, _repo, old, copy) = fixture();
    fs::create_dir_all(paths.state_dir()).unwrap();
    fs::rename(old.join("subagents.sqlite3"), paths.subagents_database()).unwrap();
    let result = prune_legacy(&paths);
    assert!(
        result.is_err(),
        "missing product authority must refuse pruning"
    );
    assert!(copy.exists());
}

#[test]
fn foreign_schema_is_retained_without_blocking_safe_artifacts() {
    let (_home, paths, _repo, old, copy) = fixture();
    let foreign = paths.state_root().join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    let database = rusqlite::Connection::open(foreign.join("subagents.sqlite3")).unwrap();
    database.execute_batch("PRAGMA user_version=99;").unwrap();
    drop(database);
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!copy.exists());
    assert!(foreign.exists());
    assert!(!old.exists());
}

#[test]
fn legacy_artifact_inventory_issue_merge_is_order_independent() {
    let launch = PathBuf::from("fixture launch");
    let artifact = |issue: &str| Artifacts {
        launch: launch.clone(),
        issue: Some(issue.to_owned()),
        ..Artifacts::default()
    };
    let mut forward = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    for issue in ["a retained issue", "z retained issue"] {
        add(&mut forward, artifact(issue));
    }
    for issue in ["z retained issue", "a retained issue"] {
        add(&mut reverse, artifact(issue));
    }
    assert!(
        forward == reverse,
        "issue order must not change the inventory"
    );
    assert_eq!(forward[&launch].issue.as_deref(), Some("a retained issue"));
}

#[test]
fn stray_tombstone_files_are_ignored() {
    let (_home, paths, _repo, old, copy) = fixture();
    let dir = old.join("deleted-artifacts");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".partial"), "not json").unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!copy.exists());
}

#[test]
fn partial_uuid_tombstone_retains_inventory_but_not_safe_artifacts() {
    let (_home, paths, _repo, old, copy) = fixture();
    let dir = old.join("deleted-artifacts");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{}.json", uuid::Uuid::new_v4())),
        "partial",
    )
    .unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
    assert!(!copy.exists());
    assert!(old.exists());
}

#[test]
fn legacy_artifacts_are_listed_and_pruned_under_the_state_locks() {
    let (_home, paths, repo, old, copy) = fixture();
    let mut notice = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    let text = String::from_utf8(notice).unwrap();
    assert!(text.contains("Owned worktree:"));
    assert!(text.contains("refs/heads/plak/old-c1"));
    assert!(text.contains(&maintenance::candidate_ref(
        &maintenance::digest_text("old"),
        "cand-old-a1"
    )));
    assert_eq!(
        super::super::state::prune_confirmed(&paths, &inventory).unwrap(),
        2
    );
    assert!(!copy.exists());
    assert!(!old.exists());
    assert!(
        !repo
            .path()
            .join(".git/plak-namespaces")
            .join(&maintenance::digest_text("old")[..16])
            .exists()
    );
    assert_eq!(
        std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["show-ref", "--verify", "--quiet", "refs/heads/plak/old-c1"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .code(),
        Some(1)
    );
    assert_eq!(
        std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &maintenance::candidate_ref(&maintenance::digest_text("old"), "cand-old-a1")
            ])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .code(),
        Some(1)
    );
}

#[test]
fn dirty_legacy_artifacts_are_named_and_retained_even_with_yes() {
    let (_home, paths, _repo, old, copy) = fixture();
    fs::write(copy.join("uncommitted.txt"), b"keep").unwrap();
    let mut notice = Vec::new();
    confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    assert!(
        String::from_utf8(notice)
            .unwrap()
            .contains("UNCOMMITTED CHANGES; retained")
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(copy.exists());
    assert!(old.exists());
}

#[test]
fn missing_launch_is_retained_without_blocking_other_groups() {
    let (_home, paths, repo, old, copy) = fixture();
    fs::remove_dir_all(repo.path()).unwrap();
    let mut notice = Vec::new();
    confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    assert!(String::from_utf8(notice).unwrap().contains("UNPRUNABLE"));
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(old.exists());
    assert!(!copy.exists());
}

#[test]
fn jj_fork_is_retained_without_blocking_inventory() {
    let (_home, paths, repo, old, _copy) = fixture();
    let fork = repo.path().join(".plak/workspaces/jj-copy");
    fs::create_dir_all(fork.join(".jj")).unwrap();
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    db.execute(
        "UPDATE agent_workspaces SET path=?1",
        [fork.display().to_string()],
    )
    .unwrap();
    drop(db);
    let mut notice = Vec::new();
    confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    let text = String::from_utf8(notice).unwrap();
    assert!(text.contains("JJ artifacts retained"), "{text}");
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(fork.exists());
    assert!(old.exists());
}

#[test]
fn clean_copy_with_unadopted_commit_is_retained() {
    let (_home, paths, _repo, old, copy) = fixture();
    fs::write(copy.join("valuable.txt"), b"unadopted").unwrap();
    let run = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .current_dir(&copy)
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["add", "valuable.txt"]);
    run(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-m",
        "unadopted",
    ]);
    let mut notice = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    assert!(
        String::from_utf8(notice)
            .unwrap()
            .contains("UNADOPTED COMMITS")
    );
    let mut kept = Vec::new();
    let counts =
        super::super::state::prune_confirmed_with_report(&paths, &inventory, &mut kept).unwrap();
    assert_eq!(counts.state_entries, 0);
    assert_eq!(counts.artifact_groups, 0);
    assert_eq!(counts.kept_entries, 1);
    assert!(
        String::from_utf8(kept)
            .unwrap()
            .contains("with 1 unadopted worktrees / 1 unadopted refs")
    );
    assert!(copy.exists());
    assert!(old.exists());
    // A missing copy must not make its unlanded branch disposable.
    fs::remove_dir_all(&copy).unwrap();
    let mut notice = Vec::new();
    confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    assert!(
        String::from_utf8(notice)
            .unwrap()
            .contains("UNADOPTED COMMITS")
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(old.exists());
}

#[test]
fn unadopted_candidate_is_retained_even_at_launch_head() {
    let (_home, paths, repo, old, copy) = fixture();
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    db.execute("UPDATE agent_candidates SET adopted_by=NULL", [])
        .unwrap();
    drop(db);
    let mut notice = Vec::new();
    confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut notice).unwrap();
    assert!(
        String::from_utf8(notice)
            .unwrap()
            .contains("UNADOPTED CANDIDATE")
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    assert!(copy.exists());
    assert!(old.exists());
    let pin = maintenance::candidate_ref(&maintenance::digest_text("old"), "cand-old-a1");
    assert!(
        std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["show-ref", "--verify", &pin])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn discarded_candidate_without_pin_does_not_block_pruning() {
    let (_home, paths, repo, old, copy) = fixture();
    let database = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    database
        .execute("UPDATE agent_candidates SET adopted_by=NULL", [])
        .unwrap();
    drop(database);
    let pin = maintenance::candidate_ref(&maintenance::digest_text("old"), "cand-old-a1");
    assert!(
        std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["update-ref", "-d", &pin])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!copy.exists());
    assert!(!old.exists());
}

#[test]
fn changed_inventory_refuses_before_removal() {
    let (_home, paths, _repo, old, copy) = fixture();
    let inventory =
        confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
    fs::create_dir(paths.state_root().join("r9.e9.p9.t9")).unwrap();
    assert!(
        super::super::state::prune_confirmed(&paths, &inventory)
            .unwrap_err()
            .contains("inventory changed")
    );
    assert!(old.exists());
    assert!(copy.exists());
}

#[test]
fn retained_group_does_not_block_a_safe_group() {
    let (_home, paths, _repo, old, copy) = fixture();
    let db = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO agent_trees VALUES('missing', ?1)",
        ["/missing/plak-test-launch"],
    )
    .unwrap();
    db.execute(
        "INSERT INTO agent_workspaces VALUES('missing', ?1, 'fork')",
        ["/missing/plak-test-launch/.plak/worktrees/missing"],
    )
    .unwrap();
    drop(db);
    let mut report = Vec::new();
    let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report).unwrap();
    assert!(
        String::from_utf8(report)
            .unwrap()
            .contains("launch ancestor missing (unmounted volume?)")
    );
    assert_eq!(
        super::super::state::prune_confirmed(&paths, &inventory).unwrap(),
        1
    );
    assert!(!copy.exists());
    assert!(old.exists(), "retained group's inventory must survive");
    assert_eq!(
        prune_legacy(&paths).unwrap(),
        1,
        "already removed safe group is harmless"
    );
}

#[test]
fn carried_forward_current_session_protects_foreign_source_artifacts() {
    let (_home, paths, _repo, old, copy) = fixture();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    let source = uuid::Uuid::new_v4().to_string();
    let database = rusqlite::Connection::open(old.join("subagents.sqlite3")).unwrap();
    for table in ["agent_trees", "agent_workspaces", "agent_candidates"] {
        database
            .execute(&format!("UPDATE {table} SET tree_id=?1"), [&source])
            .unwrap();
    }
    drop(database);
    record.bind_plakous_locator(
        "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap()
    ).unwrap();
    record.link_carried_forward(&source, "r1.e1.p1.t1");
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
    assert!(!old.exists());
    assert!(copy.exists());
    assert!(
        paths
            .state_dir()
            .join("deleted-artifacts")
            .join(format!("{source}.json"))
            .exists()
    );
    assert_eq!(prune_legacy(&paths).unwrap(), 0);
    crate::store::ProductStore::open(&paths)
        .unwrap()
        .delete_session(&record.id, record.store_revision)
        .unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
    assert!(!copy.exists());
}

#[test]
fn a_busy_legacy_holder_protects_its_worktrees_and_refs() {
    let (_home, paths, _repo, old, copy) = fixture();
    let holders = old.join("runtime.sqlite3-holders");
    fs::create_dir(&holders).unwrap();
    let holder = fs::File::create(holders.join("session")).unwrap();
    rustix::fs::flock(
        &holder,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    assert!(prune_legacy(&paths).unwrap_err().contains("holder lock"));
    assert!(copy.exists());
    assert!(old.exists());
}

#[test]
fn a_busy_current_holder_does_not_protect_legacy_artifacts() {
    let (_home, paths, _repo, old, copy) = fixture();
    let holders = paths.state_dir().join("runtime.sqlite3-holders");
    fs::create_dir_all(&holders).unwrap();
    let holder = fs::File::create(holders.join("session")).unwrap();
    rustix::fs::flock(
        &holder,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!copy.exists());
    assert!(!old.exists());
    assert!(holders.join("session").exists());
}

#[test]
fn noncanonical_home_current_sessions_protect_overlapping_physical_artifacts() {
    let (_home, original, _repo, old, copy) = fixture();
    fs::create_dir_all(original.home().join("nested")).unwrap();
    let paths = PlakPaths::explicit(original.home().join("nested/.."));
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
    crate::store::sessions::save_session(&paths, &mut record).unwrap();
    let database = rusqlite::Connection::open(paths.subagents_database()).unwrap();
    database.execute_batch("PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT,launch_checkout TEXT); CREATE TABLE agent_workspaces(tree_id TEXT,path TEXT,mode TEXT); CREATE TABLE agent_candidates(tree_id TEXT,candidate_id TEXT,adopted_by TEXT);").unwrap();
    let launch = copy.parent().unwrap().parent().unwrap().parent().unwrap();
    database
        .execute(
            "INSERT INTO agent_trees VALUES(?1,?2)",
            rusqlite::params![record.id, launch.display().to_string()],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO agent_workspaces VALUES(?1,?2,'fork')",
            rusqlite::params![record.id, copy.display().to_string()],
        )
        .unwrap();
    drop(database);
    let holders = paths.state_dir().join("runtime.sqlite3-holders");
    fs::create_dir_all(&holders).unwrap();
    let holder = fs::File::create(holders.join("session")).unwrap();
    rustix::fs::flock(
        &holder,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    assert!(!old.exists());
    assert!(copy.exists());
}

#[test]
fn prunable_missing_worktrees_release_registration_and_branch() {
    let (_home, paths, repo, _old, copy) = fixture();
    fs::remove_dir_all(&copy).unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 2);
    let registrations = std::process::Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&registrations.stdout).contains(&copy.display().to_string()));
}

#[test]
fn changed_tree_inventory_can_retry_a_partial_delete() {
    use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, Tier};
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = git_repo();
    paths.ensure_layout().unwrap();
    let record = crate::store::sessions::SessionRecord::new(repo.path().to_path_buf());
    let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
        &record.id,
        repo.path().to_path_buf(),
    );
    let request =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "editor", "edit").with_fork();
    tree.spawn_agent("root", &request, None).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    remember_deleted_tree(&paths, &record.id).unwrap();
    tree.spawn_agent("root", &request, None).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    remember_deleted_tree(&paths, &record.id).unwrap();
    let artifact: Artifacts = serde_json::from_slice(
        &fs::read(
            paths
                .state_dir()
                .join("deleted-artifacts")
                .join(format!("{}.json", record.id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(artifact.worktrees.len(), 2);
    for node in tree
        .nodes()
        .values()
        .filter(|node| node.parent_agent_id.is_some())
    {
        let branch = node.branch.as_ref().unwrap();
        assert!(
            artifact
                .references
                .contains(&format!("refs/heads/{branch}"))
        );
        let dir = node.path.file_name().unwrap().to_str().unwrap();
        assert!(
            !artifact
                .references
                .contains(&format!("refs/heads/plak/{dir}"))
        );
    }
}

#[test]
fn deleted_tree_has_no_live_resolver_input_protection() {
    use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, Tier};
    let (_home, paths) = PlakPaths::isolated_test_home();
    let repo = git_repo();
    paths.ensure_layout().unwrap();
    let record = crate::store::sessions::SessionRecord::new(repo.path().to_path_buf());
    let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
        &record.id,
        repo.path().to_path_buf(),
    );
    let request =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "editor", "edit").with_fork();
    let child = tree.spawn_agent("root", &request, None).unwrap();
    fs::write(
        tree.node(&child.agent_id).unwrap().path.join("SEED.txt"),
        "child\n",
    )
    .unwrap();
    fs::write(repo.path().join("SEED.txt"), "parent\n").unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let source = tree
        .attempt(&child.agent_id)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    let adopt = serde_json::from_value(serde_json::json!({"candidate_id": source})).unwrap();
    assert!(tree.adopt_candidate("root", &adopt, None).is_err());
    let conflict = tree
        .candidates()
        .values()
        .find(|row| row.source_candidate_id.is_some())
        .unwrap()
        .candidate_id
        .clone();
    let mut resolver = request;
    resolver.inputs = Some(vec![conflict.to_string()]);
    let resolver = tree.spawn_agent("root", &resolver, None).unwrap();
    let path = tree.node(&resolver.agent_id).unwrap().path.clone();
    fs::write(path.join("SEED.txt"), "parent\n").unwrap();
    fs::write(path.join("RESOLVED.txt"), "resolution\n").unwrap();
    tree.record_turn_completed(&resolver.agent_id).unwrap();
    tree.settle_for_test();
    let candidate = tree
        .attempt(&resolver.agent_id)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    let adopt = serde_json::from_value(serde_json::json!({"candidate_id": candidate})).unwrap();
    tree.adopt_candidate("root", &adopt, None).unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    remember_deleted_tree(&paths, &record.id).unwrap();
    let artifact: Artifacts = serde_json::from_slice(
        &fs::read(
            paths
                .state_dir()
                .join("deleted-artifacts")
                .join(format!("{}.json", record.id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(artifact.unadopted, BTreeSet::new());
}

#[test]
fn deleted_session_tombstones_keep_ownership_after_tree_rows_are_gone() {
    let (_home, paths, repo, old, copy) = fixture();
    fs::remove_dir_all(old).unwrap();
    let dir = paths.state_dir().join("deleted-artifacts");
    fs::create_dir_all(&dir).unwrap();
    let tombstone = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
    fs::write(
        &tombstone,
        serde_json::to_vec(&Artifacts {
            launch: repo.path().to_path_buf(),
            worktrees: BTreeSet::from([copy.clone()]),
            unadopted: BTreeSet::new(),
            issue: None,
            references: BTreeSet::from([
                "refs/heads/plak/old-c1".to_owned(),
                maintenance::candidate_ref(&maintenance::digest_text("old"), "cand-old-a1"),
            ]),
            ..Artifacts::default()
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(prune_legacy(&paths).unwrap(), 1);
    assert!(!copy.exists());
    assert!(!tombstone.exists());
}
