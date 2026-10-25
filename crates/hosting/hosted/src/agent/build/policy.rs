//! Session execution policy and budgets.

use crate::binding::CompositionSelection;
use plakous::agent::{AgentBuilder, AgentError};
use plakous::model::{CompiledProviderConfiguration, ToolEffect};
use plakous::subscriptions::{NativeProvider, NativeToolMode, SubscriptionRequestLimits};
use plakous::tools::{
    AlwaysAllowPolicy, CodingPresentation, ToolPolicy, ToolPolicyDecision, ToolPolicyDenial,
    ToolPolicyFuture, ToolPolicyRevision, WorkspacePolicy,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub fn effective_presentation_label(
    composition: CompositionSelection,
    provider: Option<NativeProvider>,
    native_tool_mode: Option<NativeToolMode>,
) -> &'static str {
    match composition {
        CompositionSelection::Direct => "direct",
        CompositionSelection::JsPtc => "js-ptc",
        CompositionSelection::PyPtc => "py-ptc",
        CompositionSelection::NativePtc => "native-ptc",
        CompositionSelection::FollowCatalog => {
            if provider == Some(NativeProvider::Codex)
                && native_tool_mode == Some(NativeToolMode::CodeModeOnly)
            {
                "native-ptc"
            } else {
                "direct"
            }
        }
    }
}

#[must_use]
pub(crate) fn composition_requests_native_ptc(composition: CompositionSelection) -> bool {
    matches!(
        composition,
        CompositionSelection::NativePtc | CompositionSelection::FollowCatalog
    )
}

#[must_use]
pub(crate) fn composition_is_js_ptc(composition: CompositionSelection) -> bool {
    composition == CompositionSelection::JsPtc
}

#[must_use]
pub(crate) fn composition_is_py_ptc(composition: CompositionSelection) -> bool {
    composition == CompositionSelection::PyPtc
}

use crate::error::{CliFailure, CliResult};

pub(super) fn observation_budget_cap_bytes(effective_input_tokens: u64) -> Option<usize> {
    // The product window cap uses text-byte units; L0 still independently
    // charges one token per byte against its actual headroom.
    Some(
        usize::try_from(effective_input_tokens.saturating_mul(32) / 100)
            .unwrap_or(usize::MAX)
            .clamp(32 * 1024, 256 * 1024),
    )
}

/// Upper bound on one response's output tokens on the subscription routes
/// where Plak picks the request bound (SuperGrok). The direct API-key routes
/// send each model's own limit instead. A model's own ceiling is held to it so one response stays
/// cost-bounded and most of the window is left for input; it is large enough
/// that a whole-file write or a long patch fits in one tool call.
pub(crate) const RESPONSE_OUTPUT_CEILING: u32 = 32_768;
/// Minimum/fallback bound retained by dynamic and fixture routes.
pub const NATIVE_RUN_MAX_OUTPUT_TOKENS: u32 = 4_096;
/// Default model-round cap for one root turn: the product `turn.max_model_rounds`.
///
/// A turn has no wall clock, so this cap is the only thing that stops a turn
/// whose model never answers; reaching it buys one tool-free wrap-up round, not
/// a silent cut. One poll is one round: a "research this repo" turn on
/// 2026-09-17 spent 66 rounds polling a single 40-second `cargo check` at
/// one-second yields. The former 256 was about four times that burn while a
/// 20-minute deadline also bounded the turn. With the deadline gone, 512 keeps
/// room for a long build-and-fix turn and stays at half the runtime's
/// `1..=1024` ceiling, which is also the configurable range.
pub const NATIVE_RUN_MAX_MODEL_ROUNDS: usize = 512;
/// Largest `turn.max_model_rounds` a product config may set: the runtime ceiling.
pub const NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING: usize = 1_024;
pub const NATIVE_RUN_MODEL_OPERATION_TIMEOUT: Duration = Duration::from_secs(300);
pub const NATIVE_RUN_MODEL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Conservative input-liability budget one admitted root turn may reserve
/// across all of its model rounds.
///
/// The subscription gate sums a byte-derived upper bound per dispatch
/// (Codex `ceil(bytes/4) + 512`, Grok `bytes + 512`) and denies the round that
/// would cross the budget. Per-round admission is the context envelope's job;
/// this budget only has to stay above
/// `admitted inference requests × one full-envelope request` so it can never
/// bind before the envelope does. It is stated as that product over
/// [`NATIVE_RUN_TURN_INFERENCE_ALLOWANCE`], which covers the wrap-up round as
/// well as the budgeted ones at the largest configurable round cap. The former
/// `1_000_000` bound was the subscription *smoke* profile: a 60-round Code Mode
/// turn on a 272k model summed past it and died with `Responses
/// fence-admission` after ~50 rounds of work (2026-09-03).
pub const NATIVE_RUN_TURN_INPUT_LIABILITY_BUDGET: u64 =
    NATIVE_RUN_TURN_INFERENCE_ALLOWANCE as u64 * NATIVE_RUN_ROUND_INPUT_LIABILITY_CEILING;
/// Inference requests one root turn may reserve from the subscription gate.
///
/// The gate's inference scope is the turn, and it denies once `reserved`
/// reaches the allowance. The gate is built before any session's config is
/// known, so it is sized for [`NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING`]; the
/// runtime's own round cap is what binds. The budgeted rounds are not the
/// whole turn: when a turn reaches its round cap the runtime runs one further
/// round with tool selection withdrawn so the user still gets an answer. That
/// wrap-up round is a real inference request, so the allowance is the round cap
/// plus it. Without the `+ 1` the gate refuses exactly the round that exists to
/// rescue the turn, on every OAuth account, and the turn dies of gate denial
/// instead of answering.
///
/// Every physical dispatch takes a permit, including every generation the
/// protocol kernel replays under the turn retry policy (a refusal, a failure
/// after send, or a stream cut before any tool call), so the allowance also
/// carries [`NATIVE_RUN_TURN_RETRY_RESERVE`]: a turn that replays at most that
/// many times keeps every round it was promised.
pub const NATIVE_RUN_TURN_INFERENCE_ALLOWANCE: usize =
    NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING + 1 + NATIVE_RUN_TURN_RETRY_RESERVE;
/// Replay headroom in a root turn's inference allowance. Rounds and replays
/// draw from the one pooled allowance, so a turn that replays less can spend
/// the remainder on rounds; what the reserve guarantees is that even a turn at
/// the round ceiling still has 64 permits for replays. Past the pool, the gate
/// refuses the dispatch and the turn fails with it.
pub const NATIVE_RUN_TURN_RETRY_RESERVE: usize = 64;
/// Largest byte-derived liability one admitted round can carry: a full 2M
/// token envelope serialized at up to four bytes per token.
pub(in crate::agent) const NATIVE_RUN_ROUND_INPUT_LIABILITY_CEILING: u64 = 8_000_000;

/// Product (run + TUI host + spawned children) subscription gate limits.
///
/// Catalog-only probes may use zero inference allowance. Product coding turns
/// must allow multi-round inference or every turn dies with gate denial /
/// `InvalidConfiguration`. One definition so attach, rebind, and child spawn
/// cannot drift.
///
/// # Errors
///
/// Fails only if the gate rejects the constants above, which it does not.
pub fn product_subscription_request_limits() -> CliResult<SubscriptionRequestLimits> {
    // auth_refresh, catalog, inference, max_inference_input_tokens, max_elapsed, retries.
    // No elapsed clock: a turn has none, and auth/catalog refreshes inside a
    // turn are charged to that turn's scope (spec 01 §3.3).
    SubscriptionRequestLimits::new(
        8,
        16,
        u32::try_from(NATIVE_RUN_TURN_INFERENCE_ALLOWANCE).unwrap_or(u32::MAX),
        NATIVE_RUN_TURN_INPUT_LIABILITY_BUDGET,
        None,
        u32::try_from(NATIVE_RUN_TURN_RETRY_RESERVE).unwrap_or(u32::MAX),
    )
    .map_err(|_| CliFailure::new("product subscription gate limits are invalid"))
}

pub(in crate::agent) const PLAK_PRODUCT_EXTENSION_ISSUER_V1: &[u8] =
    b"plak.product-extension.issuer.v1";
pub(in crate::agent) const PRODUCT_JAVASCRIPT_DIRECT_CANDIDATE_V1: &[u8] =
    b"plak.candidate.product-javascript.direct.v1";
pub(in crate::agent) const PYTHON_DIRECT_VERTICAL_CANDIDATE_V1: &[u8] =
    b"plak.candidate.product-python.wire-compatible-direct.v1";

pub(crate) fn attach_codex_minimal(
    builder: AgentBuilder,
    descriptor: &plakous::tools::CodexMinimalLocalCodeModeDescriptorV1,
    host_path: &Path,
) -> Result<AgentBuilder, AgentError> {
    let profile = plakous::agent::LocalCompositionProfile::cell(&descriptor.into())?;
    let artifact = plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1(host_path)?;
    let attachment = plakous::agent::LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .build()?;
    plakous::agent::attach_local_composition(builder, Some(attachment))
}

pub(crate) fn product_javascript_candidate_attachment(
    descriptor: &plakous::tools::ProductJavaScriptCompositionDescriptorV1,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
    presentation_kind: CodingPresentation,
) -> Option<plakous::agent::LocalCompositionAttachment> {
    let artifact =
        plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1(host_path).ok()?;
    let profile = plakous::agent::LocalCompositionProfile::cell(&descriptor.into()).ok()?;
    let issuer = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[PLAK_PRODUCT_EXTENSION_ISSUER_V1],
    )
    .ok()?;
    let selected = configuration.selected_identity().digest().identity_bytes();
    let effective = configuration.effective_identity().digest().identity_bytes();
    // Plak-owned custom composition candidate: not provider-native, not default.
    let qualification_candidate = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[
            PRODUCT_JAVASCRIPT_DIRECT_CANDIDATE_V1,
            &selected,
            &effective,
        ],
    )
    .ok()?;
    let evidence = plakous::agent::ProductExtensionEvidence::cell(
        &profile,
        &artifact,
        issuer,
        qualification_candidate,
        configuration,
    )
    .ok()?;
    plakous::agent::LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence)
        .coding_presentation_kind(presentation_kind)
        .build()
        .ok()
}

pub(crate) fn product_python_attachment(
    descriptor: &plakous::tools::PythonPtcDescriptorV1,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
    presentation_kind: CodingPresentation,
) -> CliResult<plakous::agent::LocalCompositionAttachment> {
    let artifact = plakous::agent::LocalCompositionEngineArtifact::monty_worker_v1(host_path)
        .map_err(|error| CliFailure::new(format!("Python host identity is invalid: {error}")))?;
    let profile = plakous::agent::LocalCompositionProfile::cell(&descriptor.into())
        .map_err(|error| CliFailure::new(format!("Python profile identity is invalid: {error}")))?;
    let issuer = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[PLAK_PRODUCT_EXTENSION_ISSUER_V1],
    )
    .map_err(|error| CliFailure::new(format!("Python issuer identity is invalid: {error}")))?;
    let selected = configuration.selected_identity().digest().identity_bytes();
    let effective = configuration.effective_identity().digest().identity_bytes();
    let qualification_candidate = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[PYTHON_DIRECT_VERTICAL_CANDIDATE_V1, &selected, &effective],
    )
    .map_err(|error| CliFailure::new(format!("Python candidate identity is invalid: {error}")))?;
    let evidence = plakous::agent::ProductExtensionEvidence::cell(
        &profile,
        &artifact,
        issuer,
        qualification_candidate,
        configuration,
    )
    .map_err(|error| CliFailure::new(format!("Python extension evidence is invalid: {error}")))?;
    plakous::agent::LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence)
        .coding_presentation_kind(presentation_kind)
        .build()
        .map_err(|error| CliFailure::new(format!("Python attachment is invalid: {error}")))
}

/// Plak's explicit non-interactive effect-authorization product policy.
#[must_use]
pub fn product_effect_policy() -> Arc<AlwaysAllowPolicy> {
    Arc::new(AlwaysAllowPolicy)
}

/// Occupancy fence for a non-`mutate` tier: deny workspace-mutating coding
/// tools (`apply_patch` and friends) but keep the grant-compiled tree tools
/// and process/shell tools. `read_only` and `execute` may exec; they still
/// cannot write files through Plak file tools. Adoption is a grant, not a
/// tier: a `read_only` supervisor may `adopt_candidate` / `discard_workspace`
/// when granted.
///
/// `ReadOnlyToolPolicy` is the wrong fence here: it denies every
/// `ToolEffect::Mutating` definition, including those topology tools.
#[derive(Clone, Copy, Debug)]
pub(in crate::agent) struct OccupancyReadOnlyPolicy {
    /// Distinguishes `execute` occupancy identity from `read_only`.
    execute_copy: bool,
}

/// The process-session tool family, by the canonical names
/// `plakous_tools::coding::PROCESS_SESSION_*` register (start, write, poll,
/// terminate). Spelled here because the CLI's pinned plakous facade surface
/// does not export the constants.
pub(in crate::agent) const PROCESS_SESSION_TOOLS: [&str; 4] = [
    "process_session_start",
    "process_session_write",
    "process_session_poll",
    "process_session_terminate",
];

pub(in crate::agent) fn is_process_session_tool(name: &str) -> bool {
    PROCESS_SESSION_TOOLS.contains(&name)
}

/// Provider-visible shell names that lower to the process-session family.
pub(in crate::agent) fn is_process_presentation_tool(name: &str) -> bool {
    matches!(
        name,
        "exec_command"
            | "run_terminal_command"
            | "Bash"
            | "write_stdin"
            | "shell"
            | "TaskStop"
            | "get_command_or_subagent_output"
            | "kill_command_or_subagent"
            | "stop"
    )
}

pub(crate) fn occupancy_read_only_allows(name: &str, effect: ToolEffect) -> bool {
    match effect {
        ToolEffect::ReadOnly => true,
        ToolEffect::Mutating => {
            matches!(
                name,
                "update_plan"
                    | "merge_plan"
                    | "submit_result"
                    | "send_agent_message"
                    | "spawn_agent"
                    | "interrupt_agent"
                    | "close_agent"
                    | "recover_agent"
                    | "adopt_candidate"
                    | "discard_workspace"
            ) || is_process_session_tool(name)
                || is_process_presentation_tool(name)
        }
        _ => false,
    }
}

impl ToolPolicy for OccupancyReadOnlyPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        let identity = if self.execute_copy {
            "plak.occupancy.execute.v5"
        } else {
            "plak.occupancy.read-only.v8"
        };
        #[allow(
            clippy::expect_used,
            reason = "occupancy policy identity is a compile-time portable literal"
        )]
        ToolPolicyRevision::new(identity, 1).expect("occupancy policy identity is portable")
    }

    fn evaluate(&self, request: plakous::tools::ToolPolicyRequest) -> ToolPolicyFuture {
        let allowed = occupancy_read_only_allows(
            request.definition().name().as_str(),
            request.definition().effect(),
        );
        let decision = if allowed {
            ToolPolicyDecision::Allowed
        } else {
            ToolPolicyDecision::Denied(ToolPolicyDenial::new(
                "occupancy fence denies workspace-mutating coding tools on this tier",
            ))
        };
        Box::pin(async move { Ok(decision) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionToolFence {
    Full,
    OccupancyReadOnly,
    /// `execute`: same allow-list as `OccupancyReadOnly`; distinct identity.
    OccupancyExecute,
}

pub(crate) fn session_tool_policy(fence: SessionToolFence) -> Arc<dyn ToolPolicy> {
    match fence {
        SessionToolFence::OccupancyReadOnly => Arc::new(OccupancyReadOnlyPolicy {
            execute_copy: false,
        }),
        SessionToolFence::OccupancyExecute => {
            Arc::new(OccupancyReadOnlyPolicy { execute_copy: true })
        }
        SessionToolFence::Full => product_effect_policy(),
    }
}

pub(crate) fn stamped_workspace_policy(
    physical_root: &Path,
    relative_cwd: &Path,
) -> CliResult<WorkspacePolicy> {
    let mut policy = WorkspacePolicy::host(physical_root, relative_cwd)
        .map_err(|_| CliFailure::new("workspace policy could not be initialized"))?;
    if let Some(stamp) = crate::topology_runtime::generation_stamp_for_workspace_root(physical_root)
    {
        policy = policy.with_generation_stamp(stamp);
    }
    Ok(policy)
}

pub(in crate::agent) fn install_occupancy_fence(copy: &Path) -> CliResult<()> {
    if crate::git_redirect_fence::is_minted_occupancy_copy(copy)
        && let Err(error) = crate::git_redirect_fence::install_for_copy(
            copy,
            crate::topology_runtime::workspace_base_for_root(copy).as_deref(),
        )
    {
        return Err(CliFailure::new(format!(
            "occupancy fence could not be installed: {error}"
        )));
    }
    Ok(())
}

/// The tier fence: `read_only` and `execute` refuse Plak file writes and
/// allow process tools; `mutate` is unfenced.
pub(crate) fn occupancy_session_fence(tier: crate::agent_tree_protocol::Tier) -> SessionToolFence {
    match tier {
        crate::agent_tree_protocol::Tier::ReadOnly => SessionToolFence::OccupancyReadOnly,
        crate::agent_tree_protocol::Tier::Execute => SessionToolFence::OccupancyExecute,
        crate::agent_tree_protocol::Tier::Mutate => SessionToolFence::Full,
    }
}

#[cfg(test)]
mod observation_policy_tests;
