use super::*;
use std::ffi::OsStr;

#[test]
fn isolated_test_homes_resolve_symlinked_roots_and_remain_distinct() {
    let root = tempfile::tempdir().unwrap();
    let physical = root.path().join("physical");
    fs::create_dir(&physical).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&physical, &alias).unwrap();
    let first = PlakPaths::isolated_tempdir_in(&alias);
    let second = PlakPaths::isolated_tempdir_in(&alias);
    assert_eq!(
        first.path().parent().unwrap(),
        physical.canonicalize().unwrap()
    );
    assert_ne!(first.path(), second.path());
    assert_eq!(first.path(), first.path().canonicalize().unwrap());
}

#[test]
fn resolve_xdg_state_home_matrix_rejects_relative_roots_and_handles_empty_values() {
    assert_eq!(
        resolve_xdg_state_home(Some(OsStr::new("/state")), None).unwrap(),
        PathBuf::from("/state")
    );
    assert_eq!(
        resolve_xdg_state_home(None, Some(OsStr::new("/home/user"))).unwrap(),
        PathBuf::from("/home/user/.local/state")
    );
    assert_eq!(
        resolve_xdg_state_home(Some(OsStr::new("")), Some(OsStr::new("/home/user"))).unwrap(),
        PathBuf::from("/home/user/.local/state")
    );
    assert!(resolve_xdg_state_home(Some(OsStr::new("relative-state")), None).is_err());
    assert!(resolve_xdg_state_home(None, Some(OsStr::new("relative-home"))).is_err());
    assert!(
        resolve_xdg_state_home(Some(OsStr::new("")), Some(OsStr::new("relative-home"))).is_err()
    );
    assert!(resolve_xdg_state_home(Some(OsStr::new("")), Some(OsStr::new(""))).is_err());
    assert!(resolve_xdg_state_home(None, None).is_err());
}

#[test]
fn project_discovery_finds_the_nearest_configuration_home() {
    let root = tempfile::tempdir().unwrap();
    let stop = Some(root.path());
    let product_home = root.path().join("elsewhere/home");
    let outer = root.path().join("outer");
    let nested = outer.join("a/b/c");
    fs::create_dir_all(&nested).unwrap();
    // No `.plak` anywhere: nothing is discovered.
    assert_eq!(
        discover_project_root(&nested, &[product_home.as_path()], stop),
        None
    );
    // An empty `.plak` is not a configuration home.
    fs::create_dir(outer.join(".plak")).unwrap();
    assert_eq!(
        discover_project_root(&nested, &[product_home.as_path()], stop),
        None
    );
    // `agents/` alone makes it a home, and it answers from a deep cwd.
    fs::create_dir(outer.join(".plak/agents")).unwrap();
    assert_eq!(
        discover_project_root(&nested, &[product_home.as_path()], stop),
        Some(outer.canonicalize().unwrap())
    );
    // A closer home wins over an outer one.
    let inner = outer.join("a");
    fs::create_dir_all(inner.join(".plak")).unwrap();
    fs::write(inner.join(".plak/config.toml"), "").unwrap();
    assert_eq!(
        discover_project_root(&nested, &[product_home.as_path()], stop),
        Some(inner.canonicalize().unwrap())
    );
    // From the home directory itself the same root answers.
    assert_eq!(
        discover_project_root(&inner, &[product_home.as_path()], stop),
        Some(inner.canonicalize().unwrap())
    );
}

#[test]
fn workspace_constructor_discovers_parent_overlay_and_excludes_product_home() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join(".plak");
    fs::create_dir_all(home.join("agents")).unwrap();
    let nested = root.path().join("project/nested");
    fs::create_dir_all(&nested).unwrap();
    // Discovery keeps walking past the temporary root, so a real home in a
    // developer's ancestors may answer; nothing inside the root may.
    let canonical_root = root.path().canonicalize().unwrap();
    let outside_root = |paths: PlakPaths| {
        paths
            .project_root()
            .is_none_or(|found| !found.starts_with(&canonical_root))
    };
    assert!(outside_root(PlakPaths::for_workspace(
        home.clone(),
        root.path()
    )));
    assert!(outside_root(PlakPaths::for_workspace(
        home.clone(),
        &nested
    )));
    let project = root.path().join("project");
    fs::create_dir_all(project.join(".plak/agents")).unwrap();
    assert_eq!(
        PlakPaths::for_workspace(home, &nested).project_root(),
        Some(project.canonicalize().unwrap().as_path())
    );
}

#[test]
fn launch_discovery_never_takes_the_default_home_for_a_project() {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    let work = user.join("work/repo");
    fs::create_dir_all(user.join(".plak/agents")).unwrap();
    fs::write(user.join(".plak/config.toml"), "").unwrap();
    fs::create_dir_all(&work).unwrap();
    let private = root.path().join("private-home");
    let paths = PlakPaths::explicit(private.clone()).discovered_from(Some(user.as_os_str()), &work);
    assert_eq!(paths.project_root(), None);
    // A real project below `$HOME` is still discovered.
    fs::create_dir_all(work.join(".plak/agents")).unwrap();
    let paths = PlakPaths::explicit(private).discovered_from(Some(user.as_os_str()), &work);
    assert_eq!(
        paths.project_root(),
        Some(work.canonicalize().unwrap().as_path())
    );
}

#[test]
fn project_discovery_never_returns_the_product_home() {
    let root = tempfile::tempdir().unwrap();
    let product_home = root.path().join(".plak");
    fs::create_dir_all(product_home.join("agents")).unwrap();
    // From the home's own parent, the product home is excluded even though
    // it is a real configuration home.
    assert_eq!(
        discover_project_root(root.path(), &[product_home.as_path()], Some(root.path())),
        None
    );
}

#[cfg(unix)]
#[test]
fn project_discovery_fails_closed_on_a_symlinked_home() {
    let root = tempfile::tempdir().unwrap();
    let product_home = root.path().join("elsewhere/home");
    let real = root.path().join("real");
    let work = root.path().join("work");
    fs::create_dir_all(real.join(".plak/agents")).unwrap();
    fs::create_dir_all(&work).unwrap();
    std::os::unix::fs::symlink(real.join(".plak"), work.join(".plak")).unwrap();
    // `.plak` itself is a symlink: not a configuration home.
    assert_eq!(
        discover_project_root(&work, &[product_home.as_path()], Some(root.path())),
        None
    );
}

#[test]
fn resolves_home_dot_plak() {
    let paths = PlakPaths::resolve(None, Some(OsStr::new("/home/user"))).unwrap();
    assert_eq!(paths.home(), Path::new("/home/user/.plak"));
    assert_eq!(
        paths.config_file(),
        PathBuf::from("/home/user/.plak/config.toml")
    );
}

#[test]
fn plak_home_overrides() {
    let paths = PlakPaths::resolve(
        Some(OsStr::new("/tmp/plak-home")),
        Some(OsStr::new("/home/user")),
    )
    .unwrap();
    assert_eq!(paths.home(), Path::new("/tmp/plak-home"));
}

#[cfg(unix)]
#[test]
fn private_state_home_refuses_symlink_and_permissive_home_without_mutation() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temporary = tempfile::tempdir().unwrap();
    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o755)).unwrap();
    let sentinel = external.join("sentinel");
    fs::write(&sentinel, "outside").unwrap();
    let linked = temporary.path().join("linked-home");
    symlink(&external, &linked).unwrap();

    assert!(
        PlakPaths::explicit(linked)
            .ensure_private_state_home()
            .is_err()
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "outside");
    assert_eq!(
        fs::metadata(&external).unwrap().permissions().mode() & 0o777,
        0o755
    );

    let permissive = temporary.path().join("permissive-home");
    fs::create_dir(&permissive).unwrap();
    fs::write(permissive.join("config.toml"), "").unwrap();
    fs::set_permissions(&permissive, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        PlakPaths::explicit(permissive.clone())
            .ensure_private_state_home()
            .is_err()
    );
    assert_eq!(
        fs::metadata(permissive).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn private_state_home_is_created_private_and_layout_refuses_symlinked_child() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temporary = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temporary.path().join("home"));
    paths.ensure_private_state_home().unwrap();
    assert_eq!(
        fs::metadata(paths.home()).unwrap().permissions().mode() & 0o777,
        0o700
    );

    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    let sentinel = external.join("sentinel");
    fs::write(&sentinel, "outside").unwrap();
    symlink(&external, paths.secrets_dir()).unwrap();
    assert!(paths.ensure_layout().is_err());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "outside");
}

#[cfg(unix)]
#[test]
fn private_state_home_rejects_symlinked_ancestor() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::set_permissions(external.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let external_home = external.path().join("plak");
    fs::create_dir(&external_home).unwrap();
    fs::set_permissions(&external_home, fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = external_home.join("sentinel");
    fs::write(&sentinel, b"untouched").unwrap();
    let redirected_parent = root.path().join("redirected");
    symlink(external.path(), &redirected_parent).unwrap();

    let paths = PlakPaths::explicit(redirected_parent.join("plak"));
    assert!(paths.ensure_private_state_home().is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
}
