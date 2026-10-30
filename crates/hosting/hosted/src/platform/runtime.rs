//! Direct API-key model construction through the public facade.

use std::num::NonZeroU64;
use std::sync::Arc;
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

use plakous::model::{
    ConfiguredLanguageModel, ModelAdapterProfileId, ProviderProfileRevisionId,
    ProviderStorageDisposition, ReplayDisposition, RetentionDisposition, ToolName,
};

#[cfg(feature = "anthropic")]
#[path = "anthropic.rs"]
pub(super) mod anthropic;
#[cfg(not(feature = "anthropic"))]
#[path = "anthropic_unavailable.rs"]
mod anthropic;
#[path = "listing.rs"]
pub(super) mod listing;

pub(crate) fn openai_profile(model: &str) -> Option<&'static str> {
    openai::profile(model)
}
#[cfg(feature = "openai")]
#[path = "openai.rs"]
pub(super) mod openai;
#[cfg(not(feature = "openai"))]
#[path = "openai_unavailable.rs"]
mod openai;
#[cfg(feature = "xai-api")]
#[path = "xai.rs"]
pub(super) mod xai;
#[cfg(not(feature = "xai-api"))]
#[path = "xai_unavailable.rs"]
mod xai;

pub use listing::api_model_catalog;

use super::{
    ApiCatalogSource, ApiModelCatalog, ApiModelLists, ApiToolMode, PlatformAdmission,
    PlatformModel, api_composition, bundled_catalog, catalog,
};
use crate::agent::build::api::ApiRuntimeSpec;
#[cfg(any(feature = "openai", feature = "xai-api"))]
mod retention;
#[cfg(not(all(feature = "openai", feature = "anthropic", feature = "xai-api")))]
#[path = "unavailable.rs"]
mod unavailable;
use crate::binding::{CompositionSelection, PlatformProvider};
use crate::error::{CliFailure, CliResult};

pub(super) use anthropic::AdmittedModel;
pub use anthropic::Credentials as AnthropicCredentials;
pub use openai::Credentials as OpenAiCredentials;
pub use xai::Credentials as XaiCredentials;
/// Product-owned sources; the host never reads a product secret file.
pub trait PlatformCredentials: OpenAiCredentials + AnthropicCredentials + XaiCredentials {}
impl<T: OpenAiCredentials + AnthropicCredentials + XaiCredentials> PlatformCredentials for T {}

struct Construction<'a, 's> {
    provider: PlatformProvider,
    credentials: &'a dyn PlatformCredentials,
    spec: &'a ApiRuntimeSpec<'s>,
    row: &'a PlatformModel,
    behavior: plakous::model::ProviderBehaviorConfiguration,
    maximum: NonZeroU64,
    window: u64,
    observer: crate::exchange_observation::Observer,
}

#[cfg(test)]
pub(crate) async fn construct_platform_runtime(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    spec: &ApiRuntimeSpec<'_>,
    observer: crate::exchange_observation::Observer,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    construct_platform_runtime_with(
        provider,
        credentials,
        spec,
        observer,
        &PlatformAdmission::default(),
        tests::review_clock_for(provider),
    )
    .await
}

pub(crate) async fn build_runtime(
    provider: PlatformProvider,
    parent: &crate::agent::ApiTopologyParent,
    opening_durable_session: bool,
    tool_fence: crate::agent::SessionToolFence,
    topology_extensions: &crate::tools::ProductToolExtensions,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    let telemetry = parent.state_roots.telemetry_emitter();
    let admission = parent.state_roots.platform_admission();
    let runtime = Box::pin(construct_platform_runtime_with(
        provider,
        parent.state_roots.platform_credentials(),
        &ApiRuntimeSpec {
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
            topology_extensions,
            telemetry: telemetry.as_ref(),
            spawned_child: parent.topology_caller_id.is_some(),
        },
        parent.state_roots.exchange_observer(),
        &admission,
        Arc::new(plakous::model::SystemClock),
    ))
    .await?;
    Ok(runtime)
}

pub(crate) async fn construct_platform_runtime_with(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    spec: &ApiRuntimeSpec<'_>,
    observer: crate::exchange_observation::Observer,
    admission: &PlatformAdmission,
    clock: Arc<dyn plakous::model::Clock>,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    crate::capabilities::require(provider.account())?;
    if provider.account() != spec.account || provider.protocol() != spec.wire {
        return Err(CliFailure::new(
            "API target/account/protocol binding is inconsistent",
        ));
    }
    let composition =
        api_composition(provider, spec.literal_model, spec.composition).map_err(CliFailure::new)?;
    let spec = &ApiRuntimeSpec {
        composition,
        ..*spec
    };
    let native_ptc = composition == CompositionSelection::NativePtc;
    let registry = crate::agent::build::coding_tool_registry_with_extensions(
        spec.plan_state_authority,
        composition,
        native_ptc,
        spec.topology_extensions,
    )?;
    let effort = spec
        .effort
        .ok_or_else(|| CliFailure::new("API model requires an explicit reasoning effort"))?;
    let extra: Vec<_> = spec
        .topology_extensions
        .names()
        .map(ToolName::as_str)
        .collect();
    let catalog = listing::api_model_catalog_observed(
        provider,
        credentials,
        spec.product_home,
        admission.lists,
        unix_now(),
        observer.clone(),
    )
    .await;
    let row = catalog
        .admit(spec.literal_model, Some(effort))
        .map_err(CliFailure::from)?
        .clone();
    // An exact reopen keeps the window number the session was created with;
    // a new session takes the configured selection.
    let window = match &admission.created_evidence {
        Some(stored) => row.reopen_window(provider, stored)?,
        None => row.window(admission.window),
    };
    let behavior = platform_behavior(
        provider,
        effort,
        &row,
        native_ptc,
        crate::agent::build::api::configured_tools(
            crate::coding_routes::platform_presentation(provider),
            composition,
            spec.wire,
            &registry,
            &extra,
        )?,
    )?;
    let maximum = NonZeroU64::new(window)
        .ok_or_else(|| CliFailure::new("API context envelope is invalid"))?;
    let input = Construction {
        provider,
        credentials,
        spec,
        row: &row,
        behavior,
        maximum,
        window,
        observer,
    };
    let configured = match provider {
        PlatformProvider::OpenAi => openai::construct(input, Arc::clone(&clock)).await?,
        PlatformProvider::Anthropic => anthropic::construct(input).await?,
        PlatformProvider::Xai => xai::construct(input, Arc::clone(&clock)).await?,
    };
    crate::agent::build::api::runtime_binding(spec, configured, registry, clock)
}

fn api_error(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("API model construction failed: {error}"))
}

fn platform_behavior(
    provider: PlatformProvider,
    effort: plakous::agent::ReasoningEffort,
    row: &PlatformModel,
    native_ptc: bool,
    tools: Vec<plakous::model::ConfiguredTool>,
) -> CliResult<plakous::model::ProviderBehaviorConfiguration> {
    use plakous::model::{
        CachePolicy, ContextAdmissionRequirement, LocalDurabilityDisposition,
        OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProcessingTierDisposition,
        ProviderBehaviorConfiguration, ReasoningEffortDisposition, ReasoningIncludeDisposition,
        ReasoningSummaryDisposition, ResponseStateDisposition, ToolChoice, ToolPresentationMode,
    };
    Ok(ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        // Codex Code Mode is a free-form outer composition surface, as on
        // the Codex OAuth route.
        tool_mode: if native_ptc {
            ToolPresentationMode::LocalCompositionOnly
        } else {
            ToolPresentationMode::Direct
        },
        reasoning_effort: ReasoningEffortDisposition::Enabled(effort),
        reasoning_summary: match provider {
            PlatformProvider::OpenAi => ReasoningSummaryDisposition::Auto,
            PlatformProvider::Anthropic | PlatformProvider::Xai => {
                ReasoningSummaryDisposition::Omit
            }
        },
        reasoning_include: match provider {
            PlatformProvider::Anthropic => ReasoningIncludeDisposition::Omit,
            PlatformProvider::OpenAi | PlatformProvider::Xai => {
                ReasoningIncludeDisposition::EncryptedContent
            }
        },
        replay: ReplayDisposition::StrictEncrypted,
        cache: CachePolicy::Disabled,
        tools,
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            std::num::NonZeroU32::new(row.output_bound())
                .ok_or_else(|| CliFailure::new("API output bound is invalid"))?,
        ),
        storage: match provider {
            PlatformProvider::Anthropic => ProviderStorageDisposition::OmittedRouteContract,
            PlatformProvider::OpenAi | PlatformProvider::Xai => {
                ProviderStorageDisposition::DisabledOnWire
            }
        },
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new(format!("{}.platform.api-key", provider.account()))
                .map_err(api_error)?,
            revision: ProviderProfileRevisionId::new("platform-api-key-retention-v1")
                .map_err(api_error)?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::DurableRequired,
    })
}

#[cfg(all(test, feature = "openai", feature = "anthropic", feature = "xai-api"))]
#[path = "tests.rs"]
pub(crate) mod tests;

pub(crate) fn carry_input_window<P: plakous_host_protocol::ProtocolExtension>(
    provider: PlatformProvider,
    config: &crate::runtime::HostConfig<P>,
) -> CliResult<plakous::sessions::SessionInputWindow> {
    let admission = config.state_roots.platform_admission();
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let catalog = executor
        .block_on(listing::api_model_catalog_observed(
            provider,
            config.state_roots.platform_credentials(),
            Some(&config.product_home),
            admission.lists,
            unix_now(),
            config.state_roots.exchange_observer(),
        ))
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let row = catalog
        .admit(&config.model_id, config.effort)
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let window = row.window(admission.window);
    let (total_window, admitted_input) = match provider {
        PlatformProvider::Anthropic => (None, window),
        PlatformProvider::OpenAi | PlatformProvider::Xai => (
            Some(window),
            window.saturating_sub(u64::from(row.output_bound())),
        ),
    };
    Ok(plakous::sessions::SessionInputWindow {
        route: provider.account().to_owned(),
        total_window,
        admitted_input: Some(admitted_input),
    })
}

pub(super) fn bundled(provider: PlatformProvider, source: ApiCatalogSource) -> ApiModelCatalog {
    let mut catalog = match provider {
        PlatformProvider::Anthropic => anthropic::bundled(source),
        PlatformProvider::OpenAi => openai::bundled(source),
        PlatformProvider::Xai => xai::bundled(source),
    };
    catalog.provider = provider;
    catalog
}
