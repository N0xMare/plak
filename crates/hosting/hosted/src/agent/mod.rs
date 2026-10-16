//! Shared coding-agent construction for `plak run` and the TUI host.

//!
//! Split by responsibility: `build` constructs and opens sessions,
//! `children` runs a topology's child lifecycle, and `roots` holds the
//! state one hosted session shares. Every name the rest of the host used
//! from `crate::agent` is re-exported here.

pub mod build;
pub mod children;
pub(crate) mod feed_projection;
pub mod live;
mod park;
mod recovery;
mod reresolve;
mod result_context;
pub mod roots;
pub mod transient;

// The test modules below reach these through `use super::*`, as they did when
// the module was one file.
#[cfg(test)]
use crate::binding::{CompositionSelection, ResolvedProtocol};
#[cfg(test)]
use crate::child_prompts::ChildAssignmentStartKind;
#[cfg(test)]
use crate::tools::ProductToolExtensions;
#[cfg(test)]
use plakous::agent::{
    AgentErrorKind, CapabilityRequirement, ContextSourceSelector, InstructionCompositionStrategy,
    ReasoningEffort, ResolvedContextSourceFact, TurnResult,
};
#[cfg(test)]
use plakous::model::{
    CachePolicy, LocalDurabilityDisposition, ModelId, OutputBoundDisposition,
    ParallelToolDisposition, ProcessingTierDisposition, ReasoningEffortDisposition,
    ToolPresentationMode, TurnId,
};
#[cfg(test)]
use plakous::prompts::identity::PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER;
#[cfg(test)]
use plakous::subscriptions::{NativeProvider, NativeToolMode};
#[cfg(test)]
use plakous::tools::{
    CodexMinimalLocalCodeModeV1, CodingPresentation, PROCESS_TOOL_PROJECTION_REVISION,
    ProductJavaScriptCompositionV1, ProviderModelProjectionIdentityV2, PythonPtcPresentationV1,
    PythonPtcV1, ToolProjectionMap, WorkspacePolicy,
};
#[cfg(test)]
use std::num::NonZeroU32;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::str::FromStr;
#[cfg(test)]
use std::sync::Arc;

// Kept as unit modules so golden fixtures can exercise private constructors
// without adding test-only helpers to the hosted public API.
#[cfg(test)]
#[path = "../../tests/common/rendered_prompts.rs"]
pub(crate) mod rendered_prompts; // test-only; `pub(crate)` so rendered_wire reuses the route matrix

#[cfg(test)]
#[path = "../../tests/common/rendered_wire.rs"]
pub(crate) mod rendered_wire; // test-only; `pub(crate)` so Markdown goldens reuse the kernel body

#[cfg(test)]
#[path = "../../tests/common/rendered_lifecycle.rs"]
mod rendered_lifecycle;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "product configuration fixtures fail fast"
)]
mod product_gate_tests;

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub use build::build_or_open_injected_session;
pub use build::direct_composition_capable;
pub use build::{
    ApiTopologyParent, AttachedCodingSession, AttachedReleaseError, CodingSessionOpenFailure,
    NATIVE_RUN_MAX_MODEL_ROUNDS, NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING, NATIVE_RUN_MAX_OUTPUT_TOKENS,
    NATIVE_RUN_MODEL_CLEANUP_TIMEOUT, NATIVE_RUN_MODEL_OPERATION_TIMEOUT,
    NATIVE_RUN_TURN_INFERENCE_ALLOWANCE, NATIVE_RUN_TURN_INPUT_LIABILITY_BUDGET,
    NATIVE_RUN_TURN_RETRY_RESERVE, ProductAgentSpec, RESTORED_PAUSED_RESUME_HINT, SpawnParent,
    api_topology_parent, build_api_root_coding_session, build_coding_session,
    build_runtime_session, coding_session_runtime_binding,
    coding_session_runtime_binding_attaching_root, coding_session_target_binding,
    compaction_session_runtime_binding, effective_presentation_label, open_api_root_coding_session,
    open_coding_session, product_effect_policy, product_subscription_request_limits,
    rejoin_inflight_after_resume, selected_coding_provider_configuration,
    selected_coding_provider_configuration_with_processing_tier, topology_tree_is_restored_paused,
};
pub(crate) use build::{
    SessionToolFence, api_session_runtime_binding, build_api_coding_session,
    coding_session_runtime_binding_with_attach, coding_tool_registry_with_extensions,
    direct_extension_tool, enroll_product_instructions, invoke_topology_tool_on_hub,
    node_coding_presentation, occupancy_session_fence, open_api_coding_session_with_fence,
    product_javascript_candidate_attachment, product_prompt_variant, product_python_attachment,
    projection_target, session_tool_policy, stamped_workspace_policy, subagent_catalog_profile_id,
    vocabulary_for_presentation,
};
use build::{
    coding_session_runtime_binding_for, require_reconstructed_coding_projection,
    spawn_child_composition, spawn_parent_tree_id, spawn_roster_composition,
    spawn_roster_processing_tier,
};
/// Offline injection of a child's runtime binding; see the module's own docs.
#[cfg(any(test, feature = "test-support"))]
pub use children::injected_child_binding;
use children::{
    ProductChildSessionBinder, ProductLiveCanceller, ProductMailDeliverer,
    format_session_reopen_error, incompatible_session_message, reattach_inflight_children,
};
#[cfg(test)]
pub(crate) use recovery::TREE_FEED_READ_FAILED;
#[cfg(test)]
pub(crate) use recovery::tests::Fixture as RecoveryFixture;
pub(crate) use recovery::{
    RecoveryPassStart, TerminalExchange, committed_wait_evidence, read_tree_feed,
    start_tree_recovery_pass, wait_evidence_in,
};
pub use roots::ProductStateRoots;
pub use roots::TelemetryHealth;
pub use roots::{
    ExtraTelemetrySink, ProductTelemetry, RosterSource, TelemetrySinkFactory, TreeStoreFactory,
};

// Test-only reach into the submodules' private items (`use super::*`).
#[cfg(test)]
use crate::agent_tree::turn_outcome::{
    RATE_LIMIT_BLOCK_THRESHOLD, TurnOutcome, TurnRefusal, classify_provider_refusal,
};
#[cfg(test)]
use build::{
    NATIVE_RUN_ROUND_INPUT_LIABILITY_CEILING, PROCESS_SESSION_TOOLS,
    ensure_workspace_excludes_product_state, is_codex_code_mode_candidate,
    is_exact_code_mode_attachment, node_session, product_identity_profile,
    spawn_parent_root_account, stamp_root_account, topology_grant_caller_id,
};
#[cfg(test)]
pub(crate) use build::{coding_tool_registry, selected_harness_profile};
pub(crate) use build::{occupancy_read_only_allows, seed_genesis_tree};
#[cfg(test)]
pub(crate) use children::assignment_start_prompt;
#[cfg(test)]
use children::{
    SESSION_LEASE_HOLD_HINT, child_plan_state_authority, child_session_workspace_root,
    child_turn_was_cancelled, finalize_topology_child_turn, frozen_child_tool_selection,
    frozen_openrouter_child_pin, openrouter_child_runtime_binding, openrouter_frozen_pin,
    spawn_child_operation_id, submit_harness_prompt,
};
