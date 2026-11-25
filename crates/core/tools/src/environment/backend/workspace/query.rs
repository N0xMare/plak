//! Repository query walk, glob retain, path list, and file scan.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::Dir;

use super::super::text_search::{
    PathGlob, RepositoryLiteralMatcher, repository_row_bytes, utf8_prefix_len,
};
use super::super::workspace_types::compile_query_regex;
use super::super::{RepositoryQueryLimits, RepositoryQueryMatch, validate_path_bytes};
use super::{
    ExecutionBackendError, ExecutionWorkspace, FileObservation, IgnoreChain, OperationContext,
    QueryFileRead, RepositoryQuery, RepositoryQueryEntry, RepositoryQueryResult,
    RepositoryQueryStopReason, WorkspaceContentIdentity, check_workspace_control,
    directory_ignores, is_ignored, open_directory_entry_for_query, open_query_entry_beneath,
    query_entry_kind_is_walkable, read_unchanged_query_file,
};

pub(super) struct DiscoveredRepositoryFile {
    pub(super) path: PathBuf,
    pub(super) is_directory: bool,
    pub(super) size: u64,
    pub(super) observation: FileObservation,
}

pub(super) struct RepositoryQueryWalk {
    pub(super) files: Vec<DiscoveredRepositoryFile>,
    pub(super) entries_skipped: usize,
    pub(super) stop_reason: Option<RepositoryQueryStopReason>,
}

fn matches_query_glob(query: &RepositoryQuery, matcher: &PathGlob, relative: &Path) -> bool {
    // A file root is matched by its own path; anything beneath a
    // directory root by its path relative to that root.
    let within = relative
        .strip_prefix(query.root())
        .ok()
        .filter(|within| !within.as_os_str().is_empty())
        .unwrap_or(relative);
    matcher.matches(within)
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal workspace query walk passes borrowed root/control state explicitly"
)]
pub(super) fn walk_repository_query_directories(
    workspace: &ExecutionWorkspace,
    root: &fs::File,
    control: &OperationContext,
    limits: RepositoryQueryLimits,
    mut pending: BTreeMap<PathBuf, IgnoreChain>,
    mut first_directory: Option<(PathBuf, fs::File)>,
    mut files: Vec<DiscoveredRepositoryFile>,
    query: &RepositoryQuery,
    glob_matcher: Option<&PathGlob>,
) -> Result<RepositoryQueryWalk, ExecutionBackendError> {
    let mut entries_skipped = 0_usize;
    let mut stop_reason = None;
    let mut tree_entries = 0_usize;
    while let Some((directory_path, parent_ignores)) = pending.pop_first() {
        check_workspace_control(control)?;
        let directory = if let Some((first_path, first_handle)) = first_directory.take() {
            if first_path == directory_path {
                first_handle
            } else {
                first_directory = Some((first_path, first_handle));
                match open_query_entry_beneath(root, workspace, &directory_path, true) {
                    RepositoryQueryEntry::Opened(dir) => dir,
                    RepositoryQueryEntry::Vanished | RepositoryQueryEntry::Symlink => continue,
                    RepositoryQueryEntry::Skipped => {
                        entries_skipped = entries_skipped.saturating_add(1);
                        continue;
                    }
                }
            }
        } else {
            match open_query_entry_beneath(root, workspace, &directory_path, true) {
                RepositoryQueryEntry::Opened(dir) => dir,
                RepositoryQueryEntry::Vanished | RepositoryQueryEntry::Symlink => continue,
                RepositoryQueryEntry::Skipped => {
                    entries_skipped = entries_skipped.saturating_add(1);
                    continue;
                }
            }
        };
        let ignores = directory_ignores(&directory, &directory_path, &parent_ignores);
        let remaining = limits.max_tree_entries.saturating_sub(tree_entries);
        let mut children = Vec::with_capacity(remaining.min(256));
        {
            let Ok(mut entries) = Dir::read_from(&directory) else {
                entries_skipped = entries_skipped.saturating_add(1);
                continue;
            };
            for entry in &mut entries {
                check_workspace_control(control)?;
                if tree_entries == limits.max_tree_entries {
                    stop_reason = Some(RepositoryQueryStopReason::TreeEntries);
                    break;
                }
                let Ok(entry) = entry else {
                    tree_entries = tree_entries.saturating_add(1);
                    entries_skipped = entries_skipped.saturating_add(1);
                    continue;
                };
                let name = entry.file_name().to_bytes();
                if matches!(name, b"." | b"..") {
                    continue;
                }
                tree_entries = tree_entries.saturating_add(1);
                // Version-control metadata is never repository content. Other
                // dot entries (`.github`, `.cargo`) are walked unless ignored.
                if name.eq_ignore_ascii_case(b".git") || name.eq_ignore_ascii_case(b".jj") {
                    continue;
                }
                let file_type = entry.file_type();
                if query_entry_kind_is_walkable(file_type) == Some(false) {
                    entries_skipped = entries_skipped.saturating_add(1);
                    continue;
                }
                let child_name = PathBuf::from(std::ffi::OsStr::from_bytes(name));
                let is_directory = file_type == rustix::fs::FileType::Directory;
                if is_ignored(&ignores, &directory_path.join(&child_name), is_directory) {
                    continue;
                }
                children.push((child_name, file_type));
            }
        }
        if stop_reason.is_some() {
            break;
        }
        children.sort_by(|left, right| left.0.cmp(&right.0));
        for (child_name, file_type) in children {
            check_workspace_control(control)?;
            let relative = directory_path.join(&child_name);
            if validate_path_bytes(&relative).is_err() {
                entries_skipped = entries_skipped.saturating_add(1);
                continue;
            }
            if file_type == rustix::fs::FileType::Symlink {
                continue;
            }
            // Skip opening regular files whose path cannot match the glob.
            // Directories and unknown-type entries are still opened so nested
            // matches are never missed.
            if file_type == rustix::fs::FileType::RegularFile
                && glob_matcher
                    .is_some_and(|matcher| !matches_query_glob(query, matcher, &relative))
            {
                continue;
            }
            let Some(metadata) = inspect_query_child(&directory, &child_name, &mut entries_skipped)
            else {
                continue;
            };
            if metadata.is_dir() {
                if let Some(depth) = query.list_depth {
                    if glob_matcher
                        .is_none_or(|matcher| matches_query_glob(query, matcher, &relative))
                    {
                        files.push(DiscoveredRepositoryFile {
                            path: relative.clone(),
                            is_directory: true,
                            size: 0,
                            observation: FileObservation::capture(&metadata),
                        });
                    }
                    let level = relative
                        .strip_prefix(query.root())
                        .map_or(usize::MAX, |path| path.components().count());
                    if level >= depth {
                        continue;
                    }
                }
                pending.insert(relative, Arc::clone(&ignores));
            } else if metadata.is_file() {
                if glob_matcher.is_none_or(|matcher| matches_query_glob(query, matcher, &relative))
                {
                    files.push(DiscoveredRepositoryFile {
                        path: relative,
                        is_directory: false,
                        size: metadata.len(),
                        observation: FileObservation::capture(&metadata),
                    });
                }
            } else {
                // An unknown-kind entry that opened as a socket, fifo or
                // device has no contents this walk can survey.
                entries_skipped = entries_skipped.saturating_add(1);
            }
        }
    }
    Ok(RepositoryQueryWalk {
        files,
        entries_skipped,
        stop_reason,
    })
}

fn inspect_query_child(
    directory: &fs::File,
    name: &Path,
    skipped: &mut usize,
) -> Option<fs::Metadata> {
    let child = match open_directory_entry_for_query(directory, name) {
        RepositoryQueryEntry::Opened(child) => child,
        RepositoryQueryEntry::Vanished | RepositoryQueryEntry::Symlink => return None,
        RepositoryQueryEntry::Skipped => {
            *skipped = skipped.saturating_add(1);
            return None;
        }
    };
    if let Ok(metadata) = child.metadata() {
        Some(metadata)
    } else {
        *skipped = skipped.saturating_add(1);
        None
    }
}

pub(super) fn retain_repository_query_glob_matches(
    query: &RepositoryQuery,
    glob_matcher: Option<&PathGlob>,
    files: &mut Vec<DiscoveredRepositoryFile>,
) {
    files.sort_by(|left, right| left.path.cmp(&right.path));
    if let Some(matcher) = glob_matcher {
        files.retain(|entry| matches_query_glob(query, matcher, &entry.path));
    }
}

pub(super) fn list_repository_query_paths(
    control: &OperationContext,
    query: &RepositoryQuery,
    files: Vec<DiscoveredRepositoryFile>,
    files_considered: usize,
    entries_skipped: usize,
    mut stop_reason: Option<RepositoryQueryStopReason>,
) -> Result<RepositoryQueryResult, ExecutionBackendError> {
    let mut matches = Vec::new();
    let limits = query.limits();
    let empty = WorkspaceContentIdentity::from_bytes(&[])?;
    let mut result_bytes = 0_usize;
    for entry in files.into_iter().take(files_considered) {
        check_workspace_control(control)?;
        if matches.len() == limits.max_matches {
            stop_reason = Some(RepositoryQueryStopReason::Matches);
            break;
        }
        let is_directory = entry.is_directory;
        let entry_size = if is_directory { 0 } else { entry.size };
        let relative = entry.path;
        if query.list_depth.is_some() {
            // Match the SDK's lossy JSON encoding, including escapes and separators.
            let row_bytes = serde_json::json!({
                "path":relative.to_string_lossy(),
                "kind":if is_directory { "directory" } else { "file" }
            })
            .to_string()
            .len()
            .saturating_add(1);
            if row_bytes > limits.max_result_bytes.saturating_sub(result_bytes) {
                stop_reason = Some(RepositoryQueryStopReason::ResultBytes);
                break;
            }
            result_bytes = result_bytes.saturating_add(row_bytes);
        }
        matches.push(RepositoryQueryMatch {
            is_directory,
            entry_size,
            path: relative,
            line: 1,
            column: 1,
            bytes: Arc::from([]),
            truncated: false,
            content: empty,
        });
    }
    Ok(RepositoryQueryResult {
        matches: matches.into(),
        files_considered,
        files_scanned: 0,
        bytes_scanned: 0,
        skipped_binary: 0,
        skipped_oversize: 0,
        skipped_changed: 0,
        entries_skipped,
        lossy_lines: 0,
        stop_reason,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal workspace scan passes borrowed workspace/root state explicitly"
)]
pub(super) fn scan_repository_query_files(
    workspace: &ExecutionWorkspace,
    root: &fs::File,
    query: &RepositoryQuery,
    control: &OperationContext,
    limits: RepositoryQueryLimits,
    files: Vec<DiscoveredRepositoryFile>,
    files_considered: usize,
    mut entries_skipped: usize,
    mut stop_reason: Option<RepositoryQueryStopReason>,
) -> Result<RepositoryQueryResult, ExecutionBackendError> {
    let mut matches = Vec::new();
    let mut files_scanned = 0_usize;
    let mut bytes_scanned = 0_u64;
    let mut skipped_binary = 0_usize;
    let mut skipped_oversize = 0_usize;
    let mut skipped_changed = 0_usize;
    let mut result_bytes = format!("matches={} incomplete=true\n", limits.max_matches).len();
    let pattern = RepositoryLiteralMatcher::new(query.literal(), query.match_case());
    let regex = query
        .regex
        .as_deref()
        .map(|pattern| compile_query_regex(pattern, query.match_case()))
        .transpose()?;
    'files: for entry in files.into_iter().take(files_considered) {
        let relative = entry.path;
        let reported_bytes = entry.size;
        let discovered = entry.observation;
        check_workspace_control(control)?;
        if reported_bytes > limits.max_file_bytes {
            skipped_oversize = skipped_oversize.saturating_add(1);
            continue;
        }
        if reported_bytes > limits.max_total_bytes.saturating_sub(bytes_scanned) {
            stop_reason = Some(RepositoryQueryStopReason::ScannedBytes);
            break;
        }
        let mut file = match open_query_entry_beneath(root, workspace, &relative, false) {
            RepositoryQueryEntry::Opened(file) => file,
            RepositoryQueryEntry::Vanished | RepositoryQueryEntry::Symlink => {
                skipped_changed = skipped_changed.saturating_add(1);
                continue;
            }
            RepositoryQueryEntry::Skipped => {
                entries_skipped = entries_skipped.saturating_add(1);
                continue;
            }
        };
        let bytes = match read_unchanged_query_file(&mut file, discovered, limits.max_file_bytes)? {
            QueryFileRead::Changed => {
                skipped_changed = skipped_changed.saturating_add(1);
                continue;
            }
            QueryFileRead::Oversize => {
                skipped_oversize = skipped_oversize.saturating_add(1);
                continue;
            }
            QueryFileRead::Bytes(bytes) => bytes,
        };
        drop(file);
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
                    let retained = utf8_prefix_len(line, line.len().min(limits.max_match_bytes));
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
                        entry_size: 0,
                        is_directory: false,
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
                    check_workspace_control(control)?;
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
                    entry_size: 0,
                    is_directory: false,
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
        skipped_changed,
        entries_skipped,
        lossy_lines: 0,
        stop_reason,
    })
}
