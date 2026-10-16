//! One hosted coding session.
//!
//! The turn loop, its bounded cancel, the session record, and the ports a
//! product supplies it with. Everything here is written against `plakous` and
//! `plakous-host-protocol`; nothing in it knows Plak's configuration schema,
//! its XDG layout, or its database.
//!
#![forbid(unsafe_code)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::match_same_arms,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps,
    clippy::struct_excessive_bools,
    clippy::cast_possible_truncation,
    clippy::format_push_string,
    clippy::assigning_clones,
    clippy::single_match,
    clippy::manual_let_else,
    clippy::unused_self,
    clippy::return_self_not_must_use,
    clippy::if_same_then_else,
    clippy::collapsible_if,
    clippy::unnested_or_patterns,
    clippy::inefficient_to_string,
    clippy::format_collect,
    clippy::needless_return,
    reason = "inherited by the hosted extraction, which allowed \
              these crate-wide; tightening them is its own change"
)]
#![allow(
    missing_docs,
    reason = "hosted extraction; the public surface is \
              documented as it is narrowed"
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "unit tests"
    )
)]

/// How the host classifies and renders one tool call's activity.
pub mod activity;
/// The session-binding vocabulary: composition, wire, target, and the
/// `[root] agent` fact a session compares its bound tuple against.
pub mod binding;
/// The provider-grounded context projection a turn reports against.
pub mod context;
/// The closed reasoning-effort vocabulary.
pub mod effort;
/// Where a hosted session writes its event log.
pub mod session_home;
/// The session event log.
pub mod session_log;
/// The port a hosted session persists its record through.
pub mod session_store;
/// The product session record and the rules it is committed under.
pub mod sessions;
/// The inert subscription-allowance projection.
pub mod subscription_usage;

pub mod agent;
pub mod agent_tree_protocol;
pub(crate) mod jobs;
// The hosted driver composes the tree runtime through its public vocabulary.
pub use plakous_recursive_subagents::{
    cell_budget, epoch as orchestration, hub as topology_runtime, tools as agent_tree_tools,
    tree as agent_tree,
};
pub mod coding_routes;
pub mod error;
pub(crate) use plakous_workspace::git_redirect_fence;
#[cfg(any(test, feature = "test-support"))]
pub use plakous_workspace::occupancy_vcs;
pub use plakous_workspace::occupancy_vcs::maintenance as workspace_maintenance;
pub(crate) mod interrupt;
pub mod openrouter;
pub mod product_observation;
pub mod prompt_inputs;
pub mod runtime;
pub(crate) mod storage_pressure;
pub mod surface_protocol;
pub mod tool_plan;
pub mod tools;
pub mod topology_extensions;
pub mod topology_navigator;
pub use plakous_workspace::workspace;

pub use error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};

mod api_runtime;
mod call_detail;
pub mod mcp_binding;
mod process_environment;

// A session release joins one in-flight store call and then commits the shutdown receipt, each
// bounded by the agent's store deadline; the host's release timeout must cover both.
const _: () = assert!(
    plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT.as_nanos()
        >= 2 * plakous::agent::DEFAULT_STORE_OPERATION_TIMEOUT.as_nanos()
);

pub mod platform;

/// Child assignment instructions and their byte-pinned renderers.
pub mod child_prompts;
