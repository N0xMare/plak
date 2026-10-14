//! The model-visible strings this pack owns.
//!
//! A tool's description and its refusals travel with the tool (spec 02 §6.3):
//! they change together and they are pinned together.

/// Standard read_file tool description.
pub const PLAKOUS_TOOL_READ_FILE_DESCRIPTION: &str = "Read one workspace text file. path is workspace-relative, or exactly a path a process result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused. offset is a 1-indexed start line (default 1). limit is a maximum line count (default 200, max 2000). Returns a JSON string with content (the file text, preserving LF/CRLF and final newline), offset (1-indexed), lines, eof, and next_offset (null at EOF), plus optional workspace_generation. A page holds at most 256 KiB; continue from next_offset. A line over 2000 characters is clipped with a marker and counted in clipped_lines; invalid UTF-8 is replaced and marked lossy=true; binary files are refused. In JavaScript use JSON.parse(await tools.read_file({...})).content; in Python use json.loads(await read_file({...}))[\"content\"]. Metadata is not file content. Check eof before treating the result as the entire file.";

/// Standard apply_patch git-diff tool description.
pub const PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION: &str = "Change workspace files in exactly one of three forms. patch: a bounded UTF-8 git-style unified diff or a Codex *** Begin Patch envelope, applied after exact all-file preflight; binary, rename, copy, and mode-only patches are rejected. file_path with old_string, new_string and optional replace_all: one exact replacement (an empty old_string creates the file, or overwrites it with replace_all). file_path with edits: every edits[].old_string non-empty, unique in the original file and non-overlapping; all apply together or none does. The forms do not combine.\n\nModify example:\ndiff --git a/old.txt b/old.txt\n--- a/old.txt\n+++ b/old.txt\n@@ -1 +1 @@\n-before\n+after\n\nCreate example:\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+created\n\nPrefer one small successful patch over many retries.";

/// Standard inspect_image tool description.
pub const PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION: &str = "Inspect and return a PNG, JPEG, GIF, or WebP from the configured workspace, bounded to 196608 bytes.";

/// Codex OAuth Direct/custom `apply_patch` description (Codex CLI 0.153.x wire).
///
/// Reproduced from OpenAI Codex (Apache-2.0); see the README License section.
pub const PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION: &str = "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.";

/// Grok OAuth read_file tool description.
pub const PLAKOUS_GROK_READ_DESCRIPTION: &str = "Read a text file. target_file is workspace-relative; absolute paths and .. are rejected, except exactly a path a run_terminal_command result or completion notice named on its `full output:` line. A `full output:` path issued by an earlier process (before a restart or reload) or another agent is refused. Default limit is 1000 lines; maximum limit is 2000 lines. Returns a JSON string with content (the file text, preserving LF/CRLF and final newline), offset (1-indexed), lines, eof, and next_offset (null at EOF), plus optional workspace_generation. A page holds at most 256 KiB; continue from next_offset. A line over 2000 characters is clipped with a marker and counted in clipped_lines; invalid UTF-8 is replaced and marked lossy=true; binary files are refused. In JavaScript use JSON.parse(await tools.read_file({...})).content; in Python use json.loads(await read_file({...}))[\"content\"]. Metadata is not file content. Check eof before treating the result as the entire file.";

/// Grok OAuth search_replace tool description.
pub const PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION: &str = "Replace an exact string in a file.\n\n- `old_string` must match exactly one place in the file. If it appears more than once, add surrounding lines to make it unique, or set `replace_all` to change every occurrence (handy for renaming an identifier).\n- To create a new file, set `old_string` to an empty string.";

/// Grok OAuth write tool description.
pub const PLAKOUS_GROK_WRITE_DESCRIPTION: &str = "Create or overwrite a file.\n\n- Writing to an existing path replaces the file — read it first with the read_file tool when you need the current contents.\n- Missing parent directories are created.\n- file_path is relative to the workspace root (omit `.` for the root). Absolute paths and `..` are rejected.";

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
