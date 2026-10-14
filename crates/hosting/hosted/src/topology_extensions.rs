//! The product's extension set over the registrations a tree grants.
//!
//! `plakous-recursive-subagents` compiles the caller's grant into
//! `ToolRegistration`s and stops there: what an "extension set" is — nested
//! eligibility, duplicate-name refusal, the agreement pack it carries — is the
//! host's vocabulary, and the tree has no opinion about it.

#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "test-support"))]
use crate::agent_tree::ProductAgentTree;
#[cfg(any(test, feature = "test-support"))]
use crate::agent_tree_tools::topology_tool_registrations;
#[cfg(any(test, feature = "test-support"))]
use crate::error::CliResult;
#[cfg(any(test, feature = "test-support"))]
use crate::tools::ProductToolExtensions;

/// Grant-compiled topology tools for one caller on one tree. The production
/// attach path builds its set from `topology_tool_registrations_on` with the
/// runtime's own ports (`agent/build.rs`); this is the bare-tree convenience
/// the prompt goldens and unit tests drive.
#[cfg(any(test, feature = "test-support"))]
pub fn topology_tool_extensions(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
) -> CliResult<ProductToolExtensions> {
    ProductToolExtensions::from_nested_registrations(topology_tool_registrations(tree, caller_id)?)
}

/// [`topology_tool_extensions`] with the await and interrupt slots spelled in
/// `vocabulary` (Grok's output and kill tools on a Grok node).
#[cfg(any(test, feature = "test-support"))]
pub fn topology_tool_extensions_in(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
    vocabulary: crate::agent_tree_tools::TopologyVocabulary,
) -> CliResult<ProductToolExtensions> {
    ProductToolExtensions::from_nested_registrations(
        crate::agent_tree_tools::topology_tool_registrations_in(tree, caller_id, vocabulary)?,
    )
}
