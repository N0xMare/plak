use super::{
    Errno, ExecutionBackendErrorKind, ExecutionWorkspace, Mode, OFlags, create_missing_parents, fs,
    open_workspace_beneath, openat_component,
};
use crate::{ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope};
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
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

/// A directory open of a multi-component path classifies the final component
/// only. A file in the middle stays not-a-directory; a final link is a symlink
/// escape. Neither open creates anything.
#[test]
fn a_multi_component_open_reports_a_file_parent_and_a_final_link() -> TestResult {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::create_dir(root.path().join("dir"))?;
    fs::write(root.path().join("dir").join("file"), b"x")?;
    symlink(outside.path(), root.path().join("dir").join("link"))?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000997".parse()?),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let workspace = ExecutionWorkspace::from_environment(&environment);
    let directory = OFlags::RDONLY | OFlags::DIRECTORY;
    let Err(through_file) =
        open_workspace_beneath(&workspace, Path::new("dir/file/leaf"), directory)
    else {
        return Err("a file component was opened as a directory".into());
    };
    assert_eq!(
        through_file.kind(),
        ExecutionBackendErrorKind::NotADirectory
    );
    assert!(
        through_file
            .message()
            .as_str()
            .contains("a parent component is not a directory: dir/file/leaf"),
        "{:?}",
        through_file.message()
    );
    assert!(through_file.proven_no_effect());
    let Err(through_link) = open_workspace_beneath(&workspace, Path::new("dir/link"), directory)
    else {
        return Err("a final link was opened".into());
    };
    assert_eq!(
        through_link.kind(),
        ExecutionBackendErrorKind::SymlinkEscape
    );
    assert!(
        through_link
            .message()
            .as_str()
            .contains("path traverses a symbolic link: dir/link"),
        "{:?}",
        through_link.message()
    );
    assert!(through_link.proven_no_effect());
    assert_eq!(fs::read(root.path().join("dir").join("file"))?, b"x");
    assert_eq!(fs::read_dir(outside.path())?.count(), 0);
    Ok(())
}

/// A search-only parent has no read permission. Classification still opens it
/// and reports a final link as a symlink escape, which is what a readable
/// parent reports.
#[cfg(target_os = "linux")]
#[test]
fn a_search_only_parent_of_a_final_link_is_a_symlink_escape() -> TestResult {
    if rustix::process::geteuid().is_root() {
        return Ok(());
    }
    let root = tempfile::tempdir()?;
    let parent = root.path().join("search");
    fs::create_dir(&parent)?;
    symlink("target", parent.join("link"))?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000996".parse()?),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let workspace = ExecutionWorkspace::from_environment(&environment);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o100))?;
    let restore = RestoreSearch { path: parent };
    let Err(error) = open_workspace_beneath(
        &workspace,
        Path::new("search/link"),
        OFlags::RDONLY | OFlags::DIRECTORY,
    ) else {
        return Err("a final link under a search-only parent was opened".into());
    };
    assert_eq!(error.kind(), ExecutionBackendErrorKind::SymlinkEscape);
    assert!(
        error
            .message()
            .as_str()
            .contains("path traverses a symbolic link: search/link"),
        "{:?}",
        error.message()
    );
    assert!(error.proven_no_effect());
    drop(restore);
    Ok(())
}

/// `inlink/outlink` stated from the root follows `inlink` and sees the outside
/// symlink. The confined classifier refuses to open `inlink` and keeps the
/// original `ENOTDIR`.
#[cfg(target_os = "linux")]
#[test]
fn confined_classification_does_not_follow_an_intermediate_link() -> TestResult {
    let outside = tempfile::tempdir()?;
    symlink("/", outside.path().join("outlink"))?;
    let root = tempfile::tempdir()?;
    symlink(outside.path(), root.path().join("inlink"))?;
    let root_fd = fs::File::open(root.path())?;
    let path = Path::new("inlink/outlink");
    assert_eq!(
        super::confined_link_errno(&root_fd, path, DIRECTORY, Errno::NOTDIR),
        Errno::NOTDIR
    );
    assert_eq!(
        super::link_errno(&root_fd, path, DIRECTORY, Errno::NOTDIR),
        Errno::LOOP
    );
    Ok(())
}

#[cfg(target_os = "linux")]
struct RestoreSearch {
    path: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl Drop for RestoreSearch {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
    }
}

/// Walking and scanning a repository with many files and nested directories
/// keeps open descriptors bounded and filters regular-file globs before open.
#[test]
fn repository_query_filters_globs_before_open_and_bounds_open_fds() -> TestResult {
    use super::super::{OperationContext, RepositoryQuery, RepositoryQueryLimits};
    use plakous_model::CancellationToken;

    let root = tempfile::tempdir()?;
    for dir_idx in 0..10 {
        let dir = root.path().join(format!("dir_{dir_idx:02}"));
        fs::create_dir(&dir)?;
        for file_idx in 0..15 {
            fs::write(
                dir.join(format!("file_{file_idx:02}.rs")),
                format!("needle_{dir_idx}_{file_idx}\n"),
            )?;
            fs::write(
                dir.join(format!("ignored_{file_idx:02}.txt")),
                "needle_non_matching_ext\n",
            )?;
        }
    }
    #[cfg(target_os = "linux")]
    let unreadable = if rustix::process::geteuid().is_root() {
        None
    } else {
        let path = root.path().join("dir_00").join("unreadable.txt");
        fs::write(&path, "needle_unreadable\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))?;
        Some(RestoreSearch { path })
    };

    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000995".parse()?),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let workspace = ExecutionWorkspace::from_environment(&environment);
    let control = OperationContext::new(CancellationToken::new());
    let limits = RepositoryQueryLimits {
        max_files: 500,
        max_matches: 500,
        max_tree_entries: 1000,
        max_file_bytes: 64 * 1024,
        max_total_bytes: 1024 * 1024,
        max_match_bytes: 256,
        max_result_bytes: 1024 * 1024,
    };

    #[cfg(target_os = "linux")]
    let fds_before = fs::read_dir("/proc/self/fd")?.count();

    let scan = super::query_repository_sync(
        &workspace,
        &RepositoryQuery::new(".", "needle", crate::RepositoryMatchCase::Sensitive, limits)?
            .with_path_glob("*.rs")?,
        &control,
    )?;
    assert_eq!(scan.files_considered, 150);
    assert_eq!(scan.files_scanned, 150);
    assert_eq!(scan.matches.len(), 150);
    assert_eq!(
        scan.entries_skipped, 0,
        "non-matching unreadable .txt file must be filtered before open"
    );

    let listed = super::query_repository_sync(
        &workspace,
        &RepositoryQuery::glob(".", "*.rs", limits)?,
        &control,
    )?;
    assert_eq!(listed.files_considered, 150);
    assert_eq!(listed.matches.len(), 150);
    assert_eq!(listed.entries_skipped, 0);

    #[cfg(target_os = "linux")]
    {
        let fds_after = fs::read_dir("/proc/self/fd")?.count();
        assert!(
            fds_after.saturating_sub(fds_before) <= 8,
            "query leaked open file descriptors: before={fds_before} after={fds_after}"
        );
        drop(unreadable);
    }
    Ok(())
}
