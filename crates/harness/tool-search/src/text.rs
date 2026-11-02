//! The model-visible strings this pack owns.
//!
//! A tool's description and its refusals travel with the tool (spec 02 §6.3):
//! they change together and they are pinned together.

/// Standard search_repository tool description.
pub const PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION: &str = "Search regular UTF-8 files below one directory for a literal string in one bounded deterministic query. path is workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused, except exactly a path a process result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused; an admitted one is searched whole, up to its 64 MiB bound. query is required for content search. Matching is ASCII-insensitive by default; set case_sensitive=true for exact byte matching. paths_only=true with filename_glob lists matching paths (query is still required but ignored). With use_regex, paths_only lists unique matching files and counts_only lists per-file match counts. head_limit caps returned matches (max 4096). offset skips that many matches before applying head_limit. Paths matched by .gitignore or .ignore are not searched, nor is .git; other dotfiles are. Content scans are bounded at 256 MiB and traversal at 10,000 entries; files over 1 MiB and binary files are skipped and counted. Incompleteness is reported on the first line with the bound and how to narrow path, filename_glob or query.";

/// Grok OAuth grep tool description.
pub const PLAKOUS_GROK_GREP_DESCRIPTION: &str = "Search file contents with regular expressions (ripgrep).\n\n- Full regex syntax, so escape literal special characters: `functionCall\\(`, or `interface\\{\\}` to find interface{} in Go.\n- Pass pattern as a raw regex string — no surrounding quotes.\n- Output is ripgrep-style: ':' marks match lines, '-' marks context lines, grouped by file. Large results are capped; head_limit may be at most 4096.\n- The type filter maps a closed set of names (`rust`, `py`, `js`, `ts`, `go`, `java`, `md`, `json`) to filename globs; unknown types fail.\n- path, when set, is workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused, except exactly a path a run_terminal_command result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused; an admitted one is searched whole, up to its 64 MiB bound. Omit path to search the workspace root.
- Paths matched by .gitignore or .ignore are not searched, nor is .git; other dotfiles are. Files over 1 MiB and binary files are skipped, scanned text is bounded at 256 MiB and traversal at 10,000 entries. The first result line reports skipped counts or an early stop, names the bound and suggests narrowing path or pattern only for an early stop.";

/// Grok OAuth list_dir tool description.
pub const PLAKOUS_GROK_LIST_DIR_DESCRIPTION: &str = "Lists files and directories in a given path.\nThe target_directory parameter is workspace-relative or absolute inside the workspace (omit `.` for the root); paths outside the workspace and `..` are refused. A missing path fails with `path does not exist: <path>`.\n\nOther details:\n    - The result does not display dot-files and dot-directories.\n    - .gitignore filtering is not applied; all non-dot entries under the directory are listed.";

/// search_repository tool, returned when paths_only is set without filename_glob.
pub const PLAKOUS_TOOL_SEARCH_REQUIRES_GLOB: &str = "glob search requires filename_glob";

/// search_repository tool, returned when query is omitted for a content search.
pub const PLAKOUS_TOOL_SEARCH_REQUIRES_QUERY: &str = "repository search requires query";

/// search_repository tool result, returned when the bounded result buffer is full.
pub const PLAKOUS_TOOL_SEARCH_RESULT_BOUND: &str = "repository query model-visible result exceeds its exact byte limit; narrow path or lower head_limit";

/// Regex search result line when no file matched.
pub(crate) const PLAKOUS_TOOL_SEARCH_NO_MATCHES: &str = "No matches found.";

/// Regex search result line when nothing matched in the files searched while
/// the header names files or entries that were not.
pub(crate) const PLAKOUS_TOOL_SEARCH_NO_MATCHES_SEARCHED: &str =
    "No matches in the files searched.";

/// Regex search result line when `offset` skips every entry.
pub(crate) fn search_no_entries_at_offset(offset: usize, entries: usize) -> String {
    format!("No entries at offset {offset}; {entries} in all.")
}

/// Search header note for job-output lines matched with invalid UTF-8 replaced.
pub(crate) fn search_lossy_lines_note(lines: u64) -> String {
    format!("{lines} lines with invalid UTF-8 were matched with U+FFFD")
}
