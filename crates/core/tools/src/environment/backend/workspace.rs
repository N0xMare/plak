//! Workspace execution backend responsibilities.

use plakous_model::{Digest, DigestDomain};
#[cfg(unix)]
use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, RenameFlags, mkdirat, open, openat, renameat_with, unlinkat,
};
#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, openat2};
#[cfg(unix)]
use rustix::io::Errno;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use tokio::sync::Semaphore;

use std::io::Write;
#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

#[cfg(not(unix))]
use super::ready_error;
#[cfg(unix)]
use super::workspace_types::compile_query_regex;
use super::{
    ExecutionBackendError, ExecutionBackendErrorKind, ExecutionBackendFuture,
    ExecutionOperationControl, ExecutionWorkspace, MAX_WORKSPACE_FILE_BYTES, RepositoryMatchCase,
    RepositoryQuery, RepositoryQueryMatch, RepositoryQueryResult, RepositoryQueryStopReason,
    WORKSPACE_CONTENT_IDENTITY_REVISION, WorkspaceContentIdentity, WorkspaceExpectedFileState,
    WorkspaceMutationDisposition, WorkspaceMutationResultItem, WorkspaceMutationTransaction,
    WorkspaceMutationTransactionResult, WorkspaceReadCursor, WorkspaceReadSlice,
    WorkspaceReadSliceRequest, WorkspaceReplacement, backend_error, conflict_error, io_error, lock,
    recovery_required_error, resource_error, validate_path_bytes, validate_relative_path,
    workspace_error,
};
#[cfg(unix)]
pub(super) fn verified_workspace_root(
    workspace: &ExecutionWorkspace,
) -> Result<fs::File, ExecutionBackendError> {
    let descriptor = open(
        workspace.root(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| workspace_error("workspace root descriptor open failed"))?;
    let root = fs::File::from(descriptor);
    let metadata = root
        .metadata()
        .map_err(|_| io_error("workspace root descriptor metadata failed"))?;
    if metadata.dev() != workspace.root_device || metadata.ino() != workspace.root_inode {
        return Err(conflict_error("workspace root object changed"));
    }
    Ok(root)
}

#[cfg(unix)]
pub(super) fn workspace_descriptor_path(
    _workspace: &ExecutionWorkspace,
    relative: &Path,
) -> Result<PathBuf, ExecutionBackendError> {
    validate_relative_path(relative, true)?;
    Ok(if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative.to_path_buf()
    })
}

/// Names what the kernel actually said about a workspace path.
///
/// Every one of these was a `SymlinkEscape` carrying
/// "workspace descriptor resolution refused the path", because the mapping was
/// `Err(_) =>` and the `Errno` was thrown away. A missing file was reported as
/// a symlink escape, and a model that misspelled a filename was told nothing it
/// could act on. The kernel already knew; nothing was reading it.
///
/// Each of these is decided by looking, before anything is opened for writing,
/// so each is proven to have had no effect.
#[cfg(unix)]
fn workspace_open_error(relative: &Path, errno: Errno) -> ExecutionBackendError {
    let path = relative.display();
    let error = match errno {
        Errno::NOENT => backend_error(
            ExecutionBackendErrorKind::NotFound,
            format!("path does not exist: {path}"),
        ),
        // A component of the path is not a directory, so the path cannot name
        // anything. Saying which is more useful than saying "not found".
        Errno::NOTDIR => backend_error(
            ExecutionBackendErrorKind::NotFound,
            format!("path does not exist; a parent component is not a directory: {path}"),
        ),
        Errno::ISDIR => backend_error(
            ExecutionBackendErrorKind::IsADirectory,
            format!("path is a directory: {path}"),
        ),
        // `NO_SYMLINKS` reports a link as a loop, and that is the one case
        // `SymlinkEscape` is for.
        Errno::LOOP => backend_error(
            ExecutionBackendErrorKind::SymlinkEscape,
            format!("path traverses a symbolic link: {path}"),
        ),
        // `BENEATH` reports an escape it blocked before following anything.
        Errno::XDEV => backend_error(
            ExecutionBackendErrorKind::OutsideWorkspace,
            format!("path resolves outside the workspace: {path}"),
        ),
        Errno::NXIO | Errno::NODEV | Errno::OPNOTSUPP => backend_error(
            ExecutionBackendErrorKind::NotARegularFile,
            format!("path is not a regular file: {path}"),
        ),
        _ => backend_error(
            ExecutionBackendErrorKind::Io,
            format!("workspace path could not be opened: {path}"),
        ),
    };
    error.with_proven_no_effect(true)
}

#[cfg(unix)]
fn workspace_path_components(
    relative: &Path,
) -> Result<Vec<&std::ffi::OsStr>, ExecutionBackendError> {
    let mut names = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => names.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(backend_error(
                    ExecutionBackendErrorKind::OutsideWorkspace,
                    format!("path is outside the workspace: {}", relative.display()),
                ));
            }
        }
    }
    Ok(names)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_workspace_beneath_at(
    root: &fs::File,
    relative: &Path,
    flags: OFlags,
) -> Result<fs::File, ExecutionBackendError> {
    let names = workspace_path_components(relative)?;
    if names.is_empty() {
        return openat(
            root,
            ".",
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(fs::File::from)
        .map_err(|errno| workspace_open_error(relative, errno));
    }
    let mut owned: Option<fs::File> = None;
    for (index, name) in names.iter().enumerate() {
        let is_last = index + 1 == names.len();
        let open_flags = if is_last {
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        let parent = owned.as_ref().map_or(root, |file| file);
        match openat(parent, *name, open_flags, Mode::empty()) {
            Ok(descriptor) => {
                if is_last {
                    return Ok(fs::File::from(descriptor));
                }
                owned = Some(fs::File::from(descriptor));
            }
            Err(errno) => return Err(workspace_open_error(relative, errno)),
        }
    }
    Err(workspace_open_error(relative, Errno::NOENT))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn observe_open_workspace_file_at(
    root: &fs::File,
    relative: &Path,
) -> Result<Option<fs::File>, ExecutionBackendError> {
    let names = workspace_path_components(relative)?;
    if names.is_empty() {
        return match openat(
            root,
            ".",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => {
                let file = fs::File::from(descriptor);
                if file
                    .metadata()
                    .map_err(|_| io_error("workspace mutation metadata failed"))?
                    .is_file()
                {
                    Ok(Some(file))
                } else {
                    Err(workspace_error(
                        "workspace mutation target is not a regular file",
                    ))
                }
            }
            Err(Errno::NOENT) => Ok(None),
            Err(Errno::LOOP | Errno::XDEV) => Err(backend_error(
                ExecutionBackendErrorKind::SymlinkEscape,
                "workspace mutation refuses symbolic links",
            )),
            Err(_) => Err(io_error("workspace mutation target open failed")),
        };
    }
    let mut owned: Option<fs::File> = None;
    for (index, name) in names.iter().enumerate() {
        let is_last = index + 1 == names.len();
        let open_flags = if is_last {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        let parent = owned.as_ref().map_or(root, |file| file);
        match openat(parent, *name, open_flags, Mode::empty()) {
            Ok(descriptor) => {
                if is_last {
                    let file = fs::File::from(descriptor);
                    if file
                        .metadata()
                        .map_err(|_| io_error("workspace mutation metadata failed"))?
                        .is_file()
                    {
                        return Ok(Some(file));
                    }
                    return Err(workspace_error(
                        "workspace mutation target is not a regular file",
                    ));
                }
                owned = Some(fs::File::from(descriptor));
            }
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP | Errno::XDEV) => {
                return Err(backend_error(
                    ExecutionBackendErrorKind::SymlinkEscape,
                    "workspace mutation refuses symbolic links",
                ));
            }
            Err(_) => return Err(io_error("workspace mutation target open failed")),
        }
    }
    Ok(None)
}

/// What one directory child contributed to the bounded repository walk.
///
/// A walk is a survey of a live tree, not a transaction over it: an entry that
/// is not traversable never aborts the query. Only an opened descriptor carries
/// the walk forward.
#[cfg(unix)]
enum RepositoryQueryEntry {
    /// The child opened and may be a directory or a regular file.
    Opened(fs::File),
    /// The child disappeared between `read_dir` and `open`.
    Vanished,
    /// The child is a symbolic link, which the walk never follows.
    Symlink,
    /// The child is not traversable: a socket, fifo, device, or failed open.
    Skipped,
}

/// Reports whether a directory child's kind can take part in the walk.
///
/// `None` means the kind is unknown and only the opened descriptor's metadata
/// can decide.
#[cfg(unix)]
const fn query_entry_kind_is_walkable(file_type: rustix::fs::FileType) -> Option<bool> {
    match file_type {
        rustix::fs::FileType::Directory
        | rustix::fs::FileType::RegularFile
        | rustix::fs::FileType::Symlink => Some(true),
        rustix::fs::FileType::Unknown => None,
        _ => Some(false),
    }
}

#[cfg(unix)]
fn open_directory_entry_for_query(directory: &fs::File, name: &Path) -> RepositoryQueryEntry {
    // `O_NONBLOCK` keeps an unknown-kind fifo from parking the walk on a
    // writer that may never arrive; regular files and directories ignore it.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    #[cfg(target_os = "linux")]
    let opened = openat2(
        directory,
        name,
        flags,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    );
    #[cfg(all(unix, not(target_os = "linux")))]
    let opened = openat(directory, name, flags, Mode::empty());
    match opened {
        Ok(descriptor) => RepositoryQueryEntry::Opened(fs::File::from(descriptor)),
        Err(Errno::LOOP) => RepositoryQueryEntry::Symlink,
        Err(Errno::NOENT) => RepositoryQueryEntry::Vanished,
        Err(_) => RepositoryQueryEntry::Skipped,
    }
}

#[cfg(unix)]
fn open_workspace_beneath(
    workspace: &ExecutionWorkspace,
    relative: &Path,
    flags: OFlags,
) -> Result<fs::File, ExecutionBackendError> {
    let root = verified_workspace_root(workspace)?;
    let path = workspace_descriptor_path(workspace, relative)?;
    #[cfg(target_os = "linux")]
    {
        let descriptor = openat2(
            &root,
            &path,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|errno| workspace_open_error(relative, errno))?;
        Ok(fs::File::from(descriptor))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        open_workspace_beneath_at(&root, &path, flags)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct FileObservation {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
impl FileObservation {
    fn capture(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[cfg(unix)]
struct WorkspaceSnapshot {
    path: PathBuf,
    content: WorkspaceContentIdentity,
    observation: FileObservation,
    bytes: Arc<[u8]>,
}

#[derive(Default)]
pub(super) struct WorkspaceSnapshotCache {
    #[cfg(unix)]
    entries: VecDeque<WorkspaceSnapshot>,
}

#[cfg(unix)]
impl WorkspaceSnapshotCache {
    fn get(
        &mut self,
        path: &Path,
        content: WorkspaceContentIdentity,
        observation: FileObservation,
    ) -> Option<Arc<[u8]>> {
        let position = self.entries.iter().position(|entry| {
            entry.path == path && entry.content == content && entry.observation == observation
        })?;
        let entry = self.entries.remove(position)?;
        let bytes = Arc::clone(&entry.bytes);
        self.entries.push_back(entry);
        Some(bytes)
    }

    fn insert(&mut self, snapshot: WorkspaceSnapshot) {
        self.entries.retain(|entry| entry.path != snapshot.path);
        self.entries.push_back(snapshot);
        while self.entries.len() > 4 {
            self.entries.pop_front();
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded workspace read, cache, and resume remain one visible semantic operation"
)]
pub(super) fn read_workspace_sync(
    workspace: &ExecutionWorkspace,
    request: &WorkspaceReadSliceRequest,
    control: &ExecutionOperationControl,
    snapshots: &Mutex<WorkspaceSnapshotCache>,
) -> Result<WorkspaceReadSlice, ExecutionBackendError> {
    #[cfg(unix)]
    {
        control.check_cancelled()?;
        let mut file = open_workspace_beneath(workspace, request.path(), OFlags::RDONLY)?;
        let metadata = file
            .metadata()
            .map_err(|_| io_error("workspace file metadata failed"))?;
        if !metadata.is_file() {
            // "not a file" is two different answers, and a model can act on
            // only one of them.
            let path = request.path().display();
            return Err(if metadata.is_dir() {
                backend_error(
                    ExecutionBackendErrorKind::IsADirectory,
                    format!("path is a directory: {path}"),
                )
            } else {
                backend_error(
                    ExecutionBackendErrorKind::NotARegularFile,
                    format!("path is not a regular file: {path}"),
                )
            }
            .with_proven_no_effect(true));
        }
        if metadata.len() > MAX_WORKSPACE_FILE_BYTES {
            return Err(resource_error(
                "workspace file exceeds the hard content identity limit",
            ));
        }
        if request.offset() > metadata.len() {
            return Err(conflict_error(
                "workspace read cursor offset exceeds its content length",
            ));
        }
        let slice_end = request
            .offset()
            .saturating_add(request.max_bytes())
            .min(metadata.len());
        let opened = FileObservation::capture(&metadata);
        let expected = request.expected_content();
        let cached =
            expected.and_then(|content| lock(snapshots).get(request.path(), content, opened));
        let (content, complete_bytes) = if let Some(bytes) = cached {
            (
                expected.ok_or_else(|| conflict_error("workspace read cache was inconsistent"))?,
                bytes,
            )
        } else {
            let mut reader = CancellationReader::new(&mut file, control);
            let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
            reader.read_to_end(&mut bytes).map_err(|_| {
                if reader.cancelled {
                    backend_error(
                        ExecutionBackendErrorKind::Cancelled,
                        "execution environment operation was cancelled",
                    )
                } else {
                    io_error("workspace file read failed")
                }
            })?;
            let after = reader
                .inner
                .metadata()
                .map_err(|_| io_error("workspace file metadata failed"))?;
            if opened != FileObservation::capture(&after)
                || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != metadata.len()
            {
                return Err(conflict_error("workspace file changed while reading"));
            }
            let content = WorkspaceContentIdentity::from_bytes(&bytes)?;
            let bytes: Arc<[u8]> = bytes.into();
            lock(snapshots).insert(WorkspaceSnapshot {
                path: request.path.clone(),
                content,
                observation: opened,
                bytes: Arc::clone(&bytes),
            });
            (content, bytes)
        };
        if request
            .expected_content()
            .is_some_and(|expected| expected != content)
        {
            return Err(conflict_error(
                "workspace content changed before read slice resume",
            ));
        }
        let start = usize::try_from(request.offset())
            .map_err(|_| resource_error("workspace read offset is not addressable"))?;
        let end = usize::try_from(slice_end)
            .map_err(|_| resource_error("workspace read end is not addressable"))?;
        let retained: Arc<[u8]> = complete_bytes[start..end].to_vec().into();
        let next = (slice_end < metadata.len()).then(|| WorkspaceReadCursor {
            path: request.path.clone(),
            content,
            offset: slice_end,
        });
        Ok(WorkspaceReadSlice {
            content,
            offset: request.offset(),
            bytes: retained,
            next,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (workspace, request, control, snapshots);
        Err(backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "workspace reads are unsupported on this platform",
        ))
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded traversal, scan, ordering, and result accounting remain one visible semantic operation"
)]
pub(super) fn query_repository_sync(
    workspace: &ExecutionWorkspace,
    query: &RepositoryQuery,
    control: &ExecutionOperationControl,
) -> Result<RepositoryQueryResult, ExecutionBackendError> {
    #[cfg(unix)]
    {
        control.check_cancelled()?;
        let limits = query.limits();
        let mut entries_skipped = 0_usize;
        // A glob with a literal leading directory prefix can only match
        // beneath that prefix, so the walk starts there instead of at the
        // query root. Pruning changes only what is visited: every path is
        // still named relative to the workspace root, and every containment
        // rule still applies to the pruned start.
        // The glob is relative to the query root.
        let walk_root = match query.path_glob.as_deref().and_then(glob_walk_prefix) {
            Some(prefix) => query.root().join(prefix),
            None => query.root().to_path_buf(),
        };
        let pruned = walk_root != query.root();
        let start_descriptor = match open_workspace_beneath(workspace, &walk_root, OFlags::RDONLY) {
            Ok(descriptor) => descriptor,
            // A glob prefix that names nothing traversable matches nothing.
            Err(_) if pruned => return Ok(empty_repository_query_result(1)),
            Err(error) => return Err(error),
        };
        let start_metadata = start_descriptor
            .metadata()
            .map_err(|_| io_error("repository query root metadata failed"))?;
        let mut pending = BTreeMap::new();
        let root_ignores = ancestor_ignores(workspace, &walk_root);
        let mut files = Vec::new();
        let mut tree_entries = 0_usize;
        let mut stop_reason = None;
        if start_metadata.is_file() {
            files.push((
                walk_root.clone(),
                start_metadata.len(),
                FileObservation::capture(&start_metadata),
                start_descriptor,
            ));
        } else if start_metadata.is_dir() {
            pending.insert(walk_root.clone(), (start_descriptor, root_ignores));
        } else if pruned {
            return Ok(empty_repository_query_result(1));
        } else {
            return Err(workspace_error(
                "repository query root is not a regular file or directory",
            ));
        }
        while let Some((directory_path, (directory, parent_ignores))) = pending.pop_first() {
            control.check_cancelled()?;
            let ignores = directory_ignores(&directory, &directory_path, &parent_ignores);
            let remaining = limits.max_tree_entries.saturating_sub(tree_entries);
            let mut children = Vec::with_capacity(remaining.min(256));
            let Ok(mut entries) = Dir::read_from(&directory) else {
                entries_skipped = entries_skipped.saturating_add(1);
                continue;
            };
            for entry in &mut entries {
                control.check_cancelled()?;
                if children.len() == remaining {
                    stop_reason = Some(RepositoryQueryStopReason::TreeEntries);
                    break;
                }
                let Ok(entry) = entry else {
                    entries_skipped = entries_skipped.saturating_add(1);
                    continue;
                };
                let name = entry.file_name().to_bytes();
                // Version-control metadata is never repository content. Other
                // dot entries (`.github`, `.cargo`) are walked unless ignored.
                if matches!(name, b"." | b".." | b".git" | b".jj") {
                    continue;
                }
                if query_entry_kind_is_walkable(entry.file_type()) == Some(false) {
                    entries_skipped = entries_skipped.saturating_add(1);
                    continue;
                }
                let child_name = PathBuf::from(std::ffi::OsStr::from_bytes(name));
                let is_directory = entry.file_type() == rustix::fs::FileType::Directory;
                if is_ignored(&ignores, &directory_path.join(&child_name), is_directory) {
                    continue;
                }
                children.push(child_name);
            }
            if stop_reason.is_some() {
                break;
            }
            tree_entries = tree_entries.saturating_add(children.len());
            children.sort();
            for child_name in children {
                control.check_cancelled()?;
                let relative = directory_path.join(&child_name);
                validate_path_bytes(&relative)?;
                let child = match open_directory_entry_for_query(&directory, &child_name) {
                    RepositoryQueryEntry::Opened(child) => child,
                    RepositoryQueryEntry::Vanished | RepositoryQueryEntry::Symlink => continue,
                    RepositoryQueryEntry::Skipped => {
                        entries_skipped = entries_skipped.saturating_add(1);
                        continue;
                    }
                };
                let Ok(metadata) = child.metadata() else {
                    entries_skipped = entries_skipped.saturating_add(1);
                    continue;
                };
                if metadata.is_dir() {
                    pending.insert(relative, (child, Arc::clone(&ignores)));
                } else if metadata.is_file() {
                    files.push((
                        relative,
                        metadata.len(),
                        FileObservation::capture(&metadata),
                        child,
                    ));
                } else {
                    // An unknown-kind entry that opened as a socket, fifo or
                    // device has no contents this walk can survey.
                    entries_skipped = entries_skipped.saturating_add(1);
                }
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        if let Some(glob) = query.path_glob.as_deref() {
            let matcher = PathGlob::new(glob)?;
            files.retain(|(relative, _, _, _)| {
                // A file root is matched by its own path; anything beneath a
                // directory root by its path relative to that root.
                let within = relative
                    .strip_prefix(query.root())
                    .ok()
                    .filter(|within| !within.as_os_str().is_empty())
                    .unwrap_or(relative);
                matcher.matches(within)
            });
        }

        let files_considered = files.len().min(limits.max_files);
        if files.len() > files_considered {
            stop_reason = Some(RepositoryQueryStopReason::Files);
        }
        if query.list_only {
            let mut matches = Vec::new();
            let empty = WorkspaceContentIdentity::from_bytes(&[])?;
            for (relative, _, _, _) in files.into_iter().take(files_considered) {
                control.check_cancelled()?;
                if matches.len() == limits.max_matches {
                    stop_reason = Some(RepositoryQueryStopReason::Matches);
                    break;
                }
                matches.push(RepositoryQueryMatch {
                    path: relative,
                    line: 1,
                    column: 1,
                    bytes: Arc::from([]),
                    truncated: false,
                    content: empty,
                });
            }
            return Ok(RepositoryQueryResult {
                matches: matches.into(),
                files_considered,
                files_scanned: 0,
                bytes_scanned: 0,
                skipped_binary: 0,
                skipped_oversize: 0,
                entries_skipped,
                stop_reason,
            });
        }
        let mut matches = Vec::new();
        let mut files_scanned = 0_usize;
        let mut bytes_scanned = 0_u64;
        let mut skipped_binary = 0_usize;
        let mut skipped_oversize = 0_usize;
        let mut result_bytes = format!("matches={} incomplete=true\n", limits.max_matches).len();
        let pattern = RepositoryLiteralMatcher::new(query.literal(), query.match_case());
        let regex = query
            .regex
            .as_deref()
            .map(|pattern| compile_query_regex(pattern, query.match_case()))
            .transpose()?;
        'files: for (relative, reported_bytes, discovered, mut file) in
            files.into_iter().take(files_considered)
        {
            control.check_cancelled()?;
            if reported_bytes > limits.max_file_bytes {
                skipped_oversize = skipped_oversize.saturating_add(1);
                continue;
            }
            if reported_bytes > limits.max_total_bytes.saturating_sub(bytes_scanned) {
                stop_reason = Some(RepositoryQueryStopReason::ScannedBytes);
                break;
            }
            let opened_metadata = file
                .metadata()
                .map_err(|_| io_error("repository query file metadata failed"))?;
            let opened = FileObservation::capture(&opened_metadata);
            if opened != discovered {
                return Err(conflict_error("repository query file changed before open"));
            }
            if opened.length > limits.max_file_bytes {
                skipped_oversize = skipped_oversize.saturating_add(1);
                continue;
            }
            let retained_limit = limits.max_file_bytes.saturating_add(1);
            let mut bytes = Vec::with_capacity(usize::try_from(reported_bytes).unwrap_or(0));
            Read::by_ref(&mut file)
                .take(retained_limit)
                .read_to_end(&mut bytes)
                .map_err(|_| io_error("repository query file read failed"))?;
            let after = file
                .metadata()
                .map_err(|_| io_error("repository query file metadata failed"))?;
            if opened != FileObservation::capture(&after) {
                return Err(conflict_error(
                    "repository query file changed while reading",
                ));
            }
            let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if observed > limits.max_file_bytes {
                skipped_oversize = skipped_oversize.saturating_add(1);
                continue;
            }
            if observed > limits.max_total_bytes.saturating_sub(bytes_scanned) {
                stop_reason = Some(RepositoryQueryStopReason::ScannedBytes);
                break;
            }
            bytes_scanned = bytes_scanned.saturating_add(observed);
            files_scanned = files_scanned.saturating_add(1);
            if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                skipped_binary = skipped_binary.saturating_add(1);
                continue;
            }
            let content = WorkspaceContentIdentity::from_bytes(&bytes)?;
            let mut line_number = 1_u64;
            let mut line_start = 0_usize;
            while line_start <= bytes.len() {
                let line_end = bytes[line_start..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| line_start.saturating_add(offset));
                let line = &bytes[line_start..line_end];
                if let Some(regex) = regex.as_ref() {
                    // A regular expression reports a line once, as grep does.
                    if let Some(found) = regex.find(line) {
                        let column = found.start();
                        let retained =
                            utf8_prefix_len(line, line.len().min(limits.max_match_bytes));
                        let truncated = retained < line.len();
                        let row_bytes = repository_row_bytes(
                            &relative,
                            line_number,
                            column,
                            retained,
                            truncated,
                        );
                        if matches.len() == limits.max_matches
                            || row_bytes > limits.max_result_bytes.saturating_sub(result_bytes)
                        {
                            stop_reason = Some(if matches.len() == limits.max_matches {
                                RepositoryQueryStopReason::Matches
                            } else {
                                RepositoryQueryStopReason::ResultBytes
                            });
                            break 'files;
                        }
                        matches.push(RepositoryQueryMatch {
                            path: relative.clone(),
                            line: line_number,
                            column: u64::try_from(column.saturating_add(1)).unwrap_or(u64::MAX),
                            bytes: line[..retained].to_vec().into(),
                            truncated,
                            content,
                        });
                        result_bytes = result_bytes.saturating_add(row_bytes);
                    }
                    if line_end == bytes.len() {
                        break;
                    }
                    line_start = line_end.saturating_add(1);
                    line_number = line_number.saturating_add(1);
                    continue;
                }
                let mut prefix_len = 0_usize;
                for (index, byte) in line.iter().copied().enumerate() {
                    if index.is_multiple_of(65_536) {
                        control.check_cancelled()?;
                    }
                    while prefix_len > 0 && !pattern.matches(byte, prefix_len) {
                        prefix_len = pattern.failure[prefix_len - 1];
                    }
                    if pattern.matches(byte, prefix_len) {
                        prefix_len = prefix_len.saturating_add(1);
                    }
                    if prefix_len != pattern.needle.len() {
                        continue;
                    }
                    let column = index.saturating_add(1).saturating_sub(pattern.needle.len());
                    let retained_bound = line.len().min(limits.max_match_bytes);
                    let retained = utf8_prefix_len(line, retained_bound);
                    let truncated = retained < line.len();
                    let row_bytes =
                        repository_row_bytes(&relative, line_number, column, retained, truncated);
                    if matches.len() == limits.max_matches
                        || row_bytes > limits.max_result_bytes.saturating_sub(result_bytes)
                    {
                        stop_reason = Some(if matches.len() == limits.max_matches {
                            RepositoryQueryStopReason::Matches
                        } else {
                            RepositoryQueryStopReason::ResultBytes
                        });
                        break 'files;
                    }
                    matches.push(RepositoryQueryMatch {
                        path: relative.clone(),
                        line: line_number,
                        column: u64::try_from(column.saturating_add(1)).unwrap_or(u64::MAX),
                        bytes: line[..retained].to_vec().into(),
                        truncated,
                        content,
                    });
                    result_bytes = result_bytes.saturating_add(row_bytes);
                    prefix_len = 0;
                }
                if line_end == bytes.len() {
                    break;
                }
                line_start = line_end.saturating_add(1);
                line_number = line_number.saturating_add(1);
            }
        }
        Ok(RepositoryQueryResult {
            matches: matches.into(),
            files_considered,
            files_scanned,
            bytes_scanned,
            skipped_binary,
            skipped_oversize,
            entries_skipped,
            stop_reason,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (workspace, query, control);
        Err(backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "repository search is unsupported on this platform",
        ))
    }
}

/// Builds the result of a walk that surveyed nothing.
#[cfg(unix)]
fn empty_repository_query_result(entries_skipped: usize) -> RepositoryQueryResult {
    RepositoryQueryResult {
        matches: Arc::from([]),
        files_considered: 0,
        files_scanned: 0,
        bytes_scanned: 0,
        skipped_binary: 0,
        skipped_oversize: 0,
        entries_skipped,
        stop_reason: None,
    }
}

/// Returns the literal leading directory prefix every path matching `pattern`
/// must have, when the pattern has one.
///
/// A pattern with no `/` also matches a bare file name at any depth, so it
/// never yields a prefix. Brace alternatives contribute only the prefix they
/// all share.
#[cfg(unix)]
fn glob_walk_prefix(pattern: &str) -> Option<PathBuf> {
    let mut expansions = expand_glob_braces(pattern).into_iter();
    let mut prefix = literal_directory_prefix(&expansions.next()?);
    for expansion in expansions {
        prefix = shared_path_prefix(&prefix, &literal_directory_prefix(&expansion));
    }
    (!prefix.as_os_str().is_empty()).then_some(prefix)
}

/// Returns the longest leading run of components two paths share.
#[cfg(unix)]
fn shared_path_prefix(left: &Path, right: &Path) -> PathBuf {
    left.components()
        .zip(right.components())
        .take_while(|(left, right)| left == right)
        .map(|(component, _)| component)
        .collect()
}

/// Returns the leading wildcard-free directory components of one glob.
#[cfg(unix)]
fn literal_directory_prefix(pattern: &str) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut components = pattern.split('/').peekable();
    while let Some(component) = components.next() {
        // The last component names files, never a directory to descend into.
        if components.peek().is_none()
            || component.is_empty()
            || component == "."
            || component == ".."
            || component.starts_with('.')
            || component.contains(['*', '?', '[', ']', '{', '}'])
        {
            break;
        }
        prefix.push(component);
    }
    prefix
}

/// One compiled path glob. A pattern without `/` matches the file name at any
/// depth, as ripgrep's `-g` does; a pattern with `/` matches the path relative
/// to the query root. `*` and `?` stay within one component, `**` crosses
/// components, and `[...]` classes and `{a,b}` alternatives are supported.
#[cfg(unix)]
struct PathGlob {
    matcher: globset::GlobMatcher,
    name_only: bool,
}

#[cfg(unix)]
impl PathGlob {
    fn new(pattern: &str) -> Result<Self, ExecutionBackendError> {
        let name_only = !pattern.contains('/');
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|_| {
                backend_error(
                    ExecutionBackendErrorKind::InvalidRequest,
                    "repository glob pattern is invalid",
                )
            })?
            .compile_matcher();
        Ok(Self { matcher, name_only })
    }

    fn matches(&self, path: &Path) -> bool {
        if self.name_only {
            return path
                .file_name()
                .is_some_and(|name| self.matcher.is_match(Path::new(name)));
        }
        self.matcher.is_match(path)
    }
}

/// The `.gitignore`-style rules in force in one directory: every ancestor's
/// rules, deepest last.
#[cfg(unix)]
type IgnoreChain = Arc<[Arc<ignore::gitignore::Gitignore>]>;

/// Whether the deepest rule that speaks about `path` ignores it. A deeper
/// `!pattern` re-includes what a shallower rule excluded.
#[cfg(unix)]
fn is_ignored(chain: &IgnoreChain, path: &Path, is_directory: bool) -> bool {
    for rules in chain.iter().rev() {
        match rules.matched(path, is_directory) {
            ignore::Match::Ignore(_) => return true,
            ignore::Match::Whitelist(_) => return false,
            ignore::Match::None => {}
        }
    }
    false
}

/// Builds one directory's rules from its `.gitignore` and `.ignore`, read
/// through the already-open directory descriptor without following links.
/// The ignore crate is only a matcher here; the walk stays descriptor-based.
#[cfg(unix)]
fn directory_ignores(directory: &fs::File, relative: &Path, parent: &IgnoreChain) -> IgnoreChain {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(relative);
    let mut any = false;
    for name in [".gitignore", ".ignore"] {
        any |= add_ignore_file(&mut builder, directory, Path::new(name));
    }
    extend_chain(parent, any.then(|| builder.build().ok()).flatten())
}

/// The rules of every directory from the workspace root down to `walk_root`,
/// plus the repository's `.git/info/exclude`, so a search that starts in a
/// subdirectory still honours the root `.gitignore`.
#[cfg(unix)]
fn ancestor_ignores(workspace: &ExecutionWorkspace, walk_root: &Path) -> IgnoreChain {
    let mut chain: IgnoreChain = Arc::from([]);
    let open_directory = |relative: &Path| {
        open_workspace_beneath(workspace, relative, OFlags::RDONLY | OFlags::DIRECTORY).ok()
    };
    if let Some(git_info) = open_directory(Path::new(".git/info")) {
        let mut builder = ignore::gitignore::GitignoreBuilder::new("");
        if add_ignore_file(&mut builder, &git_info, Path::new("exclude")) {
            chain = extend_chain(&chain, builder.build().ok());
        }
    }
    let mut ancestor = PathBuf::new();
    let components = walk_root
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .collect::<Vec<_>>();
    for component in &components {
        let relative = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor.as_path()
        };
        if let Some(directory) = open_directory(relative) {
            chain = directory_ignores(&directory, &ancestor, &chain);
        }
        ancestor.push(component);
    }
    chain
}

#[cfg(unix)]
fn extend_chain(parent: &IgnoreChain, rules: Option<ignore::gitignore::Gitignore>) -> IgnoreChain {
    match rules.filter(|rules| !rules.is_empty()) {
        Some(rules) => parent
            .iter()
            .cloned()
            .chain(std::iter::once(Arc::new(rules)))
            .collect(),
        None => Arc::clone(parent),
    }
}

/// Adds one ignore file's lines; a missing, linked, unreadable or oversized
/// file contributes nothing.
#[cfg(unix)]
fn add_ignore_file(
    builder: &mut ignore::gitignore::GitignoreBuilder,
    directory: &fs::File,
    name: &Path,
) -> bool {
    const MAX_IGNORE_FILE_BYTES: u64 = 1_048_576;
    let Ok(descriptor) = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) else {
        return false;
    };
    let mut file = fs::File::from(descriptor);
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= MAX_IGNORE_FILE_BYTES)
    {
        return false;
    }
    let mut text = String::new();
    if Read::by_ref(&mut file)
        .take(MAX_IGNORE_FILE_BYTES)
        .read_to_string(&mut text)
        .is_err()
    {
        return false;
    }
    let mut added = false;
    for line in text.lines() {
        added |= builder.add_line(None, line).is_ok();
    }
    added
}

/// Bytes one reported match row costs in the model-visible result.
#[cfg(unix)]
fn repository_row_bytes(
    relative: &Path,
    line_number: u64,
    column: usize,
    retained: usize,
    truncated: bool,
) -> usize {
    relative
        .to_string_lossy()
        .len()
        .saturating_add(decimal_len(line_number))
        .saturating_add(decimal_len(
            u64::try_from(column.saturating_add(1)).unwrap_or(u64::MAX),
        ))
        .saturating_add(retained)
        .saturating_add(4)
        .saturating_add(if truncated { " …".len() } else { 0 })
}

#[cfg(unix)]
fn expand_glob_braces(pattern: &str) -> Vec<String> {
    let Some(start) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(end_rel) = pattern[start + 1..].find('}') else {
        return vec![pattern.to_owned()];
    };
    let end = start + 1 + end_rel;
    let inner = &pattern[start + 1..end];
    if inner.is_empty() || inner.contains('{') || inner.contains('/') {
        return vec![pattern.to_owned()];
    }
    let prefix = &pattern[..start];
    let suffix = &pattern[end + 1..];
    inner
        .split(',')
        .map(|alt| format!("{prefix}{alt}{suffix}"))
        .collect()
}

#[cfg(unix)]
struct RepositoryLiteralMatcher<'a> {
    needle: &'a [u8],
    match_case: RepositoryMatchCase,
    failure: Vec<usize>,
}

#[cfg(unix)]
impl<'a> RepositoryLiteralMatcher<'a> {
    fn new(needle: &'a [u8], match_case: RepositoryMatchCase) -> Self {
        let mut failure = vec![0; needle.len()];
        let mut matched = 0_usize;
        for index in 1..needle.len() {
            while matched > 0 && !literal_byte_eq(needle[index], needle[matched], match_case) {
                matched = failure[matched - 1];
            }
            if literal_byte_eq(needle[index], needle[matched], match_case) {
                matched = matched.saturating_add(1);
            }
            failure[index] = matched;
        }
        Self {
            needle,
            match_case,
            failure,
        }
    }

    fn matches(&self, byte: u8, needle_index: usize) -> bool {
        literal_byte_eq(byte, self.needle[needle_index], self.match_case)
    }
}

#[cfg(unix)]
fn literal_byte_eq(left: u8, right: u8, match_case: RepositoryMatchCase) -> bool {
    match match_case {
        RepositoryMatchCase::Sensitive => left == right,
        RepositoryMatchCase::AsciiInsensitive => left.eq_ignore_ascii_case(&right),
    }
}

#[cfg(unix)]
fn utf8_prefix_len(bytes: &[u8], maximum: usize) -> usize {
    let mut retained = maximum.min(bytes.len());
    while retained > 0 && std::str::from_utf8(&bytes[..retained]).is_err() {
        retained = retained.saturating_sub(1);
    }
    retained
}

#[cfg(unix)]
fn decimal_len(value: u64) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

#[cfg(unix)]
struct PreflightMutation {
    relative: PathBuf,
    disposition: WorkspaceMutationDisposition,
    // Declared before `created_parents`: an unpublished temporary file is
    // removed before the directories it sits in.
    temporary: Option<MutationTemporaryFile>,
    created_parents: Option<CreatedParents>,
}

#[cfg(not(unix))]
pub(super) fn mutate_workspace_sync(
    _workspace: &ExecutionWorkspace,
    _transaction: &WorkspaceMutationTransaction,
    _control: &ExecutionOperationControl,
) -> Result<WorkspaceMutationTransactionResult, ExecutionBackendError> {
    Err(backend_error(
        ExecutionBackendErrorKind::Unsupported,
        "workspace mutation is unsupported on this platform",
    ))
}

#[cfg(unix)]
pub(super) fn mutate_workspace_sync(
    workspace: &ExecutionWorkspace,
    transaction: &WorkspaceMutationTransaction,
    control: &ExecutionOperationControl,
) -> Result<WorkspaceMutationTransactionResult, ExecutionBackendError> {
    control.check_cancelled()?;
    let mut preflight = Vec::with_capacity(transaction.mutations().len());
    for item in transaction.mutations() {
        control.check_cancelled()?;
        let mut observed = observe_workspace_file(workspace, item.path())?;
        let metadata = observed
            .as_ref()
            .map(fs::File::metadata)
            .transpose()
            .map_err(|_| io_error("workspace mutation metadata failed"))?;
        verify_expected_file_state(observed.as_mut(), item.expected(), control)?;
        let disposition =
            mutation_disposition(item.expected(), item.replacement(), observed.is_some())?;
        let created_parents = if disposition == WorkspaceMutationDisposition::Created {
            Some(create_missing_parents(workspace, item.path())?)
        } else {
            None
        };
        let temporary = match (disposition, item.replacement()) {
            (
                WorkspaceMutationDisposition::Created | WorkspaceMutationDisposition::Updated,
                WorkspaceReplacement::Content(bytes),
            ) => Some(write_temporary_file(
                workspace,
                item.path(),
                bytes,
                metadata.as_ref().map(fs::Metadata::permissions).as_ref(),
            )?),
            (WorkspaceMutationDisposition::Created | WorkspaceMutationDisposition::Updated, _) => {
                return Err(io_error("workspace mutation preparation was inconsistent"));
            }
            _ => None,
        };
        preflight.push(PreflightMutation {
            relative: item.path.clone(),
            disposition,
            temporary,
            created_parents,
        });
    }

    // Revalidate every precondition after all fallible staging and before the
    // first external publication. A conflict here is therefore zero-effect.
    for (item, _prepared) in transaction.mutations().iter().zip(&preflight) {
        control.check_cancelled()?;
        let mut observed = observe_workspace_file(workspace, item.path())?;
        verify_expected_file_state(observed.as_mut(), item.expected(), control)?;
    }

    // This is deliberately the last cancellation point. Returning Cancelled
    // after publication started would misrepresent a possibly committed effect.
    control.check_cancelled()?;
    let mut results = Vec::with_capacity(transaction.mutations().len());
    for (item, mut prepared) in transaction.mutations().iter().zip(preflight) {
        match prepared.disposition {
            WorkspaceMutationDisposition::Created => {
                let Some(temporary) = prepared.temporary.take() else {
                    return Err(io_error("workspace mutation preparation was inconsistent"));
                };
                publish_created_file(temporary)?;
                if let Some(parents) = prepared.created_parents.as_mut() {
                    parents.keep();
                }
            }
            WorkspaceMutationDisposition::Updated => {
                let Some(temporary) = prepared.temporary.take() else {
                    return Err(io_error("workspace mutation preparation was inconsistent"));
                };
                publish_replacement_file(temporary)?;
            }
            WorkspaceMutationDisposition::Deleted => {
                let (parent, name) =
                    open_workspace_parent(workspace, &prepared.relative).map_err(|_| {
                        recovery_required_error("workspace mutation recovery is required")
                    })?;
                unlinkat(&parent, &name, AtFlags::empty()).map_err(|_| {
                    recovery_required_error("workspace mutation delete publication is ambiguous")
                })?;
                parent.sync_all().map_err(|_| {
                    recovery_required_error("workspace mutation delete durability is ambiguous")
                })?;
            }
            WorkspaceMutationDisposition::Unchanged => {}
        }
        results.push(WorkspaceMutationResultItem {
            path: item.path.clone(),
            disposition: prepared.disposition,
        });
    }
    Ok(WorkspaceMutationTransactionResult {
        items: results.into(),
    })
}

#[cfg(unix)]
fn mutation_disposition(
    expected: WorkspaceExpectedFileState,
    replacement: &WorkspaceReplacement,
    existed: bool,
) -> Result<WorkspaceMutationDisposition, ExecutionBackendError> {
    match (expected, replacement) {
        (WorkspaceExpectedFileState::Missing, WorkspaceReplacement::Missing) => {
            Ok(WorkspaceMutationDisposition::Unchanged)
        }
        (WorkspaceExpectedFileState::Missing, WorkspaceReplacement::Content(_)) => {
            Ok(WorkspaceMutationDisposition::Created)
        }
        (WorkspaceExpectedFileState::Any, WorkspaceReplacement::Content(_)) => Ok(if existed {
            WorkspaceMutationDisposition::Updated
        } else {
            WorkspaceMutationDisposition::Created
        }),
        (
            WorkspaceExpectedFileState::Content(expected),
            WorkspaceReplacement::Content(replacement),
        ) => {
            if WorkspaceContentIdentity::from_bytes(replacement)? == expected {
                Ok(WorkspaceMutationDisposition::Unchanged)
            } else {
                Ok(WorkspaceMutationDisposition::Updated)
            }
        }
        (WorkspaceExpectedFileState::Content(_), WorkspaceReplacement::Missing) => {
            Ok(WorkspaceMutationDisposition::Deleted)
        }
        (WorkspaceExpectedFileState::Any, WorkspaceReplacement::Missing) => {
            Err(io_error("workspace mutation preparation was inconsistent"))
        }
    }
}

#[cfg(unix)]
fn verify_expected_file_state(
    file: Option<&mut fs::File>,
    expected: WorkspaceExpectedFileState,
    control: &ExecutionOperationControl,
) -> Result<(), ExecutionBackendError> {
    match (file, expected) {
        (None, WorkspaceExpectedFileState::Missing | WorkspaceExpectedFileState::Any)
        | (Some(_), WorkspaceExpectedFileState::Any) => Ok(()),
        (Some(_), WorkspaceExpectedFileState::Missing)
        | (None, WorkspaceExpectedFileState::Content(_)) => Err(conflict_error(
            "workspace mutation expected state did not match",
        )),
        (Some(file), WorkspaceExpectedFileState::Content(expected_content)) => {
            let metadata = file
                .metadata()
                .map_err(|_| io_error("workspace mutation preflight metadata failed"))?;
            if metadata.len() != expected_content.byte_len() {
                return Err(conflict_error(
                    "workspace mutation expected content did not match",
                ));
            }
            let mut reader = CancellationReader::new(file, control);
            let digest = Digest::sha256_reader(
                DigestDomain::Blob,
                WORKSPACE_CONTENT_IDENTITY_REVISION,
                expected_content.byte_len(),
                &mut reader,
            )
            .map_err(|_| {
                if reader.cancelled {
                    backend_error(
                        ExecutionBackendErrorKind::Cancelled,
                        "execution environment operation was cancelled",
                    )
                } else {
                    conflict_error("workspace mutation expected content did not match")
                }
            })?;
            if WorkspaceContentIdentity::new(expected_content.byte_len(), digest)?
                == expected_content
            {
                Ok(())
            } else {
                Err(conflict_error(
                    "workspace mutation expected content did not match",
                ))
            }
        }
    }
}

#[cfg(unix)]
struct CancellationReader<'a> {
    inner: &'a mut fs::File,
    control: &'a ExecutionOperationControl,
    cancelled: bool,
}

#[cfg(unix)]
impl<'a> CancellationReader<'a> {
    fn new(inner: &'a mut fs::File, control: &'a ExecutionOperationControl) -> Self {
        Self {
            inner,
            control,
            cancelled: false,
        }
    }
}

#[cfg(unix)]
impl Read for CancellationReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.control.cancellation().is_cancelled() {
            self.cancelled = true;
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "workspace mutation preflight cancelled",
            ))
        } else {
            self.inner.read(buffer)
        }
    }
}

#[cfg(unix)]
fn observe_workspace_file(
    workspace: &ExecutionWorkspace,
    relative: &Path,
) -> Result<Option<fs::File>, ExecutionBackendError> {
    let root = verified_workspace_root(workspace)?;
    let path = workspace_descriptor_path(workspace, relative)?;
    #[cfg(target_os = "linux")]
    {
        match openat2(
            &root,
            &path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(descriptor) => {
                let file = fs::File::from(descriptor);
                if file
                    .metadata()
                    .map_err(|_| io_error("workspace mutation metadata failed"))?
                    .is_file()
                {
                    Ok(Some(file))
                } else {
                    Err(workspace_error(
                        "workspace mutation target is not a regular file",
                    ))
                }
            }
            Err(Errno::NOENT) => Ok(None),
            Err(Errno::LOOP | Errno::XDEV) => Err(backend_error(
                ExecutionBackendErrorKind::SymlinkEscape,
                "workspace mutation refuses symbolic links",
            )),
            Err(_) => Err(io_error("workspace mutation target open failed")),
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        observe_open_workspace_file_at(&root, &path)
    }
}

#[cfg(unix)]
fn open_workspace_parent(
    workspace: &ExecutionWorkspace,
    relative: &Path,
) -> Result<(fs::File, PathBuf), ExecutionBackendError> {
    validate_relative_path(relative, false)?;
    let target_name = relative
        .file_name()
        .ok_or_else(|| workspace_error("workspace mutation target has no name"))?
        .into();
    let parent = relative.parent().unwrap_or_else(|| Path::new("."));
    let descriptor = open_workspace_beneath(workspace, parent, OFlags::RDONLY | OFlags::DIRECTORY)?;
    Ok((descriptor, target_name))
}

/// Directories a create made for its target, removed again unless the file
/// they hold was published.
#[cfg(unix)]
struct CreatedParents {
    created: Vec<(fs::File, PathBuf)>,
}

#[cfg(unix)]
impl CreatedParents {
    fn keep(&mut self) {
        self.created.clear();
    }
}

#[cfg(unix)]
impl Drop for CreatedParents {
    fn drop(&mut self) {
        for (parent, name) in self.created.drain(..).rev() {
            let _ = unlinkat(&parent, &name, AtFlags::REMOVEDIR);
            let _ = parent.sync_all();
        }
    }
}

/// Creates each missing directory above `relative`, one component at a time
/// beneath the verified root. Every component is opened `O_NOFOLLOW |
/// O_DIRECTORY`, so an existing symbolic link or non-directory refuses the
/// create instead of being followed.
#[cfg(unix)]
fn create_missing_parents(
    workspace: &ExecutionWorkspace,
    relative: &Path,
) -> Result<CreatedParents, ExecutionBackendError> {
    validate_relative_path(relative, false)?;
    let mut created = CreatedParents {
        created: Vec::new(),
    };
    let Some(parent) = relative.parent() else {
        return Ok(created);
    };
    let root = verified_workspace_root(workspace)?;
    let descriptor_parent = workspace_descriptor_path(workspace, parent)?;
    let names = workspace_path_components(&descriptor_parent)?;
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut current = fs::File::from(
        openat(&root, ".", directory_flags, Mode::empty())
            .map_err(|errno| workspace_open_error(parent, errno))?,
    );
    for name in names {
        match openat(&current, name, directory_flags, Mode::empty()) {
            Ok(descriptor) => current = fs::File::from(descriptor),
            Err(Errno::NOENT) => {
                match mkdirat(&current, name, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(errno) => return Err(workspace_open_error(parent, errno)),
                }
                current
                    .sync_all()
                    .map_err(|_| io_error("workspace mutation parent sync failed"))?;
                let next = fs::File::from(
                    openat(&current, name, directory_flags, Mode::empty())
                        .map_err(|errno| workspace_open_error(parent, errno))?,
                );
                created.created.push((current, PathBuf::from(name)));
                current = next;
            }
            Err(errno) => return Err(workspace_open_error(parent, errno)),
        }
    }
    Ok(created)
}

#[cfg(unix)]
static NEXT_MUTATION_TEMP: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
struct MutationTemporaryFile {
    parent: fs::File,
    name: PathBuf,
    target_name: PathBuf,
    published: bool,
}

#[cfg(unix)]
impl MutationTemporaryFile {
    fn create(
        workspace: &ExecutionWorkspace,
        target: &Path,
    ) -> Result<(Self, fs::File), ExecutionBackendError> {
        let (parent, target_name) = open_workspace_parent(workspace, target)?;
        for _ in 0..32 {
            let sequence = NEXT_MUTATION_TEMP.fetch_add(1, Ordering::Relaxed);
            let name = PathBuf::from(format!(".plak-mutation-{}-{sequence}", std::process::id()));
            match openat(
                &parent,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(descriptor) => {
                    return Ok((
                        Self {
                            parent,
                            name,
                            target_name,
                            published: false,
                        },
                        fs::File::from(descriptor),
                    ));
                }
                Err(Errno::EXIST) => {}
                Err(_) => return Err(io_error("workspace mutation temporary file create failed")),
            }
        }
        Err(io_error(
            "workspace mutation could not allocate a temporary file",
        ))
    }
}

#[cfg(unix)]
impl Drop for MutationTemporaryFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = unlinkat(&self.parent, &self.name, AtFlags::empty());
            let _ = self.parent.sync_all();
        }
    }
}

#[cfg(unix)]
fn write_temporary_file(
    workspace: &ExecutionWorkspace,
    target: &Path,
    bytes: &[u8],
    permissions: Option<&fs::Permissions>,
) -> Result<MutationTemporaryFile, ExecutionBackendError> {
    let (temporary, mut file) = MutationTemporaryFile::create(workspace, target)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions.clone())
            .map_err(|_| io_error("workspace mutation permissions copy failed"))?;
    }
    file.write_all(bytes)
        .map_err(|_| io_error("workspace mutation temporary file write failed"))?;
    file.sync_all()
        .map_err(|_| io_error("workspace mutation temporary file sync failed"))?;
    drop(file);
    Ok(temporary)
}

#[cfg(unix)]
fn publish_created_file(mut temporary: MutationTemporaryFile) -> Result<(), ExecutionBackendError> {
    renameat_with(
        &temporary.parent,
        &temporary.name,
        &temporary.parent,
        &temporary.target_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| recovery_required_error("workspace mutation create publication is ambiguous"))?;
    temporary.published = true;
    temporary.parent.sync_all().map_err(|_| {
        recovery_required_error("workspace mutation create durability is ambiguous")
    })?;
    Ok(())
}

#[cfg(unix)]
fn publish_replacement_file(
    mut temporary: MutationTemporaryFile,
) -> Result<(), ExecutionBackendError> {
    renameat_with(
        &temporary.parent,
        &temporary.name,
        &temporary.parent,
        &temporary.target_name,
        RenameFlags::empty(),
    )
    .map_err(|_| recovery_required_error("workspace mutation replacement is ambiguous"))?;
    temporary.published = true;
    temporary.parent.sync_all().map_err(|_| {
        recovery_required_error("workspace mutation replacement durability is ambiguous")
    })?;
    Ok(())
}

// Only process-cwd preflight calls this helper, before any child is spawned.
// Metadata and canonicalization failures are therefore no-effect path refusals;
// reusing this mapping after dispatch would incorrectly erase uncertainty.
#[cfg(unix)]
/// The refusal one component of a resolved path earns from its own errno.
///
/// A path that is not there is not a policy decision, and a component that is
/// not a directory is why the rest of the path cannot exist. Both are the same
/// answer to the caller: choose an existing directory. Anything else is the
/// filesystem refusing for a reason of its own and stays an I/O failure.
fn resolve_component_error(path: &Path, error: &std::io::Error) -> ExecutionBackendError {
    let path = path.display();
    let refusal = match error.kind() {
        std::io::ErrorKind::NotFound => backend_error(
            ExecutionBackendErrorKind::NotFound,
            format!("path does not exist: {path}"),
        ),
        std::io::ErrorKind::NotADirectory => backend_error(
            ExecutionBackendErrorKind::NotFound,
            format!("path does not exist; a parent component is not a directory: {path}"),
        ),
        _ => backend_error(
            ExecutionBackendErrorKind::Io,
            format!("workspace path could not be resolved: {path}"),
        ),
    };
    refusal.with_proven_no_effect(true)
}

pub(super) fn resolve_existing_no_symlink(
    workspace: &ExecutionWorkspace,
    relative: &Path,
) -> Result<PathBuf, ExecutionBackendError> {
    validate_relative_path(relative, true)?;
    let candidate = workspace.cwd().join(relative);
    let beneath_root = candidate
        .strip_prefix(workspace.root())
        .map_err(|_| workspace_error("workspace path escapes the root"))?;
    let mut current = workspace.root().to_path_buf();
    for component in beneath_root.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            // The message always said "choose an existing directory"; now the
            // kind says it too, instead of reporting a typo as a policy
            // refusal. Nothing was opened or written on the way here.
            resolve_component_error(&current, &error)
        })?;
        if metadata.file_type().is_symlink() {
            return Err(backend_error(
                ExecutionBackendErrorKind::SymlinkEscape,
                format!("path traverses a symbolic link: {}", current.display()),
            )
            .with_proven_no_effect(true));
        }
    }
    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| resolve_component_error(&candidate, &error))?;
    if !canonical.starts_with(workspace.root()) {
        return Err(backend_error(
            ExecutionBackendErrorKind::OutsideWorkspace,
            format!(
                "path resolves outside the workspace: {}",
                relative.display()
            ),
        )
        .with_proven_no_effect(true));
    }
    Ok(canonical)
}

#[cfg(unix)]
const MAX_BLOCKING_FILESYSTEM_OPERATIONS: usize = 8;

#[cfg(unix)]
static FILESYSTEM_BLOCKING_LANES: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_BLOCKING_FILESYSTEM_OPERATIONS)));

pub(super) fn run_filesystem_blocking<T, F>(operation: F) -> ExecutionBackendFuture<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ExecutionBackendError> + Send + 'static,
{
    #[cfg(unix)]
    {
        Box::pin(async move {
            let permit = Arc::clone(&FILESYSTEM_BLOCKING_LANES)
                .acquire_owned()
                .await
                .map_err(|_| io_error("filesystem blocking lane closed"))?;
            let result = tokio::task::spawn_blocking(operation)
                .await
                .map_err(|_| io_error("filesystem blocking operation failed to join"))?;
            drop(permit);
            result
        })
    }
    #[cfg(not(unix))]
    {
        let _ = operation;
        ready_error(backend_error(
            ExecutionBackendErrorKind::Unsupported,
            "filesystem operations are unsupported on this platform",
        ))
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod query_walk_tests {
    use super::{
        Dir, PathBuf, RepositoryQueryEntry, glob_walk_prefix, literal_directory_prefix,
        open_directory_entry_for_query, shared_path_prefix,
    };
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    /// The walk lists a directory and only then opens each child. An entry
    /// that disappears inside that window vanished; it was never unopenable.
    #[test]
    fn a_child_removed_after_the_listing_vanishes_instead_of_being_skipped() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("gone.txt"), b"x").expect("write child");
        let handle = fs::File::open(directory.path()).expect("open directory");
        let listed = Dir::read_from(&handle)
            .expect("read directory")
            .flatten()
            .map(|entry| PathBuf::from(std::ffi::OsStr::from_bytes(entry.file_name().to_bytes())))
            .collect::<Vec<_>>();
        assert!(listed.contains(&PathBuf::from("gone.txt")));
        fs::remove_file(directory.path().join("gone.txt")).expect("remove child");
        assert!(matches!(
            open_directory_entry_for_query(&handle, Path::new("gone.txt")),
            RepositoryQueryEntry::Vanished
        ));
    }

    /// The walk never follows a symbolic link, whatever it points at.
    #[test]
    fn a_symbolic_link_is_refused_rather_than_opened() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("real.txt"), b"x").expect("write child");
        std::os::unix::fs::symlink("real.txt", directory.path().join("link.txt"))
            .expect("create symlink");
        let handle = fs::File::open(directory.path()).expect("open directory");
        assert!(matches!(
            open_directory_entry_for_query(&handle, Path::new("link.txt")),
            RepositoryQueryEntry::Symlink
        ));
    }

    #[test]
    fn a_glob_prefix_is_its_leading_wildcard_free_directories() {
        assert_eq!(
            glob_walk_prefix("docs/spec/*.md"),
            Some(PathBuf::from("docs/spec"))
        );
        assert_eq!(glob_walk_prefix("docs/**"), Some(PathBuf::from("docs")));
        assert_eq!(glob_walk_prefix("src/lib.rs"), Some(PathBuf::from("src")));
        assert_eq!(
            glob_walk_prefix("docs/{spec,guide}/*.md"),
            Some(PathBuf::from("docs"))
        );
        // A pattern without `/` also matches a bare file name at any depth.
        assert_eq!(glob_walk_prefix("*.md"), None);
        assert_eq!(glob_walk_prefix("**/*.rs"), None);
        assert_eq!(glob_walk_prefix("/etc/*.conf"), None);
        assert_eq!(glob_walk_prefix("../outside/*.md"), None);
        assert_eq!(glob_walk_prefix(".git/*"), None);
        // Brace alternatives contribute only what they all share.
        assert_eq!(glob_walk_prefix("{docs,src}/a/*.md"), None);
        assert_eq!(
            glob_walk_prefix("docs/spec/{a,b}/*.md"),
            Some(PathBuf::from("docs/spec"))
        );
        assert_eq!(literal_directory_prefix("a.md"), PathBuf::new());
        assert_eq!(
            shared_path_prefix(Path::new("a/b/c"), Path::new("a/b/d")),
            PathBuf::from("a/b")
        );
    }
}
