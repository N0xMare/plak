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
    PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8,
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
    "4a00436626d8492a9445b7a4a9c2d86fd20b2af4049b94f52a9be7a4b85f2d59";
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
    "e95103336a4c1e0af1f6b027dd4550b7059c992e0f2a08002b32a90aa9c67891";
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
    assert!(combined.starts_with("The `apply_patch` tool"));
    assert!(PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION.contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT));
}

/// The concatenated digest of every string this module owns.
const GROK_PROMPTS_DIGEST: &str =
    "d72c2ee8d0e875b874d6e014b80a1abf44fe7202653c2ee075fdd7a90bf3bb7c";
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
    assert!(combined.contains("regular expressions"));
    assert!(combined.starts_with("Run a one-shot `/bin/sh -c` command"));
    assert!(!combined.contains("background: true"));
    assert!(!combined.contains("multiline mode is unsupported"));
    assert!(!combined.contains("Plak"));
}

/// The concatenated digest of every string this module owns.
const ERROR_PROMPTS_DIGEST: &str =
    "58ed92e9cad4985d000f02b472c213add7949c866f1cb2c5ef7dc26ddfcc2499";
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
