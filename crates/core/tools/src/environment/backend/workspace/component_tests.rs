use super::{
    Errno, ExecutionBackendErrorKind, ExecutionWorkspace, Mode, OFlags, create_missing_parents, fs,
    openat_component,
};
use crate::{ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope};
use std::os::unix::fs::symlink;
use std::path::Path;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Both kernels answer a no-follow directory open of any link with ENOTDIR;
/// the component open reports every link as a loop and leaves the rest alone.
#[test]
fn a_linked_component_is_a_loop_and_a_file_component_is_not_a_directory() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("dir"))?;
    fs::write(root.path().join("file"), b"x")?;
    symlink("dir", root.path().join("l_dir"))?;
    symlink("file", root.path().join("l_file"))?;
    symlink("missing", root.path().join("l_dangling"))?;
    let parent = fs::File::open(root.path())?;
    let open = |name: &str, flags| openat_component(&parent, name, flags, Mode::empty()).err();
    for link in ["l_dir", "l_file", "l_dangling"] {
        assert_eq!(open(link, DIRECTORY), Some(Errno::LOOP), "{link}");
    }
    assert_eq!(open("file", DIRECTORY), Some(Errno::NOTDIR));
    assert_eq!(open("missing", DIRECTORY), Some(Errno::NOENT));
    assert_eq!(open("dir", DIRECTORY), None);
    // Without O_DIRECTORY an ENOTDIR names a non-directory component.
    let file = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    assert_eq!(open("file/child", file), Some(Errno::NOTDIR));
    Ok(())
}

/// A create beneath a linked parent refuses before anything is made, on the
/// link's side or the workspace's.
#[test]
fn a_create_beneath_a_linked_component_is_a_symlink_escape_with_no_effect() -> TestResult {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    symlink(outside.path(), root.path().join("link"))?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000998".parse()?),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let workspace = ExecutionWorkspace::from_environment(&environment);
    let Err(error) = create_missing_parents(&workspace, Path::new("link/new")) else {
        return Err("a linked parent was traversed".into());
    };
    assert_eq!(error.kind(), ExecutionBackendErrorKind::SymlinkEscape);
    assert!(error.proven_no_effect());
    assert_eq!(fs::read_dir(outside.path())?.count(), 0);
    assert_eq!(fs::read_dir(root.path())?.count(), 1);
    Ok(())
}
