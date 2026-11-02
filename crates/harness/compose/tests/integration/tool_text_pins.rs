//! The tool descriptions and refusals, pinned across the packs that own them.
//!
//! Each pack owns its own strings, but these four digests span packs: a Codex
//! costume's text is spread over files, process and plan, and the coding
//! digest covers every canonical description at once. They are byte-identical
//! to the tables that lived in `plakous-tools/src/prompts/` before the cut --
//! a pack move may not move a pin.

#![allow(missing_docs, reason = "integration test crate")]

use plakous_tool_files::{
    PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION, PLAKOUS_GROK_READ_DESCRIPTION,
    PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION, PLAKOUS_GROK_WRITE_DESCRIPTION,
    PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION, PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT,
    PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY, PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY,
    PLAKOUS_TOOL_IMAGE_TOO_LARGE, PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
    PLAKOUS_TOOL_PATCH_HUNK_CONFLICT, PLAKOUS_TOOL_PATCH_KIND_CHANGED,
    PLAKOUS_TOOL_PATCH_MALFORMED, PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8,
    PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE, PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT,
    PLAKOUS_TOOL_READ_FILE_DESCRIPTION, PLAKOUS_TOOL_READ_OFFSET_PAST_EOF,
    PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS, PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND,
    PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8, PLAKOUS_TOOL_WRITE_STALE, PLAKOUS_TOOL_WRITE_UNREAD,
};
use plakous_tool_plan::{
    PLAKOUS_CODEX_UPDATE_PLAN_DESCRIPTION, PLAKOUS_GROK_TODO_DESCRIPTION,
    PLAKOUS_MERGE_PLAN_DESCRIPTION, PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP,
};
use plakous_tool_process::{
    PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION, PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
    PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT, PLAKOUS_TOOL_OUTPUT_OMITTED,
    PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION,
};
use plakous_tool_search::{
    PLAKOUS_GROK_GREP_DESCRIPTION, PLAKOUS_GROK_LIST_DIR_DESCRIPTION,
    PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION, PLAKOUS_TOOL_SEARCH_REQUIRES_GLOB,
    PLAKOUS_TOOL_SEARCH_REQUIRES_QUERY, PLAKOUS_TOOL_SEARCH_RESULT_BOUND,
};
use plakous_tools::PLAKOUS_TOOL_UPDATE_PLAN_DESCRIPTION;
use plakous_tools::PromptDigest;

/// The concatenated digest of every string this module owns.
const CODING_PROMPTS_DIGEST: &str =
    "81f531ab86b6338bdf6d31aeb0926d05bef167c3d7f3904faa0df19fe10b2a0d";
#[test]
fn coding_prompts_digest_is_stable() {
    let combined = [
        PLAKOUS_TOOL_READ_FILE_DESCRIPTION,
        PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION,
        PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION,
        PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
        PLAKOUS_TOOL_UPDATE_PLAN_DESCRIPTION,
        PLAKOUS_MERGE_PLAN_DESCRIPTION,
        PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION,
        PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
    ]
    .concat();
    assert_eq!(
        PromptDigest::of(&combined),
        PromptDigest::from_hex(CODING_PROMPTS_DIGEST)
    );
    assert!(combined.starts_with("Read one workspace text file."));
    assert!(PLAKOUS_TOOL_READ_FILE_DESCRIPTION.contains("`full output:`"));
    assert!(!PLAKOUS_TOOL_READ_FILE_DESCRIPTION.contains("Bash"));
    assert!(!PLAKOUS_TOOL_READ_FILE_DESCRIPTION.contains("run_terminal_command"));
    assert!(PLAKOUS_TOOL_READ_FILE_DESCRIPTION.contains("default 200, max 2000"));
    assert!(PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION.contains("head_limit"));
    assert!(PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION.contains("counts_only"));
    assert!(PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION.contains("offset"));
    assert!(PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION.contains("`full output:`"));
    assert!(PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION.contains("earlier process"));
    assert!(PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION.contains("Codex *** Begin Patch"));
    assert!(PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION.contains("returns the output that follows"));
    assert!(PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION.contains("Background jobs survive the turn"));
    assert!(PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION.contains("30000 ms"));
    assert!(PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION.contains("session cwd"));
    assert!(
        PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION
            .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
    );
    assert!(!PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION.contains("background: true"));
}

/// The concatenated digest of every string this module owns.
const CODEX_PROMPTS_DIGEST: &str =
    "fa4df4427be0c199e13da016e1e939f0262ea47923c882cf579435c06dc6eaab";
#[test]
fn codex_prompts_digest_is_stable() {
    let combined = [
        PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION,
        PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION,
        PLAKOUS_CODEX_UPDATE_PLAN_DESCRIPTION,
    ]
    .concat();
    assert_eq!(
        PromptDigest::of(&combined),
        PromptDigest::from_hex(CODEX_PROMPTS_DIGEST)
    );
    assert!(combined.contains("raw *** Begin Patch envelope, not JSON"));
    assert!(PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION.contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT));
}

/// The concatenated digest of every string this module owns.
const GROK_PROMPTS_DIGEST: &str =
    "8ddfba6cef018332c9c42d0018ca666ef17bceaf5a952a19011317fd9457a9a7";
#[test]
fn grok_prompts_digest_is_stable() {
    let combined = [
        PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
        PLAKOUS_GROK_READ_DESCRIPTION,
        PLAKOUS_GROK_GREP_DESCRIPTION,
        PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION,
        PLAKOUS_GROK_TODO_DESCRIPTION,
        PLAKOUS_GROK_WRITE_DESCRIPTION,
        PLAKOUS_GROK_LIST_DIR_DESCRIPTION,
    ]
    .concat();
    assert_eq!(
        PromptDigest::of(&combined),
        PromptDigest::from_hex(GROK_PROMPTS_DIGEST)
    );
    assert!(combined.contains("30000 ms"));
    assert!(PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION.contains("read_file"));
    assert!(PLAKOUS_GROK_READ_DESCRIPTION.contains("`full output:`"));
    assert!(PLAKOUS_GROK_READ_DESCRIPTION.contains("run_terminal_command"));
    assert!(PLAKOUS_GROK_READ_DESCRIPTION.contains(
        "workspace-relative or absolute inside the workspace; paths outside the workspace and `..` are refused"
    ));
    assert!(!PLAKOUS_GROK_READ_DESCRIPTION.contains("JSON.parse"));
    assert!(!PLAKOUS_GROK_READ_DESCRIPTION.contains("absolute paths"));
    assert!(PLAKOUS_GROK_READ_DESCRIPTION.contains("one metadata line"));
    assert!(plakous_tool_files::PLAKOUS_GROK_READ_CELL_DESCRIPTION.contains("JSON.parse"));
    assert!(PLAKOUS_TOOL_READ_FILE_DESCRIPTION.contains("JSON.parse"));
    assert!(combined.contains("regular expressions"));
    assert!(PLAKOUS_GROK_GREP_DESCRIPTION.contains("`full output:`"));
    assert!(PLAKOUS_GROK_GREP_DESCRIPTION.contains("run_terminal_command"));
    assert!(PLAKOUS_GROK_GREP_DESCRIPTION.contains("earlier process"));
    assert!(!PLAKOUS_GROK_LIST_DIR_DESCRIPTION.contains("`full output:`"));
    assert!(combined.starts_with("Run a one-shot `/bin/sh -c` command"));
    assert!(!combined.contains("background: true"));
    assert!(!combined.contains("multiline mode is unsupported"));
    assert!(!combined.contains("Plak"));
}

/// The concatenated digest of every string this module owns.
const ERROR_PROMPTS_DIGEST: &str =
    "b1cab0675f7f3811e36803b9460c9d320dd5784055704963981e6e9286a4c0e9";
#[test]
fn error_prompts_digest_is_stable() {
    let combined = [
        PLAKOUS_TOOL_OUTPUT_OMITTED,
        PLAKOUS_TOOL_SEARCH_RESULT_BOUND,
        PLAKOUS_TOOL_SEARCH_REQUIRES_GLOB,
        PLAKOUS_TOOL_SEARCH_REQUIRES_QUERY,
        PLAKOUS_TOOL_READ_OFFSET_PAST_EOF,
        PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT,
        PLAKOUS_TOOL_PATCH_MALFORMED,
        PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY,
        PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE,
        PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8,
        PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP,
        PLAKOUS_TOOL_PATCH_HUNK_CONFLICT,
        PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY,
        PLAKOUS_TOOL_PATCH_KIND_CHANGED,
        PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT,
        PLAKOUS_TOOL_IMAGE_TOO_LARGE,
        PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8,
        PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND,
        PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS,
        PLAKOUS_TOOL_WRITE_UNREAD,
        PLAKOUS_TOOL_WRITE_STALE,
    ]
    .concat();
    assert_eq!(
        PromptDigest::of(&combined),
        PromptDigest::from_hex(ERROR_PROMPTS_DIGEST)
    );
    assert!(combined.starts_with("\n[... {omitted} bytes of output omitted ...]"));
    assert!(PLAKOUS_TOOL_SEARCH_RESULT_BOUND.contains("narrow path or lower head_limit"));
    assert!(PLAKOUS_TOOL_PATCH_HUNK_CONFLICT.contains("re-read the file and retry"));
    assert!(PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT.contains("re-read the file and retry"));
}
