//! This package contains named binaries that must each fail to compile.

/// A harmless library target lets Cargo resolve dependencies independently of
/// the deliberately invalid binaries.
pub fn dependency_probe() {}

/// Direct use of the unsupported sibling tier is deliberately permitted.
pub fn agent_internal_runtime_is_nameable(_: Option<plakous_agent::internal::ToolRuntime>) {}
