//! Construction of the product coding session: the agent spec, tool
//! registries and harness profiles, the build/open/resume entry points, and
//! the session runtime bindings the product composes.

use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::{
    Agent, AgentBuilder, AgentConfig, AgentRequestPolicy, CapabilityRequirement,
    ContextSourceCatalog, ContextSourceResolver, ContextSourceSelector, GenerationLimits,
    HarnessProfileId, HarnessProfileSpec, HarnessProfileSpecBuilder,
    InstructionCompositionStrategy, ReasoningEffort, ReasoningPolicy, RecoveredTurnHandle,
    ResolvedContextSourceFact, compile_explicit_runtime_profile,
};
use plakous::model::{
    CachePolicy, CompiledProviderConfiguration, ConfiguredLanguageModel, ConfiguredTool,
    ContextAdmissionRequirement, EffectiveProviderConfiguration, LocalDurabilityDisposition,
    ModelId, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition,
    ProcessingTierDisposition, ProviderBehaviorConfiguration, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, ResponseStateDisposition, RetentionDisposition,
    SelectedProviderConfiguration, ToolChoice, ToolEffect, ToolExecutionDisposition,
    ToolPresentationMode,
};
use plakous::sessions::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, CompactReason,
    CompactSessionOutcome, CompactSessionRequest, ContextQuery, ContextStatusRequest,
    CreateSessionOutcome, CreateSessionRequest, ForkSessionOutcome, ForkSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, OpenedSession, PortableSessionHistory,
    RebindSessionOutcome, RebindSessionRequest, SessionContextStatus, SessionLocator,
    SessionOperationId, SessionRuntimeBinding, SessionTargetBinding, isolated_tool_scheduler,
};
use plakous::subscriptions::SubscriptionRequestLimits;
use plakous::subscriptions::{
    NativeProvider, NativeToolMode, ResolvedProvider, native_provider_route_selection,
};
use plakous::tools::{
    AlwaysAllowPolicy, CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1, CodingPresentation,
    CompiledSurface, CompositionRuntime, FrozenToolRegistry, InMemoryPlanState,
    PROCESS_TOOL_PROJECTION_REVISION, PlanStateIdentity, ProcessSessionTools,
    ProductJavaScriptCompositionV1, ProviderModelProjectionIdentityV2, PythonPtcPresentationV1,
    PythonPtcV1, ToolPolicy, ToolPolicyDecision, ToolPolicyDenial, ToolPolicyFuture,
    ToolPolicyRevision, ToolProjectionMapV2, ToolRegistryBuilder, WorkspacePolicy,
    plan_restoration_note, process_presentation_candidate_with_extensions_v2,
};

use crate::binding::CompositionSelection;
use crate::tools::{ProductToolExtensions, product_surface_with_fence};

use super::{
    ProductChildSessionBinder, ProductLiveCanceller, ProductMailDeliverer, ProductStateRoots,
    format_session_reopen_error, incompatible_session_message, reattach_inflight_children,
};

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

fn observation_budget_cap_bytes(effective_input_tokens: u64) -> Option<usize> {
    // The product window cap uses text-byte units; L0 still independently
    // charges one token per byte against its actual headroom.
    Some(
        usize::try_from(effective_input_tokens.saturating_mul(32) / 100)
            .unwrap_or(usize::MAX)
            .clamp(32 * 1024, 256 * 1024),
    )
}

#[cfg(test)]
mod observation_policy_tests {
    use super::observation_budget_cap_bytes;

    #[test]
    fn hosted_observation_cap_tracks_the_effective_input_window_in_text_bytes() {
        for (tokens, bytes) in [
            (1, 32_768),
            (100_000, 32_768),
            (163_840, 52_428),
            (256_000, 81_920),
            (258_400, 82_688),
            (500_000, 160_000),
            (1_000_000, 262_144),
            (u64::MAX, 262_144),
        ] {
            assert_eq!(
                observation_budget_cap_bytes(tokens),
                Some(bytes),
                "{tokens}"
            );
        }
    }
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
pub(super) const NATIVE_RUN_ROUND_INPUT_LIABILITY_CEILING: u64 = 8_000_000;

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
pub(super) const PLAK_PRODUCT_EXTENSION_ISSUER_V1: &[u8] = b"plak.product-extension.issuer.v1";
pub(super) const PRODUCT_JAVASCRIPT_DIRECT_CANDIDATE_V1: &[u8] =
    b"plak.candidate.product-javascript.direct.v1";
pub(super) const PYTHON_DIRECT_VERTICAL_CANDIDATE_V1: &[u8] =
    b"plak.candidate.product-python.wire-compatible-direct.v1";

pub(crate) fn product_javascript_candidate_attachment(
    descriptor: &plakous::tools::ProductJavaScriptCompositionDescriptorV1,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
    presentation_kind: CodingPresentation,
) -> Option<plakous::agent::LocalCompositionAttachment> {
    let artifact =
        plakous::agent::LocalCompositionEngineArtifact::rquickjs_sidecar_v1(host_path).ok()?;
    let profile =
        plakous::agent::LocalCompositionProfile::product_javascript_v1(descriptor).ok()?;
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
    let evidence = plakous::agent::ProductExtensionEvidence::product_javascript_v1(
        &profile,
        &artifact,
        plakous::agent::ProductExtensionLimits {
            max_source_bytes: 1_048_576,
            max_yield_time_ms: 10_000,
            max_output_tokens: 10_000,
        },
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
    let artifact = plakous::agent::LocalCompositionEngineArtifact::monty_pool_v1(host_path)
        .map_err(|error| CliFailure::new(format!("Python host identity is invalid: {error}")))?;
    let profile = plakous::agent::LocalCompositionProfile::python_ptc_v1(descriptor)
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
    let evidence = plakous::agent::ProductExtensionEvidence::python_ptc_v1(
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
pub(super) struct OccupancyReadOnlyPolicy {
    /// Distinguishes `execute` occupancy identity from `read_only`.
    execute_copy: bool,
}

/// The process-session tool family, by the canonical names
/// `plakous_tools::coding::PROCESS_SESSION_*` register (start, write, poll,
/// terminate). Spelled here because the CLI's pinned plakous facade surface
/// does not export the constants.
pub(super) const PROCESS_SESSION_TOOLS: [&str; 4] = [
    "process_session_start",
    "process_session_write",
    "process_session_poll",
    "process_session_terminate",
];

pub(super) fn is_process_session_tool(name: &str) -> bool {
    PROCESS_SESSION_TOOLS.contains(&name)
}

/// Provider-visible shell names that lower to the process-session family.
pub(super) fn is_process_presentation_tool(name: &str) -> bool {
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

pub(super) fn install_occupancy_fence(copy: &Path) -> CliResult<()> {
    if crate::git_redirect_fence::is_minted_occupancy_copy(copy)
        && let Err(error) = crate::git_redirect_fence::install_for_copy(copy)
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

/// Inputs for building the product coding agent (run + TUI host).
#[derive(Clone)]

pub struct ProductAgentSpec {
    /// Attached host completion channel used by the prompt compiler.
    pub prompt_host_facts: plakous_host_protocol::PromptHostFacts,
    pub provider: NativeProvider,
    pub resolved: Arc<ResolvedProvider>,
    pub effort: Option<ReasoningEffort>,
    pub effort_explicit: bool,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub plan_state_authority: String,
    pub composition: CompositionSelection,
    /// Durable `ProductAgentTree` id. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions keep the root tree id.
    pub topology_tree_id: Option<String>,
    /// Grant-compiled caller on that tree. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions use the spawned agent id.
    pub topology_caller_id: Option<String>,
    pub state_roots: ProductStateRoots,
    /// Product `compaction.enabled`; gates tool-group scheduled compact.
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    /// Product `subagents`: whether a fresh root may open a topology hub. A
    /// tree that already exists is joined regardless.
    pub topology_enabled: bool,
    /// The product home this session belongs to, when it has one.
    ///
    /// `None` is a real state — a session constructed with no product home
    /// behind it — and it is stated here rather than discovered: nothing under
    /// `agent/**` reads the environment for it. The topology, the product instruction tail and the
    /// OpenRouter binding are the three things that ask.
    pub product_home: Option<PathBuf>,
}

/// Product topology parent for an API-key hub root.
#[derive(Clone)]
pub struct ApiTopologyParent {
    /// The per-request output bound a session being reopened was created with.
    ///
    /// `None` for a new session. See `ProductOpenRouterSpec::created_output_bound`.
    /// Attached host completion channel used by the prompt compiler.
    pub prompt_host_facts: plakous_host_protocol::PromptHostFacts,
    pub created_output_bound: Option<OutputBoundDisposition>,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub plan_state_authority: String,
    pub composition: CompositionSelection,
    /// Durable `ProductAgentTree` id. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions keep the root tree id.
    pub topology_tree_id: Option<String>,
    /// Grant-compiled caller on that tree. Root sessions omit this and use
    /// [`Self::plan_state_authority`]. Child sessions use the spawned agent id.
    pub topology_caller_id: Option<String>,
    pub state_roots: ProductStateRoots,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub target: crate::binding::ApiTarget,
    /// Product `compaction.enabled`; gates tool-group scheduled compact.
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    /// Product `subagents`: whether a fresh root may open a topology hub.
    pub topology_enabled: bool,
    /// The product home this hub root belongs to, when it has one; see
    /// [`ProductAgentSpec::product_home`].
    pub product_home: Option<PathBuf>,
}

/// Spawn/mail/rejoin parent: OAuth native spec or API-key hub root.
#[derive(Clone)]
pub enum SpawnParent {
    Native(ProductAgentSpec),
    Api(ApiTopologyParent),
}

impl SpawnParent {
    pub(crate) fn from_native(spec: ProductAgentSpec) -> Self {
        Self::Native(spec)
    }

    pub(super) fn state_roots(&self) -> &ProductStateRoots {
        match self {
            Self::Native(spec) => &spec.state_roots,
            Self::Api(parent) => &parent.state_roots,
        }
    }

    /// The product's `subagents` fact for this parent.
    /// The product home behind this parent, when it has one.
    pub(super) fn product_home(&self) -> Option<&Path> {
        match self {
            Self::Native(spec) => spec.product_home.as_deref(),
            Self::Api(parent) => parent.product_home.as_deref(),
        }
    }

    pub(super) const fn topology_enabled(&self) -> bool {
        match self {
            Self::Native(spec) => spec.topology_enabled,
            Self::Api(parent) => parent.topology_enabled,
        }
    }
}

pub(super) fn topology_tree_id(spec: &ProductAgentSpec) -> &str {
    spec.topology_tree_id
        .as_deref()
        .unwrap_or(spec.plan_state_authority.as_str())
}

/// Grant-compiled caller. Root sessions (`topology_caller_id` absent) keep the
/// hub lease, but grants are keyed by genesis `root_agent_id` / durable tree
/// id — never a successor session UUID after rebind.
pub(super) fn topology_grant_caller_id<'a>(
    caller_id: Option<&'a str>,
    tree_id: Option<&'a str>,
    plan_state_authority: &'a str,
) -> &'a str {
    caller_id
        .filter(|id| !id.is_empty())
        .or_else(|| tree_id.filter(|id| !id.is_empty()))
        .unwrap_or(plan_state_authority)
}

pub(super) fn topology_caller_id(spec: &ProductAgentSpec) -> &str {
    topology_grant_caller_id(
        spec.topology_caller_id.as_deref(),
        spec.topology_tree_id.as_deref(),
        spec.plan_state_authority.as_str(),
    )
}

pub(super) fn spawn_parent_tree_id(parent: &SpawnParent) -> &str {
    match parent {
        SpawnParent::Native(spec) => topology_tree_id(spec),
        SpawnParent::Api(parent) => parent
            .topology_tree_id
            .as_deref()
            .unwrap_or(parent.plan_state_authority.as_str()),
    }
}

pub(super) fn spawn_parent_caller_id(parent: &SpawnParent) -> &str {
    match parent {
        SpawnParent::Native(spec) => topology_caller_id(spec),
        SpawnParent::Api(parent) => topology_grant_caller_id(
            parent.topology_caller_id.as_deref(),
            parent.topology_tree_id.as_deref(),
            parent.plan_state_authority.as_str(),
        ),
    }
}

pub(super) fn spawn_parent_is_root(parent: &SpawnParent) -> bool {
    match parent {
        SpawnParent::Native(spec) => spec.topology_caller_id.is_none(),
        SpawnParent::Api(parent) => parent.topology_caller_id.is_none(),
    }
}

/// Whether this parent states a product home at all.
///
/// Only the *presence* of the home is the question here — the tree store itself
/// is reached through the roots' port — so this is the fact without the
/// product type: `false` is a session with no product home, which a durable
/// open refuses and a non-durable one treats as "no topology".
pub(super) fn topology_home_stated(
    parent: &SpawnParent,
    opening_durable_session: bool,
) -> CliResult<bool> {
    match parent.product_home() {
        Some(_) => Ok(true),
        None if opening_durable_session => Err(CliFailure::new(
            "plak product home is unavailable for a durable topology open",
        )),
        None => Ok(false),
    }
}

pub(crate) fn topology_extensions_if_enabled(
    parent: &SpawnParent,
    selected_tuple: &str,
    attach: crate::topology_runtime::TopologyHubAttach,
    opening_durable_session: bool,
    acquired_owner: &mut Option<String>,
) -> CliResult<ProductToolExtensions> {
    topology_extensions_if_enabled_with_owner(
        parent,
        selected_tuple,
        attach,
        opening_durable_session,
        acquired_owner,
    )
}

fn topology_extensions_if_enabled_with_owner(
    parent: &SpawnParent,
    selected_tuple: &str,
    attach: crate::topology_runtime::TopologyHubAttach,
    opening_durable_session: bool,
    acquired_owner: &mut Option<String>,
) -> CliResult<ProductToolExtensions> {
    if attach == crate::topology_runtime::TopologyHubAttach::LookupOnly {
        if crate::topology_runtime::lookup(spawn_parent_tree_id(parent)).is_some() {
            return topology_extensions_from_hub_with_owner(
                parent,
                selected_tuple,
                attach,
                true,
                acquired_owner,
            );
        }
        // Single-agent compact recompiles the coding runtime after attach.
        // `subagents=false` never inserts a hub; LookupOnly must not fail closed.
        if spawn_parent_is_root(parent) {
            return Ok(ProductToolExtensions::empty());
        }
        return topology_extensions_from_hub_with_owner(
            parent,
            selected_tuple,
            attach,
            true,
            acquired_owner,
        );
    }
    if crate::topology_runtime::lookup(spawn_parent_tree_id(parent)).is_some() {
        return topology_extensions_from_hub_with_owner(
            parent,
            selected_tuple,
            attach,
            true,
            acquired_owner,
        );
    }
    if !topology_home_stated(parent, opening_durable_session)? {
        return Ok(ProductToolExtensions::empty());
    }
    // Through the roots' store port: whether a tree is persisted is a question
    // for whoever owns the rows, not for a path the agent builder resolves.
    let existing = match parent
        .state_roots()
        .tree_store()
        .map_err(|failure| failure.message().to_owned())
        .and_then(|store| {
            store
                .exists(spawn_parent_tree_id(parent))
                .map_err(|error| error.detail.clone())
        }) {
        Ok(existing) => existing,
        Err(error) if opening_durable_session => {
            return Err(CliFailure::recovery_required(format!(
                "known topology tree could not be opened ({error}); RecoveryRequired"
            )));
        }
        Err(_) => false,
    };
    if existing {
        return topology_extensions_from_hub_with_owner(
            parent,
            selected_tuple,
            attach,
            true,
            acquired_owner,
        );
    }
    // The gate is read live, from the file, at the moment a fresh root would
    // open a tree — not from the attach-time snapshot on the spec. A
    // `plak config set orchestration.subagents …` from another shell is in force for
    // the next `/clear`, rebind or plan recompile, and an unreadable roster
    // refuses creation.
    if opening_durable_session || !parent.state_roots().roster_source().topology_admitted()? {
        return Ok(ProductToolExtensions::empty());
    }
    topology_extensions_from_hub_with_owner(parent, selected_tuple, attach, false, acquired_owner)
}

fn topology_extensions_from_hub_with_owner(
    parent: &SpawnParent,
    selected_tuple: &str,
    attach: crate::topology_runtime::TopologyHubAttach,
    must_exist: bool,
    acquired_owner: &mut Option<String>,
) -> CliResult<ProductToolExtensions> {
    let tree_id = spawn_parent_tree_id(parent).to_owned();
    let caller_id = spawn_parent_caller_id(parent);
    let is_root = spawn_parent_is_root(parent);
    let workspace_root = match parent {
        SpawnParent::Native(spec) => spec.workspace_root.clone(),
        SpawnParent::Api(parent) => parent.workspace_root.clone(),
    };
    let selected_tuple = selected_tuple.to_owned();
    let root_account = spawn_parent_root_account(parent);
    let root_effort = spawn_parent_root_effort(parent);
    let roots = parent.state_roots();
    let roster_source = Arc::clone(roots.roster_source());
    let runtime = crate::topology_runtime::resolve_topology_runtime(
        &tree_id.clone(),
        is_root,
        attach,
        || {
            roots
                .tree_store()
                .map_err(|error| crate::agent_tree::TreeRuntimeError::new(error.message()))
        },
        roots.topology_observer(),
        move |store| {
            load_or_bootstrap_topology_tree(
                store,
                &tree_id,
                &workspace_root,
                &selected_tuple,
                root_account,
                root_effort.as_deref(),
                must_exist,
                roster_source.as_ref(),
            )
            .map_err(|error| crate::agent_tree::TreeRuntimeError::new(error.message()))
        },
    )?;
    // The resolver has acquired this lease; hub existence alone is not ownership.
    // Publish the marker before compiling extensions, which may still fail.
    if attach == crate::topology_runtime::TopologyHubAttach::AttachRoot {
        *acquired_owner = Some(spawn_parent_tree_id(parent).to_owned());
    }
    topology_extensions_from_runtime(runtime, parent, caller_id)
}

/// The account spelling the roster freezes for this parent's root.
///
/// The product owns its account names — `grok` for xAI, `claude` for
/// Anthropic — so the spelling comes from the roster source that owns the rest
/// of the roster vocabulary, not from a table in the constructor.
pub(super) fn spawn_parent_root_account(parent: &SpawnParent) -> &'static str {
    match parent {
        SpawnParent::Native(spec) => spec.state_roots.roster_source().account_name(spec.provider),
        SpawnParent::Api(parent) => parent.target.account(),
    }
}

pub(super) fn spawn_parent_root_effort(parent: &SpawnParent) -> Option<String> {
    match parent {
        SpawnParent::Native(spec) => spec.effort.map(crate::runtime::effort_label),
        SpawnParent::Api(parent) => parent.effort.map(crate::runtime::effort_label),
    }
}

/// The root's tree from `store` with the reopen law applied, or a fresh
/// genesis tree published through it. `store` is the roots' one handle and the
/// roster is the product's answer; nothing here reads a configuration file.
#[allow(
    clippy::too_many_arguments,
    reason = "one genesis binds every root fact the epoch freezes"
)]
pub(super) fn load_or_bootstrap_topology_tree(
    store: &dyn crate::agent_tree::TreeStore,
    tree_id: &str,
    workspace_root: &Path,
    selected_tuple: &str,
    root_account: &str,
    root_effort: Option<&str>,
    must_exist: bool,
    roster_source: &dyn crate::agent::RosterSource,
) -> CliResult<crate::agent_tree::ProductAgentTree> {
    let roster = roster_source.roster(root_account, selected_tuple, root_effort)?;
    let persisted = store.load(tree_id).map_err(|error| {
        if must_exist {
            CliFailure::recovery_required(format!(
                "known topology tree could not be opened ({error}); RecoveryRequired"
            ))
        } else {
            CliFailure::new(error.detail)
        }
    })?;
    if let Some(persisted) = persisted {
        let mut tree = crate::agent_tree::ProductAgentTree::reopen(persisted);
        if !tree.snapshot.digest_matches() {
            return Err(CliFailure::recovery_required(
                "orchestration snapshot digest does not match persisted roster; RecoveryRequired",
            ));
        }
        if stamp_root_account(&mut tree, root_account) {
            let acknowledged = store
                .save(&tree)
                .map_err(|error| CliFailure::new(error.detail))?;
            tree.acknowledge_revision(acknowledged);
        }
        return Ok(tree);
    }
    if must_exist {
        return Err(CliFailure::recovery_required(
            "known topology tree is missing from the product store; RecoveryRequired",
        ));
    }
    let mut tree = genesis_tree(
        tree_id,
        workspace_root,
        selected_tuple,
        root_account,
        roster,
    )?;
    let acknowledged = store
        .save(&tree)
        .map_err(|error| CliFailure::new(error.detail))?;
    tree.acknowledge_revision(acknowledged);
    Ok(tree)
}

fn genesis_tree(
    tree_id: &str,
    workspace_root: &Path,
    selected_tuple: &str,
    root_account: &str,
    roster: crate::orchestration::RosterInput,
) -> CliResult<crate::agent_tree::ProductAgentTree> {
    let writer_mode = roster.writer_mode;
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root(
        tree_id,
        tree_id,
        "root",
        workspace_root.to_path_buf(),
        selected_tuple,
        writer_mode,
    )
    .map_err(|error| CliFailure::new(error.message))?;
    tree.install_snapshot(crate::orchestration::OrchestrationSnapshot::freeze(roster));
    stamp_root_account(&mut tree, root_account);
    Ok(tree)
}

/// Persists a genesis tree for a session that has not opened yet.
///
/// A profile fork gives its new session a tree of its own, frozen on the
/// profile's roster, so the source keeps its tree, epoch and cards and still
/// reopens with exactly the tools it sealed. The new session finds this tree
/// under its own id when its tools compile.
///
/// # Errors
/// The tree store is unavailable, a tree already exists under `tree_id`, or
/// the save fails.
pub(crate) fn seed_genesis_tree(
    roots: &ProductStateRoots,
    tree_id: &str,
    workspace_root: &Path,
    selected_tuple: &str,
    root_account: &str,
    roster: crate::orchestration::RosterInput,
) -> CliResult<()> {
    let store = roots.tree_store()?;
    if store
        .exists(tree_id)
        .map_err(|error| CliFailure::new(error.detail))?
    {
        return Err(CliFailure::new(
            "a topology tree already exists for the new session",
        ));
    }
    let tree = genesis_tree(
        tree_id,
        workspace_root,
        selected_tuple,
        root_account,
        roster,
    )?;
    // This handle is dropped; the session's own open reloads the tree.
    let _revision = store
        .save(&tree)
        .map_err(|error| CliFailure::new(error.detail))?;
    Ok(())
}

pub(super) fn stamp_root_account(
    tree: &mut crate::agent_tree::ProductAgentTree,
    root_account: &str,
) -> bool {
    let root_id = tree.root_agent_id.clone();
    let empty = tree
        .node(&root_id)
        .map(|node| node.account.is_empty())
        .unwrap_or(false);
    if empty {
        tree.set_node_account_if_empty(&root_id, root_account);
    }
    empty
}

/// The rejoin spec this host parked on the hub. The hub keeps it as `Any`
/// precisely so the tree cannot read it; this is the one place that knows it
/// is a [`SpawnParent`].
pub(super) fn rejoin_parent(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
) -> Option<SpawnParent> {
    runtime
        .rejoin_spec()?
        .downcast_ref::<SpawnParent>()
        .cloned()
}

pub(super) fn topology_extensions_from_runtime(
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    parent: &SpawnParent,
    caller_id: &str,
) -> CliResult<ProductToolExtensions> {
    let caller_id = if spawn_parent_is_root(parent) {
        runtime
            .read(|tree| tree.root_agent_id.clone())
            .ok()
            .unwrap_or_else(|| caller_id.to_owned())
    } else {
        caller_id.to_owned()
    };
    let registrations = crate::agent_tree_tools::topology_tool_registrations_on(
        runtime.driver().clone(),
        caller_id,
        topology_host_ports(&runtime, Some(parent.clone()))?,
    )?;
    ProductToolExtensions::from_nested_registrations(registrations).map(|extensions| {
        extensions.with_host_facts(match parent {
            SpawnParent::Native(spec) => spec.prompt_host_facts,
            SpawnParent::Api(parent) => parent.prompt_host_facts,
        })
    })
}

/// The spelling this node's pack gives the await and interrupt slots.
///
/// It is the presentation [`CodingPresentation::admit`] would compile for
/// this parent — Grok's output and kill tools on the Grok pack, pi's `wait`
/// and `stop` on the pi pack (OpenRouter), the Plak names on every other pack
/// (spec 03 §3) — not a second provider match.
fn topology_vocabulary(
    parent: &SpawnParent,
) -> CliResult<crate::agent_tree_tools::TopologyVocabulary> {
    parent_coding_presentation(parent).map(vocabulary_for_presentation)
}

/// Await and interrupt spelling for one compiled coding presentation.
pub(crate) const fn vocabulary_for_presentation(
    presentation: CodingPresentation,
) -> crate::agent_tree_tools::TopologyVocabulary {
    match presentation {
        CodingPresentation::Grok => crate::agent_tree_tools::TopologyVocabulary::TaskTools,
        CodingPresentation::Pi => crate::agent_tree_tools::TopologyVocabulary::WaitStop,
        _ => crate::agent_tree_tools::TopologyVocabulary::Plak,
    }
}

/// The coding presentation a tree node's bound account and model literal
/// compile, read from the same route catalog a session compiles against. An
/// unattached or unenrolled pair has none.
pub(crate) fn node_coding_presentation(account: &str, model: &str) -> Option<CodingPresentation> {
    if let Some(provider) = crate::binding::PlatformProvider::for_account(account) {
        return crate::coding_routes::platform_route_presentation(provider, model).ok();
    }
    if let Some(provider) = crate::binding::oauth_provider_for_account(account) {
        return crate::coding_routes::selected_route_presentation(provider, model).ok();
    }
    (account == "openrouter").then_some(CodingPresentation::Pi)
}

/// A parent whose route the catalog cannot place is an error: its await
/// spelling is never guessed.
fn parent_coding_presentation(parent: &SpawnParent) -> CliResult<CodingPresentation> {
    match parent {
        SpawnParent::Native(spec) => CodingPresentation::admit(
            spec.resolved.configuration(),
            crate::coding_routes::product_presentation_routes()?,
        )
        .map_err(|_| CliFailure::new("native route has no enrolled coding vocabulary")),
        SpawnParent::Api(parent) => match parent.target {
            crate::binding::ApiTarget::Platform { provider } => {
                crate::coding_routes::platform_route_presentation(provider, &parent.model)
            }
            crate::binding::ApiTarget::OpenRouter { .. } => Ok(CodingPresentation::Pi),
        },
    }
}

/// The host's topology ports for `runtime`. Without a parent the tree's own
/// tools still run, but nothing that opens a child session can.
fn topology_host_ports(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    parent: Option<SpawnParent>,
) -> CliResult<crate::agent_tree_tools::TopologyHostPorts> {
    let observer = parent
        .as_ref()
        .and_then(|parent| parent.state_roots().topology_observer());
    let Some(parent) = parent else {
        return Ok(crate::agent_tree_tools::TopologyHostPorts::default());
    };
    Ok(crate::agent_tree_tools::TopologyHostPorts {
        vocabulary: topology_vocabulary(&parent)?,
        job_host: Some(parent.state_roots().job_registry().clone()),
        binder: Some(Arc::new(ProductChildSessionBinder {
            parent: parent.clone(),
        })),
        deliverer: Some(Arc::new(ProductMailDeliverer {
            parent: parent.clone(),
            runtime: Arc::clone(runtime),
        })),
        canceller: Some(Arc::new(ProductLiveCanceller {
            runtime: Arc::clone(runtime),
        })),
        recoverer: Some(Arc::new(super::recovery::ProductChildRecoverer {
            parent,
            runtime: Arc::clone(runtime),
        })),
        observer,
    })
}

pub struct TopologyInvokeReceipt {
    pub call_id: String,
    pub name: String,
    pub succeeded: bool,
    pub detail: String,
}

/// Idle JSONL `invoke_tool`: TopologyTool on the attached hub, not a model turn.
pub async fn invoke_topology_tool_on_hub(
    tree_id: &str,
    caller_id: Option<&str>,
    name: &str,
    arguments: &serde_json::Value,
    operation_id: &str,
) -> Result<TopologyInvokeReceipt, String> {
    let runtime = crate::topology_runtime::lookup(tree_id)
        .ok_or_else(|| "invoke_tool requires an attached topology hub".to_owned())?;
    let caller = runtime
        .read(|tree| {
            Ok::<_, String>(match caller_id {
                Some(id) if !id.is_empty() => {
                    tree.node(id).map_err(|error| error.message)?;
                    id.to_owned()
                }
                _ => tree.root_agent_id.clone(),
            })
        })
        .map_err(|error| error.message)??;
    let ports = topology_host_ports(&runtime, rejoin_parent(&runtime))
        .map_err(|error| error.message().to_owned())?;
    let outcome = crate::agent_tree_tools::invoke_granted_topology_tool_on(
        runtime.driver().clone(),
        caller,
        name,
        arguments,
        operation_id.to_owned(),
        ports,
    )
    .await
    .map_err(|error| error.message().to_owned())?;
    Ok(TopologyInvokeReceipt {
        call_id: operation_id.to_owned(),
        name: name.to_owned(),
        succeeded: outcome.is_success(),
        detail: outcome.joined_text(),
    })
}

pub(crate) fn coding_tool_registry(
    plan_state_authority: &str,
    composition: CompositionSelection,
    attach_native: bool,
) -> CliResult<FrozenToolRegistry> {
    coding_tool_registry_with_extensions(
        plan_state_authority,
        composition,
        attach_native,
        &ProductToolExtensions::empty(),
    )
}

pub(crate) fn coding_tool_registry_with_extensions(
    plan_state_authority: &str,
    composition: CompositionSelection,
    attach_native: bool,
    extensions: &ProductToolExtensions,
) -> CliResult<FrozenToolRegistry> {
    let identity = PlanStateIdentity::new(format!("plak.session.{plan_state_authority}"), 1)
        .map_err(|_| CliFailure::new("plan-state authority identity is invalid"))?;
    let process = Arc::new(
        ProcessSessionTools::posix_sh()
            .map_err(|_| CliFailure::new("process-session tools could not be initialized"))?,
    );
    // Every executor is a pack's own registration: the four coding packs and
    // the product extension pack. Composition markers below are enrollment for
    // a driver, not a model-visible tool, and are not a pack.
    let mut builder = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(
            Arc::new(InMemoryPlanState::with_identity(identity)),
            process,
        ),
        &mut builder,
    )
    .map_err(|_| CliFailure::new("coding tools could not be initialized"))?;
    extensions.register_into(&mut builder)?;
    match composition {
        CompositionSelection::Direct => {}
        CompositionSelection::NativePtc if attach_native => {
            CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)
                .map_err(|_| CliFailure::new("Code Mode markers could not be initialized"))?;
        }
        CompositionSelection::FollowCatalog if attach_native => {
            CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)
                .map_err(|_| CliFailure::new("Code Mode markers could not be initialized"))?;
        }
        CompositionSelection::JsPtc => {
            ProductJavaScriptCompositionV1::register_markers_into(&mut builder).map_err(|_| {
                CliFailure::new("product JavaScript markers could not be initialized")
            })?;
        }
        CompositionSelection::PyPtc => {
            PythonPtcV1::register_markers_into(&mut builder)
                .map_err(|_| CliFailure::new("product Python markers could not be initialized"))?;
        }
        CompositionSelection::FollowCatalog | CompositionSelection::NativePtc => {}
    }
    Ok(builder.freeze())
}

/// The projection identity of one resolved target; `subject` names the
/// caller in the two failure messages.
pub(crate) fn projection_target(
    configuration: &CompiledProviderConfiguration,
    subject: &str,
) -> CliResult<ProviderModelProjectionIdentityV2> {
    let effective = configuration.effective();
    let descriptor = effective.descriptor();
    let family = descriptor
        .model_family()
        .ok_or_else(|| CliFailure::new(format!("{subject} has no exact model family")))?;
    ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        family.as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|_| CliFailure::new(format!("{subject} identity is invalid")))
}

/// The model and provider configuration a node's session is constructed with,
/// and its callable surface compiled for exactly that configuration.
pub(super) struct NodeSession {
    pub(super) configured_model: ConfiguredLanguageModel,
    pub(super) configuration: CompiledProviderConfiguration,
    pub(super) presentation: CodingPresentation,
    pub(super) surface: CompiledSurface,
}

/// Compiles a node's session configuration and its callable surface together.
///
/// Direct topology extras (`direct_extras`, empty when the presentation does
/// not carry them as Direct tools) are part of the session configuration's
/// effective identity, so a tier-filtered projection compiled for the
/// unextended configuration names a target the projection admission refuses.
/// Returning both from one place keeps that pairing out of the caller's hands.
pub(super) fn node_session(
    job_config: plakous::tools::JobConfig,
    resolved: ConfiguredLanguageModel,
    registry: &FrozenToolRegistry,
    extensions: &ProductToolExtensions,
    direct_extras: &[plakous::model::ToolName],
    fence: SessionToolFence,
    runtime: Option<CompositionRuntime>,
) -> CliResult<NodeSession> {
    let configuration = compiled_configuration_with_direct_extensions(
        resolved.configuration(),
        registry,
        direct_extras,
    )?;
    let configured_model = if direct_extras.is_empty() {
        resolved
    } else {
        ConfiguredLanguageModel::from_registered(
            resolved.registered().clone(),
            configuration.clone(),
            resolved.initial_authority().clone(),
        )
        .map_err(|_| CliFailure::new("extended Direct model configuration is invalid"))?
    };
    let presentation = CodingPresentation::admit(
        &configuration,
        crate::coding_routes::product_presentation_routes()?,
    )
    .map_err(|_| CliFailure::new("native provider is unsupported"))?;
    let surface = product_surface_with_fence(
        job_config,
        presentation,
        registry.clone(),
        projection_target(&configuration, "product tool surface target")?,
        extensions,
        fence,
        runtime,
    )?;
    Ok(NodeSession {
        configured_model,
        configuration,
        presentation,
        surface,
    })
}

pub(super) fn is_codex_code_mode_candidate(provider: NativeProvider, model: &ModelId) -> bool {
    provider == NativeProvider::Codex
        && provider.enrolled_tool_mode(model.as_str()) == Some(NativeToolMode::CodeModeOnly)
}

/// Frozen processing tier for this child's template: a child runs on its own
/// card's tier, never its parent's. A node whose card is not in the current
/// epoch is Standard.
pub(super) fn spawn_roster_processing_tier(
    tree_id: &str,
    agent_id: &str,
) -> ProcessingTierDisposition {
    crate::topology_runtime::lookup(tree_id)
        .and_then(|runtime| {
            runtime
                .read(|tree| {
                    let template = tree.node(agent_id).ok()?.template.clone()?;
                    Some(tree.snapshot.binding(&template)?.processing_tier.clone())
                })
                .ok()
                .flatten()
        })
        .unwrap_or(ProcessingTierDisposition::Standard)
}

/// A spawned template carries its own composition: a template
/// that omits `composition` resolves to plakous-default for that template's
/// own account/model, never to the parent's presentation. `None` here means
/// the child has no template binding at all, not "the template said nothing".
pub(super) fn spawn_roster_composition(
    tree_id: &str,
    agent_id: &str,
) -> Option<CompositionSelection> {
    crate::topology_runtime::lookup(tree_id).and_then(|runtime| {
        runtime
            .read(|tree| {
                let node = tree.node(agent_id).ok()?;
                let template = node.template.clone()?;
                let binding = tree.snapshot.binding(&template)?;
                Some(binding.composition.map_or(
                    CompositionSelection::FollowCatalog,
                    CompositionSelection::from,
                ))
            })
            .ok()
            .flatten()
    })
}

/// Roster pin is bind authority. A child's composition comes from
/// its own template or, unpinned, from the hub. Plak js-ptc/py-ptc
/// is inherited by every unpinned child, Codex included: on a CodeModeOnly row
/// it replaces the explicit Direct presentation, as on spark.
/// Plakous-default and native-ptc are provider-bound: native-ptc is the
/// vertical's own PTC system and only Codex enrolls one (Code Mode), so both
/// are inherited only by a Codex child, and a native-ptc root can still spawn
/// a Claude or Grok class — it resolves its own presentation through
/// plakous-default instead of refusing.
pub fn spawn_child_composition(
    parent_composition: CompositionSelection,
    target_provider: NativeProvider,
    roster_composition: Option<CompositionSelection>,
) -> Result<CompositionSelection, String> {
    let child_composition = match roster_composition {
        Some(pinned) => pinned,
        None => match (parent_composition, target_provider) {
            (CompositionSelection::FollowCatalog, NativeProvider::Codex) => parent_composition,
            (CompositionSelection::FollowCatalog, _) => CompositionSelection::Direct,
            (CompositionSelection::NativePtc, NativeProvider::Codex) => parent_composition,
            (CompositionSelection::NativePtc, _) => CompositionSelection::FollowCatalog,
            _ => parent_composition,
        },
    };
    if child_composition == CompositionSelection::NativePtc
        && target_provider != NativeProvider::Codex
    {
        return Err(format!(
            "spawn child: {}; or drop `composition = \"native-ptc\"` from its [agent] template \
             so it resolves its own presentation",
            crate::binding::native_ptc_refusal(target_provider)
        ));
    }
    Ok(child_composition)
}

pub(super) fn is_exact_code_mode_attachment(
    provider: NativeProvider,
    model: &ModelId,
    native_tool_mode: Option<NativeToolMode>,
) -> bool {
    native_tool_mode == Some(NativeToolMode::CodeModeOnly)
        && is_codex_code_mode_candidate(provider, model)
}

/// Whether this row can carry the Direct coding pack, and therefore a Plak
/// js/py-ptc overlay on it. Codex `Direct` rows are Direct by catalog;
/// `CodeModeOnly` rows are Direct-capable through the explicit second
/// presentation. Hybrid `CodeMode` stays unenrolled.
pub fn direct_composition_capable(
    provider: NativeProvider,
    native_tool_mode: Option<NativeToolMode>,
) -> bool {
    match provider {
        NativeProvider::Codex => matches!(
            native_tool_mode,
            Some(NativeToolMode::Direct | NativeToolMode::CodeModeOnly)
        ),
        NativeProvider::Xai | NativeProvider::Claude => native_tool_mode.is_none(),
        _ => false,
    }
}

pub(super) fn exact_process_presentation_candidate(
    job_config: plakous::tools::JobConfig,
    provider: NativeProvider,
    resolved: &ResolvedProvider,
    configuration: &CompiledProviderConfiguration,
    registry: FrozenToolRegistry,
    extra_nested: &[plakous::model::ToolName],
) -> CliResult<ToolProjectionMapV2> {
    let route = configuration.selected().route();
    let requested = route.requested_model();
    let exact_route = native_provider_route_selection(provider, requested.clone())
        .map_err(|_| CliFailure::new("native process presentation candidate route is invalid"))?;
    if &exact_route != route || resolved.selected_model() != requested {
        return Err(CliFailure::new(
            "native process presentation candidate does not match the exact selected route",
        ));
    }
    let candidate_enrolled = provider.enrolls(requested.as_str());
    if !candidate_enrolled {
        return Err(CliFailure::new(
            "model has no exact built-in process-presentation candidate",
        ));
    }
    process_presentation_candidate_with_extensions_v2(
        job_config,
        registry,
        configuration,
        crate::coding_routes::product_presentation_routes()?,
        extra_nested,
    )
    .map_err(|error| {
        CliFailure::new(format!(
            "native process presentation candidate could not be compiled: {error}"
        ))
    })
}

/// Rebuilds selected/effective tools so Direct extras are prefix-seal covered.
///
/// Children inherit `parent.resolved` and apply grant-compiled extras at bind.
/// Topology names are never stored on the resolved provider.
pub(super) fn compiled_configuration_with_direct_extensions(
    source: &CompiledProviderConfiguration,
    registry: &FrozenToolRegistry,
    extra_nested: &[plakous::model::ToolName],
) -> CliResult<CompiledProviderConfiguration> {
    if extra_nested.is_empty() {
        return Ok(source.clone());
    }
    let mut behavior = source.selected().behavior().clone();
    for name in extra_nested {
        if behavior
            .tools
            .iter()
            .any(|tool| tool.definition().name() == name)
        {
            return Err(CliFailure::new(
                "product Direct catalog contains a duplicate tool",
            ));
        }
        let definition = registry
            .definition(name)
            .cloned()
            .ok_or_else(|| CliFailure::new("product Direct extension is not registered"))?;
        behavior.tools.push(ConfiguredTool::new(
            definition,
            ToolExecutionDisposition::Local,
            plakous::model::ToolCallerDisposition::DirectOnly,
        ));
    }
    behavior
        .tools
        .sort_by(|left, right| left.definition().name().cmp(right.definition().name()));
    let selected =
        SelectedProviderConfiguration::new(source.selected().route().clone(), behavior.clone())
            .map_err(|_| CliFailure::new("extended Direct provider behavior is invalid"))?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        source.effective().descriptor().clone(),
        source.effective().model_implementation().clone(),
        source.effective().authority().clone(),
        behavior,
        source.effective().family_policy().clone(),
    )
    .map_err(|_| CliFailure::new("extended Direct provider configuration could not be compiled"))?;
    CompiledProviderConfiguration::new(selected, effective)
        .map_err(|_| CliFailure::new("extended Direct provider configuration is invalid"))
}

/// `pub(crate)` so rendered prompt/wire goldens can compile the same harness
/// spec offline. Production behaviour is unchanged.
pub(crate) fn selected_harness_profile(
    provider: NativeProvider,
    selected_model: &ModelId,
    effort: impl Into<Option<ReasoningEffort>>,
    opening_durable_session: bool,
    code_mode: bool,
    extra_nested: &[plakous::model::ToolName],
    subagents: bool,
    composition: CompositionSelection,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<Option<HarnessProfileSpec>> {
    let effort = effort.into();
    if opening_durable_session {
        return Ok(None);
    }
    if code_mode && provider != NativeProvider::Codex {
        return Err(CliFailure::new(
            "Code Mode harness profile is valid only for Codex",
        ));
    }
    let variant = product_prompt_variant();
    let profile = match provider {
        NativeProvider::Xai => plakous::profiles::grok::recommended_for(variant)
            .map_err(|_| CliFailure::new("grok harness profile is invalid"))?,
        NativeProvider::Codex if code_mode => {
            let mut names = CodexMinimalLocalCodeModeV1::nested_names().to_vec();
            names.retain(|name| {
                surface
                    .exposed_names()
                    .iter()
                    .any(|exposed| exposed.as_str() == *name)
            });
            names.extend(extra_nested.iter().map(plakous::model::ToolName::as_str));
            plakous::profiles::codex::code_mode_for(variant, &names)
                .map_err(|_| CliFailure::new("Codex Code Mode harness profile is invalid"))?
        }
        NativeProvider::Codex => plakous::profiles::codex::recommended_for(variant)
            .map_err(|_| CliFailure::new("Codex harness profile is invalid"))?,
        NativeProvider::Claude => plakous::profiles::claude::recommended_for(variant)
            .map_err(|_| CliFailure::new("claude harness profile is invalid"))?,
        _ => {
            return Err(CliFailure::new("native provider is unsupported"));
        }
    };
    let profile = match effort {
        Some(effort) => profile
            .with_preferred_reasoning_effort(effort)
            .map_err(|_| CliFailure::new("harness profile effort override is invalid"))?,
        None => profile,
    };
    product_identity_profile(
        &profile,
        provider,
        selected_model,
        subagents,
        composition,
        surface,
        workspace_root,
        cwd,
    )
    .map(Some)
}

/// The one place `PLAK_PROMPT_VARIANT` is read. The prompts crate takes the
/// variant as an argument everywhere, so the environment is consulted here,
/// at the product layer, and never under construction (spec 01 §2.1).
pub(crate) fn product_prompt_variant() -> plakous::prompts::bases::PromptVariant {
    plakous::prompts::bases::PromptVariant::from_env()
}

fn composition_profile_id(base: &HarnessProfileId, suffix: &str) -> CliResult<HarnessProfileId> {
    HarnessProfileId::new(format!("{}{suffix}", base.as_str()))
        .map_err(|_| CliFailure::new("product composition profile id is invalid"))
}

pub(crate) fn subagent_catalog_profile_id(
    base: &str,
    subagents: bool,
) -> CliResult<HarnessProfileId> {
    let id = if subagents {
        format!("{base}.subagents.v1")
    } else {
        base.to_owned()
    };
    HarnessProfileId::new(id).map_err(|_| CliFailure::new("subagent catalog profile id is invalid"))
}

pub(super) fn product_identity_profile(
    profile: &HarnessProfileSpec,
    provider: NativeProvider,
    selected_model: &ModelId,
    subagents: bool,
    composition: CompositionSelection,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<HarnessProfileSpec> {
    match provider {
        NativeProvider::Codex | NativeProvider::Xai | NativeProvider::Claude => {}
        _ => return Err(CliFailure::new("native provider is unsupported")),
    }
    product_target_identity_profile(
        profile,
        selected_model.as_str(),
        subagents,
        composition,
        surface,
        workspace_root,
        cwd,
    )
}

/// The product's first-turn instruction profile for one target: the family
/// base (or the PTC composition's replacement of it) with the identity tail
/// and the paragraphs the compiled surface earned, assembled once by
/// `prompts::plan::target_instructions`. A PTC composition also suffixes the
/// profile id (`.js-ptc.v1` / `.py-ptc.v1`) so the checkpoint plan names the
/// composition it was compiled for.
pub(crate) fn product_target_identity_profile(
    profile: &HarnessProfileSpec,
    selected_model: &str,
    subagents: bool,
    composition: CompositionSelection,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<HarnessProfileSpec> {
    let profile_id = subagent_catalog_profile_id(profile.id().as_str(), subagents)?;
    let profile_id = match composition {
        CompositionSelection::JsPtc => composition_profile_id(&profile_id, ".js-ptc.v1")?,
        CompositionSelection::PyPtc => composition_profile_id(&profile_id, ".py-ptc.v1")?,
        CompositionSelection::Direct
        | CompositionSelection::FollowCatalog
        | CompositionSelection::NativePtc => profile_id,
    };
    // The environment block states the effort the profile asked for, the way
    // Codex states it in its turn context. The label is the prompts crate's
    // word, not this crate's.
    let selected_effort = profile
        .preferred_reasoning_effort()
        .map(plakous::prompts::identity::product_effort_label);
    let base_instructions = plakous::prompts::plan::target_instructions(
        profile.base_instructions(),
        crate::prompt_inputs::prompt_composition(composition),
        surface,
        workspace_root,
        cwd,
        selected_model,
        selected_effort.as_deref(),
    );
    let mut builder = HarnessProfileSpecBuilder::new()
        .id(profile_id)
        .semantic_version(profile.semantic_version())
        .provenance(profile.provenance())
        .base_instructions(base_instructions)
        .instruction_strategy(profile.instruction_strategy())
        .instruction_sources(profile.instruction_sources().to_vec())
        .tool_exposure_strategy(profile.tool_exposure_strategy())
        .tool_overrides(profile.tool_overrides().to_vec())
        .reasoning_strategy(profile.reasoning_strategy())
        .preferred_reasoning_effort(profile.preferred_reasoning_effort())
        .continuation_strategy(profile.continuation_strategy())
        .cache_strategy(profile.cache_strategy());
    if let Some(display_name) = profile.display_name() {
        builder = builder.display_name(display_name);
    }
    builder
        .build()
        .map_err(|error| CliFailure::new(format!("product identity profile is invalid: {error}")))
}

pub(crate) fn enroll_product_instructions(
    profile: &HarnessProfileSpec,
    product_home: Option<&Path>,
    workspace_root: &Path,
    cwd: &Path,
) -> CliResult<(HarnessProfileSpec, Vec<ResolvedContextSourceFact>)> {
    let discovery = plakous::prompts::instructions::discover_instruction_sources(
        product_home,
        workspace_root,
        cwd,
    )
    .map_err(|error| CliFailure::new(format!("product instructions are invalid: {error}")))?;
    if discovery.sources().is_empty() {
        return Ok((profile.clone(), Vec::new()));
    }

    let mut catalog = ContextSourceCatalog::new();
    let mut selectors = Vec::with_capacity(discovery.sources().len());
    for source in discovery.sources() {
        let registered = source.registered_source().map_err(|error| {
            CliFailure::new(format!(
                "instruction source {} could not be registered: {error}",
                source.source_id()
            ))
        })?;
        selectors.push(
            ContextSourceSelector::new(registered.source_id()).map_err(|error| {
                CliFailure::new(format!(
                    "instruction source {} has an invalid selector: {error}",
                    source.source_id()
                ))
            })?,
        );
        catalog.insert(registered).map_err(|error| {
            CliFailure::new(format!("instruction source catalog is invalid: {error}"))
        })?;
    }
    let resolved = ContextSourceResolver::new(catalog)
        .resolve(&selectors)
        .map_err(|error| {
            CliFailure::new(format!("instruction sources could not resolve: {error}"))
        })?;

    let mut builder = HarnessProfileSpecBuilder::new()
        .id(profile.id().clone())
        .semantic_version(profile.semantic_version())
        .provenance(profile.provenance())
        .base_instructions(profile.base_instructions())
        .instruction_strategy(InstructionCompositionStrategy::BaseThenSources)
        .instruction_sources(selectors)
        .tool_exposure_strategy(profile.tool_exposure_strategy())
        .tool_overrides(profile.tool_overrides().to_vec())
        .reasoning_strategy(profile.reasoning_strategy())
        .preferred_reasoning_effort(profile.preferred_reasoning_effort())
        .continuation_strategy(profile.continuation_strategy())
        .cache_strategy(profile.cache_strategy());
    if let Some(display_name) = profile.display_name() {
        builder = builder.display_name(display_name);
    }
    let enrolled = builder
        .build()
        .map_err(|error| CliFailure::new(format!("instruction profile is invalid: {error}")))?;
    Ok((enrolled, resolved))
}

pub struct CodingSessionOpenFailure {
    pub failure: CliFailure,
    pub current_locator: Option<SessionLocator>,
}

impl From<CliFailure> for CodingSessionOpenFailure {
    fn from(failure: CliFailure) -> Self {
        Self {
            failure,
            current_locator: None,
        }
    }
}

impl From<AgentSessionsError> for CodingSessionOpenFailure {
    fn from(error: AgentSessionsError) -> Self {
        Self {
            failure: CliFailure::new(
                if error.kind() == AgentSessionsErrorKind::IncompatibleBuild {
                    incompatible_session_message(&error)
                } else {
                    error.to_string()
                },
            ),
            current_locator: error.current_locator(),
        }
    }
}

/// An attachment release can fail in its children, its canonical parent, or both.
/// A successfully released parent locator remains available for publication.
#[derive(Debug)]
pub struct AttachedReleaseError {
    released_locator: Option<SessionLocator>,
    topology: Option<crate::topology_runtime::TopologyShutdownError>,
    canonical: Option<AgentSessionsError>,
}
impl AttachedReleaseError {
    /// The parent's released head, even when owned child shutdown failed.
    pub fn released_locator(&self) -> Option<SessionLocator> {
        self.released_locator
    }
}
impl std::fmt::Display for AttachedReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = &self.topology {
            write!(f, "{error}")?;
        }
        if let Some(error) = &self.canonical {
            if self.topology.is_some() {
                write!(f, "; ")?;
            }
            write!(f, "canonical session release failed: {error}")?;
        }
        Ok(())
    }
}
impl std::error::Error for AttachedReleaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.canonical
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
            .or_else(|| {
                self.topology
                    .as_ref()
                    .map(|error| error as &(dyn std::error::Error + 'static))
            })
    }
}

/// The one line a reopen refusal gives an operator.
///
/// A holder this host can prove is gone has its branch reclaimed at once, so
/// reaching here means the opposite: the holder is alive, or it is somewhere
/// this process cannot speak for, and the lease's own clock is the only
/// remaining answer. Without saying so the refusal reads as a broken session
/// and the user stops trying, when the remedy is to wait. The lease is the
/// store's, so the wait is an upper bound rather than a countdown: the holder
/// renews for at most that long.
pub(super) fn reopen_refusal(reason: &str, retry_after_seconds: Option<u64>) -> CliFailure {
    retry_after_seconds.map_or_else(
        || CliFailure::recovery_required(reason.to_owned()),
        |seconds| {
            CliFailure::recovery_required(format!(
                "{reason}: this session's branch is held by another plak process that \
                 could not be confirmed gone; retry in about {seconds} seconds"
            ))
        },
    )
}

/// One live coding session together with the exact public workflow service
/// that created or reopened it.
///
/// Retaining `AgentSessions` is required for driver-owned compact/fork/rebind
/// workflows. This wrapper composes that public authority; it does not own a
/// second agent loop or persistence implementation.
pub struct AttachedCodingSession {
    pub(super) reachability_probe: Option<plakous::model::ReachabilityProbe>,
    /// Set when this reopen kept a budget the route has since moved. Reported
    /// once at session open, never during a turn.
    pub(crate) output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
    pub(super) sessions: Arc<AgentSessions>,
    pub(super) opened: Option<OpenedSession>,
    pub(super) recovered_turn: Option<RecoveredTurnHandle>,
    pub(super) locator: SessionLocator,
    /// Tree id when this session holds a root hub lease. Child/mail sessions
    /// never own the hub; their `release` must not drop the working copy.
    pub(super) topology_owner: Option<String>,
}

impl AttachedCodingSession {
    pub(crate) fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        self.reachability_probe.clone()
    }

    #[cfg(test)]
    pub(crate) fn opened_for_host_tests(
        sessions: Arc<AgentSessions>,
        session: OpenedSession,
    ) -> Self {
        Self {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn claim_injected_topology_owner(&mut self, tree_id: &str) {
        self.topology_owner = claim_root_topology_owner(tree_id, true);
    }

    pub fn agent(&self) -> Option<&Agent> {
        self.opened.as_ref().map(OpenedSession::agent)
    }

    /// Unix milliseconds of the newest operator prompt or steer in the
    /// attached session's committed history; `None` when detached.
    pub(crate) fn last_operator_input_unix_ms(&self) -> Option<u64> {
        self.opened
            .as_ref()
            .and_then(OpenedSession::last_operator_input_unix_ms)
    }

    pub fn locator(&self) -> SessionLocator {
        self.opened
            .as_ref()
            .map_or(self.locator, OpenedSession::locator)
    }

    #[must_use]
    pub fn sessions(&self) -> &AgentSessions {
        self.sessions.as_ref()
    }

    pub(crate) fn close_admission(&self) {
        if let Some(opened) = &self.opened {
            opened.close_admission();
        }
    }

    pub(crate) fn shared_sessions(&self) -> Arc<AgentSessions> {
        Arc::clone(&self.sessions)
    }

    pub(crate) fn schedule_passive_checkpoint(&self) {
        let sessions = Arc::clone(&self.sessions);
        tokio::spawn(async move {
            let _ = sessions.passive_checkpoint().await;
        });
    }

    /// Takes the sole result/control authority for a root turn restored by
    /// exact open. A caller must settle this turn before admitting another.
    pub(crate) fn take_recovered_turn(&mut self) -> Option<RecoveredTurnHandle> {
        self.recovered_turn.take()
    }

    pub(crate) const fn has_recovered_turn(&self) -> bool {
        self.recovered_turn.is_some()
    }

    pub(crate) async fn create_with_runtime(
        &self,
        operation_id: SessionOperationId,
        runtime: SessionRuntimeBinding,
    ) -> Result<Self, CodingSessionOpenFailure> {
        match self
            .sessions
            .create(CreateSessionRequest::new(operation_id, runtime))
            .await
            .map_err(|error| {
                let raw = format!(
                    "agent could not be initialized ({:?}): {error}",
                    error.kind()
                );
                CliFailure::new(
                    crate::storage_pressure::sessions_storage_pressure(&error, &raw).unwrap_or(raw),
                )
            })? {
            CreateSessionOutcome::Created { session, .. } => {
                let locator = session.locator();
                Ok(Self {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&self.sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                })
            }
            _ => Err(CliFailure::new("agent session creation was unavailable").into()),
        }
    }

    pub(crate) fn clear_job_counter(
        &self,
        runtime: SessionRuntimeBinding,
    ) -> CliResult<SessionRuntimeBinding> {
        let source = self
            .agent()
            .ok_or_else(|| CliFailure::new("clear source agent is unavailable"))?;
        runtime.with_job_counter(source).map_err(|error| {
            CliFailure::new(format!("clear job counter could not be seeded: {error}"))
        })
    }

    /// Forks onto `runtime`'s compilation of this binding (current
    /// instruction files, the source's history).
    pub async fn fork(
        &self,
        operation_id: SessionOperationId,
        runtime: SessionRuntimeBinding,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        Box::pin(self.sessions.fork(ForkSessionRequest::recompiling(
            operation_id,
            self.locator(),
            runtime,
        )))
        .await
    }

    /// An exact ordinary fork of `source`; product forks recompile
    /// ([`Self::fork`]).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn fork_from(
        &self,
        operation_id: SessionOperationId,
        source: SessionLocator,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        self.sessions
            .fork(ForkSessionRequest::new(operation_id, source))
            .await
    }

    pub async fn compact_from(
        &self,
        operation_id: SessionOperationId,
        source: SessionLocator,
        reason: CompactReason,
        pending_user_input: Option<String>,
        runtime: SessionRuntimeBinding,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        self.sessions
            .compact({
                let request = CompactSessionRequest::new(operation_id, source, reason, runtime);
                match pending_user_input {
                    Some(input) => request.with_pending_user_input(input),
                    None => request,
                }
            })
            .await
    }

    pub async fn rebind_from(
        &self,
        operation_id: SessionOperationId,
        source: SessionLocator,
        target: SessionTargetBinding,
        runtime: SessionRuntimeBinding,
    ) -> Result<RebindSessionOutcome, AgentSessionsError> {
        Box::pin(
            self.sessions
                .rebind(RebindSessionRequest::recompiling_target(
                    operation_id,
                    source,
                    target,
                    runtime,
                )),
        )
        .await
    }

    pub async fn context_status(
        &self,
        operation_id: SessionOperationId,
        query: ContextQuery,
    ) -> Result<SessionContextStatus, AgentSessionsError> {
        self.sessions
            .context_status(ContextStatusRequest::new(
                operation_id,
                self.locator(),
                query,
            ))
            .await
    }

    #[allow(
        clippy::result_large_err,
        reason = "the error carries the released locator and both lease failures; it is returned once at session teardown"
    )]
    pub async fn release(mut self) -> Result<SessionLocator, AttachedReleaseError> {
        let topology = match self.topology_owner.take() {
            Some(tree_id) => crate::topology_runtime::release_root_lease(&tree_id)
                .await
                .err(),
            None => None,
        };
        // A child's failure cannot retain the parent's canonical lease.
        match self.release_opened().await {
            Ok(locator) if topology.is_none() => Ok(locator),
            Ok(locator) => Err(AttachedReleaseError {
                released_locator: Some(locator),
                topology,
                canonical: None,
            }),
            Err(canonical) => Err(AttachedReleaseError {
                released_locator: canonical.released_locator(),
                topology,
                canonical: Some(canonical),
            }),
        }
    }

    /// Process-exit release: no hub retirement, product saves or driver join.
    #[allow(
        clippy::result_large_err,
        reason = "the error carries the released locator and both lease failures; it is returned once at process exit"
    )]
    pub async fn abandon_and_release(
        mut self,
        job_budget: Duration,
    ) -> Result<SessionLocator, AttachedReleaseError> {
        let peers = match self.topology_owner.take() {
            Some(tree_id) => crate::topology_runtime::abandon_live_agents(&tree_id).await,
            None => Vec::new(),
        };
        match self.opened.take() {
            Some(opened) => opened
                .abandon_and_release(&peers, job_budget)
                .await
                .map_err(|canonical| AttachedReleaseError {
                    released_locator: canonical.released_locator(),
                    topology: None,
                    canonical: Some(canonical),
                }),
            None => Ok(self.locator),
        }
    }

    /// Takes over a released predecessor's agent-tree lease when this attach
    /// did not take one of its own, so replacing one attachment of a session
    /// with another never drops the tree between them.
    pub(crate) fn inherit_topology_owner(&mut self, previous: &mut Self) {
        if self.topology_owner.is_none() {
            self.topology_owner = previous.topology_owner.take();
        }
    }

    pub(crate) async fn release_opened(mut self) -> Result<SessionLocator, AgentSessionsError> {
        match self.opened.take() {
            Some(opened) => opened.release().await,
            None => Ok(self.locator),
        }
    }

    pub(crate) async fn release_opened_for_transfer(
        mut self,
    ) -> Result<SessionLocator, AgentSessionsError> {
        match self.opened.take() {
            Some(opened) => opened.release_for_transfer().await,
            None => Ok(self.locator),
        }
    }

    pub(crate) async fn detach_for_transfer(
        &mut self,
    ) -> Result<SessionLocator, AgentSessionsError> {
        let Some(opened) = self.opened.take() else {
            return Ok(self.locator);
        };
        self.locator = opened.locator();
        let locator = opened.release_for_transfer().await?;
        self.locator = locator;
        Ok(locator)
    }

    /// Quiesces the live driver and releases its source lease while retaining
    /// the session workflow service needed to publish a canonical branch.
    pub async fn detach(&mut self) -> Result<SessionLocator, AgentSessionsError> {
        let Some(opened) = self.opened.take() else {
            return Ok(self.locator);
        };
        self.locator = opened.locator();
        let locator = opened.release().await?;
        self.locator = locator;
        Ok(locator)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn stub_for_host_tests(sessions: Arc<AgentSessions>, locator: SessionLocator) -> Self {
        Self {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            opened: None,
            recovered_turn: None,
            locator,
            topology_owner: None,
        }
    }
}

/// Materializes Plak's complete explicit provider selection before any live
/// model resolution occurs.
pub fn selected_coding_provider_configuration(
    provider: NativeProvider,
    model: ModelId,
    effort: impl Into<Option<ReasoningEffort>>,
    native_tool_mode: Option<NativeToolMode>,
    composition: CompositionSelection,
) -> CliResult<SelectedProviderConfiguration> {
    selected_coding_provider_configuration_with_processing_tier(
        provider,
        model,
        effort.into(),
        native_tool_mode,
        composition,
        ProcessingTierDisposition::Standard,
    )
}

/// Materializes an exact provider selection with an independently selected
/// processing tier. Product admission must validate named tiers against the
/// authenticated catalog before calling this constructor.
pub fn selected_coding_provider_configuration_with_processing_tier(
    provider: NativeProvider,
    model: ModelId,
    effort: Option<ReasoningEffort>,
    native_tool_mode: Option<NativeToolMode>,
    composition: CompositionSelection,
    processing_tier: ProcessingTierDisposition,
) -> CliResult<SelectedProviderConfiguration> {
    if provider == NativeProvider::Codex && effort == Some(ReasoningEffort::Ultra) {
        return Err(CliFailure::new(
            "Codex Ultra is a multi-agent orchestration mode, not a single-agent reasoning effort",
        ));
    }
    let candidate_enrolled = provider.enrolls(model.as_str());
    if !candidate_enrolled {
        return Err(CliFailure::new(
            "model has no exact built-in process-presentation candidate",
        ));
    }
    let route = native_provider_route_selection(provider, model.clone())
        .map_err(|_| CliFailure::new("native provider route is invalid"))?;
    let code_mode_candidate = match (provider, native_tool_mode, composition) {
        (_, _, CompositionSelection::NativePtc)
            if !matches!(
                (provider, native_tool_mode),
                (NativeProvider::Codex, Some(NativeToolMode::CodeModeOnly))
            ) =>
        {
            return Err(CliFailure::new(crate::binding::native_ptc_refusal(
                provider,
            )));
        }
        (NativeProvider::Codex, Some(NativeToolMode::Direct), _) => false,
        (
            NativeProvider::Codex,
            Some(NativeToolMode::CodeModeOnly),
            CompositionSelection::NativePtc | CompositionSelection::FollowCatalog,
        ) => {
            if !is_codex_code_mode_candidate(provider, route.requested_model()) {
                return Err(CliFailure::new(
                    "Codex model requires CodeModeOnly but has no exact enrolled local composition presentation",
                ));
            }
            true
        }
        // explicit Direct on a CodeModeOnly row is a second enrolled
        // presentation (no `exec`/`wait`), and Plak js/py-ptc may replace that
        // Direct presentation exactly as on a catalog-Direct row. Plak PTC never
        // attaches *beside* Code Mode because this arm compiles no Code Mode.
        (
            NativeProvider::Codex,
            Some(NativeToolMode::CodeModeOnly),
            CompositionSelection::Direct
            | CompositionSelection::JsPtc
            | CompositionSelection::PyPtc,
        ) => false,
        (NativeProvider::Codex, Some(NativeToolMode::CodeMode), _) => {
            return Err(CliFailure::new(
                "Codex hybrid CodeMode is catalog-admitted but has no exact enrolled product presentation",
            ));
        }
        (NativeProvider::Codex, None, _) => {
            return Err(CliFailure::new(
                "Codex model has no recognized authenticated tool-mode authority",
            ));
        }
        (_, None, _) => false,
        (_, Some(_), _) => {
            return Err(CliFailure::new(
                "native tool-mode evidence belongs to a different provider family",
            ));
        }
    };
    let product_javascript = composition_is_js_ptc(composition);
    let product_python = composition_is_py_ptc(composition);
    if (product_javascript || product_python)
        && !direct_composition_capable(provider, native_tool_mode)
    {
        return Err(CliFailure::new(
            "Plak-owned custom composition requires exact Direct-capable catalog authority",
        ));
    }
    let registry = coding_tool_registry("provider-selection", composition, code_mode_candidate)?;
    let catalog = crate::coding_routes::selected_route_presentation(provider, model.as_str())?;
    let outer_selection = if product_javascript || product_python {
        composition
    } else if code_mode_candidate {
        CompositionSelection::NativePtc
    } else {
        CompositionSelection::Direct
    };
    let selected_names = outer_selection
        .outer_marker_names()
        .unwrap_or_else(|| catalog.configured_canonical_names());
    let mut tools = selected_names
        .iter()
        .map(|name| {
            let definition = registry
                .definitions()
                .iter()
                .find(|definition| definition.name().as_str() == *name)
                .cloned()
                .ok_or_else(|| CliFailure::new("coding catalog capability is not registered"))?;
            Ok(ConfiguredTool::new(
                definition,
                ToolExecutionDisposition::Local,
                plakous::model::ToolCallerDisposition::DirectOnly,
            ))
        })
        .collect::<CliResult<Vec<_>>>()?;
    tools.sort_by(|left, right| left.definition().name().cmp(right.definition().name()));
    let (reasoning_summary, reasoning_include, replay, parallel_tools, output_bound, storage) =
        match provider {
            // Codex asks for `reasoning.summary: "auto"` like the official
            // client whenever an effort is on the wire; without an effort the
            // request carries no reasoning object and so no summary either
            // (the dialect's behaviour check expects exactly that pairing).
            NativeProvider::Codex => (
                if effort.is_some() {
                    ReasoningSummaryDisposition::Auto
                } else {
                    ReasoningSummaryDisposition::Omit
                },
                ReasoningIncludeDisposition::EncryptedContent,
                ReplayDisposition::StrictEncrypted,
                // Every release-enrolled CodeModeOnly row in the reviewed
                // 0.160.1 reference catalog uses Responses Lite, whose descriptor does
                // not advertise ParallelTools. A future catalog change may
                // expose it, but disabling parallel calls remains safe.
                if native_tool_mode == Some(NativeToolMode::CodeModeOnly) {
                    ParallelToolDisposition::Disabled
                } else {
                    ParallelToolDisposition::Enabled
                },
                OutputBoundDisposition::NoRequestBound,
                ProviderStorageDisposition::DisabledOnWire,
            ),
            NativeProvider::Xai => (
                ReasoningSummaryDisposition::Concise,
                ReasoningIncludeDisposition::EncryptedContent,
                ReplayDisposition::StrictEncrypted,
                ParallelToolDisposition::Disabled,
                OutputBoundDisposition::Enforced(
                    plakous::xai::grok_max_output_tokens(model.as_str())
                        .map(|ceiling| ceiling.min(RESPONSE_OUTPUT_CEILING))
                        .and_then(NonZeroU32::new)
                        .ok_or(CliFailure::new("Grok model has no enrolled output ceiling"))?,
                ),
                ProviderStorageDisposition::DisabledOnWire,
            ),
            NativeProvider::Claude => (
                ReasoningSummaryDisposition::Omit,
                ReasoningIncludeDisposition::Omit,
                ReplayDisposition::StrictEncrypted,
                ParallelToolDisposition::Enabled,
                OutputBoundDisposition::Enforced(
                    NonZeroU32::new(plakous::anthropic::claude_max_output_tokens(model.as_str()))
                        .ok_or(CliFailure::new("Claude output limit is invalid"))?,
                ),
                ProviderStorageDisposition::OmittedRouteContract,
            ),
            _ => return Err(CliFailure::new("native coding provider is unsupported")),
        };
    let retention = RetentionDisposition::RouteContract {
        profile: route.integration_profile().clone(),
        revision: route.integration_revision().clone(),
    };
    SelectedProviderConfiguration::new(
        route,
        ProviderBehaviorConfiguration {
            processing_tier,
            // Provider projection and harness-local composition are separate
            // axes. Native Codex Code Mode is a free-form outer composition
            // surface; Plak-owned JavaScript/Python projections are ordinary
            // structured function tools on a Direct-capable provider route.
            tool_mode: if code_mode_candidate {
                ToolPresentationMode::LocalCompositionOnly
            } else {
                ToolPresentationMode::Direct
            },
            reasoning_effort: effort.map_or(
                ReasoningEffortDisposition::Disabled,
                ReasoningEffortDisposition::Enabled,
            ),
            reasoning_summary,
            reasoning_include,
            replay,
            cache: match provider {
                NativeProvider::Claude => CachePolicy::Enabled {
                    requirement: CapabilityRequirement::Required,
                },
                NativeProvider::Codex | NativeProvider::Xai | _ => CachePolicy::Disabled,
            },
            hosted_tools: Vec::new(),
            tools,
            tool_choice: ToolChoice::Auto,
            parallel_tools,
            output: OutputPolicy::default(),
            output_bound,
            storage,
            retention,
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::DurableRequired,
        },
    )
    .map_err(|_| CliFailure::new("native provider behavior is invalid"))
}

/// Builds and returns the attached public Plakous session authority.
///
/// Product code that persists or later reopens a conversation must retain this
/// handle while live and persist its current locator.
pub async fn build_coding_session(
    spec: ProductAgentSpec,
    create_operation_id: &str,
    seed: Option<PortableSessionHistory>,
    stderr: &mut dyn Write,
) -> CliResult<AttachedCodingSession> {
    let operation_id = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("plak create operation identity is invalid"))?;
    Box::pin(build_or_open_coding_session(
        spec,
        None,
        seed,
        operation_id,
        stderr,
    ))
    .await
    .map_err(|error| error.failure)
}

/// Reopens one exact product-bound Plakous session without substituting current
/// defaults for its persisted runtime authority.
pub async fn open_coding_session(
    spec: ProductAgentSpec,
    locator: SessionLocator,
    stderr: &mut dyn Write,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let operation_id = SessionOperationId::new(format!("plak:open:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("plak open operation identity is invalid"))?;
    Box::pin(build_or_open_coding_session(
        spec,
        Some(locator),
        None,
        operation_id,
        stderr,
    ))
    .await
}

/// Creates a durable product session from an already sealed public Plakous
/// runtime binding. This is the provider-neutral lane used by API-key dialects.
pub async fn build_runtime_session(
    state_roots: &ProductStateRoots,
    runtime: SessionRuntimeBinding,
    create_operation_id: &str,
) -> CliResult<AttachedCodingSession> {
    let operation_id = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("plak create operation identity is invalid"))?;
    build_or_open_runtime_session(
        state_roots.runtime_sessions()?,
        runtime,
        None,
        None,
        operation_id,
        None,
        None,
    )
    .await
    .map_err(|error| error.failure)
}

pub(super) async fn build_or_open_coding_session(
    mut spec: ProductAgentSpec,
    locator: Option<SessionLocator>,
    seed: Option<PortableSessionHistory>,
    operation_id: SessionOperationId,
    stderr: &mut dyn Write,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let sessions = spec
        .state_roots
        .runtime_sessions()
        .map_err(CodingSessionOpenFailure::from)?;
    // Attachment catalogs are identity-bearing, so restore bounds before compilation.
    if let Some(locator) = locator {
        spec.state_roots = spec
            .state_roots
            .for_exact_reopen(&sessions, locator)
            .await?;
    } else {
        spec.state_roots = spec.state_roots.without_reopen_artifact();
    }
    let tree_id = topology_tree_id(&spec).to_owned();
    let is_root = spec.topology_caller_id.is_none();
    let reattach_spec = spec.clone();
    let reattach_parent = SpawnParent::from_native(reattach_spec.clone());
    let runtime = match coding_session_runtime_binding_for(
        spec,
        stderr,
        locator.is_some(),
        SessionToolFence::Full,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        &mut None,
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Err(cleanup) = crate::topology_runtime::release_root_lease(&tree_id).await {
                return Err(CodingSessionOpenFailure::from(
                    error.with_cleanup_failure(cleanup),
                ));
            }
            return Err(CodingSessionOpenFailure::from(error));
        }
    };
    let topology_owner = claim_root_topology_owner(&tree_id, is_root);
    if let Some(topology) = crate::topology_runtime::lookup(&tree_id) {
        topology.remember_rejoin_spec(Arc::new(reattach_parent.clone()));
        super::recovery::install_retirement_recovery(&topology, &tree_id).await;
    }
    // A reattach recompiles nothing, so it has no moved budget to report.
    let result = build_or_open_runtime_session(
        sessions,
        runtime,
        locator,
        seed,
        operation_id,
        topology_owner,
        None,
    )
    .await;
    if result.is_err() {
        if let Err(cleanup) = crate::topology_runtime::release_root_lease(&tree_id).await {
            return result.map_err(|error| CodingSessionOpenFailure {
                failure: error.failure.with_cleanup_failure(cleanup),
                current_locator: error.current_locator,
            });
        }
        return result;
    }
    if is_root && let Some(topology) = crate::topology_runtime::lookup(&tree_id) {
        reattach_inflight_children(&reattach_parent, &topology).await;
    }
    result
}

pub(super) fn claim_root_topology_owner(tree_id: &str, is_root: bool) -> Option<String> {
    if is_root && crate::topology_runtime::lookup(tree_id).is_some() {
        Some(tree_id.to_owned())
    } else {
        None
    }
}

pub fn api_topology_parent(
    product_home: PathBuf,
    topology_tree_id: String,
    record: &crate::sessions::SessionRecord,
    cwd: PathBuf,
    state_roots: ProductStateRoots,
    model: String,
    effort: Option<ReasoningEffort>,
    target: crate::binding::ApiTarget,
    plan_state_authority: String,
    scheduled_compaction: bool,
    max_model_rounds: usize,
    topology_enabled: bool,
) -> ApiTopologyParent {
    ApiTopologyParent {
        // The caller already hands the home in; the parent states it rather
        // than letting the binder rediscover it.
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        product_home: Some(product_home),
        workspace_root: record.workspace_root.clone(),
        cwd,
        plan_state_authority,
        composition: record.composition,
        topology_tree_id: Some(topology_tree_id),
        topology_caller_id: None,
        state_roots,
        model,
        effort,
        target,
        scheduled_compaction,
        max_model_rounds,
        topology_enabled,
        created_output_bound: None,
    }
}

pub(crate) struct PreparedApiRuntime {
    pub runtime: SessionRuntimeBinding,
    pub evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
    /// Set when this reopen kept a budget the route has since moved.
    pub output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
}

/// Compiles against an attached hub without acquiring session ownership.
pub(crate) async fn api_session_runtime_binding(
    parent: &ApiTopologyParent,
    opening_durable_session: bool,
    tool_fence: SessionToolFence,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<PreparedApiRuntime> {
    api_session_runtime_binding_with_owner(
        parent,
        opening_durable_session,
        tool_fence,
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        cached_evidence,
        &mut None,
    )
    .await
}

/// An API runtime whose topology hub is looked up, or attached when a
/// profile fork's own tree has none yet. The caller drops an attached lease.
pub(crate) async fn api_session_runtime_binding_for(
    parent: &ApiTopologyParent,
    opening_durable_session: bool,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    acquired_owner: &mut Option<String>,
) -> CliResult<PreparedApiRuntime> {
    api_session_runtime_binding_with_owner(
        parent,
        opening_durable_session,
        SessionToolFence::Full,
        topology_attach,
        None,
        acquired_owner,
    )
    .await
}

async fn api_session_runtime_binding_with_owner(
    parent: &ApiTopologyParent,
    opening_durable_session: bool,
    tool_fence: SessionToolFence,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
    acquired_owner: &mut Option<String>,
) -> CliResult<PreparedApiRuntime> {
    install_occupancy_fence(&parent.cwd)?;
    let spawn_parent = SpawnParent::Api(parent.clone());
    let topology_extensions = topology_extensions_if_enabled_with_owner(
        &spawn_parent,
        &parent.model,
        topology_attach,
        opening_durable_session,
        acquired_owner,
    )?;
    match parent.target {
        crate::binding::ApiTarget::OpenRouter { wire } => {
            let prepared = Box::pin(crate::openrouter::construct_openrouter_runtime_with_fence(
                crate::openrouter::ProductOpenRouterSpec {
                    job_config: parent.state_roots.job_config(),
                    exchange_observer: parent.state_roots.exchange_observer(),
                    credentials: parent.state_roots.openrouter_credentials(),
                    sidecars: Arc::clone(parent.state_roots.sidecars()),
                    product_home: parent.product_home.clone(),
                    literal_model: parent.model.clone(),
                    effort: parent.effort,
                    composition: parent.composition,
                    wire,
                    workspace_root: parent.workspace_root.clone(),
                    cwd: parent.cwd.clone(),
                    plan_state_authority: parent.plan_state_authority.clone(),
                    opening_durable_session,
                    occupancy_tier: crate::agent_tree_protocol::Tier::Mutate,
                    scheduled_compaction: parent.scheduled_compaction,
                    max_model_rounds: parent.max_model_rounds,
                    topology_extensions,
                    cached_evidence,
                    telemetry: parent.state_roots.telemetry_emitter(),
                    spawned_child: parent.topology_caller_id.is_some(),
                    created_output_bound: parent.created_output_bound,
                },
                tool_fence,
            ))
            .await?;
            Ok(PreparedApiRuntime {
                runtime: prepared.runtime.with_job_host(
                    parent.state_roots.job_registry().for_agent(
                        parent
                            .topology_caller_id
                            .clone()
                            .unwrap_or_else(|| "root".to_owned()),
                        parent
                            .topology_tree_id
                            .as_deref()
                            .unwrap_or(parent.plan_state_authority.as_str()),
                    ),
                ),
                output_bound_rederived: prepared.output_bound_rederived,
                evidence: Some(prepared.evidence),
            })
        }
        crate::binding::ApiTarget::Platform { provider } => {
            let telemetry = parent.state_roots.telemetry_emitter();
            let admission = parent.state_roots.platform_admission();
            let runtime = Box::pin(crate::platform::construct_platform_runtime_with(
                provider,
                parent.state_roots.platform_credentials(),
                &crate::api_runtime::ApiRuntimeSpec {
                    job_config: parent.state_roots.job_config(),
                    account: provider.account(),
                    sidecars: parent.state_roots.sidecars().as_ref(),
                    product_home: parent.product_home.as_deref(),
                    literal_model: &parent.model,
                    effort: parent.effort,
                    composition: parent.composition,
                    wire: provider.protocol(),
                    workspace_root: &parent.workspace_root,
                    cwd: &parent.cwd,
                    plan_state_authority: &parent.plan_state_authority,
                    opening_durable_session,
                    tool_fence,
                    scheduled_compaction: parent.scheduled_compaction,
                    max_model_rounds: parent.max_model_rounds,
                    topology_extensions: &topology_extensions,
                    telemetry: telemetry.as_ref(),
                    spawned_child: parent.topology_caller_id.is_some(),
                },
                parent.state_roots.exchange_observer(),
                &admission,
            ))
            .await?;
            Ok(PreparedApiRuntime {
                output_bound_rederived: None,
                runtime: runtime.with_job_host(
                    parent.state_roots.job_registry().for_agent(
                        parent
                            .topology_caller_id
                            .clone()
                            .unwrap_or_else(|| "root".to_owned()),
                        parent
                            .topology_tree_id
                            .as_deref()
                            .unwrap_or(parent.plan_state_authority.as_str()),
                    ),
                ),
                evidence: None,
            })
        }
    }
}

pub async fn build_api_root_coding_session(
    parent: ApiTopologyParent,
    create_operation_id: &str,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<AttachedCodingSession> {
    Box::pin(build_api_coding_session(
        parent,
        create_operation_id,
        None,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        cached_evidence,
    ))
    .await
}

pub(crate) async fn build_api_coding_session(
    parent: ApiTopologyParent,
    create_operation_id: &str,
    seed: Option<PortableSessionHistory>,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
) -> CliResult<AttachedCodingSession> {
    let operation_id = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("plak create operation identity is invalid"))?;
    Box::pin(build_or_open_api_coding_session(
        parent,
        None,
        seed,
        operation_id,
        topology_attach,
        cached_evidence,
        SessionToolFence::Full,
    ))
    .await
    .map_err(|error| error.failure)
}

pub async fn open_api_root_coding_session(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    Box::pin(open_api_coding_session(
        parent,
        locator,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        cached_evidence,
    ))
    .await
}

pub(crate) async fn open_api_coding_session(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    Box::pin(open_api_coding_session_with_fence(
        parent,
        locator,
        topology_attach,
        cached_evidence,
        SessionToolFence::Full,
    ))
    .await
}

pub(crate) async fn open_api_coding_session_with_fence(
    parent: ApiTopologyParent,
    locator: SessionLocator,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
    tool_fence: SessionToolFence,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let operation_id = SessionOperationId::new(format!("plak:open:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("plak open operation identity is invalid"))?;
    Box::pin(build_or_open_api_coding_session(
        parent,
        Some(locator),
        None,
        operation_id,
        topology_attach,
        cached_evidence,
        tool_fence,
    ))
    .await
}

pub(super) async fn build_or_open_api_coding_session(
    parent: ApiTopologyParent,
    locator: Option<SessionLocator>,
    seed: Option<PortableSessionHistory>,
    operation_id: SessionOperationId,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    cached_evidence: Option<plakous::openrouter::OpenRouterChatModelEvidence>,
    tool_fence: SessionToolFence,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let tree_id = parent
        .topology_tree_id
        .as_deref()
        .unwrap_or(parent.plan_state_authority.as_str())
        .to_owned();
    let is_root = parent.topology_caller_id.is_none();
    if parent.product_home.is_none() {
        return Err(CliFailure::new("plak product home is unavailable").into());
    }
    let sessions = parent
        .state_roots
        .runtime_sessions()
        .map_err(CodingSessionOpenFailure::from)?;
    // Read before anything is compiled: the configuration a reopen builds has
    // to carry the process budgets and output bound the session was created
    // under, not today's product policy or route attestation.
    //
    // This read opens the very branch the open below opens, so it is the first
    // place a locator that lags the durable head is noticed. The typed head has
    // to survive: the caller's stale-head adoption retries with it, and a bare
    // failure here would strand every API session whose record lagged, which is
    // the one case adoption exists for.
    let mut parent = parent;
    if let Some(locator) = locator {
        parent.state_roots = parent
            .state_roots
            .for_exact_reopen(&sessions, locator)
            .await?;
        parent.created_output_bound = Some(
            sessions
                .created_request_output_bound(locator)
                .await
                .map_err(CodingSessionOpenFailure::from)?,
        );
    } else {
        parent.state_roots = parent.state_roots.without_reopen_artifact();
    }
    let reattach_parent = SpawnParent::Api(parent.clone());
    let mut topology_owner = None;
    // Set only on the success arm, which is the only arm that compiled anything.
    let output_bound_rederived;
    let runtime = match Box::pin(api_session_runtime_binding_with_owner(
        &parent,
        locator.is_some(),
        tool_fence,
        topology_attach,
        cached_evidence,
        &mut topology_owner,
    ))
    .await
    {
        Ok(prepared) => {
            output_bound_rederived = prepared.output_bound_rederived;
            prepared.runtime
        }
        Err(error) => {
            if let Some(acquired) = topology_owner.take() {
                if let Err(cleanup) = crate::topology_runtime::release_root_lease(&acquired).await {
                    return Err(CodingSessionOpenFailure::from(
                        error.with_cleanup_failure(cleanup),
                    ));
                }
            }
            return Err(CodingSessionOpenFailure::from(error));
        }
    };
    let acquired_owner = topology_owner.clone();
    let result = build_or_open_runtime_session(
        sessions,
        runtime,
        locator,
        seed,
        operation_id,
        topology_owner,
        output_bound_rederived,
    )
    .await;
    if result.is_err() {
        if let Some(acquired) = acquired_owner {
            if let Err(cleanup) = crate::topology_runtime::release_root_lease(&acquired).await {
                return result.map_err(|error| CodingSessionOpenFailure {
                    failure: error.failure.with_cleanup_failure(cleanup),
                    current_locator: error.current_locator,
                });
            }
        }
        return result;
    }
    if topology_attach == crate::topology_runtime::TopologyHubAttach::AttachRoot
        && let Some(topology) = crate::topology_runtime::lookup(&tree_id)
    {
        topology.remember_rejoin_spec(Arc::new(reattach_parent.clone()));
        super::recovery::install_retirement_recovery(&topology, &tree_id).await;
    }
    if is_root
        && topology_attach == crate::topology_runtime::TopologyHubAttach::AttachRoot
        && let Some(topology) = crate::topology_runtime::lookup(&tree_id)
    {
        reattach_inflight_children(&reattach_parent, &topology).await;
    }
    result
}

#[must_use]
pub fn topology_tree_is_restored_paused(tree_id: &str) -> bool {
    crate::topology_runtime::is_restored_paused(tree_id)
}

pub const RESTORED_PAUSED_RESUME_HINT: &str = "the agent tree is paused after an interrupt; relaunch with `plak run --resume-tree`, or open the TUI and press r on /tree to resume the tree";

pub async fn rejoin_inflight_after_resume(tree_id: &str) {
    let Some(runtime) = crate::topology_runtime::lookup(tree_id) else {
        return;
    };
    let Some(parent) = rejoin_parent(&runtime) else {
        return;
    };
    reattach_inflight_children(&parent, &runtime).await;
}

pub(super) async fn build_or_open_runtime_session(
    sessions: Arc<AgentSessions>,
    runtime: SessionRuntimeBinding,
    locator: Option<SessionLocator>,
    seed: Option<PortableSessionHistory>,
    operation_id: SessionOperationId,
    topology_owner: Option<String>,
    // Set only by a reopen whose route has moved its output budget; carried to
    // the attached session so it is reported once, at open.
    output_bound_rederived: Option<crate::openrouter::OutputBoundRederived>,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let reachability_probe = plakous::sessions::internal::reachability_probe(&runtime);
    if let Some(locator) = locator {
        if seed.is_some() {
            return Err(CliFailure::new("a carried history seeds only a new session").into());
        }
        let opened =
            Box::pin(sessions.open(OpenSessionRequest::new(operation_id, locator, runtime)))
                .await
                .map_err(|error| CodingSessionOpenFailure {
                    failure: CliFailure::new(format_session_reopen_error(
                        "exact session reopen failed",
                        &error,
                    )),
                    current_locator: error.current_locator(),
                })?;
        return match opened {
            OpenSessionOutcome::Opened { session, .. } => {
                require_reconstructed_coding_projection(&session)?;
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner,
                })
            }
            OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
                require_reconstructed_coding_projection(&session)?;
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: Some(turn),
                    locator,
                    topology_owner,
                })
            }
            OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds,
                ..
            } => Err(reopen_refusal(
                "exact session reopen requires explicit recovery; no new turn was admitted",
                retry_after_seconds,
            )
            .into()),
            _ => Err(CliFailure::new("exact session reopen was unavailable").into()),
        };
    }
    match Box::pin(sessions.create(create_request(operation_id, runtime, seed)))
        .await
        .map_err(|error| {
            CliFailure::new(format!(
                "agent could not be initialized ({:?}): {error}",
                error.kind()
            ))
        })? {
        CreateSessionOutcome::Created { session, .. } => {
            require_reconstructed_coding_projection(&session)?;
            let locator = session.locator();
            Ok(AttachedCodingSession {
                output_bound_rederived,
                reachability_probe: reachability_probe.clone(),
                sessions: Arc::clone(&sessions),
                opened: Some(session),
                recovered_turn: None,
                locator,
                topology_owner,
            })
        }
        _ => Err(CliFailure::new("agent session creation was unavailable").into()),
    }
}

fn create_request(
    operation_id: SessionOperationId,
    runtime: SessionRuntimeBinding,
    seed: Option<PortableSessionHistory>,
) -> CreateSessionRequest {
    let request = CreateSessionRequest::new(operation_id, runtime);
    match seed {
        Some(history) => request.with_history(history),
        None => request,
    }
}

pub(super) fn require_reconstructed_coding_projection(
    session: &OpenedSession,
) -> Result<(), CodingSessionOpenFailure> {
    if session
        .agent()
        .stable_request_prefix()
        .tool_projection_v2()
        .is_none()
    {
        return Err(CliFailure::new(format!(
            "session {} was created by an incompatible plak build; start a new session",
            session.locator().session_id()
        ))
        .into());
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub async fn build_or_open_injected_session(
    sessions: Arc<AgentSessions>,
    locator: Option<SessionLocator>,
    seed: Option<PortableSessionHistory>,
    operation_id: SessionOperationId,
    runtime: SessionRuntimeBinding,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    let reachability_probe = plakous::sessions::internal::reachability_probe(&runtime);
    if let Some(locator) = locator {
        if seed.is_some() {
            return Err(CliFailure::new("a carried history seeds only a new session").into());
        }
        let opened = sessions
            .open(OpenSessionRequest::new(operation_id, locator, runtime))
            .await
            .map_err(|error| CodingSessionOpenFailure {
                failure: CliFailure::new(format!("injected exact reopen failed: {error}")),
                current_locator: error.current_locator(),
            })?;
        return match opened {
            OpenSessionOutcome::Opened { session, .. } => {
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                })
            }
            OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: Some(turn),
                    locator,
                    topology_owner: None,
                })
            }
            OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds,
                ..
            } => Err(reopen_refusal(
                "injected exact reopen requires recovery",
                retry_after_seconds,
            )
            .into()),
            _ => Err(CliFailure::new("injected exact reopen was unavailable").into()),
        };
    }
    match sessions
        .create(create_request(operation_id, runtime, seed))
        .await
        .map_err(|error| {
            CliFailure::new(format!(
                "injected session create failed ({:?}): {error}",
                error.kind()
            ))
        })? {
        CreateSessionOutcome::Created { session, .. } => {
            let locator = session.locator();
            Ok(AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: reachability_probe.clone(),
                sessions: Arc::clone(&sessions),
                opened: Some(session),
                recovered_turn: None,
                locator,
                topology_owner: None,
            })
        }
        _ => Err(CliFailure::new("injected session create was unavailable").into()),
    }
}

/// Constructs the exact sealed runtime authority used by create, open, and
/// driver-owned session workflows.
pub fn coding_session_runtime_binding(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
) -> CliResult<SessionRuntimeBinding> {
    coding_session_runtime_binding_for(
        spec,
        stderr,
        false,
        SessionToolFence::Full,
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        &mut None,
    )
}

/// Fresh conversation create (`/clear`) must attach a new root tree, not look
/// up the previous hub lease.
pub fn coding_session_runtime_binding_attaching_root(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
) -> CliResult<SessionRuntimeBinding> {
    coding_session_runtime_binding_for(
        spec,
        stderr,
        false,
        SessionToolFence::Full,
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        &mut None,
    )
}

/// Builds a native runtime with the caller's attach choice and reports the
/// tree id when that build took an `AttachRoot` lease.
pub(crate) fn coding_session_runtime_binding_with_attach(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
    attach: crate::topology_runtime::TopologyHubAttach,
    attached_tree: &mut Option<String>,
) -> CliResult<SessionRuntimeBinding> {
    coding_session_runtime_binding_for(
        spec,
        stderr,
        false,
        SessionToolFence::Full,
        attach,
        attached_tree,
    )
}

pub(super) const fn native_provider_kind(provider: NativeProvider) -> &'static str {
    match provider {
        NativeProvider::Codex => "codex",
        NativeProvider::Xai => "xai",
        NativeProvider::Claude => "claude",
        _ => "native",
    }
}

pub(super) fn coding_session_runtime_binding_for(
    spec: ProductAgentSpec,
    stderr: &mut dyn Write,
    opening_durable_session: bool,
    fence: SessionToolFence,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    attached_tree: &mut Option<String>,
) -> CliResult<SessionRuntimeBinding> {
    let topology_spec = spec.clone();
    let ProductAgentSpec {
        product_home,
        prompt_host_facts: _,
        provider,
        resolved,
        effort,
        effort_explicit,
        workspace_root,
        cwd,
        plan_state_authority,
        composition,
        topology_tree_id: _,
        topology_caller_id: _,
        state_roots,
        scheduled_compaction,
        max_model_rounds,
        topology_enabled: _,
    } = spec;
    let _binding_span = tracing::info_span!(
        "session_runtime_binding",
        provider = native_provider_kind(provider),
        model = resolved.selected_model().as_str(),
        composition = composition.as_str(),
        session_id = plan_state_authority.as_str(),
    )
    .entered();
    install_occupancy_fence(&cwd)?;
    let model = resolved.model();

    let resolved_tool_mode = resolved.native_tool_mode();
    let code_mode_candidate = composition_requests_native_ptc(composition)
        && is_exact_code_mode_attachment(provider, resolved.selected_model(), resolved_tool_mode);
    let product_javascript = composition_is_js_ptc(composition);
    let product_python = composition_is_py_ptc(composition);
    if composition == CompositionSelection::NativePtc && !code_mode_candidate {
        return Err(CliFailure::new(crate::binding::native_ptc_refusal(
            provider,
        )));
    }
    if (product_javascript || product_python)
        && !direct_composition_capable(provider, resolved_tool_mode)
    {
        return Err(CliFailure::new(
            "Plak-owned custom composition requires exact Direct-capable catalog authority; no inference was dispatched",
        ));
    }
    if product_javascript || product_python {
        if resolved.configuration().selected().behavior().tool_mode != ToolPresentationMode::Direct
            || resolved.configuration().effective().behavior().tool_mode
                != ToolPresentationMode::Direct
        {
            return Err(CliFailure::new(
                "product composition provider projection is not Direct-capable",
            ));
        }
    } else if code_mode_candidate {
        resolved
            .require_native_tool_mode(NativeToolMode::CodeModeOnly)
            .map_err(|error| {
                CliFailure::new(format!(
                    "Codex Code Mode requires exact CodeModeOnly catalog authority: {error}"
                ))
            })?;
        if resolved.configuration().selected().behavior().tool_mode
            != ToolPresentationMode::LocalCompositionOnly
            || resolved.configuration().effective().behavior().tool_mode
                != ToolPresentationMode::LocalCompositionOnly
        {
            return Err(CliFailure::new(
                "Codex Code Mode provider configuration has the wrong exact tool mode",
            ));
        }
    } else if provider == NativeProvider::Codex {
        match resolved_tool_mode {
            Some(NativeToolMode::Direct)
                if resolved.configuration().effective().behavior().tool_mode
                    == ToolPresentationMode::Direct => {}
            Some(NativeToolMode::CodeModeOnly)
                if resolved.configuration().effective().behavior().tool_mode
                    == ToolPresentationMode::Direct => {}
            Some(NativeToolMode::CodeModeOnly) => {
                return Err(CliFailure::new(
                    "Codex catalog requires CodeModeOnly for a model without the exact V1 presentation",
                ));
            }
            Some(NativeToolMode::CodeMode) => {
                return Err(CliFailure::new(
                    "Codex hybrid CodeMode has no exact enrolled product presentation",
                ));
            }
            Some(NativeToolMode::Direct) => {
                return Err(CliFailure::new(
                    "Codex Direct catalog authority resolved to a non-Direct presentation",
                ));
            }
            None => {
                return Err(CliFailure::new(
                    "Codex model has no recognized authenticated tool-mode authority",
                ));
            }
        }
    }

    let product_extensions = topology_extensions_if_enabled(
        &SpawnParent::from_native(topology_spec.clone()),
        resolved.selected_model().as_str(),
        topology_attach,
        opening_durable_session,
        attached_tree,
    )?;
    let frozen_tools = coding_tool_registry_with_extensions(
        &plan_state_authority,
        composition,
        code_mode_candidate,
        &product_extensions,
    )?;
    let extra_nested = product_extensions.names().cloned().collect::<Vec<_>>();
    let direct_extras =
        !extra_nested.is_empty() && !product_javascript && !product_python && !code_mode_candidate;
    // One callable surface for this node, compiled once with the session's
    // own configuration: the presentation's core menu plus every extension.
    // It is the nested catalog the PTC descriptors list, the names the PTC
    // base instruction names, and the set the earned agreement paragraphs are
    // judged against.
    let runtime = if product_javascript {
        Some(CompositionRuntime::ProductJavaScript)
    } else if product_python {
        Some(CompositionRuntime::PythonPtc)
    } else if code_mode_candidate {
        Some(CompositionRuntime::CodexNative)
    } else {
        None
    };
    let NodeSession {
        configured_model,
        configuration: session_configuration,
        presentation: presentation_kind,
        surface,
    } = node_session(
        state_roots.job_config(),
        resolved.configured_model(),
        &frozen_tools,
        &product_extensions,
        if direct_extras { &extra_nested } else { &[] },
        fence,
        runtime,
    )?;
    // A fresh resolution past the admitted window is compiled for this node
    // exactly as the construction above compiled the current one.
    let model_reresolver = {
        let (job_config, registry, extensions) = (
            state_roots.job_config(),
            frozen_tools.clone(),
            product_extensions.clone(),
        );
        let extras = if direct_extras {
            extra_nested.clone()
        } else {
            Vec::new()
        };
        super::reresolve::NativeModelReresolver::for_resolution(
            provider,
            &resolved,
            Box::new(move |configured| {
                node_session(
                    job_config,
                    configured,
                    &registry,
                    &extensions,
                    &extras,
                    fence,
                    runtime,
                )
                .map(|session| session.configured_model)
            }),
        )
    };
    let effective_tools = &session_configuration.effective().behavior().tools;
    if effective_tools.iter().any(|configured| {
        frozen_tools
            .definitions()
            .iter()
            .all(|actual| configured.definition() != actual)
    }) {
        return Err(CliFailure::new(
            "resolved provider tool configuration does not match the product registry",
        ));
    }
    let physical_root = std::fs::canonicalize(&workspace_root)
        .map_err(|_| CliFailure::new("recorded workspace root could not be resolved"))?;
    ensure_workspace_excludes_product_state(&physical_root, state_roots.paths())?;
    let physical_cwd = std::fs::canonicalize(&cwd)
        .map_err(|_| CliFailure::new("workspace current directory could not be resolved"))?;
    let relative_cwd = physical_cwd.strip_prefix(&physical_root).map_err(|_| {
        CliFailure::new("workspace current directory is outside the recorded workspace root")
    })?;
    let workspace = Arc::new(stamped_workspace_policy(&physical_root, relative_cwd)?);
    let request_output_bound = resolved
        .configuration()
        .effective()
        .behavior()
        .output_bound
        .bound();
    let generation_limits = GenerationLimits::new(request_output_bound, Vec::new())
        .map_err(|_| CliFailure::new("native run generation policy is invalid"))?;
    // Required-or-omit: harness plan PreferEffort omits reasoning
    // when the model does not admit it; when enabled, require it honestly.
    let reasoning = match effort {
        Some(effort) => ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: CapabilityRequirement::Required,
        },
        None => ReasoningPolicy::Disabled,
    };
    let request_policy = AgentRequestPolicy::new(generation_limits, reasoning);
    let config = AgentConfig::default()
        .with_observation_budget_cap_bytes(observation_budget_cap_bytes(
            resolved
                .configuration()
                .effective()
                .descriptor()
                .context_contract()
                .map_or(0, |contract| contract.capacity().rule().input_ceiling()),
        ))
        .with_max_model_rounds(max_model_rounds)
        .with_model_cleanup_timeout(NATIVE_RUN_MODEL_CLEANUP_TIMEOUT)
        .with_model_operation_timeout(NATIVE_RUN_MODEL_OPERATION_TIMEOUT)
        // Full-tool multi-step coding emits more live events than smoke.
        .with_event_capacity(4_096)
        .with_event_byte_capacity(16 * 1024 * 1024)
        .with_scheduled_compaction_enabled(scheduled_compaction);

    let builder = AgentBuilder::new(
        configured_model,
        frozen_tools.clone(),
        Arc::clone(&workspace),
    )
    // The product compiled the projection below from its own pack list and
    // route catalog, so the product is what admits it at construction.
    .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
    .post_compaction_note(Arc::new(plan_restoration_note))
    .host_process_options(crate::process_environment::options(&physical_root)?)
    .job_config(state_roots.job_config())
    .job_host(
        state_roots.job_registry().for_agent(
            topology_spec
                .topology_caller_id
                .clone()
                .unwrap_or_else(|| "root".to_owned()),
            topology_spec
                .topology_tree_id
                .as_deref()
                .unwrap_or(topology_spec.plan_state_authority.as_str()),
        ),
    );
    let mut builder = builder.tool_policy(session_tool_policy(fence));
    if let Some((reresolver, admitted_until)) = model_reresolver {
        builder = builder.model_reresolver(reresolver, admitted_until);
    }
    if let Some(telemetry) = state_roots.telemetry_emitter() {
        builder = builder.telemetry(telemetry);
    }

    if product_javascript {
        let target = projection_target(resolved.configuration(), "product JavaScript target")?;
        let descriptor = ProductJavaScriptCompositionV1::compile_for_kind(
            state_roots.job_config(),
            frozen_tools.clone(),
            target,
            presentation_kind,
            surface.exposed_names().iter().cloned(),
        )
        .map_err(|error| CliFailure::new(format!(
            "product JavaScript descriptor could not be compiled: {error}; no inference was dispatched"
        )))?;
        let host_path = state_roots.sidecars().javascript_host().map_err(|error| {
            CliFailure::new(format!(
                "product JavaScript host is unavailable: {error}; set PLAK_JS_HOST or place plakous-js-host beside plak; no inference was dispatched"
            ))
        })?;
        let attachment = product_javascript_candidate_attachment(
            &descriptor,
            &host_path,
            resolved.configuration(),
            presentation_kind,
        )
        .ok_or_else(|| {
            CliFailure::new(
                "product JavaScript artifact or evidence is unavailable; no inference was dispatched",
            )
        })?;
        builder = plakous::agent::attach_local_composition(builder, Some(attachment)).map_err(
            |error| {
                CliFailure::new(format!(
                    "product JavaScript artifact or evidence is unavailable: {error}; no inference was dispatched"
                ))
            },
        )?;
    } else if product_python {
        let target = projection_target(resolved.configuration(), "product Python target")?;
        let descriptor = PythonPtcV1::compile_for_kind(
            state_roots.job_config(),
            frozen_tools.clone(),
            target,
            PythonPtcPresentationV1::StructuredFunctions,
            presentation_kind,
            surface.exposed_names().iter().cloned(),
        )
        .map_err(|error| {
            CliFailure::new(format!(
                "product Python descriptor could not be compiled: {error}; no inference was dispatched"
            ))
        })?;
        let host_path = state_roots.sidecars().python_host().map_err(|error| {
            CliFailure::new(format!(
                "product Python host is unavailable: {error}; set PLAK_MONTY_WORKER or place monty beside plak; no inference was dispatched"
            ))
        })?;
        let attachment = product_python_attachment(
            &descriptor,
            &host_path,
            resolved.configuration(),
            presentation_kind,
        )?;
        builder = plakous::agent::attach_local_composition(builder, Some(attachment)).map_err(
            |error| {
                CliFailure::new(format!(
                    "product Python artifact or evidence is unavailable: {error}; no inference was dispatched"
                ))
            },
        )?;
    } else if code_mode_candidate {
        let target = projection_target(resolved.configuration(), "Code Mode resolved target")?;
        let descriptor = CodexMinimalLocalCodeModeV1::compile_for_catalog(
            state_roots.job_config(),
            frozen_tools.clone(),
            target,
            &surface
                .exposed_names()
                .iter()
                .filter(|name| name.as_str() != "inspect_image")
                .cloned()
                .collect::<Vec<_>>(),
        )
        .map_err(|error| {
            CliFailure::new(format!(
                "Code Mode descriptor could not be compiled: {error}"
            ))
        })?;
        let host_path = state_roots.sidecars().javascript_host().map_err(|error| {
            CliFailure::new(format!(
                "Codex Code Mode sidecar is unavailable: {error}; set PLAK_JS_HOST or place plakous-js-host beside plak"
            ))
        })?;
        builder =
            plakous::agent::codex_minimal_local_code_mode_sidecar(builder, &descriptor, host_path)
                .map_err(|error| {
                    CliFailure::new(format!(
                        "Codex Code Mode (rquickjs sidecar) could not attach: {error}"
                    ))
                })?;
    } else if !opening_durable_session {
        let projection = exact_process_presentation_candidate(
            state_roots.job_config(),
            provider,
            resolved.as_ref(),
            &session_configuration,
            frozen_tools.clone(),
            if direct_extras {
                extra_nested.as_slice()
            } else {
                &[]
            },
        )?;
        builder = builder.tool_projection_v2(if fence == SessionToolFence::Full {
            projection
        } else {
            surface.projection().clone()
        });
    }

    // Exact reopen must not offer a freshly compiled product default as a
    // replacement for the reconstruction-complete plan in the checkpoint.
    // Plakous rehydrates that plan and still validates its model, tools,
    // workspace, provider configuration, and fresh runtime authority.
    // Rotate `.subagents.v1` only when topology extras actually compiled
    // (Direct extras or Code Mode nested names), not merely because the
    // product flag is on.
    let profile = if opening_durable_session {
        None
    } else {
        selected_harness_profile(
            provider,
            resolved.selected_model(),
            effort,
            opening_durable_session,
            code_mode_candidate,
            extra_nested.as_slice(),
            !extra_nested.is_empty(),
            composition,
            &surface,
            workspace.root(),
            workspace.cwd(),
        )?
    };
    if let Some(profile) = profile {
        let (profile, context_sources) = enroll_product_instructions(
            &profile,
            product_home.as_deref(),
            workspace.root(),
            workspace.cwd(),
        )?;
        // Explicit harness plans own reasoning at agent construction and would
        // otherwise overwrite CLI `--effort` with the profile default (Codex
        // High / Grok Low). Pin operator effort into the plan before compile.
        let plan = compile_explicit_runtime_profile(
            &profile,
            model.as_ref(),
            &session_configuration,
            &frozen_tools,
            workspace.as_ref(),
            context_sources,
        )
        .map_err(|error| CliFailure::new(format!("harness plan could not be compiled: {error}")))?;
        // Effort / preferred-feature downgrades must not stay silent when
        // the operator chose `--effort`, and should at least warn otherwise.
        let effort_downgraded = plan
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.as_str().contains("reasoning effort downgraded"));
        if effort_downgraded {
            for diagnostic in plan.diagnostics() {
                let _ = writeln!(stderr, "plan diagnostic: {}", diagnostic.as_str());
            }
            if effort_explicit {
                return Err(CliFailure::new(
                    "requested --effort was downgraded by the harness plan; refuse to run",
                ));
            }
        }
        builder = builder.harness_plan(plan);
    }

    if topology_spec.topology_caller_id.is_some() {
        builder = isolated_tool_scheduler(builder).map_err(|error| {
            CliFailure::new(format!(
                "spawn child tool scheduler is invalid ({:?}): {error}",
                error.kind()
            ))
        })?;
    }

    Ok(SessionRuntimeBinding::new(
        builder.config(config).request_policy(request_policy),
    ))
}

pub(super) fn ensure_workspace_excludes_product_state(
    workspace: &Path,
    protected: impl IntoIterator<Item = PathBuf>,
) -> CliResult<()> {
    if protected
        .into_iter()
        .any(|path| path.starts_with(workspace) || workspace.starts_with(&path))
    {
        return Err(CliFailure::new(
            "workspace overlaps Plak credential or product state authority",
        ));
    }
    Ok(())
}

/// Exact compaction runtime: same coding binding as the live turn with compact max output only.
pub fn compaction_session_runtime_binding(
    spec: &ProductAgentSpec,
) -> CliResult<SessionRuntimeBinding> {
    let mut sink = std::io::sink();
    let binding = coding_session_runtime_binding(spec.clone(), &mut sink)?;
    binding
        .with_compaction_output_limit()
        .map_err(|_| CliFailure::new("compaction generation policy is invalid"))
}

#[must_use]
pub fn coding_session_target_binding(resolved: &ResolvedProvider) -> SessionTargetBinding {
    let route = resolved.configuration().selected().route();
    SessionTargetBinding::new(
        route.protocol().clone(),
        route.route().clone(),
        route.requested_model().clone(),
        resolved.configuration().selected_identity(),
        resolved.configuration().effective_identity(),
        resolved.initial_authority().account_binding(),
    )
}
