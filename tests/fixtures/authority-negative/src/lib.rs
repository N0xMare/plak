//! This package contains named binaries that must each fail to compile.

/// A harmless library target lets Cargo resolve dependencies independently of
/// the deliberately invalid binaries.
pub fn dependency_probe() {}

/// Direct use of the unsupported sibling tier is deliberately permitted.
pub fn agent_internal_runtime_is_nameable(_: Option<plakous_agent::internal::ToolRuntime>) {}

/// Hosted construction is nameable, but not an external extension point.
pub fn hosted_factory_is_nameable(
    _: &dyn plakous_hosted::runtime::RuntimeFactory<plakous_host_protocol::Core>,
) {
}

/// Registered authority is nameable without exposing its constructor.
pub fn registered_authority_is_nameable(
    _: Option<plakous_hosted::runtime::RegisteredRuntimeAuthority>,
) {
}
