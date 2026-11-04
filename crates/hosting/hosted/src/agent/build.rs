//! Construction of the product coding session: the agent spec, tool
//! registries and harness profiles, the build/open/resume entry points, and
//! the session runtime bindings the product composes.

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter"
))]
pub(crate) mod api;
mod binding;
#[cfg(any(test, feature = "test-support"))]
mod injected;
mod policy;
mod profile;
mod session;
mod tools;
mod topology;

pub(super) use binding::coding_session_runtime_binding_for;
#[cfg(test)]
pub(super) use binding::ensure_workspace_excludes_product_state;
pub(crate) use binding::{
    api_session_runtime_binding, api_session_runtime_binding_for,
    coding_session_runtime_binding_with_attach,
};
pub use binding::{
    coding_session_runtime_binding, coding_session_runtime_binding_attaching_root,
    coding_session_target_binding, compaction_session_runtime_binding,
    selected_coding_provider_configuration,
    selected_coding_provider_configuration_with_processing_tier,
};
#[cfg(any(test, feature = "test-support"))]
pub use injected::{
    build_or_open_bound_session, create_seeded_bound_session, injected_root_tool_registrations,
    verify_bound_session,
};
pub use policy::{
    NATIVE_RUN_MAX_MODEL_ROUNDS, NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING, NATIVE_RUN_MAX_OUTPUT_TOKENS,
    NATIVE_RUN_MODEL_CLEANUP_TIMEOUT, NATIVE_RUN_MODEL_OPERATION_TIMEOUT,
    NATIVE_RUN_TURN_INFERENCE_ALLOWANCE, NATIVE_RUN_TURN_INPUT_LIABILITY_BUDGET,
    NATIVE_RUN_TURN_RETRY_RESERVE, effective_presentation_label, product_effect_policy,
    product_subscription_request_limits,
};
#[cfg(test)]
pub(super) use policy::{NATIVE_RUN_ROUND_INPUT_LIABILITY_CEILING, PROCESS_SESSION_TOOLS};
pub(crate) use policy::{SessionToolFence, occupancy_read_only_allows, occupancy_session_fence};
#[cfg(test)]
pub(crate) use policy::{session_tool_policy, stamped_workspace_policy};
#[cfg(test)]
pub(super) use profile::product_identity_profile;
#[cfg(test)]
pub(crate) use profile::selected_harness_profile;
#[cfg(test)]
pub(crate) use profile::{
    enroll_product_instructions, product_prompt_variant, subagent_catalog_profile_id,
};
#[cfg(test)]
pub(super) use session::reopen_refusal;
pub(super) use session::require_reconstructed_coding_projection;
pub use session::{
    AttachedCodingSession, AttachedReleaseError, CodingSessionOpenFailure, SeededHistory,
    SeededLine, build_api_root_coding_session, build_coding_session, build_runtime_session,
    open_api_root_coding_session, open_coding_session,
};
pub(crate) use session::{
    build_api_coding_session_seed, build_coding_session_seed, open_api_coding_session_with_fence,
    verify_api_coding_session, verify_coding_session,
};
#[cfg(test)]
pub(crate) use tools::coding_tool_registry;
pub(crate) use tools::coding_tool_registry_with_extensions;
#[cfg(test)]
pub(super) use tools::{
    bind_node_tool_plan, is_codex_code_mode_candidate, is_exact_code_mode_attachment, node_session,
};
pub use tools::{direct_composition_capable, spawn_child_composition};
#[cfg(test)]
pub(crate) use tools::{direct_extension_tool, projection_target};
pub(super) use tools::{
    frozen_parent_composition, spawn_roster_composition, spawn_roster_processing_tier,
};
#[cfg(test)]
pub(super) use topology::SpawnParentInputs;
#[cfg(test)]
pub(crate) use topology::topology_extensions_if_enabled;
pub use topology::{
    ApiTopologyParent, ProductAgentSpec, SpawnParent, TopologyInvokeReceipt, api_topology_parent,
    invoke_topology_tool_on_hub,
};
pub(crate) use topology::{
    node_coding_presentation, seed_genesis_tree, vocabulary_for_presentation,
};
pub(super) use topology::{rejoin_parent, spawn_parent_tree_id};
#[cfg(test)]
pub(super) use topology::{
    spawn_parent_root_account, stamp_root_account, topology_grant_caller_id,
};
