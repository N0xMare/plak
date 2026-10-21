//! Plak-owned OpenRouter Chat construction through the supported facade.

use std::collections::BTreeSet;
use std::future::Future;
use std::num::NonZeroU32;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use plakous::agent::ReasoningEffort;
use plakous::model::{
    CachePolicy, ConfiguredLanguageModel, ContextAdmissionRequirement, LocalDurabilityDisposition,
    ModelAdapterProfileId, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition,
    ProcessingTierDisposition, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, ResponseStateDisposition, RetentionDisposition,
    ToolChoice, ToolPresentationMode,
};
use plakous::openrouter::{
    OpenRouterApiKeySource, OpenRouterChatClient, OpenRouterChatDiscoveryError,
    OpenRouterChatEndpointCandidate, OpenRouterChatModelEvidence, OpenRouterKeyUsage,
    OpenRouterReasoningFacts, OpenRouterRetentionPolicy, OpenRouterToolChoiceModes,
};
use plakous::sessions::{SessionRuntimeBinding, SessionTargetBinding};
use plakous::tools::CodingPresentation;
#[cfg(test)]
use plakous::tools::CompiledSurface;

#[cfg(test)]
use crate::agent::NATIVE_RUN_MAX_OUTPUT_TOKENS;
use crate::binding::{CompositionSelection, ResolvedProtocol};
use crate::error::{CliFailure, CliFailureKind, CliResult};

const OPENROUTER_CHAT_PROFILE: &str = "openrouter.chat.dynamic-v1";
const OPENROUTER_CHAT_RETENTION_REVISION: &str = "openrouter-chat-gateway-retention-v1";
const OPENROUTER_RESPONSES_PROFILE: &str = "openrouter.responses.dynamic-custom-v1";
const OPENROUTER_RESPONSES_INTEGRATION_REVISION: &str = "openrouter-responses-integration-v1";
/// Inputs for a newly discovered Custom/experimental OpenRouter Chat runtime.
pub struct ProductOpenRouterSpec {
    /// Construction-bound process budgets.
    pub job_config: plakous::tools::JobConfig,
    /// The per-request output bound a session being reopened was created with.
    ///
    /// `None` for a new session, which takes the figure this route attests now.
    pub created_output_bound: Option<OutputBoundDisposition>,
    /// Optional product-supplied observation; never credential or dispatch authority.
    pub exchange_observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
    /// The credentials this binding dispatches under, resolved by the product.
    pub credentials: Arc<dyn OpenRouterCredentials>,
    /// Where the evaluator workers a local composition dispatches into are
    /// installed. The binder asks through the composition's locator port;
    /// the product supplies installation paths.
    pub sidecars: Arc<dyn plakous::tools::SidecarLocator>,
    /// The product home whose instructions the profile enrolls, when there is
    /// one. The binder reads nothing else out of it.
    pub product_home: Option<PathBuf>,
    pub literal_model: String,
    pub effort: Option<ReasoningEffort>,
    pub composition: CompositionSelection,
    pub wire: ResolvedProtocol,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub plan_state_authority: String,
    pub opening_durable_session: bool,
    /// Spawn tier of this session's node; the root is `mutate`.
    pub occupancy_tier: crate::agent_tree_protocol::Tier,
    /// Product `compaction.enabled`; gates tool-group scheduled compact.
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    pub topology_extensions: crate::tools::ProductToolExtensions,
    pub cached_evidence: Option<OpenRouterChatModelEvidence>,
    pub telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
    /// A `spawn_agent` child in the parent's process; see
    /// `isolated_tool_scheduler`.
    pub spawned_child: bool,
}

/// Exact facade runtime plus its reconstruction-complete session target.
pub struct ProductOpenRouterRuntime {
    /// Set when this reopen kept a budget the route has since moved.
    pub output_bound_rederived: Option<OutputBoundRederived>,
    pub runtime: SessionRuntimeBinding,
    pub target: SessionTargetBinding,
    pub evidence: OpenRouterChatModelEvidence,
}

struct PreparedOpenRouter {
    configured: ConfiguredLanguageModel,
    registry: plakous::tools::FrozenToolRegistry,
    evidence: OpenRouterChatModelEvidence,
    /// Hosted web search on the same route, key and model, whatever the wire.
    retrieval: plakous::openrouter::OpenRouterRetrieval,
    output_bound_rederived: Option<OutputBoundRederived>,
}

/// A reopened session's own output budget beside the one its route now attests.
///
/// The session keeps its own, because that figure reaches the configuration
/// identity and from there the persisted harness plan, tool projection target,
/// request prefix and admission evidence, none of which can be re-signed. The
/// operator is told what the route now offers and how to get it, rather than
/// being left on a budget that silently stopped matching the route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputBoundRederived {
    /// The figure this session was created with, and still uses.
    pub previous: Option<NonZeroU32>,
    /// The figure the route attests now.
    pub current: Option<NonZeroU32>,
}

/// The OpenRouter credentials a binder is handed.
///
/// The native path takes a `ResolvedProvider` resolved before construction; this
/// is the same shape for OpenRouter. Everything else in this module is hosted —
/// route admission, endpoint eligibility, the wire behaviour — and none of it
/// should know that the key comes from a product secret store keyed by a
/// product home.
///
/// A trait and not a resolved `SecretString`: `ProductOpenRouterApiKeySource`
/// re-resolves at every attempt fence and retains no secret, so a one-shot value
/// would break the refresh it exists for.
pub trait OpenRouterCredentials: Send + Sync + 'static {
    /// Fail fast with a construction diagnostic before any client is built.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when no credential is available.
    fn preflight(&self) -> CliResult<()>;

    /// The source the client re-resolves through at every attempt fence.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the credential binding is invalid.
    fn source(&self) -> CliResult<Arc<dyn OpenRouterApiKeySource>>;

    /// The selected key's safe account label: its environment variable name
    /// or `sha256:` plus eight hex digits, never a key prefix or suffix.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the selected key cannot be read.
    fn key_label(&self) -> CliResult<String>;
}

/// Authenticates, discovers one literal model, and constructs its exact Chat runtime.
pub async fn construct_openrouter_runtime(
    spec: ProductOpenRouterSpec,
) -> CliResult<ProductOpenRouterRuntime> {
    let fence = crate::agent::occupancy_session_fence(spec.occupancy_tier);
    construct_openrouter_runtime_with_fence(spec, fence).await
}

pub(crate) async fn construct_openrouter_runtime_with_fence(
    spec: ProductOpenRouterSpec,
    fence: crate::agent::SessionToolFence,
) -> CliResult<ProductOpenRouterRuntime> {
    let prepared = prepare_openrouter(&spec).await?;
    spec.topology_extensions.connect_retrieval(Some(Arc::new(
        plakous::openrouter::OpenRouterRetrievalPort::new(prepared.retrieval),
    )))?;
    let runtime = openrouter_runtime_binding(
        &spec,
        prepared.configured,
        prepared.registry,
        fence,
        Arc::new(plakous::model::SystemClock),
    )?;
    let target = runtime.target_binding();
    Ok(ProductOpenRouterRuntime {
        runtime,
        target,
        evidence: prepared.evidence,
        output_bound_rederived: prepared.output_bound_rederived,
    })
}

/// Compaction runtime: same OpenRouter coding binding with compact max output only.
pub async fn construct_openrouter_compaction_runtime(
    mut spec: ProductOpenRouterSpec,
) -> CliResult<SessionRuntimeBinding> {
    spec.opening_durable_session = true;
    let prepared = prepare_openrouter(&spec).await?;
    api_compaction_runtime_binding(&spec, &prepared.configured, &prepared.registry)
}

async fn prepare_openrouter(spec: &ProductOpenRouterSpec) -> CliResult<PreparedOpenRouter> {
    if spec.composition == CompositionSelection::NativePtc {
        return Err(CliFailure::new(
            crate::binding::OPENROUTER_NATIVE_PTC_REFUSAL,
        ));
    }
    // Resolve once for an immediate, safe construction diagnostic. The source
    // itself never retains this secret and re-resolves at every attempt fence.
    spec.credentials.preflight()?;
    let client = OpenRouterChatClient::connect_with_retention(
        spec.credentials.source()?,
        OpenRouterRetentionPolicy::gateway_default(),
    )
    .and_then(OpenRouterChatClient::with_plak_attribution)
    .map(|client| client.with_exchange_observer(spec.exchange_observer.clone()))
    .map_err(|error| CliFailure::new(format!("OpenRouter connection is invalid: {error}")))?;
    let evidence = reuse_or_discover_evidence(
        spec.cached_evidence.clone(),
        |evidence| evidence.model().as_str() == spec.literal_model && evidence_is_fresh(evidence),
        || client.discover_exact(&spec.literal_model),
    )
    .await
    .map_err(|error| match error {
        OpenRouterChatDiscoveryError::AbsentModel => CliFailure::classified(
            CliFailureKind::Usage,
            format!(
                "model `{}` is not in the OpenRouter catalog of account `openrouter`; \
                 run `plak models openrouter` to list the models it offers",
                spec.literal_model
            ),
        ),
        error => CliFailure::new(format!("OpenRouter exact discovery failed: {error}")),
    })?;
    let reasoning_disable_wire =
        openrouter_reasoning_disable_wire(spec.wire, spec.effort, &evidence);
    if let Err(diagnostic) = openrouter_endpoint_admission(
        &evidence,
        spec.wire,
        spec.effort.is_some(),
        reasoning_disable_wire,
    ) {
        return Err(CliFailure::new(diagnostic));
    }
    let reasoning_effort = reasoning_disposition(spec.wire, spec.effort, &evidence)?;

    let registry = crate::agent::coding_tool_registry_with_extensions(
        &spec.plan_state_authority,
        spec.composition,
        false,
        &spec.topology_extensions,
    )?;
    let extra_names: Vec<String> = spec
        .topology_extensions
        .names()
        .map(|name| name.as_str().to_owned())
        .collect();
    let extra_refs: Vec<&str> = extra_names.iter().map(String::as_str).collect();
    let reasoning_on_wire = spec.effort.is_some() || reasoning_disable_wire;
    // One derivation serves both halves of the capacity rule: the bound Plak
    // sends on the wire and the output it reserves out of the window.
    // A session already on disk was compiled under a bound this route may since
    // have moved, and that bound reaches the configuration identity and from
    // there the persisted plan, projection target, request prefix and admission
    // evidence. None of those can be re-signed on reopen, and the request
    // prefix has to satisfy the persisted projection and the live adapter at
    // once, so a reopened session keeps the figure it was created under. The
    // created figure alone never authorizes reopen: only equal admission
    // envelopes resume. A changed envelope refuses before inference; a new
    // session gets the route's current figure.
    let derived_bound = openrouter_output_bound(&evidence, spec.wire, reasoning_on_wire)?;
    // The created disposition is carried whole, not just its figure: a session
    // created before this route derived anything spelled the same figure
    // `Enforced`, and the two spellings digest differently.
    let output_bound_disposition = spec
        .created_output_bound
        .unwrap_or(OutputBoundDisposition::DerivedFromRoute(derived_bound));
    let output_bound = output_bound_disposition.bound().unwrap_or(derived_bound);
    let behavior = openrouter_behavior(
        reasoning_effort,
        spec.composition,
        spec.wire,
        &registry,
        &extra_refs,
        output_bound_disposition,
    )?;
    let maximum_projected_input =
        openrouter_maximum_projected_input(&evidence, spec.wire, reasoning_on_wire, output_bound)?;
    // The discovered `context_length` is the model's total window the meter and
    // capacity-derived fence divide by; the product bound admitted under it stays the
    // dispatch gate.
    let context_window = evidence.context_length();
    let context_contract = match spec.wire {
        ResolvedProtocol::Chat => {
            client.custom_context_contract(context_window, maximum_projected_input)
        }
        ResolvedProtocol::Responses => {
            client.custom_responses_context_contract(context_window, maximum_projected_input)
        }
        ResolvedProtocol::Messages => {
            client.custom_messages_context_contract(context_window, maximum_projected_input)
        }
    }
    .map_err(|error| CliFailure::new(format!("OpenRouter context envelope is invalid: {error}")))?;
    let configured = match spec.wire {
        ResolvedProtocol::Chat => client
            .configured_gateway_managed_behavior(evidence.clone(), context_contract, behavior)
            .map_err(|error| {
                CliFailure::new(format!("OpenRouter Chat target is unavailable: {error}"))
            })?,
        ResolvedProtocol::Responses => client
            .configured_responses_gateway_managed_behavior(
                evidence.clone(),
                context_contract,
                behavior,
            )
            .map_err(|error| {
                CliFailure::new(format!(
                    "OpenRouter Responses target is unavailable: {error}"
                ))
            })?,
        ResolvedProtocol::Messages => client
            .configured_messages_gateway_managed_behavior(
                evidence.clone(),
                context_contract,
                behavior,
            )
            .map_err(|error| {
                CliFailure::new(format!(
                    "OpenRouter Messages target is unavailable: {error}"
                ))
            })?,
    };
    let output_bound_rederived = route_moved_output_bound(spec.created_output_bound, derived_bound);
    let retrieval = client.retrieval(evidence.clone()).map_err(|error| {
        CliFailure::new(format!("OpenRouter web search is unavailable: {error}"))
    })?;
    Ok(PreparedOpenRouter {
        configured,
        registry,
        evidence,
        retrieval,
        output_bound_rederived,
    })
}

fn api_compaction_runtime_binding(
    spec: &ProductOpenRouterSpec,
    source: &ConfiguredLanguageModel,
    registry: &plakous::tools::FrozenToolRegistry,
) -> CliResult<SessionRuntimeBinding> {
    openrouter_runtime_binding(
        spec,
        source.clone(),
        registry.clone(),
        crate::agent::occupancy_session_fence(spec.occupancy_tier),
        Arc::new(plakous::model::SystemClock),
    )?
    .with_compaction_output_limit()
    .map_err(|_| CliFailure::new("compaction generation policy is invalid"))
}

fn evidence_is_fresh(evidence: &OpenRouterChatModelEvidence) -> bool {
    let now = time::OffsetDateTime::now_utc();
    let Ok(now_seconds) = u64::try_from(now.unix_timestamp()) else {
        return false;
    };
    let expires = evidence.expires_at();
    now_seconds < expires.seconds()
        || (now_seconds == expires.seconds() && now.nanosecond() < expires.nanoseconds())
}

async fn reuse_or_discover_evidence<T, E, Reusable, Discover, Discovery>(
    cached: Option<T>,
    reusable: Reusable,
    discover: Discover,
) -> Result<T, E>
where
    Reusable: FnOnce(&T) -> bool,
    Discover: FnOnce() -> Discovery,
    Discovery: Future<Output = Result<T, E>>,
{
    if let Some(evidence) = cached.filter(reusable) {
        Ok(evidence)
    } else {
        discover().await
    }
}

/// What to report when a reopened session's route has moved its output budget.
///
/// `None` for a new session, which is compiled with the attested figure and has
/// nothing to compare, and `None` when the created figure is the attested one.
/// Only the figures are compared: a session created before this route derived
/// anything recorded the same figure as a product choice, and that is the same
/// budget, not a move.
fn route_moved_output_bound(
    created: Option<OutputBoundDisposition>,
    derived: NonZeroU32,
) -> Option<OutputBoundRederived> {
    let previous = created?.bound();
    (previous != Some(derived)).then_some(OutputBoundRederived {
        previous,
        current: Some(derived),
    })
}

fn openrouter_behavior(
    reasoning_effort: ReasoningEffortDisposition,
    composition: CompositionSelection,
    wire: ResolvedProtocol,
    registry: &plakous::tools::FrozenToolRegistry,
    extra_names: &[&str],
    output_bound: OutputBoundDisposition,
) -> CliResult<ProviderBehaviorConfiguration> {
    let tools = crate::api_runtime::configured_tools(
        CodingPresentation::Pi,
        composition,
        wire,
        registry,
        extra_names,
    )?;
    Ok(ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: match wire {
            ResolvedProtocol::Chat
                if !matches!(reasoning_effort, ReasoningEffortDisposition::Enabled(_)) =>
            {
                ReplayDisposition::Disabled
            }
            ResolvedProtocol::Chat | ResolvedProtocol::Responses => {
                ReplayDisposition::DropUnreplayableReasoning
            }
            ResolvedProtocol::Messages => ReplayDisposition::Disabled,
        },
        cache: CachePolicy::Disabled,
        tools,
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        // Derived from the route's own attestation, not a product choice, so
        // reopen re-derives it rather than demanding the persisted figure.
        output_bound,
        storage: ProviderStorageDisposition::DisabledOnWire,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new(match wire {
                ResolvedProtocol::Chat => OPENROUTER_CHAT_PROFILE,
                ResolvedProtocol::Responses => OPENROUTER_RESPONSES_PROFILE,
                ResolvedProtocol::Messages => "openrouter.messages.dynamic-custom-v1",
            })
            .map_err(|_| CliFailure::new("OpenRouter retention profile is invalid"))?,
            revision: ProviderProfileRevisionId::new(match wire {
                ResolvedProtocol::Chat => OPENROUTER_CHAT_RETENTION_REVISION,
                ResolvedProtocol::Responses => OPENROUTER_RESPONSES_INTEGRATION_REVISION,
                ResolvedProtocol::Messages => "openrouter-messages-integration-v1",
            })
            .map_err(|_| CliFailure::new("OpenRouter retention revision is invalid"))?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::DurableRequired,
    })
}

fn openrouter_reasoning_disable_wire(
    wire: ResolvedProtocol,
    effort: Option<ReasoningEffort>,
    evidence: &OpenRouterChatModelEvidence,
) -> bool {
    matches!(wire, ResolvedProtocol::Chat)
        && effort.is_none()
        && evidence
            .reasoning()
            .is_some_and(|reasoning| reasoning.default_enabled() && !reasoning.mandatory())
}

fn openrouter_endpoint_admission(
    evidence: &OpenRouterChatModelEvidence,
    wire: ResolvedProtocol,
    effort_requested: bool,
    reasoning_disable_wire: bool,
) -> Result<(), String> {
    let reasoning_on_wire = effort_requested || reasoning_disable_wire;
    if evidence
        .gateway_admits(openrouter_protocol_id(wire), reasoning_on_wire)
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    Err(openrouter_endpoint_rejection(
        wire,
        reasoning_on_wire,
        evidence.endpoints(),
    ))
}

fn openrouter_endpoint_rejection(
    wire: ResolvedProtocol,
    reasoning_on_wire: bool,
    endpoints: &[OpenRouterChatEndpointCandidate],
) -> String {
    openrouter_endpoint_rejection_for_parameters(
        wire,
        reasoning_on_wire,
        &endpoints
            .iter()
            .map(|endpoint| {
                (
                    endpoint.supported_parameters().clone(),
                    endpoint.tool_choice(),
                )
            })
            .collect::<Vec<_>>(),
    )
}

fn openrouter_endpoint_rejection_for_parameters(
    wire: ResolvedProtocol,
    reasoning_on_wire: bool,
    endpoints: &[(BTreeSet<String>, OpenRouterToolChoiceModes)],
) -> String {
    // Diagnostic vocabulary only; provider evidence owns admission eligibility.
    let mut parameters = vec!["max_tokens", "tools"];
    if !matches!(wire, ResolvedProtocol::Chat) {
        parameters.push("tool_choice");
    }
    if reasoning_on_wire {
        parameters.push("reasoning");
    }
    let require_tool_choice_auto = matches!(wire, ResolvedProtocol::Messages);
    let mut observed = BTreeSet::new();
    let mut any_tool_choice_auto_failure = false;
    for (supported_parameters, tool_choice) in endpoints {
        observed.extend(supported_parameters.iter().map(String::as_str));
        let names_satisfied = parameters
            .iter()
            .all(|parameter| supported_parameters.contains(*parameter));
        if names_satisfied
            && require_tool_choice_auto
            && supported_parameters.contains("tools")
            && !tool_choice.auto
        {
            any_tool_choice_auto_failure = true;
        }
    }
    if any_tool_choice_auto_failure {
        return "OpenRouter exact endpoint discovery does not admit the complete selected-wire request required by the Plak agent: endpoint attests tools without tool_choice auto".to_owned();
    }
    let missing = parameters
        .iter()
        .filter(|parameter| !observed.contains(*parameter))
        .copied()
        .collect::<Vec<_>>();
    let detail = if missing.is_empty() {
        "the required parameters are split across endpoints; no one endpoint attests the complete request".to_owned()
    } else {
        format!(
            "discovery does not attest required parameter(s): {}",
            missing.join(", ")
        )
    };
    format!(
        "OpenRouter exact endpoint discovery does not admit the complete selected-wire request required by the Plak agent: {detail}"
    )
}

/// The output rung is only one admission fact. Equal endpoint admission
/// envelopes preserve resume; even an in-rung change to a minimum guarantee
/// rotates the provider admission identity and refuses exact reopen.
fn openrouter_output_bound(
    evidence: &OpenRouterChatModelEvidence,
    wire: ResolvedProtocol,
    reasoning_on_wire: bool,
) -> CliResult<NonZeroU32> {
    evidence
        .gateway_output_bound(openrouter_protocol_id(wire), reasoning_on_wire)
        .map_err(|error| CliFailure::new(error.to_string()))
}

fn openrouter_protocol_id(wire: ResolvedProtocol) -> &'static str {
    match wire {
        ResolvedProtocol::Chat => plakous::openrouter::OPENROUTER_CHAT_PROTOCOL_ID,
        ResolvedProtocol::Responses => plakous::openrouter::OPENROUTER_RESPONSES_PROTOCOL_ID,
        ResolvedProtocol::Messages => plakous::openrouter::OPENROUTER_MESSAGES_PROTOCOL_ID,
    }
}

fn openrouter_maximum_projected_input(
    evidence: &OpenRouterChatModelEvidence,
    wire: ResolvedProtocol,
    reasoning_on_wire: bool,
    output_bound: NonZeroU32,
) -> CliResult<u64> {
    evidence
        .gateway_maximum_projected_input(
            openrouter_protocol_id(wire),
            reasoning_on_wire,
            output_bound,
        )
        .map_err(|error| CliFailure::new(error.to_string()))
}

fn reasoning_effort_wire_spelling(effort: ReasoningEffort) -> Result<&'static str, CliFailure> {
    match effort {
        ReasoningEffort::Low => Ok("low"),
        ReasoningEffort::Medium => Ok("medium"),
        ReasoningEffort::High => Ok("high"),
        ReasoningEffort::XHigh => Ok("xhigh"),
        ReasoningEffort::Max => Ok("max"),
        ReasoningEffort::Ultra => Ok("ultra"),
        ReasoningEffort::TokenBudget(_) => {
            Err(CliFailure::new("OpenRouter has no budget selector"))
        }
        _ => Err(CliFailure::new(
            "requested OpenRouter reasoning effort is outside the admitted selector set",
        )),
    }
}

fn reasoning_disposition(
    wire: ResolvedProtocol,
    effort: Option<ReasoningEffort>,
    evidence: &OpenRouterChatModelEvidence,
) -> CliResult<ReasoningEffortDisposition> {
    let reasoning = evidence.reasoning();
    let budget_mapped = budget_mapped_efforts();
    reasoning_disposition_for_efforts(
        wire,
        effort,
        reasoning
            .and_then(OpenRouterReasoningFacts::supported_efforts)
            .or_else(|| {
                reasoning
                    .is_some_and(OpenRouterReasoningFacts::supports_max_tokens)
                    .then_some(budget_mapped.as_slice())
            }),
    )
}

/// A model that attests a reasoning token budget (`supports_max_tokens`) but
/// enumerates no efforts takes OpenRouter's normalized `low`/`medium`/`high`,
/// which the gateway maps onto a share of that budget.
fn budget_mapped_efforts() -> Vec<String> {
    ["low", "medium", "high"].map(str::to_owned).to_vec()
}

fn reasoning_disposition_for_efforts(
    wire: ResolvedProtocol,
    effort: Option<ReasoningEffort>,
    supported_efforts: Option<&[String]>,
) -> CliResult<ReasoningEffortDisposition> {
    let Some(effort) = effort else {
        return Ok(ReasoningEffortDisposition::Disabled);
    };
    let wire_spelling = reasoning_effort_wire_spelling(effort)?;
    let Some(supported_efforts) = supported_efforts else {
        return Err(CliFailure::new(format!(
            "OpenRouter {wire:?} model attests no reasoning effort enumeration"
        )));
    };
    if supported_efforts
        .iter()
        .any(|effort| effort == wire_spelling)
    {
        return Ok(ReasoningEffortDisposition::Enabled(effort));
    }
    Err(CliFailure::new(format!(
        "OpenRouter {wire:?} does not enumerate reasoning effort \"{wire_spelling}\" for this model (attested: [{}])",
        supported_efforts.join(", ")
    )))
}

fn openrouter_runtime_binding(
    spec: &ProductOpenRouterSpec,
    configured: ConfiguredLanguageModel,
    registry: plakous::tools::FrozenToolRegistry,
    fence: crate::agent::SessionToolFence,
    clock: Arc<dyn plakous::model::Clock>,
) -> CliResult<SessionRuntimeBinding> {
    crate::api_runtime::runtime_binding(
        &crate::api_runtime::ApiRuntimeSpec {
            job_config: spec.job_config,
            account: "openrouter",
            sidecars: spec.sidecars.as_ref(),
            product_home: spec.product_home.as_deref(),
            literal_model: &spec.literal_model,
            effort: spec.effort,
            composition: spec.composition,
            wire: spec.wire,
            workspace_root: &spec.workspace_root,
            cwd: &spec.cwd,
            plan_state_authority: &spec.plan_state_authority,
            opening_durable_session: spec.opening_durable_session,
            tool_fence: fence,
            scheduled_compaction: spec.scheduled_compaction,
            max_model_rounds: spec.max_model_rounds,
            topology_extensions: &spec.topology_extensions,
            telemetry: spec.telemetry.as_ref(),
            spawned_child: spec.spawned_child,
        },
        configured,
        registry,
        clock,
    )
}

#[cfg(test)]
pub(crate) fn openrouter_product_profile(
    composition: CompositionSelection,
    effort: Option<ReasoningEffort>,
    wire: ResolvedProtocol,
    literal_model: &str,
    subagents: bool,
    workspace_root: &Path,
    cwd: &Path,
    surface: &CompiledSurface,
) -> CliResult<plakous::agent::HarnessProfileSpec> {
    crate::api_runtime::api_product_profile(
        "openrouter",
        composition,
        effort,
        wire,
        literal_model,
        subagents,
        workspace_root,
        cwd,
        surface,
    )
}

/// Result of one on-demand OpenRouter inference-key allowance read.
#[derive(Clone, Debug)]
pub enum OpenRouterKeyUsageProbe {
    /// No OpenRouter key in the product secret store.
    NotConfigured,
    /// The authenticated read succeeded, with the product's safe key label.
    Ok(Box<OpenRouterKeyUsage>, String),
    /// A named failure carrying no secret material.
    Failed(String),
}

/// Reads the OpenRouter inference key's own allowance with one authenticated
/// `GET /api/v1/key`.
///
/// This is invoked on demand when the Usage surface is opened, never on the
/// turn path. It never requests the management-key Credits endpoint, which an
/// inference key cannot call.
#[must_use]
pub fn probe_openrouter_key_usage(
    credentials: &dyn OpenRouterCredentials,
) -> OpenRouterKeyUsageProbe {
    // "Is a key configured at all" is the credential's own preflight; an
    // unconfigured product is `NotConfigured`, not a failure.
    if credentials.preflight().is_err() {
        return OpenRouterKeyUsageProbe::NotConfigured;
    }
    let source = match credentials.source() {
        Ok(source) => source,
        Err(error) => return OpenRouterKeyUsageProbe::Failed(error.message().to_owned()),
    };
    let label = match credentials.key_label() {
        Ok(label) => label,
        Err(error) => return OpenRouterKeyUsageProbe::Failed(error.message().to_owned()),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return OpenRouterKeyUsageProbe::Failed(error.to_string()),
    };
    runtime.block_on(async move {
        let client = match OpenRouterChatClient::connect_with_retention(
            source,
            OpenRouterRetentionPolicy::gateway_default(),
        )
        .and_then(OpenRouterChatClient::with_plak_attribution)
        {
            Ok(client) => client,
            Err(error) => {
                return OpenRouterKeyUsageProbe::Failed(format!(
                    "OpenRouter connection is invalid: {error}"
                ));
            }
        };
        match client.key_usage().await {
            Ok(usage) => OpenRouterKeyUsageProbe::Ok(Box::new(usage), label),
            Err(error) => OpenRouterKeyUsageProbe::Failed(key_usage_failure(&error, &label)),
        }
    })
}

fn key_usage_failure(error: &plakous::openrouter::OpenRouterKeyUsageError, label: &str) -> String {
    use plakous::openrouter::OpenRouterKeyUsageError;
    let status = match error {
        OpenRouterKeyUsageError::Unauthorized => "HTTP 401: invalid or revoked",
        OpenRouterKeyUsageError::Forbidden => "HTTP 403: forbidden",
        _ => return error.to_string(),
    };
    let remedy = if label == "OPENROUTER_API_KEY" {
        "unset it to use the key file, or replace it"
    } else {
        "run `plak key set openrouter` to replace it"
    };
    format!("OpenRouter rejected the API key from {label} ({status}); {remedy}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit fixtures fail fast")]
mod tests {
    fn chat_review_clock() -> Arc<dyn plakous::model::Clock> {
        let now = plakous::model::Timestamp::new(
            plakous::openrouter::OPENROUTER_CHAT_REVIEW_EXPIRES_AT - 30 * 86_400 + 60,
            0,
        )
        .unwrap();
        Arc::new(plakous::model::FixedClock::new(now))
    }

    use super::*;

    #[test]
    fn key_usage_rejection_names_status_source_and_remedy_only() {
        use plakous::openrouter::OpenRouterKeyUsageError;
        for (error, status) in [
            (
                OpenRouterKeyUsageError::Unauthorized,
                "HTTP 401: invalid or revoked",
            ),
            (OpenRouterKeyUsageError::Forbidden, "HTTP 403: forbidden"),
        ] {
            let env = key_usage_failure(&error, "OPENROUTER_API_KEY");
            assert!(env.contains("from OPENROUTER_API_KEY"));
            assert!(env.contains(status));
            assert!(env.contains("unset it to use the key file, or replace it"));
            let file = key_usage_failure(&error, "sha256:1234abcd");
            assert!(file.contains("from sha256:1234abcd"));
            assert!(file.contains(status));
            assert!(file.contains("plak key set openrouter"));
        }
    }
    use crate::agent::enroll_product_instructions;
    use plakous::agent::InstructionCompositionStrategy;
    use std::cell::Cell;
    use std::num::NonZeroU32;
    use std::sync::Mutex;

    use plakous::tools::ToolRegistryBuilder;

    /// The pi surface with no topology extensions.
    fn pi_surface() -> CompiledSurface {
        crate::tools::test_support::surface(CodingPresentation::Pi, &[])
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct TestEvidence {
        literal: &'static str,
        wire: ResolvedProtocol,
        expires_at: u64,
        discovery: u8,
    }

    async fn resolve_test_evidence(
        cached: Option<TestEvidence>,
        literal: &'static str,
        wire: ResolvedProtocol,
        now: u64,
        discoveries: &Cell<usize>,
    ) -> TestEvidence {
        reuse_or_discover_evidence(
            cached,
            |evidence| {
                evidence.literal == literal && evidence.wire == wire && now < evidence.expires_at
            },
            || async {
                discoveries.set(discoveries.get() + 1);
                Ok::<_, std::convert::Infallible>(TestEvidence {
                    literal,
                    wire,
                    expires_at: now + 10,
                    discovery: 2,
                })
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn initial_openrouter_construction_discovers() {
        let discoveries = Cell::new(0);
        let evidence = resolve_test_evidence(
            None,
            "author/model",
            ResolvedProtocol::Chat,
            5,
            &discoveries,
        )
        .await;

        assert_eq!(discoveries.get(), 1);
        assert_eq!(evidence.discovery, 2);
    }

    #[tokio::test]
    async fn same_binding_clear_reuses_evidence_without_discovery() {
        let discoveries = Cell::new(0);
        let cached = TestEvidence {
            literal: "author/model",
            wire: ResolvedProtocol::Messages,
            expires_at: 20,
            discovery: 1,
        };
        let evidence = resolve_test_evidence(
            Some(cached),
            "author/model",
            ResolvedProtocol::Messages,
            5,
            &discoveries,
        )
        .await;

        assert_eq!(discoveries.get(), 0);
        assert_eq!(evidence, cached);
    }

    #[tokio::test]
    async fn mismatched_or_expired_evidence_is_not_reused() {
        for cached in [
            TestEvidence {
                literal: "other/model",
                wire: ResolvedProtocol::Chat,
                expires_at: 20,
                discovery: 1,
            },
            TestEvidence {
                literal: "author/model",
                wire: ResolvedProtocol::Responses,
                expires_at: 20,
                discovery: 1,
            },
            TestEvidence {
                literal: "author/model",
                wire: ResolvedProtocol::Chat,
                expires_at: 5,
                discovery: 1,
            },
        ] {
            let discoveries = Cell::new(0);
            let evidence = resolve_test_evidence(
                Some(cached),
                "author/model",
                ResolvedProtocol::Chat,
                5,
                &discoveries,
            )
            .await;
            assert_eq!(discoveries.get(), 1);
            assert_eq!(evidence.discovery, 2);
        }
    }

    #[test]
    fn absent_reasoning_effort_is_disabled() {
        for wire in [
            ResolvedProtocol::Chat,
            ResolvedProtocol::Responses,
            ResolvedProtocol::Messages,
        ] {
            assert_eq!(
                reasoning_disposition_for_efforts(wire, None, Some(&["medium".to_owned()]))
                    .unwrap(),
                ReasoningEffortDisposition::Disabled
            );
        }
    }

    #[test]
    fn enumerated_reasoning_effort_is_admitted() {
        let efforts = ["high".to_owned(), "medium".to_owned(), "low".to_owned()];
        assert_eq!(
            reasoning_disposition_for_efforts(
                ResolvedProtocol::Chat,
                Some(ReasoningEffort::Medium),
                Some(&efforts),
            )
            .unwrap(),
            ReasoningEffortDisposition::Enabled(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn unenumerated_reasoning_effort_rejects_with_attested_list() {
        let error = reasoning_disposition_for_efforts(
            ResolvedProtocol::Responses,
            Some(ReasoningEffort::Medium),
            Some(&["high".to_owned(), "low".to_owned()]),
        )
        .unwrap_err();
        assert!(
            error
                .message()
                .contains("does not enumerate reasoning effort \"medium\"")
        );
        assert!(error.message().contains("attested: [high, low]"));
    }

    #[test]
    fn reasoning_effort_rejects_without_enumeration_object() {
        for wire in [
            ResolvedProtocol::Chat,
            ResolvedProtocol::Responses,
            ResolvedProtocol::Messages,
        ] {
            let error =
                reasoning_disposition_for_efforts(wire, Some(ReasoningEffort::Medium), None)
                    .unwrap_err();
            assert!(
                error
                    .message()
                    .contains("attests no reasoning effort enumeration"),
                "{wire:?}: {}",
                error.message()
            );
        }
    }

    #[test]
    fn a_budget_only_reasoning_model_takes_normalized_efforts() {
        let mapped = budget_mapped_efforts();
        for wire in [
            ResolvedProtocol::Chat,
            ResolvedProtocol::Responses,
            ResolvedProtocol::Messages,
        ] {
            assert!(matches!(
                reasoning_disposition_for_efforts(wire, Some(ReasoningEffort::Low), Some(&mapped)),
                Ok(ReasoningEffortDisposition::Enabled(ReasoningEffort::Low))
            ));
            assert!(
                reasoning_disposition_for_efforts(wire, Some(ReasoningEffort::Max), Some(&mapped))
                    .is_err()
            );
        }
    }

    #[test]
    fn token_budget_effort_rejects_openrouter_budget_selector() {
        let error = reasoning_disposition_for_efforts(
            ResolvedProtocol::Chat,
            Some(ReasoningEffort::TokenBudget(NonZeroU32::new(128).unwrap())),
            Some(&["medium".to_owned()]),
        )
        .unwrap_err();
        assert!(
            error
                .message()
                .contains("OpenRouter has no budget selector")
        );
    }

    #[test]
    fn messages_coding_shape_rejects_tool_choice_auto_false() {
        let parameters = BTreeSet::from([
            "max_tokens".to_owned(),
            "tools".to_owned(),
            "tool_choice".to_owned(),
        ]);
        let tool_choice = OpenRouterToolChoiceModes {
            none: true,
            auto: false,
            required: false,
            function: false,
        };
        let rejection = openrouter_endpoint_rejection_for_parameters(
            ResolvedProtocol::Messages,
            false,
            &[(parameters, tool_choice)],
        );
        assert!(rejection.contains("endpoint attests tools without tool_choice auto"));
    }

    #[test]
    fn endpoint_rejection_names_missing_fields_without_relaxing_exact_admission() {
        let missing = openrouter_endpoint_rejection_for_parameters(
            ResolvedProtocol::Chat,
            false,
            &[(
                BTreeSet::from(["tools".to_owned()]),
                OpenRouterToolChoiceModes::default(),
            )],
        );
        assert!(missing.contains("max_tokens"));

        let split = openrouter_endpoint_rejection_for_parameters(
            ResolvedProtocol::Chat,
            false,
            &[
                (
                    BTreeSet::from(["max_tokens".to_owned()]),
                    OpenRouterToolChoiceModes {
                        none: false,
                        auto: true,
                        required: false,
                        function: false,
                    },
                ),
                (
                    BTreeSet::from(["tools".to_owned()]),
                    OpenRouterToolChoiceModes::default(),
                ),
            ],
        );
        assert!(split.contains("split across endpoints"));
    }

    #[derive(Clone, Copy)]
    struct AdmissionProbe {
        has_reasoning_param: bool,
        tool_choice_auto: bool,
        max_completion_tokens: Option<u64>,
        max_prompt_tokens: Option<u64>,
    }

    fn chat_tool_choice(auto: bool) -> OpenRouterToolChoiceModes {
        OpenRouterToolChoiceModes {
            none: false,
            auto,
            required: false,
            function: false,
        }
    }

    fn chat_parameters(probe: AdmissionProbe) -> BTreeSet<String> {
        let mut parameters = BTreeSet::from([
            "max_tokens".to_owned(),
            "tools".to_owned(),
            "tool_choice".to_owned(),
        ]);
        if probe.has_reasoning_param {
            parameters.insert("reasoning".to_owned());
        }
        parameters
    }

    fn endpoint_json(probe: AdmissionProbe, tag: &str) -> serde_json::Value {
        let mut parameters = vec!["max_tokens", "tools", "tool_choice"];
        if probe.has_reasoning_param {
            parameters.push("reasoning");
        }
        serde_json::json!({
            "tag": tag,
            "provider_name": "Provider",
            "supported_parameters": parameters,
            "supports_tool_choice": {
                "none": false,
                "auto": probe.tool_choice_auto,
                "required": false,
                "function": false
            },
            "max_prompt_tokens": probe.max_prompt_tokens,
            "max_completion_tokens": probe.max_completion_tokens,
        })
    }

    async fn discover_admission_evidence(
        probes: &[(AdmissionProbe, &str)],
        context_length: u64,
        default_enabled_reasoning: bool,
    ) -> OpenRouterChatModelEvidence {
        use std::io;
        use std::sync::Arc;

        use plakous::openrouter::{OpenRouterApiKey, StaticOpenRouterApiKeySource};
        use plakous_http::{Client, Limits, TlsConfigIdentity};
        use plakous_openrouter::{
            OpenRouterChatDiscoveryConfig, OpenRouterChatRoute, OpenRouterOrigin,
            OpenRouterRetentionPolicy,
        };
        use rcgen::{CertifiedKey, generate_simple_self_signed};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::{ClientConfig, RootCertStore, ServerConfig};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;

        const MODEL: &str = "vendor/admission-test";

        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("cert");
        let certificate = cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("protocols")
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .expect("server cert");
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate.to_vec()))
            .expect("root cert");
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocols")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = Client::with_tls_config_and_identity(
            Limits::default(),
            client_config,
            TlsConfigIdentity::from_parts(
                1,
                &[b"plak-host-openrouter-admission-v1", certificate.as_ref()],
            )
            .expect("tls identity"),
        )
        .expect("client");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let origin = format!("https://{}", listener.local_addr().expect("addr"));
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let reasoning = if default_enabled_reasoning {
            serde_json::json!({
                "supported_efforts": ["medium"],
                "default_enabled": true,
                "supports_max_tokens": false,
                "mandatory": false
            })
        } else {
            serde_json::Value::Null
        };
        let models = serde_json::json!({
            "data": [{
                "id": MODEL,
                "canonical_slug": MODEL,
                "context_length": context_length,
                "supported_parameters": ["tools", "tool_choice"],
                "reasoning": reasoning,
            }],
            "links": {},
            "total_count": 1
        })
        .to_string();
        let endpoints = serde_json::json!({
            "data": {
                "id": MODEL,
                "endpoints": probes
                    .iter()
                    .map(|(probe, tag)| endpoint_json(*probe, tag))
                    .collect::<Vec<_>>(),
            }
        })
        .to_string();
        let replies = [
            ("application/json", models),
            ("application/json", endpoints),
        ];
        let serve = tokio::spawn(async move {
            for (content_type, body) in replies {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
            Ok::<_, io::Error>(())
        });
        let source = Arc::new(
            StaticOpenRouterApiKeySource::new(
                OpenRouterApiKey::new("test-catalog-key").expect("api key"),
                "test-account",
                "test-authority",
            )
            .expect("source"),
        );
        let route = OpenRouterChatRoute::new_local_fixture_with_retention(
            source,
            OpenRouterOrigin::new(origin).expect("origin"),
            OpenRouterRetentionPolicy::strict(),
        )
        .expect("route")
        .with_clock(chat_review_clock());
        let evidence = route
            .discover_chat_model(&client, MODEL, OpenRouterChatDiscoveryConfig::default())
            .await
            .expect("discovery");
        serve.await.expect("serve").expect("serve io");
        evidence
    }

    #[tokio::test]
    async fn admission_and_projected_input_share_reasoning_on_wire_eligible_set() {
        let wire = ResolvedProtocol::Chat;
        let reasoning_capable = AdmissionProbe {
            has_reasoning_param: true,
            tool_choice_auto: true,
            max_completion_tokens: Some(u64::from(NATIVE_RUN_MAX_OUTPUT_TOKENS)),
            max_prompt_tokens: Some(32_000),
        };
        let completion_only = AdmissionProbe {
            has_reasoning_param: false,
            tool_choice_auto: true,
            max_completion_tokens: Some(128),
            max_prompt_tokens: Some(64_000),
        };
        let param_only_reasoning = AdmissionProbe {
            has_reasoning_param: true,
            tool_choice_auto: true,
            max_completion_tokens: Some(u64::from(NATIVE_RUN_MAX_OUTPUT_TOKENS)),
            max_prompt_tokens: Some(48_000),
        };
        let evidence = discover_admission_evidence(
            &[
                (reasoning_capable, "provider/reasoning-capable"),
                (completion_only, "provider/completion-only"),
                (param_only_reasoning, "provider/param-only-reasoning"),
            ],
            65_536,
            false,
        )
        .await;
        assert!(
            evidence
                .gateway_admits(openrouter_protocol_id(wire), false)
                .unwrap()
        );
        assert!(
            evidence
                .gateway_admits(openrouter_protocol_id(wire), true)
                .unwrap()
        );
        assert!(openrouter_endpoint_admission(&evidence, wire, false, false).is_ok());
        assert!(openrouter_endpoint_admission(&evidence, wire, false, true).is_ok());
        let param_only_evidence = discover_admission_evidence(
            &[(
                AdmissionProbe {
                    has_reasoning_param: true,
                    tool_choice_auto: true,
                    max_completion_tokens: Some(u64::from(NATIVE_RUN_MAX_OUTPUT_TOKENS)),
                    max_prompt_tokens: Some(48_000),
                },
                "provider/param-only-reasoning",
            )],
            65_536,
            false,
        )
        .await;
        assert!(openrouter_endpoint_admission(&param_only_evidence, wire, false, true).is_ok());

        // The derived bound is what the capacity rule reserves, and the
        // completion-bound proof is unchanged: `completion-only` attests 128,
        // far under the budget a route must prove, so the reasoning-free
        // eligible set is still refused.
        let bound_without_reasoning = openrouter_output_bound(&evidence, wire, false).unwrap();
        assert_eq!(bound_without_reasoning.get(), NATIVE_RUN_MAX_OUTPUT_TOKENS);
        assert_eq!(
            openrouter_maximum_projected_input(&evidence, wire, false, bound_without_reasoning)
                .unwrap_err()
                .message(),
            "OpenRouter endpoint discovery does not prove the configured completion bound"
        );
        let bound_with_reasoning = openrouter_output_bound(&evidence, wire, true).unwrap();
        assert_eq!(bound_with_reasoning.get(), NATIVE_RUN_MAX_OUTPUT_TOKENS);
        assert_eq!(
            openrouter_maximum_projected_input(&evidence, wire, true, bound_with_reasoning)
                .unwrap(),
            32_000
        );

        let rejection = openrouter_endpoint_rejection_for_parameters(
            wire,
            true,
            &[(
                chat_parameters(AdmissionProbe {
                    has_reasoning_param: false,
                    tool_choice_auto: true,
                    max_completion_tokens: Some(u64::from(NATIVE_RUN_MAX_OUTPUT_TOKENS)),
                    max_prompt_tokens: Some(48_000),
                }),
                chat_tool_choice(true),
            )],
        );
        assert!(rejection.contains("reasoning"));
        assert!(!rejection.contains("supports_reasoning"));
    }

    #[tokio::test]
    async fn default_enabled_reasoning_disable_admits_messages_not_chat_without_reasoning_param() {
        let endpoint = AdmissionProbe {
            has_reasoning_param: false,
            tool_choice_auto: true,
            max_completion_tokens: Some(u64::from(NATIVE_RUN_MAX_OUTPUT_TOKENS)),
            max_prompt_tokens: Some(32_000),
        };
        let evidence =
            discover_admission_evidence(&[(endpoint, "provider/completion-only")], 65_536, true)
                .await;
        assert!(!openrouter_reasoning_disable_wire(
            ResolvedProtocol::Messages,
            None,
            &evidence,
        ));
        assert!(openrouter_reasoning_disable_wire(
            ResolvedProtocol::Chat,
            None,
            &evidence,
        ));
        assert!(
            openrouter_endpoint_admission(&evidence, ResolvedProtocol::Messages, false, false,)
                .is_ok()
        );
        assert!(
            openrouter_endpoint_admission(&evidence, ResolvedProtocol::Chat, false, true,).is_err()
        );
    }

    /// Every OpenRouter protocol wears the pi pack: pi 1.1.0's four tools plus
    /// `wait` and `stop`, no search, plan or image tool.
    #[test]
    fn every_openrouter_protocol_wears_the_six_tool_pi_pack() {
        let names = plakous::tools::menu_names(
            &plakous::tools::standard_coding_menus(plakous::tools::JobConfig::default()),
            CodingPresentation::Pi,
        )
        .expect("pi menu")
        .iter()
        .map(|name| name.as_str().to_owned())
        .collect::<Vec<_>>();
        assert_eq!(names, ["bash", "edit", "read", "stop", "wait", "write"]);
        let routes = crate::coding_routes::product_presentation_routes().expect("catalog");
        for (protocol, route, profile) in [
            (
                "openrouter.chat-completions.v1",
                "openrouter.platform",
                OPENROUTER_CHAT_PROFILE,
            ),
            (
                "openrouter.responses.v1",
                "openrouter.responses",
                OPENROUTER_RESPONSES_PROFILE,
            ),
            (
                "openrouter.messages.v1",
                "openrouter.messages",
                "openrouter.messages.dynamic-custom-v1",
            ),
        ] {
            let tuple = plakous::tools::PresentationRouteTuple::selected(
                plakous::model::IntegrationClass::Custom,
                protocol,
                route,
                profile,
                "vendor/any-model",
            );
            assert_eq!(routes.admit(&tuple).expect("pi"), CodingPresentation::Pi);
        }
    }

    /// An OpenRouter root granted agent tools presents its await and
    /// interrupt as pi's `wait` and `stop`: the tree's capability claims the
    /// pack's process-only entries of the same names, nothing is presented
    /// twice, `wait_agent` and `interrupt_agent` are absent, and the claimed
    /// entries raise the tree's own text unchanged.
    #[test]
    fn an_openrouter_hub_presents_the_tree_await_as_pis_wait_and_stop_once() {
        use crate::agent_tree::ProductAgentTree;
        use crate::agent_tree_protocol::WriterMode;
        use crate::agent_tree_tools::TopologyVocabulary;
        use crate::topology_extensions::topology_tool_extensions_in;
        use std::sync::{Arc, Mutex};

        let root = tempfile::tempdir().unwrap();
        let tree = ProductAgentTree::bootstrap_root_for_test(
            "tree-pi-hub",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let extensions = topology_tool_extensions_in(
            Arc::new(Mutex::new(tree)),
            "root",
            TopologyVocabulary::WaitStop,
        )
        .unwrap();
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "pi-hub",
            CompositionSelection::Direct,
            false,
            &extensions,
        )
        .unwrap();
        let surface = crate::tools::product_surface(
            plakous::tools::JobConfig::default(),
            CodingPresentation::Pi,
            registry,
            crate::agent::rendered_prompts::suite::synthetic_target(b"pi-hub"),
            &extensions,
        )
        .unwrap();
        let names = surface
            .exposed_names()
            .iter()
            .map(plakous::model::ToolName::as_str)
            .collect::<Vec<_>>();
        for name in ["wait", "stop"] {
            assert_eq!(
                names.iter().filter(|exposed| **exposed == name).count(),
                1,
                "{name}: {names:?}"
            );
            let entry = surface
                .projection()
                .by_exposed(&plakous::model::ToolName::new(name).unwrap())
                .unwrap();
            assert!(
                entry
                    .canonical_operations()
                    .values()
                    .all(|identity| identity.name().as_str() == name),
                "{name} binds the tree's capability"
            );
            assert_eq!(format!("{:?}", entry.raising()), "ModelContentIdentityV1");
        }
        for name in ["wait_agent", "interrupt_agent"] {
            assert!(!names.contains(&name), "{name}: {names:?}");
        }
        for name in ["bash", "read", "close_agent", "spawn_agent"] {
            assert!(names.contains(&name), "{name}: {names:?}");
        }
    }

    /// A tree whose root has one child that reported with unread mail.
    fn pi_hub_tree(
        workspace: &Path,
        tree_id: &str,
    ) -> (Arc<Mutex<crate::agent_tree::ProductAgentTree>>, String) {
        use crate::agent_tree_protocol::{SendAgentMessageRequest, SpawnAgentRequest, WriterMode};

        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            workspace.to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
            .unwrap()
            .agent_id;
        tree.send_agent_message(
            &child,
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "halfway there".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child).unwrap();
        (Arc::new(Mutex::new(tree)), child)
    }

    /// The pi surface a node compiles: a hub with the tree's capability, or a
    /// leaf with none.
    fn pi_surface_on(
        tree: Option<&Arc<Mutex<crate::agent_tree::ProductAgentTree>>>,
        composition: CompositionSelection,
    ) -> CompiledSurface {
        let extensions = tree.map_or_else(
            || crate::tools::ProductToolExtensions::new([]).unwrap(),
            |tree| {
                crate::topology_extensions::topology_tool_extensions_in(
                    Arc::clone(tree),
                    "root",
                    crate::agent_tree_tools::TopologyVocabulary::WaitStop,
                )
                .unwrap()
            },
        );
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "pi-hub-calls",
            composition,
            false,
            &extensions,
        )
        .unwrap();
        let runtime = (composition == CompositionSelection::JsPtc)
            .then_some(plakous::tools::CompositionRuntime::ProductJavaScript);
        crate::tools::product_surface_with_fence(
            plakous::tools::JobConfig::default(),
            CodingPresentation::Pi,
            registry,
            crate::agent::rendered_prompts::suite::synthetic_target(b"pi-hub-calls"),
            &extensions,
            crate::agent::SessionToolFence::Full,
            runtime,
        )
        .unwrap()
    }

    /// One model (or cell) call through a compiled surface, the way the tool
    /// runtime runs it: prepared by the projection, executed by the
    /// registered executor (the tree's `TopologyTool` for a claimed `wait` or
    /// `stop`) and raised by the projection. Returns the outcome kind and the
    /// text the caller sees.
    async fn call_through(
        surface: &CompiledSurface,
        environment: &Arc<plakous_tools::ExecutionEnvironment>,
        call: u128,
        name: &str,
        arguments: &serde_json::Value,
        lineage: plakous_model::ToolCallerLineage,
    ) -> (plakous::tools::ToolOutcomeKind, String) {
        use plakous_model::{CancellationToken, Clock, FixedClock, Timestamp};
        use plakous_tools::{EffectAuthorizationGrantSource, ProgressSink, ToolPolicyRevision};
        use std::str::FromStr;

        let request = plakous_tools::ToolInvocationRequest::new(
            plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000101").unwrap(),
            plakous_model::TurnId::from_str("00000000-0000-0000-0000-000000000102").unwrap(),
            plakous_model::AttemptId::from_str("00000000-0000-0000-0000-000000000103").unwrap(),
            plakous_model::CallId::from_str(&uuid::Uuid::from_u128(call).to_string()).unwrap(),
            plakous_model::ToolName::new(name).unwrap(),
            plakous_model::ToolCorrelation::new(None, None, lineage).unwrap(),
            arguments.to_string(),
        );
        let projection = surface.projection();
        let prepared = projection.prepare(request).unwrap();
        let receipt = prepared.receipt().cloned().expect("projection receipt");
        let run_id = prepared.invocation().run_id();
        let turn_id = prepared.invocation().turn_id();
        let attempt_id = prepared.invocation().attempt_id();
        let call_id = prepared.invocation().call_id();
        let (invocation, _) = prepared.into_parts();
        let binding = plakous_tools::internal::authorization_binding(
            &invocation,
            environment.descriptor().clone(),
            ToolPolicyRevision::new("pi-hub-calls", 1).unwrap(),
        )
        .unwrap();
        let grant = plakous_tools::internal::authorization_grant(
            binding,
            EffectAuthorizationGrantSource::Policy,
        );
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(72, 0).unwrap()));
        let (progress, _receiver) = ProgressSink::channel(8, call_id, Arc::clone(&clock)).unwrap();
        let context = plakous_tools::internal::tool_context(
            run_id,
            turn_id,
            attempt_id,
            CancellationToken::new(),
            None,
            clock,
            Arc::clone(environment),
            grant,
            progress,
        );
        let executor = plakous_tools::internal::executor(surface.registry(), &invocation).unwrap();
        let outcome = executor.execute(invocation, context).await.unwrap();
        let raised = projection.raise(&receipt, outcome).unwrap();
        let text = raised
            .parts()
            .iter()
            .filter_map(|part| match part {
                plakous_model::ToolResultPart::Text(text) => Some(text.text()),
                _ => None,
            })
            .collect::<String>();
        (raised.kind(), text)
    }

    fn host_environment(workspace: &Path) -> Arc<plakous_tools::ExecutionEnvironment> {
        use std::str::FromStr;
        Arc::new(
            plakous_tools::ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(
                    plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000101").unwrap(),
                ),
                workspace,
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
        )
    }

    /// Pi's `wait` and `stop` on an OpenRouter hub run through the compiled
    /// hub projection and the tree's own tool (spec 03 §4.4): a Direct `wait`
    /// naming a child is the await in the topology envelope, a bare `wait`
    /// with nothing pending is the process tool's bare await, and a `stop` on
    /// a process id is the process stop, both with a leaf's bytes. In a js-ptc
    /// cell the same `wait` returns the raw receipt and `stop` naming a
    /// running child the raw interrupt receipt.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_openrouter_hubs_wait_and_stop_run_through_the_compiled_projection() {
        use plakous::tools::ToolOutcomeKind;
        use plakous_model::ToolCallerLineage;
        use serde_json::{Value, json};
        use std::str::FromStr;

        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("hub")).unwrap();
        std::fs::create_dir_all(workspace.path().join("leaf")).unwrap();
        let (tree, child) = pi_hub_tree(workspace.path(), "tree-pi-hub-calls");
        let hub = pi_surface_on(Some(&tree), CompositionSelection::Direct);
        let leaf = pi_surface_on(None, CompositionSelection::Direct);
        let hub_env = host_environment(&workspace.path().join("hub"));
        let leaf_env = host_environment(&workspace.path().join("leaf"));

        let (kind, text) = call_through(
            &hub,
            &hub_env,
            1,
            "wait",
            &json!({"ids": [child], "timeout_ms": 5000}),
            ToolCallerLineage::Direct,
        )
        .await;
        assert_eq!(kind, ToolOutcomeKind::Success, "{text}");
        let envelope: Value = serde_json::from_str(&text).unwrap();
        let receipt: Value =
            serde_json::from_str(envelope["result"].as_str().expect("topology envelope")).unwrap();
        assert_eq!(
            receipt["agents"][0]["agent_id"],
            child.as_str(),
            "{receipt}"
        );
        assert_eq!(receipt["mail"][0]["body"], "halfway there", "{receipt}");

        // Nothing pending now: a bare wait, then a process start and its stop,
        // show a leaf's bytes.
        let mut shown = Vec::new();
        for (surface, environment) in [(&hub, &hub_env), (&leaf, &leaf_env)] {
            let mut texts = Vec::new();
            for (call, name, arguments) in [
                (2, "wait", json!({"timeout_ms": 0})),
                (3, "bash", json!({"command": "sleep 30", "timeout": 0})),
                (4, "stop", json!({"id": "p1"})),
            ] {
                let (kind, text) = call_through(
                    surface,
                    environment,
                    call,
                    name,
                    &arguments,
                    ToolCallerLineage::Direct,
                )
                .await;
                texts.push((name, kind, text));
            }
            shown.push(texts);
        }
        let [hub_texts, leaf_texts] = shown.as_slice() else {
            panic!("two runs");
        };
        assert_eq!(
            hub_texts[2].1,
            ToolOutcomeKind::Success,
            "{:?}",
            hub_texts[2]
        );
        assert_eq!(hub_texts[0], leaf_texts[0]);
        assert_eq!(hub_texts[2], leaf_texts[2]);

        // One js-ptc composition: the cell sees the raw receipts.
        let (tree, child) = pi_hub_tree(workspace.path(), "tree-pi-hub-cell");
        let hub = pi_surface_on(Some(&tree), CompositionSelection::JsPtc);
        let cell = || {
            ToolCallerLineage::local_programmatic(
                41,
                7,
                "cell-1",
                1,
                plakous_model::CallId::from_str(&uuid::Uuid::from_u128(9).to_string()).unwrap(),
                1,
            )
            .unwrap()
        };
        let (kind, text) = call_through(
            &hub,
            &hub_env,
            11,
            "wait",
            &json!({"ids": [child], "timeout_ms": 5000}),
            cell(),
        )
        .await;
        assert_eq!(kind, ToolOutcomeKind::Success, "{text}");
        let raw: Value = serde_json::from_str(&text).unwrap();
        assert!(raw.get("result").is_none(), "{raw}");
        assert_eq!(raw["mail"][0]["body"], "halfway there", "{raw}");
        let running = {
            let mut tree = tree.lock().unwrap();
            let running = tree
                .spawn_agent(
                    "root",
                    &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                    Some("op-spawn-running"),
                )
                .unwrap()
                .agent_id;
            tree.set_active_turn(&running, true).unwrap();
            running
        };
        let (kind, text) =
            call_through(&hub, &hub_env, 12, "stop", &json!({"id": running}), cell()).await;
        assert_eq!(kind, ToolOutcomeKind::Success, "{text}");
        let raw: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(raw["status"], "interrupted", "{raw}");
        assert_eq!(raw["agent_id"], running.as_str(), "{raw}");
    }

    #[test]
    fn product_profiles_identify_plak_and_the_exact_openrouter_target() {
        let (_tmp, root, cwd) = {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let cwd = root.join("nested");
            std::fs::create_dir_all(&cwd).unwrap();
            (tmp, root, cwd)
        };
        for (wire, _wire_name) in [
            (ResolvedProtocol::Chat, "chat"),
            (ResolvedProtocol::Responses, "responses"),
            (ResolvedProtocol::Messages, "messages"),
        ] {
            let profile = openrouter_product_profile(
                CompositionSelection::Direct,
                None,
                wire,
                "author/model-v1",
                false,
                &root,
                &cwd,
                &pi_surface(),
            )
            .unwrap();

            assert!(profile.base_instructions().contains("Workspace root:"));
            assert!(profile.base_instructions().contains("cwd:"));
            assert!(
                profile
                    .base_instructions()
                    .contains("You are author/model-v1.")
            );
            assert!(
                profile.base_instructions().contains(
                    "This turn's prompts/instructions identify you; earlier messages show session/agent history."
                )
            );
            assert!(
                profile
                    .base_instructions()
                    .contains("Commit or push only when the user asks.")
            );
            assert!(!profile.base_instructions().contains("The coding tools are"));
            assert!(
                profile
                    .base_instructions()
                    .starts_with(plakous::prompts::bases::PI_PREAMBLE)
            );
            assert!(
                profile.base_instructions().contains(
                    "<tools>\n- read: Read file contents\n- bash: Execute shell commands"
                )
            );
        }
    }

    #[test]
    fn javascript_and_python_profiles_name_the_outer_schema_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let cwd = root.join("nested");
        std::fs::create_dir_all(&cwd).unwrap();
        let javascript = openrouter_product_profile(
            CompositionSelection::JsPtc,
            None,
            ResolvedProtocol::Chat,
            "author/model-v1",
            false,
            &root,
            &cwd,
            &pi_surface(),
        )
        .unwrap();
        assert_eq!(javascript.id().as_str(), "plak.openrouter.chat.js-ptc.v1");
        assert!(javascript.base_instructions().contains("JSON field source"));
        assert!(javascript.base_instructions().contains("text(...)"));
        assert!(
            javascript
                .base_instructions()
                .contains("Nested tools available inside cells:")
        );
        assert!(
            javascript
                .base_instructions()
                .contains("bash, edit, read, stop, wait, write")
        );
        assert!(!javascript.base_instructions().contains("inspect_image"));
        assert!(
            javascript
                .base_instructions()
                .contains("do not echo the source as assistant text")
        );
        assert!(
            !javascript
                .base_instructions()
                .contains("prefer apply_patch")
        );

        let python = openrouter_product_profile(
            CompositionSelection::PyPtc,
            None,
            ResolvedProtocol::Chat,
            "author/model-v1",
            false,
            &root,
            &cwd,
            &pi_surface(),
        )
        .unwrap();
        assert_eq!(python.id().as_str(), "plak.openrouter.chat.py-ptc.v1");
        assert!(
            python
                .base_instructions()
                .contains("code (string), not source")
        );
        assert!(
            python
                .base_instructions()
                .contains("do not echo the code as assistant text")
        );
        assert!(python.base_instructions().contains("Printed lines"));
        assert!(python.base_instructions().contains("wait_py"));
    }

    #[test]
    fn product_profile_enrolls_global_and_workspace_instructions() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let root = temporary.path().join("workspace");
        let cwd = root.join("nested");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("AGENTS.md"), "global OpenRouter instructions").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root OpenRouter instructions").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "nested OpenRouter instructions").unwrap();
        let profile = openrouter_product_profile(
            CompositionSelection::Direct,
            None,
            ResolvedProtocol::Responses,
            "author/model-v1",
            false,
            &root,
            &cwd,
            &pi_surface(),
        )
        .unwrap();

        let (profile, facts) =
            enroll_product_instructions(&profile, Some(&home), &root, &cwd).unwrap();

        assert_eq!(
            profile.instruction_strategy(),
            InstructionCompositionStrategy::BaseThenSources
        );
        assert_eq!(profile.instruction_sources().len(), 3);
        assert_eq!(
            facts
                .iter()
                .map(plakous::agent::ResolvedContextSourceFact::content)
                .collect::<Vec<_>>(),
            [
                "global OpenRouter instructions",
                "root OpenRouter instructions",
                "nested OpenRouter instructions",
            ]
        );
    }

    #[test]
    fn openrouter_javascript_and_python_enroll_agents_md() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let root = temporary.path().join("workspace");
        let cwd = root.join("nested");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("AGENTS.md"), "Prefer apply_patch for all edits").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root OpenRouter instructions").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "nested OpenRouter instructions").unwrap();
        for composition in [CompositionSelection::JsPtc, CompositionSelection::PyPtc] {
            let profile = openrouter_product_profile(
                composition,
                None,
                ResolvedProtocol::Chat,
                "author/model-v1",
                false,
                &root,
                &cwd,
                &pi_surface(),
            )
            .unwrap();
            let (enrolled, facts) =
                enroll_product_instructions(&profile, Some(&home), &root, &cwd).unwrap();
            assert_eq!(facts.len(), 3);
            assert_eq!(
                enrolled.instruction_strategy(),
                InstructionCompositionStrategy::BaseThenSources
            );
            assert_eq!(enrolled.instruction_sources().len(), 3);
            assert!(
                facts
                    .iter()
                    .any(|fact| fact.content().contains("Prefer apply_patch"))
            );
        }
    }

    #[test]
    fn absent_openrouter_effort_is_disabled_reasoning_and_named_effort_uses_enumeration() {
        for wire in [
            ResolvedProtocol::Chat,
            ResolvedProtocol::Responses,
            ResolvedProtocol::Messages,
        ] {
            assert_eq!(
                reasoning_disposition_for_efforts(wire, None, Some(&["medium".to_owned()]))
                    .unwrap(),
                ReasoningEffortDisposition::Disabled
            );
            let error =
                reasoning_disposition_for_efforts(wire, Some(ReasoningEffort::Medium), None)
                    .unwrap_err();
            assert!(
                error
                    .message()
                    .contains("attests no reasoning effort enumeration"),
                "{wire:?}: {}",
                error.message()
            );
        }
    }

    #[test]
    fn openrouter_compaction_registry_has_no_tools() {
        assert!(ToolRegistryBuilder::default().freeze().is_empty());
    }

    /// A reasoning route gets the budget its endpoints attest, not a fixed
    /// 4,096 that a reasoning model spends entirely on thinking.
    #[tokio::test]
    async fn the_output_bound_follows_what_the_route_attests() {
        let wire = ResolvedProtocol::Chat;
        let probe = |max_completion_tokens| AdmissionProbe {
            has_reasoning_param: true,
            tool_choice_auto: true,
            max_completion_tokens,
            max_prompt_tokens: Some(64_000),
        };

        // A route that attests a large completion budget gets it, up to the
        // product ceiling.
        let generous = discover_admission_evidence(
            &[(probe(Some(65_536)), "provider/generous")],
            262_144,
            false,
        )
        .await;
        assert_eq!(
            openrouter_output_bound(&generous, wire, true)
                .unwrap()
                .get(),
            32_768
        );
        const {
            assert!(32_768 > NATIVE_RUN_MAX_OUTPUT_TOKENS);
        }

        // A route that attests less than the ceiling gets exactly what it
        // attested.
        let modest =
            discover_admission_evidence(&[(probe(Some(8_192)), "provider/modest")], 262_144, false)
                .await;
        assert_eq!(
            openrouter_output_bound(&modest, wire, true).unwrap().get(),
            8_192
        );

        // The gateway may route to any eligible endpoint, so the smallest
        // attested budget is the one the route can rely on.
        let mixed = discover_admission_evidence(
            &[
                (probe(Some(65_536)), "provider/generous"),
                (probe(Some(8_192)), "provider/modest"),
            ],
            262_144,
            false,
        )
        .await;
        assert_eq!(
            openrouter_output_bound(&mixed, wire, true).unwrap().get(),
            8_192
        );

        // Never below the budget a route must prove, so the admission rule
        // still refuses an endpoint that attests less.
        let thin =
            discover_admission_evidence(&[(probe(Some(128)), "provider/thin")], 262_144, false)
                .await;
        let bound = openrouter_output_bound(&thin, wire, true).unwrap();
        assert_eq!(bound.get(), NATIVE_RUN_MAX_OUTPUT_TOKENS);
        assert_eq!(
            openrouter_maximum_projected_input(&thin, wire, true, bound)
                .unwrap_err()
                .message(),
            "OpenRouter endpoint discovery does not prove the configured completion bound"
        );
    }

    /// Output is reserved out of the same window the input ceiling is drawn
    /// from, so a route never reserves more than half its window and the
    /// capacity rule still holds exactly.
    #[tokio::test]
    async fn the_output_bound_leaves_room_to_read() {
        let wire = ResolvedProtocol::Chat;
        let evidence = discover_admission_evidence(
            &[(
                AdmissionProbe {
                    has_reasoning_param: true,
                    tool_choice_auto: true,
                    // The endpoint would happily take the whole window as
                    // output; the half-window rule is what stops it.
                    max_completion_tokens: Some(32_768),
                    max_prompt_tokens: Some(16_384),
                },
                "provider/half-window",
            )],
            32_768,
            false,
        )
        .await;
        let bound = openrouter_output_bound(&evidence, wire, true).unwrap();
        assert_eq!(bound.get(), 16_384);
        let input = openrouter_maximum_projected_input(&evidence, wire, true, bound).unwrap();
        assert_eq!(input, 16_384);
        // input ceiling + reserved output <= window, exactly.
        assert_eq!(input + u64::from(bound.get()), 32_768);

        // An endpoint whose own prompt ceiling ignores the output budget does
        // not widen the window: the reservation comes out first.
        let greedy_prompt = discover_admission_evidence(
            &[(
                AdmissionProbe {
                    has_reasoning_param: true,
                    tool_choice_auto: true,
                    max_completion_tokens: Some(32_768),
                    max_prompt_tokens: Some(32_000),
                },
                "provider/greedy-prompt",
            )],
            32_768,
            false,
        )
        .await;
        let bound = openrouter_output_bound(&greedy_prompt, wire, true).unwrap();
        let input = openrouter_maximum_projected_input(&greedy_prompt, wire, true, bound).unwrap();
        assert!(input + u64::from(bound.get()) <= 32_768);
    }

    /// Endpoint churn can leave the coarse output rung unchanged while rotating
    /// admission identity. Only equal admission envelopes preserve resume.
    #[tokio::test]
    async fn endpoint_churn_within_a_rung_does_not_move_the_derived_bound() {
        let wire = ResolvedProtocol::Chat;
        let probe = |max_completion_tokens| AdmissionProbe {
            has_reasoning_param: true,
            tool_choice_auto: true,
            max_completion_tokens: Some(max_completion_tokens),
            max_prompt_tokens: Some(131_072),
        };
        let mut bounds = Vec::new();
        for endpoints in [
            // One generous endpoint.
            vec![(probe(131_072), "provider/a")],
            // The gateway adds a second, less generous one.
            vec![
                (probe(131_072), "provider/a"),
                (probe(40_000), "provider/b"),
            ],
            // And a third, more so, which still sits on the same rung.
            vec![
                (probe(131_072), "provider/a"),
                (probe(40_000), "provider/b"),
                (probe(32_768), "provider/c"),
            ],
        ] {
            let evidence = discover_admission_evidence(&endpoints, 163_840, false).await;
            bounds.push(
                openrouter_output_bound(&evidence, wire, true)
                    .unwrap()
                    .get(),
            );
        }
        assert_eq!(bounds, vec![32_768, 32_768, 32_768], "{bounds:?}");

        // Crossing a rung does move it, which is the remaining exposure.
        let thin =
            discover_admission_evidence(&[(probe(9_000), "provider/thin")], 163_840, false).await;
        assert_eq!(
            openrouter_output_bound(&thin, wire, true).unwrap().get(),
            8_192
        );
        // Snapping is always downward: a route is never told it has more output
        // than it attested.
    }

    /// The request policy the session initialises with carries the route's
    /// derived bound, not the product constant.
    ///
    /// The agent revalidates the policy it is handed against
    /// `effective_request_policy(configuration)`, which reads
    /// `behavior.output_bound`. When the hosted side built its limits from
    /// `NATIVE_RUN_MAX_OUTPUT_TOKENS` instead, every OpenRouter route failed
    /// with "effective provider behavior disagrees with the agent request
    /// policy" before any dispatch. Both sides now read the behavior.
    #[tokio::test]
    async fn the_session_request_policy_follows_the_route_bound_not_the_constant() {
        let wire = ResolvedProtocol::Chat;
        let evidence = discover_admission_evidence(
            &[(
                AdmissionProbe {
                    has_reasoning_param: true,
                    tool_choice_auto: true,
                    max_completion_tokens: Some(65_536),
                    max_prompt_tokens: Some(131_072),
                },
                "provider/deepseek",
            )],
            163_840,
            false,
        )
        .await;
        let bound = openrouter_output_bound(&evidence, wire, true).unwrap();
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "output-bound-policy-test",
            CompositionSelection::FollowCatalog,
            false,
            &crate::tools::ProductToolExtensions::default(),
        )
        .unwrap();
        let behavior = openrouter_behavior(
            ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
            CompositionSelection::FollowCatalog,
            wire,
            &registry,
            &[],
            OutputBoundDisposition::DerivedFromRoute(bound),
        )
        .unwrap();
        assert_eq!(
            behavior.output_bound,
            OutputBoundDisposition::DerivedFromRoute(bound)
        );
        assert!(behavior.output_bound.is_derived_from_route());

        // Earlier candidates, by their recorded figures. cand2 recorded
        // `Enforced(4096)`, before this route derived anything; cand4 and cand5
        // recorded `DerivedFromRoute(32768)`, which is what the route still
        // attests. So the old session is told, once, and the current ones are
        // not told anything.
        let created = NonZeroU32::new(4_096).expect("created bound");
        let attested = NonZeroU32::new(32_768).expect("attested bound");
        assert_eq!(
            route_moved_output_bound(Some(OutputBoundDisposition::Enforced(created)), attested),
            Some(OutputBoundRederived {
                previous: Some(created),
                current: Some(attested),
            }),
            "a session created before the route derived anything hears about the move"
        );
        assert_eq!(
            route_moved_output_bound(
                Some(OutputBoundDisposition::DerivedFromRoute(attested)),
                attested
            ),
            None,
            "a session already on the attested figure hears nothing"
        );
        assert_eq!(
            route_moved_output_bound(Some(OutputBoundDisposition::Enforced(attested)), attested),
            None,
            "the same figure is the same budget, whichever way it was spelled"
        );
        assert_eq!(
            route_moved_output_bound(None, attested),
            None,
            "a new session has nothing to compare"
        );
        let note =
            crate::runtime::events::output_bound_rederived_note(Some(created), Some(attested));
        assert!(note.contains("4096"), "{note}");
        assert!(note.contains("32768"), "{note}");
        assert!(note.contains("start a new session"), "{note}");

        // A session already on disk keeps the disposition it was created with,
        // spelling and figure both. The figure reaches the configuration
        // identity and from there the persisted harness plan, the tool
        // projection's provider target, the stable request prefix and the
        // admission evidence, none of which can be re-signed on reopen; and the
        // prefix has to satisfy the persisted projection and the live adapter
        // at once, so there is no figure but the created one that works. A
        // session created before this route derived anything spelled the same
        // figure `Enforced`, which digests differently, so the spelling is
        // carried too.
        let created = NonZeroU32::new(4_096).expect("created bound");
        assert_ne!(created, bound, "the route has moved its figure");
        for disposition in [
            OutputBoundDisposition::Enforced(created),
            OutputBoundDisposition::DerivedFromRoute(created),
        ] {
            let reopened = openrouter_behavior(
                ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
                CompositionSelection::FollowCatalog,
                wire,
                &registry,
                &[],
                disposition,
            )
            .unwrap();
            assert_eq!(reopened.output_bound, disposition);
            assert_eq!(
                crate::api_runtime::behavior_generation_limits(&reopened)
                    .unwrap()
                    .max_output_tokens(),
                Some(created),
                "the wire carries the created figure, not today's"
            );
        }
        // This is the figure the agent will compare against, and it is the
        // route's, not the product constant that used to be read here.
        let limits = crate::api_runtime::behavior_generation_limits(&behavior).unwrap();
        assert_eq!(limits.max_output_tokens(), Some(bound));
        assert_ne!(bound.get(), NATIVE_RUN_MAX_OUTPUT_TOKENS);
    }

    /// The DeepSeek V4 Flash shape from the Harbor bakeoff: a large window and
    /// a large attested completion budget. It still admits, and it no longer
    /// gets 4,096.
    #[tokio::test]
    async fn the_deepseek_route_shape_still_admits_with_a_workable_budget() {
        let wire = ResolvedProtocol::Chat;
        let evidence = discover_admission_evidence(
            &[(
                AdmissionProbe {
                    has_reasoning_param: true,
                    tool_choice_auto: true,
                    max_completion_tokens: Some(65_536),
                    max_prompt_tokens: Some(131_072),
                },
                "provider/deepseek",
            )],
            163_840,
            false,
        )
        .await;
        assert!(openrouter_endpoint_admission(&evidence, wire, true, false).is_ok());
        let bound = openrouter_output_bound(&evidence, wire, true).unwrap();
        assert_eq!(bound.get(), 32_768);
        let input = openrouter_maximum_projected_input(&evidence, wire, true, bound).unwrap();
        assert_eq!(input, 65_536);
        assert!(input + u64::from(bound.get()) <= evidence.context_length());
    }

    // An OpenRouter (pi pack) hub through the real projection admission and
    // AgentSessions, offline. The loopback route replaces live discovery.
    struct PiHubOpenRouterCredentials;
    impl OpenRouterCredentials for PiHubOpenRouterCredentials {
        fn preflight(&self) -> CliResult<()> {
            Ok(())
        }
        fn source(&self) -> CliResult<Arc<dyn OpenRouterApiKeySource>> {
            use plakous::openrouter::{OpenRouterApiKey, StaticOpenRouterApiKeySource};
            Ok(Arc::new(
                StaticOpenRouterApiKeySource::new(
                    OpenRouterApiKey::new("test-catalog-key").expect("api key"),
                    "test-account",
                    "test-authority",
                )
                .expect("source"),
            ))
        }
        fn key_label(&self) -> CliResult<String> {
            Ok("PROBE".to_owned())
        }
    }

    struct PiHubWorkers(PathBuf);
    impl plakous::tools::SidecarLocator for PiHubWorkers {
        fn javascript_host(&self) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }
        fn python_host(&self) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }
    }

    fn pi_hub_extensions(
        directory: &Path,
        vocabulary: crate::agent_tree_tools::TopologyVocabulary,
    ) -> crate::tools::ProductToolExtensions {
        let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            "tree-pi-hub",
            "root",
            "owner",
            directory.to_path_buf(),
            "vendor/admission-test",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .expect("tree");
        crate::topology_extensions::topology_tool_extensions_in(
            Arc::new(Mutex::new(tree)),
            "root",
            vocabulary,
        )
        .expect("extensions")
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum CatalogDrift {
        Equal,
        Changed,
        ToolOmitting,
    }

    async fn pi_hub_local_route_evidence(
        probes: &[(AdmissionProbe, &str)],
        context_length: u64,
        default_enabled_reasoning: bool,
        drift: Option<CatalogDrift>,
    ) -> (
        plakous_openrouter::OpenRouterChatRoute,
        plakous_http::Client,
        OpenRouterChatModelEvidence,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        use std::io;
        use std::sync::Arc;

        use plakous::openrouter::{OpenRouterApiKey, StaticOpenRouterApiKeySource};
        use plakous_http::{Client, Limits, TlsConfigIdentity};
        use plakous_openrouter::{
            OpenRouterChatDiscoveryConfig, OpenRouterChatRoute, OpenRouterOrigin,
            OpenRouterRetentionPolicy,
        };
        use rcgen::{CertifiedKey, generate_simple_self_signed};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::{ClientConfig, RootCertStore, ServerConfig};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;

        const MODEL: &str = "vendor/admission-test";

        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("cert");
        let certificate = cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("protocols")
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .expect("server cert");
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate.to_vec()))
            .expect("root cert");
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocols")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = Client::with_tls_config_and_identity(
            Limits::default(),
            client_config,
            TlsConfigIdentity::from_parts(
                1,
                &[b"plak-host-openrouter-admission-v1", certificate.as_ref()],
            )
            .expect("tls identity"),
        )
        .expect("client");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let origin = format!("https://{}", listener.local_addr().expect("addr"));
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let reasoning = if default_enabled_reasoning {
            serde_json::json!({
                "supported_efforts": ["medium"],
                "default_enabled": true,
                "supports_max_tokens": false,
                "mandatory": false
            })
        } else {
            serde_json::Value::Null
        };
        let models = serde_json::json!({
            "data": [{
                "id": MODEL,
                "canonical_slug": MODEL,
                "context_length": context_length,
                "supported_parameters": ["tools", "tool_choice"],
                "reasoning": reasoning,
            }],
            "links": {},
            "total_count": 1
        })
        .to_string();
        let endpoints = serde_json::json!({
            "data": {
                "id": MODEL,
                "endpoints": probes
                    .iter()
                    .map(|(probe, tag)| endpoint_json(*probe, tag))
                    .collect::<Vec<_>>(),
            }
        })
        .to_string();
        let mut replies = vec![
            ("application/json", models.clone()),
            ("application/json", endpoints.clone()),
        ];
        if let Some(changed_fact) = drift {
            let mut changed: serde_json::Value =
                serde_json::from_str(&endpoints).expect("endpoints");
            let mut added = changed["data"]["endpoints"][0].clone();
            added["tag"] = serde_json::json!("provider/added");
            added["provider_name"] = serde_json::json!("Added");
            if changed_fact == CatalogDrift::Changed {
                // In the same output rung and below the same input ceiling:
                // the minimum attested guarantee itself remains authority.
                added["max_completion_tokens"] = serde_json::json!(40_000);
            }
            if changed_fact == CatalogDrift::ToolOmitting {
                added["supported_parameters"] = serde_json::json!(["max_tokens"]);
                added["max_completion_tokens"] = serde_json::json!(4_096);
            }
            changed["data"]["endpoints"]
                .as_array_mut()
                .expect("rows")
                .push(added);
            replies.extend([
                ("application/json", models),
                ("application/json", changed.to_string()),
            ]);
            if changed_fact == CatalogDrift::Equal {
                replies.push(("text/event-stream", concat!(
                    "data: {\"id\":\"drift-resume\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"vendor/admission-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Resumed exactly.\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"drift-resume\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"vendor/admission-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"id\":\"drift-resume\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"vendor/admission-test\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n",
                    "data: [DONE]\n\n"
                ).to_owned()));
            }
        }
        let serve = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (content_type, body) in replies {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(head) = request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let text = String::from_utf8_lossy(&request[..head]);
                        let length = text
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if request.len() >= head + 4 + length {
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nx-request-id: drift-resume\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
                requests.push(String::from_utf8(request).map_err(io::Error::other)?);
            }
            Ok::<_, io::Error>(requests)
        });
        let source = Arc::new(
            StaticOpenRouterApiKeySource::new(
                OpenRouterApiKey::new("test-catalog-key").expect("api key"),
                "test-account",
                "test-authority",
            )
            .expect("source"),
        );
        let route = OpenRouterChatRoute::new_local_fixture_with_retention(
            source,
            OpenRouterOrigin::new(origin).expect("origin"),
            OpenRouterRetentionPolicy::strict(),
        )
        .expect("route")
        .with_clock(chat_review_clock());
        let evidence = route
            .discover_chat_model(&client, MODEL, OpenRouterChatDiscoveryConfig::default())
            .await
            .expect("discovery");
        let serve = tokio::spawn(async move { serve.await.expect("serve").expect("serve io") });
        (route, client, evidence, serve)
    }

    fn pi_hub_context_contract(
        route: &plakous_openrouter::OpenRouterChatRoute,
        reported_context_window: u64,
        maximum_projected_input: u64,
    ) -> plakous_model::ContextContract {
        use plakous_model::{
            CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceSource,
            CountCertificateKind, Digest, DigestDomain, EvidenceFreshness, MeasurementCertificate,
            ProtocolId, RuntimeModelAuthority, Timestamp,
        };
        let identity = route.identity();
        let authority = RuntimeModelAuthority::authority_class_for(
            identity.account_binding(),
            identity.credential_authority_binding(),
        )
        .expect("authority");
        let declaration = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[
                b"plakous-facade/openrouter-custom-context-v2",
                identity.fingerprint().as_bytes(),
                b"chat-completions-family",
                &reported_context_window.to_be_bytes(),
                &maximum_projected_input.to_be_bytes(),
            ],
        )
        .expect("declaration");
        let freshness = EvidenceFreshness::new(Timestamp::new(1, 0).expect("ts"), None).expect("f");
        let revision = "plakous-facade-openrouter-custom-context-v2";
        let capacity = CapacityCertificate::custom_envelope(
            "plakous-facade-openrouter-custom-capacity",
            revision,
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(reported_context_window),
                maximum_projected_input,
                auto_compact_input: None,
            },
            authority,
            freshness,
            declaration,
            revision,
        )
        .expect("capacity");
        let measurement = MeasurementCertificate::new(
            "plakous-facade-openrouter-chat-wire-bound",
            revision,
            "complete-lowered-chat-body-utf8-byte-upper-bound",
            revision,
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(authority),
            freshness,
        )
        .expect("measurement");
        ContextContract::new(
            ProtocolId::new("chat-completions-family").expect("protocol"),
            "openrouter-chat-custom-accounting-v1",
            "openrouter-chat-canonical-v1",
            "openrouter-chat-output-reservation-v1",
            capacity,
            vec![measurement],
        )
        .expect("contract")
    }

    /// The pi hub admission path, offline: the product's own registry, behavior,
    /// runtime binding and `ProductProjectionAdmission` over a loopback-route
    /// OpenRouter Chat model.
    async fn pi_hub_local(
        composition: CompositionSelection,
        spawned_child: bool,
        create_vocabulary: crate::agent_tree_tools::TopologyVocabulary,
        reopen_vocabulary: crate::agent_tree_tools::TopologyVocabulary,
        fence: crate::agent::SessionToolFence,
        drift: Option<CatalogDrift>,
    ) -> Result<(), String> {
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
            OpenSessionRequest, SessionBackend, SessionOperationId,
        };
        let wire = ResolvedProtocol::Chat;
        let (route, client, evidence, serve) = pi_hub_local_route_evidence(
            &[(
                AdmissionProbe {
                    has_reasoning_param: false,
                    tool_choice_auto: true,
                    max_completion_tokens: Some(65_536),
                    max_prompt_tokens: Some(131_072),
                },
                "provider/pi-hub",
            )],
            163_840,
            false,
            drift,
        )
        .await;
        let directory = tempfile::tempdir().expect("tempdir");
        let worker = directory.path().join("uninvoked-worker");
        std::fs::write(&worker, b"offline artifact identity fixture").expect("worker");
        let spec = |opening: bool, vocabulary| ProductOpenRouterSpec {
            job_config: plakous::tools::JobConfig::default(),
            created_output_bound: None,
            exchange_observer: None,
            credentials: Arc::new(PiHubOpenRouterCredentials),
            sidecars: Arc::new(PiHubWorkers(worker.clone())),
            product_home: None,
            literal_model: "vendor/admission-test".to_owned(),
            effort: None,
            composition,
            wire,
            workspace_root: directory.path().to_path_buf(),
            cwd: directory.path().to_path_buf(),
            plan_state_authority: "fixture-pi-hub".to_owned(),
            opening_durable_session: opening,
            occupancy_tier: crate::agent_tree_protocol::Tier::Mutate,
            scheduled_compaction: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_extensions: pi_hub_extensions(directory.path(), vocabulary),
            cached_evidence: None,
            telemetry: None,
            spawned_child,
        };
        let binding = |spec: &ProductOpenRouterSpec,
                       evidence: &OpenRouterChatModelEvidence|
         -> Result<SessionRuntimeBinding, String> {
            let registry = crate::agent::coding_tool_registry_with_extensions(
                &spec.plan_state_authority,
                spec.composition,
                false,
                &spec.topology_extensions,
            )
            .map_err(|e| format!("registry: {}", e.message()))?;
            let extra_names: Vec<String> = spec
                .topology_extensions
                .names()
                .map(|name| name.as_str().to_owned())
                .collect();
            let extra_refs: Vec<&str> = extra_names.iter().map(String::as_str).collect();
            let bound = openrouter_output_bound(evidence, wire, false)
                .map_err(|e| format!("bound: {}", e.message()))?;
            let behavior = openrouter_behavior(
                reasoning_disposition(wire, None, evidence)
                    .map_err(|e| format!("reasoning: {}", e.message()))?,
                spec.composition,
                wire,
                &registry,
                &extra_refs,
                OutputBoundDisposition::DerivedFromRoute(bound),
            )
            .map_err(|e| format!("behavior: {}", e.message()))?;
            let max_input = openrouter_maximum_projected_input(evidence, wire, false, bound)
                .map_err(|e| format!("input: {}", e.message()))?;
            let contract = pi_hub_context_contract(&route, evidence.context_length(), max_input);
            let configured =
                plakous_openrouter::OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
                    route.clone(),
                    evidence.clone(),
                    contract,
                    behavior,
                    client.pooled(),
                )
                .map_err(|e| format!("configured: {e}"))?;
            openrouter_runtime_binding(spec, configured, registry, fence, chat_review_clock())
                .map_err(|e| format!("binding: {}", e.message()))
        };
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).expect("backend"),
        )
        .expect("sessions");
        let created = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("fixture:pi:create").expect("id"),
                binding(&spec(false, create_vocabulary), &evidence)?,
            ))
            .await
            .map_err(|error| format!("create [{:?}]: {error}", error.kind()))?;
        let CreateSessionOutcome::Created { session, .. } = created else {
            return Err("create: not created".into());
        };
        let locator = session.release().await.expect("release");
        let reopened_evidence = if drift.is_some() {
            route
                .discover_chat_model(
                    &client,
                    "vendor/admission-test",
                    plakous_openrouter::OpenRouterChatDiscoveryConfig::default(),
                )
                .await
                .expect("rediscovery")
        } else {
            evidence
        };
        let result = match sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("fixture:pi:reopen").expect("id"),
                locator,
                binding(&spec(true, reopen_vocabulary), &reopened_evidence)?,
            ))
            .await
        {
            Ok(OpenSessionOutcome::Opened { session, .. }) => {
                if drift == Some(CatalogDrift::Equal) {
                    let turn = session
                        .agent()
                        .prompt("Resume on the exact admitted route.")
                        .await
                        .expect("admit");
                    let terminal = turn.result().await.expect("terminal");
                    if let plakous::agent::TurnResultOutcome::Failed(error) = terminal.outcome() {
                        panic!("resume failed: {}", error.message());
                    }
                    assert!(
                        matches!(
                            terminal.outcome(),
                            plakous::agent::TurnResultOutcome::Completed(_)
                        ),
                        "{terminal:?}"
                    );
                }
                session.release().await.expect("release");
                Ok(())
            }
            Ok(_) => Err("reopen: not opened".into()),
            Err(error) => Err(format!("reopen [{:?}]: {error}", error.kind())),
        };
        if drift == Some(CatalogDrift::Equal) && result.is_err() {
            serve.abort();
            let _ = serve.await;
            return result;
        }
        let requests = serve.await.expect("served requests");
        assert_eq!(
            requests.len(),
            match drift {
                None => 2,
                Some(CatalogDrift::Changed | CatalogDrift::ToolOmitting) => 4,
                Some(CatalogDrift::Equal) => 5,
            }
        );
        for (index, request) in requests.iter().enumerate() {
            let path = if index == 4 {
                "POST /api/v1/chat/completions "
            } else if index % 2 == 0 {
                "GET /api/v1/models "
            } else {
                "GET /api/v1/models/vendor/admission-test/endpoints "
            };
            assert!(request.starts_with(path), "exact path {index}");
            assert!(request.lines().any(|line| line.eq_ignore_ascii_case("authorization: Bearer test-catalog-key")));
        }
        result
    }

    /// An OpenRouter hub with the WaitStop vocabulary creates and reopens
    /// through projection admission and `AgentSessions`, offline. Every
    /// composition meets every occupancy fence, and the root/spawned-child axis
    /// alternates so each of its values meets every composition and fence.
    #[tokio::test]
    async fn pi_hub_local_route_creates_and_reopens() {
        use crate::agent::SessionToolFence;
        use crate::agent_tree_tools::TopologyVocabulary;
        let mut failures = Vec::new();
        let compositions = [
            CompositionSelection::Direct,
            CompositionSelection::JsPtc,
            CompositionSelection::PyPtc,
        ];
        let fences = [
            SessionToolFence::Full,
            SessionToolFence::OccupancyExecute,
            SessionToolFence::OccupancyReadOnly,
        ];
        for (composition_index, composition) in compositions.into_iter().enumerate() {
            for (fence_index, fence) in fences.into_iter().enumerate() {
                let spawned_child = (composition_index + fence_index) % 2 == 1;
                if let Err(error) = pi_hub_local(
                    composition,
                    spawned_child,
                    TopologyVocabulary::WaitStop,
                    TopologyVocabulary::WaitStop,
                    fence,
                    None,
                )
                .await
                {
                    failures.push(format!(
                        "{composition:?} child={spawned_child} {fence:?}: {error}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[tokio::test]
    async fn catalog_drift_reopens_persisted_session_and_refuses_changed_admission() {
        use crate::agent::SessionToolFence;
        use crate::agent_tree_tools::TopologyVocabulary;
        for changed_fact in [false, true] {
            let result = pi_hub_local(
                CompositionSelection::Direct,
                false,
                TopologyVocabulary::WaitStop,
                TopologyVocabulary::WaitStop,
                SessionToolFence::Full,
                Some(if changed_fact {
                    CatalogDrift::Changed
                } else {
                    CatalogDrift::Equal
                }),
            )
            .await;
            if changed_fact {
                assert!(
                    result.is_err_and(|error| error.starts_with("reopen [Revalidation]")),
                    "admission drift refuses before inference"
                );
            } else {
                result.expect("equal admission reopens and resumes");
            }
        }
    }

    #[tokio::test]
    async fn chat_tool_omitting_catalog_drift_refuses_persisted_resume_before_inference() {
        use crate::agent::SessionToolFence;
        use crate::agent_tree_tools::TopologyVocabulary;
        let result = pi_hub_local(
            CompositionSelection::Direct,
            false,
            TopologyVocabulary::WaitStop,
            TopologyVocabulary::WaitStop,
            SessionToolFence::Full,
            Some(CatalogDrift::ToolOmitting),
        )
        .await;
        assert!(
            result.is_err_and(|error| error.starts_with("reopen [Revalidation]")),
            "tool-omitting admission drift refuses before inference"
        );
    }

    /// A legacy pi hub (Plak `wait_agent` / `interrupt_agent`) does not reopen
    /// on the live WaitStop host, on every composition. The live host no longer
    /// binds those executors: `ExternalBindingUnavailable`, surfaced as
    /// [`plakous::sessions::AgentSessionsErrorKind::Internal`].
    #[tokio::test]
    async fn pre_w3_b_pi_hub_local_route_is_refused() {
        use crate::agent::SessionToolFence;
        use crate::agent_tree_tools::TopologyVocabulary;
        let mut failures = Vec::new();
        for composition in [
            CompositionSelection::Direct,
            CompositionSelection::JsPtc,
            CompositionSelection::PyPtc,
        ] {
            let outcome = pi_hub_local(
                composition,
                false,
                TopologyVocabulary::Plak,
                TopologyVocabulary::WaitStop,
                SessionToolFence::Full,
                None,
            )
            .await;
            let Err(error) = outcome else {
                failures.push(format!("{composition:?}: reopened"));
                continue;
            };
            if !error.starts_with("reopen [Internal]")
                || !error
                    .contains("a required tool executor binding is unavailable: plak.topology.")
            {
                failures.push(format!("{composition:?}: {error}"));
            }
        }
        assert!(
            failures.is_empty(),
            "legacy pi hub reopen must be ExternalBindingUnavailable surfaced as Internal: {failures:#?}"
        );
    }
}
