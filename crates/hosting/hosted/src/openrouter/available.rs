//! Plak-owned OpenRouter Chat construction through the supported facade.
use super::{
    NonZeroU32, OpenRouterKeyUsageProbe, OutputBoundRederived, ProductOpenRouterRuntime,
    ProductOpenRouterSpec,
};

use std::collections::BTreeSet;
use std::future::Future;

#[cfg(test)]
use std::path::Path;

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
    OpenRouterChatClient, OpenRouterChatDiscoveryError, OpenRouterChatEndpointCandidate,
    OpenRouterKeySource, OpenRouterReasoningFacts, OpenRouterRetentionPolicy,
    OpenRouterToolChoiceModes,
};
pub use plakous::openrouter::{OpenRouterChatModelEvidence, OpenRouterKeyUsage};
use plakous::sessions::SessionRuntimeBinding;
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
struct PreparedOpenRouter {
    configured: ConfiguredLanguageModel,
    registry: plakous::tools::FrozenToolRegistry,
    evidence: OpenRouterChatModelEvidence,
    /// Hosted web search on the same route, key and model, whatever the wire.
    retrieval: plakous::openrouter::OpenRouterRetrieval,
    output_bound_rederived: Option<OutputBoundRederived>,
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
    /// Prepares one read's source and optional best-effort observation scope.
    ///
    /// # Errors
    /// The selected source or its safe label cannot be read.
    fn usage_read(&self, account: &crate::account::AccountRef) -> CliResult<OpenRouterUsageRead> {
        Ok(OpenRouterUsageRead {
            source: self.source(account)?,
            label: self.key_label(account)?,
            observer: None,
        })
    }
    /// Fail fast with a construction diagnostic before any client is built.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when no credential is available.
    fn preflight(&self, _account: &crate::account::AccountRef) -> CliResult<()>;

    /// The source the client re-resolves through at every attempt fence.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the credential binding is invalid.
    fn source(&self, _account: &crate::account::AccountRef) -> CliResult<Arc<OpenRouterKeySource>>;

    /// The selected key's safe account label: its environment variable name
    /// or `sha256:` plus eight hex digits, never a key prefix or suffix.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the selected key cannot be read.
    fn key_label(&self, _account: &crate::account::AccountRef) -> CliResult<String>;
}

/// One physical allowance read, including an optional observation lifetime.
pub struct OpenRouterUsageRead {
    /// Exact source, revalidated by the provider client at dispatch.
    pub source: Arc<OpenRouterKeySource>,
    /// Credential-free product label.
    pub label: String,
    /// Dropped on cancellation, including when no response was available.
    pub observer: Option<Box<dyn super::KeyUsageObserver>>,
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

#[allow(
    clippy::too_many_lines,
    reason = "OpenRouter preparation keeps catalog admission, credentials, and the exact route policy in one failure boundary."
)]
async fn prepare_openrouter(spec: &ProductOpenRouterSpec) -> CliResult<PreparedOpenRouter> {
    if spec.account.vertical() != crate::account::Vertical::OpenRouter {
        return Err(
            crate::capabilities::AccountAdmissionError::AccountTargetMismatch {
                account: spec.account.to_string(),
            }
            .into(),
        );
    }
    if spec.composition == CompositionSelection::NativePtc {
        return Err(CliFailure::new(
            crate::binding::OPENROUTER_NATIVE_PTC_REFUSAL,
        ));
    }
    // Resolve once for an immediate, safe construction diagnostic. The source
    // itself never retains this secret and re-resolves at every attempt fence.
    spec.credentials.preflight(&spec.account)?;
    let client = OpenRouterChatClient::connect_with_retention(
        spec.credentials.source(&spec.account)?,
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
                "model `{}` is not in the OpenRouter catalog of account `openrouter`; inspect its available models",
                spec.literal_model
            ),
        ).with_advice(plakous_host_protocol::refusal::RefusalAdvice::UnknownModel { model: spec.literal_model.clone(), account: spec.account.to_string(), catalog: "OpenRouter".into() }),
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

    let registry = crate::agent::build::coding_tool_registry_with_extensions(
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
    let behavior = match spec.topology_extensions.frozen_presentation() {
        Some(frozen) => openrouter_behavior_from_tools(
            reasoning_effort,
            spec.wire,
            frozen.selected_tools().to_vec(),
            output_bound_disposition,
        )?,
        None => openrouter_behavior(
            reasoning_effort,
            spec.composition,
            spec.wire,
            &registry,
            &extra_refs,
            output_bound_disposition,
        )?,
    };
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
    let tools = crate::agent::build::api::configured_tools(
        CodingPresentation::Pi,
        composition,
        wire,
        registry,
        extra_names,
    )?;
    openrouter_behavior_from_tools(reasoning_effort, wire, tools, output_bound)
}

fn openrouter_behavior_from_tools(
    reasoning_effort: ReasoningEffortDisposition,
    wire: ResolvedProtocol,
    tools: Vec<plakous::model::ConfiguredTool>,
    output_bound: OutputBoundDisposition,
) -> CliResult<ProviderBehaviorConfiguration> {
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
    crate::agent::build::api::runtime_binding(
        &crate::agent::build::api::ApiRuntimeSpec {
            runner_services: spec.runner_services.clone(),
            job_config: spec.job_config,
            account: spec.account.clone(),
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

#[allow(
    clippy::too_many_arguments,
    reason = "The OpenRouter profile fixture names the exact composition, wire, model, tool surface, and workspace used by production compilation."
)]
#[cfg(test)]
pub(crate) fn openrouter_product_profile(
    composition: CompositionSelection,
    effort: Option<ReasoningEffort>,
    wire: ResolvedProtocol,
    literal_model: &str,
    subagents: bool,
    workspace: plakous::prompts::identity::PromptWorkspace<'_>,
    surface: &CompiledSurface,
) -> CliResult<plakous::agent::HarnessProfileSpec> {
    crate::agent::build::api::api_product_profile(
        &"openrouter".parse()?,
        composition,
        effort,
        wire,
        literal_model,
        subagents,
        workspace,
        surface,
    )
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
    account: &crate::account::AccountRef,
) -> OpenRouterKeyUsageProbe {
    let prepared = match prepare_key_usage(credentials, account) {
        Ok(prepared) => prepared,
        Err(reason) => return reason,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return OpenRouterKeyUsageProbe::Failed(error.to_string()),
    };
    runtime.block_on(read_key_usage(prepared))
}

/// One cancellable allowance GET, without inference, login or refresh. The
/// caller owns its deadline; the legacy synchronous probe keeps its policy.
pub async fn read_openrouter_key_usage(
    credentials: &dyn OpenRouterCredentials,
    account: &crate::account::AccountRef,
) -> OpenRouterKeyUsageProbe {
    match prepare_key_usage(credentials, account) {
        Ok(prepared) => read_key_usage(prepared).await,
        Err(reason) => reason,
    }
}

fn prepare_key_usage(
    credentials: &dyn OpenRouterCredentials,
    account: &crate::account::AccountRef,
) -> Result<OpenRouterUsageRead, OpenRouterKeyUsageProbe> {
    // "Is a key configured at all" is the credential's own preflight; an
    // unconfigured product is `NotConfigured`, not a failure.
    if credentials.preflight(account).is_err() {
        return Err(OpenRouterKeyUsageProbe::NotConfigured);
    }
    credentials
        .usage_read(account)
        .map_err(|error| OpenRouterKeyUsageProbe::Failed(error.message().to_owned()))
}

async fn read_key_usage(prepared: OpenRouterUsageRead) -> OpenRouterKeyUsageProbe {
    let observation = fetch_key_usage(prepared.source, prepared.label).await;
    if let Some(observer) = prepared.observer {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.completed(&observation);
        }));
    }
    observation
}

async fn fetch_key_usage(
    source: Arc<OpenRouterKeySource>,
    label: String,
) -> OpenRouterKeyUsageProbe {
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
        Err(error) => {
            let reason = key_usage_failure(&error, &label);
            if matches!(
                error,
                plakous::openrouter::OpenRouterKeyUsageError::Unauthorized
                    | plakous::openrouter::OpenRouterKeyUsageError::Forbidden
            ) {
                OpenRouterKeyUsageProbe::Rejected {
                    reason,
                    source: if label == "OPENROUTER_API_KEY" {
                        plakous_host_protocol::views::KeySource::Environment
                    } else {
                        plakous_host_protocol::views::KeySource::File
                    },
                }
            } else {
                OpenRouterKeyUsageProbe::Failed(reason)
            }
        }
    }
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
        "replace the rejected API key from the key file"
    };
    format!("OpenRouter rejected the API key from {label} ({status}); {remedy}")
}

pub(crate) fn carry_input_window<P: plakous_host_protocol::ProtocolExtension>(
    config: &crate::runtime::HostConfig<P>,
) -> CliResult<plakous::sessions::SessionInputWindow> {
    let credentials = config.state_roots.openrouter_credentials();
    let account = config.session.account_ref()?;
    credentials.preflight(&account)?;
    let client = OpenRouterChatClient::connect_with_retention(
        credentials.source(&account)?,
        OpenRouterRetentionPolicy::gateway_default(),
    )
    .and_then(OpenRouterChatClient::with_plak_attribution)
    .map_err(|e| CliFailure::new(e.to_string()))?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let evidence = executor
        .block_on(client.discover_exact(&config.model_id))
        .map_err(|e| CliFailure::new(e.to_string()))?
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let wire = config
        .session
        .target_selection
        .api_target(&config.session.account_ref()?)?
        .ok_or_else(|| CliFailure::new("missing API target"))?
        .protocol();
    let reasoning = config.effort.is_some();
    let output = openrouter_output_bound(&evidence, wire, reasoning)?;
    let admitted = openrouter_maximum_projected_input(&evidence, wire, reasoning, output)?;
    Ok(plakous::sessions::SessionInputWindow {
        route: format!("openrouter/{wire:?}"),
        total_window: Some(evidence.context_length()),
        admitted_input: Some(admitted),
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
#[cfg(test)]
pub(crate) use tests::binding_matrix::binding_matrix_facts;
