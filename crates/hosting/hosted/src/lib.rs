//! One hosted coding session.
//!
//! The host loop, its bounded cancel, the session record, and the ports a
//! product supplies it with. Everything here is written against `plakous` and
//! `plakous-host-protocol`; nothing in it knows Plak's configuration schema,
//! its XDG layout, or its database.
//!
//! Products supply [`runtime::HostConfig`] and the persistence/credential ports;
//! [`runtime`] owns the host loop around Plakous' single-agent driver and
//! observes that driver's canonical terminal settlement.
//! [`host_service`] routes attachments and bounded observations, not execution
//! authority. Its wire handlers do not turn a reconnect or an observation into
//! a second turn admission.
//!
//! [`sessions::SessionRecord`] holds product intents and discardable projections.
//! The exact Plakous locator remains canonical history authority, while
//! [`session_store::SessionRecordStore`] owns product-record commits and their
//! compare-and-swap revisions. [`runtime::RootBindingDocument`] and
//! [`runtime::SurfaceHandler`] keep settings parsing and publication in the
//! product, outside the host loop's authority.
//!
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "unit tests"
    )
)]

/// Exact local account references and total vertical identity.
pub mod account;
/// How the host classifies and renders one tool call's activity.
pub mod activity;
/// Image files attached to a prompt: reading, sniffing, bounds and chips.
pub mod attachments;
/// The session-binding vocabulary: composition, wire, target, and the
/// `[root] agent` fact a session compares its bound tuple against.
pub mod binding;
/// The route identity and catalog capability registry.
pub mod capabilities;
/// The provider-grounded context projection a turn reports against.
pub mod context;
/// The closed reasoning-effort vocabulary.
pub mod effort;
mod exchange_observation;
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
mod visual_read;

#[allow(
    missing_docs,
    reason = "agent attachment and provider configuration fields await contracts"
)]
pub mod agent;
pub mod agent_tree_protocol;
mod driver;
pub(crate) mod jobs;
mod node_usage;
pub(crate) mod off_loop;
/// Host terminal-pin admission for destructive workspace operations.
pub mod workspace_reclamation;
// The hosted driver composes the tree runtime through its public vocabulary.
pub(crate) use plakous_recursive_subagents::task_set::{TaskHandle, TaskSet};
pub use plakous_recursive_subagents::{
    cell_budget, epoch as orchestration, hub as topology_runtime, tools as agent_tree_tools,
    tree as agent_tree,
};
pub mod coding_routes;
pub mod error;
/// The root's optional tools frozen into its session record at creation.
pub mod frozen_tools;
pub mod host_service;
pub(crate) use plakous_workspace::git_redirect_fence;
#[cfg(any(test, feature = "test-support"))]
pub use plakous_workspace::occupancy_vcs;
/// Whether this build may run the git CLI for workspace VCS reads.
pub use plakous_workspace::occupancy_vcs::git_cli_enabled;
pub use plakous_workspace::occupancy_vcs::maintenance as workspace_maintenance;
pub use plakous_workspace::{JJ_CLI_SUPPORTED, JJ_LIB_VERSION, built_jj_library_version};
pub(crate) mod intent_dispatch;
pub(crate) mod interrupt;
pub mod openrouter;
#[allow(
    missing_docs,
    reason = "product observation fields and closed presentation vocabularies await contracts"
)]
pub mod product_observation;
pub mod prompt_inputs;
pub mod request_slots;
#[cfg(test)]
pub(crate) mod role_table;
pub mod runtime;
pub mod skills;
pub(crate) mod storage_pressure;
pub mod surface_protocol;
pub mod tool_plan;
pub mod tools;
pub mod topology_extensions;
pub mod topology_navigator;
pub use plakous_workspace::workspace;

pub use error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};

mod call_detail;
mod frozen_tables;
pub mod mcp_binding;
mod node;
mod process_environment;
#[cfg(unix)]
pub use process_environment::operator_launch;
#[cfg(feature = "test-support")]
mod test_diagnostics;
/// Without the gate's build, nothing is reported.
#[cfg(not(feature = "test-support"))]
mod test_diagnostics {
    pub(crate) const fn credential_wait_refused(_: bool) {}
}
pub mod runner_adoption;
#[doc(hidden)]
pub mod runner_reader;
/// Host crash cuts, public only to the gate build's tests.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support_runner;
#[cfg(not(feature = "test-support"))]
mod test_support_runner;

// A session release joins one in-flight store call and then commits the shutdown receipt, each
// bounded by the agent's store deadline; the host's release timeout must cover both.
const _: () = assert!(
    plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT.as_nanos()
        >= 2 * plakous::agent::DEFAULT_STORE_OPERATION_TIMEOUT.as_nanos()
);

pub mod platform;

/// Child assignment instructions and their byte-pinned renderers.
pub mod child_prompts;
