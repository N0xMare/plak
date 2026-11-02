//! The model-visible strings this pack owns.
//!
//! A tool's description and its refusals travel with the tool (spec 02 §6.3):
//! they change together and they are pinned together.

/// Blind whole-file overwrite without an explicit text-read observation.
pub const PLAKOUS_TOOL_WRITE_UNREAD: &str = "Write refused: {path} has no current read in this session (each change to that file, resume or reload consumes earlier reads). Read the file with the file-reading tool, then retry.";

/// Blind whole-file overwrite after the observed content changed.
pub const PLAKOUS_TOOL_WRITE_STALE: &str = "Write refused: {path} changed on disk since its last read. Read the file again with the file-reading tool, then retry; no files were changed.";

/// A binary file cannot acquire a successful text-read observation.
pub(crate) const PLAKOUS_TOOL_WRITE_BINARY: &str = "Write refused: {path} is binary and the file-reading tool cannot read it. Delete it in a separate tool call, then create it again; no files were changed.";

/// A whole-file identity cannot be read above the backend's file-size limit.
pub(crate) const PLAKOUS_TOOL_WRITE_TOO_LARGE: &str = "File change refused: {path} exceeds the readable file-size limit. Use a process tool to remove or shrink it, then retry; no files were changed.";

/// A directory cannot be a whole-file mutation target.
pub(crate) const PLAKOUS_TOOL_WRITE_DIRECTORY: &str = "File change refused: {path} is a directory. Choose a file path, then retry; no files were changed.";

/// A path beneath a file or other non-directory can be neither read nor created.
pub(crate) const PLAKOUS_TOOL_WRITE_PARENT_NOT_DIRECTORY: &str = "File change refused: {path} has a parent component that is not a directory. Choose a path inside an existing directory, then retry; no files were changed.";

/// Explicitly enrolled SDK file transaction description.
pub const PLAKOUS_TOOL_EDIT_FILES_DESCRIPTION: &str = "Validate ordered file operations together, then publish one workspace transaction with content/absence checks. Validation failure changes no target; publication is per-file, without rollback, and failure requires recovery. expect is a content revision or absent. Move copies bounded bytes to an absent destination, then deletes the source. It does not preserve the source inode or permissions: newly created destinations are 0600; replacements retain the original destination's permissions. Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; moves list both paths and unchanged files are omitted. Whole-file writes to existing on-disk files require an explicit text read in this session attachment and unchanged content, not just unchanged mtime. Read again after each change to that file or resume/reload; expect is an additional check, not a bypass. Files created within this transaction need no prior read.";

/// Standard read_file tool description.
///
/// This is the canonical definition text nested catalogs copy. It returns the
/// neutral JSON string, so the parsing sentences stay here and not on a Direct
/// presentation.
pub const PLAKOUS_TOOL_READ_FILE_DESCRIPTION: &str = "Read one workspace text file. path is workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused, except exactly a path a process result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused. offset is a 1-indexed start line (default 1). limit is a maximum line count (default 200, max 2000). Returns a JSON string with content (the file text, preserving LF/CRLF and final newline), offset (1-indexed), lines, eof, and next_offset (null at EOF), plus optional workspace_generation. A page holds at most 256 KiB; continue from next_offset. A line over 2000 characters is clipped with a marker and counted in clipped_lines; invalid UTF-8 is replaced and marked lossy=true; binary files are refused. In JavaScript use JSON.parse(await tools.read_file({...})).content; in Python use json.loads(await read_file({...}))[\"content\"]. Metadata is not file content. Check eof before treating the result as the entire file.";

/// Standard apply_patch git-diff tool description.
pub const PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION: &str = "Change workspace files in exactly one of three forms. patch: a bounded UTF-8 git-style unified diff or a Codex *** Begin Patch envelope, applied after exact all-file preflight; Codex Update File accepts an optional *** Move to: destination immediately after its header, with or without content hunks; the destination must not exist. All-file preflight precedes per-file publication without rollback; publication failure requires recovery. Git-style rename, binary, copy, and mode-only patches are rejected. file_path with old_string, new_string and optional replace_all: one exact replacement (an empty old_string creates the file, or overwrites it with replace_all). file_path with edits: every edits[].old_string non-empty, unique in the original file and non-overlapping; all apply together or none does. The forms do not combine. Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; moves list both paths and unchanged files are omitted. Empty old_string with replace_all requires an explicit text read of an existing file in this session attachment and unchanged content, not just unchanged mtime. Read again after each change to that file or resume/reload; context-checked patches need no prior read.\n\nModify example:\ndiff --git a/old.txt b/old.txt\n--- a/old.txt\n+++ b/old.txt\n@@ -1 +1 @@\n-before\n+after\n\nCreate example:\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+created\n\nPrefer one small successful patch over many retries.";

/// Standard inspect_image tool description.
pub const PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION: &str = "Inspect and return a PNG, JPEG, GIF, or WebP from the configured workspace, bounded to 196608 bytes.";

/// Codex OAuth Direct/custom `apply_patch` description (Codex CLI 0.153.x wire).
///
/// Adapted from OpenAI Codex (Apache-2.0); see the README License section.
pub const PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION: &str = "Supply a raw *** Begin Patch envelope, not JSON. Update File allows *** Move to: path after its header, with optional hunks; destination must be absent. Paths obey workspace policy. All-file preflight precedes per-file publication without rollback. Success: changed_files=N then A/M/D and each JSON-quoted relative changed path in lexical order; moves list both paths, unchanged files are omitted.";

/// Grok OAuth Direct `read_file` tool description.
///
/// The Direct result string is the file text plus one metadata line, not the
/// neutral JSON document. Nested cells use [`PLAKOUS_GROK_READ_CELL_DESCRIPTION`].
pub const PLAKOUS_GROK_READ_DESCRIPTION: &str = "Read a text file. target_file is workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused, except exactly a path a run_terminal_command result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused. Default limit is 1000 lines; maximum limit is 2000 lines. Returns the file text verbatim, then one metadata line `offset=<n> lines=<n> eof=<true|false> next_offset=<n|null>`, with `clipped_lines=<n>` when lines were clipped, `lossy=true` when invalid UTF-8 was replaced, `workspace_generation=<n>` when the workspace stamps one, and `final_newline=false` when the text does not end in a newline (the line break before the metadata line is then not file content). No line numbers. The text preserves LF/CRLF. `observation_truncated=true` marks a page cut to fit the context: lines and next_offset then count only the lines shown. A page holds at most 256 KiB; continue from next_offset. A line over 2000 characters is clipped with a marker and counted in clipped_lines; invalid UTF-8 is replaced and marked lossy=true; binary files are refused. The metadata line is not file content. Check eof before treating the result as the entire file.";

/// Grok `read_file` description for a JS or Python composition cell.
///
/// The cell receives the neutral JSON string and parses `content` out of it.
pub const PLAKOUS_GROK_READ_CELL_DESCRIPTION: &str = "Read a text file from a composition cell. target_file is workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused, except exactly a path a run_terminal_command result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused. Default limit is 1000 lines; maximum limit is 2000 lines. Returns a JSON string with content (the file text, preserving LF/CRLF and final newline), offset (1-indexed), lines, eof, and next_offset (null at EOF), plus optional workspace_generation, clipped_lines and lossy. A page holds at most 256 KiB; continue from next_offset. A line over 2000 characters is clipped with a marker and counted in clipped_lines; invalid UTF-8 is replaced and marked lossy=true; binary files are refused. In JavaScript use JSON.parse(await tools.read_file({...})).content; in Python use json.loads(await read_file({...}))[\"content\"]. Metadata is not file content. Check eof before treating the result as the entire file.";

/// Grok OAuth search_replace tool description.
pub const PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION: &str = "Replace an exact string in a file.\n\n- `old_string` must match exactly one place in the file. If it appears more than once, add surrounding lines to make it unique, or set `replace_all` to change every occurrence (handy for renaming an identifier).\n- To create a new file, set `old_string` to an empty string. An empty old_string with replace_all overwrites only after an explicit text read in this session attachment, with unchanged content; read again after each change to that file or resume/reload.\n- file_path is workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused.\n- Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; unchanged files are omitted.";

/// Grok OAuth write tool description.
pub const PLAKOUS_GROK_WRITE_DESCRIPTION: &str = "Create or overwrite a file.\n\n- Overwriting an existing file requires an explicit text read in this session attachment and unchanged content, not just unchanged mtime. Read again after each change to that file or resume/reload.\n- Missing parent directories are created.\n- file_path is workspace-relative or absolute inside the workspace (omit `.` for the root); paths outside the workspace and `..` are refused.\n- Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; unchanged files are omitted.";

/// apply_patch tool, returned when neither patch nor search_replace fields are supplied.
pub const PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT: &str =
    "apply_patch requires patch text or search_replace fields";

/// apply_patch tool, returned when a create hunk targets a non-empty file.
pub const PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY: &str = "create patch does not apply to an empty file";

/// apply_patch tool, returned when a delete hunk leaves non-empty content.
pub const PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY: &str = "delete patch did not produce an empty file";

/// inspect_image tool, returned when the image exceeds the configured byte limit.
pub const PLAKOUS_TOOL_IMAGE_TOO_LARGE: &str = "image exceeds the configured byte limit";

/// apply_patch tool, returned when a unified-diff hunk context does not match.
pub const PLAKOUS_TOOL_PATCH_HUNK_CONFLICT: &str =
    "patch conflict: hunk context did not match; re-read the file and retry with current text";

/// apply_patch tool, returned when preflight kind disagrees with parsed patch.
pub const PLAKOUS_TOOL_PATCH_KIND_CHANGED: &str = "patch operation changed during preparation";

/// apply_patch tool, returned when unified diff parsing fails.
pub const PLAKOUS_TOOL_PATCH_MALFORMED: &str = "patch text is malformed";

/// apply_patch tool, returned when a modify/delete target is not UTF-8.
pub const PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8: &str = "patch target is not valid UTF-8";

/// apply_patch tool, returned when a modify/delete target exceeds the byte limit.
pub const PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE: &str =
    "patch target exceeds the configured byte limit";

/// apply_patch tool, returned when workspace preflight detects a content conflict.
pub const PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT: &str = "patch conflict: workspace content did not match expected input; re-read the file and retry with current text";

/// read_file tool, returned when offset is past EOF.
pub const PLAKOUS_TOOL_READ_OFFSET_PAST_EOF: &str = "read offset is past the end of the file";

/// apply_patch search_replace path, returned when old_string matches more than once.
pub const PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS: &str = "old_string matched more than once; add surrounding lines to make it unique, or pass replace_all";

/// apply_patch search_replace path, returned when old_string is absent from the file.
pub const PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND: &str =
    "old_string was not found in the file; re-read it and copy the exact text";

/// apply_patch search_replace path, returned when the target file is not UTF-8.
pub const PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8: &str = "the file to edit is not valid UTF-8";

/// Every read presentation, returned for a file with NUL bytes in its first 8 KiB.
pub(crate) const PLAKOUS_TOOL_READ_BINARY: &str =
    "file looks binary (it contains NUL bytes); this read returns text only";

/// apply_patch multi-edit path, returned when two edits cover overlapping
/// text; `{first}` and `{second}` are their indices.
pub(crate) const PLAKOUS_TOOL_EDITS_OVERLAP: &str = "edits[{first}] and edits[{second}] overlap in the original file; merge nearby or nested changes into one edit";

/// apply_patch multi-edit path, returned when an edit's old text is absent.
pub(crate) const PLAKOUS_TOOL_EDIT_NOT_FOUND: &str =
    "edits[{index}]: old text was not found in the file; re-read it and copy the exact text";

/// apply_patch multi-edit path, returned when an edit's old text is not unique.
pub(crate) const PLAKOUS_TOOL_EDIT_AMBIGUOUS: &str = "edits[{index}]: old text matched more than once in the original file; add surrounding lines to make it unique";

/// apply_patch multi-edit path, returned when an edit's old text is empty.
pub(crate) const PLAKOUS_TOOL_EDIT_EMPTY: &str =
    "edits[{index}]: old text is empty; create a file with a write instead";

/// apply_patch, returned when `edits` arrive without `file_path` or beside a
/// `patch`.
pub(crate) const PLAKOUS_TOOL_EDITS_FORM: &str =
    "edits need file_path and cannot be combined with patch, old_string, new_string or replace_all";
