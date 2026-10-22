//! Canonical capability identity: the names, revisions and bounds every pack,
//! composition and descriptor agrees on.
//!
//! These are durable format. A registered tool id, a canonical operation name
//! and a projection revision are persisted in session descriptors and compared
//! on reopen, so they stay in the core with the registry that enrolls them
//! even though the executors that carry them live in the pack crates.

/// Id-merge task-list capability (`merge_plan`). Not in the standard manifest;
/// the Grok OAuth `todo_write` presentation lowers onto it. Neutral so any
/// presentation with merge-by-id todo semantics can bind it.
pub const MERGE_PLAN_CAPABILITY_NAME: &str = "merge_plan";

/// Canonical bounded-read capability.
pub const READ_FILE_CAPABILITY_NAME: &str = "read_file";
/// Canonical exact-content patch capability.
pub const APPLY_PATCH_CAPABILITY_NAME: &str = "apply_patch";
/// Canonical image-inspection capability.
pub const INSPECT_IMAGE_CAPABILITY_NAME: &str = "inspect_image";
/// Canonical bounded repository-search capability.
pub const SEARCH_REPOSITORY_CAPABILITY_NAME: &str = "search_repository";
/// Canonical optimistically revisioned plan-replacement capability.
pub const UPDATE_PLAN_CAPABILITY_NAME: &str = "update_plan";
/// Canonical Codex freeform patch-envelope capability. Durable: the Codex
/// menu binds it and a persisted descriptor names it.
pub const CODEX_PATCH_ENVELOPE_CANONICAL_NAME: &str = "patch_envelope";

/// Turn-level observation budget for one process result.
pub const PROCESS_OBSERVATION_BYTES: usize = 40_000;
/// Turn-level observation budget for one Claude `Bash` result.
pub const CLAUDE_BASH_OBSERVATION_BYTES: usize = 30_000;

/// Canonical standard-tool pack semantic revision.
pub const STANDARD_CODING_TOOL_VERSION: &str = "3.0.0";
/// Exact process-tool projection catalog revision.
pub const PROCESS_TOOL_PROJECTION_REVISION: u32 = 15;
/// Canonical provider-neutral process-session start operation name.
pub const PROCESS_SESSION_CAPABILITY_NAME: &str = "process_session_start";
/// Canonical process-session write operation name.
pub const PROCESS_SESSION_WRITE_NAME: &str = "process_session_write";
/// Canonical process-session cursor-bound poll operation name.
pub const PROCESS_SESSION_POLL_NAME: &str = "process_session_poll";
/// Canonical process-session terminate operation name.
pub const PROCESS_SESSION_TERMINATE_NAME: &str = "process_session_terminate";
/// Canonical process-session capability revision.
pub const PROCESS_SESSION_CAPABILITY_REVISION: &str = "10";
/// Exact durable canonical registration identity.
pub const PROCESS_SESSION_REGISTERED_TOOL_ID: &str = "plakous.process-session-start.v3";
/// Exact durable write registration identity.
pub const PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID: &str = "plakous.process-session-write.v3";
/// Exact durable poll registration identity.
pub const PROCESS_SESSION_POLL_REGISTERED_TOOL_ID: &str = "plakous.process-session-poll.v3";
/// Exact durable terminate registration identity.
pub const PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID: &str =
    "plakous.process-session-terminate.v3";
/// Exact process-local shared executor binding identity.
pub const PROCESS_SESSION_EXECUTOR_ID: &str = "plakous.process-session-executor.v3";
/// Maximum bytes one standard read or search operation may retain from a file.
pub const MAX_STANDARD_TEXT_BYTES: u64 = 786_432;
/// Maximum inline image bytes returned through a standard tool outcome.
pub const MAX_STANDARD_IMAGE_BYTES: u64 = 196_608;
/// Maximum repository-search results.
pub const MAX_STANDARD_SEARCH_MATCHES: usize = 4_096;
/// Maximum repository files inspected by one search.
pub const MAX_STANDARD_SEARCH_FILES: usize = 10_000;
/// Maximum changes in one patch request.
pub const MAX_STANDARD_PATCH_CHANGES: usize = 256;
/// Maximum plan items retained by the default plan state.
pub const MAX_STANDARD_PLAN_ITEMS: usize = 256;
/// Maximum UTF-8 bytes in one plan step.
pub const MAX_STANDARD_PLAN_STEP_BYTES: usize = 4_096;
/// Maximum UTF-8 bytes in one plan explanation.
pub const MAX_STANDARD_PLAN_EXPLANATION_BYTES: usize = 16_384;
