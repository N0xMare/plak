//! Direct API-key model construction through the public facade.

use std::num::NonZeroU64;
use std::sync::Arc;

use plakous::model::{
    ConfiguredLanguageModel, ModelAdapterProfileId, ProviderProfileRevisionId,
    ProviderStorageDisposition, ReplayDisposition, RetentionDisposition, ToolName,
};

mod catalog;
mod listing;

pub use catalog::{
    ApiAdmissionError, ApiCatalogSource, ApiModelCatalog, ApiModelStatus, ApiWindow, PlatformModel,
    bundled_catalog, effort_names,
};
pub use listing::{ApiModelLists, api_model_catalog};

use crate::api_runtime::ApiRuntimeSpec;
use crate::binding::{CompositionSelection, PlatformProvider};
use crate::error::{CliFailure, CliResult};

/// Product-owned sources; the host never reads a product secret file.
pub trait PlatformCredentials: Send + Sync + 'static {
    fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>>;
    fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>>;
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>>;
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct UnavailableCredentials;

#[cfg(any(test, feature = "test-support"))]
impl PlatformCredentials for UnavailableCredentials {
    fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>> {
        Err(CliFailure::new("OpenAI API key is unavailable"))
    }
    fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>> {
        Err(CliFailure::new("Anthropic API key is unavailable"))
    }
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
        Err(CliFailure::new("xAI API key is unavailable"))
    }
}

/// The tool mode an API model row is enrolled under.
///
/// An API key reads no authenticated Codex catalog, so the product owns this
/// fact per row (the reviewed `OPENAI_API_CODE_MODE_ONLY` table).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiToolMode {
    /// Structured tools only.
    Direct,
    /// The model is trained on Codex Code Mode: `plakous-default` binds
    /// `native-ptc`, and `direct` stays available as the second presentation.
    CodeModeOnly,
}

/// `openai` API models enrolled as `CodeModeOnly`. Reviewed 2026-10-07 against
/// the `tool_mode` of the same model ids in the Codex 0.162.1 reference
/// catalog; a test holds this table to that fixture.
const OPENAI_API_CODE_MODE_ONLY: &[&str] = &[
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

fn openai_api_tool_mode(model: &str) -> ApiToolMode {
    if OPENAI_API_CODE_MODE_ONLY.contains(&model) {
        ApiToolMode::CodeModeOnly
    } else {
        ApiToolMode::Direct
    }
}

/// The composition an API session binds: `plakous-default` follows the row's
/// enrolled tool mode, and `native-ptc` (the vertical's own PTC system) is
/// admitted only where it is enrolled: Codex Code Mode on a `CodeModeOnly`
/// `openai` API row. The result is never `FollowCatalog`.
pub fn api_composition(
    provider: PlatformProvider,
    model: &str,
    selection: CompositionSelection,
) -> Result<CompositionSelection, &'static str> {
    let code_mode_only = platform_model(provider, model)
        .is_some_and(|row| row.tool_mode == ApiToolMode::CodeModeOnly);
    match selection {
        CompositionSelection::FollowCatalog if code_mode_only => {
            Ok(CompositionSelection::NativePtc)
        }
        CompositionSelection::FollowCatalog | CompositionSelection::Direct => {
            Ok(CompositionSelection::Direct)
        }
        CompositionSelection::JsPtc | CompositionSelection::PyPtc => Ok(selection),
        CompositionSelection::NativePtc if code_mode_only => Ok(selection),
        CompositionSelection::NativePtc => Err(provider.native_ptc_refusal()),
    }
}

/// Every usable model one API route offers offline, in listing order: the
/// bundled reviewed table classified by the provider's family rules. Parse-time
/// validation, composition decisions and the Ready catalog read these rows;
/// session construction re-admits against the live list.
#[must_use]
pub fn platform_models(provider: PlatformProvider) -> Vec<PlatformModel> {
    bundled_catalog(provider, "offline")
        .models
        .into_iter()
        .filter(PlatformModel::usable)
        .collect()
}

/// The usable offline row for one exact literal on one API route.
#[must_use]
pub fn platform_model(provider: PlatformProvider, model: &str) -> Option<PlatformModel> {
    platform_models(provider)
        .into_iter()
        .find(|row| row.model == model)
}

/// What an API session is admitted under besides its spec: whether lists are
/// live, the window selection a new session binds, and, on exact reopen, the
/// semantic evidence the session was created with.
#[derive(Clone, Debug, Default)]
pub(crate) struct PlatformAdmission {
    pub lists: ApiModelLists,
    pub window: ApiWindow,
    pub created_evidence: Option<Vec<u8>>,
}

#[cfg(test)]
pub(crate) async fn construct_platform_runtime(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    spec: &ApiRuntimeSpec<'_>,
    observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
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

pub(crate) async fn construct_platform_runtime_with(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    spec: &ApiRuntimeSpec<'_>,
    observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
    admission: &PlatformAdmission,
    clock: Arc<dyn plakous::model::Clock>,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
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
    let registry = crate::agent::coding_tool_registry_with_extensions(
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
        catalog::unix_now(),
        observer.clone(),
    )
    .await;
    let row = catalog
        .admit(spec.literal_model, Some(effort))
        .map_err(|error| CliFailure::new(error.to_string()))?
        .clone();
    // An exact reopen keeps the window number the session was created with;
    // a new session takes the configured selection.
    let window = match &admission.created_evidence {
        Some(stored) => row
            .reopen_window(provider, stored)
            .map_err(CliFailure::new)?,
        None => row.window(admission.window),
    };
    let behavior = platform_behavior(
        provider,
        effort,
        &row,
        native_ptc,
        crate::api_runtime::configured_tools(
            crate::coding_routes::platform_presentation(provider),
            composition,
            spec.wire,
            &registry,
            &extra,
        )?,
    )?;
    let maximum = NonZeroU64::new(window)
        .ok_or_else(|| CliFailure::new("API context envelope is invalid"))?;
    let configured: ConfiguredLanguageModel = match provider {
        PlatformProvider::OpenAi => {
            use plakous::openai::{
                OpenAiPlatformModel, OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts,
                PlatformProjectPolicy, PlatformStoragePolicy,
            };
            let facts = OpenAiPlatformModelFacts::from_model_id(spec.literal_model)
                .ok_or_else(|| CliFailure::new("OpenAI API model is not enrolled"))?;
            let authority = OpenAiPlatformModelBuilder::new(
                openai_credential_reference()?,
                credentials.openai()?,
                facts,
                PlatformStoragePolicy::Stateless,
                PlatformProjectPolicy::DefaultProject,
            )
            .clock(Arc::clone(&clock))
            .context_envelope(maximum)
            .admitted_facts(row.binding_evidence_for(provider, window))
            .seal()
            .map_err(api_error)?;
            let provisional = authority
                .selected_configuration(behavior)
                .map_err(api_error)?;
            let selected = authority
                .selected_configuration(exact_route_retention(&provisional))
                .map_err(api_error)?;
            let model = Arc::new(
                OpenAiPlatformModel::with_exchange_observer(authority, selected, observer.clone())
                    .map_err(api_error)?,
            );
            spec.topology_extensions.connect_retrieval(Some(Arc::new(
                plakous::openai::OpenAiRetrievalPort::new(
                    model.retrieval(plakous::openai::lowest_responses_effort(&row.efforts)),
                ),
            )))?;
            spec.topology_extensions.connect_image_gen(Some(Arc::new(
                plakous::openai::OpenAiImagePort::new(model.images()),
            )))?;
            model.configured().await.map_err(api_error)?
        }
        PlatformProvider::Anthropic => {
            let model = row
                .anthropic
                .as_ref()
                .ok_or_else(|| CliFailure::new("Anthropic API model facts are unavailable"))?;
            let (configured, retrieval) =
                plakous::anthropic::AnthropicLanguageModel::configured_with_retrieval(
                    credentials.anthropic()?,
                    "plak/anthropic",
                    model,
                    &row.binding_evidence_for(provider, window),
                    behavior,
                    maximum,
                    observer.clone(),
                )
                .map_err(api_error)?;
            spec.topology_extensions.connect_retrieval(Some(Arc::new(
                plakous::anthropic::AnthropicRetrievalPort::new(retrieval),
            )))?;
            configured
        }
        PlatformProvider::Xai => {
            let source = credentials.xai()?;
            let provisional =
                plakous::xai::xai_platform_selection_for(spec.literal_model, behavior)
                    .map_err(api_error)?;
            let selected = plakous::xai::xai_platform_selection_for(
                spec.literal_model,
                exact_route_retention(&provisional),
            )
            .map_err(api_error)?;
            plakous::xai::XaiPlatformModel::preflight_configuration(&selected, clock.as_ref())
                .map_err(api_error)?;
            let authority = source.grant().map_err(api_error)?.authority().clone();
            let model = Arc::new(
                plakous::xai::XaiPlatformModel::with_admitted_facts(
                    authority,
                    source,
                    maximum,
                    selected,
                    Arc::clone(&clock),
                    observer,
                    &row.binding_evidence_for(provider, window),
                )
                .map_err(api_error)?,
            );
            spec.topology_extensions.connect_retrieval(Some(Arc::new(
                plakous::xai::XaiRetrievalPort::new(
                    model.retrieval(plakous::openai::lowest_responses_effort(&row.efforts)),
                ),
            )))?;
            spec.topology_extensions.connect_image_gen(Some(Arc::new(
                plakous::xai::XaiImagePort::new(model.images()),
            )))?;
            model.configured().await.map_err(api_error)?
        }
    };
    crate::api_runtime::runtime_binding(spec, configured, registry, clock)
}

/// The credential reference the OpenAI API route resolves its key under, for
/// both session construction and the model listing.
pub(crate) fn openai_credential_reference() -> CliResult<plakous::openai::CredentialReference> {
    plakous::openai::CredentialReference::application("plak/openai").map_err(api_error)
}

fn exact_route_retention(
    selected: &plakous::model::SelectedProviderConfiguration,
) -> plakous::model::ProviderBehaviorConfiguration {
    let mut behavior = selected.behavior().clone();
    behavior.retention = RetentionDisposition::RouteContract {
        profile: selected.route().integration_profile().clone(),
        revision: selected.route().integration_revision().clone(),
    };
    behavior
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

#[cfg(test)]
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
            catalog::unix_now(),
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
